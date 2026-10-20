//! Steer arbitration: event type, shared history, and the spawned arbitrator task.
//!
//! The arbitration round machine itself is shared with the paused-stream sink and lives in
//! [`super::arbiter`]; this module is the adapter that feeds it the spawned-task inputs and
//! surfaces its progress as [`SteerArbEvent`]s.

use super::action;
use super::arbiter::{self, ArbitrationChannel, SleepNotice};
use crate::llm::ChatClient;
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
    /// A steering instruction whose arbitration ended in a **terminal Sleep**: it never reached
    /// an action, so the host queues it for the next seam instead of dropping it.
    DeferredSteer {
        user_msg: String,
        reason: String,
    },
    Finished {
        decision: Option<crate::orchestrator::SteerDecision>,
        user_msg: String,
    },
}

pub use crate::orchestrator::SharedSteeringHistory;

/// Arbitration channel of the spawned-task host: progress is pushed onto the steer-arbitration
/// event channel and rendered by [`super::drain`] on the UI side. It carries progress only —
/// the arbitrator's execution snapshot comes from the registry ([`arbiter::active_subtasks_render`]).
struct EventChannel<'a> {
    tx: &'a tokio::sync::mpsc::UnboundedSender<SteerArbEvent>,
}

impl ArbitrationChannel for EventChannel<'_> {
    fn round_started(&mut self, _round: usize) {
        // "Arbitrating user steering instruction..." is emitted once, before the task is
        // spawned; later rounds surface through the streamed deltas only.
    }

    fn decision_delta(&mut self, delta: &str) {
        let _ = self.tx.send(SteerArbEvent::Delta(delta.to_string()));
    }

    fn sleep_notice(&mut self, notice: &SleepNotice) {
        let _ = self.tx.send(SteerArbEvent::Delta(notice.delta_text()));
    }
}

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
) -> tokio::task::JoinHandle<()> {
    let client = client.clone();
    let stats = stats.clone();
    let goal = goal.to_string();
    // The pane rows are **not** a snapshot source any more: the arbitrator reads the worker
    // registry (see [`arbiter::active_subtasks_render`] / [`arbiter::orchestrator_status_label`]),
    // which is the same table the pane is projected from. A stale `is_active` row can therefore
    // no longer make the arbitrator believe work is running. The parameter stays because the host
    // still hands over its pane state; it is only logged here for diagnostics.
    tracing::debug!(
        pane_rows = subagents.len(),
        "Steer arbitration: execution snapshot comes from the worker registry, not from pane rows"
    );
    let tx = arb_tx.clone();
    let msg = user_msg.clone();

    renderer.on_event(&Event::Status(
        "Arbitrating user steering instruction...".to_string(),
    ));
    let _ = renderer.flush();

    tokio::spawn(async move {
        let mut synthesized_answer_opt = None;

        let mut channel = EventChannel { tx: &tx };
        // A cancelled sleep does not end this host's work: the arbitration result is still
        // post-processed (delegations, notices, history entry, `Finished` event).
        let arbiter::ArbitrationOutcome {
            decision, sleep, ..
        } = arbiter::arbitrate_steering(
            &mut channel,
            &arbiter::ArbitrationRequest {
                client: &client,
                stats: &stats,
                goal: &goal,
                user_msg: &msg,
                steering_history: steering_history.as_ref(),
            },
        )
        .await;

        // Durable steering around Sleep: if the round machine ended on a Sleep decision, the
        // instruction never reached an action, so it is queued for the next seam rather than lost
        // with this arbitration.
        if arbiter::is_terminal_sleep(decision.as_ref()) {
            let reason = arbiter::durable_steer_reason(&sleep);
            tracing::warn!(
                sleep_extensions = sleep.extensions,
                slept_secs = sleep.slept_secs,
                reason = %reason,
                "Steering instruction survived a terminal arbitrator sleep — queued for the next seam"
            );
            let _ = tx.send(SteerArbEvent::DeferredSteer {
                user_msg: msg.clone(),
                reason,
            });
        }

        if let Some(ref d) = decision {
            let tasks = arbiter::delegation_tasks(Some(d), &msg);
            let mut completed_deliverables = Vec::new();
            for (agent, task_id, prompt) in tasks {
                let _ = tx.send(SteerArbEvent::DelegationStarted {
                    agent,
                    task_id: task_id.clone(),
                    prompt: prompt.clone(),
                });
                let deliverable =
                    arbiter::run_delegated_subtask(&client, &stats, agent, &task_id, &prompt).await;
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
            for (st, subtask_action) in action::routed(Some(d)) {
                if subtask_action != action::ACTION_FORWARD_NOTICE {
                    continue;
                }
                let target = action::notice_target(st);
                let msg_to_send = st.message.as_deref().unwrap_or(&msg);
                let posted = arbiter::post_notice_observable(&target, msg_to_send);
                let _ = tx.send(SteerArbEvent::Delta(format!(
                    "\n[Arbitrator]: Forwarded notice {} to {} — awaiting specialist reply.\n",
                    posted.notice.notice_id, target
                )));
                if !posted.dropped_older.is_empty() {
                    // Inbox backpressure stays (drop-oldest at INBOX_CAPACITY) but is surfaced
                    // instead of silently losing steering instructions.
                    let drop_text = arbiter::capacity_drop_text(&target, &posted.dropped_older);
                    let _ = tx.send(SteerArbEvent::Delta(format!("[Arbitrator]: {drop_text}\n")));
                }
                forwarded_notices.push((posted.notice.notice_id, target.clone()));
            }
        }

        let recorded_resp = arbiter::recorded_response(
            decision.as_ref(),
            synthesized_answer_opt.as_deref(),
            &forwarded_notices,
        );

        if let Some(ref hist_lock) = steering_history
            && let Ok(mut hist) = hist_lock.write()
        {
            hist.push((msg.clone(), recorded_resp));
        }

        let _ = tx.send(SteerArbEvent::Finished {
            decision,
            user_msg: msg,
        });
    })
}
