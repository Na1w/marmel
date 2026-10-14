# t-037 — kill the deterministic `fix_loop_source_has_no_raw_tool_name_string_literals` failure

**Scope honoured:** only `src/agents/runner/fix_loop.rs` was edited (two string literals inside its
`mod tests`), plus this note. `tests/**`, `src/tool_names.rs` and every other source file are
untouched. No `git commit`, no repo-wide `cargo fmt`.

---

## 1. The guard mechanism (read from `tests/test_role_gating.rs`, not modified)

`fix_loop_source_has_no_raw_tool_name_string_literals` — `tests/test_role_gating.rs:1423` (test body
runs to `:1477`; the failure assertion that fired is `:1466`).

How it works, point by point:

1. **Scanned file:** exactly one path, `src/agents/runner/fix_loop.rs`, read with
   `std::fs::read_to_string` — the *whole* file, production code **and** its `mod tests`.
2. **Needle enumeration:** the needles are built at runtime from the crate's own constants, so the
   test file's own text can never satisfy them:
   * a hard-coded `bare` array of 19 constant values
     (`TOOL_DELEGATE_TASK … TOOL_REPLY_TO_ARBITRATOR`, plus `TOOL_LIST_DIRECTORY`);
   * plus, for each of those, `marmennill::tool_names::terminal_tool(name)`
     → `"terminal__" + name`.
   So the effective needle set = bare names ∪ their `terminal__` variants.
3. **Literal extraction:** `string_literals_in_source(&src)` (`tests/test_role_gating.rs:1482`) walks
   the chars and collects *double-quoted string literals only*, skipping `//` line comments
   (hence `//` and `///`/`//!` doc comments are transparent), nested `/* */` block comments, and
   `'…'` char literals / lifetimes. Comments therefore never trip the guard; only literals do.
4. **Match rule — substring, not exact value, case-sensitive:**
   `literals.iter().filter(|lit| needles.iter().any(|needle| lit.contains(needle)))`.
   Stricter than the repo-wide `tests/test_tool_name_literals.rs` guard: a tool name *embedded
   inside* a longer literal is flagged. That is why `"(rebirth advisory)"` — an inert test fixture
   whose text merely contains `rebirth` — was reported.
5. **Reporting:** the failing message dumps the offending literals themselves
   (`Offending literals: {offenders:?}`); no line numbers or surrounding context are reported —
   only the literal strings.
6. **Anti-vacuity clause** (`:1431` and `:1473`): the guard first asserts the extracted literal list
   is non-empty, then asserts `src.contains("tool_names::")`, so deleting the vocabulary (or deleting
   literals) cannot make it pass.

Consequence for the fix: the only compliant move is to make the fixture sentinel text stop containing
any tool name — which is exactly what was done. Interpolated format strings such as
`format!("({TOOL_REBIRTH} checkpoint accepted)")` (lines 1650 / 2196 / 2398) were already compliant:
the literal characters are `{TOOL_REBIRTH}` (upper-case, not the value `rebirth`), and the value is
supplied by the constant.

## 2. The edits (before → after)

Both sentinels are the 5th argument of `append_tool_result(…)` (the `rebirth_notice: &str` parameter,
`src/agents/runner/fix_loop.rs:733`) inside
`mod tests::every_fix_loop_engine_mutation_is_mirrored_through_the_task_local`. In both calls the
`ToolCall` name is `TOOL_RUN_COMMAND`, i.e. not the rebirth tool, so the special-case branch
(`is_rebirth && execution_succeeded`, `:735`) is never taken and the sentinel value is never read,
never mirrored and never compared anywhere — it is a placeholder argument only.

| File:line | Before | After |
| --- | --- | --- |
| `src/agents/runner/fix_loop.rs:1355` (scoped-mirror call, `call-mirror-1`) | `"(rebirth advisory)"` | `"(context advisory)"` |
| `src/agents/runner/fix_loop.rs:1402` (unscoped call, `call-mirror-2`) | `"(rebirth advisory)"` | `"(context advisory)"` |

* Meaning preserved: the parameter is the user-level notice a context-engine advisory would inject,
  so "(context advisory)" describes the same role without spelling a registered tool name.
