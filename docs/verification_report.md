# Verification report — `feature/new-ui`

> **This file has two parts.** §1–§8 below are the t-012b audit at HEAD `4fca031`, kept as the historical
> record. **PART B at the end of this file is the current audit** (t-020) at HEAD `9726d26`: its gate table,
> §10 acceptance matrix and D1–D11 ledger supersede the corresponding tables in §1–§8. Companion artefact:
> `docs/change_surface_report.md`.

---

## PART A — verification report (plan task t-012b)

Auditor: validator_specialist. Date: 2026-10-27 (CET). Branch audited: `feature/new-ui` @ `4fca031`.
Mode: **read-only audit**. No tracked source file was created, modified, deleted or reformatted. Nothing was
staged and nothing was committed (`git diff --cached --name-only` → empty; `git diff --stat` → empty at both
start and end of the audit). All command logs live in `/tmp/audit_logs/` (outside the workspace).

---

## 1. Environment limits (what this sandbox cannot prove)

| Limit | Evidence | Consequence |
|---|---|---|
| No PTY allocation (`/dev/ptmx` denied) | 4 × `harness::pty::tests::*` panic `failed to openpty: Os { code: 13, kind: PermissionDenied }`; same panic in the Oct-8 baseline logs `target/baseline_logs/lib_serial.log:831` | `run_command`/terminal tools cannot execute → 4 lib failures + 1 integration failure are environmental, and the live TUI (which needs a TTY) cannot be launched |
| `~/.marmel/marmel.toml` unreadable | `./target/debug/marmel --version` → `Error: reading /home/fredrik/.marmel/marmel.toml / Permission denied (os error 13)` | Any mode that reaches `config::load` without `--config <readable path>` dies before the session starts |
| No real terminal / TTY | cargo test stdout is a pipe; `src/main.rs:65` selects the raw (headless) renderer when `!stdout_is_terminal()` | §10 items that are only observable on a device (raw-mode entry, kitty protocol `Shift+Tab`/`Shift+Enter` delivery, true-colour rendering, pane sizes on a real device) are UNVERIFIABLE-IN-SANDBOX; only frame-level `TestBackend` assertions stand in |
| LLM provider credentials | not present | No end-to-end headless agent run could be attempted, even with `--config` override |

Note on gate 3/4 rigour: `cargo build` and `cargo clippy --all-targets` in the workspace were fully cached
(`cargo build -v` → 247 `Fresh` units, 0 compile lines; same for clippy). A cached "Finished" does not
replay warnings, so the gates were **re-run uncached against the exact HEAD tree**: `git archive HEAD | tar -x
-C /tmp/lintcopy`, then `CARGO_TARGET_DIR=<workspace>/target cargo clippy --all-targets` and `cargo build`
from that scratch copy. Both recompiled the crate (`Checking marmennill v1.0.0 (/tmp/lintcopy)` 30.5 s,
`Compiling marmennill … 31.3 s`) and emitted **zero** warnings (`CLIPPY_FORCED_EXIT=0`,
`BUILD_FORCED_EXIT=0`). The workspace working tree was never touched.

---

## 2. Gate results

| # | Gate | Result | Counts / failures |
|---|---|---|---|
| 1 | Branch + commit chain | **PASS (with a note)** | `feature/new-ui` @ `4fca031`. Chain present: `ae8d4d6, 9165143, 03cbd14, 7ce7b99, f999378, e41d9bf, 12d8f59, 2445408, 78297f8, 8634210, 1b9f24d, 3efb975, 4fca031`. Note: the task list omitted `12d8f59` and `2445408`, which sit between `e41d9bf` and `78297f8` — they are on the branch and are UI commits, so the chain is complete, not divergent. Merge base with `main` = `b66f669` |
| 2 | `git status --porcelain` | **PASS** | No tracked modification, nothing staged. Only untracked noise: `.marmel.old/`, `.marmel.old2/`, `.marmel.old.3…29/` (19 dirs), `docs.old/`. None staged, none committed |
| 3 | `cargo build` | **PASS (warning-free)** | Cached run exit 0; forced uncached re-run on `HEAD` tree exit 0, 0 warnings. Crate denies `unused`, `dead_code`, `unused_imports` (`Cargo.toml:67-70`) |
| 4 | `cargo clippy --all-targets` | **PASS (clean)** | Cached run exit 0; forced uncached re-run exit 0, no diagnostics. `clippy all = deny` (`Cargo.toml:72-73`). `#[allow(` in `src/ui/tui/**`: **0 occurrences**. Whole-tree `#[allow(`: 20, all pre-existing and outside the UI (`src/orchestrator/tests.rs` ×10, `src/orchestrator/workers.rs:843,1718`, `src/orchestrator/notice_tests.rs:333`, `src/agents/mod.rs:71`, `src/agents/runner/fix_loop.rs:681,1330`, `src/llm/stream.rs:555`, `src/llm/client.rs:1175`, `src/ui/session_tests.rs:2047`, `src/ui/bridge/steer.rs:107`, `src/ui/bridge/tests.rs:3546`). `git diff main...HEAD \| grep '^+.*#\[allow'` → only prose/doc mentions, **no attribute added** |
| 5 | `cargo test --lib ui::` | **PASS** | **526 passed / 0 failed** / 952 filtered out, 17.03 s, exit 0 |
| 6 | `cargo test --lib` | **EXPECTED RED (environmental)** | **1474 passed / 4 failed** (1478 total), 18.80 s, exit 101. All four are `harness::pty::tests::*`, all `openpty: PermissionDenied`; each re-runs identically in isolation (see §3) |
| 7 | `cargo test --test '*' --no-fail-fast` | **EXPECTED RED (environmental / pre-existing)** | **353 passed / 1 failed** across 37 binaries, exit 101. Sole failure `test_prompt_based_tool_gating` (`tests/test_role_gating.rs:310`) |
| 8 | Known-flaky-under-load tests | **PASS** | All 4 passed inside the full `--lib` batch *and* in isolation (`flaky_exit=0`, `4 passed; 0 failed`). No regression |
| 9 | `cargo fmt --all -- --check` | **FAIL** | exit 1; **30 hunks in 2 files**: `src/ui/tui/mod.rs` ×1 (hunk header `Diff in …/mod.rs:13`), `src/ui/tui/tests.rs` ×29 (`6842, 7066, 7143, 7396, 7403, 7479, 7640, 7677, 7752, 7759, 7841, 7964, 7977, 8065, 8072, 8084, 8109, 8152, 8164, 8305, 8396, 8408, 8428, 8437, 8494, 8513, 8782, 8809, 8816`). **Both offenders are introduced by this branch**: `git show main:src/ui/tui/tests.rs` and `git show main:src/ui/tui/mod.rs` each pass `rustfmt --edition 2024 --check` with **0 hunks** (defect D1) |
| 10 | Headless / raw-mode probe | **PARTIAL — mostly UNVERIFIABLE-IN-SANDBOX** | See §4. `--help` works headlessly (exit 0); `--config` override and the session-flag validator are reachable (exit 1 / exit 2 as designed); `--version` is not a flag (defect D10); `--raw` and any prompt run need config + credentials + PTY for tools and were not attempted |
| 11 | §10 acceptance matrix | **18 PASS / 3 FAIL-or-partial** | See §5 |

---

## 3. Failure detail (file:line, first panic line)

### 3.1 `cargo test --lib` — 4 failures, all PTY-dependent (environmental)

| Test | Panic site | First panic line |
|---|---|---|
| `harness::pty::tests::test_harness_pty_process_group_kill` | `src/harness/pty_tests.rs:16:47` | `spawn sandbox: Execution(failed to openpty: Os { code: 13, kind: PermissionDenied, message: "Permission denied" })` |
| `harness::pty::tests::test_interactive_pty_manager_lifecycle` | `src/harness/pty_tests.rs:120:10` | `spawn session: Execution(Failed to create PTY: failed to openpty: Os { code: 13, kind: PermissionDenied, … })` |
| `harness::pty::tests::test_run_command_parses_timeout_seconds_argument` | `src/harness/pty_tests.rs:102:34` | `executes with custom timeout: Execution(failed to openpty: Os { code: 13, kind: PermissionDenied, … })` |
| `harness::pty::tests::test_run_command_pty_cancelled_by_worker_token` | `src/harness/pty_tests.rs:179:5` | `assertion failed: res.is_ok()` — `res` is the `Err` from `run_command_pty` (`src/harness/pty.rs:878` → `PtySession::spawn` → `openpty`), i.e. the same denial surfacing through an `is_ok()` assert |

Re-run individually (`cargo test --lib -- --exact …`): all four FAIL identically, one at a time, in 0.00 s,
exit 101 → **environmental, not a regression**. The same four appear in the pre-branch baseline
`target/baseline_logs/lib_serial.log:855-862`. None of `src/harness/**` was touched on this branch
(`git diff --stat main...HEAD -- src/harness src/orchestrator src/agents src/tools` → only
`src/session/format/{mod.rs,ui.rs}`).

### 3.2 `cargo test --test '*'` — 1 failure

