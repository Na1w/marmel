# UI state surface — what exists, what is persisted, what is lost

Crate `marmennill`, workspace `/home/fredrik/marmel/wip`, HEAD `a5e6763` (`1.0.0-refactor`).
Read-only research; no source file was modified. Question: can every UI pane/text buffer come back
byte-identical after restart ("exactly the right texts in all boxes")?

Headline: the **only** UI artefact written to disk is the semantic journal `.marmel/.ui_transcript.json`,
and **it is never read back on boot** (`src/ui/session.rs:302` = `UiTranscript::new()`; the only caller
of `UiTranscript::load` in the crate is a unit test at `src/ui/transcript.rs:335`). No renderer state of
any kind — text buffers, scroll, selection, panel visibility, composer — has an on-disk home.

## UiRecord & UiTranscript

`src/ui/transcript.rs:11-40` — `#[serde(tag = "type")] pub enum UiRecord`, 8 variants, all `String`/`Option<String>`:

| Variant | Line | Fields |
|---|---|---|
| `User` | 16 | `text: String` |
| `Assistant` | 18-23 | `content: Option<String>`, `thinking: Option<String>` (both `skip_serializing_if`) |
| `SteerResponse` | 25 | `text: String` |
| `ToolCall` | 27 | `display: String` |
| `ToolResult` | 29 | `display: String` |
| `TaskCompleted` | 31 | `task_id: String` |
| `TaskFailed` | 33-37 | `task_id: String`, `reason: Option<String>` |
| `Status` | 39 | `text: String` |

There is **no variant carrying subagent pane text** (`SubagentMessage` / `SubagentThinking` / subagent
log lines are rendered only in the TUI, see `src/ui/tui/mod.rs:519-555, 556-626`) and no variant
carrying any non-text UI state.

`UiTranscript` (`src/ui/transcript.rs:43-46`): newtype over a **private** `records: Vec<UiRecord>`.
API: `new` 49, `records()` 55, `is_empty` 59, `len` 63, `append` 67, `clear` 71, `load` **76-86**,
`save` **89-99**, `extract_steering_history` 113, `from_legacy_messages` 168 (production-dead: callers
are only `transcript.rs` tests 378/569/600/638).

- `load` (`:76`): missing file → `Ok(Self::default())` (`:78-80`); otherwise `fs::read_to_string` +
  `serde_json::from_str`, both wrapped in `anyhow::Context`. No schema/version field, no tolerance —
  a parse error is a hard `Err`, and nothing on the boot path calls it.
- `save` (`:89`): `create_dir_all(parent)` (`:91-94`) → `serde_json::to_string_pretty` (`:95`) →
  `std::fs::write` full-file overwrite (`:96-97`). **Not atomic** (no temp+rename) and **every call
  site discards the error** (`let _ = ui_transcript.save(...)`), so a corrupt/partial journal is silent.

Paths & lifecycle: `UI_TRANSCRIPT_FILE = ".ui_transcript.json"` (`src/manager/phase.rs:119`),
`Plan::ui_transcript_path()` = `.marmel/.ui_transcript.json` (`src/manager/phase.rs:302-304`).
Deleted by `Plan::clear` (`:415-422`), by plan archive (`:566-569`), and at `/reset` sites
`session.rs:363, 437, 1111, 1623, 1950`. Sibling file `.session_transcript.json`
(`TRANSCRIPT_FILE`, `phase.rs:117`, `plan.transcript_path()` `:298-300`) is the **LLM context**
transcript (`ctx.save_transcript`, e.g. `session.rs:690`) — not the UI surface.

## Renderer surface (today)

`src/ui/mod.rs:88-130` — the whole trait, with the UI state each method drives:

