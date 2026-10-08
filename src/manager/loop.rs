//! Turn-budget constants, wall-clock bounds and repeated-failure accounting
//! consumed by the **live** turn loops.
//!
//! # Why this module is small
//!
//! The `AgentLoop` / `ManagerLoop` state machines that used to live here were
//! **deleted** (decision + evidence: `docs/decision_dead_code_manager.md`,
//! recon item H8). Neither type was ever constructed outside `src/manager/`
//! (only unit tests), and both duplicated executors that *do* run in the
//! shipped binary:
//!
//! * Manager interactive turn loop (turn budget, steer queue, compaction
//!   trigger, parallel read-only tool fan-out, sequential write dispatch):
//!   `src/ui/session.rs`.
//! * Specialist turn loop (LLM call, XML rescue, repetition gate, tool
//!   dispatch, abort/cancellation): `src/agents/runner/execution.rs` and
//!   `src/agents/runner/fix_loop.rs`.
//! * Silent-dispatch delegation (one task per `delegate_task` call, auto
//!   check-off): `src/orchestrator/delegate.rs::handle_delegate_task` →
//!   `OrchestratorManager::delegate`.
//!
//! Keeping a second, un-instantiated copy of those loops meant ~40 green unit
//! tests certified behaviour the binary never ran, and the two executors could
//! (and did) drift. Only the items the live loops actually import remain here.
//!
//! # Requirements enforced by the surviving items
//!
//! * **REQ-LOOP-002** (turn limit half): the 100-turn cap below is enforced at
//!   `src/ui/session.rs`.
//! * **REQ-LOOP-002** (wall-clock half — restored by t-031c): the 600 s turn
//!   watchdog below is enforced **in the live session loop**
//!   (`src/ui/session.rs::run_session_with_bounds`), together with an absolute
//!   per-turn hard cap. The constants used to be enforced only by the deleted
//!   `AgentLoop::run_turn`, which the binary never called.
//! * **REQ-LOOP-003** (parallel read-only tools): the read/write split below is
//!   the gate the live loop uses to decide whether a batch of tool calls may be
//!   dispatched concurrently.
//! * **H5 failure budget** (recon `docs/recon_bugs_manager.md`): [`FailureBudget`]
//!   below is the per-session repeated-failure accounting the live loop applies
//!   per task id / tool call, so an impossible task cannot be retried blindly
//!   for the whole turn budget.

use crate::tool_names::{TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_READ_FILE};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Maximum number of turns per interactive request (REQ-LOOP-002).
///
/// Live caller: the Manager session loop in `src/ui/session.rs`, which breaks
/// its turn loop once this budget is exhausted.
pub const MAX_TURNS: usize = 100;

/// Wall-clock watchdog bound for **one** turn, in seconds (REQ-LOOP-002).
///
/// A turn is torn down once it has gone this long with **no observable
/// progress** — no status/event traffic from workers, no completed tool call,
/// no streamed steer arbitration. Progress-aware on purpose: a legitimately
/// long delegation keeps emitting `emit_status(...)` lines from
/// `src/agents/runner/{execution,fix_loop}.rs`, so it is never cut off, while a
/// genuinely stuck turn (hung tool, silent worker, wedged round) dies.
///
/// Live enforcement: `src/ui/session.rs` (checked in the tool-round poll loops
/// and against the in-flight backend call), with the original 600 s value of
/// the deleted `TURN_WATCHDOG_SECS`.
pub const TURN_WATCHDOG_SECS: u64 = 600;

/// Absolute wall-clock bound for **one** turn, in seconds, regardless of how
/// much progress it reports.
///
/// This is the backstop that makes an infinite turn impossible: a turn made of
/// many *individually* legitimate delegations (each up to tens of minutes) can
/// keep the [`TURN_WATCHDOG_SECS`] idle window refreshed forever. Deliberately
/// generous and config-independent — it is a runaway guard, not a UX timeout.
pub const TURN_HARD_CAP_SECS: u64 = 3 * 60 * 60;

/// How many failures of the same task/tool call are tolerated inside one
/// session before the live loop stops retrying it blindly (recon H5).
///
/// Reaching the threshold injects a strategy-change escalation into the model
/// context; the *next* attempt is refused outright and the turn ends.
pub const TASK_FAILURE_ESCALATION_THRESHOLD: u32 = 2;

