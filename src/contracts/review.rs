use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use quark_ui::document::DocumentEvent;

use crate::contracts::shared::{AccountEpoch, JobId, SourceTarget};
use crate::core::forge::github::DeviceFlowState;
use crate::core::forge::model::{
    ForgeUser, PullRequestFilter, PullRequestInfo, PullRequestOverview, PullRequestPage,
    PullRequestReviewComment, PullRequestReviewData, PullRequestReviewThreadComment,
    ReviewThreadResolution,
};
use crate::core::forge::parse_pull_request_url;
use crate::core::review::{
    ReviewDecision, ReviewDraftId, ReviewForge, ReviewSession, ReviewSessionKey, ReviewTarget,
    ReviewThreadId,
};

#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerFormat {
    Bold,
    Italic,
    Code,
    Link,
    BulletList,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CardKey {
    Thread(ReviewThreadId),
    Draft(ReviewDraftId),
    NewComment,
    Conversation,
    Overview(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReviewAction {
    /// Sign in to the open pull request's forge (GitHub when none is open).
    OpenAuthModal,
    OpenSignIn(ReviewForge),
    CloseOverlay,
    StartDeviceFlow,
    OpenDeviceFlowBrowser,
    /// Check the typed Bitbucket API token and keep it if it works.
    SignInBitbucket,
    OpenAccountMenu,
    SignOut(ReviewForge),
    LoadPullRequest {
        url: String,
    },
    PreviewPullRequest,
    OpenPullRequestInBrowser,
    OpenComposer,
    OpenComposerAt(SourceTarget),
    SubmitComment,
    CancelComment,
    FormatComment(ComposerFormat),
    SetComposerPreview(bool),
    ReplyToThread(ReviewThreadId),
    ReplyToFocused,
    ResolveFocused,
    ToggleFocused,
    EditComment {
        comment_id: String,
    },
    ToggleCommentMenu {
        comment_id: String,
    },
    DeleteComment {
        comment_id: String,
    },
    SubmitReview {
        decision: ReviewDecision,
    },
    DiscardDrafts,
    ToggleThread(ReviewThreadId),
    SetThreadResolved {
        id: ReviewThreadId,
        resolved: bool,
    },
    EditDraft(ReviewDraftId),
    DeleteDraft(ReviewDraftId),
    CardDocument {
        card: CardKey,
        event: DocumentEvent,
    },
    CopyCardSelection(CardKey),
    ScrollComposer(i32),
    OpenSubmitMenu,
    RequestAuthorAvatars(Vec<String>),
    PeekPullRequest {
        url: String,
    },
    ToggleFileComments(String),
    OpenOverview,
    CloseOverview,
    ToggleOverview,
    RefreshOverview,
    ToggleOverviewChecks,
    OpenConversationComposer,
    OpenLink(String),
    Pulls(PullsAction),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PullsAction {
    Open,
    Close,
    Refresh,
    SetState(PullRequestFilter),
    LoadMore,
    Move(i32),
    Select(String),
    Review,
    OpenOnGitHub,
    CopyLink,
    FocusFilter,
    EscapeFilter,
    ClearFilter,
    LocateClone,
    CloneChosen { repo: String, path: PathBuf },
}

impl From<PullsAction> for ReviewAction {
    fn from(action: PullsAction) -> Self {
        Self::Pulls(action)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ReviewScope {
    pub account: AccountEpoch,
    pub request: JobId,
}

/// A request to a pull request forge. Thread and comment ids are the forge's
/// own ids (`ReviewThread::remote_id`, `ReviewComment::remote_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeRequest {
    LoadPullRequest {
        url: String,
        repo_path: PathBuf,
        token: Option<Secret>,
    },
    FetchUser {
        forge: ReviewForge,
        token: Secret,
    },
    PeekPullRequest {
        pr: ReviewTarget,
        token: Option<Secret>,
    },
    FetchReviewComments {
        pr: ReviewTarget,
        token: Option<Secret>,
    },
    FetchReviewData {
        pr: ReviewTarget,
        token: Option<Secret>,
    },
    SubmitSessionDrafts {
        session: Box<ReviewSession>,
        decision: ReviewDecision,
        body: Option<String>,
        token: Option<Secret>,
    },
    AddThreadReply {
        pr: ReviewTarget,
        thread_id: String,
        token: Option<Secret>,
        body: String,
    },
    UpdateComment {
        pr: ReviewTarget,
        comment_id: String,
        token: Option<Secret>,
        body: String,
    },
    DeleteComment {
        pr: ReviewTarget,
        comment_id: String,
        token: Option<Secret>,
    },
    SetThreadResolution {
        pr: ReviewTarget,
        thread_id: String,
        token: Option<Secret>,
        resolved: bool,
    },
    FetchPullRequestOverview {
        pr: ReviewTarget,
        token: Option<Secret>,
    },
    AddPullRequestComment {
        pr: ReviewTarget,
        token: Option<Secret>,
        body: String,
    },
    FetchMyPullRequests {
        forge: ReviewForge,
        token: Secret,
        filter: PullRequestFilter,
        cursor: Option<String>,
    },
}

impl ForgeRequest {
    pub fn forge(&self) -> ReviewForge {
        match self {
            Self::LoadPullRequest { url, .. } => {
                parse_pull_request_url(url).map_or(ReviewForge::GitHub, |pr| pr.forge)
            }
            Self::FetchUser { forge, .. } | Self::FetchMyPullRequests { forge, .. } => *forge,
            Self::SubmitSessionDrafts { session, .. } => session.target.forge,
            Self::PeekPullRequest { pr, .. }
            | Self::FetchReviewComments { pr, .. }
            | Self::FetchReviewData { pr, .. }
            | Self::AddThreadReply { pr, .. }
            | Self::UpdateComment { pr, .. }
            | Self::DeleteComment { pr, .. }
            | Self::SetThreadResolution { pr, .. }
            | Self::FetchPullRequestOverview { pr, .. }
            | Self::AddPullRequestComment { pr, .. } => pr.forge,
        }
    }

    pub fn token(&self) -> Option<&Secret> {
        match self {
            Self::FetchUser { token, .. } | Self::FetchMyPullRequests { token, .. } => Some(token),
            Self::LoadPullRequest { token, .. }
            | Self::PeekPullRequest { token, .. }
            | Self::FetchReviewComments { token, .. }
            | Self::FetchReviewData { token, .. }
            | Self::SubmitSessionDrafts { token, .. }
            | Self::AddThreadReply { token, .. }
            | Self::UpdateComment { token, .. }
            | Self::DeleteComment { token, .. }
            | Self::SetThreadResolution { token, .. }
            | Self::FetchPullRequestOverview { token, .. }
            | Self::AddPullRequestComment { token, .. } => token.as_ref(),
        }
    }

    /// The failure response this request reports when it cannot run.
    pub fn fail(self, message: String) -> ForgeResponse {
        match self {
            Self::LoadPullRequest { url, .. } => {
                ForgeResponse::PullRequestLoadFailed { url, message }
            }
            Self::FetchUser { .. } => ForgeResponse::UserFetchFailed { message },
            Self::PeekPullRequest { pr, .. } => {
                ForgeResponse::PullRequestPeekFailed { pr, message }
            }
            Self::FetchReviewComments { pr, .. } => {
                ForgeResponse::ReviewCommentsLoadFailed { pr, message }
            }
            Self::FetchReviewData { pr, .. } => ForgeResponse::ReviewDataLoadFailed { pr, message },
            Self::SubmitSessionDrafts { session, .. } => ForgeResponse::DraftsSubmitFailed {
                submitted: Vec::new(),
                draft_ids: session.pending_drafts().map(|draft| draft.id).collect(),
                pr: session.target,
                message,
            },
            Self::AddThreadReply { pr, thread_id, .. } => ForgeResponse::ThreadReplyAddFailed {
                pr,
                thread_id,
                message,
            },
            Self::UpdateComment { pr, comment_id, .. } => ForgeResponse::CommentUpdateFailed {
                pr,
                comment_id,
                message,
            },
            Self::DeleteComment { pr, comment_id, .. } => ForgeResponse::CommentDeleteFailed {
                pr,
                comment_id,
                message,
            },
            Self::SetThreadResolution { pr, thread_id, .. } => {
                ForgeResponse::ThreadResolutionChangeFailed {
                    pr,
                    thread_id,
                    message,
                }
            }
            Self::FetchPullRequestOverview { pr, .. } => {
                ForgeResponse::PullRequestOverviewLoadFailed { pr, message }
            }
            Self::AddPullRequestComment { pr, body, .. } => {
                ForgeResponse::PullRequestCommentAddFailed { pr, body, message }
            }
            Self::FetchMyPullRequests { .. } => ForgeResponse::MyPullRequestsLoadFailed { message },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestComparison {
    pub info: PullRequestInfo,
    pub left_ref: String,
    pub right_ref: String,
}

#[derive(Debug, Clone)]
pub enum ForgeResponse {
    PullRequestCached {
        url: String,
        comparison: PullRequestComparison,
    },
    PullRequestLoaded {
        url: String,
        comparison: PullRequestComparison,
    },
    PullRequestLoadFailed {
        url: String,
        message: String,
    },
    UserFetched(ForgeUser),
    UserFetchFailed {
        message: String,
    },
    PullRequestPeeked {
        pr: ReviewTarget,
        info: PullRequestInfo,
    },
    PullRequestPeekFailed {
        pr: ReviewTarget,
        message: String,
    },
    ReviewCommentsLoaded {
        pr: ReviewTarget,
        comments: Vec<PullRequestReviewComment>,
    },
    ReviewCommentsLoadFailed {
        pr: ReviewTarget,
        message: String,
    },
    ReviewDataLoaded {
        pr: ReviewTarget,
        data: Box<PullRequestReviewData>,
    },
    ReviewDataLoadFailed {
        pr: ReviewTarget,
        message: String,
    },
    DraftsSubmitted {
        pr: ReviewTarget,
        draft_ids: Vec<ReviewDraftId>,
    },
    /// `submitted` drafts went through before the failure; `draft_ids`
    /// did not. Bitbucket posts drafts one by one, so both can be non-empty.
    DraftsSubmitFailed {
        pr: ReviewTarget,
        submitted: Vec<ReviewDraftId>,
        draft_ids: Vec<ReviewDraftId>,
        message: String,
    },
    ThreadReplyAdded {
        pr: ReviewTarget,
        thread_id: String,
        comment: PullRequestReviewThreadComment,
    },
    ThreadReplyAddFailed {
        pr: ReviewTarget,
        thread_id: String,
        message: String,
    },
    CommentUpdated {
        pr: ReviewTarget,
        comment: PullRequestReviewThreadComment,
    },
    CommentUpdateFailed {
        pr: ReviewTarget,
        comment_id: String,
        message: String,
    },
    CommentDeleted {
        pr: ReviewTarget,
        comment_id: String,
        comment: Option<PullRequestReviewThreadComment>,
    },
    CommentDeleteFailed {
        pr: ReviewTarget,
        comment_id: String,
        message: String,
    },
    ThreadResolutionChanged {
        pr: ReviewTarget,
        resolution: ReviewThreadResolution,
    },
    ThreadResolutionChangeFailed {
        pr: ReviewTarget,
        thread_id: String,
        message: String,
    },
    PullRequestOverviewLoaded {
        pr: ReviewTarget,
        overview: Box<PullRequestOverview>,
    },
    PullRequestOverviewLoadFailed {
        pr: ReviewTarget,
        message: String,
    },
    PullRequestCommentAdded {
        pr: ReviewTarget,
    },
    PullRequestCommentAddFailed {
        pr: ReviewTarget,
        body: String,
        message: String,
    },
    MyPullRequestsLoaded(PullRequestPage),
    MyPullRequestsLoadFailed {
        message: String,
    },
    /// `forge` refused the request's credentials. Sent just before the
    /// request's own failure response, under the same scope.
    TokenRejected {
        forge: ReviewForge,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewEffect {
    Forge {
        scope: ReviewScope,
        request: Box<ForgeRequest>,
    },
    StartDeviceFlow {
        flow: JobId,
        client_id: String,
    },
    PollDeviceFlow {
        flow: JobId,
        client_id: String,
        device_code: String,
        interval_seconds: u32,
    },
    LoadToken(ReviewForge),
    SaveToken {
        forge: ReviewForge,
        token: Secret,
    },
    ClearToken(ReviewForge),
    LoadSession {
        scope: ReviewScope,
        target: ReviewTarget,
        pull_request: Box<PullRequestInfo>,
    },
    SaveSession {
        scope: ReviewScope,
        session: Box<ReviewSession>,
    },
    SaveSessionDraft {
        scope: ReviewScope,
        session: Box<ReviewSession>,
    },
    FetchAvatar {
        url: String,
    },
    ResolveClones {
        job: JobId,
        current: Option<PathBuf>,
    },
    ValidateClone {
        repo: String,
        path: PathBuf,
    },
}

#[derive(Debug)]
pub struct AvatarImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub enum ReviewEvent {
    Forge {
        scope: ReviewScope,
        response: Box<ForgeResponse>,
    },
    DeviceFlowStarted {
        flow: JobId,
        state: DeviceFlowState,
    },
    DeviceFlowStartFailed {
        flow: JobId,
        message: String,
    },
    DeviceFlowCompleted {
        flow: JobId,
        token: Secret,
    },
    DeviceFlowFailed {
        flow: JobId,
        message: String,
    },
    TokenLoaded {
        forge: ReviewForge,
        token: Option<Secret>,
    },
    TokenLoadFailed {
        forge: ReviewForge,
        message: String,
    },
    TokenSaveFailed {
        forge: ReviewForge,
        message: String,
    },
    SessionLoaded {
        scope: ReviewScope,
        target: ReviewTarget,
        session: Box<ReviewSession>,
    },
    SessionLoadFailed {
        scope: ReviewScope,
        target: ReviewTarget,
        message: String,
    },
    SessionSaved {
        scope: ReviewScope,
        key: ReviewSessionKey,
    },
    SessionSaveFailed {
        scope: ReviewScope,
        key: ReviewSessionKey,
        message: String,
    },
    AvatarFetched {
        url: String,
        image: Arc<AvatarImage>,
    },
    AvatarFetchFailed {
        url: String,
        message: String,
    },
    ClonesResolved {
        job: JobId,
        clones: Vec<(String, PathBuf)>,
    },
    CloneValidated {
        repo: String,
        result: Result<PathBuf, String>,
    },
}

impl ReviewEvent {
    pub fn account_scoped(&self) -> bool {
        match self {
            Self::Forge { response, .. } => matches!(
                **response,
                ForgeResponse::UserFetched(_)
                    | ForgeResponse::UserFetchFailed { .. }
                    | ForgeResponse::MyPullRequestsLoaded(_)
                    | ForgeResponse::MyPullRequestsLoadFailed { .. }
                    | ForgeResponse::TokenRejected { .. }
            ),
            Self::DeviceFlowStarted { .. }
            | Self::DeviceFlowStartFailed { .. }
            | Self::DeviceFlowCompleted { .. }
            | Self::DeviceFlowFailed { .. }
            | Self::TokenLoaded { .. }
            | Self::TokenLoadFailed { .. }
            | Self::TokenSaveFailed { .. }
            | Self::AvatarFetched { .. }
            | Self::AvatarFetchFailed { .. }
            | Self::ClonesResolved { .. }
            | Self::CloneValidated { .. } => true,
            Self::SessionLoaded { .. }
            | Self::SessionLoadFailed { .. }
            | Self::SessionSaved { .. }
            | Self::SessionSaveFailed { .. } => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewContext {
    pub account: AccountEpoch,
    pub target: ReviewTarget,
    pub pull_request: Arc<PullRequestInfo>,
    pub left_ref: String,
    pub right_ref: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::shell::{Action, AppEvent, Effect};

    #[test]
    fn routes_through_root_contracts() {
        assert_eq!(
            Action::from(ReviewAction::SignOut(ReviewForge::GitHub)),
            Action::Review(ReviewAction::SignOut(ReviewForge::GitHub))
        );
        assert_eq!(
            Effect::from(ReviewEffect::LoadToken(ReviewForge::GitHub)),
            Effect::Review(ReviewEffect::LoadToken(ReviewForge::GitHub))
        );
        assert!(matches!(
            AppEvent::from(ReviewEvent::TokenLoaded {
                forge: ReviewForge::GitHub,
                token: None
            }),
            AppEvent::Review(ReviewEvent::TokenLoaded { token: None, .. })
        ));
    }
}
