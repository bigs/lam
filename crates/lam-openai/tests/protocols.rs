//! Offline protocol, lossless-replay, and loopback transport tests.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use lam::{
    CodecId, CodecRef, CompactionArtifact, CompactionOutput, CompactionReason, CompactionRecord,
    CompactionRequest, Compactor, ContextEntry, ContextSequence, ContextTransition, EncodedPayload,
    ModelCodec, ModelCostSource, ModelDelta, ModelDirective, ModelEventSink, ModelProvider,
    ModelRequestConfig, ModelResponseMetadata, OutputContract, ProjectedContextEntry, Revision,
    RunEvent, RunId, RunProgress, Timestamp, ToolCallDelta,
};
use lam_openai::chat_completions::{
    ChatCompletions, REQUEST_CODEC_ID as CHAT_REQUEST_CODEC_ID,
    RESPONSE_CODEC_ID as CHAT_RESPONSE_CODEC_ID,
};
use lam_openai::responses::{
    OpenAiResponsesCompactor, REQUEST_CODEC_ID as RESPONSES_REQUEST_CODEC_ID,
    RESPONSE_CODEC_ID as RESPONSES_RESPONSE_CODEC_ID, Responses,
};
use lam_openai::{BuildError, ModelPricing, ProviderError};
use serde_json::{Value, json};

#[test]
fn responses_request_is_stateless_and_replays_encrypted_reasoning_unchanged() {
    let (_, codec) = Responses::builder("gpt-test")
        .api_key("test-key")
        .extra_body(json!({
            "store": true,
            "parallel_tool_calls": true,
            "instructions": "ignored",
            "include": ["message.output_text.logprobs"],
            "reasoning": { "effort": "high" }
        }))
        .build_parts()
        .expect("valid adapter");
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs_1",
        "summary": [{ "type": "summary_text", "text": "Inspect the result" }],
        "encrypted_content": "opaque-ciphertext"
    });
    let function_call = json!({
        "type": "function_call",
        "id": "fc_1",
        "call_id": "call_1",
        "name": "eval",
        "arguments": "{\"intent\":\"Calculate the result\",\"source\":\"1 + 1\",\"timeoutMs\":250}",
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
    let projection = codec.project_response(&response).expect("valid eval");
    assert_eq!(
        projection.directive,
        ModelDirective::Eval(lam::EvalRequest {
            intent: "Calculate the result".to_owned(),
            source: "1 + 1".to_owned(),
            timeout: Some(Duration::from_millis(250)),
        })
    );
    assert_eq!(
        projection.display,
        [
            ModelDelta::Reasoning("Inspect the result".to_owned()),
            ModelDelta::ToolCall(ToolCallDelta {
                index: 1,
                call_id: Some("call_1".to_owned()),
                name: Some("eval".to_owned()),
                arguments:
                    "{\"intent\":\"Calculate the result\",\"source\":\"1 + 1\",\"timeoutMs\":250}"
                        .to_owned(),
            }),
        ]
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
        .encode_request(
            &context,
            &ModelRequestConfig::agent(&OutputContract::Text, "runtime instructions"),
        )
        .expect("context can be replayed");
    assert_eq!(request.codec.id.as_str(), RESPONSES_REQUEST_CODEC_ID);
    let body = &request.value["body"];
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert!(body.get("parallel_tool_calls").is_none());
    assert_eq!(body["instructions"], "runtime instructions");
    assert_eq!(
        body["tools"][0]["parameters"]["required"],
        json!(["intent", "source", "timeoutMs"])
    );
    assert_eq!(
        body["tools"][0]["parameters"]["properties"]["intent"]["maxLength"],
        120
    );
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
    assert!(
        body["tools"][0]["description"]
            .as_str()
            .unwrap()
            .contains("at most once per assistant response")
    );
}

#[test]
fn responses_replays_rejections_for_parallel_eval_siblings() {
    let (_, codec) = Responses::builder("gpt-test")
        .api_key("test-key")
        .build_parts()
        .unwrap();
    let call = |id: &str, source: &str| {
        json!({
            "type": "function_call",
            "id": format!("fc_{id}"),
            "call_id": id,
            "name": "eval",
            "arguments": json!({
                "intent": format!("Run {source}"),
                "source": source,
                "timeoutMs": null,
            }).to_string(),
            "status": "completed",
        })
    };
    let response = response_payload(
        RESPONSES_RESPONSE_CODEC_ID,
        "text",
        "response",
        json!({
            "status": "completed",
            "output": [call("call_1", "1 + 1"), call("call_2", "2 + 2")],
        }),
    );
    let projection = codec.project_response(&response).unwrap();
    assert_eq!(projection.rejected_eval_calls, 1);
    assert!(matches!(
        projection.directive,
        ModelDirective::Eval(lam::EvalRequest { ref source, .. }) if source == "1 + 1"
    ));

    let request = codec
        .encode_request(
            &[
                projected(1, model_transition(), response),
                projected(
                    2,
                    eval_transition(),
                    payload("lam/eval", json!({ "status": "success", "output": 2 })),
                ),
                projected(
                    3,
                    eval_transition(),
                    payload(
                        "lam/eval",
                        json!({
                            "status": "rejected",
                            "message": "combine the work in one eval"
                        }),
                    ),
                ),
            ],
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
        )
        .unwrap();
    let input = request.value["body"]["input"].as_array().unwrap();
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_2");
    assert!(input[3]["output"].as_str().unwrap().contains("rejected"));
}

#[test]
fn responses_reject_unusable_calls_instead_of_failing_projection() {
    let (_, codec) = Responses::builder("gpt-test")
        .api_key("test-key")
        .build_parts()
        .unwrap();
    let call = |name: &str, arguments: &str| {
        response_payload(
            RESPONSES_RESPONSE_CODEC_ID,
            "text",
            "response",
            json!({
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": name,
                    "arguments": arguments,
                    "status": "completed",
                }],
            }),
        )
    };

    // Loose providers habitually emit snake_case field names; the alias keeps
    // the call an ordinary eval instead of a rejection.
    let snake_case = codec
        .project_response(&call(
            "eval",
            "{\"intent\":\"Sum\",\"source\":\"1 + 1\",\"timeout_ms\":250}",
        ))
        .unwrap();
    assert!(matches!(
        snake_case.directive,
        ModelDirective::Eval(lam::EvalRequest { timeout: Some(timeout), .. })
            if timeout == Duration::from_millis(250)
    ));

    let invalid = codec
        .project_response(&call(
            "eval",
            "{\"intent\":\"Sum\",\"source\":\"1 + 1\",\"timeout\":5}",
        ))
        .unwrap();
    let ModelDirective::Rejected { message } = &invalid.directive else {
        panic!("invalid eval arguments should reject the call: {invalid:?}");
    };
    assert!(message.contains("unknown field `timeout`"), "{message}");
    assert!(message.contains("`timeoutMs`"), "{message}");

    let unsupported = codec
        .project_response(&call("bash", "{\"command\":\"ls\"}"))
        .unwrap();
    let ModelDirective::Rejected { message } = &unsupported.directive else {
        panic!("an unknown function should reject the call: {unsupported:?}");
    };
    assert!(message.contains("`bash`"), "{message}");
    assert!(message.contains("`eval`"), "{message}");
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
            &ModelRequestConfig::agent(
                &OutputContract::Structured {
                    schema: schema.clone(),
                },
                "runtime instructions",
            ),
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
        codec
            .project_response(&response)
            .expect("valid output")
            .directive,
        ModelDirective::Output(json!({ "answer": 42 }))
    );
}

