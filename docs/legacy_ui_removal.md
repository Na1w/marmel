# Legacy TUI rendering retired (task t-014)

Branch `feature/new-ui`. `render::draw` already routed **every** pane through the component render
functions (`draw_new_ui` → `components/{header,status_bar,plan_pane,transcript,activity,input,help}`),
so the `#[cfg(test)] mod legacy_panes` block in `src/ui/tui/render.rs` was the last superseded draw
path. It is now deleted, together with the helpers and tests that only existed to serve it.
Authority: `docs/ui_design_spec.md` §9 (deletion map) and §0 (frozen invariants).

## (a) Deleted symbols

| Symbol | Home it was deleted from | Why |
|---|---|---|
| `mod legacy_panes` (whole module, incl. its `use super::*`, `Constraint/Direction/Layout`, `Block/Borders/Paragraph/Scrollbar/ScrollbarOrientation/ScrollbarState/Wrap`, `unicode_width::{UnicodeWidthChar, UnicodeWidthStr}`, `ACTIVITY_STATUS_KEYWORDS`/`FocusedPanel` imports) | `src/ui/tui/render.rs:368-1163` | Dead draw path: `draw` never calls it (spec §9.2). |
| `TuiRenderer::render_chat` | `render.rs` legacy module | Replaced by `components::transcript`. |
| `TuiRenderer::render_plan` | `render.rs` legacy module | Replaced by `components::plan_pane`. |
| `TuiRenderer::render_subagents` | `render.rs` legacy module | Replaced by `components::activity`. |
| `TuiRenderer::render_status` | `render.rs` legacy module | Replaced by `components::status_bar` (+ `header`). |
| `TuiRenderer::render_input` | `render.rs` legacy module | Replaced by `components::input`. |
| `TuiRenderer::format_count` | `render.rs` legacy module | Live copy already exists as `components::status_bar::format_count` (used by `status_bar`, `header`, `activity`). |
| `TuiRenderer::format_token_counts` | `render.rs` legacy module | Superseded by the `status_bar` token fields; no caller left. |
| `formatting::rect_contains` | `src/ui/tui/formatting.rs` | Zero callers after the mouse arms were retired (spec §9.2); now an orphan. |
| `use ratatui::layout::Rect;` | `src/ui/tui/formatting.rs:4` | Only existed for `rect_contains` (unused-import deny). |

Test-support fixtures removed because their only readers were the removed legacy-pane tests
(`dead_code` is denied, so they could not stay):

| Fixture | Home |
|---|---|
| `draw_subagent_panel`, `draw_status_line` | `src/ui/tui/tests.rs` |
| `SPINNER_FRAMES`, `auth_row`, `drawn_occurrences`, `drawn_row_of`, `CAP_TALL`, `drawn_line_numbers` | `src/ui/tui/tests.rs` |
| `rows` (buffer→text helper) | `src/ui/tui/components/input.rs` tests |

Doc/prose pointers that named retired symbols or removed tests were repointed (not code):
`src/ui/tui/mod.rs` (`ACTIVITY_STATUS_KEYWORDS` doc, dropped the `[`TuiRenderer::render_status`]`
link), `src/ui/tui/tests.rs` (one doc comment citing a removed test name).

## (b) Surviving consumed helpers (spec §9.1) — grep-verified location

Every §9.1 helper already lives in a **surviving, non-`#[cfg(test)]` module** — it was reachable
from the legacy area only through `use super::*` → `pub use formatting::*` (`tui/mod.rs`), so no
relocation was required and none was performed: signatures, visibility and algorithms are untouched,
and every call site still resolves unchanged.

| Symbol | Old path | New path (post-t-014) | Out-of-module consumer kept compiling |
|---|---|---|---|
| `extract_plan_line_task_id` | `src/ui/tui/formatting.rs` | `src/ui/tui/formatting.rs` (unchanged; also re-exported as `crate::ui::tui::*`) | `src/plan_parse.rs:2746`, `components/plan_pane.rs` |
| `visual_line_offset_of_first_pending` | `src/ui/tui/formatting.rs` | `src/ui/tui/formatting.rs` (unchanged) | `src/plan_parse.rs:2762`, `components/plan_pane.rs:167` |
| `visual_line_offset_of_task` | `src/ui/tui/formatting.rs` | `src/ui/tui/formatting.rs` (unchanged) | `src/plan_parse.rs:2767`, `components/plan_pane.rs:166` |
| `line_matches_task_id` | `src/ui/tui/formatting.rs` | `src/ui/tui/formatting.rs` (unchanged; still the thin delegate to the single surviving grammar owner `crate::plan_parse::line_matches_task_id`, spec §9.1) | `src/plan_parse.rs:2775`, `components/plan_pane.rs:191` |
| `clamp_warning_sentences` | `src/ui/tui/mod.rs` (`pub(crate)`) | `src/ui/tui/mod.rs` (unchanged; lives with the `token_witness_warnings` producer that shares its sentence shape) | `src/ui/session.rs:1547` |

