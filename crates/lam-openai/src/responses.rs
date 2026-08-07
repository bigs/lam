//! OpenAI Responses API adapter.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use lam::{
    CodecId, CodecRef, CompactionArtifact, CompactionError, CompactionOutput, CompactionPlan,
    CompactionRequest, Compactor, ContextTransition, EncodedPayload, Model, ModelCodec, ModelDelta,
    ModelDescriptor, ModelDirective, ModelEventSink, ModelProvider, ModelRequestConfig,
    ModelResponseMetadata, ModelResponseProjection, OutputContract, ProjectedContextEntry,
    ServiceUnavailableRetry, ToolCallDelta,
};
use serde_json::{Map, Value, json};

use reqwest::header::HeaderMap;

use crate::auth::SharedAuthSource;
use crate::common::{
    BuiltConfig, CODEC_VERSION, EVAL_TOOL_DESCRIPTION, OutputKind, RESPONSES_REQUEST_CODEC_ID,
    RESPONSES_RESPONSE_CODEC_ID, SharedBuilder, codec, eval_parameters, invalid_eval_rejection,
    output_value, parse_eval_arguments, parse_request, parse_response, request_payload,
    response_payload, unsupported_function_rejection,
};
use crate::context::{
    LAM_CODEC_VERSION, LAM_EVAL_CODEC_ID, LAM_MESSAGES_CODEC_ID, NativeRole, compaction_record,
    compaction_text, eval_output, is_codec, messages, unsupported,
};
use crate::error::{BuildError, CodecError, ProviderError};
use crate::metadata::{ModelPricing, UsageDialect, response_metadata};
use crate::transport::{RequestHeaderSource, StreamBody};

const RESPONSES_PATH: &str = "/responses";
const COMPACTION_REPLACEMENT_CODEC_ID: &str = "openai/responses-compaction";
const COMPACTION_RESPONSE_CODEC_ID: &str = "openai/responses-compaction-response";
const RESPONSES_DESCRIPTOR_CODEC: &str = "openai/responses";

/// Codec identifier for encoded Responses request bodies.
pub const REQUEST_CODEC_ID: &str = RESPONSES_REQUEST_CODEC_ID;

/// Codec identifier for completed, provider-native Responses payloads.
pub const RESPONSE_CODEC_ID: &str = RESPONSES_RESPONSE_CODEC_ID;

/// Current Responses request and response representation version.
pub const PAYLOAD_VERSION: u32 = CODEC_VERSION;

/// Entry point for configuring OpenAI's Responses API.
pub struct Responses;

impl Responses {
    /// Starts a Responses adapter builder for `model`.
    #[must_use]
    pub fn builder(model: impl Into<String>) -> ResponsesBuilder {
        ResponsesBuilder {
            shared: SharedBuilder::new(model),
        }
    }
}

/// Configures an OpenAI Responses provider and its lossless context codec.
pub struct ResponsesBuilder {
    shared: SharedBuilder,
}

