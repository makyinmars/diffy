use std::collections::{HashSet, VecDeque};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::api::{BitbucketApi, str_at};
use super::overview::reviewers;
use super::transport;
use crate::core::error::{DiffyError, Result};
use crate::core::forge::github::my_pulls::excerpt;
use crate::core::forge::model::{MyPullRequest, PullRequestFilter, PullRequestPage};
use crate::core::review::ReviewForge;

const PAGE_BUDGET: usize = 5;

#[derive(Deserialize, Serialize)]
struct Cursor {
    filter: String,
    user: String,
    discovery: Option<String>,
    pending: VecDeque<String>,
}

impl BitbucketApi {
    /// Authored pull requests across accessible workspaces. Each call reads at
    /// most five discovery/listing pages; loaded rows, not the unseen history,
    /// are sorted by activity. Failed pages remain in the opaque retry cursor.
    /// The supported workspace endpoint accepts repeated state parameters:
    /// https://developer.atlassian.com/cloud/bitbucket/rest/api-group-workspaces/#api-workspaces-workspace-pullrequests-selected-user-get
    pub fn fetch_my_pull_requests(
        &self,
        filter: PullRequestFilter,
        cursor: Option<&str>,
    ) -> Result<PullRequestPage> {
        let credentials = self.credentials();
        if credentials.is_anonymous() {
            return Err(DiffyError::auth(
                "Bitbucket sign-in is required to list your pull requests",
            ));
        }
        let root = self.api_root();
        let mut cursor: Cursor = match cursor {
            Some(cursor) => serde_json::from_str(cursor)?,
            None => {
                let user = self.fetch_current_user()?;
                if user.uuid.is_empty() {
                    return Err(DiffyError::Parse(
                        "missing Bitbucket account UUID".to_owned(),
                    ));
                }
                Cursor {
                    filter: filter.id().to_owned(),
                    user: user.uuid,
                    discovery: Some(format!("{root}/user/workspaces?pagelen=50")),
                    pending: VecDeque::new(),
                }
            }
        };
        if cursor.user.is_empty() || cursor.filter != filter.id() {
            return Err(DiffyError::Parse(
                "invalid Bitbucket pull request cursor".to_owned(),
            ));
        }
        if let Some(url) = &cursor.discovery {
            validate_url(root, url, true, &cursor.user)?;
        }
        for url in &cursor.pending {
            validate_url(root, url, false, &cursor.user)?;
        }
        let mut page = PullRequestPage::default();
        let mut attempted = HashSet::new();
        for _ in 0..PAGE_BUDGET {
            let eligible = cursor
                .pending
                .iter()
                .position(|url| !attempted.contains(url));
            let (url, discovery) = match eligible.and_then(|index| cursor.pending.remove(index)) {
                Some(url) => (url, false),
                None => match cursor
                    .discovery
                    .as_ref()
                    .filter(|url| !attempted.contains(*url))
                    .cloned()
                {
                    Some(url) => (url, true),
                    None => break,
                },
            };
            attempted.insert(url.clone());
            let result =
                transport::get_json(credentials, &url, "Bitbucket authored pull requests fetch")
                    .and_then(|value| {
                        let values =
                            value
                                .get("values")
                                .and_then(Value::as_array)
                                .ok_or_else(|| {
                                    DiffyError::Parse("missing Bitbucket listing values".to_owned())
                                })?;
                        let next = transport::next_page(&value);
                        if let Some(next) = &next {
                            validate_url(root, next, discovery, &cursor.user)?;
                            let current = Url::parse(&url).ok();
                            if Url::parse(next).ok() == current {
                                return Err(DiffyError::Parse(
                                    "repeated Bitbucket page cursor".to_owned(),
                                ));
                            }
                        }
                        if discovery {
                            let urls = values
                                .iter()
                                .map(|access| {
                                    let workspace = access
                                        .pointer("/workspace/slug")
                                        .and_then(Value::as_str)
                                        .filter(|slug| !slug.is_empty())
                                        .ok_or_else(|| {
                                            DiffyError::Parse(
                                                "missing Bitbucket workspace slug".to_owned(),
                                            )
                                        })?;
                                    authored_url(root, workspace, &cursor.user, filter)
                                })
                                .collect::<Result<Vec<_>>>()?;
                            cursor.pending.extend(urls);
                            cursor.discovery = next;
                        } else {
                            page.pulls.extend(values.iter().map(parse_pull));
                            if let Some(next) = next {
                                cursor.pending.push_back(next);
                            }
                        }
                        Ok(())
                    });
            if let Err(error) = result {
                if matches!(error, DiffyError::Auth { .. }) {
                    return Err(error);
                }
                page.warnings.push(error.to_string());
                if discovery {
                    cursor.discovery = Some(url);
                } else {
                    cursor.pending.push_back(url);
                }
            }
        }
        page.pulls.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        if cursor.discovery.is_some() || !cursor.pending.is_empty() {
            page.next_cursor = Some(serde_json::to_string(&cursor)?);
        }
        Ok(page)
    }
}

