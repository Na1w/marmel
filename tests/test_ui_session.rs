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

static TEST_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

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
    /// Shared abort / user-exit flags (trait-default abort surface).
    input_state: marmennill::ui::InputState,
    subagents: Vec<marmennill::ui::SubagentDetail>,
    events: Vec<Event>,
}

impl ScriptedRenderer {
    fn new(read_script: Vec<String>) -> Self {
        Self {
            read_script,
            read_cursor: 0,
            poll_script: Vec::new(),
            poll_cursor: 0,
            input_state: marmennill::ui::InputState::default(),
            subagents: Vec::new(),
            events: Vec::new(),
        }
    }

    fn with_poll(read_script: Vec<String>, poll_script: Vec<String>) -> Self {
        Self {
            read_script,
            read_cursor: 0,
            poll_script,
            poll_cursor: 0,
            input_state: marmennill::ui::InputState::default(),
            subagents: Vec::new(),
            events: Vec::new(),
        }
    }
}

impl Renderer for ScriptedRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        self.events.push(event.clone());
    }
    fn set_subagents(&mut self, subagents: Vec<marmennill::ui::SubagentDetail>) {
        self.subagents = subagents;
    }
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
    // Abort-flag surface uses the trait defaults backed by `input_state`,
    // except `request_user_exit`, which also sets the user-exit flag.
    fn input_state(&mut self) -> &mut marmennill::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &marmennill::ui::InputState {
        &self.input_state
    }
    fn request_user_exit(&mut self) {
        self.input_state.user_exit = true;
        self.input_state.aborted = true;
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
    let _lock = TEST_MUTEX.lock().await;
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

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::manager::Plan::at(tmp.path());
    let stats = std::sync::Arc::new(marmennill::harness::HarnessStats::new());
    let manager = std::sync::Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        stats,
    ));

    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(manager))
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
    let _lock = TEST_MUTEX.lock().await;
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
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
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
                        "index": 0,
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
    let _lock = TEST_MUTEX.lock().await;
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

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::manager::Plan::at(tmp.path());
    let stats = std::sync::Arc::new(marmennill::harness::HarnessStats::new());
    let manager = std::sync::Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        stats,
    ));

    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(manager))
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
    let _lock = TEST_MUTEX.lock().await;
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

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::manager::Plan::at(tmp.path());
    let stats = std::sync::Arc::new(marmennill::harness::HarnessStats::new());
    let manager = std::sync::Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        stats,
    ));

    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(manager))
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
    let _lock = TEST_MUTEX.lock().await;
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
            16384,
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

#[tokio::test]
async fn test_steering_conversation_history_accumulates_and_passes_to_arbitrator() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let received_bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let rb = received_bodies.clone();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            let body = String::from_utf8_lossy(&req.body).to_string();
            rb.lock().unwrap().push(body.clone());
            if body.contains("How many are left?") {
                ResponseTemplate::new(200).set_body_string(
                    "data: {\"id\":\"c2\",\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\": \\\"RespondDirectly\\\", \\\"response\\\": \\\"There are 2 tests remaining.\\\"}\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
                )
            } else {
                ResponseTemplate::new(200).set_body_string(
                    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\": \\\"RespondDirectly\\\", \\\"response\\\": \\\"Running tests right now.\\\"}\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
                )
            }
        })
        .mount(&server)
        .await;

    let client =
        marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test".to_string());
    let stats = Arc::new(marmennill::harness::HarnessStats::new());
    let (arb_tx, mut arb_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = ScriptedRenderer::new(vec![]);
    let steering_history = Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));

    // Turn 1: user asks "What are you doing now?"
    marmennill::ui::bridge::spawn_steer_arbitration(
        &client,
        stats.clone(),
        "build system",
        &[],
        "What are you doing now?".to_string(),
        &arb_tx,
        &mut renderer,
        Some(Arc::clone(&steering_history)),
    );

    // Wait for first arbitration to finish
    loop {
        if let Some(event) = arb_rx.recv().await
            && matches!(
                event,
                marmennill::ui::bridge::SteerArbEvent::Finished { .. }
            )
        {
            break;
        }
    }

    // Verify history now contains turn 1
    {
        let hist = steering_history.read().unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].0, "What are you doing now?");
        assert!(hist[0].1.contains("Running tests right now."));
    }

    // Turn 2: user asks "How many are left?"
    marmennill::ui::bridge::spawn_steer_arbitration(
        &client,
        stats.clone(),
        "build system",
        &[],
        "How many are left?".to_string(),
        &arb_tx,
        &mut renderer,
        Some(Arc::clone(&steering_history)),
    );

    // Wait for second arbitration to finish
    loop {
        if let Some(event) = arb_rx.recv().await
            && matches!(
                event,
                marmennill::ui::bridge::SteerArbEvent::Finished { .. }
            )
        {
            break;
        }
    }

    // Verify history now contains turn 1 AND turn 2
    {
        let hist = steering_history.read().unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[1].0, "How many are left?");
        assert!(hist[1].1.contains("There are 2 tests remaining."));
    }

    // Verify that the second request payload sent to the mock server actually contained the history!
    let bodies = received_bodies.lock().unwrap();
    assert!(bodies.len() >= 2);
    let second_req = &bodies[1];
    assert!(
        second_req.contains("What are you doing now?")
            && second_req.contains("Running tests right now."),
        "Second request must contain the accumulated conversation history, got: {second_req}"
    );
}

#[tokio::test]
async fn test_steering_conversation_history_accumulates_worker_reply_to_arbitrator() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let received_bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let rb = received_bodies.clone();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            let body = String::from_utf8_lossy(&req.body).to_string();
            rb.lock().unwrap().push(body.clone());
            if body.contains("Original User Inquiry") {
                // Evaluation of worker reply
                ResponseTemplate::new(200).set_body_string(
                    "data: {\"id\":\"c_eval\",\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\": \\\"SynthesizeResponse\\\", \\\"response\\\": \\\"Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar.\\\"}\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
                )
            } else if body.contains("Hur gick testet?") {
                // Turn 2: user asks follow-up
                ResponseTemplate::new(200).set_body_string(
                    "data: {\"id\":\"c2\",\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\": \\\"RespondDirectly\\\", \\\"response\\\": \\\"Testet gick bra.\\\"}\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
                )
            } else {
                // Turn 1: user asks "Hur går det för codern?" -> ForwardToWorker
                ResponseTemplate::new(200).set_body_string(
                    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\": \\\"ForwardToWorker\\\", \\\"subtasks\\\": [{\\\"tool_call_id\\\": \\\"call_1\\\", \\\"action\\\": \\\"ForwardNotice\\\", \\\"agent_name\\\": \\\"coder\\\", \\\"message\\\": \\\"Hur går det för codern?\\\"}]}\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
                )
            }
        })
        .mount(&server)
        .await;

    let client =
        marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test".to_string());
    let cfg = marmennill::config::Config {
        backend_url: format!("{}/v1", server.uri()),
        ..Default::default()
    };
    marmennill::config::set_active(cfg);

    let stats = Arc::new(marmennill::harness::HarnessStats::new());
    let (arb_tx, mut arb_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    marmennill::orchestrator::set_event_sender(event_tx);

    let mut renderer = ScriptedRenderer::new(vec![]);
    let steering_history = Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
    marmennill::orchestrator::set_steering_history(Arc::clone(&steering_history));

    // Turn 1: user asks "Hur går det för codern?"
    marmennill::ui::bridge::spawn_steer_arbitration(
        &client,
        stats.clone(),
        "build PPC JIT",
        &[],
        "Hur går det för codern?".to_string(),
        &arb_tx,
        &mut renderer,
        Some(Arc::clone(&steering_history)),
    );

    // Wait for first arbitration to finish
    loop {
        if let Some(event) = arb_rx.recv().await
            && matches!(
                event,
                marmennill::ui::bridge::SteerArbEvent::Finished { .. }
            )
        {
            break;
        }
    }

    // Verify history recorded initial pending status
    let notice_id = {
        let hist = steering_history.read().unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].0, "Hur går det för codern?");
        assert!(hist[0].1.contains("awaiting specialist reply"));
        // Extract notice id from recorded text (e.g. "Forwarded notice notice-1 to coder...")
        let text = &hist[0].1;
        let start = text.find("notice-").unwrap();
        let end = text[start..]
            .find(' ')
            .map(|i| start + i)
            .unwrap_or(text.len());
        text[start..end].to_string()
    };

    // Specialist worker replies via handle_reply_to_arbitrator_async
    let args = serde_json::json!({
        "notice_id": notice_id,
        "message": "Status: tagit bort de två jit_invalidate_all()-anropen och ctest 7/7 passerar."
    });
    let reply_res = marmennill::harness::handle_reply_to_arbitrator_async("coder", &args).await;
    assert!(reply_res.is_ok());

    // Verify history was UPDATED with worker reply and synthesized response
    {
        let hist = steering_history.read().unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].0, "Hur går det för codern?");
        assert!(hist[0].1.contains(
            "Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar."
        ));
        assert!(hist[0].1.contains("[Specialist coder]:"));
        assert!(!hist[0].1.contains("awaiting specialist reply"));
    }

    // Turn 2: user asks "Hur gick testet?"
    marmennill::ui::bridge::spawn_steer_arbitration(
        &client,
        stats.clone(),
        "build PPC JIT",
        &[],
        "Hur gick testet?".to_string(),
        &arb_tx,
        &mut renderer,
        Some(Arc::clone(&steering_history)),
    );

    loop {
        if let Some(event) = arb_rx.recv().await
            && matches!(
                event,
                marmennill::ui::bridge::SteerArbEvent::Finished { .. }
            )
        {
            break;
        }
    }

    // Verify that the prompt sent to the model for turn 2 contained the specialist's reply in steering history!
    let bodies = received_bodies.lock().unwrap();
    let turn2_req = bodies
        .iter()
        .find(|b| b.contains("Hur gick testet?"))
        .expect("Turn 2 request must exist");
    assert!(
        turn2_req
            .contains("Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar."),
        "Turn 2 request must contain the specialist reply from steering history! Request was: {turn2_req}"
    );
}

