use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use quark_app::testing::By;
use quark_ui::accessibility::AccessibilityRole as Role;

use crate::app::commands::fixtures::{self, REPO};
use crate::app::commands::harness::{self, Harness};
use crate::contracts::guide::{
    Guide, GuideAnchor, GuideEffect, GuideEvent, GuideFile, GuideStep, HunkSide, HunkSpot, StepKind,
};
use crate::contracts::mutations::{AiBackend, MutationsEvent};
use crate::contracts::navigation::{RepositoryContext, RepositoryRevision, RepositorySyncReason};
use crate::contracts::review::{ReviewContext, Secret};
use crate::contracts::shared::{AccountEpoch, FileKey, JobId, Side};
use crate::contracts::shell::{Effect, Intent, ShellEvent};
use crate::contracts::viewport::CompareManifestSnapshot;
use crate::core::forge::model::PullRequestInfo;
use crate::core::review::ReviewTarget;

const BASE: &str = "b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1";
const HEAD: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const NEWER: &str = "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2";

fn manifest(head: &str, working_tree: bool) -> Rc<CompareManifestSnapshot> {
    let mut manifest = (*fixtures::manifest(
        1,
        vec![
            fixtures::file(1, "src/store.rs"),
            fixtures::file(2, "src/view.rs"),
            fixtures::file(3, "Cargo.lock"),
        ],
    ))
    .clone();
    manifest.resolved_left = BASE.to_owned();
    manifest.resolved_right = head.to_owned();
    manifest.working_tree = working_tree;
    Rc::new(manifest)
}

fn review(head: &str) -> Intent {
    Intent::SetReviewTarget(Some(Rc::new(ReviewContext {
        account: AccountEpoch::default(),
        target: ReviewTarget::github("acme", "widgets", 7),
        pull_request: Arc::new(PullRequestInfo {
            title: "Widen store ids".to_owned(),
            number: 7,
            head_sha: head.to_owned(),
            ..PullRequestInfo::default()
        }),
        left_ref: "refs/diffy/pr/7/base".to_owned(),
        right_ref: "refs/diffy/pr/7/head".to_owned(),
    })))
}

/// A repository with pull request #7 open at HEAD.
fn opened(api_key: bool) -> Harness {
    let mut ui = harness::harness();
    harness::deliver(
        &mut ui,
        ShellEvent::RepositoryDialogClosed {
            path: Some(PathBuf::from(REPO)),
        },
    );
    let app = &mut ui.app_mut().0;
    app.inject(Intent::RepositoryChanged(Rc::new(RepositoryContext {
        revision: RepositoryRevision(1),
        snapshot: Arc::new(fixtures::snapshot(RepositorySyncReason::Open, false)),
    })));
    app.inject(review(HEAD));
    app.inject(Intent::PublishManifest(manifest(HEAD, false)));
    if api_key {
        harness::deliver(
            &mut ui,
            MutationsEvent::AiKeysLoaded {
                openai: None,
                anthropic: Some(Secret::new("sk-test")),
            },
        );
    }
    ui.frame();
    ui
}

fn guide_effects(ui: &Harness) -> Vec<GuideEffect> {
    harness::effects(ui)
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Guide(effect) => Some(effect),
            _ => None,
        })
        .collect()
}

fn generated_job(ui: &Harness) -> JobId {
    guide_effects(ui)
        .into_iter()
        .rev()
        .find_map(|effect| match effect {
            GuideEffect::Generate(job) => Some(job.request.job),
            _ => None,
        })
        .expect("a generate effect")
}

fn anchor(path: &str, side: HunkSide, line: u32) -> GuideAnchor {
    GuideAnchor {
        path: path.to_owned(),
        hunk: Some(HunkSpot { side, line }),
    }
}

