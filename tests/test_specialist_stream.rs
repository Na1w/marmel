//! Integration tests verifying that background specialist and validator streaming
//! outputs properly reach the UI event bus via PreemptibleStreamSink.
//!
//! Regression test for the issue where PreemptibleStreamSink::emit dropped all
//! stream events, preventing subagent thoughts and content from reaching the UI.

use std::sync::LazyLock;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Mutex to serialize tests that register global event and status senders.
static TEST_BUS_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[tokio::test]
async fn test_preemptible_stream_sink_forwards_all_stream_events_to_orchestrator_bus() {
    let _guard = TEST_BUS_MUTEX.lock().await;
    use marmennill::llm::{StreamEvent, StreamSink};

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel();
    marmennill::orchestrator::set_event_sender(event_tx);
    marmennill::orchestrator::set_status_sender(status_tx);

    let mut sink =
        marmennill::orchestrator::PreemptibleStreamSink::register("coder-t-1", "test-model");

    sink.emit(StreamEvent::Thinking("thinking chunk".to_string()));
    sink.emit(StreamEvent::Content("content chunk".to_string()));
    sink.emit(StreamEvent::Status("status note".to_string()));

    let ev1 = event_rx.try_recv().ok();
    assert!(
        matches!(ev1, Some(marmennill::ui::Event::SubagentThinking { ref agent_tag, ref text }) if agent_tag == "coder-t-1" && text == "thinking chunk"),
        "expected thinking chunk on UI bus, got: {ev1:?}"
    );

    let ev2 = event_rx.try_recv().ok();
    assert!(
        matches!(ev2, Some(marmennill::ui::Event::SubagentMessage { ref agent_tag, ref text }) if agent_tag == "coder-t-1" && text == "content chunk"),
        "expected content chunk on UI bus, got: {ev2:?}"
    );
    assert_eq!(status_rx.try_recv().ok(), Some("status note".to_string()));
}

#[tokio::test]
async fn test_mock_specialist_live_execution_streams_thinking_and_content_to_ui() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    // Hermeticity: scope this run to an isolated temporary workspace root. Without
    // this scoping the suite resolves `.marmel/prompts/` from the repository
    // working directory and would read (and assert against) the real per-task
    // prompt/verdict files of the checkout.
    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    marmennill::harness::with_workspace_root(tmp.path().to_path_buf(), async move {

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|req: &Request| {
            let body_str = String::from_utf8_lossy(&req.body);
            if body_str.contains("leave_verdict") || body_str.contains("Specialist Deliverable") {
                // Automated validator pass: return approval verdict tool call
                let val_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"val_1\",\"type\":\"function\",\"function\":{\"name\":\"leave_verdict\",\"arguments\":\"{\\\"verdict\\\":\\\"APPROVED\\\",\\\"comments\\\":\\\"Code verified successfully\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(val_sse)
            } else {
                // Specialist turn: stream thinking and assistant content
                let spec_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"<think>Analyzing codebase structure.</think>Implementing file write now.\\n\\nMISSION COMPLETE (t-001)\"}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(spec_sse)
            }
        })
        .mount(&server)
        .await;

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<marmennill::ui::Event>();
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    marmennill::orchestrator::set_event_sender(event_tx);
    marmennill::orchestrator::set_status_sender(status_tx);

    let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
    let req = marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Coder,
        prompt: "Create hello.txt file".to_string(),
        snippets: vec![],
        task_id: Some("t-001".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let ctx = marmennill::agents::IsolatedContext::from_request(
        "You are the Coder specialist.".to_string(),
        &req,
    );
    let cfg = marmennill::config::Config {
        backend_url: format!("{}/v1", server.uri()),
        model: "test-model".to_string(),
        ..Default::default()
    };
    let token = tokio_util::sync::CancellationToken::new();

    let deliverable = marmennill::agents::run_specialist_live(
        &client,
        marmennill::agents::Agent::Coder,
        &ctx,
        &cfg,
        &token,
    )
    .await
    .expect("specialist live execution should succeed");

    assert!(
        deliverable.contains("MISSION COMPLETE (t-001)"),
        "deliverable should indicate mission complete, got: {deliverable}"
    );

    let mut events = Vec::new();
    while let Ok(ev) = event_rx.try_recv() {
        events.push(ev);
    }
    let mut statuses = Vec::new();
    while let Ok(st) = status_rx.try_recv() {
        statuses.push(st);
    }

    let has_thinking = events.iter().any(|ev| {
        matches!(ev, marmennill::ui::Event::SubagentThinking { agent_tag, text } if agent_tag == "coder-t-001" && text.contains("Analyzing codebase structure."))
    });
    assert!(
        has_thinking,
        "Expected Event::SubagentThinking for coder-t-001 on UI bus, but got events: {:?}",
        events
    );

    let has_message = events.iter().any(|ev| {
        matches!(ev, marmennill::ui::Event::SubagentMessage { agent_tag, text } if agent_tag == "coder-t-001" && text.contains("Implementing file write now."))
    });
    assert!(
        has_message,
        "Expected Event::SubagentMessage for coder-t-001 on UI bus, but got events: {:?}",
        events
    );

    let has_status = statuses.iter().any(|st| st.contains("coder-t-001"));
    assert!(
        has_status,
        "Expected coder-t-001 status updates on status bus, but got: {:?}",
        statuses
    );
    })
    .await;
}

