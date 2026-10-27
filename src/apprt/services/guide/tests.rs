use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use super::GuideServices;
use crate::ai::RunOutcome;
use crate::contracts::guide::{
    ChangedFile, GuideRequest, GuideSubject, HunkSide, HunkSpot, StepKind,
};
use crate::contracts::shared::JobId;
use crate::core::review::ReviewTarget;
use crate::platform::guide_store::GuideStore;

const REPLY: &str = r#"{"overview":"Store and view.","steps":[
  {"title":"Change the store","explanation":"Line five changes.","hunks":[0]},
  {"title":"Add the view","explanation":"A new file.","hunks":[1]}
]}"#;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit(dir: &Path, files: &[(&str, String)]) -> String {
    for (path, text) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "change"]);
    git(dir, &["rev-parse", "HEAD"])
}

fn lines(change: Option<usize>) -> String {
    (1..=20)
        .map(|n| match change {
            Some(at) if at == n => format!("changed {n}\n"),
            _ => format!("line {n}\n"),
        })
        .collect()
}

struct Fixture {
    _dir: TempDir,
    _cache: TempDir,
    services: GuideServices,
    store: GuideStore,
    request: GuideRequest,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q"]);
    for (key, value) in [("user.name", "Diffy"), ("user.email", "diffy@example.com")] {
        git(dir.path(), &["config", key, value]);
    }
    let base = commit(
        dir.path(),
        &[
            ("src/store.rs", lines(None)),
            ("Cargo.lock", "lock v1\n".to_owned()),
        ],
    );
    let head = commit(
        dir.path(),
        &[
            ("src/store.rs", lines(Some(5))),
            ("src/view.rs", "fn render() {}\nfn paint() {}\n".to_owned()),
            ("Cargo.lock", "lock v2 secret\n".to_owned()),
        ],
    );
    let cache = TempDir::new().unwrap();
    let store = GuideStore::in_dir(cache.path());
    let file = |path: &str, status, stats| ChangedFile {
        path: path.to_owned(),
        old_path: None,
        status,
        stats: Some(stats),
        binary: false,
    };
    let request = GuideRequest {
        job: JobId(1),
        subject: GuideSubject::PullRequest(ReviewTarget::github("acme", "widgets", 7)),
        repo_path: dir.path().to_path_buf(),
        base,
        head,
        title: Some("Store and view".to_owned()),
        description: None,
        files: vec![
            file("Cargo.lock", 'M', (1, 1)),
            file("src/store.rs", 'M', (1, 1)),
            file("src/view.rs", 'A', (2, 0)),
        ],
    };
    Fixture {
        _dir: dir,
        _cache: cache,
        services: GuideServices::new(store.clone()),
        store,
        request,
    }
}

fn no_progress(_: usize, _: usize) -> bool {
    true
}