#[tokio::test]
async fn test_stream_preemption_on_synchronous_bridge() {
    let _lock = TEST_MUTEX.lock().await;
    use marmennill::llm::{PauseAction, StreamControl, StreamSink};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\": \\\"RespondDirectly\\\", \\\"response\\\": \\\"Pausing and answering.\\\"}\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
        ))
        .mount(&server)
        .await;

    let client = marmennill::llm::ChatClient::new(
        format!("{}/v1", server.uri()),
        "test-shared-model".to_string(),
    );
    let stats = Arc::new(marmennill::harness::HarnessStats::new());

    // Register an active specialist stream using the same model
    let mut specialist_sink =
        marmennill::orchestrator::PreemptibleStreamSink::register("coder", "test-shared-model");

    let mut renderer = ScriptedRenderer::new(vec![]);
    let mut steer_queue = Vec::new();
    let mut steer_abort = false;
    let (arb_tx, _arb_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_arb_tx2, mut arb_rx2) = tokio::sync::mpsc::unbounded_channel();
    let steering_history = Arc::new(std::sync::RwLock::new(Vec::new()));

    let mut bridge = marmennill::ui::bridge::RendererSink {
        renderer: &mut renderer,
        steer_queue: &mut steer_queue,
        steer_abort_requested: &mut steer_abort,
        arb_tx: &arb_tx,
        arb_rx: &mut arb_rx2,
        client: &client,
        stats,
        goal: "test goal",
        subagents: &[],
        plan: None,
        ctx: None,
        steering_history: Some(steering_history),
    };

    // Run on_pause concurrently with specialist stream yielding its slot
    let pause_fut = bridge.on_pause("What is the status?");
    let specialist_fut = async {
        // Specialist polls control, sees pause signal, and awaits on_pause action
        let mut attempts = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if matches!(specialist_sink.poll_control(), StreamControl::Pause { .. }) {
                break;
            }
            attempts += 1;
            if attempts > 50 {
                panic!("Specialist did not receive Pause signal in time");
            }
        }
        specialist_sink.on_pause("").await
    };

    let (bridge_action, specialist_action) = tokio::join!(pause_fut, specialist_fut);
    assert_eq!(bridge_action, PauseAction::Resume);
    assert_eq!(specialist_action, PauseAction::Resume);
}

#[tokio::test]
async fn test_steering_arbitrator_sleep_re_invokes_after_delay() {
    let _lock = TEST_MUTEX.lock().await;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let req_counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&req_counter);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &Request| {
            let count = counter_clone.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                // Call 1: Arbitrator decides to Sleep for 1 second
                ResponseTemplate::new(200).set_body_string(
                    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\": \\\"Sleep\\\", \\\"sleep_seconds\\\": 1, \\\"response\\\": \\\"Waiting 1s for tests...\\\"}\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
                )
            } else {
                // Call 2: After waking up from sleep, Arbitrator is re-invoked and responds directly!
                ResponseTemplate::new(200).set_body_string(
                    "data: {\"id\":\"c2\",\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\": \\\"RespondDirectly\\\", \\\"response\\\": \\\"Tests have now completed without errors.\\\"}\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
                )
            }
        })
        .mount(&server)
        .await;

    let client =
        marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test".to_string());
    let stats = Arc::new(marmennill::harness::HarnessStats::new());
    let (arb_tx, mut arb_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut renderer = ScriptedRenderer::new(vec![]);
    let steering_history = Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));

    marmennill::ui::bridge::spawn_steer_arbitration(
        &client,
        stats.clone(),
        "run tests",
        &[],
        "Wait for tests and report".to_string(),
        &arb_tx,
        &mut renderer,
        Some(Arc::clone(&steering_history)),
    );

    let mut finished_decision = None;
    let mut received_deltas = Vec::new();

    while let Some(event) = arb_rx.recv().await {
        match event {
            marmennill::ui::bridge::SteerArbEvent::Delta(d) => received_deltas.push(d),
            marmennill::ui::bridge::SteerArbEvent::Finished { decision, .. } => {
                finished_decision = decision;
                break;
            }
            _ => {}
        }
    }

    // Verify that the arbitrator was called twice: once for Sleep, and once re-invoked after waking up!
    assert_eq!(req_counter.load(Ordering::SeqCst), 2);

    // Verify deltas include sleep and wake-up notifications
    let all_deltas = received_deltas.join("");
    assert!(all_deltas.contains("sleeping for 1s"));
    assert!(all_deltas.contains("woke up after 1s"));

    // Verify the final decision was RespondDirectly with the re-evaluated answer
    let dec = finished_decision.expect("must have finished decision");
    assert_eq!(dec.decision, "RespondDirectly");
    assert_eq!(
        dec.response.as_deref(),
        Some("Tests have now completed without errors.")
    );

    // Verify conversation history recorded the progression
    let hist = steering_history.read().unwrap();
    assert_eq!(hist.len(), 2);
    assert!(hist[0].1.contains("slept for 1s"));
    assert!(
        hist[1]
            .1
            .contains("Tests have now completed without errors.")
    );
}

/// Verify that an interactive session persists its visible chat history to
/// `.ui_transcript.json` (the persisted UI transcript) so it can be loaded back
/// from disk with `UiTranscript::load`.
#[tokio::test]
async fn test_ui_session_saves_ui_transcript_to_disk() {
    let _lock = TEST_MUTEX.lock().await;
    use marmennill::ui::{UiRecord, UiTranscript};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(completion_sse("First assistant reply.")),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let cfg = Config {
        backend_url: format!("{}/v1", server.uri()),
        system_prompt_path: PathBuf::from("prompts/system.md"),
        ui_mode: "tui".to_string(),
        ..Config::default()
    };

    // Run one turn with the goal "First goal", then end the session with "/abort".
    let mgr = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::from_config(&cfg),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));
    let mut renderer = ScriptedRenderer::new(vec!["First goal".to_string(), "/abort".to_string()]);
    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr))
        .await
        .expect("session succeeds");

    // Assert that .ui_transcript.json was created on disk
    let ui_transcript_path = plan.ui_transcript_path();
    assert!(
        ui_transcript_path.exists(),
        "ui_transcript.json must be persisted"
    );

    let loaded = UiTranscript::load(&ui_transcript_path).expect("must load ui_transcript.json");
    assert_eq!(loaded.records().len(), 2);
    assert_eq!(
        loaded.records()[0],
        UiRecord::User {
            text: "First goal".to_string()
        }
    );
    assert_eq!(
        loaded.records()[1],
        UiRecord::Assistant {
            content: Some("First assistant reply.".to_string()),
            thinking: None,
        }
    );
}

