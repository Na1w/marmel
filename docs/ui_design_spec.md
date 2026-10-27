# Marmel TUI — UI design spec (branch `feature/new-ui`)

Implementation contract for the TUI rebuild. Claims about **existing** code are cited `file:line` from
`docs/research_ui_tui_arch.md` (ARCH), `docs/research_ui_dataflow.md` (DATA), `docs/research_ui_ux_baseline.md`
(UX). Sections 0–10 are normative: the research docs describe what exists today, this file states what we build.

## 0. Frozen invariants (do not change)

- Dependencies (Cargo.toml:33-39): ratatui 0.30, crossterm 0.29, ratatui-textarea 0.9, tui-markdown 0.3, ansi-to-tui 8.0. No new crates. Widgets used: Block, Paragraph, Span, Line, Layout, Constraint::{Length,Min,Percentage}, Scrollbar, TextArea, plus the existing markdown/ANSI bridge (formatting::render_markdown_lines src/ui/tui/formatting.rs:1148, parse_ansi_lines :1138).
- Lints: edition 2024; unused, dead_code, unused_imports and clippy all denied. Every symbol introduced here is called by a pane, a key handler, or a test. No speculative public API; adding #[allow(dead_code)] is forbidden.
- Renderer contract unchanged: the Renderer trait (src/ui/mod.rs:144) keeps its signatures; the session loop still drives read_input/poll_input/flush (src/ui/session.rs:1965,2080,2822; ARCH §2). dyn Renderer stays Send, not Sync (src/ui/mod.rs:192).
- Event source unchanged: synchronous crossterm::event::poll/read (src/ui/tui/events.rs:19-27), one event per blocking call (:302-304). No EventStream (feature enabled, never used — ARCH §1), no input thread, no new channel.
- Cadence constants unchanged: 30 ms input poll (events.rs:19), 25 ms dirty floor (mod.rs:434,1097), 125 ms spinner tier (mod.rs:438), 3 s abort-arm window (mod.rs:410-415), 250 ms plan-mtime cache (render.rs:272), 125 ms spinner phase (render.rs:1109), 200-line log cap (mod.rs:470, merge_logs :496-512).
- Test seams preserved: per-pane render_*(&mut Frame, Rect) against TestBackend (src/ui/tui/tests.rs:803-828,831-843), the mpsc pair as input boundary (mod.rs:165-166; drain_submissions tests.rs:4524-4530), on_event(&Event) as state boundary, Renderer as loop boundary (src/ui/session_tests.rs:3832, tests/test_session_restore_panel_abort.rs, tests/test_panel_paused_indicator.rs:588, tests/test_session_abort_flush.rs:218), capture_ui/restore_panes as persistence boundary (mod.rs:1291,1400).
- Terminal setup/teardown unchanged: init (mod.rs:515-547), shutdown (:1160-1169), Drop (:1829-1833), leave_alt_screen (:1836-1847). DisableMouseCapture stays in both teardown paths as defensive idempotent cleanup even though capture is never enabled — the `shutdown()` teardown and the `leave_alt_screen()` teardown in `src/ui/tui/mod.rs` (the import plus exactly those two call sites; see §10.20).

## 1. Design goals (measurable)

- G1 Plan progress always visible — done/total + active task id rendered in every frame at >=40x10, plan pane open or not — today never rendered; summary helper feeds only the arbitrator prompt (ui/helpers.rs:37, bridge/arbiter.rs:586).
- G2 Keymap discoverable in one keystroke — ? or F1 shows the full legend; /help prints the identical table — today legend hard-coded and stale (events.rs:789-813).
- G3 No destructive action un-confirmed — /reset*, /exit*, /cancel <id>, x each open a confirm overlay; zero bare words exit — today bare stop/cancel/exit/quit/q/abort exit immediately (helpers.rs:195-207, session.rs:2081-2084); /reset deletes un-prompted (events.rs:814-834).
- G4 One-glance live status — tokens in/out, ctx %, elapsed, active/paused/queued counts, max validation round present at >=90 cols and still present while abort armed — today 1-row bar (render.rs:357-359), armed banner erases metrics (:1117-1125).
- G5 No silent key press — every toggle leaves a persistent state chip; toggling an empty pane shows its empty state — today Ctrl+A is a silent no-op with no workers (events.rs:598-609 vs render.rs:368,393-394).
- G6 Focus visible, typing never swallowed — one focus badge per pane title + input border colour; printable keys always reach the composer — today focus = border colour only and Delete/Backspace/printables dead unless Chat focused (events.rs:141-154,214-220).
- G7 Scrollback honest — "↓ N new" / "LIVE" chip; new output never moves the viewport unless follow is on — today auto-scroll re-armed on submit/toggle (events.rs:784,846, render.rs:464-469), no behind-indicator (events.rs:509).
- G8 Roster and errors complete — every live worker reachable (no 3-row silent cap); last error sticky + recallable (e, /retry) — today list clamped to 3 rows (render.rs:647-653), errors one categorical string (session.rs:2345-2356).

## 2. Layout spec

### 2.1 Region budget (default, terminal >= 100x30)

chrome = header 1 + status 2 + input 3..9 + footer 1; the body absorbs the remainder through `Constraint::Min`.
Root split (replaces `render.rs:354-364`, which was `[Min(3), Length(1), Length(3)]`):

```rust
// components/layout.rs — compute_layout(Size) -> Panes
Layout::vertical([
    Constraint::Length(1),        // header
    Constraint::Min(6),           // body  (absorbs all slack; hard floor 6)
    Constraint::Length(2),        // status (2 rows at >= 20 rows, 1 row below)
    Constraint::Length(input_h),  // input, input_h = (draft_lines + 2).clamp(3, 9)
    Constraint::Length(1),        // footer legend (0-length only by the <20-row rule in 2.5)
]).split(size)
```

`input_h` comes from the `TextArea` line count (mod.rs:89) — the composer grows with the draft instead of
being fixed at 3 (UX friction 13); `Min(6)` keeps the body >= 6 rows while chrome is at its default size.

### 2.2 DEFAULT 100x30 — mode M4 Split, focus Transcript (body = 23 rows)

