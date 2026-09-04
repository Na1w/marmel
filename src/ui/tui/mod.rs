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
use crossterm::event::DisableMouseCapture;
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
    /// The current text in the input box.
    pub(crate) input_text: String,
    /// Byte offset of the input cursor within `input_text` (always on a char
    /// boundary). Drives insert/delete and the rendered caret position.
    pub(crate) cursor: usize,
    /// Whether the user is in the "confirm abort" state.
    pub(crate) confirm_abort: bool,
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
    /// List of active specialist subagents.
    pub(crate) subagents: Vec<SubagentDetail>,
    /// Index of the currently selected subagent. Defaults to `0`.
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
    /// Whether subagent details auto-scroll. Defaults to `false`.
    pub(crate) subagent_autoscroll: bool,

    /// Completed input lines forwarded to the session loop.
    pub(crate) rx: Receiver<String>,
    pub(crate) tx: Sender<String>,
    pub(crate) aborted: bool,
    /// Persistent terminal instance to preserve Ratatui frame diff buffers.
    pub(crate) terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,

    pub(crate) chat_auto_scroll: bool,
    pub(crate) plan_max_scroll: u16,
    pub(crate) plan_auto_scroll: bool,

    pub(crate) chat_area: Rect,
    pub(crate) plan_area: Rect,
    pub(crate) subagent_area: Rect,
    pub(crate) input_area: Rect,

    pub(crate) active_agent: String,
    pub(crate) last_render: std::time::Instant,
    pub(crate) waiting_for_token_since: Option<std::time::Instant>,

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

    fn rehydrate_messages(&mut self, messages: &[crate::types::Message]) {
        self.commit_turn_content();
        for msg in messages.iter().skip(1) {
            match msg {
                crate::types::Message::User { content } => {
                    if !content.starts_with("(SYSTEM NOTICE:") && !content.starts_with("(SYSTEM:") {
                        self.messages.push(format!("User: {content}"));
                    }
                }
                crate::types::Message::Assistant {
                    content,
                    tool_calls,
                    ..
                } => {
                    for call in tool_calls {
                        let args_val =
                            serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                                .unwrap_or_else(|_| {
                                    serde_json::Value::String(call.function.arguments.clone())
                                });
                        self.messages.push(format!(
                            "[Tool Call] {}",
                            crate::ui::helpers::format_tool_call_display(
                                &call.function.name,
                                &args_val
                            )
                        ));
                    }
                    if let Some(c) = content
                        && !c.trim().is_empty()
                    {
                        self.messages.push(c.clone());
                    }
                }
                crate::types::Message::Tool { content, .. } => {
                    self.messages.push(format!("[Tool Result] {content}"));
                }
                _ => {}
            }
        }
        let w = self.chat_width.get();
        let n = self.estimated_chat_lines(w);
        let h = self.chat_height.get();
        self.chat_scroll = n.saturating_sub(h) as u16;
        let _ = self.flush();
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
pub(crate) fn is_abort(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("/abort")
        || t.eq_ignore_ascii_case("/exit")
        || t.eq_ignore_ascii_case("/quit")
        || t.eq_ignore_ascii_case("/q")
        || t.eq_ignore_ascii_case(":q")
        || t.eq_ignore_ascii_case(":q!")
}
