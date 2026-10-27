# Marmennill (marmel)

A terminal-driven autonomous coding assistant in Rust. Marmel connects to any OpenAI-compatible chat completions backend (such as Ollama, vLLM, or OpenRouter) and coordinates a **Manager + Specialist Subagent** architecture to plan, execute, and validate software tasks within a local workspace. It runs either as an interactive Ratatui TUI or as a headless, pipe-friendly CLI.

---

## Table of Contents

- [Overview](#overview)
- [Quick Start](#quick-start)
- [Model Recommendations](#model-recommendations)
- [Configuration](#configuration)
- [Usage & Interface](#usage--interface)
  - [CLI Flags](#cli-flags)
  - [Interactive TUI](#interactive-tui)
  - [Keyboard Shortcuts](#keyboard-shortcuts)
  - [Slash Commands](#slash-commands)
  - [Headless Raw Mode](#headless-raw-mode)
  - [Runtime Directory (`.marmel/`)](#runtime-directory-marmel)
- [Real-Time Steering & Live Dialogue](#real-time-steering--live-dialogue)
- [Agent Archetypes & Customization](#agent-archetypes--customization)
  - [Built-In Archetypes](#built-in-archetypes)
  - [Custom Archetypes (`AGENTS.md`)](#custom-archetypes-agentsmd)
  - [Custom Skills (`skills/`)](#custom-skills-skills)
- [Security & Sandboxing](#security--sandboxing)
- [Testing](#testing)
- [License](#license)

---

## Overview

Marmel is designed for autonomous multi-step software engineering with continuous user supervision:

- **Plan-Driven Execution:** When given a goal, the Manager creates a disk-backed execution plan at `.marmel/execution_plan.md` using a checkbox format (`- [ ] [t-xxx]`), and a task line may additionally carry an optional validation directive from a closed set — `(validate)` or `(no-validate)` — written **immediately before** the `(role)` hint and never after it: `- [ ] [t-001] Audit the parser (no-validate) (researcher)`. No directive means the role/config default, a blank or unrecognised paren group is never read as `(no-validate)`, and the token decides validation — not the wording of the brief. Tasks are checked off only when the deliverable carries its `MISSION COMPLETE (t-xxx)` marker — `FAILED`, `REPLAN REQUIRED`, or a marker-less result leaves the box unchecked — so a resumed run continues from the next unchecked task.
- **Dynamic Subagents & JIT Prompt Architect:** Tasks are not sent to static prompts. An Agent Architect dynamically synthesizes a bespoke subagent blueprint (`.marmel/prompts/<task_id>.md`) combining archetype capabilities, domain skills (`skills/`), and least-privilege tool allowlists before delegation.
- **Automated Validation:** Work produced by specialists is audited by an independent Validator subagent. If tests fail or requirements are missed, feedback is returned for revision. The audit is not unconditional: precedence runs role opt-out (`validator` / `planner`), then operator config (`enable_validator = false` or `max_validator_iterations = 0`), then the per-task `(no-validate)` / `(validate)` plan directive, then the role/config default — and a task designated validation-free still completes normally and checks its box off.
- **Real-Time Steering & Interactive Dialogue:** You can type into the prompt at any point. Marmel pauses active subagents to answer questions, adjust the execution plan, or converse directly with working agents in real time.
- **Model Context Protocol (MCP):** Connect external tool servers via stdio or HTTP/SSE using standard MCP configurations.
- **Workspace Security:** File operations are strictly confined to the workspace. On Linux, terminal commands are isolated using Linux Landlock LSM to protect sensitive directories like `~/.ssh` and `~/.gnupg`.

```text
                           ┌───────────────────────────┐
                           │     User (TUI / CLI)      │
                           └─────────────┬─────────────┘
                                         │  Real-Time Steering / Feedback
                                         ▼
                           ┌───────────────────────────┐
                           │     Steer Arbitrator      │◄────────────┐
                           └──────┬─────────────┬──────┘             │
                  Steer Directives│             │ Inquiry / Guidance │ Live Response /
                  / Plan Updates  │             │                    │ Clarification
                                  ▼             ▼                    │
      ┌───────────────────────────────────┐   ┌──────────────────────┴────────────┐
      │        OrchestratorManager        │   │      Dynamic Subagent Worker      │
      │  • Execution plan (.marmel/)      │   │   (Coder, Debugger, Custom...)    │
      │  • Audits via Validator loop      │   └───────────────────┬───────────────┘
      │  • Dispatches tasks sequentially  │                       │
      └─────────────────┬─────────────────┘                       │
                        │                                         │
                        │ delegate_task                           │
                        ▼                                         │
      ┌───────────────────────────────────┐                       │
      │       Agent Architect (JIT)       │                       │
      │  • Dynamic Skill Catalog          │                       │
      │    (skills/*.md, AGENTS.md)       │                       │
      │  • Tailored prompt + tool list    │                       │
      │  • Writes .marmel/prompts/        │                       │
      └─────────────────┬─────────────────┘                       │
                        │ spawns with tailored blueprint          │
                        └─────────────────────────────────────────┤
                                                                  ▼
                                                  ┌───────────────────────────────┐
                                                  │      Scoped Tool Harness      │
                                                  │  • Workspace Path Confinement │
                                                  │  • Linux Landlock Sandbox     │
                                                  │  • MCP Servers (stdio / SSE)  │
                                                  └───────────────────────────────┘
```

---

## Quick Start

### Prerequisites

- **Rust toolchain 1.98+** (edition 2024).
- An **OpenAI-compatible LLM backend** reachable over HTTP (e.g. Ollama at `http://localhost:11434/v1`, vLLM, or OpenRouter).

### Installation & Build

```bash
git clone https://github.com/Na1w/marmel.git
cd marmel
cargo build --release
```

The compiled binary will be placed at `target/release/marmel`.

### Run

```bash
# Start the interactive TUI
./target/release/marmel

# Start with an initial goal
./target/release/marmel "Refactor the parser module and add unit tests"

# Headless raw mode (suitable for scripts and pipes)
./target/release/marmel --raw "Explain the architecture in src/main.rs"
```

---

## Model Recommendations

Marmel requires reliable JSON schema tool calling and solid instruction-following to successfully drive autonomous loops:

- **Recommended:**
  - **Qwen 3.8 27B** (`qwen-3.8-27b`) — the built-in default (`src/config.rs`), and the value used by the examples in this file. Solid balance of reasoning depth and dependable tool invocation on local hardware.
  - **Qwen 3.8 Flash Next** (`qwen-3.8-flash-next`) — When run on modern inference engines such as Strata, this model is significantly faster than Qwen 3.8 27B while offering a similar level of overall performance.
  - **DeepSeek v4.1 Flash** — Offers a similar level of performance to both models above.
- **Minimum:**
  - Models around the lighter recommended size. Smaller parameter sizes or aggressive quantizations frequently fail JSON tool schemas or drift from task scopes.
- **Not Recommended:**
  - **Qwen 3.6 35B A3B** — While this model works most of the time, it tends to struggle with certain tool calls that cause tasks to take an artificially long time or fail altogether; it is therefore recommended to avoid it for now.
  - Models that drop or malform JSON tool-call schemas during multi-turn loops. Marmel recovers pseudo-XML calls when it can (`enable_xml_rescue`), but a model that never emits a valid schema cannot drive the plan, the validator loop, or the fix loop at all.

Note that Marmel is heavily KV-cache optimized, which benefits both local and cloud inference: it increases turnaround speed for local setups and reduces costs in the cloud.

---

## Configuration

### Lookup Order

Marmel reads exactly one configuration file — the first candidate below wins, files are never layered — and then applies environment variables on top of it, so an environment variable overrides the file for the keys it covers:

1. `--config <path>` CLI flag.
2. `./marmel.toml` (current workspace root).
3. `./.marmel.toml`
4. `./.marmel/marmel.toml`
5. `./.marmel/config.toml`
6. `~/.marmel/marmel.toml`
7. `~/.marmel/config.toml`
8. `~/.config/marmel/config.toml`
9. `~/.config/marmel/marmel.toml`
10. Environment variables (`MARMEL_BACKEND_URL`, `MARMEL_MODEL`, `MARMEL_AUTH_TOKEN`) — applied after the file, so they override it; `MARMEL_AUTH_TOKEN` applies only when the resolved token is still empty.
11. Session environment variables (`MARMEL_RESUME`, one-way `MARMEL_SESSION_DISABLE`, `MARMEL_SESSION_COMMIT_MIN_MS`) — they fold into the `session_*` keys the same way, with `--resume` / `--fresh` beating all three.
12. Built-in defaults.

### Configuration Reference (`marmel.toml`)

| Field | Default | Description |
|---|---|---|
| `backend_url` | `http://localhost:8000/v1` | Base URL for OpenAI-compatible chat completions (no trailing slash). |
| `auth_token` | `""` | Optional Bearer token for authentication. |
| `model` | `qwen-3.8-27b` | Model identifier to request from the backend, sent verbatim (no validation, alias table, or allow-list). See [Model Recommendations](#model-recommendations). |
| `temperature` | `0.7` | Sampling temperature. |
| `top_p` | `0.9` | Nucleus sampling probability. |
| `frequency_penalty` | `0.0` | Repetition penalty. |
| `presence_penalty` | `0.0` | Topic freshness penalty. |
| `max_context_tokens` | `8192` | Maximum context tokens before compaction triggers. |
| `max_thinking_tokens` | `32768` | Maximum reasoning/thinking tokens before forcing continuation. Hard **cap**: it cuts a reasoning run that goes over. Aliases `reasoning_budget` / `thinking_budget` are accepted per specialist. |
| `reasoning_effort` | _unset_ | Reasoning-effort **dial** forwarded to the backend (`none`, `minimal`, `low`, `medium`, `high` are the documented vocabulary; the value itself is free-form pass-through). Applies to the Manager and to roles that do not override it. Unset, empty, or whitespace-only ⇒ the key is omitted from the request body entirely. See [Reasoning Effort](#reasoning-effort-reasoning_effort). |
| `preserve_thinking` | `true` | Retain thinking blocks in the session transcript. |
| `command_timeout_secs` | `60` | Timeout per terminal command or PTY invocation. Valid range `1..=300` seconds; an out-of-range value is logged and clamped into that range, never applied verbatim. |
| `sandbox_disabled` | `false` | Fail-closed opt-out of the Linux Landlock command sandbox: `true` runs commands unsandboxed and always logs an attributable warning. `MARMEL_DISABLE_SANDBOX=1` is an equivalent escape hatch — either source opts out. |
| `sandbox_extra_readonly_paths` | `[]` | TOML array of strings: paths the Landlock sandbox additionally allows **read + execute** for, beyond the built-in allow-list (which already covers `/usr`, `/bin`, `/lib`, `/lib64`, `/opt` and the package stores `/nix/store` and `/gnu/store`). For toolchain locations it does not cover — a non-default `store =` prefix in `nix.conf`, a store under another path, another read-only toolchain dir — e.g. `["/mnt/nix/store"]`. Entries must be **absolute**; relative, missing or already-built-in entries are skipped with a warning. Never grants write. Empty ⇒ built-in list only; a non-empty value **replaces** the list rather than appending (same merge semantics as `orchestration.mcp_servers`). See [Extra Sandbox Read-Only Paths](#extra-sandbox-read-only-paths-sandbox_extra_readonly_paths). |
| `max_repetition_threshold` | `5` | Accepted for compatibility and merged, but no longer read: the effective threshold is `[monitoring] repetition_threshold`. |
| `enable_xml_rescue` | `true` | Parse pseudo-XML tool calls into valid JSON tool calls. |
| `ui_mode` | `"tui"` | Interface mode: `"tui"` (Ratatui) or `"raw"` (streaming stdout). Raw mode is selected automatically whenever stdout is not a terminal — a pipe or a redirect — even with `"tui"` set. |
| `debug` | `false` | Not a `marmel.toml` key: it is absent from the deserializing mirror, so only the `--debug` flag sets it. Logs all LLM and tool traffic to `.marmel/debug.log`. |
| `session_enabled` | `true` | Capture the session bundle at all. `MARMEL_SESSION_DISABLE` can only turn it **off**; a disabled capture makes `--resume` a usage error (`exit 2`). |
| `session_resume` | `false` | Resume automatically on every launch. Overridden by `--resume` / `--fresh` and by `MARMEL_RESUME`. |
| `session_commit_min_interval_ms` | `250` | Floor between throttled commits (UI and worker-edge triggers only). Never delays turn boundaries, tool results, or ending commits. |
| `session_commit_on_tool_result` | `true` | Commit after each tool result. |
| `session_worker_mirror_max_messages` | `4000` | Cap on the mirrored conversation history stored per specialist. |
| `session_max_bundle_bytes` | `33554432` | Bundle size ceiling (32 MiB). |
| `system_prompt_path` | `prompts/system.md` | Replacement Manager prompt file; `~` is expanded and a relative path resolves against the current directory. |
| `base_prompt` | _unset_ | Extra text appended to the Manager prompt under `## Machine Specific Guidance`. |
| `[monitoring]` | — | Loop-breaker and stream-budget block: `enabled`, `repetition_threshold` (default `5`), `min_pattern_len` (`5`), `max_stream_tokens` (`32768`), `max_thinking_tokens` (`32768`). |
| `[orchestration]` | — | Top-level settings: `max_recursion_depth` (default `3`; `0` restores the default), `mcp_servers` (Manager-visible server names), `manager_module`. |
| `[orchestration.concurrency]` | _unset_ | Simultaneous-session cap per **backend URL + model** pair: `max_sessions` plus quoted per-URL overrides (e.g. `"http://host:8080/v1" = 2`). Unset ⇒ **unlimited** (nothing is metered); `0` ⇒ unlimited plus a warning. See [Session Concurrency](#session-concurrency-orchestrationconcurrency). |
| `[orchestration.specialists.<role>]` | — | Per-role overrides for `model`, `backend_url`, `auth_token`, `tools`, `mcp_servers`, `reasoning_effort`, and validator settings (`validator_model`, `validator_backend_url`, `validator_auth_token`, `validator_reasoning_effort`, `max_validator_iterations`, `max_fix_loop_rounds`, `enable_validator`). Top-level `max_fix_loop_rounds` is silently dropped — declare it per role (precedence: role, then `[orchestration.specialists.validator]`, then the built-in `200`). |
| Role ids | — | `coder`, `researcher`, `debugger`, `validator`, `generalist`, `planner` — the only names that resolve. `deepbrain` aliases `generalist`. These are also the `(role)` hints of a plan line; `validator` and `planner` deliverables are never validated, a role opt-out that outranks any `(validate)` directive. |
| `[mcp_servers.<name>]` | — | External MCP server registration (`command`, `args`, `env`, `url`): exactly one transport, and `command` wins if both `command` and `url` are set. Tools arrive named `<server>__<tool>`. No `enabled`, `timeout`, or `headers` key exists. |

### Example `marmel.toml`

```toml
backend_url = "http://localhost:11434/v1"
model = "qwen-3.8-27b"
max_context_tokens = 8192
ui_mode = "tui"

# Fail-closed sandbox opt-out (default false): true runs commands unsandboxed.
sandbox_disabled = false

# Simultaneous-session cap per backend URL + model pair (unset = unlimited).
[orchestration.concurrency]
max_sessions = 2
"http://192.168.50.162:8080/v1" = 1

# Optional external MCP servers
[mcp_servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]

# Specialist overrides
[orchestration.specialists.coder]
model = "qwen-3.8-27b"
backend_url = "http://localhost:11434/v1"
mcp_servers = ["fs"]
max_validator_iterations = 5
# Up to 5 audit passes by default; point the auditor at another endpoint to give it its own cap.

[orchestration.specialists.researcher]
model = "qwen-3.8-flash-next"
tools = ["read_file", "run_command", "grep_search", "glob"]
```

### Reasoning Effort (`reasoning_effort`)

`max_thinking_tokens` is a **ceiling**: it cuts off a reasoning run that overruns, it does
not tell the model how hard to think. `reasoning_effort` is the matching **dial** — an
optional hint sent as `reasoning_effort` in the chat-completions request body
(`ChatRequest::reasoning_effort` in `src/types/wire.rs`, serialized only when set). It can
be declared in three places:

- top level (`src/config.rs` → `Config::reasoning_effort`) — the Manager, plus every role
  that does not override it;
- per specialist (`[orchestration.specialists.<role>]` → `reasoning_effort`) — that role's
  own turns;
- per validator (`[orchestration.specialists.<role>]` → `validator_reasoning_effort`, the
  key sitting next to `validator_model`) — that role's validation passes.

```toml
# Top level: applies to the Manager and to roles that do not override it.
reasoning_effort = "medium"

# A strong model with high effort for coding.
[orchestration.specialists.coder]
model = "qwen-3.8-27b"
reasoning_effort = "high"

# A cheap model with low effort for research, and an even cheaper dial for the
# validator passes that audit it.
[orchestration.specialists.researcher]
model = "qwen-3.8-flash-next"
reasoning_effort = "low"
validator_model = "qwen-3.8-flash-next"
validator_reasoning_effort = "minimal"
```

**Precedence** — per request, first match wins (a blank/whitespace-only value counts as
unset and falls through to the next level):

| Request | Value used |
|---|---|
| Manager turn | top-level `reasoning_effort` |
| Specialist turn | `[orchestration.specialists.<role>].reasoning_effort`, else top level |
| Validator pass | `[orchestration.specialists.<role>].validator_reasoning_effort`, else top level — **never** the specialist's own value, which belongs to a different model |
| All of the above unset | key omitted from the request body (never sent as `null` or `""`) |

- **Free-form pass-through.** `none`, `minimal`, `low`, `medium`, and `high` are the
  documented vocabulary, but no value is validated: backends use different vocabularies, so
  anything else is forwarded verbatim. Trimming is the only normalization, and an empty or
  whitespace-only string is treated as unset.
- **Dial vs. ceiling.** `max_thinking_tokens` stays the hard cap and is read independently
  per turn; both knobs can be set at once. Selecting an effort level never derives a
  thinking budget — no effort level implies a `max_thinking_tokens` default.
- **Aliases.** Only inside `[orchestration.specialists.<role>]`: `max_thinking_tokens` also
  accepts `reasoning_budget` and `thinking_budget`, and `enable_validator` accepts
  `auto_validate` and `enable_validation`. Those spellings are not recognized at the top
  level, where an unrecognized key is dropped silently.
- **Cost.** Each task runs up to `max_validator_iterations` (default `5`) validator passes,
  each a full session against the validator model, so a low-effort validator multiplies the
  saving: it is billed five times per task, not once.
- **Not covered.** The internal fast calls — the steer arbitrator, the Agent Architect
  prompt builder, and the mid-turn notice path — stay thinking-off (`enable_thinking =
  false`) and never send `reasoning_effort`; this knob is for the Manager, specialist, and
  validator sessions only.

### Session Concurrency (`[orchestration.concurrency]`)

By default Marmel runs **as many LLM sessions at once as the work asks for**: every
`delegate_task` in a Manager round, every validator pass, the Manager's own turns, and the
JIT prompt architect all stream concurrently. That is the right shape for a backend with
real capacity. It is the wrong shape for one model on one host — a single gguf model behind
one server serves one or a few requests at a time, so ten parallel streams buy queueing,
KV-cache pressure, and 429s instead of throughput.

```toml
[orchestration.concurrency]
# Default cap for every backend URL + model pair.
max_sessions = 4

# Optional per-backend override, keyed by backend_url — the key MUST be quoted.
"http://192.168.50.162:8080/v1" = 2
"https://openrouter.ai/api/v1" = 8
```

- **Unset means unlimited.** Omit the table, or leave it empty, and nothing is metered:
  behaviour is exactly what it was before this key existed.
- **`max_sessions`** is the default cap for every pair. For a **local single-model
  backend, start at 1 or 2** — that is where a lone model on a lone host stops losing
  ground to queueing.
- **Per-URL overrides** are quoted keys inside the same table; a bare
  `http://192.168.50.162:8080/v1 = 2` is a TOML syntax error, never a silently dropped
  key. Trailing slashes and stray whitespace in a key are ignored, so `"…/v1"` and
  `"…/v1/"` are one and the same key — exactly as they are one backend on the wire. A key
  that is not a URL (`max_session = 4`, a typo) can never match a backend: it is reported
  as a warning and does nothing.
- **The cap is per backend URL + model, not per host.** Two models behind one host get
  the cap each; one model reached through two spellings of one URL shares a single cap.
- **`max_sessions = 0` is treated as unlimited** and logs a warning naming the key, the
  offending value, and the usable range (`1..=`). It is never clamped up to `1`, which
  would silently serialize traffic you asked to run in parallel.
- **Queueing is visible.** A session waiting for a slot is labelled **`(Queued n/m)`** in
  the **Subagents** panel: `n` is its position in the queue (1 = next in line), `m` how
  many sessions are queued for that pair including itself. Positions are strictly FIFO —
  a late arrival cannot barge past a session already in line. A queued session stays part
  of the running round (green name, live clock, live token count, and counted in the
  `[N active: …]` status line); cancelling it releases its place in line. Headless
  (`--raw`) output is unchanged.
- **The wait is bounded, separately from the stream timeout.** Queueing for a slot is
  capped at **900 seconds per backend call** (`QUEUE_WAIT_TIMEOUT_SECS`, fixed), which is
  a different clock from the **1200 second** per-stream read timeout
  (`OVERALL_READ_TIMEOUT_SECS`): the wait is spent before any stream exists, so no stream
  watchdog is armed yet, and queueing never eats the read budget of the call that finally
  runs. If the queue bound expires, **that delegation fails** with a distinguishable error
  (`ChatError::QueueTimeout`) naming the backend, the model, the cap, and the seconds
  waited — the request was never sent, and a queue timeout is never retried. Queueing does
  still consume the enclosing wall-clock bounds (the turn and fix-loop caps), because those
  measure real time rather than stream time.
- **Reserved slot for the internal fast calls.** The steer arbitrator and the notice
  evaluation draw on their **own** reserved slot (capacity 1 per pair) instead of queuing
  behind specialist work — a cap must never make steering unresponsive. The documented
  cost is that a pair can momentarily show **at most one extra in-flight stream**
  (`max_sessions + 1`), intentionally bounded. If that reserve is still busy after 20 s,
  the internal call proceeds un-metered with a warning (fail-open) rather than failing.

What draws on the cap, and what draws on the reserve:

| Call class | Metered against |
|---|---|
| Manager turn, specialist turn, validator / fix-loop pass, JIT prompt architect | The cap of the backend URL + model that call resolves to. |
| Steer arbitrator, notice evaluation | A separate reserved slot (1 per pair), fail-open after 20 s. |

Validator passes share the cap of the specialist they audit by default, because the
validator endpoint falls back to the same URL and model. Point `validator_backend_url` /
`validator_model` — or `[orchestration.specialists.validator]` — somewhere else and the
audit traffic gets its own cap, so a saturated writer endpoint cannot stall an auditor
aimed at another one.

---

## Usage & Interface

### CLI Flags

| Flag | Description |
|---|---|
| `--config <path>` | Path to an explicit configuration file. |
| `--raw` | Force headless stdout streaming mode (no TUI). |
| `--debug` | Log every LLM and tool exchange to `.marmel/debug.log`. The only way to set `debug` at all. |
| `--resume` | Rehydrate the previous run from `.marmel/session/`. A bundle that exists but is refused exits with code `3`; combining it with `--fresh`, or resuming while capture is disabled, is a usage error with exit `2`. |
| `--fresh` | Force a fresh start even when `session_resume` or `MARMEL_RESUME` is set — the escape hatch, and always the winner. |
| `-h`, `--help` | Show command-line help. |
| `[PROMPT]` | Optional initial prompt to immediately start the session. |

Model, backend URL, and auth token are configuration keys and environment variables, never flags. There is no `--model`, `--temperature`, `--no-tui`, `--version`, or short flag other than `-h`: the parser has a catch-all arm that turns the first unrecognized token into the session goal, so `marmel --model gpt-x "task"` silently runs with `--model` as the goal and `ignoring extra argument: gpt-x` on stderr.

### Interactive TUI

The rebuilt TUI (branch `feature/new-ui`) is a titled-pane layout: a **header** row, the **Transcript**, **Execution Plan** and **Workers** panes, a two-row **status bar**, the **composer**, and a one-row **key legend** in the footer. The full user guide — screen anatomy, view modes, focus, every binding, the small-terminal ladder, and rendered frames — is in [docs/new_ui.md](docs/new_ui.md).

- **Four view modes**, chosen with `F2`–`F5` or the pane toggles: `M1 Transcript`, `M2 Plan` (plan left 55 %, transcript right 45 %), `M3 Workers` (same split), `M4 Split` (transcript left 60 %, plan over workers right 40 %). `Ctrl+P` shows or hides the Plan pane, `Ctrl+A` the Workers pane. Booting with a non-empty `.marmel/execution_plan.md` opens the plan pane, and the first active worker opens the workers pane.
- **Progress is always on screen.** `✓ 3/7 done` and the task id in flight (`▶ t-004`) are drawn in the header at every size down to the 40×10 floor, and each pane counts its own contents on its bottom border (`✓ 3 done ⧗ 4 open ▶ in flight`, `▶ 1 ⏸ 1 ⧗ 1 ✓ 0`). A worker waiting for a concurrency slot reads `⧗ queued 2/3` — its queue position out of the sessions queued for that backend URL + model pair (see [Session Concurrency](#session-concurrency-orchestrationconcurrency)).
- **Focus is visible**: a `●` badge in the focused pane's title plus the focus border colour. `Tab` / `Shift+Tab` cycle Input → Transcript → Plan → Workers, skipping panes that are not drawn.
- **Nothing destructive runs on one keystroke**: `/reset`, `/exit`, `/cancel-all`, `x` and `Ctrl+D` stop at a confirm overlay that names the action and its consequence, answered by `y` / `n` / `Esc`.
- **Keyboard only.** Mouse capture is not enabled, so terminal-native selection and copy keep working; scroll with `PageUp` / `PageDown` / `Up` / `Down` / `Home` / `End`.
- **Degrades cleanly**: below 90 columns the split restacks, below 70 the header sheds optional chips, below 20 rows the status bar drops to one row and the footer legend goes, and below 40×10 a single "Terminal too small" notice is drawn.

### Keyboard Shortcuts

The complete 26-row table is the on-screen legend: press `?` or `F1`, or run `/help`. Core bindings:

| Shortcut | Action |
|---|---|
| `Enter` | Send message. |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+Enter` / `Ctrl+J` | Insert newline in multi-line prompt. |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo text in input box. |
| `Ctrl+Up` / `Ctrl+Down` | Browse input history. |
| `Tab` | Accept a slash-command completion, else cycle focus forward. `Shift+Tab` cycles backward. |
| `F2` | Cycle view mode (`M4 → M1 → M2 → M3 → M4`). |
| `F3` / `F4` / `F5` | Transcript / Plan / Workers view mode. |
| `Ctrl+P` / `Ctrl+A` | Show or hide the Plan / Workers pane. |
| `Ctrl+T` | Toggle thinking blocks (echoed by the `think:on` chip). |
| `Ctrl+F` | Toggle transcript follow (the `● LIVE` / `↓ 3 new` chip on the transcript border). |
| `?` / `F1` | Keymap and command overlay (`?` only with an empty composer). |
| `PageUp` / `PageDown` | Scroll the focused pane by 10 lines (the transcript when the composer holds focus). |
| `Up` / `Down` | History when the draft is empty, else scroll the focused pane by 1. |
| `←` / `→` | With Workers focused: previous / next worker. Otherwise: caret. |
| `Home` / `End` | Line start / end in the composer, else pane top / bottom. |
| `x` | Cancel the selected worker (opens the confirm overlay; Workers pane focused). |
| `e` | Print the last error again. |
| `y` / `n` | Confirm / dismiss the pending action — honoured only inside the confirm overlay. |
| `Ctrl+D` | Quit confirm overlay (never exits un-armed). |
| `Esc` / `Ctrl+C` | Close an overlay; else arm abort (`Abort armed. Press ESC again to abort.`); a second press within 3 s exits. Any printable key disarms the arm. |

`Ctrl+D` as an immediate exit, mouse scrolling/clicking, and the bare words `stop` / `cancel` / `exit` / `quit` / `q` / `abort` as commands are all gone: a bare word is ordinary steering text, and `?` prints what replaced each removed binding.

### Slash Commands

Type these directly into the chat input (they match case-insensitively, on line 1 only, and they exist only in the TUI — raw mode reads no input at all). Multi-line drafts are never parsed as commands, so a pasted block that starts with `/reset` arrives verbatim. The table below is the one the composer autocompletes from and `/help` prints — **22 spellings**, 20 `/`-prefixed plus the two vim spellings of the quit family (`:q` / `:q!`, accepted but not autocompleted, because command mode is `/`-only) — and every row is either answered by the TUI itself or routed through the confirm overlay: none of them is delivered to the model as a prompt. `/retry` is not a command (no retry action exists in the backend); an unknown spelling (`/xyz`, `/paln`) or a bad argument (`/plan maybe`, `/quit now`) is refused locally, with the accepted argument read off the table.

| Command | Action |
|---|---|
| `/help` | Print the key legend and this command table in the transcript (the same body `F1` draws). |
| `/thought` | Toggle display of reasoning and thinking content (the `think:` chip, `Ctrl+T`). |
| `/follow [on\|off]` | Toggle transcript follow (the `● LIVE` / `↓ N new` chip on the transcript border, `Ctrl+F`). |
| `/plan [on\|off]` | Show or hide the Plan pane (the `plan:` chip, `Ctrl+P`). |
| `/workers [on\|off]` | Show or hide the Workers pane (the `workers:` chip, `Ctrl+A`). |
| `/mode transcript\|plan\|workers\|split` | Set the view mode (`F3`–`F5`); a bare `/mode` prints the current mode and the arguments it takes. |
| `/error` | Print the sticky last error again (the `e` key). |
| `/cancel <worker key or task id>` | Cancel one worker (confirm). |
| `/cancel` | Selected worker when the Workers pane is focused; otherwise a quit spelling (confirm). |
| `/cancel-all` | Cancel every running worker (confirm). |
| `/reset` (`/reset_plan`, `/reset-plan`, `/clear_plan`, `/clear-plan`, `/reset_execution_plan`) | Clear the execution plan, session bundle, steering history and transcript, and tell the model (confirm). |
| `/exit` (`/quit`, `/q`, `/abort`, `/stop`, and the vim spellings `:q` / `:q!`) | Quit the session, cancel running workers, and stamp a `userAbort` ending on the bundle (confirm). |

Command semantics, refusal wording and the autocomplete rules: [docs/new_ui.md §6](docs/new_ui.md).

### Headless Raw Mode

Marmel runs in raw mode when invoked with `--raw`, when `ui_mode` is `"raw"`, or whenever stdout is not a terminal — a pipe or a redirect — and then outputs structured, pipe-friendly events:

```bash
marmel --raw "Summarize the repository structure"
```

Output format:
```text
[assistant] Analyzing workspace structure...
[tool] glob(pattern="*")
[tool-result] Cargo.toml, src/, tests/
[assistant] The project is a standard Rust application...
[done]
```

- **No input, no commands.** Raw mode never reads stdin: `marmel --raw` with no `[PROMPT]` exits `0` immediately, and slash commands, keyboard shortcuts, and mid-flight steering are unreachable.
- **Full event vocabulary.** Besides the tags above there are `[steer]`, `[thinking]`, `[status]`, `<agent_tag>:content`, `<agent_tag>:thinking`, and `[delegation] STARTED → …` / `DONE    …` / `FAILED  …`. Token counters and queue-state transitions are silent, so headless output is byte-stable.
- **Logs go to stderr.** In raw mode `tracing` writes to stderr instead of the rotating `.marmel/marmel.log`.

### Runtime Directory (`.marmel/`)

Internal session files are stored in `.marmel/` within the workspace root:

| File / Folder | Purpose |
|---|---|
| `.marmel/execution_plan.md` | Active execution plan with checked/unchecked tasks. |
| `.marmel/execution_plan_archive.md` | Copy of the latest archived plan, kept alongside the timestamped snapshot. |
| `.marmel/prompts/` | Synthesized agent prompts and tool grants (`<task_id>.md`). |
| `.marmel/prompts/<task_id>-validation.md` | Auditor brief for the validator pass of that task. It is authored for every **planned** task whose plan line carries `(validate)` or **no** directive — the directive alone decides, and the wording of the task line or of the brief decides nothing. The `validator` and `planner` roles are role-level opt-outs and are never briefed, whatever their line says, and an ad-hoc subtask delegated by the Steer Arbitrator is never briefed either: it has no plan line and is not audited (see the next row). A planned task with `(validate)` or no directive still needs a readable copy of this file — when it is missing, blank or unreadable the run fails closed, the deliverable is reported as not validated and its plan line stays unchecked. The exemption is a task carrying a `-validation-skip.md` record, and it explains only a brief that was never written — a blank or unreadable brief is never exempted. |
| `.marmel/prompts/<task_id>-validation-skip.md` | The record that this task needs **no** validation, written in place of the auditor brief. Two producers exist, and the one line the file holds names which one authored it: a plan line carrying the explicit `(no-validate)` directive writes `Plan directive (no-validate): validation not required`, and an ad-hoc subtask delegated through the Steer Arbitrator writes `Ad-hoc delegated task (no plan line): validation not required` — such a task has no plan line at all, so the plan side never took a validation decision for it, and the steering path records that default before the run begins. Arbitrator-created ad-hoc tasks therefore need no auditor brief and are not validated. Planned tasks keep their contract untouched: a `(validate)` or directive-free line still requires a readable `<task_id>-validation.md`, and it still fails closed — deliverable reported as not validated, plan line left unchecked — when that brief is missing, blank or unreadable. The exemption never crosses provenance: an ad-hoc id that collides with a real plan-line task id gets **no** ad-hoc record, so the plan's own decision stays authoritative, and the `validator` / `planner` role-level opt-outs get no record at all. The two files are mutually exclusive: a `(validate)` or directive-free line deletes a stale record **before** the brief is attempted, so a brief write that fails leaves no record behind (and is logged) and its missing brief stays a fail-closed verdict gap instead of a declared skip. For an exempted task no validator runs and no fix loop fires: the task completes normally with `MISSION COMPLETE (<task-id>)`, its plan line is checked off — an ad-hoc id has no plan line to check — and the worker panel reads `Completed (validation skipped by design)`. Fail-closed is preserved when neither file exists, when the record is blank, when the task id is rejected, or when the stale-brief delete or the record write itself fails; the per-role switches (`enable_validator` / `auto_validate`, `max_validator_iterations = 0`) remain the operator-level opt-out, and they outrank both producers. |
| `.marmel/marmel.log` | Rotating application log for TUI mode (5 MiB, 3 backups). Raw mode logs to stderr instead. |
| `.marmel/debug.log` | LLM and tool traffic, written only under `--debug`. |
| `.marmel/archive/` | Archived completed execution plans. |
| `.marmel/session/` | The **session bundle**: the split-file snapshot that `--resume` reads back (see [Session Restore](docs/session_restore.md)). It is a directory, never a single file. |
| `.marmel/session/manifest.json` | The bundle's root document and **commit point**: format/schema versions, session id, wall/monotonic anchors, how the previous process ended, the identity floors, and the member list with each member's byte length and fingerprint. It is written **LAST** — that rename *is* the commit — and it is the only file that names the member set. |
| `.marmel/session/manager.json` | Manager member: the message history (pinned system prompt and goal preserved byte-for-byte), the pin witness, and the engine scalars. |
| `.marmel/session/ui.json` | UI member: pane text (chat, subagents, plan panel), status line, composer text + caret + history, scroll / selection / focus, subagent panels, and the token witnesses. |
| `.marmel/session/workers/<key>.json` | One member per subagent, named after its registry key with escaping (e.g. `coder-t-018%237.json`): full mirrored conversation history, status, counters, and pending/dangling tool-call state. |
| `.marmel/session/.<member>.tmp.<pid>.<nanos>.<seq>` | Staging name every atomic member write goes through: written there, then renamed into place. A leftover from another process is deleted by the commit sweep; one carrying this process's own id belongs to a write in flight and is left alone. |
| `.marmel/forced_phase.txt` | **LEGACY.** Nothing reads or writes it; a stale copy is deleted by `/reset`. |
| `.marmel/.session_transcript.json` | **LEGACY.** Written by older releases only; never written again, never merged or upgraded into a bundle. It is named in the "legacy journals found, not restorable" report on a fresh boot, and swept by `/reset`, `Plan::clear()` and `Plan::archive()`. |
| `.marmel/.ui_transcript.json` | **LEGACY.** Same handling as `.marmel/.session_transcript.json`: never written again, only named on a fresh boot, and deleted with the plan by `/reset`, `Plan::clear()` and `Plan::archive()`. |

---

## Real-Time Steering & Live Dialogue

Marmel allows you to interact with the assistant at any time—even while subagents are running commands or streaming output. The chat input of the TUI is the only steering entry point (raw mode reads no input at all), and anything you type there that is not one of the slash commands listed above goes to the Steer Arbitrator while a stream or worker is active; when nothing is running it is queued into the Manager's context as a plain user message:

- **Immediate Pausing:** A line typed mid-flight is arbitrated on the spot, interrupting the live model call at its next chunk boundary. The arbitrator borrows the model slot: streams on its own model are paused, then resumed with the partial output carried over — unless the decision cancels them. A stream on a different model keeps running, and one that has not acknowledged the pause within 2 seconds — typically a worker blocked inside a long tool call — is cancelled rather than waited for. The arbitrator's own call never queues behind specialist work: it draws one reserved slot per backend + model pair and proceeds un-metered with a warning if that slot stays busy for 20 seconds.
- **Direct Answers & Status:** Ask questions (e.g. *"What are you currently doing?"* or *"Why SQLite?"*) to get instant answers without disrupting the agent's work.
- **Steering Decisions:** The arbitrator can answer directly (`RespondDirectly`), queue the instruction (`QueueAndContinue`), forward it to a worker (`ForwardToWorker`), approve or reject the plan (`ApprovePlan` / `RejectPlan`), delegate a fresh subtask (`DelegateTask`), sleep and re-evaluate (`Sleep`, capped at 5 arbitration rounds and a 300-second cumulative sleep budget), or abort the turn outright (`AbortImmediately`). A per-subtask action outside `ForwardNotice` / `Cancel` / `DelegateTask` / `Sleep` is rejected with a warning and never silently defaulted, a decision outside the list above falls through to the queue-or-answer fallback, and the `SwitchTier` / `SwitchModel` fields are accepted by the JSON shape but map to no action.
- **Course Correction:** Provide feedback, cancel one running subtask mid-flight (`Cancel` fires that worker's cancellation token and aborts its stream), or have the turn aborted outright (`AbortImmediately` / `RejectPlan` cancel every active worker). No steering decision rewrites the plan — an instruction aimed at the plan reaches the Manager as a user message at the next turn boundary, and it is the Manager that rewrites `.marmel/execution_plan.md` from there.
- **Direct Dialogue with Workers:** If your inquiry concerns an active subagent's internal reasoning, the arbitrator posts a numbered notice (`notice-1`, `notice-2`, …) addressed to that one worker and resumes the stream straight away — your answer is not awaited before the work continues. The worker reads the notice at the start of its next turn or right after its current tool round, so a notice posted mid-turn reaches it inside that same turn. The notice for a `ForwardNotice` steering decision is filed exactly once, by the single posting owner `arbiter::post_forward_notices`: the host renders what that posting returned and never posts again, so no second copy of your raw instruction sits pending next to the arbitrator's rewritten inquiry. The notice is addressed by the worker's effective registry key — the token the arbitrator is shown as `Address This Worker With` in its `Active Subtasks` block — and a target that resolves to no routable worker key (`NoticeTargeting::Unroutable`) is refused visibly instead of being filed into an inbox nobody drains. The arbitrator's follow-up question is filed under that same worker key, so round two reaches the worker that was addressed in round one rather than any other worker of the same role.
- **Notice Ids & Reply Contract:** The only legal `notice_id` is the literal value the delivered notice shows on its `ID: <notice-id>` line, and the worker answers by naming exactly that id with `reply_to_arbitrator`. A reply naming any other id — an omitted or empty one, one no pending notice carries, an already-answered one, or one addressed to another worker — comes back to it as a tool error naming its rejection class, and exactly four such classes exist: `missing-notice-id`, `unknown-notice-id`, `already-replied`, `worker-mismatch`. A rejected reply records nothing, resolves nothing, and spends no arbitrator turn. The reply is keyed to the same effective worker key the notice was addressed with (`active_worker_key_for_reply`, the identity behind `reply_worker_identity()`), so the reply identity and the notice addressing key cannot disagree. A valid reply is evaluated by the arbitrator, which either synthesizes the user-facing answer or posts a follow-up notice under the worker key of the notice it answers. The reply classes are distinct from the delivery bounds: a notice stays deliverable for 10 minutes, and one past that window is simply no longer pending, so a reply naming it is refused as `unknown-notice-id` rather than by a class of its own. One worker inbox holds 64 undelivered notices under a drop-oldest bound (the drop is reported, never silent), at most 256 worker inboxes are retained, and each notice accepts one worker answer.

---

## Agent Archetypes & Customization

### Built-In Archetypes

Marmel comes with 6 built-in archetypes preconfigured in the binary, and they are the only delegatable roles: `delegate_task` resolves exactly the ids of the six rows below, with `deepbrain` accepted as an alias for `generalist`. You do not need to create any configuration files to use them:

| Archetype | Description | Default Tools |
|---|---|---|
| **Coder** | System architecture, implementation, refactoring, test suites. | `delegate_task`, `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `rebirth`, `sleep`, `reply_to_arbitrator` |
| **Researcher** | Codebase reconnaissance, documentation inspection, API contracts. | `delegate_task`, `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `rebirth`, `sleep`, `reply_to_arbitrator` |
| **Debugger** | Crash forensics, regression isolation, minimal fixes, interactive PTY. | `delegate_task`, `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `pty_*`, `rebirth`, `sleep`, `reply_to_arbitrator` |
| **Validator** | Independent auditor; runs test suites and issues formal verdicts. | `delegate_task`, `read_file`, `grep_search`, `glob`, `run_command`, `pty_*`, `leave_verdict`, `rebirth`, `sleep`, `reply_to_arbitrator` — no `write_file` or `replace` |
| **Generalist** | Polymath for cross-cutting logic and multi-disciplinary tasks. | `*` (every tool, `delegate_task` included) |
| **Planner** | Mission architect for workspace reconnaissance and phased planning. | `read_file`, `grep_search`, `glob`, `create_plan`, `rebirth`, `sleep`, `reply_to_arbitrator` |

> [!NOTE]
> The Manager is restricted to planning (`create_plan`), reading (`read_file`, `grep_search`, `glob`), and delegation (`delegate_task`). It cannot directly modify files (`write_file`, `replace`) or execute commands (`run_command`).

Two rules are enforced ahead of any allowlist: `leave_verdict` is Validator-only, and `create_plan` executes only through the Manager — the same call routed through a specialist is refused even when its allowlist or a synthesized blueprint names it. `reply_to_arbitrator` is granted to every specialist unconditionally. A specialist can delegate at all only because `delegate_task` sits in its own allowlist — every built-in except the **Planner** carries it — and only the **Coder** and **Generalist** declare recursion; nested delegation stays bounded by `max_recursion_depth`. A plan line's `(validate)` / `(no-validate)` directive changes only whether that task's deliverable is audited — never which role receives the task — and the **Validator** and **Planner** rows are exempt from validation by role, so no directive buys their audit back.

### Custom Archetypes (`AGENTS.md`)

To adjust what the built-in roles advertise, add an `AGENTS.md` file to your workspace root (or globally at `~/.marmel/AGENTS.md`). A section whose heading maps onto a built-in id overrides that role's default skills and tools; a new heading is surfaced to the planner as guidance but cannot itself be delegated to, because `delegate_task` resolves only the six built-in ids:

```markdown
## SecurityAuditor
description: Vulnerability scanning and security review.
skills: verification, research
tools: read_file, grep_search, glob, run_command, leave_verdict, reply_to_arbitrator
```

Only `description:`, `skills:`, and `tools:` are read; every other `key:` line and all free text are ignored. `##` and `###` headings both work, and the role id is the heading lowercased with spaces and hyphens turned into underscores — so the section above registers as `securityauditor`, which matches no built-in id and is therefore planner guidance only. `~/.marmel/AGENTS.md` is read first and `<workspace>/AGENTS.md` second, both keyed by id, so the project file wins on a collision.

### Custom Skills (`skills/`)

Skills provide reusable domain knowledge and automatically grant suggested tools. A skill is a top-level `*.md` file or a subdirectory holding a `SKILL.md` — the file stem or directory name becomes its id, and a top-level `README.md` is skipped — inside `<workspace>/skills/` or `~/.marmel/skills/`. Registration runs built-ins first, then `~/.marmel/skills/`, then `<workspace>/skills/`, always keyed by id, so a project skill shadows a user skill, which shadows a built-in:

```markdown
---
name: SQL Migrations
description: Best practices for database schema migrations and rollback safety.
tools:
  - run_command
  - read_file
  - write_file
---

### Database Migration Rules
- Ensure migrations are wrapped in transactions where supported.
- Provide a corresponding rollback script for every forward migration.
- Verify migration status with the CLI before proceeding.
```

Only the `id`, `name`, `description`, and `suggested_tools` (or `tools`) front matter keys are read; any other key is ignored, and the body below the front matter is the content that gets injected. The compiled-in skills are `clean_code`, `debugging`, `research`, `verification`, and `testing`.

When a matching task is delegated, the skill content is injected into the specialist's prompt and its `tools` widen the allowlist for that run; `rebirth` is always appended, and neither a skill nor a synthesized blueprint can widen the verdict class — `leave_verdict` stays Validator-only.

---

## Security & Sandboxing

Marmel is designed to prevent accidental modifications outside the target project:

1. **Path Confinement:** All file tools (`read_file`, `write_file`, `replace`, `grep_search`, `glob`) validate paths against the workspace root. Path traversal attempts (e.g. `../../etc/passwd` or `~/.ssh`) are blocked.
2. **Process Sandboxing (Linux Landlock LSM):** Terminal commands (`run_command` and PTY sessions) execute inside an unprivileged Landlock sandbox on Linux. Read/write access is restricted to the workspace, temporary build directories (`/tmp`, `/var/tmp`), and language caches (`~/.cargo`, `~/.cache`, `~/.npm`). Read-only grants additionally cover `~/.rustup`, `~/.config`, `~/.local`, and `~/.gitconfig`, and the device nodes `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/tty`, `/dev/urandom`, `/dev/random`, `/dev/pts`, and `/dev/shm` are read/write; the FHS prefixes are read-only (see [Extra Sandbox Read-Only Paths](#extra-sandbox-read-only-paths-sandbox_extra_readonly_paths)). A `CARGO_HOME` or `RUSTUP_HOME` set outside `~/.cargo` / `~/.rustup` is granted the matching read/write or read-only rule. Access to sensitive directories such as `~/.ssh` and `~/.gnupg` is denied by the kernel.
3. **Non-Linux Platforms:** On macOS and Windows, path confinement and working directory boundaries are actively enforced in-process.

**Sandbox Opt-Out (fail-closed).** The sandbox is always on unless you explicitly opt out: the typed knob `sandbox_disabled` defaults to `false` in `marmel.toml`, and the equivalent environment escape hatch is `MARMEL_DISABLE_SANDBOX`. Either source opts out on its own, and every honoured opt-out is announced with an attributable warning naming the setting that was set (`warning: Landlock sandbox DISABLED by <setting>: this command runs WITHOUT Landlock.`). The parsing is fail-closed — only `1`, `true`, `yes` or `on` (case-insensitive, surrounding blanks ignored) disable the sandbox, while a malformed or falsy value such as `0`, `false`, `off`, `no`, an empty value, or a typo leaves it **enabled**. Opting out is also never a way to make a failed sandbox non-fatal: if Landlock cannot be established, the command is refused instead of running unsandboxed — the sandbox re-entry step prints the reason to stderr and exits with `SANDBOX_REFUSE_EXIT_CODE` (`73`, `sysexits.h` `EX_UNAVAILABLE`) without exec-ing the command.

### Extra Sandbox Read-Only Paths (`sandbox_extra_readonly_paths`)

The read-only part of the ruleset covers the FHS toolchain prefixes (`/usr`, `/bin`, `/lib`, `/lib64`, `/opt`, `/etc`, `/dev`, `/proc`, `/sys`, `/run`, `/var`) **plus the package stores `/nix/store` and `/gnu/store`** — the stores are named explicitly because Landlock resolves symlinks before it matches a rule, and on NixOS or Guix `/bin`, `/usr/bin` and `/lib` are symlink farms whose real targets live in the store. So sandboxed `execve` works out of the box there and nothing has to be configured. Set this key only when your executables resolve somewhere the built-in list does not reach: a custom `store = …` prefix in `nix.conf`, a Guix store under another path, or any other read-only toolchain directory — that is preferable to opting out of the sandbox altogether.

```toml
# nix.conf says `store = /mnt/nix/store`, so /nix/store is not where the binaries live:
sandbox_extra_readonly_paths = ["/mnt/nix/store"]
```

Entries must be **absolute**, and each one joins exactly the **read + execute** grant of the prefixes above: it never grants write and it never weakens the workspace rule or the fail-closed behaviour. An entry that is relative, does not exist on this host, or duplicates a built-in path never becomes a rule and is reported in one line (`warning: ignoring sandbox_extra_readonly_paths entry "./tools": not absolute` — likewise `does not exist` and `already covered by the built-in allow-list`). A refusal is still loud and now names the image it could not reach: `error: failed to exec /nix/store/<hash>-bash/bin/bash under the Landlock sandbox: Permission denied (os error 13). The path is not covered by the sandbox allow-list.`

*Advanced/internal:* the configured list travels to the sandbox re-entry child (which loads no config file) as the `:`-separated environment variable `MARMEL_SANDBOX_EXTRA_RO_PATHS`. It is an internal hand-off, not a supported knob — use the config key; a path containing `:` cannot be transported intact (it arrives split into two entries).

---

## Testing

Run the test suite:

```bash
cargo test
```

The tests live in two places: integration tests under `tests/` (`test_orchestrator.rs`, `test_role_gating.rs`, `test_validation_loop.rs`, `test_dynamic_prompts.rs`, `test_llm_pool.rs`, `test_session_boot.rs`, and the `test_session_restore_*.rs` family) and unit tests beside the code they cover — modules named `*_tests.rs` (`src/harness/fs_tests.rs`, `src/harness/pty_tests.rs`, `src/manager/phase_tests.rs`, `src/orchestrator/steer_tests.rs`, `src/mcp/http_tests.rs`) and inline `#[cfg(test)]` modules inside files such as `src/plan_parse.rs` and `src/agents/prompt_builder.rs`.

The test suite covers:
- Core orchestration and recursive delegation bounds.
- Tool allowlists, role gating, and path confinement.
- Stream preemption and steer arbitration.
- Repetition breakers and XML tool call rescue.
- Per-backend session concurrency caps, queue accounting, and queue timeouts.
- Session bundle commit, boot, abort-flush, and restore ladder behaviour, including refused restores (exit `3`).
- Validator and fix-loop verdict bounds, including the auditor that is reminded three times and then fails closed without a verdict.
- The plan-line validation directive end to end: `(no-validate)` writes `<task_id>-validation-skip.md` and no auditor brief, `(validate)` or a directive-free line writes the brief and clears the stale counterpart, a blank or unrecognised token stays no directive at all (`src/plan_parse.rs`), a durable record lets the task complete and check its box off, the ad-hoc steering default records its own provenance line for an id with no plan line while an id that **is** a plan line gets no record at all, and neither record on disk still fails closed with the plan line unchecked (`tests/test_dynamic_prompts.rs`, `tests/test_validation_loop.rs`).
- Internal fast calls and reserved concurrency slots.

To verify linting and formatting:

```bash
cargo clippy --all-targets --all-features
cargo fmt -- --check
```

---

## License

This project is licensed under the [MIT License](LICENSE).
