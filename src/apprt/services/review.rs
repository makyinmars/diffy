use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::contracts::review::{
    AvatarImage, ForgeRequest, ForgeResponse, PullRequestComparison, Secret,
};
use crate::core::error::{DiffyError, Result};
use crate::core::forge::bitbucket::{self, BitbucketApi, Credentials};
use crate::core::forge::github::{
    CreatePullRequestReview, DeviceFlowState, GitHubApi, parse_pr_url, poll_for_token,
    start_device_flow,
};
use crate::core::forge::model::{PullRequestInfo, ReviewThreadResolution};
use crate::core::frecency::{FrecencyStore, recent_repo_paths};
use crate::core::http;
use crate::core::repo_discovery;
use crate::core::review::{ReviewForge, ReviewSession, ReviewSessionKey, ReviewTarget};
use crate::core::vcs::discovery;
use crate::core::vcs::git::{GitService, forge_remotes};
use crate::core::vcs::model::VcsKind;
use crate::platform::persistence::SettingsStore;
use crate::platform::review_store::ReviewStore;
use crate::platform::secrets;

const CLONE_CANDIDATES: usize = 400;

pub trait ForgeBackend: Send + Sync + 'static {
    fn execute(&self, request: ForgeRequest, emit: &mut dyn FnMut(ForgeResponse));
}

#[derive(Clone)]
pub struct ReviewServices {
    settings_store: SettingsStore,
    review_store: ReviewStore,
    forge: Arc<dyn ForgeBackend>,
}

impl ReviewServices {
    pub fn new(
        settings_store: SettingsStore,
        review_store: ReviewStore,
        forge: Arc<dyn ForgeBackend>,
    ) -> Self {
        Self {
            settings_store,
            review_store,
            forge,
        }
    }

    pub fn forge(&self) -> &Arc<dyn ForgeBackend> {
        &self.forge
    }

    pub fn start_device_flow(&self, client_id: &str) -> Result<DeviceFlowState> {
        if client_id.trim().is_empty() {
            return Err(DiffyError::General(
                "GitHub client id is empty. Set DIFFY_GITHUB_CLIENT_ID.".to_owned(),
            ));
        }
        start_device_flow(client_id)
    }

    pub fn poll_device_flow(
        &self,
        client_id: &str,
        device_code: &str,
        interval_seconds: u32,
    ) -> Result<Secret> {
        loop {
            match poll_for_token(client_id, device_code)? {
                Some(token) => return Ok(Secret::new(token)),
                None => thread::sleep(Duration::from_secs(u64::from(interval_seconds.max(5)))),
            }
        }
    }

    pub fn load_keyring_token(&self, forge: ReviewForge) -> Result<Option<Secret>> {
        Ok(secrets::load_forge_token(forge)?.map(Secret::new))
    }

    pub fn save_keyring_token(&self, forge: ReviewForge, token: &Secret) -> Result<()> {
        secrets::save_forge_token(forge, token.expose())
    }

    pub fn clear_keyring_token(&self, forge: ReviewForge) -> Result<()> {
        secrets::clear_forge_token(forge)
    }

    pub fn load_dev_token(&self, forge: ReviewForge) -> Result<Option<Secret>> {
        Ok(secrets::load_token_file(&self.dev_token_path(forge))?.map(Secret::new))
    }

    pub fn save_dev_token(&self, forge: ReviewForge, token: &Secret) -> Result<()> {
        secrets::save_token_file(&self.dev_token_path(forge), token.expose())
    }

    pub fn clear_dev_token(&self, forge: ReviewForge) -> Result<()> {
        secrets::clear_token_file(&self.dev_token_path(forge))
    }

    fn dev_token_path(&self, forge: ReviewForge) -> PathBuf {
        self.settings_store
            .dir()
            .join(format!("{}-token.dev", forge.key()))
    }

    pub fn load_session(
        &self,
        target: ReviewTarget,
        pull_request: PullRequestInfo,
    ) -> Result<ReviewSession> {
        Ok(self
            .review_store
            .load_session(&target, &pull_request.head_sha)?
            .unwrap_or_else(|| ReviewSession::new(target, pull_request)))
    }

