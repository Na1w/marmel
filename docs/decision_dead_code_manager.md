# Decision record — dead vs. live code in `src/manager/` (recon H7 + H8)

**Task:** t-031f (Phase 4a gate). **Scope touched:** `src/manager/loop.rs`,
`src/manager/mod.rs` (re-export list only, required by the deletions),
`src/manager/context.rs`, `src/manager/context_tests.rs`,
`src/manager/loop_tests.rs` (deleted), `tests/test_context.rs` (kept, unchanged),
this document. **Not touched:** `src/ui/**`, `src/harness/monitor/**`,
`src/agents/**`, `src/manager/phase.rs`, `src/orchestrator/**` — those files are
owned by other Phase 4a tasks; findings about them are recorded as *hand-offs* in
§6, not acted on here.

> Line references are **as of this change** and were re-anchored by symbol name
> (the recon's numbers had already drifted, e.g. `ui/session.rs` was refactored
> and `src/ui/session/tools.rs` no longer exists). Other agents are editing
> `src/ui/**` concurrently, so re-anchor by symbol if a ref has moved.

---

## 1. Decision rule used

For every symbol named in `docs/recon_bugs_manager.md` H7/H8:

* **(a) non-test caller?** — `grep -rn "\b<symbol>\b" src tests --include='*.rs'`
  plus a construction-site grep (a type is only "live" if something *constructs
  or calls* it, not just comments about it).
* **(b) required by a documented REQ-\*?** — the requirement text in the module
  headers of `src/manager/{context.rs,loop.rs}` (REQ-CORE-001…006,
  REQ-LOOP-001…004, REQ-PLAN-002). A symbol is only "required" if the *live*
  requirement behaviour is not already satisfied elsewhere.
* **(c) decision** — WIRE (connect into the real runtime path with minimal,
  honest wiring) or DELETE (symbol + its tests + its now-orphaned helpers).
  No `#[allow(dead_code)]`, no `#[cfg(test)]` masking.

---

## 2. The live executor path (established first, per the task brief)

| Responsibility | Live implementation (evidence) |
|---|---|
| Manager interactive turn loop (turn budget, steer drain, compaction/advisory, parallel read fan-out, sequential write dispatch, tool-result append, transcript save) | `src/ui/session.rs` — turn loop at `:196-205` (`turn_count > crate::manager::r#loop::MAX_TURNS`), compaction/advisory at `:230-236`, `:337-343`, `:1105-1111`, parallel-read gate at `:383-384` (`crate::manager::is_read_tool`), sequential tool loop with abort break at `:725`, check-off at `:689`, `:1061` |
| Specialist turn loop (LLM call, XML rescue, repetition gate, tool dispatch, cancellation) | `src/agents/runner/execution.rs` (`:234` `monitor.rescue_xml`, `:260` `monitor.feed_text`, `:691-694` compaction/advisory, `:554`/`:653` abort) and `src/agents/runner/fix_loop.rs:515-518` |
| Silent-dispatch delegation (`delegate_task` → one task per call, auto check-off) | `src/orchestrator/delegate.rs::handle_delegate_task` (`:26` arg parse, `:47` mandatory `task_id`, `:67-80` re-delegation guard) → `OrchestratorManager::delegate` (`src/orchestrator/mod.rs:190+`) |
| Manager-side marker check-off double gate | `src/orchestrator/mod.rs:351` (`Plan::check_plan_on_marker`) |
| Rebirth entry point | `src/harness/mod.rs:186` (`engine.perform_rebirth`) |
| Token accounting consumers | `src/llm/client.rs:42` (`count_assistant_tokens`), `:263` (`count_tokens`), `src/ui/session.rs:670,1042`, `src/ui/tui/events.rs:785` (`count_text_tokens`) |

**Reachability facts (greps run in this task):**

* `AgentLoop` / `ManagerLoop`: constructed **only** in `src/manager/loop_tests.rs`.
  Outside `src/manager/` the only hits are prose comments
  (`src/harness/monitor/mod.rs:81`, `src/orchestrator/mod.rs:101`).
* `MAX_TURNS` and `is_read_tool` (same file, `src/manager/loop.rs`) **are** live:
  `src/ui/session.rs:200` and `:384`.
