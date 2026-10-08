//! `RendererSink`: the `StreamSink` adapter bridging LLM streaming into the renderer.

use super::action;
use super::arbiter::{self, ArbitrationChannel, SleepNotice};
use super::drain::drain_steer_arbitration_events;
use super::steer::{SharedSteeringHistory, SteerArbEvent};
use crate::llm::{ChatClient, PauseAction, StreamControl, StreamEvent, StreamSink};
use crate::ui::helpers::{format_active_subtasks, is_abort_command, is_reset_command};
use crate::ui::{Event, Renderer, SubagentDetail};
use std::sync::Arc;

/// What the paused-stream host asks the sleep chain for, given the `Sleep` decision it renders.
///
/// The duration is resolved by the single owner, [`arbiter::clamped_sleep_secs`] — the owner's
/// default, its `SLEEP_MIN_SECS` floor and the arbitrator's own upper budget knob. This site used
/// to re-type the default (`unwrap_or(5)`) and apply its own upper clamp, which skipped the floor:
/// a steered `"sleep_seconds": 0` reached the host verbatim, so the arbitrator slept
/// `SLEEP_MIN_SECS` while the host announced `"Steering arbitrator sleeping for 0s..."` and then
/// slept nothing — the phantom / hot-loop sleep the arbiter's floor was added to remove. Host and
/// arbitrator now agree on the number by construction (gate t-072).
pub(crate) fn host_sleep_request(decision: Option<&crate::orchestrator::SteerDecision>) -> u64 {
    arbiter::clamped_sleep_secs(decision.and_then(|d| d.sleep_seconds))
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
    pub subagents: &'a [SubagentDetail],
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
        // The arbitration round machine (context snapshot, SYSTEM NOTICE wake-up rounds,
        // preemption, decision application and sleep/re-evaluation) lives in `arbiter`.
        let mut channel = RendererChannel {
            renderer: &mut *self.renderer,
            subagents: self.subagents,
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

        let recorded_resp = arbiter::recorded_response(decision.as_ref(), None, &[]);
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

                let deliverable = arbiter::run_delegated_subtask(
                    self.client,
                    &self.stats,
                    agent,
                    &task_id,
                    &prompt,
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
                    "\n[Steering Subtask — {} ({}):]\n{}\n",
                    agent.as_str(),
                    task_id,
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
                    let target = decision
                        .as_ref()
                        .and_then(|d| {
                            action::first(Some(d), action::ACTION_FORWARD_NOTICE)
                                .map(action::notice_target)
                        })
                        .unwrap_or("worker");
                    let posted = arbiter::post_notice_observable(target, user_msg);
                    let notice = posted.notice;
                    self.renderer.on_event(&Event::Status(format!(
                        "Notice {} forwarded to {} — resuming stream...",
                        notice.notice_id, target
                    )));
                    if !posted.dropped_older.is_empty() {
                        // Backpressure is kept (drop-oldest at INBOX_CAPACITY) but never silent.
                        let drop_text = arbiter::capacity_drop_text(target, &posted.dropped_older);
                        tracing::warn!(target_worker = target, reason = %drop_text, "Steering notice dropped by inbox capacity");
                        self.renderer
                            .on_event(&Event::Status(format!("[Arbitrator] {drop_text}")));
                    }
                    self.steer_queue.push(user_msg.to_string());
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
/// renderer (status events + streamed steer responses).
struct RendererChannel<'a> {
    renderer: &'a mut dyn Renderer,
    subagents: &'a [SubagentDetail],
}

impl ArbitrationChannel for RendererChannel<'_> {
    fn active_subtasks(&self, round: usize) -> String {
        if round == 1 {
            format_active_subtasks(self.subagents)
        } else {
            crate::orchestrator::get_active_subtasks_str()
        }
    }

    fn has_active_work(&self, _round: usize) -> bool {
        // The renderer's subagent table cannot change during a paused turn, so it counts as
        // active work in every round (behaviour kept from the original `on_pause`).
        crate::orchestrator::has_active_workers() || self.subagents.iter().any(|s| s.is_active)
    }

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
