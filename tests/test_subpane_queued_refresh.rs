//! Repro for the "specialists' subpane does not update when an agent gets queued" report.
//!
//! The user-visible symptom: a specialist row appears in the subpane showing only its
//! start/turn line, and while the agent is genuinely waiting for a backend slot nothing else ever
//! reaches the pane — no `(Queued 1 of 1)` label, no progress — so it reads as a pure visual
//! glitch rather than a stalled run.
//!
//! The five scenarios below separate the two halves of the mechanism, and every wait in them is
//! ordered by an observable event rather than by a sleep, so the failure is deterministic:
//!
//! 1. [`queued_transition_drained_before_its_row_exists_is_consumed_and_never_replayed`] —
//!    the original repro, now green **because of fix #1** (`docs/diagnose_subpane_stale_refresh.md`
//!    §3): `session::project_then_drain_delegation_events` runs the row projection *before* the
//!    `events.drain(..)`, so the transition reaches a row that already exists. On the pre-fix
//!    ordering this assertion failed: the drain consumed the `Queued` while
//!    `update_subagent_queue`'s row-existence gate skipped it, and there was no replay path
//!    because `WorkerEntry` carried no queue field.
//! 2. [`the_same_queued_transition_reaches_the_panel_once_a_row_already_exists`] — control. The
//!    identical, pool-driven transition *does* reach the pane when the row is already open, which
//!    isolates the cause to the row-existence gate plus the missing replay, and rules out a broken
//!    emitter, a mis-attributed identity, or a dead event sink.
//! 3. [`no_delegation_drain_runs_while_the_manager_turn_is_in_flight`] — control. While the
//!    Manager's own backend call is in flight the loop performs no delegation drain at all, so a
//!    transition produced in that window sits unread in the vector. A steering-delegated subtask
//!    awaited inside the stream sink (`src/ui/bridge/sink.rs`) lives entirely inside such a
//!    window — this is the second, additive half of the freeze.
//! 4. [`a_queued_label_the_drain_could_not_fold_is_re_derived_from_the_registry`] — the **durability**
//!    layer (fix #3): the pool's rank is mirrored onto the live `WorkerEntry`, so even when the
//!    `Queued` record is gone from the vector (which is exactly what `events.drain(..)` does to it)
//!    the next projection re-derives the `(Queued n/m)` label instead of losing it for the whole
//!    slot wait. This is the end-to-end half of what
//!    `src/ui/helpers.rs::a_dropped_queued_event_is_re_derived_from_the_registry_by_the_next_projection`
//!    pins at the unit tier.
//! 5. [`the_pool_rank_reaches_the_registry_inside_the_drain_free_window`] — the **precondition** of
//!    fix #4: inside scenario 3's drain-free window — the window the in-sink
//!    `await_with_panel_pump` runs in — the rank is already readable on the registry entry, i.e.
//!    the only structure the pump projects from. Without it the pump would have had nothing to
//!    surface, and the pane would provably stay frozen until the turn ended.
//!
//! What these tests prove and what they do not:
//! * they prove the queue transition is emitted by the production gate (real pool, real permit
//!   wait, real `CallContext` attribution), is appended to the drained vector, is consumed by a
//!   loop drain, and — since fix #3 — is mirrored onto the worker registry entry as well;
//! * they exercise the real `run_session` loop and the real `Renderer::set_subagents` boundary —
//!   the single authoritative writer feeding the subpane.
//! * they do **not** pixel-draw the subpane: `render_subagents` and `show_subagent_panel` are
//!   `pub(crate)`, so an integration test cannot call them. The assertion instead uses the panel's
//!   own draw predicate (`row.queue.is_some() && row.is_active`, from `src/ui/tui/render.rs`
//!   `render_subagents`) applied to the rows the host was handed. The pixel level of the same
//!   regression layer lives in `src/ui/tui/tests.rs`
//!   (`a_queued_rows_whole_pane_is_rebuilt_from_the_registry_alone` and its neighbours).
//! * they do **not** drive a real nested `delegate_task` tool round or the steering sink of
//!   `src/ui/bridge/sink.rs`; scenario 3 shows the drain-free window those paths sit inside, and
//!   scenario 5 shows the state the in-sink pump reads there. The pump itself is pinned in
//!   `src/ui/bridge/tests.rs` (`test_sink_panel_pump_moves_the_pane_while_its_own_await_is_in_flight`).
//!
//! Determinism: the worker registry, the pool table, the active config and the delegation event
//! sink are process-global, so one test runs at a time inside this binary and each uses its own
//! model names, task ids and mock servers. The transition in scenario 1 is produced *before*
//! `run_session` is spawned, so no drain can precede it; the repro's panel poll is deliberately
//! short (2 s) because the mechanism is a *dropped* event — waiting longer cannot make it pass.
//! Every other wait is a positive wait ordered by an observable event with a deliberately generous
//! bound (`PANEL_DEADLINE`), so a loaded machine can never turn a real pass into a timeout. The
//! scenarios that assert *absence* of progress are delimited by observable events at both ends as
//! well, never by a sleep: the window opens when the session's own backend reports that it has been
//! handed the Manager's request (`Scenario::session_requests` — wiremock records a request *before*
//! it awaits the held response, so this is exact proof that the turn has been dispatched and that
//! the loop has already passed the last drain boundary in front of it), and it closes when the held
//! reply reaches the renderer. A slow runner can therefore only make those windows longer, never
//! shorter or raced.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use marmennill::agents::Agent;
use marmennill::config::{ConcurrencyConfig, Config, set_active};
use marmennill::harness::HarnessStats;
use marmennill::llm::client::{ChatClient, StreamedReply};
use marmennill::orchestrator::pool;
use marmennill::orchestrator::pool::QueuePosition;
use marmennill::orchestrator::workers::{self, worker_entries};
use marmennill::orchestrator::{
    ActiveWorkerGuard, DelegationEvent, OrchestratorManager, clear_delegation_event_sink,
    register_active_worker_with_token, set_delegation_event_sink,
};
use marmennill::types::{ChatRequest, Message};
use marmennill::ui::{Event, InputState, Renderer, SubagentDetail, run_session};

