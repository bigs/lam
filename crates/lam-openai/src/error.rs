use std::time::Duration;

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
    /// The response stream idle timeout must allow time for network progress.
    #[error("model stream idle timeout must be greater than zero")]
    InvalidStreamIdleTimeout,
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
    /// Authentication could not produce a usable credential.
    #[error("model authentication failed: {message}")]
    Auth {
        /// Authentication diagnostic.
        message: String,
    },
    /// The HTTP request or response stream failed.
    #[error("model HTTP request failed: {0}")]
    Http(#[source] reqwest::Error),
    /// The provider left an event stream open without sending more data.
    #[error("model event stream was idle for {timeout:?}")]
    StreamIdle {
        /// Maximum permitted delay between response body chunks.
        timeout: Duration,
    },
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
    /// A completed stream could not be folded into the native response.
    #[error("completed stream could not be folded into a response: {message}")]
    Codec {
        /// Codec diagnostic.
        message: String,
    },
}

impl ProviderError {
    pub(crate) fn is_response_body_failure(&self) -> bool {
        matches!(self, Self::Http(error) if error.is_body() || error.is_decode())
            || matches!(self, Self::StreamIdle { .. })
    }

    pub(crate) fn is_context_overflow(&self) -> bool {
        let text = match self {
            Self::HttpStatus {
                status: 400 | 413 | 422,
                body,
            } => body,
            Self::Api { message } => message,
            _ => return false,
        }
        .to_ascii_lowercase();
        [
            "context_length_exceeded",
            "context length",
            "context window",
            "maximum context",
            "too many tokens",
            "prompt is too long",
        ]
        .iter()
        .any(|needle| text.contains(needle))
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderError;

    #[test]
    fn classifies_only_recognized_context_errors() {
        assert!(
            ProviderError::HttpStatus {
                status: 400,
                body: r#"{"code":"context_length_exceeded"}"#.to_owned(),
            }
            .is_context_overflow()
        );
        assert!(
            ProviderError::Api {
                message: "maximum context length was exceeded".to_owned(),
            }
            .is_context_overflow()
        );
        assert!(
            !ProviderError::HttpStatus {
                status: 401,
                body: "context length".to_owned(),
            }
            .is_context_overflow()
        );
    }
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
