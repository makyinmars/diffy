use std::thread;
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use super::api::{BitbucketApi, login, pull_request_url, str_at};
use super::transport;
use crate::core::error::{DiffyError, Result};
use crate::core::forge::model::PullRequestReviewThreadComment;
use crate::core::review::{
    ReviewDecision, ReviewDraft, ReviewDraftId, ReviewDraftKind, ReviewSession, ReviewSide,
    ReviewTarget,
};

// Atlassian asks clients to send writes one at a time, a second apart.
const WRITE_SPACING: Duration = Duration::from_secs(1);

/// What a draft submission did: the drafts posted, and why it stopped when
/// it did not finish (the drafts left unposted, which may be none when only
/// the review decision failed).
#[derive(Debug)]
pub struct DraftSubmission {
    pub posted: Vec<ReviewDraftId>,
    pub failure: Option<(Vec<ReviewDraftId>, String)>,
}

impl BitbucketApi {
    /// Bitbucket has no pending review to submit, so post each pending draft
    /// as a comment, then approve or request changes.
    pub fn submit_drafts(
        &self,
        session: &ReviewSession,
        decision: ReviewDecision,
    ) -> DraftSubmission {
        let pr = &session.target;
        let drafts: Vec<&ReviewDraft> = session.pending_drafts().collect();
        let mut posted = Vec::with_capacity(drafts.len());
        for (index, draft) in drafts.iter().enumerate() {
            if index > 0 {
                thread::sleep(WRITE_SPACING);
            }
            let result =
                draft_comment(session, draft).and_then(|body| self.post_comment(pr, &body));
            if let Err(error) = result {
                let unposted = drafts[index..].iter().map(|draft| draft.id).collect();
                let message = if posted.is_empty() {
                    error.user_message()
                } else {
                    format!(
                        "Posted {} of {} comments. {}",
                        posted.len(),
                        drafts.len(),
                        error.user_message()
                    )
                };
                return DraftSubmission {
                    posted,
                    failure: Some((unposted, message)),
                };
            }
            posted.push(draft.id);
        }
        let (path, verb) = match decision {
            ReviewDecision::Comment => {
                return DraftSubmission {
                    posted,
                    failure: None,
                };
            }
            ReviewDecision::Approve => ("/approve", "approving"),
            ReviewDecision::RequestChanges => ("/request-changes", "requesting changes"),
        };
        if !posted.is_empty() {
            thread::sleep(WRITE_SPACING);
        }
        let failure = transport::send(
            self.credentials(),
            Method::POST,
            &pull_request_url(pr, path),
            None,
            "Bitbucket review decision",
        )
        .err()
        .map(|error| {
            let message = if posted.is_empty() {
                error.user_message()
            } else {
                format!(
                    "Comments posted, but {verb} failed. {}",
                    error.user_message()
                )
            };
            (Vec::new(), message)
        });
        DraftSubmission { posted, failure }
    }

    pub fn post_comment(&self, pr: &ReviewTarget, body: &Value) -> Result<Value> {
        let text = transport::send(
            self.credentials(),
            Method::POST,
            &pull_request_url(pr, "/comments"),
            Some(body),
            "Bitbucket comment create",
        )?;
        serde_json::from_str(&text).map_err(Into::into)
    }

    pub fn reply(
        &self,
        pr: &ReviewTarget,
        thread_id: &str,
        body: &str,
    ) -> Result<PullRequestReviewThreadComment> {
        let parent = comment_id(thread_id)?;
        let value = self.post_comment(
            pr,
            &json!({ "content": { "raw": body }, "parent": { "id": parent } }),
        )?;
        Ok(posted_comment(&value))
    }

    pub fn add_general_comment(&self, pr: &ReviewTarget, body: &str) -> Result<()> {
        self.post_comment(pr, &json!({ "content": { "raw": body } }))?;
        Ok(())
    }

    pub fn update_comment(
        &self,
        pr: &ReviewTarget,
        id: &str,
        body: &str,
    ) -> Result<PullRequestReviewThreadComment> {
        let id = comment_id(id)?;
        let text = transport::send(
            self.credentials(),
            Method::PUT,
            &pull_request_url(pr, &format!("/comments/{id}")),
            Some(&json!({ "content": { "raw": body } })),
            "Bitbucket comment update",
        )?;
        Ok(posted_comment(&serde_json::from_str(&text)?))
    }

    pub fn delete_comment(&self, pr: &ReviewTarget, id: &str) -> Result<()> {
        let id = comment_id(id)?;
        transport::send(
            self.credentials(),
            Method::DELETE,
            &pull_request_url(pr, &format!("/comments/{id}")),
            None,
            "Bitbucket comment delete",
        )?;
        Ok(())
    }

    /// Resolve or reopen the thread rooted at `thread_id`. Resolving an
    /// already resolved thread is done.
    pub fn set_resolution(&self, pr: &ReviewTarget, thread_id: &str, resolved: bool) -> Result<()> {
        let id = comment_id(thread_id)?;
        let (method, accepted, context): (Method, &[StatusCode], &str) = if resolved {
            (
                Method::POST,
                &[StatusCode::CONFLICT],
                "Bitbucket thread resolve",
            )
        } else {
            (Method::DELETE, &[], "Bitbucket thread reopen")
        };
        transport::send_accepting(
            self.credentials(),
            method,
            &pull_request_url(pr, &format!("/comments/{id}/resolve")),
            None,
            context,
            accepted,
        )?;
        Ok(())
    }
}