/// Returns `true` for read-only tools eligible for parallel execution
/// (REQ-LOOP-003).
///
/// Every other tool — writers, `run_command`, plan tools, `rebirth`, PTY tools,
/// MCP tools and `delegate_task` — must run sequentially in the order it
/// appears in the assistant response.
pub fn is_read_tool(name: &str) -> bool {
    matches!(name, TOOL_READ_FILE | TOOL_GREP_SEARCH | TOOL_GLOB)
}

/// Why the wall-clock bound tore a turn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineKind {
    /// No observable progress for [`TURN_WATCHDOG_SECS`] worth of idle time.
    Stalled,
    /// The absolute per-turn bound was reached even though the turn was busy.
    HardCap,
}

impl DeadlineKind {
    /// Short, stable tag used in status lines and debug logs.
    pub fn label(self) -> &'static str {
        match self {
            DeadlineKind::Stalled => "stalled turn",
            DeadlineKind::HardCap => "turn deadline",
        }
    }

    /// User-visible reason. Wording is deliberately explicit: the turn was cut
    /// off by a bound, not by the user, and in-flight work was cancelled.
    pub fn describe(self, idle_limit: Duration, hard_limit: Duration) -> String {
        match self {
            DeadlineKind::Stalled => format!(
                "turn watchdog: no progress for {}s (limit {}s) — turn aborted and in-flight work cancelled",
                idle_limit.as_secs(),
                idle_limit.as_secs()
            ),
            DeadlineKind::HardCap => format!(
                "turn deadline: this turn exceeded its {}s wall-clock bound — turn aborted and in-flight work cancelled",
                hard_limit.as_secs()
            ),
        }
    }
}

/// Per-turn wall-clock watchdog (REQ-LOOP-002, wall-clock half).
///
/// Two independent bounds: an **idle** bound (no observable progress) and an
/// **absolute** hard cap. The session loop arms it per turn
/// ([`TurnWatchdog::rearm`]), feeds it observable progress
/// ([`TurnWatchdog::note_progress`]) and asks it before blocking on anything
/// that can hang.
///
/// The clock is injectable (`*_at` variants) so the bound is testable without
/// waiting 600 s.
#[derive(Debug, Clone, Copy)]
pub struct TurnWatchdog {
    started: Instant,
    last_progress: Instant,
    idle_limit: Duration,
    hard_limit: Duration,
}

impl TurnWatchdog {
    /// Arm a watchdog with the given idle / absolute bounds.
    pub fn new(idle_limit: Duration, hard_limit: Duration) -> Self {
        Self::at(Instant::now(), idle_limit, hard_limit)
    }

    /// Arm a watchdog anchored at an explicit start instant (test seam).
    pub fn at(start: Instant, idle_limit: Duration, hard_limit: Duration) -> Self {
        Self {
            started: start,
            last_progress: start,
            idle_limit,
            hard_limit,
        }
    }

    /// Start timing a new turn.
    pub fn rearm(&mut self) {
        self.rearm_at(Instant::now());
    }

    /// Start timing a new turn anchored at an explicit instant (test seam).
    pub fn rearm_at(&mut self, now: Instant) {
        self.started = now;
        self.last_progress = now;
    }

    /// Record observable forward progress, resetting the stalled-turn window.
    pub fn note_progress(&mut self) {
        self.note_progress_at(Instant::now());
    }

    /// [`TurnWatchdog::note_progress`] anchored at an explicit instant (test seam).
    pub fn note_progress_at(&mut self, now: Instant) {
        if now > self.last_progress {
            self.last_progress = now;
        }
    }

    /// Whether the turn must be torn down, and why (`None` = still inside the
    /// bounds).
    pub fn expired(&self) -> Option<DeadlineKind> {
        self.expired_at(Instant::now())
    }

    /// [`TurnWatchdog::expired`] anchored at an explicit instant (test seam).
    ///
    /// The hard cap is evaluated first: it is the unconditional bound, so a
    /// turn that is both idle and over-cap is reported as the harder failure.
    pub fn expired_at(&self, now: Instant) -> Option<DeadlineKind> {
        if self.elapsed_at(now) >= self.hard_limit {
            return Some(DeadlineKind::HardCap);
        }
        if self.idle_at(now) >= self.idle_limit {
            return Some(DeadlineKind::Stalled);
        }
        None
    }

    /// Total wall-clock time this turn has been running.
    pub fn elapsed(&self) -> Duration {
        self.elapsed_at(Instant::now())
    }