#[test]
fn mechanical_only_guides_never_call_a_model_and_are_cached() {
    let mut fx = fixture();
    fx.request.files.retain(|file| file.path == "Cargo.lock");
    let guide = fx
        .services
        .generate_with(
            &fx.request,
            "fake-model".to_owned(),
            &|| true,
            &mut no_progress,
            |_, _| panic!("mechanical changes must not call a model"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(guide.steps.len(), 1);
    assert_eq!(guide.steps[0].kind, StepKind::Mechanical);
    assert_eq!(guide.steps[0].anchors[0].path, "Cargo.lock");
    assert!(guide.overview.contains("No model was called"));
    assert_eq!(
        fx.services.load(
            &fx.request.subject.key(),
            &fx.request.base,
            &fx.request.head
        ),
        Some(guide)
    );
    assert_eq!(
        fx.services.generate_with(
            &fx.request,
            "fake-model".to_owned(),
            &|| false,
            &mut no_progress,
            |_, _| panic!("cancelled mechanical changes must not call a model"),
        ),
        Ok(None)
    );
}

#[test]
fn git_quoted_utf8_paths_keep_their_real_hunks() {
    let mut fx = fixture();
    let path = "src/café\t\"\\.rs";
    git(fx._dir.path(), &["config", "core.quotePath", "true"]);
    fx.request.base = commit(fx._dir.path(), &[(path, "before\n".into())]);
    fx.request.head = commit(fx._dir.path(), &[(path, "after\n".into())]);
    fx.request.files = vec![ChangedFile {
        path: path.into(),
        old_path: None,
        status: 'M',
        stats: Some((1, 1)),
        binary: false,
    }];
    let guide = fx
        .services
        .generate_with(
            &fx.request,
            "fake-model".into(),
            &|| true,
            &mut no_progress,
            |prompt, forward| {
                assert!(prompt.contains("H0 @@"), "{prompt}");
                assert!(prompt.contains("+after"), "{prompt}");
                forward(r#"{"overview":"Change.","steps":[{"title":"Edit","hunks":[0]}]}"#.into());
                Ok(RunOutcome::Finished)
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(guide.steps[0].anchors[0].path, path);
    assert_eq!(
        guide.steps[0].anchors[0].hunk,
        Some(HunkSpot {
            side: HunkSide::Old,
            line: 0
        })
    );
}

#[test]
fn generates_a_guide_from_the_repository_and_caches_it() {
    let fx = fixture();
    let mut prompt_seen = String::new();
    let guide = fx
        .services
        .generate_with(
            &fx.request,
            "fake-model".to_owned(),
            &|| true,
            &mut no_progress,
            |prompt, forward| {
                prompt_seen = prompt;
                for chunk in REPLY.as_bytes().chunks(16) {
                    forward(String::from_utf8_lossy(chunk).into_owned());
                }
                Ok(RunOutcome::Finished)
            },
        )
        .expect("generated")
        .expect("not cancelled");

    assert!(prompt_seen.contains("Title: Store and view"));
    assert!(prompt_seen.contains("H0 @@ -2,7 +2,7 @@"), "{prompt_seen}");
    assert!(prompt_seen.contains("+changed 5"));
    assert!(prompt_seen.contains("- Cargo.lock (lockfile)"));
    assert!(
        !prompt_seen.contains("secret"),
        "lockfile contents stay local"
    );

    assert_eq!(guide.model, "fake-model");
    assert_eq!(
        (guide.base.as_str(), guide.head.as_str()),
        (fx.request.base.as_str(), fx.request.head.as_str())
    );
    let anchors: Vec<_> = guide.steps[..2]
        .iter()
        .map(|step| (step.anchors[0].path.as_str(), step.anchors[0].hunk))
        .collect();
    assert_eq!(
        anchors,
        [
            (
                "src/store.rs",
                Some(HunkSpot {
                    side: HunkSide::Old,
                    line: 4
                })
            ),
            (
                "src/view.rs",
                Some(HunkSpot {
                    side: HunkSide::New,
                    line: 0
                })
            ),
        ]
    );
    assert_eq!(guide.steps[2].kind, StepKind::Mechanical);

    let key = fx.request.subject.key();
    let cached = fx
        .store
        .load(&key, &fx.request.base, &fx.request.head)
        .unwrap();
    assert_eq!(cached.as_ref(), Some(&guide));
    assert_eq!(
        fx.services
            .load(&key, &fx.request.base, "a-newer-head")
            .map(|g| g.head),
        Some(fx.request.head.clone()),
        "another head still finds the guide, which the UI marks out of date"
    );
}

#[test]
fn cancelled_runs_never_reach_the_model_or_the_cache() {
    let fx = fixture();
    let outcome = fx.services.generate_with(
        &fx.request,
        "fake-model".to_owned(),
        &|| false,
        &mut no_progress,
        |_, _| panic!("the model must not run"),
    );
    assert_eq!(outcome, Ok(None));

    let outcome = fx.services.generate_with(
        &fx.request,
        "fake-model".to_owned(),
        &|| true,
        &mut no_progress,
        |_, _| Ok(RunOutcome::Cancelled),
    );
    assert_eq!(outcome, Ok(None));
    let key = fx.request.subject.key();
    assert!(
        fx.services
            .load(&key, &fx.request.base, &fx.request.head)
            .is_none()
    );
}

#[test]
fn bad_replies_and_backend_errors_fail_without_caching() {
    let fx = fixture();
    let key = fx.request.subject.key();
    for (reply, error) in [
        (Ok("Sorry, I can't help with that."), "wasn\u{2019}t JSON"),
        (Err("Anthropic API returned 401 Unauthorized"), "401"),
    ] {
        let outcome = fx.services.generate_with(
            &fx.request,
            "fake-model".to_owned(),
            &|| true,
            &mut no_progress,
            |_, forward| {
                let text = reply.map_err(str::to_owned)?;
                forward(text.to_owned());
                Ok(RunOutcome::Finished)
            },
        );
        let message = outcome.expect_err("fails");
        assert!(message.contains(error), "{message}");
    }
    assert!(
        fx.services
            .load(&key, &fx.request.base, &fx.request.head)
            .is_none()
    );
}

#[test]
fn a_folder_that_is_not_a_repository_fails_readably() {
    let fx = fixture();
    let empty = TempDir::new().unwrap();
    let request = GuideRequest {
        repo_path: empty.path().to_path_buf(),
        ..fx.request.clone()
    };
    let message = fx
        .services
        .generate_with(
            &request,
            "m".to_owned(),
            &|| true,
            &mut no_progress,
            |_, _| panic!("the model must not run"),
        )
        .expect_err("fails");
    assert!(
        message.starts_with("Guides need a Git repository"),
        "{message}"
    );
}
