# Deep-Dive Analysis: Orchestrator & Manager Subsystems

**Scope:** `src/orchestrator/*` and `src/manager/*` (including their test files)
**Method:** Static reading of all in-scope files plus targeted cross-reference greps into `src/agents/`, `src/harness/`, `src/ui/`, and `src/main.rs` to trace runtime wiring.
**Status:** Observations only — no source files were modified.

---

## 1. Orchestrator Architecture

### 1.1 Component map

| Component | File | Role |
|---|---|---|
| `OrchestratorManager` | `src/orchestrator/mod.rs:142-549` | The Manager: owns the plan, registry, journal, cancellation token, delegation lifecycle events. |
| `SpecialistRegistry` | `src/orchestrator/registry.rs:30` | Single authority mapping `Agent` role → `SpecialistEntry` (module, tool allowlist, model override). |
| `CrashJournal` | `src/orchestrator/freeze.rs:77` | File-backed Deep-Freeze snapshot/journal for crash recovery. |
| `ActiveWorkerGuard` + statics | `src/orchestrator/workers.rs:39-49` | Process-wide registry of in-flight/recently-completed specialist workers for UI/steering visibility. |
| Event bus | `src/orchestrator/bus.rs` | Global status/event channels + process-wide cancellation token. |
| Steer arbitrator | `src/orchestrator/steer.rs` | LLM-backed mid-flight user-steering decision engine. |
| Preemption coordinator | `src/orchestrator/preemption.rs` | Model-slot borrowing: pauses conflicting specialist streams while the arbitrator runs. |
| Plan progress summarizer | `src/orchestrator/plan_summary.rs:7` | Parses the plan and correlates pending tasks with active workers. |

### 1.2 The Manager invariant ("never does domain work")

The module doc (`src/orchestrator/mod.rs:1-10`) states the Manager owns user interaction, goal decomposition, planning, delegation, and synthesis, and is *strictly forbidden* from domain work. This is enforced structurally and procedurally:

- `guard_no_domain_work()` (`src/orchestrator/mod.rs:249-258`) rejects a configuration where `orchestration.manager_module` points into `/agents/` — i.e. the Manager cannot be configured as a domain worker. It is invoked at the entry of `run_executing` (`src/orchestrator/mod.rs:488-490`).
- The Manager's only permitted tools are `delegate_task`, plan creation/updates, read-only diagnostics, and synthesis (`src/orchestrator/mod.rs:5-10`).
- `synthesize()` (`src/orchestrator/mod.rs:524-530`) is deliberately trivial — it only concatenates deliverable content. This is "the ONLY Manager prose permitted" (doc at `src/orchestrator/mod.rs:523`).

### 1.3 Bus ↔ Manager ↔ Specialists: message/event flow

The "bus" (`src/orchestrator/bus.rs`) is a set of three process-wide statics, not a message-routing broker:

1. `STATUS_SENDER` — `RwLock<Option<tokio::sync::mpsc::UnboundedSender<String>>>` (`bus.rs:3-4`). Populated by `set_status_sender` (`bus.rs:45-49`); drained by the UI session (`src/ui/session.rs:38`).
2. `EVENT_SENDER` — same shape for `crate::ui::Event` (`bus.rs:6-13`); populated at `src/ui/session.rs:39`.
3. `GLOBAL_CANCELLATION_TOKEN` — a `LazyLock<RwLock<CancellationToken>>` (`bus.rs:10-13`).

`emit_status` / `emit_event` (`bus.rs:59-73`) are fire-and-forget: they clone the sender under a read lock and `tx.send(...)`, ignoring send errors (`let _ = tx.send(...)`). This makes emission safe from any thread/task but means a dead UI receiver silently drops events.

**Delegation flow** (`OrchestratorManager::delegate`, `src/orchestrator/mod.rs:268-377`):

1. **Resolve** the requested role against the registry (`mod.rs:269-273`). Unknown roles are rejected with `"unknown specialist: ..."` — the registry is the authority (REQ-ORCH-002).
2. **Fractal depth gate** (`mod.rs:275-292`): `RecursionDepth::step(max)` (`mod.rs:138-143`) returns `None` past the bound; the delegation is rejected *before* any worker is spawned and before any lifecycle event is emitted. Default bound is `DEFAULT_MAX_RECURSION_DEPTH = 3` (`mod.rs:56`), hydrated from the `[orchestration]` TOML block via `OrchestrationConfig::from_config` (`mod.rs:87-104`).
3. **UI surface**: a `DelegationEvent::Started` is pushed into `delegation_events` (`mod.rs:296-303`), an `Arc<std::sync::Mutex<Vec<DelegationEvent>>>` (`mod.rs:187`). Interior mutability lets the shared `Arc<OrchestratorManager>` be drained by the UI without a `&mut` borrow, keeping `delegate` on `&self`.
4. **Deep-Freeze snapshot**: `journal.snapshot(entry.agent, &req)` persists the in-flight request *before* the worker runs (`mod.rs:315-324`).
5. **Isolated context**: `IsolatedContext::from_request(role_prompt, &req)` (`mod.rs:327-329`; definition at `src/agents/mod.rs:156-183`) — the specialist sees only role prompt + brief + snippets, never the Manager transcript (REQ-ORCH-003). `IsolatedContext::into_engine` builds the engine via `ContextEngineFactory::specialist_context` (`src/agents/mod.rs:177-182`).
6. **Worker registration**: `register_active_worker(...)` returns an RAII `ActiveWorkerGuard` (`mod.rs:330-333`); the guard's `Drop` moves the entry to the recently-completed ring (`workers.rs:51-84`).
7. **Run**: the worker is obtained from `registry.worker(agent)` (`registry.rs:151-160`, which constructs fresh `Arc<dyn Specialist>` per call) and run with a *child* cancellation token (`mod.rs:347-348`). If the manager token is already cancelled, a synthetic `FAILED (aborted)` deliverable is returned without spawning (`mod.rs:337-345`).
8. **Teardown**: journal checkpoint cleared (`mod.rs:364-366`), `DelegationEvent::Completed` pushed (`mod.rs:369-373`), and `apply_check_off` binds/validates the task id (`mod.rs:377`).

