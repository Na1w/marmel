//! Turn state machine driving the agent loop and automatic plan check-off.
//!
//! REQ-LOOP-001 (Turn Lifecycle): each agent turn passes through
//! `PrepareTurn -> CallBackend -> StreamResponse -> ProcessResponse -> ExecuteTools
//! -> CheckFinish`, then either starts the next turn or completes.
//!
//! REQ-LOOP-002 (Turn Limits): maximum 100 turns per interactive request; a 600 s
//! watchdog bounds the entire turn.
//!
//! REQ-LOOP-003 (Parallel Tool Execution): independent read-only tools
//! (`read_file`, `grep_search`, `glob`) run in parallel via `FuturesUnordered`;
//! writing tools (`write_file`, `replace`, `run_command`) run sequentially in the
//! order they appear.
//!
//! REQ-LOOP-004 (Mid-Flight Steering & Abort): user input during execution is
//! queued as `Steer(prompt)`, drained at the top of `PrepareTurn` and injected as
//! an immediate user message. An `Abort` stops the turn immediately, kills active
//! PTY process groups with `SIGKILL`, and reverts the session to ready.
//!
//! REQ-PLAN-002 (Disk check-off) is wired into `ExecuteTools`: successful tool
//! outputs (no `ERROR`/`FAILED`/`REPLAN REQUIRED`) toggle the annotated task on
//! disk in `.marmel/execution_plan.md`.

use anyhow::Result;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::agents::{Agent, DelegationRequest, Deliverable};
use crate::harness::monitor::{HarnessMonitor, Intervention};
use crate::harness::{HarnessStats, ToolCaller, ToolInvocation, ToolResult, dispatch_for};
use crate::orchestrator::{MAX_EXECUTING_ROUNDS, OrchestratorManager, brief_for_task};
use crate::tool_names::{
    TOOL_DELEGATE_TASK, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_READ_FILE, TOOL_REPLACE,
    TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
};
use crate::types::{Message, ToolCall};

use super::phase::Plan;

/// Maximum number of turns per interactive request (REQ-LOOP-002).
pub const MAX_TURNS: usize = 100;
/// Watchdog bound for the entire turn, in seconds (REQ-LOOP-002).
pub const TURN_WATCHDOG_SECS: u64 = 600;

/// The discrete phases a single agent turn passes through (REQ-LOOP-001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPhase {
    /// Collect + prepare the transcript; drain queued steer prompts.
    PrepareTurn,
    /// Send the prepared request to the LLM backend.
    CallBackend,
    /// Stream the assistant response from the backend.
    StreamResponse,
    /// Demux content vs. tool calls from the response.
    ProcessResponse,
    /// Execute any requested tools (parallel reads, sequential writes).
    ExecuteTools,
    /// Decide whether to continue or finish the session.
    CheckFinish,
}

impl TurnPhase {
    /// Advance to the next phase.
    pub fn next(self) -> Self {
        match self {
            TurnPhase::PrepareTurn => TurnPhase::CallBackend,
            TurnPhase::CallBackend => TurnPhase::StreamResponse,
            TurnPhase::StreamResponse => TurnPhase::ProcessResponse,
            TurnPhase::ProcessResponse => TurnPhase::ExecuteTools,
            TurnPhase::ExecuteTools => TurnPhase::CheckFinish,
            TurnPhase::CheckFinish => TurnPhase::PrepareTurn,
        }
    }
}

/// A steering or abort signal queued by the user mid-flight (REQ-LOOP-004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// User-supplied prompt injected at the top of `PrepareTurn`.
    Steer(String),
    /// Immediate halt (Ctrl+C / /abort); kills PTY process groups and reverts
    /// to ready.
    Abort,
}

/// Outcome of a single agent turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// More work remains; the loop should start the next turn.
    Continue,
    /// A tool produced an error and the loop should stop.
    ToolError(String),
    /// A terminal error occurred.
    Error(String),
    /// The user aborted the session.
    Aborted,
    /// The session finished (e.g. plan complete or max turns reached).
    Complete,
}

