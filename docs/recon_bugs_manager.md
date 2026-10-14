# Recon bug audit — manager core (`src/manager/`)

**Scope:** `src/manager/{mod.rs,loop.rs,context.rs,phase.rs}` + `loop_tests.rs`, `context_tests.rs`, `phase_tests.rs`.
**Out of scope (referenced as evidence only):** `src/orchestrator/*`, `src/ui/*`, rehydration/freeze code.
**Method:** static read of the in-scope files + reachability grep over `src/` + `cargo test --lib manager::` (read-only; no source file was modified).
**Test run result:** `56 passed; 1 failed` — `manager::loop::tests::test_agentloop_abort_midflight_interrupts_execute_tools` fails in this sandbox with `failed to openpty: Os { code: 13, PermissionDenied }` (see M13).

Reachability facts established by grep (used repeatedly below):
* `AgentLoop` / `ManagerLoop` are **never constructed outside `src/manager/`** — the only hits outside are comments (`src/harness/monitor/mod.rs:81`, `src/orchestrator/mod.rs:106`).
* `Plan` (phase.rs) **is live**: `src/harness/plan.rs:15,98`, `src/orchestrator/mod.rs:46`, `src/ui/session/tools.rs`.
* `ContextEngine` **is live**: `src/ui/session.rs:33,627,734,1530`, `src/agents/runner/execution.rs:698`, `src/agents/runner/fix_loop.rs:517`.
* Zero callers outside `src/manager/` for: `compact_with_retry` (context.rs:432), `compact_context` (477), `force_compact_context` (489), `reset_compaction_retry_count` (415), `compaction_retry_count` (411), `utf8_safe_slice` (128), `SlowPrefillTracker`/`record_prefill`/`prefill_tracker` (170-210), `inject_context_limit_exceeded` (545), `determine_phase`/`forced_phase`/`MissionPhase`/`is_silent_dispatcher` (phase.rs:47-52,640-655).

---

## CRITICAL

### C1. `src/manager/phase.rs:222-243` — `MissionMarker::parse` reports a FAILED deliverable as `Complete`
`REPLAN REQUIRED` → `MISSION COMPLETE` → `FAILED` are plain `upper.contains(..)` checks; **`MISSION COMPLETE` is tested before `FAILED` and there is no position/negation test**, and `find_task_id` (172-181) grabs the *first* `t-xxx` anywhere in the text.
*Failing scenario:* a specialist deliverable `"FAILED: replace tool rejected; I did not emit MISSION COMPLETE (t-014) because the build broke"` → `contains("MISSION COMPLETE")` is true → `MissionMarker::Complete` → `Plan::check_plan_on_marker` (587-630) flips `- [ ] [t-014]` → `- [x]` on disk. The task is marked done, `pending_tasks()` no longer lists it, and once the last line flips the plan is auto-archived (phase.rs:529-531) — a mission "completes" with the work undone. Same text also makes `archive()` (425-432) and `is_complete()` succeed.
Live reachability: `src/orchestrator/mod.rs:531-535` gates on the agents-layer marker first (double gate) but still calls this content parse as the state-mutating predicate; `src/manager/loop.rs:513-517` calls it with only `r.is_error` as a gate.
**Severity: Critical.**
**Minimal fix:** prefer the structured `Deliverable.marker` (`src/agents/mod.rs:212-216`) over re-parsing content; if content must be parsed, match markers on the **last** non-empty line block and evaluate `FAILED`/`REPLAN` before `MISSION COMPLETE`. Delete the duplicated parser: `phase.rs:172-243` is a verbatim copy of `src/agents/mod.rs:90-140`.

---

## HIGH

