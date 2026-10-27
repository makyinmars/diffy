use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde_json::Value;

use crate::core::error::{DiffyError, Result};
use crate::core::http;

pub(super) const API_ROOT: &str = "https://api.bitbucket.org/2.0";
const AGENT: &str = "diffy/0.1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
// Bitbucket answers 429 without Retry-After or reset headers.
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);
// Pages are followed through `next`; this bounds a runaway listing.
const MAX_PAGES: usize = 50;

/// How requests authenticate with an Atlassian API token: HTTP Basic auth
/// with the account email as the username, or (since August 2026) the token
/// alone as a Bearer token.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Credentials {
    auth: Option<Auth>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Auth {
    Basic { email: String, token: String },
    Bearer(String),
}

impl Credentials {
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// `email:api_token` or a bare `api_token`, the forms Diffy stores.
    /// RFC 7617 forbids `:` in the user part and API tokens hold neither `:`
    /// nor `@`, so a `:` marks Basic auth. Bitbucket only accepts the
    /// Atlassian account email there: a username before the `:` (the old
    /// app password form) is refused.
    pub fn parse(secret: &str) -> Option<Self> {
        let secret = secret.trim();
        if secret.is_empty() {
            return None;
        }
        let auth = match secret.split_once(':') {
            Some((email, token)) => {
                let (email, token) = (email.trim(), token.trim());
                if !email.contains('@') || token.is_empty() {
                    return None;
                }
                Auth::Basic {
                    email: email.to_owned(),
                    token: token.to_owned(),
                }
            }
            None => Auth::Bearer(secret.to_owned()),
        };
        Some(Self { auth: Some(auth) })
    }

    pub fn is_anonymous(&self) -> bool {
        self.auth.is_none()
    }

    pub(super) fn key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.auth.hash(&mut hasher);
        hasher.finish()
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.auth {
            Some(Auth::Basic { .. }) => "Credentials(basic)",
            Some(Auth::Bearer(_)) => "Credentials(bearer)",
            None => "Credentials(anonymous)",
        })
    }
}

struct Pause {
    credential: u64,
    until: SystemTime,
}

static PAUSES: Mutex<Vec<Pause>> = Mutex::new(Vec::new());

pub(super) fn get_json(credentials: &Credentials, url: &str, context: &str) -> Result<Value> {
    let body = send(credentials, Method::GET, url, None, context)?;
    serde_json::from_str(&body).map_err(Into::into)
}

/// Every `values` entry of a paginated listing, following `next` links.
pub(super) fn get_pages(credentials: &Credentials, url: &str, context: &str) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    let mut next = Some(url.to_owned());
    let mut pages = 0;
    while let Some(url) = next.take() {
        if pages == MAX_PAGES {
            tracing::warn!(context, pages, "bitbucket: stopped following pages");
            break;
        }
        pages += 1;
        let page = get_json(credentials, &url, context)?;
        next = next_page(&page);
        if let Some(Value::Array(page_values)) = page.get("values") {
            values.extend(page_values.iter().cloned());
        }
    }
    Ok(values)
}

/// Send a JSON body (or none) and return the response text.
pub(super) fn send(
    credentials: &Credentials,
    method: Method,
    url: &str,
    body: Option<&Value>,
    context: &str,
) -> Result<String> {
    send_accepting(credentials, method, url, body, context, &[])
}

/// Like [`send`], also treating the `accepted` failure statuses as done
/// (resolving an already resolved thread answers 409).
pub(super) fn send_accepting(
    credentials: &Credentials,
    method: Method,
    url: &str,
    body: Option<&Value>,
    context: &str,
    accepted: &[StatusCode],
) -> Result<String> {
    let credential = credentials.key();
    paused(credential, SystemTime::now())?;
    let body = body.map(Value::to_string);
    let (status, text) = http::block_on(async {
        let mut request = http::client()
            .request(method, url)
            .timeout(REQUEST_TIMEOUT)
            .header(USER_AGENT, AGENT)
            .header(ACCEPT, "application/json");
        match &credentials.auth {
            Some(Auth::Basic { email, token }) => {
                request = request.basic_auth(email, Some(token));
            }
            Some(Auth::Bearer(token)) => request = request.bearer_auth(token),
            None => {}
        }
        request = match body {
            Some(body) => request.header(CONTENT_TYPE, "application/json").body(body),
            // Bodiless POSTs (approve, resolve) are refused without a length.
            None => request.header(CONTENT_LENGTH, "0"),
        };
        let response = request
            .send()
            .await
            .map_err(|error| DiffyError::network(format!("{context} failed: {error}")))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| DiffyError::network(format!("{context} read failed: {error}")))?;
        Ok((status, text))
    })?;
    settle(credential, status, SystemTime::now())?;
    if status.is_success() || accepted.contains(&status) {
        Ok(text)
    } else {
        Err(failure(status, &text, context, credentials.is_anonymous()))
    }
}

