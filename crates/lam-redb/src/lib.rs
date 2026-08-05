//! Durable [`JournalStore`] implementation backed by redb.

use std::num::NonZeroUsize;
use std::path::Path;

use lam_core::{
    ActorId, AppendOutcome, EventBatch, JournalError, JournalPage, JournalStore, Revision,
    StoredEvent,
};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

const HEADS: TableDefinition<&str, u64> = TableDefinition::new("lam_actor_heads_v1");
const EVENTS: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("lam_actor_events_v1");
const CHECKPOINTS: TableDefinition<&str, (u64, &[u8])> =
    TableDefinition::new("lam_actor_checkpoints_v1");

/// Durable actor-journal storage in one redb database.
pub struct RedbStore {
    database: Database,
}

impl RedbStore {
    /// Creates a database when absent or opens an existing database.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, RedbStoreError> {
        let database = Database::create(path).map_err(database_error)?;
        Self::initialize(database)
    }

    /// Opens an existing database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RedbStoreError> {
        let database = Database::open(path).map_err(database_error)?;
        let read = database.begin_read().map_err(database_error)?;
        read.open_table(HEADS).map_err(database_error)?;
        read.open_table(EVENTS).map_err(database_error)?;
        drop(read);
        // Databases written before the checkpoint table existed need it
        // created on open so every session can bootstrap from a checkpoint.
        let write = database.begin_write().map_err(database_error)?;
        {
            write.open_table(CHECKPOINTS).map_err(database_error)?;
        }
        write.commit().map_err(database_error)?;
        Ok(Self { database })
    }

    fn initialize(database: Database) -> Result<Self, RedbStoreError> {
        let write = database.begin_write().map_err(database_error)?;
        {
            write.open_table(HEADS).map_err(database_error)?;
            write.open_table(EVENTS).map_err(database_error)?;
            write.open_table(CHECKPOINTS).map_err(database_error)?;
        }
        write.commit().map_err(database_error)?;
        Ok(Self { database })
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

    fn read_page(
        &self,
        actor: &ActorId,
        after: Revision,
        limit: NonZeroUsize,
    ) -> Result<JournalPage, RedbStoreError> {
        let read = self.database.begin_read().map_err(database_error)?;
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

    fn append_batch(
        &self,
        actor: &ActorId,
        expected: Revision,
        events: EventBatch,
    ) -> Result<AppendOutcome, JournalError<RedbStoreError>> {
        let write = self
            .database
            .begin_write()
            .map_err(journal_database_error)?;
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
        let write = self
            .database
            .begin_write()
            .map_err(journal_database_error)?;
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

fn database_error(error: impl Into<redb::Error>) -> RedbStoreError {
    RedbStoreError::Database(error.into())
}

fn journal_database_error(error: impl Into<redb::Error>) -> JournalError<RedbStoreError> {
    JournalError::Backend(database_error(error))
}