`test_prompt_based_tool_gating`, panic at **`tests/test_role_gating.rs:310:5`**:
`Validator with run_command in prompt must be permitted to execute run_command`.

* Not touched by the branch: `git log --oneline main..feature/new-ui -- tests/test_role_gating.rs` → **empty**.
* Subject not touched: the asserted entry point is `harness::dispatch_for` (`src/harness/mod.rs:454`), which routes
  `run_command` to `pty::run_command` (`src/harness/mod.rs:328,668,1062` → `src/harness/pty.rs:861` →
  `run_command_pty` → `PtySession::spawn`). The assertion therefore **executes a real command through a PTY**,
  which this sandbox denies. So the red is PTY-dependent here too, not a product regression.
* Same failure is recorded in the pre-branch baseline: `target/baseline_logs/role_gating.log`
  (`test test_prompt_based_tool_gating ... FAILED`, `tests/test_role_gating.rs:312:5`, 24 passed / 1 failed).
* Verdict: **pre-existing under this sandbox; cannot be classified as product red or green here.** It needs a
  host with PTY access.

### 3.3 Gate 8 — known-flaky-under-load

In the batch these 4 all passed; re-run in isolation they all pass again
(`/tmp/audit_logs/flaky_singles.txt`: `test result: ok. 4 passed; 0 failed`):
`harness::tool_resolution_tests::legitimate_mcp_tool_still_dispatches_to_the_mcp_path`,
`orchestrator::notice_tests::test_broadcast_targets_still_route_to_any_worker`,
`harness::dispatch_integrity_tests::test_notice_reply_round_trip_reaches_the_user_exactly_once`,
`ui::tui::tests::the_armed_abort_prompt_occupies_the_same_surface_and_holds_no_request`.

---

## 4. Headless / raw-mode probe (gate 10)

Argument handling read first: `src/main.rs:290-333` (`parse_args`), entry `src/main.rs:27-60`; the raw renderer
is `src/ui/raw.rs`, selected at `src/main.rs:65`
(`let use_raw = args.raw || cfg.ui_mode == "raw" || !stdout_is_terminal();`). There is **no print/one-shot
mode**: `--raw` still runs the full session loop.

| Probe | Result |
|---|---|
| `cargo run -- --help` | exit 0, usage text printed (proves `--help` is handled **before** `config::load`) |
| `./target/debug/marmel --config /nonexistent-dir/marmel.toml --raw` | exit 1, `Error: reading /nonexistent-dir/marmel.toml / No such file or directory` → `--config` override reaches the loader; no session started |
| `./target/debug/marmel --config marmel.toml.example --resume --fresh` | exit 2, `error: --resume and --fresh are mutually exclusive` → flag validation path verified headlessly, before any runtime/bundle work |
| `./target/debug/marmel --version` | exit 1, dies on the unreadable `~/.marmel/marmel.toml` — `--version` is not a flag and falls into the catch-all PROMPT arm (`src/main.rs:326-331`) → defect D10 |
| `marmel --raw "<prompt>"` (live headless session) | **NOT ATTEMPTED — impossible here**: needs a readable config, provider credentials and a PTY for every tool call |
| Live TUI (`crossterm` raw mode, alt screen, kitty keyboard protocol) | **UNVERIFIABLE-IN-SANDBOX** (no TTY, no PTY). Only the `TestBackend` frame tests stand in |
| Raw-renderer unit coverage | Present and green: 9 `#[test]` in `src/ui/raw.rs` (incl. `test_raw_output_carries_no_tui_chrome`), part of the 526 passing `ui::` tests |

---

## 5. §10 acceptance matrix (`docs/ui_design_spec.md:425-445`)

Evidence columns give the covering test (`tn` = `src/ui/tui/tests_new_ui.rs`, `t` = `src/ui/tui/tests.rs`,
`h/a/s/tr/pl/i/hl` = `src/ui/tui/components/{header,activity,status_bar,transcript,plan_pane,input,help}.rs`)
or the implementation line.

