//! Generic OpenAI-compatible Chat Completions adapter.

use lam::{
    CodecId, CodecRef, CompactionArtifact, ContextTransition, EncodedPayload, Model, ModelCodec,
    ModelDelta, ModelDescriptor, ModelDirective, ModelEventSink, ModelProvider, ModelRequestConfig,
    ModelResponseMetadata, ModelResponseProjection, OutputContract, ProjectedContextEntry,
    ToolCallDelta,
};
use serde_json::{Map, Value, json};

use crate::common::{
    BuiltConfig, CHAT_REQUEST_CODEC_ID, CHAT_RESPONSE_CODEC_ID, CODEC_VERSION,
    EVAL_TOOL_DESCRIPTION, OutputKind, SharedBuilder, eval_parameters, output_value,
    parse_eval_arguments, parse_request, parse_response, request_payload, response_payload,
};
use crate::context::{
    LAM_CODEC_VERSION, LAM_EVAL_CODEC_ID, LAM_MESSAGES_CODEC_ID, NativeRole, compaction_record,
    compaction_text, eval_output, is_codec, messages, unsupported,
};
use crate::error::{BuildError, CodecError, ProviderError};
use crate::metadata::{ModelPricing, UsageDialect, response_metadata as metadata_view};
use crate::transport::StreamBody;

const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const COMPACTION_REPLACEMENT_CODEC_ID: &str = "openai/chat-compaction";
const CHAT_DESCRIPTOR_CODEC: &str = "openai/chat-completions";

/// Codec identifier for encoded Chat Completions request bodies.
pub const REQUEST_CODEC_ID: &str = CHAT_REQUEST_CODEC_ID;

/// Codec identifier for lossless native Chat Completions response streams.
pub const RESPONSE_CODEC_ID: &str = CHAT_RESPONSE_CODEC_ID;

/// Current Chat Completions request and response representation version.
pub const PAYLOAD_VERSION: u32 = CODEC_VERSION;

/// Entry point for configuring an OpenAI-compatible Chat Completions API.
pub struct ChatCompletions;

impl ChatCompletions {
    /// Starts a Chat Completions adapter builder for `model`.
    #[must_use]
    pub fn builder(model: impl Into<String>) -> ChatCompletionsBuilder {
        ChatCompletionsBuilder {
            shared: SharedBuilder::new(model),
            include_usage: true,
        }
    }
}

/// Configures a generic Chat Completions provider and lossless stream codec.
pub struct ChatCompletionsBuilder {
    shared: SharedBuilder,
    include_usage: bool,
}

impl ChatCompletionsBuilder {
    /// Sets an API key sent as an HTTP bearer token.
    ///
    /// Authentication is optional so local compatible servers remain usable.
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.shared = self.shared.api_key(api_key);
        self
    }

    /// Replaces the API base URL. The default is `https://api.openai.com/v1`.
    ///
    /// Fireworks uses `https://api.fireworks.ai/inference/v1`.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.shared = self.shared.base_url(base_url);
        self
    }

    /// Adds provider-specific top-level request fields.
    ///
    /// This is where compatible extensions such as Fireworks'
    /// `reasoning_effort` and `reasoning_history` belong. Lam overwrites
    /// protocol invariants including `model`, `messages`, `stream`, `tools`,
    /// `n`, and `parallel_tool_calls`.
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

    /// Controls OpenAI's `stream_options.include_usage` request extension.
    ///
    /// It is enabled by default. Disable it for compatible servers which
    /// reject the option; providers such as Fireworks may emit usage without
    /// it.
    #[must_use]
    pub const fn include_usage(mut self, include_usage: bool) -> Self {
        self.include_usage = include_usage;
        self
    }

    /// Uses an embedding-provided HTTP client.
    #[must_use]
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.shared = self.shared.http_client(client);
        self
    }

    /// Builds a model ready for [`lam::Lam::builder`].
    pub fn build(self) -> Result<Model<ChatCompletionsProvider, ChatCompletionsCodec>, BuildError> {
        let (provider, codec) = self.build_parts()?;
        let descriptor =
            ModelDescriptor::new("openai-compatible", codec.model(), CHAT_DESCRIPTOR_CODEC)
                .expect("the Chat Completions descriptor is nonempty");
        Ok(Model::new(provider, codec).with_descriptor(descriptor))
    }

    /// Builds the transport and codec separately for custom composition.
    pub fn build_parts(
        self,
    ) -> Result<(ChatCompletionsProvider, ChatCompletionsCodec), BuildError> {
        let BuiltConfig {
            model,
            extra_body,
            pricing,
            transport,
        } = self.shared.build(CHAT_COMPLETIONS_PATH, false)?;
        Ok((
            ChatCompletionsProvider { transport },
            ChatCompletionsCodec {
                model,
                extra_body,
                pricing,
                include_usage: self.include_usage,
            },
        ))
    }
}

