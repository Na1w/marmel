# The new Marmel terminal UI

Audience: anyone who runs `marmel` and reads its screen.
Everything below was read out of the implementation on branch `feature/new-ui`
(`src/ui/tui/events.rs`, `src/ui/tui/components/help.rs`,
`src/ui/tui/components/input.rs`, `src/ui/tui/state.rs`, `src/ui/tui/layout.rs`),
not from the design spec. Where the two disagree, §9 says so.

---

## 1. What changed, and why you will notice it

The rebuilt UI keeps every pane you already use — transcript, execution plan,
workers, composer — and makes the state of the session readable without hunting
for it. Progress is now a permanent part of the screen: `✓ 3/7 done` and the task
id in flight sit in the header row at every size from 100×30 down to the 40×10
floor, and each pane says what it is holding (`3/7 done ▶ t-004` on the plan
frame, `▶ 1 ⏸ 1 ⧗ 1 ✓ 0` on the workers frame). Every pane carries a visible
focus badge, every key press leaves a visible echo (a `plan:on` chip, a
`● LIVE` / `↓ 3 new` chip on the transcript's own border), the footer legend and
the `?` overlay tell you the keymap instead of assuming you memorised it, and
nothing destructive happens on a single keystroke: `/reset`, `/exit`,
`/cancel-all`, `x` and `Ctrl+D` all stop at a confirm overlay that names the
action and what it costs.

## 2. Starting it

```bash
git checkout feature/new-ui
cargo run
```

A **real terminal is required**: the TUI is only chosen when stdout is a TTY. If
stdout is a pipe or a file, or you pass `--raw`, or `ui_mode = "raw"` is set in
the config, Marmel runs in headless raw mode instead, which has no panes, no
keyboard and no slash commands (`src/main.rs:65`).

> The frames in this guide are deterministic `TestBackend` captures
> (`docs/new_ui_preview.txt`), not photographs. The sandbox this branch was built
> in could not open a PTY, so nobody has looked at the live screen; the frames are
> cell-by-cell dumps of what the renderer draws, and the layout of colours is
> therefore unverified (§9).

## 3. Screen anatomy

The 100×30 frame below is a session with a 7-task plan (3 checked), three live
workers, a non-empty draft, and focus on the transcript.

```text
 Marmel M4 Split ✓ 3/7 done ▶ t-004 ctx 45% elapsed 10m 12s
┌ Transcript ● ────────────────────────────────────────────┐┌ Execution Plan ─────3/7 done ▶ t-004 ┐
│User: rebuild the TUI so plan, workers and progress stay  ││# Plan                               ▲│
│legible                                                   ││                                     █│
│Marmennill: I split the work into seven tasks and started ││- [x] [t-001] research UX baseline   █│
│with the audit.                                           ││- [x] [t-002] audit TUI architecture █│
│[Status] delegating validator_specialist (t-003)          ││- [x] [t-003] draft the design spec  █│
│Tool: read_file(src/ui/tui/mod.rs)                        ││- [ ] [t-004] scaffold state, layout,█│
│Marmennill: every pane is now drawn by its own component  ││theme                                █│
│renderer.                                                 ││- [ ] [t-005] pane chrome and chips  █│
│Streaming: the status bar chips ar                        ││- [ ] [t-006] wire the roster rank   ║│
│                                                          ││into the status row                  ▼│
│                                                          │└ ✓ 3 done ⧗ 4 open ▶ in flight ───────┘
│                                                          │┌ Workers ─────────────────────────────┐
│                                                          ││ > ▶ running coder_specialist t-004 3…│
│                                                          ││   ⧗ queued 2/3 researcher_specialist…│
│                                                          ││   ⏸ paused validator_specialist t-00…│
│                                                          ││                                      │
│                                                          ││                                      │
│                                                          ││──────────────────────────────────────│
│                                                          ││▶ running coder_specialist t-004 3m 1…│
│                                                          ││[Output]                              │
│                                                          ││wrote layout.rs, 292 lines            │
└  ● LIVE ─────────────────────────────────────────────────┘└ ▶ 1 ⏸ 1 ⧗ 1 ✓ 0  1/3  detail 0/0 ────┘
 Tokens 12.4k in / 3.2k out | 3a | ctx 45% | 1p | 1q | coder_specialist-t-004… | elapsed 10m 12s
 plan:on  workers:on  think:off  3a 1p 1q
┌ Input (Enter send, Alt/Shift+Enter newline, Ctrl+Z undo) ────────────────────────────────────────┐
│add the focus badge to every pane title                                                           │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
 F1 help  Tab focus  F2 mode  F3/F4/F5 panes  x cancel-worker  e last error  Esc arm  Ctrl+A/P panes
```

| Region | What it holds |
|---|---|
| **Header** (1 row) | `Marmel`, the mode name (`M4 Split`), `✓ 3/7 done`, `▶ t-004` (the task in flight), `val N` (the highest validation round across the worker rows, printed only when one has been recorded), `ctx 45%`, `elapsed 10m 12s`. `Marmel` and `✓ done/total` are never dropped at any width; `▶ t-004` is printed at every size that draws a header at all (§7). |
| **Transcript pane** | The conversation: your input (`User: …`), Manager output (`Marmennill: …`), `[Status]` and `Tool:` lines, thinking blocks when `think:on`. Markdown-rendered. Its **bottom border is its own status**: `● LIVE` while follow is on, `↓ 3 new` when you have scrolled back, `↓ follow off` when follow is off and nothing is unseen, plus `scroll 8/8` whenever there is somewhere to scroll. Empty state: `No output yet. Type an instruction and press Enter (? for keys).` |
| **Execution Plan pane** | The plan body with task checkboxes and state glyphs (`✓` done, `▶` in flight, `⧗` open). Title right side repeats `3/7 done ▶ t-004`; the bottom border counts `✓ 3 done ⧗ 4 open ▶ in flight`. A scrollbar appears once the body overflows, and the pane auto-scrolls to the task in flight. |
| **Workers pane** | Top half: the roster, one row per live worker — ` > ` selection marker, state glyph and label (`▶ running`, `⧗ queued 2/3`, `⏸ paused`), the worker key, its task id. Bottom half (only at ≥ 60 columns): the selected worker's detail — `[Output]`, thinking, queued steering, logs. Bottom border: the tally `▶ 1 ⏸ 1 ⧗ 1 ✓ 0  1/3  detail 0/0`. Empty state: `No specialist launched yet.` |
| **Status bar** (2 rows at ≥ 20 rows) | Row 1: `Tokens 12.4k in / 3.2k out \| 3a \| ctx 45% \| 1p \| 1q \| <worker names> \| elapsed 10m 12s`. Row 2: the state chips `plan:on  workers:on  think:off  3a 1p 1q`, where `3a 1p 1q` are the active / paused / queued worker counts (the same counts row 1 spells as `3a`, `1p`, `1q`). A chip reads `narrow` when a toggle is on but the narrow-terminal ladder hid that pane (§7), so no chip ever advertises a pane you cannot see. |
| **Composer** | The draft. Title: `Input (Enter send, Alt/Shift+Enter newline, Ctrl+Z undo)`, plus chips when relevant: `command /reset` (the draft is in command mode), `history 3/12` (you are browsing history), `lines 2 · chars 140` (a long draft), and a **ghost completion** row while you type a slash command. |
| **Footer legend** (1 row at ≥ 20 rows) | ` F1 help  Tab focus  F2 mode  F3/F4/F5 panes  x cancel-worker  e last error  Esc arm  Ctrl+A/P panes`. Below 70 columns it shortens to ` F1 help  Esc arm  Enter send`, and at 100 columns a long line is clipped with `…`. |

The panes open themselves as the session gets live: booting with a non-empty
`.marmel/execution_plan.md` opens the plan pane, a worker starting a task opens
the plan pane, and any active worker opens the workers pane
(`src/ui/tui/mod.rs:1451`, `:1820`, `:2204`). With nothing live you start in
`M1 Transcript`.

## 4. View modes, focus, and follow

### 4.1 View modes

| Mode | What is on screen | Go straight there | Notes |
|---|---|---|---|
| `M1 Transcript` | Transcript only | `F3` | Boot mode when no pane is open. |
| `M2 Plan` | Plan left (55 %) + Transcript right (45 %) | `F4` | |
| `M3 Workers` | Workers left (55 %) + Transcript right (45 %) | `F5` | |
| `M4 Split` | Transcript left (60 %) + Plan over Workers in the side column (40 %) | `F2` cycles `M4 → M1 → M2 → M3 → M4` | The narrow-terminal ladder restacks it (§7). |

`Ctrl+P` shows or hides the Plan pane and `Ctrl+A` shows or hides the Workers
pane. Each toggle moves the mode to whatever the resulting pair of panes means,
so it is never a silent no-op — in `M1`, `Ctrl+A` opens the workers pane (mode
`M3`) even when there are no workers yet, and the pane draws its empty state. The
`plan:` / `workers:` chips on status row 2 are the persistent echo.

### 4.2 Focus

`Tab` walks the ring **Input → Transcript → Plan → Workers → Input**, skipping
panes the current mode does not draw. `Shift+Tab` walks it backwards. A `Tab`
typed while the draft holds a slash command is spent on the ghost completion
instead (§6), and if there is nothing to complete it falls back to focus.

The focus marker is a `●` badge in the focused pane's **title** (e.g.
`┌ Transcript ● ─…`), reinforced by the focus border colour. Exactly one pane
carries it. Focus can never sit on a pane that is not on screen: changing mode
pulls a stranded focus back onto a visible pane.

