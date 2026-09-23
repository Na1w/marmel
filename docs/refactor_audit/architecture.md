# Marmennill (marmel) — Architecture Review & Refactor Targets

> **Scope:** architecture-level review of the `marmennill` crate (v0.9.0, 83 `.rs` files, 34,046 LOC in `src/`).
> This document is the *design* input for the refactor tasks `t-b4s6`–`t-b6u4`. It is built on top of two
> completed audits — **do not re-audit**:
>
> - `docs/refactor_audit/structure.md` — module map, LOC table, cross-module dependency graph, findings F1–F7.
> - `docs/refactor_audit/duplicates.md` — ranked duplication findings §1–§6.
> - `docs/architecture.md` — older background doc (project purpose, boot flow, config model); skimmed only.
>
> **Constraint for the refactor phase: moves + re-exports only.** No logic changes, no signature changes,
> no dependency-direction fixes. Every proposed change in this document must keep `cargo build` green and
> the test suite at baseline parity (370 pass / 1 known failure: `test_handle_delegate_task_rejection_anchoring`).
> Direction changes (breaking cycles, error-policy migration, logging consolidation) are recorded here as
> **future work** only.

---

## 1. Module boundary leaks

### 1.1 The `harness` ↔ `orchestrator` cycle (structure.md §2, F1 — severe)

The low-level tool layer (`harness`) calls *up* into the top-level orchestration module at ~15 sites,
while `orchestrator` simultaneously depends on `harness`:

| Direction | Sites (from structure.md) | What crosses the boundary |
|---|---|---|
| `harness` → `orchestrator` | `harness/mod.rs:250, 312, 362, 441-446, 516, 741, 806-807, 845-847, 898, 982-983, 1002, 1022-1023`; `harness/pty.rs:250, 534` | `handle_delegate_task` (×6), `global_cancellation_token`, `has_active_workers`, `CURRENT_WORKER_TOKEN`, `caller_allows_tool`, `is_current_or_global_cancelled`, `MAX_EXECUTING_ROUNDS` |
| `orchestrator` → `harness` | `orchestrator/mod.rs:33`, `orchestrator/steer.rs:18` | `HarnessStats`, `ToolError`, `ToolResult` |

**Which direction should stay:** `orchestrator → harness`. Orchestration is a policy layer that *uses*
tools; the tool layer must not know about delegation, worker tracking, or role-gating policy. The
architecturally correct layering is:

```
ui ──> orchestrator ──> agents ──> harness ──> mcp / llm-transport
              └────────> manager ─────────────┘
```

**How to break the cycle (FUTURE WORK — explicitly NOT in this moves-only refactor):**
1. Define a `DelegationPolicy` (or `ToolPolicy`) trait in `harness/` (or a new `policy/` module)
   exposing exactly what the dispatcher needs: `handle_delegate(args) -> ToolResult`,
   `allows(agent, tool) -> bool`, `cancelled() -> bool`, `active_workers() -> usize`.
2. Implement that trait in `orchestrator` and inject it into the harness at boot (replacing the
   global lookups `get_mcp_manager`-style, or keeping a single `RwLock<Option<Arc<dyn ToolPolicy>>>`).
3. `harness` then depends only on the trait; `orchestrator` depends on `harness` types as it already does.
   Cycle gone, and `harness` becomes extractable as its own crate.

Do **not** attempt this in `t-b6u4`: the dispatcher call sites are logic, and changing them is
out of scope for moves-only. `t-b6u4` must note the cycle in its report and leave it intact.

### 1.2 Other leaks (all documented in structure.md §2.1; restated here as the leak inventory)

| ID | Leak | Direction that should stay | Fix (future) |
|---|---|---|---|
| F2 | `manager` ↔ `agents`: `manager/loop.rs:31` uses `agents::{Agent, DelegationRequest, Deliverable}`; `agents/mod.rs:204` `IsolatedContext::into_engine` constructs `manager::ContextEngine` | `agents → manager` (agents consume the context engine) | Move `IsolatedContext::into_engine` to a free function in `manager`, or have `agents` return a neutral `IsolatedContext` and let `manager`/`orchestrator` build the engine |
| F3 | `types` → `mcp`: `types.rs:543` `ToolDef::from_mcp(&crate::mcp::McpTool)` | `mcp → types` (protocol layer converts *into* wire types) | Move `from_mcp` into `mcp/` (or `types/tools.rs` behind an adapter taking plain fields); `types` then depends only on `tool_names` |
| F4 | `harness` → `manager::ContextEngine`: `dispatch_with_engine` / `dispatch_for_with_engine` + async twins take `&mut ContextEngine` (`harness/mod.rs:561-954`) | `manager → harness` | Pass a narrow `ToolOutputSink` trait, or move the `*_with_engine` variants up into `manager/loop.rs` |
| F5 | `orchestrator/mod.rs:29-31` re-exports `agents::{Agent, DelegationRequest, Deliverable, IsolatedContext, MissionMarker, Specialist}` | — | Delete the re-exports; callers (`ui/mod.rs`, tests) import from `agents::` directly |
| F5b | Deep-path reach into internals instead of re-exports: `crate::orchestrator::bus::global_cancellation_token()` (`harness/mod.rs:362, 441`), `crate::agents::validation::run_plan_validation` (`harness/mod.rs:264, 846`) | — | Re-export at the module root where the item is semantically owned |
| F6 | `debug_log` is a hidden cross-cutting dependency (10 modules, global mutable logger, coexists with `tracing`) | — | See §5 (error/logging policy) |
| F7 | Global mutable state scattered across layers: `config::set_active/get_active`, `harness::{set_workspace_root, set_mcp_manager}`, `orchestrator::bus` (senders, cancellation tokens, `CURRENT_WORKER_TOKEN`) | — | Treat as a unit in a future `infra/` or explicit DI pass |