/// HTTP transport for a streaming OpenAI-compatible Chat Completions endpoint.
pub struct ChatCompletionsProvider {
    transport: crate::transport::HttpTransport,
}

impl ModelProvider for ChatCompletionsProvider {
    type Error = ProviderError;

    fn invoke(
        &self,
        request: EncodedPayload,
        events: ModelEventSink,
    ) -> impl Future<Output = Result<EncodedPayload, Self::Error>> + Send {
        let transport = self.transport.clone();
        async move {
            let request = parse_request(request, CHAT_REQUEST_CODEC_ID)?;
            let model = request
                .body
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::InvalidRequest {
                    message: "Chat Completions request has no model".to_owned(),
                })?
                .to_owned();
            let request_body_bytes = serde_json::to_vec(&request.body).map_or(0, |body| body.len());
            let message_count = request
                .body
                .get("messages")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let tool_count = request
                .body
                .get("tools")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let stream = request.body.get("stream").and_then(Value::as_bool);
            let include_usage = request
                .body
                .pointer("/stream_options/include_usage")
                .and_then(Value::as_bool);
            let reasoning_effort = request.body.get("reasoning_effort").and_then(Value::as_str);
            let max_tokens = request.body.get("max_tokens").and_then(Value::as_u64);
            let max_completion_tokens = request
                .body
                .get("max_completion_tokens")
                .and_then(Value::as_u64);
            tracing::debug!(
                target: "lam_openai::chat_completions",
                event = "chat.request_metadata",
                model,
                output_kind = ?request.output_kind,
                body_bytes = request_body_bytes,
                message_count,
                tool_count,
                stream,
                include_usage,
                reasoning_effort,
                max_tokens,
                max_completion_tokens,
                "prepared Chat Completions request"
            );
            let mut chunks = Vec::new();
            let mut event_count = 0_u64;
            let mut saw_done = false;
            let mut saw_finish_reason = false;
            let mut saw_usage = false;
            let mut emitted = DeltaSummary::default();
            let body = transport
                .post_stream("chat_completions", &request.body, |event| {
                    event_count += 1;
                    if event.data == "[DONE]" {
                        saw_done = true;
                        tracing::debug!(
                            target: "lam_openai::chat_completions",
                            event = "chat.done_received",
                            event_index = event_count,
                            chunks = chunks.len(),
                            "received Chat Completions done marker"
                        );
                        return Ok(());
                    }
                    let chunk: Value = serde_json::from_str(&event.data).map_err(|error| {
                        tracing::error!(
                            target: "lam_openai::chat_completions",
                            event = "chat.invalid_event_json",
                            event_index = event_count,
                            data_bytes = event.data.len(),
                            error = %error,
                            "Chat Completions event was not valid JSON"
                        );
                        ProviderError::InvalidEventJson {
                            message: error.to_string(),
                        }
                    })?;
                    if event.event.as_deref() == Some("error")
                        || chunk.get("type").and_then(Value::as_str) == Some("error")
                    {
                        return Err(ProviderError::Api {
                            message: chunk.to_string(),
                        });
                    }
                    if let Some(error) = chunk.get("error") {
                        return Err(ProviderError::Api {
                            message: error.to_string(),
                        });
                    }
                    let summary = summarize_chunk(&chunk);
                    emitted.add(summary);
                    saw_usage |= chunk.get("usage").is_some_and(|usage| !usage.is_null());
                    let finish_reasons = chunk
                        .get("choices")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|choice| choice.get("finish_reason").and_then(Value::as_str))
                        .collect::<Vec<_>>();
                    let has_finish_reason = chunk
                        .get("choices")
                        .and_then(Value::as_array)
                        .is_some_and(|choices| {
                            choices.iter().any(|choice| {
                                choice
                                    .get("finish_reason")
                                    .is_some_and(|reason| !reason.is_null())
                            })
                        });
                    saw_finish_reason |= has_finish_reason;
                    let top_level_type = chunk.get("type").and_then(Value::as_str);
                    let choice_count = chunk
                        .get("choices")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    let usage_present = chunk.get("usage").is_some_and(|usage| !usage.is_null());
                    tracing::trace!(
                        target: "lam_openai::chat_completions",
                        event = "chat.chunk_decoded",
                        event_index = event_count,
                        native_chunk_index = chunks.len() + 1,
                        top_level_type,
                        choice_count,
                        finish_reasons = ?finish_reasons,
                        usage_present,
                        text_bytes = summary.text_bytes,
                        reasoning_bytes = summary.reasoning_bytes,
                        tool_call_count = summary.tool_call_count,
                        tool_argument_bytes = summary.tool_argument_bytes,
                        "decoded Chat Completions chunk"
                    );
                    emit_chunk_deltas(&events, &chunk);
                    chunks.push(chunk);
                    Ok(())
                })
                .await;
            let saw_terminal = saw_done || saw_finish_reason;
            let body = match body {
                Ok(body) => body,
                Err(error)
                    if saw_terminal && !chunks.is_empty() && error.is_response_body_failure() =>
                {
                    tracing::warn!(
                        target: "lam_openai::chat_completions",
                        event = "chat.trailing_body_failure_accepted",
                        error = %error,
                        chunks = chunks.len(),
                        event_count,
                        saw_done,
                        saw_finish_reason,
                        saw_usage,
                        "accepting a semantically complete Chat Completions response after a trailing body failure"
                    );
                    StreamBody::Events
                }
                Err(error) => {
                    tracing::error!(
                        target: "lam_openai::chat_completions",
                        event = "chat.stream_failed",
                        error_kind = provider_error_kind(&error),
                        response_body_failure = error.is_response_body_failure(),
                        chunks = chunks.len(),
                        event_count,
                        saw_done,
                        saw_finish_reason,
                        saw_usage,
                        text_bytes = emitted.text_bytes,
                        reasoning_bytes = emitted.reasoning_bytes,
                        tool_call_count = emitted.tool_call_count,
                        tool_argument_bytes = emitted.tool_argument_bytes,
                        "Chat Completions stream failed"
                    );
                    return Err(error);
                }
            };
            match body {
                StreamBody::Events => {
                    if chunks.is_empty() || !saw_terminal {
                        tracing::error!(
                            target: "lam_openai::chat_completions",
                            event = "chat.missing_terminal",
                            chunks = chunks.len(),
                            event_count,
                            saw_done,
                            saw_finish_reason,
                            saw_usage,
                            "Chat Completions stream ended without a terminal chunk"
                        );
                        return Err(ProviderError::MissingTerminal {
                            expected: "a final Chat Completions chunk",
                        });
                    }
                    tracing::debug!(
                        target: "lam_openai::chat_completions",
                        event = "chat.stream_completed",
                        chunks = chunks.len(),
                        event_count,
                        saw_done,
                        saw_finish_reason,
                        saw_usage,
                        text_bytes = emitted.text_bytes,
                        reasoning_bytes = emitted.reasoning_bytes,
                        tool_call_count = emitted.tool_call_count,
                        tool_argument_bytes = emitted.tool_argument_bytes,
                        "completed Chat Completions stream"
                    );
                    Ok(response_payload(
                        CHAT_RESPONSE_CODEC_ID,
                        request.output_kind,
                        &model,
                        "chunks",
                        Value::Array(chunks),
                    ))
                }
                StreamBody::Json(response) => {
                    if let Some(error) = response.get("error") {
                        return Err(ProviderError::Api {
                            message: error.to_string(),
                        });
                    }
                    Ok(response_payload(
                        CHAT_RESPONSE_CODEC_ID,
                        request.output_kind,
                        &model,
                        "response",
                        response,
                    ))
                }
            }
        }
    }

    fn is_context_overflow(&self, error: &Self::Error) -> bool {
        error.is_context_overflow()
    }
}

