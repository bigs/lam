use lam_core::RunId;
use serde::{Deserialize, Serialize};

/// Codec used for model-visible notices emitted by Lam itself.
pub const SYSTEM_NOTICE_CODEC_ID: &str = "lam/system-notice";

/// Current representation version for [`SystemNotice`] payloads.
pub const SYSTEM_NOTICE_CODEC_VERSION: u32 = 1;

/// Host component recorded as the source of Lam runtime notices.
pub const RUNTIME_COMPONENT_ID: &str = "lam/runtime";

/// State of the TypeScript isolate after a runtime resumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum IsolateState {
    /// The previous in-memory heap was lost and a fresh isolate was created.
    Reset,
}

/// Durable knowledge about an eval interrupted before its result was recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InterruptedEvalOutcome {
    /// Effects may have happened, but no authoritative result is available.
    Unknown,
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
}