### H1. `src/manager/context.rs:149-175, 391, 537` — compaction never repairs *dangling* assistant `tool_calls`
`prune_orphan_tool_messages` only drops `Message::Tool` whose id has no surviving assistant. The reverse invariant (an `Assistant{tool_calls}` whose `Tool` responses are missing) is never enforced by `compact()` / `compact_to_target()`.
*Failing scenario (confirmed live):* `src/ui/session.rs:707` appends the assistant message (with `tool_calls`) to the transcript, then the tool loop at `src/ui/session.rs:1135-1138` `break`s on `renderer.aborted() || user_exit_requested()` — the remaining calls get **no** `Message::Tool`. The next turn (or a `load_transcript` resume, context.rs:556-576) sends an assistant `tool_calls` message with no matching tool results → provider 400 (`tool_use` without `tool_result`), and `compact()` keeps it forever because it is the newest message.
**Severity: High.**
**Minimal fix:** add a second pass in `compact()`/`compact_to_target()`: for each surviving `Assistant{tool_calls}`, synthesize `Message::Tool{content:"(aborted)"}` for missing ids, or drop the assistant message.

### H2. `src/manager/loop.rs:868-873` — hard-error return leaks every in-flight delegation
`run_executing` pops handles and on `Ok(Err(e))` does `return Err(e)` **without aborting or awaiting the remaining `handles`** (the abort branch at 855-865 does cancel them; the error branch does not).
*Failing scenario:* 3 tasks delegated, t-002 returns a hard error after 10 s; `run_executing` returns while the t-001/t-003 `tokio::spawn` tasks keep running for up to `DELEGATE_TIMEOUT_SECS` (1800 s) — specialists keep writing files/running commands after the manager has already surfaced an error and started a new turn. `loop_tests.rs:593-655` proves the leak: after `run_executing` returns, the 30 s override future is still spawned and uncancelled.
**Severity: High.**
**Minimal fix:** before `return Err(e)`, `for h in handles { h.abort(); }` (+ `abort_pty_process_groups()`), mirroring the abort path.

### H3. `src/manager/loop.rs:875-880` — `JoinError` swallowed as success
`Err(_) => { abort_pty_process_groups(); return Ok(results); }` conflates *cancellation* with *panic*, and the `round` deliverables collected so far are dropped (never `results.extend(round)`).
*Failing scenario:* a specialist task panics → `JoinError` → `run_executing` returns `Ok(partial)`; the caller sees a normal end of the Executing phase, `pending_tasks()` still lists the task, and the plan silently loops back to the next round with no error surfaced anywhere.
**Severity: High.**
**Minimal fix:** distinguish `e.is_cancelled()` from `e.is_panic()`; on panic return `Err(anyhow!("delegation task panicked …"))`, and always `results.extend(round)` before returning.

### H4. `src/manager/loop.rs:708-725` — ManagerLoop drops all `Signal::Steer` input and cannot be aborted mid-flight
`Signal::Steer(_) => {}` (717) discards the queued prompt — the doc claims it is "deferred to the final synthesis round", but nothing stores or replays it (`std::mem::take` drops it). Additionally `ManagerLoop` exposes **no** `abort_flag_handle()` (only `AgentLoop` has it, `loop.rs:537`), and `signal(&mut self)` cannot be called while `run_executing(&mut self)` is awaiting.
*Failing scenario:* user presses Ctrl+C / types a steering message while a 30-minute delegation is in flight → neither is observable by the running loop; the abort is only honoured at the next round boundary in `drain_signals` (loop.rs:760-766), i.e. after all spawned delegations finish. The existing test `loop_tests.rs:393-412` only ever signals abort **before** `run_executing`, so the mid-flight contract is untested and unreachable.
**Severity: High.**
**Minimal fix:** keep deferred steers in a `Vec<String>` and flush them into the synthesis round (or into the transcript) instead of dropping them; add `pub fn abort_flag_handle()` to `ManagerLoop` and document that the UI must set it.

