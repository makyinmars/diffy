use std::rc::Rc;
use std::sync::Arc;

use quark_ui::element::{ScrollAlign, ScrollHandle};

use crate::contracts::commands::{CommandDescriptor, CommandSource, ToastUpdate};
use crate::contracts::guide::{
    ChangedFile, Guide, GuideAction, GuideAnchor, GuideEffect, GuideEvent, GuideRequest,
    GuideSubject, HunkSide, HunkSpot,
};
use crate::contracts::shared::{JobId, SharedContext, Side, SourceTarget};
use crate::contracts::shell::{Controller, Intent, Reduction};
use crate::contracts::viewport::pipeline::ManifestFile;
use crate::core::file_rank;
use crate::core::keymap::ShortcutCommand;
use diffy_diff::FileStatus;

/// Where the panel stands. A ready guide lives beside the status, so a
/// failed regenerate keeps showing the previous guide.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Idle,
    Generating {
        job: JobId,
        bytes: usize,
        steps: usize,
    },
    Unavailable(String),
    Failed(String),
}

/// Why there is nothing to guide right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// No compare yet, or the open PR's compare is still loading.
    Waiting,
    /// Working-tree and staged compares have no commit to pin a guide to.
    WorkingTree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    subject: GuideSubject,
    base: String,
    head: String,
}

#[derive(Debug, Default)]
pub struct GuideController {
    open: bool,
    target: Option<Target>,
    /// Outlives `target` while a compare reloads, so the guide stays up
    /// until a different subject is ready.
    subject: Option<GuideSubject>,
    blocked: Option<Blocked>,
    guide: Option<Arc<Guide>>,
    status: Status,
    step: usize,
    hunk: usize,
    next_job: u64,
    lookup: Option<JobId>,
    description: Option<String>,
    pub scroll: ScrollHandle,
}

impl GuideController {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn guide(&self) -> Option<&Arc<Guide>> {
        self.guide.as_ref()
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn blocked(&self) -> Option<Blocked> {
        self.blocked
    }

    pub fn generating(&self) -> bool {
        matches!(self.status, Status::Generating { .. })
    }

    /// The guide was made for another base or head than the open compare.
    pub fn stale(&self) -> bool {
        match (&self.guide, &self.target) {
            (Some(guide), Some(target)) => !guide.is_for(&target.base, &target.head),
            _ => false,
        }
    }

    pub fn current_head(&self) -> Option<&str> {
        self.target.as_ref().map(|target| target.head.as_str())
    }

    pub fn selection(&self) -> (usize, usize) {
        (self.step, self.hunk)
    }

    /// The open pull request's description, when the overview has loaded it.
    pub fn observe_description(&mut self, description: Option<&str>) {
        self.description = description
            .map(str::trim)
            .filter(|body| !body.is_empty())
            .map(str::to_owned);
    }

    fn job(&mut self) -> JobId {
        self.next_job += 1;
        JobId(self.next_job)
    }

    fn refresh(&mut self, cx: &SharedContext) -> Reduction {
        let (next, blocked) = match derive(cx) {
            Ok(target) => (Some(target), None),
            Err(blocked) => (None, Some(blocked)),
        };
        let mut reduction = Reduction {
            changed: self.blocked != blocked,
            ..Reduction::default()
        };
        self.blocked = blocked;
        if next == self.target {
            return reduction;
        }
        reduction.merge(self.stop());
        reduction.changed = true;
        if let Some(next) = &next
            && self.subject.as_ref() != Some(&next.subject)
        {
            self.forget();
            self.subject = Some(next.subject.clone());
        }
        self.target = next;
        let Some(target) = &self.target else {
            self.lookup = None;
            return reduction;
        };
        let (subject, base, head) = (
            target.subject.key(),
            target.base.clone(),
            target.head.clone(),
        );
        let job = self.job();
        self.lookup = Some(job);
        reduction.effect(GuideEffect::Load {
            job,
            subject,
            base,
            head,
        })
    }

