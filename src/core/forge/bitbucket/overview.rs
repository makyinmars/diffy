use serde_json::Value;

use super::api::{BitbucketApi, login, parse_pull_request, pull_request_url, str_at};
use super::transport;
use crate::core::error::Result;
use crate::core::forge::model::{
    PullRequestActor, PullRequestCheckContext, PullRequestCheckSummary, PullRequestOverview,
    PullRequestReviewer, PullRequestTimelineItem, PullRequestTimelineKind,
};
use crate::core::review::ReviewTarget;

impl BitbucketApi {
    /// The pull request, its reviewers, build statuses and conversation:
    /// general comments, approvals and change requests from the activity
    /// log, and its commits. Inline comments live in the review threads.
    pub fn fetch_overview(&self, pr: &ReviewTarget) -> Result<PullRequestOverview> {
        let credentials = self.credentials();
        let value = self.pull_request_value(pr)?;
        let mut overview = parse_overview(&value);
        let mut info = parse_pull_request(&value)?;
        self.add_diffstat(pr, &value, &mut info);
        overview.additions = info.additions;
        overview.deletions = info.deletions;
        overview.changed_files = info.changed_files;

        // Activity rejects some page lengths, so it is read at its default.
        let activity = transport::get_pages(
            credentials,
            &pull_request_url(pr, "/activity"),
            "Bitbucket activity fetch",
        )?;
        let commits = transport::get_pages(
            credentials,
            &pull_request_url(pr, "/commits?pagelen=100"),
            "Bitbucket commits fetch",
        )
        .unwrap_or_else(|error| {
            tracing::warn!("bitbucket: commits unavailable: {error}");
            Vec::new()
        });
        let statuses = transport::get_pages(
            credentials,
            &pull_request_url(pr, "/statuses?pagelen=100"),
            "Bitbucket statuses fetch",
        )
        .unwrap_or_else(|error| {
            tracing::warn!("bitbucket: statuses unavailable: {error}");
            Vec::new()
        });
        overview.commit_count = i32::try_from(commits.len()).unwrap_or(i32::MAX);
        overview.checks = check_summary(&statuses, &info.head_sha);
        overview.timeline = timeline(&activity, &commits, &overview.url);
        overview.timeline_total = overview.timeline.len();
        Ok(overview)
    }
}

/// The overview fields the pull request object carries. States use the
/// model's vocabulary: OPEN, MERGED, CLOSED (declined or superseded).
pub(super) fn parse_overview(value: &Value) -> PullRequestOverview {
    let state = match str_at(value, "/state").as_str() {
        "OPEN" => "OPEN",
        "MERGED" => "MERGED",
        _ => "CLOSED",
    };
    let author = actor(value.get("author"));
    let reviewers = reviewers(value);
    let decision = if reviewers.iter().any(|r| r.state == "CHANGES_REQUESTED") {
        Some("CHANGES_REQUESTED")
    } else if reviewers.iter().any(|r| r.state == "APPROVED") {
        Some("APPROVED")
    } else {
        None
    };
    let body = str_at(value, "/summary/raw");
    PullRequestOverview {
        title: str_at(value, "/title"),
        body: if body.is_empty() {
            str_at(value, "/description")
        } else {
            body
        },
        url: str_at(value, "/links/html/href"),
        state: state.to_owned(),
        is_draft: value.get("draft").and_then(Value::as_bool).unwrap_or(false),
        author,
        created_at: str_at(value, "/created_on"),
        updated_at: str_at(value, "/updated_on"),
        base_branch: str_at(value, "/destination/branch/name"),
        head_branch: str_at(value, "/source/branch/name"),
        // Bitbucket reports mergeability only through merge checks; a change
        // request blocks the merge the way GitHub's review decision does.
        merge_state_status: if decision == Some("CHANGES_REQUESTED") {
            "BLOCKED".to_owned()
        } else {
            String::new()
        },
        review_decision: decision.map(ToOwned::to_owned),
        reviewers,
        detailed: true,
        ..PullRequestOverview::default()
    }
}

