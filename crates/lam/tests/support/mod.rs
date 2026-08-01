use std::collections::VecDeque;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use lam::{
    EncodedPayload, EvalRequest, ModelCodec, ModelDelta, ModelDirective, ModelEventSink,
    ModelProvider, OutputContract,
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
    response: EncodedPayload,
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
            Ok(step.response)
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ScriptedCodec;

impl ModelCodec for ScriptedCodec {
    type Error = ScriptError;

    fn encode_request(
        &self,
        context: &[lam::ProjectedContextEntry],
        output: &OutputContract,
        system_prompt: &str,
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
        let output = match output {
            OutputContract::Text => json!({ "kind": "text" }),
            OutputContract::Structured { schema } => {
                json!({ "kind": "structured", "schema": schema })
            }
        };
        Ok(native(json!({
            "context": context,
            "output": output,
            "systemPrompt": system_prompt,
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
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct ScriptError(String);

pub(crate) fn eval(source: &str) -> ScriptedStep {
    ScriptedStep {
        response: native(json!({ "kind": "eval", "source": source })),
        deltas: Vec::new(),
        gate: None,
    }
}

pub(crate) fn output(value: impl Into<Value>) -> ScriptedStep {
    ScriptedStep {
        response: native(json!({ "kind": "output", "value": value.into() })),
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

fn native(value: Value) -> EncodedPayload {
    EncodedPayload::new(scripted_codec(), value)
}