    fn forget(&mut self) {
        self.subject = None;
        self.guide = None;
        self.status = Status::Idle;
        self.step = 0;
        self.hunk = 0;
        self.scroll.set_offset(0.0, 0.0);
    }

    fn stop(&mut self) -> Reduction {
        let Status::Generating { job, .. } = self.status else {
            return Reduction::default();
        };
        self.status = Status::Idle;
        Reduction::changed().effect(GuideEffect::Cancel { job })
    }

    fn generate(&mut self, cx: &SharedContext) -> Reduction {
        self.open = true;
        if self.generating() {
            return Reduction::changed();
        }
        let (Some(target), Some(manifest), Some(repo)) = (
            self.target.clone(),
            cx.manifest.as_ref(),
            cx.repository.as_ref(),
        ) else {
            return Reduction::changed();
        };
        let job = self.job();
        let (title, description) = match &cx.review {
            Some(review) => (
                Some(review.pull_request.title.clone()),
                self.description.clone(),
            ),
            None => (None, None),
        };
        let request = GuideRequest {
            job,
            subject: target.subject,
            repo_path: repo.snapshot.path.clone(),
            base: target.base,
            head: target.head,
            title,
            description,
            files: manifest.files.iter().map(changed_file).collect(),
        };
        self.status = Status::Generating {
            job,
            bytes: 0,
            steps: 0,
        };
        Reduction::changed().intent(Intent::RequestGuide(Rc::new(request)))
    }

    fn select(&mut self, step: usize, hunk: usize, cx: &SharedContext) -> Reduction {
        self.open = true;
        let Some(anchor) = self
            .guide
            .as_ref()
            .and_then(|guide| guide.steps.get(step))
            .and_then(|s| s.anchors.get(hunk))
            .cloned()
        else {
            return Reduction::changed();
        };
        self.step = step;
        self.hunk = hunk;
        self.scroll
            .scroll_to_item(&step_key(step), ScrollAlign::Nearest);
        let reduction = Reduction::changed();
        match reveal(&anchor, cx) {
            Some(intent) => reduction.intent(intent),
            None => reduction.intent(Intent::Toast(ToastUpdate::info(format!(
                "{} isn\u{2019}t in this diff anymore.",
                anchor.path
            )))),
        }
    }

    fn step_by(&mut self, delta: i32, cx: &SharedContext) -> Reduction {
        let Some(count) = self.guide.as_ref().map(|guide| guide.steps.len()) else {
            return Reduction::default();
        };
        if count == 0 {
            return Reduction::default();
        }
        let step = (self.step as i64 + i64::from(delta)).clamp(0, count as i64 - 1) as usize;
        self.select(step, 0, cx)
    }

    /// Walks every anchor of the guide in order, crossing into the next or
    /// previous step at either end of the current one.
    fn hunk_by(&mut self, delta: i32, cx: &SharedContext) -> Reduction {
        let Some(guide) = self.guide.clone() else {
            return Reduction::default();
        };
        let flat: Vec<(usize, usize)> = guide
            .steps
            .iter()
            .enumerate()
            .flat_map(|(step, s)| (0..s.anchors.len()).map(move |hunk| (step, hunk)))
            .collect();
        let Some(last) = flat.len().checked_sub(1) else {
            return Reduction::default();
        };
        let at = flat
            .iter()
            .position(|&spot| spot == (self.step, self.hunk))
            .unwrap_or(0);
        let next = (at as i64 + i64::from(delta)).clamp(0, last as i64) as usize;
        let (step, hunk) = flat[next];
        self.select(step, hunk, cx)
    }

