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
    rehydrated: Vec<marmennill::types::Message>,
    rehydrated_ui: Vec<marmennill::ui::UiRecord>,
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
            rehydrated: Vec::new(),
            rehydrated_ui: Vec::new(),
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
            rehydrated: Vec::new(),
            rehydrated_ui: Vec::new(),
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
    fn rehydrate_ui(&mut self, records: &[marmennill::ui::UiRecord]) {
        self.rehydrated_ui = records.to_vec();
    }
    fn rehydrate_messages(&mut self, messages: &[marmennill::types::Message]) {
        self.rehydrated = messages.to_vec();
        let transcript = marmennill::ui::UiTranscript::from_legacy_messages(messages);
        self.rehydrated_ui = transcript.records().to_vec();
    }
    fn set_subagents(&mut self, subagents: Vec<marmennill::ui::SubagentDetail>) {
        self.subagents = subagents;
    }
    fn rehydrate_subagents(&mut self, subagents: &[marmennill::ui::SubagentDetail]) {
        self.subagents = subagents.to_vec();
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
async fn test_ui_session_rehydrates_transcript_and_resumes_plan() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let turn_counter = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let tc = turn_counter.clone();
            move |req: &wiremock::Request| {
                let n = tc.fetch_add(1, Ordering::SeqCst);
                let body = String::from_utf8_lossy(&req.body);
                if n == 0 {
                    // Turn 1 of session 1: assistant emits a tool call
                    ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call-glob-1",
                        "glob",
                        r#"{"pattern": "Cargo.toml"}"#,
                    ))
                } else if n == 1 {
                    // Turn 1 part 2 of session 1: assistant finishes turn after tool result
                    ResponseTemplate::new(200)
                        .set_body_string(completion_sse("Found Cargo.toml, proceeding."))
                } else {
                    // Resumed session (session 2):
                    // Verify that the prompt payload contains the EXECUTING phase notice and pending task t-102
                    assert!(
                        body.contains("t-102"),
                        "resumed session must contain pending task t-102 in context"
                    );
                    assert!(
                        body.contains("EXECUTING"),
                        "resumed session must contain EXECUTING phase notice"
                    );
                    assert!(
                        body.contains("Do NOT call `create_plan` again"),
                        "resumed session must prohibit calling create_plan"
                    );
                    ResponseTemplate::new(200)
                        .set_body_string(completion_sse("Resumed turn reply."))
                }
            }
        })
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    plan.create("# Execution Plan: Test Rehydration\n\n- [x] [t-101] First task\n- [ ] [t-102] Second task\n")
        .unwrap();

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

    // Run session 1: user gives initial goal, does 1 turn with tool execution, then aborts
    let mut renderer1 = ScriptedRenderer::new(vec!["/abort".to_string()]);
    marmennill::ui::run_session(
        &cfg,
        &mut renderer1,
        Some("Build feature X".to_string()),
        Some(mgr.clone()),
    )
    .await
    .expect("session 1 succeeds");

    assert!(
        plan.transcript_path().exists(),
        "session transcript should be saved to disk"
    );

    // Run session 2: restarted with no initial argument, user presses Enter to resume
    let mut renderer2 = ScriptedRenderer::new(vec!["".to_string(), "/abort".to_string()]);
    marmennill::ui::run_session(&cfg, &mut renderer2, None, Some(mgr))
        .await
        .expect("session 2 succeeds");

    // Verify that session 2 rehydrated past transcript records
    assert!(
        !renderer2.rehydrated_ui.is_empty(),
        "renderer2 should have received rehydrated records from session 1"
    );
    // Verify that the rehydrated transcript contains the tool call from session 1
    assert!(
        renderer2.rehydrated_ui.iter().any(|r| matches!(r, marmennill::ui::UiRecord::ToolResult { display } if display.contains("Cargo.toml"))),
        "rehydrated records should include tool result from session 1"
    );
    assert!(turn_counter.load(Ordering::SeqCst) >= 3);
}