/// The comment a pending draft posts. Inline drafts anchor to `to` on the
/// new side or `from` on the old side; a multi-line draft names its first
/// line in `start_to` / `start_from`. A reply names the thread's root
/// comment as its parent. Only `content.raw` is sent: Bitbucket rejects
/// `content.markup`.
pub(super) fn draft_comment(session: &ReviewSession, draft: &ReviewDraft) -> Result<Value> {
    match &draft.kind {
        ReviewDraftKind::General => Ok(json!({ "content": { "raw": draft.body } })),
        ReviewDraftKind::Reply { thread_id } => {
            let parent = session.thread_remote_id(thread_id).ok_or_else(|| {
                DiffyError::General("This review thread cannot be replied to.".to_owned())
            })?;
            Ok(json!({
                "content": { "raw": draft.body },
                "parent": { "id": comment_id(&parent)? },
            }))
        }
        ReviewDraftKind::InlineComment => {
            let anchor = draft.anchor.as_ref().ok_or_else(|| {
                DiffyError::General("inline review draft is missing an anchor".to_owned())
            })?;
            let mut inline = json!({ "path": anchor.path });
            if let (Some(side), Some(range)) = (anchor.side, anchor.line_range) {
                let (line, start) = match side {
                    ReviewSide::New => ("to", "start_to"),
                    ReviewSide::Old => ("from", "start_from"),
                };
                inline[line] = json!(range.end_inclusive());
                if let Some(first) = range.multi_line_start() {
                    inline[start] = json!(first);
                }
            }
            Ok(json!({ "content": { "raw": draft.body }, "inline": inline }))
        }
    }
}

fn comment_id(id: &str) -> Result<i64> {
    id.trim()
        .parse()
        .map_err(|_| DiffyError::Parse(format!("not a Bitbucket comment id: {id}")))
}

fn posted_comment(value: &Value) -> PullRequestReviewThreadComment {
    let id = value.get("id").and_then(Value::as_i64);
    let parent = value.pointer("/parent/id").and_then(Value::as_i64);
    PullRequestReviewThreadComment {
        id: id.map(|id| id.to_string()).unwrap_or_default(),
        numeric_id: id,
        reply_to_id: parent.map(|id| id.to_string()),
        reply_to_numeric_id: parent,
        author_login: login(value.get("user")),
        body: str_at(value, "/content/raw"),
        path: str_at(value, "/inline/path"),
        url: str_at(value, "/links/html/href"),
        created_at: str_at(value, "/created_on"),
        updated_at: str_at(value, "/updated_on"),
        viewer_can_update: true,
        viewer_can_delete: true,
        ..PullRequestReviewThreadComment::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::forge::model::{
        PullRequestInfo, PullRequestReviewData, PullRequestReviewThread,
    };
    use crate::core::review::{ReviewAnchor, ReviewForge, ReviewLineRange, ReviewThreadId};

    fn session() -> ReviewSession {
        ReviewSession::new(
            ReviewTarget {
                forge: ReviewForge::Bitbucket,
                owner: "acme".to_owned(),
                repo: "widgets".to_owned(),
                number: 5,
            },
            PullRequestInfo::default(),
        )
    }

    fn body(session: &ReviewSession, id: ReviewDraftId) -> Value {
        draft_comment(session, &session.drafts[&id]).unwrap()
    }

    #[test]
    fn drafts_post_to_the_side_and_lines_they_anchor() {
        let mut session = session();
        let new_line = session.create_inline_draft(
            ReviewAnchor::inline("src/lib.rs", ReviewSide::New, ReviewLineRange::new(12, 1)),
            "Single",
        );
        let old_range = session.create_inline_draft(
            ReviewAnchor::inline("src/lib.rs", ReviewSide::Old, ReviewLineRange::new(4, 3)),
            "Range",
        );
        let general = session.create_general_draft("Overall");

        assert_eq!(
            body(&session, new_line),
            json!({ "content": { "raw": "Single" }, "inline": { "path": "src/lib.rs", "to": 12 } })
        );
        assert_eq!(
            body(&session, old_range),
            json!({ "content": { "raw": "Range" }, "inline": { "path": "src/lib.rs", "from": 6, "start_from": 4 } })
        );
        assert_eq!(
            body(&session, general),
            json!({ "content": { "raw": "Overall" } })
        );
    }

    #[test]
    fn replies_name_their_thread_root_as_parent() {
        let mut session = session();
        session.apply_review_data(PullRequestReviewData {
            threads: vec![PullRequestReviewThread {
                id: "7".to_owned(),
                path: "src/lib.rs".to_owned(),
                line: Some(3),
                side: Some(ReviewSide::New),
                subject_type: "LINE".to_owned(),
                ..PullRequestReviewThread::default()
            }],
            ..PullRequestReviewData::default()
        });
        let thread = |id: &str| ReviewThreadId::remote(ReviewForge::Bitbucket, id);
        let reply = session.create_reply_draft(thread("7"), "Hi");
        let orphan = session.create_reply_draft(thread("8"), "Lost");

        assert_eq!(
            body(&session, reply),
            json!({ "content": { "raw": "Hi" }, "parent": { "id": 7 } })
        );
        assert!(draft_comment(&session, &session.drafts[&orphan]).is_err());
    }

    #[test]
    fn comment_ids_must_be_numbers() {
        assert_eq!(comment_id(" 42 ").unwrap(), 42);
        assert!(comment_id("PRRC_abc").is_err());
    }
}
