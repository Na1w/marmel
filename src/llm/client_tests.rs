use super::*;
use crate::net::{BACKOFF_BASE_MS, MAX_ATTEMPTS, Retryable};
use crate::types::Message;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::MockServer;

fn sse_body(text: &str) -> String {
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

fn request() -> ChatRequest {
    ChatRequest {
        model: String::new(),
        messages: vec![Message::User {
            content: "hi".to_string(),
        }],
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
        stream: None,
        enable_thinking: None,
        tools: None,
    }
}

/// Pin the numbers of the single-owner retry policy as seen by the LLM client,
/// so an accidental policy change (attempt count or backoff base) is caught
/// here as well as in `src/net/retry.rs`.
#[test]
fn test_llm_uses_shared_policy_values() {
    assert_eq!(
        MAX_ATTEMPTS, 3,
        "the LLM client must get exactly the shared attempt count"
    );
    assert_eq!(
        BACKOFF_BASE_MS, 1000,
        "the LLM client must get exactly the shared linear backoff base"
    );
}

/// Guard for the shared retry path used by `ChatClient::chat_stream`:
/// * exactly `crate::net::MAX_ATTEMPTS` requests are made (retry attempt count),
/// * the pause between attempts follows the shared linear schedule
///   `BACKOFF_BASE_MS × attempt` (growing, one sleep per retry),
/// * and the reply from the first successful attempt is returned.
#[tokio::test]
async fn test_llm_retry_backoff() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let hits: Arc<Mutex<Vec<std::time::Instant>>> = Arc::new(Mutex::new(Vec::new()));

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            let hits = hits.clone();
            move |_req: &Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                hits.lock().unwrap().push(std::time::Instant::now());
                match n {
                    0 => ResponseTemplate::new(503),
                    1 => ResponseTemplate::new(429),
                    _ => ResponseTemplate::new(200).set_body_string(sse_body("ok")),
                }
            }
        })
        .mount(&server)
        .await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(&request()).await.unwrap();

    assert_eq!(reply.content, "ok");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        MAX_ATTEMPTS as usize,
        "one 503, one 429 and one success == exactly MAX_ATTEMPTS attempts"
    );

    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), MAX_ATTEMPTS as usize);
    let first_gap = hits[1] - hits[0];
    let second_gap = hits[2] - hits[1];

    // The LLM client must sleep exactly what the shared policy prescribes:
    // `BACKOFF_BASE_MS × 1` before attempt 2 and `× 2` before attempt 3
    // (10% slack for scheduling jitter).
    assert!(
        first_gap >= Duration::from_millis(BACKOFF_BASE_MS * 9 / 10)
            && first_gap < Duration::from_millis(2 * BACKOFF_BASE_MS),
        "attempt 2 started {first_gap:?} after attempt 1, expected one sleep of {BACKOFF_BASE_MS}ms"
    );
    assert!(
        second_gap >= Duration::from_millis(2 * BACKOFF_BASE_MS * 9 / 10)
            && second_gap < Duration::from_millis(4 * BACKOFF_BASE_MS),
        "attempt 3 started {second_gap:?} after attempt 2, expected one sleep of {}ms",
        2 * BACKOFF_BASE_MS
    );
    assert!(
        second_gap > first_gap,
        "backoff must grow with the attempt number: {first_gap:?} -> {second_gap:?}"
    );
}

/// Exhaustion guard: a persistently retryable status is retried exactly
/// `MAX_ATTEMPTS` times and the last error is surfaced unchanged.
#[tokio::test]
async fn test_llm_retry_exhaustion() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(503)
            }
        })
        .mount(&server)
        .await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let err = client.chat(&request()).await.unwrap_err();
    assert!(err.to_string().contains("503"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        MAX_ATTEMPTS as usize,
        "retryable failures must stop at exactly MAX_ATTEMPTS attempts"
    );
}

/// Non-retryable failures must short-circuit on the shared path: a single
/// request, and the backend status/body surfaced verbatim as the error.
#[tokio::test]
async fn test_llm_non_retryable_error_short_circuits() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(400).set_body_string("messages must not be empty")
            }
        })
        .mount(&server)
        .await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let err = client.chat(&request()).await.unwrap_err();
    let msg = err.to_string();

    assert!(
        msg.contains("400") && msg.contains("messages must not be empty"),
        "status and body must be surfaced unchanged, got: {msg}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a non-retryable status must never be retried"
    );
}

