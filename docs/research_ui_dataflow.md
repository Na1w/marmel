# What runtime data and events are actually available for a terminal UI in Marmel

Branch `feature/new-ui`, read-only inspection. Primary files: `src/ui/bridge/{action,sink,drain,arbiter,mod}.rs`,
`src/ui/session.rs`, `src/ui/transcript.rs`, `src/session/format/ui.rs`,
`src/orchestrator/{plan_summary,workers,bus,notice}.rs`.
Enum/type *definition* sites that those files only reference (`src/ui/mod.rs`, `src/ui/helpers.rs`,
`src/orchestrator/{delegation,steer,preemption}.rs`, `src/ui/tui/*`) are cited where needed and marked
*(ref.)* below — they are definition sites, not deep-dives.

---

## 1. Event / action vocabulary that reaches the UI

### 1.1 `Event` — the only renderer-facing enum (`src/ui/mod.rs:83-126`, *(ref.)*)

| Variant | Payload | Real producer(s) | Note |
|---|---|---|---|
| `Message(String)` | `ui/mod.rs:85` | streamed content: `bridge/sink.rs:115`; bound-stop banner `session.rs:2338,2351,3693`; steering-subtask deliverable `bridge/drain.rs:77-82` | streamed chunk **and** one-shot blocks share one variant |
| `SteerResponse(String)` | `ui/mod.rs:87` | arbitrator stream delta `bridge/sink.rs:511-515`; `bridge/drain.rs:38`; `bridge/drain.rs:277`; `bridge/sink.rs:462` | auto-journalled as `UiRecord::SteerResponse` at `session.rs:2049-2051` |
| `Thinking(String)` | `ui/mod.rs:88` | `bridge/sink.rs:119` (`StreamEvent::Thinking`) | reasoning deltas |
| `SubagentMessage { agent_tag, text }` | `ui/mod.rs:90` | `orchestrator/preemption.rs:233` | per-specialist streamed content, keyed by *stream tag*, not registry key |
| `SubagentThinking { agent_tag, text }` | `ui/mod.rs:95` | `orchestrator/preemption.rs:239` | resolves to a row via `helpers::subagent_row_key_for_tag` |
| `SubagentStreamPause { agent_tag, paused }` | `ui/mod.rs:109` | `orchestrator/preemption.rs:213`; folded into rows at `session.rs:1701-1706` | the only typed *panel-state* event in the channel |
| `ToolCall(String)` | `ui/mod.rs:114` | `session.rs:2541-2544,3104`, `session.rs:3086-3089` (display from `helpers::format_tool_call_display`, `ui/helpers.rs:147`) | flat string, no id/name/args structure |
| `ToolResult(String)` | `ui/mod.rs:116` | `session.rs:2941` | `is_error` is known (`session.rs:2873-2875`) but not carried |
| `Status(String)` | `ui/mod.rs:118` | process-wide via `orchestrator/bus.rs:4-6,96-104` → `status_rx` drains `session.rs:2044-2046,2229-2232,2689-2692,2792-2795,3285-3288,3371-3374`; direct emits e.g. `session.rs:2275,2434,2464,2981` | plain text; all worker progress prose arrives this way |
| `Delegation(DelegationEvent)` | `ui/mod.rs:119` | `ui/helpers.rs:1090` (drain), `session.rs:2534-2538`, `bridge/drain.rs:45-49,71-75`, `bridge/sink.rs:277-281,303-307` | see 1.2 |
| `Done` | `ui/mod.rs:121` | `session.rs:3816` | session finished |
| `TokensIn(usize)` | `ui/mod.rs:123` | `session.rs:2264,2274,2428,3120` | always `ctx.token_count()` of the **Manager** context |
| `TokensOut(usize)` | `ui/mod.rs:125` | **no production emit site** (only handler `ui/tui/mod.rs:556` and `ui/tui/tests.rs:5506`) | renderer estimates output tokens itself (`ui/tui/mod.rs:557-565`) |

