//! Keyboard and composer editing for [`TuiRenderer`] — the spec §5 keymap.
//!
//! One entry point per event, one function per binding: [`TuiRenderer::handle_events`]
//! drains crossterm, [`TuiRenderer::apply_terminal_event`] routes exactly one event
//! (and is the seam the §10.3 legend-parity test feeds synthetic events through), and
//! [`TuiRenderer::on_key_event`] holds the keymap in precedence order.
//!
//! Four rules shape the file:
//!
//! * **The composer is never starved (§4.4).** Printables, paste, `Backspace`,
//!   `Delete`, `Ctrl+Z/Y/J` and the caret keys reach `textarea.input(key)` in every
//!   mode and every focus state. The pre-rebuild gating — editing keys and printables
//!   were dead unless Chat held focus — is gone (UX friction 14, G6). The one
//!   printable that can be spent elsewhere is `?`, which opens the legend on an empty
//!   composer. **Every other printable reaches the draft in every focus**: the bare
//!   letter mnemonics (`x` for cancel-worker, `e` for last-error) are gone, because
//!   they armed on an empty composer — exactly the moment a letter is the first
//!   character of what the operator is typing. What carries those two capabilities
//!   now cannot collide with a draft at all: the `/error` and
//!   `/cancel <worker key or task id>` rows of the §6 command table, and the `Ctrl+X`
//!   chord (a Ctrl chord never inserts a character).
//! * **One key, one owner: focus decides it.** The keys that could belong either to
//!   the composer or to a pane viewport — `Up`, `Down`, `PageUp`, `PageDown`, `Home`,
//!   `End` — are settled by one predicate, `composer_has_focus`, which
//!   reads the real [`state::FocusPane`] and never the shape of the draft or the
//!   legacy `focused_panel` fold. The composer owns them iff it holds focus; a pane
//!   owns them otherwise, and then the composer's text, caret and history walk are
//!   left alone. Composer history is reachable through the `Ctrl+Up` / `Ctrl+Down`
//!   chord **alone**, in every focus: the plain arrows are never a history recall at
//!   all. With the composer focused they move the caret inside a multi-line draft,
//!   and where the caret has nowhere to go the press is an explicit no-op — not
//!   claimed, not repaint-worthy, and never a pane scroll either. This is what keeps
//!   "scroll the views" and "walk the input history" from being the same keystroke:
//!   an arrow that only ever *walked* a single-line draft was still reaching into the
//!   composer while the operator was reaching for the transcript (operator request,
//!   the reported Up/Down-versus-scroll conflict).
//!   A pane with two viewports splits them the same way: in the Workers pane
//!   `Up`/`Down` own the **roster** window and `PageUp`/`PageDown`/`Home`/`End` own
//!   the selected worker's **detail** log, so no key answers to both halves and every
//!   half has exactly one owner (`Self::scroll_worker_detail_to` holds the table).
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