/// Pin the pre-existing retryability classification of the LLM client: the
/// transient statuses/timeouts/transport failures it always retried, and the
/// rest it always surfaced immediately.
#[test]
fn test_chat_error_retryability_matches_shared_policy() {
    for status in [503u16, 429, 502, 504] {
        assert!(
            ChatError::HttpStatus {
                status,
                body: String::new()
            }
            .is_retryable(),
            "HTTP {status} must be retried"
        );
    }
    for status in [400u16, 401, 403, 404, 408, 409, 422, 500, 501, 505] {
        assert!(
            !ChatError::HttpStatus {
                status,
                body: String::new()
            }
            .is_retryable(),
            "HTTP {status} must surface immediately"
        );
    }
    assert!(ChatError::InitialTimeout.is_retryable());
    assert!(ChatError::StallTimeout.is_retryable());
    assert!(ChatError::ReadTimeout.is_retryable());
    assert!(ChatError::Transport("connection refused".into()).is_retryable());
    assert!(ChatError::Stream("malformed sse frame".into()).is_retryable());

    // The single retry owner must never re-send a request whose deltas were
    // already delivered to the caller: that class is not retryable, and it
    // terminates the turn as `Truncated` with the partial reply attached.
    let interrupted = ChatError::Interrupted {
        deltas: 4217,
        bytes: 18302,
        cause: Box::new(ChatError::Stream("connection reset".into())),
        reply: Box::new(StreamedReply::default()),
    };
    assert!(
        !interrupted.is_retryable(),
        "a failure after deltas were delivered must never be retried"
    );
    assert_eq!(interrupted.terminal_outcome(), ReplyOutcome::Truncated);
    assert_eq!(interrupted.terminal_cause(), TerminalCause::Transport);
}

#[tokio::test]
async fn test_llm_prefill_delay_triggers_initial_timeout_and_retries() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    // Stalled prompt pre-fill: delay 1.5s (exceeding initial_timeout_secs of 1s)
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_millis(1500))
                        .set_body_string(sse_body("delayed"))
                } else {
                    // Quick success on retry 3
                    ResponseTemplate::new(200).set_body_string(sse_body("recovered"))
                }
            }
        })
        .mount(&server)
        .await;

    let client =
        ChatClient::new(server.uri(), "test-model".to_string()).with_initial_timeout_secs(1);
    let reply = client.chat(&request()).await.unwrap();

    assert_eq!(reply.content, "recovered");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_llm_on_delta_abort_during_prefill() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(1000))
                .set_body_string(sse_body("never seen")),
        )
        .mount(&server)
        .await;

    let client =
        ChatClient::new(server.uri(), "test-model".to_string()).with_initial_timeout_secs(10);

    let mut check_count = 0;
    let reply = client
        .chat_stream(&request(), |_delta| {
            check_count += 1;
            // Abort immediately on the 2nd polling tick (100ms in)
            check_count < 2
        })
        .await
        .unwrap();

    assert_eq!(reply.content, "");
    assert!(check_count >= 2);
    // An aborted turn is a cancellation, never an empty answer.
    assert_eq!(reply.outcome, ReplyOutcome::Cancelled);
    assert_eq!(reply.cause, TerminalCause::CallerAbort);
    assert!(!reply.is_success());
}

#[test]
fn test_consume_event_does_not_double_count_tokens() {
    let before = get_global_token_counts().1;

    let mut state = ReadState::default();
    let mut progress = StreamProgress::new(|_| true);

    let ev = eventsource_stream::Event {
        event: "message".to_string(),
        data: serde_json::json!({
            "choices": [{
                "delta": { "content": "hello world from streaming chunk" }
            }]
        })
        .to_string(),
        id: String::new(),
        retry: None,
    };

    assert_eq!(
        consume_event(&ev, &mut state, &mut progress).unwrap(),
        ConsumeMark::Continue,
        "a plain content delta is not a terminal marker"
    );
    assert_eq!(state.content, "hello world from streaming chunk");
    assert_eq!(
        progress.deltas(),
        1,
        "the delta must be counted as delivered"
    );

    let after = get_global_token_counts().1;
    assert_eq!(
        before, after,
        "consume_event must not increment token counter on chunks"
    );
}

