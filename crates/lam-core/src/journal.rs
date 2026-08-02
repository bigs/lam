use std::error::Error;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::{ActorEvent, ActorId, Revision};

/// An ordered, nonempty actor-event batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventBatch {
    first: ActorEvent,
    remaining: Vec<ActorEvent>,
}

impl EventBatch {
    /// Creates a batch containing one event.
    #[must_use]
    pub const fn one(first: ActorEvent) -> Self {
        Self {
            first,
            remaining: Vec::new(),
        }
    }

    /// Creates a batch with a required first event.
    #[must_use]
    pub const fn new(first: ActorEvent, remaining: Vec<ActorEvent>) -> Self {
        Self { first, remaining }
    }

    /// Adds an event to the end of the batch.
    pub fn push(&mut self, event: ActorEvent) {
        self.remaining.push(event);
    }

    /// Returns the number of events in the nonempty batch.
    #[must_use]
    pub const fn event_count(&self) -> usize {
        1 + self.remaining.len()
    }

    /// Iterates over events in append order.
    pub fn iter(&self) -> impl Iterator<Item = &ActorEvent> {
        std::iter::once(&self.first).chain(self.remaining.iter())
    }

    /// Consumes the batch into an ordered vector.
    #[must_use]
    pub fn into_vec(self) -> Vec<ActorEvent> {
        let mut events = Vec::with_capacity(self.event_count());
        events.push(self.first);
        events.extend(self.remaining);
        events
    }
}

/// One actor event paired with its actor-local journal revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredEvent {
    /// Event revision.
    pub revision: Revision,
    /// Stored event.
    pub event: ActorEvent,
}

/// One bounded, internally consistent journal read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalPage {
    /// Journal head observed in the same store view as `events`.
    pub head: Revision,
    /// Contiguous events strictly after the requested revision.
    pub events: Vec<StoredEvent>,
}

/// Result of an atomic conditional append.
#[must_use = "append conflicts must be handled before actor state can advance"]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendOutcome {
    /// Every event was committed and this is the new journal head.
    Appended {
        /// New head after the batch.
        head: Revision,
    },
    /// The caller's projection was stale and no event was appended.
    Conflict {
        /// Revision supplied by the caller.
        expected: Revision,
        /// Actual journal head at the append linearization point.
        actual: Revision,
    },
}

/// A journal operation failed independently of compare-and-append contention.
#[derive(Debug, thiserror::Error)]
pub enum JournalError<E> {
    /// Backend-specific operation failure.
    #[error("journal backend failed: {0}")]
    Backend(#[source] E),
    /// Appending the batch would overflow the revision representation.
    #[error("journal revision space is exhausted")]
    RevisionExhausted,
}

/// Pluggable ordered storage for Lam actor events.
pub trait JournalStore: Send + Sync {
    /// Backend-specific operation error.
    type Error: Error + Send + Sync + 'static;

    /// Reads at most `limit` contiguous events strictly after `after`.
    fn read(
        &self,
        actor: &ActorId,
        after: Revision,
        limit: NonZeroUsize,
    ) -> impl Future<Output = Result<JournalPage, JournalError<Self::Error>>> + Send;

    /// Atomically appends a nonempty batch when the journal is at `expected`.
    fn append(
        &self,
        actor: &ActorId,
        expected: Revision,
        events: EventBatch,
    ) -> impl Future<Output = Result<AppendOutcome, JournalError<Self::Error>>> + Send;
}

impl<S> JournalStore for Arc<S>
where
    S: JournalStore + ?Sized,
{
    type Error = S::Error;

    fn read(
        &self,
        actor: &ActorId,
        after: Revision,
        limit: NonZeroUsize,
    ) -> impl Future<Output = Result<JournalPage, JournalError<Self::Error>>> + Send {
        (**self).read(actor, after, limit)
    }

    fn append(
        &self,
        actor: &ActorId,
        expected: Revision,
        events: EventBatch,
    ) -> impl Future<Output = Result<AppendOutcome, JournalError<Self::Error>>> + Send {
        (**self).append(actor, expected, events)
    }
}
