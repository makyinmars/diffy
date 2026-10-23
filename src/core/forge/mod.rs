pub mod bitbucket;
pub mod github;
pub mod model;

use crate::core::review::{ReviewForge, ReviewTarget};

/// A repository on a forge: a GitHub `owner/repo` or a Bitbucket
/// `workspace/repo_slug`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForgeRepo {
    pub forge: ReviewForge,
    pub owner: String,
    pub repo: String,
}

impl ForgeRepo {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    pub fn clone_url(&self) -> String {
        let host = match self.forge {
            ReviewForge::GitHub => "github.com",
            ReviewForge::Bitbucket => "bitbucket.org",
        };
        format!("https://{host}/{}/{}.git", self.owner, self.repo)
    }

    pub fn matches(&self, forge: ReviewForge, owner: &str, repo: &str) -> bool {
        self.forge == forge
            && self.owner.eq_ignore_ascii_case(owner)
            && self.repo.eq_ignore_ascii_case(repo)
    }
}

/// The lowercase key Diffy saves a repository's local clone under: GitHub
/// keeps its `owner/repo`; other forges prefix their host.
pub fn clone_key(forge: ReviewForge, slug: &str) -> String {
    let slug = slug.to_ascii_lowercase();
    match forge {
        ReviewForge::GitHub => slug,
        ReviewForge::Bitbucket => format!("bitbucket.org/{slug}"),
    }
}

/// The forge repository a git remote URL points at.
pub fn parse_remote(url: &str) -> Option<ForgeRepo> {
    github::parse_remote(url).or_else(|| bitbucket::parse_remote(url))
}

/// A repository typed as `owner/repo` (GitHub) or pasted as a forge URL.
pub fn parse_repo_reference(input: &str) -> Option<ForgeRepo> {
    if input.contains("bitbucket.org") {
        return bitbucket::parse_remote(input);
    }
    github::parse_repo_reference(input)
}

/// The forge and pull request a web URL points at.
pub fn parse_pull_request_url(url: &str) -> Option<ReviewTarget> {
    github::parse_pr_url(url)
        .map(|pr| ReviewTarget::github(pr.owner, pr.repo, pr.number))
        .or_else(|| bitbucket::parse_pr_url(url))
}

/// A pull request URL, or `owner/repo#N` on `short_forge` (the open
/// repository's forge, GitHub when unknown).
pub fn parse_pull_request_reference(input: &str, short_forge: ReviewForge) -> Option<ReviewTarget> {
    parse_pull_request_url(input).or_else(|| parse_short_reference(input, short_forge))
}

fn parse_short_reference(input: &str, forge: ReviewForge) -> Option<ReviewTarget> {
    let (slug, number) = input.trim().split_once('#')?;
    let (owner, repo) = slug.split_once('/')?;
    if !valid_slug_part(owner) || !valid_slug_part(repo) {
        return None;
    }
    Some(ReviewTarget {
        forge,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number: number.parse().ok().filter(|n: &i32| *n > 0)?,
    })
}

pub(crate) fn valid_slug_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_pr_reference(input: &str) -> Option<ReviewTarget> {
        parse_pull_request_reference(input, ReviewForge::GitHub)
    }

    #[test]
    fn parses_short_references_and_file_tabs() {
        let short = parse_pr_reference("owner/repo#12").unwrap();
        assert_eq!(
            (short.owner.as_str(), short.repo.as_str(), short.number),
            ("owner", "repo", 12)
        );
        let tab = parse_pr_reference("www.github.com/owner/repo/pull/7/files").unwrap();
        assert_eq!(tab.web_url(), "https://github.com/owner/repo/pull/7");
        assert!(parse_pr_reference("owner/repo#0").is_none());
        assert!(parse_pr_reference("fix the #12 bug").is_none());
        assert!(parse_pr_reference("owner/repo").is_none());
    }

    #[test]
    fn urls_name_their_forge_and_short_references_follow_the_open_repository() {
        let github = parse_pull_request_reference(
            "https://github.com/owner/repo/pull/3",
            ReviewForge::Bitbucket,
        )
        .unwrap();
        assert_eq!(github, ReviewTarget::github("owner", "repo", 3));

        let bitbucket = parse_pull_request_reference(
            "https://bitbucket.org/ws/repo/pull-requests/9/diff",
            ReviewForge::GitHub,
        )
        .unwrap();
        assert_eq!(bitbucket.forge, ReviewForge::Bitbucket);
        assert_eq!(
            bitbucket.web_url(),
            "https://bitbucket.org/ws/repo/pull-requests/9"
        );

        assert_eq!(
            parse_pull_request_reference("ws/repo#9", ReviewForge::Bitbucket).map(|pr| pr.forge),
            Some(ReviewForge::Bitbucket)
        );
        assert_eq!(
            parse_pull_request_reference("ws/repo#9", ReviewForge::GitHub).map(|pr| pr.forge),
            Some(ReviewForge::GitHub)
        );
    }

    #[test]
    fn remotes_and_typed_repositories_dispatch_by_host() {
        let bitbucket = parse_remote("git@bitbucket.org:ws/repo.git").unwrap();
        assert_eq!(bitbucket.forge, ReviewForge::Bitbucket);
        assert_eq!(bitbucket.clone_url(), "https://bitbucket.org/ws/repo.git");
        let github = parse_remote("git@github.com:owner/repo.git").unwrap();
        assert_eq!(github.clone_url(), "https://github.com/owner/repo.git");
        assert_eq!(
            parse_repo_reference("https://bitbucket.org/ws/repo").map(|repo| repo.forge),
            Some(ReviewForge::Bitbucket)
        );
        assert_eq!(
            parse_repo_reference("owner/repo").map(|repo| repo.forge),
            Some(ReviewForge::GitHub)
        );
    }
}
