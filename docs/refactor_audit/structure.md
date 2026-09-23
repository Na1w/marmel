# Marmennill (marmel) — Structure & Coupling Audit

Audit date: 2026-09-22. Scope: `src/` layout, cross-module dependencies, the deprecated
`agent` alias, misplaced top-level items, and leftover repo-root artifacts.
No source code was modified by this audit.

Crate: `marmennill` v0.9.0 (edition 2024), binary `marmel`. Total: **83 `.rs` files in `src/`, 34,046 LOC**.

---

## 1. Module map of `src/`

### 1.1 Crate root & CLI

| Path | LOC | Purpose |
|---|---:|---|
| `src/lib.rs` | 19 | Declares the 12 public modules + deprecated `pub use manager as agent;` alias (see §3). |
| `src/main.rs` | 264 | CLI entry point (`marmel`): arg parsing, sandbox-exec subcommand dispatch, boot of config/debug-log/session. |

### 1.2 `manager/` — core turn loop & plan state (1,850 LOC incl. tests)

| Path | LOC | Purpose |
|---|---:|---|
| `src/manager/mod.rs` | 12 | Re-exports of context / loop / phase items. |
| `src/manager/context.rs` | 689 | Context engine: cl100k_base token counting, compaction, rebirth. |
| `src/manager/loop.rs` | 784 | Turn state machine driving the agent loop; automatic plan check-off. |
| `src/manager/phase.rs` | 669 | Mission phase state machine, on-disk plan management (`.marmel/execution_plan.md`), auto check-off. |
| `src/manager/{context,loop,phase}_tests.rs` | 751+566+553 | Unit tests for the above. |

Public API: `ContextEngine`, `ContextEngineFactory`, `MissionPhase`, `Plan`, `output_is_success`
(phase), loop re-exports from `r#loop` (see `manager/mod.rs:8`). Phase constants consumed cross-module: `PLAN_FILE`, `FORCED_PHASE_FILE`, `MARMEL_DIR`.

### 1.3 `agents/` — specialist subagents & live runner (4,792 LOC incl. tests)

| Path | LOC | Purpose |
|---|---:|---|
| `src/agents/mod.rs` | 373 | `Agent` role enum, `MissionMarker`, `DelegationRequest`, `IsolatedContext`, `Deliverable`, `Specialist` trait; re-exports. |
| `src/agents/catalog.rs` | 607 | Skill & agent catalog: discovery/resolution of skills and archetypes from disk (`Catalog::discover`). |
| `src/agents/prompt_builder.rs` | 736 | Dynamic prompt builder / agent architect; offline blueprint pre-generation. |
| `src/agents/coder.rs` | 60 | Coder specialist impl. |
| `src/agents/debugger.rs` | 60 | Debugger specialist impl. |
| `src/agents/generalist.rs` | 42 | Generalist specialist impl. |
| `src/agents/planner.rs` | 54 | Planner specialist impl. |
| `src/agents/researcher.rs` | 53 | Researcher specialist impl. |
| `src/agents/validator.rs` | 81 | Validator (quality auditor) specialist impl. |
| `src/agents/validation.rs` | 911 | Automated validator loop, verdict parsing (`leave_verdict`), plan validation. |
| `src/agents/runner/mod.rs` | 96 | `run_specialist_live` entry + re-exports. |
| `src/agents/runner/assembly.rs` | 60 | Deliverable assembly & revision tracking. |
| `src/agents/runner/execution.rs` | 797 | Specialist execution loop / tool-turn state machine. |
| `src/agents/runner/formatting.rs` | 177 | `format_tool_args_full` / `format_tool_args_preview`. |

Public API: `Agent`, `MissionMarker`, `DelegationRequest`, `IsolatedContext` (`into_engine()` →
`manager::ContextEngine`!), `Deliverable`, `Specialist`, `ValidationOutcome`, `run_specialist_live`,
`Catalog`, `AgentArchetype`, `Skill`, `SkillSource`, `PromptBuilder`, `AgentBlueprint`.

### 1.4 `harness/` — tool dispatcher & built-in tools (4,232 LOC incl. tests)

