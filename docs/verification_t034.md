# t-034 — FINAL re-gate of the tree (read-only except scoped rustfmt)

**Date of run:** same session, all commands executed against the working tree at
`/home/fredrik/marmel/wip` (git `HEAD` = `bbfb893` on branch `1.0.0-refactor`; the t-014e…t-035
work is **uncommitted**, so `HEAD` is *not* the shipped tree).
**Toolchain:** `cargo 1.98.0 (797e8a9bc)`, `rustc 1.98.0 (88d9e12ae)`, `rustfmt 1.9.0-stable`,
edition `2024` (read from `Cargo.toml:[package] edition = "2024"`).
**Edits made:** none to semantics. Only the scoped `rustfmt --edition 2024` pass of §3.
No `git commit`. No repo-wide `cargo fmt` (only the read-only `cargo fmt --check` for evidence).

Raw logs: `.marmel/t034_logs/01_check.log … 18_cargo_fmt_check.txt`, plus md5 before/after
`.marmel/t034_logs/md5_before.txt` / `md5_after.txt` and byte backups in `.marmel/t034_backup/`.

---

## 1. Gate table — command → exit status → passed / failed / ignored

### 1.1 Steps 1–8 (executed before the rustfmt pass, in the required order)

| # | Command | Exit | passed | failed | ignored | Expectation match |
|---|---|---|---|---|---|---|
| 1 | `cargo check --all-targets` | **0** | n/a | n/a | n/a | PASS (0 warnings, 0 errors) |
| 2 | `cargo clippy --all-targets --all-features -- -D warnings` | **0** | n/a | n/a | n/a | PASS (0 diagnostics) |
| 3a | `cargo test --test test_session_restore_panel_abort -- --test-threads=1` (run 1) | **0** | 10 | 0 | 0 | matches 10/0/0 |
| 3b | `cargo test --test test_session_restore_panel_abort -- --test-threads=1` (run 2) | **0** | 10 | 0 | 0 | matches 10/0/0 |
| 4 | `cargo test --test test_session_restore_ui` | **0** | 9 | 0 | 0 | matches 9/0/0 |
| 5 | `cargo test --test test_tool_name_literals` | **0** | 1 | 0 | 0 | matches 1/0/0 |
| 6a | `cargo test --test test_session_config` | **0** | 35 | 0 | 0 | matches 35/0/0 |
| 6b | `cargo test --test test_session_config -- --test-threads=1` | **0** | 35 | 0 | 0 | matches 35/0/0 |
| 7a | `cargo test --lib -- session::` | **0** | 66 | 0 | 0 | matches 66 (882 filtered out) |
| 7b | `cargo test --lib -- ui::` | **0** | 196 | 0 | 0 | matches 196 (752 filtered out) |
| 7c | `cargo test --lib -- manager::` | **0** | 76 | 0 | 0 | 76 (872 filtered out) |
| 7d | `cargo test --lib -- agents::` | **0** | 60 | 0 | 0 | matches ~60 (888 filtered out) |
| 8a | `cargo test --test test_session_bundle` | **0** | 22 | 0 | 0 | matches 22 |
| 8b | `cargo test --test test_session_boot` | **0** | 6 | 0 | 0 | matches 6 |
| 8c | `cargo test --test test_session_commit` | **0** | 9 | 0 | 0 | matches 9 |
| 8d | `cargo test --test test_session_abort_flush` | **0** | 6 | 0 | 0 | matches 6 |
| 8e | `cargo test --test test_session_restore_manager` | **0** | 8 | 0 | 0 | matches 8 |
| 8f | `cargo test --test test_session_restore_dangling` | **0** | 12 | 0 | 0 | matches 12 |
| 8g | `cargo test --test test_session_restore_workers` | **0** | 8 | 0 | 0 | matches 8 |
| 8h | `cargo test --test test_ui_session` | **0** | 21 | 0 | 0 | matches 21 |
| 9a | `cargo test` (default profile, plain = fail-fast) | **101** | 943 | 5 | 0 | fail-fast stops after `--lib`; integration targets not reached |
| 9b | `cargo test --no-fail-fast` (whole tree, pre-format) | **101** | **1218** | **6** | 0 | 29 targets; 2 targets failed (`--lib`, `--test test_role_gating`) |

