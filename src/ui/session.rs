//! Interactive session runner driving multi-turn manager and specialist execution.

use super::bridge::{
    RendererSink, SteerArbEvent, drain_steer_arbitration_events_with_transcript,
    spawn_steer_arbitration,
};
use super::helpers::*;
use super::{Event, Renderer, SubagentDetail, UiRecord, UiTranscript};
use crate::config::Config;
use crate::llm::{ChatClient, StreamConfig, chat_client_turn};
use crate::manager::context::{
    ABORTED_TOOL_RESULT, CompactionOutcome, ContextEngine, count_text_tokens, manager_wire_tools,
};
use crate::manager::r#loop::{DeadlineKind, FailureBudget, FailureVerdict, TurnWatchdog};
use crate::orchestrator::OrchestratorManager;
use crate::orchestrator::pool;
use crate::session::commit::{CapturePayload, CommitError, CommitLock, CommitPolicy};
use crate::session::restore::RestoreRefused;
use crate::session::{
    DanglingToolCallState, IdentityFloors, NonRestorableKind, NonRestorableNotice, ProcessEnding,
    RepairReport, RestoreOutcome, SubagentPanel, UiJournalRecord, UiSection, WorkerSection,
};
use crate::types::Message;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Execution bounds applied to the live session loop: a failure budget and a
/// wall-clock bound.
///
/// Production always runs on [`SessionBounds::default()`]. The bounds are
/// deliberately **config-independent**: they are a runaway safety net for the
/// interactive loop, not a user-facing knob, and putting them in `marmel.toml`
/// would let a bad config silently remove the only bound that exists.
///
/// The explicit variant exists because a 600 s watchdog and a 3 h hard cap
/// cannot be exercised hermetically at their production values;
/// `tests/test_ui_session.rs` drives the *real* loop with small ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionBounds {
    /// Wall-clock watchdog: how long one turn may go with **no observable
    /// progress** before it is torn down.
    pub turn_idle: Duration,
    /// Absolute wall-clock bound for a single turn, progress or not.
    pub turn_hard_cap: Duration,
    /// How many failures of the same task/tool call are tolerated inside one
    /// session before the loop escalates and then refuses to retry.
    pub failure_threshold: u32,
}

impl Default for SessionBounds {
    fn default() -> Self {
        Self {
            turn_idle: Duration::from_secs(crate::manager::r#loop::TURN_WATCHDOG_SECS),
            turn_hard_cap: Duration::from_secs(crate::manager::r#loop::TURN_HARD_CAP_SECS),
            failure_threshold: crate::manager::r#loop::TASK_FAILURE_ESCALATION_THRESHOLD,
        }
    }
}

// ── Why a session execution bound ended a turn ───────────────────────────────
//
// The session loop has several ways to stop a turn: the wall-clock halves of
// [`SessionBounds`], and the per-request **turn budget**
// ([`crate::manager::r#loop::MAX_TURNS`]). A bare `turn_count > MAX_TURNS` break
// stops the loop **silently**: an operator sees a session stop responding once the
// budget is spent, with nothing distinguishing that from a clean end-of-turn
// finish.
//
// The vocabulary below is the single stop-reason type the loop carries:
// every bound exit produces one, it is surfaced through the session's existing
// status channel (renderer `Message`/`Status` event + a `Status` record in the
// UI transcript + `tracing::warn!`), and it exposes a stable machine-readable
// [`BoundStopReason::code`] so a caller, a log field, or the journaled line can
// tell "turn budget reached" apart from a clean finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundStopReason {
    /// A wall-clock half of [`SessionBounds`] tore the turn down. `text` is the
    /// user-visible wording owned by [`DeadlineKind::describe`].
    Deadline { kind: DeadlineKind, text: String },
    /// The per-request turn budget was exhausted: `limit` turns are allowed and
    /// all of them ran, so turn `turn` was refused. (The count is deliberately
    /// reported as *refused*, matching the loop's pre-incremented counter:
    /// exactly `limit` turns ran, so that off-by-one is intentional.)
    TurnCap { limit: usize, turn: usize },
}

impl BoundStopReason {
    /// Stable machine-readable tag for logs, journal lines and the session
    /// report. Never reworded without updating every consumer of the journal.
    pub fn code(&self) -> &'static str {
        match self {
            BoundStopReason::Deadline {
                kind: DeadlineKind::Stalled,
                ..
            } => "turn_watchdog",
            BoundStopReason::Deadline {
                kind: DeadlineKind::HardCap,
                ..
            } => "turn_hard_cap",
            BoundStopReason::TurnCap { .. } => "turn_cap",
        }
    }

    /// Short tag used in the `(Ready)` status line.
    ///
    /// The deadline branch wording is byte-stable so the existing bound status
    /// lines do not move.
    pub fn label(&self) -> &'static str {
        match self {
            BoundStopReason::Deadline { text, .. } => {
                if text.contains("hard cap") {
                    "turn hard cap"
                } else {
                    "turn watchdog"
                }
            }
            BoundStopReason::TurnCap { .. } => "turn budget",
        }
    }

    /// The user-visible (and journaled) reason. The turn-budget wording leads
    /// with [`BoundStopReason::code`] so the journaled line is machine-readable
    /// as well as human-readable, and names both the real limit and the turn
    /// count.
    pub fn text(&self) -> String {
        match self {
            BoundStopReason::Deadline { text, .. } => text.clone(),
            BoundStopReason::TurnCap { limit, turn } => format!(
                "{}: turn budget exhausted — the per-request limit is {limit} turns and all {limit} ran, so turn {turn} was refused. This request stopped at the turn budget, not because the work finished; split the goal or send a new instruction to continue.",
                self.code()
            ),
        }
    }
}

/// What the session loop reports back to its caller: the machine-readable
/// counterpart of the messages the operator was shown.
///
/// This is what makes a capped finish distinguishable from a clean one from
/// *outside* the UI, which `run_session` alone cannot do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStopReport {
    /// Every session execution bound that cut a turn off, in the order it fired.
    /// Empty means no bound was hit: the turns finished on their own (user exit,
    /// abort or a clean end of turn).
    pub stops: Vec<BoundStopReason>,
}

impl SessionStopReport {
    /// Was the per-request turn budget exhausted at any point during the
    /// session? A capped finish must never be reportable as a clean finish.
    pub fn turn_cap_reached(&self) -> bool {
        self.stops
            .iter()
            .any(|stop| matches!(stop, BoundStopReason::TurnCap { .. }))
    }

    /// The stable machine-readable codes of every bound hit, in order.
    pub fn codes(&self) -> Vec<&'static str> {
        self.stops.iter().map(BoundStopReason::code).collect()
    }
}

// ── The Manager request budget ───────────────────────────────────────────────
//
// [`ContextEngine::should_compact`] prices the **request**, i.e. transcript plus
// the serialized `ChatRequest.tools` payload
// ([`crate::manager::context::request_tokens`]). That schema term is only real if
// the live path declares the list it actually sends: an engine that never calls
// [`ContextEngine::set_tools`] budgets messages alone and therefore under-counts
// every Manager request by the size of the advertised schemas (thousands of
// tokens for the Manager default list). [`manager_wire_tools`] is the same
// assembly `src/llm/stream.rs::build_request` performs, so the charged schema and
// the wire schema are equal **by construction** — never a superset (which would
// compact transcript away too early) and never a subset (which would let an
// over-budget request through).

/// Build the session's Manager context engine, pre-charged with the exact tool
/// schemas the Manager advertises.
///
/// The live loop constructs its engine **here** so the wiring is a single,
/// test-reachable line instead of an inline `ContextEngine::new` that silently
/// budgets messages only. The pinned goal is deliberately *not* set here: the
/// interactive goal does not exist until the first input is read, and
/// `run_session_with_bounds` pins it with [`ContextEngine::set_goal`] as before.
pub fn build_manager_context(cfg: &Config, system_prompt: String) -> ContextEngine {
    let mut ctx = ContextEngine::new(cfg.max_context_tokens);
    ctx.set_system_prompt(system_prompt);
    sync_manager_tool_schema(&mut ctx, cfg);
    ctx
}

/// Re-charge `ctx` with the tool schemas the Manager advertises **right now** and
/// return the charged schema token count.
///
/// The advertised Manager list is not static: MCP servers boot lazily and
/// reconnect, and the advertised MCP view is policy-filtered per request
/// ([`crate::harness::allowed_mcp_tools`]). The session therefore re-declares the
/// list at every budget decision rather than trusting a one-time construction
/// charge — a stale charge would compact against a schema the wire does not
/// carry.
pub fn sync_manager_tool_schema(ctx: &mut ContextEngine, cfg: &Config) -> usize {
    let previous = ctx.tool_schema_tokens();
    let advertised = manager_wire_tools(&cfg.orchestration.mcp_servers);
    ctx.set_tools(&advertised);
    let charged = ctx.tool_schema_tokens();
    if previous != charged {
        tracing::info!(
            "tool-schema budget {}: {previous} -> {charged} tokens across {} advertised tool(s)",
            if previous == 0 {
                "charged"
            } else {
                "re-priced"
            },
            advertised.len(),
        );
    }
    charged
}

// ---------------------------------------------------------------------------
// Boot-time restore: the ONE exact restore pass
// ---------------------------------------------------------------------------

/// `--resume` as spelled on the command line.
///
/// **Documented deviation:** the design prefers an explicit parameter through
/// `ui::tui::run` / `ui::raw::run` and rejects smuggling the trigger through
/// `Config`. Both `run` entry points (`ui/tui/mod.rs:1602`, `ui/raw.rs:20`) and
/// `CliArgs` itself live in other modules, so the only seam left is a one-shot
/// handoff from `main` — the only place that sees `CliArgs` — to the module that
/// owns the session loop, made exactly once via [`set_resume_request`].
static RESUME_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The `--fresh` half of the [`RESUME_REQUESTED`] handoff.
static FRESH_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Hand the CLI's resume decision to the session loop.
pub fn set_resume_request(resume: bool, fresh: bool) {
    RESUME_REQUESTED.store(resume, Ordering::SeqCst);
    FRESH_REQUESTED.store(fresh, Ordering::SeqCst);
}

/// The resume trigger, resolved with this precedence:
///
/// ```text
///   --fresh                    wins over everything else: never restore
///   --resume                   restore
///   MARMEL_RESUME=<bool>       restore (environment)
///   session_resume = true      restore (config file)
///   none of the above          fresh start — byte-for-byte today's behaviour
/// ```
///
/// `MARMEL_RESUME` is already folded into `cfg.session_resume` by
/// `config::apply_session_env` (`config.rs:423`), env beating the file, so those
/// two spellings are a single read here and neither can outrank `--fresh`.
/// `--resume --fresh` and `--resume` with capture disabled are usage errors that
/// `main` exits `2` on (`main.rs:41`) long before this is consulted.
pub fn resume_requested(cfg: &Config) -> bool {
    (RESUME_REQUESTED.load(Ordering::SeqCst) || cfg.session_resume)
        && !FRESH_REQUESTED.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Session commit triggers
//
// WHEN a bundle is written, and by whom. **Only** the thread that owns
// `ctx`, `ui_transcript` and `renderer` may commit — `Renderer: Send` but **not**
// `Sync`, so `capture_ui` is legal only here. Worker threads therefore never
// write a file: [`mark_worker_lifecycle_edge`] records that a worker mirror
// changed, and this pump turns that into a `workers/*.json` member on the
// session thread.
//
// **Coalescing**: the 24 legacy `save_transcript` sites keep their
// event semantics but only mirror in memory ([`manager_capture`]); a bundle is
// written once per trigger, with T-4/T-5 throttled to
// `Config::session_commit_min_interval_ms` and mandatory triggers (T-1, T-2,
// T-3, T-9) never throttled.
//
// **Interrupted-session triggers**: T-6/T-7 (`UserAbort`), T-8
// (`PanicHook`, [`commit_from_panic`]) and T-10 (`Reset` / `PlanArchived`,
// [`mark_ending_commit`] + [`confirm_bundle_swept`]). These are the endings that
// make an INTERRUPTED run restorable, so none of them is ever throttled and none
// of them may be skipped: an abort is frequently the last bundle a dying process
// writes.
// ---------------------------------------------------------------------------

/// Why a commit was requested — the `T-n` trigger vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitTrigger {
    /// T-1: one round of the turn loop ended, after the pairing repair.
    ManagerTurnBoundary,
    /// T-2: the assistant `tool_calls` batch is persisted, nothing dispatched yet.
    PreDispatch,
    /// T-3: a tool result landed.
    ToolResult,
    /// T-4: a UI-visible change (throttled).
    UiEvent,
    /// T-5: a worker lifecycle edge (throttled).
    WorkerEdge,
    /// T-9: the session loop reached its tail.
    CleanTail,
    /// T-6/T-7: the user aborted — `/abort`, the two-stage Ctrl-C/Esc
    /// confirm, or an abort the loop observed mid-round. Mandatory: this is
    /// frequently the LAST bundle a dying process ever writes.
    UserAbort,
    /// T-10: `/reset`. The ending is recorded first, then
    /// `Plan::clear()`/`sweep_session_state()` deletes the bundle.
    Reset,
    /// T-10: the plan was archived — the session is **finished**, which
    /// is a different verdict from "interrupted".
    PlanArchived,
}

impl CommitTrigger {
    /// Only T-4/T-5 respect the rate floor; losing T-1/T-2/T-3/T-9
    /// loses facts.
    fn throttled(self) -> bool {
        matches!(self, CommitTrigger::UiEvent | CommitTrigger::WorkerEdge)
    }

    /// The ending this trigger writes. Every in-loop writer here records
    /// `InProgress`; the loop tail records `CleanTail`, an abort records
    /// `UserAbort` and a reset or plan archive records `Reset`/`PlanArchived`.
    /// That mapping is what lets a later boot tell `CleanTail`, an honest abort
    /// and `HardKillInferred` apart at all.
    fn ending(self) -> ProcessEnding {
        match self {
            CommitTrigger::CleanTail => ProcessEnding::CleanTail,
            CommitTrigger::UserAbort => ProcessEnding::UserAbort,
            CommitTrigger::Reset => ProcessEnding::Reset,
            CommitTrigger::PlanArchived => ProcessEnding::PlanArchived,
            _ => ProcessEnding::InProgress,
        }
    }
}

/// What one request decided. Kept so a commit result is never discarded;
/// it lands in [`session_commit_ledger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitDecision {
    /// A bundle was written; `members` counts the members actually renamed.
    Written { members: usize },
    /// Inside the `session_commit_min_interval_ms` window: folded into the next commit.
    Throttled,
    /// Switched off by config (`session_commit_on_tool_result = false`, or the
    /// bundle is already over `session_max_bundle_bytes`).
    Suppressed,
    /// `session_enabled = false`: nothing is written, ever.
    Disabled,
    /// Another commit holds the session commit lock.
    Busy,
    /// The commit failed; the reason is logged, never swallowed.
    Failed,
}

/// The process-wide capture ledger: every request and every decision, so a
/// commit result is never thrown away and a test (or an operator) can tell how
/// many bundles a run really wrote.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionCommitLedger {
    pub requests: u64,
    pub written: u64,
    pub throttled: u64,
    pub suppressed: u64,
    pub disabled: u64,
    pub busy: u64,
    pub failed: u64,
    pub pre_dispatch: u64,
    pub turn_boundary: u64,
    pub tool_result: u64,
    pub ui_event: u64,
    pub worker_edge: u64,
    pub clean_tail: u64,
    /// T-6/T-7 abort requests.
    pub user_abort: u64,
    /// T-10 `/reset` requests.
    pub reset: u64,
    /// T-10 plan-archive requests.
    pub plan_archived: u64,
    /// T-8 panic-hook flushes that reached the committer.
    pub panic_hook: u64,
    /// Successful commits whose manager member was **dangling** (the T-2 witness:
    /// an assistant `tool_calls` batch persisted before its results existed).
    pub dangling_commits: u64,
    /// Dangling call count in the most recent successful commit.
    pub last_dangling_calls: usize,
    /// `ending` of the most recent successful commit.
    pub last_ending: Option<ProcessEnding>,
}

static COMMIT_LEDGER: std::sync::LazyLock<std::sync::Mutex<SessionCommitLedger>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(SessionCommitLedger::default()));

fn ledger() -> std::sync::MutexGuard<'static, SessionCommitLedger> {
    COMMIT_LEDGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A copy of the ledger (the counters a caller/tests read).
pub fn session_commit_ledger() -> SessionCommitLedger {
    ledger().clone()
}

/// A worker lifecycle edge is pending: set from `orchestrator::workers` — which
/// may run on a **worker** thread — and consumed only here, on the session
/// thread. Deliberately an atomic, never a write: only this thread consumes it.
static WORKER_EDGE: AtomicBool = AtomicBool::new(false);

/// Record that a worker started / completed / failed / was preempted, and that
/// its journal must therefore be harvested into the next bundle.
pub fn mark_worker_lifecycle_edge() {
    WORKER_EDGE.store(true, Ordering::SeqCst);
}

/// Take the pending edge flag.
pub fn take_worker_lifecycle_edge() -> bool {
    WORKER_EDGE.swap(false, Ordering::SeqCst)
}

/// The worker members: every live registry entry joined with its mirror.
/// Both sides are clones with no map guard outliving the call, so a harvest can
/// never deadlock against a worker.
fn harvest_worker_sections() -> Vec<WorkerSection> {
    crate::orchestrator::workers::live_worker_states()
        .into_iter()
        .map(|(key, state)| {
            let role = crate::agents::Agent::from_str(&state.info.agent_name)
                .unwrap_or(crate::agents::Agent::Coder);
            let journal = crate::session::worker_journal::registry_get(&key);
            crate::session::worker_journal::project_worker_section(
                &key,
                &state,
                role,
                journal.as_deref(),
            )
        })
        .collect()
}

/// The floors of the three identity counters, read at commit time.
fn current_identity_floors() -> IdentityFloors {
    IdentityFloors {
        worker_id_seq_floor: crate::orchestrator::workers::worker_id_seq_floor(),
        completed_seq_floor: crate::orchestrator::workers::completed_seq_floor(),
        notice_id_floor: crate::orchestrator::notice::notice_id_floor(),
    }
}

/// Mirror the manager state **and** request a commit — the pair behind every
/// transcript-mutating site. The mirror is always cheap and in-memory; the
/// commit is the pump's decision, so most calls write nothing at all.
fn capture_and_request(
    pump: &mut SessionCommitPump,
    trigger: CommitTrigger,
    ctx: &ContextEngine,
    renderer: &dyn Renderer,
) {
    crate::session::manager_capture::capture_into_session_state(ctx);
    pump.request(trigger, ctx, renderer);
}

/// The session-thread commit pump. One per session; it owns the throttle clock,
/// the bundle identity and the [`CommitLock`].
#[derive(Debug)]
pub struct SessionCommitPump {
    bundle: std::path::PathBuf,
    session_id: String,
    enabled: bool,
    policy: CommitPolicy,
    /// Shared with the panic hook ([`PANIC_TARGET`]) as an `Arc`, so a hook that
    /// runs while this pump's thread is unwinding can still `try_guard` it and
    /// never blocks on its owner.
    lock: Arc<CommitLock>,
    last_commit: Option<std::time::Instant>,
    /// A throttled request still owed a commit.
    pending: bool,
    /// The bundle went over `session_max_bundle_bytes`: stop issuing *throttled*
    /// commits, keep the mandatory ones (the committer never truncates).
    over_ceiling: bool,
}

