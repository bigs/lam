//! ChatGPT subscription authentication through the official Codex login cache.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use lam_openai::{AuthSource, SharedAuthSource, bearer_header};
#[cfg(unix)]
use nix::fcntl::{Flock, FlockArg};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use lam_openai::ProviderError;

pub(crate) const CODEX_BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Hardcoded mirror of the official codex-cli version whose compatibility
/// identity the ChatGPT backend gates subscription model routing on.
/// lam-agent no longer shells out to an installed `codex` binary, so this
/// constant must be bumped manually whenever the endpoint starts rejecting it
/// (see docs/TODO.md).
pub(crate) const CODEX_CLIENT_VERSION: &str = "0.146.0";
/// OAuth2 authorization-code + PKCE login endpoints, mirroring the official
/// Codex CLI's flow. The loopback redirect port is fixed by client registration.
const OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OAUTH_SCOPE: &str = "openid profile email offline_access";
/// Loopback port the Codex OAuth client registration allows. The
/// authorization server rejects any other port for this client id
/// (`authorize_hydra_invalid_request`), so unlike the generic xAI login we
/// bind this fixed port instead of an ephemeral one.
const OAUTH_REDIRECT_PORT: u16 = 1455;
/// How long `lam-agent login openai` waits for the browser redirect.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const REFRESH_SKEW: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexAuthFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_mode: Option<String>,
    #[serde(
        rename = "OPENAI_API_KEY",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens: Option<CodexTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_refresh: Option<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexTokens {
    id_token: String,
    access_token: String,
    refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct JwtExpiry {
    exp: u64,
}

#[derive(Debug, Default, Deserialize)]
struct IdTokenClaims {
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<ChatGptClaims>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatGptClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_account_is_fedramp: bool,
}

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct TokenExchangeRequest<'a> {
    grant_type: &'static str,
    client_id: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    code_verifier: &'a str,
}

#[derive(Debug, Deserialize)]
struct TokenExchangeResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Authorization code extracted from the browser redirect.
struct OAuthCode {
    code: String,
}

/// Injectable OAuth endpoints so the login flow can be tested against local
/// listeners.
struct OAuthEndpoints {
    authorize_base: String,
    token_url: String,
}

