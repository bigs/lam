//! Public facade for the Lam coding-agent runtime.
//!
//! The actor builder will arrive in a later implementation slice. The completed
//! persistent-eval kernel and append-only actor state model are re-exported
//! here so embedders can use the implemented primitives through the intended
//! public crate.

pub use lam_core::{
    ACTOR_EVENT_SCHEMA_VERSION, ActorEvent, ActorEventData, ActorId, ActorState, AdmissionDecision,
    AdmittedMessage, AppendOutcome, CodecId, CodecRef, ComponentId, ContextEntry, ContextSequence,
    ContextTransition, DeliveryMode, EncodedPayload, EventBatch, InvalidIdentifier, JournalError,
    JournalPage, JournalStore, MemStore, MessageEnvelope, MessageError, MessageId, MessageSource,
    PrincipalId, ProjectedContextEntry, Revision, RunId, RunProgress, StateError, StoredEvent,
    Timestamp,
};
pub use lam_deno::{
    ConsoleEntry, ConsoleLevel, EvalError, EvalOptions, EvalOutput, EvalValue, Isolate,
    IsolateBuildError, IsolateBuilder, Namespace, Never, OperationContext, RuntimeException,
};
