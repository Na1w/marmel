//! Phase D integration tests (direct streaming, thinking demuxer, request construction).

use marmennill::llm::ThinkingDemuxer;
use marmennill::llm::client::{ChatClient, ReplyOutcome, TerminalCause};
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

/// A bare-bones SSE backend on a real socket, so the transport can be cut in the
/// middle of a message. It answers every connection it accepts and counts how
/// many it accepted — the observable proof that a mid-stream failure is never
/// re-sent as a second request.
mod raw_backend {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    /// One SSE `data:` frame with a content delta.
    pub fn frame(text: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "chatcmpl-cut",
                "choices": [{ "delta": { "content": text }, "finish_reason": null }]
            })
        )
    }

    pub fn request() -> marmennill::types::ChatRequest {
        marmennill::types::ChatRequest {
            model: "test-model".to_string(),
            messages: vec![marmennill::types::Message::User {
                content: "hi".to_string(),
            }],
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stream: Some(true),
            enable_thinking: None,
            tools: None,
        }
    }

    pub struct Backend {
        base_url: String,
        accepted: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Backend {
        /// `responses[i]` is the sequence of SSE frames written on the i-th
        /// accepted connection (later connections reuse the last entry). The
        /// advertised `Content-Length` deliberately promises more bytes than
        /// arrive, and the socket is then closed: a dropped connection
        /// mid-message, exactly like a backend that dies while streaming.
        pub fn spawn(responses: Vec<Vec<String>>) -> Backend {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");

            let accepted = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let (accepted_task, stop_task) = (accepted.clone(), stop.clone());

            let handle = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while !stop_task.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let n = accepted_task.fetch_add(1, Ordering::SeqCst);
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                            let mut buf = [0u8; 16384];
                            let _ = stream.read(&mut buf);

                            let frames = responses
                                .get(n)
                                .cloned()
                                .unwrap_or_else(|| responses.last().cloned().unwrap());
                            let written: usize = frames.iter().map(|f| f.len()).sum();
                            let headers = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                                 Content-Length: {}\r\n\r\n",
                                written + 4096
                            );
                            let _ = stream.write_all(headers.as_bytes());
                            let _ = stream.flush();
                            for frame in &frames {
                                if stream.write_all(frame.as_bytes()).is_err() {
                                    break;
                                }
                                let _ = stream.flush();
                                // One TCP segment per SSE frame, so the client
                                // really does deliver deltas before the cut.
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            drop(stream);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });

            Backend {
                base_url: format!("http://{addr}"),
                accepted,
                stop,
                handle: Some(handle),
            }
        }

        pub fn base_url(&self) -> &str {
            &self.base_url
        }

        pub fn accepted(&self) -> usize {
            self.accepted.load(Ordering::SeqCst)
        }
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

/// (defect 2) A connection dropped after deltas were already delivered must not
/// trigger a full-stream retry of the same request: exactly ONE request reaches
/// the backend, and the turn terminates as `Truncated` with its partial output.
#[tokio::test]
async fn test_stream_cut_after_deltas_sends_exactly_one_request() {
    const DELTAS: usize = 60;
    let frames: Vec<String> = (0..DELTAS)
        .map(|i| raw_backend::frame(&format!("delta{i} ")))
        .collect();

    let backend = raw_backend::Backend::spawn(vec![frames]);
    let client = ChatClient::new(backend.base_url(), "test-model");

    let reply = client
        .chat(&raw_backend::request())
        .await
        .expect("the partial reply is returned honestly, carrying its terminal state");

    assert_eq!(reply.outcome, ReplyOutcome::Truncated);
    assert_eq!(reply.cause, TerminalCause::Transport);
    assert!(!reply.is_success());
    assert_eq!(reply.finish_reason, None);
    assert!(
        reply.deltas >= DELTAS / 2,
        "the delivered delta count must be recorded, got {}",
        reply.deltas
    );
    assert!(reply.bytes > 0, "the delivered byte count must be recorded");
    assert!(
        reply.content.starts_with("delta0 "),
        "partial output must be preserved, got {:?}",
        &reply.content[..reply.content.len().min(40)]
    );
    assert_eq!(
        backend.accepted(),
        1,
        "a failure after deltas must never re-send the same request"
    );
}

/// The guard must not disable the single retry owner: a failure before any delta
/// arrives (the body dies before the first SSE frame) is still retried, so the
/// turn can complete on the second attempt.
#[tokio::test]
async fn test_failure_before_any_delta_is_still_retried_once() {
    let dead_body_dies_first = Vec::new();
    let good = vec![
        raw_backend::frame("complete answer"),
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "chatcmpl-ok",
                "choices": [{ "delta": {}, "finish_reason": "stop" }]
            })
        ),
        "data: [DONE]\n\n".to_string(),
    ];

    let backend = raw_backend::Backend::spawn(vec![dead_body_dies_first, good]);
    let client = ChatClient::new(backend.base_url(), "test-model");

    let reply = client.chat(&raw_backend::request()).await.unwrap();

    assert!(
        reply.is_success(),
        "a clean second attempt must still succeed, got {:?}",
        reply.outcome
    );
    assert_eq!(reply.outcome, ReplyOutcome::Complete);
    assert_eq!(reply.content, "complete answer");
    assert_eq!(
        backend.accepted(),
        2,
        "a pre-delta transport failure stays retryable through the shared policy"
    );
}
