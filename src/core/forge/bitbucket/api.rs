use std::sync::{Mutex, PoisonError};

use serde_json::Value;

use super::comments::{self, Viewer};
use super::transport::{self, API_ROOT, Credentials};
use crate::core::error::{DiffyError, Result};
use crate::core::forge::model::{ForgeUser, PullRequestInfo, PullRequestReviewData};
use crate::core::review::ReviewTarget;

/// The signed-in Bitbucket account. `uuid` decides which comments the viewer
/// may edit; nicknames are display names and need not be unique.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketUser {
    pub user: ForgeUser,
    pub uuid: String,
}

// Users already read per credential, so review refreshes do not re-read them.
static VIEWERS: Mutex<Vec<(u64, BitbucketUser)>> = Mutex::new(Vec::new());

#[derive(Debug, Clone, Default)]
pub struct BitbucketApi {
    credentials: Credentials,
}

impl BitbucketApi {
    pub fn new(credentials: Credentials) -> Self {
        Self { credentials }
    }

    pub(super) fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    pub fn fetch_current_user(&self) -> Result<BitbucketUser> {
        let value = transport::get_json(
            &self.credentials,
            &format!("{API_ROOT}/user"),
            "Bitbucket user fetch",
        )?;
        let user = parse_user(&value)?;
        let mut viewers = VIEWERS.lock().unwrap_or_else(PoisonError::into_inner);
        let key = self.credentials.key();
        viewers.retain(|(known, _)| *known != key);
        viewers.push((key, user.clone()));
        Ok(user)
    }

    fn viewer(&self) -> Option<BitbucketUser> {
        if self.credentials.is_anonymous() {
            return None;
        }
        let key = self.credentials.key();
        let cached = VIEWERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(known, _)| *known == key)
            .map(|(_, user)| user.clone());
        cached.or_else(|| match self.fetch_current_user() {
            Ok(user) => Some(user),
            Err(error) => {
                tracing::warn!("bitbucket: could not read the signed-in user: {error}");
                None
            }
        })
    }

    pub fn fetch_review_data(&self, pr: &ReviewTarget) -> Result<PullRequestReviewData> {
        let comments = transport::get_pages(
            &self.credentials,
            &pull_request_url(pr, "/comments?pagelen=100"),
            "Bitbucket comments fetch",
        )?;
        let viewer = self.viewer();
        Ok(PullRequestReviewData {
            threads: comments::threads(
                &comments,
                Viewer {
                    signed_in: !self.credentials.is_anonymous(),
                    uuid: viewer.as_ref().map(|viewer| viewer.uuid.as_str()),
                },
            ),
            ..PullRequestReviewData::default()
        })
    }

    pub fn fetch_pull_request(&self, pr: &ReviewTarget) -> Result<PullRequestInfo> {
        let value = self.pull_request_value(pr)?;
        let mut info = parse_pull_request(&value)?;
        self.add_diffstat(pr, &value, &mut info);
        Ok(info)
    }

    pub(super) fn pull_request_value(&self, pr: &ReviewTarget) -> Result<Value> {
        transport::get_json(
            &self.credentials,
            &pull_request_url(pr, ""),
            "Bitbucket pull request fetch",
        )
    }

    /// Fill the changed-file counts. They are a summary, so a diffstat that
    /// times out (Bitbucket answers 555) does not block the pull request.
    pub(super) fn add_diffstat(
        &self,
        pr: &ReviewTarget,
        value: &Value,
        info: &mut PullRequestInfo,
    ) {
        let diffstat = value
            .pointer("/links/diffstat/href")
            .and_then(Value::as_str)
            .map(|href| with_query(href, "pagelen=500"))
            .unwrap_or_else(|| pull_request_url(pr, "/diffstat?pagelen=500"));
        match transport::get_pages(&self.credentials, &diffstat, "Bitbucket diffstat fetch") {
            Ok(files) => apply_diffstat(info, &files),
            Err(error) => tracing::warn!("bitbucket: diffstat unavailable: {error}"),
        }
    }
}

pub(super) fn pull_request_url(pr: &ReviewTarget, path: &str) -> String {
    format!(
        "{API_ROOT}/repositories/{}/{}/pullrequests/{}{path}",
        pr.owner, pr.repo, pr.number
    )
}

