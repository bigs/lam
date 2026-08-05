use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lam_redb::RedbStore;
use redb::{Database, ReadOnlyDatabase, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const SESSIONS_DIR: &str = "sessions";
const INDEX_FILE: &str = "index.redb";
const INDEX_LOCK_FILE: &str = "index.lock";
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

impl Session {
    pub(crate) fn diagnostic_log_path(&self) -> PathBuf {
        self.database_path.with_extension("debug.jsonl")
    }
}

/// Whether startup selected an existing session or created a fresh one.
pub(crate) struct SessionSelection {
    pub(crate) session: Session,
    pub(crate) resumed: bool,
    pub(crate) lease: SessionLease,
}

/// Durable index of TUI sessions and the latest session for each working directory.
pub(crate) struct SessionCatalog {
    index_path: PathBuf,
    lock_path: PathBuf,
    sessions_dir: PathBuf,
}

/// One catalog row for the session picker: identity plus the cached
/// first-user-message preview, so listing sessions never opens their
/// journals.
pub(crate) struct SessionListing {
    pub(crate) session: Session,
    pub(crate) preview: Option<String>,
}

/// An exclusive, process-scoped claim on a session journal.
pub(crate) struct SessionLease {
    _file: File,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionRecord {
    schema_version: u32,
    id: u64,
    cwd: String,
    created_at_unix_ms: u64,
    last_opened_at_unix_ms: u64,
    /// First user message, cached once discovered. A session's first message
    /// never changes, so the cache never invalidates. Absent on records
    /// written before this field existed and on sessions with no user
    /// message yet.
    #[serde(default)]
    preview: Option<String>,
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

        let catalog = Self {
            index_path: sessions_dir.join(INDEX_FILE),
            lock_path: sessions_dir.join(INDEX_LOCK_FILE),
            sessions_dir,
        };
        catalog.with_write_database(|database| {
            let write = database.begin_write().map_err(database_error)?;
            {
                write.open_table(META).map_err(database_error)?;
                write.open_table(LATEST_BY_CWD).map_err(database_error)?;
                write.open_table(SESSIONS).map_err(database_error)?;
            }
            write.commit().map_err(database_error)
        })?;
        Ok(catalog)
    }

    pub(crate) fn resume_or_create(&self, cwd: &Path) -> Result<SessionSelection, SessionError> {
        let cwd = cwd_key(cwd)?;
        self.with_write_database(|database| {
            let current = {
                let read = database.begin_read().map_err(database_error)?;
                let latest = read.open_table(LATEST_BY_CWD).map_err(database_error)?;
                latest
                    .get(cwd.as_str())
                    .map_err(database_error)?
                    .map(|id| id.value())
            };

            if let Some(id) = current {
                let session = self.session(id, &cwd);
                match SessionLease::acquire(&session) {
                    Ok(lease) => {
                        let session = self.load_and_touch(database, id, &cwd, false)?;
                        return Ok(SessionSelection {
                            session,
                            resumed: true,
                            lease,
                        });
                    }
                    Err(SessionError::SessionInUse { .. }) => {}
                    Err(error) => return Err(error),
                }
            }

            let (session, lease) = self.create_for_key(database, cwd)?;
            Ok(SessionSelection {
                session,
                resumed: false,
                lease,
            })
        })
    }

    pub(crate) fn create(&self, cwd: &Path) -> Result<(Session, SessionLease), SessionError> {
        let cwd = cwd_key(cwd)?;
        self.with_write_database(|database| self.create_for_key(database, cwd))
    }

    pub(crate) fn list(&self, cwd: &Path) -> Result<Vec<SessionListing>, SessionError> {
        let cwd = cwd_key(cwd)?;
        self.with_read_database(|database| {
            let read = database.begin_read().map_err(database_error)?;
            let sessions = read.open_table(SESSIONS).map_err(database_error)?;
            let mut matches = Vec::new();
            for item in sessions.iter().map_err(database_error)? {
                let (id, encoded) = item.map_err(database_error)?;
                let id = id.value();
                let record = serde_json::from_slice::<SessionRecord>(encoded.value())
                    .map_err(SessionError::Serialize)?;
                validate_record(&record, id)?;
                if record.cwd == cwd {
                    matches.push(SessionListing {
                        session: self.session(id, &cwd),
                        preview: record.preview,
                    });
                }
            }
            matches.sort_unstable_by_key(|listing| std::cmp::Reverse(listing.session.id));
            Ok(matches)
        })
    }

    /// Caches a session's first-user-message preview in its catalog record so
    /// later listings serve it from the index instead of scanning the
    /// session journal.
    pub(crate) fn store_preview(&self, id: u64, preview: &str) -> Result<(), SessionError> {
        self.with_write_database(|database| {
            let record = {
                let read = database.begin_read().map_err(database_error)?;
                let sessions = read.open_table(SESSIONS).map_err(database_error)?;
                let encoded = sessions
                    .get(id)
                    .map_err(database_error)?
                    .ok_or(SessionError::MissingRecord { id })?;
                serde_json::from_slice::<SessionRecord>(encoded.value())
                    .map_err(SessionError::Serialize)?
            };
            validate_record(&record, id)?;
            let updated = SessionRecord {
                preview: Some(preview.to_owned()),
                ..record
            };
            let encoded = serde_json::to_vec(&updated).map_err(SessionError::Serialize)?;
            let write = database.begin_write().map_err(database_error)?;
            {
                let mut sessions = write.open_table(SESSIONS).map_err(database_error)?;
                sessions
                    .insert(id, encoded.as_slice())
                    .map_err(database_error)?;
            }
            write.commit().map_err(database_error)
        })
    }

    pub(crate) fn select(
        &self,
        id: u64,
        cwd: &Path,
    ) -> Result<(Session, SessionLease), SessionError> {
        let cwd = cwd_key(cwd)?;
        self.with_write_database(|database| {
            let session = self.session(id, &cwd);
            let lease = SessionLease::acquire(&session)?;
            let session = self.load_and_touch(database, id, &cwd, true)?;
            Ok((session, lease))
        })
    }

    /// Drops a session from the catalog and removes its files. The session
    /// must belong to `cwd` and must not be open anywhere: the running TUI
    /// holds its own lease, so its active session is refused here too.
    pub(crate) fn delete(&self, id: u64, cwd: &Path) -> Result<(), SessionError> {
        let cwd = cwd_key(cwd)?;
        self.with_write_database(|database| {
            let record = {
                let read = database.begin_read().map_err(database_error)?;
                let sessions = read.open_table(SESSIONS).map_err(database_error)?;
                let encoded = sessions
                    .get(id)
                    .map_err(database_error)?
                    .ok_or(SessionError::MissingRecord { id })?;
                serde_json::from_slice::<SessionRecord>(encoded.value())
                    .map_err(SessionError::Serialize)?
            };
            validate_record(&record, id)?;
            if record.cwd != cwd {
                return Err(SessionError::WrongWorkingDirectory { id });
            }

            let session = self.session(id, &cwd);
            // Claim the journal for the deletion: another TUI holding the
            // lease is still using this session, and removing its files would
            // strand it.
            let lease = SessionLease::acquire(&session)?;
            let write = database.begin_write().map_err(database_error)?;
            {
                let mut sessions = write.open_table(SESSIONS).map_err(database_error)?;
                sessions.remove(id).map_err(database_error)?;
                let mut latest = write.open_table(LATEST_BY_CWD).map_err(database_error)?;
                let was_latest = latest
                    .get(cwd.as_str())
                    .map_err(database_error)?
                    .is_some_and(|current| current.value() == id);
                if was_latest {
                    match newest_for_cwd(&sessions, &cwd)? {
                        Some(newest) => {
                            latest
                                .insert(cwd.as_str(), newest)
                                .map_err(database_error)?;
                        }
                        None => {
                            latest.remove(cwd.as_str()).map_err(database_error)?;
                        }
                    }
                }
            }
            write.commit().map_err(database_error)?;

            // The index no longer names this session and this operation still
            // holds the index lock, so releasing the lease before removing its
            // own file opens no window for another TUI to claim the session.
            drop(lease);
            remove_session_files(&session);
            Ok(())
        })
    }

    fn create_for_key(
        &self,
        database: &Database,
        cwd: String,
    ) -> Result<(Session, SessionLease), SessionError> {
        let now = now_unix_ms()?;
        let write = database.begin_write().map_err(database_error)?;
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

        // Create and claim the journal before publishing the catalog entry. If
        // either step fails, dropping the index transaction leaves the previous
        // cwd selection untouched.
        RedbStore::create(&session.database_path).map_err(SessionError::Journal)?;
        let lease = SessionLease::acquire(&session)?;

        let record = SessionRecord {
            schema_version: INDEX_SCHEMA_VERSION,
            id,
            cwd: cwd.clone(),
            created_at_unix_ms: now,
            last_opened_at_unix_ms: now,
            preview: None,
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
        Ok((session, lease))
    }

    fn load_and_touch(
        &self,
        database: &Database,
        id: u64,
        cwd: &str,
        make_latest: bool,
    ) -> Result<Session, SessionError> {
        let record = {
            let read = database.begin_read().map_err(database_error)?;
            let sessions = read.open_table(SESSIONS).map_err(database_error)?;
            let encoded = sessions
                .get(id)
                .map_err(database_error)?
                .ok_or(SessionError::MissingRecord { id })?;
            serde_json::from_slice::<SessionRecord>(encoded.value())
                .map_err(SessionError::Serialize)?
        };
        validate_record(&record, id)?;
        if record.cwd != cwd {
            return Err(SessionError::WrongWorkingDirectory { id });
        }

        let session = self.session(id, cwd);
        if !session.database_path.is_file() {
            return Err(SessionError::MissingJournal {
                id,
                path: session.database_path,
            });
        }

        let updated = SessionRecord {
            last_opened_at_unix_ms: now_unix_ms()?,
            ..record
        };
        let encoded = serde_json::to_vec(&updated).map_err(SessionError::Serialize)?;
        let write = database.begin_write().map_err(database_error)?;
        {
            let mut sessions = write.open_table(SESSIONS).map_err(database_error)?;
            sessions
                .insert(id, encoded.as_slice())
                .map_err(database_error)?;
            if make_latest {
                let mut latest = write.open_table(LATEST_BY_CWD).map_err(database_error)?;
                latest.insert(cwd, id).map_err(database_error)?;
            }
        }
        write.commit().map_err(database_error)?;
        Ok(session)
    }

    fn session(&self, id: u64, cwd: &str) -> Session {
        Session {
            id,
            cwd: PathBuf::from(cwd),
            database_path: self.sessions_dir.join(format!("session-{id:08}.redb")),
        }
    }

    fn with_write_database<T>(
        &self,
        operation: impl FnOnce(&Database) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let _lock = self.lock_index(false)?;
        let database = Database::create(&self.index_path).map_err(database_error)?;
        operation(&database)
    }

    fn with_read_database<T>(
        &self,
        operation: impl FnOnce(&ReadOnlyDatabase) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let _lock = self.lock_index(true)?;
        let database = ReadOnlyDatabase::open(&self.index_path).map_err(database_error)?;
        operation(&database)
    }

    fn lock_index(&self, shared: bool) -> Result<File, SessionError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|source| SessionError::Lock {
                path: self.lock_path.clone(),
                source,
            })?;
        let result = if shared {
            file.lock_shared()
        } else {
            file.lock()
        };
        result.map_err(|source| SessionError::Lock {
            path: self.lock_path.clone(),
            source,
        })?;
        Ok(file)
    }
}

