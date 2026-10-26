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

/// Where a delegation stands while it waits for a backend concurrency slot.
///
/// Orthogonal to [`SubagentDetail::is_active`]: a queued delegation belongs to
/// the current round, is visible in the panel, and keeps its clocks — only its
/// slot state is different. `None` means "not waiting" (running, or never
/// capped). Positions come from the pool's FIFO ticket ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueState {
    /// 1-based rank in this backend key's FIFO (`1` = next in line, not running).
    pub position: usize,
    /// Queue depth for the key, **including** this delegation.
    pub of: usize,
}

/// Detailed status of an active specialist subagent.
#[derive(Debug, Clone, Default)]
pub struct SubagentDetail {
    /// The **effective registry key** of the worker this row projects:
    /// `WorkerEntry::key` (`{agent}-{task}`, `{agent}-wN`, or the `{base}#N` form a
    /// duplicate registration takes). Row identity, selection, the presentation
    /// maps (`subagent_is_thinking`, `subagent_turn_thinking`) and every registry
    /// lookup all use this string — never a name derived from the agent enum.
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
    /// Whether this subagent is currently running. Written by the registry
    /// projection only (`helpers::apply_registry_entry`): it mirrors
    /// `WorkerEntry::is_active`, so a row is open exactly while the Manager and the
    /// steering arbitrator consider that worker running.
    pub is_active: bool,
    /// Concurrency-slot queue state, when this subagent is waiting for a slot.
    /// Set by a `Queued` delegation event, cleared by `Running`, `Completed` and
    /// `Failed` alike — so a row that finishes, fails or is cancelled never keeps
    /// a stale `(n/m)` behind.
    pub queue: Option<QueueState>,
    /// Whether a **stream of this worker is currently paused by steering
    /// arbitration** (its model slot was yielded to the Steer Arbitrator).
    ///
    /// Orthogonal to [`SubagentDetail::is_active`]: a paused worker is still
    /// registered, still dispatched and still visible — only its stream is held
    /// while the arbitrator runs. `is_active` is never overloaded with it.
    ///
    /// Set by exactly one thing: the typed pause transition that the preemption
    /// handshake emits when a stream really yielded its slot
    /// ([`Event::SubagentStreamPause`]). A steering round that **skipped**
    /// pausing because the arbitrator had spare session capacity emits no such
    /// event, so the row keeps reading `(Active)`. Cleared by the matching
    /// resumption transition and by every teardown that closes the row
    /// (`apply_registry_entry`, the lifecycle arm, `set_subagents`), so a stale
    /// `Paused` can never outlive the stream that produced it.
    pub paused: bool,
    /// The context tokens used by this subagent.
    pub context_tokens: usize,

