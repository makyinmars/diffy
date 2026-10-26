use std::collections::HashMap;

use serde_json::{Value, json};

use super::api::{
    GitHubApi, bool_field, int_field, optional_string_field, parse_check_summary, string_field,
};
use super::overview::{actor, nodes};
use crate::core::error::{DiffyError, Result};
use crate::core::forge::model::{
    MyPullRequest, PullRequestCheckSummary, PullRequestFilter, PullRequestPage,
    PullRequestReviewer, PullRequestStats,
};
use crate::core::review::ReviewForge;

const EXCERPT_CHARS: usize = 280;
const SEARCH_LIMIT: usize = 1_000;

const CORE_QUERY: &str = r#"
query DiffyMyPullRequests($search: String!, $after: String) {
  viewer { login }
  search(query: $search, type: ISSUE, first: 50, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        id
        number
        title
        state
        url
        isDraft
        updatedAt
        bodyText
        repository { nameWithOwner }
        headRefName
        baseRefName
        reviewDecision
        latestReviews(first: 10) {
          nodes { state author { login avatarUrl } }
        }
        reviewRequests(first: 10) {
          nodes {
            requestedReviewer {
              __typename
              ... on User { login avatarUrl }
            }
          }
        }
        reviewThreads(first: 100) {
          nodes { isResolved }
        }
      }
    }
  }
}
"#;

const STATS_QUERY: &str = r#"
query DiffyMyPullRequestStats($ids: [ID!]!) {
  nodes(ids: $ids) {
      ... on PullRequest { id mergeable additions deletions changedFiles }
  }
}
"#;

const CHECKS_QUERY: &str = r#"
query DiffyMyPullRequestChecks($ids: [ID!]!) {
  nodes(ids: $ids) {
      ... on PullRequest {
        id
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                state
                contexts(first: 10) {
                  totalCount
                  checkRunCountsByState { state count }
                  statusContextCountsByState { state count }
                  nodes {
                    __typename
                    ... on CheckRun { name status conclusion detailsUrl }
                    ... on StatusContext { context state targetUrl }
                  }
                }
              }
            }
          }
        }
      }
  }
}
"#;