/// Verify that every session starts from a clean slate and still persists state:
/// 1. Startup loads no prior on-disk state — the scripted line becomes the goal.
/// 2. A stale `.ui_transcript.json` from an earlier run is not carried into the
///    transcript written by the new session.
/// 3. Saving the UI transcript still happens during the session.
#[tokio::test]
async fn test_ui_session_starts_fresh_and_still_saves_transcript() {
    let _lock = TEST_MUTEX.lock().await;
    use marmennill::ui::{UiRecord, UiTranscript};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(completion_sse("Fresh session reply.")),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    plan.create("# Plan\n- [ ] [t-001] old pending task\n")
        .expect("plan created");

    // Pre-create a stale UI transcript left behind by an earlier run.
    let mut old_ui_transcript = UiTranscript::new();
    old_ui_transcript.append(UiRecord::User {
        text: "Old user goal from yesterday".to_string(),
    });
    old_ui_transcript
        .save(plan.ui_transcript_path())
        .expect("old transcript saved");

    let cfg = Config {
        backend_url: format!("{}/v1", server.uri()),
        system_prompt_path: PathBuf::from("prompts/system.md"),
        ui_mode: "tui".to_string(),
        ..Config::default()
    };

    let mgr = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::from_config(&cfg),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    // The user provides a brand new goal; it becomes the goal of this session.
    let mut renderer =
        ScriptedRenderer::new(vec!["Brand new goal".to_string(), "/abort".to_string()]);

    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr))
        .await
        .expect("session runs cleanly");

    // 1. A new session writes its own transcript to disk.
    let saved = UiTranscript::load(plan.ui_transcript_path()).expect("must load ui_transcript");
    let has_fresh_goal = saved.records().iter().any(|r| match r {
        UiRecord::User { text } => text == "Brand new goal",
        _ => false,
    });
    assert!(has_fresh_goal, "New session must still save transcripts");

    // 2. The stale transcript from the earlier run is not carried over.
    let stale_carried_over = saved.records().iter().any(|r| match r {
        UiRecord::User { text } => text == "Old user goal from yesterday",
        _ => false,
    });
    assert!(
        !stale_carried_over,
        "New session must start from an empty transcript, not from disk state"
    );
}

// ---------------------------------------------------------------------------
// t-031b — DEFECT CLASS H2 (live executor): leaked in-flight round work on a
// hard error path.
//
// The live delegation round lives in `src/ui/session.rs`: every parallel
// tool batch (`all_parallel && tool_calls.len() > 1`) is fanned out into
// `tokio::task::spawn_blocking` handles and then joined one by one. Every
// *abort* exit of that join loop calls `crate::orchestrator::cancel_all()`
// first, which (i) cancels the global cancellation token that in-flight PTY
// command loops poll (`src/harness/pty.rs:253` -> `session.teardown()` ->
// `kill_process_group`) and (ii) cancels every registered worker token via
// `cancel_all_active_workers()` (`src/orchestrator/workers.rs:527`).
//
// A *hard error* raised inside the round (here: a renderer write failure, the
// `renderer.flush()?` at `src/ui/session.rs:506`/`:705`) used to propagate out
// of `run_session` with `?` and simply **detach** the remaining handles — no
// `cancel_all()`, so the delegated/parallel work kept running after the session
// aborted.
//
// The test is fully hermetic: no `openpty`, no real specialist. The in-flight
// work is modelled the same way `OrchestratorManager::delegate` models it
// (`src/orchestrator/mod.rs:276-281`): a worker registered in the live registry
// with an explicit cancellation token, kept active until the round aborts.
// ---------------------------------------------------------------------------

/// SSE body with TWO read-only tool calls -> forces the parallel fan-out branch.
fn two_parallel_tool_calls_sse() -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-h2",
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_a",
                            "type": "function",
                            "function": { "name": "read_file", "arguments": "{\"path\": \"Cargo.toml\"}" }
                        },
                        {
                            "index": 1,
                            "id": "call_b",
                            "type": "function",
                            "function": { "name": "read_file", "arguments": "{\"path\": \"AGENTS.md\"}" }
                        }
                    ]
                },
                "finish_reason": null
            }]
        })
    )
}

/// Scripted renderer that raises a hard write error at the exact instant the
/// parallel round has been fanned out but not yet joined.
struct RoundHardErrorRenderer {
    read_script: Vec<String>,
    read_cursor: usize,
    input_state: marmennill::ui::InputState,
    tool_call_events: usize,
    fired: bool,
    /// `is_globally_cancelled()` observed at the instant the hard error was raised.
    cancelled_when_raised: Option<bool>,
    /// Stands in for the round's in-flight delegated work: a live registry entry
    /// plus the cancellation token the worker itself would hold.
    inflight: Option<(
        marmennill::orchestrator::ActiveWorkerGuard,
        tokio_util::sync::CancellationToken,
    )>,
}

impl Renderer for RoundHardErrorRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        if matches!(event, Event::ToolCall(_)) {
            self.tool_call_events += 1;
        }
    }
    fn set_subagents(&mut self, _subagents: Vec<marmennill::ui::SubagentDetail>) {}
    fn flush(&mut self) -> anyhow::Result<()> {
        // The fan-out loop emits one `ToolCall` event per parallel call and then
        // flushes once every handle has been spawned (`src/ui/session.rs:506`).
        if self.tool_call_events >= 2 && !self.fired {
            self.fired = true;
            self.cancelled_when_raised = Some(marmennill::orchestrator::is_globally_cancelled());
            let token = marmennill::orchestrator::global_cancellation_token().child_token();
            let guard = marmennill::orchestrator::register_active_worker_with_token(
                Some("t-h2leak".to_string()),
                "coder".to_string(),
                "in-flight round work".to_string(),
                Some(token.clone()),
            );
            self.inflight = Some((guard, token));
            return Err(anyhow::anyhow!("simulated renderer write failure (EPIPE)"));
        }
        Ok(())
    }
    fn read_input(&mut self) -> Option<String> {
        let line = self.read_script.get(self.read_cursor).cloned();
        self.read_cursor += 1;
        line
    }
    fn input_state(&mut self) -> &mut marmennill::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &marmennill::ui::InputState {
        &self.input_state
    }
    fn shutdown(&mut self) {}
}

#[tokio::test]
async fn test_ui_session_hard_error_mid_round_cancels_inflight_work() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(two_parallel_tool_calls_sse()))
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let mut renderer = RoundHardErrorRenderer {
        read_script: vec!["goal: fan out two read calls".to_string()],
        read_cursor: 0,
        input_state: marmennill::ui::InputState::default(),
        tool_call_events: 0,
        fired: false,
        cancelled_when_raised: None,
        inflight: None,
    };

    let result = marmennill::ui::run_session(&cfg, &mut renderer, None, Some(manager)).await;

    // The hard error must still surface (we are not swallowing it).
    assert!(
        result.is_err(),
        "a renderer write failure inside the round must propagate as a hard error"
    );

    let (_, token) = renderer
        .inflight
        .as_ref()
        .expect("the round must have been fanned out before the hard error");

    assert_eq!(
        renderer.cancelled_when_raised,
        Some(false),
        "precondition: nothing was cancelled at the instant the round hit the hard error"
    );

    // H2: the round's in-flight work must be cancelled, not detached.
    assert!(
        token.is_cancelled(),
        "H2 leak: the hard error returned out of run_session while the round still \
         had in-flight work registered in the worker registry; that work must be \
         cancelled (cancel_all -> cancel_all_active_workers), otherwise delegated \
         specialists keep writing files/running commands after the session aborted"
    );
    assert!(
        marmennill::orchestrator::is_globally_cancelled(),
        "H2 leak: the global cancellation token must be cancelled on the hard-error \
         path so in-flight PTY command loops tear down their process groups \
         (src/harness/pty.rs polls is_current_or_global_cancelled -> kill_process_group)"
    );
}

// ---------------------------------------------------------------------------
// H5 — execution bounds (recon H5: "no failure budget, no wall-clock bound")
//
// The live session loop used to be bounded by nothing but a turn counter
// (`MAX_TURNS`): a turn that hung, or a task that failed the same way forever,
// kept the session alive and kept in-flight specialist work running. These two
// tests drive the *real* loop (no openpty, wiremock backend only) with small
// injected bounds to prove the wall-clock bound and the repeated-failure budget
// actually fire.
// ---------------------------------------------------------------------------

/// Renderer standing in for a session that has in-flight delegated work.
///
/// * `init()` registers a live worker plus the cancellation token that worker
///   would poll — the same shape `src/orchestrator/workers.rs` gives a real
///   delegated specialist.
/// * `flush()` records the cancellation state at the first flush after the
///   bound has been rendered, i.e. the moment the loop tears the turn down.
struct BoundProbeRenderer {
    read_script: Vec<String>,
    read_cursor: usize,
    input_state: marmennill::ui::InputState,
    events: Vec<Event>,
    inflight: Option<(
        marmennill::orchestrator::ActiveWorkerGuard,
        tokio_util::sync::CancellationToken,
    )>,
    /// `(globally_cancelled, worker_token_cancelled)` at the bound flush.
    observed: Option<(bool, bool)>,
}

