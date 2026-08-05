use std::collections::HashMap;
use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::{
    ActorEvent, ActorId, AppendOutcome, EventBatch, JournalError, JournalPage, JournalStore,
    Revision, StoredEvent,
};

/// Pure-Rust in-memory reference implementation of [`JournalStore`].
pub struct MemStore {
    journals: RwLock<HashMap<ActorId, Vec<ActorEvent>>>,
    checkpoints: RwLock<HashMap<ActorId, (Revision, Vec<u8>)>>,
}

impl MemStore {
    /// Constructs an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            journals: RwLock::new(HashMap::new()),
            checkpoints: RwLock::new(HashMap::new()),
        }
    }

    fn read_journals(&self) -> RwLockReadGuard<'_, HashMap<ActorId, Vec<ActorEvent>>> {
        self.journals
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_journals(&self) -> RwLockWriteGuard<'_, HashMap<ActorId, Vec<ActorEvent>>> {
        self.journals
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalStore for MemStore {
    type Error = Infallible;

    async fn read(
        &self,
        actor: &ActorId,
        after: Revision,
        limit: NonZeroUsize,
    ) -> Result<JournalPage, JournalError<Self::Error>> {
        let journals = self.read_journals();
        let Some(journal) = journals.get(actor) else {
            return Ok(JournalPage {
                head: Revision::ZERO,
                events: Vec::new(),
            });
        };
        let head = Revision::new(
            u64::try_from(journal.len()).map_err(|_| JournalError::RevisionExhausted)?,
        );
        if after >= head {
            return Ok(JournalPage {
                head,
                events: Vec::new(),
            });
        }

        let start = usize::try_from(after.get()).map_err(|_| JournalError::RevisionExhausted)?;
        let end = start.saturating_add(limit.get()).min(journal.len());
        let events = journal[start..end]
            .iter()
            .cloned()
            .enumerate()
            .map(|(offset, event)| {
                let revision = after
                    .checked_advance(offset + 1)
                    .ok_or(JournalError::RevisionExhausted)?;
                Ok(StoredEvent { revision, event })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(JournalPage { head, events })
    }

    async fn append(
        &self,
        actor: &ActorId,
        expected: Revision,
        events: EventBatch,
    ) -> Result<AppendOutcome, JournalError<Self::Error>> {
        let mut journals = self.write_journals();
        let actual = Revision::new(
            u64::try_from(journals.get(actor).map_or(0, Vec::len))
                .map_err(|_| JournalError::RevisionExhausted)?,
        );
        if actual != expected {
            return Ok(AppendOutcome::Conflict { expected, actual });
        }

        let head = actual
            .checked_advance(events.event_count())
            .ok_or(JournalError::RevisionExhausted)?;
        journals
            .entry(actor.clone())
            .or_default()
            .extend(events.into_vec());
        Ok(AppendOutcome::Appended { head })
    }

    async fn write_checkpoint(
        &self,
        actor: &ActorId,
        revision: Revision,
        blob: &[u8],
    ) -> Result<(), JournalError<Self::Error>> {
        self.checkpoints
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(actor.clone(), (revision, blob.to_vec()));
        Ok(())
    }

    async fn read_checkpoint(
        &self,
        actor: &ActorId,
    ) -> Result<Option<(Revision, Vec<u8>)>, JournalError<Self::Error>> {
        Ok(self
            .checkpoints
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(actor)
            .cloned())
    }
}
