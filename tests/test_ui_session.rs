//! Reproduction regression test for the "session exits after one turn" defect
//! (execution plan task t-rpr1).
//!
//! Root cause (from `src/ui/mod.rs::run_session`): at the *bottom* of the main
//! loop the code calls `renderer.poll_input()` **unconditionally**. For the
//! interactive TUI renderer `poll_input()` is **non-blocking**
//! (`handle_events(false)` + `rx.try_recv()`). If the user has not typed a
//! complete line at the exact instant of that poll, it returns `None`, which
//! sets `keep_going = false` and terminates the session — even though the
//! renderer is interactive and should instead block (via `read_input()`) for
//! the next steering line.
//!
//! This test drives the real `run_session` with:
//!   * a **mock renderer** whose `poll_input()` always returns `None` (the
//!     "user hasn't typed at the poll instant" case) and whose `read_input()`
//!     returns a scripted sequence of lines; and
//!   * a **mock streaming adapter** (a wiremock backend that always yields a
//!     valid SSE reply) so the first turn succeeds.
//!
//! The assertion encodes the *required* behaviour: an interactive session must
//! NOT terminate after a single turn. It must keep looping, take a second
//! steering line ("steer2"), run a **second** turn, and only then terminate via
//! an explicit `/abort`.
//!
//! ## Documented failing behaviour (current code)
//!
//! With the un-fixed loop, after turn 1 the loop-bottom `poll_input()` returns
//! `None` → `keep_going = false` → the session exits after **one** turn:
//!   * the backend was called only **once** (assertion `backend_calls == 2`
//!     fails with `== 1`); and
//!   * the renderer never received an abort (`aborted() == false`).
//!
//! The test therefore FAILS against the current code, documenting the bug. It
//! is expected to pass once `src/ui/mod.rs::run_session` uses the blocking
//! `read_input()` at the loop bottom for interactive renderers (t-fix1).

use marmennill::config::Config;
use marmennill::ui::{Event, Renderer};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A scripted mock renderer that reproduces the TUI's non-blocking poll.
///
/// * `poll_input()` always returns `None` — it is the non-blocking poll that,
///   at the instant after turn 1, finds no completed input line.
/// * `read_input()` draws from a scripted queue of lines: the first call is the
///   initial *goal*; later calls are the steering lines the *fixed* loop-bottom
///   would consume while blocking for input.
struct ScriptedRenderer {
    /// Lines returned by `read_input()`, consumed in order.
    read_script: Vec<String>,
    read_cursor: usize,
    /// Lines returned by `poll_input()`, consumed in order.
    poll_script: Vec<String>,
    poll_cursor: usize,
    /// Whether an abort was requested (via `/abort`).
    aborted: bool,
}

impl ScriptedRenderer {
    fn new(read_script: Vec<String>) -> Self {
        Self {
            read_script,
            read_cursor: 0,
            poll_script: Vec::new(),
            poll_cursor: 0,
            aborted: false,
        }
    }

    fn with_poll(read_script: Vec<String>, poll_script: Vec<String>) -> Self {
        Self {
            read_script,
            read_cursor: 0,
            poll_script,
            poll_cursor: 0,
            aborted: false,
        }
    }
}

impl Renderer for ScriptedRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, _event: &Event) {}
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Non-blocking: returns scripted poll lines or `None`.
    fn poll_input(&mut self) -> Option<String> {
        if self.poll_cursor < self.poll_script.len() {
            let line = self.poll_script[self.poll_cursor].clone();
            self.poll_cursor += 1;
            if line.is_empty() { None } else { Some(line) }
        } else {
            None
        }
    }
    /// Blocking: returns the next scripted line, or `None` when exhausted.
    fn read_input(&mut self) -> Option<String> {
        let line = self.read_script.get(self.read_cursor).cloned();
        self.read_cursor += 1;
        line
    }
    fn request_abort(&mut self) {
        self.aborted = true;
    }
    fn aborted(&self) -> bool {
        self.aborted
    }
    fn clear_abort(&mut self) {
        self.aborted = false;
    }
    fn shutdown(&mut self) {}
}

/// Build a `Config` pointing at a mock backend that always yields a valid reply.
fn config_for_backend(backend: &str) -> Config {
    Config {
        backend_url: format!("{backend}/v1"),
        // Point at a real, loadable system prompt so `load_system_prompt` succeeds.
        system_prompt_path: PathBuf::from("prompts/system.md"),
        ui_mode: "tui".to_string(),
        ..Config::default()
    }
}

