# Runtime smoke verification — rebuilt terminal UI (t-018)

* Branch: `feature/new-ui`
* HEAD verified before and during the run: `d8cb75da7ea30fb43931e0301c47daa26b572380`
  (`fix(ui): inject the frame's token snapshot instead of reading the global (D5)`)
* Crate / bin: library `marmennill`, binary `marmel` (edition 2024)
* Captured: 2026-10-27 (UTC), inside the marmel sandbox on this host
* Auditor role: read-only for `src/`. The only files added by this task are
  `docs/runtime_smoke_report.md` (this file) and `tests/test_new_ui_runtime_smoke.rs`.
  No file under `src/` was modified (`git status` after the run: only these two paths).

Every claim below is backed by a command that was actually run in this sandbox and
whose output is quoted, or is explicitly labelled **UNTESTED**.

---

## 1. Environment and sandbox limitations

| Property | Observed value | How it was established |
|---|---|---|
| OS / arch | Linux x86_64, 56 logical CPUs, load average ≈ 20–25 during the run (other marmel sessions share this host) | `nproc`, `uptime` |
| Rust toolchain | stable (`rustup` default), edition 2024 | `cargo build` output |
| Pseudo-terminals | **not available** | see below |
| Real LLM backend | `http://localhost:8000/v1` is **not** listening in this sandbox | headless run output, §6 |
| User config `~/.marmel/marmel.toml` | present but **unreadable** (`Permission denied`) | CLI probe, §6 |
| Workspace under test | `/home/fredrik/marmel/wip`, git worktree clean apart from untracked `.marmel.old*` / `docs.old/` (never committed) | `git status` |

### 1.1 The PTY denial, established once and cheaply

```
$ python3 -c "import pty; pty.openpty()"
openpty FAILED: OSError out of pty devices

$ script -qec "true" /dev/null
script: failed to create pseudo-terminal: Permission denied

$ ls -l /dev/ptmx
crw-rw-rw- 1 root tty 5, 2 Oct 27 09:29 /dev/ptmx
```

The same denial is what the crate itself hits, verbatim, in the four environmental
unit-test failures listed in §5.2:

```
spawn sandbox: Execution(failed to openpty: Os { code: 13, kind: PermissionDenied, message: "Permission denied" })
spawn session: Execution(Failed to create PTY: failed to openpty: Os { code: 13, kind: PermissionDenied, message: "Permission denied" })
```

**Consequence:** an interactive `marmel` launch (raw mode + alternate screen + keystrokes)
cannot be started or driven here at all. See §7 (UNTESTED).

### 1.2 An API boundary that shapes the method

The only draw path is `render::draw` → `TuiRenderer::draw_new_ui`, and it is
`pub(crate)` (`src/ui/tui/mod.rs:730`); so is the keystroke seam
`TuiRenderer::apply_terminal_event` (`src/ui/tui/events.rs:334`) and
`on_key_event`, `set_view_mode`, `set_focus`, `set_follow`. Therefore **a `tests/`
integration test (external crate) cannot paint a frame or press a key at all** — the
whole-frame `TestBackend` evidence can only come from the in-crate suites
(`src/ui/tui/tests_new_ui.rs`, `src/ui/tui/tests.rs`, `src/ui/tui/components/*`).
That is why this report grades the frame matrix from those suites (they are run
here, not merely cited) and why the added `tests/test_new_ui_runtime_smoke.rs`
drives the **public** seam (`Renderer` trait + `ui::tui::formatting`) instead.

### 1.3 One hazard observed while capturing

`cargo test` output and the TUI of *another* marmel session on this host share one
stdout stream, so captured logs are contaminated with live escape-code frames
(`SMOKE …` lines interleaved with `\x1b[<row>;<col>H` runs). It cost nothing but it
is worth knowing: **captured terminal evidence in this sandbox must be filtered**,
and it is the reason §8 quotes cursor-addressed rows rather than claiming a clean
screen scrape.

---

## 2. Method

1. **Frame-level evidence** — the in-crate `TestBackend` suites were *executed* in
   this sandbox (`cargo test --lib ui::` = 560 tests, all green, §5) and the
   scenario-specific pins quoted by name with their verdict (`… ok`). Each row of
   §3 names the test(s) that draw the scenario, the viewport they draw it at, and an
   excerpt of what that test asserts.
2. **Fresh public-seam smoke** — `tests/test_new_ui_runtime_smoke.rs` (added here,
   8 tests, no pty, no raw mode, no alternate screen) drives a real `TuiRenderer`
   through `marmennill::ui::Renderer` only: boot state, a streaming flood with hostile
   payloads, a 50-row roster, capture→restore round trips, steering traces, error
   traffic, the abort/exit/gating seams, and the public text pipeline at
   100/80/60/40/39 columns.
