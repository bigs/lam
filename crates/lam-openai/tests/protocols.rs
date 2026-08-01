//! Offline protocol, lossless-replay, and loopback transport tests.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use lam::{
    CodecId, CodecRef, ContextEntry, ContextSequence, ContextTransition, EncodedPayload,
    ModelCodec, ModelCostSource, ModelDelta, ModelDirective, ModelEventSink, ModelProvider,
    OutputContract, ProjectedContextEntry, Revision, RunEvent, RunId, RunProgress, Timestamp,
};
use lam_openai::chat_completions::{
    ChatCompletions, REQUEST_CODEC_ID as CHAT_REQUEST_CODEC_ID,
    RESPONSE_CODEC_ID as CHAT_RESPONSE_CODEC_ID,
};
use lam_openai::responses::{
    REQUEST_CODEC_ID as RESPONSES_REQUEST_CODEC_ID,
    RESPONSE_CODEC_ID as RESPONSES_RESPONSE_CODEC_ID, Responses,
};
use lam_openai::{BuildError, ModelPricing};
use serde_json::{Value, json};

#[test]
fn responses_request_is_stateless_and_replays_encrypted_reasoning_unchanged() {
    let (_, codec) = Responses::builder("gpt-test")
        .api_key("test-key")
        .extra_body(json!({
            "store": true,
            "parallel_tool_calls": true,
            "include": ["message.output_text.logprobs"],
            "reasoning": { "effort": "high" }
        }))
        .build_parts()
        .expect("valid adapter");
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs_1",
        "summary": [],
        "encrypted_content": "opaque-ciphertext"
    });
    let function_call = json!({
        "type": "function_call",
        "id": "fc_1",
        "call_id": "call_1",
        "name": "eval",
        "arguments": "{\"source\":\"1 + 1\",\"timeoutMs\":250}",
        "status": "completed"
    });
    let native_response = json!({
        "id": "resp_1",
        "object": "response",
        "model": "gpt-test",
        "status": "completed",
        "output": [reasoning.clone(), function_call.clone()]
    });
    let response = response_payload(
        RESPONSES_RESPONSE_CODEC_ID,
        "text",
        "response",
        native_response,
    );
    assert_eq!(
        codec.interpret_response(&response).expect("valid eval"),
        ModelDirective::Eval(lam::EvalRequest {
            source: "1 + 1".to_owned(),
            timeout: Some(Duration::from_millis(250)),
        })
    );

    let context = vec![
        projected(1, model_transition(), response),
        projected(
            2,
            eval_transition(),
            payload("lam/eval", json!({ "status": "success", "output": 2 })),
        ),
    ];
    let request = codec
        .encode_request(&context, &OutputContract::Text)
        .expect("context can be replayed");
    assert_eq!(request.codec.id.as_str(), RESPONSES_REQUEST_CODEC_ID);
    let body = &request.value["body"];
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert!(
        body["include"]
            .as_array()
            .unwrap()
            .contains(&json!("reasoning.encrypted_content"))
    );
    assert_eq!(body["input"][0], reasoning);
    assert_eq!(body["input"][1], function_call);
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["input"][2]["call_id"], "call_1");
}

#[test]
fn responses_structured_output_uses_json_schema_and_decodes_json() {
    let (_, codec) = Responses::builder("gpt-test")
        .api_key("test-key")
        .build_parts()
        .expect("valid adapter");
    let schema = json!({
        "type": "object",
        "properties": { "answer": { "type": "integer" } },
        "required": ["answer"],
        "additionalProperties": false
    });
    let request = codec
        .encode_request(
            &[user_message("question")],
            &OutputContract::Structured {
                schema: schema.clone(),
            },
        )
        .expect("valid request");
    assert_eq!(request.value["body"]["text"]["format"]["schema"], schema);
    let response = response_payload(
        RESPONSES_RESPONSE_CODEC_ID,
        "structured",
        "response",
        json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "{\"answer\":42}" }]
            }]
        }),
    );
    assert_eq!(
        codec.interpret_response(&response).expect("valid output"),
        ModelDirective::Output(json!({ "answer": 42 }))
    );
}

