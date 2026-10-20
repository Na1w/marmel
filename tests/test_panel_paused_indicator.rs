//! End-to-end guard for the Subagents panel's **arbitration-pause** state.
//!
//! The crate's own unit tests cover how the state is *drawn* (`(Paused …)` row
//! label, the parked-stream status bar, the `[N paused: …]` main-bar segment)
//! and how one typed transition is *applied* to a row. What they cannot cover is
//! the **path** that decides it: a real steering round on a real session loop
//! asking whether a running worker's stream has to yield its model slot, and the
//! row set the panel host is handed as a result. That is what this file drives —
//! a live `run_session` loop, a live worker stream registered in the worker
//! registry and in the preemption registry, and the production Steer Arbitrator
//! round in between.
//!
//! Pinned, one test each:
//!
//! 1. **uncapped pair** (`max_sessions` unconfigured ⇒ unlimited): the steering
//!    round really pauses the worker stream, and the panel row for that worker
//!    ends up `paused == true` while it stays `is_active == true`;
//! 2. **resume**: when the arbitration round hands the slot back, the *same* row
//!    returns to `paused == false` and the pause marker's condition is gone —
//!    the row stays `(Active)`, it is never closed and reopened;
//! 3. **spare capacity** (a configured cap with a free session slot for the
//!    arbitrator's own `backend + model` pair): the same round leaves the row
//!    `paused == false` in every push and emits **no** pause transition at all,
//!    while the stream keeps streaming. This is the cross-feature guard: the
//!    conditional-pause gate and the panel indicator must agree;
//! 4. **full pair at the pause gate** (cap configured, every permit of the pair
//!    taken): the pause happens again, so the skip path cannot be reached just
//!    because a cap exists — while a free slot of that same capped pair leaves the
//!    very same stream and row untouched;
//! 5. **staleness**: a worker cancelled by the round while its row is paused
//!    never leaves a lingering `Paused` row behind — the closed row reads
//!    `paused == false`;
//! 6. **main-bar count**: the row set behind the `[N paused: …]` segment reports
//!    the paused workers, including the two-worker case (`2 paused: …`).
//!
//! What "the panel shows" means in this file: the panel is a projection of the
//! row set the session loop hands its host through `Renderer::set_subagents`,
//! and every pause marker is keyed on one predicate of that row —
//! `row.paused && row.is_active` for the `(Paused …)` label and the parked
//! stream bar, and the same filter over the same rows for the main-bar segment.
//! Everything asserted here is therefore the last thing a renderer can be given
//! before it draws, plus the typed transition that produced it. The character
//!-level drawing of those spans is a `ui::tui` unit-test concern (the draw
//! functions are crate-internal and the TUI's own `init()` needs a terminal), so
//! this file deliberately does not re-implement them.
//!
//! Determinism notes: the worker registry, the preemption registry, the active
//! config, the pool table and the session's event/status sinks are
//! process-global statics, so every test takes `SERIAL` **and** uses its own
//! model name, its own agent/task ids and its own backend. Pool entries are
//! keyed by `(backend, model)` and live for the process lifetime, so unique keys
//! (not cleanup) keep the scenarios from reading each other's state. Nothing
//! here sleeps on a production timeout: the worker's chunk cadence
//! ([`CHUNK_GAP`]) is far below the acknowledgement bound, and every wait is a
//! bounded poll.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use marmennill::config::{ConcurrencyConfig, Config, set_active};
use marmennill::harness::HarnessStats;
use marmennill::harness::monitor::RepetitionDetector;
use marmennill::llm::{ChatClient, PauseAction, ResumableStreamOutput, chat_stream_resumable};
use marmennill::orchestrator::pool;
use marmennill::orchestrator::preemption::{
    PreemptHandle, PreemptibleStreamSink, preempt_conflicting_stream_on_backend,
};
use marmennill::orchestrator::{
    ActiveWorkerGuard, OrchestratorManager, register_active_worker_with_token,
};
use marmennill::types::{ChatRequest, Message};
use marmennill::ui::{Event, Renderer, SubagentDetail, run_session};

/// Serialize the file: worker registry, preemption registry, pool table and the
/// active config are process-global.
static SERIAL: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

/// Gap between the scripted worker backend's SSE chunks. Far below
/// `orchestrator::preemption::PREEMPT_ACK_TIMEOUT`, so a stream that is asked to
/// yield its slot always acknowledges on its next chunk — the pause is never
/// attributed to a timeout.
const CHUNK_GAP: Duration = Duration::from_millis(60);

/// Bound for every witness poll in this file. Generous against the scripted
/// timings, short enough that a missing witness fails the test instead of
/// hanging it.
const DEADLINE: Duration = Duration::from_secs(10);

/// Status line the session loop emits synchronously when it hands a mid-flight
/// steering instruction to the Steer Arbitrator.
const ARBITRATION_MARK: &str = "Arbitrating user steering instruction";
/// ... and the marker of the same round when the session hands it over from inside
/// a streaming Manager turn instead of from its loop top.
const ROUND_STARTED_MARK: &str = "evaluating steering instruction";
/// Status line the preemption handshake emits when a stream really yields its
/// model slot — the wording half of the same fact the panel row carries.
const PAUSE_MARK: &str = "Yielded model slot";
/// And its counterpart when the slot is handed back.
const RESUME_MARK: &str = "Model slot reclaimed";
/// Emitted by the registry when a steering decision cancels a live worker.
const CANCEL_MARK: &str = "Cancelled active specialist worker";

const RESPOND_DIRECTLY: &str =
    r#"{"decision":"RespondDirectly","response":"Understood — carrying on."}"#;

// ---------------------------------------------------------------------------
// witnesses: what the session loop handed the panel host
// ---------------------------------------------------------------------------

/// Everything the live session loop pushed at its host, recorded verbatim.
///
/// `rows` is the panel: one entry per `Renderer::set_subagents` push, each a
/// full row set. `events` is every typed event the loop forwarded, and
/// `statuses` the status traffic the handshake and the registry emit.
#[derive(Clone, Default)]
struct Witness {
    rows: Arc<Mutex<Vec<Vec<SubagentDetail>>>>,
    events: Arc<Mutex<Vec<Event>>>,
    statuses: Arc<Mutex<Vec<String>>>,
    /// Set by the driver to stop the session loop once the witness is complete.
    kill: Arc<AtomicBool>,
    /// The scripted steering line becomes available once the workers really
    /// stream, so the arbitration round never races the stream registration.
    steer_armed: Arc<AtomicBool>,
    steer_taken: Arc<AtomicBool>,
    polls: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}