impl BoundProbeRenderer {
    fn new(read_script: Vec<String>) -> Self {
        Self {
            read_script,
            read_cursor: 0,
            input_state: marmennill::ui::InputState::default(),
            events: Vec::new(),
            inflight: None,
            observed: None,
        }
    }
}

impl Renderer for BoundProbeRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        let token = marmennill::orchestrator::global_cancellation_token().child_token();
        let guard = marmennill::orchestrator::register_active_worker_with_token(
            Some("t-h5bound".to_string()),
            "coder".to_string(),
            "in-flight specialist work".to_string(),
            Some(token.clone()),
        );
        self.inflight = Some((guard, token));
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        self.events.push(event.clone());
    }
    fn set_subagents(&mut self, _subagents: Vec<marmennill::ui::SubagentDetail>) {}
    fn flush(&mut self) -> anyhow::Result<()> {
        let bound_reported = self.events.iter().any(
            |e| matches!(e, Event::Message(text) if text.contains("in-flight work cancelled")),
        );
        if bound_reported && self.observed.is_none() {
            let worker_cancelled = self
                .inflight
                .as_ref()
                .map(|(_, token)| token.is_cancelled())
                .unwrap_or(false);
            self.observed = Some((
                marmennill::orchestrator::is_globally_cancelled(),
                worker_cancelled,
            ));
        }
        Ok(())
    }
    fn read_input(&mut self) -> Option<String> {
        let line = self.read_script.get(self.read_cursor).cloned();
        self.read_cursor += 1;
        line
    }
    fn input_state(&mut self) -> &mut marmennill::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &marmennill::ui::InputState {
        &self.input_state
    }
    fn request_user_exit(&mut self) {
        self.input_state.user_exit = true;
        self.input_state.aborted = true;
    }
    fn shutdown(&mut self) {}
}

/// Recon H5 / REQ-LOOP-002: a turn that blows its wall-clock bound is terminated
/// with a user-visible reason **and** its in-flight work is cancelled.
#[tokio::test]
async fn test_ui_session_wall_clock_bound_terminates_runaway_turn() {
    let _lock = TEST_MUTEX.lock().await;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // A backend that answers, but only after the injected hard cap has expired.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(completion_sse("still thinking, and thinking, and ..."))
                .set_delay(Duration::from_millis(1_500)),
        )
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let mut renderer = BoundProbeRenderer::new(vec!["goal: keep going forever".to_string()]);

    // Idle bound deliberately far above the hard cap, so the *absolute* per-turn
    // bound is what fires.
    let bounds = marmennill::ui::session::SessionBounds {
        turn_idle: Duration::from_secs(30),
        turn_hard_cap: Duration::from_millis(300),
        failure_threshold: 2,
    };

    let started = Instant::now();
    let result = marmennill::ui::session::run_session_with_bounds(
        &cfg,
        &mut renderer,
        None,
        Some(manager),
        bounds,
    )
    .await;
    let took = started.elapsed();

    assert!(
        result.is_ok(),
        "a wall-clock bound is an orderly stop, not a hard error: {result:?}"
    );

    // (1) The reason must be user-visible, and must say it was a bound that also
    //     cancelled in-flight work — not a silent break out of the loop.
    let bound_lines: Vec<String> = renderer
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Message(text)
                if text.contains("wall-clock bound") || text.contains("turn watchdog") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        !bound_lines.is_empty(),
        "the turn bound must be reported to the user; events: {:?}",
        renderer.events.len()
    );
    assert!(
        bound_lines
            .iter()
            .any(|text| text.contains("in-flight work cancelled")),
        "the reported reason must state that in-flight work was cancelled: {bound_lines:?}"
    );

    // (2) In-flight work must actually have been cancelled at that instant.
    let (global_cancelled, worker_cancelled) = renderer
        .observed
        .expect("the bound must be reported to the user before the session ends");
    assert!(
        worker_cancelled,
        "H5 leak: the bounded turn ended while a registered specialist worker was \
         still active and uncancelled"
    );
    assert!(
        global_cancelled,
        "H5 leak: the global cancellation token must be cancelled so PTY command \
         loops tear down their process groups"
    );
    let (_, token) = renderer
        .inflight
        .as_ref()
        .expect("the probe worker must have been registered");
    assert!(
        token.is_cancelled(),
        "the worker token must stay cancelled after the session ends"
    );

    // (3) It was the bound that ended the turn, not the backend answering.
    assert!(
        took < Duration::from_millis(1_400),
        "the per-turn hard cap must cut the turn off before the 1.5 s backend \
         response arrives (session took {took:?})"
    );
}

/// Renderer for the repeated-failure budget: it records what the loop reported
/// and asks for an orderly exit once the loop refuses to retry.
struct FailureBudgetRenderer {
    read_script: Vec<String>,
    read_cursor: usize,
    input_state: marmennill::ui::InputState,
    events: Vec<Event>,
}

impl FailureBudgetRenderer {
    fn new(read_script: Vec<String>) -> Self {
        Self {
            read_script,
            read_cursor: 0,
            input_state: marmennill::ui::InputState::default(),
            events: Vec::new(),
        }
    }
    fn tool_results(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::ToolResult(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
    fn statuses(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Status(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Renderer for FailureBudgetRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        if let Event::ToolResult(text) = event
            && text.contains("failure budget exhausted")
        {
            // The escalation worked: stop the session instead of watching the
            // model retry the same failing call for `MAX_TURNS` turns.
            self.request_user_exit();
        }
        self.events.push(event.clone());
    }
    fn set_subagents(&mut self, _subagents: Vec<marmennill::ui::SubagentDetail>) {}
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_input(&mut self) -> Option<String> {
        let line = self.read_script.get(self.read_cursor).cloned();
        self.read_cursor += 1;
        line
    }
    fn input_state(&mut self) -> &mut marmennill::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &marmennill::ui::InputState {
        &self.input_state
    }
    fn request_user_exit(&mut self) {
        self.input_state.user_exit = true;
        self.input_state.aborted = true;
    }
    fn shutdown(&mut self) {}
}

/// Recon H5: the same failing task/tool call must escalate after a small
/// threshold instead of being retried blindly for the whole session.
#[tokio::test]
async fn test_ui_session_repeated_failure_escalates_instead_of_retrying_forever() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Every backend turn returns the *identical* failing call: a read of a file
    // that does not exist. Stable arguments => stable failure identity.
    let failing_args = "{\"path\": \"definitely-missing-9f3a.txt\"}";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(tool_call_sse(
            "call_fail",
            marmennill::tool_names::TOOL_READ_FILE,
            failing_args,
        )))
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let mut renderer = FailureBudgetRenderer::new(vec!["goal: read the missing file".to_string()]);

    let bounds = marmennill::ui::session::SessionBounds {
        turn_idle: std::time::Duration::from_secs(30),
        turn_hard_cap: std::time::Duration::from_secs(300),
        failure_threshold: 2,
    };

    let result = marmennill::ui::session::run_session_with_bounds(
        &cfg,
        &mut renderer,
        None,
        Some(manager),
        bounds,
    )
    .await;
    assert!(result.is_ok(), "session should stop orderly: {result:?}");

    let results = renderer.tool_results();
    let real_failures: Vec<&String> = results
        .iter()
        .filter(|text| !text.contains("failure budget exhausted"))
        .collect();
    let refusals: Vec<&String> = results
        .iter()
        .filter(|text| text.contains("failure budget exhausted"))
        .collect();

    // (1) The failing call is dispatched exactly `threshold` times, never more.
    assert_eq!(
        real_failures.len(),
        2,
        "the same failing call must be attempted exactly `failure_threshold` times; \
         tool results: {results:?}"
    );

    // (2) After the threshold the loop refuses to re-dispatch it and says why.
    assert!(
        !refusals.is_empty(),
        "the failure budget must refuse a third unchanged attempt; got: {results:?}"
    );

    // (3) The escalation is surfaced to the user, not just counted internally.
    let statuses = renderer.statuses();
    let escalations: Vec<&String> = statuses
        .iter()
        .filter(|text| text.contains("repeated failure"))
        .collect();
    assert!(
        !escalations.is_empty(),
        "repeated failures must be escalated visibly; statuses: {statuses:?}"
    );

    // (4) No runaway retry loop: the session stopped after a handful of turns
    //     instead of grinding through `MAX_TURNS` (100).
    let backend_calls = server
        .received_requests()
        .await
        .expect("wiremock request log")
        .len();
    assert!(
        backend_calls <= 4,
        "the session must stop once the failure budget is spent, not keep asking \
         the backend for another attempt (backend calls: {backend_calls})"
    );
}

/// Recon H5 / REQ-LOOP-002: the *stalled* half of the wall-clock bound.
///
/// A tool call that produces no observable progress must not be able to hold the
/// turn open indefinitely: the watchdog tears the turn down, cancels everything
/// in flight (the running tool polls the same global token), and says so.
#[tokio::test]
async fn test_ui_session_stalled_turn_watchdog_cancels_inflight_tool_call() {
    let _lock = TEST_MUTEX.lock().await;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // A 60 s sleep is far longer than the injected idle bound and produces no
    // progress events, which is exactly the stalled-turn shape.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(tool_call_sse(
            "call_sleep",
            marmennill::tool_names::TOOL_SLEEP,
            "{\"seconds\": 60}",
        )))
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let mut renderer = BoundProbeRenderer::new(vec!["goal: wait around for an hour".to_string()]);

    // Idle bound far *below* the hard cap, so the stalled watchdog is what fires.
    let bounds = marmennill::ui::session::SessionBounds {
        turn_idle: Duration::from_millis(400),
        turn_hard_cap: Duration::from_secs(600),
        failure_threshold: 2,
    };

    let started = Instant::now();
    let result = marmennill::ui::session::run_session_with_bounds(
        &cfg,
        &mut renderer,
        None,
        Some(manager),
        bounds,
    )
    .await;
    let took = started.elapsed();

    assert!(
        result.is_ok(),
        "an orderly bound stop must not be a hard error: {result:?}"
    );

    let watchdog_lines: Vec<String> = renderer
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Message(text) if text.contains("turn watchdog") => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !watchdog_lines.is_empty(),
        "a turn with no observable progress must be stopped by the watchdog and \
         reported; events: {:?}",
        renderer.events.len()
    );
    assert!(
        watchdog_lines
            .iter()
            .any(|text| text.contains("in-flight work cancelled")),
        "the stalled reason must state that in-flight work was cancelled: {watchdog_lines:?}"
    );

    let (global_cancelled, worker_cancelled) = renderer
        .observed
        .expect("the stalled bound must be reported before the session ends");
    assert!(
        global_cancelled,
        "the stalled watchdog must cancel the global token so in-flight tools and \
         PTY process groups tear down"
    );
    assert!(
        worker_cancelled,
        "the stalled watchdog must cancel registered in-flight workers"
    );

    // The 60 s call was cancelled instead of being waited on.
    assert!(
        took < Duration::from_secs(10),
        "a 60 s in-flight call must be cancelled by the 400 ms idle bound, not \
         waited out (session took {took:?})"
    );
}