#[tokio::test]
async fn test_specialist_consecutive_thinking_nudges_triggers_replan_required() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    // Hermeticity: isolated temporary workspace root — see the rationale on the
    // first test of this file.
    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    marmennill::harness::with_workspace_root(tmp.path().to_path_buf(), async move {

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|_req: &Request| {
            // Specialist turns emit thinking well in excess of 256 tokens (1024 chars) without executing tools
            let long_thought = format!(
                "<think>{}</think>",
                "This problem is extraordinarily intricate and requires deep mathematical analysis beyond normal limits. "
                    .repeat(25)
            );
            let spec_sse = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{long_thought}\"}}}}]}}\n\ndata: [DONE]\n\n"
            );
            ResponseTemplate::new(200).set_body_string(spec_sse)
        })
        .mount(&server)
        .await;

    let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
    let req = marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Coder,
        prompt: "Solve ultra-complex task".to_string(),
        snippets: vec![],
        task_id: Some("t-002".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let ctx = marmennill::agents::IsolatedContext::from_request(
        "You are the Coder specialist.".to_string(),
        &req,
    );
    let cfg = marmennill::config::Config {
        backend_url: format!("{}/v1", server.uri()),
        model: "test-model".to_string(),
        max_thinking_tokens: 256,
        ..Default::default()
    };
    let token = tokio_util::sync::CancellationToken::new();

    let deliverable = marmennill::agents::run_specialist_live(
        &client,
        marmennill::agents::Agent::Coder,
        &ctx,
        &cfg,
        &token,
    )
    .await
    .expect("specialist execution returns deliverable string");

    assert!(
        deliverable.contains("REPLAN REQUIRED (t-002): task too complex"),
        "deliverable should indicate REPLAN REQUIRED due to task too complex after 5 consecutive thinking nudges, got: {deliverable}"
    );
    assert!(
        deliverable
            .contains("exceeded single-turn reasoning budget of 256 tokens 5 times consecutively"),
        "deliverable should mention exceeding reasoning budget 5 times consecutively, got: {deliverable}"
    );
    })
    .await;
}

#[tokio::test]
async fn test_specialist_thinking_nudges_reset_when_thinking_within_budget() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    // Hermeticity: isolated temporary workspace root — see the rationale on the
    // first test of this file.
    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    marmennill::harness::with_workspace_root(tmp.path().to_path_buf(), async move {

    let server = MockServer::start().await;
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cc = call_count.clone();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body_str = String::from_utf8_lossy(&req.body);
            if body_str.contains("leave_verdict") || body_str.contains("Specialist Deliverable") {
                let val_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"val_1\",\"type\":\"function\",\"function\":{\"name\":\"leave_verdict\",\"arguments\":\"{\\\"verdict\\\":\\\"APPROVED\\\",\\\"comments\\\":\\\"Code verified\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n";
                return ResponseTemplate::new(200).set_body_string(val_sse);
            }

            if n < 3 {
                // Turns 0, 1, 2: overthinking (3 times in a row)
                let long_thought = format!(
                    "<think>{}</think>",
                    "This problem is extraordinarily intricate and requires deep mathematical analysis beyond normal limits. "
                        .repeat(25)
                );
                let spec_sse = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{long_thought}\"}}}}]}}\n\ndata: [DONE]\n\n"
                );
                ResponseTemplate::new(200).set_body_string(spec_sse)
            } else if n == 3 {
                // Turn 3: Thinks within budget, and executes a tool call
                let spec_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"<think>Brief thought.</think>\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"glob\",\"arguments\":\"{\\\"pattern\\\":\\\"*\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(spec_sse)
            } else if n < 7 {
                // Turns 4, 5, 6: overthinking again (3 times in a row).
                // If counter was not reset, 3 + 3 = 6 would have triggered REPLAN REQUIRED at 5!
                let long_thought = format!(
                    "<think>{}</think>",
                    "This problem is extraordinarily intricate and requires deep mathematical analysis beyond normal limits. "
                        .repeat(25)
                );
                let spec_sse = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{long_thought}\"}}}}]}}\n\ndata: [DONE]\n\n"
                );
                ResponseTemplate::new(200).set_body_string(spec_sse)
            } else {
                // Turn 7: completes mission
                let spec_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"<think>Done.</think>MISSION COMPLETE (t-003)\"}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(spec_sse)
            }
        })
        .mount(&server)
        .await;

    let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
    let req = marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Coder,
        prompt: "Multi-turn task with thinking reset".to_string(),
        snippets: vec![],
        task_id: Some("t-003".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let ctx = marmennill::agents::IsolatedContext::from_request(
        "You are the Coder specialist.".to_string(),
        &req,
    );
    let cfg = marmennill::config::Config {
        backend_url: format!("{}/v1", server.uri()),
        model: "test-model".to_string(),
        max_thinking_tokens: 256,
        ..Default::default()
    };
    let token = tokio_util::sync::CancellationToken::new();

    let deliverable = marmennill::agents::run_specialist_live(
        &client,
        marmennill::agents::Agent::Coder,
        &ctx,
        &cfg,
        &token,
    )
    .await
    .expect("specialist execution should complete without premature replan");

    assert!(
        deliverable.contains("MISSION COMPLETE (t-003)"),
        "deliverable should complete successfully, got: {deliverable}"
    );
    // 8 specialist turns + 1 automated validator pass. The validator pass only
    // exists because the run is hermetic: in a scoped root the verdict-file
    // requirement is satisfiable, so validation actually runs.
    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 9);
    })
    .await;
}

