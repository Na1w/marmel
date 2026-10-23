# Diagnosis: `reply_to_arbitrator` round trip (marmennill / marmel)

**Mode:** strictly read-only. No tracked file was modified, no branch/commit created.
**Baseline:** `de3e99c` (`HEAD -> main`, "Fix/subagent panel refresh (#25)"), tracked tree clean before and after.
**Final tree check:** `git status --porcelain --untracked-files=no` → empty (verified twice; the single throwaway probe file `tests/probe_roundtrip_tmp.rs` was deleted).

---

## 0. How this was verified

Static read of every hop in both directions (citations below are from the working tree at `de3e99c`), plus narrow **existing** test runs (all green, offline) and two throwaway probe binaries run against the real public crate API:

| Run | Result |
| --- | --- |
| `cargo test --offline --lib -- --test-threads=1 orchestrator::notice_tests` | 38 passed, 1150 filtered |
| `cargo test --offline --lib -- --test-threads=1 harness::dispatch_integrity_tests` | 13 passed (4 reply-integrity) |
| `cargo test --offline --lib -- --test-threads=1 ui::bridge::tests` | 40 passed, 1148 filtered |
| `cargo test --offline --test test_steer_history -- --test-threads=1` | 9 passed |
| `cargo test --offline --test test_midturn_notice -- --test-threads=1` | 3 passed |

Probe output (deleted afterwards; the probe only used `pub` API, so no tracked file was touched):

```
[P1] target='t-001' drained by coder-t-001 -> 1 (id notice-4)
[P2] target='coder'  drained by coder-t-001 -> 1
[P3] target='call_1': drained by coder-t-001 -> 0, by coder -> 0,
     by the orphan key itself -> 1; still pending=true        orphan inbox key = 'call_1'
[P4] reply naming the schema's example id 'steer-1' -> unknown-notice-id
[P4] reply naming no id                                     -> missing-notice-id
[P4] reply naming the real id by a bare role name            -> ok=false
[P5] follow-up id reused=notice-1 target='coder'
[P5] follow-up delivery: A(coder-t-101) -> 1, B(coder-t-102) -> 0   (total 1)
[P5] notice_addresses_worker(follow-up, coder-t-102) = true
[P5] the OTHER same-role worker answering A's dialogue id -> accepted=true
[P6] ids issued by the two posting hosts: notice-1 / notice-2
     worker drains 2 notices:
        notice-1 -> "How is the coder doing? (arbitrator rewrite)"
        notice-2 -> "how is my coder doing?? (raw user text)"
     arbitrator received reply bound to inquiry: "How is the coder doing? (arbitrator rewrite)"
     the second notice is still pending after the worker replied to the first: true
     replying twice to the same id -> already-replied
     answering the duplicate copy -> accepted=true (second arbitrator turn)
[P7] real worker answering an orphan-targeted id -> worker-mismatch
     "notice 'notice-3' is pending but addressed to 'call_1', not to the replying worker 'coder-t-001'"
```

---

## 1. Confirmed root causes

### RC-1 — One steer is posted as **two** notices (plus a third delivery as a Manager turn) — PRIMARY

Posting hosts (both run for the same user steer):