#[test]
fn responses_compaction_replays_only_the_durable_materialized_view() {
    let (_, codec) = Responses::builder("gpt-test")
        .api_key("test-key")
        .build_parts()
        .expect("valid adapter");
    let artifact = CompactionArtifact::summary("Current goal and state.");
    let replacement = codec
        .materialize_compaction(&artifact)
        .unwrap()
        .expect("Responses supports neutral compaction");
    let source = response_payload(
        RESPONSES_RESPONSE_CODEC_ID,
        "text",
        "response",
        json!({
            "output": [{
                "type": "reasoning",
                "encrypted_content": "raw-compaction-secret"
            }]
        }),
    );
    let record = CompactionRecord {
        strategy: "summary-tail".to_owned(),
        reason: CompactionReason::Threshold,
        source: Some(source),
        artifact: Some(artifact),
        replacement,
        metadata: ModelResponseMetadata::default(),
    };
    let context = vec![
        projected(
            10,
            ContextTransition::Compaction {
                covers_through: ContextSequence::new(8),
                run_id: None,
            },
            record.encode().unwrap(),
        ),
        user_message("exact tail"),
    ];
    let request = codec
        .encode_request(
            &context,
            &ModelRequestConfig::agent(&OutputContract::Text, "runtime"),
        )
        .unwrap();
    let input = request.value["body"]["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["role"], "user");
    assert!(input[0].to_string().contains("Continue the pending task"));
    assert!(input[0].to_string().contains("Current goal and state."));
    assert_eq!(input[1]["content"][0]["text"], "exact tail");
    assert!(!request.value.to_string().contains("raw-compaction-secret"));

    let summary_request = codec
        .encode_request(
            &[user_message("history")],
            &ModelRequestConfig::compaction("summarize", 512),
        )
        .unwrap();
    assert!(summary_request.value["body"].get("tools").is_none());
    assert_eq!(summary_request.value["body"]["max_output_tokens"], 512);
}

#[tokio::test]
async fn responses_native_compactor_preserves_the_canonical_checkpoint() {
    let native = json!({
        "id": "cmp_1",
        "object": "response.compaction",
        "model": "gpt-test",
        "output": [
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "retained" }]
            },
            {
                "type": "compaction",
                "id": "cmp_item_1",
                "encrypted_content": "opaque-checkpoint"
            }
        ],
        "usage": {
            "input_tokens": 120,
            "output_tokens": 8,
            "total_tokens": 128
        }
    });
    let server = MockServer::start("application/json", native.to_string());
    let model = Responses::builder("gpt-test")
        .api_key("test-key")
        .base_url(format!("{}/v1", server.origin))
        .build()
        .expect("valid adapter");
    let compactor = OpenAiResponsesCompactor::new(&model);
    let context = vec![user_message("history")];
    let request = CompactionRequest {
        reason: CompactionReason::Manual,
        context: context.clone(),
        instructions: "runtime instructions".to_owned(),
        target_model: None,
        units: lam::atomic_compaction_units(&context),
        previous: None,
        retain_tokens: 0,
        max_output_tokens: 512,
    };

    let plan = compactor
        .compact(&request)
        .await
        .expect("native compaction");
    assert_eq!(plan.strategy, "openai-responses-native");
    assert_eq!(plan.covers_through, ContextSequence::new(1));
    let CompactionOutput::Exact {
        replacement,
        artifact,
    } = plan.output
    else {
        panic!("native compaction must return an exact checkpoint");
    };
    assert!(artifact.is_none());
    assert_eq!(replacement.value, native["output"]);
    assert_eq!(plan.source.expect("full response retained").value, native);
    assert_eq!(plan.metadata.usage.unwrap().total_tokens, 128);

    let request = server.finish();
    assert_eq!(request.path, "/v1/responses/compact");
    assert_eq!(request.body["model"], "gpt-test");
    assert_eq!(request.body["instructions"], "runtime instructions");
    assert_eq!(request.body["input"][0]["role"], "user");
    assert!(request.body.get("tools").is_none());
    assert!(request.body.get("stream").is_none());
    assert!(request.body.get("store").is_none());
}