#[tokio::test]
async fn test_specialist_truncated_tool_call_feedback_and_recovery() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    // Hermeticity: isolated temporary workspace root — see the rationale on the
    // first test of this file.
    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    marmennill::harness::with_workspace_root(tmp.path().to_path_buf(), async move {

    let server = MockServer::start().await;
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cc = call_count.clone();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body_str = String::from_utf8_lossy(&req.body);

            // In all requests from Marmel, body must be completely valid JSON!
            let _parsed: serde_json::Value =
                serde_json::from_str(&body_str).expect("every request body sent by Marmel must be valid JSON");

            if body_str.contains("leave_verdict") || body_str.contains("Specialist Deliverable") {
                let val_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"val_1\",\"type\":\"function\",\"function\":{\"name\":\"leave_verdict\",\"arguments\":\"{\\\"verdict\\\":\\\"APPROVED\\\",\\\"comments\\\":\\\"Code verified\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n";
                return ResponseTemplate::new(200).set_body_string(val_sse);
            }

            if n == 0 {
                // Turn 0: Model streams a truncated tool call (unterminated JSON string)
                let truncated_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_bad\",\"type\":\"function\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{\\\"command\\\":\\\"cd /tmp && cat\"}}]}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(truncated_sse)
            } else if n == 1 {
                // Turn 1: Model should have received feedback about the malformed/truncated tool call!
                assert!(
                    body_str.contains("Invalid or truncated arguments for tool 'run_command'"),
                    "request should contain feedback about truncated tool call, got: {body_str}"
                );
                // Model successfully executes a valid tool call
                let ok_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_ok\",\"type\":\"function\",\"function\":{\"name\":\"glob\",\"arguments\":\"{\\\"pattern\\\":\\\"*\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(ok_sse)
            } else {
                // Turn 2: Concludes with MISSION COMPLETE
                let complete_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"MISSION COMPLETE (t-004)\"}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(complete_sse)
            }
        })
        .mount(&server)
        .await;

    let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
    let req = marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Coder,
        prompt: "Fix truncated tool call".to_string(),
        snippets: vec![],
        task_id: Some("t-004".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let ctx = marmennill::agents::IsolatedContext::from_request(
        "You are the Coder specialist.".to_string(),
        &req,
    );
    let cfg = marmennill::config::Config {
        backend_url: format!("{}/v1", server.uri()),
        model: "test-model".to_string(),
        ..Default::default()
    };
    let token = tokio_util::sync::CancellationToken::new();

    let deliverable = marmennill::agents::run_specialist_live(
        &client,
        marmennill::agents::Agent::Coder,
        &ctx,
        &cfg,
        &token,
    )
    .await
    .expect("specialist execution should succeed after recovering from truncated tool call");

    assert!(
        deliverable.contains("MISSION COMPLETE (t-004)"),
        "deliverable should complete successfully, got: {deliverable}"
    );
    })
    .await;
}

#[tokio::test]
async fn test_specialist_repeated_malformed_tool_calls_triggers_replan() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    // Hermeticity: isolated temporary workspace root — see the rationale on the
    // first test of this file.
    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    marmennill::harness::with_workspace_root(tmp.path().to_path_buf(), async move {

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|req: &Request| {
            let body_str = String::from_utf8_lossy(&req.body);
            // Every request body sent by Marmel must be valid JSON!
            let _parsed: serde_json::Value =
                serde_json::from_str(&body_str).expect("every request body sent by Marmel must be valid JSON");

            // Always stream a truncated tool call
            let truncated_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_bad\",\"type\":\"function\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{\\\"command\\\":\\\"cd /tmp && cat\"}}]}}]}\n\ndata: [DONE]\n\n";
            ResponseTemplate::new(200).set_body_string(truncated_sse)
        })
        .mount(&server)
        .await;

    let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
    let req = marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Coder,
        prompt: "Endless truncated tool call".to_string(),
        snippets: vec![],
        task_id: Some("t-005".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let ctx = marmennill::agents::IsolatedContext::from_request(
        "You are the Coder specialist.".to_string(),
        &req,
    );
    let cfg = marmennill::config::Config {
        backend_url: format!("{}/v1", server.uri()),
        model: "test-model".to_string(),
        ..Default::default()
    };
    let token = tokio_util::sync::CancellationToken::new();

    let deliverable = marmennill::agents::run_specialist_live(
        &client,
        marmennill::agents::Agent::Coder,
        &ctx,
        &cfg,
        &token,
    )
    .await
    .expect("specialist execution returns deliverable string");

    assert!(
        deliverable.contains("REPLAN REQUIRED (t-005)"),
        "deliverable should indicate REPLAN REQUIRED due to repeated malformed tool calls, got: {deliverable}"
    );
    assert!(
        deliverable
            .contains("repeatedly produced truncated or invalid tool calls 5 times consecutively"),
        "deliverable should mention 5 times consecutively, got: {deliverable}"
    );
    })
    .await;
}

