//! Interactive 3-panel Ratatui terminal UI.
//!
//! This renderer reproduces the `marmennill-cli` UI/UX contract (see
//! `REFERENCE_UI_CONTRACT.md`):
//!
//! - **Vertical layout**: `Main (Min 3)` + `Status (Length 1)` + `Input (Length 3)`.
//! - **Horizontal layout**: `Chat (60%)` + right panel (`40%`).
//! - **Right panel split**: `Plan` (top 50%) + `Subagents` (bottom 50%) when both
//!   are active; otherwise the active panel fills the right side.
//! - **`FocusedPanel`** (`Chat`, `Plan`, `Subagents`) with `Tab` cycling.
//! - **Prefix-based message styling** with the exact colors/modifiers from the
//!   reference contract, plus a `show_thought` toggle and sentence streaming
//!   (`extract_complete_sentences`) for `Marmennill: ` prefixed steer output.
//!
//! Keyboard (via crossterm event-stream):
//! - `Enter` — send the current input line to the agent.
//! - `Esc` / `Ctrl+C` — confirm-abort: first press arms a confirmation state
//!   ("press again to abort, or any other key to continue"); second press quits.
//!   While not on Chat focus, `Esc` first returns focus to Chat.
//! - `Tab` — cycle focus between the three panels.
//! - `Left` / `Right` / `Home` / `End` / `Delete` — move the input cursor
//!   (in Chat focus; `Left`/`Right` select subagents when the Subagents panel is
//!   focused, `Home`/`End` scroll the focused panel).
//! - `Backspace` — delete the grapheme before the cursor.
//! - `Ctrl+P` — toggle the plan panel.
//! - `Ctrl+A` — toggle the subagents panel.
//! - `Ctrl+Up` / `Ctrl+Down` — input history navigation.
//! - `/thought` — toggle the thinking block display.
//! - `/help` — show this command/keybinding legend in the chat pane.
//! - `/reset` — clear the execution plan (handled locally).
//! - `/abort` — explicit abort command.