/// A tool invocation parsed from an assistant response together with any task-id
/// annotation embedded in its arguments (used for REQ-PLAN-002 check-off).
#[derive(Debug, Clone)]
struct PendingTool {
    invocation: ToolInvocation,
    /// Optional task id like `t-001` extracted from the tool arguments/name.
    task_id: Option<String>,
}

/// Regex matching a `[t-xxx]` task id annotation embedded in tool arguments.
/// Compiled exactly once via `OnceLock` (CODE_REVIEW Point 2).
static TASK_ID_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

/// Extract an optional `[t-xxx]` task id from a tool's arguments JSON string.
fn extract_task_id(_name: &str, args: &serde_json::Value) -> Option<String> {
    // A `task_id` may be embedded in the JSON arguments (plan annotation).
    let candidate = args.get("task_id").and_then(|v| v.as_str());
    if let Some(c) = candidate {
        return Some(c.to_string());
    }
    // Fall back to scanning the raw argument text for `[t-xxx]`.
    let raw = args.to_string();
    let re = TASK_ID_RE
        .get_or_init(|| regex::Regex::new(r"\[(t-[A-Za-z0-9_-]+)\]").expect("valid task regex"));
    re.captures(&raw).map(|m| m[1].to_string())
}

/// Returns `true` for read-only tools eligible for parallel execution
/// (REQ-LOOP-003).
fn is_read_tool(name: &str) -> bool {
    matches!(name, TOOL_READ_FILE | TOOL_GREP_SEARCH | TOOL_GLOB)
}

/// Returns `true` for writing/executing tools that must run sequentially.
///
/// `delegate_task` is included as a **sequential** (blocking, synchronous-from-
/// Manager) tool per REQ-ORCH-005: it mutates the plan/workspace and returns a
/// deliverable, so it must NOT be parallelized with reads (REQ-LOOP-003,
/// t-or01 §2.3).
fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        TOOL_DELEGATE_TASK | TOOL_WRITE_FILE | TOOL_REPLACE | TOOL_RUN_COMMAND
    )
}

/// The turn state machine. Each `run_turn` walks through the strict phase
/// sequence and returns an outcome; the caller loops until `Complete`.
#[derive(Debug)]
pub struct AgentLoop {
    plan: Plan,
    turn_count: usize,
    transcript: Vec<Message>,
    pending_signals: Vec<Signal>,
    pending_tools: Vec<PendingTool>,
    /// Process-group ids of active PTY sessions, tracked for abort (REQ-LOOP-004).
    active_pty_pids: Vec<i32>,
    /// Abort/cancel flag set by `Signal::Abort` immediately (REQ-LOOP-004), so a
    /// signal raised mid-flight during `ExecuteTools` interrupts an in-flight
    /// parallel read dispatch without waiting for the next turn to drain. The
    /// flag is cleared at the top of each turn after it is drained.
    abort_flag: Arc<AtomicBool>,
    /// Composed resilience monitor (REQ-HARN-001…004): XML tool rescue,
    /// semantic repetition/cycle detector, text repetition breaker, and the
    /// shared stats registry. Present in every loop (Manager turn loop AND each
    /// specialist's delegated turn) so the resilience harness is active in the
    /// live runtime, not just unit-tested.
    monitor: HarnessMonitor,
    /// Whether an XML-rescued call has been detected this turn (for reporting).
    rescued_this_turn: bool,
    /// The role whose tool calls this loop dispatches. Defaults to the Manager
    /// (REQ-ORCH-001); a specialist's delegated turn sets
    /// `ToolCaller::Specialist(agent)` so its tools are gated by the registry
    /// allowlist (REQ-ORCH-002).
    caller: ToolCaller,
}