| Method | Line | State driven |
|---|---|---|
| `init` | 89 | terminal/raw-mode setup; TUI additionally seeds `plan_content`, `show_plan_panel`, `had_active_plan` from the on-disk plan (`tui/mod.rs:417-424`) |
| `on_event(&Event)` | 90 | all chat/subagent/status mutation (`tui/mod.rs:430-877`) |
| `flush` / `force_flush` | 91-94 | redraw (25 ms throttle, `tui/mod.rs:879-888`) |
| `poll_input` / `read_input` | 95-102 | composer → session loop (`tui/mod.rs:890-910`) |
| `request_abort` / `aborted` / `clear_abort` | 103-113 | `InputState.aborted` + TUI `confirm_abort`/`abort_armed_at` (`tui/mod.rs:920-945`) |
| `request_user_exit` / `user_exit_requested` | 114-121 | `InputState.user_exit` |
| `input_state` / `input_state_shared` | 123-125 | `InputState { aborted, user_exit }` (`mod.rs:75-80`) |
| `shutdown` | 126 | leave alt screen / flush |
| `set_subagents(Vec<SubagentDetail>)` | 127 | authoritative subagent list merge (`tui/mod.rs:957-1041`); called at `session.rs:833, 1205, 1336, 1713, 1912` |
| `set_thinking_budgets(&Config)` | 128 | `thinking_budgets` / `default_thinking_budget` (`tui/mod.rs:256-275`); called at `session.rs:267` |
| `reset_active_agent` | 129 | `active_agent = "Manager"` (`tui/mod.rs:1047-1049`); called at `session.rs:571` |

**The rehydration surface is gone.** `rehydrate_ui` / `rehydrate_messages` / `rehydrate_subagents`
were deleted (removal map: `docs/recon_rehydration.md` §B, lines 129-134 of `src/ui/mod.rs`, plus
overrides at `src/ui/tui/mod.rs:1074-1143` and `src/ui/raw.rs:161-208`). Verified: `grep -rn "rehydrate"
src/ --include=*.rs` → **0 hits**. `set_subagents` / `set_thinking_budgets` / `reset_active_agent`
survive unchanged. **Consequence: there is today no API through which any renderer could be handed
restored state.** Boot is always the default-constructed renderer.

## SubagentDetail fields

`src/ui/mod.rs:17-37` (`#[derive(Debug, Clone, Default)]` — **not `Serialize`**), the per-subagent pane model:

| Field | Line | Note for restore |
|---|---|---|
| `name: String` | 21 | role id, e.g. `coder-t-001`; the join key used everywhere (`tui/mod.rs:962`) |
| `task_id: Option<String>` | 22 | rendered in list + plan highlight (`tui/render.rs:516, 657-667`) |
| `prompt: String` | 23 | **never rendered**, but part of the model |
| `started_at: Option<Instant>` | 24 | monotonic — unserializable across processes |
| `worked_duration: Duration` | 26 | accumulated; drives `Ns` text (`render.rs:679-684`) |
| `last_activity_at: Option<Instant>` | 28 | monotonic |
| `logs: Vec<String>` | 30 | **pane text** (rendered `render.rs:816-830`) |
| `thinking: String` | 31 | **pane text** (rendered `render.rs:781-798`, gated on `show_thought`) |
| `content: String` | 32 | **pane text** (rendered `render.rs:799-815`) |
| `is_active: bool` | 34 | drives `Active`/`Idle`, spinner, panel auto-open |
| `context_tokens: usize` | 36 | rendered `render.rs:669-674` |

## Complete UI state inventory

Owner file:line = where the state lives. "Persisted?" = does a byte-identical copy reach disk today?

