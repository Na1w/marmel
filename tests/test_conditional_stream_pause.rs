//! Conditional stream pause on steering: the Steer Arbitrator only borrows a
//! model slot from a running specialist stream when it actually has to.
//!
//! The rule pinned here, end to end through the public preemption surface:
//!
//! * the arbitrator's own `backend + model` pair still has a free session slot
//!   and nobody is queued => **no pause at all**: no pause signal reaches the
//!   stream, the stream keeps producing chunks, the arbitration round still
//!   completes, and a `Cancel` / abort decision still cancels that stream
//!   ([`PreemptHandle::Running`]);
//! * the pair is full, or somebody is already queued for it, or **no cap is
//!   configured at all** (unlimited, including a rejected `0`) => the original
//!   pause handshake, with the stream resumed on Assistant Prefill and its
//!   partial output carried into the continuation request;
//! * a stream on a *different* model is never a preemption target either way.
//!
//! Determinism notes: the pool registry, the active config, the preemption
//! registry and the status sink are process-global statics, so every test here
//! takes `SERIAL` **and** uses its own backend URL and its own model name —
//! pool entries are keyed by `(backend, model)` and live for the process
//! lifetime, so unique keys (not cleanup) keep the tests from polluting each
//! other. Chunk timing comes from a scripted loopback SSE backend and a
//! test-held gate, so nothing here races on wall-clock sleeps: the only bounds
//! used are production ones (the preemption ack timeout is far larger than the
//! scripted inter-chunk gap).

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use marmennill::config::{ConcurrencyConfig, Config, set_active};
use marmennill::harness::HarnessStats;
use marmennill::harness::monitor::RepetitionDetector;
use marmennill::llm::{
    ChatClient, PauseAction, ResumableStreamOutput, StreamControl, StreamEvent, StreamSink,
    chat_stream_resumable,
};
use marmennill::orchestrator::pool;
use marmennill::orchestrator::preemption::{
    PreemptHandle, PreemptibleStreamSink, preempt_conflicting_stream,
    preempt_conflicting_stream_on_backend,
};
use marmennill::orchestrator::set_status_sender;
use marmennill::orchestrator::steer::{
    SteerContext, SteerDecision, SteerSubtaskDecision, arbitrate_steer_context_stream,
};
use marmennill::types::{ChatRequest, Message};

/// Serialize this file: pools, the active config, the preemption registry and
/// the status sink are process-global. Unique backend URLs + model names per
/// test are what keep the *pool* state disjoint even though entries are never
/// evicted.
static SERIAL: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

/// Status lines the pause path emits (and the skip path must not).
const PAUSE_MARK: &str = "Yielded model slot";
const RESUME_MARK: &str = "Model slot reclaimed";

/// Marker embedded in the arbitrator's user message so the scripted backend can
/// tell a steering call apart from a specialist turn.
const STEER_MARKER: &str = "steer-marker-conditional-pause";

/// Inter-chunk gap of the scripted backend. Far below the production
/// preemption ack bound, so the pause is always observed on the next chunk.
const CHUNK_GAP: Duration = Duration::from_millis(150);

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// Install a fresh active config whose global cap is `max_sessions` per
/// `backend + model` pair. `0` is a *rejected* cap (fails open to unlimited).
fn activate(max_sessions: usize) {
    let mut cfg = Config::default();
    cfg.orchestration.concurrency = ConcurrencyConfig {
        by_backend: std::collections::HashMap::new(),
        max_sessions: Some(max_sessions),
    };
    set_active(cfg);
}

/// Cap **one** backend with a per-URL override, leaving every other backend
/// unlimited (the global key stays unset).
fn activate_override(backend_url: &str, max_sessions: usize) {
    let mut cfg = Config::default();
    cfg.orchestration.concurrency = ConcurrencyConfig {
        by_backend: std::collections::HashMap::from([(backend_url.to_string(), max_sessions)]),
        max_sessions: None,
    };
    set_active(cfg);
}

/// Back to "no cap configured" — the unlimited case.
fn deactivate() {
    set_active(Config::default());
}

/// Capture every status line emitted while the guard is alive.
fn capture_status() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    set_status_sender(tx);
    rx
}

fn drain_status(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }
    lines
}

fn mentions(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|line| line.contains(needle))
}

/// Poll `cond` until it holds or `deadline` passes — polling keeps timing
/// assertions off the critical path.
async fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if started.elapsed() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn chat_request(model: &str, prompt: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message::User {
            content: prompt.to_string(),
        }],
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
        stream: Some(true),
        enable_thinking: None,
        reasoning_effort: None,
        tools: None,
    }
}

