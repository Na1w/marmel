# Marmel TUI — render/event architecture baseline (branch `feature/new-ui`)

**Question answered:** how is the current Marmel TUI architected at the render/event level — entry path,
main loop, app state, render chain, public surface, and test seams.

**Method / sources (read-only, no web):** `src/ui/mod.rs` (238 L), `src/ui/tui/mod.rs` (1862 L),
`src/ui/tui/render.rs` (1197 L), `src/ui/tui/events.rs` (898 L), `src/ui/tui/formatting.rs` (1271 L),
`src/ui/raw.rs` (595 L); `src/ui/helpers.rs` only for helper signatures the TUI calls;
`src/ui/tui/tests.rs`, `src/ui/session_tests.rs`, `tests/test_session_restore_panel_abort.rs` for the
test seams in §6; `src/main.rs` / `src/ui/session.rs` only for the call boundary into the renderer.
Keymap/operator detail is covered in the sibling doc `docs/research_ui_ux_baseline.md`.

**Headline shape:** one `TuiRenderer` god-object (`src/ui/tui/mod.rs:78-208`, 50+ fields) is simultaneously
the pane model, the composer, the event handler, the throttle, and the owner of the `ratatui::Terminal`
(`src/ui/tui/mod.rs:168`). There is **no render loop of its own**: rendering is driven entirely by the
session loop's calls to `poll_input` / `read_input` / `flush`, each of which funnels through
`drain_and_throttle_flush` (`src/ui/tui/mod.rs:422-442`). Input is polled synchronously with
`crossterm::event::poll`/`read` — the `event-stream` feature is enabled in `Cargo.toml:31` but
`crossterm::event::EventStream` is never used anywhere in `src/` (grep: no hits).

---

## 1. Entry path, setup/restore, and how raw mode differs

**Dispatch (outside the TUI, for boundary):** `src/main.rs:65` computes
`use_raw = args.raw || cfg.ui_mode == "raw" || !stdout_is_terminal()`; `src/main.rs:88-92` calls
`ui::raw::run(...)` or `ui::tui::run(...)`.

- **TUI entry:** `pub async fn marmennill::ui::tui::run(cfg, initial, manager)`
  (`src/ui/tui/mod.rs:1850-1857`) — body is exactly `let mut renderer = TuiRenderer::new();`
  (`1855`) then `super::run_session(cfg, &mut renderer, initial, manager).await` (`1856`).
  So `tui::run` owns no loop and no terminal setup; the shared session loop is
  `pub use session::run_session` (`src/ui/mod.rs:11`).
- **Terminal setup:** `Renderer::init` for the TUI, `src/ui/tui/mod.rs:515-547`:
  `enable_raw_mode()?` (`516`) → `execute!(stdout, EnterAlternateScreen,
  PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES))?` (`517-521`) →
  `self.terminal = Some(Terminal::new(CrosstermBackend::new(stdout))?)` (`522-524`) →
  best-effort `EnableBracketedPaste` (`534`, deliberately last and `let _ =`, rationale `525-533`) →
  optional plan load that auto-opens the Plan panel (`536-543`) → first paint `force_flush()?` (`545`).
  A second `Terminal::new` fallback exists in `render_terminal` (`396-403`) if `terminal` is `None`.
- **Restore — three paths, all mode-mirroring:**
  - `shutdown()` (`1160-1169`): `PopKeyboardEnhancementFlags`, `LeaveAlternateScreen`,
    `DisableMouseCapture`, `DisableBracketedPaste`, then `disable_raw_mode()`.
  - `impl Drop for TuiRenderer` (`1829-1833`) → calls `shutdown()`.
  - `pub fn leave_alt_screen()` (`1836-1847`) — same flag set, for the panic path; reached via
    `pub fn ui::restore()` (`src/ui/mod.rs:231-234`, which calls `tui::leave_alt_screen()` **and**
    `raw::restore()`), installed in the panic hook at `src/main.rs:422-426`.
  - Note: `init` never enables mouse capture (no `EnableMouseCapture` anywhere in `src/`), yet both
    teardown paths unconditionally emit `DisableMouseCapture` (`1165`, `1844`); mouse arms exist at
    `src/ui/tui/events.rs:225-287`.
