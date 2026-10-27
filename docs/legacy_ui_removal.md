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