#[tokio::test]
async fn test_ui_session_recovers_frozen_and_injects_deliverable() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let req_counter = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let rc = req_counter.clone();
            move |req: &wiremock::Request| {
                let n = rc.fetch_add(1, Ordering::SeqCst);
                let body = String::from_utf8_lossy(&req.body);
                if n == 0 {
                    // Specialist worker running to complete the frozen task
                    ResponseTemplate::new(200).set_body_string(completion_sse(
                        "Recovered work completed.\n\nMISSION COMPLETE (t-801)",
                    ))
                } else {
                    // Manager turn 1: verify that context received the recovered deliverable!
                    assert!(
                        body.contains("t-801"),
                        "manager turn should contain the recovered task id t-801"
                    );
                    assert!(
                        body.contains("Recovered work completed"),
                        "manager turn should contain the recovered deliverable text"
                    );
                    ResponseTemplate::new(200)
                        .set_body_string(completion_sse("Synthesis after recovery."))
                }
            }
        })
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    plan.create("# Execution Plan: Crash Recovery\n\n- [ ] [t-801] Interrupted task\n- [ ] [t-802] Next task\n")
        .unwrap();

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

    // Simulate an interrupted task by manually creating a freeze snapshot
    let frozen_req = marmennill::orchestrator::DelegationRequest {
        agent_name: marmennill::agents::Agent::Generalist,
        prompt: "Complete the interrupted task.".to_string(),
        snippets: vec![],
        task_id: Some("t-801".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let wid = mgr
        .journal
        .snapshot(marmennill::agents::Agent::Generalist, &frozen_req)
        .unwrap();
    assert!(mgr.journal.is_frozen());

    // Boot run_session — should detect frozen task, recover it, check it off, and inject deliverable
    let mut renderer = ScriptedRenderer::new(vec!["".to_string(), "/abort".to_string()]);
    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr.clone()))
        .await
        .expect("recovery session succeeds");

    // Frozen state must be cleared
    assert!(
        !mgr.journal.is_frozen(),
        "frozen checkpoint must be cleared after recovery"
    );

    // Task t-801 must be checked off in plan
    let plan_text = plan.read().unwrap().unwrap();
    assert!(
        plan_text.contains("- [x] [t-801]"),
        "task t-801 should be checked off in plan after recovery"
    );

    // Renderer must have received the recovered task ToolResult event
    assert!(
        renderer
            .events
            .iter()
            .any(|ev| matches!(ev, Event::ToolResult(r) if r.contains("[Recovered task t-801]"))),
        "renderer should have received ToolResult for recovered task"
    );

    // Verify subagent was populated in renderer with recovered deliverable
    let sa = renderer
        .subagents
        .iter()
        .find(|s| s.task_id.as_deref() == Some("t-801"))
        .expect("recovered specialist for t-801 should be present in subagents");
    assert_eq!(sa.name, "generalist-t-801");
    assert!(
        !sa.is_active,
        "recovered subagent should be marked inactive after completion"
    );
    assert!(
        sa.content.contains("MISSION COMPLETE"),
        "subagent should carry the recovered deliverable content"
    );
    assert!(
        sa.logs.iter().any(|l| l.contains("completed task t-801")),
        "logs should record completion"
    );

    let _ = wid;
}