- **Headless/raw mode differs by absence:** `pub async fn ui::raw::run` (`src/ui/raw.rs:20-27`) does the
  identical two-line dance with `RawRenderer::new()`. `RawRenderer` holds only `buffer: Vec<u8>` +
  `input_state` (`46-51`); `init()` is a documented no-op (`91-94`); it never enters raw mode or the
  alternate screen; `restore()` (`31-41`) only defensively calls `disable_raw_mode()` when
  `stdout().is_terminal()`; output is one labelled line per 512-byte UTF-8 chunk
  (`push_line` `61-66`, `chunk_utf8(text, 512)`) flushed through `flush()` (`150-158`) and `shutdown()`
  (`179-181`); `poll_input`/`read_input` are the trait defaults — no stdin (`160-163`).
  `RawRenderer` honestly claims an empty pane model in `capture_ui` (`194-196`).
- **The contract both modes implement:** `pub trait Renderer: Send` (`src/ui/mod.rs:144`) with
  `init/on_event/flush/force_flush/poll_input/read_input/request_abort/aborted/clear_abort/
  request_user_exit/user_exit_requested/input_state/input_state_shared/shutdown/set_subagents/
  set_thinking_budgets/reset_active_agent/capture_ui/restore_panes/announce_restore`, plus
  `pub struct InputState { aborted, user_exit }` (`src/ui/mod.rs:131-135`).
  Threading note (`src/ui/mod.rs:192`): `dyn Renderer` is `Send` but **not** `Sync` because
  `TuiRenderer` holds `Cell`/`RefCell` — capture/restore is session-thread only.

## 2. Main loop: event source, cadence, redraw policy, resize

- **Event source:** synchronous crossterm polling inside
  `pub(crate) fn handle_events(&mut self, blocking: bool) -> bool` (`src/ui/tui/events.rs:15`):
  `event::poll(Duration::from_millis(30))` when blocking (`19`), `event::poll(Duration::ZERO)`
  when not (`21`), then `event::read()` (`27`). Keys accepted on `Press | Repeat` (`28-29`);
  `Err(_) => break` (`300`); in blocking mode exactly **one** event is consumed per call (`302-304`).
  No `EventStream`, no dedicated input thread, no channel from a reader task — input handling runs on
  the session thread.
- **Who drives it (the "main loop" is the session loop):** `renderer.read_input()` at
  `src/ui/session.rs:1965` (idle prompt) and `renderer.poll_input()` at `src/ui/session.rs:2080` and
  `2822` (mid-turn steering); `renderer.flush()`/`set_subagents()`/`on_event()` are called throughout
  (`src/ui/session.rs:1704-1706`, `2251`, `2276`, `2420`, `2651`, `2997`, `1231`, `1643`).
- **TUI-side loop bodies:**
  - `poll_input()` (`src/ui/tui/mod.rs:1103-1106`): non-blocking `drain_and_throttle_flush(false)` +
    `rx.try_recv()`.
  - `read_input()` (`1108-1123`): commits streaming text, repaints once (`1109-1111`), then busy-loops
    `drain_and_throttle_flush(true)` + `rx.try_recv()`; returns `None` if `self.aborted()` (`1115-1117`)
    so the session loop stops instead of hanging. Effective idle cadence = one 30 ms poll per iteration.
- **Tick/poll intervals (all constants):** 30 ms input poll (`events.rs:19`); 25 ms dirty-redraw floor
  (`mod.rs:434`, also enforced inside `flush()` `1097`); 125 ms spinner-only repaint (`438`);
  3 s abort-confirmation window (`410-415`); 250 ms plan-file re-stat cache (`render.rs:272`);
  125 ms spinner phase (`render.rs:1109`, `912`).