#[test]
fn chat_replays_reasoning_extensions_and_tool_calls_from_native_chunks() {
    let (_, codec) = ChatCompletions::builder("accounts/test/model")
        .extra_body(json!({
            "reasoning_effort": "high",
            "reasoning_history": "preserved",
            "stream": false,
            "parallel_tool_calls": true
        }))
        .build_parts()
        .expect("valid adapter");
    let chunks = json!([
        {
            "id": "chat_1",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "reasoning_content": "inspect ",
                    "reasoning_signature": "enc-",
                    "reasoning_details": [{
                        "index": 0,
                        "type": "encrypted",
                        "data": "part-"
                    }],
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "eval", "arguments": "{\"source\":\"" }
                    }]
                },
                "finish_reason": null
            }]
        },
        {
            "id": "chat_1",
            "choices": [{
                "index": 0,
                "delta": {
                    "reasoning_content": "state",
                    "reasoning_signature": "opaque",
                    "reasoning_details": [{
                        "index": 0,
                        "type": "encrypted",
                        "data": "two"
                    }],
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "2 + 2\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }
    ]);
    let response = response_payload(CHAT_RESPONSE_CODEC_ID, "text", "chunks", chunks.clone());
    assert_eq!(response.value["chunks"], chunks);
    assert_eq!(
        codec.interpret_response(&response).expect("valid eval"),
        ModelDirective::Eval(lam::EvalRequest {
            source: "2 + 2".to_owned(),
            timeout: None,
        })
    );

    let request = codec
        .encode_request(
            &[
                projected(1, model_transition(), response),
                projected(
                    2,
                    eval_transition(),
                    payload("lam/eval", json!({ "status": "success", "output": 4 })),
                ),
            ],
            &OutputContract::Text,
        )
        .expect("context can be replayed");
    assert_eq!(request.codec.id.as_str(), CHAT_REQUEST_CODEC_ID);
    let body = &request.value["body"];
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["reasoning_history"], "preserved");
    let assistant = &body["messages"][0];
    assert_eq!(assistant["reasoning_content"], "inspect state");
    assert_eq!(assistant["reasoning_signature"], "enc-opaque");
    assert_eq!(assistant["reasoning_details"][0]["data"], "part-two");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        "{\"source\":\"2 + 2\"}"
    );
    assert_eq!(body["messages"][1]["role"], "tool");
    assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
}

#[test]
fn chat_structured_output_uses_json_schema_and_decodes_json() {
    let (_, codec) = ChatCompletions::builder("test-model")
        .include_usage(false)
        .build_parts()
        .expect("valid adapter");
    let schema = json!({ "type": "array", "items": { "type": "integer" } });
    let request = codec
        .encode_request(
            &[user_message("numbers")],
            &OutputContract::Structured {
                schema: schema.clone(),
            },
        )
        .expect("valid request");
    assert_eq!(
        request.value["body"]["response_format"]["json_schema"]["schema"],
        schema
    );
    assert!(request.value["body"].get("stream_options").is_none());
    assert_eq!(request.value["body"]["messages"][0]["role"], "system");
    assert!(
        request.value["body"]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("lam_output_schema")
    );
    let response = response_payload(
        CHAT_RESPONSE_CODEC_ID,
        "structured",
        "response",
        json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "[1,2,3]" },
                "finish_reason": "stop"
            }]
        }),
    );
    assert_eq!(
        codec.interpret_response(&response).expect("valid output"),
        ModelDirective::Output(json!([1, 2, 3]))
    );
}

#[test]
fn adapter_rejects_invalid_cost_rates() {
    assert!(matches!(
        ChatCompletions::builder("test-model")
            .pricing(ModelPricing::new(f64::NAN, 1.0))
            .build_parts(),
        Err(BuildError::InvalidPricing)
    ));
}

#[test]
fn resumption_notice_closes_pending_eval_in_both_native_protocols() {
    let notice = recovery_notice_message();

    let (_, responses) = Responses::builder("gpt-test")
        .api_key("test-key")
        .build_parts()
        .expect("valid adapter");
    let responses_call = response_payload(
        RESPONSES_RESPONSE_CODEC_ID,
        "text",
        "response",
        json!({
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_responses",
                "name": "eval",
                "arguments": "{\"source\":\"sideEffect()\"}"
            }]
        }),
    );
    let request = responses
        .encode_request(
            &[
                projected(1, model_transition(), responses_call),
                projected(2, recovery_transition(), notice.clone()),
            ],
            &OutputContract::Text,
        )
        .expect("notice closes Responses call");
    assert_eq!(
        request.value["body"]["input"][1]["type"],
        "function_call_output"
    );
    assert_eq!(
        request.value["body"]["input"][1]["call_id"],
        "call_responses"
    );
    assert!(
        request.value["body"]["input"][1]["output"]
            .as_str()
            .unwrap()
            .contains("interruptedEvalOutcome")
    );

    let (_, chat) = ChatCompletions::builder("test-model")
        .build_parts()
        .expect("valid adapter");
    let chat_call = response_payload(
        CHAT_RESPONSE_CODEC_ID,
        "text",
        "response",
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_chat",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": "{\"source\":\"sideEffect()\"}"
                        }
                    }]
                }
            }]
        }),
    );
    let request = chat
        .encode_request(
            &[
                projected(1, model_transition(), chat_call),
                projected(2, recovery_transition(), notice),
            ],
            &OutputContract::Text,
        )
        .expect("notice closes Chat call");
    assert_eq!(request.value["body"]["messages"][1]["role"], "tool");
    assert_eq!(
        request.value["body"]["messages"][1]["tool_call_id"],
        "call_chat"
    );
    assert!(
        request.value["body"]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("interruptedEvalOutcome")
    );
}

