//! Minimal Linode API v4 client: bearer auth, pagination, rate-limit retry, and error
//! bodies surfaced as readable messages. Shared by every `linode.*` handler.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use reqwest::header::{ACCEPT, HeaderMap, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

pub const DEFAULT_BASE_URL: &str = "https://api.linode.com/v4";
const USER_AGENT: &str = concat!("ifx/", env!("CARGO_PKG_VERSION"));
const MAX_ATTEMPTS: u32 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const PAGE_SIZE: u32 = 100;

/// One entry of a Linode error body (`{"errors": [{"reason": ..., "field": ...}]}`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct FieldError {
    pub reason: String,
    #[serde(default)]
    pub field: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("no Linode token: use CREDENTIALS_DIRECTORY/linode-token or LINODE_TOKEN")]
    MissingToken,
    #[error("reading Linode credential file: {source}")]
    CredentialRead {
        #[source]
        source: std::io::Error,
    },
    #[error("Linode credential must be nonempty ASCII without whitespace")]
    InvalidCredential,
    /// Non-2xx response. `message` is built from `errors[].field`/`reason` when present.
    #[error("{method} {path}: HTTP {status}: {message}")]
    Api {
        method: Method,
        path: String,
        status: StatusCode,
        errors: Vec<FieldError>,
        message: String,
    },
    #[error("{method} {path}: gave up after {attempts} attempts (last status {status})")]
    Exhausted {
        method: Method,
        path: String,
        status: StatusCode,
        attempts: u32,
    },
    #[error("{method} {path}: {source}")]
    Transport {
        method: Method,
        path: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{what}: timed out after {}s (last: {last})", .after.as_secs())]
    Timeout {
        what: String,
        after: Duration,
        last: String,
    },
}

impl ApiError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, ApiError::Api { status, .. } if *status == StatusCode::NOT_FOUND)
    }
}

/// True when `e` is (or wraps) a 404 from the API.
pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>()
        .is_some_and(ApiError::is_not_found)
}

/// Linode API client. Cheap to construct; the token and HTTP client are resolved on
/// first use so a stack that never touches Linode never needs `LINODE_TOKEN`.
#[derive(Debug)]
pub struct Linode {
    base_url: String,
    token: Option<String>,
    http: OnceLock<Result<reqwest::Client, String>>,
    poll_interval: Duration,
    wait_timeout: Duration,
    retry_backoff: Duration,
}

impl Linode {
    /// Client configured from the environment: `CREDENTIALS_DIRECTORY/linode-token`
    /// (authoritative when the directory is set), otherwise `LINODE_TOKEN`, read lazily.
    /// `LINODE_API_URL` (default [`DEFAULT_BASE_URL`]), `LINODE_POLL_INTERVAL_MS`
    /// (default 5000) and `LINODE_WAIT_TIMEOUT_SECS` (default 600).
    pub fn from_env() -> Self {
        let base = std::env::var("LINODE_API_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        let mut c = Self::new(base, None);
        if let Some(ms) = env_u64("LINODE_POLL_INTERVAL_MS") {
            c.poll_interval = Duration::from_millis(ms);
        }
        if let Some(s) = env_u64("LINODE_WAIT_TIMEOUT_SECS") {
            c.wait_timeout = Duration::from_secs(s);
        }
        c
    }

    /// Client for an explicit endpoint and token (tests, alternate endpoints).
    pub fn with_base_url(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::new(base_url.into(), Some(token.into()))
    }

    fn new(base_url: String, token: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            http: OnceLock::new(),
            poll_interval: Duration::from_secs(5),
            wait_timeout: Duration::from_secs(600),
            retry_backoff: Duration::from_secs(1),
        }
    }

