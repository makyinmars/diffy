//! Guided review: builds the prompt from a compare's changed files and
//! hunks, and turns the model's JSON reply into validated guide steps.
//!
//! The model only sees numbers this module assigns (`F3` for a file, `H12`
//! for a hunk) and answers with them. Line numbers come from the diff, never
//! from the model, so a reply can only point at changes that exist.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::Value;

use super::rank::extract_json;
use crate::contracts::guide::{
    ChangedFile, GuideAnchor, GuideFile, GuideStep, HunkSide, HunkSpot, StepKind,
};
use crate::core::file_rank;

pub const GUIDE_MAX_TOKENS: u32 = 32_000;
pub const MAX_GUIDE_BYTES: usize = 96_000;
pub const MAX_GUIDE_FILES: usize = 400;
pub const MECHANICAL_TITLE: &str = "Mechanical and generated";
pub const OTHER_TITLE: &str = "Other changes";

const MAX_DESCRIPTION_BYTES: usize = 4_000;
const MAX_HUNK_BYTES: usize = 3_000;
const LONG_LINE_BYTES: usize = 256;
const MAX_MECHANICAL_LINES: usize = 200;
const HUNK_LABEL_BYTES: usize = 8;
const MORE_FILES_BYTES: usize = 40;
const FILES_HEADER: &str = "\nFiles (number, status, +added -removed, path):\n";
const HUNKS_HEADER: &str = "\nHunks:\n";
const MAX_STEPS: usize = 12;
const MAX_TITLE_CHARS: usize = 80;
const MAX_BODY_CHARS: usize = 900;
const MAX_OVERVIEW_CHARS: usize = 600;

const PROMPT: &str = "You are guiding a reviewer through a code change they have not seen. Split the change into a few ordered steps that tell its story, so the reviewer can read the diff one step at a time.

Reply with only a JSON object, with no prose and no code fence:
{\"overview\":\"...\",\"steps\":[{\"title\":\"...\",\"explanation\":\"...\",\"hunks\":[0,1],\"files\":[2]}]}

- overview: one or two sentences on what the change does and why.
- steps: 1 to 8 steps in reading order. Put foundations first (data model, contracts, core logic), then the code that uses them, then tests. A small change can be one step.
- title: at most 8 words.
- explanation: two to four sentences on what changed in the step and why it matters to a reviewer. Name risks and behavior changes.
- hunks: the H numbers of the hunks that make up the step, in the order to read them.
- files: the F numbers of whole files that belong to the step. Use it for files whose hunks are not shown.
- Use only the F and H numbers listed below. Put every F file in a step. Never invent file names or line numbers.
";

/// One hunk of a file's unified diff, as git printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchHunk {
    pub header: String,
    pub lines: Vec<String>,
    pub spot: HunkSpot,
}

/// What the prompt numbered, so a reply can be mapped back to the diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outline {
    files: Vec<OutlineFile>,
    indexed: usize,
    hunks: Vec<(usize, usize)>,
    mechanical: Vec<GuideFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OutlineFile {
    path: String,
    spots: Vec<HunkSpot>,
}

impl Outline {
    pub fn sent_hunks(&self) -> usize {
        self.hunks.len()
    }

    pub fn indexed_files(&self) -> usize {
        self.indexed
    }

