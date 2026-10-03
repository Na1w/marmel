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
| `max_thinking_tokens` | `32768` | Maximum reasoning/thinking tokens before forcing continuation. |
| `preserve_thinking` | `true` | Retain thinking blocks in the session transcript. |
| `command_timeout_secs` | `60` | Timeout per terminal command or PTY invocation. |
| `max_repetition_threshold` | `5` | Consecutive identical turns that trigger loop interruption. |
| `enable_xml_rescue` | `true` | Parse pseudo-XML tool calls into valid JSON tool calls. |
| `ui_mode` | `"tui"` | Interface mode: `"tui"` (Ratatui) or `"raw"` (streaming stdout). |
| `debug` | `false` | Write detailed debug logs to `debug.log`. |
| `[orchestration]` | — | Top-level settings: `max_recursion_depth` (default 3), `mcp_servers`. |
| `[orchestration.specialists.<role>]` | — | Per-role overrides for `model`, `backend_url`, `tools`, `mcp_servers`, and validator settings. |
| `[mcp_servers.<name>]` | — | External MCP server registration (`command`, `args`, `env`, `url`). |

### Example `marmel.toml`

```toml
backend_url = "http://localhost:11434/v1"
model = "qwen-3.8-27b"
max_context_tokens = 8192
ui_mode = "tui"

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
3. **Subagents:** Real-time log and deliverables from delegated specialists.

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
| `.marmel/.session_frozen.json` | Checkpoint used for crash recovery and resume. |
| `.marmel/.session_journal.json` | Append-only event journal. |

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

---

## Testing

Run the test suite:

```bash
cargo test
```

The test suite covers:
- Core orchestration and recursive delegation bounds.
- Tool allowlists, role gating, and path confinement.
- Crash recovery and state rehydration.
- Stream preemption and steer arbitration.
- Repetition breakers and XML tool call rescue.

To verify linting and formatting:

```bash
cargo clippy --all-targets --all-features
cargo fmt -- --check
```

---

## License

This project is licensed under the [MIT License](LICENSE).