3. **Recorded frames** — `docs/new_ui_preview.txt` (deterministic `TestBackend` dumps
   at 100x30, 80x24, 60x16, 40x10, 39x9) is cited for what a frame *looks like*, and
   those same sizes are re-pinned by tests executed here (§4).
4. **Non-TUI CLI** — the built binary `target/debug/marmel` was run with `--help`,
   `--version`, `--raw`, `--config`, `--resume --fresh` (§6).
5. **PTY/live TUI** — attempted once, denied (§1.1); declared untested (§7).

Commands used (all in `/home/fredrik/marmel/wip`):

```
cargo fmt --all -- --check                      # exit 0
cargo build                                     # exit 0, "Finished dev profile"
cargo clippy --all-targets -- -D warnings       # exit 0, no warnings
cargo test --lib ui::                           # exit 0   (560 passed)
cargo test --lib --no-fail-fast                 # exit 101 (1508 passed, 4 failed)
cargo test --test '*' --no-fail-fast            # exit 101 (362 passed, 1 failed, 2 ignored)
cargo test --test test_new_ui_runtime_smoke -- --nocapture   # exit 0 (8 passed)
./target/debug/marmel --help | --version | --raw | --config … # §6
```

---

## 3. Frame matrix — scenario × viewport → verdict

Legend for verdicts: **PASS** = drawn and asserted by a test that ran green here;
**PASS (public seam)** = asserted by the new `tests/` file (no frame, pane model /
text pipeline only); **PARTIAL** = behaviour pinned but not as pixels; **UNTESTED** =
no evidence obtainable here.