impl GitHubApi {
    pub fn fetch_my_pull_requests(
        &self,
        filter: PullRequestFilter,
        cursor: Option<&str>,
    ) -> Result<PullRequestPage> {
        if !self.has_token() {
            return Err(DiffyError::auth(
                "GitHub authentication is required to list your pull requests".to_owned(),
            ));
        }
        let (after, seen) = match cursor {
            Some(cursor) => {
                let value: Value = serde_json::from_str(cursor)?;
                if value.get("filter").and_then(Value::as_str) != Some(filter.id()) {
                    return Err(DiffyError::Parse(
                        "GitHub pull request cursor belongs to another filter".to_owned(),
                    ));
                }
                let after = value.get("after").and_then(Value::as_str);
                let seen = value.get("seen").and_then(Value::as_u64);
                match (after, seen) {
                    (Some(after), Some(seen))
                        if !after.is_empty() && seen < SEARCH_LIMIT as u64 =>
                    {
                        (Some(after.to_owned()), seen as usize)
                    }
                    _ => {
                        return Err(DiffyError::Parse(
                            "invalid GitHub pull request cursor".to_owned(),
                        ));
                    }
                }
            }
            None => (None, 0),
        };
        let state = match filter {
            PullRequestFilter::Open => "is:open",
            PullRequestFilter::Merged => "is:merged",
            PullRequestFilter::Closed => "is:closed is:unmerged",
        };
        let search = format!("is:pr {state} author:@me archived:false sort:updated-desc");
        let core = self.graphql_query(CORE_QUERY, json!({ "search": search, "after": after }))?;
        let mut pulls = parse_core(&core);
        let total_count = core
            .pointer("/data/search/issueCount")
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok());
        let has_next = core
            .pointer("/data/search/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                DiffyError::Parse("missing GitHub pull request page information".to_owned())
            })?;
        let seen = seen + nodes(&core, "/data/search/nodes").count();
        let limit_reached = seen >= SEARCH_LIMIT
            && (has_next || total_count.is_some_and(|count| count > SEARCH_LIMIT));
        let next_cursor = if has_next && seen < SEARCH_LIMIT && !pulls.is_empty() {
            let next = core
                .pointer("/data/search/pageInfo/endCursor")
                .and_then(Value::as_str)
                .filter(|next| !next.is_empty() && Some(*next) != after.as_deref())
                .ok_or_else(|| {
                    DiffyError::Parse("invalid GitHub pull request page cursor".to_owned())
                })?;
            Some(json!({ "filter": filter.id(), "after": next, "seen": seen }).to_string())
        } else {
            None
        };
        if pulls.is_empty() {
            return Ok(PullRequestPage {
                pulls,
                next_cursor,
                total_count,
                limit_reached,
                warnings: Vec::new(),
            });
        }
        let variables =
            json!({ "ids": pulls.iter().map(|pull| pull.id.as_str()).collect::<Vec<_>>() });
        let (stats, checks) = std::thread::scope(|scope| {
            let stats = scope.spawn(|| self.graphql_query(STATS_QUERY, variables.clone()));
            let checks = scope.spawn(|| self.graphql_query(CHECKS_QUERY, variables.clone()));
            (stats.join(), checks.join())
        });
        let mut warnings = Vec::new();
        let stats = optional(stats, "stats", &mut warnings);
        let checks = optional(checks, "checks", &mut warnings);
        let stats = stats?;
        let checks = checks?;
        let stats = stats.as_ref().map(parse_stats).unwrap_or_default();
        let checks = checks.as_ref().map(parse_checks).unwrap_or_default();
        for pull in &mut pulls {
            pull.stats = stats.get(&pull.id).cloned();
            pull.checks = checks.get(&pull.id).cloned();
        }
        Ok(PullRequestPage {
            pulls,
            next_cursor,
            total_count,
            limit_reached,
            warnings,
        })
    }
}

