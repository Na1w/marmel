# Diagnosis + fix design: ad-hoc (Steer Arbitrator) tasks are revoked because `<task_id>-validation.md` is missing

**Mode:** READ-ONLY diagnosis. No source file was edited, no branch created, nothing committed.
The only file written by this pass is this report.

**Symptom (user):** every ad-hoc task created via the Steer Arbitrator ends revoked —
`MISSION COMPLETE` → `REVOKED` → `FAILED` — because `.marmel/prompts/<task_id>-validation.md`
does not exist.
**Stated preference (user):** ad-hoc tasks should be **`no-validate` by default**; authoring the
auditor brief is the secondary option.

---

## 0. Empirical baseline measured in this workspace

| Command | Result |
|---|---|
| `cargo test --lib agents::runner::execution` | `2 passed; 0 failed` (`a_skip_record_only_ever_takes_validation_off_for_an_eligible_role`, `hard_opt_out_roles_are_never_validation_eligible_under_any_record_combination`) |
| `cargo test --test test_validation_loop` | `19 passed; 0 failed` — incl. `test_missing_validation_verdict_file_fails_and_leaves_plan_unchecked`, `test_validation_skip_record_completes_the_run_and_checks_the_plan_line`, `test_a_no_validate_coder_line_authors_no_brief_and_never_reaches_the_validator` |
| `cargo test --test test_dynamic_prompts` | `10 passed; 0 failed` |
| `cargo test --lib prompt_builder` | `18 passed; 0 failed` |
| `cargo test --test test_specialist_stream` | `13 passed; 0 failed` — incl. `test_missing_verdict_file_is_an_unconditional_hard_failure`, `test_validation_skip_record_is_an_exemption_not_a_verdict_gap` |
| `cargo test --lib orchestrator::steer_tests::test_execute_steer_subtask_runs_specialist` | `ok` in 0.05 s — **see §7.1: it cannot observe this bug or its fix** (the live runner is bypassed inside `cargo test`) |
| `git status --porcelain --untracked-files=no` (start and end) | empty |

Log evidence of the live failure (production runs in this workspace):

- `.marmel.old.4/marmel.log:6174` —
  `ERROR marmennill::agents::runner::execution: coder-steer-task-1: Validation was not performed — /home/fredrik/marmel/wip/.marmel/prompts/steer-task-1-validation.md does not exist, so no validation verdict is recorded for this task. The deliverable is reported as not validated.`
- `.marmel.old.8/marmel.log:5172` — identical message for `researcher-steer-task-1`.
- `.marmel/prompts/` today holds only plan-authored ids (`t-001`..`t-005` with
  `-validation-skip.md`, `t-006`/`t-007` with `-validation.md`, `t-008` blueprint only);
  **no `steer-task-*` verdict record of either kind exists anywhere**, only `steer-task-1.md` /
  `steer-test-1.md` blueprints.

---

## 1. Confirmed root-cause chain (every link verified in code)