### 4.3 Follow / auto-scroll

Follow is on at boot: the transcript shows the newest line. Scrolling it back
(`PageUp`, or `Up` when that key scrolls rather than walking history) turns
follow **off** and the transcript border starts counting `↓ 3 new` — and `↓ follow
off` once nothing is unseen. Returning to the tail turns follow back on
(`● LIVE`). `Ctrl+F` toggles follow explicitly. The rule is symmetric, so
the chip never lags the viewport: the scroll path and the chip are the same
decision (`scroll_by` → `set_follow`, `src/ui/tui/events.rs:487-495`).

## 5. Keyboard

These are all the bindings the rebuilt TUI has; the same table is what `?`/`F1`
draws and what `/help` prints (`legend()` in
`src/ui/tui/components/help.rs:87-246`, 26 rows).

### 5.1 Composer

| Key | Action |
|---|---|
| `Enter` | Submit the draft. |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+Enter` / `Ctrl+J` | Insert a newline (multi-line drafts). |
| Printable key / paste | Insert in the composer, **in any mode and any focus**. A paste always goes to the composer. |
| `Backspace` / `Delete` | Edit the composer, in any focus. |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo the draft. |
| `Ctrl+Up` / `Ctrl+Down` | Previous / next input history entry (the composer shows `history 3/12`). |
| `Up` / `Down` | Move the caret inside a multi-line draft; with an empty draft, walk history; otherwise scroll the focused pane by 1. |
| `Ctrl+P` / `Ctrl+A` | Show / hide the Plan / Workers pane. |
| `Ctrl+T` | Toggle thinking blocks (echo: the `think:on` chip). |

### 5.2 Navigation, scrolling, panes

| Key | Action |
|---|---|
| `Tab` | Accept a ghost completion if the draft offers one, else cycle focus forward. |
| `Shift+Tab` | Cycle focus backward. |
| `F2` | Cycle the view mode (`M4 → M1 → M2 → M3 → M4`). |
| `F3` / `F4` / `F5` | Go to Transcript / Plan / Workers. |
| `Left` / `Right` | With the Workers pane focused: previous / next worker in the roster. Otherwise: caret. |
| `PageUp` / `PageDown` | Scroll the focused pane by 10 lines — the transcript when the composer holds focus (turns follow off/on accordingly). |
| `Home` / `End` | With the composer focused: line start / end. Otherwise: pane top / bottom (jumping to the bottom re-arms follow). |
| `Ctrl+F` | Toggle transcript follow. |
| Resize | Clamp scroll offsets to the new size and redraw immediately. |

### 5.3 Help

| Key | Action |
|---|---|
| `F1` | Open the keymap + command overlay (works in every state). |
| `?` | Same, but only while the composer is **empty** — with a draft, `?` types. |
| `Esc` | Close the overlay. Closing an overlay never arms abort. |

The overlay is content-sized, centred, clamped to 80 % of the screen, and its
bottom border reads `Esc close` … `/help prints the full table`. `/help` prints
the identical table into the transcript.

### 5.4 Destructive actions and the confirm overlay

| Key | Action |
|---|---|
| `Ctrl+D` | Open the **Quit confirm**. It never exits un-armed; a second `Ctrl+D` just refreshes the same overlay. |
| `x` | Cancel the **selected** worker in the Workers pane (opens a confirm). Only works with the Workers pane focused, at least one worker, and an empty composer; otherwise `x` types the letter. |
| `y` | Run the pending action — honoured **only** inside the confirm overlay. |
| `n` | Dismiss the pending action. |
| `Esc` | Dismiss the pending action (same as `n` here). |
| `Esc` / `Ctrl+C` (no overlay open) | Arm abort: the status line reads `Abort armed. Press ESC again to abort.` and the same surface shows the armed prompt. |
| `Esc` / `Ctrl+C` again within 3 s | Exit the session (cancel workers, stamp the abort ending). |
| Any printable key while armed | Disarm the abort (`Abort cancelled.`) and then type the character. |
| `Ctrl+C` | Identical to `Esc` in every state. |

The confirm overlay always names the pending action and its consequence, so you
can read what `y` will do before you press it. The body it draws (border title
`Confirm`, bottom border ` Esc close ` … `/help prints the full table`) is, for
`/cancel-all`:

```text
Cancel every worker (/cancel-all)
Cancels every running worker and every in-flight stream.
y / n — confirm / dismiss the pending action
```

While an abort is armed the same surface holds the prompt instead, and it honours
no `y`/`n`:

```text
Abort armed — Esc / Ctrl+C again to abort
any other key cancels the arm; Esc once closes this surface
```

The status line says the same thing (`Confirm cancel of worker 'coder_specialist-t-004' — y to cancel it, n or Esc to leave it running.`).
There are exactly four confirmable actions: quit, cancel one worker, cancel all
workers, and a destructive slash command. There is no "clear transcript" surface
(§9).

### 5.5 Errors and misc

| Key | Action |
|---|---|
| `e` | Print the sticky last error again (with `see .marmel/marmel.log`). Only while the composer is empty; with a draft, `e` types. |

### 5.6 Precedence, in one paragraph

`y` / `n` belong to the confirm overlay and are ordinary letters outside it (typing
`no` still types). Then global view keys (`F1`–`F5`, `Esc`, `Tab`), then `Ctrl`
chords, then `Enter`, then navigation and editing, then printables — and among
printables a single-letter mnemonic (`?`, `x`, `e`) wins **only with an empty
composer**. Any printable first disarms a pending abort, and so do the `Ctrl+P`,
`Ctrl+A`, `Ctrl+T`, `Ctrl+F`, `Ctrl+Z`, `Ctrl+Y` and `Ctrl+J` chords. A multi-line
draft is never interpreted as a command, so a pasted block whose first line is
`/reset` arrives verbatim.

### 5.7 Bindings that no longer exist

Printed under the help table, so old muscle memory learns the replacement:

- **Mouse scroll / click** — removed; see §8.
- **`Ctrl+D` as immediate exit** — it now opens the Quit confirm.
- **Bare `stop` / `cancel` / `exit` / `quit` / `q` / `abort`** — now ordinary
  steering text; the slash forms (`/exit`, `/cancel …`) open a confirm. Submitting
  a bare word echoes: `That is steering text, not a command … Use /exit (or Ctrl+D) for a confirmed quit, or Esc twice to arm abort.`
- **`Esc` re-focusing Chat** — `Esc` closes an overlay or arms abort; it never
  changes focus.
- **The 3-row worker roster clamp** — every live worker is reachable.

## 6. Slash commands

Type them in the composer (line 1 only, case-insensitive, recognised after
trimming). The table below is the real command table the composer's autocomplete
and `/help` both read (`slash_commands()` in
`src/ui/tui/components/input.rs:114-162`): **22 spellings** — 20 `/`-prefixed
ones plus the two vim spellings of the quit family. One resolver reads that same
table for every submission (`command_intent` in
`src/ui/tui/events.rs:242-299`, dispatched at `:1133` and `:895`), so every row
is either answered by the TUI itself or handed to the confirm overlay and then
the session loop. Nothing in the table is forwarded to the model as a prompt,
and there is no chain of string compares behind it: the table **is** the
vocabulary.

| Command | Effect | What actually happens |
|---|---|---|
| `/help` | Print the key legend and this command table. | Printed into the transcript by the TUI — the same body the `F1` overlay draws (`help_text()` built from `legend()` plus this table), so the three surfaces cannot disagree. |
| `/thought` | Toggle the thinking block. | Done in the TUI, the `Ctrl+T` transition; the `think:` chip echoes it. |
| `/follow [on\|off]` | Toggle transcript follow. | Done in the TUI, the same transition as `Ctrl+F`: the transcript border shows `● LIVE` again or starts counting `↓ N new`. Bare form toggles; `on` / `off` state the target. |
| `/plan [on\|off]` | Show or hide the Execution Plan pane. | Done in the TUI, the `Ctrl+P` transition; the `plan:` chip on status row 2 is the persistent echo. |
| `/workers [on\|off]` | Show or hide the Workers pane. | Done in the TUI, the `Ctrl+A` transition; the `workers:` chip on status row 2 is the persistent echo. |
| `/mode transcript\|plan\|workers\|split` | Set the view mode. | Done in the TUI, the `F3`/`F4`/`F5` transitions: `transcript` → `M1`, `plan` → `M2`, `workers` → `M3`, `split` → `M4`, plus a notice naming the mode and its tag. A bare `/mode` is a question, not a no-op: it prints the mode on screen and the arguments it accepts. |
| `/error` | Show the last error again. | Done in the TUI — exactly what the `e` key does (`recall_last_error`, `src/ui/tui/events.rs:1236`): the sticky §8.5 error buffer first (category plus the whole text), a transcript scan as fallback, then `see .marmel/marmel.log`. |
| `/cancel <worker key or task id>` | Cancel **one** worker. | Confirm overlay; `y` cancels that row. The argument matches a roster key or task id case-insensitively; an unknown name is still confirmed and then reports `Worker '…' is not running any more.` rather than cancelling a different row. |
| `/cancel` (no argument) | Cancel one worker, or quit. | With the **Workers pane focused** and at least one worker, it names the selected row (confirm). Anywhere else the spelling keeps its old meaning — a quit (confirm). |
| `/cancel-all` | Cancel every worker. | Confirm overlay, then bulk cancel. |
| `/reset` | Clear plan, session bundle, steering history and transcript (confirm). | Confirm overlay; `y` hands the command to the session loop, which clears the plan + session bundle + journals and tells the model. |
| `/reset_plan`, `/reset-plan`, `/clear_plan`, `/clear-plan`, `/reset_execution_plan` | Same as `/reset` (all five are real spellings). | Same confirm, same single path. |
| `/exit`, `/quit`, `/q`, `/abort`, `/stop` | Quit the session. | Confirm overlay; `y` runs the loop's abort branch (cancel workers, stamp a `userAbort` ending). |
| `:q`, `:q!` | Quit the session (the two vim spellings). | Rows of the table, so `/help` lists them, and they route through the **same confirm overlay** as `/quit` (`is_abort_command`, `src/ui/helpers.rs:197-211`). They are **not** autocompleted and never chip the composer as a command, because command mode is `/`-only (`command_token`, `src/ui/tui/components/input.rs:170`). |

`/retry` is **not** a command and is not in the table: no retry action exists in
the backend, so advertising one would advertise a dead key. Typing it is refused
like any other unknown spelling (see below).

**Command or prose?** A `/`-prefixed token made of ASCII letters, digits, `_` and
`-` is a command (`/plan`, `/reset_plan`); everything else is ordinary text and
goes to the model — `/tmp/x is the path`, `// note`, a lone `/`, a bare `stop`,
and any multi-line draft whatever its first line spells (`looks_like_command`,
`src/ui/tui/events.rs:187`).

