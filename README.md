# Marmennill (marmel)

A terminal-driven autonomous coding assistant in Rust. Marmel connects to any OpenAI-compatible chat completions backend (such as Ollama, vLLM, or OpenRouter) and coordinates a **Manager + Specialist Subagent** architecture to plan, execute, and validate software tasks within a local workspace. It runs either as an interactive Ratatui TUI or as a headless, pipe-friendly CLI.

---

## Table of Contents

- [Overview](#overview)
- [Background & Origins](#background--origins)
- [Quick Start](#quick-start)
- [Model Recommendations](#model-recommendations)
- [Configuration](#configuration)
  - [Lookup Order](#lookup-order)
  - [Configuration Reference (`marmel.toml`)](#configuration-reference-marmeltoml)
  - [Example `marmel.toml`](#example-marmeltoml)
  - [Reasoning Effort (`reasoning_effort`)](#reasoning-effort-reasoning_effort)
  - [Session Concurrency (`[orchestration.concurrency]`)](#session-concurrency-orchestrationconcurrency)
  - [Git Workflow & Worktrees](#git-workflow--worktrees)
- [Usage & Interface](#usage--interface)
  - [CLI Flags](#cli-flags)
  - [Resuming a Previous Run](#resuming-a-previous-run)
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
  - [Extra Sandbox Read-Only Paths (`sandbox_extra_readonly_paths`)](#extra-sandbox-read-only-paths-sandbox_extra_readonly_paths)
- [Testing](#testing)
- [License](#license)

---

## Overview

Marmel is built for autonomous, multi-step software development with continuous operator oversight:

- **Plan-Driven Autonomy:** When given a goal, the Manager explores the workspace and generates a disk-backed execution plan (`.marmel/execution_plan.md`). Tasks are executed sequentially or in parallel phases, tracked with standard markdown checkboxes (`- [ ] [t-001]`). If interrupted, Marmel resumes seamlessly from the next unfinished task by default.
- **Dynamic Subagents & JIT Prompt Architect:** Subagents are not static templates. A Just-In-Time (JIT) Agent Architect dynamically synthesizes tailored blueprints for each task, assembling role archetypes (Coder, Researcher, Debugger, etc.), relevant domain skills (`skills/`), and least-privilege tool allowlists before delegation.
- **Automated Validation:** Deliverables from specialist agents are independently audited by a Validator subagent. If tests fail or requirements are missed, actionable feedback is returned in an automated fix loop. Individual tasks can optionally declare `(no-validate)` when formal testing is unnecessary.
- **Real-Time Steering & Live Dialogue:** You can type into the prompt at any point. Active subagents pause to answer questions, explain reasoning, incorporate feedback, or adjust the plan on the fly.
- **Isolated Per-Task Git Worktrees:** Subagents work in isolated git worktrees (`.worktrees/<task_id>`) on dedicated task branches, commit their changes as `"Marmel Coder"`, and merge back cleanly to avoid file conflicts.
- **Offline Reference Search (MCP):** Connect external tool servers via Model Context Protocol (stdio or SSE), including the bundled `zim-search` engine for lightning-fast offline querying of multi-gigabyte Kiwix/ZIM dumps (such as Wikipedia or Stack Overflow).
- **Workspace Sandboxing:** File operations are strictly confined to the workspace. Terminal commands are isolated by a platform sandbox — Linux Landlock LSM on Linux, a Seatbelt (SBPL) profile enforced by Marmel itself on macOS — to protect sensitive directories like `~/.ssh` and `~/.gnupg`.

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
                                                  │  • Landlock/Seatbelt sandbox  │
                                                  │  • Git Worktree Isolation     │
                                                  │  • MCP Servers (stdio / SSE)  │
                                                  └───────────────────────────────┘
```

---

## Background & Origins

### The Genesis: Kvaser and Deep Research

In March 2026, Fredrik Andersson began experimenting with an autonomous AI harness designed for deep research. Originally conceived to tackle complex genealogy investigations, the system quickly expanded to encompass broader historical and technical research domains.

The initial architecture was built around **Qwen 3.6 27B**—a capable model for its size, but one with inevitable knowledge gaps given its parameter budget. To guarantee factual integrity without hallucination, the system enforced strict Retrieval-Augmented Generation (RAG) against offline ZIM archives (curated mirrors of Wikipedia, historical databases, and reference corpora).

As experiments pushed into smaller models, a fundamental bottleneck emerged: **context degradation**. When conversational context accumulated, smaller models rapidly lost coherence, hallucinated tool invocations, and drifted from their objectives.

The breakthrough lay in the architecture: **treating LLM agents as bounded subroutines**. Instead of maintaining a sprawling, monolithic conversation, an orchestrator dispatched discrete, highly bounded tasks to specialized subagents equipped with fresh, focused context windows. The project was christened **Kvaser**—a playful nod to the Norse mythological being Kvasir (formed from the saliva of the gods and brewed into the Mead of Poetry), aptly capturing how the harness distilled raw knowledge into structured insight.

### The Accidental Coder & Reverse MCP

By late June 2026, Kvaser had stabilized enough to invite external testers. To Fredrik's surprise, testers almost immediately repurposed the autonomous harness to write software rather than conduct research.

In a rapid one-week sprint, Docker container sandboxing and coding toolchains were bolted onto the server. In early July 2026, a quick hack added support for **reverse MCP**, giving birth to `kvaser-cli`. This allowed the remote server to operate transparently on the user's local machine instead of inside remote containers. Simultaneously, real-time interactive steering was introduced, enabling operators to converse with the system, exchange ideas, and guide execution mid-flight.

However, Kvaser remained a massive, monolithic server system. It was tightly coupled to Fredrik's specific server hardware and notoriously intricate to configure. External testers loved the autonomous coding experience, but nobody could reliably configure or maintain the underlying server infrastructure. It was clear that what people really wanted was the autonomous coding functionality, packaged without the operational overhead.

### The Birth of Marmel

Marmel (*Marmennill*, a prophetic water-being in Icelandic folklore) was chosen as an intentional continuation of the play on words from Kvaser. In folklore, the Marmennill is renowned for foretelling the future—drawing a poetic parallel both to the lineage of Kvaser and to the predictive nature of next-token LLM systems (even if Marmennill hails specifically from Icelandic legend rather than pan-Norse mythology).

The project was conceived as a lightweight, single-binary incarnation of `kvaser-cli`.

In a fitting showcase of the concept, **the first version of Marmel was synthesized by `kvaser-cli` itself**: it analyzed Kvaser's codebase, excised the core orchestration engine, tools, and UI, and distilled them into a self-contained Rust application with minimal configuration.

### Sandboxing: Autonomy Without Confirmation Fatigue

A defining design pillar underpinning both `kvaser-cli` and Marmel is robust sandboxing. Many existing AI coding tools require the user to babysit every file change and shell execution, inducing prompt fatigue as operators mindlessly press "yes" to dozens of confirmation dialogues.

The goal of Marmel has always been **true autonomy**: enabling agents to plan, edit, run test suites, and execute commands across multi-step tasks without stalling for permission. To make that autonomy safe, security had to be enforced structurally at the system boundary rather than interactively by the user.

By combining in-process workspace path confinement with kernel-enforced sandboxing (Linux Landlock LSM, macOS Seatbelt) and isolated per-task git worktrees, Marmel gives subagents full freedom to build and test code within the project directory, while guaranteeing that sensitive user data (such as `~/.ssh`, `~/.gnupg`, or system configurations) remains strictly protected.

### Autonomous Dogfooding Since v0.4

Since Marmel `v0.4`, virtually no code in this repository has been written by hand. Marmel has developed, debugged, and refactored itself based on user requirements and architectural goals.

For a solo developer, rigorously testing an autonomous coding assistant across thousands of synthetic edge cases is impractical. Developing Marmel *with* Marmel kills two birds with one stone: the assistant is validated against real, complex Rust code under strict compiler (`rustc`) and linter (`clippy`) constraints. If an agent hallucinates, mishandles context, or breaks a workflow, the failure is immediate and unmissable.

---

## Quick Start

### Prerequisites

- **Rust toolchain 1.98+** (edition 2024).
- An **OpenAI-compatible LLM backend** reachable over HTTP (e.g. Ollama at `http://localhost:11434/v1`, vLLM, or OpenRouter).
- **Platform support:** Marmel is developed primarily on **Linux** (which also provides kernel-enforced Landlock command sandboxing) and tested extensively on **macOS**, where the same command policy is enforced by a Seatbelt (SBPL) profile that Marmel applies itself in the sandbox re-entry child. **Windows** support is maintained on a best-effort basis.

### Installation & Build

```bash
git clone https://github.com/Na1w/marmel.git
cd marmel
cargo build --release
```

The compiled binary is placed at `target/release/marmel`.

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

Marmel requires reliable JSON schema tool calling and solid instruction-following to drive autonomous loops effectively:

- **Recommended:**
  - **Qwen 3.8 27B** (`qwen-3.8-27b`) — Built-in default. Excellent balance of reasoning depth and dependable tool invocation on local hardware.
  - **Qwen 3.8 Flash Next** (`qwen-3.8-flash-next`) — Significantly faster than 27B on modern inference engines (e.g. Strata) with comparable quality.
  - **DeepSeek v4.1 Flash** — High-performance alternative offering fast turnaround and solid tool reliability.
- **Minimum:**
  - Models around the lighter recommended sizes. Sub-7B models or aggressively quantized checkpoints often fail JSON schema constraints or drift from task scopes.
- **Not Recommended:**
  - Models that drop or malform tool-call schemas during multi-turn interactions. While Marmel can rescue pseudo-XML calls (`enable_xml_rescue`), consistent schema compliance is vital for plan execution and validator loops.

Marmel is heavily KV-cache optimized, reducing turnaround latency for local setups and lowering inference costs in the cloud.

---

## Configuration

### Lookup Order

Marmel loads the first configuration file found from this list (files are not merged):

1. `--config <path>` CLI flag.
2. `./marmel.toml` (current workspace root).
3. `./.marmel.toml`
4. `./.marmel/marmel.toml`
5. `./.marmel/config.toml`
6. `~/.marmel/marmel.toml`
7. `~/.marmel/config.toml`
8. `~/.config/marmel/config.toml`
9. `~/.config/marmel/marmel.toml`
10. Environment variables (`MARMEL_BACKEND_URL`, `MARMEL_MODEL`, `MARMEL_AUTH_TOKEN`, `MARMEL_RESUME`, `MARMEL_SESSION_DISABLE`).
11. Built-in defaults.

> **Home directory (`~`):** Resolves from `$HOME` on Unix-like systems and `%USERPROFILE%` on Windows. Global configs, custom skills, and global instructions can reside in `~/.marmel/`.

### Configuration Reference (`marmel.toml`)

| Field | Default | Description |
|---|---|---|
| `backend_url` | `http://localhost:8000/v1` | Base URL for OpenAI-compatible chat completions (no trailing slash). |
| `backend_urls` | `[]` | Optional list of backend URLs evaluated in priority order (primary -> spillover/fallback). |
| `auth_token` | `""` | Optional Bearer token for authentication. |
| `model` | `qwen-3.8-27b` | Model identifier requested from the backend. |
| `temperature` | `0.7` | Sampling temperature. |
| `top_p` | `0.9` | Nucleus sampling probability. |
| `frequency_penalty` | `0.0` | Repetition penalty. |
| `presence_penalty` | `0.0` | Topic freshness penalty. |
| `max_context_tokens` | `8192` | Maximum context tokens before compaction triggers. |
| `max_thinking_tokens` | `32768` | Hard cap on reasoning/thinking tokens before forcing continuation. |
| `reasoning_effort` | _unset_ | Optional reasoning-effort hint forwarded to backend (`none`, `minimal`, `low`, `medium`, `high`). |
| `preserve_thinking` | `true` | Retain thinking blocks in the session transcript. |
| `command_timeout_secs` | `60` | Timeout in seconds for a single `run_command` invocation (clamped to `1..=300`). |
| `sandbox_disabled` | `false` | Disable the command sandbox (`true` runs unsandboxed; Landlock on Linux, Seatbelt on macOS). |
| `sandbox_extra_readonly_paths` | `[]` | Additional absolute paths granted read+execute access in the sandbox. |
| `enable_xml_rescue` | `true` | Attempt to parse pseudo-XML tool calls when a model emits malformed JSON. |
| `ui_mode` | `"tui"` | Interface mode: `"tui"` (Ratatui) or `"raw"` (streaming stdout). Automatically degrades to raw if stdout is redirected. |
| `session_enabled` | `true` | Enable persistent session bundling in `.marmel/session/`. |
| `session_resume` | `false` | Always request session resume on startup (overridden by `--fresh` or `--resume`). |
| `session_commit_min_interval_ms` | `250` | Minimum throttle interval between snapshot commits. |
| `session_commit_on_tool_result` | `true` | Commit session state immediately after each tool execution. |
| `system_prompt_path` | `prompts/system.md` | Path to a custom Manager system prompt template. |
| `base_prompt` | _unset_ | Additional instructions appended to the Manager prompt. |
| `git_workflow_policy` | `"auto"` | Git workflow mode: `"auto"`, `"off"`, or `"enforce_worktree"`. |
| `[monitoring]` | — | Settings for repetition breakers and token ceilings (`repetition_threshold = 5`, `min_pattern_len = 5`). |
| `[orchestration]` | — | Top-level concurrency and delegation settings (`max_recursion_depth = 3`). |
| `[orchestration.concurrency]` | _unset_ | Concurrency caps per backend URL and model pair. |
| `[orchestration.specialists.<role>]` | — | Per-role configuration overrides (`model`, `backend_url`, `backend_urls`, `tools`, `mcp_servers`, `reasoning_effort`, `max_validator_iterations`). |
| `[mcp_servers.<name>]` | — | External MCP server registration (`command`, `args`, `env`, `url`, `prompt`, `idle_timeout_secs`, `request_timeout_secs`). |

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
# Optional idle timeout in seconds (default: 300, 0 = keep alive indefinitely)
idle_timeout_secs = 300

# Specialist overrides
[orchestration.specialists.coder]
model = "qwen-3.8-27b"
backend_url = "http://localhost:11434/v1"
mcp_servers = ["fs"]
max_validator_iterations = 5

[orchestration.specialists.researcher]
model = "qwen-3.8-flash-next"
tools = ["read_file", "run_command", "grep_search", "glob"]
```

### MCP Servers & On-Demand Lifecycle (`[mcp_servers.<name>]`)

External MCP servers (stdio and HTTP/SSE) are managed on demand with an idle-timeout connection cache:

- **On-Demand:** Server connections are established on demand and do not consume idle resources indefinitely.
- **Connection Cache:** Active connections are cached across consecutive tool calls to avoid launch overhead.
- **Idle Timeout (`idle_timeout_secs`):** Connections inactive for longer than `idle_timeout_secs` (default `300` seconds / 5 minutes) are automatically closed by a background reaper. Set `idle_timeout_secs = 0` to disable idle teardown. Subsequent calls reconnect transparently.
- **Request Timeout (`request_timeout_secs`):** Per-request deadline for tool executions and MCP protocol handshakes (default `180` seconds / 3 minutes). Can also be specified as `timeout_secs` or `timeout`.

### Reasoning Effort (`reasoning_effort`)

While `max_thinking_tokens` serves as a hard **ceiling** on reasoning tokens, `reasoning_effort` acts as an adjustable **dial** forwarded to compatible backends:

- Configurable at the top level (Manager and non-overridden specialists).
- Configurable per specialist (`[orchestration.specialists.<role>].reasoning_effort`).
- Configurable per validator (`[orchestration.specialists.<role>].validator_reasoning_effort`).

```toml
reasoning_effort = "medium"

[orchestration.specialists.coder]
reasoning_effort = "high"

[orchestration.specialists.researcher]
reasoning_effort = "low"
validator_reasoning_effort = "minimal"
```

### Session Concurrency (`[orchestration.concurrency]`)

When running against a local LLM server with limited GPU memory, running multiple subagents, validator loops, and JIT prompt architects in parallel can cause severe queuing and out-of-memory errors. The concurrency table caps simultaneous streams per backend URL and model:

```toml
[orchestration.concurrency]
max_sessions = 4
"http://192.168.50.162:8080/v1" = 1
"https://openrouter.ai/api/v1" = 8
```

- When slots are saturated, sessions queue cleanly and display a **`(Queued n/m)`** indicator in the Subagents panel.
- Fast internal calls (such as the Steer Arbitrator) utilize a dedicated reserved slot so interactive steering remains responsive even under heavy load.

### Git Workflow & Worktrees

Marmel integrates tightly with git to keep changes clean, auditable, and isolated:

- **`auto`** (default): Autodetects repository state and injects workspace branch context into agent prompts.
- **`off`**: Disables git context injection.
- **`enforce_worktree`**: Enforces strict git worktree isolation for subtasks.

When worktree isolation is active, agents create ephemeral worktrees under `.worktrees/<task_id>` inside the workspace. Each specialist operates on an isolated `task/<task_id>` branch, commits as **`"Marmel Coder"`**, and merges changes back into the active feature branch upon task completion.

---

## Usage & Interface

### CLI Flags

| Flag | Description |
|---|---|
| `--config <path>` | Path to an explicit configuration file. |
| `--raw` | Force headless stdout streaming mode (no TUI). |
| `--debug` | Log LLM and tool interactions to `.marmel/debug.log`. |
| `--resume` | Resume the previous run from `.marmel/session/`. |
| `--fresh` | Force a fresh start, ignoring any existing session or unfinished plan. |
| `--keep-session` | Preserve runtime and session files in `.marmel/` upon completion. By default, completed execution plans clean up Marmel's runtime files (`prompts/`, `session/`, `archive/`, `research/`, `tmp/`, `marmel.log*`, `execution_plan.md`) leaving user-authored files intact (or removing `.marmel/` entirely if empty). |
| `-h`, `--help` | Show command-line help. |
| `[PROMPT]` | Optional initial prompt to immediately start the session. |

### Resuming a Previous Run

**Resume is the default when an existing plan is unfinished.** Launching `marmel` with no flags in a workspace containing pending tasks in `.marmel/execution_plan.md` automatically resumes the previous session, restoring transcript history, worker state, and plan progress.

- Pass `--fresh` to explicitly discard the previous state and start over.
- Pass `--resume` to explicitly force rehydration of `.marmel/session/`.

### Interactive TUI

The Ratatui TUI provides a responsive, multi-pane terminal interface:

- **View Modes:** Cycle between four view layouts (`F2` or `F3`–`F5`):
  - **M1 (Transcript):** Full-screen conversation and tool output.
  - **M2 (Plan):** Side-by-side view with Execution Plan.
  - **M3 (Workers):** Side-by-side view with Active Subagents.
  - **M4 (Split):** Comprehensive layout showing Transcript, Plan, and Workers simultaneously.
- **Pane Toggles:** Show/hide the Plan pane with `Ctrl+P`, or Workers pane with `Ctrl+A`.
- **Focus & Scrolling:** Navigate between panes using `Tab` / `Shift+Tab`. Scroll viewport content with `Up`/`Down` or `PageUp`/`PageDown`.

### Keyboard Shortcuts

Press `?` or `F1` (or run `/help`) inside the TUI to view the live keymap:

| Shortcut | Action |
|---|---|
| `Enter` | Send message. |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+Enter` | Insert newline in multi-line prompt. |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo text in input box. |
| `Ctrl+Up` / `Ctrl+Down` | Browse input history. |
| `Tab` / `Shift+Tab` | Accept completion, or cycle focus between panes. |
| `F2` | Cycle view mode (`M4 → M1 → M2 → M3 → M4`). |
| `F3` / `F4` / `F5` | Switch directly to Transcript / Plan / Workers view. |
| `Ctrl+P` / `Ctrl+A` | Show or hide Plan / Workers pane. |
| `Ctrl+T` | Toggle visibility of reasoning and thinking blocks. |
| `Ctrl+F` | Toggle transcript auto-follow (`LIVE` mode). |
| `PageUp` / `PageDown` | Scroll focused pane by 10 lines. |
| `Up` / `Down` | Move cursor in input box, or scroll focused pane. |
| `Home` / `End` | Jump to start / end of text or focused viewport. |
| `Ctrl+X` | Cancel the selected subagent worker (with confirmation). |
| `Ctrl+D` | Open Quit confirmation overlay. |
| `Esc` / `Ctrl+C` | Dismiss overlay, or arm session abort. |

### Slash Commands

Type slash commands directly into the TUI input box:

| Command | Action |
|---|---|
| `/help` | Display keybindings and available slash commands. |
| `/thought` | Toggle display of reasoning and thinking content. |
| `/follow [on\|off]` | Toggle transcript auto-follow. |
| `/plan [on\|off]` | Toggle visibility of the Plan pane. |
| `/workers [on\|off]` | Toggle visibility of the Workers pane. |
| `/mode <name>` | Switch view mode (`transcript`, `plan`, `workers`, `split`). |
| `/error` | Display the last recorded error details. |
| `/cancel <id>` | Cancel a specific worker by task ID or worker key. |
| `/cancel-all` | Cancel all currently executing subagents. |
| `/reset` | Clear the execution plan, session history, and transcript. |
| `/exit` (or `/quit`) | Safely terminate the session and stop all workers. |

### Headless Raw Mode

When invoked with `--raw`, or when standard output is piped into another command or file, Marmel runs in headless raw mode:

```bash
marmel --raw "Analyze src/main.rs and list public functions"
```

Raw mode streams structured, parseable event lines:
```text
[assistant] Analyzing workspace structure...
[tool] glob(pattern="src/**/*.rs")
[tool-result] src/main.rs, src/config.rs
[assistant] Analysis complete...
[done]
```

### Terminal Commands (`run_command` & PTY)

Specialists execute shell commands via `run_command` (bounded by `command_timeout_secs`) or interactive PTY sessions (`pty_spawn`, `pty_write`, `pty_read`):

- **Linux & macOS:** Commands execute inside `/bin/sh` with command sandboxing (Landlock on Linux, Seatbelt on macOS).
- **Windows:** Commands execute via `%COMSPEC%` (`cmd.exe`).
- Clean exit codes and command outputs are preserved even when timeouts or cancellations occur.

### Runtime Directory (`.marmel/`)

Marmel maintains session state and working files in `.marmel/`:

- `.marmel/execution_plan.md`: The active plan and task checklist.
- `.marmel/prompts/`: Synthesized task prompts and validation briefs (`<task_id>.md`).
- `.marmel/session/`: Snapshot bundle for session resumption (`manifest.json`, worker journals).
- `.marmel/marmel.log`: Application log file (with automatic rotation).
- `.marmel/archive/`: Historical copies of completed plans.
- `.marmel/research/`: Internal intermediate research notes created during execution.

> [!TIP]
> **Automatic Workspace Cleanup:** When an execution plan finishes all tasks, Marmel automatically deletes its temporary runtime files upon exit (`prompts/`, `session/`, `archive/`, `research/`, `tmp/`, logs, and execution plan). User-created files in `.marmel/` (such as `marmel.toml` or `skills/`) are always preserved. Pass `--keep-session` to keep runtime snapshots.

---

## Real-Time Steering & Live Dialogue

Unlike traditional batch agents, Marmel allows continuous interactive steering:

- **Interrupt & Pause:** Typing a prompt while subagents are running immediately pauses active streams at the next chunk boundary without losing partial progress.
- **Inquire Mid-Flight:** Ask questions like *"Why did you choose that crate?"* or *"What are you currently testing?"* to get immediate clarification from working agents.
- **Course Correction:** Redirect the approach, cancel unproductive subagents, or reject the plan without restarting the session.
- **Direct Subagent Dialogue:** The Steer Arbitrator can route instructions directly to a specific worker, which receives the notice mid-turn and adjusts its execution accordingly.

---

## Agent Archetypes & Customization

### Built-In Archetypes

Marmel includes 6 built-in specialist archetypes:

| Archetype | Description | Default Capabilities |
|---|---|---|
| **Coder** | System architecture, implementation, refactoring, tests. | File manipulation, shell commands, codebase search, git worktrees. |
| **Researcher** | Codebase reconnaissance, API discovery, documentation. | File reading, search, shell commands, offline ZIM reference search. |
| **Debugger** | Defect isolation, regression analysis, interactive PTY debugging. | File editing, shell commands, interactive pseudo-terminal sessions. |
| **Validator** | Independent auditing, test verification, formal verdicts. | Test execution, read-only inspection, verification verdicts (no direct file editing). |
| **Generalist** | Polymath for cross-cutting logic and complex multi-step tasks. | Unrestricted tool access and nested delegation. |
| **Planner** | High-level workspace reconnaissance and phased plan creation. | Read-only inspection, plan creation, task decomposition. |

### Custom Archetypes (`ARCHETYPES.md`)

Define custom domain archetypes or override built-in roles by adding an `ARCHETYPES.md` file to your project root or `~/.marmel/ARCHETYPES.md`:

```markdown
## SecurityAuditor
description: Vulnerability scanning and security review.
skills: verification, research
tools: read_file, grep_search, glob, run_command, leave_verdict, reply_to_arbitrator
```

### Project Instructions (`AGENTS.md`)

Marmel automatically incorporates repository guidelines into every agent prompt. Place an `AGENTS.md` (or `CLAUDE.md` / `GEMINI.md`) file in your project root or subdirectory to dictate coding style, test commands, and git conventions.

### Custom Skills (`skills/`)

Skills provide modular, reusable domain instructions and tool grants. Create a markdown file or directory under `skills/` or `~/.marmel/skills/`:

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

---

## Offline Reference Search (zim-search + kiwix MCP)

Marmel includes a dedicated workspace crate, `zim-search` (`crates/zim-search`), which indexes and queries offline ZIM archives (such as full Wikipedia or Stack Exchange dumps) via Tantivy/Okapi BM25, serving results over MCP.

- Enables agents to perform offline research against multi-gigabyte knowledge archives without internet access or API rate limits.
- Provides whole articles or ranked passages.
- Includes a standalone web viewer (`zim-search serve`) for human inspection.

### Build, Index, Register

```bash
# 1. Build zim-search
cargo build --release -p zim-search

# 2. Index a Kiwix archive directory
target/release/zim-search index --kiwix-dir /path/to/kiwix --index /path/to/index

# 3. Test search via CLI
target/release/zim-search search --kiwix-dir /path/to/kiwix --index /path/to/index "concurrency"
```

Register the MCP server in `marmel.toml`:

```toml
[mcp_servers.kiwix]
command = "/path/to/target/release/zim-search"
args = ["mcp", "--kiwix-dir", "/path/to/kiwix", "--index", "/path/to/index"]
```

When registered, `marmel` automatically captures the server's MCP `initialize` `instructions` (alongside any optional `prompt` field on `[mcp_servers.<name>]`) and injects `## MCP Server Guidance` into the Manager, Planner, PromptBuilder, and specialist prompts so offline archive search is prioritized automatically before external web lookups.

---

## Security & Sandboxing

Marmel enforces layered security boundaries to protect your host system:

1. **Path Confinement:** All file access tools (`read_file`, `write_file`, `replace`, etc.) strictly validate paths against the workspace boundary in-process on all platforms. Traversal attempts (`../../`) are blocked.
2. **Process Sandboxing (Linux Landlock & macOS Seatbelt):** Terminal commands execute within an unprivileged kernel-enforced sandbox (Landlock LSM on Linux, Seatbelt/SBPL on macOS). Read/write access is restricted to the workspace, temporary directories (`/tmp`), and language caches (`~/.cargo`, `~/.cache`). Sensitive directories (`~/.ssh`, `~/.gnupg`) are strictly inaccessible. A host with no sandbox backend, or an enforcement failure, refuses the command rather than running it unsandboxed.
3. **Windows:** Command-level process sandboxing is currently unsupported; file access tools remain strictly confined to the workspace via in-process path validation.

### Extra Sandbox Read-Only Paths (`sandbox_extra_readonly_paths`)

If your build toolchain resides outside standard system paths (for example, custom Nix stores or shared mounted toolchains), configure extra read-only grants in `marmel.toml`:

```toml
sandbox_extra_readonly_paths = ["/mnt/nix/store"]
```

---

## Testing

Run the test suite:

```bash
cargo test
```

Verify formatting and linter checks:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

---

## License

Marmel, including the `zim-search` crate, is licensed under the [MIT License](LICENSE).

Copyright (c) 2026 Fredrik Andersson.
