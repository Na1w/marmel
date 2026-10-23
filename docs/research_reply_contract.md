# `reply_to_arbitrator` — execution-path map (read-only research)

Workspace: `/home/fredrik/marmel/wip` (marmennill / marmel).
Method: code inspection only (no edits, no web search). Every claim below carries a
`file:line` reference that was opened and read. Line numbers are from the current
working tree (unmodified).

Purpose: give an implementer a precise map of the **bidirectional** round trip
`user → arbitrator → worker` and `worker → arbitrator → user`, plus a ranked list of
the places where that round trip can currently break **silently**.

---

## 0. Contract at a glance

| Concern | Owner | Authority |
|---|---|---|
| Notice id allocation / store / TTL / caps | `src/orchestrator/notice.rs` | single source of truth (`PENDING_NOTICES`, `WORKER_INBOXES`, `REPLIED_NOTICES`) |
| Notice → worker routing (which inbox a worker drains) | `src/orchestrator/notice.rs:544-651` (`WorkerRoutingIdentity::routes`) | exact-field matching, no substrings |
| Arbitrator decision → subtask action | `src/orchestrator/steer.rs:325-348, 457` (normalizers) | the only vocabulary source |
| Target resolution for a steer subtask | `src/ui/bridge/action.rs:91-151` | registry-driven (`worker_entries()`, `is_active`) |
| Worker reply acceptance | `src/orchestrator/notice.rs:898-1000` (`record_worker_reply_for_notice`) | strict: id + addressee must match, no guessing |
| Arbitrator turn over a worker reply | `src/orchestrator/notice.rs:1151-1258` (`evaluate_worker_reply`) | LLM round trip, 60 s timeout |
| User-visible output of the arbitrator answer | `src/orchestrator/bus.rs:106-111` → `src/ui/session.rs` event drain | global `EVENT_SENDER` |
| Tool schema + advertising | `src/types/tools.rs:385-408`, `src/agents/runner/fix_loop.rs:337, 586-625` | always advertised |
| Dispatch of the tool call | `src/harness/mod.rs:588-590, 698-700, 918, 1057` | gate bypass at `:630`, `:994` |

---

## 1. Direction A — user → arbitrator → worker (notice posting)

### A1. Session loop bootstrap (the channel the whole UI depends on)

1. `src/ui/session.rs:1818-1824` — the session loop creates `status_tx` / `event_tx` /
   `steer_arb_tx` channels plus the shared `steering_history`, and registers them
   globally: `orchestrator::set_status_sender`, `set_event_sender`,
   `set_steering_history`.
   - If this registration never happens (or a different session object is live),
     `bus::emit_event` (`src/orchestrator/bus.rs:106-111`) and
     `bus::record_steering_exchange` (`src/orchestrator/bus.rs:144-151`) **return
     silently**. No error, no log.
2. `src/ui/session.rs:2130-2143` — a mid-flight user steer is arbitrated **only** when
   `crate::orchestrator::has_active_workers()` is true; otherwise it is pushed to
   `steer_queue` (becomes a plain `Message::User` at `src/ui/session.rs:2146-2148`)
   and **no notice is ever created**.
3. `src/ui/session.rs:2836-2845` and `src/ui/session.rs:3415-3424` — the two inner
   `poll_input` sites (Manager parked inside a tool call) call `spawn_steer_arbitration`
   **without** the `has_active_workers()` guard. The rationale comment at
   `src/ui/session.rs:2108-2129` explains the guard removal is intentional, but the
   consequence is that a `ForwardToWorker` decision can be posted into an inbox that no
   live worker will ever drain.

### A2. Arbitration host (spawned-task host) → decision → notice

4. `src/ui/bridge/steer.rs:66-216` `spawn_steer_arbitration(...)`:
   - builds an `EventChannel` (`src/ui/bridge/steer.rs:74-91`) that pushes
     `SteerArbEvent::Delta` onto `steer_arb_tx`;
   - runs `arbiter::arbitrate_steering` (`src/ui/bridge/steer.rs:104-115`,
     implementation `src/ui/bridge/arbiter.rs:446-604`);
   - terminal-`Sleep` durability: `SteerArbEvent::DeferredSteer`
     (`src/ui/bridge/steer.rs:122-136`);
   - delegation branch `src/ui/bridge/steer.rs:138-172`;
   - **notice posting** `src/ui/bridge/steer.rs:174-198`:
     ```rust
     if normalize_steer_decision(Some(&d.decision)) == "ForwardToWorker" {
         for (st, subtask_action) in action::routed(Some(d)) {
             if subtask_action != action::ACTION_FORWARD_NOTICE { continue; }
             let target = action::notice_target(st);                       // steer.rs:183
             let msg_to_send = st.message.as_deref().unwrap_or(&msg);      // steer.rs:184
             let posted = arbiter::post_notice_observable(&target, msg_to_send); // :185
             // Delta: "Forwarded notice {id} to {target} — awaiting specialist reply."  :186-189
             forwarded_notices.push((posted.notice.notice_id, target));    // :196
         }
     }
     ```
   - history entry: `src/ui/bridge/steer.rs:200-210` using
     `arbiter::recorded_response(decision, synthesized, &forwarded_notices)`
     (`src/ui/bridge/arbiter.rs:649-678`, text at `:671`
     `"Forwarded notice {id} to {target} (awaiting specialist reply)"`, fallback with **no id** at `:675`);
   - finally sends `SteerArbEvent::Finished { decision, user_msg }`
     (`src/ui/bridge/steer.rs:212-215`).

