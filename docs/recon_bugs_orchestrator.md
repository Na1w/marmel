# Recon bug audit — `src/orchestrator/*` (read-only)

Scope: `bus.rs`, `registry.rs`, `steer.rs`, `steer_extractor.rs`, `preemption.rs`, `delegate.rs`,
`delegation.rs`, `workers.rs`, `notice.rs`, `plan_summary.rs`, `mod.rs` — excluding the
deep-freeze/journal/recover code (`mod.rs:16,58,100-102,243-254,330-335,365-516`) and `freeze.rs`.
Call sites outside the scope are quoted only as reachability evidence
(`src/agents/runner/*`, `src/agents/validation.rs`, `src/harness/mod.rs`, `src/ui/bridge/*`,
`src/manager/phase.rs`). No source file was modified. Line numbers verified at audit time.

---

## HIGH

**H1 — Steering notices are stolen by the wrong worker**
`src/orchestrator/notice.rs:103-115` (`drain_worker_notices`), keys built at `workers.rs:170`.
Match rule is unanchored: `key_lower.starts_with("{target}-") || key_lower.contains(&target)`.
Scenario: arbitrator targets `agent_name = "coder"` (`src/ui/bridge/steer.rs:251`); the validator
loop registers key `validator-coder-t-001` (`src/agents/validation.rs:259` →
`format!("validator-{agent}")` → `register_loop_worker` → `workers.rs:170`).
`"validator-coder-t-001".contains("coder")` ⇒ **the validator drains the coder's notice** and the
coder never receives the user's instruction. Same class: target `t-1` is drained by `coder-t-10`.
Severity: High. Fix: match exact key, then split `{agent}-{task}` and compare fields; delete
`key_lower.contains(&target)`; keep the `"*"`/`"worker"` wildcard.

**H2 — Preemption handshake is sequential and silently drops non-acking streams**
`src/orchestrator/preemption.rs:348-360`.
`for … { timeout(2s, yielded_rx).await }` waits **serially**, and a stream that misses the 2 s
window is silently omitted from `PreemptHandle::Active`.
Scenario: 4 workers share the model, two are mid-tool-call (no SSE chunk ⇒ `poll_control:248` never
runs) → arbitration starts only after 2 s × 2 = 4 s and those two are absent from the handle, so a
`Cancel` decision (`complete_with_subtask_decision:305-327`) never reaches them — the "cancelled"
worker keeps running. The `PauseSignal` also stays queued: on the next chunk that worker emits
`"Yielded model slot …"` (`:259`) then immediately resumes (`yielded_tx.send` fails → `:265`).
Severity: High. Fix: send all signals first, await the `yielded_rx` set concurrently under one
shared deadline, and for non-acked identities cancel `identity.cancel_token` directly; drop stale
signals sink-side.

**H3 — Subtask `action` strings compared raw; variants silently drop Cancel/Forward/Delegate**
`src/orchestrator/preemption.rs:310-312`, `src/orchestrator/steer.rs:419,424`
(+ `src/ui/bridge/steer.rs:138,250`, `src/ui/bridge/sink.rs:170+`).
`normalize_steer_decision` (`steer.rs:286-303`) strips `_`/`-`/space for the **top-level** decision
only; per-subtask actions use raw `eq_ignore_ascii_case("Cancel"|"ForwardNotice"|"DelegateTask")`.
Scenario: model emits `"action": "cancel_task"` or `"forward_notice"` → no match →
`preemption.rs:316-320`/`cancel_active_worker` (`workers.rs:532`) never fires and
`post_notice_to_worker` is never called, yet the arbitrator's streamed `response` already told the
user it was forwarded/cancelled.
Severity: High. Fix: add `normalize_steer_subtask_action()` in the orchestrator and use it at every
subtask branch.