**Boot frame vs. this worked frame (normative).** The frame below is **M4 Split**, the §4.1 default *mode*; it
is **not** the boot frame. `TuiRenderer::new` constructs `view_mode: state::ViewMode::for_panes(false, false)`
(`src/ui/tui/mod.rs`, the `view_mode:` line of the constructor — currently :433), i.e. **M1 Transcript** with
both side panes closed and the composer focused (`focus: state::FocusPane::Input`). Contract, stated plainly:
**the boot state is M1 Transcript** — that is what the `Marmel M1 Transcript …` frame at 100x30 in
`docs/new_ui_preview.txt` (case a, "idle/empty boot state") and the boot frame tests pin — while **M4 Split is
the contract for the 100x30 layout budget drawn below**, the mode the operator reaches with `F2`/`F5`,
`/mode split`, or by switching a side pane on with `Ctrl+P`/`Ctrl+A`. Nothing in §2 is mode-conditional: the
chrome, the region budget and the §2.5 ladder are identical in M1 and M4.

````
header Length(1): Marmel — M4 Split — 3/7 done — > t-004 — val 1 — ctx 41% — 12:03
body: left column Transcript (Min(6)), right column Plan (top) + Workers (bottom)
status: tokens in/out, ctx %, elapsed, active/paused/queued, validation round
+--------------------------------------------------------------------------------------------------+
| Marmel  M4 Split  3/7 done  > t-004  val 1  ctx 41%  12:03                                       |  header
|+- Transcript ● ─────────────────────────────────────────+ +- Execution Plan ───────────────────+ |
|| User: rebuild the TUI on feature/new-ui                | | ## Tasks                            | |
|| [t-007] Specialist coder_specialist started task …     | | - [x] t-006 research baseline       | |
|| [Arbitrator]: forwarded notice notice-3 to coder       | | - [x] t-008 layout spec             | |  body
||                                                        | | - [ ] > t-004 wireframe  (▶ coder)  | | 23 rows
||   (streaming content region, markdown via 1148)        | | - [ ] t-011 keymap                  | |
||                                           |█|          | | Overall Progress: 3/7 (42%)          | |
||                                           | |          | +- Workers ─────────────────────────--+ |
||                                           |▼| 2 new    | | coder_specialist  ▶ (Active)         | |
|+------------------------------------------+---+         | | validator         ⏸ (Queued 2/3)     | |
|                                                          | | writer            (Idle)             | |
|                                                          | +- Details for coder_specialist -----+ |
|                                                          | | [Output]  ```rust … (ANSI: 1138)   | |
|                                                          | +- - - - - - - - - - - - - - - - - - -+ |
|+- Tokens in 12.4k / out 3.1k | ctx 41% | 12:03 | 1a 0p 2q | val 1 - - - - - - - - - - - - - - - - +|  status
|| ● LIVE  plan:on  workers:on  think:off  follow:on      |                                        |  row 2
|+- Input (Enter send, Alt/Shift+Enter newline, Ctrl+Z undo)-+                                     |  input
|| > rebuild the TUI…                                                              |               |  3..9
|+- - - - - - - - - - - - - - - - - - - - - - - - - - - - -+                                      |
| F1 help  Tab focus  F2 mode  F3/F4/F5 panes  x cancel-worker  e last error  Esc arm/overlay  Ctrl+A/P/T |  footer
+--------------------------------------------------------------------------------------------------+
````

Header chip order and drop priority are normative (§7.4). `val 1` may only be rendered after plumbing
delta **D1** (§9.3): per-worker `validation_rounds` is unprojected today (DATA §4.1, gap 1).
`ctx %` = `TuiRenderer::orchestrator_context_tokens` (mod.rs:195) ÷ `config.max_context_tokens`
(src/config.rs:262) — **spot-check source**, labelled as such on screen by the fixed label `ctx`.
No arbitration-round counter is ever rendered: `round_started(round)` discards its argument in both hosts
(DATA gap 9), so the number is not data.

Other modes at the same 100x30 budget (schematic, not to scale; chrome = header/status/input/footer is
identical in every mode, which is what makes G1/G2/G4 mode-independent):

```
M1 Transcript (body 23)   M2 Plan (body 23)               M3 Workers (body 23)
+----------------------+  +---------------+-----------+   +---------------+-----------+
| Transcript ●         |  | Execution Plan|Transcript |   | Workers       |Transcript |
|  width 98, Min(6)    |  | Percentage(55)| ●         |   | Percentage(55)| ●         |
|  absorbs all 23 rows |  | - [x] t-006   |Percent(45)|   | ▶ coder  Min(4)|Percent(45)|
|  ...scrollback...    |  | - [ ] > t-004 |  width 43 |   | ⏸ validator   | width 43  |
|  ▼ scrollbar         |  | 3/7 (42%) ▼   |  ▼ scr    |   | detail Min(3) |  ▼ scr    |
+----------------------+  +---------------+-----------+   +---------------+-----------+
   Plan/Workers ABSENT       Transcript focusable          roster never clamped to 3