### 1.3 UI reaching into internals

`ui` is the top layer and *may* depend down, but today it reaches deep:

- `ui/session.rs` drives the **tool dispatcher directly**: `crate::harness::dispatch_for(&invocation, ToolCaller::Manager)` (lines ~623, 957), `crate::harness::ToolInvocation`, `crate::harness::monitor::HarnessMonitor` + `Intervention` (repetition blocking in-session), `crate::harness::ToolResult::err`.
- `ui/session.rs` reaches into **orchestration cancellation globals**: `crate::orchestrator::{cancel_all, reset_cancellation, is_globally_cancelled}` (lines ~79, 1156, 1160, 426, 1193) and `crate::orchestrator::DelegationEvent`.
- `ui/bridge.rs` reaches into `crate::orchestrator::steer::{SteerContext, arbitrate_steer_context_stream}`, `crate::orchestrator::normalize_steer_decision`, `crate::manager::phase::Plan`, `crate::manager::context::ContextEngine` (via `RendererSink.ctx`).
- `ui/mod.rs:13` imports `crate::orchestrator::DelegationEvent` for the `Event::Delegation` variant — this is the F5 re-export path in action.

**Assessment:** the UI is where the turn loop *should* live (it owns the renderer and the user), so
driving `chat_client_turn` + tool dispatch from `ui/session.rs` is defensible as *composition*, but the
cancellation globals and deep `bus::`/`validation::` paths are leaks. **Future fix:** the session should
receive an `&OrchestratorManager` (it already does) and call methods on it (`mgr.cancel_all()` etc.)
instead of free-function globals; `DelegationEvent` should be imported from its owning module.
Not part of this refactor (would be a signature/API change).

**Refactor-phase rule for `t-b5t2`/`t-b6u4`:** when moving UI code into submodules, keep every
`crate::…` path byte-identical; do not "clean up" imports while moving.

---
## 2. `src/types/` submodule split (concrete)

`src/types.rs` is 731 LOC with exactly **13 pub items** (verified by reading the file):

