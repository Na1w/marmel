use super::drain::drain_steer_arbitration_events;
use super::*;
use crate::agents::{Agent, Deliverable, MissionMarker};
use crate::ui::{Event, Renderer, SubagentDetail};

struct TestRenderer {
    events: Vec<Event>,
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
        forwarded: Vec::new(),
    })
    .unwrap();

    drain_steer_arbitration_events(
        &mut rx,
        &mut renderer,
        &mut steer_queue,
        &mut steer_abort,
        Some(&mut subagents),
    );

    assert_eq!(subagents.len(), 1);
    assert_eq!(subagents[0].name, "coder-steer-task-1");
    assert!(!subagents[0].is_active);

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
        forwarded: Vec::new(),
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

    let steer_responses: Vec<_> = renderer
        .events
        .iter()
        .filter_map(|e| match e {
            Event::SteerResponse(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(steer_responses, vec!["Instruction queued for next turn."]);

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
        forwarded: Vec::new(),
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
            forwarded: Vec::new(),
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
        forwarded: Vec::new(),
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
        forwarded: Vec::new(),
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
        forwarded: Vec::new(),
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
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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

    // Production shape: after the pane became a projection of the registry, a row's name **is**
    // the effective registry key. The old version of this test hand-named the row `"coder"` and
    // kept a separate `task_id` — the exact drift (row not addressable by the token the
    // arbitrator and the Manager use) that the single match rule now refuses to work with.
    let key = "coder-t-steer-cancel-001".to_string();
    let mut subagents = vec![
        crate::ui::helpers::subagent_row_for_registry_key(&key)
            .expect("the registered worker must project a pane row under its effective key"),
    ];
    assert_eq!(subagents[0].name, key);

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
        forwarded: Vec::new(),
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
    let row = subagents
        .iter()
        .find(|s| s.name == key)
        .expect("the addressed row must still be there after the registry projection");
    assert!(
        row.logs
            .iter()
            .any(|l| l.contains("cancelled by arbitrator")),
        "Subagent logs should contain cancellation note"
    );
    assert!(
        renderer.events.iter().any(|e| matches!(
            e,
            Event::Status(s) if s.contains(&format!("Cancelled specialist subagent '{key}'"))
        )),
        "the status line must name the effective registry key, got {:?}",
        renderer.events
    );

    // Liveness is the registry's call, not the bridge's: the worker's guard is still alive, so
    // the row must still read as open — the drain may annotate it, it may not close it.
    assert!(
        crate::orchestrator::has_active_workers(),
        "the registry still holds the cancelled worker while its guard is alive"
    );
    let registry_row_is_active = crate::orchestrator::worker_entries()
        .into_iter()
        .find(|entry| entry.key == key)
        .expect("the worker is still in the registry read model")
        .is_active;
    assert_eq!(
        row.is_active, registry_row_is_active,
        "the pane row must mirror the registry's liveness authority"
    );

    // Closing the row happens through the registry: dropping the guard completes the worker,
    // and the next projection flips the row to inactive.
    drop(guard);
    crate::ui::helpers::project_subagent_rows(&mut subagents);
    let closed = subagents
        .iter()
        .find(|s| s.name == key)
        .expect("the completed worker is still in the recently-completed ring");
    assert!(
        !closed.is_active,
        "the row closes when the registry says the worker is done, not before"
    );
}

/// (a) The single match rule is an **AND over whole tokens**, so a cancel addressed to one
/// task cancels exactly one registry entry and leaves a sibling worker of the **same agent**
/// — and its pane row — untouched. The old OR-of-names rule closed both rows.
#[test]
fn test_drain_steer_cancel_addresses_one_entry_and_leaves_the_sibling_row_open() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let token_a = tokio_util::sync::CancellationToken::new();
    let guard_a = crate::orchestrator::register_active_worker_with_token(
        Some("t-sibling-a".to_string()),
        "coder".to_string(),
        "First build".to_string(),
        Some(token_a.clone()),
    );
    let token_b = tokio_util::sync::CancellationToken::new();
    let guard_b = crate::orchestrator::register_active_worker_with_token(
        Some("t-sibling-b".to_string()),
        "coder".to_string(),
        "Second build".to_string(),
        Some(token_b.clone()),
    );

    let key_a = "coder-t-sibling-a".to_string();
    let key_b = "coder-t-sibling-b".to_string();
    let mut subagents = Vec::new();
    crate::ui::helpers::project_subagent_rows(&mut subagents);
    for key in [&key_a, &key_b] {
        assert!(
            subagents.iter().any(|row| &row.name == key),
            "both sibling rows must be projected from the registry, got {:?}",
            subagents.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
        );
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = TestRenderer::new();
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;
    tx.send(SteerArbEvent::Finished {
        decision: Some(crate::orchestrator::SteerDecision {
            decision: "ForwardToWorker".to_string(),
            response: None,
            tier: None,
            model: None,
            subtasks: vec![crate::orchestrator::SteerSubtaskDecision {
                tool_call_id: "t-sibling-b".to_string(),
                action: "Cancel".to_string(),
                message: None,
                agent_name: Some("coder".to_string()),
                prompt: None,
                sleep_seconds: None,
            }],
            sleep_seconds: None,
        }),
        user_msg: "cancel only the second coder".to_string(),
        forwarded: Vec::new(),
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
        token_b.is_cancelled(),
        "the addressed worker (whole effective key {key_b}) must be cancelled"
    );
    assert!(
        !token_a.is_cancelled(),
        "a sibling worker sharing only the agent name must not be cancelled"
    );

    let row_a = subagents
        .iter()
        .find(|row| row.name == key_a)
        .expect("the sibling row must survive the projection");
    let row_b = subagents
        .iter()
        .find(|row| row.name == key_b)
        .expect("the addressed row must survive the projection");
    assert!(
        !row_a
            .logs
            .iter()
            .any(|l| l.contains("cancelled by arbitrator")),
        "the sibling row must not be annotated: the cancel addressed {key_b} only, got {:?}",
        row_a.logs
    );
    assert!(
        row_b
            .logs
            .iter()
            .any(|l| l.contains("cancelled by arbitrator")),
        "the addressed row carries the audit line"
    );
    assert!(
        row_a.is_active,
        "the sibling worker is still running in the registry, so its row must stay open"
    );

    drop(guard_a);
    drop(guard_b);
}

/// (a2) One rule, one target: the whole **effective** key — including a `{base}#N` collision
/// key — addresses exactly that registry entry, and a token that is not a whole token
/// addresses nothing (no prefix/substring match, no OR fallback).
#[test]
fn test_bridge_match_rule_resolves_whole_effective_tokens_with_and_semantics() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let token = tokio_util::sync::CancellationToken::new();
    let guard = crate::orchestrator::register_active_worker_with_token(
        Some("t-token-1".to_string()),
        "coder".to_string(),
        "Whole token addressing".to_string(),
        Some(token.clone()),
    );
    let key = crate::orchestrator::active_worker_entries()
        .into_iter()
        .find(|(_, info)| info.task_id.as_deref() == Some("t-token-1"))
        .expect("the worker must be in the active read model")
        .0;

    // Whole effective key, no agent name.
    assert_eq!(
        action::addressed_worker_keys(None, Some(&key)),
        vec![key.clone()],
        "the effective key must address exactly its own worker"
    );
    // Whole effective key spelled in the task slot together with the agent name (AND).
    assert_eq!(
        action::addressed_worker_keys(Some("coder"), Some("t-token-1")),
        vec![key.clone()],
        "agent + task must both match the same worker"
    );
    // AND semantics: one argument that matches nothing cancels nothing (the old OR rule did).
    assert!(
        action::addressed_worker_keys(Some("coder"), Some("t-does-not-exist")).is_empty(),
        "an unmatched task token must not fall back to the agent-name half"
    );
    assert!(
        action::addressed_worker_keys(Some("architect"), Some("t-token-1")).is_empty(),
        "an unmatched agent name must not fall back to the task half"
    );
    // Whole-token equality only: a prefix of the key is not an address.
    assert!(
        action::addressed_worker_keys(None, Some("coder-t-token")).is_empty(),
        "a prefix of the effective key must never address a worker"
    );
    // No address at all addresses nothing.
    assert!(action::addressed_worker_keys(None, None).is_empty());

    // A forwarded notice addressed to one worker is posted under that worker's **effective
    // registry key** — the exact spelling the runner drains its inbox with, and therefore the
    // identity its `reply_to_arbitrator` must carry.
    let addressed = action::notice_targets(&forward_notice("t-token-1", Some("coder")));
    assert_eq!(
        addressed,
        action::NoticeTargeting::Addresses(vec![key.clone()]),
        "the notice target must be the effective registry key the pane row is named by"
    );

    // No `agent_name` at all (the prompt only ever spelled it out for `DelegateTask`): the
    // task token still resolves to the effective key.
    assert_eq!(
        action::notice_targets(&forward_notice("t-token-1", None)),
        action::NoticeTargeting::Addresses(vec![key.clone()]),
        "a task-shaped token must resolve to the effective key on its own"
    );

    // A non-registry `tool_call_id` (`call_1` — the OpenAI-style id models really emit) must
    // never become an inbox key: the agent half resolves the live worker instead. Filing it
    // under `call_1` is what made the notice undrainable and the correct reply a
    // `worker-mismatch`.
    assert_eq!(
        action::notice_targets(&forward_notice("call_1", Some("coder"))),
        action::NoticeTargeting::Addresses(vec![key.clone()]),
        "an unknown tool_call_id must resolve through the agent half, never be filed as an inbox key"
    );

    // With neither half pointing at a live worker, the target is **refused** rather than
    // emitted as an unroutable token.
    assert_eq!(
        action::notice_targets(&forward_notice("call_1", None)),
        action::NoticeTargeting::Unroutable {
            token: "call_1".to_string()
        },
        "a token no worker drains must be reported, not filed into a dead inbox"
    );
    assert_eq!(
        action::notice_targets(&forward_notice("", None)),
        action::NoticeTargeting::Unroutable {
            token: "the arbitrator named no worker".to_string()
        },
        "an omitted/empty tool_call_id with no agent name is refused, not silently broadcast"
    );

    // A deliberate broadcast token stays a broadcast — but it is resolved to the live keys,
    // never posted under a token nobody drains while workers are running.
    let broadcast = action::notice_targets(&forward_notice("worker", None));
    match broadcast {
        action::NoticeTargeting::Addresses(keys) => {
            assert!(
                keys.contains(&key),
                "a `worker` broadcast must reach the live worker's key, got {keys:?}"
            );
        }
        other => panic!("a broadcast token must never be refused: {other:?}"),
    }

    drop(guard);

    // With no live worker behind it, the broadcast token itself is the target: the documented
    // broadcast semantics survive, and it is the only non-registry token this rule emits.
    assert_eq!(
        action::notice_targets(&forward_notice("worker", None)),
        action::NoticeTargeting::Addresses(vec!["worker".to_string()]),
        "an explicit broadcast with no live worker stays a `worker` broadcast"
    );
}

/// A `ForwardNotice` subtask for the targeting assertions.
fn forward_notice(
    tool_call_id: &str,
    agent_name: Option<&str>,
) -> crate::orchestrator::SteerSubtaskDecision {
    crate::orchestrator::SteerSubtaskDecision {
        tool_call_id: tool_call_id.to_string(),
        action: "ForwardNotice".to_string(),
        message: Some("use postgres".to_string()),
        agent_name: agent_name.map(str::to_string),
        prompt: None,
        sleep_seconds: None,
    }
}

/// Two live workers of the **same role** must be addressed individually: a `ForwardNotice`
/// that names only the role posts one notice per effective key, so the second worker is never
/// reached through a de-facto role broadcast under a raw name token.
#[test]
fn test_bridge_notice_targeting_addresses_every_same_role_worker_individually() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let first = crate::orchestrator::register_active_worker_with_token(
        Some("t-target-a".to_string()),
        "coder".to_string(),
        "first coder".to_string(),
        Some(tokio_util::sync::CancellationToken::new()),
    );
    let second = crate::orchestrator::register_active_worker_with_token(
        Some("t-target-b".to_string()),
        "coder".to_string(),
        "second coder".to_string(),
        Some(tokio_util::sync::CancellationToken::new()),
    );

    let targeting = action::notice_targets(&forward_notice("call_ambiguous", Some("coder")));
    let keys = match targeting {
        action::NoticeTargeting::Addresses(keys) => keys,
        other => panic!("two live coders must be addressed, not refused: {other:?}"),
    };
    assert!(
        keys.contains(&first.0) && keys.contains(&second.0),
        "one notice per live key, got {keys:?} (want {} and {})",
        first.0,
        second.0
    );
    assert!(
        !keys.iter().any(|k| k == "coder" || k == "call_ambiguous"),
        "never post under a raw name token or an unroutable id: {keys:?}"
    );

    // A precise token still addresses exactly one of them.
    assert_eq!(
        action::notice_targets(&forward_notice("t-target-b", Some("coder"))),
        action::NoticeTargeting::Addresses(vec![second.0.clone()]),
        "the addressed worker only — the sibling must not receive the notice"
    );

    drop((first, second));
}

/// (b) A **stale pane row** can no longer make the arbitrator believe subagents are executing:
/// both the status label and the active-subtask table come from the registry read model.
#[test]
fn test_bridge_stale_pane_row_cannot_report_subagents_executing() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // A row that outlived its worker — the shape that used to feed the round-1 snapshot and the
    // `has_active_workers() || rows.any(is_active)` union.
    let stale_rows = vec![SubagentDetail {
        name: "coder-t-stale-row".to_string(),
        task_id: Some("t-stale-row".to_string()),
        prompt: "finished a long time ago".to_string(),
        is_active: true,
        ..Default::default()
    }];
    assert!(
        stale_rows.iter().any(|s| s.is_active),
        "the fixture must really be a stale, 'active-looking' pane row"
    );
    assert!(
        !crate::orchestrator::has_active_workers(),
        "the registry must report no running worker for this fixture"
    );

    assert_eq!(
        arbiter::orchestrator_status_label(crate::orchestrator::has_active_workers()),
        "Active (planning/turn)",
        "a stale pane row must not push the arbitrator status to 'subagents executing'"
    );
    assert!(
        !arbiter::active_subtasks_render().contains("Active Background Subagents"),
        "the arbitrator's subtask table must not list an active worker the registry does not \
         have, got {:?}",
        arbiter::active_subtasks_render()
    );

    // And the drain does not resurrect the stale row either: the projection drops rows the
    // registry no longer holds.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = TestRenderer::new();
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;
    let mut subagents = stale_rows;
    tx.send(SteerArbEvent::Finished {
        decision: Some(crate::orchestrator::SteerDecision {
            decision: "ForwardToWorker".to_string(),
            response: None,
            tier: None,
            model: None,
            subtasks: vec![crate::orchestrator::SteerSubtaskDecision {
                tool_call_id: "t-stale-row".to_string(),
                action: "Cancel".to_string(),
                message: None,
                agent_name: Some("coder".to_string()),
                prompt: None,
                sleep_seconds: None,
            }],
            sleep_seconds: None,
        }),
        user_msg: "cancel the stale row".to_string(),
        forwarded: Vec::new(),
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
        !subagents.iter().any(|row| row.name == "coder-t-stale-row"),
        "the bridge must re-derive rows from the registry, not keep a row with no worker \
         behind it, got {:?}",
        subagents.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
    );
    assert!(
        !subagents
            .iter()
            .any(|row| row.name == "coder-t-stale-row" && row.is_active),
        "no bridge path may hand-write an `is_active` flag"
    );
}

/// (c) The paused-stream host drains with **no row buffer** (`sink.rs::poll_control`). That is
/// the correct shape now: the drain cannot hand-close rows, so the only thing it does is the
/// registry cancel — and the pane follows on the session loop's projection edge.
#[test]
fn test_drain_steer_cancel_without_a_row_buffer_still_uses_registry_truth() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let token = tokio_util::sync::CancellationToken::new();
    let guard = crate::orchestrator::register_active_worker_with_token(
        Some("t-no-buffer".to_string()),
        "coder".to_string(),
        "Drained from poll_control".to_string(),
        Some(token.clone()),
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = TestRenderer::new();
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;
    tx.send(SteerArbEvent::Finished {
        decision: Some(crate::orchestrator::SteerDecision {
            decision: "ForwardToWorker".to_string(),
            response: None,
            tier: None,
            model: None,
            subtasks: vec![crate::orchestrator::SteerSubtaskDecision {
                tool_call_id: "coder-t-no-buffer".to_string(),
                action: "Cancel".to_string(),
                message: None,
                agent_name: None,
                prompt: None,
                sleep_seconds: None,
            }],
            sleep_seconds: None,
        }),
        user_msg: "cancel it".to_string(),
        forwarded: Vec::new(),
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
        token.is_cancelled(),
        "a drain with `subagents: None` must still reach the registry — the pane is not the \
         thing being cancelled"
    );
    assert!(
        renderer.events.iter().any(|e| matches!(
            e,
            Event::Status(s) if s.contains("Cancelled specialist subagent 'coder-t-no-buffer'")
        )),
        "the status line must still name the effective key, got {:?}",
        renderer.events
    );
    // The registry keeps the worker live (and therefore its row open) until the guard drops.
    assert!(
        crate::orchestrator::has_active_workers(),
        "the cancelled worker stays live in the registry until its guard is dropped"
    );

    drop(guard);
}

/// Regression guard for the union feedback loop: no bridge file may re-read pane rows as a
/// source of truth for what is running, and none may hand-write row liveness.
#[test]
fn test_bridge_tree_reads_execution_state_from_the_registry_only() {
    let production: Vec<(&str, String)> = vec![
        ("action.rs", include_str!("action.rs")),
        ("arbiter.rs", include_str!("arbiter.rs")),
        ("drain.rs", include_str!("drain.rs")),
        ("sink.rs", include_str!("sink.rs")),
        ("steer.rs", include_str!("steer.rs")),
    ]
    .into_iter()
    .map(|(name, src)| {
        (
            name,
            src.split(&["#[cfg(", "test)", "]"].concat())
                .next()
                .unwrap_or(src)
                .to_string(),
        )
    })
    .collect();

    for (name, production) in &production {
        // Comments explain the history (and name the removed helper), so only code counts.
        let code: String = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("format_active_subtasks"),
            "src/ui/bridge/{name} must not feed pane rows into the arbitrator snapshot again"
        );
        assert!(
            !code.contains("is_active ="),
            "src/ui/bridge/{name} must not write row liveness: the registry owns it"
        );
        assert!(
            !code.contains("has_active_work("),
            "src/ui/bridge/{name} must not reintroduce a host-side 'is work running' rule"
        );
    }
}

#[test]
fn test_drain_steer_abort_cancels_all_active_workers() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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

    // Production shape again: the pane rows are the registry projection, so the row is named by
    // the effective key rather than by an invented `"researcher"` display name.
    let mut subagents = Vec::new();
    crate::ui::helpers::project_subagent_rows(&mut subagents);
    let key = "researcher-t-steer-abort-002".to_string();
    assert!(
        subagents.iter().any(|row| row.name == key),
        "the registered worker must be projected as a row under its effective key, got {:?}",
        subagents.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
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
        forwarded: Vec::new(),
    })
    .unwrap();

    drain_steer_arbitration_events(
        &mut rx,
        &mut renderer,
        &mut steer_queue,
        &mut steer_abort,
        Some(&mut subagents),
    );

    assert!(steer_abort);
    assert!(renderer.aborted());
    assert!(
        token.is_cancelled(),
        "Active worker token must be cancelled on AbortImmediately"
    );
    assert!(
        subagents.iter().any(|row| row.name == key),
        "an abort must not hand-close or drop the row either — the registry still holds it"
    );

    drop(guard);
    crate::orchestrator::reset_cancellation();
}