| State item | Owner file:line | Persisted? | How / why not |
|---|---|---|---|
| Chat pane text buffer `messages: Vec<String>` | `tui/mod.rs:43` | **LOST** | never serialized (`grep Serialize src/ui` → only `transcript.rs:12,43`). Nearest proxy: the semantic journal, never replayed |
| Streaming `current_thought` / `current_content` | `tui/mod.rs:45,47` | **LOST** | mid-turn partials; folded into `messages` by `commit_turn_content` (`events.rs:682-703`) |
| Assistant turn text | journal `session.rs:703-707` | **PARTIAL** | journaled from `ctx.messages()` last Assistant, not from screen bytes; can differ from `messages` |
| User lines (`User: …`) | journal `session.rs:381, 442, 1959` | **PARTIAL** | journal has raw `text`; the pane shows `"User: {trimmed}"` (`events.rs:787`) — prefix not in journal |
| Steer replies (`Marmennill: …`) | journal `session.rs:399`, `bridge/drain.rs:113,166,247` | **PARTIAL** | journal stores `text`; pane sentence-chunks it (`mod.rs:476-480`, `events.rs:821-839`) |
| Tool call/result lines | journal `session.rs:843, 1208, 1346, 1716` | **PARTIAL** | pane adds `[Tool Call] `/`[Tool Result] ` (`mod.rs:585, 618`), journal stores bare `display` |
| Local `[CLI]` lines (`/help`, `/thought`, `/reset`) | `events.rs:724-773` | **LOST** | produced only by the TUI, never journaled |
| Subagent pane text: `logs` / `thinking` / `content` | `mod.rs:30-32`, rendered `render.rs:781-830` | **LOST** | no `UiRecord` variant; `SubagentMessage`/`SubagentThinking` events come from `orchestrator/preemption.rs:221,227` and are never appended to the journal |
| Subagent list rows (name, ctx tokens, elapsed, status) | `render.rs:657-716` | **LOST** | derived from `Vec<SubagentDetail>` seeded empty (`session.rs:319`) |
| `is_active` / `task_id` / `context_tokens` | `mod.rs:22,34,36` | **LOST** | in-memory only |
| Plan pane text `plan_content` | `tui/mod.rs:49` | **PERSISTED-TODAY** | re-read from `.marmel/execution_plan.md` in `init` (`mod.rs:417-424`) and in `draw` (`render.rs:266-322`); archived variant `render.rs:296-303` |
| Plan active-task highlight `active_plan_task` | `tui/mod.rs:135` | **LOST** | derived from live `subagents`; reset at `mod.rs:1032` |
| Plan elapsed ("Elapsed: …") | `tui/mod.rs:289-325`, `render.rs:1036-1043` | **LOST** | from `PLAN_STARTED_AT`/`PLAN_COMPLETED_AT` process statics (`phase.rs:179-183`) |
| Status line `status_line` | `tui/mod.rs:68` | **PARTIAL/LOST** | live `Event::Status` writes it (`mod.rs:633`) with no journal write (`session.rs:393-396`); only a few notices are journaled as `UiRecord::Status` (`session.rs:1248,1367,1758,1873,2117,2172,2206`); boot default `"Ready"` (`mod.rs:188,897`) |
| **Input/composer text buffer** `input_text` + `textarea` + `cursor` | `tui/mod.rs:51,53,56` | **LOST — not persisted at all** | no writer anywhere; `submit` drops it (`events.rs:708-715`); `set_input_content`/`ensure_textarea_in_sync` (`events.rs:338-360`) show a restore would just need lines+cursor |
| Composer history `history` / `history_index` / `input_draft` | `tui/mod.rs:91-95` | **LOST** | pushed at `events.rs:779-784`; no disk write |
| Undo/redo stack (inside `TextArea`) | `tui/mod.rs:51` (`events.rs:204-208`) | **LOST** | `ratatui_textarea` internal state |
| Scroll: `chat_scroll`, `plan_scroll`, `subagent_scroll`, `subagent_list_scroll`, `plan_max_scroll` | `tui/mod.rs:72,74,87,89,133` | **LOST — not persisted at all** | mutated only in `events.rs:487-568`, clamped at `events.rs:469-480` |
| Auto-scroll flags `chat_auto_scroll`, `subagent_autoscroll`, `plan_auto_scroll` | `tui/mod.rs:132,124,134` | **LOST** | user-driven (`events.rs:201, 578`) |
| `focused_panel` (Chat/Plan/Subagents) | `tui/mod.rs:70`, enum `:34-38` | **LOST** | `cycle_focus` `events.rs:796-818`; default `Chat` |
| `show_subagent_panel` | `tui/mod.rs:82` | **LOST** | default `false` (`:198`); auto-true at `events.rs:595`, `mod.rs:1004`; toggled `events.rs:575-587` |
| `show_plan_panel` | `tui/mod.rs:76` | **DERIVED** | re-derived from disk plan at `mod.rs:422`, `render.rs:316` — user's manual Ctrl+P hide is not remembered |
| `show_thought` | `tui/mod.rs:80` | **LOST** | default `false` (`:197`), toggled `events.rs:197, 722`; also gates cache invalidation (`render.rs:25-35, 101-104`) |
| Thinking budgets `thinking_budgets`, `default_thinking_budget` | `tui/mod.rs:159,161` | **PERSISTED (via Config, not UI state)** | rebuilt from `Config` at boot (`mod.rs:256-275`, called `session.rs:267`); rendered `render.rs:880` |
| `subagent_turn_thinking`, `subagent_is_thinking` | `tui/mod.rs:163,165` | **LOST** | per-turn streaming maps |
| Agent tab state `active_agent` | `tui/mod.rs:142` | **LOST** | default `"Manager"` (`:236`), reassigned `mod.rs:729, 790, 852, 1018`, reset `:1047` |
| `selected_subagent_idx` | `tui/mod.rs:85` | **LOST** | `events.rs:94, 111, 636`, `mod.rs:1037` |
| Token/context counters `tokens_in/out`, `orchestrator_context_tokens` | `tui/mod.rs:153-157` | **LOST** | rendered `render.rs:1012-1015` (`max` with per-process `llm::get_global_token_counts()` → resets to 0 on restart) |
| Modal/notice state `confirm_abort`, `abort_armed_at`, `InputState` | `tui/mod.rs:58,60`, `mod.rs:77-78` | **LOST** (deliberately transient) | 3 s window (`mod.rs:344-349`); rendered as `[ABORT ARMED]` bar `render.rs:1016-1024` |
| Timing/anime `frame_counter`, `session_start`, `waiting_for_token_since`, `last_render` | `tui/mod.rs:66,150,144,143` | **LOST** | spinner phase + `(Ns)` suffix in status (`render.rs:1001-1010`) |
| Layout caches (`chat_area`, `*_width`, cached rendered lines) | `tui/mod.rs:100-122,137-140` | **DERIVED** | recomputed per frame; must not be restored |
| `RawRenderer` state | `raw.rs:45-48` | n/a | only `buffer: Vec<u8>` (flushed to stdout, `raw.rs:84-90`) + `input_state`. Nothing to restore; headless mode has no boxes |