#[tokio::test]
async fn test_specialist_malformed_tool_calls_reset_on_successful_tool_call() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    // Hermeticity: isolated temporary workspace root — see the rationale on the
    // first test of this file.
    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    marmennill::harness::with_workspace_root(tmp.path().to_path_buf(), async move {

    let server = MockServer::start().await;
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cc = call_count.clone();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body_str = String::from_utf8_lossy(&req.body);

            // In all requests from Marmel, body must be completely valid JSON!
            let _parsed: serde_json::Value =
                serde_json::from_str(&body_str).expect("every request body sent by Marmel must be valid JSON");

            if body_str.contains("leave_verdict") || body_str.contains("Specialist Deliverable") {
                let val_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"val_1\",\"type\":\"function\",\"function\":{\"name\":\"leave_verdict\",\"arguments\":\"{\\\"verdict\\\":\\\"APPROVED\\\",\\\"comments\\\":\\\"Code verified\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n";
                return ResponseTemplate::new(200).set_body_string(val_sse);
            }

            if n < 3 {
                // Turns 0, 1, 2: 3 malformed tool calls in a row
                let truncated_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_bad\",\"type\":\"function\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{\\\"command\\\":\\\"cat <<EOF\"}}]}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(truncated_sse)
            } else if n == 3 {
                // Turn 3: Successful tool call! Counter must reset to 0!
                let ok_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_ok\",\"type\":\"function\",\"function\":{\"name\":\"glob\",\"arguments\":\"{\\\"pattern\\\":\\\"*\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(ok_sse)
            } else if n < 7 {
                // Turns 4, 5, 6: 3 more malformed tool calls in a row.
                // If counter did not reset, 3 + 3 = 6 would have triggered REPLAN REQUIRED at 5!
                let truncated_sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_bad\",\"type\":\"function\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{\\\"command\\\":\\\"cat <<EOF\"}}]}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(truncated_sse)
            } else {
                // Turn 7: Concludes successfully with MISSION COMPLETE
                let complete_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"MISSION COMPLETE (t-006)\"}}]}\n\ndata: [DONE]\n\n";
                ResponseTemplate::new(200).set_body_string(complete_sse)
            }
        })
        .mount(&server)
        .await;

    let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
    let req = marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Coder,
        prompt: "Reset test for malformed calls".to_string(),
        snippets: vec![],
        task_id: Some("t-006".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let ctx = marmennill::agents::IsolatedContext::from_request(
        "You are the Coder specialist.".to_string(),
        &req,
    );
    let cfg = marmennill::config::Config {
        backend_url: format!("{}/v1", server.uri()),
        model: "test-model".to_string(),
        ..Default::default()
    };
    let token = tokio_util::sync::CancellationToken::new();

    let deliverable = marmennill::agents::run_specialist_live(
        &client,
        marmennill::agents::Agent::Coder,
        &ctx,
        &cfg,
        &token,
    )
    .await
    .expect("specialist execution should complete without premature replan");

    assert!(
        deliverable.contains("MISSION COMPLETE (t-006)"),
        "deliverable should complete successfully, got: {deliverable}"
    );
    assert!(
        !deliverable.contains("REPLAN REQUIRED"),
        "deliverable should NOT trigger REPLAN REQUIRED because counter was reset by successful call, got: {deliverable}"
    );
    // 8 specialist turns + 1 automated validator pass (see the note in
    // `test_specialist_thinking_nudges_reset_when_thinking_within_budget`).
    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 9);
    })
    .await;
}

// Hermetic workspace scoping + unconditional missing-verdict failure

/// Minimal SSE fixture: a single tool call.
fn tool_call_sse(call_id: &str, tool_name: &str, args_json: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-tss",
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": { "name": tool_name, "arguments": args_json }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    )
}

/// Minimal SSE fixture: plain assistant text.
fn text_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-tss",
            "choices": [{ "delta": { "content": text }, "finish_reason": "stop" }]
        })
    )
}

/// `(name, byte length, modification time)` for every entry of `dir`, sorted.
/// Used to prove the repository's own `.marmel/prompts/` is neither rewritten
/// nor replaced by a scoped test run.
fn snapshot_dir(dir: &std::path::Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut out: Vec<(String, u64, std::time::SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if let Ok(meta) = path.metadata() {
                out.push((
                    name,
                    meta.len(),
                    meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                ));
            }
        }
    }
    out.sort();
    out
}

/// A prompts directory that exists but holds **no** `{tid}-validation.md` **and no**
/// `{tid}-validation-skip.md` record, and **no execution plan at all**: the task id
/// is not plan-tracked and nothing designates the task validation-free. Only in that
/// combination is a missing verdict an unconditional hard failure — the validator
/// must not run, the deliverable must be reported as not validated, and no MISSION
/// COMPLETE may be reported. With a skip record on disk the same absence is a
/// deliberate exemption; see
/// [`test_validation_skip_record_is_an_exemption_not_a_verdict_gap`].
#[tokio::test]
async fn test_missing_verdict_file_is_an_unconditional_hard_failure() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".marmel").join("prompts"))
        .expect("prompts dir without any -validation.md");
    assert!(
        !root
            .join(".marmel/prompts/t-901-validation-skip.md")
            .exists(),
        "fixture: no no-validation record may exist for this gap"
    );

    marmennill::harness::with_workspace_root(root.clone(), async move {
        let validator_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let vc = validator_calls.clone();
        let turns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tc = turns.clone();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                if body.contains(marmennill::tool_names::TOOL_LEAVE_VERDICT)
                    || body.contains("Specialist Deliverable")
                {
                    vc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    return ResponseTemplate::new(200).set_body_string(text_sse(
                        "validation must not run without a recorded verdict file",
                    ));
                }
                if tc.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    let args = serde_json::json!({
                        "path": "unconditional-gap.txt",
                        "content": "written by the specialist"
                    })
                    .to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_write_1",
                        marmennill::tool_names::TOOL_WRITE_FILE,
                        &args,
                    ));
                }
                ResponseTemplate::new(200)
                    .set_body_string(text_sse("Work delivered.\n\nMISSION COMPLETE (t-901)"))
            })
            .mount(&server)
            .await;

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "Write a file; verdict file is missing".to_string(),
            snippets: vec![],
            task_id: Some("t-901".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx =
            marmennill::agents::IsolatedContext::from_request("You are the Coder.".to_string(), &req);
        let cfg = marmennill::config::Config {
            backend_url: format!("{}/v1", server.uri()),
            model: "test-model".to_string(),
            ..Default::default()
        };
        let token = tokio_util::sync::CancellationToken::new();

        let deliverable = marmennill::agents::run_specialist_live(&client, marmennill::agents::Agent::Coder, &ctx, &cfg, &token)
            .await
            .expect("run returns a deliverable even when validation hard-failed");

        assert_eq!(
            validator_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no validation pass may run once the verdict gap is recorded, got deliverable: {deliverable}"
        );
        assert!(
            deliverable.contains("Validation was not performed"),
            "deliverable must report that validation was not performed, got: {deliverable}"
        );
        assert!(
            deliverable.contains("t-901-validation.md does not exist"),
            "deliverable must name the missing verdict file, got: {deliverable}"
        );
        assert!(
            deliverable.contains("not validated"),
            "deliverable must be reported as not validated, got: {deliverable}"
        );
        assert!(
            deliverable.contains("FAILED"),
            "deliverable must be a FAILED report, got: {deliverable}"
        );
        assert!(
            !deliverable.contains("MISSION COMPLETE"),
            "a hard verdict gap must never report MISSION COMPLETE, got: {deliverable}"
        );
    })
    .await;
}

