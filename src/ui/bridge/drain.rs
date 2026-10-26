//! Drain steer-arbitration events from the channel into the renderer / steer queue.

use super::action;
use super::arbiter;
use super::steer::{SteerArbEvent, queue_steer, queue_steer_as};
use crate::ui::{Event, MANAGER_TARGET, Renderer, SteerStage, SteerTrace, SubagentDetail};

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
    // Which specialists this instruction ended up working through, in the order the
    // arbitration delegated them. Local to one drain pass: the delegations of one
    // instruction are always queued ahead of its `Finished` event, so they land in the
    // same pass. It is a *witness* of what the drain already folded into rows, never a
    // second owner of delegation state.
    let mut delegated_agents: Vec<String> = Vec::new();
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
                    "\n[Steering Subtask — {} ({})]\n{}\n{}\n",
                    agent.as_str(),
                    task_id,
                    action::deliverable_verdict_line(&task_id, &deliverable),
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
                delegated_agents.push(agent.as_str().to_string());
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
                // The queue push and the operator-visible projection are one call, so the
                // pane can only ever announce an instruction the queue really received.
                let trace = SteerTrace::queued(
                    user_msg,
                    MANAGER_TARGET,
                    format!("arbitration ended sleeping without acting on it — {reason}"),
                );
                queue_steer(steer_queue, renderer, &trace);
                renderer.on_event(&Event::Status(arbiter::durable_steer_status(&reason)));
            }
            SteerArbEvent::UnroutableTarget { token, user_msg } => {
                // The round refused to file this notice. Rendered as a lifecycle fact next
                // to the arbitrator prose that already streams: the instruction reached
                // nobody, and the panes say so with the token that could not be routed.
                renderer.note_steer(&SteerTrace {
                    stage: SteerStage::Revoked,
                    text: user_msg,
                    target: token.clone(),
                    notice_id: None,
                    reason: arbiter::unroutable_target_text(&token),
                });
            }
            SteerArbEvent::Finished {
                decision,
                user_msg,
                forwarded,
            } => {
                if let Some(ref d) = decision {
                    // Never compare the raw `action` string: route every subtask through the
                    // orchestrator's single normalizer (see `super::action`).
                    for (st, subtask_action) in action::routed(Some(d)) {
                        if subtask_action != action::ACTION_CANCEL {
                            continue;
                        }
                        // **One match rule** (cancel and post-to-worker share it, see
                        // `super::action::addressed_worker_keys`): the `(agent, task)` pair is
                        // resolved against the worker registry with whole-token AND semantics —
                        // the same rule `orchestrator::workers::cancel_active_worker` applies and
                        // the same rule notice routing uses. The old OR-of-names scan over pane
                        // rows is gone: it closed every sibling row that merely shared an agent
                        // name, and it could close rows no worker was behind at all.
                        let addressed = action::addressed_worker_keys(
                            st.agent_name.as_deref(),
                            Some(&st.tool_call_id),
                        );
                        // One status line per addressed **effective key**. Joining the keys
                        // into a single label suggested one worker named `a, b` was cancelled,
                        // and the `agent_name`-less fallback label was a token no registry
                        // entry owns.
                        let named: Vec<String> = if addressed.is_empty() {
                            vec![
                                st.agent_name
                                    .as_deref()
                                    .unwrap_or(&st.tool_call_id)
                                    .to_string(),
                            ]
                        } else {
                            addressed.clone()
                        };
                        let cancelled = crate::orchestrator::cancel_active_worker(
                            st.agent_name.as_deref(),
                            Some(&st.tool_call_id),
                        );
                        if cancelled {
                            for key in &named {
                                renderer.on_event(&Event::Status(format!(
                                    "[Arbitrator] Cancelled specialist subagent '{key}'"
                                )));
                            }
                        }
                        if let Some(sub) = subagents.as_deref_mut() {
                            // Row state is never hand-written here: the rows are re-derived from
                            // the registry (`helpers::project_subagent_rows` — the same projection
                            // the pane runs on its loop edges), and only the rows whose name is an
                            // **addressed effective key** get the audit line. Whether a row stays
                            // open is the registry's call (`WorkerEntry::is_active`), so a cancelled
                            // worker closes when its guard drops, exactly as the Manager sees it.
                            let refreshed = crate::ui::helpers::project_subagent_rows(sub);
                            let mut annotated = false;
                            for s in sub.iter_mut() {
                                if addressed.contains(&s.name) {
                                    s.logs.push("[cancelled by arbitrator]".to_string());
                                    annotated = true;
                                }
                            }
                            if refreshed || annotated {
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
                    // The instruction reached specialists as delegated subtasks. Who ran them
                    // is read off the decision's own `DelegateTask` subtasks, with the
                    // deliverables this pass already folded into rows as the fallback for an
                    // arbitrator that left the agent handle out.
                    let mut reached: Vec<String> = Vec::new();
                    let delegated = decision
                        .as_ref()
                        .map(|d| {
                            action::routed(Some(d))
                                .into_iter()
                                .filter(|(_, route)| *route == action::ACTION_DELEGATE_TASK)
                                .map(|(st, _)| st.agent_name.clone().unwrap_or_default())
                                .collect::<Vec<String>>()
                        })
                        .unwrap_or_default();
                    for name in delegated.into_iter().chain(delegated_agents.drain(..)) {
                        let name = name.trim().to_string();
                        if !name.is_empty() && !reached.contains(&name) {
                            reached.push(name);
                        }
                    }
                    renderer.note_steer(&SteerTrace {
                        stage: SteerStage::Delivered,
                        text: user_msg.clone(),
                        target: if reached.is_empty() {
                            MANAGER_TARGET.to_string()
                        } else {
                            reached.join(", ")
                        },
                        notice_id: None,
                        reason: "the arbitrator answered it with delegated subtask(s)".to_string(),
                    });
                    renderer.on_event(&Event::Status(
                        "Steering subtask delegation finished".to_string(),
                    ));
                } else {
                    // No delegation reached a specialist, so nothing this pass witnessed is
                    // attributable to an instruction any more.
                    delegated_agents.clear();
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
                            queue_steer(
                                steer_queue,
                                renderer,
                                &SteerTrace::queued(
                                    user_msg,
                                    MANAGER_TARGET,
                                    "the arbitrator aborted the run; the instruction is handed to the Manager",
                                ),
                            );
                        }
                        "ForwardToWorker" => {
                            // The arbitration round owns the posting (see
                            // `arbiter::post_forward_notices`); this host only renders the
                            // outcome it was handed. Rendering — not re-posting — is the
                            // whole point: the second copy used to be filed under a `worker`
                            // broadcast with the raw user text, so one steer left two pending
                            // notices, and answering one left the phantom copy pending until
                            // the TTL while a second reply came back `already-replied`.
                            if forwarded.is_empty() {
                                // Nothing reached a specialist. The instruction must still
                                // survive, so it is queued for the next seam rather than lost.
                                let trace = SteerTrace::queued(
                                    user_msg.clone(),
                                    MANAGER_TARGET,
                                    "no forwarding notice was posted for this instruction",
                                );
                                queue_steer(steer_queue, renderer, &trace);
                                renderer.on_event(&Event::Status(
                                    "No forwarding notice was posted for this steering \
                                     instruction — queued for the next turn instead"
                                        .to_string(),
                                ));
                            } else {
                                // One projection per notice the round filed: the instruction,
                                // the specialist it is sitting in front of, and the id that
                                // specialist has to reply to. The posting itself stays where
                                // it was — in `arbiter::post_forward_notices`.
                                for (notice_id, target) in &forwarded {
                                    renderer.note_steer(&SteerTrace {
                                        stage: SteerStage::Delivered,
                                        text: user_msg.clone(),
                                        target: target.clone(),
                                        notice_id: Some(notice_id.clone()),
                                        reason:
                                            "filed in the specialist's inbox; awaiting its reply"
                                                .to_string(),
                                    });
                                }
                                renderer.on_event(&Event::Status(arbiter::forwarded_status(
                                    &forwarded, false,
                                )));
                            }
                        }
                        "ApprovePlan" => {
                            // The queue receives the wrapper it always received; the pane names
                            // the instruction the operator actually sent.
                            queue_steer_as(
                                steer_queue,
                                renderer,
                                &SteerTrace::queued(
                                    user_msg,
                                    MANAGER_TARGET,
                                    "the arbitrator approved the plan on the operator's behalf",
                                ),
                                "User approved plan.",
                            );
                        }
                        "RejectPlan" => {
                            crate::orchestrator::cancel_all();
                            renderer.request_abort();
                            *steer_abort_requested = true;
                            let queued = format!("User rejected plan: {user_msg}");
                            queue_steer_as(
                                steer_queue,
                                renderer,
                                &SteerTrace::queued(
                                    user_msg,
                                    MANAGER_TARGET,
                                    "the arbitrator rejected the plan",
                                ),
                                &queued,
                            );
                        }
                        "QueueAndContinue" => {
                            queue_steer(
                                steer_queue,
                                renderer,
                                &SteerTrace::queued(
                                    user_msg,
                                    MANAGER_TARGET,
                                    "held while the running specialists continue",
                                ),
                            );
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
                                queue_steer(
                                    steer_queue,
                                    renderer,
                                    &SteerTrace::queued(
                                        user_msg,
                                        MANAGER_TARGET,
                                        "the arbitrator returned no action and no answer for it",
                                    ),
                                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::{SteerDecision, SteerSubtaskDecision};

    /// A renderer that records the two seams this drain owns: the events it was
    /// handed, and the steering-instruction transitions it was told about.
    #[derive(Default)]
    struct Probe {
        traces: Vec<SteerTrace>,
        input_state: crate::ui::InputState,
    }

    impl Renderer for Probe {
        fn init(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn on_event(&mut self, _event: &Event) {}
        fn flush(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn note_steer(&mut self, trace: &SteerTrace) {
            self.traces.push(trace.clone());
        }
        fn input_state(&mut self) -> &mut crate::ui::InputState {
            &mut self.input_state
        }
        fn input_state_shared(&self) -> &crate::ui::InputState {
            &self.input_state
        }
        fn shutdown(&mut self) {}
    }

    /// Run one drain pass over `events` and hand back the steer queue plus every
    /// transition the drain projected to the renderer.
    fn drain(events: Vec<SteerArbEvent>) -> (Vec<String>, Probe) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for event in events {
            tx.send(event).unwrap();
        }
        drop(tx);
        let mut queue = Vec::new();
        let mut probe = Probe::default();
        let mut aborted = false;
        drain_steer_arbitration_events(
            &mut rx,
            &mut probe,
            &mut queue,
            &mut aborted,
            None::<&mut Vec<SubagentDetail>>,
        );
        (queue, probe)
    }

    fn subtask(action: &str, agent: Option<&str>) -> SteerSubtaskDecision {
        SteerSubtaskDecision {
            tool_call_id: "call-t-013b".to_string(),
            action: action.to_string(),
            message: None,
            agent_name: agent.map(str::to_string),
            prompt: None,
            sleep_seconds: None,
        }
    }

    fn decision(kind: &str, subtasks: Vec<SteerSubtaskDecision>) -> SteerDecision {
        SteerDecision {
            decision: kind.to_string(),
            response: None,
            tier: None,
            model: None,
            subtasks,
            sleep_seconds: None,
        }
    }

    /// Affordance 1, at the seam that owns it: the queue push and the operator-visible
    /// projection are one call, so the pane can only ever announce the string the queue
    /// actually received (spec §3, §8.3; DATA §6 gap 7).
    #[test]
    fn a_held_instruction_is_announced_as_the_text_the_queue_received() {
        let (queue, probe) = drain(vec![SteerArbEvent::DeferredSteer {
            user_msg: "use the shorter fixture".to_string(),
            reason: "slept 5s then capped".to_string(),
        }]);
        assert_eq!(queue, vec!["use the shorter fixture".to_string()]);
        assert_eq!(probe.traces.len(), 1, "one transition for one instruction");
        let trace = &probe.traces[0];
        assert_eq!(trace.stage, SteerStage::Queued);
        assert_eq!(trace.target, MANAGER_TARGET);
        assert_eq!(trace.text, queue[0], "the pane names what the queue holds");
        assert!(
            trace.reason.contains("slept 5s then capped"),
            "the sleep chain that ended the round is the stated reason: {}",
            trace.reason
        );
        assert_eq!(trace.notice_id, None, "no notice was filed here");
    }

    /// Affordance 2: every notice the round filed is named with its target and its id —
    /// and the drain never re-posts it, so the queue gets nothing extra.
    #[test]
    fn a_forwarded_notice_is_announced_per_notice_with_target_and_id() {
        let (queue, probe) = drain(vec![SteerArbEvent::Finished {
            decision: Some(decision("ForwardToWorker", Vec::new())),
            user_msg: "switch to the shorter fixture".to_string(),
            forwarded: vec![
                (
                    "notice-one".to_string(),
                    "coder_specialist-t-013b".to_string(),
                ),
                (
                    "notice-two".to_string(),
                    "reviewer_specialist-t-013b".to_string(),
                ),
            ],
        }]);
        assert!(
            queue.is_empty(),
            "a forwarded instruction is not queued again"
        );
        assert_eq!(probe.traces.len(), 2, "one transition per filed notice");
        for (trace, (id, target)) in probe.traces.iter().zip(vec![
            ("notice-one", "coder_specialist-t-013b"),
            ("notice-two", "reviewer_specialist-t-013b"),
        ]) {
            assert_eq!(trace.stage, SteerStage::Delivered);
            assert_eq!(trace.target, target);
            assert_eq!(trace.notice_id.as_deref(), Some(id));
            assert!(trace.reason.contains("inbox"), "{}", trace.reason);
        }
    }

    /// A `ForwardToWorker` that filed nothing must still survive — and the pane says it
    /// is being held for the Manager rather than leaving the operator guessing.
    #[test]
    fn an_empty_forward_holds_the_instruction_and_says_why() {
        let (queue, probe) = drain(vec![SteerArbEvent::Finished {
            decision: Some(decision("ForwardToWorker", Vec::new())),
            user_msg: "address nobody in particular".to_string(),
            forwarded: Vec::new(),
        }]);
        assert_eq!(queue, vec!["address nobody in particular".to_string()]);
        assert_eq!(probe.traces.len(), 1);
        assert_eq!(probe.traces[0].stage, SteerStage::Queued);
        assert_eq!(probe.traces[0].target, MANAGER_TARGET);
        assert!(
            probe.traces[0]
                .reason
                .contains("no forwarding notice was posted"),
            "{}",
            probe.traces[0].reason
        );
    }

    /// The refusal the arbitrator already streams as prose also arrives as a typed fact:
    /// the instruction reached no inbox, and the token that could not be routed is named.
    #[test]
    fn an_unroutable_target_is_announced_as_never_delivered() {
        let (queue, probe) = drain(vec![SteerArbEvent::UnroutableTarget {
            token: "ghost_worker".to_string(),
            user_msg: "tell the ghost what to do".to_string(),
        }]);
        assert!(queue.is_empty(), "a refused instruction is not queued here");
        assert_eq!(probe.traces.len(), 1);
        let trace = &probe.traces[0];
        assert_eq!(trace.stage, SteerStage::Revoked);
        assert_eq!(trace.target, "ghost_worker");
        assert_eq!(trace.notice_id, None, "nothing was ever filed");
        assert_eq!(
            trace.reason,
            arbiter::unroutable_target_text("ghost_worker")
        );
    }

    /// An answer delivered as delegated subtask(s) names the specialists that ran it —
    /// read off the decision's own subtasks, never guessed.
    #[test]
    fn a_delegated_answer_names_the_specialists_it_reached() {
        let (queue, probe) = drain(vec![SteerArbEvent::Finished {
            decision: Some(decision(
                "DelegateTask",
                vec![subtask("DelegateTask", Some("coder_specialist"))],
            )),
            user_msg: "have a specialist do it".to_string(),
            forwarded: Vec::new(),
        }]);
        assert_eq!(probe.traces.len(), 1);
        let trace = &probe.traces[0];
        assert_eq!(trace.stage, SteerStage::Delivered);
        assert_eq!(trace.target, "coder_specialist");
        assert!(
            trace.reason.contains("delegated subtask"),
            "{}",
            trace.reason
        );
        assert!(
            queue.is_empty(),
            "the delegation deliverable report is a separate arm, not this one: {queue:?}"
        );
    }

    /// The wrapper arms keep the queue byte-identical to before the affordance existed:
    /// the queue receives the wrapper, the pane names the operator's own words.
    #[test]
    fn a_wrapped_decision_queues_the_wrapper_and_announces_the_instruction() {
        let (queue, probe) = drain(vec![SteerArbEvent::Finished {
            decision: Some(decision("ApprovePlan", Vec::new())),
            user_msg: "ship it".to_string(),
            forwarded: Vec::new(),
        }]);
        assert_eq!(queue, vec!["User approved plan.".to_string()]);
        assert_eq!(probe.traces.len(), 1);
        assert_eq!(probe.traces[0].stage, SteerStage::Queued);
        assert_eq!(probe.traces[0].text, "ship it");
        assert_eq!(probe.traces[0].target, MANAGER_TARGET);
    }
}