impl Witness {
    fn new() -> Self {
        Self::default()
    }

    fn push_rows(&self, rows: Vec<SubagentDetail>) {
        self.rows.lock().expect("rows lock").push(rows);
    }

    fn push_event(&self, event: &Event) {
        self.events.lock().expect("events lock").push(event.clone());
    }

    fn push_status(&self, status: &str) {
        self.statuses
            .lock()
            .expect("statuses lock")
            .push(status.to_string());
    }

    /// Every row set the host was given, in order.
    fn rows(&self) -> Vec<Vec<SubagentDetail>> {
        self.rows.lock().expect("rows lock").clone()
    }

    fn statuses(&self) -> Vec<String> {
        self.statuses.lock().expect("statuses lock").clone()
    }

    /// The `paused` values of every `Event::SubagentStreamPause` for `tag`, in
    /// arrival order — the typed transition the row label is driven by.
    fn pause_transitions(&self, tag: &str) -> Vec<bool> {
        self.events
            .lock()
            .expect("events lock")
            .iter()
            .filter_map(|event| match event {
                Event::SubagentStreamPause { agent_tag, paused } if agent_tag == tag => {
                    Some(*paused)
                }
                _ => None,
            })
            .collect()
    }

    /// Every stream tag a pause transition named.
    fn paused_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .events
            .lock()
            .expect("events lock")
            .iter()
            .filter_map(|event| match event {
                Event::SubagentStreamPause { agent_tag, paused } if *paused => {
                    Some(agent_tag.clone())
                }
                _ => None,
            })
            .collect();
        tags.sort();
        tags
    }

    /// How many streamed content chunks arrived for `tag` — proof the stream was
    /// really in flight (and, for the skip path, that it kept streaming).
    fn chunks(&self, tag: &str) -> usize {
        self.events
            .lock()
            .expect("events lock")
            .iter()
            .filter(|event| {
                matches!(event, Event::SubagentMessage { agent_tag, .. } if agent_tag == tag)
            })
            .count()
    }

    fn saw_status(&self, needle: &str) -> bool {
        self.statuses().iter().any(|line| line.contains(needle))
    }

    /// `true` when any status line contains any of `needles`.
    fn saw_any_status(&self, needles: &[&str]) -> bool {
        needles.iter().any(|needle| self.saw_status(needle))
    }

    /// The Steer Arbitrator's streamed decision, as the session loop forwarded it.
    /// Its arrival is the witness that the round ran **and** its result was routed
    /// back into the session, not just that a request reached the backend.
    fn steer_answer(&self) -> String {
        self.events
            .lock()
            .expect("events lock")
            .iter()
            .filter_map(|event| match event {
                Event::SteerResponse(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn saw_steer_answer(&self, needle: &str) -> bool {
        self.steer_answer().contains(needle)
    }

    /// The most recent view of one row, as the host was last given it.
    fn latest_row(&self, name: &str) -> Option<SubagentDetail> {
        self.rows()
            .iter()
            .rev()
            .find_map(|rows| rows.iter().find(|row| row.name == name).cloned())
    }

    /// Every push in which the row for `name` satisfied the pause-marker
    /// predicate, as the index of the push it appeared in.
    fn paused_push_indexes(&self, name: &str) -> Vec<usize> {
        self.rows()
            .iter()
            .enumerate()
            .filter(|(_, rows)| rows.iter().any(|row| row.name == name && draws_paused(row)))
            .map(|(index, _)| index)
            .collect()
    }

    /// Every push in which the row for `name` was present and *not* paused.
    fn active_unpaused_push_indexes(&self, name: &str) -> Vec<usize> {
        self.rows()
            .iter()
            .enumerate()
            .filter(|(_, rows)| {
                rows.iter()
                    .any(|row| row.name == name && row.is_active && !row.paused)
            })
            .map(|(index, _)| index)
            .collect()
    }
}

/// The one predicate the `(Paused …)` label and the parked-stream status bar are
/// keyed on in `ui/tui/render.rs`. A row that satisfies it is a row the panel
/// draws as paused; a row that never satisfies it can never show the marker.
fn draws_paused(row: &SubagentDetail) -> bool {
    row.paused && row.is_active
}

/// The row set the main bar's `[N paused: …]` segment counts: the same
/// `is_active && paused` filter `ui/tui/render.rs` applies to the rows it was
/// handed, keeping the still-running round intact.
fn main_bar_paused_count(rows: &[SubagentDetail]) -> usize {
    rows.iter()
        .filter(|row| row.is_active && row.paused)
        .count()
}

// ---------------------------------------------------------------------------
// the host under test: a recording `Renderer` driven by the real session loop
// ---------------------------------------------------------------------------

/// A panel host with no terminal: it records exactly what the session loop
/// pushes and scripts the only two inputs the loop needs — the goal line and one
/// mid-flight steering line.
struct PanelProbe {
    w: Witness,
    goal: String,
    steer: String,
    reads: usize,
    input_state: marmennill::ui::InputState,
}

impl Renderer for PanelProbe {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_event(&mut self, event: &Event) {
        if let Event::Status(text) = event {
            self.w.push_status(text);
        }
        self.w.push_event(event);
    }

    /// The panel edge: the loop's authoritative row set, verbatim.
    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        self.w.push_rows(subagents);
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn poll_input(&mut self) -> Option<String> {
        self.w.polls.fetch_add(1, Ordering::SeqCst);
        if self.w.kill.load(Ordering::SeqCst) {
            return None;
        }
        if self.w.steer_armed.load(Ordering::SeqCst)
            && !self.w.steer_taken.swap(true, Ordering::SeqCst)
        {
            return Some(self.steer.clone());
        }
        None
    }

    /// First call is the goal; afterwards an empty line keeps the round machine
    /// alive (the loop re-enters its harvest, where the panel is re-projected),
    /// and `None` ends the session once the driver has its witness.
    fn read_input(&mut self) -> Option<String> {
        self.w.reads.fetch_add(1, Ordering::SeqCst);
        if self.reads == 0 {
            self.reads += 1;
            return Some(self.goal.clone());
        }
        if self.w.kill.load(Ordering::SeqCst) {
            None
        } else {
            Some(String::new())
        }
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

    /// The driver's kill switch, so a scenario ends on the witness rather than
    /// on a wall-clock guess.
    fn aborted(&self) -> bool {
        self.input_state.aborted || self.w.kill.load(Ordering::SeqCst)
    }

    fn shutdown(&mut self) {}
}

// ---------------------------------------------------------------------------
// backends
// ---------------------------------------------------------------------------

/// One OpenAI-style SSE body (the shape the session and the arbitrator expect).
fn sse(text: &str) -> String {
    let delta = serde_json::json!({
        "id": "chatcmpl-panel",
        "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": { "content": text, "role": "assistant" }, "finish_reason": null }]
    });
    let stop = serde_json::json!({
        "id": "chatcmpl-panel",
        "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
    });
    format!("data: {delta}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

/// The session backend: Manager turns answer plainly, and the Steer Arbitrator
/// answers with the scripted decision. Both are counted, so a test can prove the
/// arbitration round really ran.
struct SessionBackend {
    server: MockServer,
    arbitrator_calls: Arc<std::sync::atomic::AtomicUsize>,
    assistant_calls: Arc<std::sync::atomic::AtomicUsize>,
}

async fn session_backend(decision: &str) -> SessionBackend {
    let server = MockServer::start().await;
    let arbitrator_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let assistant_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let arbitrator = Arc::clone(&arbitrator_calls);
            let assistant = Arc::clone(&assistant_calls);
            let decision = decision.to_string();
            move |req: &wiremock::Request| {
                let body = String::from_utf8_lossy(&req.body);
                let reply = |text: &str| {
                    ResponseTemplate::new(200)
                        .insert_header("Content-Type", "text/event-stream")
                        .set_body_string(sse(text))
                };
                if body.contains("Steer Arbitrator") {
                    arbitrator.fetch_add(1, Ordering::SeqCst);
                    reply(&decision)
                } else {
                    assistant.fetch_add(1, Ordering::SeqCst);
                    reply("Continuing the round.")
                }
            }
        })
        .mount(&server)
        .await;
    SessionBackend {
        server,
        arbitrator_calls,
        assistant_calls,
    }
}

/// The worker's own model endpoint: an endless run of unique chunks (no `[DONE]`)
/// so the stream stays in flight and keeps its slot until arbitration drops it,
/// then the Assistant Prefill continuation, which finishes the turn.
///
/// Connections are served in order: the first `in_flight_turns` requests are the
/// workers' own endless turns, every later one is the continuation of a stream
/// that was cut at a pause boundary. The counter is the witness for **how often a
/// worker had to re-request its turn**: a stream that kept running never produces
/// a second connection.
async fn worker_backend(in_flight_turns: usize) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind worker backend");
    let addr = listener.local_addr().expect("worker backend addr");
    let connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let connections = Arc::clone(&connections);
        async move {
            while let Ok((stream, _)) = listener.accept().await {
                let served = connections.fetch_add(1, Ordering::SeqCst);
                let in_flight_turn = served < in_flight_turns;
                tokio::spawn(async move {
                    serve_worker_conn(stream, in_flight_turn).await;
                });
            }
        }
    });
    (format!("http://{addr}/v1"), connections)
}

