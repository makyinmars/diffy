use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::contracts::review::{
    AvatarImage, ForgeRequest, ForgeResponse, PullRequestComparison, Secret,
};
use crate::core::compare::{LayoutMode, RendererKind};
use crate::core::error::{DiffyError, Result};
use crate::core::forge::github::{
    CreatePullRequestReview, DeviceFlowState, GitHubApi, parse_pr_url, poll_for_token,
    start_device_flow,
};
use crate::core::forge::model::PullRequestInfo;
use crate::core::frecency::{FrecencyStore, recent_repo_paths};
use crate::core::http;
use crate::core::repo_discovery;
use crate::core::review::{ReviewForge, ReviewSession, ReviewSessionKey, ReviewTarget};
use crate::core::vcs::discovery;
use crate::core::vcs::git::{github_remotes, pr_ref_path};
use crate::core::vcs::model::{VcsCompareRequest, VcsCompareSpec, VcsKind};
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
            let Ok((root, keys)) = github_remotes(&path) else {
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
        let (root, keys) = github_remotes(path).map_err(|_| {
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

    fn load_pull_request(
        &self,
        url: &str,
        repo_path: &Path,
        token: Option<Secret>,
    ) -> Result<PullRequestComparison> {
        let total_started = Instant::now();
        let parsed = parse_pr_url(url)
            .ok_or_else(|| DiffyError::Parse("not a valid GitHub pull request URL".to_owned()))?;
        let token = token.map(Secret::into_inner).unwrap_or_default();

        let stage_started = Instant::now();
        let mut repo = discovery::open_repository(repo_path)?;
        tracing::info!(
            owner = %parsed.owner,
            repo = %parsed.repo,
            number = parsed.number,
            elapsed_ms = stage_started.elapsed().as_millis(),
            "github-pr: opened repository"
        );
        if !repo.capabilities().forge_pull_requests {
            return Err(DiffyError::General(
                "this repository backend does not support GitHub pull request comparisons"
                    .to_owned(),
            ));
        }
        if repo.location().kind != VcsKind::GIT {
            repo = discovery::open_git_repository(repo_path)?;
        }
        let stage_started = Instant::now();
        let (info, left_ref, right_ref) = repo.resolve_pull_request_comparison(url, &token)?;
        tracing::info!(
            owner = %parsed.owner,
            repo = %parsed.repo,
            number = parsed.number,
            changed_files = info.changed_files,
            elapsed_ms = stage_started.elapsed().as_millis(),
            total_elapsed_ms = total_started.elapsed().as_millis(),
            "github-pr: resolved comparison refs"
        );
        Ok(PullRequestComparison {
            info,
            left_ref,
            right_ref,
        })
    }

    fn cached_pull_request_comparison(
        &self,
        url: &str,
        repo_path: &Path,
    ) -> Result<Option<PullRequestComparison>> {
        let parsed = parse_pr_url(url)
            .ok_or_else(|| DiffyError::Parse("not a valid GitHub pull request URL".to_owned()))?;
        let target = ReviewTarget::github(parsed.owner, parsed.repo, parsed.number);
        let sessions = self.review_store.load_sessions_for_target(&target)?;
        if sessions.is_empty() {
            return Ok(None);
        }

        let mut repo = discovery::open_repository(repo_path)?;
        if !repo.capabilities().forge_pull_requests {
            return Ok(None);
        }
        if repo.location().kind != VcsKind::GIT {
            let Ok(git_repo) = discovery::open_git_repository(repo_path) else {
                return Ok(None);
            };
            repo = git_repo;
        }

        for session in sessions.into_iter().rev() {
            let info = session.pull_request;
            if info.base_branch.is_empty() || info.head_branch.is_empty() {
                continue;
            }
            let left_ref = pr_ref_path(info.number, &info.base_branch);
            let right_ref = pr_ref_path(info.number, &info.head_branch);
            let request = VcsCompareRequest {
                spec: VcsCompareSpec::MergeBaseRange {
                    base: left_ref.clone(),
                    head: right_ref.clone(),
                },
                layout: LayoutMode::default(),
                renderer: RendererKind::default(),
            };
            if repo.resolve_compare_request(&request).is_ok() {
                tracing::info!(
                    owner = %target.owner,
                    repo = %target.repo,
                    number = target.number,
                    "github-pr: using cached comparison refs"
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
        match request.forge() {
            ReviewForge::GitHub => self.execute_github(request, emit),
            ReviewForge::Bitbucket => {
                emit(request.fail("Bitbucket pull requests aren't supported yet.".to_owned()))
            }
        }
    }
}

impl LiveForge {
    fn execute_github(&self, request: ForgeRequest, emit: &mut dyn FnMut(ForgeResponse)) {
        let response = match request {
            ForgeRequest::LoadPullRequest {
                url,
                repo_path,
                token,
            } => {
                if let Ok(Some(comparison)) = self.cached_pull_request_comparison(&url, &repo_path)
                {
                    emit(ForgeResponse::PullRequestCached {
                        url: url.clone(),
                        comparison,
                    });
                }
                match self.load_pull_request(&url, &repo_path, token) {
                    Ok(comparison) => ForgeResponse::PullRequestLoaded { url, comparison },
                    Err(error) => ForgeResponse::PullRequestLoadFailed {
                        url,
                        message: error.user_message(),
                    },
                }
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
                            message: error.user_message(),
                        },
                    }
                }
            }
            ForgeRequest::PeekPullRequest { pr, token } => {
                match public_api(token).fetch_pull_request(&pr.owner, &pr.repo, pr.number) {
                    Ok(info) => ForgeResponse::PullRequestPeeked { pr, info },
                    Err(error) => ForgeResponse::PullRequestPeekFailed {
                        pr,
                        message: error.user_message(),
                    },
                }
            }
            ForgeRequest::FetchReviewComments { pr, token } => match public_api(token)
                .fetch_pull_request_review_comments(&pr.owner, &pr.repo, pr.number)
            {
                Ok(comments) => ForgeResponse::ReviewCommentsLoaded { pr, comments },
                Err(error) => ForgeResponse::ReviewCommentsLoadFailed {
                    pr,
                    message: error.user_message(),
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
                        message: error.user_message(),
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
                        draft_ids,
                        message: error.user_message(),
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
                    message: error.user_message(),
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
                    message: error.user_message(),
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
                    message: error.user_message(),
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
                    message: error.user_message(),
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
                    message: error.user_message(),
                },
            },
            ForgeRequest::FetchMyPullRequests { token, .. } => {
                match GitHubApi::with_token(token.into_inner()).fetch_my_pull_requests() {
                    Ok(pulls) => ForgeResponse::MyPullRequestsLoaded(pulls),
                    Err(error) => ForgeResponse::MyPullRequestsLoadFailed {
                        message: error.user_message(),
                    },
                }
            }
            ForgeRequest::AddPullRequestComment { pr, token, body } => {
                match api(token).add_pull_request_comment(&pr.owner, &pr.repo, pr.number, &body) {
                    Ok(()) => ForgeResponse::PullRequestCommentAdded { pr },
                    Err(error) => ForgeResponse::PullRequestCommentAddFailed {
                        pr,
                        body,
                        message: error.user_message(),
                    },
                }
            }
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