/// Counterpart of [`test_missing_verdict_file_is_an_unconditional_hard_failure`]:
/// the same absent `{tid}-validation.md`, but **with** the `{tid}-validation-skip.md`
/// record the plan side writes for a validation-free task, is not a verdict gap.
/// The validator is never contacted (its call counter stays at zero while the
/// specialist consumes exactly its own two turns), the deliverable reports MISSION
/// COMPLETE, the plan line checks off, the status channel carries the
/// `[Validator] SKIPPED by design for <tag>` line, and the worker status becomes
/// `Completed (validation skipped by design)`.
#[tokio::test]
async fn test_validation_skip_record_is_an_exemption_not_a_verdict_gap() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();
    let prompts = root.join(".marmel").join("prompts");
    std::fs::create_dir_all(&prompts).expect("scoped prompts dir");
    assert!(
        !prompts.join("t-902-validation.md").exists(),
        "fixture: a plan-designated validation-free task gets no verdict brief"
    );
    std::fs::write(
        prompts.join("t-902-validation-skip.md"),
        "Planner-designated: no validation required (research/documentation task)\n",
    )
    .expect("no-validation record");

    marmennill::harness::with_workspace_root(root.clone(), async move {
        // One unchecked plan line so the check-off is observable on disk.
        std::fs::write(
            root.join(".marmel").join("execution_plan.md"),
            "# Execution Plan\n\n### Phase 1: Documentation\n- [ ] [t-902] Write the audit note\n",
        )
        .expect("plan file");
        let plan = marmennill::manager::Plan::at(root.join(".marmel"));

        let validator_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let vc = validator_calls.clone();
        let turns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tc = turns.clone();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                if body.contains(marmennill::tool_names::TOOL_LEAVE_VERDICT)
                    || body.contains("Specialist Deliverable")
                {
                    vc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    return ResponseTemplate::new(200).set_body_string(text_sse(
                        "validation must not run for a task holding a skip record",
                    ));
                }
                if tc.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    let args = serde_json::json!({
                        "path": "validation-skip-exemption.txt",
                        "content": "written by the specialist"
                    })
                    .to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_write_1",
                        marmennill::tool_names::TOOL_WRITE_FILE,
                        &args,
                    ));
                }
                ResponseTemplate::new(200)
                    .set_body_string(text_sse("Work delivered.\n\nMISSION COMPLETE (t-902)"))
            })
            .mount(&server)
            .await;

        let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        marmennill::orchestrator::set_status_sender(status_tx);

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "Write a file; the plan designated this task validation-free".to_string(),
            snippets: vec![],
            task_id: Some("t-902".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx =
            marmennill::agents::IsolatedContext::from_request("You are the Coder.".to_string(), &req);
        let cfg = marmennill::config::Config {
            backend_url: format!("{}/v1", server.uri()),
            model: "test-model".to_string(),
            ..Default::default()
        };
        let token = tokio_util::sync::CancellationToken::new();

        let deliverable = marmennill::agents::run_specialist_live(
            &client,
            marmennill::agents::Agent::Coder,
            &ctx,
            &cfg,
            &token,
        )
        .await
        .expect("a recorded skip is a normal completion, not an error");

        // Zero validator calls, and only the specialist's own two turns served.
        assert_eq!(
            validator_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the validator may not be engaged for a recorded skip, got deliverable: {deliverable}"
        );
        assert_eq!(
            turns.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "only the specialist's own turns may be served, got deliverable: {deliverable}"
        );
        assert!(
            deliverable.contains("MISSION COMPLETE (t-902)"),
            "a plan-designated validation-free task must complete, got: {deliverable}"
        );
        assert!(!deliverable.contains("FAILED"), "got: {deliverable}");
        assert!(
            !deliverable.contains("Validation was not performed"),
            "the verdict-gap wording must not appear, got: {deliverable}"
        );
        assert!(
            !deliverable.contains("not validated"),
            "the deliverable must not be reported as unvalidated, got: {deliverable}"
        );

        // The plan line checks off like any approved deliverable.
        assert!(
            plan
                .check_plan_on_deliverable(None, Some("t-902"), &deliverable)
                .expect("plan check is not an IO error"),
            "a skipped-validation deliverable must check its plan line off"
        );
        let plan_text = std::fs::read_to_string(plan.plan_path()).expect("plan file");
        assert!(
            plan_text.contains("- [x] [t-902]"),
            "plan line must be checked off, got: {plan_text}"
        );

        // The exemption is announced once on the status channel, naming the worker
        // tag and the reason read off disk — and the worker label is honest: nothing
        // approved the deliverable, nothing rejected it.
        let mut statuses = Vec::new();
        while let Ok(line) = status_rx.try_recv() {
            statuses.push(line);
        }
        assert!(
            statuses.iter().any(|line| line == "[Validator] SKIPPED by design for coder-t-902: Planner-designated: no validation required (research/documentation task)"),
            "the exemption must be announced verbatim on the status channel, got: {statuses:?}"
        );
        let state = marmennill::orchestrator::workers::worker_state_snapshot("coder-t-902")
            .expect("the worker entry survives its run in the completed ring");
        assert_eq!(
            state.info.status,
            "Completed (validation skipped by design)",
            "the worker must report the skipped-validation completion label"
        );
    })
    .await;
}