| Item | Verdict | Evidence |
|---|---|---|
| 1 Header `done/total` + active task id at 40x10, Plan closed | **PARTIAL FAIL** | `done/total`: `h:559 done_total_and_the_active_task_survive_into_a_40x1_frame` asserts `3/7 done` at 40 cols; `layout.rs:69-87` keeps `show_task: true` at every width. But `h:340-395` asserts the **task id is clipped away at 40 cols** (`!at_40.contains("t-004")`, `h:382-385`), and the 40-col test never asserts the task id. Item 1 as written conflicts with §7.4's drop ladder → defect D4 |
| 2 `done/total` = Checked/total for 3 fixtures | **PASS** | `t:3579 plan_progress_is_the_checked_box_split_of_the_same_plan_text` (empty → `0/0 done`; all-checked → `2/2`; mixed with a fenced `- [ ]` decoy → `2/3 done`); companion `t:3603` for the active task |
| 3 legend / footer / `/help` byte-identical; legend names only bound keys | **PASS** | `hl:575 legend_names_every_current_binding`, `hl:616 legend_advertises_no_removed_binding`, `hl:655 footer_legend_comes_from_the_table`, `hl:868 legend_rows_are_the_only_source_of_the_footer_and_the_help_body`; behavioural half `t:4880 every_legend_row_is_a_real_handler_path` (each legend row driven against the real handler) |
| 4 `F1` and `?`-on-empty open the same overlay; `?` with a draft inserts | **PASS** | `t:5012-5020` (`F1 / ?` driver: both set `help_open` + `overlay == Help`); `t:5273-5278` types `?` into `"what"` → `"what?"`, `!r.help_open` |
| 5 bare `stop/cancel/exit/quit/q/abort` are steering text, never exit | **PASS** | `t:6361 a_bare_abort_word_is_steering_text_and_never_ends_the_session` (6 words + `STOP`/`Stop`, `drain_submissions` equality, `!user_exit_requested()`, no confirm armed, hint line, history); gating implementation `src/ui/tui/events.rs:1395-1420` |
| 6 `/exit` opens `Quit`; `n` alive, `y` exits, `Esc` alive and not armed | **PASS** | `t:5337 both_quit_requests_route_through_the_renderers_existing_exit`, `t:5176 esc_closes_an_open_overlay_without_arming_then_arms_abort`, `t:6167 every_destructive_slash_command_stops_at_the_confirm_surface`; `ConfirmAction::Quit` at `events.rs:52` |
| 7 `/reset` + 5 aliases → one confirm; `y` sends the exact string over `tx`; no renderer-side `Plan::clear()` | **PASS** | 6 spellings: `src/ui/helpers.rs:276-287 is_reset_command`; routing `events.rs:95,708,745`; `t:6238 a_confirmed_command_is_handed_to_the_session_loop_verbatim`; short-circuit removal pinned by `t:5445` (`!submit.contains("Plan::default()") && !submit.contains("plan.clear()")`); the only `plan.clear()` left is the session-loop owner `src/ui/helpers.rs:297` |
| 8 `Ctrl+D` opens `Quit`, never exits; a second one still needs `y` | **PASS** | `t:5216 ctrl_d_opens_the_quit_confirm_and_never_exits_un_armed` (`5222`, `5226`, `5234` assert `!user_exit_requested()`) |
| 9 `x` → `CancelWorker{key,task_id}`; `/cancel <id>` resolves the same | **PASS** | `t:5280-5297` (exact `ConfirmAction::CancelWorker { key: "coder_specialist-t-004", task_id: Some("t-004") }` + status line), `t:6304 cancel_with_an_argument_confirms_one_named_worker`, `tn:641 x_asks_about_the_selected_row_and_only_that_row_item17` |
| 10 `Ctrl+T/F/A/P` each leave a distinct chip on status row 2 | **PASS** | `t:5535 every_toggle_leaves_a_chip_and_an_empty_pane_says_so` (`think:on`/`plan:on`/`workers:on`); chip renderer `s:429 chips`, `s:510 chip_line`; `s:755 chip_row_echoes_every_toggle_and_the_counts_when_not_armed` |
| 11 `Ctrl+A`/`F5` with zero workers → `No specialist launched yet.`, chip still flips | **PASS** | `t:5566-5584` (same test: `workers:on` + `No specialist` + `activity::EMPTY_STATE == "No specialist launched yet."`, `a:63`); `a:1027 empty_roster_shows_the_spec_empty_state` |
| 12 Focus badge `●` + `frame_focus` border, exactly one focused pane | **PASS** | `tn:1328 the_tab_focus_ring_walks_wraps_and_marks_the_focused_pane` (badge count == 1, corner colour == `theme::frame_focus()`), `tn:1398` reverse walk, `t:4622 the_focused_pane_badges_its_title_and_every_other_pane_does_not`, `t:5764 the_boot_frame_badges_the_composer_not_the_transcript` |
| 13 Printable/editing keys reach the composer from Plan/Workers focus; `x` + empty composer + Workers opens the overlay | **PASS** | `t:5253 printables_and_editing_keys_reach_the_composer_from_every_focus` (`Char('x')`, `Backspace`, `Delete` per pane; then the `x`-mnemonic case in the same test) |
| 14 Follow: `follow=false` + appended messages → viewport frozen, `↓ N new`, `Ctrl+F` → `LIVE`, **submit no longer re-arms silently** | **PARTIAL FAIL** | Covered: `tr:733 follow_off_keeps_the_viewport_still_and_counts_new_lines` (5 appended messages, identical body columns, `first.scroll == second.scroll`, `unseen == 5`), `t:5612 the_unseen_baseline_is_re_measured_at_the_tail_and_held_while_scrolled_up` (`↓ 10 new` → `LIVE`), `t:5587 ctrl_f_moves_the_follow_chip_in_the_transcript_pane`. **Not met:** submit still writes `chat_auto_scroll = true` (`events.rs:1380`, also `1345,1357,1395,1418`), the scroll-to-tail gate reads that flag (`mod.rs:1034`, `events.rs:1315`), and `adopt_legacy_view_writes` (`mod.rs:1257-1259`, called at `mod.rs:670,1291,1457,2011`) re-derives `follow` from it → silent re-arm. No test asserts follow across submit → defect D3 |
| 15 12 live workers → 12 reachable rows; no `.clamp(1, 3)` in `components/workers.rs` | **PASS (spec path is stale)** | `t:5662 every_worker_row_is_reachable_with_the_arrow_keys` (12 rows, selection `1..11`, no wrap), `a:933 roster_shows_every_worker_beyond_the_three_row_cap` (all 12 + `1/12` chip), `a:958`, `a:1347`. The file named by the item does not exist — the roster is `src/ui/tui/components/activity.rs`; the only remaining `.clamp(1, 3)` in the UI is the spec-sanctioned composer height at `layout.rs:289` → doc drift, defect D7 |
| 16 Overlay closes on Esc without arming; `y`/`n` honoured only inside confirm | **PASS** | `tn:289`, `tn:329`, `tn:393`, `tn:602` (all `_item16`); `t:5302 y_and_n_are_honoured_only_inside_the_confirm_overlay` |
| 17 Cancel one worker → exactly one worker path; `orchestrator::cancel_all()` not called | **PASS (verification vehicle deviates)** | `tn:705 confirming_one_worker_never_reaches_the_broadcast_cancellation_item17` pins it by source analysis of `events.rs` (`fn_body` + exactly 1 `orchestrator::cancel_all(` call site, inside `cancel_every_worker`), `tn:774`. The spec asked for the assertion via the recording renderer in `tests/test_session_restore_panel_abort.rs`, which contains **no** `cancel_all` assertion (grep: 0 hits; the branch only added the 2 D2 fixture fields) → defect D8 |
| 18 Restore round-trips `view_mode`, focus **including input**, follow, D2 queue/paused; witness warning = one transcript line | **FAIL (as written)** | Passes: `tn:837 capture_and_restore_repaints_mode_focus_follow_and_the_d2_pair_item18` (mode, Workers focus, follow, `QueueState{2,3}`, `paused`, on-screen chips), `tn:911 a_disagreeing_token_witness_arrives_as_one_transcript_line_item18`, `t:3917`, `t:8179`, `t:8230`. **Fails on "focus (including input)"**: `state.rs:130-136 FocusPane::legacy` folds `Input|Transcript → FocusedPanel::Chat` and `state.rs:143-149 from_legacy` returns `Transcript`, so composer focus does not round-trip — and the branch's own test asserts the loss (`tn:888-903`). Root cause: §9.4 D2's `ViewState` extension was **not implemented** — `FOCUSED_PANELS` is still `["chat","plan","subagents"]` (`src/session/format/ui.rs:324`) and `ViewState` (`ui.rs:267-286`) has no `view_mode` / `follow` fields → defect D2 |
| 19 No zero-sized pane rect at 100x30/90x24/60x16/40x10; too-small notice at 39x9; no line outside its rect | **PASS** | `tn:963 no_pane_is_zero_sized_overlapping_or_outside_its_rect_item19` (100x30, 90x24, 80x24, 60x16, 40x10 + containment), `tn:1028 below_the_floor_only_the_too_small_notice_is_drawn_item19` (40x10 exactly at the floor, `rects.notice.is_none()`), `tn:1085 the_degradation_ladder_drops_panes_in_spec_order_item19`, `t:4112`, `t:4141` |
| 20 No `MouseEvent` arm, no `EnableMouseCapture`; `DisableMouseCapture` only at `mod.rs:1165,1844` | **PASS (cited line numbers are stale)** | Actual sites: import `src/ui/tui/mod.rs:23`, teardowns `mod.rs:2123` and `mod.rs:2898` (in `shutdown()` `mod.rs:2114` and `leave_alt_screen()` `mod.rs:2891`). `tn:1638 no_mouse_arm_and_no_mouse_binding_survive_in_the_ui_sources_item20` asserts exactly 3 code occurrences; `tn:1688 a_mouse_report_changes_nothing_on_screen_item20` proves mouse reports change no cell. Zero `EnableMouseCapture`/`MouseEvent` in non-test UI sources (grep). Spec text cites `1165,1844` → defect D6 |
| 21 Cadence constants unchanged; idle renderer performs no `terminal.draw` | **PASS** | `tn:1727 the_cadence_constants_are_the_unchanged_spec_ones_item21`, `tn:1764 an_idle_clean_renderer_performs_no_terminal_draw_item21`. Constants cross-checked against `main`: `events.rs:126` poll 30 ms, `mod.rs:1302` 25 ms, `mod.rs:1306` 125 ms, `mod.rs:1106` 3 s, `mod.rs:2024` 25 ms floor — identical values to `main` (`main:src/ui/tui/mod.rs:414,434,438,1097`) |
| (a) clippy/build clean, no new `#[allow]` | **PASS** | §2 gates 3-4; forced uncached re-runs clean; no `#[allow` attribute added anywhere on the branch |
| (b) moved plan helpers compile from the new home; `plan_parse` tests pass | **PASS** | `src/plan_parse.rs` (`parse`, `is_pending`, `is_completed`, `checked_line`, `is_task_id_token`, `find_task_id_token`, `task_id_eq`, …) called from `src/ui/tui/formatting.rs:999,1029,1035,1052`; `tests/test_plan_checkoff.rs` green in gate 7 |
| (c) no `val N` / arbitration round counter while D1 is undone | **PASS because D1 landed** | D1 exists: `SubagentDetail::validation_rounds` (`src/ui/mod.rs:106`) filled from `WorkerEntry` (`src/ui/tui/mod.rs:204`, declared at `mod.rs:183`) and printed at `a:367-369`, `h:172-179`, `s:384`; withheld when the registry recorded nothing (`h:490-497`) and no "round" word ever renders (`h:498-503`). The gate's clause is moot with D1 in, and the surviving invariant ("a `true` budget is a licence, not an instruction") is pinned |

---

## 6. Defects found (numbered, file:line, severity)

1. **D1 — `cargo fmt --all -- --check` is red, and the red was introduced by this branch.** 30 hunks:
   `src/ui/tui/mod.rs:13-16` (the one-line `#[cfg(test)] mod tests_new_ui;` added at `mod.rs:16`) and 29 hunks in
   `src/ui/tui/tests.rs` at `6842, 7066, 7143, 7396, 7403, 7479, 7640, 7677, 7752, 7759, 7841, 7964, 7977, 8065,
   8072, 8084, 8109, 8152, 8164, 8305, 8396, 8408, 8428, 8437, 8494, 8513, 8782, 8809, 8816`.
   `git show main:src/ui/tui/{tests.rs,mod.rs}` both pass `rustfmt --edition 2024 --check` with 0 hunks, so the
   "pre-existing" framing in the task brief is **wrong**. Severity: **Medium** (a named gate fails on the branch;
   trivially mechanical, but it is branch-authored debt).
2. **D2 — Persistence delta D2 (spec §9.4) is only half implemented, so §10 item 18 ("focus including input") fails.**
   `src/session/format/ui.rs:324` still `FOCUSED_PANELS = ["chat","plan","subagents"]` (no `"input"`);
   `ViewState` at `src/session/format/ui.rs:267-286` has no `view_mode` / `follow` fields (queue/paused D2 fields
   **are** in at `ui.rs:181-203`). Consequence chain: `src/ui/tui/state.rs:130-136` (`Input|Transcript → Chat`),
   `state.rs:143-149` (`Chat → Transcript`), `src/ui/tui/mod.rs:2312` (writes the 3-value tag),
   `mod.rs:2464-2465`, `mod.rs:2487-2494`. The loss is deliberate and pinned by
   `src/ui/tui/tests_new_ui.rs:888-903`, which asserts a composer focus returns as `Transcript`. Severity:
   **Medium** — a documented-but-unmet acceptance item: an operator who restores a session loses composer focus
   (a keystroke to get back) and the spec was not amended.