// ---------------------------------------------------------------------------
// t-057: a fail-closed plan verdict must reach the *user*, not only the prompt.
// ---------------------------------------------------------------------------

/// Guard for the UNKNOWN-plan surfacing path wired into
/// `src/ui/session.rs::run_session_with_bounds` (t-057).
///
/// Setup: a temporary workspace root (`harness::with_workspace_root`) with a temp
/// `.marmel/` holding an execution plan that **exists but holds no parseable
/// `- [ ] [t-xxx]` task line** — exactly the shape that used to be read as
/// "nothing pending / all complete". The body carries a `SENTINEL` line so the
/// test can prove the unparseable body was never embedded in the system prompt as
/// though it were an active plan. (A parse failure is used instead of a
/// permission failure: `chmod`-based unreadability is not reproducible for every
/// CI user, and both routes produce the same `PlanGate::Unknown` warning.)
///
/// Required behaviour asserted:
/// 1. the renderer received an `Event::Status` carrying the plan-state warning
///    **before the first assistant turn** (the wiring at the top of the session
///    loop, not the end-of-turn plan gate);
/// 2. the UI transcript journal's **first** record is that `UiRecord::Status`
///    (so it is still visible after a restart);
/// 3. the system prompt actually sent to the backend declares the plan state
///    UNKNOWN (fail-closed) and never presents the plan as active/complete.
///
/// Pty-free: scripted in-memory renderer + wiremock backend; the repository's
/// real `.marmel/` is never touched.
#[tokio::test]
async fn test_ui_session_unknown_plan_state_surfaces_status() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let tmp = tempfile::tempdir().expect("tempdir");
    let marmel_dir = tmp.path().join(".marmel");
    std::fs::create_dir_all(&marmel_dir).expect("create temp .marmel dir");
    // Non-empty plan text with zero task lines => the plan cannot be parsed.
    std::fs::write(
        marmel_dir.join("execution_plan.md"),
        "# Execution Plan\n\nAll work finished, nothing pending.\nSENTINEL-UNPARSEABLE-PLAN-BODY\n",
    )
    .expect("write unparseable plan");

    // Capture the request bodies: the system prompt is what the backend saw.
    let bodies: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let bodies = Arc::clone(&bodies);
            move |req: &wiremock::Request| {
                bodies
                    .lock()
                    .expect("bodies lock")
                    .push(String::from_utf8_lossy(&req.body).to_string());
                ResponseTemplate::new(200).set_body_string(completion_sse("noted"))
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());
    let plan = marmennill::manager::phase::Plan::at(&marmel_dir);
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(&cfg.backend_url, "test-model"),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let mut renderer = ScriptedRenderer::new(vec!["goal".to_string(), "/abort".to_string()]);

    // Scope the workspace root so nothing in the session can resolve the
    // repository's real `.marmel/` even by accident.
    marmennill::harness::with_workspace_root(tmp.path(), async {
        marmennill::ui::run_session(&cfg, &mut renderer, None, Some(manager)).await
    })
    .await
    .expect("run_session should complete");

    // (1) A renderer `Status` event carries the plan-state warning, and it is
    //     emitted at **session start** — before any assistant output. (The
    //     end-of-turn plan gate surfaces the same text; this ordering pins the
    //     prompt-load surfacing wired at the top of `run_session_with_bounds`.)
    let status_events: Vec<String> = renderer
        .events
        .iter()
        .filter_map(|event| match event {
            marmennill::ui::Event::Status(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    let warning_pos = renderer
        .events
        .iter()
        .position(|event| {
            matches!(
                event,
                marmennill::ui::Event::Status(text) if text.contains("plan state is UNKNOWN")
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "an unparseable plan must be surfaced as a renderer Status event; \
                 Status events seen: {status_events:?}"
            )
        });
    let warning = &status_events[status_events
        .iter()
        .position(|text| text.contains("plan state is UNKNOWN"))
        .expect("the plan warning counted above")];
    assert!(
        warning.contains("could not be parsed")
            && warning.contains(marmennill::manager::phase::PLAN_FILE),
        "the Status event must name the failing plan file and why it is unknown, got: {warning}"
    );
    if let Some(first_output_pos) = renderer.events.iter().position(|event| {
        matches!(
            event,
            marmennill::ui::Event::Message(_) | marmennill::ui::Event::Done
        )
    }) {
        assert!(
            warning_pos < first_output_pos,
            "the plan-state warning must be surfaced at session start, before the first \
             assistant output — not only by the end-of-turn plan gate (warning at event \
             index {warning_pos}, first assistant output at index {first_output_pos})"
        );
    }

    // (2) The same warning is journaled as the **first** UI-transcript `Status`
    //     record, so it is still visible after a restart.
    let transcript = marmennill::ui::UiTranscript::load(plan.ui_transcript_path())
        .expect("UI transcript should be readable");
    let status_records: Vec<&String> = transcript
        .records()
        .iter()
        .filter_map(|record| match record {
            marmennill::ui::UiRecord::Status { text } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            transcript.records().first(),
            Some(marmennill::ui::UiRecord::Status { text })
                if text.contains("plan state is UNKNOWN")
        ),
        "the plan-state warning must be journaled as the first UI-transcript Status \
         record (session start, not only at the end of the first turn); \
         Status records: {status_records:?}"
    );

    // (3) Every prompt sent to the backend is fail-closed about the plan.
    let bodies = bodies.lock().expect("bodies lock");
    assert!(
        !bodies.is_empty(),
        "the session must have issued at least one chat completion request"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("Execution Plan State: UNKNOWN")),
        "the system prompt must state the plan state is UNKNOWN instead of leaving \
         the plan out (which reads as 'no work pending / all complete')"
    );
    for body in bodies.iter() {
        assert!(
            !body.contains("SENTINEL-UNPARSEABLE-PLAN-BODY"),
            "an unparseable plan body must never be replayed to the model as an active plan"
        );
        assert!(
            !body.contains("## Active Execution Plan"),
            "the prompt must not present an unparseable plan as an active execution plan"
        );
    }
}

// ---------------------------------------------------------------------------
// t-063 (manager gate item B): the assistant `tool_calls` <-> `role:"tool"` pair
// invariant must hold on the **live** abort/bound path, not only inside
// `compact()`.
//
// The repair used to be reachable only through `compact()`, which (a) is gated at
// > 90% utilization and (b) sits *after* the `bound_stop_reason` /
// `user_exit_requested` breaks that end a turn. A round cut off in the middle
// therefore left the transcript holding the assistant with **all** its
// `tool_calls` and no `role:"tool"` result for the skipped tail — and that
// transcript is what the next turn re-sends verbatim (`src/ui/session.rs` builds
// the request from `ctx.messages()`), which is a provider 400.
//
// These tests drive the *real* session loop: scripted in-memory renderer,
// wiremock backend, `harness::with_workspace_root` — pty-free, and the
// repository's real `.marmel/` is never reachable.
// ---------------------------------------------------------------------------

/// SSE body whose assistant turn carries **three** tool calls: a long `sleep`
/// followed by two reads. The mixed names force the *sequential* dispatch branch
/// of the round, and the slow first call is what lets an execution bound or a
/// user exit cut the tail of the round off after the assistant carrying all three
/// calls was already appended to the transcript.
fn three_tool_calls_sse(sleep_secs: u64) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-t063",
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_slow",
                            "type": "function",
                            "function": {
                                "name": marmennill::tool_names::TOOL_SLEEP,
                                "arguments": format!("{{\"seconds\": {sleep_secs}}}")
                            }
                        },
                        {
                            "index": 1,
                            "id": "call_b",
                            "type": "function",
                            "function": {
                                "name": marmennill::tool_names::TOOL_READ_FILE,
                                "arguments": "{\"path\": \"notes.txt\"}"
                            }
                        },
                        {
                            "index": 2,
                            "id": "call_c",
                            "type": "function",
                            "function": {
                                "name": marmennill::tool_names::TOOL_READ_FILE,
                                "arguments": "{\"path\": \"AGENTS.md\"}"
                            }
                        }
                    ]
                },
                "finish_reason": null
            }]
        })
    )
}

