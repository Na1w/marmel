# Marmel (Marmennill) — Project Overview & Architecture Map

> **Scope of this document:** a read-only architecture survey of the marmel Rust codebase, covering project purpose, tech stack, entry-point bootstrapping, the configuration model, module topology, the prompt system, and key domain types. All claims cite concrete file paths and line references. No source files were modified.
>
> Primary sources: `Cargo.toml`, `Cargo.lock`, `README.md`, `marmel.toml.example`, `marmel.toml.cloud`, `src/main.rs`, `src/lib.rs`, `src/types.rs`, `src/config.rs`, `src/tool_names.rs`, `src/prompts.rs`, and all files under `prompts/`.

---

## 1. Project Purpose

**Marmel** (crate name `marmennill`, binary `marmel`, version 0.6.0 — `Cargo.toml:1-8`) is an **autonomous agentic coding assistant**: a terminal-driven orchestrator that plans, delegates, executes, and validates multi-step software engineering and research tasks against **any OpenAI-compatible LLM backend** (Ollama, vLLM, OpenRouter, local GPU servers) (`README.md:1-7`).

### 1.1 The problem it solves

Single-shot LLM coding assistants degrade on long, multi-step work: they lose context, repeat themselves, hallucinate APIs, and have no notion of durable progress. Marmel addresses this with a **fractal Manager + Specialist Subagent architecture** (`README.md` "Features"; `src/orchestrator/mod.rs:1-11`):

- A **Manager (`OrchestratorManager`)** decomposes a user goal into a **disk-backed execution plan** at `.marmel/execution_plan.md` (`- [ ] [t-xxx]` checkbox format), delegates every atomic task via `delegate_task`, and synthesizes the final answer. The Manager is *strictly forbidden* from performing domain work itself (`src/orchestrator/mod.rs:249` `guard_no_domain_work`; `prompts/system.md` "Forbidden: NO DOMAIN WORK (REQ-ORCH-001)").
- Each specialist runs in an **isolated context** — it sees only its role prompt, the task brief, and bounded snippets, never the Manager's transcript (`src/agents/mod.rs:156` `IsolatedContext`; `src/agents/runner.rs:8-30`).
- Deliverables are **automatically audited** by a Validator subagent; rejected work is fed back for revision up to `max_validator_iterations` (default 5) (`src/agents/validation.rs:12-16, 22`; `README.md` "Validation loop").
- A **resilience harness** (XML tool-call rescue, semantic repetition detection, text loop breaking) and a **context engine** (BPE token counting, KV-cache prefix preservation, proactive "rebirth" compaction) keep long agent loops productive (`src/harness/monitor.rs:1-18`; `src/manager/context.rs:1-20`).
- **Deep-Freeze crash recovery** journals in-flight delegations to disk so a crashed session rehydrates cleanly (`src/orchestrator/freeze.rs:1-15`).

### 1.2 Target users

Developers and researchers who want an autonomous, terminal-native coding/research agent that runs against **local or self-hosted models** (the shipped `marmel.toml.cloud` targets a local Ollama endpoint at `http://127.0.0.1:11434/v1`), with per-specialist model routing (e.g. a coder on a local GPU, validators on a cheaper model) and optional external tool servers via MCP.

### 1.3 Runtime modes

| Mode | Trigger | Behavior |
|---|---|---|
| Interactive TUI | default when stdout is a terminal | 3-panel Ratatui UI (Chat / Plan / Subagents) (`src/ui/tui/mod.rs:1-6, 37`) |
| Headless raw | `--raw` flag, `ui_mode = "raw"`, or non-terminal stdout (`src/main.rs:71`) | pipe-friendly labelled streaming lines (`[assistant]`, `[tool]`, …) (`src/ui/raw.rs:1-10, 44`) |
| Internal sandbox exec | `marmel --internal-sandbox-exec <cwd> <command>` (`src/main.rs:21-44`) | re-executes a shell command inside the Landlock sandbox (Linux) — used to confine spawned subshells |

---

## 2. Tech Stack

### 2.1 Language & build

| Property | Value | Source |
|---|---|---|
| Language | Rust, **edition 2024** | `Cargo.toml:4` |
| Minimum toolchain | `rust-version = "1.98"` | `Cargo.toml:5` |
| Package / binary | `marmennill` v0.6.0 → binary `marmel` (`src/main.rs`) | `Cargo.toml:1-13` |
| Default run target | `default-run = "marmel"` | `Cargo.toml:6` |
| License (per README) | MIT | `README.md` "License" |
| Repository | `https://github.com/Na1w/marmel.git` | `README.md` "Project metadata" |

### 2.2 Key dependencies (`Cargo.toml:15-49`) with resolved versions from `Cargo.lock` (369 packages total)

