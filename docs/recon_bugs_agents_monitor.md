# Defect Audit — Specialist Agent Layer (`src/agents/**`) + Activity Monitor (`src/harness/monitor/**`)

Read-only audit. No source files were modified. Scope: `src/agents/{mod,catalog,coder,debugger,generalist,planner,researcher,validation,prompt_builder}.rs`,
`src/agents/runner/{mod,assembly,execution,fix_loop,formatting}.rs`, `src/harness/monitor/{mod,common,code,repetition,xml}.rs`.
(`src/manager/*` and `src/orchestrator/*` were only consulted for cross-boundary evidence, not audited.)

Baseline: `cargo test --lib --offline` → 372 passed, 7 failed (pre-existing, out of scope: `harness::pty`×4,
`harness::test_harness_delegate_task…`, `manager::loop` abort_midflight, `orchestrator::notice_tests` reply_to_arbitrator).

---

## HIGH

### H1 — Validation silently skipped when the task has no `-validation.md` prompt
`src/agents/runner/execution.rs:126-141`

`requires_validation` is computed as: if `.marmel/prompts/` **exists** (`has_prompts_dir`, :126) then it requires
`{tid}-validation.md` to exist (`has_validation_prompt`, :127-130), else falls back to the keyword/role heuristic.
So the *presence of any prompt file* converts "no validation prompt" into "no validation at all".
At :141 `validation_passed = !requires_validation || max_val_iterations == 0` → task is marked validated without any validator ever running.

Failing scenario (reproduced in this workspace): `.marmel/prompts/` contains `t-001.md … t-007.md` and **zero**
`*-validation.md`. Every one of those tasks gets `requires_validation = false`, `validation_passed = true`
(`grep -c` on the prompts dir returned 0 validation files). A second trigger: `task_id` absent → the whole branch is skipped.
Contributing cause: `prompt_builder.rs:568-589` (heuristic that *generates* validation prompts) never emits one for
Researcher/Generalist roles — and Generalist carries the `"*"` namespace, so it can also bypass role gating.

Severity: High (silent QA bypass, plan tasks auto-checked).
Minimal fix: default to *requiring* validation when `auto_validate_enabled` unless an explicit opt-out marker exists;
i.e. `has_prompts_dir && !has_validation_prompt` → `requires_validation = true` (generic validator brief), not `false`.

### H2 — `approved` path in marker assembly re-injects `MISSION COMPLETE` over an explicit `FAILED`
`src/agents/runner/assembly.rs:21-37`

`has_complete = upper.contains("MISSION COMPLETE")` (:21) is OR-ed with the *approved* verdict; when the verdict is
approved the function appends `"\n\nMISSION COMPLETE ({tid})"` (:33-34) or bare `MISSION COMPLETE` (:37) **without
checking for `FAILED`/`ERROR`** in the result. `mod.rs:129-132` then gives `MissionMarker::parse` precedence, so the
marker wins and the plan task is auto-checked.

Failing scenario: specialist prints `FAILED: could not reproduce the crash (missing repro)` and the validator (or the
fallback in H4/H5) returns approved → the appended marker makes `MissionMarker::parse` report Complete → the manager
marks the task done although the agent declared failure.

Severity: High.
Minimal fix: in the approved branch, skip appending the marker when the text already contains `FAILED:`/`ERROR:`;
prefer the terminal marker over the verdict, not the reverse.

### H3 — Rejection revocation only strips exact uppercase markers
`src/agents/runner/assembly.rs:47-49`

On rejection the revocation is `.replace("MISSION COMPLETE", "REVOKED")` (plus a lowercase variant). Any other casing
(`Mission Complete`, `MISSION COMPLETE.` inside prose, or a marker produced by H2's `(tid)` suffix form combined with
mixed-case text) survives, and `MissionMarker::parse` in `mod.rs:129-132` then classifies the rejected deliverable as
Complete.