/// A canned OpenAI-style SSE completion body (one valid assistant reply).
fn completion_sse(text: &str) -> String {
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

/// The reproduction test (t-rpr1).
///
/// Asserts the *required* behaviour: the loop must NOT terminate after a single
/// turn. It should take a second steering line and run a second turn, then stop
/// via an explicit `/abort`.
#[tokio::test]
async fn test_ui_run_session_continues_after_first_turn() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Count how many turns actually reach the (mock) backend.
    let calls = Arc::new(AtomicUsize::new(0));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &wiremock::Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                // Every turn yields a valid, non-empty assistant reply.
                let body = if n == 0 {
                    completion_sse("first reply")
                } else {
                    completion_sse("second reply")
                };
                ResponseTemplate::new(200).set_body_string(body)
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    // Scripted interactive user input:
    //   1. "goal"        -> initial goal (read_input, before the loop).
    //   2. "steer2"      -> a steering line the loop-bottom should read (blocking)
    //                       after turn 1, causing a SECOND turn.
    //   3. "/abort"      -> explicit abort to terminate cleanly after turn 2.
    let mut renderer = ScriptedRenderer::new(vec![
        "goal".to_string(),
        "steer2".to_string(),
        "/abort".to_string(),
    ]);

    // No orchestrator manager (headless of the delegation subsystem).
    marmennill::ui::run_session(&cfg, &mut renderer, None, None)
        .await
        .expect("run_session should complete without error");

    let backend_calls = calls.load(Ordering::SeqCst);

    // REQUIRED behaviour: the interactive loop must NOT stop after one turn.
    // With the bug, `poll_input()` at the loop bottom returns `None` and the
    // session exits after turn 1 (backend_calls == 1). The correct behaviour is
    // a second turn (backend_calls == 2).
    assert_eq!(
        backend_calls, 2,
        "loop must NOT terminate after a single turn: expected 2 backend calls \
         (a second turn from the 'steer2' line), but the loop exited after {} turn(s) \
         because the loop-bottom `poll_input()` returned `None` and set keep_going=false",
        backend_calls
    );

    // The session should have terminated via an explicit `/abort`, not by the
    // silent `keep_going = false` path.
    assert!(
        renderer.aborted(),
        "an interactive session must terminate via an explicit /abort, not by the \
         silent keep_going=false path triggered by a non-blocking poll_input()==None"
    );
}

/// Verify that when an active execution plan is incomplete, the session
/// automatically nudges the model (up to 5 times) to continue emitting tool calls,
/// and respects the 5-retry safeguard.
#[tokio::test]
async fn test_ui_run_session_auto_nudges_when_plan_incomplete_capped_at_5() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let calls = Arc::new(AtomicUsize::new(0));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &wiremock::Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                // Return plain text without tool calls
                let body = completion_sse("I am thinking about the task.");
                ResponseTemplate::new(200).set_body_string(body)
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::agent::phase::Plan::at(tmp.path());
    plan.create("# Plan\n- [ ] [t-001] incomplete task\n")
        .expect("plan created");

    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(&cfg.backend_url, "test-model"),
        plan,
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let mut renderer = ScriptedRenderer::new(vec!["start".to_string(), "/abort".to_string()]);

    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(manager))
        .await
        .expect("run_session should complete");

    let backend_calls = calls.load(Ordering::SeqCst);
    // 1 initial turn + 5 auto-nudges before hitting the 5-retry safeguard and yielding to read_input
    assert_eq!(
        backend_calls, 6,
        "expected 1 initial turn + 5 auto-nudge retries = 6 backend calls, but got {backend_calls}"
    );
}

fn tool_call_sse(id: &str, name: &str, args: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args
                        }
                    }]
                },
                "finish_reason": null
            }]
        })
    )
}