Lib target detail in 9b: `944 passed; 4 failed` → only the 4 `harness::pty::tests::*`.
`test_role_gating` in 9b: `23 passed; 2 failed`.
(The 5th failure in 9a was the tracing-race flake
`orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation`; it passed
in 9b — see §4.)

### 1.2 Step 10 — post-format re-gate (proves formatting changed nothing)

| # | Command | Exit | passed | failed | ignored | vs. pre-format |
|---|---|---|---|---|---|---|
| 10.1 | `cargo check --all-targets` | **0** | n/a | n/a | n/a | identical (clean) |
| 10.2 | `cargo clippy --all-targets --all-features -- -D warnings` | **0** | n/a | n/a | n/a | identical (0 diagnostics) |
| 10.3 | `cargo test --test test_session_restore_panel_abort -- --test-threads=1` | **0** | 10 | 0 | 0 | identical |
| 10.4 | `cargo test --test test_session_config` | **0** | 35 | 0 | 0 | identical |
| 10.5 | `cargo test --lib -- ui::` | **0** | 196 | 0 | 0 | identical |
| 10.6* | `cargo test --test test_tool_name_literals` | **0** | 1 | 0 | 0 | identical (extra evidence) |
| 10.7* | `cargo test --lib -- session::` | **0** | 66 | 0 | 0 | identical (extra evidence) |
| 10.8* | `cargo test --lib -- manager::` | **0** | 76 | 0 | 0 | identical (extra evidence) |
| 10.9* | `cargo test --lib -- agents::` | **0** | 60 | 0 | 0 | identical (extra evidence) |
| 10.10* | `cargo test --test test_role_gating` | **101** | 23 | 2 | 0 | identical pair of failures (extra evidence) |
| 10.11* | `cargo test --no-fail-fast` (whole tree, **post-format = shipped tree**) | **101** | **1216** | **8** | 0 | 8 = 4 pty + 2 notice flakes + 2 role_gating (see §4) |
| 10.12* | `cargo test` (plain, post-format) | **101** | 943 | 5 | 0 | fail-fast, stops at `--lib` |

\* additional confirmations beyond the required five; the required five are 10.1–10.5.

Post-format whole-tree per-target tally (29 targets, the numbers that describe the shipped tree):

```
src/lib.rs 942/6 FAILED   src/main.rs 14/0   test_agent 2/0   test_context 8/0   test_dynamic_prompts 4/0
test_harness 15/0  test_llm 5/0  test_midturn_notice 3/0  test_monitor 2/0  test_orchestrator 8/0
test_plan_checkoff 6/0  test_role_gating 23/2 FAILED  test_session_abort_flush 6/0  test_session_boot 6/0
test_session_bundle 22/0  test_session_commit 9/0  test_session_config 35/0  test_session_restore_dangling 12/0
test_session_restore_manager 8/0  test_session_restore_panel_abort 10/0  test_session_restore_ui 9/0
test_session_restore_workers 8/0  test_specialist_schema_charge 4/0  test_specialist_stream 10/0
test_steer_history 9/0  test_tool_name_literals 1/0  test_ui_session 21/0  test_validation_loop 14/0
Doc-tests 0/0        TOTAL: 1216 passed / 8 failed / 0 ignored
```

---

## 2. Expected-vs-actual for every gate in the mission brief

| Brief expectation | Actual | Status |
|---|---|---|
| panel_abort 10/0/0 (×2) | 10/0/0, 10/0/0 | OK |
| restore_ui 9/0/0 | 9/0/0 | OK |
| tool_name_literals 1/0/0 | 1/0/0 | OK |
| session_config 35/0/0 (+serial) | 35/0/0, 35/0/0 | OK |
| lib session:: 66 | 66 | OK |
| lib ui:: 196 | 196 | OK |
| lib agents:: ~60 | 60 | OK |
| bundle 22 / boot 6 / commit 9 / abort_flush 6 / restore_manager 8 / dangling 12 / workers 8 / ui_session 21 | 22 / 6 / 9 / 6 / 8 / 12 / 8 / 21 | OK |
| `lib manager::` (no number given) | 76 | recorded |

Nothing that was expected to pass failed. Two targets are red: `--lib` and `--test test_role_gating`.

---

## 3. rustfmt (step 10) — exactly the listed files, edition 2024

Command actually used (read of `Cargo.toml` first: `edition = "2024"`):

