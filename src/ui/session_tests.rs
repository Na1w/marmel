use super::helpers::*;
use super::*;
use crate::config::Config;
use crate::llm::ChatClient;
use crate::manager::context::ContextEngine;
use crate::orchestrator::OrchestratorManager;
use crate::types::Message;
use crate::ui::raw::RawRenderer;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn config_for_backend(backend: &str) -> Config {
    Config {
        backend_url: format!("{backend}/v1"),
        system_prompt_path: std::path::PathBuf::from("prompts/system.md"),
        ui_mode: "tui".to_string(),
        ..Config::default()
    }
}

fn completion_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": { "content": text },
                "finish_reason": null
            }]
        })
    )
}

#[tokio::test]
async fn test_ui_run_session_raw_single_turn() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let calls = Arc::new(AtomicUsize::new(0));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &wiremock::Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_string(completion_sse("single reply"))
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let stats = Arc::new(crate::harness::HarnessStats::new());
    let manager = Arc::new(OrchestratorManager::new(
        ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        stats,
    ));

    let mut renderer = RawRenderer::new();
    run_session(&cfg, &mut renderer, Some("goal".to_string()), Some(manager))
        .await
        .expect("run_session should complete without error");

    let backend_calls = calls.load(Ordering::SeqCst);
    assert_eq!(backend_calls, 1);
}

fn tool_call_sse(id: &str, name: &str, arguments: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments
                        }
                    }]
                },
                "finish_reason": null
            }]
        })
    )
}

#[tokio::test]
async fn test_ui_run_session_executes_tool_calls() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let calls = Arc::new(AtomicUsize::new(0));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |req: &wiremock::Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call-glob-1",
                        crate::tool_names::TOOL_GLOB,
                        r#"{"pattern": "Cargo.toml"}"#,
                    ))
                } else {
                    let body_str = String::from_utf8_lossy(&req.body);
                    assert!(
                        body_str.contains("Cargo.toml") || body_str.contains("call-glob-1"),
                        "second LLM request must contain tool result payload"
                    );
                    ResponseTemplate::new(200).set_body_string(completion_sse("Analysis complete."))
                }
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let stats = Arc::new(crate::harness::HarnessStats::new());
    let manager = Arc::new(OrchestratorManager::new(
        ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        stats,
    ));

    let mut renderer = RawRenderer::new();
    run_session(
        &cfg,
        &mut renderer,
        Some("analysera projektet".to_string()),
        Some(manager),
    )
    .await
    .expect("run_session should complete");

    let backend_calls = calls.load(Ordering::SeqCst);
    assert_eq!(backend_calls, 2);
}

struct RecordingRenderer {
    subagents: Vec<SubagentDetail>,
    delegation_events: Vec<DelegationEvent>,
    events: Vec<Event>,
    /// How many times the loop pushed an authoritative row set. A fold that
    /// changes nothing must not push, so the count is part of the contract.
    subagent_pushes: usize,
    input_queue: std::collections::VecDeque<String>,
    /// Abort / user-exit flags (the `Renderer` trait's abort surface).
    input_state: crate::ui::InputState,
}

impl RecordingRenderer {
    fn new() -> Self {
        Self {
            subagents: Vec::new(),
            delegation_events: Vec::new(),
            events: Vec::new(),
            subagent_pushes: 0,
            input_queue: std::collections::VecDeque::new(),
            input_state: crate::ui::InputState::default(),
        }
    }
}

impl Renderer for RecordingRenderer {
    fn init(&mut self) -> Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        self.events.push(event.clone());
        if let Event::Delegation(de) = event {
            self.delegation_events.push(de.clone());
        }
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn poll_input(&mut self) -> Option<String> {
        self.input_queue.pop_front()
    }
    // `read_input` uses the trait default (queued input is only drained by
    // `poll_input` in this double).
    fn input_state(&mut self) -> &mut crate::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &crate::ui::InputState {
        &self.input_state
    }
    fn shutdown(&mut self) {}
    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        self.subagent_pushes += 1;
        self.subagents = subagents;
    }
}

fn test_manager(dir: &tempfile::TempDir) -> OrchestratorManager {
    OrchestratorManager::new(
        crate::llm::ChatClient::new("http://localhost:9999/v1", "test-model"),
        crate::manager::phase::Plan::at(dir.path()),
        Arc::new(crate::harness::HarnessStats::new()),
    )
}

#[test]
fn drain_delegation_events_folds_lifecycle_into_subagent_list() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = test_manager(&tmp);
    let mut renderer = RecordingRenderer::new();
    let mut subagents = Vec::<SubagentDetail>::new();

    {
        let mut ev = manager.delegation_events.lock().unwrap();
        ev.push(DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-1".to_string()),
        });
        ev.push(DelegationEvent::Completed {
            agent: crate::agents::Agent::Coder,
            task: Some("t-1".to_string()),
        });
    }

    drain_delegation_events_with_transcript(Some(&manager), &mut renderer, &mut subagents, None);

    assert_eq!(renderer.delegation_events.len(), 2);
    assert_eq!(subagents.len(), 1);
    assert_eq!(subagents[0].name, "coder-t-1");
    assert!(!subagents[0].is_active);
}

/// Queue state is an **orthogonal** overlay on a lifecycle row: `Queued` installs
/// it while leaving `is_active` and both clocks exactly as `Started` left them,
/// `Running` takes it back down, and closing the delegation takes it down with it.
#[test]
fn queue_state_moves_orthogonally_to_the_lifecycle_row() {
    let mut subagents = Vec::<SubagentDetail>::new();
    let task = || Some("t-011".to_string());

    update_subagent_lifecycle(
        &mut subagents,
        crate::agents::Agent::Coder,
        task(),
        None,
        true,
    );
    assert_eq!(subagents.len(), 1);
    assert_eq!(subagents[0].name, "coder-t-011");
    assert!(subagents[0].is_active);
    assert_eq!(subagents[0].queue, None);
    let started_at = subagents[0]
        .started_at
        .expect("Started must arm the work clock");
    let logs_after_started = subagents[0].logs.len();

    // Queued: the queue field moves, nothing else does. A wait in the queue is
    // never billed as worked time.
    assert!(
        update_subagent_queue(
            &mut subagents,
            crate::agents::Agent::Coder,
            task(),
            Some(QueueState { position: 2, of: 3 }),
        ),
        "an existing row must accept a queue transition"
    );
    assert_eq!(subagents[0].queue, Some(QueueState { position: 2, of: 3 }));
    assert!(
        subagents[0].is_active,
        "a queued delegation is still dispatched, so it stays active"
    );
    assert_eq!(subagents[0].started_at, Some(started_at));
    assert_eq!(subagents[0].worked_duration, std::time::Duration::ZERO);
    assert_eq!(
        subagents[0].logs.len(),
        logs_after_started,
        "a slot transition is state, not a log line"
    );
    assert_eq!(subagents.len(), 1);

    // The pool re-reports position while waiting: the newest value wins.
    assert!(update_subagent_queue(
        &mut subagents,
        crate::agents::Agent::Coder,
        task(),
        Some(QueueState { position: 1, of: 3 }),
    ));
    assert_eq!(subagents[0].queue, Some(QueueState { position: 1, of: 3 }));
    assert_eq!(subagents[0].started_at, Some(started_at));

    // Running: queue cleared, the delegation keeps running on the same clocks.
    assert!(update_subagent_queue(
        &mut subagents,
        crate::agents::Agent::Coder,
        task(),
        None,
    ));
    assert_eq!(subagents[0].queue, None);
    assert!(subagents[0].is_active);
    assert_eq!(subagents[0].started_at, Some(started_at));

    // Completed: lifecycle closed, so the queue field goes down too.
    update_subagent_lifecycle(
        &mut subagents,
        crate::agents::Agent::Coder,
        task(),
        None,
        false,
    );
    assert_eq!(subagents[0].queue, None);
    assert!(!subagents[0].is_active);
    assert_eq!(subagents.len(), 1, "no second row for the same delegation");
}

/// A queue transition with no lifecycle row is dropped, not materialized: rows
/// belong to the `Started`/`Completed` bracket, and inventing one here would
/// leave a permanently active, permanently queued row behind (design §7).
#[test]
fn queue_transitions_never_create_panel_rows() {
    let mut subagents = Vec::<SubagentDetail>::new();

    assert!(
        !update_subagent_queue(
            &mut subagents,
            crate::agents::Agent::Coder,
            Some("t-011".to_string()),
            Some(QueueState { position: 1, of: 2 }),
        ),
        "no row means no queue update"
    );
    assert!(
        !update_subagent_queue(
            &mut subagents,
            crate::agents::Agent::Coder,
            Some("t-011".to_string()),
            None,
        ),
        "clearing a queue that was never installed must not create a row either"
    );
    assert!(
        subagents.is_empty(),
        "queue traffic must never create a panel row, got: {subagents:?}"
    );

    // A row for another task must not be picked up by a mismatched key.
    update_subagent_lifecycle(
        &mut subagents,
        crate::agents::Agent::Coder,
        Some("t-011".to_string()),
        None,
        true,
    );
    assert!(!update_subagent_queue(
        &mut subagents,
        crate::agents::Agent::Coder,
        Some("t-999".to_string()),
        Some(QueueState { position: 1, of: 1 }),
    ));
    assert_eq!(subagents[0].queue, None);
    assert_eq!(subagents.len(), 1);
}

/// The loop's event pump is the seam that turns a delivered arbitration-pause
/// transition into **authoritative panel state**: it folds the transition into the
/// rows every host renders from, re-pushes only on a real change, and still hands
/// the event to the host. That is what lets the TUI panel and the bridge pane
/// agree without a second channel, a timer or a background task.
#[test]
fn forward_ui_event_folds_a_pause_transition_into_the_authoritative_rows() {
    let mut subagents = Vec::<SubagentDetail>::new();
    update_subagent_lifecycle(
        &mut subagents,
        crate::agents::Agent::Coder,
        Some("t-011".to_string()),
        None,
        true,
    );
    assert_eq!(subagents.len(), 1);
    assert!(!subagents[0].paused, "a running row starts unpaused");

    let mut renderer = RecordingRenderer::new();
    let pause = |renderer: &mut RecordingRenderer,
                 subagents: &mut Vec<SubagentDetail>,
                 tag: &str,
                 paused: bool| {
        super::session::forward_ui_event(
            subagents,
            renderer,
            &Event::SubagentStreamPause {
                agent_tag: tag.to_string(),
                paused,
            },
        );
    };

    pause(&mut renderer, &mut subagents, "coder-t-011", true);
    assert!(subagents[0].paused, "the loop's row carries the pause");
    assert!(
        subagents[0].is_active,
        "the pause is orthogonal to the lifecycle row it folds into"
    );
    assert_eq!(
        subagents[0].queue, None,
        "and it says nothing about the queue"
    );
    assert!(
        renderer.subagents.first().is_some_and(|r| r.paused),
        "a real change is pushed to the host"
    );
    assert_eq!(renderer.subagent_pushes, 1);
    assert_eq!(
        renderer.events.len(),
        1,
        "the event still reaches the host's own handler"
    );

    // The same transition twice is not a change, so nothing is re-pushed.
    pause(&mut renderer, &mut subagents, "coder-t-011", true);
    assert_eq!(
        renderer.subagent_pushes, 1,
        "a repeat of a delivered transition must not re-push the pane"
    );
    assert_eq!(renderer.events.len(), 2);

    // The reclaim edge (arbitrator handed the slot back).
    pause(&mut renderer, &mut subagents, "coder-t-011", false);
    assert!(
        !subagents[0].paused,
        "the resume handshake clears the flag on the authoritative row"
    );
    assert_eq!(renderer.subagent_pushes, 2);

    // A pause for a worker this stream owns no row for is dropped on the floor:
    // the registry projection owns row existence.
    pause(&mut renderer, &mut subagents, "coder-t-999", true);
    assert_eq!(subagents.len(), 1, "no row is invented by a pause");
    assert_eq!(
        renderer.subagent_pushes, 2,
        "and nothing is pushed for a row that does not exist"
    );

    // Every other event passes straight through the seam untouched.
    super::session::forward_ui_event(
        &mut subagents,
        &mut renderer,
        &Event::Status("Ready".into()),
    );
    assert_eq!(renderer.subagent_pushes, 2);
    assert_eq!(renderer.events.len(), 5);
    assert!(!subagents[0].paused, "a status line never pauses a row");
}

/// Normal completion through the authoritative drain: the row carries its queue
/// position while the call waits, and no queue state survives the `Completed`
/// arm — drained in the order the stream guarantees.
#[test]
fn drain_queue_state_survives_until_completion_leaves_no_stale_row() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = test_manager(&tmp);
    let mut renderer = RecordingRenderer::new();
    let mut subagents = Vec::<SubagentDetail>::new();
    let coder = crate::agents::Agent::Coder;

    {
        let mut ev = manager.delegation_events.lock().unwrap();
        ev.push(DelegationEvent::Started {
            agent: coder,
            task: Some("t-011".to_string()),
        });
        ev.push(DelegationEvent::Queued {
            agent: coder,
            task: Some("t-011".to_string()),
            position: 2,
            of: 3,
        });
    }
    drain_delegation_events_with_transcript(Some(&manager), &mut renderer, &mut subagents, None);
    assert_eq!(subagents.len(), 1);
    assert_eq!(
        subagents[0].queue,
        Some(QueueState { position: 2, of: 3 }),
        "the queued call must show its place in line"
    );
    assert!(subagents[0].is_active);

    {
        let mut ev = manager.delegation_events.lock().unwrap();
        ev.push(DelegationEvent::Queued {
            agent: coder,
            task: Some("t-011".to_string()),
            position: 1,
            of: 2,
        });
        ev.push(DelegationEvent::Running {
            agent: coder,
            task: Some("t-011".to_string()),
        });
    }
    drain_delegation_events_with_transcript(Some(&manager), &mut renderer, &mut subagents, None);
    assert_eq!(subagents[0].queue, None, "the slot is held: no queue");
    assert!(subagents[0].is_active, "and the call is still running");

    {
        let mut ev = manager.delegation_events.lock().unwrap();
        ev.push(DelegationEvent::Completed {
            agent: coder,
            task: Some("t-011".to_string()),
        });
    }
    drain_delegation_events_with_transcript(Some(&manager), &mut renderer, &mut subagents, None);
    assert_eq!(subagents.len(), 1);
    assert!(!subagents[0].is_active);
    assert_eq!(subagents[0].queue, None, "no stale queue row survives");
    // The renderer saw every arm, in order, and the panel was re-pushed.
    assert_eq!(renderer.delegation_events.len(), 5);
    assert!(matches!(
        renderer.delegation_events[1],
        DelegationEvent::Queued {
            position: 2,
            of: 3,
            ..
        }
    ));
    assert!(matches!(
        renderer.delegation_events[3],
        DelegationEvent::Running { .. }
    ));
    assert_eq!(
        renderer.subagents.len(),
        1,
        "the panel push must carry the same single row"
    );
    assert_eq!(renderer.subagents[0].name, subagents[0].name);
    assert_eq!(renderer.subagents[0].queue, None);
    assert!(!renderer.subagents[0].is_active);
}