/// Requested reviewers and anyone who approved, requested changes or
/// commented, without the author and without repeats. People are matched by
/// account uuid: nicknames are display names and need not be unique.
pub(super) fn reviewers(value: &Value) -> Vec<PullRequestReviewer> {
    let author = identity(value.get("author"));
    let mut seen: Vec<String> = Vec::new();
    let mut reviewers: Vec<PullRequestReviewer> = Vec::new();
    let participants = value
        .get("participants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|participant| {
            let state = match participant.get("state").and_then(Value::as_str) {
                Some("approved") => "APPROVED",
                Some("changes_requested") => "CHANGES_REQUESTED",
                _ if str_at(participant, "/role") == "REVIEWER" => "REQUESTED",
                _ => "COMMENTED",
            };
            (participant.get("user"), state)
        });
    let requested = value
        .get("reviewers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|account| (Some(account), "REQUESTED"));
    for (account, state) in participants.chain(requested) {
        let actor = actor(account);
        let id = identity(account);
        if actor.login.is_empty() || id.is_empty() || id == author || seen.contains(&id) {
            continue;
        }
        seen.push(id);
        reviewers.push(PullRequestReviewer {
            actor,
            state: state.to_owned(),
        });
    }
    reviewers
}

/// Who an account is: its uuid, else its account id, else its name.
fn identity(account: Option<&Value>) -> String {
    let Some(account) = account.filter(|account| !account.is_null()) else {
        return String::new();
    };
    ["/uuid", "/account_id"]
        .into_iter()
        .map(|pointer| str_at(account, pointer))
        .find(|value| !value.is_empty() && value != "{}")
        .unwrap_or_else(|| login(Some(account)))
}

/// Build statuses on the pull request's head commit, newest per key.
/// Bitbucket's SUCCESSFUL/FAILED/INPROGRESS/STOPPED map onto the model's
/// SUCCESS/FAILURE/PENDING/CANCELLED.
pub(super) fn check_summary(statuses: &[Value], head_sha: &str) -> PullRequestCheckSummary {
    let mut latest: Vec<&Value> = Vec::new();
    for status in statuses.iter().filter(|status| on_commit(status, head_sha)) {
        let key = str_at(status, "/key");
        match latest.iter_mut().find(|known| str_at(known, "/key") == key) {
            Some(known) if str_at(status, "/updated_on") > str_at(known, "/updated_on") => {
                *known = status
            }
            Some(_) => {}
            None => latest.push(status),
        }
    }
    let mut summary = PullRequestCheckSummary {
        total_count: i32::try_from(latest.len()).unwrap_or(i32::MAX),
        contexts: latest
            .iter()
            .map(|status| PullRequestCheckContext {
                name: Some(str_at(status, "/name"))
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| str_at(status, "/key")),
                state: match str_at(status, "/state").as_str() {
                    "SUCCESSFUL" => "SUCCESS",
                    "FAILED" => "FAILURE",
                    "STOPPED" => "CANCELLED",
                    _ => "PENDING",
                }
                .to_owned(),
                details_url: str_at(status, "/url"),
            })
            .collect(),
        ..PullRequestCheckSummary::default()
    };
    let contexts = std::mem::take(&mut summary.contexts);
    for context in &contexts {
        summary.add_state(&context.state, 1);
    }
    summary.contexts = contexts;
    summary.state = if summary.total_count == 0 {
        None
    } else if summary.failure_count > 0 {
        Some("FAILURE".to_owned())
    } else if summary.pending_count > 0 {
        Some("PENDING".to_owned())
    } else {
        Some("SUCCESS".to_owned())
    };
    summary
}

/// Whether a status belongs to `head_sha` (12 characters in Bitbucket's
/// pull request object; the status links the full hash).
fn on_commit(status: &Value, head_sha: &str) -> bool {
    let Some(commit) = status
        .pointer("/links/commit/href")
        .and_then(Value::as_str)
        .and_then(|href| href.rsplit('/').next())
    else {
        return true;
    };
    let shared = head_sha.len().min(commit.len());
    head_sha.is_empty()
        || matches!(
            (commit.get(..shared), head_sha.get(..shared)),
            (Some(commit), Some(head)) if commit.eq_ignore_ascii_case(head)
        )
}

