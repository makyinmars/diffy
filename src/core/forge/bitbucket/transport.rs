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

/// How requests authenticate. An Atlassian API token goes out as HTTP Basic
/// auth with the account email as the username.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    basic: Option<(String, String)>,
}

impl Credentials {
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// `email:api_token`, the form Diffy stores. RFC 7617 forbids `:` in the
    /// user part, so the first `:` separates the two.
    pub fn parse(secret: &str) -> Option<Self> {
        let (email, token) = secret.trim().split_once(':')?;
        let (email, token) = (email.trim(), token.trim());
        (!email.is_empty() && !token.is_empty()).then(|| Self {
            basic: Some((email.to_owned(), token.to_owned())),
        })
    }

    pub fn is_anonymous(&self) -> bool {
        self.basic.is_none()
    }

    fn key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.basic.hash(&mut hasher);
        hasher.finish()
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.basic.is_some() {
            "Credentials(basic)"
        } else {
            "Credentials(anonymous)"
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
    let credential = credentials.key();
    paused(credential, SystemTime::now())?;
    let body = body.map(Value::to_string);
    let (status, text) = http::block_on(async {
        let mut request = http::client()
            .request(method, url)
            .timeout(REQUEST_TIMEOUT)
            .header(USER_AGENT, AGENT)
            .header(ACCEPT, "application/json");
        if let Some((email, token)) = &credentials.basic {
            request = request.basic_auth(email, Some(token));
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
    if status.is_success() {
        Ok(text)
    } else {
        Err(failure(status, &text, context))
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

fn failure(status: StatusCode, body: &str, context: &str) -> DiffyError {
    let message = error_message(body).unwrap_or_else(|| body.trim().to_owned());
    let details = format!("{context} returned {status}: {message}");
    if status == StatusCode::UNAUTHORIZED {
        DiffyError::auth(details)
    } else {
        DiffyError::Network {
            details,
            retryable: http::status_is_retryable(status),
        }
    }
}

/// The message of a `{"type": "error", "error": {"message": ...}}` body.
fn error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    let message = error.get("message")?.as_str()?.trim();
    let detail = error
        .get("detail")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|detail| !detail.is_empty());
    Some(match detail {
        Some(detail) => format!("{message} ({detail})"),
        None => message.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn credentials_split_email_and_token_and_never_print_them() {
        let credentials = Credentials::parse(" me@example.com:ATATT3x:tail ").unwrap();
        assert_eq!(
            credentials.basic,
            Some(("me@example.com".to_owned(), "ATATT3x:tail".to_owned()))
        );
        assert_eq!(format!("{credentials:?}"), "Credentials(basic)");
        assert!(Credentials::parse("token-only").is_none());
        assert!(Credentials::parse(":token").is_none());
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
        let error = failure(StatusCode::UNAUTHORIZED, body, "Bitbucket user fetch");
        assert!(matches!(error, DiffyError::Auth { .. }));
        assert_eq!(
            error.to_string(),
            "Bitbucket user fetch returned 401 Unauthorized: Bad token (Expired)"
        );
        let error = failure(
            StatusCode::NOT_FOUND,
            "not json",
            "Bitbucket pull request fetch",
        );
        assert_eq!(
            error.to_string(),
            "Bitbucket pull request fetch returned 404 Not Found: not json"
        );
    }
}
