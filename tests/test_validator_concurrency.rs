//! Concurrency-pool integration for the **validator family** of LLM calls: the
//! deliverable validator pass, the plan audit, and every other turn the shared
//! fix-loop driver (`src/agents/runner/fix_loop.rs`) issues.
//!
//! The contract these tests pin (docs/concurrency_design_decisions.md §4):
//!
//! * **Which pool.** A validator, fix-loop or plan-audit call is metered against
//!   the pool of the pair the *validator* backend chain resolves to
//!   (`resolve_validator_backend`: specialist `validator_*` override →
//!   `[specialists.validator]` → global config). With no overrides that pair **is**
//!   the audited specialist's pair, so validator passes count against the same
//!   `max_sessions` cap — one semaphore, not two. An override that resolves to
//!   another endpoint or model lands on a different key and therefore a different
//!   pool, which is correct: another model server has its own slots.
//! * **Which class.** Every fix-loop turn is attributed as `CallClass::Validator`
//!   with the auditor run's own identity (`validator-coder` + the audited task), so
//!   a queued auditor call is never blamed on the specialist it audits, and it is
//!   metered against the **cap** — never the steer/notice reserve.
//! * **No over-holding.** Nothing in the validator path acquires or releases a slot
//!   (the single chokepoint stays `ChatClient::chat_stream`), so no permit is held
//!   across the fix loop's tool round, and per-call acquisition keeps a validator
//!   pass nested inside its own specialist from self-deadlocking at
//!   `max_sessions = 1` — including a whole cycle of specialist turns, a rejected
//!   pass, a revision turn and a second pass, which must still take turns on a
//!   single slot.
//!
//! The pool registry, the active config, the status sender and the process-wide
//! cancellation token are process-global, so every test takes `VC_TEST_MUTEX` and
//! uses its own mock URL / model name, so pool keys cannot collide either. This
//! file is its own test binary, so it cannot disturb the library unit tests' state.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use marmennill::agents::Agent;
use marmennill::agents::runner::fix_loop::{
    FixLoopBounds, FixLoopResult, LoopParams, fix_loop_call_context, register_loop_worker,
    resolve_validator_backend, run_fix_loop,
};
use marmennill::agents::runner::resolve_specialist_backend;
use marmennill::config::{
    ConcurrencyConfig, Config, MonitoringConfig, SpecialistConfig, set_active,
};
use marmennill::harness::ToolCaller;
use marmennill::harness::monitor::HarnessMonitor;
use marmennill::llm::ChatClient;
use marmennill::manager::ContextEngineFactory;
use marmennill::orchestrator::pool;
use marmennill::orchestrator::{reset_cancellation, set_status_sender};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialize these tests: pool entries, the active config, the status sender and
/// the global cancellation token are all process-global state.
static VC_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Install an active config whose global cap is `max_sessions` per
/// `backend + model` pair.
fn activate(max_sessions: usize) {
    let mut cfg = Config::default();
    cfg.orchestration.concurrency = ConcurrencyConfig {
        by_backend: HashMap::new(),
        max_sessions: Some(max_sessions),
    };
    set_active(cfg);
}

/// A capped config with an explicit global pair — the input both backend
/// resolution chains are asked about.
fn capped_cfg(backend_url: &str, model: &str, max_sessions: usize) -> Config {
    let mut cfg = Config {
        backend_url: backend_url.to_string(),
        model: model.to_string(),
        auth_token: "test-token".to_string(),
        ..Config::default()
    };
    cfg.orchestration.concurrency = ConcurrencyConfig {
        by_backend: HashMap::new(),
        max_sessions: Some(max_sessions),
    };
    cfg
}

fn insert_specialist(cfg: &mut Config, role: &str, sc: SpecialistConfig) {
    cfg.orchestration.specialists.insert(role.to_string(), sc);
}

// ── wire fixtures ────────────────────────────────────────────

fn sse_text(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-valpool",
            "choices": [{
                "delta": { "content": text },
                "finish_reason": "stop"
            }]
        })
    )
}

fn sse_tool_calls(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let items: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(idx, (id, name, args))| {
            serde_json::json!({
                "index": idx,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args.to_string() }
            })
        })
        .collect();
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-valpool",
            "choices": [{
                "delta": { "content": null, "tool_calls": items },
                "finish_reason": "tool_calls"
            }]
        })
    )
}