impl ResponsesBuilder {
    /// Sets the OpenAI API key used as an HTTP bearer token.
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.shared = self.shared.api_key(api_key);
        self
    }

    /// Uses a dynamic authorization source such as a refreshable OAuth token.
    ///
    /// When both an API key and an auth source are configured, the auth source
    /// takes precedence.
    #[must_use]
    pub fn auth_source(mut self, auth: SharedAuthSource) -> Self {
        self.shared = self.shared.auth_source(auth);
        self
    }

    /// Adds default headers sent on every request to this provider.
    #[must_use]
    pub fn default_headers(mut self, headers: HeaderMap) -> Self {
        self.shared = self.shared.default_headers(headers);
        self
    }

    /// Adds headers that are regenerated for each outbound request.
    #[must_use]
    pub fn request_headers(mut self, headers: std::sync::Arc<dyn RequestHeaderSource>) -> Self {
        self.shared = self.shared.request_headers(headers);
        self
    }

    /// Replaces the API base URL. The default is `https://api.openai.com/v1`.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.shared = self.shared.base_url(base_url);
        self
    }

    /// Adds provider-specific top-level request fields.
    ///
    /// lam overwrites protocol invariants including `model`, `input`,
    /// `instructions`, `store`, `stream`, `tools`, and
    /// `parallel_tool_calls`. Lam omits `parallel_tool_calls` so the provider
    /// can stream a complete native tool-call batch; the runtime executes only
    /// the first eval and rejects any siblings.
    #[must_use]
    pub fn extra_body(mut self, extra_body: Value) -> Self {
        self.shared = self.shared.extra_body(extra_body);
        self
    }

    /// Configures USD token prices used for best-effort cost estimates.
    #[must_use]
    pub fn pricing(mut self, pricing: ModelPricing) -> Self {
        self.shared = self.shared.pricing(pricing);
        self
    }

    /// Uses an embedding-provided HTTP client.
    #[must_use]
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.shared = self.shared.http_client(client);
        self
    }

    /// Sets the maximum permitted delay between streaming response body chunks.
    ///
    /// An idle stream fails the provider request. Partially streamed responses
    /// are never projected into output or eval directives.
    #[must_use]
    pub fn stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.shared = self.shared.stream_idle_timeout(timeout);
        self
    }

    /// Configures automatic retries when the endpoint returns HTTP 503.
    ///
    /// Intermediate 503 responses never enter model-visible context. After the
    /// configured budget is exhausted, the host receives a provider error.
    /// Defaults to [`ServiceUnavailableRetry::default`] (12 retries, 250ms
    /// initial backoff, 512s cap so the full budget stays exponential).
    #[must_use]
    pub fn service_unavailable_retry(mut self, policy: ServiceUnavailableRetry) -> Self {
        self.shared = self.shared.service_unavailable_retry(policy);
        self
    }

    /// Builds a model ready for [`lam::Lam::builder`].
    pub fn build(self) -> Result<Model<ResponsesProvider, ResponsesCodec>, BuildError> {
        let (provider, codec) = self.build_parts()?;
        let descriptor = ModelDescriptor::new("openai", codec.model(), RESPONSES_DESCRIPTOR_CODEC)
            .expect("the Responses descriptor is nonempty");
        Ok(Model::new(provider, codec).with_descriptor(descriptor))
    }

    /// Builds the transport and codec separately for custom composition.
    pub fn build_parts(self) -> Result<(ResponsesProvider, ResponsesCodec), BuildError> {
        let BuiltConfig {
            model,
            extra_body,
            pricing,
            transport,
        } = self.shared.build(RESPONSES_PATH, true)?;
        Ok((
            ResponsesProvider { transport },
            ResponsesCodec {
                model,
                extra_body,
                pricing,
            },
        ))
    }
}

/// HTTP transport for the streaming Responses endpoint.
pub struct ResponsesProvider {
    transport: crate::transport::HttpTransport,
}

impl ModelProvider for ResponsesProvider {
    type Error = ProviderError;

    fn invoke(
        &self,
        request: EncodedPayload,
        events: ModelEventSink,
    ) -> impl Future<Output = Result<EncodedPayload, Self::Error>> + Send {
        let transport = self.transport.clone();
        async move {
            let request = parse_request(request, RESPONSES_REQUEST_CODEC_ID)?;
            let model = request
                .body
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::InvalidRequest {
                    message: "Responses request has no model".to_owned(),
                })?
                .to_owned();
            let mut completed = None;
            let body = transport
                .post_stream("responses", &request.body, |event| {
                    if event.data == "[DONE]" {
                        return Ok(());
                    }
                    let value: Value = serde_json::from_str(&event.data).map_err(|error| {
                        ProviderError::InvalidEventJson {
                            message: error.to_string(),
                        }
                    })?;
                    let kind = value
                        .get("type")
                        .and_then(Value::as_str)
                        .or(event.event.as_deref())
                        .unwrap_or_default();
                    match kind {
                        "response.output_text.delta" => emit_delta(&events, &value, false),
                        "response.reasoning_text.delta"
                        | "response.reasoning_summary_text.delta" => {
                            emit_delta(&events, &value, true);
                        }
                        "response.output_item.added" => emit_tool_item(&events, &value),
                        "response.function_call_arguments.delta" => {
                            emit_tool_arguments(&events, &value);
                        }
                        "response.completed" => {
                            completed = value.get("response").cloned();
                            if completed.is_none() {
                                return Err(ProviderError::InvalidEventJson {
                                    message: "response.completed has no response".to_owned(),
                                });
                            }
                        }
                        "error" | "response.failed" | "response.incomplete" => {
                            return Err(ProviderError::Api {
                                message: value.to_string(),
                            });
                        }
                        _ => {}
                    }
                    Ok(())
                })
                .await?;
            if let StreamBody::Json(response) = body {
                completed = Some(response);
            }
            let response = completed.ok_or(ProviderError::MissingTerminal {
                expected: "response.completed",
            })?;
            if response.get("error").is_some_and(|error| !error.is_null()) {
                return Err(ProviderError::Api {
                    message: response["error"].to_string(),
                });
            }
            Ok(response_payload(
                RESPONSES_RESPONSE_CODEC_ID,
                request.output_kind,
                &model,
                "response",
                response,
            ))
        }
    }

    fn is_context_overflow(&self, error: &Self::Error) -> bool {
        error.is_context_overflow()
    }
}