    /// How often `GET` polls run while waiting for a resource to settle.
    pub fn with_poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }

    /// How long to wait for a resource to settle (provisioning, resize, deletion).
    pub fn with_wait_timeout(mut self, d: Duration) -> Self {
        self.wait_timeout = d;
        self
    }

    /// Initial delay before retrying a 429/5xx without a `Retry-After` header.
    pub fn with_retry_backoff(mut self, d: Duration) -> Self {
        self.retry_backoff = d;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    pub fn wait_timeout(&self) -> Duration {
        self.wait_timeout
    }

    fn token(&self) -> Result<String, ApiError> {
        if let Some(t) = &self.token {
            return Ok(t.clone());
        }
        let directory = std::env::var_os("CREDENTIALS_DIRECTORY");
        resolve_token(directory.as_deref().map(Path::new), || {
            std::env::var("LINODE_TOKEN").ok()
        })
    }

    fn http(&self) -> anyhow::Result<&reqwest::Client> {
        self.http
            .get_or_init(|| {
                reqwest::Client::builder()
                    .user_agent(USER_AGENT)
                    .timeout(Duration::from_secs(60))
                    .build()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| anyhow!("building http client: {e}"))
    }

    /// One request with retry on 429 / 502 / 503 / 504. Returns the parsed JSON body
    /// (an empty object for empty bodies).
    /// `query` values are appended verbatim (callers pass numbers only).
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        filter: Option<&Value>,
        query: &[(&str, String)],
    ) -> anyhow::Result<Value> {
        let token = self.token()?;
        let http = self.http()?;
        let mut url = format!("{}{}", self.base_url, path);
        if !query.is_empty() {
            let q: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            url.push('?');
            url.push_str(&q.join("&"));
        }
        let transport = |source| ApiError::Transport {
            method: method.clone(),
            path: path.into(),
            source,
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = http
                .request(method.clone(), &url)
                .bearer_auth(&token)
                .header(ACCEPT, "application/json");
            if let Some(f) = filter {
                req = req.header("X-Filter", f.to_string());
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(transport)?;
            let status = resp.status();
            if is_retryable(status) {
                if attempt >= MAX_ATTEMPTS {
                    return Err(ApiError::Exhausted {
                        method,
                        path: path.into(),
                        status,
                        attempts: attempt,
                    }
                    .into());
                }
                let delay = retry_after(resp.headers())
                    .unwrap_or_else(|| self.retry_backoff.saturating_mul(1 << (attempt - 1)))
                    .min(MAX_BACKOFF);
                tracing::warn!(%method, path, %status, ?delay, "linode: retrying");
                tokio::time::sleep(delay).await;
                continue;
            }
            let text = resp.text().await.map_err(transport)?;
            if !status.is_success() {
                let errors = parse_errors(&text);
                let message = if errors.is_empty() {
                    let snippet: String = text.chars().take(200).collect();
                    if snippet.trim().is_empty() {
                        status.to_string()
                    } else {
                        snippet
                    }
                } else {
                    errors
                        .iter()
                        .map(|e| match &e.field {
                            Some(f) => format!("{f}: {}", e.reason),
                            None => e.reason.clone(),
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                };
                return Err(ApiError::Api {
                    method,
                    path: path.into(),
                    status,
                    errors,
                    message,
                }
                .into());
            }
            if text.trim().is_empty() {
                return Ok(json!({}));
            }
            return serde_json::from_str(&text)
                .with_context(|| format!("{method} {path}: unparseable response body"));
        }
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.request(Method::GET, path, None, None, &[]).await
    }

    /// `GET` that maps a 404 to `None`.
    pub async fn get_opt(&self, path: &str) -> anyhow::Result<Option<Value>> {
        match self.get(path).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn post(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        self.request(Method::POST, path, Some(body), None, &[])
            .await
    }

    pub async fn put(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        self.request(Method::PUT, path, Some(body), None, &[]).await
    }

    /// `DELETE`; an already-missing resource (404) counts as success.
    pub async fn delete(&self, path: &str) -> anyhow::Result<()> {
        match self.request(Method::DELETE, path, None, None, &[]).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Every item of a paginated collection, optionally narrowed with an `X-Filter`.
    pub async fn list(&self, path: &str, filter: Option<&Value>) -> anyhow::Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let q = [
                ("page", page.to_string()),
                ("page_size", PAGE_SIZE.to_string()),
            ];
            let v = self.request(Method::GET, path, None, filter, &q).await?;
            let data = v
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("GET {path}: response has no `data` array"))?;
            out.extend(data.iter().cloned());
            let pages = v.get("pages").and_then(Value::as_u64).unwrap_or(1);
            if u64::from(page) >= pages {
                return Ok(out);
            }
            page += 1;
        }
    }

    /// Poll `GET path` until `done` accepts the body. `what` names the wait in errors;
    /// `describe` renders the last body for the timeout message.
    pub async fn wait_until(
        &self,
        path: &str,
        what: &str,
        done: impl Fn(&Value) -> bool,
        describe: impl Fn(&Value) -> String,
    ) -> anyhow::Result<Value> {
        let start = Instant::now();
        loop {
            let v = self.get(path).await?;
            if done(&v) {
                return Ok(v);
            }
            if start.elapsed() >= self.wait_timeout {
                return Err(ApiError::Timeout {
                    what: what.into(),
                    after: self.wait_timeout,
                    last: describe(&v),
                }
                .into());
            }
            tracing::debug!(path, what, last = %describe(&v), "linode: waiting");
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Poll `GET path` until it returns 404.
    pub async fn wait_gone(&self, path: &str, what: &str) -> anyhow::Result<()> {
        let start = Instant::now();
        loop {
            if self.get_opt(path).await?.is_none() {
                return Ok(());
            }
            if start.elapsed() >= self.wait_timeout {
                return Err(ApiError::Timeout {
                    what: what.into(),
                    after: self.wait_timeout,
                    last: "still present".into(),
                }
                .into());
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

// Keep source selection separate from the process environment so failure and
// precedence tests never mutate global state or need real credentials.
fn resolve_token(
    directory: Option<&Path>,
    environment: impl FnOnce() -> Option<String>,
) -> Result<String, ApiError> {
    let token = if let Some(directory) = directory {
        if directory.as_os_str().is_empty() {
            return Err(ApiError::InvalidCredential);
        }
        std::fs::read_to_string(directory.join("linode-token"))
            .map_err(|source| ApiError::CredentialRead { source })?
            .trim_end_matches(['\r', '\n'])
            .to_owned()
    } else {
        environment().ok_or(ApiError::MissingToken)?
    };
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(ApiError::InvalidCredential);
    }
    Ok(token)
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn credential_file_is_authoritative_and_accepts_a_terminal_newline() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("linode-token"), "fixture-token\r\n").unwrap();
        let token = resolve_token(Some(directory.path()), || panic!("environment fallback"));
        assert_eq!(token.unwrap(), "fixture-token");
    }

    #[test]
    fn unavailable_credential_does_not_fall_back_to_environment() {
        let directory = tempfile::tempdir().unwrap();
        let error =
            resolve_token(Some(directory.path()), || panic!("environment fallback")).unwrap_err();
        assert!(matches!(error, ApiError::CredentialRead { .. }));
    }

    #[test]
    fn invalid_credentials_fail_without_disclosing_contents() {
        let directory = tempfile::tempdir().unwrap();
        for contents in [
            "",
            "\n",
            "fixture token",
            "fixture\ntoken",
            "fixture\0token",
            "é",
        ] {
            std::fs::write(directory.path().join("linode-token"), contents).unwrap();
            let error = resolve_token(Some(directory.path()), || panic!("environment fallback"))
                .unwrap_err();
            assert!(matches!(error, ApiError::InvalidCredential));
            assert_eq!(
                error.to_string(),
                "Linode credential must be nonempty ASCII without whitespace"
            );
        }
    }

    #[test]
    fn environment_remains_supported_without_a_credential_directory() {
        assert_eq!(
            resolve_token(None, || Some("fixture-token".into())).unwrap(),
            "fixture-token"
        );
        assert!(matches!(
            resolve_token(None, || None),
            Err(ApiError::MissingToken)
        ));
        assert!(matches!(
            resolve_token(Some(Path::new("")), || panic!("environment fallback")),
            Err(ApiError::InvalidCredential)
        ));
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn is_retryable(s: StatusCode) -> bool {
    matches!(
        s,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn retry_after(h: &HeaderMap) -> Option<Duration> {
    let secs: u64 = h.get(RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

fn parse_errors(text: &str) -> Vec<FieldError> {
    #[derive(Deserialize)]
    struct Body {
        #[serde(default)]
        errors: Vec<FieldError>,
    }
    serde_json::from_str::<Body>(text)
        .map(|b| b.errors)
        .unwrap_or_default()
}

/// `serde_json` helpers shared by the resource modules.
pub(crate) mod js {
    use serde_json::{Map, Value};

    /// Clone `obj[key]`, or `Null` when absent.
    pub fn get(obj: &Value, key: &str) -> Value {
        obj.get(key).cloned().unwrap_or(Value::Null)
    }

    pub fn str<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
        obj.get(key).and_then(Value::as_str)
    }

    pub fn i64(obj: &Value, key: &str) -> Option<i64> {
        obj.get(key).and_then(Value::as_i64)
    }

    pub fn bool(obj: &Value, key: &str) -> Option<bool> {
        obj.get(key).and_then(Value::as_bool)
    }

    /// Copy the named keys from `from` into `into`, skipping absent/null ones.
    pub fn copy(into: &mut Map<String, Value>, from: &Value, keys: &[&str]) {
        for k in keys {
            if let Some(v) = from.get(*k).filter(|v| !v.is_null()) {
                into.insert((*k).to_string(), v.clone());
            }
        }
    }

    /// Sorted copy of a JSON array (used to compare set-like inputs such as tags).
    pub fn sorted(v: &Value) -> Value {
        match v.as_array() {
            Some(a) => {
                let mut a = a.clone();
                a.sort_by_key(|x| x.to_string());
                Value::Array(a)
            }
            None => v.clone(),
        }
    }

    /// Replace `obj[key]` by `f(obj[key])` when present.
    pub fn map_key(obj: &mut Value, key: &str, f: impl Fn(&Value) -> Value) {
        if let Some(o) = obj.as_object_mut()
            && let Some(v) = o.get(key)
        {
            let nv = f(v);
            o.insert(key.to_string(), nv);
        }
    }
}
