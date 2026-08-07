# lam-tui

`lam-tui` is the interactive `lam-agent` executable. It combines the embeddable
runtime with the coding capability pack, hierarchical subagents, and a compact
Ratatui conversation interface.

## Configuration

Lam reads custom providers from `~/.lam/providers.toml` by default. `--config
PATH` selects another file. A signed-in Codex subscription is added to the
default provider catalog automatically, so it does not need a TOML entry. Lam
can also start without `~/.lam/providers.toml` when Codex login credentials are
available. An explicit `--config` file stays authoritative and does not receive
automatic providers.

### Agent batteries (`~/.lam/config.toml`)

Optional web search packs live in a separate file from inference providers.
Copy [`config.example.toml`](config.example.toml) to `~/.lam/config.toml` and
set API keys via environment variables:

```toml
[exa]
enabled = true
api_key_env = "EXA_API_KEY"
# functions = ["search", "contents", "context", "answer", "findSimilar"]

[parallel]
enabled = true
api_key_env = "PARALLEL_API_KEY"
# functions = ["search", "extract"]
```

When a key is present, Lam installs the corresponding namespaces
(`lam.exa.*`, `lam.parallel.*`) for the root agent and subagents. Missing keys
soft-skip that provider with a startup warning. Function names and request
shapes match each provider's public API; call `lam.dir()` for schemas.

Provider names and model IDs form stable selector strings such as
`openai/gpt-5`. Provider-native model paths may contain `/`; the provider name
is simply prepended to form the selector. A ready-to-copy configuration for
custom OpenAI Responses, Fireworks, and Synthetic providers is in
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

The supported provider types are `openai-responses`,
`openai-chat-completions`, `openai-codex`, and `xai-supergrok`. A provider can
set either `api_key` directly or `api_key_env` to resolve it from the process environment.
When storing a key in the file, restrict its filesystem permissions. An optional
model-level `extra_body` table carries provider-specific request options; Lam's
protocol invariants still override conflicting keys.

### Codex subscription (`openai-codex`)

Use a Codex-enabled ChatGPT subscription without an OpenAI Platform API key.
Codex subscription models use a **272,000-token** context window (the official
Codex catalog standard), not the Platform API ~1.05M window used for
`openai-responses` entries of the same model names.

Sign in once, then start Lam:

```bash
lam-agent login openai
lam-agent
# headless / SSH:
lam-agent login openai --no-browser
```

Press `Alt+M`, or type `/model`, to select the Codex provider and model. The
picker groups all usable models by provider. The selected provider and model
are stored in the durable session and return when that session resumes.

Lam reads the shared Codex login cache from `$CODEX_HOME/auth.json`, or
`~/.codex/auth.json` when `CODEX_HOME` is not set. It refreshes an expiring
access token and writes the updated tokens back with user-only permissions.

`lam-agent login openai` runs the OAuth2 authorization-code + PKCE flow directly
against OpenAI — no `codex` executable is required. It prints an authorization
URL, opens your browser unless `--no-browser` is given, and waits for the
redirect on a local loopback listener before exchanging the code for tokens.
If a valid login already exists in the shared cache, the command refuses to
overwrite it and tells you to run `lam-agent logout openai` first; pass
`--force` to replace it anyway.

Subscription model routing is gated on the compatibility identity of the
official Codex CLI, so Lam sends the hardcoded `CODEX_CLIENT_VERSION`
(crates/lam-tui/src/codex.rs) in the `version` and `User-Agent` headers; bump
that constant when a model requires a newer Codex version. `lam-agent logout
openai` removes the shared login, so it also signs the official Codex CLI out.

Do not set `api_key` or `api_key_env` on an `openai-codex` provider. Available
models and usage limits depend on the selected ChatGPT workspace and plan. Lam
does not allow an `api_base` override for this provider because that could send
the shared Codex credential to another server.

### SuperGrok subscription (`xai-supergrok`)

Use a SuperGrok or X Premium subscription instead of an xAI console API key:

```toml
[[providers]]
name = "xai"
type = "xai-supergrok"

[[providers.models]]
id = "grok-4.5"
name = "Grok 4.5"
context_window = 500000
efforts = ["low", "medium", "high"]
```

Sign in once with device-code OAuth:

```bash
lam-agent login xai
# headless / SSH:
lam-agent login xai --no-browser
```

Credentials are stored at `~/.lam/auth/xai.json` (mode `0600`). On first use of
an `xai-supergrok` provider with no stored credentials, Lam also tries to import
`~/.grok/auth.json` from the official Grok Build CLI, then falls back to an
interactive device login. Inference goes to `https://cli-chat-proxy.grok.com/v1`
via the Responses API and draws from the same weekly SuperGrok usage pool as
Grok chat and Grok Build.

Each model's required `efforts` array is ordered from least to most effort. Lam
starts that model at the final (maximum) value and `/effort` can select any
listed value. By default, Responses providers receive it at
`reasoning.effort`, while Chat Completions providers receive it at
`reasoning_effort`. Set a provider-level `effort_path`, such as
`effort_path = "reasoning.effort"`, when an OpenAI-compatible provider uses a
different request shape. Do not also put the effort field in `extra_body`.

For OpenAI Responses models that support it, request provider-visible reasoning
summaries with a sibling `extra_body` field. Lam's `/effort` control still owns
`reasoning.effort` and injects the selected value beside this setting:

```toml
[[providers.models]]
id = "gpt-5.6-luna"
name = "GPT-5.6 Luna"
context_window = 1050000
efforts = ["none", "low", "medium", "high", "xhigh", "max"]

[providers.models.extra_body.reasoning]
summary = "auto"
```

`summary` may also be `concise` or `detailed` when the endpoint accepts those
modes. Leave the field unset for providers/models that reject it (including
most custom Responses proxies). Completed summaries are durable in the native
response payload and reappear after restart; partial streamed summary text is
ephemeral like other token deltas.

## Project instructions

At boot, Lam follows the Codex convention for project instruction files: it
walks from the working directory up to the nearest ancestor containing a
`.git` marker and reads `AGENTS.md` (preferred) or `CLAUDE.md` (fallback) at
every directory from that root down to the working directory, inclusive. When
no `.git` marker exists, only the working directory is considered.

The discovered files are interpolated into the root agent's system prompt as a
single `Project instructions` section, ordered root-first so deeper files take
precedence on conflicts. The section is read once at boot and is not shown in
the transcript; subagents do not inherit it, and no global or home-directory
file is consulted.

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

- Type a message and press Enter to send it to the root coding agent. Every
  message is admitted as a durable steer: if the root is idle it starts a new
  run, and if the root is mid-run it is delivered at the next model boundary
  (after the pending tool result, or immediately after the terminal output).
- Type `/` to open command completion. `/new`, `/session`, `/agents`, `/compact`,
  `/model`, `/effort`, `/exit`, and its `/quit` alias are available. `/model`
  opens models grouped by provider; `/effort` lists the active model's
  configured effort values.
- Press `Alt+M` to open the provider and model picker directly. Type to filter,
  use Up/Down to select an item, and press Enter twice to confirm the switch.
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
- Until a sent message is delivered, it appears as a one-line pending preview
  directly above the input bar (with a `+N more` marker when several are
  queued). At the moment the runner consumes it, its row enters the
  conversation at its exact journal position, like any other user message.
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
shell namespaces based in that directory. Their paths are not confined to it,
and the local shell inherits the Lam process's host authority. See `lam-code`'s
safety notes before using the TUI with untrusted instructions.
