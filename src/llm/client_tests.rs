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
        reasoning_effort: None,
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

/// Pin the retryability classification of the LLM client: the transient
/// statuses/timeouts/transport failures it retries, and the rest it surfaces
/// immediately.
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

/// `QueueTimeout` is terminal and never retried: the queue bound for a capped
/// `backend + model` pair already expired, the request was never sent, and going
/// back into the queue would double-spend the wait budget and let this call
/// leapfrog the sessions already in line. The message must name the cap and the
/// key, or the failed delegation is not attributable.
#[test]
fn test_queue_timeout_is_terminal_and_non_retryable() {
    let queued_out = ChatError::QueueTimeout {
        waited_secs: 900,
        cap: 2,
        backend: "http://192.168.50.162:8080/v1".to_string(),
        model: "llama-3".to_string(),
    };
    assert!(
        !queued_out.is_retryable(),
        "an expired queue wait must never be retried"
    );
    assert_eq!(
        queued_out.terminal_outcome(),
        ReplyOutcome::Error,
        "nothing was ever sent, so there is no partial stream to truncate"
    );
    assert_eq!(queued_out.terminal_cause(), TerminalCause::QueueTimeout);
    assert_eq!(
        TerminalCause::QueueTimeout.outcome(),
        ReplyOutcome::Error,
        "a queue timeout is never reported as a cut stream"
    );

    let msg = queued_out.to_string();
    for expected in ["cap 2", "http://192.168.50.162:8080/v1", "llama-3", "900s"] {
        assert!(
            msg.contains(expected),
            "the failure must name {expected:?}, got: {msg}"
        );
    }
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
                    // Stalled first event: delay 1.5s (exceeding initial_timeout_secs of 1s)
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
#[cfg(unix)]
struct ScriptedMcp {
    _dir: tempfile::TempDir,
    config: crate::mcp::McpServerConfig,
}

#[cfg(unix)]
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
            // `sh` resolved through PATH — its absolute location differs between
            // runners (it lives in `/usr/bin` on macOS), and the `cat` plus
            // `sleep` in the script are resolved the same way by the shell.
            command: Some("sh".to_string()),
            args: vec![
                "-c".to_string(),
                format!("cat '{}' ; sleep 30", path.display()),
            ],
            ..Default::default()
        },
    }
}

/// This test swaps the process-global MCP registry, so it takes a lock of its own.
#[cfg(unix)]
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

/// A recovery turn built from out-of-range settings must reach the wire with
/// every sampling parameter inside its documented range: `frequency_penalty: 2.5`
/// / `temperature: 2.1` are exactly what makes the provider answer HTTP 400, so an
/// out-of-range recovery request can never recover.
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

// ---------------------------------------------------------------------------
// Pre-send wire normalization in `ChatClient::try_chat_once`: strict
// OpenAI-compatible chat formats accept `role: "system"` only in the first
// position and answer HTTP 400 for anything later, so every system message
// after index 0 must leave as `user` with byte-identical content.
// ---------------------------------------------------------------------------

/// Like [`sse_mock`], but additionally records each request body as JSON so the
/// assertions can be made on the payload that really went on the wire.
async fn capturing_sse_mock(
    server: &wiremock::MockServer,
    sse: String,
) -> Arc<Mutex<Vec<serde_json::Value>>> {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, Request, ResponseTemplate};

    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let bodies = bodies.clone();
            move |req: &Request| {
                let sent: serde_json::Value =
                    serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
                bodies.lock().unwrap().push(sent);
                ResponseTemplate::new(200).set_body_string(sse.clone())
            }
        })
        .mount(server)
        .await;
    bodies
}