/// The `id` of every `tool_calls` entry in a message array (a captured chat
/// request's `messages`, or a saved transcript file — both serialize with a
/// `role` tag).
fn tool_call_ids(messages: &serde_json::Value) -> Vec<String> {
    messages
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m.get("tool_calls").and_then(|c| c.as_array()).cloned())
        .flatten()
        .filter_map(|c| c.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .collect()
}

/// The `tool_call_id` of every `role:"tool"` message in a message array.
fn tool_result_ids(messages: &serde_json::Value) -> Vec<String> {
    messages
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .filter_map(|m| {
            m.get("tool_call_id")
                .and_then(|i| i.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// Every defect of the pairing invariant in a message array: a `tool_calls` entry
/// with **no** result, one with duplicate results, or a result whose id belongs to
/// no surviving `tool_calls` entry. Empty means the array is a valid
/// OpenAI-compatible chat sequence.
fn pairing_defects(messages: &serde_json::Value) -> Vec<String> {
    let calls = tool_call_ids(messages);
    let results = tool_result_ids(messages);
    let mut defects = Vec::new();
    for id in calls.iter() {
        let n = results.iter().filter(|r| *r == id).count();
        if n == 0 {
            defects.push(format!("tool_call {id} has no tool result"));
        } else if n > 1 {
            defects.push(format!("tool_call {id} has {n} tool results"));
        }
    }
    for id in results.iter() {
        if !calls.contains(id) {
            defects.push(format!("tool result {id} has no parent tool_call"));
        }
    }
    defects
}

/// The content of the `role:"tool"` message answering `id` in a message array.
fn tool_result_content(messages: &serde_json::Value, id: &str) -> Option<String> {
    messages
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .find(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("tool")
                && m.get("tool_call_id").and_then(|i| i.as_str()) == Some(id)
        })
        .and_then(|m| m.get("content").and_then(|c| c.as_str()))
        .map(str::to_string)
}

/// Recon H5 bound + t-063: when the wall-clock bound cuts a round off, the calls
/// in the skipped tail still need a result — the **next** request the session
/// builds must be a valid chat sequence, not an assistant `tool_calls` list with
/// holes in it.
#[tokio::test]
async fn test_ui_session_bound_mid_round_repairs_pairing_for_next_request() {
    let _lock = TEST_MUTEX.lock().await;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let first_turn = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let requests = Arc::clone(&requests);
            let first_turn = Arc::clone(&first_turn);
            move |req: &wiremock::Request| {
                requests
                    .lock()
                    .expect("request capture lock")
                    .push(serde_json::from_slice(&req.body).expect("chat request body is JSON"));
                let body = if first_turn.fetch_add(1, Ordering::SeqCst) == 0 {
                    three_tool_calls_sse(30)
                } else {
                    completion_sse("Reporting the blocker instead of repeating the call.")
                };
                ResponseTemplate::new(200).set_body_string(body)
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("notes.txt"), "note\n").expect("write notes.txt");
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let transcript_path = plan.transcript_path();
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    // read #1 is the goal; read #2 is the line the user types after the bound
    // ended turn 1 — it is what makes a **second** request leave the session with
    // the torn-off transcript attached.
    let mut renderer = ScriptedRenderer::new(vec![
        "goal: run the slow call, then read the files".to_string(),
        "keep going, then report".to_string(),
        "/abort".to_string(),
    ]);

    // Idle bound far below the 30 s call, hard cap far above it: the *stalled*
    // half of the bound is what tears the round down.
    let bounds = marmennill::ui::session::SessionBounds {
        turn_idle: Duration::from_millis(300),
        turn_hard_cap: Duration::from_secs(120),
        failure_threshold: 5,
    };

    marmennill::harness::with_workspace_root(tmp.path(), async {
        marmennill::ui::session::run_session_with_bounds(
            &cfg,
            &mut renderer,
            None,
            Some(manager),
            bounds,
        )
        .await
    })
    .await
    .expect("an execution bound is an orderly stop, not a hard error");

    // (1) Precondition: the bound fired mid-round, and turn 2 really did put the
    //     torn transcript back on the wire.
    let captured = requests.lock().expect("request capture lock").clone();
    assert!(
        captured.len() >= 2,
        "a bound must end the turn, not the session: the loop has to build a second \
         request out of the transcript the round left behind (requests seen: {})",
        captured.len()
    );
    let second = captured[1]
        .get("messages")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    assert_eq!(
        tool_call_ids(&second),
        vec![
            "call_slow".to_string(),
            "call_b".to_string(),
            "call_c".to_string()
        ],
        "precondition: the assistant carrying all three tool calls of the aborted \
         round must be part of the next request"
    );

    // (2) The invariant: every one of those three calls is answered. This is the
    //     regression under test — before the fix `call_b`/`call_c` had no result
    //     at all and the provider rejects the request.
    let defects = pairing_defects(&second);
    assert!(
        defects.is_empty(),
        "every tool_call_id of the aborted round must have exactly one tool result \
         in the next request; defects: {defects:?}; tool results present: \
         {:?}; full request: {:#}",
        tool_result_ids(&second),
        captured[1]
    );

    // (3) The calls that were never dispatched get the engine's placeholder, the
    //     joined one keeps its real (bound) error.
    assert_eq!(
        tool_result_content(&second, "call_b").as_deref(),
        Some(marmennill::manager::context::ABORTED_TOOL_RESULT),
        "a tool call that was skipped by the bound must be answered with the \
         `{}` placeholder",
        marmennill::manager::context::ABORTED_TOOL_RESULT
    );
    assert_eq!(
        tool_result_content(&second, "call_c").as_deref(),
        Some(marmennill::manager::context::ABORTED_TOOL_RESULT),
        "a tool call that was skipped by the bound must be answered with the \
         `{}` placeholder",
        marmennill::manager::context::ABORTED_TOOL_RESULT
    );
    assert!(
        tool_result_content(&second, "call_slow")
            .map(|c| c.contains("turn watchdog"))
            .unwrap_or(false),
        "the call that was actually torn down must keep its bound error as its result"
    );

    // (4) The repair is persisted at the round boundary, not only applied to the
    //     in-memory copy used to build the request: the transcript file the
    //     session writes is what a rehydration would load.
    let on_disk = std::fs::read_to_string(&transcript_path).expect("transcript on disk");
    let on_disk: serde_json::Value =
        serde_json::from_str(&on_disk).expect("transcript file is valid JSON");
    let disk_defects = pairing_defects(&on_disk);
    assert!(
        disk_defects.is_empty(),
        "the transcript left on disk after an aborted round must itself satisfy the \
         pairing invariant; defects: {disk_defects:?}"
    );

    // (5) The repair is not silent: it goes through the session's status channel.
    let statuses: Vec<String> = renderer
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Status(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        statuses.iter().any(|s| s.contains("pairing repaired")),
        "a repaired tool-call pairing must be surfaced on the status line; \
         statuses: {statuses:?}"
    );
}

/// Renderer that exits the session **inside the round**: it requests the user
/// exit the instant the first `ToolCall` event is rendered, i.e. while that call
/// is still in flight. That is the deterministic shape of "the user typed
/// `/abort` mid-round" — the still-undispatched tail of the round is skipped by
/// the `renderer.aborted() || renderer.user_exit_requested()` break, and the
/// in-flight call is torn down through `cancel_all()`.
struct ExitOnFirstToolCallRenderer {
    read_script: Vec<String>,
    read_cursor: usize,
    input_state: marmennill::ui::InputState,
    events: Vec<Event>,
    exit_fired: bool,
}

impl ExitOnFirstToolCallRenderer {
    fn new(read_script: Vec<String>) -> Self {
        Self {
            read_script,
            read_cursor: 0,
            input_state: marmennill::ui::InputState::default(),
            events: Vec::new(),
            exit_fired: false,
        }
    }
}

impl Renderer for ExitOnFirstToolCallRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        self.events.push(event.clone());
        if !self.exit_fired && matches!(event, Event::ToolCall(_)) {
            self.exit_fired = true;
            self.request_user_exit();
        }
    }
    fn set_subagents(&mut self, _subagents: Vec<marmennill::ui::SubagentDetail>) {}
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_input(&mut self) -> Option<String> {
        let line = self.read_script.get(self.read_cursor).cloned();
        self.read_cursor += 1;
        line
    }
    fn input_state(&mut self) -> &mut marmennill::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &marmennill::ui::InputState {
        &self.input_state
    }
    fn shutdown(&mut self) {}
}

/// t-063: a **user exit** in the middle of a round ends the session, so there is
/// no next request to inspect — but the transcript the session leaves behind (and
/// would rehydrate) must still satisfy the pairing invariant, with a placeholder
/// for every call that was never dispatched.
#[tokio::test]
async fn test_ui_session_user_exit_mid_round_persists_paired_transcript() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let requests = Arc::clone(&requests);
            move |req: &wiremock::Request| {
                requests
                    .lock()
                    .expect("request capture lock")
                    .push(serde_json::from_slice(&req.body).expect("chat request body is JSON"));
                ResponseTemplate::new(200).set_body_string(three_tool_calls_sse(5))
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("notes.txt"), "note\n").expect("write notes.txt");
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let transcript_path = plan.transcript_path();
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    // The exit is raised by the renderer itself, at the instant the first call of
    // the round is announced — mid-round, with two calls of the round never
    // dispatched.
    let mut renderer = ExitOnFirstToolCallRenderer::new(vec![
        "goal: run the slow call, then read the files".to_string(),
    ]);

    marmennill::harness::with_workspace_root(tmp.path(), async {
        marmennill::ui::run_session(&cfg, &mut renderer, None, Some(manager)).await
    })
    .await
    .expect("a user exit is an orderly stop, not a hard error");

    let captured = requests.lock().expect("request capture lock").clone();
    assert_eq!(
        captured.len(),
        1,
        "precondition: the user exit must have ended the session inside the first \
         round (requests seen: {})",
        captured.len()
    );

    let on_disk = std::fs::read_to_string(&transcript_path).expect("transcript on disk");
    let on_disk: serde_json::Value =
        serde_json::from_str(&on_disk).expect("transcript file is valid JSON");

    assert_eq!(
        tool_call_ids(&on_disk),
        vec![
            "call_slow".to_string(),
            "call_b".to_string(),
            "call_c".to_string()
        ],
        "precondition: the transcript must still carry the assistant with all three \
         tool calls of the exited round"
    );
    let defects = pairing_defects(&on_disk);
    assert!(
        defects.is_empty(),
        "an exited round must not leave dangling tool_calls in the transcript; \
         defects: {defects:?}; results: {:?}",
        tool_result_ids(&on_disk)
    );
    let placeholders = ["call_b", "call_c"]
        .iter()
        .filter(|id| {
            tool_result_content(&on_disk, id).as_deref()
                == Some(marmennill::manager::context::ABORTED_TOOL_RESULT)
        })
        .count();
    assert_eq!(
        placeholders,
        2,
        "both never-dispatched calls must be answered with the `{}` placeholder",
        marmennill::manager::context::ABORTED_TOOL_RESULT
    );

    let statuses: Vec<String> = renderer
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Status(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        statuses.iter().any(|s| s.contains("pairing repaired")),
        "a repaired tool-call pairing must be surfaced on the status line even when \
         the session is exiting; statuses: {statuses:?}"
    );
}

