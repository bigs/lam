//! Durable [`JournalStore`] implementation backed by redb.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use lam_core::{
    ActorId, AppendOutcome, EventBatch, JournalError, JournalPage, JournalStore, Revision,
    StoredEvent,
};
use redb::{
    Database, ReadOnlyDatabase, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction,
};

const HEADS: TableDefinition<&str, u64> = TableDefinition::new("lam_actor_heads_v1");
const EVENTS: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("lam_actor_events_v1");
const CHECKPOINTS: TableDefinition<&str, (u64, &[u8])> =
    TableDefinition::new("lam_actor_checkpoints_v1");

/// Durable actor-journal storage in one redb database.
pub struct RedbStore {
    database: Database,
    path: PathBuf,
}

/// Sizing snapshot for teardown maintenance decisions.
#[derive(Clone, Copy, Debug)]
pub struct StoreFootprint {
    /// Total journal file length in bytes.
    pub file_bytes: u64,
    /// Bytes an in-place compaction could plausibly reclaim: file length
    /// beyond the pages currently allocated to live data.
    pub reclaimable_bytes: u64,
}

impl RedbStore {
    /// Creates a database when absent or opens an existing database.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, RedbStoreError> {
        let path = path.as_ref().to_path_buf();
        let database = Database::create(&path).map_err(database_error)?;
        Self::initialize(database, path)
    }

    /// Opens an existing database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RedbStoreError> {
        let path = path.as_ref().to_path_buf();
        let database = Database::open(&path).map_err(database_error)?;
        let read = database.begin_read().map_err(database_error)?;
        read.open_table(HEADS).map_err(database_error)?;
        read.open_table(EVENTS).map_err(database_error)?;
        // Databases written before the checkpoint table existed need it
        // created on open so every session can bootstrap from a checkpoint.
        // Checked read-only first: ordinary opens must not pay a write
        // commit (or dirty the last-commit state) just to probe a table.
        let needs_checkpoints = match read.open_table(CHECKPOINTS) {
            Ok(_) => false,
            Err(redb::TableError::TableDoesNotExist(_)) => true,
            Err(error) => return Err(database_error(error)),
        };
        drop(read);
        if needs_checkpoints {
            let write = begin_write(&database).map_err(database_error)?;
            {
                write.open_table(CHECKPOINTS).map_err(database_error)?;
            }
            write.commit().map_err(database_error)?;
        }
        Ok(Self { database, path })
    }

    fn initialize(database: Database, path: PathBuf) -> Result<Self, RedbStoreError> {
        let write = begin_write(&database).map_err(database_error)?;
        {
            write.open_table(HEADS).map_err(database_error)?;
            write.open_table(EVENTS).map_err(database_error)?;
            write.open_table(CHECKPOINTS).map_err(database_error)?;
        }
        write.commit().map_err(database_error)?;
        Ok(Self { database, path })
    }

    /// Lists actor journals in canonical actor-ID order.
    pub fn actor_ids(&self) -> Result<Vec<ActorId>, RedbStoreError> {
        let read = self.database.begin_read().map_err(database_error)?;
        let heads = read.open_table(HEADS).map_err(database_error)?;
        heads
            .iter()
            .map_err(database_error)?
            .map(|item| {
                let (actor, _) = item.map_err(database_error)?;
                ActorId::new(actor.value()).map_err(|error| RedbStoreError::ActorId {
                    message: error.to_string(),
                })
            })
            .collect()
    }

    /// Rewrites the database in place to reclaim freed pages and shrink the
    /// file toward its live content. Returns whether pages were relocated.
    ///
    /// Safe only when no other read or write transaction is active; teardown
    /// paths call this after the actor system has fully shut down. The
    /// operation is self-gating: a database with nothing to reclaim returns
    /// false quickly, so callers can invoke it unconditionally.
    pub fn compact(&mut self) -> Result<bool, RedbStoreError> {
        self.database
            .compact()
            .map_err(|error| RedbStoreError::Database(error.into()))
    }

    /// Measures the journal file against its live allocation so teardown
    /// paths can skip compaction when there is little to reclaim. Takes the
    /// write lock briefly; intended for quiescent teardown use.
    pub fn footprint(&self) -> Result<StoreFootprint, RedbStoreError> {
        let file_bytes = std::fs::metadata(&self.path)
            .map_err(RedbStoreError::Metadata)?
            .len();
        let write = self.database.begin_write().map_err(database_error)?;
        let stats = write.stats().map_err(database_error)?;
        let allocated = stats
            .allocated_pages()
            .saturating_mul(stats.page_size() as u64);
        write.abort().map_err(database_error)?;
        Ok(StoreFootprint {
            file_bytes,
            reclaimable_bytes: file_bytes.saturating_sub(allocated),
        })
    }

    fn read_page(
        &self,
        actor: &ActorId,
        after: Revision,
        limit: NonZeroUsize,
    ) -> Result<JournalPage, RedbStoreError> {
        read_page_from(&self.database, actor, after, limit)
    }

    fn append_batch(
        &self,
        actor: &ActorId,
        expected: Revision,
        events: EventBatch,
    ) -> Result<AppendOutcome, JournalError<RedbStoreError>> {
        let write = begin_write(&self.database).map_err(journal_database_error)?;
        let head = {
            let mut heads = write.open_table(HEADS).map_err(journal_database_error)?;
            let actual = heads
                .get(actor.as_str())
                .map_err(journal_database_error)?
                .map_or(Revision::ZERO, |value| Revision::new(value.value()));
            if actual != expected {
                return Ok(AppendOutcome::Conflict { expected, actual });
            }

            let events = events.into_vec();
            let head = expected
                .checked_advance(events.len())
                .ok_or(JournalError::RevisionExhausted)?;
            let mut events_table = write.open_table(EVENTS).map_err(journal_database_error)?;
            for (index, event) in events.into_iter().enumerate() {
                let revision = expected
                    .checked_advance(index + 1)
                    .expect("the batch head was checked");
                let value = serde_json::to_vec(&event)
                    .map_err(RedbStoreError::Serialization)
                    .map_err(JournalError::Backend)?;
                events_table
                    .insert((actor.as_str(), revision.get()), value.as_slice())
                    .map_err(journal_database_error)?;
            }
            heads
                .insert(actor.as_str(), head.get())
                .map_err(journal_database_error)?;
            head
        };
        write.commit().map_err(journal_database_error)?;
        Ok(AppendOutcome::Appended { head })
    }
}