impl Default for AgentLoop {
    fn default() -> Self {
        Self::new(Plan::default())
    }
}

impl AgentLoop {
    /// Create a new agent loop bound to a plan manager, with a fresh isolated
    /// stats registry and a fully armed resilience monitor. The loop dispatches
    /// as the Manager (REQ-ORCH-001).
    pub fn new(plan: Plan) -> Self {
        Self {
            plan,
            turn_count: 0,
            transcript: Vec::new(),
            pending_signals: Vec::new(),
            pending_tools: Vec::new(),
            active_pty_pids: Vec::new(),
            abort_flag: Arc::new(AtomicBool::new(false)),
            monitor: HarnessMonitor::with_new_stats(),
            rescued_this_turn: false,
            caller: ToolCaller::Manager,
        }
    }

    /// Create an agent loop bound to a plan manager and a shared session stats
    /// registry. The monitor records all resilience interventions into the same
    /// `Arc<HarnessStats>` so counters are aggregated across the session
    /// (REQ-HARN-004), while each loop keeps its own isolated repetition buffers.
    pub fn with_stats(plan: Plan, stats: Arc<HarnessStats>) -> Self {
        Self {
            plan,
            turn_count: 0,
            transcript: Vec::new(),
            pending_signals: Vec::new(),
            pending_tools: Vec::new(),
            active_pty_pids: Vec::new(),
            abort_flag: Arc::new(AtomicBool::new(false)),
            monitor: HarnessMonitor::new(stats),
            rescued_this_turn: false,
            caller: ToolCaller::Manager,
        }
    }

    /// Set the caller role whose tool calls this loop dispatches. A specialist
    /// turn must set `ToolCaller::Specialist(agent)` so its tools are gated by
    /// the registry allowlist (REQ-ORCH-002); the default is the Manager.
    pub fn with_caller(mut self, caller: ToolCaller) -> Self {
        self.caller = caller;
        self
    }

    /// Access the composed resilience monitor (for wiring the streaming layer).
    pub fn monitor(&mut self) -> &mut HarnessMonitor {
        &mut self.monitor
    }

    /// Feed a chunk of streamed assistant output into the text repetition
    /// detector (REQ-HARN-003). Returns `true` when the stream must be
    /// terminated because a ≥5-length pattern repeated ≥5 times; the repeated
    /// block is truncated and `repetition_breaks` is incremented once.
    pub fn feed_stream_text(&mut self, chunk: &str) -> bool {
        self.monitor.feed_text(chunk)
    }

    /// Intercept plain-text XML tool calls in `text` (REQ-HARN-001): returns
    /// structured [`ToolCall`]s with `call_text_{uuid}` ids and increments
    /// `xml_tool_rescues`. The caller routes the returned calls to execution.
    pub fn rescue_xml_calls(&mut self, text: &str) -> Vec<ToolCall> {
        let calls = self.monitor.rescue_xml(text);
        if !calls.is_empty() {
            self.rescued_this_turn = true;
        }
        calls
    }

    /// The current turn phase, computed from the loop's internal state.
    pub fn turn(&self) -> usize {
        self.turn_count
    }

    /// Queue a user signal for the next turn (REQ-LOOP-004).
    ///
    /// An `Abort` additionally sets the shared abort flag **immediately**, so a
    /// signal raised mid-flight during `ExecuteTools` interrupts an in-flight
    /// parallel read dispatch without waiting for the next turn to drain
    /// (REQ-LOOP-004: cancel in-flight tool futures and SIGKILL active PTY
    /// process groups as soon as abort fires). The flag is cleared at the top of
    /// each turn after it is drained.
    pub fn signal(&mut self, signal: Signal) {
        if matches!(signal, Signal::Abort) {
            self.abort_flag.store(true, Ordering::SeqCst);
        }
        self.pending_signals.push(signal);
    }