/// Failure and cancel while queued: the lifecycle `Failed` arm is what closes the
/// delegation, and it must take the queue state down with it. Neither path ever
/// emits `Running`, so no queue state is left dangling either way.
#[test]
fn drain_queue_state_is_cleared_by_failure_and_by_cancel() {
    for reason in [
        Some("compilation error: unresolved import".to_string()),
        // A cancel that landed while the call waited for a slot.
        Some("Task aborted by user instruction.\n\nFAILED (aborted)".to_string()),
        None,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let manager = test_manager(&tmp);
        let mut renderer = RecordingRenderer::new();
        let mut subagents = Vec::<SubagentDetail>::new();
        let coder = crate::agents::Agent::Coder;

        {
            let mut ev = manager.delegation_events.lock().unwrap();
            ev.push(DelegationEvent::Started {
                agent: coder,
                task: Some("t-011".to_string()),
            });
            ev.push(DelegationEvent::Queued {
                agent: coder,
                task: Some("t-011".to_string()),
                position: 1,
                of: 2,
            });
            ev.push(DelegationEvent::Failed {
                agent: coder,
                task: Some("t-011".to_string()),
                reason: reason.clone(),
            });
        }
        drain_delegation_events_with_transcript(
            Some(&manager),
            &mut renderer,
            &mut subagents,
            None,
        );

        assert_eq!(subagents.len(), 1, "reason: {reason:?}");
        assert!(
            !subagents[0].is_active,
            "reason: {reason:?} must close the delegation"
        );
        assert_eq!(
            subagents[0].queue, None,
            "no queue row may outlive its delegation (reason: {reason:?})"
        );
        assert!(
            manager.delegation_events.lock().unwrap().is_empty(),
            "the drain must consume every event"
        );
    }
}

/// Queue arms with no lifecycle row in the same stream: they still travel to the
/// renderer, but they must not conjure a panel row of their own.
#[test]
fn drain_queue_arms_without_started_emit_without_creating_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = test_manager(&tmp);
    let mut renderer = RecordingRenderer::new();
    let mut subagents = Vec::<SubagentDetail>::new();
    let coder = crate::agents::Agent::Coder;

    {
        let mut ev = manager.delegation_events.lock().unwrap();
        ev.push(DelegationEvent::Queued {
            agent: coder,
            task: Some("t-011".to_string()),
            position: 1,
            of: 1,
        });
        ev.push(DelegationEvent::Running {
            agent: coder,
            task: Some("t-011".to_string()),
        });
    }
    drain_delegation_events_with_transcript(Some(&manager), &mut renderer, &mut subagents, None);

    assert!(
        subagents.is_empty(),
        "queue arms alone must not create rows, got: {subagents:?}"
    );
    assert_eq!(renderer.delegation_events.len(), 2);
    assert!(
        renderer.subagents.is_empty(),
        "the pushed panel list must stay empty too, got: {:?}",
        renderer.subagents
    );
}

#[test]
fn test_is_reset_command_matches_aliases() {
    assert!(is_reset_command("/reset"));
    assert!(is_reset_command("  /reset  "));
    assert!(is_reset_command("/RESET"));
    assert!(is_reset_command("/reset_plan"));
    assert!(is_reset_command("/reset-plan"));
    assert!(is_reset_command("/clear_plan"));
    assert!(is_reset_command("/clear-plan"));
    assert!(is_reset_command("/reset_execution_plan"));
    assert!(!is_reset_command("/q"));
    assert!(!is_reset_command("reset"));
    assert!(!is_reset_command("hello world"));
}

#[test]
fn test_handle_reset_command_clears_plan_and_notifies() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    plan.create("# Plan\n- [ ] [t-1] test\n").unwrap();
    assert!(plan.exists());

    let mut renderer = RecordingRenderer::new();
    let mut ctx = ContextEngine::new(4096);
    ctx.set_system_prompt("sys".to_string());
    ctx.set_goal("goal".to_string());

    handle_reset_command(&plan, &mut renderer, Some(&mut ctx));

    assert!(!plan.exists());
    assert!(
        renderer
            .events
            .iter()
            .any(|ev| matches!(ev, Event::Message(m) if m.contains("cleared and reset")))
    );
    assert!(
        renderer
            .events
            .iter()
            .any(|ev| matches!(ev, Event::Status(s) if s.contains("reset")))
    );
}

#[tokio::test]
async fn test_renderer_sink_handles_reset_command() {
    use crate::llm::StreamSink;
    use crate::ui::bridge::RendererSink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    plan.create("# Plan\n- [ ] [t-1] test\n").unwrap();
    assert!(plan.exists());

    let mut renderer = RecordingRenderer::new();
    renderer.input_queue.push_back("/reset".to_string());

    let mut steer_queue = Vec::new();
    let mut steer_abort_requested = false;
    let (arb_tx, _arb_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_arb_tx2, mut arb_rx) = tokio::sync::mpsc::unbounded_channel();
    let client = ChatClient::new("http://localhost:11434/v1", "test".to_string());
    let stats = Arc::new(crate::harness::HarnessStats::new());
    let mut ctx = ContextEngine::new(4096);

    let mut sink = RendererSink {
        renderer: &mut renderer,
        steer_queue: &mut steer_queue,
        steer_abort_requested: &mut steer_abort_requested,
        arb_tx: &arb_tx,
        arb_rx: &mut arb_rx,
        client: &client,
        stats,
        goal: "goal",
        subagents: &[],
        plan: Some(&plan),
        ctx: Some(&mut ctx),
        steering_history: None,
    };

    let ctrl = sink.poll_control();
    assert!(matches!(ctrl, crate::llm::StreamControl::Continue));
    assert!(
        !plan.exists(),
        "Plan must be cleared after /reset in RendererSink"
    );
    assert!(
        ctx.messages().iter().any(|m| match m {
            Message::User { content } => content.contains("User executed /reset"),
            _ => false,
        }),
        "ContextEngine must receive reset system notice"
    );
}

fn multiple_tool_calls_sse(calls: &[(&str, &str, &str)]) -> String {
    let tool_calls_json: Vec<_> = calls
        .iter()
        .enumerate()
        .map(|(idx, (id, name, args))| {
            serde_json::json!({
                "index": idx,
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": args
                }
            })
        })
        .collect();

    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": tool_calls_json
                },
                "finish_reason": null
            }]
        })
    )
}

#[tokio::test]
async fn test_ui_run_session_executes_parallel_delegations() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let calls = Arc::new(AtomicUsize::new(0));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &wiremock::Request| {
                let call_idx = calls.fetch_add(1, Ordering::SeqCst);
                if call_idx == 0 {
                    ResponseTemplate::new(200).set_body_string(multiple_tool_calls_sse(&[
                        (
                            "call-del-1",
                            crate::tool_names::TOOL_DELEGATE_TASK,
                            r#"{"agent_name": "coder", "prompt": "task 1", "task_id": "t-001"}"#,
                        ),
                        (
                            "call-del-2",
                            crate::tool_names::TOOL_DELEGATE_TASK,
                            r#"{"agent_name": "generalist", "prompt": "task 2", "task_id": "t-002"}"#,
                        ),
                    ]))
                } else {
                    ResponseTemplate::new(200).set_body_string(completion_sse("All tasks completed."))
                }
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let stats = Arc::new(crate::harness::HarnessStats::new());
    let manager = Arc::new(OrchestratorManager::new(
        ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        stats,
    ));

    let mut renderer = RecordingRenderer::new();
    run_session(
        &cfg,
        &mut renderer,
        Some("run parallel tasks".to_string()),
        Some(manager),
    )
    .await
    .expect("run_session should complete");

    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let started_t1 = renderer.delegation_events.iter().any(|ev| match ev {
        DelegationEvent::Started { task, .. } => task.as_deref() == Some("t-001"),
        _ => false,
    });
    let started_t2 = renderer.delegation_events.iter().any(|ev| match ev {
        DelegationEvent::Started { task, .. } => task.as_deref() == Some("t-002"),
        _ => false,
    });
    assert!(started_t1, "task t-001 should be started");
    assert!(started_t2, "task t-002 should be started");
}

// ---------------------------------------------------------------------------
// The boot wiring — `resume_requested`, `boot_if_requested`, `boot`.
//
// These exercise the ONE restore pass the session loop runs before its manager
// loop: `load_bundle` -> floors + `WORKERS` -> the engine -> the panes -> the
// loud report. The fixtures write a real bundle through the real committer, so
// nothing here is mocked at the loader seam.
// ---------------------------------------------------------------------------

use super::session::{
    BootSink, boot_if_requested, build_manager_context, build_restored_context, resume_requested,
    set_resume_request,
};
use crate::session::commit::{CapturePayload, CommitLock, CommitPolicy, write_bundle};
use crate::session::format::TimePoint;
use crate::session::{
    DanglingToolCallState, HistorySource, INTERRUPTED_STATUS_LABEL, IdentityFloors, MANAGER_FILE,
    ManagerSection, MessageRecord, NonRestorableKind, PinWitness, ProcessEnding,
    SESSION_RESTORE_NOTICE_PREFIX, SubagentPanel, ToolCallRecord, ToolFunctionRecord,
    UiJournalRecord, UiSection, WorkerSection, WorkerStatus,
};