5. The worker snapshot the arbitrator sees is the **registry**, not pane rows:
   `arbiter::active_subtasks_render` (`src/ui/bridge/arbiter.rs:382-388`) →
   `orchestrator::get_active_subtasks_str` (`src/orchestrator/workers.rs:1046-1085`).
   Critical detail: `src/orchestrator/workers.rs:1079` renders
   `Tool Call ID: {info.task_id.unwrap_or(id)}`, i.e. the value the arbitrator is told
   to echo into `tool_call_id` is a **task id** (`t-001`), not a provider tool-call id.

### A3. Target resolution (this is where targeting is decided)

6. `src/ui/bridge/action.rs:40-42` `route()` → `orchestrator::steer::normalize_steer_subtask_action`
   (`src/orchestrator/steer.rs:457`, alias table `src/orchestrator/steer.rs:433`).
7. `src/ui/bridge/action.rs:91-110` `addressed_worker_keys(agent, task)` — filters
   `orchestrator::worker_entries()` (`is_active`) whose `routing_identity` routes
   **both** the cleaned agent name **and** the cleaned task.
8. `src/ui/bridge/action.rs:113-131` `routing_identity` pins the authoritative
   `agent_name` / `task_id` off the registry entry.
9. `src/ui/bridge/action.rs:141-151` `notice_target(subtask)`:
   ```rust
   match addressed_worker_keys(subtask.agent_name.as_deref(), Some(&subtask.tool_call_id)) {
       [only] => that effective key,
       many   => joined keys,
       []     => subtask.agent_name.unwrap_or(subtask.tool_call_id),  // fallback
   }
   ```
   The effective registry key comes from
   `orchestrator::register_active_worker_with_token`
   (`src/orchestrator/workers.rs:611-650`, base key `{agent}-{task}` or `{agent}-w{id}`,
   slot disambiguation `{base}#{n}` via `KEY_DISAMBIGUATOR = '#'` at
   `src/orchestrator/workers.rs:163, 717`), and is handed to the runner as
   `_active_guard.0` (`src/agents/runner/execution.rs:302`).

### A4. Store write

10. `src/ui/bridge/arbiter.rs:278-294` `post_notice_observable(target, inquiry)` →
    `notice::post_notice_to_worker_tracked(target, inquiry, None)`.
11. `src/orchestrator/notice.rs:459-519` `post_notice_to_worker_tracked`:
    - `:466-468` optional `notice_id_override` (used only by the follow-up path, §3 B5);
    - `:472` `clean_target = target.trim().to_ascii_lowercase()` — the **post target is
      the inbox key**;
    - `:480` insert into `PENDING_NOTICES`;
    - `:484-496` push into `WORKER_INBOXES[clean_target]`, drop-oldest at
      `INBOX_CAPACITY = 64` (`notice.rs:142`);
    - `:498-512` evicted notices are removed from `PENDING_NOTICES` (so an evicted id
      later replies as `unknown-notice-id`);
    - id allocation: `notice.rs:50-53` `next_notice_id()` → `notice-{n}` (`:31`
      `NEXT_NOTICE_ID`, floor seeding `:40-42`).
12. Every post and every drain runs `prune_notice_storage`
    (`src/orchestrator/notice.rs:323-427`): TTL `NOTICE_TTL_MS = 10 * 60 * 1000`
    (`:126`), `INBOX_IDLE_TTL_MS` (`:132`), `MAX_INBOX_KEYS = 256` (`:147`),
    `REPLIED_NOTICES_CAP = 64` (`:151`), expiry helper `:198-200`.

### A5. Delivery into the worker's context

13. `src/agents/runner/fix_loop.rs:821-835` `inject_drained_notices` appends a
    `Message::User` built by `notice::render_notice_for_worker`
    (`src/orchestrator/notice.rs:1042-1051`), which prints
    `ID: {notice_id}` and instructs the model to call
    `reply_to_arbitrator` with exactly that id.
    - turn start: `fix_loop.rs:838-843`;
    - mid-turn (after each tool round): `fix_loop.rs:845-860` via
      `drain_worker_notices_mid_turn`.
    - both call sites pass `&_active_guard.0`, the **effective registry key**
      (`src/agents/runner/execution.rs:493` and `:1113`).