| Crate | Declared | Locked (`Cargo.lock`) | Used for |
|---|---|---|---|
| `tokio` | 1.44 (features `full`) | 1.53.1 | Multi-thread async runtime; `main.rs:74-76` builds it explicitly; all I/O, PTY processes, MCP stdio channels |
| `tokio-util` | 0.7 (`rt`) | 0.7.19 | `CancellationToken` for cooperative abort/preemption (`src/orchestrator/bus.rs:8-13`) |
| `futures` / `futures-util` | 0.3.31 | 0.3.34 | `FuturesUnordered` parallel read-only tool execution (`src/manager/loop.rs:8-9`), `StreamExt` for SSE |
| `async-trait` | 0.1.86 | 0.1.92 | Async traits: `Specialist` (`src/agents/mod.rs:193`), `StreamSink` (`src/llm/stream.rs:45`) |
| `reqwest` | 0.13 (`json`, `stream`) | 0.13.4 | HTTP client for the OpenAI-compatible chat-completions backend (`src/llm/client.rs:1`) |
| `eventsource-stream` | 0.2 | 0.2.3 | SSE event parsing for streaming replies and MCP HTTP/SSE (`src/llm/client.rs:4`, `src/mcp/http.rs:6`) |
| `serde` / `serde_json` | 1.0 | 1.0.229 / 1.0.151 | Wire types (`src/types.rs`), config structs, JSON tool arguments |
| `toml` | 1.1 | 1.1.4+spec-1.1.0 | `marmel.toml` parsing (`src/config.rs:157` `load`) |
| `ratatui` | 0.30 | 0.30.2 | 3-panel TUI rendering (`src/ui/tui/mod.rs:25`, `src/ui/tui/render.rs:11`) |
| `crossterm` | 0.29 (`event-stream`) | 0.29.0 | Terminal raw mode, alternate screen, key events (`src/ui/tui/mod.rs:19-24`) |
| `portable-pty` | 0.9 | 0.9.0 | PTY-backed `run_command` and interactive `pty_*` sessions (`src/harness/pty.rs:12`) |
| `tiktoken-rs` | 0.12 | 0.12.0 | `cl100k_base` BPE token counting for the context engine and token accounting (`src/manager/context.rs:14`; `src/llm/client.rs:35-47`) |
| `regex` | 1.11 | 1.13.1 | `grep_search`, plan checkbox parsing (`src/harness/search.rs:9`; `src/manager/phase.rs:9`) |
| `ignore` | 0.4 | 0.4.33 | `.gitignore`-aware file walking for `grep_search`/`glob` (`src/harness/search.rs:10`) |
| `unicode-width` / `unicode-segmentation` | 0.2 / 1.12 | 0.2.2 / 1.13.3 | Terminal width math and grapheme-safe cursor editing in the TUI |
| `uuid` | 1.25 (`v4`) | 1.26.0 | Worker/delegation ids (`src/orchestrator/freeze.rs:11`) |
| `anyhow` / `thiserror` | 1.0 / 2.0 | 1.0.104 / 1.0.69 & 2.0.20 (both in graph) | Application errors (`anyhow`) and typed errors (`thiserror`, e.g. `ToolError`, `src/harness/mod.rs:176`) |
| `tracing` / `tracing-subscriber` | 0.1 / 0.3 (`env-filter`) | 0.1.44 / 0.3.23 | Structured logging to stderr (raw mode) or `.marmel/marmel.log` (TUI) (`src/main.rs:151-176`) |
| `chrono` | 0.4 (`serde`) | 0.4.45 | Timestamps in worker tracking and journals (`src/orchestrator/workers.rs:9-10`) |
| `libc` (unix only) | 0.2 | 0.2.189 | Process-group `SIGKILL`, `getpwuid` home lookup (`src/harness/pty.rs:20`; `src/config.rs:367`) |
| `landlock` (linux only) | 0.4 | 0.4.7 | Kernel-enforced LSM sandbox for spawned shells (`src/harness/sandbox.rs:1-15`) |
| `tempfile` (dev) | 3.17 | 3.27.0 | Temp dirs in tests |
| `wiremock` (dev) | 0.6 | 0.6.5 | Mock OpenAI backend for integration tests (`tests/`) |

---

## 3. Entry Point Analysis

### 3.1 `src/main.rs` — bootstrap sequence (264 lines)

`fn main()` (`src/main.rs:19`) performs the following ordered steps:

1. **Sandbox re-exec short-circuit** (`src/main.rs:21-44`): if argv[1] is `--internal-sandbox-exec`, the process applies `harness::sandbox::apply_sandbox` (Landlock on Linux) and `exec`s `sh -c <command>` in the given cwd (Unix `exec`; spawn/wait elsewhere). This is how child shells inherit kernel confinement.
2. **CLI parsing** (`parse_args`, `src/main.rs:105-145`): `--config <path>`, `--raw`, `--debug`, `-h/--help`, and one optional positional `PROMPT` into `CliArgs` (`src/main.rs:8-17`).
3. **Workspace root** (`src/main.rs:52`): `harness::set_workspace_root(&cwd)` pins all tool path resolution to the current directory (`src/harness/mod.rs:38-60`, with a task-local scoped override `with_workspace_root` at `src/harness/mod.rs:25-33`).
4. **Configuration** (`src/main.rs:54-58`): `config::load(args.config)` resolves and merges the TOML config + env vars, then `config::set_active(cfg.clone())` publishes it into a process-global `RwLock` (`src/config.rs:142-155`) so any module (e.g. the specialist runner, `src/agents/runner.rs:48`) can retrieve it.
5. **Debug logging** (`src/main.rs:60-69`): with `--debug`, `debug_log::init` opens `debug.log` in the workspace (`src/debug_log.rs:14-40`) and records all LLM HTTP traffic and tool I/O.
6. **UI-mode decision & panic hook** (`src/main.rs:71-72`): `use_raw = args.raw || cfg.ui_mode == "raw" || !stdout_is_terminal()`; `setup_panic_hook` (`src/main.rs:151-190`) routes `tracing` to stderr (raw) or to a rotated `.marmel/marmel.log` file (TUI; 5 MiB, 3 backups — `src/main.rs:146-149`) and installs a panic hook that restores the terminal via `ui::restore()`.
7. **Tokio runtime** (`src/main.rs:74-76`): a multi-thread runtime with all features enabled.
8. **MCP boot** (`src/main.rs:78-82`): if `cfg.mcp_servers` is non-empty, `mcp::McpManager::boot` spawns/initializes stdio servers and connects HTTP/SSE servers (`src/mcp/client.rs:369`), then registers the manager globally (`harness::set_mcp_manager`, `src/harness/mod.rs:63`).
9. **Manager construction** (`boot_manager`, `src/main.rs:96-103`): a default `Plan`, shared `HarnessStats`, a `ChatClient::from_config`, and `OrchestratorManager::from_config` (which builds the canonical `SpecialistRegistry` and hydration of `[orchestration]`).
10. **Session run** (`src/main.rs:85-89`): `rt.block_on(ui::raw::run(...))` or `ui::tui::run(...)`; both funnel into `ui::run_session` (`src/ui/session.rs:18`), which wires status/event/steer channels, performs Deep-Freeze recovery, and drives the multi-turn Manager loop.
11. **Teardown** (`src/main.rs:91-92`): `orchestrator::cancel_all()` cancels the global token (`src/orchestrator/bus.rs:24-29`) and the runtime shuts down with a 300 ms grace period.