impl SessionLease {
    fn acquire(session: &Session) -> Result<Self, SessionError> {
        let path = session.database_path.with_extension("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| SessionError::Lock {
                path: path.clone(),
                source,
            })?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => SessionError::SessionInUse { id: session.id },
            std::fs::TryLockError::Error(source) => SessionError::Lock {
                path: path.clone(),
                source,
            },
        })?;
        Ok(Self { _file: file })
    }
}

/// The newest session still recorded for `cwd`, or `None` when the directory
/// has none left. Identifiers ascend with creation, so iteration order makes
/// the last match the newest — the same order the picker lists sessions in.
fn newest_for_cwd(
    sessions: &impl ReadableTable<u64, &'static [u8]>,
    cwd: &str,
) -> Result<Option<u64>, SessionError> {
    let mut newest = None;
    for item in sessions.iter().map_err(database_error)? {
        let (id, encoded) = item.map_err(database_error)?;
        let id = id.value();
        let record = serde_json::from_slice::<SessionRecord>(encoded.value())
            .map_err(SessionError::Serialize)?;
        validate_record(&record, id)?;
        if record.cwd == cwd {
            newest = Some(id);
        }
    }
    Ok(newest)
}

/// Removes a deleted session's journal, its lease file, and its diagnostic
/// log. The index is the source of truth and no longer names the session, so
/// every removal is best-effort: leftover files are inert.
fn remove_session_files(session: &Session) {
    for path in [
        session.database_path.clone(),
        session.database_path.with_extension("lock"),
        session.diagnostic_log_path(),
    ] {
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                target: "lam_tui::session",
                session_id = session.id,
                path = %path.display(),
                %error,
                "deleted session file could not be removed"
            );
        }
    }
}