| Item | Lines | Kind |
|---|---|---|
| `Message` | 10–73 | enum + `role()/content()/reasoning_content()` |
| `ToolCall` | 75–104 | struct + `new()` |
| `ToolFunction` | 105–111 | struct |
| `ChatRequest` | 112–132 | struct |
| `ToolDef` | 133–145 | struct |
| `ToolFunctionDef` | 140–145 | struct |
| `impl ToolDef` | 146–607 | 18 schema builders (`delegate_task`, `create_plan`, `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `rebirth`, `archive_current_plan`, `pty_spawn/write/read/close/list`, `leave_verdict`, `sleep`) + `minify_json_schema` + `from_mcp` + `default_tools` + `manager_tools` |
| `ChatChunk` | 608–613 | struct |
| `ChunkChoice` | 614–619 | struct |
| `ChunkDelta` | 620–629 | struct |
| `ChunkToolCall` | 630–638 | struct |
| `ChunkToolFunction` | 639–643 | struct |
| `mod tests` | 644–731 | 5 tests (4 about `ToolDef::from_mcp`/`ToolCall`, 1 serialization) |

**Note on the suggested buckets:** the task suggested buckets `events/messages`, `plan/marker types`,
`deliverables`, `config-shared`. Reading the file shows `types.rs` contains **only** OpenAI wire DTOs
and `ToolDef` schema builders — there are no event, plan/marker, deliverable, or config types in it
(`MissionMarker`/`Deliverable`/`DelegationRequest` live in `agents/mod.rs`; `DelegationEvent`/`Delegation`
in `orchestrator/mod.rs`; config types in `config.rs`). The actual split is therefore **two submodules**:

### 2.1 `src/types/wire.rs` (~150 LOC)

Pure OpenAI chat-completions request/response DTOs:

- `Message` (+ `impl Message`)
- `ToolCall` (+ `impl ToolCall::new`)
- `ToolFunction`
- `ChatRequest`
- `ChatChunk`, `ChunkChoice`, `ChunkDelta`, `ChunkToolCall`, `ChunkToolFunction`
- Tests: `tool_call_serializes_with_type_function`, `tool_call_deserializes_missing_type_as_function`

### 2.2 `src/types/tools.rs` (~490 LOC)

Tool schema definitions and builders:

- `ToolDef`, `ToolFunctionDef`
- `impl ToolDef` in full: all 18 builders, `minify_json_schema`, `from_mcp`, `default_tools`, `manager_tools`
- Tests: `from_mcp_uses_qualified_name`, `from_mcp_preserves_description_and_schema`, `from_mcp_minifies_schema_metadata`

`tools.rs` keeps `use crate::tool_names::*;` (the 4 raw-name default lists, duplicates.md §1) and the
`crate::mcp::McpTool` import in `from_mcp` (F3 inversion travels with the item — fixing the inversion
itself is future work; see §1.2 F3).

### 2.3 `src/types/mod.rs` — RE-EXPORT FACADE RULE (mandatory)

```rust
//! Wire types for the OpenAI-compatible chat completions API and tool calls.
mod wire;
mod tools;
pub use tools::{ToolDef, ToolFunctionDef};
pub use wire::{
    ChatChunk, ChatRequest, ChunkChoice, ChunkDelta, ChunkToolCall, ChunkToolFunction, Message,
    ToolCall, ToolFunction,
};
```

**Rule:** `types/mod.rs` must re-export **every** `pub` item of every submodule, explicitly named
(not `pub use wire::*` — explicit names make the facade auditable and keep the re-export list as the
single source of truth for the public API). Downstream paths that must keep working (verified by grep):
`types::Message` (70 sites), `types::ToolDef` (10), `types::ToolCall` (10), `types::ChatRequest` (4),
plus `types::ToolFunction` / chunk types via `use crate::types::*` in `llm/` and `mcp/`. Zero call-site
edits are permitted or expected in `t-b4s6`.

---

## 3. Decomposition proposals (moves + re-exports only)

Target: no file > ~800 LOC. Every proposal is a pure move of items into a new submodule plus
`pub use` re-exports at the old path, so `crate::harness::X`, `crate::ui::bridge::X`, etc. all keep
working. Line ranges verified against the current sources.

### 3.1 `src/ui/session.rs` (1,227 LOC) → `src/ui/session/`

The file is one public function, `run_session` (line 19, body to line 1227). A moves-only split means
extracting cohesive blocks of the body into private free functions in sibling submodules (behavior
bit-identical; the function merely delegates). Blocks identified in the body:

| New file | Content (current lines in `session.rs`) | ~LOC |
|---|---|---:|
| `session/mod.rs` | `run_session` entry: setup (lines 19–284: init, freeze-recovery call, transcript load, rehydration, goal, client/stats/stream_cfg) + the outer `while !renderer.aborted()` loop skeleton + shutdown (1223–1227); re-exports of submodules' private helpers | ~350 |
| `session/recovery.rs` | Deep-Freeze recovery block: `recover_frozen` spawn + status/event drain loop + abort handling (lines ~55–115) → `async fn recover_frozen_deliverable(...) -> Option<(String,String)>` | ~70 |
| `session/turn.rs` | Per-turn driver: `turn_count`/`MAX_TURNS` inner loop, status drain, steer/delegation drains, compaction/rebirth advisory, `RendererSink` construction, `chat_client_turn` call + `classify_llm_error` error path (lines ~338–418) | ~150 |
| `session/tools.rs` | Tool-call handling: `tool_calls` extraction, `delegate_task` detection (`delegated_agent`/`delegated_task`), `update_subagent_lifecycle` + `Event::Delegation`, `monitor.observe_tool` + `Intervention::Block/Cut` path, `ToolInvocation` + `dispatch_for` `spawn_blocking` dispatch, handle join + result rendering (lines ~420–700) | ~300 |
| `session/input.rs` | Input & command handling: `is_abort_command`/`is_reset_command`/`handle_reset_command` call sites, steer-queue drain into `ctx`, `read_input` loop, `UiRecord`/`UiTranscript` appends (lines ~700–1222) | ~300 |

`src/ui/session_tests.rs` (433 LOC, attached via `#[path]` in `ui/mod.rs`) moves its `#[path]` to
`session/mod.rs` and is renamed `session/tests.rs` — it tests `run_session` behavior and stays
co-located. `session/mod.rs` keeps `pub async fn run_session` with the identical signature and
`ui/mod.rs` keeps `pub use session::run_session;` (now resolving through `session/mod.rs`).

### 3.2 `src/ui/bridge.rs` (1,467 LOC) → `src/ui/bridge/`

Verified inventory: `SteerArbEvent` (10–30), `SharedSteeringHistory` (32), `spawn_steer_arbitration`
(35–273), `drain_steer_arbitration_events` (274–290), `drain_steer_arbitration_events_with_transcript`
(291–~517), `RendererSink` struct (519–535) + `impl StreamSink for RendererSink` (536–~933),
`mod tests` (935–1467, ~530 LOC).

| New file | Content | ~LOC |
|---|---|---:|
| `bridge/mod.rs` | Re-exports only: `SteerArbEvent`, `SharedSteeringHistory`, `spawn_steer_arbitration`, both `drain_*` fns, `RendererSink` | ~30 |
| `bridge/steer.rs` | `SteerArbEvent`, `SharedSteeringHistory`, `spawn_steer_arbitration` (lines 10–273) | ~270 |
| `bridge/drain.rs` | `drain_steer_arbitration_events`, `drain_steer_arbitration_events_with_transcript` (lines 274–517) | ~250 |
| `bridge/sink.rs` | `RendererSink` + full `StreamSink` impl incl. `on_pause` (lines 519–933) | ~420 |
| `bridge/tests.rs` | The existing `mod tests` (lines 935–1467), attached to `bridge/mod.rs` via `#[cfg(test)] #[path = "tests.rs"] mod tests;` | ~530 |

`RendererSink`'s fields are `pub` and constructed in `session.rs` (lines ~394–407) — the re-export in
`bridge/mod.rs` keeps that construction site unchanged. (If `session/` extraction per §3.1 makes the
fields reachable only via `super::bridge::RendererSink`, the re-export preserves the exact path.)

### 3.3 `src/harness/mod.rs` (1,188 LOC) → submodules

Verified inventory: globals `MCP_MANAGER`/`WORKSPACE_ROOT` + `with_workspace_root`/`set_workspace_root`/
`get_workspace_root`/`set_mcp_manager`/`get_mcp_manager` (19–75), `ToolInvocation` (76–82), `ToolResult`
(83–106), `HarnessStats` (107–166), `ToolCaller` (167–220), `ToolError` (221–231), plan handlers
`write_plan`/`write_plan_async`/`write_plan_internal`/`archive_plan` (232–335), sleep handlers
`handle_sleep`/`handle_sleep_async` (336–476), `block_on_safe` (477–503), dispatch family
`dispatch`/`handle_rebirth`/`dispatch_with_engine`/`apply_tool_output_length_limit`/`dispatch_for`/
`dispatch_for_with_engine`/`dispatch_for_async`/`dispatch_for_async_with_engine`/
`dispatch_manager_async`/`dispatch_specialist_async`/`dispatch_manager`/`normalize_tool_name`/
`dispatch_specialist` (504–1051), `mod tests` (1052–1188, ~135 LOC).

| New file | Content | ~LOC |
|---|---|---:|
| `harness/mod.rs` | Globals + workspace-root/MCP-manager accessors (19–75), dispatch family (504–1051), `block_on_safe`, `mod tests`, re-exports of moved items | ~650 |
| `harness/plan.rs` | `write_plan`, `write_plan_async`, `write_plan_internal`, `archive_plan` (232–335) | ~105 |
| `harness/sleep.rs` | `handle_sleep`, `handle_sleep_async` (336–476) | ~140 |
| `harness/common.rs` | `ToolInvocation`, `ToolResult`, `HarnessStats`, `ToolCaller`, `ToolError` (76–231) | ~155 |

`common.rs` items are `pub` and re-exported from `mod.rs` so `crate::harness::{ToolInvocation,
ToolResult, ToolError, HarnessStats, ToolCaller}` paths (used by `orchestrator`, `ui`, `manager`,
tests) are unchanged. The dispatch family stays in `mod.rs` because it is the module's raison d'être
and already < 800 after the moves. The ~15 orchestrator call sites (F1) stay where they are — cycle
untouched, per §1.1.

### 3.4 `src/harness/monitor.rs` (1,261 LOC) → `src/harness/monitor/`

Verified inventory: `ToolCallRecord` (39–84), `Intervention` (85–96), semantic-JSON helpers
`semantic_json_value_eq`/`is_pagination_tool`/`pagination_only_differs`/`is_consecutive_repeat`/
`strip_pagination`/`semantic_json_eq` (97–181), `XMLToolRescue` + block parsers
`find_next_tool_call_block`/`parse_tool_call_block`/`try_embedded_json`/`try_function_attr`/
`try_legacy_function_block`/`extract_inner_text`/`extract_attribute`/`make_rescued_call`/`uuid_v4`
(182–484), `ToolRepetitionDetector` (485–619), `RepetitionDetector` (620–825), code-pattern heuristics
`is_code_pattern`/`is_code_line`/`is_code_word`/`is_markdown_divider` (826–1069),
`prune_orphan_tool_messages` (1070–1118), `HarnessMonitor` (1119–1258), tests attached via
`#[path = "monitor_tests.rs"]` (600 LOC, separate file).

| New file | Content | ~LOC |
|---|---|---:|
| `monitor/mod.rs` | `HarnessMonitor` (1119–1258), `prune_orphan_tool_messages`, `Intervention`, `ToolCallRecord`, `#[path = "monitor_tests.rs"] mod tests;`, re-exports | ~250 |
| `monitor/xml.rs` | `XMLToolRescue` + all 9 block-parsing helpers (182–484) | ~300 |
| `monitor/repetition.rs` | `ToolRepetitionDetector`, `RepetitionDetector`, semantic-JSON helpers (97–181, 485–825) | ~500 |
| `monitor/code.rs` | `is_code_pattern`, `is_code_line`, `is_code_word`, `is_markdown_divider` (826–1069) | ~240 |

`monitor_tests.rs` moves to `harness/monitor/monitor_tests.rs` with the `#[path]` target updated
(relative to the new `mod.rs` location — same directory, so the attribute text is unchanged).
`crate::harness::monitor::{HarnessMonitor, Intervention, ToolCallRecord, semantic_json_eq,
prune_orphan_tool_messages, XMLToolRescue, ToolRepetitionDetector, RepetitionDetector}` all keep
working via `pub use` in `monitor/mod.rs`. Note: `ui/session.rs` uses `crate::harness::monitor::{HarnessMonitor, Intervention}` — unaffected.

### 3.5 `src/orchestrator/mod.rs` (855 LOC) → submodules

Verified inventory: `OrchestrationConfig` (73–120), `DelegationEvent` (121–133), `RecursionDepth`
(134–155), `Delegation` (156–170), `OrchestratorManager` (171–639, ~470 LOC incl. `new`,
`from_config`, `cancel`, `is_cancelled`, `with_cancellation_token`, `guard_no_domain_work`,
`create_plan`, `delegate`, `recover_frozen`, `apply_check_off`, `run_executing`, `synthesize`,
`abort`), `handle_delegate_task` (640–811), `caller_allows_tool` (812–824), `TASK_LINE_RE` +
`brief_for_task` (825–852), tests via `#[path = "tests.rs"]` (528 LOC, separate file), and the F5
re-exports of `agents` types (29–31).

| New file | Content | ~LOC |
|---|---|---:|
| `orchestrator/mod.rs` | `OrchestratorManager` (171–639) + `#[path = "tests.rs"] mod tests;` + re-exports of moved items + the existing `agents` re-exports (kept — F5 cleanup is future work) | ~520 |
| `orchestrator/delegation.rs` | `OrchestrationConfig`, `DelegationEvent`, `RecursionDepth`, `Delegation` (73–170) | ~100 |
| `orchestrator/delegate.rs` | `handle_delegate_task`, `caller_allows_tool`, `TASK_LINE_RE`, `brief_for_task` (640–852) | ~215 |

`tests.rs` (528 LOC) moves to `orchestrator/tests.rs` (already there — the `#[path]` stays).
`crate::orchestrator::{handle_delegate_task, caller_allows_tool, brief_for_task, DelegationEvent,
Delegation, RecursionDepth, OrchestrationConfig}` unchanged via re-exports. `handle_delegate_task`
returns `Result<ToolResult, ToolError>` — the `harness` import stays in `delegate.rs`, not a new
dependency edge (orchestrator→harness already exists).

### 3.6 `src/agents/validation.rs` (911 LOC) → `src/agents/validation/`

Verified inventory: `ValidationOutcome` (9–28), `is_leave_verdict_tool` (29–38), `parse_verdict_args`
(39–123), `run_automated_validation` (124–148), `run_automated_validation_inner` (149–558, ~410 LOC),
`run_plan_validation` (559–571), `run_plan_validation_inner` (572–835, ~260 LOC), `mod tests`
(836–911, ~75 LOC).

| New file | Content | ~LOC |
|---|---|---:|
| `validation/mod.rs` | `ValidationOutcome`, `is_leave_verdict_tool`, `parse_verdict_args`, `run_automated_validation`, `run_plan_validation`, `mod tests`, re-exports | ~180 |
| `validation/automated.rs` | `run_automated_validation_inner` (149–558) | ~410 |
| `validation/plan.rs` | `run_plan_validation_inner` (572–835) | ~265 |

`crate::agents::validation::{ValidationOutcome, run_automated_validation, run_plan_validation,
parse_verdict_args, is_leave_verdict_tool}` unchanged via re-exports. Note the F5b deep paths
`crate::agents::validation::run_plan_validation` from `harness/mod.rs:264, 846` keep working.
The §6b duplication between the two `*_inner` functions (duplicates.md, ~500 LOC) is **not** touched —
it is a logic change, future work.

---
## 4. Trait review

There are **two** real traits in the crate (verified in code). There is **no `Agent` trait** — `Agent`
is a plain enum (`src/agents/mod.rs:33`) with `as_str`/`from_str`/`FromStr`/`Display`.

### 4.1 `Renderer` (`src/ui/mod.rs:78–105`)

Required (8): `init`, `on_event`, `flush`, `poll_input`, `read_input`, `request_abort`, `aborted`,
`shutdown`. Default (10): `force_flush`, `clear_abort`, `request_user_exit`, `user_exit_requested`,
`set_subagents`, `rehydrate_ui`, `rehydrate_messages`, `rehydrate_subagents`, `set_thinking_budgets`,
`reset_active_agent`.

**Cohesion problems:**
- **Default-method bloat:** 10 of 18 methods are defaults, several of them no-ops. Implementors
  (`TuiRenderer`, `RawRenderer`, 2 test doubles) each re-implement the same abort-flag dance
  (duplicates.md §2: `request_abort`/`aborted`/`clear_abort` copy-pasted 4×, ~30 LOC).
- **`poll_input` vs `read_input` are near-duplicates** (duplicates.md §2): in the TUI, `read_input`
  is a strict superset of `poll_input` with a verbatim ~12-line drain/flush block; in `raw.rs` and
  `TestRenderer` both are `None`; in `RecordingRenderer` they have *opposite* queue semantics
  (`poll_input` drains, `read_input` returns `None`) — a latent behavioral trap.
- **`rehydrate_messages` default reaches into `UiTranscript::from_legacy_messages`** — the trait's
  default method couples the trait to the transcript module; fine today, but it means every
  implementor inherits transcript knowledge.
- **`set_thinking_budgets(&mut self, _cfg: &Config)`** pulls `config::Config` into the trait signature —
  a config leak into the UI abstraction.

**Consolidation suggestions (future work — trait changes are not moves-only):**
1. Split into `Renderer` (lifecycle: `init`/`flush`/`force_flush`/`shutdown`/`on_event`) +
   `InputSource` (`poll_input`/`read_input`/`request_abort`/`aborted`/`clear_abort`/
   `request_user_exit`/`user_exit_requested`) with a shared `InputState { aborted, user_exit }`
   helper struct so the flag dance is written once (duplicates.md §2 consolidation).
2. Move `set_subagents`/`rehydrate_*`/`reset_active_agent`/`set_thinking_budgets` into a third
   `StatefulRenderer` trait (only the TUI needs them) or make them free functions taking
   `&mut dyn Renderer`.
3. `on_pause`-style steering hooks (`RendererSink::on_pause`) already live on the `StreamSink` side —
   keep them there; do not grow `Renderer` further.

### 4.2 `Specialist` (`src/agents/mod.rs:220–256`)

Required (2): `name() -> Agent`, `tool_namespaces() -> &[&'static str]`. Default (2): `run` (a large
~20-LOC default that does cancellation checks, calls `run_specialist_llm`, parses the mission marker),
`may_recurse` (returns `false`).

**Cohesion problems:**
- **The default `run` is the whole execution policy**: no implementor overrides it (all 6 specialist
  impls — coder, debugger, generalist, planner, researcher, validator — only provide `name` +
  `tool_namespaces`). A trait whose core method is 100% default is an *interface for data*, not for
  behavior.
- **`may_recurse` is dead-weight polymorphism** unless a future specialist overrides it (verify
  before removing; grep shows no overrides today).
- **`Agent` enum vs `Specialist::name()`**: the enum and the trait name are coupled — every new
  specialist requires both an enum variant and a struct. That is acceptable (the enum is the
  wire-visible role id), but the duplication of namespace lists in `catalog.rs` (duplicates.md §1)
  means the *same* tool list is encoded in `catalog.rs` literals and in `tool_namespaces()`.

**Consolidation suggestions (future work):**
1. Collapse `Specialist` into a plain struct `SpecialistSpec { name: Agent, namespaces: &'static [&'static str], may_recurse: bool }`
   (or keep the trait but move the default `run` body into `run_specialist_live`, which is where it
   already effectively lives). The 6 one-line impl files (`coder.rs` 60 LOC, `debugger.rs` 60, etc.)
   would become data entries — this also kills duplicates.md §7 (per-file embedded tests).
2. Make `catalog.rs` the single source of truth for tool lists (`base_tool_list()` helper,
   duplicates.md §1) and have `tool_namespaces()` derive from it.
3. `Deliverable`/`MissionMarker`/`IsolatedContext` are *not* trait members but travel with the trait —
   fine as-is; they belong in `agents/` core (already do).

### 4.3 `Agent` (enum, not a trait)

`Agent` (10 variants) + `Display`/`FromStr` + `as_str`/`from_str` is a well-shaped value type. No
action. Note `Specialist::name()` returns `Agent` by value — cheap, fine.

### 4.4 `StreamSink` (llm) — for completeness

`RendererSink` (bridge) and the llm test sinks implement `StreamSink` (`emit`, `is_aborted`,
`poll_control`, `on_pause`). It is small and cohesive; no change.

---

## 5. Error policy

### 5.1 Actual usage (verified by grep)

- **`anyhow`**: 28 files under `src/` use it; 32 `anyhow::Result`, 23 `anyhow::anyhow!`, 11
  `anyhow::Error`, 1 `bail!`, 1 `Context`. It is the de-facto error type at every layer *above* the
  tool boundary: `ui` (all `Result<()>`), `orchestrator` (`OrchestratorManager` methods), `manager`,
  `agents`, `llm/client` (transport errors wrapped), `main.rs`.
- **`thiserror`** (declared in `Cargo.toml:45`): used at exactly **2 sites**:
  - `harness/mod.rs:220` — `ToolError` (4 variants, `#[from] anyhow::Error` on `Execution`).
  - `llm/client.rs:85` — `ChatError` (`pub(crate)`; 4+ variants: `HttpStatus`, `InitialTimeout`,
    `StallTimeout`, `Transport`).
- **`tracing`**: declared (`Cargo.toml:46-47`, with `tracing-subscriber` env-filter) and used
  sporadically: 73× `tracing::warn!`, 23× `info!`, 4× `error!`, 3× `debug!` — mostly in
  `orchestrator/mod`, `freeze`, `agents/validation`, `ui/session`. Meanwhile `debug_log.rs` (446 LOC,
  10 modules, structure.md F6) is a hand-rolled global logger for all I/O traffic.

**Assessment:** the codebase has *already* converged on the right pattern — `thiserror` at the two
places with closed, structured error sets (`ToolError`, `ChatError`) and `anyhow` everywhere else —
but the policy was never stated, so new code picks arbitrarily.

### 5.2 Recommended policy (statement, for future code)

1. **`thiserror` for internal APIs** with a closed set of expected failures: tool dispatch
   (`ToolError`), LLM transport (`ChatError`), and any new module boundary error (e.g. a future
   `DelegationPolicy` error, `FreezeError`). Variants must be matchable by callers.
2. **`anyhow` at the binary boundary and for composition glue**: `main.rs`, `ui/session.rs`,
   `OrchestratorManager` methods, `manager/loop.rs` — anywhere the caller only needs to report/propagate.
   Use `.context(...)` at each layer hop instead of inventing wrapper enums.
3. **Rule of thumb:** if a function's error type is part of its *contract* (callers match on it),
   `thiserror`; if it is *plumbing*, `anyhow`. `ToolError::Execution(#[from] anyhow::Error)` is the
   correct bridge pattern — keep it.
4. **No new error enums** in this refactor (moves-only); this section is guidance for `t-c*` tasks
   and beyond.

### 5.3 `debug_log.rs` → `tracing` migration stance (recommendation only — NOT this refactor)

- **Recommend: migrate, in a dedicated follow-up task, not bundled with the moves.** Rationale:
  - `tracing` is already a dependency, already initialized (`main.rs:153,172`), and already used for
    103 call sites; `debug_log` duplicates structured-logging concerns (structure.md F6, §4 row).
  - `debug_log`'s API (`log_llm_request`, `log_tool_invocation`, `log_user_input`, …) maps 1:1 onto
    `tracing` spans/events: `log_llm_request` → `tracing::debug!(target: "llm.request", …)`, with the
    `--debug` flag becoming an `EnvFilter`/`RUST_LOG` setting in `main.rs`.
  - Doing it *during* the moves would double the churn on every file being moved and make the
    moves-only diff unreviewable.
- **Sequencing:** (a) finish all moves (`t-b4s6`–`t-b6u4`); (b) add `tracing` spans behind the same
  opt-in flag; (c) delete `debug_log.rs` once all 10 calling modules are converted. The `--debug`
  CLI flag and `marmel.log` output format must stay user-visible-compatible (README documents it).

---

## 6. Test placement convention

### 6.1 Convention (adopt going forward)

1. **Unit tests co-locate with the item they test**, in the same module tree:
   - `#[cfg(test)] mod tests` inline in small files, or
   - a sibling `<module>_tests.rs` attached with `#[cfg(test)] #[path = "<module>_tests.rs"] mod tests;`
     (the existing pattern: `monitor_tests.rs`, `steer_tests.rs`, `session_tests.rs`, `tui/tests.rs`).
   When a file splits into a directory (per §3), its test file moves *with the module* into the new
   directory and the `#[path]` attribute is updated to the new relative location.
2. **Integration tests live in `tests/`** — one file per *behavior area* (not per source file):
   `tests/test_agent.rs`, `test_harness.rs`, `test_llm.rs`, `test_monitor.rs`, `test_orchestrator.rs`,
   `test_ui_session.rs`, `test_validation_loop.rs`, `test_role_gating.rs`, `test_dynamic_prompts.rs`,
   `test_specialist_stream.rs`, `test_context.rs`.
3. **Shared fixtures in `tests/common/`** (currently `tests/common/mod.rs`, 23 LOC: `mock_backend()`,
   `completion_sse()`). Extend it with the duplicated fixtures called out in duplicates.md §4
   (`test_manager` factory ×5, SSE payload builders, `RecordingRenderer` base) — **future work**,
   not this refactor.
4. **Rule:** a test that reaches only `crate-internal` (`pub(crate)`) items is a *unit* test →
   co-located. A test that goes through the public `marmennill::` API or spawns a full session is an
   *integration* test → `tests/`.

### 6.2 Mapping: where each sibling `*_tests.rs` belongs after the §3 splits

| Test file (today) | LOC | After refactor | Rationale |
|---|---:|---|---|
| `src/types.rs` inline `mod tests` (644–731) | 88 | `types/wire.rs` (2 ToolCall tests) + `types/tools.rs` (3 `from_mcp` tests) | Tests follow the item they cover |
| `src/ui/session_tests.rs` | 433 | `src/ui/session/tests.rs` (attached from `session/mod.rs`) | Tests `run_session`; module becomes a directory |
| `src/ui/bridge.rs` inline `mod tests` (935–1467) | 533 | `src/ui/bridge/tests.rs` (attached from `bridge/mod.rs`) | Tests `drain_*`/`RendererSink`; module becomes a directory |
| `src/ui/tui/tests.rs` | 2,240 | unchanged (largest file in crate; a *later* task may split it by panel) | Already co-located correctly |
| `src/harness/monitor_tests.rs` | 600 | `src/harness/monitor/monitor_tests.rs` (same `#[path]` text, new directory) | Follows `monitor/` split |
| `src/harness/fs_tests.rs` (291), `pty_tests.rs` (189) | 480 | unchanged | Files not split |
| `src/harness/mod.rs` inline `mod tests` (1052–1188) | 136 | stays in `harness/mod.rs` inline | Dispatch tests stay with dispatch |
| `src/orchestrator/tests.rs` | 528 | unchanged (attached from `orchestrator/mod.rs`, which stays a file) | `OrchestratorManager` stays in `mod.rs` |
| `src/orchestrator/steer_tests.rs` | 528 | unchanged | Untouched module |
| `src/agents/validation.rs` inline `mod tests` (836–911) | 75 | `src/agents/validation/tests.rs` or inline in `validation/mod.rs` | Follows `validation/` split |
| `src/agents/{coder,debugger,planner,researcher,validator}.rs` inline tests | ~35 | unchanged (dedup per duplicates.md §7 is future work) | — |
| `src/manager/{context,loop,phase}_tests.rs` (751/566/553) | 1,870 | unchanged | Untouched modules |
| `src/llm/{client,stream,thinking}_tests.rs` (192/223/200) | 615 | unchanged | Untouched modules |
| `src/mcp/http_tests.rs` | 179 | unchanged | Untouched module |

Integration tests in `tests/` are **not moved** by this refactor; `tests/common/mod.rs` stays as the
fixture home.

---
## 7. Target module tree & ordered execution

Final `src/` layout after all moves (changes vs. today marked; unmarked files are untouched):

```
src/
├── lib.rs                                  (unchanged; `pub mod types;` now resolves to types/mod.rs)
├── main.rs                                 (unchanged)
├── config.rs                               (unchanged)
├── prompts.rs                              (unchanged; move to agents/ is a future task)
├── tool_names.rs                           (unchanged)
├── debug_log.rs                            (unchanged; tracing migration is a future task)
│
├── types/                                  [NEW DIRECTORY — was types.rs, 731 LOC]
│   ├── mod.rs                              [NEW] facade: pub use of every item (see §2.3 rule)
│   ├── wire.rs                             [NEW] Message, ToolCall, ToolFunction, ChatRequest,
│   │                                         ChatChunk, ChunkChoice, ChunkDelta, ChunkToolCall,
│   │                                         ChunkToolFunction + 2 ToolCall tests
│   └── tools.rs                            [NEW] ToolDef, ToolFunctionDef, all 18 schema builders,
│                                             minify_json_schema, from_mcp, default_tools,
│                                             manager_tools + 3 from_mcp tests
│
├── llm/                                    (unchanged: mod, client, stream, thinking, *_tests)
├── mcp/                                    (unchanged: mod, client, http, http_tests)
│
├── manager/                                (unchanged: mod, context, loop, phase, *_tests)
│
├── agents/
│   ├── mod.rs                              (unchanged: Agent, MissionMarker, DelegationRequest,
│   │                                         IsolatedContext, Deliverable, Specialist)
│   ├── catalog.rs / coder.rs / debugger.rs / generalist.rs / planner.rs /
│   │   researcher.rs / validator.rs / prompt_builder.rs   (unchanged)
│   ├── runner/                             (unchanged: mod, assembly, execution, formatting)
│   └── validation/                         [NEW DIRECTORY — was validation.rs, 911 LOC]
│       ├── mod.rs                          [NEW] ValidationOutcome, is_leave_verdict_tool,
│       │                                     parse_verdict_args, run_automated_validation,
│       │                                     run_plan_validation, tests, re-exports
│       ├── automated.rs                    [NEW] run_automated_validation_inner (~410 LOC)
│       └── plan.rs                         [NEW] run_plan_validation_inner (~265 LOC)
│
├── harness/
│   ├── mod.rs                              [SHRUNK] globals + workspace/MCP accessors + dispatch
│   │                                         family + block_on_safe + inline tests + re-exports
│   ├── common.rs                           [NEW] ToolInvocation, ToolResult, HarnessStats,
│   │                                         ToolCaller, ToolError
│   ├── plan.rs                             [NEW] write_plan(_async/_internal), archive_plan
│   ├── sleep.rs                            [NEW] handle_sleep, handle_sleep_async
│   ├── fs.rs / pty.rs / search.rs / sandbox.rs / workspace.rs / *_tests.rs   (unchanged)
│   └── monitor/                            [NEW DIRECTORY — was monitor.rs, 1261 LOC]
│       ├── mod.rs                          [NEW] HarnessMonitor, Intervention, ToolCallRecord,
│       │                                     prune_orphan_tool_messages,
│       │                                     #[path="monitor_tests.rs"] tests, re-exports
│       ├── xml.rs                          [NEW] XMLToolRescue + 9 block-parsing helpers
│       ├── repetition.rs                   [NEW] ToolRepetitionDetector, RepetitionDetector,
│       │                                     semantic-JSON helpers
│       ├── code.rs                         [NEW] is_code_pattern/line/word, is_markdown_divider
│       └── monitor_tests.rs                [MOVED] (from harness/monitor_tests.rs)
│
├── orchestrator/
│   ├── mod.rs                              [SHRUNK] OrchestratorManager + #[path="tests.rs"]
│   │                                         + re-exports (incl. kept agents re-exports, F5)
│   ├── delegation.rs                       [NEW] OrchestrationConfig, DelegationEvent,
│   │                                         RecursionDepth, Delegation
│   ├── delegate.rs                         [NEW] handle_delegate_task, caller_allows_tool,
│   │                                         TASK_LINE_RE, brief_for_task
│   ├── bus.rs / freeze.rs / preemption.rs / registry.rs / steer.rs /
│   │   steer_extractor.rs / plan_summary.rs / workers.rs / tests.rs / steer_tests.rs
│   │                                         (unchanged)
│
└── ui/
    ├── mod.rs                              (unchanged: Renderer trait, Event, SubagentDetail,
    │                                         restore; `pub use session::run_session` still works)
    ├── bridge/                             [NEW DIRECTORY — was bridge.rs, 1467 LOC]
    │   ├── mod.rs                          [NEW] re-exports only
    │   ├── steer.rs                        [NEW] SteerArbEvent, SharedSteeringHistory,
    │   │                                     spawn_steer_arbitration
    │   ├── drain.rs                        [NEW] drain_steer_arbitration_events(_with_transcript)
    │   ├── sink.rs                         [NEW] RendererSink + StreamSink impl
    │   └── tests.rs                        [MOVED] (was inline mod tests, lines 935–1467)
    ├── session/                            [NEW DIRECTORY — was session.rs, 1227 LOC]
    │   ├── mod.rs                          [NEW] run_session (entry + loop skeleton) +
    │   │                                     #[path="tests.rs"] tests + re-exports
    │   ├── recovery.rs                     [NEW] Deep-Freeze recovery block
    │   ├── turn.rs                         [NEW] per-turn driver (stream + LLM call + errors)
    │   ├── tools.rs                        [NEW] tool-call dispatch, monitor intervention,
    │   │                                     delegation lifecycle
    │   ├── input.rs                        [NEW] input/command handling, steer-queue drain,
    │   │                                     transcript appends
    │   └── tests.rs                        [MOVED] (from ui/session_tests.rs)
    ├── raw.rs / transcript.rs / helpers.rs (unchanged)
    └── tui/                                (unchanged: mod, render, events, formatting, tests)
```

### 7.1 Ordered execution (each step independently build-green)

Every step is *one* mechanical move + re-export pass; after each, `cargo build` must be green and
`cargo test` at baseline parity (370 pass / 1 known failure). Order is bottom-up by dependency so no
step ever moves code that another pending step also touches.

| Step | Task | Move | Why safe / green |
|---|---|---|---|
| 1 | `t-b4s6` | `types.rs` → `types/{mod,wire,tools}.rs` | `types` is a leaf (depends only on `tool_names` + `mcp`); 22 consumer files keep working via the facade re-exports; no consumer path changes |
| 2 | `t-b5t2` | `ui/session.rs` → `ui/session/`; `ui/bridge.rs` → `ui/bridge/` | `ui` is the top layer — nothing depends *on* `ui` except `main.rs` (which uses `ui::run_session`, preserved via `ui/mod.rs` re-export); internal `super::` paths in `bridge/*` and `session/*` updated at move time; `session_tests.rs` re-attached via `#[path]` |
| 3 | `t-b6u4`a | `harness/mod.rs` → split out `common.rs`, `plan.rs`, `sleep.rs` | All moved items re-exported from `harness/mod.rs`; consumers (`orchestrator`, `ui`, `manager`, tests) use `crate::harness::X` paths — unchanged; F1 cycle sites untouched |
| 4 | `t-b6u4`b | `harness/monitor.rs` → `harness/monitor/` | `crate::harness::monitor::X` paths preserved by `monitor/mod.rs` re-exports; `monitor_tests.rs` `#[path]` target now local; `ui/session/tools.rs` (step 2) references `crate::harness::monitor::{HarnessMonitor, Intervention}` — unchanged |
| 5 | `t-b6u4`c | `orchestrator/mod.rs` → split out `delegation.rs`, `delegate.rs` | Re-exports preserve `crate::orchestrator::X` for `ui`, `harness` (F1 sites), `manager`; `tests.rs` `#[path]` unchanged; F5 agents re-exports stay in `mod.rs` |
| 6 | `t-b6u4`d | `agents/validation.rs` → `agents/validation/` | `crate::agents::validation::X` paths preserved; deep-path consumers (`harness/mod.rs:264,846`) unchanged; `agents/mod.rs` re-export `pub use validation::ValidationOutcome` unchanged |

**Post-refactor file-size check (target < ~800 LOC):** the largest remaining files are
`orchestrator/mod.rs` (~520), `harness/mod.rs` (~650), `agents/runner/execution.rs` (797 — untouched,
already at target), `ui/tui/render.rs` (1,098) and `ui/tui/tests.rs` (2,240) — the TUI pair is out of
scope for the six named files; flag for a future `t-*` task if the <800 rule is applied crate-wide.

### 7.2 Explicitly deferred (documented, NOT in this refactor)

1. Break `harness` ↔ `orchestrator` cycle via injected `ToolPolicy` trait (§1.1).
2. Break `manager` ↔ `agents` (F2) and `harness → manager::ContextEngine` (F4).
3. Move `ToolDef::from_mcp` out of `types` to kill the `types → mcp` inversion (F3).
4. Delete `orchestrator`'s `agents` re-exports (F5) and fix F5b deep paths.
5. Error-policy enforcement + `debug_log` → `tracing` migration (§5).
6. Renderer trait split + Specialist→struct consolidation (§4).
7. Dedup per `duplicates.md` §1–§7 (tool-name constants, validation-loop driver, SSE pump, test doubles).
8. Split `ui/tui/tests.rs` (2,240 LOC) and `ui/tui/render.rs` (1,098 LOC) if the size rule goes crate-wide.