impl SessionCommitPump {
    /// `session_id` is the restored id when one exists (identity continuity);
    /// a fresh session mints one, because the manifest, the member
    /// headers and the L4/L5 cross-checks all require it.
    pub fn new(plan: &crate::manager::phase::Plan, cfg: &Config, session_id: &str) -> Self {
        let bundle = crate::session::bundle_dir(plan);
        let session_id = if session_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            session_id.to_string()
        };
        let policy = CommitPolicy {
            min_interval: Duration::from_millis(cfg.session_commit_min_interval_ms),
            max_bundle_bytes: cfg.session_max_bundle_bytes,
            allow_throttled: cfg.session_commit_on_tool_result,
        };
        let lock = Arc::new(CommitLock::new());
        if cfg.session_enabled {
            // Register what the panic hook may reach. Capture
            // being switched off leaves the hook with nothing to flush, which is
            // the honest answer — this run never wrote a bundle.
            let mut slot = PANIC_TARGET
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(PanicFlushTarget {
                bundle: bundle.clone(),
                session_id: session_id.clone(),
                policy: policy.clone(),
                lock: Arc::clone(&lock),
            });
        }
        SessionCommitPump {
            bundle,
            session_id,
            enabled: cfg.session_enabled,
            policy,
            lock,
            last_commit: None,
            pending: false,
            over_ceiling: false,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The pump's commit lock. Read-only access, for diagnostics and tests: the
    /// panic hook holds the SAME `Arc` (it was handed one at construction) and
    /// only ever *tries* it — nothing on that path may block.
    pub fn commit_lock(&self) -> &CommitLock {
        &self.lock
    }

    pub fn bundle(&self) -> &std::path::Path {
        &self.bundle
    }

    /// Has this pump actually written the bundle at least once? Ending markers for
    /// an aborted run consult this: an abort at the goal prompt must not stamp a
    /// previous run's bundle (which is what would be found at boot) with an empty
    /// transcript and a `userAbort` ending it never earned.
    pub fn has_committed(&self) -> bool {
        self.last_commit.is_some()
    }

    /// Is a throttled request still owed a commit?
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// One trigger request: mirror stays, bundle is written only at the throttle
    /// boundary or when the trigger is mandatory.
    pub fn request(
        &mut self,
        trigger: CommitTrigger,
        ctx: &ContextEngine,
        renderer: &dyn Renderer,
    ) -> CommitDecision {
        {
            let mut slot = ledger();
            slot.requests += 1;
            match trigger {
                CommitTrigger::PreDispatch => slot.pre_dispatch += 1,
                CommitTrigger::ManagerTurnBoundary => slot.turn_boundary += 1,
                CommitTrigger::ToolResult => slot.tool_result += 1,
                CommitTrigger::UiEvent => slot.ui_event += 1,
                CommitTrigger::WorkerEdge => slot.worker_edge += 1,
                CommitTrigger::CleanTail => slot.clean_tail += 1,
                CommitTrigger::UserAbort => slot.user_abort += 1,
                CommitTrigger::Reset => slot.reset += 1,
                CommitTrigger::PlanArchived => slot.plan_archived += 1,
            }
        }
        if !self.enabled {
            return decide(CommitDecision::Disabled);
        }
        // `session_commit_on_tool_result` switches T-3 off entirely.
        if trigger == CommitTrigger::ToolResult && !self.policy.allow_throttled {
            return decide(CommitDecision::Suppressed);
        }
        if trigger.throttled() {
            if self.over_ceiling {
                return decide(CommitDecision::Suppressed);
            }
            if let Some(at) = self.last_commit
                && at.elapsed() < self.policy.min_interval
            {
                self.pending = true;
                return decide(CommitDecision::Throttled);
            }
        } else {
            // A mandatory trigger wins and cancels the pending one.
            self.pending = false;
        }
        self.commit(trigger, ctx, renderer)
    }

    /// Write now. `ending` comes from the trigger, never guessed.
    fn commit(
        &mut self,
        trigger: CommitTrigger,
        ctx: &ContextEngine,
        renderer: &dyn Renderer,
    ) -> CommitDecision {
        let manager = ctx.capture_manager_section();
        let dangling = match &manager.dangling {
            DanglingToolCallState::Dangling { calls } => calls.len(),
            _ => 0,
        };
        let payload = CapturePayload {
            manager,
            ui: Some(renderer.capture_ui()),
            workers: harvest_worker_sections(),
            identity_floors: current_identity_floors(),
        };
        let outcome = crate::session::commit::write_bundle(
            &self.bundle,
            &self.session_id,
            &payload,
            trigger.ending(),
            &self.policy,
            &self.lock,
        );
        match outcome {
            Ok(report) => {
                self.last_commit = Some(std::time::Instant::now());
                self.pending = false;
                let total: u64 = crate::session::commit::last_manifest(&self.bundle)
                    .map(|manifest| manifest.members.iter().map(|m| m.byte_len).sum())
                    .unwrap_or(0);
                if total > self.policy.max_bundle_bytes && !self.over_ceiling {
                    self.over_ceiling = true;
                    tracing::warn!(
                        total_bytes = total,
                        max_bundle_bytes = self.policy.max_bundle_bytes,
                        "session bundle is over session_max_bundle_bytes: throttled triggers \
                         stop committing, mandatory triggers (turn boundary, pre-dispatch, \
                         clean tail) keep going"
                    );
                }
                let mut slot = ledger();
                slot.written += 1;
                slot.last_dangling_calls = dangling;
                slot.last_ending = Some(trigger.ending());
                if dangling > 0 {
                    slot.dangling_commits += 1;
                }
                drop(slot);
                tracing::debug!(
                    trigger = ?trigger,
                    members = report.members_written.len(),
                    skipped_identical = report.members_skipped_identical,
                    orphans_removed = report.orphans_removed,
                    "session bundle committed"
                );
                CommitDecision::Written {
                    members: report.members_written.len(),
                }
            }
            Err(CommitError::Locked) => decide(CommitDecision::Busy),
            Err(err) => {
                tracing::warn!("session commit failed: {err}");
                decide(CommitDecision::Failed)
            }
        }
    }
}

/// The ONLY thing a panic hook may reach about a session: the
/// bundle identity, the commit policy, and the session's [`CommitLock`] — never
/// the renderer (`TuiRenderer: !Sync`) and never the engine (`ctx` lives on the
/// session thread).
#[derive(Debug, Clone)]
pub struct PanicFlushTarget {
    pub bundle: std::path::PathBuf,
    pub session_id: String,
    pub policy: CommitPolicy,
    pub lock: Arc<CommitLock>,
}

/// The live session's panic-flush target. Written by [`SessionCommitPump::new`]
/// (one per session), read by [`commit_from_panic`] from whatever thread panicked.
static PANIC_TARGET: std::sync::LazyLock<std::sync::Mutex<Option<PanicFlushTarget>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// Poison-tolerant, exactly like every other guard on this path: the payload is
/// one `Option`, every mutation is a plain assignment, and a panicked holder left
/// it consistent (precedent: `src/manager/phase.rs`, `commit::last_commit_lock`).
fn panic_target() -> Option<PanicFlushTarget> {
    PANIC_TARGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// What the hook in `main` may say when the flush itself unwound: which bundle was
/// registered, if any. Read-only and poison-tolerant like everything else here.
pub fn panic_target_bundle_hint() -> String {
    panic_target()
        .map(|target| target.bundle.display().to_string())
        .unwrap_or_else(|| "no session bundle".to_string())
}

/// What the panic hook did, so it can never report success it did not earn: if
/// the panic happened before any successful commit, T-8 writes nothing and logs
/// why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicFlushOutcome {
    /// `true` only when a manifest with `ending: panicHook` was renamed into
    /// place (manifest-last, so the bundle really carries it).
    pub written: bool,
    /// Members the flush re-listed (0 is normal: an unchanged member keeps its
    /// bytes).
    pub members_written: usize,
    /// Plain English for the user and the log: what was written, or why nothing
    /// could be.
    pub detail: String,
}

/// T-8: the panic-hook flush. **Pure synchronous I/O** — no runtime, no
/// `block_on`, no renderer, no engine, no allocation of a task. It rewrites the
/// manifest of the last committed bundle with `ending: panicHook`, carrying the
/// member bytes exactly as the last successful commit wrote them: an unchanged
/// member keeps its bytes, the manifest is the thing that changes.
///
/// Thread model: a panic runs the hook **on whichever thread panicked**, and
/// nothing here touches session-thread-only state. What it cannot honestly do is
/// capture a pane or a transcript that was never committed — that data does not
/// exist, and this says so out loud instead of racing the session thread for it.
pub fn commit_from_panic() -> PanicFlushOutcome {
    let outcome = panic_flush_inner();
    // `let _ =` on a commit is forbidden: the outcome is counted, logged and
    // returned to `main`, which repeats it on stderr.
    {
        let mut slot = ledger();
        slot.panic_hook += 1;
        if outcome.written {
            slot.written += 1;
            slot.last_ending = Some(ProcessEnding::PanicHook);
        } else {
            slot.failed += 1;
        }
    }
    if outcome.written {
        tracing::warn!("panic hook: {}", outcome.detail);
    } else {
        tracing::error!("panic hook: {}", outcome.detail);
    }
    outcome
}

fn panic_flush_inner() -> PanicFlushOutcome {
    let fail = |detail: String| PanicFlushOutcome {
        written: false,
        members_written: 0,
        detail,
    };
    let Some(target) = panic_target() else {
        return fail(
            "no session bundle is registered in this process (the panic happened before any \
             session started, or session capture is disabled), so nothing was written"
                .to_string(),
        );
    };
    let cached = crate::session::commit::LAST_COMMIT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(last) = cached.as_ref() else {
        return fail(format!(
            "no successful commit exists for {} yet, so a panic cannot write a bundle it has \
             no bytes for: the session died before its first commit",
            target.bundle.display()
        ));
    };
    if last.bundle != target.bundle {
        return fail(format!(
            "the last successful commit was for {} but this session's bundle is {}: refusing \
             to mark a bundle that is not this session's",
            last.bundle.display(),
            target.bundle.display()
        ));
    }
    let payload = match payload_from_last_commit(last) {
        Ok(payload) => payload,
        Err(why) => {
            return fail(format!(
                "the members of the last successful commit could not be re-read ({why}), so the \
                 panic hook wrote nothing"
            ));
        }
    };
    drop(cached);
    match crate::session::commit::write_bundle(
        &target.bundle,
        &target.session_id,
        &payload,
        ProcessEnding::PanicHook,
        &target.policy,
        &target.lock,
    ) {
        Ok(report) => PanicFlushOutcome {
            written: true,
            members_written: report.members_written.len(),
            detail: format!(
                "the session bundle {} was marked panicHook (session id {}, {} member(s) \
                 rewritten, the rest kept byte-identical from the last successful commit): a \
                 later --resume will see an interrupted session",
                target.bundle.display(),
                target.session_id,
                report.members_written.len()
            ),
        },
        // The session thread holds the commit lock (a panic *inside* a commit).
        // Blocking here would deadlock the hook against the thread that is
        // unwinding, so the flush degrades honestly: the last committed bundle
        // survives with `inProgress`, and the next boot infers the hard kill.
        Err(CommitError::Locked) => fail(format!(
            "the session commit lock was already held while the panic was unwinding, so {} was \
             left as it was last committed (ending inProgress): the next boot will classify this \
             run as HardKillInferred rather than guess",
            target.bundle.display()
        )),
        Err(err) => fail(format!(
            "the panic-hook flush of {} failed: {err}",
            target.bundle.display()
        )),
    }
}

/// Rebuild the payload the panic hook re-commits **from the commit cache alone**
/// (global state only). A member that will not deserialize is reported, not
/// swallowed: the flush then writes nothing rather than a manifest that lists
/// members it cannot vouch for.
fn payload_from_last_commit(
    last: &crate::session::commit::LastCommit,
) -> Result<CapturePayload, String> {
    let mut manager: Option<crate::session::ManagerSection> = None;
    let mut ui: Option<UiSection> = None;
    let mut workers: Vec<WorkerSection> = Vec::new();
    for (rel_path, (_, _, bytes)) in &last.members {
        // The cached bytes are the whole member **file** — header frame plus
        // section, exactly what `write_file_atomic` was handed — so the frame is
        // parsed and its section unwrapped, the same way
        // `session::restore::parse_member` reads the file back off disk.
        if rel_path == crate::session::MANAGER_FILE {
            let file: crate::session::ManagerFile =
                serde_json::from_slice(bytes).map_err(|err| format!("{rel_path}: {err}"))?;
            manager = Some(file.section);
        } else if rel_path == crate::session::UI_FILE {
            let file: crate::session::UiFile =
                serde_json::from_slice(bytes).map_err(|err| format!("{rel_path}: {err}"))?;
            ui = Some(file.section);
        } else {
            let file: crate::session::WorkerFile =
                serde_json::from_slice(bytes).map_err(|err| format!("{rel_path}: {err}"))?;
            workers.push(file.section);
        }
    }
    let manager = manager.ok_or_else(|| "manager.json is not in the last commit".to_string())?;
    Ok(CapturePayload {
        manager,
        ui,
        workers,
        identity_floors: last.manifest.identity_floors,
    })
}

/// T-10 step 1: record the ending of a session that is about to be **deleted** —
/// `/reset` (`Reset`) or a plan archive (`PlanArchived`). Mandatory and never
/// throttled: a crash between this commit and the deletion must leave a bundle
/// that says `reset`, not one still claiming `inProgress` (which a later boot
/// would report as a hard kill that never happened).
pub fn mark_ending_commit(
    pump: &mut SessionCommitPump,
    trigger: CommitTrigger,
    ctx: &ContextEngine,
    renderer: &dyn Renderer,
) -> CommitDecision {
    crate::session::manager_capture::capture_into_session_state(ctx);
    pump.request(trigger, ctx, renderer)
}

/// T-10 step 2: after `Plan::clear()` / `Plan::archive()` deleted the plan,
/// confirm the bundle really is gone — a cleared plan means "this session MUST
/// NOT be resurrected", so a leftover bundle is deleted here, on the session
/// thread, loudly. Returns `true` when the bundle is gone; when the delete is
/// blocked (permissions, a busy mount) the survivor is re-marked with the
/// ending and `false` is returned.
///
/// The delete, not the marker, is the safety-critical half: `session::restore`
/// does not refuse a `Reset`-marked bundle, so a bundle that survives a reset
/// could be resurrected — which is why reaching the marker branch is logged at
/// error level rather than treated as clean.
pub fn confirm_bundle_swept(
    pump: &mut SessionCommitPump,
    trigger: CommitTrigger,
    plan: &crate::manager::phase::Plan,
    ctx: &ContextEngine,
    renderer: &dyn Renderer,
) -> bool {
    let bundle = crate::session::bundle_dir(plan);
    if !bundle.exists() {
        return true;
    }
    tracing::warn!(
        "a {end:?} session sweep left {} in place: deleting it here, because a session the user \
         ended must never be resurrected (A D3)",
        bundle.display(),
        end = trigger.ending()
    );
    if std::fs::remove_dir_all(&bundle).is_ok() && !bundle.exists() {
        return true;
    }
    tracing::error!(
        "session bundle {} could not be deleted; its manifest is rewritten with ending {:?} so a \
         later boot at least reads the truth about it (A §7.4 marker rule)",
        bundle.display(),
        trigger.ending()
    );
    mark_ending_commit(pump, trigger, ctx, renderer);
    false
}

/// T-10 end to end: one `/reset`. Mark the bundle with `reset`, run the plan
/// clear (`Plan::clear()` → `sweep_session_state()`), then confirm the bundle
/// really was deleted. The order is the contract: a crash between the mark and
/// the delete leaves a bundle that already says `reset` — never one still
/// claiming `inProgress`, which a later boot would report as a hard kill that
/// never happened. Every `/reset` call site in the loop goes through here.
fn handle_reset_command_and_commit(
    pump: &mut SessionCommitPump,
    plan: &crate::manager::phase::Plan,
    ctx: &mut ContextEngine,
    renderer: &mut dyn Renderer,
    ui_transcript: &mut UiTranscript,
    steering_history: &Arc<std::sync::RwLock<Vec<(String, String)>>>,
) {
    mark_ending_commit(pump, CommitTrigger::Reset, ctx, &*renderer);
    handle_reset_command(plan, renderer, Some(&mut *ctx));
    ui_transcript.clear();
    if let Ok(mut hist) = steering_history.write() {
        hist.clear();
    }
    // The legacy `.ui_transcript.json` deletion stays: the boot path only NAMES a
    // leftover legacy journal (`src/session/restore.rs:804`, `no_bundle`) and
    // never deletes one, so this is the removal that guarantees `/reset` leaves
    // none. The path is sourced locally from the plan directory because the
    // session loop never writes that journal.
    let legacy_ui_journal = plan.dir().join(".ui_transcript.json");
    if legacy_ui_journal.is_file() {
        let _ = std::fs::remove_file(&legacy_ui_journal);
    }
    confirm_bundle_swept(pump, CommitTrigger::Reset, plan, ctx, &*renderer);
}

/// `true` when the plan file existed at the start of the round and is gone now
/// **with an archive written in its place**: that is the fingerprint of
/// `archive_plan` → `Plan::archive()` having run during this round (a tool, so it
/// may have run on another thread; only this thread may commit).
///
/// `/reset` deliberately does not match: `Plan::clear()` in `phase.rs` deletes
/// the plan *and* `execution_plan_archive.md`, and it is handled where the user
/// asked for it, in [`handle_reset_command_and_commit`]. The archive file name is
/// spelled here as it is spelled in `phase.rs` — the plan module owns it, and
/// this is the only read-side consumer.
fn plan_archived_during_round(
    plan: &crate::manager::phase::Plan,
    plan_present_at_round_start: bool,
) -> bool {
    if !plan_present_at_round_start || plan.plan_path().exists() {
        return false;
    }
    plan.plan_path()
        .parent()
        .is_some_and(|dir| dir.join("execution_plan_archive.md").is_file())
}

/// Count one non-write decision in the ledger and hand it back.
fn decide(decision: CommitDecision) -> CommitDecision {
    {
        let mut slot = ledger();
        match decision {
            CommitDecision::Written { .. } => slot.written += 1,
            CommitDecision::Throttled => slot.throttled += 1,
            CommitDecision::Suppressed => slot.suppressed += 1,
            CommitDecision::Disabled => slot.disabled += 1,
            CommitDecision::Busy => slot.busy += 1,
            CommitDecision::Failed => slot.failed += 1,
        }
    }
    decision
}

/// The transcript half of the three loud destinations a restore must reach. Only
/// the session loop owns a `UiTranscript`, so [`boot`] is handed that journal
/// instead of owning one: every refusal, repair and loss sentence reaches the
/// panes, the journaled `UiRecord::Status` and `tracing::warn!`, and none of the
/// three may be a discarded `let _`.
pub struct BootSink<'a> {
    pub ui_transcript: &'a mut UiTranscript,
}

/// What the one boot pass did. Nothing here is dropped: the caller prints it,
/// the panes draw it, the journal records it and the log carries it.
///
/// The field set is kept verbatim; the added fields are what lets the session
/// **continue** from the restored state instead of starting over: the goal, the
/// subagent panes, the reconstructed journal.
#[derive(Debug, Default)]
pub struct BootReport {
    /// `true` when live state was installed (engine vector, worker registry,
    /// panes). `false` means the pass installed **nothing**: no bundle existed.
    pub restored: bool,
    /// `true` for `RestoreOutcome::PartiallyRestored` (the one tolerated
    /// `Unrecoverable` worker): the run continues, with the losses surfaced.
    pub partial: bool,
    pub manager_messages: usize,
    pub workers_installed: usize,
    pub interrupted_worker_keys: Vec<String>,
    pub side_effect_risk_keys: Vec<String>,
    /// N-MGR-1, the single synthetic manager notice.
    pub restore_notice: Option<String>,
    pub ui_restored: bool,
    pub session_id: String,
    /// The restored pinned goal — `manager.messages[1]` verbatim.
    /// Never re-typed, and `ContextEngine::set_goal` is never called for it.
    pub goal: String,
    pub subagent_panels: Vec<SubagentPanel>,
    pub journal: Vec<UiJournalRecord>,
    pub repair: RepairReport,
    pub unrecoverable_workers: Vec<String>,
    /// The ending the loader derived for the previous process (an
    /// `InProgress` bundle is *reported* as `HardKillInferred`, never rewritten).
    pub previous_ending: Option<ProcessEnding>,
    /// Sentences naming the searched path and what was found instead, for the
    /// loud fresh start required when there is no bundle.
    pub no_bundle_lines: Vec<String>,
}