#[tokio::test]
async fn responses_provider_sends_store_false_and_returns_completed_native_response() {
    let completed = json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "model": "gpt-test",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "hello" }]
        }],
        "usage": {
            "input_tokens": 40,
            "input_tokens_details": { "cached_tokens": 10 },
            "output_tokens": 5,
            "output_tokens_details": { "reasoning_tokens": 2 },
            "total_tokens": 45,
            "future_detail": "preserved"
        }
    });
    let stream = format!(
        "event: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        json!({ "type": "response.output_text.delta", "delta": "hello" }),
        json!({ "type": "response.completed", "response": completed })
    );
    let server = MockServer::start("text/event-stream", stream);
    let (provider, codec) = Responses::builder("gpt-test")
        .api_key("test-key")
        .base_url(format!("{}/v1", server.origin))
        .pricing(ModelPricing::new(1.0, 4.0).cached_input(0.1))
        .build_parts()
        .expect("valid adapter");
    let request = codec
        .encode_request(&[user_message("hello")], &OutputContract::Text)
        .expect("valid request");
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let captured_deltas = Arc::clone(&deltas);
    let response = provider
        .invoke(
            request,
            ModelEventSink::new(move |delta| captured_deltas.lock().unwrap().push(delta)),
        )
        .await
        .expect("successful response");
    let captured = server.finish();
    assert_eq!(captured.path, "/v1/responses");
    assert_eq!(captured.body["store"], false);
    assert_eq!(captured.body["include"][0], "reasoning.encrypted_content");
    assert_eq!(response.value["response"], completed);
    let metadata = codec.response_metadata(&response);
    let usage = metadata.usage.expect("usage metadata");
    assert_eq!(usage.input_tokens, 40);
    assert_eq!(usage.cached_input_tokens, Some(10));
    assert_eq!(usage.reasoning_tokens, Some(2));
    assert_eq!(usage.native["future_detail"], "preserved");
    let cost = metadata.cost.expect("cost estimate");
    assert_eq!(cost.source, ModelCostSource::Estimated);
    assert!((cost.amount_usd - 0.000_051).abs() < f64::EPSILON);
    assert_eq!(
        *deltas.lock().unwrap(),
        vec![ModelDelta::Text("hello".to_owned())]
    );
}