Everything else exported by `formatting.rs` stayed put per spec §9.1; the only formatting symbol
dropped is `rect_contains`, which had no caller anywhere in `src/` or `tests/`.

## (c) Tests removed (46)

All of them called a deleted legacy fn directly, or called the `draw_subagent_panel` /
`draw_status_line` fixtures that wrapped one — required for the crate to compile.

| Test (module `ui::tui::tests` unless noted) | Reason | Re-establish in t-015? |
|---|---|---|
| `test_chat_renders_white_content_and_gates_thought` | body called `render_chat` | yes → `components::transcript` |
| `test_chat_scrolling_renders_windowed_view` | body called `render_chat` | yes → `components::transcript` |
| `test_chat_auto_scroll_shows_final_lines_of_multiline_markdown_response` | body called `render_chat` | yes → `components::transcript` (follow/scrollback G7) |
| `test_plan_auto_scrolls_to_first_pending_task` | body called `render_plan` | yes → `components::plan_pane` |
| `test_plan_auto_scrolls_to_started_task` | body called `render_plan` | yes → `components::plan_pane` |
| `test_render_plan_highlights_all_concurrent_active_tasks_and_avoids_substring_collision` | body called `render_plan` | yes → `components::plan_pane` (active-task highlight) |
| `test_token_counting_and_formatting_in_status_bar` | body called `TuiRenderer::format_token_counts` | yes → `components::status_bar` (`format_count` already tested there) |
| `test_orchestrator_context_tokens_in_status_bar` | body called `render_status` | yes → `components::status_bar` (G4 ctx %) |
| `test_waiting_for_token_elapsed_time_in_status_bar` | body called `render_status` | yes → `components::status_bar` (G4 elapsed) |
| `test_status_bar_displays_right_aligned_elapsed_time` | body called `render_status` | yes → `components::status_bar` / `header` |
| `test_subagent_context_tokens_rendering_in_list` | body called `render_subagents` | yes → `components::activity` |
| `test_subagent_time_counter_rendering_in_list` | body called `render_subagents` | yes → `components::activity` |
| `test_subagent_thinking_visibility_controlled_by_show_thought` | body called `render_subagents` | yes → `components::activity` (thinking gate) |
| `test_tui_renders_subagent_thinking_progress_and_output_during_streaming` | body called `render_subagents` (×4) | yes → `components::activity` (streaming row) |
| `test_subagent_thinking_shows_remaining_budget_countdown` | body called `render_subagents` (×3) | yes → `components::activity` (budget countdown) |
| `a_finished_worker_with_leftover_thinking_stops_claiming_to_be_thinking` | body called `render_subagents` | yes → `components::activity` (stale-spinner guard) |
| `a_collided_row_renders_from_its_registry_entry_and_the_label_says_what_it_is` | body called `render_subagents` | yes → `components::activity` (row identity / ctx from registry) |
| `the_runners_second_registration_opens_no_panel_row_and_never_steals_the_selection` | used `draw_subagent_panel` | yes → `components::activity` (double registration) |
| `a_restored_collision_row_is_still_drawn_with_its_history_and_its_badge` | used `draw_subagent_panel` | yes → `components::activity` + restore seam |
| `queued_rows_are_labelled_with_their_position_in_the_list` | used `draw_subagent_panel` | yes → `components::activity` (queue label) |
| `a_queued_row_stays_visibly_active_and_colours_only_its_slot_label` | used `draw_subagent_panel` | yes → `components::activity` (queue colouring) |
| `the_detail_pane_spells_out_the_slot_wait` | used `draw_subagent_panel` | yes → `components::activity` (detail/queue text) |
| `a_row_that_is_not_running_never_shows_a_queue_position` | used `draw_subagent_panel` | yes → `components::activity` |
| `a_queued_row_still_counts_as_active_in_the_main_status_line` | used `draw_status_line` | yes → `components::status_bar` (active count) |
| `a_queued_only_round_keeps_the_status_line_animating` | used `draw_status_line` | yes → animation tier (`ACTIVITY_STATUS_KEYWORDS`) |
| `queue_position_updates_never_move_the_selection_or_the_list_window` | used `draw_subagent_panel` | yes → `components::activity` (selection stability) |
| `a_restored_row_carries_the_queue_position_the_bundle_recorded` | used `draw_subagent_panel` | yes → restore seam + `components::activity` |
| `a_queued_rows_whole_pane_is_rebuilt_from_the_registry_alone` | used `draw_subagent_panel` | yes → `components::activity` (registry-only rebuild) |
| `a_queued_row_keeps_its_label_while_thinking_and_output_stream_into_it` | used `draw_subagent_panel` | yes → `components::activity` |
| `a_push_carrying_no_log_lines_never_empties_a_row_that_has_them` | used `draw_subagent_panel` | yes → log-merge contract (`merge_logs` pure fn still covered) |
| `a_re_queued_row_follows_every_rank_edge_in_its_label_and_its_bar` | used `draw_subagent_panel` | yes → `components::activity` (rank edges) |
| `a_held_stream_is_labelled_paused_and_keeps_its_live_shape` | used `draw_subagent_panel` | yes → `components::activity` (arbitration pause) |
| `the_detail_pane_spells_out_the_held_model_slot` | used `draw_subagent_panel` | yes → `components::activity` (pause reason) |
| `the_authoritative_rows_from_the_loop_own_the_pause_label` | used `draw_subagent_panel` | yes → `components::activity` |
| `a_restored_row_carries_the_pause_the_bundle_recorded` | used `draw_subagent_panel` | yes → restore seam (D2 pause field) |
| `the_main_status_line_names_held_streams_without_dropping_them_from_the_round` | used `draw_status_line` | yes → `components::status_bar` (paused segment) |
| `a_round_with_a_parked_stream_keeps_the_status_line_animating` | used `draw_status_line` | yes → animation tier |
| `an_interleaved_push_sequence_leaves_exactly_the_ordered_union_of_log_lines` | used `draw_subagent_panel` | yes → log-merge contract through the real push path |
| `the_same_authoritative_row_pushed_five_times_leaves_the_log_list_byte_identical` | used `draw_subagent_panel` | yes → log-merge idempotence |
| `a_longer_incoming_list_sharing_a_prefix_is_adopted_without_duplicating_that_prefix` | used `draw_subagent_panel` | yes → log-merge prefix adoption |
| `dedup_is_content_sensitive_and_order_stable_across_a_legitimately_repeating_line` | used `draw_subagent_panel` | yes → log-merge dedup |
| `a_long_running_worker_stays_at_the_per_row_log_cap_at_every_pump_and_keeps_the_newest_lines` | used `draw_subagent_panel` (`CAP_TALL`) | yes → per-row 200-line cap (spec §0 cadence) |
| `the_per_row_log_cap_binds_each_row_on_its_own_stream` | used `draw_subagent_panel` | yes → per-row log cap |
| `an_empty_log_push_after_the_cap_neither_shrinks_nor_clears_the_capped_list` | used `draw_subagent_panel` | yes → per-row log cap |
| `a_lifecycle_line_the_cap_has_already_trimmed_is_re_appended_at_the_tail_by_the_next_pump` | used `draw_subagent_panel` | yes → per-row log cap |
| `the_composer_frame_is_painted_once_even_after_the_legacy_pass` (`ui::tui::components::input::tests`) | body called `render_input` twice (double-draw guard) | **no** — its subject (legacy pass + component pass on one `TextArea`) no longer exists; single-frame ownership is already pinned by `the_draft_body_is_drawn_inside_the_frame_it_owns` |