const SSE_HEADERS: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";

async fn serve_worker_conn(mut stream: TcpStream, in_flight_turn: bool) {
    let mut head = [0u8; 8192];
    let _ = stream.read(&mut head).await;
    if stream.write_all(SSE_HEADERS).await.is_err() {
        return;
    }
    if in_flight_turn {
        // Unique chunks: the repetition guard must never be what ends the turn.
        for index in 0..400usize {
            let payload = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "chatcmpl-worker",
                    "object": "chat.completion.chunk",
                    "choices": [{ "index": 0, "delta": { "content": format!("chunk{index} ") }, "finish_reason": null }]
                })
            );
            if !write_sse(&mut stream, &payload).await {
                return;
            }
            tokio::time::sleep(CHUNK_GAP).await;
        }
    } else {
        // The continuation turn: a normal, completed reply, terminated properly.
        let payload = sse("resumed after arbitration ");
        let framed = format!("{:x}\r\n{payload}\r\n0\r\n\r\n", payload.len());
        let _ = stream.write_all(framed.as_bytes()).await;
        let _ = stream.flush().await;
    }
}

/// One length-prefixed HTTP/1.1 chunk written to the raw worker backend.
async fn write_sse(stream: &mut TcpStream, payload: &str) -> bool {
    let framed = format!("{:x}\r\n{payload}\r\n", payload.len());
    stream.write_all(framed.as_bytes()).await.is_ok() && stream.flush().await.is_ok()
}

fn worker_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message::User {
            content: "write the module and keep streaming it".to_string(),
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

// ---------------------------------------------------------------------------
// scenario
// ---------------------------------------------------------------------------

/// One panel scenario. `model`, `agent`, `tasks` and both backends are unique per
/// test, which is what keeps the process-global registries and pool entries from
/// overlapping.
struct Scenario {
    /// The model the session (and therefore the Steer Arbitrator) runs on — also
    /// the model the worker streams on, so the worker's stream is a preemption
    /// target by construction.
    model: &'static str,
    agent: &'static str,
    tasks: &'static [&'static str],
    /// `Some(n)` installs `max_sessions = n` as the active cap; `None` leaves the
    /// pair uncapped, which means **unlimited** — and unlimited always pauses.
    cap: Option<usize>,
    decision: &'static str,
}

/// One live worker: its registry row, its preemption-registered stream and the
/// task driving that stream through the production resumable loop.
struct Worker {
    tag: String,
    guard: Option<ActiveWorkerGuard>,
    token: CancellationToken,
    stream: JoinHandle<anyhow::Result<ResumableStreamOutput>>,
}