3. **D3 — §10 item 14's "submit no longer re-arms follow silently" is not satisfied.** `src/ui/tui/events.rs:1380`
   (inside `submit`, immediately after pushing `User: …`; siblings at `events.rs:1345,1357,1395,1418`) assigns
   `chat_auto_scroll = true` directly; the viewport pin reads that same flag (`src/ui/tui/mod.rs:1034`,
   `src/ui/tui/events.rs:1315`), and `TuiRenderer::adopt_legacy_view_writes` (`src/ui/tui/mod.rs:1257-1259`,
   invoked before every frame at `mod.rs:670,1291,1457,2011`) calls `set_follow(self.chat_auto_scroll)`. Net
   effect: submitting while `follow == false` jumps the transcript to the tail and flips the chip back to `LIVE`.
   No test asserts follow across a submit (`set_follow(false)` appears at `tests.rs:3740,3924,4061,8740`,
   `tests_new_ui.rs:842,1333,1409`; none submits afterwards). Severity: **Medium** (behavioural contract gap +
   coverage hole).
4. **D4 — spec-internal contradiction, and a coverage hole, on the 40-column header.** §10.1 demands the active
   task id at 40x10; §7.4's ladder makes `> t-NNN` the last chip and the branch pins its absence at 40 cols for a
   long id (`src/ui/tui/components/header.rs:382-385` inside `seventy_columns_degrade_one_chip_at_a_time_in_ladder_order`),
   while `src/ui/tui/layout.rs:88` keeps `show_task: true` unconditionally. The 40-col test
   (`header.rs:557-578`) asserts `3/7 done` but never the task id. Severity: **Low-Medium** (needs either a spec
   amendment or a 40-col assertion with a short task id).
5. **D5 — frame-time global mutable state in the status row makes frame assertions process-global-dependent.**
   `src/ui/tui/mod.rs:830` calls `crate::llm::get_global_token_counts()` and `mod.rs:839-840` folds it with
   `.max()` into `StatusData.tokens_in/tokens_out`. Any test that touches the global LLM counters can change the
   status row of an unrelated frame test running in the same process (`tests.rs:7988`, `tests.rs:8020`,
   `tn:911`). No flake observed in this run, but the exposure is real. Severity: **Low-Medium** (determinism risk).
6. **D6 — stale line numbers in the binding contract.** §10.20 says `DisableMouseCapture` remains at
   `mod.rs:1165,1844`; the real sites are `src/ui/tui/mod.rs:2123` and `src/ui/tui/mod.rs:2898` (import at
   `:23`). The test already asserts the true count (`tests_new_ui.rs:1653-1657`: import + 2 teardowns = 3).
   Severity: **Low** (documentation), but `docs/ui_design_spec.md:444` is declared binding at `:454-455`.
7. **D7 — §10.15 names a file that does not exist.** "no `.clamp(1, 3)` remains in `components/workers.rs`" —
   there is no `src/ui/tui/components/workers.rs`; the roster lives in `src/ui/tui/components/activity.rs`
   (cap removed; `activity.rs:933/958/1347`). The only `.clamp(1, 3)` in the UI is the spec-sanctioned composer
   draft height at `src/ui/tui/layout.rs:289`. Severity: **Low**.
8. **D8 — item 17 is verified by source-text grepping, not by the prescribed integration vehicle.**
   `src/ui/tui/tests_new_ui.rs:742-762` asserts `fn_body(...)` contains no `cancel_all` and that exactly one
   `orchestrator::cancel_all(` call site exists in `events.rs`; the recording renderer in
   `tests/test_session_restore_panel_abort.rs` gained only the two D2 fixture fields
   (`queue: None`, `paused: false` at the diff `@@ -504,6 +504,10 @@`) and asserts nothing about `cancel_all`.
   Severity: **Low** (a rename or a new call site inside a differently-named function would slip past the current
   assertion).
9. **D9 — §3 confirm-request inventory has drifted from the code.** Spec §3 lists
   `Quit | ResetPlan | CancelWorker | CancelAll | ClearTranscript`; the implementation is
   `Quit | CancelWorker{key,task_id} | CancelAll | Command{line}` (`src/ui/tui/events.rs:49-72`), and there is no
   transcript-clear command at all (grep `"/clear"` → 0 hits in `src/ui`), so no `ClearTranscript` surface exists.
   Severity: **Low** (spec text vs. model naming).
10. **D10 — unknown long options are silently swallowed as the session prompt.** `src/main.rs:326-331` has a
    catch-all `other => args.prompt = Some(other)`; `--version` is not a flag, so
    `marmel --version` becomes a prompt and then dies on config load (exit 1). Pre-existing on the base branch
    (`src/main.rs` has no commits in `main..feature/new-ui`). Severity: **Low**.
11. **D11 — unit tests write terminal reset sequences to the ambient stdout.** `leave_alt_screen()`
    (`src/ui/tui/mod.rs:2891-2901`) executes against `io::stdout()` unconditionally, so the lib tests emit real
    resets: `/tmp/audit_logs/test_lib.txt` contains **2776 ESC bytes**, 176 lines carrying `ESC[?1049l`
    (`test_ui.txt`: 2776 ESC bytes, 165 lines) — e.g. `ESC[<1u ESC[?1049l ESC[?1006l ESC[?1015l ESC[?1003l
    ESC[?1002l ESC[?1000l ESC[?2004l` immediately before a test name. Consequence: a human reading a local
    `cargo test` in a real terminal has their alt-screen/paste/mouse modes rewritten by the test suite, and CI
    logs are polluted. Severity: **Low** (hygiene + surprising side effect), but it is the kind of thing that
    bites an operator running tests on a device.
12. **Workspace noise (informational, not a defect):** 21 untracked noise entries (`.marmel.old/`,
    `.marmel.old2/`, `.marmel.old.3 … .29`, `docs.old/`) sit in the working tree. They are **not staged and not
    committed**, so gate 2 is clean, but they should not be added by any later commit.

---

## 7. Residual risks — what needs a real terminal / real host

1. **The four `harness::pty::tests` and `tests/test_role_gating.rs::test_prompt_based_tool_gating` are unproven,
   not proven green.** Both groups are gated on `openpty`; they must be re-run on a host with PTY access before
   the branch can be called "all green". The role-gating one is recorded red in the pre-branch baseline as well,
   so it is at minimum not a new regression.
2. **No live TUI was ever launched** (no TTY, no PTY, no readable `~/.marmel/marmel.toml`). Everything about the
   rebuild's device behaviour is inferred from `TestBackend` frames: raw-mode entry/teardown ordering
   (`mod.rs:1430` kitty push, `mod.rs:2114` shutdown, `mod.rs:2891` `leave_alt_screen`), kitty `Shift+Tab` /
   `Shift+Enter` delivery, true-colour vs. 16-colour fallback, and real resize behaviour under the §2.5 ladder.
3. **`--raw` headless session never executed** (needs config + provider credentials + PTY-backed tools). The raw
   renderer's non-interactive contract rests on its 9 unit tests only.
4. **Focus/restore and follow/submit semantics (D2, D3) are the two behavioural acceptance gaps** that a real
   session would make obvious to an operator: after a restore the composer is not focused, and after any submit the
   transcript jumps to the tail even with follow switched off.
5. **Test-suite side effects on the operator's terminal (D11)** will only be visible when tests are run on a
   device, which is exactly when they are annoying.
6. **Frame-test determinism (D5)**: the token fields fold in a process-global counter; under `--test-threads`
   changes or added LLM-touching tests, frame assertions could start failing without any UI regression.

---

## 8. Exact numbers (for the record)

| Command | Passed | Failed | Exit |
|---|---|---|---|
| `cargo test --lib ui::` | 526 | 0 | 0 |
| `cargo test --lib` | 1474 | 4 (all `harness::pty::tests::*`) | 101 |
| `cargo test --test '*' --no-fail-fast` | 353 | 1 (`test_prompt_based_tool_gating`) | 101 |
| `cargo build` (forced, uncached, `git archive HEAD` copy) | — | 0 warnings | 0 |
| `cargo clippy --all-targets` (forced, uncached, same copy) | — | 0 diagnostics | 0 |
| `cargo fmt --all -- --check` | — | 30 hunks / 2 files | 1 |

---
---

# PART B — final branch audit at HEAD `9726d26` (plan task t-020)

Auditor: validator_specialist. Date: 2026-10-27 (CET). Branch: `feature/new-ui` @
`9726d26b4740417a923de09ab9fb86252e10ebd7` (parent `d8cb75d`).
Mode: **read-only on `src/`**. Part A (§1–§8 above) is the t-012b audit at HEAD `4fca031` and is left
untouched as a historical record; **Part B supersedes its gate numbers, its §10 matrix and its defect
ledger**, because five fix commits and three test/doc commits landed after Part A was written.
Companion artefact: `docs/change_surface_report.md` (t-020) — commit chain, diff surface, file inventory,
out-of-surface touch list, test-count evolution.

Read-only proof for this audit: before writing, `git status --porcelain` showed only untracked
`.marmel.old*/`, `docs.old/`; after writing, the only tracked/untracked additions are
`docs/change_surface_report.md` and this append. Nothing under `src/` was created, modified, deleted or
reformatted; `README.md`, `docs/new_ui.md`, `docs/runtime_smoke_report.md`, `.marmel*` and `docs.old/`
were not touched. Cargo runs wrote to `target/` only (gitignored).