/// Register a specialist stream in the preemption registry with full identity.
fn register_stream(
    agent_tag: &str,
    task_id: &str,
    token: &CancellationToken,
    model: &str,
) -> PreemptibleStreamSink {
    PreemptibleStreamSink::register_full(
        agent_tag,
        Some(agent_tag.split('-').next().unwrap_or(agent_tag).to_string()),
        Some(task_id.to_string()),
        Some(token.clone()),
        model,
    )
}

/// A sink that **delegates** every call to the real `PreemptibleStreamSink`
/// (so the production handshake is what runs) while recording what the stream
/// loop actually observed.
struct RecordingSink {
    inner: PreemptibleStreamSink,
    controls: Vec<StreamControl>,
    content: String,
    pause_polls: usize,
    pause_callbacks: usize,
    deltas: Arc<AtomicUsize>,
}

impl RecordingSink {
    fn new(inner: PreemptibleStreamSink, deltas: Arc<AtomicUsize>) -> Self {
        Self {
            inner,
            controls: Vec::new(),
            content: String::new(),
            pause_polls: 0,
            pause_callbacks: 0,
            deltas,
        }
    }
}

#[async_trait::async_trait]
impl StreamSink for RecordingSink {
    fn emit(&mut self, event: StreamEvent) {
        if let StreamEvent::Content(text) = &event {
            self.content.push_str(text);
            self.deltas.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.emit(event);
    }

    fn poll_control(&mut self) -> StreamControl {
        let control = self.inner.poll_control();
        if matches!(control, StreamControl::Pause { .. }) {
            self.pause_polls += 1;
        }
        self.controls.push(control.clone());
        control
    }

    async fn on_pause(&mut self, user_input: &str) -> PauseAction {
        self.pause_callbacks += 1;
        self.inner.on_pause(user_input).await
    }
}

type StreamTask = JoinHandle<(RecordingSink, anyhow::Result<ResumableStreamOutput>)>;

/// Drive one specialist turn through the real resumable stream loop against
/// `client`, with the sink registered before the task starts (so a concurrent
/// preemption round can never race the registration).
fn spawn_stream(
    client: ChatClient,
    model: &str,
    mut sink: RecordingSink,
    token: CancellationToken,
) -> StreamTask {
    let req = chat_request(model, "write the code");
    let mut detector = RepetitionDetector::new(3, 5);
    tokio::spawn(async move {
        let output = chat_stream_resumable(
            &client,
            &req,
            &mut sink,
            512,
            16_384,
            &mut detector,
            false,
            Some(&token),
        )
        .await;
        (sink, output)
    })
}

/// Script for the scripted loopback backend.
#[derive(Clone, Default)]
struct Script {
    /// Chunks of the **first** streamed call, before the test-held gate.
    head: Vec<String>,
    /// Chunks of the first streamed call, after the gate opens.
    tail: Vec<String>,
    /// Chunks of every later streamed call (the resume continuation).
    continuation: Vec<String>,
    /// Hold the first call open on the gate until the test releases it.
    gate_first: bool,
}

/// A minimal chunked-transfer SSE backend on loopback whose chunk timing, and
/// therefore the pause point, the test controls. Every request body is recorded
/// so assertions can read what the client really sent — including the prefill a
/// resumed stream carries over.
struct ScriptedBackend {
    url: String,
    bodies: Arc<Mutex<Vec<String>>>,
    release: mpsc::Sender<()>,
}

const SSE_HEADERS: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";

fn delta_sse(text: &str) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-cond-pause",
            "choices": [{ "delta": { "content": text }, "finish_reason": null }]
        })
    )
}

/// The arbitrator reply: the shape `arbitrate_steer_context_stream` parses into
/// `RespondDirectly`.
fn steer_decision_json() -> String {
    serde_json::json!({
        "decision": "RespondDirectly",
        "response": "Steered: keep going, nothing needs pausing."
    })
    .to_string()
}

impl ScriptedBackend {
    async fn start(script: Script, gap: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let addr = listener.local_addr().expect("loopback address");
        let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let (release_tx, release_rx) = mpsc::channel::<()>(1);
        let mut gate = script.gate_first.then_some(release_rx);
        let bodies_task = Arc::clone(&bodies);

        tokio::spawn(async move {
            let mut call = 0usize;
            while let Ok((stream, _)) = listener.accept().await {
                // Only the very first call is held open on the gate.
                let gate = if call == 0 { gate.take() } else { None };
                let bodies = Arc::clone(&bodies_task);
                tokio::spawn(serve(stream, script.clone(), call, gate, bodies, gap));
                call += 1;
            }
        });

        Self {
            url: format!("http://{addr}/v1"),
            bodies,
            release: release_tx,
        }
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn client(&self, model: &str) -> ChatClient {
        ChatClient::new_with_token(self.url.clone(), model, "test-token")
    }

    /// Bodies received by the backend, oldest first.
    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().expect("body log").clone()
    }