### 3.2 `src/lib.rs` — library surface (18 lines)

The library crate exposes exactly eleven public modules plus one alias (`src/lib.rs:4-18`):

```rust
pub mod manager;              // turn loop state machine, plan management, context engine
pub use manager as agent;     // backwards-compatibility alias (lib.rs:6)
pub mod agents;               // specialist subagents, live runner, automated verification
pub mod config;               // config schema + TOML loading
pub mod debug_log;            // --debug traffic logging
pub mod harness;              // tool dispatcher + built-in tools
pub mod llm;                  // SSE chat client, stream channel, thinking demuxer
pub mod mcp;                  // MCP client (stdio + HTTP/SSE)
pub mod orchestrator;         // Manager, registry, steer, freeze, bus, workers
pub mod prompts;              // statically embedded prompts
pub mod tool_names;           // canonical tool-name constants
pub mod types;                // OpenAI wire types + tool definitions
pub mod ui;                   // TUI / raw renderers + session loop
```

Note the dual naming: new code says `manager`, older paths (and several doc comments, e.g. `src/harness/workspace.rs:7`) use `agent`.

---

## 4. Configuration Model

### 4.1 Structs in `src/config.rs`

| Struct | Lines | Responsibility |
|---|---|---|
| `Config` | `src/config.rs:92-115` | Resolved runtime configuration: `backend_url`, `auth_token`, `model`, sampling (`temperature`, `top_p`, `frequency_penalty`, `presence_penalty`), `max_context_tokens`, `system_prompt_path`, `preserve_thinking`, `command_timeout_secs`, `max_repetition_threshold`, `enable_xml_rescue`, `ui_mode`, `debug`, `monitoring`, `orchestration`, `mcp_servers` |
| `OrchestrationConfig` | `src/config.rs:14-34` | `max_recursion_depth` (default 3, `DEFAULT_MAX_RECURSION_DEPTH` at line 9), `manager_module`, `specialists` table, manager-level `mcp_servers` |
| `MonitoringConfig` | `src/config.rs:37-60` | Resilience harness: `enabled`, `repetition_threshold` (5), `min_pattern_len` (5), `max_stream_tokens` (32768) |
| `SpecialistConfig` | `src/config.rs:63-90` | Per-role: `module`, `tools` allowlist, optional `model`/`backend_url`/`auth_token`, validator overrides (`validator_model`, `validator_backend_url`, `validator_auth_token`), `max_validator_iterations`, `enable_validator` (aliases `auto_validate`, `enable_validation`), `mcp_servers` |
| `PartialConfig` / `PartialMonitoringConfig` / `PartialOrchestrationConfig` | `src/config.rs:218-254` | Option-field mirrors of the TOML file used for non-destructive merging |

### 4.2 How `marmel.toml.example` / `marmel.toml.cloud` map onto the structs

| TOML key (example files) | Rust target | Notes |
|---|---|---|
| `backend_url`, `auth_token`, `model` | `Config` (`config.rs:93-95`) | Example: `http://localhost:8000/v1` + `llama3.1-8b-instruct`; cloud file: `http://127.0.0.1:11434/v1` + `deepseek-v4-flash:cloud` |
| `temperature`, `top_p`, `frequency_penalty`, `presence_penalty` | `Config` (`config.rs:96-99`) | Sampling forwarded into `ChatRequest` (`src/types.rs:96-101`) |
| `max_context_tokens` | `Config` (`config.rs:100`) | 8192 in example; **128000** in `marmel.toml.cloud`; drives compaction thresholds |
| `preserve_thinking` | `Config` (`config.rs:102`) | Controls `ThinkingDemuxer` tag retention (`src/llm/thinking.rs:53`) |
| `command_timeout_secs` | `Config` (`config.rs:103`) | Default 60 s for `run_command`/PTY (`src/harness/pty.rs:17`) |
| `max_repetition_threshold`, `enable_xml_rescue` | `Config` (`config.rs:104-105`) | Legacy top-level knobs mirrored by `[monitoring]` |
| `ui_mode` | `Config` (`config.rs:106`) | `"tui"` or `"raw"` |
| `[monitoring]` | `MonitoringConfig` (`config.rs:37`) | Example sets `repetition_threshold = 5`, `min_pattern_len = 7`, `max_stream_tokens = 32768` |
| `[orchestration]` `max_recursion_depth` | `OrchestrationConfig` (`config.rs:14`) | 3 in both example files; fractal delegation bound (`src/orchestrator/mod.rs:56`) |
| `[orchestration.specialists.<role>]` | `SpecialistConfig` (`config.rs:63`) | Five roles in both files: `coder`, `debugger`, `researcher`, `validator`, `generalist`; `tools` allowlists (generalist = `["*"]`), `model` (`deepseek-v4-flash:cloud`), `validator_model` (`gemma4:cloud`), `max_validator_iterations = 5`; commented-out per-role `backend_url` (e.g. LAN Ollama `192.168.50.151:11434/v1`) and OpenRouter for the generalist |
| `[mcp_servers.<name>]` | `HashMap<String, McpServerConfig>` (`config.rs:114`; struct at `src/mcp/client.rs:19-28`) | Exactly one transport per server: stdio (`command` + `args` + optional `env`) or remote (`url`) — enforced by convention in `marmel.toml.example:4000-5205` and validated by tests (`config.rs:497-530`) |