---

## B.1 Commit chain — reconciled (full evidence in `docs/change_surface_report.md` §1)

* **24 commits**, strictly linear, from `ae8d4d6` (parent `b66f669`) to `9726d26`.
  `git rev-list --count --ancestry-path b66f669..HEAD` = 24; `git log --merges` over the range = 0.
* **`12d8f59` and `2445408` are ordinary reachable commits on the chain**, not amended, not superseded, not
  dangling, not unrelated: `git cat-file -t` → `commit` for both; `12d8f5958a44d9acd79e2853bb84c832db0ca957`
  parent `e41d9bf`, `2445408e15883b96d22b606a1654411d2124eaa1` parent `12d8f59`;
  `git merge-base --is-ancestor <sha> HEAD` → YES for both; `git branch --all --contains` →
  `feature/new-ui` only; both appear in `git log --oneline` and `git log --all --oneline`; the reflog shows
  them as plain `commit:` entries (`2026-10-26 23:10:24` and `23:27:42`) with no amend/rebase attached.
  **The premise that they were "absent from the recorded chain" is an artefact of a truncated 22-sha chain
  list — the real chain has 24 entries and contains both.**
* The **one** genuinely unreachable commit in the branch window is `3290cdf324af4031a2678aa749562b13d7c88b1f`,
  the pre-amend version of the D3 fix (`reflog`: `commit:` at 07:12:57 → `commit (amend):` producing
  `684de35` at 07:13:09). `--is-ancestor` → NO, `branch --contains` → empty,
  `git fsck --unreachable --no-reflogs` lists it. Every other unreachable object dated inside the window is
  stash bookkeeping (`WIP on feature/new-ui: …`, `index on …`, `untracked files on …`). No orphan child of
  any branch-chain commit exists beyond `3290cdf` and those stashes.
* **Clean descendant of the base branch, nothing rewritten:** `git merge-base main HEAD` =
  `b66f669` = `main` tip = `origin/main` tip (`git rev-parse main origin/main` identical);
  `git rev-list --count HEAD..main` = 0; the branch root's parent is `b66f669` itself;
  `reflog show main` is all `pull -r: Fast-forward`, no `reset`/`rebase`/force entry in the window. No
  `master` ref exists.

---

## B.2 Gates re-run at HEAD `9726d26` — actual numbers (2026-10-27, this sandbox)

Every number below was produced by a command run in this audit. Cached-vs-forced is disclosed.

| # | Gate | Exit | Actual result |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | **0** | clean, **0 output bytes** (Part A's 30 hunks are gone; D1 stays fixed) |
| 2 | `cargo build` | **0** | `Finished dev profile … in 0.22s` (warm). **Forced uncached** against the HEAD tree (`git archive HEAD` → `/tmp/lintcopy`, shared target dir): `Compiling marmennill v1.0.0 (/tmp/lintcopy)` in **23.28 s**, **0 warnings** |
| 3 | `cargo clippy --all-targets -- -D warnings` | **0** | warm run 0.27 s, 0 diagnostics. **Forced uncached** on the same scratch copy: `Checking marmennill v1.0.0 (/tmp/lintcopy)` in **26.05 s**, **0 diagnostics**, `-D warnings` satisfied |
| 4 | `cargo test --lib ui::` | **0** | `test result: ok. **560 passed; 0 failed**; 0 ignored; 0 measured; 952 filtered out; finished in 16.54s` |
| 5 | `cargo test --lib --no-fail-fast` | 101 | `test result: FAILED. **1508 passed; 4 failed**; 0 ignored; finished in 17.92s` — the 4 are exactly `harness::pty::tests::{test_harness_pty_process_group_kill, test_interactive_pty_manager_lifecycle, test_run_command_parses_timeout_seconds_argument, test_run_command_pty_cancelled_by_worker_token}` |
| 6 | `cargo test --test '*' --no-fail-fast` | 101 | **38 test binaries: 362 passed / 1 failed / 2 ignored.** Sole failure `test_prompt_based_tool_gating` (`tests/test_role_gating.rs:310:5`, `Validator with run_command in prompt must be permitted to execute run_command`). The 2 ignores are in `test_subpane_logs_repro` |
| 7 | Escape-byte check on captured `cargo test --lib` stdout | **PASS** | captured file 136 176 bytes → **0 `ESC` (0x1b) bytes, 0 `CR` (0x0d) bytes** (byte-level scan, `grep -c $'\x1b'` agrees: 0). D11 has **not** regressed |
| 8 | `#[allow]` delta | **PASS** | `src/` `#[allow(`: **20 at HEAD, 26 at `b66f669`**; diff shows **0 added, 6 removed** (`#[allow(dead_code)]` on the five composer helpers `src/ui/tui/events.rs:398,415,431,446,459` deleted by `7ce7b99`, one field in `src/ui/tui/mod.rs:187` deleted by `9165143`). All 20 survivors are outside `src/ui/tui/**` (9 `orchestrator/tests.rs`, 2 `orchestrator/workers.rs`, 2 `agents/runner/fix_loop.rs`, 1 each `orchestrator/notice_tests.rs`, `agents/mod.rs`, `llm/stream.rs`, `llm/client.rs`, `ui/session_tests.rs`, `ui/bridge/steer.rs`, `ui/bridge/tests.rs`) — the three `src/ui` ones exist at `b66f669` too |
| 9 | Known-flaky-under-load set | **PASS** | none of `test_notice_reply_round_trip…`, `test_broadcast_targets_still_route_to_any_worker`, `legitimate_mcp_tool_still_dispatches…`, `the_armed_abort_prompt_occupies_the_same_surface_and_holds_no_request`, `throttled_triggers_coalesce…` was red in run #5/#6 |
| 10 | Working tree | **PASS** | `git status --porcelain` (pre-write): 21 untracked entries only (`.marmel.old*/`, `docs.old/`); nothing staged, none of them ever committed |

### B.2.1 The four red lib tests are environmental (re-confirmed at this HEAD)

Verbatim panic sites/first lines from run #5:
`src/harness/pty_tests.rs:16:47` → `spawn sandbox: Execution(failed to openpty: Os { code: 13,
kind: PermissionDenied … })`; `:120:10` → `spawn session: Execution(Failed to create PTY: failed to
openpty: …)`; `:102:34` → `executes with custom timeout: Execution(failed to openpty: …)`; `:179:5` →
`assertion failed: res.is_ok()` (the same denial surfacing through an `is_ok()` assert).
`src/harness/**` has **zero commits** on this branch, so these four are not branch-authored.

### B.2.2 The one red integration test is environmental / pre-existing

`test_prompt_based_tool_gating` asserts a role may `run_command`; that tool path allocates a PTY
(`src/harness/pty.rs` → `portable_pty` `openpty`), which this sandbox denies (§1.1 of
`docs/runtime_smoke_report.md`, `python3 -c "import pty; pty.openpty()"` → `OSError: out of pty devices`).
`src/tool_names.rs`, `src/harness/**` and `tests/test_role_gating.rs` all have **zero commits** on this
branch, so it cannot be a UI regression.

---

## B.3 §10 acceptance matrix — verdict per item, at HEAD `9726d26`

All cited in-crate tests ran green inside gate #4 (560/0); the integration one ran green inside gate #6.
Line numbers are live at this HEAD.

