use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::ProviderError;

/// Shared HTTP client used by batteries providers.
#[derive(Clone, Debug)]
pub(crate) struct HttpClient {
    client: reqwest::Client,
    timeout: Duration,
}

impl HttpClient {
    pub(crate) fn new(timeout: Duration) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("lam-batteries/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| ProviderError::Transport {
                message: error.to_string(),
            })?;
        Ok(Self { client, timeout })
    }

    pub(crate) async fn post_json<B, R>(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &B,
    ) -> Result<R, ProviderError>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let response = self
            .client
            .post(url)
            .headers(headers)
            .timeout(self.timeout)
            .json(body)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    ProviderError::Transport {
                        message: format!("request timed out after {:?}", self.timeout),
                    }
                } else {
                    ProviderError::Transport {
                        message: error.to_string(),
                    }
                }
            })?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ProviderError::Transport {
                message: error.to_string(),
            })?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            return Err(ProviderError::from_status(status.as_u16(), &body));
        }

        serde_json::from_slice(&bytes).map_err(|error| ProviderError::Upstream {
            status: status.as_u16(),
            message: format!("could not decode JSON response: {error}"),
        })
    }
}

pub(crate) fn api_key_header(name: &str, value: &str) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
        ProviderError::invalid(format!("invalid header name `{name}`: {error}"))
    })?;
    let header_value = HeaderValue::from_str(value).map_err(|_| {
        ProviderError::invalid("API key contains characters that cannot form an HTTP header value")
    })?;
    headers.insert(header_name, header_value);
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(headers)
}

pub(crate) fn cap_usize(
    value: Option<usize>,
    max: usize,
    field: &str,
) -> Result<Option<usize>, ProviderError> {
    match value {
        None => Ok(None),
        Some(0) => Err(ProviderError::invalid(format!(
            "`{field}` must be at least 1"
        ))),
        Some(n) if n > max => Err(ProviderError::invalid(format!(
            "`{field}` {n} exceeds the host maximum of {max}"
        ))),
        Some(n) => Ok(Some(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_reject_zero_and_over_max() {
        assert!(cap_usize(Some(0), 10, "n").is_err());
        assert!(cap_usize(Some(11), 10, "n").is_err());
        assert_eq!(cap_usize(Some(10), 10, "n").unwrap(), Some(10));
        assert_eq!(cap_usize(None, 10, "n").unwrap(), None);
    }
}
