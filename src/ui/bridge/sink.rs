//! `RendererSink`: the `StreamSink` adapter bridging LLM streaming into the renderer.

use super::drain::drain_steer_arbitration_events;
use super::steer::{SharedSteeringHistory, SteerArbEvent};
use crate::llm::{ChatClient, PauseAction, StreamControl, StreamEvent, StreamSink};
use crate::ui::helpers::{
    format_active_subtasks, format_plan_progress_summary, is_abort_command, is_reset_command,
};
use crate::ui::{Event, Renderer, SubagentDetail};
use std::sync::Arc;

pub struct RendererSink<'a> {
    pub renderer: &'a mut dyn Renderer,
    pub steer_queue: &'a mut Vec<String>,
    pub steer_abort_requested: &'a mut bool,
    #[allow(dead_code)]
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
        let mut loop_count = 0;
        let mut decision = None;

        while loop_count < 5 {
            loop_count += 1;
            self.renderer.on_event(&Event::Status(
                "Stream paused — evaluating steering instruction...".to_string(),
            ));
            let _ = self.renderer.flush();

            let plan_content = crate::manager::phase::Plan::default()
                .read()
                .unwrap_or(None)
                .unwrap_or_default();
            let active_subtasks_str = if loop_count == 1 {
                format_active_subtasks(self.subagents)
            } else {
                crate::orchestrator::get_active_subtasks_str()
            };
            let plan_progress_str = format_plan_progress_summary(&plan_content);

            let has_active = crate::orchestrator::has_active_workers()
                || self.subagents.iter().any(|s| s.is_active);
            let history_str = self
                .steering_history
                .as_ref()
                .and_then(|h| h.read().ok())
                .map(|h| crate::orchestrator::format_steering_history(&h))
                .unwrap_or_else(|| "None".to_string());

            let effective_msg = if loop_count == 1 {
                user_msg.to_string()
            } else {
                format!(
                    "{user_msg} (SYSTEM NOTICE: You already slept as requested and have now woken up to re-evaluate. Inspect the updated Active Subtasks and Plan Progress above and deliver your direct factual response or action now.)"
                )
            };

            let ctx = crate::orchestrator::steer::SteerContext {
                main_goal: self.goal,
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

            let preempt_handle = crate::orchestrator::preempt_conflicting_stream(
                self.client.model(),
                &effective_msg,
            )
            .await;

            let renderer = &mut *self.renderer;
            let cur_decision = crate::orchestrator::steer::arbitrate_steer_context_stream(
                self.client,
                &self.stats,
                ctx,
                |delta| {
                    renderer.on_event(&Event::SteerResponse(delta.to_string()));
                    let _ = renderer.flush();
                },
            )
            .await;

            let is_global_abort = matches!(
                crate::orchestrator::normalize_steer_decision(
                    cur_decision.as_ref().map(|d| d.decision.as_str())
                ),
                "AbortImmediately" | "RejectPlan"
            );

            if is_global_abort {
                preempt_handle.complete_all(PauseAction::Abort);
                crate::orchestrator::cancel_all();
            } else {
                preempt_handle.complete_with_subtask_decision(cur_decision.as_ref());
                if let Some(ref d) = cur_decision {
                    for st in &d.subtasks {
                        if st.action.eq_ignore_ascii_case("Cancel") {
                            crate::orchestrator::cancel_active_worker(
                                st.agent_name.as_deref(),
                                Some(&st.tool_call_id),
                            );
                        }
                    }
                }
            }

            decision = cur_decision;

            if let Some(ref d) = decision
                && crate::orchestrator::normalize_steer_decision(Some(&d.decision)) == "Sleep"
            {
                let sleep_secs = d.sleep_seconds.unwrap_or(5).min(300);
                self.renderer.on_event(&Event::Status(format!(
                    "Steering arbitrator sleeping for {sleep_secs}s..."
                )));
                let _ = self.renderer.flush();
                if let Some(ref hist_lock) = self.steering_history
                    && let Ok(mut hist) = hist_lock.write()
                {
                    let note = d.response.as_deref().unwrap_or("Slept");
                    hist.push((
                        user_msg.to_string(),
                        format!("{note} (slept for {sleep_secs}s)"),
                    ));
                }
                let cancel = crate::orchestrator::bus::global_cancellation_token();
                let was_cancelled = tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)) => {
                        self.renderer.on_event(&Event::Status(format!(
                            "Steering arbitrator woke up after {sleep_secs}s — re-evaluating status..."
                        )));
                        false
                    }
                    _ = cancel.cancelled() => {
                        self.renderer.on_event(&Event::Status(
                            "Steering arbitrator sleep cancelled".to_string(),
                        ));
                        true
                    }
                };
                let _ = self.renderer.flush();
                if was_cancelled {
                    return PauseAction::Resume;
                }
                continue;
            }

            break;
        }

        let recorded_resp = if let Some(ref d) = decision {
            if let Some(ref r) = d.response {
                r.clone()
            } else if crate::orchestrator::normalize_steer_decision(Some(&d.decision)) == "Sleep" {
                format!("Slept for {}s", d.sleep_seconds.unwrap_or(5))
            } else if crate::orchestrator::normalize_steer_decision(Some(&d.decision))
                == "ForwardToWorker"
            {
                "Forwarded notice to worker (awaiting specialist reply)".to_string()
            } else {
                format!("Decision: {}", d.decision)
            }
        } else {
            "No decision".to_string()
        };

        if let Some(ref hist_lock) = self.steering_history
            && let Ok(mut hist) = hist_lock.write()
        {
            hist.push((user_msg.to_string(), recorded_resp));
        }

        let tasks = decision
            .as_ref()
            .map(|d| crate::orchestrator::steer::extract_tasks_to_delegate(d, user_msg))
            .unwrap_or_default();

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

                let res = crate::orchestrator::steer::execute_steer_subtask(
                    self.client,
                    self.stats.clone(),
                    agent,
                    Some(task_id.clone()),
                    &prompt,
                )
                .await;

                let deliverable = match res {
                    Ok(deliv) => deliv,
                    Err(e) => crate::agents::Deliverable {
                        marker: crate::agents::MissionMarker::Failed {
                            reason: e.to_string(),
                        },
                        content: format!("Execution failed: {e}"),
                        task_id: Some(task_id.clone()),
                    },
                };

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
                    let sleep_secs = decision
                        .as_ref()
                        .and_then(|d| d.sleep_seconds)
                        .unwrap_or(5)
                        .min(300);
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
                            d.subtasks.iter().find_map(|st| {
                                if st.action.eq_ignore_ascii_case("ForwardNotice") {
                                    st.agent_name.as_deref().or(Some(&st.tool_call_id))
                                } else {
                                    None
                                }
                            })
                        })
                        .unwrap_or("worker");
                    let notice = crate::orchestrator::post_notice_to_worker(target, user_msg, None);
                    self.renderer.on_event(&Event::Status(format!(
                        "Notice {} forwarded to {} — resuming stream...",
                        notice.notice_id, target
                    )));
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
