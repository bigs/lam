//! Provider-independent domain logic for Lam.
//!
//! This crate owns the append-only actor state model and deliberately does not
//! depend on V8 or `deno_core`.

mod context;
mod event;
mod journal;
mod mem_store;
mod message;
mod projection;
mod types;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use context::{ContextEntry, ContextTransition, RunProgress};
pub use event::{ACTOR_EVENT_SCHEMA_VERSION, ActorEvent, ActorEventData};
pub use journal::{
    AppendOutcome, EventBatch, JournalError, JournalPage, JournalStore, StoredEvent,
};
pub use mem_store::MemStore;
pub use message::{DeliveryMode, MessageEnvelope, MessageError, MessageSource};
pub use projection::{
    ActorState, AdmissionDecision, AdmittedMessage, ProjectedContextEntry, StateError,
};
pub use types::{
    ActorId, CodecId, CodecRef, ComponentId, ContextSequence, EncodedPayload, InvalidIdentifier,
    MessageId, PrincipalId, Revision, RunId, Timestamp,
};
