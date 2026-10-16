//! Interactive 3-panel Ratatui terminal UI.

mod events;
pub mod formatting;
mod render;

pub use formatting::*;

#[cfg(test)]
mod tests;

use super::{Event, Renderer, SubagentDetail};
use crate::config::Config;
use crate::orchestrator::OrchestratorManager;
use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

/// Which panel currently holds focus (drives border color and navigation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusedPanel {
    Chat,
    Plan,
    Subagents,
}

pub struct TuiRenderer {
    /// Ordered chat/log transcript. Each entry is a single message string
    /// (may contain embedded `\n`).
    pub(crate) messages: Vec<String>,
    /// Streaming assistant "thinking" block (between ` thinking` and ` response`).
    pub(crate) current_thought: String,
    /// Streaming assistant content (final answer text).
    pub(crate) current_content: String,
    /// The execution plan text. Defaults to `"No active execution plan."`.
    pub(crate) plan_content: String,
    /// Multi-line text editor widget for input.
    pub(crate) textarea: ratatui_textarea::TextArea<'static>,
    /// The current text in the input box.
    pub(crate) input_text: String,
    /// Byte offset of the input cursor within `input_text` (always on a char
    /// boundary). Drives insert/delete and the rendered caret position.
    pub(crate) cursor: usize,
    /// Whether the user is in the "confirm abort" state.
    pub(crate) confirm_abort: bool,
    /// Timestamp when confirm-abort was armed. Abort expires after 3 seconds.
    pub(crate) abort_armed_at: Option<std::time::Instant>,
    /// Whether UI state has changed and needs a flush/redraw.
    pub(crate) dirty: bool,
    /// Abort / user-exit flags (the `Renderer` trait's abort surface).
    pub(crate) input_state: crate::ui::InputState,
    /// Frame counter for animations (e.g. status spinner).
    pub(crate) frame_counter: u64,
    /// The status bar text. Defaults to `"Ready"`.
    pub(crate) status_line: String,
    /// Which panel has focus (drives border color). Defaults to `Chat`.
    pub(crate) focused_panel: FocusedPanel,
    /// Vertical scroll offset of the chat panel.
    pub(crate) chat_scroll: u16,
    /// Vertical scroll offset of the plan panel.
    pub(crate) plan_scroll: u16,
    /// Whether the plan panel is visible. Defaults to `false`.
    pub(crate) show_plan_panel: bool,
    /// Track if an active plan was previously open on disk for auto-open/close.
    pub(crate) had_active_plan: bool,
    /// Whether ` thinking`/` response` blocks are displayed. Defaults to `false`.
    pub(crate) show_thought: bool,
    /// Whether the subagents panel is visible. Defaults to `false`.
    pub(crate) show_subagent_panel: bool,
    pub(crate) subagents: Vec<SubagentDetail>,
    /// Defaults to `0`.
    pub(crate) selected_subagent_idx: usize,
    /// Scroll offset of the subagent details pane.
    pub(crate) subagent_scroll: u16,
    /// Scroll offset of the subagent list.
    pub(crate) subagent_list_scroll: u16,
    /// Input command history (most recent last).
    pub(crate) history: Vec<String>,
    /// Current position in history navigation. `None` = not navigating.
    pub(crate) history_index: Option<usize>,
    /// Saved draft of `input_text` when history navigation begins.
    pub(crate) input_draft: String,
    /// Buffer for streaming steer-mode sentences awaiting completion.
    pub(crate) steer_sentence_buffer: String,

    /// Cached chat inner width (area width − 2). Defaults to `80`.
    pub(crate) chat_width: std::cell::Cell<usize>,
    /// Cached chat inner height (area height − 2). Defaults to `15`.
    pub(crate) chat_height: std::cell::Cell<usize>,
    /// Cached subagent details width. Defaults to `40`.
    pub(crate) subagent_width: std::cell::Cell<usize>,
    /// Cached subagent details height. Defaults to `15`.
    pub(crate) subagent_height: std::cell::Cell<usize>,
    /// Cached plan panel width. Defaults to `40`.
    pub(crate) plan_width: std::cell::Cell<usize>,
    /// Cached plan panel height. Defaults to `15`.
    pub(crate) plan_height: std::cell::Cell<usize>,
    /// Width used for the current line cache. Defaults to `0`.
    pub(crate) cached_chat_width: std::cell::Cell<usize>,
    /// `show_thought` value used for the current line cache.
    pub(crate) cached_show_thought: std::cell::Cell<bool>,
    /// Per-message cached wrapped-line counts.
    pub(crate) cached_message_lines: std::cell::RefCell<Vec<usize>>,
    /// Sum of all cached per-message line counts.
    pub(crate) cached_total_message_lines: std::cell::Cell<usize>,
    /// Styled lines cached per message in `messages` so markdown/ANSI is not re-parsed on every frame.
    pub(crate) cached_rendered_messages: std::cell::RefCell<Vec<Vec<ratatui::text::Line<'static>>>>,
    /// `show_thought` value that the rendered-message cache was built with.
    pub(crate) cached_rendered_show_thought: std::cell::Cell<bool>,
    /// Whether subagent details auto-scroll. Defaults to `false`.
    pub(crate) subagent_autoscroll: bool,

    /// Completed input lines forwarded to the session loop.
    pub(crate) rx: Receiver<String>,
    pub(crate) tx: Sender<String>,
    /// Persistent terminal instance to preserve Ratatui frame diff buffers.
    pub(crate) terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,

    pub(crate) chat_auto_scroll: bool,
    pub(crate) plan_max_scroll: u16,
    pub(crate) plan_auto_scroll: bool,
    pub(crate) active_plan_task: Option<String>,

    pub(crate) chat_area: Rect,
    pub(crate) plan_area: Rect,
    pub(crate) subagent_area: Rect,
    pub(crate) input_area: Rect,

    pub(crate) active_agent: String,
    pub(crate) last_render: std::time::Instant,
    pub(crate) waiting_for_token_since: Option<std::time::Instant>,
    pub(crate) last_plan_check: std::time::Instant,
    pub(crate) plan_mtime: Option<std::time::SystemTime>,
    pub(crate) archive_mtime: Option<std::time::SystemTime>,
    pub(crate) plan_is_archived: bool,
    #[allow(dead_code)]
    pub(crate) session_start: std::time::Instant,