The harness-level entry point is `handle_delegate_task` (`mod.rs:560-699`), wired into tool dispatch at `src/harness/mod.rs:326/499/587`. It parses/validates arguments (`mod.rs:562-596`), rejects re-delegation of already-checked tasks (`mod.rs:598-619`), builds a throwaway Manager rooted at `Plan::default()` (`mod.rs:624-631`), and runs the delegation synchronously-from-the-caller via `Handle::block_on` on a scoped thread with `catch_unwind` (`mod.rs:634-663`), falling back to `futures::executor::block_on` outside a runtime (`mod.rs:672`).

### 1.4 Registry details

- `SpecialistRegistry::canonical()` (`registry.rs:37-122`) registers the five roles: Coder, Researcher, Debugger, Validator, Generalist. Generalist holds the universal `"*"` allowlist (`registry.rs:119`); Validator is deliberately read-only (no `write_file`/`replace`/`run_command`, but has PTY tools and `leave_verdict`, `registry.rs:100-116`).
- `SpecialistEntry::allows` (`registry.rs:175-196`) implements namespace matching: exact match, `terminal__` prefix stripping, and `*`-suffix wildcard matching.
- A registry unit test (`registry.rs:210-224`) asserts the entry allowlist is *identical* to the worker's declared `tool_namespaces()` — the registry is the single source of truth so a dispatched worker cannot use tools the registry would gate.

---

## 2. Concurrency Model

### 2.1 Async primitives inventory

| Primitive | Location | Purpose |
|---|---|---|
| `tokio::sync::mpsc::UnboundedSender` | `bus.rs:3-13` | Status/UI event fan-out to renderers. |
| `tokio_util::sync::CancellationToken` | `bus.rs:10-14`, `mod.rs:218`, `mod.rs:347`, `mod.rs:420` | Hierarchical cancellation (global → manager → per-delegation child). |
| `tokio::sync::mpsc::UnboundedSender<PauseSignal>` + `oneshot` | `preemption.rs:21-26`, `preemption.rs:119`, `preemption.rs:200` | Stream pause/resume handoff. |
| `std::sync::Mutex` (static) | `freeze.rs:64-65` (`JOURNAL_MUTEX`), `phase.rs:79-80` (`PLAN_MUTEX`) | Serialize file mutations across parallel subagents. |
| `std::sync::Mutex<Vec<DelegationEvent>>` | `mod.rs:187` | Interior-mutable UI event queue on a shared `Arc` manager. |
| `std::sync::RwLock` statics | `workers.rs:39-46`, `bus.rs:3-13`, `preemption.rs:33-34`, `phase.rs:84-88` | Worker registry, bus senders, active streams, plan timing. |
| `Arc<AtomicBool>` | `loop.rs:167-170`, `loop.rs:525` | Mid-flight abort flag shared with the UI/backend layer. |
| `tokio::spawn` | `loop.rs:729-733` | Parallel delegation of independent pending tasks. |
| `tokio::task::spawn_blocking` | `loop.rs:398`, `loop.rs:451` | Run synchronous tool dispatch off the async executor. |
| `FuturesUnordered` | `loop.rs:26`, `loop.rs:393` | Overlap parallel read-only tool calls. |
| `std::thread::scope` + `catch_unwind` | `mod.rs:635-663` | Synchronous-from-caller delegation with panic containment. |

### 2.2 How workers are spawned and supervised

There are two distinct execution models:

**A. Manager-loop parallel delegation** (`ManagerLoop::run_executing`, `src/manager/loop.rs:695-779`): for each round, every pending task is wrapped in a `DelegationRequest` and pushed as a `tokio::spawn`ed future (`loop.rs:729-733`) holding a clone of the shared `Arc<OrchestratorManager>` and the abort flag. The loop then drains the handles with `handles.pop()` + `h.await` (`loop.rs:747-776`). On abort, *every* still-in-flight handle is explicitly `.abort()`ed (`loop.rs:751-756`) — the code comments note that dropping an aborted `JoinHandle` would leave the task running, hence the explicit cancel-all. Per-task failures are logged and returned as a hard error (`loop.rs:762-766`); a cancelled task (`Err(_)` from join) stops the round (`loop.rs:767-772`).

**B. Synchronous-from-Manager delegation** (`OrchestratorManager::delegate`, `mod.rs:268-377`): a single `worker.run(&ctx, &child_token).await` call blocks the calling future until the deliverable returns. "Supervision" is therefore minimal: no join-handle reaping, no restart policy. Failure surfaces as the deliverable's `MissionMarker` (`Failed`/`Replan`) rather than a task crash; only a hard `Err` (unknown role, depth gate, IO) propagates.

There is no persistent worker pool: workers are constructed on demand (`registry.worker()` allocates a new `Arc<dyn Specialist>` each call, `registry.rs:151-160`) and live exactly as long as one delegation.

### 2.3 Shutdown / cleanup semantics

Three layers, from coarse to fine:

1. **Process-wide**: `cancel_all()` cancels the global token (`bus.rs:23-28`); `reset_cancellation()` swaps in a fresh token for a new turn (`bus.rs:38-43`). `main.rs:91` calls `cancel_all()` and `rt.shutdown_timeout(300ms)` at exit.
2. **Manager scope**: `OrchestratorManager` holds a *child* of the global token (`mod.rs:218`); `cancel()` (`mod.rs:233-235`) and `abort()` (`mod.rs:541-549`) cancel it plus the global token. `abort()` deliberately does *not* touch `repetition_breaks` (comment at `mod.rs:533-540`).
3. **Per-delegation**: each `delegate()` mints `self.cancellation_token.child_token()` (`mod.rs:347`), so cancelling the manager cascades to all in-flight workers, and cancelling the global token cascades through the manager.

