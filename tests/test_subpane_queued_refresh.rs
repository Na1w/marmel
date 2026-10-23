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
//! bound (`PANEL_DEADLINE`), so a loaded machine can never turn a real pass into a timeout; the
//! scenarios that assert *absence* of progress bound themselves with `MANAGER_TURN_DELAY` instead,
//! which is ~10x wider than the observation made inside it and carries an explicit slip guard.

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
/// loop performs no delegation drain at all. It is an order of magnitude wider than the few hundred
/// milliseconds those scenarios spend observing inside it, so the
/// `window_opened_at.elapsed() < MANAGER_TURN_DELAY` slip guard cannot be tripped by ordinary
/// scheduling noise — and it never lengthens a passing run, because `finish` stops the loop instead
/// of waiting for the held turn to be answered.
const MANAGER_TURN_DELAY: Duration = Duration::from_millis(4000);

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
        if let Event::Status(text) = event {
            self.w
                .statuses
                .lock()
                .expect("statuses lock")
                .push(text.clone());
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
        .respond_with(template.set_body_string(sse("Continuing the round.")))
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
    _session_backend: MockServer,
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
            _session_backend: backend,
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

    /// Stop the loop and unwind every process-global this scenario touched.
    async fn finish(mut self, call: Option<JoinHandle<Result<StreamedReply, anyhow::Error>>>) {
        self.witness.kill.store(true, Ordering::SeqCst);
        if let Some(call) = call {
            let _ = tokio::time::timeout(Duration::from_secs(6), call).await;
        }
        if let Some(session) = self.session.take() {
            let _ = tokio::time::timeout(Duration::from_secs(6), session).await;
        }
        self.worker = None;
        clear_delegation_event_sink();
        set_active(Config::default());
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

    // The loop's first boundary has run once its row is on the panel; the Manager turn now sits
    // inside its held backend call.
    assert!(
        wait_until(PANEL_DEADLINE, || witness.latest_row(&row_name).is_some()).await,
        "the row must be on the panel before the transition is produced; pushes: {}",
        witness.pushes().len()
    );

    let held = hold_slot(&backend, queue_model).await;
    let call = tokio::spawn(queued_specialist_call(
        backend.clone(),
        queue_model.to_string(),
        task.to_string(),
    ));
    wait_until_queued(&backend, queue_model, 1).await;
    let window_opened_at = Instant::now();

    assert!(
        wait_until(PANEL_DEADLINE, || is_queued_for(&vector, task)).await,
        "the gate must have appended the Queued event; events: {:?}",
        events_in(&vector)
    );

    // Still inside the held turn: the event is unread and the panel has not moved.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        window_opened_at.elapsed() < MANAGER_TURN_DELAY,
        "test window slipped past the held Manager turn (elapsed {:?}); the blind-window assertions \
         below are not meaningful on this machine",
        window_opened_at.elapsed()
    );
    assert!(
        is_queued_for(&vector, task),
        "the Queued event must still be sitting unread in the vector: the loop runs no delegation \
         drain while its own backend call is in flight; events: {:?}",
        events_in(&vector)
    );
    assert!(
        witness.queued_pushes(&row_name).is_empty(),
        "no queued push may have reached the panel during the drain-free window; pushes: {}",
        witness.pushes().len()
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

    // The loop's first boundary has run; the Manager turn now sits inside its held call.
    assert!(
        wait_until(PANEL_DEADLINE, || witness.latest_row(&row_name).is_some()).await,
        "the row must be on the panel before the transition is produced; pushes: {}",
        witness.pushes().len()
    );

    let held = hold_slot(&backend, queue_model).await;
    let call = tokio::spawn(queued_specialist_call(
        backend.clone(),
        queue_model.to_string(),
        task.to_string(),
    ));
    wait_until_queued(&backend, queue_model, 1).await;
    let window_opened_at = Instant::now();
    assert!(
        wait_until(PANEL_DEADLINE, || is_queued_for(&vector, task)).await,
        "the gate must have appended the Queued event; events: {:?}",
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

    // (b) The loop itself is still parked: the event is unread and the panel has not moved.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        window_opened_at.elapsed() < MANAGER_TURN_DELAY,
        "test window slipped past the held Manager turn (elapsed {:?}); the assertions below \
         are not meaningful on this machine",
        window_opened_at.elapsed()
    );
    assert!(
        is_queued_for(&vector, task),
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