pub(super) fn next_page(page: &Value) -> Option<String> {
    page.get("next")
        .and_then(Value::as_str)
        .filter(|next| !next.is_empty())
        .map(ToOwned::to_owned)
}

fn settle(credential: u64, status: StatusCode, now: SystemTime) -> Result<()> {
    let mut pauses = PAUSES.lock().unwrap_or_else(PoisonError::into_inner);
    pauses.retain(|pause| pause.credential != credential);
    if status == StatusCode::TOO_MANY_REQUESTS {
        let until = now + RATE_LIMIT_COOLDOWN;
        pauses.push(Pause { credential, until });
        return Err(rate_limit_error(until, now));
    }
    Ok(())
}

fn paused(credential: u64, now: SystemTime) -> Result<()> {
    let pauses = PAUSES.lock().unwrap_or_else(PoisonError::into_inner);
    match pauses.iter().find(|pause| pause.credential == credential) {
        Some(pause) if pause.until > now => Err(rate_limit_error(pause.until, now)),
        _ => Ok(()),
    }
}

fn rate_limit_error(until: SystemTime, now: SystemTime) -> DiffyError {
    let minutes = until
        .duration_since(now)
        .unwrap_or_default()
        .as_secs()
        .div_ceil(60)
        .max(1);
    DiffyError::network_fatal(format!(
        "Bitbucket rate limit reached. Try again in {minutes} min."
    ))
}

fn failure(status: StatusCode, body: &str, context: &str, anonymous: bool) -> DiffyError {
    let details = match error_message(body).or_else(|| plain_message(body)) {
        Some(message) => format!("{context} returned {status}: {message}"),
        None => format!("{context} returned {status}."),
    };
    if status != StatusCode::UNAUTHORIZED {
        return DiffyError::Network {
            details,
            retryable: http::status_is_retryable(status),
        };
    }
    // API tokens expire within a year, so a stored token stops working.
    let hint = if anonymous {
        "Sign in to Bitbucket to continue."
    } else {
        "Bitbucket rejected the API token, which may be wrong or expired. Sign in again with a valid token."
    };
    let separator = if details.ends_with('.') { " " } else { ". " };
    DiffyError::auth(format!("{details}{separator}{hint}"))
}

/// The message of a `{"type": "error", "error": {"message": ...}}` body,
/// with its detail: a string, the scopes a token lacks
/// (`{"required": [...], "granted": [...]}`), or per-field problems.
fn error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error").unwrap_or(&value);
    let message = error.get("message")?.as_str()?.trim();
    let detail = match error.get("detail") {
        Some(Value::String(detail)) => Some(detail.trim().to_owned()),
        Some(detail) => detail
            .get("required")
            .and_then(Value::as_array)
            .map(|scopes| {
                let scopes: Vec<&str> = scopes.iter().filter_map(Value::as_str).collect();
                format!("needs {}", scopes.join(", "))
            }),
        None => None,
    }
    .or_else(|| {
        let fields = error.get("fields")?.as_object()?;
        let problems: Vec<String> = fields
            .iter()
            .map(|(field, problem)| match problem {
                Value::Array(items) => {
                    let items: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
                    format!("{field}: {}", items.join(" "))
                }
                other => format!("{field}: {}", other.as_str().unwrap_or_default()),
            })
            .collect();
        Some(problems.join("; "))
    })
    .filter(|detail| !detail.is_empty());
    Some(match detail {
        Some(detail) => format!("{message} ({detail})"),
        None => message.to_owned(),
    })
}