```

### 2.3 Body constraints per mode

| Mode | Body split | Explicit `Constraint` list |
|---|---|---|
| M1 Transcript | Transcript fills the body | `Layout::vertical([Min(6)])` inside body |
| M2 Plan | Plan left, Transcript right | `Layout::horizontal([Percentage(55), Percentage(45)])`, each side `vertical([Min(6)])` |
| M3 Workers | Workers left, Transcript right | `Layout::horizontal([Percentage(55), Percentage(45)])`; Workers `vertical([Min(4), Min(3)])` (roster / detail) |
| M4 Split (default) | Transcript left, side column right | `Layout::horizontal([Percentage(60), Percentage(40)])` (as today, `render.rs:371-378`), side `vertical([Min(6), Min(6)])` (Plan top, Workers bottom, replacing the `Percentage(50)/Percentage(50)` pair at `render.rs:385-395`) |

Every pane draws its own `Block` + right-side `Scrollbar` when scrollable (pattern kept from
`render.rs:478-492`, `616-620`, `1013-1028`). Pane minimums inside a pane (Workers roster `Min(4)`,
detail `Min(3)`) replace the current `Length(list_len)` + `Min(3)` pair at `render.rs:647-653`, whose
`.clamp(1, 3)` is the 3-row silent cap G8 removes.

### 2.4 Zero-rect rule

A pane is either **absent** (mode does not include it → it is never split, never drawn, never hit-tested)
or **present with width >= 1 and height >= 1**. No pane may be rendered into a 0x0 or 0xN rect. Rules:

1. `compute_layout` returns `Option<Rect>` per pane; absent panes are `None`, not zero rects.
2. The `Min()` floors above are only satisfiable when `body.height >= 6 && body.width >= 20`; when the
   terminal is below that, the degradation ladder in 2.5 changes the mode *before* splitting, so the
   split never receives an area smaller than its floors.
3. `input_h` is clamped by `min(available - 1(header) - 1(status) - 1(body))` before the root split, so the
   composer can never starve the body.
4. Scroll offsets are re-clamped on resize (`clamp_scrolls`, events.rs:488-504) and geometry is written
   back after the draw, as today (render.rs:419-421).

### 2.5 Narrow-terminal degradation (applied in order, first match wins)

| Condition | Effect |
|---|---|
| `width < 90` | M4 → single-column body: Transcript + **only the focused side pane** (`vertical([Min(6), Min(6)])`); the other side pane is absent. The header already carries plan progress (G1), so nothing important is lost. |
| `width < 70` | header drops `val`, then `ctx`, then `elapsed` (drop order §7.4); status row 1 keeps tokens + active/paused/queued; footer legend switches to the short form `F1 help  Esc arm  Enter send`. |
| `rows < 20` | status collapses to 1 row (metrics only; an armed-abort prompt is appended to it), footer is **dropped** (Length(0) at the root level is an explicit rule, not a pane rect), `input_h` becomes `(draft_lines + 2).clamp(1, 3)` with a `+n` overflow marker in the border title. |
| `width < 60` | Workers detail pane is absent (roster only); M2/M3 revert to M1 with the focused side pane as the single column. |
| `width < 40 \|\| rows < 10` | **too-small notice**: exactly one pane, `Block` + centred `Paragraph`: `Terminal too small — Marmel needs at least 40x10 (currently WxH). Resize to continue.` Header/status/input/footer are all absent (never zero-sized). Key handling keeps working (§4.4); nothing else is drawn. |

## 3. Pane inventory

New code lives under `src/ui/tui/components/` (dir is new; today `src/ui/tui/` holds only
`mod.rs`, `render.rs`, `events.rs`, `formatting.rs`, `tests.rs`). `TuiRenderer` keeps ownership of all state
(ARCH §3) and keeps the `render_*(&mut Frame, Rect)` methods that tests call (tests.rs:803-828); each method
becomes a thin call into the component with a borrowed `FrameContext`.

| Pane | Module file | Exact data source (type / field) | Shows | Interaction |
|---|---|---|---|---|
| Header | `components/header.rs` | `PlanProgress` computed in-spec from `TuiRenderer::plan_content` (mod.rs:87) using `plan_parse::CheckboxState` (plan_parse.rs:50-55) + `SubagentDetail::task_id` / `active_plan_task` (mod.rs:171-173, ui/mod.rs:40) | `3/7 done`, `> t-004`, `ctx %`, `elapsed`, mode id, `val N` (only after D1) | none (read-only); `F1` for legend |
| Status bar | `components/status_bar.rs` | `tokens_in`/`tokens_out`/`orchestrator_context_tokens` (mod.rs:191-195), `session_start` (mod.rs:187), `status_line` (mod.rs:106), `ACTIVITY_STATUS_KEYWORDS` spinner tier (mod.rs:56), counts over `subagents: Vec<SubagentDetail>` (`is_active`/`paused`/`queue`, ui/mod.rs:55,60,76) | tokens in/out, ctx %, elapsed, active/paused/queued counts, max validation round (post-D1); row 2 = state chips + abort-arm prompt | none; chips are state echoes (G5) |
| Plan / task overview | `components/plan.rs` | `plan_content` (mod.rs:87) polled from `.marmel/execution_plan.md` with the 250 ms mtime cache (render.rs:266-322); active-task set as today (render.rs:512-538) | plan markdown, checkbox colouring, `done/total` line, active-task highlight, `No active execution plan.` empty state | `PageUp/Down`, `Home/End`, `Ctrl+P` toggle, `Tab` focus |
| Transcript | `components/transcript.rs` | `messages: Vec<String>` (mod.rs:81) via `formatting::render_markdown_lines` (1148) / `parse_ansi_lines` (1138); `current_thought`/`current_content` (mod.rs:83,85), `steer_sentence_buffer` (:135) | scrollback + streaming region + `↓ N new`/`LIVE` chip + scrollbar | `PageUp/Down`, `Home/End`, `Ctrl+F` follow toggle, `e` last error |
| Worker activity feed | `components/workers.rs` | `subagents: Vec<SubagentDetail>` (mod.rs:121) plus the per-frame `orchestrator::worker_entries()` snapshot (render.rs:659) and badge from `helpers::subagent_row_badge` (helpers.rs:627) | roster (state glyph, ctx, elapsed), selection highlight, roster `n/total`, detail `[Thinking]/[Output]/[Logs]` (logs merged + capped at 200, mod.rs:496-512) | `Tab` focus, `←/→` select worker, `↑/↓` scroll, `x` cancel selected |
| Input / composer | `components/composer.rs` | `textarea: ratatui_textarea::TextArea` (mod.rs:89), `input_text`/`cursor` (:91,:93), `history`/`history_index`/`input_draft` (:129-133) | draft with real height 3..9, caret, `+n` overflow marker, slash-command hint in the border title | typing (always), `Enter`, history, undo/redo, paste (events.rs:288-291,319-338) |
| Help / keymap overlay | `components/help_overlay.rs` + `components/keymap.rs` | `keymap::legend()` — generated from the same table the key handler matches on (replaces the hard-coded legend, events.rs:789-813) | full key table + slash-command table with argument hints | `?` (empty composer) / `F1` open; `Esc` close (no arming) |
| Confirm overlay | `components/confirm_overlay.rs` | `confirm_action: Option<ConfirmAction>` (`src/ui/tui/events.rs`) — exactly `ConfirmAction::{Quit, CancelWorker{key,task_id}, CancelAll, Command{line}}`; the overlay text is built by `help::ConfirmRequest` | what will happen, what it touches, `y`/`n` prompt | `y` yes, `n` no, `Esc` dismiss; all other keys ignored |
| (support) layout | `components/layout.rs` | pure function over `Size` + pane visibility | `Panes { header, body, status, input, footer, per-pane Option<Rect> }` | — |
| (support) palette | `components/palette.rs` | `Color`/`Style` constants + `Glyph` enum | §7 | — |

**Confirm-action inventory (normative; matches `enum ConfirmAction` in `src/ui/tui/events.rs`).** There are
exactly four confirm surfaces: `Quit`, `CancelWorker { key, task_id }`, `CancelAll`, `Command { line }`. There
is no `ResetPlan` variant and no `ClearTranscript` confirm surface — the renderer never clears the transcript
itself, so nothing can confirm such an action. Transcript clearing happens **only** through `/reset` (all six
spellings, `is_reset_command` in `src/ui/helpers.rs`): the confirm arms a `Command { line }`, and on `y` the
verbatim line goes to the session loop, whose reset path clears the plan, the bundle, the steering history and
the transcript (§6, G3).

## 4. View modes & focus model

### 4.1 Modes

`enum ViewMode { Transcript, Plan, Workers, Split }` — stored on the renderer (`view_mode`), persisted
(§9.4). Default `Split` (M4) — the default *mode*, not the boot frame: a fresh renderer boots in M1 Transcript
with both side panes closed (see §2.2). Keys: `F2` cycles M4→M1→M2→M3→M4; `F3` M1; `F4` M2; `F5` M3. `Ctrl+P` /
`Ctrl+A` keep toggling the Plan / Workers pane (events.rs:176-184, 185-194): in M1 they switch the mode so
the toggle is never a silent no-op (G5, UX friction 3), and an empty pane toggles **on** and renders its
empty state (§8.1).

### 4.2 Focus

`enum FocusPane { Input, Transcript, Plan, Workers }` replaces `FocusedPanel { Chat, Plan, Subagents }`
(mod.rs:34-38). `Tab` cycles Input→Transcript→(Plan)→(Workers)→Input skipping absent panes; `Shift+Tab`
reverses (kitty-protocol terminals only, same caveat as `Shift+Enter`, mod.rs:517-521). `Esc` no longer
re-focuses Transcript while it arms abort (UX friction 8) — it arms abort, or closes an overlay.
Focus is drawn as a colour-independent badge `●` in the pane title **and** the focus border colour (§7.3),
so the operator always knows whether they are typing into the composer (UX friction 14, G6).

### 4.3 Per-mode emphasis

| Mode | Emphasis | Present panes |
|---|---|---|
| M1 Transcript | transcript full body width, streaming region largest | Transcript (+ header/status/input/footer) |
| M2 Plan | Plan 55 % left, auto-scroll to active task (render.rs:579-600 pattern kept) | Plan, Transcript |
| M3 Workers | Workers 55 % left, roster never truncated (G8) | Workers, Transcript |
| M4 Split | default 60/40 as today; Plan top / Workers bottom in the side column | Transcript, Plan, Workers |

Header and status bar are identical in all modes — plan progress, live counters and the key legend are never
mode-conditional (G1, G4, P1.1).

### 4.4 The composer rule

**Printable keys always reach the composer, in every mode and every focus state.** Concretely:
`Char`, `Shift+Char`, paste, `Backspace`, `Delete`, `Ctrl+Z/Y/J` are routed to `textarea.input(key)`
regardless of `focus` — this closes the dead-key hole at events.rs:141-154,214-220. Only non-printable
navigation/shortcut keys are interpreted as commands when a non-input pane is focused
(`↑↓←→`, `Home/End`, `PageUp/PageDown`, `Tab`, `F1–F5`, `Ctrl+A/P/T/F/Z/Y`, `Esc`, `Ctrl+C`, `Ctrl+D`).
Single-letter shortcuts (`x`, `e`, `?`) are honoured **only when the composer is empty**; if the draft is
non-empty they insert literally, because that rule protects typing (G6) which outranks mnemonics.

## 5. Keymap

`components/keymap.rs` holds one table; `handle_events` matches it and `keymap::legend()` renders the same
table into the footer, the `F1` overlay and `/help` — one source, so the legend cannot go stale
(UX friction 1, defect 1).

| Key | Action | Scope |
|---|---|---|
| `Enter` | submit composer (events.rs:32,41 → 764) | composer |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+Enter`, `Ctrl+J`, `Char('\n')` | insert newline (events.rs:33-40, 210-213) | composer |
| printable / `Char` / paste | insert in composer — **always**, any focus (fixes events.rs:214-220; paste events.rs:288-291,319-338) | composer |
| `Backspace`, `Delete`, `Char(0x08)`, `Char(0x7f)` | edit composer — **always**, any focus (fixes events.rs:141-154) | composer |
| `Ctrl+Z` / `Ctrl+Y` | undo / redo (events.rs:202-209) | composer |
| `Ctrl+Up` / `Ctrl+Down` | input history prev / next (events.rs:65-66,78-79) | composer |
| `Up` / `Down` | composer empty & caret on first line: history; otherwise scroll focused pane ±1 (events.rs:67-74,80-87) | composer / pane |
| `Left` / `Right` | Workers focused: prev/next worker; else move caret (events.rs:90-122) | pane / composer |
| `Home` / `End` | composer: line start/end; pane: top/bottom (events.rs:123-140) | pane / composer |
| `PageUp` / `PageDown` | scroll focused pane ±10 (events.rs:62-63) | pane |
| `Tab` / `Shift+Tab` | cycle focus fwd / back (§4.2); `Tab` also accepts a slash completion (§6) | global |
| `F1` / `?` (empty composer) | open keymap+command overlay (G2) | global |
| `F2` / `F3` / `F4` / `F5` | mode cycle / Transcript / Plan / Workers | global |
| `Ctrl+P` / `Ctrl+A` | show Plan pane / show Workers pane (events.rs:176-184,185-194) | global |
| `Ctrl+T` | toggle thinking display (events.rs:195-201); state chip, not a chat line (G5) | global |
| `Ctrl+F` | toggle transcript follow (LIVE / ↓ N new) (G7) | global |
| `x` (empty composer, Workers focused) | cancel **selected** worker → confirm overlay → `orchestrator::cancel_active_worker(key, task_id)` (workers.rs:1455); **never** `cancel_all()` (bus.rs:38) | pane |
| `e` (empty composer) | show last sticky error in the transcript | global |
| `Esc` | overlay open → close; else arm abort (3 s window, mod.rs:410-415); armed again → `request_user_exit` (events.rs:44-59) — never re-focuses | global |
| `Ctrl+C` | same as `Esc` (events.rs:156-172) | global |
| `Ctrl+D` | open the `Quit` confirm overlay — never exits un-armed | global |
| printable while armed | disarm (events.rs:214-216) | global |
| `y` / `n` | honoured **only inside** the confirm overlay | overlay |
| resize | clamp scrolls + force redraw (events.rs:293-298) | global |

