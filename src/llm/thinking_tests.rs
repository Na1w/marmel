use super::*;
use crate::types::Message;

fn msg(content: &str) -> Message {
    Message::User {
        content: content.to_string(),
    }
}

fn req() -> ChatRequest {
    ChatRequest {
        model: "m".to_string(),
        messages: vec![msg("hi")],
        temperature: Some(0.7),
        top_p: None,
        frequency_penalty: Some(0.2),
        presence_penalty: None,
        stream: Some(true),
        enable_thinking: Some(true),
        tools: None,
    }
}

/// REQ-LLM-002: interleaved `[thinking]` tags separate the reasoning
/// channel from the main payload, even when tags span delta boundaries.
#[test]
fn test_llm_stream_thinking_demux() {
    let raw = "Hello[thinking]Let me parse this carefully[/thinking] world";
    let mut d = ThinkingDemuxer::new();
    // Feed it one char at a time to exercise incremental tag-boundary handling.
    for ch in raw.chars() {
        d.push(&ch.to_string());
    }
    assert_eq!(
        d.content(),
        "Hello world",
        "tags must be stripped from payload"
    );
    assert_eq!(
        d.thinking(),
        "Let me parse this carefully",
        "thinking content routed to dedicated channel"
    );
    assert!(
        !d.is_in_thinking(),
        "must not be left inside a thinking block"
    );
}

/// REQ-LLM-002: `preserve_thinking = true` keeps the raw tags in the payload.
#[test]
fn test_llm_thinking_preserve() {
    let mut d = ThinkingDemuxer::with_preserve(true);
    d.demux_all("a[thinking]b[/thinking]c");
    assert_eq!(
        d.content(),
        "a[thinking]b[/thinking]c",
        "tags preserved in payload when configured"
    );
    assert_eq!(d.thinking(), "b");
}

/// REQ-LLM-002: into_message carries reasoning in a separate field.
#[test]
fn test_llm_into_message_strips_thinking() {
    let mut d = ThinkingDemuxer::new();
    d.demux_all("vis[thinking]reason[/thinking]ible");
    let m = d.into_message();
    match m {
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
        } => {
            assert_eq!(content.as_deref(), Some("visible"));
            assert_eq!(reasoning_content.as_deref(), Some("reason"));
            assert!(tool_calls.is_empty());
        }
        _ => panic!("expected assistant message"),
    }
}

/// REQ-LLM-003: a recovery request disables thinking and shifts
/// frequency_penalty (+0.5) and temperature (+0.1) for one turn.
#[test]
fn test_llm_recovery_suppresses_thinking() {
    let base = req();
    let recovered = apply_recovery(&base, RecoveryAdjustment::default());

    assert_eq!(recovered.enable_thinking, Some(false));
    assert_eq!(
        recovered.frequency_penalty,
        Some(0.2 + 0.5),
        "frequency_penalty += 0.5"
    );
    assert_eq!(recovered.temperature, Some(0.7 + 0.1), "temperature += 0.1");

    // Original request must be untouched (one-turn semantics).
    assert_eq!(base.enable_thinking, Some(true));
    assert_eq!(base.frequency_penalty, Some(0.2));
    assert_eq!(base.temperature, Some(0.7));
}

/// REQ-LLM-003: recovery works when penalties/temperature are None.
#[test]
fn test_llm_recovery_defaults_when_absent() {
    let mut base = req();
    base.frequency_penalty = None;
    base.temperature = None;
    let recovered = apply_recovery(&base, RecoveryAdjustment::default());
    assert_eq!(recovered.frequency_penalty, Some(0.5));
    assert_eq!(recovered.temperature, Some(0.8)); // 0.7 default + 0.1
}

/// REQ-LLM-004: nudge policy allows up to 3 empty-production attempts.
#[test]
fn test_llm_empty_production_nudges() {
    let policy = NudgePolicy::default();
    assert!(policy.should_nudge(0), "attempt 1 of 3 allowed");
    assert!(policy.should_nudge(1), "attempt 2 of 3 allowed");
    assert!(policy.should_nudge(2), "attempt 3 of 3 allowed");
    assert!(!policy.should_nudge(3), "3 used -> terminal error");

    let messages = vec![msg("hi")];
    let nudged = policy.nudge(messages.clone());
    assert_eq!(nudged.len(), 2, "a user nudge is appended");
    match &nudged[1] {
        Message::User { content } => assert_eq!(content, "?"),
        _ => panic!("nudge must be a user message"),
    }
}

/// REQ-LLM-002: multi-byte UTF-8 characters and emojis (e.g. 🔍, 🚀)
/// must not panic on char boundary slicing during partial tag detection.
#[test]
fn test_llm_stream_thinking_demux_multibyte_utf8() {
    let raw = "1. **Analyze**: analyze this code. 🔍 [thinking]reasoning steps 🚀[/thinking] done!";
    let mut d = ThinkingDemuxer::new();
    for ch in raw.chars() {
        d.push(&ch.to_string());
    }
    assert_eq!(d.content(), "1. **Analyze**: analyze this code. 🔍  done!");
    assert_eq!(d.thinking(), "reasoning steps 🚀");
}

#[test]
fn test_partial_prefix_split_multibyte() {
    // String ending with multi-byte emoji followed by partial tag prefix.
    let buf = "Hello 🔍[thin";
    assert_eq!(
        partial_prefix_split(buf, "[thinking]"),
        Some("Hello 🔍".len())
    );

    // String ending directly with multi-byte emoji (no tag prefix).
    let buf_emoji = "Hello 🔍";
    assert_eq!(partial_prefix_split(buf_emoji, "[thinking]"), None);
}

#[test]
fn test_llm_stream_think_tags_demux() {
    let raw = "<think>\nI should report this honestly.\n</think>\nVisible message to user";
    let mut d = ThinkingDemuxer::new();
    for ch in raw.chars() {
        d.push(&ch.to_string());
    }
    assert_eq!(d.content(), "\nVisible message to user");
    assert_eq!(d.thinking(), "\nI should report this honestly.\n");
}

#[test]
fn test_llm_stream_thinking_incremental_emission() {
    let mut d = ThinkingDemuxer::new();
    let mut emitted_content = String::new();
    let mut emitted_thinking = String::new();

    let chunks = vec![
        "Hello, ",
        "[thin",
        "king]Let me ",
        "reason[/thin",
        "king] world!",
    ];

    for chunk in chunks {
        d.push_delta(chunk, |kind, text| match kind {
            DeltaKind::Content => emitted_content.push_str(text),
            DeltaKind::Thinking => emitted_thinking.push_str(text),
        });
    }
    d.finish_delta(|kind, text| match kind {
        DeltaKind::Content => emitted_content.push_str(text),
        DeltaKind::Thinking => emitted_thinking.push_str(text),
    });

    assert_eq!(emitted_content, "Hello,  world!");
    assert_eq!(emitted_thinking, "Let me reason");
    assert_eq!(d.content(), "Hello,  world!");
    assert_eq!(d.thinking(), "Let me reason");
}