    /// Specialist turns only: the bodies that are not steering calls.
    fn specialist_bodies(&self) -> Vec<String> {
        self.bodies()
            .into_iter()
            .filter(|body| !body.contains(STEER_MARKER))
            .collect()
    }

    /// Open the gate that keeps the first streamed call in flight.
    async fn release(&self) {
        let _ = self.release.send(()).await;
    }
}

/// Read one HTTP request (headers + full body per `Content-Length`).
async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut acc: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await;
        let n = match read {
            Ok(Ok(n)) if n > 0 => n,
            // A peer that hangs up mid-request still gets whatever arrived, so a
            // truncated request body is observable rather than lost.
            _ => {
                return if acc.is_empty() {
                    None
                } else {
                    Some(String::from_utf8_lossy(&acc).into_owned())
                };
            }
        };
        acc.extend_from_slice(&buf[..n]);
        let Some(head_end) = acc.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&acc[..head_end]).into_owned();
        let length = head
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if acc.len() >= head_end + 4 + length {
            return Some(String::from_utf8_lossy(&acc[head_end + 4..]).into_owned());
        }
    }
}

async fn write_sse_chunk(stream: &mut TcpStream, payload: &str) -> io::Result<()> {
    let frame = format!("{:x}\r\n{}\r\n", payload.len(), payload);
    stream.write_all(frame.as_bytes()).await?;
    stream.flush().await
}

/// Answer one backend call: a steering call gets the arbitrator decision, a
/// specialist turn gets its scripted chunks, with the gate in between.
async fn serve(
    mut stream: TcpStream,
    script: Script,
    call: usize,
    mut gate: Option<mpsc::Receiver<()>>,
    bodies: Arc<Mutex<Vec<String>>>,
    gap: Duration,
) {
    let Some(body) = read_request(&mut stream).await else {
        return;
    };
    bodies.lock().expect("body log").push(body.clone());

    let is_steer = body.contains(STEER_MARKER);
    if stream.write_all(SSE_HEADERS.as_bytes()).await.is_err() {
        return;
    }

    let chunks: Vec<String> = if is_steer {
        vec![steer_decision_json()]
    } else if call == 0 {
        script.head.clone()
    } else {
        script.continuation.clone()
    };
    for (index, chunk) in chunks.iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(gap).await;
        }
        if write_sse_chunk(&mut stream, &delta_sse(chunk))
            .await
            .is_err()
        {
            return;
        }
    }

    if !is_steer && call == 0 {
        // Keep the turn genuinely in flight until the steer round is over.
        if let Some(receiver) = gate.as_mut() {
            let _ = tokio::time::timeout(Duration::from_secs(10), receiver.recv()).await;
        }
        for chunk in &script.tail {
            tokio::time::sleep(gap).await;
            if write_sse_chunk(&mut stream, &delta_sse(chunk))
                .await
                .is_err()
            {
                return;
            }
        }
    }

    if write_sse_chunk(&mut stream, "data: [DONE]\n\n")
        .await
        .is_err()
    {
        return;
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
    let _ = stream.flush().await;
}

fn steer_context() -> SteerContext<'static> {
    SteerContext {
        main_goal: "keep the suite green",
        orchestrator_status: "Active",
        pending_approval: "None",
        plan_progress: "None",
        plan_content: "None",
        available_agents: "",
        steering_history: "None",
        user_message: STEER_MARKER,
        active_subtasks: "None",
    }
}

fn subtask(tool_call_id: &str, agent_name: &str, action: &str) -> SteerSubtaskDecision {
    SteerSubtaskDecision {
        tool_call_id: tool_call_id.to_string(),
        action: action.to_string(),
        message: None,
        agent_name: Some(agent_name.to_string()),
        prompt: None,
        sleep_seconds: None,
    }
}