fn with_query(url: &str, query: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{query}")
}

pub(super) fn parse_pull_request(value: &Value) -> Result<PullRequestInfo> {
    let state = str_at(value, "/state");
    let info = PullRequestInfo {
        title: str_at(value, "/title"),
        state: if state == "OPEN" { "open" } else { "closed" }.to_owned(),
        draft: value.get("draft").and_then(Value::as_bool).unwrap_or(false),
        merged: state == "MERGED",
        author_login: login(value.get("author")),
        number: value
            .get("id")
            .and_then(Value::as_i64)
            .and_then(|id| i32::try_from(id).ok())
            .unwrap_or_default(),
        additions: 0,
        deletions: 0,
        changed_files: 0,
        base_branch: str_at(value, "/destination/branch/name"),
        head_branch: str_at(value, "/source/branch/name"),
        base_sha: str_at(value, "/destination/commit/hash"),
        head_sha: str_at(value, "/source/commit/hash"),
        base_repo_url: clone_url(value.pointer("/destination/repository/full_name")),
        head_repo_url: clone_url(value.pointer("/source/repository/full_name")),
    };
    if info.base_branch.is_empty() || info.head_branch.is_empty() {
        return Err(DiffyError::Parse(
            "failed to parse Bitbucket pull request response".to_owned(),
        ));
    }
    Ok(info)
}

pub(super) fn apply_diffstat(info: &mut PullRequestInfo, files: &[Value]) {
    let sum = |key: &str| {
        files
            .iter()
            .filter_map(|file| file.get(key).and_then(Value::as_i64))
            .sum::<i64>()
            .clamp(0, i64::from(i32::MAX)) as i32
    };
    info.additions = sum("lines_added");
    info.deletions = sum("lines_removed");
    info.changed_files = i32::try_from(files.len()).unwrap_or(i32::MAX);
}

pub(super) fn parse_user(value: &Value) -> Result<BitbucketUser> {
    let login = login(Some(value));
    if login.is_empty() {
        return Err(DiffyError::Parse(
            "missing account in Bitbucket user response".to_owned(),
        ));
    }
    let name = str_at(value, "/display_name");
    Ok(BitbucketUser {
        user: ForgeUser {
            name: if name.is_empty() { login.clone() } else { name },
            avatar_url: str_at(value, "/links/avatar/href"),
            login,
        },
        uuid: str_at(value, "/uuid"),
    })
}