#[tokio::test]
async fn responses_native_checkpoint_is_installed_and_replayed_by_the_actor() {
    let first = json!({
        "id": "resp_first",
        "object": "response",
        "status": "completed",
        "model": "gpt-test",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "first" }]
        }]
    });
    let compacted = json!({
        "id": "cmp_actor",
        "object": "response.compaction",
        "model": "gpt-test",
        "output": [{
            "type": "compaction",
            "id": "cmp_item",
            "encrypted_content": "actor-opaque-checkpoint"
        }]
    });
    let second = json!({
        "id": "resp_second",
        "object": "response",
        "status": "completed",
        "model": "gpt-test",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "second" }]
        }]
    });
    let server = MockServer::start_mixed(vec![
        (
            "text/event-stream",
            format!(
                "event: response.completed\ndata: {}\n\n",
                json!({ "type": "response.completed", "response": first })
            ),
        ),
        ("application/json", compacted.to_string()),
        (
            "text/event-stream",
            format!(
                "event: response.completed\ndata: {}\n\n",
                json!({ "type": "response.completed", "response": second })
            ),
        ),
    ]);
    let model = Responses::builder("gpt-test")
        .api_key("test-key")
        .base_url(format!("{}/v1", server.origin))
        .build()
        .unwrap();
    let native = OpenAiResponsesCompactor::new(&model);
    let mut actor = lam::Lam::builder(model)
        .compactor(native)
        .compaction_config(lam::CompactionConfig::default().retain(lam::ContextAmount::Tokens(0)))
        .build()
        .actor("native-compaction")
        .build()
        .await
        .unwrap();
    let actor_ref = actor.actor_ref();

    assert_eq!(actor.call("first task").await.unwrap(), "first");
    actor.compact().await.unwrap().unwrap();
    let state = actor_ref.state().await.unwrap();
    let record = CompactionRecord::decode(&state.context().last().unwrap().entry.payload)
        .unwrap()
        .unwrap();
    assert!(record.artifact.is_none());
    assert_eq!(
        record.replacement.value[0]["encrypted_content"],
        "actor-opaque-checkpoint"
    );
    assert_eq!(record.source.unwrap().value, compacted);
    assert_eq!(actor.call("second task").await.unwrap(), "second");
    actor.shutdown().await.unwrap();

    let requests = server.finish_all();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/v1/responses", "/v1/responses/compact", "/v1/responses"]
    );
    assert_eq!(
        requests[2].body["input"][0]["encrypted_content"],
        "actor-opaque-checkpoint"
    );
    assert_eq!(requests[2].body["input"][1]["role"], "user");
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
    let projection = codec.project_response(&response).expect("valid eval");
    assert_eq!(
        projection.directive,
        ModelDirective::Eval(lam::EvalRequest {
            intent: "Evaluate TypeScript".to_owned(),
            source: "2 + 2".to_owned(),
            timeout: None,
        })
    );
    assert_eq!(
        projection.display,
        [
            ModelDelta::Reasoning("inspect ".to_owned()),
            ModelDelta::ToolCall(ToolCallDelta {
                index: 0,
                call_id: Some("call_1".to_owned()),
                name: Some("eval".to_owned()),
                arguments: "{\"source\":\"".to_owned(),
            }),
            ModelDelta::Reasoning("state".to_owned()),
            ModelDelta::ToolCall(ToolCallDelta {
                index: 0,
                call_id: None,
                name: None,
                arguments: "2 + 2\"}".to_owned(),
            }),
        ]
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
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
        )
        .expect("context can be replayed");
    assert_eq!(request.codec.id.as_str(), CHAT_REQUEST_CODEC_ID);
    let body = &request.value["body"];
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("parallel_tool_calls").is_none());
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["required"],
        json!(["intent", "source", "timeoutMs"])
    );
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
    assert!(
        body["tools"][0]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("at most once per assistant response")
    );
}

