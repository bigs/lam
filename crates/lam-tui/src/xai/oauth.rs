//! Device-code OAuth against auth.x.ai for SuperGrok / X Premium.

use std::collections::HashMap;
use std::io;
use std::process::Command;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use thiserror::Error;

use super::auth::{AuthError, XaiCredentialStore, XaiCredentials};
use super::proxy::proxy_client_version;

/// Shared Grok CLI / third-party agent OAuth client used by Grok Build.
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access conversations:read conversations:write";
const DEVICE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_DEVICE_DURATION: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    token_type: Option<String>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Runs the RFC 8628 device-code login and persists tokens under `~/.lam/auth/xai.json`.
pub(crate) async fn device_login(
    store: &XaiCredentialStore,
    open_browser: bool,
) -> Result<XaiCredentials, OAuthError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(OAuthError::Http)?;
    let device = request_device_code(&client).await?;
    let open_url = device
        .verification_uri_complete
        .as_deref()
        .unwrap_or(device.verification_uri.as_str());

    println!("SuperGrok / X Premium login");
    println!();
    println!("  1. Open:  {open_url}");
    println!("  2. Code:  {}", device.user_code);
    println!();
    println!("Waiting for authorization…");

    if open_browser {
        let _ = open_url_in_browser(open_url);
    }

    let credentials = poll_for_token(&client, &device).await?;
    store.save(&credentials).map_err(OAuthError::Auth)?;
    println!(
        "Signed in. Credentials saved to {}.",
        store.path().display()
    );
    if let Some(email) = credentials.email.as_deref() {
        println!("Account: {email}");
    }
    Ok(credentials)
}

/// Returns credentials safe to use for a request.
///
/// When `force` is false, non-expired credentials are returned immediately.
/// When expired (or `force` is true), takes the credential lock, reloads from
/// disk (another process may already have refreshed), and only then refreshes
/// against auth.x.ai if still needed. This keeps concurrent `lam-agent`
/// processes from invalidating each other's refresh tokens.
pub(crate) async fn ensure_fresh(
    store: &XaiCredentialStore,
    credentials: XaiCredentials,
) -> Result<XaiCredentials, OAuthError> {
    synchronize(store, credentials, false).await
}

/// Like [`ensure_fresh`], but always coordinates with disk and refreshes when
/// the on-disk access token still matches the caller's (e.g. after HTTP 401).
pub(crate) async fn force_refresh(
    store: &XaiCredentialStore,
    credentials: XaiCredentials,
) -> Result<XaiCredentials, OAuthError> {
    synchronize(store, credentials, true).await
}

