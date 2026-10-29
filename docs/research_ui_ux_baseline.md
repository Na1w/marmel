# Marmel TUI — operator-facing baseline (branch `feature/new-ui`)

**Question answered:** how operable and discoverable is the current Marmel TUI for a human operator today, and what are its concrete ergonomic weaknesses?

**Method / sources (read-only, no web):** `src/ui/tui/events.rs` (key/mouse/input handling),
`src/ui/tui/render.rs` + `src/ui/tui/mod.rs` (what is drawn and how state is held — needed to
establish the on-screen information architecture), `src/ui/mod.rs` (renderer contract),
`src/ui/raw.rs` (headless mode), `src/ui/bridge/steer.rs` + `src/ui/bridge/drain.rs` and
`src/ui/session.rs` (where typed input actually goes), `src/ui/helpers.rs` (command parsing),
plus README sections *Interactive TUI* (README.md:375-382), *Keyboard Shortcuts* (README.md:384-401),
*Slash Commands* (README.md:403-412), *Headless Raw Mode* (README.md:414-433),
*Real-Time Steering & Live Dialogue* (README.md:461-470).

**Headline verdict:** the keymap is functional and mostly documented in README, but almost nothing is
discoverable from inside the app (three panel titles and one typed-only `/help` legend, which is itself
out of date); destructive bare-word aliases (`stop`, `cancel`, `exit`) are wired to full session exit;
plan/subagent feedback during long runs is squeezed through a single 1-row status line that is
overwritten and truncated; and there is no way to cancel one turn or one worker from the keyboard.

---

## 1. Current keymap — every bound key/chord

All bindings live in one match arm set in `handle_events` (`src/ui/tui/events.rs:15`).
Modifier handling is enabled by `PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)`
(`src/ui/tui/mod.rs:517-521`), i.e. `Shift+Enter` etc. only arrive on terminals that speak the kitty
protocol.