**Unknown command.** A command-shaped token that is not a row of the table —
`/xyz`, a misspelling such as `/paln`, or `/retry` — is refused in the composer
and never sent to the model. The transcript and the status line get:
`/xyz is not a command — /help prints the command table, F1 the key legend.`
The composer's ghost row says `/xyz is not a command` while you type it.

**Bad argument.** A row given an argument it does not accept is refused the same
way, and the accepted argument is read off the table itself rather than
restated (`advertised_arguments`, `src/ui/tui/events.rs:1101`):

```text
/plan takes on|off — "/plan maybe" was not run.
/mode takes transcript|plan|workers|split — "/mode frobnicate" was not run.
/quit takes no argument — "/quit now" was not run.
```

`/cancel <worker key or task id>` is the one row of that family with an argument
of its own.

**Autocomplete.** Start a `/` and the composer's frame shows the completion: a
unique prefix names it (`/tho` → `/thought`), an ambiguous prefix completes only
to the common prefix and counts the rest (`/rese` → `/reset  (3 more)  Tab`),
`/` alone reports how many commands match, and a token that matches nothing says
`/zz is not a command`. `Tab` accepts the completion into the draft.

## 7. Small terminals: what degrades to what

Geometry comes from `compute_layout` and its thresholds in
`src/ui/tui/layout.rs:20-35`. The ladder is applied in order, first match wins.