fn verdict_call(verdict: &str, comments: &str) -> String {
    sse_tool_calls(&[(
        "call_verdict_1",
        "leave_verdict",
        serde_json::json!({"verdict": verdict, "comments": comments}),
    )])
}

fn read_missing_file_call() -> String {
    sse_tool_calls(&[(
        "call_read",
        "read_file",
        serde_json::json!({"path": "definitely-missing-file.rs"}),
    )])
}

/// What a recording backend captured about the calls it served.
#[derive(Clone, Default)]
struct Recorder {
    calls: Arc<AtomicUsize>,
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
    /// Pool snapshot of `backend + model` **as observed while each request was in
    /// flight** — the only way to see that a call really was metered against that
    /// pool, and how many slots that call was holding.
    slots: Arc<std::sync::Mutex<Vec<pool::PoolSnapshot>>>,
}

/// Mount a backend that replays `turns` in order (falling back to a sentinel) and
/// records request count, request bodies and the in-flight pool snapshot for
/// `backend + model`. The server is passed in already started, because the pool
/// key under test is derived from its URI.
async fn mount_recording(
    server: &MockServer,
    turns: Vec<String>,
    backend: &str,
    model: &str,
) -> Recorder {
    let rec = Recorder::default();
    let calls = rec.calls.clone();
    let bodies = rec.bodies.clone();
    let slots = rec.slots.clone();
    let backend = backend.to_string();
    let model = model.to_string();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            let idx = calls.fetch_add(1, Ordering::SeqCst);
            bodies
                .lock()
                .expect("capture lock")
                .push(String::from_utf8_lossy(&req.body).to_string());
            if let Some(snapshot) = pool::pool_snapshot(&backend, &model) {
                slots.lock().expect("capture lock").push(snapshot);
            }
            let body = turns
                .get(idx)
                .cloned()
                .unwrap_or_else(|| sse_text("Unexpected call"));
            ResponseTemplate::new(200).set_body_string(body)
        })
        .mount(server)
        .await;
    rec
}

/// A backend that answers anything with one verdict turn.
async fn mount_verdict(server: &MockServer, verdict: &str, comments: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(verdict_call(verdict, comments)))
        .mount(server)
        .await;
}

// ── the auditor run, driven directly ─────────────────────────

/// Drive one auditor/fix-loop run — the very driver `run_automated_validation` and
/// the plan audit use — against `backend + model`, ending on a verdict.
async fn drive_audit_run(
    backend: &str,
    model: &str,
    worker_name: &str,
    task_id: &str,
) -> FixLoopResult {
    let cfg = Config::default();
    let mon_cfg = MonitoringConfig::default();
    let client = ChatClient::new_with_token(backend, model, "test-token");
    let token = CancellationToken::new();
    let guard = register_loop_worker(
        Some(task_id.to_string()),
        worker_name.to_string(),
        "validator concurrency pass".to_string(),
        &token,
    );
    let mut engine = ContextEngineFactory::new(64_000).specialist_context(
        "auditor role prompt".to_string(),
        "brief under audit".to_string(),
    );
    let mut params = LoopParams {
        client: &client,
        model: model.to_string(),
        tag: format!("{worker_name}-{task_id}"),
        worker_name: worker_name.to_string(),
        task_id: Some(task_id.to_string()),
        worker_key: guard.0.clone(),
        engine: &mut engine,
        tools: Vec::new(),
        token: &token,
        mon_cfg: &mon_cfg,
        cfg: &cfg,
        temperature: 0.0,
        status_template: format!("{worker_name}: auditing the deliverable..."),
        verdict_log_role: "coder".to_string(),
        bounds: FixLoopBounds::default(),
        abort_log_prefix: worker_name.to_string(),
        emit_tool_status: false,
        rebirth_notice: "(checkpoint accepted)".to_string(),
        reasoning_effort: None,
    };
    let mut monitor = HarnessMonitor::new_with_config(
        Arc::new(marmennill::harness::HarnessStats::new()),
        &mon_cfg,
    );
    run_fix_loop(
        &mut params,
        &mut monitor,
        ToolCaller::Specialist(Agent::Validator),
    )
    .await
    .expect("an auditor pass is an outcome, never a hard error")
}