/// One steer round that must take the **pause** path: the stream must observe
/// `StreamControl::Pause`, hand back its action receiver, and be resumed.
async fn pause_round(backend: &str, model: &str, agent_tag: &str, task_id: &str) {
    let token = CancellationToken::new();
    let mut sink = register_stream(agent_tag, task_id, &token, model);

    let backend = backend.to_string();
    let model = model.to_string();
    let handle_task = tokio::spawn(async move {
        preempt_conflicting_stream_on_backend(&backend, &model, "steer now").await
    });

    let paused = wait_until(Duration::from_secs(5), || {
        matches!(sink.poll_control(), StreamControl::Pause { .. })
    })
    .await;
    assert!(
        paused,
        "a full, queued or uncapped pair must pause the conflicting stream"
    );

    let (action_tx, action_rx) = oneshot::channel();
    let sink_task = tokio::spawn(async move {
        let action = sink.on_pause("steer now").await;
        let _ = action_tx.send(action);
    });

    let handle = handle_task.await.expect("preemption task ran");
    assert!(
        matches!(handle, PreemptHandle::Active(_)),
        "the handle must be Active when the stream yielded its slot"
    );
    assert!(
        !token.is_cancelled(),
        "a stream that acknowledges in time must not be cancelled"
    );

    handle.complete_all(PauseAction::Resume);
    let action = tokio::time::timeout(Duration::from_secs(5), action_rx)
        .await
        .expect("the paused stream is handed its action")
        .expect("action channel open");
    assert_eq!(action, PauseAction::Resume);
    sink_task.await.expect("pause task ran");
}

// ---------------------------------------------------------------------------
// 1. spare capacity => the conflicting stream is never paused
// ---------------------------------------------------------------------------

/// Spare capacity for the arbitrator's own `backend + model` pair means the
/// arbitrator runs on its own slot: the running specialist stream receives no
/// pause signal, keeps producing chunks (the scripted gate proves it was still
/// in flight and finished on the **same** request), the arbitration round still
/// completes, and the handle leaves the stream alone when the decision does not
/// address it.
#[tokio::test]
async fn test_spare_capacity_leaves_the_conflicting_stream_running_and_streaming() {
    let _serial = SERIAL.lock().await;
    deactivate();

    activate(2);
    let model = "model-cond-spare";
    let backend = ScriptedBackend::start(
        Script {
            head: vec!["ALPHA-9c1 ".to_string(), "BETA-9c1 ".to_string()],
            tail: vec!["GAMMA-9c1".to_string()],
            continuation: vec!["OMEGA-9c1".to_string()],
            gate_first: true,
        },
        CHUNK_GAP,
    )
    .await;
    let url = backend.url().to_string();

    // The capacity question itself must be side-effect free.
    let pools_before = pool::open_pool_count();
    assert!(
        pool::has_spare_session_capacity(&url, model),
        "a capped, untouched pair has spare capacity"
    );
    assert_eq!(
        pool::open_pool_count(),
        pools_before,
        "asking about spare capacity must not open a pool entry"
    );

    let deltas = Arc::new(AtomicUsize::new(0));
    let token = CancellationToken::new();
    let sink = RecordingSink::new(
        register_stream("coder-t-901", "t-901", &token, model),
        Arc::clone(&deltas),
    );
    let stream = spawn_stream(backend.client(model), model, sink, token.clone());

    assert!(
        wait_until(Duration::from_secs(5), || deltas.load(Ordering::SeqCst)
            >= 1)
        .await,
        "the specialist stream must start producing chunks"
    );
    let snapshot = pool::pool_snapshot(&url, model).expect("a capped pair has a pool");
    assert_eq!(
        (snapshot.available, snapshot.waiting),
        (1, 0),
        "the running stream holds one of two slots, nobody is queued"
    );

    let mut status = capture_status();

    let handle = preempt_conflicting_stream_on_backend(&url, model, "steer now").await;
    assert!(
        matches!(handle, PreemptHandle::Running(ref ids) if ids.len() == 1),
        "spare capacity must leave the conflicting stream running instead of preempting it"
    );

    // The steer round itself completes on the same backend+model pair.
    let stats = HarnessStats::new();
    let decision = tokio::time::timeout(
        Duration::from_secs(10),
        arbitrate_steer_context_stream(&backend.client(model), &stats, steer_context(), |_| {}),
    )
    .await
    .expect("the arbitrator must not block behind the running stream")
    .expect("the arbitrator parsed a decision");
    assert_eq!(decision.decision, "RespondDirectly");

    // A decision that does not address the stream leaves it running.
    handle.complete_with_subtask_decision(Some(&decision));

    // Only now is the rest of the turn allowed to arrive.
    backend.release().await;
    let (sink, output) = stream.await.expect("stream task ran");
    let output = output.expect("the resumable turn completed");

    assert_eq!(
        sink.pause_polls, 0,
        "StreamControl::Pause must never reach the stream loop"
    );
    assert_eq!(
        sink.pause_callbacks, 0,
        "on_pause must never run when pausing was skipped"
    );
    assert!(
        sink.controls
            .iter()
            .all(|control| *control == StreamControl::Continue),
        "every control poll must be Continue, got {:?}",
        sink.controls
    );
    let content = &output.reply.content;
    for marker in ["ALPHA-9c1", "BETA-9c1", "GAMMA-9c1"] {
        assert!(
            content.contains(marker) && sink.content.contains(marker),
            "{marker} must still have streamed after the steer round (content {content:?})"
        );
    }
    assert!(
        !content.contains("OMEGA-9c1"),
        "no continuation request may be issued for a stream that was never paused"
    );
    assert_eq!(
        backend.specialist_bodies().len(),
        1,
        "the turn must stay on its single original request"
    );
    assert!(!output.was_aborted_by_steer);
    assert!(
        !token.is_cancelled(),
        "a stream the decision does not address keeps running"
    );

    let lines = drain_status(&mut status);
    assert!(
        !mentions(&lines, PAUSE_MARK) && !mentions(&lines, RESUME_MARK),
        "skipping the pause must emit no pause/resume status, got {lines:?}"
    );

    deactivate();
}