fn sample(head: &str) -> Arc<Guide> {
    Arc::new(Guide {
        overview: "Store ids widen to 64 bits and a new view renders them.".to_owned(),
        steps: vec![
            GuideStep {
                title: "Widen the store".to_owned(),
                body: "Ids move to u64 so large imports stop wrapping.".to_owned(),
                kind: StepKind::Model,
                files: vec![GuideFile {
                    path: "src/store.rs".to_owned(),
                    note: None,
                }],
                anchors: vec![
                    anchor("src/store.rs", HunkSide::Old, 1),
                    anchor("src/store.rs", HunkSide::New, 12),
                ],
            },
            GuideStep {
                title: "Render the store".to_owned(),
                body: "A new view paints the widened ids.".to_owned(),
                kind: StepKind::Model,
                files: vec![GuideFile {
                    path: "src/view.rs".to_owned(),
                    note: None,
                }],
                anchors: vec![anchor("src/view.rs", HunkSide::New, 0)],
            },
            GuideStep {
                title: "Mechanical and generated".to_owned(),
                body: "Listed by name only.".to_owned(),
                kind: StepKind::Mechanical,
                files: vec![GuideFile {
                    path: "Cargo.lock".to_owned(),
                    note: Some("lockfile".to_owned()),
                }],
                anchors: vec![GuideAnchor {
                    path: "Cargo.lock".to_owned(),
                    hunk: None,
                }],
            },
        ],
        model: "claude-sonnet-5-5".to_owned(),
        base: BASE.to_owned(),
        head: head.to_owned(),
        created_ms: 1,
    })
}

fn ready(ui: &mut Harness) {
    ui.click_node(By::test_id("guide.generate"));
    let job = generated_job(ui);
    harness::deliver(
        ui,
        GuideEvent::Generated {
            job,
            guide: sample(HEAD),
        },
    );
}

fn reveals(ui: &Harness) -> Option<(FileKey, Side, u32)> {
    let selection = ui.app().0.shared().selected_file?;
    let guide = &ui.app().0.controllers().guide;
    let (step, hunk) = guide.selection();
    let anchor = &guide.guide()?.steps[step].anchors[hunk];
    let spot = anchor.hunk?;
    let side = match spot.side {
        HunkSide::Old => Side::Old,
        HunkSide::New => Side::New,
    };
    Some((selection.key, side, spot.line))
}

#[test]
fn opening_the_guide_never_spends_tokens() {
    let mut ui = opened(true);
    assert!(ui.try_find(By::test_id("guide")).is_none());
    ui.key("i");
    ui.find(By::role_name(Role::Complementary, "Guide"));
    ui.find(By::test_id("guide.empty"));
    assert!(
        !guide_effects(&ui)
            .iter()
            .any(|effect| matches!(effect, GuideEffect::Generate(_))),
        "only Generate asks the model"
    );
    let loads: Vec<_> = guide_effects(&ui)
        .into_iter()
        .filter_map(|effect| match effect {
            GuideEffect::Load { subject, head, .. } => Some((subject, head)),
            _ => None,
        })
        .collect();
    assert_eq!(
        loads.last(),
        Some(&("pr:github/acme/widgets#7".to_owned(), HEAD.to_owned())),
        "the cache is checked for this PR head"
    );
    ui.key("i");
    assert!(ui.try_find(By::test_id("guide")).is_none());
}

#[test]
fn without_a_backend_the_guide_links_to_ai_settings() {
    let mut ui = opened(false);
    ui.key("i");
    ui.click_node(By::test_id("guide.generate"));
    let notice = ui.find(By::test_id("guide.unavailable"));
    assert_eq!(notice.role, Some(Role::Alert));
    ui.find(By::role_name(
        Role::Alert,
        "Guided review needs an AI backend",
    ));
    ui.find(By::name("Add an AI key under Settings \u{2192} AI first."));
    ui.click_node(By::role_name(Role::Button, "Open AI settings"));
    ui.find(By::role_name(Role::Dialog, "Settings"));
    assert!(ui.app().0.controllers().commands.settings_open());

    ui.key("escape");
    harness::deliver(
        &mut ui,
        MutationsEvent::AiKeysLoaded {
            openai: None,
            anthropic: Some(Secret::new("sk-added")),
        },
    );
    ui.click_node(By::role_name(Role::Button, "Try again"));
    ui.find(By::test_id("guide.generating"));
    generated_job(&ui);
}