fn emit_delta(events: &ModelEventSink, value: &Value, reasoning: bool) {
    if let Some(delta) = value
        .get("delta")
        .and_then(Value::as_str)
        .filter(|delta| !delta.is_empty())
    {
        let delta = if reasoning {
            ModelDelta::Reasoning(delta.to_owned())
        } else {
            ModelDelta::Text(delta.to_owned())
        };
        events.emit(delta);
    }
}

fn emit_tool_item(events: &ModelEventSink, value: &Value) {
    let Some(item) = value.get("item") else {
        return;
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return;
    }
    let index = value
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(0);
    events.emit(ModelDelta::ToolCall(ToolCallDelta {
        index,
        call_id: item
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        name: item.get("name").and_then(Value::as_str).map(str::to_owned),
        arguments: item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }));
}

fn emit_tool_arguments(events: &ModelEventSink, value: &Value) {
    let Some(arguments) = value.get("delta").and_then(Value::as_str) else {
        return;
    };
    let index = value
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(0);
    events.emit(ModelDelta::ToolCall(ToolCallDelta {
        index,
        call_id: None,
        name: None,
        arguments: arguments.to_owned(),
    }));
}

/// Pure Responses request/replay codec.
#[derive(Clone)]
pub struct ResponsesCodec {
    model: String,
    extra_body: Map<String, Value>,
    pricing: Option<ModelPricing>,
}

impl ResponsesCodec {
    /// Returns the configured request model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    fn compaction_body(
        &self,
        context: &[ProjectedContextEntry],
        instructions: &str,
    ) -> Result<Value, CodecError> {
        let mut body = Map::new();
        body.insert("model".to_owned(), Value::String(self.model.clone()));
        body.insert("input".to_owned(), Value::Array(encode_context(context)?));
        if !instructions.is_empty() {
            body.insert(
                "instructions".to_owned(),
                Value::String(instructions.to_owned()),
            );
        }
        Ok(Value::Object(body))
    }
}

/// OpenAI's standalone Responses compaction endpoint as a Lam compactor.
///
/// The complete provider response is retained as provenance and its `output`
/// array becomes the exact replay checkpoint. Model eligibility is deliberately
/// left to explicit runtime configuration and provider errors.
pub struct OpenAiResponsesCompactor {
    transport: crate::transport::HttpTransport,
    codec: Arc<ResponsesCodec>,
    descriptor: ModelDescriptor,
}

impl OpenAiResponsesCompactor {
    /// Uses the credentials, endpoint, model, and codec of a Responses model.
    #[must_use]
    pub fn new(model: &Model<ResponsesProvider, ResponsesCodec>) -> Self {
        let (provider, codec) = model.shared_parts();
        Self {
            transport: provider.transport.child("compact"),
            codec,
            descriptor: model.descriptor().clone(),
        }
    }
}