    fn finish(&mut self, job: JobId) -> bool {
        matches!(self.status, Status::Generating { job: current, .. } if current == job)
    }
}

/// The sibling key of a step's card, so the panel can scroll it into view.
pub fn step_key(step: usize) -> String {
    format!("guide.step.{step}")
}

fn derive(cx: &SharedContext) -> Result<Target, Blocked> {
    let (Some(manifest), Some(repo)) = (cx.manifest.as_ref(), cx.repository.as_ref()) else {
        return Err(Blocked::Waiting);
    };
    if manifest.working_tree {
        return Err(Blocked::WorkingTree);
    }
    if manifest.resolved_left.is_empty() || manifest.resolved_right.is_empty() {
        return Err(Blocked::Waiting);
    }
    let subject = match &cx.review {
        Some(review) => {
            let head = review.pull_request.head_sha.as_str();
            let right = manifest.resolved_right.as_str();
            if !head.is_empty() && !right.starts_with(head) && !head.starts_with(right) {
                return Err(Blocked::Waiting);
            }
            GuideSubject::PullRequest(review.target.clone())
        }
        None => {
            let path = &repo.snapshot.path;
            let saved = cx
                .settings
                .settings
                .last_compare
                .as_ref()
                .filter(|compare| compare.repo_path.as_deref() == Some(path.as_path()));
            let (left, right) = match saved {
                Some(compare) => (compare.left_ref.clone(), compare.right_ref.clone()),
                None => (
                    manifest.resolved_left.clone(),
                    manifest.resolved_right.clone(),
                ),
            };
            GuideSubject::Compare {
                repo: path.clone(),
                left,
                right,
            }
        }
    };
    Ok(Target {
        subject,
        base: manifest.resolved_left.clone(),
        head: manifest.resolved_right.clone(),
    })
}

fn changed_file(file: &ManifestFile) -> ChangedFile {
    let path = file.paths.display_path().into_owned();
    let old_path = matches!(file.status, FileStatus::Renamed | FileStatus::Copied)
        .then(|| file.paths.old_path().map(|old| old.into_owned()))
        .flatten()
        .filter(|old| *old != path);
    ChangedFile {
        path,
        old_path,
        status: file_rank::status_code(file.status),
        stats: file.stats.known(),
        binary: file.binary,
    }
}

fn reveal(anchor: &GuideAnchor, cx: &SharedContext) -> Option<Intent> {
    let manifest = cx.manifest.as_ref()?;
    let file = manifest.files.iter().find(|file| {
        file.paths.display_path() == anchor.path
            || file.paths.old_path().is_some_and(|old| old == anchor.path)
    })?;
    let (side, lines) = match anchor.hunk {
        Some(HunkSpot { side, line }) => (side, line..line + 1),
        None => (HunkSide::New, 0..0),
    };
    Some(Intent::RevealSource(SourceTarget {
        file: file.key,
        revision: file.revision,
        side: match side {
            HunkSide::Old => Side::Old,
            HunkSide::New => Side::New,
        },
        lines,
    }))
}

impl Controller for GuideController {
    type Action = GuideAction;
    type Event = GuideEvent;

    fn reduce_action(&mut self, action: GuideAction, cx: &SharedContext) -> Reduction {
        match action {
            GuideAction::Toggle => {
                self.open = !self.open;
                Reduction::changed()
            }
            GuideAction::Open => {
                self.open = true;
                Reduction::changed()
            }
            GuideAction::Close => {
                self.open = false;
                Reduction::changed()
            }
            GuideAction::Generate => self.generate(cx),
            GuideAction::Cancel => self.stop(),
            GuideAction::SelectStep(step) => self.select(step, 0, cx),
            GuideAction::StepBy(delta) => self.step_by(delta, cx),
            GuideAction::HunkBy(delta) => self.hunk_by(delta, cx),
        }
    }