| Path | LOC | Purpose |
|---|---:|---|
| `src/harness/mod.rs` | 1,188 | Tool dispatch (`dispatch`, `dispatch_for`, async variants), `ToolInvocation`, `ToolResult`, `ToolCaller`, `HarnessStats`, sleep/rebirth handlers, MCP manager global. |
| `src/harness/fs.rs` | 345 | `read_file` (paginated), `replace` (strict), `write_file`. |
| `src/harness/pty.rs` | 729 | PTY shell execution with process-group isolation; cancellation checks per chunk. |
| `src/harness/monitor.rs` | 1,261 | Loop monitor: XML rescue, semantic repetition detection, cycle breaking (`HarnessMonitor`, `Intervention`). |
| `src/harness/search.rs` | 195 | `grep_search` (gitignore-aware) and `glob`. |
| `src/harness/sandbox.rs` | 127 | Landlock LSM sandbox (Linux). |
| `src/harness/workspace.rs` | 186 | `.marmel` workspace dir ownership; canonical path resolution (`Workspace`). |
| `src/harness/{fs,pty,monitor}_tests.rs` | 291+189+600 | Unit tests. |

Public API: `ToolInvocation`, `ToolResult`, `ToolError`, `HarnessStats`, `ToolCaller`, `dispatch*`
families, workspace-root globals (`set_workspace_root` etc.), MCP manager global
(`set_mcp_manager`/`get_mcp_manager`), `MAX_TOOL_OUTPUT_CHARS`.

### 1.5 `llm/` — LLM transport (2,576 LOC incl. tests)

| Path | LOC | Purpose |
|---|---:|---|
| `src/llm/mod.rs` | 18 | Re-exports of client / stream / thinking items. |
| `src/llm/client.rs` | 559 | Reqwest SSE chat client with retry & timeout watchdogs; token counters. |
| `src/llm/stream.rs` | 878 | Shared stream channel for Manager + specialist turns; pause/resume/abort control. |
| `src/llm/thinking.rs` | 337 | Thinking demuxer and reasoning-suppression policy. |
| `src/llm/{client,stream,thinking}_tests.rs` | 192+223+200 | Unit tests (wiremock-based). |

Public API: `ChatClient`, `StreamedReply`, token counters, `NullSink`, `VecSink`, `PauseAction`,
`ResumableStreamOutput`, `StreamConfig/Control/Event/Sink/Target`, `TurnStreamHandler`,
`chat_client_turn`, `chat_stream_resumable`, `drive_streamed_turn`, `ThinkingDemuxer`, `DeltaKind`,
`NudgePolicy`, `apply_recovery`, `demux_stream`.

### 1.6 `mcp/` — Model Context Protocol client (1,256 LOC incl. tests)

| Path | LOC | Purpose |
|---|---:|---|
| `src/mcp/mod.rs` | 12 | Re-exports. |
| `src/mcp/client.rs` | 575 | JSON-RPC 2.0 MCP client (stdio + HTTP/SSE), `McpManager`. |
| `src/mcp/http.rs` | 359 | HTTP + SSE transport for remote MCP servers. |
| `src/mcp/http_tests.rs` | 179 | Unit tests. |

Public API: `McpClient`, `McpManager`, `McpServerConfig`, `McpTool`, `HttpSseConnection`.

### 1.7 `orchestrator/` — fractal delegation & steering (4,468 LOC incl. tests)

| Path | LOC | Purpose |
|---|---:|---|
| `src/orchestrator/mod.rs` | 855 | `OrchestratorManager`, `DelegationEvent`, `Delegation`, `RecursionDepth`, `handle_delegate_task`, `caller_allows_tool`, `brief_for_task`. Re-exports agents types. |
| `src/orchestrator/bus.rs` | 90 | Global session event bus, status dispatch, process-wide cancellation token. |
| `src/orchestrator/freeze.rs` | 371 | Deep-Freeze crash recovery journal (SPEC §3.4). |
| `src/orchestrator/preemption.rs` | 578 | Model-slot coordinator & stream preemption. |
| `src/orchestrator/registry.rs` | 204 | `SpecialistRegistry`: role → worker + tool-allowlist mapping. |
| `src/orchestrator/steer.rs` | 582 | Steer Arbitrator: real-time user steering during execution. |
| `src/orchestrator/steer_extractor.rs` | 118 | Incremental streaming JSON extractor for steer responses. |
| `src/orchestrator/plan_summary.rs` | 159 | Plan progress / active-worker summary generation. |
| `src/orchestrator/workers.rs` | 589 | Active-worker registry, token tracking, RAII guards (`ActiveWorkerGuard`). |
| `src/orchestrator/{steer,tests}.rs`, `src/orchestrator/steer_tests.rs` | 635+528 | Unit tests. |

