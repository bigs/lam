//! Public facade for the Lam coding-agent runtime.
//!
//! The public builder combines the persistent-eval kernel, append-only actor
//! state, and provider-neutral model loop behind one embeddable facade.

mod actor;
mod command;
mod compaction;
mod compaction_engine;
mod error;
mod eval;
mod model;
mod notice;
mod prompt;
mod recovery;
mod run;
mod runner;
mod runtime_event;
mod runtime_journal;

pub use actor::{
    AbortHandle, Actor, ActorBuilder, ActorRef, Clock, Lam, LamBuilder, LamRuntime, MessageReceipt,
    ModelSwitchPolicy, ModelSwitchReceipt, SystemClock,
};
pub use compaction::{
    CompactionReceipt, FallbackCompactor, SummaryTailCompactor, TruncateOldestCompactor,
};
pub use error::{ActorBuildError, ActorError};
pub use eval::EvalOutcome;
pub use model::Model;
pub use notice::{
    InterruptedEvalOutcome, IsolateState, RUNTIME_COMPONENT_ID, SYSTEM_NOTICE_CODEC_ID,
    SYSTEM_NOTICE_CODEC_VERSION, SystemNotice,
};
pub use run::{Run, RunEvent};
pub use runtime_event::{RuntimeEvent, RuntimeEvents};

pub use lam_core::{
    ACTOR_EVENT_SCHEMA_VERSION, ActorEvent, ActorEventData, ActorId, ActorState, AdmissionDecision,
    AdmittedMessage, AppendOutcome, COMPACTION_RECORD_CODEC_ID, COMPACTION_RECORD_CODEC_VERSION,
    CodecId, CodecRef, CompactionArtifact, CompactionConfig, CompactionConfigError,
    CompactionError, CompactionFuture, CompactionOutput, CompactionPlan, CompactionReason,
    CompactionRecord, CompactionRequest, CompactionUnit, Compactor, ComponentId, ContextAmount,
    ContextEntry, ContextSequence, ContextTransition, DeliveryMode, EncodedPayload, EvalRequest,
    EventBatch, InvalidIdentifier, JournalError, JournalPage, JournalStore, MemStore,
    MessageEnvelope, MessageError, MessageId, MessageSource, ModelCodec, ModelCost,
    ModelCostSource, ModelDelta, ModelDescriptor, ModelDirective, ModelEventSink, ModelId,
    ModelProvider, ModelRequestConfig, ModelResponseMetadata, ModelSelection, OutputContract,
    PrincipalId, ProjectedContextEntry, Revision, RunId, RunProgress, StateError, StoredEvent,
    Timestamp, TokenUsage, atomic_compaction_units, compaction_prefix_len, estimate_entry_tokens,
};
pub use lam_deno::{
    ConsoleEntry, ConsoleLevel, EvalError, EvalOptions, EvalOutput, EvalValue, Isolate,
    IsolateBuildError, IsolateBuilder, Namespace, Never, OperationContext, RuntimeException,
};