#[tokio::test]
async fn test_ui_session_rehydrates_subagents_and_populates_agent_pane() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let turn_counter = Arc::new(AtomicUsize::new(0));

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let tc = turn_counter.clone();
            move |req: &wiremock::Request| {
                let n = tc.fetch_add(1, Ordering::SeqCst);
                let _body = String::from_utf8_lossy(&req.body);
                if n == 0 {
                    // Turn 1 of session 1: manager delegates task t-901 to coder
                    ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call-del-901",
                        "delegate_task",
                        r#"{"agent_name": "coder", "task_id": "t-901", "prompt": "build feature A"}"#,
                    ))
                } else if n == 1 {
                    // Turn 1 part 2 of session 1: manager finishes turn after delegation result
                    ResponseTemplate::new(200)
                        .set_body_string(completion_sse("Finished delegating t-901."))
                } else {
                    // Resumed turn
                    ResponseTemplate::new(200)
                        .set_body_string(completion_sse("Resumed turn reply."))
                }
            }
        })
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    plan.create("# Execution Plan\n\n- [ ] [t-901] First task\n- [ ] [t-902] Second task\n")
        .unwrap();

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

    // Run session 1: user gives initial goal, manager delegates t-901, finishes turn, then aborts
    let mut renderer1 = ScriptedRenderer::new(vec!["/abort".to_string()]);
    marmennill::ui::run_session(
        &cfg,
        &mut renderer1,
        Some("Build feature set".to_string()),
        Some(mgr.clone()),
    )
    .await
    .expect("session 1 succeeds");

    assert!(plan.transcript_path().exists());

    // Run session 2 (rehydration): restarted with no initial argument, user presses Enter to resume
    let mut renderer2 = ScriptedRenderer::new(vec!["".to_string(), "/abort".to_string()]);
    marmennill::ui::run_session(&cfg, &mut renderer2, None, Some(mgr))
        .await
        .expect("session 2 succeeds");

    // Verify that session 2 rehydrated subagent list for the Agent pane
    assert!(
        !renderer2.subagents.is_empty(),
        "renderer2 should have rehydrated subagents list for agent pane"
    );
    let sa = renderer2
        .subagents
        .iter()
        .find(|s| s.task_id.as_deref() == Some("t-901"))
        .expect("coder-t-901 should be present in rehydrated subagents");
    assert_eq!(sa.name, "coder-t-901");
    assert_eq!(sa.prompt, "build feature A");
    assert!(!sa.is_active);
    assert!(
        sa.logs.iter().any(|l| l.contains("started task t-901")),
        "logs should record task start"
    );
    assert!(
        sa.logs.iter().any(|l| l.contains("completed task t-901")),
        "logs should record task completion"
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

#[tokio::test]
async fn test_ui_session_rehydrates_without_plan_if_transcript_exists() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(completion_sse("Conversational reply.")),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    // NOTE: plan.exists() is FALSE! We do not create an execution plan file.
    assert!(!plan.exists());

    // Pre-create a transcript file (e.g. from previous conversational exchange)
    let transcript_path = plan.transcript_path();
    std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
    let past_msgs = vec![
        marmennill::types::Message::System {
            content: "system prompt".to_string(),
        },
        marmennill::types::Message::User {
            content: "What is this codebase?".to_string(),
        },
        marmennill::types::Message::Assistant {
            content: Some("It is Marmel.".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ];
    let json = serde_json::to_string(&past_msgs).unwrap();
    std::fs::write(&transcript_path, json).unwrap();

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

    let mut renderer = ScriptedRenderer::new(vec!["/abort".to_string()]);
    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr))
        .await
        .expect("session succeeds");

    // Even though plan.exists() is false, past transcript should have been rehydrated from transcript_path
    assert!(
        !renderer.rehydrated.is_empty(),
        "transcript should be rehydrated from disk even if plan.exists() is false"
    );
    assert_eq!(
        renderer.rehydrated.get(1).and_then(|m| m.content()),
        Some("What is this codebase?")
    );
    assert_eq!(
        renderer.rehydrated.get(2).and_then(|m| m.content()),
        Some("It is Marmel.")
    );
}

#[tokio::test]
async fn test_ui_session_recovered_deliverable_placed_after_rehydrated_transcript() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(completion_sse("Synthesis reply.")),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    plan.create("# Execution Plan\n\n- [ ] [t-701] Task 1\n- [ ] [t-702] Task 2\n")
        .unwrap();

    let transcript_path = plan.transcript_path();
    let past_msgs = vec![
        marmennill::types::Message::System {
            content: "system prompt".to_string(),
        },
        marmennill::types::Message::User {
            content: "Initial user goal".to_string(),
        },
    ];
    let json = serde_json::to_string(&past_msgs).unwrap();
    std::fs::write(&transcript_path, json).unwrap();

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

    // Manually snapshot a frozen task to simulate crash recovery
    let frozen_req = marmennill::orchestrator::DelegationRequest {
        agent_name: marmennill::agents::Agent::Generalist,
        prompt: "Run task".to_string(),
        snippets: vec![],
        task_id: Some("t-701".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    mgr.journal
        .snapshot(marmennill::agents::Agent::Generalist, &frozen_req)
        .unwrap();

    let mut renderer = ScriptedRenderer::new(vec!["/abort".to_string()]);
    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr))
        .await
        .expect("session succeeds");

    // Verify transcript was rehydrated
    assert!(!renderer.rehydrated.is_empty());
    assert_eq!(
        renderer.rehydrated.get(1).and_then(|m| m.content()),
        Some("Initial user goal")
    );

    // Verify ToolResult for recovered deliverable was emitted in events
    assert!(renderer.events.iter().any(|e| match e {
        Event::ToolResult(text) => text.contains("Recovered task t-701"),
        _ => false,
    }));
}