#[test]
fn chat_replays_rejections_for_parallel_eval_siblings() {
    let (_, codec) = ChatCompletions::builder("test-model")
        .include_usage(false)
        .build_parts()
        .unwrap();
    let response = response_payload(
        CHAT_RESPONSE_CODEC_ID,
        "text",
        "response",
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "eval",
                                "arguments": "{\"intent\":\"First\",\"source\":\"1 + 1\",\"timeoutMs\":null}"
                            }
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": {
                                "name": "eval",
                                "arguments": "{\"intent\":\"Second\",\"source\":\"2 + 2\",\"timeoutMs\":null}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        }),
    );
    let projection = codec.project_response(&response).unwrap();
    assert_eq!(projection.rejected_eval_calls, 1);
    assert!(matches!(
        projection.directive,
        ModelDirective::Eval(lam::EvalRequest { ref source, .. }) if source == "1 + 1"
    ));

    let request = codec
        .encode_request(
            &[
                projected(1, model_transition(), response),
                projected(
                    2,
                    eval_transition(),
                    payload("lam/eval", json!({ "status": "success", "output": 2 })),
                ),
                projected(
                    3,
                    eval_transition(),
                    payload(
                        "lam/eval",
                        json!({
                            "status": "rejected",
                            "message": "combine the work in one eval"
                        }),
                    ),
                ),
            ],
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
        )
        .unwrap();
    let messages = request.value["body"]["messages"].as_array().unwrap();
    assert_eq!(messages[1]["tool_call_id"], "call_1");
    assert_eq!(messages[2]["tool_call_id"], "call_2");
    assert!(
        messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("rejected")
    );
}

#[test]
fn chat_rejects_unusable_calls_and_replays_their_rejection_results() {
    let (_, codec) = ChatCompletions::builder("test-model")
        .include_usage(false)
        .build_parts()
        .unwrap();
    let response = response_payload(
        CHAT_RESPONSE_CODEC_ID,
        "text",
        "response",
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": "{\"intent\":\"Sum\",\"source\":\"1 + 1\",\"timeout\":5}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }),
    );
    let projection = codec.project_response(&response).unwrap();
    assert_eq!(projection.rejected_eval_calls, 0);
    let ModelDirective::Rejected { message } = &projection.directive else {
        panic!("invalid eval arguments should reject the call: {projection:?}");
    };
    assert!(message.contains("unknown field `timeout`"), "{message}");

    let request = codec
        .encode_request(
            &[
                projected(1, model_transition(), response),
                projected(
                    2,
                    eval_transition(),
                    payload(
                        "lam/eval",
                        json!({ "status": "rejected", "message": message }),
                    ),
                ),
            ],
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
        )
        .unwrap();
    let messages = request.value["body"]["messages"].as_array().unwrap();
    assert_eq!(messages[1]["tool_call_id"], "call_1");
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("unknown field"),
        "the replayed result must carry the rejection reason"
    );
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
            &ModelRequestConfig::agent(
                &OutputContract::Structured {
                    schema: schema.clone(),
                },
                "runtime instructions",
            ),
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
            .starts_with("runtime instructions\n\n")
    );
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
        codec
            .project_response(&response)
            .expect("valid output")
            .directive,
        ModelDirective::Output(json!([1, 2, 3]))
    );
}