## Gate results after the change

| Command | Baseline | Now |
|---|---|---|
| `cargo build` | clean | clean (no warnings) |
| `cargo clippy --all-targets` | clean | clean (no warnings) |
| `cargo test --lib ui::` | 512 passed | **466 passed, 0 failed** (−46 removed) |
| `cargo test --lib` | 1460 passed, 4 env failures | **1414 passed, 4 failed** — only `harness::pty::*` `openpty: PermissionDenied` (environmental) |
| `cargo test --test '*'` | 353 passed, 1 pre-existing failure | **353 passed, 1 failed** — `tests/test_role_gating.rs::test_prompt_based_tool_gating` (pre-existing) |

Known flake observed once under load and not chased:
`harness::dispatch_integrity_tests::test_notice_reply_round_trip_reaches_the_user_exactly_once`
(passes in isolation and on the final full run).

## Re-established in t-015

The 46 removed tests are re-established against the surviving surface — `render::draw` →
`draw_new_ui` → `components/*`, driven through the renderer seam (`TuiRenderer::on_event`,
`set_subagents`, `set_view_mode`, `set_follow`, `draw_new_ui` into a `TestBackend`) and through
`components::{activity,plan_pane,transcript}::render` for the two cases whose subject lives inside a
pane. 34 new tests in `src/ui/tui/tests.rs` cover them; where one test guarded several removed
assertions the mapping below names the covering test more than once. Assertions name state and
drawn substrings rather than cell-exact chrome wherever the old test's exactness was about the
retired border layout.