fn optional(
    joined: std::thread::Result<Result<Value>>,
    part: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<Value>> {
    match joined {
        Ok(Ok(value)) => Ok(Some(value)),
        Ok(Err(error @ DiffyError::Auth { .. })) => Err(error),
        Ok(Err(error)) => {
            warnings.push(format!("Pull request {part} unavailable: {error}"));
            Ok(None)
        }
        Err(_) => {
            warnings.push(format!("Pull request {part} unavailable"));
            Ok(None)
        }
    }
}

fn search_nodes(value: &Value) -> impl Iterator<Item = &Value> {
    nodes(value, "/data/search/nodes").filter(|node| !string_field(node, "id").is_empty())
}

fn parse_core(value: &Value) -> Vec<MyPullRequest> {
    let viewer = value
        .pointer("/data/viewer/login")
        .and_then(Value::as_str)
        .unwrap_or_default();
    search_nodes(value)
        .map(|node| parse_pull(node, viewer))
        .collect()
}

fn parse_pull(node: &Value, viewer: &str) -> MyPullRequest {
    let mut reviewers: Vec<PullRequestReviewer> = nodes(node, "/reviewRequests/nodes")
        .filter_map(|request| request.get("requestedReviewer"))
        .filter(|reviewer| string_field(reviewer, "__typename") == "User")
        .map(|reviewer| PullRequestReviewer {
            actor: actor(Some(reviewer)),
            state: "REQUESTED".to_owned(),
        })
        .filter(|reviewer| !reviewer.actor.login.is_empty())
        .collect();
    let requested_reviewers = reviewers.len();
    for review in nodes(node, "/latestReviews/nodes") {
        let actor = actor(review.get("author"));
        let state = string_field(review, "state");
        if actor.login.is_empty()
            || state == "PENDING"
            || actor.login.eq_ignore_ascii_case(viewer)
            || reviewers.iter().any(|r| r.actor.login == actor.login)
        {
            continue;
        }
        reviewers.push(PullRequestReviewer { actor, state });
    }
    MyPullRequest {
        forge: ReviewForge::GitHub,
        id: string_field(node, "id"),
        number: int_field(node, "number") as i32,
        title: string_field(node, "title"),
        state: string_field(node, "state"),
        url: string_field(node, "url"),
        repository: node
            .pointer("/repository/nameWithOwner")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        is_draft: bool_field(node, "isDraft"),
        updated_at: string_field(node, "updatedAt"),
        head_branch: string_field(node, "headRefName"),
        base_branch: string_field(node, "baseRefName"),
        review_decision: optional_string_field(node, "reviewDecision"),
        reviewers,
        requested_reviewers,
        unresolved_threads: nodes(node, "/reviewThreads/nodes")
            .filter(|thread| !bool_field(thread, "isResolved"))
            .count(),
        excerpt: excerpt(&string_field(node, "bodyText")),
        stats: None,
        checks: None,
    }
}

fn parse_stats(value: &Value) -> HashMap<String, PullRequestStats> {
    nodes(value, "/data/nodes")
        .filter_map(|node| {
            let count = |key| {
                node.get(key)
                    .and_then(Value::as_i64)
                    .and_then(|count| i32::try_from(count).ok())
            };
            let id = optional_string_field(node, "id")?;
            Some((
                id,
                PullRequestStats {
                    mergeable: string_field(node, "mergeable"),
                    additions: count("additions")?,
                    deletions: count("deletions")?,
                    changed_files: count("changedFiles")?,
                },
            ))
        })
        .collect()
}

fn parse_checks(value: &Value) -> HashMap<String, PullRequestCheckSummary> {
    nodes(value, "/data/nodes")
        .filter_map(|node| {
            let rollup = node
                .pointer("/commits/nodes/0/commit/statusCheckRollup")
                .filter(|rollup| !rollup.is_null())?;
            rollup
                .pointer("/contexts/totalCount")
                .and_then(Value::as_i64)?;
            Some((optional_string_field(node, "id")?, parse_rollup(rollup)))
        })
        .collect()
}

fn parse_rollup(rollup: &Value) -> PullRequestCheckSummary {
    let mut summary = parse_check_summary(rollup);
    let counts = nodes(rollup, "/contexts/checkRunCountsByState")
        .chain(nodes(rollup, "/contexts/statusContextCountsByState"))
        .collect::<Vec<_>>();
    if !counts.is_empty() {
        summary.success_count = 0;
        summary.failure_count = 0;
        summary.pending_count = 0;
        for count in counts {
            summary.add_state(
                &string_field(count, "state"),
                int_field(count, "count") as i32,
            );
        }
    }
    summary
}

pub(crate) fn excerpt(body: &str) -> String {
    let mut out = String::new();
    for (index, word) in body.split_whitespace().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() > EXCERPT_CHARS {
            let cut: String = out.chars().take(EXCERPT_CHARS).collect();
            return format!("{}\u{2026}", cut.trim_end());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::forge::test_http::{Request, Server};

    fn api(server: &Server) -> GitHubApi {
        let mut api = GitHubApi::with_token("fixture-token");
        api.graphql_url = Some(format!("{}/graphql", server.root));
        api
    }

    fn request(request: Request) -> Value {
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/graphql");
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer fixture-token")
        );
        serde_json::from_str(&request.body).unwrap()
    }

    fn core_page(pulls: Vec<Value>, next: Option<&str>, count: usize) -> Value {
        json!({ "data": { "viewer": { "login": "me" }, "search": {
            "nodes": pulls, "issueCount": count,
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next }
        } } })
    }

    #[test]
    fn wire_filters_authored_search_and_preserves_states_drafts_and_unknown_metadata() {
        for (filter, qualifier, state) in [
            (PullRequestFilter::Open, "is:open", "OPEN"),
            (PullRequestFilter::Merged, "is:merged", "MERGED"),
            (PullRequestFilter::Closed, "is:closed is:unmerged", "CLOSED"),
        ] {
            let server = Server::new(3, move |incoming| {
                let body = request(incoming);
                let query = body["query"].as_str().unwrap();
                if query.contains("query DiffyMyPullRequests(") {
                    assert_eq!(
                        body["variables"]["search"],
                        format!("is:pr {qualifier} author:@me archived:false sort:updated-desc")
                    );
                    assert!(body["variables"]["after"].is_null());
                    assert!(query.contains("first: 50, after: $after"));
                    assert!(query.contains("pageInfo { hasNextPage endCursor }"));
                    (
                        200,
                        core_page(
                            vec![json!({ "id": "PR_1", "state": state, "isDraft": true })],
                            None,
                            1,
                        ),
                    )
                } else {
                    assert!(query.contains("nodes(ids: $ids)"));
                    assert!(!query.contains("search("));
                    assert_eq!(body["variables"], json!({ "ids": ["PR_1"] }));
                    (
                        200,
                        json!({ "data": { "nodes": [null, { "id": "PR_1", "additions": 0 }] } }),
                    )
                }
            });
            let page = api(&server).fetch_my_pull_requests(filter, None).unwrap();
            assert_eq!(page.pulls.len(), 1);
            assert_eq!(page.pulls[0].state, state);
            assert!(page.pulls[0].is_draft);
            assert!(page.pulls[0].stats.is_none());
            assert!(page.pulls[0].checks.is_none());
            assert_eq!(page.total_count, Some(1));
            assert!(page.next_cursor.is_none());
            server.finish();
        }
    }

    #[test]
    fn wire_pages_use_provider_cursor_and_enrich_only_the_same_ids() {
        let server = Server::new(6, |incoming| {
            let body = request(incoming);
            let query = body["query"].as_str().unwrap();
            if query.contains("query DiffyMyPullRequests(") {
                if body["variables"]["after"].is_null() {
                    (
                        200,
                        core_page(
                            vec![json!({ "id": "PR_1" }), json!({ "id": "PR_2" })],
                            Some("opaque+/=\""),
                            3,
                        ),
                    )
                } else {
                    assert_eq!(body["variables"]["after"], "opaque+/=\"");
                    (
                        200,
                        core_page(vec![json!({ "id": "PR_3", "state": "MERGED" })], None, 3),
                    )
                }
            } else {
                let ids = body["variables"]["ids"].as_array().unwrap();
                assert!(ids == &vec![json!("PR_1"), json!("PR_2")] || ids == &vec![json!("PR_3")]);
                if query.contains("Stats(") {
                    let nodes = ids.iter().rev().map(|id| json!({ "id": id, "mergeable": "UNKNOWN", "additions": if id == "PR_1" { 7 } else { 2 }, "deletions": 1, "changedFiles": 1 })).collect::<Vec<_>>();
                    (200, json!({ "data": { "nodes": nodes } }))
                } else {
                    (
                        200,
                        json!({ "data": { "nodes": [{ "id": "PR_1", "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "state": "SUCCESS", "contexts": { "totalCount": 1, "nodes": [] } } } }] } }] } }),
                    )
                }
            }
        });
        let api = api(&server);
        let first = api
            .fetch_my_pull_requests(PullRequestFilter::Merged, None)
            .unwrap();
        assert_eq!(first.pulls[0].stats.as_ref().unwrap().additions, 7);
        assert_eq!(first.pulls[1].stats.as_ref().unwrap().additions, 2);
        assert_eq!(
            first.pulls[0].checks.as_ref().unwrap().state.as_deref(),
            Some("SUCCESS")
        );
        assert!(first.pulls[1].checks.is_none());
        assert!(matches!(
            api.fetch_my_pull_requests(PullRequestFilter::Closed, first.next_cursor.as_deref()),
            Err(DiffyError::Parse(_))
        ));
        let second = api
            .fetch_my_pull_requests(PullRequestFilter::Merged, first.next_cursor.as_deref())
            .unwrap();
        assert_eq!(second.pulls[0].id, "PR_3");
        assert!(second.pulls[0].checks.is_none());
        assert!(second.next_cursor.is_none());
        server.finish();
    }

    #[test]
    fn wire_empty_page_skips_optional_queries() {
        let server = Server::new(1, |incoming| {
            request(incoming);
            (200, core_page(Vec::new(), None, 0))
        });
        let page = api(&server)
            .fetch_my_pull_requests(PullRequestFilter::Open, None)
            .unwrap();
        assert!(page.pulls.is_empty());
        assert_eq!(page.total_count, Some(0));
        assert!(page.next_cursor.is_none());
        server.finish();
    }

    #[test]
    fn wire_401_in_core_or_either_optional_query_is_auth_and_sanitized() {
        for operation in [
            "DiffyMyPullRequests(",
            "DiffyMyPullRequestStats(",
            "DiffyMyPullRequestChecks(",
        ] {
            let count = if operation == "DiffyMyPullRequests(" {
                1
            } else {
                3
            };
            let server = Server::new(count, move |incoming| {
                let body = request(incoming);
                let query = body["query"].as_str().unwrap();
                if query.contains(operation) {
                    (401, json!({ "message": "reflected-secret" }))
                } else if query.contains("DiffyMyPullRequests(") {
                    (200, core_page(vec![json!({ "id": "PR_1" })], None, 1))
                } else {
                    (200, json!({ "data": { "nodes": [] } }))
                }
            });
            let error = api(&server)
                .fetch_my_pull_requests(PullRequestFilter::Open, None)
                .unwrap_err();
            assert!(matches!(error, DiffyError::Auth { .. }));
            assert!(!error.to_string().contains("reflected-secret"));
            server.finish();
        }
    }

    #[test]
    fn wire_optional_failure_keeps_core_and_reports_unknown_metadata() {
        for status in [403, 502, 200] {
            let server = Server::new(3, move |incoming| {
                let body = request(incoming);
                if body["query"]
                    .as_str()
                    .unwrap()
                    .contains("DiffyMyPullRequests(")
                {
                    (200, core_page(vec![json!({ "id": "PR_1" })], None, 1))
                } else {
                    (
                        status,
                        json!({ "data": { "nodes": [{ "id": "PR_1", "additions": 99 }] }, "errors": [{ "type": "FORBIDDEN", "message": "Resource not accessible" }] }),
                    )
                }
            });
            let page = api(&server)
                .fetch_my_pull_requests(PullRequestFilter::Open, None)
                .unwrap();
            assert_eq!(page.pulls.len(), 1);
            assert!(page.pulls[0].stats.is_none());
            assert!(page.pulls[0].checks.is_none());
            assert_eq!(page.warnings.len(), 2);
            server.finish();
        }
    }

    #[test]
    fn wire_search_cap_stops_even_when_provider_advertises_more() {
        let server = Server::new(3, |incoming| {
            let body = request(incoming);
            if body["query"]
                .as_str()
                .unwrap()
                .contains("DiffyMyPullRequests(")
            {
                assert_eq!(body["variables"]["after"], "page-19");
                (
                    200,
                    core_page(
                        (0..50)
                            .map(|id| json!({ "id": format!("PR_{id}") }))
                            .collect(),
                        Some("page-20"),
                        1_200,
                    ),
                )
            } else {
                (200, json!({ "data": { "nodes": [] } }))
            }
        });
        let cursor = json!({ "filter": "closed", "after": "page-19", "seen": 950 }).to_string();
        let page = api(&server)
            .fetch_my_pull_requests(PullRequestFilter::Closed, Some(&cursor))
            .unwrap();
        assert_eq!(page.pulls.len(), 50);
        assert!(page.limit_reached);
        assert!(page.next_cursor.is_none());
        assert_eq!(page.total_count, Some(1_200));
        server.finish();
    }

    #[test]
    fn core_stats_and_checks_merge_by_node_id() {
        let core = json!({ "data": {
            "viewer": { "login": "me" },
            "search": { "nodes": [
                {
                    "id": "PR_1", "number": 12, "title": "Tidy the widget",
                    "url": "https://github.com/owner/repo/pull/12", "isDraft": false,
                    "updatedAt": "2026-01-02T00:00:00Z", "bodyText": "Line one\n\nline two",
                    "repository": { "nameWithOwner": "Owner/Repo" },
                    "headRefName": "feature", "baseRefName": "main",
                    "reviewDecision": "CHANGES_REQUESTED",
                    "latestReviews": { "nodes": [
                        { "state": "CHANGES_REQUESTED", "author": { "login": "alpha" } },
                        { "state": "COMMENTED", "author": { "login": "me" } }
                    ] },
                    "reviewRequests": { "nodes": [
                        { "requestedReviewer": { "__typename": "User", "login": "bravo" } },
                        { "requestedReviewer": { "__typename": "Team" } }
                    ] },
                    "reviewThreads": { "nodes": [
                        { "isResolved": false }, { "isResolved": true }, { "isResolved": false }
                    ] }
                },
                {}
            ] }
        } });
        let stats = json!({ "data": { "nodes": [
            { "id": "PR_1", "mergeable": "CONFLICTING", "additions": 5, "deletions": 2, "changedFiles": 1 }
        ] } });
        let checks = json!({ "data": { "nodes": [
            { "id": "PR_1", "commits": { "nodes": [ { "commit": { "statusCheckRollup": {
                "state": "FAILURE",
                "contexts": {
                    "totalCount": 30,
                    "checkRunCountsByState": [
                        { "state": "SUCCESS", "count": 20 },
                        { "state": "FAILURE", "count": 2 },
                        { "state": "IN_PROGRESS", "count": 7 }
                    ],
                    "statusContextCountsByState": [ { "state": "SUCCESS", "count": 1 } ],
                    "nodes": [ { "__typename": "CheckRun", "name": "lint", "conclusion": "FAILURE" } ]
                }
            } } } ] } }
        ] } });

        let mut pulls = parse_core(&core);
        assert_eq!(pulls.len(), 1);
        let pull = &mut pulls[0];
        pull.stats = parse_stats(&stats).remove(&pull.id);
        pull.checks = parse_checks(&checks).remove(&pull.id);

        assert_eq!(pull.repo_key(), "owner/repo");
        assert_eq!(pull.excerpt, "Line one line two");
        assert_eq!(pull.unresolved_threads, 2);
        assert_eq!(pull.requested_reviewers, 1);
        let reviewers: Vec<(&str, &str)> = pull
            .reviewers
            .iter()
            .map(|r| (r.actor.login.as_str(), r.state.as_str()))
            .collect();
        assert_eq!(
            reviewers,
            [("bravo", "REQUESTED"), ("alpha", "CHANGES_REQUESTED")]
        );
        assert_eq!(pull.stats.as_ref().unwrap().mergeable, "CONFLICTING");
        let checks = pull.checks.as_ref().unwrap();
        assert_eq!(
            (
                checks.total_count,
                checks.success_count,
                checks.failure_count,
                checks.pending_count
            ),
            (30, 21, 2, 7)
        );
        assert_eq!(checks.contexts[0].name, "lint");
    }

    #[test]
    fn excerpt_collapses_whitespace_and_truncates() {
        assert_eq!(excerpt("  a\n\n b  "), "a b");
        let long = "word ".repeat(100);
        let cut = excerpt(&long);
        assert!(cut.ends_with('\u{2026}'));
        assert!(cut.chars().count() <= EXCERPT_CHARS + 1);
    }
}