#[derive(Clone, Debug)]
pub(crate) struct CodexCredentials {
    access_token: String,
    pub(crate) account_id: String,
    pub(crate) is_fedramp: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CodexCredentialStore {
    path: PathBuf,
}

/// Exclusive inter-process lock for Codex credential read/refresh/write.
///
/// Held while reloading from disk and optionally refreshing so concurrent
/// `lam-agent` processes do not race the rotating refresh token. The sidecar
/// lock file lives next to the credential cache; the official Codex CLI does
/// not take part in this protocol, but it rewrites the cache atomically, so
/// cross-tool corruption is not possible either.
#[derive(Debug)]
pub(crate) struct CredentialLock {
    #[cfg(unix)]
    _guard: Flock<fs::File>,
    #[cfg(not(unix))]
    _file: fs::File,
}

impl CodexCredentialStore {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn default_store() -> Result<Self, CodexAuthError> {
        let home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .or_else(|| env::var_os("USERPROFILE"))
                    .map(|home| PathBuf::from(home).join(".codex"))
            })
            .ok_or(CodexAuthError::HomeUnavailable)?;
        Ok(Self::new(home.join("auth.json")))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Path of the sidecar lock file used for cross-process refresh
    /// coordination.
    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }

    /// Acquire an exclusive lock for credential reload/refresh, mirroring the
    /// SuperGrok credential store.
    ///
    /// On Unix this uses `flock(2)`. On other platforms the lock is
    /// best-effort (open-only) so single-process use still works.
    pub(crate) fn lock(&self) -> Result<CredentialLock, CodexAuthError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| CodexAuthError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let path = self.lock_path();
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| CodexAuthError::Write {
                path: path.clone(),
                source,
            })?;
        #[cfg(unix)]
        {
            let guard = Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, source)| {
                CodexAuthError::Write {
                    path,
                    source: source.into(),
                }
            })?;
            Ok(CredentialLock { _guard: guard })
        }
        #[cfg(not(unix))]
        {
            Ok(CredentialLock { _file: file })
        }
    }

    /// Whether the official Codex CLI has a credential cache that Lam can use.
    /// The file is validated only when the runtime builds the provider.
    pub(crate) fn credentials_present(&self) -> bool {
        self.path.is_file()
    }

    fn load(&self) -> Result<CodexAuthFile, CodexAuthError> {
        let source = fs::read_to_string(&self.path).map_err(|source| CodexAuthError::Read {
            path: self.path.clone(),
            source,
        })?;
        let file = serde_json::from_str(&source).map_err(|source| CodexAuthError::Parse {
            path: self.path.clone(),
            source,
        })?;
        validate_auth_file(&file, &self.path)?;
        Ok(file)
    }

    fn save(&self, file: &CodexAuthFile) -> Result<(), CodexAuthError> {
        let body = serde_json::to_vec_pretty(file).map_err(|source| CodexAuthError::Serialize {
            path: self.path.clone(),
            source,
        })?;
        // The exclusive lock serializes Lam refreshes, but the official Codex
        // CLI (or a crashed earlier write) may race or litter this name.
        let temp = self
            .path
            .with_extension(format!("json.lam.{}.tmp", std::process::id()));
        let result = (|| {
            {
                let mut output =
                    fs::File::create(&temp).map_err(|source| CodexAuthError::Write {
                        path: temp.clone(),
                        source,
                    })?;
                output
                    .write_all(&body)
                    .and_then(|()| output.sync_all())
                    .map_err(|source| CodexAuthError::Write {
                        path: temp.clone(),
                        source,
                    })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).map_err(
                        |source| CodexAuthError::Write {
                            path: temp.clone(),
                            source,
                        },
                    )?;
                }
            }
            fs::rename(&temp, &self.path).map_err(|source| CodexAuthError::Write {
                path: self.path.clone(),
                source,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    /// Deletes the credential cache and the sidecar lock file.
    ///
    /// Because the cache is shared with the official Codex CLI, this also signs
    /// that tool out. The lock file is disposable coordination state (recreated
    /// on demand), so its removal is best-effort.
    pub(crate) fn remove(&self) -> Result<(), CodexAuthError> {
        let _ = fs::remove_file(self.lock_path());
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(CodexAuthError::Write {
                path: self.path.clone(),
                source,
            }),
        }
    }
}

pub(crate) struct CodexAuthSession {
    store: CodexCredentialStore,
    client: reqwest::Client,
    refresh_url: String,
    request_lock: Mutex<()>,
    expected_account_id: Mutex<Option<String>>,
}

impl CodexAuthSession {
    pub(crate) fn new(store: CodexCredentialStore) -> Self {
        Self::with_refresh_url(store, OAUTH_TOKEN_URL)
    }

    fn with_refresh_url(store: CodexCredentialStore, refresh_url: impl Into<String>) -> Self {
        let mut client_headers = HeaderMap::new();
        // Token refresh goes to the same account service the official CLI
        // uses, so mirror its originator here as well as on model requests.
        client_headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
        Self {
            store,
            client: reqwest::Client::builder()
                .default_headers(client_headers)
                .user_agent(format!("lam-agent/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("static Codex HTTP client configuration is valid"),
            refresh_url: refresh_url.into(),
            request_lock: Mutex::new(()),
            expected_account_id: Mutex::new(None),
        }
    }

    pub(crate) async fn credentials(&self) -> Result<CodexCredentials, CodexAuthError> {
        let _guard = self.request_lock.lock().await;
        let mut file = self.store.load()?;
        if needs_refresh(&file)? {
            // Serialize refresh across lam-agent processes; the rotating
            // refresh token is single-use. The official Codex CLI does not
            // take this lock, but it rewrites the cache atomically, so either
            // writer leaves a consistent file.
            let _lock = self.store.lock()?;
            // Another process may have refreshed while we waited.
            file = self.store.load()?;
            if needs_refresh(&file)? {
                self.refresh(&mut file).await?;
                self.store.save(&file)?;
            }
        }
        let credentials = credentials_from_file(&file, self.store.path())?;
        let mut expected = self.expected_account_id.lock().await;
        match expected.as_deref() {
            Some(account_id) if account_id != credentials.account_id => {
                Err(CodexAuthError::AccountChanged)
            }
            Some(_) => Ok(credentials),
            None => {
                *expected = Some(credentials.account_id.clone());
                Ok(credentials)
            }
        }
    }

    async fn refresh(&self, file: &mut CodexAuthFile) -> Result<(), CodexAuthError> {
        let refresh_token = file
            .tokens
            .as_ref()
            .map(|tokens| tokens.refresh_token.as_str())
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| CodexAuthError::InvalidCredentials {
                path: self.store.path.clone(),
                message: "refresh token is missing; run `lam-agent login openai`".to_owned(),
            })?;
        let response = self
            .client
            .post(&self.refresh_url)
            .header("Content-Type", "application/json")
            .json(&RefreshRequest {
                client_id: OAUTH_CLIENT_ID,
                grant_type: "refresh_token",
                refresh_token,
            })
            .send()
            .await
            .map_err(CodexAuthError::RefreshHttp)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CodexAuthError::RefreshStatus {
                status: status.as_u16(),
                message: refresh_error_message(&body),
            });
        }
        let response = response
            .json::<RefreshResponse>()
            .await
            .map_err(CodexAuthError::RefreshHttp)?;
        let tokens = file
            .tokens
            .as_mut()
            .expect("validated Codex auth has tokens");
        if let Some(id_token) = response.id_token {
            tokens.id_token = id_token;
        }
        if let Some(access_token) = response.access_token {
            tokens.access_token = access_token;
        }
        if let Some(refresh_token) = response.refresh_token {
            tokens.refresh_token = refresh_token;
        }
        validate_auth_file(file, self.store.path())?;
        Ok(())
    }
}