```
rustfmt --edition 2024 --config skip_children=true <the 27 listed files>
rustfmt exit status: 0
```

**Why `skip_children=true`:** plain `rustfmt --edition 2024 src/ui/tui/mod.rs` follows the
`mod tests;` declaration and *also* rewrites `src/ui/tui/tests.rs`, which is **not** on the
allowed list (verified: with default child-following the check output contained
`src/ui/tui/tests.rs`, 18 hunks; with `skip_children=true` it does not). Likewise
`src/main.rs`/`src/config.rs`/`src/session/commit.rs`/`src/session/restore.rs`/`src/tool_names.rs`
carry inline or `mod` children, so module-following risks pulling in files outside the mission
scope. `skip_children=true` keeps the blast radius exactly on the 27 listed files, honouring
"never format the whole repo". `cargo fmt` was never run in write mode.

### 3.1 Files rustfmt actually reformatted — **13**

| File | changed lines (±) | size |
|---|---|---|
| `src/session/restore.rs` | 17 | 2681 |
| `src/ui/session.rs` | 119 | 4138 |
| `src/ui/session_tests.rs` | 236 | 3477 |
| `src/ui/tui/mod.rs` | 41 | 1649 |
| `src/config.rs` | 28 | 1672 |
| `src/main.rs` | 9 | 616 |
| `tests/test_session_boot.rs` | 31 | 507 |
| `tests/test_session_bundle.rs` | 172 | 1138 |
| `tests/test_session_commit.rs` | 49 | 665 |
| `tests/test_session_restore_dangling.rs` | 137 | 1868 |
| `tests/test_session_restore_panel_abort.rs` | 160 | 2738 |
| `tests/test_session_restore_ui.rs` | 59 | 1550 |
| `tests/test_ui_session.rs` | 3 | 2957 |

All 13 diffs were whitespace/line-breaking only (rustfmt `Diff in …` hunks; no string literal,
identifier, or statement content changed) — confirmed by the byte-identical test outcomes in §1.2.

### 3.2 Listed files already rustfmt-clean — **14** (no diff produced)

`src/session/commit.rs`, `src/session/worker_journal.rs`, `src/session/time.rs`,
`src/session/format/ui.rs`, `src/session/format/worker.rs`, `src/session/format/restore.rs`,
`src/orchestrator/notice.rs`, `src/agents/runner/fix_loop.rs`, `src/tool_names.rs`,
`tests/test_session_abort_flush.rs`, `tests/test_session_config.rs`,
`tests/test_session_restore_manager.rs`, `tests/test_session_restore_workers.rs`,
`tests/test_tool_name_literals.rs`.

### 3.3 Scope audit

* 27 files hashed before and after; exactly 13 hashes changed (§3.1).
* `git status --short` file set is unchanged by the format pass (no new/unexpected paths).
* `src/ui/tui/tests.rs` was **not** touched (mtime unchanged: 04:14:54 vs. 23:46 for the pass).
* Read-only `cargo fmt --check` after the pass: remaining repo-wide diffs are confined to
  **2 files** — `src/ui/raw.rs` (7 hunks) and `src/ui/tui/tests.rs` (18 hunks), 25 hunks /
  29 lines total. Neither is on the allowed list, so they stay. Note for the record: the brief's
  figure of "138 pre-existing repo-wide diffs" does not match today's tree (25 hunks in 2 files);
  earlier tasks evidently already formatted the rest. No file in the §3.1/§3.2 list contributes
  any remaining diff.

---

## 4. Failure classification (whole tree) — BASELINE vs NEW, with isolation verdict

Every failure below was re-run in isolation (`--exact`) as required.

