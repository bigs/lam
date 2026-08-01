use std::error::Error;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EncodedPayload, ProjectedContextEntry};

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

/// One eval request computed from a provider-native response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvalRequest {
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
}

/// Ephemeral model output suitable for interactive display.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", content = "delta", rename_all = "camelCase")]
pub enum ModelDelta {
    /// Ordinary visible text.
    Text(String),
    /// Visible reasoning or thinking text when a provider exposes it.
    Reasoning(String),
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
}

/// Pure translation between Lam context and one provider-native protocol.
pub trait ModelCodec: Send + Sync + 'static {
    /// Codec-specific interpretation failure.
    type Error: Error + Send + Sync + 'static;

    /// Encodes model-visible context and the requested output shape.
    fn encode_request(
        &self,
        context: &[ProjectedContextEntry],
        output: &OutputContract,
    ) -> Result<EncodedPayload, Self::Error>;

    /// Interprets one untouched completed provider response.
    fn interpret_response(&self, response: &EncodedPayload) -> Result<ModelDirective, Self::Error>;
}
