//! User-facing rendering and interactive session loop.

pub mod bridge;
pub(crate) mod helpers;
pub mod raw;
pub mod session;
pub mod transcript;
pub mod tui;

pub use helpers::{chunk_utf8, format_active_subtasks, format_plan_progress_summary};
pub use session::run_session;
pub use transcript::{UiRecord, UiTranscript};

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
    /// Accumulated duration this subagent has actively spent working.
    pub worked_duration: std::time::Duration,
    /// Instant of the most recent activity (streaming chunk, log, status, or lifecycle).
    pub last_activity_at: Option<std::time::Instant>,
    /// Ordered log lines for this subagent (status / tool activity).
    pub logs: Vec<String>,
    pub thinking: String,
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
    Thinking(String),
    /// A chunk of visible content from a specific subagent/specialist.
    SubagentMessage {
        agent_tag: String,
        text: String,
    },
    /// A chunk of reasoning/thinking from a specific subagent/specialist.
    SubagentThinking {
        agent_tag: String,
        text: String,
    },
    /// A tool invocation (rendered as `name(arguments)`).
    ToolCall(String),
    /// The textual result of a tool execution.
    ToolResult(String),
    /// A status / phase update (e.g. "calling backend…", "aborting…").
    Status(String),
    Delegation(DelegationEvent),
    /// The session has finished.
    Done,
    /// Estimated input/prompt tokens added.
    TokensIn(usize),
    /// Estimated output/completion tokens added.
    TokensOut(usize),
}

/// Shared abort / user-exit flag state for renderers that use the trait's
/// default abort-flag implementations (see [`Renderer`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct InputState {
    /// Whether an abort was requested (e.g. via `/abort` or Esc / Ctrl+C).
    pub aborted: bool,
    /// Whether an explicit user exit was requested.
    pub user_exit: bool,
}

/// Abstraction over the interactive TUI and the headless raw mode.
///
/// The abort-flag surface (`request_abort` / `aborted` / `clear_abort` /
/// `request_user_exit` / `user_exit_requested`) has trait defaults backed by
/// [`InputState`]; renderers with extra abort state (e.g. the TUI's
/// confirm-abort) override the individual methods they need.
pub trait Renderer: Send {
    fn init(&mut self) -> Result<()>;
    fn on_event(&mut self, event: &Event);
    fn flush(&mut self) -> Result<()>;
    fn force_flush(&mut self) -> Result<()> {
        self.flush()
    }
    /// Non-blocking input poll; no input by default.
    fn poll_input(&mut self) -> Option<String> {
        None
    }
    /// Blocking input read; no input by default.
    fn read_input(&mut self) -> Option<String> {
        None
    }
    fn request_abort(&mut self) {
        self.input_state().aborted = true;
    }
    fn aborted(&self) -> bool {
        self.input_state_shared().aborted
    }
    fn clear_abort(&mut self) {
        let state = self.input_state();
        state.aborted = false;
        state.user_exit = false;
    }
    /// Request a user exit; by default this is a plain abort.
    fn request_user_exit(&mut self) {
        self.request_abort();
    }
    /// Whether a user exit was explicitly requested.
    fn user_exit_requested(&self) -> bool {
        self.input_state_shared().user_exit
    }
    /// Access the renderer's shared abort / user-exit state.
    fn input_state(&mut self) -> &mut InputState;
    /// Shared-borrow view of the renderer's abort / user-exit state.
    fn input_state_shared(&self) -> &InputState;
    fn shutdown(&mut self);
    fn set_subagents(&mut self, _subagents: Vec<SubagentDetail>) {}
    fn set_thinking_budgets(&mut self, _cfg: &crate::config::Config) {}
    fn reset_active_agent(&mut self) {}

    // ---- session-rehydration hooks (flow §4.7, design A §6) ----
    //
    // All three have default bodies, so a renderer that keeps no pane model
    // (`RawRenderer`, the test renderers) compiles and behaves unchanged.
    //
    // **Threading:** `Renderer: Send`, and `TuiRenderer` holds `Cell`/`RefCell`
    // layout state, so `dyn Renderer` is `Send` but NOT `Sync`. `capture_ui`
    // therefore takes `&self`, never `&dyn Renderer` handed to another thread:
    // a capture/restore is legal only on the session thread, reached through the
    // `&mut dyn Renderer` the session loop already owns (flow §4.3, §4.7).
    // Nothing here requires `Sync`, and nothing may be added that would.

    /// CAPTURE the pane model into the durable `UiSection` (A §6.1). `&self`
    /// because a capture never mutates the UI.
    ///
    /// Pane strings are the **byte authority** for the display (A D4): the
    /// returned `chat_messages` is the live pane buffer verbatim — embedded
    /// newlines, prefixes the pane spells, empty and duplicate entries included
    /// (U1–U3). The default body returns an **empty** section, which is what a
    /// renderer with no pane model can honestly claim; it is a loadable section
    /// (`UiSection::structural_violations()` is empty for it).
    fn capture_ui(&self) -> crate::session::format::UiSection {
        crate::session::format::UiSection::default()
    }

    /// REHYDRATE the panes from a stored section (A D4). Assign, never append,
    /// never re-render, and never regenerate a line from a journal record — the
    /// crate has no `UiRecord → pane line` formatter and adding one for the
    /// restore path is prohibited (U1). Scroll/caret are re-clamped by the
    /// renderer's own existing clamp after assignment, not validated here.
    ///
    /// The default body is a **no-op**: a renderer without a pane model has
    /// nothing to rehydrate.
    fn restore_panes(&mut self, _section: &crate::session::format::UiSection) {}

    /// One-shot, loud surfacing of what the loader did and what it could not
    /// restore (A §8.4: the report reaches the UI, the journaled UI record and
    /// the log; discarding it is prohibited by D6).
    ///
    /// The default body is a no-op: the report itself is still logged/returned by
    /// the caller, so a renderer that cannot draw one loses only the on-screen
    /// copy.
    fn announce_restore(&mut self, _report: &crate::session::format::RepairReport) {}
}

pub fn restore() {
    let _ = tui::leave_alt_screen();
    let _ = raw::restore();
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