| # | Scenario | Viewport(s) | Verdict | Evidence excerpt (observed in this sandbox) |
|---|---|---|---|---|
| 1 | View mode inventory M1–M4 | 100x30 | PASS | `ui::tui::tests_new_ui::every_view_mode_shows_its_own_pane_inventory ... ok`; companion `an_absent_pane_is_never_split_never_drawn_and_never_announced ... ok` (100x30) |
| 2 | Default / idle boot | 100x30 (+ public seam at any size) | PASS | `the_plan_pane_states_the_absent_plan ... ok`; preview *case a* (idle boot, 100x30) row 3 `│No output yet. Type an instruction and press Enter (? for keys).`; new test: `SMOKE boot: status_line="Ready" mode="transcript" focus="input" follow=Some(true)` |
| 3 | Busy / streaming (content + thought + steer + tool traffic) | 100x30 (frames), public seam | PASS | `ui::tui::components::transcript::tests::streaming_content_and_steer_regions_are_drawn ... ok`; preview *case b* shows `│Streaming: the status bar chips ar` inside the transcript body; new test: `SMOKE busy: chat_lines=4 worker_rows=14 status_line="Delegating to specialist validator-t-003 for t-003"` |
| 4 | Plan pane visible (mixed checked/unchecked, active task) | 100x30, 80x24 | PASS | `the_plan_pane_draws_done_active_and_pending_rows ... ok`, `the_plan_pane_shows_an_archived_plan ... ok`; live frame §8 rows 11–18: `┌ Execution Plan ────…─15/17 done ▶ t-003 ┐` … `└ ✓ 15 done ⧗ 2 open ▶ in flight  scroll 79 ─…┘` |
| 5 | Workers roster + detail (running / queued / paused) | 100x30, 60x24, 59x30 | PASS | `the_workers_tally_keeps_the_done_count_the_position_and_the_detail_chip ... ok`, `a_held_stream_is_labelled_paused_and_keeps_its_live_shape ... ok`, `a_queued_row_keeps_its_rank_while_thinking_and_output_stream_into_it ... ok`; new test: `SMOKE roster: rows=50 queued=17 paused=10` (50 rows survive projection + round trip) |
| 6 | Focus ring — every pane in turn, exactly one badge | 100x30 | PASS | `the_tab_focus_ring_walks_wraps_and_marks_the_focused_pane ... ok`, `shift_tab_walks_backwards_and_the_ring_skips_panes_the_mode_removed ... ok`, `the_focused_pane_badges_its_title_and_every_other_pane_does_not ... ok`, `a_mode_that_removes_a_pane_repairs_the_focus_onto_a_pane_that_is_there ... ok`; live frame row 21 carries the composer badge: `┌ Input (…) ● ─…┐` |
| 7 | Confirm overlay — `Quit` (Ctrl+D) | 100x30 | PASS | `every_confirm_action_draws_its_surface_last_over_the_body_item16 ... ok` iterates `Quit (Ctrl+D)` and asserts the two lines `Quit the session (Ctrl+D)` / `Ends the session and cancels every running worker.` over the body |
| 8 | Confirm overlay — `CancelWorker` (x) | 100x30 | PASS | same test, case `CancelWorker (x)`: `Cancel worker 'coder_specialist-t-004'` / `Stops that one worker; every other worker keeps running.`; companion `x_asks_about_the_selected_row_and_only_that_row_item17 ... ok` |
| 9 | Confirm overlay — `CancelAll` (/cancel-all) | 100x30 | PASS | same test, case `CancelAll (/cancel-all)`: `Cancel every worker (/cancel-all)` / `Cancels every running worker and every in-flight stream.` |
| 10 | Confirm overlay — `Command` (/reset) | 100x30 | PASS | same test, case `Command (/reset)`: `Reset the execution plan (/reset)` / `Clears plan, session bundle, steering history and transcript, and tells the model.` |
| 11 | Overlay lifecycle (Esc closes, y/n only inside overlay, confirm-over-help returns to help) | 100x30 | PASS | `an_open_help_overlay_closes_on_esc_without_arming_abort_item16 ... ok`, `y_and_n_are_honoured_only_inside_the_confirm_overlay_item16 ... ok`, `a_confirm_over_the_help_legend_returns_to_the_legend_on_esc_item16 ... ok` |
| 12 | Help overlay + legend rows | 100x30 (overlay), 80x24 (preview + live) | PASS | `components::help::tests::overlay_prints_the_table_and_the_close_hint ... ok`, `legend_rows_are_the_only_source_of_the_footer_and_the_help_body ... ok`, `every_legend_row_is_a_real_handler_path ... ok`, `clipped_overlay_points_at_the_full_table ... ok`, `overlay_is_centred_and_clamped_to_80_percent ... ok`; preview *case c* (80x24) shows the `┌ Keys ─…┐` overlay over the body; live frame row 24: ` F1 help  Tab focus  F2 mode  F3/F4/F5 panes  x cancel-worker  e last error  Es…` |
| 13 | Sticky error line (§8.5) | pane model; no frame pin | **PARTIAL** | Behaviour green: `a_reported_error_stays_recallable_after_the_status_line_moved_on ... ok` (recalled text starts `✗ rate limit exceeded\nLLM error:`), `e_recalls_the_last_error_or_reports_what_is_missing ... ok`, `only_error_traffic_is_remembered_and_repeats_do_not_restamp_it ... ok`, `the_error_command_recalls_what_the_e_key_recalls ... ok`. New test: `SMOKE error-line: transcript keeps Some("LLM error: http (transport error: error sending request for url (http://")`. Gap: the §8.5 buffer + `e`-recall are reached through `pub(crate)` keys, so **no `TestBackend` frame asserts the drawn `✗ …` line** (see §9 risk 4) |
| 14 | Queued steering | pane model (public seam) | PASS (public seam) + PASS (in-crate) | `delegation_queued_and_running_move_only_the_queue_field ... ok`, `a_re_queued_row_follows_every_rank_edge_in_its_label_and_its_bar ... ok`, `queue_position_updates_never_move_the_selection_or_the_list_window ... ok`; new test feeds `note_steer` for `Queued`/`Delivered`/`Revoked` × empty/ANSI/3 000-char CJK: `SMOKE steering: chat_lines=9 status="Ready"`, no panic |
| 15 | Geometry: no zero-sized pane, no overlap, nothing drawn outside its rect | 100x30, 90x24, **80x24**, **60x16**, 40x10 | PASS | `no_pane_is_zero_sized_overlapping_or_outside_its_rect_item19 ... ok` — loops exactly `[(100,30),(90,24),(80,24),(60,16),(40,10)]` and asserts `leaf_rects` pairwise `!overlaps`, `contains`, and `cells_outside_every_pane(...).is_empty()`; companion `no_pane_rect_is_zero_sized_at_any_supported_terminal_size ... ok` at 100x30/90x24/60x16/40x10/120x60 |
| 16 | Content not blank (idle and busy) | 100x30, 80x24, 60x16, 60x20, 40x10 | PASS | preview cases a–e are non-empty dumps at those sizes; `a_shrunken_pane_clamps_the_scroll_offset_it_can_no_longer_honour ... ok` draws a 200-line transcript at 100x30 / 60x20 / **40x10** / 120x40 and asserts the frame `contains("pane line 199")` — the body is never blank rows; `the_scroll_state_chip_is_drawn_once_in_the_transcript_pane ... ok` (100x30/80x24/60x16/40x10, busy fixture, exactly one `LIVE` chip on the transcript border); `the_status_row_keeps_every_count_and_clips_names_into_the_free_columns ... ok` (100x30/80x24/60x16) asserts the status row keeps every count and clips only names; new tests assert a non-blank status line and a non-empty transcript after the flood |
| 17 | No stray escape bytes in rendered content | 100/80/60/40/39 cols | PASS (public seam) | new test `the_text_pipeline_holds_at_every_smoke_viewport_width ... ok`: for 11 hostile payloads (SGR + OSC-8 escapes, 4 000-char word, CJK, emoji ZWJ + flags, CRLF+tabs, NUL/BEL/BS, fences, empty, whitespace, lone `\n`) every `Line` span of `render_message_lines` and `parse_ansi_lines` is asserted `!contains('\u{1b}')` at all five widths |
| 18 | No line overruns its rect / clipping is marked | 100x30 … 40x10 | PASS (rect rule) / **PARTIAL** (word-break is a taste call) | the `cells_outside_every_pane` rule above; the recorded frames show clipping is always ellipsis-marked, never silent: `workers/…`, `coder_specialist-t-004 3…`, `coder_specialist-… | elapsed 10m 12s`, legend `Es…`. Mid-token cuts do exist **by design** for chips/names (§7.4), so a blanket "never mid-word" claim would be false — graded PARTIAL and put in §9 risk 2 |
| 19 | Live repaint actually reaches a terminal stream | 80x24 (host session, piped stdout) | PASS (out-of-band) | §8 quotes the decoded cursor-addressed rows of a real 80x24 frame emitted by the rebuilt UI in this sandbox |