### 4.3 Loading pipeline (`src/config.rs:157-255`)

1. Start from `Config::default()` (`config.rs:117-140`: backend `http://localhost:8000/v1`, model `llama3.1-8b-instruct`, `system_prompt_path = prompts/system.md`, `ui_mode = tui`, `enable_xml_rescue = true`).
2. **File resolution** (`resolve_config_path`, `config.rs:185-215`), first match wins: explicit `--config` → CWD `marmel.toml`, `.marmel.toml`, `.marmel/marmel.toml`, `.marmel/config.toml` → home `~/.marmel/marmel.toml`, `~/.marmel/config.toml`, `~/.config/marmel/config.toml`, `~/.config/marmel/marmel.toml`.
3. The file is parsed into `PartialConfig` and **field-wise merged** (`merge`, `config.rs:257-349`); empty strings/absent fields keep defaults; `orchestration.specialists` and `mcp_servers` maps are extended rather than replaced.
4. **Environment overrides** (`config.rs:165-179`): `MARMEL_AUTH_TOKEN`, `MARMEL_BACKEND_URL`, `MARMEL_MODEL`.
5. `expand_paths` (`config.rs:351-364`) resolves `~` and relative `system_prompt_path` against the CWD; `home_dir` (`config.rs:367-388`) falls back to `getpwuid` on Unix.

The merged config is stored twice: once in the `ACTIVE_CONFIG` global for on-demand retrieval, and once cloned into `main`'s local `cfg` for boot wiring.

---

## 5. Module Topology

```
marmel (bin)                          marmennill (lib)
└── src/main.rs                       └── src/lib.rs
    bootstrap: sandbox re-exec,           ├── manager/        (alias: agent)
    config, MCP boot, Manager,            │   ├── loop.rs        turn state machine + Silent Dispatcher
    ui::raw | ui::tui                     │   ├── context.rs     token budget, compaction, rebirth
                                          │   └── phase.rs       MissionPhase, Plan, disk check-off
                                          ├── agents/          specialist subagents
                                          │   ├── mod.rs         Agent enum, IsolatedContext, Deliverable, Specialist trait
                                          │   ├── coder.rs / debugger.rs / researcher.rs / generalist.rs / validator.rs
                                          │   ├── runner.rs      live specialist execution (run_specialist_llm / run_specialist_live)
                                          │   └── validation.rs  automated validator loop + ValidationOutcome
                                          ├── orchestrator/    Manager & coordination
                                          │   ├── mod.rs         OrchestratorManager, delegate(), RecursionDepth
                                          │   ├── registry.rs    SpecialistRegistry (role → worker + tool namespaces)
                                          │   ├── workers.rs     active/completed worker registry (RAII guards)
                                          │   ├── steer.rs       Steer Arbitrator (mid-flight user steering)
                                          │   ├── preemption.rs  model-slot borrowing & stream pause/resume
                                          │   ├── freeze.rs      Deep-Freeze crash journal (.session_frozen.json)
                                          │   ├── bus.rs         global event/status channels + cancellation token
                                          │   └── plan_summary.rs plan progress rendering
                                          ├── harness/         tool execution layer
                                          │   ├── mod.rs         dispatch(), ToolCaller, ToolError, HarnessStats, workspace root
                                          │   ├── fs.rs          read_file / write_file / replace (+ safe path resolution)
                                          │   ├── search.rs      grep_search (ignore crate) / glob
                                          │   ├── pty.rs         run_command + pty_* via portable-pty, process-group kill
                                          │   ├── monitor.rs     XML rescue, semantic repetition, loop breaking
                                          │   ├── sandbox.rs     Landlock LSM confinement (Linux)
                                          │   └── workspace.rs   .marmel/ directory owner (plan, log, archive)
                                          ├── llm/             backend transport
                                          │   ├── client.rs      ChatClient: reqwest SSE, watchdogs, retry, token counters
                                          │   ├── stream.rs      StreamSink/StreamControl, resumable turns, pause/resume
                                          │   └── thinking.rs    [thinking] tag demuxer, recovery adjustments, nudges
                                          ├── mcp/             Model Context Protocol client
                                          │   ├── client.rs      McpManager, stdio JSON-RPC 2.0, McpTool (server__tool)
                                          │   └── http.rs        Streamable HTTP/SSE transport (Mcp-Session-Id)
                                          ├── ui/              presentation
                                          │   ├── session.rs     run_session: channels, recovery, multi-turn loop
                                          │   ├── bridge.rs      renderer sink + steer arbitration events
                                          │   ├── raw.rs         headless labelled streaming
                                          │   ├── tui/           3-panel Ratatui (mod, render, events, formatting)
                                          │   └── helpers.rs     chunk_utf8, subtask/plan formatting
                                          ├── prompts.rs       include_str! prompt registry
                                          ├── types.rs         OpenAI wire types + ToolDef builders
                                          ├── tool_names.rs    canonical tool-name constants
                                          ├── config.rs        schema + TOML/env loading
                                          ├── debug_log.rs     --debug traffic journal
                                          └── widget.rs        standalone widget-definition parser (not declared in lib.rs)
```