### Deprecated / deleted (with reasons)

- **All mouse arms (events.rs:226-287) deleted.** `EnableMouseCapture` is executed nowhere in `src/`, so the
  block is unreachable in practice (UX §1 "Unreachable bindings", defect 2); the `/help` legend advertised it
  (events.rs:806). `DisableMouseCapture` remains in both teardown paths only (the `shutdown()` and
  `leave_alt_screen()` teardowns in `src/ui/tui/mod.rs`) per §0 and §10.20.
- **`Ctrl+D` as immediate quit (events.rs:173-175) removed.** A single un-armed keystroke ended the session
  with no arming and no confirm; G3 requires an arming or confirm step on every exit path.
- **Bare words `stop`, `cancel`, `exit`, `quit`, `q`, `abort` no longer terminate the session.** They are
  ordinary steering text again; only slash forms (`/exit`, `/quit`, `/q`, `/abort`, `/stop`, `/cancel`) exist,
  and each opens the confirm overlay. Today `is_abort_command` matches all of them (helpers.rs:190-208) and
  mid-flight they exit immediately (session.rs:2081-2084), so a one-word instruction such as "stop" killed the
  run (UX friction 6). `is_abort_command` shrinks to slash spellings and keeps the single-line rule
  (helpers.rs:183-186) so a pasted multi-line block is still delivered verbatim.