async fn synchronize(
    store: &XaiCredentialStore,
    local: XaiCredentials,
    force: bool,
) -> Result<XaiCredentials, OAuthError> {
    if !force && !local.is_expired() {
        return Ok(local);
    }

    let _lock = store.lock().map_err(OAuthError::Auth)?;
    // Another process may have refreshed while we waited for the lock.
    if let Some(disk) = store.load().map_err(OAuthError::Auth)? {
        if !force && !disk.is_expired() {
            return Ok(disk);
        }
        if force && disk.access_token != local.access_token && !disk.is_expired() {
            // Peer already rotated past our rejected token.
            return Ok(disk);
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(OAuthError::Http)?;
        // Prefer the disk refresh token; it is the latest known secret.
        let refreshed = refresh_token(&client, &disk).await?;
        store.save(&refreshed).map_err(OAuthError::Auth)?;
        return Ok(refreshed);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(OAuthError::Http)?;
    let refreshed = refresh_token(&client, &local).await?;
    store.save(&refreshed).map_err(OAuthError::Auth)?;
    Ok(refreshed)
}

async fn request_device_code(client: &reqwest::Client) -> Result<DeviceCodeResponse, OAuthError> {
    let mut form = HashMap::new();
    form.insert("client_id", CLIENT_ID);
    form.insert("scope", SCOPE);
    form.insert("referrer", "lam-agent");

    let response = client
        .post(DEVICE_URL)
        .headers(oauth_form_headers())
        .form(&form)
        .send()
        .await
        .map_err(OAuthError::Http)?;
    let status = response.status();
    let body = response.text().await.map_err(OAuthError::Http)?;
    if !status.is_success() {
        return Err(OAuthError::Status {
            status: status.as_u16(),
            body: truncate(&body),
            stage: "device authorization",
        });
    }
    serde_json::from_str(&body).map_err(|source| OAuthError::Json {
        stage: "device authorization",
        source,
    })
}

async fn poll_for_token(
    client: &reqwest::Client,
    device: &DeviceCodeResponse,
) -> Result<XaiCredentials, OAuthError> {
    let started = Instant::now();
    let mut interval = Duration::from_secs(device.interval.unwrap_or(5).max(1))
        .max(Duration::from_secs(1))
        .min(Duration::from_secs(30));
    if interval < DEFAULT_POLL_INTERVAL {
        interval = DEFAULT_POLL_INTERVAL.min(interval.max(Duration::from_secs(1)));
    }
    let deadline =
        started + Duration::from_secs(device.expires_in.max(30)).min(MAX_DEVICE_DURATION);

    loop {
        if Instant::now() >= deadline {
            return Err(OAuthError::TimedOut);
        }
        tokio::time::sleep(interval).await;

        let mut form = HashMap::new();
        form.insert("grant_type", DEVICE_GRANT);
        form.insert("device_code", device.device_code.as_str());
        form.insert("client_id", CLIENT_ID);

        let response = client
            .post(TOKEN_URL)
            .headers(oauth_form_headers())
            .form(&form)
            .send()
            .await
            .map_err(OAuthError::Http)?;
        let status = response.status();
        let body = response.text().await.map_err(OAuthError::Http)?;
        let token: TokenResponse =
            serde_json::from_str(&body).map_err(|source| OAuthError::Json {
                stage: "device token poll",
                source,
            })?;

        if let Some(error) = token.error.as_deref() {
            match error {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval = interval.saturating_add(Duration::from_secs(5));
                    continue;
                }
                "access_denied" => {
                    return Err(OAuthError::Denied(
                        token
                            .error_description
                            .unwrap_or_else(|| "access denied".to_owned()),
                    ));
                }
                "expired_token" => return Err(OAuthError::TimedOut),
                other => {
                    return Err(OAuthError::Status {
                        status: status.as_u16(),
                        body: truncate(token.error_description.as_deref().unwrap_or(other)),
                        stage: "device token poll",
                    });
                }
            }
        }

        if !status.is_success() {
            return Err(OAuthError::Status {
                status: status.as_u16(),
                body: truncate(&body),
                stage: "device token poll",
            });
        }

        return credentials_from_token(token, None);
    }
}

async fn refresh_token(
    client: &reqwest::Client,
    credentials: &XaiCredentials,
) -> Result<XaiCredentials, OAuthError> {
    let mut form = HashMap::new();
    form.insert("grant_type", "refresh_token");
    form.insert("refresh_token", credentials.refresh_token.as_str());
    form.insert("client_id", CLIENT_ID);

    let response = client
        .post(TOKEN_URL)
        .headers(oauth_form_headers())
        .form(&form)
        .send()
        .await
        .map_err(OAuthError::Http)?;
    let status = response.status();
    let body = response.text().await.map_err(OAuthError::Http)?;
    if !status.is_success() {
        return Err(OAuthError::Status {
            status: status.as_u16(),
            body: truncate(&body),
            stage: "token refresh",
        });
    }
    let token: TokenResponse = serde_json::from_str(&body).map_err(|source| OAuthError::Json {
        stage: "token refresh",
        source,
    })?;
    if let Some(error) = token.error.as_deref() {
        return Err(OAuthError::Status {
            status: status.as_u16(),
            body: truncate(token.error_description.as_deref().unwrap_or(error)),
            stage: "token refresh",
        });
    }
    credentials_from_token(token, Some(credentials))
}

fn credentials_from_token(
    token: TokenResponse,
    previous: Option<&XaiCredentials>,
) -> Result<XaiCredentials, OAuthError> {
    let access = token
        .access_token
        .filter(|value| !value.is_empty())
        .ok_or_else(|| OAuthError::Message("token response missing access_token".to_owned()))?;
    let refresh = token
        .refresh_token
        .filter(|value| !value.is_empty())
        .or_else(|| previous.map(|credentials| credentials.refresh_token.clone()))
        .ok_or_else(|| OAuthError::Message("token response missing refresh_token".to_owned()))?;
    let expires_in = token.expires_in.unwrap_or(3_600).max(60);
    let mut credentials = XaiCredentials::from_token_response(
        access,
        refresh,
        expires_in,
        token.token_type,
        token.scope,
    );
    if let Some(previous) = previous {
        credentials.user_id = previous.user_id.clone();
        credentials.email = previous.email.clone();
    }
    Ok(credentials)
}

fn oauth_form_headers() -> HeaderMap {
    let version = proxy_client_version();
    let user_agent = format!("lam-agent/{version}");
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    if let Ok(value) = HeaderValue::from_str(&user_agent) {
        headers.insert(USER_AGENT, value);
    }
    headers.insert(
        "x-grok-client-identifier",
        HeaderValue::from_static("lam-agent"),
    );
    if let Ok(value) = HeaderValue::from_str(version) {
        headers.insert("x-grok-client-version", value);
    }
    headers
}

pub(crate) fn open_url_in_browser(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(url).spawn()?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open").arg(url).spawn()?;
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd").args(["/C", "start", "", url]).spawn()?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        let _ = writeln!(
            io::stderr(),
            "lam-agent: open the verification URL in a browser manually"
        );
        Ok(())
    }
}

