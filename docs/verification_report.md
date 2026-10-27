# Verification report — `feature/new-ui` (plan task t-012b)

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