/// Verify that when mid-flight steering arbitration decides `AbortImmediately`,
/// the session aborts the current turn, clears the abort flag, injects the steering
/// redirection into context, and stays alive for the subsequent turn instead of exiting.
#[tokio::test]
async fn test_ui_session_steer_abort_redirection_resets_abort_and_continues() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let assistant_turns = Arc::new(AtomicUsize::new(0));
    let arbitrator_calls = Arc::new(AtomicUsize::new(0));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let assistant_turns = assistant_turns.clone();
            let arbitrator_calls = arbitrator_calls.clone();
            move |req: &wiremock::Request| {
                let body_str = String::from_utf8_lossy(&req.body);
                if body_str.contains("Steer Arbitrator") || body_str.contains("Arbitrate the user")
                {
                    arbitrator_calls.fetch_add(1, Ordering::SeqCst);
                    let body =
                        completion_sse(r#"{"decision": "AbortImmediately", "response": null}"#);
                    ResponseTemplate::new(200).set_body_string(body)
                } else {
                    let n = assistant_turns.fetch_add(1, Ordering::SeqCst);
                    let body = if n == 0 {
                        tool_call_sse("call_1", "read_file", r#"{"path": "Cargo.toml"}"#)
                    } else {
                        completion_sse("Second turn response redirected to user instruction.")
                    };
                    ResponseTemplate::new(200).set_body_string(body)
                }
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    // Scripted input:
    // Initial goal: "start long task"
    // Mid-flight poll input: "" (before turn 1), then "stop and list files instead" during tool execution
    // After turn 2: "/abort" to finish
    let mut renderer = ScriptedRenderer::with_poll(
        vec!["start long task".to_string(), "/abort".to_string()],
        vec![String::new(), "stop and list files instead".to_string()],
    );

    marmennill::ui::run_session(&cfg, &mut renderer, None, None)
        .await
        .expect("run_session should complete without error");

    let total_assistant_turns = assistant_turns.load(Ordering::SeqCst);
    assert!(
        total_assistant_turns >= 2,
        "expected at least 2 assistant turns (turn 1 aborted + turn 2 redirection), but got {total_assistant_turns}"
    );

    assert!(renderer.aborted(), "session should end via final /abort");
}

/// Verify that when user inputs a question mid-stream during LLM generation,
/// the active stream pauses immediately, the steer arbitrator answers with RespondDirectly,
/// and the stream seamlessly resumes with assistant prefix continuation, delivering the full answer.
#[tokio::test]
async fn test_ui_session_stream_pause_and_resume_on_user_question() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let assistant_requests = Arc::new(AtomicUsize::new(0));
    let arbitrator_calls = Arc::new(AtomicUsize::new(0));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let assistant_requests = assistant_requests.clone();
            let arbitrator_calls = arbitrator_calls.clone();
            move |req: &wiremock::Request| {
                let body_str = String::from_utf8_lossy(&req.body);
                if body_str.contains("Steer Arbitrator") || body_str.contains("Arbitrate the user")
                {
                    arbitrator_calls.fetch_add(1, Ordering::SeqCst);
                    let body = completion_sse(
                        r#"{"decision": "RespondDirectly", "response": "I am currently analyzing your project files."}"#,
                    );
                    ResponseTemplate::new(200).set_body_string(body)
                } else {
                    let n = assistant_requests.fetch_add(1, Ordering::SeqCst);
                    let body = if n == 0 {
                        // Part 1 before pause
                        completion_sse("First part of generation...")
                    } else {
                        // Part 2 continuation after resume
                        completion_sse(" and second part after resume.")
                    };
                    ResponseTemplate::new(200).set_body_string(body)
                }
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    // Scripted input:
    // Initial goal: "start generation task"
    // Mid-flight poll input: "what are you doing?"
    // After turn: "/abort" to finish session
    let mut renderer = ScriptedRenderer::with_poll(
        vec!["start generation task".to_string(), "/abort".to_string()],
        vec![
            String::new(),
            String::new(),
            String::new(),
            "what are you doing?".to_string(),
        ],
    );

    marmennill::ui::run_session(&cfg, &mut renderer, None, None)
        .await
        .expect("run_session should complete without error");

    let arb = arbitrator_calls.load(Ordering::SeqCst);
    assert_eq!(
        arb, 1,
        "steer arbitrator should have been invoked exactly once"
    );

    let reqs = assistant_requests.load(Ordering::SeqCst);
    assert_eq!(
        reqs, 2,
        "expected initial stream + continuation stream = 2 assistant requests"
    );

    assert!(renderer.aborted(), "session should end via final /abort");
}

/// Verify that a running specialist stream on a shared model can be preempted
/// by the Steer Arbitrator, yielding its GPU/model slot, and subsequently
/// resumed seamlessly via Assistant Prefill continuation.
#[tokio::test]
async fn test_specialist_stream_preemption_and_resumption_on_shared_model() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (first_chunk_sent_tx, first_chunk_sent_rx) = tokio::sync::oneshot::channel();
    let (preempt_done_tx, preempt_done_rx) = tokio::sync::oneshot::channel();

    // Spawn mock streaming server
    tokio::spawn(async move {
        // First connection: initial specialist stream
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf).await;

        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
        stream.write_all(headers.as_bytes()).await.unwrap();

        // Send chunk 1
        let chunk1_data =
            "data: {\"choices\":[{\"delta\":{\"content\":\"Specialist part 1...\"}}]}\n\n";
        let chunk1 = format!("{:x}\r\n{}\r\n", chunk1_data.len(), chunk1_data);
        stream.write_all(chunk1.as_bytes()).await.unwrap();

        // Give client time to read and parse chunk 1
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Notify test that first chunk was sent
        let _ = first_chunk_sent_tx.send(());

        // Wait until steer preemption has been registered
        let _ = preempt_done_rx.await;

        // Send chunk 2 to trigger poll_control inside on_chunk_with_sink
        let chunk2_data = "data: {\"choices\":[{\"delta\":{\"content\":\" (cutting)\"}}]}\n\n";
        let chunk2 = format!("{:x}\r\n{}\r\n", chunk2_data.len(), chunk2_data);
        let _ = stream.write_all(chunk2.as_bytes()).await;

        // Client drops stream when paused.
        // Second connection: continuation stream after resume
        if let Ok((mut stream2, _)) = listener.accept().await {
            let mut buf2 = [0u8; 1024];
            let _ = stream2.read(&mut buf2).await;
            let headers2 = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            stream2.write_all(headers2.as_bytes()).await.unwrap();

            let chunk_cont_data = "data: {\"choices\":[{\"delta\":{\"content\":\" and specialist part 2 completed.\"}}]}\n\n";
            let chunk_cont = format!("{:x}\r\n{}\r\n", chunk_cont_data.len(), chunk_cont_data);
            stream2.write_all(chunk_cont.as_bytes()).await.unwrap();

            let done_data = "data: [DONE]\n\n";
            let done_chunk = format!("{:x}\r\n{}\r\n0\r\n\r\n", done_data.len(), done_data);
            stream2.write_all(done_chunk.as_bytes()).await.unwrap();
        }
    });

    let client = marmennill::llm::ChatClient::new(format!("http://{addr}/v1"), "shared-model");
    let req = marmennill::types::ChatRequest {
        model: "shared-model".to_string(),
        messages: vec![marmennill::types::Message::User {
            content: "write code".to_string(),
        }],
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
        stream: Some(true),
        enable_thinking: None,
        tools: None,
    };

    let mut rep_detector = marmennill::harness::monitor::RepetitionDetector::new(3, 5);
    let mut sink =
        marmennill::orchestrator::PreemptibleStreamSink::register("coder", "shared-model");

    // Spawn specialist stream in background
    let specialist_task = tokio::spawn(async move {
        marmennill::llm::chat_stream_resumable(
            &client,
            &req,
            &mut sink,
            512,
            &mut rep_detector,
            false,
            None,
        )
        .await
    });

    // Wait until specialist received first chunk
    first_chunk_sent_rx.await.unwrap();

    // Now preempt the active stream for the Steer Arbitrator
    let preempt_fut =
        marmennill::orchestrator::preempt_conflicting_stream("shared-model", "what is happening?");

    // Unblock the mock server to deliver chunk 2 so the client polls control and pauses
    let _ = preempt_done_tx.send(());

    let handle = preempt_fut.await;
    assert!(
        matches!(handle, marmennill::orchestrator::PreemptHandle::Active(_)),
        "expected an active preempt handle for conflicting shared-model stream"
    );

    // Arbitrator finishes and grants resumption
    handle.complete_all(marmennill::llm::PauseAction::Resume);

    let stream_out = specialist_task
        .await
        .expect("specialist task join")
        .expect("stream resumable result");

    assert!(
        stream_out.reply.content.contains("Specialist part 1..."),
        "result should contain pre-pause content"
    );
    assert!(
        stream_out
            .reply
            .content
            .contains("and specialist part 2 completed."),
        "result should contain post-resumption content"
    );
}