/// The conversation in time order: general comments, approvals and change
/// requests from the activity log, and commits.
pub(super) fn timeline(
    activity: &[Value],
    commits: &[Value],
    url: &str,
) -> Vec<PullRequestTimelineItem> {
    let comments: Vec<&Value> = activity
        .iter()
        .filter_map(|entry| entry.get("comment"))
        .collect();
    let mut items: Vec<PullRequestTimelineItem> = activity
        .iter()
        .filter(|entry| {
            entry
                .get("comment")
                .is_none_or(|comment| in_conversation(comment, &comments))
        })
        .filter_map(|entry| activity_item(entry, url))
        .chain(commits.iter().filter_map(commit_item))
        .collect();
    items.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    items
}

/// Whether a comment belongs to the conversation: published, not deleted,
/// and not part of an inline thread. Replies may omit `inline`, so the
/// thread root decides. Pending comments are an unfinished web review.
fn in_conversation(comment: &Value, comments: &[&Value]) -> bool {
    let flag =
        |comment: &Value, key: &str| comment.get(key).and_then(Value::as_bool).unwrap_or(false);
    if flag(comment, "deleted") || flag(comment, "pending") {
        return false;
    }
    let mut current = comment;
    for _ in 0..64 {
        if current.get("inline").is_some_and(Value::is_object) {
            return false;
        }
        let Some(parent) = current.pointer("/parent/id").and_then(Value::as_i64) else {
            return true;
        };
        match comments
            .iter()
            .find(|known| known.get("id").and_then(Value::as_i64) == Some(parent))
        {
            Some(known) => current = known,
            None => return true,
        }
    }
    true
}

fn activity_item(entry: &Value, url: &str) -> Option<PullRequestTimelineItem> {
    if let Some(comment) = entry.get("comment") {
        return Some(PullRequestTimelineItem {
            id: comment.get("id").map(Value::to_string).unwrap_or_default(),
            kind: PullRequestTimelineKind::Comment,
            author: actor(comment.get("user")),
            body: str_at(comment, "/content/raw"),
            created_at: str_at(comment, "/created_on"),
            url: str_at(comment, "/links/html/href"),
        });
    }
    let (decision, state) = [
        ("approval", "APPROVED"),
        ("changes_requested", "CHANGES_REQUESTED"),
    ]
    .into_iter()
    .find_map(|(key, state)| Some((entry.get(key)?, state)))?;
    let author = actor(decision.get("user"));
    let date = str_at(decision, "/date");
    Some(PullRequestTimelineItem {
        id: format!("{state}:{}:{date}", author.login),
        kind: PullRequestTimelineKind::Review {
            state: state.to_owned(),
            comments: 0,
        },
        author,
        body: String::new(),
        created_at: date,
        url: url.to_owned(),
    })
}

fn commit_item(commit: &Value) -> Option<PullRequestTimelineItem> {
    let hash = str_at(commit, "/hash");
    if hash.is_empty() {
        return None;
    }
    let user = commit
        .pointer("/author/user")
        .filter(|user| !user.is_null());
    let author = match user {
        Some(_) => actor(user),
        None => PullRequestActor {
            login: author_name(&str_at(commit, "/author/raw")),
            avatar_url: String::new(),
        },
    };
    Some(PullRequestTimelineItem {
        kind: PullRequestTimelineKind::Commit {
            short_oid: hash.chars().take(7).collect(),
        },
        author,
        body: str_at(commit, "/message")
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        created_at: str_at(commit, "/date"),
        url: str_at(commit, "/links/html/href"),
        id: hash,
    })
}

/// `Name <email>` as git records it, without the email.
fn author_name(raw: &str) -> String {
    raw.split_once('<')
        .map_or(raw, |(name, _)| name)
        .trim()
        .to_owned()
}