Failing scenario: validator rejects, agent text contained `Mission complete` (title case, common in prose) → revocation
is a no-op → task marked complete despite `REJECTED`.

Severity: High.
Minimal fix: strip/normalize markers case-insensitively via the same parser used for detection (`MissionMarker::parse`)
rather than string `replace`.

### H4 — Inverted fallback in the validation-prompt loader ("no prompt ⇒ assumed approved")
`src/agents/validation.rs:181-196`

If `.marmel/prompts` exists and `{tid}-validation.md` is missing, it returns
`Ok((true, "No validation prompt defined for task; deliverable assumed approved."))` (:191). Missing configuration is
treated as a pass, and this composes with H1 to make validation opt-out-by-accident.

Severity: High.
Minimal fix: return the *generic* validator brief (or `Ok((false, "no validation config"))`) instead of an approval.

### H5 — Fix loop auto-approves after 3 verdict reminders
`src/agents/runner/fix_loop.rs:434-456`; `src/agents/validation.rs:308-315`, `:435-439`

After three nudges (`reminder {n}/3`) the loop exits with "assumed approved". A validator that never calls
`leave_verdict` — because the call was blocked by permissions, malformed (see M5), or the model simply stalls —
yields an **APPROVED** verdict. Locked in by `tests/test_validation_loop.rs:586`
(`test_validator_reminded_3_times_and_assumed_approved`), so a fix needs a test change too.

Severity: High (fail-open termination of the safety loop).
Minimal fix: fail closed — `Exhausted`/`verdict = false` with comments "no explicit verdict" and let the escalation
path decide; also surface the `Exhausted` variant (currently dead, see Dead code).

### H6 — Verdict parsing defaults to approved; any role can set `validation_passed`
`src/agents/validation.rs:110-127`; `src/agents/runner/execution.rs:570-575`, `:612-617`, `:620`

`validation.rs:110-127` maps unknown verdict tokens (e.g. `NEEDS_WORK`, `MAYBE`, empty string) to approved.
In `execution.rs:570-575` the `leave_verdict` handler takes the verdict from **any** tool name matched by
`is_leave_verdict_tool` and parses with `unwrap_or((true, …))` (:572), assigning `validation_passed` — with no check
that the calling role is the validator. The dispatch result is discarded (`let _ =` at :612-617) and the loop `break`s
at :620. The XML rescuer (M5) can synthesize exactly such a call from prose.

Failing scenario: specialist emits a malformed `<tool_call>leave_verdict …` block (or names a custom MCP tool
`foo_leave_verdict`, see UNCONFIRMED list); rescue produces a call, `unwrap_or` yields approved, loop breaks, task
validated. Self-validation / validation bypass.

Severity: High.
Minimal fix: default unknown verdicts to *rejected*; gate `leave_verdict` handling on the validator/auditor role and
on an exact tool-name match; propagate dispatch errors instead of `let _ =`.

---

## MEDIUM

### M1 — Blueprint tool allow-list *replaces* registry namespaces (permission drift)
`src/agents/runner/fix_loop.rs:73-97`; `src/harness/mod.rs:416-421`; `src/agents/catalog.rs:148-189`

`assemble_tools` intersects registry namespaces with the blueprint's `allowed_tools` by *replacing* the effective set
(no union/intersection semantics), and `harness/mod.rs:416-421` gates **only** on the prompt list. So the AGENTS.md /
SKILL.md / LLM-synthesized blueprint — not the archetype — is the effective permission set.

Evidence: `AGENTS.md` Researcher entry lacks `write_file`/`replace`/`run_command`; `.marmel/prompts/t-003.md`
(researcher) lists `allowed_tools=[read_file,grep_search,glob,rebirth,reply_to_arbitrator]` → `write_file` is
Forbidden even though `execution.rs:41-43` instructs "You MUST save files". Conversely `catalog.rs:148-169`
(debugger archetype) lacks `write_file` while `debugger.rs:30` grants it; `catalog.rs:172-189` (researcher) lacks
`replace` while `researcher.rs:31` grants it. Any test asserting "validator has write_file" therefore depends on which
source wins at runtime.

