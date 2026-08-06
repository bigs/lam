//! ChatGPT subscription authentication through the official Codex login cache.

use std::env;
use std::fs;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use lam_openai::{AuthSource, SharedAuthSource, bearer_header};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::Mutex;

use lam_openai::ProviderError;

pub(crate) const CODEX_BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
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
        let temp = self
            .path
            .with_extension(format!("json.lam.{}.tmp", std::process::id()));
        {
            let mut output = fs::File::create(&temp).map_err(|source| CodexAuthError::Write {
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
        })?;
        Ok(())
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
        Self::with_refresh_url(store, REFRESH_TOKEN_URL)
    }

    fn with_refresh_url(store: CodexCredentialStore, refresh_url: impl Into<String>) -> Self {
        let mut client_headers = HeaderMap::new();
        client_headers.insert("originator", HeaderValue::from_static("lam-agent"));
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
            self.refresh(&mut file).await?;
            self.store.save(&file)?;
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

pub(crate) fn installed_client_version() -> Result<&'static str, CodexAuthError> {
    static VERSION: OnceLock<Result<String, String>> = OnceLock::new();
    VERSION
        .get_or_init(detect_installed_client_version)
        .as_deref()
        .map_err(|message| CodexAuthError::ClientVersion(message.clone()))
}

fn detect_installed_client_version() -> Result<String, String> {
    let output = Command::new("codex")
        .arg("--version")
        .output()
        .map_err(|error| format!("could not run `codex --version`: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`codex --version` exited with status {:?}",
            output.status.code()
        ));
    }
    parse_client_version(&String::from_utf8_lossy(&output.stdout))
        .map(str::to_owned)
        .ok_or_else(|| "`codex --version` did not return `codex-cli VERSION`".to_owned())
}

fn parse_client_version(output: &str) -> Option<&str> {
    let mut fields = output.split_whitespace();
    if fields.next()? != "codex-cli" {
        return None;
    }
    let version = fields.next()?;
    (version.starts_with(|character: char| character.is_ascii_digit())
        && version.matches('.').count() >= 2
        && version.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+')
        }))
    .then_some(version)
}

pub(crate) fn default_headers(
    credentials: &CodexCredentials,
    client_version: &str,
) -> Result<HeaderMap, CodexAuthError> {
    let mut headers = HeaderMap::new();
    // The ChatGPT Codex backend gates model routing on the compatibility
    // identity of the official client that owns the shared login. Use the
    // installed CLI version instead of Lam's unrelated package version.
    headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
    headers.insert(
        "version",
        HeaderValue::from_str(client_version).map_err(CodexAuthError::InvalidHeader)?,
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&format!("codex_cli_rs/{client_version} (lam-agent)"))
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

pub(crate) fn login(no_browser: bool) -> Result<CodexCredentialStore, CodexAuthError> {
    let mut command = Command::new("codex");
    command.args(["login", "--config", "cli_auth_credentials_store=\"file\""]);
    if no_browser {
        command.arg("--device-auth");
    }
    let status = command.status().map_err(CodexAuthError::CodexCommand)?;
    if !status.success() {
        return Err(CodexAuthError::CodexCommandStatus(status.code()));
    }
    let store = CodexCredentialStore::default_store()?;
    store.load()?;
    Ok(store)
}

pub(crate) fn logout() -> Result<(), CodexAuthError> {
    let status = Command::new("codex")
        .args(["logout", "--config", "cli_auth_credentials_store=\"file\""])
        .status()
        .map_err(CodexAuthError::CodexCommand)?;
    if status.success() {
        Ok(())
    } else {
        Err(CodexAuthError::CodexCommandStatus(status.code()))
    }
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
    #[error("could not determine the installed Codex client version: {0}")]
    ClientVersion(String),
    #[error("could not run the official `codex` CLI: {0}")]
    CodexCommand(std::io::Error),
    #[error("the official `codex` CLI exited unsuccessfully (status {0:?})")]
    CodexCommandStatus(Option<i32>),
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
        let headers = default_headers(&credentials, "0.146.0").unwrap();
        assert_eq!(headers["ChatGPT-Account-ID"], "account-123");
        assert_eq!(headers["originator"], "codex_cli_rs");
        assert_eq!(headers["version"], "0.146.0");
        assert_eq!(headers[USER_AGENT], "codex_cli_rs/0.146.0 (lam-agent)");
    }

    #[test]
    fn parses_official_codex_client_versions() {
        assert_eq!(parse_client_version("codex-cli 0.146.0\n"), Some("0.146.0"));
        assert_eq!(
            parse_client_version("codex-cli 0.147.0-alpha.3\n"),
            Some("0.147.0-alpha.3")
        );
        assert_eq!(parse_client_version("lam-agent 0.1.0\n"), None);
        assert_eq!(parse_client_version("codex-cli invalid\n"), None);
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