* `compact_with_retry` / `compact_context` / `force_compact_context` /
  `compact_to_target` / `compaction_retry_count` / `reset_compaction_retry_count` /
  `inject_context_limit_exceeded` / `CONTEXT_LIMIT_EXCEEDED_MESSAGE` /
  `COMPACTION_RETRY*` / `COMPACTION_OVER_LIMIT_TARGET_RATIO`: **zero** non-test
  callers. The live compaction call sites are only
  `should_compact()` + `compact()` (see table above).
  → the recon's guess that "`compact_with_retry` is likely the live one" is
  **false**; the live escalation does not exist.
* `SlowPrefillTracker` / `record_prefill` / `prefill_tracker{,_mut}` /
  `SLOW_PREFILL_THRESHOLD_SECS` / `MIN_TURNS_AFTER_REBIRTH`: **zero** non-test
  callers. The only internally-reachable member (`note_rebirth`) was reached only
  from `perform_rebirth` and only to reset a tracker nothing ever feeds.
* `context::utf8_safe_slice`: **zero** non-test callers. It is not the shared
  helper — `src/text_util.rs` exposes `truncate_chars` /
  `truncate_with_ellipsis` (`:24`, `:40`), and the live char-boundary work uses
  `is_char_boundary`/`floor_char_boundary` loops (`src/markers.rs:351,378`,
  `src/ui/helpers.rs:94`, `src/harness/mod.rs:217,221`,
  `src/orchestrator/workers.rs`). So REQ-CORE-006 is satisfied without it.
* Compaction **constants** — the recon is right that they are partially live:
  `REBIRTH_ADVISORY_TRIGGER_RATIO` / `COMPACTION_TRIGGER_RATIO` /
  `COMPACTION_TARGET_RATIO` are live (they back `rebirth_advisory_threshold`,
  `compaction_threshold`, `compaction_target`, used by `should_compact()` /
  `compact()`); only the *retry-escalation* constants were dead.

---

## 3. Decision table