Cleanup hooks: `ActiveWorkerGuard::drop` (`workers.rs:51-84`) unregisters the worker, persists its last token count into `WORKER_CONTEXT_TOKENS` (so the UI can render "idle" tokens after completion), and appends to `RECENT_COMPLETED_WORKERS` capped at 10 entries (`workers.rs:69-72`). `PreemptibleStreamSink::drop` (`preemption.rs:92-99`) removes the stream from `ACTIVE_STREAMS`. Journal cleanup happens in `delegate` (`mod.rs:364-366`) and `recover_frozen` (`mod.rs:426`).

### 2.4 Synchronous delegation inside an async world

`handle_delegate_task` is a synchronous tool handler that must call async `delegate`. When a runtime handle exists, it spawns the delegation on a scoped OS thread and `block_on`s it there (`mod.rs:634-663`), wrapping the future in `catch_unwind(AssertUnwindSafe)` so a panic mid-delegation degrades to a `FAILED` deliverable instead of unwinding through the tool layer. This is a pragmatic but heavy-handed bridge (see §6.4).

---

## 3. Steering & Preemption

### 3.1 Steering (`steer.rs`)

The Steer Arbitrator is an LLM call that classifies a mid-flight user message against the live plan/goal/subtask state. Key pieces:

- **Contract types**: `SteerDecision` (`steer.rs:50-63`) mirrors the caesar `SteerDecisionResponse` shape — `decision` ∈ {`RespondDirectly`, `AbortImmediately`, `QueueAndContinue`, `ForwardToWorker`, `ApprovePlan`, `RejectPlan`, `DelegateTask`, `Sleep`, `SwitchTier`, `SwitchModel`} plus optional `tier`, `model`, `subtasks`, `sleep_seconds`. `SteerSubtaskDecision` (`steer.rs:31-44`) carries per-worker actions (`ForwardNotice` | `Cancel` | `DelegateTask` | `Sleep`).
- **Context assembly**: `SteerContext` (`steer.rs:194-205`) bundles main goal, orchestrator status, pending approval, plan progress/content, available agents, steering history, user message, and active subtasks. `arbitrate_steer_context_stream` (`steer.rs:207-385`) formats these into a single user prompt with a default agent roster fallback (`steer.rs:209-219`) and the environment block (`format_environment_block()` at `steer.rs:228`).
- **Streaming JSON extraction**: `StreamingResponseExtractor` (`steer.rs:78-192`) is a hand-rolled incremental parser that locates the `"response"` field in the SSE stream and emits decoded deltas (handling `\"`, `\\`, `\n`, `\uXXXX` escapes) until the closing quote. This lets the user watch the arbitrator "type" its decision.
- **Robust parsing**: after streaming, the raw reply is fence-stripped and brace-sliced (`steer.rs:318-329`); if JSON parsing fails, a `sleep`/`terminal__sleep` *tool call* from the reply is converted into a `Sleep` decision (`steer.rs:341-366`). A `Sleep` without `sleep_seconds` defaults to 5 (`steer.rs:334-336`). The whole arbitration is bounded by a 60 s `tokio::time::timeout` (`steer.rs:291-299`); any failure returns `None`.
- **Fallback semantics** (caesar §5.3): `resolve_steer_outcome` (`steer.rs:452-462`) is the pure decision: arbitrator decided → `Decided`; unavailable + active subtasks → `QueueInstruction` (preserve running jobs); unavailable + idle → `SteerImmediately`.
- **Delegation extraction**: `extract_tasks_to_delegate` (`steer.rs:491-535`) turns explicit `DelegateTask` subtasks (or a top-level `DelegateTask` decision) into `(Agent, task_id, prompt)` triples, defaulting the agent to `Coder` and the prompt to the raw user message.
- **Ad-hoc execution**: `execute_steer_subtask` (`steer.rs:548-565`) builds a fresh `OrchestratorManager` and — notably — *replaces* its cancellation token with the global one (`steer.rs:557`), so steer-spawned subtasks obey global abort. `synthesize_steer_subtask_response` (`steer.rs:571-640`) then answers the user in the user's own language from the specialist findings.

**How steering interrupts the manager loop**: in `ManagerLoop::drain_signals` (`loop.rs:648-666`), a `Signal::Steer` is *deliberately dropped* in Executing mode — the silent dispatcher does not inject user prose mid-round; it is deferred to final synthesis (doc at `loop.rs:583-588`). Contrast with `AgentLoop::drain_signals` (`loop.rs:281-299`), which injects steer prompts as `Message::User` into the transcript. The actual arbitration happens outside the loop, in the UI bridge (`src/ui/bridge.rs:88-140`): preempt conflicting streams → arbitrate → abort-or-resume preempted streams → optionally delegate steer subtasks.

### 3.2 Preemption (`preemption.rs`)

Purpose: "slot borrowing" on shared models. When the user steers mid-flight, specialist streams using the same model are paused so the Steer Arbitrator gets the GPU/LLM slot, then resumed with prefix caching or aborted (module doc, `preemption.rs:1-6`).

Mechanics:

- Specialists register a `PreemptibleStreamSink` (`preemption.rs:47-99`) in the LLM runner (`src/agents/runner.rs:396`, `src/agents/validation.rs:144`). Registration inserts a `StreamEntry` (agent tag, model, pause channel) into the static `ACTIVE_STREAMS` map (`preemption.rs:57-79`); `Drop` removes it (`preemption.rs:92-99`).
- The sink implements the `StreamSink` control protocol: `poll_control` (`preemption.rs:104-112`) does a non-blocking `try_recv` on the pause channel and returns `StreamControl::Pause { user_input }`; `on_pause` (`preemption.rs:113-138`) creates a oneshot, hands the *sender* back to the preemptor via the `PauseSignal.yielded_tx` channel, emits a status ("Yielded model slot ..."), and awaits the arbitrator's `PauseAction` (defaulting to `Resume` if the preemptor vanished).
- `preempt_conflicting_stream(target_model, user_msg)` (`preemption.rs:183-213`) snapshots all registered streams whose model conflicts with the target (`models_conflict`, `preemption.rs:37-44` — empty model strings conflict with everything; comparison is case-insensitive), sends `PauseSignal`s, and waits up to **2 s** per stream for the yield confirmation (`preemption.rs:207`). Streams that do not confirm within 2 s are simply not tracked in the handle (they keep running).
- `PreemptHandle` (`preemption.rs:142-181`) resolves the pause: `complete_all(action)` resumes/aborts everything; `complete_with_subtask_decision` inspects the steer decision and sends `PauseAction::Abort` only to workers explicitly cancelled by a `Cancel` subtask matching the agent tag or tool-call id (`preemption.rs:156-180`).