### H5. `src/manager/loop.rs:759-771` + `MAX_EXECUTING_ROUNDS=100` (`src/orchestrator/mod.rs:78`) — no per-task failure accounting or wall-clock bound
`attempts` counts rounds, not failures. A task that returns a `FAILED` deliverable (not a hard error) stays unchecked and is re-delegated in the next round, unchanged.
*Failing scenario:* one impossible task → 100 rounds × up to 1800 s per delegation ≈ 50 h of specialist work, with the same brief each time and no backoff/replan escalation. `TURN_WATCHDOG_SECS` (loop.rs:43) is applied only in `AgentLoop::run_turn` (loop.rs:314) — `run_executing` has no watchdog at all.
**Severity: High.**
**Minimal fix:** track a `HashMap<task_id, fail_count>`, drop a task from the pending set (or emit `TurnOutcome::Error`) after 2 failures, and wrap the round loop in a wall-clock deadline.

### H6. `src/manager/loop.rs:511-526` + `121-140` — tasks are checked off by unrelated tool calls
`check_off()` calls `plan.check_off_on_success(task, "ok")` (524) — a **hard-coded success literal**, so *any* successful tool whose args mention a task id flips that plan line. `extract_task_id` (121-138) does not require an explicit `task_id`; it falls back to `TASK_ID_RE = \(?\[?(t-[A-Za-z0-9_-]+)\]?\)?` run over the whole arguments JSON (verified: matches inside `"path":"src/chart-notes.md"` → `t-notes`).
*Failing scenario:* the agent writes a doc whose content quotes `[t-012]` (or writes `.marmel/execution_plan.md` itself) → the write tool succeeds → `check_off("t-012")` → the task is marked complete without being executed.
**Severity: High** (confined to `AgentLoop`, which is currently dead code — see H8; the live path `src/ui/session/tools.rs:96` reads `task_id` explicitly, which is why this is not firing today).
**Minimal fix:** drop the regex fallback in `extract_task_id`; require an explicit `task_id` argument, and never pass the literal `"ok"` — pass the real tool output to `check_off_on_success`.

### H7. `src/manager/context.rs:432-497, 411-419, 128-140, 170-210` — three requirements are implemented only in code that the runtime never executes
`compact_with_retry` / `compact_context` / `force_compact_context` / `reset_compaction_retry_count` (REQ-CORE-003 hard-limit escalation), `SlowPrefillTracker::record_prefill` (REQ-CORE-005 slow-prefill cooling) and `utf8_safe_slice` (REQ-CORE-006) have **no non-test callers** (grep, see preamble). 12 of the 26 context tests exercise these paths and are green (`test_compaction_retry_*`, `test_slow_prefill_*`, `test_context_utf8_safe_slicing`).
*Failing scenario:* a transcript that jumps from 85 % to >100 % of `max_context_tokens` in one tool result (huge `run_command` output) never gets the forced compaction — the live loop only calls `should_compact()/compact()` (`src/ui/session.rs:627,734,1530`), so the oversized request goes straight to the backend; `compaction_retry_count` is also never reset, so even if wired it self-disables after 2 attempts (cap at context.rs:48).
**Severity: High (unreachable safety net + false assurance from green tests).**
**Minimal fix:** either wire the escalation (`if token_count() > max_context_tokens { compact_with_retry(max) }`) at the live call sites, or delete the dead code and its tests and record the requirement as not implemented. Same for `SlowPrefillTracker` and `utf8_safe_slice`.

### H8. `src/manager/loop.rs` (whole `AgentLoop`) — the audited turn loop is dead code that duplicates the live executor
`AgentLoop::run_turn` / `ManagerLoop::run_executing` are never instantiated outside the module (grep). `ManagerLoop::run_executing` (loop.rs:759-884, struct at 617) is a near-copy of `OrchestratorManager::run_executing` (`src/orchestrator/mod.rs:571-620`) minus the domain-work guard and the timeout/worker-cancellation handling, and `AgentLoop`'s turn loop is a no-op pass-through of `CallBackend`/`StreamResponse`/`ProcessResponse` (332-346) with the real work in `src/ui/session.rs`.
*Consequence:* ~40 unit tests (all of `loop_tests.rs`) certify behaviour the shipped binary never runs, and two executors can drift silently. Every defect in this file must be triaged with that in mind.
**Severity: High (architecture/consistency), severity of C1/H2/H3/H4/H6 is bounded by it.**
**Minimal fix:** delete `AgentLoop`/`ManagerLoop` or make the live loop call them; if kept, add a compile-time/runtime assertion that the live path uses them.