/// Proof of scoped **reads**: the verdict file is resolved under the scoped root
/// only. The canary lives in the temporary root, so it can only reach the
/// validator brief if verdict resolution honours the scope instead of the
/// repository's own `.marmel/prompts/`.
#[tokio::test]
async fn test_verdict_file_reads_are_scoped_to_the_isolated_workspace_root() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();
    let prompts = root.join(".marmel").join("prompts");
    std::fs::create_dir_all(&prompts).expect("scoped prompts dir");
    std::fs::write(
        prompts.join("t-902-validation.md"),
        "SCOPED-ROOT-VERDICT-CANARY: verify the deliverable and leave a verdict.",
    )
    .expect("scoped verdict file");

    marmennill::harness::with_workspace_root(root.clone(), async move {
        assert_ne!(
            crate_root_probe(),
            root.canonicalize().unwrap_or(root.clone()),
            "probe helper must not be confused with the scoped root"
        );

        let bodies: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::default();
        let captured = bodies.clone();
        let turns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tc = turns.clone();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                captured.lock().unwrap().push(body.clone());
                if body.contains("Specialist Deliverable") {
                    let args = serde_json::json!({ "verdict": "APPROVED", "comments": "canary run verified" })
                        .to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_verdict_1",
                        marmennill::tool_names::TOOL_LEAVE_VERDICT,
                        &args,
                    ));
                }
                if tc.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    let args = serde_json::json!({ "path": "scoped-read-probe.txt", "content": "x" }).to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_write_1",
                        marmennill::tool_names::TOOL_WRITE_FILE,
                        &args,
                    ));
                }
                ResponseTemplate::new(200)
                    .set_body_string(text_sse("Work delivered.\n\nMISSION COMPLETE (t-902)"))
            })
            .mount(&server)
            .await;

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "Write a file; verdict brief comes from the scoped root".to_string(),
            snippets: vec![],
            task_id: Some("t-902".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx =
            marmennill::agents::IsolatedContext::from_request("You are the Coder.".to_string(), &req);
        let cfg = marmennill::config::Config {
            backend_url: format!("{}/v1", server.uri()),
            model: "test-model".to_string(),
            ..Default::default()
        };
        let token = tokio_util::sync::CancellationToken::new();

        let deliverable = marmennill::agents::run_specialist_live(&client, marmennill::agents::Agent::Coder, &ctx, &cfg, &token)
            .await
            .expect("scoped run with a recorded verdict file must validate");

        let bodies = bodies.lock().unwrap();
        assert!(
            bodies.iter().any(|b| b.contains("SCOPED-ROOT-VERDICT-CANARY")),
            "the validator brief must come from the scoped root's verdict file; bodies: {bodies:?}"
        );
        assert!(
            deliverable.contains("MISSION COMPLETE (t-902)"),
            "with a recorded verdict file the run must be validated and approved, got: {deliverable}"
        );
        assert!(
            root.join("scoped-read-probe.txt").is_file(),
            "tool writes must land inside the scoped root"
        );
    })
    .await;
}

/// The workspace root visible to the running task (i.e. outside any scope).
fn crate_root_probe() -> std::path::PathBuf {
    let cwd = std::env::current_dir().expect("cwd");
    cwd.canonicalize().unwrap_or(cwd)
}