/// Hold one slot of `backend + model` from outside the client, the way a
/// competing session (e.g. the audited specialist's own turn) would.
async fn hold_slot(backend: &str, model: &str) -> pool::SessionPermit {
    pool::acquire_session_with_wait(backend, model, None, Duration::from_secs(5), None)
        .await
        .expect("the first slot is free")
        .expect("a cap is configured, so this pair is metered")
}

/// Poll until `want` sessions are queued for `backend + model`, or give up.
/// Polling (not sleeping) keeps the assertion deterministic: a waiter registers
/// itself on its first poll, and that timing belongs to the runtime.
async fn wait_until_queued(backend: &str, model: &str, want: usize) -> pool::PoolSnapshot {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = pool::pool_snapshot(backend, model).expect("a capped pair has a pool");
        if snapshot.waiting >= want || Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn drained(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }
    lines
}

fn approved(outcome: FixLoopResult) -> String {
    match outcome {
        FixLoopResult::Verdict { approved, critique } => {
            assert!(
                approved,
                "expected an approved verdict, critique: {critique}"
            );
            critique
        }
        other => panic!("expected an approved verdict, got {other:?}"),
    }
}

// ── which pool does a validator pass count against? ──────────

/// The default case: no validator override anywhere, so the validator chain
/// resolves to the very pair the audited specialist runs on. One key ⇒ one
/// semaphore ⇒ the `max_validator_iterations` passes of a task count against the
/// same `max_sessions` cap as ordinary sessions (design §4).
#[tokio::test]
async fn validator_passes_share_the_specialist_pool_by_default() {
    let _serial = VC_TEST_MUTEX.lock().await;
    reset_cancellation();

    let cfg = capped_cfg("http://val-same.invalid:8080/v1", "model-shared-pool", 1);
    let (specialist_backend, _, specialist_model) = resolve_specialist_backend(&cfg, Agent::Coder);
    let (validator_backend, _, validator_model, _) = resolve_validator_backend(&cfg, Agent::Coder);

    assert_eq!(specialist_backend, validator_backend);
    assert_eq!(specialist_model, validator_model);
    assert_eq!(
        pool::pool_key(&specialist_backend, &specialist_model),
        pool::pool_key(&validator_backend, &validator_model),
        "with nothing overridden, the auditor pair must land on the specialist's pool key"
    );

    set_active(cfg);
    let specialist_pool =
        pool::entry_for(&specialist_backend, &specialist_model).expect("a capped pair has a pool");
    let validator_pool = pool::entry_for(&validator_backend, &validator_model)
        .expect("the auditor pair is the same capped pair");
    assert!(
        Arc::ptr_eq(&specialist_pool, &validator_pool),
        "one semaphore for one key: validator passes must not open a second pool"
    );
    assert_eq!(validator_pool.limit(), 1);
    set_active(Config::default());
}