- **`Esc`'s side effect of silently re-focusing Chat (events.rs:51-53) removed** (UX friction 8).
- **The 3-row roster clamp (render.rs:647-653) removed** (G8).

## 6. Slash commands

Recognised only for single-line submissions (helpers.rs:183-186) — kept deliberately. Command names are
matched case-insensitively after trimming. The composer border title shows the argument hint of the command
being typed; `/` alone lists all commands in the transcript and the hint line shows the candidate count.

| Command | Args | Effect | Autocomplete / notes |
|---|---|---|---|
| `/help` | — | print `keymap::legend()` + this table into the transcript (identical to `F1`; replaces events.rs:789-813) | `/h` prefix matches → `/help` |
| `/thought` | — | toggle thinking block (events.rs:778-786); leaves a `think:on/off` chip | chip, not a chat line |
| `/follow` | — | toggle transcript follow; chip `LIVE` / `↓ N new` (G7) | `/f` |
| `/plan`, `/workers` | — | show Plan / Workers pane (= `Ctrl+P` / `Ctrl+A`) | |
| `/mode` | `transcript\|plan\|workers\|split` | set view mode | `/mode <tab>` lists the four names |
| `/error` | — | print the sticky last error + the log path (`.marmel/marmel.log`) | pairs with `e` |
| `/retry` | — | re-submit the most recent history entry verbatim (history at mod.rs:129-133) | confirm only if the composer is non-empty |
| `/cancel` | `<worker key or task id>` | cancel **one** worker → confirm overlay → `cancel_active_worker` (workers.rs:1455) | no arg + Workers focus ⇒ uses the selected row; **`cancel_all()` not called** |
| `/cancel-all` | — | cancel every worker → confirm overlay → `orchestrator::cancel_all()` (bus.rs:38) | distinct from `/cancel` |
| `/exit` `/quit` `/q` `/abort` `/stop` | — | `Quit` confirm overlay → `request_user_exit` (mod.rs:1141-1149) | replaces the bare-word aliases (helpers.rs:195-207) |
| `/reset` (+ `/reset_plan`, `/reset-plan`, `/clear_plan`, `/clear-plan`, `/reset_execution_plan`) | — | **unified:** confirm overlay listing plan + session bundle + steering history + transcript; on `y` the renderer **sends the command to the session loop** (`tx.send`, mod.rs:165-166) and the full path runs — `Plan::clear()`, `handle_reset_command`, `ui_transcript.clear()`, steering-history clear, `[System]` context note (session.rs:1987-1997, 2086-2096, 936-959; helpers.rs:212-222, 225-245) | all six spellings identical |

**`/reset` defect resolved.** Today the five locally matched spellings short-circuit inside the renderer
(events.rs:814-834) — transcript and steering history survive and the model is never told — while
`/reset_execution_plan` reaches the loop and takes the full path (session.rs:2086-2096), and both delete the
bundle unprompted (UX friction 12, defect 3). Fix: delete the local short-circuit, match all six spellings,
confirm, then hand off to the loop exactly as `/reset_execution_plan` does today.

**Autocomplete.** When line 1 of the composer starts with `/`, the hint line (input border title) shows the
single best candidate plus `(N more)`. `Tab` accepts that completion (inserting the remainder after the
caret); with no slash prefix `Tab` cycles focus instead — one key, disambiguated by the leading `/`.
Ambiguity is resolved by longest-common-prefix, never by guessing a command the user did not type.

## 7. Visual language

### 7.1 Palette slots (`components/palette.rs`, concrete values)