    /// Drain queued steer prompts; if an abort is queued, returns `true`.
    ///
    /// The shared abort flag is cleared at the start so a stale flag from a
    /// previous turn does not leak into a fresh turn; it is re-armed below if an
    /// `Abort` signal is actually pending.
    fn drain_signals(&mut self) -> bool {
        self.abort_flag.store(false, Ordering::SeqCst);
        let signals = std::mem::take(&mut self.pending_signals);
        let mut aborted = false;
        for s in signals {
            match s {
                Signal::Steer(prompt) => {
                    self.transcript.push(Message::User { content: prompt });
                }
                Signal::Abort => aborted = true,
            }
        }
        if aborted {
            // Re-arm so any in-flight tool future that checks the flag sees it.
            self.abort_flag.store(true, Ordering::SeqCst);
        }
        aborted
    }

    /// Run one full turn through the state machine. Returns the outcome.
    pub async fn run_turn(&mut self) -> Result<TurnOutcome> {
        self.turn_count += 1;
        if self.turn_count > MAX_TURNS {
            return Ok(TurnOutcome::Complete);
        }

        let mut phase = TurnPhase::PrepareTurn;
        let deadline = Instant::now() + Duration::from_secs(TURN_WATCHDOG_SECS);

        loop {
            // Watchdog: bound the whole turn (REQ-LOOP-002).
            if Instant::now() >= deadline {
                return Ok(TurnOutcome::Error("turn watchdog exceeded".to_string()));
            }

            match phase {
                TurnPhase::PrepareTurn => {
                    // Drain queued steer/abort signals (REQ-LOOP-004).
                    let aborted = self.drain_signals();
                    if aborted {
                        self.abort_pty_process_groups();
                        return Ok(TurnOutcome::Aborted);
                    }
                    phase = TurnPhase::CallBackend;
                }
                TurnPhase::CallBackend => {
                    // (The real LLM call lives in the UI/backend layer; here we
                    // expose the hook so the phase machine is complete.)
                    phase = TurnPhase::StreamResponse;
                }
                TurnPhase::StreamResponse => {
                    // Streaming is handled by the backend layer; the transcript
                    // is already populated by the caller via `push_message`.
                    phase = TurnPhase::ProcessResponse;
                }
                TurnPhase::ProcessResponse => {
                    // Demux content vs tool calls. The caller supplies tool calls
                    // via `enqueue_tools`; here we just advance.
                    phase = TurnPhase::ExecuteTools;
                }
                TurnPhase::ExecuteTools => {
                    let tools = std::mem::take(&mut self.pending_tools);
                    if tools.is_empty() {
                        // No tool calls this turn: advance to CheckFinish so the
                        // loop can decide continue vs complete.
                        phase = TurnPhase::CheckFinish;
                        continue;
                    }
                    let mut error: Option<String> = None;

                    // REQ-HARN-002: semantic repetition & cycle gate. Before any
                    // tool executes, observe it in the sliding buffer. A ≥3
                    // identical repetition blocks, an ≥3 alternating cycle cuts;
                    // the SPEC error payload is returned and the call is NOT
                    // dispatched. Pagination-only variation is exempt.
                    let mut blocked: Vec<String> = Vec::new();
                    let mut filtered = Vec::new();
                    for tool in &tools {
                        let intervention = self
                            .monitor
                            .observe_tool(&tool.invocation.name, &tool.invocation.arguments);
                        match intervention {
                            Intervention::None => filtered.push(tool.clone()),
                            other => {
                                if let Some(msg) = self.monitor.intervention_error(other) {
                                    blocked.push(msg);
                                }
                            }
                        }
                    }
                    // If any call was blocked/cut, return the SPEC error instead
                    // of executing it (REQ-HARN-002).
                    if !blocked.is_empty() {
                        return Ok(TurnOutcome::ToolError(blocked.join("\n")));
                    }
                    let tools = filtered;

                    // Partition into parallel reads and sequential writes
                    // (REQ-LOOP-003).
                    let reads: Vec<_> = tools
                        .iter()
                        .filter(|t| is_read_tool(&t.invocation.name))
                        .cloned()
                        .collect();
                    let writes: Vec<_> = tools
                        .iter()
                        .filter(|t| is_write_tool(&t.invocation.name))
                        .cloned()
                        .collect();

                    // Parallel read-only tools via FuturesUnordered. Each read is
                    // spawned onto a blocking thread so multiple reads overlap.
                    let mut futures = FuturesUnordered::new();
                    for tool in reads {
                        let invocation = tool.invocation.clone();
                        let caller = self.caller;
                        futures.push(async move {
                            let result = tokio::task::spawn_blocking(move || {
                                dispatch_for(&invocation, caller)
                            })
                            .await
                            .map_err(|e| crate::harness::ToolError::Execution(e.into()))
                            .and_then(|r| r);
                            (tool, result)
                        });
                    }
                    let mut completed: Vec<(
                        PendingTool,
                        Result<ToolResult, crate::harness::ToolError>,
                    )> = Vec::new();
                    while let Some(res) = futures.next().await {
                        // REQ-LOOP-004: an abort raised mid-flight interrupts the
                        // in-flight parallel read dispatch immediately. Dropping
                        // the `FuturesUnordered` cancels the remaining pending
                        // futures; SIGKILL every active PTY process group before
                        // returning.
                        if self.abort_flag.load(Ordering::SeqCst) {
                            self.abort_pty_process_groups();
                            return Ok(TurnOutcome::Aborted);
                        }
                        completed.push(res);
                    }
                    for (tool, res) in completed {
                        match res {
                            Ok(r) => {
                                if r.is_error {
                                    error = Some(r.content);
                                } else {
                                    self.check_off(&tool, &r.content);
                                }
                            }
                            Err(e) => error = Some(e.to_string()),
                        }
                    }

                    // Sequential write tools in order of appearance.
                    for tool in writes {
                        // REQ-LOOP-004: an abort raised mid-flight stops the
                        // sequential write loop immediately and SIGKILLs every
                        // active PTY process group.
                        if self.abort_flag.load(Ordering::SeqCst) {
                            self.abort_pty_process_groups();
                            return Ok(TurnOutcome::Aborted);
                        }
                        if error.is_some() {
                            break;
                        }
                        let inv = tool.invocation.clone();
                        let caller = self.caller;
                        let dispatch_res =
                            tokio::task::spawn_blocking(move || dispatch_for(&inv, caller)).await;

                        match dispatch_res {
                            Ok(Ok(r)) => {
                                if r.is_error {
                                    error = Some(r.content);
                                } else {
                                    self.check_off(&tool, &r.content);
                                }
                            }
                            Ok(Err(e)) => error = Some(e.to_string()),
                            Err(e) => error = Some(format!("task join error: {e}")),
                        }
                        // REQ-LOOP-004: check again after dispatch so a mid-flight
                        // abort raised while a long-running write was executing is
                        // caught as soon as it returns.
                        if self.abort_flag.load(Ordering::SeqCst) {
                            self.abort_pty_process_groups();
                            return Ok(TurnOutcome::Aborted);
                        }
                    }

                    if let Some(err) = error {
                        return Ok(TurnOutcome::ToolError(err));
                    }
                    phase = TurnPhase::CheckFinish;
                }
                TurnPhase::CheckFinish => {
                    // If the plan is complete, finish; else continue to next turn.
                    if self.plan.is_complete() {
                        return Ok(TurnOutcome::Complete);
                    }
                    return Ok(TurnOutcome::Continue);
                }
            }
        }
    }