/// A validator override resolves the auditor somewhere else, and "somewhere else"
/// means a separate pool with its own accounting: a different model behind one
/// host, a different host (where a per-URL cap can be aimed at the auditor
/// alone), or an override on the specialist side that moves only the specialist.
#[tokio::test]
async fn a_validator_override_resolves_to_a_separate_pool() {
    let _serial = VC_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    // (a) `validator_model` on the audited role: same host, different model.
    let mut cfg = capped_cfg("http://val-split-a.invalid:8080/v1", "model-global-a", 1);
    insert_specialist(
        &mut cfg,
        "coder",
        SpecialistConfig {
            validator_model: Some("model-auditor-a".to_string()),
            ..Default::default()
        },
    );
    let (sb, _, sm) = resolve_specialist_backend(&cfg, Agent::Coder);
    let (vb, _, vm, _) = resolve_validator_backend(&cfg, Agent::Coder);
    assert_eq!(vb, sb, "the host is inherited, only the model moved");
    assert_ne!(vm, sm);
    set_active(cfg);
    let specialist_pool = pool::entry_for(&sb, &sm).expect("capped");
    let validator_pool = pool::entry_for(&vb, &vm).expect("capped");
    assert!(
        !Arc::ptr_eq(&specialist_pool, &validator_pool),
        "a different model behind one host is a different pool"
    );
    assert_eq!(validator_pool.limit(), 1);

    // (b) `validator_backend_url` on the audited role: a different endpoint, so a
    // per-URL `max_sessions` override can be aimed at the auditor alone.
    let mut cfg = capped_cfg("http://val-split-b.invalid:8080/v1", "model-global-b", 1);
    insert_specialist(
        &mut cfg,
        "coder",
        SpecialistConfig {
            validator_backend_url: Some("http://val-auditor.invalid:8080/v1".to_string()),
            ..Default::default()
        },
    );
    cfg.orchestration
        .concurrency
        .by_backend
        .insert("http://val-auditor.invalid:8080/v1".to_string(), 4);
    let (sb, _, sm) = resolve_specialist_backend(&cfg, Agent::Coder);
    let (vb, _, vm, _) = resolve_validator_backend(&cfg, Agent::Coder);
    assert_ne!(vb, sb);
    set_active(cfg);
    let specialist_pool = pool::entry_for(&sb, &sm).expect("capped by max_sessions");
    let validator_pool = pool::entry_for(&vb, &vm).expect("capped by the URL override");
    assert!(!Arc::ptr_eq(&specialist_pool, &validator_pool));
    assert_eq!(
        specialist_pool.limit(),
        1,
        "the audited role keeps the global cap"
    );
    assert_eq!(
        validator_pool.limit(),
        4,
        "the auditor endpoint carries its own cap"
    );

    // (c) The mirror case, worth pinning: an override on the **specialist** side
    // alone moves the specialist off the global pair while the validator chain
    // falls through to it — so the two land on two pools. The chain in
    // `resolve_validator_backend` falls back to the global config, never to the
    // audited specialist's resolved pair.
    let mut cfg = capped_cfg("http://val-split-c.invalid:8080/v1", "model-global-c", 1);
    insert_specialist(
        &mut cfg,
        "coder",
        SpecialistConfig {
            model: Some("model-specialist-c".to_string()),
            ..Default::default()
        },
    );
    let (sb, _, sm) = resolve_specialist_backend(&cfg, Agent::Coder);
    let (vb, _, vm, _) = resolve_validator_backend(&cfg, Agent::Coder);
    assert_eq!(sm, "model-specialist-c");
    assert_eq!(vm, "model-global-c");
    assert_ne!(
        pool::pool_key(&sb, &sm),
        pool::pool_key(&vb, &vm),
        "only a shared resolved pair shares a cap"
    );

    set_active(Config::default());
}

/// The behavioural half of the same statement: an auditor's calls are metered
/// against the pool of the pair its client was built from, so an auditor pointed
/// at its own model runs straight through a saturated specialist slot instead of
/// queueing behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_auditor_on_its_own_pair_does_not_queue_behind_the_specialist_slot() {
    let _serial = VC_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let specialist_model = "model-ownpool-specialist";
    let auditor_model = "model-ownpool-auditor";
    let server = MockServer::start().await;
    let backend = format!("{}/v1", server.uri());
    let rec = mount_recording(
        &server,
        vec![verdict_call("APPROVED", "audited on its own pair")],
        &backend,
        auditor_model,
    )
    .await;

    // The specialist pair is saturated by someone else…
    let held = hold_slot(&backend, specialist_model).await;
    // …while the auditor runs on the pair `resolve_validator_backend` produced.
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        drive_audit_run(&backend, auditor_model, "validator-coder", "t-502"),
    )
    .await
    .expect("an auditor on its own pair must never wait for the specialist's slot");
    approved(outcome);
    assert_eq!(
        rec.calls.load(Ordering::SeqCst),
        1,
        "the auditor's request reached its own backend"
    );

    let specialist = pool::pool_snapshot(&backend, specialist_model).expect("capped");
    assert_eq!(
        specialist.available, 0,
        "the held specialist slot stayed held"
    );
    assert_eq!(
        specialist.waiting, 0,
        "the auditor never joined the specialist's queue"
    );
    let auditor = pool::pool_snapshot(&backend, auditor_model).expect("capped");
    assert_eq!(
        auditor.available, 1,
        "the auditor took and returned its own slot"
    );
    assert_eq!(auditor.waiting, 0);
    // The auditor's own in-flight snapshot proved it was metered against *its*
    // pair (limit 1, one slot held) — never against the specialist's.
    let slots = rec.slots.lock().expect("capture lock").clone();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].limit, 1);
    assert_eq!(
        slots[0].available, 0,
        "only the auditor's own call was in flight"
    );

    drop(held);
    set_active(Config::default());
}