| Slot | Value | Used by |
|---|---|---|
| `frame_focus` | `Style::default().fg(Color::Cyan)` | focused pane border (keeps today's Cyan, render.rs:426-430) |
| `frame_idle` | `Style::default().fg(Color::DarkGray)` | unfocused border (keeps DarkGray) |
| `frame_alert` | `Style::default().fg(Color::Red)` | abort-armed input border (render.rs:1183-1187), `✗` |
| `frame_confirm` | `Style::default().fg(Color::Yellow)` | confirm overlay border |
| `chip` | `Style::default().fg(Color::White).bg(Color::DarkGray)` | status row-2 state chips |
| `chip_active` | `Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)` | `▶` running, active plan task (render.rs:548-569 pattern) |
| `chip_done` | `Style::default().fg(Color::Green)` | `✓`, `[x]`, plan headers |
| `chip_pending` | `Style::default().fg(Color::LightRed)` | `[ ]`, pending task (render.rs:548-569) |
| `text_primary` | `Style::default()` | body text |
| `text_dim` | `Style::default().fg(Color::DarkGray)` | secondary header/status fields |
| `error` | `Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)` | sticky error line, `LLM error:` |

### 7.2 State glyphs (`enum Glyph`)

`✓` done/completed · `▶` running/active · `⧗` queued-or-validating · `⏸` paused · `✗` failed/aborted.
They replace the prose-only `(Active | Paused | Queued n/m | Idle)` labels (render.rs:717-739) but the word
label is kept beside the glyph whenever the rect has room, so the pane stays readable on terminals without
the glyphs. Glyphs must be measured with `unicode-width` (`UnicodeWidthChar::width`, already imported at
render.rs:15) and never assumed to be 1 cell; grapheme boundaries come from `unicode-segmentation`
(both crates already in `Cargo.toml:41-42`).

### 7.3 Borders & focus

`Block::bordered()` (single-line) for every pane; focused pane = `frame_focus` + `●` badge in the title
(G6 — visible without colour). Titles: ` Transcript ● `, ` Execution Plan `, ` Workers `, ` Input (…hint) `.
Overlays are drawn **last**, centred, `Clear` + a bordered `Paragraph` sized to their content clamped to
`Percentage(80)` of the frame — the pattern the Subagents block already uses by rendering itself after its
children (render.rs:1030). The abort-armed state no longer replaces the metrics row
(replaces render.rs:1117-1125, UX friction 11): the prompt lives on status row 2 and row 1 keeps counting.

### 7.4 Truncation, wrapping, chip drop order

- Transcript: `Paragraph::wrap(Wrap { trim: false })` + `scroll((chat_scroll, 0))`, as today (render.rs:471-475).
- Header/status/roster lines are single-row: built as `Vec<Span>`, then clipped by `fit_line` to `rect.width`
  measured with `unicode-width`; a clipped line ends in `…`. No `Line` may exceed its rect (test §10.19 reuses
  `formatting::rect_contains`, formatting.rs:1131).
- Wrapped-line counts keep using `formatting::count_wrapped_rendered_lines` (:847) / `wrapped_lines` (:872).
- Header drop order when narrow: `val` → `ctx` → `elapsed` → mode name. `done/total` **and** the active task
  id `> t-NNN` are **never** dropped by the width budget (G1, §10.1): `header_budget` keeps `show_task: true`
  at every width down to the 40-column too-small floor (the `show_task` arm of `header_budget`,
  `src/ui/tui/layout.rs`), and the 40x10 frame of `docs/new_ui_preview.txt` carries `▶ t-004`. The only
  thing that can still take the task id away is the width-*fit* pass in `header::drop_to_fit`, which sits
  **after** the budget and re-runs the same order (`val` → `ctx` → `elapsed` → mode name → task id) only when
  the assembled line is wider than the rect; it therefore reaches the task id last, and only for an id long
  enough not to fit next to the mandatory chips. Whatever survives both passes is clipped with `…` by
  `header::fit_spans` if it still overruns the rect.
  **resolved:** §10.1 and §7.4 agree — the task id is not a budget casualty: at the narrowest supported width
  (40 cols) an ordinary `t-NNN` id stays on the header line, and the budget ladder stops at the mode name.
  Status row 1 drop order: `queued` → `paused` → `active` names (counts kept) → `val` → `ctx`.
- `status_line` keeps only its first line, as today (mod.rs:776-782), but the sticky-error buffer (§8.5) keeps
  the full text.

## 8. States

1. **Idle / empty.** Transcript empty state: `No output yet. Type an instruction and press Enter (? for keys).`
   Plan empty state: `No active execution plan.` (keep the literal used at render.rs:292 for the read-failure
   variant, which stays `Error reading execution plan.`). Workers empty state:
   `No specialist launched yet.` — reached by `Ctrl+A`/`F5` with zero rows, fixing the silent no-op
   (events.rs:598-609 vs render.rs:368,393-394; UX friction 3, P2.1).
2. **Manager thinking.** Streaming thinking region renders only when `show_thought` (mod.rs:235), toggled by
   `Ctrl+T`/`/thought`; the `think:on` chip on status row 2 is the persistent signal (P1.3). Spinner tier and
   activity keywords keep the 125 ms cadence (§0, mod.rs:438, render.rs:1109).
3. **Parallel specialists running.** Roster shows *every* live row with a scroll indicator; header shows
   `done/total` + active task; status row 1 shows `Na Pb Qc` counts derived from `SubagentDetail`
   (`is_active`, `paused`, `queue`) exactly as the active/paused segments are derived today
   (render.rs:1054-1093). Badges stay from `helpers::subagent_row_badge` (helpers.rs:627); row
   existence/liveness/queue remain registry-derived — no second state owner (DATA §2 step 8).
4. **Validation loop.** Only `val N` (max over workers) may appear, and only after **D1** (§9.3); today
   `validation_rounds` lives only on `WorkerEntry` (workers.rs:477) and is not projected into the UI row
   (DATA §4.1, gap 1). Until D1 lands the status bar shows the word `validating` only when the status prose
   matches `ACTIVITY_STATUS_KEYWORDS`. **Never** an arbitration round counter: `round_started(round)` discards
   `round` in both hosts (`bridge/arbiter.rs:529-537`, `bridge/sink.rs:503-521`, DATA gap 9).
5. **Error / abort.** A failure produces one sticky transcript line `✗ <cause>` plus `see .marmel/marmel.log`
   (the categorical `LLM error: {category}` text is the only source today, session.rs:2345-2356,
   helpers.rs:247-261 — the spec keeps that text but stores it verbatim in `last_error` and keeps it
   recallable with `e` or `/error`). Abort arming shows the prompt on status row 2 with the metrics still
   live; the second `Esc`/`Ctrl+C` runs `request_user_exit` (mod.rs:1141-1149). Cancel-one-worker
   (`x`, `/cancel <id>`) must not call `cancel_all()` (bus.rs:38).
6. **Session restore.** After `restore_panes` (mod.rs:1400) the panes come back with their `view_mode`,
   `focus`, `follow`, scroll offsets, pane toggles, plan text and worker rows; `announce_restore`
   (mod.rs:1560) emits the report and `token_witness_warnings` (mod.rs:1792-1814) appears as exactly **one**
   transcript line (its sentences are already clamped by `clamp_warning_sentences`, mod.rs:1743). Restore must
   not re-arm abort and must not adopt stored token counters (session/format/ui.rs:126-140).

## 9. Backwards compatibility & deletion map

### 9.1 Symbols that MUST be migrated (they have out-of-module consumers)

| Symbol | Home today | External consumer | Required action |
|---|---|---|---|
| `extract_plan_line_task_id` | formatting.rs:1029 | `src/plan_parse.rs:2746` | move to `src/plan_parse.rs`, update the call |
| `visual_line_offset_of_first_pending` | formatting.rs:999 | `src/plan_parse.rs:2762` | move, update the call |
| `visual_line_offset_of_task` | formatting.rs:1041 | `src/plan_parse.rs:2767` | move, update the call |
| `line_matches_task_id` | formatting.rs:1035 | `src/plan_parse.rs:2775` | move; note `plan_parse` already owns a sibling `line_matches_task_id` (plan_parse.rs:593) used by `manager/phase.rs:951` — **one** implementation may survive |
| `clamp_warning_sentences` | `src/ui/tui/mod.rs:1743` (`pub(crate)`) | `src/ui/session.rs:1547` | move to a session-visible module (`src/ui/notes.rs`) and repoint the call site in the same commit |

The four `plan_parse` call sites are inside its `#[cfg(test)] mod tests` (plan_parse.rs:885), and the
ownership boundary is documented in prose at `plan_parse.rs:573,591` — both must be updated when the helpers
move. Everything else exported by `formatting.rs` (the ARCH §5 list: `message_style`, `LineSegment`,
`parse_line_segments`, `strip_think_tags`, `format_terminal_math`, `format_math_expr`,
`render_message_lines`, `count_wrapped_rendered_lines`, `wrapped_lines`, `clean_task_id`,
`extract_complete_sentences`, `rect_contains`, `parse_ansi_lines`, `render_markdown_lines`,
`extract_thought_and_content`) has zero hits outside `src/ui/tui/` and stays put: components reuse it instead
of reimplementing markdown/ANSI/wrapping, and `pub use formatting::*;` (tui/mod.rs:7) stays so
`crate::ui::tui::<symbol>` paths keep resolving (ARCH §5).

**Name-collision trap:** `formatting::clean_task_id(&str) -> &str` (formatting.rs:1012) is a *different*
symbol from `helpers::clean_task_id(&str) -> String` (helpers.rs:1101), and the bare `clean_task_id` calls at
session.rs:2879,3457,3849 resolve through `use super::helpers::*` (session.rs:7). A component must never
glob-import both; if it needs the formatting one, call it fully qualified.

### 9.2 Deleted outright

- Mouse arms `events.rs:226-287` (all of them) — see §5.
- `#[allow(dead_code)]` composer helpers `cursor_left` (mod.rs:399), `cursor_right` (:416),
  `delete_backward` (:432), `delete_forward` (:447), `insert_char` (:460): **delete them.** The composer is
  driven by `textarea.input(key)` (events.rs:70-71,101-103,148-152,217-219) and §0 forbids new
  `#[allow(dead_code)]`; leaving them means the deny-lists (`unused`, `dead_code`) fail the build.
- `FocusedPanel` (mod.rs:34-38), `cycle_focus` (events.rs:854-876), the hard-coded `/help` legend
  (events.rs:789-813), the 3-row `list_len` clamp (render.rs:647-653), the armed banner that erases metrics
  (render.rs:1117-1125), the local `/reset` short-circuit (events.rs:814-834), the bare-word aliases inside
  `is_abort_command` (helpers.rs:195-207).
- The rehydration converters `focused_panel_tag` / `focused_panel_from_tag` / `subagent_panel_from_detail` /
  `subagent_detail_from_panel` / `time_point_from_instant` / `duration_from_secs` (mod.rs:1620-1742) may be
  rewritten, not kept, provided `capture_ui`/`restore_panes` keep their signatures (§0).

### 9.3 Plumbing delta D1 (prerequisite for `val N`)

Add `validation_rounds: usize` (and, if used by a pane, `status_label: Option<String>`) to
`SubagentDetail` (ui/mod.rs:33-79), filled in `helpers::apply_registry_entry` (helpers.rs:550-610) from
`WorkerEntry::validation_rounds` / `status_label` (workers.rs:477, :459) — the sanctioned route
(DATA "Cheapest surfacing routes"). Until this lands, no pane may print a per-worker or max round number.

### 9.4 Persistence delta D2 + new view state (restore must not break old bundles)

`SubagentPanel` (session/format/ui.rs:145-180) has no queue/pause fields, so a restored queued/paused worker
reads as plain active/idle (DATA gap 15). D2: add `#[serde(default)] queue: Option<QueuePanel { position, of }>`
and `#[serde(default)] paused: bool`, mirroring `QueueState` (ui/mod.rs:24-29) and
`SubagentDetail::paused` (:76). Add to `ViewState` (:241-260): `#[serde(default)] view_mode: String` and
`#[serde(default)] follow: bool`, and extend `FOCUSED_PANELS` (:298) with `"input"`
(currently `["chat", "plan", "subagents"]`; `structural_violations` at :307,326 refuses anything else).
Because the structs use `#[serde(deny_unknown_fields)]`, only *added* fields with defaults are compatible:
do not rename or remove any existing field, and keep `UiSection::default()` loadable (the `Default` impl at
:268-280 must gain the new fields with the same values the renderer boots with).

**Implemented (t-017b), one correction to the field types:** `view_mode` is `#[serde(default)] Option<String>` and `follow` is `#[serde(default)] Option<bool>`, because `None` is the honest spelling of "this bundle predates the field" — an old `ui.json` then restores through the fields it does have (mode from the two pane toggles via `ViewMode::for_panes`, follow from `chat_auto_scroll`) instead of being refused or silently defaulted to an off flag; `FOCUSED_PANELS` is now `["chat", "input", "plan", "subagents"]` (`"chat"` keeps naming the transcript pane, `"input"` is the composer) with the new `VIEW_MODES = ["transcript", "plan", "workers", "split"]` checked by `structural_violations` only when a mode is stated, and the loss-free pane table is `FocusPane::tag`/`FocusPane::from_tag` while `legacy()`/`from_legacy()` survive for the pre-rebuild `events.rs` fields only.

## 10. Acceptance criteria

1. Header renders `done/total` and the active task id in every frame at 40x10 with the Plan pane closed (unit test over `PlanProgress` + `render_header` against `TestBackend`). **resolved with §7.4:** the task id is never a budget casualty — `header_budget.show_task` is `true` at 40 cols, so an ordinary `t-NNN` id sits on the header line at the 40x10 floor. Only the width-fit pass, which runs after the budget and in the same order, can take the id back, and only for an id too long to fit beside the mandatory chips.
2. Header `done/total` equals the `CheckboxState::Checked` / total split of the same `plan_content` string for 3 fixtures (empty, all-checked, mixed).
3. `keymap::legend()`, the footer text and the `/help` transcript output are byte-identical, and the legend names no key that `handle_events` does not bind (assert the legend rows against the table).
4. `F1` and `?`-on-empty-composer open the same overlay; `?` with a non-empty composer inserts `?`.
5. Typing `stop`, `cancel`, `exit`, `quit`, `q`, `abort` (composer non-empty or not) produces a submitted steering message, never `user_exit_requested()` — assert via `drain_submissions` (tests.rs:4524-4530) and `input_state`.
6. `/exit` opens `Quit`; `n` leaves the session alive, `y` exits; `Esc` from the overlay leaves it alive and **not armed**.
7. `/reset` and all five aliases route to the same confirm request, and on `y` the exact string is sent to the session loop over `tx` (no renderer-side `Plan::clear()` remains; grep for the removed short-circuit).
8. `Ctrl+D` opens `Quit` and does not set `user_exit`; a second `Ctrl+D` still does not exit without `y`.
9. `x` (empty composer, Workers focused, row selected) opens `CancelWorker{key,task_id}`; `/cancel <id>` resolves the same request for a named key/task.
10. `Ctrl+T`, `Ctrl+F`, `Ctrl+A`, `Ctrl+P` each leave a distinct persistent chip on status row 2 (assert chip text after each toggle).
11. `Ctrl+A`/`F5` with zero workers renders `No specialist launched yet.`; the chip still flips.
12. Focus badge test: at every focus state the focused pane's title buffer contains `●` and its border cell carries `frame_focus`; exactly one pane is focused.
13. Printable-key routing: with focus on Plan and Workers, `Char('x')`, `Backspace` and `Delete` mutate `textarea` content in that order; `x` with an empty composer and Workers focus instead opens the overlay (both cases asserted).
14. Follow test: with `follow = false`, appending 5 messages does not change `chat_scroll` and the chip reads `↓ 5 new`; `Ctrl+F` re-arms and the chip reads `LIVE`; **nothing that is not an explicit gesture touches follow** — a submit, a command notice and arriving output all leave it alone, and only the `Ctrl+F` / `/follow on|off` transition or a scroll/jump that lands on the tail re-arms it (`chat_auto_scroll` is a pure projection of `follow`, written only by `set_follow`/`project_follow_mirror`; the old events.rs writes and the `adopt_legacy_view_writes` re-adoption that made the silent re-arm are gone).
15. Roster test: 12 live workers produce 12 reachable rows (scroll offset changes selection). The roster and its detail pane are owned by `src/ui/tui/components/activity.rs` and the plan pane by `src/ui/tui/components/plan_pane.rs` — there is no `components/workers.rs`. No row-count `.clamp(1, 3)` remains in either module; the only `.clamp(1, 3)` left in the UI is the sanctioned `rows < 20` composer height, inside `input_height` in `src/ui/tui/layout.rs` (verified at `layout.rs:289` on 2026-10-27).
16. An overlay open closes on Esc without arming abort; y/n are honoured only inside the confirm overlay.
17. Cancel-one-worker test: x -> confirm -> exactly one worker path, with orchestrator::cancel_all() not called (assert via the recording renderer in tests/test_session_restore_panel_abort.rs).
18. Restore test: capture_ui -> restore_panes round-trips view_mode, focus (including input), follow, and D2's queue/paused; token_witness_warnings appears as one transcript line.
19. Layout test: at 100x30, 90x24, 60x16 and 40x10 no pane rect has width or height 0; at 39x9 the too-small notice appears; no rendered line exceeds its rect (rect_contains, formatting.rs:1131 may be reused).
20. Source hygiene: grep over src/ finds no `MouseEvent` arm and no executed `EnableMouseCapture` anywhere in `src/` (the token survives only in prose comments recording that it is never executed); `DisableMouseCapture` survives as the `crossterm::event` import plus **exactly two** teardown sites in `src/ui/tui/mod.rs` — one in `shutdown()`, one in `leave_alt_screen()` — and nowhere else (three code occurrences in total, the count pinned by a test). No line numbers are quoted here on purpose: they move with every edit. Verified 2026-10-27 at `mod.rs:24` (import), `mod.rs:2124` (`shutdown()`) and `mod.rs:2899` (`leave_alt_screen()`); the numbers this item used to cite (`1165,1844`) were stale.
21. Cadence invariants unchanged (30 ms / 25 ms / 125 ms / 3 s / 250 ms constants of §0), and an idle renderer performs no terminal.draw (ARCH §2).

Additional gates: (a) `cargo clippy --all-targets -- -D warnings` and `cargo build` pass with **zero**
`#[allow(dead_code)]` / `#[allow(unused)]` added; (b) the four moved plan-parsing helpers compile from their
new home and `plan_parse` tests still pass; (c) `val N` and any arbitration-round counter are absent until D1
is implemented — a test asserts the status bar renders no round number while D1 is undone.

---

This document is the binding contract for tasks t-006..t-014 on `feature/new-ui`: no implementation detail
above may be changed without amending this file first.
