use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use quark_components::FuzzyMatcher;
use quark_ui::text_input::TextField;

use super::picker::expand_path;
use super::repo_picker::{short_parent, tilde};
use crate::core::forge::{
    ForgeRepo, parse_pull_request_reference, parse_remote, parse_repo_reference,
};
use crate::core::repo_discovery::{DiscoveredRepo, RepoScan};
use crate::core::review::{ReviewForge, ReviewTarget};
use crate::core::search::fuzzy::match_ranges;

pub const MAX_REPO_ROWS: usize = 8;
const ROOT_HINTS: usize = 3;
const NESTED_WORKTREES: usize = 3;

type Ranges = Vec<(usize, usize)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Typed,
    Repositories,
    PullRequests,
    More,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Self::Typed => "Typed",
            Self::Repositories => "Repositories",
            Self::PullRequests => "Pull requests",
            Self::More => "More",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneSource {
    pub repo: ForgeRepo,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherChoice {
    Repository(PathBuf),
    PullRequest { url: String, local: Option<PathBuf> },
    Path(PathBuf),
    PullRequests,
    OpenFolder,
    Clone(Option<CloneSource>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LauncherRow {
    pub section: Section,
    pub label: String,
    pub highlights: Ranges,
    pub branch: Option<String>,
    pub detail: Option<String>,
    pub choice: LauncherChoice,
    pub worktree: bool,
    pub nested: bool,
}

impl LauncherRow {
    fn item(
        section: Section,
        label: String,
        detail: Option<String>,
        choice: LauncherChoice,
    ) -> Self {
        Self {
            section,
            label,
            highlights: Vec::new(),
            branch: None,
            detail,
            choice,
            worktree: false,
            nested: false,
        }
    }
}

#[derive(Debug, Clone)]
struct RepoEntry {
    path: PathBuf,
    name: String,
    alias: String,
    location: String,
    branch: Option<String>,
    remote: Option<ForgeRepo>,
    worktrees: Vec<RepoEntry>,
}

impl RepoEntry {
    fn new(path: PathBuf, found: Option<&DiscoveredRepo>, home: Option<&Path>) -> Self {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            alias: tilde(&path, home),
            location: short_parent(&path, home),
            branch: found.and_then(|repo| repo.branch.clone()),
            remote: found
                .and_then(|repo| repo.remote.as_deref())
                .and_then(parse_remote),
            worktrees: found
                .map(|repo| {
                    repo.worktrees
                        .iter()
                        .map(|tree| RepoEntry::new(tree.path.clone(), Some(tree), home))
                        .collect()
                })
                .unwrap_or_default(),
            name,
            path,
        }
    }

    fn row(&self, highlights: Ranges, worktree: bool, nested: bool) -> LauncherRow {
        LauncherRow {
            section: Section::Repositories,
            label: self.name.clone(),
            highlights,
            branch: self.branch.clone(),
            detail: Some(if nested {
                self.location.clone()
            } else {
                self.alias.clone()
            }),
            choice: LauncherChoice::Repository(self.path.clone()),
            worktree,
            nested,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pick {
    repo: usize,
    worktree: Option<usize>,
}

#[derive(Debug)]
pub struct Launcher {
    pub query: TextField,
    pub rows: Vec<LauncherRow>,
    pub selected: usize,
    pub roots: Vec<String>,
    pub signed_in: bool,
    moved: bool,
    entries: Vec<RepoEntry>,
    matched: Vec<(Pick, Ranges)>,
    counts: HashMap<PathBuf, Option<usize>>,
    requested: HashSet<PathBuf>,
    capacity: usize,
    matcher: FuzzyMatcher,
    /// The forge a short `owner/repo#N` names, shared with the palette.
    short_forge: Rc<Cell<ReviewForge>>,
}

impl Default for Launcher {
    fn default() -> Self {
        let mut launcher = Self {
            query: TextField::new(""),
            rows: Vec::new(),
            selected: 0,
            roots: Vec::new(),
            signed_in: false,
            moved: false,
            entries: Vec::new(),
            matched: Vec::new(),
            counts: HashMap::new(),
            requested: HashSet::new(),
            capacity: MAX_REPO_ROWS,
            matcher: FuzzyMatcher::default(),
            short_forge: Rc::new(Cell::new(ReviewForge::GitHub)),
        };
        launcher.refilter();
        launcher
    }
}

impl Launcher {
    pub fn with_short_forge(short_forge: Rc<Cell<ReviewForge>>) -> Self {
        Self {
            short_forge,
            ..Self::default()
        }
    }

    pub fn reset(&mut self) {
        self.query.set_text("");
        self.counts.clear();
        self.requested.clear();
        self.refilter();
    }

    pub fn set_entries(&mut self, recent: Vec<PathBuf>, scan: Option<&RepoScan>) {
        let home = dirs::home_dir();
        let home = home.as_deref();
        let find = |path: &Path| scan.and_then(|scan| scan.find(path));
        let owners: HashMap<&Path, &Path> = scan
            .into_iter()
            .flat_map(|scan| scan.repos.iter().chain(&scan.known))
            .flat_map(|repo| {
                repo.worktrees
                    .iter()
                    .map(|tree| (tree.path.as_path(), repo.path.as_path()))
            })
            .collect();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut entries: Vec<RepoEntry> = recent
            .into_iter()
            .map(|path| {
                owners
                    .get(path.as_path())
                    .map_or(path, |owner| owner.to_path_buf())
            })
            .filter(|path| seen.insert(path.clone()))
            .map(|path| {
                let found = find(&path);
                RepoEntry::new(path, found, home)
            })
            .collect();
        if let Some(scan) = scan {
            let mut rest: Vec<&DiscoveredRepo> = scan
                .repos
                .iter()
                .chain(&scan.known)
                .filter(|repo| !seen.contains(&repo.path))
                .collect();
            rest.sort_by_key(|repo| Reverse(repo.active));
            entries.extend(
                rest.into_iter()
                    .map(|repo| RepoEntry::new(repo.path.clone(), Some(repo), home)),
            );
            let mut groups: HashMap<String, usize> = HashMap::new();
            for repo in &scan.repos {
                if let Some(parent) = repo.path.parent() {
                    *groups.entry(tilde(parent, home)).or_default() += 1;
                }
            }
            let mut groups: Vec<(String, usize)> = groups.into_iter().collect();
            groups.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            self.roots = groups
                .into_iter()
                .take(ROOT_HINTS)
                .map(|(group, _)| group)
                .collect();
        }
        self.entries = entries;
        self.refresh();
    }

    pub fn count(&self, path: &Path) -> Option<Option<usize>> {
        self.counts.get(path).copied()
    }

    pub fn set_count(&mut self, path: PathBuf, count: Option<usize>) -> bool {
        let shown = self
            .rows
            .iter()
            .any(|row| row.choice == LauncherChoice::Repository(path.clone()));
        self.counts.insert(path, count);
        shown
    }

    pub fn take_count_requests(&mut self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for row in &self.rows {
            if let LauncherChoice::Repository(path) = &row.choice
                && self.requested.insert(path.clone())
            {
                paths.push(path.clone());
            }
        }
        paths
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        let capacity = capacity.clamp(1, MAX_REPO_ROWS);
        if capacity != self.capacity {
            self.capacity = capacity;
            self.refresh();
        }
    }

    pub fn selected_choice(&self) -> Option<&LauncherChoice> {
        self.rows.get(self.selected).map(|row| &row.choice)
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as i32 - 1;
        self.selected = (self.selected as i32 + delta).clamp(0, last) as usize;
        self.moved = true;
    }

    pub fn find_local(&self, pr: &ReviewTarget) -> Option<&Path> {
        self.entries
            .iter()
            .find(|entry| {
                entry
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.matches(pr.forge, &pr.owner, &pr.repo))
            })
            .map(|entry| entry.path.as_path())
    }

    /// A typed pull request. A short `owner/repo#N` names one on the open
    /// repository's forge, or GitHub with none open, as in the palette.
    fn typed_pull_request(&self, query: &str) -> Option<ReviewTarget> {
        parse_pull_request_reference(query, self.short_forge.get())
    }

    pub fn refilter(&mut self) {
        let query = self.query.text().trim().to_owned();
        self.matched.clear();
        if query.is_empty() {
            self.matched = (0..self.entries.len().min(MAX_REPO_ROWS))
                .map(|repo| {
                    let pick = Pick {
                        repo,
                        worktree: None,
                    };
                    (pick, Vec::new())
                })
                .collect();
        } else {
            self.matcher.set_query(&query);
            let mut positions = Vec::new();
            let mut scored: Vec<(i32, usize, Pick, Ranges)> = Vec::new();
            let picks = self.entries.iter().enumerate().flat_map(|(repo, entry)| {
                std::iter::once((
                    Pick {
                        repo,
                        worktree: None,
                    },
                    entry,
                ))
                .chain(entry.worktrees.iter().enumerate().map(
                    move |(index, tree)| {
                        let pick = Pick {
                            repo,
                            worktree: Some(index),
                        };
                        (pick, tree)
                    },
                ))
            });
            for (order, (pick, entry)) in picks.enumerate() {
                positions.clear();
                if let Some(score) = self.matcher.score(&entry.name, &mut positions) {
                    scored.push((score, order, pick, match_ranges(&entry.name, &positions)));
                    continue;
                }
                positions.clear();
                if let Some(score) = self.matcher.score(&entry.alias, &mut positions) {
                    scored.push((score, order, pick, Vec::new()));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            self.matched = scored
                .into_iter()
                .take(MAX_REPO_ROWS)
                .map(|(_, _, pick, highlights)| (pick, highlights))
                .collect();
        }
        self.build(&query);
        self.selected = self.default_selection();
        self.moved = false;
    }

    fn refresh(&mut self) {
        let moved = self.moved;
        let keep = self.selected_choice().cloned().filter(|_| moved);
        self.refilter();
        if let Some(index) = keep.and_then(|keep| self.rows.iter().position(|r| r.choice == keep)) {
            self.selected = index;
            self.moved = true;
        }
    }

    fn build(&mut self, query: &str) {
        self.rows.clear();
        if let Some(pr) = self.typed_pull_request(query) {
            let reference = format!("{}/{}#{}", pr.owner, pr.repo, pr.number);
            let local = self.find_local(&pr).map(Path::to_path_buf);
            let home = dirs::home_dir();
            self.rows.push(LauncherRow::item(
                Section::Typed,
                match local {
                    Some(_) => format!("Open {reference}"),
                    None => format!("Locate clone for {reference}\u{2026}"),
                },
                local.as_deref().map(|path| tilde(path, home.as_deref())),
                LauncherChoice::PullRequest {
                    url: pr.web_url(),
                    local,
                },
            ));
        } else if looks_like_path(query) {
            self.rows.push(LauncherRow::item(
                Section::Typed,
                "Open this path".to_owned(),
                Some(query.to_owned()),
                LauncherChoice::Path(expand_path(query)),
            ));
        }
        let mut shown = 0;
        for (pick, highlights) in &self.matched {
            if shown >= self.capacity {
                break;
            }
            let repo = &self.entries[pick.repo];
            if let Some(index) = pick.worktree {
                self.rows
                    .push(repo.worktrees[index].row(highlights.clone(), true, false));
                shown += 1;
                continue;
            }
            self.rows.push(repo.row(highlights.clone(), false, false));
            shown += 1;
            if !query.is_empty() {
                continue;
            }
            for tree in repo.worktrees.iter().take(NESTED_WORKTREES) {
                if shown >= self.capacity {
                    break;
                }
                self.rows.push(tree.row(Vec::new(), true, true));
                shown += 1;
            }
        }
        let (label, detail) = pull_request_row(self.signed_in);
        self.rows.push(LauncherRow::item(
            Section::PullRequests,
            label.to_owned(),
            Some(detail.to_owned()),
            LauncherChoice::PullRequests,
        ));
        self.rows.push(LauncherRow::item(
            Section::More,
            "Open folder\u{2026}".to_owned(),
            None,
            LauncherChoice::OpenFolder,
        ));
        let source = clone_source(query);
        self.rows.push(LauncherRow::item(
            Section::More,
            match &source {
                Some(source) => format!("Clone {}\u{2026}", source.repo.slug()),
                None => "Clone from GitHub\u{2026}".to_owned(),
            },
            Some(match &source {
                Some(_) => "Choose a folder".to_owned(),
                None => "owner/repo or URL".to_owned(),
            }),
            LauncherChoice::Clone(source),
        ));
    }

    pub fn set_signed_in(&mut self, signed_in: bool) {
        if self.signed_in != signed_in {
            self.signed_in = signed_in;
            self.refresh();
        }
    }

    fn default_selection(&self) -> usize {
        let first = |section: Section| self.rows.iter().position(|row| row.section == section);
        first(Section::Typed)
            .or_else(|| first(Section::Repositories))
            .or_else(|| {
                self.rows
                    .iter()
                    .position(|row| matches!(row.choice, LauncherChoice::Clone(Some(_))))
            })
            .unwrap_or(0)
    }
}

pub fn pull_request_row(signed_in: bool) -> (&'static str, &'static str) {
    if signed_in {
        ("Your pull requests", "Open PRs you authored")
    } else {
        ("Sign in to see your PRs", "GitHub or Bitbucket")
    }
}

fn looks_like_path(query: &str) -> bool {
    query.starts_with(['/', '~', '.'])
}

fn clone_source(query: &str) -> Option<CloneSource> {
    let repo = parse_repo_reference(query)?;
    let url = if query.contains("://") || query.starts_with("git@") {
        query.to_owned()
    } else {
        repo.clone_url()
    };
    Some(CloneSource { repo, url })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(repos: &[(&str, Option<&str>)]) -> RepoScan {
        RepoScan {
            repos: repos
                .iter()
                .map(|(path, remote)| DiscoveredRepo {
                    path: PathBuf::from(path),
                    branch: Some("main".to_owned()),
                    active: None,
                    remote: remote.map(str::to_owned),
                    worktrees: Vec::new(),
                })
                .collect(),
            ..RepoScan::default()
        }
    }

    fn labels(launcher: &Launcher) -> Vec<&str> {
        launcher.rows.iter().map(|row| row.label.as_str()).collect()
    }

    #[test]
    fn recents_lead_and_typing_filters_and_offers_typed_rows() {
        let mut launcher = Launcher::default();
        launcher.set_entries(
            vec![PathBuf::from("/w/beta")],
            Some(&scan(&[
                ("/w/alpha", Some("git@github.com:owner/alpha.git")),
                ("/w/beta", None),
            ])),
        );
        assert_eq!(&labels(&launcher)[..2], ["beta", "alpha"]);
        assert_eq!(launcher.selected, 0);

        launcher.query.set_text("alp");
        launcher.refilter();
        assert_eq!(&labels(&launcher)[..1], ["alpha"]);

        launcher.query.set_text("Owner/Alpha#3");
        launcher.refilter();
        assert_eq!(
            launcher.selected_choice(),
            Some(&LauncherChoice::PullRequest {
                url: "https://github.com/Owner/Alpha/pull/3".to_owned(),
                local: Some(PathBuf::from("/w/alpha")),
            })
        );

        launcher.query.set_text("other/repo");
        launcher.refilter();
        assert!(matches!(
            launcher.selected_choice(),
            Some(LauncherChoice::Clone(Some(source))) if source.url == "https://github.com/other/repo.git"
        ));

        launcher.query.set_text("~/somewhere");
        launcher.refilter();
        assert_eq!(launcher.rows[0].label, "Open this path");
    }

    #[test]
    fn short_pull_request_references_follow_the_open_repository_forge() {
        let forge = Rc::new(Cell::new(ReviewForge::GitHub));
        let mut launcher = Launcher::with_short_forge(Rc::clone(&forge));
        launcher.set_entries(
            Vec::new(),
            Some(&scan(&[(
                "/w/widgets",
                Some("git@bitbucket.org:acme/widgets.git"),
            )])),
        );
        launcher.query.set_text("acme/widgets#7");
        launcher.refilter();
        assert_eq!(
            launcher.selected_choice(),
            Some(&LauncherChoice::PullRequest {
                url: "https://github.com/acme/widgets/pull/7".to_owned(),
                local: None,
            })
        );

        forge.set(ReviewForge::Bitbucket);
        launcher.refilter();
        assert_eq!(
            launcher.selected_choice(),
            Some(&LauncherChoice::PullRequest {
                url: "https://bitbucket.org/acme/widgets/pull-requests/7".to_owned(),
                local: Some(PathBuf::from("/w/widgets")),
            })
        );

        launcher
            .query
            .set_text("https://github.com/acme/widgets/pull/7");
        launcher.refilter();
        assert!(matches!(
            launcher.selected_choice(),
            Some(LauncherChoice::PullRequest { url, .. }) if url.starts_with("https://github.com/")
        ));
    }
}