---

## MEDIUM

**M1. `loop.rs:435-447` (and `449-483`)** — successful tool output is discarded: `Ok(r)` is used only for `r.is_error` and `check_off`; no `Message::Tool` is ever appended, so `AgentLoop.transcript` (161, fed only by `push_message`/`drain_signals`) never receives tool results. Skipped writes (`if error.is_some() { break; }`, 457-459) and abort-cancelled reads (427-433) additionally leave assistant `tool_calls` unanswered. *Fix:* append `Message::Tool` for every queued call, including error/aborted placeholders.

**M2. `loop.rs:308-311`** — `turn_count > MAX_TURNS` returns `TurnOutcome::Complete` with no log, indistinguishable from "plan complete"; the counter is also incremented for the refused turn. *Fix:* return a distinct outcome (e.g. `Error("turn budget exhausted")`) or at least `tracing::warn!`. (Off-by-one itself is correct: 100 turns run.)

**M3. `loop.rs:313-320, 405-411, 462-468`** — the 600 s watchdog is only evaluated between phase iterations; a single `spawn_blocking` dispatch can run for hours without the deadline ever being checked. *Fix:* wrap dispatches in `tokio::time::timeout(deadline - now, …)`.

**M4. `loop.rs:396-433`** — (a) when `futures.next()` returns `None` the abort flag is not re-checked, so an abort raised by the last parallel read is deferred a whole turn (PTY SIGKILL deferred too); (b) the comment's claim that dropping `FuturesUnordered` "cancels the remaining pending futures" is false — `tokio::task::spawn_blocking` work is not cancellable, so cancelled `run_command`/`write_file` bodies keep running and can mutate the workspace after `Aborted` is returned. *Fix:* check the flag after the loop; use `tokio::process`/cancellable futures for blocking work.

**M5. `context.rs:357-401` and `498-540`** — compaction is not target-safe: the pinned prefix (`messages[0..prefix_end]`, including a rebirth checkpoint pinned at 365-372/507-514) is never budgeted and the tail loop always keeps ≥1 message (`if total + cost > target && !kept_tail.is_empty()`), so a transcript whose pinned prefix alone exceeds the target "compacts" successfully and stays over the hard limit; `removed` is computed *before* orphan pruning (532-539) so pruning-only passes report `false`. Then `COMPACTION_RETRY_CAP` (48) permanently blocks further attempts. *(Magnitude UNCONFIRMED: needs a fixture with a >target pinned prefix to observe.)* *Fix:* if `count_tokens(&kept) > target`, report failure instead of truncating to a still-over-budget transcript.

**M6. `context.rs:570-577`** — `slot2` picks the last `Message::User` that differs from the goal, but both injected advisories are `Message::User` (334-341, 545-551). After a rebirth, `messages[2]` is frequently the boilerplate "CONTEXT BUDGET ADVISORY…" or "CONTEXT LIMIT EXCEEDED" text and the real last instruction is discarded. *Fix:* filter out `REBIRTH_ADVISORY_MESSAGE` / `CONTEXT_LIMIT_EXCEEDED_MESSAGE` / continuation injections when selecting.

**M7. `context.rs:69-72, 94-105`** — the budget model counts only messages (3 tokens framing; 1 token per tool call; `tool_call_id`, `name` and the serialized tool schemas are free). `ChatRequest.tools` (`src/types/wire.rs:182`) ships the whole tool catalog with every request, so "90 % of budget" can still be >100 % of the real window. *(UNCONFIRMED magnitude — confirm by `count_tokens` over the tool-definition JSON of the manager catalog.)* *Fix:* add a constant/measured tool-schema term to `compaction_threshold`/`compaction_target` (or subtract it from `max_context_tokens` once at startup).

