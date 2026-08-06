//! CLI chat-proxy routing for SuperGrok subscription inference.

use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use lam_openai::{
    AuthSource, ProviderError, RequestHeaderSource, SharedAuthSource, bearer_header,
    try_insert_header,
};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use tokio::sync::Mutex;

use super::auth::{XaiCredentialStore, XaiCredentials};
use super::oauth::{self, OAuthError};

/// Subscription inference endpoint used by Grok Build and partner agents.
pub(crate) const CLI_PROXY_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";

const CLIENT_IDENTIFIER: &str = "lam-agent";

/// Floor for `x-grok-client-version`. The CLI chat proxy rejects older values
/// with HTTP 426; this is independent of Lam's own crate version.
const MIN_PROXY_CLIENT_VERSION: &str = "0.1.202";

/// Builds the static headers required by the Grok CLI chat proxy.
pub(crate) fn proxy_default_headers(client_mode: &str) -> Result<HeaderMap, ProviderError> {
    let version = proxy_client_version();
    let user_agent = format!("lam-agent/{version}");
    let mut headers = HeaderMap::new();
    try_insert_header(&mut headers, "x-grok-client-identifier", CLIENT_IDENTIFIER)?;
    try_insert_header(&mut headers, "x-grok-client-version", version)?;
    try_insert_header(&mut headers, "x-grok-client-mode", client_mode)?;
    try_insert_header(&mut headers, "X-XAI-Token-Auth", "xai-grok-cli")?;
    try_insert_header(
        &mut headers,
        "x-authenticateresponse",
        "authenticate-response",
    )?;
    try_insert_header(&mut headers, USER_AGENT.as_str(), &user_agent)?;
    Ok(headers)
}

/// Version string accepted by the Grok CLI chat proxy.
///
/// Prefers the installed Grok Build CLI version from `~/.grok/version.json`
/// when present and newer than [`MIN_PROXY_CLIENT_VERSION`]; otherwise uses
/// the minimum the proxy currently requires.
pub(crate) fn proxy_client_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            installed_grok_cli_version()
                .filter(|installed| version_at_least(installed, MIN_PROXY_CLIENT_VERSION))
                .unwrap_or_else(|| MIN_PROXY_CLIENT_VERSION.to_owned())
        })
        .as_str()
}

fn installed_grok_cli_version() -> Option<String> {
    let path = grok_cli_version_path()?;
    let source = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&source).ok()?;
    value
        .get("version")
        .or_else(|| value.get("stable_version"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|version| !version.is_empty() && version_looks_semver(version))
        .map(str::to_owned)
}

fn grok_cli_version_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".grok").join("version.json"))
}

fn version_looks_semver(value: &str) -> bool {
    value
        .split('.')
        .take(3)
        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

/// Compares dotted numeric versions (`0.2.118` ≥ `0.1.202`).
fn version_at_least(candidate: &str, minimum: &str) -> bool {
    let parse = |value: &str| -> Vec<u64> {
        value
            .split('.')
            .map(|part| {
                part.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    let left = parse(candidate);
    let right = parse(minimum);
    let len = left.len().max(right.len());
    for index in 0..len {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    true
}

/// Per-request affinity headers expected by the CLI chat proxy.
pub(crate) struct ProxyAffinityHeaders {
    session_id: String,
    model: String,
    counter: AtomicU64,
}

impl ProxyAffinityHeaders {
    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            session_id: request_id("session"),
            model: model.into(),
            counter: AtomicU64::new(1),
        }
    }
}

impl RequestHeaderSource for ProxyAffinityHeaders {
    fn headers(&self) -> HeaderMap {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let req_id = format!("{}-{}", self.session_id, n);
        let model = self
            .model
            .rsplit('/')
            .next()
            .unwrap_or(self.model.as_str())
            .to_ascii_lowercase();
        let mut headers = HeaderMap::new();
        let _ = try_insert_header(&mut headers, "x-grok-session-id", &self.session_id);
        let _ = try_insert_header(&mut headers, "x-grok-conv-id", &self.session_id);
        let _ = try_insert_header(&mut headers, "x-grok-req-id", &req_id);
        let _ = try_insert_header(&mut headers, "x-grok-model-override", &model);
        headers
    }
}

/// Refreshable SuperGrok OAuth auth source backed by the on-disk credential store.
pub(crate) struct XaiAuthSource {
    store: XaiCredentialStore,
    credentials: Mutex<XaiCredentials>,
}

impl XaiAuthSource {
    pub(crate) fn new(store: XaiCredentialStore, credentials: XaiCredentials) -> Self {
        Self {
            store,
            credentials: Mutex::new(credentials),
        }
    }
}

impl AuthSource for XaiAuthSource {
    fn authorization(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<HeaderValue>, ProviderError>> + Send + '_>> {
        Box::pin(async move {
            let mut guard = self.credentials.lock().await;
            let refreshed = oauth::ensure_fresh(&self.store, guard.clone())
                .await
                .map_err(|error| ProviderError::Auth {
                    message: error.to_string(),
                })?;
            *guard = refreshed;
            Ok(Some(bearer_header(&guard.access_token)?))
        })
    }

    fn on_unauthorized(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ProviderError>> + Send + '_>> {
        Box::pin(async move {
            let mut guard = self.credentials.lock().await;
            let refreshed = oauth::force_refresh(&self.store, guard.clone())
                .await
                .map_err(|error| ProviderError::Auth {
                    message: error.to_string(),
                })?;
            *guard = refreshed;
            Ok(true)
        })
    }
}

pub(crate) fn xai_auth_source(
    store: XaiCredentialStore,
    credentials: XaiCredentials,
) -> SharedAuthSource {
    Arc::new(XaiAuthSource::new(store, credentials))
}

fn request_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("lam-{prefix}-{nanos:x}-{n:x}")
}

impl From<OAuthError> for ProviderError {
    fn from(error: OAuthError) -> Self {
        Self::Auth {
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MIN_PROXY_CLIENT_VERSION, version_at_least};

    #[test]
    fn accepts_installed_cli_versions_above_the_floor() {
        assert!(version_at_least("0.1.202", MIN_PROXY_CLIENT_VERSION));
        assert!(version_at_least("0.2.118", MIN_PROXY_CLIENT_VERSION));
        assert!(!version_at_least("0.1.0", MIN_PROXY_CLIENT_VERSION));
        assert!(!version_at_least("0.1.201", MIN_PROXY_CLIENT_VERSION));
    }
}