/// The decoder's terminal marks are not interchangeable: `[DONE]` is a completed
/// stream, a refused hook call is a caller cancellation, and anything else is
/// just another event.
#[test]
fn test_consume_event_marks_done_abort_and_continue() {
    let event = |data: &str| eventsource_stream::Event {
        event: "message".to_string(),
        data: data.to_string(),
        id: String::new(),
        retry: None,
    };

    let mut state = ReadState::default();
    let mut progress = StreamProgress::new(|_| true);
    assert_eq!(
        consume_event(&event("[DONE]"), &mut state, &mut progress).unwrap(),
        ConsumeMark::Done,
        "`[DONE]` is the terminal marker"
    );

    let chunk = serde_json::json!({ "choices": [{ "delta": { "content": "more" } }] }).to_string();
    assert_eq!(
        consume_event(&event(&chunk), &mut state, &mut progress).unwrap(),
        ConsumeMark::Continue
    );

    let mut refusing = StreamProgress::new(|_delta| false);
    assert_eq!(
        consume_event(&event(&chunk), &mut state, &mut refusing).unwrap(),
        ConsumeMark::Aborted,
        "a hook that refuses the delta is a caller-initiated cancellation"
    );
    assert!(refusing.has_emitted_deltas());
}

#[test]
fn test_chat_client_default_and_custom_watchdogs() {
    let client = ChatClient::new("http://localhost:8000/v1", "test-model");
    assert_eq!(client.initial_timeout_secs, 300);
    assert_eq!(client.stall_timeout_secs, 300);

    let customized = client
        .with_stall_timeout_secs(600)
        .with_initial_timeout_secs(120);
    assert_eq!(customized.initial_timeout_secs, 120);
    assert_eq!(customized.stall_timeout_secs, 600);
}

/// One SSE `data:` frame carrying a content delta, terminated like a provider sends it.
fn content_chunk(text: &str) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{ "delta": { "content": text }, "finish_reason": null }]
        })
    )
}

/// One SSE `data:` frame carrying a tool-call delta fragment.
fn tool_call_chunk(call_id: Option<&str>, name: Option<&str>, args: &str) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "function": { "name": name, "arguments": args }
                    }]
                },
                "finish_reason": null
            }]
        })
    )
}

const DONE: &str = "data: [DONE]\n\n";

/// Mount a single-response mock and hand back the server plus the request count.
async fn sse_mock(server: &wiremock::MockServer, body: String) -> Arc<AtomicUsize> {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, Request, ResponseTemplate};

    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_string(body.clone())
            }
        })
        .mount(server)
        .await;
    calls
}

/// (a) The caller pulls the plug mid-stream: the terminal state is `Cancelled`,
/// it is never a success, and the request is *not* re-sent.
#[tokio::test]
async fn test_mid_stream_cancellation_is_cancelled_and_never_retried() {
    let server = MockServer::start().await;
    let body = content_chunk("alpha ") + &content_chunk("beta ") + &(content_chunk("gamma") + DONE);
    let calls = sse_mock(&server, body).await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let mut deltas_seen = 0usize;
    let reply = client
        .chat_stream(&request(), |delta| {
            if !delta.is_empty() {
                deltas_seen += 1;
            }
            // Stands in for the cancellation token / preemption / `cancel_all()`.
            deltas_seen < 2
        })
        .await
        .unwrap();

    assert_eq!(reply.outcome, ReplyOutcome::Cancelled);
    assert_eq!(reply.cause, TerminalCause::CallerAbort);
    assert!(!reply.is_success());
    assert!(reply.deltas >= 1, "the delivered deltas must be recorded");
    assert!(
        !reply.content.is_empty(),
        "the partial output must be preserved, not discarded"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a cancelled stream must never be re-sent"
    );
}

/// (b) The provider body stops without `[DONE]` and without a `finish_reason`:
/// a truncated answer, not an empty-but-successful one.
#[tokio::test]
async fn test_stream_ending_without_done_marker_is_truncated() {
    let server = MockServer::start().await;
    let calls = sse_mock(&server, content_chunk("half an answer")).await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(&request()).await.unwrap();

    assert_eq!(reply.outcome, ReplyOutcome::Truncated);
    assert_eq!(reply.cause, TerminalCause::StreamEnd);
    assert_eq!(reply.finish_reason, None);
    assert!(!reply.is_success());
    assert_eq!(reply.content, "half an answer");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a cut stream must not be re-billed by a second attempt"
    );
}

