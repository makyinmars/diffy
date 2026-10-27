use std::collections::HashMap;

use super::*;
use crate::contracts::guide::{ChangedFile, HunkSide, HunkSpot, StepKind};

const PATCH: &str = "diff --git a/src/store.rs b/src/store.rs
index 1111111..2222222 100644
--- a/src/store.rs
+++ b/src/store.rs
@@ -1,4 +1,5 @@
 pub struct Store {
-    items: Vec<u32>,
+    items: Vec<u64>,
+    limit: usize,
 }
 
@@ -10,3 +11,4 @@ impl Store {
     fn len(&self) -> usize {
         self.items.len()
+        // counted
     }
diff --git a/src/view.rs b/src/view.rs
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/view.rs
@@ -0,0 +1,2 @@
+fn render() {}
+fn paint() {}
diff --git a/src/old_name.rs b/src/new_name.rs
similarity index 90%
rename from src/old_name.rs
rename to src/new_name.rs
index 4444444..5555555 100644
--- a/src/old_name.rs
+++ b/src/new_name.rs
@@ -3,2 +3,2 @@ fn helper() {
-    old();
+    new();
     done();
diff --git a/src/gone.rs b/src/gone.rs
deleted file mode 100644
index 6666666..0000000
--- a/src/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-fn gone() {}
--- not a header, a removed line
";

fn file(path: &str, status: char, stats: Option<(u32, u32)>) -> ChangedFile {
    ChangedFile {
        path: path.to_owned(),
        old_path: None,
        status,
        stats,
        binary: false,
    }
}

fn files() -> Vec<ChangedFile> {
    vec![
        file("Cargo.lock", 'M', Some((10, 8))),
        file("src/store.rs", 'M', Some((3, 1))),
        file("src/view.rs", 'A', Some((2, 0))),
        ChangedFile {
            old_path: Some("src/old_name.rs".to_owned()),
            ..file("src/new_name.rs", 'R', Some((1, 1)))
        },
        file("src/gone.rs", 'D', Some((0, 2))),
        ChangedFile {
            binary: true,
            ..file("assets/logo.png", 'M', None)
        },
        file("tests/store_test.rs", 'M', Some((5, 0))),
    ]
}

fn patches() -> HashMap<String, Vec<PatchHunk>> {
    let mut patches: HashMap<_, _> = parse_patch(PATCH).into_iter().collect();
    let lock = "diff --git a/Cargo.lock b/Cargo.lock\n--- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1 +1 @@\n-lockfile-secret-line\n+lockfile-other-line\n";
    patches.insert("Cargo.lock".to_owned(), parse_patch(lock).remove(0).1);
    patches
}

fn prompt() -> GuidePrompt {
    build_prompt(
        Some("Widen store ids"),
        Some("We need 64-bit ids."),
        &files(),
        &patches(),
    )
}

#[test]
fn quoted_paths_decode_octal_utf8_and_c_escapes() {
    assert_eq!(
        unquote(r#""src/caf\303\251\t\"\\.rs""#),
        "src/café\t\"\\.rs"
    );
    assert_eq!(unquote(r#""\a\b\v\f\r\n""#), "\u{7}\u{8}\u{b}\u{c}\r\n");
}

#[test]
fn mechanical_only_outlines_produce_a_synthetic_guide() {
    let prompt = build_prompt(
        None,
        None,
        &[
            file("Cargo.lock", 'M', Some((1, 1))),
            file("gone.rs", 'D', Some((0, 1))),
            file("renamed.rs", 'R', Some((0, 0))),
        ],
        &HashMap::new(),
    );
    assert!(!prompt.outline.needs_model());
    let guide = mechanical_guide(&prompt.outline).unwrap();
    assert_eq!(guide.steps.len(), 1);
    assert_eq!(guide.steps[0].kind, StepKind::Mechanical);
    assert_eq!(guide.steps[0].files.len(), 3);
    assert!(mechanical_guide(&Outline::default()).is_err());
    assert!(self::prompt().outline.needs_model());
}

fn spot(side: HunkSide, line: u32) -> Option<HunkSpot> {
    Some(HunkSpot { side, line })
}

#[test]
fn patches_split_into_files_and_hunks_with_first_changed_lines() {
    let parsed = parse_patch(PATCH);
    let paths: Vec<&str> = parsed.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "src/store.rs",
            "src/view.rs",
            "src/new_name.rs",
            "src/gone.rs"
        ]
    );
    let store = &parsed[0].1;
    assert_eq!(store.len(), 2);
    assert_eq!(store[0].header, "@@ -1,4 +1,5 @@");
    assert_eq!(Some(store[0].spot), spot(HunkSide::Old, 1));
    assert_eq!(store[0].lines.len(), 6, "the blank context line stays");
    assert_eq!(Some(store[1].spot), spot(HunkSide::New, 12));
    assert_eq!(Some(parsed[1].1[0].spot), spot(HunkSide::New, 0));
    assert_eq!(Some(parsed[2].1[0].spot), spot(HunkSide::Old, 2));
    let gone = &parsed[3].1;
    assert_eq!(gone.len(), 1);
    assert_eq!(
        gone[0].lines.last().map(String::as_str),
        Some("--- not a header, a removed line"),
        "a removed line that looks like a header stays in its hunk"
    );
}

#[test]
fn quoted_paths_and_missing_counts_parse() {
    let patch = "diff --git \"a/dir/sp ace.rs\" \"b/dir/sp ace.rs\"\n--- \"a/dir/sp ace.rs\"\n+++ \"b/dir/sp ace.rs\"\n@@ -7 +7 @@\n-a\n+b\n";
    let parsed = parse_patch(patch);
    assert_eq!(parsed[0].0, "dir/sp ace.rs");
    assert_eq!(Some(parsed[0].1[0].spot), spot(HunkSide::Old, 6));
}

#[test]
fn mechanical_files_are_named_with_a_reason() {
    let reasons: Vec<Option<&str>> = files().iter().map(mechanical_reason).collect();
    assert_eq!(
        reasons,
        [
            Some("lockfile"),
            None,
            None,
            None,
            Some("deleted"),
            Some("binary or asset"),
            None,
        ]
    );
    let renamed = ChangedFile {
        old_path: Some("a.rs".to_owned()),
        ..file("b.rs", 'R', Some((0, 0)))
    };
    assert_eq!(mechanical_reason(&renamed), Some("renamed without edits"));
    assert_eq!(
        mechanical_reason(&file("vendor/lib/x.go", 'M', Some((4, 4)))),
        Some("generated or vendored")
    );
}

#[test]
fn prompt_numbers_reviewable_files_and_hunks_and_names_the_rest() {
    let prompt = prompt();
    let text = &prompt.text;
    assert!(text.contains("Title: Widen store ids\n"));
    assert!(text.contains("Description:\nWe need 64-bit ids.\n"));
    for line in [
        "F0 R +1 -1 src/old_name.rs -> src/new_name.rs\n",
        "F1 M +3 -1 src/store.rs\n",
        "F2 A +2 -0 src/view.rs\n",
        "F3 M +5 -0 tests/store_test.rs (hunks not shown)\n",
        "- Cargo.lock (lockfile)\n",
        "- src/gone.rs (deleted)\n",
        "- assets/logo.png (binary or asset)\n",
        "\nF1 src/store.rs\nH1 @@ -1,4 +1,5 @@\n pub struct Store {\n",
        "H2 @@ -10,3 +11,4 @@ impl Store {\n",
        "H3 @@ -0,0 +1,2 @@\n+fn render() {}\n",
    ] {
        assert!(text.contains(line), "missing {line:?} in\n{text}");
    }
    assert!(
        !text.contains("lockfile-secret-line"),
        "mechanical contents are never sent"
    );
    assert!(!text.contains("fn gone"));
    assert_eq!(prompt.outline.indexed_files(), 4);
    assert_eq!(prompt.outline.sent_hunks(), 4);
}

fn hunk(at: u32, bytes: usize) -> PatchHunk {
    let lines: Vec<String> = (0..bytes / 20).map(|i| format!("+line {i:>12}")).collect();
    PatchHunk {
        header: format!("@@ -{at},0 +{at},{} @@", lines.len()),
        lines,
        spot: HunkSpot {
            side: HunkSide::New,
            line: at,
        },
    }
}

#[test]
fn budget_shows_every_file_first_and_stays_under_the_cap() {
    let files: Vec<ChangedFile> = (0..60)
        .map(|i| file(&format!("src/m{i:02}.rs"), 'M', Some((30, 0))))
        .collect();
    let patches: HashMap<String, Vec<PatchHunk>> = files
        .iter()
        .map(|f| (f.path.clone(), (0..4).map(|h| hunk(h * 100, 600)).collect()))
        .collect();
    let prompt = build_prompt(None, None, &files, &patches);
    assert!(
        prompt.text.len() <= MAX_GUIDE_BYTES,
        "{}",
        prompt.text.len()
    );
    for f in &files {
        assert!(
            prompt.text.contains(&format!(" {}\nH", f.path)),
            "{} shows at least one hunk",
            f.path
        );
    }
    let sent = prompt.outline.sent_hunks();
    assert!(sent > 60 && sent < 240, "{sent} hunks sent");
    assert!(prompt.text.contains("F0 M +30 -0 src/m00.rs\n"));
}

#[test]
fn long_lines_and_hunks_are_cut_and_file_lists_are_capped() {
    let long = PatchHunk {
        header: "@@ -1,1 +1,1 @@".to_owned(),
        lines: vec![format!("+{}", "x".repeat(1_000))],
        spot: HunkSpot {
            side: HunkSide::New,
            line: 0,
        },
    };
    let tall = hunk(1, 8_000);
    let files = vec![
        file("src/a.rs", 'M', Some((1, 1))),
        file("src/b.rs", 'M', Some((400, 0))),
    ];
    let patches = HashMap::from([
        ("src/a.rs".to_owned(), vec![long]),
        ("src/b.rs".to_owned(), vec![tall]),
    ]);
    let description = "d".repeat(10_000);
    let prompt = build_prompt(None, Some(&description), &files, &patches);
    assert!(prompt.text.contains("...[truncated]"));
    assert!(!prompt.text.contains(&"x".repeat(300)));
    assert!(prompt.text.contains("more lines]\n"));
    assert!(prompt.text.contains("[description truncated]"));
    assert!(!prompt.text.contains(&"d".repeat(5_000)));

    let many: Vec<ChangedFile> = (0..MAX_GUIDE_FILES + 10)
        .map(|i| file(&format!("src/f{i:04}.rs"), 'M', Some((1, 1))))
        .collect();
    let prompt = build_prompt(None, None, &many, &HashMap::new());
    assert_eq!(prompt.outline.indexed_files(), MAX_GUIDE_FILES);
    assert!(prompt.text.contains("10 more files are not listed.\n"));
    assert!(prompt.text.len() <= MAX_GUIDE_BYTES);
}

const REPLY: &str = r#"{"overview":"Widens store ids to 64 bits and renders them.","steps":[
  {"title":"Widen the store","explanation":"Store ids move to u64 and gain a limit.","hunks":[1,2]},
  {"title":"Render the store","explanation":"A new view paints it.","hunks":["H3"],"files":["F0"]}
]}"#;

#[test]
fn a_valid_reply_becomes_steps_with_anchors_and_closing_steps() {
    let prompt = prompt();
    let guide = parse_guide(REPLY, &prompt.outline).expect("guide");
    assert_eq!(
        guide.overview,
        "Widens store ids to 64 bits and renders them."
    );
    let titles: Vec<&str> = guide.steps.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Widen the store",
            "Render the store",
            OTHER_TITLE,
            MECHANICAL_TITLE
        ]
    );
    let widen = &guide.steps[0];
    assert_eq!(widen.kind, StepKind::Model);
    let spots: Vec<_> = widen
        .anchors
        .iter()
        .map(|a| (a.path.as_str(), a.hunk))
        .collect();
    assert_eq!(
        spots,
        [
            ("src/store.rs", spot(HunkSide::Old, 1)),
            ("src/store.rs", spot(HunkSide::New, 12)),
        ]
    );
    let render = &guide.steps[1];
    let spots: Vec<_> = render
        .anchors
        .iter()
        .map(|a| (a.path.as_str(), a.hunk))
        .collect();
    assert_eq!(
        spots,
        [
            ("src/view.rs", spot(HunkSide::New, 0)),
            ("src/new_name.rs", spot(HunkSide::Old, 2)),
        ],
        "a listed file brings its hunks after the named ones"
    );
    let other = &guide.steps[2];
    assert_eq!(other.kind, StepKind::Other);
    assert_eq!(other.anchors.len(), 1);
    assert_eq!(other.anchors[0].path, "tests/store_test.rs");
    assert_eq!(other.anchors[0].hunk, None);
    let mechanical = &guide.steps[3];
    assert_eq!(mechanical.kind, StepKind::Mechanical);
    let named: Vec<_> = mechanical
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.note.as_deref()))
        .collect();
    assert_eq!(
        named,
        [
            ("Cargo.lock", Some("lockfile")),
            ("src/gone.rs", Some("deleted")),
            ("assets/logo.png", Some("binary or asset")),
        ]
    );
}