Public API: ~40 items (bus event/cancellation fns, freeze types, preemption sinks, steer arbitration
fns, worker registry fns, `OrchestratorManager`, `SpecialistRegistry`, `generate_plan_progress_summary`).
This is the single largest public API surface in the crate.

### 1.8 `ui/` — rendering & interactive session (7,640 LOC incl. tests)

| Path | LOC | Purpose |
|---|---:|---|
| `src/ui/mod.rs` | 108 | `Renderer` trait, `Event`, `SubagentDetail`, terminal restore; re-exports. |
| `src/ui/bridge.rs` | 1,467 | Bridge between LLM streaming turns, renderer, and steer arbitration. |
| `src/ui/session.rs` | 1,227 | Interactive session runner (multi-turn manager + specialist execution). |
| `src/ui/raw.rs` | 246 | Headless (raw) streaming mode. |
| `src/ui/transcript.rs` | 339 | Persistent UI transcript journal (`UiTranscript`, `UiRecord`). |
| `src/ui/helpers.rs` | 732 | Formatting, command inspection, subagent lifecycle, error classification. |
| `src/ui/tui/mod.rs` | 1,083 | 3-panel Ratatui terminal UI controller. |
| `src/ui/tui/render.rs` | 1,098 | Frame drawing & panel rendering. |
| `src/ui/tui/events.rs` | 698 | Keyboard/mouse/input-editing events. |
| `src/ui/tui/formatting.rs` | 739 | Text parsing, terminal math, word wrapping. |
| `src/ui/{session,tui}_tests.rs` (2 files) | 433+2,240 | Unit tests (the TUI test file is the largest single file in `src/`). |

### 1.9 Top-level single-file modules (see §4 for placement analysis)

| Path | LOC | Purpose |
|---|---:|---|
| `src/types.rs` | 731 | OpenAI-compatible wire types + all tool schema builders (`ToolDef::*`). |
| `src/config.rs` | 578 | Config schema, TOML parsing, path expansion, active-config global. |
| `src/prompts.rs` | 54 | `include_str!`-embedded prompt constants + `format_environment_block`. |
| `src/tool_names.rs` | 61 | Wire-visible tool name string constants (`TOOL_*`, `TERMINAL_*`). |
| `src/debug_log.rs` | 446 | Opt-in debug logging for all I/O traffic (`--debug`). |

### 1.10 Integration tests (`tests/`)

| Path | Purpose |
|---|---|
| `tests/common/mod.rs` | Shared test helpers. |
| `tests/test_agent.rs`, `test_dynamic_prompts.rs`, `test_role_gating.rs`, `test_validation_loop.rs`, `test_specialist_stream.rs` | Agent/specialist behavior. |
| `tests/test_context.rs` | Context engine. |
| `tests/test_harness.rs` | Tool harness dispatch. |
| `tests/test_llm.rs` | LLM client/stream/thinking (wiremock). |
| `tests/test_monitor.rs` | Harness monitor. |
| `tests/test_orchestrator.rs` | Orchestrator end-to-end. |
| `tests/test_ui_session.rs` | UI session (incl. transcript/pause/resume scenarios). |

---

## 2. Cross-module dependency analysis

Import graph derived from all `use crate::…` / `use marmennill::…` statements in `src/`, `tests/`.
Direction = "depends on".

