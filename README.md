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

- **Plan-Driven Execution:** When given a goal, the Manager creates a disk-backed execution plan at `.marmel/execution_plan.md` using a checkbox format (`- [ ] [t-xxx]`). Tasks are automatically checked off as they finish, and interrupted sessions resume seamlessly from the next unchecked task.
- **Dynamic Subagents & JIT Prompt Architect:** Tasks are not sent to static prompts. An Agent Architect dynamically synthesizes a bespoke subagent blueprint (`.marmel/prompts/<task_id>.md`) combining archetype capabilities, domain skills (`skills/`), and least-privilege tool allowlists before delegation.
- **Automated Validation:** Work produced by specialists is audited by an independent Validator subagent. If tests fail or requirements are missed, feedback is returned for revision.
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
  - **Qwen 2.5 / 3.8 27B** (`qwen-3.8-27b`) — solid balance of speed, reasoning depth, and dependable tool invocation on local hardware.
  - **DeepSeek v3 / v4 Flash** — fast prefill, accurate code generation, and strong multi-turn stability.
- **Minimum:**
  - **Gemma 4 12B** — minimum viable model size. Smaller parameter sizes or aggressive quantizations frequently fail JSON tool schemas or drift from task scopes.
- **Not Recommended:**
  - **Qwen 3.6 35B** — exhibits known regressions in tool schema reliability during multi-turn agent loops.

---

## Configuration

### Lookup Order

Marmel searches for configuration in the following priority order (first match wins):

1. `--config <path>` CLI flag.
2. `./marmel.toml` (current workspace root).
3. `./.marmel.toml`
4. `./.marmel/marmel.toml`
5. `./.marmel/config.toml`
6. `~/.marmel/marmel.toml`
7. `~/.marmel/config.toml`
8. `~/.config/marmel/config.toml`
9. `~/.config/marmel/marmel.toml`
10. Environment variables (`MARMEL_BACKEND_URL`, `MARMEL_MODEL`, `MARMEL_AUTH_TOKEN`).
11. Built-in defaults.

### Configuration Reference (`marmel.toml`)