/// A short plain-text body; HTML error pages and long bodies are left out
/// of the message.
fn plain_message(body: &str) -> Option<String> {
    let body = body.trim();
    (!body.is_empty() && !body.starts_with('<') && body.len() <= 200 && !body.contains('\n'))
        .then(|| body.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn credentials_split_email_and_token_and_never_print_them() {
        let credentials = Credentials::parse(" me@example.com:ATATT3x:tail ").unwrap();
        assert!(matches!(
            &credentials.auth,
            Some(Auth::Basic { email, token }) if email == "me@example.com" && token == "ATATT3x:tail"
        ));
        assert_eq!(format!("{credentials:?}"), "Credentials(basic)");

        let bearer = Credentials::parse("ATATT3xToken=").unwrap();
        assert!(matches!(&bearer.auth, Some(Auth::Bearer(token)) if token == "ATATT3xToken="));
        assert_eq!(format!("{bearer:?}"), "Credentials(bearer)");

        assert!(Credentials::parse("me@example.com:").is_none());
        // Bitbucket refuses usernames with API tokens; only the email works.
        assert!(Credentials::parse("maky:ATATT3xToken").is_none());
        assert!(Credentials::parse("  ").is_none());
        assert!(Credentials::anonymous().is_anonymous());
    }

    #[test]
    fn pages_follow_next_until_it_is_missing() {
        assert_eq!(
            next_page(&json!({ "next": "https://api.bitbucket.org/2.0/x?page=2", "values": [] })),
            Some("https://api.bitbucket.org/2.0/x?page=2".to_owned())
        );
        assert_eq!(next_page(&json!({ "values": [] })), None);
        assert_eq!(next_page(&json!({ "next": "" })), None);
    }

    #[test]
    fn rate_limits_pause_only_that_credential() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let credential = 0xb17b_0c4e;
        assert!(settle(credential, StatusCode::TOO_MANY_REQUESTS, now).is_err());
        let error = paused(credential, now + Duration::from_secs(30)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Bitbucket rate limit reached. Try again in 1 min."
        );
        assert!(paused(credential + 1, now).is_ok());
        assert!(paused(credential, now + RATE_LIMIT_COOLDOWN).is_ok());
        assert!(settle(credential, StatusCode::OK, now).is_ok());
        assert!(paused(credential, now).is_ok());
    }

    #[test]
    fn errors_carry_bitbucket_messages_and_401_is_an_auth_error() {
        let body = r#"{"type": "error", "error": {"message": "Bad token", "detail": "Expired"}}"#;
        let error = failure(
            StatusCode::UNAUTHORIZED,
            body,
            "Bitbucket user fetch",
            false,
        );
        assert!(matches!(error, DiffyError::Auth { .. }));
        assert_eq!(
            error.to_string(),
            "Bitbucket user fetch returned 401 Unauthorized: Bad token (Expired). Bitbucket rejected the API token, which may be wrong or expired. Sign in again with a valid token."
        );
        let error = failure(
            StatusCode::NOT_FOUND,
            "not json",
            "Bitbucket pull request fetch",
            false,
        );
        assert_eq!(
            error.to_string(),
            "Bitbucket pull request fetch returned 404 Not Found: not json"
        );
    }

    /// Bodies shaped like the ones Bitbucket documents: an empty 401 to an
    /// anonymous request, a 403 naming the scopes a token lacks, a 400 with
    /// field errors, a removed endpoint's 404, and an HTML error page that
    /// must not reach a toast.
    #[test]
    fn error_bodies_become_short_readable_messages() {
        let anonymous = failure(StatusCode::UNAUTHORIZED, "", "Bitbucket user fetch", true);
        assert_eq!(
            anonymous.to_string(),
            "Bitbucket user fetch returned 401 Unauthorized. Sign in to Bitbucket to continue."
        );

        let scopes = r#"{"type": "error", "error": {"message": "Your credentials lack one or more required privilege scopes.", "detail": {"required": ["read:user:bitbucket"], "granted": ["read:pullrequest:bitbucket"]}}}"#;
        assert_eq!(
            failure(StatusCode::FORBIDDEN, scopes, "Bitbucket user fetch", false).to_string(),
            "Bitbucket user fetch returned 403 Forbidden: Your credentials lack one or more required privilege scopes. (needs read:user:bitbucket)"
        );

        let fields = r#"{"type": "error", "error": {"message": "Bad request", "fields": {"content": ["This field is required."]}}}"#;
        assert_eq!(
            failure(
                StatusCode::BAD_REQUEST,
                fields,
                "Bitbucket comment create",
                false
            )
            .to_string(),
            "Bitbucket comment create returned 400 Bad Request: Bad request (content: This field is required.)"
        );

        let removed =
            r#"{"message": "Resource not found", "detail": "There is no API hosted at this URL."}"#;
        assert_eq!(
            failure(
                StatusCode::NOT_FOUND,
                removed,
                "Bitbucket workspaces fetch",
                false
            )
            .to_string(),
            "Bitbucket workspaces fetch returned 404 Not Found: Resource not found (There is no API hosted at this URL.)"
        );

        let html = "<!DOCTYPE html>\n<html><body>Something went wrong</body></html>";
        assert_eq!(
            failure(
                StatusCode::BAD_GATEWAY,
                html,
                "Bitbucket diffstat fetch",
                false
            )
            .to_string(),
            "Bitbucket diffstat fetch returned 502 Bad Gateway."
        );
    }
}