fn authored_url(
    root: &str,
    workspace: &str,
    user: &str,
    filter: PullRequestFilter,
) -> Result<String> {
    let mut url =
        Url::parse(root).map_err(|_| DiffyError::Parse("invalid Bitbucket API root".to_owned()))?;
    url.path_segments_mut()
        .map_err(|_| DiffyError::Parse("invalid Bitbucket API root".to_owned()))?
        .extend(["workspaces", workspace, "pullrequests", user]);
    let states: &[&str] = match filter {
        PullRequestFilter::Open => &["OPEN"],
        PullRequestFilter::Merged => &["MERGED"],
        PullRequestFilter::Closed => &["DECLINED", "SUPERSEDED"],
    };
    let mut query = url.query_pairs_mut();
    for state in states {
        query.append_pair("state", state);
    }
    query
        .append_pair("sort", "-updated_on")
        .append_pair("pagelen", "50")
        .append_pair("fields", "+values.participants,+values.reviewers");
    drop(query);
    Ok(url.into())
}

fn validate_url(root: &str, candidate: &str, discovery: bool, user: &str) -> Result<()> {
    let invalid = || DiffyError::Parse("invalid Bitbucket page URL".to_owned());
    let root = Url::parse(root).map_err(|_| invalid())?;
    let url = Url::parse(candidate).map_err(|_| invalid())?;
    if url.origin() != root.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    if discovery {
        if url.path() != format!("{}/user/workspaces", root.path()) {
            return Err(invalid());
        }
    } else {
        let prefix = format!("{}/workspaces/", root.path());
        let suffix = authored_url(
            root.as_str().trim_end_matches('/'),
            "workspace",
            user,
            PullRequestFilter::Open,
        )?;
        let suffix = Url::parse(&suffix).map_err(|_| invalid())?;
        let suffix = suffix
            .path()
            .split("/pullrequests/")
            .nth(1)
            .ok_or_else(invalid)?;
        let workspace = url
            .path()
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(&format!("/pullrequests/{suffix}")))
            .filter(|workspace| !workspace.is_empty() && !workspace.contains('/'));
        if workspace.is_none() {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn parse_pull(value: &Value) -> MyPullRequest {
    let reviewers = reviewers(value);
    let decision = if reviewers.iter().any(|r| r.state == "CHANGES_REQUESTED") {
        Some("CHANGES_REQUESTED")
    } else if reviewers.iter().any(|r| r.state == "APPROVED") {
        Some("APPROVED")
    } else {
        None
    };
    let repository = str_at(value, "/destination/repository/full_name");
    let number = value
        .get("id")
        .and_then(Value::as_i64)
        .and_then(|id| i32::try_from(id).ok())
        .unwrap_or_default();
    let body = Some(str_at(value, "/summary/raw"))
        .filter(|body| !body.is_empty())
        .unwrap_or_else(|| str_at(value, "/description"));
    MyPullRequest {
        forge: ReviewForge::Bitbucket,
        id: format!("bitbucket:{repository}#{number}"),
        number,
        title: str_at(value, "/title"),
        state: str_at(value, "/state"),
        url: str_at(value, "/links/html/href"),
        repository,
        is_draft: value.get("draft").and_then(Value::as_bool).unwrap_or(false),
        updated_at: str_at(value, "/updated_on"),
        head_branch: str_at(value, "/source/branch/name"),
        base_branch: str_at(value, "/destination/branch/name"),
        review_decision: decision.map(ToOwned::to_owned),
        requested_reviewers: reviewers.iter().filter(|r| r.state == "REQUESTED").count(),
        reviewers,
        unresolved_threads: 0,
        excerpt: excerpt(&body),
        stats: None,
        checks: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use serde_json::json;

    use super::*;
    use crate::core::forge::bitbucket::Credentials;
    use crate::core::forge::test_http::{Request, Server};

    fn api(server: &Server) -> BitbucketApi {
        let mut api = BitbucketApi::new(Credentials::parse("fixture-token").unwrap());
        api.api_root = Some(format!("{}/2.0", server.root));
        api
    }

    fn request(request: &Request) -> Url {
        assert_eq!(request.method, "GET");
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer fixture-token")
        );
        let host = &request
            .headers
            .iter()
            .find(|(name, _)| name == "host")
            .unwrap()
            .1;
        Url::parse(&format!("http://{host}{}", request.target)).unwrap()
    }

    fn user() -> Value {
        json!({ "nickname": "me", "uuid": "{me}" })
    }

    fn workspaces(slugs: &[&str]) -> Value {
        json!({ "values": slugs.iter().map(|slug| json!({ "workspace": { "slug": slug } })).collect::<Vec<_>>() })
    }

    #[test]
    fn wire_filters_use_authored_workspace_path_repeated_states_and_encoded_fields() {
        for (filter, states) in [
            (PullRequestFilter::Open, vec!["OPEN"]),
            (PullRequestFilter::Merged, vec!["MERGED"]),
            (PullRequestFilter::Closed, vec!["DECLINED", "SUPERSEDED"]),
        ] {
            let expected = states.clone();
            let server = Server::new(3, move |incoming| {
                let url = request(&incoming);
                match url.path() {
                    "/2.0/user" => (200, user()),
                    "/2.0/user/workspaces" => (200, workspaces(&["acme"])),
                    "/2.0/workspaces/acme/pullrequests/%7Bme%7D" => {
                        let pairs = url.query_pairs().collect::<Vec<_>>();
                        assert_eq!(
                            pairs
                                .iter()
                                .filter(|(key, _)| key == "state")
                                .map(|(_, value)| value.as_ref())
                                .collect::<Vec<_>>(),
                            expected
                        );
                        assert!(
                            pairs
                                .iter()
                                .any(|(key, value)| key == "sort" && value == "-updated_on")
                        );
                        assert!(
                            pairs
                                .iter()
                                .any(|(key, value)| key == "pagelen" && value == "50")
                        );
                        assert!(pairs.iter().any(|(key, value)| key == "fields"
                            && value == "+values.participants,+values.reviewers"));
                        assert!(
                            incoming
                                .target
                                .contains("fields=%2Bvalues.participants%2C%2Bvalues.reviewers")
                        );
                        (
                            200,
                            json!({ "values": expected.iter().enumerate().map(|(id, state)| json!({ "id": id + 1, "state": state, "draft": true, "destination": { "repository": { "full_name": "acme/repo" } } })).collect::<Vec<_>>() }),
                        )
                    }
                    path => panic!("unexpected path {path}"),
                }
            });
            let page = api(&server).fetch_my_pull_requests(filter, None).unwrap();
            assert_eq!(
                page.pulls
                    .iter()
                    .map(|pull| pull.state.as_str())
                    .collect::<Vec<_>>(),
                states
            );
            assert!(
                page.pulls
                    .iter()
                    .all(|pull| pull.is_draft && pull.stats.is_none() && pull.checks.is_none())
            );
            assert!(page.next_cursor.is_none());
            assert!(page.total_count.is_none());
            server.finish();
        }
    }

    #[test]
    fn wire_discovery_and_history_resume_beyond_twenty_workspaces_with_bounded_calls() {
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let server = Server::new(25, move |incoming| {
            observed.fetch_add(1, Ordering::SeqCst);
            let url = request(&incoming);
            if url.path() == "/2.0/user" {
                (200, user())
            } else if url.path() == "/2.0/user/workspaces" {
                (
                    200,
                    json!({ "values": (0..23).map(|id| json!({ "workspace": { "slug": format!("ws{id}") } })).collect::<Vec<_>>() }),
                )
            } else {
                let id = url
                    .path()
                    .split('/')
                    .nth(3)
                    .unwrap()
                    .strip_prefix("ws")
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                (
                    200,
                    json!({ "values": [{ "id": id + 1, "updated_on": format!("2026-10-{:02}", id + 1), "state": "MERGED" }] }),
                )
            }
        });
        let api = api(&server);
        let mut cursor = None;
        let mut seen = HashSet::new();
        for batch in 0..5 {
            let before = requests.load(Ordering::SeqCst);
            let page = api
                .fetch_my_pull_requests(PullRequestFilter::Merged, cursor.as_deref())
                .unwrap();
            assert!(
                requests.load(Ordering::SeqCst) - before <= PAGE_BUDGET + usize::from(batch == 0)
            );
            assert!(
                page.pulls
                    .windows(2)
                    .all(|pair| pair[0].updated_at >= pair[1].updated_at)
            );
            seen.extend(page.pulls.into_iter().map(|pull| pull.number));
            cursor = page.next_cursor;
        }
        assert_eq!(seen.len(), 23);
        assert!(cursor.is_none());
        server.finish();
    }

    #[test]
    fn wire_follows_opaque_discovery_and_pull_pages_without_rebuilding_queries() {
        let server = Server::new(6, |incoming| {
            let mut url = request(&incoming);
            if url.path() == "/2.0/user" {
                return (200, user());
            }
            let page_two = url
                .query_pairs()
                .any(|(key, value)| key == "opaque" && value == "next+=");
            if url.path() == "/2.0/user/workspaces" {
                if page_two {
                    return (200, workspaces(&["last"]));
                }
                url.set_query(Some("opaque=next%2B%3D"));
                return (
                    200,
                    json!({ "values": [{ "workspace": { "slug": "first" } }], "next": url.as_str() }),
                );
            }
            if url.path().contains("/first/") && !page_two {
                url.query_pairs_mut().append_pair("opaque", "next+=");
                return (
                    200,
                    json!({ "values": [{ "id": 1 }], "next": url.as_str() }),
                );
            }
            (
                200,
                json!({ "values": [{ "id": if page_two { 2 } else { 3 } }] }),
            )
        });
        let page = api(&server)
            .fetch_my_pull_requests(PullRequestFilter::Open, None)
            .unwrap();
        assert_eq!(
            page.pulls
                .iter()
                .map(|pull| pull.number)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(page.next_cursor.is_none());
        server.finish();
    }

    #[test]
    fn wire_partial_workspace_failure_warns_and_retries_without_repeating_successes() {
        let mut failed = false;
        let server = Server::new(5, move |incoming| {
            let url = request(&incoming);
            match url.path() {
                "/2.0/user" => (200, user()),
                "/2.0/user/workspaces" => (200, workspaces(&["bad", "good"])),
                path if path.contains("/bad/") && !failed => {
                    failed = true;
                    (502, json!({ "error": { "message": "unavailable" } }))
                }
                path => (
                    200,
                    json!({ "values": [{ "id": if path.contains("/bad/") { 2 } else { 1 } }] }),
                ),
            }
        });
        let api = api(&server);
        let first = api
            .fetch_my_pull_requests(PullRequestFilter::Open, None)
            .unwrap();
        assert_eq!(first.pulls[0].number, 1);
        assert_eq!(first.warnings.len(), 1);
        assert!(matches!(
            api.fetch_my_pull_requests(PullRequestFilter::Closed, first.next_cursor.as_deref()),
            Err(DiffyError::Parse(_))
        ));
        let second = api
            .fetch_my_pull_requests(PullRequestFilter::Open, first.next_cursor.as_deref())
            .unwrap();
        assert_eq!(second.pulls[0].number, 2);
        assert!(second.warnings.is_empty());
        assert!(second.next_cursor.is_none());
        server.finish();
    }

    #[test]
    fn wire_refusal_after_a_successful_workspace_is_never_hidden() {
        for status in [401, 403] {
            let server = Server::new(4, move |incoming| {
                let url = request(&incoming);
                match url.path() {
                    "/2.0/user" => (200, user()),
                    "/2.0/user/workspaces" => (200, workspaces(&["good", "bad"])),
                    path if path.contains("/good/") => (200, json!({ "values": [{ "id": 1 }] })),
                    _ => (status, json!({ "error": { "message": "denied" } })),
                }
            });
            let result = api(&server).fetch_my_pull_requests(PullRequestFilter::Open, None);
            if status == 401 {
                assert!(matches!(result, Err(DiffyError::Auth { .. })));
            } else {
                let page = result.unwrap();
                assert_eq!(page.pulls.len(), 1);
                assert_eq!(page.warnings.len(), 1);
                assert!(page.next_cursor.is_some());
            }
            server.finish();
        }
    }

    #[test]
    fn wire_empty_discovery_is_a_complete_empty_page_and_401_is_auth() {
        for status in [200, 401] {
            let server = Server::new(2, move |incoming| {
                let url = request(&incoming);
                if url.path() == "/2.0/user" {
                    (200, user())
                } else {
                    (status, json!({ "values": [] }))
                }
            });
            let result = api(&server).fetch_my_pull_requests(PullRequestFilter::Closed, None);
            if status == 200 {
                let page = result.unwrap();
                assert!(page.pulls.is_empty());
                assert!(page.next_cursor.is_none());
            } else {
                assert!(matches!(result, Err(DiffyError::Auth { .. })));
            }
            server.finish();
        }
    }

    #[test]
    fn untrusted_cursor_and_next_urls_never_receive_credentials() {
        let server = Server::new(0, |_| unreachable!());
        let client = api(&server);
        for url in [
            "https://api.bitbucket.org.evil.test/2.0/user/workspaces",
            "https://api.bitbucket.org@evil.test/2.0/user/workspaces",
            "http://external.test/2.0/user/workspaces",
        ] {
            let cursor =
                json!({ "filter": "open", "user": "{me}", "discovery": url, "pending": [] })
                    .to_string();
            assert!(matches!(
                client.fetch_my_pull_requests(PullRequestFilter::Open, Some(&cursor)),
                Err(DiffyError::Parse(_))
            ));
            let cursor =
                json!({ "filter": "open", "user": "{me}", "discovery": null, "pending": [url] })
                    .to_string();
            assert!(matches!(
                client.fetch_my_pull_requests(PullRequestFilter::Open, Some(&cursor)),
                Err(DiffyError::Parse(_))
            ));
        }
        server.finish();
        let server = Server::new(3, |incoming| {
            let url = request(&incoming);
            match url.path() {
                "/2.0/user" => (200, user()),
                "/2.0/user/workspaces" => (200, workspaces(&["acme"])),
                _ => (
                    200,
                    json!({ "values": [{ "id": 1 }], "next": "https://external.test/steal" }),
                ),
            }
        });
        let page = api(&server)
            .fetch_my_pull_requests(PullRequestFilter::Open, None)
            .unwrap();
        assert_eq!(page.warnings.len(), 1);
        assert!(page.pulls.is_empty());
        assert!(page.next_cursor.is_some());
        server.finish();
    }

    #[test]
    fn authored_pull_requests_map_reviewers_decisions_and_clone_keys() {
        let user =
            |nickname: &str| json!({ "nickname": nickname, "links": { "avatar": { "href": "" } } });
        let pull = parse_pull(&json!({
            "id": 5,
            "title": "Tidy the widget",
            "state": "OPEN",
            "draft": true,
            "author": user("ada"),
            "summary": { "raw": "Line one\n\nline two" },
            "updated_on": "2026-10-02T00:00:00+00:00",
            "source": { "branch": { "name": "feature" } },
            "destination": { "branch": { "name": "main" }, "repository": { "full_name": "Acme/Widgets" } },
            "links": { "html": { "href": "https://bitbucket.org/Acme/Widgets/pull-requests/5" } },
            "reviewers": [user("bob")],
            "participants": [{ "user": user("cy"), "role": "PARTICIPANT", "state": "approved" }]
        }));

        assert_eq!(pull.forge, ReviewForge::Bitbucket);
        assert_eq!(pull.id, "bitbucket:Acme/Widgets#5");
        assert_eq!((pull.number, pull.is_draft), (5, true));
        assert_eq!(pull.repo_key(), "bitbucket.org/acme/widgets");
        assert_eq!(pull.excerpt, "Line one line two");
        assert_eq!(pull.review_decision.as_deref(), Some("APPROVED"));
        assert_eq!(pull.requested_reviewers, 1);
        let reviewers: Vec<(&str, &str)> = pull
            .reviewers
            .iter()
            .map(|r| (r.actor.login.as_str(), r.state.as_str()))
            .collect();
        assert_eq!(reviewers, [("cy", "APPROVED"), ("bob", "REQUESTED")]);
    }
}