/// The name Diffy shows as `@login` for a Bitbucket account: its nickname,
/// else the account id, else the display name.
pub(super) fn login(account: Option<&Value>) -> String {
    let Some(account) = account.filter(|account| !account.is_null()) else {
        return String::new();
    };
    ["/nickname", "/account_id", "/display_name"]
        .into_iter()
        .map(|pointer| str_at(account, pointer))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

fn clone_url(full_name: Option<&Value>) -> String {
    full_name
        .and_then(Value::as_str)
        .filter(|name| name.contains('/'))
        .map(|name| format!("https://bitbucket.org/{name}.git"))
        .unwrap_or_default()
}

pub(super) fn str_at(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn pull_request() -> Value {
        json!({
            "type": "pullrequest",
            "id": 42,
            "title": "Tidy the widget",
            "state": "OPEN",
            "draft": false,
            "author": {
                "type": "user",
                "display_name": "Ada Lovelace",
                "nickname": "ada",
                "account_id": "557058:ada",
                "links": { "avatar": { "href": "https://avatar.example/ada/128" } }
            },
            "source": {
                "branch": { "name": "feature/widget" },
                "commit": { "hash": "1a2b3c4d5e6f" },
                "repository": { "full_name": "fork-owner/widgets" }
            },
            "destination": {
                "branch": { "name": "main" },
                "commit": { "hash": "abcdefabcdef" },
                "repository": { "full_name": "acme/widgets" }
            },
            "links": {
                "diffstat": {
                    "href": "https://api.bitbucket.org/2.0/repositories/acme/widgets/diffstat/fork-owner/widgets:1a2b3c4d5e6f%0Dabcdefabcdef?from_pullrequest_id=42&topic=true"
                }
            }
        })
    }

    #[test]
    fn pull_request_maps_branches_shas_and_fork_clone_urls() {
        let info = parse_pull_request(&pull_request()).unwrap();
        assert_eq!(info.title, "Tidy the widget");
        assert_eq!(
            (info.state.as_str(), info.merged, info.draft),
            ("open", false, false)
        );
        assert_eq!(info.author_login, "ada");
        assert_eq!(info.number, 42);
        assert_eq!(
            (info.base_branch.as_str(), info.head_branch.as_str()),
            ("main", "feature/widget")
        );
        assert_eq!(
            (info.base_sha.as_str(), info.head_sha.as_str()),
            ("abcdefabcdef", "1a2b3c4d5e6f")
        );
        assert_eq!(info.base_repo_url, "https://bitbucket.org/acme/widgets.git");
        assert_eq!(
            info.head_repo_url,
            "https://bitbucket.org/fork-owner/widgets.git"
        );
    }

    #[test]
    fn merged_and_declined_pull_requests_are_closed() {
        let mut value = pull_request();
        value["state"] = json!("MERGED");
        let merged = parse_pull_request(&value).unwrap();
        assert_eq!((merged.state.as_str(), merged.merged), ("closed", true));
        value["state"] = json!("DECLINED");
        let declined = parse_pull_request(&value).unwrap();
        assert_eq!(
            (declined.state.as_str(), declined.merged),
            ("closed", false)
        );
    }

    #[test]
    fn a_deleted_fork_leaves_the_head_repository_empty() {
        let mut value = pull_request();
        value["source"]["repository"] = Value::Null;
        let info = parse_pull_request(&value).unwrap();
        assert_eq!(info.head_repo_url, "");
        value["source"]["branch"] = Value::Null;
        assert!(parse_pull_request(&value).is_err());
    }

    #[test]
    fn diffstat_sums_lines_and_counts_files() {
        let mut info = parse_pull_request(&pull_request()).unwrap();
        apply_diffstat(
            &mut info,
            &[
                json!({ "status": "modified", "lines_added": 3, "lines_removed": 1 }),
                json!({ "status": "added", "lines_added": 10, "lines_removed": 0 }),
                json!({ "status": "removed", "lines_added": 0, "lines_removed": 4 }),
            ],
        );
        assert_eq!(
            (info.additions, info.deletions, info.changed_files),
            (13, 5, 3)
        );
    }

    #[test]
    fn current_user_maps_nickname_display_name_and_avatar() {
        let user = parse_user(&json!({
            "type": "user",
            "display_name": "Ada Lovelace",
            "nickname": "ada",
            "account_id": "557058:ada",
            "uuid": "{d301aafa-0000-4000-8000-000000000001}",
            "links": { "avatar": { "href": "https://avatar.example/ada/128" } }
        }))
        .unwrap();
        assert_eq!(
            user.user,
            ForgeUser {
                login: "ada".to_owned(),
                name: "Ada Lovelace".to_owned(),
                avatar_url: "https://avatar.example/ada/128".to_owned(),
            }
        );
        assert_eq!(user.uuid, "{d301aafa-0000-4000-8000-000000000001}");
        assert!(parse_user(&json!({ "type": "user" })).is_err());
    }

    #[test]
    fn logins_prefer_nickname_then_account_id() {
        assert_eq!(
            login(Some(&json!({ "nickname": "ada", "account_id": "1" }))),
            "ada"
        );
        assert_eq!(
            login(Some(
                &json!({ "account_id": "557058:x", "display_name": "X" })
            )),
            "557058:x"
        );
        assert_eq!(login(Some(&json!({ "display_name": "Team" }))), "Team");
        assert_eq!(login(Some(&Value::Null)), "");
        assert_eq!(login(None), "");
    }

    #[test]
    fn diffstat_links_gain_a_page_length() {
        assert_eq!(
            with_query("https://x/diffstat/a..b", "pagelen=500"),
            "https://x/diffstat/a..b?pagelen=500"
        );
        assert_eq!(
            with_query("https://x/diffstat?topic=true", "pagelen=500"),
            "https://x/diffstat?topic=true&pagelen=500"
        );
    }
}