// ---------------------------------------------------------------------------
// 2. pool at capacity => pause, then resume with partial output carried over
// ---------------------------------------------------------------------------

/// With the pair saturated (the stream itself holds the only slot) the original
/// behaviour is preserved: the stream yields its model slot, and on resume it
/// continues on a **continuation request that carries the partial output** as
/// Assistant Prefill — the final content holds both halves.
#[tokio::test]
async fn test_saturated_pair_pauses_the_stream_and_resumes_with_partial_output_carried_over() {
    let _serial = SERIAL.lock().await;
    deactivate();

    activate(1);
    let model = "model-cond-full";
    let backend = ScriptedBackend::start(
        Script {
            head: vec!["ALPHA-4d2 ".to_string(), "BETA-4d2 ".to_string()],
            tail: vec!["NEVER-4d2".to_string()],
            continuation: vec!["OMEGA-4d2".to_string()],
            gate_first: true,
        },
        CHUNK_GAP,
    )
    .await;
    let url = backend.url().to_string();

    let deltas = Arc::new(AtomicUsize::new(0));
    let token = CancellationToken::new();
    let sink = RecordingSink::new(
        register_stream("coder-t-902", "t-902", &token, model),
        Arc::clone(&deltas),
    );
    let stream = spawn_stream(backend.client(model), model, sink, token.clone());

    assert!(
        wait_until(Duration::from_secs(5), || deltas.load(Ordering::SeqCst)
            >= 1)
        .await,
        "the specialist stream must start producing chunks"
    );
    assert_eq!(
        pool::pool_snapshot(&url, model)
            .expect("a capped pair has a pool")
            .available,
        0,
        "the running stream holds the only slot of this pair"
    );
    assert!(
        !pool::has_spare_session_capacity(&url, model),
        "a saturated pair has no spare capacity"
    );

    let mut status = capture_status();

    let handle = preempt_conflicting_stream_on_backend(&url, model, "steer now").await;
    assert!(
        matches!(handle, PreemptHandle::Active(_)),
        "a full pair must still preempt the running stream"
    );

    backend.release().await;
    handle.complete_all(PauseAction::Resume);

    let (sink, output) = stream.await.expect("stream task ran");
    let output = output.expect("the resumable turn completed");

    assert_eq!(
        sink.pause_polls, 1,
        "the stream loop must be cut exactly once by the pause"
    );
    assert_eq!(
        sink.pause_callbacks, 1,
        "the pause handshake must run exactly once"
    );
    let content = &output.reply.content;
    assert!(
        content.contains("ALPHA-4d2"),
        "the pre-pause output must survive the pause (content {content:?})"
    );
    assert!(
        content.contains("OMEGA-4d2"),
        "the stream must resume and finish on the continuation (content {content:?})"
    );
    assert!(
        !content.contains("NEVER-4d2"),
        "the yielded turn must not keep delivering chunks"
    );
    assert!(!output.was_aborted_by_steer);
    assert!(
        !token.is_cancelled(),
        "a stream that acknowledges in time is resumed, not cancelled"
    );

    let bodies = backend.specialist_bodies();
    assert_eq!(
        bodies.len(),
        2,
        "a paused turn resumes through one continuation request"
    );
    assert!(
        !bodies[0].contains("ALPHA-4d2"),
        "the first request carries only the user prompt"
    );
    assert!(
        bodies[1].contains("ALPHA-4d2"),
        "the continuation request must carry the partial output as Assistant Prefill"
    );

    let lines = drain_status(&mut status);
    assert!(
        mentions(&lines, PAUSE_MARK),
        "the pause must still be reported to the user, got {lines:?}"
    );
    assert!(
        mentions(&lines, RESUME_MARK),
        "the resume must still be reported to the user, got {lines:?}"
    );

    deactivate();
}