#[test]
fn fenced_prose_wrapped_and_bare_array_replies_parse() {
    let outline = prompt().outline;
    for reply in [
        format!("FENCEjson\n{REPLY}\nFENCE").replace("FENCE", "\u{60}\u{60}\u{60}"),
        format!("Here is the guide:\n{REPLY}\nHope it helps."),
    ] {
        let guide = parse_guide(&reply, &outline).expect("guide");
        assert_eq!(guide.steps[0].title, "Widen the store");
    }
    let bare = r#"[{"title":"Everything","summary":"All of it.","files":[0,1,2,3]}]"#;
    let guide = parse_guide(bare, &outline).expect("guide");
    assert_eq!(guide.overview, "");
    assert_eq!(guide.steps[0].body, "All of it.");
    assert_eq!(
        guide.steps[1].kind,
        StepKind::Mechanical,
        "nothing left over"
    );
}

#[test]
fn unknown_and_duplicate_numbers_are_dropped() {
    let outline = prompt().outline;
    let reply = r#"{"steps":[
      {"title":"First","explanation":"x","hunks":[1,1,99,-1,"x",2.5,"H2"],"files":[42,"F9"]},
      {"title":"Second","explanation":"y","hunks":[1,3]},
      {"title":"Ghost","explanation":"z","hunks":[77],"files":[400]}
    ]}"#;
    let guide = parse_guide(reply, &outline).expect("guide");
    let titles: Vec<&str> = guide.steps.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["First", "Second", OTHER_TITLE, MECHANICAL_TITLE]);
    assert_eq!(guide.steps[0].anchors.len(), 2, "H1 and H2 once each");
    assert_eq!(guide.steps[1].anchors.len(), 1, "H1 was already claimed");
    assert_eq!(guide.steps[1].anchors[0].path, "src/view.rs");
    let other: Vec<&str> = guide.steps[2]
        .files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(other, ["src/new_name.rs", "tests/store_test.rs"]);
}

