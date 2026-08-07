use schemars::JsonSchema;
use serde::Serialize;
use thiserror::Error;

/// Structured failure from a batteries HTTP provider function.
#[derive(Clone, Debug, JsonSchema, Serialize, Error)]
#[serde(tag = "code", rename_all = "camelCase")]
pub enum ProviderError {
    /// The request was rejected by local policy or was incomplete.
    #[error("invalid request: {message}")]
    InvalidRequest {
        /// Validation diagnostic.
        message: String,
    },
    /// Authentication failed or the API key was rejected.
    #[error("authentication failed: {message}")]
    Auth {
        /// Upstream diagnostic.
        message: String,
    },
    /// The provider reported rate limiting.
    #[error("rate limited: {message}")]
    RateLimited {
        /// Upstream diagnostic.
        message: String,
        /// Optional retry-after hint in seconds.
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_after_secs: Option<u64>,
    },
    /// The HTTP transport failed (timeout, DNS, connection).
    #[error("transport error: {message}")]
    Transport {
        /// Transport diagnostic.
        message: String,
    },
    /// The provider returned a non-success HTTP status or unusable body.
    #[error("upstream error (HTTP {status}): {message}")]
    Upstream {
        /// HTTP status code when available, otherwise zero.
        status: u16,
        /// Upstream diagnostic or body excerpt.
        message: String,
    },
}

impl ProviderError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    pub(crate) fn from_status(status: u16, body: &str) -> Self {
        let message = truncate(body, 2_000);
        match status {
            401 | 403 => Self::Auth { message },
            429 => Self::RateLimited {
                message,
                retry_after_secs: None,
            },
            _ => Self::Upstream { status, message },
        }
    }
}

/// Host-side configuration or build failure for a batteries pack.
#[derive(Debug, Error)]
pub enum BatteriesError {
    /// A required field was missing or contradictory.
    #[error("invalid batteries configuration: {message}")]
    InvalidConfig {
        /// Validation diagnostic.
        message: String,
    },
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut out = value.chars().take(max).collect::<String>();
    out.push('…');
    out
}