/// The resume trigger lives in process-wide statics (the only seam `main` has),
/// so the test that flips them must not overlap the full-loop tests, which read
/// them through `run_session`.
static RESUME_FLAGS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A renderer double WITH a pane model: the restore hooks (`capture_ui`,
/// `restore_panes`, `announce_restore`) are what the boot pass drives, and each
/// call is recorded verbatim for assertions.
#[derive(Debug, Default)]
struct BootRenderer {
    live: UiSection,
    restored_panes: Vec<UiSection>,
    announcements: Vec<crate::session::RepairReport>,
    subagents: Vec<SubagentDetail>,
    events: Vec<Event>,
    input_state: crate::ui::InputState,
}

impl BootRenderer {
    fn status_lines(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Status(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Renderer for BootRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        self.events.push(event.clone());
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn input_state(&mut self) -> &mut crate::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &crate::ui::InputState {
        &self.input_state
    }
    fn shutdown(&mut self) {}
    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        self.subagents = subagents;
    }
    fn capture_ui(&self) -> UiSection {
        self.live.clone()
    }
    fn restore_panes(&mut self, section: &UiSection) {
        self.restored_panes.push(section.clone());
        self.live = section.clone();
    }
    fn announce_restore(&mut self, report: &crate::session::RepairReport) {
        self.announcements.push(report.clone());
    }
}

// ------------------------------------------------------------------- fixtures

fn boot_manager_section(messages: Vec<MessageRecord>) -> ManagerSection {
    let witness = PinWitness {
        system_prompt_len: messages
            .first()
            .map(|record| record.content().map_or(0, |content| content.len()))
            .unwrap_or(0) as u32,
        goal_len: messages
            .get(1)
            .map(|record| record.content().map_or(0, |content| content.len()))
            .unwrap_or(0) as u32,
    };
    ManagerSection {
        pinned_prefix_end: 2,
        pin_witness: witness,
        max_context_tokens: 128,
        rebirth_advisory_emitted: false,
        consecutive_rebirths: 0,
        dangling: DanglingToolCallState::observe(&messages),
        messages,
    }
}

/// The pinned pair plus one tool round trip: `[0]` the prompt, `[1]` the goal
/// verbatim, then an assistant call, its result and a closing assistant turn
/// whose `content` is `Some("")` (a distinction only the bundle keeps).
fn boot_transcript() -> Vec<MessageRecord> {
    vec![
        MessageRecord::System {
            content: "system prompt".to_string(),
        },
        MessageRecord::User {
            content: "the goal".to_string(),
        },
        MessageRecord::Assistant {
            content: None,
            reasoning_content: Some("thinking about the mission".to_string()),
            tool_calls: vec![ToolCallRecord {
                id: "call-1".to_string(),
                kind: "function".to_string(),
                function: ToolFunctionRecord {
                    name: crate::tool_names::TOOL_READ_FILE.to_string(),
                    arguments: "{\"path\":\"src/main.rs\"}".to_string(),
                },
            }],
        },
        MessageRecord::Tool {
            tool_call_id: "call-1".to_string(),
            content: "file bytes".to_string(),
        },
        MessageRecord::Assistant {
            content: Some(String::new()),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ]
}

fn boot_timepoint(elapsed_secs: f64) -> TimePoint {
    TimePoint {
        elapsed_secs,
        wall: crate::session::time::anchor_wall()
            .checked_add_signed(chrono::Duration::seconds(elapsed_secs as i64))
            .expect("in range"),
    }
}

fn boot_worker(key: &str) -> WorkerSection {
    WorkerSection {
        worker_key: key.to_string(),
        worker_id_seq_value: 2,
        role: crate::agents::Agent::Coder,
        task_id: Some("t-012".to_string()),
        status: WorkerStatus::InProgress,
        status_label: "running".to_string(),
        started: boot_timepoint(1.0),
        completed: None,
        last_activity: None,
        worked_secs: 12.5,
        completed_seq: None,
        context_tokens: 64,
        last_tokens: 7,
        counters: Default::default(),
        brief: format!("brief for {key}"),
        snippets: vec!["snippet".to_string()],
        image_urls: vec![],
        audio_urls: vec![],
        recursion_granted: false,
        history: vec![],
        history_source: HistorySource::LiveMirror,
        dangling: DanglingToolCallState::Balanced,
        unrecoverable_reason: None,
        // One `PendingNotice` for what the dead process's notice store still
        // held: the boot wiring must turn it into the loader's own notice
        // sentence (`worker_journal::queued_notice_notices`), never its own.
        non_restorable: vec![crate::session::NonRestorableNotice {
            kind: NonRestorableKind::PendingNotice,
            subject: key.to_string(),
            detail: format!("{key} had one queued notice when the process died"),
        }],
    }
}

fn boot_payload(workers: Vec<WorkerSection>) -> CapturePayload {
    CapturePayload {
        manager: boot_manager_section(boot_transcript()),
        ui: Some(UiSection {
            chat_messages: vec!["User: the goal".to_string(), "[CLI] restore me".to_string()],
            status_line: "phase 2/3".to_string(),
            subagent_panels: vec![SubagentPanel {
                name: "coder-t012-resume".to_string(),
                task_id: Some("t-012".to_string()),
                prompt: "brief for coder-t012-resume".to_string(),
                logs: vec!["started".to_string()],
                thinking: "thinking".to_string(),
                content: "content".to_string(),
                is_active: true,
                context_tokens: 42,
                worked_duration_secs: 5.0,
                started_at: Some(boot_timepoint(1.0)),
                last_activity_at: None,
                turn_thinking: Some("turn thinking".to_string()),
                is_thinking: Some(true),
            }],
            journal: vec![
                UiJournalRecord::User {
                    text: "restored user line".to_string(),
                },
                UiJournalRecord::Status {
                    text: "restored status line".to_string(),
                },
            ],
            tokens_in: 1234,
            tokens_out: 7,
            orchestrator_context_tokens: 42,
            ..UiSection::default()
        }),
        workers,
        identity_floors: IdentityFloors {
            worker_id_seq_floor: 3,
            completed_seq_floor: 1,
            notice_id_floor: 4,
        },
    }
}

/// Write a real bundle where `boot` will look for it: `session::bundle_dir(plan)`.
fn write_boot_bundle(
    plan: &crate::manager::phase::Plan,
    payload: &CapturePayload,
    ending: ProcessEnding,
) -> std::path::PathBuf {
    let bundle = crate::session::bundle_dir(plan);
    std::fs::create_dir_all(&bundle).expect("bundle dir");
    let policy = CommitPolicy {
        min_interval: std::time::Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    write_bundle(
        &bundle,
        "session-boot-1",
        payload,
        ending,
        &policy,
        &CommitLock::new(),
    )
    .expect("the commit must succeed");
    bundle
}

/// Append a byte to one committed member: `byte_len`/`fingerprint` then disagree
/// with the manifest, which is a fail-closed loader refusal.
fn tamper_member(bundle: &std::path::Path, rel: &str) {
    let path = bundle.join(rel);
    let mut bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("read {rel}: {error}"));
    bytes.push(b'\n');
    std::fs::write(&path, &bytes).expect("tamper");
}

/// Flip one byte inside a worker member: same length, wrong fingerprint. Exactly
/// one such member is a tolerated partial, not a refusal.
fn corrupt_worker_member(bundle: &std::path::Path, rel: &str) {
    let path = bundle.join(rel);
    let mut bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("read {rel}: {error}"));
    bytes[10] = b'x';
    std::fs::write(&path, &bytes).expect("corrupt");
}

fn boot_cfg() -> Config {
    config_for_backend("http://127.0.0.1:9")
}

// ---------------------------------------------------------------------- tests

/// Trigger precedence, spelled out: `--fresh` outranks `--resume`, which
/// outranks `MARMEL_RESUME` / `session_resume`. `MARMEL_RESUME` reaches this
/// function already folded into `cfg.session_resume` by
/// `config::apply_session_env`, so the env spelling is the same read.
#[test]
fn resume_trigger_precedence_is_fresh_then_resume_then_env_then_config() {
    let _resume_guard = RESUME_FLAGS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut cfg = config_for_backend("http://localhost:9999");
    cfg.session_resume = false;

    set_resume_request(false, false);
    assert!(!resume_requested(&cfg), "nothing asked: fresh start");

    cfg.session_resume = true;
    assert!(
        resume_requested(&cfg),
        "session_resume = true (or MARMEL_RESUME folded into it) restores"
    );

    set_resume_request(true, false);
    assert!(resume_requested(&cfg), "--resume restores");

    cfg.session_resume = false;
    assert!(resume_requested(&cfg), "--resume alone restores");

    set_resume_request(true, true);
    assert!(
        !resume_requested(&cfg),
        "--fresh wins over --resume and over the config/env spelling"
    );

    set_resume_request(false, true);
    assert!(
        !resume_requested(&cfg),
        "--fresh alone also outranks session_resume = true"
    );

    // Leave the process-wide handoff exactly as the rest of the suite expects it.
    set_resume_request(false, false);
    assert!(!resume_requested(&cfg));
}

/// The fresh path is untouched by the restore pass: with a real bundle sitting
/// on disk and nobody asking for a resume, the pass installs **nothing**, and the
/// prompt this process composed stays at `[0]`.
#[test]
fn fresh_boot_never_reads_a_bundle_and_keeps_the_live_prompt() {
    let _resume_guard = RESUME_FLAGS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_resume_request(false, false);

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let bundle = write_boot_bundle(
        &plan,
        &boot_payload(vec![boot_worker("coder-t012-fresh")]),
        ProcessEnding::UserAbort,
    );
    assert!(
        bundle.join(crate::session::MANIFEST_FILE).exists(),
        "the fixture must really have committed a bundle where a resume would look"
    );

    let cfg = boot_cfg();
    assert!(
        !resume_requested(&cfg),
        "the default Config asks for a fresh run"
    );
    let mut ctx = build_manager_context(&cfg, "the prompt this process composed".to_string());
    assert!(
        ctx.pin_setter_calls() > 0,
        "a fresh run installs its own prompt through a pin writer"
    );

    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let transcript_path = tmp.path().join(".ui_transcript.json");
    let mut session_id = String::new();
    let report = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        boot_if_requested(
            resume_requested(&cfg),
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };
    assert!(
        report.expect("a fresh boot never refuses").is_none(),
        "no resume asked, so no live state may be installed"
    );

    assert_eq!(
        ctx.messages().len(),
        1,
        "only the live system prompt sits in the engine"
    );
    match &ctx.messages()[0] {
        Message::System { content } => assert_eq!(content, "the prompt this process composed"),
        other => panic!("expected the live system prompt, got {other:?}"),
    }
    assert!(renderer.restored_panes.is_empty());
    assert!(renderer.announcements.is_empty());
    assert!(renderer.subagents.is_empty());
    assert!(renderer.events.is_empty());
    assert!(
        crate::orchestrator::workers::worker_state_snapshot("coder-t012-fresh").is_none(),
        "a fresh run must not touch the worker registry"
    );
    assert!(transcript.is_empty());
    assert!(
        !transcript_path.exists(),
        "a fresh boot must not write the legacy transcript through the restore path"
    );
    assert!(session_id.is_empty());
}

/// The one pass, in the required order, through a real bundle: engine vector +
/// floors/`WORKERS` + panes + report + the manager's restore notice.
#[test]
fn resume_boot_installs_manager_history_workers_and_panes_in_one_pass() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let key = "coder-t012-resume";
    let payload = boot_payload(vec![boot_worker(key)]);
    let bundle = write_boot_bundle(&plan, &payload, ProcessEnding::UserAbort);
    let stored_ui = payload.ui.clone().expect("the fixture carries a ui member");

    // Clean the shared registry so the assertions below read only this bundle.
    crate::orchestrator::workers::remove_worker_entry_for_test(key);

    let cfg = boot_cfg();
    let mut ctx = build_restored_context(&cfg);
    assert!(ctx.messages().is_empty(), "the resume engine starts empty");
    assert_eq!(
        ctx.pin_setter_calls(),
        0,
        "the resume branch must not run a pin writer before the restore"
    );

    let mut renderer = BootRenderer {
        // Live counters that deliberately disagree with the stored witnesses:
        // the streams own these numbers, so restore keeps the live ones and
        // reports the difference.
        live: UiSection {
            tokens_in: 11,
            tokens_out: 22,
            orchestrator_context_tokens: 33,
            ..UiSection::default()
        },
        ..BootRenderer::default()
    };
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let report = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        boot_if_requested(
            true,
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };
    let report = report
        .expect("a committed, uncorrupted bundle must not be refused")
        .expect("a bundle existed, so the pass must have restored");
    assert!(
        bundle.join(crate::session::MANIFEST_FILE).exists(),
        "a successful restore must leave the bundle on disk exactly as it was"
    );

    // ---- the manager transcript, installed wholesale and never re-pinned
    assert!(report.restored && !report.partial);
    assert_eq!(report.session_id, "session-boot-1");
    assert_eq!(report.manager_messages, 5);
    assert_eq!(
        ctx.messages().len(),
        6,
        "the 5 stored record(s) plus exactly one synthetic N-MGR-1 notice"
    );
    match &ctx.messages()[0] {
        Message::System { content } => assert_eq!(content, "system prompt"),
        other => panic!("[0] must stay the stored prompt, got {other:?}"),
    }
    match &ctx.messages()[1] {
        Message::User { content } => assert_eq!(content, "the goal"),
        other => panic!("[1] must stay the stored goal, got {other:?}"),
    }
    match ctx
        .messages()
        .last()
        .expect("a restored transcript is never empty")
    {
        Message::User { content } => assert!(
            content.starts_with(SESSION_RESTORE_NOTICE_PREFIX),
            "N-MGR-1 must be the last appended message, got {content}"
        ),
        other => panic!("N-MGR-1 must be a User message, got {other:?}"),
    }
    assert_eq!(
        ctx.pin_setter_calls(),
        0,
        "R-PIN-2: a restore never routes through set_system_prompt / set_goal"
    );
    assert_eq!(
        report.goal, "the goal",
        "the loop continues on the stored goal"
    );
    assert_eq!(report.session_id, "session-boot-1");
    assert_eq!(
        session_id, "session-boot-1",
        "the loop adopts the stored id"
    );
    assert_eq!(report.previous_ending, Some(ProcessEnding::UserAbort));
    let notice = report.restore_notice.clone().expect("N-MGR-1 exists");
    assert!(
        notice.contains(key) && notice.contains(INTERRUPTED_STATUS_LABEL),
        "the manager must be told which worker is interrupted, got {notice}"
    );

    // ---- the worker registry: rebuilt under the stored key, interrupted, and
    // **never relaunched** (policy P-R1)
    assert_eq!(report.workers_installed, 1);
    assert_eq!(report.interrupted_worker_keys, vec![key.to_string()]);
    let state = crate::orchestrator::workers::worker_state_snapshot(key)
        .expect("the restored worker must own its stored key");
    assert_eq!(state.info.status, INTERRUPTED_STATUS_LABEL);
    assert_eq!(state.info.task_id.as_deref(), Some("t-012"));
    assert_eq!(state.info.agent_name, "coder");
    assert!(
        state.is_active(),
        "the entry stays open so cancel/`has_active_workers` still see it"
    );
    assert!(state.completed_seq.is_none());
    assert!(state.info.cancel_token.is_some(), "a fresh token is minted");
    assert!(
        crate::orchestrator::workers::worker_id_seq_floor() >= 3,
        "the identity floors are raised, never assigned"
    );

    // ---- the panes: assigned verbatim, once, and announced
    assert_eq!(
        renderer.restored_panes.len(),
        1,
        "exactly one pane assign per session boot"
    );
    let panes = &renderer.restored_panes[0];
    assert_eq!(
        panes.chat_messages,
        vec!["User: the goal".to_string(), "[CLI] restore me".to_string()],
        "U1: pane bytes are assigned verbatim, never regenerated"
    );
    assert_eq!(
        panes.status_line, "phase 2/3",
        "the status line comes back verbatim"
    );
    assert_eq!(panes.subagent_panels, stored_ui.subagent_panels);
    assert_eq!(renderer.announcements.len(), 1);
    assert_eq!(renderer.subagents.len(), 1);
    assert_eq!(renderer.subagents[0].name, "coder-t012-resume");
    assert_eq!(
        renderer.subagents[0].worked_duration,
        std::time::Duration::from_secs_f64(5.0)
    );

    // ---- the loud report: panes + journal + exactly one prefix-less Status
    let statuses = renderer.status_lines();
    assert_eq!(
        statuses
            .iter()
            .filter(|line| line.starts_with("Session restored (session id session-boot-1"))
            .count(),
        1,
        "one summary line only, got {statuses:?}"
    );
    let journaled: Vec<String> = transcript
        .records()
        .iter()
        .filter_map(|record| match record {
            UiRecord::Status { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        journaled
            .iter()
            .any(|line| line.starts_with("Session restored")),
        "the report must be journaled too, got {journaled:?}"
    );
    assert!(
        journaled
            .iter()
            .any(|line| line.contains("Not restorable after restart")),
        "every D5 loss is surfaced, got {journaled:?}"
    );

    // The restore pass writes no legacy `.ui_transcript.json`: the durable UI
    // journal is the bundle's own `ui` member, and that is what must still be on
    // disk after a resume.
    assert!(
        bundle.join(crate::session::UI_FILE).is_file(),
        "the session bundle's UI member is the only durable UI journal"
    );
    assert!(
        transcript.records().iter().any(
            |record| matches!(record, UiRecord::User { text } if text == "restored user line")
        ),
        "the visible chat history must survive a resume in the legacy journal too"
    );

    // ---- losses are reported, never hidden: the three per-worker capabilities,
    // the two session-wide ones, and the queued notice the dead store lost.
    let kinds: Vec<NonRestorableKind> = report
        .repair
        .notices
        .iter()
        .map(|notice| notice.kind.clone())
        .collect();
    for expected in [
        NonRestorableKind::CancellationToken,
        NonRestorableKind::JoinHandle,
        NonRestorableKind::DropGuard,
        NonRestorableKind::PendingNotice,
        NonRestorableKind::ToolSchemaCharge,
        NonRestorableKind::HarnessStats,
    ] {
        assert!(
            kinds.contains(&expected),
            "{expected:?} must be reported as not restorable, got {kinds:?}"
        );
    }
    assert!(
        report.repair.warnings.iter().any(|warning| warning
            .starts_with("the system prompt this process composed was NOT installed")),
        "the deviation from the composed prompt must be stated, got {:?}",
        report.repair.warnings
    );
    assert!(
        report
            .repair
            .warnings
            .iter()
            .any(|warning| warning.starts_with("tokens_in witness 1234")),
        "the token witnesses are compared and reported, got {:?}",
        report.repair.warnings
    );
    assert!(
        report
            .repair
            .warnings
            .iter()
            .any(|warning| warning.contains("max_context_tokens changed")),
        "a Config change since the bundle is reported, got {:?}",
        report.repair.warnings
    );
    assert!(report.repair.pins_verified);
    assert_eq!(report.repair.workers_restored, 1);

    crate::orchestrator::workers::remove_worker_entry_for_test(key);
}

/// A refused bundle installs **nothing** and leaves no half-session behind; the
/// typed error is what `main` maps onto `session::EXIT_RESTORE_REFUSED` (3).
#[test]
fn refusal_installs_nothing_and_is_carried_as_exit_three() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let key = "coder-t012-refused";
    let bundle = write_boot_bundle(
        &plan,
        &boot_payload(vec![boot_worker(key)]),
        ProcessEnding::UserAbort,
    );
    crate::orchestrator::workers::remove_worker_entry_for_test(key);
    tamper_member(&bundle, MANAGER_FILE);

    let cfg = boot_cfg();
    let mut ctx = build_restored_context(&cfg);
    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let transcript_path = tmp.path().join(".ui_transcript.json");
    let mut session_id = String::new();
    let outcome = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        boot_if_requested(
            true,
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };
    let refused = match outcome {
        Err(refused) => refused,
        Ok(report) => panic!("a tampered member must refuse, got {report:?}"),
    };

    // The exit code the CLI must end on, and the mapping `main` performs.
    assert_eq!(crate::session::EXIT_RESTORE_REFUSED, 3);
    let carried = anyhow::Error::new(refused);
    assert!(
        carried
            .downcast_ref::<crate::session::restore::RestoreRefused>()
            .is_some(),
        "main maps the refusal by type, so the carried error must downcast"
    );
    let shown = carried.to_string();
    assert!(
        shown.contains("session restore refused") && shown.contains("was NOT modified"),
        "the refusal is printed verbatim through `Refusal::display`, got {shown}"
    );

    // Nothing half-installed: engine, registry, panes, session id, transcript.
    assert!(
        ctx.messages().is_empty(),
        "a refusal must leave the engine exactly as it found it"
    );
    assert_eq!(ctx.pin_setter_calls(), 0);
    assert!(
        crate::orchestrator::workers::worker_state_snapshot(key).is_none(),
        "a refusal installs no worker"
    );
    assert!(renderer.restored_panes.is_empty());
    assert!(renderer.announcements.is_empty());
    assert!(renderer.subagents.is_empty());
    assert!(session_id.is_empty());

    // Still loud: the refusal reaches the panes and the journal.
    let statuses = renderer.status_lines();
    assert!(
        statuses
            .iter()
            .any(|line| line.contains("session restore refused")
                && line.contains("was NOT modified")),
        "the refusal sentence must reach the renderer, got {statuses:?}"
    );
    assert!(
        statuses.iter().any(|line| line.contains("exit code 3")),
        "the run must say it stops with exit code 3, got {statuses:?}"
    );
    assert!(
        transcript
            .records()
            .iter()
            .any(|record| matches!(record, UiRecord::Status { text } if text.contains("session restore refused"))),
        "the refusal must be journaled as a Status record"
    );
    // Loudness lives in the in-memory journal above; the pass must not re-create
    // the legacy `.ui_transcript.json` on its way out.
    assert!(
        !transcript_path.exists(),
        "a refused restore never writes the legacy UI journal file"
    );
}

/// A tolerated partial: exactly one corrupt worker member is accepted. The
/// session continues, the good worker is restored-interrupted, the bad one is
/// `Unrecoverable`, and the loss is surfaced.
#[test]
fn partial_restore_continues_and_surfaces_the_unrecoverable_worker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let good = "coder-t012-good";
    let bad = "coder-t012-bad";
    let bundle = write_boot_bundle(
        &plan,
        &boot_payload(vec![boot_worker(good), boot_worker(bad)]),
        ProcessEnding::UserAbort,
    );
    crate::orchestrator::workers::remove_worker_entry_for_test(good);
    crate::orchestrator::workers::remove_worker_entry_for_test(bad);
    corrupt_worker_member(&bundle, &format!("workers/{bad}.json"));

    let cfg = boot_cfg();
    let mut ctx = build_restored_context(&cfg);
    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let outcome = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        boot_if_requested(
            true,
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };
    let report = match outcome {
        Ok(Some(report)) => report,
        Ok(None) => panic!("a partial restore still installs what it can"),
        Err(refused) => panic!("one corrupt worker member is the tolerated partial, got {refused}"),
    };

    assert!(report.restored);
    assert!(report.partial, "RestoreOutcome::PartiallyRestored");
    assert_eq!(report.unrecoverable_workers, vec![bad.to_string()]);
    assert_eq!(report.workers_installed, 2, "both keys are rebuilt");
    assert_eq!(report.interrupted_worker_keys, vec![good.to_string()]);
    assert!(
        report
            .repair
            .notices
            .iter()
            .any(|notice| notice.kind == NonRestorableKind::WorkerHistory && notice.subject == bad),
        "the lost history must be named, got {:?}",
        report.repair.notices
    );
    let journaled: Vec<String> = transcript
        .records()
        .iter()
        .filter_map(|record| match record {
            UiRecord::Status { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        journaled.iter().any(|line| {
            line.starts_with("Session restored") && line.contains("capability(s) not restorable")
        }),
        "the partial restore must be counted out loud, got {journaled:?}"
    );
    assert!(
        journaled.iter().any(|line| line.contains(bad)),
        "the unrecoverable worker must be named in the journaled report, got {journaled:?}"
    );

    // The rest of the session is intact and continues.
    assert_eq!(ctx.messages().len(), 6);
    assert_eq!(ctx.pin_setter_calls(), 0);
    assert_eq!(renderer.restored_panes.len(), 1);
    assert_eq!(renderer.announcements.len(), 1);
    let good_state = crate::orchestrator::workers::worker_state_snapshot(good)
        .expect("the good worker keeps its stored key");
    assert_eq!(good_state.info.status, INTERRUPTED_STATUS_LABEL);
    assert!(good_state.is_active(), "open, and **not** relaunched");
    if let Some(state) = crate::orchestrator::workers::worker_state_snapshot(bad) {
        assert!(
            !state.is_active(),
            "an Unrecoverable worker must never look like an active delegation"
        );
    }

    crate::orchestrator::workers::remove_worker_entry_for_test(good);
    crate::orchestrator::workers::remove_worker_entry_for_test(bad);
}

/// `--resume` with no bundle is the loud fresh start: exit 0, everything says
/// so, nothing is installed.
#[test]
fn resume_without_a_bundle_is_a_loud_fresh_start() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let cfg = boot_cfg();
    let mut ctx = build_restored_context(&cfg);
    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let report = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        boot_if_requested(
            true,
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };
    let report = report.expect("no bundle is never an error");
    assert!(
        report.is_none(),
        "nothing existed, so nothing was installed"
    );
    let statuses = renderer.status_lines();
    assert!(
        statuses
            .iter()
            .any(|line| line.contains("No session bundle at") && line.contains("starting fresh")),
        "the missing bundle must be named out loud, got {statuses:?}"
    );
    assert!(
        transcript
            .records()
            .iter()
            .any(|record| matches!(record, UiRecord::Status { text } if text.contains("No session bundle at"))),
        "the loud fresh start must be journaled as well"
    );
    assert!(ctx.messages().is_empty());
    assert!(
        ctx.pin_setter_calls() == 0,
        "the pass may not touch the pins"
    );
    assert!(renderer.restored_panes.is_empty());
    assert!(renderer.announcements.is_empty());
    assert!(session_id.is_empty());

    // The pass leaves a resume-branch engine with **no** pins — correct for a
    // restore, fatal for a fresh start: the next writer in the session loop is
    // `ctx.set_goal`, which pads `messages[0]` with an empty `Message::System`.
    // The caller contract is explicit (`boot_if_requested`): when a requested
    // resume yields `Ok(None)`, the loop MUST install the composed prompt first.
    ctx.set_system_prompt("composed system prompt".to_string());
    ctx.set_goal("a goal with no bundle behind it".to_string());
    let Message::System { content: prompt } = &ctx.messages()[0] else {
        panic!("the repaired engine must carry the prompt at [0]");
    };
    assert_eq!(prompt, "composed system prompt");
    let Message::User { content: goal } = &ctx.messages()[1] else {
        panic!("the goal must be pinned at [1]");
    };
    assert_eq!(goal, "a goal with no bundle behind it");
}

/// The resume branch builds the engine **without** a system prompt on purpose (a
/// restore may never route through a pin setter), so a resume trigger with **no**
/// bundle must re-install the composed prompt before the goal is pinned; otherwise
/// `set_goal` pads `messages[0]` with an **empty** `System` and the session runs
/// with no system prompt, silently, exit `0`. Asserted on the request that is
/// really sent, through a wiremock backend: `[0]` is byte-identical to a plain
/// fresh start and is the composed prompt. The trigger here is
/// `Config::session_resume`, but because `run_session` reads [`resume_requested`]
/// (which consults the process-wide `FRESH_REQUESTED` and `RESUME_REQUESTED`
/// statics), this test must hold [`RESUME_FLAGS_LOCK`] to avoid racing with
/// tests that mutate those flags.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn resume_without_a_bundle_sends_the_same_request_as_a_fresh_start() {
    let _resume_guard = RESUME_FLAGS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_resume_request(false, false);

    const GOAL: &str = "the same goal in both runs";

    /// One one-turn session. Returns the parsed backend requests, the `Status`
    /// sentences the session surfaced, whether the legacy
    /// `.ui_transcript.json` appeared on disk, and the prompt this process
    /// composes for this plan.
    async fn one_run(session_resume: bool) -> (Vec<serde_json::Value>, Vec<String>, bool, String) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with({
                let bodies = Arc::clone(&bodies);
                move |req: &wiremock::Request| {
                    bodies
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(String::from_utf8_lossy(&req.body).to_string());
                    ResponseTemplate::new(200).set_body_string(completion_sse("single reply"))
                }
            })
            .mount(&server)
            .await;

        let cfg = Config {
            session_resume,
            ..config_for_backend(&server.uri())
        };
        // A private plan dir per run, so `session::bundle_dir(plan)` can never exist:
        // the resume run always lands on the loud fresh start.
        let tmp = tempfile::tempdir().expect("tempdir");
        let plan = crate::manager::phase::Plan::at(tmp.path());
        let composed = load_system_prompt_with_plan_and_warning(&cfg, &plan)
            .expect("the system prompt must compose")
            .0;
        assert!(!composed.is_empty(), "a composed prompt cannot be empty");

        let stats = Arc::new(crate::harness::HarnessStats::new());
        let manager = Arc::new(OrchestratorManager::new(
            ChatClient::new(server.uri(), "marmel-manager"),
            plan,
            stats,
        ));
        // A recording renderer: the session loop writes no `.ui_transcript.json`,
        // so the loud fresh start has a renderer witness and no file witness.
        let mut renderer = BootRenderer::default();
        run_session(&cfg, &mut renderer, Some(GOAL.to_string()), Some(manager))
            .await
            .expect("run_session should complete without error");

        // The legacy journal stays absent: neither run may re-create it under the
        // plan directory (checked while the temp dir is still alive).
        let legacy_journal = tmp.path().join(".ui_transcript.json").is_file();
        let statuses = renderer.status_lines();
        let requests = bodies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|body| serde_json::from_str(body).expect("the backend request must be JSON"))
            .collect();
        (requests, statuses, legacy_journal, composed)
    }

    let (fresh, fresh_statuses, fresh_legacy_journal, fresh_prompt) = one_run(false).await;
    let (resumed, resumed_statuses, resumed_legacy_journal, resumed_prompt) = one_run(true).await;

    for (label, requests, prompt) in [
        ("fresh start", &fresh, &fresh_prompt),
        ("resume with no bundle", &resumed, &resumed_prompt),
    ] {
        assert_eq!(
            requests.len(),
            1,
            "{label}: the Manager must have made exactly one request"
        );
        let messages = requests[0]["messages"]
            .as_array()
            .unwrap_or_else(|| panic!("{label}: no message array in {:?}", requests[0]));
        assert_eq!(
            messages[0]["role"].as_str(),
            Some("system"),
            "{label}: [0] must be the system pin, got {:?}",
            messages[0]
        );
        let sent = messages[0]["content"]
            .as_str()
            .unwrap_or_else(|| panic!("{label}: [0] carries no content: {:?}", messages[0]));
        assert!(
            !sent.is_empty(),
            "{label}: [0] is an empty System message — the prompt was never installed"
        );
        assert_eq!(
            sent, prompt,
            "{label}: [0] must be the composed prompt verbatim"
        );
        assert_eq!(
            messages[1]["role"].as_str(),
            Some("user"),
            "{label}: [1] must be the pinned goal"
        );
        assert_eq!(messages[1]["content"].as_str(), Some(GOAL));
    }

    assert_eq!(
        fresh[0]["messages"], resumed[0]["messages"],
        "a resume trigger with no bundle must send byte-for-byte the fresh request"
    );
    // The only thing a resume trigger may add when there is nothing to restore is
    // noise for the user — the loud fresh start. The session loop writes no
    // `.ui_transcript.json`, so the witness is the renderer, plus a guard that the
    // legacy journal never reappears.
    assert!(
        resumed_statuses
            .iter()
            .any(|line| line.contains("No session bundle at")),
        "the loud fresh start must reach the renderer, got {resumed_statuses:?}"
    );
    for (label, statuses) in [
        ("fresh start", &fresh_statuses),
        ("resume with no bundle", &resumed_statuses),
    ] {
        assert!(
            !statuses
                .iter()
                .any(|line| line.contains("Session restored")),
            "{label}: neither run had a bundle, so neither may claim a restore: {statuses:?}"
        );
    }
    for (label, legacy) in [
        ("fresh start", fresh_legacy_journal),
        ("resume with no bundle", resumed_legacy_journal),
    ] {
        assert!(
            !legacy,
            "{label}: the session loop no longer writes `.ui_transcript.json`, so none may exist"
        );
    }
}