#[test]
fn long_titles_and_bodies_are_clipped_and_steps_capped() {
    let outline = prompt().outline;
    let steps: Vec<String> = (0..20)
        .map(|i| {
            format!(
                r#"{{"title":"{}","explanation":"{}","hunks":[{}]}}"#,
                "t".repeat(200),
                "b ".repeat(800),
                i % 4
            )
        })
        .collect();
    let reply = format!(r#"{{"steps":[{}]}}"#, steps.join(","));
    let guide = parse_guide(&reply, &outline).expect("guide");
    let first = &guide.steps[0];
    assert_eq!(first.title.chars().count(), 81);
    assert!(first.title.ends_with('\u{2026}'));
    assert!(first.body.chars().count() <= 901);
    let model = guide
        .steps
        .iter()
        .filter(|s| s.kind == StepKind::Model)
        .count();
    assert!(model <= 4, "steps without unclaimed hunks drop out");
}

#[test]
fn unusable_replies_fail_with_readable_errors() {
    let outline = prompt().outline;
    let cases = [
        ("not json at all", "wasn\u{2019}t JSON"),
        (r#"{"steps":"nope"}"#, "had no steps"),
        (r#"{"overview":"only"}"#, "had no steps"),
        ("42", "had no steps"),
        (
            r#"{"steps":[{"title":"","hunks":[1]}]}"#,
            "pointed at the changed files",
        ),
        (
            r#"{"steps":[{"title":"Lost","hunks":[50],"files":[9]}]}"#,
            "pointed at the changed files",
        ),
        ("[]", "pointed at the changed files"),
        (
            r#"{"steps":[{"title":"Cut off","hunks":[1"#,
            "wasn\u{2019}t JSON",
        ),
    ];
    for (reply, expected) in cases {
        let error = parse_guide(reply, &outline).expect_err(reply);
        assert!(error.contains(expected), "{reply}: {error}");
    }
}

#[test]
fn drafted_steps_counts_titles_in_a_partial_reply() {
    assert_eq!(
        drafted_steps(r#"{"overview":"x","steps":[{"title":"a"},{"tit"#),
        1
    );
    assert_eq!(drafted_steps(REPLY), 2);
}
