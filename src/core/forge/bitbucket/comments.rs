use std::collections::HashMap;

use serde_json::Value;

use super::api::{login, str_at};
use crate::core::forge::model::{PullRequestReviewThread, PullRequestReviewThreadComment};
use crate::core::review::ReviewSide;

const DELETED_BODY: &str = "_This comment was deleted._";
// Parent links are followed to the thread root; a cycle or a runaway chain
// stops here.
const MAX_DEPTH: usize = 64;

/// Who is reading, for edit and delete permissions.
#[derive(Debug, Clone, Copy)]
pub(super) struct Viewer<'a> {
    pub signed_in: bool,
    pub uuid: Option<&'a str>,
}

/// Inline and file comment threads from Bitbucket's flat, chronological
/// comment list. General pull request comments (no `inline`) belong to the
/// conversation, not the diff, and are left out. Pending comments are part
/// of an unfinished review in the web UI that the API cannot publish.
pub(super) fn threads(comments: &[Value], viewer: Viewer<'_>) -> Vec<PullRequestReviewThread> {
    let comments: Vec<&Value> = comments
        .iter()
        .filter(|comment| !bool_at(comment, "/pending"))
        .collect();
    let by_id: HashMap<i64, &Value> = comments
        .iter()
        .filter_map(|comment| Some((id(comment)?, *comment)))
        .collect();
    let mut members: HashMap<i64, Vec<&Value>> = HashMap::new();
    let mut roots: Vec<&Value> = Vec::new();
    for comment in &comments {
        let Some(root) = root_id(comment, &by_id) else {
            continue;
        };
        if Some(root) == id(comment) {
            roots.push(comment);
        }
        members.entry(root).or_default().push(comment);
    }
    roots
        .into_iter()
        .filter(|root| root.get("inline").is_some_and(Value::is_object))
        .filter_map(|root| {
            let members = members.get(&id(root)?)?;
            thread(root, members, viewer)
        })
        .collect()
}

fn thread(root: &Value, members: &[&Value], viewer: Viewer<'_>) -> Option<PullRequestReviewThread> {
    let live = |comment: &&&Value| !bool_at(comment, "/deleted");
    if !members.iter().any(|comment| live(&comment)) {
        return None;
    }
    let inline = root.get("inline")?;
    let anchor = Anchor::read(inline);
    let resolved = root.get("resolution").is_some_and(Value::is_object);
    let comments = members
        .iter()
        .filter(|comment| live(comment) || id(comment) == id(root))
        .map(|comment| thread_comment(comment, &anchor, viewer))
        .collect();
    Some(PullRequestReviewThread {
        id: id(root)?.to_string(),
        path: str_at(inline, "/path"),
        line: anchor.line.filter(|_| !anchor.outdated),
        original_line: anchor.line.filter(|_| anchor.outdated),
        start_line: anchor.start.filter(|_| !anchor.outdated),
        original_start_line: anchor.start.filter(|_| anchor.outdated),
        side: anchor.side,
        start_side: anchor.start.and(anchor.side),
        subject_type: anchor.subject().to_owned(),
        is_collapsed: resolved,
        is_outdated: anchor.outdated,
        is_resolved: resolved,
        viewer_can_reply: viewer.signed_in,
        viewer_can_resolve: viewer.signed_in && !resolved,
        viewer_can_unresolve: viewer.signed_in && resolved,
        comments,
    })
}

fn thread_comment(
    comment: &Value,
    anchor: &Anchor,
    viewer: Viewer<'_>,
) -> PullRequestReviewThreadComment {
    let deleted = bool_at(comment, "/deleted");
    let user = comment.get("user");
    let own = !deleted
        && viewer.uuid.is_some_and(|uuid| {
            !uuid.is_empty() && user.is_some_and(|user| str_at(user, "/uuid") == uuid)
        });
    let parent = comment.pointer("/parent/id").and_then(Value::as_i64);
    PullRequestReviewThreadComment {
        id: id(comment).map(|id| id.to_string()).unwrap_or_default(),
        numeric_id: id(comment),
        reply_to_id: parent.map(|id| id.to_string()),
        reply_to_numeric_id: parent,
        author_login: login(user),
        author_avatar_url: user
            .map(|user| str_at(user, "/links/avatar/href"))
            .unwrap_or_default(),
        body: if deleted {
            DELETED_BODY.to_owned()
        } else {
            str_at(comment, "/content/raw")
        },
        path: comment
            .pointer("/inline/path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        line: anchor.line,
        original_line: None,
        start_line: anchor.start,
        original_start_line: None,
        subject_type: anchor.subject().to_owned(),
        url: str_at(comment, "/links/html/href"),
        created_at: str_at(comment, "/created_on"),
        updated_at: str_at(comment, "/updated_on"),
        outdated: anchor.outdated,
        state: String::new(),
        viewer_can_update: own,
        viewer_can_delete: own,
        reactions: Vec::new(),
    }
}

/// Where an `inline` object points. `to` is the new-file line and `from` the
/// old-file line; for multi-line comments they are the last lines and
/// `start_to` / `start_from` the first. A context line can carry both; the
/// new side wins, as it does on GitHub.
struct Anchor {
    side: Option<ReviewSide>,
    line: Option<u32>,
    start: Option<u32>,
    outdated: bool,
}

impl Anchor {
    fn read(inline: &Value) -> Self {
        let (side, line, start) = match (line_at(inline, "/to"), line_at(inline, "/from")) {
            (Some(to), _) => (
                Some(ReviewSide::New),
                Some(to),
                line_at(inline, "/start_to"),
            ),
            (None, Some(from)) => (
                Some(ReviewSide::Old),
                Some(from),
                line_at(inline, "/start_from"),
            ),
            (None, None) => (None, None, None),
        };
        Self {
            side,
            line,
            start: start.filter(|start| Some(*start) != line),
            outdated: bool_at(inline, "/outdated"),
        }
    }

    fn subject(&self) -> &'static str {
        if self.line.is_some() { "LINE" } else { "FILE" }
    }
}