    pub fn save_session(&self, session: &ReviewSession) -> Result<ReviewSessionKey> {
        let key = session.key();
        self.review_store.save_session(session)?;
        Ok(key)
    }

    pub fn resolve_clones(&self, current: Option<PathBuf>) -> Vec<(String, PathBuf)> {
        let dir = self.settings_store.dir();
        let recent = FrecencyStore::open(dir)
            .map(|store| recent_repo_paths(Some(&store), CLONE_CANDIDATES))
            .unwrap_or_default();
        let discovered = repo_discovery::load_cache(dir)
            .map(|scan| scan.known.into_iter().chain(scan.repos))
            .into_iter()
            .flatten()
            .map(|repo| repo.path);
        let mut seen = HashSet::new();
        let mut clones: Vec<(String, PathBuf)> = Vec::new();
        let candidates = current.into_iter().chain(recent).chain(discovered);
        for path in candidates
            .filter(|path| seen.insert(path.clone()))
            .take(CLONE_CANDIDATES)
        {
            let Ok((root, keys)) = forge_remotes(&path) else {
                continue;
            };
            for key in keys {
                if clones.iter().all(|(known, _)| *known != key) {
                    clones.push((key, root.clone()));
                }
            }
        }
        clones
    }

    pub fn validate_clone(&self, repo: &str, path: &Path) -> Result<PathBuf> {
        let (root, keys) = forge_remotes(path).map_err(|_| {
            DiffyError::General(format!("{} is not a Git repository.", path.display()))
        })?;
        if keys.iter().any(|key| key.eq_ignore_ascii_case(repo)) {
            Ok(root)
        } else {
            Err(DiffyError::General(format!(
                "No remote in {} points at {repo}.",
                root.display()
            )))
        }
    }

    pub fn fetch_avatar(&self, url: &str) -> Result<AvatarImage> {
        let bytes = http::block_on(async {
            let response = crate::core::http::client()
                .get(url)
                .header("User-Agent", "diffy/0.1")
                .send()
                .await
                .map_err(|error| DiffyError::network(format!("avatar fetch failed: {error}")))?;
            http::response_bytes(response, "avatar fetch").await
        })?;
        let img = image::load_from_memory(&bytes)
            .map_err(|error| DiffyError::Parse(format!("avatar decode failed: {error}")))?
            .to_rgba8();
        let (width, height) = img.dimensions();
        let mut rgba = img.into_raw();
        apply_circular_mask(&mut rgba, width, height);
        Ok(AvatarImage {
            rgba,
            width,
            height,
        })
    }
}

#[derive(Clone)]
pub struct LiveForge {
    review_store: ReviewStore,
}

impl LiveForge {
    pub fn new(review_store: ReviewStore) -> Self {
        Self { review_store }
    }

    /// Report cached refs first when an earlier session left them, then the
    /// freshly fetched comparison.
    fn load_and_report(
        &self,
        url: String,
        repo_path: &Path,
        target: Result<ReviewTarget>,
        fetch_info: impl FnOnce(&ReviewTarget) -> Result<PullRequestInfo>,
        emit: &mut dyn FnMut(ForgeResponse),
    ) -> ForgeResponse {
        let target = match target {
            Ok(target) => target,
            Err(error) => {
                return ForgeResponse::PullRequestLoadFailed {
                    url,
                    message: error.user_message(),
                };
            }
        };
        if let Ok(Some(comparison)) = self.cached_pull_request_comparison(&target, repo_path) {
            emit(ForgeResponse::PullRequestCached {
                url: url.clone(),
                comparison,
            });
        }
        match self.load_pull_request(&target, repo_path, fetch_info) {
            Ok(comparison) => ForgeResponse::PullRequestLoaded { url, comparison },
            Err(error) => ForgeResponse::PullRequestLoadFailed {
                url,
                message: failure_message(target.forge, &error, emit),
            },
        }
    }