### 5.1 One-line responsibility per module

| Module (file) | Responsibility |
|---|---|
| `src/main.rs` | Binary entry: sandbox re-exec, arg parsing, config/MCP/manager boot, UI-mode dispatch, shutdown (`main.rs:19-93`). |
| `src/lib.rs` | Crate root exposing the eleven public modules and the `agent` alias (`lib.rs:4-18`). |
| `src/manager/` | Manager-level core: turn state machine (`loop.rs:41-145`: `MAX_TURNS=100`, `TURN_WATCHDOG_SECS=600`, read/write tool classification), context engine (`context.rs:30-39`: rebirth advisory 80%, compaction 90%→70%, slow-prefill 300 s), and mission phase/plan management (`phase.rs:34-40`: `.marmel/execution_plan.md`, `forced_phase.txt`, transcript). |
| `src/agents/` | The five specialist roles (`Agent` enum, `mod.rs:27-46`), the `Specialist` trait with per-role `tool_namespaces` and `may_recurse` (`mod.rs:193`), isolated-context construction (`IsolatedContext`, `mod.rs:156`), the live runner (`runner.rs:8-68`, test-bypass logic at `runner.rs:23-33`), and the automated validation loop (`validation.rs:12-22`). |
| `src/orchestrator/` | `OrchestratorManager` (`mod.rs:165-231`) owning planning/delegation/synthesis with `RecursionDepth` bounds (`mod.rs:128-146`); `registry.rs` canonical role registry (`registry.rs:37`); `workers.rs` live worker tracking with RAII `ActiveWorkerGuard`; `steer.rs` Steer Arbitrator (decision vocabulary `RespondDirectly`/`AbortImmediately`/`QueueAndContinue`/`ForwardToWorker`/…, `steer.rs:8-19`); `preemption.rs` model-slot borrowing (`preemption.rs:1-9`); `freeze.rs` Deep-Freeze journal (`freeze.rs:25-27`); `bus.rs` global event/status/cancellation; `plan_summary.rs` progress summaries. |
| `src/harness/` | Tool dispatcher and built-ins: `dispatch_for` with per-caller gating (`mod.rs:435`), 10 000-char output cap (`mod.rs:398`), `HarnessStats` counters (`mod.rs:107`); filesystem tools with `resolve_safe_path` confinement (`fs.rs:39-45`); gitignore-aware search (`search.rs:7-9`); PTY command execution with `stty -echo` + `ulimit -f` + process-group `SIGKILL` (`pty.rs:1-20`); resilience monitor (`monitor.rs:1-18`: `XMLToolRescue`, semantic tool-repetition, text loop breaking); Landlock sandbox (`sandbox.rs:17-31`); `.marmel/` workspace owner (`workspace.rs:27-47`). |
| `src/llm/` | `ChatClient` reqwest SSE client with 300 s initial / 60 s inter-chunk / 1200 s overall watchdogs and 3-attempt backoff (`client.rs:11-19`); global token accounting (`client.rs:24-32`); shared stream channel with `StreamControl::{Continue, Abort, Pause}` (`stream.rs:14-40`); `[thinking]`/`<think>`/`<thought>` demuxer with one-turn recovery adjustments (+0.5 frequency penalty, +0.1 temperature) (`thinking.rs:1-16, 51-55`). |
| `src/mcp/` | MCP client: `McpServerConfig` (stdio or remote URL), `McpTool::qualified_name` = `server__tool` (`client.rs:46-48`), `McpManager::boot` (`client.rs:369`), scoped `tools_for_servers` (`client.rs:413`); HTTP/SSE "Streamable HTTP" transport with session-id echo (`http.rs:1-9`). |
| `src/ui/` | `run_session` orchestration of channels, Deep-Freeze recovery and steering history (`session.rs:18-70`); `bridge.rs` steer-arbitration event plumbing (`SteerArbEvent`, `bridge.rs:10-33`); `raw.rs` headless renderer; `tui/` Ratatui 3-panel renderer with focus cycling (`tui/mod.rs:29-37`); `helpers.rs` UTF-8-safe chunking and formatting. |
| `src/prompts.rs` | Compile-time embedding of all 12 prompt files plus a runtime environment block (`prompts.rs:3-14, 18-31`). |
| `src/types.rs` | OpenAI wire types and tool-schema builders (see §7.1). |
| `src/tool_names.rs` | Canonical tool-name constants (see §7.2). |
| `src/config.rs` | Config schema, TOML/env loading, global active config (see §4). |
| `src/debug_log.rs` | `--debug` journal of all LLM HTTP and tool traffic to `debug.log` (`debug_log.rs:1-13`). |
| `src/widget.rs` | Self-contained parser for a declarative widget language (`widget NAME KIND { prop = value }`, `widget.rs:1-40`). **Observation:** it is *not* declared in `src/lib.rs:4-18` and no `crate::widget` references exist elsewhere, so it is currently unwired dead code kept for future renderer use. |