    /// Estimated total input (prompt) tokens across the session.
    pub tokens_in: usize,
    /// Estimated total output (completion) tokens across the session.
    pub tokens_out: usize,
    /// Context window size (tokens) of the orchestrator/main turn.
    pub orchestrator_context_tokens: usize,
    /// Map of specialist role -> max_thinking_tokens.
    pub(crate) thinking_budgets: std::collections::HashMap<String, usize>,
    /// Default thinking budget for specialists without specific overrides.
    pub(crate) default_thinking_budget: usize,
    /// Thinking content accumulated during the current active turn per subagent.
    pub(crate) subagent_turn_thinking: std::collections::HashMap<String, String>,
    /// Whether the subagent is actively streaming reasoning tokens in the current turn.
    pub(crate) subagent_is_thinking: std::collections::HashMap<String, bool>,
    /// Optional local override or cache of execution plan start instant.
    pub(crate) plan_start_time: Option<std::time::Instant>,
    /// Optional local override or cache of execution plan completed instant.
    pub(crate) plan_completed_time: Option<std::time::Instant>,
}

impl TuiRenderer {
    pub fn new() -> Self {
        let (tx, rx) = channel::<String>();
        Self {
            messages: Vec::new(),
            current_thought: String::new(),
            current_content: String::new(),
            plan_content: "No active execution plan.".to_string(),
            textarea: ratatui_textarea::TextArea::default(),
            input_text: String::new(),
            cursor: 0,
            confirm_abort: false,
            abort_armed_at: None,
            dirty: true,
            input_state: crate::ui::InputState::default(),
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
            cached_rendered_messages: std::cell::RefCell::new(Vec::new()),
            cached_rendered_show_thought: std::cell::Cell::new(false),
            subagent_autoscroll: true,

            rx,
            tx,
            terminal: None,

            chat_auto_scroll: true,
            plan_max_scroll: 0,
            plan_auto_scroll: false,
            active_plan_task: None,

            chat_area: Rect::default(),
            plan_area: Rect::default(),
            subagent_area: Rect::default(),
            input_area: Rect::default(),

            active_agent: "Manager".to_string(),
            last_render: std::time::Instant::now(),
            waiting_for_token_since: None,
            last_plan_check: std::time::Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(std::time::Instant::now),
            plan_mtime: None,
            archive_mtime: None,
            plan_is_archived: false,
            session_start: std::time::Instant::now(),
            thinking_budgets: std::collections::HashMap::new(),
            default_thinking_budget: crate::config::DEFAULT_MAX_THINKING_TOKENS,
            subagent_turn_thinking: std::collections::HashMap::new(),
            subagent_is_thinking: std::collections::HashMap::new(),
            plan_start_time: None,
            plan_completed_time: None,
        }
    }

    /// Populate thinking budget limits per specialist and globally from config.
    pub fn set_thinking_budgets(&mut self, cfg: &Config) {
        let global_budget = if cfg.max_thinking_tokens != crate::config::DEFAULT_MAX_THINKING_TOKENS
        {
            cfg.max_thinking_tokens
        } else {
            cfg.monitoring
                .as_ref()
                .map(|m| m.max_thinking_tokens)
                .unwrap_or(cfg.max_thinking_tokens)
        };
        self.default_thinking_budget = global_budget;
        self.thinking_budgets.clear();
        for (role, spec) in &cfg.orchestration.specialists {
            if let Some(b) = spec.max_thinking_tokens {
                self.thinking_budgets.insert(role.clone(), b);
            }
        }
    }

    /// Resolve the thinking token budget for a given subagent.
    pub(crate) fn get_thinking_budget(&self, sa: &SubagentDetail) -> usize {
        if let Some(b) = self.thinking_budgets.get(&sa.name) {
            return *b;
        }
        let role = sa.name.split('-').next().unwrap_or(&sa.name);
        if let Some(b) = self.thinking_budgets.get(role) {
            return *b;
        }
        self.default_thinking_budget
    }

    /// Total duration spent working on an execution plan.
    /// Only counts when actively working on a plan; freezes as soon as the plan is completed / archived.
    pub(crate) fn total_project_elapsed(&self) -> Option<std::time::Duration> {
        let using_global_plan = self.plan_start_time.is_none();
        let (inst, _) = self
            .plan_start_time
            .map(|t| (t, chrono::Local::now()))
            .or_else(|| {
                if self.had_active_plan {
                    crate::manager::phase::get_plan_start_time()
                } else {
                    None
                }
            })?;
        let global_completed = if using_global_plan && self.had_active_plan {
            crate::manager::phase::get_plan_completed_time()
        } else {
            None
        };
        if let Some(comp) = self.plan_completed_time.or(global_completed) {
            Some(comp.checked_duration_since(inst).unwrap_or_default())
        } else if self.plan_is_archived {
            if using_global_plan && self.had_active_plan {
                crate::manager::phase::record_plan_completed();
            }
            Some(inst.elapsed())
        } else {
            if using_global_plan && self.had_active_plan {
                let plan = crate::manager::phase::Plan::default();
                if plan.is_complete() {
                    crate::manager::phase::record_plan_completed();
                    if let Some(comp) = crate::manager::phase::get_plan_completed_time() {
                        return Some(comp.checked_duration_since(inst).unwrap_or_default());
                    }
                }
            }
            Some(inst.elapsed())
        }
    }

    /// The plan-start instant the plan box is actually drawing: the local
    /// override if the renderer holds one, otherwise the process-global plan
    /// start that [`Self::total_project_elapsed`] falls back to. Capture uses
    /// this so the stored timer is the one the pane showed, not whichever field
    /// happened to be populated.
    fn plan_start_instant(&self) -> Option<std::time::Instant> {
        self.plan_start_time.or_else(|| {
            if self.had_active_plan {
                crate::manager::phase::get_plan_start_time().map(|(instant, _wall)| instant)
            } else {
                None
            }
        })
    }

    /// The plan-completed instant the box is drawing, with the same fallback
    /// order as [`Self::total_project_elapsed`]: the local override, then the
    /// global completion only when the box is reading the global start.
    fn plan_completed_instant(&self) -> Option<std::time::Instant> {
        self.plan_completed_time.or_else(|| {
            if self.plan_start_time.is_none() && self.had_active_plan {
                crate::manager::phase::get_plan_completed_time()
            } else {
                None
            }
        })
    }