**H4 — No TTL/eviction for notices; workers drain only at turn boundaries ⇒ forwarded steering lost**
`src/orchestrator/notice.rs:57-59, 88-96` — `PENDING_NOTICES`/`WORKER_INBOXES`/`REPLIED_NOTICES`
never expire and drained-empty inbox vecs are never removed. Drain happens only at turn start:
`src/agents/runner/execution.rs:164`, `src/agents/runner/fix_loop.rs:325`.
Scenario: a `ForwardToWorker` notice is posted while the specialist is on its last turn (or inside a
validator fix-loop that then ends) ⇒ it is never drained and never answered, although the user was
told "Forwarded notice notice-7 — awaiting specialist reply"
(`src/ui/bridge/steer.rs:255`). The entry then lives for the whole process and a later worker with a
loosely-matching key drains a stale instruction from a previous task (see H1). All three maps grow
unbounded per session.
Severity: High. Fix: `expires_at_ms` + prune on post;
`WORKER_INBOXES.remove_if(key, |v| v.is_empty())`; emit a user-visible "worker finished without
answering notice-X" when a worker guard drops with pending notices for that key.

**H5 — Worker registry entry clobbered on key collision; pseudo-random fallback key**
`src/orchestrator/workers.rs:182` (`WORKERS.insert` overwrites a live entry) + `:169-173`
(`format!("{agent_name}-{}", Instant::now().elapsed().as_nanos())`). Measured with a compiled probe:
`Instant::now().elapsed().as_nanos()` returns 202/33/41/39/40 — a near-constant low-entropy value,
not a unique id.
Scenario A: `extract_tasks_to_delegate` restarts `idx` at 1 per decision (`steer.rs:421-442`), so two
overlapping steer rounds both yield `steer-task-1` + same agent ⇒ identical key. The second `insert`
replaces the live `WorkerState` (including `cancel_token`); when the first worker's
`ActiveWorkerGuard::drop` (`:107-117`) runs it flips the shared entry to completed, so the still
running worker vanishes from `has_active_workers()` (`:313`), cannot be cancelled
(`cancel_active_worker` skips non-active, `:537`), and its plan line renders as Pending.
Scenario B: task-id-less workers collide (`coder-33`, `coder-41`) and such keys can also collide with
a real task id.
Severity: High. Fix: on collision append a monotonic `AtomicU64` suffix (replacing `:172`) and return
the generated key in `ActiveWorkerGuard`.

**H6 — `record_worker_reply` guesses the notice when exactly one is pending**
`src/orchestrator/notice.rs:140-157` (`else if PENDING_NOTICES.len() == 1 { take the first }`).
Scenario: a worker replies with a stale/typo'd `notice_id` while one notice from an earlier round is
still pending (nothing removes pending notices without a reply — H4) ⇒ the reply is bound to the
wrong `SteerNotice` and `evaluate_worker_reply` (`:180-298`) answers the user using the wrong
`notice.user_inquiry`. `src/harness/mod.rs:519-533` compounds it: on `Err` it reads the pending
notice *without removing it*, keeping the poisoned entry.
Severity: High. Fix: require exact id, `tracing::warn!` + `Err` on miss, and remove the pending entry
on every consumption path.

---

## MEDIUM

**M1 — `plan_summary` parses the plan differently from the plan authority (phantom tasks, wrong %)**
`plan_summary.rs:32-34` vs `src/manager/phase.rs:69-78,660`. `RE_CHECKBOX_DONE.is_match(trimmed)`
matches `[x]` **anywhere** in a line, and `is_pending` is true for any line containing a `t-…` id
(`RE_ID.is_match`) that is not a `#` heading.
Scenario: plan `- [ ] [t-001] Implement parser` / `- [ ] [t-002] Write tests` /
`- Note: items are marked [x] when done` → summary `"Overall Progress: 1/3 tasks completed (33%)"`
while `Plan::is_complete()`/`pending_tasks()` report 0/2. The arbitrator consumes this summary
(`src/ui/bridge/steer.rs:103`) and reasons on wrong progress.
Severity: Medium. Fix: anchor on `ALL_TASKS_RE`/`TASK_LINE_RE` and classify from the line-start
checkbox only; test that summary counts equal `pending_tasks()`/`all_tasks()`.

