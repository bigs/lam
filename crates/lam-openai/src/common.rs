use std::time::Duration;

use lam::{CodecId, CodecRef, EncodedPayload, EvalRequest, ModelDirective, OutputContract};
use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::auth::{SharedAuthSource, StaticBearer};
use crate::error::{BuildError, CodecError, ProviderError};
use crate::metadata::ModelPricing;
use crate::transport::{HttpTransport, RequestHeaderSource};

pub(crate) const EVAL_TOOL_DESCRIPTION: &str = "Run one TypeScript program with top-level await in a persistent Deno isolate. Emit at most one eval tool call in the current model output. Do not emit sibling or parallel eval calls together; Lam executes only the first. After receiving its tool result, you may issue another eval call in the next assistant continuation, so normal inspect, edit, and test loops are supported. Within one eval program, await dependent operations sequentially and use Promise.all for work that should actually run concurrently. Include a brief one-line intent describing the operation for the user. Top-level state persists across calls. Return a value with the final expression; lam.result(value) makes it explicit. Pass structured values directly to lam.result without JSON.stringify; the runtime handles encoding. Use registered lam APIs for host interaction. Ordinary TypeScript template literals are fine, including ${interpolation}. A bare backtick character inside a template literal is invalid TypeScript and aborts transpile; escape it as \\` or, for multi-line payloads that contain backticks (patches, markdown, shell), pass a string[] of lines instead (lam.edit.apply patch and lam.edit.write content accept string | string[]).";

const LEGACY_EVAL_INTENT: &str = "Evaluate TypeScript";
const MAX_EVAL_INTENT_CHARS: usize = 120;

pub(crate) const RESPONSES_REQUEST_CODEC_ID: &str = "openai/responses-request";
pub(crate) const RESPONSES_RESPONSE_CODEC_ID: &str = "openai/responses";
pub(crate) const CHAT_REQUEST_CODEC_ID: &str = "openai/chat-completions-request";
pub(crate) const CHAT_RESPONSE_CODEC_ID: &str = "openai/chat-completions";
pub(crate) const CODEC_VERSION: u32 = 1;

pub(crate) const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub(crate) const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OutputKind {
    Text,
    Structured,
}

impl OutputKind {
    pub(crate) const fn from_contract(output: &OutputContract) -> Self {
        match output {
            OutputContract::Text => Self::Text,
            OutputContract::Structured { .. } => Self::Structured,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestEnvelope {
    pub(crate) output_kind: OutputKind,
    pub(crate) body: Value,
}

pub(crate) struct ResponseEnvelope<'a> {
    pub(crate) output_kind: OutputKind,
    pub(crate) value: &'a Value,
}

pub(crate) struct SharedBuilder {
    model: String,
    base_url: String,
    api_key: Option<String>,
    auth: Option<SharedAuthSource>,
    default_headers: HeaderMap,
    request_headers: Option<std::sync::Arc<dyn RequestHeaderSource>>,
    extra_body: Value,
    pricing: Option<ModelPricing>,
    client: Option<reqwest::Client>,
    stream_idle_timeout: Duration,
}

pub(crate) struct BuiltConfig {
    pub(crate) model: String,
    pub(crate) extra_body: Map<String, Value>,
    pub(crate) pricing: Option<ModelPricing>,
    pub(crate) transport: HttpTransport,
}

impl SharedBuilder {
    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: DEFAULT_OPENAI_BASE_URL.to_owned(),
            api_key: None,
            auth: None,
            default_headers: HeaderMap::new(),
            request_headers: None,
            extra_body: Value::Object(Map::new()),
            pricing: None,
            client: None,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
        }
    }

    pub(crate) fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    pub(crate) fn auth_source(mut self, auth: SharedAuthSource) -> Self {
        self.auth = Some(auth);
        self
    }

    pub(crate) fn default_headers(mut self, headers: HeaderMap) -> Self {
        self.default_headers = headers;
        self
    }

    pub(crate) fn request_headers(
        mut self,
        headers: std::sync::Arc<dyn RequestHeaderSource>,
    ) -> Self {
        self.request_headers = Some(headers);
        self
    }

    pub(crate) fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub(crate) fn extra_body(mut self, extra_body: Value) -> Self {
        self.extra_body = extra_body;
        self
    }

    pub(crate) fn pricing(mut self, pricing: ModelPricing) -> Self {
        self.pricing = Some(pricing);
        self
    }