    /// [`TurnWatchdog::elapsed`] anchored at an explicit instant (test seam).
    pub fn elapsed_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }

    /// Idle time since the last observed progress.
    pub fn idle(&self) -> Duration {
        self.idle_at(Instant::now())
    }

    /// [`TurnWatchdog::idle`] anchored at an explicit instant (test seam).
    pub fn idle_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_progress)
    }

    /// Time left before the **absolute** per-turn bound.
    ///
    /// Used to bound an await whose internal progress the session loop cannot
    /// observe — a single backend call. The idle bound is not used there: a
    /// live-but-slow stream does not surface events into the session loop, so
    /// only the absolute bound may cut it off.
    pub fn time_to_hard_limit(&self) -> Duration {
        self.time_to_hard_limit_at(Instant::now())
    }

    /// [`TurnWatchdog::time_to_hard_limit`] anchored at an explicit instant
    /// (test seam).
    pub fn time_to_hard_limit_at(&self, now: Instant) -> Duration {
        self.hard_limit.saturating_sub(self.elapsed_at(now))
    }

    /// Time left before the **idle** bound fires.
    ///
    /// The live caller is the tool-round join loop: `join_poll_slice` in
    /// `src/ui/session.rs` caps each iteration's handle poll by this value, so a
    /// round being joined never sleeps past the moment the idle bound is allowed
    /// to break the wait (the slice is additionally floored at 1 ms so a
    /// sub-millisecond remainder cannot turn the loop into a spin). While a round
    /// is being joined, the idle bound — not the absolute cap — is what tears the
    /// wait down, so it is the bound the poll interval has to honour.
    pub fn time_to_idle_limit(&self) -> Duration {
        self.time_to_idle_limit_at(Instant::now())
    }

    /// [`TurnWatchdog::time_to_idle_limit`] anchored at an explicit instant
    /// (test seam).
    pub fn time_to_idle_limit_at(&self, now: Instant) -> Duration {
        self.idle_limit.saturating_sub(self.idle_at(now))
    }
}

/// Outcome of recording one more failure of the same key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureVerdict {
    /// Below the escalation threshold: an unchanged retry is still allowed.
    Continue { count: u32 },
    /// Threshold reached: the loop must surface a strategy-change escalation
    /// before any further attempt.
    Escalate { count: u32 },
}

/// Per-session accounting of repeated failures of the same task/tool call
/// (recon H5: "no failure budget").
///
/// Deliberately a plain `HashMap<String, u32>`: explicit, local to the session
/// loop, and trivially auditable. A key is a plan task id for delegated work
/// (`task:t-012`) or a tool signature for direct tool calls
/// (`tool:read_file:{...}`).
///
/// Policy with the default threshold of 2:
/// 1. failure → `Continue { 1 }` — retry is allowed,
/// 2. failure → `Escalate { 2 }` — inject a strategy-change notice,
/// 3. next attempt → [`FailureBudget::retry_allowed`] is `false`, so the call is
///    **never dispatched**; the session loop stops and says why.
#[derive(Debug, Clone, Default)]
pub struct FailureBudget {
    counts: HashMap<String, u32>,
    threshold: u32,
}

impl FailureBudget {
    /// Budget with the given escalation threshold. A threshold of `0` is
    /// meaningless (it would refuse every call before the first attempt), so it
    /// is clamped to `1`.
    pub fn new(threshold: u32) -> Self {
        Self {
            counts: HashMap::new(),
            threshold: threshold.max(1),
        }
    }

    /// The effective escalation threshold.
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Recorded failure count for `key`.
    pub fn count(&self, key: &str) -> u32 {
        self.counts.get(key).copied().unwrap_or(0)
    }

    /// Whether an unchanged retry of `key` may be dispatched at all.
    pub fn retry_allowed(&self, key: &str) -> bool {
        self.count(key) < self.threshold
    }