/// THE boot function: one pass, on the session thread, in the fixed
/// order `load_bundle` (L0–L9 verify, N0–N6 normalize) → identity floors +
/// `WORKERS` → the engine → the panes → the loud report → `N-MGR-1`.
///
/// Every rule lives in the piece that already owns it; nothing is reimplemented
/// here:
/// * [`crate::session::restore::load_bundle`] — verify before parse, repair
///   twice, refuse fail-closed, never touch live state or a byte of the bundle;
/// * [`crate::session::install_restored_workers`] — the `fetch_max` floors, the
///   `WORKERS` rebuild under the stored keys, restore-as-interrupted (P-R1: **no
///   worker is ever relaunched**), the completed-ring cap;
/// * [`ContextEngine::install_restored_messages`] / [`ContextEngine::restore_engine_flags`]
///   — the wholesale install. **No pin writer runs on this path**: neither
///   `set_system_prompt` nor `set_goal` may touch `[0]`/`[1]` of a restored
///   transcript, which is why the resume branch builds its engine through
///   [`build_restored_context`] instead of [`build_manager_context`];
/// * [`Renderer::restore_panes`] / [`Renderer::announce_restore`] — the pane bytes
///   are assigned verbatim, never regenerated.
pub fn boot(
    plan: &crate::manager::phase::Plan,
    cfg: &Config,
    ctx: &mut ContextEngine,
    renderer: &mut dyn Renderer,
    session_id: &mut String,
    sink: &mut BootSink<'_>,
) -> Result<BootReport, RestoreRefused> {
    let bundle = crate::session::bundle_dir(plan);
    let outcome = crate::session::restore::load_bundle(&bundle);
    let (session, derived_ending, mut repair, unrecoverable_workers) = match outcome {
        RestoreOutcome::NoBundle {
            bundle,
            legacy_journals,
            orphan_members,
        } => {
            let report = no_bundle_report(&bundle, &legacy_journals, &orphan_members);
            surface_restore_lines(renderer, sink, &report.no_bundle_lines);
            return Ok(report);
        }
        RestoreOutcome::Refused { refusal, report } => {
            // Fail-closed: the bundle was never read into live state, so
            // there is nothing to un-install. The refusal is still printed through
            // all three destinations before the typed error carries it out to
            // `main`, which maps it to `session::EXIT_RESTORE_REFUSED` (3).
            let mut lines = vec![
                refusal.display(&bundle),
                format!(
                    "Session restore refused ({}): nothing was installed, and no fresh session is \
                     started under a flag that asked for the old one — this run stops with exit \
                     code {}.",
                    refusal.code.label(),
                    crate::session::EXIT_RESTORE_REFUSED
                ),
            ];
            lines.extend(report.notices.iter().map(|notice| {
                format!(
                    "Not restorable after restart [{:?}] {}: {}",
                    notice.kind, notice.subject, notice.detail
                )
            }));
            lines.extend(
                report
                    .warnings
                    .iter()
                    .map(|warning| format!("Restore warning: {warning}")),
            );
            surface_restore_lines(renderer, sink, &lines);
            return Err(RestoreRefused { refusal, bundle });
        }
        RestoreOutcome::Restored {
            session,
            ending,
            report,
        } => (session, ending, report, Vec::new()),
        RestoreOutcome::PartiallyRestored {
            session,
            ending,
            unrecoverable_workers,
            report,
        } => (session, ending, report, unrecoverable_workers),
    };

    // ---------------------------------------------------------------- 1. floors + WORKERS
    // Before the engine, so no id, `completed_seq` or notice id can be re-issued
    // under a floor the bundle already spent.
    let worker_report =
        crate::session::install_restored_workers(&session.workers, &session.identity_floors);
    repair.workers_restored = worker_report.installed;
    repair.completed_dropped_by_ring_cap = worker_report.completed_dropped_by_ring_cap;
    for notice in &worker_report.notices {
        push_repair_notice(&mut repair, notice.clone());
    }
    for warning in &worker_report.warnings {
        push_repair_warning(&mut repair, warning.clone());
    }

    // The capture-side losses are part of the worker sections: the loader cannot
    // read the dead process's notice store, so the boot wiring supplies the
    // queued-notice count and merges every other recorded loss.
    for section in &session.workers {
        let mut queued = 0usize;
        for notice in &section.non_restorable {
            if notice.kind == NonRestorableKind::PendingNotice {
                queued += 1;
            } else {
                push_repair_notice(&mut repair, notice.clone());
            }
        }
        if queued > 0 {
            for notice in
                crate::session::worker_journal::queued_notice_notices(&section.worker_key, queued)
            {
                push_repair_notice(&mut repair, notice);
            }
        }
    }
    // Two losses the previous process always takes with it: the MCP view is
    // re-advertised, and the intervention counters restart.
    push_repair_notice(
        &mut repair,
        NonRestorableNotice {
            kind: NonRestorableKind::ToolSchemaCharge,
            subject: "session".to_string(),
            detail: format!(
                "the previous process's tool-schema token charge did not survive: this process \
                 re-declared the advertised Manager tool list and re-priced it to {} token(s)",
                ctx.tool_schema_tokens()
            ),
        },
    );
    push_repair_notice(
        &mut repair,
        NonRestorableNotice {
            kind: NonRestorableKind::HarnessStats,
            subject: "session".to_string(),
            detail: "the previous process's harness intervention counters did not survive: this \
                     process starts from zero, so a repeated intervention may need to be seen \
                     again before it is escalated"
                .to_string(),
        },
    );

    // ---------------------------------------------------------------- 2. the engine
    let manager_messages = session.manager.messages.len();
    ctx.install_restored_messages(session.manager.messages.clone());
    ctx.restore_engine_flags(
        session.manager.rebirth_advisory_emitted,
        session.manager.consecutive_rebirths,
    );
    // The prompt this Config would have composed is deliberately not installed;
    // the difference is a reported deviation, never a silent re-pin.
    push_repair_warning(
        &mut repair,
        format!(
            "the system prompt this process composed was NOT installed: [0]/[1] keep the restored \
             bytes (pin witness {} / {} byte(s), pinned window [0..{}) as stored)",
            session.manager.pin_witness.system_prompt_len,
            session.manager.pin_witness.goal_len,
            session.manager.pinned_prefix_end
        ),
    );
    if session.manager.max_context_tokens != cfg.max_context_tokens {
        push_repair_warning(
            &mut repair,
            format!(
                "max_context_tokens changed since the bundle was written: {} stored, {} in this \
                 Config — this run budgets against the live value",
                session.manager.max_context_tokens, cfg.max_context_tokens
            ),
        );
    }

    // ---------------------------------------------------------------- 3. the panes
    // Captured BEFORE the assign: the token witnesses compare the stored counters
    // against the ones THIS process owns, and the streams own those numbers — a
    // restore never adopts them.
    let live_ui = renderer.capture_ui();
    renderer.restore_panes(&session.ui);
    // A renderer with no pane model gets a no-op `restore_panes`, so the subagent
    // list is handed through the loop's own channel as well. The journaled panels
    // keep their order and length (the restore contract pins both); the rows the
    // **restored registry** says are open are then joined on by their effective
    // key, so a `{agent}-wN` / `{base}#N` worker restored under its real key gets
    // a row of the same name instead of an unaddressable panel name.
    let panels: Vec<SubagentDetail> =
        seed_restored_subagent_rows(&session.ui.subagent_panels, &worker_report.interrupted_keys);
    renderer.set_subagents(panels);
    // A panel that was showing an in-flight subagent keeps its byte:
    // `is_active: true` is never flipped — the pane must keep saying what the
    // screen said. What died with the process is the task behind it, so the pane
    // gets a `JoinHandle` notice of its own keyed on the **panel name**: the
    // worker-key notices from `install_restored_workers` name the registry entry,
    // never the pane. Folded in BEFORE `announce_restore` so the loss reaches the
    // panes, the journal and the log, and lands in `BootReport::repair.notices`.
    for panel in &session.ui.subagent_panels {
        if !panel.is_active {
            continue;
        }
        push_repair_notice(
            &mut repair,
            NonRestorableNotice {
                kind: NonRestorableKind::JoinHandle,
                subject: panel.name.clone(),
                detail: format!(
                    "the subagent panel '{}' is drawn as active, but the task driving this \
                     subagent ended with the previous process: its join handle did not survive \
                     and no task exists for it, so it can be neither joined nor aborted and no \
                     result will ever arrive; the panel is restored read-only, for its history \
                     only, until this delegation is re-armed",
                    panel.name
                ),
            },
        );
    }
    // A restored scroll offset that the live clamp moved is only legal when a
    // `warnings` entry names the offset AND both values. This capture is taken
    // AFTER `restore_panes` (last step `clamp_scrolls`,
    // `ui/tui/events.rs:465-481`) and after `set_subagents` (re-clamps
    // `selected_subagent_idx`, `ui/tui/mod.rs:1066-1068`), so the sentence names
    // the number the panes ended on. Folded BEFORE `announce_restore`, which is
    // what puts it in the panes and the log — stated exactly once.
    let restored_ui = renderer.capture_ui();
    for warning in scroll_clamp_warnings(&session.ui, &restored_ui) {
        tracing::warn!("session restore (ui.json): {warning}");
        push_repair_warning(&mut repair, warning);
    }
    renderer.announce_restore(&repair);

    // **Documented deviation:** `TuiRenderer::token_witness_warnings`
    // (`ui/tui/mod.rs:1581`) is an *inherent* method, so it is not reachable
    // through `&mut dyn Renderer`. The same comparison lives here, and it is
    // folded in **after** `announce_restore` on purpose: `TuiRenderer::restore_panes`
    // already drew the identical sentence into the panes and logged it, so folding
    // it in before would draw one disagreement as two pane lines.
    for warning in token_witness_warnings(&session.ui, &live_ui) {
        push_repair_warning(&mut repair, warning);
    }
    let ui_restored = !session.ui.chat_messages.is_empty() || !session.ui.journal.is_empty();

    // The report, journaled (the third destination; the panes were served by
    // `announce_restore`), and the visible history re-seeded from the restored
    // journal so the session continues rather than starting blank.
    seed_restored_transcript(sink, &session.ui.journal);
    let summary = format!(
        "Session restored (session id {}, previous ending {:?}): {} manager message(s), {} \
         worker(s), {} pane line(s){}.",
        session.session_id,
        derived_ending,
        manager_messages,
        worker_report.installed,
        session.ui.chat_messages.len(),
        if repair.notices.is_empty() {
            String::new()
        } else {
            format!(" — {} capability(s) not restorable", repair.notices.len())
        }
    );
    let mut journal_lines = vec![summary.clone()];
    journal_lines.extend(repair.notices.iter().map(|notice| {
        format!(
            "Not restorable after restart [{:?}] {}: {}",
            notice.kind, notice.subject, notice.detail
        )
    }));
    journal_lines.extend(
        repair
            .warnings
            .iter()
            .map(|warning| format!("Restore warning: {warning}")),
    );
    journal_restore_lines(sink, &journal_lines);
    // One status line only, and it carries no `[CLI]` prefix on purpose: a TUI
    // `Status` event with that prefix would draw a second pane line for a report
    // `announce_restore` already drew.
    renderer.on_event(&Event::Status(summary));

    // ---------------------------------------------------------------- 4. N-MGR-1 last
    // After the normalized vector and before any new user input: the manager is
    // told once, in one message, what is interrupted and what is lost.
    *session_id = session.session_id.clone();
    let notice = crate::session::restore_notice(
        session_id,
        &session.manifest.ending,
        &derived_ending,
        &session.workers,
        &worker_report,
    );
    ctx.append_restore_notice(&notice);

    Ok(BootReport {
        restored: true,
        partial: !unrecoverable_workers.is_empty(),
        manager_messages,
        workers_installed: worker_report.installed,
        interrupted_worker_keys: worker_report.interrupted_keys.clone(),
        side_effect_risk_keys: worker_report.side_effect_risk_keys.clone(),
        restore_notice: Some(notice),
        ui_restored,
        session_id: session.session_id.clone(),
        goal: session
            .manager
            .messages
            .get(1)
            .and_then(|record| record.content())
            .unwrap_or_default()
            .to_string(),
        subagent_panels: session.ui.subagent_panels.clone(),
        journal: session.ui.journal.clone(),
        repair,
        unrecoverable_workers,
        previous_ending: Some(derived_ending),
        no_bundle_lines: Vec::new(),
    })
}

/// The `--resume` decision point. `Ok(None)` means the session continues as
/// a fresh run — either nothing asked for a restore, or `--resume` was asked and
/// there is no bundle, which is answered with a **loud** fresh start (exit
/// `0`). `Err(RestoreRefused)` is the hard stop (exit `EXIT_RESTORE_REFUSED`).
///
/// **Caller contract:** `Ok(None)` while a resume was asked means the resume-branch
/// engine ([`build_restored_context`], which deliberately carries no `[0]`) kept
/// its empty transcript, so the caller MUST install the composed system prompt
/// before pinning the goal — otherwise [`ContextEngine::set_goal`] pads
/// `messages[0]` with an empty `System` and the Manager runs with no prompt at all.
/// `src/ui/session.rs` does that immediately after this call.
pub fn boot_if_requested(
    resume_requested: bool,
    plan: &crate::manager::phase::Plan,
    cfg: &Config,
    ctx: &mut ContextEngine,
    renderer: &mut dyn Renderer,
    session_id: &mut String,
    sink: &mut BootSink<'_>,
) -> Result<Option<BootReport>, RestoreRefused> {
    if !resume_requested {
        return Ok(None);
    }
    let report = boot(plan, cfg, ctx, renderer, session_id, sink)?;
    if report.restored {
        Ok(Some(report))
    } else {
        Ok(None)
    }
}

/// The engine for a **restored** session: constructed exactly like
/// [`build_manager_context`] — real budgets, and the live advertised tool list
/// charged to the budget — but with **no** `set_system_prompt`. The restored
/// vector arrives wholesale through
/// [`ContextEngine::install_restored_messages`].
pub fn build_restored_context(cfg: &Config) -> ContextEngine {
    let mut ctx = ContextEngine::new(cfg.max_context_tokens);
    sync_manager_tool_schema(&mut ctx, cfg);
    ctx
}

/// L0 with a voice: "no bundle" is a normal outcome, never silent.
fn no_bundle_report(
    bundle: &std::path::Path,
    legacy_journals: &[std::path::PathBuf],
    orphan_members: &[String],
) -> BootReport {
    let mut lines = vec![format!(
        "No session bundle at {}: starting fresh.",
        bundle.display()
    )];
    if !orphan_members.is_empty() {
        lines.push(format!(
            "{} member-looking file(s) sit under {} but no {} lists them, so none of them was \
             read: {}",
            orphan_members.len(),
            bundle.display(),
            crate::session::MANIFEST_FILE,
            orphan_members.join(", ")
        ));
    }
    for path in legacy_journals {
        lines.push(format!(
            "Legacy journal {} was found and is NOT restorable: it was not read, not merged and \
             not upgraded; the session bundle is the only durable session format.",
            path.display()
        ));
    }
    for line in &lines {
        tracing::warn!("{line}");
    }
    BootReport {
        no_bundle_lines: lines,
        ..BootReport::default()
    }
}

/// Union of `NonRestorableNotice`s, deduplicated by `(kind, subject)`.
/// `RepairReport` is a durable type owned by `session::format` and has no
/// push helper, so the union rule lives with the wiring that performs the union.
fn push_repair_notice(report: &mut RepairReport, notice: NonRestorableNotice) {
    let key = notice.dedup_key();
    if !report.notices.iter().any(|n| n.dedup_key() == key) {
        report.notices.push(notice);
    }
}

/// Same dedup rule for the human-readable deviation list.
fn push_repair_warning(report: &mut RepairReport, warning: String) {
    if !report.warnings.contains(&warning) {
        report.warnings.push(warning);
    }
}

/// Send sentences to the renderer and the journal (the two destinations the
/// loader cannot reach). Status text carries no `[CLI]` prefix so a TUI never
/// draws one refusal as two pane lines.
fn surface_restore_lines(renderer: &mut dyn Renderer, sink: &mut BootSink<'_>, lines: &[String]) {
    for line in lines {
        renderer.on_event(&Event::Status(line.clone()));
    }
    journal_restore_lines(sink, lines);
}

/// The journaled half: one `UiRecord::Status` per sentence. The journal stays in
/// memory — the session bundle's `ui` member is the only durable UI state.
fn journal_restore_lines(sink: &mut BootSink<'_>, lines: &[String]) {
    for line in lines {
        sink.ui_transcript
            .append(UiRecord::Status { text: line.clone() });
    }
}

/// Seed the live in-memory `UiTranscript` from the restored `UiSection` journal
/// (1:1 mirror), so a resumed session keeps its visible chat history; the
/// bundle's `ui` member is what persists that journal from here on.
fn seed_restored_transcript(sink: &mut BootSink<'_>, journal: &[UiJournalRecord]) {
    for record in journal_records(journal) {
        sink.ui_transcript.append(record);
    }
}

/// `UiJournalRecord` → `UiRecord`: the two enums are the same closed set of eight
/// variants by construction, so this is a rename, not a re-derivation.
fn journal_records(journal: &[UiJournalRecord]) -> Vec<UiRecord> {
    journal
        .iter()
        .map(|record| match record {
            UiJournalRecord::User { text } => UiRecord::User { text: text.clone() },
            UiJournalRecord::Assistant { content, thinking } => UiRecord::Assistant {
                content: content.clone(),
                thinking: thinking.clone(),
            },
            UiJournalRecord::SteerResponse { text } => {
                UiRecord::SteerResponse { text: text.clone() }
            }
            UiJournalRecord::ToolCall { display } => UiRecord::ToolCall {
                display: display.clone(),
            },
            UiJournalRecord::ToolResult { display } => UiRecord::ToolResult {
                display: display.clone(),
            },
            UiJournalRecord::TaskCompleted { task_id } => UiRecord::TaskCompleted {
                task_id: task_id.clone(),
            },
            UiJournalRecord::TaskFailed { task_id, reason } => UiRecord::TaskFailed {
                task_id: task_id.clone(),
                reason: reason.clone(),
            },
            UiJournalRecord::Status { text } => UiRecord::Status { text: text.clone() },
        })
        .collect()
}

/// The token-witness comparison behind `TuiRenderer::token_witness_warnings`
/// (`ui/tui/mod.rs:1581`), phrased identically so a report reads the same
/// whichever half produced it. See the call site for why it runs after
/// `announce_restore`.
fn token_witness_warnings(stored: &UiSection, live: &UiSection) -> Vec<String> {
    [
        ("tokens_in", stored.tokens_in, live.tokens_in),
        ("tokens_out", stored.tokens_out, live.tokens_out),
        (
            "orchestrator_context_tokens",
            stored.orchestrator_context_tokens,
            live.orchestrator_context_tokens,
        ),
    ]
    .into_iter()
    .filter(|(_, stored, live)| stored != live)
    .map(|(field, stored, live)| {
        format!(
            "{field} witness {stored} != this process's live {live}: kept the live counter (the \
             streams own it) and reported the difference instead"
        )
    })
    .collect()
}