    pub(crate) fn http_client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(client);
        self
    }

    pub(crate) fn stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.stream_idle_timeout = timeout;
        self
    }

    pub(crate) fn build(
        self,
        path: &'static str,
        require_api_key: bool,
    ) -> Result<BuiltConfig, BuildError> {
        if self.model.trim().is_empty() {
            return Err(BuildError::EmptyModel);
        }
        if require_api_key && self.api_key.is_none() && self.auth.is_none() {
            return Err(BuildError::MissingApiKey);
        }
        let Value::Object(extra_body) = self.extra_body else {
            return Err(BuildError::ExtraBodyMustBeObject);
        };
        if self.pricing.is_some_and(|pricing| !pricing.is_valid()) {
            return Err(BuildError::InvalidPricing);
        }
        if self.stream_idle_timeout.is_zero() {
            return Err(BuildError::InvalidStreamIdleTimeout);
        }
        let client = match self.client {
            Some(client) => client,
            None => reqwest::Client::builder()
                .build()
                .map_err(BuildError::HttpClient)?,
        };
        let authorization = resolve_authorization(self.auth, self.api_key)?;
        let endpoint = endpoint(&self.base_url, path)?;
        let mut transport =
            HttpTransport::new(client, endpoint, authorization, self.stream_idle_timeout);
        if !self.default_headers.is_empty() {
            transport = transport.with_default_headers(self.default_headers);
        }
        if let Some(headers) = self.request_headers {
            transport = transport.with_request_headers(headers);
        }
        Ok(BuiltConfig {
            model: self.model,
            extra_body,
            pricing: self.pricing,
            transport,
        })
    }
}

fn resolve_authorization(
    auth: Option<SharedAuthSource>,
    api_key: Option<String>,
) -> Result<Option<SharedAuthSource>, BuildError> {
    if let Some(auth) = auth {
        return Ok(Some(auth));
    }
    let Some(api_key) = api_key else {
        return Ok(None);
    };
    // Validate the header shape with the same error type as before.
    let mut value =
        HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(BuildError::InvalidApiKey)?;
    value.set_sensitive(true);
    let _ = value;
    Ok(Some(Arc::new(
        StaticBearer::new(api_key).expect("header validation already succeeded"),
    )))
}

fn endpoint(base_url: &str, path: &str) -> Result<reqwest::Url, BuildError> {
    let base = reqwest::Url::parse(base_url).map_err(|error| BuildError::InvalidBaseUrl {
        message: error.to_string(),
    })?;
    if base.scheme() != "http" && base.scheme() != "https" {
        return Err(BuildError::InvalidBaseUrl {
            message: "scheme must be http or https".to_owned(),
        });
    }
    if base.cannot_be_a_base()
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(BuildError::InvalidBaseUrl {
            message: "URL must have a host and no credentials, query, or fragment".to_owned(),
        });
    }
    let joined = format!("{}{}", base.as_str().trim_end_matches('/'), path);
    reqwest::Url::parse(&joined).map_err(|error| BuildError::InvalidBaseUrl {
        message: error.to_string(),
    })
}

pub(crate) fn request_payload(codec_id: &str, output: OutputKind, body: Value) -> EncodedPayload {
    EncodedPayload::new(
        codec(codec_id),
        json!({
            "outputKind": output,
            "body": body,
        }),
    )
}

pub(crate) fn parse_request(
    request: EncodedPayload,
    expected_codec_id: &str,
) -> Result<RequestEnvelope, ProviderError> {
    let expected = codec(expected_codec_id);
    if request.codec != expected {
        return Err(ProviderError::UnexpectedRequestCodec {
            expected: display_codec(&expected),
            received: display_codec(&request.codec),
        });
    }
    serde_json::from_value(request.value).map_err(|error| ProviderError::InvalidRequest {
        message: error.to_string(),
    })
}

pub(crate) fn response_payload(
    codec_id: &str,
    output_kind: OutputKind,
    model: &str,
    field: &str,
    value: Value,
) -> EncodedPayload {
    let mut envelope = Map::new();
    envelope.insert(
        "outputKind".to_owned(),
        serde_json::to_value(output_kind).expect("OutputKind is serializable"),
    );
    envelope.insert("model".to_owned(), Value::String(model.to_owned()));
    envelope.insert(field.to_owned(), value);
    EncodedPayload::new(codec(codec_id), Value::Object(envelope))
}

pub(crate) fn parse_response<'a>(
    response: &'a EncodedPayload,
    expected_codec_id: &str,
    field: &str,
) -> Result<ResponseEnvelope<'a>, CodecError> {
    let expected = codec(expected_codec_id);
    if response.codec != expected {
        return Err(CodecError::UnexpectedResponseCodec {
            expected: display_codec(&expected),
            received: display_codec(&response.codec),
        });
    }
    let output_kind = response
        .value
        .get("outputKind")
        .cloned()
        .ok_or_else(|| CodecError::InvalidPayload {
            message: "response envelope has no outputKind".to_owned(),
        })
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| CodecError::InvalidPayload {
                message: format!("invalid response outputKind: {error}"),
            })
        })?;
    let value = response
        .value
        .get(field)
        .ok_or_else(|| CodecError::InvalidPayload {
            message: format!("response envelope has no {field}"),
        })?;
    Ok(ResponseEnvelope { output_kind, value })
}