### 5.2 Cross-cutting data flow

1. **User input** → `ui::session::run_session` → Manager turn (`manager::loop::ManagerLoop`).
2. **Manager** calls `create_plan` (writes `.marmel/execution_plan.md`) then `delegate_task` per unchecked `- [ ] [t-xxx]` item (`orchestrator/mod.rs:268` `delegate`, `560` `handle_delegate_task`).
3. **Delegation** resolves the role in `SpecialistRegistry` (`registry.rs:145`), checks `caller_allows_tool` (`orchestrator/mod.rs:702`) and `RecursionDepth::step` (`orchestrator/mod.rs:138`), snapshots to the Crash Journal, builds an `IsolatedContext`, and runs `runner::run_specialist_llm` with the role's prompt and tool allowlist.
4. **Tools** execute through `harness::dispatch_for` (`harness/mod.rs:435`) with RBAC; MCP tools are namespaced `server__tool` and dispatched via the global `McpManager`.
5. **Validation**: `validation::run_automated_validation` (`validation.rs:22`) audits the deliverable with a role-matched validator prompt; `ValidationOutcome::{Approved, Rejected, Aborted}` drives revision loops.
6. **Check-off**: a `MISSION COMPLETE (t-xxx)` marker flips the plan checkbox on disk (`manager/phase.rs:176-235` `MissionMarker`, `Plan::check_plan_on_marker`); the plan is archived when complete.
7. **UI** receives everything through the `orchestrator::bus` event/status channels (`bus.rs:1-45`) as `ui::Event` variants (`ui/mod.rs:47-70`).

---

## 6. Prompt System

### 6.1 Embedding pipeline

`src/prompts.rs` statically embeds every prompt at compile time via `include_str!` (`prompts.rs:3-14`):

| Constant | File | Consumer |
|---|---|---|
| `SYSTEM_PROMPT` | `prompts/system.md` | Manager system prompt; loaded per session via `load_system_prompt_with_plan` (`src/ui/session.rs:33`) and overridable by `system_prompt_path` config |
| `STEER_ARBITRATOR_PROMPT` | `prompts/steer_arbitrator.md` | Steer Arbitrator; also re-exported as `STEER_ARBITRATOR_SYSTEM_PROMPT` in `src/orchestrator/steer.rs:26` |
| `CODER_PROMPT` / `DEBUGGER_PROMPT` / `RESEARCHER_PROMPT` / `GENERALIST_PROMPT` | `prompts/{coder,debugger,researcher,generalist}.md` | Re-exported as `*_ROLE_PROMPT` inside each specialist module (e.g. `src/agents/coder.rs:16`) |
| `VALIDATOR_PROMPT` | `prompts/validator.md` | Generic validator (`src/agents/validator.rs:16`) |
| `VALIDATOR_{CODER,DEBUGGER,RESEARCHER,GENERALIST}_PROMPT` | `prompts/validator_*.md` | Role-matched auditors selected in `validation.rs:24-30` |
| `VALIDATOR_PLANNER_PROMPT` | `prompts/validator_planner.md` | Strategic plan auditing (plan-format enforcement) |

`format_environment_block()` (`prompts.rs:18-31`) injects a `## Workspace & Environment` section (OS, arch, shell, CWD) into agent prompts at runtime.

### 6.2 Role → prompt → allowlist matrix

| Role | Prompt file | Core mandate | Tool allowlist (canonical registry, `src/orchestrator/registry.rs:37`; mirrored in `marmel.toml.example`) |
|---|---|---|---|
| Manager | `prompts/system.md` | Plan, delegate, synthesize; **no domain work** (REQ-ORCH-001); plan is sole source of truth; 2–4 parallel tasks per phase | `delegate_task`, `create_plan`, `archive_current_plan`, `read_file`, `grep_search`, `glob`, `rebirth`, `sleep` (`types.rs:554-566` `manager_tools`) |
| `coder` | `prompts/coder.md` | Elite Software Engineer: architecture, implementation, tests; `write_file` mandatory; delegate deep debugging to `debugger` | `delegate_task`, `write_file`, `replace`, `read_file`, `run_command`, `grep_search`, `glob`, `rebirth`, `sleep` (`src/agents/coder.rs:20-32`; `may_recurse = true`) |
| `debugger` | `prompts/debugger.md` | Crash forensics, PTY GDB/LLDB, ABI/codegen analysis; reproduce before fixing; `[BUG DISCOVERED - REPLAN REQUIRED]` escalation | adds `pty_spawn/write/read/close/list` (`src/agents/debugger.rs:20-38`; `may_recurse = false`) |
| `researcher` | `prompts/researcher.md` | Information retrieval & synthesis; zero-hallucination; plain-text math only in chat | read-only + `run_command` + `delegate_task` (`src/agents/researcher.rs:20-32`) |
| `validator` | `prompts/validator.md` | Independent QA auditor; **read-only**; verdict only via `leave_verdict` tool, never chat text | `delegate_task`, `read_file`, `grep_search`, `glob`, `pty_*`, `leave_verdict`, `rebirth` (`src/agents/validator.rs:24-33`) |
| `generalist` | `prompts/generalist.md` | Cross-domain polymath; universal tool access; `write_file` mandatory for file deliverables | `["*"]` (`src/agents/generalist.rs:20-22`; `may_recurse = true`) |
| Steer Arbitrator | `prompts/steer_arbitrator.md` | Real-time mid-flight steering: `AbortImmediately` / `QueueAndContinue` / `ForwardToWorker` (+ subtask actions `ForwardNotice`/`Cancel`/`DelegateTask`/`Sleep`); language matching; zero filler | Runs as an LLM turn with plan/subtask context (`src/orchestrator/steer.rs:194-207` `SteerContext`) |