/// The scroll-clamp comparison folded into `RepairReport::warnings` at boot,
/// reached through the single shared sentence builder
/// `crate::ui::tui::clamp_warning_sentences` so the report, the
/// `tracing::warn!` and the pane line from `announce_restore` match word for
/// word. Unlike the token witnesses, `live` is captured AFTER the restore: the
/// restore's own clamping is exactly what moved these numbers.
fn scroll_clamp_warnings(stored: &UiSection, live: &UiSection) -> Vec<String> {
    crate::ui::tui::clamp_warning_sentences(&[
        (
            "chat_scroll",
            u64::from(stored.view.chat_scroll),
            u64::from(live.view.chat_scroll),
        ),
        (
            "plan_scroll",
            u64::from(stored.view.plan_scroll),
            u64::from(live.view.plan_scroll),
        ),
        (
            "subagent_scroll",
            u64::from(stored.view.subagent_scroll),
            u64::from(live.view.subagent_scroll),
        ),
        (
            "subagent_list_scroll",
            u64::from(stored.view.subagent_list_scroll),
            u64::from(live.view.subagent_list_scroll),
        ),
        (
            "selected_subagent_idx",
            stored.view.selected_subagent_idx as u64,
            live.view.selected_subagent_idx as u64,
        ),
    ])
}

/// The inverse projection of [`SubagentPanel`] into the loop's own
/// [`SubagentDetail`] model. Mirrors `ui::tui::subagent_detail_from_panel`
/// (`ui/tui/mod.rs:1489`), which is private to that file: the session loop
/// needs the same projection because its `subagents` vector is the authority
/// the next frame is built from.
fn subagent_detail_from_panel(panel: &SubagentPanel) -> SubagentDetail {
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
        queue: None,
    }
}

/// Restore-side row set: the journaled panels keep their exact order and length
/// (the restore contract pins both), and every registry key the restored worker
/// journal reports as **interrupted while open** then joins the list on its
/// effective key if no panel already carries that name.
///
/// This is what fixes the restore misnaming: a `{agent}-wN` / `{base}#N` worker
/// is journaled under its effective registry key, and a pane rebuilt from the
/// stream tag would be unaddressable by the token/thinking lookups that key on
/// the registry entry.
fn seed_restored_subagent_rows(
    panels: &[SubagentPanel],
    interrupted_keys: &[String],
) -> Vec<SubagentDetail> {
    let mut rows: Vec<SubagentDetail> = panels.iter().map(subagent_detail_from_panel).collect();
    for key in interrupted_keys {
        if rows.iter().any(|row| &row.name == key) {
            continue;
        }
        match crate::ui::helpers::subagent_row_for_registry_key(key) {
            Some(row) => rows.push(row),
            None => tracing::debug!(
                key = %key,
                "restored worker key has no registry entry to project a subagent row from"
            ),
        }
    }
    rows
}

/// The single refresh edge for the subagent pane: the pane is rebuilt from the
/// worker registry (`helpers::project_subagent_rows`) and the authoritative row
/// set is pushed to the renderer whenever the projected set changed.
/// No new channel and no new timer: this runs on edges the loop already has.
fn project_subagent_pane(subagents: &mut Vec<SubagentDetail>, renderer: &mut dyn Renderer) {
    if crate::ui::helpers::project_subagent_rows(subagents) {
        renderer.set_subagents(subagents.clone());
    }
}

