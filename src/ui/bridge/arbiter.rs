//! Shared steering-arbitration engine for both bridge hosts.
//!
//! `RendererSink::on_pause` (stream paused for mid-flight steering) and
//! `spawn_steer_arbitration` (background arbitration task) used to carry their own copy of the
//! whole arbitration round machine: context snapshot, wake-up SYSTEM NOTICE, preemption handling,
//! decision branching, sleep/re-evaluation loop, delegated-subtask execution and the steering
//! history summary. This module owns that logic once; the two hosts are thin adapters that only
//! supply their differing inputs and surface the progress through their own channel
//! (renderer events vs. the [`super::drain`] event channel).
//!
//! Deliberately *not* unified: the per-channel wording of the sleep notices and the per-round
//! notion of "is work active" differ between the two hosts. Both differences are kept as they
//! were and are expressed explicitly (see [`SleepNotice::status_text`] /
//! [`SleepNotice::delta_text`] and the `ArbitrationChannel` snapshot methods).

use super::action;
use crate::agents::{Agent, Deliverable, MissionMarker};
use crate::harness::HarnessStats;
use crate::llm::{ChatClient, PauseAction};
use crate::manager::r#loop::{DeadlineKind, TurnWatchdog};
use crate::orchestrator::notice::{INBOX_CAPACITY, SteerNotice};
use crate::orchestrator::steer::{
    SteerContext, arbitrate_steer_context_stream, execute_steer_subtask,
};
use crate::orchestrator::{
    SharedSteeringHistory, SteerDecision, cancel_active_worker, cancel_all,
    extract_tasks_to_delegate, format_steering_history, normalize_steer_decision,
    preempt_conflicting_stream,
};
use crate::tool_args::{SLEEP_DEFAULT_SECS, SLEEP_MIN_SECS};
use crate::ui::helpers::format_plan_progress_summary;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Upper bound on arbitration rounds for a single steering instruction: every `Sleep` decision
/// re-evaluates the situation in a fresh round.
pub(crate) const MAX_ARBITRATION_ROUNDS: usize = 5;

/// Maximum number of **chained sleep extensions** one steering instruction may obtain inside a
/// single arbitration (sleep → wake → re-evaluate → sleep again counts as one extension per
/// sleep).
///
/// Without this bound a arbitrator that keeps answering `Sleep` extends its own sleep budget
/// indefinitely and stalls the fix loop: each extension is up to 300 s
/// ([`arbitrate_steering`] clamps a single sleep to that), so the round bound alone still allows
/// hours of sleeping. Reaching the cap is deterministic and observable — see [`SleepStop`] and
/// [`SleepNotice::Capped`] — never a silent `continue`.
pub(crate) const MAX_CHAINED_SLEEP_EXTENSIONS: usize = 3;

/// Cumulative wall-clock bound on all sleeps of one chained arbitration, in seconds.
///
/// Enforced with the crate's existing [`TurnWatchdog`] / [`DeadlineKind`] machinery (see
/// [`SleepBudget`]) instead of inventing a second timer: every requested sleep is clamped to the
/// remaining budget and the chain stops once the budget is spent. "Spent" includes a remainder
/// too small to cover the sleep floor — a sub-second remainder is refused, never slept as zero
/// seconds (see [`SleepBudget::allow_at`], gate t-076).
pub(crate) const MAX_CHAINED_SLEEP_SECONDS: u64 = 300;

/// Wall-clock bound of a single arbitrator sleep, kept from the original per-decision clamp.
pub(crate) const MAX_SINGLE_SLEEP_SECONDS: u64 = 300;

/// How long **one** arbitrator sleep actually is, given the `sleep_seconds` a
/// `Sleep` decision asked for (gate t-070).
///
/// * **Lower bound — owned by the sleep tool.** A `Sleep` with no duration
///   resolves to [`crate::tool_args::SLEEP_DEFAULT_SECS`], and a sub-minimum
///   request (`0`, or anything below [`crate::tool_args::SLEEP_MIN_SECS`]) is
///   clamped **up** to that minimum. Both numbers are the `sleep`-argument
///   owner's constants ([`crate::tool_args`]); they are deliberately not
///   re-typed here. Before this floor a steered `sleep_seconds: 0` produced a
///   zero-length wait: the loop re-asked the arbitrator immediately — a hot loop
///   against the LLM — while both hosts announced "sleeping for 0s" for a sleep
///   that never happened.
/// * **Upper bound — owned by the arbitrator.** [`MAX_SINGLE_SLEEP_SECONDS`] and
///   the chained [`MAX_CHAINED_SLEEP_SECONDS`] budget are the arbitrator's own
///   knobs and are **deliberately not coupled** to
///   [`crate::tool_args::SLEEP_MAX_SECS`]: the tool ceiling bounds what the
///   `sleep` *tool* may wait, these bound how long **one steering instruction**
///   may keep the arbitration loop asleep. Wiring them together would let a
///   change to the tool ceiling silently rewrite the arbitration budget (and the
///   other way round), so the separation is kept — and asserted — here (see
///   `arbitrator_upper_budget_is_not_the_sleep_tool_ceiling`).
///
/// The floor is the **lower** bound of the sleep itself; the chained budget may clamp a
/// request down afterwards but must never undo it — a remaining budget smaller than
/// [`SLEEP_MIN_SECS`] is refused rather than slept as zero seconds (see
/// [`SleepBudget::allow_at`], gate t-076).
pub(crate) fn clamped_sleep_secs(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(SLEEP_DEFAULT_SECS)
        .clamp(SLEEP_MIN_SECS, MAX_SINGLE_SLEEP_SECONDS)
}