- **Redraw policy** (`drain_and_throttle_flush`, `mod.rs:417-442`): if any terminal event was handled →
  clear `dirty` and `force_flush()` (full repaint, bypasses the 25 ms floor, `424-426`); else if
  `dirty` and ≥25 ms since last render → `flush()` (`433-437`); else if a spinner is live
  (any active subagent **or** `status_line` contains an `ACTIVITY_STATUS_KEYWORDS` entry, `428-431`,
  list at `56`) and ≥125 ms → `flush()` (`438-440`); else nothing (0 % CPU idle, documented `421`).
  `dirty` is set by `on_event` (`550`) and by input mutations (e.g. `58`, `337` in events.rs).
  Frame counter is incremented (`395`) but nothing renders from it — animation phase is wall-clock
  (`session_start`).
- **Resize:** explicit arm `Ok(TermEvent::Resize(_, _))` (`events.rs:293-298`) → `clamp_scrolls()`
  (`488-504`, clamps chat/plan/subagent-detail/subagent-list scroll using the cached geometry) then
  `flush()`. Geometry is refreshed *during* the draw: `chat_width/chat_height` (`render.rs:436-439`),
  `plan_width/plan_height` (`572-575`), `subagent_width/subagent_height` (`891-894`), and the outer
  rects `chat_area/plan_area/subagent_area` are written back after `terminal.draw` (`render.rs:419-421`,
  fields `mod.rs:175-178`); `input_area` is used from the draw closure (`render.rs:364`, `events.rs:280`).
  ratatui's `Terminal::draw` additionally auto-resizes its own diff buffer — the `Terminal` instance is
  kept alive across frames specifically to preserve that (`mod.rs:167-168`).

## 3. App state: struct, key fields, mutation sites

- **The struct:** `pub struct TuiRenderer` — `src/ui/tui/mod.rs:78-208` (single instance, created in
  `run` at `1855`; `Default` = `new()` at `445-449`; `new()` creates the `std::sync::mpsc` channel at
  `212`). There is no separate `App`, `Model`, or view-state type in these modules.
- **Field groups (line = declaration):**
  - Pane text buffers: `messages: Vec<String>` (`81`), `current_thought` (`83`), `current_content`
    (`85`), `plan_content` (`87`), `steer_sentence_buffer` (`135`), `status_line` (`106`).
  - Composer: `textarea: ratatui_textarea::TextArea<'static>` (`89`), `input_text` (`91`),
    `cursor` byte offset (`93`), `history`/`history_index`/`input_draft` (`129-133`).
  - Abort: `confirm_abort` (`96`), `abort_armed_at` (`98`), `input_state: crate::ui::InputState` (`102`).
  - View toggles/focus/scroll: `dirty` (`100`), `frame_counter` (`104`), `focused_panel: FocusedPanel`
    (`108`, enum `34-38`), `chat_scroll` (`110`), `plan_scroll` (`112`), `show_plan_panel` (`114`),
    `had_active_plan` (`116`), `show_thought` (`118`), `show_subagent_panel` (`120`),
    `subagents: Vec<SubagentDetail>` (`121`), `selected_subagent_idx` (`123`), `subagent_scroll` (`125`),
    `subagent_list_scroll` (`127`), `subagent_autoscroll` (`162`), `chat_auto_scroll` (`170`),
    `plan_max_scroll`/`plan_auto_scroll`/`active_plan_task` (`171-173`).
  - Derived caches (interior mutability, hence `!Sync`): `chat_width/chat_height` (`138-140`),
    `subagent_width/height` (`142-144`), `plan_width/height` (`146-148`), `cached_chat_width` (`150`),
    `cached_show_thought` (`152`), `cached_message_lines` (`154`), `cached_total_message_lines` (`156`),
    `cached_rendered_messages: RefCell<Vec<Vec<Line>>>` (`158`),
    `cached_rendered_show_thought` (`160`).
  - Transport/terminal: `rx: Receiver<String>` / `tx: Sender<String>` (`165-166`),
    `terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>` (`168`).
  - Geometry of last frame: `chat_area`/`plan_area`/`subagent_area`/`input_area` (`175-178`).
  - Clocks/file cache: `active_agent` (`180`), `last_render` (`181`), `waiting_for_token_since` (`182`),
    `last_plan_check` (`183`), `plan_mtime`/`archive_mtime`/`plan_is_archived` (`184-186`),
    `session_start` (`187-188`), `plan_start_time`/`plan_completed_time` (`205-207`).
  - Counters/budgets: `tokens_in`/`tokens_out`/`orchestrator_context_tokens` (`191-195`, the only
    `pub` fields), `thinking_budgets`/`default_thinking_budget` (`197-199`),
    `subagent_turn_thinking` (`201`), `subagent_is_thinking` (`203`).