| Terminal | What you get |
|---|---|
| **≥ 100 × 30** | Full layout: header 1 row, body (absorbs all slack, floor 6 rows), status 2 rows, composer `(draft lines + 2)` clamped to 3…9, footer legend 1 row. `M4 Split` is two columns, 60 / 40. |
| **< 90 columns** | `M4 Split` stacks: transcript on top, **one** side pane below (the focused one, or Plan when both are on). The hidden side's chip reads `narrow` instead of `on`. `M2`/`M3` keep their two columns. |
| **< 70 columns** | Header drops `val N`, then `ctx %`, then `elapsed`, then the mode name; `Marmel`, `✓ done/total` and `▶ t-004` stay. The footer legend shortens to ` F1 help  Esc arm  Enter send`. |
| **< 60 columns** | The worker **detail** half is not drawn at all (roster only). `M2`/`M3` collapse to a single column holding that pane. |
| **< 20 rows** | Status bar collapses to **1 row** — the chip row (`plan:on …`) and the footer legend disappear. The composer clamps to 1…3 rows with a `+n` overflow marker. The body floor stays 6 rows. |
| **40 × 10** | The floor. Still: header (with `✓ 3/7 done ▶ t-004`), transcript with `● LIVE  scroll 8/8`, one status row with tokens and the active count, and the composer. Fields are shed one at a time, never in a block. |
| **< 40 columns or < 10 rows** | Nothing but the notice: `Terminal too small — Marmel needs at least 40x10 (currently 39x9). Resize to continue.` No pane, chip or metric is drawn, so nothing lies about a pane that does not exist. |