**Naming note (kept deliberately — nothing was renamed):** the sticky-error tests in `src/ui/tui/tests.rs` — `e_recalls_the_last_error_or_reports_what_is_missing` (`:6117`), quoted in row 13, and its sibling `an_error_recalled_with_e_survives_a_transcript_clear` (`:4724`) — still spell the removed bare-`e` mnemonic in their **names**, while the presses they drive now submit `/error` (the bare `e` binding is gone; see `docs/new_ui.md` §5.7).

---

## 4. Degradation ladder (`docs/ui_design_spec.md` §2.5) at 60x16, 40x10, 39x9

All of the following tests were **run in this sandbox and are green** (`cargo test
--lib ui::` → 560/0). Viewports listed are the ones each test actually draws.

| Ladder step | Viewports drawn | Evidence excerpt |
|---|---|---|
| `width < 90` → single column, the *unfocused* side pane disappears and the chip stops lying | 90x30, **89x30** | `the_degradation_ladder_drops_panes_in_spec_order_item19 ... ok`: at 90 all three body panes; at 89 `at_89.transcript.is_some() && at_89.plan.is_some() && at_89.workers.is_none()` and the status row must contain `workers:narrow` |
| `width < 70` → header drops `val` → `ctx` → `elapsed` → mode name; short legend | 70x30, **69x30** | same test: at 70 header contains `ctx`, `elapsed`, `M4 Split`; at 69 `!contains("ctx") && !contains("elapsed") && !contains("M4 Split")` while `Marmel`, `3/7 done` and `t-004` **stay** (G1 + §7.4: the task id is never a budget casualty); footer still present and contains `F1 help`, and `!footer_row.contains("Tab focus")` → the short legend form |
| `rows < 20` → status collapses to 1 row, footer dropped, composer clamps to 1–3 rows | 100x20, **100x19** | same test: `status.height == Some(2)` at 20 rows, `Some(1)` at 19 rows with `Tokens` still on the row, `footer.is_none()` at 19 rows, `input.height <= 3` |
| `width < 60` → Workers **detail** absent (roster only); M2/M3 revert to a single column | 60x30 boundary check, **59x30**, 60x16, 60x24 | same test: `layout::workers_detail_present(60) == true`, `== false` for 59; at 59 with M3: `workers.is_some() && transcript.is_none()` and `workers.width == body.width`; `no_pane_rect_is_zero_sized_at_any_supported_terminal_size` and the scroll-chip / status-row pins all draw at 60x16 |
| `width < 40 \|\| rows < 10` → too-small notice, nothing else drawn | **39x9**, 40x10 | `below_the_floor_only_the_too_small_notice_is_drawn_item19 ... ok`: `rects.notice == Some(Rect::new(0,0,39,9))`, header/status/input/footer/transcript/plan/workers all `None`, `rects.mode == ViewMode::Transcript` ("the ladder overrides the mode rather than inventing geometry"), screen contains `Terminal too small`, `40x10`, `currently 39x9`, and must **not** contain `Input`, `Tokens`, `Transcript`, `Execution Plan`, `Workers`; at 40x10 the notice is absent and header+status+input exist |