Renderer surface (state pushes, not events): `set_subagents(Vec<SubagentDetail>)` `ui/mod.rs:183`,
`reset_active_agent()` `ui/mod.rs:185`, `poll_input()` `ui/mod.rs:152`,
capture/restore hooks `capture_ui` / `restore_panes` / `announce_restore` `ui/mod.rs:207,219,228`.

### 1.2 `DelegationEvent` — worker lifecycle (`src/orchestrator/delegation.rs:68-100`, *(ref.)*)

| Arm | Fields | Line |
|---|---|---|
| `Started` | `agent: Agent`, `task: Option<String>` | `:70` |
| `Queued` | `agent`, `task`, `position: usize`, `of: usize` (pool FIFO ticket) | `:80-85` |
| `Running` | `agent`, `task` (clears queue state) | `:89` |
| `Completed` | `agent`, `task` | `:91` |
| `Failed` | `agent`, `task`, `reason: Option<String>` | `:93-97` |

Canonical order `Started -> Queued* -> Running -> Completed | Failed` (`delegation.rs:60-66`); the vector is
drained exactly once by `ui/helpers.rs:1013-1096` (`events.drain(..)`, no replay).

### 1.3 `UiRecord` — the journal vocabulary (`src/ui/transcript.rs:18-43`), durable twin `UiJournalRecord` (`src/session/format/ui.rs:191-221`)

| Variant | Payload | Where appended |
|---|---|---|
| `User { text }` | `transcript.rs:20` | `session.rs:2023,2027,2100` |
| `Assistant { content?, thinking? }` | `transcript.rs:22-27` | `session.rs:2414-2418` — **final blocks only**, never deltas |
| `SteerResponse { text }` | `transcript.rs:29` | `session.rs:2050`; `bridge/drain.rs:113,201,281` |
| `ToolCall { display }` | `transcript.rs:31` | `session.rs:2543,3088` |
| `ToolResult { display }` | `transcript.rs:33` | `session.rs:2940-2942` |
| `TaskCompleted { task_id }` | `transcript.rs:35` | `session.rs:2930`; `bridge/drain.rs:99`; `ui/helpers.rs:1075` |
| `TaskFailed { task_id, reason? }` | `transcript.rs:37-41` | `session.rs:2901-2904`; `ui/helpers.rs:1082-1086` |
| `Status { text }` | `transcript.rs:43` | `session.rs:2984,3111,3558,3695,3998,4040,4071` (warnings, escalations, compaction, plan) |

`UiTranscript::extract_steering_history()` (`transcript.rs:90-140`) is the replay source for steering Q/A pairs
and classifies synthetic/system messages out of the pair set (`transcript.rs:97-105`).

### 1.4 Steering & arbitration vocabulary

