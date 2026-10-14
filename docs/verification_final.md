# Final verification — t-015 FINAL GATE (comment cleanup + Swedish removal + cross-platform portability)

Read-only gate. **Nothing was fixed, formatted, reverted, or re-styled during this audit** — only measured.
Budget protocol honoured: one combined cheap-gate script, then each expensive suite run exactly once.

## 0. Provenance

| Item | Value |
|---|---|
| CWD / HEAD | `/home/fredrik/marmel/wip` @ `974b601` ("Refactor part 1"), branch `1.0.0-refactor` |
| Toolchain | `cargo 1.98.0` / `rustc 1.98.0 (88d9e12ae)` — same as `docs/baseline_test_run.md` |
| Worktree delta | `git status --porcelain` → **89 dirty files**; `git diff HEAD --stat` → **86 files changed, 711 insertions(+), 850 deletions(-)** (net −139 lines) |
| Baseline reference | `docs/baseline_test_run.md` @ content pin `974b601` |
| Baseline HEAD cross-check | the HEAD column of §6 reproduces the baseline comment table **exactly** (agents 1175 · harness 1712+475=2187 · llm 483 · net 137 · mcp 617 · manager 1649 · orchestrator 1851 · ui 704+482+344=1530 · types 17 · tests 1424 · root 1664 · **TOTAL 12734**) → counting method is baseline-compatible |

---

## 1. Cheap gates — one combined script (`/tmp/gate1.sh`), real exit codes

| Gate | Command | Exit | Output |
|---|---|---:|---|
| build | `cargo build` | **0** | `Finished dev profile … in 10.35s`; `warning/error` lines = **0** |
| fmt | `cargo fmt --all -- --check` | **0** | diff lines = **0** |
| clippy | `cargo clippy --all-targets --all-features -- -D warnings` | **0** | diagnostics = **0**; `duplicate_mod` matches = **0** |

Clippy caveat (measurement note, not a defect): the clippy invocation replayed a warm cache
(`Checking marmennill` count = 0, finished in 0.32s). Cargo fingerprints source mtimes, and the run
happened immediately after a fresh `cargo build` of the same tree, so the cached analysis is valid
for the delivered sources; no cache-bust re-run was performed (budget rule: no re-auditing of a pass).

## 2. `cargo test --no-fail-fast` (full workspace, run once)

**exit 101 · TOTAL 949 passed / 11 failed / 0 ignored** (baseline: 944 passed / 12 failed).

**Fail-fast caveat, explicitly:** plain `cargo test` (without `--no-fail-fast`) **aborts at the `--lib`
target** — the log contains `error: test failed, to rerun pass '--lib'`, and the 5 sanctioned lib
failures (plus order-dependent flakiness, §3) are therefore expected. Any headline tally from a
fail-fast run under-reports the integration suites; read the numbers with that caveat.

Per-target results from this run: `lib` **805 passed / 10 failed** (815 total), `bin (src/main.rs)`
**8 passed / 0 failed**, 16 integration targets **136 passed / 1 failed** (137 total), doc-tests 0.
Lib total grew from baseline 811 → **815 (+4 tests)**; integration total unchanged at 137.

## 3. Serial floor — `cargo test --lib -- --test-threads=1` (run once)