#[derive(Clone, Copy, Default)]
struct DeltaSummary {
    text_bytes: u64,
    reasoning_bytes: u64,
    tool_call_count: u64,
    tool_argument_bytes: u64,
}

impl DeltaSummary {
    fn add(&mut self, other: Self) {
        self.text_bytes = self.text_bytes.saturating_add(other.text_bytes);
        self.reasoning_bytes = self.reasoning_bytes.saturating_add(other.reasoning_bytes);
        self.tool_call_count = self.tool_call_count.saturating_add(other.tool_call_count);
        self.tool_argument_bytes = self
            .tool_argument_bytes
            .saturating_add(other.tool_argument_bytes);
    }
}

fn summarize_chunk(chunk: &Value) -> DeltaSummary {
    let mut summary = DeltaSummary::default();
    let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
        return summary;
    };
    for delta in choices
        .iter()
        .filter_map(|choice| choice.get("delta").and_then(Value::as_object))
    {
        summary.text_bytes = summary.text_bytes.saturating_add(
            delta
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, |text| text.len() as u64),
        );
        for field in ["reasoning_content", "reasoning", "thinking"] {
            summary.reasoning_bytes = summary.reasoning_bytes.saturating_add(
                delta
                    .get(field)
                    .and_then(Value::as_str)
                    .map_or(0, |text| text.len() as u64),
            );
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            summary.tool_call_count = summary.tool_call_count.saturating_add(calls.len() as u64);
            for call in calls {
                summary.tool_argument_bytes = summary.tool_argument_bytes.saturating_add(
                    call.pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .map_or(0, |arguments| arguments.len() as u64),
                );
            }
        }
    }
    summary
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