- **Where state is mutated:**
  - `on_event` (`549-1090`), one `match` over `super::Event` (`src/ui/mod.rs:83`): `TokensIn` `552`,
    `TokensOut` `556`, `Message` `559`, `SteerResponse` `590`, `Thinking` `607`,
    `SubagentMessage` `638`, `SubagentThinking` `659`, `SubagentStreamPause` `680`, `ToolCall` `705`,
    `ToolResult` `743`, `Status` `776`, `Delegation` `874` (sub-arms `Started` `877`,
    `Completed` `909`, `Queued` `963`, `Running` `993`, `Failed` `1012`), `Done` `1080`;
    tail `1085` commits on handled/Done.
  - Input handlers: `src/ui/tui/events.rs` — keymap `31-223`, mouse `225-287`, paste `288-292`/`319-338`,
    composer sync/edit helpers `340-486`, scroll helpers `488-596`, panel toggle `598-627`,
    row resolution/upsert `629-697`, history `699-731`, `commit_turn_content` `733-762`,
    `submit` `764-851` (local commands `/thought` `779-786`, `/help` `789-813`,
    `/reset|/reset_plan|/clear_plan…` `814-834`; otherwise echo `User: …` `845`, history `838-840`,
    token count `843-844`, abort check `847-849`, and hand-off `self.tx.send(line)` `850`),
    `cycle_focus` `854-876`, `append_steer_sentence` `879-897`.
  - Authoritative row adoption: `set_subagents` (`1171-1276`) with log merge `merge_logs` (`496-512`,
    cap `MAX_SUBAGENT_ROW_LOGS = 200` `470`).
  - Render-time mutation: `draw` re-reads the plan from disk and can open the Plan panel
    (`render.rs:266-322`), recomputes autoscroll offsets (`324-344`), and writes geometry caches
    (`436-439`, `572-578`, `891-901`, `782`).
  - Abort surface: `request_abort` (`1133-1139`), `request_user_exit` (`1141-1149`),
    `clear_abort` (`1151-1158`) — all also call `orchestrator::cancel_all()` / `reset_cancellation()`.
  - Session rehydration: `capture_ui` (`1291`), `restore_panes` (`1400`), `announce_restore` (`1560`),
    `token_witness_warnings` (`1792-1814`), `invalidate_pane_caches` (`1819-1826`).

## 4. Render structure and drawn regions

- **Chain:** `Renderer::flush`/`force_flush` (`mod.rs:1092-1101`) →
  `pub(crate) fn render_terminal(&mut self)` (`mod.rs:393-407`, sets `last_render`, bumps
  `frame_counter`, takes/recreates the `Terminal`) → `pub(crate) fn draw(&mut self, terminal)`
  (`render.rs:262-423`) → `terminal.draw(|frame| …)` (`render.rs:350-417`) → pane functions
  `render_chat` (`425-493`), `render_plan` (`495-624`), `render_subagents` (`626-1031`),
  `render_status` (`1053-1178`), `render_input` (`1180-1196`). Everything is a whole-frame redraw —
  no dirty-rect or per-pane invalidation below the widget level (caches are text caches only,
  `render.rs:22-116`).