struct Run {
    w: Witness,
    /// Connections the worker backend has served — the stream's own record of
    /// whether it was ever cut off and had to re-request its turn.
    worker_connections: Arc<AtomicUsize>,
    session: JoinHandle<anyhow::Result<()>>,
    workers: Vec<Worker>,
    backend: SessionBackend,
    /// Held session-pair permits; dropping them hands the slots back.
    slots: Vec<pool::SessionPermit>,
    _plan_dir: tempfile::TempDir,
    cfg: Config,
}

impl Run {
    /// Start the workers, then the session loop, and let both run.
    async fn start(sc: &Scenario) -> Self {
        let backend = session_backend(sc.decision).await;
        let (worker_endpoint, worker_connections) = worker_backend(sc.tasks.len()).await;

        // The client appends `/chat/completions` to `backend_url`, so the session
        // and the arbitration round address the mock through its `/v1` path.
        let mut cfg = Config {
            backend_url: format!("{}/v1", backend.server.uri()),
            model: sc.model.to_string(),
            system_prompt_path: PathBuf::from("prompts/system.md"),
            ui_mode: "tui".to_string(),
            ..Default::default()
        };
        cfg.orchestration.concurrency = ConcurrencyConfig {
            by_backend: std::collections::HashMap::new(),
            max_sessions: sc.cap,
        };
        // The pool resolves its cap from the process-global active config, so the
        // scenario installs the config it is testing against.
        set_active(cfg.clone());

        let plan_dir = tempfile::tempdir().expect("plan dir");
        let plan = marmennill::manager::Plan::at(plan_dir.path());
        let stats = Arc::new(HarnessStats::new());
        let manager = Arc::new(OrchestratorManager::new(
            ChatClient::new(backend.server.uri(), sc.model),
            plan,
            stats,
        ));

        // Register every worker **before** the session starts: the registry entry
        // is what puts the row on the panel, and the preemption registration is
        // what makes its stream addressable by the arbitration round.
        let mut workers = Vec::new();
        for task in sc.tasks {
            let token = CancellationToken::new();
            let guard = register_active_worker_with_token(
                Some((*task).to_string()),
                sc.agent.to_string(),
                "hold the model slot and keep streaming".to_string(),
                Some(token.clone()),
            );
            let tag = guard.0.clone();
            let sink = PreemptibleStreamSink::register_full(
                tag.clone(),
                Some(sc.agent.to_string()),
                Some((*task).to_string()),
                Some(token.clone()),
                sc.model,
            );
            let client = ChatClient::new(worker_endpoint.clone(), sc.model);
            let request = worker_request(sc.model);
            let token_for_stream = token.clone();
            let stream = tokio::spawn(async move {
                let mut detector = RepetitionDetector::new(3, 5);
                let mut sink = sink;
                chat_stream_resumable(
                    &client,
                    &request,
                    &mut sink,
                    512,
                    16384,
                    &mut detector,
                    false,
                    Some(&token_for_stream),
                )
                .await
            });
            workers.push(Worker {
                tag,
                guard: Some(guard),
                token,
                stream,
            });
        }

        let probe = PanelProbe {
            w: Witness::new(),
            goal: "land the change and keep the suite green".to_string(),
            steer: "hold on — focus on the failing test first".to_string(),
            reads: 0,
            input_state: marmennill::ui::InputState::default(),
        };
        let w = probe.w.clone();
        let session_cfg = cfg.clone();
        let session = tokio::spawn(async move {
            let mut probe = probe;
            run_session(&session_cfg, &mut probe, None, Some(manager)).await
        });

        let mut run = Run {
            w,
            worker_connections: Arc::clone(&worker_connections),
            session,
            workers,
            backend,
            slots: Vec::new(),
            _plan_dir: plan_dir,
            cfg,
        };

        let _ = &mut run;
        run
    }

    /// How many times the Steer Arbitrator's own endpoint was called.
    fn arbitrator_calls(&self) -> usize {
        self.backend.arbitrator_calls.load(Ordering::SeqCst)
    }

    /// Wait until the session loop has come back around to the input prompt, i.e.
    /// its first Manager turn is over and no backend call of the session's own is
    /// in flight any more.
    async fn wait_back_at_prompt(&self) {
        assert!(
            wait_until(DEADLINE, || self.w.reads.load(Ordering::SeqCst) >= 2).await,
            "the session loop must return to its input prompt after the first turn \
             [reads={} polls={}]",
            self.w.reads.load(Ordering::SeqCst),
            self.w.polls.load(Ordering::SeqCst)
        );
    }

    /// Take `n` session permits of one `backend + model` pair and hold them, which
    /// is what turns that pair into a full pool. `url` is only a pool key here —
    /// nothing is ever dialed on it — so a scenario can saturate one pair while the
    /// session's own pair (and therefore its event pump) keeps flowing.
    async fn hold_pair(&mut self, url: &str, model: &str, n: usize) {
        for _ in 0..n {
            let slot = pool::acquire_session(url, model, None, None)
                .await
                .expect("a capped pair hands out a session slot")
                .expect("a capped pair has a pool to hand out");
            self.slots.push(slot);
        }
    }

    /// Wait until every worker really streams, then release the scripted
    /// steering line and wait for the session loop to hand it to the arbitrator.
    async fn drive_round(&self) {
        for worker in &self.workers {
            let streaming = wait_until(DEADLINE, || self.w.chunks(&worker.tag) > 0).await;
            assert!(
                streaming,
                "worker {} must be streaming before the steering round starts \
                 [connections={} polls={} reads={} statuses={:?}]",
                worker.tag,
                self.worker_connections.load(Ordering::SeqCst),
                self.w.polls.load(Ordering::SeqCst),
                self.w.reads.load(Ordering::SeqCst),
                self.w.statuses()
            );
        }
        self.w.steer_armed.store(true, Ordering::SeqCst);
        let arbitrated = wait_until(DEADLINE, || self.arbitrator_calls() > 0).await;
        assert!(
            arbitrated,
            "the session loop must put the mid-flight steering instruction in front of the \
             Steer Arbitrator [polls={} reads={} statuses={:?}]",
            self.w.polls.load(Ordering::SeqCst),
            self.w.reads.load(Ordering::SeqCst),
            self.w.statuses()
        );
        assert!(
            self.w
                .saw_any_status(&[ARBITRATION_MARK, ROUND_STARTED_MARK]),
            "the round must announce itself on the status line: {:?}",
            self.w.statuses()
        );
    }