/// `Duration::from_secs_f64` panics on a negative/NaN/infinite input, so a corrupt
/// number degrades to zero instead of taking a restored session down.
fn duration_from_secs(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

/// Drive a full interactive session, dispatching events to `renderer`.
/// This is the production entry point: it applies [`SessionBounds::default()`]
/// (turn cap, per-turn wall-clock watchdog + hard cap, repeated-failure budget).
pub async fn run_session(
    cfg: &Config,
    renderer: &mut dyn Renderer,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
) -> Result<()> {
    run_session_with_bounds(cfg, renderer, initial, manager, SessionBounds::default()).await
}

/// The live session loop with explicit execution bounds.
///
/// Bounds are injected rather than read from [`Config`] so the wall-clock and
/// failure-budget paths can be tested in milliseconds. `run_session` — the only
/// production caller — passes the defaults, so the shipped binary is bounded by
/// the constants in `src/manager/loop.rs` and nothing else.
///
/// Signature-preserving wrapper over [`run_session_with_bounds_report`], which
/// is where the machine-readable [`SessionStopReport`] is produced.
pub async fn run_session_with_bounds(
    cfg: &Config,
    renderer: &mut dyn Renderer,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
    bounds: SessionBounds,
) -> Result<()> {
    run_session_with_bounds_report(cfg, renderer, initial, manager, bounds)
        .await
        .map(|_| ())
}

/// [`run_session_with_bounds`] plus a machine-readable account of **why** the
/// session stopped.
///
/// Every bound exit carries a [`BoundStopReason`], shown through the session's
/// status channel and returned here, so a caller can say
/// `report.turn_cap_reached()` instead of guessing from a silent stop.
pub async fn run_session_with_bounds_report(
    cfg: &Config,
    renderer: &mut dyn Renderer,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
    bounds: SessionBounds,
) -> Result<SessionStopReport> {
    renderer.clear_abort();
    renderer.init()?;
    renderer.set_thinking_budgets(cfg);
    crate::debug_log::log_session_start(cfg, &cfg.ui_mode);

    let plan = manager.as_ref().map(|m| m.plan.clone()).unwrap_or_default();

    // The plan check is fail-closed and also reports a plan-integrity warning.
    // The prompt (and `tracing::warn!`) alone is not enough — the user must see
    // it too, so the warning is captured here and surfaced through the session's
    // existing warning channel once the transcript exists below.
    let (system, plan_warning) = load_system_prompt_with_plan_and_warning(cfg, &plan)?;
    // The live Manager engine is built with the exact advertised tool list
    // charged to the budget, so `should_compact()` measures the request that is
    // really sent (transcript **+** tool schemas).
    //
    // The resume trigger is resolved here because it decides **how** the engine
    // is built. On the resume branch the engine is still constructed — same
    // budgets, same live tool-schema charge — but no prompt is installed:
    // `set_system_prompt` inside [`build_manager_context`] is one of the two
    // writers that would overwrite a restored pin, and the boot pass below
    // installs the restored vector wholesale instead.
    // The prompt is still *composed* here because the plan-integrity warning
    // half of [`load_system_prompt_with_plan_and_warning`] is unchanged for a
    // resume, and the difference between the composed bytes and the restored
    // `[0]` is reported as a `RepairReport::warnings` entry by the boot pass,
    // never applied.
    //
    // **The resume branch must never be the only answer to a resume.** When the
    // boot pass finds no bundle — the loud fresh start — it installs
    // nothing, and an engine with no `[0]` would let the `ctx.set_goal` below pad
    // `messages[0]` with an **empty** `System` (`ContextEngine::set_goal`) —
    // the Manager would then run a whole session with no system prompt. The
    // composed prompt is therefore installed right after the boot pass, in exactly
    // the order a fresh boot installs it, so `--resume` / `MARMEL_RESUME=1` /
    // `session_resume = true` with no bundle is byte-for-byte the fresh request
    // (`session_tests.rs::resume_without_a_bundle_sends_the_same_request_as_a_fresh_start`).
    let resume = resume_requested(cfg);
    let mut ctx = if resume {
        build_restored_context(cfg)
    } else {
        build_manager_context(cfg, system.clone())
    };

    let stats = manager
        .as_ref()
        .map(|m| Arc::clone(&m.stats))
        .unwrap_or_else(|| Arc::new(crate::harness::HarnessStats::new()));
    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let mut monitor =
        crate::harness::monitor::HarnessMonitor::new_with_config(stats.clone(), mon_cfg);

    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let (steer_arb_tx, mut steer_arb_rx) = tokio::sync::mpsc::unbounded_channel::<SteerArbEvent>();
    let steering_history = Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
    crate::orchestrator::set_status_sender(status_tx);
    crate::orchestrator::set_event_sender(event_tx);
    crate::orchestrator::set_steering_history(Arc::clone(&steering_history));

    let mut ui_transcript = UiTranscript::new();

    // An UNKNOWN plan state (unreadable / unparseable plan file) must reach the
    // user, not only the model. Same channel as every other plan-integrity
    // warning in this loop: a renderer `Status` event plus a `Status` record in
    // the UI transcript, via the session's single surfacing helper.
    if let Some(warning) = plan_warning {
        surface_plan_warning(&mut *renderer, &mut ui_transcript, warning);
    }

    // THE single restore pass, and the only place in the crate where a session
    // bundle is read into live state. The position is load-bearing: the `Plan`
    // exists (the bundle hangs off it), the transcript paths exist, and
    // the boot pass runs **before** the manager loop, before `subagents` is seeded,
    // and before any pin writer — the resume branch above already skipped
    // `set_system_prompt`, and `set_goal` further down is skipped too, so neither
    // `build_manager_context` nor `ctx.set_goal` can overwrite the restored pins.
    //
    // One pass, fixed order. **Order note for the reviewer:** the short order
    // `load_bundle` → manager context → workers → panes → announce is a subset of
    // the order fixed here: `verify (L0-L7) → normalize (L8) → floors → install
    // WORKERS → install the engine → restore the panes → append N-MGR-1`, and the
    // loader/installer implements exactly that (floors first, so no id/seq/notice
    // can be re-issued below a floor *while* the registry is being rebuilt). This
    // pass follows that order, which is the superset constraint: the engine
    // install is still the step that owns the transcript, and no worker can touch
    // the engine before it exists, so the two orders differ only in that the
    // floors are raised before the engine vector is swapped — never in what is
    // installed or in what the Manager is finally shown.
    let mut session_id = String::new();
    let boot = {
        let mut sink = BootSink {
            ui_transcript: &mut ui_transcript,
        };
        boot_if_requested(
            resume,
            &plan,
            cfg,
            &mut ctx,
            &mut *renderer,
            &mut session_id,
            &mut sink,
        )
    };
    let boot = match boot {
        Ok(report) => report,
        Err(refused) => {
            // A refusal installed **nothing**: `load_bundle` is fail-closed and
            // never touched live state, so there is nothing to roll back, and a
            // refusal is never papered over by a fresh session under a flag that
            // asked for the old one: `boot` already printed the refusal, notices
            // and warnings (renderer, journaled `UiRecord::Status`,
            // `tracing::warn!`), and `main` maps this error onto
            // `session::EXIT_RESTORE_REFUSED` (3) after its teardown.
            return Err(anyhow::Error::new(refused));
        }
    };

    // A loud fresh start re-joins the **fresh** engine contract: the resume branch
    // built the engine *without* a system prompt on purpose (a real restore must
    // not route through a pin setter). An empty engine would let the
    // `ctx.set_goal` below pad `messages[0]` with an empty `Message::System`
    // (`ContextEngine::set_goal`), so the composed prompt is installed here in
    // the fresh-boot order. With `boot.is_some()` bytes were restored: no-op.
    if resume && boot.is_none() {
        ctx.set_system_prompt(system.clone());
    }

    // The ONE committer of this session. Built after the boot pass — so a resumed
    // session keeps the restored `session_id` and its L4/L5 cross-checks keep
    // working — and before the turn loop, so every trigger below is served by the
    // single writer the thread model allows. `session_enabled = false` leaves it
    // inert: requests are counted, nothing is ever written.
    let mut commit_pump = SessionCommitPump::new(&plan, cfg, &session_id);

    let mut steer_queue = Vec::<String>::new();
    let mut steer_abort_requested = false;
    // A resumed session adopts the restored subagent panes as the loop's
    // authoritative list instead of starting from an empty one, so the subagent
    // panes and the status line keep showing the interrupted delegations. Those
    // delegations are open in the worker registry and were **not** relaunched:
    // the manager is told by N-MGR-1 and decides.
    let mut subagents: Vec<SubagentDetail> = boot
        .as_ref()
        .map(|boot| {
            seed_restored_subagent_rows(&boot.subagent_panels, &boot.interrupted_worker_keys)
        })
        .unwrap_or_default();

    // Execution bounds, alive for the whole session (not per request):
    //  * `watchdog` — the per-turn wall-clock bound, re-armed at every turn,
    //  * `failures` — repeated-failure accounting keyed by plan task id (or by
    //    tool signature for direct tool calls).
    let mut watchdog = TurnWatchdog::new(bounds.turn_idle, bounds.turn_hard_cap);
    let mut failures = FailureBudget::new(bounds.failure_threshold);

    let client = ChatClient::from_config(cfg);
    // Call class for every backend call this session's Manager makes, plus the
    // tool rounds it dispatches on its behalf (design §1.5). The Manager turn has
    // no plan task of its own, so it carries an agent label and no task id — that
    // is what makes a queued Manager call distinguishable from a queued
    // delegation in pool logs and status traffic.
    let manager_call_context =
        pool::CallContext::for_agent(pool::CallClass::ManagerTurn, pool::MANAGER_AGENT, None);
    let harness_stats = stats.clone();
    // The Manager's wire knobs come from `StreamConfig::from_config`, which reads
    // the thinking **cap** from `[monitoring].max_thinking_tokens` and the effort
    // **dial** from the top-level `Config.reasoning_effort` (top level applies to
    // the Manager). Blank values normalize to unset at the wire chokepoint.
    let stream_cfg = StreamConfig::from_config(cfg);

    drain_steer_arbitration_events_with_transcript(
        &mut steer_arb_rx,
        &mut *renderer,
        &mut steer_queue,
        &mut steer_abort_requested,
        Some(&mut subagents),
        Some(&mut ui_transcript),
    );

    // A resumed session already owns its goal — it is `messages[1]` of the
    // restored vector, verbatim — so the interactive prompt below is skipped on
    // that branch: its early exits (`return Ok(SessionStopReport::default())`)
    // would end a resumed session before its first turn, and a re-typed goal
    // would re-write the pinned slot. The positional `PROMPT` of a `--resume`
    // run is set aside first: on a resume it becomes an ordinary user message
    // *after* N-MGR-1 and must never become `messages[1]`.
    let resume_prompt = boot.as_ref().and_then(|_| initial.clone());
    let goal = match boot.as_ref().map(|boot| boot.goal.clone()) {
        Some(goal) => {
            crate::debug_log::log_user_input("restored_goal", &goal);
            goal
        }
        None => match initial {
            Some(g) => {
                crate::debug_log::log_user_input("initial_argument", &g);
                g
            }
            None => loop {
                match renderer.read_input() {
                    Some(line) => {
                        if is_abort_command(&line) {
                            crate::debug_log::log_user_input("command", &line);
                            // An abort here is still an exit path and gets a mandatory ending
                            // commit — but only if THIS run already wrote the bundle. Marking it
                            // unconditionally would stamp a previous run's bundle (the one a
                            // `--resume` boot reads) with an empty transcript and a `userAbort`
                            // ending this run never earned; leaving it alone keeps its own
                            // ending honest (an `inProgress` there is a real hard kill).
                            if commit_pump.has_committed() {
                                capture_and_request(
                                    &mut commit_pump,
                                    CommitTrigger::UserAbort,
                                    &ctx,
                                    &*renderer,
                                );
                            }
                            renderer.request_user_exit();
                            renderer.shutdown();
                            return Ok(SessionStopReport::default());
                        }
                        if is_reset_command(&line) {
                            crate::debug_log::log_user_input("command", &line);
                            handle_reset_command_and_commit(
                                &mut commit_pump,
                                &plan,
                                &mut ctx,
                                &mut *renderer,
                                &mut ui_transcript,
                                &steering_history,
                            );
                            continue;
                        }
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            crate::debug_log::log_user_input("interactive_goal", trimmed);
                            break trimmed.to_string();
                        }
                    }
                    None => {
                        renderer.shutdown();
                        return Ok(SessionStopReport::default());
                    }
                }
            },
        },
    };

    if boot.is_some() {
        // Resume: both pin writers are off this path. `ctx.set_goal` is the second
        // clobber site and is never called, and the goal is not journaled again —
        // the seeded transcript already carries it.
        if let Some(prompt) = resume_prompt.filter(|prompt| !prompt.trim().is_empty()) {
            crate::debug_log::log_user_input("resume_prompt", &prompt);
            ctx.append(Message::User {
                content: prompt.clone(),
            });
            ui_transcript.append(UiRecord::User { text: prompt });
        }
    } else {
        ctx.set_goal(goal.clone());
        ui_transcript.append(UiRecord::User { text: goal.clone() });
    }

    // Every bound exit is collected here so the caller gets the same distinction
    // the operator gets on screen — a turn-cap stop is recorded and reportable,
    // never conflated with a clean finish.
    let mut session_stops: Vec<BoundStopReason> = Vec::new();

    crate::orchestrator::reset_cancellation();

    // Remember whether this round started with a plan file, so an archive that
    // lands mid-round is detectable at the round boundary (only this thread may
    // write the bundle).
    let mut plan_present_at_round_start = plan.exists();

    while !renderer.aborted() {
        let mut had_events = false;
        while let Ok(msg) = status_rx.try_recv() {
            renderer.on_event(&Event::Status(msg));
            had_events = true;
        }
        while let Ok(ev) = event_rx.try_recv() {
            if let Event::SteerResponse(ref text) = ev {
                ui_transcript.append(UiRecord::SteerResponse { text: text.clone() });
            }
            renderer.on_event(&ev);
            had_events = true;
        }
        if had_events {
            let _ = renderer.flush();
        }

        drain_steer_arbitration_events_with_transcript(
            &mut steer_arb_rx,
            &mut *renderer,
            &mut steer_queue,
            &mut steer_abort_requested,
            Some(&mut subagents),
            Some(&mut ui_transcript),
        );
        drain_delegation_events_with_transcript(
            manager.as_deref(),
            &mut *renderer,
            &mut subagents,
            Some(&mut ui_transcript),
        );
        // The pane's row set is a pure projection of the worker registry, rebuilt
        // on an edge the loop already has: a worker registered / finished since
        // the last harvest can now be joined, closed or pruned by key.
        // No new channel, no timer.
        project_subagent_pane(&mut subagents, &mut *renderer);

        if let Some(steer) = renderer.poll_input() {
            if is_abort_command(&steer) {
                crate::debug_log::log_user_input("command", &steer);
                renderer.request_user_exit();
                break;
            }
            if is_reset_command(&steer) {
                crate::debug_log::log_user_input("command", &steer);
                handle_reset_command_and_commit(
                    &mut commit_pump,
                    &plan,
                    &mut ctx,
                    &mut *renderer,
                    &mut ui_transcript,
                    &steering_history,
                );
                continue;
            }
            if !steer.trim().is_empty() {
                crate::debug_log::log_user_input("midflight_steer", &steer);
                ui_transcript.append(UiRecord::User {
                    text: steer.clone(),
                });
                // Whether a mid-flight steer gets its own arbitration round is
                // decided by **registry truth only** — the same single source the
                // pane rows, the arbitration snapshot and the status label now
                // come from.
                //
                // The old union added `|| subagents.iter().any(|s| s.is_active)`.
                // That term is dead here, and where it is not dead it is wrong:
                // `project_subagent_pane` rebuilt `subagents` from
                // `orchestrator::worker_entries()` a few statements above (same
                // iteration of this loop, before `poll_input`), so a row can only
                // be active **because** a registry entry is active — the term can
                // never add a case `has_active_workers()` does not already cover.
                // The one case where pane and registry disagree is a row that is
                // stale by design: a journaled panel restored with
                // `is_active: true` while the task behind it died with the
                // previous process (restore path above,
                // `NonRestorableKind::JoinHandle`). There the term only bought a
                // full arbitration round with nothing to arbitrate.
                //
                // A genuinely in-flight delegation is not lost either way: an
                // event-only row (a `Started` that beat its own registration) is
                // dropped by that same projection before this guard runs, so the
                // term never covered the window; and while a delegation really is
                // running the Manager is parked inside the tool call, where the
                // inner `poll_input` sites arbitrate unconditionally. A steer with
                // no live worker is applied at the next turn boundary through
                // `steer_queue` instead.
                if crate::orchestrator::has_active_workers() {
                    spawn_steer_arbitration(
                        &client,
                        harness_stats.clone(),
                        &goal,
                        &subagents,
                        steer,
                        &steer_arb_tx,
                        &mut *renderer,
                        Some(Arc::clone(&steering_history)),
                    );
                } else {
                    steer_queue.push(steer);
                }
            }
        }
        for steer in steer_queue.drain(..) {
            ctx.append(Message::User { content: steer });
        }

        let mut turn_count = 0;
        let mut nudge_count = 0;
        // Set when a session execution bound — a wall-clock deadline, or the
        // per-request turn budget — terminates this turn, carrying the
        // user-visible reason and its machine-readable code.
        let mut bound_stop_reason: Option<BoundStopReason> = None;
        loop {
            turn_count += 1;

            // T-5: a worker started / completed / failed / was preempted since
            // this was last checked. The flag is set from
            // `orchestrator::workers`, possibly on a worker thread — harvesting
            // and writing happen HERE, on the session thread.
            if take_worker_lifecycle_edge() {
                // The same edge that asks for a commit also re-projects the pane:
                // a worker registration, completion, failure or preemption just
                // happened, and the row set follows the registry — it is never
                // opened or closed by an event handler.
                project_subagent_pane(&mut subagents, &mut *renderer);
                commit_pump.request(CommitTrigger::WorkerEdge, &ctx, &*renderer);
            }

            // The turn-cap exit is recorded at the identical break point and
            // surfaced by the machinery the wall-clock exits already use (after
            // this loop): `tracing::warn!`, a renderer `Message` + `Status` event,
            // a matching `Status` record in the UI transcript, and the
            // machine-readable code the session report returns.
            if turn_count > crate::manager::r#loop::MAX_TURNS {
                let reason = BoundStopReason::TurnCap {
                    limit: crate::manager::r#loop::MAX_TURNS,
                    turn: turn_count,
                };
                tracing::warn!(
                    session_stop_reason = reason.code(),
                    max_turns = crate::manager::r#loop::MAX_TURNS,
                    turn_count,
                    "turn budget exhausted: {}",
                    reason.text()
                );
                // This break happens at the **top** of the loop, so it never
                // passes the round-boundary repair below. The transcript is
                // already repaired by the previous round's boundary, but the
                // invariant is asserted here as well (idempotent, and only
                // surfaced if it actually changed something), so a capped finish
                // always leaves a valid chat sequence on disk and on the next
                // request.
                ensure_pairing_at_boundary(
                    "before turn-budget exit",
                    &mut ctx,
                    &mut *renderer,
                    &mut ui_transcript,
                );

                // T-1 at a top-of-loop exit: a capped session still ends with a
                // committed round, not with a silently discarded one.
                capture_and_request(
                    &mut commit_pump,
                    CommitTrigger::ManagerTurnBoundary,
                    &ctx,
                    &*renderer,
                );

                bound_stop_reason = Some(reason);
                break;
            }
            if renderer.aborted() || renderer.user_exit_requested() {
                // The abort commit must land BEFORE the `break`, while the round's
                // facts and a live renderer are still reachable — this is the last
                // bundle a session the user stopped ever has to write, so it is
                // mandatory and never throttled.
                capture_and_request(&mut commit_pump, CommitTrigger::UserAbort, &ctx, &*renderer);
                break;
            }
            // Bound ONE turn of the request on the wall clock.
            watchdog.rearm();

            // Status traffic from live workers/specialists is forward progress:
            // it refreshes the stalled-turn window so a legitimately long
            // delegation is never cut off by the watchdog.
            while let Ok(msg) = status_rx.try_recv() {
                renderer.on_event(&Event::Status(msg));
                watchdog.note_progress();
            }
            drain_steer_arbitration_events_with_transcript(
                &mut steer_arb_rx,
                &mut *renderer,
                &mut steer_queue,
                &mut steer_abort_requested,
                Some(&mut subagents),
                Some(&mut ui_transcript),
            );
            drain_delegation_events_with_transcript(
                manager.as_deref(),
                &mut *renderer,
                &mut subagents,
                Some(&mut ui_transcript),
            );
            renderer.flush()?;

            for steer in steer_queue.drain(..) {
                ctx.append(Message::User { content: steer });
            }

            // Re-price the advertised tool schema before every budget decision. The
            // advertised Manager list is not static (MCP servers boot lazily /
            // reconnect and the MCP view is policy-filtered per request), so a
            // one-time charge could compact against a stale schema list.
            sync_manager_tool_schema(&mut ctx, cfg);
            if ctx.should_compact() {
                let outcome = ctx.compact();
                renderer.on_event(&Event::TokensIn(ctx.token_count()));
                surface_compaction_outcome(&mut *renderer, &mut ui_transcript, &outcome);
            } else if ctx.should_advise_rebirth() {
                ctx.inject_rebirth_advisory();
                renderer.on_event(&Event::Status(
                    "context advisory: rebirth recommended (>= 80% budget)".to_string(),
                ));
            }

            renderer.reset_active_agent();
            renderer.on_event(&Event::TokensIn(ctx.token_count()));
            renderer.on_event(&Event::Status(format!("Running ({})", stream_cfg.model)));
            renderer.flush()?;

            // This is the exact line that turns the transcript into the next
            // provider request, so the tool-call pairing invariant is enforced here
            // as well — one idempotent call, grammar owned by `context.rs`. A
            // repair here means some earlier path ended a turn with an assistant
            // `tool_calls` entry and no result for it; it is surfaced instead of
            // silently rewritten.
            ensure_pairing_at_boundary(
                "before request",
                &mut ctx,
                &mut *renderer,
                &mut ui_transcript,
            );

            let msgs = ctx.messages().to_vec();
            let mut bridge = RendererSink {
                renderer: &mut *renderer,
                steer_queue: &mut steer_queue,
                steer_abort_requested: &mut steer_abort_requested,
                arb_tx: &steer_arb_tx,
                arb_rx: &mut steer_arb_rx,
                client: &client,
                stats: harness_stats.clone(),
                goal: &goal,
                subagents: &subagents,
                plan: Some(&plan),
                ctx: Some(&mut ctx),
                steering_history: Some(Arc::clone(&steering_history)),
            };
            // No await in the live loop may be unbounded. A single backend call is
            // bounded by the absolute per-turn cap (`src/llm/client.rs` caps one
            // stream at `OVERALL_READ_TIMEOUT_SECS`; this cap is the backstop for a
            // turn that keeps re-entering the backend, and it makes an endless turn
            // impossible). On expiry: cancel in-flight work through the same
            // plumbing the abort paths use, and say why.
            let llm_budget = watchdog.time_to_hard_limit();
            // The scope wraps the whole turn, so `ManagerTurn` is the class the
            // pool gate sees for this backend call on whichever worker polls the
            // turn, and it is discharged the moment the turn returns, times out or
            // is cancelled — never carried into the tool round below.
            let assistant = match pool::with_call_context(
                manager_call_context.clone(),
                tokio::time::timeout(
                    llm_budget,
                    chat_client_turn(&client, msgs, &stream_cfg, &mut bridge),
                ),
            )
            .await
            {
                Err(_elapsed) => {
                    let kind = watchdog.expired().unwrap_or(DeadlineKind::HardCap);
                    let reason = kind.describe(bounds.turn_idle, bounds.turn_hard_cap);
                    tracing::warn!(
                        elapsed_secs = watchdog.elapsed().as_secs(),
                        "turn bound reached during backend call: {reason}"
                    );
                    crate::orchestrator::cancel_all();
                    bound_stop_reason = Some(BoundStopReason::Deadline {
                        kind,
                        text: reason.clone(),
                    });
                    renderer.on_event(&Event::Message(format!("\n[{reason}]\n")));
                    renderer.on_event(&Event::Status(format!("{} (Ready)", kind.label())));
                    renderer.flush()?;
                    break;
                }
                Ok(Ok(m)) => m,
                Ok(Err(e)) => {
                    let category = classify_llm_error(&e);
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        category = %category,
                        "LLM turn failed"
                    );
                    renderer.on_event(&Event::Message(format!(
                            "\n[Error] LLM backend call failed ({category}): {e:#}\n(Check that your LLM server is running at {} for model `{}`)",
                            client.backend_url(),
                            stream_cfg.model
                        )));
                    renderer.on_event(&Event::Status(format!("LLM error: {category} (Ready)")));
                    renderer.flush()?;
                    break;
                }
            };
            // A completed backend call is forward progress for the turn.
            watchdog.note_progress();

            if renderer.aborted()
                || renderer.user_exit_requested()
                || crate::orchestrator::is_globally_cancelled()
            {
                break;
            }

            let tool_calls = match &assistant {
                Message::Assistant {
                    tool_calls,
                    content,
                    ..
                } => {
                    if tool_calls.is_empty() {
                        if let Some(text) = content {
                            crate::debug_log::log_user_output("assistant_reply", text);
                            if cfg.enable_xml_rescue {
                                monitor.rescue_xml(text)
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        tool_calls.clone()
                    }
                }
                _ => Vec::new(),
            };

            ctx.append(assistant);
            let round_trigger = if tool_calls.is_empty() {
                CommitTrigger::ManagerTurnBoundary
            } else {
                CommitTrigger::PreDispatch
            };
            capture_and_request(&mut commit_pump, round_trigger, &ctx, &*renderer);
            if let Some(Message::Assistant {
                content,
                reasoning_content,
                ..
            }) = ctx.messages().last()
            {
                let c = content.as_ref().filter(|s| !s.trim().is_empty()).cloned();
                let r = reasoning_content
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .cloned();
                if c.is_some() || r.is_some() {
                    ui_transcript.append(UiRecord::Assistant {
                        content: c,
                        thinking: r,
                    });
                }
            }
            renderer.flush()?;

            if tool_calls.is_empty() {
                ctx.reset_consecutive_rebirths();
                sync_manager_tool_schema(&mut ctx, cfg);
                if ctx.should_compact() {
                    let outcome = ctx.compact();
                    capture_and_request(&mut commit_pump, CommitTrigger::UiEvent, &ctx, &*renderer);
                    renderer.on_event(&Event::TokensIn(ctx.token_count()));
                    surface_compaction_outcome(&mut *renderer, &mut ui_transcript, &outcome);
                } else if ctx.should_advise_rebirth() {
                    ctx.inject_rebirth_advisory();
                    capture_and_request(&mut commit_pump, CommitTrigger::UiEvent, &ctx, &*renderer);
                    renderer.on_event(&Event::Status(
                        "context advisory: rebirth recommended (>= 80% budget)".to_string(),
                    ));
                }
            }

            for steer in steer_queue.drain(..) {
                ctx.append(Message::User { content: steer });
            }

            if tool_calls.is_empty() {
                let current_plan = manager.as_ref().map(|m| m.plan.clone()).unwrap_or_default();
                // Read the plan fail-closed. A plan that cannot be read or parsed
                // is shown to the user through the warning channel instead of being
                // reported as "no pending work".
                let pending = match read_plan_gate(&current_plan) {
                    PlanGate::Read { pending, .. } => pending,
                    PlanGate::Unknown { warning } => {
                        surface_plan_warning(&mut *renderer, &mut ui_transcript, warning);
                        Vec::new()
                    }
                };
                if !steer_abort_requested
                    && !renderer.aborted()
                    && !crate::orchestrator::is_globally_cancelled()
                    && !pending.is_empty()
                    && nudge_count < 5
                {
                    nudge_count += 1;
                    let pending_str = pending.join(", ");
                    renderer.on_event(&Event::Status(format!(
                        "Auto-nudge ({nudge_count}/5): Plan incomplete (pending: {pending_str})"
                    )));
                    renderer.flush()?;
                    ctx.append(Message::User {
                        content: format!(
                            "(SYSTEM NOTICE: The execution plan is active on disk with pending tasks: [{pending_str}]. You are in the EXECUTING phase. You must call `delegate_task` to dispatch these pending tasks to specialists. Do NOT call `create_plan` again, and do NOT output conversational filler until all tasks are marked [x].)"
                        ),
                    });
                    continue;
                }
                break;
            }

            nudge_count = 0;

            let all_parallel = tool_calls.iter().all(|c| {
                c.function.name == crate::tool_names::TOOL_DELEGATE_TASK
                    || crate::manager::is_read_tool(&c.function.name)
            });

            if all_parallel && tool_calls.len() > 1 {
                ctx.reset_consecutive_rebirths();
                let mut handles = Vec::new();
                // Aligned index-for-index with `handles`: which unit of work each
                // joined result belongs to (failure budget).
                let mut failure_keys: Vec<String> = Vec::new();
                for call in &tool_calls {
                    let name = call.function.name.clone();
                    let args_str = call.function.arguments.clone();
                    let args_val = serde_json::from_str::<serde_json::Value>(&args_str)
                        .unwrap_or_else(|_| serde_json::Value::String(args_str.clone()));

                    let is_delegate = name == crate::tool_names::TOOL_DELEGATE_TASK;
                    let delegated_agent = if is_delegate {
                        args_val
                            .get("agent_name")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|s| s.parse::<crate::agents::Agent>().ok())
                    } else {
                        None
                    };
                    let delegated_task = if is_delegate {
                        args_val
                            .get("task_id")
                            .and_then(serde_json::Value::as_str)
                            .and_then(crate::task_id::normalize_task_id)
                    } else {
                        None
                    };

                    // Failure budget: one identity per repeated unit of work — the
                    // plan task id for delegations, the tool signature for
                    // everything else.
                    let failure_key = failure_key_for(&name, &args_val, delegated_task.as_deref());
                    failure_keys.push(failure_key.clone());

                    if let Some(agent) = delegated_agent {
                        let task_prompt = args_val
                            .get("prompt")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        update_subagent_lifecycle(
                            &mut subagents,
                            agent,
                            delegated_task.clone(),
                            Some(task_prompt),
                            true,
                        );
                        renderer.set_subagents(subagents.clone());
                        renderer.on_event(&Event::Delegation(
                            crate::orchestrator::DelegationEvent::Started {
                                agent,
                                task: delegated_task.clone(),
                            },
                        ));
                    } else {
                        let display = format_tool_call_display(&name, &args_val);
                        renderer.on_event(&Event::ToolCall(display.clone()));
                        ui_transcript.append(UiRecord::ToolCall { display });
                    }

                    if call.is_malformed() {
                        let call_id = call.id.clone();
                        let err_msg = format!(
                            "ERROR: Invalid or truncated arguments for tool '{name}': output was cut off or contained unterminated JSON. Please reissue the tool call with complete, valid JSON arguments."
                        );
                        let handle = tokio::task::spawn_blocking(move || {
                            (
                                call_id,
                                delegated_agent,
                                delegated_task,
                                Ok(crate::harness::ToolResult::err(err_msg)),
                            )
                        });
                        handles.push(handle);
                        continue;
                    }

                    // Repeated-failure budget: once the same unit of work has
                    // failed `threshold` times in this session, do not dispatch it
                    // again. The refusal is reported as a normal tool error so the
                    // model sees the escalation instead of an endless retry loop.
                    if !failures.retry_allowed(&failure_key) {
                        let count = failures.count(&failure_key);
                        let err_msg = failure_refusal_message(&failure_key, count);
                        tracing::warn!(
                            failures = count,
                            "failure budget exhausted, refusing to retry {failure_key}"
                        );
                        let call_id = call.id.clone();
                        let handle = tokio::task::spawn_blocking(move || {
                            (
                                call_id,
                                delegated_agent,
                                delegated_task,
                                Ok(crate::harness::ToolResult::err(err_msg)),
                            )
                        });
                        handles.push(handle);
                        continue;
                    }

                    let intervention = monitor.observe_tool(&name, &args_val);
                    if matches!(
                        intervention,
                        crate::harness::monitor::Intervention::Block
                            | crate::harness::monitor::Intervention::Cut
                    ) {
                        let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                            format!(
                                "ERROR: Tool repetition detected for '{}'. Do not repeat identical calls.",
                                call.function.name
                            )
                        });
                        tracing::warn!(
                            "Manager tool {} blocked by repetition detector",
                            call.function.name
                        );
                        let call_id = call.id.clone();
                        let handle = tokio::task::spawn_blocking(move || {
                            (
                                call_id,
                                delegated_agent,
                                delegated_task,
                                Ok(crate::harness::ToolResult::err(err_msg)),
                            )
                        });
                        handles.push(handle);
                        continue;
                    }

                    let invocation = crate::harness::ToolInvocation {
                        name: name.clone(),
                        arguments: args_val,
                    };

                    let call_id = call.id.clone();
                    // Tool work runs on a blocking thread, which a task-local never
                    // reaches by itself, so the Manager context is carried across
                    // explicitly: a `delegate_task` fanned out here inherits it and
                    // then overwrites it with its own agent/task (see
                    // `orchestrator::delegate::handle_delegate_task`).
                    let manager_context = manager_call_context.clone();
                    let handle = tokio::task::spawn_blocking(move || {
                        pool::with_call_context_sync(manager_context, || {
                            (
                                call_id,
                                delegated_agent,
                                delegated_task,
                                crate::harness::dispatch_for(
                                    &invocation,
                                    crate::harness::ToolCaller::Manager,
                                ),
                            )
                        })
                    });
                    handles.push(handle);
                }
                // A failing flush here propagates out of `run_session`. The round
                // has just been fanned out, so every handle in `handles` is still
                // in flight: `?` would detach them and let delegated/parallel work
                // (and its PTY process groups) keep running after the session
                // aborted. Cancel through the same plumbing the abort paths use
                // (`cancel_all` -> global token, which `src/harness/pty.rs` polls
                // to SIGKILL process groups, plus `cancel_all_active_workers`) and
                // abort the handles we still own.
                if let Err(e) = renderer.flush() {
                    crate::orchestrator::cancel_all();
                    for handle in handles.iter() {
                        handle.abort();
                    }
                    // This round has been fanned out but never joined, so the
                    // assistant `tool_calls` in the transcript have no results. A
                    // rehydration installs the newest manager capture, so close the
                    // invariant and commit before leaving: this is the round's last
                    // chance to reach disk. The ending stays `inProgress` — no
                    // terminal marker was reached, so a later boot infers the death
                    // (`HardKillInferred`) rather than a clean exit.
                    ctx.ensure_tool_call_pairs();
                    capture_and_request(
                        &mut commit_pump,
                        CommitTrigger::ManagerTurnBoundary,
                        &ctx,
                        &*renderer,
                    );
                    return Err(e);
                }

                // Indexed so the still-unjoined tail of `handles` stays reachable
                // from the error path below.
                for idx in 0..handles.len() {
                    // A bound hit in an earlier join iteration already cancelled
                    // everything: drop out instead of re-joining the rest.
                    if bound_stop_reason.is_some() {
                        break;
                    }
                    let mut handle = &mut handles[idx];
                    // Attribute this join result to the unit of work that produced it.
                    let failure_key = failure_keys[idx].clone();
                    if renderer.user_exit_requested() {
                        break;
                    }
                    let res = loop {
                        let mut had_events = false;
                        while let Ok(msg) = status_rx.try_recv() {
                            renderer.on_event(&Event::Status(msg));
                            had_events = true;
                        }
                        while let Ok(ev) = event_rx.try_recv() {
                            if let Event::SteerResponse(ref text) = ev {
                                ui_transcript
                                    .append(UiRecord::SteerResponse { text: text.clone() });
                            }
                            renderer.on_event(&ev);
                            had_events = true;
                        }
                        if had_events {
                            let _ = renderer.flush();
                            watchdog.note_progress();
                        }
                        drain_steer_arbitration_events_with_transcript(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                            &mut steer_abort_requested,
                            Some(&mut subagents),
                            Some(&mut ui_transcript),
                        );
                        drain_delegation_events_with_transcript(
                            manager.as_deref(),
                            &mut *renderer,
                            &mut subagents,
                            Some(&mut ui_transcript),
                        );
                        if renderer.user_exit_requested() {
                            // The abort commit lands BEFORE `cancel_all()` and
                            // before the in-flight handle is torn down by the
                            // round's teardown, so a session stopped mid-round
                            // leaves a bundle that says so instead of one that
                            // claims `inProgress`.
                            capture_and_request(
                                &mut commit_pump,
                                CommitTrigger::UserAbort,
                                &ctx,
                                &*renderer,
                            );
                            crate::orchestrator::cancel_all();
                            break (
                                String::new(),
                                None,
                                None,
                                Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                    "aborted by user"
                                ))),
                            );
                        }
                        if renderer.aborted()
                            || steer_abort_requested
                            || crate::orchestrator::is_globally_cancelled()
                        {
                            crate::orchestrator::cancel_all();
                            let _ =
                                tokio::time::timeout(Duration::from_millis(500), &mut handle).await;
                            break (
                                String::new(),
                                None,
                                None,
                                Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                    "aborted"
                                ))),
                            );
                        }
                        // Wall-clock bound: a turn that stops making progress (or
                        // exceeds the absolute cap) cancels the in-flight round
                        // through the same plumbing the abort paths use.
                        if let Some(kind) = watchdog.expired() {
                            let reason = kind.describe(bounds.turn_idle, bounds.turn_hard_cap);
                            tracing::warn!(
                                elapsed_secs = watchdog.elapsed().as_secs(),
                                "turn bound reached while awaiting parallel tool round: {reason}"
                            );
                            crate::orchestrator::cancel_all();
                            bound_stop_reason = Some(BoundStopReason::Deadline {
                                kind,
                                text: reason.clone(),
                            });
                            break (
                                String::new(),
                                None,
                                None,
                                Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                    "{reason}"
                                ))),
                            );
                        }
                        match tokio::time::timeout(join_poll_slice(&watchdog), &mut handle).await {
                            Ok(Ok(r)) => break r,
                            Ok(Err(e)) => {
                                break (
                                    String::new(),
                                    None,
                                    None,
                                    Err(crate::harness::ToolError::Execution(e.into())),
                                );
                            }
                            Err(_) => {
                                let mut had_events = false;
                                while let Ok(msg) = status_rx.try_recv() {
                                    renderer.on_event(&Event::Status(msg));
                                    had_events = true;
                                }
                                while let Ok(ev) = event_rx.try_recv() {
                                    if let Event::SteerResponse(ref text) = ev {
                                        ui_transcript
                                            .append(UiRecord::SteerResponse { text: text.clone() });
                                    }
                                    renderer.on_event(&ev);
                                    had_events = true;
                                }
                                if had_events {
                                    let _ = renderer.flush();
                                    watchdog.note_progress();
                                }
                                drain_steer_arbitration_events_with_transcript(
                                    &mut steer_arb_rx,
                                    &mut *renderer,
                                    &mut steer_queue,
                                    &mut steer_abort_requested,
                                    Some(&mut subagents),
                                    Some(&mut ui_transcript),
                                );
                                drain_delegation_events_with_transcript(
                                    manager.as_deref(),
                                    &mut *renderer,
                                    &mut subagents,
                                    Some(&mut ui_transcript),
                                );
                                if let Some(input) = renderer.poll_input() {
                                    if is_abort_command(&input) {
                                        crate::debug_log::log_user_input("command", &input);
                                        renderer.request_user_exit();
                                    } else if is_reset_command(&input) {
                                        handle_reset_command_and_commit(
                                            &mut commit_pump,
                                            &plan,
                                            &mut ctx,
                                            &mut *renderer,
                                            &mut ui_transcript,
                                            &steering_history,
                                        );
                                    } else if !input.trim().is_empty() {
                                        spawn_steer_arbitration(
                                            &client,
                                            harness_stats.clone(),
                                            &goal,
                                            &subagents,
                                            input,
                                            &steer_arb_tx,
                                            &mut *renderer,
                                            Some(Arc::clone(&steering_history)),
                                        );
                                    }
                                }
                                if renderer.user_exit_requested() {
                                    // Same rule one level deeper: the round is
                                    // still in flight here too.
                                    capture_and_request(
                                        &mut commit_pump,
                                        CommitTrigger::UserAbort,
                                        &ctx,
                                        &*renderer,
                                    );
                                    crate::orchestrator::cancel_all();
                                    break (
                                        String::new(),
                                        None,
                                        None,
                                        Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                            "aborted by user"
                                        ))),
                                    );
                                }
                            }
                        }
                    };

                    let (call_id, agent, task, tool_res) = res;
                    let (result_content, is_error) = match tool_res {
                        Ok(r) => (r.content, r.is_error),
                        Err(e) => (format!("ERROR: {e}"), true),
                    };
                    if let Some(ag) = agent {
                        update_subagent_lifecycle(&mut subagents, ag, task.clone(), None, false);
                        let tid = task.clone().unwrap_or_else(|| ag.to_string());
                        let clean_tid = task.as_deref().map(clean_task_id);
                        let sa_name = match &clean_tid {
                            Some(t) if !t.is_empty() => format!("{}-{t}", ag.as_str()),
                            _ => ag.as_str().to_string(),
                        };
                        if let Some(sa) =
                            find_subagent_mut(&mut subagents, &sa_name, clean_tid.as_deref())
                            && !is_error
                        {
                            sa.content = result_content.clone();
                            sa.context_tokens = count_text_tokens(&result_content);
                        }
                        if is_error {
                            let reason =
                                crate::ui::helpers::extract_failure_reason(&result_content);
                            renderer.on_event(&Event::Delegation(
                                crate::orchestrator::DelegationEvent::Failed {
                                    agent: ag,
                                    task,
                                    reason: Some(reason.clone()),
                                },
                            ));
                            ui_transcript.append(UiRecord::TaskFailed {
                                task_id: tid,
                                reason: Some(reason),
                            });
                        } else {
                            // `is_error == false` is not completion evidence: the
                            // plan box flips only through the orchestrator's
                            // marker-checked entry point
                            // (`Plan::check_plan_on_deliverable`), so a deliverable
                            // with no valid completion marker leaves its plan task
                            // pending. The outcome is classified before anything is
                            // said: `handle_delegate_task` already ran
                            // `apply_check_off` for a marker-bearing deliverable, so
                            // an `AlreadyChecked` here is the normal path and must
                            // NOT be reported as a missing marker.
                            if let Some(ref tid_task) = task {
                                let outcome =
                                    marker_gated_check_off(&plan, Some(tid_task), &result_content);
                                if let Some(warning) = check_off_warning(Some(tid_task), &outcome) {
                                    surface_plan_warning(
                                        &mut *renderer,
                                        &mut ui_transcript,
                                        warning,
                                    );
                                }
                            }
                            renderer.on_event(&Event::Delegation(
                                crate::orchestrator::DelegationEvent::Completed { agent: ag, task },
                            ));
                            ui_transcript.append(UiRecord::TaskCompleted { task_id: tid });
                        }
                        // Registry first, then the authoritative push: a delegation
                        // tool result is exactly the edge where the worker entry
                        // appears (nested `delegate_task`) or goes inactive.
                        project_subagent_pane(&mut subagents, &mut *renderer);
                        renderer.set_subagents(subagents.clone());
                    } else {
                        project_subagent_pane(&mut subagents, &mut *renderer);
                        renderer.on_event(&Event::ToolResult(result_content.clone()));
                        ui_transcript.append(UiRecord::ToolResult {
                            display: result_content.clone(),
                        });
                    }
                    drain_delegation_events_with_transcript(
                        manager.as_deref(),
                        &mut *renderer,
                        &mut subagents,
                        Some(&mut ui_transcript),
                    );

                    ctx.append(Message::Tool {
                        tool_call_id: call_id,
                        content: result_content,
                    });
                    capture_and_request(
                        &mut commit_pump,
                        CommitTrigger::ToolResult,
                        &ctx,
                        &*renderer,
                    );

                    // Repeated-failure accounting: an abort/cancel result is not
                    // the tool's own failure, so it must not consume the budget.
                    if is_error
                        && !(renderer.aborted()
                            || renderer.user_exit_requested()
                            || crate::orchestrator::is_globally_cancelled())
                    {
                        match failures.record(&failure_key) {
                            FailureVerdict::Continue { .. } => {}
                            FailureVerdict::Escalate { count } => {
                                let note = failure_escalation_note(&failure_key, count);
                                tracing::warn!(
                                    failures = count,
                                    "repeated failure escalation for {failure_key}"
                                );
                                ctx.append(Message::User {
                                    content: note.clone(),
                                });
                                renderer.on_event(&Event::Status(format!(
                                    "repeated failure: {} failed {count}x (strategy change required)",
                                    failure_label(&failure_key)
                                )));
                                ui_transcript.append(UiRecord::Status { text: note });
                            }
                        }
                    } else if !is_error {
                        failures.clear(&failure_key);
                    }
                    watchdog.note_progress();

                    // Flush only after the round result just received has been
                    // appended to the context/transcript, so an error raised here
                    // cannot drop a completed deliverable. If the write does fail,
                    // cancel + abort the still-unjoined tail of the round instead of
                    // detaching it (see the note above the join loop).
                    if let Err(e) = renderer.flush() {
                        crate::orchestrator::cancel_all();
                        for handle in handles[idx + 1..].iter() {
                            handle.abort();
                        }
                        // The tail of the round is torn down here, so the calls
                        // after `idx` never get a `Message::Tool`. Repair and capture
                        // before propagating, so the manager state a rehydration
                        // would install is a valid chat request. That capture is a
                        // real commit: this return leaves the loop entirely, and
                        // without it the joined half of the round (already a
                        // completed deliverable) would exist only in memory.
                        ctx.ensure_tool_call_pairs();
                        capture_and_request(
                            &mut commit_pump,
                            CommitTrigger::ManagerTurnBoundary,
                            &ctx,
                            &*renderer,
                        );
                        return Err(e);
                    }

                    if renderer.user_exit_requested() {
                        break;
                    }
                }
                // The break paths above (bound, abort, user exit) can leave the rest
                // of the round running. Cancel through the shared token and abort
                // every handle we still own; aborting a completed handle is a no-op,
                // so this only closes genuinely detached work.
                for handle in handles.iter() {
                    handle.abort();
                }
            } else {
                for call in &tool_calls {
                    if bound_stop_reason.is_some() {
                        break;
                    }
                    if renderer.aborted() || renderer.user_exit_requested() {
                        break;
                    }

                    let name = call.function.name.clone();
                    let args_str = call.function.arguments.clone();
                    let args_val = serde_json::from_str::<serde_json::Value>(&args_str)
                        .unwrap_or_else(|_| serde_json::Value::String(args_str.clone()));

                    let is_delegate = name == crate::tool_names::TOOL_DELEGATE_TASK;
                    let args_obj = args_val.as_object();
                    let delegated_agent = if is_delegate {
                        args_obj
                            .and_then(|o| o.get("agent_name"))
                            .and_then(serde_json::Value::as_str)
                            .and_then(|s| s.parse::<crate::agents::Agent>().ok())
                    } else {
                        None
                    };
                    let delegated_task = if is_delegate {
                        args_obj
                            .and_then(|o| o.get("task_id"))
                            .and_then(serde_json::Value::as_str)
                            .and_then(crate::task_id::normalize_task_id)
                    } else {
                        None
                    };

                    let failure_key = failure_key_for(&name, &args_val, delegated_task.as_deref());

                    if let Some(agent) = delegated_agent {
                        let task_prompt = args_obj
                            .and_then(|o| o.get("prompt"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        update_subagent_lifecycle(
                            &mut subagents,
                            agent,
                            delegated_task.clone(),
                            Some(task_prompt),
                            true,
                        );
                        renderer.set_subagents(subagents.clone());
                        renderer.on_event(&Event::Delegation(
                            crate::orchestrator::DelegationEvent::Started {
                                agent,
                                task: delegated_task.clone(),
                            },
                        ));
                    } else {
                        let display = format_tool_call_display(&name, &args_val);
                        renderer.on_event(&Event::ToolCall(display.clone()));
                        ui_transcript.append(UiRecord::ToolCall { display });
                    }
                    renderer.flush()?;

                    drain_delegation_events_with_transcript(
                        manager.as_deref(),
                        &mut *renderer,
                        &mut subagents,
                        Some(&mut ui_transcript),
                    );

                    if name == crate::tool_names::TOOL_REBIRTH {
                        renderer
                            .on_event(&Event::ToolCall(format_tool_call_display(&name, &args_val)));
                        renderer.flush()?;
                        let res = crate::harness::handle_rebirth(&mut ctx, &args_val);
                        match res {
                            Ok(r) => {
                                renderer.on_event(&Event::ToolResult(r.content.clone()));
                                if !r.is_error {
                                    ui_transcript.append(UiRecord::Status {
                                        text: format!("Rebirth checkpoint applied: {}", r.content),
                                    });
                                    capture_and_request(
                                        &mut commit_pump,
                                        CommitTrigger::UiEvent,
                                        &ctx,
                                        &*renderer,
                                    );
                                    renderer.on_event(&Event::TokensIn(ctx.token_count()));
                                    renderer.on_event(&Event::Status(
                                        "rebirth checkpoint applied".to_string(),
                                    ));
                                    let current_plan = manager
                                        .as_ref()
                                        .map(|m| m.plan.clone())
                                        .unwrap_or_default();
                                    // Same fail-closed plan read as the auto-nudge:
                                    // an unreadable/unparseable plan is surfaced,
                                    // never reported as "nothing pending".
                                    let pending = match read_plan_gate(&current_plan) {
                                        PlanGate::Read { pending, .. } => pending,
                                        PlanGate::Unknown { warning } => {
                                            surface_plan_warning(
                                                &mut *renderer,
                                                &mut ui_transcript,
                                                warning,
                                            );
                                            Vec::new()
                                        }
                                    };
                                    let continuation_msg = if !pending.is_empty() {
                                        let pending_str = pending.join(", ");
                                        format!(
                                            "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. You have active pending tasks in your plan: [{pending_str}]. Do not call rebirth consecutively without making progress. Proceed immediately with delegating or executing these pending tasks.)"
                                        )
                                    } else {
                                        "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. Do not call rebirth consecutively without making progress. Proceed immediately with delivering your final synthesis to the user.)".to_string()
                                    };
                                    ctx.append(Message::User {
                                        content: continuation_msg,
                                    });
                                    capture_and_request(
                                        &mut commit_pump,
                                        CommitTrigger::UiEvent,
                                        &ctx,
                                        &*renderer,
                                    );
                                } else {
                                    ctx.append(Message::Tool {
                                        tool_call_id: call.id.clone(),
                                        content: format!("ERROR: {}", r.content),
                                    });
                                    capture_and_request(
                                        &mut commit_pump,
                                        CommitTrigger::ToolResult,
                                        &ctx,
                                        &*renderer,
                                    );
                                }
                            }
                            Err(e) => {
                                renderer.on_event(&Event::ToolResult(format!("ERROR: {e}")));
                                ctx.append(Message::Tool {
                                    tool_call_id: call.id.clone(),
                                    content: format!("ERROR: {e}"),
                                });
                                capture_and_request(
                                    &mut commit_pump,
                                    CommitTrigger::ToolResult,
                                    &ctx,
                                    &*renderer,
                                );
                            }
                        }
                        renderer.flush()?;
                        continue;
                    }

                    ctx.reset_consecutive_rebirths();

                    if call.is_malformed() {
                        let err_msg = format!(
                            "ERROR: Invalid or truncated arguments for tool '{name}': output was cut off or contained unterminated JSON. Please reissue the tool call with complete, valid JSON arguments."
                        );
                        renderer.on_event(&Event::ToolResult(err_msg.clone()));
                        renderer.flush()?;
                        ctx.append(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: err_msg,
                        });
                        capture_and_request(
                            &mut commit_pump,
                            CommitTrigger::ToolResult,
                            &ctx,
                            &*renderer,
                        );
                        continue;
                    }

                    // Repeated-failure budget: refuse to re-dispatch work that has
                    // already failed the threshold number of times this session.
                    if !failures.retry_allowed(&failure_key) {
                        let count = failures.count(&failure_key);
                        let err_msg = failure_refusal_message(&failure_key, count);
                        tracing::warn!(
                            failures = count,
                            "failure budget exhausted, refusing to retry {failure_key}"
                        );
                        renderer.on_event(&Event::ToolResult(err_msg.clone()));
                        renderer.flush()?;
                        ctx.append(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: err_msg,
                        });
                        capture_and_request(
                            &mut commit_pump,
                            CommitTrigger::ToolResult,
                            &ctx,
                            &*renderer,
                        );
                        continue;
                    }
                    let intervention = monitor.observe_tool(&name, &args_val);
                    if matches!(
                        intervention,
                        crate::harness::monitor::Intervention::Block
                            | crate::harness::monitor::Intervention::Cut
                    ) {
                        let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                            format!(
                                "ERROR: Tool repetition detected for '{}'. Do not repeat identical calls.",
                                call.function.name
                            )
                        });
                        tracing::warn!(
                            "Manager tool {} blocked by repetition detector",
                            call.function.name
                        );
                        renderer.on_event(&Event::ToolResult(format!("ERROR: {err_msg}")));
                        renderer.flush()?;
                        ctx.append(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: format!("ERROR: {err_msg}"),
                        });
                        capture_and_request(
                            &mut commit_pump,
                            CommitTrigger::ToolResult,
                            &ctx,
                            &*renderer,
                        );
                        continue;
                    }

                    let invocation = crate::harness::ToolInvocation {
                        name: name.clone(),
                        arguments: args_val,
                    };

                    // Same explicit propagation as the parallel round: the tool
                    // runs on a blocking thread, and a nested `delegate_task`
                    // installed there must be attributed to this session's Manager.
                    let manager_context = manager_call_context.clone();
                    let mut handle = tokio::task::spawn_blocking(move || {
                        pool::with_call_context_sync(manager_context, || {
                            crate::harness::dispatch_for(
                                &invocation,
                                crate::harness::ToolCaller::Manager,
                            )
                        })
                    });

                    let result = loop {
                        let mut had_events = false;
                        while let Ok(msg) = status_rx.try_recv() {
                            renderer.on_event(&Event::Status(msg));
                            had_events = true;
                        }
                        while let Ok(ev) = event_rx.try_recv() {
                            if let Event::SteerResponse(ref text) = ev {
                                ui_transcript
                                    .append(UiRecord::SteerResponse { text: text.clone() });
                            }
                            renderer.on_event(&ev);
                            had_events = true;
                        }
                        if had_events {
                            let _ = renderer.flush();
                            watchdog.note_progress();
                        }
                        drain_steer_arbitration_events_with_transcript(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                            &mut steer_abort_requested,
                            Some(&mut subagents),
                            Some(&mut ui_transcript),
                        );
                        drain_delegation_events_with_transcript(
                            manager.as_deref(),
                            &mut *renderer,
                            &mut subagents,
                            Some(&mut ui_transcript),
                        );
                        if renderer.user_exit_requested() {
                            // The sequential branch breaks out of a live call; the
                            // abort must still be recorded first.
                            capture_and_request(
                                &mut commit_pump,
                                CommitTrigger::UserAbort,
                                &ctx,
                                &*renderer,
                            );
                            crate::orchestrator::cancel_all();
                            break Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                "aborted by user"
                            )));
                        }
                        if renderer.aborted()
                            || steer_abort_requested
                            || crate::orchestrator::is_globally_cancelled()
                        {
                            crate::orchestrator::cancel_all();
                            let _ =
                                tokio::time::timeout(Duration::from_millis(500), &mut handle).await;
                            break Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                "aborted"
                            )));
                        }
                        // Wall-clock bound: a turn that stops making progress (or
                        // exceeds the absolute cap) cancels the in-flight round
                        // through the same plumbing the abort paths use.
                        if let Some(kind) = watchdog.expired() {
                            let reason = kind.describe(bounds.turn_idle, bounds.turn_hard_cap);
                            tracing::warn!(
                                elapsed_secs = watchdog.elapsed().as_secs(),
                                "turn bound reached while awaiting sequential tool call: {reason}"
                            );
                            crate::orchestrator::cancel_all();
                            bound_stop_reason = Some(BoundStopReason::Deadline {
                                kind,
                                text: reason.clone(),
                            });
                            // Give the cancelled call a moment to tear its own work
                            // down (PTY process groups, sleep waits) instead of
                            // detaching it, mirroring the abort path above.
                            let _ =
                                tokio::time::timeout(Duration::from_millis(500), &mut handle).await;
                            break Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                "{reason}"
                            )));
                        }
                        match tokio::time::timeout(join_poll_slice(&watchdog), &mut handle).await {
                            Ok(res) => {
                                break res
                                    .map_err(|e| crate::harness::ToolError::Execution(e.into()))
                                    .and_then(|r| r);
                            }
                            Err(_) => {
                                let mut had_events = false;
                                while let Ok(msg) = status_rx.try_recv() {
                                    renderer.on_event(&Event::Status(msg));
                                    had_events = true;
                                }
                                while let Ok(ev) = event_rx.try_recv() {
                                    if let Event::SteerResponse(ref text) = ev {
                                        ui_transcript
                                            .append(UiRecord::SteerResponse { text: text.clone() });
                                    }
                                    renderer.on_event(&ev);
                                    had_events = true;
                                }
                                if had_events {
                                    let _ = renderer.flush();
                                    watchdog.note_progress();
                                }
                                drain_steer_arbitration_events_with_transcript(
                                    &mut steer_arb_rx,
                                    &mut *renderer,
                                    &mut steer_queue,
                                    &mut steer_abort_requested,
                                    Some(&mut subagents),
                                    Some(&mut ui_transcript),
                                );
                                drain_delegation_events_with_transcript(
                                    manager.as_deref(),
                                    &mut *renderer,
                                    &mut subagents,
                                    Some(&mut ui_transcript),
                                );
                                if let Some(input) = renderer.poll_input() {
                                    if is_abort_command(&input) {
                                        crate::debug_log::log_user_input("command", &input);
                                        renderer.request_user_exit();
                                    } else if is_reset_command(&input) {
                                        handle_reset_command_and_commit(
                                            &mut commit_pump,
                                            &plan,
                                            &mut ctx,
                                            &mut *renderer,
                                            &mut ui_transcript,
                                            &steering_history,
                                        );
                                    } else if !input.trim().is_empty() {
                                        spawn_steer_arbitration(
                                            &client,
                                            harness_stats.clone(),
                                            &goal,
                                            &subagents,
                                            input,
                                            &steer_arb_tx,
                                            &mut *renderer,
                                            Some(Arc::clone(&steering_history)),
                                        );
                                    }
                                }
                                if renderer.user_exit_requested() {
                                    // Mandatory, mid-call.
                                    capture_and_request(
                                        &mut commit_pump,
                                        CommitTrigger::UserAbort,
                                        &ctx,
                                        &*renderer,
                                    );
                                    crate::orchestrator::cancel_all();
                                    break Err(crate::harness::ToolError::Execution(
                                        anyhow::anyhow!("aborted by user"),
                                    ));
                                }
                            }
                        }
                    };

                    let (result_content, is_error) = match result {
                        Ok(res) => (res.content, res.is_error),
                        Err(e) => (format!("ERROR: {e}"), true),
                    };
                    if let Some(agent) = delegated_agent {
                        update_subagent_lifecycle(
                            &mut subagents,
                            agent,
                            delegated_task.clone(),
                            None,
                            false,
                        );
                        let tid = delegated_task.clone().unwrap_or_else(|| agent.to_string());
                        let clean_tid = delegated_task.as_deref().map(clean_task_id);
                        let sa_name = match &clean_tid {
                            Some(t) if !t.is_empty() => format!("{}-{t}", agent.as_str()),
                            _ => agent.as_str().to_string(),
                        };
                        if let Some(sa) =
                            find_subagent_mut(&mut subagents, &sa_name, clean_tid.as_deref())
                            && !is_error
                        {
                            sa.content = result_content.clone();
                            sa.context_tokens = count_text_tokens(&result_content);
                        }
                        if is_error {
                            let reason =
                                crate::ui::helpers::extract_failure_reason(&result_content);
                            renderer.on_event(&Event::Delegation(
                                crate::orchestrator::DelegationEvent::Failed {
                                    agent,
                                    task: delegated_task,
                                    reason: Some(reason.clone()),
                                },
                            ));
                            ui_transcript.append(UiRecord::TaskFailed {
                                task_id: tid,
                                reason: Some(reason),
                            });
                        } else {
                            // Same marker check as the parallel branch: a non-error
                            // result without a completion marker must never tick a
                            // plan box, and a box the orchestrator already ticked
                            // (`AlreadyChecked`) is never reported as a warning.
                            if let Some(ref tid) = delegated_task {
                                let outcome =
                                    marker_gated_check_off(&plan, Some(tid), &result_content);
                                if let Some(warning) = check_off_warning(Some(tid), &outcome) {
                                    surface_plan_warning(
                                        &mut *renderer,
                                        &mut ui_transcript,
                                        warning,
                                    );
                                }
                            }
                            renderer.on_event(&Event::Delegation(
                                crate::orchestrator::DelegationEvent::Completed {
                                    agent,
                                    task: delegated_task,
                                },
                            ));
                            ui_transcript.append(UiRecord::TaskCompleted { task_id: tid });
                        }
                        project_subagent_pane(&mut subagents, &mut *renderer);
                        renderer.set_subagents(subagents.clone());
                    } else {
                        project_subagent_pane(&mut subagents, &mut *renderer);
                        renderer.on_event(&Event::ToolResult(result_content.clone()));
                        ui_transcript.append(UiRecord::ToolResult {
                            display: result_content.clone(),
                        });
                    }
                    renderer.flush()?;

                    drain_delegation_events_with_transcript(
                        manager.as_deref(),
                        &mut *renderer,
                        &mut subagents,
                        Some(&mut ui_transcript),
                    );

                    ctx.append(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: result_content,
                    });
                    capture_and_request(
                        &mut commit_pump,
                        CommitTrigger::ToolResult,
                        &ctx,
                        &*renderer,
                    );

                    // Repeated-failure accounting: an abort/cancel result is not
                    // the tool's own failure, so it must not consume the budget.
                    if is_error
                        && !(renderer.aborted()
                            || renderer.user_exit_requested()
                            || crate::orchestrator::is_globally_cancelled())
                    {
                        match failures.record(&failure_key) {
                            FailureVerdict::Continue { .. } => {}
                            FailureVerdict::Escalate { count } => {
                                let note = failure_escalation_note(&failure_key, count);
                                tracing::warn!(
                                    failures = count,
                                    "repeated failure escalation for {failure_key}"
                                );
                                ctx.append(Message::User {
                                    content: note.clone(),
                                });
                                renderer.on_event(&Event::Status(format!(
                                    "repeated failure: {} failed {count}x (strategy change required)",
                                    failure_label(&failure_key)
                                )));
                                ui_transcript.append(UiRecord::Status { text: note });
                            }
                        }
                    } else if !is_error {
                        failures.clear(&failure_key);
                    }
                    watchdog.note_progress();
                    if renderer.user_exit_requested() {
                        break;
                    }
                }
            }

            // Single repair boundary for the whole round. Every way this round can
            // end early leaves the transcript half-paired: the assistant carrying
            // **all** `tool_calls` was appended before dispatch, and the skipped
            // tail never got a `Message::Tool`. That happens on the parallel join
            // breaks (`bound_stop_reason`, `user_exit_requested`, and the
            // abort/cancel/bound breaks inside the poll loop, where the unjoined
            // tail is only `handle.abort()`ed) and on the sequential
            // `bound_stop_reason` / `renderer.aborted() ||
            // renderer.user_exit_requested()` breaks. All of them fall through to
            // this point *before* the breaks below that end the turn. Repairing
            // here (idempotent; grammar owned by
            // `ContextEngine::ensure_tool_call_pairs`) makes the transcript both
            // sendable on the next turn and valid on disk.
            ensure_pairing_at_boundary(
                "after tool round",
                &mut ctx,
                &mut *renderer,
                &mut ui_transcript,
            );

            // The turn boundary is committed AFTER the pairing repair, so the
            // persisted manager transcript is already pair-valid and the loader
            // never has to normalize a torn round.
            capture_and_request(
                &mut commit_pump,
                CommitTrigger::ManagerTurnBoundary,
                &ctx,
                &*renderer,
            );

            // The round was cut off by an execution bound -> end the turn; the
            // reason is surfaced once, after the turn loop.
            if bound_stop_reason.is_some() {
                break;
            }
            if renderer.user_exit_requested() {
                break;
            }

            if !tool_calls.is_empty() {
                sync_manager_tool_schema(&mut ctx, cfg);
                if ctx.should_compact() {
                    let outcome = ctx.compact();
                    capture_and_request(&mut commit_pump, CommitTrigger::UiEvent, &ctx, &*renderer);
                    renderer.on_event(&Event::TokensIn(ctx.token_count()));
                    surface_compaction_outcome(&mut *renderer, &mut ui_transcript, &outcome);
                } else if ctx.should_advise_rebirth() {
                    ctx.inject_rebirth_advisory();
                    capture_and_request(&mut commit_pump, CommitTrigger::UiEvent, &ctx, &*renderer);
                    renderer.on_event(&Event::Status(
                        "context advisory: rebirth recommended (>= 80% budget)".to_string(),
                    ));
                }
            }

            let current_plan = manager.as_ref().map(|m| m.plan.clone()).unwrap_or_default();
            // `is_complete()` is already fail-closed for an unreadable plan, but the
            // failure itself must stay visible. Read the plan the same way as
            // everywhere else: the "all tasks COMPLETE" notice is only ever
            // injected for a plan that really was read and parsed, and a plan that
            // cannot be read/parsed is surfaced as a warning. `complete` is judged
            // from that single read, so notice and warning cannot disagree because
            // two reads saw two files.
            match read_plan_gate(&current_plan) {
                PlanGate::Unknown { warning } => {
                    surface_plan_warning(&mut *renderer, &mut ui_transcript, warning);
                }
                PlanGate::Read { complete: true, .. } => {
                    ctx.append(Message::User {
                        content: "(SYSTEM NOTICE: All execution plan tasks are now COMPLETE [x]. Do NOT execute any more tools or re-delegate. Deliver your comprehensive final answer/synthesis to the user now.)".to_string(),
                    });
                    capture_and_request(&mut commit_pump, CommitTrigger::UiEvent, &ctx, &*renderer);
                }
                PlanGate::Read { .. } => {}
            }

            // `archive_plan` runs as a tool, i.e. possibly on another thread, and it
            // deletes the bundle with it. The only thread allowed to write the
            // bundle is this one, so the archive is detected here at the round
            // boundary: the plan is committed with the `planArchived` ending first,
            // then the removal is confirmed.
            if plan_archived_during_round(&current_plan, plan_present_at_round_start) {
                tracing::warn!(
                    "execution plan was archived during this round: committing the session with \
                     ending planArchived before the bundle is swept (flow §1.9)"
                );
                if commit_pump.has_committed() {
                    mark_ending_commit(
                        &mut commit_pump,
                        CommitTrigger::PlanArchived,
                        &ctx,
                        &*renderer,
                    );
                    confirm_bundle_swept(
                        &mut commit_pump,
                        CommitTrigger::PlanArchived,
                        &current_plan,
                        &ctx,
                        &*renderer,
                    );
                }
            }
            plan_present_at_round_start = current_plan.plan_path().exists();
        }

        // Make the bound user-visible exactly once per turn, keep the session
        // interactive (same reset pattern the steer-abort redirection uses), and
        // leave an explicit note in the context so the model cannot silently
        // repeat the approach that just hit the bound.
        //
        // The turn-budget exit travels this exact path too — one stop vocabulary,
        // one status channel — and every stop is additionally handed to the caller
        // through [`SessionStopReport`], so a capped finish can never be reported
        // as a clean one.
        if let Some(reason) = bound_stop_reason.take() {
            session_stops.push(reason.clone());
            let text = reason.text();
            let label = reason.label();
            tracing::warn!(
                session_stop_reason = reason.code(),
                "session turn loop stopped by an execution bound: {text}"
            );
            renderer.on_event(&Event::Message(format!("\n[Session bound] {text}\n")));
            renderer.on_event(&Event::Status(format!("{label} (Ready)")));
            ui_transcript.append(UiRecord::Status { text: text.clone() });
            // Surface the teardown *before* the cancellation state is reset, so the
            // report and the cancelled state are observed together (and so no
            // renderer write can be interleaved with a half-reset session).
            renderer.flush()?;
            ctx.append(Message::User {
                content: format!(
                    "(SYSTEM NOTICE: the previous turn was terminated by a session execution bound: {text}. Do not repeat the same failing approach; report the blocker and ask for direction if no alternative strategy is available.)"
                ),
            });
            capture_and_request(&mut commit_pump, CommitTrigger::UiEvent, &ctx, &*renderer);
            if crate::orchestrator::is_globally_cancelled() {
                crate::orchestrator::reset_cancellation();
            }
        }
        drain_steer_arbitration_events_with_transcript(
            &mut steer_arb_rx,
            &mut *renderer,
            &mut steer_queue,
            &mut steer_abort_requested,
            Some(&mut subagents),
            Some(&mut ui_transcript),
        );
        if renderer.user_exit_requested() {
            break;
        }
        if steer_abort_requested || renderer.aborted() {
            if steer_abort_requested {
                // Steer arbitrator requested AbortImmediately / RejectPlan -> reset abort state, reset subagents, and start next turn immediately
                steer_abort_requested = false;
                renderer.clear_abort();
                crate::orchestrator::reset_cancellation();
                // No row is closed or rewritten here: the row disappears when the
                // worker registry entry goes inactive (the cancellation lands on
                // the worker thread), and the projection below folds that in.
                // What is renderer-local — the visible log line — is still written.
                for s in subagents.iter_mut() {
                    if s.is_active {
                        s.logs.push("[aborted by user]".to_string());
                    }
                }
                project_subagent_pane(&mut subagents, &mut *renderer);
                renderer.set_subagents(subagents.clone());
                for steer in steer_queue.drain(..) {
                    ctx.append(Message::User { content: steer });
                }
                capture_and_request(&mut commit_pump, CommitTrigger::UiEvent, &ctx, &*renderer);
                renderer.on_event(&Event::Status(
                    "Steering redirection: aborted current turn, starting next turn with updated context".to_string(),
                ));
                renderer.flush()?;
                continue;
            } else {
                break;
            }
        }

        // If user queued instructions during execution, start next turn immediately without blocking at read_input
        if !steer_queue.is_empty() {
            for steer in steer_queue.drain(..) {
                ctx.append(Message::User { content: steer });
            }
            capture_and_request(
                &mut commit_pump,
                CommitTrigger::ManagerTurnBoundary,
                &ctx,
                &*renderer,
            );
            continue;
        }

        match renderer.read_input() {
            Some(line) => {
                if is_abort_command(&line) {
                    crate::debug_log::log_user_input("command", &line);
                    renderer.request_user_exit();
                    break;
                }
                if is_reset_command(&line) {
                    crate::debug_log::log_user_input("command", &line);
                    handle_reset_command_and_commit(
                        &mut commit_pump,
                        &plan,
                        &mut ctx,
                        &mut *renderer,
                        &mut ui_transcript,
                        &steering_history,
                    );
                    continue;
                }
                if !line.trim().is_empty() {
                    crate::debug_log::log_user_input("interactive_input", &line);
                    ctx.append(Message::User {
                        content: line.clone(),
                    });
                    capture_and_request(&mut commit_pump, CommitTrigger::UiEvent, &ctx, &*renderer);
                    ui_transcript.append(UiRecord::User { text: line });
                } else {
                    continue;
                }
            }
            None => {
                break;
            }
        }
    }

    // The last word of a session names HOW it ended, which is the only thing that
    // lets a later boot tell `CleanTail` from `UserAbort` from
    // `HardKillInferred`. An abort that broke out of the round still has
    // `aborted` / `user_exit_requested` set here, and that is the truth the
    // manifest must carry — a session the user stopped is not a session that
    // finished. Mandatory either way: the throttle never skips the last word, and
    // it runs BEFORE `shutdown()` because the UI capture needs a live renderer.
    let tail_trigger = if renderer.aborted() || renderer.user_exit_requested() {
        CommitTrigger::UserAbort
    } else {
        CommitTrigger::CleanTail
    };
    capture_and_request(&mut commit_pump, tail_trigger, &ctx, &*renderer);

    renderer.on_event(&Event::Done);
    renderer.flush().ok();
    renderer.shutdown();
    Ok(SessionStopReport {
        stops: session_stops,
    })
}