- **Text caches feeding the Chat pane:** `ensure_rendered_message_cache` (`100-116`, markdown/ANSI
  parse per message, keyed on `show_thought`), `ensure_message_cache` (`22-59`, wrapped-line counts,
  full recompute on width/`show_thought` change, incremental append, truncate on removal),
  `invalidate_last_message_cache` (`63-92`, steer-append fast path), `estimated_chat_lines` (`201-219`),
  `estimated_subagent_lines` (`221-260`).
- **Layout constraints (no `Constraint::Ratio` anywhere — only `Length`/`Min`/`Percentage`):**
  1. Root vertical: `[Min(3), Length(1), Length(3)]` = main / status / input
     (`render.rs:354-364`).
  2. Main horizontal, only when `has_plan || has_subagents` (`367-370`):
     `[Percentage(60), Percentage(40)]` = chat / right column (`371-378`); otherwise chat takes the
     whole main area.
  3. Right column vertical when both visible: `[Percentage(50), Percentage(50)]` = Plan / Subagents
     (`385-395`).
  4. Inside Subagents: `[Length(list_len), Min(3)]` where `list_len = subagents.len().clamp(1,3)`
     capped by inner height (`647-653`); then `[Length(1), Min(2), Length(1)]` (separator / details /
     per-row status bar) or `[Length(1), Min(2)]` when the bar is suppressed
     (`800-821`, condition `794-798`).
- **Drawn regions and their content:**
  - **Chat & Logs** (`425-493`): border Cyan when focused else DarkGray (`426-430`), title
    `" Chat & Logs (Focus: Tab) "` (`434`); body = cached styled lines of `messages` (`441-448`) +
    optional streaming thought (`450-452`, `render_streaming_thought` `118-143`) + streaming content
    (`455-457`, `145-175`) + streaming steer sentence (`460-462`, `177-199`); `Paragraph` with
    `Wrap { trim: false }` and `scroll((chat_scroll, 0))` (`471-475`); right-side `Scrollbar` when
    scrollable (`478-492`).
  - **Execution Plan** (`495-624`): title `" Execution Plan "` (`507-510`); active-task set built from
    `subagents[*].task_id`/`-t-` name slicing plus `active_plan_task` (`512-538`); per-line styling:
    `#`/`##` green bold, active-task pending item yellow bold, `[x]` green, `[ ]` light red
    (`548-569`); auto-scroll targets the active task, else first pending, else bottom
    (`579-600`, `visual_line_offset_of_task` / `…_first_pending_`); `plan_max_scroll` recomputed per
    frame (`577-578`); scrollbar (`616-620`). Plan text itself comes from
    `.marmel/<PLAN_FILE>` / `execution_plan_archive.md` with a 250 ms mtime cache
    (`266-322`).
  - **Specialist Subagents** (`626-1031`): title with `[n/total]` (`633-644`); one
    `crate::orchestrator::worker_entries()` snapshot per frame (`659`); list rows `> name (Active|Idle|
    Queued n/m|Paused)(ctx, elapsed)` + validator/duplicate badge (`661-764`, status arms `717-739`,
    badge `745-754` via `helpers::subagent_row_badge`, `src/ui/helpers.rs:627`); list auto-windowing to
    keep the selection visible (`766-787`); detail pane `=== Details for X ===` then `[Thinking]`
    (gated by `show_thought`), `[Output]` (markdown when it contains ``` ` ```), `[Logs]` (ANSI parsed)
    (`823-889`); scroll clamp + own right-side details scrollbar (`891-906`, `1013-1028`); a per-row bottom
    bar with spinner + `Paused` / `Queued n/m` / thinking-budget / `Idle` text as its own left-aligned
    `Paragraph` (`909-1011`, thinking-liveness rule `919-933`, idle arm `1000-1005`); uniquely among the
    panes, its outer bordered `subagent_block` is rendered **last** (`1030`) — an explicit overlay of the
    frame on top of the inner widgets, whereas Chat/Plan render their block inside the `Paragraph`.
  - **Status line** (`1053-1178`): `[N active: …]` and `[N paused: …]` segments derived from rows
    (`1054-1093`), wait timer `(<n.n>s)` from `waiting_for_token_since` (`1102-1105`), 4-frame spinner
    keyed to `session_start/125` when a keyword matches (`1106-1112`), token totals merged with
    `crate::llm::get_global_token_counts()` via `max` (`1113-1116`, helpers `format_count` `1033`,
    `format_token_counts` `1043`), full-width red `[ABORT ARMED]` variant (`1117-1125`), otherwise
    `Ctx: … | Tokens: … | Status: …` left-padded against a right-aligned `Elapsed:` (`1127-1169`).
  - **Input** (`1180-1196`): the `ratatui_textarea::TextArea` itself is the widget — border Red when
    abort-armed else Cyan (`1183-1187`), title `" Input (Enter: Send, Alt/Shift+Enter: Newline,
    Ctrl+Z: Undo) "` (`1188-1191`), `set_block` + `render_widget(&self.textarea, area)` (`1193-1195`).