fn emit_chunk_deltas(events: &ModelEventSink, chunk: &Value) {
    for delta in chunk_deltas(chunk) {
        events.emit(delta);
    }
}

fn chunk_deltas(chunk: &Value) -> Vec<ModelDelta> {
    let mut output = Vec::new();
    let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
        return output;
    };
    for delta in choices
        .iter()
        .filter_map(|choice| choice.get("delta").and_then(Value::as_object))
    {
        for field in ["reasoning_content", "reasoning", "thinking"] {
            if let Some(text) = delta
                .get(field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                output.push(ModelDelta::Reasoning(text.to_owned()));
            }
        }
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            output.push(ModelDelta::Text(text.to_owned()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .unwrap_or(position);
                let function = call.get("function");
                output.push(ModelDelta::ToolCall(ToolCallDelta {
                    index,
                    call_id: call.get("id").and_then(Value::as_str).map(str::to_owned),
                    name: function
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    arguments: function
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }));
            }
        }
    }
    output
}

/// Pure Chat Completions request/replay codec.
#[derive(Clone)]
pub struct ChatCompletionsCodec {
    model: String,
    extra_body: Map<String, Value>,
    pricing: Option<ModelPricing>,
    include_usage: bool,
}

impl ChatCompletionsCodec {
    /// Returns the configured request model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl ModelCodec for ChatCompletionsCodec {
    type Error = CodecError;

    fn encode_request(
        &self,
        context: &[ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
    ) -> Result<EncodedPayload, Self::Error> {
        let mut messages = encode_context(context)?;
        let mut body = self.extra_body.clone();
        body.insert("model".to_owned(), Value::String(self.model.clone()));
        body.insert("stream".to_owned(), Value::Bool(true));
        if self.include_usage {
            let mut stream_options = match body.remove("stream_options") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(options)) => options,
                Some(_) => {
                    return Err(CodecError::InvalidPayload {
                        message: "Chat Completions stream_options must be an object".to_owned(),
                    });
                }
            };
            stream_options.insert("include_usage".to_owned(), Value::Bool(true));
            body.insert("stream_options".to_owned(), Value::Object(stream_options));
        }
        body.insert("n".to_owned(), Value::Number(1.into()));
        body.remove("tools");
        body.remove("tool_choice");
        body.remove("parallel_tool_calls");
        if config.enable_eval {
            body.insert("parallel_tool_calls".to_owned(), Value::Bool(false));
            body.insert("tools".to_owned(), Value::Array(vec![eval_tool()]));
            body.insert("tool_choice".to_owned(), Value::String("auto".to_owned()));
        }
        if let Some(max_output_tokens) = config.max_output_tokens {
            body.insert(
                "max_tokens".to_owned(),
                Value::Number(max_output_tokens.into()),
            );
        }
        let mut system_sections = Vec::new();
        if !config.system_prompt.is_empty() {
            system_sections.push(config.system_prompt.to_owned());
        }
        if let OutputContract::Structured { schema } = config.output {
            system_sections.push(format!(
                "Return only JSON matching this schema: <lam_output_schema>{schema}</lam_output_schema>"
            ));
            body.insert(
                "response_format".to_owned(),
                json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "lam_output",
                        "schema": schema,
                        "strict": true
                    }
                }),
            );
        }
        if !system_sections.is_empty() {
            messages.insert(
                0,
                json!({
                    "role": "system",
                    "content": system_sections.join("\n\n")
                }),
            );
        }
        body.insert("messages".to_owned(), Value::Array(messages));
        Ok(request_payload(
            CHAT_REQUEST_CODEC_ID,
            OutputKind::from_contract(config.output),
            Value::Object(body),
        ))
    }

    fn project_response(
        &self,
        response: &EncodedPayload,
    ) -> Result<ModelResponseProjection, Self::Error> {
        let (output_kind, message, finish_reason) = response_message(response)?;
        if let Some(reason) = finish_reason
            && !matches!(reason.as_str(), "stop" | "tool_calls" | "function_call")
        {
            return Err(CodecError::InvalidDirective {
                message: format!("Chat Completions stopped with finish_reason `{reason}`"),
            });
        }
        let calls = tool_calls(&message)?;
        if calls.len() > 1 {
            return Err(CodecError::InvalidDirective {
                message: "the model requested more than one eval".to_owned(),
            });
        }
        let display = response_display_deltas(response, &message, &calls);
        let directive = if let Some(call) = calls.first() {
            if call.name != "eval" {
                return Err(CodecError::InvalidDirective {
                    message: format!("the model requested unsupported function `{}`", call.name),
                });
            }
            parse_eval_arguments(&call.arguments).map(ModelDirective::Eval)?
        } else {
            output_value(output_kind, message_text(&message)?).map(ModelDirective::Output)?
        };
        Ok(ModelResponseProjection { display, directive })
    }

    fn response_metadata(&self, response: &EncodedPayload) -> ModelResponseMetadata {
        let (model, usage) =
            if let Some(chunks) = response.value.get("chunks").and_then(Value::as_array) {
                let model = chunks
                    .iter()
                    .rev()
                    .find_map(|chunk| chunk.get("model").and_then(Value::as_str))
                    .unwrap_or(&self.model)
                    .to_owned();
                let usage = chunks.iter().rev().find_map(|chunk| chunk.get("usage"));
                (model, usage)
            } else {
                let native = response.value.get("response");
                let model = native
                    .and_then(|response| response.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or(&self.model)
                    .to_owned();
                let usage = native.and_then(|response| response.get("usage"));
                (model, usage)
            };
        metadata_view(model, usage, UsageDialect::ChatCompletions, self.pricing)
    }

    fn materialize_compaction(
        &self,
        artifact: &CompactionArtifact,
    ) -> Result<Option<EncodedPayload>, Self::Error> {
        Ok(Some(EncodedPayload::new(
            compaction_replacement_codec(),
            json!([{
                "role": "user",
                "content": compaction_text(artifact),
            }]),
        )))
    }

    fn accepts_compaction_replacement(&self, replacement: &EncodedPayload) -> bool {
        replacement.codec == compaction_replacement_codec()
    }
}

