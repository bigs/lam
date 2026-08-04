# lam-tui

`lam-tui` is the interactive `lam-agent` executable. It combines the embeddable
runtime with the coding capability pack, hierarchical subagents, and a compact
Ratatui conversation interface.

## Configuration

Lam reads `~/.lam/providers.toml` by default. `--config PATH` selects another
file. Provider names and model IDs form stable selector strings such as
`openai/gpt-5`. Provider-native model paths may contain `/`; the provider name
is simply prepended to form the selector. A ready-to-copy configuration for
OpenAI Responses, Fireworks, and Synthetic is in
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
efforts = ["none", "low", "medium", "high", "xhigh", "max"]

[[providers.models]]
id = "gpt-5-mini"
name = "GPT-5 mini"
context_window = 128000
efforts = ["none", "low", "medium", "high"]

[[providers]]
name = "local"
type = "openai-chat-completions"
api_base = "http://127.0.0.1:8000/v1"

[[providers.models]]
id = "coder"
name = "Local coder"
context_window = 32768
efforts = ["low", "high"]
```

The supported provider types are `openai-responses` and
`openai-chat-completions`. A provider can set either `api_key` directly or
`api_key_env` to resolve it from the process environment. When storing a key in
the file, restrict its filesystem permissions. An optional model-level
`extra_body` table carries provider-specific request options; Lam's protocol
invariants still override conflicting keys.

Each model's required `efforts` array is ordered from least to most effort. Lam
starts that model at the final (maximum) value and `/effort` can select any
listed value. By default, Responses providers receive it at
`reasoning.effort`, while Chat Completions providers receive it at
`reasoning_effort`. Set a provider-level `effort_path`, such as
`effort_path = "reasoning.effort"`, when an OpenAI-compatible provider uses a
different request shape. Do not also put the effort field in `extra_body`.

## Sessions

Lam keeps durable, directory-scoped sessions under `~/.lam/sessions`. The
session catalog is `index.redb`; each session has an independent
`session-<index>.redb` actor journal. The catalog records every session and the
latest session initiated from each canonical working directory.

Starting Lam resumes the latest session for the current directory, including
its root context, selected model, durable mailbox, compaction records, and all
actor journals. Each actor's conversation is reconstructed as an independent
view; ephemeral token deltas such as reasoning are not recoverable after
restart. The ready row includes the session index and exact journal path so a
session can be retained as a debugging or replay fixture. Historic journals are
not removed when a newer session becomes current.

`/new` gracefully closes the current runtime, creates a new journal rooted in
the same working directory, makes it current in the index, and clears the
visible conversation for the fresh session.

`/session` lists every session rooted in the current working directory, newest
first. Each choice previews the first user message on one line. Selecting a
session gracefully closes the current runtime, restores the selected journal,
and makes it the directory's default for the next launch.

## Debug diagnostics

Start Lam with `--debug-log` to append transport and runtime diagnostics to
`~/.lam/sessions/session-<index>.debug.jsonl`. The active file follows `/new`
and `/session`, is reused across process restarts, and is restricted to the
current user on Unix systems.

The log contains request and response sizes, timing, selected response headers,
SSE framing counters, protocol terminal markers, model/run correlation, and
HTTP error classifications and source chains. It does not record API keys,
authorization headers, prompts, generated text or reasoning, tool arguments,
or raw SSE payloads.

## Interaction

- Type a message and press Enter to call the root coding agent.
- Type `/` to open command completion. `/new`, `/session`, `/agents`, `/compact`,
  `/model`, `/effort`, `/exit`, and its `/quit` alias are available. `/model`
  opens models grouped by provider; `/effort` lists the active model's
  configured effort values.
- `/agents` opens the session's actor tree. Selecting an actor switches the
  conversation pane to that actor without interrupting a running root call.
- Tab completes an open menu. Otherwise it switches focus between the input
  shelf and conversation.
- Up/Down moves through wrapped or explicit input rows first. Beyond the top or
  bottom row, it navigates prior user messages and restores the current draft
  after the newest history item.
- In the conversation, arrows or `j`/`k` move by message. Enter or Space
  expands and collapses a row. Page Up/Down move in larger steps.
- Moving above the newest row detaches the viewport: incoming events continue
  below without moving the selection or scroll position. Move back to the
  newest row, or press End, to resume following live output.
- User messages start expanded. Agent text is expanded while it streams and
  stays that way when the run completes, including intermediate text that
  leads into a tool call. Both remain manually collapsible.
- Expanded user, agent, and reasoning rows render Markdown as it streams;
  collapsed previews remain compact plain text.
- Mouse wheel navigation and click-to-expand are supported.
- While the root is working, press Escape twice within 1.5 seconds to
  recoverably stop its complete agent tree. The first physical press shows a
  confirmation above the input; key repeat cannot confirm it, and any other
  key or timeout dismisses it. Draft input and pane focus are preserved.
- `Ctrl-C` clears a nonempty input draft; pressed again on an empty input, it
  exits from either pane.

The input shelf grows upward as text wraps and as completion choices appear.
The conversation is a virtual scrollback scoped to the agent named in the top
bar. Ordinary output, reasoning, eval calls/results, and compaction are
separate, expandable rows. Text, reasoning, and eval arguments continue
streaming into each agent's own view; background agents cannot move the visible
selection or viewport. Each eval row uses the model's brief intent as its stable
title while collapsed or expanded; the generated TypeScript is shown only in
its expanded body. The intent title streams as partial eval-argument JSON
arrives. The eventual eval outcome appears in its own result row. Reasoning,
eval call, and result rows render at full intensity while their run is
writing them, then turn faint once the stream moves on, keeping the
transcript calm; selecting a row also restores its full intensity.

After interruption, Lam reloads every affected view from its durable journal.
Incomplete text, reasoning, and tool-call JSON disappear; a committed eval call
remains paired with its interrupted result, and a model-visible runtime notice
marks the terminal boundary. Buffered events from the closed runs are ignored,
descendants remain browseable under `/agents`, and the root is ready for the
next message.

Run it from the project directory the agent should operate within:

```bash
cargo run -p lam-tui --bin lam-agent
```

The root and every subagent receive read/write filesystem, editing, and local
shell namespaces scoped to that directory. These are application-level path
guardrails, not an OS sandbox; see `lam-code`'s safety notes before using the
TUI with untrusted instructions.