60 × 16 — header shed to brand + progress + task id, one status row, no footer
legend, side panes taken away by the ladder:

```text
 Marmel ✓ 3/7 done ▶ t-004
┌ Transcript ● ────────────────────────────────────────────┐
│User: rebuild the TUI so plan, workers and progress stay  │
│legible                                                   │
│Marmennill: I split the work into seven tasks and started │
│with the audit.                                           │
│[Status] delegating validator_specialist (t-003)          │
│Tool: read_file(src/ui/tui/mod.rs)                        │
│Marmennill: every pane is now drawn by its own component  │
│renderer.                                                 │
│Streaming: the status bar chips ar                        │
└  ● LIVE ─────────────────────────────────────────────────┘
 Tokens 12.4k in / 3.2k out | 3a | ctx 45% | 1p | 1q
┌ Input (Enter send, Alt/Shift+Enter newline, Ctrl+Z undo) ┐
│add the focus badge to every pane title                   │
└──────────────────────────────────────────────────────────┘
```

40 × 10 — the floor:

```text
 Marmel ✓ 3/7 done ▶ t-004
┌ Transcript ● ────────────────────────┐
│Marmennill: every pane is now drawn  ▲│
│by its own component renderer.       █│
│Streaming: the status bar chips ar   ▼│
└  ● LIVE  scroll 8/8 ─────────────────┘
 Tokens 12.4k in / 3.2k out | 3a
┌ Input (Enter send, Alt/Shift+Enter ne┐
│d the focus badge to every pane title │
└──────────────────────────────────────┘
```

