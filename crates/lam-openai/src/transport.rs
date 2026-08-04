use std::error::Error as _;
use std::time::Instant;

use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;
use tracing::{Instrument, field};

use crate::error::ProviderError;

const MAX_ERROR_BODY_CHARS: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    authorization: Option<HeaderValue>,
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
    pub(crate) const fn new(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        authorization: Option<HeaderValue>,
    ) -> Self {
        Self {
            client,
            endpoint,
            authorization,
        }
    }

    pub(crate) fn child(&self, suffix: &str) -> Self {
        let mut endpoint = self.endpoint.clone();
        let path = format!("{}/{}", endpoint.path().trim_end_matches('/'), suffix);
        endpoint.set_path(&path);
        Self {
            client: self.client.clone(),
            endpoint,
            authorization: self.authorization.clone(),
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
            let mut request = self.client.post(self.endpoint.clone()).json(body);
            if let Some(authorization) = &self.authorization {
                request = request.header(AUTHORIZATION, authorization);
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

            let is_event_stream = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"));
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
                    event = "http.json_body_completed",
                    body_bytes = body.len(),
                    "received complete model JSON response"
                );
                let value = serde_json::from_slice(&body).map_err(|error| {
                    ProviderError::InvalidEventJson {
                        message: error.to_string(),
                    }
                })?;
                return Ok(StreamBody::Json(value));
            }

            let mut decoder = SseDecoder::default();
            let mut stream = response.bytes_stream();
            let started = Instant::now();
            let mut last_chunk = started;
            let mut chunk_count = 0_u64;
            let mut total_bytes = 0_u64;
            let mut event_count = 0_u64;
            while let Some(chunk) = stream.next().await {
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
        ProviderError::Http(_) => "http",
        ProviderError::HttpStatus { .. } => "http_status",
        ProviderError::InvalidEventStream { .. } => "invalid_event_stream",
        ProviderError::InvalidEventJson { .. } => "invalid_event_json",
        ProviderError::Api { .. } => "api",
        ProviderError::MissingTerminal { .. } => "missing_terminal",
    }
}

fn bounded_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let mut bounded = text.chars().take(MAX_ERROR_BODY_CHARS).collect::<String>();
    if text.chars().count() > MAX_ERROR_BODY_CHARS {
        bounded.push('…');
    }
    bounded
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
    use super::{SseDecoder, sanitized_endpoint};

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
}