/// (c) Some providers legitimately answer with empty content plus a terminal
/// `finish_reason`. That stays `Complete` — it is distinguishable from a cut
/// stream precisely by the recorded `finish_reason`.
#[tokio::test]
async fn test_genuine_empty_completion_with_finish_reason_is_complete() {
    let server = MockServer::start().await;
    let body = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{ "delta": {}, "finish_reason": "stop" }]
        })
    ) + DONE;
    let calls = sse_mock(&server, body).await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(&request()).await.unwrap();

    assert_eq!(reply.outcome, ReplyOutcome::Complete);
    assert_eq!(reply.cause, TerminalCause::Done);
    assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
    assert!(reply.is_success());
    assert!(reply.content.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// (d) A tool-call stream cut in the middle of its arguments must terminate as
/// `Truncated` and must NOT be handed over as an executable tool call.
#[tokio::test]
async fn test_truncated_tool_call_arguments_are_not_parsed_as_a_call() {
    let server = MockServer::start().await;
    let body = tool_call_chunk(
        Some("call_0"),
        Some(crate::tool_names::TOOL_READ_FILE),
        "{\"path\":\"a",
    ) + &tool_call_chunk(None, None, ".rs\"}");
    let calls = sse_mock(&server, body).await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(&request()).await.unwrap();

    assert_eq!(reply.outcome, ReplyOutcome::Truncated);
    assert!(!reply.is_success());
    assert!(
        reply.tool_calls.is_empty(),
        "half-assembled arguments must never become a tool call, got {:?}",
        reply.tool_calls
    );
    assert_eq!(
        reply.dropped_tool_calls, 1,
        "the unfinished fragment must be counted, not hidden"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// A complete tool-call stream (with `[DONE]`) still yields the parsed call —
/// the filtering above must not swallow well-formed turns.
#[tokio::test]
async fn test_completed_tool_call_stream_still_parses() {
    let server = MockServer::start().await;
    let body = tool_call_chunk(
        Some("call_0"),
        Some(crate::tool_names::TOOL_READ_FILE),
        "{\"path\":\"a.rs\"}",
    ) + &format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
        })
    ) + DONE;
    let calls = sse_mock(&server, body).await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(&request()).await.unwrap();

    assert_eq!(reply.outcome, ReplyOutcome::Complete);
    assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(reply.dropped_tool_calls, 0);
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(
        reply.tool_calls[0].function.name,
        crate::tool_names::TOOL_READ_FILE
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// (e) A cancellation that lands before a single response byte: an empty
/// `Cancelled` reply — distinguishable from the genuine empty completion above —
/// and no request is repeated.
#[tokio::test]
async fn test_cancellation_before_response_is_cancelled_not_empty_success() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_string(content_chunk("never seen") + DONE)
            }
        })
        .mount(&server)
        .await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client
        .chat_stream(&request(), |_delta| false)
        .await
        .unwrap();

    assert_eq!(reply.outcome, ReplyOutcome::Cancelled);
    assert_eq!(reply.cause, TerminalCause::CallerAbort);
    assert!(!reply.is_success());
    assert_eq!(reply.deltas, 0);
    assert!(reply.content.is_empty());
    assert!(
        calls.load(Ordering::SeqCst) <= 1,
        "a cancelled turn must not be re-attempted"
    );
}

/// A fake stdio MCP server replaying the `initialize` handshake and a
/// `tools/list` answer carrying `tools`.
struct ScriptedMcp {
    _dir: tempfile::TempDir,
    config: crate::mcp::McpServerConfig,
}

fn scripted_mcp(tools: &[&str]) -> ScriptedMcp {
    let dir = tempfile::tempdir().expect("tempdir for the scripted MCP server");
    let path = dir.path().join("responses.jsonl");
    let listed: Vec<serde_json::Value> = tools
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    let frames = [
        serde_json::json!({
            "jsonrpc": "2.0", "id": 0,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "scripted", "version": "0"}
            }
        }),
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": listed}}),
    ];
    let body: String = frames.iter().map(|frame| format!("{frame}\n")).collect();
    std::fs::write(&path, body).expect("write the scripted MCP frames");
    ScriptedMcp {
        _dir: dir,
        config: crate::mcp::McpServerConfig {
            command: Some("/bin/sh".to_string()),
            args: vec![
                "-c".to_string(),
                format!("cat '{}' ; sleep 30", path.display()),
            ],
            ..Default::default()
        },
    }
}

