use lam_core::{CodecId, CodecRef, RunId};
use serde::{Deserialize, Serialize};

/// Codec used for model-visible notices emitted by Lam itself.
pub const SYSTEM_NOTICE_CODEC_ID: &str = "lam/system-notice";

/// Current representation version for [`SystemNotice`] payloads.
pub const SYSTEM_NOTICE_CODEC_VERSION: u32 = 1;

/// Host component recorded as the source of Lam runtime notices.
pub const RUNTIME_COMPONENT_ID: &str = "lam/runtime";

pub(crate) fn system_notice_codec() -> CodecRef {
    CodecRef::new(
        CodecId::new(SYSTEM_NOTICE_CODEC_ID).expect("Lam's system notice codec id is valid"),
        SYSTEM_NOTICE_CODEC_VERSION,
    )
}

/// State of the TypeScript isolate after a runtime boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum IsolateState {
    /// The current TypeScript heap remains available.
    Retained,
    /// The previous in-memory heap was lost rather than retained.
    Reset,
}

/// Why a recoverable actor run was deliberately closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InterruptionReason {
    /// The embedding acted on an explicit user request.
    User,
}

/// Durable knowledge about an eval interrupted before its result was recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InterruptedEvalOutcome {
    /// Effects may have happened, but no authoritative result is available.
    Unknown,
    /// A model-visible eval failure was recorded in the same atomic boundary.
    FailureRecorded,
}

/// One structured, model-visible notice produced by the Lam runtime.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SystemNotice {
    /// A durable actor was loaded into a fresh process-local runtime.
    RuntimeResumed {
        /// Whether the previous TypeScript heap was retained or reset.
        isolate_state: IsolateState,
        /// Run which was active when the new runtime loaded the journal.
        #[serde(skip_serializing_if = "Option::is_none")]
        resumed_run_id: Option<RunId>,
        /// Status of an eval request lacking a durable result.
        #[serde(skip_serializing_if = "Option::is_none")]
        interrupted_eval_outcome: Option<InterruptedEvalOutcome>,
    },
    /// An embedding deliberately stopped one active run without retiring it.
    RunInterrupted {
        /// Run closed by this notice.
        run_id: RunId,
        /// Source of the interruption request.
        reason: InterruptionReason,
        /// Whether the persistent TypeScript heap survived.
        isolate_state: IsolateState,
        /// Status of a durable eval request lacking its normal result.
        #[serde(skip_serializing_if = "Option::is_none")]
        interrupted_eval_outcome: Option<InterruptedEvalOutcome>,
    },
}

impl SystemNotice {
    pub(crate) const fn runtime_resumed(
        resumed_run_id: Option<RunId>,
        interrupted_eval_outcome: Option<InterruptedEvalOutcome>,
    ) -> Self {
        Self::RuntimeResumed {
            isolate_state: IsolateState::Reset,
            resumed_run_id,
            interrupted_eval_outcome,
        }
    }

    pub(crate) const fn run_interrupted(
        run_id: RunId,
        isolate_state: IsolateState,
        interrupted_eval_outcome: Option<InterruptedEvalOutcome>,
    ) -> Self {
        Self::RunInterrupted {
            run_id,
            reason: InterruptionReason::User,
            isolate_state,
            interrupted_eval_outcome,
        }
    }
}
