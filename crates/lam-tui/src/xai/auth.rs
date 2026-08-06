//! Durable SuperGrok OAuth credential storage under `~/.lam/auth/`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const AUTH_DIR: &str = ".lam/auth";
const AUTH_FILE: &str = "xai.json";
const SCHEMA_VERSION: u32 = 1;
/// Refresh a little early so requests never race the absolute expiry.
const REFRESH_SKEW: Duration = Duration::from_secs(120);

/// OAuth tokens for the shared Grok CLI / SuperGrok client.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct XaiCredentials {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    /// Unix epoch seconds when the access token should be treated as expired.
    pub(crate) expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) email: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthFile {
    version: u32,
    credentials: XaiCredentials,
}

/// File-backed SuperGrok credential store.
#[derive(Clone, Debug)]
pub(crate) struct XaiCredentialStore {
    path: PathBuf,
}

impl XaiCredentialStore {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn default_store() -> Result<Self, AuthError> {
        Ok(Self::new(default_auth_path()?))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> Result<Option<XaiCredentials>, AuthError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let source = fs::read_to_string(&self.path).map_err(|source| AuthError::Io {
            path: self.path.clone(),
            source,
        })?;
        let file: AuthFile =
            serde_json::from_str(&source).map_err(|source| AuthError::Parse {
                path: self.path.clone(),
                source,
            })?;
        if file.version != SCHEMA_VERSION {
            return Err(AuthError::UnsupportedVersion {
                path: self.path.clone(),
                version: file.version,
            });
        }
        Ok(Some(file.credentials))
    }

    pub(crate) fn save(&self, credentials: &XaiCredentials) -> Result<(), AuthError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| AuthError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let file = AuthFile {
            version: SCHEMA_VERSION,
            credentials: credentials.clone(),
        };
        let body = serde_json::to_vec_pretty(&file).map_err(|source| AuthError::Serialize {
            path: self.path.clone(),
            source,
        })?;
        let temp = self.path.with_extension("json.tmp");
        {
            let mut out = fs::File::create(&temp).map_err(|source| AuthError::Io {
                path: temp.clone(),
                source,
            })?;
            out.write_all(&body).map_err(|source| AuthError::Io {
                path: temp.clone(),
                source,
            })?;
            out.sync_all().map_err(|source| AuthError::Io {
                path: temp.clone(),
                source,
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&temp, fs::Permissions::from_mode(0o600));
            }
        }
        fs::rename(&temp, &self.path).map_err(|source| AuthError::Io {
            path: self.path.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub(crate) fn clear(&self) -> Result<(), AuthError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(AuthError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Loads credentials from the official Grok Build CLI when present.
    pub(crate) fn import_grok_cli() -> Result<Option<XaiCredentials>, AuthError> {
        let path = grok_cli_auth_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let source = fs::read_to_string(&path).map_err(|source| AuthError::Io {
            path: path.clone(),
            source,
        })?;
        let value: serde_json::Value =
            serde_json::from_str(&source).map_err(|source| AuthError::Parse {
                path: path.clone(),
                source,
            })?;
        let Some(object) = value.as_object() else {
            return Ok(None);
        };
        for entry in object.values() {
            let access = entry
                .get("key")
                .or_else(|| entry.get("access_token"))
                .and_then(serde_json::Value::as_str);
            let refresh = entry
                .get("refresh_token")
                .and_then(serde_json::Value::as_str);
            let (Some(access), Some(refresh)) = (access, refresh) else {
                continue;
            };
            let expires_at = entry
                .get("expires_at")
                .and_then(|value| {
                    value
                        .as_str()
                        .and_then(|text| {
                            // RFC3339 from Grok CLI.
                            httpdate_to_unix(text)
                        })
                        .or_else(|| value.as_u64())
                })
                .unwrap_or_else(|| now_unix().saturating_add(3_600));
            return Ok(Some(XaiCredentials {
                access_token: access.to_owned(),
                refresh_token: refresh.to_owned(),
                expires_at,
                token_type: Some("Bearer".to_owned()),
                scope: None,
                user_id: entry
                    .get("user_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                email: entry
                    .get("email")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            }));
        }
        Ok(None)
    }
}

impl XaiCredentials {
    pub(crate) fn from_token_response(
        access_token: String,
        refresh_token: String,
        expires_in: u64,
        token_type: Option<String>,
        scope: Option<String>,
    ) -> Self {
        let expires_at = now_unix()
            .saturating_add(expires_in)
            .saturating_sub(REFRESH_SKEW.as_secs());
        Self {
            access_token,
            refresh_token,
            expires_at,
            token_type,
            scope,
            user_id: None,
            email: None,
        }
    }

    pub(crate) fn is_expired(&self) -> bool {
        now_unix() >= self.expires_at
    }

}

pub(crate) fn default_auth_path() -> Result<PathBuf, AuthError> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or(AuthError::HomeUnavailable)?;
    Ok(PathBuf::from(home).join(AUTH_DIR).join(AUTH_FILE))
}

fn grok_cli_auth_path() -> Result<PathBuf, AuthError> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or(AuthError::HomeUnavailable)?;
    Ok(PathBuf::from(home).join(".grok").join("auth.json"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn httpdate_to_unix(value: &str) -> Option<u64> {
    // Prefer a minimal RFC3339 parser without a chrono dependency.
    // Example: 2026-08-06T19:41:52.910372Z
    let trimmed = value.trim().trim_end_matches('Z');
    let (date, time) = trimmed.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    let time = time.split('.').next()?;
    let mut time_parts = time.split(':');
    let hour: u32 = time_parts.next()?.parse().ok()?;
    let minute: u32 = time_parts.next()?.parse().ok()?;
    let second: u32 = time_parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }
    // Days from civil date (Howard Hinnant algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146_097 + doe as i64 - 719_468) as i64;
    let secs = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    u64::try_from(secs).ok()
}

#[derive(Debug, Error)]
pub(crate) enum AuthError {
    #[error("could not determine the home directory for SuperGrok credentials")]
    HomeUnavailable,
    #[error("could not read SuperGrok credentials at `{path}`: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("could not parse SuperGrok credentials at `{path}`: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("could not serialize SuperGrok credentials for `{path}`: {source}")]
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("unsupported SuperGrok credential schema version {version} in `{path}`")]
    UnsupportedVersion { path: PathBuf, version: u32 },
}

#[cfg(test)]
mod tests {
    use super::{XaiCredentialStore, XaiCredentials, httpdate_to_unix};
    use tempfile::tempdir;

    #[test]
    fn round_trips_credentials() {
        let dir = tempdir().unwrap();
        let store = XaiCredentialStore::new(dir.path().join("xai.json"));
        let credentials = XaiCredentials {
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at: 1_700_000_000,
            token_type: Some("Bearer".to_owned()),
            scope: Some("openid".to_owned()),
            user_id: Some("user".to_owned()),
            email: Some("user@example.com".to_owned()),
        };
        store.save(&credentials).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.refresh_token, "refresh");
        assert_eq!(loaded.user_id.as_deref(), Some("user"));
    }

    #[test]
    fn parses_rfc3339_expiry() {
        let stamp = httpdate_to_unix("2026-08-06T19:41:52.910372Z").unwrap();
        assert!(stamp > 1_700_000_000);
    }
}
