use reqwest::header::InvalidHeaderValue;

/// Invalid model-adapter configuration.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// A required model identifier was empty.
    #[error("model identifier must not be empty")]
    EmptyModel,
    /// The OpenAI Responses adapter was built without authentication.
    #[error("an API key is required for the OpenAI Responses API")]
    MissingApiKey,
    /// A bearer token could not be represented as an HTTP header.
    #[error("API key is not a valid HTTP bearer token")]
    InvalidApiKey(#[source] InvalidHeaderValue),
    /// The configured base URL is invalid or unsafe to log as model metadata.
    #[error("invalid API base URL: {message}")]
    InvalidBaseUrl {
        /// URL diagnostic.
        message: String,
    },
    /// Provider-specific request options must be a JSON object.
    #[error("extra request body must be a JSON object")]
    ExtraBodyMustBeObject,
    /// Configured token prices must be finite and non-negative.
    #[error("model token prices must be finite and non-negative")]
    InvalidPricing,
    /// Reqwest could not construct its reusable client.
    #[error("failed to construct HTTP client")]
    HttpClient(#[source] reqwest::Error),
}

/// An HTTP provider failed before yielding one completed native response.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The provider was handed a payload produced by another codec.
    #[error("unexpected request codec: expected {expected}, received {received}")]
    UnexpectedRequestCodec {
        /// Required codec identifier and version.
        expected: String,
        /// Actual codec identifier and version.
        received: String,
    },
    /// The codec-produced request envelope is malformed.
    #[error("invalid encoded request: {message}")]
    InvalidRequest {
        /// Validation diagnostic.
        message: String,
    },
    /// The HTTP request or response stream failed.
    #[error("model HTTP request failed")]
    Http(#[source] reqwest::Error),
    /// The server rejected the request with a non-success status.
    #[error("model endpoint returned HTTP {status}: {body}")]
    HttpStatus {
        /// Numeric HTTP status.
        status: u16,
        /// Provider error body, bounded for diagnostics.
        body: String,
    },
    /// The response body did not conform to Server-Sent Events.
    #[error("invalid model event stream: {message}")]
    InvalidEventStream {
        /// Parser diagnostic.
        message: String,
    },
    /// One event contained invalid JSON.
    #[error("invalid JSON in model event: {message}")]
    InvalidEventJson {
        /// Parser diagnostic.
        message: String,
    },
    /// A provider-native terminal error event was received.
    #[error("model endpoint reported an error: {message}")]
    Api {
        /// Provider diagnostic.
        message: String,
    },
    /// The event stream ended without the protocol's terminal payload.
    #[error("model event stream ended before {expected}")]
    MissingTerminal {
        /// Expected terminal event.
        expected: &'static str,
    },
}

/// A native payload could not be encoded into or interpreted from context.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CodecError {
    /// Context contains a payload this adapter cannot replay losslessly.
    #[error("unsupported context payload codec {codec}")]
    UnsupportedContext {
        /// Incompatible codec identifier and version.
        codec: String,
    },
    /// The codec was handed a response produced by another adapter.
    #[error("unexpected response codec: expected {expected}, received {received}")]
    UnexpectedResponseCodec {
        /// Required codec identifier and version.
        expected: String,
        /// Actual codec identifier and version.
        received: String,
    },
    /// A Lam-native or provider-native payload is structurally invalid.
    #[error("invalid model payload: {message}")]
    InvalidPayload {
        /// Validation diagnostic.
        message: String,
    },
    /// The model emitted a response Lam cannot execute safely.
    #[error("invalid model directive: {message}")]
    InvalidDirective {
        /// Protocol diagnostic.
        message: String,
    },
}