// ---------------------------------------------------------------------------
// 3. `max_sessions` not configured (unlimited) => still pauses
// ---------------------------------------------------------------------------

/// The regression guard: with **no cap configured** there is no bounded spare
/// slot to reason about, so the arbitrator must never take the skip path — the
/// stream is paused exactly as before the conditional-pause feature existed. The
/// same holds for a cap that is configured but rejected (`0`), and the capacity
/// probe must not open a pool entry for an uncapped pair either.
#[tokio::test]
async fn test_unlimited_capacity_still_pauses_the_conflicting_stream() {
    let _serial = SERIAL.lock().await;

    // (a) no cap at all.
    deactivate();
    let url = "http://cond-pause-uncapped:9411/v1".to_string();
    let model = "model-cond-uncapped";
    let pools_before = pool::open_pool_count();
    assert!(
        !pool::has_spare_session_capacity(&url, model),
        "no configured cap means unlimited, which is never spare capacity"
    );
    assert_eq!(
        pool::open_pool_count(),
        pools_before,
        "the probe must not open a pool entry for an uncapped pair"
    );
    pause_round(&url, model, "coder-t-903", "t-903").await;
    assert_eq!(
        pool::open_pool_count(),
        pools_before,
        "an uncapped steer round must not open a pool entry either"
    );

    // (b) a cap that is rejected as unusable (`0`) fails open to unlimited.
    activate(0);
    assert!(
        !pool::has_spare_session_capacity(&url, model),
        "a rejected cap resolves to unlimited, so it is not spare capacity either"
    );
    assert_eq!(
        pool::open_pool_count(),
        pools_before,
        "a rejected cap must not open a pool entry"
    );
    pause_round(&url, model, "coder-t-904", "t-904").await;
    assert_eq!(
        pool::open_pool_count(),
        pools_before,
        "a rejected cap must not open a pool entry through the steer round either"
    );

    deactivate();
}

// ---------------------------------------------------------------------------
// 4. a different model is never a preemption target
// ---------------------------------------------------------------------------