    pub(crate) fn render_terminal(&mut self) -> Result<()> {
        self.last_render = std::time::Instant::now();
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

    /// Check whether abort is actively armed and within the 3-second confirmation window.
    pub(crate) fn is_abort_armed(&self) -> bool {
        self.confirm_abort
            && self
                .abort_armed_at
                .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
    }

    /// Drain pending terminal events and apply render-throttling:
    /// - Force a full re-render when terminal events were handled.
    /// - When state is dirty (new events, tokens, status updates), flush up to 40 FPS (25 ms).
    /// - When an animated spinner is active, flush at 8 FPS (125 ms).
    /// - When completely idle and clean, do NOT re-render (0% CPU idle).
    pub(crate) fn drain_and_throttle_flush(&mut self, blocking: bool) {
        let handled = self.handle_events(blocking);
        if handled {
            self.dirty = false;
            let _ = self.force_flush();
        } else {
            let active_spinner = self.subagents.iter().any(|s| s.is_active)
                || [
                    "Running",
                    "Delegating",
                    "calling backend",
                    "calling model",
                    "Starting",
                    "streaming",
                    "thinking",
                    "Arbitrating",
                ]
                .iter()
                .any(|k| self.status_line.contains(k));

            if self.dirty {
                if self.last_render.elapsed() >= Duration::from_millis(25) {
                    self.dirty = false;
                    let _ = self.flush();
                }
            } else if active_spinner && self.last_render.elapsed() >= Duration::from_millis(125) {
                let _ = self.flush();
            }
        }
    }
}

impl Default for TuiRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn estimate_stream_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else if text.len() <= 4 {
        1
    } else {
        text.len().div_ceil(4)
    }
}

impl Renderer for TuiRenderer {
    fn init(&mut self) -> Result<()> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        self.terminal = Some(Terminal::new(backend)?);

        let plan = crate::manager::phase::Plan::default();
        if let Ok(Some(content)) = plan.read()
            && !content.trim().is_empty()
        {
            self.plan_content = content;
            self.show_plan_panel = true;
            self.had_active_plan = true;
        }