/// Pool table, worker registry, active config and the delegation event sink are all
/// process-global: one test at a time inside this binary.
static SERIAL: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

/// Bound for the "must have happened" polls in the passing controls. Deliberately generous: each of
/// them is a *positive* wait for a real pool round-trip plus a real HTTP exchange, and a loaded
/// machine has been measured to stall those for seconds. A tight bound here would only convert a
/// genuine pass into a flaky failure — no assertion in this file gains strength from a short
/// positive deadline, because every failure mode under test is carried by a *negative* assertion
/// inside a bounded window (`MANAGER_TURN_DELAY`) or by the deliberately short `REPRO_DEADLINE`.
const PANEL_DEADLINE: Duration = Duration::from_secs(12);
/// Bound for the panel poll in the repro. Deliberately short: the mechanism drops the event, so a
/// longer wait only slows the failure down.
const REPRO_DEADLINE: Duration = Duration::from_secs(2);
/// The Manager turn of scenarios 3 and 5 is held this long: that is the window in which the session
/// loop performs no delegation drain at all. Scenario 3's blind-window watch is delimited at both
/// ends by observable events — the request landing on the session mock
/// ([`Scenario::session_requests`]) and the held reply reaching the renderer
/// ([`Witness::answered_with`]) — so this delay is no longer a timing assumption the assertions
/// lean on: it only has to outlast the test's own setup (one poll tick plus a pool round-trip), and
/// 4 s leaves orders of magnitude of slack for a loaded runner.
const MANAGER_TURN_DELAY: Duration = Duration::from_millis(4000);

/// The body the session backend answers every Manager turn with ([`session_backend`]). Scenario 3
/// watches for this text to reach the renderer: streamed content is handed to the host from inside
/// the turn itself, so it is the exact end of the held turn — the loop cannot reach any drain or
/// projection boundary of its own before that call has returned.
const SESSION_REPLY: &str = "Continuing the round.";

// ---------------------------------------------------------------------------
// the panel predicate under test
// ---------------------------------------------------------------------------

/// The queue marker's draw condition, verbatim from `src/ui/tui/render.rs`:
/// `let queue_label = if row.is_active && row.queue.is_some()` (`:717`, label built at
/// `:723-727`). A row that fails this predicate shows no queue information at all, whatever the
/// pool ledger says.
fn draws_queued(row: &SubagentDetail) -> bool {
    row.is_active && row.queue.is_some()
}

#[derive(Clone, Default)]
struct Witness {
    rows: Arc<Mutex<Vec<Vec<SubagentDetail>>>>,
    statuses: Arc<Mutex<Vec<String>>>,
    messages: Arc<Mutex<Vec<String>>>,
    kill: Arc<AtomicBool>,
    reads: Arc<AtomicUsize>,
}

impl Witness {
    fn pushes(&self) -> Vec<Vec<SubagentDetail>> {
        self.rows.lock().expect("rows lock").clone()
    }

    fn statuses(&self) -> Vec<String> {
        self.statuses.lock().expect("statuses lock").clone()
    }

    /// Everything the streamed turn handed to the host as assistant text.
    fn messages(&self) -> Vec<String> {
        self.messages.lock().expect("messages lock").clone()
    }

    /// True once the turn's own reply has reached the host. The reply is emitted by the
    /// `StreamSink` from *inside* `chat_client_turn`, i.e. strictly before the session loop can
    /// return from its backend call and reach any drain or projection boundary — which is what
    /// makes it usable as the exact closing edge of a "nothing happened during the turn" watch.
    fn answered_with(&self, marker: &str) -> bool {
        self.messages().iter().any(|line| line.contains(marker))
    }

    /// Every push in which the row for `name` satisfied [`draws_queued`].
    fn queued_pushes(&self, name: &str) -> Vec<usize> {
        self.pushes()
            .iter()
            .enumerate()
            .filter(|(_, rows)| rows.iter().any(|row| row.name == name && draws_queued(row)))
            .map(|(index, _)| index)
            .collect()
    }

    /// The most recent view of one row, as the host was last handed it.
    fn latest_row(&self, name: &str) -> Option<SubagentDetail> {
        self.pushes()
            .iter()
            .rev()
            .find_map(|rows| rows.iter().find(|row| row.name == name).cloned())
    }