impl AuthSource for CodexAuthSession {
    fn authorization(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<HeaderValue>, ProviderError>> + Send + '_>> {
        Box::pin(async move {
            let credentials = self
                .credentials()
                .await
                .map_err(|error| ProviderError::Auth {
                    message: error.to_string(),
                })?;
            Ok(Some(bearer_header(&credentials.access_token)?))
        })
    }
}

pub(crate) async fn load_codex_auth(
    store: CodexCredentialStore,
) -> Result<(SharedAuthSource, CodexCredentials), CodexAuthError> {
    let session = Arc::new(CodexAuthSession::new(store));
    let credentials = session.credentials().await?;
    Ok((session, credentials))
}

pub(crate) fn default_headers(credentials: &CodexCredentials) -> Result<HeaderMap, CodexAuthError> {
    let mut headers = HeaderMap::new();
    // The ChatGPT Codex backend gates model routing on the compatibility
    // identity of the official client that owns the shared login. Use the
    // hardcoded mirror of the official codex-cli version (CODEX_CLIENT_VERSION)
    // instead of Lam's unrelated package version; see docs/TODO.md.
    headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
    headers.insert(
        "version",
        HeaderValue::from_str(CODEX_CLIENT_VERSION).map_err(CodexAuthError::InvalidHeader)?,
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&format!("codex_cli_rs/{CODEX_CLIENT_VERSION} (lam-agent)"))
            .map_err(CodexAuthError::InvalidHeader)?,
    );
    headers.insert(
        "ChatGPT-Account-ID",
        HeaderValue::from_str(&credentials.account_id).map_err(CodexAuthError::InvalidHeader)?,
    );
    if credentials.is_fedramp {
        headers.insert("X-OpenAI-Fedramp", HeaderValue::from_static("true"));
    }
    Ok(headers)
}

/// Runs the OAuth2 authorization-code + PKCE login against OpenAI and writes
/// the shared Codex credential cache so the official Codex CLI can use it too.
pub(crate) async fn login(
    no_browser: bool,
    force: bool,
) -> Result<CodexCredentialStore, CodexAuthError> {
    let store = CodexCredentialStore::default_store()?;
    let verifier = pkce_verifier()?;
    let state = oauth_state()?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", OAUTH_REDIRECT_PORT))
        .await
        .map_err(CodexAuthError::OAuthListener)?;
    let endpoints = OAuthEndpoints {
        authorize_base: OAUTH_AUTHORIZE_URL.to_owned(),
        token_url: OAUTH_TOKEN_URL.to_owned(),
    };
    login_with(
        store.clone(),
        listener,
        &endpoints,
        no_browser,
        force,
        &verifier,
        &state,
    )
    .await?;
    Ok(store)
}

/// Removes the shared Codex credential cache (and our sidecar lock file).
///
/// This also signs the official Codex CLI out because the cache is shared.
pub(crate) fn logout() -> Result<(), CodexAuthError> {
    let store = CodexCredentialStore::default_store()?;
    store.remove()
}

