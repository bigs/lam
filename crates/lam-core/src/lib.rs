//! Provider-independent domain logic for Lam.
//!
//! This crate owns the append-only actor state model and deliberately does not
//! depend on V8 or `deno_core`.

mod compaction;
mod context;
mod event;
mod journal;
mod mem_store;
mod message;
mod model;
mod projection;
mod types;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use compaction::{
    COMPACTION_RECORD_CODEC_ID, COMPACTION_RECORD_CODEC_VERSION, CompactionArtifact,
    CompactionConfig, CompactionConfigError, CompactionError, CompactionFuture, CompactionOutput,
    CompactionPlan, CompactionReason, CompactionRecord, CompactionRequest, CompactionUnit,
    Compactor, ContextAmount, atomic_compaction_units, compaction_prefix_len,
    estimate_entry_tokens,
};
pub use context::{ContextEntry, ContextTransition, RunProgress};
pub use event::{ACTOR_EVENT_SCHEMA_VERSION, ActorEvent, ActorEventData};
pub use journal::{
    AppendOutcome, EventBatch, JournalError, JournalPage, JournalStore, StoredEvent,
};
pub use mem_store::MemStore;
pub use message::{DeliveryMode, MessageEnvelope, MessageError, MessageSource};
pub use model::{
    EvalRequest, ModelCodec, ModelCost, ModelCostSource, ModelDelta, ModelDirective,
    ModelEventSink, ModelProvider, ModelRequestConfig, ModelResponseMetadata,
    ModelResponseProjection, OutputContract, ServiceUnavailableRetry, TokenUsage, ToolCallDelta,
};
pub use projection::{
    ActorState, AdmissionDecision, AdmittedMessage, Checkpoint, CheckpointEntry,
    ProjectedContextEntry, StateError,
};
pub use types::{
    ActorId, CodecId, CodecRef, ComponentId, ContextSequence, EncodedPayload, InvalidIdentifier,
    MessageId, ModelDescriptor, ModelId, ModelSelection, PrincipalId, Revision, RunId, Timestamp,
};