    pub fn needs_model(&self) -> bool {
        !self.files.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidePrompt {
    pub text: String,
    pub outline: Outline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedGuide {
    pub overview: String,
    pub steps: Vec<GuideStep>,
}

/// Why a file is listed by name instead of read, or `None` when it is
/// worth reviewing line by line.
pub fn mechanical_reason(file: &ChangedFile) -> Option<&'static str> {
    if let Some(reason) = file_rank::noise_reason(&file.path, file.binary) {
        return Some(reason);
    }
    match (file.status, file.stats) {
        ('D', _) => Some("deleted"),
        ('R', Some((0, 0))) => Some("renamed without edits"),
        ('C', Some((0, 0))) => Some("copied without edits"),
        (_, Some((0, 0))) => Some("no line changes"),
        _ => None,
    }
}

/// Indexes of the reviewable files in reading order: central and source
/// code first, then config, tests and docs.
pub fn reviewable_order(files: &[ChangedFile]) -> Vec<usize> {
    let candidates: Vec<usize> = (0..files.len())
        .filter(|&i| mechanical_reason(&files[i]).is_none())
        .collect();
    let tiers: Vec<(file_rank::Tier, &str)> = candidates
        .iter()
        .map(|&i| {
            let file = &files[i];
            (file_rank::tier(&file.path, file.binary), file.path.as_str())
        })
        .collect();
    file_rank::heuristic_order(&tiers)
        .into_iter()
        .map(|i| candidates[i as usize])
        .collect()
}

/// Splits git's unified diff into files and hunks. Paths are the new path,
/// or the old one for deletions.
pub fn parse_patch(text: &str) -> Vec<(String, Vec<PatchHunk>)> {
    let mut files: Vec<(String, Vec<PatchHunk>)> = Vec::new();
    let mut header: Option<FileHeader> = None;
    let mut hunk: Option<OpenHunk> = None;
    for line in text.lines() {
        if let Some(open) = hunk.as_mut()
            && open.wants(line)
        {
            open.push(line);
            continue;
        }
        if let Some(open) = hunk.take()
            && let Some(file) = files.last_mut()
        {
            file.1.push(open.finish());
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            header = Some(FileHeader::from_git_line(rest));
            files.push((
                header.as_ref().map(FileHeader::path).unwrap_or_default(),
                Vec::new(),
            ));
            continue;
        }
        let Some(current) = header.as_mut() else {
            continue;
        };
        if let Some(path) = line.strip_prefix("--- ") {
            current.old = side_path(path, "a/");
        } else if let Some(path) = line.strip_prefix("+++ ") {
            current.new = side_path(path, "b/");
        } else if let Some(path) = line.strip_prefix("rename to ") {
            current.new = Some(unquote(path));
        } else if line.starts_with("@@") {
            hunk = OpenHunk::start(line);
        } else {
            continue;
        }
        if let Some(file) = files.last_mut() {
            file.0 = current.path();
        }
    }
    if let Some(open) = hunk
        && let Some(file) = files.last_mut()
    {
        file.1.push(open.finish());
    }
    files.retain(|(path, _)| !path.is_empty());
    files
}

#[derive(Debug, Default)]
struct FileHeader {
    old: Option<String>,
    new: Option<String>,
}

impl FileHeader {
    fn from_git_line(rest: &str) -> Self {
        let (old, new) = rest.split_once(" b/").unwrap_or((rest, ""));
        Self {
            old: Some(unquote(old.strip_prefix("a/").unwrap_or(old))),
            new: (!new.is_empty()).then(|| unquote(new)),
        }
    }

    fn path(&self) -> String {
        self.new
            .clone()
            .or_else(|| self.old.clone())
            .unwrap_or_default()
    }
}

fn side_path(path: &str, prefix: &str) -> Option<String> {
    let path = path.trim_end_matches('\t');
    if path == "/dev/null" {
        return None;
    }
    let path = unquote(path);
    Some(path.strip_prefix(prefix).unwrap_or(&path).to_owned())
}

fn unquote(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return path.to_owned();
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut bytes = inner.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        match bytes.next() {
            Some(first @ b'0'..=b'3') => {
                let mut value = first - b'0';
                for _ in 0..2 {
                    let Some(&next @ b'0'..=b'7') = bytes.peek() else {
                        break;
                    };
                    bytes.next();
                    value = value * 8 + next - b'0';
                }
                out.push(value);
            }
            Some(b'a') => out.push(7),
            Some(b'b') => out.push(8),
            Some(b't') => out.push(b'\t'),
            Some(b'n') => out.push(b'\n'),
            Some(b'v') => out.push(11),
            Some(b'f') => out.push(12),
            Some(b'r') => out.push(b'\r'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct OpenHunk {
    header: String,
    lines: Vec<String>,
    old_left: u32,
    new_left: u32,
    old_line: u32,
    new_line: u32,
    spot: Option<HunkSpot>,
}

impl OpenHunk {
    fn start(header: &str) -> Option<Self> {
        let ranges = header.strip_prefix("@@ ")?;
        let (ranges, _) = ranges.split_once(" @@")?;
        let (old, new) = ranges.split_once(' ')?;
        let (old_start, old_len) = parse_range(old.strip_prefix('-')?)?;
        let (new_start, new_len) = parse_range(new.strip_prefix('+')?)?;
        Some(Self {
            header: header.to_owned(),
            lines: Vec::new(),
            old_left: old_len,
            new_left: new_len,
            old_line: old_start,
            new_line: new_start,
            spot: None,
        })
    }

    fn wants(&self, line: &str) -> bool {
        if line.starts_with('\\') {
            return true;
        }
        match line.as_bytes().first() {
            Some(b'+') => self.new_left > 0,
            Some(b'-') => self.old_left > 0,
            Some(b' ') | None => self.old_left > 0 && self.new_left > 0,
            _ => false,
        }
    }

    fn push(&mut self, line: &str) {
        let at = |side, line: u32| HunkSpot {
            side,
            line: line.saturating_sub(1),
        };
        match line.as_bytes().first() {
            Some(b'+') => {
                self.spot.get_or_insert(at(HunkSide::New, self.new_line));
                self.new_line += 1;
                self.new_left -= 1;
            }
            Some(b'-') => {
                self.spot.get_or_insert(at(HunkSide::Old, self.old_line));
                self.old_line += 1;
                self.old_left -= 1;
            }
            Some(b'\\') => {}
            _ => {
                self.old_line += 1;
                self.new_line += 1;
                self.old_left -= 1;
                self.new_left -= 1;
            }
        }
        self.lines.push(line.to_owned());
    }

    fn finish(self) -> PatchHunk {
        let spot = self.spot.unwrap_or(HunkSpot {
            side: HunkSide::New,
            line: self.new_line.saturating_sub(1),
        });
        PatchHunk {
            header: self.header,
            lines: self.lines,
            spot,
        }
    }
}

fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

/// Builds the prompt under [`MAX_GUIDE_BYTES`]. Files go in
/// [`reviewable_order`]; hunks fill the budget in two passes, every file's
/// first hunk before any file's second, so a large change still shows each
/// file. Mechanical files are named but never read.
pub fn build_prompt(
    title: Option<&str>,
    description: Option<&str>,
    files: &[ChangedFile],
    patches: &HashMap<String, Vec<PatchHunk>>,
) -> GuidePrompt {
    let order = reviewable_order(files);
    let mut text = String::from(PROMPT);
    write_context(&mut text, title, description);

    let mut indexed = 0;
    let mut listing_bytes = 0;
    let mut line = String::new();
    for &i in order.iter().take(MAX_GUIDE_FILES) {
        line.clear();
        write_file_line(&mut line, indexed, &files[i], false);
        if listing_bytes + line.len() > MAX_GUIDE_BYTES / 2 {
            break;
        }
        listing_bytes += line.len();
        indexed += 1;
    }
    let mechanical: Vec<GuideFile> = files
        .iter()
        .filter_map(|file| {
            mechanical_reason(file).map(|reason| GuideFile {
                path: file.path.clone(),
                note: Some(reason.to_owned()),
            })
        })
        .collect();
    let named = mechanical_section(&mechanical);
    let rendered: Vec<Vec<String>> = order[..indexed]
        .iter()
        .map(|&i| {
            patches
                .get(&files[i].path)
                .map(|hunks| hunks.iter().map(render_hunk).collect())
                .unwrap_or_default()
        })
        .collect();

    let fixed = text.len()
        + FILES_HEADER.len()
        + listing_bytes
        + MORE_FILES_BYTES
        + named.len()
        + HUNKS_HEADER.len();
    let mut left = MAX_GUIDE_BYTES.saturating_sub(fixed);
    let mut sent: Vec<Vec<bool>> = rendered.iter().map(|h| vec![false; h.len()]).collect();
    let take = |file: usize, hunk: usize, left: &mut usize, sent: &mut [Vec<bool>]| {
        let header = if sent[file].contains(&true) {
            0
        } else {
            files[order[file]].path.len() + HUNK_LABEL_BYTES
        };
        let cost = rendered[file][hunk].len() + HUNK_LABEL_BYTES + header;
        if cost <= *left {
            *left -= cost;
            sent[file][hunk] = true;
        }
    };
    for (file, bodies) in rendered.iter().enumerate() {
        if !bodies.is_empty() {
            take(file, 0, &mut left, &mut sent);
        }
    }
    for (file, bodies) in rendered.iter().enumerate() {
        for hunk in 1..bodies.len() {
            take(file, hunk, &mut left, &mut sent);
        }
    }

    text.push_str(FILES_HEADER);
    for (number, &i) in order[..indexed].iter().enumerate() {
        write_file_line(&mut text, number, &files[i], sent[number].contains(&true));
    }
    if order.len() > indexed {
        let _ = writeln!(text, "{} more files are not listed.", order.len() - indexed);
    }
    text.push_str(&named);

    let mut hunks = Vec::new();
    for (file, bodies) in rendered.iter().enumerate() {
        let mut first = true;
        for (hunk, body) in bodies.iter().enumerate() {
            if !sent[file][hunk] {
                continue;
            }
            if hunks.is_empty() {
                text.push_str(HUNKS_HEADER);
            }
            if first {
                let _ = writeln!(text, "\nF{file} {}", files[order[file]].path);
                first = false;
            }
            let _ = write!(text, "H{} {body}", hunks.len());
            hunks.push((file, hunk));
        }
    }

    let outline_files = order
        .iter()
        .map(|&i| OutlineFile {
            path: files[i].path.clone(),
            spots: patches
                .get(&files[i].path)
                .map(|hunks| hunks.iter().map(|hunk| hunk.spot).collect())
                .unwrap_or_default(),
        })
        .collect();
    GuidePrompt {
        text,
        outline: Outline {
            files: outline_files,
            indexed,
            hunks,
            mechanical,
        },
    }
}

fn mechanical_section(mechanical: &[GuideFile]) -> String {
    let mut named = String::new();
    if mechanical.is_empty() {
        return named;
    }
    named.push_str("\nListed by name only, not part of any step:\n");
    for file in mechanical.iter().take(MAX_MECHANICAL_LINES) {
        let _ = writeln!(
            named,
            "- {} ({})",
            file.path,
            file.note.as_deref().unwrap_or_default()
        );
    }
    if mechanical.len() > MAX_MECHANICAL_LINES {
        let _ = writeln!(
            named,
            "- and {} more",
            mechanical.len() - MAX_MECHANICAL_LINES
        );
    }
    named
}

fn write_context(text: &mut String, title: Option<&str>, description: Option<&str>) {
    if let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) {
        let _ = write!(text, "\nTitle: {title}\n");
    }
    if let Some(description) = description.map(str::trim).filter(|d| !d.is_empty()) {
        let cut = floor_char_boundary(description, MAX_DESCRIPTION_BYTES);
        let _ = write!(text, "\nDescription:\n{}\n", &description[..cut]);
        if cut < description.len() {
            text.push_str("[description truncated]\n");
        }
    }
}

fn write_file_line(out: &mut String, number: usize, file: &ChangedFile, shown: bool) {
    let _ = write!(out, "F{number} {} ", file.status);
    match file.stats {
        Some((added, removed)) => {
            let _ = write!(out, "+{added} -{removed} ");
        }
        None => out.push_str("? "),
    }
    if let Some(old) = file.old_path.as_deref().filter(|old| *old != file.path) {
        let _ = write!(out, "{old} -> ");
    }
    out.push_str(&file.path);
    if !shown {
        out.push_str(" (hunks not shown)");
    }
    out.push('\n');
}

fn render_hunk(hunk: &PatchHunk) -> String {
    let mut out = String::new();
    out.push_str(&hunk.header);
    out.push('\n');
    for (at, line) in hunk.lines.iter().enumerate() {
        if out.len() >= MAX_HUNK_BYTES {
            let _ = writeln!(out, "[... {} more lines]", hunk.lines.len() - at);
            break;
        }
        if line.len() > LONG_LINE_BYTES {
            out.push_str(&line[..floor_char_boundary(line, LONG_LINE_BYTES)]);
            out.push_str("...[truncated]");
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut at = index;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Validates a reply against what the prompt numbered. Unknown and
/// duplicate numbers are dropped, files nobody placed land in a trailing
/// "Other changes" step, and mechanical files always close the guide.
pub fn parse_guide(reply: &str, outline: &Outline) -> Result<ParsedGuide, String> {
    let value = extract_json(reply)
        .ok_or_else(|| "The model\u{2019}s reply wasn\u{2019}t JSON.".to_owned())?;
    let (overview, items) = match &value {
        Value::Object(map) => (
            map.get("overview")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            map.get("steps").and_then(Value::as_array),
        ),
        Value::Array(items) => ("", Some(items)),
        _ => ("", None),
    };
    let items = items.ok_or_else(|| "The model\u{2019}s reply had no steps.".to_owned())?;

    let mut claimed = vec![false; outline.files.iter().map(|f| f.spots.len()).sum()];
    let offsets: Vec<usize> = outline
        .files
        .iter()
        .scan(0, |at, file| {
            let start = *at;
            *at += file.spots.len();
            Some(start)
        })
        .collect();
    let slot = |file: usize, hunk: usize| offsets[file] + hunk;

    struct Draft {
        title: String,
        body: String,
        hunks: Vec<(usize, usize)>,
        files: Vec<usize>,
    }
    let mut drafts: Vec<Draft> = Vec::new();
    for item in items {
        if drafts.len() == MAX_STEPS {
            break;
        }
        let Some(map) = item.as_object() else {
            continue;
        };
        let title = clip(map.get("title").and_then(Value::as_str), MAX_TITLE_CHARS);
        if title.is_empty() {
            continue;
        }
        let body = ["explanation", "body", "summary"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str));
        let mut draft = Draft {
            title,
            body: clip(body, MAX_BODY_CHARS),
            hunks: Vec::new(),
            files: Vec::new(),
        };
        for number in numbers(map.get("hunks"), 'H') {
            let Some(&(file, hunk)) = outline.hunks.get(number) else {
                continue;
            };
            let at = slot(file, hunk);
            if !claimed[at] {
                claimed[at] = true;
                draft.hunks.push((file, hunk));
            }
        }
        for number in numbers(map.get("files"), 'F') {
            if number < outline.indexed && !draft.files.contains(&number) {
                draft.files.push(number);
            }
        }
        drafts.push(draft);
    }
    for draft in &mut drafts {
        for &file in &draft.files {
            for hunk in 0..outline.files[file].spots.len() {
                let at = slot(file, hunk);
                if !claimed[at] {
                    claimed[at] = true;
                    draft.hunks.push((file, hunk));
                }
            }
        }
    }
    let placed: Vec<bool> = (0..outline.files.len())
        .map(|file| {
            drafts.iter().any(|draft| {
                draft.files.contains(&file) || draft.hunks.iter().any(|(f, _)| *f == file)
            })
        })
        .collect();
    for (file, spots) in outline.files.iter().map(|f| f.spots.len()).enumerate() {
        if !placed[file] {
            continue;
        }
        for hunk in 0..spots {
            let at = slot(file, hunk);
            if claimed[at] {
                continue;
            }
            claimed[at] = true;
            if let Some(draft) = drafts
                .iter_mut()
                .find(|d| d.files.contains(&file) || d.hunks.iter().any(|(f, _)| *f == file))
            {
                draft.hunks.push((file, hunk));
            }
        }
    }

    let anchor = |file: usize, hunk: Option<usize>| GuideAnchor {
        path: outline.files[file].path.clone(),
        hunk: hunk.map(|hunk| outline.files[file].spots[hunk]),
    };
    let mut steps: Vec<GuideStep> = Vec::new();
    for draft in drafts {
        let mut anchors: Vec<GuideAnchor> = draft
            .hunks
            .iter()
            .map(|&(file, hunk)| anchor(file, Some(hunk)))
            .collect();
        let mut touched: Vec<usize> = Vec::new();
        for &(file, _) in &draft.hunks {
            if !touched.contains(&file) {
                touched.push(file);
            }
        }
        for &file in &draft.files {
            if !touched.contains(&file) {
                touched.push(file);
                if outline.files[file].spots.is_empty() {
                    anchors.push(anchor(file, None));
                }
            }
        }
        if anchors.is_empty() {
            continue;
        }
        steps.push(GuideStep {
            title: draft.title,
            body: draft.body,
            kind: StepKind::Model,
            files: touched
                .iter()
                .map(|&file| GuideFile {
                    path: outline.files[file].path.clone(),
                    note: None,
                })
                .collect(),
            anchors,
        });
    }
    if steps.is_empty() {
        return Err("None of the model\u{2019}s steps pointed at the changed files.".to_owned());
    }

    let rest: Vec<usize> = (0..outline.files.len()).filter(|&f| !placed[f]).collect();
    if !rest.is_empty() {
        steps.push(GuideStep {
            title: OTHER_TITLE.to_owned(),
            body: "The guide didn\u{2019}t place these files in a step.".to_owned(),
            kind: StepKind::Other,
            files: rest
                .iter()
                .map(|&file| GuideFile {
                    path: outline.files[file].path.clone(),
                    note: None,
                })
                .collect(),
            anchors: rest
                .iter()
                .flat_map(|&file| {
                    let spots = outline.files[file].spots.len();
                    let hunks: Vec<Option<usize>> = if spots == 0 {
                        vec![None]
                    } else {
                        (0..spots).map(Some).collect()
                    };
                    hunks.into_iter().map(move |hunk| (file, hunk))
                })
                .map(|(file, hunk)| anchor(file, hunk))
                .collect(),
        });
    }
    if let Some(step) = mechanical_step(outline) {
        steps.push(step);
    }
    Ok(ParsedGuide {
        overview: clip(Some(overview), MAX_OVERVIEW_CHARS),
        steps,
    })
}

fn mechanical_step(outline: &Outline) -> Option<GuideStep> {
    if outline.mechanical.is_empty() {
        return None;
    }
    Some(GuideStep {
        title: MECHANICAL_TITLE.to_owned(),
        body: "Listed by name only. Their contents weren\u{2019}t sent to the model.".to_owned(),
        kind: StepKind::Mechanical,
        files: outline.mechanical.clone(),
        anchors: outline
            .mechanical
            .iter()
            .map(|file| GuideAnchor {
                path: file.path.clone(),
                hunk: None,
            })
            .collect(),
    })
}

pub fn mechanical_guide(outline: &Outline) -> Result<ParsedGuide, String> {
    let step = mechanical_step(outline).ok_or("There are no changed files to guide.")?;
    Ok(ParsedGuide {
        overview: "This change contains only mechanical or generated files. No model was called."
            .to_owned(),
        steps: vec![step],
    })
}

fn numbers(value: Option<&Value>, prefix: char) -> Vec<usize> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => {
                let text = text.trim();
                text.strip_prefix(prefix)
                    .or_else(|| text.strip_prefix(prefix.to_ascii_lowercase()))
                    .unwrap_or(text)
                    .parse()
                    .ok()
            }
            _ => None,
        })
        .filter_map(|number| usize::try_from(number).ok())
        .collect()
}

fn clip(text: Option<&str>, max_chars: usize) -> String {
    let text = text
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}\u{2026}", text[..at].trim_end()),
        None => text,
    }
}

/// Counts the steps a partial reply has drafted so far, for progress.
pub fn drafted_steps(partial: &str) -> usize {
    partial.matches("\"title\"").count()
}

#[cfg(test)]
mod tests;