        self.force_flush()?;
        Ok(())
    }

    fn on_event(&mut self, event: &Event) {
        self.dirty = true;
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
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                let is_active_subagent = self.active_agent != "Manager"
                    && self
                        .subagents
                        .iter()
                        .any(|s| s.name == self.active_agent && s.is_active);
                if is_active_subagent {
                    if let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                    {
                        sa.content.push_str(text);
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                } else {
                    if self.active_agent != "Manager" {
                        self.active_agent = "Manager".to_string();
                    }
                    self.current_content.push_str(text);
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
            }
            Event::SteerResponse(text) => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
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
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                let is_active_subagent = self.active_agent != "Manager"
                    && self
                        .subagents
                        .iter()
                        .any(|s| s.name == self.active_agent && s.is_active);
                if is_active_subagent {
                    if let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                    {
                        sa.thinking.push_str(text);
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                } else {
                    if self.active_agent != "Manager" {
                        self.active_agent = "Manager".to_string();
                    }
                    self.current_thought.push_str(text);
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
            }
            Event::SubagentMessage { agent_tag, text } => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                self.subagent_turn_thinking.remove(agent_tag);
                self.subagent_is_thinking.insert(agent_tag.clone(), false);
                if let Some(sa) = self.subagents.iter_mut().find(|s| s.name == *agent_tag) {
                    sa.content.push_str(text);
                    sa.last_activity_at = Some(std::time::Instant::now());
                } else {
                    self.upsert_subagent(agent_tag, true, "running");
                    if let Some(sa) = self.subagents.iter_mut().find(|s| s.name == *agent_tag) {
                        sa.content.push_str(text);
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                }
            }
            Event::SubagentThinking { agent_tag, text } => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                self.subagent_is_thinking.insert(agent_tag.clone(), true);
                self.subagent_turn_thinking
                    .entry(agent_tag.clone())
                    .or_default()
                    .push_str(text);
                if let Some(sa) = self.subagents.iter_mut().find(|s| s.name == *agent_tag) {
                    sa.thinking.push_str(text);
                    sa.last_activity_at = Some(std::time::Instant::now());
                } else {
                    self.upsert_subagent(agent_tag, true, "running");
                    if let Some(sa) = self.subagents.iter_mut().find(|s| s.name == *agent_tag) {
                        sa.thinking.push_str(text);
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                }
            }
            Event::ToolCall(text) => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                self.commit_turn_content();
                let is_active_subagent = self.active_agent != "Manager"
                    && self
                        .subagents
                        .iter()
                        .any(|s| s.name == self.active_agent && s.is_active);
                if is_active_subagent {
                    self.subagent_turn_thinking.remove(&self.active_agent);
                    self.subagent_is_thinking
                        .insert(self.active_agent.clone(), false);
                    if let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                    {
                        let log_entry = text.clone();
                        if sa.logs.last().map(String::as_str) != Some(log_entry.as_str()) {
                            sa.logs.push(log_entry);
                        }
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                } else {
                    if self.active_agent != "Manager" {
                        self.active_agent = "Manager".to_string();
                    }
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
                let is_active_subagent = self.active_agent != "Manager"
                    && self
                        .subagents
                        .iter()
                        .any(|s| s.name == self.active_agent && s.is_active);
                if is_active_subagent {
                    if let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                    {
                        let log_entry = text.clone();
                        if sa.logs.last().map(String::as_str) != Some(log_entry.as_str()) {
                            sa.logs.push(log_entry);
                        }
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                } else {
                    if self.active_agent != "Manager" {
                        self.active_agent = "Manager".to_string();
                    }
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
                if !self.is_abort_armed() {
                    self.status_line = text.lines().next().unwrap_or("").to_string();
                }
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
                // Live-track logs for subagents: match by agent name prefix (e.g. "coder-<id>: ...")
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
                        if text.contains("thinking / calling model")
                            || text.contains("calling backend")
                        {
                            self.subagent_turn_thinking.remove(&sa.name);
                            self.subagent_is_thinking.insert(sa.name.clone(), false);
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
                        .find(|s| s.name == self.active_agent && s.is_active)
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
                    if text.contains("thinking / calling model") || text.contains("calling backend")
                    {
                        self.subagent_turn_thinking.remove(&sa.name);
                        self.subagent_is_thinking.insert(sa.name.clone(), false);
                    }
                    sa.last_activity_at = Some(std::time::Instant::now());
                }
            }
            Event::Delegation(de) => {
                self.commit_turn_content();
                match de {
                    crate::orchestrator::DelegationEvent::Started { agent, task } => {
                        let clean_task = task
                            .as_ref()
                            .map(|t| crate::task_id::normalize_task_id_ref(t))
                            .filter(|t| !t.is_empty());
                        let name = match clean_task {
                            Some(t) => format!("{agent}-{t}"),
                            None => format!("{agent}"),
                        };
                        self.active_agent = name.clone();
                        let t = clean_task.unwrap_or("(no task id)");
                        if let Some(tid) = clean_task {
                            self.active_plan_task = Some(tid.to_string());
                            self.plan_auto_scroll = true;
                            self.show_plan_panel = true;
                        }
                        // Fold into the local subagent list so the panel stays
                        // live even before the session loop pushes the
                        // authoritative list.
                        self.upsert_subagent(&name, true, &format!("started task {t}"));
                        self.subagent_turn_thinking.remove(&name);
                        self.subagent_is_thinking.insert(name.clone(), false);
                        if let Some(s) = self.subagents.iter_mut().find(|s| s.name == name) {
                            s.task_id = clean_task.map(|tid| tid.to_string());
                        }
                        self.status_line = format!("Delegating to specialist {name} for {t}");
                    }
                    crate::orchestrator::DelegationEvent::Completed { agent, task } => {
                        let clean_task = task
                            .as_ref()
                            .map(|t| crate::task_id::normalize_task_id_ref(t))
                            .filter(|t| !t.is_empty());
                        let name = match clean_task {
                            Some(t) => format!("{agent}-{t}"),
                            None => format!("{agent}"),
                        };
                        let t = clean_task.unwrap_or("(no task id)");
                        self.upsert_subagent(&name, false, &format!("completed task {t}"));
                        self.subagent_turn_thinking.remove(&name);
                        self.subagent_is_thinking.remove(&name);
                        self.plan_mtime = None;
                        self.last_plan_check = std::time::Instant::now()
                            .checked_sub(std::time::Duration::from_secs(1))
                            .unwrap_or_else(std::time::Instant::now);

                        let completed_msg = match clean_task {
                            Some(clean) => format!("[{clean}] completed."),
                            None => format!("[{agent}] completed."),
                        };
                        self.messages.push(completed_msg);
                        if self.chat_auto_scroll {
                            let w = self.chat_width.get();
                            let n = self.estimated_chat_lines(w);
                            let h = self.chat_height.get();
                            self.chat_scroll = n.saturating_sub(h) as u16;
                        }
                        if self.active_plan_task.as_deref() == clean_task {
                            self.active_plan_task = self
                                .subagents
                                .iter()
                                .find(|s| s.is_active && s.task_id.is_some() && s.name != name)
                                .and_then(|s| s.task_id.clone());
                        }
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
                    crate::orchestrator::DelegationEvent::Failed {
                        agent,
                        task,
                        reason,
                    } => {
                        let clean_task = task
                            .as_ref()
                            .map(|t| crate::task_id::normalize_task_id_ref(t))
                            .filter(|t| !t.is_empty());
                        let name = match clean_task {
                            Some(t) => format!("{agent}-{t}"),
                            None => format!("{agent}"),
                        };
                        let t = clean_task.unwrap_or("(no task id)");
                        let log_entry = match reason.as_deref().filter(|r| !r.is_empty()) {
                            Some(r) => format!("failed task {t}: {r}"),
                            None => format!("failed task {t}"),
                        };
                        self.upsert_subagent(&name, false, &log_entry);
                        self.plan_mtime = None;
                        self.last_plan_check = std::time::Instant::now()
                            .checked_sub(std::time::Duration::from_secs(1))
                            .unwrap_or_else(std::time::Instant::now);

                        let failed_msg =
                            match (clean_task, reason.as_deref().filter(|r| !r.is_empty())) {
                                (Some(clean), Some(r)) => format!("[{clean} failed: {r}]"),
                                (Some(clean), None) => format!("[{clean}] failed."),
                                (None, Some(r)) => format!("[{agent} failed: {r}]"),
                                (None, None) => format!("[{agent}] failed."),
                            };
                        self.messages.push(failed_msg);
                        if self.chat_auto_scroll {
                            let w = self.chat_width.get();
                            let n = self.estimated_chat_lines(w);
                            let h = self.chat_height.get();
                            self.chat_scroll = n.saturating_sub(h) as u16;
                        }
                        if self.active_plan_task.as_deref() == clean_task {
                            self.active_plan_task = self
                                .subagents
                                .iter()
                                .find(|s| s.is_active && s.task_id.is_some() && s.name != name)
                                .and_then(|s| s.task_id.clone());
                        }
                        let remaining_active: Vec<&str> = self
                            .subagents
                            .iter()
                            .filter(|s| s.is_active && s.name != name)
                            .map(|s| s.name.as_str())
                            .collect();
                        if !remaining_active.is_empty() {
                            self.active_agent = remaining_active[0].to_string();
                            self.status_line = format!(
                                "{name} failed {t}. Active: {}",
                                remaining_active.join(", ")
                            );
                        } else {
                            self.active_agent = "Manager".to_string();
                            self.status_line = match reason.as_deref().filter(|r| !r.is_empty()) {
                                Some(r) => format!("{name} failed task {t}: {r}"),
                                None => format!("{name} failed task {t}"),
                            };
                        }
                    }
                };
            }
            Event::Done => {
                self.commit_turn_content();
            }
        }
        let handled = self.handle_events(false);
        if handled || matches!(event, Event::Done) {
            let _ = self.force_flush();
        } else {
            let _ = self.flush();
        }
    }

    fn force_flush(&mut self) -> Result<()> {
        self.render_terminal()
    }

    fn flush(&mut self) -> Result<()> {
        if self.last_render.elapsed() < Duration::from_millis(25) {
            return Ok(());
        }
        self.render_terminal()
    }

    fn poll_input(&mut self) -> Option<String> {
        self.drain_and_throttle_flush(false);
        self.rx.try_recv().ok()
    }

    fn read_input(&mut self) -> Option<String> {
        self.commit_turn_content();
        self.status_line = "Ready".to_string();
        let _ = self.force_flush();
        loop {
            // Esc / Ctrl+C sets `aborted`; return `None` so the session loop
            // stops cleanly instead of hanging while waiting for input.
            if self.aborted() {
                return None;
            }
            self.drain_and_throttle_flush(true);
            if let Ok(line) = self.rx.try_recv() {
                return Some(line);
            }
        }
    }

    fn input_state(&mut self) -> &mut crate::ui::InputState {
        &mut self.input_state
    }

    fn input_state_shared(&self) -> &crate::ui::InputState {
        &self.input_state
    }

    fn request_abort(&mut self) {
        self.input_state.aborted = true;
        self.confirm_abort = false;
        self.abort_armed_at = None;
        self.dirty = true;
        crate::orchestrator::cancel_all();
    }

    fn request_user_exit(&mut self) {
        self.input_state.aborted = true;
        self.input_state.user_exit = true;
        self.confirm_abort = false;
        self.abort_armed_at = None;
        self.status_line = "Aborted by user.".to_string();
        self.dirty = true;
        crate::orchestrator::cancel_all();
    }

    fn clear_abort(&mut self) {
        self.input_state.aborted = false;
        self.input_state.user_exit = false;
        self.confirm_abort = false;
        self.abort_armed_at = None;
        self.dirty = true;
        crate::orchestrator::reset_cancellation();
    }

    fn shutdown(&mut self) {
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = disable_raw_mode();
    }

    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        // Adopt the session-loop's authoritative list, preserving any live
        // thinking/content already streamed into the local entries.
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
                    if existing.prompt.is_empty() && !incoming.prompt.is_empty() {
                        existing.prompt = incoming.prompt.clone();
                    }
                    if incoming.task_id.is_some() {
                        existing.task_id = incoming.task_id.clone();
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
        // Drop entries absent from the authoritative list.
        self.subagents
            .retain(|s| subagents.iter().any(|i| i.name == s.name));
        let any_active = self.subagents.iter().any(|s| s.is_active);
        if any_active {
            self.show_subagent_panel = true;
            if self.active_agent != "Manager"
                && !self
                    .subagents
                    .iter()
                    .any(|s| s.name == self.active_agent && s.is_active)
            {
                let active_names: Vec<&str> = self
                    .subagents
                    .iter()
                    .filter(|s| s.is_active)
                    .map(|s| s.name.as_str())
                    .collect();
                if !active_names.is_empty() {
                    self.active_agent = active_names[0].to_string();
                } else {
                    self.active_agent = "Manager".to_string();
                }
            }
        } else {
            if self.active_agent != "Manager" {
                self.active_agent = "Manager".to_string();
            }
            if self.status_line.contains("Active:")
                || self.status_line.starts_with("Delegating to specialist")
            {
                self.status_line = "Ready".to_string();
            }
            self.active_plan_task = None;
        }
        if let Some(name) = activated_name
            && let Some(idx) = self.subagents.iter().position(|s| s.name == name)
        {
            self.selected_subagent_idx = idx;
        } else if self.selected_subagent_idx >= self.subagents.len() {
            self.selected_subagent_idx = self.subagents.len().saturating_sub(1);
        }
    }

    fn set_thinking_budgets(&mut self, cfg: &crate::config::Config) {
        self.set_thinking_budgets(cfg);
    }

    fn reset_active_agent(&mut self) {
        self.active_agent = "Manager".to_string();
    }

    // ---- session-rehydration hooks ----

    /// CAPTURE the pane model verbatim. This is a pure field read: no event is
    /// replayed, nothing is re-rendered, and no line is regenerated. `&self`
    /// because a capture never mutates the UI.
    fn capture_ui(&self) -> crate::session::format::UiSection {
        let budgets = self
            .thinking_budgets
            .iter()
            .map(|(role, budget)| (role.clone(), *budget))
            .collect();
        crate::session::format::UiSection {
            // The pane vector is copied element-for-element, in order,
            // including embedded newlines and empty or duplicate entries.
            chat_messages: self.messages.clone(),
            status_line: self.status_line.clone(),
            subagent_panels: self
                .subagents
                .iter()
                .map(|detail| {
                    // Projection of `SubagentDetail`, plus the per-turn
                    // thinking state, which lives in the renderer's two maps
                    // (`subagent_turn_thinking`, `subagent_is_thinking`) keyed by
                    // the same `name`, and drives the detail pane's spinner and
                    // thinking bar. `None` distinguishes "no map entry" from
                    // "entry holding an empty string" — the pane draws a
                    // different bar for each, so the distinction is pane state.
                    let mut panel = subagent_panel_from_detail(detail);
                    panel.turn_thinking = self.subagent_turn_thinking.get(&detail.name).cloned();
                    panel.is_thinking = self.subagent_is_thinking.get(&detail.name).copied();
                    panel
                })
                .collect(),
            // The semantic journal is NOT renderer state. The live
            // `UiTranscript` is a local of `ui::session::run_session`, so
            // `capture_ui` cannot honestly fill this field; the committer must
            // take it from the transcript owner.
            journal: Vec::new(),
            composer: crate::session::format::ComposerState {
                input_text: self.input_text.clone(),
                cursor: self.cursor,
                input_draft: self.input_draft.clone(),
                history: self.history.clone(),
                history_index: self.history_index,
            },
            view: crate::session::format::ViewState {
                chat_scroll: self.chat_scroll,
                plan_scroll: self.plan_scroll,
                subagent_scroll: self.subagent_scroll,
                subagent_list_scroll: self.subagent_list_scroll,
                selected_subagent_idx: self.selected_subagent_idx,
                focused_panel: focused_panel_tag(self.focused_panel).to_string(),
                show_thought: self.show_thought,
                show_subagent_panel: self.show_subagent_panel,
                show_plan_panel: self.show_plan_panel,
                chat_auto_scroll: self.chat_auto_scroll,
                subagent_autoscroll: self.subagent_autoscroll,
                plan_auto_scroll: self.plan_auto_scroll,
            },
            // Witness only: `Config` stays the authority; capture states the
            // numbers it holds, and restore never adopts them.
            budgets: crate::session::format::BudgetWitness {
                thinking_budgets: budgets,
                default_thinking_budget: self.default_thinking_budget,
            },

            // ---- plan panel: every byte of the plan box ----
            // `plan_content` is the pane text; `plan_is_archived` and
            // `plan_max_scroll` stay out (derived per frame).
            plan_content: self.plan_content.clone(),
            had_active_plan: self.had_active_plan,
            active_plan_task: self.active_plan_task.clone(),
            // The timer the box actually drew: the local override if it has one,
            // otherwise the process-global plan start the renderer itself falls
            // back to (`total_project_elapsed`).
            plan_start: self.plan_start_instant().map(time_point_from_instant),
            plan_completed: self.plan_completed_instant().map(time_point_from_instant),

            // ---- streaming partials ----
            // The in-flight thinking/answer region of the chat pane. Display
            // only: capture records the bytes, it does not close the turn or
            // invent a transcript record.
            current_thought: self.current_thought.clone(),
            current_content: self.current_content.clone(),

            // ---- agent / notice identity ----
            // `Some(..)` always: a capture knows which agent the status line was
            // crediting. `None` is reserved for "a bundle predates this field".
            active_agent: Some(self.active_agent.clone()),
            steer_sentence_buffer: Some(self.steer_sentence_buffer.clone()),

            // ---- token counters (witness only) ----
            // Recorded, never adopted: `on_event` derives these from THIS
            // process's streams, so adopting them would double-count or erase a
            // running total. `restore_panes` says the drift out loud.
            tokens_in: self.tokens_in as u64,
            tokens_out: self.tokens_out as u64,
            orchestrator_context_tokens: self.orchestrator_context_tokens as u64,

            // ---- abort arming (display-only witness) ----
            // The armed window stays out of the live model: a 3-second armed
            // window has no meaning after a restart, and nothing re-arms it.
            // What IS recorded here is that the screen was showing the arming
            // prompt, and when: `is_abort_armed` already applies the 3-second
            // rule, so `true` means "the prompt was genuinely on screen at
            // capture instant".
            abort_armed: self.is_abort_armed(),
            abort_armed_at: self.abort_armed_at.map(time_point_from_instant),
        }
    }

    /// REHYDRATE the panes. Assigns wholesale — never appends, never
    /// regenerates a line from a journal record — then lets the renderer's own
    /// sync/clamp paths do what a resize already does.
    fn restore_panes(&mut self, section: &crate::session::format::UiSection) {
        // 1. Display authority first (fixed order of application).
        self.messages = section.chat_messages.clone();
        self.status_line = section.status_line.clone();
        self.subagents = section
            .subagent_panels
            .iter()
            .map(subagent_detail_from_panel)
            .collect();
        // The per-turn thinking state comes back with its own panels, so the
        // detail pane's spinner / "thinking n/budget" bar is in the state the
        // screen showed. An absent entry stays absent — the pane's fallback then
        // reads `thinking`, exactly as it does without a restore.
        self.subagent_turn_thinking = section
            .subagent_panels
            .iter()
            .filter_map(|panel| {
                panel
                    .turn_thinking
                    .clone()
                    .map(|text| (panel.name.clone(), text))
            })
            .collect();
        self.subagent_is_thinking = section
            .subagent_panels
            .iter()
            .filter_map(|panel| panel.is_thinking.map(|value| (panel.name.clone(), value)))
            .collect();

        // The plan box is pane text too: its content, and the text it flags as
        // in flight. An EMPTY stored plan text means "this bundle predates the
        // field" (`#[serde(default)]`), not "the box was blank": the plan pane is
        // disk-backed and a fresh process fills it at `init`, so assigning an
        // empty string would blank a pane that was never blank and force an
        // immediate reload. One predicate covers the whole plan group.
        let stores_plan_pane = !section.plan_content.is_empty();
        if stores_plan_pane {
            self.plan_content = section.plan_content.clone();
        }

        // The in-flight streaming region: the half-streamed thinking and answer
        // text come back as DISPLAY bytes. Nothing here resumes the provider
        // request or fabricates a finished transcript record; if the next turn
        // completes, the existing flush path (`events.rs`) folds these bytes into
        // the pane the same way it would have in the previous process.
        self.current_thought = section.current_thought.clone();
        self.current_content = section.current_content.clone();

        // Agent identity and the steer-sentence half-buffer. `None` = the bundle
        // predates the field: keep what this process derived at boot rather than
        // inventing `"Manager"`.
        if let Some(agent) = section
            .active_agent
            .as_ref()
            .filter(|agent| !agent.is_empty())
        {
            self.active_agent = agent.clone();
        }
        if let Some(buffer) = section.steer_sentence_buffer.clone() {
            self.steer_sentence_buffer = buffer;
        }

        // 2. Composer: assign text + caret, then rebuild the widget through the
        // existing sync path instead of hand-building a `TextArea`. The
        // textarea's undo/redo stack and in-widget selection anchors are not
        // restorable (ratatui-textarea internals).
        self.input_text = section.composer.input_text.clone();
        self.input_draft = section.composer.input_draft.clone();
        self.history = section.composer.history.clone();
        self.history_index = section.composer.history_index;
        self.cursor = section.composer.cursor;
        self.sanitize_cursor();
        self.ensure_textarea_in_sync();
        let caret = self.cursor;
        self.place_textarea_caret_at_byte(caret);

        // 3. Toggles and auto-scroll flags before the offsets.
        self.show_thought = section.view.show_thought;
        self.show_subagent_panel = section.view.show_subagent_panel;
        self.chat_auto_scroll = section.view.chat_auto_scroll;
        self.subagent_autoscroll = section.view.subagent_autoscroll;
        self.plan_auto_scroll = section.view.plan_auto_scroll;
        if stores_plan_pane {
            self.show_plan_panel = section.view.show_plan_panel;
            self.had_active_plan = section.had_active_plan;
            self.active_plan_task = section.active_plan_task.clone();
        }
        // An out-of-set spelling is a loader refusal, not something the
        // renderer invents: keep the current focus in that case.
        self.focused_panel =
            focused_panel_from_tag(&section.view.focused_panel).unwrap_or(self.focused_panel);

        // 4. Scroll offsets and selection, verbatim; the live clamp re-checks
        // them against the freshly restored content.
        self.chat_scroll = section.view.chat_scroll;
        self.plan_scroll = section.view.plan_scroll;
        self.subagent_scroll = section.view.subagent_scroll;
        self.subagent_list_scroll = section.view.subagent_list_scroll;
        self.selected_subagent_idx = section.view.selected_subagent_idx;

        // 4b. Time projections. `to_instant()` is total: a hostile
        // `elapsed_secs`/wall pair degrades to "now", it cannot panic and cannot
        // produce a negative duration. The plan timer is only ever a display
        // timer (`total_project_elapsed`).
        self.plan_start_time = section.plan_start.as_ref().map(|point| point.to_instant());
        self.plan_completed_time = section
            .plan_completed
            .as_ref()
            .map(|point| point.to_instant());

        // 5. Witnesses: recorded, never adopted (the rule for `budgets`, the
        // token counters and abort arming alike). Said out loud in the same shape
        // `announce_restore` uses, then logged — a silent version is forbidden,
        // and `token_witness_warnings` is public so the loader can fold the
        // identical sentence into `RepairReport::warnings`.
        for warning in self.token_witness_warnings(section) {
            self.messages
                .push(format!("[CLI] Restore warning: {warning}"));
            tracing::warn!("session restore (ui.json): {warning}");
        }
        // A 3-second confirm-abort window has no meaning in a new process, and
        // nothing re-arms it. The arming is therefore NOT restored: the live
        // renderer is left unarmed, and the stored `true` becomes a loud
        // statement instead. Silently re-arming would let one keypress abort a
        // session the user never asked to abort, and silently dropping it would
        // hide why the pane says "Abort armed. Press ESC again to abort."
        self.confirm_abort = false;
        self.abort_armed_at = None;
        if section.abort_armed {
            let detail = "confirm-abort was armed when the session was captured; \
                          the armed window is not re-armed after a restart, so it was \
                          dropped — press the abort key again if you still want to abort";
            self.messages.push(format!(
                "[CLI] Not restorable after restart [Other] session: {detail}"
            ));
            tracing::warn!("session restore (ui.json): {detail}");
        }

        // 6. Derived state is never restored, and the line/render caches are
        // keyed by message count, so a wholesale replace MUST invalidate them or
        // the first frame would draw stale bytes.
        self.invalidate_pane_caches();
        self.clamp_scrolls();
        // The clamp above MAY move a restored offset (a stored 9000 cannot
        // survive a 40-line pane), and that move is only legal when a sentence
        // names the offset AND both values. The sentence exists —
        // `clamp_warning_sentences` below, in the `token_witness_warnings` shape
        // — and `ui::session::boot` folds it into `RepairReport::warnings` BEFORE
        // `announce_restore`, which draws it into the panes and logs it. It is
        // deliberately not pushed into `self.messages` here: the number that must
        // be named is the one the renderer ends on (a later selection clamp in
        // `set_subagents` can move the index again), and drawing it twice would
        // make one moved offset read as two disagreements.
        self.dirty = true;
    }

    /// Surface the restore report out loud: a status line, a chat line per loss,
    /// and a `tracing::warn!` — never a `let _` swallow. The third destination,
    /// the journaled `UiRecord::Status`, is written by the transcript owner
    /// (`ui::session`), which is the only place that holds a `UiTranscript`.
    fn announce_restore(&mut self, report: &crate::session::format::RepairReport) {
        for notice in &report.notices {
            self.messages.push(format!(
                "[CLI] Not restorable after restart [{:?}] {}: {}",
                notice.kind, notice.subject, notice.detail
            ));
        }
        for warning in &report.warnings {
            self.messages
                .push(format!("[CLI] Restore warning: {warning}"));
        }
        for counts in &report.worker_counts {
            self.messages.push(format!(
                "[CLI] Worker {} history normalized: {} -> {} message(s) ({})",
                counts.key,
                counts.before,
                counts.after,
                if counts.applied_rules.is_empty() {
                    "no rule applied".to_string()
                } else {
                    counts.applied_rules.join(", ")
                }
            ));
        }
        self.status_line = format!(
            "Session restored: {} pane line(s), {} worker(s){}",
            report.ui_lines_restored,
            report.workers_restored,
            if report.notices.is_empty() {
                String::new()
            } else {
                format!(" — {} capability(s) lost", report.notices.len())
            }
        );
        self.dirty = true;

        tracing::warn!(
            pins_verified = report.pins_verified,
            pinned_prefix_end_expected = ?report.pinned_prefix_end_expected,
            pinned_prefix_end_actual = ?report.pinned_prefix_end_actual,
            orphan_tool_results_pruned = report.orphan_tool_results_pruned,
            aborted_results_synthesized = report.aborted_results_synthesized,
            tool_results_regrouped = report.tool_results_regrouped,
            normalization_passes = report.normalization_passes,
            normalization_stable = report.normalization_stable,
            manager_messages_before = report.manager_messages_before,
            manager_messages_after = report.manager_messages_after,
            ui_lines_restored = report.ui_lines_restored,
            workers_restored = report.workers_restored,
            workers_unrecoverable = ?report.workers_unrecoverable,
            completed_dropped_by_ring_cap = report.completed_dropped_by_ring_cap,
            notices = report.notices.len(),
            warnings = report.warnings.len(),
            "session restore: repair report surfaced to the UI"
        );
    }
}

/// The persisted spelling of [`FocusedPanel`] (`"chat" | "plan" |
/// "subagents"`, the same set as `session::format::FOCUSED_PANELS`).
fn focused_panel_tag(panel: FocusedPanel) -> &'static str {
    match panel {
        FocusedPanel::Chat => "chat",
        FocusedPanel::Plan => "plan",
        FocusedPanel::Subagents => "subagents",
    }
}

/// Inverse of [`focused_panel_tag`]; `None` for an out-of-set spelling, which
/// the loader refuses rather than repairs.
fn focused_panel_from_tag(tag: &str) -> Option<FocusedPanel> {
    match tag {
        "chat" => Some(FocusedPanel::Chat),
        "plan" => Some(FocusedPanel::Plan),
        "subagents" => Some(FocusedPanel::Subagents),
        _ => None,
    }
}

/// Project one live [`SubagentDetail`] onto its durable mirror.
/// `worked_duration_secs` is carried as an explicit number, never re-derived
/// from the two re-anchored instants.
fn subagent_panel_from_detail(detail: &SubagentDetail) -> crate::session::format::SubagentPanel {
    crate::session::format::SubagentPanel {
        name: detail.name.clone(),
        task_id: detail.task_id.clone(),
        prompt: detail.prompt.clone(),
        logs: detail.logs.clone(),
        thinking: detail.thinking.clone(),
        content: detail.content.clone(),
        is_active: detail.is_active,
        context_tokens: detail.context_tokens,
        worked_duration_secs: detail.worked_duration.as_secs_f64(),
        // Projected through [`time_point_from_instant`].
        started_at: detail.started_at.map(time_point_from_instant),
        last_activity_at: detail.last_activity_at.map(time_point_from_instant),
        // This function projects [`SubagentDetail`] and nothing else, so the two
        // per-turn thinking fields are left empty here; `capture_ui` overlays
        // them from the renderer's maps, keyed by the same `name`.
        turn_thinking: None,
        is_thinking: None,
    }
}

/// Project a live `Instant` onto the durable `TimePoint` pair.
///
/// [`SubagentDetail`] stores bare `Instant`s with **no** wall companion, and
/// `TimePoint::to_instant()` re-anchors from the wall half — so sampling the wall
/// clock *now* would place every stored instant at capture time and lose its age.
/// The wall half is therefore derived from the monotonic age: both halves are
/// sampled in one step, `age` comes from the monotonic clock and is exact within
/// this process, so `wall = now_wall - age` is where that instant actually
/// happened.
///
/// Honest limit: an instant from **before** this process's anchor cannot carry a
/// negative `elapsed_secs`, and `from_instant_with_wall` floors a wall clock older
/// than the anchor, so such an instant re-anchors to roughly "now". A subagent
/// never predates the process, and the loss is asserted by
/// `an_instant_older_than_the_process_anchor_honestly_loses_its_age`.
fn time_point_from_instant(instant: std::time::Instant) -> crate::session::format::TimePoint {
    let age = std::time::Instant::now().saturating_duration_since(instant);
    let age_delta = chrono::Duration::from_std(age).unwrap_or_else(|_| chrono::Duration::zero());
    crate::session::format::TimePoint::from_instant_with_wall(
        instant,
        chrono::Local::now() - age_delta,
    )
}

/// The inverse projection: a stored panel becomes the live model the subagent
/// panes draw, byte-for-byte for every pane string. The two per-turn thinking
/// fields are NOT part of [`SubagentDetail`]; `restore_panes` re-installs them
/// into the renderer's `subagent_turn_thinking` / `subagent_is_thinking` maps.
fn subagent_detail_from_panel(panel: &crate::session::format::SubagentPanel) -> SubagentDetail {
    SubagentDetail {
        name: panel.name.clone(),
        task_id: panel.task_id.clone(),
        prompt: panel.prompt.clone(),
        started_at: panel.started_at.as_ref().map(|point| point.to_instant()),
        worked_duration: duration_from_secs(panel.worked_duration_secs),
        last_activity_at: panel
            .last_activity_at
            .as_ref()
            .map(|point| point.to_instant()),
        logs: panel.logs.clone(),
        thinking: panel.thinking.clone(),
        content: panel.content.clone(),
        is_active: panel.is_active,
        context_tokens: panel.context_tokens,
    }
}

/// `Duration::from_secs_f64` panics on a negative/NaN/infinite input, so a
/// hostile or corrupt number degrades to zero instead of taking the UI down.
fn duration_from_secs(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

/// The scroll-clamp notice: one machine-readable line per restored offset whose
/// stored value differs from the live one.
///
/// This is the ONLY producer of that sentence, so a report and the log line that
/// accompanies it can never phrase the same disagreement two different ways. The
/// boot wiring in `ui::session` folds it into `RepairReport::warnings` through
/// that module's `scroll_clamp_warnings` helper, which compares the two
/// `UiSection`s (boot holds only a `&mut dyn Renderer`, so it reads the live
/// offsets from a re-capture rather than off the renderer).
pub(crate) fn clamp_warning_sentences(moved: &[(&str, u64, u64)]) -> Vec<String> {
    moved
        .iter()
        .copied()
        .filter(|(_, stored, live)| stored != live)
        .map(|(field, stored, live)| {
            format!(
                "{field} clamped by the live geometry: {stored} -> {live}: the stored value does \
                 not fit the restored content, and the live value is what the panes show now"
            )
        })
        .collect()
}

impl TuiRenderer {
    /// Move the widget caret to a restored byte offset — the inverse of
    /// `sync_input_from_textarea`'s (row, col) → byte mapping, so the caret is
    /// visible where the user left it.
    fn place_textarea_caret_at_byte(&mut self, byte: usize) {
        let byte = byte.min(self.input_text.len());
        let mut row = 0usize;
        let mut line_start = 0usize;
        let mut idx = 0usize;
        while idx < byte {
            if self.input_text.as_bytes()[idx] == b'\n' {
                row += 1;
                line_start = idx + 1;
            }
            idx += 1;
        }
        let line = self.textarea.lines().get(row).cloned().unwrap_or_default();
        let col_bytes = (byte - line_start).min(line.len());
        // `byte` is on a char boundary (loader precondition + `sanitize_cursor`),
        // so the slice is safe; the widget counts columns in chars.
        let col = line[..col_bytes].chars().count();
        self.textarea
            .move_cursor(ratatui_textarea::CursorMove::Jump(row as u16, col as u16));
    }

    /// The token counters are a **witness**, never an adoption: that is the rule
    /// already governing `BudgetWitness`, extended to `tokens_in`, `tokens_out`
    /// and `orchestrator_context_tokens`. `on_event` derives them from THIS
    /// process's streams, so adopting stored numbers would double-count a running
    /// total or erase one that has not started.
    ///
    /// One sentence per disagreeing counter, phrased for
    /// [`crate::session::format::RepairReport::warnings`] so the loader can push
    /// them in verbatim; `restore_panes` emits the identical sentence to the pane
    /// and the log, because a silent version is forbidden.
    pub fn token_witness_warnings(
        &self,
        section: &crate::session::format::UiSection,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for (field, stored, live) in [
            ("tokens_in", section.tokens_in, self.tokens_in as u64),
            ("tokens_out", section.tokens_out, self.tokens_out as u64),
            (
                "orchestrator_context_tokens",
                section.orchestrator_context_tokens,
                self.orchestrator_context_tokens as u64,
            ),
        ] {
            if stored != live {
                out.push(format!(
                    "{field} witness {stored} != this process's live {live}: kept the live \
                     counter (the streams own it) and reported the difference instead"
                ));
            }
        }
        out
    }

    /// Drop every derived chat cache so the next frame is rebuilt from the
    /// restored bytes: derived caches are never stored, and a frame must never
    /// draw a line that was not stored.
    fn invalidate_pane_caches(&self) {
        self.cached_rendered_messages.borrow_mut().clear();
        self.cached_message_lines.borrow_mut().clear();
        self.cached_chat_width.set(0);
        self.cached_total_message_lines.set(0);
        self.cached_show_thought.set(!self.show_thought);
        self.cached_rendered_show_thought.set(!self.show_thought);
    }
}

impl Drop for TuiRenderer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Leave the alternate screen (best-effort, from the panic hook).
pub fn leave_alt_screen() -> std::io::Result<()> {
    let _ = disable_raw_mode();
    execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableMouseCapture
    )
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
pub(crate) fn is_abort(line: &str) -> bool {
    crate::ui::helpers::is_abort_command(line)
}