// ── call class and attribution ───────────────────────────────

/// The context the driver installs for its turns: class `Validator` — a **capped**
/// class, never the steer/notice reserve — carrying the auditor's worker label and
/// the audited task id.
#[tokio::test]
async fn the_fix_loop_context_is_a_validator_call_with_the_runs_identity() {
    let _serial = VC_TEST_MUTEX.lock().await;
    let cfg = Config::default();
    let mon_cfg = MonitoringConfig::default();
    let client = ChatClient::new("http://val-ctx.invalid:8080/v1", "model-ctx");
    let token = CancellationToken::new();
    let mut engine = ContextEngineFactory::new(64_000)
        .specialist_context("auditor role prompt".to_string(), "brief".to_string());
    let params = LoopParams {
        client: &client,
        model: "model-ctx".to_string(),
        tag: "validator-coder-t-501".to_string(),
        worker_name: "validator-coder".to_string(),
        task_id: Some("t-501".to_string()),
        worker_key: "validator-coder-t-501".to_string(),
        engine: &mut engine,
        tools: Vec::new(),
        token: &token,
        mon_cfg: &mon_cfg,
        cfg: &cfg,
        temperature: 0.0,
        status_template: "validator-coder: auditing...".to_string(),
        verdict_log_role: "coder".to_string(),
        bounds: FixLoopBounds::default(),
        abort_log_prefix: "validator-coder".to_string(),
        emit_tool_status: false,
        rebirth_notice: "(checkpoint accepted)".to_string(),
        reasoning_effort: None,
    };

    let context = fix_loop_call_context(&params);
    assert_eq!(context.class, pool::CallClass::Validator);
    assert!(
        context.class.is_capped() && !context.class.uses_reserve(),
        "an auditor pass must draw from the cap, never from the steer/notice reserve"
    );
    assert_eq!(context.agent.as_deref(), Some("validator-coder"));
    assert_eq!(context.task.as_deref(), Some("t-501"));
    assert_eq!(context.describe(), "validator validator-coder [t-501]");
}

/// End to end: with the shared cap saturated, the auditor's turn queues **against
/// that same cap** and its queue traffic names the auditor run and the audited
/// task — not the specialist delegation it audits — and the reserve stays intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_auditor_turn_is_metered_as_a_validator_call_on_the_shared_cap() {
    let _serial = VC_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let model = "model-queue-attribution";
    let server = MockServer::start().await;
    mount_verdict(&server, "APPROVED", "audited while queued").await;
    let backend = format!("{}/v1", server.uri());

    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);
    // The audited specialist's own session holds the only slot for this pair.
    let held = hold_slot(&backend, model).await;

    let run = tokio::spawn({
        let backend = backend.clone();
        let model = model.to_string();
        async move { drive_audit_run(&backend, &model, "validator-coder", "t-503").await }
    });
    let queued = wait_until_queued(&backend, model, 1).await;
    assert_eq!(
        queued.waiting, 1,
        "the auditor's turn must be in line for the shared cap, got {queued:?}"
    );
    assert_eq!(queued.available, 0, "the shared cap is saturated");
    assert_eq!(
        queued.reserve_available,
        pool::INTERNAL_RESERVE_CAPACITY,
        "an auditor pass must not draw on the steer/notice reserve"
    );

    let statuses = drained(&mut status_rx);
    assert!(
        statuses.iter().any(|line| {
            line.contains("queued for a backend slot")
                && line.contains("validator validator-coder")
                && line.contains("[t-503]")
        }),
        "a queued auditor call must name the auditor run and the audited task, got: {statuses:?}"
    );
    assert!(
        !statuses
            .iter()
            .any(|line| line.contains("specialist coder")),
        "a queued validator pass must not be attributed to the audited specialist, got: {statuses:?}"
    );

    drop(held);
    let outcome = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the queued auditor pass must run once a slot frees")
        .expect("the pass must not panic");
    assert_eq!(approved(outcome), "audited while queued");

    let after = pool::pool_snapshot(&backend, model).expect("capped");
    assert_eq!(after.waiting, 0, "no waiter may linger after the pass ends");
    assert_eq!(
        after.available, 1,
        "the pass must return the slot it took (no leak, no over-hold)"
    );

    set_active(Config::default());
}

