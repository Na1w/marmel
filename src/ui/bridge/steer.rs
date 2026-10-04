//! Steer arbitration: event type, shared history, and the spawned arbitrator task.

use crate::llm::{ChatClient, PauseAction};
use crate::ui::helpers::{format_active_subtasks, format_plan_progress_summary};
use crate::ui::{Event, Renderer, SubagentDetail};
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

pub use crate::orchestrator::SharedSteeringHistory;

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

        let mut forwarded_notices = Vec::new();
        if let Some(ref d) = decision
            && crate::orchestrator::normalize_steer_decision(Some(&d.decision)) == "ForwardToWorker"
        {
            for st in &d.subtasks {
                if st.action.eq_ignore_ascii_case("ForwardNotice") {
                    let target = st.agent_name.as_deref().unwrap_or(&st.tool_call_id);
                    let msg_to_send = st.message.as_deref().unwrap_or(&msg);
                    let notice =
                        crate::orchestrator::post_notice_to_worker(target, msg_to_send, None);
                    let _ = tx.send(SteerArbEvent::Delta(format!(
                        "\n[Arbitrator]: Forwarded notice {} to {} — awaiting specialist reply.\n",
                        notice.notice_id, target
                    )));
                    forwarded_notices.push((notice.notice_id, target.to_string()));
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
            } else if !forwarded_notices.is_empty() {
                forwarded_notices
                    .iter()
                    .map(|(nid, tgt)| {
                        format!("Forwarded notice {nid} to {tgt} (awaiting specialist reply)")
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
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