// Execution-bound helpers

/// Stable identity for a repeated unit of work inside one session.
///
/// Delegations are keyed by the plan task id they belong to, so re-delegating
/// the same task under a slightly different prompt still counts against the
/// budget. Everything else is keyed by tool name plus a truncated argument
/// signature, which is what makes "the same failing call over and over"
/// detectable without holding on to unbounded history.
fn failure_key_for(name: &str, args: &serde_json::Value, task_id: Option<&str>) -> String {
    if let Some(raw) = task_id {
        let task = clean_task_id(raw);
        if !task.is_empty() {
            return format!("task:{task}");
        }
    }
    let signature = crate::text_util::truncate_with_ellipsis(&args.to_string(), 120);
    format!("tool:{name}:{signature}")
}

/// User-facing rendering of a [`failure_key_for`] key.
fn failure_label(key: &str) -> String {
    match key.split_once(':') {
        Some(("task", task)) => format!("task {task}"),
        Some(("tool", rest)) => rest.split(':').next().unwrap_or(rest).to_string(),
        _ => key.to_string(),
    }
}

/// Escalation injected the moment a task/tool call crosses the failure budget:
/// it forbids an unchanged retry and names concrete strategies instead.
fn failure_escalation_note(key: &str, count: u32) -> String {
    format!(
        "(SYSTEM NOTICE: {} has failed {count} times in this session. Do NOT retry it unchanged: narrow the task, gather the missing context, split it into smaller tasks, or report it as a blocker and replan. Blindly repeating the same call is itself a failure.)",
        failure_label(key)
    )
}

