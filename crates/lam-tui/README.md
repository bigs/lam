# lam-tui

`lam-tui` is the interactive `lam` executable. It combines the embeddable
runtime with the coding capability pack, hierarchical subagents, and a compact
Ratatui conversation interface.

## Configuration

Lam reads `~/.lam/providers.toml` by default. `--config PATH` selects another
file. Provider names and model IDs form stable selector strings such as
`openai/gpt-5`. Provider-native model paths may contain `/`; the provider name
is simply prepended to form the selector. A ready-to-copy configuration for
OpenAI Responses and Fireworks is in
[`providers.example.toml`](providers.example.toml).

```toml
default_model = "openai/gpt-5"

[[providers]]
name = "openai"
type = "openai-responses"
api_key_env = "OPENAI_API_KEY"
# api_base = "https://api.openai.com/v1"

[[providers.models]]
id = "gpt-5"
name = "GPT-5"
context_window = 400000

[providers.models.extra_body.reasoning]
effort = "high"

[[providers.models]]
id = "gpt-5-mini"
name = "GPT-5 mini"
context_window = 128000

[[providers]]
name = "local"
type = "openai-chat-completions"
api_base = "http://127.0.0.1:8000/v1"

[[providers.models]]
id = "coder"
name = "Local coder"
context_window = 32768

[providers.models.extra_body]
reasoning_effort = "high"
```

The supported provider types are `openai-responses` and
`openai-chat-completions`. A provider can set either `api_key` directly or
`api_key_env` to resolve it from the process environment. When storing a key in
the file, restrict its filesystem permissions. An optional model-level
`extra_body` table carries provider-specific request options; Lam's protocol
invariants still override conflicting keys. The smallest configured context
window is used as the root's conservative automatic-compaction threshold, so a
model switch cannot silently exceed a smaller target model's limit.

## Interaction

- Type a message and press Enter to call the root coding agent.
- Type `/` to open command completion. `/compact`, `/model`, and `/exit` are
  available. `/model` opens models grouped by provider.
- Tab completes an open menu. Otherwise it switches focus between the input
  shelf and conversation.
- In the conversation, arrows or `j`/`k` move by message. Enter or Space
  expands and collapses a row. Page Up/Down move in larger steps.
- User messages start expanded. Agent text is expanded while it streams and
  stays that way when the run completes; if the same run continues into a tool
  call, the intermediate text collapses. Both remain manually collapsible.
- Mouse wheel navigation and click-to-expand are supported.
- `Ctrl-C` exits from either pane.

The input shelf grows upward as text wraps and as completion choices appear.
The conversation is a virtual scrollback: ordinary output, reasoning, eval
calls/results, compaction, and subagent lifecycle events are separate,
expandable rows. Eval arguments stream into the call row while the model
constructs them; the eventual eval outcome appears in its own result row.

Run it from the project directory the agent should operate within:

```bash
cargo run -p lam-tui --bin lam
```

The root and every subagent receive read/write filesystem, editing, and local
shell namespaces scoped to that directory. These are application-level path
guardrails, not an OS sandbox; see `lam-code`'s safety notes before using the
TUI with untrusted instructions.