fn encode_context(context: &[ProjectedContextEntry]) -> Result<Vec<Value>, CodecError> {
    let mut native = Vec::new();
    let mut pending_eval = None;
    for projected in context {
        let payload = &projected.entry.payload;
        if matches!(
            projected.entry.transition,
            ContextTransition::Compaction { .. }
        ) {
            if pending_eval.is_some() {
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
                        message: "Chat Completions compaction replacement is not a message array"
                            .to_owned(),
                    })?;
            native.extend(replacement.iter().cloned());
        } else if is_codec(payload, LAM_MESSAGES_CODEC_ID, LAM_CODEC_VERSION) {
            for message in messages(&payload.value)? {
                if message.closes_interrupted_eval
                    && let Some(tool_call_id) = pending_eval.take()
                {
                    native.push(tool_output(tool_call_id, message.text.clone()));
                }
                let role = match message.role {
                    NativeRole::User => "user",
                    NativeRole::System => "system",
                };
                native.push(json!({ "role": role, "content": message.text }));
            }
        } else if is_codec(payload, CHAT_RESPONSE_CODEC_ID, CODEC_VERSION) {
            let (_, mut message, _) = response_message(payload)?;
            let calls = tool_calls(&message)?;
            if calls.len() > 1 {
                return Err(CodecError::InvalidPayload {
                    message: "Chat Completions payload contains more than one tool call".to_owned(),
                });
            }
            if let Some(call) = calls.first() {
                pending_eval = Some(call.id.clone());
            }
            omit_null_request_fields(&mut message);
            native.push(message);
        } else if is_codec(payload, LAM_EVAL_CODEC_ID, LAM_CODEC_VERSION) {
            let tool_call_id = pending_eval
                .take()
                .ok_or_else(|| CodecError::InvalidPayload {
                    message: "lam/eval has no preceding Chat Completions tool call".to_owned(),
                })?;
            native.push(tool_output(tool_call_id, eval_output(&payload.value)?));
        } else {
            return Err(unsupported(payload));
        }
    }
    if pending_eval.is_some() {
        return Err(CodecError::InvalidPayload {
            message: "Chat Completions tool call has no eval result or interruption notice"
                .to_owned(),
        });
    }
    Ok(native)
}

fn omit_null_request_fields(message: &mut Value) {
    let Some(message) = message.as_object_mut() else {
        return;
    };
    for field in ["tool_calls", "reasoning_content", "reasoning", "thinking"] {
        if message.get(field).is_some_and(Value::is_null) {
            message.remove(field);
        }
    }
}