/// No permit may be held across non-LLM work (design §4). The fix loop's own tool
/// round is the non-LLM work of this path, so when the loop's **second** turn is
/// in flight the pool must show exactly one slot taken — the in-flight call — and
/// not one held hostage by the tool round.
#[tokio::test]
async fn no_slot_is_held_across_the_fix_loops_tool_round() {
    let _serial = VC_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(2);

    let model = "model-tool-round";
    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let server = MockServer::start().await;
    let backend = format!("{}/v1", server.uri());
    let rec = mount_recording(
        &server,
        vec![
            read_missing_file_call(),
            verdict_call("APPROVED", "tool round done"),
        ],
        &backend,
        model,
    )
    .await;

    let outcome = marmennill::harness::with_workspace_root(tmp.path().to_path_buf(), async move {
        drive_audit_run(&backend, model, "validator-coder", "t-504").await
    })
    .await;
    approved(outcome);
    assert_eq!(
        rec.calls.load(Ordering::SeqCst),
        2,
        "one tool round plus the verdict turn"
    );

    let slots = rec.slots.lock().expect("capture lock").clone();
    assert_eq!(slots.len(), 2, "both turns reached the backend");
    for (n, snapshot) in slots.iter().enumerate() {
        assert_eq!(snapshot.limit, 2, "cap for this pair");
        assert_eq!(
            snapshot.available,
            1,
            "turn {}: only the in-flight call may hold a slot — a permit kept across \
             the tool round would leave {} of 2 free, got {snapshot:?}",
            n + 1,
            snapshot.available,
        );
        assert_eq!(snapshot.waiting, 0, "nobody else was queued");
    }

    set_active(Config::default());
}

// ── the `max_sessions = 1` self-deadlock question ────────────