**M2 — Task→worker correlation can pick an unrelated worker**
`plan_summary.rs:64-67` (first `RE_ID` capture) + `workers.rs:465-469`
(`s.info.prompt.to_lowercase().contains(&tid)` fallback).
Scenario: worker `coder-t-005`'s brief mentions "after t-001 lands" ⇒
`get_active_subtask_by_id("t-001")` returns it and a **pending** line is rendered "Currently In
Progress". `plan_summary.rs:67` also lower-cases the id while `task_id` is stored verbatim;
correlation only survives because `get_active_subtask_by_id` lower-cases again (accidental coupling).
Severity: Medium. Fix: drop the prompt-substring fallback; exact case-insensitive id match, else key
suffix match.

**M3 — Steering history rewritten by heuristics and unbounded**
`bus.rs:152-224` (stages 2-3), `steer.rs:307-319`. Stage 2 uses bidirectional substring matching
(`inq.contains(q) || q.contains(inq)`), stage 3 "most recent pending entry", and both **overwrite**
`*resp_text`.
Scenario: entry A = `("Fix the parser", "Forwarded notice notice-3 … awaiting specialist reply")`;
the user later writes "Fix the parser now" → stage 2 matches A and erases the "awaiting specialist
reply" text, so the coder's later `reply_to_arbitrator` cannot be correlated (stage 1 fails) and the
reply is appended as a **duplicate** entry. `format_steering_history` then feeds the corrupted,
ever-growing transcript (no cap; also pushed at `src/ui/bridge/steer.rs:161`) into every arbitration
prompt ⇒ token blowup + wrong reasoning.
Severity: Medium. Fix: store the `notice_id` with the entry, match only by id, never rewrite in
stage 3, cap history to the last N at push time.

**M4 — Arbitrator failure swallowed after partial text already streamed**
`steer.rs:177-193` (`_ => return None` on timeout/`Err`), `:256-272`. No log, no `emit_status`;
`did_stream_response` means the user already saw partial arbitrator prose, then the outcome degrades
to `QueueInstruction`/`SteerImmediately` (`:372-381`) with no explanation.
Severity: Medium. Fix: log + status on the 60 s timeout; buffer deltas until `decision` is known.

**M5 — `evaluate_worker_reply` fabricates an answer on timeout**
`notice.rs:238-256` (`_ => String::new()` discards both timeout and transport error); fallback
`:284-297` emits `"Specialist [x] explains: …"` marked `SynthesizeResponse`, surfaced as an
authoritative answer (`src/harness/mod.rs:589`).
Severity: Medium. Fix: return `Err` on timeout/transport error; use the raw-reply fallback only when
the model actually replied.

**M6 — Steer subtasks run against the live plan and the global cancel token**
`steer.rs:489-491` (`Plan::default()` = live `.marmel`, `src/manager/phase.rs:259-262`;
`manager.cancellation_token = global_cancellation_token()`).
(a) `extract_tasks_to_delegate` passes `tool_call_id` verbatim as `task_id` (`steer.rs:430-441`), so
`apply_check_off` (`mod.rs:362,531-547`) can check off a **real** plan item for an ad-hoc steer task.
(b) `manager.cancel()`/`abort()` on that manager cancels the process-wide token, killing unrelated
workers.
Severity: Medium. Fix: prefix steer ids (`steer-<round>-<n>`); keep the manager's own
`child_token()`, never the global token.

**M7 — `DelegationEvent::Started` without a terminal event; events never drained in-process**
`mod.rs:229-234, 338-359`, error propagation at `:601`. `delegation_events` is a `Vec` the Manager
never drains, and on `Err`/panic no `Completed`/`Failed` twin is pushed ⇒ the UI shows a phantom
running specialist for the rest of the session.
Severity: Medium. Fix: push the terminal event from a scope guard/`match` covering every exit path;
bound or drain the vec.

