use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
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
            endpoint = %self.endpoint,
            status = field::Empty,
            request_id = field::Empty,
        );
        async {
            let mut request = self.client.post(self.endpoint.clone()).json(body);
            if let Some(authorization) = &self.authorization {
                request = request.header(AUTHORIZATION, authorization);
            }
            let response = request.send().await.map_err(ProviderError::Http)?;
            let status = response.status();
            tracing::Span::current().record("status", status.as_u16());
            if let Some(request_id) = response.headers().get("x-request-id")
                && let Ok(request_id) = request_id.to_str()
            {
                tracing::Span::current().record("request_id", request_id);
            }
            if !status.is_success() {
                let body = response.bytes().await.map_err(ProviderError::Http)?;
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
                let body = response.bytes().await.map_err(ProviderError::Http)?;
                let value = serde_json::from_slice(&body).map_err(|error| {
                    ProviderError::InvalidEventJson {
                        message: error.to_string(),
                    }
                })?;
                return Ok(StreamBody::Json(value));
            }

            let mut decoder = SseDecoder::default();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(ProviderError::Http)?;
                for event in decoder.push(&chunk)? {
                    on_event(event)?;
                }
            }
            for event in decoder.finish()? {
                on_event(event)?;
            }
            Ok(StreamBody::Events)
        }
        .instrument(span)
        .await
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
    use super::SseDecoder;

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
}