// `arbiter` — the single owner of the steer-arbitration round machine

/// Wake-up notice wording, pinned here so the single-source helper
/// (`arbiter::wake_up_notice`) used by both hosts cannot drift silently.
const WAKE_UP_NOTICE: &str = "SYSTEM NOTICE: You already slept as requested and have now woken up to re-evaluate. Inspect the updated Active Subtasks and Plan Progress above and deliver your direct factual response or action now.";

/// SSE body carrying one streamed assistant text chunk.
fn sse_chunk(content: &str) -> String {
    let payload = serde_json::json!({
        "id": "chatcmpl-1",
        "choices": [{ "delta": { "content": content }, "finish_reason": null }],
    });
    format!("data: {payload}\n\ndata: [DONE]\n\n")
}

fn decision_json(decision: &str, response: &str, sleep_seconds: Option<u64>) -> String {
    match sleep_seconds {
        Some(secs) => format!(
            r#"{{"decision": "{decision}", "response": "{response}", "sleep_seconds": {secs}}}"#
        ),
        None => format!(r#"{{"decision": "{decision}", "response": "{response}"}}"#),
    }
}

fn steer_decision(
    decision: &str,
    response: Option<&str>,
    sleep_seconds: Option<u64>,
) -> crate::orchestrator::SteerDecision {
    crate::orchestrator::SteerDecision {
        decision: decision.to_string(),
        response: response.map(str::to_string),
        tier: None,
        model: None,
        subtasks: Vec::new(),
        sleep_seconds,
    }
}

/// Test double for the host side of one arbitration run.
#[derive(Default)]
struct RecordingChannel {
    rounds_started: Vec<usize>,
    deltas: Vec<String>,
    sleep_notices: Vec<arbiter::SleepNotice>,
}

impl arbiter::ArbitrationChannel for RecordingChannel {
    fn round_started(&mut self, round: usize) {
        self.rounds_started.push(round);
    }

    fn decision_delta(&mut self, delta: &str) {
        self.deltas.push(delta.to_string());
    }

    fn sleep_notice(&mut self, notice: &arbiter::SleepNotice) {
        self.sleep_notices.push(*notice);
    }
}

#[test]
fn test_arbiter_wake_up_notice_wording_is_single_sourced() {
    let notice = arbiter::wake_up_notice("check on the coder");
    assert_eq!(notice, format!("check on the coder ({WAKE_UP_NOTICE})"));
    assert_eq!(
        notice.matches("SYSTEM NOTICE").count(),
        1,
        "exactly one notice block per wake-up instruction"
    );
}

#[test]
fn test_arbiter_sleep_notice_wording_per_host_channel() {
    // paused-stream sink -> renderer status events
    assert_eq!(
        arbiter::SleepNotice::Started { sleep_secs: 7 }.status_text(),
        "Steering arbitrator sleeping for 7s..."
    );
    assert_eq!(
        arbiter::SleepNotice::Woke { sleep_secs: 7 }.status_text(),
        "Steering arbitrator woke up after 7s — re-evaluating status..."
    );
    assert_eq!(
        arbiter::SleepNotice::Cancelled.status_text(),
        "Steering arbitrator sleep cancelled"
    );

    // spawned arbitrator -> streamed deltas on the steer-arb channel
    assert_eq!(
        arbiter::SleepNotice::Started { sleep_secs: 7 }.delta_text(),
        "\n[Steering Arbitrator sleeping for 7s...]\n"
    );
    assert_eq!(
        arbiter::SleepNotice::Woke { sleep_secs: 7 }.delta_text(),
        "[Steering Arbitrator woke up after 7s — re-evaluating status...]\n\n"
    );
    assert_eq!(
        arbiter::SleepNotice::Cancelled.delta_text(),
        "[Steering Arbitrator sleep cancelled]\n"
    );
}

#[test]
fn test_arbiter_recorded_response_chain_for_both_hosts() {
    // No decision at all.
    assert_eq!(
        arbiter::recorded_response(None, None, &[], &[]),
        "No decision"
    );

    // Decision without a direct response falls back to the decision name.
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision("RespondDirectly", None, None)),
            None,
            &[],
            &[]
        ),
        "Decision: RespondDirectly"
    );

    // A direct response is recorded verbatim.
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision(
                "RespondDirectly",
                Some("Coder is running the suite."),
                None
            )),
            None,
            &[],
            &[]
        ),
        "Coder is running the suite."
    );

    // Sleep without a response.
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision("Sleep", None, Some(12))),
            None,
            &[],
            &[]
        ),
        "Slept for 12s"
    );

    // ForwardToWorker with **no** posted notice: the entry must not claim an outstanding
    // specialist reply that no worker can ever answer.
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision("ForwardToWorker", None, None)),
            None,
            &[],
            &[]
        ),
        "ForwardToWorker decision carried no forwarded notice (nothing is awaiting a \
         specialist reply)"
    );

    // ForwardToWorker whose target resolved to no live worker: named, not silent.
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision("ForwardToWorker", None, None)),
            None,
            &[],
            &["call_1".to_string()]
        ),
        "notice not forwarded: no active specialist is addressed by 'call_1' (nothing is \
         awaiting a specialist reply)"
    );

    // ForwardToWorker with posted notices (spawned-arbitrator host), in posting order.
    let forwarded = vec![
        ("notice-1".to_string(), "coder".to_string()),
        ("notice-2".to_string(), "debugger".to_string()),
    ];
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision("ForwardToWorker", None, None)),
            None,
            &forwarded,
            &[]
        ),
        "Forwarded notice notice-1 to coder (awaiting specialist reply), \
         Forwarded notice notice-2 to debugger (awaiting specialist reply)"
    );

    // A synthesized answer beats the decision response and the notices.
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision("DelegateTask", Some("ignored"), None)),
            Some("Synthesized answer"),
            &forwarded,
            &[]
        ),
        "Synthesized answer"
    );
}