    /// Check off a task on disk when the tool output was successful
    /// (REQ-PLAN-002). The task id may be absent; that is fine.
    ///
    /// t-205: leverages the marker-aware path. For a `delegate_task` invocation
    /// the returned `output` is the subagent deliverable content, so
    /// [`Plan::check_plan_on_marker`] parses its `MISSION COMPLETE` / `FAILED` /
    /// `REPLAN` terminal marker to gate the check-off (only a genuine completion
    /// flips the line). Non-delegation tools keep the legacy free-form success
    /// heuristic via `check_off_on_success`, and neither path bypasses the
    /// archive/check-off guards.
    fn check_off(&self, tool: &PendingTool, output: &str) {
        if let Some(task) = &tool.task_id {
            if tool.invocation.name == TOOL_DELEGATE_TASK {
                let _ = self
                    .plan
                    .check_plan_on_marker(Some(task), output)
                    .unwrap_or(false);
            } else {
                // Non-delegation tools: the loop already surfaced any tool error
                // via `r.is_error` before calling `check_off`. Legacy behavior
                // uses a hardcoded success string (the free-form `output_is_success`
                // heuristic on e.g. `read_file` output would falsely flag benign
                // occurrences of "error" like "thiserror"), so keep "ok".
                let _ = self.plan.check_off_on_success(task, "ok");
            }
        }
    }