The UI bridge drives the full sequence (`src/ui/bridge.rs:88-105`): preempt → arbitrate → `complete_all(Abort)` on `AbortImmediately`/`RejectPlan`, else `complete_with_subtask_decision`.

### 3.3 Freeze semantics (`freeze.rs`)

Deep-Freeze is crash-recovery persistence, *not* a pause mechanism:

- On every delegation start, `CrashJournal::snapshot` (`freeze.rs:105-126`) mints a UUID `worker_id`, appends a `FreezeSnapshot { worker_id, agent_name, sub_req }` to `.marmel/.session_frozen.json`, and appends a `Frozen` event to the append-only `.marmel/.session_journal.json`. All journal operations serialize on the static `JOURNAL_MUTEX` (`freeze.rs:64-65`).
- On clean termination, `delegate` calls `journal.clear(&worker_id, true)` (`mod.rs:364-366`), which removes *only* the snapshot whose `worker_id` matches (`freeze.rs:144-169`) — a foreign id cannot stomp another in-flight freeze (tested at `freeze.rs:288-296`) — and appends a `Resolved` (or `Failed`) journal event.
- Recovery: `OrchestratorManager::recover_frozen` (`mod.rs:394-432`) reads the first frozen snapshot, re-resolves the role (failing loudly with a `Failed` journal event if the role is no longer registered, `mod.rs:401-411`), rebuilds the isolated context from the *preserved* `sub_req` — documented as "the SOLE exception to cognitive isolation, scoped strictly to the frozen session" (`freeze.rs:9-11`) — reruns the worker, and clears the checkpoint with `clean = !matches!(marker, Failed)`.
- The file format is forward/backward compatible: `RawFrozen` (`freeze.rs:68-72`) accepts either a single snapshot (legacy) or a list. A corrupt file degrades to an empty list with a `tracing::warn` (`freeze.rs:203-209`), i.e. recovery is skipped rather than crashing.
- Bootstrap: the UI session checks `mgr.journal.is_frozen()` at startup and spawns `recover_frozen()` on a task, streaming its status and honoring user abort via `cancel_all()` (`src/ui/session.rs:44-90`).

---

## 4. Manager Loop

### 4.1 Phase machine (`src/manager/phase.rs`)

`MissionPhase` (`phase.rs:44-49`) has exactly two states: `Conversational` (read-only tools) and `Executing` (full toolset). Phase resolution (`determine_phase`, `phase.rs:598-606`) is:

1. `.marmel/forced_phase.txt` override wins if present and parseable (REQ-PLAN-004, `phase.rs:584-595`).
2. Else `Executing` iff `.marmel/execution_plan.md` exists, else `Conversational`.

`Plan::is_silent_dispatcher(Executing)` (`phase.rs:612-614`) is the formal gate for REQ-PLAN-003 silent-dispatcher behavior.

The plan file contract (module doc `phase.rs:1-33`): `.marmel/execution_plan.md` with `- [ ] [t-xxx]` checkboxes; successful tool output (no `ERROR`/`FAILED`/`REPLAN REQUIRED`, case-insensitive — `output_is_success`, `phase.rs:73-77`) toggles the matching line to `[x]`. `MissionMarker` (`phase.rs:176-231`) is the plan-side terminal-marker parser: `REPLAN REQUIRED` is checked *before* `FAILED` so a combined string classifies as replan (`phase.rs:202-207`), `MISSION COMPLETE` extracts the `(t-xxx)` id via `find_task_id` (`phase.rs:157-163`), and benign counters like `0 failed` are sanitized out of the failed check (`contains_failed_marker`, `phase.rs:189-197`).

### 4.2 Turn state machine (`src/manager/loop.rs` — `AgentLoop`)

`TurnPhase` (`loop.rs:47-75`) is the strict six-phase sequence `PrepareTurn → CallBackend → StreamResponse → ProcessResponse → ExecuteTools → CheckFinish` (REQ-LOOP-001). Bounds: `MAX_TURNS = 100` (`loop.rs:41`) and a 600 s whole-turn watchdog (`TURN_WATCHDOG_SECS`, `loop.rs:43`; enforced at `loop.rs:311-314`).

`run_turn` (`loop.rs:301-500`) highlights:

- **PrepareTurn**: `drain_signals()` (`loop.rs:281-299`) clears a stale abort flag, drains queued signals (steer → injected as user message; abort → returns true), and re-arms the flag if an abort is actually pending. An abort here SIGKILLs tracked PTY process groups and returns `TurnOutcome::Aborted` (`loop.rs:319-323`).
- **CallBackend / StreamResponse / ProcessResponse**: hooks only — the real LLM call and transcript population live in the UI/backend layer (comments at `loop.rs:325-340`).
- **ExecuteTools**:
  1. Repetition gate (REQ-HARN-002): every pending tool is observed by `monitor.observe_tool`; ≥3 identical repetitions or alternating cycles are blocked with the SPEC error *before dispatch* (`loop.rs:349-377`).
  2. Partition into parallel reads (`read_file`, `grep_search`, `glob` — `is_read_tool`, `loop.rs:137-139`) and sequential writes (everything else, including `delegate_task`, plan tools, PTY, MCP — `is_write_tool`, `loop.rs:145-150`).
  3. Reads run concurrently via `FuturesUnordered`, each on `spawn_blocking` (`loop.rs:391-405`); the abort flag is checked after every completion and mid-flight abort drops the `FuturesUnordered` (cancelling pending futures) and SIGKILLs PTY groups (`loop.rs:413-421`).
  4. Writes run strictly in order, each on `spawn_blocking`, with abort checks before and after each dispatch (`loop.rs:437-471`).
  5. Successful outputs feed `check_off` (`loop.rs:496-515`): `delegate_task` results go through the marker-aware `Plan::check_plan_on_marker`; other tools use the legacy `check_off_on_success(task, "ok")` heuristic (hardcoded `"ok"` because free-form output could contain benign "error" strings — comment at `loop.rs:508-512`).
