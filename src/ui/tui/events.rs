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
/// armed-abort flow already runs, or cancels **one** named worker — nothing else is
/// reachable from a keystroke.
///
/// The worker identity is captured at request time, so a roster that changes under
/// the overlay (a worker finishing, a new one registered) can never retarget `y`
/// onto a different row, and `orchestrator::cancel_all()` is never reachable from
/// here (spec §5, `bus.rs:38`).
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
                    self.scroll_to_top();
                    self.set_follow(self.chat_auto_scroll);
                    self.dirty = true;
                }
                return true;
            }
            KeyCode::End => {
                if self.focus == FocusPane::Input {
                    self.edit_key(key);
                } else {
                    self.scroll_to_bottom();
                    self.set_follow(self.chat_auto_scroll);
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
    fn cycle_view_mode(&mut self) {
        let next = match self.view_mode {
            ViewMode::Split => ViewMode::Transcript,
            ViewMode::Transcript => ViewMode::Plan,
            ViewMode::Plan => ViewMode::Workers,
            ViewMode::Workers => ViewMode::Split,
        };
        self.set_view_mode(next);
    }

    /// `Ctrl+P` / `Ctrl+A` (§5): show or hide the Plan / Workers pane. The pane
    /// toggles are the mode's inventory spelled out, so the route is
    /// [`Self::set_view_mode`]: in M1 the key changes the mode instead of doing
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
        self.set_view_mode(ViewMode::for_panes(plan_visible, workers_visible));
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

    /// Scroll the focused pane and restate follow from what the scroll path decided
    /// (G7): scrolling back off the tail turns follow **off**, and returning to the
    /// tail turns it back on, so the chip can never lag the viewport.
    fn scroll_by(&mut self, up: bool, amount: u16) {
        if up {
            self.scroll_up(amount);
        } else {
            self.scroll_down(amount);
        }
        self.set_follow(self.chat_auto_scroll);
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
        self.disarm_abort();
        self.confirm_action = Some(ConfirmAction::Quit);
        self.show_confirm();
        self.status_line = "Confirm quit — y to quit, n or Esc to keep working.".to_string();
        self.dirty = true;
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
        self.disarm_abort();
        self.confirm_action = Some(ConfirmAction::CancelWorker {
            key: key.clone(),
            task_id,
        });
        self.show_confirm();
        self.status_line = format!(
            "Confirm cancel of worker '{key}' — y to cancel it, n or Esc to leave it running."
        );
        self.dirty = true;
        true
    }

    /// `y` inside the confirm surface: run the pending request.
    ///
    /// `Quit` takes the same exit path the armed-abort flow uses today, and
    /// `CancelWorker` addresses one worker by name through the registry's
    /// exact-identity matching. `confirm_all()`-style bulk cancellation is not
    /// reachable from a keystroke (spec §5).
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
            Some(ConfirmAction::Quit) | None => {
                Renderer::request_user_exit(self);
            }
        }
        self.dirty = true;
        true
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

    /// `e` (§5): show the last error again.
    ///
    /// Spec §8.5 gives the renderer a dedicated `last_error` buffer that stores the
    /// categorical text verbatim; that buffer does not exist yet and needs the t-012
    /// plumbing, so this recalls the error traffic the session loop has already
    /// pushed into the transcript and the status line, and says so out loud when
    /// there is nothing to recall (G5 — `e` is never a silent key).
    fn recall_last_error(&mut self) {
        let found = Self::error_line(&self.messages, &self.status_line);
        match found {
            Some(text) => {
                self.messages
                    .push(format!("✗ {text}\nsee .marmel/marmel.log"));
            }
            None => self.messages.push(
                "[CLI] No error recorded to recall. The sticky last-error buffer \
                 (spec §8.5) is still pending plumbing: once it lands, `e` and `/error` \
                 print the stored cause here."
                    .to_string(),
            ),
        }
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

    pub(crate) fn scroll_up(&mut self, amount: u16) {
        match self.focused_panel {
            FocusedPanel::Chat => {
                self.chat_auto_scroll = false;
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
                    self.chat_auto_scroll = true;
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

    pub(crate) fn scroll_to_top(&mut self) {
        match self.focused_panel {
            FocusedPanel::Chat => {
                self.chat_auto_scroll = false;
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

    pub(crate) fn scroll_to_bottom(&mut self) {
        match self.focused_panel {
            FocusedPanel::Chat => {
                let w = self.chat_width.get();
                let n = self.estimated_chat_lines(w);
                let h = self.chat_height.get();
                self.chat_scroll = n.saturating_sub(h) as u16;
                self.chat_auto_scroll = true;
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
        // Handle the `/thought` toggle locally.
        if line.trim().eq_ignore_ascii_case("/thought") {
            self.show_thought = !self.show_thought;
            let state = if self.show_thought { "ON" } else { "OFF" };
            self.messages
                .push(format!("[CLI] Thinking display: {state}"));
            self.chat_auto_scroll = true;
            return;
        }
        // §5 + acceptance criterion 3: `/help`, the `F1` overlay and the footer
        // print **one** table. The legend used to be a hard-coded string here and had
        // already gone stale (it advertised mouse bindings the handler never had), so
        // the text now comes from `components::help::help_text()`, which is built
        // from `legend()` plus the slash-command table the composer's autocomplete
        // reads. Printing this rather than a copy is what makes the three surfaces
        // byte-identical.
        if line.trim().eq_ignore_ascii_case("/help") {
            self.messages.push(format!("[CLI] {}", help::help_text()));
            self.chat_auto_scroll = true;
            return;
        }
        let trimmed_lower = line.trim().to_ascii_lowercase();
        if trimmed_lower == "/reset"
            || trimmed_lower == "/reset_plan"
            || trimmed_lower == "/reset-plan"
            || trimmed_lower == "/clear_plan"
            || trimmed_lower == "/clear-plan"
        {
            let plan = crate::manager::phase::Plan::default();
            let _ = plan.clear();
            self.show_plan_panel = false;
            self.had_active_plan = false;
            self.plan_content.clear();
            self.focused_panel = FocusedPanel::Chat;
            self.status_line = "Ready".to_string();
            self.messages.push(
                "[CLI] Execution plan reset and cleared. Phase reverted to Conversational."
                    .to_string(),
            );
            self.chat_auto_scroll = true;
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
        self.chat_auto_scroll = true;
        if is_abort(&line) {
            Renderer::request_user_exit(self);
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
        self.chat_auto_scroll = true;
    }
}