impl Compactor for OpenAiResponsesCompactor {
    fn compact<'a>(&'a self, request: &'a CompactionRequest) -> lam::CompactionFuture<'a> {
        Box::pin(async move {
            if let Some(target) = &request.target_model
                && target.codec() != self.descriptor.codec()
            {
                return Err(CompactionError::new(format!(
                    "native Responses checkpoint is incompatible with target codec `{}`",
                    target.codec()
                )));
            }
            let covers_through = request
                .units
                .last()
                .ok_or_else(|| CompactionError::new("context has no atomic unit to compact"))?
                .covers_through();
            let body = self
                .codec
                .compaction_body(&request.context, &request.instructions)
                .map_err(|error| CompactionError::new(error.to_string()))?;
            let response = self
                .transport
                .post_json("responses_compact", &body)
                .await
                .map_err(|error| CompactionError::new(error.to_string()))?;
            if response.get("object").and_then(Value::as_str) != Some("response.compaction") {
                return Err(CompactionError::new(
                    "Responses compaction endpoint returned an unexpected object",
                ));
            }
            let output = response
                .get("output")
                .and_then(Value::as_array)
                .filter(|output| !output.is_empty())
                .ok_or_else(|| {
                    CompactionError::new(
                        "Responses compaction endpoint returned no canonical output",
                    )
                })?
                .clone();
            let model = response
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(self.codec.model())
                .to_owned();
            let metadata = response_metadata(
                model,
                response.get("usage"),
                UsageDialect::Responses,
                self.codec.pricing,
            );
            Ok(CompactionPlan {
                strategy: "openai-responses-native".to_owned(),
                covers_through,
                output: CompactionOutput::exact(EncodedPayload::new(
                    compaction_replacement_codec(),
                    Value::Array(output),
                )),
                source: Some(EncodedPayload::new(
                    codec(COMPACTION_RESPONSE_CODEC_ID),
                    response,
                )),
                metadata,
            })
        })
    }
}

impl ModelCodec for ResponsesCodec {
    type Error = CodecError;

    fn encode_request(
        &self,
        context: &[ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
    ) -> Result<EncodedPayload, Self::Error> {
        let input = encode_context(context)?;
        let mut body = self.extra_body.clone();
        let include = include_encrypted_reasoning(body.remove("include"))?;
        body.insert("model".to_owned(), Value::String(self.model.clone()));
        body.insert("input".to_owned(), Value::Array(input));
        body.remove("instructions");
        if !config.system_prompt.is_empty() {
            body.insert(
                "instructions".to_owned(),
                Value::String(config.system_prompt.to_owned()),
            );
        }
        body.insert("store".to_owned(), Value::Bool(false));
        body.insert("stream".to_owned(), Value::Bool(true));
        body.insert("include".to_owned(), Value::Array(include));
        body.remove("tools");
        body.remove("tool_choice");
        body.remove("parallel_tool_calls");
        if config.enable_eval {
            body.insert("tools".to_owned(), Value::Array(vec![eval_tool()]));
            body.insert("tool_choice".to_owned(), Value::String("auto".to_owned()));
        }
        if let Some(max_output_tokens) = config.max_output_tokens {
            body.insert(
                "max_output_tokens".to_owned(),
                Value::Number(max_output_tokens.into()),
            );
        }
        if let OutputContract::Structured { schema } = config.output {
            let mut text = body
                .remove("text")
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            text.insert(
                "format".to_owned(),
                json!({
                    "type": "json_schema",
                    "name": "lam_output",
                    "schema": schema,
                    "strict": true
                }),
            );
            body.insert("text".to_owned(), Value::Object(text));
        }
        Ok(request_payload(
            RESPONSES_REQUEST_CODEC_ID,
            OutputKind::from_contract(config.output),
            Value::Object(body),
        ))
    }

    fn project_response(
        &self,
        response: &EncodedPayload,
    ) -> Result<ModelResponseProjection, Self::Error> {
        let envelope = parse_response(response, RESPONSES_RESPONSE_CODEC_ID, "response")?;
        if let Some(status) = envelope.value.get("status").and_then(Value::as_str)
            && status != "completed"
        {
            return Err(CodecError::InvalidDirective {
                message: format!("Responses status is {status}, not completed"),
            });
        }
        let calls = function_calls(envelope.value)?;
        let rejected_eval_calls = calls.len().saturating_sub(1);
        let display = response_display_deltas(envelope.value);
        let directive = if let Some(call) = calls.first() {
            if call.name == "eval" {
                match parse_eval_arguments(&call.arguments) {
                    Ok(request) => ModelDirective::Eval(request),
                    Err(reason) => invalid_eval_rejection(&reason),
                }
            } else {
                unsupported_function_rejection(&call.name)
            }
        } else {
            let text = response_text(envelope.value)?;
            output_value(envelope.output_kind, text).map(ModelDirective::Output)?
        };
        Ok(ModelResponseProjection {
            display,
            directive,
            rejected_eval_calls,
        })
    }

    fn response_metadata(&self, response: &EncodedPayload) -> ModelResponseMetadata {
        let native = response.value.get("response");
        let model = native
            .and_then(|response| response.get("model"))
            .and_then(Value::as_str)
            .unwrap_or(&self.model)
            .to_owned();
        response_metadata(
            model,
            native.and_then(|response| response.get("usage")),
            UsageDialect::Responses,
            self.pricing,
        )
    }

    fn materialize_compaction(
        &self,
        artifact: &CompactionArtifact,
    ) -> Result<Option<EncodedPayload>, Self::Error> {
        Ok(Some(EncodedPayload::new(
            compaction_replacement_codec(),
            json!([{
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": compaction_text(artifact),
                }]
            }]),
        )))
    }

    fn accepts_compaction_replacement(&self, replacement: &EncodedPayload) -> bool {
        replacement.codec == compaction_replacement_codec()
    }
}

