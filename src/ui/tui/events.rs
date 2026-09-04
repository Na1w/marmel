//! Keyboard, mouse, and input editing events for TuiRenderer.

use super::formatting::*;
use super::{FocusedPanel, TuiRenderer, is_abort};
use crate::ui::{Renderer, SubagentDetail};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

impl TuiRenderer {
    /// Drain any pending crossterm events, updating the input line / focus /
    /// abort state and forwarding completed lines to the session loop.
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
            handled = true;
            match event::read() {
                Ok(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    match key.code {
                        KeyCode::Enter => self.submit(),
                        KeyCode::Esc => {
                            // Esc arms confirm-abort (or aborts if already armed or focus switched).
                            if self.confirm_abort {
                                self.aborted = true;
                                self.confirm_abort = false;
                                self.status_line = "Aborted by user.".to_string();
                                crate::orchestrator::cancel_all();
                            } else if self.focused_panel != FocusedPanel::Chat {
                                self.focused_panel = FocusedPanel::Chat;
                                self.confirm_abort = true;
                                self.status_line =
                                    "Abort armed. Press ESC again to abort.".to_string();
                            } else {
                                self.confirm_abort = true;
                                self.status_line =
                                    "Abort armed. Press ESC again to abort.".to_string();
                            }
                        }
                        KeyCode::Tab => self.cycle_focus(),
                        KeyCode::PageUp => self.scroll_up(10),
                        KeyCode::PageDown => self.scroll_down(10),
                        KeyCode::Up => {
                            if key.modifiers.contains(KeyModifiers::CONTROL) {
                                self.history_prev();
                            } else {
                                self.scroll_up(1);
                            }
                        }
                        KeyCode::Down => {
                            if key.modifiers.contains(KeyModifiers::CONTROL) {
                                self.history_next();
                            } else {
                                self.scroll_down(1);
                            }
                        }
                        KeyCode::Left => {
                            if self.focused_panel == FocusedPanel::Subagents
                                && self.selected_subagent_idx > 0
                            {
                                self.selected_subagent_idx -= 1;
                                self.subagent_autoscroll = true;
                                let w = self.subagent_width.get();
                                let n = self.estimated_subagent_lines(w);
                                let h = self.subagent_height.get();
                                self.subagent_scroll = n.saturating_sub(h) as u16;
                            } else if self.focused_panel == FocusedPanel::Chat {
                                // F1: move input cursor left by one grapheme.
                                self.cursor_left();
                            }
                        }
                        KeyCode::Right => {
                            if self.focused_panel == FocusedPanel::Subagents
                                && !self.subagents.is_empty()
                                && self.selected_subagent_idx + 1 < self.subagents.len()
                            {
                                self.selected_subagent_idx += 1;
                                self.subagent_autoscroll = true;
                                let w = self.subagent_width.get();
                                let n = self.estimated_subagent_lines(w);
                                let h = self.subagent_height.get();
                                self.subagent_scroll = n.saturating_sub(h) as u16;
                            } else if self.focused_panel == FocusedPanel::Chat {
                                // F1: move input cursor right by one grapheme.
                                self.cursor_right();
                            }
                        }
                        KeyCode::Home => {
                            if self.focused_panel == FocusedPanel::Chat {
                                // F1: move input cursor to the start.
                                self.cursor = 0;
                            } else {
                                self.scroll_to_top();
                            }
                        }
                        KeyCode::End => {
                            if self.focused_panel == FocusedPanel::Chat {
                                // F1: move input cursor to the end.
                                self.cursor = self.input_text.len();
                            } else {
                                self.scroll_to_bottom();
                            }
                        }
                        KeyCode::Delete => {
                            // F1: forward-delete the grapheme after the cursor.
                            if self.focused_panel == FocusedPanel::Chat {
                                self.delete_forward();
                            }
                        }
                        KeyCode::Backspace | KeyCode::Char('\x08') | KeyCode::Char('\x7f') => {
                            // F1: delete the grapheme before the cursor.
                            self.delete_backward();
                        }
                        KeyCode::Char(c) => {
                            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                            if ctrl && (c == 'c' || c == 'C') {
                                // Ctrl+C arms/triggers confirm-abort.
                                if self.confirm_abort {
                                    self.aborted = true;
                                    self.confirm_abort = false;
                                    self.status_line = "Aborted by user.".to_string();
                                    crate::orchestrator::cancel_all();
                                } else {
                                    self.confirm_abort = true;
                                    self.status_line =
                                        "Abort armed. Press Ctrl+C or ESC again to abort."
                                            .to_string();
                                }
                            } else if ctrl && (c == 'd' || c == 'D') {
                                // Ctrl+D is treated as abort.
                                self.aborted = true;
                                self.confirm_abort = false;
                                self.status_line = "Aborted by user.".to_string();
                                crate::orchestrator::cancel_all();
                            } else if ctrl && (c == 'p' || c == 'P') {
                                self.confirm_abort = false;
                                // F12: single Ctrl+P handler (toggle plan panel).
                                self.show_plan_panel = !self.show_plan_panel;
                                if !self.show_plan_panel && self.focused_panel == FocusedPanel::Plan
                                {
                                    self.focused_panel = FocusedPanel::Chat;
                                }
                            } else if ctrl && (c == 'a' || c == 'A') {
                                self.confirm_abort = false;
                                // F12: single Ctrl+A handler (toggle subagents).
                                self.toggle_subagent_panel();
                                if !self.show_subagent_panel
                                    && self.focused_panel == FocusedPanel::Subagents
                                {
                                    self.focused_panel = FocusedPanel::Chat;
                                }
                            } else if !c.is_control() {
                                self.confirm_abort = false;
                                // F1: insert the character at the cursor.
                                self.insert_char(c);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(TermEvent::Mouse(mouse)) => match mouse.kind {
                    crossterm::event::MouseEventKind::ScrollUp => {
                        let (x, y) = (mouse.column, mouse.row);
                        if rect_contains(self.chat_area, x, y) {
                            let old_focus = self.focused_panel;
                            self.focused_panel = FocusedPanel::Chat;
                            self.scroll_up(3);
                            self.focused_panel = old_focus;
                        } else if rect_contains(self.plan_area, x, y) {
                            let old_focus = self.focused_panel;
                            self.focused_panel = FocusedPanel::Plan;
                            self.scroll_up(3);
                            self.focused_panel = old_focus;
                        } else if rect_contains(self.subagent_area, x, y) {
                            let old_focus = self.focused_panel;
                            self.focused_panel = FocusedPanel::Subagents;
                            self.scroll_up(3);
                            self.focused_panel = old_focus;
                        } else {
                            self.scroll_up(3);
                        }
                    }
                    crossterm::event::MouseEventKind::ScrollDown => {
                        let (x, y) = (mouse.column, mouse.row);
                        if rect_contains(self.chat_area, x, y) {
                            let old_focus = self.focused_panel;
                            self.focused_panel = FocusedPanel::Chat;
                            self.scroll_down(3);
                            self.focused_panel = old_focus;
                        } else if rect_contains(self.plan_area, x, y) {
                            let old_focus = self.focused_panel;
                            self.focused_panel = FocusedPanel::Plan;
                            self.scroll_down(3);
                            self.focused_panel = old_focus;
                        } else if rect_contains(self.subagent_area, x, y) {
                            let old_focus = self.focused_panel;
                            self.focused_panel = FocusedPanel::Subagents;
                            self.scroll_down(3);
                            self.focused_panel = old_focus;
                        } else {
                            self.scroll_down(3);
                        }
                    }
                    crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        let (x, y) = (mouse.column, mouse.row);
                        if rect_contains(self.chat_area, x, y) {
                            self.focused_panel = FocusedPanel::Chat;
                        } else if rect_contains(self.plan_area, x, y) {
                            self.focused_panel = FocusedPanel::Plan;
                        } else if rect_contains(self.subagent_area, x, y) {
                            self.focused_panel = FocusedPanel::Subagents;
                        }
                        // F7: click within the input area places the cursor at the
                        // nearest grapheme boundary. The input area occupies the
                        // bottom 3 rows of the terminal.
                        let input_area = self.input_area;
                        if rect_contains(input_area, x, y) {
                            self.focused_panel = FocusedPanel::Chat;
                            self.click_to_cursor(x, input_area);
                        }
                    }
                    _ => {}
                },
                Ok(TermEvent::Resize(_, _)) => {
                    // F6: explicit resize handling — clamp all scroll offsets and
                    // trigger a flush so no stale viewport shows blank space.
                    self.clamp_scrolls();
                    let _ = self.flush();
                }
                Ok(_) => {}
                Err(_) => break,
            }
            if blocking {
                break;
            }
        }
        handled
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

    /// Move the input cursor one grapheme to the left (F1).
    pub(crate) fn cursor_left(&mut self) {
        self.sanitize_cursor();
        let before = &self.input_text[..self.cursor];
        let graphemes: Vec<&str> = before.graphemes(true).collect();
        if let Some(last) = graphemes.last() {
            self.cursor -= last.len();
        }
        self.sanitize_cursor();
    }

    /// Move the input cursor one grapheme to the right (F1).
    pub(crate) fn cursor_right(&mut self) {
        self.sanitize_cursor();
        let after = &self.input_text[self.cursor..];
        if let Some(first) = after.graphemes(true).next() {
            self.cursor += first.len();
        }
        self.sanitize_cursor();
    }

    /// Delete the grapheme immediately before the cursor (Backspace, F1).
    pub(crate) fn delete_backward(&mut self) {
        self.sanitize_cursor();
        let before = &self.input_text[..self.cursor];
        let graphemes: Vec<&str> = before.graphemes(true).collect();
        if let Some(last) = graphemes.last() {
            let start = self.cursor - last.len();
            self.input_text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
        self.sanitize_cursor();
    }

    /// Delete the grapheme immediately after the cursor (Delete, F1).
    pub(crate) fn delete_forward(&mut self) {
        self.sanitize_cursor();
        let after = &self.input_text[self.cursor..];
        if let Some(first) = after.graphemes(true).next() {
            self.input_text
                .replace_range(self.cursor..self.cursor + first.len(), "");
        }
        self.sanitize_cursor();
    }

    /// Insert a character at the cursor position (F1).
    pub(crate) fn insert_char(&mut self, c: char) {
        self.sanitize_cursor();
        self.input_text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// F7: click within the input area places the cursor at the
    /// nearest grapheme boundary.
    pub(crate) fn click_to_cursor(&mut self, x: u16, area: Rect) {
        let relative_x = x.saturating_sub(area.x);
        let mut byte_offset = 0;
        let mut visual_x = 0;
        for grapheme in self.input_text.graphemes(true) {
            if visual_x >= relative_x {
                break;
            }
            byte_offset += grapheme.len();
            visual_x += UnicodeWidthStr::width(grapheme) as u16;
        }
        self.cursor = byte_offset.min(self.input_text.len());
        self.sanitize_cursor();
    }

    /// Clamp all scroll offsets to their recomputed maxima (F6).
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

    /// Toggle the subagents panel; when turned on, scroll to bottom and focus it.
    pub(crate) fn toggle_subagent_panel(&mut self) {
        self.show_subagent_panel = !self.show_subagent_panel;
        if self.show_subagent_panel {
            self.subagent_autoscroll = true;
            let w = self.subagent_width.get();
            let n = self.estimated_subagent_lines(w);
            let h = self.subagent_height.get();
            self.subagent_scroll = n.saturating_sub(h) as u16;
            if !self.subagents.is_empty() {
                self.focused_panel = FocusedPanel::Subagents;
            }
        }
    }

    /// Create or update a subagent entry in the local list (t-c304). Used by
    /// the `Delegation` event handler so the panel reflects Started/Completed
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
                existing.is_active = is_active;
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
                    last_activity_at: Some(now),
                    logs: vec![log.to_string()],
                    thinking: String::new(),
                    content: String::new(),
                    is_active,
                    context_tokens: 0,
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
            self.input_text = self.history[idx].clone();
            self.cursor = self.input_text.len();
        }
    }

    /// Walk forward through input history (Ctrl+Down).
    pub(crate) fn history_next(&mut self) {
        if let Some(idx) = self.history_index {
            if idx + 1 < self.history.len() {
                self.history_index = Some(idx + 1);
                self.input_text = self.history[idx + 1].clone();
                self.cursor = self.input_text.len();
            } else {
                self.history_index = None;
                self.input_text = self.input_draft.clone();
                self.cursor = self.input_text.len();
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
        let line = std::mem::take(&mut self.input_text);
        self.cursor = 0;
        if line.trim().is_empty() {
            return;
        }
        // Handle the `/thought` toggle locally (reference §13).
        if line.trim().eq_ignore_ascii_case("/thought") {
            self.show_thought = !self.show_thought;
            let state = if self.show_thought { "ON" } else { "OFF" };
            self.messages
                .push(format!("[CLI] Thinking display: {state}"));
            self.chat_auto_scroll = true;
            return;
        }
        // Handle the `/help` command locally (F2): print the command/keybinding
        // legend into the chat pane and do not forward to the session loop.
        if line.trim().eq_ignore_ascii_case("/help") {
            self.messages.push(
                "[CLI] Available slash commands:\n\
                 /help - show this help\n\
                 /thought - toggle the thinking block display\n\
                 /reset, /reset_plan, /reset-plan, /clear_plan, /clear-plan - clear the execution plan\n\
                 /abort, /exit, /quit, /q, :q, :q! - abort the current session\n\n\
                 [CLI] Keybindings:\n\
                 Tab - cycle focus (Chat / Plan / Subagents)\n\
                 Ctrl+P - toggle the plan panel\n\
                 Ctrl+A - toggle the subagents panel\n\
                 Ctrl+Up / Ctrl+Down - input history navigation\n\
                 Esc / Ctrl+C - confirm-abort (first press arms, second aborts; Esc first returns focus to Chat)\n\
                 Mouse scroll / click - scroll panels / change focus\n\
                 Left / Right / Home / End / Delete - move the input cursor\n\
                 Backspace - delete the grapheme before the cursor"
                    .to_string(),
            );
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
            let plan = crate::agent::phase::Plan::default();
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
        let user_tokens = tiktoken_rs::cl100k_base_singleton()
            .encode_ordinary(&line)
            .len();
        self.tokens_in = self.tokens_in.saturating_add(user_tokens);
        // Echo the user's line into the chat view.
        self.messages.push(format!("User: {trimmed}"));
        self.chat_auto_scroll = true;
        if is_abort(&line) {
            self.aborted = true;
        }
        // Forward the line to the session loop.
        let _ = self.tx.send(line);
    }

    /// Cycle focus: Chat → Plan → Subagents → Chat (reference §2).
    pub(crate) fn cycle_focus(&mut self) {
        let has_plan = !self.plan_content.trim().is_empty() && self.show_plan_panel;
        let has_subagents = self.show_subagent_panel && !self.subagents.is_empty();
        self.focused_panel = match self.focused_panel {
            FocusedPanel::Chat => {
                if has_plan {
                    FocusedPanel::Plan
                } else if has_subagents {
                    FocusedPanel::Subagents
                } else {
                    FocusedPanel::Chat
                }
            }
            FocusedPanel::Plan => {
                if has_subagents {
                    FocusedPanel::Subagents
                } else {
                    FocusedPanel::Chat
                }
            }
            FocusedPanel::Subagents => FocusedPanel::Chat,
        };
    }

    /// Append a completed steer sentence as a `Marmennill: ` message (reference §10).
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