/// Cheap read-only handle to one actor journal for boot-time queries.
///
/// Read-only opens skip the write-path setup which makes full opens cost
/// proportional to live data, so session previews can query even a large
/// journal in microseconds instead of seconds.
pub struct ReadOnlyStore {
    database: ReadOnlyDatabase,
}

impl ReadOnlyStore {
    /// Opens an existing journal without the write-path setup.
    ///
    /// Requires a journal whose last writer closed cleanly: redb marks the
    /// file recovery-required for as long as a read-write handle is alive and
    /// clears the mark only on teardown, and read-only opens reject a marked
    /// file outright. Callers reading a journal that a killed session left
    /// behind must fall back to [`RedbStore::open`], which clears the mark.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RedbStoreError> {
        let database = ReadOnlyDatabase::open(path).map_err(database_error)?;
        Ok(Self { database })
    }

    /// Reads one bounded page of actor events in journal order.
    pub fn read_page(
        &self,
        actor: &ActorId,
        after: Revision,
        limit: NonZeroUsize,
    ) -> Result<JournalPage, RedbStoreError> {
        read_page_from(&self.database, actor, after, limit)
    }
}

fn read_page_from(
    database: &impl ReadableDatabase,
    actor: &ActorId,
    after: Revision,
    limit: NonZeroUsize,
) -> Result<JournalPage, RedbStoreError> {
    let read = database.begin_read().map_err(database_error)?;
    let head = {
        let heads = read.open_table(HEADS).map_err(database_error)?;
        heads
            .get(actor.as_str())
            .map_err(database_error)?
            .map_or(Revision::ZERO, |value| Revision::new(value.value()))
    };
    if after >= head {
        return Ok(JournalPage {
            head,
            events: Vec::new(),
        });
    }

    let first = after
        .checked_advance(1)
        .expect("a revision below the head can advance");
    let events_table = read.open_table(EVENTS).map_err(database_error)?;
    let range = events_table
        .range((actor.as_str(), first.get())..=(actor.as_str(), head.get()))
        .map_err(database_error)?;
    let events = range
        .take(limit.get())
        .map(|item| {
            let (key, value) = item.map_err(database_error)?;
            let event = serde_json::from_slice(value.value())?;
            Ok(StoredEvent {
                revision: Revision::new(key.value().1),
                event,
            })
        })
        .collect::<Result<Vec<_>, RedbStoreError>>()?;

    Ok(JournalPage { head, events })
}