fn validate_record(record: &SessionRecord, id: u64) -> Result<(), SessionError> {
    if record.schema_version != INDEX_SCHEMA_VERSION || record.id != id {
        return Err(SessionError::InvalidRecord { id });
    }
    Ok(())
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
    #[error("could not lock session state `{path}`: {source}")]
    Lock {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("session {id} is open in another TUI")]
    SessionInUse { id: u64 },
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
    #[error("session {id} belongs to a different working directory")]
    WrongWorkingDirectory { id: u64 },
    #[error("session {id} journal is missing at `{path}`")]
    MissingJournal { id: u64, path: PathBuf },
    #[error("could not create session journal: {0}")]
    Journal(#[source] lam_redb::RedbStoreError),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{SessionCatalog, SessionError};

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
        let first_session = first.session.clone();

        drop(first);
        drop(catalog);
        let catalog = SessionCatalog::open(&lam_dir).unwrap();
        let resumed = catalog.resume_or_create(&first_cwd).unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.session, first_session);

        let other = catalog.resume_or_create(&second_cwd).unwrap();
        assert!(!other.resumed);
        assert_ne!(other.session.id, first_session.id);
        assert_eq!(other.session.cwd, second_cwd);
    }

    #[test]
    fn new_session_advances_the_index_and_becomes_current() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let catalog = SessionCatalog::open(temp.path().join("lam-home")).unwrap();

        let initial = catalog.resume_or_create(&cwd).unwrap().session;
        let (fresh, fresh_lease) = catalog.create(&cwd).unwrap();
        assert_eq!(fresh.id, initial.id + 1);
        assert_ne!(fresh.database_path, initial.database_path);
        assert!(initial.database_path.is_file());
        assert!(fresh.database_path.is_file());

        drop(fresh_lease);
        let resumed = catalog.resume_or_create(&cwd).unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.session, fresh);
    }

    #[test]
    fn lists_newest_first_and_selects_an_older_session_for_the_same_directory() {
        let temp = tempfile::tempdir().unwrap();
        let first_cwd = temp.path().join("first");
        let second_cwd = temp.path().join("second");
        fs::create_dir_all(&first_cwd).unwrap();
        fs::create_dir_all(&second_cwd).unwrap();
        let first_cwd = first_cwd.canonicalize().unwrap();
        let second_cwd = second_cwd.canonicalize().unwrap();
        let catalog = SessionCatalog::open(temp.path().join("lam-home")).unwrap();

        let (oldest, oldest_lease) = catalog.create(&first_cwd).unwrap();
        let (newest, newest_lease) = catalog.create(&first_cwd).unwrap();
        let (_, other_lease) = catalog.create(&second_cwd).unwrap();

        assert_eq!(
            catalog
                .list(&first_cwd)
                .unwrap()
                .into_iter()
                .map(|listing| listing.session)
                .collect::<Vec<_>>(),
            vec![newest, oldest.clone()]
        );
        drop((oldest_lease, newest_lease, other_lease));
        let (selected, selected_lease) = catalog.select(oldest.id, &first_cwd).unwrap();
        assert_eq!(selected, oldest);
        drop(selected_lease);
        assert_eq!(
            catalog.resume_or_create(&first_cwd).unwrap().session.id,
            oldest.id
        );
    }

    #[test]
    fn concurrent_catalogs_share_the_index_without_sharing_active_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let lam_dir = temp.path().join("lam-home");
        let first_catalog = SessionCatalog::open(&lam_dir).unwrap();
        let second_catalog = SessionCatalog::open(&lam_dir).unwrap();

        let first = first_catalog.resume_or_create(&cwd).unwrap();
        let second = second_catalog.resume_or_create(&cwd).unwrap();

        assert_ne!(first.session.id, second.session.id);
        assert_eq!(
            first_catalog
                .list(&cwd)
                .unwrap()
                .into_iter()
                .map(|listing| listing.session)
                .collect::<Vec<_>>(),
            vec![second.session.clone(), first.session.clone()]
        );
    }

    #[test]
    fn previews_are_cached_in_the_index_once_stored() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let catalog = SessionCatalog::open(temp.path().join("lam-home")).unwrap();

        let (session, _lease) = catalog.create(&cwd).unwrap();
        let listed = catalog.list(&cwd).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].preview, None, "a fresh session has no preview");

        catalog.store_preview(session.id, "first message").unwrap();
        let listed = catalog.list(&cwd).unwrap();
        assert_eq!(listed[0].preview.as_deref(), Some("first message"));

        assert!(
            catalog.store_preview(9_999, "orphan").is_err(),
            "previews only attach to existing records"
        );
    }

    #[test]
    fn delete_drops_the_record_and_removes_the_session_files() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let catalog = SessionCatalog::open(temp.path().join("lam-home")).unwrap();

        let (session, lease) = catalog.create(&cwd).unwrap();
        fs::write(session.diagnostic_log_path(), b"{}\n").unwrap();
        drop(lease);

        catalog.delete(session.id, &cwd).unwrap();

        assert!(catalog.list(&cwd).unwrap().is_empty());
        assert!(!session.database_path.exists());
        assert!(!session.database_path.with_extension("lock").exists());
        assert!(!session.diagnostic_log_path().exists());
        assert!(
            matches!(
                catalog.delete(session.id, &cwd),
                Err(SessionError::MissingRecord { id }) if id == session.id
            ),
            "a deleted session has no record left to delete"
        );
    }

    #[test]
    fn deleting_the_current_session_repoints_the_directory_at_the_newest_remaining() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let catalog = SessionCatalog::open(temp.path().join("lam-home")).unwrap();

        let (oldest, oldest_lease) = catalog.create(&cwd).unwrap();
        let (middle, middle_lease) = catalog.create(&cwd).unwrap();
        let (newest, newest_lease) = catalog.create(&cwd).unwrap();
        drop((oldest_lease, middle_lease, newest_lease));

        catalog.delete(newest.id, &cwd).unwrap();
        let resumed = catalog.resume_or_create(&cwd).unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.session.id, middle.id);
        drop(resumed);

        catalog.delete(middle.id, &cwd).unwrap();
        let resumed = catalog.resume_or_create(&cwd).unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.session.id, oldest.id);
        drop(resumed);

        // The last session for the directory leaves no selection behind.
        catalog.delete(oldest.id, &cwd).unwrap();
        let fresh = catalog.resume_or_create(&cwd).unwrap();
        assert!(!fresh.resumed);
        assert_eq!(catalog.list(&cwd).unwrap().len(), 1);
    }

    /// The replacement path in the TUI: create the successor, release the old
    /// lease, then delete the session it replaced.
    #[test]
    fn replacing_the_only_session_leaves_the_successor_current() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let catalog = SessionCatalog::open(temp.path().join("lam-home")).unwrap();

        let replaced = catalog.resume_or_create(&cwd).unwrap();
        let (successor, successor_lease) = catalog.create(&cwd).unwrap();
        drop(replaced.lease);
        catalog.delete(replaced.session.id, &cwd).unwrap();

        assert_eq!(
            catalog
                .list(&cwd)
                .unwrap()
                .into_iter()
                .map(|listing| listing.session.id)
                .collect::<Vec<_>>(),
            vec![successor.id]
        );
        assert!(!replaced.session.database_path.exists());
        // Creating the successor already claimed the directory, so deleting the
        // session it replaced must not repoint it.
        drop(successor_lease);
        let resumed = catalog.resume_or_create(&cwd).unwrap();
        assert!(resumed.resumed);
        assert_eq!(resumed.session.id, successor.id);
    }

    #[test]
    fn delete_refuses_a_session_held_by_another_lease() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let catalog = SessionCatalog::open(temp.path().join("lam-home")).unwrap();

        let (session, lease) = catalog.create(&cwd).unwrap();
        assert!(matches!(
            catalog.delete(session.id, &cwd),
            Err(SessionError::SessionInUse { id }) if id == session.id
        ));

        assert!(session.database_path.is_file());
        assert_eq!(
            catalog
                .list(&cwd)
                .unwrap()
                .into_iter()
                .map(|listing| listing.session.id)
                .collect::<Vec<_>>(),
            vec![session.id]
        );
        drop(lease);
        assert_eq!(
            catalog.resume_or_create(&cwd).unwrap().session.id,
            session.id
        );
    }

    #[test]
    fn delete_refuses_a_session_from_another_directory() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        let other_cwd = temp.path().join("elsewhere");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other_cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let other_cwd = other_cwd.canonicalize().unwrap();
        let catalog = SessionCatalog::open(temp.path().join("lam-home")).unwrap();

        let (session, lease) = catalog.create(&cwd).unwrap();
        drop(lease);

        assert!(matches!(
            catalog.delete(session.id, &other_cwd),
            Err(SessionError::WrongWorkingDirectory { id }) if id == session.id
        ));
        assert!(session.database_path.is_file());
        assert_eq!(catalog.list(&cwd).unwrap().len(), 1);
    }
}
