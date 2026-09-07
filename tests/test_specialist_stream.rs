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
        matches!(ev1, Some(marmennill::ui::Event::Thinking(ref t)) if t == "thinking chunk"),
        "expected thinking chunk on UI bus, got: {ev1:?}"
    );

    let ev2 = event_rx.try_recv().ok();
    assert!(
        matches!(ev2, Some(marmennill::ui::Event::Message(ref c)) if c == "content chunk"),
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
    let mut cfg = marmennill::config::Config::default();
    cfg.backend_url = format!("{}/v1", server.uri());
    cfg.model = "test-model".to_string();
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

    // Assert that the specialist's thinking tokens were delivered to the UI event bus
    let has_thinking = events.iter().any(|ev| {
        matches!(ev, marmennill::ui::Event::Thinking(text) if text.contains("Analyzing codebase structure."))
    });
    assert!(
        has_thinking,
        "Expected Event::Thinking on UI bus from specialist stream, but got events: {:?}",
        events
    );

    // Assert that the specialist's content tokens were delivered to the UI event bus
    let has_message = events.iter().any(|ev| {
        matches!(ev, marmennill::ui::Event::Message(text) if text.contains("Implementing file write now."))
    });
    assert!(
        has_message,
        "Expected Event::Message on UI bus from specialist stream, but got events: {:?}",
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
