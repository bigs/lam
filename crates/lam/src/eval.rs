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
    /// Lam deliberately did not run this call: a sibling beyond the first
    /// eval, an unknown function, or invalid eval arguments.
    Rejected {
        /// Model-visible explanation of why the call did not run.
        message: String,
    },
}

impl EvalOutcome {
    pub(crate) fn parallel_tool_call_rejected() -> Self {
        Self::Rejected {
            message: "This eval was not executed because it was emitted alongside another tool call in the same model response. Lam executes only the first. After receiving that result, issue the remaining eval as a new tool call, or combine genuinely concurrent work into one TypeScript program with `Promise.all`."
                .to_owned(),
        }
    }
}