/// Proof of hermeticity: a scoped specialist run sees a different workspace root
/// than the checkout, writes only inside its own root, and leaves the
/// repository's real `.marmel/prompts/` byte-for-byte and mtime-for-mtime
/// untouched.
#[tokio::test]
async fn test_scoped_runs_never_read_or_write_the_repository_marmel_prompts() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    let repo_root = std::env::current_dir()
        .expect("cwd")
        .canonicalize()
        .expect("canonical cwd");
    let repo_prompts = repo_root.join(".marmel").join("prompts");
    let before = snapshot_dir(&repo_prompts);
    assert!(
        !before.is_empty(),
        "precondition: the checkout is expected to own a non-empty .marmel/prompts directory"
    );

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();

    let scoped_root = marmennill::harness::with_workspace_root(root.clone(), async move {
        let seen = marmennill::harness::get_workspace_root();
        let turns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tc = turns.clone();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                if body.contains("Specialist Deliverable") {
                    let args = serde_json::json!({ "verdict": "APPROVED", "comments": "hermetic run verified" })
                        .to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_verdict_1",
                        marmennill::tool_names::TOOL_LEAVE_VERDICT,
                        &args,
                    ));
                }
                if tc.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    let args = serde_json::json!({ "path": "hermetic-write-marker.txt", "content": "x" }).to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_write_1",
                        marmennill::tool_names::TOOL_WRITE_FILE,
                        &args,
                    ));
                }
                ResponseTemplate::new(200)
                    .set_body_string(text_sse("Work delivered.\n\nMISSION COMPLETE (t-903)"))
            })
            .mount(&server)
            .await;

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "Write a file inside the scoped workspace".to_string(),
            snippets: vec![],
            task_id: Some("t-903".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx =
            marmennill::agents::IsolatedContext::from_request("You are the Coder.".to_string(), &req);
        let cfg = marmennill::config::Config {
            backend_url: format!("{}/v1", server.uri()),
            model: "test-model".to_string(),
            ..Default::default()
        };
        let token = tokio_util::sync::CancellationToken::new();

        let deliverable = marmennill::agents::run_specialist_live(&client, marmennill::agents::Agent::Coder, &ctx, &cfg, &token)
            .await
            .expect("hermetic run must succeed");
        assert!(
            deliverable.contains("MISSION COMPLETE (t-903)"),
            "a scoped run must not depend on the repository's prompt files, got: {deliverable}"
        );
        seen
    })
    .await;

    assert_ne!(
        scoped_root, repo_root,
        "the run must have observed the scoped temporary root, not the checkout"
    );
    assert!(
        root.join("hermetic-write-marker.txt").is_file(),
        "the specialist's write must land in the scoped root"
    );
    assert!(
        !repo_root.join("hermetic-write-marker.txt").is_file(),
        "the specialist's write must NOT land in the repository root"
    );
    let after = snapshot_dir(&repo_prompts);
    assert_eq!(
        before, after,
        "the repository's .marmel/prompts/ must be untouched (name/size/mtime snapshot)"
    );
}

/// A **reversed** directive spelling on the plan line — `(researcher) (no-validate)`
/// — is not a directive, so the write side authors the auditor brief exactly as for
/// any other line, and a live run under that plan is validated normally: the
/// validator is engaged once, no `-validation-skip.md` record exists, and the plan
/// line checks off with the mistyped token still on it. A syntax error must be safe,
/// never a silent way to switch validation off.
#[tokio::test]
async fn test_a_reversed_directive_order_still_engages_the_validator() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();
    let prompts = root.join(".marmel").join("prompts");
    std::fs::create_dir_all(&prompts).expect("scoped prompts dir");

    let plan_line = "- [ ] [t-904] Audit the vendor API (researcher) (no-validate)";
    let plan_markdown = format!("# Execution Plan\n\n### Phase 1: Research\n{plan_line}\n");
    std::fs::write(
        root.join(".marmel").join("execution_plan.md"),
        &plan_markdown,
    )
    .expect("plan file");
    let plan = marmennill::manager::Plan::at(root.join(".marmel"));

    marmennill::harness::with_workspace_root(root.clone(), async move {
        // The plan side runs for real, so the record layout below is what the
        // planner path actually leaves for this line.
        marmennill::agents::PromptBuilder::pregenerate_for_plan_offline(
            &plan_markdown,
            &marmennill::agents::Catalog::new(),
            &prompts,
        );
        assert!(
            prompts.join("t-904-validation.md").exists(),
            "an inert `(no-validate)` placement keeps validation on"
        );
        assert!(
            !prompts.join("t-904-validation-skip.md").exists(),
            "and leaves no skip record behind"
        );

        let validator_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let vc = validator_calls.clone();
        let turns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tc = turns.clone();
        let validator_bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let vb = validator_bodies.clone();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                if body.contains(marmennill::tool_names::TOOL_LEAVE_VERDICT)
                    || body.contains("Specialist Deliverable")
                {
                    vc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    vb.lock().expect("capture lock").push(body.clone());
                    let args = serde_json::json!({
                        "verdict": "APPROVED",
                        "comments": "The audit note matches the vendor API."
                    })
                    .to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_verdict_1",
                        marmennill::tool_names::TOOL_LEAVE_VERDICT,
                        &args,
                    ));
                }
                if tc.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    let args = serde_json::json!({
                        "path": "reversed-directive-audit.md",
                        "content": "written by the specialist"
                    })
                    .to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_write_1",
                        marmennill::tool_names::TOOL_WRITE_FILE,
                        &args,
                    ));
                }
                ResponseTemplate::new(200)
                    .set_body_string(text_sse("Findings recorded.\n\nMISSION COMPLETE (t-904)"))
            })
            .mount(&server)
            .await;

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "Audit the vendor API; the plan line mistyped its directive".to_string(),
            snippets: vec![],
            task_id: Some("t-904".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx = marmennill::agents::IsolatedContext::from_request(
            "You are the Coder.".to_string(),
            &req,
        );
        let cfg = marmennill::config::Config {
            backend_url: format!("{}/v1", server.uri()),
            model: "test-model".to_string(),
            ..Default::default()
        };
        let token = tokio_util::sync::CancellationToken::new();

        let deliverable = marmennill::agents::run_specialist_live(
            &client,
            marmennill::agents::Agent::Coder,
            &ctx,
            &cfg,
            &token,
        )
        .await
        .expect("an inert directive is a normally validated run");

        assert_eq!(
            turns.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the specialist serves its own two turns"
        );
        assert_eq!(
            validator_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the mistyped line must still be validated"
        );
        let bodies = validator_bodies.lock().expect("capture lock").clone();
        assert!(
            bodies[0].contains("Independent Quality Verification for t-904"),
            "the validator is driven by the brief the plan side authored"
        );
        assert!(
            deliverable.contains("MISSION COMPLETE (t-904)"),
            "an approved run completes, got: {deliverable}"
        );
        assert!(
            !deliverable.contains("Validation was not performed"),
            "no verdict gap may be reported for an authored brief, got: {deliverable}"
        );

        assert!(
            plan.check_plan_on_deliverable(None, Some("t-904"), &deliverable)
                .expect("plan check is not an IO error"),
            "a validated deliverable checks its plan line off"
        );
        let plan_text = std::fs::read_to_string(plan.plan_path()).expect("plan file");
        assert!(
            plan_text.contains("- [x] [t-904] Audit the vendor API (researcher) (no-validate)"),
            "only the checkbox moves; the mistyped token stays on the line:\n{plan_text}"
        );
    })
    .await;
}