    fn reduce_event(&mut self, event: GuideEvent, _cx: &SharedContext) -> Reduction {
        match event {
            GuideEvent::Loaded { job, guide } => {
                if self.lookup != Some(job) {
                    return Reduction::default();
                }
                self.lookup = None;
                let Some(guide) = guide else {
                    return Reduction::default();
                };
                if self.guide.as_ref() != Some(&guide) {
                    self.guide = Some(guide);
                    self.step = 0;
                    self.hunk = 0;
                }
                Reduction::changed()
            }
            GuideEvent::Progress { job, bytes, steps } => {
                if !self.finish(job) {
                    return Reduction::default();
                }
                self.status = Status::Generating { job, bytes, steps };
                Reduction::changed()
            }
            GuideEvent::Generated { job, guide } => {
                if !self.finish(job) {
                    return Reduction::default();
                }
                self.status = Status::Idle;
                let current = self
                    .target
                    .as_ref()
                    .is_some_and(|target| guide.is_for(&target.base, &target.head));
                if !current {
                    return Reduction::changed();
                }
                self.guide = Some(guide);
                self.step = 0;
                self.hunk = 0;
                self.scroll.set_offset(0.0, 0.0);
                Reduction::changed()
            }
            GuideEvent::Failed { job, message } => {
                if !self.finish(job) {
                    return Reduction::default();
                }
                self.status = Status::Failed(message.clone());
                let reduction = Reduction::changed();
                if self.open {
                    return reduction;
                }
                reduction.intent(Intent::Toast(ToastUpdate::error(format!(
                    "The guide failed: {message}"
                ))))
            }
        }
    }

    fn apply_intent(&mut self, intent: &Intent, cx: &SharedContext) -> Reduction {
        match intent {
            Intent::GuideUnavailable { job, reason } => {
                if !self.finish(*job) {
                    return Reduction::default();
                }
                self.status = Status::Unavailable(reason.clone());
                Reduction::changed()
            }
            Intent::ActivateWorkspace { .. } => {
                let mut reduction = self.stop();
                self.forget();
                reduction.merge(self.refresh(cx));
                reduction.changed = true;
                reduction
            }
            Intent::PublishManifest(_)
            | Intent::SetReviewTarget(_)
            | Intent::RepositoryChanged(_) => self.refresh(cx),
            _ => Reduction::default(),
        }
    }
}

impl CommandSource for GuideController {
    fn commands(&self, cx: &SharedContext, out: &mut Vec<CommandDescriptor>) {
        let has_guide = self.guide.is_some();
        let generating = self.generating();
        out.push(
            CommandDescriptor::new(
                "guide.toggle",
                if self.open {
                    "Hide guide"
                } else {
                    "Show guide"
                },
                GuideAction::Toggle,
            )
            .detail("A step-by-step walk through the change")
            .shortcut(ShortcutCommand::ToggleGuide)
            .enabled(cx.repository.is_some()),
        );
        out.push(
            CommandDescriptor::new(
                "guide.generate",
                if has_guide {
                    "Regenerate guide"
                } else {
                    "Generate guide"
                },
                GuideAction::Generate,
            )
            .detail("Uses your AI backend. Nothing is posted to the forge.")
            .enabled(self.target.is_some() && !generating),
        );
        if generating {
            out.push(CommandDescriptor::new(
                "guide.cancel",
                "Cancel guide",
                GuideAction::Cancel,
            ));
        }
        for (id, title, action, shortcut) in [
            (
                "guide.next_step",
                "Next guide step",
                GuideAction::StepBy(1),
                ShortcutCommand::NextGuideStep,
            ),
            (
                "guide.previous_step",
                "Previous guide step",
                GuideAction::StepBy(-1),
                ShortcutCommand::PreviousGuideStep,
            ),
            (
                "guide.next_hunk",
                "Next hunk in the guide",
                GuideAction::HunkBy(1),
                ShortcutCommand::NextGuideHunk,
            ),
            (
                "guide.previous_hunk",
                "Previous hunk in the guide",
                GuideAction::HunkBy(-1),
                ShortcutCommand::PreviousGuideHunk,
            ),
        ] {
            out.push(
                CommandDescriptor::new(id, title, action)
                    .shortcut(shortcut)
                    .enabled(has_guide),
            );
        }
    }
}