    fn load_pull_request(
        &self,
        target: &ReviewTarget,
        repo_path: &Path,
        fetch_info: impl FnOnce(&ReviewTarget) -> Result<PullRequestInfo>,
    ) -> Result<PullRequestComparison> {
        let total_started = Instant::now();
        let forge = target.forge.key();
        let stage_started = Instant::now();
        let mut repo = discovery::open_repository(repo_path)?;
        tracing::info!(
            forge,
            owner = %target.owner,
            repo = %target.repo,
            number = target.number,
            elapsed_ms = stage_started.elapsed().as_millis(),
            "pull-request: opened repository"
        );
        if !repo.capabilities().forge_pull_requests {
            return Err(DiffyError::General(format!(
                "this repository backend does not support {} pull request comparisons",
                target.forge.name()
            )));
        }
        if repo.location().kind != VcsKind::GIT {
            repo = discovery::open_git_repository(repo_path)?;
        }
        let stage_started = Instant::now();
        let info = fetch_info(target)?;
        tracing::info!(
            forge,
            owner = %target.owner,
            repo = %target.repo,
            number = target.number,
            elapsed_ms = stage_started.elapsed().as_millis(),
            "pull-request: fetched metadata"
        );
        let stage_started = Instant::now();
        let (info, left_ref, right_ref) = repo.resolve_pull_request_comparison(target, info)?;
        tracing::info!(
            forge,
            owner = %target.owner,
            repo = %target.repo,
            number = target.number,
            changed_files = info.changed_files,
            elapsed_ms = stage_started.elapsed().as_millis(),
            total_elapsed_ms = total_started.elapsed().as_millis(),
            "pull-request: resolved comparison refs"
        );
        Ok(PullRequestComparison {
            info,
            left_ref,
            right_ref,
        })
    }

    fn cached_pull_request_comparison(
        &self,
        target: &ReviewTarget,
        repo_path: &Path,
    ) -> Result<Option<PullRequestComparison>> {
        let sessions = self.review_store.load_sessions_for_target(target)?;
        if sessions.is_empty() {
            return Ok(None);
        }

        let repo = discovery::open_repository(repo_path)?;
        if !repo.capabilities().forge_pull_requests {
            return Ok(None);
        }
        let Ok(git) = GitService::new_with_open(repo_path) else {
            return Ok(None);
        };
        for session in sessions.into_iter().rev() {
            let info = session.pull_request;
            if info.base_branch.is_empty() || info.head_branch.is_empty() {
                continue;
            }
            if let Ok((info, left_ref, right_ref)) =
                git.cached_pull_request_comparison(target, info)
            {
                tracing::info!(
                    forge = target.forge.key(),
                    owner = %target.owner,
                    repo = %target.repo,
                    number = target.number,
                    "pull-request: using cached comparison refs"
                );
                return Ok(Some(PullRequestComparison {
                    info,
                    left_ref,
                    right_ref,
                }));
            }
        }

        Ok(None)
    }
}

fn api(token: Option<Secret>) -> GitHubApi {
    GitHubApi::with_token(token.map(Secret::into_inner).unwrap_or_default())
}

fn public_api(token: Option<Secret>) -> GitHubApi {
    match token {
        Some(token) if !token.is_empty() => GitHubApi::with_token(token.into_inner()),
        _ => GitHubApi::new(),
    }
}

impl ForgeBackend for LiveForge {
    fn execute(&self, request: ForgeRequest, emit: &mut dyn FnMut(ForgeResponse)) {
        // Only a token that was sent can be refused; a missing one is not.
        let sent = request.token().is_some_and(|token| !token.is_empty());
        let mut emit = |response: ForgeResponse| {
            if sent || !matches!(response, ForgeResponse::TokenRejected { .. }) {
                emit(response);
            }
        };
        match request.forge() {
            ReviewForge::GitHub => self.execute_github(request, &mut emit),
            ReviewForge::Bitbucket => self.execute_bitbucket(request, &mut emit),
        }
    }
}

/// The message for a failed request. A refused token is also reported, so
/// the account can ask for a new sign-in.
fn failure_message(
    forge: ReviewForge,
    error: &DiffyError,
    emit: &mut dyn FnMut(ForgeResponse),
) -> String {
    if matches!(error, DiffyError::Auth { .. }) {
        emit(ForgeResponse::TokenRejected { forge });
    }
    error.user_message()
}

fn bitbucket_api(token: Option<Secret>) -> BitbucketApi {
    let credentials = token
        .filter(|token| !token.is_empty())
        .and_then(|token| {
            let credentials = Credentials::parse(token.expose());
            if credentials.is_none() {
                tracing::warn!("bitbucket: stored credentials are not a token or email:token");
            }
            credentials
        })
        .unwrap_or_else(Credentials::anonymous);
    BitbucketApi::new(credentials)
}