Every specialist prompt ends with the terminal-marker discipline (`MISSION COMPLETE`, `FAILED`, `REPLAN REQUIRED`), which `MissionMarker::parse` (`src/agents/mod.rs:110`, `src/manager/phase.rs:202`) uses for automatic plan check-off.

### 6.3 Validator prompt family

The four role-specific auditor prompts (`validator_coder.md`, `validator_debugger.md`, `validator_researcher.md`, `validator_generalist.md`) share the same discipline block — tool-calls only, no chat verdicts, English-only — and differ in their verification workflow (code/architecture analysis, root-cause & ABI checks, fact/source validation, cross-domain reasoning). `validator_planner.md` audits execution-plan structure: `# Execution Plan` header, `- [ ] [t-xxx]` task IDs, phased headers, atomic granularity, mandatory validation steps, and feasibility. Selection logic: `validation.rs:24-30` maps `Agent::Coder → VALIDATOR_CODER_ROLE_PROMPT`, etc., defaulting to `VALIDATOR_ROLE_PROMPT`.

---

## 7. Key Domain Types

### 7.1 `src/types.rs` — OpenAI wire types & tool schemas (694 lines)

| Type | Lines | Purpose |
|---|---|---|
| `Message` (tagged enum) | `types.rs:9-50` | `System`/`User`/`Assistant{content, reasoning_content, tool_calls}`/`Tool{tool_call_id, content}`; serde-tagged by `role` |
| `ToolCall` / `ToolFunction` | `types.rs:52-87` | Assistant tool invocation (`id`, `type` defaulting to `"function"`, `function{name, arguments}`) |
| `ChatRequest` | `types.rs:89-108` | `/chat/completions` body: model, messages, sampling, `stream`, `enable_thinking` (forces reasoning suppression), `tools` |
| `ToolDef` / `ToolFunctionDef` | `types.rs:110-122` | Tool schema envelope; builders for every built-in: `delegate_task` (124), `create_plan` (169), `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `rebirth` (307), `archive_current_plan` (328), `pty_spawn` (343) … `pty_list`, `leave_verdict` (435), `sleep` (461) |
| `ToolDef::minify_json_schema` | `types.rs:489-518` | Strips `$schema`/`title`/`$id`/empty `$defs` from MCP schemas to protect KV-cache efficiency |
| `ToolDef::from_mcp` | `types.rs:520-529` | Wraps an `McpTool` under its `server__tool` qualified name |
| `ToolDef::default_tools` / `manager_tools` | `types.rs:531-569` | Full specialist toolset vs. the Manager's restricted set (no write/PTY tools) |
| `ChatChunk` / `ChunkChoice` / `ChunkDelta` / `ChunkToolCall` / `ChunkToolFunction` | `types.rs:571-610` | SSE streaming chunk deserialization (`reasoning` aliased to `reasoning_content`) |

### 7.2 `src/tool_names.rs` — canonical tool-name constants (61 lines)

Single source of truth for wire-visible tool names, referenced by the harness dispatcher, registry allowlists, and docs (`tool_names.rs:1-9`):

| Constant group | Members (lines) |
|---|---|
| Core tools | `TOOL_DELEGATE_TASK` (12), `TOOL_READ_FILE` (14), `TOOL_WRITE_FILE` (16), `TOOL_REPLACE` (18), `TOOL_RUN_COMMAND` (20), `TOOL_GREP_SEARCH` (22), `TOOL_GLOB` (24), `TOOL_CREATE_PLAN` (26), `TOOL_ARCHIVE_PLAN` (28), `TOOL_REBIRTH` (30) |
| PTY tools | `TOOL_PTY_SPAWN` (32), `TOOL_PTY_WRITE` (34), `TOOL_PTY_READ` (36), `TOOL_PTY_CLOSE` (38), `TOOL_PTY_LIST` (40) |
| Validation & pacing | `TOOL_LEAVE_VERDICT` (42), `TOOL_SLEEP` (44) |
| Namespaced variants | `TERMINAL_READ_FILE` … `TERMINAL_SLEEP` (47-61) — `terminal__*` "caesar-style" aliases kept for compatibility with legacy tool naming |

### 7.3 Orchestration domain types (supporting cast)

| Type | Location | Purpose |
|---|---|---|
| `Agent` enum | `src/agents/mod.rs:27-46` | The five canonical roles (`coder`, `researcher`, `debugger`, `validator`, `generalist`) with `as_str`/`from_str` |
| `DelegationRequest` / `IsolatedContext` / `Deliverable` | `src/agents/mod.rs:132, 156, 185` | Delegation payload, the specialist's isolated view (role prompt + brief + snippets), and the returned work product |
| `Specialist` trait | `src/agents/mod.rs:193` | `name()`, `tool_namespaces()`, `may_recurse()` — implemented by the five role structs |
| `MissionMarker` | `src/agents/mod.rs:80` / `src/manager/phase.rs:176` | `MISSION COMPLETE (t-xxx)` / `FAILED` / `REPLAN REQUIRED` terminal markers |
| `OrchestratorManager` | `src/orchestrator/mod.rs:165-231` | Manager state: client, plan, registry, orchestration config, stats, depth, crash journal, delegation events, cancellation token |
| `RecursionDepth` | `src/orchestrator/mod.rs:128-146` | Fractal depth counter; `step()` rejects delegation beyond the bound |
| `SpecialistEntry` / `SpecialistRegistry` | `src/orchestrator/registry.rs:14, 30` | Role → worker descriptor (module path, tool namespaces, model override) |
| `FreezeSnapshot` / `CrashJournal` | `src/orchestrator/freeze.rs:32, 77` | Deep-Freeze checkpoint (`.session_frozen.json`) + append-only journal (`.session_journal.json`) |
| `SteerDecision` / `SteerSubtaskDecision` | `src/orchestrator/steer.rs:50, 30` | Arbitrator output vocabulary incl. per-subtask actions |
| `ContextEngine` | `src/manager/context.rs:210` | Token budget enforcement: 80% rebirth advisory, 90% compaction to 70%, pinned `messages[0]`/`[1]` (REQ-CORE-001…006, `context.rs:4-20`) |
| `AgentLoop` / `TurnPhase` / `Signal` | `src/manager/loop.rs:152, 47, 78` | Turn lifecycle `PrepareTurn → CallBackend → StreamResponse → ProcessResponse → ExecuteTools → CheckFinish` with parallel read-only tool execution (`loop.rs:1-20`) |
| `ChatClient` / `StreamedReply` | `src/llm/client.rs:67, 58` | SSE client with watchdogs, retries, and global token counters |
| `McpServerConfig` / `McpTool` / `McpManager` | `src/mcp/client.rs:19, 32, 358` | MCP registration, discovery (`tools/list`), execution (`tools/call`) |

---

## 8. Runtime State: the `.marmel/` Directory

Owned by `harness::workspace::Workspace` (`src/harness/workspace.rs:27-47`), which creates and probe-validates the directory at boot:

| Path | Constant | Purpose |
|---|---|---|
| `.marmel/execution_plan.md` | `manager/phase.rs:36` | Disk-backed plan; sole source of truth; auto check-off |
| `.marmel/forced_phase.txt` | `manager/phase.rs:38` | Phase override (`Conversational`/`Executing`) read every turn (REQ-PLAN-004) |
| `.marmel/.session_transcript.json` | `manager/phase.rs:40` | Session transcript for UI rehydration |
| `.marmel/.session_frozen.json` | `orchestrator/freeze.rs:25` | Deep-Freeze in-flight delegation snapshot |
| `.marmel/.session_journal.json` | `orchestrator/freeze.rs:27` | Append-only freeze/recover journal |
| `.marmel/marmel.log` | `harness/workspace.rs:15` | Rotated session log (5 MiB, 3 backups, `main.rs:146-149`) |
| `.marmel/archive/` | `harness/workspace.rs:19` | Completed-plan archive |
| `debug.log` | `debug_log.rs:11` | `--debug` LLM/tool traffic journal |

---

## 9. Testing Layout (brief)

- **Unit tests**: inline `#[cfg(test)]` blocks plus dedicated `*_tests.rs` companions (`harness/fs_tests.rs`, `harness/monitor_tests.rs`, `harness/pty_tests.rs`, `llm/client_tests.rs`, `llm/stream_tests.rs`, `llm/thinking_tests.rs`, `manager/context_tests.rs`, `manager/loop_tests.rs`, `manager/phase_tests.rs`, `mcp/http_tests.rs`, `orchestrator/tests.rs`, `ui/session_tests.rs`, `ui/tui/tests.rs`).
- **Integration tests**: `tests/` (`test_agent.rs`, `test_context.rs`, `test_harness.rs`, `test_llm.rs`, `test_monitor.rs`, `test_orchestrator.rs`, `test_role_gating.rs`, `test_ui_session.rs`, `test_validation_loop.rs`) with `wiremock` mocking the backend; the live runner bypasses network calls under `cargo test` unless `MARMEL_LIVE_TEST` is set (`src/agents/runner.rs:23-33`).

