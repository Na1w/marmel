//! Bridge between LLM streaming turns, interactive renderer, and steer arbitration.

use super::helpers::{
    format_active_subtasks, format_plan_progress_summary, is_abort_command, is_reset_command,
};
use super::{Event, Renderer, SubagentDetail};
use crate::llm::{ChatClient, PauseAction, StreamControl, StreamEvent, StreamSink};
use std::sync::Arc;

pub enum SteerArbEvent {
    Delta(String),
    DelegationStarted {
        agent: crate::agents::Agent,
        task_id: String,
        prompt: String,
    },
    DelegationCompleted {
        agent: crate::agents::Agent,
        task_id: String,
        deliverable: crate::agents::Deliverable,
    },
    SynthesizedAnswer {
        user_msg: String,
        answer: String,
    },
    Finished {
        decision: Option<crate::orchestrator::SteerDecision>,
        user_msg: String,
    },
}

pub type SharedSteeringHistory = Arc<std::sync::RwLock<Vec<(String, String)>>>;

#[allow(clippy::too_many_arguments)]
pub fn spawn_steer_arbitration(
    client: &ChatClient,
    stats: Arc<crate::harness::HarnessStats>,
    goal: &str,
    subagents: &[SubagentDetail],
    user_msg: String,
    arb_tx: &tokio::sync::mpsc::UnboundedSender<SteerArbEvent>,
    renderer: &mut dyn Renderer,
    steering_history: Option<SharedSteeringHistory>,
) {
    let client = client.clone();
    let stats = stats.clone();
    let goal = goal.to_string();
    let active_subtasks_str = format_active_subtasks(subagents);
    let initial_has_active =
        crate::orchestrator::has_active_workers() || subagents.iter().any(|s| s.is_active);
    let tx = arb_tx.clone();
    let msg = user_msg.clone();

    renderer.on_event(&Event::Status(
        "Arbitrating user steering instruction...".to_string(),
    ));
    let _ = renderer.flush();

    tokio::spawn(async move {
        let mut loop_count = 0;
        let mut decision = None;
        let mut synthesized_answer_opt = None;

        while loop_count < 5 {
            loop_count += 1;
            let delta_tx = tx.clone();
            let history_str = steering_history
                .as_ref()
                .and_then(|h| h.read().ok())
                .map(|h| crate::orchestrator::format_steering_history(&h))
                .unwrap_or_else(|| "None".to_string());

            let plan_content = crate::manager::phase::Plan::default()
                .read()
                .unwrap_or(None)
                .unwrap_or_default();
            let plan_progress_str = format_plan_progress_summary(&plan_content);
            let active_subtasks_str = if loop_count == 1 {
                active_subtasks_str.clone()
            } else {
                crate::orchestrator::get_active_subtasks_str()
            };
            let has_active = if loop_count == 1 {
                initial_has_active
            } else {
                crate::orchestrator::has_active_workers()
            };

            let effective_msg = if loop_count == 1 {
                msg.clone()
            } else {
                format!(
                    "{msg} (SYSTEM NOTICE: You already slept as requested and have now woken up to re-evaluate. Inspect the updated Active Subtasks and Plan Progress above and deliver your direct factual response or action now.)"
                )
            };

            let ctx = crate::orchestrator::steer::SteerContext {
                main_goal: &goal,
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
                crate::orchestrator::preempt_conflicting_stream(client.model(), &effective_msg)
                    .await;

            let cur_decision = crate::orchestrator::steer::arbitrate_steer_context_stream(
                &client,
                &stats,
                ctx,
                move |delta| {
                    let _ = delta_tx.send(SteerArbEvent::Delta(delta.to_string()));
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
                let _ = tx.send(SteerArbEvent::Delta(format!(
                    "\n[Steering Arbitrator sleeping for {sleep_secs}s...]\n"
                )));
                if let Some(ref hist_lock) = steering_history
                    && let Ok(mut hist) = hist_lock.write()
                {
                    let note = d.response.as_deref().unwrap_or("Slept");
                    hist.push((msg.clone(), format!("{note} (slept for {sleep_secs}s)")));
                }
                let cancel = crate::orchestrator::bus::global_cancellation_token();
                let was_cancelled = tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)) => {
                        let _ = tx.send(SteerArbEvent::Delta(format!(
                            "[Steering Arbitrator woke up after {sleep_secs}s — re-evaluating status...]\n\n"
                        )));
                        false
                    }
                    _ = cancel.cancelled() => {
                        let _ = tx.send(SteerArbEvent::Delta(
                            "[Steering Arbitrator sleep cancelled]\n".to_string()
                        ));
                        true
                    }
                };
                if was_cancelled {
                    break;
                }
                continue;
            }

            break;
        }

        if let Some(ref d) = decision {
            let tasks = crate::orchestrator::steer::extract_tasks_to_delegate(d, &msg);
            let mut completed_deliverables = Vec::new();
            for (agent, task_id, prompt) in tasks {
                let _ = tx.send(SteerArbEvent::DelegationStarted {
                    agent,
                    task_id: task_id.clone(),
                    prompt: prompt.clone(),
                });
                let res = crate::orchestrator::steer::execute_steer_subtask(
                    &client,
                    stats.clone(),
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
                let _ = tx.send(SteerArbEvent::DelegationCompleted {
                    agent,
                    task_id: task_id.clone(),
                    deliverable: deliverable.clone(),
                });
                completed_deliverables.push((agent, task_id, deliverable));
            }

            if !completed_deliverables.is_empty() {
                let delta_tx = tx.clone();
                let _ = delta_tx.send(SteerArbEvent::Delta("\n".to_string()));
                let synth_res = crate::orchestrator::steer::synthesize_steer_subtask_response(
                    &client,
                    &stats,
                    &msg,
                    &completed_deliverables,
                    move |delta| {
                        let _ = delta_tx.send(SteerArbEvent::Delta(delta.to_string()));
                    },
                )
                .await;
                if let Ok(synthesized) = synth_res {
                    synthesized_answer_opt = Some(synthesized.clone());
                    let _ = tx.send(SteerArbEvent::SynthesizedAnswer {
                        user_msg: msg.clone(),
                        answer: synthesized,
                    });
                }
            }
        }

        let recorded_resp = if let Some(ref synth) = synthesized_answer_opt {
            synth.clone()
        } else if let Some(ref d) = decision {
            if let Some(ref r) = d.response {
                r.clone()
            } else if crate::orchestrator::normalize_steer_decision(Some(&d.decision)) == "Sleep" {
                format!("Slept for {}s", d.sleep_seconds.unwrap_or(5))
            } else {
                format!("Decision: {}", d.decision)
            }
        } else {
            "No decision".to_string()
        };

        if let Some(ref hist_lock) = steering_history
            && let Ok(mut hist) = hist_lock.write()
        {
            hist.push((msg.clone(), recorded_resp));
        }

        let _ = tx.send(SteerArbEvent::Finished {
            decision,
            user_msg: msg,
        });
    });
}