| Key / chord | Effect | Evidence |
|---|---|---|
| `Enter` (unmodified) | Submit composer as one user message / steering instruction | `events.rs:32,41` → `events.rs:764` |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+Enter` | Insert newline (multi-line prompt) | `events.rs:33-40` |
| `Ctrl+J`, and raw `Char('\n')` | Insert newline | `events.rs:210-213` |
| `Esc` (1st) | Arm abort (3 s window), silently re-focus Chat, status "Abort armed…" | `events.rs:44-59` |
| `Esc` (2nd, armed) | Full exit: `request_user_exit` | `events.rs:46-49`; `tui/mod.rs:1141-1149` |
| `Ctrl+C` (1st / 2nd) | Same arm/exit semantics as `Esc` | `events.rs:156-172` |
| `Ctrl+D` | **Immediate** exit, no arming, no confirmation | `events.rs:173-175` |
| Any printable char while armed | Disarms abort | `events.rs:214-216` |
| `Tab` | Cycle focus Chat → Plan → Subagents → Chat; only panels that are visible *and* populated | `events.rs:61` → `events.rs:854-876` |
| `PageUp` / `PageDown` | Scroll focused panel ±10 lines | `events.rs:62-63` → `events.rs:506,523` |
| `Ctrl+Up` / `Ctrl+Down` | Input history prev/next | `events.rs:65-66,78-79` → `events.rs:699,718` |
| `Up` / `Down` | In Chat with cursor off first line: move cursor; otherwise scroll ±1 | `events.rs:67-74,80-87` |
| `Left` / `Right` | Subagents panel focused: previous / next selected worker; in Chat: move cursor | `events.rs:90-122` |
| `Home` / `End` | In Chat: line start/end; in Plan or Subagents: jump to panel top/bottom | `events.rs:123-140` → `events.rs:557,574` |
| `Delete`, `Backspace` (and `Char(\x08)`/`Char(\x7f)`) | Edit composer — **only when Chat is focused** | `events.rs:141-154` |
| `Ctrl+P` | Toggle Plan panel (drops focus out of Plan if hidden) | `events.rs:176-184` |
| `Ctrl+A` | Toggle Subagents panel; on-open scrolls to bottom and steals focus | `events.rs:185-194` → `events.rs:598-609` |
| `Ctrl+T` | Toggle thinking/reasoning display; prints `[CLI] Thinking display: ON/OFF` into chat | `events.rs:195-201` |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo composer edits | `events.rs:202-209` |
| Printable char | Insert in composer — **only when Chat is focused**; other control chars ignored | `events.rs:214-220` |
| Mouse wheel up/down | Scroll the panel under the pointer by 3 (temporarily borrowing focus) | `events.rs:226-266` |
| Left click | Focus the panel under the pointer; click in input area places caret at nearest grapheme | `events.rs:268-284` → `events.rs:469` |
| Bracketed paste | Whole paste spliced into composer as one edit, never submits | `events.rs:288-291` → `events.rs:318-341`; enabled `tui/mod.rs:525-534` |
| Terminal resize | Clamp all scroll offsets, force redraw | `events.rs:293-298` |

Abort/exit semantics behind those keys: armed state expires after 3 s (`tui/mod.rs:410-415`); the armed
banner is a red full-width bar (`render.rs:1117-1125`); `request_user_exit` sets `aborted` +
`user_exit`, writes status "Aborted by user." and calls `orchestrator::cancel_all()`
(`tui/mod.rs:1141-1149`).

**Unreachable bindings:** the whole mouse block (`events.rs:226-287`) is dead in practice —
`EnableMouseCapture` is never executed anywhere in `src/` (grep returns zero hits; only
`DisableMouseCapture` appears, in cleanup at `tui/mod.rs:1165` and `tui/mod.rs:1844`).

---

## 2. Slash commands (complete list)

Slash commands exist **only in the TUI** — "raw mode reads no input at all" (README.md:405, README.md:431).
Sources: primary = code (`events.rs:764-851`, `helpers.rs:183-223`, `session.rs:1967-2096`); README.md:407-412
agrees except for the note below.

| Command (and aliases) | Arguments | Effect | Source |
|---|---|---|---|
| `/help` | none | Prints the keybinding+command legend into the chat pane; not forwarded to the model | `events.rs:789-813` |
| `/thought` | none | Toggle thinking block display; prints `[CLI] Thinking display: ON/OFF` | `events.rs:778-786` |
| `/reset`, `/reset_plan`, `/reset-plan`, `/clear_plan`, `/clear-plan` | none | **Local (renderer-only)** path: `Plan::clear()` (deletes plan, archive, forced-phase file, sweeps session bundle), hides plan panel, status `Ready`, chat line `[CLI] Execution plan reset and cleared. Phase reverted to Conversational.` | `events.rs:814-834`; `manager/phase.rs:377-408` |
| `/reset_execution_plan` | none | **Not** in the local matcher, so it travels to the session loop and takes the *full* path: commit + `handle_reset_command` + `ui_transcript.clear()` + steering-history clear + appends `[System] User executed /reset…` to the model context | `helpers.rs:212-222`; `session.rs:1987-1997,2086-2096,936-959`; `helpers.rs:225-245` |
| `/abort`, `abort`, `/exit`, `exit`, `/quit`, `quit`, `/q`, `:q`, `:q!`, `/stop`, `stop`, `/cancel`, `cancel` | none | Exit session, cancel running workers, stamp `userAbort` ending on the bundle | `helpers.rs:190-208`; `events.rs:847-850`; `session.rs:1967-1985,2081-2084`; README.md:412 |
| Empty submission | — | Sends an empty string, dropped by the session loop | `events.rs:774-777`; `session.rs:2098` |
| Anything else | free text | Idle: appended to Manager context as a user message at next round. With live workers: a **steering instruction** arbitrated on the spot | `session.rs:2098-2148`; README.md:463 |

Parsing rule worth knowing: commands are recognised **only for single-line submissions**
(`helpers.rs:183-186`), so a pasted multi-line block whose first line is `/reset` or `stop` is delivered
verbatim instead of firing the command (`helpers.rs:171-182`; README.md:405 case-insensitivity).

No command takes an argument, and there is no command for: plan progress, worker list, cancel-one-worker,
retry, search, resume, or key reference other than `/help`.

---

## 3. On-screen information architecture

Layout is fixed (`render.rs:354-396`): Chat 60 % left; right column 40 % split 50/50 Plan (top) /
Subagents (bottom); one status row; input box 3 rows. Panels appear/disappear (right column collapses
entirely when neither panel qualifies, `render.rs:370-378`).

**Visible at a glance**
- Chat pane titled ` Chat & Logs (Focus: Tab) ` (`render.rs:431-434`), auto-scrolled to the newest line
  (`render.rs:464-474`), with a right-edge scrollbar when scrollback exists (`render.rs:477-492`).
- Streaming regions inline: Manager thinking (only if `show_thought`), Manager answer, steer sentences
  (`render.rs:450-462`).
- Status row (`render.rs:1053-1177`): `Ctx: N | Tokens: in/out (total) | Status: <text>` left,
  `Elapsed: …` right (`render.rs:1127-1144`); `[N active: names]` and `[N paused: names]` segments
  (`render.rs:1066-1093`); a wait timer `(12.3s)` (`render.rs:1102-1105`); a `- \ | /` spinner while the
  status text contains an activity keyword (`render.rs:1099-1112`, keyword list `tui/mod.rs:56-76`).
- Input box title ` Input (Enter: Send, Alt/Shift+Enter: Newline, Ctrl+Z: Undo) ` (`render.rs:1188-1191`).
- Subagents panel title ` Specialist Subagents [i/n] (Ctrl-A | ◄/► Agent | ▲/▼ Scroll) `
  (`render.rs:634-639`); one row per worker: name + `(Active | Paused | Queued n/m | Idle)` + ctx tokens
  + elapsed + role badge (`render.rs:717-760`).
- Plan pane ` Execution Plan ` (`render.rs:507-510`): raw markdown with colour-coded checkboxes
  (`render.rs:554-568`), auto-scrolled to the active or first pending task (`render.rs:579-600`),
  scrollbar (`render.rs:609-623`), live from disk with a 250 ms mtime cache (`render.rs:266-322`);
  auto-opens when a plan appears (`render.rs:314-319`; README.md:382).
- Abort-armed red banner replacing the status row content (`render.rs:1117-1125`).

**Only findable by scrolling, toggling, or typing**
- The whole transcript: no search, no jump-to-message; `PageUp`/`Home` only
  (`events.rs:62,129`; `render.rs:477-492`).
- Thinking blocks: hidden by default (`tui/mod.rs:235`) and `Ctrl+T` is on no on-screen hint.
- Full specialist logs: list capped to at most **3 visible rows** (`render.rs:647-649`), detail pane
  shows `[Thinking]` (needs `show_thought`), `[Output]`, `[Logs]` below it (`render.rs:832-886`), logs
  ring-buffered at 200 lines (`tui/mod.rs:470,507-508`).
- Every command/key reference: only via typing `/help` (`events.rs:789-813`) or reading README.md:384-412.
- Steering outcome for one's own instruction: status-row sentences (`drain.rs:213-305`), arbitrator
  progress as `[Arbitrator]: …` deltas (`bridge/steer.rs:176-206`), queued-steer notice
  "Instruction queued for next turn" (`drain.rs:277-288`).
- Error detail: the operator sees `LLM error: <category>` where the category is a substring guess
  (`http`/`connectivity`/`timeout`/`stream`/`unknown`) — `session.rs:2345-2356`, `helpers.rs:247-261`;
  the underlying cause goes to `tracing`/`.marmel/marmel.log`, or `.marmel/debug.log` with `--debug`
  (README.md:367, README.md:433).
- Plan progress as a number: never rendered — `format_plan_progress_summary` is used only to build the
  arbitrator's prompt (`helpers.rs:37`, `bridge/arbiter.rs:586`).
- Headless mode shows labelled lines only, no status line, no composer, and deliberately silences token
  counters and queue transitions (`raw.rs:96-145`, `raw.rs:160-163`; README.md:431-433).

---

## 4. Friction points (each tied to evidence)

1. **Shortcuts are undocumented on screen.** Only three hints exist: `(Focus: Tab)` (`render.rs:434`),
   `(Ctrl-A | ◄/► Agent | ▲/▼ Scroll)` (`render.rs:635,638`), and the input title
   (`render.rs:1191`). `Ctrl+P`, `Ctrl+T`, `Ctrl+D`, `Ctrl+Up/Down`, `PageUp/Down`, `Esc`/`Ctrl+C` appear
   nowhere until the operator already knows to type `/help` (README.md:384-401 is the actual reference).
2. **`/help` is stale and advertises dead features.** Its legend omits `Ctrl+D`, `PageUp/PageDown`,
   `Ctrl+Enter` and describes `Home/End` as input-only, while the code binds `Ctrl+D` to immediate exit
   (`events.rs:173-175`), paging (`events.rs:62-63`) and panel jump (`events.rs:123-140`)
   (legend text: `events.rs:791-808`). It also promises "Mouse scroll / click" (`events.rs:806`) that
   cannot fire, because mouse capture is never enabled.
3. **`Ctrl+A` with no specialists is a silent no-op.** It flips `show_subagent_panel`
   (`events.rs:598-609`), but the panel only renders when `!subagents.is_empty()`
   (`render.rs:368,393-394`) — the operator gets zero feedback that the key did anything.
4. **Scroll wheel does nothing inside the app** (`events.rs:226-266` unreachable), while the alternate
   screen offers no native scrollback — reading history means `PageUp` per 10 lines.
5. **History is hard to reach and hard to use.** Plain `Up` scrolls the chat instead of recalling input
   (`events.rs:67-74`); recall is `Ctrl+Up` only (`events.rs:65-66`); there is no history search and no
   listing; history survives only inside the session bundle (`tui/mod.rs:1324-1329`, `tui/mod.rs:1468-1469`).
6. **Bare words destroy the run.** `stop`, `cancel`, `exit`, `quit`, `q`, `abort` are exit commands
   (`helpers.rs:195-207`) and mid-flight they exit immediately (`session.rs:2081-2084`). A one-word
   steering instruction such as "stop" kills the session instead of steering it — no confirmation
   (contrast: `Esc`/`Ctrl+C` do arm, `events.rs:44-59`).
7. **No turn-level or worker-level cancel.** Every abort path exits the session and `cancel_all()`s every
   worker (`events.rs:44-60,156-175`; `tui/mod.rs:1141-1149`). Cancelling a single subtask exists only as
   an arbitrator decision reached by prose (`README.md:468`), never as an operator action.
8. **`Esc` is overloaded.** The first press both arms abort *and* silently re-focuses Chat
   (`events.rs:51-53`), discarding the panel focus/worker selection the operator was working in.
9. **Plan is nearly invisible during long runs.** The plan panel is off at boot unless a plan file already
   exists (`tui/mod.rs:233,536-543`); there is no "x/y tasks done" readout anywhere (the only summary
   helper feeds the arbitrator prompt — `helpers.rs:37`, `bridge/arbiter.rs:586`); a plan longer than the
   pane shows one window at a time (`render.rs:577-606`).
10. **Everything important funnels through one status row that is overwritten and truncated.**
    `Event::Status` keeps only the first line (`tui/mod.rs:776-782`) and the row is 1 cell tall
    (`render.rs:357-359`); when the terminal is narrow the left text is silently cut
    (`render.rs:1146-1169`). Queue/pause state is visible only if the Subagents panel happens to be open
    (`render.rs:717-739`).
11. **Abort arming hides the state used to decide whether to abort.** While armed, the status row shows
    only the banner — tokens, ctx and elapsed vanish (`render.rs:1117-1125`) — and incoming status
    events stop updating `status_line` at all (`tui/mod.rs:781`).
12. **Same command, two behaviours: `/reset`.** The five locally matched spellings never reach the
    session loop (`events.rs:814-834` returns), so `ui_transcript` and steering history survive and the
    model is never told (`session.rs:946-949` and `helpers.rs:239-244` are skipped), whereas
    `/reset_execution_plan` takes the full path (`session.rs:2086-2096`). Both delete the session bundle
    (README.md:411; `phase.rs:406`) with no confirmation and no undo.
13. **Multi-line input is effectively invisible.** The composer is fixed at 3 rows total
    (`render.rs:354-360`, `render.rs:1188-1195`): a multi-paragraph or pasted draft shows about one line,
    with no line/character count or "buffer continues" marker — the operator cannot see what they are
    about to send.
14. **Focus state is only a border colour.** Typing, `Delete` and `Backspace` do nothing unless Chat is
    focused (`events.rs:141-154,214-220`); nothing on screen says "you are not typing into the composer".
15. **Reading scrollback loses context silently.** Any new activity re-arms auto-scroll on submit/toggle
    paths (`events.rs:784,846`; `render.rs:464-469`) and there is no "N new lines below" indicator after
    the operator scrolls up (`events.rs:509`).
16. **Errors are categorical, transient, and not recallable.** `LLM error: {category} (Ready)`
    (`session.rs:2356`) plus one chat line (`tui/mod.rs:803-816`); the real message is in
    `.marmel/marmel.log` (README.md:433) — no retry key, no "show last error". A plan read failure shows
    the literal `Error reading execution plan.` inside the plan pane (`render.rs:292`).
17. **Headless mode is unsteerable and unabortable by contract.** `poll_input`/`read_input` are the trait
    defaults (no stdin at all) (`raw.rs:160-163`); README.md:431 states slash commands, shortcuts and
    mid-flight steering are unreachable — a scripted run in trouble can only be SIGKILLed.

---

## 5. Prioritised improvement proposals (intended operator-visible behaviour only)

### P1 — core operability
- **P1.1** The footer always shows the live, currently-valid key legend so the operator never has to
  know a command exists to find it.
- **P1.2** Any printed key reference (`/help`, titles) is generated from the actual bindings, so the
  operator is never told about a chord that does nothing (and dead mouse claims disappear).
- **P1.3** Every toggle shows persistent state (Plan/Subagents shown or hidden, thinking ON/OFF,
  follow ON/OFF) in the status area instead of a transient chat line.
- **P1.4** Bare words (`stop`, `cancel`, `exit`, `quit`, `q`, `abort`) are treated as steering
  instructions; exit requires a slash form plus one confirmation step.
- **P1.5** One key stops the current turn without ending the session, and one key cancels the currently
  selected specialist while the rest keep running.
- **P1.6** A plan progress readout ("3/7 done · active t-042 · 12m elapsed") is always visible even when
  the plan panel is closed, so long runs stay legible at a glance.
- **P1.7** Failures land as a distinct, sticky chat entry naming the cause and where to read more, and
  the last error stays recallable (plus a retry affordance) instead of a one-word category on a 1-row bar.

### P2 — feedback and input ergonomics
- **P2.1** `Ctrl+A` with no workers opens the panel and states that no specialist has been launched yet.
- **P2.2** Scroll wheel and click work as the legend claims, or the UI stops advertising them.
- **P2.3** Plain `Up`/`Down` in an empty composer recalls input history, and a searchable history view
  is available with recall that survives restarts even without a session bundle.
- **P2.4** The composer grows to reveal multi-line drafts (with a visible line/byte count), so the
  operator can read what they are about to send.
- **P2.5** Immediately after submitting mid-flight, the operator sees where the instruction went —
  arbitrating, answered directly, queued for next turn, forwarded to worker *X* (with notice id), or
  rejected — rather than waiting for a rotating status sentence.
- **P2.6** The subagent roster shows every live worker (or a scroll indicator) instead of silently
  cutting off past three rows.
- **P2.7** When the operator scrolls up, a "↓ N new lines / live" indicator shows how far behind the
  newest output they are, and new output never yanks the viewport without an explicit key.
- **P2.8** While abort is armed, the counters (tokens, ctx, elapsed, active workers) remain readable and
  the arming prompt is shown in its own region.
- **P2.9** Focus state is labelled on screen and typing while a non-input panel is focused either routes
  to the composer or says why it did not.

### P3 — reach and polish
- **P3.1** The operator can search the transcript and jump between matches.
- **P3.2** A "what changed while I was away" line reports tasks checked off, deliverables landed and
  workers finished since the last interaction.
- **P3.3** One key jumps straight to a named specialist's detail pane without cycling with `Tab`.
- **P3.4** A command lists the effective keymap/configuration as currently in force.
- **P3.5** Raw mode optionally accepts stdin lines for steering and abort, so scripted runs can be
  redirected instead of only killed.
- **P3.6** `/reset` behaves identically for every alias, warns what it will delete (plan, bundle,
  steering history) and offers a recovery path or undo.

---

## 6. Notable consistency defects found while auditing (fix candidates, not proposals)
1. `/help` legend ≠ implemented bindings (`events.rs:791-808` vs `events.rs:33-40,62-63,123-140,173-175`).
2. Mouse handling implemented but mouse capture never enabled (`events.rs:226-287`; no
   `EnableMouseCapture` in `src/`, cleanup-only `DisableMouseCapture` at `tui/mod.rs:1165,1844`).
3. README.md:411 lists `/reset_execution_plan` as a reset alias, but the TUI's local matcher omits it
   (`events.rs:815-820`), so that alias produces a visible `User: /reset_execution_plan` chat line and a
   different result than `/reset` (see friction 12).
4. `Ctrl+Enter` inserts a newline in code (`events.rs:33-35`) and README.md:389 documents it, but the
   in-app `/help` legend (`events.rs:798`) lists only `Shift+Enter`/`Alt+Enter`/`Ctrl+J` — so the
   operator reading help on screen learns one chord fewer than the app actually honours.