#[tokio::test]
async fn test_ui_session_saves_and_rehydrates_ui_transcript() {
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

    // Session 1: Run with initial goal "First goal", then immediately "/abort" after the turn.
    {
        let mgr = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
            marmennill::llm::ChatClient::from_config(&cfg),
            plan.clone(),
            Arc::new(marmennill::harness::HarnessStats::new()),
        ));
        let mut renderer =
            ScriptedRenderer::new(vec!["First goal".to_string(), "/abort".to_string()]);
        marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr))
            .await
            .expect("session 1 succeeds");

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

    // Session 2: Resume/rehydrate session from disk on the same directory
    {
        let mgr = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
            marmennill::llm::ChatClient::from_config(&cfg),
            plan.clone(),
            Arc::new(marmennill::harness::HarnessStats::new()),
        ));
        let mut renderer = ScriptedRenderer::new(vec!["/abort".to_string()]);
        marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr))
            .await
            .expect("session 2 succeeds");

        // Verified that rehydrate_ui was called directly with the saved records!
        assert_eq!(renderer.rehydrated_ui.len(), 2);
        assert_eq!(
            renderer.rehydrated_ui[0],
            UiRecord::User {
                text: "First goal".to_string()
            }
        );
        assert_eq!(
            renderer.rehydrated_ui[1],
            UiRecord::Assistant {
                content: Some("First assistant reply.".to_string()),
                thinking: None,
            }
        );
        // And legacy rehydration was NOT called (rehydrated remains empty)
        assert!(
            renderer.rehydrated.is_empty(),
            "clean ui_transcript rehydration bypasses legacy conversion"
        );
    }
}

#[tokio::test]
async fn test_ui_session_migrates_legacy_transcript_to_ui_transcript() {
    let _lock = TEST_MUTEX.lock().await;
    use marmennill::ui::{UiRecord, UiTranscript};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(completion_sse("Another reply.")))
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

    // Pre-create ONLY legacy .session_transcript.json (no .ui_transcript.json)
    let transcript_path = plan.transcript_path();
    std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
    let past_msgs = vec![
        marmennill::types::Message::System {
            content: "system prompt".to_string(),
        },
        marmennill::types::Message::User {
            content: "Legacy user message".to_string(),
        },
        marmennill::types::Message::Assistant {
            content: Some("Legacy assistant reply".to_string()),
            reasoning_content: Some("Legacy thinking".to_string()),
            tool_calls: vec![],
        },
    ];
    let json = serde_json::to_string(&past_msgs).unwrap();
    std::fs::write(&transcript_path, json).unwrap();
    assert!(!plan.ui_transcript_path().exists());

    let mgr = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::from_config(&cfg),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));
    let mut renderer = ScriptedRenderer::new(vec!["/abort".to_string()]);
    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr))
        .await
        .expect("session succeeds");

    // Check that .ui_transcript.json was migrated and saved
    assert!(
        plan.ui_transcript_path().exists(),
        "must migrate and create ui_transcript.json"
    );
    let migrated =
        UiTranscript::load(plan.ui_transcript_path()).expect("load migrated ui_transcript");
    assert_eq!(migrated.records().len(), 2);
    assert_eq!(
        migrated.records()[0],
        UiRecord::User {
            text: "Legacy user message".to_string()
        }
    );
    assert_eq!(
        migrated.records()[1],
        UiRecord::Assistant {
            content: Some("Legacy assistant reply".to_string()),
            thinking: Some("Legacy thinking".to_string()),
        }
    );

    // Check that renderer was rehydrated with the migrated records
    assert_eq!(renderer.rehydrated_ui.len(), 2);
    assert_eq!(
        renderer.rehydrated_ui[0],
        UiRecord::User {
            text: "Legacy user message".to_string()
        }
    );
}

#[tokio::test]
async fn test_ui_session_rehydrates_steering_history_from_ui_transcript() {
    let _lock = TEST_MUTEX.lock().await;
    use marmennill::ui::{UiRecord, UiTranscript};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(completion_sse("Assistant ready.")),
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

    // Pre-populate UI transcript with user steering and arbitrator response
    let mut initial_transcript = UiTranscript::new();
    initial_transcript.append(UiRecord::User {
        text: "Hur går det för codern?".to_string(),
    });
    initial_transcript.append(UiRecord::SteerResponse {
        text: "\n[Arbitrator]: Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar.\n"
            .to_string(),
    });
    initial_transcript.save(plan.ui_transcript_path()).unwrap();

    let mgr = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::from_config(&cfg),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));
    let mut renderer = ScriptedRenderer::new(vec!["/abort".to_string()]);
    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr))
        .await
        .expect("session rehydrates and succeeds");

    // Verify steering history in orchestrator bus was restored from the transcript
    let restored_hist = marmennill::orchestrator::get_steering_history()
        .expect("steering history must be registered in orchestrator bus");
    let hist = restored_hist.read().unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].0, "Hur går det för codern?");
    assert_eq!(
        hist[0].1,
        "Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar."
    );
}

