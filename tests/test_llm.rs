//! Phase D integration tests (direct streaming, thinking demuxer, request construction).

use marmennill::llm::ThinkingDemuxer;
use marmennill::llm::thinking::DeltaKind;
use marmennill::types::{ChatRequest, Message, ToolDef};

#[test]
fn test_integration_thinking_demuxer_streaming_chunks() {
    let mut demuxer = ThinkingDemuxer::new();
    let mut thinking_out = Vec::new();
    let mut content_out = Vec::new();

    let chunks = vec![
        "[thinking]\nLet me ",
        "analyze the bug in module A.\n",
        "[/thinking]\nHere is the final fix.",
    ];

    for chunk in chunks {
        demuxer.push_delta(chunk, |kind, text| match kind {
            DeltaKind::Thinking => thinking_out.push(text.to_string()),
            DeltaKind::Content => content_out.push(text.to_string()),
        });
    }

    let thinking_full = thinking_out.concat();
    let content_full = content_out.concat();

    assert!(thinking_full.contains("analyze the bug in module A"));
    assert!(content_full.contains("Here is the final fix"));
}

#[test]
fn test_integration_chat_request_payload_construction() {
    let req = ChatRequest {
        model: "llama-3-70b".to_string(),
        messages: vec![
            Message::System {
                content: "System prompt".to_string(),
            },
            Message::User {
                content: "Hello".to_string(),
            },
        ],
        tools: Some(vec![ToolDef::read_file(), ToolDef::write_file()]),
        stream: Some(true),
        enable_thinking: None,
        temperature: Some(0.7),
        top_p: Some(0.95),
        presence_penalty: Some(0.0),
        frequency_penalty: Some(0.0),
    };

    let json_str = serde_json::to_string(&req).unwrap();
    assert!(json_str.contains("llama-3-70b"));
    assert!(json_str.contains("read_file"));
    assert!(json_str.contains("write_file"));
}