/// A validator pass runs **inside** the audited specialist's delegation. If the
/// delegation held its slot for the whole delegation (the rejected per-delegation
/// design), or if an auditor pass took more than one slot at a time,
/// `max_sessions = 1` would self-deadlock: the auditor would queue behind the very
/// session that is waiting for the auditor's verdict.
///
/// Acquisition is per backend call, so it cannot: every specialist turn and every
/// auditor turn takes one slot and returns it before the next one runs. This test
/// drives the real delegation end to end — specialist turns, tool round, then the
/// validator pass on the **same** `backend + model` — under `max_sessions = 1`,
/// and fails if anything other than a completed run comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_sessions_one_does_not_self_deadlock_a_validator_pass() {
    let _serial = VC_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();
    let model = "model-no-self-deadlock";
    let task_id = "t-505";

    let prompts = root.join(".marmel").join("prompts");
    std::fs::create_dir_all(&prompts).expect("prompts dir");
    std::fs::write(
        prompts.join(format!("{task_id}-validation.md")),
        "# Validation brief AUDIT-505\n\nConfirm calculate() returns 42.\n",
    )
    .expect("verdict file");
    std::fs::write(
        root.join(".marmel").join("execution_plan.md"),
        format!("# Execution Plan\n\n### Phase 1: Implementation\n- [ ] [{task_id}] Deliver the change\n"),
    )
    .expect("plan file");

    // Specialist: write a file, then conclude. Validator: record a verdict.
    let turns = vec![
        sse_tool_calls(&[(
            "call_write_1",
            "write_file",
            serde_json::json!({"path": "src/lib.rs", "content": "pub fn calculate() -> i32 { 42 }"}),
        )]),
        sse_text(&format!("Work delivered.\n\nMISSION COMPLETE ({task_id})")),
        verdict_call("APPROVED", "calculate() returns 42"),
    ];
    let server = MockServer::start().await;
    let backend_url = format!("{}/v1", server.uri());
    let rec = mount_recording(&server, turns, &backend_url, model).await;

    let mut cfg = capped_cfg(&backend_url, model, 1);
    insert_specialist(
        &mut cfg,
        "coder",
        SpecialistConfig {
            module: "src/agents/coder.rs".to_string(),
            tools: vec!["write_file".to_string(), "read_file".to_string()],
            enable_validator: Some(true),
            max_validator_iterations: Some(3),
            ..Default::default()
        },
    );
    set_active(cfg.clone());

    let ctx = marmennill::agents::IsolatedContext {
        role_system_prompt: "You are the Coder specialist.".to_string(),
        brief: format!("Deliver the change for {task_id} and verify it."),
        snippets: vec![],
        task_id: Some(task_id.to_string()),
        image_urls: vec![],
        audio_urls: vec![],
        blueprint: None,
    };
    let client = ChatClient::new(backend_url.clone(), model);
    let token = CancellationToken::new();

    let within = marmennill::harness::with_workspace_root(root.clone(), async move {
        tokio::time::timeout(
            Duration::from_secs(45),
            marmennill::agents::run_specialist_live(&client, Agent::Coder, &ctx, &cfg, &token),
        )
        .await
    })
    .await;

    let deliverable = within.expect(
        "max_sessions = 1 must not self-deadlock a delegation and its validator pass: \
         the run never completed inside the bounded window",
    );
    let deliverable = deliverable.expect("the specialist run must not error");
    assert!(
        deliverable.contains(&format!("MISSION COMPLETE ({task_id})")),
        "deliverable: {deliverable}"
    );
    assert!(
        !deliverable.contains("not validated"),
        "the validator pass must have run, got: {deliverable}"
    );

    assert_eq!(
        rec.calls.load(Ordering::SeqCst),
        3,
        "two specialist turns plus one validator turn"
    );
    let bodies = rec.bodies.lock().expect("capture lock").clone();
    assert!(
        bodies[2].contains("AUDIT-505"),
        "the third request must be the validator pass, driven by the recorded brief"
    );

    // One pool served both roles: all three calls were metered against this single
    // capped entry, each holding exactly one of its one slot, and every call gave
    // its slot straight back.
    let slots = rec.slots.lock().expect("capture lock").clone();
    assert_eq!(slots.len(), 3, "specialist x2 + validator x1, same pool");
    for snapshot in &slots {
        assert_eq!(snapshot.limit, 1, "the shared cap");
        assert_eq!(
            snapshot.available, 0,
            "the in-flight call held the only slot"
        );
        assert_eq!(snapshot.waiting, 0, "nothing ever queued behind itself");
    }
    let after = pool::pool_snapshot(&backend_url, model).expect("capped pair");
    assert_eq!(after.available, 1, "every call released its slot");
    assert_eq!(after.waiting, 0, "no waiter lingered");

    set_active(Config::default());
}