/// Verify that during Deep-Freeze recovery of an interrupted task, user inquiries
/// submitted via poll_input are arbitrated live by the Steer Arbitrator and streamed
/// as SteerResponse events to the UI.
#[tokio::test]
async fn test_ui_session_recovery_arbitrates_user_input_and_streams_steer_response() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|req: &wiremock::Request| {
            let body_str = String::from_utf8_lossy(&req.body);
            if body_str.contains("Steer Arbitrator") || body_str.contains("Arbitrate the user") {
                let body = completion_sse(
                    r#"{"decision": "RespondDirectly", "response": "Återställning pågår för t-001."}"#,
                );
                ResponseTemplate::new(200).set_body_string(body)
            } else {
                let body = completion_sse("Mock specialist recovery response.\n\nMISSION COMPLETE (t-001)");
                ResponseTemplate::new(200).set_body_string(body)
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());
    let tmp = tempfile::tempdir().unwrap();
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    plan.create("# Plan\n- [ ] [t-001] frozen task\n")
        .expect("plan created");

    let mgr = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::from_config(&cfg),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    // Freeze a delegation in the journal
    let sub_req = marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Generalist,
        prompt: "Perform recovery work".to_string(),
        snippets: vec![],
        task_id: Some("t-001".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    mgr.journal
        .snapshot(marmennill::agents::Agent::Generalist, &sub_req)
        .expect("snapshot frozen");
    assert!(mgr.journal.is_frozen());

    // Scripted input: midflight poll_input during recovery asks a status question,
    // then /abort at the resume prompt.
    let mut renderer = ScriptedRenderer::with_poll(
        vec!["/abort".to_string()],
        vec!["Hur går det med återställningen?".to_string()],
    );

    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr.clone()))
        .await
        .expect("session recovers and terminates on abort");

    // The journal should be cleared after recovery
    assert!(!mgr.journal.is_frozen());

    // Verify that SteerResponse was emitted to the renderer
    let had_steer_response = renderer.events.iter().any(|ev| match ev {
        marmennill::ui::Event::SteerResponse(text) => text.contains("Återställning pågår"),
        _ => false,
    });
    assert!(
        had_steer_response,
        "Steering arbitrator response must be streamed to renderer during recovery"
    );
}

/// Verify that when multiple tasks are frozen in .session_frozen.json,
/// recover_frozen loops through and recovers all of them sequentially.
#[tokio::test]
async fn test_ui_session_recovers_multiple_frozen_tasks_sequentially() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|_req: &wiremock::Request| {
            let body = completion_sse("Mock task completed.\n\nMISSION COMPLETE");
            ResponseTemplate::new(200).set_body_string(body)
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());
    let tmp = tempfile::tempdir().unwrap();
    let plan = marmennill::manager::phase::Plan::at(tmp.path());
    plan.create("# Plan\n- [ ] [t-001] first\n- [ ] [t-002] second\n")
        .expect("plan created");

    let mgr = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::from_config(&cfg),
        plan.clone(),
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    // Freeze two tasks
    let req1 = marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Generalist,
        prompt: "Task 1".to_string(),
        snippets: vec![],
        task_id: Some("t-001".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let req2 = marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Coder,
        prompt: "Task 2".to_string(),
        snippets: vec![],
        task_id: Some("t-002".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    mgr.journal
        .snapshot(marmennill::agents::Agent::Generalist, &req1)
        .unwrap();
    mgr.journal
        .snapshot(marmennill::agents::Agent::Coder, &req2)
        .unwrap();
    assert_eq!(mgr.journal.frozen_all().unwrap().len(), 2);

    let mut renderer = ScriptedRenderer::new(vec!["/abort".to_string()]);

    marmennill::ui::run_session(&cfg, &mut renderer, None, Some(mgr.clone()))
        .await
        .expect("session recovers all tasks");

    // All frozen checkpoints must be released
    assert!(!mgr.journal.is_frozen());
    assert_eq!(mgr.journal.frozen_all().unwrap().len(), 0);
}
