//! Keyboard and composer editing for [`TuiRenderer`] — the spec §5 keymap.
//!
//! One entry point per event, one function per binding: [`TuiRenderer::handle_events`]
//! drains crossterm, [`TuiRenderer::apply_terminal_event`] routes exactly one event
//! (and is the seam the §10.3 legend-parity test feeds synthetic events through), and
//! [`TuiRenderer::on_key_event`] holds the keymap in precedence order.
//!
//! Three rules shape the file:
//!
//! * **The composer is never starved (§4.4).** Printables, paste, `Backspace`,
//!   `Delete`, `Ctrl+Z/Y/J` and the caret keys reach `textarea.input(key)` in every
//!   mode and every focus state. The pre-rebuild gating — editing keys and printables
//!   were dead unless Chat held focus — is gone (UX friction 14, G6).
//! * **View state moves only through the transitions** of `mod.rs` (`set_view_mode`,
//!   `set_focus`, `cycle_focus_pane`, `set_follow`, `open_help`, `show_confirm`,
//!   `close_overlay`). No key arm writes `view_mode` / `focus` / `follow` / `overlay`
//!   by hand, so the stored state and the legacy pane mirrors cannot drift apart.
//! * **No silent key press (G5).** Every toggle ends in a visible chip: `plan:`,
//!   `workers:` and `think:` on status row 2, `● LIVE` / `↓ N new` in the transcript
//!   pane that owns the viewport. An empty pane toggles **on** and renders its empty
//!   state (§8.1), so `Ctrl+A` with no workers is never a no-op.
//! * **The command table is the vocabulary (spec §6).** `command_intent` resolves
//!   the first line of a submission against `components::input::slash_commands()`
//!   — the same table the composer autocompletes and `/help` prints — and every
//!   row of it has an owner here: the commands the renderer answers itself
//!   (`run_local_command`, each one the same state transition as its key), and the
//!   destructive family, which stops at the confirm surface
//!   (`arm_destructive_command`) and is then the session loop's to
//!   run. A token the table does not carry is answered where it is typed instead
//!   of being forwarded to the model, so advertised and handled are one set.
//!
//! There is deliberately **no mouse handling**: mouse capture is never switched on
//! anywhere in `src/`, so the pre-rebuild mouse arms were unreachable in practice,
//! and §5 / §9.2 delete them. The capture-off teardown call stays in the two
//! teardown paths only (§0), as defensive cleanup for a terminal that inherited
//! mouse mode elsewhere.

use super::components::{help, input};
use super::state::{self, FocusPane, ViewMode};
use super::{FocusedPanel, TuiRenderer, is_abort};
use crate::ui::{Renderer, SubagentDetail};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

/// The destructive action the confirm surface is holding for `y` / `n` (spec §3,
/// G3). The surface carries a tag, not a closure: `y` either runs the quit path the
/// armed-abort flow already runs, cancels **one** named worker, cancels every
/// worker, or hands one already-confirmed slash command back to the session loop —
/// nothing else is reachable from a keystroke.
///
/// The worker identity is captured at request time, so a roster that changes under
/// the overlay (a worker finishing, a new one registered) can never retarget `y`
/// onto a different row, and `orchestrator::cancel_all()` is never reachable from a
/// **single-worker** request (spec §5, `bus.rs:38`). It is reachable from
/// [`ConfirmAction::CancelAll`] alone, which is what the `/cancel-all` row of §6
/// documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfirmAction {
    /// End the session through [`Renderer::request_user_exit`] — the same exit path
    /// the armed-abort flow uses today.
    Quit,
    /// Cancel one worker by its effective registry key and task id.
    CancelWorker {
        /// The selected roster row's key (`SubagentDetail::name`).
        key: String,
        /// Its task id, when the row carries one.
        task_id: Option<String>,
    },
    /// `/cancel-all` (§6): cancel every running worker.
    CancelAll,
    /// A destructive **slash command** the operator has already confirmed. `y`
    /// sends this exact spelling to the session loop over `tx`, so the loop's own
    /// command path runs — the quit spellings through its abort branch, and every
    /// `/reset` spelling through `handle_reset_command_and_commit` (plan clear,
    /// session bundle, steering history, transcript, `[System]` context note).
    /// That is the unified `/reset` route of §6: one path for all six spellings,
    /// and no renderer-side short-circuit that leaves the transcript behind.
    Command {
        /// The command verbatim, as the operator typed it.
        line: String,
    },
}

impl ConfirmAction {
    /// Project the request onto the confirm surface's body (spec §3, §7.3): the
    /// action's **name** and its **consequence**, both one line. The renderer draws
    /// this, so the text can never describe an action other than the one `y` runs.
    pub(crate) fn describe(&self) -> help::ConfirmRequest {
        match self {
            ConfirmAction::Quit => help::ConfirmRequest::new(
                "Quit the session (Ctrl+D)",
                "Ends the session and cancels every running worker.",
            ),
            ConfirmAction::CancelWorker { key, .. } => help::ConfirmRequest::new(
                format!("Cancel worker '{key}'"),
                "Stops that one worker; every other worker keeps running.",
            ),
            ConfirmAction::CancelAll => help::ConfirmRequest::new(
                "Cancel every worker (/cancel-all)",
                "Cancels every running worker and every in-flight stream.",
            ),
            ConfirmAction::Command { line } => {
                let token = line.trim();
                if crate::ui::helpers::is_reset_command(token) {
                    help::ConfirmRequest::new(
                        format!("Reset the execution plan ({token})"),
                        "Clears plan, session bundle, steering history and \
                         transcript, and tells the model.",
                    )
                } else if crate::ui::helpers::is_abort_command(token) {
                    help::ConfirmRequest::new(
                        format!("Quit the session ({token})"),
                        "Ends the session and cancels every running worker.",
                    )
                } else {
                    help::ConfirmRequest::new(
                        format!("Run {token}"),
                        "Sends the command to the session loop.",
                    )
                }
            }
        }
    }
}

/// A §6 command the TUI answers by itself, with its argument already resolved.
///
/// Each arm is the **same state transition** as the key the operator can use
/// instead — `/plan` is `Ctrl+P`, `/workers` is `Ctrl+A`, `/follow` is `Ctrl+F`,
/// `/mode <name>` is `F3`/`F4`/`F5`, `/error` is `e` — and none of them reaches
/// the session loop, so a command is never a prompt to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalCommand {
    /// `/help`: print the one legend the `F1` overlay and the footer share.
    Help,
    /// `/thought`: the `Ctrl+T` thinking chip.
    Thinking,
    /// `/error`: the `e` sticky-error recall (spec §8.5).
    RecallError,
    /// `/mode [transcript|plan|workers|split]`: `F3`–`F5`. `None` is the bare
    /// form, which prints the current mode and the arguments the row advertises.
    Mode(Option<ViewMode>),
    /// `/plan [on|off]`: the `Ctrl+P` plan-pane toggle; `None` toggles.
    Plan(Option<bool>),
    /// `/workers [on|off]`: the `Ctrl+A` workers-pane toggle; `None` toggles.
    Workers(Option<bool>),
    /// `/follow [on|off]`: the `Ctrl+F` follow toggle (G7); `None` toggles.
    Follow(Option<bool>),
}

/// Who owns the first line of a submission, resolved against the command table
/// the composer autocompletes and `/help` prints ([`input::slash_commands`]).
///
/// One table decides the owner, which is what keeps **advertised and handled the
/// same set**: every row is either answered here or handed to the confirm
/// surface, and a token that is not a row is refused with the composer's own
/// wording instead of being forwarded to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandIntent {
    /// Ordinary text, never a command: a multi-line submission (spec §5), a draft
    /// outside command mode, or prose that merely starts with a slash.
    Text,
    /// A command the renderer answers itself.
    Local(LocalCommand),
    /// A command the confirm surface and the session loop own: the six `/reset`
    /// spellings, the quit family (`:q` and `:q!` included) and the `/cancel`
    /// spellings.
    Loop,
    /// A row of the table, given an argument it does not accept.
    BadArgument {
        /// The command token, lower-cased the way the table lookup is.
        name: String,
        /// The argument as the operator typed it.
        argument: String,
    },
    /// Command mode, but the token names no row of the table.
    Unknown {
        /// The token as the operator typed it.
        token: String,
    },
}