14. Drain: `src/orchestrator/notice.rs:674-688` (`drain_worker_notices`,
    `drain_worker_notices_mid_turn`) → `:690-725` `drain_worker_notices_report`:
    selects every inbox key where `identity.routes(key)`
    (`notice.rs:595-612` + `:623-651`), removes those inboxes, sorts by `created_at_ms`.
15. Inbox reclaim on worker exit: `notice.rs:745-768` `reclaim_worker_inbox`
    (called from `ActiveWorkerGuard::drop`) removes **only empty** entries, so a notice
    queued for a dead worker survives until the TTL prune.

### A6. The three posting hosts (one decision can post twice)

| Host | Where arbitration runs | Where the notice is posted | History entry recorded |
|---|---|---|---|
| spawned-task host | `src/ui/bridge/steer.rs:104-115` | `src/ui/bridge/steer.rs:185` (per subtask, uses `st.message`) | `src/ui/bridge/steer.rs:200-210` **with ids** |
| drain host (same steer, consumes `SteerArbEvent::Finished`) | — | `src/ui/bridge/drain.rs:225-248`, post at `:233` (**raw `user_msg`**, target `unwrap_or("worker")`) | none |
| paused-stream sink host | `src/ui/bridge/sink.rs:196-210` | `src/ui/bridge/sink.rs:389-412`, post at `:397` | `src/ui/bridge/sink.rs:226` (`recorded_response(..., &[])` → **no id**) |

The spawned-task host and the drain host are **the same steer**: `steer.rs:212-215`
emits `Finished { decision, user_msg }` and `drain.rs:127+` consumes it and posts a
second notice (drain call sites: `src/ui/session.rs:1937, 2059, 2233, 2705, 2808, 3301,
3387, 3710`; and `src/ui/bridge/sink.rs:148`). See ranked break point **#2**.

### A7. Arbitrator prompt assumptions