Severity: Medium-High (tool permission mismatches, both directions).
Minimal fix: single source of truth — intersect(registry ∪ archetype, blueprint) and assert equality in a unit test
per role.

### M2 — No turn / iteration / deadline cap on specialist or validation loops
`src/agents/runner/execution.rs:145` (`loop {`) and `src/agents/runner/fix_loop.rs:312` (`loop {`)

`grep -rn "max_turns" src` → no hits. Termination depends entirely on the model emitting a terminal marker or verdict;
with H5/H6 fail-open paths removed there is no bounded exit at all.

Severity: Medium (non-terminating/cost-unbounded sessions; can hang the manager).
Minimal fix: add `max_turns` + wall-clock deadline to `LoopParams` and return a new `Exhausted(TurnBudget)` variant.

### M3 — Repetition detection is O(n²) over the whole 16 384-char buffer, called per streamed delta
`src/harness/monitor/repetition.rs:15`, `:291-305`; call site `src/llm/stream.rs:288-289`

`is_repeating()` is invoked for every streamed delta and scans the full buffer (cap 16 384 chars, :15) instead of a
tail window. Measured with a standalone replica (`/tmp/rep_bench.rs`, same algorithm): 4 500 chunks ≈ 22.5k chars →
**23 s** of pure detection; the ~26k-char corpus exceeded a 20 s timeout.

Severity: Medium (multi-second stalls on long outputs; effectively a stream timeout bug).
Minimal fix: keep a rolling tail window (e.g. last 512 bytes) and incremental line/gram counters.

### M4 — Repetition intervention text lacks the `ERROR: ` prefix → blocked calls counted as executed
`src/agents/runner/fix_loop.rs:223-231`, `:265`; consumer `src/agents/runner/execution.rs:399`, `:681-686`, `:717`

The intervention payload (`"TOOL REPETITION DETECTED: …"`) is not prefixed with `"ERROR: "`, so the caller computes
`execution_succeeded = true`, increments `tools_executed_count` (:681-686) and resets the nudge counter — defeating the
guards at :399/:717 ("no tools executed"). `intervention_fallback` params (`fix_loop.rs`) are never consumed.

Severity: Medium (repetition guard becomes a no-op and masks stuck agents).
Minimal fix: prefix intervention results with `ERROR: ` (or return a typed `Blocked`) and count only successful dispatches.

### M5 — XML rescuer emits wrong argument key for `grep_search` (`query` vs `pattern`)
`src/harness/monitor/xml.rs:150-160` (also attribute list `:171-181`); `src/agents/runner/formatting.rs:27-33`, `:101-117`
Schema: `src/harness/search.rs:26` requires `pattern`.

When the rescued block body is plain text, the rescuer inserts the arg under `"query"` (:157) while `grep_search`
requires `"pattern"` → the rescued call **always** fails with a schema error, and the model receives a confusing
error instead of results. `formatting.rs:28`/`:103` preview the same stale `"query"` key (falling back to `pattern` at
:33/:117) so previews show empty search terms.
Related: the rescuer also fires on `<tool_call>` text appearing inside prose or fenced code
(`execution.rs:240-245`, `fix_loop.rs:400-405`), executing quoted examples.

Severity: Medium.
Minimal fix: use `pattern` for `grep_search` in the rescuer and previews; skip rescue inside fenced code blocks.

### M6 — Validator role contract is internally inconsistent
`src/agents/validator.rs:7-9`, `:58`, `:66-67`; `src/agents/validation.rs:205-206`; `src/agents/prompt_builder.rs:592`; `src/agents/catalog.rs` (validator archetype)