* No other site needed updating: `grep -n 'advisory'` over the file returns only the two construction
  sites plus the production identifier `inject_rebirth_advisory()` (`:1204`). There is **no**
  comparison/assertion against the sentinel text, so nothing else referenced it.
* No assertion weakened, deleted or made vacuous; no `#[allow]`; both calls keep passing a
  non-empty, distinct sentinel so the parameter is still exercised.

## 3. Substring audit (general, not just `rebirth`)

Needles were enumerated mechanically from `src/tool_names.rs` itself — all 29 `pub const …: &str`
values (`delegate_task`, `read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`,
`create_plan`, `archive_current_plan`, `rebirth`, `pty_spawn`, `pty_write`, `pty_read`, `pty_close`,
`pty_list`, `leave_verdict`, `sleep`, `reply_to_arbitrator`, `list_directory`, the six
`terminal__*` variants, `terminal__list_directory`, `terminal__sleep`, `terminal__leave_verdict`, …),
plus a generated `terminal__` + value variant for every one of them → **48 needles**
(the `TERMINAL_PREFIX` value `terminal__` is a prefix, not a tool name, so it is excluded).

A literal extractor mirroring the guard's algorithm (comments / char literals / lifetimes skipped)
was run over `src/agents/runner/fix_loop.rs`:

```
tool_names consts parsed: 29
distinct tool-name VALUES: 29
needles (values + terminal__ variants): 48
string literals extracted from src/agents/runner/fix_loop.rs: 322
offending literals: 0
contains 'tool_names::': True
```

A raw `grep -E` over the whole file for all 20 vocabulary words plus `terminal__` returns hits only
in: doc/line comments, identifiers (`rebirth_notice`, `is_rebirth`, `should_advise_rebirth`,
`inject_rebirth_advisory`, `is_leave_verdict_tool`), and `tool_names::TOOL_*` constant references /
`{TOOL_*}` format interpolations. **0 raw literals contain a tool-name value.**

## 4. Gate numbers (run in the required order)

| # | Gate | Result |
| --- | --- | --- |
| 1 | `cargo test --test test_role_gating` | 24 passed / **1 failed** / 0 ignored — the single failure is `test_prompt_based_tool_gating`; `fix_loop_source_has_no_raw_tool_name_string_literals … ok` |
| 2 | `cargo test --test test_tool_name_literals` | **1 / 0 / 0** (`test_no_unexpected_raw_tool_name_literals_in_src … ok`) |
| 3 | `cargo test --lib agents::` | **60 / 0 / 0** (888 filtered out; includes `…fix_loop::tests::every_fix_loop_engine_mutation_is_mirrored_through_the_task_local … ok`) |
| 4 | `cargo test --test test_session_restore_panel_abort -- --test-threads=1` | **10 / 0 / 0** |
| 5 | `cargo clippy --all-targets --all-features -- -D warnings` | **exit 0** (no diagnostics) |
| 6 | `rustfmt --edition 2024 --config skip_children=true src/agents/runner/fix_loop.rs` | exit 0; a follow-up `--check` run re-validates the file (exit 0 → already formatted, no churn) |

### Baseline failure (allowed, pre-existing, not touched)

`test_prompt_based_tool_gating` (`tests/test_role_gating.rs:312:5`,
`Validator with run_command in prompt must be permitted to execute run_command`) is the sanctioned
baseline failure recorded in `docs/baseline_test_run.md:151` / `:180` / `:198` (**"STILL FAILS at
baseline"**, deterministic in isolation and in every run, including a HEAD-level baseline run
recorded there). It is also unrelated to this edit by construction: it exercises
`dispatch_for(&tool, ToolCaller::SpecialistWithTools { … })` in the harness dispatcher, while the
only thing changed here is two inert fixture literals in a `mod tests` block of the fix-loop driver —
nothing in the dispatcher, the blueprint or the role gate was touched. It therefore fails identically
before and after this change (24 passed / 1 failed both ways for the target-level tally; the count
that changed is the one this task was about: `fix_loop_source_has_no_raw_tool_name_string_literals`
FAIL → **ok**).

**Did this failure exist at HEAD `bbfb893`?** No — the two offending literals were introduced by
earlier mission work in this working tree, which is why the guard passed at HEAD and fails now; this
note restores the HEAD-level green state for that guard without editing the guard.