New test-support fixtures in `src/ui/tui/tests.rs` (the retired `draw_subagent_panel` /
`draw_status_line` / `CAP_TALL` / `drawn_line_numbers` equivalents): `auth_row`, `drawn_logs`,
`drawn_occurrences`, `drawn_row_of`, `draw_workers_pane`, `roster_row_of`, `roster_lines`,
`status_row_worker`, `metrics_row`, `draw_new_frame_styled`, `row_style`, `draw_plan_pane`.

| Removed test (t-014) | Re-established as |
|---|---|
| `test_chat_renders_white_content_and_gates_thought` | `the_transcript_prints_the_streamed_content_and_gates_the_thought_buffer` |
| `test_chat_scrolling_renders_windowed_view` | `a_transcript_scrolled_away_from_the_bottom_counts_what_is_below_it` |
| `test_chat_auto_scroll_shows_final_lines_of_multiline_markdown_response` | `a_followed_transcript_shows_the_last_lines_of_a_long_answer` |
| `test_plan_auto_scrolls_to_first_pending_task` | `a_plan_with_nothing_in_flight_scrolls_to_its_first_open_task` (renderer seam) + `the_plan_pane_falls_back_to_its_first_open_task_and_then_to_its_tail` (pane seam, incl. the finished-plan tail) |
| `test_plan_auto_scrolls_to_started_task` | `a_long_plan_auto_scrolls_to_the_task_in_flight` (+ `a_short_plan_auto_scroll_clamps_instead_of_jumping_the_viewport` for the clamped case) |
| `test_render_plan_highlights_all_concurrent_active_tasks_and_avoids_substring_collision` | `every_task_in_flight_is_marked_and_a_prefixed_sibling_is_not` |
| `test_token_counting_and_formatting_in_status_bar` | `token_counters_are_formatted_the_same_way_on_the_new_row` |
| `test_orchestrator_context_tokens_in_status_bar` | `the_ctx_ratio_is_the_renderers_own_pair_and_is_withheld_without_a_budget` |
| `test_waiting_for_token_elapsed_time_in_status_bar` | `the_slot_wait_and_parked_stream_sentences_arm_the_backend_wait_clock` |
| `test_status_bar_displays_right_aligned_elapsed_time` | `the_project_clock_is_the_last_field_of_the_row_and_the_first_the_ladder_takes_back` |
| `test_subagent_context_tokens_rendering_in_list` | `a_queued_row_is_ranked_on_its_roster_row_and_its_neighbours_keep_their_own_state` (`N ctx` on the row) |
| `test_subagent_time_counter_rendering_in_list` | same test (`1m 1s` on the row); per-cell metric colouring stays owned by `components::activity::tests::row_metrics_show_tokens_and_elapsed` |
| `test_subagent_thinking_visibility_controlled_by_show_thought` | `the_thinking_half_is_gated_by_the_toggle_and_reports_its_budget` |
| `test_tui_renders_subagent_thinking_progress_and_output_during_streaming` | `the_thinking_half_is_gated_by_the_toggle_and_reports_its_budget` + `a_closed_row_still_recalls_its_thinking_and_its_logs` |
| `test_subagent_thinking_shows_remaining_budget_countdown` | `the_thinking_half_is_gated_by_the_toggle_and_reports_its_budget` (budget header needs the renderer seam, which is why it is not a component test) |
| `a_finished_worker_with_leftover_thinking_stops_claiming_to_be_thinking` | `a_closed_row_still_recalls_its_thinking_and_its_logs`. The "stops claiming" half is **dropped**: the new pane has no live `[Thinking: N]` bar and derives its glyph from the row's own flags, so there is no stale-spinner claim left to disprove. |
| `a_collided_row_renders_from_its_registry_entry_and_the_label_says_what_it_is` | `a_collided_row_is_badged_and_measured_from_the_entry_that_owns_its_key` |
| `the_runners_second_registration_opens_no_panel_row_and_never_steals_the_selection` | `the_runners_second_registration_opens_no_roster_row_and_never_steals_the_selection` |
| `a_restored_collision_row_is_still_drawn_with_its_history_and_its_badge` | split: `a_restored_row_carries_the_queue_position_the_bundle_recorded` (bundle history reaches the row) + `a_collided_row_is_badged_and_measured_from_the_entry_that_owns_its_key` (badge and ctx come from the entry). The projection half is pinned where it lives, in `ui::helpers::tests::a_double_registered_delegation_projects_one_row_and_keeps_every_line`. |
| `queued_rows_are_labelled_with_their_position_in_the_list` | `a_queued_row_is_ranked_on_its_roster_row_and_its_neighbours_keep_their_own_state` |
| `a_queued_row_stays_visibly_active_and_colours_only_its_slot_label` | dropped: it asserted the colour of one `Span` inside the retired status line. The state it stood for (queued row stays live, rank rides the row) is re-pinned by the queue tests above; per-cell colour is asserted in the component suites that own it (`components::activity::tests::the_selection_marker_never_borrows_the_focus_signal`, `components::input::tests`). |
| `the_detail_pane_spells_out_the_slot_wait` | dropped for its prose half (`components::activity::tests::queued_worker_carries_the_queue_glyph_and_rank` owns the pane's wording); its renderer half — a queued round arming the wait clock — is `the_slot_wait_and_parked_stream_sentences_arm_the_backend_wait_clock` |
| `a_row_that_is_not_running_never_shows_a_queue_position` | `a_row_that_is_not_running_never_shows_a_queue_position` (same name, new surface) |
| `a_queued_row_still_counts_as_active_in_the_main_status_line` | `the_status_row_names_held_and_queued_streams_and_keeps_every_count` |
| `a_queued_only_round_keeps_the_status_line_animating` | dropped: the spinner-frame fixture (`SPINNER_FRAMES`) went with the legacy pass. Re-pinned without it by the surviving `the_slot_wait_keyword_is_shared_by_both_animation_tiers` plus the new `the_slot_wait_and_parked_stream_sentences_arm_the_backend_wait_clock`. |
| `queue_position_updates_never_move_the_selection_or_the_list_window` | `queue_position_updates_never_move_the_selection_or_the_list_window` (same name, new surface) |
| `a_restored_row_carries_the_queue_position_the_bundle_recorded` | `a_restored_row_carries_the_queue_position_the_bundle_recorded` (same name; capture → bundle → `restore_panes` → drawn row) |
| `a_queued_rows_whole_pane_is_rebuilt_from_the_registry_alone` | dropped as a whole-pane rebuild: the pane no longer rebuilds itself, it is handed rows. Registry-only truth is pinned where it now lives — `the_worker_lifecycle_the_registry_keeps_lands_on_the_pane_row`, `a_collided_row_is_badged_and_measured_from_the_entry_that_owns_its_key`, and `ui::helpers::tests::a_collision_row_without_a_live_plain_sibling_keeps_its_row`. |
| `a_queued_row_keeps_its_label_while_thinking_and_output_stream_into_it` | `a_queued_row_keeps_its_rank_while_thinking_and_output_stream_into_it` |
| `a_push_carrying_no_log_lines_never_empties_a_row_that_has_them` | `a_push_carrying_no_log_lines_never_empties_a_row_that_has_them` (same name, through `set_subagents`) |
| `a_re_queued_row_follows_every_rank_edge_in_its_label_and_its_bar` | `a_re_queued_row_follows_every_rank_edge_in_its_label_and_its_bar` (same name; "1/3 → 2/3 → 3/3" plus the delegation-event fold) |
| `a_held_stream_is_labelled_paused_and_keeps_its_live_shape` | `a_held_stream_is_labelled_paused_and_keeps_its_live_shape` (same name) |
| `the_detail_pane_spells_out_the_held_model_slot` | `a_held_stream_is_labelled_paused_and_keeps_its_live_shape` + the surviving `a_paused_row_is_marked_counted_and_explained_in_the_new_frame`; the wording itself is owned by `components::activity::tests::a_paused_row_states_why_it_gave_up_its_model_slot` |
| `the_authoritative_rows_from_the_loop_own_the_pause_label` | `the_authoritative_rows_from_the_loop_own_the_pause_label` (same name) |
| `a_restored_row_carries_the_pause_the_bundle_recorded` | `a_restored_row_carries_the_pause_the_bundle_recorded` (same name, D2 pause field through the bundle) |
| `the_main_status_line_names_held_streams_without_dropping_them_from_the_round` | `the_status_row_names_held_and_queued_streams_and_keeps_every_count` |
| `a_round_with_a_parked_stream_keeps_the_status_line_animating` | dropped with `a_queued_only_round_keeps_the_status_line_animating` (same fixture, same replacement tests) |
| `an_interleaved_push_sequence_leaves_exactly_the_ordered_union_of_log_lines` | `an_interleaved_push_sequence_leaves_exactly_the_ordered_union_of_log_lines` (same name, real push path) |
| `the_same_authoritative_row_pushed_five_times_leaves_the_log_list_byte_identical` | `the_same_authoritative_row_pushed_five_times_leaves_the_log_list_byte_identical` (same name) |
| `a_longer_incoming_list_sharing_a_prefix_is_adopted_without_duplicating_that_prefix` | `a_longer_incoming_list_sharing_a_prefix_is_adopted_without_duplicating_that_prefix` (same name) |
| `dedup_is_content_sensitive_and_order_stable_across_a_legitimately_repeating_line` | `dedup_is_content_sensitive_and_order_stable_across_a_legitimately_repeating_line` (same name) |
| `a_long_running_worker_stays_at_the_per_row_log_cap_at_every_pump_and_keeps_the_newest_lines` | `the_per_row_log_cap_binds_each_row_on_its_own_stream` — it pumps both workers past `MAX_SUBAGENT_ROW_LOGS` and asserts each row's exact newest window at every pump |
| `the_per_row_log_cap_binds_each_row_on_its_own_stream` | `the_per_row_log_cap_binds_each_row_on_its_own_stream` (same name) |
| `an_empty_log_push_after_the_cap_neither_shrinks_nor_clears_the_capped_list` | `an_empty_log_push_after_the_cap_neither_shrinks_nor_clears_the_capped_list` (same name) |
| `a_lifecycle_line_the_cap_has_already_trimmed_is_re_appended_at_the_tail_by_the_next_pump` | `a_lifecycle_line_the_cap_has_already_trimmed_is_re_appended_at_the_tail_by_the_next_pump` (same name) |
| `the_composer_frame_is_painted_once_even_after_the_legacy_pass` (`components::input::tests`) | dropped in t-014 and stays dropped: its subject was the legacy pass drawing over the component's `TextArea`. Single ownership of the composer frame is pinned by `components::input::tests::the_draft_body_is_drawn_inside_the_frame_it_owns`. |

### Neighbouring suites audited for legacy coupling (t-015)

- `src/ui/bridge/tests.rs` — **no change needed**. It never names `TuiRenderer`, a
  `TestBackend`, `FocusedPanel`, a pane title or a mouse event (grep-clean): all 45 tests assert the
  bridge protocol (envelope round-trips, command/event mapping, capability and session negotiation),
  which the retirement did not touch.
- `src/ui/session_tests.rs` — **no change needed**. Its two `TuiRenderer` uses assert surviving
  seams, not chrome: the destructive-command gating owner (`gates_destructive_commands`) and the
  restore/clamp contract on `plan_max_scroll` / `restore_panes` / `capture_ui`. The `status_lines()`
  helper appearing throughout is `BootRenderer::status_lines`, a local double that collects the
  `Event::Status` values the session loop emits — not the retired status line. No pane title, border
  glyph, `FocusedPanel` chrome or mouse-driven focus assertion remains in either file.

### Gate results after t-015

| Command | Before t-015 | After t-015 |
|---|---|---|
| `cargo build` | clean | clean (no warnings) |
| `cargo clippy --all-targets` | clean | clean (no warnings) |
| `cargo test --lib ui::` | 466 passed, 0 failed | **500 passed, 0 failed** (+34) |
| `cargo test --lib` | 1414 passed, 4 failed | **1448 passed, 4 failed** — still only `harness::pty::*` `openpty: PermissionDenied` (environmental) |
| `cargo test --test '*'` | 353 passed, 1 failed | 353 passed, 1 failed — still only `tests/test_role_gating.rs::test_prompt_based_tool_gating` (pre-existing) |

Known flake, observed again under load and not chased:
`harness::dispatch_integrity_tests::test_notice_reply_round_trip_reaches_the_user_exactly_once`.