/// Builds the authorize URL for the Codex OAuth client.
///
/// The parameter set mirrors the official Codex CLI exactly (fixed redirect
/// port, simplified-flow and organization flags, originator; no `audience`).
/// The authorization server validates this contract strictly: deviations
/// surface as `authorize_hydra_invalid_request` before any login page.
fn build_authorize_url(
    authorize_base: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> String {
    format!(
        "{authorize_base}?response_type=code&client_id={client_id}&redirect_uri={redirect_uri}&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator={originator}",
        client_id = url_encode(OAUTH_CLIENT_ID),
        redirect_uri = url_encode(redirect_uri),
        scope = url_encode(OAUTH_SCOPE),
        challenge = url_encode(challenge),
        state = url_encode(state),
        originator = url_encode("lam-agent"),
    )
}

/// Core login flow with injectable endpoints, listener, and OAuth secrets so
/// tests can run it entirely against local listeners.
async fn login_with(
    store: CodexCredentialStore,
    listener: tokio::net::TcpListener,
    endpoints: &OAuthEndpoints,
    no_browser: bool,
    force: bool,
    verifier: &str,
    state: &str,
) -> Result<(), CodexAuthError> {
    ensure_overwrite_allowed(&store, force)?;
    let challenge = pkce_challenge(verifier);
    let port = listener
        .local_addr()
        .map_err(CodexAuthError::OAuthListener)?
        .port();
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let authorize_url =
        build_authorize_url(&endpoints.authorize_base, &redirect_uri, &challenge, state);

    println!("OpenAI / Codex login");
    println!();
    println!("  1. Open:  {authorize_url}");
    println!();
    println!("Waiting for authorization…");

    if !no_browser {
        let _ = crate::xai::open_url_in_browser(&authorize_url);
    }

    let code = receive_authorization_code(listener, state).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(CodexAuthError::OAuthHttp)?;
    let token = exchange_code(
        &client,
        &endpoints.token_url,
        &code,
        verifier,
        &redirect_uri,
    )
    .await?;
    let file = auth_file_from_token_response(token, store.path())?;
    store.save(&file)?;
    println!(
        "Signed in. Credentials saved to {}.",
        store.path().display()
    );
    Ok(())
}

/// Safety valve: refuse to overwrite valid existing credentials unless forced.
///
/// The cache is shared with the official Codex CLI, so silently replacing a
/// working login would strand that tool (and the account the user chose).
fn ensure_overwrite_allowed(
    store: &CodexCredentialStore,
    force: bool,
) -> Result<(), CodexAuthError> {
    if !force && store.load().is_ok() {
        return Err(CodexAuthError::CredentialsExist {
            path: store.path().to_path_buf(),
        });
    }
    Ok(())
}

/// Waits for the browser redirect on the loopback listener and returns the
/// authorization code, validating the OAuth state parameter.
async fn receive_authorization_code(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String, CodexAuthError> {
    let accept = async {
        let (mut stream, _peer) = listener
            .accept()
            .await
            .map_err(CodexAuthError::OAuthListener)?;
        let request_line = read_request_line(&mut stream).await?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        if method != "GET" {
            return Err(CodexAuthError::OAuthRedirect(format!(
                "expected a GET redirect, got {method}"
            )));
        }
        let code = parse_redirect(target, expected_state)?.code;
        let body = "You are signed in. You can close this window and return to lam-agent.";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        Ok(code)
    };
    tokio::time::timeout(LOGIN_TIMEOUT, accept)
        .await
        .map_err(|_| CodexAuthError::OAuthTimedOut)?
}

/// Reads the first HTTP request line from the loopback redirect connection.
async fn read_request_line(stream: &mut tokio::net::TcpStream) -> Result<String, CodexAuthError> {
    let mut buffer = [0_u8; 8192];
    let mut used = 0;
    loop {
        let read = stream
            .read(&mut buffer[used..])
            .await
            .map_err(CodexAuthError::OAuthListener)?;
        if read == 0 {
            break;
        }
        used += read;
        if let Some(end) = buffer[..used].iter().position(|byte| *byte == b'\n') {
            let line = &buffer[..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            return Ok(String::from_utf8_lossy(line).into_owned());
        }
        if used == buffer.len() {
            break;
        }
    }
    Err(CodexAuthError::OAuthRedirect(
        "no HTTP request received on the callback listener".to_owned(),
    ))
}

/// Parses the OAuth redirect request target and validates the state param.
fn parse_redirect(target: &str, expected_state: &str) -> Result<OAuthCode, CodexAuthError> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/auth/callback" {
        return Err(CodexAuthError::OAuthRedirect(format!(
            "unexpected redirect path {path}"
        )));
    }
    let params = parse_query(query);
    if let Some(error) = params.get("error") {
        let description = params
            .get("error_description")
            .filter(|value| !value.trim().is_empty())
            .map(String::as_str)
            .unwrap_or(error);
        return Err(CodexAuthError::OAuthDenied(description.to_owned()));
    }
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return Err(CodexAuthError::OAuthStateMismatch);
    }
    let code = params
        .get("code")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CodexAuthError::OAuthRedirect("redirect is missing the authorization code".to_owned())
        })?;
    Ok(OAuthCode { code: code.clone() })
}

/// Parses an application/x-www-form-urlencoded query string.
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=') {
            Some((key, value)) => {
                params.insert(percent_decode(key), percent_decode(value));
            }
            None => {
                params.insert(percent_decode(pair), String::new());
            }
        }
    }
    params
}

/// Decodes one percent-encoded query component (%XX and + for space).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        output.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        output.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Percent-encodes a value for use inside a URL query string.
fn url_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(char::from(byte));
            }
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

fn truncate(body: &str) -> String {
    const MAX: usize = 512;
    let mut out = body.chars().take(MAX).collect::<String>();
    if body.chars().count() > MAX {
        out.push('…');
    }
    out
}