```
main.rs ──> manager(alias: agent!), config, harness, llm, mcp, orchestrator, ui

ui ───────> orchestrator (DelegationEvent, OrchestratorManager), manager (ContextEngine), llm, config, types
orchestrator > agents (Agent, DelegationRequest, Deliverable, MissionMarker, Specialist) — incl. pub re-export at mod.rs:29
            > harness (HarnessStats, ToolError, ToolResult)
            > llm (ChatClient, stream control), manager::phase::Plan, config, tool_names, types
manager ───> agents (Agent, DelegationRequest, Deliverable)      [loop.rs:31]
            > orchestrator (MAX_EXECUTING_ROUNDS, OrchestratorManager, brief_for_task)  [loop.rs:34]
            > harness (HarnessStats, monitor), tool_names, types
harness ───> agents (Agent, validation::{run_plan_validation,…}, Catalog, PromptBuilder)   [mod.rs:171-296, 806, 845-847…]
            > orchestrator (handle_delegate_task ×6, global_cancellation_token, has_active_workers,
                             CURRENT_WORKER_TOKEN, caller_allows_tool, is_current_or_global_cancelled)  [mod.rs:250-1023, pty.rs:250,534]
            > manager (ContextEngine, phase::{Plan, PLAN_FILE, FORCED_PHASE_FILE, MARMEL_DIR})
            > mcp (McpManager global), tool_names, types
agents ─────> manager::ContextEngine (IsolatedContext::into_engine — mod.rs:204)
            > types, tool_names, config (runner/execution, validation), prompts, debug_log
llm ────────> types, config, debug_log
mcp ────────> types (ToolDef::from_mcp — via types.rs:543 `crate::mcp::McpTool`), tool_names, debug_log
types ──────> tool_names (glob import), mcp (McpTool in ToolDef::from_mcp)
debug_log ──> config
prompts ────> (none — pure include_str!)
```

### 2.1 Flagged problems

**F1 — Cyclic dependency: `harness` ↔ `orchestrator` (severe).**
The low-level tool layer depends on the top-level orchestration module at ~15 sites
(`src/harness/mod.rs:250, 312, 362, 441-446, 516, 741, 806-807, 845-847, 898, 982-983, 1002, 1022-1023`,
`src/harness/pty.rs:250, 534`), while `orchestrator` simultaneously depends on `harness`
(`src/orchestrator/mod.rs:33`, `src/orchestrator/steer.rs:18`). This is a hard compile-level cycle broken only
by Rust's module system; architecturally the harness (file/PTY tools) should be below both manager and
orchestrator. The coupling exists because `delegate_task` dispatch, cancellation tokens, worker tracking,
and role-gating all live in `orchestrator`, and the dispatcher must call back up into it.

**F2 — Cyclic dependency: `manager` ↔ `agents`.**
`manager::loop` uses `agents::{Agent, DelegationRequest, Deliverable}` (loop.rs:31) **and**
`orchestrator::OrchestratorManager` (loop.rs:34), while `agents::IsolatedContext::into_engine`
constructs a `manager::ContextEngine` (mod.rs:204). So the "core" manager module is neither below nor above
the agent layer; both pull from each other.

**F3 — Low-level depends on high-level: `types` → `mcp`.**
`src/types.rs:543` (`ToolDef::from_mcp(tool: &crate::mcp::McpTool)`) makes the wire-type module depend on
the MCP protocol module, and `types` is imported by 22 of 83 source files. A conversion function arguably
belongs in `mcp/` or a thin adapter, keeping `types` dependency-free (it would then only depend on
`tool_names`).

**F4 — `harness` → `manager::ContextEngine`.**
Dispatch variants `dispatch_with_engine`, `dispatch_for_with_engine`, and async twins pass
`&mut crate::manager::ContextEngine` (mod.rs:561-954), tying the tool layer to the context-engine type.
Combined with F1, `harness` effectively depends on *all* higher layers (agents, manager, orchestrator).

**F5 — Awkward re-export surface: `orchestrator` re-exports agent types.**
`src/orchestrator/mod.rs:29-31` does `pub use crate::agents::{Agent, DelegationRequest, Deliverable,
IsolatedContext, MissionMarker, Specialist}`, forcing consumers (e.g. `src/ui/mod.rs`, tests) to import core
domain types from the orchestration module — a path through which `ui` transitively pulls agents items via
orchestrator. Callers should import these from `agents::` directly.

**F5b — Cross-layer reach into internals.** Several call sites use deep paths instead of re-exports:
`crate::orchestrator::bus::global_cancellation_token()` (harness/mod.rs:362, 441),
`crate::agents::validation::run_plan_validation` (harness/mod.rs:264, 846).

**F6 — `debug_log` is a hidden cross-cutting dependency.** 10 modules call into it (llm/client ×8 refs,
ui/session ×15, harness/mod ×6, orchestrator/mod ×4, ui/bridge, mcp/client, agents/validation,
agents/runner/execution, main). Not a cycle, but a global mutable-logger style dependency. Two logging
infrastructures coexist: the hand-rolled `debug_log.rs` (all I/O traffic logging) and the declared
`tracing` crate (used only sporadically for warn/error paths in orchestrator/mod, freeze, agents/validation),
with no unified policy about which one to use.

