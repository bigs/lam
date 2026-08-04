use std::time::Duration;

use lam::{CodecId, CodecRef, EncodedPayload, EvalRequest, OutputContract};
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::{BuildError, CodecError, ProviderError};
use crate::metadata::ModelPricing;
use crate::transport::HttpTransport;

pub(crate) const EVAL_TOOL_DESCRIPTION: &str = "Run one TypeScript program with top-level await in a persistent Deno isolate. Include a brief one-line intent describing the operation for the user. Top-level state persists across calls. Return a value with the final expression; `lam.result(value)` makes it explicit. Use registered lam APIs for host interaction. Put dependent work in one program and use `Promise.all` for independent work.";

const LEGACY_EVAL_INTENT: &str = "Evaluate TypeScript";
const MAX_EVAL_INTENT_CHARS: usize = 120;

pub(crate) const RESPONSES_REQUEST_CODEC_ID: &str = "openai/responses-request";
pub(crate) const RESPONSES_RESPONSE_CODEC_ID: &str = "openai/responses";
pub(crate) const CHAT_REQUEST_CODEC_ID: &str = "openai/chat-completions-request";
pub(crate) const CHAT_RESPONSE_CODEC_ID: &str = "openai/chat-completions";
pub(crate) const CODEC_VERSION: u32 = 1;

pub(crate) const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

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
    extra_body: Value,
    pricing: Option<ModelPricing>,
    client: Option<reqwest::Client>,
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
            extra_body: Value::Object(Map::new()),
            pricing: None,
            client: None,
        }
    }

    pub(crate) fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
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

    pub(crate) fn build(
        self,
        path: &'static str,
        require_api_key: bool,
    ) -> Result<BuiltConfig, BuildError> {
        if self.model.trim().is_empty() {
            return Err(BuildError::EmptyModel);
        }
        if require_api_key && self.api_key.is_none() {
            return Err(BuildError::MissingApiKey);
        }
        let Value::Object(extra_body) = self.extra_body else {
            return Err(BuildError::ExtraBodyMustBeObject);
        };
        if self.pricing.is_some_and(|pricing| !pricing.is_valid()) {
            return Err(BuildError::InvalidPricing);
        }
        let client = match self.client {
            Some(client) => client,
            None => reqwest::Client::builder()
                .build()
                .map_err(BuildError::HttpClient)?,
        };
        let authorization = self
            .api_key
            .map(|api_key| {
                let mut value = HeaderValue::from_str(&format!("Bearer {api_key}"))
                    .map_err(BuildError::InvalidApiKey)?;
                value.set_sensitive(true);
                Ok::<_, BuildError>(value)
            })
            .transpose()?;
        let endpoint = endpoint(&self.base_url, path)?;
        Ok(BuiltConfig {
            model: self.model,
            extra_body,
            pricing: self.pricing,
            transport: HttpTransport::new(client, endpoint, authorization),
        })
    }
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
                "description": "A TypeScript program to evaluate in the persistent Deno isolate. Top-level await is supported."
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

pub(crate) fn parse_eval_arguments(arguments: &str) -> Result<EvalRequest, CodecError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Arguments {
        #[serde(default)]
        intent: Option<String>,
        source: String,
        timeout_ms: Option<u64>,
    }

    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| CodecError::InvalidDirective {
            message: format!("eval arguments are invalid: {error}"),
        })?;
    if arguments.source.trim().is_empty() {
        return Err(CodecError::InvalidDirective {
            message: "eval source must not be empty".to_owned(),
        });
    }
    let intent = match arguments.intent {
        Some(intent) => {
            let intent = intent.trim();
            if intent.is_empty() {
                return Err(CodecError::InvalidDirective {
                    message: "eval intent must not be empty".to_owned(),
                });
            }
            if intent.contains(['\n', '\r']) {
                return Err(CodecError::InvalidDirective {
                    message: "eval intent must be one line".to_owned(),
                });
            }
            if intent.chars().count() > MAX_EVAL_INTENT_CHARS {
                return Err(CodecError::InvalidDirective {
                    message: format!(
                        "eval intent must be at most {MAX_EVAL_INTENT_CHARS} characters"
                    ),
                });
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
}