**M8 — Silent Dispatcher: no per-delegation timeout; failing tasks re-delegated up to 100 rounds**
`mod.rs:582-605`, `:307-318` (`worker.run(&ctx, &child_token).await` has no timeout).
Scenario: a specialist returns `FAILED`/`REPLAN` so the plan line stays unchecked ⇒ the loop
re-delegates the whole `pending_tasks()` batch every round until `MAX_EXECUTING_ROUNDS`
(`mod.rs:78` = 100) — up to `100 × pending` full LLM delegations. A specialist hung inside `run()`
blocks the loop forever.
Severity: Medium. Fix: per-task attempt counter with loud failure; wrap `worker.run` in
`tokio::time::timeout` + `child_token` select.

**M9 — `handle_delegate_task`: thread-per-delegation, swallowed panic, discarded stats**
`delegate.rs:107, 115-141, 142-151`. One extra OS thread per delegation;
`catch_unwind(...).unwrap_or_else` (`:127-138`) and the `join()` `Err` arm (`:144-151`) fabricate
`FAILED (aborted)` **without logging the panic payload**; `Arc::new(HarnessStats::new())` (`:107`) is
thrown away, so `record_steer_arbitration` (`steer.rs:275`, `notice.rs:275,290`) counts land in a dead
object — `OrchestratorManager.stats` (`mod.rs:97`) is never read anywhere in `src/orchestrator`, and
`steer_arbitrations` (`src/harness/common.rs:50,88`) has no reader.
Severity: Medium. Fix: take `Arc<HarnessStats>` as a parameter (the loop has one,
`src/manager/loop.rs:215`), `tracing::error!` the payload, reuse the caller's blocking-pool thread.
UNCONFIRMED: whether nested `Handle::block_on` can deadlock when the caller sits in the
`new_current_thread` runtime of `block_on_safe` (`src/harness/mod.rs:102-109`). Evidence needed: a
test calling `handle_delegate_task` from inside `block_on_safe` with a worker that `tokio::spawn`s.

**M10 — `brief_for_task` falls back to a brief containing the literal placeholder `task-id`**
`delegate.rs:219, 234-236`. The regex requires exactly `- [ ] [t-xxx] desc`, so
`- [ ] **[t-001]** desc`, `- [ ] t-001: desc`, `1. [ ] [t-001] …` fall through to a generic brief
instructing the specialist to end with the literal `MISSION COMPLETE (task-id)`, which cannot bind to
the real id; `check_plan_on_marker` (`src/manager/phase.rs:587-623`) then depends entirely on the
`task_id` override and the content gate at `mod.rs:535` silently yields `Ok(false)` ⇒ the task never
checks off.
Severity: Medium. Fix: interpolate the real `task_id` and widen/reuse `ALL_TASKS_RE`.

**M11 — Sink infers a truncated `task_id` from the agent tag**
`preemption.rs:172-176` (`agent_tag.rfind('-')`) ⇒ for tag `coder-t-001` the inferred `task_id` is
`"001"`; `agent_name` (`:165-171`, `find('-')`) is right only by luck of key composition. Both feed
`StreamIdentity::matches` (`:80-97`) and every preemption status message.
Severity: Medium. Fix: pass explicit `task_id`/`agent_name` at both registration sites
(`src/agents/runner/fix_loop.rs:340-346`, `:546`) and delete the inference (or share one key-split
helper with `workers.rs:169-170`).

