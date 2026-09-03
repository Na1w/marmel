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