/// Changing the capacity rule must not widen the preemption surface: a stream on
/// a model other than the arbitrator's stays untouched — both when the pair has
/// spare capacity and when capacity is unknown (the backend-blind entry point,
/// which always takes the pause path for a *conflicting* model).
#[tokio::test]
async fn test_different_model_stream_is_never_touched() {
    let _serial = SERIAL.lock().await;

    let token = CancellationToken::new();
    let mut sink = register_stream("coder-t-905", "t-905", &token, "model-cond-other");
    let mut status = capture_status();

    // Spare capacity for the arbitrator's pair, but no conflicting stream.
    activate(2);
    let spare_backend = "http://cond-pause-other-model:9412/v1";
    assert!(pool::has_spare_session_capacity(
        spare_backend,
        "model-cond-arb"
    ));
    let handle =
        preempt_conflicting_stream_on_backend(spare_backend, "model-cond-arb", "steer now").await;
    assert!(
        matches!(handle, PreemptHandle::None),
        "a non-conflicting model must not be preempted even with spare capacity"
    );
    handle.complete_all(PauseAction::Abort);

    // Unknown backend (always-pause entry point), still no model conflict.
    let handle = preempt_conflicting_stream("model-cond-arb", "steer now").await;
    assert!(
        matches!(handle, PreemptHandle::None),
        "a non-conflicting model must not be preempted with unknown capacity"
    );
    handle.complete_all(PauseAction::Abort);

    for _ in 0..5 {
        assert_eq!(
            sink.poll_control(),
            StreamControl::Continue,
            "an unrelated stream must never receive a pause signal"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !token.is_cancelled(),
        "an unrelated stream must never be cancelled by a steer round"
    );

    let lines = drain_status(&mut status);
    assert!(
        !mentions(&lines, PAUSE_MARK) && !mentions(&lines, RESUME_MARK),
        "no pause/resume status for an untouched stream, got {lines:?}"
    );

    drop(sink);
    deactivate();
}

// ---------------------------------------------------------------------------
// 5. cancel / abort semantics on the skip-pause path
// ---------------------------------------------------------------------------

/// Skipping the pause must not skip the cancel path. `PreemptHandle::Running`
/// carries the same identities, so a `Cancel` subtask and a global abort fire
/// the worker cancellation tokens exactly as they do for an `Active` handle,
/// while every other action leaves the stream running.
#[tokio::test]
async fn test_cancel_and_abort_still_fire_when_pausing_was_skipped() {
    let _serial = SERIAL.lock().await;
    deactivate();
    activate(2);
    let backend = "http://cond-pause-cancel:9413/v1";

    // (a) a `Cancel` subtask cancels its own worker; other actions do not.
    {
        let model = "model-cond-cancel-skip";
        assert!(pool::has_spare_session_capacity(backend, model));
        let cancelled_token = CancellationToken::new();
        let kept_token = CancellationToken::new();
        let rejected_token = CancellationToken::new();
        let mut first = register_stream("coder-t-906", "t-906", &cancelled_token, model);
        let mut second = register_stream("tester-t-907", "t-907", &kept_token, model);
        let mut third = register_stream("researcher-t-908", "t-908", &rejected_token, model);

        let handle = preempt_conflicting_stream_on_backend(backend, model, "cancel coder").await;
        assert!(
            matches!(handle, PreemptHandle::Running(ref ids) if ids.len() == 3),
            "the skip path must still track every conflicting stream"
        );
        for (index, sink) in [&mut first, &mut second, &mut third]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                sink.poll_control(),
                StreamControl::Continue,
                "no pause signal may reach skipped stream {index}"
            );
        }

        let decision = SteerDecision {
            decision: "ForwardToWorker".to_string(),
            response: Some("Cancelling coder.".to_string()),
            tier: None,
            model: None,
            subtasks: vec![
                subtask("t-906", "coder", "Cancel"),
                subtask("t-907", "tester", "ForwardNotice"),
                // Rejected by the shared action normalizer: must not cancel.
                subtask("t-908", "researcher", "Explode"),
            ],
            sleep_seconds: None,
        };
        handle.complete_with_subtask_decision(Some(&decision));

        assert!(
            cancelled_token.is_cancelled(),
            "a Cancel subtask must still cancel a stream that was never paused"
        );
        assert!(
            !kept_token.is_cancelled(),
            "a non-Cancel action leaves its stream running"
        );
        assert!(
            !rejected_token.is_cancelled(),
            "an unrecognized action must not cancel the stream"
        );
        drop((first, second, third));
    }

    // (b) a global abort cancels every conflicting stream, paused or not.
    {
        let model = "model-cond-abort-skip";
        assert!(pool::has_spare_session_capacity(backend, model));
        let tokens = [CancellationToken::new(), CancellationToken::new()];
        let sinks = [
            register_stream("coder-t-909", "t-909", &tokens[0], model),
            register_stream("tester-t-910", "t-910", &tokens[1], model),
        ];

        let handle =
            preempt_conflicting_stream_on_backend(backend, model, "abort everything").await;
        assert!(matches!(handle, PreemptHandle::Running(_)));
        handle.complete_all(PauseAction::Abort);

        for (index, token) in tokens.iter().enumerate() {
            assert!(
                token.is_cancelled(),
                "a global abort must cancel stream {index} even though pausing was skipped"
            );
        }
        drop(sinks);
    }

    // (c) the plain resume action leaves every skipped stream running.
    {
        let model = "model-cond-resume-skip";
        assert!(pool::has_spare_session_capacity(backend, model));
        let tokens = [CancellationToken::new(), CancellationToken::new()];
        let sinks = [
            register_stream("coder-t-911", "t-911", &tokens[0], model),
            register_stream("tester-t-912", "t-912", &tokens[1], model),
        ];

        let handle = preempt_conflicting_stream_on_backend(backend, model, "status check").await;
        assert!(matches!(handle, PreemptHandle::Running(_)));
        handle.complete_all(PauseAction::Resume);

        for token in &tokens {
            assert!(
                !token.is_cancelled(),
                "Resume must not cancel a stream that was never paused"
            );
        }
        drop(sinks);
    }

    deactivate();
}

// ---------------------------------------------------------------------------
// 6. someone already queued => not spare => pause
// ---------------------------------------------------------------------------

