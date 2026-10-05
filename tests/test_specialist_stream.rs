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

    // Assert that the specialist's thinking tokens were delivered to the UI event bus tagged for coder-t-001
    let has_thinking = events.iter().any(|ev| {
        matches!(ev, marmennill::ui::Event::SubagentThinking { agent_tag, text } if agent_tag == "coder-t-001" && text.contains("Analyzing codebase structure."))
    });
    assert!(
        has_thinking,
        "Expected Event::SubagentThinking for coder-t-001 on UI bus, but got events: {:?}",
        events
    );

    // Assert that the specialist's content tokens were delivered to the UI event bus tagged for coder-t-001
    let has_message = events.iter().any(|ev| {
        matches!(ev, marmennill::ui::Event::SubagentMessage { agent_tag, text } if agent_tag == "coder-t-001" && text.contains("Implementing file write now."))
    });
    assert!(
        has_message,
        "Expected Event::SubagentMessage for coder-t-001 on UI bus, but got events: {:?}",
        events
    );

    // Assert that status updates were emitted for the specialist lifecycle
    let has_status = statuses.iter().any(|st| st.contains("coder-t-001"));
    assert!(
        has_status,
        "Expected coder-t-001 status updates on status bus, but got: {:?}",
        statuses
    );
}

#[tokio::test]
async fn test_specialist_consecutive_thinking_nudges_triggers_replan_required() {
    let _guard = TEST_BUS_MUTEX.lock().await;

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
}

#[tokio::test]
async fn test_specialist_thinking_nudges_reset_when_thinking_within_budget() {
    let _guard = TEST_BUS_MUTEX.lock().await;

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
    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 8);
}

#[tokio::test]
async fn test_specialist_truncated_tool_call_feedback_and_recovery() {
    let _guard = TEST_BUS_MUTEX.lock().await;

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
}

#[tokio::test]
async fn test_specialist_repeated_malformed_tool_calls_triggers_replan() {
    let _guard = TEST_BUS_MUTEX.lock().await;

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
}

#[tokio::test]
async fn test_specialist_malformed_tool_calls_reset_on_successful_tool_call() {
    let _guard = TEST_BUS_MUTEX.lock().await;

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
    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 8);
}