/// One auditor pass is the easy case; the real nesting risk is the **whole
/// validator cycle** on one pair: specialist turns, a `REJECTED` pass, the
/// feedback-injected revision turn, then a second auditor pass — five LLM turns
/// against one `backend + model` key capped at one slot. Nothing in this path may
/// ever need two slots at once, so the cycle must complete turn by turn: each
/// turn takes the single slot and gives it straight back, and **both** auditor
/// turns are metered against the specialist's own pool rather than a second,
/// validator-local allowance. Fails if any turn had to queue behind a slot held
/// by its own delegation, or if an auditor turn skipped the shared pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_validator_and_revision_turns_take_turns_on_one_slot() {
    let _serial = VC_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();
    let model = "model-multi-pass";
    let task_id = "t-506";

    let prompts = root.join(".marmel").join("prompts");
    std::fs::create_dir_all(&prompts).expect("prompts dir");
    std::fs::write(
        prompts.join(format!("{task_id}-validation.md")),
        "# Validation brief AUDIT-506\n\nConfirm calculate() returns 42.\n",
    )
    .expect("verdict file");
    std::fs::write(
        root.join(".marmel").join("execution_plan.md"),
        format!("# Execution Plan\n\n### Phase 1: Implementation\n- [ ] [{task_id}] Deliver the change\n"),
    )
    .expect("plan file");

    // Specialist writes and concludes → auditor pass 1 rejects → specialist
    // revises and concludes → auditor pass 2 approves. Five turns, one pair,
    // one slot.
    let turns = vec![
        sse_tool_calls(&[(
            "call_write_1",
            "write_file",
            serde_json::json!({"path": "src/lib.rs", "content": "pub fn calculate() -> i32 { 42 }"}),
        )]),
        sse_text(&format!(
            "First pass delivered.\n\nMISSION COMPLETE ({task_id})"
        )),
        sse_tool_calls(&[(
            "call_verdict_1",
            "leave_verdict",
            serde_json::json!({
                "verdict": "REJECTED",
                "comments": "Add a regression test for calculate()."
            }),
        )]),
        sse_text(&format!(
            "Revised with tests.\n\nMISSION COMPLETE ({task_id})"
        )),
        sse_tool_calls(&[(
            "call_verdict_2",
            "leave_verdict",
            serde_json::json!({
                "verdict": "APPROVED",
                "comments": "The regression test now covers calculate()."
            }),
        )]),
    ];
    let server = MockServer::start().await;
    let backend_url = format!("{}/v1", server.uri());
    let rec = mount_recording(&server, turns, &backend_url, model).await;

    let mut cfg = capped_cfg(&backend_url, model, 1);
    insert_specialist(
        &mut cfg,
        "coder",
        SpecialistConfig {
            module: "src/agents/coder.rs".to_string(),
            tools: vec!["write_file".to_string(), "read_file".to_string()],
            enable_validator: Some(true),
            max_validator_iterations: Some(3),
            ..Default::default()
        },
    );
    set_active(cfg.clone());

    let ctx = marmennill::agents::IsolatedContext {
        role_system_prompt: "You are the Coder specialist.".to_string(),
        brief: format!("Deliver the change for {task_id} and verify it."),
        snippets: vec![],
        task_id: Some(task_id.to_string()),
        image_urls: vec![],
        audio_urls: vec![],
        blueprint: None,
    };
    let client = ChatClient::new(backend_url.clone(), model);
    let token = CancellationToken::new();

    let within = marmennill::harness::with_workspace_root(root.clone(), async move {
        tokio::time::timeout(
            Duration::from_secs(45),
            marmennill::agents::run_specialist_live(&client, Agent::Coder, &ctx, &cfg, &token),
        )
        .await
    })
    .await;
    let deliverable = within
        .expect(
            "a rejected pass, a revision turn and a second validator pass must not \
             self-deadlock at max_sessions = 1: the cycle never completed inside the \
             bounded window",
        )
        .expect("the specialist run must not error");

    assert_eq!(
        rec.calls.load(Ordering::SeqCst),
        5,
        "specialist x2 + auditor x1 + revision x1 + auditor x1, all on one pair"
    );
    let bodies = rec.bodies.lock().expect("capture lock").clone();
    assert!(
        bodies[2].contains("AUDIT-506"),
        "call 3 must be the first auditor pass"
    );
    assert!(
        bodies[4].contains("AUDIT-506"),
        "call 5 must be the second auditor pass"
    );
    assert!(
        bodies[4].contains("Revised with tests"),
        "the second auditor pass must audit the revised deliverable, got: {bodies:?}"
    );
    assert!(
        !deliverable.contains("not validated"),
        "the cycle must end on a recorded verdict, got: {deliverable}"
    );
    assert!(
        !deliverable.contains("maximum validator iterations"),
        "the cycle must not run out of validator iterations, got: {deliverable}"
    );

    // Five turns, one pool, one slot: every call — specialist and auditor alike —
    // held the only slot while it was in flight, and nothing ever queued. That is
    // the proof that both auditor passes drew on the specialist's cap and that no
    // second, validator-local allowance exists on this path.
    let slots = rec.slots.lock().expect("capture lock").clone();
    assert_eq!(
        slots.len(),
        5,
        "all five turns were metered against this one capped pair"
    );
    for (n, snapshot) in slots.iter().enumerate() {
        assert_eq!(snapshot.limit, 1, "the shared cap");
        assert_eq!(
            snapshot.available,
            0,
            "turn {}: exactly one call may hold the only slot",
            n + 1
        );
        assert_eq!(
            snapshot.waiting,
            0,
            "turn {}: nothing may queue behind its own delegation",
            n + 1
        );
    }
    let after = pool::pool_snapshot(&backend_url, model).expect("capped pair");
    assert_eq!(after.available, 1, "every call released its slot");
    assert_eq!(after.waiting, 0, "no waiter lingered");

    set_active(Config::default());
}