/// A free permit is not spare capacity when another session is already in the
/// FIFO queue for that pair — the ledger, not the momentary permit count, owns
/// the answer. With a permit free and one session queued, the steer round must
/// fall back to pausing the conflicting stream.
#[tokio::test]
async fn test_queued_session_blocks_the_skip_path_and_pauses() {
    let _serial = SERIAL.lock().await;
    deactivate();
    activate(2);

    let backend = "http://cond-pause-queued:9414/v1";
    let model = "model-cond-queued";
    let pools_before = pool::open_pool_count();
    let entry = pool::entry_for(backend, model).expect("a configured cap opens a pool");
    assert_eq!(pool::open_pool_count(), pools_before + 1);

    let held = pool::acquire_session_with_wait(backend, model, None, Duration::from_secs(5), None)
        .await
        .expect("pool open")
        .expect("a cap is configured, so this pair is metered");
    let snapshot = pool::pool_snapshot(backend, model).expect("a capped pair has a pool");
    assert_eq!(
        (snapshot.available, snapshot.waiting),
        (1, 0),
        "one permit free, nobody queued"
    );
    assert!(
        pool::has_spare_session_capacity(backend, model),
        "a free permit with an empty queue is spare capacity"
    );

    // A session waiting for that very permit.
    let ticket = pool::queue_ticket(&entry);
    let snapshot = pool::pool_snapshot(backend, model).expect("a capped pair has a pool");
    assert_eq!(
        (snapshot.available, snapshot.waiting),
        (1, 1),
        "the permit is still free, but a session is queued for it"
    );
    assert!(
        !pool::has_spare_session_capacity(backend, model),
        "a queued session means no spare capacity even with a free permit"
    );

    let handle = preempt_conflicting_stream_on_backend(backend, model, "steer now").await;
    assert!(
        matches!(handle, PreemptHandle::None),
        "no stream is registered for this model, so there is nothing to pause"
    );

    // Register the conflicting stream and re-run the round: it must pause, and
    // the full yield/ack/resume handshake must still happen.
    pause_round(backend, model, "coder-t-913", "t-913").await;

    drop(ticket);
    drop(held);
    deactivate();
}

// ---------------------------------------------------------------------------
// 7. the capacity probe never opens a pool entry
// ---------------------------------------------------------------------------

/// `has_spare_session_capacity` must not change what it measures: no pool entry
/// is created for an uncapped pair, a rejected cap, or a capped pair that has
/// not been used yet. Only the acquiring paths open an entry.
#[tokio::test]
async fn test_capacity_probe_never_opens_a_pool_entry() {
    let _serial = SERIAL.lock().await;

    let uncapped = "http://cond-pause-probe:9415/v1";
    let capped = "http://cond-pause-probe-capped:9416/v1";
    let model = "model-cond-probe";

    let before = pool::open_pool_count();

    deactivate();
    assert!(
        !pool::has_spare_session_capacity(uncapped, model),
        "no active cap: unlimited, so never spare"
    );
    assert!(
        !pool::has_spare_session_capacity(capped, model),
        "no active cap: unlimited, so never spare"
    );

    // A rejected cap fails open to unlimited, whether it came from the global
    // key or from a per-URL override.
    activate(0);
    assert!(
        !pool::has_spare_session_capacity(capped, model),
        "a rejected global cap fails open to unlimited"
    );
    activate_override(capped, 0);
    assert!(
        !pool::has_spare_session_capacity(capped, model),
        "a rejected per-URL cap fails open to unlimited as well"
    );

    assert_eq!(
        pool::open_pool_count(),
        before,
        "the capacity probe must never open a pool entry"
    );

    // Cap exactly this one backend; the other one stays unlimited.
    activate_override(capped, 2);
    assert!(
        pool::has_spare_session_capacity(capped, model),
        "a configured cap makes the untouched pair spare"
    );
    assert!(
        pool::has_spare_session_capacity(&format!("{capped}/"), model),
        "the pair is keyed by the normalized backend URL, so a trailing slash is the same pair"
    );
    assert_eq!(
        pool::open_pool_count(),
        before,
        "still no entry opened by the probe"
    );
    assert!(
        !pool::has_spare_session_capacity(uncapped, model),
        "a backend with no cap stays unlimited even while another backend is capped"
    );

    // Only the acquiring path opens the entry, and an untouched pool is spare.
    let entry = pool::entry_for(capped, model).expect("a configured cap opens a pool");
    assert_eq!(pool::open_pool_count(), before + 1);
    assert_eq!(entry.limit(), 2);
    assert!(pool::has_spare_session_capacity(capped, model));

    let first = pool::wait_for_slot(&entry, None, Duration::from_secs(5), None)
        .await
        .expect("first slot");
    assert!(
        pool::has_spare_session_capacity(capped, model),
        "one free permit of two is still spare"
    );
    let second = pool::wait_for_slot(&entry, None, Duration::from_secs(5), None)
        .await
        .expect("second slot");
    assert!(
        !pool::has_spare_session_capacity(capped, model),
        "a saturated pair has no spare capacity"
    );
    drop(first);
    assert!(pool::has_spare_session_capacity(capped, model));
    drop(second);

    deactivate();
}
