# PRE-CHANGE Verification Baseline — `marmennill` (marmel)

Read-only measurement run. **No source file was created, modified, or fixed** other than this
report file itself (`docs/baseline_test_run.md`). All measurements below were executed on disk.

## 0. Provenance / environment

| Item | Value |
|---|---|
| Workspace (CWD) | `/home/fredrik/marmel/wip` |
| Package | `marmennill v1.0.0`, edition 2024, `default-run = marmel` |
| Toolchain | `cargo 1.98.0 (797e8a9bc 2026-08-05)`, `rustc 1.98.0 (88d9e12ae 2026-08-18)` |
| HEAD (measurement window) | started at `31f4e90` (branch `refactor`/`1.0.0-refactor`, == `main` / `origin/main`); **during the run a concurrent process committed the worktree as `974b601 "Refactor part 1"`** (reflog `HEAD@{0}`) |
| Worktree state at baseline | **dirty**: ~78 modified tracked files, 8 tracked deletions, ~14 untracked new files (`git status --porcelain`). Post-commit check: `git diff HEAD -- src tests` is **empty** and no `src`/`tests` file mtime changed ⇒ the measured code bytes are identical to commit `974b601`; the baseline therefore pins to that tree |
| Baseline content pin | `974b601` (== the worktree contents measured here). Only remaining untracked entries after the commit: `.marmel.old/`, `docs/`, `marmel.toml.df` |
| Build cache | `target/` ≈ 11 GB, warm (deps pre-built; lib + all 16 integration test binaries present) |
| Wall clock (total suite) | `cargo test` aborted at lib in **18 s**; full coverage via `--no-fail-fast` in **78 s** — well under the 10-minute threshold |
| Raw logs | `target/baseline_logs/` (gitignored; `build.log`, `fmt.log`, `clippy.log`, `test.log`, `test_all.log`, `lib_r1..3.log`, `lib_serial.log`, `role_gating.log`, `clippy_forced*.log`) |

## 1. Exact commands used

```bash
cargo build 2>&1 | tail -40
cargo fmt --check 2>&1 | tail -20
cargo clippy --all-targets -- -D warnings 2>&1 | tail -60
cargo test 2>&1 | tail -120
```

Supplementary (needed because step 4 aborted early and because of observed flakiness; all read-only):

```bash
cargo test --no-fail-fast                                   # full coverage: cargo test stops at first failing target
cargo test --lib                                            # repeated x3 (flakiness probe)
cargo test --lib -- --test-threads=1                        # serialised probe (84.18 s)
cargo test --lib -- --exact <test_path>                     # per-test isolation probe (8 suspects)
cargo test --test test_role_gating                          # per-target integration probe
cargo clippy --lib --bins -- -D warnings -C debuginfo=0      # cache-busting corroboration
cargo clippy --all-targets -- -D warnings -C debuginfo=0     # cache-busting corroboration (all targets)
ls -l /dev/pts/ptmx                                          # verify sanctioned PTY failure cause
for d in src/agents src/harness src/llm src/net src/mcp src/manager src/orchestrator src/ui src/types tests; do printf "%s %s\n" "$d" "$(grep -rE '^\s*(//|///|//!)' $d | wc -l)"; done
for f in src/*.rs; do printf "%s %s\n" "$f" "$(grep -E '^\s*(//|///|//!)' $f | wc -l)"; done
```

## 2. Compile status

```
$ cargo build 2>&1 | tail -40
   Compiling marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.96s
```

**`cargo build` → exit 0 · SUCCESS · 0 warnings · 0 errors** (3.96 s, warm cache).

## 3. `cargo fmt --check` status

```
$ cargo fmt --check 2>&1 | tail -20
(no output)
```

**exit 0 · 0 lines of diff output · 0 files needing reformat → FMT: CLEAN.**

## 4. `cargo clippy --all-targets -- -D warnings` diagnostic count

```
$ cargo clippy --all-targets -- -D warnings 2>&1 | tail -60
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.26s
```

**exit 0 · 0 diagnostics** (clean under `-D warnings`). The run was a cache replay, so it was
re-verified with a flag-changed (cache-busting) pass that forced real re-analysis:

```
$ cargo clippy --lib --bins    -- -D warnings -C debuginfo=0   → exit 0, 11.26 s, 0 warnings, 0 errors ("Checking marmennill")
$ cargo clippy --all-targets   -- -D warnings -C debuginfo=0   → exit 0, 17.70 s, 0 warnings, 0 errors ("Checking marmennill")
$ cargo clippy --all-targets   -- -D warnings                 → exit 0, 16.22 s, re-analysis ("Checking marmennill"), 0 diagnostics  (end-of-session re-check)
```