#[test]
fn test_arbiter_delegation_tasks_and_failed_deliverable_shape() {
    assert!(arbiter::delegation_tasks(None, "run cargo check").is_empty());

    let tasks = arbiter::delegation_tasks(
        Some(&steer_decision("DelegateTask", None, None)),
        "run cargo check",
    );
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].0, Agent::Coder);
    assert_eq!(tasks[0].1, "steer-task-1");
    assert_eq!(tasks[0].2, "run cargo check");
}

/// Runs one arbitration entry-point test with a current-thread runtime.
///
/// The global cancellation token is process-wide (a `Sleep` arbitration waits on it), so
/// arbitration tests are serialized through the same workers mutex the drain tests use. The
/// runtime is driven from a synchronous test to keep that lock out of any `await` point.
fn with_arbitration_test_lock<R>(test: impl std::future::Future<Output = R>) -> R {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::orchestrator::reset_cancellation();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime for the arbitration test");
    let result = runtime.block_on(test);
    crate::orchestrator::reset_cancellation();
    result
}

#[test]
fn test_arbiter_entry_plain_steer_runs_a_single_round() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    with_arbitration_test_lock(async {
        let server = MockServer::start().await;
        // Only a wake-up round carries the SYSTEM NOTICE: if this mock answers, the plain
        // round wrongly received the notice payload.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains(WAKE_UP_NOTICE))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(sse_chunk(&decision_json(
                    "RespondDirectly",
                    "WAKEUP_ROUND_ANSWER",
                    None,
                ))),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(sse_chunk(&decision_json(
                    "RespondDirectly",
                    "Coder is running the suite.",
                    None,
                ))),
            )
            .mount(&server)
            .await;

        let client = crate::llm::ChatClient::new_with_token(server.uri(), "mock", "tok");
        let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
        let history = std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
        let mut channel = RecordingChannel::default();

        let outcome = arbiter::arbitrate_steering(
            &mut channel,
            &arbiter::ArbitrationRequest {
                client: &client,
                stats: &stats,
                goal: "keep the suite green",
                user_msg: "how is the coder doing?",
                steering_history: Some(&history),
            },
        )
        .await;

        assert!(!outcome.sleep_cancelled);
        assert_eq!(
            channel.rounds_started,
            vec![1],
            "a non-sleep decision must not trigger a wake-up round"
        );

        let decision = outcome.decision.expect("the arbitrator must answer");
        assert_eq!(
            crate::orchestrator::normalize_steer_decision(Some(&decision.decision)),
            "RespondDirectly"
        );
        assert_eq!(
            decision.response.as_deref(),
            Some("Coder is running the suite."),
            "the plain round payload must not carry the wake-up notice"
        );
        assert!(
            channel
                .deltas
                .iter()
                .any(|d| d.contains("Coder is running the suite.")),
            "the arbitrator's streamed response must reach the host channel"
        );
        assert!(
            channel.sleep_notices.is_empty(),
            "no sleep notices for a direct answer"
        );
        assert!(
            history.read().unwrap().is_empty(),
            "the arbiter records history only for sleep rounds"
        );
    });
}

#[test]
fn test_arbiter_entry_pause_steer_sleeps_then_re_evaluates_with_notice() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    with_arbitration_test_lock(async {
        let server = MockServer::start().await;
        // Answers only when the instruction carries the wake-up SYSTEM NOTICE.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains(WAKE_UP_NOTICE))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(sse_chunk(&decision_json(
                    "RespondDirectly",
                    "WAKEUP_ROUND_ANSWER",
                    None,
                ))),
            )
            .mount(&server)
            .await;
        // First (plain) round: sleep, then re-evaluate.
        let sleep_mock =
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_string(sse_chunk(
                    &decision_json("Sleep", "Waiting for the build", Some(0)),
                )))
                .up_to_n_times(1);
        sleep_mock.mount(&server).await;

        let client = crate::llm::ChatClient::new_with_token(server.uri(), "mock", "tok");
        let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
        let history = std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
        let mut channel = RecordingChannel::default();

        let outcome = arbiter::arbitrate_steering(
            &mut channel,
            &arbiter::ArbitrationRequest {
                client: &client,
                stats: &stats,
                goal: "wait for the build",
                user_msg: "wait for the build to finish",
                steering_history: Some(&history),
            },
        )
        .await;

        assert!(!outcome.sleep_cancelled);
        assert_eq!(
            channel.rounds_started,
            vec![1, 2],
            "a sleep decision must re-evaluate in a second round"
        );
        assert_eq!(
            channel.sleep_notices,
            vec![
                // This fixture asks for `sleep_seconds: 0`; the arbitrator
                // floors that to the sleep tool's own minimum
                // (`crate::tool_args::SLEEP_MIN_SECS`) instead of spinning.
                arbiter::SleepNotice::Started {
                    sleep_secs: crate::tool_args::SLEEP_MIN_SECS
                },
                arbiter::SleepNotice::Woke {
                    sleep_secs: crate::tool_args::SLEEP_MIN_SECS
                },
            ]
        );

        let decision = outcome.decision.expect("the wake-up round must answer");
        assert_eq!(
            decision.response.as_deref(),
            Some("WAKEUP_ROUND_ANSWER"),
            "the wake-up round must carry the SYSTEM NOTICE payload"
        );

        let hist = history.read().unwrap();
        assert_eq!(hist.len(), 1, "the sleep round records one history entry");
        assert_eq!(hist[0].0, "wait for the build to finish");
        // The floored sleep is what the history names: the arbitrator slept
        // `SLEEP_MIN_SECS`, not the requested 0.
        assert_eq!(
            hist[0].1,
            format!(
                "Waiting for the build (slept for {}s)",
                crate::tool_args::SLEEP_MIN_SECS
            )
        );
    });
}

#[test]
fn test_arbiter_entry_reports_cancelled_sleep_and_stops_rounds() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    with_arbitration_test_lock(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(sse_chunk(&decision_json(
                    "Sleep",
                    "Waiting for the tests",
                    Some(5),
                ))),
            )
            .mount(&server)
            .await;

        let client = crate::llm::ChatClient::new_with_token(server.uri(), "mock", "tok");
        let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
        let history = std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
        let mut channel = RecordingChannel::default();

        let canceller = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            crate::orchestrator::cancel_all();
        });

        let outcome = arbiter::arbitrate_steering(
            &mut channel,
            &arbiter::ArbitrationRequest {
                client: &client,
                stats: &stats,
                goal: "keep going",
                user_msg: "wait for the tests",
                steering_history: Some(&history),
            },
        )
        .await;
        let _ = canceller.await;

        assert!(
            outcome.sleep_cancelled,
            "a cancelled sleep must be reported to the host"
        );
        assert_eq!(
            channel.rounds_started,
            vec![1],
            "a cancelled sleep must not start another round"
        );
        assert_eq!(
            channel.sleep_notices,
            vec![
                arbiter::SleepNotice::Started { sleep_secs: 5 },
                arbiter::SleepNotice::Cancelled,
            ]
        );

        let decision = outcome.decision.expect("the sleep decision is kept");
        assert_eq!(
            crate::orchestrator::normalize_steer_decision(Some(&decision.decision)),
            "Sleep"
        );
        let hist = history.read().unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].1, "Waiting for the tests (slept for 5s)");
    });
}

// One action vocabulary for both layers, durable steering around Sleep, a capped
// chained-sleep budget, and observable inbox-capacity drops

/// One subtask decision with a raw (possibly non-canonical) action spelling.
fn subtask_with(
    tool_call_id: &str,
    action: &str,
    agent_name: Option<&str>,
) -> crate::orchestrator::SteerSubtaskDecision {
    crate::orchestrator::SteerSubtaskDecision {
        tool_call_id: tool_call_id.to_string(),
        action: action.to_string(),
        message: None,
        agent_name: agent_name.map(str::to_string),
        prompt: None,
        sleep_seconds: None,
    }
}

/// (a) Equivalent spellings of one action must route to one and the same typed
/// action in the UI bridge — the bridge delegates to the orchestrator's single
/// normalizer instead of comparing the raw string.
#[test]
fn test_bridge_subtask_action_spelling_variants_route_to_one_typed_action() {
    let groups: &[(&[&str], action::SteerSubtaskAction)] = &[
        (
            &[
                "ForwardNotice",
                "forward_notice",
                "FORWARD NOTICE",
                "forward",
                "send notice",
                // Tool-name spellings must come from `crate::tool_names`: a raw
                // literal here trips the repo-wide tool-name literal guard.
                crate::tool_names::TOOL_REPLY_TO_ARBITRATOR,
            ],
            action::ACTION_FORWARD_NOTICE,
        ),
        (
            &[
                "Cancel",
                "cancel",
                "  CANCEL  ",
                "cancel_task",
                "Cancel Task",
                "abort",
                "terminate task",
                "stop task",
            ],
            action::ACTION_CANCEL,
        ),
        (
            &["DelegateTask", "delegate", "new task", "spawn_task"],
            action::ACTION_DELEGATE_TASK,
        ),
        (
            &["Sleep", "sleep_task", "wait", crate::tool_names::TOOL_SLEEP],
            action::ACTION_SLEEP,
        ),
    ];

    for (spellings, expected) in groups {
        let mut first: Option<action::SteerSubtaskAction> = None;
        for raw in *spellings {
            let got = action::route(&subtask_with("t-035c-route", raw, None));
            assert_eq!(got, *expected, "spelling {raw:?} must route to {expected}");
            assert!(got.is_known(), "spelling {raw:?} is inside the vocabulary");
            match first {
                None => first = Some(got),
                Some(prev) => assert_eq!(
                    prev, got,
                    "every spelling of one action must select the same branch"
                ),
            }
        }
    }

    // A spelling outside the vocabulary stays an explicit rejection: no branch matches it.
    let rejected = action::route(&subtask_with("t-035c-route", "DoSomethingElse", None));
    assert_eq!(
        rejected,
        action::SteerSubtaskAction::Unknown,
        "an unrecognized action must not fall through to a default branch"
    );
    assert!(!rejected.is_known());
}