    async fn wait_paused(&self, tag: &str) -> bool {
        wait_until(DEADLINE, || {
            self.w.pause_transitions(tag).iter().any(|paused| *paused)
        })
        .await
    }

    async fn wait_resumed(&self, tag: &str) -> bool {
        wait_until(DEADLINE, || {
            let transitions = self.w.pause_transitions(tag);
            transitions.iter().any(|paused| *paused) && transitions.iter().any(|paused| !paused)
        })
        .await
    }

    async fn wait_status(&self, needle: &str) -> bool {
        wait_until(DEADLINE, || self.w.saw_status(needle)).await
    }

    /// Close one worker's registry row the way the runner does: drop its guard.
    fn close_worker(&mut self, index: usize) {
        let worker = self.workers.get_mut(index).expect("worker index");
        let guard = worker.guard.take();
        drop(guard);
    }

    /// Hand the held session slots back without ending the scenario.
    fn release_slots(&mut self) {
        self.slots.clear();
    }

    /// Hand the held pool slots back and stop the session loop.
    async fn finish(self) -> Summary {
        let w = self.w.clone();
        drop(self.slots);
        self.w.kill.store(true, Ordering::SeqCst);
        let session = tokio::time::timeout(DEADLINE, self.session)
            .await
            .expect("the session loop stops on the kill switch")
            .expect("session task");
        for worker in self.workers {
            worker.stream.abort();
            drop(worker.token);
            drop(worker.guard);
        }
        Summary {
            w,
            arbitrator_calls: self.backend.arbitrator_calls.load(Ordering::SeqCst),
            assistant_calls: self.backend.assistant_calls.load(Ordering::SeqCst),
            worker_connections: self.worker_connections.load(Ordering::SeqCst),
            session,
        }
    }
}

struct Summary {
    w: Witness,
    arbitrator_calls: usize,
    assistant_calls: usize,
    worker_connections: usize,
    session: anyhow::Result<()>,
}

/// Poll `cond` until it holds or `deadline` passes, so no test depends on a
/// wall-clock sleep being long enough.
async fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if started.elapsed() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The row for `name` must exist on the panel by now — the registry projection
/// owns row existence, and every scenario registers its worker before the
/// session starts.
async fn wait_for_row(w: &Witness, name: &str) -> SubagentDetail {
    assert!(
        wait_until(DEADLINE, || w.latest_row(name).is_some()).await,
        "the worker registry must put a row for {name} on the panel"
    );
    w.latest_row(name).expect("row present")
}

/// Back to "no cap configured" for anything that runs after this file's scenario.
fn clear_cap() {
    set_active(Config::default());
}

// ---------------------------------------------------------------------------
// 1 + 2. the round takes the slot: the row reads paused, then active again
// ---------------------------------------------------------------------------

/// With **no configured cap** the arbitrator has no bounded spare slot of its own
/// to borrow, so the round yields the worker's model slot — and the panel row
/// follows the handshake: `paused == true`, while the worker stays `is_active`,
/// which is exactly the condition the `(Paused …)` label and the parked-stream
/// bar draw on.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_uncapped_steering_round_pauses_the_workers_panel_row() {
    let _serial = SERIAL.lock().await;
    let run = Run::start(&Scenario {
        model: "panel-pause-uncapped-model",
        agent: "panelcoder",
        tasks: &["t-9401"],
        cap: None,
        decision: RESPOND_DIRECTLY,
    })
    .await;
    let tag = run.workers[0].tag.clone();

    let row = wait_for_row(&run.w, &tag).await;
    assert!(
        row.is_active && !row.paused,
        "a live worker starts un-paused: {row:?}"
    );

    run.wait_back_at_prompt().await;
    run.drive_round().await;
    let paused = run.wait_paused(&tag).await;
    let summary = run.finish().await;

    assert!(
        paused,
        "an uncapped pair must pause the conflicting stream, so the panel must be told it is paused"
    );
    assert!(
        summary.w.saw_status(PAUSE_MARK),
        "the handshake's own status line must accompany the pause: {:?}",
        summary.w.statuses()
    );

    // The typed transition: exactly one pause for this worker, named by its own
    // stream tag — never a name parsed out of a status line.
    let transitions = summary.w.pause_transitions(&tag);
    assert_eq!(
        transitions.first().copied(),
        Some(true),
        "the round's first panel transition for {tag} must be the pause: {transitions:?}"
    );
    assert_eq!(
        transitions.iter().filter(|paused| **paused).count(),
        1,
        "one steering round pauses its worker exactly once: {transitions:?}"
    );

    // And the panel consequence: the exact row set the host was handed while the
    // stream was parked, i.e. the row the renderer draws as `(Paused …)`. The
    // round hands the slot back before the loop is done with the turn, so the
    // *latest* row is already un-paused again by the time the run is over: the
    // witness is the parked push itself, snapshotted at the moment it was pushed.
    let paused_pushes = summary.w.paused_push_indexes(&tag);
    assert!(
        !paused_pushes.is_empty(),
        "the panel must be handed a row set where {tag} reads paused while active: {:?}",
        summary.w.rows()
    );
    let parked = summary.w.rows()[paused_pushes[0]]
        .iter()
        .find(|row| row.name == tag)
        .expect("parked row present")
        .clone();
    assert!(
        parked.paused,
        "the round must park the worker's stream on the panel row: {parked:?}"
    );
    assert!(
        parked.is_active,
        "a parked stream is still a live worker: the pause must never close the row: {parked:?}"
    );
    assert_eq!(
        main_bar_paused_count(&summary.w.rows()[paused_pushes[0]]),
        1,
        "the parked row set is exactly what the main bar's `[N paused: …]` segment counts"
    );
    assert_eq!(
        summary.arbitrator_calls, 1,
        "one steering instruction is one arbitration round"
    );
    assert!(
        summary.assistant_calls >= 1,
        "the round must have run against the real backend on both sides: the arbitrator \
         decided and the session kept talking to the model"
    );
    summary.session.expect("the session loop completes");
    clear_cap();
}