#[test]
fn chat_compaction_replays_only_the_durable_materialized_view() {
    let (_, codec) = ChatCompletions::builder("test-model")
        .build_parts()
        .expect("valid adapter");
    let artifact = CompactionArtifact::summary("Current goal and state.");
    let replacement = codec
        .materialize_compaction(&artifact)
        .unwrap()
        .expect("Chat supports neutral compaction");
    let record = CompactionRecord {
        strategy: "summary-tail".to_owned(),
        reason: CompactionReason::Manual,
        source: Some(response_payload(
            CHAT_RESPONSE_CODEC_ID,
            "text",
            "response",
            json!({ "private_reasoning": "raw-compaction-secret" }),
        )),
        artifact: Some(artifact),
        replacement,
        metadata: ModelResponseMetadata::default(),
    };
    let context = vec![
        projected(
            10,
            ContextTransition::Compaction {
                covers_through: ContextSequence::new(8),
                run_id: None,
            },
            record.encode().unwrap(),
        ),
        user_message("exact tail"),
    ];
    let request = codec
        .encode_request(
            &context,
            &ModelRequestConfig::agent(&OutputContract::Text, "runtime"),
        )
        .unwrap();
    let messages = request.value["body"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "system, checkpoint, and exact tail");
    assert_eq!(messages[1]["role"], "user");
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("Continue the pending task")
    );
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("Current goal and state.")
    );
    assert_eq!(messages[2]["content"], "exact tail");
    assert!(!request.value.to_string().contains("raw-compaction-secret"));

    let summary_request = codec
        .encode_request(
            &[user_message("history")],
            &ModelRequestConfig::compaction("summarize", 512),
        )
        .unwrap();
    assert!(summary_request.value["body"].get("tools").is_none());
    assert_eq!(summary_request.value["body"]["max_tokens"], 512);
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
fn adapter_rejects_a_zero_stream_idle_timeout() {
    assert!(matches!(
        ChatCompletions::builder("test-model")
            .stream_idle_timeout(Duration::ZERO)
            .build_parts(),
        Err(BuildError::InvalidStreamIdleTimeout)
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
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
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
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
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

#[test]
fn durable_interruption_replays_eval_failure_and_notice_in_both_protocols() {
    let failure = payload(
        "lam/eval",
        json!({
            "status": "failure",
            "error": {
                "kind": "interrupted",
                "effects_may_have_completed": true,
                "previous_generation": 1,
                "new_generation": 2
            }
        }),
    );
    let notice = interruption_notice_message();

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
                projected(2, eval_transition(), failure.clone()),
                projected(3, interruption_transition(), notice.clone()),
            ],
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
        )
        .expect("durable interruption should replay through Responses");
    let input = request.value["body"]["input"].as_array().unwrap();
    assert_eq!(input[1]["type"], "function_call_output");
    assert_eq!(input[1]["call_id"], "call_responses");
    assert!(input[1]["output"].as_str().unwrap().contains("interrupted"));
    assert_eq!(input[2]["role"], "developer");
    assert!(input[2].to_string().contains("runInterrupted"));

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
                projected(2, eval_transition(), failure),
                projected(3, interruption_transition(), notice),
            ],
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
        )
        .expect("durable interruption should replay through Chat Completions");
    let messages = request.value["body"]["messages"].as_array().unwrap();
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "call_chat");
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("interrupted")
    );
    assert_eq!(messages[2]["role"], "system");
    assert!(
        messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("runInterrupted")
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
        "event: response.output_item.added\ndata: {}\n\nevent: response.function_call_arguments.delta\ndata: {}\n\nevent: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "eval",
                "arguments": ""
            }
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1,
            "delta": "{\"source\":\"1 + 1\"}"
        }),
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
        .encode_request(
            &[user_message("hello")],
            &ModelRequestConfig::agent(&OutputContract::Text, "runtime instructions"),
        )
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
    assert_eq!(captured.body["instructions"], "runtime instructions");
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
        vec![
            ModelDelta::ToolCall(ToolCallDelta {
                index: 1,
                call_id: Some("call_1".to_owned()),
                name: Some("eval".to_owned()),
                arguments: String::new(),
            }),
            ModelDelta::ToolCall(ToolCallDelta {
                index: 1,
                call_id: None,
                name: None,
                arguments: "{\"source\":\"1 + 1\"}".to_owned(),
            }),
            ModelDelta::Text("hello".to_owned()),
        ]
    );
}