`prompts/steer_arbitrator.md:28` (ForwardToWorker: "You must specify the subagent's
tool_call_id"), `:51` (`"tool_call_id": "the Tool Call ID of the target subagent"`),
and `:54` documents `agent_name` as *"(for DelegateTask)" only* — the arbitrator is
never told to fill `agent_name` for a `ForwardNotice` subtask, which is exactly the
field `notice_target` depends on when `tool_call_id` fails to resolve.

---

## 2. Direction B — worker → arbitrator → user

### B1. Tool call reaches the handler

1. Specialist async dispatch: `src/harness/mod.rs:698-700`
   `TOOL_REPLY_TO_ARBITRATOR => handle_reply_to_arbitrator_async(&caller_str, &tool.arguments)`
   where `caller_str = caller.role_name()` (`src/harness/common.rs:107-113`, i.e. the
   bare role name such as `coder`).
2. Manager async dispatch: `src/harness/mod.rs:588-590` with the literal `"Manager"`.
3. Sync twins: `src/harness/mod.rs:918` (Manager) and `:1057` (specialist) →
   `handle_reply_to_arbitrator` (`src/harness/mod.rs:869-874`, which just
   `block_on_safe(handle_reply_to_arbitrator_async(...))` at `:873`, definition
   `:286-310`).
4. Gate bypass: `src/harness/mod.rs:629-646` (async) and `:993-1010` (sync) —
   `if gate_name == TOOL_REPLY_TO_ARBITRATOR { true }`, so the reply tool executes even
   when the caller's allow-list does not contain it. `TOOL_CREATE_PLAN` is refused for
   specialists at `src/harness/mod.rs:612-615`.
5. MCP ordering asymmetry: in the **specialist** path `mcp_route(name)` is consulted at
   `src/harness/mod.rs:622` *before* the built-in match; in the **Manager** path MCP is
   consulted *last* (`src/harness/mod.rs:600-607`). Shadowing is prevented by the gate
   itself (`resolves_to_builtin_handler` check inside `McpDispatchGate::route`,
   `src/harness/mod.rs:263` region, warning "…owned by the built-in dispatcher, which
   resolves first"), so this is an inconsistency rather than an active bug.
6. Alias folding: `canonical_dispatch_name` (`src/harness/mod.rs:94`, used at `:663`,
   `:1026`) → `tool_names::canonical_tool_spelling`
   (`src/tool_names.rs:234-236`), and `TOOL_REPLY_TO_ARBITRATOR` is in
   `CANONICAL_TOOL_NAMES` (`src/tool_names.rs:45-46, 133`), so alias spellings resolve
   to the same arm.

### B2. Strict reply contract (the gate most failures land on)

`src/harness/mod.rs:738-867`:

| Step | Line | Behaviour |
|---|---|---|
| id extraction, `""` when absent | `:745-748` | deliberate: an absent id is fed to the strict store call so the model gets the `missing-notice-id` class |
| `message` mandatory | `:749-755` | `ToolError::BadArguments { detail: "missing mandatory string field `message`" }` |
| worker identity | `:757` → `reply_worker_identity` `:726-728` | **`caller.trim()`** — the bare role name, *not* the effective registry key (doc comment `:733-737` admits "the role name is the best available identity") |
| record | `:758-766` | `record_worker_reply_for_notice(&worker_key, id, message)` |
| rejection → tool error | `:767-776` | `ToolError::BadArguments` with `"reply rejected ({kind}): {rejection} — nothing was recorded and no notice was resolved; send the exact notice_id shown as ID: <notice_id>"` |
| status ping | `:779-781` | `[{caller}] Replied to notice {id} — Steer Arbitrator evaluating...` |
| arbitrator client | `:783-787` | **fresh** `ChatClient::from_config(config::get_active().or_else(load).unwrap_or_default())` |
| arbitrator turn | `:789-798` | `evaluate_worker_reply(..., |_delta| {})` — streamed deltas are **discarded** |

Rejection classes and their source (`src/orchestrator/notice.rs:779-862`, `:898-1000`):

| Class (`kind()`, `notice.rs:805-812`) | Raised at | Meaning in the current design |
|---|---|---|
| `missing-notice-id` | `notice.rs:918-920` | empty/whitespace id |
| `unknown-notice-id` | `notice.rs:922-938` **and** `:964-969` | not in `PENDING_NOTICES`; includes *expired (10 min TTL)*, *capacity-evicted*, *inbox-key never matching*, and the `remove_if` race |
| `already-replied` | `notice.rs:925-933` | id is in `REPLIED_NOTICES` (ring cap 64, `:963-997`) |
| `worker-mismatch` | `notice.rs:940-946` | pending notice exists but `notice_addresses_worker` fails (`:856-862`, evaluated **both directions**) |

Every rejection emits a `tracing::warn!` (`notice.rs:907-913`) — so a broken round trip
*is* observable in logs, but not in the UI.

Atomic consumption: `PENDING_NOTICES.remove_if` (`notice.rs:951-959`), then queued
duplicates are dropped by id across all inboxes
(`drop_queued_notice_copies`, `notice.rs:867-888`), then the reply is stored in
`REPLIED_NOTICES` (`notice.rs:963-997`).

### B3. Arbitrator turn over the reply

`src/orchestrator/notice.rs:1151-1258`:
- prompt `WORKER_REPLY_SYSTEM_PROMPT` (`:1090-1110`), `decision ∈ {SynthesizeResponse,
  AskFollowUp}`;
- call class `NoticeEval` (`:1143-1146`, class defined `src/orchestrator/pool.rs:820-860`,
  reserve semantics `:837-848`, `INTERNAL_QUEUE_WAIT_SECS = 20` at `:73`,
  `acquire_reserve` fail-open at `:723-748`) — deliberately *not* the `Specialist`
  class, because the call is issued **inside the specialist's own tool round**
  (`notice.rs:1188-1194`);
- 60 s timeout (`:1195-1205`); on timeout/error `raw = ""` (`:1215`);
- JSON extraction `:1219-1231`, parse `:1233`;
- **fallback**: any parse failure yields `Ok(WorkerReplyEvaluation { decision:
  "SynthesizeResponse", response: Some("Specialist [{tag}] explains: {reply}") })`
  (`:1244-1257`). A dead/blank backend therefore still produces a user-visible line —
  the failure is noisy, not silent.

### B4. Back to the user

- `SynthesizeResponse`: `src/harness/mod.rs:832-834`
  `emit_event(Event::SteerResponse("\n[Arbitrator]: {resp}\n\n"))`; history at
  `:840-844`; tool result at `:845-848`.
- evaluation error: `src/harness/mod.rs:851-865` emits the same shape of fallback line.
- `bus::emit_event` (`src/orchestrator/bus.rs:106-111`) → the session loop drains
  `event_rx` at `src/ui/session.rs:2044-2057` and `:2689-2700` (also `:2797`, `:3290`,
  `:3376`), appends `UiRecord::SteerResponse` to `ui_transcript` and calls
  `forward_ui_event`.
- Event type `src/ui/mod.rs:87`; renderer sinks `src/ui/tui/mod.rs:590`,
  `src/ui/raw.rs:99`; journal mapping `src/ui/session.rs:1494-1495`;
  journal twin `src/session/format/ui.rs:201`.
- Transcript read-back: `src/ui/transcript.rs:29` (`UiRecord::SteerResponse`),
  `:90-140` `extract_steering_history` (strips the `[Arbitrator]:` prefix at
  `:110-113`, decides pending-ness with `steer::steering_entry_is_pending` at `:123`).

### B5. Follow-up (arbitrator → worker, second hop of the round trip)

`src/harness/mod.rs:800-827`:
```rust
if eval.decision.eq_ignore_ascii_case("AskFollowUp") && let Some(follow_up) = eval.follow_up_prompt {
    let follow_up_notice = post_notice_to_worker(caller, &follow_up, Some(notice_id)); // :805
    // user_status → Event::SteerResponse (:807-810) else emit_status (:811-815)
    // history record with "Forwarded follow-up {id} to {caller}: … (awaiting specialist reply)" (:816-824)
    // tool result tells the worker: "…Check notices and reply when ready." (:825-828)
}
```
Two properties matter:
- **target = the bare role name (`caller`)**, not the effective registry key the
  original notice was posted to;
- **`notice_id_override = Some(notice_id)`** (`notice.rs:466-468`) reuses the *same* id.
  Since the original id was just consumed from `PENDING_NOTICES` (`notice.rs:951-959`)
  and pushed into `REPLIED_NOTICES`, the same id is pending again. A second reply that
  races the re-post is rejected `already-replied`
  (`notice.rs:925-933`), and the `REPLIED_NOTICES` entry is never cleared.

### B6. History correlation

`bus::record_steering_exchange` (`src/orchestrator/bus.rs:144-191`) tries: (1) the most
recent *pending* entry naming the id as a whole token
(`steer::entry_names_notice_id`, `src/orchestrator/steer.rs:509-521`), (2) exact
inquiry equality (`:530-534`), (3) append. "Pending" is decided by
`PENDING_ENTRY_MARKERS = ["awaiting specialist reply", "ForwardToWorker", "follow-up"]`
(`src/orchestrator/steer.rs:478-488`) — prose outside that vocabulary leaves the
placeholder unresolved (documented coupling at `notice.rs:1084-1089`).

---

## 3. Notice/id store invariants worth knowing before fixing anything

- ids: `notice-{n}` only (`src/orchestrator/notice.rs:50-53`); floor reseeded on restore
  (`:40-42` ← `src/session/worker_journal.rs:707, 721-724`,
  `src/session/format/envelope.rs:160-162`, captured at `src/ui/session.rs:449`).
- **Notices themselves are non-restorable**: `src/session/format/restore.rs:162, 212, 226`
  (`NonRestorableKind::Notice` / `NonRestorableNotice`).
- inbox map key = **post target, lower-cased** (`notice.rs:94, 472`).
- broadcast targets `"*"` and `"worker"` (`notice.rs:533` `BROADCAST_TARGETS`).
- matching (`WorkerRoutingIdentity::routes`, `notice.rs:623-651`): broadcast, effective
  key, natural key (`split_natural_key`, `:567-592`, strips `-wN` and the trailing
  task-id token), agent name / role-family prefix `{target}-`, and task-id equality.
  Disambiguator `#` (`notice.rs:526`).
- caps: 64 notices/inbox (`:142`, drop-oldest), 256 inbox keys (`:147`), 64 replied
  notices (`:151`), TTL 10 min (`:126`).
- `registry.rs` / `bus.rs` / `pool.rs` hold **no** notice id indirection:
  `src/orchestrator/registry.rs` only decides tool permission
  (`SpecialistEntry::allows` hard-codes `reply_to_arbitrator` at `:109-111`, consumed by
  `caller_allows_tool`, `src/orchestrator/delegate.rs:234-239`);
  `src/orchestrator/bus.rs` only carries status/event/history senders;
  `src/orchestrator/pool.rs` only meters the arbitrator call. So an unresolvable id can
  only come from *posting to a key nobody drains*, *expiry/eviction*, or *the worker
  never receiving the rendered id* — not from registry indirection.

---

## 4. Tool schema / registration / assembly (verified working)

- Schema: `src/types/tools.rs:385-408` — `notice_id` **and** `message` both required.
  Included in `default_tools()` (`:480`) and `manager_tools()` (`:502`).
- Name/alias: `src/tool_names.rs:45-46`, canonical list `:133`, normalization
  `:222-236`, BUILTIN table derived `:354-386`.
- Advertising to specialists: `src/agents/runner/fix_loop.rs:337`
  `ALWAYS_ADVERTISED_TOOLS = [TOOL_REPLY_TO_ARBITRATOR]`; class
  `BlueprintToolClass::AlwaysAdvertised` (`:373-404`); `assemble_tools` includes it
  unconditionally (`:586-625`, decision at `:594-595`); per-caller wrappers
  `assemble_tools_for_caller` (`:525-534`) and `advertised_tools_for_caller` (`:542-552`).
- Runtime assembly: `src/agents/runner/execution.rs:126-133`
  `specialist_advertised_tools(...)` called at `:277`, list moved into the context
  engine at `:286-287`; caller identity built once at `:213` / `:108-116`.
- Per-role blueprints list it too: `src/agents/catalog.rs:140, 164, 182, 199, 222, 240`
  and `src/agents/{coder,planner,researcher,debugger,validator}.rs`
  (`coder.rs:38`, `planner.rs:34`, `researcher.rs:37`, `debugger.rs:44`,
  `validator.rs:69`); Manager side `src/manager/context.rs:138`.

**Conclusion: "schema not assembled" is NOT a plausible cause here.** The tool is
advertised, allow-listed, and dispatched on all four paths (async/sync ×
Manager/specialist).

Note on the brief: `src/agents/runner/assembly.rs` is **deliverable assembly**
("Deliverable assembly and revision tracking", `assembly.rs:1-4`) — it has nothing to do
with tool lists. The real tool assembler is `src/agents/runner/fix_loop.rs:586`.

---

## 5. Ranked list: where the round trip silently breaks today

Ranked by (likelihood × silence). Each entry states the evidence and the observable
symptom.

**1. `notice_target` hands the arbitrator's `tool_call_id` to the registry as a *task
id* — a mismatch drops the notice into a key nobody drains.**
`src/ui/bridge/action.rs:141-151` resolves with
`addressed_worker_keys(agent_name, Some(&tool_call_id))`; the arbitrator's
`tool_call_id` is what it was told to copy — the registry "Tool Call ID" column
(`src/orchestrator/workers.rs:1079` renders `task_id.unwrap_or(id)`) — which is only
sometimes the effective key, and `split_natural_key` (`notice.rs:567-592`) will not
rescue a model-invented value. If the pair does not match, the fallback is
`agent_name` — but `prompts/steer_arbitrator.md:54` asks for `agent_name` only for
`DelegateTask`, so the fallback collapses to the bogus `tool_call_id` string. Result:
`WORKER_INBOXES["<bogus>"]` (posted fine, `notice.rs:484-496`) is never selected by
`identity.routes` (`notice.rs:696-700`) → the worker never sees `ID: …`, and any
id it guesses comes back `unknown-notice-id` (`notice.rs:922-938`).
Silence: the user is told "Forwarded notice … to …" (`steer.rs:186-189`) and "Notice
… forwarded to … — waiting for specialist reply" (`drain.rs:235-238`). Nothing ever
comes back. Only a `tracing::warn!` (`notice.rs:907-913`) appears later.

**2. One steer posts two notices: the spawned host posts at `steer.rs:185`, the drain
host posts again at `drain.rs:233`.**
`steer.rs:212-215` emits `Finished { decision, user_msg }`; `drain.rs:225-248` re-enters
the `ForwardToWorker` branch for the *same* decision and posts
`post_notice_observable(&target, &user_msg)` with a **new id** and the **raw user
message** (not `st.message`), with target from `action::first(...)` else
`"worker"` (a broadcast target, `notice.rs:533`). It also pushes `user_msg` onto
`steer_queue` (`drain.rs:247`), so the raw instruction additionally becomes a Manager
turn (`session.rs:2146-2148`). The worker sees **two ids for one instruction**, and
`record_steering_exchange` only knows the first id (`steer.rs:200-210` +
`arbiter.rs:668-674`). Symptom: replies that "work" but never update the visible
steering entry, or the arbitrator answers twice with different wording.

**3. The follow-up hop loses exact addressing and reuses the id.**
`src/harness/mod.rs:805` posts the follow-up to the **bare role name** (`caller`) with
`Some(notice_id)`. With two live workers of one role (`coder-t-001`, `coder-t-002`) the
role-family prefix routes the follow-up to both (`notice.rs:623-651`), whichever worker
drains first consumes it, and the *other* worker's reply is then rejected
`worker-mismatch` or `already-replied` (`notice.rs:925-946`). Reusing the id also means
the reply for the follow-up is indistinguishable from the reply for the original notice
in `REPLIED_NOTICES` (`:963-997`).

**4. Worker reply identity is the bare role name, while notices are posted to the
effective key.**
`src/harness/mod.rs:726-728` returns `caller.trim()` (`ToolCaller::role_name()`,
`src/harness/common.rs:107-113`); the runner knows the effective key
(`execution.rs:302`, `_active_guard.0`) but the harness never receives it. The only
thing keeping this working is the **bidirectional** identity check
(`notice.rs:856-862`, `notice_addresses_worker` tries both directions). That
bidirectionality is also what makes two workers of the same role indistinguishable to
the reply contract: a `coder` reply can claim any `coder`-addressed notice, and a
Manager reply (caller `"Manager"`) can only legitimately answer `"*"`/`"worker"`
notices.

**5. Arbitration is gated at one site and ungated at two others.**
`session.rs:2130` requires `has_active_workers()`; `session.rs:2836` and `:3415` do not.
Either the steer is never arbitrated (no notice at all — it silently becomes the next
Manager turn, `session.rs:2141-2143`), or it is arbitrated against a registry with no
live worker (`workers.rs:1005`) and the notice lands in an inbox that will only ever be
reclaimed empty (`notice.rs:745-768`) or TTL-dropped.

**6. Session restore makes every previously displayed notice id invalid.**
`src/session/format/restore.rs:162, 212, 226` classify notices as non-restorable; only
the id **floor** is reseeded (`session/worker_journal.rs:707, 721-724`;
`session/format/envelope.rs:160-162`; captured at `session.rs:449`). After a restore,
`PENDING_NOTICES` is empty and the transcript still shows `ID: notice-N` → every reply
is `unknown-notice-id` with no recovery path.

**7. Silent UI loss when the global senders are not registered.**
`bus::emit_event` (`bus.rs:106-111`) and `record_steering_exchange`
(`bus.rs:149-151`) no-op without `EVENT_SENDER` / `STEERING_HISTORY`, and only
`session.rs:1818-1824` registers them. A second/parallel session (bridge, tests, or a
restart path that swaps senders) makes the arbitrator's answer vanish with no error
while the worker still gets `Ok(ToolResult)` (`harness/mod.rs:845-848`).

**8. Arbitrator progress lines are rendered but never journaled.**
`drain.rs:37-39` turns `SteerArbEvent::Delta` into `Event::SteerResponse` for display
only, with **no** `ui_transcript.append`, whereas the `SynthesizedAnswer`
(`drain.rs:109-115`) and `Finished`/`d.response` (`drain.rs:184-189`) branches do
append. So the "Forwarded notice notice-7 to coder-t-001 — awaiting specialist reply"
line (`steer.rs:186-189`) never reaches `UiTranscript`
(`src/ui/transcript.rs:29`, `extract_steering_history` `:90-140`) or the journal twin
(`src/session/format/ui.rs:201`) — after a redraw/restore the user cannot even see the
id the round trip is keyed on.

**9. A model-created history entry can never be correlated.**
`src/ui/bridge/sink.rs:226` records history with `recorded_response(decision, None,
&[])`, producing the id-less `"Forwarded notice to worker (awaiting specialist
reply)"` (`arbiter.rs:675`). The reply-time correlation step 1 (`bus.rs:160-170`) then
cannot match by id and falls back to whole-string inquiry equality (`bus.rs:174-184`);
because the notice body may have been rewritten (`st.message`, `steer.rs:184`) or
case-folded, equality fails and a *duplicate* pending entry is appended
(`bus.rs:186-189`). Related: the pending vocabulary
(`steer.rs:478-488`) is prose-coupled (see `notice.rs:1084-1089`).

**10. The arbitrator turn runs inside the worker's own tool round.**
`notice.rs:1188-1194` is explicit that this must not inherit the `Specialist` class.
Budget: 60 s timeout (`:1195-1205`) plus up to `INTERNAL_QUEUE_WAIT_SECS = 20`
(`pool.rs:73`) reserve wait (`acquire_reserve` is fail-open, `pool.rs:723-748`, so no
deadlock). Failure is **not** silent — `harness/mod.rs:851-865` and
`notice.rs:1244-1257` still emit a `SynthesizeResponse` line — but a stalled backend
looks exactly like a low-quality arbitrator answer, which can be misdiagnosed as "the
reply didn't work".

**11. TTL/eviction pruning runs on every post and every drain.**
`prune_notice_storage` (`notice.rs:323-427`) with `NOTICE_TTL_MS = 600_000` (`:126`),
`INBOX_IDLE_TTL_MS` (`:132`), `INBOX_CAPACITY = 64` drop-oldest (`:142`,
`:484-512`), `MAX_INBOX_KEYS = 256` eviction (`:400-423`). A worker that finishes a
long turn more than 10 minutes after the notice was posted gets `unknown-notice-id`
(`:922-938`) for an id it was shown verbatim (`:1042-1051`). Also: `remove_if`
(`:951-959`) converts a prune race into `unknown-notice-id` (`:964-969`).

**12. Manager-role replies can essentially never validate.**
`harness/mod.rs:588-590` labels the caller `"Manager"`; `notice_addresses_worker`
(`:856-862`) will only accept that against `"*"`/`"worker"` broadcast notices
(`:533`). Any Manager-side `reply_to_arbitrator` against a per-worker notice returns
`worker-mismatch`.

---

## 6. Code smells / recent-looking inconsistencies (not fixed, per instructions)

1. **Schema vs. runtime leniency mismatch** — `types/tools.rs:385-408` marks
   `notice_id` required, while `harness/mod.rs:745-748` deliberately substitutes `""`
   (documented at `:741-744`). Providers that enforce `required` will reject the call
   before the handler's `missing-notice-id` class is ever produced.
2. **Schema advertises an id class that does not exist** —
   `types/tools.rs:397` says "e.g. `'notice-1'` or `'steer-1'`"; the store only ever
   mints `notice-{n}` (`notice.rs:50-53`). `'steer-1'` appears nowhere in the notice
   store.
3. **Prompt prose vs. advertised schema** — the specialist system prompt's tool list is
   built from `ctx.blueprint.allowed_tools` or a hard-coded fallback string
   (`execution.rs:215-223`) that omits `reply_to_arbitrator`. The *schema* carries the
   tool (fix_loop `AlwaysAdvertised`), so a specialist can be told "Tools available:
   …" without the one tool the notice text (`notice.rs:1044-1046`) orders it to call.
4. **MCP consult order differs between paths** — specialist: `mcp_route` before built-ins
   (`harness/mod.rs:622`); Manager: after built-ins (`harness/mod.rs:600-607`). Safety
   comes from `resolves_to_builtin_handler` inside the gate (`harness/mod.rs:108`,
   `route()` near `:263`), not from the ordering.
5. **Arbitrator evaluation client is re-derived, not inherited** —
   `harness/mod.rs:783-787` builds a fresh `ChatClient` from
   `config::get_active()/load()/default()`, ignoring the session's steering client and
   any per-role/steering model or effort settings the arbitration path uses
   (`steer.rs:104-115` / `arbiter.rs:446-604` receive `client` from `session.rs`), and
   `evaluate_worker_reply` sends `model: String::new()` (`notice.rs:1168`).
6. **Streamed arbitrator deltas are dropped in the reply path** —
   `harness/mod.rs:789-797` passes `|_delta| {}` while `notice.rs:1239` / `:1249`
   deliberately push the synthesized answer through `on_delta`. The user therefore sees
   nothing during up to 60 s except the one-shot status at `harness/mod.rs:779-781`.
7. **Same-id follow-up reuse** — `harness/mod.rs:805` + `notice.rs:466-468` put a
   consumed id back into `PENDING_NOTICES` while `REPLIED_NOTICES` still holds the prior
   reply (`notice.rs:963-997`), and `REPLIED_NOTICES` is never pruned per id.
8. **Duplicate-post structure is structural, not accidental** — `steer.rs:174-198` and
   `drain.rs:225-248` implement the same action twice with *different* message text
   (`st.message` vs. `user_msg`) and *different* target defaults (`agent_name`/
   `tool_call_id` vs. `"worker"`). `sink.rs:389-412` is a third copy with the `"worker"`
   default.
9. **Test fixtures hide ranked break #1** — `src/ui/bridge/tests.rs:644-669` asserts
   `addressed_worker_keys` behaviour with registry-shaped keys, but the end-to-end
   fixtures feed non-registry ids (e.g. `tests/test_ui_session.rs:763` uses
   `tool_call_id: "call_1"` with `agent_name: "coder"`), and
   `src/orchestrator/preemption.rs:1350-1363` builds subtasks with `tool_call_id:
   "t-96{label}"` plus `agent_name`. So the "no `agent_name`, bogus `tool_call_id`"
   combination that breaks targeting in production is never exercised.
10. **Dead union removed in one place, still referenced elsewhere** —
    `session.rs:2108-2129` explains why `subagents.iter().any(|s| s.is_active)` is not a
    substitute for `has_active_workers()`; the equivalent union still appears at
    `sink.rs:183` (diagnostic-only there) and in assertions
    (`bridge/tests.rs:697, 710`).
11. **`SteerSubtaskDecision.message` vs. `prompt`** — `steer.rs:29-46` has both
    `message` and `prompt`; `notice_target`/post use `message` (`steer.rs:184`) while
    delegation uses `prompt`. A decision that fills the other field silently degrades to
    forwarding the raw user message.
12. **`reclaim_worker_inbox` keeps non-empty inboxes forever** (`notice.rs:745-768`) —
    intentional ("a replacement worker stays steerable"), but combined with break #1 it
    is how orphan keys accumulate toward `MAX_INBOX_KEYS = 256` eviction (`:400-423`).

---

## 7. Fastest confirmation path for the implementer (read-only)

1. Reproduce with one live worker; check the log for
   `specialist reply from worker '<role>' rejected (<kind>)`
   (`notice.rs:907-913`) — the `worker_tag` and `addressed_to` in the message
   (`notice.rs:838-845`) immediately distinguishes break #1/#3/#4 from #11.
2. Log/inspect the post target at `arbiter.rs:278-294` → `notice.rs:472` and compare
   with the drained key at `notice.rs:696-700` (the runner passes
   `execution.rs:493`/`:1113`'s effective key). A target that is neither the effective
   key, a natural key, a role-family prefix, nor `*`/`worker` **is** break #1.
3. Count `post_notice_to_worker_tracked` invocations per steer: two ids for one user
   instruction ⇒ break #2.
4. `evaluate_worker_reply` fallback detection: the emitted line starts with
   `Specialist [<role>] explains:` (`notice.rs:1247`, `harness/mod.rs:830`) ⇒ the arbitrator
   turn itself failed (break #10), not the reply contract.