/// RFC 7636 PKCE verifier: 32 random bytes, base64url-encoded (43 chars).
fn pkce_verifier() -> Result<String, CodexAuthError> {
    random_token(32)
}

/// Opaque OAuth state value for redirect CSRF protection.
fn oauth_state() -> Result<String, CodexAuthError> {
    random_token(16)
}

/// RFC 7636 S256 challenge for a PKCE verifier.
fn pkce_challenge(verifier: &str) -> String {
    let hash = digest(&SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

fn random_token(byte_count: usize) -> Result<String, CodexAuthError> {
    let mut bytes = vec![0_u8; byte_count];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| CodexAuthError::OAuthRandom)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Exchanges an authorization code for tokens at the OAuth token endpoint.
async fn exchange_code(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenExchangeResponse, CodexAuthError> {
    let response = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&TokenExchangeRequest {
            grant_type: "authorization_code",
            client_id: OAUTH_CLIENT_ID,
            code,
            redirect_uri,
            code_verifier: verifier,
        })
        .send()
        .await
        .map_err(CodexAuthError::OAuthHttp)?;
    let status = response.status();
    let body = response.text().await.map_err(CodexAuthError::OAuthHttp)?;
    if !status.is_success() {
        return Err(CodexAuthError::OAuthTokenStatus {
            status: status.as_u16(),
            body: truncate(&body),
        });
    }
    let token: TokenExchangeResponse =
        serde_json::from_str(&body).map_err(|source| CodexAuthError::OAuthJson { source })?;
    if let Some(error) = token.error.as_deref() {
        let description = token
            .error_description
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| error.to_owned());
        return Err(CodexAuthError::OAuthDenied(description));
    }
    Ok(token)
}

/// Builds a shared Codex auth file from an authorization-code token response.
fn auth_file_from_token_response(
    token: TokenExchangeResponse,
    path: &Path,
) -> Result<CodexAuthFile, CodexAuthError> {
    let id_token = token
        .id_token
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| CodexAuthError::InvalidCredentials {
            path: path.to_path_buf(),
            message: "the OAuth token response is missing id_token".to_owned(),
        })?;
    let access_token = token
        .access_token
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| CodexAuthError::InvalidCredentials {
            path: path.to_path_buf(),
            message: "the OAuth token response is missing access_token".to_owned(),
        })?;
    let refresh_token = token
        .refresh_token
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| CodexAuthError::InvalidCredentials {
            path: path.to_path_buf(),
            message: "the OAuth token response is missing refresh_token".to_owned(),
        })?;
    let claims: IdTokenClaims = decode_jwt(&id_token)?;
    let account_id = claims
        .auth
        .as_ref()
        .and_then(|auth| auth.chatgpt_account_id.clone())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| CodexAuthError::InvalidCredentials {
            path: path.to_path_buf(),
            message: "id_token is missing the ChatGPT account id".to_owned(),
        })?;
    let file = CodexAuthFile {
        auth_mode: Some("chatgpt".to_owned()),
        openai_api_key: None,
        tokens: Some(CodexTokens {
            id_token,
            access_token,
            refresh_token,
            account_id: Some(account_id),
            extra: Map::new(),
        }),
        last_refresh: None,
        extra: Map::new(),
    };
    validate_auth_file(&file, path)?;
    Ok(file)
}

fn validate_auth_file(file: &CodexAuthFile, path: &Path) -> Result<(), CodexAuthError> {
    if file.auth_mode.as_deref() != Some("chatgpt") {
        return Err(CodexAuthError::InvalidCredentials {
            path: path.to_path_buf(),
            message: "Codex is not signed in with ChatGPT; run `lam-agent login openai`".to_owned(),
        });
    }
    let Some(tokens) = &file.tokens else {
        return Err(CodexAuthError::InvalidCredentials {
            path: path.to_path_buf(),
            message: "token data is missing; run `lam-agent login openai`".to_owned(),
        });
    };
    if tokens.access_token.trim().is_empty() || tokens.id_token.trim().is_empty() {
        return Err(CodexAuthError::InvalidCredentials {
            path: path.to_path_buf(),
            message: "token data is incomplete; run `lam-agent login openai`".to_owned(),
        });
    }
    Ok(())
}

fn credentials_from_file(
    file: &CodexAuthFile,
    path: &Path,
) -> Result<CodexCredentials, CodexAuthError> {
    let tokens = file
        .tokens
        .as_ref()
        .expect("validated Codex auth has tokens");
    let claims: IdTokenClaims = decode_jwt(&tokens.id_token)?;
    let account_id = tokens
        .account_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| claims.auth.as_ref()?.chatgpt_account_id.clone())
        .ok_or_else(|| CodexAuthError::InvalidCredentials {
            path: path.to_path_buf(),
            message: "ChatGPT account ID is missing; run `lam-agent login openai`".to_owned(),
        })?;
    Ok(CodexCredentials {
        access_token: tokens.access_token.clone(),
        account_id,
        is_fedramp: claims
            .auth
            .is_some_and(|claims| claims.chatgpt_account_is_fedramp),
    })
}

