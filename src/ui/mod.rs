//! User-facing rendering and interactive session loop.

pub(crate) mod bridge;
pub(crate) mod helpers;
pub mod raw;
pub mod session;
pub mod tui;

pub use helpers::{chunk_utf8, format_active_subtasks, format_plan_progress_summary};
pub use session::run_session;

use crate::orchestrator::DelegationEvent;
use anyhow::Result;

/// Detailed status of an active specialist subagent.
#[derive(Debug, Clone, Default)]
pub struct SubagentDetail {
    /// The specialist role id (e.g. `coder`, `researcher`).
    pub name: String,
    pub task_id: Option<String>,
    pub prompt: String,
    pub started_at: Option<std::time::Instant>,
    /// Instant of the most recent activity (streaming chunk, log, status, or lifecycle).
    pub last_activity_at: Option<std::time::Instant>,
    /// Ordered log lines for this subagent (status / tool activity).
    pub logs: Vec<String>,
    /// Streaming "thinking" block for this subagent.
    pub thinking: String,
    /// Streaming final-answer content for this subagent.
    pub content: String,
    /// Whether this subagent is currently running (between Started/Completed).
    pub is_active: bool,
    /// The context tokens used by this subagent.
    pub context_tokens: usize,
}

/// A single agent event dispatched to the active renderer.
#[derive(Debug, Clone)]
pub enum Event {
    /// A chunk of visible assistant content (streamed as it arrives).
    Message(String),
    /// A steer response chunk or completed message.
    SteerResponse(String),
    /// A chunk of reasoning / thinking-channel content.
    Thinking(String),
    /// A tool invocation (rendered as `name(arguments)`).
    ToolCall(String),
    /// The textual result of a tool execution.
    ToolResult(String),
    /// A status / phase update (e.g. "calling backend…", "aborting…").
    Status(String),
    /// A delegation lifecycle event.
    Delegation(DelegationEvent),
    /// The session has finished.
    Done,
    /// Estimated input/prompt tokens added.
    TokensIn(usize),
    /// Estimated output/completion tokens added.
    TokensOut(usize),
}

/// Abstraction over the interactive TUI and the headless raw mode.
pub trait Renderer: Send {
    fn init(&mut self) -> Result<()>;
    fn on_event(&mut self, event: &Event);
    fn flush(&mut self) -> Result<()>;
    fn poll_input(&mut self) -> Option<String>;
    fn read_input(&mut self) -> Option<String>;
    fn request_abort(&mut self);
    fn aborted(&self) -> bool;
    fn clear_abort(&mut self) {}
    fn shutdown(&mut self);
    fn set_subagents(&mut self, _subagents: Vec<SubagentDetail>) {}
    fn rehydrate_messages(&mut self, _messages: &[crate::types::Message]) {}
}

pub fn restore() {
    let _ = tui::leave_alt_screen();
    let _ = raw::restore();
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