pub(crate) fn drain_steer_arbitration_events(
    arb_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SteerArbEvent>,
    renderer: &mut dyn Renderer,
    steer_queue: &mut Vec<String>,
    steer_abort_requested: &mut bool,
    mut subagents: Option<&mut Vec<SubagentDetail>>,
) {
    let mut dirty = false;
    while let Ok(ev) = arb_rx.try_recv() {
        dirty = true;
        match ev {
            SteerArbEvent::Delta(delta) => {
                renderer.on_event(&Event::SteerResponse(delta));
            }
            SteerArbEvent::DelegationStarted {
                agent,
                task_id,
                prompt,
            } => {
                renderer.on_event(&Event::Delegation(
                    crate::orchestrator::DelegationEvent::Started {
                        agent,
                        task: Some(task_id.clone()),
                    },
                ));
                renderer.on_event(&Event::Status(format!(
                    "Steering delegated subtask '{task_id}' to {}...",
                    agent.as_str()
                )));
                if let Some(sub) = subagents.as_deref_mut() {
                    crate::ui::helpers::update_subagent_lifecycle(
                        sub,
                        agent,
                        Some(task_id),
                        Some(prompt),
                        true,
                    );
                    renderer.set_subagents(sub.clone());
                }
            }
            SteerArbEvent::DelegationCompleted {
                agent,
                task_id,
                deliverable,
            } => {
                renderer.on_event(&Event::Delegation(
                    crate::orchestrator::DelegationEvent::Completed {
                        agent,
                        task: Some(task_id.clone()),
                    },
                ));
                renderer.on_event(&Event::Message(format!(
                    "\n[Steering Subtask — {} ({}):]\n{}\n",
                    agent.as_str(),
                    task_id,
                    deliverable.content
                )));
                renderer.on_event(&Event::Status(format!(
                    "Steering subtask '{}' completed by {}",
                    task_id,
                    agent.as_str()
                )));
                if let Some(sub) = subagents.as_deref_mut() {
                    crate::ui::helpers::update_subagent_lifecycle(
                        sub,
                        agent,
                        Some(task_id.clone()),
                        None,
                        false,
                    );
                    renderer.set_subagents(sub.clone());
                }
                steer_queue.push(format!(
                    "(User steering resulted in subtask '{task_id}' executed by specialist '{}'. Deliverable:\n{})",
                    agent.as_str(),
                    deliverable.content
                ));
            }
            SteerArbEvent::SynthesizedAnswer { .. } => {
                // User steering inquiry was answered directly to the user by arbitrator.
                // Do NOT push to steer_queue so it does not trigger an orchestrator turn.
            }
            SteerArbEvent::Finished { decision, user_msg } => {
                if let Some(ref d) = decision {
                    for st in &d.subtasks {
                        if st.action.eq_ignore_ascii_case("Cancel") {
                            let target = st.agent_name.as_deref().unwrap_or(&st.tool_call_id);
                            let cancelled = crate::orchestrator::cancel_active_worker(
                                st.agent_name.as_deref(),
                                Some(&st.tool_call_id),
                            );
                            if cancelled {
                                renderer.on_event(&Event::Status(format!(
                                    "[Arbitrator] Cancelled specialist subagent '{target}'"
                                )));
                            }
                            if let Some(sub) = subagents.as_deref_mut() {
                                for s in sub.iter_mut() {
                                    let matches_id = s.task_id.as_deref() == Some(&st.tool_call_id);
                                    let matches_agent = st
                                        .agent_name
                                        .as_ref()
                                        .map(|a| a.eq_ignore_ascii_case(&s.name))
                                        .unwrap_or(false);
                                    if s.is_active && (matches_id || matches_agent) {
                                        s.is_active = false;
                                        s.logs.push("[cancelled by arbitrator]".to_string());
                                    }
                                }
                                renderer.set_subagents(sub.clone());
                            }
                        }
                    }
                }

                let has_delegations = decision
                    .as_ref()
                    .map(|d| {
                        crate::orchestrator::normalize_steer_decision(Some(&d.decision))
                            == "DelegateTask"
                            || d.subtasks
                                .iter()
                                .any(|s| s.action.eq_ignore_ascii_case("DelegateTask"))
                    })
                    .unwrap_or(false);

                if has_delegations {
                    renderer.on_event(&Event::Status(
                        "Steering subtask delegation finished".to_string(),
                    ));
                } else {
                    let norm = crate::orchestrator::normalize_steer_decision(
                        decision.as_ref().map(|d| d.decision.as_str()),
                    );
                    match norm {
                        "RespondDirectly" => {
                            renderer.on_event(&Event::Status(
                                "Answered via direct steer response".to_string(),
                            ));
                        }
                        "Sleep" => {
                            renderer.on_event(&Event::Status(
                                "Steering arbitrator sleep completed".to_string(),
                            ));
                        }
                        "AbortImmediately" => {
                            crate::orchestrator::cancel_all();
                            renderer.request_abort();
                            *steer_abort_requested = true;
                            steer_queue.push(user_msg);
                        }
                        "ForwardToWorker" => {
                            steer_queue.push(user_msg);
                            renderer.on_event(&Event::Status(
                                "Notice forwarded to specialist".to_string(),
                            ));
                        }
                        "ApprovePlan" => {
                            steer_queue.push("User approved plan.".to_string());
                        }
                        "RejectPlan" => {
                            crate::orchestrator::cancel_all();
                            renderer.request_abort();
                            *steer_abort_requested = true;
                            steer_queue.push(format!("User rejected plan: {user_msg}"));
                        }
                        "QueueAndContinue" => {
                            steer_queue.push(user_msg);
                            if decision
                                .as_ref()
                                .and_then(|d| d.response.as_ref())
                                .is_none()
                            {
                                renderer.on_event(&Event::SteerResponse(
                                    "Instruction queued for next turn while active tasks continue.\n".to_string(),
                                ));
                            }
                            renderer.on_event(&Event::Status(
                                "Instruction queued for next turn".to_string(),
                            ));
                        }
                        _ => {
                            let has_response = decision
                                .as_ref()
                                .and_then(|d| d.response.as_ref())
                                .is_some();
                            if !has_response {
                                steer_queue.push(user_msg);
                                renderer.on_event(&Event::Status(
                                    "Instruction queued for next turn".to_string(),
                                ));
                            } else {
                                renderer.on_event(&Event::Status(
                                    "Answered via direct steer response".to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    if dirty {
        let _ = renderer.flush();
    }
}

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
            StreamEvent::Content(text) => self.renderer.on_event(&Event::Message(text)),
            StreamEvent::Thinking(text) => self.renderer.on_event(&Event::Thinking(text)),
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
                    self.steer_queue.push(user_msg.to_string());
                    self.renderer.on_event(&Event::Status(
                        "Notice queued for worker — resuming stream...".to_string(),
                    ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{Agent, Deliverable, MissionMarker};
    use crate::ui::Event;

    struct TestRenderer {
        events: Vec<Event>,
        aborted: bool,
    }
    impl TestRenderer {
        fn new() -> Self {
            Self {
                events: Vec::new(),
                aborted: false,
            }
        }
    }
    impl Renderer for TestRenderer {
        fn init(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn on_event(&mut self, event: &Event) {
            self.events.push(event.clone());
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll_input(&mut self) -> Option<String> {
            None
        }
        fn read_input(&mut self) -> Option<String> {
            None
        }
        fn request_abort(&mut self) {
            self.aborted = true;
        }
        fn aborted(&self) -> bool {
            self.aborted
        }
        fn shutdown(&mut self) {}
    }

    #[test]
    fn test_drain_steer_delegation_events_updates_ui_and_queue() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;
        let mut subagents = Vec::new();

        tx.send(SteerArbEvent::DelegationStarted {
            agent: Agent::Coder,
            task_id: "steer-task-1".to_string(),
            prompt: "Check files".to_string(),
        })
        .unwrap();

        tx.send(SteerArbEvent::DelegationCompleted {
            agent: Agent::Coder,
            task_id: "steer-task-1".to_string(),
            deliverable: Deliverable {
                marker: MissionMarker::Complete {
                    task_id: Some("steer-task-1".to_string()),
                },
                content: "All files inspected cleanly.".to_string(),
                task_id: Some("steer-task-1".to_string()),
            },
        })
        .unwrap();

        tx.send(SteerArbEvent::Finished {
            decision: Some(crate::orchestrator::SteerDecision {
                decision: "DelegateTask".to_string(),
                response: Some("Delegated task to coder.".to_string()),
                tier: None,
                model: None,
                subtasks: vec![crate::orchestrator::SteerSubtaskDecision {
                    tool_call_id: "steer-task-1".to_string(),
                    action: "DelegateTask".to_string(),
                    message: None,
                    agent_name: Some("coder".to_string()),
                    prompt: Some("Check files".to_string()),
                    sleep_seconds: None,
                }],
                sleep_seconds: None,
            }),
            user_msg: "check files".to_string(),
        })
        .unwrap();

        drain_steer_arbitration_events(
            &mut rx,
            &mut renderer,
            &mut steer_queue,
            &mut steer_abort,
            Some(&mut subagents),
        );

        // Subagents lifecycle should have updated
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].name, "coder-steer-task-1");
        assert!(!subagents[0].is_active);

        // Steer queue should receive the deliverable summary
        assert_eq!(steer_queue.len(), 1);
        assert!(steer_queue[0].contains("All files inspected cleanly."));
        assert!(steer_queue[0].contains("steer-task-1"));

        // Delegation Started and Completed events surfaced to renderer
        let delegation_events: Vec<_> = renderer
            .events
            .iter()
            .filter_map(|e| {
                if let Event::Delegation(de) = e {
                    Some(de)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(delegation_events.len(), 2);
        assert!(matches!(
            delegation_events[0],
            crate::orchestrator::DelegationEvent::Started {
                agent: Agent::Coder,
                ..
            }
        ));
        assert!(matches!(
            delegation_events[1],
            crate::orchestrator::DelegationEvent::Completed {
                agent: Agent::Coder,
                ..
            }
        ));

        // Delivered message should be visible to user
        assert!(renderer.events.iter().any(|e| matches!(
            e,
            Event::Message(m) if m.contains("All files inspected cleanly.")
        )));
    }

    #[test]
    fn test_drain_steer_queue_and_continue_with_streamed_response() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;

        // The streaming arbitrator streams the user-facing explanation first
        tx.send(SteerArbEvent::Delta(
            "Instruktionen har köats för nästa tur.".to_string(),
        ))
        .unwrap();

        // Then completes with QueueAndContinue decision
        tx.send(SteerArbEvent::Finished {
            decision: Some(crate::orchestrator::SteerDecision {
                decision: "QueueAndContinue".to_string(),
                response: Some("Instruktionen har köats för nästa tur.".to_string()),
                tier: None,
                model: None,
                subtasks: Vec::new(),
                sleep_seconds: None,
            }),
            user_msg: "lägg till en extra feature sen".to_string(),
        })
        .unwrap();

        drain_steer_arbitration_events(
            &mut rx,
            &mut renderer,
            &mut steer_queue,
            &mut steer_abort,
            None,
        );

        assert_eq!(
            steer_queue,
            vec!["lägg till en extra feature sen".to_string()]
        );
        assert!(!steer_abort);

        // Verify the explanation was sent to the renderer as SteerResponse
        let steer_responses: Vec<_> = renderer
            .events
            .iter()
            .filter_map(|e| match e {
                Event::SteerResponse(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            steer_responses,
            vec!["Instruktionen har köats för nästa tur."]
        );

        // Verify status was also emitted
        assert!(renderer.events.iter().any(|e| matches!(
            e,
            Event::Status(s) if s.contains("Instruction queued for next turn")
        )));
    }

    #[test]
    fn test_drain_steer_queue_and_continue_fallback_response_when_none() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;

        // Model returned QueueAndContinue without a response field (null)
        tx.send(SteerArbEvent::Finished {
            decision: Some(crate::orchestrator::SteerDecision {
                decision: "QueueAndContinue".to_string(),
                response: None,
                tier: None,
                model: None,
                subtasks: Vec::new(),
                sleep_seconds: None,
            }),
            user_msg: "fix docs later".to_string(),
        })
        .unwrap();

        drain_steer_arbitration_events(
            &mut rx,
            &mut renderer,
            &mut steer_queue,
            &mut steer_abort,
            None,
        );

        assert_eq!(steer_queue, vec!["fix docs later".to_string()]);
        assert!(!steer_abort);

        // Fallback SteerResponse should be emitted informing the user
        let steer_responses: Vec<_> = renderer
            .events
            .iter()
            .filter_map(|e| match e {
                Event::SteerResponse(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(steer_responses.len(), 1);
        assert!(steer_responses[0].contains("Instruction queued for next turn"));

        // Status line was also emitted
        assert!(renderer.events.iter().any(|e| matches!(
            e,
            Event::Status(s) if s.contains("Instruction queued for next turn")
        )));
    }

    #[test]
    fn test_drain_steer_respond_directly_does_not_queue() {
        for decision_str in ["RespondDirectly", "respond_directly", "Respond Directly"] {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let mut renderer = TestRenderer::new();
            let mut steer_queue = Vec::new();
            let mut steer_abort = false;

            tx.send(SteerArbEvent::Finished {
                decision: Some(crate::orchestrator::SteerDecision {
                    decision: decision_str.to_string(),
                    response: Some("Coder arbetar med tester.".to_string()),
                    tier: None,
                    model: None,
                    subtasks: Vec::new(),
                    sleep_seconds: None,
                }),
                user_msg: "hur går det?".to_string(),
            })
            .unwrap();

            drain_steer_arbitration_events(
                &mut rx,
                &mut renderer,
                &mut steer_queue,
                &mut steer_abort,
                None,
            );

            assert!(
                steer_queue.is_empty(),
                "Expected steer_queue to be empty for {decision_str}, but got: {steer_queue:?}"
            );
            assert!(!steer_abort);
            assert!(renderer.events.iter().any(|e| matches!(
                e,
                Event::Status(s) if s.contains("Answered via direct steer response")
            )));
        }
    }

    #[test]
    fn test_drain_steer_synthesized_answer_does_not_queue() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;

        tx.send(SteerArbEvent::SynthesizedAnswer {
            user_msg: "vad gör subagenten?".to_string(),
            answer: "Subagenten kör cargo check.".to_string(),
        })
        .unwrap();

        tx.send(SteerArbEvent::Finished {
            decision: Some(crate::orchestrator::SteerDecision {
                decision: "RespondDirectly".to_string(),
                response: Some("Subagenten kör cargo check.".to_string()),
                tier: None,
                model: None,
                subtasks: Vec::new(),
                sleep_seconds: None,
            }),
            user_msg: "vad gör subagenten?".to_string(),
        })
        .unwrap();

        drain_steer_arbitration_events(
            &mut rx,
            &mut renderer,
            &mut steer_queue,
            &mut steer_abort,
            None,
        );

        assert!(
            steer_queue.is_empty(),
            "Synthesized answer must not be pushed to steer_queue, got: {steer_queue:?}"
        );
        assert!(!steer_abort);
    }

    #[test]
    fn test_drain_steer_sleep_does_not_queue() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;

        tx.send(SteerArbEvent::Finished {
            decision: Some(crate::orchestrator::SteerDecision {
                decision: "Sleep".to_string(),
                response: Some("Väntar 5 sekunder...".to_string()),
                tier: None,
                model: None,
                subtasks: Vec::new(),
                sleep_seconds: Some(5),
            }),
            user_msg: "vänta lite".to_string(),
        })
        .unwrap();

        drain_steer_arbitration_events(
            &mut rx,
            &mut renderer,
            &mut steer_queue,
            &mut steer_abort,
            None,
        );

        assert!(
            steer_queue.is_empty(),
            "Sleep decision must not be pushed to steer_queue, got: {steer_queue:?}"
        );
        assert!(!steer_abort);
        assert!(renderer.events.iter().any(|e| matches!(
            e,
            Event::Status(s) if s.contains("Steering arbitrator sleep completed")
        )));
    }

    #[test]
    fn test_drain_steer_fallback_with_response_does_not_queue() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;

        // Unrecognized decision name, but with a valid direct response string
        tx.send(SteerArbEvent::Finished {
            decision: Some(crate::orchestrator::SteerDecision {
                decision: "InformationalRemark".to_string(),
                response: Some("All tasks are on track.".to_string()),
                tier: None,
                model: None,
                subtasks: Vec::new(),
                sleep_seconds: None,
            }),
            user_msg: "status?".to_string(),
        })
        .unwrap();

        drain_steer_arbitration_events(
            &mut rx,
            &mut renderer,
            &mut steer_queue,
            &mut steer_abort,
            None,
        );

        assert!(
            steer_queue.is_empty(),
            "Answered inquiry with unknown decision name must not be queued, got: {steer_queue:?}"
        );
        assert!(!steer_abort);
        assert!(renderer.events.iter().any(|e| matches!(
            e,
            Event::Status(s) if s.contains("Answered via direct steer response")
        )));
    }

    #[test]
    fn test_drain_steer_cancel_subtask_cancels_worker_and_updates_subagents() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;

        let token = tokio_util::sync::CancellationToken::new();
        let guard = crate::orchestrator::register_active_worker_with_token(
            Some("t-steer-cancel-001".to_string()),
            "coder".to_string(),
            "Running long build".to_string(),
            Some(token.clone()),
        );

        let mut subagents = vec![SubagentDetail {
            name: "coder".to_string(),
            task_id: Some("t-steer-cancel-001".to_string()),
            prompt: "Running long build".to_string(),
            is_active: true,
            ..Default::default()
        }];

        assert!(!token.is_cancelled());

        tx.send(SteerArbEvent::Finished {
            decision: Some(crate::orchestrator::SteerDecision {
                decision: "ForwardToWorker".to_string(),
                response: None,
                tier: None,
                model: None,
                subtasks: vec![crate::orchestrator::SteerSubtaskDecision {
                    tool_call_id: "t-steer-cancel-001".to_string(),
                    action: "Cancel".to_string(),
                    message: None,
                    agent_name: Some("coder".to_string()),
                    prompt: None,
                    sleep_seconds: None,
                }],
                sleep_seconds: None,
            }),
            user_msg: "cancel coder".to_string(),
        })
        .unwrap();

        drain_steer_arbitration_events(
            &mut rx,
            &mut renderer,
            &mut steer_queue,
            &mut steer_abort,
            Some(&mut subagents),
        );

        assert!(
            token.is_cancelled(),
            "Active worker token must be cancelled"
        );
        assert!(
            !subagents[0].is_active,
            "Subagent in UI must be marked inactive"
        );
        assert!(
            subagents[0]
                .logs
                .iter()
                .any(|l| l.contains("cancelled by arbitrator")),
            "Subagent logs should contain cancellation note"
        );

        drop(guard);
    }

    #[test]
    fn test_drain_steer_abort_cancels_all_active_workers() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;

        let token = tokio_util::sync::CancellationToken::new();
        let guard = crate::orchestrator::register_active_worker_with_token(
            Some("t-steer-abort-002".to_string()),
            "researcher".to_string(),
            "Researching...".to_string(),
            Some(token.clone()),
        );

        assert!(!token.is_cancelled());

        tx.send(SteerArbEvent::Finished {
            decision: Some(crate::orchestrator::SteerDecision {
                decision: "AbortImmediately".to_string(),
                response: Some("Aborting all!".to_string()),
                tier: None,
                model: None,
                subtasks: Vec::new(),
                sleep_seconds: None,
            }),
            user_msg: "stop all".to_string(),
        })
        .unwrap();

        drain_steer_arbitration_events(
            &mut rx,
            &mut renderer,
            &mut steer_queue,
            &mut steer_abort,
            None,
        );

        assert!(steer_abort);
        assert!(renderer.aborted());
        assert!(
            token.is_cancelled(),
            "Active worker token must be cancelled on AbortImmediately"
        );

        drop(guard);
        crate::orchestrator::reset_cancellation();
    }
}
