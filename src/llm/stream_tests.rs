use super::*;
use crate::llm::client::Terminal;

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
                outcome: ReplyOutcome::Complete,
                cause: TerminalCause::Done,
                ..StreamedReply::default()
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

#[tokio::test]
async fn test_turn_stream_handler_does_not_trigger_repetition_on_thinking() {
    let mut rep = crate::harness::monitor::RepetitionDetector::new(3, 5);
    let mut handler = TurnStreamHandler::for_sink(1000, &mut rep, true);
    let mut sink = VecSink::default();

    // Stream repeating thinking blocks
    let repeating_thought = "<think>\nLet's consider rule 1.\nWait, check condition.\nLet's consider rule 1.\nWait, check condition.\nLet's consider rule 1.\nWait, check condition.\n</think>";
    let continue_streaming = handler.on_chunk_with_sink(repeating_thought, &mut sink);
    assert!(
        continue_streaming,
        "Thinking chunks must never trip repetition detector"
    );
    assert!(!handler.rep_triggered);

    // Stream content
    let continue_content = handler.on_chunk_with_sink("Here is the final answer.", &mut sink);
    assert!(continue_content);
    assert!(!handler.rep_triggered);
}

#[tokio::test]
async fn test_turn_stream_handler_enforces_thinking_budget() {
    let mut rep = crate::harness::monitor::RepetitionDetector::new(3, 5);
    // 256 max thinking tokens (budget is clamped to min 256)
    let mut handler = TurnStreamHandler::for_sink_with_thinking_budget(5000, 256, &mut rep, true);
    let mut sink = VecSink::default();

    // Start thinking block
    assert!(handler.on_chunk_with_sink("<think>\n", &mut sink));
    assert!(!handler.thinking_budget_exceeded);

    // Stream 250 characters (~63 tokens)
    let chunk_short = "a".repeat(250);
    assert!(handler.on_chunk_with_sink(&chunk_short, &mut sink));
    assert!(!handler.thinking_budget_exceeded);

    // Stream 1200 characters (~300 tokens), exceeding 256 token budget
    let chunk_long = "b".repeat(1200);
    let continue_streaming = handler.on_chunk_with_sink(&chunk_long, &mut sink);
    assert!(
        !continue_streaming,
        "Streaming should be cut off when thinking budget is exceeded"
    );
    assert!(handler.thinking_budget_exceeded);
    assert!(!handler.budget_exceeded);
}

#[test]
fn test_stream_config_thinking_budget_defaults() {
    let default_cfg = StreamConfig::default();
    assert_eq!(
        default_cfg.max_thinking_tokens,
        crate::config::DEFAULT_MAX_THINKING_TOKENS
    );

    let mut app_cfg = crate::config::Config::default();
    let stream_cfg = StreamConfig::from_config(&app_cfg);
    assert_eq!(
        stream_cfg.max_thinking_tokens,
        crate::config::DEFAULT_MAX_THINKING_TOKENS
    );

    if let Some(ref mut mon) = app_cfg.monitoring {
        mon.max_thinking_tokens = 4096;
    }
    let stream_cfg2 = StreamConfig::from_config(&app_cfg);
    assert_eq!(stream_cfg2.max_thinking_tokens, 4096);
}

// ---------------------------------------------------------------------------
// Cluster net-C11 guard: the single finalize helper shared by every successful
// exit site of `ChatClient::try_chat_once` (`ReplyAccumulator::finalize`).
// These tests pin content assembly, tool-call order, token accounting and the
// per-terminal-site logging difference.
// ---------------------------------------------------------------------------

use crate::llm::client::ReplyAccumulator;
use crate::llm::get_global_token_counts;
use crate::tool_names::{TOOL_READ_FILE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE};

fn counted_assistant_tokens(
    content: &str,
    reasoning: &str,
    tool_calls: &[crate::types::ToolCall],
) -> usize {
    crate::manager::context::count_assistant_tokens(
        if content.is_empty() {
            None
        } else {
            Some(content)
        },
        if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
        tool_calls,
    )
}

type AcceptAllHook = Box<dyn FnMut(&str) -> bool>;

/// A delivery hook that accepts everything and has seen no deltas yet.
fn progress() -> crate::llm::client::StreamProgress<AcceptAllHook> {
    crate::llm::client::StreamProgress::new(Box::new(|_| true))
}

/// Same accumulated deltas regardless of the terminal reason.
fn accumulated_reply() -> ReplyAccumulator {
    let mut tool_calls = std::collections::BTreeMap::new();
    tool_calls.insert(
        0,
        (
            Some("call_0".to_string()),
            TOOL_READ_FILE.to_string(),
            "{\"path\":\"a.rs\"}".to_string(),
        ),
    );
    ReplyAccumulator {
        content: "Done.".to_string(),
        reasoning: "Checking.".to_string(),
        raw: "Checking.Done.".to_string(),
        tool_calls,
    }
}