pub(crate) fn eval_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "intent": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_EVAL_INTENT_CHARS,
                "description": "A brief one-line description of what this program is intended to accomplish, written for the user."
            },
            "source": {
                "type": "string",
                "description": "A TypeScript program to evaluate in the persistent Deno isolate. Top-level await is supported. Template literals are ordinary TypeScript; if a string body must contain backtick characters, escape them or build the text from double-quoted strings / a string[] joined with newlines (lam.edit patch and content accept string | string[] for this)."
            },
            "timeoutMs": {
                "type": ["integer", "null"],
                "minimum": 1,
                "description": "Requested timeout in milliseconds, bounded by the host, or null for the host default."
            }
        },
        "required": ["intent", "source", "timeoutMs"],
        "additionalProperties": false
    })
}

/// Parses eval call arguments, or explains in model-addressed language why
/// they are invalid so the runtime can return the reason as the call's
/// rejection result.
pub(crate) fn parse_eval_arguments(arguments: &str) -> Result<EvalRequest, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Arguments {
        #[serde(default)]
        intent: Option<String>,
        source: String,
        // Providers without strict schema enforcement habitually emit
        // snake_case field names; accept the alias rather than fail the call.
        #[serde(alias = "timeout_ms")]
        timeout_ms: Option<u64>,
    }

    let arguments: Arguments = serde_json::from_str(arguments)
        .map_err(|error| format!("eval arguments are invalid: {error}"))?;
    if arguments.source.trim().is_empty() {
        return Err("eval source must not be empty".to_owned());
    }
    let intent = match arguments.intent {
        Some(intent) => {
            let intent = intent.trim();
            if intent.is_empty() {
                return Err("eval intent must not be empty".to_owned());
            }
            if intent.contains(['\n', '\r']) {
                return Err("eval intent must be one line".to_owned());
            }
            if intent.chars().count() > MAX_EVAL_INTENT_CHARS {
                return Err(format!(
                    "eval intent must be at most {MAX_EVAL_INTENT_CHARS} characters"
                ));
            }
            intent.to_owned()
        }
        // Version-one journals may contain provider-native eval calls from
        // before intent became part of the public tool schema.
        None => LEGACY_EVAL_INTENT.to_owned(),
    };
    Ok(EvalRequest {
        intent,
        source: arguments.source,
        timeout: arguments.timeout_ms.map(Duration::from_millis),
    })
}

/// Rejection directive for an eval call whose arguments could not be used.
pub(crate) fn invalid_eval_rejection(reason: &str) -> ModelDirective {
    ModelDirective::Rejected {
        message: format!(
            "This eval call was not executed: {reason}. Send one corrected eval call whose JSON arguments match the tool schema: `intent`, `source`, and `timeoutMs`."
        ),
    }
}

/// Rejection directive for a call to a function that does not exist.
pub(crate) fn unsupported_function_rejection(name: &str) -> ModelDirective {
    ModelDirective::Rejected {
        message: format!(
            "This call was not executed because `{name}` is not an available function. The only available function is `eval`."
        ),
    }
}

pub(crate) fn output_value(output_kind: OutputKind, text: String) -> Result<Value, CodecError> {
    match output_kind {
        OutputKind::Text => Ok(Value::String(text)),
        OutputKind::Structured => {
            serde_json::from_str(&text).map_err(|error| CodecError::InvalidDirective {
                message: format!("structured model output is not valid JSON: {error}"),
            })
        }
    }
}

pub(crate) fn codec(id: &str) -> CodecRef {
    CodecRef::new(
        CodecId::new(id).expect("lam-openai codec identifiers are valid"),
        CODEC_VERSION,
    )
}

pub(crate) fn display_codec(codec: &CodecRef) -> String {
    format!("{}@{}", codec.id, codec.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_intent_is_trimmed_and_must_be_one_brief_line() {
        let request = parse_eval_arguments(
            r#"{"intent":"  Inspect the workspace  ","source":"1 + 1","timeoutMs":null}"#,
        )
        .expect("valid eval arguments");
        assert_eq!(request.intent, "Inspect the workspace");

        let multiline = parse_eval_arguments(
            r#"{"intent":"Inspect\nthe workspace","source":"1 + 1","timeoutMs":null}"#,
        )
        .expect_err("multiline intent should be rejected");
        assert!(multiline.to_string().contains("must be one line"));

        let too_long = "x".repeat(MAX_EVAL_INTENT_CHARS + 1);
        let arguments = json!({
            "intent": too_long,
            "source": "1 + 1",
            "timeoutMs": null
        });
        let too_long = parse_eval_arguments(&arguments.to_string())
            .expect_err("overlong intent should be rejected");
        assert!(too_long.to_string().contains("at most 120 characters"));
    }

    #[test]
    fn legacy_eval_arguments_receive_a_stable_fallback_intent() {
        let request = parse_eval_arguments(r#"{"source":"1 + 1","timeoutMs":null}"#)
            .expect("version-one eval arguments remain readable");
        assert_eq!(request.intent, LEGACY_EVAL_INTENT);
    }

    #[test]
    fn snake_case_timeout_is_accepted_as_an_alias() {
        let request = parse_eval_arguments(r#"{"intent":"Sum","source":"1 + 1","timeout_ms":250}"#)
            .expect("snake_case timeout should parse via the alias");
        assert_eq!(request.timeout, Some(Duration::from_millis(250)));
    }
}