## 5. Public surface a rewrite must preserve or deliberately delete

- **Module wiring:** `src/ui/tui/mod.rs:3-10` — `mod events;` (`3`), `pub mod formatting;` (`4`),
  `mod render;` (`5`), **`pub use formatting::*;` (`7`)**, `#[cfg(test)] mod tests;` (`9-10`).
  `render.rs:3` and `events.rs:3` themselves do `use super::formatting::*;`, so every formatting symbol
  is unqualified inside the TUI and simultaneously reachable as `crate::ui::tui::formatting::x` and
  `crate::ui::tui::x`.
- **`pub` items in `src/ui/tui/mod.rs`:** `TuiRenderer` (`78`); its three `pub` fields `tokens_in`,
  `tokens_out`, `orchestrator_context_tokens` (`191-195`); `new` (`211`);
  `set_thinking_budgets` (`294`); `token_witness_warnings` (`1792`); `leave_alt_screen` (`1836`,
  consumed by `src/ui/mod.rs:232`); `run` (`1850`, consumed by `src/main.rs:91`).
- **`pub(crate)` items in `src/ui/tui/mod.rs`:** `FocusedPanel` (`34`), `ACTIVITY_STATUS_KEYWORDS`
  (`56`, read by `render.rs:4` for both animation tiers), `MAX_SUBAGENT_ROW_LOGS` (`470`),
  `merge_logs` (`496`), `clamp_warning_sentences` (`1743`, the only one consumed outside the module:
  `src/ui/session.rs:1547`), `is_abort` (`1860`), plus the `pub(crate)` methods listed in §3/§4 and the
  `pub(crate)` struct fields. Private helpers a rewrite may drop: `estimate_stream_tokens` (`452`),
  `plan_start_instant`/`plan_completed_instant` (`370`, `383`), `get_thinking_budget` (`314`),
  `total_project_elapsed` (`327`), the rehydration converters `focused_panel_tag`/`focused_panel_from_tag`/
  `subagent_panel_from_detail`/`time_point_from_instant`/`subagent_detail_from_panel`/`duration_from_secs`
  (`1620-1742`), `place_textarea_caret_at_byte` (`1761`), `invalidate_pane_caches` (`1819`).
