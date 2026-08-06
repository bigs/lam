use std::error::Error as _;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use lam::ServiceUnavailableRetry;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tracing::{Instrument, field};

use crate::auth::SharedAuthSource;
use crate::error::ProviderError;

const MAX_ERROR_BODY_CHARS: usize = 16 * 1024;

/// Builds additional headers that must differ on every outbound request.
pub trait RequestHeaderSource: Send + Sync {
    /// Returns headers to merge into the current request.
    fn headers(&self) -> HeaderMap;
}

// Re-export for crate consumers that need HeaderMap construction helpers.

#[derive(Clone)]
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    authorization: Option<SharedAuthSource>,
    default_headers: HeaderMap,
    request_headers: Option<std::sync::Arc<dyn RequestHeaderSource>>,
    stream_idle_timeout: Duration,
    service_unavailable_retry: ServiceUnavailableRetry,
}

pub(crate) enum StreamBody {
    Events,
    Json(Value),
}

pub(crate) struct SseEvent {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

impl HttpTransport {
    pub(crate) fn new(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        authorization: Option<SharedAuthSource>,
        stream_idle_timeout: Duration,
        service_unavailable_retry: ServiceUnavailableRetry,
    ) -> Self {
        Self {
            client,
            endpoint,
            authorization,
            default_headers: HeaderMap::new(),
            request_headers: None,
            stream_idle_timeout,
            service_unavailable_retry,
        }
    }

    pub(crate) fn with_default_headers(mut self, headers: HeaderMap) -> Self {
        self.default_headers = headers;
        self
    }

    pub(crate) fn with_request_headers(
        mut self,
        headers: std::sync::Arc<dyn RequestHeaderSource>,
    ) -> Self {
        self.request_headers = Some(headers);
        self
    }

    pub(crate) fn child(&self, suffix: &str) -> Self {
        let mut endpoint = self.endpoint.clone();
        let path = format!("{}/{}", endpoint.path().trim_end_matches('/'), suffix);
        endpoint.set_path(&path);
        Self {
            client: self.client.clone(),
            endpoint,
            authorization: self.authorization.clone(),
            default_headers: self.default_headers.clone(),
            request_headers: self.request_headers.clone(),
            stream_idle_timeout: self.stream_idle_timeout,
            service_unavailable_retry: self.service_unavailable_retry,
        }
    }

    pub(crate) async fn post_json(
        &self,
        protocol: &'static str,
        body: &Value,
    ) -> Result<Value, ProviderError> {
        match self.post_stream(protocol, body, |_| Ok(())).await? {
            StreamBody::Json(value) => Ok(value),
            StreamBody::Events => Err(ProviderError::MissingTerminal {
                expected: "a JSON response body",
            }),
        }
    }