/// The `role` of every message of a captured request body, in wire order.
fn wire_roles(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|msg| msg["role"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The role of every message of an in-memory history: `Message` has no
/// `PartialEq`, so roles are compared through the `role()` accessor.
fn local_roles(messages: &[Message]) -> Vec<&'static str> {
    messages.iter().map(|msg| msg.role()).collect()
}

fn system_msg(text: &str) -> Message {
    Message::System {
        content: text.to_string(),
    }
}

fn user_msg(text: &str) -> Message {
    Message::User {
        content: text.to_string(),
    }
}

/// The rebirth checkpoint spelled as `Message::System`, the shape that reaches
/// the wire from session bundles restored from older builds (`perform_rebirth`
/// emits the checkpoint as `Message::User`); the request-boundary normalization
/// covers both. Built from the real detector constant so the checkpoint bytes the
/// normalization has to preserve are the ones under test.
fn checkpoint_content() -> String {
    format!(
        "{prefix}1. demote mid-history system messages on the wire; 2. cover it with tests)",
        prefix = crate::manager::context::REBIRTH_CHECKPOINT_PREFIX,
    )
}

/// Send `messages` against a recording mock and hand back the body that reached
/// the server (the last attempt's body — every attempt is normalized alike).
async fn sent_body_for(messages: &[Message]) -> serde_json::Value {
    let server = MockServer::start().await;
    let bodies = capturing_sse_mock(&server, sse_body("normalized")).await;

    let mut req = request();
    req.messages = messages.to_vec();

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(&req).await.expect("request sent");
    assert!(reply.is_success(), "the mock answers with a terminal frame");

    let bodies = bodies.lock().unwrap();
    // Normalization happens on every attempt, so the last attempt is the payload
    // to inspect; attempt counts are pinned by the retry tests above.
    assert!(
        !bodies.is_empty(),
        "the request body must have reached the mock server"
    );
    bodies.last().expect("non-empty").clone()
}

/// The regression itself: `[system, user, system]` — the shape a rebirth
/// checkpoint leaves behind — must reach the wire as `["system","user","user"]`,
/// and the demoted message must keep its content byte for byte.
#[tokio::test]
async fn test_llm_mid_history_system_message_is_demoted_to_user_on_the_wire() {
    let checkpoint = checkpoint_content();
    assert!(
        checkpoint.starts_with("(SYSTEM: REBIRTH CHECKPOINT"),
        "the fixture must really carry the checkpoint prefix, got {checkpoint}"
    );
    let history = vec![
        system_msg("pinned system prompt"),
        user_msg("implement the fix"),
        Message::System {
            content: checkpoint.clone(),
        },
    ];

    let sent = sent_body_for(&history).await;
    assert_eq!(
        wire_roles(&sent),
        vec!["system", "user", "user"],
        "only index 0 may keep role `system` on the wire, got {sent}"
    );
    let demoted = &sent["messages"][2];
    assert_eq!(
        demoted["role"].as_str(),
        Some("user"),
        "the checkpoint must be demoted to a user turn, got {demoted}"
    );
    assert_eq!(
        demoted["content"].as_str().unwrap_or_default().as_bytes(),
        checkpoint.as_bytes(),
        "the demoted message must be sent with byte-identical content"
    );
}

/// Index 0 keeps whatever role it has: with a restored history that opens on a
/// user turn, the later system message is still demoted but nothing else moves.
#[tokio::test]
async fn test_llm_first_position_keeps_its_role_when_it_is_not_system() {
    let sent = sent_body_for(&[
        user_msg("a history that opens on a user turn"),
        system_msg("an instruction that arrived mid-history"),
    ])
    .await;

    assert_eq!(
        wire_roles(&sent),
        vec!["user", "user"],
        "index 0 keeps `user` and the later system message is demoted, got {sent}"
    );
    assert_eq!(
        sent["messages"][1]["content"].as_str(),
        Some("an instruction that arrived mid-history"),
        "demotion must not touch content"
    );
}

/// A pure multi-system history keeps exactly one system message: the first.
#[tokio::test]
async fn test_llm_only_the_first_of_consecutive_system_messages_stays_system() {
    let sent = sent_body_for(&[
        system_msg("prompt A"),
        system_msg("prompt B"),
        system_msg("prompt C"),
    ])
    .await;

    assert_eq!(
        wire_roles(&sent),
        vec!["system", "user", "user"],
        "consecutive system messages after index 0 are all demoted, got {sent}"
    );
    assert_eq!(sent["messages"][0]["content"].as_str(), Some("prompt A"));
    assert_eq!(sent["messages"][1]["content"].as_str(), Some("prompt B"));
    assert_eq!(sent["messages"][2]["content"].as_str(), Some("prompt C"));
}

/// The demotion happens on the cloned payload only: the caller's own history
/// keeps its real roles (the rebirth/checkpoint detectors and
/// `pinned_prefix_end` depend on that) and its content.
#[tokio::test]
async fn test_llm_system_demotion_mutates_only_the_cloned_payload_not_the_callers_history() {
    let checkpoint = checkpoint_content();
    let server = MockServer::start().await;
    let bodies = capturing_sse_mock(&server, sse_body("normalized")).await;

    let mut req = request();
    req.messages = vec![
        system_msg("pinned system prompt"),
        user_msg("implement the fix"),
        Message::System {
            content: checkpoint.clone(),
        },
    ];

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(&req).await.expect("request sent");
    assert!(reply.is_success(), "the mock answers with a terminal frame");

    let sent = {
        let guard = bodies.lock().unwrap();
        assert!(
            !guard.is_empty(),
            "the request body must have reached the mock server"
        );
        guard.last().expect("non-empty").clone()
    };
    assert_eq!(
        wire_roles(&sent),
        vec!["system", "user", "user"],
        "the wire payload must be normalized, got {sent}"
    );
    assert_eq!(
        local_roles(&req.messages),
        vec!["system", "user", "system"],
        "the caller's history must never see the demotion"
    );
    assert_eq!(
        req.messages[2].content(),
        Some(checkpoint.as_str()),
        "the in-memory checkpoint content must be untouched"
    );
}

// ---------------------------------------------------------------------------
// The `reasoning_effort` dial on the wire: whatever the caller set on
// `ChatRequest::reasoning_effort` must arrive verbatim in the body that reaches
// `/chat/completions` (the client never strips or rewrites it), and an unset —
// or blank, once it went through `stream::build_request` — dial must be omitted
// from the body entirely (`skip_serializing_if = "Option::is_none"`), never sent
// as `""` or `null`.
// ---------------------------------------------------------------------------

/// Like [`sent_body_for`], but sends a caller-built [`ChatRequest`] instead of the
/// default fixture with a swapped-in history, so the assertions can be made on a
/// request that carries a specific optional dial.
async fn sent_body_for_request(req: &ChatRequest) -> serde_json::Value {
    let server = MockServer::start().await;
    let bodies = capturing_sse_mock(&server, sse_body("effort")).await;

    let client = ChatClient::new(server.uri(), "test-model".to_string());
    let reply = client.chat(req).await.expect("request sent");
    assert!(reply.is_success(), "the mock answers with a terminal frame");

    let bodies = bodies.lock().unwrap();
    assert!(
        !bodies.is_empty(),
        "the request body must have reached the mock server"
    );
    // A 200 answer means a single attempt; `last()` keeps the helper robust in
    // case the fixture ever trips the shared retry policy.
    bodies.last().expect("non-empty").clone()
}

/// A request that asks for `reasoning_effort: Some("low")` must put exactly that
/// key/value pair in the body the backend actually receives — the client clones
/// the request for its pre-send normalization, and that clone must not drop the
/// dial (or leak `null`).
#[tokio::test]
async fn test_llm_reasoning_effort_arrives_verbatim_in_the_request_body() {
    let mut req = request();
    req.reasoning_effort = Some("low".to_string());

    let sent = sent_body_for_request(&req).await;

    assert_eq!(
        sent.get("reasoning_effort"),
        Some(&serde_json::json!("low")),
        "`reasoning_effort: Some(\"low\")` must reach the wire unchanged, got {sent}"
    );
    // Belt and braces on the exact spelling: the backend compares this string.
    assert_eq!(
        sent["reasoning_effort"].as_str(),
        Some("low"),
        "the effort must be serialized as the exact string, no padding or quoting, got {sent}"
    );
    // The rest of the body is untouched by the dial: model fallback and streaming
    // flag still hold, and `tools` is absent in this fixture.
    assert_eq!(sent["model"], serde_json::json!("test-model"));
    assert_eq!(sent["stream"], serde_json::json!(true));
    assert!(
        sent.get("tools").is_none(),
        "the fixture advertises no tools, got {sent}"
    );
}

/// The unset case: with `reasoning_effort: None` the key must be missing from the
/// body entirely, while the neighbouring optional dials still serialize (so the
/// omission is the field's own, not an accidentally empty payload).
#[tokio::test]
async fn test_llm_reasoning_effort_is_omitted_from_the_body_when_unset() {
    let mut req = request();
    req.reasoning_effort = None;
    req.temperature = Some(0.5);
    req.enable_thinking = Some(false);

    let sent = sent_body_for_request(&req).await;

    assert!(
        sent.get("reasoning_effort").is_none(),
        "`reasoning_effort` must never appear when unset — not as `null`, not as `\"\"` — got {sent}"
    );
    assert!(
        sent.get("enable_thinking").is_some() && sent.get("temperature").is_some(),
        "the neighbouring optional fields must still serialize, got {sent}"
    );
    assert_eq!(sent["enable_thinking"], serde_json::json!(false));
}

/// The `StreamConfig` path end to end: `build_request` is the single chokepoint
/// where the Manager/role dial reaches the wire, so a padded value must arrive
/// trimmed (never `"  high "`, which the backend cannot match to a vocabulary).
#[tokio::test]
async fn test_llm_stream_config_reasoning_effort_arrives_trimmed_on_the_wire() {
    use crate::llm::stream::{StreamConfig, build_request};

    let cfg = StreamConfig {
        reasoning_effort: Some("  high ".to_string()),
        ..StreamConfig::default()
    };
    let built = build_request(&cfg, vec![user_msg("think about this")]);
    assert_eq!(
        built.reasoning_effort.as_deref(),
        Some("high"),
        "`build_request` must trim the configured dial before serialization"
    );

    let sent = sent_body_for_request(&built).await;
    assert_eq!(
        sent["reasoning_effort"],
        serde_json::json!("high"),
        "a padded `StreamConfig::reasoning_effort` must arrive trimmed, got {sent}"
    );
}

/// The blank-value guard: a configured but meaningless effort (`""`, spaces, a
/// tab) must be normalized to "unset" by `build_request` and therefore stay off
/// the wire — a backend that validates the field rejects `reasoning_effort: ""`.
#[tokio::test]
async fn test_llm_stream_config_blank_reasoning_effort_never_reaches_the_wire() {
    use crate::llm::stream::{StreamConfig, build_request};

    for blank in ["", "   ", "\t"] {
        let cfg = StreamConfig {
            reasoning_effort: Some(blank.to_string()),
            ..StreamConfig::default()
        };
        let built = build_request(&cfg, vec![user_msg("think about this")]);
        assert!(
            built.reasoning_effort.is_none(),
            "a blank configured effort ({blank:?}) must normalize to `None`"
        );

        let sent = sent_body_for_request(&built).await;
        assert!(
            sent.get("reasoning_effort").is_none(),
            "a blank effort ({blank:?}) must be omitted from the body, got {sent}"
        );
    }
}

/// Design §5 / invariant I7: every client starts on the pool's 20 s
/// internal-reserve clock, and the only legal override is a **shortening** of it.
/// Raising the bound would let a steer or notice call sit waiting on its reserve
/// slot and re-introduce exactly the steering stall the reserve exists to remove,
/// so the ceiling is enforced here rather than left to convention. The reserve
/// clock is also kept separate from the 900 s session-queue bound: an internal
/// fast call never inherits the budget of an ordinary session.
#[test]
fn test_internal_reserve_wait_is_bounded_at_twenty_seconds() {
    use crate::orchestrator::pool;

    assert_eq!(
        pool::INTERNAL_QUEUE_WAIT_SECS,
        20,
        "design §5 fixes the internal-reserve bound at 20 s"
    );

    let client = ChatClient::new("http://reserve-bound:8080/v1", "m");
    assert_eq!(client.reserve_wait(), pool::internal_queue_wait());
    assert_eq!(
        client.reserve_wait(),
        Duration::from_secs(20),
        "the default reserve wait must be the 20 s bound"
    );

    let shorter = ChatClient::new("http://reserve-bound:8080/v1", "m").with_reserve_wait_secs(1);
    assert_eq!(shorter.reserve_wait(), Duration::from_secs(1));

    let raised = ChatClient::new("http://reserve-bound:8080/v1", "m").with_reserve_wait_secs(3600);
    assert_eq!(
        raised.reserve_wait(),
        Duration::from_secs(20),
        "the 20 s ceiling must never be raised by a caller"
    );

    assert_eq!(
        client.queue_wait,
        Duration::from_secs(pool::QUEUE_WAIT_TIMEOUT_SECS),
        "the session queue bound keeps its own number"
    );
    assert!(
        client.reserve_wait() < client.queue_wait,
        "an internal fast call must never inherit the session queue budget"
    );
}

// --- trust-store regression (`bug.md`) -------------------------------------
//
// A host with no TLS trust store makes `reqwest::Client` impossible to build.
// The old fallback (`build_http_client(..).unwrap_or_default()`) re-ran that
// exact build through `Client::default()` → `ClientBuilder::new().build()
// .expect("Client::new()")`, so the process aborted with a reqwest panic and
// exit code 101 before it could do any work. The verdict is now **stored** on
// the client and surfaced as a non-retryable `ChatError::TrustStore`.
//
// These tests use the `with_http_client_failure` seam (`#[cfg(test)] impl
// ChatClient` in `client.rs`): no CI runner is store-less, and env tricks
// cannot make one (`openssl-probe` reads `SSL_CERT_FILE`/`SSL_CERT_DIR` *and*
// still probes the well-known certificate directories), so the verdict has to
// be injected.

/// Slice out one function — signature plus body, brace-counted from the first
/// `{` after `marker` — so a source guard can be scoped to a single function
/// instead of to a whole file. `client.rs` legitimately uses
/// `unwrap_or_default()` elsewhere (attribution strings, body dumps), so a
/// whole-file ban would be a false positive.
fn fn_region(src: &str, marker: &str) -> String {
    let start = src
        .find(marker)
        .unwrap_or_else(|| panic!("client.rs must still define {marker:?}"));
    let body = &src[start..];
    let open = body
        .find('{')
        .unwrap_or_else(|| panic!("{marker:?} must be followed by its body"));
    let mut depth = 0usize;
    for (idx, ch) in body[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return body[..open + idx + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated body for {marker:?}");
}

/// Drop whole-line comments from a scanned region: `client.rs` deliberately
/// *documents* the removed `unwrap_or_default()` / `Client::new()` trap in the
/// doc comment of the `http` field, and prose about the bug must not trip the
/// guard that keeps the bug from coming back.
fn code_only(region: &str) -> String {
    region
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*'))
        })
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Needle scan that respects identifier boundaries, so `Client::new(` finds a
/// reqwest construction but never a `ChatClient::new(` mention (which is legal
/// and everywhere), while punctuation-led needles such as `.expect(` match
/// wherever they occur.
fn mentions(haystack: &str, needle: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let ident_led = needle.chars().next().is_some_and(is_ident);
    haystack.match_indices(needle).any(|(idx, _)| {
        !ident_led || !matches!(haystack[..idx].chars().next_back(), Some(c) if is_ident(c))
    })
}

/// Guard: the shared HTTP-client construction path must contain **no** path
/// that builds a second client when the shared build fails — every one of those
/// re-runs the failing build and aborts the process (`unwrap_or_default()` →
/// `Client::default()` → `.expect("Client::new()")`, `reqwest 0.13.4`). The scan
/// is scoped to the helper, its reason extraction, the startup probe and the
/// four client-owning constructors, and separately asserts the construction
/// still goes through the shared builder exactly once per process.
#[test]
fn test_http_client_construction_path_has_no_panic_on_default_trap() {
    let src = include_str!("client.rs");

    // Markers of the construction path, each unique in the file.
    let construction_fns = [
        "fn shared_http_client()",
        "fn build_failure_reason(",
        "fn http_client_startup_error()",
        "pub fn from_config(cfg:",
        "pub fn new(backend_url: impl Into<String>, model: impl Into<String>) -> Self {",
        "pub fn new_with_token(",
        "pub fn with_client(",
    ];
    // The panic-on-default family: any of these inside the construction path
    // either fabricates a client of its own or unwraps the stored verdict.
    let traps = [
        "unwrap_or_default(",
        "unwrap_or_else(",
        "unwrap(",
        ".expect(",
        "Client::default(",
        "Client::new(",
        "Client::builder(",
    ];
    for marker in construction_fns {
        let region = code_only(&fn_region(src, marker));
        for trap in traps {
            assert!(
                !mentions(&region, trap),
                "{marker} must never reach for a fallback client or unwrap a build \
                 failure ({trap}); the verdict is stored and reported as \
                 ChatError::TrustStore"
            );
        }
    }

    // Positive side of the same guard: one memoized shared build, propagated
    // as an error, and consumed by every constructor.
    let helper = fn_region(src, "fn shared_http_client()");
    assert!(
        helper.contains("HTTP_CLIENT") && helper.contains("get_or_init"),
        "the shared client must be built at most once and memoized"
    );
    assert!(
        helper.contains("crate::net::build_http_client("),
        "the LLM client must keep constructing through the shared net::http builder"
    );
    assert!(
        helper.contains("map_err"),
        "a build failure must be mapped into HttpClientError, never discarded"
    );
    assert!(
        src.contains("static HTTP_CLIENT") && src.contains("OnceLock"),
        "the memoized verdict must live in client.rs so every constructor reports \
         the same reason (and src/net/http.rs stays a pure Result factory)"
    );
    assert_eq!(
        src.matches("http: shared_http_client()").count(),
        3,
        "from_config/new/new_with_token must each store the shared verdict"
    );

    // Whole-file belt-and-braces: nothing in this file may construct a reqwest
    // client behind the shared helper any more (the old `default_http_client()`
    // name is gone for good). Comment lines are excluded because the removed
    // trap is documented prose, not code.
    let code = code_only(src);
    for banned in [
        "Client::default(",
        "Client::new(",
        "Client::builder(",
        "default_http_client",
    ] {
        assert!(
            !mentions(&code, banned),
            "client.rs must not contain {banned} in code: it re-runs a build that \
             can fail and panics (bug.md)"
        );
    }
    for line in code.lines() {
        assert!(
            !(line.contains("build_http_client") && line.contains("unwrap")),
            "a build_http_client call must never be unwrapped: {line}"
        );
    }

    // Ordering: the verdict is refused **before** the pool gate, so a
    // store-less run occupies no concurrency slot of another session and burns
    // no retry attempt.
    let refusal = code
        .find("if let Err(err) = &self.http")
        .expect("chat_stream must refuse a client-less process");
    let gate = code
        .find("self.acquire_slot().await")
        .expect("chat_stream must still take its slot through acquire_slot");
    assert!(
        refusal < gate,
        "the trust-store refusal must precede the concurrency gate"
    );
}

/// Regression for `bug.md`: a stored HTTP-client failure must come back as an
/// ordinary `Err` — the process must never reach `reqwest::Client::default()`,
/// which is where the old fallback panicked with `Client::new(): ... No CA
/// certificates were loaded from the system` and exit code 101.
///
/// The mock backend answers with a **retryable** status, so had the failure
/// been treated as an ordinary transport problem the request would show up here
/// as attempts. Zero requests plus an instant refusal is the contract.
#[tokio::test]
async fn test_missing_trust_store_surfaces_as_error_not_a_panic() {
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

    let backend = server.uri();
    let client = ChatClient::new(backend.clone(), "test-model")
        .with_http_client_failure("builder error: No CA certificates were loaded from the system");

    let started = std::time::Instant::now();
    let err = client
        .chat(&request())
        .await
        .expect_err("a process with no HTTP client must return Err, never panic");
    let elapsed = started.elapsed();
    let msg = err.to_string();

    for expected in [
        "no TLS trust store",
        "No CA certificates were loaded from the system",
        "ca-certificates",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        backend.as_str(),
    ] {
        assert!(
            msg.contains(expected),
            "the failure must be actionable and attributable — it must name \
             {expected:?}, got: {msg}"
        );
    }

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "with no HTTP client there is no transport: not one request may be sent"
    );
    assert!(
        elapsed < Duration::from_millis(BACKOFF_BASE_MS),
        "the verdict is terminal, so no attempt and no backoff sleep may be spent \
         (the refusal took {elapsed:?}, one backoff step is {BACKOFF_BASE_MS} ms)"
    );
}

/// Classification guard: the stored verdict surfaces as
/// [`ChatError::TrustStore`] attributed to the client's backend, is **never**
/// retryable (the verdict was taken once, at construction, so a re-attempt can
/// only fail identically), leaves the turn in `Error` (nothing was ever sent, so
/// there is no partial stream to truncate) and is caused by `Transport`.
#[test]
fn test_trust_store_error_is_terminal_and_non_retryable() {
    let client = ChatClient::new("http://store-less:8080/v1", "llama-3")
        .with_http_client_failure("No CA certificates were loaded from the system");
    let stored = client
        .http
        .as_ref()
        .expect_err("the seam must store the build verdict, not a client");
    let error = client.trust_store_error(stored);

    assert!(
        matches!(
            &error,
            ChatError::TrustStore { backend, reason }
                if backend == "http://store-less:8080/v1"
                    && reason.contains("No CA certificates were loaded from the system")
        ),
        "the stored verdict must surface as ChatError::TrustStore naming its \
         backend, got: {error:?}"
    );
    assert!(
        !error.is_retryable(),
        "a client-less process must never be retried — the build verdict does not \
         change at runtime"
    );
    assert_eq!(
        error.terminal_outcome(),
        ReplyOutcome::Error,
        "nothing was sent, so there is no partially delivered stream to truncate"
    );
    assert_eq!(
        error.terminal_cause(),
        TerminalCause::Transport,
        "no client exists to speak the protocol: that is the transport class"
    );

    let msg = error.to_string();
    for expected in [
        "no TLS trust store",
        "ca-certificates",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "http://store-less:8080/v1",
    ] {
        assert!(
            msg.contains(expected),
            "the message must carry {expected:?}, got: {msg}"
        );
    }
}

/// The reason text is what makes the failure actionable, and reqwest's own
/// `Display` for a builder error is literally `"builder error"` — the useful
/// half ("No CA certificates were loaded from the system") only survives in the
/// source chain. Proven against a **real** reqwest build failure, obtainable
/// without a store-less host by inverting the TLS version range.
#[test]
fn test_build_failure_reason_keeps_the_actionable_chain() {
    let err = reqwest::Client::builder()
        .tls_version_min(reqwest::tls::Version::TLS_1_3)
        .tls_version_max(reqwest::tls::Version::TLS_1_2)
        .build()
        .expect_err("an inverted TLS range must fail the client build");
    assert!(err.is_builder(), "the probe must produce a builder error");
    assert_eq!(
        err.to_string(),
        "builder error",
        "reqwest's Display says nothing — this is why the chain has to be walked"
    );

    let reason = build_failure_reason(&err);
    assert!(
        reason.contains("empty supported tls versions"),
        "build_failure_reason must capture the source chain, got: {reason}"
    );
    assert_ne!(
        reason.trim(),
        "builder error",
        "the reason must be more than reqwest's bare kind"
    );

    // And that chain is what the user ends up reading, verbatim, through the
    // TrustStore error.
    let client = ChatClient::new("http://store-less:8080/v1", "m").with_http_client_failure(reason);
    let stored = client.http.as_ref().expect_err("verdict stored");
    let msg = client.trust_store_error(stored).to_string();
    assert!(
        msg.contains("empty supported tls versions") && msg.contains("no TLS trust store"),
        "the captured chain must survive into the reported error, got: {msg}"
    );
}

/// Process-level invariant, host-agnostic: constructing a client never panics
/// whether or not a trust store exists, and every client carries the *same*
/// verdict the startup probe reports (one memoized build, one shared reason).
#[test]
fn test_construction_verdict_matches_the_startup_probe() {
    let client = ChatClient::new("http://construction:8080/v1", "m");
    match http_client_startup_error() {
        // A store-less host: still no panic, and the client carries the reason.
        Some(message) => {
            assert!(
                client.http.is_err(),
                "the probe says the build failed, so this client must carry that verdict"
            );
            assert!(
                message.contains("no TLS trust store") && message.contains("SSL_CERT_FILE"),
                "the startup line must be the actionable one, got: {message}"
            );
        }
        // A normal host: the shared client was built once and reused.
        None => {
            assert!(
                client.http.is_ok(),
                "a host with a trust store must get a working client, not a verdict"
            );
            assert!(
                ChatClient::new("http://construction:8080/v1", "m")
                    .http
                    .as_ref()
                    .ok()
                    .is_some(),
                "the memoized verdict must stay Ok for every later client"
            );
        }
    }
}