Frame-level ladder confirmation at the exact preview sizes, from
`docs/new_ui_preview.txt` (regenerated deterministically by the in-crate scenario):

* 100x30 header: ` Marmel M4 Split ✓ 3/7 done ▶ t-004 ctx 45% elapsed 10m 12s`
* 60x16 header:  ` Marmel ✓ 3/7 done ▶ t-004`  → mode name, `ctx`, `elapsed` all gone, `✓ 3/7` and the task id kept
* 60x16 status row: ` Tokens 12.4k in / 3.2k out | 3a | ctx 45% | 1p | 1q` → counts kept, roster/clock dropped
* 40x10 status row: ` Tokens 12.4k in / 3.2k out | 3a` → the ladder takes `ctx`, then the counts, last
* 40x10 transcript border: `└  ● LIVE  scroll 8/8 ─…┘` → the pane's own scroll chip survives the floor
* 39x9: exactly one pane, the centred notice, nothing else

**Verdict for the ladder: PASS.** It degrades field-by-field in the documented order
(`val` → `ctx` → `elapsed` → mode name in the header; roster/clock before counts in the
status row; footer then second status row as rows shrink) and never corrupts the frame:
no zero rect, no overlap, no chrome for a pane the ladder removed, and below the floor a
single notice pane.

---

## 5. Test gates — actual counts

### 5.1 The gate table

| Gate | Command | Exit | Result |
|---|---|---|---|
| Format | `cargo fmt --all -- --check` | 0 | clean, including the new `tests/` file |
| Build | `cargo build` | 0 | `Finished dev profile ... in 0.21s` |
| Lints | `cargo clippy --all-targets -- -D warnings` | 0 | zero warnings; no `#[allow]` added anywhere by this task |
| UI unit suite | `cargo test --lib ui::` | 0 | `test result: ok. 560 passed; 0 failed; 0 ignored; 952 filtered out` |
| Full lib | `cargo test --lib --no-fail-fast` | 101 | `1508 passed; 4 failed` — all 4 environmental (below) |
| Integration | `cargo test --test '*' --no-fail-fast` | 101 | 38 test binaries: **362 passed, 1 failed, 2 ignored**; the 1 failure is the pre-existing PTY-dependent `test_prompt_based_tool_gating` |
| New smoke file | `cargo test --test test_new_ui_runtime_smoke` | 0 | `8 passed; 0 failed` |

The new integration file is inside the 362 (8 of them). Excluding this task's own
tests, the pre-existing integration total is **354 passed / 1 failed / 2 ignored**.

### 5.2 The four red lib tests — all one environmental cause (PTY)

```
harness::pty::tests::test_harness_pty_process_group_kill            src/harness/pty_tests.rs:16:47
harness::pty::tests::test_run_command_parses_timeout_seconds_argument  :102:34
harness::pty::tests::test_interactive_pty_manager_lifecycle         :120:10
harness::pty::tests::test_run_command_pty_cancelled_by_worker_token :179:5
```

Verbatim causes: `Execution(failed to openpty: Os { code: 13, kind: PermissionDenied,
message: "Permission denied" })` (first three) and `assertion failed: res.is_ok()`
(the fourth, downstream of the same denial). These are the same four lines the task
brief lists; nothing else in the library is red.

### 5.3 Integration suite, per binary (run of this sandbox)

`test_agent` 2/0 · `test_call_context` 6/0 · `test_conditional_stream_pause` 7/0 ·
`test_context` 8/0 · `test_delegation_queue_events` 6/0 · `test_dynamic_prompts` 10/0 ·
`test_harness` 15/0 · `test_internal_fast_calls` 7/0 · `test_llm` 5/0 ·
`test_llm_pool` 10/0 · `test_midturn_notice` 4/0 · `test_monitor` 2/0 ·
**`test_new_ui_runtime_smoke` 8/0 (new)** · `test_orchestrator` 8/0 ·
`test_panel_paused_indicator` 6/0 · `test_plan_checkoff` 8/0 ·
`test_reasoning_effort` 6/0 · `test_rebirth_system_role` 4/0 ·
**`test_role_gating` 24/1 (RED)** · `test_session_abort_flush` 6/0 ·
`test_session_boot` 6/0 · `test_session_bundle` 22/0 · `test_session_commit` 9/0 ·
`test_session_config` 35/0 · `test_session_restore_dangling` 12/0 ·
`test_session_restore_manager` 8/0 · `test_session_restore_panel_abort` 11/0 ·
`test_session_restore_ui` 9/0 · `test_session_restore_workers` 8/0 ·
`test_specialist_schema_charge` 4/0 · `test_specialist_stream` 13/0 ·
`test_steer_history` 10/0 · `test_subpane_logs_repro` 3/0 (+2 ignored) ·
`test_subpane_queued_refresh` 5/0 · `test_tool_name_literals` 1/0 ·
`test_ui_session` 23/0 · `test_validation_loop` 23/0 · `test_validator_concurrency` 8/0