---

## 10. Notable Observations

1. **`src/widget.rs` is unwired** — it defines a declarative widget parser but is not listed in `src/lib.rs:4-18` and is referenced nowhere else in `src/` (verified by grep). It is dormant scaffolding, likely for a future declarative UI.
2. **README version drift** — the README's project-metadata section states version `0.5.0` while `Cargo.toml:3` declares `0.6.0`; the README dependency table also lists `unicode-segmentation 1.13` vs the manifest's `1.12` (locked at 1.13.3). Treat the manifest + lockfile as authoritative.
3. **Two config systems for specialists** — `src/config.rs:63-90` (`SpecialistConfig`) is the authoritative schema; `src/orchestrator/mod.rs:67-100` carries a lighter runtime `OrchestrationConfig` hydrated via `from_config` (line 87) that keeps only the tool allowlists.
4. **Security layering** — in-process path confinement (`resolve_safe_path`, `src/harness/fs.rs:39`) applies on all platforms; Landlock LSM (`src/harness/sandbox.rs`) adds kernel enforcement on Linux via the `--internal-sandbox-exec` re-exec path (`src/main.rs:21-44`).
5. **Manager/toolset asymmetry** — `ToolDef::manager_tools` (`types.rs:554-569`) deliberately omits write/PTY/verdict tools, encoding the "no domain work" rule at the schema level in addition to the prompt-level prohibition and `guard_no_domain_work` (`orchestrator/mod.rs:249`).

---

*Report generated from static analysis of the workspace at commit state described by the files listed in the header. All line numbers refer to the current files on disk.*