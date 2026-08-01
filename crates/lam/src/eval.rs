use lam_deno::{EvalError, EvalOutput};
use serde::Serialize;

/// Complete model-visible result of one eval program.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum EvalOutcome {
    /// The TypeScript program completed normally.
    Success {
        /// JSON result and captured console calls.
        output: EvalOutput,
    },
    /// Evaluation failed, including timeout and isolate replacement outcomes.
    Failure {
        /// Structured kernel failure.
        error: EvalError,
    },
}