use super::components::{activity, help, input};
use super::state::{self, FocusPane, ViewMode};
use super::{TuiRenderer, is_abort};
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
/// `/mode <name>` is `F3`/`F4`/`F5`, `/cancel <worker key or task id>` is
/// `Ctrl+X` — and none of them reaches the session loop, so a command is never a
/// prompt to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalCommand {
    /// `/help`: print the one legend the `F1` overlay and the footer share.
    Help,
    /// `/thought`: the `Ctrl+T` thinking chip.
    Thinking,
    /// `/error`: the sticky-error recall (spec §8.5), now the only route to it and
    /// therefore focus-independent.
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
    /// 3. `?` is the only printable that can be spent anywhere but the draft, and it
    ///    takes the key only on an **empty** composer; every other printable inserts,
    ///    in every focus and mode, whatever the draft says (§4.4). The bare-letter
    ///    mnemonics that used to sit in this arm — `x` (cancel the selected worker,
    ///    Workers pane only) and `e` (recall the last error, pane focus only) — are
    ///    removed on the operator's request: their precondition was an empty
    ///    composer, which is precisely when a letter is the first character of a word.
    ///    The two capabilities moved to spellings that cannot collide with typing: the
    ///    `/error` and `/cancel <worker key or task id>` rows of the §6 table, and the
    ///    `Ctrl+X` chord. `?` keeps its (unchanged) empty-draft precondition: on an
    ///    empty draft it opens the legend instead of inserting, which is the one
    ///    remaining keystroke in this class.
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
                // The collision-free spelling of the removed bare-`x` mnemonic: a
                // Ctrl chord never reaches the textarea, so it cannot intercept a
                // letter, and it carries the old scope and the old semantics exactly
                // — the selected Workers roster row, the confirm surface, nothing
                // else (§5, operator request).
                'x' => {
                    self.disarm_abort();
                    self.request_cancel_selected_worker_by_chord();
                    return true;
                }
                'z' => {
                    self.disarm_abort();
                    self.ensure_textarea_in_sync();
                    let before = self.input_text.clone();
                    self.textarea.undo();
                    self.sync_input_from_textarea();
                    self.end_history_walk_if_edited(&before);
                    self.dirty = true;
                    return true;
                }
                'y' => {
                    self.disarm_abort();
                    self.ensure_textarea_in_sync();
                    let before = self.input_text.clone();
                    self.textarea.redo();
                    self.sync_input_from_textarea();
                    self.end_history_walk_if_edited(&before);
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

        // 5. Navigation and composer editing. Every arm here settles the
        //    composer-versus-pane question with one predicate,
        //    [`Self::composer_has_focus`] — the real `FocusPane`, never the draft
        //    shape, the caret, or the legacy `focused_panel` fold that spells
        //    `Input` and `Transcript` with the same `Chat` token.
        match key.code {
            KeyCode::Up | KeyCode::Down => return self.scroll_or_history(key, ctrl),
            KeyCode::PageUp => {
                // A viewport gesture is a pane key: the composer has no viewport of
                // its own, so with the composer focused the key has no owner and is
                // reported as not a key press at all instead of claiming the event
                // and leaving the follow/scroll state describing a movement that
                // never happened.
                if self.composer_has_focus() {
                    return false;
                }
                self.scroll_by(true, 10);
                return true;
            }
            KeyCode::PageDown => {
                if self.composer_has_focus() {
                    return false;
                }
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
                if self.composer_has_focus() {
                    self.edit_key(key);
                } else if self.scroll_to_top() {
                    // The gesture away from the tail states the follow decision
                    // itself (`scroll_to_top` → `set_follow(false)`) — and a pane
                    // already at its top has nothing to state and nothing to
                    // repaint, while still owning the key.
                    self.dirty = true;
                }
                return true;
            }
            KeyCode::End => {
                if self.composer_has_focus() {
                    self.edit_key(key);
                } else if self.scroll_to_bottom() {
                    // And the jump that actually reaches the tail is the explicit
                    // way back on.
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

        // 6. Printables. The character is typed in any mode and any focus (§4.4).
        //    Only `?` can be spent elsewhere, and only on an empty draft: every other
        //    printable — `x` and `e` included, at the start of a draft or mid-word —
        //    reaches the textarea. The bare-letter mnemonics that used to be here are
        //    `/error`, `/cancel <worker key or task id>` and `Ctrl+X` now.
        if let KeyCode::Char(c) = key.code {
            if !ctrl && !alt && c == '?' && self.composer_is_empty() {
                self.open_help();
                return true;
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
    /// The walk order is independent of the geometry, and the geometry no longer
    /// argues with it: under the single slot rule of [`super::layout`] the pane that
    /// holds the **leading** slot (the left column of a columns split, the top row of
    /// a stack, or the whole body in M1) is the Plan in both modes that draw a Plan
    /// (M2 and M4), the Transcript in M1 and the roster in M3. Walking
    /// M1 → M2 → M3 → M4 therefore never carries the Plan from one side of the frame
    /// to the other; the leading slot is the Plan wherever a Plan is on screen.
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
    /// the follow decision itself — off the tail turns follow **off**, an arrival on
    /// the tail turns it back on — so the chip can never lag the viewport, and no
    /// caller has to restate it from the legacy mirror.
    ///
    /// The frame is only marked dirty when a viewport actually moved: a press at an
    /// extreme, or on a pane that has nothing to scroll, has nothing to repaint, and
    /// leaving `dirty` set there is what used to make the chip look like it had
    /// toggled (defect 9).
    fn scroll_by(&mut self, up: bool, amount: u16) {
        let moved = if up {
            self.scroll_up(amount)
        } else {
            self.scroll_down(amount)
        };
        if moved {
            self.dirty = true;
        }
    }

    /// `Up` / `Down` (§5). One predicate owns the decision: the composer holds the
    /// key **iff** it holds focus, and a pane holds it otherwise. That is the whole
    /// fix for the reported overlap — the same key used to be a composer key and a
    /// pane key at once, decided by the shape of the draft, so `Up` behind the
    /// composer could scroll the transcript and `Up` on the transcript could
    /// rewrite the composer's draft.
    ///
    /// * `Ctrl+Up` / `Ctrl+Down` is the **only** history binding, in every focus. It
    ///   is a chord, so it is unambiguous, it is how a multi-line entry is walked,
    ///   and no caret guard can get in front of it. It is also the only way to reach
    ///   the walk at all: the operator who wants the previous submission asks for it
    ///   by name instead of arrowing into it.
    /// * With the **composer** focused: the caret moves inside a multi-line draft
    ///   whenever it can. Everywhere else the plain keys belong to nothing — they are
    ///   not a history recall (that was the reported conflict: an arrow spent on
    ///   scrolling replaced the draft), they are not a pane step (the composer owns
    ///   the key), and they are not claimed, so no repaint, `follow` decision or
    ///   scroll offset is left describing a movement that never happened.
    /// * With a **pane** focused: one step of that pane's viewport, and never a
    ///   touch on the composer — not its draft, not its caret, not its history
    ///   bookkeeping. The Workers pane is the one pane with two viewports, so its
    ///   arrows belong to the roster window
    ///   ([`Self::scroll_worker_roster_by`]) while the selected worker's detail log
    ///   keeps the page and jump keys — the two-viewport rule is spelled out at
    ///   [`Self::scroll_worker_detail_to`].
    ///
    /// Returns whether the key did something. A composer press with nowhere for the
    /// caret to go is not claimed: it neither repaints nor leaves `follow` and a
    /// scroll offset stating a movement that never happened.
    fn scroll_or_history(&mut self, key: KeyEvent, ctrl: bool) -> bool {
        let up = key.code == KeyCode::Up;
        if ctrl {
            let walked = if up {
                self.history_prev()
            } else {
                self.history_next()
            };
            if walked {
                self.dirty = true;
            }
            return walked;
        }
        if !self.composer_has_focus() {
            if self.focus == FocusPane::Workers {
                // Defect 14: the roster window (`subagent_list_scroll`) had no key
                // owner at all — only a resize clamp — while the pane itself
                // advertised `↑`/`↓` as the keys that scroll it. They are, and they
                // are its only owner. The detail half one level in keeps
                // `PageUp`/`PageDown`/`Home`/`End`, so the pane's two viewports
                // still answer to two different keys.
                if self.scroll_worker_roster_by(up, 1) {
                    self.dirty = true;
                }
            } else {
                self.scroll_by(up, 1);
            }
            return true;
        }
        self.ensure_textarea_in_sync();
        let last_line = self.textarea.lines().len().saturating_sub(1);
        let line = self.textarea.cursor().0;
        let caret_moves = (up && line > 0) || (!up && line < last_line);
        if caret_moves {
            self.edit_key(key);
            return true;
        }
        // The composer holds the key, and there is nothing inside the composer for
        // it to do: the caret is against the edge of the draft. The history walk is
        // **not** the fallback any more — a plain arrow here belongs to no one, and
        // is reported as not a key press. Claiming it (or walking the history
        // through it) is exactly what made the operator's scroll attempt rewrite the
        // draft, and it is what the `Ctrl+Up` / `Ctrl+Down` chord of §5 is there to
        // make unnecessary.
        false
    }

    /// `Left` / `Right` in the Workers pane: move the roster selection, and ask the
    /// pane to show the selected row's newest line.
    ///
    /// This is the **selection reveal**, not a viewport gesture: it is the one place
    /// on the key path allowed to arm `subagent_autoscroll`, because it is choosing
    /// which detail the pane shows rather than scrolling one. The scroll keys state
    /// the same flag through the movement-guarded arms instead (defect 15), and the
    /// roster window itself belongs to `Up` / `Down`
    /// ([`Self::scroll_worker_roster_by`], defect 14) — the selection keys never
    /// write `subagent_list_scroll` by hand; the pane reveals the row it is given,
    /// against the same bounds the arrows clamp to.
    ///
    /// The reveal states the request and stops there. "Show me the newest line" is
    /// answered by the pass that counts the detail body
    /// (`components::activity::draw_detail`), which is the only place that knows
    /// where this row's tail is; the offset the keys left stands until that frame
    /// resolves it, so no estimate of the tail is ever parked into
    /// `subagent_scroll` from here.
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

    /// `Ctrl+X` (§5): ask to cancel the **selected** worker of the Workers pane — the
    /// collision-free replacement for the bare `x` the operator stopped wanting.
    ///
    /// The precondition is part of the binding and is unchanged from the old key: the
    /// request is scoped to the pane, so a row can never be cancelled from a focus
    /// that is not showing the roster. What a chord can do and a letter could not is
    /// never be silent (G5): a letter whose guard failed fell through and typed
    /// itself, while the chord says in the status line why it did nothing. The cancel
    /// itself still waits for `y`.
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

    /// The `Ctrl+X` arm of the keymap: [`Self::request_cancel_selected_worker`] plus
    /// the notice for the case where that guard is not met. It adds no capability and
    /// changes no semantics — same selected row, same captured identity, same confirm
    /// surface — it only replaces the letter that used to carry them.
    fn request_cancel_selected_worker_by_chord(&mut self) {
        if self.request_cancel_selected_worker() {
            return;
        }
        self.notice(if self.subagents.is_empty() {
            "Ctrl+X asks to cancel the selected roster row — no worker is on the roster."
        } else {
            "Ctrl+X asks to cancel the selected roster row of the Workers pane — press \
             Ctrl+A or F5 to focus it, or type /cancel <worker key or task id>."
        });
    }

    /// Put one destructive request on the confirm surface and state it in the
    /// status line. This is the single place the surface is armed, so every entry
    /// point — `Ctrl+D`, `Ctrl+X`, a slash command — ends on the same surface with the
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

    /// The **one** predicate answering "does the composer own this key, or does a
    /// pane?" — the real [`FocusPane`], projected through
    /// [`state::FocusPane::composer_has_focus`].
    ///
    /// Every arm of [`Self::on_key_event`] that has to choose between the composer
    /// and a pane viewport asks this and nothing else. It used to ask two different
    /// questions in two adjacent arms — `Home`/`End` tested the stored focus while
    /// `Up`/`Down`/`PageUp`/`PageDown` tested the shape of the draft (or nothing at
    /// all) — and the legacy three-valued `focused_panel` fold then resolved both
    /// onto the same `Chat` spelling, which collapsed the composer and the
    /// transcript into one owner. Inside the composer the draft now decides only
    /// whether the caret has somewhere to go; the history walk belongs to the
    /// `Ctrl+Up`/`Ctrl+Down` chord alone. Either way the draft never decides *who*
    /// gets the key.
    fn composer_has_focus(&self) -> bool {
        self.focus.composer_has_focus()
    }

    /// Route one key to the composer widget. This is the single place the §4.4 rule
    /// lives, and it contains no focus test anywhere.
    fn edit_key(&mut self, key: KeyEvent) {
        self.ensure_textarea_in_sync();
        let before = self.input_text.clone();
        self.textarea.input(key);
        self.sync_input_from_textarea();
        self.end_history_walk_if_edited(&before);
        self.dirty = true;
    }

    /// Insert a newline at the caret (§5: modified Enter and `Ctrl+J`).
    fn newline(&mut self) {
        self.ensure_textarea_in_sync();
        let before = self.input_text.clone();
        self.textarea.insert_newline();
        self.sync_input_from_textarea();
        self.end_history_walk_if_edited(&before);
        self.dirty = true;
    }

    /// End a history browse the moment the operator changes the draft.
    ///
    /// `history_index` is the renderer's claim that what stands in the composer is
    /// *entry n of the history*, and the `history n/m` chip draws it. Left set
    /// after a hand edit, the next `Ctrl+Up`/`Ctrl+Down` walks on from the stale
    /// position and overwrites what was just typed. A key that only moves the caret
    /// leaves the browse alone — walking the caret through a recalled entry is not
    /// an edit of it.
    fn end_history_walk_if_edited(&mut self, before: &str) {
        if self.history_index.is_some() && self.input_text != before {
            self.history_index = None;
        }
    }

    /// Whether the draft is empty — the precondition that turns `?` from a typed
    /// character into the legend key (§4.4). It is the only remaining use of this
    /// gate in the printable arm: the bare-letter mnemonics that also used it are
    /// gone, so no letter ever depends on what the draft does or does not say.
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
        self.end_history_walk_if_edited(&buffer);
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
            // `/error` already states its outcome in the transcript, sticky §8.5
            // buffer first, so the command is that function and nothing else.
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

    /// `/error`: print the sticky last error again (spec §8.5). This is now the only
    /// route to the recall, and a slash command is focus-independent: the draft, the
    /// pane and the caret are the same wherever the `/error` line is submitted from
    /// (§4.4), which is exactly why the bare `e` mnemonic was removed instead of
    /// re-scoped.
    ///
    /// The §8.5 buffer comes first, because it is the report as it was made: the
    /// categorical cause plus the **whole** text, multi-line included. `status_line`
    /// keeps only the first line of that traffic (§7.4) and the transcript can be
    /// cleared (`/reset`, a restore, a pruning), so a transcript-only scan loses the
    /// cause exactly when the operator needs it — which is why the scan below is the
    /// fallback, not the source. Saying so out loud when the buffer is empty is what
    /// keeps `/error` from ever being a silent command (G5).
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
        let before = self.input_text.clone();
        // A single insert_str splices the block in one edit: one undo record and one
        // line rewrite, so a very large paste cannot trigger a per-char re-render storm.
        self.textarea.insert_str(body);
        self.sync_input_from_textarea();
        self.end_history_walk_if_edited(&before);
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

    /// Install a **recalled history entry** in the composer.
    ///
    /// This is [`Self::set_input_content`] with the caret parked at the end of the
    /// recalled entry's *first* line instead of at `CursorMove::End`. For the
    /// single-line case the two are the same position, so a recalled command still
    /// lands ready to re-submit. It matters for a multi-line entry: the caret is kept
    /// off the entry's last line, so an arrow spent inside the recalled text answers
    /// for the entry's first line rather than for a line the operator never typed.
    /// The walk itself is unaffected either way — `Ctrl+Up`/`Ctrl+Down` short-circuit
    /// ahead of any caret question (§5), so a recalled entry can never be
    /// half-walked by an arrow, which is what the plain-key history fallback used to
    /// make possible.
    fn recall_input_content(&mut self, text: &str) {
        self.set_input_content(text);
        let Some(first_line) = self.textarea.lines().first() else {
            return;
        };
        if self.textarea.lines().len() < 2 {
            return;
        }
        let first_line = first_line.clone();
        let column = first_line.chars().count();
        self.textarea
            .move_cursor(ratatui_textarea::CursorMove::Jump(0, column as u16));
        self.cursor = first_line.len();
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

    /// Clamp every scroll offset into the range its pane can draw.
    ///
    /// The three ranges are the ones the panes published, not measurements taken
    /// here: the transcript and the worker detail go through
    /// [`Self::transcript_tail`] / [`Self::worker_detail_tail`], which read the
    /// geometry those panes measured and wrote back on the frame
    /// (`draw_new_ui`), and the plan reads `plan_max_scroll`, which the plan pane
    /// publishes the same way. So a resize clamp, a scroll key and a draw can never
    /// land on three different offsets (audit defects 11/12; the pin pass in
    /// `render.rs` used to run the same arithmetic a third time, from an estimate).
    pub(crate) fn clamp_scrolls(&mut self) {
        self.chat_scroll = self.chat_scroll.min(self.transcript_tail());

        self.plan_scroll = self.plan_scroll.min(self.plan_max_scroll);

        self.subagent_scroll = self.subagent_scroll.min(self.worker_detail_tail());

        // The roster's clamp is deliberately the coarse one it has always had: this
        // pass only knows the pane rect, not the split the pane is going to choose.
        // The authoritative range is the pane's (`components::activity::roster_window`),
        // applied when the window is drawn and reconciled against on the next `↑`/`↓`
        // (`events.rs::scroll_worker_roster_by`, defect 14) — so a resize can leave a
        // stale offset stored for one frame, never one drawn or walked.
        let list_height = self.subagent_area.height.saturating_sub(3).max(1) as usize;
        let max_list = self.subagents.len().saturating_sub(list_height);
        self.subagent_list_scroll = self.subagent_list_scroll.min(max_list as u16);
    }

    /// Move the transcript viewport to `target` and state **only** the follow
    /// decision the move actually expresses (defect 9).
    ///
    /// `toward_tail` says which way the gesture was pulling. The rule, which is the
    /// whole of G7 as spec §10 item 14 states it:
    ///
    /// * an **away** gesture that moved the viewport turns follow off;
    /// * a **toward** gesture turns it back on only when it *arrived* on the tail
    ///   (and the pane has a tail to arrive on, i.e. `tail > 0`);
    /// * a press that lands where the viewport already stands states nothing at all:
    ///   `follow`, its legacy mirror, the `● LIVE` / `↓ N new` chip and the dirty
    ///   flag are all left exactly as they were. That is what stops a transcript
    ///   that fits its pane — `max_scroll == 0`, the case with no tail to leave —
    ///   from toggling its own chip on every single arrow press.
    ///
    /// The transcript arm never assigns the legacy `chat_auto_scroll` mirror: that
    /// flag is a projection of `follow` and moves only through
    /// [`Self::set_follow`].
    ///
    /// Returns whether the viewport moved, which is what tells the caller whether
    /// there is anything at all to repaint.
    fn scroll_transcript_to(&mut self, target: u16, toward_tail: bool) -> bool {
        let tail = self.transcript_tail();
        let stored = self.chat_scroll;
        // Where the viewport genuinely stands, which is not always what the field
        // says: with follow on the transcript pane pins itself to the tail it measured
        // (`components/transcript.rs`), so the tail is what a press compares against
        // even when no frame has written `chat_scroll` back yet.
        let current = if self.follow { tail } else { stored };
        if target == current {
            // The viewport is already where the press asked for, so there is no
            // movement and no decision: `follow`, its mirror, the chip and the
            // dirty flag all stay as they are. The one piece of bookkeeping still
            // worth doing is a stale offset — while the pane is pinned the field can
            // lag behind what is drawn. Reconciling it changes no pixel (the pin
            // ignores it), states nothing, and is what lets the next press measure
            // its step from the position the operator is really looking at.
            if target != stored {
                self.chat_scroll = target;
            }
            return false;
        }
        self.chat_scroll = target;
        if toward_tail {
            if tail > 0 && target >= tail {
                // Only an arrival re-arms: this is the explicit scroll-to-end
                // gesture §10 item 14 allows. A press that moved the viewport
                // without getting it there states no decision.
                self.set_follow(true);
            }
        } else {
            self.set_follow(false);
        }
        true
    }

    /// Put the plan viewport where the operator asked for it.
    ///
    /// `plan_auto_scroll` does **not** mean "the viewport sits at the tail". It
    /// means "centre the pane on the task in flight": `components/plan_pane.rs`
    /// answers it with an offset computed from the active task, never from what the
    /// operator asked for. So no scroll gesture ever writes it **on** (defect 13):
    /// an operator who pages or jumps to the tail asked for the tail, and handing
    /// the viewport back to the in-flight task on the next frame is exactly the
    /// follow-then-jump flap this closes.
    ///
    /// What a gesture does write is the opposite intent, and only once it has
    /// actually taken the viewport over: `false`, meaning "this offset is the
    /// operator's, stop centring". The release is still stated when the offset
    /// itself did not change but centring was live and the pane is scrollable —
    /// releasing it visibly moves the body back to the operator's offset. With
    /// nothing to scroll (`plan_max_scroll == 0`) a press has nowhere to go and
    /// changes neither the offset nor the intent.
    ///
    /// Returns whether anything the operator can see changed.
    fn scroll_plan_to(&mut self, target: u16) -> bool {
        if target == self.plan_scroll {
            if self.plan_auto_scroll && self.plan_max_scroll > 0 {
                self.plan_auto_scroll = false;
                return true;
            }
            return false;
        }
        self.plan_scroll = target;
        self.plan_auto_scroll = false;
        true
    }

    /// Move the worker-detail viewport and state the `subagent_autoscroll`
    /// decision the move expresses, under the same no-movement-no-decision rule as
    /// the transcript (defects 9 and 15): the flag is turned off by a gesture that
    /// moved the detail off its tail and back on only by one that arrived on it, so
    /// it can never flap on a press with nowhere to go.
    ///
    /// # The Workers pane: two viewports, two owners
    ///
    /// The pane shows a roster of workers **and** the selected worker's detail log,
    /// so its two offsets are addressed by two disjoint sets of keys, and no key
    /// answers to both:
    ///
    /// | key | viewport it owns | offset it writes |
    /// |---|---|---|
    /// | `Up` / `Down` | the roster window | `subagent_list_scroll` |
    /// | `PageUp` / `PageDown` | the selected worker's detail log | `subagent_scroll` |
    /// | `Home` / `End` | the detail log, top / tail | `subagent_scroll` |
    /// | `Left` / `Right` | the roster selection (the window reveals the new row) | `selected_subagent_idx` |
    ///
    /// So the detail functions here never touch the roster offset, and
    /// [`Self::scroll_worker_roster_by`] never touches the detail offset or this
    /// flag. (Defect 14 was exactly this hole: the roster offset had no key owner at
    /// all, only a resize clamp, while the pane advertised `↑`/`↓` as its scroll
    /// keys.)
    fn scroll_worker_detail_to(&mut self, target: u16, toward_tail: bool) -> bool {
        let tail = self.worker_detail_tail();
        let stored = self.subagent_scroll;
        let current = if self.subagent_autoscroll {
            tail
        } else {
            stored
        };
        if target == current {
            // Same no-movement-no-decision rule, with the same stale-offset
            // reconciliation as the transcript arm: while the flag is on the pane
            // pins this offset to the tail it measured, so the stored field can lag
            // behind what was drawn.
            if target != stored {
                self.subagent_scroll = target;
            }
            return false;
        }
        self.subagent_scroll = target;
        if toward_tail {
            if tail > 0 && target >= tail {
                self.subagent_autoscroll = true;
            }
        } else {
            self.subagent_autoscroll = false;
        }
        true
    }

    /// The roster window's drawable bounds, asked of the pass that draws it.
    ///
    /// The key path deliberately does not estimate the roster's height: the pane
    /// owns that geometry (`components::activity::roster_window`), so the press is
    /// clamped against the rect the last frame actually published
    /// (`subagent_area`) with the same §2.5 detail rule the frame applies. A press
    /// can therefore never claim a window position the pane is going to refuse, and
    /// the two ends of the range are the two ends the pane's own scrollbar draws.
    ///
    /// `(0, 0)` — no rows, or a Workers rect no frame has published yet — means
    /// there is no window to move.
    fn worker_roster_bounds(&self) -> (usize, usize) {
        let pane = self.subagent_area;
        let selected = self
            .selected_subagent_idx
            .min(self.subagents.len().saturating_sub(1));
        activity::roster_window(
            pane,
            super::layout::workers_detail_present(pane.width),
            self.subagents.len(),
            selected,
        )
    }

    /// `Up` / `Down` with the Workers pane focused: walk the **roster window** by
    /// `amount` rows, and nothing else (defect 14).
    ///
    /// The window is the outer viewport of the Workers pane — the list of workers —
    /// and this is its only key owner. It is a window, not a log: the selection
    /// stays where it is (`←`/`→` move that, and reveal the row), the detail half
    /// keeps its own offset, and **no follow-ish flag is written at all**, because
    /// moving a list window says nothing about following the tail of a log. That is
    /// the roster half of the no-movement-no-decision rule: at either end of the
    /// window — including a roster that fits its half, where the bounds collapse to
    /// a single position — the offset does not move, nothing is stated, and no frame
    /// is asked for.
    ///
    /// Direction follows the other two viewports, which all count rows hidden from
    /// the top: `up` reveals the **earlier** rows (offset toward `lower`), `down` the
    /// later ones. A roster, unlike a log, has both ends worth walking.
    ///
    /// Returns whether the window moved.
    fn scroll_worker_roster_by(&mut self, up: bool, amount: u16) -> bool {
        let (lower, upper) = self.worker_roster_bounds();
        let current = usize::from(self.subagent_list_scroll).clamp(lower, upper);
        let target = if up {
            current.saturating_sub(usize::from(amount)).max(lower)
        } else {
            current.saturating_add(usize::from(amount)).min(upper)
        };
        if target == current {
            // Already at the end the press asked for. The only bookkeeping still
            // worth doing is a stale offset — the pane's own clamp can land the
            // drawn window somewhere the stored value does not say — so it is
            // reconciled here and nothing else is touched, exactly as the transcript
            // and detail arms reconcile theirs.
            if usize::from(self.subagent_list_scroll) != current {
                self.subagent_list_scroll = current as u16;
            }
            return false;
        }
        self.subagent_list_scroll = target as u16;
        true
    }

    /// Scroll the focused pane one step up (G7). For the transcript and the worker
    /// detail the gesture also states the follow decision it expresses (spec §10
    /// item 14): a press that pulled the viewport off the tail turns the follow-ish
    /// flag **off**, and a press with nowhere to go states nothing.
    ///
    /// The target is the real [`FocusPane`], never the legacy `focused_panel` fold.
    /// The fold spells `Input` and `Transcript` with one `Chat` token, so resolving
    /// the target from it moved the transcript viewport while the composer held
    /// focus — a scroll of a pane the operator was not looking at. The composer owns
    /// no viewport, so it names no scroll target and returns `false`.
    ///
    /// For the Workers pane the target is the **detail** half. Plain `Up` / `Down` no
    /// longer reach this function there — they own the roster window
    /// ([`Self::scroll_worker_roster_by`]) — so what arrives here is the
    /// `PageUp` / `PageDown` page gesture.
    pub(crate) fn scroll_up(&mut self, amount: u16) -> bool {
        match self.focus {
            FocusPane::Input => false,
            FocusPane::Transcript => {
                let target = self.chat_scroll.saturating_sub(amount);
                self.scroll_transcript_to(target, false)
            }
            FocusPane::Plan => {
                let target = self.plan_scroll.saturating_sub(amount);
                self.scroll_plan_to(target)
            }
            FocusPane::Workers => {
                let target = self.subagent_scroll.saturating_sub(amount);
                self.scroll_worker_detail_to(target, false)
            }
        }
    }

    /// Scroll the focused pane one step down. Re-arming follow is reserved for the
    /// press that actually **arrived** on the tail; a partial press toward it moves
    /// the viewport and states no decision, and a press already sitting on the tail
    /// moves nothing and states nothing. The plan arm never re-arms centring at all
    /// (defect 13, [`Self::scroll_plan_to`]).
    pub(crate) fn scroll_down(&mut self, amount: u16) -> bool {
        match self.focus {
            FocusPane::Input => false,
            FocusPane::Transcript => {
                let tail = self.transcript_tail();
                let target = self.chat_scroll.saturating_add(amount).min(tail);
                self.scroll_transcript_to(target, true)
            }
            FocusPane::Plan => {
                let target = self
                    .plan_scroll
                    .saturating_add(amount)
                    .min(self.plan_max_scroll);
                self.scroll_plan_to(target)
            }
            FocusPane::Workers => {
                let tail = self.worker_detail_tail();
                let target = self.subagent_scroll.saturating_add(amount).min(tail);
                self.scroll_worker_detail_to(target, true)
            }
        }
    }

    /// Jump one pane's viewport to its top. For the transcript this is an explicit
    /// gesture away from the tail, so it is stated as the follow decision it is
    /// (G7, item 14) rather than as a write of the legacy mirror — but only when
    /// the viewport was anywhere other than the top to begin with.
    pub(crate) fn scroll_to_top(&mut self) -> bool {
        match self.focus {
            FocusPane::Input => false,
            FocusPane::Transcript => self.scroll_transcript_to(0, false),
            FocusPane::Plan => self.scroll_plan_to(0),
            FocusPane::Workers => self.scroll_worker_detail_to(0, false),
        }
    }

    /// Jump one pane's viewport to its bottom — for the transcript the explicit
    /// "jump to bottom" gesture that §10 item 14 allows to turn follow back on,
    /// stated through [`Self::set_follow`] and never by assigning the mirror. For
    /// the plan it is the tail **as an offset**: the jump does not hand the pane
    /// back to the in-flight task (defect 13).
    pub(crate) fn scroll_to_bottom(&mut self) -> bool {
        match self.focus {
            FocusPane::Input => false,
            FocusPane::Transcript => {
                let tail = self.transcript_tail();
                self.scroll_transcript_to(tail, true)
            }
            FocusPane::Plan => {
                let target = self.plan_max_scroll;
                self.scroll_plan_to(target)
            }
            FocusPane::Workers => {
                let tail = self.worker_detail_tail();
                self.scroll_worker_detail_to(tail, true)
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

    /// Walk backward through the input history — the binding is the `Ctrl+Up` chord
    /// and nothing else, in every focus (the plain arrows never reach this: see
    /// [`Self::scroll_or_history`]). Returns whether the draft actually changed,
    /// which is what lets the caller treat an empty history as an unclaimed chord
    /// instead of a swallowed one.
    ///
    /// The walk is bookkeeping of the **composer**: `history_index` and
    /// `input_draft` only ever describe what stands in the composer, and only the
    /// composer's keys may move them.
    pub(crate) fn history_prev(&mut self) -> bool {
        let last = match self.history.len() {
            0 => return false,
            len => len - 1,
        };
        let step = match self.history_index {
            None => Some(last),
            Some(index) if index > 0 => Some(index - 1),
            // Already at the oldest entry. Restating the same entry used to rebuild
            // the textarea — resetting a caret the operator had moved — and to look
            // like the walk was still going.
            Some(_) => None,
        };
        let Some(index) = step else {
            return false;
        };
        if self.history_index.is_none() {
            self.input_draft = self.input_text.clone();
        }
        self.history_index = Some(index);
        let text = self.history[index].clone();
        self.recall_input_content(&text);
        true
    }

    /// Walk forward through the input history — the `Ctrl+Down` chord and nothing
    /// else, in every focus. Returns whether the draft changed: with no walk in
    /// progress there is nothing to walk forward to, and the chord is then not a key
    /// press at all rather than a claimed no-op.
    pub(crate) fn history_next(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            let text = self.history[index + 1].clone();
            self.recall_input_content(&text);
        } else {
            self.history_index = None;
            let text = self.input_draft.clone();
            self.recall_input_content(&text);
        }
        true
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
            // No pin here. This arm's job is to fold the streaming buffers into the
            // transcript; keeping the viewport on the newest line while `follow` is
            // on is the transcript pane's decision, taken once per frame against the
            // body it counted itself. Estimating the tail from a key path is what let
            // a frame disagree with a scroll key about the geometry (audit defect 11).
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

    /// `/error` is the **whole** route to the recall now that the bare `e` mnemonic is
    /// gone: the same line, the same notice, from the composer focus and from a pane
    /// focus, with no confirm and nothing sent. The letter itself only ever types.
    #[test]
    fn the_error_command_recalls_the_sticky_error_in_every_focus() {
        let planted = vec![
            "User: rebuild the TUI".to_string(),
            "LLM error: http — 503 upstream".to_string(),
        ];
        let mut from_composer = TuiRenderer::new();
        let mut from_pane = TuiRenderer::new();
        from_composer.messages = planted.clone();
        from_pane.messages = planted.clone();
        from_composer.set_focus(state::FocusPane::Input);
        from_pane.set_focus(state::FocusPane::Transcript);

        submit(&mut from_composer, "/error");
        submit(&mut from_pane, "/error");

        assert_eq!(
            from_composer.messages, from_pane.messages,
            "`/error` recalled different traffic per focus"
        );
        let recalled = from_composer.messages.last().cloned().unwrap_or_default();
        assert!(recalled.starts_with('✗'), "{recalled}");
        assert!(recalled.contains("marmel.log"), "{recalled}");
        assert!(
            from_composer.confirm_action.is_none(),
            "the recall is a notice, never a confirm"
        );
        assert!(drain_submissions(&mut from_composer).is_empty());

        // The letter that used to carry the recall is a letter everywhere — over a
        // pane focus included, which is where it used to be spent (§4.4).
        let mut typed = TuiRenderer::new();
        typed.messages = planted;
        typed.set_focus(state::FocusPane::Transcript);
        assert!(press(&mut typed, KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(typed.input_text, "e", "the bare letter reaches the draft");
        assert_eq!(
            typed.messages.len(),
            2,
            "and recalled nothing: {:?}",
            typed.messages
        );
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
    ///
    /// The second half of the rule changed owner with the scroll-geometry
    /// unification (audit defects 11/12): while follow is on, arriving output does
    /// NOT get pinned to a tail estimated here. The key path states the decision and
    /// leaves the offset alone; the frame that draws the transcript pins the body to
    /// the tail it counted, and that is what the pins
    /// [`crate::ui::tui::tests::a_followed_transcript_shows_the_last_lines_of_a_long_answer`]
    /// and
    /// `tests_new_ui::a_submit_while_following_still_pins_the_transcript_to_the_bottom_item14`
    /// hold at the seam where the pin actually happens. What is pinned here is the
    /// half this file owns: the decision, and the fact that no key path writes the
    /// viewport out from under the pane.
    #[test]
    fn output_that_arrives_never_arms_follow_and_never_moves_the_viewport_itself() {
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

        // Follow on: the same output keeps the decision on and its mirror with it,
        // and still moves nothing from here. The transcript pane is handed
        // `follow` and pins itself against its own measurement; an estimate written
        // from the event path is exactly the second geometry that used to disagree
        // with the frame below it.
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
        assert_eq!(
            follow_pair(&renderer),
            (true, true),
            "the committed turn restated the follow decision"
        );
        assert_eq!(
            renderer.chat_scroll, 0,
            "and the event path wrote no offset"
        );
        let view = state::UiView::from_renderer(&renderer);
        assert!(view.follow, "the frame is handed the follow request");
        let tail = renderer.transcript_tail();
        assert!(tail > 0, "the fixture has scrollback to pin against");
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

    // ---- the key x focus x draft ownership rule -----------------------------
    //
    // The reported defect is that scrolling the panes and walking the composer's
    // input history were the same key, decided by the shape of the draft. The rule
    // these guards hold: the composer owns `Up`/`Down`/`PageUp`/`PageDown`/`Home`/
    // `End` **iff** `FocusPane::Input` holds focus (one predicate,
    // [`FocusPane::composer_has_focus`]), and a pane owns them otherwise. Every one
    // of them drives the public seam the live loop calls.

    /// Defects 1, 2, 5 and 7 plus the reported scroll conflict: the composer's keys
    /// never move a pane viewport or state a follow decision, a pane's keys never
    /// touch the composer — not its draft, not its caret, not its history
    /// bookkeeping — and a plain arrow is never a history recall in either focus.
    #[test]
    fn up_and_down_follow_the_focus_and_never_cross_the_line() {
        // Composer focused with a scrollback transcript waiting and a history to
        // walk: `Up` recalls **nothing**. The composer owns the key, there is no
        // caret movement to make, and the walk is reachable by the chord alone, so
        // the press is an explicit no-op — no recall, no scroll, no repaint.
        let mut renderer = transcript_with_scrollback();
        renderer.set_focus(FocusPane::Input);
        renderer.chat_width.set(80);
        renderer.chat_height.set(10);
        renderer.history = vec!["earlier note".to_string()];
        renderer.set_input_content("");
        // Park the transcript's viewport somewhere in the middle of its scrollback so
        // "no pane viewport moved" is an offset claim, not a tautology.
        renderer.chat_scroll = 12;
        let offset = renderer.chat_scroll;
        let follow = follow_pair(&renderer);
        renderer.dirty = false;
        assert!(
            !press(&mut renderer, KeyCode::Up, KeyModifiers::NONE),
            "a plain `Up` behind the composer is not claimed once the history \
             fallback is gone"
        );
        assert!(
            renderer.input_text.is_empty(),
            "the draft was not replaced by a history entry: {:?}",
            renderer.input_text
        );
        assert_eq!(
            renderer.history_index, None,
            "and no walk was started behind the operator's back"
        );
        assert!(!renderer.dirty, "an unclaimed press asked for no repaint");
        assert_eq!(renderer.chat_scroll, offset, "no pane viewport moved");
        assert_eq!(
            follow_pair(&renderer),
            follow,
            "and no follow decision was stated for a gesture the operator never made"
        );
        // The same arrow one focus over *does* own the key and steps the pane — which
        // is the whole point of the operator's report.
        renderer.set_focus(FocusPane::Transcript);
        assert!(
            press(&mut renderer, KeyCode::Up, KeyModifiers::NONE),
            "the transcript took the same key"
        );
        assert!(renderer.chat_scroll < offset, "and it scrolled");
        assert!(
            renderer.input_text.is_empty() && renderer.history_index.is_none(),
            "while the composer kept its empty draft and its unbegun walk"
        );
        // The chord reaches the walk from either focus.
        assert!(
            press(&mut renderer, KeyCode::Up, KeyModifiers::CONTROL),
            "`Ctrl+Up` is the history binding over a pane focus too"
        );
        assert_eq!(renderer.input_text, "earlier note");
        assert_eq!(renderer.history_index, Some(0));

        // Transcript focused over a multi-line draft: the draft keeps its text, its
        // caret and its walk bookkeeping, whatever `Up` does to the viewport.
        let mut renderer = transcript_with_scrollback();
        renderer.chat_width.set(80);
        renderer.chat_height.set(10);
        renderer.set_input_content("line one\nline two");
        let draft = renderer.input_text.clone();
        let caret = renderer.textarea.cursor();
        let index = renderer.history_index;
        assert!(
            press(&mut renderer, KeyCode::Up, KeyModifiers::NONE),
            "the pane took the key"
        );
        assert_eq!(
            renderer.input_text, draft,
            "the unfocused draft was not edited"
        );
        assert_eq!(
            renderer.textarea.cursor(),
            caret,
            "and the unfocused caret did not move"
        );
        assert_eq!(renderer.history_index, index, "nor did the history walk");
        assert!(renderer.history_index.is_none());
    }

    /// Defect 3: `Down` used to be swallowed by the history bookkeeping whenever
    /// the draft was empty, so the transcript could not be scrolled down at all in
    /// the default state. The focused pane scrolls instead.
    #[test]
    fn down_with_a_pane_in_focus_and_an_empty_draft_scrolls_the_pane() {
        let mut renderer = transcript_with_scrollback();
        renderer.chat_width.set(80);
        renderer.chat_height.set(10);
        assert!(
            press(&mut renderer, KeyCode::End, KeyModifiers::NONE),
            "`End` jumps a pane focus to the tail"
        );
        let tail = renderer.chat_scroll;
        assert!(tail > 0, "the fixture has scrollback to move in");
        assert!(press(&mut renderer, KeyCode::Up, KeyModifiers::NONE));
        let above = renderer.chat_scroll;
        assert!(above < tail, "`Up` pulled the viewport off the tail");

        renderer.set_input_content("");
        assert!(
            press(&mut renderer, KeyCode::Down, KeyModifiers::NONE),
            "`Down` with an empty draft is a pane scroll, not a swallowed no-op"
        );
        assert_eq!(renderer.chat_scroll, above + 1, "and it moved the viewport");
    }

    /// Defect 4, in its new and stronger form: with the composer focused and nothing
    /// inside the composer for the arrow to do, the press is an explicit no-op **even
    /// when a history is waiting to be walked**. It does not claim the event, so
    /// nothing repaints and no `follow` / scroll state is left describing a movement
    /// that never happened.
    #[test]
    fn a_composer_press_with_nothing_to_walk_is_not_claimed() {
        let mut renderer = transcript_with_scrollback();
        renderer.set_focus(FocusPane::Input);
        renderer.history.clear();
        renderer.set_input_content("");
        let follow = follow_pair(&renderer);

        assert!(
            !press(&mut renderer, KeyCode::Up, KeyModifiers::NONE),
            "an empty history is not a key press"
        );
        assert!(renderer.input_text.is_empty(), "and it restated nothing");
        assert_eq!(follow_pair(&renderer), follow, "and it stated no decision");

        assert!(
            !press(&mut renderer, KeyCode::Down, KeyModifiers::NONE),
            "nor is `Down` with no walk in progress"
        );
        assert!(renderer.history_index.is_none());

        // The walk is no longer what rescues the key: a **full** history behind an
        // empty single-line draft still leaves the plain arrows unclaimed. This is
        // the reported conflict — the operator's arrow was recalling history while
        // they were trying to scroll.
        let mut renderer = transcript_with_scrollback();
        renderer.set_focus(FocusPane::Input);
        renderer.chat_scroll = 12;
        renderer.history = vec!["older entry".to_string(), "newest entry".to_string()];
        renderer.set_input_content("");
        let before = (
            renderer.input_text.clone(),
            renderer.history_index,
            renderer.chat_scroll,
            follow_pair(&renderer),
        );
        renderer.dirty = false;
        assert!(
            !press(&mut renderer, KeyCode::Up, KeyModifiers::NONE),
            "a waiting history does not make a plain composer `Up` a key press"
        );
        assert!(
            !press(&mut renderer, KeyCode::Down, KeyModifiers::NONE),
            "nor does it make a plain composer `Down` one"
        );
        assert_eq!(
            renderer.input_text, before.0,
            "the draft stayed the operator's"
        );
        assert_eq!(
            renderer.history_index, before.1,
            "and the walk never started"
        );
        assert_eq!(renderer.chat_scroll, before.2, "and no viewport moved");
        assert_eq!(
            follow_pair(&renderer),
            before.3,
            "and no decision was stated"
        );
        assert!(!renderer.dirty, "and nothing was scheduled for repaint");

        // A multi-line draft with the caret against its edge has nowhere to go
        // either, and no draft shape turns a plain arrow into a walk.
        let mut renderer = transcript_with_scrollback();
        renderer.set_focus(FocusPane::Input);
        renderer.history = vec!["first\nsecond".to_string()];
        renderer.set_input_content("keep\nthis");
        let before = (renderer.input_text.clone(), renderer.history_index);
        assert!(
            !press(&mut renderer, KeyCode::Up, KeyModifiers::NONE),
            "the caret is already on the first line"
        );
        assert_eq!(renderer.input_text, before.0, "and the draft stayed put");
        assert_eq!(renderer.history_index, before.1);
    }

    /// Defects 6 and 8: the walk is driven by the explicit `Ctrl+Up`/`Ctrl+Down`
    /// binding, so a recalled multi-line entry cannot abort it with a caret, and a
    /// hand edit ends the browse so a stale walk can never overwrite the edit.
    #[test]
    fn a_history_walk_survives_a_multiline_entry_and_an_edit_ends_it() {
        let mut renderer = transcript_with_scrollback();
        renderer.set_focus(FocusPane::Input);
        renderer.history = vec!["first entry".to_string(), "line one\nline two".to_string()];

        assert!(
            press(&mut renderer, KeyCode::Up, KeyModifiers::CONTROL),
            "`Ctrl+Up` is the explicit history binding"
        );
        assert_eq!(renderer.input_text, "line one\nline two");
        assert_eq!(renderer.history_index, Some(1));
        assert_eq!(
            renderer.textarea.cursor(),
            (0, "line one".chars().count()),
            "a multi-line recall parks the caret on its first line, where the next \
             `Up` cannot turn it into a caret move"
        );

        assert!(press(&mut renderer, KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(
            renderer.input_text, "first entry",
            "and the walk kept walking through the multi-line entry"
        );
        assert_eq!(renderer.history_index, Some(0));

        // At the oldest entry the walk is over: pressing again restates nothing and
        // leaves the caret alone.
        let caret = renderer.textarea.cursor();
        assert!(
            !press(&mut renderer, KeyCode::Up, KeyModifiers::CONTROL),
            "walking past the oldest entry is not a key press"
        );
        assert_eq!(renderer.textarea.cursor(), caret, "and it reset no caret");

        // A hand edit ends the browse.
        assert!(press(&mut renderer, KeyCode::Char('!'), KeyModifiers::NONE));
        assert_eq!(renderer.input_text, "first entry!");
        assert_eq!(
            renderer.history_index, None,
            "editing a recalled entry ends the walk"
        );
        assert!(
            !press(&mut renderer, KeyCode::Down, KeyModifiers::CONTROL),
            "and there is no stale walk left to walk forward on"
        );
        assert_eq!(
            renderer.input_text, "first entry!",
            "so the edit is never overwritten by an old entry"
        );
    }

    /// Defect 10 (consistency): `PageUp`/`PageDown` answer the same focus question
    /// as `Home`/`End`. The composer has no viewport, so with the composer focused
    /// they are an unclaimed no-op; with a pane focused they page that pane.
    #[test]
    fn page_keys_are_pane_keys() {
        let mut renderer = transcript_with_scrollback();
        renderer.set_focus(FocusPane::Input);
        renderer.chat_width.set(80);
        renderer.chat_height.set(10);
        renderer.set_input_content("a draft");
        let offset = renderer.chat_scroll;
        let follow = follow_pair(&renderer);
        for code in [KeyCode::PageUp, KeyCode::PageDown] {
            assert!(
                !press(&mut renderer, code, KeyModifiers::NONE),
                "the composer holds focus, so {code:?} has no viewport to page"
            );
            assert_eq!(renderer.chat_scroll, offset, "{code:?} moved a viewport");
            assert_eq!(follow_pair(&renderer), follow, "{code:?} stated a decision");
        }

        renderer.set_focus(FocusPane::Transcript);
        assert!(press(&mut renderer, KeyCode::End, KeyModifiers::NONE));
        let tail = renderer.chat_scroll;
        assert!(tail > 0, "the fixture has scrollback to page");
        assert!(press(&mut renderer, KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(
            renderer.chat_scroll,
            tail.saturating_sub(10),
            "the focused transcript pages by ten"
        );
        assert!(press(&mut renderer, KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(renderer.chat_scroll, tail, "and back to the tail");
    }

    // ---- the follow / auto-scroll contract: a decision only for a movement ----
    //
    // The other half of the reported overlap: every `Up`/`Down`/`PageUp`/`PageDown`/
    // `Home`/`End` press used to restate the follow flags whether or not the
    // viewport could move, so a pane with nothing to scroll flipped `● LIVE` into
    // `↓ follow off` and back on every press (defect 9), and a plan scrolled to its
    // tail was handed back to the in-flight task (defect 13). The contract these
    // guards pin:
    //
    // | press                              | viewport | follow-ish flag |
    // |------------------------------------|----------|-----------------|
    // | moved away from the tail           | moves    | off             |
    // | moved and arrived on the tail      | moves    | on              |
    // | moved, still short of the tail     | moves    | unchanged       |
    // | nothing to scroll / already there  | still    | unchanged       |

    /// The tail the key path measures, spelled out so a fixture says what it means.
    /// It is the pane's own published maximum once a frame has drawn the transcript,
    /// and the renderer's line estimate before then — [`TuiRenderer::transcript_tail`]
    /// is the one place that chooses between the two.
    fn transcript_tail_of(renderer: &TuiRenderer) -> u16 {
        renderer.transcript_tail()
    }

    /// Defect 9, the visible half: over a transcript that fits its pane there is no
    /// tail to leave, so no scroll key may state a follow decision at all. The chip
    /// keeps saying `● LIVE`, the mirror keeps agreeing, and the frame is not even
    /// marked dirty because there is nothing to repaint.
    #[test]
    fn a_pane_with_nothing_to_scroll_states_no_follow_decision() {
        let mut renderer = transcript_with_scrollback();
        // Wide and tall: every line of the fixture is on screen, `max_scroll == 0`.
        renderer.chat_width.set(400);
        renderer.chat_height.set(400);
        assert_eq!(
            transcript_tail_of(&renderer),
            0,
            "the fixture must have nothing to scroll"
        );

        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ] {
            renderer.dirty = false;
            assert!(
                press(&mut renderer, code, KeyModifiers::NONE),
                "the transcript holds focus, so {code:?} is its key"
            );
            assert_eq!(
                renderer.chat_scroll, 0,
                "{code:?} moved an immovable offset"
            );
            assert_eq!(
                follow_pair(&renderer),
                (true, true),
                "{code:?} broke follow on a pane with nothing to scroll"
            );
            assert!(
                !renderer.dirty,
                "{code:?} asked for a repaint of a viewport that did not move"
            );
        }
    }

    /// Defect 9, the other half: with a pane that *can* scroll, a press whose
    /// offset already sits at the requested extreme moves nothing and therefore
    /// states nothing. Re-arming follow stays reserved for `Ctrl+F`, `/follow on`,
    /// and the press that actually travels onto the tail.
    #[test]
    fn a_press_already_at_the_requested_extreme_states_nothing() {
        let mut renderer = transcript_with_scrollback();
        renderer.chat_width.set(80);
        renderer.chat_height.set(10);
        let tail = transcript_tail_of(&renderer);
        assert!(tail > 10, "the fixture needs room to be scrolled: {tail}");

        // Sitting on the top with follow off: the away gestures have nowhere to go.
        renderer.set_follow(false);
        renderer.chat_scroll = 0;
        for code in [KeyCode::Up, KeyCode::PageUp, KeyCode::Home] {
            renderer.dirty = false;
            assert!(press(&mut renderer, code, KeyModifiers::NONE));
            assert_eq!(renderer.chat_scroll, 0, "{code:?} moved the offset");
            assert_eq!(
                follow_pair(&renderer),
                (false, false),
                "{code:?} restated a follow decision for no movement"
            );
            assert!(!renderer.dirty, "{code:?} repainted a still viewport");
        }

        // Sitting on the tail with follow off: the toward gestures move nothing, so
        // they do not arm follow either.
        renderer.chat_scroll = tail;
        for code in [KeyCode::Down, KeyCode::PageDown, KeyCode::End] {
            renderer.dirty = false;
            assert!(press(&mut renderer, code, KeyModifiers::NONE));
            assert_eq!(renderer.chat_scroll, tail, "{code:?} moved the offset");
            assert_eq!(
                follow_pair(&renderer),
                (false, false),
                "{code:?} armed follow without moving the viewport"
            );
            assert!(!renderer.dirty, "{code:?} repainted a still viewport");
        }

        // The press that does move states its decision, in both directions.
        assert!(press(&mut renderer, KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(renderer.chat_scroll, tail - 1, "`Up` moved one line");
        assert!(
            !renderer.follow,
            "and an away press states the decision it is"
        );
    }

    /// The arrival rule: a press that travels toward the tail without getting there
    /// moves the viewport and states no decision; the one that lands on the tail is
    /// what re-arms follow, and only when there is a tail to land on.
    #[test]
    fn only_the_press_that_arrives_at_the_tail_re_arms_follow() {
        let mut renderer = transcript_with_scrollback();
        renderer.chat_width.set(80);
        renderer.chat_height.set(10);
        let tail = transcript_tail_of(&renderer);
        assert!(tail > 10, "the fixture needs room to be scrolled: {tail}");

        renderer.set_follow(false);
        renderer.chat_scroll = 0;

        // Half-way down: a real move, no decision.
        assert!(press(&mut renderer, KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(renderer.chat_scroll, 10, "the page moved the viewport");
        assert_eq!(
            follow_pair(&renderer),
            (false, false),
            "a press short of the tail armed follow"
        );

        // One line short, then the step that arrives.
        renderer.chat_scroll = tail - 1;
        assert!(press(&mut renderer, KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            renderer.chat_scroll, tail,
            "the viewport arrived on the tail"
        );
        assert_eq!(
            follow_pair(&renderer),
            (true, true),
            "the arrival re-armed follow and its mirror"
        );

        // And the away press from the tail breaks it again.
        assert!(press(&mut renderer, KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(renderer.chat_scroll, tail.saturating_sub(10));
        assert_eq!(
            follow_pair(&renderer),
            (false, false),
            "leaving the tail broke follow"
        );
    }

    /// Defect 13: `plan_auto_scroll` means "centre the pane on the task in flight",
    /// so a manual gesture to the plan tail must never write it. The tail is an
    /// offset the operator asked for, and the next frame must not trade it for the
    /// in-flight task.
    #[test]
    fn a_manual_plan_gesture_never_hands_the_viewport_back_to_the_active_task() {
        let mut renderer = TuiRenderer::new();
        renderer.set_focus(FocusPane::Plan);
        renderer.plan_max_scroll = 40;
        renderer.plan_scroll = 12;
        // What launching a task leaves behind (`on_event` arms the centring).
        renderer.plan_auto_scroll = true;

        // `End` is the explicit jump to the tail: the offset becomes the operator's
        // and the centring intent is released, never re-armed.
        assert!(press(&mut renderer, KeyCode::End, KeyModifiers::NONE));
        assert_eq!(renderer.plan_scroll, 40, "`End` went to the tail");
        assert!(
            !renderer.plan_auto_scroll,
            "a manual jump to the tail re-armed active-task centring"
        );

        // From the tail one `Up` is one line, not a jump back to the in-flight task.
        assert!(press(&mut renderer, KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(renderer.plan_scroll, 39, "`Up` moved one line");
        assert!(!renderer.plan_auto_scroll);

        // Paging back down onto the tail does not hand it back either.
        assert!(press(&mut renderer, KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(renderer.plan_scroll, 40, "`PageDown` went to the tail");
        assert!(
            !renderer.plan_auto_scroll,
            "the tail is an offset, not a request to centre on the active task"
        );

        // With nothing to scroll, a plan press changes neither the offset nor the
        // intent, and asks for no repaint.
        let mut renderer = TuiRenderer::new();
        renderer.set_focus(FocusPane::Plan);
        renderer.plan_max_scroll = 0;
        renderer.plan_auto_scroll = true;
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ] {
            renderer.dirty = false;
            assert!(press(&mut renderer, code, KeyModifiers::NONE));
            assert_eq!(
                renderer.plan_scroll, 0,
                "{code:?} moved an immovable offset"
            );
            assert!(
                renderer.plan_auto_scroll,
                "{code:?} on an unscrollable plan released its centring intent"
            );
            assert!(!renderer.dirty, "{code:?} repainted a still viewport");
        }
    }

    /// The worker-detail tail as the key path measures it, spelled out so the
    /// fixture says what it means. Like the transcript's, it is the pane's own
    /// published maximum once a frame has drawn the Workers pane, and the renderer's
    /// line estimate before then — [`TuiRenderer::worker_detail_tail`] is the one
    /// place that chooses between the two.
    fn worker_detail_tail_of(renderer: &TuiRenderer) -> u16 {
        renderer.worker_detail_tail()
    }

    /// A Workers fixture holding one row whose detail is long enough to scroll.
    fn worker_detail_with_log() -> TuiRenderer {
        let mut renderer = TuiRenderer::new();
        renderer.subagents.push(SubagentDetail {
            name: "coder_specialist-t-004".to_string(),
            logs: (0..40).map(|i| format!("- log line {i:02}")).collect(),
            is_active: true,
            ..SubagentDetail::default()
        });
        renderer.set_focus(FocusPane::Workers);
        renderer
    }

    /// Defects 9 and 15 in the third pane. The worker detail carries its own
    /// auto-scroll flag, and it obeys exactly the transcript's rule: a press that
    /// moves nothing states no decision at all, and the flag comes back on only for
    /// the press that **arrives** on the detail tail. The one other key path allowed
    /// to arm it is the `Left`/`Right` selection reveal (`select_worker`), which
    /// chooses a different detail rather than scrolling this one.
    ///
    /// The keys under test are the detail half's own — `PageUp`/`PageDown`/`Home`/
    /// `End`. Plain `Up`/`Down` are deliberately absent: with the Workers pane
    /// focused they own the **roster window** (defect 14,
    /// [`Self::scroll_worker_roster_by`]), which is what
    /// [`the_workers_arrows_walk_the_roster_window_and_leave_the_log_alone`] is for.
    #[test]
    fn a_worker_detail_press_with_nothing_to_scroll_states_no_autoscroll_decision() {
        let mut renderer = worker_detail_with_log();
        assert_eq!(renderer.subagent_scroll, 0);
        assert!(
            renderer.subagent_autoscroll,
            "a fresh renderer follows the running worker's detail"
        );

        // (a) A pane tall enough for the whole detail: there is no tail to leave, so
        // no scroll key may state a decision — the jump keys included.
        renderer.subagent_width.set(400);
        renderer.subagent_height.set(400);
        assert_eq!(
            worker_detail_tail_of(&renderer),
            0,
            "the fixture must have nothing to scroll"
        );
        for code in [
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ] {
            renderer.dirty = false;
            assert!(
                press(&mut renderer, code, KeyModifiers::NONE),
                "the workers pane holds focus, so {code:?} is its key"
            );
            assert_eq!(
                renderer.subagent_scroll, 0,
                "{code:?} moved an immovable offset"
            );
            assert!(
                renderer.subagent_autoscroll,
                "{code:?} turned worker-detail autoscroll off for no movement"
            );
            assert!(!renderer.dirty, "{code:?} repainted a still viewport");
        }

        // (b) With room to scroll, a press already sitting at the requested extreme
        // still states nothing — in both directions, and with the flag off.
        renderer.subagent_width.set(40);
        renderer.subagent_height.set(4);
        renderer.subagent_autoscroll = false;
        let tail = worker_detail_tail_of(&renderer);
        assert!(tail > 10, "the fixture needs room to be scrolled: {tail}");

        for code in [KeyCode::PageUp, KeyCode::Home] {
            renderer.subagent_scroll = 0;
            renderer.dirty = false;
            assert!(press(&mut renderer, code, KeyModifiers::NONE));
            assert_eq!(renderer.subagent_scroll, 0, "{code:?} moved the offset");
            assert!(
                !renderer.subagent_autoscroll,
                "{code:?} restated an autoscroll decision for no movement"
            );
            assert!(!renderer.dirty, "{code:?} repainted a still viewport");
        }

        for code in [KeyCode::PageDown, KeyCode::End] {
            renderer.subagent_scroll = tail;
            renderer.dirty = false;
            assert!(press(&mut renderer, code, KeyModifiers::NONE));
            assert_eq!(
                renderer.subagent_scroll, tail,
                "{code:?} moved an offset already at its tail"
            );
            assert!(
                !renderer.subagent_autoscroll,
                "{code:?} armed autoscroll without moving the viewport"
            );
            assert!(!renderer.dirty, "{code:?} repainted a still viewport");
        }

        // (c) A real move states its decision: short of the tail it stays off, the
        // arrival turns it on, and pulling back off the tail turns it off again.
        renderer.subagent_scroll = 0;
        assert!(press(&mut renderer, KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(renderer.subagent_scroll, 10, "the page moved the detail");
        assert!(
            !renderer.subagent_autoscroll,
            "a press short of the tail armed it"
        );

        renderer.subagent_scroll = tail - 10;
        assert!(press(&mut renderer, KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(
            renderer.subagent_scroll, tail,
            "the viewport arrived on the detail tail"
        );
        assert!(
            renderer.subagent_autoscroll,
            "the arrival re-armed the flag"
        );
        assert!(renderer.dirty, "and a moved viewport asks for a repaint");

        assert!(press(&mut renderer, KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(renderer.subagent_scroll, tail - 10);
        assert!(
            !renderer.subagent_autoscroll,
            "leaving the tail turned the flag off"
        );
    }

    /// A Workers fixture the frame has already laid out: `count` roster rows, the
    /// selection parked mid-list so the window has room to travel in both directions,
    /// and a **published** `subagent_area` — the rect the key path clamps `↑`/`↓`
    /// against instead of estimating its own.
    fn workers_pane_with_roster(count: usize) -> TuiRenderer {
        let mut renderer = TuiRenderer::new();
        renderer.subagents = (0..count)
            .map(|index| SubagentDetail {
                name: format!("coder_specialist-t-{index:04}"),
                logs: (0..40)
                    .map(|line| format!("- log line {line:02}"))
                    .collect(),
                is_active: true,
                ..SubagentDetail::default()
            })
            .collect();
        renderer.selected_subagent_idx = count.saturating_sub(1) / 2;
        renderer.subagent_area = ratatui::layout::Rect::new(0, 0, 80, 16);
        renderer.set_focus(FocusPane::Workers);
        renderer
    }

    /// The roster window's bounds as the **pane** states them for this fixture — the
    /// same call the key path makes, spelled out here so a failing assertion says
    /// which of the two ends disagrees.
    fn roster_bounds_of(renderer: &TuiRenderer) -> (usize, usize) {
        let pane = renderer.subagent_area;
        activity::roster_window(
            pane,
            crate::ui::tui::layout::workers_detail_present(pane.width),
            renderer.subagents.len(),
            renderer.selected_subagent_idx,
        )
    }

    /// Defect 14: the Workers pane has two viewports, and `↑`/`↓` own the **outer**
    /// one — the roster window. The presses walk it one row at a time against the
    /// bounds the pane publishes, stop at both ends instead of running past them, and
    /// touch nothing else: not the selection (`←`/`→`), not the detail offset
    /// (`PageUp`/`PageDown`/`Home`/`End`), not one follow-ish flag.
    #[test]
    fn the_workers_arrows_walk_the_roster_window_and_leave_the_log_alone() {
        let mut renderer = workers_pane_with_roster(12);
        let (lower, upper) = roster_bounds_of(&renderer);
        assert!(
            upper > lower,
            "the fixture needs a window with room to travel: {lower}..{upper}"
        );

        // Down walks toward the later rows, one step per press, and stops on the end.
        for step in lower..upper {
            renderer.subagent_list_scroll = step as u16;
            renderer.dirty = false;
            assert!(press(&mut renderer, KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(
                usize::from(renderer.subagent_list_scroll),
                step + 1,
                "Down walked the window by one row"
            );
            assert!(renderer.dirty, "a moved window asks for a repaint");
        }
        renderer.dirty = false;
        assert!(press(&mut renderer, KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            usize::from(renderer.subagent_list_scroll),
            upper,
            "Down past the end of the window did not run off it"
        );
        assert!(!renderer.dirty, "a press at the end repainted nothing");

        // Up walks back toward the earlier rows and stops at its own end.
        for step in (lower + 1..=upper).rev() {
            renderer.subagent_list_scroll = step as u16;
            renderer.dirty = false;
            assert!(press(&mut renderer, KeyCode::Up, KeyModifiers::NONE));
            assert_eq!(
                usize::from(renderer.subagent_list_scroll),
                step - 1,
                "Up walked the window by one row"
            );
        }
        renderer.subagent_list_scroll = lower as u16;
        renderer.dirty = false;
        assert!(press(&mut renderer, KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            usize::from(renderer.subagent_list_scroll),
            lower,
            "Up past the head of the window did not run off it"
        );
        assert!(!renderer.dirty, "a press at the head repainted nothing");

        // A stale offset — a stored position the pane would refuse — is reconciled
        // into the window rather than drawn or argued about. Held above the end, it is
        // pulled down to `upper` and the press that would have walked off the end does
        // nothing else (no movement, no repaint).
        renderer.subagent_list_scroll = u16::MAX;
        renderer.dirty = false;
        assert!(press(&mut renderer, KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            usize::from(renderer.subagent_list_scroll),
            upper,
            "an out-of-range offset was not pulled into the window"
        );
        assert!(
            !renderer.dirty,
            "reconciling a stale offset painted nothing"
        );
        // The very next press walks from that reconciled position.
        renderer.dirty = false;
        assert!(press(&mut renderer, KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            usize::from(renderer.subagent_list_scroll),
            upper - 1,
            "the walk did not continue from the reconciled position"
        );
        assert!(renderer.dirty, "a moved window asks for a repaint");

        // And everything the arrows do not own stayed exactly where it was, in both
        // states of the detail's follow flag.
        for autoscroll in [true, false] {
            renderer.subagent_list_scroll = lower as u16;
            renderer.subagent_scroll = 5;
            renderer.subagent_autoscroll = autoscroll;
            renderer.set_follow(true);
            let selected = renderer.selected_subagent_idx;
            let chat_scroll = renderer.chat_scroll;
            let plan_scroll = renderer.plan_scroll;
            for code in [KeyCode::Up, KeyCode::Down] {
                renderer.dirty = false;
                assert!(press(&mut renderer, code, KeyModifiers::NONE));
                assert_eq!(
                    renderer.selected_subagent_idx, selected,
                    "{code:?} moved the roster selection"
                );
                assert_eq!(renderer.subagent_scroll, 5, "{code:?} moved the detail log");
                assert_eq!(
                    renderer.subagent_autoscroll, autoscroll,
                    "{code:?} restated the detail's follow flag"
                );
                assert!(renderer.follow, "{code:?} flipped transcript follow");
                assert_eq!(
                    renderer.chat_scroll, chat_scroll,
                    "{code:?} scrolled the transcript"
                );
                assert_eq!(
                    renderer.plan_scroll, plan_scroll,
                    "{code:?} scrolled the plan"
                );
            }
            assert_eq!(
                usize::from(renderer.subagent_list_scroll),
                lower + 1,
                "the pair walked the window by exactly one row (Up at the head moved nothing)"
            );
        }
    }

    /// The roster half of the no-movement-no-decision rule: a roster that fits its
    /// half has no window to move, so an arrow is still the pane's key (it is claimed)
    /// but moves no offset, states no flag and asks for no repaint (defect 14).
    #[test]
    fn a_roster_with_nothing_to_scroll_states_nothing_when_the_arrows_are_pressed() {
        let mut renderer = workers_pane_with_roster(2);
        let (lower, upper) = roster_bounds_of(&renderer);
        assert_eq!((lower, upper), (0, 0), "two rows fit their half");

        renderer.subagent_scroll = 7;
        renderer.subagent_autoscroll = false;
        renderer.set_follow(true);
        for code in [KeyCode::Up, KeyCode::Down] {
            renderer.dirty = false;
            assert!(
                press(&mut renderer, code, KeyModifiers::NONE),
                "{code:?} is still the roster's key"
            );
            assert_eq!(
                renderer.subagent_list_scroll, 0,
                "{code:?} moved an immovable window"
            );
            assert_eq!(renderer.subagent_scroll, 7, "{code:?} moved the log");
            assert!(
                !renderer.subagent_autoscroll,
                "{code:?} flipped the detail's follow flag"
            );
            assert!(renderer.follow, "{code:?} flipped transcript follow");
            assert!(!renderer.dirty, "{code:?} repainted a still window");
        }
    }

    /// The other half of one-key-one-owner: the detail's own page and jump keys move
    /// the log — and state its follow decision — while the roster window stands
    /// exactly where the arrows left it.
    #[test]
    fn the_detail_page_and_jump_keys_leave_the_roster_window_where_it_stands() {
        let mut renderer = workers_pane_with_roster(12);
        let (lower, upper) = roster_bounds_of(&renderer);
        assert!(upper > lower, "the roster needs room to be parked in");
        let parked = lower + 1;
        renderer.subagent_list_scroll = parked as u16;
        renderer.subagent_scroll = 0;
        renderer.subagent_autoscroll = false;
        let selected = renderer.selected_subagent_idx;

        for code in [
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ] {
            assert!(
                press(&mut renderer, code, KeyModifiers::NONE),
                "{code:?} is the detail's key"
            );
            assert_eq!(
                usize::from(renderer.subagent_list_scroll),
                parked,
                "{code:?} walked the roster window"
            );
            assert_eq!(
                renderer.selected_subagent_idx, selected,
                "{code:?} moved the roster selection"
            );
        }

        let tail = worker_detail_tail_of(&renderer);
        assert!(tail > 10, "the log needs room to scroll: {tail}");
        assert_eq!(
            renderer.subagent_scroll, tail,
            "the page and jump keys did not walk the log"
        );
        assert!(
            renderer.subagent_autoscroll,
            "arriving on the log tail did not re-arm its flag"
        );
    }

    /// Defect 15, on the key path: the flag is the request, and the key path only
    /// ever states the flag and the offset it moved. The pane resolves "the newest
    /// line" against a body it counted itself — `ActivityData::autoscroll` is this
    /// very flag, carried across untouched — so no tail estimated here can be parked
    /// into `subagent_scroll` as an offset. The resolving half is held where it
    /// happens, by `a_tail_request_pins_the_detail_to_the_newest_line_it_measured` in
    /// `components::activity`.
    #[test]
    fn the_follow_flag_is_the_tail_request_and_the_offset_stays_the_operators() {
        let mut renderer = worker_detail_with_log();
        renderer.subagent_width.set(40);
        renderer.subagent_height.set(4);
        let tail = worker_detail_tail_of(&renderer);
        assert!(tail > 10, "the log needs room to scroll: {tail}");

        renderer.subagent_scroll = tail;
        assert!(renderer.subagent_autoscroll, "a fresh renderer follows");
        assert_eq!(
            state::UiView::from_renderer(&renderer).scroll.workers,
            renderer.subagent_scroll,
            "the view model mirrors the offset and carries the flag beside it"
        );

        assert!(press(&mut renderer, KeyCode::PageUp, KeyModifiers::NONE));
        assert!(
            !renderer.subagent_autoscroll,
            "leaving the tail did not turn the flag off"
        );
        assert!(
            renderer.subagent_scroll < tail,
            "the gesture did not move the offset it owns"
        );
        assert_eq!(
            state::UiView::from_renderer(&renderer).scroll.workers,
            renderer.subagent_scroll,
            "the pane was handed the offset the key left, with follow off"
        );

        assert!(press(&mut renderer, KeyCode::End, KeyModifiers::NONE));
        assert!(
            renderer.subagent_autoscroll,
            "arriving on the tail did not re-arm the flag"
        );
        assert_eq!(
            state::UiView::from_renderer(&renderer).scroll.workers,
            renderer.subagent_scroll,
            "the flag came back and the offset is still the mirror"
        );
    }
}
