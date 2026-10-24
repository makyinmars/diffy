use std::collections::BTreeMap;

use std::ops::Range;

use diffy_diff::{DiffDocument, ObjectId};
use serde::{Deserialize, Serialize};

use crate::contracts::shared::{FileKey, FileRevision, Side, SourceTarget};
use crate::core::compare::stats::CompareFilePaths;
use crate::core::forge::model::{
    PullRequestInfo, PullRequestReviewComment, PullRequestReviewData, PullRequestReviewMetadata,
    PullRequestReviewSummary, PullRequestReviewThread, PullRequestReviewThreadComment,
};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize,
)]
pub enum ReviewForge {
    #[default]
    GitHub,
    Bitbucket,
}

impl ReviewForge {
    /// Lowercase key for storage keys, review ids, keyring accounts and file
    /// names. Persisted GitHub data depends on `"github"`.
    pub const fn key(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Bitbucket => "bitbucket",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::Bitbucket => "Bitbucket",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub struct ReviewTarget {
    pub forge: ReviewForge,
    pub owner: String,
    pub repo: String,
    pub number: i32,
}

impl ReviewTarget {
    pub fn github(owner: impl Into<String>, repo: impl Into<String>, number: i32) -> Self {
        Self {
            forge: ReviewForge::GitHub,
            owner: owner.into(),
            repo: repo.into(),
            number,
        }
    }

    pub fn web_url(&self) -> String {
        match self.forge {
            ReviewForge::GitHub => format!(
                "https://github.com/{}/{}/pull/{}",
                self.owner, self.repo, self.number
            ),
            ReviewForge::Bitbucket => format!(
                "https://bitbucket.org/{}/{}/pull-requests/{}",
                self.owner, self.repo, self.number
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub struct ReviewSessionKey {
    pub target: ReviewTarget,
    pub head_sha: String,
}

impl ReviewSessionKey {
    pub fn new(target: ReviewTarget, head_sha: impl Into<String>) -> Self {
        Self {
            target,
            head_sha: head_sha.into(),
        }
    }

    pub fn storage_key(&self) -> String {
        format!(
            "{}/{}/{}/{}/{}",
            self.target.forge.key(),
            sanitize_key_part(&self.target.owner),
            sanitize_key_part(&self.target.repo),
            self.target.number,
            sanitize_key_part(&self.head_sha)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub enum ReviewSide {
    Old,
    New,
}

impl From<ReviewSide> for Side {
    fn from(side: ReviewSide) -> Self {
        match side {
            ReviewSide::Old => Self::Old,
            ReviewSide::New => Self::New,
        }
    }
}

impl From<Side> for ReviewSide {
    fn from(side: Side) -> Self {
        match side {
            Side::Old => Self::Old,
            Side::New => Self::New,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub struct ReviewLineRange {
    /// One-based source line number.
    pub start: u32,
    pub len: u32,
}

impl ReviewLineRange {
    pub const fn new(start: u32, len: u32) -> Self {
        Self { start, len }
    }

    pub fn from_inclusive(start: u32, end: u32) -> Self {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        Self {
            start,
            len: end.saturating_sub(start).saturating_add(1),
        }
    }

    pub const fn end_exclusive(self) -> u32 {
        self.start.saturating_add(self.len)
    }

    pub const fn end_inclusive(self) -> u32 {
        self.end_exclusive().saturating_sub(1)
    }

    /// The first line when the range spans several lines.
    pub const fn multi_line_start(self) -> Option<u32> {
        if self.len > 1 { Some(self.start) } else { None }
    }

    pub fn source_lines(self) -> Range<u32> {
        let start = self.start.saturating_sub(1);
        start..start.saturating_add(self.len.max(1))
    }

    pub fn from_source_lines(lines: &Range<u32>) -> Self {
        Self {
            start: lines.start.saturating_add(1),
            len: lines.end.saturating_sub(lines.start).max(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub enum ReviewSubject {
    Line,
    File,
    Unknown,
}

impl ReviewSubject {
    fn from_subject_type(value: Option<&str>) -> Self {
        match value.unwrap_or_default().to_ascii_lowercase().as_str() {
            "line" => Self::Line,
            "file" => Self::File,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub struct ReviewAnchor {
    pub path: String,
    pub side: Option<ReviewSide>,
    pub line_range: Option<ReviewLineRange>,
    pub original_line_range: Option<ReviewLineRange>,
    pub subject: ReviewSubject,
    pub commit_id: Option<String>,
    pub original_commit_id: Option<String>,
}

impl ReviewAnchor {
    pub fn inline(path: impl Into<String>, side: ReviewSide, line_range: ReviewLineRange) -> Self {
        Self {
            path: path.into(),
            side: Some(side),
            line_range: Some(line_range),
            original_line_range: None,
            subject: ReviewSubject::Line,
            commit_id: None,
            original_commit_id: None,
        }
    }

    pub fn from_comment(comment: &PullRequestReviewComment) -> Option<Self> {
        if comment.path.is_empty() {
            return None;
        }

        let side = comment.side.or(comment.start_side);
        let line_range = review_line_range(comment.start_line, comment.line);
        let original_line_range =
            review_line_range(comment.original_start_line, comment.original_line);
        let subject = ReviewSubject::from_subject_type(comment.subject_type.as_deref());
        if side.is_none() && line_range.is_none() && original_line_range.is_none() {
            return Some(Self {
                path: comment.path.clone(),
                side: None,
                line_range: None,
                original_line_range: None,
                subject,
                commit_id: non_empty(&comment.commit_id),
                original_commit_id: non_empty(&comment.original_commit_id),
            });
        }

        Some(Self {
            path: comment.path.clone(),
            side,
            line_range,
            original_line_range,
            subject,
            commit_id: non_empty(&comment.commit_id),
            original_commit_id: non_empty(&comment.original_commit_id),
        })
    }

    pub fn from_thread(thread: &PullRequestReviewThread) -> Self {
        let side = thread.side.or(thread.start_side);
        let line_range = review_line_range(thread.start_line, thread.line);
        let original_line_range =
            review_line_range(thread.original_start_line, thread.original_line);
        Self {
            path: thread.path.clone(),
            side,
            line_range,
            original_line_range,
            subject: ReviewSubject::from_subject_type(Some(&thread.subject_type)),
            commit_id: None,
            original_commit_id: None,
        }
    }

    pub fn is_outdated(&self) -> bool {
        self.line_range.is_none() && self.original_line_range.is_some()
    }

    pub fn active_line_range(&self) -> Option<ReviewLineRange> {
        self.line_range.or(self.original_line_range)
    }

    pub fn from_source_target(path: impl Into<String>, target: &SourceTarget) -> Self {
        Self::inline(
            path,
            target.side.into(),
            ReviewLineRange::from_source_lines(&target.lines),
        )
    }

    pub fn matches_paths(&self, paths: &CompareFilePaths) -> bool {
        paths.old_path().as_deref() == Some(self.path.as_str())
            || paths.new_path().as_deref() == Some(self.path.as_str())
    }

    pub fn source_side(&self, paths: &CompareFilePaths) -> Option<Side> {
        let matches_old = paths.old_path().as_deref() == Some(self.path.as_str());
        let matches_new = paths.new_path().as_deref() == Some(self.path.as_str());
        match self.side {
            Some(ReviewSide::Old) if matches_old || matches_new => Some(Side::Old),
            Some(ReviewSide::New) if matches_old || matches_new => Some(Side::New),
            None if matches_new => Some(Side::New),
            None if matches_old => Some(Side::Old),
            _ => None,
        }
    }

    pub fn lands_in(&self, document: &DiffDocument, file: u32, target: &SourceTarget) -> bool {
        let last = target.lines.end.max(target.lines.start.saturating_add(1)) - 1;
        document
            .store_line(file, target.side, target.lines.start)
            .is_some()
            && document.store_line(file, target.side, last).is_some()
    }

    pub fn hunk_in(
        &self,
        document: &DiffDocument,
        file: u32,
        target: &SourceTarget,
    ) -> Option<u32> {
        document.hunk_at(file, target.side, target.lines.start)
    }

    pub fn commit_matches(&self, head: &str) -> bool {
        let Some(head) = ObjectId::parse(head) else {
            return true;
        };
        self.commit_id
            .as_deref()
            .and_then(ObjectId::parse)
            .is_none_or(|commit| commit.matches(&head))
    }

    pub fn to_source_target(
        &self,
        file: FileKey,
        revision: FileRevision,
        paths: &CompareFilePaths,
    ) -> Option<SourceTarget> {
        let side = self.source_side(paths)?;
        let lines = self.active_line_range()?.source_lines();
        Some(SourceTarget {
            file,
            revision,
            side,
            lines,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub struct ReviewCommentId(pub String);

impl ReviewCommentId {
    pub fn new(forge: ReviewForge, id: i64) -> Self {
        Self(format!("{}:{id}", forge.key()))
    }

    pub fn remote(forge: ReviewForge, id: impl Into<String>) -> Self {
        Self(format!("{}-node:{}", forge.key(), id.into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub struct ReviewThreadId(pub String);

impl ReviewThreadId {
    /// A thread grouped from flat comments under its root comment.
    pub fn root(forge: ReviewForge, id: i64) -> Self {
        Self(format!("{}-thread:{id}", forge.key()))
    }

    /// A thread the forge tracks under its own id.
    pub fn remote(forge: ReviewForge, id: impl Into<String>) -> Self {
        Self(format!("{}-thread-node:{}", forge.key(), id.into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewReactionGroup {
    pub content: String,
    pub count: u32,
    pub viewer_has_reacted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewComment {
    pub id: ReviewCommentId,
    pub backend_id: Option<i64>,
    /// The forge's id for edits and deletes, when it can be changed. Saved
    /// under its original name so older builds still read sessions.
    #[serde(rename = "backend_node_id")]
    pub remote_id: Option<String>,
    pub thread_id: ReviewThreadId,
    pub in_reply_to: Option<ReviewCommentId>,
    #[serde(rename = "in_reply_to_node_id")]
    pub in_reply_to_remote_id: Option<String>,
    pub author_login: Option<String>,
    #[serde(default)]
    pub author_avatar_url: Option<String>,
    pub body: String,
    pub anchor: Option<ReviewAnchor>,
    pub html_url: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub outdated: bool,
    pub state: Option<String>,
    pub viewer_can_update: bool,
    pub viewer_can_delete: bool,
    #[serde(default)]
    pub reactions: Vec<ReviewReactionGroup>,
}

impl ReviewComment {
    pub fn from_comment(forge: ReviewForge, comment: &PullRequestReviewComment) -> Self {
        let root_id = comment.in_reply_to_id.unwrap_or(comment.id);
        let anchor = ReviewAnchor::from_comment(comment);
        let outdated = anchor.as_ref().is_some_and(ReviewAnchor::is_outdated);
        Self {
            id: ReviewCommentId::new(forge, comment.id),
            backend_id: Some(comment.id),
            remote_id: None,
            thread_id: ReviewThreadId::root(forge, root_id),
            in_reply_to: comment
                .in_reply_to_id
                .map(|id| ReviewCommentId::new(forge, id)),
            in_reply_to_remote_id: None,
            author_login: comment
                .user
                .as_ref()
                .and_then(|user| non_empty(&user.login)),
            author_avatar_url: comment
                .user
                .as_ref()
                .and_then(|user| non_empty(&user.avatar_url)),
            body: comment.body.clone(),
            anchor,
            html_url: non_empty(&comment.html_url),
            created_at: non_empty(&comment.created_at),
            updated_at: non_empty(&comment.updated_at),
            outdated,
            state: None,
            viewer_can_update: false,
            viewer_can_delete: false,
            reactions: Vec::new(),
        }
    }

    pub fn from_thread_comment(
        forge: ReviewForge,
        thread: &PullRequestReviewThread,
        comment: &PullRequestReviewThreadComment,
    ) -> Self {
        let thread_id = ReviewThreadId::remote(forge, thread.id.clone());
        let id = comment
            .numeric_id
            .map(|id| ReviewCommentId::new(forge, id))
            .unwrap_or_else(|| ReviewCommentId::remote(forge, comment.id.clone()));
        let in_reply_to = comment
            .reply_to_numeric_id
            .map(|id| ReviewCommentId::new(forge, id))
            .or_else(|| {
                comment
                    .reply_to_id
                    .clone()
                    .map(|id| ReviewCommentId::remote(forge, id))
            });
        let anchor = Some(ReviewAnchor::from_thread(thread));
        Self {
            id,
            backend_id: comment.numeric_id,
            remote_id: Some(comment.id.clone()),
            thread_id,
            in_reply_to,
            in_reply_to_remote_id: comment.reply_to_id.clone(),
            author_login: non_empty(&comment.author_login),
            author_avatar_url: non_empty(&comment.author_avatar_url),
            body: comment.body.clone(),
            anchor,
            html_url: non_empty(&comment.url),
            created_at: non_empty(&comment.created_at),
            updated_at: non_empty(&comment.updated_at),
            outdated: comment.outdated,
            state: non_empty(&comment.state),
            viewer_can_update: comment.viewer_can_update,
            viewer_can_delete: comment.viewer_can_delete,
            reactions: comment
                .reactions
                .iter()
                .map(|reaction| ReviewReactionGroup {
                    content: reaction.content.clone(),
                    count: reaction.count,
                    viewer_has_reacted: reaction.viewer_has_reacted,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ReviewResolution {
    Unknown,
    Unresolved,
    Resolved,
}

impl Default for ReviewResolution {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewThreadStatus {
    pub resolution: ReviewResolution,
    pub outdated: bool,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewThreadPermissions {
    pub can_reply: bool,
    pub can_resolve: bool,
    pub can_unresolve: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewThread {
    pub id: ReviewThreadId,
    /// The forge's id for replies and resolution, when it tracks threads.
    #[serde(rename = "backend_node_id")]
    pub remote_id: Option<String>,
    pub anchor: Option<ReviewAnchor>,
    pub comments: Vec<ReviewComment>,
    pub status: ReviewThreadStatus,
    pub permissions: ReviewThreadPermissions,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum ReviewSessionStatus {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ReviewDraftId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum ReviewDraftKind {
    InlineComment,
    Reply { thread_id: ReviewThreadId },
    General,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum ReviewDraftState {
    Pending,
    Submitting,
    Submitted {
        external_comment_id: Option<ReviewCommentId>,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewDraft {
    pub id: ReviewDraftId,
    pub kind: ReviewDraftKind,
    pub anchor: Option<ReviewAnchor>,
    pub body: String,
    pub state: ReviewDraftState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ReviewDecision {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewSessionMetrics {
    pub unresolved_threads: usize,
    pub resolved_threads: usize,
    pub outdated_threads: usize,
    pub pending_drafts: usize,
    pub failed_drafts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewSession {
    pub target: ReviewTarget,
    pub pull_request: PullRequestInfo,
    pub status: ReviewSessionStatus,
    pub status_message: Option<String>,
    pub metadata: Option<PullRequestReviewMetadata>,
    pub reviews: Vec<PullRequestReviewSummary>,
    pub threads: Vec<ReviewThread>,
    pub drafts: BTreeMap<ReviewDraftId, ReviewDraft>,
    #[serde(default)]
    next_draft_id: u64,
}

impl ReviewSession {
    pub fn new(target: ReviewTarget, pull_request: PullRequestInfo) -> Self {
        Self {
            target,
            pull_request,
            status: ReviewSessionStatus::Idle,
            status_message: None,
            metadata: None,
            reviews: Vec::new(),
            threads: Vec::new(),
            drafts: BTreeMap::new(),
            next_draft_id: 1,
        }
    }

    pub fn key(&self) -> ReviewSessionKey {
        ReviewSessionKey::new(self.target.clone(), self.pull_request.head_sha.clone())
    }

    pub fn from_review_comments(
        target: ReviewTarget,
        pull_request: PullRequestInfo,
        comments: Vec<PullRequestReviewComment>,
    ) -> Self {
        let mut session = Self::new(target, pull_request);
        session.status = ReviewSessionStatus::Ready;
        session.replace_comments(comments);
        session
    }

    pub fn replace_comments(&mut self, comments: Vec<PullRequestReviewComment>) {
        self.threads = build_threads(self.target.forge, comments);
        self.status = ReviewSessionStatus::Ready;
        self.status_message = None;
    }

    pub fn apply_review_data(&mut self, data: PullRequestReviewData) {
        let forge = self.target.forge;
        self.metadata = Some(data.metadata);
        self.reviews = data.reviews;
        self.threads = data
            .threads
            .iter()
            .map(|thread| ReviewThread::from_thread(forge, thread))
            .collect();
        self.status = ReviewSessionStatus::Ready;
        self.status_message = None;
    }

    pub fn merge_persisted_state(&mut self, persisted: ReviewSession) {
        if self.threads.is_empty() {
            self.metadata = persisted.metadata;
            self.reviews = persisted.reviews;
            self.threads = persisted.threads;
        }
        self.drafts = persisted.drafts;
        self.next_draft_id = self.next_draft_id.max(persisted.next_draft_id);
    }

    pub fn mark_thread_resolution(
        &mut self,
        thread_id: &ReviewThreadId,
        resolution: ReviewResolution,
    ) -> bool {
        let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| &thread.id == thread_id)
        else {
            return false;
        };
        thread.status.resolution = resolution;
        match resolution {
            ReviewResolution::Resolved => thread.permissions.can_unresolve = true,
            ReviewResolution::Unresolved => thread.permissions.can_resolve = true,
            ReviewResolution::Unknown => {}
        }
        true
    }

    pub fn thread_remote_id(&self, thread_id: &ReviewThreadId) -> Option<String> {
        self.threads
            .iter()
            .find(|thread| &thread.id == thread_id)
            .and_then(|thread| thread.remote_id.clone())
    }

    pub fn create_inline_draft(
        &mut self,
        anchor: ReviewAnchor,
        body: impl Into<String>,
    ) -> ReviewDraftId {
        self.insert_draft(ReviewDraftKind::InlineComment, Some(anchor), body)
    }

    pub fn create_reply_draft(
        &mut self,
        thread_id: ReviewThreadId,
        body: impl Into<String>,
    ) -> ReviewDraftId {
        self.insert_draft(ReviewDraftKind::Reply { thread_id }, None, body)
    }

    pub fn create_general_draft(&mut self, body: impl Into<String>) -> ReviewDraftId {
        self.insert_draft(ReviewDraftKind::General, None, body)
    }

    pub fn remove_draft(&mut self, id: ReviewDraftId) -> Option<ReviewDraft> {
        self.drafts.remove(&id)
    }

    pub fn update_draft_body(&mut self, id: ReviewDraftId, body: impl Into<String>) -> bool {
        let Some(draft) = self.drafts.get_mut(&id) else {
            return false;
        };
        draft.body = body.into();
        draft.state = ReviewDraftState::Pending;
        true
    }

    pub fn mark_draft_submitting(&mut self, id: ReviewDraftId) -> bool {
        let Some(draft) = self.drafts.get_mut(&id) else {
            return false;
        };
        draft.state = ReviewDraftState::Submitting;
        true
    }

    pub fn mark_draft_failed(&mut self, id: ReviewDraftId, message: impl Into<String>) -> bool {
        let Some(draft) = self.drafts.get_mut(&id) else {
            return false;
        };
        draft.state = ReviewDraftState::Failed(message.into());
        true
    }

    pub fn mark_draft_submitted(
        &mut self,
        id: ReviewDraftId,
        external_comment_id: Option<ReviewCommentId>,
    ) -> bool {
        let Some(draft) = self.drafts.get_mut(&id) else {
            return false;
        };
        draft.state = ReviewDraftState::Submitted {
            external_comment_id,
        };
        true
    }

    pub fn pending_drafts(&self) -> impl Iterator<Item = &ReviewDraft> {
        self.drafts
            .values()
            .filter(|draft| matches!(draft.state, ReviewDraftState::Pending))
    }

    pub fn metrics(&self) -> ReviewSessionMetrics {
        let mut metrics = ReviewSessionMetrics::default();
        for thread in &self.threads {
            match thread.status.resolution {
                ReviewResolution::Resolved => metrics.resolved_threads += 1,
                ReviewResolution::Unknown | ReviewResolution::Unresolved => {
                    metrics.unresolved_threads += 1
                }
            }
            if thread.status.outdated {
                metrics.outdated_threads += 1;
            }
        }
        for draft in self.drafts.values() {
            match draft.state {
                ReviewDraftState::Pending => metrics.pending_drafts += 1,
                ReviewDraftState::Failed(_) => metrics.failed_drafts += 1,
                ReviewDraftState::Submitting | ReviewDraftState::Submitted { .. } => {}
            }
        }
        metrics
    }

    pub fn draft_count(&self) -> usize {
        self.drafts.len()
    }

    pub fn unresolved_thread_count(&self) -> usize {
        self.threads
            .iter()
            .filter(|thread| thread.status.resolution != ReviewResolution::Resolved)
            .count()
    }

    pub fn threads_for_path<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a ReviewThread> {
        self.threads
            .iter()
            .filter(move |thread| thread.path() == Some(path))
    }

    fn insert_draft(
        &mut self,
        kind: ReviewDraftKind,
        anchor: Option<ReviewAnchor>,
        body: impl Into<String>,
    ) -> ReviewDraftId {
        let id = ReviewDraftId(self.next_draft_id);
        self.next_draft_id = self.next_draft_id.saturating_add(1);
        self.drafts.insert(
            id,
            ReviewDraft {
                id,
                kind,
                anchor,
                body: body.into(),
                state: ReviewDraftState::Pending,
            },
        );
        id
    }
}

fn build_threads(forge: ReviewForge, comments: Vec<PullRequestReviewComment>) -> Vec<ReviewThread> {
    let mut grouped: BTreeMap<i64, Vec<PullRequestReviewComment>> = BTreeMap::new();
    for comment in comments {
        grouped
            .entry(comment.in_reply_to_id.unwrap_or(comment.id))
            .or_default()
            .push(comment);
    }

    grouped
        .into_iter()
        .map(|(root_id, mut comments)| {
            comments.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.updated_at.cmp(&b.updated_at))
                    .then_with(|| a.id.cmp(&b.id))
            });
            let thread_id = ReviewThreadId::root(forge, root_id);
            let normalized = comments
                .iter()
                .map(|comment| ReviewComment::from_comment(forge, comment))
                .collect::<Vec<_>>();
            let anchor = normalized.iter().find_map(|comment| comment.anchor.clone());
            let outdated = normalized.iter().any(|comment| comment.outdated)
                || anchor.as_ref().is_some_and(ReviewAnchor::is_outdated);
            ReviewThread {
                id: thread_id,
                remote_id: None,
                anchor,
                comments: normalized,
                status: ReviewThreadStatus {
                    resolution: ReviewResolution::Unknown,
                    outdated,
                    collapsed: false,
                },
                permissions: ReviewThreadPermissions::default(),
            }
        })
        .collect()
}

impl ReviewThread {
    pub fn from_thread(forge: ReviewForge, thread: &PullRequestReviewThread) -> Self {
        let id = ReviewThreadId::remote(forge, thread.id.clone());
        let anchor = Some(ReviewAnchor::from_thread(thread));
        let resolution = if thread.is_resolved {
            ReviewResolution::Resolved
        } else {
            ReviewResolution::Unresolved
        };
        let comments = thread
            .comments
            .iter()
            .map(|comment| ReviewComment::from_thread_comment(forge, thread, comment))
            .collect();
        Self {
            id,
            remote_id: Some(thread.id.clone()),
            anchor,
            comments,
            status: ReviewThreadStatus {
                resolution,
                outdated: thread.is_outdated,
                collapsed: thread.is_collapsed,
            },
            permissions: ReviewThreadPermissions {
                can_reply: thread.viewer_can_reply,
                can_resolve: thread.viewer_can_resolve,
                can_unresolve: thread.viewer_can_unresolve,
            },
        }
    }

    pub fn path(&self) -> Option<&str> {
        self.anchor.as_ref().map(|anchor| anchor.path.as_str())
    }
}

fn review_line_range(start: Option<u32>, end: Option<u32>) -> Option<ReviewLineRange> {
    let end = end?;
    Some(
        start
            .map(|start| ReviewLineRange::from_inclusive(start, end))
            .unwrap_or_else(|| ReviewLineRange::new(end, 1)),
    )
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn sanitize_key_part(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::forge::model::PullRequestActor;

    fn github_comment(
        id: i64,
        in_reply_to_id: Option<i64>,
        body: &str,
    ) -> PullRequestReviewComment {
        PullRequestReviewComment {
            id,
            in_reply_to_id,
            path: "src/lib.rs".to_owned(),
            body: body.to_owned(),
            commit_id: "head".to_owned(),
            original_commit_id: "base".to_owned(),
            line: Some(12),
            original_line: Some(12),
            side: Some(ReviewSide::New),
            start_line: Some(10),
            original_start_line: Some(10),
            start_side: Some(ReviewSide::New),
            subject_type: Some("line".to_owned()),
            html_url: format!("https://github.test/comment/{id}"),
            created_at: format!("2026-01-01T00:00:0{id}Z"),
            updated_at: String::new(),
            user: Some(PullRequestActor {
                login: "reviewer".to_owned(),
                avatar_url: String::new(),
            }),
        }
    }

    fn pull_request_info() -> PullRequestInfo {
        PullRequestInfo {
            title: "Review me".to_owned(),
            state: "open".to_owned(),
            draft: false,
            merged: false,
            author_login: "author".to_owned(),
            number: 42,
            additions: 10,
            deletions: 2,
            changed_files: 1,
            base_branch: "main".to_owned(),
            head_branch: "feature".to_owned(),
            base_sha: "base".to_owned(),
            head_sha: "head".to_owned(),
            base_repo_url: String::new(),
            head_repo_url: String::new(),
        }
    }

    #[test]
    fn github_review_comments_are_grouped_into_threads() {
        let session = ReviewSession::from_review_comments(
            ReviewTarget::github("owner", "repo", 42),
            pull_request_info(),
            vec![
                github_comment(2, Some(1), "reply"),
                github_comment(1, None, "root"),
            ],
        );

        assert_eq!(session.threads.len(), 1);
        let thread = &session.threads[0];
        assert_eq!(thread.id, ReviewThreadId::root(ReviewForge::GitHub, 1));
        assert_eq!(thread.comments.len(), 2);
        assert_eq!(thread.comments[0].body, "root");
        assert_eq!(thread.comments[1].body, "reply");
        assert_eq!(thread.anchor.as_ref().unwrap().path, "src/lib.rs");
    }

    fn paths(old: &str, new: &str) -> CompareFilePaths {
        CompareFilePaths::from_paths(Some(old), Some(new))
    }

    #[test]
    fn github_anchor_maps_to_zero_based_source_target() {
        let anchor = ReviewAnchor::from_comment(&github_comment(1, None, "root")).unwrap();

        let target = anchor
            .to_source_target(
                FileKey(7),
                FileRevision(3),
                &paths("src/lib.rs", "src/lib.rs"),
            )
            .unwrap();

        assert_eq!(
            target,
            SourceTarget {
                file: FileKey(7),
                revision: FileRevision(3),
                side: Side::New,
                lines: 9..12,
            }
        );
    }

    #[test]
    fn sideless_anchor_takes_the_side_its_path_names() {
        let mut anchor =
            ReviewAnchor::inline("old.rs", ReviewSide::New, ReviewLineRange::new(5, 1));
        anchor.side = None;

        assert_eq!(
            anchor.source_side(&paths("old.rs", "new.rs")),
            Some(Side::Old)
        );
        anchor.path = "new.rs".to_owned();
        assert_eq!(
            anchor.source_side(&paths("old.rs", "new.rs")),
            Some(Side::New)
        );
        anchor.path = "other.rs".to_owned();
        assert_eq!(
            anchor.to_source_target(FileKey(1), FileRevision(1), &paths("old.rs", "new.rs")),
            None
        );
    }

    #[test]
    fn file_level_anchor_matches_paths_without_a_source_target() {
        let anchor = ReviewAnchor {
            path: "src/lib.rs".to_owned(),
            side: None,
            line_range: None,
            original_line_range: None,
            subject: ReviewSubject::File,
            commit_id: None,
            original_commit_id: None,
        };
        let paths = paths("src/lib.rs", "src/lib.rs");

        assert!(anchor.matches_paths(&paths));
        assert_eq!(
            anchor.to_source_target(FileKey(1), FileRevision(1), &paths),
            None
        );
    }

    #[test]
    fn outdated_anchor_targets_its_original_lines() {
        let mut comment = github_comment(1, None, "root");
        comment.line = None;
        comment.start_line = None;
        let anchor = ReviewAnchor::from_comment(&comment).unwrap();

        assert!(anchor.is_outdated());
        let target = anchor
            .to_source_target(
                FileKey(2),
                FileRevision(1),
                &paths("src/lib.rs", "src/lib.rs"),
            )
            .unwrap();
        assert_eq!(target.lines, 9..12);
    }

    #[test]
    fn source_target_round_trips_through_an_inline_anchor() {
        let target = SourceTarget {
            file: FileKey(4),
            revision: FileRevision(2),
            side: Side::Old,
            lines: 4..6,
        };

        let anchor = ReviewAnchor::from_source_target("src/lib.rs", &target);

        assert_eq!(anchor.side, Some(ReviewSide::Old));
        assert_eq!(anchor.line_range, Some(ReviewLineRange::new(5, 2)));
        assert_eq!(anchor.line_range.unwrap().end_inclusive(), 6);
        assert_eq!(
            anchor.to_source_target(
                FileKey(4),
                FileRevision(2),
                &paths("src/lib.rs", "src/lib.rs")
            ),
            Some(target)
        );
    }

    #[test]
    fn anchors_resolve_against_document_coordinates() {
        let document = diffy_diff::parse_unified(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,3 +10,4 @@\n a\n-b\n+c\n+d\n e\n",
        )
        .unwrap();
        let anchor =
            ReviewAnchor::inline("src/lib.rs", ReviewSide::New, ReviewLineRange::new(11, 2));
        let target = anchor
            .to_source_target(
                FileKey(0),
                FileRevision(0),
                &paths("src/lib.rs", "src/lib.rs"),
            )
            .unwrap();
        let outside = SourceTarget {
            lines: 40..41,
            ..target.clone()
        };

        assert!(anchor.lands_in(&document, 0, &target));
        assert_eq!(anchor.hunk_in(&document, 0, &target), Some(0));
        assert!(!anchor.lands_in(&document, 0, &outside));
        assert_eq!(anchor.hunk_in(&document, 0, &outside), None);
    }

    #[test]
    fn commit_ids_match_by_prefix() {
        let mut anchor = ReviewAnchor::inline("a.rs", ReviewSide::New, ReviewLineRange::new(1, 1));
        assert!(anchor.commit_matches("abc123"));
        anchor.commit_id = Some("ABC123def".to_owned());
        assert!(anchor.commit_matches("abc123"));
        assert!(!anchor.commit_matches("fff000"));
    }

    #[test]
    fn persisted_session_format_still_loads() {
        let json = r#"{
            "target": {"forge": "GitHub", "owner": "owner", "repo": "repo", "number": 42},
            "pull_request": {
                "title": "Review me", "state": "open", "author_login": "author", "number": 42,
                "additions": 1, "deletions": 0, "changed_files": 1,
                "base_branch": "main", "head_branch": "feature",
                "base_sha": "base", "head_sha": "head",
                "base_repo_url": "", "head_repo_url": ""
            },
            "status": "Ready",
            "status_message": null,
            "metadata": null,
            "reviews": [],
            "threads": [],
            "drafts": {
                "3": {
                    "id": 3,
                    "kind": "InlineComment",
                    "anchor": {
                        "path": "src/lib.rs",
                        "side": "New",
                        "line_range": {"start": 10, "len": 2},
                        "original_line_range": null,
                        "subject": "Line",
                        "commit_id": null,
                        "original_commit_id": null
                    },
                    "body": "Note",
                    "state": "Pending"
                }
            },
            "next_draft_id": 4
        }"#;

        let session: ReviewSession = serde_json::from_str(json).unwrap();

        let draft = &session.drafts[&ReviewDraftId(3)];
        assert_eq!(draft.body, "Note");
        let target = draft
            .anchor
            .as_ref()
            .unwrap()
            .to_source_target(
                FileKey(1),
                FileRevision(1),
                &paths("src/lib.rs", "src/lib.rs"),
            )
            .unwrap();
        assert_eq!(target.lines, 9..11);
        let mut resumed = session.clone();
        assert_eq!(resumed.create_general_draft("next"), ReviewDraftId(4));
    }

    #[test]
    fn github_sessions_keep_their_saved_keys_and_ids() {
        let key = ReviewSessionKey::new(ReviewTarget::github("o w", "repo", 7), "abc");
        assert_eq!(key.storage_key(), "github/o_w/repo/7/abc");
        assert_eq!(
            ReviewThreadId::remote(ReviewForge::GitHub, "T").0,
            "github-thread-node:T"
        );
        assert_eq!(
            ReviewThreadId::root(ReviewForge::GitHub, 3).0,
            "github-thread:3"
        );
        assert_eq!(ReviewCommentId::new(ReviewForge::GitHub, 4).0, "github:4");
        assert_eq!(
            ReviewCommentId::remote(ReviewForge::GitHub, "C").0,
            "github-node:C"
        );

        let thread: ReviewThread = serde_json::from_str(
            r#"{
                "id": "github-thread-node:T",
                "backend_node_id": "T",
                "anchor": null,
                "comments": [{
                    "id": "github:4", "backend_id": 4, "backend_node_id": "C",
                    "thread_id": "github-thread-node:T", "in_reply_to": null,
                    "in_reply_to_node_id": "P", "author_login": null, "body": "",
                    "anchor": null, "html_url": null, "created_at": null, "updated_at": null,
                    "outdated": false, "state": null,
                    "viewer_can_update": false, "viewer_can_delete": false
                }],
                "status": {"resolution": "Unresolved", "outdated": false, "collapsed": false},
                "permissions": {"can_reply": true, "can_resolve": true, "can_unresolve": false}
            }"#,
        )
        .unwrap();
        assert_eq!(thread.remote_id.as_deref(), Some("T"));
        assert_eq!(thread.comments[0].remote_id.as_deref(), Some("C"));
        assert_eq!(
            thread.comments[0].in_reply_to_remote_id.as_deref(),
            Some("P")
        );

        let summary: PullRequestReviewSummary = serde_json::from_str(
            r#"{"node_id": "R", "database_id": 9, "state": "APPROVED", "body": "",
                "author_login": "a", "submitted_at": null, "commit_oid": "",
                "viewer_did_author": false}"#,
        )
        .unwrap();
        assert_eq!((summary.id.as_str(), summary.numeric_id), ("R", Some(9)));
    }

    #[test]
    fn review_session_tracks_local_drafts() {
        let mut session = ReviewSession::new(
            ReviewTarget::github("owner", "repo", 42),
            pull_request_info(),
        );
        let first = session.create_general_draft("overall note");
        let second = session.create_inline_draft(
            ReviewAnchor::inline("src/lib.rs", ReviewSide::New, ReviewLineRange::new(12, 1)),
            "inline note",
        );

        assert_eq!(first, ReviewDraftId(1));
        assert_eq!(second, ReviewDraftId(2));
        assert_eq!(session.draft_count(), 2);
        assert_eq!(
            session.remove_draft(first).unwrap().kind,
            ReviewDraftKind::General
        );
    }
}