    /// Record one failure of `key` and report what the loop must do next.
    pub fn record(&mut self, key: &str) -> FailureVerdict {
        let entry = self.counts.entry(key.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        let count = *entry;
        if count >= self.threshold {
            FailureVerdict::Escalate { count }
        } else {
            FailureVerdict::Continue { count }
        }
    }

    /// A success pays off the debt for `key`.
    pub fn clear(&mut self, key: &str) {
        self.counts.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_names::{
        TOOL_CREATE_PLAN, TOOL_DELEGATE_TASK, TOOL_READ_FILE, TOOL_REBIRTH, TOOL_REPLACE,
        TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
    };

    /// REQ-LOOP-003: exactly the three read-only tools are parallel-safe.
    #[test]
    fn read_tool_gate_covers_only_read_only_tools() {
        assert!(is_read_tool(TOOL_READ_FILE));
        assert!(is_read_tool(TOOL_GREP_SEARCH));
        assert!(is_read_tool(TOOL_GLOB));
    }

    /// REQ-LOOP-003: mutating, executing and orchestration tools are never
    /// classified as parallel-safe reads.
    #[test]
    fn mutating_and_orchestration_tools_are_not_reads() {
        for tool in [
            TOOL_WRITE_FILE,
            TOOL_REPLACE,
            TOOL_RUN_COMMAND,
            TOOL_CREATE_PLAN,
            TOOL_REBIRTH,
            TOOL_DELEGATE_TASK,
            "mcp__server__anything",
            "",
        ] {
            assert!(!is_read_tool(tool), "{tool} must not run in parallel");
        }
    }

    /// REQ-LOOP-002: the interactive turn budget stays at 100 turns.
    #[test]
    fn turn_budget_is_one_hundred() {
        assert_eq!(MAX_TURNS, 100);
    }

    /// REQ-LOOP-002: the 600 s watchdog bound exists again, and it is paired
    /// with an absolute per-turn cap (the H5 wall-clock gap).
    #[test]
    fn turn_wall_clock_bounds_are_declared() {
        assert_eq!(TURN_WATCHDOG_SECS, 600);
        assert_eq!(TURN_HARD_CAP_SECS, 3 * 60 * 60);
        let hard_cap = Duration::from_secs(TURN_HARD_CAP_SECS);
        let watchdog = Duration::from_secs(TURN_WATCHDOG_SECS);
        assert!(hard_cap > watchdog);
        assert_eq!(TASK_FAILURE_ESCALATION_THRESHOLD, 2);
    }

    // ---- TurnWatchdog -----------------------------------------------------

    /// REQ-LOOP-002: a turn that reports no progress is cut off at the idle
    /// bound, and the reason names the watchdog.
    #[test]
    fn watchdog_fires_when_a_turn_makes_no_progress() {
        let start = Instant::now();
        let wd = TurnWatchdog::at(
            start,
            Duration::from_secs(TURN_WATCHDOG_SECS),
            Duration::from_secs(TURN_HARD_CAP_SECS),
        );
        assert_eq!(wd.expired_at(start + Duration::from_secs(599)), None);
        assert_eq!(
            wd.expired_at(start + Duration::from_secs(600)),
            Some(DeadlineKind::Stalled)
        );
        let reason = DeadlineKind::Stalled.describe(
            Duration::from_secs(TURN_WATCHDOG_SECS),
            Duration::from_secs(TURN_HARD_CAP_SECS),
        );
        assert!(reason.contains("turn watchdog"));
        assert!(reason.contains("in-flight work cancelled"));
    }

    /// The idle window is relative to the last observed progress: a busy turn
    /// (delegation status lines, completed tool calls) is never cut off by the
    /// watchdog, only by the hard cap.
    #[test]
    fn watchdog_progress_refreshes_the_stalled_window() {
        let start = Instant::now();
        let mut wd = TurnWatchdog::at(start, Duration::from_secs(10), Duration::from_secs(1000));
        wd.note_progress_at(start + Duration::from_secs(9));
        assert_eq!(wd.expired_at(start + Duration::from_secs(18)), None);
        assert_eq!(
            wd.expired_at(start + Duration::from_secs(19)),
            Some(DeadlineKind::Stalled)
        );
        assert_eq!(
            wd.idle_at(start + Duration::from_secs(15)),
            Duration::from_secs(6)
        );
    }

    /// The absolute cap fires even for a turn that keeps reporting progress —
    /// this is the bound that makes an infinite turn impossible.
    #[test]
    fn watchdog_hard_cap_fires_regardless_of_progress() {
        let start = Instant::now();
        let mut wd = TurnWatchdog::at(start, Duration::from_secs(600), Duration::from_secs(30));
        for tick in 1..=10 {
            wd.note_progress_at(start + Duration::from_secs(tick * 3));
        }
        assert_eq!(wd.expired_at(start + Duration::from_secs(29)), None);
        assert_eq!(
            wd.expired_at(start + Duration::from_secs(30)),
            Some(DeadlineKind::HardCap)
        );
        assert!(
            DeadlineKind::HardCap
                .describe(Duration::from_secs(600), Duration::from_secs(30))
                .contains("30s wall-clock bound")
        );
    }

    /// The remaining-time helpers feed `tokio::time::timeout` and never underflow.
    #[test]
    fn watchdog_reports_remaining_await_budgets() {
        let start = Instant::now();
        let wd = TurnWatchdog::at(start, Duration::from_secs(10), Duration::from_secs(30));
        assert_eq!(wd.time_to_idle_limit_at(start), Duration::from_secs(10));
        assert_eq!(wd.time_to_hard_limit_at(start), Duration::from_secs(30));
        // Well past both bounds: saturating, never negative / never panicking.
        let late = start + Duration::from_secs(500);
        assert_eq!(wd.time_to_idle_limit_at(late), Duration::ZERO);
        assert_eq!(wd.time_to_hard_limit_at(late), Duration::ZERO);
    }

    /// `rearm` bounds exactly one turn: the previous turn's elapsed time is not
    /// carried over into the next turn's budget.
    #[test]
    fn watchdog_rearm_bounds_one_turn() {
        let start = Instant::now();
        let mut wd = TurnWatchdog::at(start, Duration::from_secs(5), Duration::from_secs(6));
        // The previous turn had already burned 5 s when the loop re-armed.
        wd.rearm_at(start + Duration::from_secs(5));
        // Nothing is carried over into the new turn.
        assert_eq!(
            wd.elapsed_at(start + Duration::from_secs(6)),
            Duration::from_secs(1)
        );
        // The absolute cap is measured from the re-arm too: 9 s of total session
        // time would exceed a 6 s cap, but this turn has only used 4 s.
        assert_eq!(wd.expired_at(start + Duration::from_secs(9)), None);
        // 5 s of silence inside the new turn is the stalled bound...
        assert_eq!(
            wd.expired_at(start + Duration::from_secs(10)),
            Some(DeadlineKind::Stalled)
        );
        // ...whereas observable progress keeps the idle bound quiet and leaves the
        // absolute bound as the only way out: 6 s into the re-armed turn.
        wd.note_progress_at(start + Duration::from_secs(9));
        assert_eq!(
            wd.expired_at(start + Duration::from_secs(11)),
            Some(DeadlineKind::HardCap)
        );
    }

    // ---- FailureBudget ----------------------------------------------------

    /// Recon H5: two failures of the same task escalate, and the third attempt
    /// is refused instead of being retried blindly.
    #[test]
    fn failure_budget_escalates_at_threshold_and_then_refunds_no_more_attempts() {
        let mut budget = FailureBudget::new(TASK_FAILURE_ESCALATION_THRESHOLD);
        let key = "task:t-042";

        assert!(budget.retry_allowed(key));
        assert_eq!(budget.record(key), FailureVerdict::Continue { count: 1 });
        assert!(budget.retry_allowed(key));
        assert_eq!(budget.record(key), FailureVerdict::Escalate { count: 2 });
        // Threshold reached: no further unchanged attempt may be dispatched.
        assert!(!budget.retry_allowed(key));
        assert_eq!(budget.count(key), 2);
    }

    /// A success pays off the debt, so a task that eventually works is not
    /// penalised for its earlier failures.
    #[test]
    fn failure_budget_success_clears_the_counter() {
        let mut budget = FailureBudget::new(2);
        let key = "task:t-007";
        budget.record(key);
        budget.record(key);
        assert!(!budget.retry_allowed(key));
        budget.clear(key);
        assert_eq!(budget.count(key), 0);
        assert!(budget.retry_allowed(key));
        assert_eq!(budget.record(key), FailureVerdict::Continue { count: 1 });
    }

    /// Different tasks/tool calls are accounted separately: one impossible task
    /// must not freeze the rest of the plan.
    #[test]
    fn failure_budget_keys_are_independent() {
        let mut budget = FailureBudget::new(2);
        budget.record("task:t-001");
        budget.record("task:t-001");
        budget.record("tool:read_file:{\"path\":\"nope\"}");
        assert!(!budget.retry_allowed("task:t-001"));
        assert!(budget.retry_allowed("task:t-002"));
        assert!(budget.retry_allowed("tool:read_file:{\"path\":\"nope\"}"));
    }

    /// A degenerate threshold may never refuse a call before it was attempted.
    #[test]
    fn failure_budget_threshold_is_clamped_to_one() {
        let mut budget = FailureBudget::new(0);
        assert_eq!(budget.threshold(), 1);
        assert_eq!(
            budget.record("task:t-009"),
            FailureVerdict::Escalate { count: 1 }
        );
        assert!(!budget.retry_allowed("task:t-009"));
    }
}