- **`pub` surface of `formatting.rs` (the `pub use formatting::*` payload):** `message_style` (`12`),
  `enum LineSegment` (`67`), `parse_line_segments` (`73`), `strip_think_tags` (`129`),
  `format_terminal_math` (`446`), `format_math_expr` (`539`), `render_message_lines` (`760`),
  `count_wrapped_rendered_lines` (`847`), `wrapped_lines` (`872`),
  `visual_line_offset_of_first_pending` (`999`), `clean_task_id` (`1012`),
  `extract_plan_line_task_id` (`1029`), `line_matches_task_id` (`1035`),
  `visual_line_offset_of_task` (`1041`), `extract_complete_sentences` (`1070`), `rect_contains` (`1131`),
  `parse_ansi_lines` (`1138`), `render_markdown_lines` (`1148`), `extract_thought_and_content` (`1164`).
  **Real out-of-module consumers (all `plan_parse`, and all in tests):** `extract_plan_line_task_id`
  (`src/plan_parse.rs:2746`), `visual_line_offset_of_first_pending` (`2762`),
  `visual_line_offset_of_task` (`2767`), `line_matches_task_id` (`2775`) — cited in prose at
  `src/plan_parse.rs:573,591` as the ownership boundary; `src/manager/phase.rs:951` uses
  `crate::plan_parse::line_matches_task_id`, not this one. Every other formatting symbol has zero
  hits outside `src/ui/tui/`.
  Name-collision trap: `formatting::clean_task_id(&str) -> &str` (`1012`) is a **different** symbol from
  `helpers::clean_task_id(&str) -> String` (`src/ui/helpers.rs:1101`), and the bare `clean_task_id` at
  `src/ui/session.rs:2879,3457,3849` resolves through `use super::helpers::*` (`src/ui/session.rs:7`).
- **`pub(crate)` editing helpers that are already dead:** `cursor_left` (`399`), `cursor_right` (`416`),
  `delete_backward` (`432`), `delete_forward` (`447`), `insert_char` (`460`) are each
  `#[allow(dead_code)]` — the composer is driven by `textarea.input(key)` instead
  (`events.rs:70-71, 101-103, 148-152, 217-219`).
- **Renderer-trait overrides the session loop depends on** (`src/ui/mod.rs:144`): the TUI overrides
  `init` (`515`), `on_event` (`549`), `force_flush`/`flush` (`1092`/`1096`), `poll_input` (`1103`),
  `read_input` (`1108`), `input_state`/`input_state_shared` (`1125`/`1129`), `request_abort` (`1133`),
  `request_user_exit` (`1141`), `clear_abort` (`1151`), `shutdown` (`1160`), `set_subagents` (`1171`),
  `set_thinking_budgets` (`1278`), `reset_active_agent` (`1282`), `capture_ui` (`1291`),
  `restore_panes` (`1400`), `announce_restore` (`1560`). `RawRenderer` overrides only `init`,
  `on_event`, `flush`, `input_state*`, `request_user_exit`, `shutdown`, `capture_ui`, `restore_panes`
  (`src/ui/raw.rs:90-196+`), i.e. the headless contract is the trait minus input/abort/panel surface.
- **`pub` surface of `src/ui/raw.rs`:** `run` (`20`), `restore` (`31`, called by `src/ui/mod.rs:233`),
  `RawRenderer` (`46`) + `new` (`54`).
- **`pub` surface of `src/ui/mod.rs` that the TUI consumes:** `Event` (`83`), `InputState` (`131`),
  `Renderer` (`144`), `SubagentDetail` (`33`), `QueueState` (`24`), `restore` (`231`), and re-exports
  `helpers::{chunk_utf8, format_active_subtasks, format_plan_progress_summary}` (`10-12`).
  `SubagentDetail` is the pane row model — fields declared `src/ui/mod.rs:34-78` (`name`, `task_id`,
    `prompt`, `started_at`, `worked_duration`, `last_activity_at`, `logs`, `thinking`, `content`,
    `is_active`, `queue`, `paused`, `context_tokens`); `QueueState { position, of }` is `src/ui/mod.rs:24-29`.

## 6. Test seams — how `src/ui/tui/tests.rs` drives the TUI

- **Access model:** `#[cfg(test)] mod tests;` (`src/ui/tui/mod.rs:9-10`) with `use super::*;`
  (`src/ui/tui/tests.rs:1`), so tests reach every `pub(crate)` field and method directly and write
  state by assignment (`r.messages.push(…)` `2287-2290`, `r.show_subagent_panel = true` `855`,
  `r.last_plan_check = Instant::now()` `3914`, `r.focused_panel = …` `4581`).