### Where `UiTranscript::save` is called, and whether `load` runs on boot

`UiTranscript::save` call sites in `src/ui/session.rs` (all `let _ =`):
**382, 400, 445, 707, 844, 985, 1078, 1204, 1211, 1249, 1347, 1370, 1511, 1590, 1712, 1719, 1759,
1874, 1960, 2118, 2173, 2207** — plus journal *deletions* at 363, 437, 1111, 1623, 1950.
`bridge/drain.rs:99,113,166,247` and `helpers.rs:320+` append records but never save (the session
caller saves afterwards).

**`UiTranscript::load` is never called on boot.** Boot is `let mut ui_transcript = UiTranscript::new();`
(`session.rs:302`); the sole in-crate `load` call is the test at `transcript.rs:335`. The file exists on
disk in the live workspace (`.marmel/.ui_transcript.json`, 26 records) and is simply unread.

## Gaps that break "exact text in all boxes"

1. **Nothing is read back.** Even the one journaled artefact is write-only (`session.rs:302`), and the
   `Renderer` trait has no rehydrate/restore hook any more (`ui/mod.rs:88-130`).
2. **Journal ≠ screen bytes.** No function anywhere maps `UiRecord → chat line`. The pane adds
   `"User: "` (`events.rs:787`), `"[Tool Call] "`/`"[Tool Result] "` (`mod.rs:585,618`),
   `"Marmennill: "` (`events.rs:825-836`), `<think>\n…\n</think>\n` fold (`events.rs:683-688`), and
   `"[CLI] …"` locals (`events.rs:724-773`). Replaying the journal would produce *different text*.
