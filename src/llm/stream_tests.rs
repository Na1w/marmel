use super::*;

#[tokio::test]
async fn test_llm_stream_channel_demuxes_thinking() {
    let messages = vec![Message::User {
        content: "hi".to_string(),
    }];
    let cfg = StreamConfig::default();
    let mut sink = VecSink::default();

    let raw_payload = "[thinking]Let me reason[/thinking]Hello world!";
    let msg = drive_streamed_turn(
        |_req| async move {
            Ok(StreamedReply {
                content: "Hello world!".to_string(),
                reasoning: "Let me reason".to_string(),
                raw: raw_payload.to_string(),
                tool_calls: vec![],
            })
        },
        messages,
        &cfg,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(sink.thinking(), "Let me reason");
    assert_eq!(sink.content(), "Hello world!");

    match msg {
        Message::Assistant {
            content,
            reasoning_content,
            ..
        } => {
            assert_eq!(content.as_deref(), Some("Hello world!"));
            assert_eq!(reasoning_content.as_deref(), Some("Let me reason"));
        }
        _ => panic!("expected assistant message"),
    }
}

#[test]
fn test_repetition_detector_breaks_loop() {
    let mut rep = crate::harness::monitor::RepetitionDetector::new(3, 5);
    rep.push("Let's go.\nI'll do it.\nWait, I'll check rule 1.\nGood.\n");
    rep.push("Let's go.\nI'll do it.\nWait, I'll check rule 2.\nGood.\n");
    assert!(!rep.is_repeating());
    rep.push("Let's go.\nI'll do it.\nWait, I'll check rule 3.\nGood.\n");
    assert!(rep.is_repeating());
}

#[test]
fn test_continuation_request_helpers() {
    let base_req = ChatRequest {
        model: "test-model".to_string(),
        messages: vec![Message::User {
            content: "inspect".to_string(),
        }],
        temperature: Some(0.5),
        top_p: Some(0.9),
        frequency_penalty: None,
        presence_penalty: None,
        stream: Some(true),
        enable_thinking: None,
        tools: None,
    };

    let continuation = build_continuation_request(
        &base_req,
        &base_req.messages,
        "I have analyzed the code. ",
        "checking files",
    );
    assert_eq!(continuation.messages.len(), 2);
    match &continuation.messages[1] {
        Message::Assistant {
            content,
            reasoning_content,
            ..
        } => {
            assert_eq!(content.as_deref(), Some("I have analyzed the code. "));
            assert_eq!(reasoning_content.as_deref(), Some("checking files"));
        }
        _ => panic!("expected assistant message in continuation"),
    }

    let fallback = build_fallback_continuation_request(
        &base_req,
        &base_req.messages,
        "I have analyzed the code. ",
    );
    assert_eq!(fallback.messages.len(), 3);
    assert!(matches!(fallback.messages[1], Message::Assistant { .. }));
    assert!(matches!(fallback.messages[2], Message::User { .. }));
}

struct PausingMockSink {
    events: Vec<StreamEvent>,
    pause_at_count: usize,
    call_count: usize,
    action: PauseAction,
    pause_invoked: bool,
}

#[async_trait::async_trait]
impl StreamSink for PausingMockSink {
    fn emit(&mut self, event: StreamEvent) {
        self.events.push(event);
    }

    fn poll_control(&mut self) -> StreamControl {
        self.call_count += 1;
        if self.call_count == self.pause_at_count {
            StreamControl::Pause {
                user_input: "mid-stream user steer".to_string(),
            }
        } else {
            StreamControl::Continue
        }
    }

    async fn on_pause(&mut self, user_input: &str) -> PauseAction {
        assert_eq!(user_input, "mid-stream user steer");
        self.pause_invoked = true;
        self.action
    }
}

#[tokio::test]
async fn test_turn_stream_handler_detects_pause() {
    let mut rep = crate::harness::monitor::RepetitionDetector::new(3, 5);
    let mut handler = TurnStreamHandler::for_sink(100, &mut rep, false);
    let mut sink = PausingMockSink {
        events: Vec::new(),
        pause_at_count: 2,
        call_count: 0,
        action: PauseAction::Resume,
        pause_invoked: false,
    };

    // First chunk passes
    assert!(handler.on_chunk_with_sink("chunk1", &mut sink));
    assert!(handler.pause_requested.is_none());

    // Second chunk triggers pause in mock sink
    assert!(!handler.on_chunk_with_sink("chunk2", &mut sink));
    assert_eq!(
        handler.take_pause_request().as_deref(),
        Some("mid-stream user steer")
    );
}