/// The same round then hands the slot back, and the **same row** goes back to
/// un-paused without ever being closed and redrawn — the marker disappears while
/// the worker is still `(Active)`.
#[tokio::test(flavor = "multi_thread")]
async fn test_the_panel_row_clears_when_the_stream_reclaims_its_slot() {
    let _serial = SERIAL.lock().await;
    let run = Run::start(&Scenario {
        model: "panel-pause-resume-model",
        agent: "panelcoder",
        tasks: &["t-9402"],
        cap: None,
        decision: RESPOND_DIRECTLY,
    })
    .await;
    let tag = run.workers[0].tag.clone();

    run.wait_back_at_prompt().await;
    run.drive_round().await;
    let paused = run.wait_paused(&tag).await;
    let resumed = paused && run.wait_resumed(&tag).await;
    let summary = run.finish().await;

    assert!(paused, "the round must pause the worker first");
    assert!(
        resumed,
        "a non-Cancel decision must hand the slot back, and the panel must be told"
    );
    assert!(
        summary.w.saw_status(RESUME_MARK),
        "the handshake's resumption line must accompany the clear: {:?}",
        summary.w.statuses()
    );
    assert_eq!(
        summary.w.pause_transitions(&tag),
        vec![true, false],
        "the pause must be paired with exactly one resumption for {tag}"
    );

    let paused_pushes = summary.w.paused_push_indexes(&tag);
    let unpaused = summary.w.active_unpaused_push_indexes(&tag);
    assert!(
        paused_pushes.iter().any(|index| {
            unpaused.iter().any(|later| {
                later > index
                    && !summary.w.rows()[*later]
                        .iter()
                        .any(|row| row.name == tag && draws_paused(row))
            })
        }),
        "after the pause push there must be a push where {tag} is live again without the marker: {:?}",
        summary.w.rows()
    );
    let row = summary.w.latest_row(&tag).expect("row present");
    assert!(
        !row.paused,
        "the panel must not keep a paused row once the slot is back: {row:?}"
    );
    assert!(
        row.is_active,
        "the resumed worker is still running, its row was never closed and reopened: {row:?}"
    );
    summary.session.expect("the session loop completes");
    clear_cap();
}

// ---------------------------------------------------------------------------
// 3. spare capacity: the round runs, the row never reads paused
// ---------------------------------------------------------------------------

/// A configured cap with a free session slot for the arbitrator's own
/// `backend + model` pair means it never needs to borrow anyone's slot. The
/// steering round still runs, the worker's stream still streams — and the panel
/// must never be told the worker is paused. This is the cross-feature guard: the
/// conditional-pause gate and the panel indicator have to agree.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_steering_round_with_spare_capacity_never_pauses_the_panel_row() {
    let _serial = SERIAL.lock().await;
    let run = Run::start(&Scenario {
        model: "panel-pause-spare-model",
        agent: "panelcoder",
        tasks: &["t-9403"],
        // Four slots, nobody queued: the arbitrator has spare capacity.
        cap: Some(4),
        decision: RESPOND_DIRECTLY,
    })
    .await;
    let tag = run.workers[0].tag.clone();

    run.wait_back_at_prompt().await;
    run.drive_round().await;
    // The round has to **complete** — its decision has to be routed back into the
    // session as the answer the user gets — and the worker's stream has to keep
    // producing while it does.
    let decided = wait_until(DEADLINE, || run.w.saw_steer_answer("Understood")).await;
    let streamed = wait_until(DEADLINE, || run.w.chunks(&tag) > 8).await;
    let summary = run.finish().await;

    assert!(
        decided,
        "the arbitration round must complete on spare capacity: the session was never \
         handed the arbitrator's direct answer, it received {:?}",
        summary.w.steer_answer()
    );
    assert_eq!(
        summary.arbitrator_calls, 1,
        "the steering round really ran — the row is un-paused because pausing was \
         skipped, not because no round happened"
    );
    assert!(
        streamed && summary.w.chunks(&tag) > 8,
        "a stream that was never paused keeps streaming its chunks: {:?}",
        summary.w.chunks(&tag)
    );
    assert_eq!(
        summary.worker_connections, 1,
        "a stream left running is never cut at a pause boundary and re_requested"
    );

    assert!(
        summary.w.pause_transitions(&tag).is_empty(),
        "no stream may be told to yield its slot on spare capacity: {:?}",
        summary.w.pause_transitions(&tag)
    );
    assert!(
        summary.w.paused_tags().is_empty(),
        "no worker of this round may be marked paused: {:?}",
        summary.w.paused_tags()
    );
    assert!(
        !summary.w.saw_status(PAUSE_MARK),
        "the pause status line must stay un-emitted as well: {:?}",
        summary.w.statuses()
    );
    assert!(
        summary.w.paused_push_indexes(&tag).is_empty(),
        "no push may hand the host a paused row for {tag}: {:?}",
        summary.w.rows()
    );
    let row = summary.w.latest_row(&tag).expect("row present");
    assert!(
        !row.paused && row.is_active,
        "the row must keep reading `(Active)` through a round that skipped pausing: {row:?}"
    );
    assert_eq!(
        main_bar_paused_count(
            summary
                .w
                .rows()
                .last()
                .expect("the loop pushed at least one row set")
        ),
        0,
        "the main bar must have nothing to count as paused"
    );
    summary.session.expect("the session loop completes");
    clear_cap();
}

// ---------------------------------------------------------------------------
// 4. a full pair at the pause gate pauses again — a cap alone is no skip path
// ---------------------------------------------------------------------------