| Type | Variants / fields | Site |
|---|---|---|
| `SteerArbEvent` (spawned arbitrator → UI) | `Delta(String)`, `DelegationStarted{agent,task_id,prompt}`, `DelegationCompleted{agent,task_id,deliverable}`, `SynthesizedAnswer{user_msg,answer}`, `DeferredSteer{user_msg,reason}`, `Finished{decision,user_msg,forwarded}` | `bridge/steer.rs:12-43` *(ref.)*; consumed `bridge/drain.rs:37-320` |
| Subtask action constants | `ACTION_FORWARD_NOTICE` / `ACTION_CANCEL` / `ACTION_DELEGATE_TASK` / `ACTION_SLEEP` (aliases of `SteerSubtaskAction`) | `bridge/action.rs:31-37`; routed `:40-60` |
| `SteerDecision` / `SteerSubtaskDecision` | `decision`, `response`, `tier`, `model`, `subtasks[]`, `sleep_seconds` / `tool_call_id`, `action`, `message`, `agent_name`, `prompt`, `sleep_seconds` | `orchestrator/steer.rs:67-78`, `:36-59` *(ref.)* |
| `NoticeTargeting` | `Addresses(Vec<String>)` (effective registry keys) / `Unroutable{token}` | `bridge/action.rs:143-149`, `:152-193` |
| `ForwardOutcome` / `ForwardedNotice` | `Posted(ForwardedNotice{notice_id,target,dropped_older})` / `Unroutable{token}` | `bridge/arbiter.rs:308-325`, `:336-407` |
| `SleepNotice` | `Started{sleep_secs}`, `Woke{sleep_secs}`, `Cancelled`, `Capped{slept_secs,extensions}` + `status_text()`/`delta_text()` | `bridge/arbiter.rs:441-497` |
| `SleepChain` / `SleepStop` | `extensions`, `slept_secs`, `stop: Option<SleepStop>`; `Extensions{extensions,limit}` / `Deadline{kind,slept_secs,limit_secs}` | `bridge/arbiter.rs:83-93,199-236` |
| `ArbitrationChannel` | `round_started(round)`, `decision_delta(delta)`, `sleep_notice(notice)` | `bridge/arbiter.rs:529-538`; impls `bridge/sink.rs:503-521`, `bridge/steer.rs:54+` |
| `ArbitrationOutcome` | `decision`, `sleep_cancelled`, `sleep: SleepChain` | `bridge/arbiter.rs:550-567` |
| `SteerContext` (arbitrator's view of runtime) | `main_goal`, `orchestrator_status`, `pending_approval`, `plan_progress`, `plan_content`, `available_agents`, `steering_history`, `user_message`, `active_subtasks` | `orchestrator/steer.rs:97-107` *(ref.)*; filled `bridge/arbiter.rs:603-613` |
| `NoticeReplyRejection` | `MissingNoticeId`, `UnknownNoticeId`, `AlreadyReplied`, `WorkerMismatch` + machine tag `kind()` | `orchestrator/notice.rs:779-813` |

### 1.5 Session verdicts the UI can see (typed in-process, prose on screen)

| Type | Variants | Site | Reaches UI as |
|---|---|---|---|
| `BoundStopReason` | `Deadline{kind,text}`, `TurnCap{limit,turn}` + `code()`/`label()` | `session.rs:78-124` | `Message` + `Status` + `UiRecord::Status` (`session.rs:3693-3695`) |
| `PlanGate` | `Read{pending,complete}`, `Unknown{warning}` | `session.rs:3919-3934`, `:3940-3986` | `Status` + `UiRecord::Status` (`surface_plan_warning` `session.rs:3991-4004`) |
| `CheckOffOutcome` | `Flipped`, `AlreadyChecked`, `NotInPlan`, `Unparseable{plan_path}`, `NoMarker`, `NotComplete{verdict}`, `IdMismatch{bound,marker}`, `Io{detail}`, `Refused{detail}` | `session.rs:4085-4122` | warning prose via `check_off_warning` `session.rs:4208-4235` |
| `CommitTrigger` / `CommitDecision` | T-1…T-10; `Written{members}`, `Throttled`, `Suppressed`, `Disabled`, `Busy`, `Failed` | `session.rs:292-360` | ledger only (`session_commit_ledger()` `session.rs:404`) — **not** surfaced |

---

## 2. Bridge plumbing: sink → drain → arbiter → UI state

| Step | Mechanism | Cite |
|---|---|---|
| 1. LLM stream → UI | `RendererSink` (implements `StreamSink`): `StreamEvent::Content/Thinking/Status` → `Event::Message/Thinking/Status`, then `flush()` | `bridge/sink.rs:84-124` |
| 2. Sink holds UI state it may touch | `pub renderer`, `steer_queue`, `arb_tx/arb_rx`, `stats`, `goal`, `subagents` (the session's authoritative row buffer), `plan`, `ctx`, `steering_history` | `bridge/sink.rs:84-106` |
| 3. Per-chunk control | `poll_control()`: flush → drain arbitration events with **no** row buffer (`None`) → `poll_input()` → `Abort` / `Pause{user_input}` / `Continue` | `bridge/sink.rs:130-175` |
| 4. Pause = arbitration | `on_pause()` builds `RendererChannel` and runs `arbiter::arbitrate_steering()`; renders rounds/deltas/sleep; posts notices once via `arbiter::post_forward_notices`; returns `PauseAction::{Resume,Abort}` | `bridge/sink.rs:177-497` |
| 5. Arbitrator internals | ≤`MAX_ARBITRATION_ROUNDS`(5) rounds; each round rebuilds `plan_progress` (`format_plan_progress_summary`), `active_subtasks` (`get_active_subtasks_str`), `orchestrator_status` from **registry liveness only**; preempts streams; applies `Cancel`; chained-sleep budget | `bridge/arbiter.rs:39,505-537,569-727` |
| 6. Spawned arbitrator → UI | its channel pushes `SteerArbEvent`; both hosts are *progress only* | `bridge/steer.rs:47-57` *(ref.)*, `bridge/arbiter.rs:520-538` |
| 7. Drain → renderer/queue/rows/journal | `drain_steer_arbitration_events[_with_transcript]`: `Delta`→`SteerResponse`; `DelegationStarted/Completed`→`Delegation`+`Status`+`Message`+`update_subagent_lifecycle`+`set_subagents`; `SynthesizedAnswer`→journal; `DeferredSteer`/`Finished`→`steer_queue`+`Status`; `Cancel`→`cancel_active_worker` then `project_subagent_rows` + `[cancelled by arbitrator]` audit line; `AbortImmediately`/`RejectPlan`→`cancel_all()`+`request_abort()` | `bridge/drain.rs:8-330` (esp. `:37-107`, `:116-126`, `:127-201`, `:229-241`, `:262-300`) |
| 8. Panel state ownership | Row **existence/liveness/queue** = registry projection (`helpers::project_subagent_rows`, `apply_registry_entry` `ui/helpers.rs:550-610`); **queue** = delegation drain (`ui/helpers.rs:1052-1067`); **pause** = `Event::SubagentStreamPause` (`session.rs:1701-1706`). Order is projection-then-drain: `project_then_drain_delegation_events` `session.rs:1672-1680`, called at `session.rs:2073,2245,2713,2816,3095,3309,3395` (plus bare drains `2944,3518`) | |
| 9. Sink-side pump while the loop is blocked | `await_with_panel_pump` re-projects every 250 ms around an in-sink await (steering-delegated subtask incl. pool slot wait) | `bridge/sink.rs:27,39-52,64-82,290-300` |
| 10. Loop edges that move state | `status_rx`/`event_rx` drains `session.rs:2044-2057` (repeated at `2229,2689,2792,3285,3371`); steer-arb drain `session.rs:1937,2059,2233,2705,2808,3301,3387,3710`; worker-edge harvest `take_worker_lifecycle_edge()` → `project_subagent_pane` `session.rs:2163-2171` (`mark_worker_lifecycle_edge` set from `workers.rs:1032`) | |

**Where steering input enters (three doors, no others):**

| Door | Path | Cite |
|---|---|---|
| Idle round boundary | `renderer.poll_input()` → abort/reset command, else `has_active_workers()` ? `spawn_steer_arbitration(...)` : `steer_queue.push` ; queue is appended as `Message::User` | `session.rs:2080-2146`, `2147-2149` |
| Mid-stream | `poll_control()` → `StreamControl::Pause{user_input}` → `on_pause()` arbitration | `bridge/sink.rs:145-170`, `:177` |
| Arbitrator → worker (not UI) | `ForwardNotice` → one notice per resolved registry key; unroutable targets are refused and reported | `bridge/action.rs:80-193`, `bridge/arbiter.rs:336-430` |

Steering history is process-global and **text-only**: `SharedSteeringHistory = Arc<RwLock<Vec<(String,String)>>>`
(`orchestrator/bus.rs:12,75-131`), entries correlated by notice id / exact inquiry (`bus.rs:144-200`), and the
bridge records it from `recorded_response()` (`bridge/arbiter.rs:782-836`).

---

## 3. Plan state

### 3.1 What `generate_plan_progress_summary` exposes (`src/orchestrator/plan_summary.rs:68-160`)

| Exposed | Detail | Cite |
|---|---|---|
| Task id | always re-rendered canonically as `[t-xxx]` (on-disk spelling irrelevant); a mid-line id stays mid-line | `:41-52`, `:19-37` |
| Task text | `TaskLine::description_body` (verbatim prose parentheticals kept) | `:53-58` |
| Role hint | **kept**, normalised lower-case token → `(role)` suffix | `:28-33`, `:59-64` |
| Validation directive | **dropped deliberately** (`(validate)` / `(no-validate)` never echoed) | `:20-27` |
| Checked / unchecked | two states only, from `plan_parse::CheckboxState` | `:82-101` |
| In-progress correlation | an unchecked task whose id matches a live worker moves to *Currently In Progress* with `(Assigned to: <agent_name>, Running: <duration>)` only | `:86-99`, via `workers::get_active_subtask_by_id` `workers.rs:1397-1421` |
| Plan clock | `Plan Started At: <HH:MM:SS> (running for <human>, <n> total seconds)` | `:112-121` |
| Aggregate | `Overall Progress: c/total (pct%)` + three section headers with `(n/total)` | `:123-158` |

Empty/absent plan strings: `"No active execution plan on disk."` / `"Execution plan contains no checklist items ([ ] or [x])."` (`:70-72`, `:105-107`).

### 3.2 What the session knows, fail-closed

* `PlanGate::Read{pending, complete}` vs `PlanGate::Unknown{warning}` — an unreadable/unparseable plan is never
  reported as finished (`session.rs:3919-3986`); surfaced via `Status` + `UiRecord::Status` (`session.rs:3991-4004`).
* Auto-nudge state is visible only as prose: `"Auto-nudge (n/5): Plan incomplete (pending: …)"` (`session.rs:2462-2467`).
* Check-off truth per delegation: `marker_gated_check_off()` (`session.rs:4130-4205`) → `CheckOffOutcome` (§1.5),
  rendered at `session.rs:2916-2926`.
* Durable plan *pane* witnesses: `plan_content`, `had_active_plan`, `active_plan_task`, `plan_start`,
  `plan_completed` (`session/format/ui.rs:56-80`) — display state, recomputed live.

### 3.3 Partial / streaming plan state — **not available**

* `generate_plan_progress_summary` takes a whole plan string and returns a whole string
  (`plan_summary.rs:68`); it is documented as **model-facing** (`plan_summary.rs:22-26`, fed to
  `SteerContext::plan_progress` at `bridge/arbiter.rs:606`).
* There is **no plan event type**: `Event` has no plan variant (`ui/mod.rs:83-126`) and `UiRecord` has none
  (`transcript.rs:18-43`).
* The TUI plan pane polls the file itself: mtime check ≥250 ms inside `draw()`
  (`ui/tui/render.rs:266-311`, `plan_content` assignment `:311`), plus one read at `init()` (`ui/tui/mod.rs:536-543`).
* So the finest observable granularity is: whole plan file text (polled) + one `TaskCompleted` / `TaskFailed`
  record per delegation + a `CheckOffOutcome` verdict.

---

## 4. Worker / subagent state

### 4.1 UI row model `SubagentDetail` (`src/ui/mod.rs:33-79`) vs registry read model `WorkerEntry` (`src/orchestrator/workers.rs:436-491`)

| Concept | On the row (UI-visible) | In the registry read model | Projected into the row? |
|---|---|---|---|
| Identity | `name` = effective registry key (`:39`) | `key`, `natural_key`, `collision_handle`, `disambiguated` (`:440-447`) | key only; badge derived at draw time (`ui/helpers.rs:627-650`, `ui/tui/render.rs:741-752`) |
| Task id | `task_id` (`:40`) | `task_id` (`:449`) | yes (`ui/helpers.rs:552`) |
| Role/agent | via `name` + `Agent` in events | `agent_name` (`:448`), `call_class: Specialist|ValidatorAudit|Placeholder` (`:422-427`) | **no** field; only the `Audit`/`Dup #N` badge |
| Prompt | `prompt` (`:41`) | `prompt` (`:460`) | yes (`:553-555`) |
| Liveness | `is_active` (`:55`) | `is_active` (`:456`), `completed_seq` (`:469`) | yes (`:556`) |
| Status label | — | `status_label` ("Approved"/"Revising"/"Failed"/"Aborted") (`:459`, set `:1018-1043`) | **no** |
| Elapsed | `started_at`, `worked_duration`, `last_activity_at` (`:42-48`) | `started_at`, `started_wall`, `elapsed_secs`, `completed_wall` (`:461-470`) | active: `started_at`; closed: `worked_duration=elapsed_secs` (`:568-573`); wall clocks **not** projected |
| Iteration | — | `implementation_turns` (`:476`) | **no** |
| Validation loop | — | `validation_rounds` (`:477`), `latest_validator_feedback` (`:478`, updated `workers.rs:1000-1016`) | **no** |
| Token counts | `context_tokens` (`:78`) | `context_tokens`, `last_tokens` (`:472-475`) | merged max (`ui/helpers.rs:557-564`) |
| Queue rank | `queue: Option<QueueState{position,of}>` (`ui/mod.rs:24-31,60`) | `queue_position` (`:490`, written `:1057-1142`) | yes (`:592-598`) |
| Arbitration pause | `paused` (`:76`) | — (preemption handshake) | yes, event-driven (`session.rs:1701-1706`) |
| Streaming text | `logs`, `thinking`, `content` (`:48-50`) | — | only via lifecycle/literal writes: `logs` gets `"started/completed task …"` (`ui/helpers.rs:736-740`) and `[cancelled by arbitrator]` (`bridge/drain.rs:186`); `content` set once from the deliverable (`session.rs:2886-2890`) |
| Nesting/parent | — | **absent by design** (`workers.rs:415-419`) | n/a |

Row change-detection signature is `(name, is_active, paused, context_tokens, queue)` (`ui/helpers.rs:663-691`) — clocks
and streamed text are deliberately outside it.

### 4.2 Streaming & status for a specialist

| Channel | Shape | Cite |
|---|---|---|
| Content/thinking deltas | `Event::SubagentMessage/SubagentThinking{agent_tag,text}` | `ui/mod.rs:90-100`, emitted `orchestrator/preemption.rs:233-245` |
| Slot yield | `Event::SubagentStreamPause` | `orchestrator/preemption.rs:213` |
| Iteration / validation progress | **prose only** on the status bus (`emit_status` from the runner fix loop, e.g. `"<tag> (turn N)"`, `"VALIDATION SKIPPED by design — …"`) | `orchestrator/bus.rs:96`; `agents/runner/fix_loop.rs:469,965` *(ref.)* |
| Full per-worker table (text) | `get_active_subtasks_str()` — turns, validation rounds, validator feedback, role/tools/skills | `workers.rs:1236-1355`; used by the arbitrator (`bridge/arbiter.rs:505-513`), not by the UI |
| Steering-delegated children | `SteerArbEvent::DelegationStarted/Completed` → `Delegation` events + row lifecycle + deliverable printed as `Event::Message` | `bridge/drain.rs:40-107`; `bridge/sink.rs:271-330`; `run_delegated_subtask` `bridge/arbiter.rs:740-763` |

### 4.3 What the pane can label today

`(Active|Idle|Paused|Queued n/m)` + ctx + elapsed + badge (`ui/tui/render.rs:717-752`), driven entirely by
`SubagentDetail` + per-frame `worker_entries()` read (`ui/tui/render.rs:655-660`).

---

## 5. Transcript / message model

| Aspect | Available today | Cite |
|---|---|---|
| Roles | `User`, `Assistant{content,thinking}`, `SteerResponse`, `Status`, `ToolCall`, `ToolResult`, `TaskCompleted`, `TaskFailed` | `transcript.rs:18-43`; durable twin `session/format/ui.rs:191-221` |
| Streaming deltas vs final | Deltas are **transient events only** (`Event::Message/Thinking/SteerResponse`); the journal gets one `UiRecord::Assistant` per turn from the last assistant message | `bridge/sink.rs:111-124`; `session.rs:2403-2420` |
| In-flight partial text survival | persisted only as pane bytes: `current_thought`, `current_content`, `steer_sentence_buffer`, `active_agent` | `session/format/ui.rs:83-105` |
| Display authority vs journal | `chat_messages`/`subagent_panels` are byte authority; `journal` is a steering mirror, never re-rendered | `session/format/ui.rs:14-16,30-45` |
| Tool calls | single pre-formatted string `name(args)` | `session.rs:2541-2544,3086-3089,3104`; `ui/helpers.rs:147` |
| Tool results | single string; `is_error` computed then used only for delegation failure classification and failure budget | `session.rs:2872-2905`, `2962-2985` |
| Tool-call identity | `tool_call_id` exists in context (`Message::Tool` `session.rs:2951-2953`) but is **not** in `Event::ToolCall/ToolResult` or `UiRecord` | `ui/mod.rs:114-116` |
| Malformed calls | error text fed back to model; no distinct UI marker | `session.rs:2546-2568` |
| Completion/failure markers | `MARKER_COMPLETE`/`MARKER_FAILED`/`MARKER_REPLAN` (`markers.rs:64-68` *(ref.)*) reach the UI **only** through `CheckOffOutcome`→`check_off_warning` prose (`NoMarker`, `NotComplete{verdict}`, `IdMismatch{bound,marker}`, `Unparseable`, `Io`, `Refused`) | `session.rs:4085-4122`, `4190-4204`, `4208-4235` |
| Failure reason extraction | `helpers::extract_failure_reason` → `DelegationEvent::Failed{reason}` + `UiRecord::TaskFailed{reason}` | `session.rs:2893-2905`; `ui/helpers.rs:1145` |
| Deliverable verdict object | `Deliverable{marker, content, task_id}` (`agents/mod.rs:168-172` *(ref.)*) travels in `SteerArbEvent::DelegationCompleted` | `bridge/steer.rs:19-23`; drain uses only `.content` (`bridge/drain.rs:77-92`) |
| Repeated-failure escalation | prose `Status` + `UiRecord::Status` | `session.rs:2976-2986`, `3550-3560` |
| Compaction outcome | success = one `Status`; failure = detailed numbers in one `Status` + `UiRecord::Status` | `session.rs:4053-4071` |

---

## 6. Gaps — data that exists but is not surfaced to the UI

| # | Data | Where it lives (already public) | Why it matters to an overview |
|---|---|---|---|
| 1 | Per-worker **iteration + validation loop** (`implementation_turns`, `validation_rounds`, `latest_validator_feedback`) | `workers.rs:476-478`, `update_active_worker_progress` `:1000-1016` | the loop's single most informative progress signal is prose-only today |
| 2 | Worker **status label** (`Approved`/`Revising`/`Failed`/`Aborted`) | `workers.rs:459`, `set_active_worker_status` `:1018-1043` | lets a pane say *why* a row is still open |
| 3 | Worker **call class** and identity structure (`call_class`, `natural_key`, `collision_handle`, `disambiguated`) | `workers.rs:422-427,442-447` | only surfaces as a badge; cannot be filtered/counted |
| 4 | Wall-clock start/finish of workers (`started_wall`, `completed_wall`, `elapsed_secs`) | `workers.rs:462,466,470` | timelines, per-task duration histogram |
| 5 | Recently **completed** workers as a list (`recent_completed_entries` `:587`, `active_worker_entries` `:571`, `live_worker_states` `:388`) | `workers.rs` | a "last finished" section; rows currently fade to `Idle` only |
| 6 | Per-specialist **token** counts (`last_tokens`, `get_active_worker_tokens` `:1152`, context string `:1158`) | `workers.rs` | rows carry one merged `context_tokens` number |
| 7 | **Notice/steering lifecycle state**: `NoticeLifecycleStats{pending_notices,inbox_keys,queued_notices,recorded_replies,expired_dropped_total,capacity_evicted_total,inboxes_reclaimed_total}` (`:216-225`), `worker_inbox_len` (`:245`), `peek_worker_inbox` (`:268`), `get_pending_notice` (`:1022`), `get_worker_reply` (`:1053`), `NoticeDrainOutcome` (`:656-664`), `NoticePostOutcome{evicted_older,prune}` (`:436-442`), rejection kinds (`:779-813`), TTL/capacity constants (`:126-151`) | `orchestrator/notice.rs` | "is my steer pending, delivered, answered, expired, or dropped?" is currently only inferable from status prose (`bridge/arbiter.rs:297-306`, `bridge/sink.rs:235-253`) |
| 8 | Steering history as **structure** — today `Vec<(String,String)>` text only | `orchestrator/bus.rs:12,144-200`; `bridge/arbiter.rs:782-836` | cannot show per-notice state, target worker, or decision kind without re-parsing text |
| 9 | Arbitration **round number** and sleep chain as data (`round_started(round)` ignores `round` in both hosts; `SleepChain{extensions,slept_secs,stop}`) | `bridge/arbiter.rs:529-537,199-215`; impls `bridge/sink.rs:504`, `bridge/steer.rs:55` | "round 3/5, slept 40s, capped at extensions" is a status string, not a field |
| 10 | `Deliverable.marker` and `task_id` (terminal verdict object) | `agents/mod.rs:168-172`; carried by `SteerArbEvent::DelegationCompleted` `bridge/steer.rs:19-23` | UI could colour a row `COMPLETE/FAILED/REPLAN` instead of scanning prose (`bridge/drain.rs:77-92` uses `.content` only) |
| 11 | `CheckOffOutcome` / `PlanGate` / `BoundStopReason::code()` as **typed** UI data | `session.rs:4085-4122`, `3919-3934`, `91-104` | verdicts collapse to `Status` strings; a UI cannot build a plan-health indicator |
| 12 | Commit/session-health ledger (`session_commit_ledger()`, `CommitDecision{Written{members},Throttled,Suppressed,Busy,Failed}`) | `session.rs:338-415` | restore-safety indicator invisible to UI |
| 13 | Queue **rank of nested children** as durable registry data vs one-shot events | `workers.rs:479-490` + `helpers.rs:592-598` vs `delegation.rs:80-85` | already partly fixed; still no event carries the registry rank |
| 14 | `TokensOut` never emitted; output tokens are renderer-estimated | `ui/mod.rs:125`; no emit site; `ui/tui/mod.rs:556-565` | token accounting is display-local and inconsistent with `TokensIn` |
| 15 | Durable pane model cannot express queue/pause state | `session/format/ui.rs:145-180` (`SubagentPanel` has no `queue`/`paused`) | after restore, a queued or paused worker reads as plain active/idle |
| 16 | Tool-call structure (`tool_call_id`, tool name, args, `is_error`, malformed flag) | `session.rs:2541-2568`, `2872-2875`, `2951-2953` | no way to group call→result pairs or flag failures in a transcript view |
| 17 | Subagent `logs` are sparse literals, not the worker's status stream | `ui/helpers.rs:736-740`; `bridge/drain.rs:186` | per-worker timeline has ~2 lines per task |

**Cheapest surfacing routes (already sanctioned):** the registry read model `orchestrator::worker_entries()`
is already read per frame by the TUI (`ui/tui/render.rs:655-660`) and by the bridge
(`bridge/action.rs:88`), so items 1-6 need only extra fields on `SubagentDetail` + `apply_registry_entry`
(`ui/helpers.rs:550-610`); items 7-10 need one new typed `Event`/`UiRecord` variant pair fed from the bridge
(`bridge/drain.rs`, `bridge/arbiter.rs`) without inventing a second state owner.

---

MISSION COMPLETE