`validator.rs:58` grants `run_command`, but the generated validator brief (`validation.rs:206`) explicitly **forbids**
running commands. `validation.rs:205` offers `pty_*` to the validator, yet the generated blueprint allow-list
(`prompt_builder.rs:592`) contains only `read_file, grep_search, glob, run_command, leave_verdict, rebirth` → the
instructed tool is Forbidden at dispatch. Docs (`validator.rs:7-9`) omit `run_command`; the catalog archetype omits
both `run_command` and `pty`. `validator.rs:66-67` registers a duplicate pair `"pty__*"` / `"pty_*"`.

Severity: Medium (validator told to do things it cannot do; tool-not-found churn and false rejections).
Minimal fix: one validator tool table used by archetype, registration, brief, and blueprint generation; drop the
`pty__*` typo entry.

### M7 — Hardcoded/stale tool namespaces injected for blueprint-less roles; `reply_to_arbitrator` force-added
`src/agents/runner/execution.rs:35-38`; `src/agents/runner/fix_loop.rs:126-133`

`execution.rs:35-38` embeds a literal tool list that has drifted (no `pty_*`, `delegate_task`, `sleep`), so roles
without a blueprint are told a tool surface that does not match the registry. `fix_loop.rs:126-133` force-adds
`reply_to_arbitrator` regardless of the blueprint allow-list, contradicting M1's "blueprint is authoritative".

Severity: Medium (prompt/permission divergence; prompt-injected tool names cause repeated failed calls).
Minimal fix: derive prompt tool lists from the same `assemble_tools` result used for dispatch.

### M7b — `verdict_nudge_count` never reset after tool progress
`src/agents/runner/fix_loop.rs:308`, `:434-445`

The counter is not reset when the validator successfully uses tools, so a validator that does 3 legitimate inspection
turns *and then* needs one more nudge hits the ceiling and triggers the H5 auto-approve.

Severity: Medium.
Minimal fix: reset on any successful tool dispatch, or make the counter per-no-tool-turn-run.

### M8 — Monitor text-repetition latch never re-armed; text fed across turns
`src/harness/monitor/mod.rs:97-98`, `:116`, `:191-192`, `:207-208`; `src/agents/runner/execution.rs:267`

`repetition_fired` is set once (:192) and only cleared by `reset_text_break()` (:207), which is called **only** from
`monitor_tests.rs:401` — never from production code. After the first detection, monitor-based text repetition is
disabled for the remainder of the session. Compounding: `execution.rs:267` `monitor.feed_text(...)` accumulates across
turns while a separate per-turn `rep_detector` also runs → double detection and cross-turn false positives.

Severity: Medium (guard silently disarms).
Minimal fix: re-arm the latch at each turn boundary (`reset_text_break` per turn) and feed turn-local text.

### M9 — Code-monitor heuristics neuter the repetition detector for prose/plans
`src/harness/monitor/code.rs:34-46`, `:151`+; `src/harness/monitor/repetition.rs:338`+

`is_code_line` treats lines starting with `#` or `*` as code, so markdown headings/bullets (i.e. most agent output)
are classified as code and *excluded* from repetition counting. `is_code_word` includes `ok`, `if`, `for`, `from`,
`case`, `default`, `try`, `type`, so ordinary prose 4-grams count as "code-like" and the signal degrades.
`repetition.rs:338`+ scores the whole 16k buffer rather than a tail window → repeats from earlier turns re-trigger
(misses the current loop, flags stale text).

Severity: Medium (false negatives on real loops, false positives across turns).
Minimal fix: exclude markdown structural lines, trim the stopword-ish token list, evaluate on a tail window.

---

## LOW / UNCONFIRMED

- **UNCONFIRMED** — `is_leave_verdict_tool` suffix match `ends_with("_leave_verdict")`
  (`src/agents/validation.rs:41-49`): any MCP tool named `anything_leave_verdict` would be accepted as a verdict,
  feeding H6. Not reproduced (no such tool registered in this workspace).