3. **Subagent panes have no journal at all** — `logs`, `thinking`, `content` (`mod.rs:30-32`) exist only
   in RAM; there is no `UiRecord` for them.
4. **Source divergence**: `UiRecord::Assistant` comes from `ctx.messages()` (`session.rs:691-708`), the
   pane from streamed `Event::Message`/`Thinking`; and when `active_agent` is an active subagent those
   chunks route into `sa.content`/`sa.thinking` instead of the chat pane (`mod.rs:444-457, 492-505`).
5. **The composer is invisible to persistence** — `input_text`/`cursor`/`textarea` (`mod.rs:51-56`) are
   never written; a half-typed message vanishes on restart.
6. **Scroll/selection/focus/visibility are never written** — `chat_scroll`, `plan_scroll`,
   `subagent_scroll`, `subagent_list_scroll`, `selected_subagent_idx`, `focused_panel`,
   `show_subagent_panel`, `show_thought`, all three auto-scroll flags.
7. **Time state is monotonic-clock based** — `Instant`/`Duration` in `SubagentDetail.started_at`,
   `last_activity_at`, `worked_duration`, plus `PLAN_STARTED_AT`/`PLAN_COMPLETED_AT` (`phase.rs:179-183`)
   and `session_start`. Every rendered duration (`render.rs:679-684, 1001-1003, 1036-1043`) will differ
   after restart unless wall-clock anchors are persisted.
8. **Token counters reset** (`render.rs:1012-1014`), so the status line cannot match byte-for-byte.
9. **Journal durability**: full-file `fs::write` with no atomic rename, all errors discarded; and it is
   *deleted* by `/reset` (`session.rs:363,437,1111,1623,1950`), `Plan::clear` (`phase.rs:415-422`) and
   plan archive (`phase.rs:566-569`) — restore must not assume it survives those paths.

## Implications for the restore design

1. **Two artefacts, not one.** (a) keep `.ui_transcript.json` as the *chat* journal; (b) add a separate
   `.marmel/.ui_state.json` for the non-chat surface (composer buffer + cursor, scroll offsets,
   `selected_subagent_idx`, `focused_panel`, `show_subagent_panel`/`show_plan_panel`/`show_thought`,
   auto-scroll flags, `history`, `active_agent`, subagent list incl. per-pane text). `UiTranscript` is
   append-only text; it cannot host view state.
2. **Make the journal and the screen share one formatter.** Introduce a single pure function
   (`fn chat_line(&UiRecord) -> String` living with `UiRecord`) and route every live chat append through
   it, so replay is byte-identical *by construction* instead of a re-implementation of
   `events.rs`/`tui/mod.rs` prefixes. Today the formatting is duplicated implicitly and partially
   (gap 2/4 above).
3. **Add subagent journal variants** (`SubagentThinking`/`SubagentContent`/`SubagentLog`/
   `SubagentLifecycle`, keyed by `name` + `task_id`) so per-pane text is recoverable; `SubagentDetail`
   needs `Serialize`/`Deserialize` and `started_at`/`last_activity_at` must become wall-clock
   (`SystemTime`/`DateTime<Local>`) with `worked_duration` as an explicit persisted number.
4. **Re-add a renderer restore hook.** The deleted `rehydrate_*` trio (`docs/recon_rehydration.md` §B)
   was the only path into renderer state; the design needs an equivalent — a single
   `restore_state(&mut self, &UiState, &UiTranscript)` on `Renderer` (`ui/mod.rs:88`) called between
   `init` and the first `read_input` (`session.rs:266`, `session.rs:347`), so the very first frame
   already shows the restored panes.
5. **Composer writes must be cheap and safe**: debounced atomic write (temp + rename) on input change;
   `save` today is a full pretty-printed rewrite of a 64 KB file per event, which does not scale to
   per-keystroke.
6. **Restore order matters**: plan pane text is already exact (disk-derived, `render.rs:266-322`);
   chat/subagent text must be replayed in journal order; scroll positions only after content is in place
   (auto-scroll at `render.rs:324-344` currently overwrites any restored offset unless the
   `*_auto_scroll` flags are restored first).