// ---------------------------------------------------------------------------
// Focused boot-path tests.
//
// Five properties of the ONE restore pass the session loop runs before its
// manager loop, each pinned on a real bundle written by the real committer:
//   1. a **fresh** boot never reads the bundle directory (not even a bundle the
//      resume path would refuse), and writes nothing;
//   2. `--resume` with a real bundle installs the manager history
//      element-for-element (`content: null` vs `Some("")` included), keeps the
//      pinned pair byte-identical, never lets the id/seq/notice floors
//      decrease, and assigns the pane strings;
//   3. a refused bundle (a `format_version` this binary does not understand)
//      installs NOTHING — engine, registry, floors, panes, session id — and
//      comes back as the typed refusal `main` maps onto exit 3;
//   4. one corrupt worker file makes exactly that worker `Unrecoverable` while
//      the rest of the session installs, with the loss surfaced out loud;
//   5. trigger precedence through the pass itself: `--fresh` outranks
//      `session_resume = true`, and the `MARMEL_RESUME` spelling reaches the same
//      pass. The `--resume --fresh` usage error and the exit-3 mapping live in
//      `main`, so they are pinned end-to-end in `tests/test_session_boot.rs`.
// Worker keys carry a `t022-` prefix: the worker registry and the identity
// floors are process-wide statics shared with the `orchestrator` and
// `session::worker_journal` suites, so these tests assert only on their own
// keys and treat the floors as monotonic (`>=`), never as exclusively owned.
// ---------------------------------------------------------------------------

use crate::session::{BudgetWitness, ComposerState, RestoreOutcome, ViewState};

/// `(relative path, bytes)` for every file under `root`, sorted: the witness that
/// a pass neither wrote nor mutated anything below the plan directory.
fn tree_bytes(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                out.push((rel, bytes));
            }
        }
    }
    out.sort();
    out
}

/// The engine transcript as wire bytes — what a refusal or a fresh start must
/// leave **identical**, compared element for element.
fn wire_bytes(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| serde_json::to_string(message).expect("a wire message serializes"))
        .collect()
}

/// The three identity floors this process currently owns.
fn identity_floors() -> (u64, u64, u64) {
    (
        crate::orchestrator::workers::worker_id_seq_floor(),
        crate::orchestrator::workers::completed_seq_floor(),
        crate::orchestrator::notice::notice_id_floor(),
    )
}

/// Rewrite the committed `manifest.json`'s `format_version`: the loader refuses it
/// **before** a single member byte is parsed, and because the manifest carries no
/// self-hash it is the one tamper that leaves every member's
/// `byte_len`/`fingerprint` intact.
fn rewrite_manifest_format_version(bundle: &std::path::Path, version: u32) {
    let path = bundle.join(crate::session::MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).expect("manifest");
    let mut manifest: serde_json::Value = serde_json::from_str(&text).expect("manifest json");
    manifest["format_version"] = serde_json::json!(version);
    std::fs::write(&path, serde_json::to_vec(&manifest).expect("reserialize"))
        .expect("write manifest");
}

/// One boot pass, exactly as the session loop calls it.
fn run_boot(
    resume: bool,
    plan: &crate::manager::phase::Plan,
    cfg: &Config,
    ctx: &mut ContextEngine,
    renderer: &mut BootRenderer,
    session_id: &mut String,
    sink: &mut BootSink<'_>,
) -> Result<Option<super::session::BootReport>, crate::session::restore::RestoreRefused> {
    boot_if_requested(resume, plan, cfg, ctx, renderer, session_id, sink)
}