impl LiveForge {
    fn execute_github(&self, request: ForgeRequest, emit: &mut dyn FnMut(ForgeResponse)) {
        let response = match request {
            ForgeRequest::LoadPullRequest {
                url,
                repo_path,
                token,
            } => {
                let target = parse_pr_url(&url)
                    .map(|pr| ReviewTarget::github(pr.owner, pr.repo, pr.number))
                    .ok_or_else(|| {
                        DiffyError::Parse("not a valid GitHub pull request URL".to_owned())
                    });
                self.load_and_report(
                    url,
                    &repo_path,
                    target,
                    |target| {
                        api(token).fetch_pull_request(&target.owner, &target.repo, target.number)
                    },
                    emit,
                )
            }
            ForgeRequest::FetchUser { token, .. } => {
                if token.is_empty() {
                    ForgeResponse::UserFetchFailed {
                        message: "cannot fetch GitHub user without a token".to_owned(),
                    }
                } else {
                    match GitHubApi::with_token(token.into_inner()).fetch_current_user() {
                        Ok(user) => ForgeResponse::UserFetched(user),
                        Err(error) => ForgeResponse::UserFetchFailed {
                            message: failure_message(ReviewForge::GitHub, &error, emit),
                        },
                    }
                }
            }
            ForgeRequest::PeekPullRequest { pr, token } => {
                match public_api(token).fetch_pull_request(&pr.owner, &pr.repo, pr.number) {
                    Ok(info) => ForgeResponse::PullRequestPeeked { pr, info },
                    Err(error) => ForgeResponse::PullRequestPeekFailed {
                        pr,
                        message: failure_message(ReviewForge::GitHub, &error, emit),
                    },
                }
            }
            ForgeRequest::FetchReviewComments { pr, token } => match public_api(token)
                .fetch_pull_request_review_comments(&pr.owner, &pr.repo, pr.number)
            {
                Ok(comments) => ForgeResponse::ReviewCommentsLoaded { pr, comments },
                Err(error) => ForgeResponse::ReviewCommentsLoadFailed {
                    pr,
                    message: failure_message(ReviewForge::GitHub, &error, emit),
                },
            },
            ForgeRequest::FetchReviewData { pr, token } => {
                match api(token).fetch_pull_request_review_data(&pr.owner, &pr.repo, pr.number) {
                    Ok(data) => ForgeResponse::ReviewDataLoaded {
                        pr,
                        data: Box::new(data),
                    },
                    Err(error) => ForgeResponse::ReviewDataLoadFailed {
                        pr,
                        message: failure_message(ReviewForge::GitHub, &error, emit),
                    },
                }
            }
            ForgeRequest::SubmitSessionDrafts {
                session,
                decision,
                body,
                token,
            } => {
                let pr = session.target.clone();
                let draft_ids = session
                    .pending_drafts()
                    .map(|draft| draft.id)
                    .collect::<Vec<_>>();
                let result = CreatePullRequestReview::from_drafts(&session, decision, body)
                    .and_then(|review| {
                        api(token)
                            .create_pull_request_review(&pr.owner, &pr.repo, pr.number, &review)
                    });
                match result {
                    Ok(_) => ForgeResponse::DraftsSubmitted { pr, draft_ids },
                    Err(error) => ForgeResponse::DraftsSubmitFailed {
                        pr,
                        submitted: Vec::new(),
                        draft_ids,
                        message: failure_message(ReviewForge::GitHub, &error, emit),
                    },
                }
            }
            ForgeRequest::AddThreadReply {
                pr,
                thread_id,
                token,
                body,
            } => match api(token).add_pull_request_review_thread_reply(&thread_id, &body) {
                Ok(comment) => ForgeResponse::ThreadReplyAdded {
                    pr,
                    thread_id,
                    comment,
                },
                Err(error) => ForgeResponse::ThreadReplyAddFailed {
                    pr,
                    thread_id,
                    message: failure_message(ReviewForge::GitHub, &error, emit),
                },
            },
            ForgeRequest::UpdateComment {
                pr,
                comment_id,
                token,
                body,
            } => match api(token).update_pull_request_review_comment_graphql(&comment_id, &body) {
                Ok(comment) => ForgeResponse::CommentUpdated { pr, comment },
                Err(error) => ForgeResponse::CommentUpdateFailed {
                    pr,
                    comment_id,
                    message: failure_message(ReviewForge::GitHub, &error, emit),
                },
            },
            ForgeRequest::DeleteComment {
                pr,
                comment_id,
                token,
            } => match api(token).delete_pull_request_review_comment_graphql(&comment_id) {
                Ok(comment) => ForgeResponse::CommentDeleted {
                    pr,
                    comment_id,
                    comment,
                },
                Err(error) => ForgeResponse::CommentDeleteFailed {
                    pr,
                    comment_id,
                    message: failure_message(ReviewForge::GitHub, &error, emit),
                },
            },
            ForgeRequest::SetThreadResolution {
                pr,
                thread_id,
                token,
                resolved,
            } => match api(token).set_pull_request_review_thread_resolution(&thread_id, resolved) {
                Ok(resolution) => ForgeResponse::ThreadResolutionChanged { pr, resolution },
                Err(error) => ForgeResponse::ThreadResolutionChangeFailed {
                    pr,
                    thread_id,
                    message: failure_message(ReviewForge::GitHub, &error, emit),
                },
            },
            ForgeRequest::FetchPullRequestOverview { pr, token } => match public_api(token)
                .fetch_pull_request_overview(&pr.owner, &pr.repo, pr.number)
            {
                Ok(overview) => ForgeResponse::PullRequestOverviewLoaded {
                    pr,
                    overview: Box::new(overview),
                },
                Err(error) => ForgeResponse::PullRequestOverviewLoadFailed {
                    pr,
                    message: failure_message(ReviewForge::GitHub, &error, emit),
                },
            },
            ForgeRequest::FetchMyPullRequests {
                token,
                filter,
                cursor,
                ..
            } => {
                match GitHubApi::with_token(token.into_inner())
                    .fetch_my_pull_requests(filter, cursor.as_deref())
                {
                    Ok(pulls) => ForgeResponse::MyPullRequestsLoaded(pulls),
                    Err(error) => ForgeResponse::MyPullRequestsLoadFailed {
                        message: failure_message(ReviewForge::GitHub, &error, emit),
                    },
                }
            }
            ForgeRequest::AddPullRequestComment { pr, token, body } => {
                match api(token).add_pull_request_comment(&pr.owner, &pr.repo, pr.number, &body) {
                    Ok(()) => ForgeResponse::PullRequestCommentAdded { pr },
                    Err(error) => ForgeResponse::PullRequestCommentAddFailed {
                        pr,
                        body,
                        message: failure_message(ReviewForge::GitHub, &error, emit),
                    },
                }
            }
        };
        emit(response);
    }
}