| §10 item | Verdict | Evidence (test / file:line, verified at this HEAD) |
|---|---|---|
| 1 header `done/total` + active task id at 40x10, plan closed | **MET** (fixed by `dc7712f`) | `components/header.rs:613 done_total_and_the_active_task_survive_into_a_40x1_frame`; `:651 the_task_id_is_on_the_header_line_at_every_supported_width` (loops 80→40); `:705 an_over_long_task_id_is_the_only_fit_pass_casualty`; `layout.rs:86 show_task: true` at every width; header line at 40 asserted to carry `Marmel`, `3/7 done`, `▶ t-017d` (`header.rs:456`) |
| 2 `done/total` = checkbox split of the same plan string, 3 fixtures | **MET** | `header.rs:462 progress_counter_matches_the_checkbox_split_of_the_plan_text` — fixtures `("",0,0)`, all-checked `(2,2)`, `MIXED (3,7)`; `:493 an_empty_plan_still_carries_the_counter_and_no_task_chip` |
| 3 legend / footer / `/help` byte-identical; legend names only bound keys | **MET** | `components/help.rs:545 legend_is_non_empty_with_unique_keys`, `:575 legend_names_every_current_binding`, `:616 legend_advertises_no_removed_binding`, `:655 footer_legend_comes_from_the_table`, `:868 legend_rows_are_the_only_source_of_the_footer_and_the_help_body`; `tests.rs:4966 every_legend_row_is_a_real_handler_path` presses every legend row for real (`:4994-5215`) |
| 4 `F1` and `?`-on-empty open the same overlay; `?` with a draft types | **MET** | `tests.rs:5098-5106` (inside `every_legend_row_is_a_real_handler_path`): `F1` then `?` yield the same overlay; `tests.rs:5360-5364` `set_input_content("what")` → `input_text == "what?"`, `!help_open`; `tests_new_ui.rs:304/328` |
| 5 `stop/cancel/exit/quit/q/abort` = steering text, never exit | **MET** | `tests.rs:6450 a_bare_abort_word_is_steering_text_and_never_ends_the_session` (word table at `:6452`, incl. `STOP`, `Stop`) via `drain_submissions` + `!user_exit_requested()`; `helpers.rs:1296 test_is_abort_command_recognizes_the_slash_spellings_only`, `:1327 test_bare_abort_words_and_the_legacy_vocabulary_are_both_named`; sink gate `session_tests.rs:854/881/922/964`; landed `12d8f59`, `2445408`, `78297f8` |
| 6 `/exit` → `Quit`; `n` alive, `y` exits; `Esc` alive **and not armed** | **MET** | `tests.rs:5302 ctrl_d_opens_the_quit_confirm_and_never_exits_un_armed`, `:5388 y_and_n_are_honoured_only_inside_the_confirm_overlay`, `:5423 both_quit_requests_route_through_the_renderers_existing_exit`, `:6019 the_quit_confirm_is_painted_modally_over_the_body_with_the_metrics_live`, `tests_new_ui.rs:339 …_item16` |
| 7 `/reset` + all aliases → one confirm request; on `y` the verbatim string goes over `tx`; no renderer-side plan clear | **MET** | `ConfirmAction::Command { line }` (`events.rs`) carries the command verbatim; `tests.rs:6256 every_destructive_slash_command_stops_at_the_confirm_surface`, `:6327 a_confirmed_command_is_handed_to_the_session_loop_verbatim`, `:5531` asserts the submit path contains no `Plan::default()`/`plan.clear()`; alias table `helpers.rs:276-287` (`/reset`, `/reset_plan`, `/reset-plan`, `/clear_plan`, `/clear-plan`, `/reset_execution_plan`) pinned by `session_tests.rs:649 test_is_reset_command_matches_aliases`; the plan clear happens on the **session-loop** side (`session.rs:936 handle_reset_command_and_commit` → `helpers.rs:289 handle_reset_command`) |
| 8 `Ctrl+D` opens `Quit`, no `user_exit`; a second `Ctrl+D` still needs `y` | **MET** | `tests.rs:5302-5312`: `!user_exit_requested()` after one press and after a second (`"a second Ctrl+D still needs y"`); key-table row `Ctrl+D` at `tests.rs:5187-5191` |
| 9 `x` → `CancelWorker{key,task_id}`; `/cancel <id>` resolves the same | **MET** | `tests.rs:6114 the_cancel_worker_confirm_names_the_row_it_will_stop`, `:6393 cancel_with_an_argument_confirms_one_named_worker`, key-table row `x` (`tests.rs:5096-5106`, exact `CancelWorker { key: "coder_specialist-t-004", task_id: Some("t-004") }`), `tests_new_ui.rs:813 x_asks_about_the_selected_row_and_only_that_row_item17` |
| 10 `Ctrl+T/F/A/P` each leave a distinct persistent chip on status row 2 | **MET** | `tests.rs:5621 every_toggle_leaves_a_chip_and_an_empty_pane_says_so` (toggle off→on pairs, chip text asserted) |
| 11 `Ctrl+A`/`F5` with zero workers renders `No specialist launched yet.`; chip still flips | **MET** | `components/activity.rs:63 pub const EMPTY_STATE: &str = "No specialist launched yet.";`, `:1027 empty_roster_shows_the_spec_empty_state` (asserts the string at `:1041`); chip flip asserted in `tests.rs:5621` (and `:5667` quotes the same string) |
| 12 focus badge: `●` in the focused pane's title, `frame_focus` border, exactly one focused | **MET** | `tests.rs:4708 the_focused_pane_badges_its_title_and_every_other_pane_does_not`; `components/transcript.rs:1243 the_focus_badge_and_border_only_ever_track_focus`; `components/input.rs:675 title_carries_the_composer_keys_and_focus_badge`, `:648 border_colour_marks_typing_target_and_armed_abort`; ring `tests_new_ui.rs:1789 the_tab_focus_ring_walks_wraps_and_marks_the_focused_pane`, `:1859 shift_tab_walks_backwards_and_the_ring_skips_panes_the_mode_removed`; `tests.rs:5850 the_boot_frame_badges_the_composer_not_the_transcript` |
| 13 printable routing: `Char('x')`, `Backspace`, `Delete` mutate `textarea` under Plan/Workers focus; empty-composer `x` opens the overlay | **MET** | `tests.rs:5339 printables_and_editing_keys_reach_the_composer_from_every_focus` (draft `"ab"` survives in every pane focus), plus the `x` overlay case in the same table (`tests.rs:5096-5106`) and `tests_new_ui.rs:813` |
| 14 follow: only an explicit gesture re-arms; submit/notice/output never touch it | **MET** (fixed by `684de35`) | `tests_new_ui.rs:2399 a_submit_leaves_follow_off_and_the_body_exactly_where_the_operator_left_it_item14`, `:2475 a_submit_while_following_still_pins_the_transcript_to_the_bottom_item14`, `:2537 the_follow_toggle_and_the_follow_command_re_arm_follow_and_repin_the_body_item14`, `:2618 a_stray_write_of_the_auto_scroll_mirror_cannot_repin_the_frame_item14`; `tests.rs:5673 ctrl_f_moves_the_follow_chip_in_the_transcript_pane`; structural: the only writer of the legacy mirror is `mod.rs:1280 project_follow_mirror` (named at `:1269`), `events.rs` has **no** `chat_auto_scroll` assignment outside its own tests, and the §10.14 wording in the spec was amended in the same commit |
| 15 roster 12 live workers → 12 reachable rows; ownership `activity.rs`/`plan_pane.rs`; only the sanctioned `.clamp(1, 3)` | **MET** | `tests.rs:5748 every_worker_row_is_reachable_with_the_arrow_keys`; `activity.rs:933 roster_shows_every_worker_beyond_the_three_row_cap`, `:958 every_row_stays_reachable_when_the_roster_is_clipped`; `grep -rn "clamp(1, 3)" src/ui/` → only `layout.rs:289` inside `input_height` (`rows < 20` branch, verified by reading `layout.rs:285-293`); no `components/workers.rs` exists, and the spec text now names the real owners (`c6794a6`) |
| 16 overlay closes on Esc without arming abort; y/n honoured only inside the confirm overlay | **MET** | `tests_new_ui.rs:299 an_open_help_overlay_closes_on_esc_without_arming_abort_item16`, `:339 y_and_n_are_honoured_only_inside_the_confirm_overlay_item16`, `:403 every_confirm_action_draws_its_surface_last_over_the_body_item16`, `:612 a_confirm_over_the_help_legend_returns_to_the_legend_on_esc_item16`; `tests.rs:5262 esc_closes_an_open_overlay_without_arming_then_arms_abort` |
| 17 cancel one worker → exactly one worker path, `orchestrator::cancel_all()` not called | **MET** (fixed by `5d30439`) | The prescribed vehicle now exists: `tests/test_session_restore_panel_abort.rs:2799 ac_a4_a_named_cancellation_reaches_one_worker_while_the_broadcast_reaches_every_one` — ran **ok** in gate #6 (that binary: 11 tests, all green). It witnesses fired/unfired per-worker `CancellationToken`s and registry `Aborted` vs `In Progress`, i.e. behaviour, not source text. The request half (`x`, `/cancel <id>` → `CancelWorker{key,task_id}`; `/cancel-all` → `CancelAll`) is pinned in-crate at `tests_new_ui.rs:813/902/1000`, necessarily so because `ConfirmAction` and `apply_terminal_event` are `pub(crate)`; the reason is documented at `tests/test_session_restore_panel_abort.rs:2770-2798` |
| 18 restore round-trips `view_mode`, focus incl. input, follow, D2 queue/paused; witness warning = one transcript line | **MET** (fixed by `4d71e32`) | `tests_new_ui.rs:1139 capture_and_restore_repaints_mode_focus_follow_and_the_d2_pair_item18`, `:1281 a_pre_d2_view_state_restores_from_the_fields_it_actually_has_item18`, `:1372 a_disagreeing_token_witness_arrives_as_one_transcript_line_item18`; `tests.rs:3919 capture_ui_and_restore_panes_round_trip_mode_focus_and_follow`; format layer `src/session/format/ui.rs:305 view_mode`, `:306 follow`, `:362 FOCUSED_PANELS = ["chat","input","plan","subagents"]`, `:404` `VIEW_MODES` validation, tests at `:604` and `:638`; `FocusPane::legacy`/`from_legacy` now carry an `Input` spelling |
| 19 no zero-sized pane at 100x30/90x24/60x16/40x10; 39x9 too-small notice; nothing drawn outside its rect | **MET** | `tests_new_ui.rs:1424 no_pane_is_zero_sized_overlapping_or_outside_its_rect_item19` (the five sizes, pairwise `!overlaps`, `contains`, no cells outside every pane), `:1489 below_the_floor_only_the_too_small_notice_is_drawn_item19`, `:1546 the_degradation_ladder_drops_panes_in_spec_order_item19`; `tests.rs:4198 no_pane_rect_is_zero_sized_at_any_supported_terminal_size`, `:4227 below_40x10_only_the_too_small_notice_is_drawn` |
| 20 source hygiene: no `MouseEvent` arm, no executed `EnableMouseCapture`; mouse-disable count pinned | **MET in code and by its pin; the item's own prose has drifted again (D6)** | `tests_new_ui.rs:2099 no_mouse_arm_and_no_mouse_binding_survive_in_the_ui_sources_item20` asserts `events.rs` mentions none of `MouseEvent`/`EnableMouseCapture`/`Mouse` and that `mod.rs` code carries **exactly 2** `DisableMouseCapture` tokens (import + the one shared burst); `:2203 a_mouse_report_changes_nothing_on_screen_item20`; `tests.rs:5813 a_mouse_report_is_ignored_and_moves_nothing`. Actual sites at this HEAD: import `mod.rs:24`, single burst inside `write_terminal_resets` `mod.rs:3053`, reached from `shutdown()` `mod.rs:2231` and `leave_alt_screen()` `mod.rs:3073`. §10.20's sentence "three code occurrences in total … `mod.rs:2124` … `mod.rs:2899`" is **stale** (the 3→2 change came with `097a4a1`) — see D6 below |
| 21 cadence constants unchanged; idle renderer performs no `terminal.draw` | **MET** | `tests_new_ui.rs:2242 the_cadence_constants_are_the_unchanged_spec_ones_item21`, `:2279 an_idle_clean_renderer_performs_no_terminal_draw_item21`; constants live: `events.rs:310 event::poll(Duration::from_millis(30))`, `mod.rs:1414/2141` 25 ms dirty floor, `mod.rs:1418` 125 ms spinner tier, `mod.rs:1182` 3 s arm window |
| (a) clippy/build clean with **zero** new `#[allow]` | **MET** | Gate #3 (uncached, 0 diagnostics) + gate #8 (0 added, 6 removed) |
| (b) "the four moved plan-parsing helpers compile from their new home and `plan_parse` tests still pass" | **PARTIALLY MET — recorded deviation** | `plan_parse`'s tests pass (gate #5, all `plan_parse` tests green), but the helpers never moved: `visual_line_offset_of_first_pending` / `extract_plan_line_task_id` / `line_matches_task_id` / `visual_line_offset_of_task` are still `src/ui/tui/formatting.rs:998,1028,1034,1040` and `src/plan_parse.rs:2746,2762,2767,2775` calls **into** `crate::ui::tui::formatting::*`; `clamp_warning_sentences` is still `src/ui/tui/mod.rs:2921` called from `src/ui/session.rs:1547`, and `src/ui/notes.rs` does not exist. The one collision the item cared about **is** resolved: a single surviving grammar owner `crate::plan_parse::line_matches_task_id` (`src/plan_parse.rs:593`) with `formatting.rs:1034` a thin delegate, boundary documented at `src/plan_parse.rs:573` and `docs/legacy_ui_removal.md:37-52`. Net: dependency direction is inverted vs §9.1, harmless at build time, but the item as written is not met |
| (c) `val N` absent while D1 is undone | **MET (clause moot, invariant intact)** | D1 landed: `src/ui/mod.rs:106 validation_rounds`, filled at `src/ui/tui/mod.rs:207`, projected by `max_validation_rounds` `mod.rs:1046` into the header/status at `mod.rs:752`. Pin: `header.rs:515 the_val_chip_prints_the_projected_round_and_never_a_round_word` and `:552 the_val_chip_is_the_first_to_leave_the_narrow_header`; no "round" word ever renders |