fn encode_context(context: &[ProjectedContextEntry]) -> Result<Vec<Value>, CodecError> {
    let mut input = Vec::new();
    let mut pending_eval = VecDeque::new();
    for projected in context {
        let payload = &projected.entry.payload;
        if matches!(
            projected.entry.transition,
            ContextTransition::Compaction { .. }
        ) {
            if !pending_eval.is_empty() {
                return Err(CodecError::InvalidPayload {
                    message: "compaction replacement cannot split an eval call and result"
                        .to_owned(),
                });
            }
            let record = compaction_record(payload)?;
            if record.replacement.codec != compaction_replacement_codec() {
                return Err(unsupported(&record.replacement));
            }
            let replacement =
                record
                    .replacement
                    .value
                    .as_array()
                    .ok_or_else(|| CodecError::InvalidPayload {
                        message: "Responses compaction replacement is not an item array".to_owned(),
                    })?;
            input.extend(replacement.iter().cloned());
        } else if is_codec(payload, LAM_MESSAGES_CODEC_ID, LAM_CODEC_VERSION) {
            for message in messages(&payload.value)? {
                if message.closes_interrupted_eval {
                    for call_id in pending_eval.drain(..) {
                        input.push(function_output(call_id, message.text.clone()));
                    }
                }
                let role = match message.role {
                    NativeRole::User => "user",
                    NativeRole::System => "developer",
                };
                input.push(json!({
                    "role": role,
                    "content": [{ "type": "input_text", "text": message.text }]
                }));
            }
        } else if is_codec(payload, RESPONSES_RESPONSE_CODEC_ID, CODEC_VERSION) {
            let envelope = parse_response(payload, RESPONSES_RESPONSE_CODEC_ID, "response")?;
            let output = envelope
                .value
                .get("output")
                .and_then(Value::as_array)
                .ok_or_else(|| CodecError::InvalidPayload {
                    message: "Responses payload has no output items".to_owned(),
                })?;
            input.extend(output.iter().cloned());
            let calls = function_calls(envelope.value)?;
            if !pending_eval.is_empty() {
                return Err(CodecError::InvalidPayload {
                    message: "Responses function calls have no results before the next response"
                        .to_owned(),
                });
            }
            pending_eval.extend(calls.into_iter().map(|call| call.call_id));
        } else if is_codec(payload, LAM_EVAL_CODEC_ID, LAM_CODEC_VERSION) {
            let call_id = pending_eval
                .pop_front()
                .ok_or_else(|| CodecError::InvalidPayload {
                    message: "lam/eval has no preceding Responses function call".to_owned(),
                })?;
            input.push(function_output(call_id, eval_output(&payload.value)?));
        } else {
            return Err(unsupported(payload));
        }
    }
    if !pending_eval.is_empty() {
        return Err(CodecError::InvalidPayload {
            message: "Responses function call has no eval result or interruption notice".to_owned(),
        });
    }
    Ok(input)
}

fn compaction_replacement_codec() -> CodecRef {
    CodecRef::new(
        CodecId::new(COMPACTION_REPLACEMENT_CODEC_ID)
            .expect("the Responses compaction codec id is valid"),
        CODEC_VERSION,
    )
}