| Failing test | Where seen | Assertion output (verbatim) | Isolation re-run | Classification | Verdict |
|---|---|---|---|---|---|
| `harness::pty::tests::test_harness_pty_process_group_kill` | `--lib` (both runs) | `src/harness/pty_tests.rs:16:47` — `spawn sandbox: Execution(failed to openpty: Os { code: 13, kind: PermissionDenied, message: "Permission denied" })` | **FAILS** in isolation (exit 101, 0.00s) | **BASELINE** | Real but environmental: sandbox denies `openpty`. Deterministic, not a code regression. |
| `harness::pty::tests::test_interactive_pty_manager_lifecycle` | `--lib` | `src/harness/pty_tests.rs:120:10` — `spawn session: Execution(Failed to create PTY: failed to openpty: … PermissionDenied)` | **FAILS** in isolation | **BASELINE** | Same environment cause. |
| `harness::pty::tests::test_run_command_parses_timeout_seconds_argument` | `--lib` | `src/harness/pty_tests.rs:102:34` — `executes with custom timeout: Execution(failed to openpty: … PermissionDenied)` | **FAILS** in isolation | **BASELINE** | Same environment cause. |
| `harness::pty::tests::test_run_command_pty_cancelled_by_worker_token` | `--lib` | `src/harness/pty_tests.rs:179:5` — `assertion failed: res.is_ok()` (the `res` is the same openpty `PermissionDenied` error) | **FAILS** in isolation | **BASELINE** | Same environment cause (assertion is downstream of the PTY spawn). |
| `orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation` | plain `cargo test` (pre + post); **passed** in both `--no-fail-fast` runs | `src/orchestrator/notice_tests.rs:457:10` — `Event::SteerResponse should be emitted: Disconnected` | **PASSES 3/3** in isolation | **BASELINE** (listed tracing-race flake) | Flake under parallel load only. |
| `orchestrator::notice_tests::test_task_id_address_matches_exact_task_id_only` | post-format `--no-fail-fast` only | `src/orchestrator/notice_tests.rs:197:5` — `assertion failed: drain_worker_notices("coder-t-0010").is_empty()` | **PASSES 3/3** in isolation | **BASELINE** (listed tracing-race flake) | Flake under parallel load only. |
| `test_role_gating::test_prompt_based_tool_gating` | `test_role_gating` (pre + post) | `tests/test_role_gating.rs:312:5` — `Validator with run_command in prompt must be permitted to execute run_command` | **FAILS** in isolation | **BASELINE** | Pre-existing deterministic failure, documented since `docs/baseline_test_run.md:151,180,198,222`. |
| `test_role_gating::fix_loop_source_has_no_raw_tool_name_string_literals` | `test_role_gating` (pre + post) | `tests/test_role_gating.rs:1466:5` — ``src/agents/runner/fix_loop.rs must not contain raw tool-name string literals; use the constants from `marmennill::tool_names`. Offending literals: ["(rebirth advisory)", "(rebirth advisory)"]`` | **FAILS** in isolation (deterministic) | **NEW** | **Real, deterministic, must be reported (not fixed here — read-only mandate).** |
| `ui::tests::test_ui_run_session_*` load flakes | not observed | n/a — `--lib ui::` was 196/0/0 in both filtered runs, and no `ui::` test appears in any whole-tree failure list | n/a | none seen | No flake to classify this run. |
| Other listed flake candidates (`orchestrator::steer_tests::test_unknown_steer_subtask_action_rejection_is_logged`, `orchestrator::workers::tests::test_completion_leaves_no_stale_registry_entry`, `harness::dispatch_integrity_tests::test_reply_without_notice_id_is_rejected_and_no_notice_is_guessed`, `ui::bridge::tests::test_bridge_notice_capacity_drop_is_observable_and_inbox_stays_bounded`) | did **not** fail in either whole-tree run | n/a | not needed | BASELINE-registered, did not fire | No action. |
| `test_role_gating::fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority` (the "+1 in that file" previously documented, `docs/verification_session_restore.md:160`) | did not fail | n/a | **PASSES** in isolation | previously-flaky, now green | Superseded: the second role_gating failure is now a *different* test (see row above). |

### 4.1 The NEW failure — full report (no fix applied)

* **Test:** `fix_loop_source_has_no_raw_tool_name_string_literals` (`tests/test_role_gating.rs:1423`,
  assert site `:1466`).
* **Guard semantics:** the test reads `src/agents/runner/fix_loop.rs`, extracts every *string
  literal* in the file, and flags any literal that **contains** a tool name — needles come from
  `marmennill::tool_names::*`, including `TOOL_REBIRTH` = `rebirth`. The guard is deliberately
  stricter than `tests/test_tool_name_literals.rs`: a tool name *inside* a longer literal trips it.
* **Offenders found:** two occurrences of the literal `"(rebirth advisory)"` —
  **`src/agents/runner/fix_loop.rs:1355`** and **`src/agents/runner/fix_loop.rs:1402`**, both inside
  the in-file `mod tests` (`mod tests {` at `src/agents/runner/fix_loop.rs:1299`), passed as the
  advisory/note argument to a mirrored tool-call message (`inject_worker_notices_at_turn_start`
  and the unscoped-mutation mirror assertions).
