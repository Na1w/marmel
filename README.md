# Marmennill (marmel)

> **Autonomous agentic coding assistant** — a terminal-driven orchestrator that plans, delegates, executes, and validates multi-step software engineering and research tasks against any OpenAI-compatible LLM backend.

Marmel is a Rust-based CLI that connects to an OpenAI-compatible chat-completions backend (e.g. Ollama, vLLM, OpenRouter) and drives a **Manager + Specialist Subagent** architecture to autonomously complete complex tasks in a local workspace. It runs as an interactive TUI or a headless, pipe-friendly raw mode.

---

## Table of Contents

- [Features](#features)
- [Architecture](#architecture)
- [Quick Start](#quick-start)
- [Configuration](#configuration)
- [Usage](#usage)
- [Specialist Roles](#specialist-roles)
- [How It Works](#how-it-works)
- [Testing](#testing)
- [Dependencies](#dependencies)
- [License](#license)

---

## Features

- **Fractal Manager + Specialist orchestration** — a Manager decomposes a goal into a disk-backed execution plan and delegates each atomic task to a domain specialist. The Manager never performs domain work itself; it only plans, delegates, and synthesizes.
- **Disk-backed execution plan & auto-resume** — the plan lives at `.marmel/execution_plan.md` in `- [ ] [t-xxx]` checkbox format, auto-checked-off on completion, auto-resumed on session restart, and archived when done.
- **Five specialist roles** with per-role tool allowlists — `coder`, `researcher`, `debugger`, `validator`, and `generalist`.
- **Automated validation loop** — specialist deliverables are automatically audited by a Validator subagent; rejected work is fed back for revision (up to 5 iterations by default).
- **Multi-tier resilience harness** — XML tool-call rescue, semantic tool repetition detection, and text loop breaking (consecutive lines, line bigrams, word 4-grams) with live SSE stream interruption and automatic retry.
- **Context engine with proactive rebirth & compaction** — `cl100k_base` BPE token counting, KV-cache prefix preservation, proactive rebirth advisory at 80% budget with state preservation instructions (offsets, files, data), forced compaction at 90%, and universal `rebirth` tool availability across all agents and validators.
- **Stream preemption & cooperative pause/resume** — mid-flight user steering and queries can pause/preempt active specialist LLM streams on shared local backends without losing state, servicing arbitration before resuming.
- **Interactive Steer Arbitrator & multi-turn history** — real-time user steering mid-flight (respond, abort, queue, forward, approve/reject plan, delegate, or sleep) with multi-turn conversation memory, immediate stream preemption, and human-readable duration formatting (minutes and seconds).
- **Agent sleep tool (`sleep`)** — universal sleep tool enabling specialists and the Steer Arbitrator to pause for $N$ seconds (with clean cancellation checks and cooperative runtime yielding) before retrying or checking status.
- **Extended prefill watchdog** — 300s (5-minute) timeout window accommodating slow prefill on long-context local models (e.g. Gemma 4, Llama 3, DeepSeek) without premature aborts.
- **LLM streaming client** — SSE streaming with retry/backoff, watchdog timeouts, and `[thinking]` tag demuxing.
- **Deep-Freeze crash recovery & full UI rehydration** — in-flight delegations are snapshotted and journaled; sessions resume seamlessly with full restoration of chat history, execution plans, and past specialist subagent deliverables in the TUI Agent pane.
- **MCP (Model Context Protocol) client** — JSON-RPC 2.0 over stdio and SSE/HTTP, with tool discovery and execution.
- **Reasoning budget enforcement & stream cutoff** — configurable per-specialist and global limits on thinking tokens (`max_thinking_tokens`) with mid-stream cutoff and seamless corrective continuation prompts to prevent runaway reasoning loops.
- **High-performance, low-overhead UI rendering** — batched async event draining and 40 FPS frame throttling ensure near-zero CPU usage during idle periods and high-throughput streaming.
- **Two UI modes** — an interactive 3-panel Ratatui TUI (with subagent auto-focus, scroll clamping, and full horizontal cursor navigation) and a headless raw streaming mode.
- **Live session token accounting** — global atomic tracking of cumulative input and output tokens across Manager turns, specialist subagents, validators, and arbitrators with auto-scaled metrics in the status bar.

---

## Architecture

```
                    ┌─────────────────────────────────────────────┐
                    │                 User (TUI / raw)            │
                    └──────────────────────┬──────────────────────┘
                                           │
                    ┌──────────────────────▼──────────────────────┐
                    │              OrchestratorManager            │
                    │        (plans, delegates, synthesizes)      │
                    └───────┬──────────────┬──────────────┬───────┘
                            │ delegate_task│              │
              ┌─────────────▼──┐   ┌────────▼──────┐   ┌──▼─────────────┐
              │   Specialist   │   │   Specialist  │   │   Specialist   │
              │  (coder, etc.) │   │  (researcher) │   │  (validator)   │
              └───────┬────────┘   └────────┬──────┘   └──────┬─────────┘
                      │                     │                 │
              ┌───────▼─────────────────────▼─────────────────▼────────┐
              │                    Tool Harness                        │
              │  read_file · write_file · replace · run_command        │
              │  grep_search · glob · pty_* · delegate_task            │
              │  create_plan · archive_current_plan · rebirth          │
              │  leave_verdict · sleep                                 │
              └───────┬─────────────────────┬─────────────────┬────────┘
                      │                     │                 │
              ┌───────▼───────┐     ┌────────▼────────┐  ┌─────▼──────────┐
              │  LLM backend  │     │  Execution plan │  │  MCP servers   │
              │ (OpenAI-compat│     │ (.marmel/       │  │ (JSON-RPC 2.0) │
              │  SSE stream)  │     │  execution_plan │  │                │
              └───────────────┘     │  .md)           │  └────────────────┘
                                    └─────────────────┘
```

**Flow at a glance:**

1. The user provides a goal (via CLI prompt or TUI input).
2. The Manager builds a disk-backed execution plan at `.marmel/execution_plan.md`.
3. For each unchecked task, the Manager dispatches a `delegate_task` to the matching specialist.
4. Each specialist runs in an **isolated context** (role prompt + task brief + bounded snippets), executes tools against the local workspace, and returns a deliverable with a terminal marker (`MISSION COMPLETE`, `FAILED`, or `REPLAN REQUIRED`).
5. A Validator subagent audits the deliverable; rejected work is fed back for revision.
6. The Manager auto-checks-off completed plan tasks on disk and synthesizes the final answer.

---

## Quick Start

### Prerequisites

- **Rust toolchain 1.98+** (edition 2024).
- An **OpenAI-compatible chat-completions backend** reachable over HTTP (e.g. Ollama at `http://localhost:11434/v1`, vLLM, OpenRouter, or a local GPU server).

### Build

```bash
cargo build --release
```

The binary is produced at `target/release/marmel`.

Other useful commands:

```bash
cargo build          # debug build
cargo run            # run the marmel binary
cargo test           # run unit + integration tests
cargo check          # type-check without codegen
```

### Configure

Create a `marmel.toml` in the workspace root (see [Configuration](#configuration) for the full reference, or copy `marmel.toml.example`).

### Run

```bash
# Interactive TUI
marmel

# With an initial prompt
marmel "Refactor the parser module and add tests"

# Headless raw mode (pipe-friendly)
marmel --raw "explain src/main.rs"

# Explicit config file
marmel --config /path/to/marmel.toml
```

---

## Configuration

### Config file lookup order

Marmel searches for configuration in the following order (first match wins):

1. `--config <path>` CLI flag (explicit override).
2. `./marmel.toml` (workspace root).
3. `./.marmel.toml`
4. `./.marmel/marmel.toml`
5. `./.marmel/config.toml`
6. `~/.marmel/marmel.toml`
7. `~/.marmel/config.toml`
8. `~/.config/marmel/config.toml`
9. `~/.config/marmel/marmel.toml`
10. Environment variables (see below).
11. Built-in defaults.

### Environment variables

Applied after file config, before defaults:

| Variable | Description |
|---|---|
| `MARMEL_AUTH_TOKEN` | Bearer token for the backend. |
| `MARMEL_BACKEND_URL` | Backend base URL. |
| `MARMEL_MODEL` | Model identifier. |

### Config field reference

| Field | Default | Description |
|---|---|---|
| `backend_url` | `http://localhost:8000/v1` | OpenAI-compatible chat completions base URL (no trailing slash). |
| `auth_token` | `""` | Optional bearer token. |
| `model` | `llama3.1-8b-instruct` | Model identifier. |
| `temperature` | `0.7` | Sampling temperature. |
| `top_p` | `0.9` | Nucleus sampling. |
| `frequency_penalty` | `0.0` | Frequency penalty. |
| `presence_penalty` | `0.0` | Presence penalty. |
| `max_context_tokens` | `8192` | Context window budget; compaction triggers at 90%. |
| `max_thinking_tokens` | `16384` | Maximum reasoning/thinking tokens per single turn before cutting off runaway reasoning. |
| `preserve_thinking` | `true` | Keep `[thinking]` content in the transcript. |
| `command_timeout_secs` | `60` | Timeout for a single `run_command` / PTY invocation. |
| `max_repetition_threshold` | `5` | Consecutive identical turns that trigger cycle breaking. |
| `enable_xml_rescue` | `true` | Enable XML-rescue fallback for malformed tool calls. |
| `ui_mode` | `"tui"` | `"tui"` (Ratatui) or `"raw"` (plain streaming). |
| `system_prompt_path` | `prompts/system.md` | Path to the Manager system prompt. |
| `debug` | `false` | Detailed debug logging to `debug.log`. |
| `[monitoring]` | — | Resilience harness thresholds (`enabled`, `repetition_threshold`, `min_pattern_len`, `max_stream_tokens`, `max_thinking_tokens`). |
| `[orchestration]` | — | `max_recursion_depth` (default 3), `manager_module`, `mcp_servers`, `specialists` table. |
| `[orchestration.specialists.<role>]` | — | Per-specialist `tools`, `model`, `backend_url`, `auth_token`, `mcp_servers`, `validator_model`, `validator_backend_url`, `validator_auth_token`, `max_validator_iterations`, `enable_validator` (aliases: `auto_validate`, `enable_validation`), `max_thinking_tokens` (aliases: `reasoning_budget`, `thinking_budget`). |
| `[mcp_servers.<name>]` | — | External MCP server registration (`command`, `args`, `env`, `url`). |

### Specialist Configuration & MCP Routing

Marmel enforces strict context boundaries and Role-Based Access Control (RBAC). Both built-in tools and external MCP servers can be granted selectively per specialist.

- **Orchestrator Manager:** Performs planning, delegation, and result synthesis.
- **Specialists (`coder`, `debugger`, `researcher`, `validator`, `generalist`):** Execute domain tasks with isolated context and scoped toolsets.
- **Model routing:** Each specialist can route to different LLM backends/models (e.g. `coder` on local GPU, `researcher` on cloud).
- **Scoped MCP servers:** Each specialist only receives the tool schemas for its configured `mcp_servers`. Tools from external servers are namespaced as `<server_name>__<tool_name>` to eliminate collisions.

### Example configuration (`marmel.toml`)

```toml
backend_url = "http://localhost:8000/v1"
model = "llama3.1-8b-instruct"
max_context_tokens = 8192
ui_mode = "tui"

# ---------------------------------------------------------------------------
# External MCP Servers (Model Context Protocol)
# ---------------------------------------------------------------------------

# Local stdio MCP server
[mcp_servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]

# Remote HTTP/SSE MCP server
[mcp_servers.docs_search]
url = "https://mcp.example.com/search"

# ---------------------------------------------------------------------------
# Orchestration & Specialist Access Control
# ---------------------------------------------------------------------------

[orchestration]
max_recursion_depth = 3
# Manager can access high-level docs search MCP
mcp_servers = ["docs_search"]

# Coder gets filesystem MCP tools and local GPU model
[orchestration.specialists.coder]
tools = ["delegate_task", "write_file", "replace", "read_file", "run_command", "grep_search", "glob"]
mcp_servers = ["fs"]
model = "deepseek-coder-v2"
backend_url = "http://localhost:11434/v1"
validator_model = "gemma4:cloud"
max_validator_iterations = 5
max_thinking_tokens = 8192

# Researcher gets docs search MCP tools
[orchestration.specialists.researcher]
tools = ["delegate_task", "read_file", "run_command", "grep_search", "glob"]
mcp_servers = ["docs_search"]
model = "deepseek-v4-flash:cloud"

# Debugger with terminal PTY access (no external MCP tools needed)
[orchestration.specialists.debugger]
tools = ["delegate_task", "write_file", "replace", "read_file", "run_command", "grep_search", "glob", "pty_spawn", "pty_write", "pty_read", "pty_close", "pty_list"]
mcp_servers = []
model = "deepseek-v4-flash:cloud"
```

---

## Usage

### CLI flags

| Flag | Description |
|---|---|
| `--config <path>` | Override config file path. |
| `--raw` | Force headless stdout mode. |
| `--debug` | Detailed debug logging to `debug.log`. |
| `-h` / `--help` | Print usage. |
| `PROMPT` | Optional initial prompt to start the session. |

### Interactive TUI

```bash
marmel
marmel "Refactor the parser module and add tests"
```

The TUI is a 3-panel Ratatui interface: **Chat** / **Plan** / **Subagents**.

**Keyboard shortcuts:**

| Key | Action |
|---|---|
| `Enter` | Send input. |
| `Esc` / `Ctrl+C` | Confirm-abort (press twice to quit; Esc first restores focus to Chat). |
| `Ctrl+D` | Instant abort. |
| `Tab` | Cycle focus (Chat / Plan / Subagents). |
| `Left` / `Right` | Move cursor left/right (in Chat) or switch selected subagent (in Subagents). |
| `Home` / `End` | Move cursor to start/end (in Chat) or scroll to top/bottom (in other panels). |
| `PageUp` / `PageDown` | Scroll panel up/down by 10 lines. |
| `Up` / `Down` | Scroll panel up/down by 1 line (when not using Ctrl). |
| `Ctrl+Up` / `Ctrl+Down` | Input history navigation. |
| `Backspace` / `Delete` | Delete grapheme before / after cursor. |
| `Ctrl+P` | Toggle execution plan panel. |
| `Ctrl+A` | Toggle subagents panel. |
| `Ctrl+T` | Toggle reasoning/thinking display. |
| `Mouse Click / Scroll` | Click to focus panel or position cursor; scroll to scroll panels. |

**Slash commands:**

| Command | Action |
|---|---|
| `/help` | Show command and keybinding help in the chat pane. |
| `/thought` | Toggle display of reasoning/thinking blocks. |
| `/reset` (`/clear_plan`) | Clear active execution plan and revert phase to Conversational. |
| `/abort` (`/quit`, `:q`) | Abort current session. |

**Live Status Bar:**

The bottom status bar continuously reports session token metrics and active agent status:
```text
 Tokens: 1.5k in / 320 out (1.8k total) | Status: Running (gemma-4-12b) … [1 active: coder-t-001]
```
- **Tokens `in`:** Cumulative prompt tokens across all Manager and specialist subagent invocations.
- **Tokens `out`:** Cumulative completion tokens (content, reasoning/thinking, and tool call payloads).
- **Auto-scaled formatting:** Tokens are cleanly formatted as exact integers under $1\,000$, with `k` for thousands ($1.5\text{k}$), and `M` for millions ($1.2\text{M}$).

### Headless raw mode

```bash
marmel --raw "explain src/main.rs"
# or force raw regardless of terminal:
marmel --raw
```

Raw mode is pipe-friendly and streams labelled events to stdout:

```
[assistant] <text>
[thinking] <text>
[tool] name(args)
[tool-result] <text>
[status] <text>
[delegation] STARTED → coder on t-001
[delegation] DONE    coder on t-001
[done]
```

### Workspace / runtime state

`.marmel/` is the internal runtime directory (reserved; deliverables must **not** be written there):

| Path | Purpose |
|---|---|
| `.marmel/execution_plan.md` | Active execution plan. |
| `.marmel/forced_phase.txt` | Phase override. |
| `.marmel/marmel.log` | Session log (rotated at 5MB, 3 backups). |
| `.marmel/archive/` | Archived completed plans. |
| `.marmel/.session_frozen.json` | Deep-Freeze crash checkpoint. |
| `.marmel/.session_journal.json` | Append-only crash journal. |
| `.marmel/tmp/` | Temporary tool overflows. |

---

## Sandboxing & Cross-Platform Security Model

Marmel implements a multi-tiered security model to ensure agent operations remain strictly confined to the project workspace across all major operating systems.

### 1. Cross-Platform Path Confinement (Linux, macOS, Windows)

All built-in file and search tools (`read_file`, `write_file`, `replace`, `grep_search`, `glob`) pass every target path through canonical workspace validation (`resolve_safe_path`):
- **Workspace Confinement:** All operations are strictly restricted to the current workspace root directory and `/tmp`.
- **Path Traversal Defense:** Escapes via `../` (e.g. `../../etc/passwd` or `../../.ssh/id_rsa`) and external absolute paths are intercepted in-process and rejected with `ToolError::Forbidden ("access denied: path escapes workspace root")`.
- **Zero Dependencies:** Works identically on Linux, macOS, and Windows with zero external prerequisites.

### 2. Linux Landlock LSM Process Isolation (Linux)

For terminal command execution (`run_command` and interactive PTY sessions), Marmel leverages **Linux Landlock LSM** (Linux kernel $\ge 5.13$) for unprivileged kernel-enforced sandboxing:
- **Workspace & Build Caches (Read/Write/Exec):** Full access is granted to the workspace root, `/tmp`, `~/.cargo`, and `~/.cache`. This allows package managers (`cargo build`, `cargo add`, `npm`, `pip`) to download, cache, and compile dependencies normally.
- **System Toolchains (Read-Only + Exec):** System binaries and libraries (`/usr`, `/bin`, `/lib`, `/lib64`, `/etc`, `/dev`, `/proc`, `/sys`) and `~/.rustup` toolchains are strictly read-only.
- **Sensitive Directories (Completely Blocked):** Critical directories such as `~/.ssh`, `~/.gnupg`, and other directories outside the workspace are blocked by the kernel.
- **Inherited Sub-process Protection:** Landlock restrictions are applied via `--internal-sandbox-exec` right before the subshell starts, permanently confining bash, cargo, python, and any spawned sub-processes.

### 3. macOS & Windows Compatibility

On non-Linux systems (macOS and Windows), Landlock is conditionally bypassed while **Path Confinement**, working-directory encapsulation, and process-group cleanup remain 100% active.

---

## Specialist Roles

| Role | Focus | Tool allowlist |
|---|---|---|
| **Coder** | Software engineering, implementation, tests. | Read/write/run tools, search, delegation, sleep. |
| **Researcher** | Information retrieval, codebase exploration, documentation. | Read/write/run tools, search, delegation, sleep. |
| **Debugger** | Crash forensics, low-level diagnostics, interactive PTY GDB/LLDB. | Read/run/PTY tools, search, delegation, sleep. |
| **Validator** | Independent QA auditor; issues `leave_verdict` (APPROVED/REJECTED). | Read/search/PTY tools, verdict, sleep. |
| **Generalist** | Cross-domain polymath with universal `"*"` tool access. | All tools (including sleep). |

Each specialist runs in an **isolated context** — it sees only its role prompt, the task brief, and bounded snippets, never the Manager's full transcript.

---

## How It Works

### Turn state machine

Each agent turn walks: `PrepareTurn → CallBackend → StreamResponse → ProcessResponse → ExecuteTools → CheckFinish`.

- **Read-only tools** (`read_file`, `grep_search`, `glob`) run in parallel via `FuturesUnordered`.
- **Write tools** (`write_file`, `replace`, `run_command`, `delegate_task`) run sequentially.

### Plan lifecycle

1. The Manager creates a plan at `.marmel/execution_plan.md` in `- [ ] [t-xxx]` checkbox format.
2. The `ManagerLoop` (Silent Dispatcher) iterates unchecked tasks and delegates each to the matching specialist.
3. On a genuine `MISSION COMPLETE (t-xxx)` marker, the plan line flips to `[x]`.
4. On completion, the plan is archived to `.marmel/archive/`.

### Delegation flow

`OrchestratorManager::delegate(req)`:

1. Resolves the agent against the `SpecialistRegistry`.
2. Enforces the recursion depth bound (default 3).
3. Snapshots the delegation to the Crash Journal (Deep-Freeze).
4. Builds an isolated context (role prompt + brief + snippets).
5. Runs the specialist worker to completion.
6. Clears the frozen checkpoint.
7. Auto-checks-off the plan task on `MISSION COMPLETE`.

### Validation loop

Specialist deliverables are automatically audited by a Validator subagent. The `leave_verdict` tool requires an explicit verdict (`APPROVED` or `REJECTED`) and comments; omitted or invalid verdicts trigger immediate corrective tool errors. Rejected work is fed back to the specialist for revision, up to `max_validator_iterations` (default 5).

### Resilience

- **XML tool-call rescue** — recovers plain-text XML tool calls into structured JSON.
- **Semantic tool repetition & cycle gate** — blocks identical repeated calls and cuts alternating tool cycles.
- **Multi-tier text repetition breaker** — rolling 16,384-char buffer tracking:
  - $\ge 3$ identical consecutive lines.
  - $\ge 3$ repeated line bigrams.
  - $\ge 3$ repeated word 4-gram phrases across sentences.
- **Multi-turn thought repetition breaker** — detects repetitive reasoning loops across turns in specialist output, purges conversational chatter, and injects targeted corrective nudges.
- **Live stream interruption & auto-recovery** — cuts SSE generation mid-flight on loop detection, purges toxic history, and retries with increased `frequency_penalty`.
- **Empty-production nudge** — up to 3 attempts.
- **Reasoning budget enforcement & stream cutoff** — tracks cumulative thinking tokens during generation; if reasoning exceeds `max_thinking_tokens` (default 16,384 tokens, configurable globally or per-specialist), the stream is gracefully interrupted mid-flight and injected with an automatic continuation prompt instructing the model to stop thinking and directly output its tool calls or answer.
- **One-turn recovery** — adjusts `enable_thinking`, `frequency_penalty`, and `temperature` on failure.

### Stream preemption & interactive pause/resume

When running against local models on resource-constrained backends (such as a single GPU running Ollama or vLLM), concurrent generation can cause latency or memory contention. Marmel solves this with **cooperative stream preemption and pause/resume**:
- Mid-flight steering commands and user queries immediately preempt active specialist streams on the shared backend via `preempt_conflicting_stream`, avoiding GPU memory contention and ensuring instant arbitrator responsiveness.
- The Steer Arbitrator handles the user interaction immediately: answering status questions, queueing instructions, updating or rejecting plans, delegating subtasks, or sleeping.
- **Continuous Multi-Turn Steering History:** The arbitrator tracks past user questions and arbitrator responses across turns in `steering_history`, enabling context-aware follow-up discussions without losing context.
- Once the steering turn completes, preempted specialist streams resume smoothly from their last checkpoint.

### Sleep tool & periodic agent pausing

Marmel equips all agents and the Steer Arbitrator with a dedicated `sleep` tool (`sleep(seconds, reason)`):
- **Universal availability:** Available to `coder`, `researcher`, `debugger`, `validator`, `generalist`, and the Steer Arbitrator (`Sleep` decision with `sleep_seconds`).
- **Cooperative execution:** Executes via Tokio runtime-aware sleep with periodic cancellation token checks, ensuring immediate responsiveness if the user aborts or interrupts.
- **Polling & stabilization:** Enables agents to pause execution cleanly when waiting for external services, build steps, asynchronous processes, or cooldown periods without burning LLM inference tokens in tight loops.

### Context engine

- `cl100k_base` BPE token counting via `tiktoken-rs`.
- KV-cache prefix preservation (system prompt locked at `messages[0]`, goal at `[1]`).
- **Proactive rebirth advisory at 80%:** Generates an advisory notification instructing the model to invoke `rebirth` with summarized continuation state (active file paths, exact read line numbers or byte offsets, intermediate data) before forced compaction occurs.
- **Universal rebirth availability:** The `rebirth` tool is available to all roles, including the Validator, enabling clean state resets across the entire hierarchy.
- **Forced compaction at 90%:** Escalates from 70% to 50% target ratios on compaction retries, pruning orphaned tool calls while strictly pinning system and goal messages.
- **Slow-prefill watchdog (300s):** Provides a 5-minute timeout window accommodating slow prefill on long-context local models (e.g. Gemma 4, DeepSeek, Llama 3) without premature aborts.

### Deep-Freeze crash recovery & full UI rehydration

- **In-flight checkpointing:** Active delegations are snapshotted to `.marmel/.session_frozen.json` with an append-only journal at `.marmel/.session_journal.json`.
- **Startup recovery:** On boot, `recover_frozen()` runs immediately to complete or fail interrupted tasks.
- **Plan persistence:** If `.marmel/execution_plan.md` exists on disk with pending tasks, the Manager automatically resumes execution from the next unchecked item.
- **Full Agent Pane Rehydration:** Reconstitutes historical specialist subagents, deliverables, task briefs, and execution logs from `.marmel/transcript.json`, `.session_journal.json`, and the execution plan into the TUI Specialist Subagents pane, automatically making it visible upon resumption.
- **Clean Chat History:** Rehydrated chat logs summarize past delegations concisely (`[Tool Result] MISSION COMPLETE (...)`), keeping verbose deliverables accessible in the Subagents pane rather than cluttering the chat stream.

---

## Testing

```bash
cargo test
```

- **Unit tests** are organized into dedicated companion test modules (`*_tests.rs`) and inline test blocks.
- **Integration tests** live in `tests/` and use `wiremock` to mock the LLM backend.

Coverage areas include: config parsing, orchestration (delegation, check-off, recursion depth, Deep-Freeze recovery), agent loop (turn phases, tool classification, steer/abort, repetition detection, XML rescue), context engine (compaction, rebirth, token counting), harness (replace uniqueness, paginated read, grep gitignore, glob sorting, cross-platform PTY lifecycle), LLM (thinking demuxer, request construction), role gating, and UI session.

### Multi-Platform CI / CD

Marmel is continuously built and tested across all supported target platforms via GitHub Actions (`.github/workflows/ci.yml`):
- 🐧 **Linux** (`x86_64-unknown-linux-gnu`)
- 🍏 **macOS** (`aarch64-apple-darwin`)
- 🪟 **Windows** (`x86_64-pc-windows-msvc`)

- Every commit and pull request runs:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test --all-targets --all-features` (320+ unit & integration tests)
  - `cargo build --release` (optimized binary verification)

---

## Dependencies

### Runtime dependencies

| Crate | Version | Purpose |
|---|---|---|
| `tokio` | 1.44 (full) | Async runtime. |
| `tokio-util` | 0.7 (rt) | Async runtime utilities. |
| `futures` / `futures-util` | 0.3.31 | Async streams / combinators. |
| `async-trait` | 0.1.86 | Async trait support. |
| `reqwest` | 0.13 (json, stream) | HTTP client for LLM backend. |
| `eventsource-stream` | 0.2 | SSE event parsing. |
| `serde` / `serde_json` | 1.0 | Serialization. |
| `toml` | 1.1 | Config parsing. |
| `ratatui` | 0.30 | TUI rendering. |
| `crossterm` | 0.29 (event-stream) | Terminal handling. |
| `portable-pty` | 0.9 | PTY creation for command execution. |
| `tiktoken-rs` | 0.12 | BPE token counting (cl100k_base). |
| `regex` | 1.11 | Regex search / parsing. |
| `ignore` | 0.4 | Gitignore-aware file walking. |
| `unicode-width` | 0.2 | Terminal width calculation. |
| `unicode-segmentation` | 1.12 | Grapheme segmentation. |
| `uuid` | 1.25 (v4) | UUID generation. |
| `anyhow` | 1.0 | Error handling. |
| `thiserror` | 2.0 | Error types. |
| `tracing` / `tracing-subscriber` | 0.1 / 0.3 | Structured logging. |
| `chrono` | 0.4 (serde) | Timestamps. |
| `landlock` (linux) | 0.4 | Kernel-enforced unprivileged process isolation. |
| `libc` (unix) | 0.2 | Process-group kill, home dir lookup. |

### Dev-dependencies

- `tempfile` 3.17 — temp dirs for tests.
- `wiremock` 0.6 — mock HTTP backend for integration tests.

### Notable external integrations

- **OpenAI-compatible chat-completions API** — any backend: Ollama, vLLM, OpenRouter, etc.
- **MCP (Model Context Protocol)** servers over stdio (JSON-RPC 2.0).

---

## License
 
MIT

---

## Project metadata

- **Name:** `marmennill`
- **Binary / CLI:** `marmel`
- **Version:** `0.7.0`
- **Language:** Rust (edition 2024, `rust-version = "1.98"`)
- **Repository:** `https://github.com/Na1w/marmel.git` (branch `main`)