**F7 — Global mutable state scattered across layers.** `config::set_active/get_active`,
`harness::{set_workspace_root, set_mcp_manager}`, `orchestrator::bus` (event/status senders, cancellation
tokens, `CURRENT_WORKER_TOKEN`), and `debug_log` globals. These are the mechanism that makes F1–F2 possible;
any refactor should treat them as a unit.

### 2.2 Healthy layers (for reference)

- `llm` only depends on `types`/`config`/`debug_log` — clean leaf.
- `mcp` is otherwise self-contained (except F3 inversion).
- `ui` sits cleanly at the top (depends down, nothing depends on it except `main.rs`).
- `prompts`, `tool_names` are dependency-free-ish leaves (`types` imports `tool_names`).

---

## 3. Deprecated alias `pub use manager as agent`

Declared in `src/lib.rs:5-7`:

```rust
/// Backwards-compatibility alias for the manager module.
#[deprecated(note = "use crate::manager instead")]
pub use manager as agent;
```

### 3.1 All usage sites (repo-wide grep of `crate::agent::`, `marmennill::agent::`, and bare `use … agent`)

Code usages — **2 sites, both in the CLI binary**:

| File:Line | Usage | Note |
|---|---|---|
| `src/main.rs:4` | `use marmennill::{agent, config, harness, llm, mcp, orchestrator, ui};` | Imports the deprecated alias; this import is what makes the deprecation warning fire on every build. |
| `src/main.rs:97` | `let plan = agent::phase::Plan::default();` | Should be `manager::phase::Plan::default()` (or `crate::marmennill::manager` — as an external consumer of the lib, `marmennill::manager::phase::Plan`). |

Non-code references (doc comments only, stale naming):

| File:Line | Text |
|---|---|
| `src/harness/workspace.rs:15` | `/// Plan file name inside the marmel directory (reused from `agent::phase`).` |
| `src/harness/workspace.rs:19` | `/// Phase-override file name inside the marmel directory (reused from `agent::phase`).` — both now point at `crate::manager::phase::*` on the following lines; comments not updated after the rename. |

Exhaustive-negative check: no occurrences of `marmennill::agent::`, `crate::agent::`, or `use … agent::`
(anywhere outside `agents::`) in `tests/`, `docs/`, `README.md`, `prompts/`, `AGENTS.md`, or `Cargo.toml`.
The integration tests already use the modern `marmennill::manager::*` / `marmennill::agents::*` paths.

**Recommendation:** fix `src/main.rs:4` and `:97`, update the two doc comments in `workspace.rs`, then drop
the alias from `lib.rs`. Only 2 real call sites remain — it is safe to delete outright.

---

## 4. Misplaced / borderline top-level items

| File | Consumers (src files importing it) | Assessment & suggested home |
|---|---|---|
| `src/types.rs` (731 LOC, 36 pub items) | 22 src files + many tests — the crate's most widely imported module | **Borderline but justified as a shared kernel.** However it is a mixed bag: pure wire types (`Message`, `ToolCall`, `ChatRequest`, `ChatChunk`…) *plus* 18 tool-schema builders (`ToolDef::read_file()` etc.) that encode harness-specific knowledge. Suggested split: keep the ~150 LOC of pure wire DTOs as `types` (or move into `llm/` since they model the chat API), and move `ToolDef` + schema builders into the harness layer (e.g. `harness/tool_defs.rs`) or a dedicated `tools` module. Also removes the F3 `types → mcp` inversion if `from_mcp` moves with it. |
| `src/config.rs` (578 LOC) | 16 src files + 4 test files | **Acceptable at root** — genuinely app-wide. Minor issue: contains both the schema (`Config`, `SpecialistConfig`, …) and a process-global (`set_active`/`get_active`). If globals are refactored away (see F7), config becomes a pure leaf module and its placement is unarguable. |
| `src/prompts.rs` (54 LOC) | 5 files only: `agents/catalog.rs`, `agents/prompt_builder.rs`, `agents/runner/execution.rs`, `orchestrator/steer.rs`, `ui/helpers.rs` | **Arguably misplaced.** The consumers are almost all specialist-side; the file is a pure `include_str!` bag. Fits best as `agents/prompts.rs` (keeping shared system/steer constants there too, since only steer.rs and ui reference them — or leave those two as root-level consts in `prompts.rs`). Low-priority cleanup. |
| `src/tool_names.rs` (61 LOC) | 23 src files (second-most imported after types) | **Justified at root** *as-is* only because it's a dependency-free constant leaf used by every layer including `types`. If `ToolDef` builders move out of `types` (§4/types row), consider folding `tool_names` into the new `tools` module and keeping the name constants there; currently fine. |
| `src/debug_log.rs` (446 LOC) | 10 files across llm, harness, orchestrator, agents, mcp, ui + main | **Cross-cutting global, not a module of any layer.** Two options: (a) keep as root-level infra but consolidate with the `tracing`/`tracing_subscriber` stack (initialized in `src/main.rs:153,172` — see F6), or (b) nest under a small `infra/`/`log/` module. Its own API (`log_llm_request`, `log_tool_invocation`, …) duplicates structured-logging concerns that `tracing` already provides. |