**M8. `phase.rs:387-393, 395-403`** — `pending_tasks()`/`all_tasks()` swallow every error (`match … _ => Vec::new()`). A plan file that exists but cannot be read (permissions, invalid UTF-8, concurrent truncation) is indistinguishable from "no pending work": `run_executing` breaks (loop.rs:769-771) and reports success with 0 deliverables. *Fix:* `match Ok(Some(c)) => …, Ok(None) => vec![], Err(e) => { tracing::error!; vec![] }` and surface the error to the caller.

**M9. `phase.rs:83-97` vs `205-215`** — two divergent success heuristics. `output_is_success` does a substring test, so `TypeError`, `thiserror`, `errors.log`, `error_code=0` mark a successful tool as failed → the task is never checked off (`check_off_on_success`, 564-570). `contains_failed_marker` keeps only the `0 FAILED` sanitizers and has drifted from the `NO ERROR`/`0 ERRORS` list. *Fix:* one shared matcher with word boundaries / marker prefixes; both call sites use it.

**M10. `loop.rs:512-525`** — `let _ = self.plan.check_plan_on_marker(...).unwrap_or(false);` and `let _ = self.plan.check_off_on_success(...)` swallow plan IO errors: a failed plan write is invisible and the loop keeps going, later "completing" on stale disk state. *Fix:* log at `warn` with the task id and propagate once per turn.

**M11. `context.rs:268-278`** — `set_goal` *overwrites* `messages[1]` whenever the transcript has ≥2 entries (live: `src/ui/session.rs:498` per interactive request), so REQ-CORE-002 "immutable goal pinning" is actually "latest goal"; `perform_rebirth` (560-590) then pins the wrong original goal at `[1]`. *(Semantics intent UNCONFIRMED — needs REQ owner.)* *Fix:* rename to `replace_goal`, or append a `Goal updated` user message and keep `[1]` frozen.

**M12. `context.rs:577-586`** — rebirth emits `Message::System` at index 3 (after two `User` messages); strict providers reject non-leading system messages, and when there is no distinct last instruction `slot2` duplicates the goal verbatim at `[1]` and `[2]` (577). *Fix:* inject the checkpoint as `Message::User`, and skip `slot2` when it equals the goal.

**M13. Test-quality defects that hide the above**
* `context_tests.rs:93-152` and `471-523` assert that the orphan `call_orphan` is gone — but that message is *old* and is dropped by the positional tail-keep, so `prune_orphan_tool_messages` is never actually exercised; the pair-split case (assistant dropped, newest tool result kept) is never constructed. *(Confirm by constructing that fixture and asserting the tool result is pruned.)*
* `loop_tests.rs:129-136` (`test_agent_turn_limit`) writes `loop_.turn_count = MAX_TURNS` directly — the real exhaustion path (repeated `run_turn`) is never driven.
* `loop_tests.rs:533-568` (`…abort_midflight_interrupts_execute_tools`) depends on a real `openpty`; it currently **fails in this sandbox** and any environment that errors the tool converts the assertion into a `ToolError` path, silently un-testing abort handling.
* `loop_tests.rs:593-655` asserts the timeout error but never asserts that the spawned delegation task was cancelled (H2 leak).

---

## LOW