    pub(crate) async fn post_stream(
        &self,
        protocol: &'static str,
        body: &Value,
        mut on_event: impl FnMut(SseEvent) -> Result<(), ProviderError> + Send,
    ) -> Result<StreamBody, ProviderError> {
        let span = tracing::info_span!(
            "lam.model.http",
            protocol,
            method = "POST",
            endpoint = %sanitized_endpoint(&self.endpoint),
            status = field::Empty,
            request_id = field::Empty,
        );
        async {
            tracing::debug!(
                event = "http.request_started",
                body_bytes = serde_json::to_vec(body).map_or(0, |body| body.len()),
                "sending model HTTP request"
            );
            let mut retried_auth = false;
            let mut service_unavailable_retries = 0_u32;
            let response = loop {
                let mut request = self.client.post(self.endpoint.clone()).json(body);
                for (name, value) in &self.default_headers {
                    request = request.header(name, value);
                }
                if let Some(headers) = &self.request_headers {
                    for (name, value) in headers.headers() {
                        if let Some(name) = name {
                            request = request.header(name, value);
                        }
                    }
                }
                if let Some(authorization) = &self.authorization
                    && let Some(header) = authorization.authorization().await?
                {
                    request = request.header(AUTHORIZATION, header);
                }
                let response = match request.send().await {
                    Ok(response) => response,
                    Err(error) => {
                        let error = error.without_url();
                        trace_reqwest_error(&error, "send", 0, 0, 0, 0, 0);
                        return Err(ProviderError::Http(error));
                    }
                };
                let status = response.status();
                if status.as_u16() == 401
                    && !retried_auth
                    && let Some(authorization) = &self.authorization
                    && authorization.on_unauthorized().await?
                {
                    tracing::debug!(
                        event = "http.auth_retry",
                        "model endpoint returned 401; refreshed credentials and retrying once"
                    );
                    retried_auth = true;
                    // Drop the unauthorized body without materializing it.
                    drop(response);
                    continue;
                }
                // 503 is checked before any SSE/JSON body is read, so retries
                // never emit partial model deltas and never enter context.
                let policy = self.service_unavailable_retry;
                if status.as_u16() == 503 && service_unavailable_retries < policy.max_retries {
                    let attempt = service_unavailable_retries + 1;
                    let delay = policy.backoff(service_unavailable_retries);
                    service_unavailable_retries = attempt;
                    tracing::warn!(
                        event = "http.service_unavailable_retry",
                        attempt,
                        max_retries = policy.max_retries,
                        delay_ms = delay.as_millis() as u64,
                        "model endpoint returned 503; retrying after exponential backoff"
                    );
                    drop(response);
                    tokio::time::sleep(delay).await;
                    continue;
                }
                break response;
            };
            let status = response.status();
            tracing::Span::current().record("status", status.as_u16());
            if let Some(request_id) = response.headers().get("x-request-id")
                && let Ok(request_id) = request_id.to_str()
            {
                tracing::Span::current().record("request_id", request_id);
            }
            tracing::debug!(
                event = "http.response_headers",
                status = status.as_u16(),
                version = ?response.version(),
                content_type = ?header(response.headers(), "content-type"),
                content_length = ?header(response.headers(), "content-length"),
                content_encoding = ?header(response.headers(), "content-encoding"),
                transfer_encoding = ?header(response.headers(), "transfer-encoding"),
                request_id = ?header(response.headers(), "x-request-id")
                    .or_else(|| header(response.headers(), "request-id")),
                server = ?header(response.headers(), "server"),
                cf_ray = ?header(response.headers(), "cf-ray"),
                envoy_upstream_time = ?header(response.headers(), "x-envoy-upstream-service-time"),
                "received model HTTP response headers"
            );
            if !status.is_success() {
                let body = match response.bytes().await {
                    Ok(body) => body,
                    Err(error) => {
                        let error = error.without_url();
                        trace_reqwest_error(&error, "error_body", 0, 0, 0, 0, 0);
                        return Err(ProviderError::Http(error));
                    }
                };
                return Err(ProviderError::HttpStatus {
                    status: status.as_u16(),
                    body: bounded_body(&body),
                });
            }

            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let is_event_stream = content_type.as_deref().is_some_and(|value| {
                value.split(';').next().is_some_and(|media_type| {
                    media_type.trim().eq_ignore_ascii_case("text/event-stream")
                })
            });
            if !is_event_stream {
                let body = match response.bytes().await {
                    Ok(body) => body,
                    Err(error) => {
                        let error = error.without_url();
                        trace_reqwest_error(&error, "json_body", 0, 0, 0, 0, 0);
                        return Err(ProviderError::Http(error));
                    }
                };
                tracing::debug!(
                    event = "http.buffered_body_completed",
                    body_bytes = body.len(),
                    content_type = ?content_type,
                    "received complete model response body"
                );
                let json_error = match serde_json::from_slice(&body) {
                    Ok(value) => return Ok(StreamBody::Json(value)),
                    Err(error) => error,
                };
                if !looks_like_sse(&body) {
                    return Err(ProviderError::InvalidEventJson {
                        message: json_error.to_string(),
                    });
                }

                tracing::warn!(
                    event = "http.mislabeled_sse_body",
                    body_bytes = body.len(),
                    content_type = ?content_type,
                    "model response used SSE framing without an SSE content type"
                );
                let mut decoder = SseDecoder::default();
                let mut events = decoder.push(&body)?;
                events.extend(decoder.finish()?);
                for event in events {
                    on_event(event)?;
                }
                return Ok(StreamBody::Events);
            }

            let mut decoder = SseDecoder::default();
            let mut stream = response.bytes_stream();
            let started = Instant::now();
            let mut last_chunk = started;
            let mut chunk_count = 0_u64;
            let mut total_bytes = 0_u64;
            let mut event_count = 0_u64;
            loop {
                let next = match tokio::time::timeout(self.stream_idle_timeout, stream.next()).await
                {
                    Ok(next) => next,
                    Err(_) => {
                        tracing::error!(
                            event = "http.stream_idle",
                            chunk_count,
                            total_bytes,
                            event_count,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            timeout_ms = self.stream_idle_timeout.as_millis() as u64,
                            "model response stream stopped making progress"
                        );
                        return Err(ProviderError::StreamIdle {
                            timeout: self.stream_idle_timeout,
                        });
                    }
                };
                let Some(chunk) = next else {
                    break;
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        let error = error.without_url();
                        trace_reqwest_error(
                            &error,
                            "stream_body",
                            chunk_count,
                            total_bytes,
                            event_count,
                            decoder.pending_len(),
                            decoder.pending_data_lines(),
                        );
                        return Err(ProviderError::Http(error));
                    }
                };
                let now = Instant::now();
                chunk_count += 1;
                total_bytes = total_bytes.saturating_add(chunk.len() as u64);
                tracing::trace!(
                    event = "http.body_chunk",
                    chunk_index = chunk_count,
                    chunk_bytes = chunk.len(),
                    total_bytes,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    inter_chunk_ms = now.duration_since(last_chunk).as_millis() as u64,
                    decoder_pending_bytes = decoder.pending_len(),
                    "received model response body chunk"
                );
                last_chunk = now;
                let decoded = decoder.push(&chunk).inspect_err(|error| {
                    tracing::error!(
                        event = "http.sse_decode_failed",
                        error_kind = provider_error_kind(error),
                        chunk_count,
                        total_bytes,
                        event_count,
                        decoder_pending_bytes = decoder.pending_len(),
                        decoder_pending_data_lines = decoder.pending_data_lines(),
                        "failed to decode model SSE stream"
                    );
                })?;
                for event in decoded {
                    event_count += 1;
                    tracing::trace!(
                        event = "http.sse_event",
                        event_index = event_count,
                        event_type = ?event.event.as_deref(),
                        data_bytes = event.data.len(),
                        "decoded model SSE event"
                    );
                    if let Err(error) = on_event(event) {
                        tracing::error!(
                            event = "http.sse_callback_failed",
                            error_kind = provider_error_kind(&error),
                            chunk_count,
                            total_bytes,
                            event_count,
                            "model protocol rejected an SSE event"
                        );
                        return Err(error);
                    }
                }
            }
            let pending_bytes = decoder.pending_len();
            let pending_data_lines = decoder.pending_data_lines();
            let trailing = decoder.finish().inspect_err(|error| {
                tracing::error!(
                    event = "http.sse_finish_failed",
                    error_kind = provider_error_kind(error),
                    chunk_count,
                    total_bytes,
                    event_count,
                    decoder_pending_bytes = pending_bytes,
                    decoder_pending_data_lines = pending_data_lines,
                    "failed to finish model SSE stream"
                );
            })?;
            for event in trailing {
                event_count += 1;
                tracing::trace!(
                    event = "http.sse_event",
                    event_index = event_count,
                    event_type = ?event.event.as_deref(),
                    data_bytes = event.data.len(),
                    trailing = true,
                    "decoded trailing model SSE event"
                );
                if let Err(error) = on_event(event) {
                    tracing::error!(
                        event = "http.sse_callback_failed",
                        error_kind = provider_error_kind(&error),
                        chunk_count,
                        total_bytes,
                        event_count,
                        "model protocol rejected a trailing SSE event"
                    );
                    return Err(error);
                }
            }
            tracing::debug!(
                event = "http.stream_completed",
                chunk_count,
                total_bytes,
                event_count,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "model response body completed"
            );
            Ok(StreamBody::Events)
        }
        .instrument(span)
        .await
    }
}