**M12 — `update_active_worker_context` resurrects phantom completed workers**
`workers.rs:214-239`: when the key is absent (ring-evicted, or after H5 clobbering) it inserts a
synthetic entry (`agent_name = key`, empty prompt, `started_at = now`, `status = "Completed"`).
Scenario: after 15 finished workers, a late update for an evicted key injects a fake "Recently
Completed Subagents" row and, via the ring eviction in `Drop:118-138`, pushes a **real** completed
worker out of the list the arbitrator reads.
Severity: Medium. Fix: no-op (or `bool` return) for unknown keys; keep token counts in a separate
side table.

**M13 — Streaming extractor drops astral (surrogate-pair) characters**
`steer_extractor.rs:73-84`: a single `\uXXXX` is decoded, so for `\ud83d\ude00`
`char::from_u32(0xD83D)` is `None` and both halves vanish.
Scenario: arbitrator `response` contains an emoji ⇒ the text streamed to the TUI differs from the
final parsed decision (`steer.rs:218`) — streamed answer visibly corrupt.
Severity: Medium (Low if astral chars are disallowed by prompt policy). Fix: combine
`\uD8xx`+`\uDCxx` before `char::from_u32`.

**M14 — Worker becomes steerable only after an LLM round-trip**
`mod.rs:272-304`: `PromptBuilder::build_blueprint(...).await` (a model call) runs **before**
`register_active_worker_with_token` (`:299`).
Scenario: user steers during JIT blueprint synthesis ⇒ `has_active_workers()` is false
(`workers.rs:313`) so `resolve_steer_outcome` (`steer.rs:372-381`) picks `SteerImmediately` instead
of queueing, and neither `cancel_active_worker` nor `cancel_all_active_workers` can reach the
about-to-start worker.
Severity: Medium. Fix: register right after the depth gate (`mod.rs:223`), then update status.

**M15 — Poisoned statics silently disable cancellation**
`bus.rs:25-35, 46-52`: `global_cancellation_token()` → `unwrap_or_default()` hands out a **fresh,
unrelated** token when the `RwLock` is poisoned, and `is_globally_cancelled()` → `false`; any child
derived from that throwaway token can never be cancelled by `cancel_all()`.
Severity: Medium (low probability, severe effect). Fix: `expect("cancellation lock poisoned")`, or
store the token so poisoning is impossible.

---

## LOW

- `plan_summary.rs:24` — `let _has_workers = has_active_workers();` is dead (result unused).
- `plan_summary.rs:37-48, 51-62` — duplicated 11-line `trim_start_matches` char set; it also strips
  leading digits/dots, mangling tasks whose text starts with a number ("2FA support"), while the
  checkbox itself survives into the emitted line (`- [ ] [t-001] …`).
- `mod.rs:535` — `if let Ok(true) = …check_plan_on_marker(…)` swallows `Err` (IO failure) exactly like
  "not checked"; no `tracing::warn!`.
- `bus.rs:105, 118` — `let _ = tx.send(...)` hides "UI channel closed"; status/event channels are
  unbounded (`bus.rs:3-9`), so a stalled TUI grows memory with no backpressure signal.
- `workers.rs:396-398` vs `:436-438` — byte-identical 300-char `floor_char_boundary(297)` truncation
  blocks for active/completed rendering (boundary handling itself is correct).
- Task-id normalisation duplicated 8×: `workers.rs:161-166, 484, 498`; `delegate.rs:53-56, 72-77`;
  `mod.rs:264-268`; `preemption.rs:47-48, 53-54, 82-85`; `steer.rs:432-435, 462-465`;
  `src/manager/phase.rs:604-609` — drift already present (`trim()` vs `trim_matches()` ordering).
  Factor into one `clean_task_id()`.
- `workers.rs:584, 654, 684` — `test_worker_progress_and_completed_history`,
  `test_format_workers_summary_utf8_char_boundary_no_panic` and
  `test_workers_stress_concurrent_register_update_drop` mutate the process-global `WORKERS` map
  without holding `TEST_WORKERS_MUTEX` (`:101`), unlike `src/manager/loop_tests.rs:293` and
  `src/ui/bridge/tests.rs:483` ⇒ flaky assertions on shared statics.