/// (a2) The equivalence is not just unit-level: a non-canonical `Cancel` spelling must reach
/// the same branch of the UI bridge drain (worker cancelled, subagent marked inactive).
#[test]
fn test_bridge_drain_cancel_action_spellings_take_the_same_branch() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    for (i, spelling) in ["Cancel", "cancel_task", "ABORT", "Terminate Task"]
        .iter()
        .enumerate()
    {
        let task_id = format!("t-035c-cancel-{i}");
        let token = tokio_util::sync::CancellationToken::new();
        let guard = crate::orchestrator::register_active_worker_with_token(
            Some(task_id.clone()),
            "coder".to_string(),
            "Running a long build".to_string(),
            Some(token.clone()),
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;
        // Production shape: the row is the registry projection under the effective key, so the
        // single match rule can address it at all.
        let key = format!("coder-{task_id}");
        let mut subagents = vec![
            crate::ui::helpers::subagent_row_for_registry_key(&key)
                .expect("the registered worker must project a pane row"),
        ];

        tx.send(SteerArbEvent::Finished {
            decision: Some(crate::orchestrator::SteerDecision {
                decision: "ForwardToWorker".to_string(),
                response: None,
                tier: None,
                model: None,
                subtasks: vec![subtask_with(&task_id, spelling, Some("coder"))],
                sleep_seconds: None,
            }),
            user_msg: format!("cancel the coder ({spelling})"),
            forwarded: Vec::new(),
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
            "action spelling {spelling:?} must reach the same Cancel branch in the UI bridge"
        );
        let row = subagents
            .iter()
            .find(|row| row.name == key)
            .unwrap_or_else(|| {
                panic!(
                    "action spelling {spelling:?} must keep the addressed row in the registry \
                     projection, got {:?}",
                    subagents.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
                )
            });
        assert!(
            row.logs
                .iter()
                .any(|l| l.contains("cancelled by arbitrator")),
            "action spelling {spelling:?} must annotate the addressed row in the same branch"
        );
        assert_eq!(
            row.is_active,
            crate::orchestrator::worker_entries()
                .into_iter()
                .find(|entry| entry.key == key)
                .expect("the worker is still in the registry read model")
                .is_active,
            "action spelling {spelling:?}: row liveness must mirror the registry, never be \
             written by the bridge"
        );
        assert!(
            renderer.events.iter().any(|e| matches!(
                e,
                Event::Status(s) if s.contains("Cancelled specialist subagent")
            )),
            "action spelling {spelling:?} must surface the same status line"
        );

        drop(guard);
    }
}

/// (b) A steer whose arbitration ended in a terminal Sleep is queued for the next
/// seam instead of being dropped with the arbitration.
#[test]
fn test_bridge_deferred_steer_survives_terminal_sleep_instead_of_being_dropped() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = TestRenderer::new();
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;

    let instruction = "switch the report export to csv once the build is done";

    tx.send(SteerArbEvent::DeferredSteer {
        user_msg: instruction.to_string(),
        reason: "steering arbitrator finished sleeping without acting on the instruction \
                (3 sleep extension(s), 15s slept)"
            .to_string(),
    })
    .unwrap();
    tx.send(SteerArbEvent::Finished {
        decision: Some(steer_decision(
            "Sleep",
            Some("Waiting for the build"),
            Some(5),
        )),
        user_msg: instruction.to_string(),
        forwarded: Vec::new(),
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
        vec![instruction.to_string()],
        "the instruction must survive the terminal sleep, not be lost"
    );
    assert!(!steer_abort);
    assert!(
        renderer.events.iter().any(|e| matches!(
            e,
            Event::Status(s) if s.contains("carried to the next seam")
        )),
        "the carry-over must be observable in the UI"
    );
}

/// (b2) End-to-end through the spawned-arbitrator host: an arbitrator that only ever answers
/// `Sleep` exhausts its chained-sleep budget, and the steering instruction is still delivered
/// at the next seam instead of being lost with the arbitration.
#[test]
fn test_bridge_steer_arbitration_terminal_sleep_still_delivers_the_instruction() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    with_arbitration_test_lock(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(sse_chunk(&decision_json(
                    "Sleep",
                    "Waiting for the build",
                    Some(0),
                ))),
            )
            .mount(&server)
            .await;

        let client = crate::llm::ChatClient::new_with_token(server.uri(), "mock", "tok");
        let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
        let history = std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let instruction = "rename the report column and tell me when it is done";

        let handle = spawn_steer_arbitration(
            &client,
            stats,
            "keep the suite green",
            &[],
            instruction.to_string(),
            &tx,
            &mut renderer,
            Some(history),
        );
        handle.await.expect("the arbitration task must finish");

        let mut steer_queue = Vec::new();
        let mut steer_abort = false;
        drain_steer_arbitration_events(
            &mut rx,
            &mut renderer,
            &mut steer_queue,
            &mut steer_abort,
            None,
        );

        assert!(
            steer_queue.iter().any(|s| s == instruction),
            "a steer around a terminal arbitrator sleep must be queued for the next seam, got {steer_queue:?}"
        );
        assert!(
            renderer.events.iter().any(|e| matches!(
                e,
                Event::Status(s) if s.contains("carried to the next seam")
            )),
            "the carry-over must be surfaced to the user"
        );
        assert!(
            renderer.events.iter().any(|e| matches!(
                e,
                Event::SteerResponse(s) if s.contains("sleep budget exhausted")
            )),
            "reaching the chained-sleep cap must be observable on the host channel"
        );
    });
}

/// (b3) Same durability for the paused-stream host: a `Sleep` decision must not swallow the
/// mid-flight instruction — it is queued for the next seam and the host refuses to extend the
/// already-exhausted chained sleep.
#[test]
fn test_bridge_paused_stream_sink_terminal_sleep_queues_the_instruction() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    with_arbitration_test_lock(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(sse_chunk(&decision_json(
                    "Sleep",
                    "Waiting for the tests",
                    Some(0),
                ))),
            )
            .mount(&server)
            .await;

        let client = crate::llm::ChatClient::new_with_token(server.uri(), "mock", "tok");
        let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
        let history = std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;
        let mut subagents: Vec<SubagentDetail> = Vec::new();
        let instruction = "use postgres instead and say so";

        {
            let mut sink = super::sink::RendererSink {
                renderer: &mut renderer,
                steer_queue: &mut steer_queue,
                steer_abort_requested: &mut steer_abort,
                arb_tx: &tx,
                arb_rx: &mut rx,
                client: &client,
                stats: stats.clone(),
                goal: "keep the suite green",
                subagents: &mut subagents,
                plan: None,
                ctx: None,
                steering_history: Some(history),
            };
            let action = crate::llm::StreamSink::on_pause(&mut sink, instruction).await;
            assert_eq!(action, crate::llm::PauseAction::Resume);
        }

        assert_eq!(
            steer_queue,
            vec![instruction.to_string()],
            "the paused-stream host must carry the instruction to the next seam"
        );
        assert!(
            renderer.events.iter().any(|e| matches!(
                e,
                Event::Status(s) if s.contains("carried to the next seam")
            )),
            "the carry-over must be surfaced to the user"
        );
        assert!(
            renderer.events.iter().any(|e| matches!(
                e,
                Event::Status(s) if s.contains("sleep budget exhausted")
            )),
            "the host must not extend an exhausted chained sleep — it says so instead"
        );
    });
}

/// (c) Chained sleep stops at the documented extension cap: the cap is a typed outcome on the
/// arbitration result, it is announced on the host channel, and it binds before the round bound.
#[test]
fn test_bridge_chained_sleep_stops_at_the_documented_extension_cap() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    with_arbitration_test_lock(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(sse_chunk(&decision_json(
                    "Sleep",
                    "Waiting again",
                    Some(0),
                ))),
            )
            .mount(&server)
            .await;

        let client = crate::llm::ChatClient::new_with_token(server.uri(), "mock", "tok");
        let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
        let history = std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
        let mut channel = RecordingChannel::default();

        let outcome = arbiter::arbitrate_steering(
            &mut channel,
            &arbiter::ArbitrationRequest {
                client: &client,
                stats: &stats,
                goal: "keep the suite green",
                user_msg: "wait for the build, then tell me",
                steering_history: Some(&history),
            },
        )
        .await;

        assert!(
            !outcome.sleep_cancelled,
            "the chain ends because of the cap, not because the sleep was cancelled"
        );
        assert_eq!(
            outcome.sleep.extensions,
            arbiter::MAX_CHAINED_SLEEP_EXTENSIONS,
            "exactly the documented number of sleep extensions may be taken"
        );
        assert_eq!(
            outcome.sleep.stop,
            Some(arbiter::SleepStop::Extensions {
                extensions: arbiter::MAX_CHAINED_SLEEP_EXTENSIONS,
                limit: arbiter::MAX_CHAINED_SLEEP_EXTENSIONS,
            }),
            "hitting the cap must be reported as a typed outcome, not a silent continue"
        );

        let expected_rounds: Vec<usize> =
            (1..=(arbiter::MAX_CHAINED_SLEEP_EXTENSIONS + 1)).collect();
        assert_eq!(
            channel.rounds_started, expected_rounds,
            "one extra re-evaluation round per allowed sleep extension, then stop"
        );
        assert!(
            channel.rounds_started.len() < arbiter::MAX_ARBITRATION_ROUNDS,
            "the documented sleep cap must bind before the round bound"
        );
        assert_eq!(
            channel
                .sleep_notices
                .iter()
                .filter(|n| matches!(n, arbiter::SleepNotice::Started { .. }))
                .count(),
            arbiter::MAX_CHAINED_SLEEP_EXTENSIONS,
            "the arbitrator may not sleep more often than the documented cap"
        );
        assert!(
            channel.sleep_notices.iter().any(|n| matches!(
                n,
                arbiter::SleepNotice::Capped { extensions, .. }
                    if *extensions == arbiter::MAX_CHAINED_SLEEP_EXTENSIONS
            )),
            "the cap must be announced on the host channel"
        );
        assert!(
            arbiter::is_terminal_sleep(outcome.decision.as_ref()),
            "a capped chain leaves a terminal Sleep decision, which the hosts queue durably"
        );
    });
}

/// (c2) The cumulative sleep deadline is deterministic (injectable clock, reusing the crate's
/// `TurnWatchdog` / `DeadlineKind`), clamps an overshooting sleep, and the paused-stream host may
/// not sleep past either documented cap.
#[test]
fn test_bridge_sleep_budget_deadline_cap_and_host_clamp_are_deterministic() {
    let now = std::time::Instant::now();

    let spent = arbiter::SleepBudget::new_at(
        now - std::time::Duration::from_secs(arbiter::MAX_CHAINED_SLEEP_SECONDS),
    );
    assert_eq!(
        spent.allow_at(5, now),
        Err(arbiter::SleepStop::Deadline {
            kind: crate::manager::r#loop::DeadlineKind::HardCap,
            slept_secs: 0,
            limit_secs: arbiter::MAX_CHAINED_SLEEP_SECONDS,
        }),
        "the cumulative sleep budget must end the chain with a typed deadline outcome"
    );

    // A request that would overshoot the budget is clamped to what is left, never rounded up.
    let nearly_spent = arbiter::SleepBudget::new_at(
        now - std::time::Duration::from_secs(arbiter::MAX_CHAINED_SLEEP_SECONDS - 7),
    );
    assert_eq!(nearly_spent.allow_at(120, now), Ok(7));

    // Inside the caps the chain may still sleep.
    let fresh = arbiter::SleepBudget::new_at(now);
    assert_eq!(fresh.allow_at(5, now), Ok(5));

    // The host's own trailing sleep obeys the same documented budget.
    let clamped = arbiter::SleepChain {
        extensions: 1,
        slept_secs: arbiter::MAX_CHAINED_SLEEP_SECONDS - 10,
        stop: None,
    };
    assert_eq!(clamped.host_sleep_secs(120), Some(10));

    let at_extension_cap = arbiter::SleepChain {
        extensions: arbiter::MAX_CHAINED_SLEEP_EXTENSIONS,
        slept_secs: 0,
        stop: None,
    };
    assert_eq!(
        at_extension_cap.host_sleep_secs(30),
        None,
        "at the extension cap the host must not sleep again"
    );

    let stopped = arbiter::SleepChain {
        extensions: 1,
        slept_secs: 0,
        stop: Some(arbiter::SleepStop::Deadline {
            kind: crate::manager::r#loop::DeadlineKind::HardCap,
            slept_secs: arbiter::MAX_CHAINED_SLEEP_SECONDS,
            limit_secs: arbiter::MAX_CHAINED_SLEEP_SECONDS,
        }),
    };
    assert_eq!(stopped.host_sleep_secs(30), None);
}

// A **sub-second** remainder of the cumulative sleep budget is an exhausted
// budget, not a zero-length sleep: `remaining.is_zero()` is false but
// `remaining.as_secs()` is `0`, so the floor is applied *after* the budget
// clamp and a budget that cannot cover it is refused.

/// A [`arbiter::SleepBudget`] with exactly `remaining` left of the cumulative cap
/// (injectable clock, so this is deterministic and pty-free).
fn sleep_budget_with_remaining(
    now: std::time::Instant,
    remaining: std::time::Duration,
) -> arbiter::SleepBudget {
    arbiter::SleepBudget::new_at(
        now - std::time::Duration::from_secs(arbiter::MAX_CHAINED_SLEEP_SECONDS) + remaining,
    )
}