pub(super) fn actor(account: Option<&Value>) -> PullRequestActor {
    PullRequestActor {
        login: login(account),
        avatar_url: account
            .map(|account| str_at(account, "/links/avatar/href"))
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn user(nickname: &str) -> Value {
        user_with_uuid(nickname, &format!("{{{nickname}}}"))
    }

    fn user_with_uuid(nickname: &str, uuid: &str) -> Value {
        json!({
            "type": "user",
            "display_name": nickname.to_uppercase(),
            "nickname": nickname,
            "uuid": uuid,
            "links": { "avatar": { "href": format!("https://avatar.example/{nickname}") } }
        })
    }

    #[test]
    fn overview_maps_state_body_and_review_decisions() {
        let value = json!({
            "title": "Tidy the widget",
            "state": "OPEN",
            "draft": false,
            "summary": { "raw": "Body text", "markup": "markdown" },
            "author": user("ada"),
            "created_on": "2026-10-01T00:00:00+00:00",
            "updated_on": "2026-10-02T00:00:00+00:00",
            "source": { "branch": { "name": "feature" } },
            "destination": { "branch": { "name": "main" } },
            "links": { "html": { "href": "https://bitbucket.org/acme/widgets/pull-requests/5" } },
            "reviewers": [user("bob"), user("cy"), user("ada")],
            "participants": [
                { "user": user("bob"), "role": "REVIEWER", "approved": false, "state": "changes_requested" },
                { "user": user("dee"), "role": "PARTICIPANT", "approved": true, "state": "approved" },
                { "user": user("eve"), "role": "PARTICIPANT", "approved": false, "state": null },
                { "user": user("ada"), "role": "PARTICIPANT", "approved": false, "state": null },
                { "user": user_with_uuid("ada", "{other-ada}"), "role": "REVIEWER", "approved": true, "state": "approved" },
                { "user": user("bob"), "role": "REVIEWER", "approved": false, "state": null }
            ]
        });

        let overview = parse_overview(&value);

        assert_eq!(
            (overview.state.as_str(), overview.body.as_str()),
            ("OPEN", "Body text")
        );
        assert_eq!(
            overview.url,
            "https://bitbucket.org/acme/widgets/pull-requests/5"
        );
        let reviewers: Vec<(&str, &str)> = overview
            .reviewers
            .iter()
            .map(|r| (r.actor.login.as_str(), r.state.as_str()))
            .collect();
        assert_eq!(
            reviewers,
            [
                ("bob", "CHANGES_REQUESTED"),
                ("dee", "APPROVED"),
                ("eve", "COMMENTED"),
                ("ada", "APPROVED"),
                ("cy", "REQUESTED")
            ]
        );
        assert_eq!(
            overview.review_decision.as_deref(),
            Some("CHANGES_REQUESTED")
        );
        assert_eq!(overview.merge_state_status, "BLOCKED");
        assert!(overview.labels.is_empty());
    }

    #[test]
    fn declined_pull_requests_are_closed_and_description_backs_up_summary() {
        let overview = parse_overview(&json!({ "state": "DECLINED", "description": "Old body" }));
        assert_eq!(
            (overview.state.as_str(), overview.body.as_str()),
            ("CLOSED", "Old body")
        );
        let merged = parse_overview(&json!({ "state": "MERGED" }));
        assert_eq!(merged.state, "MERGED");
    }

    #[test]
    fn checks_keep_the_newest_status_per_key_on_the_head_commit() {
        let status = |key: &str, state: &str, updated: &str, commit: &str| {
            json!({
                "type": "build", "key": key, "name": format!("{key} build"), "state": state,
                "url": format!("https://ci.example/{key}"), "updated_on": updated,
                "links": { "commit": { "href": format!("https://api.bitbucket.org/2.0/repositories/acme/widgets/commit/{commit}") } }
            })
        };
        let head = "1a2b3c4d5e6f";
        let full = "1a2b3c4d5e6f7a8b9c0d1a2b3c4d5e6f7a8b9c0d";
        let statuses = [
            status("lint", "FAILED", "2026-10-01T00:00:00+00:00", full),
            status("lint", "SUCCESSFUL", "2026-10-02T00:00:00+00:00", full),
            status("test", "INPROGRESS", "2026-10-02T00:00:00+00:00", full),
            status(
                "old",
                "FAILED",
                "2026-09-01T00:00:00+00:00",
                "ffffffffffff0000000000000000000000000000",
            ),
        ];

        let summary = check_summary(&statuses, head);

        assert_eq!(summary.total_count, 2);
        assert_eq!(
            (
                summary.success_count,
                summary.failure_count,
                summary.pending_count
            ),
            (1, 0, 1)
        );
        assert_eq!(summary.state.as_deref(), Some("PENDING"));
        assert_eq!(summary.contexts[0].name, "lint build");
        assert_eq!(summary.contexts[0].details_url, "https://ci.example/lint");
        assert_eq!(check_summary(&[], head).state, None);
        let stopped = check_summary(&[status("x", "STOPPED", "", full)], head);
        assert_eq!(stopped.state.as_deref(), Some("FAILURE"));
    }

    #[test]
    fn timeline_merges_comments_reviews_and_commits_in_time_order() {
        let activity = [
            json!({ "comment": { "id": 9, "content": { "raw": "Looks fine" }, "user": user("cy"),
                    "created_on": "2026-10-03T00:00:00+00:00",
                    "links": { "html": { "href": "https://bitbucket.org/c9" } } },
                    "pull_request": { "id": 5 } }),
            json!({ "comment": { "id": 10, "content": { "raw": "Inline" }, "user": user("cy"),
                    "inline": { "path": "a.rs", "to": 1 }, "created_on": "2026-10-03T00:00:01+00:00" } }),
            json!({ "comment": { "id": 11, "deleted": true, "content": { "raw": "" }, "user": user("cy"),
                    "created_on": "2026-10-03T00:00:02+00:00" } }),
            json!({ "comment": { "id": 12, "parent": { "id": 10 }, "content": { "raw": "Inline reply" },
                    "user": user("dee"), "created_on": "2026-10-03T00:00:03+00:00" } }),
            json!({ "comment": { "id": 13, "pending": true, "content": { "raw": "Draft" },
                    "user": user("dee"), "created_on": "2026-10-03T00:00:04+00:00" } }),
            json!({ "comment": { "id": 14, "parent": { "id": 9 }, "content": { "raw": "Agreed" },
                    "user": user("dee"), "created_on": "2026-10-03T00:00:05+00:00" } }),
            json!({ "approval": { "date": "2026-10-04T00:00:00+00:00", "user": user("dee") } }),
            json!({ "changes_requested": { "date": "2026-10-05T00:00:00+00:00", "user": user("bob") } }),
            json!({ "update": { "state": "OPEN", "date": "2026-10-01T00:00:00+00:00", "author": user("ada") } }),
        ];
        let commits = [json!({
            "hash": "abcdef1234567890abcdef1234567890abcdef12",
            "message": "Add widget\n\nDetails",
            "date": "2026-10-02T00:00:00+00:00",
            "author": { "raw": "Ada Lovelace <ada@example.com>", "user": null },
            "links": { "html": { "href": "https://bitbucket.org/acme/widgets/commits/abcdef1" } }
        })];

        let items = timeline(
            &activity,
            &commits,
            "https://bitbucket.org/acme/widgets/pull-requests/5",
        );

        let kinds: Vec<_> = items
            .iter()
            .map(|item| (&item.kind, item.author.login.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (
                    &PullRequestTimelineKind::Commit {
                        short_oid: "abcdef1".to_owned()
                    },
                    "Ada Lovelace"
                ),
                (&PullRequestTimelineKind::Comment, "cy"),
                (&PullRequestTimelineKind::Comment, "dee"),
                (
                    &PullRequestTimelineKind::Review {
                        state: "APPROVED".to_owned(),
                        comments: 0
                    },
                    "dee"
                ),
                (
                    &PullRequestTimelineKind::Review {
                        state: "CHANGES_REQUESTED".to_owned(),
                        comments: 0
                    },
                    "bob"
                ),
            ]
        );
        assert_eq!(items[0].body, "Add widget");
        assert_eq!(items[1].body, "Looks fine");
        assert_eq!(
            items[3].url,
            "https://bitbucket.org/acme/widgets/pull-requests/5"
        );
    }
}