use super::{Event, Renderer, SubagentDetail};
use crate::config::Config;
use crate::orchestrator::OrchestratorManager;
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use std::io::{self};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Which panel currently holds focus (drives border color and navigation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusedPanel {
    Chat,
    Plan,
    Subagents,
}

/// The interactive TUI renderer.
pub struct TuiRenderer {
    /// Ordered chat/log transcript. Each entry is a single message string
    /// (may contain embedded `\n`).
    messages: Vec<String>,
    /// Streaming assistant "thinking" block (between ` thinking` and ` response`).
    current_thought: String,
    /// Streaming assistant content (final answer text).
    current_content: String,
    /// The execution plan text. Defaults to `"No active execution plan."`.
    plan_content: String,
    /// The current text in the input box.
    input_text: String,
    /// Byte offset of the input cursor within `input_text` (always on a char
    /// boundary). Drives insert/delete and the rendered caret position.
    cursor: usize,
    /// Whether the user is in the "confirm abort" state.
    confirm_abort: bool,
    /// Frame counter for animations (e.g. status spinner).
    frame_counter: u64,
    /// The status bar text. Defaults to `"Ready"`.
    status_line: String,
    /// Which panel has focus (drives border color). Defaults to `Chat`.
    focused_panel: FocusedPanel,
    /// Vertical scroll offset of the chat panel.
    chat_scroll: u16,
    /// Vertical scroll offset of the plan panel.
    plan_scroll: u16,
    /// Whether the plan panel is visible. Defaults to `false`.
    show_plan_panel: bool,
    /// Track if an active plan was previously open on disk for auto-open/close.
    had_active_plan: bool,
    /// Whether ` thinking`/` response` blocks are displayed. Defaults to `false`.
    show_thought: bool,
    /// Whether the subagents panel is visible. Defaults to `false`.
    show_subagent_panel: bool,
    /// List of active specialist subagents.
    subagents: Vec<SubagentDetail>,
    /// Index of the currently selected subagent. Defaults to `0`.
    selected_subagent_idx: usize,
    /// Scroll offset of the subagent details pane.
    subagent_scroll: u16,
    /// Scroll offset of the subagent list.
    subagent_list_scroll: u16,
    /// Input command history (most recent last).
    history: Vec<String>,
    /// Current position in history navigation. `None` = not navigating.
    history_index: Option<usize>,
    /// Saved draft of `input_text` when history navigation begins.
    input_draft: String,
    /// Buffer for streaming steer-mode sentences awaiting completion.
    steer_sentence_buffer: String,

    /// Cached chat inner width (area width − 2). Defaults to `80`.
    chat_width: std::cell::Cell<usize>,
    /// Cached chat inner height (area height − 2). Defaults to `15`.
    chat_height: std::cell::Cell<usize>,
    /// Cached subagent details width. Defaults to `40`.
    subagent_width: std::cell::Cell<usize>,
    /// Cached subagent details height. Defaults to `15`.
    subagent_height: std::cell::Cell<usize>,
    /// Cached plan panel width. Defaults to `40`.
    plan_width: std::cell::Cell<usize>,
    /// Cached plan panel height. Defaults to `15`.
    plan_height: std::cell::Cell<usize>,
    /// Width used for the current line cache. Defaults to `0`.
    cached_chat_width: std::cell::Cell<usize>,
    /// `show_thought` value used for the current line cache.
    cached_show_thought: std::cell::Cell<bool>,
    /// Per-message cached wrapped-line counts.
    cached_message_lines: std::cell::RefCell<Vec<usize>>,
    /// Sum of all cached per-message line counts.
    cached_total_message_lines: std::cell::Cell<usize>,
    /// Whether subagent details auto-scroll. Defaults to `false`.
    subagent_autoscroll: bool,

    /// Completed input lines forwarded to the session loop.
    rx: Receiver<String>,
    tx: Sender<String>,
    aborted: bool,
    /// Persistent terminal instance to preserve Ratatui frame diff buffers.
    terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,

    chat_auto_scroll: bool,
    plan_max_scroll: u16,
    plan_auto_scroll: bool,

    chat_area: Rect,
    plan_area: Rect,
    subagent_area: Rect,
    input_area: Rect,

    active_agent: String,
    last_render: std::time::Instant,
    waiting_for_token_since: Option<std::time::Instant>,

    /// Estimated total input (prompt) tokens across the session.
    pub tokens_in: usize,
    /// Estimated total output (completion) tokens across the session.
    pub tokens_out: usize,
    /// Context window size (tokens) of the orchestrator/main turn.
    pub orchestrator_context_tokens: usize,
}

impl TuiRenderer {
    pub fn new() -> Self {
        let (tx, rx) = channel::<String>();
        Self {
            messages: Vec::new(),
            current_thought: String::new(),
            current_content: String::new(),
            plan_content: "No active execution plan.".to_string(),
            input_text: String::new(),
            cursor: 0,
            confirm_abort: false,
            frame_counter: 0,
            status_line: "Ready".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            orchestrator_context_tokens: 0,
            focused_panel: FocusedPanel::Chat,
            chat_scroll: 0,
            plan_scroll: 0,
            show_plan_panel: false,
            had_active_plan: false,
            show_thought: false,
            show_subagent_panel: false,
            subagents: Vec::new(),
            selected_subagent_idx: 0,
            subagent_scroll: 0,
            subagent_list_scroll: 0,
            history: Vec::new(),
            history_index: None,
            input_draft: String::new(),
            steer_sentence_buffer: String::new(),

            chat_width: std::cell::Cell::new(80),
            chat_height: std::cell::Cell::new(15),
            subagent_width: std::cell::Cell::new(40),
            subagent_height: std::cell::Cell::new(15),
            plan_width: std::cell::Cell::new(40),
            plan_height: std::cell::Cell::new(15),
            cached_chat_width: std::cell::Cell::new(0),
            cached_show_thought: std::cell::Cell::new(false),
            cached_message_lines: std::cell::RefCell::new(Vec::new()),
            cached_total_message_lines: std::cell::Cell::new(0),
            subagent_autoscroll: true,

            rx,
            tx,
            aborted: false,
            terminal: None,

            chat_auto_scroll: true,
            plan_max_scroll: 0,
            plan_auto_scroll: false,

            chat_area: Rect::default(),
            plan_area: Rect::default(),
            subagent_area: Rect::default(),
            input_area: Rect::default(),

            active_agent: "Manager".to_string(),
            last_render: std::time::Instant::now(),
            waiting_for_token_since: None,
        }
    }

    /// Drain any pending crossterm events, updating the input line / focus /
    /// abort state and forwarding completed lines to the session loop.
    fn handle_events(&mut self, blocking: bool) -> bool {
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

    fn sanitize_cursor(&mut self) {
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
    fn cursor_left(&mut self) {
        self.sanitize_cursor();
        let before = &self.input_text[..self.cursor];
        let graphemes: Vec<&str> = before.graphemes(true).collect();
        if let Some(last) = graphemes.last() {
            self.cursor -= last.len();
        }
        self.sanitize_cursor();
    }

    /// Move the input cursor one grapheme to the right (F1).
    fn cursor_right(&mut self) {
        self.sanitize_cursor();
        let after = &self.input_text[self.cursor..];
        if let Some(first) = after.graphemes(true).next() {
            self.cursor += first.len();
        }
        self.sanitize_cursor();
    }

    /// Delete the grapheme immediately before the cursor (Backspace, F1).
    fn delete_backward(&mut self) {
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
    fn delete_forward(&mut self) {
        self.sanitize_cursor();
        let after = &self.input_text[self.cursor..];
        if let Some(first) = after.graphemes(true).next() {
            self.input_text
                .replace_range(self.cursor..self.cursor + first.len(), "");
        }
        self.sanitize_cursor();
    }

    /// Insert a character at the cursor position (F1).
    fn insert_char(&mut self, c: char) {
        self.sanitize_cursor();
        self.input_text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// F7: click within the input area places the cursor at the
    /// nearest grapheme boundary.
    fn click_to_cursor(&mut self, x: u16, area: Rect) {
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
    fn clamp_scrolls(&mut self) {
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

    fn scroll_up(&mut self, amount: u16) {
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

    fn scroll_down(&mut self, amount: u16) {
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

    fn scroll_to_top(&mut self) {
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

    fn scroll_to_bottom(&mut self) {
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
    fn toggle_subagent_panel(&mut self) {
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
    fn upsert_subagent(&mut self, name: &str, is_active: bool, log: &str) {
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
    fn history_prev(&mut self) {
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
    fn history_next(&mut self) {
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
    fn commit_turn_content(&mut self) {
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
    fn submit(&mut self) {
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
    fn cycle_focus(&mut self) {
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
    fn append_steer_sentence(&mut self, chunk: &str) {
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

    /// Ensure the per-message wrapped-line cache is up to date for `width`
    /// (reference §11.3). Performs a full recompute when the width or
    /// `show_thought` changes, an incremental append when new messages are
    /// added, and a truncate when messages are removed.
    fn ensure_message_cache(&self, width: usize) {
        let cached_w = self.cached_chat_width.get();
        let cached_st = self.cached_show_thought.get();
        let mut cached_lines = self.cached_message_lines.borrow_mut();

        if cached_w != width || cached_st != self.show_thought {
            // Full recompute.
            self.cached_chat_width.set(width);
            self.cached_show_thought.set(self.show_thought);
            cached_lines.clear();
            let mut total = 0;
            for msg in &self.messages {
                let lines = count_single_message_lines(msg, width, self.show_thought);
                cached_lines.push(lines);
                total += lines;
            }
            self.cached_total_message_lines.set(total);
        } else if cached_lines.len() < self.messages.len() {
            // Incremental append (new messages added).
            let start = cached_lines.len();
            let mut total = self.cached_total_message_lines.get();
            for msg in &self.messages[start..] {
                let lines = count_single_message_lines(msg, width, self.show_thought);
                cached_lines.push(lines);
                total += lines;
            }
            self.cached_total_message_lines.set(total);
        } else if cached_lines.len() > self.messages.len() {
            // Truncate (messages removed).
            cached_lines.truncate(self.messages.len());
            let total: usize = cached_lines.iter().sum();
            self.cached_total_message_lines.set(total);
        }
    }

    /// Recompute only the last message's line count (used when a `Marmennill:`
    /// message is appended to during steer streaming) and update the total
    /// (reference §11.4).
    fn invalidate_last_message_cache(&self) {
        let mut cached_lines = self.cached_message_lines.borrow_mut();
        if !cached_lines.is_empty() && cached_lines.len() == self.messages.len() {
            let last_idx = cached_lines.len() - 1;
            let old = cached_lines[last_idx];
            let width = self.cached_chat_width.get();
            let new_lines =
                count_single_message_lines(&self.messages[last_idx], width, self.show_thought);
            cached_lines[last_idx] = new_lines;
            let total = self.cached_total_message_lines.get().saturating_sub(old) + new_lines;
            self.cached_total_message_lines.set(total);
        }
    }

    /// Estimate the total wrapped line count of the chat transcript at `width`
    /// (reference §11.5), including streaming buffers.
    fn estimated_chat_lines(&self, width: usize) -> usize {
        self.ensure_message_cache(width);
        let mut n = self.cached_total_message_lines.get();
        if self.show_thought && !self.current_thought.is_empty() {
            n += count_single_message_lines(&self.current_thought, width, self.show_thought);
        }
        if !self.current_content.is_empty() {
            n += count_single_message_lines(&self.current_content, width, self.show_thought);
        }
        if !self.steer_sentence_buffer.is_empty() {
            let prefix = if self
                .messages
                .last()
                .is_some_and(|m| m.starts_with("Marmennill: "))
            {
                ""
            } else {
                "Marmennill: "
            };
            let formatted = format!("{prefix}{}", self.steer_sentence_buffer);
            n += wrapped_lines(&formatted, width);
        }
        n
    }

    /// Estimate the total wrapped line count of the selected subagent's
    /// details pane at `width` (reference §11.6).
    fn estimated_subagent_lines(&self, width: usize) -> usize {
        if let Some(sa) = self.subagents.get(self.selected_subagent_idx) {
            let mut n = 1; // "=== Details for ... ==="
            if self.show_thought && !sa.thinking.is_empty() {
                n += 2; // "[Thinking]", " thinking"
                for line in sa.thinking.lines() {
                    n += if line.is_empty() {
                        1
                    } else {
                        wrapped_lines(line, width)
                    };
                }
                n += 1; // " response"
            }
            if self.show_thought && !sa.content.is_empty() {
                n += 1; // "[Output]"
                for line in sa.content.lines() {
                    n += if line.is_empty() {
                        1
                    } else {
                        wrapped_lines(line, width)
                    };
                }
            }
            if !sa.logs.is_empty() {
                n += 1; // "[Logs]"
                for log in &sa.logs {
                    let log_line = format!("- {log}");
                    n += if log_line.is_empty() {
                        1
                    } else {
                        wrapped_lines(&log_line, width)
                    };
                }
            }
            n
        } else {
            1
        }
    }

    /// Draw one complete frame using the current buffer state.
    fn draw(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        // Refresh the plan content from disk (live check-off updates).
        let plan_path = std::path::Path::new(crate::agent::phase::MARMEL_DIR)
            .join(crate::agent::phase::PLAN_FILE);
        let (plan, is_archived, has_active_plan) = if plan_path.exists() {
            let content = std::fs::read_to_string(&plan_path)
                .unwrap_or_else(|_| "Error reading execution plan.".to_string());
            let active = !content.trim().is_empty();
            (content, false, active)
        } else {
            let archive = std::path::Path::new(crate::agent::phase::MARMEL_DIR)
                .join("execution_plan_archive.md");
            if archive.exists() {
                (
                    format!(
                        "[Plan completed & archived to .marmel/archive/]\n\n{}",
                        std::fs::read_to_string(archive).unwrap_or_default()
                    ),
                    true,
                    false,
                )
            } else {
                (
                    "No active execution plan on disk.".to_string(),
                    false,
                    false,
                )
            }
        };
        self.plan_content = plan.clone();

        // Auto-open plan panel when a plan appears; keep it visible so user can always see plan progress.
        if (has_active_plan || is_archived) && !self.had_active_plan {
            self.show_plan_panel = true;
            self.plan_scroll = 0;
            self.plan_auto_scroll = true;
            self.had_active_plan = true;
        }

        // Auto-scroll targets (reference §11.5 / §11.6).
        if self.chat_auto_scroll {
            let w = self.chat_width.get();
            let n = self.estimated_chat_lines(w);
            let h = self.chat_height.get();
            self.chat_scroll = n.saturating_sub(h) as u16;
        }

        if self.subagent_autoscroll {
            let w = self.subagent_width.get();
            let n = self.estimated_subagent_lines(w);
            let h = self.subagent_height.get();
            self.subagent_scroll = n.saturating_sub(h) as u16;
        } else {
            let w = self.subagent_width.get();
            let n = self.estimated_subagent_lines(w);
            let h = self.subagent_height.get();
            let max_scroll = n.saturating_sub(h) as u16;
            if self.subagent_scroll > max_scroll {
                self.subagent_scroll = max_scroll;
            }
        }

        let mut chat_area = Rect::default();
        let mut plan_area = Rect::default();
        let mut subagent_area = Rect::default();

        terminal.draw(|frame| {
            let size = frame.area();

            // 3.1 Vertical split: Main (Min 3) + Status (Length 1) + Input (Length 3).
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(1),
                    Constraint::Length(3),
                ])
                .split(size);
            let main_area = chunks[0];
            let status_area = chunks[1];
            let input_area = chunks[2];

            // 3.2 Horizontal split: Chat (60%) + right panel (40%).
            let has_plan = !self.plan_content.trim().is_empty() && self.show_plan_panel;
            let has_subagents = self.show_subagent_panel && !self.subagents.is_empty();

            let (chat_rect, right_area) = if has_plan || has_subagents {
                let main_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .split(main_area);
                (main_chunks[0], Some(main_chunks[1]))
            } else {
                (main_area, None)
            };

            // 3.3 Right panel split: Plan (top 50%) + Subagents (bottom 50%).
            let mut plan_rect = None;
            let mut subagent_rect = None;
            if let Some(r_area) = right_area {
                if has_plan && has_subagents {
                    let right_chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(r_area);
                    plan_rect = Some(right_chunks[0]);
                    subagent_rect = Some(right_chunks[1]);
                } else if has_plan {
                    plan_rect = Some(r_area);
                } else if has_subagents {
                    subagent_rect = Some(r_area);
                }
            }

            chat_area = chat_rect;
            if let Some(p) = plan_rect {
                plan_area = p;
            }
            if let Some(s) = subagent_rect {
                subagent_area = s;
            }

            self.render_chat(frame, chat_area);
            if let Some(p) = plan_rect {
                self.render_plan(frame, p, &plan, is_archived);
            }
            if let Some(s) = subagent_rect {
                self.render_subagents(frame, s);
            }
            self.render_status(frame, status_area);
            self.render_input(frame, input_area);
        })?;

        self.chat_area = chat_area;
        self.plan_area = plan_area;
        self.subagent_area = subagent_area;
        Ok(())
    }

    fn render_chat(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let chat_border_color = if self.focused_panel == FocusedPanel::Chat {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let chat_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(chat_border_color))
            .title(" Chat & Logs (Focus: Tab) ");

        let chat_w = area.width.saturating_sub(2) as usize;
        let chat_h = area.height.saturating_sub(2) as usize;
        self.chat_width.set(chat_w);
        self.chat_height.set(chat_h);

        let mut chat_lines = Vec::new();
        for msg in &self.messages {
            let (msg_style, has_special_style) = message_style(msg);
            let mut in_think = false;
            for raw_line in msg.lines() {
                let line = raw_line.replace('\t', "    ");
                let segments = parse_line_segments(&line, &mut in_think);
                for seg in segments {
                    match seg {
                        LineSegment::Thought(t) => {
                            if !self.show_thought {
                                continue;
                            }
                            let cleaned = format_terminal_math(t.trim());
                            if cleaned.is_empty() {
                                continue;
                            }
                            chat_lines.push(Line::from(Span::styled(
                                cleaned,
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::ITALIC),
                            )));
                        }
                        LineSegment::Content(c) => {
                            let trimmed = c.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let cleaned = format_terminal_math(trimmed);
                            if has_special_style {
                                chat_lines.push(Line::from(Span::styled(cleaned, msg_style)));
                            } else if trimmed.starts_with("[Tool Call] ")
                                || trimmed.starts_with("[Tool Result] ")
                            {
                                chat_lines.push(Line::from(Span::styled(
                                    cleaned,
                                    Style::default().fg(Color::Magenta),
                                )));
                            } else {
                                // Orchestrator / Model content: WHITE
                                chat_lines.push(Line::from(Span::styled(
                                    cleaned,
                                    Style::default().fg(Color::White),
                                )));
                            }
                        }
                    }
                }
            }
        }

        // Streaming thoughts (reference §4.4.1).
        if self.show_thought && !self.current_thought.is_empty() {
            let think_style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            for line in self.current_thought.lines() {
                let line = line.replace('\t', "    ");
                let trimmed = line.trim();
                if trimmed == "<think>"
                    || trimmed == "</think>"
                    || trimmed == "<thought>"
                    || trimmed == "</thought>"
                    || trimmed == "<think></think>"
                    || trimmed == "<thought></thought>"
                {
                    continue;
                }
                let cleaned = format_terminal_math(&strip_think_tags(&line));
                if cleaned.trim().is_empty() && !line.trim().is_empty() {
                    continue;
                }
                chat_lines.push(Line::from(Span::styled(cleaned, think_style)));
            }
        }

        // Streaming content (Orchestrator output - WHITE).
        if !self.current_content.is_empty() {
            for raw_line in self.current_content.lines() {
                let line = raw_line.replace('\t', "    ");
                let trimmed = line.trim();
                if trimmed == "<think>"
                    || trimmed == "</think>"
                    || trimmed == "<thought>"
                    || trimmed == "</thought>"
                    || trimmed == "<think></think>"
                    || trimmed == "<thought></thought>"
                {
                    continue;
                }
                let cleaned = format_terminal_math(&strip_think_tags(&line));
                if cleaned.trim().is_empty() && !line.trim().is_empty() {
                    continue;
                }
                chat_lines.push(Line::from(Span::styled(
                    cleaned,
                    Style::default().fg(Color::White),
                )));
            }
        }

        // Streaming steer sentence (Steer Arbitrator output - YELLOW BOLD).
        if !self.steer_sentence_buffer.is_empty() {
            let prefix = if self
                .messages
                .last()
                .is_some_and(|m| m.starts_with("Marmennill: ") || m.starts_with("Kvaser: "))
            {
                ""
            } else {
                "Marmennill: "
            };
            let formatted = format!("{prefix}{}", self.steer_sentence_buffer);
            let steer_style = Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD);
            for line in formatted.lines() {
                let line = line.replace('\t', "    ");
                chat_lines.push(Line::from(Span::styled(line, steer_style)));
            }
        }

        let chat_paragraph = Paragraph::new(chat_lines)
            .block(chat_block)
            .wrap(Wrap { trim: false })
            .scroll((self.chat_scroll, 0));
        frame.render_widget(chat_paragraph, area);

        // F4: vertical scrollbar on the chat pane.
        let total = self.estimated_chat_lines(chat_w);
        let max_scroll = total.saturating_sub(chat_h);
        if max_scroll > 0 {
            let inner = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            };
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll).position(self.chat_scroll as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                inner,
                &mut scrollbar_state,
            );
        }
    }

    fn render_plan(
        &mut self,
        frame: &mut ratatui::Frame,
        area: Rect,
        plan: &str,
        _is_archived: bool,
    ) {
        let plan_border_color = if self.focused_panel == FocusedPanel::Plan {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let plan_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(plan_border_color))
            .title(" Execution Plan ");

        let mut plan_lines = Vec::new();
        for raw_line in plan.lines() {
            let line = raw_line.replace('\t', "    ");
            let style = if line.starts_with("# ") || line.starts_with("## ") {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if line.contains("[x]") || line.contains("[X]") {
                Style::default().fg(Color::Green)
            } else if line.contains("[ ]") {
                Style::default().fg(Color::LightRed)
            } else {
                Style::default()
            };
            plan_lines.push(Line::styled(line, style));
        }

        let plan_w = area.width.saturating_sub(2) as usize;
        let plan_h = area.height.saturating_sub(2) as usize;
        self.plan_width.set(plan_w);
        self.plan_height.set(plan_h);

        let total_plan_lines = wrapped_lines(plan, plan_w.max(1));
        self.plan_max_scroll = total_plan_lines.saturating_sub(plan_h) as u16;
        let scroll_y = if self.plan_auto_scroll {
            let target_scroll = if let Some(first_pending_visual) =
                visual_line_offset_of_first_pending(plan, plan_w.max(1))
            {
                // Position the first pending task comfortably in view (leaving 1 line of context above if possible)
                let desired = first_pending_visual.saturating_sub(1);
                (desired as u16).min(self.plan_max_scroll)
            } else {
                // If all tasks are completed or no pending tasks, scroll to bottom to show completed progress
                self.plan_max_scroll
            };
            self.plan_scroll = target_scroll;
            target_scroll
        } else {
            self.plan_scroll.min(self.plan_max_scroll)
        };

        let paragraph = Paragraph::new(plan_lines)
            .block(plan_block)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0));
        frame.render_widget(paragraph, area);

        // F4: vertical scrollbar on the plan pane.
        if self.plan_max_scroll > 0 {
            let inner = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            };
            let mut scrollbar_state = ScrollbarState::new(self.plan_max_scroll as usize)
                .position(self.plan_scroll as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                inner,
                &mut scrollbar_state,
            );
        }
    }

    fn render_subagents(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let subagent_border_color = if self.focused_panel == FocusedPanel::Subagents {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let total = self.subagents.len();
        let title = if total == 0 {
            " Specialist Subagents (Ctrl-A | ◄/► Agent | ▲/▼ Scroll) ".to_string()
        } else {
            let current = (self.selected_subagent_idx + 1).min(total);
            format!(" Specialist Subagents [{current}/{total}] (Ctrl-A | ◄/► Agent | ▲/▼ Scroll) ")
        };

        let subagent_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(subagent_border_color))
            .title(title);

        let inner_area = subagent_block.inner(area);
        let list_len = (self.subagents.len() as u16)
            .clamp(1, 3)
            .min(inner_area.height.saturating_sub(3).max(1));
        let subagent_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(list_len), Constraint::Min(3)])
            .split(inner_area);

        // Subagent list.
        let mut list_lines = Vec::new();
        for (i, sa) in self.subagents.iter().enumerate() {
            let prefix = if i == self.selected_subagent_idx {
                "> "
            } else {
                "  "
            };
            let name_style = if sa.is_active {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let ctx_tokens = if sa.is_active {
                crate::orchestrator::get_active_worker_tokens(&sa.name).unwrap_or(sa.context_tokens)
            } else {
                sa.context_tokens
            };
            let ctx_info = if ctx_tokens > 0 {
                format!(", {} ctx", Self::format_count(ctx_tokens))
            } else {
                String::new()
            };
            let elapsed_str = if let Some(last) = sa.last_activity_at.or(sa.started_at) {
                let secs = last.elapsed().as_secs();
                if secs >= 60 {
                    format!(", {}m {}s ago", secs / 60, secs % 60)
                } else {
                    format!(", {}s ago", secs)
                }
            } else {
                String::new()
            };
            let status = if sa.is_active {
                Span::styled(
                    format!(" (Active{ctx_info}{elapsed_str})"),
                    Style::default().fg(Color::LightGreen),
                )
            } else {
                Span::styled(
                    format!(" (Idle{ctx_info}{elapsed_str})"),
                    Style::default().fg(Color::DarkGray),
                )
            };
            list_lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(sa.name.clone(), name_style),
                status,
            ]));
        }
        if list_lines.is_empty() {
            list_lines.push(Line::raw("No subagents."));
        }

        // Auto-adjust list scroll to keep the selected subagent visible
        // (reference §6.3).
        let list_height = subagent_chunks[0].height as usize;
        let mut list_scroll = self.subagent_list_scroll as usize;
        if list_height > 0 {
            if self.selected_subagent_idx < list_scroll {
                list_scroll = self.selected_subagent_idx;
            } else if self.selected_subagent_idx >= list_scroll + list_height {
                list_scroll = self.selected_subagent_idx + 1 - list_height;
            }
            let max_scroll = self.subagents.len().saturating_sub(list_height);
            if list_scroll > max_scroll {
                list_scroll = max_scroll;
            }
        } else {
            list_scroll = 0;
        }
        self.subagent_list_scroll = list_scroll as u16;

        let list_paragraph = Paragraph::new(list_lines)
            .wrap(Wrap { trim: false })
            .scroll((list_scroll as u16, 0));
        frame.render_widget(list_paragraph, subagent_chunks[0]);

        // Separator.
        let separator = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(separator, subagent_chunks[1]);

        // Subagent details (reference §6.4).
        let details_area = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(2)])
            .split(subagent_chunks[1])[1];

        let mut detail_lines = Vec::new();
        if let Some(sa) = self.subagents.get(self.selected_subagent_idx) {
            detail_lines.push(Line::styled(
                format!("=== Details for {} ===", sa.name),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));

            if self.show_thought && !sa.thinking.is_empty() {
                detail_lines.push(Line::styled(
                    "[Thinking]",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
                let think_style = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                detail_lines.push(Line::styled(" thinking", think_style));
                for line in sa.thinking.lines() {
                    let line = format_terminal_math(line);
                    detail_lines.push(Line::styled(line, think_style));
                }
                detail_lines.push(Line::styled(" response", think_style));
            }

            if self.show_thought && !sa.content.is_empty() {
                detail_lines.push(Line::styled(
                    "[Output]",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                for line in sa.content.lines() {
                    let line = format_terminal_math(line);
                    detail_lines.push(Line::raw(line));
                }
            }

            if !sa.logs.is_empty() {
                detail_lines.push(Line::styled(
                    "[Logs]",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
                for log in &sa.logs {
                    detail_lines.push(Line::raw(format!("- {log}")));
                }
            }
        } else {
            detail_lines.push(Line::raw("No subagents active or selected."));
        }

        self.subagent_width
            .set(details_area.width.saturating_sub(1) as usize);
        self.subagent_height.set(details_area.height as usize);

        let detail_paragraph = Paragraph::new(detail_lines)
            .wrap(Wrap { trim: false })
            .scroll((self.subagent_scroll, 0));
        frame.render_widget(detail_paragraph, details_area);

        // F4: vertical scrollbar on the subagent-details pane.
        let w = self.subagent_width.get();
        let total = self.estimated_subagent_lines(w);
        let h = self.subagent_height.get();
        let max_scroll = total.saturating_sub(h);
        if max_scroll > 0 {
            let inner = Rect {
                x: details_area.x,
                y: details_area.y,
                width: details_area.width,
                height: details_area.height,
            };
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll).position(self.subagent_scroll as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                inner,
                &mut scrollbar_state,
            );
        }

        frame.render_widget(subagent_block, area);
    }

    fn format_count(n: usize) -> String {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            format!("{n}")
        }
    }

    fn format_token_counts(tokens_in: usize, tokens_out: usize) -> String {
        let total = tokens_in.saturating_add(tokens_out);
        format!(
            "{} in / {} out ({} total)",
            Self::format_count(tokens_in),
            Self::format_count(tokens_out),
            Self::format_count(total)
        )
    }

    fn render_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        // F8: always surface the live running-subagent count, independent of
        // `status_line`, whenever any specialist is active.
        let active_agents: Vec<&str> = self
            .subagents
            .iter()
            .filter(|s| s.is_active)
            .map(|s| s.name.as_str())
            .collect();
        let mut status_str = self.status_line.clone();
        if !active_agents.is_empty() {
            let count_suffix = format!(
                " [{} active: {}]",
                active_agents.len(),
                active_agents.join(", ")
            );
            status_str.push_str(&count_suffix);
        }
        // F3: animated activity indicator. When the status line indicates an
        // active phase, append a cycling suffix derived from `frame_counter`.
        let active_phase = [
            "Running",
            "Delegating",
            "calling backend",
            "Starting",
            "streaming",
        ]
        .iter()
        .any(|k| status_str.contains(k));
        if let Some(start) = self.waiting_for_token_since {
            let elapsed = start.elapsed().as_secs_f32();
            status_str.push_str(&format!(" ({:.1}s)", elapsed));
        }
        if active_phase {
            let frames = ["…", "..", "."];
            let idx = (self.frame_counter % frames.len() as u64) as usize;
            status_str.push(' ');
            status_str.push_str(frames[idx]);
        }
        let (global_in, global_out) = crate::llm::get_global_token_counts();
        let tokens_in = self.tokens_in.max(global_in);
        let tokens_out = self.tokens_out.max(global_out);
        let tokens_str = Self::format_token_counts(tokens_in, tokens_out);
        let (status_text, status_style) = if self.confirm_abort {
            (
                " ⚠ [ABORT ARMED] Press ESC or Ctrl+C again to abort | Any other key to cancel "
                    .to_string(),
                Style::default()
                    .bg(Color::Red)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            let status_text = if self.orchestrator_context_tokens > 0 {
                let ctx_str = Self::format_count(self.orchestrator_context_tokens);
                format!(
                    " Ctx: {} | Tokens: {} | Status: {}",
                    ctx_str, tokens_str, status_str
                )
            } else {
                format!(" Tokens: {} | Status: {}", tokens_str, status_str)
            };
            (
                status_text,
                Style::default().bg(Color::DarkGray).fg(Color::White),
            )
        };
        let status_paragraph = Paragraph::new(status_text).style(status_style);
        frame.render_widget(status_paragraph, area);
    }

    fn render_input(&self, frame: &mut ratatui::Frame, area: Rect) {
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let max_text_width = area.width.saturating_sub(2) as usize;
        let cursor_char_idx = self.input_text[..self.cursor.min(self.input_text.len())]
            .chars()
            .count();

        // Calculate a sliding window (scroll_offset) that keeps the cursor visible
        let scroll_offset = if cursor_char_idx < max_text_width {
            0
        } else {
            cursor_char_idx + 1 - max_text_width
        };

        let visible_text: String = self
            .input_text
            .chars()
            .skip(scroll_offset)
            .take(max_text_width)
            .collect();

        let visible_cursor_offset = cursor_char_idx.saturating_sub(scroll_offset) as u16;
        let cursor_pos_x = area.x + 1 + visible_cursor_offset;

        let input_paragraph = Paragraph::new(visible_text.as_str()).block(input_block);
        frame.render_widget(input_paragraph, area);
        frame.set_cursor_position((cursor_pos_x, area.y + 1));
    }
}

/// Choose the message-level style by prefix matching (reference §4.3).
/// Returns `(style, has_special_style)`.
fn message_style(msg: &str) -> (Style, bool) {
    let first = msg.lines().next().unwrap_or("");
    if first.starts_with("--- Turn") || first.starts_with("=== Turn") {
        (
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            true,
        )
    } else if first.starts_with("[Status] ") || first.starts_with("[CLI] ") {
        (Style::default().fg(Color::Yellow), true)
    } else if first.starts_with("System Error: ") {
        (
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            true,
        )
    } else if first.starts_with("User: ") || first.starts_with("User (Steer): ") {
        (
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            true,
        )
    } else if first.starts_with("Marmennill: ")
        || first.starts_with("Kvaser: ")
        || first.starts_with("[Steer Arbitrator]")
    {
        (
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            true,
        )
    } else {
        // Orchestrator / Model content defaults to white text.
        (Style::default().fg(Color::White), false)
    }
}

/// Segment of a rendered line representing either reasoning thought or visible content.
#[derive(Debug, PartialEq, Eq)]
enum LineSegment<'a> {
    Thought(&'a str),
    Content(&'a str),
}

/// Parse segments of a line according to the running `in_think` state, tracking transitions across `<think>` and `</think>` tags.
fn parse_line_segments<'a>(line: &'a str, in_think: &mut bool) -> Vec<LineSegment<'a>> {
    let mut segments = Vec::new();
    let mut cursor = 0;
    while cursor < line.len() {
        if *in_think {
            // Looking for close tag: </think> or </thought>
            let close_tag = [("</think>", 8), ("</thought>", 10)]
                .iter()
                .filter_map(|(tag, len)| line[cursor..].find(tag).map(|idx| (cursor + idx, *len)))
                .min_by_key(|(idx, _)| *idx);

            if let Some((idx, len)) = close_tag {
                let thought_part = &line[cursor..idx];
                if !thought_part.is_empty() {
                    segments.push(LineSegment::Thought(thought_part));
                }
                *in_think = false;
                cursor = idx + len;
            } else {
                let thought_part = &line[cursor..];
                if !thought_part.is_empty() {
                    segments.push(LineSegment::Thought(thought_part));
                }
                break;
            }
        } else {
            // Looking for open tag: <think> or <thought>
            let open_tag = [("<think>", 7), ("<thought>", 9)]
                .iter()
                .filter_map(|(tag, len)| line[cursor..].find(tag).map(|idx| (cursor + idx, *len)))
                .min_by_key(|(idx, _)| *idx);

            if let Some((idx, len)) = open_tag {
                let content_part = &line[cursor..idx];
                if !content_part.is_empty() {
                    segments.push(LineSegment::Content(content_part));
                }
                *in_think = true;
                cursor = idx + len;
            } else {
                let content_part = &line[cursor..];
                if !content_part.is_empty() {
                    segments.push(LineSegment::Content(content_part));
                }
                break;
            }
        }
    }
    segments
}

/// Strip XML thinking tags from a rendered line.
fn strip_think_tags(line: &str) -> String {
    line.replace("<think>", "")
        .replace("</think>", "")
        .replace("<thought>", "")
        .replace("</thought>", "")
}

/// Format LaTeX math expressions ($$...$$, $...$, and common math symbols)
/// into clean, readable terminal Unicode text.
pub(crate) fn format_terminal_math(text: &str) -> String {
    let mut s = text.to_string();
    if !s.contains('$') && !s.contains('\\') && !s.contains('^') && !s.contains('_') {
        return s;
    }

    // Replace display math $$...$$
    while let Some(start) = s.find("$$") {
        if let Some(end_rel) = s[start + 2..].find("$$") {
            let end = start + 2 + end_rel;
            let inner = &s[start + 2..end];
            let formatted = format_math_expr(inner.trim());
            s.replace_range(start..end + 2, &format!("  {formatted}"));
        } else {
            let inner = &s[start + 2..];
            let formatted = format_math_expr(inner.trim());
            s.replace_range(start.., &format!("  {formatted}"));
            break;
        }
    }

    // Replace inline math $...$
    let mut i = 0;
    while i < s.len() {
        if let Some(start_rel) = s[i..].find('$') {
            let start = i + start_rel;
            if start + 1 < s.len() && s.as_bytes()[start + 1] == b'$' {
                i = start + 2;
                continue;
            }
            if let Some(end_rel) = s[start + 1..].find('$') {
                let end = start + 1 + end_rel;
                let inner = &s[start + 1..end];
                if !inner.trim().is_empty()
                    && !inner
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
                {
                    let formatted = format_math_expr(inner);
                    s.replace_range(start..end + 1, &formatted);
                    i = start + formatted.len();
                    continue;
                }
                i = end + 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    format_math_expr(&s)
}

fn format_math_expr(expr: &str) -> String {
    let mut out = expr
        .replace(r"\|", "|")
        .replace(r"\cdot", "·")
        .replace(r"\times", "×")
        .replace(r"\pm", "±")
        .replace(r"\mp", "∓")
        .replace(r"\leq", "≤")
        .replace(r"\le", "≤")
        .replace(r"\geq", "≥")
        .replace(r"\ge", "≥")
        .replace(r"\neq", "≠")
        .replace(r"\ne", "≠")
        .replace(r"\approx", "≈")
        .replace(r"\to", "→")
        .replace(r"\rightarrow", "→")
        .replace(r"\leftarrow", "←")
        .replace(r"\dots", "…")
        .replace(r"\cdots", "…")
        .replace(r"\quad", "  ")
        .replace(r"\qquad", "    ")
        .replace(r"\alpha", "α")
        .replace(r"\beta", "β")
        .replace(r"\gamma", "γ")
        .replace(r"\theta", "θ")
        .replace(r"\lambda", "λ")
        .replace(r"\mu", "μ")
        .replace(r"\pi", "π")
        .replace(r"\sigma", "σ")
        .replace(r"\omega", "ω")
        .replace(r"\Delta", "Δ")
        .replace(r"\sum", "∑")
        .replace(r"\prod", "∏")
        .replace(r"\int", "∫")
        .replace(r"\infty", "∞");

    // Replace superscripts
    out = out
        .replace("^2", "²")
        .replace("^3", "³")
        .replace("^0", "⁰")
        .replace("^1", "¹")
        .replace("^4", "⁴")
        .replace("^5", "⁵")
        .replace("^6", "⁶")
        .replace("^7", "⁷")
        .replace("^8", "⁸")
        .replace("^9", "⁹")
        .replace("^{+}", "⁺")
        .replace("^{-}", "⁻")
        .replace("^{2}", "²")
        .replace("^{3}", "³")
        .replace("^{0}", "⁰")
        .replace("^{1}", "¹")
        .replace("^{n}", "ⁿ")
        .replace("^{t}", "ᵗ")
        .replace("^{T}", "ᵀ")
        .replace("^n", "ⁿ")
        .replace("^t", "ᵗ")
        .replace("^T", "ᵀ");

    // Replace subscripts
    out = out
        .replace("_{0}", "₀")
        .replace("_{1}", "₁")
        .replace("_{2}", "₂")
        .replace("_{3}", "₃")
        .replace("_{4}", "₄")
        .replace("_{5}", "₅")
        .replace("_{6}", "₆")
        .replace("_{7}", "₇")
        .replace("_{8}", "₈")
        .replace("_{9}", "₉")
        .replace("_{i}", "ᵢ")
        .replace("_{n}", "ₙ")
        .replace("_{x}", "ₓ")
        .replace("_{y}", "ᵧ")
        .replace("_{z}", "₂")
        .replace("_0", "₀")
        .replace("_1", "₁")
        .replace("_2", "₂")
        .replace("_3", "₃")
        .replace("_4", "₄")
        .replace("_5", "₅")
        .replace("_6", "₆")
        .replace("_7", "₇")
        .replace("_8", "₈")
        .replace("_9", "₉")
        .replace("_i", "ᵢ")
        .replace("_n", "ₙ")
        .replace("_x", "ₓ")
        .replace("_y", "ᵧ");

    while let Some(start) = out.find(r"\text{") {
        if let Some(end_rel) = out[start + 6..].find('}') {
            let end = start + 6 + end_rel;
            let inner = out[start + 6..end].to_string();
            out.replace_range(start..end + 1, &inner);
        } else {
            break;
        }
    }

    while let Some(start) = out.find(r"\sqrt{") {
        if let Some(end_rel) = out[start + 6..].find('}') {
            let end = start + 6 + end_rel;
            let inner = out[start + 6..end].to_string();
            out.replace_range(start..end + 1, &format!("√({inner})"));
        } else {
            break;
        }
    }

    while let Some(start) = out.find(r"\frac{") {
        if let Some(mid_rel) = out[start + 6..].find("}{") {
            let mid = start + 6 + mid_rel;
            if let Some(end_rel) = out[mid + 2..].find('}') {
                let end = mid + 2 + end_rel;
                let num = out[start + 6..mid].to_string();
                let den = out[mid + 2..end].to_string();
                out.replace_range(start..end + 1, &format!("({num})/({den})"));
                continue;
            }
        }
        break;
    }

    out
}

/// Count the wrapped lines a single message occupies at `width`, excluding
/// think-block lines when `show_thought` is `false` (reference §11.2).
fn count_single_message_lines(msg: &str, width: usize, show_thought: bool) -> usize {
    let mut n = 0;
    let mut in_think = false;
    for line in msg.lines() {
        let segments = parse_line_segments(line, &mut in_think);
        for seg in segments {
            match seg {
                LineSegment::Thought(t) => {
                    if !show_thought {
                        continue;
                    }
                    let cleaned = format_terminal_math(t.trim());
                    if !cleaned.is_empty() {
                        n += wrapped_lines(&cleaned, width);
                    }
                }
                LineSegment::Content(c) => {
                    let cleaned = format_terminal_math(c.trim());
                    if !cleaned.is_empty() {
                        n += wrapped_lines(&cleaned, width);
                    }
                }
            }
        }
    }
    n
}

/// Grapheme-aware word-wrap line counter (reference §11.1).
///
/// - `width == 0` → returns the raw line count (no wrapping).
/// - Tabs → 4 spaces; trailing spaces trimmed.
/// - Zero-width space `\u{200B}` is treated as whitespace; non-breaking space
///   `\u{00A0}` is **not** treated as whitespace.
/// - Word-wrap breaks when `line_width >= max_line_width` or when a pending
///   word would overflow.
/// - Each raw line contributes at least 1 wrapped line.
fn wrapped_lines(text: &str, width: usize) -> usize {
    if width == 0 {
        return text.lines().count();
    }

    let mut total_lines = 0;

    for raw_line in text.lines() {
        let line = raw_line.replace('\t', "    ");
        let line = line.trim_end_matches(' ');

        let mut line_symbols = Vec::new();
        for grapheme in line.graphemes(true) {
            let is_whitespace = grapheme == "\u{200B}"
                || (grapheme.chars().all(char::is_whitespace) && grapheme != "\u{00A0}");
            let symbol_width = grapheme.width() as u16;
            line_symbols.push((is_whitespace, symbol_width));
        }

        let mut wrapped_lines_count = 0;
        let mut pending_line_empty = true;
        let mut line_width = 0;

        let mut pending_word_width = 0;
        let mut pending_word_len = 0;

        let mut pending_whitespace_widths = std::collections::VecDeque::new();
        let mut whitespace_width = 0;

        let mut non_whitespace_previous = false;
        let max_line_width = width as u16;
        let trim = false;

        for (is_whitespace, symbol_width) in line_symbols {
            if symbol_width > max_line_width {
                continue;
            }

            let word_found = non_whitespace_previous && is_whitespace;
            let trimmed_overflow =
                pending_line_empty && trim && pending_word_width + symbol_width > max_line_width;
            let whitespace_overflow =
                pending_line_empty && trim && whitespace_width + symbol_width > max_line_width;
            let untrimmed_overflow = pending_line_empty
                && !trim
                && pending_word_width + whitespace_width + symbol_width > max_line_width;

            if word_found || trimmed_overflow || whitespace_overflow || untrimmed_overflow {
                if !pending_line_empty || !trim {
                    line_width += whitespace_width;
                }
                line_width += pending_word_width;
                pending_line_empty = false;

                pending_whitespace_widths.clear();
                whitespace_width = 0;
                pending_word_width = 0;
                pending_word_len = 0;
            }

            let line_full = line_width >= max_line_width;
            let pending_word_overflow = symbol_width > 0
                && line_width + whitespace_width + pending_word_width >= max_line_width;

            if line_full || pending_word_overflow {
                let mut remaining_width = max_line_width.saturating_sub(line_width);
                wrapped_lines_count += 1;
                line_width = 0;
                pending_line_empty = true;

                while let Some(&w) = pending_whitespace_widths.front() {
                    if w > remaining_width {
                        break;
                    }
                    whitespace_width -= w;
                    remaining_width -= w;
                    pending_whitespace_widths.pop_front();
                }

                if is_whitespace && pending_whitespace_widths.is_empty() {
                    continue;
                }
            }

            if is_whitespace {
                whitespace_width += symbol_width;
                pending_whitespace_widths.push_back(symbol_width);
            } else {
                pending_word_width += symbol_width;
                pending_word_len += 1;
            }
            non_whitespace_previous = !is_whitespace;
        }

        let mut final_pending_exists = false;
        if pending_line_empty && pending_word_len == 0 && !pending_whitespace_widths.is_empty() {
            wrapped_lines_count += 1;
            final_pending_exists = true;
        }
        if !final_pending_exists {
            let mut final_line_empty = pending_line_empty;
            if !pending_line_empty || (!trim && !pending_whitespace_widths.is_empty()) {
                final_line_empty = false;
            }
            if pending_word_len > 0 {
                final_line_empty = false;
            }
            if !final_line_empty {
                wrapped_lines_count += 1;
            }
        }

        if wrapped_lines_count == 0 {
            wrapped_lines_count = 1;
        }

        total_lines += wrapped_lines_count;
    }
    total_lines
}

/// Compute the visual (wrapped) line offset of the first uncompleted task checkbox (`- [ ]`)
/// in the execution plan text. Returns `None` if no uncompleted task is present.
fn visual_line_offset_of_first_pending(text: &str, width: usize) -> Option<usize> {
    static UNCHECKED_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = UNCHECKED_RE.get_or_init(|| {
        regex::Regex::new(r"^\s*(?:[-*]|\d+\.)?\s*(\[\s*\]|\(\s*\))")
            .expect("valid unchecked regex")
    });
    let mut visual_offset = 0;
    for raw_line in text.lines() {
        if re.is_match(raw_line) {
            return Some(visual_offset);
        }
        visual_offset += wrapped_lines(raw_line, width).max(1);
    }
    None
}

/// Compute a centered rectangle of `percent_x`% width and `percent_y`% height
/// within `r` (reference §9.1).
#[allow(dead_code)]
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Extract complete sentences from a streaming buffer (reference §10).
///
/// Splits on newlines or on sentence-ending punctuation (`.`, `!`, `?`)
/// followed by whitespace. Trailing punctuation and delimiters are preserved
/// in the extracted chunk so paragraph breaks and spaces survive.
fn extract_complete_sentences(buffer: &mut String) -> Vec<String> {
    let mut sentences = Vec::new();
    loop {
        let mut split_idx = None;
        let bytes = buffer.as_bytes();
        let len = bytes.len();

        for i in 0..len {
            let b = bytes[i];
            if b == b'\n' {
                let mut next_start = i + 1;
                while next_start < len && bytes[next_start] == b'\n' {
                    next_start += 1;
                }
                split_idx = Some((next_start, next_start));
                break;
            }
            if (b == b'.' || b == b'!' || b == b'?') && i + 1 < len {
                let next = bytes[i + 1];
                if next == b' ' || next == b'\t' || next == b'\n' {
                    let mut end_punc = i;
                    while end_punc + 1 < len
                        && (bytes[end_punc + 1] == b'.'
                            || bytes[end_punc + 1] == b'!'
                            || bytes[end_punc + 1] == b'?')
                    {
                        end_punc += 1;
                    }
                    if end_punc + 1 < len
                        && (bytes[end_punc + 1] == b' '
                            || bytes[end_punc + 1] == b'\t'
                            || bytes[end_punc + 1] == b'\n')
                    {
                        let mut next_start = end_punc + 1;
                        while next_start < len
                            && (bytes[next_start] == b' '
                                || bytes[next_start] == b'\t'
                                || bytes[next_start] == b'\n')
                        {
                            next_start += 1;
                        }
                        split_idx = Some((next_start, next_start));
                        break;
                    }
                }
            }
        }

        if let Some((cut_end, next_start)) = split_idx {
            let sentence = buffer[..cut_end].to_string();
            buffer.drain(..next_start);
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
        } else {
            break;
        }
    }
    sentences
}

fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)
}

impl Default for TuiRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer for TuiRenderer {
    fn init(&mut self) -> Result<()> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        self.terminal = Some(Terminal::new(backend)?);

        let plan = crate::agent::phase::Plan::default();
        if let Ok(Some(content)) = plan.read()
            && !content.trim().is_empty()
        {
            self.plan_content = content;
            self.show_plan_panel = true;
            self.had_active_plan = true;
        }

        self.flush()?;
        Ok(())
    }

    fn on_event(&mut self, event: &Event) {
        match event {
            Event::TokensIn(count) => {
                self.orchestrator_context_tokens = *count;
                self.tokens_in = self.tokens_in.saturating_add(*count);
            }
            Event::TokensOut(count) => {
                self.tokens_out = self.tokens_out.saturating_add(*count);
            }
            Event::Message(text) => {
                self.waiting_for_token_since = None;
                let tok_count = tiktoken_rs::cl100k_base_singleton()
                    .encode_ordinary(text)
                    .len();
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                if self.active_agent == "Manager" {
                    self.current_content.push_str(text);
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                } else if let Some(sa) = self
                    .subagents
                    .iter_mut()
                    .find(|s| s.name == self.active_agent)
                {
                    sa.content.push_str(text);
                    sa.last_activity_at = Some(std::time::Instant::now());
                }
            }
            Event::SteerResponse(text) => {
                self.waiting_for_token_since = None;
                let tok_count = tiktoken_rs::cl100k_base_singleton()
                    .encode_ordinary(text)
                    .len();
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                // Stream steer arbitrator output as `Marmennill: ` yellow sentences.
                self.steer_sentence_buffer.push_str(text);
                let sentences = extract_complete_sentences(&mut self.steer_sentence_buffer);
                for s in sentences {
                    self.append_steer_sentence(&s);
                }
                if self.chat_auto_scroll {
                    let w = self.chat_width.get();
                    let n = self.estimated_chat_lines(w);
                    let h = self.chat_height.get();
                    self.chat_scroll = n.saturating_sub(h) as u16;
                }
            }
            Event::Thinking(text) => {
                self.waiting_for_token_since = None;
                let tok_count = tiktoken_rs::cl100k_base_singleton()
                    .encode_ordinary(text)
                    .len();
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                if self.active_agent == "Manager" {
                    self.current_thought.push_str(text);
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                } else if let Some(sa) = self
                    .subagents
                    .iter_mut()
                    .find(|s| s.name == self.active_agent)
                {
                    sa.thinking.push_str(text);
                    sa.last_activity_at = Some(std::time::Instant::now());
                }
            }
            Event::ToolCall(text) => {
                self.waiting_for_token_since = None;
                let tok_count = tiktoken_rs::cl100k_base_singleton()
                    .encode_ordinary(text)
                    .len();
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                self.commit_turn_content();
                if self.active_agent != "Manager"
                    && let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                {
                    let log_entry = text.clone();
                    if sa.logs.last().map(String::as_str) != Some(log_entry.as_str()) {
                        sa.logs.push(log_entry);
                    }
                    sa.last_activity_at = Some(std::time::Instant::now());
                } else {
                    self.messages.push(format!("[Tool Call] {text}"));
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
            }
            Event::ToolResult(text) => {
                self.waiting_for_token_since = None;
                self.commit_turn_content();
                if self.active_agent == "Manager" {
                    self.messages.push(format!("[Tool Result] {text}"));
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
            }
            Event::Status(text) => {
                if !self.steer_sentence_buffer.trim().is_empty() {
                    let remaining = std::mem::take(&mut self.steer_sentence_buffer);
                    self.append_steer_sentence(remaining.trim());
                }
                self.status_line = text.lines().next().unwrap_or("").to_string();
                let is_waiting = self.status_line.contains("Running")
                    || self.status_line.contains("thinking")
                    || self.status_line.contains("calling backend")
                    || self.status_line.contains("Arbitrating");
                if is_waiting {
                    if self.waiting_for_token_since.is_none() {
                        self.waiting_for_token_since = Some(std::time::Instant::now());
                    }
                } else if self.status_line.contains("Ready") {
                    self.waiting_for_token_since = None;
                }
                if text.starts_with("[CLI]")
                    || text.starts_with("[Validator] APPROVED")
                    || text.starts_with("System Error")
                    || text.starts_with("LLM error")
                {
                    for line in text.lines() {
                        self.messages.push(line.to_string());
                    }
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
                // Live-track logs for subagents: match by agent name prefix (e.g. "coder-t-001: ...")
                // or fallback to active_agent when multiple specialists run in parallel.
                let mut routed = false;
                for sa in &mut self.subagents {
                    if text.starts_with(&format!("{}:", sa.name))
                        || text.starts_with(&format!("{} ", sa.name))
                        || text.contains(&format!("for {}", sa.name))
                    {
                        let clean = if let Some(stripped) =
                            text.strip_prefix(&format!("{}: ", sa.name))
                        {
                            stripped.to_string()
                        } else if let Some(stripped) = text.strip_prefix(&format!("{}:", sa.name)) {
                            stripped.trim_start().to_string()
                        } else {
                            text.clone()
                        };
                        if sa.logs.last().map(String::as_str) != Some(clean.as_str()) {
                            sa.logs.push(clean);
                        }
                        sa.last_activity_at = Some(std::time::Instant::now());
                        routed = true;
                        break;
                    }
                }
                if !routed
                    && self.active_agent != "Manager"
                    && let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                {
                    let clean = if let Some(stripped) = text.strip_prefix(&format!("{}: ", sa.name))
                    {
                        stripped.to_string()
                    } else if let Some(stripped) = text.strip_prefix(&format!("{}:", sa.name)) {
                        stripped.trim_start().to_string()
                    } else {
                        text.clone()
                    };
                    if sa.logs.last().map(String::as_str) != Some(clean.as_str()) {
                        sa.logs.push(clean);
                    }
                    sa.last_activity_at = Some(std::time::Instant::now());
                }
            }
            Event::Delegation(de) => {
                self.commit_turn_content();
                match de {
                    crate::orchestrator::DelegationEvent::Started { agent, task } => {
                        let name = match task {
                            Some(t) if !t.trim().is_empty() => format!("{agent}-{t}"),
                            _ => format!("{agent}"),
                        };
                        self.active_agent = name.clone();
                        let t = task.as_deref().unwrap_or("(no task id)");
                        // Fold into the local subagent list so the panel stays
                        // live even before the session loop pushes the
                        // authoritative list (t-c304).
                        self.upsert_subagent(&name, true, &format!("started task {t}"));
                        self.status_line = format!("Delegating to specialist {name} for {t}");
                    }
                    crate::orchestrator::DelegationEvent::Completed { agent, task } => {
                        let name = match task {
                            Some(t) if !t.trim().is_empty() => format!("{agent}-{t}"),
                            _ => format!("{agent}"),
                        };
                        let t = task.as_deref().unwrap_or("(no task id)");
                        self.upsert_subagent(&name, false, &format!("completed task {t}"));
                        let remaining_active: Vec<&str> = self
                            .subagents
                            .iter()
                            .filter(|s| s.is_active && s.name != name)
                            .map(|s| s.name.as_str())
                            .collect();
                        if !remaining_active.is_empty() {
                            self.active_agent = remaining_active[0].to_string();
                            self.status_line = format!(
                                "{name} finished {t}. Active: {}",
                                remaining_active.join(", ")
                            );
                        } else {
                            self.active_agent = "Manager".to_string();
                            self.status_line = format!("{name} finished task {t}");
                        }
                    }
                };
            }
            Event::Done => {
                self.commit_turn_content();
            }
        }
        let handled = self.handle_events(false);
        let now = std::time::Instant::now();
        if handled
            || now.duration_since(self.last_render) >= Duration::from_millis(16)
            || matches!(
                event,
                Event::Done
                    | Event::ToolCall(_)
                    | Event::ToolResult(_)
                    | Event::Delegation(_)
                    | Event::Status(_)
                    | Event::SteerResponse(_)
            )
        {
            self.last_render = now;
            let _ = self.flush();
        }
    }

    fn flush(&mut self) -> Result<()> {
        // F3: advance the frame counter on each flush so the status spinner
        // animates at the ~16 ms render cadence.
        self.frame_counter = self.frame_counter.wrapping_add(1);
        let mut terminal = match self.terminal.take() {
            Some(t) => t,
            None => {
                let stdout = io::stdout();
                let backend = CrosstermBackend::new(stdout);
                Terminal::new(backend)?
            }
        };
        let res = self.draw(&mut terminal);
        self.terminal = Some(terminal);
        res
    }

    fn poll_input(&mut self) -> Option<String> {
        let handled = self.handle_events(false);
        if handled || self.last_render.elapsed() >= Duration::from_millis(30) {
            self.last_render = std::time::Instant::now();
            let _ = self.flush();
        }
        self.rx.try_recv().ok()
    }

    fn read_input(&mut self) -> Option<String> {
        self.status_line = "Ready".to_string();
        let _ = self.flush();
        loop {
            // Esc / Ctrl+C sets `aborted`; return `None` so the session loop
            // stops cleanly instead of hanging while waiting for input.
            if self.aborted {
                return None;
            }
            let handled = self.handle_events(true);
            if let Ok(line) = self.rx.try_recv() {
                return Some(line);
            }
            if handled || self.last_render.elapsed() >= Duration::from_millis(50) {
                self.last_render = std::time::Instant::now();
                let _ = self.flush();
            }
        }
    }

    fn request_abort(&mut self) {
        self.aborted = true;
        self.confirm_abort = false;
        self.status_line = "Aborted by user.".to_string();
        crate::orchestrator::cancel_all();
    }

    fn aborted(&self) -> bool {
        self.aborted
    }

    fn clear_abort(&mut self) {
        self.aborted = false;
        self.confirm_abort = false;
        crate::orchestrator::reset_cancellation();
    }

    fn shutdown(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        let _ = disable_raw_mode();
    }

    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        // Adopt the session-loop's authoritative list, preserving any live
        // thinking/content already streamed into the local entries (t-c304).
        let mut activated_name = None;
        for incoming in &subagents {
            match self.subagents.iter_mut().find(|s| s.name == incoming.name) {
                Some(existing) => {
                    let was_inactive = !existing.is_active;
                    existing.is_active = incoming.is_active;
                    if was_inactive && incoming.is_active {
                        activated_name = Some(incoming.name.clone());
                    }
                    existing.logs = incoming.logs.clone();
                    if incoming.last_activity_at.is_some() {
                        existing.last_activity_at = incoming.last_activity_at;
                    }
                    if incoming.context_tokens > 0 {
                        existing.context_tokens = incoming.context_tokens;
                    }
                    // Keep locally-streamed thinking/content if the incoming
                    // entry has none yet (the loop only folds lifecycle events).
                    if !incoming.thinking.is_empty() {
                        existing.thinking = incoming.thinking.clone();
                    }
                    if !incoming.content.is_empty() {
                        existing.content = incoming.content.clone();
                    }
                }
                None => {
                    if incoming.is_active {
                        activated_name = Some(incoming.name.clone());
                    }
                    self.subagents.push(incoming.clone());
                }
            }
        }
        // Drop entries that are no longer present in the authoritative list.
        self.subagents
            .retain(|s| subagents.iter().any(|i| i.name == s.name));
        if self.subagents.iter().any(|s| s.is_active) {
            self.show_subagent_panel = true;
        }
        if let Some(name) = activated_name
            && let Some(idx) = self.subagents.iter().position(|s| s.name == name)
        {
            self.selected_subagent_idx = idx;
        } else if self.selected_subagent_idx >= self.subagents.len() {
            self.selected_subagent_idx = self.subagents.len().saturating_sub(1);
        }
    }
}

/// Leave the alternate screen (best-effort, from the panic hook).
pub fn leave_alt_screen() -> std::io::Result<()> {
    let _ = disable_raw_mode();
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)
}

/// Run the interactive TUI session using the CLI's optional initial prompt.
pub async fn run(
    cfg: &Config,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
) -> Result<()> {
    let mut renderer = TuiRenderer::new();
    super::run_session(cfg, &mut renderer, initial, manager).await
}

/// Returns `true` for explicit exit/abort commands.
fn is_abort(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("/abort")
        || t.eq_ignore_ascii_case("/exit")
        || t.eq_ignore_ascii_case("/quit")
        || t.eq_ignore_ascii_case("/q")
        || t.eq_ignore_ascii_case(":q")
        || t.eq_ignore_ascii_case(":q!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_sentences_splits_on_newline() {
        let mut buf = "Hello world\nNext line".to_string();
        let s = extract_complete_sentences(&mut buf);
        assert_eq!(s, vec!["Hello world\n"]);
        assert_eq!(buf, "Next line");
    }

    #[test]
    fn extract_sentences_collapses_consecutive_newlines() {
        let mut buf = "a\n\n\nb".to_string();
        let s = extract_complete_sentences(&mut buf);
        assert_eq!(s, vec!["a\n\n\n"]);
        assert_eq!(buf, "b");
    }

    #[test]
    fn extract_sentences_splits_on_punctuation() {
        let mut buf = "Hello world. Next sentence! And? Done".to_string();
        let s = extract_complete_sentences(&mut buf);
        assert_eq!(s, vec!["Hello world. ", "Next sentence! ", "And? "]);
        assert_eq!(buf, "Done");
    }

    #[test]
    fn extract_sentences_collapses_consecutive_punctuation() {
        let mut buf = "Wait... Really!! Hmm".to_string();
        let s = extract_complete_sentences(&mut buf);
        assert_eq!(s, vec!["Wait... ", "Really!! "]);
        assert_eq!(buf, "Hmm");
    }

    #[test]
    fn extract_sentences_keeps_incomplete_tail() {
        let mut buf = "This is an incomplete sentence".to_string();
        let s = extract_complete_sentences(&mut buf);
        assert!(s.is_empty());
        assert_eq!(buf, "This is an incomplete sentence");
    }

    #[test]
    fn extract_sentences_handles_tab_whitespace() {
        let mut buf = "One.\tTwo".to_string();
        let s = extract_complete_sentences(&mut buf);
        assert_eq!(s, vec!["One.\t"]);
        assert_eq!(buf, "Two");
    }

    #[test]
    fn message_style_matches_reference_prefixes() {
        let (s, special) = message_style("User: hello");
        assert!(special);
        assert_eq!(s.fg, Some(Color::Cyan));
        assert!(s.add_modifier.contains(Modifier::BOLD));

        let (s, special) = message_style("Marmennill: reply");
        assert!(special);
        assert_eq!(s.fg, Some(Color::LightYellow));
        assert!(s.add_modifier.contains(Modifier::BOLD));

        let (s, special) = message_style("[Status] running");
        assert!(special);
        assert_eq!(s.fg, Some(Color::Yellow));

        let (s, special) = message_style("System Error: boom");
        assert!(special);
        assert_eq!(s.fg, Some(Color::Red));
        assert!(s.add_modifier.contains(Modifier::BOLD));

        let (s, special) = message_style("--- Turn 1");
        assert!(special);
        assert_eq!(s.fg, Some(Color::Green));
        assert!(s.add_modifier.contains(Modifier::BOLD));

        let (_, special) = message_style("plain line");
        assert!(!special);
    }

    #[test]
    fn centered_rect_is_centered() {
        let r = Rect::new(0, 0, 100, 100);
        let c = centered_rect(70, 60, r);
        // 70% width centered: x = 15, width = 70.
        assert_eq!(c.x, 15);
        assert_eq!(c.width, 70);
        // 60% height centered: y = 20, height = 60.
        assert_eq!(c.y, 20);
        assert_eq!(c.height, 60);
    }

    #[test]
    fn append_steer_sentence_creates_marmennill_messages() {
        let mut r = TuiRenderer::new();
        r.append_steer_sentence("Hello");
        assert_eq!(r.messages, vec!["Marmennill: Hello"]);
        // Appending to an existing Marmennill message extends it.
        r.append_steer_sentence(" world");
        assert_eq!(r.messages, vec!["Marmennill: Hello world"]);
        // A non-Marmennill last message starts a new one.
        r.messages.push("User: hi".to_string());
        r.append_steer_sentence("New");
        assert_eq!(r.messages.last().unwrap(), "Marmennill: New");
    }

    #[test]
    fn wrapped_lines_counts_single_line() {
        assert_eq!(wrapped_lines("Hello world", 80), 1);
    }

    #[test]
    fn wrapped_lines_wraps_long_words() {
        // A single long word wider than the width wraps.
        assert_eq!(wrapped_lines("abcdefghij", 5), 2);
    }

    #[test]
    fn wrapped_lines_width_zero_returns_raw_line_count() {
        assert_eq!(wrapped_lines("a\nb\nc", 0), 3);
    }

    #[test]
    fn wrapped_lines_expands_tabs_and_trims_trailing_spaces() {
        // Tab expands to 4 spaces; trailing spaces trimmed.
        assert_eq!(wrapped_lines("a\tb   ", 80), 1);
    }

    #[test]
    fn wrapped_lines_empty_string_has_no_lines() {
        // An empty string has no raw lines, so it contributes 0 wrapped lines.
        assert_eq!(wrapped_lines("", 80), 0);
    }

    #[test]
    fn count_single_message_lines_excludes_think_when_hidden() {
        let msg = "User: hi\n<think>\nsecret\n</think>\nvisible";
        // show_thought = false → think block excluded: "User: hi" + "visible".
        assert_eq!(count_single_message_lines(msg, 80, false), 2);
        // show_thought = true → think block included without raw tag lines: "User: hi" + "secret" + "visible".
        assert_eq!(count_single_message_lines(msg, 80, true), 3);
    }

    #[test]
    fn ensure_message_cache_full_recompute_on_width_change() {
        let mut r = TuiRenderer::new();
        r.messages.push("Hello world".to_string());
        r.messages.push("Line 1\nLine 2\nLine 3".to_string());

        r.ensure_message_cache(80);
        assert_eq!(r.cached_total_message_lines.get(), 4); // 1 + 3
        assert_eq!(r.cached_message_lines.borrow().len(), 2);

        // Width change triggers full recompute.
        r.ensure_message_cache(5);
        assert_eq!(r.cached_message_lines.borrow().len(), 2);
        assert!(r.cached_total_message_lines.get() > 4);
    }

    #[test]
    fn ensure_message_cache_incremental_append() {
        let mut r = TuiRenderer::new();
        r.messages.push("Hello".to_string());
        r.ensure_message_cache(80);
        assert_eq!(r.cached_message_lines.borrow().len(), 1);

        // New message appended → incremental.
        r.messages.push("World".to_string());
        r.ensure_message_cache(80);
        assert_eq!(r.cached_message_lines.borrow().len(), 2);
        assert_eq!(r.cached_total_message_lines.get(), 2);
    }

    #[test]
    fn ensure_message_cache_truncates_on_removal() {
        let mut r = TuiRenderer::new();
        r.messages.push("a".to_string());
        r.messages.push("b".to_string());
        r.messages.push("c".to_string());
        r.ensure_message_cache(80);
        assert_eq!(r.cached_message_lines.borrow().len(), 3);

        r.messages.pop();
        r.ensure_message_cache(80);
        assert_eq!(r.cached_message_lines.borrow().len(), 2);
        assert_eq!(r.cached_total_message_lines.get(), 2);
    }

    #[test]
    fn invalidate_last_message_cache_recomputes_last() {
        let mut r = TuiRenderer::new();
        r.messages.push("Marmennill: Hello".to_string());
        r.ensure_message_cache(80);
        assert_eq!(r.cached_total_message_lines.get(), 1);

        // Append to the last message and invalidate.
        r.messages.last_mut().unwrap().push_str(" world");
        r.invalidate_last_message_cache();
        assert_eq!(r.cached_total_message_lines.get(), 1);
        assert_eq!(r.cached_message_lines.borrow().len(), 1);
    }

    #[test]
    fn estimated_chat_lines_includes_streaming_buffers() {
        let mut r = TuiRenderer::new();
        r.messages.push("Hello".to_string());
        r.current_content = "Streaming content".to_string();
        r.show_thought = true;
        r.current_thought = "thinking text".to_string();

        let n = r.estimated_chat_lines(80);
        // 1 (message) + 1 (content) + 1 (thought) = 3 (markers are not rendered)
        assert_eq!(n, 3);
    }

    #[test]
    fn estimated_subagent_lines_counts_sections() {
        let mut r = TuiRenderer::new();
        r.subagents.push(SubagentDetail {
            name: "coder".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec!["log1".to_string()],
            thinking: "think".to_string(),
            content: "out".to_string(),
            is_active: true,
            context_tokens: 0,
        });
        // Default (show_thought = false): 1 (header) + 1 ([Logs]) + 1 (- log1) = 3
        assert_eq!(r.estimated_subagent_lines(80), 3);
        // When show_thought = true: 1 (header) + 2 ([Thinking], " thinking") + 1 (think) + 1 (" response")
        // + 1 ([Output]) + 1 (out) + 1 ([Logs]) + 1 (- log1) = 9
        r.show_thought = true;
        assert_eq!(r.estimated_subagent_lines(80), 9);
    }

    #[test]
    fn message_line_caching_matches_reference() {
        let mut r = TuiRenderer::new();
        r.messages.push("Hello world".to_string());
        r.messages.push("Line 1\nLine 2\nLine 3".to_string());

        let lines_80 = r.estimated_chat_lines(80);
        assert_eq!(lines_80, 4); // 1 + 3

        // Cached line count should remain valid without re-evaluating everything.
        assert_eq!(r.cached_total_message_lines.get(), 4);
        assert_eq!(r.cached_message_lines.borrow().len(), 2);

        // Appending via append_steer_sentence.
        r.append_steer_sentence("New steer sentence.");
        let lines_after_steer = r.estimated_chat_lines(80);
        assert_eq!(lines_after_steer, 5);

        // Appending to the existing steer sentence.
        r.append_steer_sentence(" Another sentence.");
        let lines_after_steer2 = r.estimated_chat_lines(80);
        assert_eq!(lines_after_steer2, 5);
    }

    #[test]
    fn history_navigation_recalls_and_restores_draft() {
        let mut r = TuiRenderer::new();
        r.history.push("first".to_string());
        r.history.push("second".to_string());
        r.input_text = "draft".to_string();

        // Ctrl+Up: save draft, go to last.
        r.history_prev();
        assert_eq!(r.input_text, "second");
        assert_eq!(r.input_draft, "draft");

        // Ctrl+Up again: go to previous.
        r.history_prev();
        assert_eq!(r.input_text, "first");

        // Ctrl+Down: forward to second.
        r.history_next();
        assert_eq!(r.input_text, "second");

        // Ctrl+Down at end: restore draft.
        r.history_next();
        assert_eq!(r.input_text, "draft");
        assert_eq!(r.history_index, None);
    }

    #[test]
    fn cycle_focus_obeys_panel_visibility() {
        let mut r = TuiRenderer::new();
        r.show_plan_panel = true;
        r.plan_content = "plan".to_string();
        r.show_subagent_panel = true;
        r.subagents.push(SubagentDetail {
            name: "coder".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec![],
            thinking: String::new(),
            content: String::new(),
            is_active: true,
            context_tokens: 0,
        });

        r.focused_panel = FocusedPanel::Chat;
        r.cycle_focus();
        assert_eq!(r.focused_panel, FocusedPanel::Plan);
        r.cycle_focus();
        assert_eq!(r.focused_panel, FocusedPanel::Subagents);
        r.cycle_focus();
        assert_eq!(r.focused_panel, FocusedPanel::Chat);
    }

    /// t-c304: a `Delegation(Started)` event creates an active subagent entry
    /// and routes subsequent `Thinking`/`Message`/`Status` events into its
    /// live thinking/content/logs fields.
    #[test]
    fn delegation_started_tracks_live_thinking_content_logs() {
        let mut r = TuiRenderer::new();

        r.on_event(&Event::Delegation(
            crate::orchestrator::DelegationEvent::Started {
                agent: crate::agents::Agent::Coder,
                task: Some("t-9".to_string()),
            },
        ));

        // The subagent entry exists and is active.
        assert_eq!(r.subagents.len(), 1);
        assert_eq!(r.subagents[0].name, "coder-t-9");
        assert!(r.subagents[0].is_active);
        assert_eq!(r.active_agent, "coder-t-9");

        // Live thinking streams into the subagent's thinking field.
        r.on_event(&Event::Thinking("reasoning...".to_string()));
        assert_eq!(r.subagents[0].thinking, "reasoning...");

        // Live content streams into the subagent's content field.
        r.on_event(&Event::Message("final answer".to_string()));
        assert_eq!(r.subagents[0].content, "final answer");

        // Status updates append to the subagent's logs.
        r.on_event(&Event::Status("calling backend".to_string()));
        assert!(r.subagents[0].logs.iter().any(|l| l == "calling backend"));
    }

    /// t-c304: a `Delegation(Completed)` event marks the subagent idle and
    /// stops routing live content into it.
    #[test]
    fn delegation_completed_marks_idle_and_stops_routing() {
        let mut r = TuiRenderer::new();

        r.on_event(&Event::Delegation(
            crate::orchestrator::DelegationEvent::Started {
                agent: crate::agents::Agent::Coder,
                task: Some("t-10".to_string()),
            },
        ));
        r.on_event(&Event::Thinking("think".to_string()));
        assert_eq!(r.subagents[0].thinking, "think");

        r.on_event(&Event::Delegation(
            crate::orchestrator::DelegationEvent::Completed {
                agent: crate::agents::Agent::Coder,
                task: Some("t-10".to_string()),
            },
        ));

        assert!(!r.subagents[0].is_active);
        assert_eq!(r.active_agent, "Manager");

        // After completion, live content no longer routes into the subagent.
        r.on_event(&Event::Message("manager content".to_string()));
        assert_eq!(r.subagents[0].content, "");
    }

    /// t-c304: `set_subagents` adopts the session-loop's authoritative list,
    /// preserving locally-streamed thinking/content and dropping stale entries.
    #[test]
    fn set_subagents_adopts_authoritative_list() {
        let mut r = TuiRenderer::new();

        // Local entry with streamed thinking.
        r.subagents.push(SubagentDetail {
            name: "coder".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec!["started task t-1".to_string()],
            thinking: "local think".to_string(),
            content: "local content".to_string(),
            is_active: true,
            context_tokens: 0,
        });

        // Authoritative list from the loop (no thinking/content, just lifecycle).
        let authoritative = vec![SubagentDetail {
            name: "coder".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec!["started task t-1".to_string()],
            thinking: String::new(),
            content: String::new(),
            is_active: true,
            context_tokens: 0,
        }];
        r.set_subagents(authoritative);

        // Local thinking/content preserved; logs adopted.
        assert_eq!(r.subagents.len(), 1);
        assert_eq!(r.subagents[0].thinking, "local think");
        assert_eq!(r.subagents[0].content, "local content");
        assert!(r.subagents[0].is_active);

        // A stale entry not in the authoritative list is dropped.
        r.subagents.push(SubagentDetail {
            name: "researcher".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec![],
            thinking: String::new(),
            content: String::new(),
            is_active: false,
            context_tokens: 0,
        });
        r.set_subagents(vec![SubagentDetail {
            name: "coder".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec![],
            thinking: String::new(),
            content: String::new(),
            is_active: false,
            context_tokens: 0,
        }]);
        assert_eq!(r.subagents.len(), 1);
        assert_eq!(r.subagents[0].name, "coder");
    }

    #[test]
    fn test_orchestrator_content_is_white_and_steer_is_yellow() {
        let (white_style, special_white) = message_style("Orchestrator answer");
        assert!(!special_white);
        assert_eq!(white_style.fg, Some(Color::White));

        let (yellow_style, special_yellow) =
            message_style("Marmennill: Direct answer from arbitrator");
        assert!(special_yellow);
        assert_eq!(yellow_style.fg, Some(Color::LightYellow));
        assert!(yellow_style.add_modifier.contains(Modifier::BOLD));

        let (kvaser_style, special_kvaser) = message_style("Kvaser: Direct answer");
        assert!(special_kvaser);
        assert_eq!(kvaser_style.fg, Some(Color::LightYellow));

        let mut r = TuiRenderer::new();
        // Orchestrator stream -> goes to current_content
        r.on_event(&Event::Message("chunk of orchestrator".to_string()));
        assert_eq!(r.current_content, "chunk of orchestrator");
        assert!(r.steer_sentence_buffer.is_empty());

        // Steer arbitrator response -> streams into steer_sentence_buffer / yellow messages
        r.on_event(&Event::SteerResponse(
            "Direct reply from arbitrator.\n".to_string(),
        ));
        assert!(
            r.messages
                .iter()
                .any(|m| m.starts_with("Marmennill: Direct reply from arbitrator."))
        );
    }

    #[test]
    fn test_chat_renders_white_content_and_gates_thought() {
        let mut r = TuiRenderer::new();
        assert!(!r.show_thought); // Default is false (OFF)

        r.messages
            .push("<think>\nsecret thoughts\n</think>\nVisible answer to user".to_string());
        r.current_thought = "streaming thoughts".to_string();
        r.current_content = "streaming visible answer".to_string();

        let backend = ratatui::backend::TestBackend::new(80, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        // 1. When show_thought is false: thoughts must NOT appear in chat
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 10);
                r.render_chat(frame, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut lines = Vec::new();
        for y in 0..10 {
            let line: String = (0..80)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            lines.push(line);
        }
        let text_off = lines.join("\n");
        assert!(
            !text_off.contains("secret thoughts"),
            "secret thoughts should be hidden when show_thought=false"
        );
        assert!(
            !text_off.contains("streaming thoughts"),
            "streaming thoughts should be hidden when show_thought=false"
        );
        assert!(text_off.contains("Visible answer to user"));
        assert!(text_off.contains("streaming visible answer"));

        // 2. Toggle thought ON via /thought
        r.input_text = "/thought".to_string();
        r.submit();
        assert!(r.show_thought);

        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 10);
                r.render_chat(frame, area);
            })
            .unwrap();

        let buffer2 = terminal.backend().buffer();
        let mut lines2 = Vec::new();
        for y in 0..10 {
            let line: String = (0..80)
                .map(|x| buffer2[(x, y)].symbol().to_string())
                .collect();
            lines2.push(line);
        }
        let text_on = lines2.join("\n");
        assert!(
            text_on.contains("secret thoughts"),
            "thoughts should be shown when show_thought=true"
        );
        assert!(
            text_on.contains("streaming thoughts"),
            "streaming thoughts should be shown when show_thought=true"
        );
        assert!(!text_off.contains("<think>"));
        assert!(!text_off.contains("</think>"));
        assert!(!text_on.contains("<think>"));
        assert!(!text_on.contains("</think>"));
    }

    #[test]
    fn test_subagent_thinking_visibility_controlled_by_show_thought() {
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut r = TuiRenderer::new();
        r.subagents.push(SubagentDetail {
            name: "coder".to_string(),
            task_id: Some("t-001".to_string()),
            prompt: "task brief".to_string(),
            started_at: None,
            last_activity_at: None,
            logs: vec!["log step".to_string()],
            thinking: "secret subagent thoughts".to_string(),
            content: "deliverable code".to_string(),
            is_active: true,
            context_tokens: 0,
        });

        // 1. By default, show_thought is false -> [Thinking] must not appear
        assert!(!r.show_thought);
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 20);
                r.render_subagents(frame, area);
            })
            .unwrap();

        let buffer1 = terminal.backend().buffer();
        let mut lines1 = Vec::new();
        for y in 0..20 {
            let line: String = (0..80)
                .map(|x| buffer1[(x, y)].symbol().to_string())
                .collect();
            lines1.push(line);
        }
        let text_off = lines1.join("\n");
        assert!(
            !text_off.contains("[Thinking]"),
            "subagent [Thinking] should be hidden when show_thought=false"
        );
        assert!(
            !text_off.contains("secret subagent thoughts"),
            "subagent thoughts should be hidden when show_thought=false"
        );
        assert!(
            !text_off.contains("[Output]"),
            "subagent [Output] should be hidden when show_thought=false"
        );
        assert!(
            !text_off.contains("deliverable code"),
            "subagent output content should be hidden when show_thought=false"
        );
        assert!(text_off.contains("[Logs]"));
        assert!(text_off.contains("log step"));

        // 2. Toggle thought ON via /thought
        r.input_text = "/thought".to_string();
        r.submit();
        assert!(r.show_thought);

        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 20);
                r.render_subagents(frame, area);
            })
            .unwrap();

        let buffer2 = terminal.backend().buffer();
        let mut lines2 = Vec::new();
        for y in 0..20 {
            let line: String = (0..80)
                .map(|x| buffer2[(x, y)].symbol().to_string())
                .collect();
            lines2.push(line);
        }
        let text_on = lines2.join("\n");
        assert!(
            text_on.contains("[Thinking]"),
            "subagent [Thinking] should be shown when show_thought=true"
        );
        assert!(
            text_on.contains("secret subagent thoughts"),
            "subagent thoughts should be shown when show_thought=true"
        );
        assert!(
            text_on.contains("[Output]"),
            "subagent [Output] should be shown when show_thought=true"
        );
        assert!(
            text_on.contains("deliverable code"),
            "subagent output content should be shown when show_thought=true"
        );
    }

    #[test]
    fn test_subagent_tool_calls_logged_with_arguments_and_stripped_prefix() {
        let mut r = TuiRenderer::new();
        r.subagents.push(SubagentDetail {
            name: "coder-t-001".to_string(),
            task_id: Some("t-001".to_string()),
            prompt: "build raytracer".to_string(),
            started_at: None,
            last_activity_at: None,
            logs: vec![],
            thinking: String::new(),
            content: String::new(),
            is_active: true,
            context_tokens: 0,
        });
        r.active_agent = "coder-t-001".to_string();

        // Status event with agent prefix should have prefix stripped
        r.on_event(&Event::Status(
            "coder-t-001: write_file(src/scene.rs)".to_string(),
        ));
        assert_eq!(
            r.subagents[0].logs.last().unwrap(),
            "write_file(src/scene.rs)"
        );

        // ToolCall event during specialist execution should route directly to subagent logs
        r.on_event(&Event::ToolCall("run_command(cargo test)".to_string()));
        assert_eq!(
            r.subagents[0].logs.last().unwrap(),
            "run_command(cargo test)"
        );
        // And not pollute the main chat
        assert!(
            !r.messages
                .iter()
                .any(|m| m.contains("run_command(cargo test)"))
        );
    }

    #[test]
    fn test_chat_scrolling_renders_windowed_view() {
        let mut r = TuiRenderer::new();
        r.focused_panel = FocusedPanel::Chat;
        for i in 0..50 {
            r.messages.push(format!("Message line {i}"));
        }

        r.chat_width.set(80);
        r.chat_height.set(8); // inner area height of 80x10 bordered box

        let total_lines = r.estimated_chat_lines(80);
        assert_eq!(total_lines, 50);

        // Scroll to bottom
        r.scroll_to_bottom();
        assert_eq!(r.chat_scroll, 42); // 50 - 8

        let backend = ratatui::backend::TestBackend::new(80, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        // 1. Render bottom view
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 10);
                r.render_chat(frame, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut bottom_text = String::new();
        for y in 0..10 {
            let line: String = (0..80)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            bottom_text.push_str(&line);
            bottom_text.push('\n');
        }
        assert!(bottom_text.contains("Message line 49"));
        assert!(!bottom_text.contains("Message line 0"));

        // 2. Scroll up to top
        r.scroll_to_top();
        assert_eq!(r.chat_scroll, 0);

        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 10);
                r.render_chat(frame, area);
            })
            .unwrap();

        let buffer2 = terminal.backend().buffer();
        let mut top_text = String::new();
        for y in 0..10 {
            let line: String = (0..80)
                .map(|x| buffer2[(x, y)].symbol().to_string())
                .collect();
            top_text.push_str(&line);
            top_text.push('\n');
        }
        assert!(top_text.contains("Message line 0"));
        assert!(!top_text.contains("Message line 49"));

        // 3. Scroll down by 5 lines
        r.scroll_down(5);
        assert_eq!(r.chat_scroll, 5);
    }

    #[test]
    fn test_plan_scrolling_with_wrapped_lines() {
        let mut r = TuiRenderer::new();
        r.focused_panel = FocusedPanel::Plan;
        let plan_text = "# Header\nThis is a long plan line that will wrap onto multiple visual lines when rendered in a narrow panel.\n- [ ] Task 1\n- [x] Task 2\n";
        r.plan_content = plan_text.to_string();

        let total_lines = wrapped_lines(&r.plan_content, 20);
        assert!(total_lines > 4); // Wrapped into more lines than raw count

        r.plan_max_scroll = total_lines.saturating_sub(5) as u16;
        assert!(r.plan_max_scroll > 0);

        // Scroll down
        r.scroll_down(2);
        assert_eq!(r.plan_scroll, 2);

        // Scroll up
        r.scroll_up(1);
        assert_eq!(r.plan_scroll, 1);

        // Scroll to top
        r.scroll_to_top();
        assert_eq!(r.plan_scroll, 0);

        // Scroll to bottom
        r.scroll_to_bottom();
        assert_eq!(r.plan_scroll, r.plan_max_scroll);
    }

    #[test]
    fn test_visual_line_offset_of_first_pending() {
        let plan = "# Plan\n## Phase 1\n- [x] [t-001] Done 1\n- [x] [t-002] Done 2\n## Phase 2\n- [ ] [t-003] Next task\n- [ ] [t-004] Later task\n";
        let offset = visual_line_offset_of_first_pending(plan, 80);
        // Line 0: # Plan
        // Line 1: ## Phase 1
        // Line 2: - [x] [t-001] Done 1
        // Line 3: - [x] [t-002] Done 2
        // Line 4: ## Phase 2
        // Line 5: - [ ] [t-003] Next task (first unchecked)
        assert_eq!(offset, Some(5));

        let completed_plan = "# Plan\n- [x] Done 1\n- [x] Done 2\n";
        assert_eq!(
            visual_line_offset_of_first_pending(completed_plan, 80),
            None
        );
    }

    #[test]
    fn test_plan_auto_scrolls_to_first_pending_task() {
        let mut r = TuiRenderer::new();
        let plan_text = "# Plan\n## Phase 1\n- [x] Task 1\n- [x] Task 2\n- [x] Task 3\n- [x] Task 4\n- [x] Task 5\n- [x] Task 6\n- [x] Task 7\n- [x] Task 8\n## Phase 2\n- [ ] Task 9\n- [ ] Task 10\n";
        r.plan_content = plan_text.to_string();
        r.show_plan_panel = true;
        r.plan_auto_scroll = true;

        let backend = ratatui::backend::TestBackend::new(80, 8);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 8);
                r.render_plan(frame, area, plan_text, false);
            })
            .unwrap();

        // Area height is 8, inner height is 6.
        // Task 9 is at visual offset 11. Desired scroll is 11 - 1 = 10.
        // Total lines is 13, max scroll is 13 - 6 = 7.
        // Scroll should be min(10, 7) = 7.
        assert_eq!(r.plan_scroll, 7);
    }

    #[test]
    fn test_subagent_scrolling_and_focus() {
        let mut r = TuiRenderer::new();
        r.focused_panel = FocusedPanel::Subagents;
        r.subagents.push(SubagentDetail {
            name: "coder".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec![
                "log1".to_string(),
                "log2".to_string(),
                "log3".to_string(),
                "log4".to_string(),
            ],
            thinking: "think line 1\nthink line 2\nthink line 3".to_string(),
            content: "output line 1\noutput line 2".to_string(),
            is_active: true,
            context_tokens: 0,
        });

        r.show_thought = true;
        r.subagent_width.set(40);
        r.subagent_height.set(5);

        // Scroll down in subagents pane
        r.scroll_down(3);
        assert_eq!(r.subagent_scroll, 3);

        // Scroll up
        r.scroll_up(2);
        assert_eq!(r.subagent_scroll, 1);

        // Scroll to bottom
        r.scroll_to_bottom();
        let max = r.estimated_subagent_lines(40).saturating_sub(5) as u16;
        assert_eq!(r.subagent_scroll, max);
    }

    #[test]
    fn test_plan_auto_open_and_close() {
        let mut r = TuiRenderer::new();
        assert!(!r.show_plan_panel);
        assert!(!r.had_active_plan);

        // Simulate active plan appearing
        let has_active_plan = true;
        if has_active_plan && !r.had_active_plan {
            r.show_plan_panel = true;
            r.plan_scroll = 0;
            r.plan_auto_scroll = true;
        }
        r.had_active_plan = has_active_plan;
        assert!(r.show_plan_panel);

        // Simulate plan completed and archived
        let has_active_plan = false;
        if !has_active_plan && r.had_active_plan {
            r.show_plan_panel = false;
            if r.focused_panel == FocusedPanel::Plan {
                r.focused_panel = FocusedPanel::Chat;
            }
        }
        r.had_active_plan = has_active_plan;
        assert!(!r.show_plan_panel);
    }

    #[test]
    fn test_subagent_switch_scrolls_to_bottom_and_autoscrolls() {
        let mut r = TuiRenderer::new();
        r.show_subagent_panel = true;
        r.subagents.push(SubagentDetail {
            name: "coder-1".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec![
                "line 1".to_string(),
                "line 2".to_string(),
                "line 3".to_string(),
            ],
            thinking: "think".to_string(),
            content: "output".to_string(),
            is_active: true,
            context_tokens: 0,
        });
        r.subagents.push(SubagentDetail {
            name: "coder-2".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ],
            thinking: "think 2".to_string(),
            content: "output 2".to_string(),
            is_active: true,
            context_tokens: 0,
        });

        r.subagent_width.set(40);
        r.subagent_height.set(4);

        // Manually scroll up
        r.focused_panel = FocusedPanel::Subagents;
        r.scroll_up(5);
        assert!(!r.subagent_autoscroll);

        // Switch subagent right -> should auto-scroll to bottom and re-enable autoscroll
        r.selected_subagent_idx += 1;
        r.subagent_autoscroll = true;
        let w = r.subagent_width.get();
        let n = r.estimated_subagent_lines(w);
        let h = r.subagent_height.get();
        r.subagent_scroll = n.saturating_sub(h) as u16;

        assert!(r.subagent_autoscroll);
        assert_eq!(r.subagent_scroll, n.saturating_sub(h) as u16);
    }

    #[test]
    fn test_subagents_auto_focuses_latest_activated_agent() {
        let mut r = TuiRenderer::new();
        r.upsert_subagent("coder-t-001", true, "started t-001");
        assert_eq!(r.selected_subagent_idx, 0);

        r.upsert_subagent("coder-t-002", true, "started t-002");
        assert_eq!(r.selected_subagent_idx, 1);

        r.upsert_subagent("researcher-t-003", true, "started t-003");
        assert_eq!(r.selected_subagent_idx, 2);

        // Completion should not shift focus away from currently selected
        r.upsert_subagent("coder-t-001", false, "completed t-001");
        assert_eq!(r.selected_subagent_idx, 2);
    }

    #[test]
    fn test_input_cursor_stepping_left_and_right() {
        let mut r = TuiRenderer::new();
        r.focused_panel = FocusedPanel::Chat;
        r.input_text = "hello world".to_string();
        r.cursor = "hello world".len();

        // Step left 5 times
        for _ in 0..5 {
            r.cursor_left();
        }
        assert_eq!(r.cursor, "hello ".len());

        // Insert character at cursor
        r.insert_char('X');
        assert_eq!(r.input_text, "hello Xworld");
        assert_eq!(r.cursor, "hello X".len());

        // Step right 2 times
        r.cursor_right();
        r.cursor_right();
        assert_eq!(r.cursor, "hello Xwo".len());

        // Delete backward
        r.delete_backward();
        assert_eq!(r.input_text, "hello Xwrld");
    }

    #[test]
    fn test_token_counting_and_formatting_in_status_bar() {
        assert_eq!(
            TuiRenderer::format_token_counts(0, 0),
            "0 in / 0 out (0 total)"
        );
        assert_eq!(
            TuiRenderer::format_token_counts(450, 120),
            "450 in / 120 out (570 total)"
        );
        assert_eq!(
            TuiRenderer::format_token_counts(1500, 320),
            "1.5k in / 320 out (1.8k total)"
        );
        assert_eq!(
            TuiRenderer::format_token_counts(25400, 1200),
            "25.4k in / 1.2k out (26.6k total)"
        );
        assert_eq!(
            TuiRenderer::format_token_counts(1_200_000, 350_000),
            "1.2M in / 350.0k out (1.6M total)"
        );

        let mut r = TuiRenderer::new();
        r.on_event(&Event::TokensIn(1000));
        r.on_event(&Event::Message("Hello world response".to_string()));
        assert_eq!(r.tokens_in, 1000);
        assert_eq!(r.orchestrator_context_tokens, 1000);
        assert!(r.tokens_out > 0);
    }

    #[test]
    fn test_orchestrator_context_tokens_in_status_bar() {
        let mut r = TuiRenderer::new();
        r.on_event(&Event::TokensIn(14500));
        assert_eq!(r.orchestrator_context_tokens, 14500);

        let backend = ratatui::backend::TestBackend::new(100, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 9, 100, 1);
                r.render_status(frame, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = (0..100)
            .map(|x| buffer[(x, 9)].symbol().to_string())
            .collect();
        assert!(
            content.contains("Ctx: 14.5k"),
            "expected 'Ctx: 14.5k' in status bar, got: {content}"
        );
    }

    #[test]
    fn test_waiting_for_token_elapsed_time_in_status_bar() {
        let mut r = TuiRenderer::new();
        r.on_event(&Event::Status("Running (test-model)".to_string()));
        assert!(r.waiting_for_token_since.is_some());

        let backend = ratatui::backend::TestBackend::new(100, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 9, 100, 1);
                r.render_status(frame, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = (0..100)
            .map(|x| buffer[(x, 9)].symbol().to_string())
            .collect();
        assert!(
            content.contains("Running (test-model)"),
            "expected 'Running (test-model)' in status bar, got: {content}"
        );
        assert!(
            content.contains("s)"),
            "expected elapsed seconds counter '(X.Xs)' in status bar, got: {content}"
        );

        // First token arrives -> timer clears
        r.on_event(&Event::Message("token 1".to_string()));
        assert!(r.waiting_for_token_since.is_none());
    }

    #[test]
    fn test_subagent_context_tokens_rendering_in_list() {
        let mut r = TuiRenderer::new();
        r.show_subagent_panel = true;
        r.subagents.push(SubagentDetail {
            name: "coder-t-1".to_string(),
            task_id: Some("t-1".to_string()),
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec![],
            thinking: String::new(),
            content: String::new(),
            is_active: true,
            context_tokens: 3500,
        });
        r.subagents.push(SubagentDetail {
            name: "researcher-t-2".to_string(),
            task_id: Some("t-2".to_string()),
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec![],
            thinking: String::new(),
            content: String::new(),
            is_active: false,
            context_tokens: 1200,
        });
        r.subagents.push(SubagentDetail {
            name: "validator".to_string(),
            task_id: None,
            prompt: String::new(),
            started_at: None,
            last_activity_at: None,
            logs: vec![],
            thinking: String::new(),
            content: String::new(),
            is_active: true,
            context_tokens: 0,
        });

        let backend = ratatui::backend::TestBackend::new(100, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 100, 20);
                r.render_subagents(frame, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut lines = Vec::new();
        for y in 0..20 {
            let line: String = (0..100)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            lines.push(line);
        }
        let full_text = lines.join("\n");
        assert!(
            full_text.contains("coder-t-1 (Active, 3.5k ctx)"),
            "expected 'coder-t-1 (Active, 3.5k ctx)' in subagents panel, got:\n{full_text}"
        );
        assert!(
            full_text.contains("researcher-t-2 (Idle, 1.2k ctx)"),
            "expected 'researcher-t-2 (Idle, 1.2k ctx)' in subagents panel, got:\n{full_text}"
        );
        assert!(
            full_text.contains("validator (Active)"),
            "expected 'validator (Active)' with no ctx string when 0, got:\n{full_text}"
        );
    }

    #[test]
    fn test_subagent_time_counter_rendering_in_list() {
        let mut r = TuiRenderer::new();
        r.show_subagent_panel = true;
        let now = std::time::Instant::now();
        r.subagents.push(SubagentDetail {
            name: "coder-t-1".to_string(),
            task_id: Some("t-1".to_string()),
            prompt: String::new(),
            started_at: Some(now - Duration::from_secs(5)),
            last_activity_at: Some(now - Duration::from_secs(5)),
            logs: vec![],
            thinking: String::new(),
            content: String::new(),
            is_active: true,
            context_tokens: 3500,
        });
        r.subagents.push(SubagentDetail {
            name: "researcher-t-2".to_string(),
            task_id: Some("t-2".to_string()),
            prompt: String::new(),
            started_at: None,
            last_activity_at: Some(now - Duration::from_secs(85)),
            logs: vec![],
            thinking: String::new(),
            content: String::new(),
            is_active: false,
            context_tokens: 1200,
        });

        let backend = ratatui::backend::TestBackend::new(100, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 100, 20);
                r.render_subagents(frame, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut lines = Vec::new();
        for y in 0..20 {
            let line: String = (0..100)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            lines.push(line);
        }
        let full_text = lines.join("\n");
        assert!(
            full_text.contains("coder-t-1 (Active, 3.5k ctx, 5s ago)"),
            "expected 'coder-t-1 (Active, 3.5k ctx, 5s ago)' in subagents panel, got:\n{full_text}"
        );
        assert!(
            full_text.contains("researcher-t-2 (Idle, 1.2k ctx, 1m 25s ago)"),
            "expected 'researcher-t-2 (Idle, 1.2k ctx, 1m 25s ago)' in subagents panel, got:\n{full_text}"
        );
    }

    #[test]
    fn test_format_terminal_math() {
        let raw = r#"$$\|(O + tD) - C\|^2 = r^2$$
Let $L = O - C$. The equation becomes:
$$(L + tD) \cdot (L + tD) = r^2$$
$$(D \cdot D)t^2 + 2(L \cdot D)t + (L \cdot L) - r^2 = 0$$

This is a quadratic equation in the form $at^2 + bt + c = 0$ where:
- $a = D \cdot D$
- $b = 2(L \cdot D)$
- $c = (L \cdot L) - r^2$"#;

        let formatted = format_terminal_math(raw);
        assert!(!formatted.contains("$$"));
        assert!(!formatted.contains(r"\cdot"));
        assert!(!formatted.contains(r"\|"));
        assert!(formatted.contains("·"));
        assert!(formatted.contains("²"));
        assert!(formatted.contains("(D · D)t² + 2(L · D)t + (L · L) - r² = 0"));
        assert!(formatted.contains("at² + bt + c = 0"));
    }
}