**exit 101 · test result: FAILED. 810 passed; 5 failed** → **exactly the expected 810 / 5**
(up from the Phase-2 gate's 807/5 — the +3 net tests all pass serially).

Failure set = the 5 sanctioned names, no deviation:
1. `harness::pty::tests::test_harness_pty_process_group_kill`
2. `harness::pty::tests::test_interactive_pty_manager_lifecycle`
3. `harness::pty::tests::test_run_command_parses_timeout_seconds_argument`
4. `harness::pty::tests::test_run_command_pty_cancelled_by_worker_token`
   (1–4: `/dev/pts/ptmx` EACCES / `openpty` code 13 — baseline §6, env-caused)
5. `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable`

**Zero non-sanctioned serial failures → no regression at the floor.** The row-11 flake of the old gate
(`config::tests::command_timeout_secs_reaches_execution_timeout`) did **not** reappear in either the
parallel (10-failure set) or the serial run.

Also sanctioned & documented: `tests/test_role_gating` → `test_prompt_based_tool_gating`
(**24 passed / 1 failed**, both in the combined run and the serial per-target run) — see
`docs/baseline_test_run.md` rows 151/180/198. Confirmed unchanged.

## 4. Integration targets — `cargo test --test <name> -- --test-threads=1`, every target under `tests/`

Exit codes: `0` for `ok` targets; `test_role_gating` = **101** (directly evidenced by
`error: test failed, to rerun pass '--test test_role_gating'` + `TEST_ALL_EXIT=101` in §2).

| target | result | failed test |
|---|---|---|
| test_agent | 2 passed / 0 | — |
| test_context | 8 / 0 | — |
| test_dynamic_prompts | 4 / 0 | — |
| test_harness | 15 / 0 | — |
| test_llm | 5 / 0 | — |
| test_midturn_notice | 3 / 0 | — |
| test_monitor | 2 / 0 | — |
| test_orchestrator | 8 / 0 | — |
| test_plan_checkoff | 6 / 0 | — |
| **test_role_gating** | **24 / 1** (exit 101) | `test_prompt_based_tool_gating` — **sanctioned** |
| test_specialist_schema_charge | 4 / 0 | — |
| test_specialist_stream | 10 / 0 | — |
| test_steer_history | 9 / 0 | — |
| **test_tool_name_literals** | **1 / 0** | — (**Phase-2 regression now closed**, see preserved rows §9) |
| test_ui_session | 21 / 0 | — |
| test_validation_loop | 14 / 0 | — |
| **TOTAL** | **136 passed / 1 failed** | 1 sanctioned |

## 5. Swedish removal — both passes from `docs/research_swedish_inventory.md` §0

**(i) diacritic pass** — `grep -rnE "[åäöÅÄÖ]" src/ tests/` → **27 lines**, byte-for-byte identical to
the §5 UTF8-DATA whitelist (last measured 27 lines): `src/harness/pty_tests.rs:778,784`;
`src/harness/pty.rs:972`; `src/text_util.rs:10,82,83,84,85,86,103,121`; `src/tool_args.rs:26,546,586,587`;
`src/ui/helpers.rs:450,641,642,654,671,672,682,683,789,940`; `tests/test_harness.rs:132,209`.
**Survivors outside the whitelist: 0.**

**(ii) curated diacritic-free word-list pass** (`grep -rn -w -i -E 'gick|testet|codern|debuggern|anropen|passerar|tagit|bort|inga|vad|har|fixat|implementerat|instruktionerna|databas|byta|bra|saknad|berodde|isolerat|implementerats|valdes|noll|konfiguration|parallellt|utmärkt|gällande' src/ tests/`)
→ **0 hits** (was 82 flagged lines across 10 files at inventory time).

**Post-t-013 requirement — asserted and PASSED:**
* `src/ui/transcript.rs` diacritic survivors = **0**; `src/orchestrator/notice.rs` diacritic survivors = **0** (both files are modified vs HEAD: `M src/ui/transcript.rs`, `M src/orchestrator/notice.rs`).
* `grep -rn 'följdfråga' src/ tests/` → **0 raw hits**.
* The only remaining Swedish-shaped text is the `\u{e4}/\u{f6}/\u{e5}` escape spelling at
  `src/ui/transcript.rs:514` (single line, inside
  `test_extract_steering_history_does_not_merge_a_swedish_only_pending_status`, with the doc-comment
  rationale at `:505-510`). **Classification: accepted, documented negative-test artifact (a pin that
  the pending heuristic must NOT match Swedish prose) — not Swedish prose.** Consistent with t-013's
  zero-accepted-residuals position.
* Documentation-path note (non-blocking): the delegation-cited `docs/comment_cleanup_report.md` does
  not exist on disk; the cleanup reports present are `docs/cleanup_report.md`,
  `docs/t007_comment_cleanup_report.md`, `docs/t011_comment_cleanup_report.md`. Nothing in §5(i)/(ii)
  depends on it — there are no residual survivors to reconcile.

## 6. Comment-count delta per module — `git show HEAD:<file>` vs work tree
(method: `grep -cE '^[[:space:]]*(//|///|//!)'`, identical to `docs/baseline_test_run.md` §8)

| Module tree | files | HEAD | work tree | delta |
|---|---:|---:|---:|---:|
| `src/agents` | 15 | 1175 | 1159 | **−16** |
| `src/harness` core | 14 | 1712 | 1724 | **+12** |
| `src/harness/monitor` | 6 | 475 | 452 | **−23** |
| `src/llm` | 7 | 483 | 474 | **−9** |
| `src/mcp` | 5 | 617 | 633 | **+16** |
| `src/net` | 4 | 137 | 137 | **0** |
| `src/manager` | 6 | 1649 | 1580 | **−69** |
| `src/orchestrator` | 14 | 1851 | 1790 | **−61** |
| `src/types` | 3 | 17 | 13 | **−4** |
| `src/ui` root | 6 | 704 | 701 | **−3** |
| `src/ui/bridge` | 7 | 482 | 465 | **−17** |
| `src/ui/tui` | 5 | 344 | 299 | **−45** |
| root `src/*.rs` | 11 | 1664 | 1622 | **−42** |
| `tests/` | 17 | 1424 | 1350 | **−74** |
| **TOTAL** | **104** | **12734** | **12399** | **−335** |

Module-tree HEAD figures aggregate exactly to the baseline table (harness = core+monitor = 2187;
ui = root+bridge+tui = 1530). The two **positive** deltas are the intentional Phase-2 portability
documentation (`src/mcp` +16 = `src/mcp/client.rs` PATH-lookup rationale; `src/harness` core +12 =
`src/harness/pty_tests.rs` / `search_tests.rs` cfg-pair rationale); every comment-cleanup target tree
is net negative. Files added at HEAD-time: none missing (`MISSING@HEAD` set empty).

## 7. Windows portability review (Phase 2 t-018..t-023 + t-024)

| Check | Result |
|---|---|
| `grep -rn '"/bin/' src/ tests/` | **0 hits** — no absolute-path program literal anywhere |
| Former residual gaps closed | `src/harness/tool_resolution_tests.rs:92` and `src/llm/client_tests.rs:667` now `command: Some("sh".to_string())` (were `"/bin/sh"`) — the two non-blocking gaps recorded by the Phase-2 gate are **fixed** |
| PATH-resolved spawn sites | `Command::new(` appears at `src/harness/pty_tests.rs:360`, `src/harness/sandbox.rs:978,1044,1082,1124,1156,1192,1206`, `src/main.rs:167,177`, `src/mcp/client.rs:91` — all bare program names |
| `cfg!(windows)` spawn specs | 5 sites: `src/prompts.rs:30`, `src/harness/search_tests.rs:746`, `src/harness/pty_tests.rs:336`, `:348`, `src/mcp/client.rs:597` |
| Paired `#[cfg(unix)]` / `#[cfg(not(unix))]` | 31 unix / 8 not-unix, paired at `src/harness/fs.rs:518`+`:553`, `src/harness/pty_tests.rs:265`+`:386`, `src/harness/pty.rs:823`+`:1578`, `src/main.rs:175`, `src/config.rs:547` |
| Unix no-op normalizers/stubs | verified on disk: `#[cfg(not(unix))] fn preserve_target_permissions(…) -> Ok(())` (`fs.rs:518`), `#[cfg(not(unix))] fn set_dir_permissions_755(…) {}` (`fs.rs:553`), `#[cfg(not(unix))] fn assert_gone_from_process_table(…) {}` (`pty_tests.rs:386`, with the doc rationale above it), `#[cfg(not(unix))] let code = 0;` in `waitpid_gone_error()` (`pty_tests.rs:265`) |
| `METHOD_TOOLS_CALL` | declared `src/mcp/protocol.rs:74`, used for request construction at `:160`; 6 further references in `src/mcp/http_tests.rs:20,57,384,460,500,551`. Remaining raw `"tools/call"` = 6, all JSON **wire-text fixtures/expectations** (`protocol.rs:74` const body, `:561`, `:777`, `:782`, `http_tests.rs:61`, `:391`) — legitimate, not dispatch call-sites |
| **`cargo check --target x86_64-pc-windows-gnu --all-targets`** | **exit 0** (target already installed via `rustup target list --installed`; no `rustup target add` needed, no network required). **0 errors**, **18 warning lines** = 17 lib-test warnings + 1 summary line, in 4 files: `src/harness/tool_resolution_tests.rs` (8), `src/harness/sandbox.rs` (4), `src/llm/client_tests.rs` (3), `src/mcp/client.rs` (2) — pre-existing dead-code/unused-import warnings under `cfg(windows)` |

**Not a gate failure but tracked:** those 17 Windows-only warnings mean a future
`cargo clippy --target x86_64-pc-windows-gnu … -- -D warnings` would not be green. The required gate
(`cargo check`, no `-D warnings`) is exit 0, and the Linux `-D warnings` gate is clean.

---

## 8. VERDICT determination

build 0 · fmt 0 (0 diff lines) · clippy `--all-targets --all-features -D warnings` 0 diagnostics ·
lib floor serial **810/5 (sanctioned set only)** · full `--no-fail-fast` 949/11 (1 sanctioned
integration + 10 parallel-lib, of which 5 sanctioned + 5 documented flaky) · integration serial
136/1 (sanctioned) · diacritic survivors **27 == whitelist exactly, 0 outside** · word-list pass
**0 hits** · `transcript.rs` / `notice.rs` **0 survivors** · `följdfråga` **0 raw hits** (1 accepted
`\u{…}` negative-pin line) · `"/bin/` **0 hits** · Windows cross-compile **exit 0** ·
comment lines **12734 → 12399 (−335)**.

## 9. Portability gate history (rows 1–10, retained verbatim from the Phase 2 gate)

| # | Failing test | Status | Evidence |
|---|---|---|---|
| 1 | `harness::pty::tests::test_harness_pty_process_group_kill` | **pre-existing (sanctioned, env)** | `src/harness/pty_tests.rs:16:47` `openpty … PermissionDenied` — byte-identical to baseline §5.2 #1; cause `/dev/pts/ptmx` mode `000` (baseline §6) |
| 2 | `harness::pty::tests::test_interactive_pty_manager_lifecycle` | **pre-existing (sanctioned, env)** | `pty_tests.rs:121:10` same `openpty` EACCES (baseline #2) |
| 3 | `harness::pty::tests::test_run_command_parses_timeout_seconds_argument` | **pre-existing (sanctioned, env)** | `pty_tests.rs:102:34` same EACCES (baseline #3) |
| 4 | `harness::pty::tests::test_run_command_pty_cancelled_by_worker_token` | **pre-existing (sanctioned, env)** | `pty_tests.rs:183:5 assertion failed: res.is_ok()` (baseline #4) |
| 5 | `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable` | **pre-existing (deterministic)** | `src/harness/mod.rs:1273:9 assertion failed: !res.is_error` — baseline #5; still fails in serial run (the only non-PTY failure) |
| 6 | `harness::tool_resolution_tests::specialist_allowlists_admit_alias_spellings_of_the_granted_tool` | **pre-existing** | `src/harness/pty.rs:1170:9 "there is no reactor running…"` — baseline #6, deterministic in isolation |
| 7 | `harness::tool_resolution_tests::aliases_that_resolved_before_still_reach_a_builtin_handler` | **pre-existing (cascade)** | `lazy_lock.rs:415:5 LazyLock … poisoned` — baseline #7 |
| 8 | `tool_names::tests::claimed_aliases_never_fall_through_to_unknown_tool` | **pre-existing (cascade)** | same poisoning — baseline #10 |
| 9 | `orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup` | **pre-existing (flaky)** | baseline §5.3 names it explicitly as flaky (`passes isolated`); appeared in the headline run, absent in repeats 1-2 |
| 10 | `orchestrator::notice_tests::test_steering_history_multiple_notices_resolved_out_of_order` | **pre-existing (flaky)** | `notice_tests.rs:681:9` — baseline #9 |

Status of those rows at this final gate: rows 1–5 remain (sanctioned serial floor, §3); rows 6–8
appear only in the parallel lib run (absent serially = documented flakiness, baseline §5.3);
rows 9–10 appeared in the parallel run and are absent at the serial floor (no regression).
The two Phase-2 REGRESSIONS that accompanied these rows — bare `"sleep"` literals at
`src/harness/pty_tests.rs:356` and `src/mcp/client.rs:604` breaking
`tests/test_tool_name_literals.rs:128` — are **resolved**: that guard target now reports
**1 passed / 0 failed** (§4) and `grep -rn '"/bin/'` is empty (§7).

VERDICT: CLEAN — regressions requiring t-016 repair: none.