    // ---- worker lifecycle projection (design §9.3 D1) ----
    //
    // These four fields already exist in the orchestrator's public read model
    // ([`crate::orchestrator::WorkerEntry`]) and were dropped at the UI boundary,
    // which is why the Workers pane could only ever print a liveness glyph and the
    // status bar had to keep its `val N` field switched off. They are **mirrored
    // from the registry**, never derived from a label string and never counted by
    // the renderer itself.
    /// The registry's own descriptive status label (`WorkerEntry::status_label`,
    /// e.g. `Running`, `Approved`, `Rejected`, `Aborted`, `Revising`).
    ///
    /// `None` = the registry has no entry for this row (an event arrived before the
    /// registration, or the row was restored with no worker behind it), which is a
    /// real distinction the pane renders: with no label the row falls back to the
    /// `is_active` glyph instead of guessing a state from prose.
    ///
    /// This is the field that stops an aborted or validator-rejected worker from
    /// reading `✓ done`: liveness (`is_active`) says the stream closed, the label
    /// says **how**.
    pub status_label: Option<String>,
    /// Implementation turns recorded by the registry
    /// (`WorkerEntry::implementation_turns`).
    pub implementation_turns: usize,
    /// Validator rounds recorded by the registry (`WorkerEntry::validation_rounds`).
    /// The `val N` field of the status bar and the Workers row is driven by this
    /// number — never by an arbitration round counter (design §8.4).
    pub validation_rounds: usize,
    /// The latest validator feedback text carried by the registry
    /// (`WorkerEntry::latest_validator_feedback`), shown in the row detail so a
    /// rejected worker says why.
    pub latest_validator_feedback: Option<String>,
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
    /// A **stream-pause transition** for one registered worker, delivered by the
    /// preemption handshake itself (`orchestrator::preemption`) rather than by
    /// status wording: `paused: true` the moment the stream yields its model slot
    /// to the Steer Arbitrator, `paused: false` when the handshake hands the slot
    /// back.
    ///
    /// `agent_tag` is the stream spelling the sink already routes its
    /// `SubagentMessage` / `SubagentThinking` traffic under — never a name parsed
    /// out of a status line — and it resolves to a panel row through the same
    /// row-key resolution those events use.
    SubagentStreamPause {
        agent_tag: String,
        paused: bool,
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

    // ---- destructive-command gate (spec §5, §6, G3) ----
    //
    // The session loop and the stream sink (`bridge::sink::RendererSink`) are
    // shared by every renderer, and until t-013 their whole-string abort test
    // (`helpers::is_abort_command`) also matched the bare words
    // `stop / cancel / exit / quit / abort`. Spec §5 removes those: in the
    // interactive TUI a bare word is ordinary steering text and **every**
    // destructive spelling goes through the confirm overlay first. A renderer
    // that cannot draw that overlay must keep the old behaviour untouched, so the
    // shared input paths ask instead of assuming — through the single owner
    // `helpers::is_abort_for_renderer`, which both consumers call. Default `false`
    // = pre-rebuild vocabulary (`is_legacy_abort_command`); the TUI overrides it to
    // `true`, which is the only thing that changes how a submitted line is read.

    /// Whether this renderer gates destructive commands behind its own confirm
    /// surface, so the session loop must only recognise the deliberate slash
    /// spellings (`is_abort_command`) rather than the pre-rebuild bare words.
    fn gates_destructive_commands(&self) -> bool {
        false
    }

    // ---- session-rehydration hooks ----
    //
    // All three have default bodies, so a renderer that keeps no pane model
    // (`RawRenderer`, the test renderers) compiles and behaves unchanged.
    //
    // **Threading:** `Renderer: Send`, and `TuiRenderer` holds `Cell`/`RefCell`
    // layout state, so `dyn Renderer` is `Send` but NOT `Sync`. `capture_ui`
    // therefore takes `&self`, never `&dyn Renderer` handed to another thread:
    // a capture/restore is legal only on the session thread, reached through the
    // `&mut dyn Renderer` the session loop already owns. Nothing here requires
    // `Sync`, and nothing may be added that would.

    /// CAPTURE the pane model into the durable `UiSection`. `&self` because a
    /// capture never mutates the UI.
    ///
    /// Pane strings are the **byte authority** for the display: `chat_messages`
    /// is the live pane buffer verbatim — embedded newlines, prefixes the pane
    /// spells, empty and duplicate entries included. A renderer with no pane
    /// model honestly claims the **empty** section, which is still loadable
    /// (`UiSection::structural_violations()` is empty for it).
    fn capture_ui(&self) -> crate::session::format::UiSection {
        crate::session::format::UiSection::default()
    }

    /// REHYDRATE the panes from a stored section. Assign, never append, never
    /// re-render, and never regenerate a line from a journal record — the crate
    /// has no `UiRecord → pane line` formatter and adding one for the restore
    /// path is prohibited. Scroll/caret are re-clamped by the renderer's own
    /// existing clamp after assignment, not validated here.
    ///
    /// The default body is a **no-op**: a renderer without a pane model has
    /// nothing to rehydrate.
    fn restore_panes(&mut self, _section: &crate::session::format::UiSection) {}

    /// One-shot, loud surfacing of what the loader did and what it could not
    /// restore: the report reaches the UI, the journaled UI record and the log,
    /// and discarding it is prohibited.
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