/// Why chained sleep was torn down — the typed, observable outcome of hitting a documented cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SleepStop {
    /// The chain already spent [`MAX_CHAINED_SLEEP_EXTENSIONS`] extensions.
    Extensions { extensions: usize, limit: usize },
    /// The cumulative [`MAX_CHAINED_SLEEP_SECONDS`] budget is spent; `kind` is the
    /// [`DeadlineKind`] the shared [`TurnWatchdog`] reported.
    Deadline {
        kind: DeadlineKind,
        slept_secs: u64,
        limit_secs: u64,
    },
}

impl SleepStop {
    /// Single-source wording of the cap, used by the log line and by the hosts.
    pub(crate) fn reason_text(self) -> String {
        match self {
            SleepStop::Extensions { extensions, limit } => format!(
                "chained sleep stopped after {extensions} sleep extension(s) — the documented limit is {limit} per steering instruction"
            ),
            SleepStop::Deadline {
                kind,
                slept_secs,
                limit_secs,
            } => format!(
                "chained sleep stopped at the {kind:?} deadline — {slept_secs}s slept of a {limit_secs}s cumulative budget"
            ),
        }
    }
}

/// The extension + wall-clock budget of chained sleep inside one arbitration.
///
/// The wall-clock half reuses [`TurnWatchdog`] (absolute hard cap) and has an injectable clock
/// ([`SleepBudget::allow_at`]), so the bound is auditable without waiting for real seconds. Sleeps
/// are accounted in **requested** seconds, which keeps the cap deterministic regardless of how
/// long a sleep actually took (a cancelled sleep still counts as an extension).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SleepBudget {
    watchdog: TurnWatchdog,
    extensions: usize,
    slept_secs: u64,
}

impl SleepBudget {
    /// Arm the budget anchored at an explicit instant (test seam, mirroring [`TurnWatchdog::at`]).
    pub(crate) fn new_at(start: Instant) -> Self {
        let limit = Duration::from_secs(MAX_CHAINED_SLEEP_SECONDS);
        Self {
            watchdog: TurnWatchdog::at(start, limit, limit),
            extensions: 0,
            slept_secs: 0,
        }
    }

    pub(crate) fn extensions(self) -> usize {
        self.extensions
    }

    pub(crate) fn slept_secs(self) -> u64 {
        self.slept_secs
    }

    /// May the chain sleep again, and for how long?
    ///
    /// `Ok(secs)` is the requested sleep clamped to the remaining cumulative budget and
    /// floored at [`SLEEP_MIN_SECS`]; `Err(stop)` names the documented cap that ended the
    /// chain.
    ///
    /// **A remainder below the floor is an exhausted budget, not a zero-length sleep**
    /// (gate t-076). A remainder of e.g. 500 ms is not [`Duration::is_zero`], but it carries
    /// zero whole seconds, so returning `Ok(remaining.as_secs())` handed back `Ok(0)`: the
    /// round machine then accounted `note_slept(0)`, slept nothing and re-asked the
    /// arbitrator immediately — exactly the hot-loop [`clamped_sleep_secs`] was added to
    /// remove, re-created because that floor is applied *before* this clamp. The floor is
    /// therefore applied **after** the budget clamp, and a budget that cannot cover it is
    /// refused through the same typed [`SleepStop::Deadline`] outcome a fully spent budget
    /// reports: the caller tells "budget exhausted" apart from "slept N seconds", no sleep
    /// is performed and nothing is accounted. Rounding a sub-second remainder **up** to the
    /// floor is not an option either — that would overshoot the watchdog hard limit
    /// ([`MAX_CHAINED_SLEEP_SECONDS`]).
    pub(crate) fn allow_at(self, requested_secs: u64, now: Instant) -> Result<u64, SleepStop> {
        if self.extensions >= MAX_CHAINED_SLEEP_EXTENSIONS {
            return Err(SleepStop::Extensions {
                extensions: self.extensions,
                limit: MAX_CHAINED_SLEEP_EXTENSIONS,
            });
        }
        let remaining = self.watchdog.time_to_hard_limit_at(now);
        if remaining < Duration::from_secs(SLEEP_MIN_SECS) {
            return Err(SleepStop::Deadline {
                kind: self
                    .watchdog
                    .expired_at(now)
                    .unwrap_or(DeadlineKind::HardCap),
                slept_secs: self.slept_secs,
                limit_secs: MAX_CHAINED_SLEEP_SECONDS,
            });
        }
        // Clamp first, floor second: the guard above guarantees
        // `remaining.as_secs() >= SLEEP_MIN_SECS`, so the floor can never push the sleep
        // past the hard limit, and a returned duration is never sub-minimum.
        Ok(requested_secs.min(remaining.as_secs()).max(SLEEP_MIN_SECS))
    }