    /// Register a PTY session's process-group id so it can be killed on abort.
    pub fn track_pty_pid(&mut self, pid: i32) {
        self.active_pty_pids.push(pid);
    }

    /// Clone of the shared abort flag, so an external task (e.g. the UI/backend
    /// layer) can raise a mid-flight abort while `run_turn` is executing tools
    /// (REQ-LOOP-004). Setting it to `true` interrupts the in-flight dispatch.
    pub fn abort_flag_handle(&self) -> Arc<AtomicBool> {
        self.abort_flag.clone()
    }

    /// Kill all active PTY process groups with SIGKILL and revert to ready
    /// (REQ-LOOP-004).
    fn abort_pty_process_groups(&self) {
        #[cfg(unix)]
        {
            for &pid in &self.active_pty_pids {
                let _ = crate::harness::pty::kill_process_group(pid);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = &self.active_pty_pids;
        }
    }

    /// Push a raw transcript message (used by the backend streaming layer).
    pub fn push_message(&mut self, msg: Message) {
        self.transcript.push(msg);
    }

    /// Queue tool calls (extracted by the response processor) for execution.
    pub fn enqueue_tools(&mut self, tools: Vec<serde_json::Value>) {
        for t in tools {
            let name = t
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let args = t
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let task_id = extract_task_id(&name, &args);
            let invocation = ToolInvocation {
                name,
                arguments: args,
            };
            self.pending_tools.push(PendingTool {
                invocation,
                task_id,
            });
        }
    }
}

/// The Manager's turn loop — a strict **Silent Dispatcher** engine
/// (REQ-ORCH-001 / REQ-PLAN-003 / REQ-LOOP-004).
///
/// In the **Executing** phase the Manager never emits conversational filler or
/// domain prose. Its output stream consists of `delegate_task` calls only. Each
/// unchecked `- [ ] [t-xxx]` plan item is delegated to the specialist whose
/// domain matches the task (via the `scheduler` closure), **one task per call**
/// (REQ-ORCH-005). A specialist MUST NOT autonomously iterate the whole plan:
/// it executes exactly the single task it was delegated (the `delegate()`
/// path binds a single `task_id` and auto-checks it on `MISSION COMPLETE`).
///
/// **Parallel delegation (REQ-ORCH-005):** independent pending tasks (sharing
/// no mutable state) are emitted concurrently. `delegate()` is synchronous
/// from the Manager's perspective, but multiple independent futures are spawned
/// and polled together, so a round of independent sub-tasks overlaps.
///
/// **Steer / Abort (REQ-LOOP-004):** `Signal::Steer` is queued; in Executing
/// mode the silent dispatcher does NOT inject user prose mid-dispatch (it is
/// deferred to final synthesis). `Signal::Abort` cancels all in-flight sub-task
/// futures immediately and SIGKILLs every active PTY process group.
pub struct ManagerLoop {
    /// The `OrchestratorManager` owning the shared `.marmel` plan and the
    /// `delegate()` method (Phase 0).
    manager: Arc<OrchestratorManager>,
    /// Maps a plan `task_id` to the specialist whose domain matches the task's
    /// type (REQ-ORCH-002 selection rule).
    scheduler: Box<dyn Fn(&str) -> Agent>,
    /// Abort/cancel flag set by `Signal::Abort` (checked between rounds and
    /// after each spawn, so in-flight futures are cancelled immediately).
    abort_flag: Arc<AtomicBool>,
    /// Queued user signals (REQ-LOOP-004).
    pending_signals: Vec<Signal>,
    /// Process-group ids of active PTY sessions, killed on abort.
    active_pty_pids: Vec<i32>,
}

impl ManagerLoop {
    /// Create a Manager turn loop rooted at a shared `OrchestratorManager` and
    /// a task-id → specialist scheduler.
    pub fn new(manager: Arc<OrchestratorManager>, scheduler: Box<dyn Fn(&str) -> Agent>) -> Self {
        Self {
            manager,
            scheduler,
            abort_flag: Arc::new(AtomicBool::new(false)),
            pending_signals: Vec::new(),
            active_pty_pids: Vec::new(),
        }
    }