fn sanitized_endpoint(endpoint: &reqwest::Url) -> String {
    let mut endpoint = endpoint.clone();
    let _ = endpoint.set_username("");
    let _ = endpoint.set_password(None);
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    endpoint.to_string()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn trace_reqwest_error(
    error: &reqwest::Error,
    phase: &'static str,
    chunk_count: u64,
    total_bytes: u64,
    event_count: u64,
    pending_bytes: usize,
    pending_data_lines: usize,
) {
    let mut sources = Vec::new();
    let mut source = error.source();
    while let Some(current) = source {
        sources.push(current.to_string());
        source = current.source();
    }
    tracing::error!(
        event = "http.reqwest_error",
        phase,
        error = %error,
        error_debug = ?error,
        source_chain = ?sources,
        is_body = error.is_body(),
        is_decode = error.is_decode(),
        is_timeout = error.is_timeout(),
        is_connect = error.is_connect(),
        is_request = error.is_request(),
        chunk_count,
        total_bytes,
        event_count,
        decoder_pending_bytes = pending_bytes,
        decoder_pending_data_lines = pending_data_lines,
        "model HTTP transport failed"
    );
}

const fn provider_error_kind(error: &ProviderError) -> &'static str {
    match error {
        ProviderError::UnexpectedRequestCodec { .. } => "unexpected_request_codec",
        ProviderError::InvalidRequest { .. } => "invalid_request",
        ProviderError::Auth { .. } => "auth",
        ProviderError::Http(_) => "http",
        ProviderError::StreamIdle { .. } => "stream_idle",
        ProviderError::HttpStatus { .. } => "http_status",
        ProviderError::InvalidEventStream { .. } => "invalid_event_stream",
        ProviderError::InvalidEventJson { .. } => "invalid_event_json",
        ProviderError::Api { .. } => "api",
        ProviderError::MissingTerminal { .. } => "missing_terminal",
        ProviderError::Codec { .. } => "codec",
    }
}