#[tokio::test]
async fn chat_provider_folds_streamed_chunks_and_streams_reasoning() {
    let first = json!({
        "id": "chat_1",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "reasoning_content": "think",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": { "name": "eval", "arguments": "{\"source\":" }
                }]
            },
            "finish_reason": null
        }]
    });
    let second = json!({
        "id": "chat_1",
        "model": "accounts/test/resolved-model",
        "choices": [{
            "index": 0,
            "delta": {
                "content": "done",
                "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": "\"1 + 1\"}" }
                }]
            },
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
        .encode_request(
            &[user_message("hello")],
            &ModelRequestConfig::agent(&OutputContract::Text, "runtime instructions"),
        )
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
    assert_eq!(captured.body["messages"][0]["role"], "system");
    assert_eq!(
        captured.body["messages"][0]["content"],
        "runtime instructions"
    );
    assert_eq!(captured.body["stream_options"]["include_usage"], true);
    let folded = &response.value["response"];
    assert!(
        response.value.get("chunks").is_none(),
        "raw chunks must not persist"
    );
    assert_eq!(folded["choices"][0]["message"]["content"], "done");
    assert_eq!(
        folded["choices"][0]["message"]["reasoning_content"],
        "think"
    );
    assert_eq!(
        folded["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"source\":\"1 + 1\"}"
    );
    assert_eq!(folded["choices"][0]["finish_reason"], "stop");
    assert_eq!(folded["usage"]["total_tokens"], 34);
    assert_eq!(folded["usage"]["provider_extension"], true);
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
            ModelDelta::ToolCall(ToolCallDelta {
                index: 0,
                call_id: Some("call_1".to_owned()),
                name: Some("eval".to_owned()),
                arguments: "{\"source\":".to_owned(),
            }),
            ModelDelta::Text("done".to_owned()),
            ModelDelta::ToolCall(ToolCallDelta {
                index: 0,
                call_id: None,
                name: None,
                arguments: "\"1 + 1\"}".to_owned(),
            })
        ]
    );
}

#[tokio::test]
async fn chat_provider_accepts_a_terminal_chunk_before_a_truncated_body() {
    let terminal = json!({
        "id": "chat_complete",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": "complete" },
            "finish_reason": "stop"
        }]
    });
    let stream = format!("data: {terminal}\n\n");
    let server = MockServer::start_truncated("text/event-stream", stream);
    let (provider, codec) = ChatCompletions::builder("test-model")
        .base_url(format!("{}/v1", server.origin))
        .build_parts()
        .expect("valid adapter");
    let request = codec
        .encode_request(
            &[user_message("hello")],
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
        )
        .expect("valid request");

    let response = provider
        .invoke(request, ModelEventSink::new(|_| {}))
        .await
        .expect("terminal response should survive a trailing body failure");

    let folded = &response.value["response"];
    assert!(
        response.value.get("chunks").is_none(),
        "raw chunks must not persist"
    );
    assert_eq!(folded["choices"][0]["message"]["content"], "complete");
    assert_eq!(folded["choices"][0]["finish_reason"], "stop");
    server.finish();
}

#[tokio::test]
async fn chat_provider_rejects_a_truncated_body_before_a_terminal_chunk() {
    let partial = json!({
        "id": "chat_partial",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": "partial" },
            "finish_reason": null
        }]
    });
    let stream = format!("data: {partial}\n\n");
    let server = MockServer::start_truncated("text/event-stream", stream);
    let (provider, codec) = ChatCompletions::builder("test-model")
        .base_url(format!("{}/v1", server.origin))
        .build_parts()
        .expect("valid adapter");
    let request = codec
        .encode_request(
            &[user_message("hello")],
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
        )
        .expect("valid request");

    let error = provider
        .invoke(request, ModelEventSink::new(|_| {}))
        .await
        .expect_err("an incomplete response must still fail");

    assert!(matches!(error, ProviderError::Http(_)));
    assert!(error.to_string().contains("decoding response body"));
    server.finish();
}