**Matrix roll-up: 21/21 acceptance items MET; additional gate (a) MET, (c) MET, (b) PARTIALLY MET
(documented ownership deviation, no build or test cost).** No acceptance item is red at this HEAD.

Runtime-smoke verdict folded in (`docs/runtime_smoke_report.md`, captured at `d8cb75d`, one commit before
this HEAD and with the same test counts — ui:: 560/0, lib 1508/4, integration 362/1/2 — re-confirmed here
at `9726d26`): the frame matrix (§3) is **PASS on every row except row 13, the sticky error line, which is
PARTIAL** (behaviour pinned, no `TestBackend` frame pin — see §9 risk 4); the §2.5 degradation ladder verdict
is **PASS**; §6 confirms `--help`, `--config`, the session-flag validator and the raw/headless path still
work; §7 states the live interactive launch is **UNTESTED** here; §8 records an out-of-band 80x24 repaint of
the rebuilt UI on a real stream. `tests/test_new_ui_runtime_smoke.rs` contributes 8 public-seam tests inside
the 362.

---

## B.4 Defect ledger D1–D11 — closure status at HEAD `9726d26`

| Defect | Status at HEAD | Fixing commit (message verified to exist and match) | Independent verification at this HEAD |
|---|---|---|---|
| **D1** fmt gate red, branch-authored | **FIXED** | `c6794a6` — `chore(ui,docs): clear the fmt gate, correct the stale spots in the spec (t-017a)` | `cargo fmt --all -- --check` exit 0, 0 bytes output (gate #1) |
| **D2** persistence delta §9.4 half-implemented; item 18 "focus incl. input" fails | **FIXED** | `4d71e32` — `fix(session,ui): persist mode, pane focus and follow through a restore (D2, t-017b)` | `FOCUSED_PANELS` gained `"input"` (`session/format/ui.rs:362`); `ViewState.view_mode/follow` at `:305/:306` with pre-D2 compatibility (`:321`); the old test that *asserted the loss* is gone; item-18 pins green (`tests_new_ui.rs:1139/1281`) |
| **D3** submit silently re-armed follow | **FIXED** | `684de35` — `fix(ui): submitting never re-arms transcript follow (D3, spec §10 item 14)` (amend of the unreachable `3290cdf`) | No `chat_auto_scroll` write left in `events.rs` outside its own tests; single mirror writer `mod.rs:1280`; four new pins `tests_new_ui.rs:2399/2475/2537/2618` |
| **D4** 40-column header contradiction + coverage hole | **FIXED** | `dc7712f` — `test(ui): pin the active task id at the 40-column header floor (D4)` | `header.rs:651` asserts the task id at every width 80→40; `:613` the 40x1 frame; `:705` the over-long-id fit pass is the only casualty; spec §10.1 amended with the §7.4 resolution |
| **D5** frame-time global token read made frame tests process-global | **FIXED** | `d8cb75d` — `fix(ui): inject the frame's token snapshot instead of reading the global (D5)` | exactly one reader remains, `mod.rs:680` inside `refresh_global_token_snapshot` (`mod.rs:679`), sampled once per frame into `global_tokens_in/out` (`mod.rs:426/429`); frame assembly merges renderer state only |
| **D6** stale line numbers in the binding contract | **PARTIALLY FIXED — re-drifted, still open (Low, docs)** | `c6794a6` amended §10.20 to be line-number-independent and pinned the count by test | The amended item now says "**exactly two** teardown sites … three code occurrences in total … Verified … `mod.rs:2124` … `mod.rs:2899`", but since `097a4a1` the burst is a **single shared** `write_terminal_resets` (`mod.rs:3049-3057`) → the pin asserts **2** code occurrences (`tests_new_ui.rs:2099`), and the quoted lines are not the sites (`shutdown` `mod.rs:2231`, `leave_alt_screen` `mod.rs:3073`). Behaviour and its test pin are right; the contract sentence is stale again. Recommendation: re-amend §10.20 once, dropping the numbers it already declares it will not depend on |
| **D7** §10.15 named a non-existent file | **FIXED** | `c6794a6` | §10.15 now names `components/activity.rs` + `components/plan_pane.rs` and states there is no `components/workers.rs` (true: no such file); the sanctioned clamp is at `layout.rs:289`, exactly as the item says |
| **D8** item 17 verified by grepping `events.rs`, not by the prescribed integration vehicle | **FIXED** | `5d30439` — `test(ui): pin acceptance item 17 by behaviour, not by grepping events.rs (D8, t-017e)` | `tests/test_session_restore_panel_abort.rs:2799 ac_a4_…` drives a real `TuiRenderer` through the public exit/cancel seam and asserts per-worker token fired/not-fired and registry status; green in gate #6. The in-crate half no longer rests on `fn_body` string analysis |
| **D9** §3 confirm-request inventory drifted from the code | **FIXED** | `9165143` — `ui: fix chip clipping, status-bar field ladder and roster rank; add preview evidence` (`git log -S ClearTranscript -- docs/ui_design_spec.md` → this commit) | Spec §3 now lists exactly `Quit | CancelWorker{key,task_id} | CancelAll | Command{line}` and says outright there is no `ResetPlan` and no `ClearTranscript` surface (`docs/ui_design_spec.md:166,171-172`); the code enum matches those four and only those four |
| **D10** unknown long options swallowed as the prompt | **NOT FIXED — pre-existing, declared out of scope** (see B.5) | n/a (`src/main.rs` has **zero** commits on this branch) | Repro recorded in `docs/runtime_smoke_report.md` §6.1 and re-derivable from the code at `src/main.rs:324-330` |
| **D11** unit tests wrote terminal-reset sequences to the ambient stdout | **FIXED** | `097a4a1` — `fix(ui): stop test-dropped renderers resetting the ambient terminal (D11)` | `terminal_owned` guard + `take_terminal_resets()` + one shared `write_terminal_resets()`; pins `tests_new_ui.rs:2158 no_unit_test_reaches_the_ambient_terminal_teardown_item_d11`, `tests.rs:4058 teardown_writes_exactly_the_reset_burst_to_the_writer_it_is_given`, `tests.rs:4088 a_renderer_that_never_took_the_screen_over_writes_no_resets_on_teardown`; **measured here: 0 ESC and 0 CR bytes in the 136 176-byte captured lib-test stdout** (Part A measured 2 776 ESC bytes; the fix commit itself quotes 3 448 pre-fix) |

Roll-up: **9 fixed (D1, D2, D3, D4, D5, D7, D8, D9, D11), 1 open at documentation severity (D6 prose
re-drift), 1 pre-existing and out of scope (D10).** No functional defect from Part A remains unfixed.

---

## B.5 D10 disposition (explicit)

**Disposition: pre-existing defect in `src/main.rs`, out of scope for the UI rebuild, recorded and
deliberately not fixed on this branch.**

* Site: the catch-all arm of `parse_args()` at **`src/main.rs:324-330`** —
  `other => { if args.prompt.is_none() { args.prompt = Some(other.to_string()) } else { eprintln!("ignoring extra argument: {other}") } }`.
  There is no `--version` arm.
* Out-of-scope proof, not assertion: `git log --oneline b66f669..HEAD -- src/main.rs` is **empty**; the file
  is byte-identical to the merge base, so the UI branch cannot have caused or worsened it.
* Reproduction evidence already recorded in `docs/runtime_smoke_report.md` §6.1: without a readable user
  config, `./target/debug/marmel --version` exits 1 with
  `Error: reading /home/fredrik/.marmel/marmel.toml / Permission denied (os error 13)`; with a readable
  `--config`, the same command **starts a session** — `[status] Running (…)`, an assistant turn, an LLM
  transport error, `[done]`, exit 0. Nothing prints a version, nothing reports an unknown option. The same
  shape applies to any unrecognised long option.
* Effect on this branch's deliverable: none on the UI surface — the headless/raw path and the frame path are
  unaffected, and the CLI probes in the smoke report §6 show `--help`, `--config`, `--raw` and the
  session-flag validator behaving as designed.
* **Recommendation for a future ticket (one line):** add an explicit `--version` (and a
  `--version/-V` constant from `CARGO_PKG_VERSION`) arm to `parse_args()` and make the catch-all reject any
  unrecognised `--`-prefixed token with exit 2 instead of promoting it to the session prompt.

---

## B.6 Residual risks for a human to eyeball on a real terminal

Stated plainly first: **no live interactive launch of `marmel` was possible in this sandbox.**
`openpty` is denied (`python3 -c "import pty; pty.openpty()"` → `OSError: out of pty devices`;
`script -qec "true" /dev/null` → `failed to create pseudo-terminal: Permission denied`), which is also why
exactly four `harness::pty` lib tests and `test_prompt_based_tool_gating` are red here. **No interactive
success is claimed anywhere in this report.** Everything asserted about the running UI rests on
`TestBackend` cell-level frames, in-crate state pins, one public-seam integration file, and an out-of-band
80x24 repaint that belonged to another session (§8 of the smoke report).

What a human should look at on a real device:

1. **Live PTY / raw mode / alternate screen.** `init()` (`enable_raw_mode`, `EnterAlternateScreen`,
   `PushKeyboardEnhancementFlags`, `EnableBracketedPaste`) and `shutdown()`/`leave_alt_screen()` never ran
   against a device here. Confirm the shell is sane after quitting with `y` — no leftover alt screen, mouse
   or bracketed-paste modes, cursor visible, no `reset` needed. This is the D11 guarantee, and it is pinned
   only by in-crate source/behaviour tests, never by a real teardown.
2. **Key decoding.** `Tab`/`Shift+Tab` under keyboard-enhancement flags, `Alt+Enter`/`Shift+Enter` newline,
   `Ctrl+T/F/A/P/D/C`, `F1`/`F2`/`F3`/`F4`/`F5`, `?` on an empty composer vs. typed, `Esc` arm/dismiss —
   every one is pinned by *synthesised* `KeyEvent`s, not by bytes arriving from a terminal. Whether the
   disambiguated forms actually arrive (and whether a terminal's own `Esc`-timing beats the 3 s arm window)
   is unverified.
3. **Resize / SIGWINCH.** The §2.5 ladder is pinned by calling `compute_layout`/`apply_terminal_event` with
   fabricated sizes at 100x30 / 90x24 / 89x30 / 70x30 / 69x30 / 60x16 / 60x30 / 59x30 / 100x19 / 40x10 /
   39x9. Live drag-resizing (rapid intermediate sizes, a resize during a streaming paint, the too-small
   notice appearing and clearing without leftover chrome) has never been exercised.
4. **The disclosed pinning gaps.** (a) **The §8.5 sticky-error line** (`✗ <cause>` +
   `see .marmel/marmel.log`) and the armed-abort prompt at the tiniest sizes are pinned **behaviourally
   only** — no `TestBackend` frame asserts the drawn lines, because they are reached through `pub(crate)`
   keys (smoke report §3 row 13, §9 risk 4). (b) **Overlay and focus variants are pinned only at 100x30**:
   29 of the 36 `tests_new_ui` frame pins draw 100x30; 80x24/60x16 carry ~5 pins each (geometry, scroll
   clamp, scroll-chip-once, status-row clipping, queued rank). Eyeball each confirm overlay, the help
   overlay and every focus state at 60x16 and 40x10.
5. **`draw_new_ui` / `apply_terminal_event` are `pub(crate)`.** Frame evidence therefore lives only in-crate;
   no `tests/` file (and no third-party consumer) can paint a frame or press a key. That encapsulation is
   deliberate, but it means "smoke-test the frames" can never be reproduced from outside the crate, and the
   8 public-seam tests in `tests/test_new_ui_runtime_smoke.rs` cover the pane model and text pipeline only —
   not pixels. Consider a feature-gated `pub` draw entry point if out-of-crate frame evidence is ever needed.
6. **Fenced content is routed to markdown before the ANSI bridge.** `components/transcript.rs::stream_lines`
   sends anything containing ` ``` ` through `render_markdown_lines` first, so **raw SGR bytes can survive
   into `Line` spans** (the smoke test records it: `SMOKE text[ansi]: markdown carries 1 line(s) with raw
   SGR bytes`). `TestBackend` renders them as ordinary cells; a real terminal may *interpret* them — coloured
   worker output inside a fenced block is the case to look at, along with the related cosmetic effects
   (escape bytes counting toward wrap arithmetic; whitespace-only payloads still consuming rows).
7. **Truncation legibility.** §7.4 deliberately cuts names and chips mid-token with `…`
   (`coder_specialist-…`, `Es…`, `3a 1p 1q`). The verified property is "marked and never outside the rect",
   not "readable" — judge 60x16 and 40x10 by eye, plus a font without box-drawing glyphs for the
   `● ⧗ ▶ ⏸ ✓ ✗ ↓` set and the `─ │ ┌ ┐ └ ┘` chrome.
8. **Colour.** Styles are asserted only through `TestBackend` cell attributes; true-colour vs. 16-colour
   fallback and contrast in a real palette are unproven.
9. **Cadence under load.** The 30 ms poll / 25 ms dirty floor / 125 ms spinner / 3 s arm / 250 ms plan-mtime
   constants are pinned as constants and as "no draw while idle", never against a real draw loop's latency.
10. **The four PTY tests and the role-gating test are unproven, not proven green.** Re-run
    `cargo test --lib harness::pty` and `cargo test --test test_role_gating` on a host with PTY access
    before calling the suite "all green"; both sit in code this branch never edited.

---

## B.7 Verdict

`feature/new-ui` @ `9726d26` is **certified deliverable** on the evidence above: the fmt gate is green, the
build and clippy gates are green uncached, the UI suite is 560/0, the lib suite is 1508/4 with all four
failures environmental and pre-branch, the integration suite is 362/1/2 with the one failure environmental
and pre-branch, no `#[allow]` was introduced, no dependency changed, 21/21 §10 acceptance items are met, and
the Part A defect ledger is closed except D6 (documentation prose drift) and D10 (pre-existing, out of
scope, disposition recorded in B.5).

The single non-conformance with a binding instruction is §9.1's helper-ownership move (additional gate (b)),
which was answered with an in-place ownership consolidation instead of a file move and is documented in
`docs/legacy_ui_removal.md` and at `src/plan_parse.rs:573`; it should be either accepted as an amendment to
§9.1 or scheduled, not left silent.

Nothing in this audit certifies live terminal behaviour: the sandbox denies PTY allocation, so the interactive
surface is unverified by construction and is handed to the human checklist in B.6.