    /// Account one completed sleep extension against the budget.
    pub(crate) fn note_slept(&mut self, slept_secs: u64) {
        self.extensions += 1;
        self.slept_secs = self.slept_secs.saturating_add(slept_secs);
    }

    /// Snapshot of what the chain did, for the host.
    pub(crate) fn chain(self) -> SleepChain {
        SleepChain {
            extensions: self.extensions,
            slept_secs: self.slept_secs,
            stop: None,
        }
    }
}

/// What chained sleep did during one arbitration, as reported to the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SleepChain {
    /// Number of sleep extensions the arbitrator actually took.
    pub(crate) extensions: usize,
    /// Cumulative sleep seconds accounted against [`MAX_CHAINED_SLEEP_SECONDS`].
    pub(crate) slept_secs: u64,
    /// Set when a documented cap ended the chain.
    pub(crate) stop: Option<SleepStop>,
}

impl SleepChain {
    /// Sleep the **host** may still add on top of the arbitration's own sleeps, clamped by the
    /// same documented budget: `None` means the budget is spent and the host must not sleep again
    /// (it says so instead — see [`SleepNotice::Capped`]).
    ///
    /// Same refuse-vs-floor policy as [`SleepBudget::allow_at`] (gate t-076), so the two halves
    /// of one arbitration can never disagree about whether a sleep is possible at all:
    ///
    /// * a remaining budget **below [`SLEEP_MIN_SECS`]** is refused with `None` — the host is
    ///   told not to sleep rather than handed a sub-minimum (zero-length) duration, which is
    ///   the hot-loop the floor exists to prevent (`Some(0)` announced a sleep that never
    ///   happened);
    /// * otherwise the request is clamped to what is left and only then floored, so a returned
    ///   duration is never sub-minimum and never overshoots [`MAX_CHAINED_SLEEP_SECONDS`].
    pub(crate) fn host_sleep_secs(&self, requested_secs: u64) -> Option<u64> {
        if self.stop.is_some() || self.extensions >= MAX_CHAINED_SLEEP_EXTENSIONS {
            return None;
        }
        let remaining = MAX_CHAINED_SLEEP_SECONDS.saturating_sub(self.slept_secs);
        if remaining < SLEEP_MIN_SECS {
            return None;
        }
        Some(requested_secs.min(remaining).max(SLEEP_MIN_SECS))
    }
}

/// Whether the arbitration's last decision is a `Sleep` that the round machine will **not**
/// re-evaluate (the loop already ended). Such a steering instruction has no action attached to it
/// and must be carried to the next seam instead of being dropped — see [`durable_steer_reason`].
///
/// The arbitrator spells `Sleep` either as the top-level decision or as a per-subtask action, so
/// both spellings are routed through the shared normalizer (`super::action`) and decide the
/// carry-over the same way — one vocabulary, one rule, in both layers.
pub(crate) fn is_terminal_sleep(decision: Option<&SteerDecision>) -> bool {
    decision.as_ref().is_some_and(|d| {
        normalize_steer_decision(Some(d.decision.as_str())) == "Sleep"
            || action::any(Some(d), action::ACTION_SLEEP)
    })
}

/// Why a steering instruction is being carried over to the next seam (deterministic wording,
/// naming the sleep chain and the cap that ended it when one was hit).
pub(crate) fn durable_steer_reason(sleep: &SleepChain) -> String {
    let cap = sleep
        .stop
        .map(|stop| format!(" — {}", stop.reason_text()))
        .unwrap_or_default();
    format!(
        "steering arbitrator finished sleeping without acting on the instruction ({} sleep extension(s), {}s slept){cap}",
        sleep.extensions, sleep.slept_secs
    )
}

/// Status line both hosts emit when an instruction is carried over instead of dropped.
pub(crate) fn durable_steer_status(reason: &str) -> String {
    format!("Steering instruction carried to the next seam — {reason}")
}

/// A steering notice posted by the bridge, together with what the bounded inbox did about it.
pub(crate) struct BridgeNotice {
    /// The notice that was posted (always the newest one, i.e. the one that survives).
    pub notice: SteerNotice,
    /// Notices evicted **oldest-first** by the [`INBOX_CAPACITY`] drop-oldest bound to make room
    /// for this one. Empty unless capacity was exceeded.
    pub dropped_older: Vec<SteerNotice>,
}

/// Post a steering notice **observably**.
///
/// The inbox bounds stay exactly as they are ([`INBOX_CAPACITY`] per target with a documented
/// drop-oldest policy, `MAX_INBOX_KEYS` targets); what must not happen is a silently lost
/// steering instruction: every notice dropped by capacity is returned here, logged, and surfaced
/// by the host through [`capacity_drop_text`].
pub(crate) fn post_notice_observable(target: &str, user_inquiry: &str) -> BridgeNotice {
    let outcome =
        crate::orchestrator::notice::post_notice_to_worker_tracked(target, user_inquiry, None);
    if !outcome.evicted_older.is_empty() {
        tracing::warn!(
            target_worker = target,
            dropped_notices = outcome.evicted_older.len(),
            inbox_capacity = INBOX_CAPACITY,
            reason = %capacity_drop_text(target, &outcome.evicted_older),
            "Steering notice inbox overflow: undelivered notice(s) dropped by the capacity bound"
        );
    }
    BridgeNotice {
        notice: outcome.notice,
        dropped_older: outcome.evicted_older,
    }
}

