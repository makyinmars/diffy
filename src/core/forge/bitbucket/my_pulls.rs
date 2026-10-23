use serde_json::Value;

use super::api::{BitbucketApi, str_at};
use super::overview::reviewers;
use super::transport::{self, API_ROOT};
use crate::core::error::{DiffyError, Result};
use crate::core::forge::github::my_pulls::excerpt;
use crate::core::forge::model::MyPullRequest;
use crate::core::review::ReviewForge;

// Bitbucket lists pull requests per workspace; this bounds the requests one
// refresh makes for accounts in many workspaces.
const MAX_WORKSPACES: usize = 20;

impl BitbucketApi {
    /// Open pull requests the signed-in user authored, across their
    /// workspaces. Bitbucket has no cross-repository listing of pull
    /// requests to review, so those are not included.
    pub fn fetch_my_pull_requests(&self) -> Result<Vec<MyPullRequest>> {
        let credentials = self.credentials();
        if credentials.is_anonymous() {
            return Err(DiffyError::auth(
                "Bitbucket sign-in is required to list your pull requests",
            ));
        }
        let user = self.fetch_current_user()?;
        let workspaces = transport::get_pages(
            credentials,
            &format!("{API_ROOT}/user/workspaces?pagelen=100"),
            "Bitbucket workspaces fetch",
        )?;
        let mut slugs: Vec<String> = workspaces
            .iter()
            .map(|access| str_at(access, "/workspace/slug"))
            .filter(|slug| !slug.is_empty())
            .collect();
        if slugs.len() > MAX_WORKSPACES {
            tracing::warn!(
                workspaces = slugs.len(),
                "bitbucket: listing pull requests in the first {MAX_WORKSPACES} workspaces"
            );
            slugs.truncate(MAX_WORKSPACES);
        }
        let selected = user.uuid.replace('{', "%7B").replace('}', "%7D");
        let mut pulls = Vec::new();
        for slug in slugs {
            let url = format!(
                "{API_ROOT}/workspaces/{slug}/pullrequests/{selected}?state=OPEN&pagelen=50&fields=%2Bvalues.participants,%2Bvalues.reviewers"
            );
            match transport::get_pages(credentials, &url, "Bitbucket pull requests fetch") {
                Ok(values) => pulls.extend(values.iter().map(parse_pull)),
                Err(error) => tracing::warn!(workspace = slug, "bitbucket: {error}"),
            }
        }
        pulls.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(pulls)
    }
}

pub(super) fn parse_pull(value: &Value) -> MyPullRequest {
    let author = super::api::login(value.get("author"));
    let reviewers = reviewers(value, &author);
    let decision = if reviewers.iter().any(|r| r.state == "CHANGES_REQUESTED") {
        Some("CHANGES_REQUESTED")
    } else if reviewers.iter().any(|r| r.state == "APPROVED") {
        Some("APPROVED")
    } else {
        None
    };
    let repository = str_at(value, "/destination/repository/full_name");
    let number = value
        .get("id")
        .and_then(Value::as_i64)
        .and_then(|id| i32::try_from(id).ok())
        .unwrap_or_default();
    let body = Some(str_at(value, "/summary/raw"))
        .filter(|body| !body.is_empty())
        .unwrap_or_else(|| str_at(value, "/description"));
    MyPullRequest {
        forge: ReviewForge::Bitbucket,
        id: format!("bitbucket:{repository}#{number}"),
        number,
        title: str_at(value, "/title"),
        url: str_at(value, "/links/html/href"),
        repository,
        is_draft: value.get("draft").and_then(Value::as_bool).unwrap_or(false),
        updated_at: str_at(value, "/updated_on"),
        head_branch: str_at(value, "/source/branch/name"),
        base_branch: str_at(value, "/destination/branch/name"),
        review_decision: decision.map(ToOwned::to_owned),
        requested_reviewers: reviewers.iter().filter(|r| r.state == "REQUESTED").count(),
        reviewers,
        unresolved_threads: 0,
        excerpt: excerpt(&body),
        stats: None,
        checks: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn authored_pull_requests_map_reviewers_decisions_and_clone_keys() {
        let user =
            |nickname: &str| json!({ "nickname": nickname, "links": { "avatar": { "href": "" } } });
        let pull = parse_pull(&json!({
            "id": 5,
            "title": "Tidy the widget",
            "state": "OPEN",
            "draft": true,
            "author": user("ada"),
            "summary": { "raw": "Line one\n\nline two" },
            "updated_on": "2026-10-02T00:00:00+00:00",
            "source": { "branch": { "name": "feature" } },
            "destination": { "branch": { "name": "main" }, "repository": { "full_name": "Acme/Widgets" } },
            "links": { "html": { "href": "https://bitbucket.org/Acme/Widgets/pull-requests/5" } },
            "reviewers": [user("bob")],
            "participants": [{ "user": user("cy"), "role": "PARTICIPANT", "state": "approved" }]
        }));

        assert_eq!(pull.forge, ReviewForge::Bitbucket);
        assert_eq!(pull.id, "bitbucket:Acme/Widgets#5");
        assert_eq!((pull.number, pull.is_draft), (5, true));
        assert_eq!(pull.repo_key(), "bitbucket.org/acme/widgets");
        assert_eq!(pull.excerpt, "Line one line two");
        assert_eq!(pull.review_decision.as_deref(), Some("APPROVED"));
        assert_eq!(pull.requested_reviewers, 1);
        let reviewers: Vec<(&str, &str)> = pull
            .reviewers
            .iter()
            .map(|r| (r.actor.login.as_str(), r.state.as_str()))
            .collect();
        assert_eq!(reviewers, [("cy", "APPROVED"), ("bob", "REQUESTED")]);
    }
}
