use std::error::Error;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CompactionArtifact, EncodedPayload, ProjectedContextEntry};

/// Requested shape of a terminal model output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputContract {
    /// Extract one ordinary text completion.
    Text,
    /// Produce a JSON value conforming to this schema.
    Structured {
        /// JSON Schema supplied to the provider codec.
        schema: Value,
    },
}

/// Per-request behavior selected by Lam's agent or compaction loop.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequestConfig<'a> {
    /// Requested terminal output shape.
    pub output: &'a OutputContract,
    /// Ephemeral system instructions for this request.
    pub system_prompt: &'a str,
    /// Whether Lam's single eval tool is available.
    pub enable_eval: bool,
    /// Optional provider output-token cap.
    pub max_output_tokens: Option<u64>,
}

impl<'a> ModelRequestConfig<'a> {
    /// Constructs an ordinary agent-loop request.
    #[must_use]
    pub const fn agent(output: &'a OutputContract, system_prompt: &'a str) -> Self {
        Self {
            output,
            system_prompt,
            enable_eval: true,
            max_output_tokens: None,
        }
    }

    /// Constructs a tool-free text request used by a model-backed compactor.
    #[must_use]
    pub const fn compaction(system_prompt: &'a str, max_output_tokens: u64) -> Self {
        Self {
            output: &OutputContract::Text,
            system_prompt,
            enable_eval: false,
            max_output_tokens: Some(max_output_tokens),
        }
    }
}

/// One eval request computed from a provider-native response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalRequest {
    /// Brief one-line explanation of what the program is intended to accomplish.
    pub intent: String,
    /// TypeScript program to evaluate in the actor's persistent isolate.
    pub source: String,
    /// Optional model-requested timeout, still bounded by the host maximum.
    pub timeout: Option<Duration>,
}

/// Provider-neutral action computed from an untouched native response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelDirective {
    /// Execute exactly one TypeScript program before the next model request.
    Eval(EvalRequest),
    /// Candidate terminal value interpreted by the codec.
    Output(Value),
    /// The response is a well-formed native message whose first call cannot
    /// be executed — an unknown function or invalid eval arguments.
    ///
    /// The runtime records the response, returns this message as the call's
    /// rejection result, and requests the model again, so the model can
    /// correct a mistake instead of failing the run.
    Rejected {
        /// Model-visible explanation of why the call was not executed.
        message: String,
    },
}

/// Provider-neutral projection of one completed native model response.
///
/// Display deltas preserve the response's visible text, reasoning, and tool
/// call stream for interactive or historical presentation. The directive is
/// the single semantic action consumed by the actor runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelResponseProjection {
    /// Ordered visible output recovered from the completed response.
    pub display: Vec<ModelDelta>,
    /// The single runtime action represented by the completed response.
    pub directive: ModelDirective,
    /// Additional native eval calls which must receive rejection results.
    ///
    /// Lam executes only the first eval from a response. Some compatible
    /// providers ignore their parallel-tool-call control, so codecs retain
    /// those sibling calls for native replay while asking the runtime to
    /// reject them without execution.
    pub rejected_eval_calls: usize,
}

/// Provider-neutral model output suitable for live or historical display.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", content = "delta", rename_all = "camelCase")]
pub enum ModelDelta {
    /// Ordinary visible text.
    Text(String),
    /// Visible reasoning or thinking text when a provider exposes it.
    Reasoning(String),
    /// One streamed fragment of a native tool call.
    ToolCall(ToolCallDelta),
}

/// Provider-neutral display view of an incrementally constructed tool call.
///
/// The index is stable only within one model response. Names and arguments are
/// fragments and must be appended in arrival order.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallDelta {
    /// Provider-native position of this call within the model response.
    pub index: usize,
    /// Provider-native call identity when the current fragment carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Function-name fragment when the current fragment carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// JSON argument fragment, possibly empty while the call is introduced.
    pub arguments: String,
}

/// Best-effort metadata computed from one completed provider response.
///
/// Provider-native response payloads remain authoritative. This view exists
/// for downstream observability; a compaction record may retain a copy beside
/// its untouched source response so replay costs remain inspectable.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelResponseMetadata {
    /// Model identifier reported by the provider, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Normalized token counts plus the untouched provider usage object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// Provider-reported or locally estimated monetary cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

/// Provider-neutral token counts with lossless access to native usage data.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// Tokens in the model input, including cached input when reported that way.
    pub input_tokens: u64,
    /// Input tokens served from a provider cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Tokens generated by the model, including hidden reasoning when billed that way.
    pub output_tokens: u64,
    /// Hidden reasoning tokens included in `output_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Provider-reported total, or input plus output when the provider omits it.
    pub total_tokens: u64,
    /// Untouched provider-native usage object.
    pub native: Value,
}

/// Monetary cost associated with one completed provider request.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    /// Cost in US dollars.
    pub amount_usd: f64,
    /// Whether the amount came from the provider or local pricing metadata.
    pub source: ModelCostSource,
}

/// Provenance of a model-request cost.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelCostSource {
    /// The provider reported the billed amount.
    ProviderReported,
    /// Lam estimated the amount from token counts and configured rates.
    Estimated,
}

