//! `RendererSink`: the `StreamSink` adapter bridging LLM streaming into the renderer.

use super::arbiter::{self, ArbitrationChannel, SleepNotice};
use super::drain::drain_steer_arbitration_events;
use super::steer::{SharedSteeringHistory, SteerArbEvent};
use crate::llm::{ChatClient, PauseAction, StreamControl, StreamEvent, StreamSink};
use crate::ui::helpers::{is_abort_command, is_reset_command};
use crate::ui::{Event, Renderer, SubagentDetail};
use std::sync::Arc;

/// What the paused-stream host asks the sleep chain for, given the `Sleep` decision it renders.
///
/// The duration is resolved by the single owner, [`arbiter::clamped_sleep_secs`] — the owner's
/// default, its `SLEEP_MIN_SECS` floor and the arbitrator's own upper budget knob. Host and
/// arbitrator agree on the number by construction: a steered `"sleep_seconds": 0` is raised to
/// that floor before it reaches the host, so the announced duration is the slept duration.
pub(crate) fn host_sleep_request(decision: Option<&crate::orchestrator::SteerDecision>) -> u64 {
    arbiter::clamped_sleep_secs(decision.and_then(|d| d.sleep_seconds))
}

/// Cadence of the in-sink panel pump (see [`await_with_panel_pump`]).
///
/// It matches the pool's own rank-refresh cadence (`QUEUE_POSITION_REFRESH` in
/// `src/orchestrator/pool.rs`), which is the only source of the `(Queued n/m)` rank the pump
/// can surface: pumping faster than the pool re-reports can only produce byte-identical row
/// sets, which the projection's signature compare throws away anyway.
const PANEL_PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Re-project the host's subagent rows from the worker registry and push them when a visible
/// edge moved — the sink-side twin of the session loop's own refresh edge
/// (`session.rs::project_subagent_pane`), calling the same single projection owner
/// (`helpers::project_subagent_rows`).
///
/// Returns whether anything was pushed. This is deliberately a *projection*, not a second event
/// consumer: the delegation-event vector, the pool's status channel and the transcript all stay
/// owned by the session loop, so nothing here can steal a record the loop still has to fold, and
/// the steer-arbitration drain in [`RendererSink::poll_control`] keeps its exact position and
/// ordering.
pub(crate) fn pump_subagent_panel(
    rows: &mut Vec<SubagentDetail>,
    renderer: &mut dyn Renderer,
) -> bool {
    if crate::ui::helpers::project_subagent_rows(rows) {
        renderer.set_subagents(rows.clone());
        let _ = renderer.flush();
        true
    } else {
        false
    }
}

/// Await `work` while keeping the subagent pane alive.
///
/// The reason this exists is structural: an await **inside the sink** (the inline
/// `arbiter::run_delegated_subtask` of a steering-delegated subtask, pool slot wait included)
/// runs while the session loop is parked on the very turn that owns the sink, so the loop
/// cannot come back to its own drain/projection edge for the whole duration. `poll_control`
/// does not help either — it is only called per streamed chunk. The pane would therefore sit
/// on the start line and show nothing about a child that is queued for a backend slot.
///
/// So the pump runs here instead, on the loop below: the future is pinned, and every
/// [`PANEL_PUMP_INTERVAL`] the pane is re-projected from the registry (which since fix #3 owns
/// the queue rank, so a `Queued` the loop never drained self-heals without any event).
pub(crate) async fn await_with_panel_pump<F>(
    rows: &mut Vec<SubagentDetail>,
    renderer: &mut dyn Renderer,
    work: F,
) -> F::Output
where
    F: std::future::Future,
{
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            out = &mut work => return out,
            _ = tokio::time::sleep(PANEL_PUMP_INTERVAL) => {
                pump_subagent_panel(rows, renderer);
            }
        }
    }
}