/// A fresh boot is untouched by the restore path. The bundle on disk is one the
/// resume path would **refuse** (an unknown `format_version`), so a pass that so
/// much as opened the directory would say so; a fresh boot says nothing, installs
/// nothing, and leaves every byte under the plan directory alone.
#[test]
fn fresh_boot_never_opens_the_bundle_dir_even_when_it_would_refuse() {
    let _resume_guard = RESUME_FLAGS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_resume_request(false, false);

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let key = "t022-fresh-unread";
    let bundle = write_boot_bundle(
        &plan,
        &boot_payload(vec![boot_worker(key)]),
        ProcessEnding::UserAbort,
    );
    rewrite_manifest_format_version(&bundle, crate::session::FORMAT_VERSION + 99);

    // Control: the fixture is live ammunition, not a no-op. The loader refuses
    // this bundle, so the silence asserted below is earned.
    match crate::session::restore::load_bundle(&bundle) {
        RestoreOutcome::Refused { refusal, .. } => {
            assert_eq!(refusal.code, crate::session::RefusalCode::FormatVersion);
        }
        other => panic!("the fixture must be a real refusal candidate, got {other:?}"),
    }

    let plan_bytes_before = tree_bytes(plan.dir());
    let floors_before = identity_floors();

    let cfg = boot_cfg();
    assert!(
        !cfg.session_resume,
        "the default Config asks for a fresh run"
    );
    assert!(!resume_requested(&cfg), "and no CLI flag was handed over");

    let mut ctx = build_manager_context(&cfg, "the prompt this process composed".to_string());
    let ctx_before = wire_bytes(ctx.messages());
    assert_eq!(
        ctx_before.len(),
        1,
        "a fresh engine starts with just its composed prompt"
    );

    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let transcript_path = tmp.path().join(".ui_transcript.json");
    let mut session_id = String::new();
    let outcome = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        run_boot(
            resume_requested(&cfg),
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };

    assert!(
        outcome.expect("a fresh boot never refuses").is_none(),
        "a fresh boot may not report a restore"
    );

    // Nothing was read: not one word reached the renderer, the journal or the log.
    assert!(
        renderer.events.is_empty(),
        "a fresh boot must not surface a single restore sentence, got {:?}",
        renderer.status_lines()
    );
    assert!(transcript.is_empty());
    assert!(!transcript_path.exists());
    assert!(renderer.restored_panes.is_empty());
    assert!(renderer.announcements.is_empty());
    assert!(renderer.subagents.is_empty());
    assert!(session_id.is_empty(), "the fresh session keeps its own id");

    // Nothing was installed, and nothing was written or mutated.
    assert_eq!(
        wire_bytes(ctx.messages()),
        ctx_before,
        "the engine is exactly what the fresh boot built"
    );
    assert_eq!(
        ctx.pin_setter_calls(),
        1,
        "the only pin writer on this path is the fresh run's own"
    );
    assert!(
        crate::orchestrator::workers::worker_state_snapshot(key).is_none(),
        "the bundle's worker key must not appear in the registry"
    );
    let floors_after = identity_floors();
    assert!(
        floors_after >= floors_before,
        "the identity floors only ever move up, got {floors_before:?} -> {floors_after:?}"
    );
    assert_eq!(
        tree_bytes(plan.dir()),
        plan_bytes_before,
        "a fresh boot never reads, writes or mutates the plan directory"
    );
}

/// A transcript whose records are deliberately *distinguishable*: multibyte text
/// in both pins, a tool-only assistant turn whose `content` is `null`, an
/// assistant turn whose `content` is `Some("")`, a tool result that is the empty
/// string, and tool arguments with hand-spaced JSON that must not be
/// re-canonicalized on the way through the bundle.
const T022_SYSTEM: &str = "system prompt: ünïcødé ✂ rules\nsecond pin line";
const T022_GOAL: &str = "the goal ✅ verbatim\n(second line, ünïcødé)";
const T022_ARGUMENTS: &str = "{ \"path\" : \"src/main.rs\", \"offset\" : 4096 }";

fn t022_transcript() -> Vec<MessageRecord> {
    vec![
        MessageRecord::System {
            content: T022_SYSTEM.to_string(),
        },
        MessageRecord::User {
            content: T022_GOAL.to_string(),
        },
        MessageRecord::Assistant {
            content: None,
            reasoning_content: Some("thinking ✂".to_string()),
            tool_calls: vec![ToolCallRecord {
                id: "call-1".to_string(),
                kind: "function".to_string(),
                function: ToolFunctionRecord {
                    name: crate::tool_names::TOOL_READ_FILE.to_string(),
                    arguments: T022_ARGUMENTS.to_string(),
                },
            }],
        },
        MessageRecord::Tool {
            tool_call_id: "call-1".to_string(),
            content: String::new(),
        },
        MessageRecord::Assistant {
            content: Some(String::new()),
            reasoning_content: None,
            tool_calls: vec![],
        },
        MessageRecord::Assistant {
            content: Some("mission complete ✅".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ]
}

/// The engine transcript reduced to a shape string that keeps the `null` /
/// `""` / missing-key distinctions the wire serializer flattens, so an
/// element-for-element comparison cannot hide a collapse.
fn wire_shape(message: &Message) -> String {
    match message {
        Message::System { content } => format!("system|{content:?}"),
        Message::User { content } => format!("user|{content:?}"),
        Message::Tool {
            tool_call_id,
            content,
        } => format!("tool|{tool_call_id}|{content:?}"),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
        } => {
            let calls: Vec<String> = tool_calls
                .iter()
                .map(|call| {
                    format!(
                        "{}|{}|{}|{}",
                        call.id, call.kind, call.function.name, call.function.arguments
                    )
                })
                .collect();
            format!(
                "assistant|null-content={}|reasoning={}|calls={}",
                content.is_none(),
                reasoning_content.as_deref().unwrap_or("<absent>"),
                calls.join(";")
            )
        }
    }
}

/// Payload builder for the boot-path tests: the manager member, the pane member,
/// the worker members and the identity floors are all chosen per test.
fn t022_payload(
    manager: ManagerSection,
    ui: UiSection,
    workers: Vec<WorkerSection>,
    floors: IdentityFloors,
) -> CapturePayload {
    CapturePayload {
        manager,
        ui: Some(ui),
        workers,
        identity_floors: floors,
    }
}

/// Floors a fixture may lawfully claim: every one of them sits above the highest
/// counter value its own workers recorded, which is what
/// `IdentityFloors::sanity_violations` refuses otherwise.
fn t022_floors() -> IdentityFloors {
    IdentityFloors {
        worker_id_seq_floor: 3,
        completed_seq_floor: 1,
        notice_id_floor: 4,
    }
}

/// Mirrors the private `orchestrator::workers::MAX_RECENT_COMPLETED`: the
/// restored registry is a bounded working set, and eviction always takes the
/// **lowest** `completed_seq` first. That cap is the only lawful way a restored
/// closed entry can leave the process-wide registry again — and because the
/// registry is a static shared with the `orchestrator` and
/// `session::worker_journal` suites, the eviction may be triggered, and counted,
/// by a concurrent test rather than by this one.
const COMPLETED_RING_CAP: usize = 10;

/// The manager history comes back element for element — every record in the
/// bundle becomes the corresponding engine message, `content: null` stays
/// distinct from `content: ""`, the tool arguments keep their exact bytes, and
/// `[0]`/`[1]` are byte-identical to what was stored.
#[test]
fn resume_boot_restores_the_manager_history_element_for_element() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let cfg = boot_cfg();

    let stored = t022_transcript();
    let mut manager = boot_manager_section(stored.clone());
    // Budget and engine flags this bundle actually spent.
    manager.max_context_tokens = cfg.max_context_tokens;
    manager.rebirth_advisory_emitted = true;
    manager.consecutive_rebirths = 3;
    let payload = t022_payload(
        manager.clone(),
        UiSection::default(),
        vec![boot_worker("t022-element-for-element")],
        t022_floors(),
    );
    write_boot_bundle(&plan, &payload, ProcessEnding::HardKillInferred);

    let mut ctx = build_restored_context(&cfg);
    let ctx_before = wire_bytes(ctx.messages());
    let floors_before = identity_floors();
    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let report = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        run_boot(
            true,
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    }
    .expect("a clean bundle is never refused")
    .expect("a bundle existed");

    assert_eq!(report.manager_messages, stored.len());
    assert!(
        ctx_before.is_empty(),
        "the resume engine starts with nothing to preserve"
    );

    // ---- element for element, in stored order, with the restore notice last
    let expected: Vec<String> = stored
        .iter()
        .cloned()
        .map(|record| wire_shape(&record.into_wire()))
        .collect();
    let installed: Vec<String> = ctx.messages().iter().map(wire_shape).collect();
    assert_eq!(
        installed[..expected.len()],
        expected,
        "every stored record must be installed in order, unchanged"
    );
    assert_eq!(
        installed.len(),
        expected.len() + 1,
        "exactly one synthetic N-MGR-1 notice is appended"
    );
    assert!(
        ctx.messages()
            .last()
            .expect("a restored transcript is never empty")
            .content()
            .unwrap_or_default()
            .starts_with(SESSION_RESTORE_NOTICE_PREFIX),
        "N-MGR-1 is the last message"
    );

    // ---- `content: null` vs `content: ""` vs a `""` tool result
    let Message::Assistant { content, .. } = &ctx.messages()[2] else {
        panic!(
            "[2] must be the tool-only assistant turn, got {:?}",
            ctx.messages()[2]
        );
    };
    assert!(
        content.is_none(),
        "a tool-only turn stored with `content: null` must not become `Some(\"\")`"
    );
    let Message::Assistant { content, .. } = &ctx.messages()[4] else {
        panic!(
            "[4] must be the empty-content assistant turn, got {:?}",
            ctx.messages()[4]
        );
    };
    assert_eq!(
        content.as_deref(),
        Some(""),
        "`content: \"\"` must not collapse into `null`"
    );
    let Message::Tool {
        tool_call_id,
        content,
    } = &ctx.messages()[3]
    else {
        panic!("[3] must be the tool result, got {:?}", ctx.messages()[3]);
    };
    assert_eq!((tool_call_id.as_str(), content.as_str()), ("call-1", ""));

    // ---- the tool call itself: id, type, name and the opaque argument string
    let Message::Assistant { tool_calls, .. } = &ctx.messages()[2] else {
        unreachable!()
    };
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "call-1");
    assert_eq!(tool_calls[0].kind, "function");
    assert_eq!(
        tool_calls[0].function.name,
        crate::tool_names::TOOL_READ_FILE
    );
    assert_eq!(
        tool_calls[0].function.arguments, T022_ARGUMENTS,
        "tool arguments are an opaque string and must not be re-canonicalized"
    );

    // ---- the pinned pair, byte for byte, and never rewritten
    let Message::System { content } = &ctx.messages()[0] else {
        panic!("[0] must stay the stored prompt")
    };
    assert_eq!(content.as_bytes(), T022_SYSTEM.as_bytes());
    let Message::User { content } = &ctx.messages()[1] else {
        panic!("[1] must stay the stored goal")
    };
    assert_eq!(content.as_bytes(), T022_GOAL.as_bytes());
    assert_eq!(
        content.len(),
        manager.pin_witness.goal_len as usize,
        "the pin witness still matches the restored bytes"
    );
    assert_eq!(ctx.pinned_prefix_end(), 2, "the pinned window is as stored");
    assert_eq!(
        ctx.pin_setter_calls(),
        0,
        "R-PIN-2: the restore never routes through a pin setter"
    );
    assert!(report.repair.pins_verified);
    assert_eq!(
        report.goal, T022_GOAL,
        "the loop continues on the stored goal"
    );

    // ---- the engine scalars the loader carried
    assert!(
        ctx.rebirth_advisory_emitted(),
        "the rebirth latch the previous process had already spent is restored"
    );
    assert_eq!(ctx.consecutive_rebirths(), 3, "and its counter with it");
    assert!(
        !report
            .repair
            .warnings
            .iter()
            .any(|warning| warning.contains("max_context_tokens changed")),
        "an unchanged budget must not be reported as changed, got {:?}",
        report.repair.warnings
    );

    // ---- the floors were raised, never lowered
    let floors_after = identity_floors();
    assert!(
        floors_after >= floors_before,
        "identity floors only move up: {floors_before:?} -> {floors_after:?}"
    );

    crate::orchestrator::workers::remove_worker_entry_for_test("t022-element-for-element");
}

/// The pane strings come back verbatim — `chat_messages` 1:1 with empty and
/// duplicate lines kept, `plan_content` byte-identical, the streaming partials,
/// composer, view and panel state included — and they are **assigned**, not
/// appended.
#[test]
fn resume_boot_assigns_every_pane_string_verbatim_including_the_plan_pane() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let key = "t022-pane-presenter";

    let mut budgets = std::collections::BTreeMap::new();
    budgets.insert("coder".to_string(), 4096);
    let stored_ui = UiSection {
        chat_messages: vec![
            "User: the goal ✅".to_string(),
            String::new(),
            "[Tool Call] read_file(\"src/main.rs\")".to_string(),
            "User: the goal ✅".to_string(),
        ],
        status_line: "phase 2/3 · coder working".to_string(),
        subagent_panels: vec![SubagentPanel {
            name: key.to_string(),
            task_id: Some("t-022".to_string()),
            prompt: "brief for the panel".to_string(),
            logs: vec!["started".to_string(), "ran read_file".to_string()],
            thinking: "panel thinking ✂".to_string(),
            content: "panel content".to_string(),
            is_active: true,
            context_tokens: 77,
            worked_duration_secs: 6.25,
            started_at: Some(boot_timepoint(1.0)),
            last_activity_at: None,
            turn_thinking: Some("turn thinking".to_string()),
            is_thinking: Some(true),
        }],
        journal: vec![
            UiJournalRecord::User {
                text: "the goal ✅".to_string(),
            },
            UiJournalRecord::ToolCall {
                display: "read_file(src/main.rs)".to_string(),
            },
            UiJournalRecord::Status {
                text: "phase 1/3".to_string(),
            },
        ],
        composer: ComposerState {
            input_text: "half-typed ✂\nsecond line".to_string(),
            cursor: 14,
            input_draft: "the draft".to_string(),
            history: vec!["first goal".to_string(), "second goal".to_string()],
            history_index: Some(1),
        },
        view: ViewState {
            chat_scroll: 3,
            plan_scroll: 1,
            subagent_scroll: 0,
            subagent_list_scroll: 2,
            selected_subagent_idx: 0,
            focused_panel: "plan".to_string(),
            show_thought: true,
            show_subagent_panel: true,
            show_plan_panel: true,
            chat_auto_scroll: false,
            subagent_autoscroll: true,
            plan_auto_scroll: true,
        },
        budgets: BudgetWitness {
            thinking_budgets: budgets,
            default_thinking_budget: 1024,
        },
        plan_content: "# Execution plan\n- [x] [t-001] first ✅\n- [ ] [t-002] ünïcødé task\n"
            .to_string(),
        had_active_plan: true,
        active_plan_task: Some("t-002".to_string()),
        plan_start: Some(boot_timepoint(2.0)),
        plan_completed: None,
        current_thought: "a half-streamed thin".to_string(),
        current_content: "a half-streamed answ".to_string(),
        active_agent: Some("coder".to_string()),
        steer_sentence_buffer: Some("half a sente".to_string()),
        tokens_in: 111,
        tokens_out: 22,
        orchestrator_context_tokens: 333,
        abort_armed: true,
        abort_armed_at: Some(boot_timepoint(3.0)),
    };
    let payload = t022_payload(
        boot_manager_section(boot_transcript()),
        stored_ui.clone(),
        vec![boot_worker(key)],
        t022_floors(),
    );
    write_boot_bundle(&plan, &payload, ProcessEnding::UserAbort);

    let cfg = boot_cfg();
    let mut ctx = build_restored_context(&cfg);
    // The live screen before the restore: a DIFFERENT pane buffer, so an append
    // would be visible.
    let mut renderer = BootRenderer {
        live: UiSection {
            chat_messages: vec!["a line this process drew itself".to_string()],
            ..UiSection::default()
        },
        ..BootRenderer::default()
    };
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let report = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        run_boot(
            true,
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    }
    .expect("a clean bundle is never refused")
    .expect("a bundle existed");

    // ---- exactly one assign, and it carries the stored section unchanged
    assert_eq!(renderer.restored_panes.len(), 1);
    assert_eq!(
        renderer.restored_panes[0], stored_ui,
        "the whole pane section must arrive unchanged"
    );
    let panes = &renderer.restored_panes[0];

    // ---- the chat pane, 1:1 with what the previous screen showed
    assert_eq!(
        panes.chat_messages,
        vec![
            "User: the goal ✅".to_string(),
            String::new(),
            "[Tool Call] read_file(\"src/main.rs\")".to_string(),
            "User: the goal ✅".to_string(),
        ],
        "U1–U3: empty lines and duplicate lines are pane content, not noise"
    );
    assert_eq!(
        panes.plan_content.as_bytes(),
        stored_ui.plan_content.as_bytes(),
        "the plan pane comes back byte for byte, not re-rendered from the plan file"
    );
    assert_eq!(
        panes.plan_content,
        "# Execution plan\n- [x] [t-001] first ✅\n- [ ] [t-002] ünïcødé task\n"
    );
    assert_eq!(panes.current_thought, "a half-streamed thin");
    assert_eq!(panes.current_content, "a half-streamed answ");
    assert_eq!(panes.status_line, "phase 2/3 · coder working");
    assert_eq!(panes.active_agent.as_deref(), Some("coder"));
    assert_eq!(panes.steer_sentence_buffer.as_deref(), Some("half a sente"));
    assert_eq!(panes.active_plan_task.as_deref(), Some("t-002"));
    assert!(panes.had_active_plan);
    assert_eq!(panes.composer.input_text, "half-typed ✂\nsecond line");
    assert_eq!(panes.composer.cursor, 14);
    assert_eq!(panes.view.focused_panel, "plan");
    assert_eq!(panes.budgets.default_thinking_budget, 1024);

    // ---- assigned, never appended: the live buffer is replaced wholesale
    assert_eq!(
        renderer.live.chat_messages, stored_ui.chat_messages,
        "restore assigns the pane vector; it never appends to what is on screen"
    );
    assert_eq!(renderer.live.plan_content, stored_ui.plan_content);

    // ---- the report mirrors the same panes, and the subagent list is handed to
    // the loop as well as through the renderer hook
    assert!(report.ui_restored);
    assert_eq!(report.subagent_panels, stored_ui.subagent_panels);
    assert_eq!(report.journal, stored_ui.journal);
    assert_eq!(renderer.subagents.len(), 1);
    let detail = &renderer.subagents[0];
    assert_eq!(detail.name, key);
    assert_eq!(detail.task_id.as_deref(), Some("t-022"));
    assert_eq!(detail.prompt, "brief for the panel");
    assert_eq!(detail.logs, vec!["started", "ran read_file"]);
    assert_eq!(detail.thinking, "panel thinking ✂");
    assert_eq!(detail.content, "panel content");
    assert!(detail.is_active);
    assert_eq!(detail.context_tokens, 77);
    assert_eq!(
        detail.worked_duration,
        std::time::Duration::from_secs_f64(6.25)
    );

    // ---- the counters stay the live session's, and the disagreement is reported
    assert!(
        report
            .repair
            .warnings
            .iter()
            .any(|warning| warning.starts_with("tokens_in witness 111")),
        "the stored counters are compared and reported, never silently adopted, got {:?}",
        report.repair.warnings
    );

    // ---- the visible history continues from the restored journal
    assert!(
        transcript
            .records()
            .iter()
            .any(|record| matches!(record, UiRecord::User { text } if text == "the goal ✅")),
        "the legacy transcript must be re-seeded from the restored journal"
    );

    crate::orchestrator::workers::remove_worker_entry_for_test(key);
}