| Field | Default | Description |
|---|---|---|
| `backend_url` | `http://localhost:8000/v1` | Base URL for OpenAI-compatible chat completions (no trailing slash). |
| `auth_token` | `""` | Optional Bearer token for authentication. |
| `model` | `qwen-3.8-27b` | Model identifier to request from the backend. |
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
| `max_repetition_threshold` | `5` | Consecutive identical turns that trigger loop interruption. |
| `enable_xml_rescue` | `true` | Parse pseudo-XML tool calls into valid JSON tool calls. |
| `ui_mode` | `"tui"` | Interface mode: `"tui"` (Ratatui) or `"raw"` (streaming stdout). |
| `debug` | `false` | Write detailed debug logs to `debug.log`. |
| `[orchestration]` | — | Top-level settings: `max_recursion_depth` (default 3), `mcp_servers`. |
| `[orchestration.concurrency]` | _unset_ | Simultaneous-session cap per **backend URL + model** pair: `max_sessions` plus quoted per-URL overrides (e.g. `"http://host:8080/v1" = 2`). Unset ⇒ **unlimited** (nothing is metered); `0` ⇒ unlimited plus a warning. See [Session Concurrency](#session-concurrency-orchestrationconcurrency). |
| `[orchestration.specialists.<role>]` | — | Per-role overrides for `model`, `backend_url`, `auth_token`, `tools`, `mcp_servers`, `reasoning_effort`, and validator settings (`validator_model`, `validator_backend_url`, `validator_auth_token`, `validator_reasoning_effort`, `max_validator_iterations`, `max_fix_loop_rounds`, `enable_validator`). |
| `[mcp_servers.<name>]` | — | External MCP server registration (`command`, `args`, `env`, `url`). |

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
model = "deepseek-coder-v2"
backend_url = "http://localhost:11434/v1"
mcp_servers = ["fs"]
max_validator_iterations = 5

[orchestration.specialists.researcher]
model = "deepseek-v4-flash"
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
model = "deepseek-v4-flash:cloud"
reasoning_effort = "high"

# A cheap model with low effort for research, and an even cheaper dial for the
# validator passes that audit it.
[orchestration.specialists.researcher]
model = "qwen-3.8-27b"
reasoning_effort = "low"
validator_model = "gemma4:cloud"
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
| `--debug` | Enable detailed logging of tool and LLM exchanges to `debug.log`. |
| `-h`, `--help` | Show command-line help. |
| `[PROMPT]` | Optional initial prompt to immediately start the session. |

### Interactive TUI

The interactive TUI contains three main panels:
1. **Chat:** Conversation stream showing Manager actions, user inputs, thinking blocks, and tool executions.
2. **Plan:** The active execution plan (`.marmel/execution_plan.md`) with task checkboxes.
3. **Subagents:** Real-time log and deliverables from delegated specialists. A specialist waiting for a concurrency slot is labelled `(Queued n/m)` — its queue position out of the sessions queued for that backend URL + model pair (see [Session Concurrency](#session-concurrency-orchestrationconcurrency)).

### Keyboard Shortcuts

| Shortcut | Action |
|---|---|
| `Enter` | Send message. |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+J` | Insert newline in multi-line prompt. |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo text in input box. |
| `Tab` | Cycle focus between Chat, Plan, and Subagents panels. |
| `Ctrl+P` | Toggle visibility of the Plan panel. |
| `Ctrl+A` | Toggle visibility of the Subagents panel. |
| `Ctrl+T` | Toggle visibility of reasoning / thinking blocks. |
| `Esc` / `Ctrl+C` | Arm abort (press twice within 3 seconds to exit). |
| `Ctrl+D` | Immediate exit. |
| `Ctrl+Up` / `Ctrl+Down` | Browse input history. |
| `PageUp` / `PageDown` | Scroll focused panel up / down by 10 lines. |
| `Home` / `End` | Jump to start / end of line (Chat) or top / bottom of panel. |
| `Mouse Click / Scroll` | Click to focus panel or cursor; scroll to navigate panels. |

### Slash Commands

Type these directly into the chat input:

| Command | Action |
|---|---|
| `/help` | Print keybinding and command reference in the chat pane. |
| `/thought` | Toggle display of reasoning and thinking content. |
| `/reset` (`/clear_plan`) | Clear the current execution plan and return to conversational mode. |
| `/abort` (`/quit`, `:q`) | Exit the current session. |

### Headless Raw Mode

When redirected or invoked with `--raw`, Marmel outputs structured, pipe-friendly events:

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

### Runtime Directory (`.marmel/`)

Internal session files are stored in `.marmel/` within the workspace root:

| File / Folder | Purpose |
|---|---|
| `.marmel/execution_plan.md` | Active execution plan with checked/unchecked tasks. |
| `.marmel/prompts/` | Synthesized agent prompts and tool grants (`<task_id>.md`). |
| `.marmel/marmel.log` | Rotating application log (5MB limit, 3 backups kept). |
| `.marmel/archive/` | Archived completed execution plans. |
| `.marmel/session/` | The **session bundle**: the split-file snapshot that `--resume` reads back (see [Session Restore](docs/session_restore.md)). It is a directory, never a single file. |
| `.marmel/session/manifest.json` | The bundle's root document and **commit point**: format/schema versions, session id, wall/monotonic anchors, how the previous process ended, the identity floors, and the member list with each member's byte length and fingerprint. It is written **LAST** — that rename *is* the commit — and it is the only file that names the member set. |
| `.marmel/session/manager.json` | Manager member: the message history (pinned system prompt and goal preserved byte-for-byte), the pin witness, and the engine scalars. |
| `.marmel/session/ui.json` | UI member: pane text (chat, subagents, plan panel), status line, composer text + caret + history, scroll / selection / focus, subagent panels, and the token witnesses. |
| `.marmel/session/workers/<key>.json` | One member per subagent, named after its registry key with escaping (e.g. `coder-t-018%237.json`): full mirrored conversation history, status, counters, and pending/dangling tool-call state. |
| `.marmel/.session_transcript.json` | **LEGACY.** Written by older releases only; never written again, never merged or upgraded into a bundle. It is named in the "legacy journals found, not restorable" report on a fresh boot, and swept by `/reset`, `Plan::clear()` and `Plan::archive()`. |
| `.marmel/.ui_transcript.json` | **LEGACY.** Same handling as `.marmel/.session_transcript.json`: never written again, only named on a fresh boot, and deleted with the plan by `/reset`, `Plan::clear()` and `Plan::archive()`. |

---

## Real-Time Steering & Live Dialogue

Marmel allows you to interact with the assistant at any time—even while subagents are running commands or streaming output:

- **Immediate Pausing:** Submitting a message while an agent is working immediately pauses execution.
- **Direct Answers & Status:** Ask questions (e.g. *"What are you currently doing?"* or *"Why SQLite?"*) to get instant answers without disrupting the agent's work.
- **Course Correction:** Provide feedback, reject an approach, adjust the plan, or abort the current subtask mid-flight.
- **Direct Dialogue with Workers:** If your inquiry concerns an active subagent's internal reasoning, Marmel communicates directly with the working agent, retrieves its explanation, and reports back to you before resuming.

---

## Agent Archetypes & Customization

### Built-In Archetypes

Marmel comes with 6 built-in archetypes preconfigured in the binary. You do not need to create any configuration files to use them:

| Archetype | Description | Default Tools |
|---|---|---|
| **Coder** | System architecture, implementation, refactoring, test suites. | `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `rebirth`, `sleep`, `reply_to_arbitrator` |
| **Researcher** | Codebase reconnaissance, documentation inspection, API contracts. | `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `rebirth`, `sleep`, `reply_to_arbitrator` |
| **Debugger** | Crash forensics, regression isolation, minimal fixes, interactive PTY. | `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `pty_*`, `rebirth`, `sleep`, `reply_to_arbitrator` |
| **Validator** | Independent auditor; runs test suites and issues formal verdicts. | `read_file`, `grep_search`, `glob`, `run_command`, `leave_verdict`, `rebirth`, `sleep`, `reply_to_arbitrator` |
| **Generalist** | Polymath for cross-cutting logic and multi-disciplinary tasks. | All tools, `rebirth`, `sleep`, `reply_to_arbitrator` |
| **Planner** | Mission architect for workspace reconnaissance and phased planning. | `read_file`, `grep_search`, `glob`, `create_plan`, `rebirth`, `sleep`, `reply_to_arbitrator` |

> [!NOTE]
> The Manager is restricted to planning (`create_plan`), reading (`read_file`, `grep_search`, `glob`), and delegation (`delegate_task`). It cannot directly modify files (`write_file`, `replace`) or execute commands (`run_command`).

### Custom Archetypes (`AGENTS.md`)

To override default roles or define new specialist archetypes for your project, add an `AGENTS.md` file to your workspace root (or globally at `~/.marmel/AGENTS.md`):

```markdown
## SecurityAuditor
description: Vulnerability scanning and security review.
skills: verification, research
tools: read_file, grep_search, glob, run_command, leave_verdict, reply_to_arbitrator
```

### Custom Skills (`skills/`)

Skills provide reusable domain knowledge and automatically grant suggested tools. Place skills in `<workspace>/skills/*.md` or `~/.marmel/skills/*.md`:

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

When tasks related to database migrations are delegated, the skill content is dynamically injected into the specialist's prompt and its `tools` are granted to the agent's allowlist.

---

## Security & Sandboxing

Marmel is designed to prevent accidental modifications outside the target project:

1. **Path Confinement:** All file tools (`read_file`, `write_file`, `replace`, `grep_search`, `glob`) validate paths against the workspace root. Path traversal attempts (e.g. `../../etc/passwd` or `~/.ssh`) are blocked.
2. **Process Sandboxing (Linux Landlock LSM):** Terminal commands (`run_command` and PTY sessions) execute inside an unprivileged Landlock sandbox on Linux. Read/write access is restricted to the workspace, temporary build directories (`/tmp`, `/var/tmp`), and language caches (`~/.cargo`, `~/.cache`, `~/.npm`). Access to sensitive directories such as `~/.ssh` and `~/.gnupg` is denied by the kernel.
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

The test suite covers:
- Core orchestration and recursive delegation bounds.
- Tool allowlists, role gating, and path confinement.
- Stream preemption and steer arbitration.
- Repetition breakers and XML tool call rescue.
- Per-backend session concurrency caps, queue accounting, and queue timeouts.

To verify linting and formatting:

```bash
cargo clippy --all-targets --all-features
cargo fmt -- --check
```

---

## License

This project is licensed under the [MIT License](LICENSE).