/// Whether `token` reads as a command word rather than as prose that happens to
/// start with a slash: `/plan` and `/reset_plan` do, `/`, `//` and `/tmp/x` do
/// not. That is what keeps `/tmp/x is the path` an instruction and a bare `/` an
/// unfinished draft, instead of a refused command.
fn looks_like_command(token: &str) -> bool {
    let Some(body) = token.strip_prefix('/') else {
        return false;
    };
    !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The table rows whose owner is the confirm surface and the session loop. The
/// vocabulary itself stays in [`crate::ui::helpers`] — the same predicates the
/// loop's own gate reads — so the two can never disagree about what is
/// destructive. `/cancel` is already one of the abort spellings there.
fn is_confirm_command(line: &str) -> bool {
    let line = line.trim();
    crate::ui::helpers::is_reset_command(line)
        || crate::ui::helpers::is_abort_command(line)
        || line.eq_ignore_ascii_case("/cancel-all")
}

/// The argument of a row that takes none: an extra word is refused rather than
/// quietly ignored, so `/help now` can never read as `/help`.
fn no_argument(argument: &str, command: LocalCommand) -> Result<LocalCommand, ()> {
    if argument.is_empty() {
        Ok(command)
    } else {
        Err(())
    }
}

/// The argument of `/plan`, `/workers` and `/follow`: bare toggles, `on` and
/// `off` state the target — the same three moves the chip and the chord make.
fn on_off_argument(argument: &str) -> Result<Option<bool>, ()> {
    match argument {
        "" => Ok(None),
        "on" => Ok(Some(true)),
        "off" => Ok(Some(false)),
        _ => Err(()),
    }
}

/// The argument of `/mode`: one of the four §4.1 modes spelled by its own
/// [`ViewMode::tag`], or bare to ask which mode is on screen.
fn mode_argument(argument: &str) -> Result<Option<ViewMode>, ()> {
    if argument.is_empty() {
        return Ok(None);
    }
    ViewMode::from_tag(&argument.to_ascii_lowercase())
        .map(Some)
        .ok_or(())
}

/// Resolve the first line of `line` against the command table: which arm of
/// [`CommandIntent`] answers it, with the argument already parsed.
fn command_intent(line: &str) -> CommandIntent {
    // A block is delivered verbatim, whatever its first line spells.
    if !crate::ui::helpers::submission_is_single_line(line) {
        return CommandIntent::Text;
    }
    let trimmed = line.trim();
    let token = input::command_token(trimmed);
    // The confirm-gated family is tested before command mode, because it owns
    // whole-line spellings that never put the composer into command mode: the vim
    // spellings `:q` and `:q!`.
    if is_confirm_command(trimmed) || token.is_some_and(is_confirm_command) {
        let Some(token) = token else {
            return CommandIntent::Loop;
        };
        // That gate reads the **whole line**, exactly as the session loop does, so
        // an argument hanging off a row that advertises none is refused here
        // instead of being forwarded as a prompt. `/cancel <worker key or task id>`
        // is the one spelling in the family with an argument.
        let name = token.to_ascii_lowercase();
        let argument = trimmed.strip_prefix(token).unwrap_or("").trim();
        if argument.is_empty() || name == "/cancel" {
            return CommandIntent::Loop;
        }
        return CommandIntent::BadArgument {
            name,
            argument: argument.to_string(),
        };
    }
    let Some(token) = token else {
        return CommandIntent::Text;
    };
    if !looks_like_command(token) {
        return CommandIntent::Text;
    }
    let name = token.to_ascii_lowercase();
    let argument = trimmed.strip_prefix(token).unwrap_or("").trim();
    let parsed = match name.as_str() {
        "/help" => no_argument(argument, LocalCommand::Help),
        "/thought" => no_argument(argument, LocalCommand::Thinking),
        "/error" => no_argument(argument, LocalCommand::RecallError),
        "/mode" => mode_argument(argument).map(LocalCommand::Mode),
        "/plan" => on_off_argument(argument).map(LocalCommand::Plan),
        "/workers" => on_off_argument(argument).map(LocalCommand::Workers),
        "/follow" => on_off_argument(argument).map(LocalCommand::Follow),
        _ => {
            return CommandIntent::Unknown {
                token: token.to_string(),
            };
        }
    };
    match parsed {
        Ok(command) => CommandIntent::Local(command),
        Err(()) => CommandIntent::BadArgument {
            name,
            argument: argument.to_string(),
        },
    }
}

impl TuiRenderer {
    /// Drain pending crossterm events (a 30 ms poll budget when `blocking`, §0) and
    /// route each one through [`Self::apply_terminal_event`]. Returns whether any
    /// event meant something, which is what lets the entry path repaint exactly once
    /// per meaningful event instead of on every key release.
    pub(crate) fn handle_events(&mut self, blocking: bool) -> bool {
        let mut handled = false;
        loop {
            let available = if blocking {
                event::poll(Duration::from_millis(30)).unwrap_or(false)
            } else {
                event::poll(Duration::ZERO).unwrap_or(false)
            };
            if !available {
                break;
            }
            match event::read() {
                Ok(event) => handled |= self.apply_terminal_event(&event),
                Err(_) => break,
            }
            if blocking {
                break;
            }
        }
        handled
    }

    /// Apply exactly one terminal event.
    ///
    /// This is the whole event surface, and therefore the seam a test can drive:
    /// acceptance criterion 3 asks that the legend name no key the handler does not
    /// bind, and the only honest way to answer that is to feed a synthetic event for
    /// every legend row through the same function the live loop calls.
    pub(crate) fn apply_terminal_event(&mut self, event: &TermEvent) -> bool {
        match event {
            // Release events are not bindings: on terminals without the kitty
            // protocol they never arrive, and acting on them would double every
            // press.
            TermEvent::Key(key)
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
            {
                self.on_key_event(*key)
            }
            // §4.4: a paste always goes to the composer, whatever pane is focused.
            TermEvent::Paste(text) => {
                self.paste_text(text);
                true
            }
            TermEvent::Resize(_, _) => {
                // Clamp every scroll offset to its recomputed maximum, then mark the
                // frame dirty: the entry path force-flushes for any handled event, so
                // a resized viewport is redrawn immediately and never shows blank
                // space below a stale offset.
                self.clamp_scrolls();
                self.dirty = true;
                true
            }
            // Anything else — including a mouse report, which the terminal is never
            // asked to send — is ignored rather than guessed at.
            _ => false,
        }
    }

    /// The §5 keymap, in precedence order. Returns whether the key did something.
    ///
    /// The order is the contract, because two rules otherwise collide:
    ///
    /// 1. `y` / `n` belong to the **confirm overlay** and are read only while a
    ///    request is pending there (§5). Outside it they are ordinary characters, so
    ///    typing `no` still types.
    /// 2. Global view keys, then Ctrl chords.
    /// 3. Single-letter mnemonics (`?`, `x`, `e`) only with an **empty composer**;
    ///    with a draft they insert literally — protecting typing outranks mnemonics
    ///    (§4.4).
    /// 4. Navigation and editing keys, then the printable insert, which happens in
    ///    every mode and every focus state.
    ///
    /// An abort arm is disarmed by any printable before that character is inserted
    /// (§5), so the 3 s window can never swallow what the operator is typing.
    pub(crate) fn on_key_event(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // 1. `y` / `n` — overlay scope, and only for a request the surface actually
        //    holds. The armed-abort prompt occupies the same surface (§8.5) without
        //    holding a request, which is why an armed `y` is typing, not consent:
        //    §5 disarms on printables rather than treating them as answers.
        if !ctrl
            && !alt
            && self.confirm_action.is_some()
            && matches!(self.overlay, state::OverlayState::Confirm)
            && let KeyCode::Char(c) = key.code
        {
            if matches!(c, 'y' | 'Y') {
                return self.confirm_pending();
            }
            if matches!(c, 'n' | 'N') {
                return self.dismiss_pending();
            }
        }

        // 2. Global view keys (scope: global).
        match key.code {
            KeyCode::F(1) => {
                self.open_help();
                return true;
            }
            KeyCode::F(2) => {
                self.cycle_view_mode();
                return true;
            }
            KeyCode::F(3) => {
                self.set_view_mode(ViewMode::Transcript);
                return true;
            }
            KeyCode::F(4) => {
                self.set_view_mode(ViewMode::Plan);
                return true;
            }
            KeyCode::F(5) => {
                self.set_view_mode(ViewMode::Workers);
                return true;
            }
            KeyCode::Esc => {
                self.escape_pressed("ESC");
                return true;
            }
            // `Shift+Tab` arrives as `BackTab` on kitty-protocol terminals and as
            // `Tab`+SHIFT elsewhere (§4.2); both walk the ring backwards.
            KeyCode::BackTab => {
                self.cycle_focus_pane(false);
                return true;
            }
            KeyCode::Tab if shift => {
                self.cycle_focus_pane(false);
                return true;
            }
            KeyCode::Tab => {
                // §6: with a `/` draft `Tab` accepts the ghost completion; with
                // nothing to accept it cycles focus (§4.2). One key, disambiguated
                // by the leading slash.
                if self.accept_ghost_completion() {
                    return true;
                }
                self.cycle_focus_pane(true);
                return true;
            }
            _ => {}
        }

        // 3. Ctrl chords. Every one of them is named in §5; an unbound Ctrl chord is
        //    deliberately not a key press at all, because the composer must not
        //    receive a character the operator never typed.
        if ctrl
            && !alt
            && let KeyCode::Char(c) = key.code
        {
            match c.to_ascii_lowercase() {
                // §5: `Ctrl+C` means the same as `Esc`, so it must not be
                //     pre-disarmed — the second press is the exit.
                'c' => {
                    self.escape_pressed("Ctrl+C");
                    return true;
                }
                'd' => {
                    self.request_quit();
                    return true;
                }
                'p' => {
                    self.disarm_abort();
                    self.toggle_pane(true);
                    return true;
                }
                'a' => {
                    self.disarm_abort();
                    self.toggle_pane(false);
                    return true;
                }
                't' => {
                    self.disarm_abort();
                    self.toggle_thinking();
                    return true;
                }
                'f' => {
                    self.disarm_abort();
                    self.toggle_follow();
                    return true;
                }
                'z' => {
                    self.disarm_abort();
                    self.ensure_textarea_in_sync();
                    self.textarea.undo();
                    self.sync_input_from_textarea();
                    self.dirty = true;
                    return true;
                }
                'y' => {
                    self.disarm_abort();
                    self.ensure_textarea_in_sync();
                    self.textarea.redo();
                    self.sync_input_from_textarea();
                    self.dirty = true;
                    return true;
                }
                'j' => {
                    self.disarm_abort();
                    self.newline();
                    return true;
                }
                _ => return false,
            }
        }

        // 4. Enter submits; a modified Enter (and the raw `Char('\n')` some
        //    terminals produce) is a newline (§5).
        if let KeyCode::Enter = key.code {
            if ctrl || alt || shift {
                self.newline();
            } else {
                self.submit();
            }
            return true;
        }
        if let KeyCode::Char('\n') = key.code {
            self.newline();
            return true;
        }

        // 5. Navigation and composer editing.
        match key.code {
            KeyCode::Up | KeyCode::Down => return self.scroll_or_history(key, ctrl),
            KeyCode::PageUp => {
                self.scroll_by(true, 10);
                return true;
            }
            KeyCode::PageDown => {
                self.scroll_by(false, 10);
                return true;
            }
            KeyCode::Left => {
                if self.focus == FocusPane::Workers && self.select_worker(false) {
                    return true;
                }
                self.edit_key(key);
                return true;
            }
            KeyCode::Right => {
                if self.focus == FocusPane::Workers && self.select_worker(true) {
                    return true;
                }
                self.edit_key(key);
                return true;
            }
            KeyCode::Home => {
                if self.focus == FocusPane::Input {
                    self.edit_key(key);
                } else {
                    // The gesture away from the tail states the follow decision
                    // itself (`scroll_to_top` → `set_follow(false)`).
                    self.scroll_to_top();
                    self.dirty = true;
                }
                return true;
            }
            KeyCode::End => {
                if self.focus == FocusPane::Input {
                    self.edit_key(key);
                } else {
                    // And the jump to the tail is the explicit way back on.
                    self.scroll_to_bottom();
                    self.dirty = true;
                }
                return true;
            }
            // §5: `Backspace`, `Delete` and the two control spellings some terminals
            // produce edit the composer in every focus state.
            KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Char('\x08')
            | KeyCode::Char('\x7f') => {
                self.edit_key(key);
                return true;
            }
            _ => {}
        }

        // 6. Printables. With an empty composer a mnemonic wins; otherwise — and for
        //    every other printable — the character is typed, in any mode and any
        //    focus (§4.4).
        if let KeyCode::Char(c) = key.code {
            if !ctrl && !alt && self.composer_is_empty() {
                match c {
                    '?' => {
                        self.open_help();
                        return true;
                    }
                    'x' if self.request_cancel_selected_worker() => return true,
                    'e' => {
                        self.recall_last_error();
                        return true;
                    }
                    _ => {}
                }
            }
            // §5: a printable press cancels a pending abort arm before it inserts.
            if self.is_abort_armed() {
                self.status_line = "Abort cancelled.".to_string();
            }
            self.disarm_abort();
            self.edit_key(key);
            return true;
        }

        false
    }

    // ---- view-mode and pane toggles (spec §4.1, §5, G5) ----------------------

    /// `F2`: walk M4 → M1 → M2 → M3 → M4 (§4.1). The mode carries its pane
    /// inventory, so the key is never a silent no-op: the toggles move with it and a
    /// focus the new mode would orphan is pulled back onto a pane that is on screen.
    ///
    /// The walk direction reads forward against the t-024 geometry, which is why the
    /// order itself does not move with the pane swap: the pane that holds the
    /// **leading** slot (the left column, or the whole body in M1) walks Plan (M4) →
    /// Transcript (M1) → Transcript (M2) → Workers (M3), and Plan → Transcript →
    /// Workers is exactly the left-to-right order of the panes M4 itself draws. Under
    /// the pre-t-024 geometry the same walk stepped Transcript → Plan → Workers, the
    /// then-left-to-right order of M4 — the trade moved the panes, not the direction.
    fn cycle_view_mode(&mut self) {
        let next = match self.view_mode {
            ViewMode::Split => ViewMode::Transcript,
            ViewMode::Transcript => ViewMode::Plan,
            ViewMode::Plan => ViewMode::Workers,
            ViewMode::Workers => ViewMode::Split,
        };
        self.set_view_mode(next);
    }

    /// The mode whose pane inventory is exactly this pair (§4.3). `Ctrl+P`,
    /// `Ctrl+A`, `/plan` and `/workers` all land here, which is what makes a pane
    /// command and its chord the same transition — and a pane toggle never a
    /// silent no-op (G5).
    fn set_panes(&mut self, plan: bool, workers: bool) {
        self.set_view_mode(ViewMode::for_panes(plan, workers));
    }

    /// `Ctrl+P` / `Ctrl+A` (§5): show or hide the Plan / Workers pane. The pane
    /// toggles are the mode's inventory spelled out, so the route is
    /// [`Self::set_panes`]: in M1 the key changes the mode instead of doing
    /// nothing (UX friction 3, G5), and an empty pane turns **on** and draws its
    /// empty state (§8.1). The `plan:` / `workers:` chip on status row 2 is the
    /// persistent echo.
    fn toggle_pane(&mut self, plan: bool) {
        let plan_visible = if plan {
            !self.show_plan_panel
        } else {
            self.show_plan_panel
        };
        let workers_visible = if plan {
            self.show_subagent_panel
        } else {
            !self.show_subagent_panel
        };
        self.set_panes(plan_visible, workers_visible);
    }

    /// `Ctrl+T`: toggle the thinking block. The chip is the echo (§5: "state chip,
    /// not a chat line") — the pre-rebuild handler pushed `[CLI] Thinking display:`
    /// into the transcript, which trained the operator to read the chat pane for a
    /// setting rather than the status row.
    fn toggle_thinking(&mut self) {
        self.show_thought = !self.show_thought;
        self.dirty = true;
    }

    /// `Ctrl+F`: toggle transcript follow (G7). The transcript pane owns the
    /// `● LIVE` / `↓ N new` chip, so this leaves one chip in one place; the stored
    /// flag and its legacy mirror move together through [`Self::set_follow`].
    fn toggle_follow(&mut self) {
        self.set_follow(!self.follow);
    }

    /// Scroll the focused pane (G7). The transcript arm of the scroll path states
    /// the follow decision itself — off the tail turns follow **off**, onto the tail
    /// turns it back on — so the chip can never lag the viewport, and no caller has
    /// to restate it from the legacy mirror.
    fn scroll_by(&mut self, up: bool, amount: u16) {
        if up {
            self.scroll_up(amount);
        } else {
            self.scroll_down(amount);
        }
        self.dirty = true;
    }

    /// `Up` / `Down` (§5): `Ctrl+Up`/`Ctrl+Down` are history; inside a multi-line
    /// draft the caret keeps moving; with an empty draft the key walks the history;
    /// otherwise it scrolls the focused pane by one, which is what the pre-rebuild
    /// handler did and what keeps `Down` useful with a one-line draft.
    fn scroll_or_history(&mut self, key: KeyEvent, ctrl: bool) -> bool {
        if ctrl {
            if key.code == KeyCode::Up {
                self.history_prev();
            } else {
                self.history_next();
            }
            self.dirty = true;
            return true;
        }
        self.ensure_textarea_in_sync();
        let last_line = self.textarea.lines().len().saturating_sub(1);
        let line = self.textarea.cursor().0;
        let up = key.code == KeyCode::Up;
        let caret_moves = (up && line > 0) || (!up && line < last_line);
        if caret_moves {
            self.edit_key(key);
            return true;
        }
        if self.composer_is_empty() {
            if up {
                self.history_prev();
            } else {
                self.history_next();
            }
            self.dirty = true;
            return true;
        }
        self.scroll_by(up, 1);
        true
    }

    /// `Left` / `Right` in the Workers pane: move the roster selection, keeping the
    /// selected row's detail in view exactly as the pre-rebuild Subagents handler did.
    fn select_worker(&mut self, forward: bool) -> bool {
        if self.subagents.is_empty() {
            return false;
        }
        let last = self.subagents.len() - 1;
        if forward {
            if self.selected_subagent_idx >= last {
                return false;
            }
            self.selected_subagent_idx += 1;
        } else {
            if self.selected_subagent_idx == 0 {
                return false;
            }
            self.selected_subagent_idx -= 1;
        }
        self.subagent_autoscroll = true;
        let w = self.subagent_width.get();
        let n = self.estimated_subagent_lines(w);
        let h = self.subagent_height.get();
        self.subagent_scroll = n.saturating_sub(h) as u16;
        self.dirty = true;
        true
    }

    // ---- overlays, abort arming and the confirm surface (spec §5, §8.5, G3) ---

    /// `Esc` / `Ctrl+C`: close an open overlay, else arm abort, else — while the 3 s
    /// window is live — take the exit path.
    ///
    /// The pre-rebuild side effect of silently re-focusing Chat is gone (UX friction
    /// 8), and closing an overlay never arms anything (acceptance criterion 16): the
    /// key is spent on the surface, which is what makes `Esc` out of `F1` and `Esc`
    /// out of a Quit confirm safe.
    fn escape_pressed(&mut self, chord: &str) {
        if self.is_abort_armed() {
            self.confirm_abort = false;
            self.abort_armed_at = None;
            self.confirm_action = None;
            Renderer::request_user_exit(self);
            return;
        }
        if self.overlay.is_open() {
            self.confirm_action = None;
            self.close_overlay();
            return;
        }
        self.confirm_abort = true;
        self.abort_armed_at = Some(Instant::now());
        self.status_line = format!("Abort armed. Press {chord} again to abort.");
        // §8.5: the armed prompt occupies the confirm surface but holds no `y`/`n`
        // request — `confirm_action` stays empty, so a printable is typing, not
        // consent, and disarms.
        self.confirm_action = None;
        self.show_confirm();
        self.dirty = true;
    }

    /// `Ctrl+D` (§5): open the Quit confirm surface. It never exits un-armed, and a
    /// second `Ctrl+D` only refreshes the same surface (acceptance criterion 8). It
    /// also does not arm abort, so `Esc` closes it and leaves the session running —
    /// the rule acceptance criterion 6 states for the slash-spelling of the same
    /// request.
    fn request_quit(&mut self) {
        self.show_request(ConfirmAction::Quit);
    }

    /// `x` (§5): ask to cancel the **selected** worker of the Workers pane.
    ///
    /// The precondition is part of the binding, not an implementation detail: the key
    /// is scoped to the pane, and when it is not met the character falls through and
    /// is typed, which is what keeps `x` usable as a letter (acceptance criterion
    /// 13). The cancel itself waits for `y`.
    fn request_cancel_selected_worker(&mut self) -> bool {
        if self.focus != FocusPane::Workers || self.subagents.is_empty() {
            return false;
        }
        let index = self
            .selected_subagent_idx
            .min(self.subagents.len().saturating_sub(1));
        let row = &self.subagents[index];
        let key = row.name.clone();
        let task_id = row.task_id.clone();
        self.show_request(ConfirmAction::CancelWorker { key, task_id })
    }

    /// Put one destructive request on the confirm surface and state it in the
    /// status line. This is the single place the surface is armed, so every entry
    /// point — `Ctrl+D`, `x`, a slash command — ends on the same surface with the
    /// same `y` / `n` affordance, and none of them arms abort on the way.
    fn show_request(&mut self, action: ConfirmAction) -> bool {
        let status = Self::confirm_status_line(&action);
        self.disarm_abort();
        self.confirm_action = Some(action);
        self.show_confirm();
        self.status_line = status;
        self.dirty = true;
        true
    }

    /// The status-line echo of a pending request. Every form says the same three
    /// things the overlay says (spec §7.3): which action, what it touches, and that
    /// `y` runs it while `n` or `Esc` leaves the session alone.
    fn confirm_status_line(action: &ConfirmAction) -> String {
        match action {
            ConfirmAction::Quit => {
                "Confirm quit — y to quit, n or Esc to keep working.".to_string()
            }
            ConfirmAction::CancelWorker { key, .. } => format!(
                "Confirm cancel of worker '{key}' — y to cancel it, n or Esc to leave it running."
            ),
            ConfirmAction::CancelAll => {
                "Confirm cancel of every worker — y to cancel them, n or Esc to leave them running."
                    .to_string()
            }
            ConfirmAction::Command { line } => format!(
                "Confirm {} — y to run it, n or Esc to keep working.",
                line.trim()
            ),
        }
    }

    /// `/cancel <worker key or task id>` (§6): resolve the argument against the
    /// roster and ask to cancel **that one** worker.
    ///
    /// An argument that names no row is still confirmed rather than guessed at: the
    /// surface prints the string the operator typed and `y` hands the same string to
    /// the registry's exact-identity matching, so an unknown name reports "not
    /// running any more" instead of cancelling a different row.
    fn request_cancel_worker_by_argument(&mut self, argument: &str) -> bool {
        let argument = argument.trim();
        let row = self.subagents.iter().find(|row| {
            row.name.eq_ignore_ascii_case(argument)
                || row
                    .task_id
                    .as_deref()
                    .is_some_and(|task| task.eq_ignore_ascii_case(argument))
        });
        let key = row
            .map(|row| row.name.clone())
            .unwrap_or_else(|| argument.to_string());
        let task_id = row.and_then(|row| row.task_id.clone());
        self.show_request(ConfirmAction::CancelWorker { key, task_id })
    }

    /// §6 / §10 item 17: is `line` a destructive slash command? If so the request is
    /// put on the confirm surface and **nothing else happens** — nothing is sent to
    /// the session loop, no plan is cleared, no worker is cancelled and no exit is
    /// requested. `y` runs it, `n` or `Esc` drops it.
    ///
    /// This is the single gate for the whole destructive family, which is what makes
    /// the routing auditable:
    ///
    /// * `/exit` `/quit` `/q` `/abort` `/stop` (`:q`, `:q!`) and a bare `/cancel`
    ///   outside the Workers pane → a `Command` request: `y` sends that exact
    ///   spelling back to the session loop, whose own abort branch ends the run.
    /// * `/cancel <worker key or task id>` → one [`ConfirmAction::CancelWorker`];
    ///   `/cancel` with no argument while the Workers pane is focused names the
    ///   selected row (spec §6).
    /// * `/cancel-all` → [`ConfirmAction::CancelAll`].
    /// * **all six** `/reset` spellings → a `Command` request, so the reset takes the
    ///   one path §6 asks for: confirm, then the session loop runs
    ///   `handle_reset_command_and_commit`. The renderer-local short-circuit that
    ///   used to answer five of the spellings is gone.
    ///
    /// Multi-line submissions are never commands (`submission_is_single_line`), so a
    /// pasted block whose first line is `/reset` still arrives verbatim.
    fn arm_destructive_command(&mut self, line: &str) -> bool {
        if !crate::ui::helpers::submission_is_single_line(line) {
            return false;
        }
        let trimmed = line.trim();
        let token = trimmed.to_ascii_lowercase();
        if crate::ui::helpers::is_reset_command(&token) {
            return self.show_request(ConfirmAction::Command {
                line: trimmed.to_string(),
            });
        }
        if token == "/cancel-all" {
            return self.show_request(ConfirmAction::CancelAll);
        }
        if token == "/cancel" {
            // §6: `/cancel` names one worker. With no argument the selected Workers
            // row is its subject; anywhere else the spelling keeps the meaning it
            // already had in the loop — a quit — which is why it is confirmed too.
            if self.focus == FocusPane::Workers && !self.subagents.is_empty() {
                return self.request_cancel_selected_worker();
            }
            return self.show_request(ConfirmAction::Command {
                line: trimmed.to_string(),
            });
        }
        if let Some(argument) = token.strip_prefix("/cancel ")
            && !argument.trim().is_empty()
        {
            return self.request_cancel_worker_by_argument(argument);
        }
        if is_abort(&token) {
            return self.show_request(ConfirmAction::Command {
                line: trimmed.to_string(),
            });
        }
        false
    }

    /// `y` inside the confirm surface: run the pending request.
    ///
    /// `Quit` takes the same exit path the armed-abort flow uses today, and
    /// `CancelWorker` addresses one worker by name through the registry's
    /// exact-identity matching. `Command` hands the confirmed slash command back to
    /// the session loop — the unified `/reset` route of §6, and the quit
    /// spellings' own abort branch. Bulk cancellation is reachable from
    /// [`ConfirmAction::CancelAll`] (`/cancel-all`, §6) and from nowhere else; the
    /// call itself lives in [`Self::cancel_every_worker`] so a single-worker
    /// request can never reach it (spec §5, §10 item 17).
    fn confirm_pending(&mut self) -> bool {
        let action = self.confirm_action.take();
        self.confirm_abort = false;
        self.abort_armed_at = None;
        self.close_overlay();
        match action {
            Some(ConfirmAction::CancelWorker { key, task_id }) => {
                let cancelled = crate::orchestrator::cancel_active_worker(
                    Some(key.as_str()),
                    task_id.as_deref(),
                );
                self.status_line = if cancelled {
                    format!("Cancelled worker '{key}'.")
                } else {
                    format!("Worker '{key}' is not running any more.")
                };
            }
            Some(ConfirmAction::CancelAll) => {
                self.cancel_every_worker();
            }
            Some(ConfirmAction::Command { line }) => {
                // The command is already confirmed: the loop's own command path is
                // the executor, so the renderer never re-implements it. It is
                // echoed and remembered like any other submission, so the composer
                // history still holds the command the operator ran.
                let trimmed = line.trim().to_string();
                if self.history.last() != Some(&trimmed) {
                    self.history.push(trimmed.clone());
                }
                self.history_index = None;
                self.input_draft.clear();
                self.messages.push(format!("User: {trimmed}"));
                // A confirmed command is still a submission: like every other one,
                // it never touches the follow state (item 14).
                let _ = self.tx.send(line);
            }
            Some(ConfirmAction::Quit) | None => {
                Renderer::request_user_exit(self);
            }
        }
        self.dirty = true;
        true
    }

    /// `/cancel-all` (§6): cancel every running worker. Deliberately its own
    /// function, so the broadcast cancellation appears nowhere in the body that
    /// handles a single-worker request.
    fn cancel_every_worker(&mut self) {
        crate::orchestrator::cancel_all();
        self.status_line = "Cancelled every running worker.".to_string();
    }

    /// `n` inside the confirm surface: drop the request and keep working
    /// (acceptance criteria 6 and 16). Nothing here arms abort either.
    fn dismiss_pending(&mut self) -> bool {
        let action = self.confirm_action.take();
        self.confirm_abort = false;
        self.abort_armed_at = None;
        self.close_overlay();
        self.status_line = match action {
            Some(ConfirmAction::CancelWorker { key, .. }) => {
                format!("Worker '{key}' was left running.")
            }
            Some(ConfirmAction::CancelAll) => "Every worker was left running.".to_string(),
            Some(ConfirmAction::Command { line }) => {
                format!("{} cancelled — nothing was sent.", line.trim())
            }
            Some(ConfirmAction::Quit) => "Quit cancelled.".to_string(),
            None => "Abort cancelled.".to_string(),
        };
        self.dirty = true;
        true
    }

    /// Take an abort arm off without exiting.
    ///
    /// The armed prompt occupies the confirm surface (§8.5), so taking the arm off
    /// takes the prompt off with it. A confirm the operator opened themselves
    /// (`Ctrl+D`, `x`) holds a `confirm_action` and is a different request, so it is
    /// left alone — which is why the guard is the request, not the surface.
    fn disarm_abort(&mut self) {
        if !self.confirm_abort && self.abort_armed_at.is_none() {
            return;
        }
        self.confirm_abort = false;
        self.abort_armed_at = None;
        if self.confirm_action.is_none() && self.overlay == state::OverlayState::Confirm {
            self.close_overlay();
        }
        self.dirty = true;
    }

    // ---- composer helpers (spec §4.4, §6) ------------------------------------

    /// Route one key to the composer widget. This is the single place the §4.4 rule
    /// lives, and it contains no focus test anywhere.
    fn edit_key(&mut self, key: KeyEvent) {
        self.ensure_textarea_in_sync();
        self.textarea.input(key);
        self.sync_input_from_textarea();
        self.dirty = true;
    }

    /// Insert a newline at the caret (§5: modified Enter and `Ctrl+J`).
    fn newline(&mut self) {
        self.ensure_textarea_in_sync();
        self.textarea.insert_newline();
        self.sync_input_from_textarea();
        self.dirty = true;
    }

    /// Whether the draft is empty — the precondition that turns `?`, `x` and `e` from
    /// typed characters into mnemonics (§4.4).
    fn composer_is_empty(&self) -> bool {
        self.input_text.is_empty() && self.textarea.lines().iter().all(|line| line.is_empty())
    }

    /// `Tab` with a `/` draft: accept the ghost completion §6 offers.
    ///
    /// Only the *remainder* of the token is inserted, at the end of what the operator
    /// typed, because `components::input::suggest` completes no further than the
    /// longest common prefix of the candidates and never guesses a command. Returns
    /// `false` when the draft is not in command mode or has nothing to complete, which
    /// is what lets `Tab` fall through to focus cycling.
    fn accept_ghost_completion(&mut self) -> bool {
        self.ensure_textarea_in_sync();
        let buffer = self.textarea.lines().join("\n");
        let Some(token) = input::command_token(&buffer) else {
            return false;
        };
        let Some(completion) = input::suggest(&buffer) else {
            return false;
        };
        let token_len = token.len();
        let Some(tail) = completion.get(token_len..) else {
            return false;
        };
        if tail.is_empty() {
            return false;
        }
        // The caret goes to the end of the typed token on line 1, so a draft with a
        // caret in the middle of the command still completes in place.
        let first_line = self.textarea.lines().first().cloned().unwrap_or_default();
        let leading = first_line.chars().count() - first_line.trim_start().chars().count();
        let column = (leading + token.chars().count()) as u16;
        self.textarea
            .move_cursor(ratatui_textarea::CursorMove::Jump(0, column));
        self.textarea.insert_str(tail);
        self.sync_input_from_textarea();
        self.dirty = true;
        true
    }

    // ---- the §6 commands the TUI answers itself -----------------------------

    /// The argument column the command table advertises for `name`. Reading the
    /// hint off the table rather than restating it is what keeps the refusal, the
    /// composer's ghost row and `/help` in agreement.
    fn advertised_arguments(name: &str) -> &'static str {
        input::slash_commands()
            .into_iter()
            .find(|command| command.name.eq_ignore_ascii_case(name))
            .map_or("", |command| command.args)
    }

    /// One notice line for a command: pushed into the transcript and recorded in
    /// `status_line` (the field `capture_ui` and the pre-rebuild surfaces read;
    /// §7.4 keeps a single line of that traffic). This is what stops a command
    /// from ever answering with silence (G5). On screen the persistent echo is the
    /// state change itself — the `plan:` / `workers:` / `think:` chips on status
    /// row 2, the mode chip in the header, the transcript's own `● LIVE` /
    /// `↓ N new` chip — and this line is the immediate one.
    ///
    /// It deliberately does **not** touch `chat_auto_scroll`: that flag is the
    /// mirror of transcript follow and moves only through [`Self::set_follow`] and
    /// the scroll path (G7), so a notice that forced it on would quietly switch
    /// follow back on underneath a `/follow off`.
    fn notice(&mut self, text: &str) {
        self.messages.push(format!("[CLI] {text}"));
        self.status_line = text.lines().next().unwrap_or("").to_string();
        self.dirty = true;
    }

    /// §6: answer the commands the composer's table carries, and refuse the
    /// spellings it does not. Returns whether the submission was answered here,
    /// i.e. that it must never reach the session loop.
    ///
    /// The confirm-gated family is already intercepted by
    /// [`Self::arm_destructive_command`] before this runs, so its rows reach here
    /// only as [`CommandIntent::Loop`], which is the loop's to answer.
    fn run_command_line(&mut self, line: &str) -> bool {
        match command_intent(line) {
            CommandIntent::Text | CommandIntent::Loop => false,
            CommandIntent::Local(command) => {
                self.run_local_command(command);
                true
            }
            CommandIntent::BadArgument { name, argument } => {
                let accepted = Self::advertised_arguments(&name);
                let expected = if accepted.is_empty() {
                    format!("{name} takes no argument")
                } else {
                    format!("{name} takes {accepted}")
                };
                self.notice(&format!("{expected} — \"{name} {argument}\" was not run."));
                true
            }
            CommandIntent::Unknown { token } => {
                self.notice(&format!(
                    "{token} is not a command — /help prints the command table, F1 the key legend."
                ));
                true
            }
        }
    }

    /// Run one command the TUI answers itself. Every arm calls the transition the
    /// mirrored key calls, so the command and the key cannot drift apart.
    fn run_local_command(&mut self, command: LocalCommand) {
        match command {
            // §5 + acceptance criterion 3: `/help`, the `F1` overlay and the
            // footer print **one** table, built from `legend()` plus the command
            // table the composer reads. Printing this rather than a copy is what
            // makes the three surfaces byte-identical, and the legend used to be
            // a hard-coded string that had already gone stale.
            LocalCommand::Help => {
                // A notice, so the same rule as `notice()`: it writes a transcript
                // line and never the follow state (G7, item 14).
                self.messages.push(format!("[CLI] {}", help::help_text()));
                self.dirty = true;
            }
            LocalCommand::Thinking => {
                self.show_thought = !self.show_thought;
                self.messages.push(format!(
                    "[CLI] Thinking display: {}",
                    if self.show_thought { "ON" } else { "OFF" }
                ));
                self.dirty = true;
            }
            // `e` already states its outcome in the transcript, sticky §8.5
            // buffer first, so `/error` is that function and nothing else.
            LocalCommand::RecallError => self.recall_last_error(),
            // Bare `/mode` is the question, not a no-op: it answers with the mode
            // on screen and the arguments the table advertises.
            LocalCommand::Mode(None) => self.notice(&format!(
                "View mode {} — /mode {}",
                self.view_mode.label(),
                Self::advertised_arguments("/mode")
            )),
            LocalCommand::Mode(Some(mode)) => {
                self.set_view_mode(mode);
                self.notice(&format!("View mode {} ({})", mode.label(), mode.tag()));
            }
            LocalCommand::Plan(on) => {
                let plan = on.unwrap_or(!self.show_plan_panel);
                self.set_panes(plan, self.show_subagent_panel);
                self.notice(&format!(
                    "Plan pane {} (Ctrl+P does the same)",
                    if self.show_plan_panel { "on" } else { "off" }
                ));
            }
            LocalCommand::Workers(on) => {
                let workers = on.unwrap_or(!self.show_subagent_panel);
                self.set_panes(self.show_plan_panel, workers);
                self.notice(&format!(
                    "Workers pane {} (Ctrl+A does the same)",
                    if self.show_subagent_panel {
                        "on"
                    } else {
                        "off"
                    }
                ));
            }
            LocalCommand::Follow(on) => {
                let follow = on.unwrap_or(!self.follow);
                self.set_follow(follow);
                self.notice(&format!(
                    "Transcript follow {} (Ctrl+F does the same)",
                    if self.follow { "on" } else { "off" }
                ));
            }
        }
    }

    /// `e` / `/error`: print the sticky last error again (spec §8.5).
    ///
    /// The §8.5 buffer comes first, because it is the report as it was made: the
    /// categorical cause plus the **whole** text, multi-line included. `status_line`
    /// keeps only the first line of that traffic (§7.4) and the transcript can be
    /// cleared (`/reset`, a restore, a pruning), so a transcript-only scan loses the
    /// cause exactly when the operator needs it — which is why the scan below is the
    /// fallback, not the source. Saying so out loud when the buffer is empty is what
    /// keeps `e` from ever being a silent key (G5).
    fn recall_last_error(&mut self) {
        if let Some(error) = self.last_error.clone() {
            self.messages.push(format!(
                "✗ {}\n{}\nsee .marmel/marmel.log",
                error.category, error.detail
            ));
            self.dirty = true;
            return;
        }
        let found = Self::error_line(&self.messages, &self.status_line);
        match found {
            Some(text) => {
                self.messages
                    .push(format!("✗ {text}\nsee .marmel/marmel.log"));
            }
            None => self.messages.push(
                "[CLI] No error recorded to recall: the sticky last-error buffer (§8.5) is \
                 empty and the transcript carries no error traffic."
                    .to_string(),
            ),
        }
        // Recalling an error is a notice, not a scroll gesture: the transcript stays
        // wherever the operator left it unless follow is already on (item 14).
        self.dirty = true;
    }

    /// The last error-looking line in the transcript, else in the status line.
    ///
    /// The spellings are the ones the session loop actually emits today
    /// (`LLM error: <category>`, `System Error …`, and the `✗` line this method
    /// itself produces), so the recall stays honest even for a session that only
    /// carries old traffic.
    fn error_line(messages: &[String], status_line: &str) -> Option<String> {
        let looks_like_error = |text: &str| {
            let trimmed = text.trim_start();
            trimmed.starts_with("LLM error")
                || trimmed.starts_with("System Error")
                || trimmed.starts_with('✗')
                || trimmed.contains("LLM error:")
        };
        messages
            .iter()
            .rev()
            .find(|message| message.lines().any(looks_like_error))
            .and_then(|message| {
                message
                    .lines()
                    .find(|line| looks_like_error(line))
                    .map(str::to_string)
            })
            .or_else(|| {
                status_line
                    .lines()
                    .find(|line| looks_like_error(line))
                    .map(str::to_string)
            })
    }

    /// Insert a pasted block into the composer as one contiguous edit.
    ///
    /// With bracketed paste enabled (see `init()`) the terminal delivers the whole
    /// body in a single `Event::Paste`, so nothing inside it can reach the
    /// `KeyCode::Enter` arm — a multi-line paste can never fire one steering command
    /// per line, and this helper never calls `submit()`.
    ///
    /// Line endings are normalised (`\r\n` and lone `\r` become `\n`) and a single
    /// trailing newline, which terminals commonly append to a paste, is dropped so the
    /// buffer does not end on an empty line that the next Enter would swallow.
    pub(crate) fn paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalised = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text.to_string()
        };
        let body = normalised.strip_suffix('\n').unwrap_or(&normalised);
        if body.is_empty() {
            return;
        }
        self.ensure_textarea_in_sync();
        // A single insert_str splices the block in one edit: one undo record and one
        // line rewrite, so a very large paste cannot trigger a per-char re-render storm.
        self.textarea.insert_str(body);
        self.sync_input_from_textarea();
        self.dirty = true;
    }

    pub(crate) fn sync_input_from_textarea(&mut self) {
        self.input_text = self.textarea.lines().join("\n");
        let cursor = self.textarea.cursor();
        let (row, col) = (cursor.0, cursor.1);
        let mut byte_idx = 0;
        for (r, line) in self.textarea.lines().iter().enumerate() {
            if r == row {
                let char_idx = col.min(line.chars().count());
                byte_idx += line
                    .chars()
                    .take(char_idx)
                    .map(|c| c.len_utf8())
                    .sum::<usize>();
                break;
            } else {
                byte_idx += line.len() + 1;
            }
        }
        self.cursor = byte_idx;
    }

    pub(crate) fn set_input_content(&mut self, text: &str) {
        self.input_text = text.to_string();
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(|s| s.to_string()).collect()
        };
        self.textarea = ratatui_textarea::TextArea::new(lines);
        self.textarea.move_cursor(ratatui_textarea::CursorMove::End);
        self.cursor = text.len();
    }

    pub(crate) fn ensure_textarea_in_sync(&mut self) {
        if self.textarea.lines().join("\n") != self.input_text {
            let lines: Vec<String> = if self.input_text.is_empty() {
                vec![String::new()]
            } else {
                self.input_text.split('\n').map(|s| s.to_string()).collect()
            };
            self.textarea = ratatui_textarea::TextArea::new(lines);
            self.textarea.move_cursor(ratatui_textarea::CursorMove::End);
        }
    }

    pub(crate) fn sanitize_cursor(&mut self) {
        if self.cursor > self.input_text.len() {
            self.cursor = self.input_text.len();
        }
        while !self.input_text.is_char_boundary(self.cursor) {
            if self.cursor == 0 {
                break;
            }
            self.cursor -= 1;
        }
    }

    /// Clamp all scroll offsets to their recomputed maxima.
    pub(crate) fn clamp_scrolls(&mut self) {
        let w = self.chat_width.get();
        let n = self.estimated_chat_lines(w);
        let h = self.chat_height.get();
        self.chat_scroll = self.chat_scroll.min(n.saturating_sub(h) as u16);

        self.plan_scroll = self.plan_scroll.min(self.plan_max_scroll);

        let w = self.subagent_width.get();
        let n = self.estimated_subagent_lines(w);
        let h = self.subagent_height.get();
        self.subagent_scroll = self.subagent_scroll.min(n.saturating_sub(h) as u16);

        let list_height = self.subagent_area.height.saturating_sub(3).max(1) as usize;
        let max_list = self.subagents.len().saturating_sub(list_height);
        self.subagent_list_scroll = self.subagent_list_scroll.min(max_list as u16);
    }

    /// Scroll the focused pane. For the transcript pane the gesture also states the
    /// follow decision it expresses (G7, spec §10 item 14): pulling the viewport off
    /// the tail turns follow **off**, and landing back on the tail turns it **on**.
    ///
    /// The transcript arm never assigns the legacy `chat_auto_scroll` mirror: that
    /// flag is a projection of `follow` and moves only through
    /// [`Self::set_follow`].
    pub(crate) fn scroll_up(&mut self, amount: u16) {
        match self.focused_panel {
            FocusedPanel::Chat => {
                self.set_follow(false);
                self.chat_scroll = self.chat_scroll.saturating_sub(amount);
            }
            FocusedPanel::Plan => {
                self.plan_auto_scroll = false;
                self.plan_scroll = self.plan_scroll.saturating_sub(amount);
            }
            FocusedPanel::Subagents => {
                self.subagent_autoscroll = false;
                self.subagent_scroll = self.subagent_scroll.saturating_sub(amount);
            }
        }
    }

    pub(crate) fn scroll_down(&mut self, amount: u16) {
        match self.focused_panel {
            FocusedPanel::Chat => {
                let w = self.chat_width.get();
                let n = self.estimated_chat_lines(w);
                let h = self.chat_height.get();
                let max_scroll = n.saturating_sub(h) as u16;
                self.chat_scroll = self.chat_scroll.saturating_add(amount).min(max_scroll);
                if self.chat_scroll >= max_scroll {
                    // A scroll that lands on the tail is the explicit
                    // scroll-to-end gesture §10 item 14 allows to re-arm follow.
                    self.set_follow(true);
                }
            }
            FocusedPanel::Plan => {
                self.plan_scroll = self
                    .plan_scroll
                    .saturating_add(amount)
                    .min(self.plan_max_scroll);
                if self.plan_scroll >= self.plan_max_scroll {
                    self.plan_auto_scroll = true;
                }
            }
            FocusedPanel::Subagents => {
                let w = self.subagent_width.get();
                let n = self.estimated_subagent_lines(w);
                let h = self.subagent_height.get();
                let max_scroll = n.saturating_sub(h) as u16;
                self.subagent_scroll = self.subagent_scroll.saturating_add(amount).min(max_scroll);
                if self.subagent_scroll >= max_scroll {
                    self.subagent_autoscroll = true;
                }
            }
        }
    }

    /// Jump one pane's viewport to its top. For the transcript this is an explicit
    /// gesture away from the tail, so it is stated as the follow decision it is
    /// (G7, item 14) rather than as a write of the legacy mirror.
    pub(crate) fn scroll_to_top(&mut self) {
        match self.focused_panel {
            FocusedPanel::Chat => {
                self.set_follow(false);
                self.chat_scroll = 0;
            }
            FocusedPanel::Plan => {
                self.plan_auto_scroll = false;
                self.plan_scroll = 0;
            }
            FocusedPanel::Subagents => {
                self.subagent_autoscroll = false;
                self.subagent_scroll = 0;
            }
        }
    }

    /// Jump one pane's viewport to its bottom — for the transcript the explicit
    /// "jump to bottom" gesture that §10 item 14 allows to turn follow back on. It
    /// is stated through [`Self::set_follow`], never by assigning the mirror.
    pub(crate) fn scroll_to_bottom(&mut self) {
        match self.focused_panel {
            FocusedPanel::Chat => {
                let w = self.chat_width.get();
                let n = self.estimated_chat_lines(w);
                let h = self.chat_height.get();
                self.chat_scroll = n.saturating_sub(h) as u16;
                self.set_follow(true);
            }
            FocusedPanel::Plan => {
                self.plan_scroll = self.plan_max_scroll;
                self.plan_auto_scroll = true;
            }
            FocusedPanel::Subagents => {
                let w = self.subagent_width.get();
                let n = self.estimated_subagent_lines(w);
                let h = self.subagent_height.get();
                self.subagent_scroll = n.saturating_sub(h) as u16;
                self.subagent_autoscroll = true;
            }
        }
    }

    /// Resolve a stream/preemption **tag** to the key of the row it belongs to.
    ///
    /// A tag is spelled `{agent}-{task}` or bare `{agent}`
    /// (`orchestrator/execution.rs:255`), while a row is named by the **effective
    /// registry key** (`WorkerEntry::key`), which may carry a handle the tag can
    /// never spell: `{agent}-wN` for a taskless worker, `{base}#N` when a live
    /// worker already owned the natural key. Without this resolution the
    /// presentation maps (`subagent_is_thinking`, `subagent_turn_thinking`) and
    /// the streamed text land on a *second* row for the same worker — or on no
    /// row at all, which is the failed lookup the pane used to hit.
    ///
    /// Order, strongest first: exact key, natural key (the tag with the `#…`
    /// collision handle stripped), the `{agent}-wN` form of a bare agent tag,
    /// then task-id equality. Falling back to the tag itself is what keeps the
    /// first event for a not-yet-registered worker working: the row is then
    /// created under the tag and re-labelled by `set_subagents` when the
    /// registry publishes the real key.
    pub(crate) fn resolve_row_key(&self, tag: &str) -> String {
        // The rule is owned by the projection layer (`ui::helpers`) so the
        // streamed text, the thinking flags and the arbitration pause can never
        // resolve the same tag onto different rows.
        crate::ui::helpers::subagent_row_key_for_tag(&self.subagents, tag)
    }

    /// Create or update a subagent entry in the local list. Used by the
    /// `Delegation` event handler so the panel reflects Started/Completed
    /// transitions immediately, and by `set_subagents` to adopt the
    /// session-loop's authoritative list.
    pub(crate) fn upsert_subagent(&mut self, name: &str, is_active: bool, log: &str) {
        if is_active {
            self.show_subagent_panel = true;
        }
        let now = std::time::Instant::now();
        let target_idx = match self
            .subagents
            .iter_mut()
            .enumerate()
            .find(|(_, s)| s.name == name)
        {
            Some((idx, existing)) => {
                if existing.is_active && !is_active {
                    if let Some(st) = existing.started_at.take() {
                        existing.worked_duration += st.elapsed();
                    }
                } else if !existing.is_active && is_active {
                    existing.started_at = Some(now);
                }
                existing.is_active = is_active;
                // A row that stops running can never keep a queue position
                // behind: completion, failure and cancel all land here, and the
                // queue state belongs to a live delegation only. The arbitration
                // pause likewise belongs to a live stream, so it goes down here
                // too instead of waiting for the next projection edge.
                if !is_active {
                    existing.queue = None;
                    existing.paused = false;
                }
                existing.last_activity_at = Some(now);
                existing.logs.push(log.to_string());
                idx
            }
            None => {
                let idx = self.subagents.len();
                self.subagents.push(SubagentDetail {
                    name: name.to_string(),
                    task_id: None,
                    prompt: String::new(),
                    started_at: if is_active { Some(now) } else { None },
                    worked_duration: std::time::Duration::ZERO,
                    last_activity_at: Some(now),
                    logs: vec![log.to_string()],
                    thinking: String::new(),
                    content: String::new(),
                    is_active,
                    context_tokens: 0,
                    queue: None,
                    paused: false,
                    // D1 lifecycle fields arrive from the registry projection
                    // (`set_subagents` mirrors `WorkerEntry`), never from here.
                    status_label: None,
                    implementation_turns: 0,
                    validation_rounds: 0,
                    latest_validator_feedback: None,
                });
                idx
            }
        };
        if is_active {
            self.selected_subagent_idx = target_idx;
        }
    }

    /// Walk backward through input history (Ctrl+Up).
    pub(crate) fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.input_draft = self.input_text.clone();
            self.history_index = Some(self.history.len() - 1);
        } else if let Some(idx) = self.history_index
            && idx > 0
        {
            self.history_index = Some(idx - 1);
        }
        if let Some(idx) = self.history_index {
            let text = self.history[idx].clone();
            self.set_input_content(&text);
        }
    }

    /// Walk forward through input history (Ctrl+Down).
    pub(crate) fn history_next(&mut self) {
        if let Some(idx) = self.history_index {
            if idx + 1 < self.history.len() {
                self.history_index = Some(idx + 1);
                let text = self.history[idx + 1].clone();
                self.set_input_content(&text);
            } else {
                self.history_index = None;
                let text = self.input_draft.clone();
                self.set_input_content(&text);
            }
        }
    }

    /// Consolidate streaming thought/content and steer sentence buffer into messages.
    pub(crate) fn commit_turn_content(&mut self) {
        if !self.steer_sentence_buffer.trim().is_empty() {
            let remaining = std::mem::take(&mut self.steer_sentence_buffer);
            self.append_steer_sentence(remaining.trim());
        }
        self.steer_sentence_buffer.clear();

        let mut msg_accum = String::new();
        if !self.current_thought.trim().is_empty() {
            msg_accum.push_str(&format!(
                "<think>\n{}\n</think>\n",
                self.current_thought.trim()
            ));
            self.current_thought.clear();
        }
        if !self.current_content.is_empty() {
            msg_accum.push_str(&self.current_content);
            self.current_content.clear();
        }
        if !msg_accum.trim().is_empty() {
            self.messages.push(msg_accum);
            if self.chat_auto_scroll {
                let w = self.chat_width.get();
                let n = self.estimated_chat_lines(w);
                let h = self.chat_height.get();
                self.chat_scroll = n.saturating_sub(h) as u16;
            }
        }
    }

    /// Send the current input line to the agent as a user message.
    pub(crate) fn submit(&mut self) {
        self.ensure_textarea_in_sync();
        let line = if !self.input_text.is_empty() {
            std::mem::take(&mut self.input_text)
        } else {
            self.textarea.lines().join("\n")
        };
        self.textarea = ratatui_textarea::TextArea::default();
        self.input_text.clear();
        self.cursor = 0;
        if line.trim().is_empty() {
            let _ = self.tx.send(String::new());
            return;
        }
        // §6 + §10 item 17: every destructive slash command — the quit family,
        // `/cancel <id>`, `/cancel-all` and all six `/reset` spellings — stops here
        // and waits on the confirm surface. Nothing is sent, no plan is cleared and
        // no exit is requested by the keystroke that typed it. This also removes the
        // renderer-local `/reset` short-circuit that used to answer five of the six
        // spellings by hand: they now all take the loop's full path on `y`.
        if self.arm_destructive_command(&line) {
            return;
        }
        // §6: the commands the TUI answers itself (`/help`, `/thought`, `/error`,
        // `/mode`, `/plan`, `/workers`, `/follow`), the refusal for a row given an
        // argument it does not take, and the refusal for a token the table does not
        // carry. All three are resolved from the one command table the composer
        // autocompletes and `/help` prints, so an advertised command is a handled
        // command and a typed command is never quietly handed to the model.
        if self.run_command_line(&line) {
            return;
        }
        self.commit_turn_content();
        // Push to history (no consecutive duplicates).
        let trimmed = line.trim().to_string();
        if self.history.last() != Some(&trimmed) {
            self.history.push(trimmed.clone());
        }
        self.history_index = None;
        self.input_draft.clear();
        let user_tokens = crate::manager::context::count_text_tokens(&line);
        self.tokens_in = self.tokens_in.saturating_add(user_tokens);
        self.messages.push(format!("User: {trimmed}"));
        // §10 item 14 (D3): submitting is not a follow decision. The old path wrote
        // `chat_auto_scroll = true` here, which the view-state bridge then adopted
        // as `follow = true` — the silent re-arm that threw away an operator who had
        // deliberately scrolled up. While follow is on the transcript pane pins the
        // new line by itself; while it is off the line is counted by the
        // `↓ N new` chip instead. Neither may be decided by a submit.
        // §5: the bare words `stop / cancel / exit / quit / q / abort` are ordinary
        // steering text again — a one-word instruction must not end a run (UX
        // friction 6). They are still delivered verbatim, and the hint says which
        // spelling does open a confirm. The session loop reads them as text too,
        // because it asks [`Renderer::gates_destructive_commands`] before applying
        // its own whole-string abort test (see `session.rs::is_abort_for_renderer`).
        if crate::ui::helpers::is_bare_abort_word(&line) {
            self.messages.push(
                "[CLI] That is steering text, not a command: a bare stop / cancel / exit / \
                 quit / q / abort never ends the session. Use /exit (or Ctrl+D) for a \
                 confirmed quit, or Esc twice to arm abort."
                    .to_string(),
            );
            self.status_line = format!("Steering text: {trimmed} — use /exit for a confirmed quit");
        }
        let _ = self.tx.send(line);
    }

    /// Append a completed steer sentence as a `Marmennill: ` message.
    pub(crate) fn append_steer_sentence(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        let prefix = "Marmennill: ";
        if let Some(last) = self.messages.last_mut() {
            if last.starts_with(prefix) {
                last.push_str(chunk);
                self.invalidate_last_message_cache();
            } else {
                let trimmed = chunk.trim_start_matches('\n');
                self.messages.push(format!("{prefix}{trimmed}"));
            }
        } else {
            let trimmed = chunk.trim_start_matches('\n');
            self.messages.push(format!("{prefix}{trimmed}"));
        }
        // Output arriving is never a follow decision (item 14): while follow is on
        // the transcript pins the new sentence on its own, and while it is off the
        // sentence is counted below the viewport instead.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Press one key through the seam the live loop drives.
    fn press(renderer: &mut TuiRenderer, code: KeyCode, modifiers: KeyModifiers) -> bool {
        renderer.apply_terminal_event(&TermEvent::Key(KeyEvent::new(code, modifiers)))
    }

    /// Everything the composer has handed to the session loop so far.
    fn drain_submissions(renderer: &mut TuiRenderer) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(line) = renderer.rx.try_recv() {
            out.push(line);
        }
        out
    }

    /// Type `line` into the composer and press Enter.
    fn submit(renderer: &mut TuiRenderer, line: &str) {
        renderer.set_input_content(line);
        renderer.submit();
    }

    /// The whole view state the §6 view commands and their chords are allowed to
    /// move — the mode, the focus a mode change may repair, and the four toggles.
    /// Comparing this tuple is what proves a command does **exactly** what its key
    /// does, rather than merely something in the same direction.
    fn view(renderer: &TuiRenderer) -> (ViewMode, FocusPane, bool, bool, bool, bool) {
        (
            renderer.view_mode,
            renderer.focus,
            renderer.show_plan_panel,
            renderer.show_subagent_panel,
            renderer.follow,
            renderer.show_thought,
        )
    }

    /// §6 + the advertised/handled rule: every row the table carries has an owner.
    /// A row that resolved to neither would be a command `/help` advertises and no
    /// key or handler runs — the exact hole this task closes.
    #[test]
    fn every_advertised_command_has_an_owner() {
        for command in input::slash_commands() {
            let intent = command_intent(command.name);
            assert!(
                matches!(intent, CommandIntent::Local(_) | CommandIntent::Loop),
                "`{}` is advertised but nothing runs it: {intent:?}",
                command.name
            );
        }
    }

    /// The argument column of the table is the argument the handler accepts, so
    /// `/help` and the ghost row can never promise a spelling the command refuses.
    #[test]
    fn the_advertised_arguments_are_the_arguments_the_handler_takes() {
        let accepted: [(&str, CommandIntent); 9] = [
            (
                "/mode split",
                CommandIntent::Local(LocalCommand::Mode(Some(ViewMode::Split))),
            ),
            (
                "/mode TRANSCRIPT",
                CommandIntent::Local(LocalCommand::Mode(Some(ViewMode::Transcript))),
            ),
            (
                "/plan on",
                CommandIntent::Local(LocalCommand::Plan(Some(true))),
            ),
            (
                "/plan off",
                CommandIntent::Local(LocalCommand::Plan(Some(false))),
            ),
            (
                "/workers on",
                CommandIntent::Local(LocalCommand::Workers(Some(true))),
            ),
            (
                "/follow off",
                CommandIntent::Local(LocalCommand::Follow(Some(false))),
            ),
            ("/error", CommandIntent::Local(LocalCommand::RecallError)),
            ("/help", CommandIntent::Local(LocalCommand::Help)),
            ("/thought", CommandIntent::Local(LocalCommand::Thinking)),
        ];
        for (line, expected) in accepted {
            assert_eq!(command_intent(line), expected, "`{line}`");
        }
    }

    /// Who owns which spelling: the confirm surface owns the destructive family
    /// including the two vim spellings, and nothing else is a command at all.
    #[test]
    fn the_owner_of_each_spelling_is_resolved_from_the_table() {
        for spelling in [
            ":q",
            ":q!",
            "/quit",
            "/exit",
            "/q",
            "/abort",
            "/stop",
            "/cancel",
            "/cancel t-004",
            "/cancel-all",
            "/reset",
            "/reset_plan",
            "/reset-plan",
            "/clear_plan",
            "/clear-plan",
            "/reset_execution_plan",
        ] {
            assert_eq!(
                command_intent(spelling),
                CommandIntent::Loop,
                "`{spelling}`"
            );
        }
        // Not commands: prose, an unfinished draft, a path, a bare word (spec §5)
        // and a block, whatever its first line spells.
        for text in [
            "keep working on t-004",
            "/tmp/x is the path",
            "/",
            "// note",
            "stop",
            "/plan\nand the rest of the block",
        ] {
            assert_eq!(command_intent(text), CommandIntent::Text, "`{text}`");
        }
        // A token shaped like a command but absent from the table is reported, not
        // forwarded — `/retry` among them, which the table no longer carries
        // because there is no retry action to route it to.
        for token in ["/xyz", "/retry", "/paln"] {
            assert_eq!(
                command_intent(token),
                CommandIntent::Unknown {
                    token: token.to_string()
                },
                "`{token}`"
            );
        }
        // A row given an argument it does not take.
        assert_eq!(
            command_intent("/plan maybe"),
            CommandIntent::BadArgument {
                name: "/plan".to_string(),
                argument: "maybe".to_string(),
            }
        );
        // The confirm family reads the whole line, so an argument hanging off a
        // row that advertises none is refused rather than sent as a prompt.
        for spelling in ["/quit now", "/cancel-all now", "/reset now"] {
            assert!(
                matches!(command_intent(spelling), CommandIntent::BadArgument { .. }),
                "`{spelling}`: {:?}",
                command_intent(spelling)
            );
        }
        // `/cancel <worker key or task id>` is the one row of that family with an
        // argument, and a multi-word one stays the loop's business.
        assert_eq!(
            command_intent("/cancel coder_specialist-t-004"),
            CommandIntent::Loop
        );
    }

    /// Each command makes the same transition as the key that mirrors it: `Ctrl+P`
    /// (plan), `Ctrl+A` (workers), `Ctrl+F` (follow), `Ctrl+T` (thinking).
    #[test]
    fn each_toggle_command_makes_its_chords_transition() {
        for (command, code, modifiers) in [
            ("/plan", KeyCode::Char('p'), KeyModifiers::CONTROL),
            ("/workers", KeyCode::Char('a'), KeyModifiers::CONTROL),
            ("/follow", KeyCode::Char('f'), KeyModifiers::CONTROL),
            ("/thought", KeyCode::Char('t'), KeyModifiers::CONTROL),
        ] {
            let mut by_command = TuiRenderer::new();
            let mut by_chord = TuiRenderer::new();
            submit(&mut by_command, command);
            assert!(
                press(&mut by_chord, code, modifiers),
                "`{code:?}` is a binding"
            );
            assert_eq!(
                view(&by_command),
                view(&by_chord),
                "`{command}` moved the view state differently from `{code:?}`"
            );
            assert!(
                drain_submissions(&mut by_command).is_empty(),
                "`{command}` reached the session loop"
            );
            assert!(
                by_command.confirm_action.is_none(),
                "`{command}` asked for a confirm its key does not ask for"
            );
            assert!(by_command.dirty, "`{command}` left nothing to repaint");
            assert!(
                by_command
                    .messages
                    .iter()
                    .any(|line| line.starts_with("[CLI]")),
                "`{command}` gave no notice: {:?}",
                by_command.messages
            );
        }
    }

    /// `/mode <name>` is `F3`/`F4`/`F5`; the fourth mode has no direct key, and
    /// bare `/mode` is a question that gets answered.
    #[test]
    fn the_mode_command_sets_the_mode_the_function_key_sets() {
        for (argument, code) in [
            ("transcript", KeyCode::F(3)),
            ("plan", KeyCode::F(4)),
            ("workers", KeyCode::F(5)),
        ] {
            let mut by_command = TuiRenderer::new();
            let mut by_key = TuiRenderer::new();
            by_command.set_view_mode(ViewMode::Split);
            by_key.set_view_mode(ViewMode::Split);
            submit(&mut by_command, &format!("/mode {argument}"));
            assert!(press(&mut by_key, code, KeyModifiers::NONE));
            assert_eq!(
                view(&by_command),
                view(&by_key),
                "`/mode {argument}` moved the view state differently from `{code:?}`"
            );
            assert_eq!(by_command.view_mode.tag(), argument);
            assert!(
                by_command.status_line.contains("View mode"),
                "`/mode {argument}`: {}",
                by_command.status_line
            );
            assert!(drain_submissions(&mut by_command).is_empty());
        }

        // `split` is the mode `F2` only cycles through, and the spelling is
        // case-insensitive like every other command.
        let mut renderer = TuiRenderer::new();
        submit(&mut renderer, "/mode SPLIT");
        assert_eq!(renderer.view_mode, ViewMode::Split);
        assert!(renderer.show_plan_panel && renderer.show_subagent_panel);

        // Bare `/mode` answers with the mode on screen and the arguments the table
        // advertises, and changes nothing.
        let mut renderer = TuiRenderer::new();
        let before = view(&renderer);
        submit(&mut renderer, "/mode");
        assert_eq!(view(&renderer), before, "bare `/mode` changed the mode");
        assert!(
            renderer
                .status_line
                .contains(&format!("View mode {}", renderer.view_mode.label())),
            "{}",
            renderer.status_line
        );
        assert!(
            renderer
                .status_line
                .contains("transcript|plan|workers|split"),
            "bare `/mode` must state the arguments it takes: {}",
            renderer.status_line
        );
        assert!(drain_submissions(&mut renderer).is_empty());
    }

    /// `on` / `off` state the target instead of flipping it, and stay idempotent.
    #[test]
    fn the_argument_forms_state_the_target_rather_than_flipping_it() {
        let mut renderer = TuiRenderer::new();
        submit(&mut renderer, "/plan on");
        assert!(renderer.show_plan_panel);
        submit(&mut renderer, "/plan on");
        assert!(renderer.show_plan_panel, "`/plan on` is idempotent");
        submit(&mut renderer, "/plan off");
        assert!(!renderer.show_plan_panel);

        // `/workers on` over a plan pane that is already on lands in M4, exactly
        // where `Ctrl+A` lands from M2.
        let mut by_command = TuiRenderer::new();
        let mut by_chord = TuiRenderer::new();
        submit(&mut by_command, "/plan on");
        submit(&mut by_command, "/workers on");
        press(&mut by_chord, KeyCode::Char('p'), KeyModifiers::CONTROL);
        press(&mut by_chord, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(view(&by_command), view(&by_chord));
        assert_eq!(by_command.view_mode, ViewMode::Split);

        // Follow is the G7 flag plus its legacy mirror, as `Ctrl+F` leaves them.
        submit(&mut renderer, "/follow off");
        assert!(!renderer.follow && !renderer.chat_auto_scroll);
        submit(&mut renderer, "/follow on");
        assert!(renderer.follow && renderer.chat_auto_scroll);
        assert!(drain_submissions(&mut renderer).is_empty());
    }

    /// `/error` is the `e` mnemonic: the same recall, the same line, no confirm.
    #[test]
    fn the_error_command_recalls_what_the_e_key_recalls() {
        let planted = vec![
            "User: rebuild the TUI".to_string(),
            "LLM error: http — 503 upstream".to_string(),
        ];
        let mut by_command = TuiRenderer::new();
        let mut by_key = TuiRenderer::new();
        by_command.messages = planted.clone();
        by_key.messages = planted;

        submit(&mut by_command, "/error");
        assert!(
            press(&mut by_key, KeyCode::Char('e'), KeyModifiers::NONE),
            "`e` is a binding with an empty composer"
        );

        assert_eq!(
            by_command.messages, by_key.messages,
            "`/error` and `e` recalled different traffic"
        );
        let recalled = by_command.messages.last().cloned().unwrap_or_default();
        assert!(recalled.starts_with('✗'), "{recalled}");
        assert!(recalled.contains("marmel.log"), "{recalled}");
        assert!(drain_submissions(&mut by_command).is_empty());
    }

    /// The rule this task is named for: an unknown `/xyz` is answered where it is
    /// typed and never reaches the model.
    #[test]
    fn an_unknown_command_is_refused_and_never_reaches_the_model() {
        for token in ["/xyz", "/retry", "/paln", "/p"] {
            let mut renderer = TuiRenderer::new();
            submit(&mut renderer, token);
            assert!(
                drain_submissions(&mut renderer).is_empty(),
                "`{token}` was sent to the session loop"
            );
            assert!(renderer.history.is_empty(), "`{token}` was recorded");
            let notice = renderer.messages.last().cloned().unwrap_or_default();
            assert!(
                notice.contains("is not a command"),
                "`{token}` gave no unknown-command feedback: {notice}"
            );
            assert!(
                renderer.status_line.contains(token),
                "`{token}`: {}",
                renderer.status_line
            );
            assert!(
                renderer.confirm_action.is_none(),
                "`{token}` opened a confirm"
            );
        }
    }

    /// A row given an argument it does not take is refused, unchanged and unsent.
    /// That covers the confirm family too: `/quit now` must neither open a confirm
    /// nor reach the model as a prompt.
    #[test]
    fn a_bad_argument_is_refused_and_changes_nothing() {
        for (line, accepted) in [
            ("/plan maybe", "on|off"),
            ("/follow yes", "on|off"),
            ("/workers up", "on|off"),
            ("/mode frobnicate", "transcript|plan|workers|split"),
            ("/help now", "no argument"),
            ("/error now", "no argument"),
            ("/quit now", "no argument"),
            ("/cancel-all now", "no argument"),
        ] {
            let mut renderer = TuiRenderer::new();
            let before = view(&renderer);
            submit(&mut renderer, line);
            assert_eq!(view(&renderer), before, "`{line}` moved the view state");
            assert!(
                renderer.confirm_action.is_none(),
                "`{line}` opened a confirm for a spelling no gate accepts"
            );
            assert!(
                drain_submissions(&mut renderer).is_empty(),
                "`{line}` was sent to the session loop"
            );
            let notice = renderer.messages.last().cloned().unwrap_or_default();
            assert!(
                notice.contains(accepted),
                "`{line}` did not name what it accepts: {notice}"
            );
            assert!(notice.contains("was not run"), "{notice}");
        }
    }

    /// The table `/help` prints is the table the dispatch reads, argument column
    /// included — and the command with no handler behind it is no longer in it.
    #[test]
    fn the_printed_command_table_names_the_arguments_and_the_vim_spellings() {
        let mut renderer = TuiRenderer::new();
        submit(&mut renderer, "/help");
        let printed = renderer.messages.last().cloned().unwrap_or_default();
        for row in [
            "/plan on|off",
            "/workers on|off",
            "/follow on|off",
            "/mode transcript|plan|workers|split",
            ":q",
            ":q!",
        ] {
            assert!(
                printed.contains(row),
                "the printed table is missing `{row}`:\n{printed}"
            );
        }
        assert!(
            !printed.contains("/retry"),
            "`/retry` is still advertised:\n{printed}"
        );
    }

    /// The commands the TUI owns are answered locally: nothing to the loop, no
    /// confirm, one visible notice each — and the destructive family still stops
    /// at the confirm surface instead of being answered here.
    #[test]
    fn the_local_commands_answer_locally_and_the_destructive_ones_stay_gated() {
        for line in [
            "/help",
            "/thought",
            "/mode",
            "/mode workers",
            "/plan",
            "/plan off",
            "/workers",
            "/workers on",
            "/follow",
            "/follow on",
            "/error",
        ] {
            let mut renderer = TuiRenderer::new();
            submit(&mut renderer, line);
            assert!(
                drain_submissions(&mut renderer).is_empty(),
                "`{line}` reached the session loop"
            );
            assert!(
                renderer
                    .messages
                    .iter()
                    .any(|line| line.starts_with("[CLI]")),
                "`{line}` left no notice in the transcript"
            );
            assert!(renderer.dirty, "`{line}` left nothing to repaint");
        }
        for line in ["/quit", ":q", "/reset", "/cancel-all"] {
            let mut renderer = TuiRenderer::new();
            submit(&mut renderer, line);
            assert!(
                renderer.confirm_action.is_some(),
                "`{line}` skipped the confirm surface"
            );
            assert!(drain_submissions(&mut renderer).is_empty());
        }
    }

    // ---- spec §10 item 14 / defect D3: submit never re-arms follow ----------

    /// The state pair the G7 rule is about: `follow` is the operator's decision and
    /// `chat_auto_scroll` is only ever its mirror, so they may never disagree.
    fn follow_pair(renderer: &TuiRenderer) -> (bool, bool) {
        (renderer.follow, renderer.chat_auto_scroll)
    }

    /// A transcript with scrollback, so a moved viewport would be visible in the
    /// offset rather than only in a flag.
    fn transcript_with_scrollback() -> TuiRenderer {
        let mut renderer = TuiRenderer::new();
        renderer.messages = (0..40)
            .map(|i| format!("line {i:02} of a long answer"))
            .collect();
        renderer.set_focus(FocusPane::Transcript);
        renderer
    }

    /// D3, the exact defect: an operator who scrolled away from the tail and then
    /// typed an instruction used to find themselves back at the bottom. Submitting
    /// must leave `follow` exactly as it was, in both directions.
    #[test]
    fn submitting_a_line_never_moves_follow_or_its_mirror() {
        // Follow off: the scroll-off-tail gesture, then a submission.
        let mut renderer = transcript_with_scrollback();
        renderer.chat_scroll = 12;
        assert!(
            press(&mut renderer, KeyCode::PageUp, KeyModifiers::NONE),
            "PageUp scrolls the focused transcript"
        );
        assert_eq!(
            follow_pair(&renderer),
            (false, false),
            "the scroll gesture left the tail and restated follow"
        );
        let offset = renderer.chat_scroll;
        assert!(offset > 0, "and the viewport is off the tail, got {offset}");

        submit(&mut renderer, "keep going with the audit");
        assert_eq!(
            drain_submissions(&mut renderer),
            vec!["keep going with the audit".to_string()],
            "the line still reached the session loop"
        );
        assert!(
            renderer
                .messages
                .iter()
                .any(|line| line == "User: keep going with the audit"),
            "and it was echoed into the transcript"
        );
        assert_eq!(
            follow_pair(&renderer),
            (false, false),
            "the submission did not re-arm follow (item 14)"
        );
        assert_eq!(
            renderer.chat_scroll, offset,
            "and it did not move the viewport it is now counting lines below"
        );

        // Follow on: the same submission keeps it on, and keeps the mirror with it.
        let mut renderer = transcript_with_scrollback();
        assert!(renderer.follow, "follow starts on");
        submit(&mut renderer, "keep going with the audit");
        assert_eq!(follow_pair(&renderer), (true, true), "still following");
    }

    /// Every shape a submission can take — plain text, the bare stop-word hint, a
    /// command answered locally, and a command that went through the confirm
    /// surface — has to leave the follow pair alone.
    #[test]
    fn every_submission_shape_leaves_the_follow_choice_alone() {
        for line in [
            "a plain instruction",
            "cancel",
            "/help",
            "/thought",
            "/error",
            "/zz not a command",
            "/plan extra argument",
            "/mode",
        ] {
            let mut renderer = transcript_with_scrollback();
            renderer.set_follow(false);
            renderer.chat_scroll = 3;
            submit(&mut renderer, line);
            assert_eq!(
                follow_pair(&renderer),
                (false, false),
                "`{line}` moved the follow state or its mirror: {:?}",
                renderer.messages.last()
            );
            assert_eq!(
                renderer.chat_scroll, 3,
                "`{line}` moved the viewport while follow was off"
            );
        }

        // A confirmed destructive command is still a submission: `y` runs it and the
        // follow decision is still nobody's to change.
        let mut renderer = transcript_with_scrollback();
        renderer.set_follow(false);
        renderer.chat_scroll = 3;
        submit(&mut renderer, "/reset");
        assert!(
            renderer.confirm_action.is_some(),
            "`/reset` waits on the confirm surface"
        );
        assert_eq!(
            follow_pair(&renderer),
            (false, false),
            "arming the confirm did not re-arm follow"
        );
        assert!(
            press(&mut renderer, KeyCode::Char('y'), KeyModifiers::NONE),
            "`y` confirms inside the surface"
        );
        assert_eq!(
            drain_submissions(&mut renderer),
            vec!["/reset".to_string()],
            "the confirmed command went to the loop"
        );
        assert_eq!(
            follow_pair(&renderer),
            (false, false),
            "and the confirmed submission left follow alone"
        );
        assert_eq!(renderer.chat_scroll, 3);
    }

    /// The G5 rule that a command answers with a notice must not be paid for with
    /// the operator's scroll position: a notice writes a transcript line, never a
    /// follow decision.
    #[test]
    fn a_command_notice_never_arms_follow_but_the_follow_command_still_does() {
        for line in [
            "/help",
            "/thought",
            "/thought",
            "/error",
            "/mode plan",
            "/plan on",
            "/workers on",
            "/nonsense",
            "/mode nonsense",
        ] {
            let mut renderer = transcript_with_scrollback();
            renderer.set_follow(false);
            submit(&mut renderer, line);
            assert_eq!(
                follow_pair(&renderer),
                (false, false),
                "`{line}` answered by moving the transcript follow state"
            );
        }

        // `/follow` and its argument forms remain the explicit way back on, and they
        // move the mirror with the decision.
        for (line, expected) in [
            ("/follow", true),
            ("/follow off", false),
            ("/follow on", true),
        ] {
            let mut renderer = transcript_with_scrollback();
            renderer.set_follow(false);
            submit(&mut renderer, line);
            assert_eq!(
                follow_pair(&renderer),
                (expected, expected),
                "`{line}` is the explicit spelling of the decision"
            );
        }
    }

    /// Output arriving is the other half of G7: new lines may only move the
    /// viewport while follow is on, and must never arm it on their own.
    #[test]
    fn output_that_arrives_never_arms_follow_and_only_pins_while_following() {
        // Follow off: steer sentences and a committed turn add lines and leave both
        // the flag and the offset exactly where the operator put them.
        let mut renderer = transcript_with_scrollback();
        renderer.set_follow(false);
        renderer.chat_scroll = 4;
        renderer.current_content = "a streamed answer".to_string();
        renderer.append_steer_sentence("and a steer sentence");
        renderer.commit_turn_content();
        assert_eq!(
            follow_pair(&renderer),
            (false, false),
            "arriving output re-armed follow"
        );
        assert_eq!(renderer.chat_scroll, 4, "and moved the viewport");

        // Follow on: the same output is pinned to the tail, which is the behaviour
        // that must not be lost by the fix.
        let mut renderer = transcript_with_scrollback();
        renderer.set_follow(true);
        renderer.chat_scroll = 0;
        renderer.current_content = "a streamed answer".to_string();
        renderer.append_steer_sentence("and a steer sentence");
        assert_eq!(
            follow_pair(&renderer),
            (true, true),
            "follow stayed on with its mirror"
        );
        renderer.commit_turn_content();
        let tail = {
            let w = renderer.chat_width.get();
            let n = renderer.estimated_chat_lines(w);
            let h = renderer.chat_height.get();
            n.saturating_sub(h) as u16
        };
        assert!(tail > 0, "the fixture has scrollback to pin against");
        assert_eq!(
            renderer.chat_scroll, tail,
            "while following, committed output is pinned to the tail"
        );
    }

    /// The flip side of item 14: the two explicit ways back onto the tail still
    /// work, and they re-pin through the same transition.
    #[test]
    fn only_the_follow_toggle_and_a_scroll_to_the_tail_re_arm_follow() {
        // `Ctrl+F` re-arms.
        let mut renderer = transcript_with_scrollback();
        assert!(press(
            &mut renderer,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL
        ));
        assert_eq!(follow_pair(&renderer), (false, false), "off");
        assert!(press(
            &mut renderer,
            KeyCode::Char('f'),
            KeyModifiers::CONTROL
        ));
        assert_eq!(follow_pair(&renderer), (true, true), "back on");

        // A scroll that lands on the tail re-arms; one that does not, does not.
        let mut renderer = transcript_with_scrollback();
        renderer.set_follow(false);
        assert!(press(&mut renderer, KeyCode::PageUp, KeyModifiers::NONE));
        assert!(!renderer.follow, "still away from the tail");
        for _ in 0..8 {
            assert!(press(&mut renderer, KeyCode::PageDown, KeyModifiers::NONE));
        }
        assert!(
            renderer.follow,
            "walking down to the tail is an explicit scroll-to-end gesture"
        );

        // `End` is the jump-to-bottom spelling, `Home` the opposite.
        let mut renderer = transcript_with_scrollback();
        renderer.set_follow(false);
        assert!(press(&mut renderer, KeyCode::End, KeyModifiers::NONE));
        assert_eq!(
            follow_pair(&renderer),
            (true, true),
            "`End` jumps to the tail"
        );
        assert!(press(&mut renderer, KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(
            follow_pair(&renderer),
            (false, false),
            "`Home` leaves the tail again"
        );
    }

    /// The bridge that folds the pre-rebuild fields into view state may never turn
    /// a mirror write into `follow = true`: that fold *was* the silent re-arm. A
    /// write going the other way (the legacy scroll path leaving the tail) is still
    /// adopted, and a divergent mirror is repaired rather than trusted.
    #[test]
    fn the_legacy_bridge_repairs_the_mirror_instead_of_re_arming_it() {
        let mut renderer = transcript_with_scrollback();
        renderer.set_follow(false);

        // What the old submit path used to do: assign the flag behind `set_follow`.
        renderer.chat_auto_scroll = true;
        renderer.adopt_legacy_view_writes();
        assert!(
            !renderer.follow,
            "a stray write of the mirror re-armed follow (D3)"
        );
        assert!(
            !renderer.chat_auto_scroll,
            "and the mirror was left disagreeing with follow"
        );

        // An explicit off-write from the legacy scroll path is still a decision.
        let mut renderer = transcript_with_scrollback();
        assert!(renderer.follow);
        renderer.chat_auto_scroll = false;
        renderer.adopt_legacy_view_writes();
        assert!(
            !renderer.follow,
            "leaving the tail through the legacy path still turns follow off"
        );
        assert!(!renderer.chat_auto_scroll);
    }
}
