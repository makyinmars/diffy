use crate::core::forge::{ForgeRepo, valid_slug_part};
use crate::core::review::ReviewForge;

/// Bitbucket Cloud's hosts: the web and HTTPS host, the host git over SSH
/// moved to in 2026 (`bitbucket.org` refuses SSH from November 2026), and
/// the SSH-over-port-443 fallback. Any other host is not Bitbucket Cloud.
const HOSTS: [&str; 4] = [
    "bitbucket.org",
    "www.bitbucket.org",
    "ssh.bitbucket.org",
    "altssh.bitbucket.org",
];

/// The repository a Bitbucket Cloud remote URL points at:
/// `git@bitbucket.org:ws/repo.git`, `git@ssh.bitbucket.org:ws/repo.git`,
/// `ssh://git@altssh.bitbucket.org:443/ws/repo.git`,
/// `https://user@bitbucket.org/ws/repo.git`, `https://bitbucket.org/ws/repo`.
pub fn parse_remote(url: &str) -> Option<ForgeRepo> {
    let (scheme, host, path) = split_host(url);
    if !is_cloud_host(host) {
        return None;
    }
    let path = match path.strip_prefix(':') {
        Some(port) if scheme => port.trim_start_matches(|c: char| c.is_ascii_digit()),
        Some(path) => return slug(path),
        None => path,
    };
    slug(path.strip_prefix('/')?)
}

/// Whether a URL, scp-style remote or bare `host/path` names a Bitbucket
/// Cloud host. `owner/repo` shorthand names no host.
pub fn names_cloud_host(input: &str) -> bool {
    let (_, host, _) = split_host(input);
    is_cloud_host(host)
}

fn is_cloud_host(host: &str) -> bool {
    HOSTS.iter().any(|known| host.eq_ignore_ascii_case(known))
}

/// Whether the input had a scheme, then the host and what follows it
/// (`:port/path`, `:path` or `/path`), with any `user@` dropped.
fn split_host(url: &str) -> (bool, &str, &str) {
    let url = url.trim();
    let (scheme, rest) = match url.split_once("://") {
        Some((_, rest)) => (true, rest),
        None => (false, url),
    };
    let rest = rest.split_once('@').map_or(rest, |(_, rest)| rest);
    let (host, path) = rest.split_at(rest.find([':', '/']).unwrap_or(rest.len()));
    (scheme, host, path)
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
            "git@ssh.bitbucket.org:ws/repo.git",
            "ssh://git@ssh.bitbucket.org/ws/repo.git",
            "ssh://git@altssh.bitbucket.org:443/ws/repo.git",
            "https://user@bitbucket.org/ws/repo.git",
            "https://x-bitbucket-api-token-auth@bitbucket.org/ws/repo.git",
            "https://bitbucket.org/ws/repo.git/",
            "https://bitbucket.org/ws/repo",
            "https://www.bitbucket.org/ws/repo",
            "HTTPS://Bitbucket.org/ws/repo",
        ] {
            let repo = parse_remote(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!((repo.owner.as_str(), repo.repo.as_str()), ("ws", "repo"));
        }
        for url in [
            "git@github.com:ws/repo.git",
            "https://bitbucket.org/ws",
            "https://bitbucket.org/ws/repo/src/main",
            "https://bitbucket.org.evil.example/ws/repo",
            "git@bitbucket.org-work:ws/repo.git",
            "https://bitbucket.example.com/scm/ws/repo.git",
            "ssh://git@bitbucket.example.com:7999/ws/repo.git",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
        }
    }
}