The single failure, verbatim:

```
test test_prompt_based_tool_gating ... FAILED
thread 'test_prompt_based_tool_gating' panicked at tests/test_role_gating.rs:310:5:
Validator with run_command in prompt must be permitted to execute run_command
```

It is the pre-existing red test the brief names. Mechanism confirmed rather than
assumed: that assertion dispatches the `run_command` tool, whose path goes through
`harness::pty` (`portable_pty::pty_system.openpty`, `src/harness/pty.rs:777,1183`),
which is the same PTY denial of §1.1 — i.e. environmental, not a UI or gating regression.

### 5.4 Known-flaky tests

None of the flaky-under-load set was red in this run:
`harness::dispatch_integrity_tests::test_notice_reply_round_trip_reaches_the_user_exactly_once`,
`orchestrator::notice_tests::test_broadcast_targets_still_route_to_any_worker`,
`harness::tool_resolution_tests::legitimate_mcp_tool_still_dispatches_to_the_mcp_path`,
`ui::tui::tests::the_armed_abort_prompt_occupies_the_same_surface_and_holds_no_request`,
`throttled_triggers_coalesce_and_mandatory_triggers_ignore_the_floor` — all `ok`.

---

## 6. Non-TUI / CLI surfaces still work

All runs used the freshly built `target/debug/marmel` (225 MB, same HEAD). `stdout` in
this sandbox is a pipe, so the binary selects raw/headless mode automatically
(`use_raw = args.raw || cfg.ui_mode == "raw" || !stdout_is_terminal()`).

| Invocation | Exit | Observed output (verbatim) |
|---|---|---|
| `marmel --help` | 0 | full usage block, `marmel — autonomous agentic coding assistant … -h, --help print this help`, empty stderr |
| `marmel` (no args, unreadable user config) | 1 | `Error: reading /home/fredrik/.marmel/marmel.toml` / `Caused by:` / `Permission denied (os error 13)` |
| `marmel --config /nonexistent.toml` | 1 | `Error: reading /nonexistent.toml` / `No such file or directory (os error 2)` |
| `marmel --config marmel.toml.example --resume --fresh` | 2 | `error: --resume and --fresh are mutually exclusive` — the session-flag validator still fires |
| `marmel --config marmel.toml.example --raw "say hello"` | 0 | `[status] Running (qwen-3.8-27b)` / `[assistant] ` / `[Error] LLM backend call failed (http): transport error: error sending request for url (http://localhost:8000/v1/chat/completions)` / `(Check that your LLM server is running at http://localhost:8000/v1 for model \`qwen-3.8-27b\`)` / `[status] LLM error: http (Ready)` / `[done]` |

**Verdict:** `--help`, the `--config` plumbing, the session-flag validator (exit 2) and
the raw/headless print path all run. The raw path runs the full session loop and
terminates honestly at the missing local LLM backend; no panic, no screen residue, exit
code 0 with the `[done]` sentinel. What is *not* exercised by this: any real LLM turn,
tool execution, or the TUI front-end.

### 6.1 D10 — `--version` is swallowed by the catch-all PROMPT arm (pre-existing; recorded, not fixed)

`parse_args()` in `src/main.rs` has no `--version` arm, and its last arm is
`other => { if args.prompt.is_none() { args.prompt = Some(other.to_string()) } … }`
(`src/main.rs:324-330`), so a long option that is not known becomes the session prompt.

Observed behaviour in this sandbox, verbatim:

```
$ ./target/debug/marmel --version                      # no readable user config
exit=1
stderr:
Error: reading /home/fredrik/.marmel/marmel.toml

Caused by:
    Permission denied (os error 13)

$ cd /tmp && marmel --config …/marmel.toml.example --version
exit=0
stdout:
[status] Running (qwen-3.8-27b)
[assistant] 
[Error] LLM backend call failed (http): transport error: error sending request for url (http://localhost:8000/v1/chat/completions)
(Check that your LLM server is running at http://localhost:8000/v1 for model `qwen-3.8-27b`)
[status] LLM error: http (Ready)
[done]
```

