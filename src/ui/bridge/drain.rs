//! Drain steer-arbitration events from the channel into the renderer / steer queue.

use super::action;
use super::arbiter;
use super::steer::SteerArbEvent;
use crate::ui::{Event, Renderer, SubagentDetail};

pub(crate) fn drain_steer_arbitration_events(
    arb_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SteerArbEvent>,
    renderer: &mut dyn Renderer,
    steer_queue: &mut Vec<String>,
    steer_abort_requested: &mut bool,
    subagents: Option<&mut Vec<SubagentDetail>>,
) {
    drain_steer_arbitration_events_with_transcript(
        arb_rx,
        renderer,
        steer_queue,
        steer_abort_requested,
        subagents,
        None,
    );
}

pub(crate) fn drain_steer_arbitration_events_with_transcript(
    arb_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SteerArbEvent>,
    renderer: &mut dyn Renderer,
    steer_queue: &mut Vec<String>,
    steer_abort_requested: &mut bool,
    mut subagents: Option<&mut Vec<SubagentDetail>>,
    mut ui_transcript: Option<&mut crate::ui::UiTranscript>,
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
                if let Some(ref mut tr) = ui_transcript {
                    tr.append(crate::ui::UiRecord::TaskCompleted {
                        task_id: task_id.clone(),
                    });
                }
                steer_queue.push(format!(
                    "(User steering resulted in subtask '{task_id}' executed by specialist '{}'. Deliverable:\n{})",
                    agent.as_str(),
                    deliverable.content
                ));
            }
            SteerArbEvent::SynthesizedAnswer { answer, .. } => {
                // User steering inquiry was answered directly to the user by arbitrator.
                // Do NOT push to steer_queue so it does not trigger an orchestrator turn.
                if let Some(ref mut tr) = ui_transcript {
                    tr.append(crate::ui::UiRecord::SteerResponse { text: answer });
                }
            }
            SteerArbEvent::DeferredSteer { user_msg, reason } => {
                // A steer whose arbitration ended in a terminal Sleep is queued here so it is
                // delivered at the next seam instead of being dropped with the arbitration.
                tracing::warn!(
                    target_worker = "arbitrator",
                    reason = %reason,
                    "Steering instruction survived a terminal arbitrator sleep — queued for the next seam"
                );
                steer_queue.push(user_msg);
                renderer.on_event(&Event::Status(arbiter::durable_steer_status(&reason)));
            }
            SteerArbEvent::Finished { decision, user_msg } => {
                if let Some(ref d) = decision {
                    // Never compare the raw `action` string: route every subtask through the
                    // orchestrator's single normalizer (see `super::action`).
                    for (st, subtask_action) in action::routed(Some(d)) {
                        if subtask_action == action::ACTION_CANCEL {
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

                if let Some(ref d) = decision
                    && let Some(ref resp) = d.response
                    && let Some(ref mut tr) = ui_transcript
                {
                    tr.append(crate::ui::UiRecord::SteerResponse { text: resp.clone() });
                }

                let has_delegations = decision
                    .as_ref()
                    .map(|d| {
                        crate::orchestrator::normalize_steer_decision(Some(&d.decision))
                            == "DelegateTask"
                            || action::any(Some(d), action::ACTION_DELEGATE_TASK)
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
                            let target = decision
                                .as_ref()
                                .and_then(|d| {
                                    action::first(Some(d), action::ACTION_FORWARD_NOTICE)
                                        .map(action::notice_target)
                                })
                                .unwrap_or("worker");
                            let posted = arbiter::post_notice_observable(target, &user_msg);
                            let notice = posted.notice;
                            renderer.on_event(&Event::Status(format!(
                                "Notice {} forwarded to {} — waiting for specialist reply...",
                                notice.notice_id, target
                            )));
                            if !posted.dropped_older.is_empty() {
                                // Capacity backpressure is kept (drop-oldest at INBOX_CAPACITY)
                                // but never silent: the dropped notice ids are surfaced.
                                let drop_text =
                                    arbiter::capacity_drop_text(target, &posted.dropped_older);
                                renderer
                                    .on_event(&Event::Status(format!("[Arbitrator] {drop_text}")));
                            }
                            steer_queue.push(user_msg);
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
                                if let Some(ref mut tr) = ui_transcript {
                                    tr.append(crate::ui::UiRecord::SteerResponse {
                                        text: "Instruction queued for next turn while active tasks continue.\n".to_string(),
                                    });
                                }
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