/// A worker section with an explicit identity: status, the `WORKER_ID_SEQ` value
/// it drew, and the `COMPLETED_SEQ` it drew when it had one.
fn t022_worker(
    key: &str,
    status: WorkerStatus,
    worker_id_seq_value: u64,
    completed_seq: Option<u64>,
) -> WorkerSection {
    let mut section = boot_worker(key);
    let label = match status {
        WorkerStatus::InProgress => "running",
        WorkerStatus::Completed => "completed",
        WorkerStatus::Failed => "failed",
        WorkerStatus::Aborted => "aborted",
        WorkerStatus::Unrecoverable => "unrecoverable",
        WorkerStatus::Unknown => "unknown",
    };
    section.status = status;
    section.status_label = label.to_string();
    section.worker_id_seq_value = worker_id_seq_value;
    section.completed_seq = completed_seq;
    if completed_seq.is_some() {
        // A closed record carries its own completion instant; the installer
        // re-anchors that instant rather than inventing one.
        section.completed = Some(boot_timepoint(2.0));
    }
    section.counters.implementation_turns = 4;
    section.counters.validation_rounds = 2;
    section.counters.latest_validator_feedback = Some("round 2 said: rerun".to_string());
    section
}

/// Every worker key in the bundle is rebuilt under exactly that key, with its
/// own id/`completed_seq`/counter values carried verbatim, and the three
/// process-wide identity floors only ever move up.
#[test]
fn resume_boot_rebuilds_every_worker_key_and_never_lowers_the_identity_floors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let running_a = "t022-worker-running-a";
    let running_b = "t022-worker-running-b";
    let finished = "t022-worker-finished";

    let (ids_before, seqs_before, notices_before) = identity_floors();
    let id_floor = ids_before + 20;
    let seq_floor = seqs_before + 20;
    let notice_floor = notices_before + 20;
    // The closed record's `COMPLETED_SEQ` is drawn just under the floor this
    // bundle claims, so it is the newest closed entry at install time: the
    // bounded completed ring evicts the **lowest** first, and that ring is a
    // process-wide static shared with the `orchestrator` and
    // `session::worker_journal` suites running concurrently in this binary.
    let closed_seq = seqs_before + 19;

    let workers = vec![
        t022_worker(running_a, WorkerStatus::InProgress, ids_before + 1, None),
        t022_worker(running_b, WorkerStatus::InProgress, ids_before + 2, None),
        t022_worker(
            finished,
            WorkerStatus::Completed,
            ids_before + 3,
            Some(closed_seq),
        ),
    ];
    let stored: Vec<WorkerSection> = workers.clone();
    for section in &workers {
        crate::orchestrator::workers::remove_worker_entry_for_test(&section.worker_key);
    }
    let payload = t022_payload(
        boot_manager_section(boot_transcript()),
        UiSection::default(),
        workers,
        IdentityFloors {
            worker_id_seq_floor: id_floor,
            completed_seq_floor: seq_floor,
            notice_id_floor: notice_floor,
        },
    );
    write_boot_bundle(&plan, &payload, ProcessEnding::HardKillInferred);

    let cfg = boot_cfg();
    let mut ctx = build_restored_context(&cfg);
    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let report = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        run_boot(
            true,
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    }
    .expect("a clean bundle is never refused")
    .expect("a bundle existed");

    // ---- the key set is identical, in the stored order
    assert_eq!(report.workers_installed, 3);
    assert!(report.unrecoverable_workers.is_empty());
    let mut expected_keys: Vec<String> = stored.iter().map(|w| w.worker_key.clone()).collect();
    expected_keys.sort();
    // `live_worker_states()` is the process-wide registry. Its bounded completed
    // ring (`orchestrator::workers::MAX_RECENT_COMPLETED` = [`COMPLETED_RING_CAP`],
    // lowest `completed_seq` evicted first) is shared with the `orchestrator` and
    // `session::worker_journal` suites running concurrently in this test binary,
    // so a restored **closed** entry can be evicted again by another suite's
    // completion. What the restore owns, and what is pinned strictly, is that
    // every OPEN key comes back under exactly its stored name, and that a missing
    // closed key is explained by a full ring rather than by the restore.
    let live = crate::orchestrator::workers::live_worker_states();
    let completed_present = live
        .iter()
        .filter(|(_, state)| state.completed_seq.is_some())
        .count();
    let ring_was_full =
        report.repair.completed_dropped_by_ring_cap > 0 || completed_present >= COMPLETED_RING_CAP;
    let mut live_keys: Vec<String> = live.into_iter().map(|(key, _)| key).collect();
    live_keys.retain(|key| key.starts_with("t022-worker-"));
    live_keys.sort();
    assert!(
        live_keys.contains(&running_a.to_string()) && live_keys.contains(&running_b.to_string()),
        "an open restored entry is never evicted and never renamed: {live_keys:?}"
    );
    if live_keys.contains(&finished.to_string()) {
        assert_eq!(
            live_keys, expected_keys,
            "the registry is rebuilt under exactly the stored keys, with no id drift"
        );
    } else {
        assert!(
            ring_was_full,
            "a restored closed entry may only leave the registry through the bounded \
             completed ring: {live_keys:?}, {completed_present} completed entries present, \
             dropped_by_ring_cap={}",
            report.repair.completed_dropped_by_ring_cap
        );
    }

    // ---- each entry keeps the identity its own record carried
    for section in &stored {
        let state = match crate::orchestrator::workers::worker_state_snapshot(&section.worker_key) {
            Some(state) => state,
            None => {
                // The one lawful way a restored key can be absent again is the
                // bounded completed ring, which only ever drops CLOSED entries —
                // and never an open one.
                assert_eq!(
                    section.worker_key, finished,
                    "an OPEN restored entry may never leave the registry"
                );
                assert!(
                    ring_was_full,
                    "a closed entry may only be absent because the shared completed ring \
                     was full: {completed_present} completed entries present, \
                     dropped_by_ring_cap={}",
                    report.repair.completed_dropped_by_ring_cap
                );
                continue;
            }
        };
        assert_eq!(
            state.worker_id_seq_value, section.worker_id_seq_value,
            "the WORKER_ID_SEQ value the previous process drew is kept, not re-drawn"
        );
        assert_eq!(state.completed_seq, section.completed_seq);
        assert_eq!(state.info.agent_name, "coder");
        assert_eq!(state.info.task_id.as_deref(), Some("t-012"));
        assert_eq!(state.info.prompt, section.brief);
        assert_eq!(state.info.context_tokens, section.context_tokens);
        assert_eq!(state.last_tokens, section.last_tokens);
        assert_eq!(state.info.implementation_turns, 4);
        assert_eq!(state.info.validation_rounds, 2);
        assert_eq!(
            state.info.latest_validator_feedback.as_deref(),
            Some("round 2 said: rerun")
        );
    }

    // ---- the interrupted pair stays open (never relaunched, policy P-R1) and the
    // completed one stays closed with its stored sequence
    assert_eq!(
        report.interrupted_worker_keys,
        vec![running_a.to_string(), running_b.to_string()]
    );
    for key in [running_a, running_b] {
        let state = crate::orchestrator::workers::worker_state_snapshot(key).expect("installed");
        assert_eq!(state.info.status, INTERRUPTED_STATUS_LABEL);
        assert!(
            state.is_active(),
            "open, so cancel/`has_active_workers` still see it"
        );
        assert!(state.completed_seq.is_none());
    }
    let done = crate::orchestrator::workers::worker_state_snapshot(finished);
    if let Some(done) = done {
        assert_eq!(done.completed_seq, Some(closed_seq));
        assert_eq!(
            done.info.status, "completed",
            "a terminal status label is kept verbatim, not rewritten to interrupted"
        );
        assert!(
            !done.is_active(),
            "a completed worker must not look active again"
        );
        assert!(done.completed_info().is_some());
    } else {
        assert!(
            ring_was_full,
            "the closed entry left the bounded ring, and that ring was demonstrably full: \
             {completed_present} completed entries present, dropped_by_ring_cap={}",
            report.repair.completed_dropped_by_ring_cap
        );
    }

    // ---- the floors moved up to the bundle's claim and nowhere down
    let (ids_after, seqs_after, notices_after) = identity_floors();
    assert!(
        ids_after >= ids_before && ids_after >= id_floor,
        "WORKER_ID_SEQ: {ids_before} -> {ids_after} (bundle floor {id_floor})"
    );
    assert!(
        seqs_after >= seqs_before && seqs_after >= seq_floor,
        "COMPLETED_SEQ: {seqs_before} -> {seqs_after} (bundle floor {seq_floor})"
    );
    assert!(
        notices_after >= notices_before && notices_after >= notice_floor,
        "NEXT_NOTICE_ID: {notices_before} -> {notices_after} (bundle floor {notice_floor})"
    );
    assert!(
        seqs_after > closed_seq,
        "the next completion can never tie with or sort before the restored one ({closed_seq})"
    );

    for key in [running_a, running_b, finished] {
        crate::orchestrator::workers::remove_worker_entry_for_test(key);
    }
}

/// A bundle this binary refuses (here: a `format_version` it does not
/// understand) installs **nothing** — not a message, not a worker, not a floor,
/// not a pane line — leaves the bundle bytes untouched, and comes back as the
/// typed `RestoreRefused` that `main` maps onto `session::EXIT_RESTORE_REFUSED`
/// (3). The engine deliberately starts **non-empty** here so "unchanged" is a
/// witness rather than a vacuity.
#[test]
fn refusal_on_an_ununderstood_format_version_installs_nothing_and_is_exit_three() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let keys = ["t022-worker-refused-a", "t022-worker-refused-b"];
    let bundle = write_boot_bundle(
        &plan,
        &t022_payload(
            boot_manager_section(boot_transcript()),
            UiSection::default(),
            keys.iter().map(|key| boot_worker(key)).collect(),
            t022_floors(),
        ),
        ProcessEnding::UserAbort,
    );
    for key in keys {
        crate::orchestrator::workers::remove_worker_entry_for_test(key);
    }
    rewrite_manifest_format_version(&bundle, crate::session::FORMAT_VERSION + 7);
    let bundle_bytes_before = tree_bytes(&bundle);

    let cfg = boot_cfg();
    // An engine with content in it, so "nothing was installed" is measurable.
    let mut ctx = build_restored_context(&cfg);
    ctx.set_system_prompt("the prompt this process composed".to_string());
    ctx.set_goal("a goal this process was about to run".to_string());
    let ctx_before = wire_bytes(ctx.messages());
    let pins_before = ctx.pin_setter_calls();
    let floors_before = identity_floors();

    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let outcome = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        run_boot(
            true,
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };

    // ---- the typed verdict, with its code, member and the two version numbers
    let refused = match outcome {
        Err(refused) => refused,
        Ok(report) => panic!("an unparseable version must refuse, got {report:?}"),
    };
    assert_eq!(
        refused.refusal.code,
        crate::session::RefusalCode::FormatVersion
    );
    assert_eq!(
        refused.refusal.member.as_deref(),
        Some(crate::session::MANIFEST_FILE)
    );
    assert_eq!(
        refused.bundle, bundle,
        "the refusal names the bundle it refused"
    );
    let message = &refused.refusal.message;
    assert!(
        message.contains(&(crate::session::FORMAT_VERSION + 7).to_string())
            && message.contains(&crate::session::FORMAT_VERSION.to_string()),
        "the refusal quotes both the stored and the understood version, got {message}"
    );

    // ---- the exit code `main` maps it onto, and the downcast `main` performs
    assert_eq!(crate::session::EXIT_RESTORE_REFUSED, 3);
    let spoken = refused.to_string();
    assert!(
        spoken.contains("session restore refused") && spoken.contains("was NOT modified"),
        "the refusal sentence names the bundle and promises it was not touched: {spoken}"
    );
    let carried = anyhow::Error::new(refused);
    let seen_by_main = carried
        .downcast_ref::<crate::session::restore::RestoreRefused>()
        .expect("main maps the refusal by type; a boxed session error must downcast");
    assert_eq!(
        seen_by_main.refusal.code,
        crate::session::RefusalCode::FormatVersion
    );
    assert_eq!(
        carried.to_string(),
        spoken,
        "what `main` prints is the refusal's own sentence, byte for byte"
    );

    // ---- nothing half-installed
    assert_eq!(
        wire_bytes(ctx.messages()),
        ctx_before,
        "the engine keeps exactly what this process put in it"
    );
    assert_eq!(
        ctx.pin_setter_calls(),
        pins_before,
        "the pass ran no pin writer of its own"
    );
    for key in keys {
        assert!(
            crate::orchestrator::workers::worker_state_snapshot(key).is_none(),
            "{key} must not have been installed under any key"
        );
    }
    // The floors are process-wide and other suites may move them, so the honest
    // statement is the design's own: they never move down — and nothing here
    // moved them up, because no member was ever parsed.
    let floors_after = identity_floors();
    assert!(
        floors_after >= floors_before,
        "a refusal never lowers a floor: {floors_before:?} -> {floors_after:?}"
    );
    assert!(renderer.restored_panes.is_empty(), "no pane assign");
    assert!(renderer.announcements.is_empty(), "no repair announcement");
    assert!(renderer.subagents.is_empty(), "no subagent list installed");
    assert!(session_id.is_empty(), "the session keeps its own id");

    // ---- the bundle itself is byte-identical: a refusal never repairs in place
    assert_eq!(
        tree_bytes(&bundle),
        bundle_bytes_before,
        "the refused bundle is left exactly as it was found"
    );

    // ---- and it is loud in all three destinations
    let statuses = renderer.status_lines();
    assert!(
        statuses
            .iter()
            .any(|line| line.contains("session restore refused")
                && line.contains("was NOT modified")),
        "the refusal reaches the panes, got {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .any(|line| line.contains("exit code 3") && line.contains("formatVersion")),
        "the panes state the exit code and the refusal code, got {statuses:?}"
    );
    assert!(
        transcript
            .records()
            .iter()
            .any(|record| matches!(record, UiRecord::Status { text } if text.contains("session restore refused"))),
        "the refusal is journaled as a Status record too"
    );
}