impl LiveForge {
    fn execute_bitbucket(&self, request: ForgeRequest, emit: &mut dyn FnMut(ForgeResponse)) {
        let response = match request {
            ForgeRequest::LoadPullRequest {
                url,
                repo_path,
                token,
            } => {
                let target = bitbucket::parse_pr_url(&url).ok_or_else(|| {
                    DiffyError::Parse("not a valid Bitbucket pull request URL".to_owned())
                });
                self.load_and_report(
                    url,
                    &repo_path,
                    target,
                    |target| bitbucket_api(token).fetch_pull_request(target),
                    emit,
                )
            }
            ForgeRequest::PeekPullRequest { pr, token } => {
                match bitbucket_api(token).fetch_pull_request(&pr) {
                    Ok(info) => ForgeResponse::PullRequestPeeked { pr, info },
                    Err(error) => ForgeResponse::PullRequestPeekFailed {
                        pr,
                        message: failure_message(ReviewForge::Bitbucket, &error, emit),
                    },
                }
            }
            ForgeRequest::FetchUser { token, .. } if Credentials::parse(token.expose()).is_none() => {
                ForgeResponse::UserFetchFailed {
                    message: "Enter your Atlassian account email. Bitbucket usernames do not work with API tokens.".to_owned(),
                }
            }
            ForgeRequest::FetchUser { token, .. } => {
                match bitbucket_api(Some(token)).fetch_current_user() {
                    Ok(user) => ForgeResponse::UserFetched(user.user),
                    Err(error) => ForgeResponse::UserFetchFailed {
                        message: failure_message(ReviewForge::Bitbucket, &error, emit),
                    },
                }
            }
            ForgeRequest::SubmitSessionDrafts {
                session,
                decision,
                token,
                ..
            } => {
                let pr = session.target.clone();
                let submission = bitbucket_api(token).submit_drafts(&session, decision);
                match submission.failure {
                    None => ForgeResponse::DraftsSubmitted {
                        pr,
                        draft_ids: submission.posted,
                    },
                    Some((draft_ids, message)) => ForgeResponse::DraftsSubmitFailed {
                        pr,
                        submitted: submission.posted,
                        draft_ids,
                        message,
                    },
                }
            }
            ForgeRequest::AddThreadReply {
                pr,
                thread_id,
                token,
                body,
            } => match bitbucket_api(token).reply(&pr, &thread_id, &body) {
                Ok(comment) => ForgeResponse::ThreadReplyAdded {
                    pr,
                    thread_id,
                    comment,
                },
                Err(error) => ForgeResponse::ThreadReplyAddFailed {
                    pr,
                    thread_id,
                    message: failure_message(ReviewForge::Bitbucket, &error, emit),
                },
            },
            ForgeRequest::UpdateComment {
                pr,
                comment_id,
                token,
                body,
            } => match bitbucket_api(token).update_comment(&pr, &comment_id, &body) {
                Ok(comment) => ForgeResponse::CommentUpdated { pr, comment },
                Err(error) => ForgeResponse::CommentUpdateFailed {
                    pr,
                    comment_id,
                    message: failure_message(ReviewForge::Bitbucket, &error, emit),
                },
            },
            ForgeRequest::DeleteComment {
                pr,
                comment_id,
                token,
            } => match bitbucket_api(token).delete_comment(&pr, &comment_id) {
                Ok(()) => ForgeResponse::CommentDeleted {
                    pr,
                    comment_id,
                    comment: None,
                },
                Err(error) => ForgeResponse::CommentDeleteFailed {
                    pr,
                    comment_id,
                    message: failure_message(ReviewForge::Bitbucket, &error, emit),
                },
            },
            ForgeRequest::SetThreadResolution {
                pr,
                thread_id,
                token,
                resolved,
            } => match bitbucket_api(token).set_resolution(&pr, &thread_id, resolved) {
                Ok(()) => ForgeResponse::ThreadResolutionChanged {
                    pr,
                    resolution: ReviewThreadResolution {
                        thread_id,
                        is_resolved: resolved,
                    },
                },
                Err(error) => ForgeResponse::ThreadResolutionChangeFailed {
                    pr,
                    thread_id,
                    message: failure_message(ReviewForge::Bitbucket, &error, emit),
                },
            },
            ForgeRequest::AddPullRequestComment { pr, token, body } => {
                match bitbucket_api(token).add_general_comment(&pr, &body) {
                    Ok(()) => ForgeResponse::PullRequestCommentAdded { pr },
                    Err(error) => ForgeResponse::PullRequestCommentAddFailed {
                        pr,
                        body,
                        message: failure_message(ReviewForge::Bitbucket, &error, emit),
                    },
                }
            }
            ForgeRequest::FetchPullRequestOverview { pr, token } => {
                match bitbucket_api(token).fetch_overview(&pr) {
                    Ok(overview) => ForgeResponse::PullRequestOverviewLoaded {
                        pr,
                        overview: Box::new(overview),
                    },
                    Err(error) => ForgeResponse::PullRequestOverviewLoadFailed {
                        pr,
                        message: failure_message(ReviewForge::Bitbucket, &error, emit),
                    },
                }
            }
            ForgeRequest::FetchMyPullRequests { token, filter, cursor, .. } => {
                match bitbucket_api(Some(token)).fetch_my_pull_requests(filter, cursor.as_deref()) {
                    Ok(pulls) => ForgeResponse::MyPullRequestsLoaded(pulls),
                    Err(error) => ForgeResponse::MyPullRequestsLoadFailed {
                        message: failure_message(ReviewForge::Bitbucket, &error, emit),
                    },
                }
            }
            ForgeRequest::FetchReviewData { pr, token } => {
                match bitbucket_api(token).fetch_review_data(&pr) {
                    Ok(data) => ForgeResponse::ReviewDataLoaded {
                        pr,
                        data: Box::new(data),
                    },
                    Err(error) => ForgeResponse::ReviewDataLoadFailed {
                        pr,
                        message: failure_message(ReviewForge::Bitbucket, &error, emit),
                    },
                }
            }
            other => other.fail("This isn't available for Bitbucket pull requests yet.".to_owned()),
        };
        emit(response);
    }
}