#[test]
fn test_finalize_helper_text_only_reply() {
    let before = get_global_token_counts().1;
    let reply = ReplyAccumulator {
        content: "Hello world!".to_string(),
        reasoning: "Let me reason".to_string(),
        raw: "[thinking]Let me reason[/thinking]Hello world!".to_string(),
        tool_calls: Default::default(),
    }
    .finalize(
        "http://backend.test",
        "test-model",
        std::time::Instant::now(),
        Terminal::from_cause(TerminalCause::Done, false),
        &progress(),
    );

    assert_eq!(reply.content, "Hello world!");
    assert_eq!(reply.reasoning, "Let me reason");
    assert_eq!(reply.raw, "[thinking]Let me reason[/thinking]Hello world!");
    assert!(reply.tool_calls.is_empty());

    let expected = counted_assistant_tokens("Hello world!", "Let me reason", &[]);
    assert!(expected > 0);
    assert!(get_global_token_counts().1 >= before + expected);
}

#[test]
fn test_finalize_helper_keeps_tool_call_order_and_ids() {
    let before = get_global_token_counts().1;
    let mut tool_calls = std::collections::BTreeMap::new();
    // Inserted out of index order on purpose: assembly must follow the stream
    // `index` key, never arrival order.
    tool_calls.insert(
        2,
        (
            Some("call_b".to_string()),
            TOOL_WRITE_FILE.to_string(),
            "{\"path\":\"b.rs\"}".to_string(),
        ),
    );
    tool_calls.insert(
        0,
        (
            Some("call_a".to_string()),
            TOOL_READ_FILE.to_string(),
            "{\"path\":\"a.rs\"}".to_string(),
        ),
    );
    // Missing id: finalize must synthesize a `call_<uuid>` id.
    tool_calls.insert(
        1,
        (
            None,
            TOOL_RUN_COMMAND.to_string(),
            "{\"command\":\"ls\"}".to_string(),
        ),
    );

    let reply = ReplyAccumulator {
        content: String::new(),
        reasoning: String::new(),
        raw: String::new(),
        tool_calls,
    }
    .finalize(
        "http://backend.test",
        "test-model",
        std::time::Instant::now(),
        Terminal::from_cause(TerminalCause::Done, true),
        &progress(),
    );

    let names: Vec<&str> = reply
        .tool_calls
        .iter()
        .map(|tc| tc.function.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![TOOL_READ_FILE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE]
    );
    assert_eq!(reply.tool_calls[0].id, "call_a");
    assert_eq!(reply.tool_calls[2].id, "call_b");
    assert_eq!(
        reply.tool_calls[1]
            .id
            .strip_prefix("call_")
            .map(|s| s.len()),
        Some(36)
    );
    assert_eq!(
        reply.tool_calls[0].function.arguments,
        "{\"path\":\"a.rs\"}"
    );
    assert_eq!(
        reply.tool_calls[2].function.arguments,
        "{\"path\":\"b.rs\"}"
    );
    assert!(reply.content.is_empty());
    assert!(reply.reasoning.is_empty());
    assert!(reply.raw.is_empty());

    let expected = counted_assistant_tokens("", "", &reply.tool_calls);
    assert!(expected > 0);
    assert!(get_global_token_counts().1 >= before + expected);
}

#[test]
fn test_finalize_helper_terminal_reasons_agree_on_payload() {
    // A `[DONE]` stop (early terminal site, no completion-summary logging) and a
    // limit/abort cut after the read loop (full-read terminal site, summary
    // logging) must yield identical payloads and identical token accounting.
    let before = get_global_token_counts().1;
    let stopped = accumulated_reply().finalize(
        "http://backend.test",
        "test-model",
        std::time::Instant::now(),
        Terminal::from_cause(TerminalCause::Done, false),
        &progress(),
    );
    let stopped_tokens =
        counted_assistant_tokens(&stopped.content, &stopped.reasoning, &stopped.tool_calls);
    let limited = accumulated_reply().finalize(
        "http://backend.test",
        "test-model",
        std::time::Instant::now(),
        // A cut *after* the read loop still reached a terminal marker here, so
        // it stays a completed reply: only the summary logging differs.
        Terminal::from_cause(TerminalCause::FinishReason, true),
        &progress(),
    );

    assert_eq!(stopped.content, limited.content);
    assert_eq!(stopped.reasoning, limited.reasoning);
    assert_eq!(stopped.raw, limited.raw);
    assert_eq!(stopped.tool_calls.len(), limited.tool_calls.len());
    for (a, b) in stopped.tool_calls.iter().zip(limited.tool_calls.iter()) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.function.name, b.function.name);
        assert_eq!(a.function.arguments, b.function.arguments);
    }

    assert!(stopped_tokens > 0);
    // Both terminal reasons must record output tokens.
    assert!(get_global_token_counts().1 >= before + stopped_tokens * 2);

    // Pre-stream aborts stay outside the finalize helper: an empty reply is
    // never a success, whatever its content looks like.
    let aborted = StreamedReply::default();
    assert!(aborted.content.is_empty());
    assert!(aborted.reasoning.is_empty());
    assert!(aborted.raw.is_empty());
    assert!(aborted.tool_calls.is_empty());
    assert!(!aborted.is_success());
    assert_eq!(aborted.outcome, ReplyOutcome::Truncated);
}