    /// Queue a user signal for the next execution round (REQ-LOOP-004).
    ///
    /// An `Abort` additionally sets the shared abort flag **immediately**, so a
    /// signal raised mid-round interrupts an in-flight parallel dispatch without
    /// waiting for the round to drain (REQ-LOOP-004: cancel all in-flight
    /// sub-tasks and SIGKILL active PTY process groups as soon as abort fires).
    /// The flag is cleared at the top of each round after it is drained.
    pub fn signal(&mut self, signal: Signal) {
        if matches!(signal, Signal::Abort) {
            self.abort_flag.store(true, Ordering::SeqCst);
        }
        self.pending_signals.push(signal);
    }

    /// Register a PTY process-group id so it can be SIGKILLed on abort
    /// (REQ-LOOP-004).
    pub fn track_pty_pid(&mut self, pid: i32) {
        self.active_pty_pids.push(pid);
    }

    /// Drain queued signals. Returns `true` when an abort is pending.
    ///
    /// The shared abort flag is cleared at the start so a stale flag from a
    /// previous round does not leak into a fresh execution; it is re-armed below
    /// if an `Abort` signal is actually pending.
    fn drain_signals(&mut self) -> bool {
        self.abort_flag.store(false, Ordering::SeqCst);
        let signals = std::mem::take(&mut self.pending_signals);
        let mut aborted = false;
        for s in signals {
            match s {
                // In Executing mode the Manager is a silent dispatcher: a steer
                // is NOT injected into the Manager transcript mid-round (no
                // filler); it is deferred to the final synthesis round.
                Signal::Steer(_) => {}
                Signal::Abort => aborted = true,
            }
        }
        if aborted {
            // Re-arm so any in-flight sub-task that checks the flag sees it.
            self.abort_flag.store(true, Ordering::SeqCst);
        }
        aborted
    }