/// Single-source wording of an inbox-overflow (capacity drop) line.
pub(crate) fn capacity_drop_text(target: &str, dropped: &[SteerNotice]) -> String {
    let ids: Vec<&str> = dropped.iter().map(|n| n.notice_id.as_str()).collect();
    format!(
        "steering notice inbox for '{target}' is capped at {INBOX_CAPACITY}: dropped the {} oldest undelivered notice(s) ({}) — the newest instruction was kept",
        dropped.len(),
        ids.join(", ")
    )
}

/// The notice appended to the user instruction on every round after the arbitrator slept.
///
/// Single source of truth for this wording — both bridge hosts build their round > 1 payload
/// through this function.
pub(crate) fn wake_up_notice(user_msg: &str) -> String {
    format!(
        "{user_msg} (SYSTEM NOTICE: You already slept as requested and have now woken up to re-evaluate. Inspect the updated Active Subtasks and Plan Progress above and deliver your direct factual response or action now.)"
    )
}

/// Sleep-phase transitions reported by [`arbitrate_steering`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SleepNotice {
    Started {
        sleep_secs: u64,
    },
    Woke {
        sleep_secs: u64,
    },
    Cancelled,
    /// A documented chained-sleep cap ([`MAX_CHAINED_SLEEP_EXTENSIONS`] extensions /
    /// [`MAX_CHAINED_SLEEP_SECONDS`] cumulative) was reached: the chain stops here and both hosts
    /// surface it instead of sleeping again.
    Capped {
        slept_secs: u64,
        extensions: usize,
    },
}

impl SleepNotice {
    /// Renderer *status* wording used by the paused-stream sink (lowercase, unbracketed).
    pub(crate) fn status_text(&self) -> String {
        match self {
            SleepNotice::Started { sleep_secs } => {
                format!("Steering arbitrator sleeping for {sleep_secs}s...")
            }
            SleepNotice::Woke { sleep_secs } => {
                format!("Steering arbitrator woke up after {sleep_secs}s — re-evaluating status...")
            }
            SleepNotice::Cancelled => "Steering arbitrator sleep cancelled".to_string(),
            SleepNotice::Capped {
                slept_secs,
                extensions,
            } => format!(
                "Steering arbitrator sleep budget exhausted after {extensions} extension(s) / {slept_secs}s — not extending the sleep (limit {MAX_CHAINED_SLEEP_EXTENSIONS} extension(s), {MAX_CHAINED_SLEEP_SECONDS}s)"
            ),
        }
    }

    /// Streamed *delta* wording used by the spawned arbitrator task (bracketed, capitalised).
    pub(crate) fn delta_text(&self) -> String {
        match self {
            SleepNotice::Started { sleep_secs } => {
                format!("\n[Steering Arbitrator sleeping for {sleep_secs}s...]\n")
            }
            SleepNotice::Woke { sleep_secs } => format!(
                "[Steering Arbitrator woke up after {sleep_secs}s — re-evaluating status...]\n\n"
            ),
            SleepNotice::Cancelled => "[Steering Arbitrator sleep cancelled]\n".to_string(),
            SleepNotice::Capped {
                slept_secs,
                extensions,
            } => format!(
                "[Steering Arbitrator sleep budget exhausted after {extensions} extension(s) / {slept_secs}s — proceeding without further sleep]\n\n"
            ),
        }
    }
}

/// Host-side surface of one steering arbitration.
pub(crate) trait ArbitrationChannel {
    /// Active-subtask table presented to the arbitrator for the given 1-based round.
    fn active_subtasks(&self, round: usize) -> String;

    /// Whether the arbitrator is told that work is executing in the given 1-based round.
    fn has_active_work(&self, round: usize) -> bool;

    /// A new arbitration round is starting.
    fn round_started(&mut self, round: usize);

    /// One streamed chunk of the arbitrator's decision output.
    fn decision_delta(&mut self, delta: &str);

    /// A sleep-phase transition (started / woke / cancelled).
    fn sleep_notice(&mut self, notice: &SleepNotice);
}

/// Inputs shared by both hosts for one arbitration run.
pub(crate) struct ArbitrationRequest<'a> {
    pub client: &'a ChatClient,
    pub stats: &'a Arc<HarnessStats>,
    pub goal: &'a str,
    pub user_msg: &'a str,
    pub steering_history: Option<&'a SharedSteeringHistory>,
}

/// Result of one arbitration run.
pub(crate) struct ArbitrationOutcome {
    /// Last decision produced by the arbitrator (`None` if it produced nothing).
    pub decision: Option<SteerDecision>,
    /// Set when the loop stopped because the arbitrator's sleep was cancelled. The paused-stream
    /// sink returns `Resume` in that case, the spawned task keeps going with its post-loop work.
    pub sleep_cancelled: bool,
    /// What chained sleep did (extensions taken, seconds accounted, and the typed [`SleepStop`]
    /// when a documented cap ended it). Hosts use it to decide whether they may sleep at all and
    /// to explain a carried-over steering instruction.
    pub sleep: SleepChain,
}