39 × 9 — one cell below the floor:

```text
┌ Marmel ─────────────────────────────┐
│Terminal too small — Marmel needs at │
│least 40x10 (currently 39x9). Resize │
│            to continue.             │
│                                     │
│                                     │
│                                     │
│                                     │
└─────────────────────────────────────┘
```

## 8. The mouse is no longer used

Mouse capture is never enabled, and the UI is **keyboard only**: mouse reports
are ignored rather than guessed at (the only remaining `DisableMouseCapture`
calls are defensive teardown cleanup, `src/ui/tui/mod.rs:2123` and `:2898`).
Scrolling is `PageUp` / `PageDown` / `Up` / `Down` / `Home` / `End`, selection is
`Left` / `Right` in the Workers pane, and focus is `Tab` / `Shift+Tab`. This also
means terminal-native text selection and copy-on-select work again.

## 9. Known drift (spec vs. code) and what was not verified

The contract for intent is `docs/ui_design_spec.md` §2 (layout), §4 (modes +
focus), §5 (keymap), §6 (commands), §7 (palette), §8 (states). Where the spec and
this branch's code disagree, **this guide documents the code**. Recorded drift:

1. **Spec §10 item 20 cites stale line numbers** for the mouse teardown. The real
   `DisableMouseCapture` sites are `src/ui/tui/mod.rs:2123` and `src/ui/tui/mod.rs:2898`
   (the import is at `src/ui/tui/mod.rs:23`); there is no `EnableMouseCapture` anywhere.