/// What the loop reports back as the tool error once the budget is spent.
fn failure_refusal_message(key: &str, count: u32) -> String {
    format!(
        "ERROR: failure budget exhausted - {} has failed {count} times this session, so the session refuses to run it unchanged. Change strategy (different arguments, smaller scope, missing context gathered) or report the blocker to the user.",
        failure_label(key)
    )
}

// Plan check-off + plan-state helpers (shared by both live loops)

/// What the session is allowed to conclude about the on-disk execution plan.
///
/// [`crate::manager::phase::Plan::pending_tasks`] folds a read failure into an
/// empty `Vec`, and an empty list is exactly what the auto-nudge and the
/// completion notice read as "nothing left to do" — so a plan that cannot be
/// read (or parsed into task lines) looks like a *finished* plan. This type
/// keeps the two apart: `Unknown` must be surfaced to the user and must never
/// be taken as completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanGate {
    /// The plan was read and parsed by the single grammar owner
    /// ([`crate::plan_parse`]); `pending` holds its unchecked task ids and
    /// `complete` mirrors [`crate::manager::phase::Plan::is_complete`] computed
    /// from that **same** read (no second lock acquisition, no second, possibly
    /// different, read). An empty `pending` list here genuinely means "nothing
    /// pending" (or no plan file at all).
    Read {
        pending: Vec<String>,
        complete: bool,
    },
    /// The plan file exists but could not be read, or holds no parseable task
    /// line. `warning` is the user-visible text; the plan state is **unknown**,
    /// so the caller must show it and must not treat the plan as complete.
    Unknown { warning: String },
}