fn compaction_replacement_codec() -> CodecRef {
    CodecRef::new(
        CodecId::new(COMPACTION_REPLACEMENT_CODEC_ID)
            .expect("the Chat Completions compaction codec id is valid"),
        CODEC_VERSION,
    )
}

fn eval_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "eval",
            "description": EVAL_TOOL_DESCRIPTION,
            "parameters": eval_parameters(),
            "strict": true
        }
    })
}

fn tool_output(tool_call_id: String, content: String) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": content
    })
}

struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn tool_calls(message: &Value) -> Result<Vec<ToolCall>, CodecError> {
    let Some(calls) = message.get("tool_calls") else {
        return Ok(Vec::new());
    };
    if calls.is_null() {
        return Ok(Vec::new());
    }
    let calls = calls.as_array().ok_or_else(|| CodecError::InvalidPayload {
        message: "assistant tool_calls is not an array".to_owned(),
    })?;
    calls
        .iter()
        .map(|call| {
            let id = call.get("id").and_then(Value::as_str).ok_or_else(|| {
                CodecError::InvalidPayload {
                    message: "assistant tool call has no id".to_owned(),
                }
            })?;
            let function = call
                .get("function")
                .ok_or_else(|| CodecError::InvalidPayload {
                    message: "assistant tool call has no function".to_owned(),
                })?;
            let string = |field: &str| {
                function
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| CodecError::InvalidPayload {
                        message: format!("assistant tool function has no {field}"),
                    })
            };
            Ok(ToolCall {
                id: id.to_owned(),
                name: string("name")?,
                arguments: string("arguments")?,
            })
        })
        .collect()
}

fn response_message(
    response: &EncodedPayload,
) -> Result<(OutputKind, Value, Option<String>), CodecError> {
    if response.value.get("chunks").is_some() {
        let envelope = parse_response(response, CHAT_RESPONSE_CODEC_ID, "chunks")?;
        let chunks = envelope
            .value
            .as_array()
            .ok_or_else(|| CodecError::InvalidPayload {
                message: "Chat Completions chunks is not an array".to_owned(),
            })?;
        Ok((
            envelope.output_kind,
            assemble_message(chunks)?,
            streamed_finish_reason(chunks),
        ))
    } else {
        let envelope = parse_response(response, CHAT_RESPONSE_CODEC_ID, "response")?;
        let choice = envelope
            .value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .ok_or_else(|| CodecError::InvalidPayload {
                message: "Chat Completions response has no first choice".to_owned(),
            })?;
        let message = choice
            .get("message")
            .cloned()
            .ok_or_else(|| CodecError::InvalidPayload {
                message: "Chat Completions response has no first message".to_owned(),
            })?;
        let finish_reason = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok((envelope.output_kind, message, finish_reason))
    }
}

fn response_display_deltas(
    response: &EncodedPayload,
    message: &Value,
    calls: &[ToolCall],
) -> Vec<ModelDelta> {
    if let Some(chunks) = response.value.get("chunks").and_then(Value::as_array) {
        return chunks.iter().flat_map(chunk_deltas).collect();
    }

    let mut deltas = Vec::new();
    for field in ["reasoning_content", "reasoning", "thinking"] {
        if let Some(reasoning) = message
            .get(field)
            .and_then(Value::as_str)
            .filter(|reasoning| !reasoning.is_empty())
        {
            deltas.push(ModelDelta::Reasoning(reasoning.to_owned()));
        }
    }
    if let Ok(text) = message_text(message)
        && !text.is_empty()
    {
        deltas.push(ModelDelta::Text(text));
    }
    deltas.extend(calls.iter().enumerate().map(|(index, call)| {
        ModelDelta::ToolCall(ToolCallDelta {
            index,
            call_id: Some(call.id.clone()),
            name: Some(call.name.clone()),
            arguments: call.arguments.clone(),
        })
    }));
    deltas
}

fn streamed_finish_reason(chunks: &[Value]) -> Option<String> {
    chunks
        .iter()
        .flat_map(|chunk| chunk.get("choices").and_then(Value::as_array).into_iter())
        .flatten()
        .filter(|choice| choice.get("index").and_then(Value::as_u64).unwrap_or(0) == 0)
        .filter_map(|choice| choice.get("finish_reason").and_then(Value::as_str))
        .next_back()
        .map(str::to_owned)
}