/// Every sub-second remainder is refused through the same typed exhaustion outcome a
/// fully spent budget reports — the caller can tell "budget exhausted" apart from
/// "slept N seconds" — and **nothing** is accounted: no `note_slept(0)`, no extension,
/// no slept seconds on the chain the host reads back.
#[test]
fn test_bridge_sleep_budget_sub_second_remainder_is_refused_not_a_zero_length_sleep() {
    let now = std::time::Instant::now();
    let floor = crate::tool_args::SLEEP_MIN_SECS;

    for sub_second in [
        std::time::Duration::from_millis(500),
        std::time::Duration::from_millis(1),
        std::time::Duration::from_nanos(999_999_999),
    ] {
        // Mirrors the round machine's accounting contract: the loop only ever calls
        // `note_slept` with the seconds `allow_at` handed back, so a refusal must mean
        // nothing is accounted at all.
        let mut budget = sleep_budget_with_remaining(now, sub_second);
        let mut handed_back: Vec<u64> = Vec::new();
        if let Ok(secs) = budget.allow_at(30, now) {
            handed_back.push(secs);
            budget.note_slept(secs);
        }
        assert!(
            handed_back.is_empty(),
            "a {sub_second:?} remainder must be refused, not handed back as a sleep to take \
             (got {handed_back:?}; Ok(0) is the zero-length hot-loop this gate forbids)"
        );
        assert_eq!(
            budget.slept_secs(),
            0,
            "the refusal for a {sub_second:?} remainder must not book note_slept(0) against the chain"
        );
        assert_eq!(
            budget.extensions(),
            0,
            "a refused sleep is not a sleep extension"
        );
        let chain = budget.chain();
        assert_eq!(
            (chain.extensions, chain.slept_secs),
            (0, 0),
            "the chain snapshot the host reads back must be untouched by a refusal"
        );
    }

    // The refusal is the observable, typed exhaustion outcome — identical in shape to a
    // fully spent budget — so both hosts surface it as a capped chain instead of sleeping.
    assert_eq!(
        sleep_budget_with_remaining(now, std::time::Duration::from_millis(500)).allow_at(30, now),
        Err(arbiter::SleepStop::Deadline {
            kind: crate::manager::r#loop::DeadlineKind::HardCap,
            slept_secs: 0,
            limit_secs: arbiter::MAX_CHAINED_SLEEP_SECONDS,
        }),
        "a sub-second remainder must be reported as budget exhaustion, not as a sleep"
    );

    // A remainder at exactly the floor is still sleepable: the cut-off is the floor, not
    // "any fraction of a second".
    assert_eq!(
        sleep_budget_with_remaining(now, std::time::Duration::from_secs(floor)).allow_at(120, now),
        Ok(floor),
        "a remainder exactly at the floor is slept (never rounded up past the hard limit)"
    );
}

/// The floor is applied **after** the budget clamp: an inside-the-budget request still
/// comes back clamped, and no reachable call returns a duration below the floor.
#[test]
fn test_bridge_sleep_budget_floor_is_applied_after_the_budget_clamp() {
    let now = std::time::Instant::now();
    let floor = crate::tool_args::SLEEP_MIN_SECS;

    // Plenty of budget left: the request stands, an overshoot is clamped to the
    // arbitrator's own ceiling, and a sub-minimum request is floored instead of zeroed.
    let fresh = sleep_budget_with_remaining(
        now,
        std::time::Duration::from_secs(arbiter::MAX_CHAINED_SLEEP_SECONDS),
    );
    assert_eq!(
        fresh.allow_at(7, now),
        Ok(7),
        "an in-budget request is untouched"
    );
    assert_eq!(
        fresh.allow_at(0, now),
        Ok(floor),
        "a sub-minimum request is raised to the floor, never returned as a zero-length sleep"
    );
    assert_eq!(
        fresh.allow_at(u64::MAX, now),
        Ok(arbiter::MAX_SINGLE_SLEEP_SECONDS),
        "the arbitrator's own upper knob still bounds the request"
    );

    // The clamp still wins at the boundary — a sub-second truncation is never rounded up
    // to the floor, because that would overshoot the watchdog hard limit.
    assert_eq!(
        sleep_budget_with_remaining(now, std::time::Duration::from_millis(1_900))
            .allow_at(120, now),
        Ok(1),
        "1.9s of budget left must yield 1 whole second, not 2"
    );

    for requested in [
        1u64,
        2,
        7,
        30,
        arbiter::MAX_SINGLE_SLEEP_SECONDS + 1,
        u64::MAX,
    ] {
        let allowed = sleep_budget_with_remaining(now, std::time::Duration::from_secs(120))
            .allow_at(requested, now)
            .expect("a 120s remainder leaves budget");
        assert!(
            allowed >= floor,
            "an allowed duration can never be sub-minimum, saw {allowed}s for {requested}s requested"
        );
        assert!(
            allowed <= 120,
            "an allowed duration can never exceed the remaining budget, saw {allowed}s of 120s"
        );
    }
}

/// The **host** chain (`SleepChain::host_sleep_secs`) applies the same refuse-vs-floor
/// policy: a budget that cannot cover the floor is refused with `None`, a sub-minimum
/// request is floored, and no chain state can hand the host a zero-length sleep.
#[test]
fn test_bridge_sleep_chain_host_sleep_refuses_a_sub_minimum_budget_and_floors_the_request() {
    let now = std::time::Instant::now();
    let floor = crate::tool_args::SLEEP_MIN_SECS;
    let limit = arbiter::MAX_CHAINED_SLEEP_SECONDS;

    // Budget fully spent: refusal.
    let spent = arbiter::SleepChain {
        extensions: 1,
        slept_secs: limit,
        stop: None,
    };
    assert_eq!(spent.host_sleep_secs(30), None);
    assert_eq!(
        spent.host_sleep_secs(0),
        None,
        "a spent budget stays a refusal even for a sub-minimum request"
    );

    // Budget left, request below the floor: floored — never a zero-length duration the
    // host would announce as a sleep that never happened.
    let fresh = arbiter::SleepChain {
        extensions: 0,
        slept_secs: 0,
        stop: None,
    };
    assert_eq!(
        fresh.host_sleep_secs(0),
        Some(floor),
        "the host must never be handed a zero-length sleep"
    );
    assert_eq!(
        fresh.host_sleep_secs(7),
        Some(7),
        "an in-budget request is untouched"
    );
    assert_eq!(
        fresh.host_sleep_secs(u64::MAX),
        Some(limit),
        "the documented cumulative budget still bounds the host extension"
    );

    // Exactly one floor-sized second left: that second is slept, never rounded up.
    let last_second = arbiter::SleepChain {
        extensions: 1,
        slept_secs: limit - floor,
        stop: None,
    };
    assert_eq!(last_second.host_sleep_secs(120), Some(floor));

    // The host agrees with the arbitrator's own budget on refuse-vs-sleep for the same
    // remaining seconds.
    for slept in [0u64, limit - 10, limit] {
        let chain = arbiter::SleepChain {
            extensions: 0,
            slept_secs: slept,
            stop: None,
        };
        let arbitrator_allows =
            sleep_budget_with_remaining(now, std::time::Duration::from_secs(limit - slept))
                .allow_at(30, now)
                .is_ok();
        assert_eq!(
            chain.host_sleep_secs(30).is_some(),
            arbitrator_allows,
            "the host chain and the arbitrator budget must agree for a {slept}s spent chain"
        );
    }

    // No reachable chain state yields a sub-minimum duration, and the extension never
    // overshoots the cumulative budget.
    for slept_secs in [0u64, 1, 250, limit - 1, limit] {
        for requested in [0u64, 1, 30, 120, u64::MAX] {
            let chain = arbiter::SleepChain {
                extensions: 0,
                slept_secs,
                stop: None,
            };
            if let Some(secs) = chain.host_sleep_secs(requested) {
                assert!(
                    secs >= floor && slept_secs.saturating_add(secs) <= limit,
                    "host extension {secs}s for slept {slept_secs}s / requested {requested}s must be \
                     at or above the floor and stay inside the {limit}s budget"
                );
            }
        }
    }
}

