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

// ---------------------------------------------------------------------------
// `arbiter` — the single owner of the steer-arbitration round machine
// ---------------------------------------------------------------------------

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
    fn active_subtasks(&self, round: usize) -> String {
        format!("snapshot of round {round}")
    }

    fn has_active_work(&self, _round: usize) -> bool {
        true
    }

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
    assert_eq!(arbiter::recorded_response(None, None, &[]), "No decision");

    // Decision without a direct response falls back to the decision name.
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision("RespondDirectly", None, None)),
            None,
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
            &[]
        ),
        "Coder is running the suite."
    );

    // Sleep without a response.
    assert_eq!(
        arbiter::recorded_response(Some(&steer_decision("Sleep", None, Some(12))), None, &[]),
        "Slept for 12s"
    );

    // ForwardToWorker with no posted notice (the sink never posts one here).
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision("ForwardToWorker", None, None)),
            None,
            &[]
        ),
        "Forwarded notice to worker (awaiting specialist reply)"
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
            &forwarded
        ),
        "Forwarded notice notice-1 to coder (awaiting specialist reply), \
         Forwarded notice notice-2 to debugger (awaiting specialist reply)"
    );

    // A synthesized answer beats the decision response and the notices.
    assert_eq!(
        arbiter::recorded_response(
            Some(&steer_decision("DelegateTask", Some("ignored"), None)),
            Some("Synthesized answer"),
            &forwarded
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
                // gate t-070: this fixture asks for `sleep_seconds: 0`; the
                // arbitrator floors that to the sleep tool's own minimum
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
        // t-070: the floored sleep is what the history names — the arbitrator
        // slept `SLEEP_MIN_SECS`, not the requested 0.
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

// ---------------------------------------------------------------------------
// t-035c — one action vocabulary for both layers, durable steering around Sleep,
// a capped chained-sleep budget, and observable inbox-capacity drops
// ---------------------------------------------------------------------------

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
                // Tool-name spellings must come from `crate::tool_names` — a raw
                // literal here trips the repo-wide literal guard (t-054).
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
        let mut subagents = vec![SubagentDetail {
            name: "coder".to_string(),
            task_id: Some(task_id.clone()),
            prompt: "Running a long build".to_string(),
            is_active: true,
            ..Default::default()
        }];

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
        assert!(
            !subagents[0].is_active,
            "action spelling {spelling:?} must mark the subagent inactive"
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
        let subagents: Vec<SubagentDetail> = Vec::new();
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
                subagents: &subagents,
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

// ---------------------------------------------------------------------------
// t-076 — a **sub-second** remainder of the cumulative sleep budget is an
// exhausted budget, not a zero-length sleep. `remaining.is_zero()` is false but
// `remaining.as_secs()` is `0`, so the old `Ok(remaining.as_secs())` returned
// `Ok(0)`, the round machine slept nothing and re-asked the arbitrator
// immediately — the very hot-loop the `SLEEP_MIN_SECS` floor removed, re-created
// because the floor was applied *before* the budget clamp.
// ---------------------------------------------------------------------------

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

/// (t-076 a) Every sub-second remainder is refused through the same typed exhaustion
/// outcome a fully spent budget reports — the caller can tell "budget exhausted" apart
/// from "slept N seconds" — and **nothing** is accounted: no `note_slept(0)`, no
/// extension, no slept seconds on the chain the host reads back.
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

/// (t-076 b) The floor is applied **after** the budget clamp: an inside-the-budget request
/// still comes back clamped, and no reachable call returns a duration below the floor.
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

/// (t-076 c) The **host** chain (`SleepChain::host_sleep_secs`) applies the same
/// refuse-vs-floor policy: a budget that cannot cover the floor is refused with `None`, a
/// sub-minimum request is floored, and no chain state can hand the host a zero-length sleep.
#[test]
fn test_bridge_sleep_chain_host_sleep_refuses_a_sub_minimum_budget_and_floors_the_request() {
    let now = std::time::Instant::now();
    let floor = crate::tool_args::SLEEP_MIN_SECS;
    let limit = arbiter::MAX_CHAINED_SLEEP_SECONDS;

    // Budget fully spent: refusal (this half held before t-076 as well).
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

    // Budget left, request below the floor: floored — the host used to get `Some(0)` and
    // announce a sleep that never happened.
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
/// intact, the drop is counted (delta, not an absolute global) and the bridge surfaces which
/// notice id was dropped — while the newest instruction survives.
#[test]
fn test_bridge_notice_capacity_drop_is_observable_and_inbox_stays_bounded() {
    use crate::orchestrator::notice;

    let target = "t035c-capacity-target";
    let capacity = notice::INBOX_CAPACITY;
    let before = notice::notice_lifecycle_stats();

    for i in 0..capacity {
        notice::post_notice_to_worker(
            target,
            &format!("filler {i}"),
            Some(&format!("t035c-fill-{i}")),
        );
    }
    assert_eq!(
        notice::worker_inbox_len(target),
        capacity,
        "the inbox is filled to exactly its documented capacity"
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = TestRenderer::new();
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;

    let mut forwarded = subtask_with("t-035c-capacity", "forward notice", Some(target));
    forwarded.message = Some("switch to postgres".to_string());

    tx.send(SteerArbEvent::Finished {
        decision: Some(crate::orchestrator::SteerDecision {
            decision: "ForwardToWorker".to_string(),
            response: None,
            tier: None,
            model: None,
            subtasks: vec![forwarded],
            sleep_seconds: None,
        }),
        user_msg: "ask the coder to switch to postgres".to_string(),
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
        notice::worker_inbox_len(target),
        capacity,
        "the INBOX_CAPACITY bound must stay intact"
    );

    // The drop is counted (delta for this key's overflow, never an absolute process total).
    let after = notice::notice_lifecycle_stats();
    let evicted_delta = after.capacity_evicted_total - before.capacity_evicted_total;
    assert!(
        evicted_delta >= 1,
        "a capacity drop must be counted, delta was {evicted_delta}"
    );

    // Drop-oldest: the oldest undelivered notice is gone, the next one and the newest survive.
    assert!(
        crate::orchestrator::get_pending_notice("t035c-fill-0").is_none(),
        "the oldest notice is the one dropped by the capacity policy"
    );
    assert!(
        crate::orchestrator::get_pending_notice("t035c-fill-1").is_some(),
        "only the oldest notice is dropped, not the whole inbox"
    );

    // The drop is observable in the UI, naming the dropped id and the bound.
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
            .any(|s| s.contains("capped at") && s.contains("t035c-fill-0")),
        "the capacity drop must be surfaced to the user, got {statuses:?}"
    );

    // And the instruction itself is still queued for the next seam.
    assert!(
        steer_queue
            .iter()
            .any(|s| s == "ask the coder to switch to postgres"),
        "the steering instruction itself must survive, got {steer_queue:?}"
    );
}

// ---------------------------------------------------------------------------
// t-072 — the paused-stream host (`sink.rs`) resolves a `Sleep` duration through
// the single owner (`arbiter::clamped_sleep_secs`), never with its own default
// and clamp. Host and arbitrator must agree on the number, in the value slept
// *and* in the text the user is shown.
// ---------------------------------------------------------------------------

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

/// (t-072 a) The host's view of a `Sleep` request **is** the owner's clamped
/// value for every input shape — no re-typed default, no host-local clamp.
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

    // The three gate inputs, pinned against the owner's constants instead of numbers.
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
    // are deliberately separate knobs (see `arbiter_upper_budget_is_not_the_sleep
    // _tool_ceiling`); the host may only ever consult the former — pinned by
    // `test_bridge_sink_sleep_site_redeclares_no_default_and_no_clamp`.
}

/// (t-072 b) The trailing host sleep — the extension the paused-stream host may add
/// on top of the arbitration's own sleeps — carries the floored value into the
/// user-visible wording: host text and arbitrator duration never disagree.
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

/// (t-072 c) End-to-end through the paused-stream host: an arbitrator steered into a
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
        let subagents: Vec<SubagentDetail> = Vec::new();

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
                subagents: &subagents,
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

/// (t-072 d) Source-level pin: the sink must **call** the owner instead of
/// re-declaring the sleep resolution. Needsles are assembled at runtime so this
/// test cannot satisfy its own forbidden patterns.
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

/// t-054 guard: no raw tool-name string literals may live anywhere under
/// `src/ui/bridge/`. The needles are built **at runtime** from the
/// `crate::tool_names` constants (mirroring `tests/test_tool_name_literals.rs`),
/// so a newly added constant is covered the moment it exists, and this file
/// itself stays free of the literals it forbids.
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