/// A configured cap is **not** spare capacity: once every session slot of the pair
/// is taken, the gate takes the worker's model slot and the panel row reads paused,
/// exactly as on an uncapped pair — and exactly the opposite of what a free slot of
/// the same capped pair does.
///
/// The gate here is driven through its public entry,
/// [`preempt_conflicting_stream_on_backend`], against two dedicated
/// `backend + model` pairs that share the worker's model, rather than through a
/// second live round: in a live round the arbitrator's pair **is** the session's own
/// pair, so saturating it parks the session loop on its next Manager call — and with
/// it the event pump that feeds the panel — which would say nothing about the gate.
/// The live session, the live worker stream and the live panel projection stay in
/// place; only the capacity witness is addressed directly.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_full_pair_at_the_pause_gate_pauses_the_row_and_a_free_slot_does_not() {
    let _serial = SERIAL.lock().await;
    let mut run = Run::start(&Scenario {
        model: "panel-pause-gate-model",
        agent: "panelcoder",
        tasks: &["t-9404"],
        // Three slots per pair: the session's own pair keeps flowing while the two
        // capacity probes below hold theirs.
        cap: Some(3),
        decision: RESPOND_DIRECTLY,
    })
    .await;
    let tag = run.workers[0].tag.clone();
    let model = run.cfg.model.clone();

    // The stream is really in flight, and its row is on the panel un-paused.
    let streaming = wait_until(DEADLINE, || run.w.chunks(&tag) > 4).await;
    assert!(
        streaming,
        "the worker must be mid-stream before the gate is exercised"
    );
    let row = wait_for_row(&run.w, &tag).await;
    assert!(
        row.is_active && !row.paused,
        "a live worker starts un-paused: {row:?}"
    );

    // --- one free slot of a capped pair: the gate leaves the stream alone ------
    let free_pair = "http://panel-pause-free-pair.invalid/v1";
    run.hold_pair(free_pair, &model, 1).await;
    let snapshot =
        pool::pool_snapshot(free_pair, &model).expect("a configured cap opens a pool for the pair");
    assert_eq!(
        (snapshot.limit, snapshot.available, snapshot.waiting),
        (3, 2, 0),
        "one of three slots taken is spare capacity"
    );
    assert!(
        pool::has_spare_session_capacity(free_pair, &model),
        "a capped pair with a free slot and nobody queued is spare"
    );
    let chunks_before = run.w.chunks(&tag);
    let handle = preempt_conflicting_stream_on_backend(free_pair, &model, "steer mid-flight").await;
    assert!(
        matches!(handle, PreemptHandle::Running(_)),
        "the gate must report the conflicting stream as left running, not paused, \
         on spare capacity"
    );
    // Nothing was ever asked of the stream, so completing the handle is a no-op.
    handle.complete_all(PauseAction::Resume);
    let kept_streaming = wait_until(DEADLINE, || run.w.chunks(&tag) > chunks_before + 4).await;
    assert!(
        kept_streaming,
        "a stream the gate passed over keeps streaming its chunks: {:} -> {:}",
        chunks_before,
        run.w.chunks(&tag)
    );
    assert_eq!(
        run.worker_connections.load(Ordering::SeqCst),
        1,
        "the spare-capacity pass never cuts the stream at a pause boundary"
    );
    assert!(
        run.w.pause_transitions(&tag).is_empty() && run.w.paused_tags().is_empty(),
        "no pause transition may reach the panel from a spare-capacity pass: {:?}",
        run.w.pause_transitions(&tag)
    );
    assert!(
        !run.w.saw_status(PAUSE_MARK),
        "and no pause status line either: {:?}",
        run.w.statuses()
    );
    assert!(
        run.w.paused_push_indexes(&tag).is_empty(),
        "nor a paused row on the panel: {:?}",
        run.w.rows()
    );

    // --- every slot taken: the same gate, the same stream, the row goes paused --
    let full_pair = "http://panel-pause-full-pair.invalid/v1";
    run.hold_pair(full_pair, &model, 3).await;
    let snapshot =
        pool::pool_snapshot(full_pair, &model).expect("a configured cap opens a pool for the pair");
    assert_eq!(
        (snapshot.limit, snapshot.available, snapshot.waiting),
        (3, 0, 0),
        "the scenario must hold every slot of the pair"
    );
    assert!(
        !pool::has_spare_session_capacity(full_pair, &model),
        "a full pair has no spare session capacity, cap or no cap"
    );
    let handle = preempt_conflicting_stream_on_backend(full_pair, &model, "steer mid-flight").await;
    let targeted = match &handle {
        PreemptHandle::Active(list) => list
            .iter()
            .map(|(identity, _ack)| identity.agent_tag.clone())
            .collect::<Vec<_>>(),
        PreemptHandle::Running(_) => panic!(
            "a full pair leaves the arbitrator no slot of its own, so the gate must pause \
             the conflicting stream — it reported it as left running instead"
        ),
        PreemptHandle::None => panic!(
            "the gate found no conflicting stream to pause: the worker's own stream must \
             still be registered against its model"
        ),
    };
    assert_eq!(
        targeted,
        vec![tag.clone()],
        "the paused stream must be exactly this worker's, named by its own tag"
    );

    let paused = run.wait_paused(&tag).await;
    assert!(
        paused,
        "a full pair must pause the stream, and the panel must be told it is paused"
    );
    assert!(
        run.w.saw_status(PAUSE_MARK),
        "the handshake's own status line must accompany the pause: {:?}",
        run.w.statuses()
    );
    let parked_pushes = run.w.paused_push_indexes(&tag);
    assert!(
        !parked_pushes.is_empty(),
        "the panel must be handed a row set where {tag} reads paused while active: {:?}",
        run.w.rows()
    );
    let parked = run.w.rows()[parked_pushes[0]]
        .iter()
        .find(|row| row.name == tag)
        .expect("parked row present")
        .clone();
    assert!(
        parked.paused && parked.is_active,
        "the parked row is a live worker the panel draws as `(Paused …)`: {parked:?}"
    );
    assert_eq!(
        main_bar_paused_count(&run.w.rows()[parked_pushes[0]]),
        1,
        "the parked row is exactly what the main bar's `[N paused: …]` segment counts"
    );

    // Hand the decision back the way the arbitration round does, and the row clears.
    handle.complete_all(PauseAction::Resume);
    let resumed = run.wait_resumed(&tag).await;
    let re_requested = wait_until(DEADLINE, || {
        run.worker_connections.load(Ordering::SeqCst) >= 2
    })
    .await;
    run.release_slots();
    let summary = run.finish().await;

    assert!(
        resumed,
        "handing the slot back must be reported to the panel too"
    );
    assert!(
        summary.w.saw_status(RESUME_MARK),
        "the handshake's resumption line must accompany the clear: {:?}",
        summary.w.statuses()
    );
    assert_eq!(
        summary.w.pause_transitions(&tag),
        vec![true, false],
        "the gate's pause must be paired with exactly one resumption"
    );
    assert!(
        re_requested,
        "a stream that yielded its slot has to re-request its turn, which is what its \
         own backend records: {} connection(s)",
        summary.worker_connections
    );
    let row = summary.w.latest_row(&tag).expect("row present");
    assert!(
        !row.paused && row.is_active,
        "once the slot is back the row reads `(Active)` again: {row:?}"
    );
    summary.session.expect("the session loop completes");
    clear_cap();
}