/// (d) A notice dropped by the inbox capacity bound is never lost silently: the bound stays
/// intact, the drop is counted (delta, not an absolute global) and the drop text names which
/// notice id was dropped — while the newest instruction survives.
///
/// The same drive also pins the **one notice per steer** rule end-to-end: the arbitration round
/// posts exactly one notice for one `ForwardNotice` subtask, under the addressed worker's own
/// effective key, and the drain host renders that result without posting anything of its own
/// (no phantom second copy, no third delivery as a Manager turn).
#[test]
fn test_bridge_notice_capacity_drop_is_observable_and_inbox_stays_bounded() {
    use crate::orchestrator::notice;

    let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // The notice must land where a live worker actually drains it: a registered worker,
    // addressed by task token, filed under its effective key.
    let guard = crate::orchestrator::register_active_worker_with_token(
        Some("t-035c-capacity".to_string()),
        "coder".to_string(),
        "capacity-bound steer".to_string(),
        Some(tokio_util::sync::CancellationToken::new()),
    );
    let target = guard.0.clone();

    let capacity = notice::INBOX_CAPACITY;
    let before = notice::notice_lifecycle_stats();

    for i in 0..capacity {
        notice::post_notice_to_worker(
            &target,
            &format!("filler {i}"),
            Some(&format!("t035c-fill-{i}")),
        );
    }
    assert_eq!(
        notice::worker_inbox_len(&target),
        capacity,
        "the inbox is filled to exactly its documented capacity"
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = TestRenderer::new();
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;

    let mut forwarded = subtask_with("t-035c-capacity", "forward notice", Some("coder"));
    forwarded.message = Some("switch to postgres".to_string());
    let decision = crate::orchestrator::SteerDecision {
        decision: "ForwardToWorker".to_string(),
        response: None,
        tier: None,
        model: None,
        subtasks: vec![forwarded],
        sleep_seconds: None,
    };

    // **The arbitration round owns the posting** — one call, one notice per subtask.
    let outcomes =
        arbiter::post_forward_notices(Some(&decision), "ask the coder to switch to postgres");
    assert_eq!(
        outcomes.len(),
        1,
        "one ForwardNotice subtask must yield exactly one outcome"
    );
    let posted = match &outcomes[0] {
        arbiter::ForwardOutcome::Posted(posted) => posted.clone(),
        other => panic!("the addressed worker must be reachable, got {other:?}"),
    };
    assert_eq!(
        posted.target, target,
        "the notice must be filed under the addressed worker's effective key"
    );
    assert_eq!(
        posted.dropped_older.len(),
        1,
        "the overflow must be reported to the caller, not swallowed"
    );
    assert!(
        arbiter::capacity_drop_text(&target, &posted.dropped_older).contains("t035c-fill-0"),
        "the drop text must name the dropped id"
    );
    let forwarded_ids = arbiter::posted_notices(&outcomes);

    // The host only renders that result.
    tx.send(SteerArbEvent::Finished {
        decision: Some(decision),
        user_msg: "ask the coder to switch to postgres".to_string(),
        forwarded: forwarded_ids,
    })
    .unwrap();

    drain_steer_arbitration_events(
        &mut rx,
        &mut renderer,
        &mut steer_queue,
        &mut steer_abort,
        None,
    );

    // Backpressure kept: the inbox never exceeds INBOX_CAPACITY.
    assert_eq!(
        notice::worker_inbox_len(&target),
        capacity,
        "the INBOX_CAPACITY bound must stay intact"
    );

    // **One notice per steer**: the whole round (posting + drain-host rendering) issued
    // exactly one notice for this decision — the drain host used to add a second copy under
    // a `worker` broadcast default. Checked per key so parallel tests cannot perturb it.
    let queued = notice::peek_worker_inbox(&target);
    assert_eq!(
        queued
            .iter()
            .filter(|n| n.user_inquiry == "switch to postgres")
            .count(),
        1,
        "exactly one copy of this steer's notice may be queued, got {queued:?}"
    );
    assert!(
        queued
            .iter()
            .all(|n| n.user_inquiry != "ask the coder to switch to postgres"),
        "the raw user text must never be posted as a second (broadcast) notice, got {queued:?}"
    );
    let after = notice::notice_lifecycle_stats();
    let drained = notice::drain_worker_notices(&target);
    assert_eq!(
        drained
            .iter()
            .filter(|n| n.notice_id == posted.notice_id)
            .count(),
        1,
        "the worker drains exactly one copy of this steer's notice"
    );
    assert!(
        drained
            .iter()
            .any(|n| n.notice_id == posted.notice_id && n.user_inquiry == "switch to postgres"),
        "the delivered copy carries the arbitrator's rewritten inquiry"
    );
    assert!(
        drained.len() == capacity,
        "the drained inbox holds exactly the capacity-bounded set, got {}",
        drained.len()
    );

    // The drop is counted (delta for this key's overflow, never an absolute process total).
    let evicted_delta = after.capacity_evicted_total - before.capacity_evicted_total;
    assert!(
        evicted_delta >= 1,
        "a capacity drop must be counted, delta was {evicted_delta}"
    );

    // Drop-oldest: the oldest undelivered notice is dropped, the next one and the newest survive.
    assert!(
        crate::orchestrator::get_pending_notice("t035c-fill-0").is_none(),
        "the oldest notice is the one dropped by the capacity policy"
    );

    // The forwarding is observable in the UI, naming the notice id and its key.
    let statuses: Vec<String> = renderer
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Status(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        statuses
            .iter()
            .any(|s| s.contains(&posted.notice_id) && s.contains(&target)),
        "the forwarded notice must be surfaced with its id and target, got {statuses:?}"
    );

    // And a forwarded steer does **not** also reach the Manager: the notice is the delivery.
    assert!(
        steer_queue.is_empty(),
        "a forwarded steer must not be pushed into steer_queue as a third delivery, got {steer_queue:?}"
    );

    drop(guard);
}

/// (a) **One notice per `ForwardNotice` steer.** The arbitration round posts it once
/// (`arbiter::post_forward_notices`); the drain host only renders what it was handed. Before the
/// fix the drain host posted a **second** copy of the same decision — under a `worker` broadcast
/// default with the raw user text — and also pushed the raw text into `steer_queue`, so one steer
/// was delivered three times, the phantom copy stayed pending for the whole 10-minute TTL, and a
/// second answer to it came back `already-replied`.
#[test]
fn test_bridge_forward_notice_posts_exactly_one_notice_per_steer() {
    use crate::orchestrator::notice;

    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let guard = crate::orchestrator::register_active_worker_with_token(
        Some("t-035c-one".to_string()),
        "coder".to_string(),
        "one notice per steer".to_string(),
        Some(tokio_util::sync::CancellationToken::new()),
    );
    let target = guard.0.clone();

    let raw_user_text = "ask the coder about the rebase t035c-once";
    let decision = crate::orchestrator::SteerDecision {
        decision: "ForwardToWorker".to_string(),
        response: Some("Passing that to the coder.".to_string()),
        tier: None,
        model: None,
        subtasks: vec![{
            let mut st = subtask_with("t-035c-one", "forward notice", Some("coder"));
            st.message = Some("how is the rebase going?".to_string());
            st
        }],
        sleep_seconds: None,
    };

    // The posting owner: exactly one notice, under the addressed worker's effective key.
    let outcomes = arbiter::post_forward_notices(Some(&decision), raw_user_text);
    assert_eq!(outcomes.len(), 1, "one subtask, one outcome");
    let posted = match &outcomes[0] {
        arbiter::ForwardOutcome::Posted(posted) => posted.clone(),
        other => panic!("the addressed worker is live, got {other:?}"),
    };
    assert_eq!(posted.target, target);
    assert_eq!(
        notice::worker_inbox_len(&target),
        1,
        "the posting owner filed exactly one copy"
    );
    // The recorded entry's ids are the ids actually pending — the correlation the reply needs.
    assert!(
        crate::orchestrator::get_pending_notice(&posted.notice_id).is_some(),
        "the forwarded id must be a real pending notice"
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = TestRenderer::new();
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;
    tx.send(SteerArbEvent::Finished {
        decision: Some(decision),
        user_msg: raw_user_text.to_string(),
        forwarded: arbiter::posted_notices(&outcomes),
    })
    .unwrap();

    drain_steer_arbitration_events(
        &mut rx,
        &mut renderer,
        &mut steer_queue,
        &mut steer_abort,
        None,
    );

    // The drain host posted nothing: still exactly one queued copy, carrying the arbitrator's
    // rewritten inquiry rather than the raw user text.
    let queued = notice::peek_worker_inbox(&target);
    assert_eq!(
        queued.len(),
        1,
        "the whole steer must yield exactly one queued notice, got {queued:?}"
    );
    assert_eq!(queued[0].notice_id, posted.notice_id);
    assert_eq!(queued[0].user_inquiry, "how is the rebase going?");
    assert!(
        notice::peek_worker_inbox("worker")
            .iter()
            .all(|n| n.user_inquiry != raw_user_text),
        "no broadcast copy of the raw user text may be filed"
    );
    assert!(
        !notice::has_worker_inbox("t-035c-one"),
        "the raw `tool_call_id` must never become an inbox key"
    );

    // And the instruction is **not** also handed to the Manager as a third delivery.
    assert!(
        steer_queue.is_empty(),
        "a forwarded steer must not be pushed into steer_queue, got {steer_queue:?}"
    );

    // The forwarding is surfaced with both the id and the key it was filed under.
    assert!(renderer.events.iter().any(|e| matches!(
        e,
        Event::Status(s) if s.contains(&posted.notice_id) && s.contains(&target)
    )));

    // One pending notice means one reply resolves the dialogue: the worker drains exactly one
    // copy, its first answer is accepted, and only a genuine second answer is `already-replied`.
    let drained = notice::drain_worker_notices(&target);
    assert_eq!(drained.len(), 1, "the worker drains one copy: {drained:?}");
    assert_eq!(drained[0].notice_id, posted.notice_id);
    let first = notice::record_worker_reply_for_notice(&target, &posted.notice_id, "rebased");
    assert!(
        first.is_ok(),
        "the addressed worker's reply must be accepted: {first:?}"
    );
    let second = notice::record_worker_reply_for_notice(&target, &posted.notice_id, "again");
    assert_eq!(
        second.err().map(|rejection| rejection.kind().to_string()),
        Some("already-replied".to_string()),
        "only a duplicate answer may be reported as already-replied"
    );

    drop(guard);
}

/// (e) An omitted `tool_call_id` must not silently drop the notice. The field now carries a serde
/// default, so the decision stays a `ForwardToWorker` and the `agent_name` half still resolves a
/// live worker — the handle only becomes load-bearing when nothing else can address anyone.
#[test]
fn test_bridge_forward_notice_survives_an_omitted_tool_call_id() {
    use crate::orchestrator::notice;

    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let guard = crate::orchestrator::register_active_worker_with_token(
        Some("t-035c-nohandle".to_string()),
        "coder".to_string(),
        "decision without a subtask handle".to_string(),
        Some(tokio_util::sync::CancellationToken::new()),
    );
    let target = guard.0.clone();

    // Exactly the shape the arbitrator emits when it leaves out `tool_call_id`.
    let decision: crate::orchestrator::SteerDecision = serde_json::from_str(
        r#"{"decision":"ForwardToWorker","subtasks":[{"action":"ForwardNotice","agent_name":"coder","message":"run the suite"}]}"#,
    )
    .expect("an omitted tool_call_id must not make the decision unparseable");
    assert_eq!(decision.subtasks[0].tool_call_id, "");

    let outcomes =
        arbiter::post_forward_notices(Some(&decision), "tell the coder to run the suite");
    assert_eq!(
        outcomes.len(),
        1,
        "one subtask must still produce one outcome"
    );
    let posted = match &outcomes[0] {
        arbiter::ForwardOutcome::Posted(posted) => posted.clone(),
        other => panic!("the agent name alone must still address the live worker: {other:?}"),
    };
    assert_eq!(posted.target, target, "filed under the live worker's key");
    let queued = notice::peek_worker_inbox(&target);
    assert_eq!(queued.len(), 1, "exactly one notice survived: {queued:?}");
    assert_eq!(queued[0].user_inquiry, "run the suite");

    notice::drain_worker_notices(&target);
    drop(guard);
}

/// (a2) The **paused-stream host** is the other posting site, and it owns exactly one post per
/// `ForwardNotice` subtask too: no second copy under a `worker` broadcast with the raw user text,
/// and no third delivery pushed into `steer_queue` as a Manager turn. No test used to run both
/// posting hosts, which is how the duplicate survived.
#[test]
fn test_bridge_sink_host_posts_exactly_one_notice_for_a_forward_notice_decision() {
    use crate::orchestrator::notice;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    with_arbitration_test_lock(async {
        let task = "t-035c-sink";
        let raw_user_text = "ask the coder about the rebase t035c-sink";
        let body = serde_json::json!({
            "decision": "ForwardToWorker",
            "response": "Handing that to the coder.",
            "subtasks": [{
                "tool_call_id": task,
                "action": "ForwardNotice",
                "agent_name": "coder",
                "message": "how is the rebase going?",
            }],
        });

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_chunk(&body.to_string())))
            .mount(&server)
            .await;

        let guard = crate::orchestrator::register_active_worker_with_token(
            Some(task.to_string()),
            "coder".to_string(),
            "paused-stream posting".to_string(),
            Some(tokio_util::sync::CancellationToken::new()),
        );
        let target = guard.0.clone();

        let client = crate::llm::ChatClient::new_with_token(server.uri(), "mock", "tok");
        let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
        let history = std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;
        let mut subagents: Vec<SubagentDetail> = Vec::new();

        {
            let mut sink = super::sink::RendererSink {
                renderer: &mut renderer,
                steer_queue: &mut steer_queue,
                steer_abort_requested: &mut steer_abort,
                arb_tx: &tx,
                arb_rx: &mut rx,
                client: &client,
                stats: stats.clone(),
                goal: "keep the suite green",
                subagents: &mut subagents,
                plan: None,
                ctx: None,
                steering_history: Some(history.clone()),
            };
            let action = crate::llm::StreamSink::on_pause(&mut sink, raw_user_text).await;
            assert_eq!(action, crate::llm::PauseAction::Resume);
        }

        // One notice, filed under the addressed worker's own key, carrying the arbitrator's
        // rewritten inquiry.
        let queued = notice::peek_worker_inbox(&target);
        assert_eq!(
            queued.len(),
            1,
            "the paused-stream host must post exactly one notice, got {queued:?}"
        );
        assert_eq!(queued[0].user_inquiry, "how is the rebase going?");
        assert_eq!(queued[0].target_worker, target);
        assert!(
            notice::peek_worker_inbox("worker")
                .iter()
                .all(|n| n.user_inquiry != raw_user_text),
            "no broadcast copy of the raw user text may be filed"
        );

        // The instruction is not additionally handed to the Manager.
        assert!(
            steer_queue.is_empty(),
            "a forwarded steer must not be queued as a Manager turn, got {steer_queue:?}"
        );

        // It is surfaced with id + key, and the recorded entry names the same id.
        assert!(renderer.events.iter().any(|e| matches!(
            e,
            Event::Status(s) if s.contains(&queued[0].notice_id) && s.contains(&target)
        )));
        let entries = history.read().unwrap().clone();
        assert_eq!(entries.len(), 1, "one steer, one entry: {entries:?}");
        assert!(
            entries[0].1.contains(&queued[0].notice_id) && entries[0].1.contains(&target),
            "the recorded entry must name the notice it is waiting on, got {}",
            entries[0].1
        );

        // And the worker drains exactly one copy and can answer it once.
        let drained = notice::drain_worker_notices(&target);
        assert_eq!(drained.len(), 1, "one delivered copy: {drained:?}");
        assert!(
            notice::record_worker_reply_for_notice(&target, &drained[0].notice_id, "rebased")
                .is_ok(),
            "the addressed worker's reply must resolve the notice"
        );

        drop(guard);
    });
}

/// A `Cancel` carrying **no** `agent_name` (only a task token) must still address exactly one
/// registry entry — and report it under that entry's effective key, never under the raw token.
#[test]
fn test_drain_steer_cancel_without_an_agent_name_addresses_one_registry_entry() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let token_a = tokio_util::sync::CancellationToken::new();
    let guard_a = crate::orchestrator::register_active_worker_with_token(
        Some("t-035c-named-a".to_string()),
        "coder".to_string(),
        "first build".to_string(),
        Some(token_a.clone()),
    );
    let token_b = tokio_util::sync::CancellationToken::new();
    let guard_b = crate::orchestrator::register_active_worker_with_token(
        Some("t-035c-named-b".to_string()),
        "coder".to_string(),
        "second build".to_string(),
        Some(token_b.clone()),
    );
    let key_b = guard_b.0.clone();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = TestRenderer::new();
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;
    tx.send(SteerArbEvent::Finished {
        decision: Some(crate::orchestrator::SteerDecision {
            decision: "ForwardToWorker".to_string(),
            response: None,
            tier: None,
            model: None,
            subtasks: vec![subtask_with("t-035c-named-b", "Cancel", None)],
            sleep_seconds: None,
        }),
        user_msg: "cancel the second build".to_string(),
        forwarded: Vec::new(),
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
        token_b.is_cancelled(),
        "the task token alone must address the second worker"
    );
    assert!(
        !token_a.is_cancelled(),
        "the sibling sharing only the role must be untouched"
    );
    let statuses: Vec<String> = renderer
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Status(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        statuses
            .iter()
            .filter(|s| s.contains("Cancelled specialist subagent"))
            .count(),
        1,
        "exactly one cancel is reported, got {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .any(|s| s.contains(&format!("Cancelled specialist subagent '{key_b}'"))),
        "the report must name the effective key, got {statuses:?}"
    );

    drop(guard_a);
    drop(guard_b);
}