/// Inserts a header when both the name and value are valid.
pub fn try_insert_header(
    headers: &mut HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), ProviderError> {
    let header_name =
        HeaderName::from_bytes(name.as_bytes()).map_err(|error| ProviderError::Auth {
            message: format!("invalid HTTP header name `{name}`: {error}"),
        })?;
    let header_value = HeaderValue::from_str(value).map_err(|error| ProviderError::Auth {
        message: format!("invalid HTTP header value for `{name}`: {error}"),
    })?;
    headers.insert(header_name, header_value);
    Ok(())
}

fn bounded_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let mut bounded = text.chars().take(MAX_ERROR_BODY_CHARS).collect::<String>();
    if text.chars().count() > MAX_ERROR_BODY_CHARS {
        bounded.push('…');
    }
    bounded
}

fn looks_like_sse(body: &[u8]) -> bool {
    body.split(|byte| *byte == b'\n')
        .map(|line| line.trim_ascii())
        .find(|line| !line.is_empty())
        .is_some_and(|line| {
            line.starts_with(b"data:") || line.starts_with(b"event:") || line.starts_with(b":")
        })
}

#[derive(Default)]
struct SseDecoder {
    pending: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    const fn pending_len(&self) -> usize {
        self.pending.len()
    }

    const fn pending_data_lines(&self) -> usize {
        self.data.len()
    }

    fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, ProviderError> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&line, &mut events)?;
        }
        Ok(events)
    }

    fn finish(mut self) -> Result<Vec<SseEvent>, ProviderError> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            self.line(&pending, &mut events)?;
        }
        self.dispatch(&mut events);
        Ok(events)
    }

    fn line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) -> Result<(), ProviderError> {
        let line =
            std::str::from_utf8(line).map_err(|error| ProviderError::InvalidEventStream {
                message: format!("event line is not UTF-8: {error}"),
            })?;
        if line.is_empty() {
            self.dispatch(events);
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').map_or((line, ""), |(field, value)| {
                (field, value.strip_prefix(' ').unwrap_or(value))
            });
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        if !self.data.is_empty() {
            events.push(SseEvent {
                event: self.event.take(),
                data: self.data.join("\n"),
            });
            self.data.clear();
        } else {
            self.event = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use lam::ServiceUnavailableRetry;
    use serde_json::json;

    use super::{HttpTransport, SseDecoder, looks_like_sse, sanitized_endpoint};
    use crate::error::ProviderError;

    fn fast_retry(max_retries: u32) -> ServiceUnavailableRetry {
        ServiceUnavailableRetry::new(max_retries)
            .with_backoff(Duration::from_millis(5), Duration::from_millis(20))
    }

    fn make_transport(origin: &str, policy: ServiceUnavailableRetry) -> HttpTransport {
        HttpTransport::new(
            reqwest::Client::new(),
            reqwest::Url::parse(&format!("{origin}/v1/test")).unwrap(),
            None,
            Duration::from_secs(2),
            policy,
        )
    }

    #[test]
    fn decodes_split_crlf_and_multiline_events() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"event: example\r\nda").unwrap().is_empty());
        let events = decoder
            .push(b"ta: one\r\ndata: two\r\n\r\n")
            .expect("valid SSE");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("example"));
        assert_eq!(events[0].data, "one\ntwo");
    }

    #[test]
    fn identifies_sse_framing_without_using_body_content() {
        assert!(looks_like_sse(
            b"\r\nevent: response.completed\ndata: {}\n\n"
        ));
        assert!(looks_like_sse(b": keep-alive\n\ndata: {}\n\n"));
        assert!(!looks_like_sse(b"{\"event\":\"response.completed\"}"));
        assert!(!looks_like_sse(b"upstream service unavailable"));
    }

    #[test]
    fn diagnostic_endpoint_omits_credentials_query_and_fragment() {
        let endpoint = reqwest::Url::parse(
            "https://user:password@example.com/v1/chat/completions?api_key=secret#fragment",
        )
        .unwrap();
        assert_eq!(
            sanitized_endpoint(&endpoint),
            "https://example.com/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn retries_service_unavailable_then_succeeds() {
        let body = json!({"ok": true});
        let hits = Arc::new(Mutex::new(0_u32));
        let origin = spawn_status_sequence(
            hits.clone(),
            vec![
                (503, r#"{"error":"overloaded"}"#.to_owned()),
                (200, body.to_string()),
            ],
        );
        let transport = make_transport(&origin, fast_retry(2));

        let value = transport
            .post_json("test", &json!({"prompt": "hi"}))
            .await
            .expect("503 should be retried");
        assert_eq!(value, body);
        assert_eq!(*hits.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn exhausts_service_unavailable_retries() {
        let max_retries = 2_u32;
        let hits = Arc::new(Mutex::new(0_u32));
        let responses = (0..=max_retries)
            .map(|_| (503_u16, r#"{"error":"still overloaded"}"#.to_owned()))
            .collect();
        let origin = spawn_status_sequence(hits.clone(), responses);
        let transport = make_transport(&origin, fast_retry(max_retries));

        let error = transport
            .post_json("test", &json!({"prompt": "hi"}))
            .await
            .expect_err("exhausted 503 retries should surface once");
        match error {
            ProviderError::HttpStatus { status, body } => {
                assert_eq!(status, 503);
                assert!(body.contains("still overloaded"));
            }
            other => panic!("expected HttpStatus, got {other}"),
        }
        assert_eq!(
            *hits.lock().unwrap(),
            max_retries + 1,
            "one initial attempt plus each configured retry"
        );
    }

    #[tokio::test]
    async fn does_not_retry_non_service_unavailable_errors() {
        let hits = Arc::new(Mutex::new(0_u32));
        let origin = spawn_status_sequence(
            hits.clone(),
            vec![(429, r#"{"error":"rate limited"}"#.to_owned())],
        );
        let transport = make_transport(&origin, fast_retry(5));

        let error = transport
            .post_json("test", &json!({"prompt": "hi"}))
            .await
            .expect_err("429 must not be retried");
        assert!(matches!(
            error,
            ProviderError::HttpStatus { status: 429, .. }
        ));
        assert_eq!(*hits.lock().unwrap(), 1);
    }

    fn spawn_status_sequence(hits: Arc<Mutex<u32>>, responses: Vec<(u16, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let address = listener.local_addr().expect("mock address");
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                drain_http_request(&mut stream);
                *hits.lock().unwrap() += 1;
                let reason = match status {
                    200 => "OK",
                    429 => "Too Many Requests",
                    503 => "Service Unavailable",
                    _ => "Error",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).expect("write");
            }
        });
        format!("http://{address}")
    }

    fn drain_http_request(stream: &mut std::net::TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).expect("read request");
            assert!(read > 0, "request closed before headers");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = std::str::from_utf8(&bytes[..header_end]).expect("UTF-8 headers");
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or(0);
        while bytes.len() - header_end < content_length {
            let read = stream.read(&mut chunk).expect("read request body");
            assert!(read > 0, "request closed before body");
            bytes.extend_from_slice(&chunk[..read]);
        }
    }
}