fn truncate(body: &str) -> String {
    const MAX: usize = 512;
    let mut out = body.chars().take(MAX).collect::<String>();
    if body.chars().count() > MAX {
        out.push('…');
    }
    out
}

#[derive(Debug, Error)]
pub(crate) enum OAuthError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("SuperGrok OAuth HTTP failed: {0}")]
    Http(#[source] reqwest::Error),
    #[error("SuperGrok OAuth {stage} returned HTTP {status}: {body}")]
    Status {
        stage: &'static str,
        status: u16,
        body: String,
    },
    #[error("SuperGrok OAuth {stage} returned invalid JSON: {source}")]
    Json {
        stage: &'static str,
        source: serde_json::Error,
    },
    #[error("SuperGrok authorization timed out before approval")]
    TimedOut,
    #[error("SuperGrok authorization was denied: {0}")]
    Denied(String),
    #[error("{0}")]
    Message(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xai::auth::{XaiCredentialStore, XaiCredentials};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn future_credentials(access: &str, refresh: &str) -> XaiCredentials {
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600;
        XaiCredentials {
            access_token: access.to_owned(),
            refresh_token: refresh.to_owned(),
            expires_at,
            token_type: Some("Bearer".to_owned()),
            scope: None,
            user_id: None,
            email: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ensure_fresh_returns_non_expired_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let store = XaiCredentialStore::new(dir.path().join("xai.json"));
        let credentials = future_credentials("access-a", "refresh-a");
        store.save(&credentials).unwrap();
        let out = ensure_fresh(&store, credentials.clone()).await.unwrap();
        assert_eq!(out.access_token, "access-a");
        assert_eq!(out.refresh_token, "refresh-a");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ensure_fresh_prefers_fresher_disk_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let store = XaiCredentialStore::new(dir.path().join("xai.json"));
        let stale = XaiCredentials {
            access_token: "stale-access".to_owned(),
            refresh_token: "stale-refresh".to_owned(),
            expires_at: 1, // expired
            token_type: Some("Bearer".to_owned()),
            scope: None,
            user_id: None,
            email: None,
        };
        let fresh = future_credentials("fresh-access", "fresh-refresh");
        store.save(&fresh).unwrap();
        // Local is expired; disk already has a peer-refreshed token.
        let out = ensure_fresh(&store, stale).await.unwrap();
        assert_eq!(out.access_token, "fresh-access");
        assert_eq!(out.refresh_token, "fresh-refresh");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn force_refresh_adopts_peer_rotated_token_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let store = XaiCredentialStore::new(dir.path().join("xai.json"));
        let local = future_credentials("local-access", "local-refresh");
        let peer = future_credentials("peer-access", "peer-refresh");
        store.save(&peer).unwrap();
        let out = force_refresh(&store, local).await.unwrap();
        assert_eq!(out.access_token, "peer-access");
    }
}