// The paused-stream host (`sink.rs`) resolves a `Sleep` duration through the single
// owner (`arbiter::clamped_sleep_secs`), never with a host-local default or clamp.
// Host and arbitrator must agree on the number, in the value slept *and* in the text
// the user is shown.

/// Parse every sleep duration announced in rendered host text, e.g.
/// "Steering arbitrator sleeping for 3s..." / "... woke up after 3s — ...".
fn rendered_sleep_durations(statuses: &[String]) -> Vec<u64> {
    let mut out = Vec::new();
    for status in statuses {
        for needle in ["sleeping for ", "woke up after "] {
            if let Some(rest) = status.split(needle).nth(1) {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if !digits.is_empty() {
                    out.push(
                        digits
                            .parse()
                            .expect("a rendered sleep duration must parse"),
                    );
                }
            }
        }
    }
    out
}

/// The host's view of a `Sleep` request **is** the owner's clamped value for every
/// input shape — no host-local default, no host-local clamp.
#[test]
fn test_bridge_sink_host_sleep_duration_is_the_owners_clamped_value() {
    for requested in [
        None,
        Some(0),
        Some(u64::MAX),
        Some(7),
        Some(crate::tool_args::SLEEP_MIN_SECS),
    ] {
        let decision = steer_decision("Sleep", None, requested);
        let host = super::sink::host_sleep_request(Some(&decision));
        assert_eq!(
            host,
            arbiter::clamped_sleep_secs(requested),
            "the host must resolve {requested:?} through arbiter::clamped_sleep_secs"
        );
        assert!(
            host > 0,
            "no sleep request may resolve to a zero-length host sleep, saw {requested:?}"
        );
    }

    // The three sleep inputs, pinned against the owner's constants, not numbers.
    let absent = steer_decision("Sleep", None, None);
    assert_eq!(
        super::sink::host_sleep_request(Some(&absent)),
        crate::tool_args::SLEEP_DEFAULT_SECS,
        "a Sleep without a duration uses the sleep-argument owner's default"
    );
    let zero = steer_decision("Sleep", None, Some(0));
    assert_eq!(
        super::sink::host_sleep_request(Some(&zero)),
        crate::tool_args::SLEEP_MIN_SECS,
        "a steered `sleep_seconds: 0` must be raised to the owner's floor — the host used to keep 0"
    );
    let huge = steer_decision("Sleep", None, Some(u64::MAX));
    assert_eq!(
        super::sink::host_sleep_request(Some(&huge)),
        arbiter::MAX_SINGLE_SLEEP_SECONDS,
        "the arbitrator's own upper budget knob still bounds the request"
    );
    // NOTE: `arbiter::MAX_SINGLE_SLEEP_SECONDS` and `tool_args::SLEEP_MAX_SECS`
    // are deliberately separate knobs — see the source guard
    // `arbitrator_upper_budget_is_not_the_sleep_tool_ceiling` in `arbiter.rs`.
    // The host may only ever consult the former, pinned by
    // `test_bridge_sink_sleep_site_redeclares_no_default_and_no_clamp`.
}

/// The trailing host sleep — the extension the paused-stream host may add on top of
/// the arbitration's own sleeps — carries the floored value into the user-visible
/// wording: host text and arbitrator duration never disagree.
#[test]
fn test_bridge_sink_host_sleep_and_its_rendered_text_agree_with_the_arbitrator() {
    // A chain with budget left: this is exactly the state in which the sink sleeps
    // on its own and renders "Steering arbitrator sleeping for {n}s...".
    let chain = arbiter::SleepChain {
        extensions: 0,
        slept_secs: 0,
        stop: None,
    };

    for requested in [None, Some(0), Some(u64::MAX)] {
        let decision = steer_decision("Sleep", None, requested);
        let owner = arbiter::clamped_sleep_secs(requested);
        let extension = chain
            .host_sleep_secs(super::sink::host_sleep_request(Some(&decision)))
            .expect("a fresh chain leaves the host its sleep");
        assert_eq!(
            extension, owner,
            "the host extension for {requested:?} must equal the arbitrator's duration"
        );

        let host_text = arbiter::SleepNotice::Started {
            sleep_secs: extension,
        }
        .status_text();
        let arbitrator_text = arbiter::SleepNotice::Started { sleep_secs: owner }.status_text();
        assert_eq!(
            host_text, arbitrator_text,
            "the text rendered for the host sleep must name the duration actually slept"
        );
        assert!(
            host_text.contains(&format!("for {owner}s")),
            "the rendered text must say {owner}s, got {host_text}"
        );
    }

    // The regression itself: `sleep_seconds: 0` is shown as the floored sleep, never "0s".
    let zero = steer_decision("Sleep", None, Some(0));
    let floored = super::sink::host_sleep_request(Some(&zero));
    let zero_text = arbiter::SleepNotice::Started {
        sleep_secs: chain.host_sleep_secs(floored).expect("budget left"),
    }
    .status_text();
    assert_eq!(
        zero_text,
        format!(
            "Steering arbitrator sleeping for {}s...",
            crate::tool_args::SLEEP_MIN_SECS
        ),
        "a steered zero-second sleep must be announced at the floor (the phantom `0s` sleep is the bug)"
    );
}

/// End-to-end through the paused-stream host: an arbitrator steered into a
/// zero-second sleep actually waits the floored minimum, and everything the host
/// renders says that same duration — the host must not announce a sleep that never
/// happened (or hide one that did).
#[test]
fn test_bridge_sink_host_and_arbitrator_agree_on_a_steered_zero_second_sleep() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    with_arbitration_test_lock(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(sse_chunk(&decision_json(
                    "Sleep",
                    "Waiting for the build",
                    Some(0),
                ))),
            )
            .mount(&server)
            .await;

        let client = crate::llm::ChatClient::new_with_token(server.uri(), "mock", "tok");
        let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
        let history = std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = TestRenderer::new();
        let mut steer_queue = Vec::new();
        let mut steer_abort = false;
        let mut subagents: Vec<SubagentDetail> = Vec::new();

        {
            let mut sink = super::sink::RendererSink {
                renderer: &mut renderer,
                steer_queue: &mut steer_queue,
                steer_abort_requested: &mut steer_abort,
                arb_tx: &tx,
                arb_rx: &mut rx,
                client: &client,
                stats: stats.clone(),
                goal: "keep the suite green",
                subagents: &mut subagents,
                plan: None,
                ctx: None,
                steering_history: Some(history.clone()),
            };
            let action = crate::llm::StreamSink::on_pause(&mut sink, "wait for the build").await;
            assert_eq!(action, crate::llm::PauseAction::Resume);
        }

        let statuses: Vec<String> = renderer
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Status(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let rendered = rendered_sleep_durations(&statuses);
        assert!(
            !rendered.is_empty(),
            "the host must render the sleeps it takes, got {statuses:?}"
        );

        // What the host resolves for this very decision, and what was actually slept.
        let decision = steer_decision("Sleep", Some("Waiting for the build"), Some(0));
        let host_secs = super::sink::host_sleep_request(Some(&decision));
        assert_eq!(
            host_secs,
            arbiter::clamped_sleep_secs(Some(0)),
            "the host must use the owner's resolution for the decision it is rendering"
        );
        assert!(
            host_secs > 0,
            "a steered zero-second sleep may never become a zero-length wait"
        );
        for observed in &rendered {
            assert_eq!(
                *observed, host_secs,
                "every rendered sleep duration must equal the duration actually slept \
                 ({host_secs}s), got {rendered:?} in {statuses:?}"
            );
        }

        // The arbitrator's own accounting of the same sleeps (written while it slept).
        let notes: Vec<String> = history
            .read()
            .unwrap()
            .iter()
            .map(|(_, resp)| resp.clone())
            .filter(|resp| resp.contains("(slept for"))
            .collect();
        assert!(
            !notes.is_empty(),
            "the arbitration must have recorded its sleeps, got {notes:?}"
        );
        for note in &notes {
            assert!(
                note.contains(&format!("(slept for {host_secs}s)")),
                "the arbitrator slept {host_secs}s, so the host must not report another \
                 duration, got {note}"
            );
        }

        // No phantom zero-length sleep anywhere in the user-visible text.
        for status in &statuses {
            assert!(
                !status.contains("sleeping for 0s") && !status.contains("woke up after 0s"),
                "the host must never announce a 0s sleep, got {status}"
            );
        }
    });
}

/// Source-level pin: the sink must **call** the owner instead of re-declaring
/// the sleep resolution. Needles are assembled at runtime so this test cannot
/// satisfy its own forbidden patterns.
#[test]
fn test_bridge_sink_sleep_site_redeclares_no_default_and_no_clamp() {
    let src = include_str!("sink.rs");
    let is_comment = |line: &str| {
        let trimmed = line.trim_start();
        trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')
    };
    let code: String = src
        .lines()
        .filter(|line| !is_comment(line))
        .collect::<Vec<&str>>()
        .join("\n");

    let owner_call = ["clamp", "ed_sleep_secs("].concat();
    assert!(
        code.contains(&owner_call),
        "sink.rs must resolve the sleep duration through the single owner ({owner_call})"
    );

    let forbidden: Vec<String> = vec![
        ["unwra", "p_or(5)"].concat(),
        ["unwra", "p_or(SLEEP_DEFAULT_SECS)"].concat(),
        ["MIN,", " MAX_SINGLE_SLEEP_SECONDS)"].concat(),
        ["MA", "X_SINGLE_SLEEP_SECONDS"].concat(),
        ["SLEEP_", "DEFAULT_SECS"].concat(),
        ["SLEEP_", "MIN_SECS"].concat(),
        ["SLEEP_", "MAX_SECS"].concat(),
    ];
    for needle in &forbidden {
        assert!(
            !code.contains(needle),
            "sink.rs re-declares sleep arithmetic ({needle}) — the resolution belongs to \
             arbiter::clamped_sleep_secs"
        );
    }

    // The host's resolver body must carry no numbers of its own at all.
    let head = code
        .find("fn host_sleep_request")
        .expect("the host sleep resolver must exist in sink.rs");
    let body_start = code[head..].find('{').expect("resolver body must open") + head + 1;
    let body_end = code[body_start..]
        .find("\n}")
        .expect("resolver body must close")
        + body_start;
    let body = &code[body_start..body_end];
    assert!(
        !body.chars().any(|c| c.is_ascii_digit()),
        "the host sleep resolver must not re-type any number, got: {body}"
    );
}

