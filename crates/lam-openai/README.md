# `lam-openai`

Lossless model adapters for [lam](../../README.md): OpenAI's Responses API and
the broadly implemented OpenAI-compatible Chat Completions API.

Both adapters return the provider-neutral `lam::Model<P, C>` expected by
`Lam::builder`, while keeping provider-native JSON authoritative in durable
context.

## Protocols

| Builder | Intended API | Durable native payload |
| --- | --- | --- |
| `responses::Responses` | OpenAI Responses | Untouched completed response object |
| `chat_completions::ChatCompletions` | OpenAI and compatible `/chat/completions` providers | Every native SSE chunk, or the untouched non-streaming response |

These are separate builders because the protocols have different replay and
tool-call semantics. They share only transport, SSE parsing, context helpers,
errors, and metadata extraction.

## OpenAI Responses

```rust,ignore
use lam::Lam;
use lam_openai::responses::Responses;

let model = Responses::builder("gpt-5.6-luna")
    .api_key(std::env::var("OPENAI_API_KEY")?)
    .build()?;

let mut actor = Lam::builder(model)
    .context_window_tokens(128_000)
    .build()
    .actor("assistant")
    .build()
    .await?;
```

The adapter always sends `store: false`, requests encrypted reasoning content,
declares only the `eval` function, and manually replays complete native output
items. Runtime-owned fields cannot be overridden through `extra_body`.

## Compatible Chat Completions

```rust,ignore
use lam_openai::{ModelPricing, chat_completions::ChatCompletions};
use serde_json::json;

let model = ChatCompletions::builder("accounts/fireworks/models/MODEL_ID")
    .api_key(std::env::var("FIREWORKS_API_KEY")?)
    .base_url("https://api.fireworks.ai/inference/v1")
    .extra_body(json!({
        "reasoning_effort": "high",
        "reasoning_history": "preserved"
    }))
    .pricing(ModelPricing::new(0.14, 0.28).cached_input(0.028))
    .build()?;
```

Unknown response fields, reasoning content, encrypted/signature values,
indexed reasoning details, and tool-call objects are preserved rather than
decoded through a closed common assistant-message type. The adapter requests
streaming usage by default; compatibility settings can disable that option for
servers which reject it.

## Transport customization

Both builders support:

- bearer API keys;
- a custom base URL;
- an embedding-provided `reqwest::Client`;
- extra request JSON for provider-specific options;
- explicit per-model pricing metadata.

lam owns the fields required for its control-flow invariants, so extra JSON
cannot replace the model, messages/input, tool declaration, output contract,
or streaming policy.

## Reasoning, usage, and cost

Completed responses remain native encoded payloads in the journal. Codecs
derive a non-authoritative metadata view containing input, cached-input,
output, reasoning, and total token counts plus the untouched provider usage
object.

`RunEvent::ModelCompleted` and Rust `tracing` expose this metadata. Cost is
reported only when the embedding supplies current USD-per-million-token rates
through `ModelPricing`; estimates are labeled and lam intentionally ships no
price catalog.

Visible text/reasoning deltas are ephemeral `ModelDelta` values. Only completed
native provider payloads become durable context.

## Native Responses compaction

`responses::OpenAiResponsesCompactor` calls OpenAI's native compaction endpoint
and returns the exact opaque checkpoint items needed for replay:

```rust,ignore
use lam::{FallbackCompactor, SummaryTailCompactor};
use lam_openai::responses::OpenAiResponsesCompactor;

let compactor = FallbackCompactor::new(
    OpenAiResponsesCompactor::new(&model),
    SummaryTailCompactor::new(model.clone()),
);
```

Selection is explicit—there is no model-name capability table. A fallback
chain can recover to the provider-neutral summary-tail strategy.

## Testing

Offline protocol tests use local mock servers and assert exact request/replay
JSON, SSE behavior, structured output, reasoning preservation, usage, and full
two-request lam loops. Ignored live tests require `OPENAI_API_KEY` or
`FIREWORKS_API_KEY` and make bounded real requests.

See the public [`lam`](../lam/README.md) facade and repository
[README](../../README.md).