impl JournalStore for RedbStore {
    type Error = RedbStoreError;

    async fn read(
        &self,
        actor: &ActorId,
        after: Revision,
        limit: NonZeroUsize,
    ) -> Result<JournalPage, JournalError<Self::Error>> {
        self.read_page(actor, after, limit)
            .map_err(JournalError::Backend)
    }

    async fn append(
        &self,
        actor: &ActorId,
        expected: Revision,
        events: EventBatch,
    ) -> Result<AppendOutcome, JournalError<Self::Error>> {
        self.append_batch(actor, expected, events)
    }

    async fn write_checkpoint(
        &self,
        actor: &ActorId,
        revision: Revision,
        blob: &[u8],
    ) -> Result<(), JournalError<Self::Error>> {
        let write = begin_write(&self.database).map_err(journal_database_error)?;
        {
            let mut checkpoints = write
                .open_table(CHECKPOINTS)
                .map_err(journal_database_error)?;
            checkpoints
                .insert(actor.as_str(), (revision.get(), blob))
                .map_err(journal_database_error)?;
        }
        write.commit().map_err(journal_database_error)?;
        Ok(())
    }

    async fn read_checkpoint(
        &self,
        actor: &ActorId,
    ) -> Result<Option<(Revision, Vec<u8>)>, JournalError<Self::Error>> {
        let read = self.database.begin_read().map_err(journal_database_error)?;
        let checkpoints = read
            .open_table(CHECKPOINTS)
            .map_err(journal_database_error)?;
        let Some(value) = checkpoints
            .get(actor.as_str())
            .map_err(journal_database_error)?
        else {
            return Ok(None);
        };
        let (revision, blob) = value.value();
        Ok(Some((Revision::new(revision), blob.to_vec())))
    }
}

/// A redb operation, actor-event serialization, or stored-key validation failed.
#[derive(Debug, thiserror::Error)]
pub enum RedbStoreError {
    /// The embedded database rejected an operation.
    #[error("redb operation failed: {0}")]
    Database(#[source] redb::Error),
    /// Filesystem metadata for the journal file could not be read.
    #[error("journal file metadata unavailable: {0}")]
    Metadata(#[source] std::io::Error),
    /// An actor event could not cross the JSON storage boundary.
    #[error("actor event serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    /// A stored actor key violated Lam's actor-ID contract.
    #[error("stored actor id is invalid: {message}")]
    ActorId {
        /// Identifier validation failure.
        message: String,
    },
}

// Every commit through this path must leave the file openable without a full
// repair pass, even when the process dies before redb's teardown runs: a
// SIGKILL, or a panic, which makes redb skip its Drop cleanup. Quick repair
// buys that by persisting the allocator state with each commit; the price is a
// second fsync per commit, roughly doubling commit latency, which is
// acceptable at journal append rates of a few commits per model turn, not per
// token. It does not make such a file readable by `ReadOnlyDatabase` — see
// [`ReadOnlyStore::open`]. `footprint` aborts rather than commits, so it stays
// on plain `begin_write`.
fn begin_write(database: &Database) -> Result<WriteTransaction, redb::TransactionError> {
    let mut write = database.begin_write()?;
    write.set_quick_repair(true);
    Ok(write)
}

fn database_error(error: impl Into<redb::Error>) -> RedbStoreError {
    RedbStoreError::Database(error.into())
}

fn journal_database_error(error: impl Into<redb::Error>) -> JournalError<RedbStoreError> {
    JournalError::Backend(database_error(error))
}
