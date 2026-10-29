//! Steer arbitration: event type, shared history, and the spawned arbitrator task.
//!
//! The arbitration round machine itself is shared with the paused-stream sink and lives in
//! [`super::arbiter`]; this module is the adapter that feeds it the spawned-task inputs and
//! surfaces its progress as [`SteerArbEvent`]s.

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
        /// The notices this arbitration already posted, as `(notice_id, target)` in posting
        /// order. The host renders them; it must never post again for the same decision.
        /// Empty means nothing was filed — either the decision carried no `ForwardNotice`
        /// subtask, or every target was refused as unroutable.
        forwarded: Vec<(String, String)>,
    },
    /// A `ForwardNotice` subtask the round **refused to file**: `token` addressed no live
    /// worker, so nothing entered an inbox. Additive progress for the UI only — the refusal
    /// itself is decided and worded by [`super::arbiter`], and the same refusal already
    /// streams through [`SteerArbEvent::Delta`] as arbitrator prose. The host must not
    /// re-post, and must not treat this as a change of decision.
    UnroutableTarget {
        token: String,
        user_msg: String,
    },
}

/// Hand a steering instruction to the host's steer queue **and** tell the renderer the same
/// fact, from the same value.
///
/// The push and the projection are one statement so they can never disagree: what the
/// renderer announces is literally what the queue received. Behaviour is unchanged — the
/// queue receives exactly the string it used to receive.
pub(crate) fn queue_steer(
    steer_queue: &mut Vec<String>,
    renderer: &mut dyn Renderer,
    trace: &crate::ui::SteerTrace,
) {
    steer_queue.push(trace.text.clone());
    renderer.note_steer(trace);
}

/// [`queue_steer`] for the arms that queue a **wrapper** around the operator's words
/// (`"User approved plan."`, `"User rejected plan: …"`) rather than the words themselves.
/// The queue still receives exactly the wrapped string it used to; the trace keeps naming
/// the instruction the operator actually sent.
pub(crate) fn queue_steer_as(
    steer_queue: &mut Vec<String>,
    renderer: &mut dyn Renderer,
    trace: &crate::ui::SteerTrace,
    queued_text: &str,
) {
    steer_queue.push(queued_text.to_string());
    renderer.note_steer(trace);
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

        // **One notice per `ForwardNotice` subtask, posted once, here.** This host is the
        // owner for the spawned-task path; the drain host only renders what comes back in
        // `Finished::forwarded`. Posting again downstream is what used to file the raw user
        // text as a second (broadcast) notice for the same decision.
        let outcomes = arbiter::post_forward_notices(decision.as_ref(), &msg);
        for outcome in &outcomes {
            match outcome {
                arbiter::ForwardOutcome::Posted(posted) => {
                    let _ = tx.send(SteerArbEvent::Delta(format!(
                        "\n[Arbitrator]: Forwarded notice {} to {} — awaiting specialist reply.\n",
                        posted.notice_id, posted.target
                    )));
                    if !posted.dropped_older.is_empty() {
                        // Inbox backpressure stays (drop-oldest at INBOX_CAPACITY) but is surfaced
                        // instead of silently losing steering instructions.
                        let drop_text =
                            arbiter::capacity_drop_text(&posted.target, &posted.dropped_older);
                        let _ =
                            tx.send(SteerArbEvent::Delta(format!("[Arbitrator]: {drop_text}\n")));
                    }
                }
                arbiter::ForwardOutcome::Unroutable { token } => {
                    // Never filed into an inbox nobody drains: the failure is told to the user.
                    let text = arbiter::unroutable_target_text(token);
                    let _ = tx.send(SteerArbEvent::Delta(format!("\n[Arbitrator]: {text}\n")));
                    // The same refusal as a typed fact for the panes, alongside the prose the
                    // arbitrator has always streamed. Posting and routing decisions are
                    // untouched: this only names the outcome the round already produced.
                    let _ = tx.send(SteerArbEvent::UnroutableTarget {
                        token: token.clone(),
                        user_msg: msg.clone(),
                    });
                }
            }
        }
        let forwarded_notices = arbiter::posted_notices(&outcomes);
        let unroutable = arbiter::unroutable_targets(&outcomes);

        let recorded_resp = arbiter::recorded_response(
            decision.as_ref(),
            synthesized_answer_opt.as_deref(),
            &forwarded_notices,
            &unroutable,
        );

        if let Some(ref hist_lock) = steering_history
            && let Ok(mut hist) = hist_lock.write()
        {
            hist.push((msg.clone(), recorded_resp));
        }

        let _ = tx.send(SteerArbEvent::Finished {
            decision,
            user_msg: msg,
            forwarded: forwarded_notices,
        });
    })
}
