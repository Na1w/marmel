//! Bridge between LLM streaming turns, interactive renderer, and steer arbitration.

use super::helpers::{
    format_active_subtasks, format_plan_progress_summary, is_abort_command, is_reset_command,
};
use super::{Event, Renderer, SubagentDetail};
use crate::llm::{ChatClient, PauseAction, StreamControl, StreamEvent, StreamSink};
use std::sync::Arc;

pub(crate) enum SteerArbEvent {
    Delta(String),
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
    let goal = goal.to_string();
    let plan_content = crate::agent::phase::Plan::default()
        .read()
        .unwrap_or(None)
        .unwrap_or_default();
    let active_subtasks_str = format_active_subtasks(subagents);
    let plan_progress_str = format_plan_progress_summary(&plan_content);
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
            orchestrator_status: if active_subtasks_str == "None" {
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
) {
    while let Ok(ev) = arb_rx.try_recv() {
        match ev {
            SteerArbEvent::Delta(delta) => {
                renderer.on_event(&Event::SteerResponse(delta));
                let _ = renderer.flush();
            }
            SteerArbEvent::Finished { decision, user_msg } => {
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
                        renderer
                            .on_event(&Event::Status("Notice forwarded to specialist".to_string()));
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
        );
        if let Some(input) = self.renderer.poll_input() {
            if is_abort_command(&input) {
                crate::debug_log::log_user_input("command", &input);
                self.renderer.request_abort();
                return StreamControl::Abort;
            } else if is_reset_command(&input) {
                crate::debug_log::log_user_input("command", &input);
                let plan = crate::agent::phase::Plan::default();
                let _ = plan.clear();
                self.renderer.on_event(&Event::Message(
                    "Execution plan has been cleared and reset by user.".to_string(),
                ));
                self.renderer
                    .on_event(&Event::Status("Execution plan reset".to_string()));
                let _ = self.renderer.flush();
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

        let ctx = crate::orchestrator::steer::SteerContext {
            main_goal: self.goal,
            orchestrator_status: if active_subtasks_str == "None" {
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
