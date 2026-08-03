use std::collections::VecDeque;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use lam::{
    CompactionArtifact, EncodedPayload, EvalRequest, ModelCodec, ModelDelta, ModelDirective,
    ModelEventSink, ModelProvider, ModelRequestConfig, ModelResponseMetadata, OutputContract,
    TokenUsage,
};
use serde_json::{Value, json};

#[derive(Clone)]
pub(crate) struct ScriptedProvider {
    shared: Arc<ScriptedState>,
}

struct ScriptedState {
    steps: Mutex<VecDeque<ScriptedStep>>,
    requests: Mutex<Vec<EncodedPayload>>,
}

pub(crate) struct ScriptedStep {
    response: Result<EncodedPayload, ScriptError>,
    pub(crate) deltas: Vec<ModelDelta>,
    pub(crate) gate: Option<Arc<Barrier>>,
}

impl ScriptedProvider {
    pub(crate) fn new(steps: impl IntoIterator<Item = ScriptedStep>) -> Self {
        Self {
            shared: Arc::new(ScriptedState {
                steps: Mutex::new(steps.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }),
        }
    }

    pub(crate) fn requests(&self) -> Vec<EncodedPayload> {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ModelProvider for ScriptedProvider {
    type Error = ScriptError;

    fn invoke(
        &self,
        request: EncodedPayload,
        events: ModelEventSink,
    ) -> impl Future<Output = Result<EncodedPayload, Self::Error>> + Send {
        let shared = Arc::clone(&self.shared);
        async move {
            shared
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            let step = shared
                .steps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or_else(|| ScriptError("scripted provider exhausted".to_owned()))?;
            for delta in step.deltas {
                events.emit(delta);
            }
            if let Some(gate) = step.gate {
                gate.wait();
            }
            step.response
        }
    }

    fn is_context_overflow(&self, error: &Self::Error) -> bool {
        error.0 == "context overflow"
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ScriptedCodec;

impl ModelCodec for ScriptedCodec {
    type Error = ScriptError;

    fn encode_request(
        &self,
        context: &[lam::ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
    ) -> Result<EncodedPayload, Self::Error> {
        let context = context
            .iter()
            .map(|entry| {
                json!({
                    "transition": &entry.entry.transition,
                    "payload": &entry.entry.payload,
                })
            })
            .collect::<Vec<_>>();
        let output = match config.output {
            OutputContract::Text => json!({ "kind": "text" }),
            OutputContract::Structured { schema } => {
                json!({ "kind": "structured", "schema": schema })
            }
        };
        Ok(native(json!({
            "context": context,
            "output": output,
            "systemPrompt": config.system_prompt,
            "enableEval": config.enable_eval,
            "maxOutputTokens": config.max_output_tokens,
        })))
    }

    fn interpret_response(&self, response: &EncodedPayload) -> Result<ModelDirective, Self::Error> {
        if response.codec != scripted_codec() {
            return Err(ScriptError("unexpected response codec".to_owned()));
        }
        match response.value.get("kind").and_then(Value::as_str) {
            Some("eval") => {
                let source = response
                    .value
                    .get("source")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ScriptError("eval response has no source".to_owned()))?;
                let timeout = response
                    .value
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .map(Duration::from_millis);
                Ok(ModelDirective::Eval(EvalRequest {
                    intent: response
                        .value
                        .get("intent")
                        .and_then(Value::as_str)
                        .unwrap_or("Evaluate TypeScript")
                        .to_owned(),
                    source: source.to_owned(),
                    timeout,
                }))
            }
            Some("output") => Ok(ModelDirective::Output(
                response
                    .value
                    .get("value")
                    .cloned()
                    .ok_or_else(|| ScriptError("output response has no value".to_owned()))?,
            )),
            Some(kind) => Err(ScriptError(format!("unknown directive `{kind}`"))),
            None => Err(ScriptError("response has no directive".to_owned())),
        }
    }

    fn response_metadata(&self, response: &EncodedPayload) -> ModelResponseMetadata {
        let Some(total_tokens) = response.value.get("usageTotal").and_then(Value::as_u64) else {
            return ModelResponseMetadata::default();
        };
        ModelResponseMetadata {
            model: Some("scripted".to_owned()),
            usage: Some(TokenUsage {
                input_tokens: total_tokens,
                cached_input_tokens: None,
                output_tokens: 0,
                reasoning_tokens: None,
                total_tokens,
                native: json!({ "totalTokens": total_tokens }),
            }),
            cost: None,
        }
    }

    fn materialize_compaction(
        &self,
        artifact: &CompactionArtifact,
    ) -> Result<Option<EncodedPayload>, Self::Error> {
        Ok(Some(EncodedPayload::new(
            scripted_compaction_codec(),
            json!({ "summary": artifact.summary, "excerpts": artifact.excerpts }),
        )))
    }

    fn accepts_compaction_replacement(&self, replacement: &EncodedPayload) -> bool {
        replacement.codec == scripted_compaction_codec()
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct RejectingCompactionCodec;

impl ModelCodec for RejectingCompactionCodec {
    type Error = ScriptError;

    fn encode_request(
        &self,
        context: &[lam::ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
    ) -> Result<EncodedPayload, Self::Error> {
        ScriptedCodec.encode_request(context, config)
    }

    fn interpret_response(&self, response: &EncodedPayload) -> Result<ModelDirective, Self::Error> {
        ScriptedCodec.interpret_response(response)
    }

    fn response_metadata(&self, response: &EncodedPayload) -> ModelResponseMetadata {
        ScriptedCodec.response_metadata(response)
    }

    fn materialize_compaction(
        &self,
        artifact: &CompactionArtifact,
    ) -> Result<Option<EncodedPayload>, Self::Error> {
        ScriptedCodec.materialize_compaction(artifact)
    }

    fn accepts_compaction_replacement(&self, _replacement: &EncodedPayload) -> bool {
        false
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct ScriptError(String);

pub(crate) fn eval(source: &str) -> ScriptedStep {
    ScriptedStep {
        response: Ok(native(json!({ "kind": "eval", "source": source }))),
        deltas: Vec::new(),
        gate: None,
    }
}

pub(crate) fn output(value: impl Into<Value>) -> ScriptedStep {
    ScriptedStep {
        response: Ok(native(json!({ "kind": "output", "value": value.into() }))),
        deltas: Vec::new(),
        gate: None,
    }
}

#[allow(dead_code)]
pub(crate) fn output_with_usage(value: impl Into<Value>, total_tokens: u64) -> ScriptedStep {
    ScriptedStep {
        response: Ok(native(json!({
            "kind": "output",
            "value": value.into(),
            "usageTotal": total_tokens,
        }))),
        deltas: Vec::new(),
        gate: None,
    }
}

#[allow(dead_code)]
pub(crate) fn overflow() -> ScriptedStep {
    ScriptedStep {
        response: Err(ScriptError("context overflow".to_owned())),
        deltas: Vec::new(),
        gate: None,
    }
}

fn scripted_codec() -> lam::CodecRef {
    lam::CodecRef::new(
        lam::CodecId::new("test/scripted").expect("fixture codec is valid"),
        1,
    )
}

fn scripted_compaction_codec() -> lam::CodecRef {
    lam::CodecRef::new(
        lam::CodecId::new("test/scripted-compaction").expect("fixture codec is valid"),
        1,
    )
}

fn native(value: Value) -> EncodedPayload {
    EncodedPayload::new(scripted_codec(), value)
}