/// t-063: `CompactionOutcome` must not be discarded on the live path. When the
/// 70% target is unreachable (the pinned prefix alone is over budget) the session
/// has to say so through its status channel instead of reporting
/// "context compacted".
#[tokio::test]
async fn test_ui_session_surfaces_unreachable_compaction() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(completion_sse("all good")))
        .mount(&server)
        .await;

    let mut cfg = config_for_backend(&server.uri());
    // A budget far below the size of the pinned system prompt: the target is
    // unreachable by construction, so `compact()` returns
    // `CompactionOutcome::TargetUnreachable` on every gate.
    cfg.max_context_tokens = 64;

    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let mut renderer = ScriptedRenderer::new(vec![
        "goal: tiny budget session".to_string(),
        "/abort".to_string(),
    ]);

    marmennill::harness::with_workspace_root(tmp.path(), async {
        marmennill::ui::run_session(&cfg, &mut renderer, None, Some(manager)).await
    })
    .await
    .expect("run_session should complete");

    let statuses: Vec<String> = renderer
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Status(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    let failures: Vec<&String> = statuses
        .iter()
        .filter(|s| s.contains("compaction") && s.contains("could not reach its target"))
        .collect();
    assert!(
        !failures.is_empty(),
        "a compaction that could not reach its target must be surfaced as a status \
         warning; statuses: {statuses:?}"
    );
    assert!(
        !statuses.iter().any(|s| s == "context compacted"),
        "the session must not claim a compaction it did not achieve; statuses: {statuses:?}"
    );
    assert!(
        failures[0].contains("64") || failures[0].contains("target"),
        "the warning must name the budget it failed against; got: {:?}",
        failures[0]
    );

    // The same warning is journaled, so it is visible after the fact too.
    let ui_transcript =
        marmennill::ui::UiTranscript::load(plan.ui_transcript_path()).expect("UI transcript");
    assert!(
        ui_transcript.records().iter().any(|record| matches!(
            record,
            marmennill::ui::UiRecord::Status { text }
                if text.contains("could not reach its target")
        )),
        "the failed compaction must also be journaled in the UI transcript"
    );
}

// ── t-074: the Manager turn-cap exit must be observable (recon M2 live analogue) ──

/// One `read_file` tool call on a **distinct** path per turn.
///
/// Distinct arguments are required: [`crate::harness::monitor::ToolRepetitionDetector`]
/// blocks the third *semantically identical* call, which would end the turn
/// through the repetition/ failure-budget machinery long before the turn budget
/// is reached. Varying the path keeps every one of the `MAX_TURNS` turns a
/// legitimate, successful round, so the **only** thing that can stop the loop is
/// the turn budget itself.
fn read_file_call_sse(turn: usize) -> String {
    tool_call_sse(
        &format!("call_t074_{turn:03}"),
        marmennill::tool_names::TOOL_READ_FILE,
        &format!("{{\"path\": \"notes-{turn:03}.txt\"}}"),
    )
}

/// Does this user-visible line name the turn budget?
fn names_turn_cap(text: &str) -> bool {
    let lower = text.to_lowercase();
    (lower.contains("turn budget") || lower.contains("turn cap") || lower.contains("max turns"))
        && lower.contains(&marmennill::manager::MAX_TURNS.to_string())
}