### L1 — The ad-hoc task id is a *valid* id
`src/orchestrator/steer.rs:652` `extract_tasks_to_delegate()` →
`:673` `let cleaned = crate::task_id::normalize_task_id_ref(&s.tool_call_id).to_string();`
(the arbitrator's own `tool_call_id`, decoration-stripped only — `src/task_id.rs:39-41`),
fallback `:676-677` `format!("steer-task-{idx}")`, and in the top-level fallback branch
`:711-713` `normalize_task_id(&s.tool_call_id)` else `"steer-task-1"`.
`steer-task-1` passes `src/task_id.rs:129-175` `validate_task_id` (ASCII alnum + `-`/`_`, ≤ 64 chars,
`MAX_TASK_ID_LEN` at `src/task_id.rs:58` explicitly names `steer-task-3` as an expected shape).
`prompts/steer_arbitrator.md` (DelegateTask paragraph, the `tool_call_id` bullet) literally tells the
model to use `steer-task-1`.
⇒ **The bug is not caused by a missing or invalid id.**

### L2 — The ad-hoc path carries no validation intent at all
`src/orchestrator/steer.rs:726-746` `execute_steer_subtask()`:
`:733` `let plan = Plan::default();` (a plan handle rooted at `.marmel`, `src/manager/phase.rs:203-207` —
**not** a plan containing this task), `:734` fresh `OrchestratorManager`, `:736-744`
`DelegationRequest { agent_name, prompt, snippets: vec![], task_id, image_urls: None, audio_urls: None, recursion_granted: false }`,
`:745` `manager.delegate(req).await`.
`extract_tasks_to_delegate` returns `Vec<(Agent, String, String)>` (`:652`) — three fields, no
directive, no plan line, no verdict intent. It never calls `Plan::create`, never appends a plan line,
never writes `<id>-validation.md` or `<id>-validation-skip.md`.
The only two entry points into it are `src/ui/bridge/arbiter.rs:617-642`
(`run_delegated_subtask` → `execute_steer_subtask(…, Some(task_id), …)` at `:624-630`), reached from
`src/ui/bridge/steer.rs:143` and `src/ui/bridge/sink.rs:264`. So **`steer.rs:745` is the single ad-hoc
delegation funnel** (the other three `delegate()` call sites are plan-bound:
`src/orchestrator/mod.rs:468` silent dispatcher over `try_pending_tasks()`, and
`src/orchestrator/delegate.rs:155` / `:186` for the `delegate_task` tool).

### L3 — The JIT branch authors only `<task_id>.md`
`src/orchestrator/mod.rs:258-273` looks up `saved_prompt_path_for_task` (helper `:509`; a grammar-rejected
id is reported and treated as "no prompt" → JIT). `src/orchestrator/mod.rs:281-294` is the JIT branch:
`PromptBuilder::build_blueprint(Some(&self.client), Some(self.client.model()), &catalog, &req, Some(&prompts_dir))`
(`build_blueprint` signature `src/agents/prompt_builder.rs:299-305`), which saves
**only** `<task_id>.md` (`prompt_file_stem` `:76-84`, `save_to_disk` `:94`).
No verdict brief, no skip record.

### L4 — The only writer of both verdict records is reachable only from `create_plan`
`src/agents/prompt_builder.rs:812-966` `pregenerate_for_plan_offline()` is the sole writer of
`<task_id>-validation.md` (template `:923-931`, `Self::delete_stale_skip_record` call `:940`, `std::fs::write` `:941`) and of
`<task_id>-validation-skip.md` (`WriteDecision::SkipRecord` arm `:862-900` →
`write_validation_skip_record` `:750-775`). Its **only two callers** are plan-creation sites:
`src/orchestrator/mod.rs:193` (`create_plan`) and `src/harness/plan.rs:75` (`create_plan` tool).
Both iterate parsed plan lines (`crate::plan_parse::TaskLine`). The write-side decision function
`validation_write_decision` (`:673-700`) is reached from nowhere else, and its default is
`ValidationDirective::Auto → WriteDecision::Brief` — "validate unless the plan line says otherwise".
⇒ **A record can exist only for an id that is a plan line. An ad-hoc id gets neither record.**

### L5 — The read side therefore fails closed (by design)
`src/agents/runner/execution.rs:250-253` `clean_task_id = ctx.task_id.as_deref().and_then(normalize_task_id)`
→ `Some("steer-task-1")`.
`:348-352` `prompts_dir = <workspace root>/.marmel/prompts`, `has_prompts_dir = prompts_dir.is_dir()`
(true: `create_plan` pregeneration created it).
`:42-51` `validation_probe_engaged` = `has_prompts_dir ∧ role-eligible ∧ max_val_iterations > 0`
(config defaults `:341-346`: `enable_validator` default `true`, `max_validator_iterations` default `5`)
→ **true** for `coder` / `researcher` / `debugger` / `generalist`.
`:376-439` probe: `ValidationBrief::Absent { path }` (`:398`) **and**
`PromptBuilder::validation_skip_record_for_task(Some(tid)) == None` (`:399-404`) → gap
`:405-409` `"{path} does not exist, so no validation verdict is recorded for this task"`.
`:456-460` `requires_validation = validation_verdict_required(...)` (`:61-70`) = `true`.
`:784-805` hard failure: `validation_passed = false`, `validator_critique =
"Validation was not performed — {gap}. The deliverable is reported as not validated."`,
`tracing::error!` (`:787` — the exact log line quoted in §0), `[Validator] NO VERDICT FILE`
status (`:789-791`), worker status `"Failed (no validation verdict file)"` (`:800-803`), `break`.
`:1203-1209` → `src/agents/runner/assembly.rs:23-78`; the rejected branch `:54-77` prefixes
`VALIDATOR REJECTION: …` (`:56-59`) and calls **`revoke_completion(fin.content)` at `:63`** →
`src/markers.rs:546-555` rewrites every case-insensitive `MISSION COMPLETE` into
`REVOCATION_TOKEN` (`"REVOKED"`, `:270`) and `failed_trailer("Validator rejected deliverable")`
(`:64-69`, `markers.rs:285-287`) is appended.
Mirrors of the same read, which must stay consistent: `src/agents/validation.rs:341-393`
(gap reason `:386-390`), `src/agents/runner/fix_loop.rs:459-474` (`validation_skip_verdict`) and
`:480-…` (`no_verdict_outcome`).

**Chain verdict: confirmed end-to-end.** The defect is a *missing write on the ad-hoc path*, not a
defect in the fail-closed read side.

---

## 2. Where the fix belongs: write side, not read side

**Decision: write side.** Concretely: at delegation time the steer path must author the durable
`<task_id>-validation-skip.md` record (the exemption mechanism the system already documents and
reads), instead of teaching the read side to guess.

Reasons, all grounded in the code:

1. **The read side is deliberately record-shaped and triple-mirrored.** All three read sites ask one
   question through one probe: `execution.rs:398-411`, `validation.rs:341-351`
   (`ValidationBrief::is_absent` gate), `fix_loop.rs:461-465`. The comments at those sites explicitly
   forbid one site being wider than another. A read-side rule ("this run is ad-hoc ⇒ no verdict needed")
   would have to be restated at all three sites and kept identical — a permanent consistency hazard.
2. **Provenance is knowable only at delegation time.** Nothing in `DelegationRequest`
   (`src/agents/mod.rs:100-120`) or `IsolatedContext` (`:123-146`, `from_request` `:135-146`) records
   *who* created the task. By the time `run_specialist_live_inner` probes the disk, "ad-hoc" is
   indistinguishable from "planned id typed wrong". A read-side fix therefore needs the very same
   new plumbing (a provenance signal) **plus** three consistent read-site edits **plus** it leaves no
   durable trace on disk.
3. **The disk record is the documented, observable exemption.** `README.md:445` defines
   `<task_id>-validation-skip.md` as the mechanism, and the read side already renders the resulting UX
   for free: `execution.rs:443-448` (`[Validator] SKIPPED by design for <tag>: <reason>`),
   `:1186-1193` worker status `Completed (validation skipped by design)`, and normal
   `MISSION COMPLETE (<id>)` assembly (`assembly.rs:23-51`).
4. **The read-side variant would break the pinned fail-closed tests**
   (`tests/test_validation_loop.rs:1175-1240`, `:1372-1440`, `:1458-1545`;
   `tests/test_specialist_stream.rs:700-760`), which exist precisely to prove "no brief + no record ⇒
   FAILED, plan line unchecked".

**Placement inside the write side — this is the crux:**

- ❌ **Do not implement it at the JIT choke point `src/orchestrator/mod.rs:281-294` unconditionally.**
  That branch is entered by *any* delegation with no saved blueprint — including a `delegate_task` call
  whose `task_id` is a typo'd or hallucinated plan id. `src/orchestrator/delegate.rs:58-78` only requires
  `task_id` to be non-empty and `:80-97` only refuses *already-checked-off* ids; nothing requires the id
  to be a real plan line. An unconditional record there would exempt exactly the class the fail-closed
  guard exists to catch (`prompt_builder.rs:892-895` comment: "A task with neither record — e.g. a task
  id refused by the grammar authority — keeps the fail-closed verdict guard"; `README.md:444`).
- ✅ **Author the record at the steer provenance site, `src/orchestrator/steer.rs:726-746`**, and keep
  the *mechanics* (grammar gate, hygiene, single writer) in `src/agents/prompt_builder.rs` next to the
  existing writer (`:750-775`). Blast radius: one call site, no schema change, no read-side change,
  no `delegate_task` behaviour change, and the "typo'd plan id" guard survives untouched.
- ⚠️ The reason string must not be reused: `VALIDATION_SKIP_REASON` (`src/agents/prompt_builder.rs:34-35`)
  says `Plan directive (no-validate): validation not required`, i.e. it asserts **plan provenance**
  that never existed for an ad-hoc task. It is pinned by `src/agents/prompt_builder.rs:1130-1134`
  (asserts `VALIDATION_SKIP_REASON.contains("Plan directive")`), `tests/test_dynamic_prompts.rs:97`
  and `tests/test_validation_loop.rs:1834` (literal string). A new, distinct constant is required.

---

## 3. Exact change list (minimal, ordered; do not implement here)

> Notation: **file:line → what to change → why**.

### C1 — `src/agents/prompt_builder.rs:34-35` → add a second, provenance-honest reason constant
```rust
/// Reason stored in the no-validation record of a task that **has no plan line**
/// (ad-hoc steering/arbitrator delegation). Deliberately distinct from
/// [`VALIDATION_SKIP_REASON`]: no plan directive ever existed for such a task.
pub(crate) const VALIDATION_SKIP_REASON_ADHOC: &str =
    "Ad-hoc delegated task (no plan line): validation not required";
```
*Why:* provenance honesty; `VALIDATION_SKIP_REASON` and its `"Plan directive"` pin
(`:1134`) and the two literal-string assertions (`tests/test_dynamic_prompts.rs:97`,
`tests/test_validation_loop.rs:1834`) must not move.

### C2 — `src/agents/prompt_builder.rs:750-775` → parameterise the single writer and widen visibility
`fn write_validation_skip_record(prompts_dir: &Path, task_id: &str)` →
`pub(crate) fn write_validation_skip_record(prompts_dir: &Path, task_id: &str, reason: &str)`, with
`:764` `format!("{reason}\n")`; update the one existing caller at `:898` to pass
`VALIDATION_SKIP_REASON`.
*Why:* keeps **one** writer for the record (single grammar gate at
`validation_skip_file_name` `:653-658`, single one-line file shape, single failure-log at `:766-773`);
forking a second writer is what would let the two records drift.

### C3 — `src/agents/prompt_builder.rs` (insert after `:775`) → the ad-hoc write-side decision + authoring
```rust
/// Write-side decision for a task that has **no plan line**. Mirrors
/// `validation_write_decision` (`:673-700`) but keyed on provenance, not on a
/// plan directive. `pub` (like `pregenerate_for_plan_offline` `:812`) so the
/// integration tests in `tests/` can drive the real writer.
pub fn record_adhoc_no_validation(
    prompts_dir: &Path,
    raw_task_id: &str,
    agent: crate::agents::Agent,
    plan_markdown: Option<&str>,
) -> Option<String>
```
Body order (each step is load-bearing):
1. `let id = validate_task_id(normalize_task_id_ref(raw_task_id))` — on error: `tracing::warn!`
   and `None`. Same gate as `validation_skip_file_name:653-658` / `save_to_disk:94`.
2. Role hard opt-out: `matches!(agent, Agent::Planner | Agent::Validator)` → `None`, no file
   (mirrors `WriteDecision::Nothing`, `:689-696`; the read side never probes for these roles,
   `execution.rs:32-34`/`:42-51`).
3. **Plan collision guard:** `plan_markdown.is_some_and(|md|
   crate::plan_parse::find_task_line(md, id).is_some())` (`src/plan_parse.rs:442-451`) →
   `tracing::warn!` + `None`. An id that *is* a plan line keeps plan semantics (§4).
4. Hygiene, exactly as the plan path's `SkipRecord` arm (`:862-900`):
   `delete_stale_validation_brief(prompts_dir, id)` (`:733-746`); on `Err` → `None` (never author a
   record while a readable brief survives — the read side ranks a present brief above a record).
5. `let _ = std::fs::create_dir_all(prompts_dir);` then
   `write_validation_skip_record(prompts_dir, id, VALIDATION_SKIP_REASON_ADHOC)` and return its name.
*Why:* keeps every invariant of the plan-side writer (mutual exclusivity, fail-closed on write failure,
no substitute file names) in one owner.

### C4 — `src/orchestrator/steer.rs:731-736` (inside `execute_steer_subtask`, right after
`let plan = Plan::default();` at `:733`, before the `DelegationRequest` at `:736`) → author the record
```rust
if let Some(raw) = task_id.as_deref() {
    // Provenance: this delegation has no plan line, so the write side never
    // took a validation decision for it. Record the ad-hoc default (no
    // validation) durably BEFORE the run, so the read side sees a record
    // instead of a verdict gap. Best effort: a failed record keeps today's
    // fail-closed behaviour (never fail open).
    let prompts_dir = crate::agents::validation::validation_prompts_dir(); // read-side spelling
    let plan_md = plan.read().ok().flatten();                              // check-off plan
    crate::agents::PromptBuilder::record_adhoc_no_validation(
        &prompts_dir, raw, agent, plan_md.as_deref(),
    );
}
```
Two different roots are used **on purpose**:
- `crate::agents::validation::validation_prompts_dir()` (`src/agents/validation.rs:118-122`) is
  `<workspace root>/.marmel/prompts` — exactly where the read side looks (`execution.rs:348-352`,
  `PromptBuilder::validation_skip_record_for_task:793-801`). Do **not** use `plan.dir().join("prompts")`
  (`src/orchestrator/mod.rs:255`): `Plan::default()` is the *relative* `.marmel`
  (`src/manager/phase.rs:203-207`), so under a scoped workspace root (`harness::with_workspace_root`)
  the two disagree and the record would be written where nobody reads it.
- `plan.read()` (`src/manager/phase.rs:356-371`) on the manager's own plan (`steer.rs:733`) is the plan
  whose lines `apply_check_off` (`src/orchestrator/mod.rs:374-398` → `Plan::check_off`
  `src/manager/phase.rs:729-745`) can touch — the correct plan to consult for the collision guard.
*Why:* `steer.rs:745` is the only place that knows "ad-hoc"; both UI steer call sites
(`bridge/steer.rs:143`, `bridge/sink.rs:264`) funnel through it, so one insertion covers both.

### C5 — explicitly **not** changed (guard rails for the implementer)
- `src/orchestrator/mod.rs:244-294` — untouched. Adding the write here would exempt typo'd
  `delegate_task` ids (§2).
- `src/agents/runner/execution.rs:42-51`, `:61-70`, `:376-439`, `:776-805`;
  `src/agents/validation.rs:118-166`, `:328-393`; `src/agents/runner/fix_loop.rs:459-474` — untouched.
- `DelegationRequest` (`src/agents/mod.rs:100-120`) — untouched. *If* a single choke point were ever
  preferred, the flag would have to be `#[serde(skip, default)] pub adhoc: bool`, because
  `src/orchestrator/delegate.rs:31` deserializes `DelegationRequest` straight from tool arguments: a
  `#[serde(default)] pub bool` would let a specialist delegate *out of* its own validation. Cost of that
  variant: every struct literal must be updated — 10 in `src/`
  (`steer.rs:736`, `mod.rs:459`, `orchestrator/tests.rs:38,82,258,677,1118`,
  `prompt_builder.rs:828,995,1597`) plus ~20 in `tests/` (`test_specialist_stream.rs` ×12,
  `test_role_gating.rs` ×4, `test_midturn_notice.rs` ×3, `test_orchestrator.rs:87`,
  `test_dynamic_prompts.rs` ×2, `test_internal_fast_calls.rs:581`,
  `test_specialist_schema_charge.rs:243`, `test_call_context.rs:460`). Not worth it for this fix.
- `prompts/steer_arbitrator.md` and `src/ui/bridge/action.rs:29-35` (`ACTION_DELEGATE_TASK` at `:35`) — untouched; no new schema field,
  no new vocabulary shown to the arbitration model.

### C6 — documentation (tracked) `README.md:444-445`
Row `<task_id>-validation.md` (`:444`) keeps its fail-closed sentence; row
`<task_id>-validation-skip.md` (`:445`) must state the **second** producer ("an ad-hoc delegated task
with no plan line, authored by `orchestrator::steer::execute_steer_subtask`") and its exact one-line
reason, and must drop the claim that the `(no-validate)` plan directive is "the only input that can ever
produce this file". `README.md:35, 37, 492, 576` restate the same contract and should be checked for the
same wording.

### C7 — optional polish (only if the noise bothers; not required for correctness)
After the fix, each ad-hoc completion reaches `apply_check_off` (`mod.rs:374-398`) with an id that has
no plan line, so `Plan::check_off` logs `check_off(steer-task-1): task id not found or already checked`
(`src/manager/phase.rs:739`) once per ad-hoc task. Same as today for ad-hoc runs; cosmetic only.

---

## 4. Planned-task semantics preserved — how, and the anti-typo guarantee

| Planned case | Behaviour after the fix | Evidence it stays pinned |
|---|---|---|
| plan line with **no** directive (`Auto`) | `<id>-validation.md` still authored by `pregenerate_for_plan_offline` (`:923-941`); missing/blank/unreadable brief still ⇒ gap ⇒ FAILED, plan line unchecked. The new code never runs for plan ids (C3 step 3). | `tests/test_validation_loop.rs:1175-1240`; `tests/test_specialist_stream.rs:700-760`; `src/agents/prompt_builder.rs:1060-1102` |
| plan line `(validate)` | brief authored (`:923-941`); stale skip record deleted first (`:940` + `delete_stale_skip_record:707-719`) | `tests/test_dynamic_prompts.rs::test_replanning_a_task_as_validated_drops_its_stale_skip_record`; `tests/test_validation_loop.rs:1908+` |
| plan line `(no-validate)` | unchanged `VALIDATION_SKIP_REASON` wording, unchanged writer semantics | `src/agents/prompt_builder.rs:1104-1141`; `tests/test_dynamic_prompts.rs:86-99`; `tests/test_validation_loop.rs:1826-1838` |
| mistyped directive (`(no-validat)`, reversed order, token inside prose) | stays `Auto` ⇒ brief authored ⇒ still validated (`src/plan_parse.rs:88-95`, `:780-…`) | `tests/test_specialist_stream.rs:1200-1290`; `tests/test_dynamic_prompts.rs::test_a_reversed_directive_order_is_inert_and_still_authors_a_brief`, `test_a_directive_token_inside_the_description_does_not_disable_validation` |
| **typo'd / hallucinated plan id passed to `delegate_task`** | **still fails closed.** `delegate.rs:58-97` accepts any non-empty unchecked id, and the JIT branch is left untouched (C5), so an id with neither record still lands on `execution.rs:398-411` → hard failure. | new test **T7** (§6) + existing `tests/test_validation_loop.rs:1175-1240` |
| brief present but blank / unreadable, with an ad-hoc record next to it | never exempted: the record is consulted only in the `Absent`/`NotFound` arm (`execution.rs:398-411`, `validation.rs:341-351`, `fix_loop.rs:461-465`) | `src/agents/validation.rs:1244-1280`, `tests/test_validation_loop.rs:1372-1440`; new test **T8** |

Anti-typo guarantee, stated as an invariant: **the ad-hoc record is authored only when the id is absent
from the plan document, and only on the steer call site.** Therefore no id reachable through
`delegate_task` (the tool path) can acquire the exemption through this fix.

---

## 5. Edge cases and the exact rule each one gets

**E1 — Ad-hoc id collides with a planned task id.** Realistic: `prompts/steer_arbitrator.md` shows
`'t-001'` as a `tool_call_id` example in the Cancel paragraph, so the model can emit a `DelegateTask`
subtask whose `tool_call_id` is a plan id.
Rule: **C3 step 3 → no record is authored.** Consequences, all correct and unchanged from today:
if the plan authored a brief, the ad-hoc run is audited against that brief; if the plan authored a
`(no-validate)` record, that record exempts it with plan provenance. Without the guard, the naive fix
would (a) delete the plan's brief via the hygiene step, (b) exempt the planned task, and (c) let an
unrelated ad-hoc deliverable **check off the plan line** through `apply_check_off`
(`mod.rs:374-398`) — a data-integrity regression.
Residual (pre-existing, out of scope): id reuse between steering and planning is itself hazardous; the
honest mitigation is the `steer-task-*` namespace (`steer.rs:677`, `:713`, and the arbitrator prompt).

**E2 — `validator` / `planner` ad-hoc tasks (role-level hard opt-out).**
Rule: **C3 step 2 → no record.** Their runs are never validation-eligible
(`execution.rs:32-34`, `:42-51`, `:61-70`; pinned by the lib test
`hard_opt_out_roles_are_never_validation_eligible_under_any_record_combination`), so a record would be
inert, would contradict `README.md:443` ("role-level opt-outs are never briefed"), and would create a
file that no read site is allowed to consult.

**E3 — Grammar-rejected steer id** (over 64 chars from a long `tool_call_id`, `..`, `/`, spaces,
non-ASCII). Rule: **C3 step 1 → `None`, warn, no file** (no repaired name — same rule as
`validation_skip_file_name:653-658`, `save_to_disk:94` (`prompt_file_stem:76-84`),
`prompt_file_stem:76-84`). The run then keeps today's outcome: `execution.rs:430-436`
(`ValidationBrief::InvalidTaskId`) → gap → FAILED, and `delegate()`/`saved_prompt_path_for_task`
(`mod.rs:258-273`) already treated the id as "prompt unavailable". **Never mint an exemption for an id
that cannot be safely addressed** — that is the whole point of the grammar authority.

**E4 — Restored sessions (`--resume`) and `/reset`.** The session bundle
(`.marmel/session/manifest.json`, `manager.json`, `ui.json`, `workers/<key>.json`, `README.md:449-453`)
mirrors worker histories only; **nothing under `src/session/` ever touches `prompts/`** (grep for
`prompts` in `src/session/*.rs` → no hits), and `Plan::clear()` (`src/manager/phase.rs:377-407`)
deletes plan/session files only. So: (i) a record authored before a restart is still on disk after
restore, and a replayed/re-issued ad-hoc task keeps the same behaviour; (ii) records survive `/reset`
(accepted: they carry no plan semantics and are re-derivable); (iii) re-running an interrupted ad-hoc
task after restore does **not** re-validate it — same as a `(no-validate)` plan task after restart.

**E5 — Repeated ids across sessions (`steer-task-1` every session).** The write is idempotent (one
line, same reason). Re-hygiene rule: step 4 deletes any stale `<id>-validation.md`, so an id that was a
*plan* task in an earlier session cannot keep a stale brief over the new ad-hoc record. The reverse
direction is already self-healing: when that id later appears as a plan line,
`pregenerate_for_plan_offline` deletes the stale ad-hoc record before writing the plan's brief
(`:940`, `delete_stale_skip_record:707-719`), restoring plan semantics — covered by new test **T5**.

**E6 — The `-validation-skip.md` grammar gate + mutual exclusivity.** The record name is produced only
by `validation_skip_file_name` (`:653-658`) after `validate_task_id`; a rejected id yields no record and
no repaired name. The two records stay mutually exclusive by construction (step 4 runs before the write;
`Err` from the delete ⇒ no record, mirroring `:870-889`). The record content stays exactly one line +
`\n` (`:764`), because `validation_skip_record:783-790` treats a blank file as *no decision* (fail-closed,
pinned by `tests/test_validation_loop.rs:1372-1440`).

**E7 — Ad-hoc task with no task id at all.** `run_delegated_subtask` always passes `Some(task_id)`
(`src/ui/bridge/arbiter.rs:624-630`), and `extract_tasks_to_delegate` always yields a non-empty id
(`steer.rs:676-677`, `:711-713`). If a future caller passes `None`, nothing is written and the runner's
"the run carries no task id" gap (`execution.rs:384-390`) still fires — unchanged.

**E8 — Write failure (read-only dir, IO error).** `write_validation_skip_record` logs and returns
`None` (`:766-773`); the run keeps the fail-closed gap. Deliberate: the fix must never turn a *record
failure* into an exemption, and equally never into a silent success.

---

## 6. Test plan

### 6.1 Existing tests at risk (must stay green; baseline all green today, §0)

| Test | Why it could break | Required outcome |
|---|---|---|
| `src/agents/prompt_builder.rs:1104-1141` `test_pregenerate_records_the_no_validation_decision` | asserts `VALIDATION_SKIP_REASON` verbatim **and** `.contains("Plan directive")` | unchanged const (C1 adds a sibling) |
| `tests/test_dynamic_prompts.rs:86-99` (`test_plan_creation_pregenerates_prompt_pairs_on_disk`) | literal `"Plan directive (no-validate): validation not required"` | unchanged const |
| `tests/test_validation_loop.rs:1802-1905` (`:1834` literal reason) | same | unchanged const |
| `src/agents/prompt_builder.rs:1227-1282`, `:1284-1325`, `:1327-1354`, `:1356-1388`, `:1390-1453`, `:1455-1525` | they exercise the writer/read helpers; **C2 changes the writer's signature** (call site `:898`) | update the one call site; readers keep their signature |
| `tests/test_validation_loop.rs:1175-1240` `test_missing_validation_verdict_file_fails_and_leaves_plan_unchecked` | the regression pin against read-side weakening | must stay exactly as is |
| `tests/test_validation_loop.rs:1263-1370` `test_validation_skip_record_completes_the_run_and_checks_the_plan_line` | writes its own reason fixture (`:1274-1279`), i.e. proves the reason string is opaque to the read side | passes with any reason string |
| `tests/test_specialist_stream.rs:700-760` and `:765-920` | the same fail-closed / exemption pair, incl. an exact status-channel assertion (`:908-911`) | unaffected (no read-side change) |
| `src/orchestrator/steer_tests.rs:255-274` `test_execute_steer_subtask_runs_specialist` | the new write runs inside it; with `validation_prompts_dir()` it drops `steer-test-1-validation-skip.md` into the repo's own gitignored `.marmel/prompts/` (already holds `steer-test-1.md`) | must not assert on the repo dir; see T6 for the scoped variant |
| `src/orchestrator/tests.rs` / the ~20 test literals listed in C5 | only break if `DelegationRequest` gains a field | avoided by the steer-site seam |
| `cargo test --test test_orchestrator`, `--test test_role_gating`, `--test test_midturn_notice`, `--test test_harness`, `--test test_internal_fast_calls`, `--test test_specialist_schema_charge`, `--test test_call_context` | blast-radius check for the C2 signature change and the steer-site insertion | run after implementation |

### 6.2 New tests to add

Write side — `src/agents/prompt_builder.rs` `mod tests` (deterministic, no network):

1. **`test_adhoc_record_carries_adhoc_provenance_not_plan_provenance`** —
   `record_adhoc_no_validation(&prompts, "steer-task-9", Agent::Coder, None)` returns
   `Some("steer-task-9-validation-skip.md")`; the file is exactly one line equal to
   `VALIDATION_SKIP_REASON_ADHOC`; assert `!VALIDATION_SKIP_REASON_ADHOC.contains("Plan directive")`
   while `VALIDATION_SKIP_REASON.contains("Plan directive")` still holds.
2. **`test_adhoc_record_is_not_authored_for_role_exempt_roles`** —
   `Agent::Planner` and `Agent::Validator` → `None` and no file on disk.
3. **`test_adhoc_record_is_not_authored_for_an_id_that_is_a_plan_line`** —
   plan markdown containing `- [ ] [t-001] Fix the parser (coder)`;
   call with `("t-001", Agent::Coder, Some(plan))` → `None`; a pre-existing
   `t-001-validation.md` is still present and byte-identical (proves the collision guard neither
   exempts nor destroys plan records). Extend with the checked-line variant
   (`- [x] [t-002] …`) to pin `find_task_line` matching completed lines too.
4. **`test_adhoc_record_replaces_a_stale_brief_and_never_coexists`** —
   pre-existing `<id>-validation.md` → after the call the brief is gone and the record exists; and the
   `Err` path (make the brief undeletable, e.g. read-only parent dir) → **no record** and the brief
   survives (fail-closed, mirrors `:870-889` and
   `src/agents/prompt_builder.rs:1390-1453`).
5. **`test_adhoc_record_is_dropped_when_the_id_later_becomes_a_plan_line`** —
   author the ad-hoc record for `t-077`, then run `pregenerate_for_plan_offline` on a plan carrying
   `- [ ] [t-077] … (coder)` → `t-077-validation.md` exists and
   `validation_skip_record(...)` is `None` (self-healing hygiene, E5).
6. **`test_adhoc_record_refuses_grammar_rejected_ids_and_writes_nothing`** —
   `"../x"`, `"a/b"`, `".."`, `".hidden"`, `""`, a 65-char id → `None` for every one and no file
   created (extend the hostile-id table at `:1273`, `:1356-1388`, `:1775-1831`).

Behaviour — `tests/test_validation_loop.rs` (mock backend, `with_workspace_root`, mirroring the
existing `t-706`/`t-709` fixtures at `:1263-1370` / `:1802-1905`):

7. **`test_adhoc_steering_task_defaults_to_no_validate`** —
   author the record through the real writer (`marmennill::agents::PromptBuilder::record_adhoc_no_validation`,
   hence its `pub` visibility), then `run_specialist_live` with `coder_ctx("steer-task-7")` and
   `coder_cfg_with_validator(url, 3)`. Assert: exactly 2 backend calls (zero validator passes), no
   request contains `Specialist Deliverable` / `leave_verdict`, deliverable contains
   `MISSION COMPLETE (steer-task-7)` and no `Validation was not performed`, status channel contains
   `[Validator] SKIPPED by design for coder-steer-task-7: Ad-hoc delegated task (no plan line): validation not required`,
   worker status is `Completed (validation skipped by design)`, and no plan line is checked (the id is
   not in the plan → `check_plan_on_deliverable` returns `Ok(false)`).
8. **`test_adhoc_plan_absent_id_without_a_record_still_fails_closed`** *(the guard against a read-side
   shortcut)* — same fixture **without** the record: `Validation was not performed`,
   `steer-task-8-validation.md does not exist`, `FAILED`, no `MISSION COMPLETE (steer-task-8)`, zero
   validator calls.
9. **`test_adhoc_record_never_rescues_a_blank_brief`** — ad-hoc record **and** a blank
   `steer-task-9-validation.md` on disk → still FAILED with the blank-file gap wording
   (mirrors `src/agents/validation.rs:1244-1280`).
10. **`test_adhoc_record_is_not_written_by_the_delegate_task_tool_path`** — drive
    `handle_delegate_task` (or `OrchestratorManager::delegate` with an un-planned id) and assert
    `<id>-validation-skip.md` was **not** created and the run still reports the verdict gap
    (anti-typo guarantee, §4).

Steer seam — `src/orchestrator/steer_tests.rs`:

11. **`test_execute_steer_subtask_authors_the_adhoc_record_for_a_unique_id`** —
    wrap in `marmennill::harness::with_workspace_root(tmp, …)` and use a unique id
    (`steer-test-adhoc-1`) so the repo's own plan can never collide (E1); assert the record file exists
    under `<tmp>/.marmel/prompts/` and that `Plan::default()` was consulted (i.e. nothing was written
    into the repository's `.marmel/`). Note: this test can only assert the **write**, because
    `try_run_specialist_live` returns `None` under `cargo test`
    (`src/agents/runner/mod.rs:100-108`, exe path contains `/deps/` unless `MARMEL_LIVE_TEST` is set)
    and `run_specialist_llm` returns the canned deliverable (`:22-52`) — which is why the existing
    `test_execute_steer_subtask_runs_specialist` passes in 0.05 s today despite the live bug.

### 6.3 Commands to run after implementation

```
cargo test --lib prompt_builder
cargo test --lib agents::runner::execution
cargo test --lib orchestrator::steer_tests
cargo test --test test_validation_loop
cargo test --test test_dynamic_prompts
cargo test --test test_specialist_stream
cargo test --test test_orchestrator -- --skip <slow>   # blast radius of the C2 signature change
cargo test                                            # full suite
```

---

## 7. Notes, residual risks, and explicitly rejected alternatives

1. **Why the existing steer test proves nothing:** `src/agents/runner/mod.rs:100-108` short-circuits
   the live runner inside `cargo test`, and `:22-52` substitutes a canned `MISSION COMPLETE`
   deliverable, so no probe, no revoke, no FAILED. Hence the behaviour tests (T7-T9) must drive
   `run_specialist_live` directly, as `tests/test_validation_loop.rs` already does.
2. **The exemption is disk-scoped, not run-scoped.** A leftover ad-hoc record for an id later re-used
   by a *different* mechanism inherits the exemption until `create_plan` re-hygienizes it (E5, T5).
   Bounded in practice by the `steer-task-*` namespace (`steer.rs:677`, `:713`,
   `prompts/steer_arbitrator.md`) and by the C3-3 collision guard at authoring time.
3. **No operator knob can force ad-hoc validation on.** `enable_validator` / `auto_validate` /
   `max_validator_iterations = 0` (`src/config.rs:229-235`, consumed at `execution.rs:341-346`) only
   ever turn validation **off**, and they outrank the directive (`README.md:445`). If a "validate ad-hoc
   tasks" preference is ever wanted, it must be a *new* switch consulted at C4 (skip the write), not a
   read-side override — that keeps the record as the single source of truth.
4. **Option B (author an auditor brief for ad-hoc tasks) rejected for the default** — it costs a real
   validator round-trip plus up to `max_validator_iterations = 5` fix-loop rounds per steering subtask
   (`execution.rs:806-830`, `fix_loop.rs`), and the steer sink `await`s the whole subtask inline
   (`src/ui/bridge/sink.rs:233-292`, comment at `:254-258` documents that it already "holds the session
   loop hostage"). Its brief text can only be the arbitrator `prompt` (`steer.rs:716-720`), which often
   falls back to the raw user message — a weak audit target. It also requires extracting the inline
   brief template (`prompt_builder.rs:923-931`) into a reusable writer and, for symmetry, deleting stale
   records first. If desired later, it is a strict addition on top of this design: same seam (C4),
   `WriteDecision::Brief` instead of `SkipRecord`.
5. **Option C (config opt-out) is a workaround, not a fix:** `enable_validator = false` /
   `max_validator_iterations = 0` disable validation for the whole role globally, not only for ad-hoc
   steering tasks.
6. **Read-side variant (rejected, documented for completeness):** would touch
   `execution.rs:42-51` + `:376-439`, `validation.rs:328-393`, `fix_loop.rs:459-474` — three sites that
   their own comments forbid from drifting — plus new plumbing through `DelegationRequest` /
   `IsolatedContext` (`agents/mod.rs:100-146`), and it would break the fail-closed pins listed in §6.1.

---

## 8. One-paragraph answer

The ad-hoc steering path mints a **valid, plan-absent task id** (`steer.rs:673`, `:677`, `:713`) and
never runs any part of the write side that owns the two verdict records
(`prompt_builder.rs:812-966`, reachable only from `create_plan` at `mod.rs:193` /
`harness/plan.rs:75`); the JIT branch writes only `<task_id>.md` (`mod.rs:281-294`). The read side then
does exactly what it was built to do for "neither record exists": verdict gap
(`execution.rs:398-411`) → hard failure (`:784-805`) → `revoke_completion` (`assembly.rs:63`,
`markers.rs:546-555`) → `FAILED`. The fix is a **write**, made at the only place that knows the task is
ad-hoc (`steer.rs:726-746`), reusing the single record writer (`prompt_builder.rs:750-775`, given a
`reason` parameter) with a **new provenance string** (sibling of `VALIDATION_SKIP_REASON` at `:34-35`)
and a **plan-collision guard**, so planned tasks — including typo'd plan ids — keep requiring a readable
`<task_id>-validation.md` and keep failing closed.

MISSION COMPLETE