- **`TestBackend` seam (19 occurrences):** the pattern is *render one pane into a synthetic terminal and
  read the buffer*, never the full frame. Canonical helpers:
  `draw_subagent_panel(r, width, height) -> (Vec<String>, Vec<Vec<Style>>)`
  (`803-828`: `TestBackend::new` `808`, `Terminal::new` `809`,
  `terminal.draw(|frame| r.render_subagents(frame, Rect::new(0,0,width,height)))` `810-815`,
  cell-text + cell-style extraction `816-827`) and `draw_status_line(r, width)` (`831-843`, 1-row
  terminal). Inline variants drive `render_chat` at 80×10 (`2292-2301` and `2328-2333`).
  `SPINNER_FRAMES` is asserted against (`846`).
- **What is deliberately *not* driven:** zero calls to `draw`, `render_terminal`,
  `drain_and_throttle_flush`, or `handle_events` in `tests.rs` — no synthetic `crossterm` key events
  are injected anywhere. Tests call the *effects* of keys instead: `submit()` (11 sites, e.g. `2325`,
  `3904`, `4634`), `paste_text()` (`4583`, `4631`), `set_input_content()` (`4603`), `cycle_focus()`,
  `upsert_subagent()`, `clamp_scrolls()`, and they assert on `r.rx.try_recv()` — the channel seam to the
  session loop (`3907`) via helper `drain_submissions(&mut r)` (`4524-4530`).
- **Throttle-aware test trick:** `poll_input()` is used as the loop-facing seam (`4557`, `4640`, `4642`)
  with `r.dirty = false` cleared first, explicitly "so the throttled flush has nothing to repaint and the
  test stays free of real render I/O" (`4554-4557`, `4637-4639`).
- **Event/state seams exercised at scale:** `on_event(&Event::…)` (92 sites, incl. `Delegation`
  lifecycle `335`, `363`, `390`, `518`, `605`, `639`), `set_subagents` (57), `capture_ui` (41) /
  `restore_panes` (38) / `announce_restore` (8), `token_witness_warnings` (5522, 5548), plus unit
  coverage of the formatting layer (`6-52`, `99-125`) and a recorded known limitation
  (`format_terminal_math` fence state is line-local, `3892-3898`).
- **Seams outside the unit module:** `src/ui/session_tests.rs:3832` builds a real
  `crate::ui::tui::TuiRenderer::new()` inside the `ui` test module; the integration test
  `tests/test_session_restore_panel_abort.rs` wraps a real `TuiRenderer` in a recording `Renderer`
  (`PaneWitness` `774-797`, `use marmennill::ui::tui::TuiRenderer` `99`) that delegates every trait
  method — including `init()` (`822`, i.e. the real raw-mode/alternate-screen path executes under
  `cargo test`), counted `flush()` (`828-831`), `set_subagents` capture (`847-850`), and
  `capture_ui`/`restore_panes` snapshotting (`857-864`). Other integration tests pin
  `ui_mode: "tui"` while driving the loop through the trait
  (`tests/test_panel_paused_indicator.rs:588`, `tests/test_session_abort_flush.rs:218`), and
  `tests/test_panel_paused_indicator.rs:281-288` documents the exact `render.rs` row-keying/filter
  predicates it depends on.
- **Net seam inventory for a rewrite:** (a) `TuiRenderer::new()` + direct field/method access from a
  `#[cfg(test)]` sibling; (b) per-pane `render_*(&mut Frame, Rect)` against `TestBackend` with
  buffer-text + style assertions; (c) the `mpsc` `tx`/`rx` pair as the input boundary; (d)
  `on_event(&Event)` as the state boundary; (e) the `Renderer` trait as the loop boundary (used by the
  integration tests and by `run_session`); (f) `capture_ui`/`restore_panes` as the persistence
  boundary. No seam exists for the real `crossterm` event stream, the throttle tiers, or
  `TuiRenderer::draw`'s layout.