pub struct RendererSink<'a> {
    pub renderer: &'a mut dyn Renderer,
    pub steer_queue: &'a mut Vec<String>,
    pub steer_abort_requested: &'a mut bool,
    pub arb_tx: &'a tokio::sync::mpsc::UnboundedSender<SteerArbEvent>,
    pub arb_rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<SteerArbEvent>,
    pub client: &'a ChatClient,
    pub stats: Arc<crate::harness::HarnessStats>,
    pub goal: &'a str,
    /// The host's **authoritative** subagent pane row buffer (`session.rs`'s `subagents`),
    /// i.e. the same vector the session loop projects from the worker registry and pushes to
    /// the renderer.
    ///
    /// It is still never an arbitration input: the arbitrator reads the worker registry, and a
    /// pane row is only a projection of that registry. The sink is handed the buffer so it can
    /// (a) report a pane/registry divergence and (b) run the **same** sanctioned projection
    /// while an await inside the sink has the session loop blocked ([`PANEL_PUMP_INTERVAL`]).
    /// That is not a second panel model: it is one buffer, one projection function, and it
    /// writes only what the registry is authoritative for.
    pub subagents: &'a mut Vec<SubagentDetail>,
    pub plan: Option<&'a crate::manager::phase::Plan>,
    pub ctx: Option<&'a mut crate::manager::context::ContextEngine>,
    pub steering_history: Option<SharedSteeringHistory>,
}

