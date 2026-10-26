use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::contracts::mutations::AiBackend;
use crate::contracts::shared::JobId;
use crate::core::review::ReviewTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuideAction {
    Toggle,
    Open,
    Close,
    /// Spends the user's tokens, so only an explicit click or command sends it.
    Generate,
    Cancel,
    SelectStep(usize),
    StepBy(i32),
    HunkBy(i32),
}

/// What a guide is about. A guide made for one subject is shown, marked out
/// of date, after that subject's head moves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GuideSubject {
    PullRequest(ReviewTarget),
    Compare {
        repo: PathBuf,
        left: String,
        right: String,
    },
}

impl GuideSubject {
    /// Stable across restarts, unlike file keys and job ids.
    pub fn key(&self) -> String {
        match self {
            Self::PullRequest(pr) => format!(
                "pr:{}/{}/{}#{}",
                pr.forge.key(),
                pr.owner,
                pr.repo,
                pr.number
            ),
            Self::Compare { repo, left, right } => {
                format!("compare:{}:{left}..{right}", repo.display())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: char,
    pub stats: Option<(u32, u32)>,
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuideRequest {
    pub job: JobId,
    pub subject: GuideSubject,
    pub repo_path: PathBuf,
    pub base: String,
    pub head: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub files: Vec<ChangedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuideJob {
    pub request: GuideRequest,
    pub backend: AiBackend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuideEffect {
    /// Read the cached guide for this subject: the exact base and head if
    /// one exists, else the newest one.
    Load {
        job: JobId,
        subject: String,
        base: String,
        head: String,
    },
    Generate(Box<GuideJob>),
    Cancel {
        job: JobId,
    },
}

#[derive(Debug, Clone)]
pub enum GuideEvent {
    Loaded {
        job: JobId,
        guide: Option<Arc<Guide>>,
    },
    Progress {
        job: JobId,
        bytes: usize,
        steps: usize,
    },
    Generated {
        job: JobId,
        guide: Arc<Guide>,
    },
    Failed {
        job: JobId,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guide {
    pub overview: String,
    pub steps: Vec<GuideStep>,
    pub model: String,
    pub base: String,
    pub head: String,
    pub created_ms: u64,
}

impl Guide {
    pub fn is_for(&self, base: &str, head: &str) -> bool {
        self.base == base && self.head == head
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Model,
    Other,
    Mechanical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideStep {
    pub title: String,
    pub body: String,
    pub kind: StepKind,
    pub files: Vec<GuideFile>,
    pub anchors: Vec<GuideAnchor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A place in the diff. Paths, not file keys, so a cached guide still
/// resolves after a reload; `None` means the top of the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideAnchor {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunk: Option<HunkSpot>,
}

/// The first changed line of a hunk, zero-based on its side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkSpot {
    pub side: HunkSide,
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HunkSide {
    Old,
    New,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::shell::{Action, AppEvent, Effect};

    #[test]
    fn routes_through_root_contracts() {
        assert_eq!(
            Action::from(GuideAction::Generate),
            Action::Guide(GuideAction::Generate)
        );
        assert_eq!(
            Effect::from(GuideEffect::Cancel { job: JobId(3) }),
            Effect::Guide(GuideEffect::Cancel { job: JobId(3) })
        );
        assert!(matches!(
            AppEvent::from(GuideEvent::Failed {
                job: JobId(3),
                message: String::new()
            }),
            AppEvent::Guide(GuideEvent::Failed { job: JobId(3), .. })
        ));
    }

    #[test]
    fn subjects_key_by_forge_repo_and_number() {
        let pr = GuideSubject::PullRequest(ReviewTarget::github("acme", "widgets", 7));
        assert_eq!(pr.key(), "pr:github/acme/widgets#7");
        let local = GuideSubject::Compare {
            repo: PathBuf::from("/src/widgets"),
            left: "main".to_owned(),
            right: "feature".to_owned(),
        };
        assert_eq!(local.key(), "compare:/src/widgets:main..feature");
    }
}
