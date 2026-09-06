//! Bridge between LLM streaming turns, interactive renderer, and steer arbitration.

use super::helpers::{
    format_active_subtasks, format_plan_progress_summary, is_abort_command, is_reset_command,
};
use super::{Event, Renderer, SubagentDetail};
use crate::llm::{ChatClient, PauseAction, StreamControl, StreamEvent, StreamSink};
use std::sync::Arc;

pub(crate) enum SteerArbEvent {
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

pub(crate) fn spawn_steer_arbitration(
    client: &ChatClient,
    stats: Arc<crate::harness::HarnessStats>,
    goal: &str,
    subagents: &[SubagentDetail],
    user_msg: String,
    arb_tx: &tokio::sync::mpsc::UnboundedSender<SteerArbEvent>,
    renderer: &mut dyn Renderer,
) {
    let client = client.clone();
    let stats = stats.clone();
    let goal = goal.to_string();
    let plan_content = crate::agent::phase::Plan::default()
        .read()
        .unwrap_or(None)
        .unwrap_or_default();
    let active_subtasks_str = format_active_subtasks(subagents);
    let plan_progress_str = format_plan_progress_summary(&plan_content);
    let has_active =
        crate::orchestrator::has_active_workers() || subagents.iter().any(|s| s.is_active);
    let tx = arb_tx.clone();
    let msg = user_msg.clone();

    renderer.on_event(&Event::Status(
        "Arbitrating user steering instruction...".to_string(),
    ));
    let _ = renderer.flush();

    tokio::spawn(async move {
        let delta_tx = tx.clone();
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
            steering_history: "None",
            user_message: &msg,
            active_subtasks: &active_subtasks_str,
        };
        let preempt_handle =
            crate::orchestrator::preempt_conflicting_stream(client.model(), &msg).await;

        let decision = crate::orchestrator::steer::arbitrate_steer_context_stream(
            &client,
            &stats,
            ctx,
            move |delta| {
                let _ = delta_tx.send(SteerArbEvent::Delta(delta.to_string()));
            },
        )
        .await;

        let is_global_abort = matches!(
            decision.as_ref().map(|d| d.decision.as_str()),
            Some("AbortImmediately") | Some("RejectPlan")
        );

        if is_global_abort {
            preempt_handle.complete_all(PauseAction::Abort);
        } else {
            preempt_handle.complete_with_subtask_decision(decision.as_ref());
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
                    let _ = tx.send(SteerArbEvent::SynthesizedAnswer {
                        user_msg: msg.clone(),
                        answer: synthesized,
                    });
                }
            }
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
    while let Ok(ev) = arb_rx.try_recv() {
        match ev {
            SteerArbEvent::Delta(delta) => {
                renderer.on_event(&Event::SteerResponse(delta));
                let _ = renderer.flush();
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
                let _ = renderer.flush();
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
                let _ = renderer.flush();
            }
            SteerArbEvent::SynthesizedAnswer { user_msg, answer } => {
                steer_queue.push(format!(
                    "(User steering inquiry: '{user_msg}'. Arbitrator answered user directly: {answer})"
                ));
            }
            SteerArbEvent::Finished { decision, user_msg } => {
                let has_delegations = decision
                    .as_ref()
                    .map(|d| {
                        d.decision.eq_ignore_ascii_case("DelegateTask")
                            || d.subtasks
                                .iter()
                                .any(|s| s.action.eq_ignore_ascii_case("DelegateTask"))
                    })
                    .unwrap_or(false);

                if has_delegations {
                    renderer.on_event(&Event::Status(
                        "Steering subtask delegation finished".to_string(),
                    ));
                    let _ = renderer.flush();
                } else {
                    match decision.as_ref().map(|d| d.decision.as_str()) {
                        Some("RespondDirectly") => {
                            renderer.on_event(&Event::Status(
                                "Answered via direct steer response".to_string(),
                            ));
                            let _ = renderer.flush();
                        }
                        Some("AbortImmediately") => {
                            renderer.request_abort();
                            *steer_abort_requested = true;
                            steer_queue.push(user_msg);
                        }
                        Some("ForwardToWorker") => {
                            steer_queue.push(user_msg);
                            renderer.on_event(&Event::Status(
                                "Notice forwarded to specialist".to_string(),
                            ));
                            let _ = renderer.flush();
                        }
                        Some("ApprovePlan") => {
                            steer_queue.push("User approved plan.".to_string());
                        }
                        Some("RejectPlan") => {
                            renderer.request_abort();
                            *steer_abort_requested = true;
                            steer_queue.push(format!("User rejected plan: {user_msg}"));
                        }
                        _ => {
                            steer_queue.push(user_msg);
                            renderer.on_event(&Event::Status(
                                "Instruction queued for next turn".to_string(),
                            ));
                            let _ = renderer.flush();
                        }
                    }
                }
            }
        }
    }
}

pub(crate) struct RendererSink<'a> {
    pub(crate) renderer: &'a mut dyn Renderer,
    pub(crate) steer_queue: &'a mut Vec<String>,
    pub(crate) steer_abort_requested: &'a mut bool,
    #[allow(dead_code)]
    pub(crate) arb_tx: &'a tokio::sync::mpsc::UnboundedSender<SteerArbEvent>,
    pub(crate) arb_rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<SteerArbEvent>,
    pub(crate) client: &'a ChatClient,
    pub(crate) stats: Arc<crate::harness::HarnessStats>,
    pub(crate) goal: &'a str,
    pub(crate) subagents: &'a [SubagentDetail],
    pub(crate) plan: Option<&'a crate::agent::phase::Plan>,
    pub(crate) ctx: Option<&'a mut crate::manager::context::ContextEngine>,
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
                self.renderer.request_abort();
                return StreamControl::Abort;
            } else if is_reset_command(&input) {
                crate::debug_log::log_user_input("command", &input);
                let default_plan = crate::agent::phase::Plan::default();
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
        self.renderer.on_event(&Event::Status(
            "Stream paused — evaluating steering instruction...".to_string(),
        ));
        let _ = self.renderer.flush();

        let plan_content = crate::agent::phase::Plan::default()
            .read()
            .unwrap_or(None)
            .unwrap_or_default();
        let active_subtasks_str = format_active_subtasks(self.subagents);
        let plan_progress_str = format_plan_progress_summary(&plan_content);

        let has_active =
            crate::orchestrator::has_active_workers() || self.subagents.iter().any(|s| s.is_active);
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
            steering_history: "None",
            user_message: user_msg,
            active_subtasks: &active_subtasks_str,
        };

        let renderer = &mut *self.renderer;
        let decision = crate::orchestrator::steer::arbitrate_steer_context_stream(
            self.client,
            &self.stats,
            ctx,
            |delta| {
                renderer.on_event(&Event::SteerResponse(delta.to_string()));
                let _ = renderer.flush();
            },
        )
        .await;

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
            match decision.as_ref().map(|d| d.decision.as_str()) {
                Some("RespondDirectly") => {
                    self.renderer.on_event(&Event::Status(
                        "Answered via direct steer response — resuming stream...".to_string(),
                    ));
                    let _ = self.renderer.flush();
                    PauseAction::Resume
                }
                Some("AbortImmediately") => {
                    self.renderer.request_abort();
                    *self.steer_abort_requested = true;
                    self.steer_queue.push(user_msg.to_string());
                    self.renderer.on_event(&Event::Status(
                        "Steering requested immediate abort".to_string(),
                    ));
                    let _ = self.renderer.flush();
                    PauseAction::Abort
                }
                Some("RejectPlan") => {
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
                Some("ForwardToWorker") => {
                    self.steer_queue.push(user_msg.to_string());
                    self.renderer.on_event(&Event::Status(
                        "Notice queued for worker — resuming stream...".to_string(),
                    ));
                    let _ = self.renderer.flush();
                    PauseAction::Resume
                }
                Some("ApprovePlan") => {
                    self.steer_queue.push("User approved plan.".to_string());
                    self.renderer.on_event(&Event::Status(
                        "Plan approved — resuming stream...".to_string(),
                    ));
                    let _ = self.renderer.flush();
                    PauseAction::Resume
                }
                _ => {
                    self.steer_queue.push(user_msg.to_string());
                    self.renderer.on_event(&Event::Status(
                        "Instruction queued for next turn — resuming stream...".to_string(),
                    ));
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
    }
    impl TestRenderer {
        fn new() -> Self {
            Self { events: Vec::new() }
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
        fn request_abort(&mut self) {}
        fn aborted(&self) -> bool {
            false
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
                }],
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
}
