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
  - [Terminal Commands (`run_command` & PTY)](#terminal-commands-run_command--pty)
  - [Runtime Directory (`.marmel/`)](#runtime-directory-marmel)
- [Real-Time Steering & Live Dialogue](#real-time-steering--live-dialogue)
- [Agent Archetypes & Customization](#agent-archetypes--customization)
  - [Built-In Archetypes](#built-in-archetypes)
  - [Custom Archetypes (`ARCHETYPES.md`)](#custom-archetypes-archetypesmd)
  - [Project Instructions (`AGENTS.md`)](#project-instructions-agentsmd)
  - [Custom Skills (`skills/`)](#custom-skills-skills)
- [Offline Reference Search (zim-search + kiwix MCP)](#offline-reference-search-zim-search--kiwix-mcp)
  - [Build, Index, Register](#build-index-register)
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
- **Model Context Protocol (MCP):** Connect external tool servers via stdio or HTTP/SSE using standard MCP configurations — including the bundled `zim-search` server, which serves offline reference corpora (see [Offline Reference Search](#offline-reference-search-zim-search--kiwix-mcp)).
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
      │    (skills/*.md, ARCHETYPES.md)   │                       │
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
| `command_timeout_secs` | `60` | Timeout for a single `run_command` invocation; the interactive PTY tools are not bounded by it. Valid range `1..=300` seconds; an out-of-range value is logged and clamped into that range, never applied verbatim. |
| `sandbox_disabled` | `false` | Fail-closed opt-out of the Linux Landlock command sandbox: `true` runs commands unsandboxed and always logs an attributable warning. `MARMEL_DISABLE_SANDBOX=1` is an equivalent escape hatch — either source opts out. |
| `sandbox_extra_readonly_paths` | `[]` | TOML array of strings: paths the Landlock sandbox additionally allows **read + execute** for, beyond the built-in allow-list (which already covers the FHS prefixes `/usr`, `/bin`, `/lib`, `/lib64`, `/opt`, `/etc`, `/dev`, `/proc`, `/sys`, `/run`, `/var` and the package stores `/nix/store` and `/gnu/store`). For toolchain locations it does not cover — a non-default `store =` prefix in `nix.conf`, a store under another path, another read-only toolchain dir — e.g. `["/mnt/nix/store"]`. Entries must be **absolute**; relative, missing or already-built-in entries are skipped with a warning. Never grants write. Empty ⇒ built-in list only; a non-empty value **replaces** the list rather than appending (same merge semantics as `orchestration.mcp_servers`). See [Extra Sandbox Read-Only Paths](#extra-sandbox-read-only-paths-sandbox_extra_readonly_paths). |
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
| `git_workflow_policy` | `"auto"` | Closed string-enum for the git-workflow prompt behaviour, validated when the config file is read: `"auto"` autodetects the workspace git state and injects a `## Git workspace state` block into the Manager prompt and every specialist prompt, `"off"` suppresses that block, `"enforce_worktree"` states isolated `git worktree` use as a requirement instead of a preference. Git guidance in the role prompts and in the built-in `git_workflow` skill is never gated on it. See [Git Workflow & Worktrees](#git-workflow--worktrees). |
| `[monitoring]` | — | Loop-breaker and stream-budget block: `enabled`, `repetition_threshold` (default `5`), `min_pattern_len` (`5`), `max_stream_tokens` (`32768`), `max_thinking_tokens` (`32768`). |
| `[orchestration]` | — | Top-level settings: `max_recursion_depth` (default `3`; `0` restores the default), `mcp_servers` (Manager-visible server names), `manager_module`. |
| `[orchestration.concurrency]` | _unset_ | Simultaneous-session cap per **backend URL + model** pair: `max_sessions` plus quoted per-URL overrides (e.g. `"http://host:8080/v1" = 2`). Unset ⇒ **unlimited** (nothing is metered); `0` ⇒ unlimited plus a warning. See [Session Concurrency](#session-concurrency-orchestrationconcurrency). |
| `[orchestration.specialists.<role>]` | — | Per-role overrides for `model`, `backend_url`, `auth_token`, `tools`, `mcp_servers`, `reasoning_effort`, `max_thinking_tokens` (aliases `reasoning_budget` / `thinking_budget`), an optional `prompt` (aliases `instructions` / `system_prompt` / `base_prompt`) appended verbatim to that role's system prompt when non-blank, and validator settings (`validator_model`, `validator_backend_url`, `validator_auth_token`, `validator_reasoning_effort`, `max_validator_iterations`, `max_fix_loop_rounds`, `enable_validator`). A `module` key is accepted but informational only — the role → module paths are hard-coded in the registry, so the value is never read at runtime. Top-level `max_fix_loop_rounds` is silently dropped — declare it per role (precedence: role, then `[orchestration.specialists.validator]`, then the built-in `200`). |
| Role ids | — | `coder`, `researcher`, `debugger`, `validator`, `generalist`, `planner` — the only names that resolve. `deepbrain` resolves as a `(role)` hint / `delegate_task` role name to `generalist`, but it is **not** a config section key: a `[orchestration.specialists.<role>]` block is matched by the canonical role name only, so `[orchestration.specialists.deepbrain]` is never read. These are also the `(role)` hints of a plan line; `validator` and `planner` deliverables are never validated, a role opt-out that outranks any `(validate)` directive. |
| `[mcp_servers.<name>]` | — | External MCP server registration (`command`, `args`, `env`, `url` and the optional `prompt`, aliases `instructions` / `system_prompt`: free-form text injected into the system prompt of every agent whose allowlist names that server): exactly one transport, and `command` wins if both `command` and `url` are set. Tools arrive named `<server>__<tool>`. No `enabled`, `timeout`, or `headers` key exists. |

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

### Git Workflow & Worktrees

Marmel's agents work on branches and commit their work, and `git_workflow_policy` decides how
much git context they are shown — a closed, parse-time-validated set of three values:

- **`auto`** (default) — the workspace git state is read from disk and a `## Git workspace state`
  block is appended to the Manager prompt and to every specialist prompt: repository root,
  current branch and linked worktrees when the workspace is already a git repo, or — when it is
  not — the instruction to `git init`, write a `.gitignore` and make an initial commit before
  real work starts.
- **`off`** — no autodetection, and no injected block anywhere.
- **`enforce_worktree`** — the same block, but isolated worktree use is stated as a requirement
  for the session rather than as a preference.

Git worktrees are created under the gitignored `.worktrees/<branch>` directory **inside** the
workspace root, because the sandbox denies writes outside it — the usual `git worktree add
../<branch>` sibling layout is refused — and every git call then addresses the worktree with
`git -C <path>`. Agents commit small and often, staging explicit paths with conventional
message prefixes, and `.marmel/` is never committed; the built-in `git_workflow` skill carries
the full procedure.

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

The rebuilt TUI (branch `feature/new-ui`) is a titled-pane layout: a **header** row, the **Execution Plan**, **Transcript** and **Workers** panes, a two-row **status bar**, the **composer**, and a one-row **key legend** in the footer. Screen anatomy, view modes, focus, every binding, the small-terminal ladder, and the rendered frames are defined by the code itself: the pane geometry lives in `src/ui/tui/layout.rs`, the key handling in `src/ui/tui/events.rs`, and the authoritative binding legend is generated by `legend()` in `src/ui/tui/components/help.rs`.

- **Four view modes**, chosen with `F2`–`F5` or the pane toggles: `M1 Transcript`, `M2 Plan` (transcript in the leading column 55 %, plan trailing 45 %), `M3 Workers` (same split, roster trailing 45 %), `M4 Split` (transcript in the wide leading column 60 %, plan at the head of the trailing side column over workers, 40 %). Wherever the transcript shares the frame with the plan and/or the workers, the transcript holds the **leading** slot and the side panes the **trailing** one — the left column of a side-by-side split, the top row when the frame stacks, the wide column when the side column cannot stack — in every mode and at every size. `Ctrl+P` shows or hides the Plan pane, `Ctrl+A` the Workers pane. Booting with a non-empty `.marmel/execution_plan.md` opens the plan pane, and the first active worker opens the workers pane.
- **Progress is always on screen.** `✓ 3/7 done` and the task id in flight (`▶ t-004`) are drawn in the header at every size down to the 40×10 floor, and each pane counts its own contents on its bottom border (`✓ 3 done ⧗ 4 open ▶ in flight`, `▶ 1 ⏸ 1 ⧗ 1 ✓ 0`). A worker waiting for a concurrency slot reads `⧗ queued 2/3` — its queue position out of the sessions queued for that backend URL + model pair (see [Session Concurrency](#session-concurrency-orchestrationconcurrency)).
- **Focus is visible**: a `●` badge in the focused pane's title plus the focus border colour. `Tab` / `Shift+Tab` cycle Input → Transcript → Plan → Workers, skipping panes that are not drawn.
- **Nothing destructive runs on one keystroke**: `/reset`, `/exit`, `/cancel-all`, `Ctrl+X` and `Ctrl+D` stop at a confirm overlay that names the action and its consequence, answered by `y` / `n` / `Esc`.
- **Keyboard only.** Mouse capture is not enabled, so terminal-native selection and copy keep working. Scrolling is split per viewport: the transcript body and the plan body take `Up` / `Down` (one line), `PageUp` / `PageDown` (ten lines) and `Home` / `End` for the ends, while in the Workers pane plain `Up` / `Down` scroll the selected worker's **detail log** and `Shift+Up` / `Shift+Down` walk the **roster window** — plain `Up` / `Down` walk the roster instead only where no detail pane is drawn (frame narrower than the 60-column `DETAIL_COLS` floor, `workers_detail_present` in `src/ui/tui/layout.rs`) — and `PageUp` / `PageDown` / `Home` / `End` scroll the selected worker's **detail log**.
- **One key, one owner: focus decides it.** The keys that could belong to either the composer or a pane — `Up` / `Down` / `PageUp` / `PageDown` / `Home` / `End` — go to the composer **iff** the composer holds focus (`composer_has_focus` in `src/ui/tui/state.rs`) and to a pane viewport otherwise; with the composer focused, `PageUp` / `PageDown` have no owner and do nothing at all. Input history is `Ctrl+Up` / `Ctrl+Down` from **any** focus, and that chord is the **only** history binding there is: plain `Up` / `Down` never recall an entry — with composer focus they move the caret inside a multi-line draft, and where the caret has nowhere to go the press is not claimed at all (no scroll, no repaint, no follow change), while with a pane focused they step that pane by one line. The shape of the draft never decides who gets the key.
- **Follow never lags the viewport.** The transcript's own border carries the chip: `● LIVE` while following, `↓ N new` for lines below the viewport once you have scrolled away, `↓ follow off` when follow is off and nothing is unseen, plus `scroll <offset>/<max>` whenever there is somewhere to scroll. A scroll that actually moved away from the tail turns follow off; only a scroll that **arrived** back on the tail turns it on; a press with nowhere to go — and a pane with nothing to scroll — states nothing and repaints nothing. Submitting a message never re-arms follow; `Ctrl+F` or `/follow on` does.
- **Colour is a platform question, not only a `TERM` question.** `NO_COLOR` wins everywhere — any non-empty value turns colour off — and `TERM=dumb` degrades to monochrome whatever else is set. An absent `TERM` means "unlabelled": on Linux and macOS that still means monochrome, exactly as before, while on Windows `TERM` is simply never set (neither `cmd.exe` nor Windows Terminal exports it), so the attached console answers instead — the palette is used when stdout is a console, and monochrome when it is redirected. Monochrome loses no meaning: the foreground and background colours go, while bold, italic, the glyphs and the words that carry the same signal stay.
- **Degrades cleanly**: below 90 columns the split restacks, below 70 the header sheds optional chips, below 20 rows the status bar drops to one row and the footer legend goes, and below 40×10 a single "Terminal too small" notice is drawn.

### Keyboard Shortcuts

The complete 25-row table is the on-screen legend: press `?` or `F1`, or run `/help`. Core bindings:

| Shortcut | Action |
|---|---|
| `Enter` | Send message. |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+Enter` / `Ctrl+J` | Insert newline in multi-line prompt. |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo text in input box. |
| `Ctrl+Up` / `Ctrl+Down` | Browse input history — the **only** history binding, in every focus (a pane focused included). |
| `Tab` | Accept a slash-command completion, else cycle focus forward. `Shift+Tab` cycles backward. |
| `F2` | Cycle view mode (`M4 → M1 → M2 → M3 → M4`). |
| `F3` / `F4` / `F5` | Transcript / Plan / Workers view mode. |
| `Ctrl+P` / `Ctrl+A` | Show or hide the Plan / Workers pane. |
| `Ctrl+T` | Toggle thinking blocks (echoed by the `think:on` chip). |
| `Ctrl+F` | Toggle transcript follow — the `● LIVE` / `↓ N new` / `↓ follow off` chip on the transcript border. A scroll that moved away from the tail turns follow off; only a scroll that **lands** on the tail turns it back on. |
| `?` / `F1` | Keymap and command overlay (`?` only with an empty composer). |
| `PageUp` / `PageDown` | Scroll the focused pane by 10 lines — in the Workers pane that is the selected worker's **detail log**. With the composer focused the key has no owner and does nothing (the composer has no viewport). |
| `Up` / `Down` | With composer focus: caret inside a multi-line draft — never history, and where the caret has nowhere to go the press is not claimed at all (no scroll, no repaint, no follow change). With a pane focused: that pane by 1 line — the selected worker's **detail log** in the Workers pane; the roster window is walked instead by `Shift+Up` / `Shift+Down`, or by plain `Up` / `Down` only where no detail pane is drawn (frame below 60 columns). History is `Ctrl+Up` / `Ctrl+Down`, from any focus. |
| `Shift+Up` / `Shift+Down` | In the Workers pane: step the **roster window** (the visible slice of the worker list) without touching the selected worker's detail log. Bound in `scroll_worker_roster_by` (`src/ui/tui/events.rs`); the chord is not one of the 25 printed legend rows. |
| `←` / `→` | With Workers focused: previous / next worker in the roster (and the pane shows that row's newest detail line); at either end, or with no worker, the caret. Otherwise: caret. |
| `Home` / `End` | Line start / end in the composer; otherwise the focused viewport's top / bottom — the detail log, not the roster, in the Workers pane. |
| `Ctrl+X` | Cancel the **selected** worker (opens the confirm overlay; Workers pane focused and at least one row on the roster — otherwise the status line says why, and `/cancel <worker key or task id>` names the row instead). Both spellings stop at the same confirm. |
| **removed** `x` | The bare-letter cancel is gone (it armed on an empty draft in the Workers pane and swallowed the letter). Replaced by `Ctrl+X`, or `/cancel <worker key or task id>`; `x` now always types into the composer. |
| **removed** `e` | The bare-letter last-error recall is gone (it armed on an empty draft over a pane focus and swallowed the letter). Replaced by `/error`, which prints the sticky last error from any focus; `e` now always types into the composer. |
| `y` / `n` | Confirm / dismiss the pending action — honoured only inside the confirm overlay. |
| `Ctrl+D` | Quit confirm overlay (never exits un-armed). |
| `Esc` / `Ctrl+C` | Close an overlay; else arm abort (`Abort armed. Press ESC again to abort.`); a second press within 3 s exits. Any printable key disarms the arm. |

The bindings and behaviours below are gone, and what follows is the very list `?` / `F1` and `/help` print under the keymap table — `removed()` in `src/ui/tui/components/help.rs`, same wording, same order — so the README and the in-app legend say the same thing:

- **mouse scroll / click** — removed: mouse capture is never enabled, so the handler block was unreachable.
- **`Ctrl+D` as immediate exit** — removed: `Ctrl+D` now opens the Quit confirm overlay.
- **bare `stop` / `cancel` / `exit` / `quit` / `q` / `abort`** — removed: ordinary steering text; the slash forms open a confirm overlay.
- **`Esc` re-focusing Chat** — removed: `Esc` closes an overlay or arms abort; it never changes focus.
- **3-row worker roster clamp** — removed: every live worker is reachable.
- **bare `x`** — removed: it armed on an empty draft in the Workers pane and swallowed the letter; `Ctrl+X`, or `/cancel <worker key or task id>`, now asks to cancel the selected worker (confirm).
- **bare `e`** — removed: it armed on an empty draft over a pane focus and swallowed the letter; `/error` prints the sticky last error from any focus.
- **plain `Up` / `Down` history recall** — removed: it fired while the operator was scrolling a pane and replaced the draft; `Ctrl+Up` / `Ctrl+Down` walks the input history in every focus.

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
| `/error` | Print the sticky last error again, plus the log path — from any focus, and the only route to the recall now that the bare `e` key is removed. |
| `/cancel <worker key or task id>` | Cancel one worker (confirm). |
| `/cancel` | Selected worker when the Workers pane is focused; otherwise a quit spelling (confirm). |
| `/cancel-all` | Cancel every running worker (confirm). |
| `/reset` (`/reset_plan`, `/reset-plan`, `/clear_plan`, `/clear-plan`, `/reset_execution_plan`) | Clear the execution plan, session bundle, steering history and transcript, and tell the model (confirm). |
| `/exit` (`/quit`, `/q`, `/abort`, `/stop`, and the vim spellings `:q` / `:q!`) | Quit the session, cancel running workers, and stamp a `userAbort` ending on the bundle (confirm). |

Command semantics, refusal wording and the autocomplete rules are implemented in `src/ui/tui/events.rs` (dispatch and refusals) and `src/ui/tui/components/input.rs` (the autocomplete table behind `slash_commands()`).

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
- **Full event vocabulary.** Besides the tags above there are `[steer]`, `[thinking]`, `[status]`, `<agent_tag>:content`, `<agent_tag>:thinking`, and `[delegation] STARTED → …` / `DONE    …` / `FAILED  …`. A `--resume` run additionally labels the replayed pane content and the repair report: `[restored]` echoes each restored transcript line, and `[restore]` / `[restore-warning]` / `[restore-lost]` / `[restore-skipped]` carry the restore summary, the warnings, the lost entries and the panels raw mode cannot redraw. Token counters and queue-state transitions are silent, so headless output is byte-stable.
- **Logs go to stderr.** In raw mode `tracing` writes to stderr instead of the rotating `.marmel/marmel.log`.

### Terminal Commands (`run_command` & PTY)

`run_command` is the one-shot shell tool carried by the `coder`, `researcher`, `debugger` and `validator` rosters, while the `pty_spawn` / `pty_write` / `pty_read` / `pty_close` / `pty_list` family drives interactive terminal sessions for `debugger` and `validator` (see [Built-In Archetypes](#built-in-archetypes)). `run_command` is bounded by `command_timeout_secs` — 60 seconds by default, 300 at most. The interactive tools are not: `pty_spawn` takes no timeout of its own, `pty_write` and `pty_read` wait for a per-call `wait_ms` (300 ms and 0 ms when the caller passes none), and a session left idle for 300 seconds is reaped — which bounds inactivity, not how long a command inside the session may run.

On Windows a one-shot command is not a pseudo-terminal session at all: it runs as a plain child process, and that changes what you get back.

- **The shell is the platform's own.** On Linux and macOS a command runs through `sh -c`, inside the [Landlock sandbox](#security--sandboxing) where the platform provides one. On Windows the shell is `%COMSPEC%` when that variable is set to something non-empty, and `cmd.exe` otherwise; the command payload reaches `cmd /C` **verbatim**, so quoting, `&&`, redirection and paths with spaces behave exactly as they do in a normal `cmd.exe` session, and the working directory is the workspace root. Its stdin is not connected, so a command that waits for input sees end-of-file instead of stalling the run.
- **The exit status is part of the answer.** A clean exit returns the captured output alone, while a non-zero exit appends one line — `[exit code: N]` — so a failed command no longer reads like a successful one.
- **Output survives the deadline.** A timeout or an abort appends its notice (`[command timed out after 60s and was killed]`, `[command aborted by cancellation signal]`) to the output captured up to that moment instead of throwing it away, and the whole child process tree is terminated, so nothing keeps running behind the run.
- **Interactive PTY sessions are unaffected**: `pty_spawn` and its siblings still drive a real pseudo-terminal on Windows as they do on Linux and macOS.

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
| `.marmel/session/` | The **session bundle**: the split-file snapshot that `--resume` reads back (implemented in `src/session/**`: `src/session/mod.rs`, `src/session/commit.rs`, `src/session/format/`, `src/session/restore.rs`). It is a directory, never a single file. |
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
| **Coder** | System architecture, implementation, refactoring, test suites. | `delegate_task`, `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `rebirth`, `sleep` (the `sleep` grant also carries its `terminal__sleep` alias), the `kiwix__*` MCP wildcard, `reply_to_arbitrator` |
| **Researcher** | Codebase reconnaissance, documentation inspection, API contracts. | `delegate_task`, `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `rebirth`, `sleep` (plus the `terminal__sleep` alias), `kiwix__*`, `reply_to_arbitrator` |
| **Debugger** | Crash forensics, regression isolation, minimal fixes, interactive PTY. | `delegate_task`, `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `pty_spawn`, `pty_write`, `pty_read`, `pty_close`, `pty_list`, the `pty__*` and `pty_*` wildcards, `kiwix__*`, `rebirth`, `sleep` (plus the `terminal__sleep` alias), `reply_to_arbitrator` |
| **Validator** | Independent auditor; runs test suites and issues formal verdicts. | `delegate_task`, `read_file`, `grep_search`, `glob`, `run_command`, `pty_spawn`, `pty_write`, `pty_read`, `pty_close`, `pty_list`, the `pty__*` and `pty_*` wildcards, `kiwix__*`, `leave_verdict`, `rebirth`, `sleep` (plus the `terminal__sleep` alias), `reply_to_arbitrator` — no `write_file` or `replace` |
| **Generalist** | Polymath for cross-cutting logic and multi-disciplinary tasks. | `*` (every tool, `delegate_task` included) |
| **Planner** | Mission architect for workspace reconnaissance and phased planning. | `read_file`, `grep_search`, `glob`, `create_plan`, `rebirth`, `sleep` (plus the `terminal__sleep` alias), `reply_to_arbitrator` |

> [!NOTE]
> The Manager roster is exactly eleven tools: planning (`create_plan`, `archive_current_plan`), reading (`read_file`, `grep_search`, `glob`), delegation (`delegate_task`), targeted subagent control (`pause_subagent`, `resume_subagent`), context compaction and waiting (`rebirth`, `sleep`), and replying to the steer arbitrator (`reply_to_arbitrator`). It cannot directly modify files (`write_file`, `replace`), execute commands (`run_command`), or pause the whole execution loop through a tool. Stopping and pausing the overall run remain user-controlled through the UI and steering.

The Manager can pause and resume one live specialist using its exact registry key (`Address This Worker With` in the subagent listing). A cooperative pause preserves the task and context, yields an active model stream, and waits before subsequent model calls or tools; an already-running tool may finish first. Other subagents and the Manager continue running. Resume continues the same task, and cancellation still works while paused. Manual pauses are process-local and are not restored after restarting Marmel.

Two rules are enforced ahead of any allowlist: `leave_verdict` is Validator-only, and `create_plan`, `pause_subagent`, and `resume_subagent` execute only through the Manager — the same call routed through a specialist is refused even when its allowlist or a synthesized blueprint names it. `reply_to_arbitrator` is granted to every specialist unconditionally. A specialist can delegate at all only because `delegate_task` sits in its own allowlist — every built-in except the **Planner** carries it — and only the **Coder** and **Generalist** declare recursion; nested delegation stays bounded by `max_recursion_depth`. A plan line's `(validate)` / `(no-validate)` directive changes only whether that task's deliverable is audited — never which role receives the task — and the **Validator** and **Planner** rows are exempt from validation by role, so no directive buys their audit back.

### Custom Archetypes (`ARCHETYPES.md`)

To adjust what the built-in roles advertise, add an `ARCHETYPES.md` file to your workspace root (or globally at `~/.marmel/ARCHETYPES.md`). A section whose heading maps onto a built-in id overrides that role's default skills and tools; a new heading is surfaced to the planner as guidance but cannot itself be delegated to, because `delegate_task` resolves only the six built-in ids:

```markdown
## SecurityAuditor
description: Vulnerability scanning and security review.
skills: verification, research
tools: read_file, grep_search, glob, run_command, leave_verdict, reply_to_arbitrator
```

Only `description:`, `skills:`, and `tools:` are read; every other `key:` line and all free text are ignored. `##` and `###` headings both work, and the role id is the heading lowercased with spaces and hyphens turned into underscores — so the section above registers as `securityauditor`, which matches no built-in id and is therefore planner guidance only. `~/.marmel/ARCHETYPES.md` is read first and `<workspace>/ARCHETYPES.md` second, both keyed by id, so the project file wins on a collision.

### Project Instructions (`AGENTS.md`)

`AGENTS.md` follows the common convention: free-form project rules (git workflow, required checks, conventions) for any agent working in the repository. Its full text is appended as a `# Project Instructions (AGENTS.md)` section to the Manager's system prompt and to every delegated worker's prompt. `~/.marmel/AGENTS.md` is included first and `<workspace>/AGENTS.md` second; the combined text is capped at 32 KiB.

If a directory has no `AGENTS.md`, `CLAUDE.md` and then `GEMINI.md` are used instead, so repositories that already ship one of those work unchanged.

`AGENTS.md` files in subdirectories apply to the files beneath them: when an agent reads the first page of a file, the instruction files found between the workspace root and that file's directory are appended to the result (outermost first, capped at 8 KiB). They take precedence over the project-level rules.

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

Only the `id`, `name`, `description`, and `suggested_tools` (or `tools`) front matter keys are read; any other key is ignored, and the body below the front matter is the content that gets injected. The compiled-in skills are `clean_code`, `debugging`, `research`, `verification`, `testing`, and `git_workflow` — the last carries the worktree, branch and commit discipline described in [Git Workflow & Worktrees](#git-workflow--worktrees) and is a default skill of `coder`, `debugger`, `validator` and `generalist`.

When a matching task is delegated, the skill content is injected into the specialist's prompt and its `tools` widen the allowlist for that run; `rebirth` is always appended, and neither a skill nor a synthesized blueprint can widen the verdict class — `leave_verdict` stays Validator-only.

---

## Offline Reference Search (zim-search + kiwix MCP)

Marmel can hand its agents a local copy of Wikipedia. The workspace ships a second binary crate, `zim-search` (`crates/zim-search`, a member of the Cargo workspace), that reads ZIM offline content archives — the Kiwix format, e.g. a full Wikipedia or Stack Exchange dump — indexes them with Tantivy/Okapi BM25 and serves them over MCP (stdio) alongside a local HTTP viewer for humans.

- **Full articles, not snippets:** `read_article` returns whole articles, `search` returns ranked passages, and `list_archives`, `list_articles`, `index_archive` and `get_archive_info` cover discovery and on-demand indexing. A `researcher` subagent can consult a Wikipedia-scale corpus offline — no network, no rate limits — before it reaches for a web tool.
- **Nothing is compiled in:** the wiring is 100 % config-driven. `mcp_servers` defaults to empty, and `zim-search` is reached over MCP stdio only, so the agent binary never links the archive or index code. You register the server once, in the same `[mcp_servers.<name>]` table as any other external server.
- **A registered server is visible to workers:** the built-in rosters for `coder`, `researcher`, `debugger` and `validator` already carry a `kiwix__*` wildcard (`generalist` uses `*`, `planner` none), and a server that an allowlist names gets an *MCP Server Guidance* block injected into subagent prompts, so agents know the offline corpus exists and what to use it for. Only `orchestration.mcp_servers` (Manager) and `specialists.<role>.mcp_servers` (specialists) grant MCP tools.
- **A human-facing viewer too:** `zim-search serve` exposes the same corpus in a browser (`127.0.0.1:8080` by default, `--host` / `--port` to change it), so an operator can inspect an archive outside the agent loop.

### Build, Index, Register

```bash
# 1. Build the binary from the repo root (no install step; it is a workspace member,
#    so the artifact lands in the shared target/ dir referenced by the config below).
cargo build --release -p zim-search                    # -> target/release/zim-search

# 2. Index the corpus into its own index directory (keep it out of the archive tree).
target/release/zim-search index --kiwix-dir /mnt/extradisk/kiwix --index /mnt/extradisk/kiwixidx

# 3. Try it from the CLI (subcommands: info, list, read, index, search, serve, mcp).
target/release/zim-search search --kiwix-dir /mnt/extradisk/kiwix --index /mnt/extradisk/kiwixidx "aeration"
```

Then register it in `marmel.toml` — the shipped `marmel.toml.example` already contains this block (`[mcp_servers.kiwix]`):

```toml
[mcp_servers.kiwix]
command = "/home/fredrik/marmel/wip/target/release/zim-search"   # absolute path, as shipped
args = ["mcp", "--kiwix-dir", "/mnt/extradisk/kiwix", "--index", "/mnt/extradisk/kiwixidx"]
```

Those six tools then reach agents as `kiwix__list_archives`, `kiwix__list_articles`, `kiwix__search`, `kiwix__read_article`, `kiwix__index_archive` and `kiwix__get_archive_info`. The archive and index locations shown here are the crate defaults — `/mnt/extradisk/kiwix` and `/mnt/extradisk/kiwixidx` — and are named per server through `args` (the legal per-server keys are `command`, `args`, `env`, `url` and `prompt`).

Point the `prompt` key at free-form guidance and it rides along in that *MCP Server Guidance* block, which is how you tell an agent to consult the corpus before it reaches for a web tool. Build the index before the first query: the `mcp` subcommand does not index on its own (`--auto-index` defaults to off there), so run `index` once, or let an agent call `index_archive`.

Registration is optional, and everything about it is opt-in: an MCP server that fails to boot logs a warning and the run continues without it. Retrieval in the default build is **lexical BM25 only** — dense ONNX vectors need the opt-in `semantic` Cargo feature and additionally per-archive opt-in at index time. Both the `search` subcommand and the `search` MCP tool **default to BM25**: vector retrieval runs only when the caller asks for it explicitly (`--mode semantic|hybrid` on the CLI, `mode` on the tool), hybrid fuses the two signals when both are available, and a vector mode requested on an index without vectors degrades to BM25. Automatic hybrid selection — hybrid is *preselected* when an archive in the library carries usable vectors — is a `serve` web-viewer behaviour. The full reference — every flag, tool schema, index layout and HTTP viewer detail — lives in [crates/zim-search/README.md](crates/zim-search/README.md).

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

The tests live in two places: integration tests under `tests/` and unit tests beside the code they cover — modules named `*_tests.rs` and inline `#[cfg(test)]` modules inside source files. The names below are a **representative selection**, not the full inventory: `tests/` currently holds 38 standalone integration files and the pattern (`test_<area>*` / `*_tests`) is what matters. Named examples: `test_orchestrator.rs`, `test_role_gating.rs`, `test_validation_loop.rs`, `test_dynamic_prompts.rs`, `test_llm_pool.rs`, `test_session_boot.rs`, and the `test_session_restore_*.rs` family, plus the UI-side `test_new_ui_runtime_smoke.rs`, `test_ui_session.rs`, `test_steer_history.rs`, `test_midturn_notice.rs`, `test_panel_paused_indicator.rs`, `test_subpane_logs_repro.rs`, `test_subpane_queued_refresh.rs`, `test_delegation_queue_events.rs`, and `test_internal_fast_calls.rs`. Unit-test modules include `src/harness/fs_tests.rs`, `src/harness/pty_tests.rs`, `src/manager/phase_tests.rs`, `src/orchestrator/steer_tests.rs`, `src/mcp/http_tests.rs`, `src/ui/tui/tests.rs`, `src/ui/tui/tests_new_ui.rs`, `src/ui/bridge/tests.rs`, `src/ui/session_tests.rs`, `src/orchestrator/notice_tests.rs`, `src/harness/dispatch_integrity_tests.rs`, and `src/harness/tool_resolution_tests.rs`, and inline `#[cfg(test)]` modules inside files such as `src/plan_parse.rs` and `src/agents/prompt_builder.rs`.

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
cargo fmt --all -- --check
```

---

## License

No license is declared in this repository yet: the `marmennill` package (and the `zim-search` crate under `crates/`) carries no `license` field in its `Cargo.toml`, and there is no `LICENSE` file in the repository root — so no license terms are stated anywhere in the tree, and no `LICENSE` link can be pointed at. Until a license file is added, treat the sources as all-rights-reserved rather than MIT-licensed.