fn include_encrypted_reasoning(existing: Option<Value>) -> Result<Vec<Value>, CodecError> {
    let mut include = match existing {
        None => Vec::new(),
        Some(Value::Array(include)) => include,
        Some(_) => {
            return Err(CodecError::InvalidPayload {
                message: "Responses include option must be an array".to_owned(),
            });
        }
    };
    let encrypted = Value::String("reasoning.encrypted_content".to_owned());
    if !include.contains(&encrypted) {
        include.push(encrypted);
    }
    Ok(include)
}

fn eval_tool() -> Value {
    json!({
        "type": "function",
        "name": "eval",
        "description": EVAL_TOOL_DESCRIPTION,
        "parameters": eval_parameters(),
        "strict": true
    })
}

fn function_output(call_id: String, output: String) -> Value {
    json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output
    })
}

struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

fn response_display_deltas(response: &Value) -> Vec<ModelDelta> {
    let mut deltas = Vec::new();
    let Some(output) = response.get("output").and_then(Value::as_array) else {
        return deltas;
    };
    for (index, item) in output.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                for field in ["content", "summary"] {
                    let Some(parts) = item.get(field).and_then(Value::as_array) else {
                        continue;
                    };
                    deltas.extend(parts.iter().filter_map(|part| {
                        part.get("text")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .map(|text| ModelDelta::Reasoning(text.to_owned()))
                    }));
                }
            }
            Some("message") => {
                let Some(content) = item.get("content").and_then(Value::as_array) else {
                    continue;
                };
                deltas.extend(content.iter().filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("refusal").and_then(Value::as_str))
                        .filter(|text| !text.is_empty())
                        .map(|text| ModelDelta::Text(text.to_owned()))
                }));
            }
            Some("function_call") => deltas.push(ModelDelta::ToolCall(ToolCallDelta {
                index,
                call_id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                name: item.get("name").and_then(Value::as_str).map(str::to_owned),
                arguments: item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })),
            _ => {}
        }
    }
    deltas
}

fn function_calls(response: &Value) -> Result<Vec<FunctionCall>, CodecError> {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| CodecError::InvalidPayload {
            message: "Responses payload has no output array".to_owned(),
        })?;
    output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .map(|item| {
            let string = |field: &str| {
                item.get(field)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| CodecError::InvalidPayload {
                        message: format!("Responses function call has no {field}"),
                    })
            };
            Ok(FunctionCall {
                call_id: string("call_id")?,
                name: string("name")?,
                arguments: string("arguments")?,
            })
        })
        .collect()
}

fn response_text(response: &Value) -> Result<String, CodecError> {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| CodecError::InvalidPayload {
            message: "Responses payload has no output array".to_owned(),
        })?;
    let mut text = String::new();
    for item in output {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in content {
            match part.get("type").and_then(Value::as_str) {
                Some("output_text") => {
                    if let Some(part) = part.get("text").and_then(Value::as_str) {
                        text.push_str(part);
                    }
                }
                Some("refusal") => {
                    if let Some(part) = part.get("refusal").and_then(Value::as_str) {
                        text.push_str(part);
                    }
                }
                _ => {}
            }
        }
    }
    if text.is_empty() {
        Err(CodecError::InvalidDirective {
            message: "completed Responses payload has neither eval nor text output".to_owned(),
        })
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_streamed_text_is_a_noop_but_whitespace_is_preserved() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = std::sync::Arc::clone(&captured);
        let events = ModelEventSink::new(move |delta| output.lock().unwrap().push(delta));

        emit_delta(&events, &json!({ "delta": "" }), false);
        emit_delta(&events, &json!({ "delta": "" }), true);
        emit_delta(&events, &json!({ "delta": " " }), false);
        emit_delta(&events, &json!({ "delta": "\n" }), true);

        assert_eq!(
            *captured.lock().unwrap(),
            [
                ModelDelta::Text(" ".to_owned()),
                ModelDelta::Reasoning("\n".to_owned()),
            ]
        );
    }

    #[test]
    fn public_codec_constants_are_stable() {
        assert_eq!(
            crate::common::codec(RESPONSES_REQUEST_CODEC_ID).version,
            CODEC_VERSION
        );
        assert_eq!(
            crate::common::display_codec(&crate::common::codec(RESPONSES_RESPONSE_CODEC_ID)),
            "openai/responses@1"
        );
    }
}
