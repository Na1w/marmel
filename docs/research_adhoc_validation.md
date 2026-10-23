# Ad-hoc arbitrator subtasks and the `<task_id>-validation.md` verdict gap

READ-ONLY research note. No source file was modified.

**Reported bug:** "ad-hoc tasks created via the arbitrator are revoked every time because
`<task_id>-validation.md` is missing" → `MISSION COMPLETE` revoked → deliverable reported `FAILED`.

**Verdict (one line):** the bug is real and structural. An ad-hoc steering subtask gets a *valid*
task id (`steer-task-1`, or the arbitrator's `tool_call_id`), which makes the read side look up
`.marmel/prompts/steer-task-1-validation.md`, but **no code path on the ad-hoc path ever authors
that brief nor a `-validation-skip.md` record** — so the deliberately fail-closed verdict guard
fires, revokes completion and marks the task `FAILED`.

**Empirical proof in this workspace:**

- `.marmel.old.4/marmel.log:6174` —
  `coder-steer-task-1: Validation was not performed — /home/fredrik/marmel/wip/.marmel/prompts/steer-task-1-validation.md does not exist, so no validation verdict is recorded for this task. The deliverable is reported as not validated.`
- `.marmel.old.8/marmel.log:5172` — same message for `researcher-steer-task-1`.
- Current `.marmel/prompts/` holds only plan-authored ids (`t-001`..`t-008`, each with
  `-validation.md` or `-validation-skip.md`); **no `steer-task-*` record of either kind exists.**

---

## 1. Complete ad-hoc task lifecycle (file:line)

### 1.1 Arbitrator emits the delegation decision (schema side)

| Step | Location | Detail |
|---|---|---|
| Action vocabulary | `src/orchestrator/steer.rs:360-376` | `SteerSubtaskAction { ForwardNotice, Cancel, DelegateTask, Sleep, Unknown }`; `DelegateTask` at `:371-372`. |
| Accepted spellings | `src/orchestrator/steer.rs:430-440` | `"delegatetask" \| "delegate" \| "delegatenewtask" \| "newtask" \| "newsubtask" \| "createtask" \| "starttask" \| "spawntask"`. |
| Normalizer (single entry point) | `src/orchestrator/steer.rs:450-467` | Unknown spelling → `Unknown` + `WARN`; **no action branch is taken**. |
| Bridge re-export / routing | `src/ui/bridge/action.rs:27-35` (`ACTION_DELEGATE_TASK` at `:34`), `route()` `:39-42`, `routed/any/first` `:45-70` | The bridge re-uses the orchestrator's type; **there is no extra schema field** — no place to carry a validate / no-validate flag. |
| Arbitrator prompt grammar | `prompts/steer_arbitrator.md:31-37`, JSON shape `:44-56` | `DelegateTask` subtask entry = `tool_call_id` ("a unique identifier like `'steer-task-1'`"), `action`, `agent_name`, `prompt`. **The prompt contains no validation vocabulary at all.** |

### 1.2 Where the ad-hoc task id comes from

`src/orchestrator/steer.rs:652-723` — `extract_tasks_to_delegate()`:

- `:668-672` agent = `Agent::from_str(agent_name)` else **defaults to `Agent::Coder`**.
- `:673` `let cleaned = crate::task_id::normalize_task_id_ref(&s.tool_call_id).to_string();`
  → **the ad-hoc task id is the arbitrator's own `tool_call_id`**, verbatim-normalized.
- `:676-677` fallback when empty: `format!("steer-task-{idx}")`.
- `:688-721` top-level fallback branch (no explicit `DelegateTask` subtask but decision is
  `DelegateTask`): id from `normalize_task_id(&s.tool_call_id)` at `:712`, else
  `"steer-task-1"` at `:713`.
- `:716-720` prompt falls back to the raw user message when `prompt` is absent/blank.

**Nothing in this function attaches a validation directive, a plan line, or any `(no-validate)`
notion.** The returned tuple is `(Agent, String task_id, String prompt)` — three fields, no verdict
intent.

### 1.3 Execution of the ad-hoc subtask

`src/orchestrator/steer.rs:726-746` — `execute_steer_subtask()`:

- `:733` `let plan = Plan::default();` → a *default* plan handle (points at `.marmel`), **not** a
  plan containing this task.
- `:734` fresh `OrchestratorManager`.
- `:736-744` `DelegationRequest { agent_name, prompt, snippets: vec![], task_id, image_urls: None,
  audio_urls: None, recursion_granted: false }`.
- `:745` `manager.delegate(req).await`.

**It never calls `create_plan`, never appends a line to `.marmel/execution_plan.md`, and never
writes `<task_id>-validation.md` or `<task_id>-validation-skip.md`.**

### 1.4 Bridge call sites

| Location | Detail |
|---|---|
| `src/ui/bridge/arbiter.rs:607-614` | `delegation_tasks()` — thin wrapper over `extract_tasks_to_delegate`. |
| `src/ui/bridge/arbiter.rs:617-642` | `run_delegated_subtask()` → `execute_steer_subtask(client, stats, agent, Some(task_id.to_string()), prompt)` at `:624-630`; on error builds `Deliverable { marker: MissionMarker::Failed { .. }, task_id: Some(task_id) }` at `:634-640`. |
| `src/ui/bridge/steer.rs:134-150` | spawned-host caller (`:134` tasks, `:143` run). |
| `src/ui/bridge/sink.rs:233-292` | sink caller; `:254-258` documents that the `await` **holds the session loop hostage** for the whole subtask (pool slot wait included) — relevant to the latency trade-off in §5. |

### 1.5 Dispatch inside `delegate()`

`src/orchestrator/mod.rs:203-364`:

- `:215-225` recursion-depth guard.
- `:230-235` `DelegationEvent::Started`.
- `:258-273` prompt lookup via `saved_prompt_path_for_task(&prompts_dir, req.task_id)` (helper at
  `:509-519`, builds `<task_id>.md`); a task id rejected by `validate_task_id` is reported and
  treated as "prompt unavailable" → JIT.
- `:281-294` **this is the ad-hoc path**: no saved blueprint → JIT
  `PromptBuilder::build_blueprint(Some(&self.client), Some(model), &catalog, &req, Some(&prompts_dir))`.
  That call persists **only** `<task_id>.md` (see §2.2) — **never a validation brief, never a skip record.**
- `:296` `IsolatedContext::from_request(...)` binds `ctx.task_id` from the request
  (`src/agents/mod.rs:135-146`).
- `:326-327` `worker.run(ctx).await`.
- `:374-398` `apply_check_off()` — requires `MissionMarker::Complete` **and** a body-level complete
  marker **and** `Plan::check_plan_on_deliverable(...)` returning `Ok(true)`. For an ad-hoc id the
  plan has no such line, so `Plan::check_off` (`src/manager/phase.rs:729-745`) logs
  `"check_off({task_id}): task id not found or already checked"` (`:739`) and returns `Ok(false)`.

Contrast: the **tool** path `delegate_task` in `src/orchestrator/delegate.rs` is plan-bound —
`task_id` mandatory `:60-78`; refuses re-delegation of checked-off tasks `:80-97`;
`strip_reserved_validation_tokens` `:292-325` removes `(validate)`/`(no-validate)` from the brief;
`brief_for_task` `:256-274`. The comment block at `:241-255` states explicitly that the directive
is honoured **only** by the write side (`agents::prompt_builder` → `<task_id>-validation.md` /
`-validation-skip.md`), never by prose.

---

## 2. Is a `<task_id>-validation.md` ever authored for an ad-hoc task? **No.**

### 2.1 The only writer of the two verdict records

`src/agents/prompt_builder.rs:812-966` — `pregenerate_for_plan_offline(plan, catalog, prompts_dir)`
is the **only** function in the codebase that writes `<task_id>-validation.md` (inline template at
`:923-931`, `std::fs::write` at `:941`, stale-record hygiene at `:940` / `:707-745`) and the only
writer of `<task_id>-validation-skip.md` (`:896-901` → `write_validation_skip_record` `:750-776`).

Its **only two callers** are plan-creation sites:

- `src/orchestrator/mod.rs:193` (inside `create_plan`)
- `src/harness/plan.rs:75` (`create_plan` tool)

Both iterate the parsed **plan lines** (`crate::plan_parse::TaskLine`, each with `.task_id`).
⇒ A brief or skip record can exist **only for an id that appears as a plan line.** Ad-hoc steering
tasks never appear as plan lines (§1.3), so they get neither record.

### 2.2 The write-decision function is never consulted on the ad-hoc path

`src/agents/prompt_builder.rs:673-700` — `validation_write_decision(directive, agent)`:

```rust
match directive {
    ValidationDirective::NoValidate => WriteDecision::SkipRecord,
    ValidationDirective::Validate | ValidationDirective::Auto if role_exempt => WriteDecision::Nothing,
    ValidationDirective::Validate | ValidationDirective::Auto => WriteDecision::Brief,
}
```

Note the default: **`Auto` → `Brief`**, i.e. "validate unless the plan line says otherwise".
`WriteDecision` is at `:45-53`. `role_exempt` (`planner`/`validator`) at `:689-692`.
This function is reached **only from `pregenerate_for_plan_offline`**, so it never runs for
`steer-task-*`.

### 2.3 What the ad-hoc JIT path does author

`src/orchestrator/mod.rs:281-294` → `build_blueprint` (`src/agents/prompt_builder.rs:299-337`) →
`save_to_disk` (`:94-...`), which writes **only** the blueprint file named by
`prompt_file_stem` (`:76-84`) = `<task_id>.md`. So the ad-hoc run does end up with
`.marmel/prompts/steer-task-1.md` — and, as a side effect, `.marmel/prompts/` is guaranteed to be a
directory (`has_prompts_dir == true` in the read-side gate; it usually already exists because plan
pregeneration created it). But there is **no** `-validation.md` and **no** `-validation-skip.md`.

(For completeness, the genuinely id-less JIT case: `prompt_file_stem` falls back to `role_name`
when there is no `task_id`, so a blueprint is saved as `coder_specialist.md` —
`prompt_builder.rs:76-84`, test `:1849-1857`. The steer path never hits this, because it always
passes `Some(task_id)` — `src/ui/bridge/arbiter.rs:624-630`.)

---

## 3. Does an ad-hoc task have a task id that participates in the verdict lookup? **Yes.**

- `src/task_id.rs:39-41` `normalize_task_id_ref`, `:47-...` `normalize_task_id`,
  `:129-175` `validate_task_id` — the grammar accepts alphanumerics plus `-` and `_`, ≤ 64 chars.
  **`steer-task-1` passes** (`prompts/steer_arbitrator.md:36` literally suggests that spelling).
- `src/agents/runner/execution.rs:250-253` `clean_task_id = ctx.task_id.as_deref().and_then(normalize_task_id)`
  → `Some("steer-task-1")`.
- `src/agents/runner/execution.rs:341-346` config defaults: `auto_validate_enabled = true`,
  `max_val_iterations = 5`; `:348-352` `prompts_dir = <ws_root>/.marmel/prompts`,
  `has_prompts_dir = prompts_dir.is_dir()`.
- `src/agents/runner/execution.rs:42-51` `validation_probe_engaged(agent, enable_validator,
  max_val_iterations, has_prompts_dir)` = `has_prompts_dir ∧ role-ok ∧ max_val_iterations > 0`
  → **true** for a `coder`/`researcher` ad-hoc run.

So the probe runs, resolves `steer-task-1-validation.md`, and lands on the fail-closed arm.
**The failure is not caused by a missing id; it is caused by a valid id with neither verdict record.**

---

## 4. The fail-closed chain: missing brief → revoke → FAILED

### 4.1 Gap classification (read side, first site)

`src/agents/runner/execution.rs:376-439`:

- `:384-390` no task id → gap `"the run carries no task id, so no `-validation.md` verdict file
  could be located under …"`.
- `:398-411` **the arm that fires for ad-hoc tasks**: `ValidationBrief::Absent { path }` **and**
  `PromptBuilder::validation_skip_record_for_task(Some(tid)) == None` → gap
  `"{path} does not exist, so no validation verdict is recorded for this task"`.
- `:412-419` `Unreadable`, `:420-425` `Blank`, `:430-436` `InvalidTaskId` → also gaps.
- `:427` `Present` → no gap.
- Role hard opt-out: `validation_configured_for_role` `:32-34` (`planner`/`validator` never
  validated); verdict-required conjunction `:61-70`.

### 4.2 Hard failure

`src/agents/runner/execution.rs:776-805`: with `requires_validation` and a `missing_verdict_reason`,
it sets `validation_passed = false`, `validator_critique = Some("Validation was not performed —
{gap}. The deliverable is reported as not validated.")`, logs `ERROR` (`:787`, the exact log line
seen in §0), emits `[Validator] NO VERDICT FILE for …` (`:789-791`), sets worker status
`"Failed (no validation verdict file)"` (`:800-803`) and `break`s the validation loop.

### 4.3 Revocation of `MISSION COMPLETE`

- `src/agents/runner/execution.rs:1202-1228` final assembly →
  `src/agents/runner/assembly.rs:23-78` `assemble_final_deliverable(validation_passed=false, …)`.
- rejected path `:54-77`: prefix `VALIDATOR REJECTION: {critique}` (`:56-59`) →
  **`revoke_completion(final_content)` at `:63`** → `failed_trailer("Validator rejected deliverable")`
  at `:64-69`.
- `src/markers.rs:546-555` `revoke_completion()` rewrites every case-insensitive occurrence of
  `MISSION COMPLETE` into `REVOCATION_TOKEN` (`REVOCATION_TOKEN` `:270`); marker grammar:
  `MARKER_COMPLETE` `:64`, `decorated()` `:278`, `failed_trailer()` `:285`.
  Consequence: the `Ok(true)` branch of `apply_check_off` is never reached anyway
  (`src/orchestrator/mod.rs:374-398`) — the plan line (which does not exist for ad-hoc) stays unchecked.
- Tests pinning this behaviour: `src/agents/mod.rs:376` (`"MISSION COMPLETE (t-006) — revoked before
  finalization"`) and `:421-432` (`"When validation failed (rejected), revoke MISSION COMPLETE and
  append FAILED"`, asserting `!contains("MISSION COMPLETE")` and
  `contains("FAILED (Validator rejected deliverable)")`).

### 4.4 Mirrors of the same read (three sites must stay consistent)

| Site | Lines | Message |
|---|---|---|
| `src/agents/validation.rs` | `ValidationBrief` enum `:55-113`; `validation_prompts_dir()` `:118-122`; `probe_validation_brief()` `:134-150`; `probe_validation_brief_for_task()` `:155`; `validation_brief_absent()` `:164-166`; gap return `:328-393` | `"No validation verdict recorded for {tid} under {dir}: the deliverable was not validated."` (`:388`) |
| `src/agents/runner/fix_loop.rs` | `validation_skip_verdict()` `:459-474`; `no_verdict_outcome()` `:480-…` | skip-record short-circuit vs. no-verdict outcome |
| `src/agents/runner/execution.rs` | `:42-51`, `:61-70`, `:376-439`, `:776-805` | as above |

---

## 5. Do ad-hoc tasks appear in the plan / how are directives parsed?

- **They never appear.** `extract_tasks_to_delegate` + `execute_steer_subtask` never touch
  `.marmel/execution_plan.md` (§1.2, §1.3); `check_off` consequently warns "task id not found"
  (`src/manager/phase.rs:729-745`).
- `src/orchestrator/plan_summary.rs:10-36` (doc) and `render_step()` `:37-64`: the arbiter-facing
  plan-progress summary is rebuilt from parsed plan lines and **deliberately drops** the
  `(validate)` / `(no-validate)` token from model-facing text (imitation hazard), while keeping the
  role hint. ⇒ the arbitration model is never even shown the directive vocabulary.
- `src/plan_parse.rs:58-102`: `ValidationDirective { Auto (= default), Validate, NoValidate }`,
  closed set `VALIDATION_DIRECTIVES = ["validate", "no-validate"]` at `:80`,
  `parse()` `:88-95` fail-closed (anything else → `Auto`); position rule for the trailing
  directive group at `:757-773` (`X (researcher) (no-validate)` is honoured; a token outside that
  grammar position is prose → stays `Auto`).

---

## 6. Two candidate fixes — touch points and trade-offs (**no implementation**)

### Option A — ad-hoc arbitrator tasks default to **no-validate**

A1. Preferred single seam (write side, keeps the invariant "every validation-eligible id has
exactly one durable record"):

| # | Touch point | Change |
|---|---|---|
| A1.1 | `src/orchestrator/mod.rs:244-294` | In the JIT / no-saved-blueprint branch (i.e. for any id with no plan line), author a **durable no-validation record** before `worker.run`. |
| A1.2 | `src/agents/prompt_builder.rs:750-776` (`write_validation_skip_record`) | currently `pub(crate)` and reachable only from `pregenerate_for_plan_offline` (`:896-901`); expose it (or a thin `write_adhoc_validation_skip_record`) for the JIT path. |
| A1.3 | `src/agents/prompt_builder.rs:34-35` (`VALIDATION_SKIP_REASON`) | the one-line reason is *"Plan directive (no-validate): validation not required"* — an ad-hoc record authored with that string **misreports provenance** (no plan directive existed). Needs a distinct reason (e.g. ad-hoc/steering provenance) or the record must be a different kind. Pinned by tests at `:1130-1134`, `:1241`, `:1312`. |
| A1.4 | `src/agents/prompt_builder.rs:673-700` (`validation_write_decision`) | optional cleaner modelling: a new `WriteDecision` arm for "eligible id with no plan line" instead of special-casing at the call site; `WriteDecision` enum `:45-53`. |
| A1.5 | `src/orchestrator/steer.rs:652-723` | if provenance/decision must be explicit at the source, mark the delegation as ad-hoc there (and/or let the arbitrator schema carry it — see A1.6). |
| A1.6 | `src/ui/bridge/action.rs:27-35`, `prompts/steer_arbitrator.md:31-37`/`:44-56` | required only if the flag becomes an explicit, model-supplied schema field. Adds a field to a vocabulary that today has exactly three subtask fields. |
| A1.7 | stale hygiene | `delete_stale_skip_record` / delete helpers `src/agents/prompt_builder.rs:707-745` — ad-hoc ids are ephemeral and can repeat (`steer-task-1` every session), so a previously written record must not shadow a later intent (and vice-versa, if Option B is used). |

A2. Alternative (read-side) placement — **weaker, not recommended**:

- `src/agents/runner/execution.rs:42-51` (`validation_probe_engaged`) and/or the gap block
  `:376-439`, plus their mirrors `src/agents/validation.rs:328-393` and
  `src/agents/runner/fix_loop.rs:459-474`.

**Trade-offs of Option A:**

- ✅ Cheapest runtime cost: no validator round-trip, no fix loop.
- ✅ Preserves the fail-closed invariant *as data*: a durable record still exists, and the three
  read sites (`execution.rs`, `validation.rs`, `fix_loop.rs`) keep working unchanged.
- ✅ Matches the documented UX for a skip: completes with `MISSION COMPLETE (<id>)`, worker panel
  reads `Completed (validation skipped by design)` (`README.md:445`).
- ⚠️ It exempts **every** id that has no plan line — including a typo'd or hallucinated plan id
  reaching `delegate_task`. That partially defeats the purpose of the fail-closed guard, which
  exists precisely to catch "id refused by the grammar / id never planned" cases
  (`prompt_builder.rs:892-895` comment; `README.md:444`).
- ⚠️ The record's provenance string becomes a lie unless `VALIDATION_SKIP_REASON` is
  specialised (A1.3); `README.md:445` states the skip record is produced by *"the only input that
  can ever produce this file"* — the `(no-validate)` plan directive. Docs + tests
  (`tests/test_dynamic_prompts.rs`, `tests/test_validation_loop.rs`, `prompt_builder.rs:1104-1200`)
  assert that exclusivity.
- ⚠️ Read-side variant (A2) additionally contradicts the pinned tests:
  `tests/test_validation_loop.rs:1180-1240` (missing brief + no record ⇒ `"Validation was not
  performed"`, `"t-701-validation.md does not exist"`, `FAILED`, no `MISSION COMPLETE (t-001)`
  token), `:1300-1360`, `:1400-1425`; `tests/test_specialist_stream.rs:735, 878, 1268`;
  `tests/test_harness.rs:765`; and `README.md:35, 37, 444, 445, 492, 576`.

### Option B — generate the validation brief for ad-hoc tasks

| # | Touch point | Change |
|---|---|---|
| B1 | `src/agents/prompt_builder.rs:923-931` | the inline brief template (front-matter `role_name: "validator"`, `task_id`, `target_brief`, allowed tools incl. `leave_verdict`, plus the 3-step verification protocol) must be extracted into a reusable `write_validation_brief(prompts_dir, task_id, brief)` shared by `pregenerate_for_plan_offline` and the ad-hoc path. |
| B2 | `src/orchestrator/mod.rs:281-294` | call it from the JIT branch (single choke point covering *both* `steer` ad-hoc and any un-planned `delegate_task` id), right where `<task_id>.md` is saved by `build_blueprint` (`prompt_builder.rs:299-337`, save at `:330-335`). |
| B2′ | *or* `src/orchestrator/steer.rs:726-746` | call it only for steering-created tasks (narrowest blast radius; leaves un-planned `delegate_task` ids still failing closed). |
| B3 | `src/agents/prompt_builder.rs:689-692` | role-exempt handling: `planner`/`validator` ad-hoc runs must **not** get a brief (the read side hard-exempts them at `execution.rs:32-34`), mirroring `WriteDecision::Nothing`. |
| B4 | `src/agents/prompt_builder.rs:707-745` | hygiene: delete any stale `-validation-skip.md` **before** attempting the brief, exactly as the plan path does (`:940`, comment `:933-939`, fail-closed write-failure handling `:941-956`) — otherwise an old record silently exempts a task that was meant to be audited. |
| B5 | `src/orchestrator/steer.rs:652-723` / `prompt_builder.rs:636-641` | the brief's `target_brief` / "Brief:" text can only come from the arbitrator `prompt` (`:716-720`, which may fall back to the raw user message). Nothing else in the ad-hoc request describes the deliverable. |

**Trade-offs of Option B:**

- ✅ Keeps the documented default posture ("no directive ⇒ validate", `README.md:444`) and requires
  **no change** to the read side, the three probes, or the fail-closed tests.
- ⚠️ Cost: one real validator LLM round-trip **plus a fix loop** per steering subtask
  (`execution.rs:806-830` validation loop, `run_automated_validation` call at `:817`, fix loop in `fix_loop.rs`), up to `max_validator_iterations = 5`
  (`execution.rs:344-346`). Arbitration-steered tasks are typically read-only inquiries ("check
  git status", "why did the build fail"), where an audit pass adds latency for little value.
- ⚠️ Latency is user-visible: the steer sink `await`s the whole subtask inline and the comment at
  `src/ui/bridge/sink.rs:254-258` records that this *already* "holds the session loop hostage" for
  the duration (pool-slot wait included). Adding a validator + fix loop lengthens that block.
- ⚠️ Brief quality: the generated brief's mission text is derived solely from the arbitrator's
  `prompt`; a thin/blank prompt (`steer.rs:716-720` falls back to the raw user message) yields a
  weak audit target, and `leave_verdict` bounds/fix-loop churn apply to a low-quality brief.
- ⚠️ If `B2` (JIT choke point) is chosen, it also affects un-planned `delegate_task` ids — a
  behaviour change beyond the reported bug. `B2′` scopes it but forks the write logic.

### Option C — already-supported operator escape hatch (no code change)

`enable_validator = false` (aliases `auto_validate`, `enable_validation`) and/or
`max_validator_iterations = 0` per specialist (`src/config.rs:229-235`; consumed at
`src/agents/runner/execution.rs:341-346`, gates `execution.rs:32-34` / `:42-51` / `:61-70`).
`README.md:445` documents these as "the operator-level opt-out, and they outrank the directive".
Trade-off: it disables validation for **that role globally**, not just for ad-hoc steering tasks —
which is why it is a workaround rather than a fix.

---

## 7. Root cause, stated precisely

The verdict-record system is keyed on **plan-line authorship**, not on task existence:
`pregenerate_for_plan_offline` (`src/agents/prompt_builder.rs:812-966`) is the sole writer of both
`<task_id>-validation.md` and `<task_id>-validation-skip.md`, and it is reachable only from
`create_plan` (`src/orchestrator/mod.rs:193`, `src/harness/plan.rs:75`). The steering/arbitrator
path creates a *valid, plan-absent* task id (`src/orchestrator/steer.rs:673`, `:677`, `:713`) and
never runs any part of that write side (`src/orchestrator/steer.rs:726-746`,
`src/orchestrator/mod.rs:281-294` write only `<task_id>.md`). The read side then correctly does
what it was designed to do for "neither record exists": gap → hard failure →
`revoke_completion` → `FAILED`
(`src/agents/runner/execution.rs:398-411`, `:776-805`,
`src/agents/runner/assembly.rs:54-77`, `src/markers.rs:546-555`).

The gap is therefore a **missing write on the ad-hoc path** (Option A1 or Option B), and not a
defect in the fail-closed read side.

MISSION COMPLETE