**L1. `loop.rs:121-140`** — dead parameter `_name` in `extract_task_id` (doc says "from the tool's arguments/name"); the same regex is duplicated at `phase.rs:175`, `src/agents/mod.rs:100`, `src/orchestrator/delegate.rs:219`. *Fix:* single shared helper.
**L2. `loop.rs:178, 257`** — `rescued_this_turn` is written, never read anywhere, and never reset per turn despite the name. *Fix:* delete or expose.
**L3. `loop.rs:63-80` vs `322-495`** — `TurnPhase::next()` is not used by the state machine (only `loop_tests.rs:30-46`); `run_turn` hard-codes the transitions, so the test certifies an unused helper. `CallBackend`/`StreamResponse`/`ProcessResponse` are empty phases — a state machine with no state.
**L4. `loop.rs:262-265`** — doc says "the current turn phase, computed from internal state"; the function returns `turn_count`.
**L5. `phase.rs:100-102, 476`** — one process-global `PLAN_MUTEX` serializes *all* `Plan` instances (including unrelated temp dirs in tests), while `read()`/`is_complete()`/`pending_tasks()` take no lock and `check_off` uses non-atomic `fs::write` → concurrent subagents can read a torn plan; no cross-process lock at all.
**L6. `phase.rs:500-516`** — the plan is rewritten via `lines().join("\n")`: trailing newline dropped, CRLF normalized; `re_box.replace(line,"[x]")` replaces the *first* empty box on the matched line.
**L7. `context.rs:190-198`** — `record_prefill` increments `turns_since_rebirth` *before* the cooldown test, so the 5th turn is not suppressed although REQ-CORE-005 says "suppressed within the first 5 turns"; `consecutive_slow` also ignores the per-model identity the comment mentions.
**L8. `phase.rs:47-52, 640-655`** — `MissionPhase`, `determine_phase()`, `forced_phase()` (REQ-PLAN-004 disk override) and `is_silent_dispatcher()` are never called outside `src/manager/` → phase gating/override is not enforced at runtime (only `Plan::exists()` is used, by callers outside the module).
**L9. `phase.rs:64-72`** — `TASK_LINE_RE`/`ALL_TASKS_RE` match `- [ ] [t-xxx]` lines anywhere, including inside fenced code blocks, so a plan that documents its own format yields phantom pending task ids.

---

## Ordered fix list (debugger task)

1. `phase.rs:222-243` — make marker parsing positional (last-block) and fail-closed; use `Deliverable.marker` instead of re-parsing content; delete the duplicate parser (`agents/mod.rs:90-140`). *(C1)*
2. `context.rs:149-175` — add reverse invariant: no surviving `Assistant{tool_calls}` without matching `Tool` messages (synthesize `(aborted)` placeholders); cover the live abort path. *(H1)*
3. `loop.rs:868-873` — abort all remaining `JoinHandle`s before `return Err(e)`; then extend `results` with the partial round. *(H2)*
4. `loop.rs:875-880` — split `JoinError` into cancelled vs panic; never report a panic as `Ok`. *(H3)*
5. `loop.rs:708-725` — stop dropping `Signal::Steer`; add `ManagerLoop::abort_flag_handle()`. *(H4)*
6. `loop.rs:759-771` — per-task failure counter + round deadline (replace round-count-only bound). *(H5)*
7. `loop.rs:121-140, 511-526` — require explicit `task_id`; stop passing the literal `"ok"` to `check_off_on_success`. *(H6)*
8. Decide dead code: wire or delete `compact_with_retry`/`SlowPrefillTracker`/`utf8_safe_slice` and `AgentLoop`/`ManagerLoop`; delete the tests that certify unwired paths. *(H7, H8)*
9. `context.rs:357-401, 498-540` — fail compaction that cannot reach target because of an un-budgeted pinned prefix; recompute `removed` after pruning. *(M5)*
10. `context.rs:570-577` — exclude injected advisory messages from `slot2`; emit the checkpoint as `User`, avoid duplicating the goal. *(M6, M12)*
11. `phase.rs:387-403` — stop swallowing read errors in `pending_tasks`/`all_tasks`. *(M8)*
12. `phase.rs:83-97, 205-215` — unify the success/failure matchers with word-boundary matching. *(M9)*
13. `context.rs:69-105` — include tool-schema tokens in the budget model. *(M7)*
14. Fix vacuous tests: orphan-prune fixture must keep the newest tool result while dropping its assistant; drive turn-limit exhaustion through `run_turn`; make the abort test independent of real `openpty`; assert delegation-task cancellation after timeout. *(M13)*
15. Low-severity cleanups: dead param/field, duplicate regexes, unused `TurnPhase::next`, stale docs, unlocked `Plan::read`, plan-file newline normalization, prefill cooldown off-by-one, unwired `MissionPhase` gating. *(L1-L9)*

*No source files were modified during this audit; only `docs/recon_bugs_manager.md` was written.*