/// Run the steering arbitration loop: snapshot context, preempt conflicting streams, ask the
/// arbitrator, apply the decision (cancel / abort / sleep-and-re-evaluate) and stop.
///
/// Chained sleep is bounded by [`MAX_CHAINED_SLEEP_EXTENSIONS`] extensions and the cumulative
/// [`MAX_CHAINED_SLEEP_SECONDS`] budget (both audited through [`SleepBudget`]); hitting either is
/// reported as a typed [`SleepStop`] on the outcome and surfaced to the host as
/// [`SleepNotice::Capped`] plus a `WARN` log line.
pub(crate) async fn arbitrate_steering<C: ArbitrationChannel>(
    channel: &mut C,
    request: &ArbitrationRequest<'_>,
) -> ArbitrationOutcome {
    let mut decision: Option<SteerDecision> = None;
    let mut round = 0usize;
    let mut sleep_budget = SleepBudget::new_at(Instant::now());
    let mut sleep_stop: Option<SleepStop> = None;

    while round < MAX_ARBITRATION_ROUNDS {
        round += 1;
        channel.round_started(round);

        let plan_content = crate::manager::phase::Plan::default()
            .read()
            .unwrap_or(None)
            .unwrap_or_default();
        let plan_progress_str = format_plan_progress_summary(&plan_content);
        let active_subtasks_str = channel.active_subtasks(round);
        let has_active = channel.has_active_work(round);
        let history_str = request
            .steering_history
            .and_then(|history| history.read().ok())
            .map(|history| format_steering_history(&history))
            .unwrap_or_else(|| "None".to_string());
        let effective_msg = if round == 1 {
            request.user_msg.to_string()
        } else {
            wake_up_notice(request.user_msg)
        };

        let ctx = SteerContext {
            main_goal: request.goal,
            orchestrator_status: if !has_active {
                "Active (planning/turn)"
            } else {
                "Active (subagents executing)"
            },
            pending_approval: "None",
            plan_progress: &plan_progress_str,
            plan_content: &plan_content,
            available_agents: "",
            steering_history: &history_str,
            user_message: &effective_msg,
            active_subtasks: &active_subtasks_str,
        };

        let preempt_handle =
            preempt_conflicting_stream(request.client.model(), &effective_msg).await;

        let cur_decision =
            arbitrate_steer_context_stream(request.client, request.stats, ctx, |delta| {
                channel.decision_delta(delta)
            })
            .await;

        let is_global_abort = matches!(
            normalize_steer_decision(cur_decision.as_ref().map(|d| d.decision.as_str())),
            "AbortImmediately" | "RejectPlan"
        );

        if is_global_abort {
            preempt_handle.complete_all(PauseAction::Abort);
            cancel_all();
        } else {
            preempt_handle.complete_with_subtask_decision(cur_decision.as_ref());
            // The raw `action` string is never compared here: every subtask is routed through
            // the orchestrator's single normalizer (see `super::action`).
            for (st, subtask_action) in action::routed(cur_decision.as_ref()) {
                if subtask_action == action::ACTION_CANCEL {
                    cancel_active_worker(st.agent_name.as_deref(), Some(&st.tool_call_id));
                }
            }
        }

        decision = cur_decision;

        if let Some(ref d) = decision
            && normalize_steer_decision(Some(&d.decision)) == "Sleep"
        {
            // Floor and default come from the `sleep`-argument owner, the ceiling
            // from this module's own budget knob (see [`clamped_sleep_secs`]): a
            // steered `sleep_seconds: 0` must never become a zero-length wait.
            let requested = clamped_sleep_secs(d.sleep_seconds);
            let sleep_secs = match sleep_budget.allow_at(requested, Instant::now()) {
                Err(stop) => {
                    // A documented chained-sleep cap was hit: deterministic, observable end of
                    // the chain — a typed outcome on the result, a WARN line and a capped notice
                    // on the host channel — never a silent `continue`.
                    tracing::warn!(
                        sleep_extensions = sleep_budget.extensions(),
                        slept_secs = sleep_budget.slept_secs(),
                        requested_sleep_secs = requested,
                        reason = %stop.reason_text(),
                        "Steer arbitrator chained sleep reached its documented budget — stopping the sleep chain"
                    );
                    sleep_stop = Some(stop);
                    channel.sleep_notice(&SleepNotice::Capped {
                        slept_secs: sleep_budget.slept_secs(),
                        extensions: sleep_budget.extensions(),
                    });
                    break;
                }
                Ok(sleep_secs) => sleep_secs,
            };
            channel.sleep_notice(&SleepNotice::Started { sleep_secs });
            if let Some(history) = request.steering_history
                && let Ok(mut hist) = history.write()
            {
                let note = d.response.as_deref().unwrap_or("Slept");
                hist.push((
                    request.user_msg.to_string(),
                    format!("{note} (slept for {sleep_secs}s)"),
                ));
            }
            let cancel = crate::orchestrator::bus::global_cancellation_token();
            let was_cancelled = tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)) => {
                    channel.sleep_notice(&SleepNotice::Woke { sleep_secs });
                    false
                }
                _ = cancel.cancelled() => {
                    channel.sleep_notice(&SleepNotice::Cancelled);
                    true
                }
            };
            sleep_budget.note_slept(sleep_secs);
            if was_cancelled {
                return ArbitrationOutcome {
                    decision,
                    sleep_cancelled: true,
                    sleep: SleepChain {
                        stop: None,
                        ..sleep_budget.chain()
                    },
                };
            }
            continue;
        }

        break;
    }

    ArbitrationOutcome {
        decision,
        sleep_cancelled: false,
        sleep: SleepChain {
            stop: sleep_stop,
            ..sleep_budget.chain()
        },
    }
}