/// Apply an anti-aliased circular alpha mask to a square-ish RGBA buffer in-place.
/// Pixels outside the inscribed circle are transparent; a 1-pixel band at the edge
/// is feathered by coverage so the circle renders smoothly.
fn apply_circular_mask(rgba: &mut [u8], width: u32, height: u32) {
    let cx = (width as f32 - 1.0) / 2.0;
    let cy = (height as f32 - 1.0) / 2.0;
    let radius = width.min(height) as f32 / 2.0 - 0.5;
    for y in 0..height {
        for x in 0..width {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let coverage = (radius - dist + 0.5).clamp(0.0, 1.0);
            if coverage < 1.0 {
                let idx = ((y * width + x) * 4 + 3) as usize;
                if let Some(alpha) = rgba.get_mut(idx) {
                    *alpha = (f32::from(*alpha) * coverage).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use crate::core::vcs::git::{PrRefSide, pr_ref_path};

    #[test]
    fn cached_history_rebuilds_distinct_refs_from_recorded_commits_without_network() {
        for forge in [ReviewForge::GitHub, ReviewForge::Bitbucket] {
            let dir = tempfile::TempDir::new().unwrap();
            let repo = git2::Repository::init(dir.path()).unwrap();
            let signature = git2::Signature::now("Diffy", "diffy@example.com").unwrap();
            let tree = repo.treebuilder(None).unwrap().write().unwrap();
            let tree = repo.find_tree(tree).unwrap();
            let base = repo
                .commit(Some("HEAD"), &signature, &signature, "base", &tree, &[])
                .unwrap();
            let parent = repo.find_commit(base).unwrap();
            let head = repo
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    "head",
                    &tree,
                    &[&parent],
                )
                .unwrap();
            let target = ReviewTarget {
                forge,
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 7,
            };
            let info = PullRequestInfo {
                number: 7,
                state: "closed".into(),
                merged: true,
                base_branch: "main".into(),
                head_branch: "main".into(),
                base_sha: base.to_string(),
                head_sha: head.to_string(),
                ..PullRequestInfo::default()
            };
            let cache = tempfile::TempDir::new().unwrap();
            let store = ReviewStore::open(cache.path().join("cache")).unwrap();
            let session = ReviewSession::new(target.clone(), info.clone());
            store.save_session(&session).unwrap();
            repo.reference("refs/diffy/pr/7/main", head, true, "legacy collision")
                .unwrap();
            let live = LiveForge::new(store.clone());
            for stale_refs in [false, true] {
                let left = pr_ref_path(forge, 7, PrRefSide::Base, "main");
                let right = pr_ref_path(forge, 7, PrRefSide::Head, "main");
                if stale_refs {
                    repo.reference(&left, head, true, "wrong base").unwrap();
                    repo.reference(&right, base, true, "wrong head").unwrap();
                }
                let comparison = live
                    .cached_pull_request_comparison(&target, dir.path())
                    .unwrap()
                    .unwrap();
                assert_ne!(comparison.left_ref, comparison.right_ref);
                assert_eq!(comparison.info, info);
                assert_eq!(repo.find_reference(&left).unwrap().target(), Some(base));
                assert_eq!(repo.find_reference(&right).unwrap().target(), Some(head));
            }
            store.remove_session(&session.key()).unwrap();
            let mut missing = info;
            missing.head_sha = "f".repeat(40);
            store
                .save_session(&ReviewSession::new(target.clone(), missing))
                .unwrap();
            assert!(
                live.cached_pull_request_comparison(&target, dir.path())
                    .unwrap()
                    .is_none()
            );
        }
    }
}