fn assemble_message(chunks: &[Value]) -> Result<Value, CodecError> {
    let mut message = Value::Object(Map::new());
    let mut found = false;
    for chunk in chunks {
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            continue;
        };
        for choice in choices {
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            if let Some(delta) = choice.get("delta") {
                merge_message_delta(&mut message, delta)?;
                found = true;
            }
        }
    }
    if !found {
        return Err(CodecError::InvalidPayload {
            message: "Chat Completions stream has no message delta".to_owned(),
        });
    }
    let object = message
        .as_object_mut()
        .expect("message is initialized as an object");
    object
        .entry("role".to_owned())
        .or_insert_with(|| Value::String("assistant".to_owned()));
    Ok(message)
}

fn merge_message_delta(target: &mut Value, delta: &Value) -> Result<(), CodecError> {
    let delta = delta
        .as_object()
        .ok_or_else(|| CodecError::InvalidPayload {
            message: "Chat Completions message delta is not an object".to_owned(),
        })?;
    let target = target
        .as_object_mut()
        .expect("message accumulator is an object");
    match streamed_tool_calls(delta)? {
        StreamedToolCalls::Missing => {}
        StreamedToolCalls::Null => {
            target.entry("tool_calls".to_owned()).or_insert(Value::Null);
        }
        StreamedToolCalls::Array(calls) => merge_tool_call_deltas(target, calls)?,
    }
    for (key, incoming) in delta {
        if key != "tool_calls" {
            merge_value(
                target.entry(key.clone()).or_insert(Value::Null),
                incoming,
                key,
            );
        }
    }
    Ok(())
}

enum StreamedToolCalls<'a> {
    Missing,
    Null,
    Array(&'a [Value]),
}

fn streamed_tool_calls(delta: &Map<String, Value>) -> Result<StreamedToolCalls<'_>, CodecError> {
    match delta.get("tool_calls") {
        None => Ok(StreamedToolCalls::Missing),
        Some(Value::Null) => Ok(StreamedToolCalls::Null),
        Some(Value::Array(calls)) => Ok(StreamedToolCalls::Array(calls)),
        Some(_) => Err(CodecError::InvalidPayload {
            message: "streamed tool_calls is not an array or null".to_owned(),
        }),
    }
}

fn merge_tool_call_deltas(
    message: &mut Map<String, Value>,
    incoming: &[Value],
) -> Result<(), CodecError> {
    let target = message
        .entry("tool_calls".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    if target.is_null() {
        *target = Value::Array(Vec::new());
    }
    let target = target
        .as_array_mut()
        .ok_or_else(|| CodecError::InvalidPayload {
            message: "accumulated tool_calls is not an array".to_owned(),
        })?;
    for (position, call) in incoming.iter().enumerate() {
        let call = call.as_object().ok_or_else(|| CodecError::InvalidPayload {
            message: "streamed tool call is not an object".to_owned(),
        })?;
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .unwrap_or(position);
        while target.len() <= index {
            target.push(Value::Object(Map::new()));
        }
        let mut call = call.clone();
        call.remove("index");
        merge_value(&mut target[index], &Value::Object(call), "tool_calls");
    }
    Ok(())
}

fn merge_value(target: &mut Value, incoming: &Value, key: &str) {
    if incoming.is_null() {
        return;
    }
    match (target, incoming) {
        (target @ Value::Null, incoming) => *target = incoming.clone(),
        (Value::Object(target), Value::Object(incoming)) => {
            for (key, value) in incoming {
                merge_value(target.entry(key.clone()).or_insert(Value::Null), value, key);
            }
        }
        (Value::Array(target), Value::Array(incoming)) => merge_array(target, incoming, key),
        (Value::String(target), Value::String(incoming)) => {
            if target == incoming {
                return;
            }
            if matches!(key, "role" | "type" | "id" | "name") {
                target.clone_from(incoming);
            } else {
                target.push_str(incoming);
            }
        }
        (target, incoming) => *target = incoming.clone(),
    }
}

fn merge_array(target: &mut Vec<Value>, incoming: &[Value], key: &str) {
    for (position, value) in incoming.iter().enumerate() {
        let indexed = value
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok());
        let identified = value.get("id").and_then(Value::as_str).and_then(|id| {
            target
                .iter()
                .position(|existing| existing.get("id").and_then(Value::as_str) == Some(id))
        });
        if let Some(index) = indexed.or(identified) {
            while target.len() <= index {
                target.push(Value::Null);
            }
            merge_value(&mut target[index], value, key);
        } else if position < target.len()
            && target[position].is_object()
            && value.is_object()
            && target[position].get("type") == value.get("type")
        {
            merge_value(&mut target[position], value, key);
        } else if !target.contains(value) {
            target.push(value.clone());
        }
    }
}