/// No raw tool-name string literals may live anywhere under `src/ui/bridge/`.
/// The needles are built **at runtime** from the `crate::tool_names` constants
/// (mirroring `tests/test_tool_name_literals.rs`), so a newly added constant is
/// covered the moment it exists, and this file stays free of the literals it
/// forbids.
#[test]
fn test_no_raw_tool_name_literals_in_bridge_tree() {
    /// Every tool name owned by `crate::tool_names` (bare + caesar variants).
    fn tool_names() -> Vec<&'static str> {
        vec![
            crate::tool_names::TOOL_DELEGATE_TASK,
            crate::tool_names::TOOL_READ_FILE,
            crate::tool_names::TOOL_WRITE_FILE,
            crate::tool_names::TOOL_REPLACE,
            crate::tool_names::TOOL_RUN_COMMAND,
            crate::tool_names::TOOL_GREP_SEARCH,
            crate::tool_names::TOOL_GLOB,
            crate::tool_names::TOOL_CREATE_PLAN,
            crate::tool_names::TOOL_ARCHIVE_PLAN,
            crate::tool_names::TOOL_REBIRTH,
            crate::tool_names::TOOL_PTY_SPAWN,
            crate::tool_names::TOOL_PTY_WRITE,
            crate::tool_names::TOOL_PTY_READ,
            crate::tool_names::TOOL_PTY_CLOSE,
            crate::tool_names::TOOL_PTY_LIST,
            crate::tool_names::TOOL_LEAVE_VERDICT,
            crate::tool_names::TOOL_SLEEP,
            crate::tool_names::TOOL_REPLY_TO_ARBITRATOR,
            crate::tool_names::TOOL_LIST_DIRECTORY,
            crate::tool_names::TERMINAL_READ_FILE,
            crate::tool_names::TERMINAL_WRITE_FILE,
            crate::tool_names::TERMINAL_REPLACE,
            crate::tool_names::TERMINAL_RUN_COMMAND,
            crate::tool_names::TERMINAL_GREP_SEARCH,
            crate::tool_names::TERMINAL_GLOB,
            crate::tool_names::TERMINAL_LIST_DIRECTORY,
            crate::tool_names::TERMINAL_SLEEP,
            crate::tool_names::TERMINAL_LEAVE_VERDICT,
        ]
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in dir
            .read_dir()
            .unwrap_or_else(|e| panic!("bridge tree must exist ({dir:?}): {e}"))
        {
            let path = entry.expect("bridge dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push((
                    path.to_string_lossy().replace('\\', "/"),
                    std::fs::read_to_string(&path).expect("read bridge source"),
                ));
            }
        }
    }

    let names = tool_names();
    assert!(!names.is_empty(), "tool-name needles must exist");

    let mut files = Vec::new();
    walk(std::path::Path::new("src/ui/bridge"), &mut files);
    assert!(
        !files.is_empty(),
        "the guard must find the sources under src/ui/bridge/"
    );

    let mut violations = Vec::new();
    for (file, content) in &files {
        for name in &names {
            let needle = format!("\"{name}\"");
            let hits = content.matches(&needle).count();
            if hits != 0 {
                violations.push(format!(
                    "{file}: {hits} raw {needle} literal(s) — use crate::tool_names::TOOL_*"
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "raw tool-name literals in src/ui/bridge/:\n{}",
        violations.join("\n")
    );
}

// ---------------------------------------------------------------------------
// fix #4 — the panel pump that keeps the subpane alive inside the sink
// (C1 in docs/diagnose_subpane_stale_refresh.md)
// ---------------------------------------------------------------------------

use super::sink::{await_with_panel_pump, pump_subagent_panel};

/// A [`TestRenderer`] that also records every authoritative panel push
/// (`Renderer::set_subagents`), optionally into a shared witness so an async test can look at
/// the panel from inside a future that is still pending.
#[derive(Default)]
struct PanelProbeRenderer {
    events: Vec<Event>,
    pushes: Vec<Vec<SubagentDetail>>,
    witness: Option<std::sync::Arc<std::sync::Mutex<Vec<Vec<SubagentDetail>>>>>,
    input_state: crate::ui::InputState,
}

impl PanelProbeRenderer {
    /// The panel's own draw predicate for the queue marker, verbatim from
    /// `src/ui/tui/render.rs:717` (`row.is_active && row.queue.is_some()`). A row failing this
    /// shows no queue information at all, whatever the pool ledger says.
    fn draws_queued(rows: &[SubagentDetail], name: &str) -> bool {
        rows.iter()
            .any(|row| row.name == name && row.is_active && row.queue.is_some())
    }
}

impl Renderer for PanelProbeRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        self.events.push(event.clone());
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        if let Some(witness) = &self.witness {
            witness
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(subagents.clone());
        }
        self.pushes.push(subagents);
    }
    fn input_state(&mut self) -> &mut crate::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &crate::ui::InputState {
        &self.input_state
    }
    fn shutdown(&mut self) {}
}

/// The pump is a **registry projection**, not a second event consumer: a live worker whose pool
/// rank landed on its registry entry (fix #3) reaches the pane with no delegation event at all.
#[test]
fn test_sink_panel_pump_surfaces_a_queued_worker_from_the_registry_alone() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let guard = crate::orchestrator::register_active_worker_with_token(
        Some("t-panel-pump-1".to_string()),
        "coder".to_string(),
        "waiting for a backend slot".to_string(),
        None,
    );
    let key = guard.0.clone();

    let mut rows: Vec<SubagentDetail> = Vec::new();
    let mut renderer = PanelProbeRenderer::default();

    // First edge: row existence itself comes from the projection.
    assert!(
        pump_subagent_panel(&mut rows, &mut renderer),
        "a live worker with no row yet is a visible edge"
    );
    let row = rows
        .iter()
        .find(|row| row.name == key)
        .expect("the live worker has a row after one projection");
    assert!(row.is_active, "a live registry entry opens its row");
    assert!(
        row.queue.is_none(),
        "no rank has been recorded on this entry yet, got {row:?}"
    );
    assert_eq!(renderer.pushes.len(), 1);

    // The pool's rank lands on the **registry entry** only — no delegation event is produced,
    // which is exactly the state a lost `Queued` leaves behind.
    assert!(
        crate::orchestrator::workers::set_active_worker_queue_position(
            &key,
            Some(crate::orchestrator::pool::QueuePosition { position: 1, of: 1 }),
        ),
        "a live entry must accept a pool rank"
    );

    assert!(
        pump_subagent_panel(&mut rows, &mut renderer),
        "a rank the registry newly reports is a visible edge; pushes: {}",
        renderer.pushes.len()
    );
    let row = rows
        .iter()
        .find(|row| row.name == key)
        .expect("the row is still on the panel");
    assert!(
        PanelProbeRenderer::draws_queued(&rows, &key),
        "the panel's own draw predicate must now hold, got {row:?}"
    );
    let queue = row.queue.as_ref().expect("queue state re-derived");
    assert_eq!((queue.position, queue.of), (1, 1));
    assert_eq!(renderer.pushes.len(), 2);

    // Self-gated: with nothing moved, the pump must not push again — this is what makes it
    // cheap enough to run on every streamed chunk.
    assert!(
        !pump_subagent_panel(&mut rows, &mut renderer),
        "an unchanged row set must not re-push"
    );
    assert_eq!(renderer.pushes.len(), 2);
}

/// `poll_control` deliberately stays **panel-silent**, and this pins that decision.
///
/// A pump here would move the pane while the Manager's own turn is still in flight, i.e. between
/// two loop drains — which is precisely the contract `tests/test_subpane_queued_refresh.rs::
/// no_delegation_drain_runs_while_the_manager_turn_is_in_flight` pins (a turn in flight is a
/// drain-free window that the loop closes at its next boundary, folding the same state then). A
/// future change that adds a pump here must update that repro **and** this test on purpose, not
/// by accident. The arbitration drain itself keeps running first, on the same channel, with the
/// same arguments.
#[tokio::test]
async fn test_poll_control_stays_panel_silent_and_keeps_owning_the_arbitration_drain() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let guard = crate::orchestrator::register_active_worker_with_token(
        Some("t-panel-pump-2".to_string()),
        "coder".to_string(),
        "delegated while the Manager's own turn is streaming".to_string(),
        None,
    );
    let key = guard.0.clone();
    assert!(
        crate::orchestrator::workers::set_active_worker_queue_position(
            &key,
            Some(crate::orchestrator::pool::QueuePosition { position: 2, of: 3 }),
        ),
        "a live entry must accept a pool rank"
    );

    let client = crate::llm::ChatClient::new("http://localhost:11434/v1", "test".to_string());
    let stats = std::sync::Arc::new(crate::harness::HarnessStats::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = PanelProbeRenderer::default();
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;
    let mut rows: Vec<SubagentDetail> = Vec::new();

    tx.send(SteerArbEvent::DelegationStarted {
        agent: Agent::Coder,
        task_id: "t-panel-pump-2".to_string(),
        prompt: "Check files".to_string(),
    })
    .expect("the arbitration channel is open");

    let control = {
        let mut sink = super::sink::RendererSink {
            renderer: &mut renderer,
            steer_queue: &mut steer_queue,
            steer_abort_requested: &mut steer_abort,
            arb_tx: &tx,
            arb_rx: &mut rx,
            client: &client,
            stats: stats.clone(),
            goal: "keep the suite green",
            subagents: &mut rows,
            plan: None,
            ctx: None,
            steering_history: None,
        };
        crate::llm::StreamSink::poll_control(&mut sink)
    };

    assert!(
        matches!(control, crate::llm::StreamControl::Continue),
        "poll_control must not change the stream control decision, got {control:?}"
    );
    assert!(
        rx.try_recv().is_err(),
        "the steer-arbitration drain must still be the one consuming its channel"
    );
    assert!(
        renderer.events.iter().any(|e| matches!(
            e,
            Event::Delegation(crate::orchestrator::DelegationEvent::Started { task, .. })
                if task.as_deref() == Some("t-panel-pump-2")
        )),
        "the arbitration event must still reach the host through the drain, got {:?}",
        renderer.events
    );
    assert!(
        steer_queue.is_empty() && !steer_abort,
        "poll_control must not feed the instruction queue or request an abort"
    );
    assert!(
        renderer.pushes.is_empty() && rows.is_empty(),
        "poll_control must not push or build panel rows: the loop's boundary drain folds this \
         state one edge later (see the repro control named in this test's doc); pushes: {}",
        renderer.pushes.len()
    );

    drop(guard);
}

/// The C1 window itself: while the sink awaits something long (a steering-delegated subtask
/// including its pool slot wait), the session loop is parked on the turn that owns the sink and
/// cannot reach its own projection edge. The in-sink pump must therefore move the pane **while
/// the awaited work is still pending** — proven by sampling the panel from inside the pending
/// future itself, so no sleep-based ordering is involved.
#[tokio::test]
// The guard serializes the process-wide worker registry for the whole scenario, and the
// scenario is the await: holding it across that await is the point, not a bug (same shape as
// `session_tests::resume_without_a_bundle_sends_the_same_request_as_a_fresh_start`).
#[allow(clippy::await_holding_lock)]
async fn test_sink_panel_pump_moves_the_pane_while_its_own_await_is_in_flight() {
    let _lock = crate::orchestrator::workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let guard = crate::orchestrator::register_active_worker_with_token(
        Some("t-panel-pump-3".to_string()),
        "coder".to_string(),
        "steering-delegated child waiting for a backend slot".to_string(),
        None,
    );
    let key = guard.0.clone();
    // The child is already queued when the sink's await starts: this is the state the pane
    // never used to learn anything about until the turn ended.
    assert!(
        crate::orchestrator::workers::set_active_worker_queue_position(
            &key,
            Some(crate::orchestrator::pool::QueuePosition { position: 1, of: 1 }),
        ),
        "a live entry must accept a pool rank"
    );

    let witness: std::sync::Arc<std::sync::Mutex<Vec<Vec<SubagentDetail>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut renderer = PanelProbeRenderer {
        witness: Some(std::sync::Arc::clone(&witness)),
        ..PanelProbeRenderer::default()
    };
    let mut rows: Vec<SubagentDetail> = Vec::new();

    // Sampled by the pending future at the exact instant its own work finished, i.e. strictly
    // inside the window in which the session loop cannot reach its projection edge.
    let mid_window: std::sync::Arc<std::sync::Mutex<Option<Vec<Vec<SubagentDetail>>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let sampled = std::sync::Arc::clone(&mid_window);
    let witness_inside = std::sync::Arc::clone(&witness);
    let row_key = key.clone();

    let deliverable = await_with_panel_pump(&mut rows, &mut renderer, async move {
        // Stands in for `arbiter::run_delegated_subtask`: the pool slot wait is the long part.
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        *sampled.lock().unwrap_or_else(|e| e.into_inner()) = Some(
            witness_inside
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        );
        format!("deliverable for {row_key}")
    })
    .await;

    assert_eq!(deliverable, format!("deliverable for {key}"));
    assert!(
        renderer.events.is_empty(),
        "the pump re-projects rows from the registry; it must not fabricate stream events, got \
         {:?}",
        renderer.events
    );

    let pushes = mid_window
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("the awaited work must have sampled the panel");
    assert!(
        pushes
            .iter()
            .any(|rows| PanelProbeRenderer::draws_queued(rows, &key)),
        "the queued row must already have reached the pane while the awaited subtask was still \
         in flight (that is the C1 window); pushes seen at that instant: {pushes:?}"
    );
    assert!(
        rows.iter().any(|row| row.name == key
            && row.is_active
            && row
                .queue
                .as_ref()
                .is_some_and(|q| (q.position, q.of) == (1, 1))),
        "the loop's own row buffer must have caught up too, got {rows:?}"
    );

    drop(guard);
}