fn root_id(comment: &Value, by_id: &HashMap<i64, &Value>) -> Option<i64> {
    let mut current = id(comment)?;
    let mut parent = comment.pointer("/parent/id").and_then(Value::as_i64);
    for _ in 0..MAX_DEPTH {
        let Some(next) = parent else {
            return Some(current);
        };
        current = next;
        parent = by_id
            .get(&next)
            .and_then(|comment| comment.pointer("/parent/id"))
            .and_then(Value::as_i64);
    }
    Some(current)
}

fn id(comment: &Value) -> Option<i64> {
    comment.get("id").and_then(Value::as_i64)
}

fn line_at(value: &Value, pointer: &str) -> Option<u32> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|line| u32::try_from(line).ok())
        .filter(|line| *line > 0)
}

fn bool_at(value: &Value, pointer: &str) -> bool {
    value
        .pointer(pointer)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ME: &str = "{me}";

    fn user(uuid: &str, nickname: &str) -> Value {
        json!({
            "type": "user",
            "uuid": uuid,
            "display_name": nickname.to_uppercase(),
            "nickname": nickname,
            "account_id": format!("557058:{nickname}"),
            "links": { "avatar": { "href": format!("https://avatar.example/{nickname}/128") } }
        })
    }

    fn comment(
        id: i64,
        parent: Option<i64>,
        inline: Option<Value>,
        by: Value,
        body: &str,
    ) -> Value {
        let mut value = json!({
            "id": id,
            "content": { "raw": body, "markup": "markdown", "html": "" },
            "user": by,
            "created_on": format!("2026-10-01T00:00:{id:02}+00:00"),
            "updated_on": format!("2026-10-01T00:00:{id:02}+00:00"),
            "deleted": false,
            "pending": false,
            "links": { "html": { "href": format!("https://bitbucket.org/acme/widgets/pull-requests/5/_/diff#comment-{id}") } }
        });
        if let Some(parent) = parent {
            value["parent"] = json!({ "id": parent });
        }
        if let Some(inline) = inline {
            value["inline"] = inline;
        }
        value
    }

    fn signed_in() -> Viewer<'static> {
        Viewer {
            signed_in: true,
            uuid: Some(ME),
        }
    }

    #[test]
    fn inline_comments_become_threads_with_nested_replies_folded_to_the_root() {
        let comments = vec![
            comment(1, None, None, user("{a}", "ada"), "General note"),
            comment(
                2,
                None,
                Some(json!({ "path": "src/lib.rs", "from": null, "to": 12 })),
                user("{a}", "ada"),
                "Why?",
            ),
            comment(
                3,
                Some(2),
                Some(json!({ "path": "src/lib.rs", "from": null, "to": 12 })),
                user(ME, "me"),
                "Because",
            ),
            comment(4, Some(3), None, user("{a}", "ada"), "Fair"),
        ];

        let threads = threads(&comments, signed_in());

        assert_eq!(threads.len(), 1);
        let thread = &threads[0];
        assert_eq!(thread.id, "2");
        assert_eq!(thread.path, "src/lib.rs");
        assert_eq!(
            (thread.side, thread.line, thread.start_line),
            (Some(ReviewSide::New), Some(12), None)
        );
        assert_eq!(thread.subject_type, "LINE");
        let bodies: Vec<&str> = thread.comments.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, ["Why?", "Because", "Fair"]);
        assert_eq!(thread.comments[2].reply_to_numeric_id, Some(3));
        assert_eq!(thread.comments[0].author_login, "ada");
        assert_eq!(
            thread.comments[0].author_avatar_url,
            "https://avatar.example/ada/128"
        );
        assert!(!thread.comments[0].viewer_can_update);
        assert!(thread.comments[1].viewer_can_update && thread.comments[1].viewer_can_delete);
        assert!(
            thread.viewer_can_reply && thread.viewer_can_resolve && !thread.viewer_can_unresolve
        );
    }

    #[test]
    fn old_side_multi_line_and_file_comments_anchor_like_github() {
        let comments = vec![
            comment(
                1,
                None,
                Some(json!({ "path": "a.rs", "from": 7, "to": null })),
                user("{a}", "ada"),
                "Removed",
            ),
            comment(
                2,
                None,
                Some(
                    json!({ "path": "a.rs", "from": null, "to": 14, "start_from": null, "start_to": 10 }),
                ),
                user("{a}", "ada"),
                "Range",
            ),
            comment(
                3,
                None,
                Some(json!({ "path": "b.rs" })),
                user("{a}", "ada"),
                "File",
            ),
            comment(
                4,
                None,
                Some(json!({ "path": "a.rs", "from": 3, "to": 4 })),
                user("{a}", "ada"),
                "Context",
            ),
        ];

        let threads = threads(&comments, signed_in());

        assert_eq!(
            (threads[0].side, threads[0].line),
            (Some(ReviewSide::Old), Some(7))
        );
        assert_eq!(
            (
                threads[1].side,
                threads[1].start_line,
                threads[1].line,
                threads[1].start_side
            ),
            (
                Some(ReviewSide::New),
                Some(10),
                Some(14),
                Some(ReviewSide::New)
            )
        );
        assert_eq!(
            (
                threads[2].side,
                threads[2].line,
                threads[2].subject_type.as_str()
            ),
            (None, None, "FILE")
        );
        assert_eq!(
            (threads[3].side, threads[3].line),
            (Some(ReviewSide::New), Some(4))
        );
    }

    #[test]
    fn resolved_outdated_deleted_and_pending_comments() {
        let mut resolved = comment(
            1,
            None,
            Some(json!({ "path": "a.rs", "to": 5, "outdated": true })),
            user("{a}", "ada"),
            "Old",
        );
        resolved["resolution"] = json!({ "type": "comment_resolution", "user": user("{b}", "bob"), "created_on": "2026-10-02T00:00:00+00:00" });
        let mut deleted_root = comment(
            2,
            None,
            Some(json!({ "path": "a.rs", "to": 9 })),
            user(ME, "me"),
            "",
        );
        deleted_root["deleted"] = json!(true);
        let reply = comment(3, Some(2), None, user("{a}", "ada"), "Still here");
        let mut lonely = comment(
            4,
            None,
            Some(json!({ "path": "a.rs", "to": 1 })),
            user("{a}", "ada"),
            "",
        );
        lonely["deleted"] = json!(true);
        let mut pending = comment(
            5,
            None,
            Some(json!({ "path": "a.rs", "to": 2 })),
            user(ME, "me"),
            "Draft",
        );
        pending["pending"] = json!(true);

        let threads = threads(
            &[resolved, deleted_root, reply, lonely, pending],
            signed_in(),
        );

        assert_eq!(threads.len(), 2);
        let old = &threads[0];
        assert!(old.is_resolved && old.is_collapsed && old.is_outdated);
        assert_eq!((old.line, old.original_line), (None, Some(5)));
        assert!(old.viewer_can_unresolve && !old.viewer_can_resolve);
        let kept = &threads[1];
        assert_eq!(kept.comments[0].body, DELETED_BODY);
        assert!(!kept.comments[0].viewer_can_update);
        assert_eq!(kept.comments[1].body, "Still here");
    }

    #[test]
    fn anonymous_readers_cannot_reply_or_edit() {
        let comments = vec![comment(
            1,
            None,
            Some(json!({ "path": "a.rs", "to": 1 })),
            user(ME, "me"),
            "Hi",
        )];
        let thread = &threads(
            &comments,
            Viewer {
                signed_in: false,
                uuid: None,
            },
        )[0];
        assert!(!thread.viewer_can_reply && !thread.viewer_can_resolve);
        assert!(!thread.comments[0].viewer_can_update);
    }

    #[test]
    fn a_parent_cycle_does_not_hang() {
        let comments = vec![
            comment(
                1,
                Some(2),
                Some(json!({ "path": "a.rs", "to": 1 })),
                user("{a}", "ada"),
                "A",
            ),
            comment(
                2,
                Some(1),
                Some(json!({ "path": "a.rs", "to": 1 })),
                user("{a}", "ada"),
                "B",
            ),
        ];
        let _ = threads(&comments, signed_in());
    }
}