/// This test swaps the process-global MCP registry, so it takes a lock of its own.
static MCP_REGISTRY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The schema list handed to the model must be the policy-filtered MCP set: a
/// name the registration policy refused is never advertised, even though the raw
/// registry still carries it. Advertising through the raw registry would offer the
/// model a name that dispatch then refuses.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_policy_refused_mcp_names_are_never_advertised() {
    let _guard = MCP_REGISTRY_LOCK.lock().await;

    // Registered under the config key `terminal`, its read-file tool composes to
    // the built-in alias `terminal__read_file` — the name the policy refuses.
    let refused = scripted_mcp(&[crate::tool_names::TOOL_READ_FILE]);
    let allowed = scripted_mcp(&["do_thing"]);

    let mut servers = std::collections::HashMap::new();
    servers.insert("terminal".to_string(), refused.config.clone());
    servers.insert("myserver".to_string(), allowed.config.clone());

    let manager = Arc::new(
        crate::mcp::McpManager::boot(&servers)
            .await
            .expect("the scripted MCP servers must boot"),
    );
    crate::harness::set_mcp_manager(Arc::clone(&manager));

    let cfg = crate::llm::stream::StreamConfig {
        mcp_servers: vec!["terminal".to_string(), "myserver".to_string()],
        ..Default::default()
    };
    let req = crate::llm::stream::build_request(&cfg, Vec::new());
    let advertised: Vec<String> = req
        .tools
        .unwrap_or_default()
        .iter()
        .map(|def| def.function.name.clone())
        .collect();
    let raw_registry: Vec<String> = manager
        .tools_for_servers(&cfg.mcp_servers)
        .iter()
        .map(|tool| tool.qualified_name())
        .collect();

    manager.shutdown().await;
    // Leave an empty registry for the rest of the suite.
    crate::harness::set_mcp_manager(Arc::new(crate::mcp::McpManager::new()));

    assert!(
        raw_registry
            .iter()
            .any(|name| name == crate::tool_names::TERMINAL_READ_FILE),
        "the scripted server must really have registered {}, got {raw_registry:?}",
        crate::tool_names::TERMINAL_READ_FILE,
    );
    assert!(
        advertised.iter().any(|name| name == "myserver__do_thing"),
        "the policy-approved tool must be advertised, got {advertised:?}",
    );
    assert!(
        !advertised
            .iter()
            .any(|name| name == crate::tool_names::TERMINAL_READ_FILE),
        "a policy-refused MCP name must never be advertised to the model, got {advertised:?}",
    );
}

/// t-035d: a recovery turn built from out-of-range settings must reach the wire
/// with every sampling parameter inside its documented range. The pre-fix
/// `frequency_penalty: 2.5` / `temperature: 2.1` are exactly what made the
/// provider answer HTTP 400, so the recovery turn could never recover.
#[tokio::test]
async fn test_llm_recovery_request_body_stays_in_documented_ranges() {
    use crate::llm::thinking::{
        MAX_PENALTY, MAX_TEMPERATURE, MAX_TOP_P, MIN_PENALTY, RecoveryAdjustment,
        apply_recovery_report,
    };
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let bodies = bodies.clone();
            move |req: &Request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
                bodies.lock().unwrap().push(body);
                ResponseTemplate::new(200).set_body_string(sse_body("recovered"))
            }
        })
        .mount(&server)
        .await;

    // A configured request whose recovery shift leaves the documented ranges.
    let mut base = request();
    base.temperature = Some(2.0);
    base.frequency_penalty = Some(2.0);
    base.presence_penalty = Some(-3.0);
    base.top_p = Some(1.5);

    let outcome = apply_recovery_report(&base, RecoveryAdjustment::default());
    assert!(
        !outcome.clamps.is_empty(),
        "the fixture must really start out of range, got {:?}",
        outcome.clamps
    );

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(&outcome.request).await.expect("request sent");
    assert!(reply.is_success(), "the mock answers with a terminal frame");

    let sent = {
        let guard = bodies.lock().unwrap();
        guard.first().expect("exactly one request").clone()
    };
    assert_eq!(
        sent["temperature"],
        serde_json::json!(MAX_TEMPERATURE),
        "temperature must be clamped in the body actually sent, got {sent}"
    );
    assert_eq!(
        sent["frequency_penalty"],
        serde_json::json!(MAX_PENALTY),
        "frequency_penalty must be clamped in the body actually sent"
    );
    assert_eq!(
        sent["presence_penalty"],
        serde_json::json!(MIN_PENALTY),
        "presence_penalty must be clamped in the body actually sent"
    );
    assert_eq!(
        sent["top_p"],
        serde_json::json!(MAX_TOP_P),
        "top_p must be clamped in the body actually sent"
    );
    assert_eq!(sent["enable_thinking"], serde_json::json!(false));

    // Nothing out of range is ever serialized, whatever the fixture was.
    for field in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
    ] {
        if let Some(value) = sent.get(field).and_then(|v| v.as_f64()) {
            assert!(
                value.is_finite(),
                "`{field}` is not a finite number: {value}"
            );
            assert!(
                (-2.0..=2.0).contains(&value),
                "`{field}` = {value} is outside every documented sampling range"
            );
        }
    }
}
