use crate::core::forge::{ForgeRepo, valid_slug_part};
use crate::core::review::ReviewForge;

/// The repository a Bitbucket Cloud remote URL points at:
/// `git@bitbucket.org:ws/repo.git`, `ssh://git@bitbucket.org/ws/repo.git`,
/// `https://user@bitbucket.org/ws/repo.git`, `https://bitbucket.org/ws/repo`.
pub fn parse_remote(url: &str) -> Option<ForgeRepo> {
    let url = url.trim();
    let (scheme, rest) = match url.split_once("://") {
        Some((_, rest)) => (true, rest),
        None => (false, url),
    };
    let rest = rest.split_once('@').map_or(rest, |(_, rest)| rest);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.strip_prefix("bitbucket.org")?;
    let path = match path.strip_prefix(':') {
        Some(port) if scheme => port.trim_start_matches(|c: char| c.is_ascii_digit()),
        Some(path) => return slug(path),
        None => path,
    };
    slug(path.strip_prefix('/')?)
}

fn slug(path: &str) -> Option<ForgeRepo> {
    let path = path.trim_end_matches('/');
    let (workspace, repo) = path.split_once('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (valid_slug_part(workspace) && valid_slug_part(repo)).then(|| ForgeRepo {
        forge: ReviewForge::Bitbucket,
        owner: workspace.to_owned(),
        repo: repo.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_remote;

    #[test]
    fn parses_remote_urls() {
        for url in [
            "git@bitbucket.org:ws/repo.git",
            "git@bitbucket.org:ws/repo",
            "ssh://git@bitbucket.org/ws/repo.git",
            "ssh://git@bitbucket.org:22/ws/repo.git",
            "https://user@bitbucket.org/ws/repo.git",
            "https://bitbucket.org/ws/repo.git/",
            "https://bitbucket.org/ws/repo",
        ] {
            let repo = parse_remote(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!((repo.owner.as_str(), repo.repo.as_str()), ("ws", "repo"));
        }
        assert_eq!(parse_remote("git@github.com:ws/repo.git"), None);
        assert_eq!(parse_remote("https://bitbucket.org/ws"), None);
        assert_eq!(parse_remote("https://bitbucket.org/ws/repo/src/main"), None);
    }
}