/// The tolerated partial, pinned through the boot pass. One corrupt worker
/// member makes **that** worker `Unrecoverable` — its key and task are kept, its
/// history is not restored, and it must never read as an in-flight delegation —
/// while everything else in the same bundle installs: the manager transcript
/// element for element, both sound workers under their own keys with their own
/// `WORKER_ID_SEQ` values and the interrupted label, and the pane strings
/// including the plan pane. The loss is then counted out loud in the panes, the
/// journal and the manager's own restore notice, and the damaged member is never
/// repaired in place.
#[test]
fn one_corrupt_worker_file_is_the_tolerated_partial_and_the_rest_installs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let good_a = "t022-partial-good-a";
    let good_b = "t022-partial-good-b";
    let bad = "t022-partial-bad";

    let stored = t022_transcript();
    let stored_ui = UiSection {
        chat_messages: vec![
            "User: the goal".to_string(),
            "[Tool Call] run_command(cargo test)".to_string(),
        ],
        status_line: "phase 3/4".to_string(),
        plan_content: "# Execution plan\n- [ ] [t-022] survive a broken worker\n".to_string(),
        tokens_in: 55,
        ..UiSection::default()
    };
    let floors = IdentityFloors {
        worker_id_seq_floor: 5,
        completed_seq_floor: 2,
        notice_id_floor: 6,
    };
    let payload = t022_payload(
        boot_manager_section(stored.clone()),
        stored_ui.clone(),
        vec![
            t022_worker(good_a, WorkerStatus::InProgress, 1, None),
            t022_worker(good_b, WorkerStatus::InProgress, 2, None),
            t022_worker(bad, WorkerStatus::InProgress, 3, None),
        ],
        floors,
    );
    let bundle = write_boot_bundle(&plan, &payload, ProcessEnding::HardKillInferred);
    for key in [good_a, good_b, bad] {
        crate::orchestrator::workers::remove_worker_entry_for_test(key);
    }
    // Exactly one member is damaged: same byte length, wrong fingerprint.
    corrupt_worker_member(&bundle, &format!("workers/{bad}.json"));
    let bundle_bytes_after_damage = tree_bytes(&bundle);

    let (ids_before, seqs_before, notices_before) = identity_floors();
    let cfg = boot_cfg();
    let mut ctx = build_restored_context(&cfg);
    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let outcome = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        run_boot(
            true,
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };
    let report = match outcome {
        Ok(Some(report)) => report,
        Ok(None) => panic!("a bundle exists, so the pass must have run"),
        Err(refused) => {
            panic!("one corrupt worker member is the tolerated partial, got {refused}")
        }
    };

    // ---- the verdict: restored, flagged partial, naming exactly one worker
    assert!(report.restored);
    assert!(
        report.partial,
        "RestoreOutcome::PartiallyRestored must reach the boot report"
    );
    assert_eq!(report.unrecoverable_workers, vec![bad.to_string()]);
    assert_eq!(
        report.workers_installed, 3,
        "the damaged key is rebuilt as well, as an Unrecoverable record"
    );

    // ---- the rest of the session installed anyway: transcript …
    assert_eq!(report.manager_messages, stored.len());
    let expected: Vec<String> = stored
        .iter()
        .cloned()
        .map(|record| wire_shape(&record.into_wire()))
        .collect();
    let installed: Vec<String> = ctx.messages().iter().map(wire_shape).collect();
    assert_eq!(
        installed[..expected.len()],
        expected,
        "a partial restore still installs the whole manager transcript"
    );
    assert_eq!(installed.len(), expected.len() + 1);

    // … the panes …
    assert_eq!(renderer.restored_panes.len(), 1);
    assert_eq!(renderer.live.chat_messages, stored_ui.chat_messages);
    assert_eq!(
        renderer.live.plan_content, stored_ui.plan_content,
        "the plan pane survives a broken worker file"
    );
    assert_eq!(renderer.live.status_line, "phase 3/4");
    assert_eq!(session_id, "session-boot-1", "the restored id is adopted");

    // … and the two sound workers, each under its own key with its own id value
    assert_eq!(
        report.interrupted_worker_keys,
        vec![good_a.to_string(), good_b.to_string()]
    );
    let live = crate::orchestrator::workers::live_worker_states();
    let completed_present = live
        .iter()
        .filter(|(_, state)| state.completed_seq.is_some())
        .count();
    let ring_was_full =
        report.repair.completed_dropped_by_ring_cap > 0 || completed_present >= COMPLETED_RING_CAP;
    let mut live_keys: Vec<String> = live.into_iter().map(|(key, _)| key).collect();
    live_keys.retain(|key| key.starts_with("t022-partial-"));
    live_keys.sort();
    assert!(
        live_keys.contains(&good_a.to_string()) && live_keys.contains(&good_b.to_string()),
        "the two sound keys come back under exactly their stored names: {live_keys:?}"
    );
    for (key, id_value) in [(good_a, 1u64), (good_b, 2)] {
        let state = crate::orchestrator::workers::worker_state_snapshot(key)
            .expect("a sound worker keeps its stored key");
        assert_eq!(state.info.status, INTERRUPTED_STATUS_LABEL);
        assert!(state.is_active(), "open, and never relaunched");
        assert_eq!(state.completed_seq, None);
        assert_eq!(
            state.worker_id_seq_value, id_value,
            "{key} keeps the WORKER_ID_SEQ the previous process drew"
        );
        assert_eq!(state.info.agent_name, "coder");
        assert_eq!(state.info.prompt, format!("brief for {key}"));
    }

    // ---- the damaged one is kept by name and closed out of the active set. It is
    // closed the moment it is installed (it carried no `completed_seq`), so the
    // process-wide bounded ring may take it again; if it is absent, that must be
    // the ring and never the installer.
    if !live_keys.contains(&bad.to_string()) {
        assert!(
            ring_was_full,
            "the damaged record left the registry through the shared bounded ring, not \
             through the installer: {live_keys:?}, {completed_present} completed entries present, \
             dropped_by_ring_cap={}",
            report.repair.completed_dropped_by_ring_cap
        );
    }
    match crate::orchestrator::workers::worker_state_snapshot(bad) {
        Some(broken) => {
            assert!(
                !broken.is_active(),
                "an Unrecoverable entry must never look like an in-flight delegation"
            );
            assert_eq!(
                broken.info.status, "unrecoverable",
                "the label says what happened"
            );
            let closed_seq = broken
                .completed_seq
                .expect("with no stored completed_seq the existing completion path closes it");
            assert!(closed_seq >= floors.completed_seq_floor);
        }
        None => assert!(
            ring_was_full,
            "{bad} was installed (`workers_installed` says so) and then evicted by the full \
             shared ring: {completed_present} completed entries present, dropped_by_ring_cap={}",
            report.repair.completed_dropped_by_ring_cap
        ),
    }

    // ---- the identity floors never move down, and the bundle's claim is honoured
    let (ids_after, seqs_after, notices_after) = identity_floors();
    assert!(ids_after >= ids_before && ids_after >= floors.worker_id_seq_floor);
    assert!(seqs_after >= seqs_before && seqs_after >= floors.completed_seq_floor);
    assert!(notices_after >= notices_before && notices_after >= floors.notice_id_floor);

    // ---- the loss is itemized, and only the damaged worker is itemized as lost
    let notices = &report.repair.notices;
    assert!(
        notices.iter().any(|notice| {
            notice.kind == NonRestorableKind::WorkerHistory
                && notice.subject == bad
                && notice.detail.contains("failed verification")
        }),
        "the loader must say which member failed verification, got {notices:?}"
    );
    assert!(
        notices.iter().any(|notice| {
            notice.kind == NonRestorableKind::Other
                && notice.subject == bad
                && notice.detail.contains("its history is gone")
        }),
        "name and task kept, history gone, must be stated, got {notices:?}"
    );
    assert_eq!(
        notices
            .iter()
            .filter(
                |notice| notice.kind == NonRestorableKind::WorkerHistory && notice.subject == bad
            )
            .count(),
        1,
        "one worker's lost history is itemized exactly once, never twice, got {notices:?}"
    );
    assert!(
        notices.iter().any(|notice| {
            notice.kind == NonRestorableKind::Other
                && notice.subject == bad
                && notice.detail.contains("its history is gone")
        }),
        "name and task kept, history gone, must be stated, got {notices:?}"
    );
    assert!(
        !notices
            .iter()
            .any(|notice| notice.kind == NonRestorableKind::WorkerHistory
                && (notice.subject == good_a || notice.subject == good_b)),
        "a sound worker's history is never reported as lost, got {notices:?}"
    );
    let warnings = &report.repair.warnings;
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains(&format!("worker member workers/{bad}.json"))),
        "the offending member is named among the warnings, got {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains(bad) && warning.contains("carried no completed_seq")),
        "closing the unrecoverable record through the existing path is reported, got {warnings:?}"
    );

    // ---- loud in all three destinations
    assert_eq!(renderer.announcements.len(), 1);
    assert!(
        renderer.announcements[0]
            .notices
            .iter()
            .any(|notice| notice.subject == bad),
        "announce_restore carries the loss to the panes"
    );
    let journaled: Vec<String> = transcript
        .records()
        .iter()
        .filter_map(|record| match record {
            UiRecord::Status { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        journaled
            .iter()
            .any(|line| line.starts_with("Session restored")
                && line.contains("capability(s) not restorable")),
        "the partial verdict is journaled, got {journaled:?}"
    );
    assert!(
        journaled
            .iter()
            .any(|line| line.contains(bad) && line.contains("failed verification")),
        "the journal names the member that failed, got {journaled:?}"
    );
    let summary = renderer
        .status_lines()
        .last()
        .cloned()
        .expect("the boot pass speaks a summary line");
    assert!(summary.starts_with("Session restored (session id session-boot-1"));
    assert!(
        summary.contains(&format!("{} manager message(s)", stored.len()))
            && summary.contains("3 worker(s)"),
        "the summary counts what installed: {summary}"
    );
    assert!(
        summary.contains(&format!(
            "{} capability(s) not restorable",
            report.repair.notices.len()
        )),
        "the count spoken out loud is the count reported, got {summary}"
    );

    // ---- and the manager's own restore notice names the worker that lost history
    let notice = report
        .restore_notice
        .clone()
        .expect("every restore ends with N-MGR-1");
    assert!(notice.starts_with(SESSION_RESTORE_NOTICE_PREFIX));
    assert!(
        notice.contains(bad),
        "the notice names the worker by key: {notice}"
    );
    assert!(
        notice.contains("unrecoverable"),
        "the notice states the unrecoverable history: {notice}"
    );

    // ---- the pass never repairs the bundle in place
    assert_eq!(
        tree_bytes(&bundle),
        bundle_bytes_after_damage,
        "a tolerated partial leaves the damaged member exactly as damaged"
    );

    for key in [good_a, good_b, bad] {
        crate::orchestrator::workers::remove_worker_entry_for_test(key);
    }
}

/// The precedence is pinned where it actually bites — inside the boot pass, not
/// only in the trigger read. With `session_resume = true` in the Config **and** a
/// real, fully restorable bundle on disk, `--fresh` still means the pass is never
/// entered: `boot_if_requested` answers `None`, the engine keeps what this
/// process put in it, no worker key appears, no pane is assigned, nothing is
/// journaled, the session keeps its own id, and every byte under the plan
/// directory is untouched. Flipping the flag to `--resume` on the **same** bundle
/// and Config then installs everything, which is what makes the silence above a
/// precedence result rather than a broken fixture.
#[test]
fn the_fresh_flag_outranks_session_resume_inside_the_boot_pass() {
    let _resume_guard = RESUME_FLAGS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let key = "t022-fresh-outranks";
    let stored_ui = UiSection {
        chat_messages: vec!["User: the goal from the previous process".to_string()],
        status_line: "phase 1/2".to_string(),
        ..UiSection::default()
    };
    write_boot_bundle(
        &plan,
        &t022_payload(
            boot_manager_section(boot_transcript()),
            stored_ui.clone(),
            vec![t022_worker(key, WorkerStatus::InProgress, 1, None)],
            t022_floors(),
        ),
        ProcessEnding::UserAbort,
    );
    crate::orchestrator::workers::remove_worker_entry_for_test(key);

    let mut cfg = boot_cfg();
    cfg.session_resume = true;

    // ---------------------------------------------------------------- `--fresh`
    set_resume_request(false, true);
    assert!(
        !resume_requested(&cfg),
        "--fresh must outrank session_resume = true before the pass is even called"
    );

    let plan_bytes_before = tree_bytes(plan.dir());
    let floors_before = identity_floors();
    let mut ctx = build_restored_context(&cfg);
    ctx.set_goal("the goal this process is about to run".to_string());
    let ctx_before = wire_bytes(ctx.messages());
    let pins_before = ctx.pin_setter_calls();
    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let transcript_path = tmp.path().join(".ui_transcript.json");
    let mut session_id = String::new();
    let entered = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        run_boot(
            resume_requested(&cfg),
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };
    let report = entered.expect("a fresh boot is never a refusal");
    assert!(
        report.is_none(),
        "--fresh never enters the restore pass, not even with session_resume = true: {report:?}"
    );
    assert_eq!(
        wire_bytes(ctx.messages()),
        ctx_before,
        "the engine keeps exactly what this process put in it"
    );
    assert_eq!(ctx.pin_setter_calls(), pins_before, "no pin writer ran");
    assert!(
        crate::orchestrator::workers::worker_state_snapshot(key).is_none(),
        "the bundle's worker key must not exist after a fresh boot"
    );
    assert!(renderer.restored_panes.is_empty(), "no pane assign");
    assert!(renderer.announcements.is_empty(), "no repair announcement");
    assert!(renderer.subagents.is_empty(), "no subagent list");
    assert!(
        renderer.live.chat_messages.is_empty(),
        "the panes stay as this process left them"
    );
    assert!(session_id.is_empty(), "the session keeps its own id");
    assert!(transcript.records().is_empty(), "nothing is journaled");
    assert!(
        !transcript_path.exists(),
        "and no legacy transcript file is created"
    );
    let floors_after_fresh = identity_floors();
    assert!(
        floors_after_fresh >= floors_before,
        "a fresh boot never lowers a floor"
    );
    assert_eq!(
        tree_bytes(plan.dir()),
        plan_bytes_before,
        "--fresh does not so much as open the bundle directory"
    );

    // ---------------------------------------------------------------- `--resume`
    // Same bundle, same Config, same engine: only the flag moved.
    set_resume_request(true, false);
    assert!(
        resume_requested(&cfg),
        "--resume on the same Config restores"
    );
    let entered = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        run_boot(
            resume_requested(&cfg),
            &plan,
            &cfg,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    };
    let report = entered
        .expect("the bundle is restorable; only the flag changed")
        .expect("a bundle existed");
    assert!(report.restored);
    assert!(!report.partial);
    assert_eq!(report.workers_installed, 1);
    assert_eq!(report.interrupted_worker_keys, vec![key.to_string()]);
    assert!(
        crate::orchestrator::workers::worker_state_snapshot(key).is_some(),
        "the same key installs once the flag asks for it"
    );
    assert_eq!(
        renderer.live.chat_messages, stored_ui.chat_messages,
        "and the panes are assigned"
    );
    assert_eq!(renderer.live.status_line, "phase 1/2");
    assert_eq!(session_id, "session-boot-1");
    assert!(!transcript.records().is_empty(), "the restore is journaled");
    assert_eq!(
        ctx.messages().len(),
        boot_transcript().len() + 1,
        "the stored transcript plus the single N-MGR-1 notice"
    );
    assert_eq!(
        ctx.pin_setter_calls(),
        pins_before,
        "the restored [0]/[1] are installed, never re-set by this process"
    );

    set_resume_request(false, false);
    crate::orchestrator::workers::remove_worker_entry_for_test(key);
}

/// The `MARMEL_RESUME` spelling reaches the same boot pass, with **no**
/// command-line flag set. Three layers are pinned in one place: the env value
/// folded into `Config` by `config::apply_session_env` (the recognized
/// spellings, the cancelling spellings, and an unrecognized value keeping the
/// file's answer), the trigger read `resume_requested` with the process-wide flag
/// statics at rest, and — decisively — a real restore through `boot_if_requested`
/// driven only by `resume_requested(&cfg)`, plus the same bundle installing
/// nothing once the env cancels the file.
#[test]
fn marmel_resume_env_reaches_the_boot_pass_and_restores() {
    use crate::config::SESSION_RESUME_ENV;

    let _resume_guard = RESUME_FLAGS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // The file says "don't resume"…
    let mut off_by_file = boot_cfg();
    off_by_file.session_resume = false;
    // …and here the file says "resume".
    let mut on_by_file = boot_cfg();
    on_by_file.session_resume = true;

    // ---- the recognized affirmative spellings all trigger restore
    for spelling in ["1", "true", "yes", "on"] {
        let cfg = crate::config::apply_session_env(
            off_by_file.clone(),
            Some(spelling.to_string()),
            None,
            None,
        );
        assert!(
            cfg.session_resume,
            "{SESSION_RESUME_ENV}={spelling} must ask for a restore over session_resume = false"
        );
        assert!(
            cfg.session_enabled,
            "asking for a restore never turns capture off"
        );
    }
    // ---- the recognized negative spellings cancel the file's `session_resume = true`
    for spelling in ["0", "false", "no", "off"] {
        let cfg = crate::config::apply_session_env(
            on_by_file.clone(),
            Some(spelling.to_string()),
            None,
            None,
        );
        assert!(
            !cfg.session_resume,
            "{SESSION_RESUME_ENV}={spelling} must cancel session_resume = true"
        );
    }
    // ---- an unrecognized value is reported and ignored, never guessed
    let unchanged =
        crate::config::apply_session_env(on_by_file.clone(), Some("maybe".to_string()), None, None);
    assert!(
        unchanged.session_resume,
        "an unrecognized value keeps the file's session_resume = true"
    );
    let unchanged_off = crate::config::apply_session_env(
        off_by_file.clone(),
        Some("maybe".to_string()),
        None,
        None,
    );
    assert!(
        !unchanged_off.session_resume,
        "and it keeps the file's session_resume = false"
    );

    // ---- the trigger read, with no command-line flag set at all
    let env_on =
        crate::config::apply_session_env(off_by_file.clone(), Some("yes".to_string()), None, None);
    let env_off =
        crate::config::apply_session_env(on_by_file.clone(), Some("0".to_string()), None, None);
    set_resume_request(false, false);
    assert!(
        resume_requested(&env_on),
        "{SESSION_RESUME_ENV}=yes alone asks for the restore pass"
    );
    assert!(
        !resume_requested(&env_off),
        "{SESSION_RESUME_ENV}=0 alone cancels it, flag statics included"
    );
    assert!(
        !resume_requested(&off_by_file),
        "the control: with nothing set anywhere, the pass is not entered"
    );

    // ---- and the env spelling really restores: the pass is driven by exactly
    // `resume_requested(&cfg)`, which is what the session loop hands it.
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::manager::phase::Plan::at(tmp.path());
    let key = "t022-env-resume";
    let stored_ui = UiSection {
        chat_messages: vec!["User: the goal the env asked to resume".to_string()],
        ..UiSection::default()
    };
    write_boot_bundle(
        &plan,
        &t022_payload(
            boot_manager_section(boot_transcript()),
            stored_ui.clone(),
            vec![t022_worker(key, WorkerStatus::InProgress, 1, None)],
            t022_floors(),
        ),
        ProcessEnding::HardKillInferred,
    );
    crate::orchestrator::workers::remove_worker_entry_for_test(key);

    let mut ctx = build_restored_context(&env_on);
    let mut renderer = BootRenderer::default();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let report = {
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        run_boot(
            resume_requested(&env_on),
            &plan,
            &env_on,
            &mut ctx,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
    }
    .expect("the bundle is sound")
    .expect("MARMEL_RESUME=yes entered the pass");
    assert!(report.restored);
    assert_eq!(report.workers_installed, 1);
    assert_eq!(report.interrupted_worker_keys, vec![key.to_string()]);
    assert_eq!(report.manager_messages, boot_transcript().len());
    assert_eq!(ctx.messages().len(), boot_transcript().len() + 1);
    assert_eq!(renderer.live.chat_messages, stored_ui.chat_messages);
    assert_eq!(session_id, "session-boot-1");
    assert!(
        crate::orchestrator::workers::worker_state_snapshot(key).is_some(),
        "the env spelling installed the worker under its stored key"
    );

    // ---- the cancelling spelling: same bundle, same pass, nothing installed
    let ctx_before = wire_bytes(ctx.messages());
    let mut second_renderer = BootRenderer::default();
    let mut second_transcript = UiTranscript::new();
    let mut second_session_id = String::new();
    let entered = {
        let mut sink = BootSink {
            ui_transcript: &mut second_transcript,
        };
        run_boot(
            resume_requested(&env_off),
            &plan,
            &env_off,
            &mut ctx,
            &mut second_renderer,
            &mut second_session_id,
            &mut sink,
        )
    };
    assert!(
        entered.expect("not a refusal").is_none(),
        "{SESSION_RESUME_ENV}=0 must keep the pass out, despite session_resume = true in the file"
    );
    assert_eq!(wire_bytes(ctx.messages()), ctx_before);
    assert!(second_renderer.restored_panes.is_empty());
    assert!(second_session_id.is_empty());

    set_resume_request(false, false);
    crate::orchestrator::workers::remove_worker_entry_for_test(key);
}

// ---------------------------------------------------------------------------
// The strong half of the "values match" branch.
//
// From an integration test `plan_scroll` can only ever be pinned at `0`: that
// offset's upper bound is `TuiRenderer::plan_max_scroll`, a per-frame display
// value derived while drawing and therefore `0` in a process that has not drawn
// yet — so every stored non-zero plan offset lands in the criterion's *second*
// branch (the clamp-difference case, pinned in `tests/test_session_restore_ui.rs`)
// and the "restored value equals the stored value" branch is never exercised for
// it. The geometry is `pub(crate)`, so that branch can only be pinned in-crate:
// give the live clamp room, then demand verbatim equality for all twelve view
// values.
// ---------------------------------------------------------------------------

/// Four panels, the selected one (`[2]`) carrying enough content that the live
/// subagent clamp genuinely has room for the stored offset — otherwise
/// `subagent_scroll` would be pinned on a vacuous fixture.
fn ac_c3_panels() -> Vec<SubagentPanel> {
    (0..4)
        .map(|i| SubagentPanel {
            name: format!("coder-t014d-unit{i}"),
            task_id: Some(format!("t-014d-unit-{i}")),
            prompt: format!("brief for panel {i}"),
            logs: (0..6)
                .map(|k| format!("log line {k} of panel {i}"))
                .collect(),
            thinking: format!("thinking of panel {i}"),
            content: if i == 2 {
                (0..30)
                    .map(|k| format!("content line {k} of the selected panel"))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                format!("content of panel {i}")
            },
            is_active: false,
            context_tokens: 40 + i,
            worked_duration_secs: 12.5 + i as f64,
            started_at: Some(boot_timepoint(1.0)),
            last_activity_at: Some(boot_timepoint(2.0)),
            turn_thinking: Some(format!("turn thinking of panel {i}")),
            is_thinking: Some(false),
        })
        .collect()
}

#[test]
fn ac_c3_view_offsets_and_toggles_come_back_verbatim_when_the_clamp_has_room() {
    let stored = UiSection {
        chat_messages: (0..40)
            .map(|i| format!("User: pane line {i} {}", "x".repeat(90)))
            .collect(),
        // A bar no default could produce, and one the renderer's own live rules
        // never rewrite on their own ("Ready" only arrives from an event).
        status_line: "Running (unit-model) — 2 task(s) in flight".to_string(),
        subagent_panels: ac_c3_panels(),
        composer: ComposerState {
            input_text: String::new(),
            cursor: 0,
            input_draft: String::new(),
            history: vec![],
            history_index: None,
        },
        view: ViewState {
            chat_scroll: 24,
            plan_scroll: 12,
            subagent_scroll: 6,
            subagent_list_scroll: 2,
            selected_subagent_idx: 2,
            focused_panel: "subagents".to_string(),
            show_thought: true,
            show_subagent_panel: true,
            show_plan_panel: true,
            chat_auto_scroll: false,
            subagent_autoscroll: true,
            plan_auto_scroll: false,
        },
        plan_content: "step 1 done\nstep 2 in flight\nstep 3 pending".to_string(),
        ..UiSection::default()
    };
    assert_eq!(
        stored.structural_violations(),
        Vec::<String>::new(),
        "a U-chk violation would make this a loader refusal, not a restore"
    );

    // A real renderer, with the live geometry that has room for the stored
    // offsets: `plan_max_scroll = 20` is what a drawn frame would have derived
    // for this plan box.
    let mut renderer = crate::ui::tui::TuiRenderer::new();
    renderer.plan_max_scroll = 20;

    // The hook call is the instant the criterion speaks about: `boot` hands the
    // committed section to exactly this hook (pinned element-by-element in
    // `tests/test_session_restore_ui.rs`). Reading the renderer any later would
    // compare something else — the boot pass ends with one `on_event(Event::Status)`,
    // whose frame flush re-reads the disk-backed plan box (`render.rs`) and moves
    // `plan_scroll`/`plan_auto_scroll` to what that frame shows. That is the
    // documented behaviour of a disk-backed pane, not a restore difference.
    renderer.restore_panes(&stored);
    let live = renderer.capture_ui();
    assert_eq!(
        live.view, stored.view,
        "AC-C3: the restored view differs from the committed one, and the restore \
         reports no clamp warning to excuse it"
    );
    for (field, got, want) in [
        (
            "chat_scroll",
            u64::from(live.view.chat_scroll),
            u64::from(stored.view.chat_scroll),
        ),
        (
            "plan_scroll",
            u64::from(live.view.plan_scroll),
            u64::from(stored.view.plan_scroll),
        ),
        (
            "subagent_scroll",
            u64::from(live.view.subagent_scroll),
            u64::from(stored.view.subagent_scroll),
        ),
        (
            "subagent_list_scroll",
            u64::from(live.view.subagent_list_scroll),
            u64::from(stored.view.subagent_list_scroll),
        ),
        (
            "selected_subagent_idx",
            live.view.selected_subagent_idx as u64,
            stored.view.selected_subagent_idx as u64,
        ),
    ] {
        assert_eq!(
            got, want,
            "AC-C3: {field} came back as {got}, the bundle holds {want}"
        );
    }
    assert_eq!(
        live.view.plan_scroll, 12,
        "AC-C3: a non-zero restored `plan_scroll` must survive once the live \
         clamp has room for it (plan_max_scroll = 20)"
    );
    assert_eq!(live.view.focused_panel, "subagents", "AC-C3: focused_panel");
    assert!(live.view.show_thought, "AC-C3: show_thought");
    assert!(live.view.show_subagent_panel, "AC-C3: show_subagent_panel");
    assert!(live.view.show_plan_panel, "AC-C3: show_plan_panel");
    assert!(!live.view.chat_auto_scroll, "AC-C3: chat_auto_scroll");
    assert!(live.view.subagent_autoscroll, "AC-C3: subagent_autoscroll");
    assert!(!live.view.plan_auto_scroll, "AC-C3: plan_auto_scroll");
    assert_eq!(
        live.view.selected_subagent_idx, 2,
        "AC-C3: the selection must still address the same panel"
    );
    assert_eq!(
        live.subagent_panels[live.view.selected_subagent_idx].name, "coder-t014d-unit2",
        "AC-C3: selected_subagent_idx must address the panel the bundle stored"
    );

    // The matching branch needs no warning: nothing moved. The hook is also the
    // place a difference would have been said out loud (the pane-restore hook
    // pushes `[CLI] Restore warning:` lines), so the absence of a scroll sentence
    // is observable in the same witness.
    assert!(
        !live
            .chat_messages
            .iter()
            .any(|line| line.contains("Restore warning") && line.contains("scroll")),
        "AC-C3: a scroll difference was announced where every offset matched: {:?}",
        live.chat_messages
    );

    // The clamp itself is still live — it just had nothing to move here. Proving
    // that keeps this test from passing because the clamp was skipped.
    renderer.plan_max_scroll = 4;
    renderer.clamp_scrolls();
    assert_eq!(
        renderer.plan_scroll, 4,
        "the clamp must still be running in this process (plan_max_scroll = 4)"
    );
}