// ---------------------------------------------------------------------------
// 5. staleness: a cancelled paused worker never leaves a Paused row behind
// ---------------------------------------------------------------------------

/// A `Cancel` subtask addresses the paused worker: the handshake tears the stream
/// down and the registry closes the row. Neither the handshake nor the projection
/// may leave a `Paused` row behind for a worker that is gone.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_paused_worker_cancelled_by_the_round_leaves_no_paused_row() {
    let _serial = SERIAL.lock().await;
    let mut run = Run::start(&Scenario {
        model: "panel-pause-cancel-model",
        agent: "panelcoder",
        tasks: &["t-9405"],
        cap: None,
        decision: r#"{"decision":"ForwardToWorker","response":"Cancelling that worker.","subtasks":[{"tool_call_id":"t-9405","action":"Cancel","agent_name":"panelcoder"}]}"#,
    })
    .await;
    let tag = run.workers[0].tag.clone();

    run.wait_back_at_prompt().await;
    run.drive_round().await;
    let paused = run.wait_paused(&tag).await;
    // The registry-side half of the teardown: the worker is cancelled and its
    // runner drops the guard, which is what closes the row.
    let cancelled = run.wait_status(CANCEL_MARK).await;
    let stream_ended = {
        let mut workers = run.workers;
        let mut ran = true;
        for worker in workers.iter_mut() {
            ran &= wait_until(DEADLINE, || worker.stream.is_finished()).await;
        }
        // `workers` is borrowed back into `run` below by value.
        run.workers = workers;
        ran
    };
    run.close_worker(0);
    let closed = wait_until(DEADLINE, || {
        run.w.latest_row(&tag).is_some_and(|row| !row.is_active)
    })
    .await;
    let summary = run.finish().await;

    assert!(
        paused,
        "the worker must really be paused before it is cancelled"
    );
    assert!(
        cancelled,
        "the round's Cancel subtask must address the running worker: {:?}",
        summary.w.statuses()
    );
    assert!(
        stream_ended,
        "a paused stream handed an abort must stop, not stay parked behind the arbitrator"
    );
    assert!(
        closed,
        "the closed worker's row must still reach the host as inactive: {:?}",
        summary.w.rows()
    );

    // The lingering-row guard itself: nothing the host was handed after the
    // cancellation may read paused, and the final view of the row is closed and
    // un-paused.
    let rows = summary.w.rows();
    let paused_pushes = summary.w.paused_push_indexes(&tag);
    let last_paused = paused_pushes.last().copied();
    if let Some(index) = last_paused {
        let after = rows.iter().skip(index + 1);
        assert!(
            after
                .clone()
                .any(|rows| rows.iter().any(|row| row.name == tag && !row.is_active)),
            "once the pause is over the row must close; the host never saw it inactive: {rows:?}"
        );
        assert!(
            after
                .filter(|rows| rows.iter().any(|row| row.name == tag && row.paused))
                .count()
                == 0,
            "no push after the last paused one may still mark {tag} paused: {rows:?}"
        );
    }
    let row = summary
        .w
        .latest_row(&tag)
        .expect("the closed row is still visible");
    assert!(
        !row.paused,
        "a finished worker must never keep a stale `Paused` label: {row:?}"
    );
    assert!(
        !row.is_active,
        "the cancelled worker's row is closed: {row:?}"
    );
    assert!(
        !summary.w.saw_status(RESUME_MARK),
        "an aborted stream never reclaimed its slot, so it is never reported resumed"
    );
    summary.session.expect("the session loop completes");
    clear_cap();
}

// ---------------------------------------------------------------------------
// 6. the main-bar count follows the rows
// ---------------------------------------------------------------------------

/// Two workers on the capped-away pair both yield their slots, so the row set the
/// main bar counts reports both — the count and the panel labels can never
/// disagree, because they read the same rows.
#[tokio::test(flavor = "multi_thread")]
async fn test_the_main_bar_paused_count_matches_the_paused_rows() {
    let _serial = SERIAL.lock().await;
    let run = Run::start(&Scenario {
        model: "panel-pause-bar-model",
        agent: "panelcoder",
        tasks: &["t-9406", "t-9407"],
        cap: None,
        decision: RESPOND_DIRECTLY,
    })
    .await;
    let first = run.workers[0].tag.clone();
    let second = run.workers[1].tag.clone();

    run.wait_back_at_prompt().await;
    run.drive_round().await;
    let paused = run.wait_paused(&first).await && run.wait_paused(&second).await;
    let resumed = run.wait_resumed(&first).await && run.wait_resumed(&second).await;
    let summary = run.finish().await;

    assert!(paused, "both workers of the round must be paused");
    assert!(resumed, "and both must be handed their slots back");

    let counts: Vec<usize> = summary
        .w
        .rows()
        .iter()
        .map(|rows| main_bar_paused_count(rows))
        .collect();
    assert!(
        counts.contains(&2),
        "the main bar must have been able to render `[2 paused: …]` from a row set: {counts:?}"
    );
    assert!(
        counts.contains(&0),
        "and `[N paused: …]` must fall away entirely once both streams reclaimed their slots: {counts:?}"
    );
    assert_eq!(
        summary.w.paused_tags(),
        {
            let mut both = vec![first.clone(), second.clone()];
            both.sort();
            both
        },
        "exactly the two workers of this round were paused"
    );
    for tag in [&first, &second] {
        let row = summary.w.latest_row(tag).expect("row present");
        assert!(
            !row.paused,
            "{tag} must read un-paused after the round: {row:?}"
        );
    }
    summary.session.expect("the session loop completes");
    clear_cap();
}