#[test]
fn generating_shows_progress_and_cancel_stops_the_job() {
    let mut ui = opened(true);
    ui.key("i");
    ui.click_node(By::test_id("guide.generate"));
    let job = generated_job(&ui);
    let request = guide_effects(&ui)
        .into_iter()
        .find_map(|effect| match effect {
            GuideEffect::Generate(job) => Some(job),
            _ => None,
        })
        .unwrap();
    assert!(matches!(request.backend, AiBackend::Api { .. }));
    assert_eq!(request.request.title.as_deref(), Some("Widen store ids"));
    assert_eq!(request.request.head, HEAD);
    assert_eq!(request.request.files.len(), 3);

    ui.find(By::role_name(Role::Status, "Writing the guide\u{2026}"));
    harness::deliver(
        &mut ui,
        GuideEvent::Progress {
            job,
            bytes: 2_048,
            steps: 2,
        },
    );
    ui.find(By::name("2 steps drafted, 2 KB received."));
    ui.click_node(By::role_name(Role::Button, "Cancel"));
    assert!(guide_effects(&ui).contains(&GuideEffect::Cancel { job }));
    ui.find(By::test_id("guide.empty"));
    harness::deliver(
        &mut ui,
        GuideEvent::Generated {
            job,
            guide: sample(HEAD),
        },
    );
    assert!(
        ui.try_find(By::test_id("guide.steps")).is_none(),
        "a cancelled job's result is dropped"
    );
}

#[test]
fn a_ready_guide_lists_steps_and_walks_the_diff() {
    let mut ui = opened(true);
    ui.key("i");
    ready(&mut ui);
    ui.find(By::test_id("guide.overview"));
    ui.find(By::name(
        "Store ids widen to 64 bits and a new view renders them.",
    ));
    let steps = ui.find_all(By::role(Role::ListBoxOption));
    let names: Vec<&str> = steps
        .iter()
        .filter_map(|node| node.name.as_deref())
        .filter(|name| name.starts_with("Step "))
        .collect();
    assert_eq!(
        names,
        [
            "Step 1: Widen the store",
            "Step 2: Render the store",
            "Step 3: Mechanical and generated",
        ]
    );
    let update = ui.accessibility_update();
    let description = |label: &str| {
        update
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label))
            .and_then(|(_, node)| node.description().map(str::to_owned))
    };
    assert_eq!(
        description("Step 1: Widen the store").as_deref(),
        Some("Ids move to u64 so large imports stop wrapping. Files: src/store.rs. Place 1 of 2.")
    );
    assert_eq!(
        description("Step 3: Mechanical and generated").as_deref(),
        Some("Listed by name only. Files: Cargo.lock (lockfile).")
    );
    ui.find(By::test_id("guide.model"));

    ui.click_node(By::role_name(
        Role::ListBoxOption,
        "Step 1: Widen the store",
    ));
    assert_eq!(reveals(&ui), Some((FileKey(1), Side::Old, 1)));
    assert!(ui.painted_text().contains("1 of 2"));
    ui.key(".");
    assert_eq!(ui.app().0.controllers().guide.selection(), (0, 1));
    assert!(ui.painted_text().contains("2 of 2"));
    ui.key(".");
    assert_eq!(
        ui.app().0.controllers().guide.selection(),
        (1, 0),
        "the next hunk crosses into the next step"
    );
    assert_eq!(reveals(&ui), Some((FileKey(2), Side::New, 0)));
    ui.key("shift+.");
    assert_eq!(ui.app().0.controllers().guide.selection(), (2, 0));
    assert_eq!(
        ui.app().0.shared().selected_file.map(|file| file.key),
        Some(FileKey(3)),
        "a file-level anchor selects the file"
    );
    ui.key("shift+,");
    ui.key(",");
    assert_eq!(ui.app().0.controllers().guide.selection(), (0, 1));
}

#[test]
fn a_moved_head_marks_the_guide_out_of_date() {
    let mut ui = opened(true);
    ui.key("i");
    ready(&mut ui);
    assert!(ui.try_find(By::test_id("guide.stale")).is_none());

    let app = &mut ui.app_mut().0;
    app.inject(review(NEWER));
    app.inject(Intent::PublishManifest(manifest(NEWER, false)));
    ui.frame();
    ui.find(By::role_name(Role::Status, "Out of date"));
    ui.find(By::name(
        "This guide is for a1a1a1a. The diff is now at c2c2c2c.",
    ));
    ui.find(By::role_name(
        Role::ListBoxOption,
        "Step 1: Widen the store",
    ));
    ui.click_node(By::test_id("guide.regenerate"));
    let job = generated_job(&ui);
    let latest = guide_effects(&ui)
        .into_iter()
        .rev()
        .find_map(|effect| match effect {
            GuideEffect::Generate(job) => Some(job.request.head),
            _ => None,
        });
    assert_eq!(latest.as_deref(), Some(NEWER));
    harness::deliver(
        &mut ui,
        GuideEvent::Generated {
            job,
            guide: sample(HEAD),
        },
    );
    assert!(
        ui.app()
            .0
            .controllers()
            .guide
            .guide()
            .is_some_and(|g| g.head == HEAD),
        "a guide for an older head is never applied as current"
    );
}