The second run is the defect in the open: **`--version` starts a session** — the token
becomes the goal text, the loop boots, the MCP boot and the LLM turn run, and the
process exits 0. Nothing prints a version and no unknown-option error is reported. Same
shape for any unrecognised long option. Left untouched per instruction (D10 is a known
pre-existing defect outside the UI rebuild); it is not a UI regression, and it does not
affect the frame path.

---

## 7. UNTESTED — live PTY / interactive TUI

A live interactive launch **could not be performed in this sandbox**, and no claim of
interactive success is made anywhere in this report. Established once (§1.1):
`openpty` → `OSError: out of pty devices`; `script` → `failed to create
pseudo-terminal: Permission denied`; the crate's own PTY tests fail with
`PermissionDenied`. `marmel` without `--raw` on a pipe would still enter the alternate
screen path, but `init()` (`enable_raw_mode`, `EnterAlternateScreen`,
`PushKeyboardEnhancementFlags`) and the crossterm event reader cannot be exercised
against a real terminal here.

**Not verified (UNTESTED):** raw-mode enable/restore, alternate screen enter/leave,
the D11 guarantee that a *test* never reaches that teardown while a real terminal is
attached (only its in-crate unit pin runs here), keyboard enhancement flags, bracketed
paste, key event decoding (including the disambiguate flags), `Tab`/`Shift+Tab`/
`Ctrl+…` actually arriving from a keyboard, resize handling / SIGWINCH, the 30/25/125 ms
cadence against a real draw loop, scrollback/wrap behaviour of a real terminal emulator,
colour rendering (styles are asserted only through `TestBackend` cell styles), true
`Esc`-arm timing behaviour under human keystrokes, and copy/paste in a real terminal.

**Checklist for the user's own machine (a real terminal, one session each):**

1. `cargo run --bin marmel` in a 100x30 terminal: confirm the frame matches
   `docs/new_ui_preview.txt` case b (header chips, transcript + plan + workers, status
   rows, composer, legend row) and that colours/focus colours read correctly (the
   `TestBackend` pins prove cells and styles, not perception).
2. Resize live: 100→89→70→59 columns and 30→20→19 rows, then to 39x9. Expect the §2.5
   ladder exactly as §4 records, `workers:narrow` appearing instead of a lying
   `workers:on`, and the `Terminal too small — Marmel needs at least 40x10 (currently
   39x9). Resize to continue.` notice below the floor with no leftover chrome.
3. Keymap smoke: `Tab`/`Shift+Tab` focus walk (badge `●` follows, one pane only),
   `F2` mode cycle M1→M4, `F3/F4/F5` pane toggles, `Ctrl+T/F/A/P` chips on status row 2,
   `F1` and `?` opening the same legend, `Esc` closing overlays without arming abort,
   `x` opening CancelWorker with the roster focused, `e` printing the sticky error,
   `Ctrl+D` opening Quit rather than exiting, and `y`/`n` inside overlays only.
4. Streaming: start a real session and watch a long streamed answer plus a worker
   row change (running → queued → paused) while scrolled away from the tail
   (`↓ N new` chip), then `Ctrl+F` to re-arm.
5. Exit hygiene: quit with `y`, then confirm the shell is back to a sane state
   (`reset` not needed, mouse/bracketed-paste modes off, cursor visible) — this is the
   ambient-terminal behaviour only the in-crate pin covers today.
6. A terminal emulator with a narrow window on a font without box-drawing glyphs —
   the frames assume U+2500/2502/250C… and the state glyphs (`● ⧗ ▶ ⏸ ✓ ✗ ↓`) exist.

---

## 8. Out-of-band observation: the rebuilt UI is live-painting at 80x24 in this sandbox

While `cargo test` was running, the stdout stream shared with this task carried the
**host marmel session's own** rebuilt TUI frames (cursor-addressed, with SGR runs).
Decoding the runs of one 80x24 repaint (only the text after each `\x1b[<row>;<col>H`
marker; interleaved test output is visible as stray text on two rows) produced:

```
 1|  Marmel M4 Split ✓ 15/17 done ▶ t-003 ctx 12.4k elapsed 1538m 19s
 2| ┌ Transcript ──────────────────────────────────────────────────────────────────┐
10| └  ● LIVE  scroll 101/101 ─────────────────────────────────────────────────────┘
11| ┌ Execution Plan ───────────────────────────────────────────15/17 done ▶ t-003 ┐
12| │## Phase 10 — Final verification & delivery                                  ▲│
16| │`docs/runtime_smoke_report.md` (validate) (validator)                        █│
18| └ ✓ 15 done ⧗ 2 open ▶ in flight  scroll 79 ───────────────────────────────────┘
19|  Tokens 12.4k in / 11.6k out | 14a | 1p | coder_specialist-… | elapsed 1538m 19s
20|  plan:on  workers:narrow  think:off  14a 1…
21| ┌ Input (Enter send, Alt/Shift+Enter newline, Ctrl+Z undo) ● ──────────────────┐
23| └──────────────────────────────────────────────────────────────────────────────┘
24|  F1 help  Tab focus  F2 mode  F3/F4/F5 panes  x cancel-worker  e last error  Es…
```