/// Automatic retries when a model endpoint reports temporary overload.
///
/// Providers apply this policy to HTTP 503 (and equivalent transport signals)
/// before any response body is consumed, so intermediate failures never reach
/// model-visible context. After retries are exhausted the host still receives
/// a provider error; that failure is not injected into the model transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceUnavailableRetry {
    /// How many times to re-send after the first overload response.
    ///
    /// Total attempts are `1 + max_retries`. Zero disables automatic retry.
    pub max_retries: u32,
    /// Delay before the first retry. Subsequent delays double until
    /// [`Self::max_backoff`].
    pub initial_backoff: Duration,
    /// Upper bound on a single retry delay.
    pub max_backoff: Duration,
}

impl Default for ServiceUnavailableRetry {
    fn default() -> Self {
        Self {
            max_retries: 12,
            initial_backoff: Duration::from_millis(250),
            // 250ms * 2^11 = 512s, so all twelve retries stay on the exponential curve.
            max_backoff: Duration::from_secs(512),
        }
    }
}

impl ServiceUnavailableRetry {
    /// Builds a policy with the given retry budget and default backoff curve.
    #[must_use]
    pub fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            ..Self::default()
        }
    }

    /// Replaces the initial and maximum backoff delays.
    #[must_use]
    pub const fn with_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_backoff = initial;
        self.max_backoff = max;
        self
    }

    /// Delay before the `attempt`-th retry (`0` = first retry after the first 503).
    #[must_use]
    pub fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.min(16);
        let factor = 1_u32.checked_shl(shift).unwrap_or(u32::MAX);
        self.initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

/// Non-blocking destination for ephemeral model deltas.
#[derive(Clone)]
pub struct ModelEventSink {
    emit: Arc<dyn Fn(ModelDelta) + Send + Sync>,
}

impl ModelEventSink {
    /// Creates a sink backed by an embedding-provided callback.
    pub fn new(emit: impl Fn(ModelDelta) + Send + Sync + 'static) -> Self {
        Self {
            emit: Arc::new(emit),
        }
    }

    /// Publishes one delta without waiting for an external consumer.
    pub fn emit(&self, delta: ModelDelta) {
        (self.emit)(delta);
    }
}

/// External model transport which returns untouched provider-native payloads.
pub trait ModelProvider: Send + Sync + 'static {
    /// Provider-specific request failure.
    type Error: Error + Send + Sync + 'static;

    /// Performs one request and returns its completed native response.
    fn invoke(
        &self,
        request: EncodedPayload,
        events: ModelEventSink,
    ) -> impl Future<Output = Result<EncodedPayload, Self::Error>> + Send;

    /// Classifies a provider failure caused by an oversized model context.
    fn is_context_overflow(&self, _error: &Self::Error) -> bool {
        false
    }
}

/// Pure translation between Lam context and one provider-native protocol.
pub trait ModelCodec: Send + Sync + 'static {
    /// Codec-specific interpretation failure.
    type Error: Error + Send + Sync + 'static;

    /// Encodes model-visible context, the requested output shape, and runtime
    /// instructions which are deliberately not part of durable context.
    fn encode_request(
        &self,
        context: &[ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
    ) -> Result<EncodedPayload, Self::Error>;

    /// Projects one untouched completed provider response into its ordered
    /// display output and single semantic runtime action.
    fn project_response(
        &self,
        response: &EncodedPayload,
    ) -> Result<ModelResponseProjection, Self::Error>;

    /// Computes optional observability metadata from a native response.
    ///
    /// Extraction is deliberately best effort: missing or unfamiliar provider
    /// fields return an empty view rather than failing an otherwise valid run.
    fn response_metadata(&self, _response: &EncodedPayload) -> ModelResponseMetadata {
        ModelResponseMetadata::default()
    }

    /// Materializes one portable artifact as an exact native context item.
    ///
    /// Returning `None` means this codec has not implemented compaction replay.
    fn materialize_compaction(
        &self,
        _artifact: &CompactionArtifact,
    ) -> Result<Option<EncodedPayload>, Self::Error> {
        Ok(None)
    }

    /// Reports whether an exact materialized replacement can be replayed.
    fn accepts_compaction_replacement(&self, _replacement: &EncodedPayload) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::ServiceUnavailableRetry;
    use std::time::Duration;

    #[test]
    fn service_unavailable_retry_defaults_and_backoff() {
        let policy = ServiceUnavailableRetry::default();
        assert_eq!(policy.max_retries, 12);
        assert_eq!(policy.initial_backoff, Duration::from_millis(250));
        assert_eq!(policy.max_backoff, Duration::from_secs(512));
        assert_eq!(policy.backoff(0), Duration::from_millis(250));
        assert_eq!(policy.backoff(1), Duration::from_millis(500));
        assert_eq!(policy.backoff(2), Duration::from_secs(1));
        assert_eq!(policy.backoff(5), Duration::from_secs(8));
        assert_eq!(policy.backoff(9), Duration::from_secs(128));
        assert_eq!(policy.backoff(11), Duration::from_secs(512));
        assert_eq!(policy.backoff(u32::MAX), Duration::from_secs(512));
    }

    #[test]
    fn service_unavailable_retry_builder_overrides() {
        let policy = ServiceUnavailableRetry::new(3)
            .with_backoff(Duration::from_millis(10), Duration::from_millis(40));
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.backoff(0), Duration::from_millis(10));
        assert_eq!(policy.backoff(1), Duration::from_millis(20));
        assert_eq!(policy.backoff(3), Duration::from_millis(40));
    }
}