---

## 5. Leftover / scratch artifacts at repo root

Checked against `git ls-files` (118 tracked files total) and `git status --porcelain`.

| Artifact | Size | Git state | Assessment |
|---|---:|---|---|
| `example_failure/` | 5.5 MB | **Untracked, NOT gitignored** — contains `debug.log`, `execution_plan.md`, `marmel.log` (a full failed-run capture) | Scratch artifact from a failure investigation (dated Sep 7). At risk of accidental commit; should be deleted or moved to docs with the logs stripped. |
| `execution_failure_2/` | 524 KB | **Untracked, NOT gitignored** — same file set (`debug.log`, `execution_plan.md`, `marmel.log`) | Second failure capture (Sep 7). Same treatment as above. |
| `.idea/` | 28 KB | Ignored (`.gitignore` lists `.idea/`; files: modules.xml, vcs.xml, wip.iml, workspace.xml) | JetBrains project dir; correctly ignored — no action needed beyond leaving it local. |
| `marmel.toml`, `marmel.toml.cloud` | 1.9 KB / 2.3 KB | **Tracked** (`git ls-files` shows both) | Local/config variants committed to git alongside the canonical template `marmel.toml.example` (also tracked). `.gitignore` ignores `/.marmel.toml` and `/config.toml` but not these names — the local configs appear to have been committed intentionally, which defeats the purpose of an `.example` template. Decide: track only `marmel.toml.example`, or rename locals into the ignored patterns. |
| `execution_plan.md`, `execution_plan.completed.{1,2}.md` | ~8 KB each | **Untracked** | Kvaser session planning files dropped in the repo root; no `.gitignore` entry covers them — either add an ignore pattern (e.g. `execution_plan*.md`) or keep out of the repo. |
| `.kvaser/` | — | **Untracked, not ignored** | Session metadata dir; should be added to `.gitignore` alongside `.marmel/`. |
| `target/`, `.marmel/` | — | Ignored | Build output / runtime workspace; correctly handled. |

Note: `*.log` is in `.gitignore`, so the *logs* inside the failure dirs would be ignored, but the directories
themselves (and their `execution_plan.md` files) are not covered by any pattern.

---

## 6. Top findings (ranked)

1. **`harness` ↔ `orchestrator` hard cycle** (§2 F1) — the tool layer calls up into orchestration at ~15 sites; this is the single biggest structural smell and blocks clean extraction of the harness as a standalone module/crate.
2. **`manager` ↔ `agents` mutual dependency** (§2 F2) plus `harness → manager::ContextEngine` (F4): there is no acyclic "bottom" below `types`/`tool_names`; four modules (harness, agents, manager, orchestrator) form a densely interlinked core.
3. **Deprecated `agent` alias has only 2 live call sites** (`src/main.rs:4`, `:97`) — trivial to remove entirely once those two lines and two stale doc comments are fixed (§3).
4. **`types.rs` is overloaded**: pure wire DTOs mixed with harness-specific tool schema builders, plus an inversion into `mcp` (§2 F3, §4).
5. **Duplicated logging infrastructure**: hand-rolled `debug_log.rs` coexists with the declared-but-unused-in-practice `tracing` dependency (§2 F6).
6. **Repo hygiene**: untracked-and-unignored failure-capture dirs (`example_failure/`, `execution_failure_2/`) and session scratch files at root; local `marmel.toml` variants tracked despite an `.example` template existing (§5).
