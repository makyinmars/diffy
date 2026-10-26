use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::ai::guide::{self as builder, GUIDE_MAX_TOKENS, MAX_GUIDE_BYTES, MAX_GUIDE_FILES};
use crate::ai::{ModelTier, RunOutcome};
use crate::contracts::guide::{Guide, GuideJob, GuideRequest};
use crate::core::vcs::git::GitService;
use crate::platform::guide_store::GuideStore;

/// Paths per `git diff` call while reading hunks.
const PATHS_PER_DIFF: usize = 64;
/// Stop reading diffs once there is clearly more than the prompt can carry.
const READ_LIMIT: usize = 4 * MAX_GUIDE_BYTES;
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub struct GuideServices {
    store: GuideStore,
}

impl GuideServices {
    pub fn new(store: GuideStore) -> Self {
        Self { store }
    }

    pub fn load(&self, subject: &str, base: &str, head: &str) -> Option<Guide> {
        self.store
            .load(subject, base, head)
            .inspect_err(|error| tracing::warn!(%error, "guide: cache read failed"))
            .ok()
            .flatten()
    }

    /// Reads the compare, asks the model, validates the reply and caches the
    /// guide. `Ok(None)` means the run was cancelled.
    pub fn generate(
        &self,
        job: GuideJob,
        live: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(usize, usize) -> bool,
    ) -> Result<Option<Guide>, String> {
        let GuideJob { request, backend } = job;
        let model = backend.model_label(ModelTier::Thorough);
        tracing::info!(
            job = request.job.0,
            backend = backend.label(),
            %model,
            files = request.files.len(),
            "guide: dispatch"
        );
        self.generate_with(&request, model, live, progress, |prompt, forward| {
            super::ai::complete(
                backend,
                prompt,
                GUIDE_MAX_TOKENS,
                ModelTier::Thorough,
                live,
                forward,
            )
        })
    }

    fn generate_with(
        &self,
        request: &GuideRequest,
        model: String,
        live: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(usize, usize) -> bool,
        complete: impl FnOnce(String, &mut dyn FnMut(String) -> bool) -> Result<RunOutcome, String>,
    ) -> Result<Option<Guide>, String> {
        let started = Instant::now();
        let Some(patches) = read_patches(request, live)? else {
            return Ok(None);
        };
        let prompt = builder::build_prompt(
            request.title.as_deref(),
            request.description.as_deref(),
            &request.files,
            &patches,
        );
        tracing::info!(
            job = request.job.0,
            bytes = prompt.text.len(),
            indexed = prompt.outline.indexed_files(),
            hunks = prompt.outline.sent_hunks(),
            "guide: prompt built"
        );
        let mut reply = String::new();
        let mut reported = Instant::now();
        let mut forward = |chunk: String| {
            reply.push_str(&chunk);
            if reported.elapsed() < PROGRESS_EVERY {
                return true;
            }
            reported = Instant::now();
            progress(reply.len(), builder::drafted_steps(&reply))
        };
        if complete(prompt.text, &mut forward)? == RunOutcome::Cancelled {
            return Ok(None);
        }
        let parsed = builder::parse_guide(&reply, &prompt.outline).inspect_err(|message| {
            tracing::warn!(job = request.job.0, %message, reply_bytes = reply.len(), "guide: reply rejected");
        })?;
        let guide = Guide {
            overview: parsed.overview,
            steps: parsed.steps,
            model,
            base: request.base.clone(),
            head: request.head.clone(),
            created_ms: now_ms(),
        };
        if let Err(error) = self.store.save(&request.subject.key(), &guide) {
            tracing::warn!(%error, "guide: cache write failed");
        }
        tracing::info!(
            job = request.job.0,
            steps = guide.steps.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "guide: finished"
        );
        Ok(Some(guide))
    }
}

/// Hunks of the reviewable files, in reading order, until the prompt budget
/// is clearly covered. Mechanical files are never read.
fn read_patches(
    request: &GuideRequest,
    live: &dyn Fn() -> bool,
) -> Result<Option<HashMap<String, Vec<builder::PatchHunk>>>, String> {
    let git = GitService::new_with_open(Path::new(&request.repo_path))
        .map_err(|error| format!("Guides need a Git repository: {}", error.user_message()))?;
    let order = builder::reviewable_order(&request.files);
    let mut patches = HashMap::new();
    let mut read = 0;
    for chunk in order[..order.len().min(MAX_GUIDE_FILES)].chunks(PATHS_PER_DIFF) {
        if !live() {
            return Ok(None);
        }
        if read >= READ_LIMIT {
            break;
        }
        let mut paths: Vec<&str> = Vec::with_capacity(chunk.len() * 2);
        for &i in chunk {
            let file = &request.files[i];
            paths.push(&file.path);
            if let Some(old) = file.old_path.as_deref().filter(|old| *old != file.path) {
                paths.push(old);
            }
        }
        let raw = git
            .diff_refs_paths(&request.base, &request.head, &paths)
            .map_err(|error| format!("Couldn\u{2019}t read the diff: {}", error.user_message()))?;
        read += raw.len();
        patches.extend(builder::parse_patch(&String::from_utf8_lossy(&raw)));
    }
    Ok(Some(patches))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests;