/// Read the plan the way the UI must judge it: fail-closed and loud.
///
/// Every branch is decided by the plan/grammar owners (`Plan::read`,
/// [`crate::plan_parse`]) — the session invents no plan grammar of its own.
pub fn read_plan_gate(plan: &crate::manager::phase::Plan) -> PlanGate {
    let content = match plan.read() {
        Ok(Some(content)) => content,
        // No plan file on disk: genuinely nothing pending, kept apart from a plan
        // that exists but cannot be read.
        Ok(None) => {
            return PlanGate::Read {
                pending: Vec::new(),
                complete: false,
            };
        }
        Err(e) => {
            return PlanGate::Unknown {
                warning: format!(
                    "execution plan at {} could not be read: {e:#} — plan state is UNKNOWN, \
                     NOT 'all tasks done'",
                    plan.plan_path().display()
                ),
            };
        }
    };
    if content.trim().is_empty() {
        return PlanGate::Read {
            pending: Vec::new(),
            complete: false,
        };
    }
    // A non-empty plan that yields no task ids at all was not parsed: report it
    // instead of letting "no pending ids" masquerade as a completed plan.
    if crate::plan_parse::all_task_ids(&content).is_empty() {
        return PlanGate::Unknown {
            warning: format!(
                "execution plan at {} could not be parsed: it holds text but no \
                 `- [ ] [t-xxx]` task lines — plan state is UNKNOWN, NOT 'all tasks done'",
                plan.plan_path().display()
            ),
        };
    }
    PlanGate::Read {
        pending: crate::plan_parse::unchecked_task_ids(&content),
        // Same two conditions as `Plan::is_complete_unlocked`, evaluated on the
        // content that was just read.
        complete: !crate::plan_parse::has_unchecked_box(&content)
            && crate::plan_parse::has_checked_box(&content),
    }
}

/// Show a plan-integrity problem through the channel the session already uses
/// for warnings: a status event on the renderer plus a `Status` record in the UI
/// transcript (the exact pair the repeated-failure escalation emits), so the
/// user sees it live and after a restart.
fn surface_plan_warning(
    renderer: &mut dyn Renderer,
    ui_transcript: &mut UiTranscript,
    warning: String,
) {
    tracing::warn!("{warning}");
    renderer.on_event(&Event::Status(warning.clone()));
    ui_transcript.append(UiRecord::Status { text: warning });
}

/// Wall-clock bound of one iteration of a tool-round join loop.
///
/// The `20 ms` slice is capped by [`TurnWatchdog::time_to_idle_limit`] — the
/// bound the loops honour is the one the watchdog actually advertises. The
/// `1 ms` floor keeps the loop from spinning when the remaining budget is
/// (sub-millisecond but) non-zero.
fn join_poll_slice(watchdog: &TurnWatchdog) -> Duration {
    const TOOL_JOIN_POLL_SLICE: Duration = Duration::from_millis(20);
    TOOL_JOIN_POLL_SLICE
        .min(watchdog.time_to_idle_limit())
        .max(Duration::from_millis(1))
}

/// Close the assistant `tool_calls` ↔ `Tool` pairing invariant on the live
/// transcript at a boundary that can end a turn early, persist it, and surface
/// it through the session's status channel — but only when the repair actually
/// changed something ([`ContextEngine::ensure_tool_call_pairs`] is idempotent).
/// Every early exit (execution bound, renderer abort, steering abort, user exit)
/// can leave an assistant `tool_calls` entry with no `Message::Tool` result,
/// which the next request would resend as a provider 400. `site` names the
/// boundary in the user-visible status line.
fn ensure_pairing_at_boundary(
    site: &str,
    ctx: &mut ContextEngine,
    renderer: &mut dyn Renderer,
    ui_transcript: &mut UiTranscript,
) {
    if !ctx.ensure_tool_call_pairs() {
        return;
    }
    // The repaired transcript is what the next request is built from, and the
    // capture below records it in memory so a later rehydration installs the
    // repaired state.
    crate::session::manager_capture::capture_into_session_state(ctx);
    let note = format!(
        "tool-call pairing repaired ({site}): missing tool results synthesized as `{ABORTED_TOOL_RESULT}`"
    );
    tracing::warn!("{note}");
    renderer.on_event(&Event::Status(note.clone()));
    ui_transcript.append(UiRecord::Status { text: note });
}

/// Report the outcome of an automatic compaction.
///
/// [`CompactionOutcome`] must never be discarded: `"context compacted"` would be
/// a lie for [`CompactionOutcome::TargetUnreachable`], where nothing reached the
/// target and the transcript is still over budget. Surfacing goes through the
/// session's one status channel (renderer `Status` event + matching `Status`
/// record in the UI transcript + `tracing::warn!`), the same channel
/// [`surface_plan_warning`] uses. Control flow is deliberately unchanged — the
/// session keeps running — but it must never claim a compaction that did not
/// happen.
fn surface_compaction_outcome(
    renderer: &mut dyn Renderer,
    ui_transcript: &mut UiTranscript,
    outcome: &CompactionOutcome,
) {
    if outcome.succeeded() {
        renderer.on_event(&Event::Status("context compacted".to_string()));
        return;
    }
    let warning = format!(
        "(SYSTEM WARNING: automatic context compaction could not reach its target — removed {} messages / {} tokens and the transcript is still {} tokens against a {}-token target, i.e. above the compaction threshold. The pinned prefix alone exceeds the target; a rebirth or a shorter goal is required, the transcript is being sent over budget.)",
        outcome.messages_removed(),
        outcome.tokens_reclaimed(),
        outcome.final_tokens(),
        outcome.target(),
    );
    tracing::warn!("{warning}");
    renderer.on_event(&Event::Status(warning.clone()));
    ui_transcript.append(UiRecord::Status { text: warning });
}

/// Why a plan box did — or did not — flip, as decided by the single check-off
/// entry point [`crate::manager::phase::Plan::check_plan_on_deliverable`].
///
/// A plain `bool` cannot tell the UI anything it is allowed to say out loud:
/// `false` conflates "the box was already ticked by the orchestrator's own
/// marker-checked check-off" (harmless — `handle_delegate_task` runs
/// `apply_check_off` before the session ever sees the deliverable) with "the
/// deliverable carried no completion marker" (a real plan-integrity warning).
/// Printing a warning for the first case would be a lie, so the two are
/// separated here and only the genuinely suspicious outcomes are surfaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOffOutcome {
    /// A pending plan box was flipped to `[x]`.
    Flipped,
    /// The task line was already `[x]` before this call — the orchestrator's
    /// marker-checked check-off already recorded the completion.
    AlreadyChecked,
    /// No plan file on disk, or the bound id has no task line in it (a
    /// delegation that was never bound to a plan task).
    NotInPlan,
    /// The plan file is readable but the grammar owner finds **no task line at
    /// all** in it — the check-off cannot be located, and the plan state is
    /// unknown (never "all tasks done"). `plan_path` names the file to fix.
    Unparseable { plan_path: String },
    /// The deliverable carries no terminal marker at all: `is_error == false`
    /// was the only completion evidence offered.
    NoMarker,
    /// The deliverable carries a terminal verdict that is **not** a completion
    /// (`verdict` names it, e.g. `crate::markers::MARKER_FAILED`).
    NotComplete { verdict: String },
    /// The completion marker names a different task than the delegation bound —
    /// checking the bound box would tick another task's line.
    IdMismatch { bound: String, marker: String },
    /// The plan could not be read or written (IO).
    Io { detail: String },
    /// The check-off was refused by the owner for a documented fail-closed rule
    /// (e.g. a body-derived id that the deliverable never mentions as a token).
    Refused { detail: String },
}

/// The single marker-checked check-off used by **both** live session loops when
/// a delegation comes back without an error.
///
/// It routes straight into [`crate::manager::phase::Plan::check_plan_on_deliverable`]
/// — the same entry point the orchestrator's `apply_check_off` uses — so a
/// deliverable that carries no terminal completion marker can never tick a plan
/// box, and `is_error == false` alone is never treated as completion. The marker
/// grammar and the task-id binding stay owned by [`crate::markers`] and
/// [`crate::plan_parse`]; the UI adds no parser of its own and makes no decision
/// of its own.
///
/// The one extra read below is **diagnostic only**: it captures the plan state
/// before the call so the outcome can be classified honestly (already checked vs
/// not in the plan). The authority to flip a box stays entirely in
/// `manager/phase.rs`.
#[must_use]
pub fn marker_gated_check_off(
    plan: &crate::manager::phase::Plan,
    task_id: Option<&str>,
    deliverable: &str,
) -> CheckOffOutcome {
    use crate::markers::MissionMarker;

    let bound = task_id
        .map(crate::task_id::normalize_task_id_ref)
        .filter(|t| !t.is_empty());
    // Diagnostic read before the call: does the plan hold a line for this id,
    // and was it already `[x]` when we got here?
    let content = match plan.read() {
        Ok(content) => content,
        Err(e) => {
            return CheckOffOutcome::Io {
                detail: format!("{e:#}"),
            };
        }
    };
    let content: &str = content.as_deref().unwrap_or("");
    let line_present =
        bound.is_some_and(|tid| crate::plan_parse::find_task_line(content, tid).is_some());
    let already_checked = bound.is_some_and(|tid| crate::plan_parse::is_checked_task(content, tid));

    match plan.check_plan_on_deliverable(None, task_id, deliverable) {
        Ok(true) => return CheckOffOutcome::Flipped,
        Err(e) => {
            return CheckOffOutcome::Io {
                detail: format!("{e:#}"),
            };
        }
        Ok(false) => {}
    }
    // `Ok(false)`: classify why, reusing the owners' parsers.
    if already_checked {
        return CheckOffOutcome::AlreadyChecked;
    }
    if !content.trim().is_empty() && crate::plan_parse::all_task_ids(content).is_empty() {
        return CheckOffOutcome::Unparseable {
            plan_path: plan.plan_path().display().to_string(),
        };
    }
    if !line_present {
        return CheckOffOutcome::NotInPlan;
    }
    match MissionMarker::resolve(None, deliverable) {
        None => CheckOffOutcome::NoMarker,
        Some(MissionMarker::Complete { task_id: marker_id }) => {
            if let (Some(bound), Some(marker_id)) = (bound, marker_id.as_deref())
                && !crate::plan_parse::task_id_eq(bound, marker_id)
            {
                return CheckOffOutcome::IdMismatch {
                    bound: bound.to_owned(),
                    marker: marker_id.to_owned(),
                };
            }
            CheckOffOutcome::Refused {
                detail: "the owner declined this check-off; see the check_plan_on_deliverable \
                         warning for the fail-closed rule that applied"
                    .to_owned(),
            }
        }
        Some(marker) => CheckOffOutcome::NotComplete {
            verdict: match marker {
                MissionMarker::Replan { .. } => crate::markers::MARKER_REPLAN.to_owned(),
                _ => crate::markers::MARKER_FAILED.to_owned(),
            },
        },
    }
}

/// The user-visible warning for a [`CheckOffOutcome`], or `None` when the
/// outcome needs no announcement (a flip, an idempotent re-check, or a
/// delegation that was never bound to a plan task).
///
/// Marker words come only from [`crate::markers`].
#[must_use]
pub fn check_off_warning(task_id: Option<&str>, outcome: &CheckOffOutcome) -> Option<String> {
    let tid = task_id.unwrap_or("unbound");
    match outcome {
        CheckOffOutcome::Flipped | CheckOffOutcome::AlreadyChecked | CheckOffOutcome::NotInPlan => {
            None
        }
        CheckOffOutcome::Unparseable { plan_path } => Some(format!(
            "the execution plan at {plan_path} holds no parseable `- [ ] [t-id]` task line, so the \
             check-off for [{tid}] could not be applied — plan state is UNKNOWN, \
             NOT 'all tasks done'"
        )),
        CheckOffOutcome::NoMarker => Some(format!(
            "task {tid} returned without an error, but its deliverable carries no {} marker — \
             the plan box stays unchecked",
            crate::markers::MARKER_COMPLETE
        )),
        CheckOffOutcome::NotComplete { verdict } => Some(format!(
            "task {tid} returned a {verdict} verdict — its plan box stays unchecked and the task \
             must be re-delegated or replanned",
        )),
        CheckOffOutcome::IdMismatch { bound, marker } => Some(format!(
            "task {bound} came back with a {marker_name} marker for [{marker}] — a completion for \
             one task can never tick another task's box, so [{bound}] stays unchecked",
            marker_name = crate::markers::MARKER_COMPLETE
        )),
        CheckOffOutcome::Io { detail } => Some(format!(
            "plan check-off for [{tid}] failed: {detail} — plan state is UNKNOWN, the box was \
             NOT checked"
        )),
        CheckOffOutcome::Refused { detail } => Some(format!(
            "plan check-off for [{tid}] was refused: {detail} — the box stays unchecked"
        )),
    }
}

#[cfg(test)]
mod budget_tests {
    use super::{build_manager_context, sync_manager_tool_schema};
    use crate::config::Config;
    use crate::llm::stream::{StreamConfig, build_request};
    use crate::manager::context::{manager_wire_tools, tools_tokens};
    use crate::types::ToolDef;

    /// `ToolDef` has no `PartialEq`; the wire form is the only identity that
    /// matters for a schema charge, so the JSON is the comparison key.
    fn wire_signature(tools: &[ToolDef]) -> String {
        serde_json::to_string(tools).expect("tool definitions serialize")
    }

    #[test]
    fn charged_manager_schema_is_exactly_the_wire_schema() {
        let cfg = Config::default();
        let request = build_request(&StreamConfig::from_config(&cfg), Vec::new());
        let wire = request
            .tools
            .expect("the Manager always advertises its tool list");
        let charged = manager_wire_tools(&cfg.orchestration.mcp_servers);
        assert_eq!(
            wire_signature(&wire),
            wire_signature(&charged),
            "the schema the budget charges must equal the schema `build_request` puts on the wire"
        );
        assert!(
            tools_tokens(&charged) > 0,
            "the Manager advertised list is never schema-free"
        );
    }

    #[test]
    fn build_manager_context_charges_the_list_and_resync_is_idempotent() {
        let cfg = Config::default();
        let exact = tools_tokens(&manager_wire_tools(&cfg.orchestration.mcp_servers));
        assert!(exact > 0);

        let mut ctx = build_manager_context(&cfg, "You are the manager.".to_string());
        assert_eq!(ctx.tool_schema_tokens(), exact);
        assert!(ctx.request_token_count() > ctx.token_count());

        // A lost charge (stale MCP boot, a caller that wiped the list) is healed by
        // the per-decision re-declaration, and re-declaring twice is a no-op.
        ctx.set_tools(&[]);
        assert_eq!(ctx.tool_schema_tokens(), 0);
        assert_eq!(sync_manager_tool_schema(&mut ctx, &cfg), exact);
        assert_eq!(sync_manager_tool_schema(&mut ctx, &cfg), exact);
        assert_eq!(ctx.tool_schema_tokens(), exact);
    }
}

#[cfg(test)]
mod restored_subagent_row_tests {
    use super::{seed_restored_subagent_rows, subagent_detail_from_panel};
    use crate::orchestrator::workers;
    use crate::session::format::{SubagentPanel, TimePoint};

    fn panel(name: &str) -> SubagentPanel {
        SubagentPanel {
            name: name.to_string(),
            task_id: None,
            prompt: format!("brief for {name}"),
            logs: vec![format!("log line for {name}")],
            thinking: String::new(),
            content: String::new(),
            is_active: true,
            context_tokens: 7,
            worked_duration_secs: 1.5,
            started_at: Some(TimePoint::now()),
            last_activity_at: Some(TimePoint::now()),
            turn_thinking: None,
            is_thinking: None,
        }
    }

    /// A restored pane and a restored registry must name the same object: the
    /// journaled panels keep their stored order and length, and every registry
    /// key reported as interrupted
    /// while open joins the list under its **effective** key — including the
    /// `{agent}-wN` / `{base}#N` spellings a stream tag can never produce.
    #[test]
    fn restored_rows_keep_panel_order_and_join_in_on_the_effective_registry_key() {
        // The same seam every other test that registers into the process-global
        // `WORKERS` table takes (see `orchestrator::workers::TEST_WORKERS_MUTEX`).
        let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let first = workers::register_active_worker(
            Some("t-9101".to_string()),
            "coder".to_string(),
            "brief one".to_string(),
        );
        let second = workers::register_active_worker(
            Some("t-9101".to_string()),
            "coder".to_string(),
            "brief two".to_string(),
        );
        let (key_a, key_b) = (first.0.clone(), second.0.clone());

        let panels = vec![panel("planner-t-9100"), panel(&key_a)];
        let interrupted = vec![
            key_a.clone(),
            key_b.clone(),
            "coder-t-9199".to_string(), // never registered: no row may be invented
        ];
        let rows = seed_restored_subagent_rows(&panels, &interrupted);

        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["planner-t-9100", key_a.as_str(), key_b.as_str()],
            "stored panels first in stored order, then each restored key exactly once"
        );
        let joined = rows
            .iter()
            .find(|row| row.name == key_b)
            .expect("key B row");
        assert!(joined.is_active, "an interrupted worker comes back running");
        assert!(
            joined.started_at.is_some(),
            "its clock comes from the registry"
        );
        assert_eq!(joined.prompt, "brief two", "and so does its brief");

        for key in [&key_a, &key_b] {
            workers::remove_worker_entry_for_test(key);
        }
    }

    /// Whether a mid-flight steer gets its own arbitration round is asked of the
    /// **worker registry only** — the loop's old
    /// `|| subagents.iter().any(|s| s.is_active)` term is gone (see the guard in
    /// `run_session_with_bounds_report`). That is sound because the loop
    /// re-projects the pane on the very edge it reads input on, which pins two
    /// things this test guards:
    ///
    /// * every active row has an **active registry entry** behind it, so the pane
    ///   term could never add a case `has_active_workers()` missed while work was
    ///   really running;
    /// * a journaled panel restored with `is_active: true` in front of a task
    ///   that died with the previous process is not a row at all, so it can no
    ///   longer re-open arbitration for work that does not exist.
    #[test]
    fn projected_rows_never_claim_liveness_the_registry_denies() {
        // Process-global `WORKERS` table: same seam as every other test that
        // registers into it (`orchestrator::workers::TEST_WORKERS_MUTEX`).
        let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let guard = workers::register_active_worker(
            Some("t-9200".to_string()),
            "coder".to_string(),
            "brief under test".to_string(),
        );
        let key = guard.0.clone();

        // Exactly the row shape the steering guard can be handed: one live
        // worker, plus a journaled panel for a delegation this process holds no
        // registry entry for (both restored with `is_active: true`).
        let mut rows = vec![
            subagent_detail_from_panel(&panel("planner-t-9299")),
            subagent_detail_from_panel(&panel(&key)),
        ];
        assert!(
            rows.iter().any(|row| row.is_active),
            "before the projection the pane looks busy"
        );

        // `project_subagent_pane` is this call plus the renderer push, so the
        // projection alone decides what the guard is allowed to see.
        crate::ui::helpers::project_subagent_rows(&mut rows);
        assert!(
            rows.iter().all(|row| row.name != "planner-t-9299"),
            "a panel with no registry entry behind it never survives as a live row"
        );
        assert!(
            rows.iter().any(|row| row.name == key && row.is_active),
            "the live worker keeps its row under its effective registry key"
        );
        assert!(
            !rows.iter().any(|row| row.is_active) || workers::has_active_workers(),
            "an active row always implies an active entry — the removed term was a subset of registry truth"
        );

        // The stale half: once the entry stops being active, nothing in the row
        // set may still look busy.
        drop(guard);
        crate::ui::helpers::project_subagent_rows(&mut rows);
        let entries = crate::orchestrator::worker_entries();
        for row in rows.iter().filter(|row| row.is_active) {
            assert!(
                entries
                    .iter()
                    .any(|entry| entry.key == row.name && entry.is_active),
                "row {} is drawn active with no active registry entry behind it",
                row.name
            );
        }
        assert!(
            rows.iter()
                .all(|row| row.name != key.as_str() || !row.is_active),
            "a finished worker never leaves an active row to re-open arbitration"
        );

        workers::remove_worker_entry_for_test(&key);
    }
}