fn message_text(message: &Value) -> Result<String, CodecError> {
    if let Some(content) = message.get("content").and_then(Value::as_str)
        && !content.is_empty()
    {
        return Ok(content.to_owned());
    }
    if let Some(parts) = message.get("content").and_then(Value::as_array) {
        let mut text = String::new();
        for part in parts {
            if let Some(part) = part
                .get("text")
                .and_then(Value::as_str)
                .or_else(|| part.get("content").and_then(Value::as_str))
            {
                text.push_str(part);
            }
        }
        if !text.is_empty() {
            return Ok(text);
        }
    }
    if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
        return Ok(refusal.to_owned());
    }
    Err(CodecError::InvalidDirective {
        message: "completed Chat Completions message has neither eval nor text output".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_streamed_text_is_a_noop_but_whitespace_is_preserved() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = std::sync::Arc::clone(&captured);
        let events = ModelEventSink::new(move |delta| output.lock().unwrap().push(delta));

        emit_chunk_deltas(
            &events,
            &json!({
                "choices": [{
                    "delta": {
                        "content": "",
                        "reasoning_content": "",
                        "role": "assistant"
                    }
                }]
            }),
        );
        emit_chunk_deltas(
            &events,
            &json!({
                "choices": [{
                    "delta": {
                        "content": " ",
                        "reasoning_content": "\n"
                    }
                }]
            }),
        );

        assert_eq!(
            *captured.lock().unwrap(),
            [
                ModelDelta::Reasoning("\n".to_owned()),
                ModelDelta::Text(" ".to_owned()),
            ]
        );
    }

    #[test]
    fn reasoning_precedes_text_from_the_same_chunk() {
        assert_eq!(
            chunk_deltas(&json!({
                "choices": [{
                    "delta": {
                        "content": "That's a great milestone",
                        "reasoning_content": "."
                    }
                }]
            })),
            [
                ModelDelta::Reasoning(".".to_owned()),
                ModelDelta::Text("That's a great milestone".to_owned()),
            ]
        );
    }

    #[test]
    fn assembles_reasoning_and_tool_arguments_without_losing_extensions() {
        let chunks = vec![
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "reasoning_content": "think ",
                        "reasoning_signature": "enc-",
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "eval", "arguments": "{\"source\":\"" }
                        }]
                    }
                }]
            }),
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "reasoning_content": "again",
                        "reasoning_signature": "opaque",
                        "tool_calls": [{
                            "index": 0,
                            "function": { "arguments": "1+1\"}" }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }),
        ];
        let message = assemble_message(&chunks).expect("valid chunks");
        assert_eq!(message["reasoning_content"], "think again");
        assert_eq!(message["reasoning_signature"], "enc-opaque");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "{\"source\":\"1+1\"}"
        );
        assert!(message["tool_calls"][0].get("index").is_none());
    }

    #[test]
    fn distinguishes_missing_null_and_array_tool_call_deltas() {
        let missing = Map::new();
        assert!(matches!(
            streamed_tool_calls(&missing).expect("missing is valid"),
            StreamedToolCalls::Missing
        ));

        let null = json!({ "tool_calls": null });
        assert!(matches!(
            streamed_tool_calls(null.as_object().expect("object")).expect("null is valid"),
            StreamedToolCalls::Null
        ));

        let array = json!({ "tool_calls": [] });
        assert!(matches!(
            streamed_tool_calls(array.as_object().expect("object")).expect("array is valid"),
            StreamedToolCalls::Array([])
        ));
    }

    #[test]
    fn assembles_text_when_streamed_tool_calls_is_null() {
        let chunks = vec![
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": "complete response",
                        "reasoning_content": null,
                        "tool_calls": null
                    }
                }]
            }),
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
        ];

        let message = assemble_message(&chunks).expect("null tool_calls is valid");
        assert_eq!(message["content"], "complete response");
        assert!(message["reasoning_content"].is_null());
        assert!(message["tool_calls"].is_null());
    }

    #[test]
    fn tool_call_array_supersedes_an_explicit_null_without_losing_the_distinction() {
        let chunks = vec![
            json!({
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": null }
                }]
            }),
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "eval", "arguments": "{}" }
                        }]
                    }
                }]
            }),
            json!({
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": null }
                }]
            }),
        ];

        let message = assemble_message(&chunks).expect("array supersedes null");
        assert_eq!(message["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn rejects_non_array_non_null_streamed_tool_calls() {
        let chunks = vec![json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "content": "response",
                    "tool_calls": {}
                }
            }]
        })];

        let error = assemble_message(&chunks).expect_err("object tool_calls must be rejected");
        assert!(error.to_string().contains("not an array or null"));
    }
}