- **CheckFinish**: plan complete → `TurnOutcome::Complete`, else `Continue` (`loop.rs:474-481`).

The resilience monitor (`HarnessMonitor`) is present in every loop (`loop.rs:171-176`): XML tool rescue (`rescue_xml_calls`, `loop.rs:248-255`), semantic repetition/cycle detection, and text-repetition stream breaking (`feed_stream_text`, `loop.rs:241-245`). Stats can be shared across loops via `with_stats` (`loop.rs:209-225`) so interventions aggregate session-wide (tested in `loop_tests.rs:100-116`).

### 4.3 The Silent Dispatcher (`ManagerLoop`, `loop.rs:583-779`)

`ManagerLoop` wraps a shared `Arc<OrchestratorManager>` plus a `scheduler: Box<dyn Fn(&str) -> Agent>` mapping task id → specialist (`loop.rs:594-608`). `run_executing` (`loop.rs:695-779`):

1. Loop while the plan is incomplete and `attempts < MAX_EXECUTING_ROUNDS` (100, `mod.rs:59`) — an un-delegate-able plan fails loudly instead of spinning.
2. Drain signals at the top of each round; abort → SIGKILL PTY groups and return gathered results (`loop.rs:701-705`).
3. Read pending tasks from disk, build one self-contained `DelegationRequest` per task (`brief_for_task`, `mod.rs:717-745`, extracts the plan line's text as the brief), and `tokio::spawn` all of them — independent tasks overlap (REQ-ORCH-005 parallel delegation; doc at `loop.rs:577-581`).
4. Drain handles: abort → explicitly `.abort()` every in-flight handle + SIGKILL PTY groups + return partial results; per-task `Err` → warn and hard-fail the loop; cancelled join → stop immediately (`loop.rs:747-772`).
5. `delegate()` auto-checks-off completed tasks, so the next round re-reads the plan for whatever remains (`loop.rs:774-776`).

Steer signals are intentionally *not* injected mid-dispatch in this mode (`drain_signals` at `loop.rs:648-666`); they are deferred to the final synthesis round.

### 4.4 Context management (`src/manager/context.rs`)

Token accounting uses a `cl100k_base` BPE singleton (`bpe()`, `context.rs:61-63`) with a 3-token per-message framing overhead (`message_tokens`, `context.rs:76-99`).

**Budget policy** (constants at `context.rs:33-59`):

| Threshold | Ratio | Constant |
|---|---|---|
| Rebirth advisory | 80% | `REBIRTH_ADVISORY_TRIGGER_RATIO` (`context.rs:33`) |
| Compaction trigger | 90% | `COMPACTION_TRIGGER_RATIO` (`context.rs:35`) |
| Compaction target | 70% | `COMPACTION_TARGET_RATIO` (`context.rs:37`) |
| Slow-prefill warning | ≥300 s, 2 consecutive, ≥5 turns post-rebirth | `SLOW_PREFILL_THRESHOLD_SECS` / `MIN_TURNS_AFTER_REBIRTH` (`context.rs:39-42`) |

**Pinned prefix (REQ-CORE-001/002)**: `set_system_prompt` locks the system prompt at `messages[0]` (`context.rs:247-254`); `set_goal` pins the goal at `messages[1]`, inserting a placeholder system message first if needed to keep indices stable (`context.rs:259-270`). Compaction and rebirth always preserve `[0]`/`[1]` (`compact_to_target`, `context.rs:469-502`; `perform_rebirth`, `context.rs:519-560`).

**Compaction paths**:

- `compact()` (`context.rs:337-384`): automatic, triggers at >90%, keeps the two pinned messages plus the most recent turns until the 70% target, then prunes orphaned `role:"tool"` messages (`prune_orphan_tool_messages`, `context.rs:143-160` — a tool message survives only if its `tool_call_id` appears on a surviving assistant message).
- `compact_with_retry(limit)` (`context.rs:403-446`): caesar-style escalation, gated by `compaction_retry_count < 2`. Retry 1: if over the hard limit, `compact_context` targets 80% of the limit; otherwise the ratio path targets 70% of the current count. Retry 2 targets 50%. On success it injects the `SYSTEM: CONTEXT LIMIT EXCEEDED` user message (`context.rs:504-509`) instructing the agent to rebirth. The retry counter resets on a successful backend response (`reset_compaction_retry_count`, `context.rs:398-401`).

**Rebirth (REQ-CORE-004)**: `perform_rebirth(summary)` (`context.rs:519-560`) collapses the transcript to exactly 4 messages — `[0]` system, `[1]` goal, `[2]` last user instruction distinct from the goal (or the goal), `[3]` the `SYSTEM: REBIRTH CHECKPOINT` injection — bumps `session_rebirths` in the shared stats, resets the slow-prefill tracker, and clears the advisory flag.

**Advisory (80%)**: `should_advise_rebirth` / `inject_rebirth_advisory` (`context.rs:307-320`) push the `CONTEXT BUDGET ADVISORY` user message instructing the agent to summarize and call the `rebirth` tool; the one-shot flag resets when compaction or rebirth brings the count back under 80%.

**Slow-prefill cooling (REQ-CORE-005)**: `SlowPrefillTracker` (`context.rs:164-206`) warns only on the second *consecutive* prefill ≥300 s and only after ≥5 recovery turns post-rebirth; a fast prefill resets the consecutive counter.

**Transcript persistence**: `save_transcript` / `load_transcript` (`context.rs:572-600`) round-trip the JSON transcript (used for `.marmel/.session_transcript.json`, path helper at `phase.rs:267-270`); load rejects files with fewer than 2 messages.

**Factory isolation (REQ-ORCH-003)**: `ContextEngineFactory` (`context.rs:621-664`) is the canonical construction point — `manager_context(system, goal)` for the Manager, `specialist_context(role_prompt, brief)` for specialists. A specialist engine starts with exactly two messages, and no code path can seed it with Manager history (doc at `context.rs:607-620`; verified by tests `context_tests.rs:45-97`).

---

## 5. Plan Lifecycle

### 5.1 Creation

`OrchestratorManager::create_plan` (`mod.rs:261-265`) logs via `debug_log::log_plan_update` and delegates to `Plan::create` (`phase.rs:273-296`), which takes `PLAN_MUTEX`, creates `.marmel/`, writes `execution_plan.md`, and calls `record_plan_start()` (`phase.rs:91-94`) — stamping both an `Instant` and wall-clock time into the static `PLAN_STARTED_AT` (`phase.rs:84-86`) and clearing any completion stamp.

### 5.2 Tracking / check-off

Three check-off entry points, all serialized by `PLAN_MUTEX` (`phase.rs:79-80`):

1. **Tool-output heuristic** (`Plan::check_off_on_success`, `phase.rs:522-528`): flips `[ ]`→`[x]` when the output lacks `ERROR`/`FAILED`/`REPLAN REQUIRED`. Used by `AgentLoop::check_off` for non-delegation tools (`loop.rs:508-512`).
2. **Marker-aware** (`Plan::check_plan_on_marker`, `phase.rs:545-581`): parses the deliverable's `MissionMarker`; only `Complete` proceeds; the task id resolves from the explicit override first, then the marker's own `(t-xxx)` token (`phase.rs:559-573`). Used by `AgentLoop::check_off` for `delegate_task` (`loop.rs:500-506`) and by the Manager's `apply_check_off` (`mod.rs:446-470`).
3. **Double-gated auto check-off** (`OrchestratorManager::apply_check_off`, `mod.rs:446-470`): a task is checked off only when the authoritative `MissionMarker` on the deliverable is `Complete` **AND** re-parsing the content still yields a `MISSION COMPLETE (t-xxx)` token via `Plan::check_plan_on_marker`. This defeats stale completion tokens leaked in a `FAILED`/`REPLAN` body (tested in `orchestrator/tests.rs:60-135`).

`Plan::check_off` (`phase.rs:445-503`) does the actual flip: it normalizes the id (strips `[]()"'`), finds the *first* line containing the id with an unchecked box, replaces the box with `[x]`, rewrites the file, and — when the flip completes the plan — calls `record_plan_completed()` and writes a best-effort completion snapshot to `.marmel/archive/` (`write_completed_snapshot`, `phase.rs:507-521`) while keeping the active file readable until an explicit `archive()` (t-203 (c), `phase.rs:491-498`).

### 5.3 Completion detection & archiving

- `is_complete` (`phase.rs:377-396`): false if *any* unchecked box (`[ ]` or `( )`) appears anywhere; must contain at least one checked box. An absent plan is never complete.
- `archive()` (`phase.rs:398-437`): refuses to archive an incomplete plan (`Ok(None)`, t-203 (a) — archiving would silently lose the working checkpoint); otherwise writes `.marmel/archive/execution_plan_<UTC timestamp>.md`, mirrors to `.marmel/execution_plan_archive.md`, deletes the active plan and the session transcript, and calls `clear_plan_start()`.
- **No stale-archive resurrection** (t-203): `read()` returns `None` when the active file is absent regardless of archives (`phase.rs:299-308`), so `pending_tasks`, `all_tasks`, `is_complete`, and `determine_phase` can never resurrect the `Executing` phase from an archived plan (tested in `phase_tests.rs:247-279`).
- `clear()` (`phase.rs:315-352`) deletes the active plan, archive, forced-phase file, and transcript.

### 5.3 Progress summary (`plan_summary.rs`)

`generate_plan_progress_summary` (`plan_summary.rs:7-115`) parses plan lines with three regexes (`plan_summary.rs:16-18`), classifies each as completed / in-progress / pending, and correlates pending task ids with live workers via `workers::get_active_subtask_by_id` (`plan_summary.rs:60-64`, backed by `workers.rs:314-322`). It prepends plan start time from `phase::get_plan_start_time()` (`plan_summary.rs:86-93`, which falls back to the plan file's filesystem metadata when the in-memory stamp is gone, `phase.rs:130-152`) and reports an overall completion percentage. The `_has_workers` binding at `plan_summary.rs:20` is computed but unused (see §6.6).

### 5.4 Handler-level plan guards

`handle_delegate_task` enforces plan hygiene at the tool boundary: `task_id` is mandatory and normalized (`mod.rs:580-596`), and re-delegating a task whose plan line already reads `[x]` is rejected with an explanatory `ToolResult::err` (`mod.rs:598-619`) — the Manager is told to proceed to synthesis instead.

---

## 6. Observations: Design Decisions, Races, Error Handling, Code Smells

### 6.1 Notable design decisions (positive)

1. **Disk as the single source of truth** (REQ-ORCH-004). The plan lives in `.marmel/execution_plan.md`; every component (Manager, specialists, UI summarizer, tests) reads the same file. This makes state observable and crash-consistent, and the "no stale-archive fallback" rule (`phase.rs:292-296`) closes a subtle resurrection bug class.
2. **Double-gated check-off** (`mod.rs:446-470`). Requiring both a genuine `MissionMarker::Complete` *and* a re-parsed content-side `MISSION COMPLETE (t-xxx)` token defends against stale completion tokens leaking from pre-validation drafts (tests `orchestrator/tests.rs:60-135`).
3. **Depth gate before side effects** (`mod.rs:275-299`). A rejected delegation emits no `Started` event and spawns no worker, keeping the UI event stream truthful.
4. **RAII everywhere for registries**: `ActiveWorkerGuard` (`workers.rs:49-84`) and `PreemptibleStreamSink::drop` (`preemption.rs:92-99`) guarantee unregistration even on panic/unwind paths.
5. **Freeze-before-run, clear-after-run** (`mod.rs:315-324`, `mod.rs:364-366`) with a *worker-id-scoped* clear (`freeze.rs:144-167`) prevents concurrent delegations from stomping each other's checkpoints (test `freeze.rs:288-296`).
6. **Loud failure over silent spin**: `MAX_EXECUTING_ROUNDS` (`mod.rs:59`, applied `loop.rs:699`), loud failure when a frozen role is unregistered (`mod.rs:401-411`), and the `guard_no_domain_work` entry check (`mod.rs:484-490`).

### 6.2 Potential race conditions

1. **Check-then-act on the plan between rounds** (`loop.rs:699-712`). `run_executing` reads `pending_tasks()` outside any lock, then spawns delegations. Two `ManagerLoop`s (or a ManagerLoop and a steer subtask) sharing the same plan dir could both read the same pending task and double-delegate it. The per-call re-delegation guard (`mod.rs:598-619`) narrows but does not close the window: the file check and the later `check_off` are not atomic with respect to the dispatch decision. Mitigating factor: `PLAN_MUTEX` serializes each individual read/flip, and the deterministic specialist driver makes double execution mostly idempotent in tests — but with live LLM workers it would duplicate work.
2. **`check_off` flips the first matching line** (`phase.rs:462-476`). The match is `line_lower.contains(&tid_lower)` — a task id that is a substring of another line's text (e.g. `t-01` vs `t-010`, or an id appearing in a description) can flip the wrong line. The regexes elsewhere (`phase.rs:66-70`, `mod.rs:722-723`) are anchored to checkbox lines; `check_off` is not.
3. **`drain_signals` clear/re-arm window** (`loop.rs:281-299`, `loop.rs:648-666`). The flag is cleared at drain start and re-armed only if an abort is pending. An abort signaled by another thread *between* `store(false)` and the re-arm would be lost until the next check. In practice `signal()` sets the flag synchronously before pushing (`loop.rs:269-277`), and the drain runs on the same thread that owns `pending_signals`, so the exposed window is the mid-flight check pattern (`abort_flag_handle`, `loop.rs:525-529`) — acceptable, but the clear-then-rearm dance is delicate and undocumented as a memory-model guarantee beyond `SeqCst`.
4. **`ACTIVE_STREAMS` snapshot vs. registration** (`preemption.rs:186-196`). The preemptor clones senders under a read lock, then sends outside the lock. A sink that drops between snapshot and send is handled (`if pause_tx.send(signal).is_ok()`), but a stream that registers *after* the snapshot is not preempted this round — a benign, eventually-consistent gap.
5. **Journal `clear` fallback tuple** (`freeze.rs:156-159`). When the `worker_id` is not found, `clear` still appends a journal event with `(Agent::Coder, None)` — a fabricated identity in the audit log. The comment calls it a "no-op guard", but the journal record is misleading for forensics.
6. **`delegation_events` unbounded growth** (`mod.rs:187`, pushed at `mod.rs:296-303` and `mod.rs:369-373`). The `Vec` is drained only if the UI consumes it; a long session without draining grows without bound while holding a `std::sync::Mutex` (locked briefly, but still a shared hot path).

### 6.3 Error-handling patterns

- **Deliverable-as-error**: specialist failures are encoded as `MissionMarker::Failed`/`Replan` deliverables rather than `Err`, keeping the round alive (`loop.rs:758-766` warns and hard-fails only on true `Err`). The handler maps markers to `ToolResult::ok`/`err` with the terminal token appended (`mod.rs:684-698`).
- **Panic containment**: `catch_unwind(AssertUnwindSafe)` around `block_on(delegate)` plus thread-scope join handling (`mod.rs:635-663`) converts panics into `FAILED` deliverables.
- **Best-effort IO with logging**: journal write failure in `delegate` logs a warning and continues with an empty `worker_id` (`mod.rs:315-323`) — the comment says it "must not silently lose a task", but returning `""` and proceeding *is* a soft failure: the task runs unfrozen, so a crash mid-run loses the checkpoint (see §6.5).
- **Silent send failures**: `emit_status`/`emit_event` ignore send errors (`bus.rs:59-73`); appropriate for fire-and-forget UI, but a closed channel means silent observability loss.
- **Graceful lock poisoning**: nearly every static `RwLock`/`Mutex` access uses `if let Ok(guard)` and degrades to a default (`bus.rs:15-21`, `workers.rs:85-124`, `phase.rs:91-152`). This avoids panics after a poisoned lock but can mask systemic poisoning as "no data".

### 6.4 Code smells

1. **`handle_delegate_task` builds a fresh `OrchestratorManager` per call** (`mod.rs:624-631`), including a new `HarnessStats` and a `ChatClient` — the shared-stats aggregation (REQ-HARN-004) and the configured manager's cancellation token are bypassed on this path. The thread-scope + `block_on` + `catch_unwind` sandwich (`mod.rs:634-663`) is also the most intricate code in the subsystem; a dedicated "sync bridge" helper with documentation would reduce risk.
2. **`recursion_granted` is dead weight** (`src/agents/mod.rs:151`). It is set to `false` at every construction site (`loop.rs:725`, `steer.rs:565`, `freeze.rs:265`) and the depth gate is documented as unconditional (`mod.rs:276-281`); the field exists only for caesar parity.
3. **Duplicated task-id regexes**. `phase.rs:66-69` (`task_line_re`), `phase.rs:157-163` (`TASK_ID_RE`), `loop.rs:112-123` (`TASK_ID_RE`), `mod.rs:715-723` (`TASK_LINE_RE`), and `plan_summary.rs:16` (`re_id`) all encode the `t-xxx` grammar with slightly different anchoring and bracket handling. A single shared parser would prevent drift (this is already partially acknowledged by the `OnceLock` "CODE_REVIEW Point 2" comments).
4. **Unused computation**: `let _has_workers = has_active_workers();` (`plan_summary.rs:20`) is computed and discarded.
5. **Hardcoded `"ok"` success string** in `AgentLoop::check_off` (`loop.rs:512`) — correct per the comment (avoiding false positives like "thiserror"), but the magic literal deserves a named constant next to `output_is_success`.
6. **`SteerDecision.decision` is a free `String`** (`steer.rs:52`) rather than an enum; validity is enforced only by `eq_ignore_ascii_case` comparisons scattered across `steer.rs` and `bridge.rs`. A serde enum would move this to parse time (the tests at `steer.rs:655-676` round-trip only known values).
7. **Hand-rolled JSON string extractor** (`StreamingResponseExtractor`, `steer.rs:78-192`) duplicates escape/unicode decoding logic that a streaming JSON parser would own. It is well-tested (`steer.rs:660-719`) but is a maintenance liability for exotic escapes (e.g. lone surrogates are silently dropped at `steer.rs:147-155`).
8. **`models_conflict` treats empty model as conflicting with everything** (`preemption.rs:37-44`). Reasonable for "default model" semantics, but it means an unconfigured specialist stream is preempted by *any* steering arbitration.

### 6.5 Robustness gaps

1. **Freeze-on-write-failure is lossy** (`mod.rs:315-323`): if `journal.snapshot` fails, delegation proceeds with `worker_id = ""` and the subsequent `journal.clear` is skipped (`mod.rs:364-366` guards on non-empty). A crash then loses the in-flight task entirely — the comment's "fail loudly" intent is not realized (it warns and continues).
2. **`recover_frozen` handles only the first snapshot** (`mod.rs:397-399` uses `journal.frozen()`, which returns `list.into_iter().next()` at `freeze.rs:127-133`). Multiple concurrent frozen snapshots (supported by `snapshot`/`frozen_all`, `freeze.rs:105-141`) are recovered one per boot; the remaining ones stay frozen until subsequent boots. There is no `recover_frozen_all`.
3. **Watchdog is advisory in `AgentLoop`** (`loop.rs:309-314`): the deadline is checked only at phase boundaries inside the `run_turn` loop; a single long `spawn_blocking` write (e.g. a 10-minute `run_command`) cannot be interrupted by it — only by the abort flag *after* the dispatch returns (`loop.rs:467-471`).
4. **`ManagerLoop` has no watchdog at all**: `run_executing` is bounded only by `MAX_EXECUTING_ROUNDS` rounds; a single delegation that never returns (and never observes cancellation) blocks the round indefinitely. Workers do receive child tokens (`mod.rs:347`), so cooperative cancellation works only if the specialist checks the token.

### 6.6 Test coverage assessment

The test suites are unusually thorough and requirement-traceable (`REQ-*` tags throughout):

- `orchestrator/tests.rs` (609 lines): context isolation at the message level (`tests.rs:24-71`), check-off gating including stale-marker adversarial cases (`tests.rs:60-135`), fractal depth rejection (`tests.rs:137-160`), Deep-Freeze snapshot/clear/rehydrate/fail-loudly (`tests.rs:216-292`), handler signature/validation (`tests.rs:296-360`), config threading and domain-module guard (`tests.rs:362-407`), delegation event lifecycle (`tests.rs:409-427`).
- `manager/phase_tests.rs` (493 lines): phase gating with forced override, auto check-off success/failure, marker parsing precedence (`REPLAN` before `FAILED`, benign `0 failed` counters), archive no-op-when-incomplete, no stale-archive resurrection, auto-snapshot on completion.
- `manager/loop_tests.rs` (566 lines): turn-phase sequence, tool classification (including `delegate_task` as sequential), steer/abort injection, turn limit, repetition blocking through the loop, XML rescue, cross-loop stats aggregation, silent-dispatcher end-to-end with deterministic workers, one-task-per-call/no-takeover, parallel delegation, abort flag arming semantics, and mid-flight abort during a slow write tool (`loop_tests.rs:263-295`).
- `manager/context_tests.rs` (733 lines): prefix locking across compaction and rebirth, orphan-tool pruning, retry escalation 70%→50% with cap, over-limit 80% path, slow-prefill boundary semantics (300 s inclusive, 2-consecutive, 5-turn cooldown, reset-on-fast), advisory/compaction threshold interplay, factory isolation between specialists.

Notably, the deterministic worker (`run_specialist_llm` in `src/agents/runner.rs:10-37`) returns a canned deliverable echoing the brief with `MISSION COMPLETE` when no live backend is available (and bypasses live calls inside the test suite unless `MARMEL_LIVE_TEST` is set, `runner.rs:41-53`), which is what makes the end-to-end orchestrator tests hermetic.

### 6.7 Summary judgment

The subsystem is a coherent, heavily documented implementation of a fractal manager/specialist architecture with unusually strong requirement traceability and test discipline. Its main structural risks are concentrated in three places: (a) the synchronous-delegation bridge in `handle_delegate_task` (`mod.rs:624-663`), which creates per-call managers and threads; (b) plan check-then-act windows around parallel delegation (`loop.rs:699-776` vs. `phase.rs:445-503`); and (c) the clear-then-rearm abort-flag dance shared by both loops (`loop.rs:281-299`, `loop.rs:648-666`). None of these are latent crashes; they are duplication/lost-signal hazards under concurrency that the current single-manager, deterministic-worker usage pattern does not exercise.

---

*Report generated from static analysis of the workspace at commit state as of analysis time. All line numbers refer to the files as read during this session.*