2. **Spec §10.15 names a file that does not exist** — `components/workers.rs`.
   The worker roster/detail component is `src/ui/tui/components/activity.rs`
   (the plan pane is `src/ui/tui/components/plan_pane.rs`).
3. **There is no `ClearTranscript` confirm surface.** `events::ConfirmAction` has
   exactly four variants — `Quit`, `CancelWorker`, `CancelAll`, `Command`
   (`src/ui/tui/events.rs:58-82`). Transcript clearing happens only as part of
   `/reset`, which is the `Command` variant.
4. **Spec §10.1 and §7.4 contradict each other about the task id at 40 columns.**
   The code always shows it: `header_budget` returns `show_task: true`
   unconditionally (`src/ui/tui/layout.rs:86`), and the 40 × 10 preview frame
   proves it (` Marmel ✓ 3/7 done ▶ t-004`).
5. **`:q` and `:q!` are rows of the command table** (`slash_commands()`,
   `src/ui/tui/components/input.rs:114-162`) and route through the same confirm
   overlay as `/quit` (`is_abort_command`, `src/ui/helpers.rs:197-211`), but they
   are the only rows the autocomplete never offers: composer command mode is
   `/`-only (`command_token`, `src/ui/tui/components/input.rs:170`), so they are
   neither completed nor shown as a `command …` chip.
6. **Boot mode differs from the spec's worked example.** Spec §2.2 presents
   `M4 Split` as the default at 100 × 30; the renderer constructs `M1 Transcript`
   (`view_mode: ViewMode::for_panes(false, false)`, `src/ui/tui/mod.rs:432`) and
   only the restored-plan / live-worker paths open side panes.

Not verified in the sandbox:

- **No live PTY run.** Every frame quoted here is a `TestBackend` capture
  (`docs/new_ui_preview.txt`); colours, true-terminal key reporting (kitty
  protocol `Shift+Tab` vs `BackTab`), and resize behaviour under a real terminal
  emulator have not been eyeballed. The handler accepts both `Shift+Tab`
  spellings, so the binding is code-verified but not machine-verified.
- **`/help` and the help overlay on a narrow terminal.** The overlay clamps to
  80 % of the screen and marks the clipped tail with
  `… see /help for the full table`, but the only rendered capture of it in
  `docs/new_ui_preview.txt` is the 80 × 24 frame.