/// A reserved-looking token that sits **inside** the description is prose, so the
/// brief is authored and its wording is handed to the auditor verbatim. The pin is
/// twofold: validation stays on, and the token never reaches the validator as a
/// directive — it reaches it as the description text of the task being audited.
#[tokio::test]
async fn test_a_directive_token_inside_the_description_reaches_the_auditor_as_prose() {
    let _guard = TEST_BUS_MUTEX.lock().await;

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();
    let prompts = root.join(".marmel").join("prompts");
    std::fs::create_dir_all(&prompts).expect("scoped prompts dir");

    let plan_line = "- [ ] [t-905] Ship (no-validate) the parser (coder)";
    let plan_markdown = format!("# Execution Plan\n\n### Phase 1: Implementation\n{plan_line}\n");
    std::fs::write(
        root.join(".marmel").join("execution_plan.md"),
        &plan_markdown,
    )
    .expect("plan file");
    let plan = marmennill::manager::Plan::at(root.join(".marmel"));

    marmennill::harness::with_workspace_root(root.clone(), async move {
        marmennill::agents::PromptBuilder::pregenerate_for_plan_offline(
            &plan_markdown,
            &marmennill::agents::Catalog::new(),
            &prompts,
        );
        assert!(
            prompts.join("t-905-validation.md").exists(),
            "a mid-description token is prose and cannot disable validation"
        );
        assert!(
            !prompts.join("t-905-validation-skip.md").exists(),
            "no skip record may be written from prose wording"
        );

        let validator_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let vc = validator_calls.clone();
        let turns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tc = turns.clone();
        let validator_bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let vb = validator_bodies.clone();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                if body.contains(marmennill::tool_names::TOOL_LEAVE_VERDICT)
                    || body.contains("Specialist Deliverable")
                {
                    vc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    vb.lock().expect("capture lock").push(body.clone());
                    let args = serde_json::json!({
                        "verdict": "APPROVED",
                        "comments": "The parser is shipped behind a flag."
                    })
                    .to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_verdict_1",
                        marmennill::tool_names::TOOL_LEAVE_VERDICT,
                        &args,
                    ));
                }
                if tc.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    let args = serde_json::json!({
                        "path": "mid-description-prose.txt",
                        "content": "written by the specialist"
                    })
                    .to_string();
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call_write_1",
                        marmennill::tool_names::TOOL_WRITE_FILE,
                        &args,
                    ));
                }
                ResponseTemplate::new(200)
                    .set_body_string(text_sse("Work delivered.\n\nMISSION COMPLETE (t-905)"))
            })
            .mount(&server)
            .await;

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "Ship (no-validate) the parser".to_string(),
            snippets: vec![],
            task_id: Some("t-905".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx = marmennill::agents::IsolatedContext::from_request(
            "You are the Coder.".to_string(),
            &req,
        );
        let cfg = marmennill::config::Config {
            backend_url: format!("{}/v1", server.uri()),
            model: "test-model".to_string(),
            ..Default::default()
        };
        let token = tokio_util::sync::CancellationToken::new();

        let deliverable = marmennill::agents::run_specialist_live(
            &client,
            marmennill::agents::Agent::Coder,
            &ctx,
            &cfg,
            &token,
        )
        .await
        .expect("a prose token is not a directive: the run is validated");

        assert_eq!(
            turns.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the specialist serves its own two turns"
        );
        assert_eq!(
            validator_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the prose spelling must leave validation on"
        );
        let bodies = validator_bodies.lock().expect("capture lock").clone();
        assert!(
            bodies[0].contains("Ship (no-validate) the parser"),
            "the description reaches the auditor verbatim, as prose"
        );
        assert!(
            deliverable.contains("MISSION COMPLETE (t-905)"),
            "an approved run completes, got: {deliverable}"
        );

        assert!(
            plan.check_plan_on_deliverable(None, Some("t-905"), &deliverable)
                .expect("plan check is not an IO error"),
            "a validated deliverable checks its plan line off"
        );
        let plan_text = std::fs::read_to_string(plan.plan_path()).expect("plan file");
        assert!(
            plan_text.contains("- [x] [t-905] Ship (no-validate) the parser (coder)"),
            "the plan line keeps its prose token through the check-off:\n{plan_text}"
        );
    })
    .await;
}