* **Evidence it is NEW, not baseline:**
  * `git show HEAD:src/agents/runner/fix_loop.rs | grep "rebirth advisory"` → **no match** at
    `HEAD` (`bbfb893`); the literals exist only in the working tree (i.e. they arrived with the
    uncommitted t-0xx mission work, and `src/agents/runner/fix_loop.rs` is on the mission's own
    touched-file list).
  * The documented "+1 more in that file" (`docs/verification_session_restore.md:160`) named
    `fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority`, which **passes** now.
  * Historically this guard passed: `docs/t007_comment_cleanup_report.md:141,223,349,381` records
    `test_role_gating fix_loop_source_has_no_raw_tool_name_string_literals` → exit 0 / 1 passed.
* **Severity:** cosmetic-in-intent but gate-breaking: it fails deterministically in isolation and
  in every whole-tree run, so it is not a flake and cannot be waved through. It is confined to
  test-module string literals; the fix is a 2-site rename of the advisory literal (e.g. to a
  wording that does not embed the tool name `rebirth`, or reference
  `marmennill::tool_names::TOOL_REBIRTH` via `format!`). Not performed here: t-034 is read-only.
* **Formatting independence:** `src/agents/runner/fix_loop.rs` was rustfmt-clean, and the same two
  failures appear before and after the format pass, so formatting neither caused nor masks it.

---

## 5. VERDICT

**The tree is shippable with one flagged exception.**

* All 20 command invocations of steps 1–8 are **green and numerically exact** (check clean, clippy clean
  under `-D warnings`, panel_abort 10/0/0 twice, restore_ui 9/0/0, tool_name_literals 1/0/0,
  session_config 35/0/0 parallel and serial, lib `session::` 66, `ui::` 196, `manager::` 76,
  `agents::` 60, and the full session sweep 22/6/9/6/8/12/8/21).
* The scoped rustfmt pass (13 of 27 listed files reformatted, whitespace-only) changed **nothing
  observable**: post-format `cargo check` exit 0, `cargo clippy … -D warnings` exit 0,
  panel_abort 10/0/0, session_config 35/0/0, `--lib ui::` 196/0/0.
* **Exact remaining known-failure count: 8** in one parallel whole-tree run
  (`cargo test --no-fail-fast`, post-format: 1216 passed / 8 failed / 0 ignored):
  * **7 BASELINE** — 4 × `harness::pty::tests::*` (sandbox `openpty` → `PermissionDenied`, fail in
    isolation, environment-only), 2 × `orchestrator::notice_tests::*` tracing-race flakes
    (`test_handle_reply_to_arbitrator_success_and_validation`,
    `test_task_id_address_matches_exact_task_id_only`; both pass 3/3 in isolation), and
    `test_role_gating::test_prompt_based_tool_gating` (`tests/test_role_gating.rs:312`,
    deterministic, documented baseline).
  * **1 NEW** — `test_role_gating::fix_loop_source_has_no_raw_tool_name_string_literals`
    (`tests/test_role_gating.rs:1466`), deterministic in isolation, caused by the two
    `"(rebirth advisory)"` literals at `src/agents/runner/fix_loop.rs:1355` and `:1402`.
  * If the 4 PTY sandbox failures are excluded as environment-forced (they fail identically at
    `HEAD` and never touch session code), the shippable-tree failure count is **4**, of which **3
    are baseline** and **1 is NEW**.
  * Flake-count note for reproducibility: the `--lib` failure count moved 4 → 5 → 6 → 5 across the
    four whole-tree runs (pre/post × plain/no-fail-fast) purely because of the two registered
    notice flakes; the deterministic set (4 pty + `test_prompt_based_tool_gating` +
    `fix_loop_source_has_no_raw_tool_name_string_literals`) is stable at **6**.
* **Action required before the tree can be called failure-free** (outside t-034 scope): rename the
  two `"(rebirth advisory)"` literals in the `src/agents/runner/fix_loop.rs` test module so they no
  longer embed the tool name `rebirth`. Nothing else in the tree blocks shipping.

No commit was made. No non-listed file was formatted. No test assertion or source semantics were
edited.
