pub mod github;
pub mod model;

use crate::core::review::ReviewTarget;

/// The forge and pull request a web URL points at.
pub fn parse_pull_request_url(url: &str) -> Option<ReviewTarget> {
    github::parse_pr_url(url).map(|pr| ReviewTarget::github(pr.owner, pr.repo, pr.number))
}
