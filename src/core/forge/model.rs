//! Forge-neutral pull request data. Each forge client converts its API
//! responses into these types; nothing outside `core::forge` sees a forge's
//! wire format.
//!
//! State strings (check states, review states, mergeability, review
//! decisions) use GitHub's uppercase vocabulary (`SUCCESS`, `APPROVED`,
//! `MERGEABLE`, ...). Other forges map their values onto it.

use serde::{Deserialize, Serialize};

use crate::core::forge::clone_key;
use crate::core::review::{ReviewForge, ReviewSide};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ForgeUser {
    pub login: String,
    pub name: String,
    pub avatar_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestInfo {
    pub title: String,
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged: bool,
    pub author_login: String,
    pub number: i32,
    pub additions: i32,
    pub deletions: i32,
    pub changed_files: i32,
    pub base_branch: String,
    pub head_branch: String,
    pub base_sha: String,
    pub head_sha: String,
    pub base_repo_url: String,
    pub head_repo_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullRequestActor {
    pub login: String,
    pub avatar_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReactionGroup {
    pub content: String,
    pub count: u32,
    pub viewer_has_reacted: bool,
}

/// A review comment from a flat comment listing, threaded by `in_reply_to_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestReviewComment {
    pub id: i64,
    pub in_reply_to_id: Option<i64>,
    pub path: String,
    pub body: String,
    pub commit_id: String,
    pub original_commit_id: String,
    pub line: Option<u32>,
    pub original_line: Option<u32>,
    pub side: Option<ReviewSide>,
    pub start_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub start_side: Option<ReviewSide>,
    pub subject_type: Option<String>,
    pub html_url: String,
    pub created_at: String,
    pub updated_at: String,
    pub user: Option<PullRequestActor>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestReviewMetadata {
    // Saved sessions keep GitHub's original field names so older builds
    // still read them.
    #[serde(rename = "node_id")]
    pub id: String,
    pub url: String,
    pub review_decision: Option<String>,
    pub mergeable: String,
    pub merge_state_status: String,
    pub is_draft: bool,
    pub is_read_by_viewer: Option<bool>,
    pub viewer_latest_review_state: Option<String>,
    pub latest_head_oid: String,
    pub commit_count: i32,
    pub labels: Vec<PullRequestLabel>,
    pub review_requests: Vec<PullRequestReviewRequest>,
    pub checks: PullRequestCheckSummary,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestLabel {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestReviewRequest {
    pub reviewer_type: String,
    pub login: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestCheckSummary {
    pub state: Option<String>,
    pub total_count: i32,
    pub success_count: i32,
    pub failure_count: i32,
    pub pending_count: i32,
    pub contexts: Vec<PullRequestCheckContext>,
}

impl PullRequestCheckSummary {
    pub(crate) fn add_state(&mut self, state: &str, count: i32) {
        match state {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => self.success_count += count,
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" => self.failure_count += count,
            _ => self.pending_count += count,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestCheckContext {
    pub name: String,
    pub state: String,
    pub details_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestReviewSummary {
    #[serde(rename = "node_id")]
    pub id: String,
    #[serde(rename = "database_id")]
    pub numeric_id: Option<i64>,
    pub state: String,
    pub body: String,
    pub author_login: String,
    pub submitted_at: Option<String>,
    pub commit_oid: String,
    pub viewer_did_author: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestReviewThreadComment {
    /// The forge's id for edits and deletes.
    pub id: String,
    pub numeric_id: Option<i64>,
    pub reply_to_id: Option<String>,
    pub reply_to_numeric_id: Option<i64>,
    pub author_login: String,
    pub author_avatar_url: String,
    pub body: String,
    pub path: String,
    pub line: Option<u32>,
    pub original_line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub subject_type: String,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
    pub outdated: bool,
    pub state: String,
    pub viewer_can_update: bool,
    pub viewer_can_delete: bool,
    pub reactions: Vec<ReactionGroup>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestReviewThread {
    /// The forge's id for replies and resolution.
    pub id: String,
    pub path: String,
    pub line: Option<u32>,
    pub original_line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub side: Option<ReviewSide>,
    pub start_side: Option<ReviewSide>,
    pub subject_type: String,
    pub is_collapsed: bool,
    pub is_outdated: bool,
    pub is_resolved: bool,
    pub viewer_can_reply: bool,
    pub viewer_can_resolve: bool,
    pub viewer_can_unresolve: bool,
    pub comments: Vec<PullRequestReviewThreadComment>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PullRequestReviewData {
    pub metadata: PullRequestReviewMetadata,
    pub reviews: Vec<PullRequestReviewSummary>,
    pub threads: Vec<PullRequestReviewThread>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewThreadResolution {
    pub thread_id: String,
    pub is_resolved: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullRequestReviewer {
    pub actor: PullRequestActor,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRequestTimelineKind {
    Comment,
    Review { state: String, comments: i32 },
    Commit { short_oid: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestTimelineItem {
    pub id: String,
    pub kind: PullRequestTimelineKind,
    pub author: PullRequestActor,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullRequestOverview {
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: String,
    pub is_draft: bool,
    pub author: PullRequestActor,
    pub created_at: String,
    pub updated_at: String,
    pub base_branch: String,
    pub head_branch: String,
    pub additions: i32,
    pub deletions: i32,
    pub changed_files: i32,
    pub commit_count: i32,
    pub mergeable: String,
    pub merge_state_status: String,
    pub review_decision: Option<String>,
    pub labels: Vec<PullRequestLabel>,
    pub reviewers: Vec<PullRequestReviewer>,
    pub team_reviewers: usize,
    pub checks: PullRequestCheckSummary,
    pub timeline: Vec<PullRequestTimelineItem>,
    pub timeline_total: usize,
    pub detailed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullRequestStats {
    pub mergeable: String,
    pub additions: i32,
    pub deletions: i32,
    pub changed_files: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MyPullRequest {
    pub forge: ReviewForge,
    pub id: String,
    pub number: i32,
    pub title: String,
    pub url: String,
    pub repository: String,
    pub is_draft: bool,
    pub updated_at: String,
    pub head_branch: String,
    pub base_branch: String,
    pub review_decision: Option<String>,
    pub reviewers: Vec<PullRequestReviewer>,
    pub requested_reviewers: usize,
    pub unresolved_threads: usize,
    pub excerpt: String,
    pub stats: Option<PullRequestStats>,
    pub checks: Option<PullRequestCheckSummary>,
}

impl MyPullRequest {
    /// How Diffy names this pull request's repository when asking for its
    /// local clone: GitHub's `owner/repo`, `bitbucket.org/ws/repo` otherwise.
    pub fn clone_label(&self) -> String {
        match self.forge {
            ReviewForge::GitHub => self.repository.clone(),
            ReviewForge::Bitbucket => format!("bitbucket.org/{}", self.repository),
        }
    }

    /// The key a local clone of this pull request's repository is saved
    /// under.
    pub fn repo_key(&self) -> String {
        clone_key(self.forge, &self.repository)
    }
}