| Item (as named in recon) | (a) non-test caller | (b) REQ-\* requirement | Decision | Rationale |
|---|---|---|---|---|
| `loop.rs::AgentLoop` (`run_turn`, `run_turn` phase machine, `execute_tools`, `check_off`, `drain_signals`, `push_message`/`enqueue_tools`, `monitor`, `abort_flag`, `track_pty_pid`, `abort_pty_process_groups`) | **None** — only `loop_tests.rs` constructs it | REQ-LOOP-001…004 / REQ-PLAN-002 **are** implemented by the live loops (§2): turn budget `ui/session.rs:196-205`, steer drain `ui/session.rs:192,226,351,1152`, abort `renderer.aborted()` + `CancellationToken`, parallel reads gate `:383-384`, check-off `:689,1061` + `orchestrator/mod.rs:351` | **DELETE** | A second, never-instantiated executor. Its `CallBackend`/`StreamResponse`/`ProcessResponse` phases were empty no-ops ("the real LLM call lives in the UI/backend layer"), so it cannot be wired without re-writing the live loop around it — that is speculative growth, not honest wiring. Wiring would also mean editing `src/ui/**`, which this task does not own. |
| `loop.rs::ManagerLoop` (`run_executing`, `signal`, `drain_signals`, `track_pty_pid`, test seams `with_delegate_timeout`/`with_delegate_override`, `TestDelegateFn`) | **None** — only `loop_tests.rs` | REQ-ORCH-001/005 delegation is live via `handle_delegate_task` → `OrchestratorManager::delegate`; the *round-loop* variant `OrchestratorManager::run_executing` (`orchestrator/mod.rs:387`) is itself only called from `orchestrator/tests.rs:437` (hand-off in §6) | **DELETE** | Near-copy of `OrchestratorManager::run_executing` minus the domain-work guard (`guard_no_domain_work`) and worker cancellation — i.e. strictly worse duplicate. Its 30-min `DELEGATE_TIMEOUT_SECS` and round loop never ran in the binary. |
| `loop.rs::Signal`, `TurnOutcome`, `TurnPhase` (+`next()`), `PendingTool`, `extract_task_id`, `TASK_ID_RE`, `is_write_tool`, `TURN_WATCHDOG_SECS`, `DELEGATE_TIMEOUT_SECS` | **None** (all only in `loop.rs` + `loop_tests.rs`); `is_write_tool` had no caller outside `AgentLoop::run_turn` | REQ-LOOP-003's read/write split survives through `is_read_tool`; REQ-LOOP-002's turn-limit half survives through `MAX_TURNS` | **DELETE** | Orphaned by the two deletions above. `is_write_tool(x)` was only `!is_read_tool(x)`. `TURN_WATCHDOG_SECS`/`DELEGATE_TIMEOUT_SECS` were constants *applied only inside the deleted loops*; keeping them would be false assurance — recorded as a live gap in §5 (H5). |
| `loop.rs::MAX_TURNS` | **LIVE** — `src/ui/session.rs:200` | REQ-LOOP-002 | **KEEP** (in `loop.rs`, path `crate::manager::r#loop::MAX_TURNS` preserved so `src/ui/session.rs` needed no edit) | Deleting it would break the live turn cap. |
| `loop.rs::is_read_tool` | **LIVE** — `src/ui/session.rs:384` | REQ-LOOP-003 | **KEEP** + new unit tests (`manager::r#loop::tests::*`) | Only the surviving helper needed regression cover after `loop_tests.rs` was deleted. |
| `context.rs::compact_with_retry`, `compact_context`, `force_compact_context`, `compact_to_target`, `compaction_retry_count` field + accessor, `reset_compaction_retry_count`, `inject_context_limit_exceeded`, `CONTEXT_LIMIT_EXCEEDED_MESSAGE`, `COMPACTION_RETRY_CAP`, `COMPACTION_RETRY1_RATIO`, `COMPACTION_RETRY2_RATIO`, `COMPACTION_OVER_LIMIT_TARGET_RATIO` | **None** (grep, §2) | REQ-CORE-003 as *documented* (> 90 % trigger, 70 % target, pins `[0]`/`[1]`, prunes orphan `tool` messages) is fully implemented and live by `should_compact()` + `compact()` + `prune_orphan_tool_messages`. The retry escalation is caesar-parity extra, **not** in the requirement text | **DELETE** | Wiring it would mean adding hard-limit-overflow compaction inside `src/ui/session.rs` / `src/agents/runner/*` (other agents' files) **and** would change token-budget behaviour at five live call sites — explicitly out of bounds for this task ("do not change compaction thresholds or token-budget semantics"). If Phase 4a wants the safety net, it should be specified and implemented in the live loop, not resurrected here. The module doc now states the gap instead of implying coverage. |
| `context.rs::SlowPrefillTracker` (+ `record_prefill`, `note_rebirth`, `turns_since_rebirth`, `ContextEngine::prefill` field, `prefill_tracker()`, `prefill_tracker_mut()`, `SLOW_PREFILL_THRESHOLD_SECS`, `MIN_TURNS_AFTER_REBIRTH`, `use std::time::Duration`) | **None** | REQ-CORE-005 (slow-prefill cooling) — real requirement, but it needs *prefill-duration instrumentation at the LLM call sites* (`src/llm/**`, `src/agents/runner/**`, `src/ui/**`), none of which exists and none of which this task owns | **DELETE**, and record REQ-CORE-005 as **not implemented** in the module doc | Wiring would require inventing a prefill-timing call path through three other agents' files — speculative feature growth, and outside the declared scope. |
| `context.rs::utf8_safe_slice` | **None** | REQ-CORE-006 is satisfied at the call sites by `src/text_util.rs` + `is_char_boundary`/`floor_char_boundary` loops (§2) | **DELETE** | It is a fourth private copy of the boundary-safety idiom with no caller. (`docs/recon_duplication_helpers.md:161` proposes moving it to `text_util` — pointless for an uncalled function; the real dedup target is the *live* copies, a different task.) |
| `tests/test_context.rs` | integration test of the **live** API surface (`ContextEngineFactory::manager_context`, `append`, `perform_rebirth`) | REQ-CORE-001/002/004 | **KEEP unchanged** | Still exercises surviving code; green after the change. |

---

## 4. What was physically removed

* `src/manager/loop_tests.rs` — **deleted file** (651 lines, ~40 unit tests).
  Every one of them certified the unwired executors; it also contained the
  sandbox-failing `test_agentloop_abort_midflight_interrupts_execute_tools`
  (needs a real `openpty`) listed as a known pre-existing failure in
  `.marmel/execution_plan.md`.
* `src/manager/loop.rs` — reduced to `MAX_TURNS` + `is_read_tool` + module doc +
  3 new tests for the surviving helpers (`read_tool_gate_covers_only_read_only_tools`,
  `mutating_and_orchestration_tools_are_not_reads`, `turn_budget_is_one_hundred`).
* `src/manager/mod.rs` — re-export list trimmed to
  `pub use r#loop::{MAX_TURNS, is_read_tool};`.
* `src/manager/context.rs` — the 12 retry-escalation items, the whole
  `SlowPrefillTracker` cluster, `utf8_safe_slice`, `ContextEngine::{prefill,
  compaction_retry_count}` fields, and `use std::time::Duration`; module doc
  updated with an explicit **Requirement status** section (REQ-CORE-003 live-only,
  REQ-CORE-005 not implemented, REQ-CORE-006 satisfied at call sites).
* `src/manager/context_tests.rs` — deleted 10 tests + 3 now-orphaned helpers
  (`fill_over_budget`, `fill_in_trigger_window`, `count_without_injection`):
  `test_context_utf8_safe_slicing`, `test_compaction_retry_escalation_70_then_50`,
  `test_compaction_retry1_over_limit_uses_80_percent`,
  `test_compaction_retry_prunes_orphan_tools`,
  `test_compaction_retry_cap_blocks_third_attempt`,
  `test_slow_prefill_threshold_is_inclusive_at_300s`,
  `test_slow_prefill_requires_two_consecutive_slow`,
  `test_slow_prefill_cooldown_boundary_suppresses_turns_1_to_4`,
  `test_slow_prefill_fast_prefill_resets_consecutive_count`,
  `test_compact_to_target_preserves_rebirth_checkpoint`.
  Remaining: 12 tests, all against live code.

No `#[allow(dead_code)]`, `#[cfg_attr(...)]` or `#[cfg(test)]` shim was added
anywhere; `cargo clippy --all-targets -- -D warnings` is clean.

---

## 5. Surviving file:line map (`src/manager/` after this change)

**`src/manager/loop.rs`** (93 lines)
* `:42` `pub const MAX_TURNS` ← live consumer `src/ui/session.rs:200`
* `:50` `pub fn is_read_tool` ← live consumer `src/ui/session.rs:384`
* `:54-93` `#[cfg(test)] mod tests` (inline, 3 tests)

**`src/manager/mod.rs`** — `:12` `pub use context::{ContextEngine, ContextEngineFactory}`,
`:13` `pub use r#loop::{MAX_TURNS, is_read_tool}`,
`:14` `pub use phase::{MissionPhase, Plan, output_is_success}`

**`src/manager/context.rs`** (486 lines)
* `:17-35` module-doc **Requirement status** block (new)
* `:44` `REBIRTH_CHECKPOINT_PREFIX`, `:47` `REBIRTH_ADVISORY_TRIGGER_RATIO`,
  `:49` `COMPACTION_TRIGGER_RATIO`, `:51` `COMPACTION_TARGET_RATIO`,
  `:54` `REBIRTH_ADVISORY_MESSAGE`
* `:70` `count_text_tokens`, `:78` `count_tokens`, `:83` `count_assistant_tokens`,
  `:100` `message_tokens` (M7 target), `:113` `rebirth_advisory_threshold`,
  `:118` `compaction_threshold`, `:123` `compaction_target`
* `:132` `prune_orphan_tool_messages` (one-directional only → H1; duplicated
  privately at `src/harness/monitor/mod.rs:36`)
* `:154` `struct ContextEngine` (fields now: `messages`, `max_context_tokens`,
  `stats`, `rebirth_advisory_emitted`, `consecutive_rebirths`)
* `:198` `set_goal` (M11), `:236` `token_count`, `:241` `should_compact`,
  `:246` `should_advise_rebirth`, `:264` `inject_rebirth_advisory`,
  `:287` **`compact()`** (the only compaction entry point; M5/H1 target),
  `:347` **`perform_rebirth()`** (M6/M12 target),
  `:392` `save_transcript`, `:402` `load_transcript`
* `:441` `ContextEngineFactory` (`:462` `manager_context`, `:476` `specialist_context`)

**Live consumers of the surviving surface** (re-anchor by symbol; `src/ui/**` is
being edited concurrently): `src/ui/session.rs:200,230-236,337-343,384,670,1042,1105-1111`;
`src/agents/runner/execution.rs:691-694`; `src/agents/runner/fix_loop.rs:515-518`;
`src/harness/mod.rs:186`; `src/llm/client.rs:42,263`; `src/ui/tui/events.rs:785`.

---

## 6. Phase 4a scope after this decision

Per cluster: does it still apply, and to which **surviving** code?

| Cluster | Verdict | Surviving target / note |
|---|---|---|
| **H1** — compaction never repairs dangling assistant `tool_calls` | **STILL APPLIES (unchanged priority)** | `src/manager/context.rs:287 compact()` + `:132 prune_orphan_tool_messages()` (only drops orphan `Message::Tool`; the reverse invariant is absent), reached live from `ui/session.rs:231,338,1106`, `agents/runner/execution.rs:692`, `fix_loop.rs:516`, and after `load_transcript` (`context.rs:402`). Producer of the dangling state is the live abort break `ui/session.rs:725` (assistant `tool_calls` appended earlier, remaining calls never answered). Duplicate prune helper: `src/harness/monitor/mod.rs:36` (monitor owner). |
| **H2** — hard-error return leaks in-flight delegations | **NO LONGER APPLIES to `src/manager/`** (code deleted) | The duplicate loop that leaked (`ManagerLoop::run_executing`) is gone. Triage the equivalent risk only in the live dispatchers: `src/ui/session.rs:383-500` (parallel `delegate_task` fan-out) and `src/orchestrator/mod.rs:387 run_executing` (early `?` return). Both are other-task scope. |
| **H3** — `JoinError` swallowed as success | **NO LONGER APPLIES to `src/manager/`** | Same deletion. Live analogue to audit: `ui/session.rs` join handling in the parallel-read/delegate branch. |
| **H4** — `Signal::Steer` dropped, no mid-flight abort handle | **NO LONGER APPLIES to `src/manager/`** (`Signal` type deleted with its consumers) | Live steer handling to audit instead: `src/ui/session.rs:58,188,192,226,351,1152` (`steer_queue`), `src/orchestrator/steer.rs`, abort via `renderer.aborted()` + `OrchestratorManager::cancellation_token` (`orchestrator/mod.rs:345-360`). |
| **H5** — no per-task failure budget / wall-clock bound | **PARTIALLY APPLIES, MOVED** | The `MAX_EXECUTING_ROUNDS=100` round-loop bug disappears from `manager` (its only non-orchestrator consumer was `ManagerLoop`; `MAX_EXECUTING_ROUNDS` now has exactly one user, `orchestrator/mod.rs:398`). **New live gap created by this decision:** REQ-LOOP-002's 600 s watchdog now exists **nowhere** in the crate (`TURN_WATCHDOG_SECS` was dead and was deleted). The live turn bound is turn-count only (`ui/session.rs:196-205`), with no deadline for a single stalled LLM/tool call. Phase 4a must introduce the watchdog/deadline **in the live loop**, not re-add the constant. |
| **H6** — tasks checked off by unrelated tool calls (regex fallback + literal `"ok"`) | **NO LONGER APPLIES to `loop.rs`** (both halves deleted: `extract_task_id`/`TASK_ID_RE` and the `check_off_on_success(task, "ok")` call site) | Live check-off today is explicit-id: `ui/session.rs:689,1061` (`Plan::check_off`), `orchestrator/mod.rs:351` (`check_plan_on_marker`), `orchestrator/delegate.rs:47-80` (mandatory `task_id` + re-delegation guard). **New orphan for the `phase.rs` owner:** `Plan::check_off_on_success` (`src/manager/phase.rs:469`) now has **zero** production callers (only `phase_tests.rs:78,109,119,126`), and `output_is_success` (`phase.rs:79`) is used only by it and re-exported at `manager/mod.rs:13` → dead-vs-live decision needed there (out of my scope). C1/M9/M10 in `phase.rs` remain valid. |
| **M5** — compaction not target-safe (un-budgeted pinned prefix; `removed` computed before pruning) | **STILL APPLIES (first half only)** | `context.rs:287 compact()` — the pinned prefix (`prefix_end` from the `REBIRTH_CHECKPOINT_PREFIX` scan) is still never budgeted and the tail loop always keeps ≥ 1 message. The second half of M5 (`removed` computed before pruning in `compact_to_target`) was **deleted** with `compact_to_target`; `compact()` returns `()` and logs, so there is no stale `removed` report left. `COMPACTION_RETRY_CAP` self-disabling is gone with the escalation. Thresholds/ratios untouched. |
| **M6** — `slot2` picks an injected advisory after rebirth | **STILL APPLIES (narrowed)** | `context.rs:347-366` (`perform_rebirth`'s `skip(2).rev()` last-`User` scan). Contaminating injections that survive: `REBIRTH_ADVISORY_MESSAGE` (injected at `context.rs:264`) and the runner/UI notice strings (`agents/runner/execution.rs:661`, `ui/session.rs:814,817`). The `CONTEXT_LIMIT_EXCEEDED_MESSAGE` member of the filter list **no longer exists** (deleted, never injected). |
| **M7** — budget model ignores tool schemas | **STILL APPLIES (unchanged)** | `context.rs:100 message_tokens` (3-token framing, 1 token per tool call, `tool_call_id`/`name`/schemas free) + `:113-128` thresholds; `ChatRequest.tools` still ships the full catalog (`src/types/wire.rs:182`). Untouched: ratios unchanged, per task constraint. |
| **M8** (`phase.rs` swallowed read errors), **M9** (divergent success heuristics) | **STILL APPLIES, unchanged — `phase.rs` owner** | `src/manager/phase.rs:322 pending_tasks`, `:330 all_tasks`, `:79 output_is_success` vs the marker matcher in `src/markers.rs`. Not modified by this task. |
| **M11** (`set_goal` overwrites the pinned goal) | **STILL APPLIES** | `context.rs:198 set_goal`, live at `ui/session.rs` per interactive request; interacts with `perform_rebirth` (`context.rs:352-355`). |
| **M12** (rebirth emits `Message::System` at index 3 / duplicates the goal) | **STILL APPLIES** | `context.rs:347-378` (`messages[3]` is `Message::System`; `slot2` falls back to the goal verbatim). |
| **M13** (vacuous / environment-bound tests) | **PARTIALLY RESOLVED** | Gone with `loop_tests.rs`: the `turn_count = MAX_TURNS` shortcut test, the `openpty`-dependent abort test, and the timeout test that never asserted delegation-task cancellation. **Still open in the surviving suite:** `src/manager/context_tests.rs:94 test_context_compaction_orphan_removal` still proves nothing about `prune_orphan_tool_messages` (the orphan is old and is dropped by the positional tail-keep) — build the pair-split fixture (drop the assistant, keep the newest tool result) as part of the H1 fix. New tests added here cover only `is_read_tool` / `MAX_TURNS`. |
| **L1** (dead `_name` param; duplicated task-id regex) | **RESOLVED for `manager/loop.rs`** | Deleted. Remaining grammar owners: `src/markers.rs:65`, `src/plan_parse.rs:138,143` — no longer four copies. Stale test label to fix by the `task_id.rs` owner: `src/task_id.rs:139` still names `"manager/loop.rs extract_task_id (tool-arg task id)"`, a site that no longer exists (label only; test compiles and passes). |
| **L2** (`rescued_this_turn` written, never read) | **RESOLVED** | Deleted with `AgentLoop`. |
| **L3** (`TurnPhase::next()` unused; empty phases) | **RESOLVED** | `TurnPhase` deleted entirely. |
| **L4** (`turn()` doc lie) | **RESOLVED** | Deleted with `AgentLoop`. |
| **L5 / L6 / L9** (`PLAN_MUTEX` scope, plan rewrite newline handling, fenced-code phantom task ids) | **STILL APPLIES — `phase.rs` owner, unchanged** | `src/manager/phase.rs:96 PLAN_MUTEX`, unlocked readers `:322,:330`, `check_off` `:409`, and the `plan_parse` grammar (`src/plan_parse.rs`). |
| **L7** (`record_prefill` cooldown off-by-one) | **RESOLVED BY DELETION** | REQ-CORE-005 is now recorded as **not implemented** (`context.rs:26-30`). If Phase 4a wants it, it is a new live-path feature (prefill timing in `src/llm/**`), not a bug fix in `manager`. |
| **L8** (`MissionPhase` / `determine_phase` / `forced_phase` / `is_silent_dispatcher` never called outside `manager`) | **STILL APPLIES — `phase.rs` owner, unchanged** | `src/manager/phase.rs:552 forced_phase`, `:566 determine_phase`, `:580 is_silent_dispatcher`; still no non-`manager` caller (REQ-PLAN-004 disk override remains unenforced). Same dead-vs-live question this file answers for `loop.rs`/`context.rs`. |

**Net effect on Phase 4a briefs:** the manager layer shrinks to one
compaction/rebirth/token-budget module (`context.rs`, 486 lines) plus two
turn-loop constants/helpers (`loop.rs`). Clusters that must be fixed **in
`src/manager/`** are now only: **H1, M5 (first half), M6, M7, M11, M12, M13
(orphan-prune fixture)**, plus the newly explicit **watchdog/deadline gap**
(created by deleting the dead `TURN_WATCHDOG_SECS`). H2/H3/H4/H6 leave the
manager layer with the deleted code; their live analogues sit in `src/ui/**` and
`src/orchestrator/**` and must be briefed there.

**Hand-offs (outside my scope, do not silently skip):**

1. `src/harness/monitor/mod.rs:81` and `src/orchestrator/mod.rs:101` still carry
   doc comments referring to `AgentLoop` / `ManagerLoop` as if they existed.
2. `OrchestratorManager::run_executing` (`src/orchestrator/mod.rs:387`) has no
   non-test caller (only `orchestrator/tests.rs:437`) → same dead-vs-live decision
   for the orchestrator owner; `MAX_EXECUTING_ROUNDS` hangs off it.
3. `Plan::check_off_on_success` + `output_is_success` are now caller-less in
   production (`src/manager/phase.rs:469`, `:79`) → decide wire-or-delete in the
   `phase.rs` task (it must also settle M9 there).
4. `tests/test_tool_name_literals.rs` `BASELINE` still lists two entries for the
   deleted `src/manager/loop_tests.rs`; they are inert (the guard only scans files
   that exist) and the test passes, but they should be dropped by whoever owns
   that guard test.
5. `src/task_id.rs:139` site label refers to the deleted `extract_task_id`.

---

## 7. Verification (all run in this task, after the edits)

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean (no diff) |
| `cargo check --all-targets` | `Finished dev profile` — 0 errors, 0 warnings |
| `cargo build` | `Finished dev profile` |
| `cargo clippy --all-targets -- -D warnings` | clean — zero warnings, **zero `dead_code`** |
| `cargo test --lib manager::` | `37 passed; 0 failed` (12 `manager::context`, 3 `manager::r#loop`, 22 `manager::phase`) |
| `cargo test --test test_context` | `1 passed; 0 failed` |
| `cargo test --lib` (full) | `457 passed; 6 failed` — all 6 are the pre-existing failures already listed in `.marmel/execution_plan.md`: 4 × `harness::pty::*` (sandbox `openpty` EACCES), `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable` (reads the live `.marmel/execution_plan.md` where `- [x] [t-001]` sits), `orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup` (parallel-isolation flake — passes with `--exact`, confirmed here). The previously-listed `manager::loop::tests::test_agentloop_abort_midflight_interrupts_execute_tools` failure is **gone**, because the unwired loop it certified was deleted. |
| `cargo test --test test_tool_name_literals` (hard constraint guard) | `1 passed` — no raw tool-name literals introduced; the new `manager::r#loop` tests use `crate::tool_names::*` constants only. |

---

## 8. Addendum (task t-031c): the wall-clock half of REQ-LOOP-002 restored in the **live** loop

§4 removed the 600 s `TURN_WATCHDOG_SECS` together with the dead `ManagerLoop`, and
hand-off item 2 above left `MAX_EXECUTING_ROUNDS` / `run_executing` unsettled. That
exposed a real gap (recon **H5**): after the deletion the live session loop had **no
wall-clock bound at all** — only `MAX_TURNS` — and **no repeated-failure budget**.
This addendum records what now closes it, in the live path (`src/ui/session.rs`).

### 8.1 Live bounds (file:line after this change)

| Bound | Where declared | Where enforced |
|---|---|---|
| Turn cap (unchanged, 100) | `src/manager/loop.rs:51` `MAX_TURNS` | `src/ui/session.rs` turn-loop head |
| Per-turn **idle** watchdog (600 s of no observable progress) | `src/manager/loop.rs:65` `TURN_WATCHDOG_SECS` (REQ-LOOP-002 intent restored) | `TurnWatchdog::expired()` checks in both tool-round poll loops + `tokio::time::timeout` around the backend call |
| Per-turn **absolute** hard cap (10 800 s) | `src/manager/loop.rs:74` `TURN_HARD_CAP_SECS` | `watchdog.time_to_hard_limit()` bounds `chat_client_turn`, `expired()` bounds the round joins |
| Repeated-failure budget (threshold 2) | `src/manager/loop.rs:81` `TASK_FAILURE_ESCALATION_THRESHOLD` | `FailureBudget` in the session loop: `retry_allowed` gate before dispatch, `record` after every failed result |

Supporting types live next to `MAX_TURNS` (`src/manager/loop.rs:95-335`):
`DeadlineKind`, `TurnWatchdog`, `FailureVerdict`, `FailureBudget` — clock-injectable
via `*_at` seams so the bounds are unit-testable without sleeping.
`src/ui/session.rs:31` `SessionBounds` + `run_session_with_bounds` (`:71`) keep the
4-argument `run_session` API intact; production always passes
`SessionBounds::default()` (`:42`).

### 8.2 Why the wall-clock bound is safe for the interactive UI (deliberate decision)

The brief asked that a wall-clock bound must not kill a legitimately long run. It does not:

1. The **idle** bound is *progress-triggered*, not turn-triggered. Every rendered
   status/delegation event, every joined tool result and every completed backend call
   calls `note_progress()` and refreshes the window. A long run that is producing work
   is never cut off; only a turn that has gone **600 s with nothing observable** is.
2. The **absolute** cap is 3 h per turn — far above any single legitimate manager
   turn (one backend stream is itself capped by `OVERALL_READ_TIMEOUT_SECS = 1200` at
   `src/llm/client.rs:14`) and only catches a truly wedged turn.
3. Neither bound is a `Config` knob on purpose: they are a runaway safety net, and a
   config file must not be able to silently remove the only bound that exists.
   Tests inject small values instead (`tests/test_ui_session.rs`).
4. Neither bound ends the session: the turn is torn down, `cancel_all()` + handle
   aborts close the in-flight work, a user-visible reason is printed, a system notice
   is appended to the context, and the session stays interactive (the cancellation
   state is reset exactly like the steer-abort redirection path does).

### 8.3 Failure-budget policy (threshold 2)

Per-session `HashMap<String, u32>` keyed by plan task id (`task:t-012`) or tool
signature (`tool:read_file:{...}`) — see `failure_key_for` in `src/ui/session.rs`.
1st failure → retry allowed; 2nd → a strategy-change notice is injected into the
context and surfaced as a status line; from then on the call is **never dispatched**
again unchanged, and the refusal is returned to the model as a tool error
(`failure budget exhausted`). Results caused by an abort/cancel do **not** consume
the budget, and a success refunds the key. This is deliberately earlier than the
monitor's repetition detector (5 consecutive identical calls,
`src/harness/monitor/repetition.rs:13`), so the failure budget — not a generic
repetition heuristic — is what stops failing work.

### 8.4 `run_executing` / `MAX_EXECUTING_ROUNDS` verdict

Confirmed hand-off item 2: `OrchestratorManager::run_executing`
(`src/orchestrator/mod.rs:391`) still has **no non-test caller** (only
`src/orchestrator/tests.rs:437`), so it stays dead and its
`MAX_EXECUTING_ROUNDS` (`src/orchestrator/mod.rs:76`) semantics were left untouched.
The wall-clock gap that its deadness exposed (an unbounded `delegate` in the dead
loop) is now covered by the live per-turn bounds above, so deleting the dead loop no
longer leaves the crate without a time bound.

### 8.5 Regression guards added

* `src/manager/loop.rs` `mod tests` — 11 unit tests (watchdog idle/hard-cap/rearm/
  remaining-budget behaviour, failure-budget escalation/clamping/key independence).
* `tests/test_ui_session.rs` — 3 hermetic integration tests driving the real loop
  with wiremock (no `openpty`): hard-cap bound fires and cancels in-flight work;
  stalled watchdog fires during a real in-flight `sleep` call; identical failing
  `read_file` is attempted exactly twice, then escalated and refused (mutation
  check: disabling the bound/gate makes all of them fail — 100 blind retries).