#[tokio::test]
async fn chat_provider_preserves_every_chunk_and_streams_reasoning() {
    let first = json!({
        "id": "chat_1",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "reasoning_content": "think" },
            "finish_reason": null
        }]
    });
    let second = json!({
        "id": "chat_1",
        "model": "accounts/test/resolved-model",
        "choices": [{
            "index": 0,
            "delta": { "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 30,
            "prompt_tokens_details": { "cached_tokens": 20 },
            "completion_tokens": 4,
            "completion_tokens_details": { "reasoning_tokens": 1 },
            "total_tokens": 34,
            "provider_extension": true
        }
    });
    let stream = format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n");
    let server = MockServer::start("text/event-stream; charset=utf-8", stream);
    let (provider, codec) = ChatCompletions::builder("accounts/test/model")
        .api_key("test-key")
        .base_url(format!("{}/inference/v1", server.origin))
        .pricing(ModelPricing::new(0.14, 0.28).cached_input(0.028))
        .build_parts()
        .expect("valid adapter");
    let request = codec
        .encode_request(&[user_message("hello")], &OutputContract::Text)
        .expect("valid request");
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let captured_deltas = Arc::clone(&deltas);
    let response = provider
        .invoke(
            request,
            ModelEventSink::new(move |delta| captured_deltas.lock().unwrap().push(delta)),
        )
        .await
        .expect("successful response");
    let captured = server.finish();
    assert_eq!(captured.path, "/inference/v1/chat/completions");
    assert_eq!(captured.body["stream"], true);
    assert_eq!(captured.body["stream_options"]["include_usage"], true);
    assert_eq!(response.value["chunks"], json!([first, second]));
    let metadata = codec.response_metadata(&response);
    assert_eq!(
        metadata.model.as_deref(),
        Some("accounts/test/resolved-model")
    );
    let usage = metadata.usage.expect("usage metadata");
    assert_eq!(usage.input_tokens, 30);
    assert_eq!(usage.cached_input_tokens, Some(20));
    assert_eq!(usage.native["provider_extension"], true);
    assert!(metadata.cost.is_some());
    assert_eq!(
        *deltas.lock().unwrap(),
        vec![
            ModelDelta::Reasoning("think".to_owned()),
            ModelDelta::Text("done".to_owned())
        ]
    );
}

#[tokio::test]
async fn responses_adapter_drives_a_complete_lam_eval_loop() {
    let first_response = json!({
        "id": "resp_eval",
        "object": "response",
        "status": "completed",
        "model": "gpt-test",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [],
                "encrypted_content": "opaque-reasoning"
            },
            {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_eval",
                "name": "eval",
                "arguments": "{\"source\":\"21 * 2\"}",
                "status": "completed"
            }
        ],
        "usage": {
            "input_tokens": 10,
            "output_tokens": 3,
            "total_tokens": 13
        }
    });
    let second_response = json!({
        "id": "resp_done",
        "object": "response",
        "status": "completed",
        "model": "gpt-test",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "42" }]
        }],
        "usage": {
            "input_tokens": 20,
            "output_tokens": 2,
            "total_tokens": 22
        }
    });
    let streams = vec![
        format!(
            "event: response.completed\ndata: {}\n\n",
            json!({ "type": "response.completed", "response": first_response })
        ),
        format!(
            "event: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            json!({ "type": "response.output_text.delta", "delta": "42" }),
            json!({ "type": "response.completed", "response": second_response })
        ),
    ];
    let server = MockServer::start_sequence("text/event-stream", streams);
    let model = Responses::builder("gpt-test")
        .api_key("test-key")
        .base_url(format!("{}/v1", server.origin))
        .pricing(ModelPricing::new(1.0, 4.0))
        .build()
        .expect("valid adapter");
    let mut actor = lam::Lam::builder(model)
        .build()
        .actor("responses-integration")
        .build()
        .await
        .expect("actor starts");
    let mut run = actor.call("compute the answer");
    let mut completed_models = Vec::new();
    while let Some(event) = run.next().await {
        if let RunEvent::ModelCompleted { metadata, .. } = event {
            completed_models.push(metadata);
        }
    }
    let answer: String = run.await.expect("run completes");
    assert_eq!(answer, "42");
    assert_eq!(completed_models.len(), 2);
    assert_eq!(
        completed_models
            .iter()
            .map(|metadata| metadata.usage.as_ref().unwrap().total_tokens)
            .sum::<u64>(),
        35
    );
    assert!(
        completed_models
            .iter()
            .all(|metadata| metadata.cost.is_some())
    );
    actor.shutdown().await.expect("actor shuts down");

    let requests = server.finish_all();
    assert_eq!(requests.len(), 2);
    let replay = requests[1].body["input"].as_array().unwrap();
    assert_eq!(replay[1]["encrypted_content"], "opaque-reasoning");
    assert_eq!(replay[2]["call_id"], "call_eval");
    assert_eq!(replay[3]["type"], "function_call_output");
    assert_eq!(replay[3]["call_id"], "call_eval");
}

#[tokio::test]
async fn chat_adapter_drives_a_complete_lam_eval_loop() {
    let first_chunks = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({
            "id": "chat_eval",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "reasoning_content": "I should calculate this.",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_eval",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": "{\"source\":\"6 * 7\"}"
                        }
                    }]
                },
                "finish_reason": null
            }]
        }),
        json!({
            "id": "chat_eval",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        })
    );
    let second_chunks = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id": "chat_done",
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": "42" },
                "finish_reason": "stop"
            }]
        })
    );
    let server = MockServer::start_sequence("text/event-stream", vec![first_chunks, second_chunks]);
    let model = ChatCompletions::builder("accounts/test/model")
        .api_key("test-key")
        .base_url(format!("{}/inference/v1", server.origin))
        .extra_body(json!({ "reasoning_history": "preserved" }))
        .build()
        .expect("valid adapter");
    let mut actor = lam::Lam::builder(model)
        .build()
        .actor("chat-integration")
        .build()
        .await
        .expect("actor starts");
    let answer: String = actor
        .call("compute the answer")
        .await
        .expect("run completes");
    assert_eq!(answer, "42");
    actor.shutdown().await.expect("actor shuts down");

    let requests = server.finish_all();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].body["reasoning_history"], "preserved");
    let messages = requests[1].body["messages"].as_array().unwrap();
    assert_eq!(messages[1]["reasoning_content"], "I should calculate this.");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_eval");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_eval");
}

