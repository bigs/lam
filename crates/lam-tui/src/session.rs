use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lam_redb::RedbStore;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const SESSIONS_DIR: &str = "sessions";
const INDEX_FILE: &str = "index.redb";
const NEXT_ID: &str = "next_session_id";
const INDEX_SCHEMA_VERSION: u32 = 1;

const META: TableDefinition<&str, u64> = TableDefinition::new("lam_tui_session_meta_v1");
const LATEST_BY_CWD: TableDefinition<&str, u64> =
    TableDefinition::new("lam_tui_latest_session_by_cwd_v1");
const SESSIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("lam_tui_sessions_v1");

/// One durable TUI session selected from the cwd-scoped catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Session {
    pub(crate) id: u64,
    pub(crate) cwd: PathBuf,
    pub(crate) database_path: PathBuf,
}

/// Whether startup selected an existing session or created a fresh one.
pub(crate) struct SessionSelection {
    pub(crate) session: Session,
    pub(crate) resumed: bool,
}

/// Durable index of TUI sessions and the latest session for each working directory.
pub(crate) struct SessionCatalog {
    database: Database,
    sessions_dir: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionRecord {
    schema_version: u32,
    id: u64,
    cwd: String,
    created_at_unix_ms: u64,
    last_opened_at_unix_ms: u64,
}

impl SessionCatalog {
    pub(crate) fn open_default() -> Result<Self, SessionError> {
        Self::open(lam_dir()?)
    }

    fn open(lam_dir: impl AsRef<Path>) -> Result<Self, SessionError> {
        let sessions_dir = lam_dir.as_ref().join(SESSIONS_DIR);
        fs::create_dir_all(&sessions_dir).map_err(|source| SessionError::CreateDirectory {
            path: sessions_dir.clone(),
            source,
        })?;
        restrict_directory(&sessions_dir)?;

        let index_path = sessions_dir.join(INDEX_FILE);
        let database = Database::create(&index_path).map_err(database_error)?;
        let write = database.begin_write().map_err(database_error)?;
        {
            write.open_table(META).map_err(database_error)?;
            write.open_table(LATEST_BY_CWD).map_err(database_error)?;
            write.open_table(SESSIONS).map_err(database_error)?;
        }
        write.commit().map_err(database_error)?;
        Ok(Self {
            database,
            sessions_dir,
        })
    }

    pub(crate) fn resume_or_create(&self, cwd: &Path) -> Result<SessionSelection, SessionError> {
        let cwd = cwd_key(cwd)?;
        let current = {
            let read = self.database.begin_read().map_err(database_error)?;
            let latest = read.open_table(LATEST_BY_CWD).map_err(database_error)?;
            latest
                .get(cwd.as_str())
                .map_err(database_error)?
                .map(|id| id.value())
        };

        if let Some(id) = current {
            let session = self.load_and_touch(id, &cwd)?;
            return Ok(SessionSelection {
                session,
                resumed: true,
            });
        }

        Ok(SessionSelection {
            session: self.create_for_key(cwd)?,
            resumed: false,
        })
    }

    pub(crate) fn create(&self, cwd: &Path) -> Result<Session, SessionError> {
        self.create_for_key(cwd_key(cwd)?)
    }

    fn create_for_key(&self, cwd: String) -> Result<Session, SessionError> {
        let now = now_unix_ms()?;
        let write = self.database.begin_write().map_err(database_error)?;
        let id = {
            let mut meta = write.open_table(META).map_err(database_error)?;
            let id = meta
                .get(NEXT_ID)
                .map_err(database_error)?
                .map_or(1, |value| value.value());
            let next = id.checked_add(1).ok_or(SessionError::IdExhausted)?;
            meta.insert(NEXT_ID, next).map_err(database_error)?;
            id
        };
        let session = self.session(id, &cwd);

        // Create the journal before publishing the catalog entry. If opening the
        // session database fails, dropping the index transaction leaves the
        // previous cwd selection untouched.
        RedbStore::create(&session.database_path).map_err(SessionError::Journal)?;

        let record = SessionRecord {
            schema_version: INDEX_SCHEMA_VERSION,
            id,
            cwd: cwd.clone(),
            created_at_unix_ms: now,
            last_opened_at_unix_ms: now,
        };
        let encoded = serde_json::to_vec(&record).map_err(SessionError::Serialize)?;
        {
            let mut sessions = write.open_table(SESSIONS).map_err(database_error)?;
            sessions
                .insert(id, encoded.as_slice())
                .map_err(database_error)?;
            let mut latest = write.open_table(LATEST_BY_CWD).map_err(database_error)?;
            latest.insert(cwd.as_str(), id).map_err(database_error)?;
        }
        write.commit().map_err(database_error)?;
        Ok(session)
    }