**CLIPPY: 0 diagnostics** (confirmed independently of the cargo cache).

## 5. `cargo test` — full suite

`cargo test` (as specified) **aborted after the lib target** and never reached the integration
suites, because cargo fail-fasts on the first failing target:

```
$ cargo test 2>&1 | tail -120
...
test result: FAILED. 802 passed; 9 failed; 0 ignored; 0 measured; 0 filtered out; finished in 16.66s
error: test failed, to rerun pass `--lib`
```
exit **101**, wall clock **18 s**.

To obtain the complete per-target picture required by this deliverable, the identical suite was
re-run with `--no-fail-fast` (test selection unchanged, only the abort policy):

```
$ cargo test --no-fail-fast      → exit 101, wall clock 78 s (1330 lines of output)
```

### 5.1 Per-target tallies (canonical `--no-fail-fast` run)

| Target | Passed | Failed | Ignored | Time |
|---|---:|---:|---:|---:|
| `unittests src/lib.rs` | 801 | **10** | 0 | 16.90 s |
| `unittests src/main.rs` (bin `marmel`) | 8 | 0 | 0 | 0.00 s |
| `tests/test_agent.rs` | 2 | 0 | 0 | 0.01 s |
| `tests/test_context.rs` | 8 | 0 | 0 | 0.48 s |
| `tests/test_dynamic_prompts.rs` | 4 | 0 | 0 | 0.08 s |
| `tests/test_harness.rs` | 15 | 0 | 0 | 2.12 s |
| `tests/test_llm.rs` | 5 | 0 | 0 | 1.50 s |
| `tests/test_midturn_notice.rs` | 3 | 0 | 0 | 1.17 s |
| `tests/test_monitor.rs` | 2 | 0 | 0 | 0.00 s |
| `tests/test_orchestrator.rs` | 8 | 0 | 0 | 0.75 s |
| `tests/test_plan_checkoff.rs` | 6 | 0 | 0 | 0.04 s |
| `tests/test_role_gating.rs` | 23 | **2** | 0 | 0.72 s |
| `tests/test_specialist_schema_charge.rs` | 4 | 0 | 0 | 1.28 s |
| `tests/test_specialist_stream.rs` | 10 | 0 | 0 | 2.73 s |
| `tests/test_steer_history.rs` | 9 | 0 | 0 | 0.00 s |
| `tests/test_tool_name_literals.rs` | 1 | 0 | 0 | 0.17 s |
| `tests/test_ui_session.rs` | 21 | 0 | 0 | 47.91 s |
| `tests/test_validation_loop.rs` | 14 | 0 | 0 | 0.69 s |
| `Doc-tests marmennill` | 0 | 0 | 0 | 0.00 s |
| **TOTAL** | **944** | **12** | **0** | **78 s** |

- **lib target total: 811 tests** (constant across runs: 801+10, 802+9, 804+7, 806+5).
- **integration total: 137 tests** (135 passed + 2 failed).
- **doc-tests: 0** (no doctests exist).
- `grep -rcE '#\[(tokio::)?test' src tests` → **967 declarations**; 948 executed
  (lib 811 + bin 8 + integration 129 binaries-worth 137 — see caveat: the grep also counts
  nested helper `fn test_*` items, e.g. `fn test_calc` ×2 and `fn test_ok` inside
  `tests/test_validation_loop.rs`, which declares 17 but executes 14).

### 5.2 Every failing test, by name (canonical run)

**lib (`unittests src/lib.rs`) — 10 failures**