#[async_trait::async_trait]
impl StreamSink for RendererSink<'_> {
    fn emit(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Content(text) => {
                self.renderer.reset_active_agent();
                self.renderer.on_event(&Event::Message(text));
            }
            StreamEvent::Thinking(text) => {
                self.renderer.reset_active_agent();
                self.renderer.on_event(&Event::Thinking(text));
            }
            StreamEvent::Status(text) => self.renderer.on_event(&Event::Status(text)),
        }
        let _ = self.renderer.flush();
    }

    fn is_aborted(&mut self) -> bool {
        self.renderer.aborted() || *self.steer_abort_requested
    }

    fn poll_control(&mut self) -> StreamControl {
        let _ = self.renderer.flush();
        // `None` for the row buffer here is correct, not an omission: the drain no longer
        // hand-writes row state at all (it cancels through the registry and, when a buffer is
        // available, only *re-projects* rows and annotates the addressed one). A mid-turn drain
        // therefore changes registry state; the pane catches up on the session loop's projection
        // edge (`session.rs` -> `helpers::project_subagent_rows`), which is the only writer of
        // `is_active`.
        //
        // Fix #4 deliberately does **not** add a panel pump here either: `poll_control` runs once
        // per streamed chunk, so a pump here would move the pane while the Manager's own turn is
        // still in flight — mid-way between two loop drains. That contract (a turn in flight is a
        // drain-free window the loop closes at its next boundary) is pinned by
        // `tests/test_subpane_queued_refresh.rs`, and the loop's boundary drain folds the same
        // state one edge later anyway. The window this sink *can* fix safely is the one where the
        // loop is not merely streaming but blocked inside an await owned by the sink itself — the
        // inline delegated-subtask await below (see [`await_with_panel_pump`]).
        drain_steer_arbitration_events(
            self.arb_rx,
            self.renderer,
            self.steer_queue,
            self.steer_abort_requested,
            None,
        );
        if let Some(input) = self.renderer.poll_input() {
            if is_abort_command(&input) {
                crate::debug_log::log_user_input("command", &input);
                self.renderer.request_user_exit();
                return StreamControl::Abort;
            } else if is_reset_command(&input) {
                crate::debug_log::log_user_input("command", &input);
                let default_plan = crate::manager::phase::Plan::default();
                let plan = self.plan.unwrap_or(&default_plan);
                let ctx = self.ctx.as_deref_mut();
                crate::ui::helpers::handle_reset_command(plan, self.renderer, ctx);
            } else if !input.trim().is_empty() {
                crate::debug_log::log_user_input("midflight_steer", &input);
                return StreamControl::Pause { user_input: input };
            }
        }
        if self.renderer.aborted() || *self.steer_abort_requested {
            StreamControl::Abort
        } else {
            StreamControl::Continue
        }
    }

    async fn on_pause(&mut self, user_msg: &str) -> PauseAction {
        // Diagnostic only — and deliberately not an input: the arbitration snapshot comes from
        // the worker registry, so a pane row whose `is_active` flag went stale cannot make the
        // arbitrator believe work is running. When pane rows and registry disagree, the pane is
        // simply behind, and the session loop's projection edge re-derives it.
        if self.subagents.iter().any(|s| s.is_active) && !crate::orchestrator::has_active_workers()
        {
            tracing::debug!(
                stale_pane_rows = self.subagents.iter().filter(|s| s.is_active).count(),
                "Steer arbitration: pane rows are ahead of the registry; the arbitrator is \
                 told what the registry says"
            );
        }
        // The arbitration round machine (context snapshot, SYSTEM NOTICE wake-up rounds,
        // preemption, decision application and sleep/re-evaluation) lives in `arbiter`.
        let mut channel = RendererChannel {
            renderer: &mut *self.renderer,
        };
        let arbiter::ArbitrationOutcome {
            decision,
            sleep_cancelled,
            sleep,
        } = arbiter::arbitrate_steering(
            &mut channel,
            &arbiter::ArbitrationRequest {
                client: self.client,
                stats: &self.stats,
                goal: self.goal,
                user_msg,
                steering_history: self.steering_history.as_ref(),
            },
        )
        .await;

        if sleep_cancelled {
            // Durable steering: an instruction whose sleep was cut short has no action attached
            // to it yet, so it is queued for the next seam instead of being dropped.
            if arbiter::is_terminal_sleep(decision.as_ref()) {
                let reason = arbiter::durable_steer_reason(&sleep);
                tracing::warn!(reason = %reason, "Steering instruction survived a cancelled arbitrator sleep — queued for the next seam");
                self.steer_queue.push(user_msg.to_string());
                self.renderer
                    .on_event(&Event::Status(arbiter::durable_steer_status(&reason)));
                let _ = self.renderer.flush();
            }
            return PauseAction::Resume;
        }

        // **One notice per `ForwardNotice` subtask, posted once, here.** This host owns the
        // posting for the paused-stream path exactly as the spawned host does for its path
        // (both go through `arbiter::post_forward_notices`), and the decision arms below only
        // render the result. The old shape posted the arbitrator's text here and then posted
        // the raw user message again as a `worker` broadcast, so one steer filed two notices
        // for the same decision and added a third delivery as a Manager turn.
        let outcomes = arbiter::post_forward_notices(decision.as_ref(), user_msg);
        for outcome in &outcomes {
            match outcome {
                arbiter::ForwardOutcome::Posted(posted) => {
                    self.renderer.on_event(&Event::Status(format!(
                        "Notice {} forwarded to {} — resuming stream...",
                        posted.notice_id, posted.target
                    )));
                    if !posted.dropped_older.is_empty() {
                        // Backpressure is kept (drop-oldest at INBOX_CAPACITY) but never silent.
                        let drop_text =
                            arbiter::capacity_drop_text(&posted.target, &posted.dropped_older);
                        tracing::warn!(target_worker = %posted.target, reason = %drop_text, "Steering notice dropped by inbox capacity");
                        self.renderer
                            .on_event(&Event::Status(format!("[Arbitrator] {drop_text}")));
                    }
                }
                arbiter::ForwardOutcome::Unroutable { token } => {
                    // Never filed into an inbox no worker drains: the refusal is surfaced.
                    self.renderer.on_event(&Event::Status(format!(
                        "[Arbitrator] {}",
                        arbiter::unroutable_target_text(token)
                    )));
                }
            }
        }
        let forwarded = arbiter::posted_notices(&outcomes);
        let unroutable = arbiter::unroutable_targets(&outcomes);

        let recorded_resp =
            arbiter::recorded_response(decision.as_ref(), None, &forwarded, &unroutable);
        if let Some(ref hist_lock) = self.steering_history
            && let Ok(mut hist) = hist_lock.write()
        {
            hist.push((user_msg.to_string(), recorded_resp));
        }

        let tasks = arbiter::delegation_tasks(decision.as_ref(), user_msg);

        if !tasks.is_empty() {
            self.renderer.on_event(&Event::Status(
                "Executing delegated subtask(s) from steering...".to_string(),
            ));
            let _ = self.renderer.flush();

            for (agent, task_id, prompt) in tasks {
                self.renderer.on_event(&Event::Delegation(
                    crate::orchestrator::DelegationEvent::Started {
                        agent,
                        task: Some(task_id.clone()),
                    },
                ));
                self.renderer.on_event(&Event::Status(format!(
                    "Steering delegated subtask '{task_id}' to {}...",
                    agent.as_str()
                )));
                let _ = self.renderer.flush();

                // C1 (docs/diagnose_subpane_stale_refresh.md): this await holds the session
                // loop hostage — the loop is parked on the turn that owns this sink, so it
                // cannot reach its own drain/projection edge for the whole subtask, pool slot
                // wait included. Pump the pane from here, or the child shows its start line and
                // then nothing at all while it is genuinely waiting for a backend slot.
                let client = self.client;
                let stats = &self.stats;
                let deliverable = await_with_panel_pump(
                    &mut *self.subagents,
                    &mut *self.renderer,
                    arbiter::run_delegated_subtask(client, stats, agent, &task_id, &prompt),
                )
                .await;

                self.renderer.on_event(&Event::Delegation(
                    crate::orchestrator::DelegationEvent::Completed {
                        agent,
                        task: Some(task_id.clone()),
                    },
                ));
                self.renderer.reset_active_agent();
                self.renderer.on_event(&Event::Message(format!(
                    "\n[Steering Subtask — {} ({})]\n{}\n{}\n",
                    agent.as_str(),
                    task_id,
                    super::action::deliverable_verdict_line(&task_id, &deliverable),
                    deliverable.content
                )));
                self.renderer.on_event(&Event::Status(format!(
                    "Steering subtask '{}' completed by {}",
                    task_id,
                    agent.as_str()
                )));
                let _ = self.renderer.flush();

                self.steer_queue.push(format!(
                    "(User steering resulted in subtask '{task_id}' executed by specialist '{}'. Deliverable:\n{})",
                    agent.as_str(),
                    deliverable.content
                ));
            }
            self.renderer.reset_active_agent();
            PauseAction::Resume
        } else {
            let norm = crate::orchestrator::normalize_steer_decision(
                decision.as_ref().map(|d| d.decision.as_str()),
            );
            match norm {
                "RespondDirectly" => {
                    self.renderer.on_event(&Event::Status(
                        "Answered via direct steer response — resuming stream...".to_string(),
                    ));
                    let _ = self.renderer.flush();
                    PauseAction::Resume
                }
                "Sleep" => {
                    let requested = host_sleep_request(decision.as_ref());
                    // Chained sleep stays inside the documented budget: the arbitrator already
                    // slept while arbitrating, so the host may only extend it while budget is left.
                    match sleep.host_sleep_secs(requested) {
                        Some(sleep_secs) => {
                            self.renderer.on_event(&Event::Status(format!(
                                "Steering arbitrator sleeping for {sleep_secs}s..."
                            )));
                            let _ = self.renderer.flush();
                            let cancel = crate::orchestrator::bus::global_cancellation_token();
                            tokio::select! {
                                _ = tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)) => {
                                    self.renderer.on_event(&Event::Status(format!(
                                        "Steering arbitrator woke up after {sleep_secs}s — resuming stream..."
                                    )));
                                }
                                _ = cancel.cancelled() => {
                                    self.renderer.on_event(&Event::Status(
                                        "Steering arbitrator sleep cancelled".to_string(),
                                    ));
                                }
                            }
                        }
                        None => {
                            let reason =
                                sleep
                                    .stop
                                    .map(|stop| stop.reason_text())
                                    .unwrap_or_else(|| {
                                        format!(
                                            "chained sleep budget of {}s already spent",
                                            arbiter::MAX_CHAINED_SLEEP_SECONDS
                                        )
                                    });
                            tracing::warn!(
                                sleep_extensions = sleep.extensions,
                                slept_secs = sleep.slept_secs,
                                requested_sleep_secs = requested,
                                reason = %reason,
                                "Steer host did not extend the arbitrator sleep: chained-sleep budget spent"
                            );
                            self.renderer.on_event(&Event::Status(
                                SleepNotice::Capped {
                                    slept_secs: sleep.slept_secs,
                                    extensions: sleep.extensions,
                                }
                                .status_text(),
                            ));
                        }
                    }
                    // Durable steering: a terminal `Sleep` must not swallow the instruction —
                    // it is queued and delivered at the next seam instead.
                    let reason = arbiter::durable_steer_reason(&sleep);
                    self.steer_queue.push(user_msg.to_string());
                    self.renderer
                        .on_event(&Event::Status(arbiter::durable_steer_status(&reason)));
                    let _ = self.renderer.flush();
                    PauseAction::Resume
                }
                "AbortImmediately" => {
                    self.renderer.request_abort();
                    *self.steer_abort_requested = true;
                    self.steer_queue.push(user_msg.to_string());
                    self.renderer.on_event(&Event::Status(
                        "Steering requested immediate abort".to_string(),
                    ));
                    let _ = self.renderer.flush();
                    PauseAction::Abort
                }
                "RejectPlan" => {
                    self.renderer.request_abort();
                    *self.steer_abort_requested = true;
                    self.steer_queue
                        .push(format!("User rejected plan: {user_msg}"));
                    self.renderer.on_event(&Event::Status(
                        "Plan rejected — aborting current turn".to_string(),
                    ));
                    let _ = self.renderer.flush();
                    PauseAction::Abort
                }
                "ForwardToWorker" => {
                    // The notices for this decision were already posted (once) above, and
                    // their ids are what the steering history recorded. Re-posting the raw
                    // user message here is what used to create the phantom second notice, so
                    // this arm only renders the outcome and decides whether the instruction
                    // still has to reach the Manager.
                    if forwarded.is_empty() {
                        // Nothing was filed (no `ForwardNotice` subtask, or every target was
                        // refused): the instruction must survive, so it is queued for the next
                        // seam instead of being dropped with this pause.
                        self.steer_queue.push(user_msg.to_string());
                        self.renderer.on_event(&Event::Status(
                            "No forwarding notice was posted for this steering instruction — \
                             queued for the next turn instead"
                                .to_string(),
                        ));
                    } else {
                        self.renderer
                            .on_event(&Event::Status(arbiter::forwarded_status(&forwarded, true)));
                    }
                    let _ = self.renderer.flush();
                    PauseAction::Resume
                }
                "ApprovePlan" => {
                    self.steer_queue.push("User approved plan.".to_string());
                    self.renderer.on_event(&Event::Status(
                        "Plan approved — resuming stream...".to_string(),
                    ));
                    let _ = self.renderer.flush();
                    PauseAction::Resume
                }
                "QueueAndContinue" => {
                    self.steer_queue.push(user_msg.to_string());
                    if decision
                        .as_ref()
                        .and_then(|d| d.response.as_ref())
                        .is_none()
                    {
                        self.renderer.on_event(&Event::SteerResponse(
                            "Instruction queued for next turn while active tasks continue.\n"
                                .to_string(),
                        ));
                    }
                    self.renderer.on_event(&Event::Status(
                        "Instruction queued for next turn — resuming stream...".to_string(),
                    ));
                    let _ = self.renderer.flush();
                    PauseAction::Resume
                }
                _ => {
                    let has_response = decision
                        .as_ref()
                        .and_then(|d| d.response.as_ref())
                        .is_some();
                    if !has_response {
                        self.steer_queue.push(user_msg.to_string());
                        self.renderer.on_event(&Event::Status(
                            "Instruction queued for next turn — resuming stream...".to_string(),
                        ));
                    } else {
                        self.renderer.on_event(&Event::Status(
                            "Answered via direct steer response — resuming stream...".to_string(),
                        ));
                    }
                    let _ = self.renderer.flush();
                    PauseAction::Resume
                }
            }
        }
    }
}

/// Arbitration channel of the paused-stream host: progress is rendered straight into the
/// renderer (status events + streamed steer responses). Progress only — the execution
/// snapshot the arbitrator is shown is the worker registry, not the renderer's pane.
struct RendererChannel<'a> {
    renderer: &'a mut dyn Renderer,
}

impl ArbitrationChannel for RendererChannel<'_> {
    fn round_started(&mut self, _round: usize) {
        self.renderer.on_event(&Event::Status(
            "Stream paused — evaluating steering instruction...".to_string(),
        ));
        let _ = self.renderer.flush();
    }

    fn decision_delta(&mut self, delta: &str) {
        self.renderer
            .on_event(&Event::SteerResponse(delta.to_string()));
        let _ = self.renderer.flush();
    }

    fn sleep_notice(&mut self, notice: &SleepNotice) {
        self.renderer.on_event(&Event::Status(notice.status_text()));
        let _ = self.renderer.flush();
    }
}