#[tokio::test]
async fn chat_provider_fails_a_nonterminal_parallel_tool_stream_after_going_idle() {
    let parallel = json!({
        "id": "chat_parallel",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": [
                    {
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": "{\"intent\":\"First\",\"source\":\"1 + 1\",\"timeoutMs\":null}"
                        }
                    },
                    {
                        "index": 1,
                        "id": "call_2",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": "{\"intent\":\"Second\",\"source\":\"2 + 2\",\"timeoutMs\":null}"
                        }
                    },
                    {
                        "index": 2,
                        "id": "call_3",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": "{\"intent\":\"Third\",\"source\":\"3 + 3\",\"timeoutMs\":null}"
                        }
                    }
                ]
            },
            "finish_reason": null
        }]
    });
    let stream = format!("data: {parallel}\n\n");
    let server = MockServer::start_stalled("text/event-stream", stream, Duration::from_millis(250));
    let (provider, codec) = ChatCompletions::builder("test-model")
        .base_url(format!("{}/v1", server.origin))
        .stream_idle_timeout(Duration::from_millis(25))
        .build_parts()
        .unwrap();
    let request = codec
        .encode_request(
            &[user_message("run the work")],
            &ModelRequestConfig::agent(&OutputContract::Text, ""),
        )
        .unwrap();

    let error = provider
        .invoke(request, ModelEventSink::new(|_| {}))
        .await
        .expect_err("an unterminated assistant message must not be projected");
    assert!(matches!(
        error,
        ProviderError::StreamIdle { timeout } if timeout == Duration::from_millis(25)
    ));
    server.finish();
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
    assert!(
        requests[0].body["instructions"]
            .as_str()
            .unwrap()
            .starts_with("You are a coding agent with one tool, `eval`")
    );
    let replay = requests[1].body["input"].as_array().unwrap();
    assert_eq!(replay[1]["encrypted_content"], "opaque-reasoning");
    assert_eq!(replay[2]["call_id"], "call_eval");
    assert_eq!(replay[3]["type"], "function_call_output");
    assert_eq!(replay[3]["call_id"], "call_eval");
    let eval_output: Value = serde_json::from_str(replay[3]["output"].as_str().unwrap())
        .expect("Responses eval output should be JSON");
    assert_eq!(
        eval_output,
        json!({
            "status": "success",
            "output": {
                "result": { "kind": "json", "value": 42 },
                "logs": [],
            },
        })
    );
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
                "delta": {
                    "role": "assistant",
                    "content": "42",
                    "reasoning_content": null,
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }]
        })
    );
    let third_chunks = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id": "chat_follow_up",
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": "ready" },
                "finish_reason": "stop"
            }]
        })
    );
    let server = MockServer::start_sequence(
        "text/event-stream",
        vec![first_chunks, second_chunks, third_chunks],
    );
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
    let follow_up: String = actor
        .call("are you ready for another request?")
        .await
        .expect("follow-up run completes");
    assert_eq!(follow_up, "ready");
    actor.shutdown().await.expect("actor shuts down");

    let requests = server.finish_all();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].body["reasoning_history"], "preserved");
    let messages = requests[1].body["messages"].as_array().unwrap();
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("You are a coding agent with one tool, `eval`")
    );
    assert_eq!(messages[2]["reasoning_content"], "I should calculate this.");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call_eval");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_eval");
    let eval_output: Value = serde_json::from_str(messages[3]["content"].as_str().unwrap())
        .expect("Chat Completions eval output should be JSON");
    assert_eq!(
        eval_output,
        json!({
            "status": "success",
            "output": {
                "result": { "kind": "json", "value": 42 },
                "logs": [],
            },
        })
    );
    let follow_up_messages = requests[2].body["messages"].as_array().unwrap();
    assert_eq!(follow_up_messages[4]["role"], "assistant");
    assert_eq!(follow_up_messages[4]["content"], "42");
    assert!(follow_up_messages[4].get("reasoning_content").is_none());
    assert!(follow_up_messages[4].get("tool_calls").is_none());
    assert_eq!(follow_up_messages[5]["role"], "user");
}