    /// SIGKILL every active PTY process group immediately (REQ-LOOP-004).
    fn abort_pty_process_groups(&self) {
        #[cfg(unix)]
        {
            for &pid in &self.active_pty_pids {
                let _ = crate::harness::pty::kill_process_group(pid);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = &self.active_pty_pids;
        }
    }

    /// Drive the **Executing** phase as a strict Silent Dispatcher
    /// (REQ-PLAN-003 / REQ-ORCH-001).
    ///
    /// Iterates the on-disk plan: for every unchecked `- [ ] [t-xxx]` it routes
    /// a single `DelegationRequest` to the specialist returned by `scheduler`,
    /// and independent tasks are delegated **in parallel** (REQ-ORCH-005). Each
    /// `delegate()` auto-checks-off the task on `MISSION COMPLETE (t-xxx)` and
    /// leaves it unchecked on `FAILED`/`REPLAN REQUIRED`.
    ///
    /// The loop is bounded by [`MAX_EXECUTING_ROUNDS`] so an un-delegate-able
    /// plan fails loudly instead of spinning. An `Abort` signal cancels all
    /// in-flight sub-tasks and SIGKILLs every active PTY process group, then
    /// returns the deliverables gathered so far.
    pub async fn run_executing(&mut self) -> Result<Vec<Deliverable>> {
        let mut results: Vec<Deliverable> = Vec::new();
        let mut attempts = 0;

        while !self.manager.plan.is_complete() && attempts < MAX_EXECUTING_ROUNDS {
            // REQ-LOOP-004: drain queued steer/abort at the top of the round.
            if self.drain_signals() {
                self.abort_flag.store(true, Ordering::SeqCst);
                self.abort_pty_process_groups();
                return Ok(results);
            }

            attempts += 1;
            let pending = self.manager.plan.pending_tasks();
            if pending.is_empty() {
                break;
            }

            // Silent Dispatcher: one self-contained DelegateRequest per task.
            let mut handles = Vec::with_capacity(pending.len());
            for task_id in pending {
                let agent = (self.scheduler)(&task_id);
                let brief = brief_for_task(&self.manager.plan, &task_id);
                let req = DelegationRequest {
                    agent_name: agent,
                    prompt: brief,
                    snippets: vec![],
                    task_id: Some(task_id),
                    image_urls: None,
                    audio_urls: None,
                    recursion_granted: false,
                };
                let mgr = self.manager.clone();
                let abort = self.abort_flag.clone();
                handles.push(tokio::spawn(async move {
                    if abort.load(Ordering::SeqCst) {
                        return Err(anyhow::anyhow!("aborted"));
                    }
                    mgr.delegate(req).await
                }));
            }

            // Poll the in-flight sub-tasks. On abort, cancel ALL remaining
            // in-flight futures immediately (REQ-LOOP-004) and SIGKILL every
            // active PTY process group before returning. Dropping an `abort()`ed
            // `JoinHandle` leaves the spawned task running in the background, so
            // we explicitly cancel every handle still in flight, not just the
            // one we happen to be awaiting.
            let mut round: Vec<Deliverable> = Vec::new();
            // Drain the in-flight handles. On abort, cancel every handle that has
            // not yet been awaited (REQ-LOOP-004: no in-flight sub-task is left
            // running), then SIGKILL all PTY process groups.
            while let Some(h) = handles.pop() {
                if self.abort_flag.load(Ordering::SeqCst) {
                    // Cancel every in-flight sub-task still pending (including
                    // this one), then SIGKILL all PTY process groups.
                    for remaining in &handles {
                        remaining.abort();
                    }
                    h.abort();
                    self.abort_pty_process_groups();
                    return Ok(results);
                }
                match h.await {
                    Ok(Ok(d)) => round.push(d),
                    Ok(Err(e)) => {
                        // A per-task failure is surfaced via its Deliverable
                        // marker (FAILED / REPLAN) rather than aborting the
                        // whole round; a hard error still aborts the loop.
                        tracing::warn!("delegation failed: {e}");
                        return Err(e);
                    }
                    Err(_) => {
                        // Task cancelled (abort raced) — stop immediately.
                        self.abort_pty_process_groups();
                        return Ok(results);
                    }
                }
            }
            results.extend(round);
            // `delegate()` auto-checked-off completed tasks (REQ-PLAN-002);
            // the next round re-reads the plan for whatever remains.
        }
        Ok(results)
    }
}

#[cfg(test)]
#[path = "loop_tests.rs"]
mod tests;