fn needs_refresh(file: &CodexAuthFile) -> Result<bool, CodexAuthError> {
    let tokens = file
        .tokens
        .as_ref()
        .expect("validated Codex auth has tokens");
    let expiry: JwtExpiry = decode_jwt(&tokens.access_token)?;
    Ok(now_unix().saturating_add(REFRESH_SKEW.as_secs()) >= expiry.exp)
}

fn decode_jwt<T: for<'de> Deserialize<'de>>(token: &str) -> Result<T, CodexAuthError> {
    // Claims are consumed unsigned: the token arrives over TLS from OpenAI's
    // token endpoint or from the local credential cache we already trust for
    // the bearer token itself. This is not a security boundary.
    let payload = token
        .split('.')
        .nth(1)
        .filter(|part| !part.is_empty())
        .ok_or(CodexAuthError::InvalidJwt)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| CodexAuthError::InvalidJwt)?;
    serde_json::from_slice(&bytes).map_err(|_| CodexAuthError::InvalidJwt)
}

fn refresh_error_message(body: &str) -> String {
    let value = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or("Codex token refresh failed; run `lam-agent login openai`")
        .chars()
        .take(500)
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[derive(Debug, Error)]
pub(crate) enum CodexAuthError {
    #[error("could not determine CODEX_HOME or the home directory")]
    HomeUnavailable,
    #[error("could not read Codex credentials at `{path}`: {source}; run `lam-agent login openai`")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse Codex credentials at `{path}`: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid Codex credentials at `{path}`: {message}")]
    InvalidCredentials { path: PathBuf, message: String },
    #[error("Codex credential token is not a valid JWT")]
    InvalidJwt,
    #[error("the Codex login changed to another ChatGPT workspace; restart Lam")]
    AccountChanged,
    #[error(
        "Codex credentials already exist at `{path}`; run `lam-agent logout openai` first, or pass --force to replace them"
    )]
    CredentialsExist { path: PathBuf },
    #[error("could not serialize Codex credentials at `{path}`: {source}")]
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("could not write Codex credentials at `{path}`: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Codex token refresh request failed: {0}")]
    RefreshHttp(reqwest::Error),
    #[error("Codex token refresh returned HTTP {status}: {message}")]
    RefreshStatus { status: u16, message: String },
    #[error("invalid Codex request header: {0}")]
    InvalidHeader(reqwest::header::InvalidHeaderValue),
    #[error("OpenAI OAuth request failed: {0}")]
    OAuthHttp(reqwest::Error),
    #[error(
        "could not start the local OAuth callback listener (the Codex flow needs port 1455 free): {0}"
    )]
    OAuthListener(std::io::Error),
    #[error("OpenAI authorization timed out; no browser redirect was received")]
    OAuthTimedOut,
    #[error("OpenAI OAuth redirect state did not match; refusing the redirect")]
    OAuthStateMismatch,
    #[error("OpenAI authorization was denied: {0}")]
    OAuthDenied(String),
    #[error("invalid OpenAI OAuth redirect: {0}")]
    OAuthRedirect(String),
    #[error("OpenAI token exchange returned HTTP {status}: {body}")]
    OAuthTokenStatus { status: u16, body: String },
    #[error("OpenAI token exchange returned invalid JSON: {source}")]
    OAuthJson { source: serde_json::Error },
    #[error("could not generate a secure random PKCE verifier")]
    OAuthRandom,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn jwt(payload: Value) -> String {
        let payload = serde_json::to_vec(&payload).unwrap();
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("header.{encoded}.signature")
    }

    fn auth_file(expiry: u64) -> CodexAuthFile {
        CodexAuthFile {
            auth_mode: Some("chatgpt".to_owned()),
            openai_api_key: None,
            tokens: Some(CodexTokens {
                id_token: jwt(serde_json::json!({
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "account-123",
                        "chatgpt_account_is_fedramp": false
                    }
                })),
                access_token: jwt(serde_json::json!({ "exp": expiry })),
                refresh_token: "refresh-old".to_owned(),
                account_id: None,
                extra: Map::new(),
            }),
            last_refresh: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn reads_account_and_builds_codex_headers() {
        let file = auth_file(now_unix() + 3_600);
        let credentials = credentials_from_file(&file, Path::new("auth.json")).unwrap();
        assert_eq!(credentials.account_id, "account-123");
        assert!(!needs_refresh(&file).unwrap());
        let headers = default_headers(&credentials).unwrap();
        assert_eq!(headers["ChatGPT-Account-ID"], "account-123");
        assert_eq!(headers["originator"], "codex_cli_rs");
        assert_eq!(headers["version"], CODEX_CLIENT_VERSION);
        assert_eq!(
            headers[USER_AGENT],
            format!("codex_cli_rs/{CODEX_CLIENT_VERSION} (lam-agent)")
        );
    }

    #[test]
    fn pkce_verifier_is_base64url_and_in_range() {
        let verifier = pkce_verifier().unwrap();
        assert!((43..=128).contains(&verifier.len()));
        assert!(
            verifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
        let state = oauth_state().unwrap();
        assert_eq!(state.len(), 22);
    }

    #[test]
    fn pkce_challenge_matches_sha256_test_vector() {
        // SHA-256("hello") base64url without padding.
        assert_eq!(
            pkce_challenge("hello"),
            "LPJNul-wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ"
        );
    }

    #[test]
    fn parses_redirect_query_with_percent_decoding() {
        let params = parse_query("code=abc123&state=xyz");
        assert_eq!(params.get("code").map(String::as_str), Some("abc123"));
        assert_eq!(params.get("state").map(String::as_str), Some("xyz"));
        let params = parse_query("code=a%2Bb%20c&state=s%2Ft");
        assert_eq!(params.get("code").map(String::as_str), Some("a+b c"));
        assert_eq!(params.get("state").map(String::as_str), Some("s/t"));
        assert_eq!(params.get("missing"), None);
    }

    #[test]
    fn parses_redirect_target_and_validates_state() {
        let code = parse_redirect("/auth/callback?code=abc&state=xyz", "xyz").unwrap();
        assert_eq!(code.code, "abc");
        assert!(matches!(
            parse_redirect("/auth/callback?code=abc&state=wrong", "xyz"),
            Err(CodexAuthError::OAuthStateMismatch)
        ));
        assert!(matches!(
            parse_redirect(
                "/auth/callback?error=access_denied&error_description=declined",
                "xyz"
            ),
            Err(CodexAuthError::OAuthDenied(message)) if message == "declined"
        ));
        assert!(matches!(
            parse_redirect("/auth/callback?state=xyz", "xyz"),
            Err(CodexAuthError::OAuthRedirect(_))
        ));
        assert!(matches!(
            parse_redirect("/other?code=abc&state=xyz", "xyz"),
            Err(CodexAuthError::OAuthRedirect(_))
        ));
    }

    #[test]
    fn auth_file_from_token_response_round_trips_through_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = CodexCredentialStore::new(&path);
        let id_token = jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-oauth",
                "chatgpt_account_is_fedramp": true
            }
        }));
        let token = TokenExchangeResponse {
            access_token: Some(jwt(serde_json::json!({ "exp": now_unix() + 3_600 }))),
            refresh_token: Some("refresh-oauth".to_owned()),
            id_token: Some(id_token),
            error: None,
            error_description: None,
        };
        let file = auth_file_from_token_response(token, &path).unwrap();
        assert_eq!(
            file.tokens.as_ref().unwrap().account_id.as_deref(),
            Some("account-oauth")
        );
        store.save(&file).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.auth_mode.as_deref(), Some("chatgpt"));
        let credentials = credentials_from_file(&loaded, &path).unwrap();
        assert_eq!(credentials.account_id, "account-oauth");
        assert!(credentials.is_fedramp);
    }

    #[test]
    fn login_refuses_to_overwrite_valid_credentials_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let store = CodexCredentialStore::new(dir.path().join("auth.json"));
        store.save(&auth_file(now_unix() + 3_600)).unwrap();
        assert!(matches!(
            ensure_overwrite_allowed(&store, false),
            Err(CodexAuthError::CredentialsExist { .. })
        ));
        assert!(ensure_overwrite_allowed(&store, true).is_ok());
        // A corrupt file is not valid credentials, so overwriting is allowed.
        fs::write(store.path(), "not json").unwrap();
        assert!(ensure_overwrite_allowed(&store, false).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exchanges_authorization_code_with_mocked_token_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let id_token = jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-exchange",
                "chatgpt_account_is_fedramp": false
            }
        }));
        let response_id_token = id_token.clone();
        let access_token = jwt(serde_json::json!({ "exp": now_unix() + 3_600 }));
        let response_access = access_token.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = vec![0_u8; 8 * 1024];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("POST /token HTTP/1.1"));
            assert!(request.contains("grant_type=authorization_code"));
            assert!(request.contains("code=the-code"));
            assert!(request.contains("code_verifier=the-verifier"));
            assert!(request.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
            assert!(
                request.contains("redirect_uri=http%3A%2F%2Flocalhost%3A9999%2Fauth%2Fcallback")
            );
            let body = serde_json::json!({
                "access_token": response_access,
                "refresh_token": "refresh-exchange",
                "id_token": response_id_token,
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = reqwest::Client::new();
        let token = exchange_code(
            &client,
            &format!("http://{address}/token"),
            "the-code",
            "the-verifier",
            "http://localhost:9999/auth/callback",
        )
        .await
        .unwrap();
        assert_eq!(token.access_token.as_deref(), Some(access_token.as_str()));
        assert_eq!(token.refresh_token.as_deref(), Some("refresh-exchange"));
        assert_eq!(token.id_token.as_deref(), Some(id_token.as_str()));
        server.await.unwrap();
    }

    #[test]
    fn authorize_url_matches_the_official_cli_contract() {
        let url = build_authorize_url(
            "https://auth.openai.com/oauth/authorize",
            "http://localhost:1455/auth/callback",
            "challenge",
            "state",
        );
        for expected in [
            "response_type=code",
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
            "scope=openid%20profile%20email%20offline_access",
            "code_challenge=challenge",
            "code_challenge_method=S256",
            "state=state",
            "id_token_add_organizations=true",
            "codex_cli_simplified_flow=true",
            "originator=lam-agent",
        ] {
            assert!(
                url.contains(expected),
                "authorize URL missing {expected}: {url}"
            );
        }
        assert!(
            !url.contains("audience="),
            "official client sends no audience: {url}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn login_runs_the_oauth_flow_against_local_listeners() {
        let dir = tempfile::tempdir().unwrap();
        let store = CodexCredentialStore::new(dir.path().join("auth.json"));

        let redirect_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_address = redirect_listener.local_addr().unwrap();
        let token_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_address = token_listener.local_addr().unwrap();

        let verifier = pkce_verifier().unwrap();
        let state = oauth_state().unwrap();
        let redirect_uri = format!("http://localhost:{}/auth/callback", redirect_address.port());
        let expected_verifier_param = format!("code_verifier={verifier}");
        let expected_redirect_param = format!("redirect_uri={}", url_encode(&redirect_uri));

        let id_token = jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-e2e",
                "chatgpt_account_is_fedramp": false
            }
        }));
        let response_id_token = id_token.clone();
        let access_token = jwt(serde_json::json!({ "exp": now_unix() + 3_600 }));
        let response_access = access_token.clone();

        let redirect_request = format!(
            "GET /auth/callback?code=e2e-code&state={state} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        );
        let browser = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(redirect_address)
                .await
                .unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            stream.write_all(redirect_request.as_bytes()).await.unwrap();
            let mut response = vec![0_u8; 4096];
            let _ = stream.read(&mut response).await;
        });

        let token_server = tokio::spawn(async move {
            let (mut stream, _) = token_listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = vec![0_u8; 8 * 1024];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("POST /token HTTP/1.1"));
            assert!(request.contains("grant_type=authorization_code"));
            assert!(request.contains("code=e2e-code"));
            assert!(request.contains(&expected_verifier_param));
            assert!(request.contains(&expected_redirect_param));
            let body = serde_json::json!({
                "access_token": response_access,
                "refresh_token": "refresh-e2e",
                "id_token": response_id_token,
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let endpoints = OAuthEndpoints {
            authorize_base: "https://auth.example.test/oauth/authorize".to_owned(),
            token_url: format!("http://{token_address}/token"),
        };
        login_with(
            store.clone(),
            redirect_listener,
            &endpoints,
            true,
            false,
            &verifier,
            &state,
        )
        .await
        .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(
            loaded.tokens.as_ref().unwrap().account_id.as_deref(),
            Some("account-e2e")
        );
        let credentials = credentials_from_file(&loaded, store.path()).unwrap();
        assert_eq!(credentials.access_token, access_token);
        assert_eq!(credentials.account_id, "account-e2e");

        browser.await.unwrap();
        token_server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refreshes_and_persists_expiring_tokens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let next_access = jwt(serde_json::json!({ "exp": now_unix() + 3_600 }));
        let response_access = next_access.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = vec![0_u8; 8 * 1024];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("POST /refresh HTTP/1.1"));
            assert!(request.contains("refresh-old"));
            let body = serde_json::json!({
                "access_token": response_access,
                "refresh_token": "refresh-new"
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "lam-codex-auth-test-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let store = CodexCredentialStore::new(&path);
        store.save(&auth_file(now_unix() + 30)).unwrap();
        let session =
            CodexAuthSession::with_refresh_url(store.clone(), format!("http://{address}/refresh"));
        let credentials = session.credentials().await.unwrap();
        assert_eq!(credentials.access_token, next_access);
        let saved = store.load().unwrap();
        assert_eq!(saved.tokens.as_ref().unwrap().refresh_token, "refresh-new");
        server.await.unwrap();
        let _ = fs::remove_file(path);
    }
}