What it proves, in this sandbox, without a pty: the new draw path emits a full 80x24
frame to a real stream — header chips with `✓ n/m done`, active task and `ctx`/`elapsed`,
the transcript's own `● LIVE  scroll n/n` chip, the plan pane with its scrollbar
(`▲ █ ▼ ║`), the two status rows including `workers:narrow` (the ladder chip), the
composer focus badge `●`, the clipped legend row, and name clipping marked with `…`.
What it does **not** prove: that raw mode, keystrokes or a real terminal's own behaviour
work (§7) — this stream was a pipe, and the frames belong to another session, not to a
launch this task controlled.

---

## 9. Residual risks worth human eyeballs

1. **Live interactive behaviour is entirely unverified here** (§7). Everything above is
   cell-level or state-level; nothing measures a real terminal's response to the escape
   burst, the resize path, or key decoding under the keyboard-enhancement flags.
2. **"No text clipped mid-word at the right edge" is not a property the code guarantees**
   — §7.4 deliberately cuts names/chips mid-token with `…` (`coder_specialist-…`,
   `Es…`, `3a 1p 1q` band). The verified part is that the cut is *marked* and never
   overruns the pane rect. A human should eyeball 60x16 and 40x10 to judge whether the
   surviving fragments are still readable.
3. **Fenced code blocks keep raw SGR bytes.** `components/transcript.rs::stream_lines`
   routes content containing ` ``` ` to `render_markdown_lines` **before** the ANSI
   bridge, so an SGR-bearing fenced block carries its escape bytes into the `Line`
   spans (the new test records this: `SMOKE text[ansi]: markdown carries 1 line(s) with
   raw SGR bytes`). `TestBackend` renders them as ordinary cells; a real terminal may
   interpret them. Worth one look with a worker that streams coloured output.
4. **The §8.5 sticky error line has no frame pin.** Its content and the `e`/`/error`
   recall are pinned behaviourally, and the public-seam test here shows the error
   traffic in the pane model, but no `TestBackend` frame asserts the drawn
   `✗ <cause>` + `see .marmel/marmel.log` lines (they are reached through
   `pub(crate)` keys). Same for the armed-abort prompt surface at the tiniest sizes.
5. **Frame evidence cannot be regenerated from `tests/`.** Because `draw_new_ui` and the
   key seam are `pub(crate)`, no out-of-crate test (or third-party consumer) can render
   the UI. That is a deliberate encapsulation, but it means "smoke test the frames" will
   always require an in-crate harness; a small `#[doc(hidden)] pub` test-only entry
   point, or a re-`pub` of `draw_new_ui` behind a feature, would make t-018-style
   verification reproducible outside the crate.
6. **Whole-frame coverage is concentrated at 100x30.** 29 of the 36 `tests_new_ui`
   frame pins draw 100x30; 80x24 and 60x16 are covered by 5 pins each (geometry,
   scroll clamp, scroll-chip-once, status-row clipping, queued-rank) plus the recorded
   preview. Scenario-specific frames (each overlay variant, each focus state, plan and
   workers panes) are pinned only at 100x30 — the ladder sizes deserve a couple of
   overlay/focus frames of their own.
7. **Whitespace-only payloads still consume rendered rows** (`SMOKE text[blank_lines]:
   rows at 100=5`) and an ANSI-bearing payload does not grow when the viewport narrows
   (`text[ansi]: rendered rows at 100=1, at 39=1`), because escape bytes count toward
   raw wrap arithmetic. Cosmetic, but it can make a transcript pane look emptier or
   fuller than the text it holds.
8. **CLI: `--version` still opens a session** (D10, §6.1) and the same catch-all swallows
   any unknown long flag; a user typing `--version` gets an LLM turn instead of a
   version. Deliberately not fixed in this task.

---

## 10. Artefacts

* `tests/test_new_ui_runtime_smoke.rs` — 8 public-seam smoke tests, independent of pty,
  no new dependencies, no `#[allow]`, fmt/clippy clean. Header comment states exactly
  what is and is not covered and why.
* `docs/runtime_smoke_report.md` — this report (the only doc touched by this task).