- `src/ui/bridge/steer.rs:176-197` — the spawned-arbitrator host posts **once per `ForwardNotice` subtask**: `:183` `action::notice_target(st)`, `:184` `msg_to_send = st.message.as_deref().unwrap_or(&msg)` (the arbitrator's rewrite), `:185` `arbiter::post_notice_observable(&target, msg_to_send)`, `:196` records `(notice_id, target)`.
- `src/ui/bridge/drain.rs:225-247` — the drain host consumes `SteerArbEvent::Finished` (sent at `steer.rs:212`) and posts a **second** notice for the *same* decision: target recomputed from the same decision (`:227-232`, default `"worker"`), message = **raw user text** (`:233` `post_notice_observable(&target, &user_msg)`).
- `src/ui/bridge/sink.rs:389-410` — a third host with the identical shape (`:396` default `"worker"`, `:397` post with the raw user message).
- Both hosts run for one steer: the arbitration is spawned at `src/ui/session.rs:2131` (guarded by `has_active_workers()` at `:2130`), `:2836`, `:3415` (unguarded), and the drain host is invoked at `session.rs:1937, 2059, 2233, 2705, 2808, 3301, 3387, 3710`.

Mechanism of the round-trip failure (probe P6):

1. Two notices with **different ids and different text** land in the same inbox (`notice_target` is the same function in both hosts, so the key matches; and when `decision` is `None` the drain/sink default `"worker"` is a broadcast target — `BROADCAST_TARGETS = ["*","worker"]`, `src/orchestrator/notice.rs:533` — so the raw user text is delivered to *every* active worker).
2. The worker is rendered two ID blocks (`render_notice_for_worker`, `src/orchestrator/notice.rs:1042-1051`: "Reply to this notice id only — a reply naming another id, or no id, is rejected."), so it answers one of them arbitrarily. The reply is bound to whichever inquiry it answered (`record_worker_reply_for_notice` returns the notice, `notice.rs:898-1000`).
3. The unanswered copy stays `PENDING` until the 10-min TTL (`NOTICE_TTL_MS`, `notice.rs:126`) and its steering-history entry stays pending (`PENDING_ENTRY_MARKERS` = "awaiting specialist reply" | "ForwardToWorker" | "follow-up", `src/orchestrator/steer.rs:478`; correlation in `src/orchestrator/bus.rs:144-191`) → the panel shows "awaiting specialist reply" forever.
4. If the worker answers **both** ids, `handle_reply_to_arbitrator_async` runs `evaluate_worker_reply` twice (`src/harness/mod.rs:789`, `src/orchestrator/notice.rs:1151-1258`) → the arbitrator mediates **two answers back to the user** for one steer, and the second `reply_to_arbitrator` for the first id is rejected `already-replied` (probe P6).
5. In addition, both hosts push the raw instruction onto `steer_queue` (`drain.rs:247`, `sink.rs:408`) → the same instruction is *also* delivered to the Manager as a user turn (`src/ui/session.rs:2145-2148`), a third delivery path that bypasses the notice contract entirely.

### RC-2 — Notice targeting: the arbitrator's `tool_call_id` is fed into the **task-id** slot, and the fallback posts to a token no worker ever drains (conditional but reachable, and *the prompt guarantees reachability*)

- The arbitrator's worker table is the registry render: `src/ui/bridge/arbiter.rs:382-388` → `src/orchestrator/workers.rs:1046 get_active_subtasks_str()`; `workers.rs:1079` `let task_id_str = info.task_id.as_deref().unwrap_or(id);` and `workers.rs:1082-1083` prints `"- Tool Call ID: {task_id_str}\n  Subagent Tag: {id}..."`. So the token the model is told to echo as "Tool Call ID" is the **task id** (`t-001`); the effective registry key is shown only as "Subagent Tag".
- `prompts/steer_arbitrator.md:28` and `:51` demand that `tool_call_id` be filled for `ForwardNotice`; `:54` scopes `agent_name` to `DelegateTask` only — **the prompt never tells the arbitrator to fill `agent_name` for a `ForwardNotice`.**
- `src/ui/bridge/action.rs:141-151`:
  `notice_target` → `addressed_worker_keys(subtask.agent_name, Some(&subtask.tool_call_id))` — i.e. the arbitrator's `tool_call_id` is placed in the **task** half — and on non-unique match falls back to `agent_name.unwrap_or_else(|| tool_call_id.clone())`.
- Routing rules (`src/orchestrator/notice.rs:614-651`, `WorkerRoutingIdentity::routes`) accept: `*`/`worker`, exact effective key, exact natural key, exact agent name / role-family prefix `{target}-`, and **exact task id**. Drain selects inboxes by those rules (`notice.rs:690-712`, `identity.routes(e.key())`), and workers drain with their effective key (`ActiveWorkerGuard(pub String)`, `src/orchestrator/workers.rs:185`; `src/agents/runner/execution.rs:493` turn start, `:1113` mid-turn → `src/agents/runner/fix_loop.rs:838-852`).

Therefore (probes P1/P2/P3):

- a **task token** (`t-001`) *does* route → candidate 1's strong form ("always misses") is **refuted**;
- a **bare role name** (`coder`) *does* route (role-family) → this is what keeps the existing tests green;
- a **non-registry token** with `agent_name` absent (`call_1`, the OpenAI-style id models habitually emit) → `notice_target` returns the raw token, the notice is filed under inbox key `call_1` (`notice.rs:472` `clean_target`), **no worker identity ever routes it** (P3: 0 delivered, still pending), and even if the worker somehow learned the id the store rejects the reply as `worker-mismatch` (P7). The worker never sees an `ID:` block, so any reply it produces is `unknown-notice-id` / `missing-notice-id`. Those orphan inboxes churn the `MAX_INBOX_KEYS = 256` table (`notice.rs:147`, eviction at `notice.rs:336-368`).
- Secondary defect in the same function: when `addressed_worker_keys` matches **more than one** entry, the `_` arm silently degrades to a role/task broadcast (`action.rs:146-150`), so the notice goes to whichever same-role worker drains first; the `Cancel` arm of the drain host joins keys instead of addressing each (`src/ui/bridge/drain.rs:142-152`).

### RC-3 — The arbitrator follow-up hop is addressed to the **bare role name** with a **reused notice id**

`src/harness/mod.rs:800-828`: on `AskFollowUp`, `:805` posts
`post_notice_to_worker(caller, &follow_up, Some(notice_id))` where `caller` is the **bare role name** (`ToolCaller::role_name()`, `src/harness/common.rs:107-113`; identity folded at `harness/mod.rs:726-728`).

- With two live workers of the same role (probe P5) the follow-up is delivered to exactly **one** of them (total 1) and *which one* is a map-iteration accident; `notice_addresses_worker`'s bidirectional tolerance (`notice.rs:856-862`) then lets the **other** same-role worker answer the first worker's dialogue id (accepted=true) → the arbitrator's answer is attributed to the wrong specialist conversation.
- The **reused id itself is not a defect**: re-inserting `PENDING_NOTICES` makes the id answerable again and this behaviour is explicitly specified and tested (`notice_tests.rs:1562 test_follow_up_repost_of_same_notice_id_is_answerable_again`). But it means the follow-up hop cannot be distinguished from the first round by id, so nothing can bind a follow-up answer to the worker that received it.
- "`REPLIED_NOTICES` is never pruned" is **true but benign**: `prune_notice_storage` (`notice.rs:323-427`) never touches it; it is ring-capped at `REPLIED_NOTICES_CAP = 64` (`notice.rs:151`, eviction at `notice.rs:970-997`). It is not a delivery failure.

### RC-4 — Reply-side identity is a bare role name while notices are keyed by effective registry key (latent, currently papered over)

`src/harness/mod.rs:698-700` dispatches with `caller_str` = role name; `:757-766` records the reply under that. It works only because of the bidirectional `notice_addresses_worker` tolerance. The runner *knows* the effective key (`_active_guard.0`, `src/agents/runner/execution.rs:302-307`) but never passes it into dispatch, so per-worker attribution (RC-3) and `worker-mismatch` precision cannot be enforced.

### RC-5 — Tool schema advertises an id class that does not exist, and is stricter in shape than the handler is in behaviour

`src/types/tools.rs:385-408`: `required: ["notice_id","message"]` (`:402`) and the description "(e.g. `'notice-1'` or `'steer-1'`)" (`:397`). The store only ever mints `notice-{n}` (`next_notice_id`, `notice.rs:50-53`); `steer-1` is a **non-existent id class** — probe P4 shows a reply naming it is rejected `unknown-notice-id`. Meanwhile `harness/mod.rs:745-748` maps an *absent* `notice_id` to `""` on purpose (deliberate: it yields the `missing-notice-id` class instead of a schema error, documented at `harness/mod.rs:743-744`). The mismatch (schema = required; handler = lenient) is real; the *leniency itself is intentional*, the **`steer-1` example is not** — it invites hallucinated ids.

### RC-6 — A `ForwardNotice` subtask that omits `tool_call_id` makes the whole decision unparseable, silently dropping the notice (new finding, not in the research ranking)

`src/orchestrator/steer.rs:31-46`: `pub struct SteerSubtaskDecision { pub tool_call_id: String, ... }` — **no `#[serde(default)]`** (all sibling fields have one). `:281` `serde_json::from_str::<SteerDecision>(json_text)` fails wholesale, falling through to the `RespondDirectly` fallback at `steer.rs:293-303` — no notice is posted at all, no status, no error surfaced to the user. Because the prompt (`prompts/steer_arbitrator.md:51`) insists on `tool_call_id`, the model sometimes omits it for a subtask it cannot name, and the steer then silently degrades.

---

## 2. Candidate verdicts

| # | Candidate | Verdict |
| --- | --- | --- |
| 1 | Notice targeting (`action.rs:91-151` + `workers.rs:1079`) orphans the inbox | **Partly real — mechanism must be restated.** It is *not* "the id is used as a task key so it always misses": exact task-id equality is a first-class routing rule (`notice.rs:648-651`), so a compliant arbitrator that echoes the rendered task id **does** route (P1). The real failure is the `agent_name`-absent fallback `unwrap_or_else(|| tool_call_id.clone())` (`action.rs:146-150`) combined with the prompt never asking for `agent_name` on `ForwardNotice` (`prompts/steer_arbitrator.md:28,51,54`) and models emitting OpenAI-style ids: then the notice is filed under a key nobody drains (P3) and even a correct reply is `worker-mismatch` (P7). `workers.rs:1079` is the *cause of the confusion* (the label says "Tool Call ID" but prints the task id), not a mismatch with the drain side. |
| 2 | Duplicate notice per steer (`steer.rs:185` + `drain.rs:225-248`) | **Real, unconditional, and the primary user-visible defect.** Confirmed by code (both hosts fire for one steer) and by probe P6 (two ids, two texts, one answer, second entry pending until TTL, or double arbitration). `sink.rs:389-410` is a third host of the same shape; the `"worker"` default is a broadcast to all workers. |
| 3 | Follow-up hop (`harness/mod.rs:805-824`, reused id, `REPLIED_NOTICES` unpruned) | **Half real.** Bare-role targeting is real and mis-attributes the follow-up with ≥2 same-role workers (P5). The reused id is *intended* and already covered by `notice_tests.rs:1562` — not a root cause. `REPLIED_NOTICES` never being TTL-pruned is true but bounded by `REPLIED_NOTICES_CAP = 64` — cosmetic, not a delivery failure. |
| 4 | Bare-role reply identity (`harness/mod.rs:726-728`) | **Real, latent** (RC-4) — the enabling condition for RC-3's ambiguity. |
| 5 | Schema leniency mismatch | **Real**, but the handler leniency is deliberate; the *harmful* part is the `'steer-1'` example in `types/tools.rs:397`. |
| 6 | (new) Missing `tool_call_id` → whole-decision parse failure | **Real** (`steer.rs:32` + `:281`, fallback `:293-303`). |

---

## 3. Minimal fix design per confirmed cause

Invariants that MUST be preserved by every change below (all currently encoded in `src/orchestrator/notice.rs`):

- **Strict notice-id targeting / reply contract:** a reply names exactly one pending `notice_id`; the four rejection classes `missing-notice-id` / `unknown-notice-id` / `already-replied` / `worker-mismatch` (`notice.rs:779-812`) stay distinguishable and are surfaced verbatim as `ToolError::BadArguments` (`harness/mod.rs:767-776`). No synthesized-notice fallback.
- **10-min TTL** (`NOTICE_TTL_MS`, `notice.rs:126`) and `INBOX_IDLE_TTL_MS` (`:132`).
- **64-per-inbox drop-oldest** (`INBOX_CAPACITY`, `notice.rs:142`) with evicted ids returned by `post_notice_to_worker_tracked` (`:459`), removed from `PENDING_NOTICES`, counted in `notice_lifecycle_stats`, and never silent (`steer.rs:188-195`, `drain.rs:239-246`, `sink.rs:402-409`).
- **256-inbox retention** (`MAX_INBOX_KEYS`, `notice.rs:147`, eviction `:336-368`).
- **Validator/Planner role rules:** role-family matching stays anchored at `WORKER_KEY_SEGMENT` boundaries (`notice.rs:530`, `:640-645`) so `validator-coder` still answers a `coder` address but `coder-t-0` never takes a `coder-t-01` notice.
- Whole-token equality only: no `contains`, no unanchored prefix match (`notice.rs:614-651`, pinned by `notice_tests.rs:212 test_unroutable_notice_is_not_delivered_or_dropped` and the disambiguated-key tests).
- Observable steering: the `Event::Status` "Notice … forwarded to …" deltas and the history text carrying the notice id (`arbiter.rs:668-675`).

**Fix 1 (RC-1) — make one steer produce one notice.**
Pick a single owner. Minimal: delete the post in `src/ui/bridge/drain.rs:233` and `src/ui/bridge/sink.rs:397`; keep everything else in those arms (status text, capacity-drop surfacing, `steer_queue.push`). To keep the status text truthful, carry the already-posted ids through the event: extend `SteerArbEvent::Finished` (`src/ui/bridge/steer.rs:212-215`) with `forwarded: Vec<(String,String)>` (already built at `steer.rs:196`) and have drain/sink render from it instead of posting. Semantics to change: `Finished` becomes "here is what was already posted", not "post it again".
Additionally decide explicitly whether a `ForwardToWorker` steer should *also* be queued to the Manager (`drain.rs:247`, `sink.rs:408`). Today it is, which is the third delivery; if the arbitrator is the mediator, drop that push for `ForwardToWorker` (keep it for `AbortImmediately`/`Sleep`/fallback arms).

**Fix 2 (RC-2) — target resolution must never emit an unroutable token, and must be per-subtask.**
In `src/ui/bridge/action.rs:141-151`: (a) resolve with the *pair* the way `addressed_worker_keys` is designed, but when it yields nothing, fall back through the registry only to an address the router accepts — agent name if present, else the task half if `plan_parse::is_task_id_token`, else **do not post**: return `None` and have `steer.rs:183-185` emit a visible "target not found among active workers: `<token>`" delta rather than filing an orphan inbox. (b) On multi-match, post to **each** effective key (or to the broadcast target deliberately), never to `agent_name.unwrap_or(tool_call_id)` which silently re-degrades to role broadcast. (c) In `drain.rs:142-152` (`Cancel`), address each matched key instead of joining them.
In `src/orchestrator/workers.rs:1079-1083`: relabel the render so the model is given the token that actually addresses the worker — print `Tool Call ID: {id}` (the effective registry key / "Subagent Tag") or add an explicit `Address this worker with: {id}` line; keeping `task_id` as the primary echo token is what makes the compliant model produce an address that only works by accident.

**Fix 3 (RC-3 + RC-4) — pin the follow-up hop to the notice's own target and to the replying worker.**
`src/harness/mod.rs:805`: replace the bare `caller` with the **target the notice was actually delivered under**, i.e. `notice.target_worker` (the store already returned that notice at `harness/mod.rs:758`, and `SteerNotice.target_worker` is set at `notice.rs:472-476`). That is a one-line, plumbing-free fix that keeps the reused-id dialogue semantics (already tested) and restores exact-token targeting for round 2.
For attribution: introduce a worker-key task-local next to the existing `CURRENT_WORKER_TOKEN.scope(...)` pattern (`src/orchestrator/bus.rs:32-34`, `src/agents/runner/execution.rs:193-200`, key available at `:302`) and pass that effective key as the reply identity into `handle_reply_to_arbitrator_async` instead of `caller_str` (`harness/mod.rs:698-700`, `reply_worker_identity` `:726-728`). Keep `ToolCaller::role_name()` as the fallback when no key is in scope (Manager, tests). This makes `worker-mismatch` precise and makes `notice_addresses_worker`'s bidirectional tolerance a compatibility path rather than the only thing preventing cross-talk.

**Fix 4 (RC-5) — schema.**
`src/types/tools.rs:397`: delete `'steer-1'`; state that the id is the literal `ID: <notice-id>` from the delivered notice block and that no other value is accepted. Keep `required: ["notice_id","message"]` (`:402`) — it is a *contract statement*, and the handler's `unwrap_or("")` (`harness/mod.rs:745-748`) must stay so a missing id still returns the machine-readable `missing-notice-id` class rather than a schema-shape error.

**Fix 5 (RC-6) — prompt tolerance so one missing field cannot kill the whole steer.**
`src/orchestrator/steer.rs:32`: add `#[serde(default)]` to `tool_call_id` (or deserialize into a tolerant shape and reject per-subtask), and keep per-subtask validation so a malformed subtask is dropped/logged instead of collapsing the decision into `RespondDirectly` (`steer.rs:281-303`).

---

## 4. Existing tests that mask the bug, and what they should assert

| Test | Why it masks | What it should assert instead |
| --- | --- | --- |
| `tests/test_ui_session.rs:736-831` (`test_steering_conversation_history_accumulates_worker_reply_to_arbitrator`), mock arbitrator fixture at `:763` | Its `ForwardNotice` subtask pairs a **non-registry** `tool_call_id:"call_1"` with `agent_name:"coder"`, so `notice_target` falls back to `coder`, which routes by role family (probe P2). It registers **no** worker in the registry, never drains an inbox, and calls `handle_reply_to_arbitrator_async("coder", …)` directly — so it can never observe RC-1, RC-2 or RC-3. | Register a worker (real `ActiveWorkerGuard` key) or assert on `get_pending_notice`/`drain_worker_notices`; assert **exactly one** notice per steer (`notice_lifecycle_stats()` / `WORKER_INBOXES` size), assert the drained copy carries the arbitrator's rewritten inquiry, and assert the reply path goes through the same effective key the worker drained with. Add a sibling case with `agent_name: null` and `tool_call_id: "call_1"` asserting the steer produces a visible targeting failure instead of an orphan inbox. |
| `src/ui/bridge/tests.rs:624-686` (`test_bridge_match_rule_resolves_whole_effective_tokens_with_and_semantics`), `notice_target` assert at `:672-680` | Exercises only the happy path where **both** `agent_name` and a task-shaped `tool_call_id` are present (and the task half is the effective key, `:644`); the `_` fallback arm (`action.rs:146-150`) is never exercised with `agent_name: None`. | Add assertions for `agent_name: None` with (a) a task token → effective key, (b) a non-registry token → the target must be rejected/visible, never a raw token; and for a multi-match decision → per-key posting, not a role broadcast. |
| `src/ui/bridge/tests.rs:400-513` and `:515-621` (Cancel subtask tests) | Feed decisions where both `agent_name` and task half are correct (`:432`, `:553`), so the joined-keys path in `drain.rs:142-152` and the `Cancel` fallback are not exercised against a missing `agent_name`. | Add a `agent_name: None` Cancel case and assert exactly one registry entry is cancelled and the notice/cancel never touches the sibling. |
| `src/ui/bridge/tests.rs:2177-2280` (`test_bridge_notice_capacity_drop_is_observable_and_inbox_stays_bounded`) | Posts through the drain host with `agent_name = target`, so it proves capacity policy (`:2182`, `:2234`, `:2243-2247`) but never counts notices-per-steer nor validates targeting; **no test in the tree runs both posting hosts for one steer.** | Keep the policy assertions, and add a `ForwardToWorker` steer through `spawn_steer_arbitration` + the drain host in the same test, asserting the number of notices issued for that steer is `1` (regression guard for RC-1). |
| `src/ui/bridge/tests.rs:1094-1116` (`recorded_response` wording) | Asserts history text ("Forwarded notice notice-1 to coder (awaiting specialist reply)") which is id-carrying but decoupled from whether the notice was ever deliverable or duplicated. | Assert the recorded ids equal the ids actually pending (correlation with `get_pending_notice`), and that no extra id was issued by the drain host. |
| `src/harness/dispatch_integrity_tests.rs:78,125,176,226` | Post notices to registry-shaped targets directly, bypassing `notice_target`; the follow-up assertion uses a bare role target, so RC-3's ambiguity is invisible with one worker per role. | Post through `action::notice_target`, and add a two-same-role-worker case asserting the follow-up is delivered to the replying worker only and that another same-role worker is rejected with `worker-mismatch`. |
| `tests/test_steer_history.rs:68` | Re-implements the pending text and correlates by it; with the duplicate notice (RC-1) two entries look alike and the test still passes. | Assert one pending entry per steer and that it is resolved by the first reply (no entry left pending after the worker answers). |

Store-level tests that are correct and must keep passing unchanged: `src/orchestrator/notice_tests.rs` (38 green), notably `:212 test_unroutable_notice_is_not_delivered_or_dropped`, the disambiguated-key/natural-key tests (`~:5800-6100` region), and `:1562 test_follow_up_repost_of_same_notice_id_is_answerable_again`.

---

## 5. Where the fix belongs

**All three layers, with the primary fix in routing/targeting + the bridge posting ownership:**

1. **Routing / targeting ownership (primary):** one notice per steer (`drain.rs:233`, `sink.rs:397` removal), non-emitting of unroutable tokens and per-key addressing (`action.rs:141-151`, `drain.rs:142-152`), follow-up target = `notice.target_worker` (`harness/mod.rs:805`), reply identity = effective worker key (task-local from `execution.rs:302`).
2. **Arbitrator prompt (secondary but required):** `prompts/steer_arbitrator.md:28,51,54` must instruct `agent_name` (or an unambiguous `Subagent Tag`/effective key) for `ForwardNotice`, and `workers.rs:1079-1083` must render the token the router actually accepts. Without this, a strictly correct model still produces accidentally-routable ids.
3. **Tool schema (small):** drop the fictitious `'steer-1'` example (`types/tools.rs:397`); keep `required: ["notice_id","message"]` and keep the handler's deliberate `missing-notice-id` leniency (`harness/mod.rs:743-748`). Plus the serde tolerance fix at `steer.rs:32` so one omitted field cannot silently drop the whole notice.

MISSION COMPLETE
