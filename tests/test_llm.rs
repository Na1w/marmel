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

#[test]
fn test_assistant_message_content_never_serializes_as_null() {
    // Both without and with tool calls, content should serialize as "" (empty string)
    // and never as null, to prevent Ollama / OpenAI backends returning HTTP 400:
    // 'invalid message content type: <nil>'
    let msg_no_content = Message::Assistant {
        content: None,
        reasoning_content: None,
        tool_calls: Vec::new(),
    };
    let json_no_content = serde_json::to_string(&msg_no_content).unwrap();
    assert!(
        json_no_content.contains(r#""content":"""#),
        "expected content to serialize as empty string, got: {json_no_content}"
    );
    assert!(
        !json_no_content.contains(r#""content":null"#),
        "content must never serialize as null"
    );

    let msg_with_tool_call = Message::Assistant {
        content: None,
        reasoning_content: None,
        tool_calls: vec![marmennill::types::ToolCall::new("call_1", "test_fn", "{}")],
    };
    let json_with_tool_call = serde_json::to_string(&msg_with_tool_call).unwrap();
    assert!(
        json_with_tool_call.contains(r#""content":"""#),
        "expected content to serialize as empty string, got: {json_with_tool_call}"
    );
    assert!(
        !json_with_tool_call.contains(r#""content":null"#),
        "content must never serialize as null"
    );

    // Also verify deserialization handles both null and empty string gracefully
    let de_null: Message = serde_json::from_str(r#"{"role":"assistant","content":null}"#).unwrap();
    assert_eq!(de_null.content(), None);

    let de_empty: Message = serde_json::from_str(r#"{"role":"assistant","content":""}"#).unwrap();
    assert_eq!(de_empty.content(), Some(""));
}