#[test]
fn a_cached_guide_for_the_open_head_shows_without_a_model_call() {
    let mut ui = opened(true);
    let job = guide_effects(&ui)
        .into_iter()
        .rev()
        .find_map(|effect| match effect {
            GuideEffect::Load { job, .. } => Some(job),
            _ => None,
        })
        .expect("a cache lookup");
    harness::deliver(
        &mut ui,
        GuideEvent::Loaded {
            job: JobId(job.0 + 100),
            guide: Some(sample(HEAD)),
        },
    );
    ui.key("i");
    ui.find(By::test_id("guide.empty"));
    harness::deliver(
        &mut ui,
        GuideEvent::Loaded {
            job,
            guide: Some(sample(HEAD)),
        },
    );
    ui.find(By::test_id("guide.steps"));
    assert!(ui.try_find(By::test_id("guide.stale")).is_none());
    assert!(
        !guide_effects(&ui)
            .iter()
            .any(|effect| matches!(effect, GuideEffect::Generate(_)))
    );
}

#[test]
fn a_failed_run_explains_and_offers_a_retry() {
    let mut ui = opened(true);
    ui.key("i");
    ui.click_node(By::test_id("guide.generate"));
    let job = generated_job(&ui);
    harness::deliver(
        &mut ui,
        GuideEvent::Failed {
            job: JobId(job.0 + 7),
            message: "stale".to_owned(),
        },
    );
    ui.find(By::test_id("guide.generating"));
    harness::deliver(
        &mut ui,
        GuideEvent::Failed {
            job,
            message: "The model\u{2019}s reply wasn\u{2019}t JSON.".to_owned(),
        },
    );
    let failed = ui.find(By::test_id("guide.failed"));
    assert_eq!(failed.role, Some(Role::Alert));
    ui.find(By::name("The model\u{2019}s reply wasn\u{2019}t JSON."));
    ui.click_node(By::role_name(Role::Button, "Try again"));
    assert_ne!(generated_job(&ui), job);
    ui.find(By::test_id("guide.generating"));
}

#[test]
fn working_tree_compares_explain_why_there_is_no_guide() {
    let mut ui = opened(true);
    let app = &mut ui.app_mut().0;
    app.inject(Intent::SetReviewTarget(None));
    app.inject(Intent::PublishManifest(manifest(HEAD, true)));
    ui.frame();
    ui.key("i");
    ui.find(By::role_name(Role::Status, "Guides need a commit range"));
    assert!(ui.try_find(By::test_id("guide.generate")).is_none());
}

#[test]
fn local_compares_get_a_guide_keyed_by_the_compare() {
    let mut ui = opened(true);
    let app = &mut ui.app_mut().0;
    app.inject(Intent::SetReviewTarget(None));
    ui.frame();
    let subject = guide_effects(&ui)
        .into_iter()
        .rev()
        .find_map(|effect| match effect {
            GuideEffect::Load { subject, .. } => Some(subject),
            _ => None,
        });
    assert!(
        subject.is_some_and(|subject| subject.starts_with("compare:")),
        "a non-PR compare is its own subject"
    );
    ui.key("i");
    ui.click_node(By::test_id("guide.generate"));
    let request = guide_effects(&ui)
        .into_iter()
        .find_map(|effect| match effect {
            GuideEffect::Generate(job) => Some(job.request),
            _ => None,
        })
        .unwrap();
    assert_eq!(request.title, None);
}

#[test]
fn the_palette_lists_guide_commands() {
    let mut ui = opened(true);
    ui.key("mod+k");
    ui.type_text("guide");
    ui.find(By::name("Show guide"));
    ui.find(By::name("Generate guide"));
}