- `steer_extractor.rs:25-58` — until `"response"` appears the whole stream accumulates in
  `self.buffer` and is rescanned from offset 0 on every chunk (O(n²), unbounded).

---

## Audited clean (do not chase)

- No lock held across an `await`: `mod.rs:229-234`/`:338-359` (`std::sync::Mutex` guards in plain
  blocks, no awaits inside), `preemption.rs:333-341` (read guard dropped by block scope before
  `timeout(...).await`), `bus.rs:102-118` (sender cloned out, guard dropped before `send`).
- No DashMap guard nesting/re-entrancy in `workers.rs`: `update_active_worker_context:208-239`'s
  `else` branch is safe because `get_mut` returned `None` (no guard held);
  `ActiveWorkerGuard::drop:118-138` collects keys before `remove`; `cancel_active_worker:532-552`
  collects before mutating.
- Char-boundary slicing correct at `workers.rs:397, 437` (`floor_char_boundary`) and
  `steer.rs:208-213` / `notice.rs:263-268` (`find('{')`/`rfind('}')` are ASCII boundaries).
- `SpecialistRegistry::canonical` (`registry.rs:38-55`) registers all six roles; `registry.rs:139-174`
  asserts registry/worker allowlist parity. `RecursionDepth::step` (`delegation.rs:90-96`) rejects
  `depth+1 > max` before any `Started` event is emitted (`mod.rs:213-234`).

---

## Fix list for one debugger task (ordered)

1. Introduce one anchored matcher (exact key → `{agent}-{task}` fields → `*` wildcard) and use it in
   `drain_worker_notices` (`notice.rs:103-115`), `worker_matches` (`workers.rs:475-528`),
   `get_active_subtask_by_id` (`workers.rs:465-469`), `StreamIdentity::matches`
   (`preemption.rs:43-115`). Regressions: `t-1` vs `t-10`; `coder` vs `validator-coder-t-001`.
   → H1, M2, M11
2. `notice.rs`: notice TTL, `remove_if` on empty inboxes, unanswered-notice status at worker exit,
   exact-id `record_worker_reply` (drop the `len()==1` guess), remove pending on every consumption
   path incl. `src/harness/mod.rs:519-533`. → H4, H6
3. Add `normalize_steer_subtask_action()` and apply in `preemption.rs:310-312`,
   `steer.rs:419-424`, `src/ui/bridge/{steer,sink}.rs` subtask loops. → H3
4. `workers.rs:169-182`: monotonic-suffix keys, never overwrite an active key, no-op updates for
   unknown keys; add a collision regression test. → H5, M12
5. `preemption.rs:348-360`: parallel handshake under one deadline + token-cancel fallback for
   non-acking streams; drop stale `PauseSignal`s. → H2
6. `plan_summary.rs`: reuse `phase.rs` task regexes/checkbox anchoring, drop the dead
   `has_active_workers()` call and the duplicated trim block; assert summary counts equal
   `pending_tasks()`/`all_tasks()`. → M1, M2, Low items
7. Error-handling sweep: log+status on arbitrator timeout (`steer.rs:177-193`), `Err` on
   `evaluate_worker_reply` timeout (`notice.rs:238-256`), log panic payloads (`delegate.rs:127-151`),
   log `check_plan_on_marker` `Err` (`mod.rs:535`). → M4, M5, M9
8. Lifecycle: register the worker before blueprint synthesis (`mod.rs:272-304`); guarantee a terminal
   `DelegationEvent` (`mod.rs:229-359`); add `worker.run` timeout + per-task attempt cap
   (`mod.rs:582-605`); `steer-`-prefixed ids + own child token (`steer.rs:489-501`); thread the shared
   `HarnessStats` into `handle_delegate_task` (`delegate.rs:107`). → M6, M7, M8, M14, M9

MISSION COMPLETE