1. `harness::pty::tests::test_harness_pty_process_group_kill` — `src/harness/pty_tests.rs:16:47` — `Failed to create PTY: failed to openpty: Os { code: 13, kind: PermissionDenied }`
2. `harness::pty::tests::test_interactive_pty_manager_lifecycle` — `src/harness/pty_tests.rs:121:10` — same `openpty` EACCES
3. `harness::pty::tests::test_run_command_parses_timeout_seconds_argument` — `src/harness/pty_tests.rs:102:34` — same `openpty` EACCES
4. `harness::pty::tests::test_run_command_pty_cancelled_by_worker_token` — `src/harness/pty_tests.rs:183:5` — `assertion failed: res.is_ok()`
5. `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable` — `src/harness/mod.rs:1273:9` — `assertion failed: !res.is_error`
6. `harness::tool_resolution_tests::specialist_allowlists_admit_alias_spellings_of_the_granted_tool` — `src/harness/pty.rs:1170:9` — `there is no reactor running, must be called from the context of a Tokio 1.x runtime`
7. `harness::tool_resolution_tests::aliases_that_resolved_before_still_reach_a_builtin_handler` — `library/std/src/sync/lazy_lock.rs:415:5` — `LazyLock instance has previously been poisoned` (cascade of #6)
8. `orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation`
9. `orchestrator::notice_tests::test_steering_history_multiple_notices_resolved_out_of_order` — `src/orchestrator/notice_tests.rs:681:9` — `assertion failed: hist[0].1.contains("Coder har fixat PPC JIT.")`
10. `tool_names::tests::claimed_aliases_never_fall_through_to_unknown_tool` — `LazyLock instance has previously been poisoned` (cascade of #6)

**integration (`tests/test_role_gating.rs`) — 2 failures**

11. `test_prompt_based_tool_gating` — `tests/test_role_gating.rs:312:5` — `Validator with run_command in prompt must be permitted to execute run_command`
12. `fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority` — (only in the combined-suite run; see §5.3)

### 5.3 Determinism / flakiness probe (critical for later diffing)

Repeated `cargo test --lib` runs (same binary, no source change) produced **different failure sets**,
so the lib failure count is *not* a stable scalar:

| Run | lib result | failures |
|---|---|---|
| `cargo test` (spec command) | 802 passed / 9 failed | 4 pty + tool_resolution ×2 + `notice_tests::test_handle_reply_to_arbitrator_ask_followup` + `notice_tests::test_steering_history_multiple…` + `tool_names::claimed_aliases…` |
| `cargo test --no-fail-fast` | 801 / 10 | 4 pty + `harness::tests::test_harness_delegate_task_manager…` + tool_resolution ×2 + `notice_tests::…_success_and_validation` + `notice_tests::…multiple_notices…` + `tool_names::…` |
| repeat 1 | 802 / 9 | 4 pty + `harness::tests::test_harness_delegate…` + tool_resolution ×2 + `notice_tests::…multiple_notices…` + `tool_names::…` |
| repeat 2 | 804 / 7 | 4 pty + `harness::tests::test_harness_delegate…` + `notice_tests::…_success_and_validation` + `notice_tests::…multiple_notices…` |
| repeat 3 | 801 / 10 | 4 pty + `harness::tests::test_harness_delegate…` + tool_resolution ×2 + `notice_tests::…_success_and_validation` + `tool_names::…` + **`ui::bridge::tests::test_bridge_notice_capacity_drop_is_observable_and_inbox_stays_bounded`** (seen once) |
| `--test-threads=1` (serial) | **806 / 5** | 4 pty + `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable` |

Isolation probes (`cargo test --lib -- --exact <name>`, run twice each where noted):

| Test | Isolated verdict |
|---|---|
| `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable` | **FAILS deterministically** (2/2 isolated, fails serial, fails parallel) → real, reproducible defect |
| `harness::tool_resolution_tests::specialist_allowlists_admit_alias_spellings_of_the_granted_tool` | **FAILS deterministically in isolation** (2/2; missing Tokio reactor at `src/harness/pty.rs:1170`) but **passes** in the serial full-lib run |
| `harness::tool_resolution_tests::aliases_that_resolved_before_still_reach_a_builtin_handler` | passes isolated → flaky (poisoning cascade) |
| `orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation` | passes isolated → flaky |
| `orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup` | passes isolated → flaky |
| `orchestrator::notice_tests::test_steering_history_multiple_notices_resolved_out_of_order` | passes isolated → flaky |
| `tool_names::tests::claimed_aliases_never_fall_through_to_unknown_tool` | passes isolated → flaky (cascade of the LazyLock poisoning) |
| `ui::bridge::tests::test_bridge_notice_capacity_drop_is_observable_and_inbox_stays_bounded` | passes isolated → flaky (observed 1/4 runs) |
| `test_prompt_based_tool_gating` (`test_role_gating`) | **FAILS deterministically** (fails in every run, incl. isolated per-target run) |
| `fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority` (`test_role_gating`) | **PASSES isolated** (24 passed / 1 failed) → flaky, order-dependent |

**Classification summary**

- **Deterministic, environment-caused (sanctioned):** the 4 `harness::pty::tests::*`.
- **Deterministic, real (not sanctioned, undocumented):** `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable`.
- **Deterministic-in-isolation, real (test-harness defect):** `harness::tool_resolution_tests::specialist_allowlists_admit_alias_spellings_of_the_granted_tool`.
- **Flaky (parallel-execution / shared-state dependent; pass in isolation):** the 2 remaining
  `harness::tool_resolution_tests::*`+`tool_names` poisoning cascade, the 3 `orchestrator::notice_tests::*`,
  `ui::bridge::tests::test_bridge_notice_capacity_drop…`, and
  `test_role_gating::fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority`.

## 6. Sanctioned failures (per `docs/cleanup_report.md:25`)

> `docs/cleanup_report.md:25` —
> `| Integration suites | partial | ≈136–142 passed / 1 sanctioned failure (`tests/test_role_gating.rs::test_prompt_based_tool_gating`) | gate tallies in `.marmel/execution_plan.md` |`

**CONFIRMED — `tests/test_role_gating.rs::test_prompt_based_tool_gating` STILL FAILS at baseline.**

```
$ cargo test --test test_role_gating      → exit 101
test result: FAILED. 24 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.96s
---- test_prompt_based_tool_gating stdout ----
thread 'test_prompt_based_tool_gating' panicked at tests/test_role_gating.rs:312:5:
Validator with run_command in prompt must be permitted to execute run_command
failures:
    test_prompt_based_tool_gating
```

It fails in the combined suite **and** in isolated per-target execution → deterministic, matching the
sanctioned record exactly (1 sanctioned integration failure, same name, same file).

Second sanctioned group, recorded at `docs/cleanup_report.md:222`
(`harness::pty::tests::*` ×4 — `openpty` EACCES because `/dev/pts/ptmx` is mode `000`):
**CONFIRMED STILL FAILING**, and the environment cause is verified on disk:

```
$ ls -l /dev/pts/ptmx
c--------- 1 root root 5, 2 Oct  4 19:40 /dev/pts/ptmx      # mode 000 → EACCES reproduced
```

Total sanctioned failures at baseline: **5** (4 × `harness::pty::tests::*` + 1 × `test_prompt_based_tool_gating`).
Everything else in §5.2 is **unsanctioned / not covered by `docs/cleanup_report.md`**.

## 7. Delta vs the claims in `docs/cleanup_report.md` (for audit awareness)

| Claim (`docs/cleanup_report.md`) | Measured at this baseline | Delta |
|---|---|---|
| `cargo test --lib` = 807 passed / 4 failed (all sanctioned pty) | 801–806 passed / **5–10** failed (serial: 806 / 5) | **−1 to −6 extra lib failures**; ≥2 are non-sanctioned |
| integration ≈136–142 passed / 1 sanctioned | 135 passed / **2** failed (role_gating only) | 1 extra (flaky) integration failure; sanctioned one reproduced ✓ |
| `cargo clippy -- -D warnings` = 0 diagnostics | **0 diagnostics** | ✓ matches (cache-busted re-check) |
| fmt status | not claimed | clean (exit 0) |
| test declarations 967 | 967 | ✓ matches |

## 8. Comment-count snapshot per top-level module (cleanup delta baseline)

Command (verbatim, as specified):
```bash
for d in src/agents src/harness src/llm src/net src/mcp src/manager src/orchestrator src/ui src/types tests; do printf "%s %s\n" "$d" "$(grep -rE '^\s*(//|///|//!)' $d | wc -l)"; done
```
plus the same pattern for root `src/*.rs` files.

| Module | Comment lines | `.rs` files | Total lines |
|---|---:|---:|---:|
| `src/agents` | 1175 | 15 | 7301 |
| `src/harness` | 2187 | 20 | 12352 |
| `src/llm` | 483 | 7 | 4186 |
| `src/net` | 137 | 4 | 520 |
| `src/mcp` | 617 | 5 | 3516 |
| `src/manager` | 1649 | 6 | 6391 |
| `src/orchestrator` | 1851 | 14 | 11351 |
| `src/ui` | 1530 | 18 | 15312 |
| `src/types` | 17 | 3 | 882 |
| `tests` | 1424 | 17 | 11704 |
| **subdirs subtotal** | **11070** | | |
| `src/config.rs` | 133 | | |
| `src/debug_log.rs` | 29 | | |
| `src/lib.rs` | 16 | | |
| `src/main.rs` | 50 | | |
| `src/markers.rs` | 564 | | |
| `src/plan_parse.rs` | 258 | | |
| `src/prompts.rs` | 3 | | |
| `src/task_id.rs` | 157 | | |
| `src/text_util.rs` | 38 | | |
| `src/tool_args.rs` | 202 | | |
| `src/tool_names.rs` | 214 | | |
| **root `src/*.rs` subtotal** | **1664** | 11 | 7787 |
| **GRAND TOTAL comment lines** | **12734** | | |

## 9. BASELINE (diff target for later verification)

```
BASELINE @ content pin 974b601 "Refactor part 1" (worktree measured at HEAD=31f4e90 dirty,
           then concurrently committed as 974b601; git diff HEAD -- src tests is empty), rustc 1.98.0:
  build: SUCCESS (exit 0, 0 warnings)
  fmt: clean (exit 0, 0 diffs)
  clippy --all-targets -- -D warnings: 0 diagnostics (exit 0; corroborated cache-busted)
  lib: 801 passed / 10 failed / 0 ignored (total 811; range across 4 runs 801–806 passed, 5–10 failed; serial 806/5)
  bin(marmel): 8 passed / 0 failed
  integration: 135 passed / 2 failed / 0 ignored (total 137, across 16 suites)
  doc-tests: 0
  TOTAL (cargo test --no-fail-fast): 944 passed; 12 failed; 0 ignored; wall clock 78s
  failed: [harness::pty::tests::test_harness_pty_process_group_kill,
           harness::pty::tests::test_interactive_pty_manager_lifecycle,
           harness::pty::tests::test_run_command_parses_timeout_seconds_argument,
           harness::pty::tests::test_run_command_pty_cancelled_by_worker_token,
           harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable,
           harness::tool_resolution_tests::specialist_allowlists_admit_alias_spellings_of_the_granted_tool,
           harness::tool_resolution_tests::aliases_that_resolved_before_still_reach_a_builtin_handler,
           orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation,
           orchestrator::notice_tests::test_steering_history_multiple_notices_resolved_out_of_order,
           tool_names::tests::claimed_aliases_never_fall_through_to_unknown_tool,
           test_role_gating::test_prompt_based_tool_gating,
           test_role_gating::fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority]
  sanctioned (docs/cleanup_report.md:25 + :222): 5 -> 4x harness::pty::tests::* (openpty EACCES, /dev/pts/ptmx mode 000)
                                                 + test_role_gating::test_prompt_based_tool_gating  [STILL FAILING = confirmed]
  unsanctioned deterministic (must be treated as pre-existing defects, NOT flakiness):
      harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable
      harness::tool_resolution_tests::specialist_allowlists_admit_alias_spellings_of_the_granted_tool
  flaky (do not count as regressions unless they appear in `--test-threads=1` too):
      harness::tool_resolution_tests::aliases_that_resolved_before_still_reach_a_builtin_handler,
      tool_names::tests::claimed_aliases_never_fall_through_to_unknown_tool,
      orchestrator::notice_tests::{test_handle_reply_to_arbitrator_success_and_validation,
                                   test_handle_reply_to_arbitrator_ask_followup,
                                   test_steering_history_multiple_notices_resolved_out_of_order},
      ui::bridge::tests::test_bridge_notice_capacity_drop_is_observable_and_inbox_stays_bounded,
      test_role_gating::fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority
  comment_lines: agents=1175 harness=2187 llm=483 net=137 mcp=617 manager=1649 orchestrator=1851
                 ui=1530 types=17 tests=1424 root_src=1664 TOTAL=12734
```

One-line form for `diff`:
```
lib: 801 passed / 10 failed (811 total, flaky range 5-10); bin: 8 passed; integration: 135 passed / 2 failed (137 total); ignored: 0; sanctioned-failures: 5 (4x harness::pty::tests::*, test_role_gating::test_prompt_based_tool_gating); unsanctioned-deterministic: 2; flaky: 6; clippy: 0 diagnostics; fmt: clean; build: ok; comment_lines_total: 12734
```

### Recommended re-derivation protocol (to keep the diff meaningful)
Because the lib failure count is order-dependent, later verification should compare **two** runs, not one:
```bash
cargo test --no-fail-fast                 # headline tallies
cargo test --lib -- --test-threads=1      # deterministic floor: baseline = 806 passed / 5 failed
```
A regression is any failure absent from this baseline's list; an improvement is only real if the
serialised run's 5-failure floor also shrinks.
