use crate::core::forge::valid_slug_part;
use crate::core::review::{ReviewForge, ReviewTarget};

/// `https://bitbucket.org/{workspace}/{repo}/pull-requests/{id}`, with any
/// trailing tab path (`/diff`, `/overview`), query or fragment.
pub fn parse_pr_url(url: &str) -> Option<ReviewTarget> {
    let trimmed = url.trim();
    let path = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let path = path
        .strip_prefix("www.bitbucket.org/")
        .or_else(|| path.strip_prefix("bitbucket.org/"))?;
    let path = path.split(['?', '#']).next()?;
    let mut parts = path.split('/');
    let workspace = parts.next()?;
    let repo = parts.next()?;
    let tab = parts.next()?;
    let number = parts.next()?;
    if tab != "pull-requests" || !valid_slug_part(workspace) || !valid_slug_part(repo) {
        return None;
    }
    Some(ReviewTarget {
        forge: ReviewForge::Bitbucket,
        owner: workspace.to_owned(),
        repo: repo.to_owned(),
        number: number.parse().ok().filter(|n: &i32| *n > 0)?,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_pr_url;
    use crate::core::review::{ReviewForge, ReviewTarget};

    fn target(number: i32) -> Option<ReviewTarget> {
        Some(ReviewTarget {
            forge: ReviewForge::Bitbucket,
            owner: "atlassian".to_owned(),
            repo: "python-bitbucket".to_owned(),
            number,
        })
    }

    #[test]
    fn parses_pull_request_urls_with_tabs_queries_and_fragments() {
        for url in [
            "https://bitbucket.org/atlassian/python-bitbucket/pull-requests/42",
            "https://bitbucket.org/atlassian/python-bitbucket/pull-requests/42/",
            "https://bitbucket.org/atlassian/python-bitbucket/pull-requests/42/diff",
            "https://bitbucket.org/atlassian/python-bitbucket/pull-requests/42/fix-it/diff",
            "https://bitbucket.org/atlassian/python-bitbucket/pull-requests/42?w=1",
            "https://bitbucket.org/atlassian/python-bitbucket/pull-requests/42#comment-7",
            "http://www.bitbucket.org/atlassian/python-bitbucket/pull-requests/42",
            "  bitbucket.org/atlassian/python-bitbucket/pull-requests/42  ",
        ] {
            assert_eq!(parse_pr_url(url), target(42), "{url}");
        }
    }

    #[test]
    fn rejects_other_bitbucket_pages_and_hosts() {
        for url in [
            "https://bitbucket.org/atlassian/python-bitbucket/pull-requests",
            "https://bitbucket.org/atlassian/python-bitbucket/pull-requests/0",
            "https://bitbucket.org/atlassian/python-bitbucket/pull-requests/abc",
            "https://bitbucket.org/atlassian/python-bitbucket/issues/42",
            "https://bitbucket.org/atlassian/python-bitbucket/src/main/README.md",
            "https://github.com/atlassian/python-bitbucket/pull/42",
            "https://bitbucket.example.com/projects/A/repos/b/pull-requests/42",
            "git@bitbucket.org:atlassian/python-bitbucket.git",
        ] {
            assert_eq!(parse_pr_url(url), None, "{url}");
        }
    }
}