    fn load_and_touch(&self, id: u64, cwd: &str) -> Result<Session, SessionError> {
        let record = {
            let read = self.database.begin_read().map_err(database_error)?;
            let sessions = read.open_table(SESSIONS).map_err(database_error)?;
            let encoded = sessions
                .get(id)
                .map_err(database_error)?
                .ok_or(SessionError::MissingRecord { id })?;
            serde_json::from_slice::<SessionRecord>(encoded.value())
                .map_err(SessionError::Serialize)?
        };
        if record.schema_version != INDEX_SCHEMA_VERSION || record.id != id || record.cwd != cwd {
            return Err(SessionError::InvalidRecord { id });
        }

        let updated = SessionRecord {
            last_opened_at_unix_ms: now_unix_ms()?,
            ..record
        };
        let encoded = serde_json::to_vec(&updated).map_err(SessionError::Serialize)?;
        let write = self.database.begin_write().map_err(database_error)?;
        {
            let mut sessions = write.open_table(SESSIONS).map_err(database_error)?;
            sessions
                .insert(id, encoded.as_slice())
                .map_err(database_error)?;
        }
        write.commit().map_err(database_error)?;

        let session = self.session(id, cwd);
        if !session.database_path.is_file() {
            return Err(SessionError::MissingJournal {
                id,
                path: session.database_path,
            });
        }
        Ok(session)
    }

    fn session(&self, id: u64, cwd: &str) -> Session {
        Session {
            id,
            cwd: PathBuf::from(cwd),
            database_path: self.sessions_dir.join(format!("session-{id:08}.redb")),
        }
    }
}

fn lam_dir() -> Result<PathBuf, SessionError> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or(SessionError::HomeUnavailable)?;
    Ok(PathBuf::from(home).join(".lam"))
}

fn cwd_key(cwd: &Path) -> Result<String, SessionError> {
    if !cwd.is_absolute() {
        return Err(SessionError::RelativeWorkingDirectory(cwd.to_path_buf()));
    }
    cwd.to_str()
        .map(str::to_owned)
        .ok_or_else(|| SessionError::NonUtf8WorkingDirectory(cwd.to_path_buf()))
}

fn now_unix_ms() -> Result<u64, SessionError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(SessionError::Clock)?
        .as_millis();
    u64::try_from(millis).map_err(|_| SessionError::ClockOverflow)
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        SessionError::Permissions {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<(), SessionError> {
    Ok(())
}

fn database_error(error: impl Into<redb::Error>) -> SessionError {
    SessionError::Database(error.into())
}

#[derive(Debug, Error)]
pub(crate) enum SessionError {
    #[error("could not determine the home directory for ~/.lam sessions")]
    HomeUnavailable,
    #[error("could not create session directory `{path}`: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not restrict session directory `{path}`: {source}")]
    Permissions {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("session working directory must be absolute: `{0}`")]
    RelativeWorkingDirectory(PathBuf),
    #[error("session working directory is not valid UTF-8: `{0}`")]
    NonUtf8WorkingDirectory(PathBuf),
    #[error("session index operation failed: {0}")]
    Database(#[source] redb::Error),
    #[error("session index serialization failed: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("session clock is before the Unix epoch: {0}")]
    Clock(#[source] std::time::SystemTimeError),
    #[error("session timestamp does not fit in a u64")]
    ClockOverflow,
    #[error("the session index has exhausted its identifiers")]
    IdExhausted,
    #[error("the session index points to missing session {id}")]
    MissingRecord { id: u64 },
    #[error("session index record {id} is inconsistent")]
    InvalidRecord { id: u64 },
    #[error("session {id} journal is missing at `{path}`")]
    MissingJournal { id: u64, path: PathBuf },
    #[error("could not create session journal: {0}")]
    Journal(#[source] lam_redb::RedbStoreError),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::SessionCatalog;

    #[test]
    fn resumes_the_latest_session_for_each_canonical_directory() {
        let temp = tempfile::tempdir().unwrap();
        let lam_dir = temp.path().join("lam-home");
        let first_cwd = temp.path().join("first");
        let second_cwd = temp.path().join("second");
        fs::create_dir_all(&first_cwd).unwrap();
        fs::create_dir_all(&second_cwd).unwrap();
        let first_cwd = first_cwd.canonicalize().unwrap();
        let second_cwd = second_cwd.canonicalize().unwrap();

        let catalog = SessionCatalog::open(&lam_dir).unwrap();
        let first = catalog.resume_or_create(&first_cwd).unwrap();
        assert!(!first.resumed);
        assert!(first.session.database_path.is_file());

        drop(catalog);
        let catalog = SessionCatalog::open(&lam_dir).unwrap();
        let resumed = catalog.resume_or_create(&first_cwd).unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.session, first.session);

        let other = catalog.resume_or_create(&second_cwd).unwrap();
        assert!(!other.resumed);
        assert_ne!(other.session.id, first.session.id);
        assert_eq!(other.session.cwd, second_cwd);
    }

    #[test]
    fn new_session_advances_the_index_and_becomes_current() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let catalog = SessionCatalog::open(temp.path().join("lam-home")).unwrap();

        let initial = catalog.resume_or_create(&cwd).unwrap().session;
        let fresh = catalog.create(&cwd).unwrap();
        assert_eq!(fresh.id, initial.id + 1);
        assert_ne!(fresh.database_path, initial.database_path);
        assert!(initial.database_path.is_file());
        assert!(fresh.database_path.is_file());

        let resumed = catalog.resume_or_create(&cwd).unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.session, fresh);
    }
}