fn user_message(text: &str) -> ProjectedContextEntry {
    projected(
        1,
        ContextTransition::Messages {
            run_id: run_id(),
            consumed_message_ids: vec![lam::MessageId::new("message-1").unwrap()],
        },
        payload(
            "lam/messages",
            json!([{
                "messageId": "message-1",
                "source": { "kind": "user", "principal": null },
                "payload": {
                    "codec": { "id": "lam/json", "version": 1 },
                    "value": text
                }
            }]),
        ),
    )
}

fn model_transition() -> ContextTransition {
    ContextTransition::Model {
        run_id: run_id(),
        progress: RunProgress::Continue,
    }
}

fn eval_transition() -> ContextTransition {
    ContextTransition::Eval { run_id: run_id() }
}

fn recovery_transition() -> ContextTransition {
    ContextTransition::Messages {
        run_id: run_id(),
        consumed_message_ids: vec![lam::MessageId::new("recovery-message").unwrap()],
    }
}

fn recovery_notice_message() -> EncodedPayload {
    payload(
        "lam/messages",
        json!([{
            "messageId": "recovery-message",
            "source": { "kind": "host", "component": "lam/runtime" },
            "payload": {
                "codec": { "id": "lam/system-notice", "version": 1 },
                "value": {
                    "type": "runtimeResumed",
                    "isolateState": "reset",
                    "resumedRunId": "run-1",
                    "interruptedEvalOutcome": "unknown"
                }
            }
        }]),
    )
}

fn run_id() -> RunId {
    RunId::new("run-1").unwrap()
}

fn projected(
    sequence: u64,
    transition: ContextTransition,
    payload: EncodedPayload,
) -> ProjectedContextEntry {
    ProjectedContextEntry {
        sequence: ContextSequence::new(sequence),
        revision: Revision::new(sequence),
        entry: ContextEntry {
            transition,
            payload,
            recorded_at: Timestamp::from_unix_millis(0),
        },
    }
}

fn payload(codec_id: &str, value: Value) -> EncodedPayload {
    EncodedPayload::new(codec(codec_id), value)
}

fn response_payload(
    codec_id: &str,
    output_kind: &str,
    field: &str,
    value: Value,
) -> EncodedPayload {
    let mut envelope = serde_json::Map::new();
    envelope.insert(
        "outputKind".to_owned(),
        Value::String(output_kind.to_owned()),
    );
    envelope.insert("model".to_owned(), Value::String("test-model".to_owned()));
    envelope.insert(field.to_owned(), value);
    payload(codec_id, Value::Object(envelope))
}

fn codec(id: &str) -> CodecRef {
    CodecRef::new(CodecId::new(id).unwrap(), 1)
}

struct CapturedRequest {
    path: String,
    body: Value,
}

struct MockServer {
    origin: String,
    request: mpsc::Receiver<CapturedRequest>,
    join: JoinHandle<()>,
    expected_requests: usize,
}

impl MockServer {
    fn start(content_type: &'static str, response_body: String) -> Self {
        Self::start_sequence(content_type, vec![response_body])
    }

    fn start_sequence(content_type: &'static str, response_bodies: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        let (sender, request) = mpsc::channel();
        let expected_requests = response_bodies.len();
        let join = std::thread::spawn(move || {
            for response_body in response_bodies {
                let (mut stream, _) = listener.accept().expect("accept request");
                let captured = read_request(&mut stream);
                sender.send(captured).expect("capture request");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        Self {
            origin: format!("http://{address}"),
            request,
            join,
            expected_requests,
        }
    }

    fn finish(self) -> CapturedRequest {
        assert_eq!(self.expected_requests, 1);
        self.finish_all().pop().expect("one captured request")
    }

    fn finish_all(self) -> Vec<CapturedRequest> {
        let requests = (0..self.expected_requests)
            .map(|_| {
                self.request
                    .recv_timeout(Duration::from_secs(2))
                    .expect("captured request")
            })
            .collect();
        self.join.join().expect("mock server thread");
        requests
    }
}

fn read_request(stream: &mut TcpStream) -> CapturedRequest {
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
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("request path")
        .to_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .expect("content-length header");
    while bytes.len() - header_end < content_length {
        let read = stream.read(&mut chunk).expect("read request body");
        assert!(read > 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .expect("JSON request body");
    CapturedRequest { path, body }
}