#[tokio::test]
async fn chat_adapter_executes_first_terminal_parallel_eval_and_rejects_siblings() {
    let parallel = json!({
        "id": "chat_parallel_eval",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": [
                    {
                        "index": 0,
                        "id": "call_first",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": "{\"intent\":\"Calculate\",\"source\":\"6 * 7\",\"timeoutMs\":null}"
                        }
                    },
                    {
                        "index": 1,
                        "id": "call_second",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": "{\"intent\":\"Do another thing\",\"source\":\"sideEffect()\",\"timeoutMs\":null}"
                        }
                    },
                    {
                        "index": 2,
                        "id": "call_third",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": "{\"intent\":\"Do a third thing\",\"source\":\"otherEffect()\",\"timeoutMs\":null}"
                        }
                    }
                ]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let first_stream = format!("data: {parallel}\n\ndata: [DONE]\n\n");
    let done = json!({
        "id": "chat_parallel_done",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }]
    });
    let second_stream = format!("data: {done}\n\ndata: [DONE]\n\n");
    let server = MockServer::start_responses(vec![
        (
            "text/event-stream",
            first_stream.clone(),
            first_stream.len(),
        ),
        (
            "text/event-stream",
            second_stream.clone(),
            second_stream.len(),
        ),
    ]);
    let model = ChatCompletions::builder("test-model")
        .base_url(format!("{}/v1", server.origin))
        .build()
        .unwrap();
    let mut actor = lam::Lam::builder(model)
        .build()
        .actor("parallel-chat-integration")
        .build()
        .await
        .unwrap();

    let answer: String = actor.call("run the work").await.unwrap();
    assert_eq!(answer, "done");
    actor.shutdown().await.unwrap();

    let requests = server.finish_all();
    let messages = requests[1].body["messages"].as_array().unwrap();
    assert_eq!(messages[2]["tool_calls"].as_array().unwrap().len(), 3);
    assert_eq!(messages[3]["tool_call_id"], "call_first");
    assert_eq!(messages[4]["tool_call_id"], "call_second");
    assert_eq!(messages[5]["tool_call_id"], "call_third");
    let first: Value = serde_json::from_str(messages[3]["content"].as_str().unwrap()).unwrap();
    let second: Value = serde_json::from_str(messages[4]["content"].as_str().unwrap()).unwrap();
    let third: Value = serde_json::from_str(messages[5]["content"].as_str().unwrap()).unwrap();
    assert_eq!(first["status"], "success");
    assert_eq!(first["output"]["result"]["value"], 42);
    assert_eq!(second["status"], "rejected");
    assert_eq!(third["status"], "rejected");
    for rejected in [second, third] {
        let message = rejected["message"].as_str().unwrap();
        assert!(message.contains("executes only the first tool call"));
        assert!(message.contains("one eval program"));
        assert!(message.contains("Promise.all"));
    }
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

fn interruption_transition() -> ContextTransition {
    ContextTransition::Interrupted {
        run_id: run_id(),
        consumed_message_ids: vec![lam::MessageId::new("interruption-message").unwrap()],
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

fn interruption_notice_message() -> EncodedPayload {
    payload(
        "lam/messages",
        json!([{
            "messageId": "interruption-message",
            "source": { "kind": "host", "component": "lam/runtime" },
            "payload": {
                "codec": { "id": "lam/system-notice", "version": 1 },
                "value": {
                    "type": "runInterrupted",
                    "runId": "run-1",
                    "reason": "user",
                    "isolateState": "reset",
                    "interruptedEvalOutcome": "failureRecorded"
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
        entry: Arc::new(ContextEntry {
            transition,
            payload,
            recorded_at: Timestamp::from_unix_millis(0),
        }),
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
        Self::start_mixed(
            response_bodies
                .into_iter()
                .map(|body| (content_type, body))
                .collect(),
        )
    }

    fn start_truncated(content_type: &'static str, response_body: String) -> Self {
        Self::start_responses(vec![(
            content_type,
            response_body.clone(),
            response_body.len() + 128,
        )])
    }

    fn start_stalled(content_type: &'static str, response_body: String, stall: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        let (sender, request) = mpsc::channel();
        let join = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let captured = read_request(&mut stream);
            sender.send(captured).expect("capture request");
            let content_length = response_body.len() + 128;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n{response_body}"
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            stream.flush().expect("flush response");
            std::thread::sleep(stall);
        });
        Self {
            origin: format!("http://{address}"),
            request,
            join,
            expected_requests: 1,
        }
    }

    fn start_mixed(responses: Vec<(&'static str, String)>) -> Self {
        Self::start_responses(
            responses
                .into_iter()
                .map(|(content_type, response_body)| {
                    let content_length = response_body.len();
                    (content_type, response_body, content_length)
                })
                .collect(),
        )
    }

    fn start_responses(responses: Vec<(&'static str, String, usize)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        let (sender, request) = mpsc::channel();
        let expected_requests = responses.len();
        let join = std::thread::spawn(move || {
            for (content_type, response_body, content_length) in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let captured = read_request(&mut stream);
                sender.send(captured).expect("capture request");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n{response_body}"
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