    /// A compact rendering of the last push, for failure messages.
    fn last_push_summary(&self) -> String {
        let pushes = self.pushes();
        let Some(last) = pushes.last() else {
            return "<no set_subagents push at all>".to_string();
        };
        last.iter()
            .map(|row| {
                format!(
                    "{{name: {}, is_active: {}, queue: {:?}, logs: {}}}",
                    row.name,
                    row.is_active,
                    row.queue,
                    row.logs.len()
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A terminal-less panel host: records every authoritative push, scripts the input the loop needs,
/// and is stopped by the driver once the witness is complete.
struct Probe {
    w: Witness,
    input_state: InputState,
}

impl Renderer for Probe {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_event(&mut self, event: &Event) {
        match event {
            Event::Status(text) => {
                self.w
                    .statuses
                    .lock()
                    .expect("statuses lock")
                    .push(text.clone());
            }
            Event::Message(text) => {
                self.w
                    .messages
                    .lock()
                    .expect("messages lock")
                    .push(text.clone());
            }
            _ => {}
        }
    }

    /// The panel edge under test.
    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        self.w.rows.lock().expect("rows lock").push(subagents);
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    /// After each round: an empty line keeps the round machine alive, so the loop keeps coming back
    /// to its drain + projection boundary. `None` (kill set) ends the session.
    fn read_input(&mut self) -> Option<String> {
        self.w.reads.fetch_add(1, Ordering::SeqCst);
        if self.w.kill.load(Ordering::SeqCst) {
            None
        } else {
            Some(String::new())
        }
    }

    fn input_state(&mut self) -> &mut InputState {
        &mut self.input_state
    }

    fn input_state_shared(&self) -> &InputState {
        &self.input_state
    }

    fn aborted(&self) -> bool {
        self.input_state.aborted || self.w.kill.load(Ordering::SeqCst)
    }

    fn shutdown(&mut self) {}
}

// ---------------------------------------------------------------------------
// backends and scenario plumbing
// ---------------------------------------------------------------------------

fn sse(text: &str) -> String {
    let delta = serde_json::json!({
        "id": "chatcmpl-subpane",
        "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": { "content": text, "role": "assistant" }, "finish_reason": null }]
    });
    let stop = serde_json::json!({
        "id": "chatcmpl-subpane",
        "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
    });
    format!("data: {delta}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

/// The session's own backend: the Manager answers plainly, optionally held open so a test can prove
/// what does — and does not — happen mid-turn.
async fn session_backend(reply_delay: Option<Duration>) -> MockServer {
    let server = MockServer::start().await;
    let mut template =
        ResponseTemplate::new(200).insert_header("Content-Type", "text/event-stream");
    if let Some(delay) = reply_delay {
        template = template.set_delay(delay);
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(template.set_body_string(sse(SESSION_REPLY)))
        .mount(&server)
        .await;
    server
}

/// The specialist's own backend. Only reached once the queued call actually gets its slot, so zero
/// requests is proof the call was still waiting.
async fn specialist_backend() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/event-stream")
                .set_body_string(sse("specialist deliverable")),
        )
        .mount(&server)
        .await;
    server
}

fn request() -> ChatRequest {
    ChatRequest {
        model: String::new(),
        messages: vec![Message::User {
            content: "hold the line".to_string(),
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

/// `max_sessions = 1` per `backend + model` pair: the second caller on a pair has to queue.
fn config_for(session_model: &str, backend_uri: &str) -> Config {
    let mut cfg = Config {
        backend_url: format!("{backend_uri}/v1"),
        model: session_model.to_string(),
        system_prompt_path: PathBuf::from("prompts/system.md"),
        ui_mode: "tui".to_string(),
        ..Default::default()
    };
    cfg.orchestration.concurrency = ConcurrencyConfig {
        by_backend: HashMap::new(),
        max_sessions: Some(1),
    };
    cfg
}

/// Take the only permit of `backend + model` and hold it, so the next caller queues.
async fn hold_slot(backend: &str, model: &str) -> pool::SessionPermit {
    pool::acquire_session_with_wait(backend, model, None, Duration::from_secs(5), None)
        .await
        .expect("the pool for a capped pair exists")
        .expect("the first permit of the pair is free")
}

/// One genuine specialist call on a capped pair, installed under the [`pool::CallContext`] a real
/// specialist turn installs for itself — that attribution is what makes the production gate emit
/// `Queued`/`Running` at all (`src/orchestrator/delegation.rs:201-208`, reporter at
/// `src/llm/client.rs:895-918`).
async fn queued_specialist_call(
    backend: String,
    model: String,
    task: String,
) -> Result<StreamedReply, anyhow::Error> {
    let client = ChatClient::new(backend, model).with_queue_wait_secs(30);
    let req = request();
    let context = pool::CallContext::for_agent(pool::CallClass::Specialist, "coder", Some(task));
    pool::with_call_context(context, client.chat_stream(&req, |_| true)).await
}

/// Poll the pool ledger until `want` callers are in line for this pair.
async fn wait_until_queued(backend: &str, model: &str, want: usize) -> pool::PoolSnapshot {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let snapshot = pool::pool_snapshot(backend, model).expect("a capped pair has a pool");
        if snapshot.waiting >= want || Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_until(deadline: Duration, cond: impl FnMut() -> bool) -> bool {
    let limit = Instant::now() + deadline;
    let mut cond = cond;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn events_in(vector: &Arc<Mutex<Vec<DelegationEvent>>>) -> Vec<DelegationEvent> {
    vector.lock().expect("event vector lock").clone()
}

fn is_queued_for(vector: &Arc<Mutex<Vec<DelegationEvent>>>, task: &str) -> bool {
    events_in(vector).iter().any(|event| {
        matches!(
            event,
            DelegationEvent::Queued { agent, task: t, position: 1, of: 1 }
                if *agent == Agent::Coder && t.as_deref() == Some(task)
        )
    })
}

/// The same question with the rank left out: event kind + task id + agent only. Used where the
/// point is that the gate appended **an** edge for this delegation, so that a rank reported
/// differently can never masquerade as "nothing was appended" — the ambiguity an exact
/// `1 of 1` pattern leaves in a failure message.
fn is_queued_edge_for(vector: &Arc<Mutex<Vec<DelegationEvent>>>, task: &str) -> bool {
    events_in(vector).iter().any(|event| {
        matches!(event, DelegationEvent::Queued { agent, task: t, .. }
            if *agent == Agent::Coder && t.as_deref() == Some(task))
    })
}

/// The rank carried by the `Queued` edge for `task`, read out so it can be asserted — and
/// reported — on its own.
fn queued_rank_for(vector: &Arc<Mutex<Vec<DelegationEvent>>>, task: &str) -> Option<QueuePosition> {
    events_in(vector).iter().find_map(|event| match event {
        DelegationEvent::Queued {
            agent,
            task: t,
            position,
            of,
        } if *agent == Agent::Coder && t.as_deref() == Some(task) => Some(QueuePosition {
            position: *position,
            of: *of,
        }),
        _ => None,
    })
}

/// One live scenario: capped config installed, session backend + Manager built, the Manager's event
/// vector wired as the process-wide delegation sink, and `run_session` running against a [`Probe`].
/// Worker registration and session start are separate steps so a test can control which came first.
struct Scenario {
    manager: Arc<OrchestratorManager>,
    witness: Witness,
    session: Option<JoinHandle<()>>,
    worker: Option<ActiveWorkerGuard>,
    row_name: String,
    config: Config,
    _plan_dir: tempfile::TempDir,
    /// The Manager's own backend, kept so a test can ask it what it has been **handed** — the
    /// exact dispatch signal for a held turn (see [`Scenario::session_requests`]) — and so dropping
    /// it at the end of the scenario still shuts the mock server down.
    session_backend: MockServer,
}

impl Scenario {
    async fn new(session_model: &str, task: &str, reply_delay: Option<Duration>) -> Scenario {
        let backend = session_backend(reply_delay).await;
        let cfg = config_for(session_model, &backend.uri());
        set_active(cfg.clone());

        let plan_dir = tempfile::tempdir().expect("plan dir");
        let plan = marmennill::manager::Plan::at(plan_dir.path());
        let manager = Arc::new(OrchestratorManager::new(
            ChatClient::new(backend.uri(), session_model),
            plan,
            Arc::new(HarnessStats::new()),
        ));
        // Exactly what `drain_delegation_events_with_transcript` does on the loop's first pass
        // (`src/ui/helpers.rs:776-780`), installed up front so the concurrency gate can already
        // write into this very vector.
        set_delegation_event_sink(Arc::clone(&manager.delegation_events));

        let probe = Probe {
            w: Witness::default(),
            input_state: InputState::default(),
        };
        let witness = probe.w.clone();
        let row_name = format!("coder-{task}");

        Scenario {
            manager,
            witness,
            session: None,
            worker: None,
            row_name,
            _plan_dir: plan_dir,
            session_backend: backend,
            config: cfg,
        }
    }

    /// Register the child as a live worker: the only thing a panel row can be a projection of.
    fn register_worker(&mut self, task: &str) {
        self.worker = Some(register_active_worker_with_token(
            Some(task.to_string()),
            "coder".to_string(),
            "hold the line and wait for a backend slot".to_string(),
            Some(CancellationToken::new()),
        ));
    }

    /// Start the real session loop against the probe.
    fn spawn_session(&mut self) {
        let cfg = self.config.clone();
        let manager = Arc::clone(&self.manager);
        let probe = Probe {
            w: self.witness.clone(),
            input_state: InputState::default(),
        };
        self.session = Some(tokio::spawn(async move {
            let mut probe = probe;
            run_session(
                &cfg,
                &mut probe,
                Some("land the change and keep the suite green".to_string()),
                Some(manager),
            )
            .await
            .expect("the scripted session completes");
        }));
    }

    /// How many requests the Manager's own backend has been **handed**.
    ///
    /// `wiremock` pushes an incoming request into its `received_requests` recording before the
    /// response is produced, and awaits `ResponseTemplate::set_delay` only after the handler has
    /// returned and released the mock-set lock (`mock_server/bare_server.rs::handle_request`,
    /// `mock_server/hyper.rs::run_server`). `chat_client_turn` (`src/ui/session.rs:2321`) is the
    /// session loop's only backend call site, so this count is an exact, machine-independent read
    /// of where the loop stands:
    ///
    /// * `0` — the loop has not issued its turn yet;
    /// * `1` — the turn is dispatched and its response is still being held: the loop is parked
    ///   inside the call, i.e. it has already passed **every** drain and projection boundary that
    ///   precedes the request, including the top-of-turn
    ///   `project_then_drain_delegation_events` (`src/ui/session.rs:2245`);
    /// * `2` — the loop has come back from the turn, so the drain-free window is over.
    async fn session_requests(&self) -> usize {
        self.session_backend
            .received_requests()
            .await
            .expect("the session backend records the requests it receives")
            .len()
    }

    /// Stop the loop and unwind every process-global this scenario touched. A join that times out
    /// or ends abnormally is reported, never swallowed: a session task that outlives its test keeps
    /// running and re-registers **its own** event vector as the process-wide delegation sink
    /// (`drain_delegation_events_with_transcript` re-points the sink at whichever manager it
    /// drains), so it silently poisons the next scenario in this binary and a green run would hide
    /// it.
    async fn finish(mut self, call: Option<JoinHandle<Result<StreamedReply, anyhow::Error>>>) {
        self.witness.kill.store(true, Ordering::SeqCst);
        let mut loose_ends: Vec<String> = Vec::new();
        if let Some(call) = call {
            match tokio::time::timeout(Duration::from_secs(6), call).await {
                Err(_elapsed) => {
                    loose_ends.push("the queued call task was still running after 6 s".to_string());
                }
                Ok(Err(join)) => {
                    loose_ends.push(format!("the queued call task ended abnormally: {join}"));
                }
                Ok(Ok(Err(error))) => {
                    loose_ends.push(format!("the queued call task returned an error: {error}"));
                }
                Ok(Ok(Ok(_reply))) => {}
            }
        }
        if let Some(session) = self.session.take() {
            match tokio::time::timeout(Duration::from_secs(6), session).await {
                Err(_elapsed) => {
                    loose_ends.push("the session task was still running after 6 s".to_string());
                }
                Ok(Err(join)) => {
                    loose_ends.push(format!("the session task ended abnormally: {join}"));
                }
                Ok(Ok(())) => {}
            }
        }
        self.worker = None;
        clear_delegation_event_sink();
        set_active(Config::default());
        assert!(
            loose_ends.is_empty(),
            "scenario teardown left loose ends: {loose_ends:?}"
        );
    }
}

/// Event-driven replacement for the old "a panel row was pushed" proxy: poll
/// [`Scenario::session_requests`] until the session backend has been handed `want` Manager-turn
/// requests, and return how many it had been handed when the poll stopped.
async fn wait_until_dispatched(scenario: &Scenario, want: usize, deadline: Duration) -> usize {
    let limit = Instant::now() + deadline;
    loop {
        let handed = scenario.session_requests().await;
        if handed >= want || Instant::now() >= limit {
            return handed;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// 1. THE REPRO — queued transition drained against a missing row: consumed, never replayed
// ---------------------------------------------------------------------------

/// A queued specialist whose panel row did not exist at the moment the delegation vector was
/// drained never gets any queue state at all, for the whole slot wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_transition_drained_before_its_row_exists_is_consumed_and_never_replayed() {
    let _serial = SERIAL.lock().await;

    let queue_model = "subpane-queue-model-1";
    let session_model = "subpane-session-model-1";
    let task = "t-901";

    let specialist = specialist_backend().await;
    let backend = specialist.uri();

    let mut scenario = Scenario::new(session_model, task, None).await;
    let vector = Arc::clone(&scenario.manager.delegation_events);
    let witness = scenario.witness.clone();
    let row_name = scenario.row_name.clone();

    // The queue transition is produced *before* the session exists, so no drain can precede it and
    // the ordering below is fixed rather than raced.
    let held = hold_slot(&backend, queue_model).await;
    let call = tokio::spawn(queued_specialist_call(
        backend.clone(),
        queue_model.to_string(),
        task.to_string(),
    ));

    // (a) The production gate really appended the transition to the vector this session will drain,
    //     and named this child: real pool, real permit wait, real specialist attribution.
    assert!(
        wait_until(PANEL_DEADLINE, || is_queued_for(&vector, task)).await,
        "the concurrency gate must append Queued(coder, {task}, 1/1) to the vector the session \
         drains; events seen: {:?}",
        events_in(&vector)
    );
    let queued_snapshot = pool::pool_snapshot(&backend, queue_model).expect("capped pair");
    assert_eq!(
        queued_snapshot.waiting, 1,
        "the call is genuinely waiting for a slot"
    );

    // (b) The lifecycle half of this delegation is **not** in that vector: a nested `delegate_task`
    //     / steering subtask pushes Started/Completed into a throw-away manager's own vector
    //     (`src/orchestrator/delegate.rs:106-115`, `src/orchestrator/steer.rs:733-745`), so this
    //     stream only ever sees the queue edge — nothing else will open the row.
    assert!(
        !events_in(&vector).iter().any(
            |event| matches!(event, DelegationEvent::Started { task: t, .. }
                if t.as_deref() == Some(task))
        ),
        "expected no Started for {task} in the drained vector, got {:?}",
        events_in(&vector)
    );

    // The child is dispatched only now, i.e. after the transition was already queued in the vector.
    scenario.register_worker(task);
    scenario.spawn_session();

    // (c) The row does reach the panel — the registry projection opens it (`src/ui/helpers.rs:373`
    //     via `session.rs:2042`) — so the pane is not empty: it shows the active row and nothing
    //     about the wait.
    assert!(
        wait_until(PANEL_DEADLINE, || witness.latest_row(&row_name).is_some()).await,
        "the registry projection must open a row for {row_name}; pushes observed: {}",
        witness.pushes().len()
    );

    // (d) The drain ran *before* the projection: the vector is now empty, so the transition was
    //     consumed, not left pending for a later pass.
    assert!(
        wait_until(PANEL_DEADLINE, || events_in(&vector).is_empty()).await,
        "the loop's first drain must have consumed the Queued event; events still in the vector: \
         {:?}",
        events_in(&vector)
    );

    // (e) THE BUG: no authoritative row set ever carried the queue state, while the pool kept
    //     reporting the very same delegation in line.
    let reached_the_panel = wait_until(REPRO_DEADLINE, || {
        !witness.queued_pushes(&row_name).is_empty()
    })
    .await;
    let still_queued = pool::pool_snapshot(&backend, queue_model).expect("capped pair");
    let row = witness.latest_row(&row_name).expect("row exists");
    assert!(
        reached_the_panel,
        "no `set_subagents` push ever drew a queued row for {row_name}: the panel stayed on the \
         plain active row for the whole slot wait while the pool ledger said the same delegation \
         was still in line. pool: limit={} available={} waiting={} | row: is_active={} queue={:?} \
         | pushes={} | last push=[{}] | queued status lines={:?}",
        still_queued.limit,
        still_queued.available,
        still_queued.waiting,
        row.is_active,
        row.queue,
        witness.pushes().len(),
        witness.last_push_summary(),
        witness
            .statuses()
            .iter()
            .filter(|line| line.contains("queued"))
            .collect::<Vec<_>>(),
    );

    drop(held);
    scenario.finish(Some(call)).await;
}

// ---------------------------------------------------------------------------
// 2. CONTROL — same transition, row already open: it folds and re-pushes
// ---------------------------------------------------------------------------

/// With a row already on the panel, the identical pool-driven transition reaches the panel and the
/// identical `Running` edge takes it back down. This isolates scenario 1's cause to the
/// row-existence gate plus the missing replay, and proves the emitter, the attribution, the sink
/// registration and the panel edge all work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_queued_transition_reaches_the_panel_once_a_row_already_exists() {
    let _serial = SERIAL.lock().await;

    let queue_model = "subpane-queue-model-2";
    let session_model = "subpane-session-model-2";
    let task = "t-902";

    let specialist = specialist_backend().await;
    let backend = specialist.uri();

    let mut scenario = Scenario::new(session_model, task, None).await;
    let vector = Arc::clone(&scenario.manager.delegation_events);
    let witness = scenario.witness.clone();
    let row_name = scenario.row_name.clone();

    scenario.register_worker(task);
    scenario.spawn_session();

    assert!(
        wait_until(PANEL_DEADLINE, || witness.latest_row(&row_name).is_some()).await,
        "the row must be on the panel before the queue transition is produced; pushes: {}",
        witness.pushes().len()
    );

    let held = hold_slot(&backend, queue_model).await;
    let call = tokio::spawn(queued_specialist_call(
        backend.clone(),
        queue_model.to_string(),
        task.to_string(),
    ));
    wait_until_queued(&backend, queue_model, 1).await;

    assert!(
        wait_until(PANEL_DEADLINE, || !witness
            .queued_pushes(&row_name)
            .is_empty())
        .await,
        "a Queued transition drained against an existing row must be folded and re-pushed; \
         events left in the vector: {:?}; last push=[{}]",
        events_in(&vector),
        witness.last_push_summary()
    );
    let folded = witness
        .latest_row(&row_name)
        .expect("row exists")
        .queue
        .expect("queue state folded onto the row");
    assert_eq!((folded.position, folded.of), (1, 1));

    // Handing the slot back takes the label down again (`Running` clears `queue`).
    drop(held);
    let _reply = call.await.expect("call task");
    assert!(
        wait_until(PANEL_DEADLINE, || witness
            .latest_row(&row_name)
            .is_some_and(|row| row.queue.is_none()))
        .await,
        "the Running edge must clear the queue state; last push=[{}]",
        witness.last_push_summary()
    );

    scenario.finish(None).await;
}

// ---------------------------------------------------------------------------
// 3. CONTROL — the in-flight Manager turn is a drain-free window
// ---------------------------------------------------------------------------

/// While the Manager's own backend call is in flight, nothing drains the delegation vector: the
/// event sits there unread and the panel cannot move. A subtask awaited inside the stream sink
/// (`src/ui/bridge/sink.rs:163-190`) lives entirely inside such a window, which is why a
/// steering-delegated specialist shows its start fold and then nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_delegation_drain_runs_while_the_manager_turn_is_in_flight() {
    let _serial = SERIAL.lock().await;

    let queue_model = "subpane-queue-model-3";
    let session_model = "subpane-session-model-3";
    let task = "t-903";

    let specialist = specialist_backend().await;
    let backend = specialist.uri();

    let mut scenario = Scenario::new(session_model, task, Some(MANAGER_TURN_DELAY)).await;
    let vector = Arc::clone(&scenario.manager.delegation_events);
    let witness = scenario.witness.clone();
    let row_name = scenario.row_name.clone();

    scenario.register_worker(task);
    scenario.spawn_session();

    // HANDSHAKE — the exact "the Manager turn is in flight" signal, in place of the old inference
    // from a panel push. A row is opened by the projection at the top of the round/turn
    // (`src/ui/session.rs:2073` / `:2168`), and the loop then runs **another** drain at `:2245`
    // before it issues the turn's request at `:2317`: a transition produced off the row push can
    // land inside that `2073 -> 2245` stretch and be destroyed by it — which is what this scenario
    // did on a macOS runner, where the loop's straight-line prep wins that race. Waiting for the
    // session backend to report that it has been handed the request puts the transition firmly
    // behind that last drain boundary instead, and the held response parks the loop there.
    let handed = wait_until_dispatched(&scenario, 1, PANEL_DEADLINE).await;
    assert_eq!(
        handed, 1,
        "the Manager's backend call must have been dispatched — with no further turn behind it — \
         before a queued transition is produced; the session backend was handed {handed} request(s) \
         while each response is held for {MANAGER_TURN_DELAY:?}"
    );

    // The row is on the panel by construction now: the projection that opens it is one of the
    // boundaries the handshake above has already proved the loop passed.
    assert!(
        witness.latest_row(&row_name).is_some(),
        "the registry projection must have opened a row for {row_name} before the turn's request; \
         pushes: {}",
        witness.pushes().len()
    );

    let held = hold_slot(&backend, queue_model).await;
    let call = tokio::spawn(queued_specialist_call(
        backend.clone(),
        queue_model.to_string(),
        task.to_string(),
    ));

    // The transition below is only ever produced by a call that genuinely reached the pool's wait,
    // so check that rather than letting a dead or slow call masquerade as a missing event —
    // `wait_until_queued` is bounded and its return alone proves nothing.
    let snapshot = wait_until_queued(&backend, queue_model, 1).await;
    assert!(
        !call.is_finished(),
        "the queued specialist call terminated before it was ever in line for a slot; pool: \
         {snapshot:?}"
    );
    assert!(
        snapshot.waiting >= 1,
        "the call must be a real waiter on the capped pair before its transition is asserted; \
         pool: {snapshot:?}"
    );

    // The queued call must have got in line *while that turn is still parked there*. If the setup
    // above ever slipped past the held response, the transition would legitimately have been eaten
    // by the loop's next boundary — say so, rather than letting it read as a missing event.
    let still_one_turn = scenario.session_requests().await;
    assert_eq!(
        still_one_turn, 1,
        "the Manager turn dispatched for this window must still be in flight now that the queued \
         call is in line; the session backend has been handed {still_one_turn} request(s), so the \
         setup above slipped past the {MANAGER_TURN_DELAY:?} held response"
    );

    assert!(
        wait_until(PANEL_DEADLINE, || is_queued_edge_for(&vector, task)).await,
        "the gate must have appended a Queued edge for {task}; events: {:?}",
        events_in(&vector)
    );
    assert_eq!(
        queued_rank_for(&vector, task),
        Some(QueuePosition { position: 1, of: 1 }),
        "with exactly one permit held and exactly one caller in line, the pool's rank for this \
         delegation is 1 of 1; events: {:?}",
        events_in(&vector)
    );

    // WATCH THE WHOLE HELD TURN: every sample, for as long as the turn has not answered, the edge
    // must still be sitting unread in the vector and nothing about the wait may have reached the
    // panel. Both ends of this watch are events, not clocks — it opens on the dispatch handshake
    // and closes the moment the turn's own reply reaches the host, which the sink emits from
    // *inside* `chat_client_turn` (`src/ui/bridge/sink.rs::emit`), i.e. strictly before the loop
    // can return from the call and reach any drain boundary. So every sample here is provably
    // taken while the turn is in flight on Linux, macOS and Windows alike: the last pre-request
    // drain (`session.rs:2245`) is behind us, the next boundary lies behind the reply, and the
    // pool appends exactly one `Queued` per acquisition (`src/orchestrator/pool.rs:583-589`,
    // re-reporting only on a rank change at `:643-652`), so a sample can neither arrive too early
    // to see the edge nor too late to have it drained.
    let mut samples = 0usize;
    let mut drained_mid_turn: Option<String> = None;
    let watch_limit = Instant::now() + PANEL_DEADLINE;
    while !witness.answered_with(SESSION_REPLY) {
        let queued_pushes = witness.queued_pushes(&row_name);
        if !is_queued_edge_for(&vector, task) || !queued_pushes.is_empty() {
            drained_mid_turn = Some(format!(
                "events={:?}, queued pushes={}, total pushes={}",
                events_in(&vector),
                queued_pushes.len(),
                witness.pushes().len()
            ));
            break;
        }
        samples += 1;
        if Instant::now() >= watch_limit {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let closed_on_reply = witness.answered_with(SESSION_REPLY);
    assert!(
        drained_mid_turn.is_none(),
        "the loop must run no delegation drain while its own backend call is in flight, but after \
         {samples} samples inside the held turn: {}",
        drained_mid_turn.unwrap_or_default()
    );
    assert!(
        samples > 0,
        "the watch sampled nothing inside the held turn (samples={samples})"
    );
    assert!(
        closed_on_reply,
        "the held Manager turn never answered within {PANEL_DEADLINE:?}, so the drain-free window \
         has no closing edge; session requests: {}, pushes: {}, statuses: {:?}",
        scenario.session_requests().await,
        witness.pushes().len(),
        witness.statuses()
    );

    // The window closes at the next boundary: the drain consumes the event and the fold lands —
    // while the call is *still* waiting for its slot, so what the pane gains is the queue label.
    // The slot is deliberately not handed back yet, which is what separates "delayed" here from
    // "dropped" in scenario 1.
    assert!(
        wait_until(PANEL_DEADLINE, || events_in(&vector).is_empty()).await,
        "the boundary drain must have consumed the Queued event once the turn ended; events: {:?}",
        events_in(&vector)
    );
    assert!(
        wait_until(PANEL_DEADLINE, || !witness
            .queued_pushes(&row_name)
            .is_empty())
        .await,
        "after the turn the queued state must reach the panel while the call is still in line; \
         last push=[{}]",
        witness.last_push_summary()
    );
    // The words about the wait reach the host on the very same transition, which is why the bug
    // reads as a visual glitch rather than a stalled run.
    assert!(
        wait_until(PANEL_DEADLINE, || witness
            .statuses()
            .iter()
            .any(|line| line.contains("queued for a backend slot")))
        .await,
        "the gate's status sentence should reach the host; statuses: {:?}",
        witness.statuses()
    );

    // Handing the slot back takes the label down again (`Running` clears `queue`).
    drop(held);
    let _reply = call.await.expect("call task");
    assert!(
        wait_until(PANEL_DEADLINE, || witness
            .latest_row(&row_name)
            .is_some_and(|row| row.queue.is_none()))
        .await,
        "the Running edge must clear the queue state; last push=[{}]",
        witness.last_push_summary()
    );

    scenario.finish(None).await;
}

// ---------------------------------------------------------------------------
// 4. REGRESSION (fix #3) — a `Queued` no drain could fold is re-derived from the registry
// ---------------------------------------------------------------------------

/// The durability half of the fix, end to end.
///
/// Scenario 1 drops the transition because the *row* did not exist when the drain ran. This
/// scenario removes the record itself — the vector is emptied, which is byte-for-byte what
/// `events.drain(..)` does to it — and leaves the delegation waiting, so the panel can only get
/// its `(Queued n/m)` label back from the **worker registry entry** the pool mirrored the rank
/// onto (`src/llm/client.rs::publish_worker_queue_position` →
/// `src/orchestrator/workers.rs::set_worker_queue_position`, fix #3).
///
/// On the pre-fix tree this test fails at the `queued_pushes` assertion: `WorkerEntry` carried no
/// rank at all and `apply_registry_entry` never wrote `row.queue`, so the projection opened a row
/// reading `coder-t-904 (Active)` and the panel stayed there for the rest of the slot wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_label_the_drain_could_not_fold_is_re_derived_from_the_registry() {
    let _serial = SERIAL.lock().await;

    let queue_model = "subpane-queue-model-4";
    let session_model = "subpane-session-model-4";
    let task = "t-904";

    let specialist = specialist_backend().await;
    let backend = specialist.uri();

    let mut scenario = Scenario::new(session_model, task, None).await;
    let vector = Arc::clone(&scenario.manager.delegation_events);
    let witness = scenario.witness.clone();
    let row_name = scenario.row_name.clone();

    // Registered *before* it queues, so the gate's rank report addresses a live entry: the
    // correlation in `set_worker_queue_position` refuses to invent one.
    scenario.register_worker(task);

    let held = hold_slot(&backend, queue_model).await;
    let call = tokio::spawn(queued_specialist_call(
        backend.clone(),
        queue_model.to_string(),
        task.to_string(),
    ));

    assert!(
        wait_until(PANEL_DEADLINE, || is_queued_for(&vector, task)).await,
        "the concurrency gate must append Queued(coder, {task}, 1/1); events: {:?}",
        events_in(&vector)
    );
    assert_eq!(
        workers::active_worker_queue_position(&row_name),
        Some(QueuePosition { position: 1, of: 1 }),
        "the same transition is mirrored onto the live registry entry the panel row is a \
         projection of; entry rank for {row_name}: {:?}",
        workers::active_worker_queue_position(&row_name)
    );
    assert_eq!(
        pool::pool_snapshot(&backend, queue_model)
            .expect("capped pair")
            .waiting,
        1,
        "the call is genuinely waiting for a slot"
    );

    // No session has run yet, so no projection has opened a row and no drain has consumed
    // anything. Dropping the record here reproduces the state a drain leaves behind for a
    // transition that reached it with no row to fold onto.
    vector.lock().expect("event vector lock").clear();

    scenario.spawn_session();

    // THE FIX: the next projection re-derives the label from the entry.
    assert!(
        wait_until(PANEL_DEADLINE, || !witness
            .queued_pushes(&row_name)
            .is_empty())
        .await,
        "no `set_subagents` push ever drew a queued row for {row_name} even though the registry \
         entry carried the pool's rank: entry rank={:?}, pool waiting={}, pushes={}, last push=[{}]",
        workers::active_worker_queue_position(&row_name),
        pool::pool_snapshot(&backend, queue_model)
            .expect("capped pair")
            .waiting,
        witness.pushes().len(),
        witness.last_push_summary()
    );
    let re_derived = witness
        .latest_row(&row_name)
        .expect("row exists")
        .queue
        .expect("queue state re-derived by the projection");
    assert_eq!((re_derived.position, re_derived.of), (1, 1));

    // The label appeared while the wait was genuinely still open: the specialist's own backend
    // had not been reached once, so the row was not queued in name only.
    assert_eq!(
        specialist
            .received_requests()
            .await
            .expect("received requests")
            .len(),
        0,
        "the queued label must reach the pane before the call gets its slot"
    );

    // Slot acquisition takes it back down: the gate retracts the rank on the entry and the
    // paired `Running` edge clears the row, so no stale `(Queued 1/1)` outlives the wait.
    drop(held);
    assert!(
        wait_until(PANEL_DEADLINE, || witness
            .latest_row(&row_name)
            .is_some_and(|row| row.queue.is_none()))
        .await,
        "the acquired slot must clear the label; entry rank={:?}, last push=[{}]",
        workers::active_worker_queue_position(&row_name),
        witness.last_push_summary()
    );

    scenario.finish(Some(call)).await;
}

// ---------------------------------------------------------------------------
// 5. REGRESSION (fix #4 precondition) — the rank is on the registry inside the
//    drain-free window the in-sink panel pump runs in
// ---------------------------------------------------------------------------

/// Scenario 3 pins that the loop contributes nothing while its own backend call is in flight.
/// That is precisely the window an `await_with_panel_pump`-wrapped subtask sits in
/// (`src/ui/bridge/sink.rs`, fix #4), so the pump needs a source that does not depend on the
/// loop's boundary at all. This scenario pins that source: inside that same window the pool's
/// rank is already readable on the live `WorkerEntry` — the only structure
/// `pump_subagent_panel` re-projects from.
///
/// On the pre-fix tree there is nothing to read: `WorkerEntry` has no queue field, so the pane
/// provably could not have moved before the turn ended — which is the frozen half of the report.
/// The pump's own behaviour is pinned in `src/ui/bridge/tests.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pool_rank_reaches_the_registry_inside_the_drain_free_window() {
    let _serial = SERIAL.lock().await;

    let queue_model = "subpane-queue-model-5";
    let session_model = "subpane-session-model-5";
    let task = "t-905";

    let specialist = specialist_backend().await;
    let backend = specialist.uri();

    let mut scenario = Scenario::new(session_model, task, Some(MANAGER_TURN_DELAY)).await;
    let vector = Arc::clone(&scenario.manager.delegation_events);
    let witness = scenario.witness.clone();
    let row_name = scenario.row_name.clone();

    scenario.register_worker(task);
    scenario.spawn_session();

    // The same handshake scenario 3 uses, for the same reason: a row push only proves the loop
    // reached a *projection*, and it still runs another drain at `src/ui/session.rs:2245` before it
    // issues the turn's request at `:2317` — a transition produced off the row push can be
    // destroyed inside that `2073 -> 2245` stretch (this is the flake that bit this scenario, whose
    // assertion is byte-identical to scenario 3's). Asking the session backend how many requests it
    // has been handed puts the transition firmly behind that last boundary instead, so the read
    // below is deterministic on a loaded 3-4 vCPU macOS runner just as it is on Linux and Windows:
    // the pool appends its one `Queued` per acquisition (`src/orchestrator/pool.rs:583-589`) into a
    // vector nothing drains until the held response is delivered.
    let handed = wait_until_dispatched(&scenario, 1, PANEL_DEADLINE).await;
    assert_eq!(
        handed, 1,
        "the Manager turn must have been dispatched — with no further turn behind it — before the \
         transition is produced; the session backend was handed {handed} request(s) while each \
         response is held for {MANAGER_TURN_DELAY:?}"
    );
    assert!(
        witness.latest_row(&row_name).is_some(),
        "the registry projection must have opened a row for {row_name} before the turn's request; \
         pushes: {}",
        witness.pushes().len()
    );

    let held = hold_slot(&backend, queue_model).await;
    let call = tokio::spawn(queued_specialist_call(
        backend.clone(),
        queue_model.to_string(),
        task.to_string(),
    ));
    let snapshot = wait_until_queued(&backend, queue_model, 1).await;
    assert!(
        !call.is_finished(),
        "the queued specialist call terminated before it was ever in line for a slot; pool: \
         {snapshot:?}"
    );
    assert!(
        snapshot.waiting >= 1,
        "the call must be a real waiter on the capped pair before its rank is asserted; pool: \
         {snapshot:?}"
    );
    let handed_while_queued = scenario.session_requests().await;
    assert_eq!(
        handed_while_queued, 1,
        "the turn whose window this scenario reads must still be in flight now that the call is in \
         line; the session backend has been handed {handed_while_queued} request(s), so the setup \
         above slipped past the {MANAGER_TURN_DELAY:?} held response"
    );
    assert!(
        wait_until(PANEL_DEADLINE, || is_queued_edge_for(&vector, task)).await,
        "the gate must have appended a Queued edge for {task}; events: {:?}",
        events_in(&vector)
    );

    // (a) What the pump reads, read directly from the registry: no drain, no projection, no
    //     renderer involvement.
    let entry = worker_entries()
        .into_iter()
        .find(|entry| entry.key == row_name)
        .expect("the delegation has a live registry entry");
    assert!(
        entry.is_active,
        "the entry behind the row is live while it waits for a slot"
    );
    assert_eq!(
        entry.queue_position,
        Some(QueuePosition { position: 1, of: 1 }),
        "the pool's rank is on the entry the pane rebuilds from, got {entry:?}"
    );
    assert_eq!(
        workers::active_worker_queue_position(&row_name),
        Some(QueuePosition { position: 1, of: 1 }),
        "and readable through the registry's own read model"
    );

    // (b) The loop is provably still parked inside the turn that opened this window: exactly one
    //     request has been handed to its backend and that turn's reply has not reached the host
    //     yet, so the loop has not been back to a boundary of its own since the rank was read.
    //     Both ends are observed facts, not a wall-clock guess, so no slip guard is needed here.
    let handed_at_read = scenario.session_requests().await;
    assert_eq!(
        handed_at_read, 1,
        "the turn this window belongs to must still be in flight; the session backend has been \
         handed {handed_at_read} request(s), so the loop came back from it and the assertions below \
         would no longer be inside a drain-free window"
    );
    assert!(
        !witness.answered_with(SESSION_REPLY),
        "the held reply had already reached the host, so this window had closed before the rank was \
         read; messages: {:?}",
        witness.messages()
    );
    assert!(
        is_queued_edge_for(&vector, task),
        "the Queued event must still sit unread in the vector: the loop runs no delegation drain \
         while its own backend call is in flight; events: {:?}",
        events_in(&vector)
    );
    assert!(
        witness.queued_pushes(&row_name).is_empty(),
        "the loop pushed nothing about the wait during that window — that is the freeze fix #4 \
         pumps through; pushes: {}",
        witness.pushes().len()
    );

    drop(held);
    scenario.finish(Some(call)).await;
}