- **UNCONFIRMED** — path basis mismatch: `Plan::default()` uses relative `".marmel"`
  (`src/manager/phase.rs:261`, `:272`) while `execution.rs:122-125` and `validation.rs:174-178` use canonicalized
  `get_workspace_root()`. Today they agree because `main.rs:52` calls `set_workspace_root(&cwd)`; breaks if a caller
  sets a different root or process cwd changes.
- **UNCONFIRMED (doc/threshold drift)** — `monitor/mod.rs:70-73` and `common.rs:57-60` document "≥3" while the default
  `repetition_threshold = 5` (`src/config.rs:57`); `monitor/mod.rs:73`, `:186` and `repetition.rs:286` describe a
  "1000-char" buffer while the actual cap is 16 384 (`repetition.rs:15`).
- **UNCONFIRMED** — `catalog.rs` `load_skills_from_dir` doc comment says "recursive" but the implementation is
  single-level.

## Dead code / duplication to unify

- `fix_loop.rs:46` — `FixLoopResult::Exhausted` is never constructed; the corresponding arms in
  `validation.rs:308` and `:435` are unreachable dead code (also hides the M2/H5 fix).
- `fix_loop.rs:465-468` — `LoopParams::dispatch_verdict_tools` is unreachable: the verdict loop at `:410-424`
  returns first. Field/params otherwise unused (`intervention_fallback`, see M4).
- `common.rs:24-52` — `same_operation` / `from_tool_call` unused in production; `semantic_json_eq` referenced only by
  tests.
- `runner/mod.rs:43-52` vs `:67-76` — duplicated test-runner detection logic.
- `execution.rs:371-374`, `:395-397`, `:711-715` — four hand-rolled terminal-marker string checks duplicating
  `MissionMarker::parse` (root cause of H2/H3 casing drift).
- `execution.rs:600-607` duplicates `assembly.rs::update_revision`.
- `SpecialistRegistry::canonical()` re-resolved per call (`execution.rs:80`, `validation.rs:216`, `:375`,
  `harness/mod.rs:423`) plus `execution.rs:81` `.expect("agent is registered")` — an `expect` on a runtime lookup that
  panics the whole run for an unregistered role id (also the ~line-81 note in the task brief).

## Verified non-findings

- `chat_stream_resumable` (`src/llm/stream.rs:576-690`) does **not** perform XML rescue, so the rescues at
  `execution.rs:240` and `fix_loop.rs:400` are live and do not double-rescue.

---

## Fix list (short, in priority order)

1. H1/H4 — make missing validation config *require* validation (fail closed) in `execution.rs:126-141` + `validation.rs:181-196`.
2. H6/H5 — default unknown verdicts to rejected; gate `leave_verdict` to validator role; return `Exhausted` instead of "assumed approved" (update `test_validation_loop.rs:586`).
3. H2/H3 — stop appending `MISSION COMPLETE` over `FAILED`; revoke markers case-insensitively via `MissionMarker::parse`; collapse the 4 duplicate marker checks.
4. M1/M6/M7 — single tool-permission source of truth for archetype/registry/brief/blueprint; fix debugger/researcher/validator mismatches and the `pty__*` duplicate.
5. M5 — `grep_search` rescued arg key `query` → `pattern` in `xml.rs:157` and `formatting.rs:28`/`:103`; skip rescue inside fenced code.
6. M4 — prefix repetition interventions with `ERROR: ` / typed `Blocked` so blocked calls are not counted as executed.
7. M8/M9 — re-arm `repetition_fired` per turn, feed turn-local text, tail-window scoring, fix markdown/code-word heuristics.
8. M3 — rolling tail window for repetition detection (kill O(n²)).
9. M2/M7b — add turn/deadline budget to `LoopParams`; reset nudge counter on tool progress.
10. Housekeeping — delete dead `Exhausted`/`dispatch_verdict_tools`/`common.rs` helpers; replace `execution.rs:81` `expect` with a graceful `Failed("agent not registered")`.