/// Subtasks the finished decision asks to delegate (empty when there is no decision).
pub(crate) fn delegation_tasks(
    decision: Option<&SteerDecision>,
    user_msg: &str,
) -> Vec<(Agent, String, String)> {
    decision
        .map(|d| extract_tasks_to_delegate(d, user_msg))
        .unwrap_or_default()
}

/// Run one steering-delegated subtask, turning an execution error into a failed deliverable.
pub(crate) async fn run_delegated_subtask(
    client: &ChatClient,
    stats: &Arc<HarnessStats>,
    agent: Agent,
    task_id: &str,
    prompt: &str,
) -> Deliverable {
    match execute_steer_subtask(
        client,
        stats.clone(),
        agent,
        Some(task_id.to_string()),
        prompt,
    )
    .await
    {
        Ok(deliverable) => deliverable,
        Err(e) => Deliverable {
            marker: MissionMarker::Failed {
                reason: e.to_string(),
            },
            content: format!("Execution failed: {e}"),
            task_id: Some(task_id.to_string()),
        },
    }
}

/// The entry recorded in the steering history for a finished arbitration.
///
/// `synthesized` is the synthesized answer of delegated subtasks (only the spawned arbitrator
/// produces one) and `forwarded` the notices forwarded to workers in posting order (only the
/// spawned arbitrator forwards them — the sink does that in its own decision handling).
pub(crate) fn recorded_response(
    decision: Option<&SteerDecision>,
    synthesized: Option<&str>,
    forwarded: &[(String, String)],
) -> String {
    if let Some(answer) = synthesized {
        return answer.to_string();
    }
    let Some(d) = decision else {
        return "No decision".to_string();
    };
    if let Some(response) = d.response.as_ref() {
        return response.clone();
    }
    match normalize_steer_decision(Some(&d.decision)) {
        // The wording must describe the sleep that was actually taken, so it
        // goes through the same floor/ceiling the loop slept with.
        "Sleep" => format!("Slept for {}s", clamped_sleep_secs(d.sleep_seconds)),
        // `forwarded` is only ever populated for a `ForwardToWorker` decision.
        "ForwardToWorker" if !forwarded.is_empty() => forwarded
            .iter()
            .map(|(notice_id, target)| {
                format!("Forwarded notice {notice_id} to {target} (awaiting specialist reply)")
            })
            .collect::<Vec<_>>()
            .join(", "),
        "ForwardToWorker" => "Forwarded notice to worker (awaiting specialist reply)".to_string(),
        _ => format!("Decision: {}", d.decision),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One arbitration test on its own current-thread runtime, with the
    /// process-global cancellation token reset before and after (the same
    /// discipline the bridge host tests use, sharing the worker mutex).
    fn arbitration_test<R>(test: impl std::future::Future<Output = R>) -> R {
        let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::orchestrator::reset_cancellation();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime for the arbitration test");
        let result = runtime.block_on(test);
        crate::orchestrator::reset_cancellation();
        result
    }

    /// SSE body carrying one streamed assistant text chunk.
    fn sse_chunk(content: &str) -> String {
        let payload = serde_json::json!({
            "id": "chatcmpl-floor",
            "choices": [{ "delta": { "content": content }, "finish_reason": null }],
        });
        format!("data: {payload}\n\ndata: [DONE]\n\n")
    }

    /// Minimal host double: remembers the rounds and the sleep notices.
    #[derive(Default)]
    struct RecordingChannel {
        rounds: Vec<usize>,
        notices: Vec<SleepNotice>,
    }

    impl ArbitrationChannel for RecordingChannel {
        fn active_subtasks(&self, round: usize) -> String {
            format!("active subtasks of round {round}")
        }

        fn has_active_work(&self, _round: usize) -> bool {
            true
        }

        fn round_started(&mut self, round: usize) {
            self.rounds.push(round);
        }

        fn decision_delta(&mut self, _delta: &str) {}

        fn sleep_notice(&mut self, notice: &SleepNotice) {
            self.notices.push(*notice);
        }
    }

    /// (f) **The floor.** A `Sleep` that asks for less than the `sleep` tool's
    /// own minimum — including `0` — must never turn into a zero-length wait,
    /// and a `Sleep` with no duration at all uses the owner's default.
    #[test]
    fn clamped_sleep_secs_floors_a_sub_minimum_request() {
        assert_eq!(
            clamped_sleep_secs(Some(0)),
            SLEEP_MIN_SECS,
            "sleep_seconds: 0 must be raised to the sleep tool's minimum"
        );
        for requested in 0..SLEEP_MIN_SECS {
            assert_eq!(
                clamped_sleep_secs(Some(requested)),
                SLEEP_MIN_SECS,
                "{requested}s is below the floor"
            );
        }
        assert_eq!(clamped_sleep_secs(Some(SLEEP_MIN_SECS)), SLEEP_MIN_SECS);
        assert_eq!(
            clamped_sleep_secs(Some(7)),
            7,
            "a valid request is untouched"
        );
        assert_eq!(
            clamped_sleep_secs(None),
            SLEEP_DEFAULT_SECS,
            "a Sleep without a duration uses the owner's default"
        );
        for requested in [None, Some(0), Some(1), Some(30), Some(u64::MAX)] {
            assert!(
                clamped_sleep_secs(requested) > 0,
                "no sleep request may ever yield a zero-length wait \
                 (the hot-loop the floor exists to prevent), saw {requested:?}"
            );
        }
        // The floor/default are the sleep-argument owner's constants, not numbers
        // re-typed in this module.
        assert_eq!(SLEEP_MIN_SECS, crate::tool_args::SLEEP_MIN_SECS);
        assert_eq!(SLEEP_DEFAULT_SECS, crate::tool_args::SLEEP_DEFAULT_SECS);
    }

    /// (f) **The ceiling stays separate.** The arbitrator's upper knobs bound
    /// one steering instruction, not the `sleep` tool, so they must not be
    /// derived from the tool's ceiling constant — proved on the source level
    /// (comment lines excluded, like the other source guards) and behaviourally.
    #[test]
    fn arbitrator_upper_budget_is_not_the_sleep_tool_ceiling() {
        let src = include_str!("arbiter.rs");
        // Production region only: everything from the first `#[cfg(test)]`
        // attribute onwards is this module's own test scaffolding.
        let production = src
            .split(&["#[cfg(", "test)", "]"].concat())
            .next()
            .expect("arbiter.rs always has a first region");
        let is_comment = |line: &str| {
            let trimmed = line.trim_start();
            trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')
        };
        let code: String = production
            .lines()
            .filter(|line| !is_comment(line))
            .collect::<Vec<&str>>()
            .join("\n");

        // Assembled at runtime (style of the sleep guards) so this test's own
        // text cannot satisfy the needle it forbids.
        let tool_ceiling = ["SLEEP_MAX", "_SECS"].concat();
        assert!(
            !code.contains(&tool_ceiling),
            "the arbitrator's upper budget (MAX_SINGLE_SLEEP_SECONDS / \
             MAX_CHAINED_SLEEP_SECONDS) must stay its own knob and must never be \
             derived from the sleep tool's ceiling constant"
        );
        for owner_constant in ["SLEEP_MIN_SECS", "SLEEP_DEFAULT_SECS"] {
            assert!(
                code.contains(owner_constant),
                "the arbitrator's floor/default must come from crate::tool_args, \
                 not from re-typed numbers ({owner_constant} missing)"
            );
        }
        for own_knob in ["MAX_SINGLE_SLEEP_SECONDS", "MAX_CHAINED_SLEEP_SECONDS"] {
            assert!(
                code.contains(own_knob),
                "{own_knob} must stay the arbitrator's own budget knob"
            );
        }

        for requested in [MAX_SINGLE_SLEEP_SECONDS + 1, u64::MAX] {
            assert_eq!(
                clamped_sleep_secs(Some(requested)),
                MAX_SINGLE_SLEEP_SECONDS,
                "a request above the arbitrator's own ceiling is refused down to it"
            );
        }
        assert!(
            MAX_CHAINED_SLEEP_SECONDS >= clamped_sleep_secs(Some(u64::MAX)),
            "the chained budget must still cover at least one maximum single sleep \
             (both are live arbitrator knobs, not zeroed-out leftovers)"
        );
    }

    /// (f) The **steered** path, end to end: a `Sleep` decision carrying
    /// `sleep_seconds: 0` sleeps the floor instead of nothing, both hosts report
    /// the floor, and the history text names the seconds actually slept.
    #[test]
    fn steered_sleep_with_zero_seconds_sleeps_the_floor_not_nothing() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        arbitration_test(async {
            let server = MockServer::start().await;
            // Round 2 (the wake-up round) answers instead of sleeping again.
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .and(body_string_contains(
                    "SYSTEM NOTICE: You already slept as requested",
                ))
                .respond_with(ResponseTemplate::new(200).set_body_string(sse_chunk(
                    r#"{"decision": "RespondDirectly", "response": "woke and answered"}"#,
                )))
                .mount(&server)
                .await;
            // Round 1: a Sleep that asks for ZERO seconds.
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_string(sse_chunk(
                    r#"{"decision": "Sleep", "response": "waiting for the build", "sleep_seconds": 0}"#,
                )))
                .up_to_n_times(1)
                .mount(&server)
                .await;

            let client = crate::llm::ChatClient::new_with_token(server.uri(), "mock", "tok");
            let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
            let history =
                std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
            let mut channel = RecordingChannel::default();

            let started = std::time::Instant::now();
            let outcome = arbitrate_steering(
                &mut channel,
                &ArbitrationRequest {
                    client: &client,
                    stats: &stats,
                    goal: "wait for the build",
                    user_msg: "wait for the build to finish",
                    steering_history: Some(&history),
                },
            )
            .await;
            let waited = started.elapsed();

            assert!(!outcome.sleep_cancelled);
            assert_eq!(
                channel.rounds,
                vec![1, 2],
                "a floored sleep must still re-evaluate in a second round"
            );
            assert_eq!(
                channel.notices,
                vec![
                    SleepNotice::Started {
                        sleep_secs: SLEEP_MIN_SECS
                    },
                    SleepNotice::Woke {
                        sleep_secs: SLEEP_MIN_SECS
                    },
                ],
                "the requested 0s must be reported as the floored sleep"
            );
            assert!(
                waited >= std::time::Duration::from_millis(500),
                "the loop must actually wait, not spin: slept {waited:?} \
                 for a {SLEEP_MIN_SECS}s floor"
            );
            assert_eq!(
                outcome.sleep.slept_secs, SLEEP_MIN_SECS,
                "the chained budget accounts the floored sleep"
            );

            let hist = history.read().unwrap();
            assert_eq!(hist.len(), 1, "one sleep round, one history entry");
            assert_eq!(
                hist[0].1,
                format!("waiting for the build (slept for {SLEEP_MIN_SECS}s)"),
                "the history text names the seconds actually slept"
            );
        });

        // The other site of the old `unwrap_or(5)` — the recorded history line of
        // a Sleep decision with no response text — is floored the same way.
        let no_response = SteerDecision {
            decision: "Sleep".to_string(),
            response: None,
            tier: None,
            model: None,
            subtasks: Vec::new(),
            sleep_seconds: Some(0),
        };
        assert_eq!(
            recorded_response(Some(&no_response), None, &[]),
            format!("Slept for {SLEEP_MIN_SECS}s")
        );
        let missing = SteerDecision {
            decision: "Sleep".to_string(),
            response: None,
            tier: None,
            model: None,
            subtasks: Vec::new(),
            sleep_seconds: None,
        };
        assert_eq!(
            recorded_response(Some(&missing), None, &[]),
            format!("Slept for {SLEEP_DEFAULT_SECS}s")
        );
    }

    /// (g) **The clamp may not undo the floor** (gate t-076). A SUB-SECOND remainder of the
    /// cumulative budget is not `Duration::is_zero()`, but it carries zero whole seconds:
    /// the old `Ok(requested.min(remaining.as_secs()))` returned `Ok(0)`, the round machine
    /// then booked `note_slept(0)` and re-asked the arbitrator immediately — the hot-loop
    /// [`clamped_sleep_secs`] exists to prevent, re-created by clamping after the floor.
    /// Such a remainder is now refused through the same typed exhaustion outcome, and the
    /// host-facing [`SleepChain::host_sleep_secs`] applies the same refuse-vs-floor policy.
    #[test]
    fn remaining_budget_below_the_floor_is_refused_never_a_zero_length_sleep() {
        let now = Instant::now();
        let spent = now - Duration::from_secs(MAX_CHAINED_SLEEP_SECONDS);

        for remaining_ms in [1u64, 500, 999] {
            let budget = SleepBudget::new_at(spent + Duration::from_millis(remaining_ms));
            let outcome = budget.allow_at(30, now);
            assert!(
                matches!(outcome, Err(SleepStop::Deadline { .. })),
                "a {remaining_ms}ms remainder must be an observable refusal, got {outcome:?}"
            );
            assert!(
                !matches!(outcome, Ok(0)),
                "Ok(0) is the zero-length sleep this gate forbids ({remaining_ms}ms remainder)"
            );
            // A refusal means the loop never accounts anything: it only ever calls
            // `note_slept` with the seconds `allow_at` handed back.
            let mut budget = SleepBudget::new_at(spent + Duration::from_millis(remaining_ms));
            if let Ok(secs) = budget.allow_at(30, now) {
                budget.note_slept(secs);
            }
            assert_eq!(
                (budget.extensions(), budget.slept_secs()),
                (0, 0),
                "a refused sub-second remainder must not book note_slept(0)"
            );
        }

        // The cut-off is the floor, not any fraction of a second: exactly `SLEEP_MIN_SECS`
        // of budget is still slept, and never rounded up past the hard limit.
        assert_eq!(
            SleepBudget::new_at(spent + Duration::from_secs(SLEEP_MIN_SECS)).allow_at(120, now),
            Ok(SLEEP_MIN_SECS)
        );
        assert_eq!(
            SleepBudget::new_at(spent + Duration::from_millis(1_900)).allow_at(120, now),
            Ok(1),
            "the budget clamp still wins over the floor inside the budget"
        );

        // The host-side half, same policy.
        let chain = SleepChain {
            extensions: 0,
            slept_secs: 0,
            stop: None,
        };
        assert_eq!(chain.host_sleep_secs(0), Some(SLEEP_MIN_SECS));
        assert_eq!(
            SleepChain {
                extensions: 0,
                slept_secs: MAX_CHAINED_SLEEP_SECONDS,
                stop: None,
            }
            .host_sleep_secs(0),
            None,
            "a budget that cannot cover the floor is refused, not slept for nothing"
        );
    }
}
