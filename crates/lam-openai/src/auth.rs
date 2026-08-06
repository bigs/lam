//! Pluggable request authentication for model HTTP transports.
//!
//! Static bearer tokens cover ordinary API keys. Dynamic sources cover OAuth
//! access tokens that must be refreshed before they expire.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use reqwest::header::HeaderValue;

use crate::error::ProviderError;

/// Asynchronous source of an HTTP `Authorization` header.
///
/// Implementations may refresh credentials before returning. Failures surface
/// as [`ProviderError`] so the runtime can stop the model request cleanly.
pub trait AuthSource: Send + Sync {
    /// Returns the current authorization header, or `None` for unauthenticated
    /// local servers.
    fn authorization(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<HeaderValue>, ProviderError>> + Send + '_>>;

    /// Called when the model endpoint rejects the request with HTTP 401.
    ///
    /// Return `Ok(true)` after credentials were refreshed or reloaded so the
    /// transport can retry once. The default is `Ok(false)` (no retry).
    fn on_unauthorized(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ProviderError>> + Send + '_>> {
        Box::pin(async { Ok(false) })
    }
}

/// Shared ownership of an [`AuthSource`].
pub type SharedAuthSource = Arc<dyn AuthSource>;

/// Builds a static bearer authorization header.
pub fn bearer_header(token: &str) -> Result<HeaderValue, ProviderError> {
    let mut value =
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| ProviderError::Auth {
            message: format!("authorization token is not a valid HTTP header: {error}"),
        })?;
    value.set_sensitive(true);
    Ok(value)
}

/// [`AuthSource`] that always returns the same bearer token.
pub struct StaticBearer {
    header: HeaderValue,
}

impl StaticBearer {
    /// Constructs a static bearer source from a raw API key or access token.
    pub fn new(token: impl AsRef<str>) -> Result<Self, ProviderError> {
        Ok(Self {
            header: bearer_header(token.as_ref())?,
        })
    }
}

impl AuthSource for StaticBearer {
    fn authorization(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<HeaderValue>, ProviderError>> + Send + '_>> {
        let header = self.header.clone();
        Box::pin(async move { Ok(Some(header)) })
    }
}