/// Residual defect 2 of the manager-cluster gate (t-067), the live analogue of
/// recon finding **M2** (`docs/recon_bugs_manager.md`): the session loop breaks
/// its turn loop when `turn_count > MAX_TURNS` **silently** — no status line, no
/// journaled record, nothing that tells the operator (or the caller) that the
/// request was cut off by the turn budget instead of finishing cleanly. The
/// operator just watches a session stop responding, with no explanation.
///
/// Required behaviour, through the SAME channel every other session exit uses
/// (renderer `Message`/`Status` event + a `Status` record in the UI transcript):
/// the exit must be reported, must name the real limit and the turn count, and
/// must leave the on-disk transcript pair-consistent (the t-063 invariant).
#[tokio::test]
async fn test_ui_session_turn_cap_surfaces_observable_stop_reason() {
    let _lock = TEST_MUTEX.lock().await;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let turn = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let requests = Arc::clone(&requests);
            let turn = Arc::clone(&turn);
            move |req: &wiremock::Request| {
                requests
                    .lock()
                    .expect("request capture lock")
                    .push(serde_json::from_slice(&req.body).expect("chat request body is JSON"));
                let n = turn.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_string(read_file_call_sse(n))
            }
        })
        .mount(&server)
        .await;

    let mut cfg = config_for_backend(&server.uri());
    // Far above anything this session produces: compaction must never fire, so
    // the turn budget is the only bound in play.
    cfg.max_context_tokens = 200_000;

    let tmp = tempfile::tempdir().expect("tempdir");
    for n in 0..=marmennill::manager::MAX_TURNS + 2 {
        std::fs::write(
            tmp.path().join(format!("notes-{n:03}.txt")),
            format!("note {n}\n"),
        )
        .expect("write notes file");
    }
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let transcript_path = plan.transcript_path();
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let mut renderer = ScriptedRenderer::new(vec![
        "goal: keep reading until the session runs out of turns".to_string(),
    ]);

    // Wall-clock bounds far above anything this session can do (a stalled turn
    // and a hard-cap teardown must never be what stops it), and a failure budget
    // that cannot be reached: every tool call below succeeds.
    let bounds = marmennill::ui::session::SessionBounds {
        turn_idle: Duration::from_secs(3_600),
        turn_hard_cap: Duration::from_secs(3_600),
        failure_threshold: 10_000,
    };

    let report = marmennill::harness::with_workspace_root(tmp.path(), async {
        marmennill::ui::session::run_session_with_bounds_report(
            &cfg,
            &mut renderer,
            None,
            Some(manager),
            bounds,
        )
        .await
    })
    .await
    .expect("hitting the turn budget is an orderly stop, not a hard error");

    // (1) Precondition: the turn budget really is what ended the loop — exactly
    //     `MAX_TURNS` successful rounds ran, and the next one was refused.
    let captured = requests.lock().expect("request capture lock").clone();
    assert_eq!(
        captured.len(),
        marmennill::manager::MAX_TURNS,
        "precondition: the session must run exactly `MAX_TURNS` backend turns and \
         refuse the next one (requests seen: {})",
        captured.len()
    );

    // (2) The caller-visible machine-readable reason: the session reports the
    //     budget exit instead of returning a bare `Ok` that looks like a clean
    //     finish (this is the "indistinguishable from plan complete" half of
    //     recon M2).
    assert!(
        report.turn_cap_reached(),
        "the caller must be able to tell a turn-cap stop from a clean finish; \
         report: {report:?}"
    );
    assert_eq!(
        report.codes(),
        vec!["turn_cap"],
        "the turn budget must be the only bound hit in this session, reported with \
         its stable machine-readable code; report: {report:?}"
    );
    let cap = report
        .stops
        .iter()
        .find_map(|stop| match stop {
            marmennill::ui::session::BoundStopReason::TurnCap { limit, turn } => {
                Some((*limit, *turn))
            }
            _ => None,
        })
        .expect("a turn-cap stop must be recorded");
    assert_eq!(
        cap,
        (
            marmennill::manager::MAX_TURNS,
            marmennill::manager::MAX_TURNS + 1
        ),
        "the recorded cap must carry the real limit and the refused turn number \
         (`{}` turns ran, turn {} refused)",
        marmennill::manager::MAX_TURNS,
        marmennill::manager::MAX_TURNS + 1
    );

    // (3) The exit must be observable: a user-visible line naming the cap.
    let cap_lines: Vec<String> = renderer
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Status(text) | Event::Message(text) => Some(text.clone()),
            _ => None,
        })
        .filter(|text| names_turn_cap(text))
        .collect();
    assert!(
        !cap_lines.is_empty(),
        "the turn-cap exit must be reported to the operator through the session's \
         status channel, naming the limit ({}) and the turn count; it must never be \
         indistinguishable from a clean end-of-turn finish. Status/Message lines that \
         mention the cap: {cap_lines:?}; distinct statuses seen: {:?}",
        marmennill::manager::MAX_TURNS,
        {
            let mut distinct: Vec<String> = renderer
                .events
                .iter()
                .filter_map(|e| match e {
                    Event::Status(text) => Some(text.clone()),
                    _ => None,
                })
                .collect();
            distinct.sort();
            distinct.dedup();
            distinct
        }
    );

    // (4) And it must be journaled, so it is attributable after the fact too —
    //     with the same machine-readable code the report carries.
    let ui_transcript =
        marmennill::ui::UiTranscript::load(plan.ui_transcript_path()).expect("UI transcript");
    let journaled: Vec<String> = ui_transcript
        .records()
        .iter()
        .filter_map(|record| match record {
            marmennill::ui::UiRecord::Status { text } => Some(text.clone()),
            _ => None,
        })
        .filter(|text| names_turn_cap(text))
        .collect();
    assert!(
        !journaled.is_empty(),
        "the turn-cap exit must also be journaled as a `Status` record in the UI \
         transcript (the same journaling the plan warnings and compaction failures \
         use); journaled cap records: {journaled:?}"
    );
    assert!(
        journaled.iter().all(|text| text.contains("turn_cap")),
        "the journaled cap line must carry the stable machine-readable code \
         (`turn_cap`) so a session summary can classify the exit without parsing \
         prose; journaled cap records: {journaled:?}"
    );

    // (5) The t-063 invariant still holds at this exit: the transcript left on
    //     disk is a valid chat sequence, every assistant `tool_calls` entry has
    //     exactly one matching tool result.
    let on_disk = std::fs::read_to_string(&transcript_path).expect("transcript on disk");
    let on_disk: serde_json::Value =
        serde_json::from_str(&on_disk).expect("transcript file is valid JSON");
    let defects = pairing_defects(&on_disk);
    assert!(
        defects.is_empty(),
        "the transcript left on disk after the turn-cap exit must satisfy the \
         tool-call pairing invariant; defects: {defects:?}"
    );
}

/// The other half of the distinction t-074 has to provide: a session that stops
/// **without** hitting a bound must not report one. Without this the
/// machine-readable reason would be meaningless (a report that always says
/// "turn cap" is as blind as a report that says nothing).
#[tokio::test]
async fn test_ui_session_clean_finish_reports_no_stop_reason() {
    let _lock = TEST_MUTEX.lock().await;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let calls = Arc::clone(&calls);
            move |_req: &wiremock::Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                // A plain reply with no tool calls: the turn ends on its own.
                ResponseTemplate::new(200).set_body_string(completion_sse(&format!("reply {n}")))
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let mut renderer = ScriptedRenderer::new(vec![
        "goal: answer once and stop".to_string(),
        "/abort".to_string(),
    ]);

    let report = marmennill::harness::with_workspace_root(tmp.path(), async {
        marmennill::ui::session::run_session_with_bounds_report(
            &cfg,
            &mut renderer,
            None,
            Some(manager),
            marmennill::ui::session::SessionBounds {
                turn_idle: Duration::from_secs(30),
                turn_hard_cap: Duration::from_secs(30),
                failure_threshold: 5,
            },
        )
        .await
    })
    .await
    .expect("a clean finish is not an error");

    assert!(
        !report.turn_cap_reached(),
        "a session that ended on its own must not report a turn-cap stop; report: {report:?}"
    );
    assert!(
        report.codes().is_empty(),
        "no execution bound fired, so no machine-readable stop reason may be recorded; \
         report: {report:?}"
    );
    // And nothing turn-cap-shaped reached the operator either.
    assert!(
        !renderer.events.iter().any(|e| match e {
            Event::Status(text) | Event::Message(text) => names_turn_cap(text),
            _ => false,
        }),
        "a clean finish must not claim a turn-cap exit; events: {:?}",
        renderer.events
    );
}
