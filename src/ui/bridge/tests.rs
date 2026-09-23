
use super::drain::drain_steer_arbitration_events;
use super::*;
use crate::agents::{Agent, Deliverable, MissionMarker};
use crate::ui::{Event, Renderer, SubagentDetail};

struct TestRenderer {
    events: Vec<Event>,
    /// Shared abort / user-exit flags (trait-default abort surface).
    input_state: crate::ui::InputState,
}
impl TestRenderer {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            input_state: crate::ui::InputState::default(),
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
    // `poll_input` / `read_input` / abort-flag surface use the trait
    // defaults (no-op input, shared `InputState`).
    fn input_state(&mut self) -> &mut crate::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &crate::ui::InputState {
        &self.input_state
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
        "Instruction queued for next turn.".to_string(),
    ))
    .unwrap();

    // Then completes with QueueAndContinue decision
    tx.send(SteerArbEvent::Finished {
        decision: Some(crate::orchestrator::SteerDecision {
            decision: "QueueAndContinue".to_string(),
            response: Some("Instruction queued for next turn.".to_string()),
            tier: None,
            model: None,
            subtasks: Vec::new(),
            sleep_seconds: None,
        }),
        user_msg: "add an extra feature later".to_string(),
    })
    .unwrap();

    drain_steer_arbitration_events(
        &mut rx,
        &mut renderer,
        &mut steer_queue,
        &mut steer_abort,
        None,
    );

    assert_eq!(steer_queue, vec!["add an extra feature later".to_string()]);
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
    assert_eq!(steer_responses, vec!["Instruction queued for next turn."]);

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
                response: Some("Coder is working on tests.".to_string()),
                tier: None,
                model: None,
                subtasks: Vec::new(),
                sleep_seconds: None,
            }),
            user_msg: "how is it going?".to_string(),
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
        user_msg: "what is the subagent doing?".to_string(),
        answer: "Subagent is running cargo check.".to_string(),
    })
    .unwrap();

    tx.send(SteerArbEvent::Finished {
        decision: Some(crate::orchestrator::SteerDecision {
            decision: "RespondDirectly".to_string(),
            response: Some("Subagent is running cargo check.".to_string()),
            tier: None,
            model: None,
            subtasks: Vec::new(),
            sleep_seconds: None,
        }),
        user_msg: "what is the subagent doing?".to_string(),
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
            response: Some("Waiting 5 seconds...".to_string()),
            tier: None,
            model: None,
            subtasks: Vec::new(),
            sleep_seconds: Some(5),
        }),
        user_msg: "wait a moment".to_string(),
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
