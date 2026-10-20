//! Internal fast calls under a concurrency cap: the call-site metering that
//! design §5 (and invariants I5/I7) requires, proven through the real call sites
//! rather than through a hand-installed context.
//!
//! The questions pinned here:
//! * the **Steer Arbitrator** call sites
//!   (`orchestrator::steer::arbitrate_steer_context_stream` and the subtask
//!   synthesis `orchestrator::steer::synthesize_steer_subtask_response`) run on
//!   the per-key **reserve**, so a cap saturated by specialist sessions never
//!   stalls steering, and neither joins the FIFO queue;
//! * the **notice evaluation** call site (`orchestrator::notice::evaluate_worker_reply`)
//!   does the same even when it is issued from inside a saturated specialist
//!   turn — the call site re-meters the inherited `Specialist` class, which is
//!   what prevents an internal fast call from queueing behind the very session
//!   that dispatched it;
//! * a reserve call **cannot self-deadlock**: it waits for the reserve slot and
//!   takes it when one is handed back, and when the reserve stays busy past the
//!   bound it runs un-metered with a warning (fail open) instead of wedging —
//!   even with both the cap and the reserve gone;
//! * the **prompt architect** call site
//!   (`agents::prompt_builder::PromptBuilder::synthesize_with_llm`) is **capped**,
//!   so JIT blueprint synthesis queues like any other session and never borrows
//!   the internal reserve;
//! * queueing introduces no preemption surface (design §7, G3, I5): a session
//!   waiting for a slot is not in `ACTIVE_STREAMS` (neither yielded nor cancelled
//!   by a steer round), while a genuinely running stream still yields its slot.
//!
//! The pool registry, the active config, the process-wide cancellation token and
//! the preemption registry are process-global, so every test here takes
//! `FAST_TEST_MUTEX`, and each test uses its own mock server (unique URL) and its
//! own model name so pool entries never collide.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use marmennill::agents::catalog::Catalog;
use marmennill::agents::prompt_builder::PromptBuilder;
use marmennill::agents::{Agent, DelegationRequest, Deliverable, MissionMarker};
use marmennill::config::{ConcurrencyConfig, Config, set_active};
use marmennill::harness::HarnessStats;
use marmennill::llm::client::ChatClient;
use marmennill::llm::{PauseAction, StreamControl, StreamSink};
use marmennill::orchestrator::notice::{SteerNotice, evaluate_worker_reply};
use marmennill::orchestrator::pool;
use marmennill::orchestrator::preemption::{
    PreemptHandle, PreemptibleStreamSink, preempt_conflicting_stream,
    preempt_conflicting_stream_on_backend,
};
use marmennill::orchestrator::steer::{
    SteerContext, arbitrate_steer_context_stream, synthesize_steer_subtask_response,
};
use marmennill::orchestrator::{CURRENT_WORKER_TOKEN, is_globally_cancelled, reset_cancellation};
use marmennill::types::{ChatRequest, Message};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialize these tests: pool entries, the active config, the global
/// cancellation token and the preemption registry are process-global state.
static FAST_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Install an active config whose global cap is `max_sessions` per
/// `backend + model` pair.
fn activate(max_sessions: usize) {
    let mut cfg = Config::default();
    cfg.orchestration.concurrency = ConcurrencyConfig {
        by_backend: std::collections::HashMap::new(),
        max_sessions: Some(max_sessions),
    };
    set_active(cfg);
}

/// Back to "no cap configured".
fn deactivate() {
    set_active(Config::default());
}

fn sse_body(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-fast",
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

/// A backend that answers anything with `text`. `delay` holds the response open
/// so a test can observe pool/reserve state while the call is genuinely in
/// flight.
async fn backend(text: &str, delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    let mut template = ResponseTemplate::new(200).set_body_string(sse_body(text));
    if !delay.is_zero() {
        template = template.set_delay(delay);
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(template)
        .mount(&server)
        .await;
    server
}

/// Hold one capped slot of `backend + model` from outside the client, the way a
/// competing specialist session would.
async fn hold_slot(backend: &str, model: &str) -> pool::SessionPermit {
    pool::acquire_session_with_wait(backend, model, None, Duration::from_secs(5), None)
        .await
        .expect("the first slot is free")
        .expect("a cap is configured, so this pair is metered")
}

/// Occupy this pair's single internal-reserve slot from outside the client, the
/// way an in-flight steer/notice call would.
async fn hold_reserve(backend: &str, model: &str) -> pool::ReservePermit {
    match pool::acquire_reserve(backend, model, None).await {
        pool::ReserveOutcome::Reserved(permit) => permit,
        other => panic!("the reserve slot is free, expected Reserved, got {other:?}"),
    }
}

/// How many requests actually reached the mock backend.
async fn hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("wiremock records its requests")
        .len()
}

/// Poll the queue ledger until `want` sessions are in line, or give up.
///
/// Polling instead of sleeping keeps the assertion deterministic: a waiter
/// registers in the FIFO ledger on its first poll, and when that happens is up to
/// the runtime.
async fn wait_until_queued(backend: &str, model: &str, want: usize) -> pool::PoolSnapshot {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = pool::pool_snapshot(backend, model).expect("a capped pair has a pool");
        if snapshot.waiting >= want {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "expected {want} queued session(s) for {backend} + {model}, got {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The arbitrator reply the steering tests feed back: a direct answer, the shape
/// `arbitrate_steer_context_stream` parses into `RespondDirectly`.
fn steer_reply() -> String {
    serde_json::json!({
        "decision": "RespondDirectly",
        "response": "Steered: stop refactoring and fix the failing test first."
    })
    .to_string()
}

fn steer_context<'a>() -> SteerContext<'a> {
    SteerContext {
        main_goal: "keep the suite green",
        orchestrator_status: "Active",
        pending_approval: "None",
        plan_progress: "None",
        plan_content: "None",
        available_agents: "",
        steering_history: "None",
        user_message: "stop and fix the failing test",
        active_subtasks: "None",
    }
}

fn notice() -> SteerNotice {
    SteerNotice {
        notice_id: "notice-fast-1".to_string(),
        user_inquiry: "Why did you choose SQLite?".to_string(),
        target_worker: "coder-t-001".to_string(),
        created_at_ms: 100,
    }
}

/// Design §5: the Steer Arbitrator call site must carry the reserve class, so a
/// cap saturated by ordinary sessions never stalls a steering round. This drives
/// the real entry point — a context installed anywhere else would not be enough.
#[tokio::test]
async fn test_steer_arbitrator_call_site_runs_on_the_reserve() {
    let _serial = FAST_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let server = backend(&steer_reply(), Duration::from_millis(300)).await;
    let backend = server.uri();
    let model = "model-steer-site";

    // Saturate the cap with something that is not the arbitrator.
    let held = hold_slot(backend.as_str(), model).await;

    let client = ChatClient::new_with_token(backend.clone(), model, "tok");
    let stats = HarnessStats::new();

    let started = Instant::now();
    let arbitration = tokio::time::timeout(
        Duration::from_secs(5),
        arbitrate_steer_context_stream(&client, &stats, steer_context(), |_| {}),
    )
    .await
    .expect("steering must never queue behind a saturated cap");
    let elapsed = started.elapsed();

    let decision = arbitration.expect("the arbitrator parsed a decision");
    assert_eq!(decision.decision, "RespondDirectly");
    assert_eq!(hits(&server).await, 1, "the arbitrator's request went out");
    assert!(
        elapsed < Duration::from_secs(2),
        "a reserve call must start immediately, waited {elapsed:?}"
    );

    // The capped slot and the FIFO queue must never have been touched, and the
    // reserve is back to capacity now that the call has returned.
    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        snapshot.available, 0,
        "the arbitrator must not consume a capped slot"
    );
    assert_eq!(
        snapshot.waiting, 0,
        "the arbitrator must never join the FIFO queue"
    );
    assert_eq!(
        snapshot.reserve_available,
        pool::INTERNAL_RESERVE_CAPACITY,
        "the reserve slot is handed back when the arbitration returns"
    );

    drop(held);
    deactivate();
}

/// The notice-evaluation call site is the second internal fast call. It is issued
/// from **inside** a specialist's own tool round, so it inherits the specialist's
/// context; the call site has to re-meter it as `NoticeEval`, otherwise the
/// evaluation queues behind the cap that same worker saturates (a self-deadlock
/// at `max_sessions = 1`). The inherited context is installed here explicitly to
/// prove the override happens.
#[tokio::test]
async fn test_notice_eval_call_site_runs_on_the_reserve() {
    let _serial = FAST_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let reply_json = serde_json::json!({
        "decision": "SynthesizeResponse",
        "response": "Coder chose SQLite because it needs no external server.",
        "follow_up_prompt": null,
        "user_status": null
    })
    .to_string();
    let server = backend(&reply_json, Duration::from_millis(300)).await;
    let backend = server.uri();
    let model = "model-notice-site";

    let held = hold_slot(backend.as_str(), model).await;

    let client = ChatClient::new_with_token(backend.clone(), model, "tok");
    let stats = HarnessStats::new();

    let evaluation = tokio::time::timeout(
        Duration::from_secs(5),
        pool::with_call_context(
            // Exactly what the specialist runner installs for its own turns.
            pool::CallContext::for_agent(
                pool::CallClass::Specialist,
                "coder-t-001",
                Some("t-001".to_string()),
            ),
            evaluate_worker_reply(
                &client,
                &stats,
                &notice(),
                "coder-t-001",
                "SQLite needs no separate server process.",
                |_| {},
            ),
        ),
    )
    .await
    .expect("a worker-reply evaluation must never queue behind the specialist that dispatched it")
    .expect("the evaluation succeeds");

    assert_eq!(evaluation.decision, "SynthesizeResponse");
    assert_eq!(hits(&server).await, 1);

    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        snapshot.available, 0,
        "the evaluation must not consume a capped slot"
    );
    assert_eq!(
        snapshot.waiting, 0,
        "the evaluation must never join the FIFO queue behind its own worker"
    );
    assert_eq!(
        snapshot.reserve_available,
        pool::INTERNAL_RESERVE_CAPACITY,
        "the reserve slot is handed back when the evaluation returns"
    );

    drop(held);
    deactivate();
}

/// A steering round issues a **second** backend call once its subtasks deliver:
/// `synthesize_steer_subtask_response`, driven from a Manager turn
/// (`ui::bridge::steer`). Design §5 classes *both* steer sites as internal fast
/// calls, so this call site must re-meter itself as `SteerArbitrator` instead of
/// inheriting the capped `ManagerTurn` class in force — otherwise the synthesis
/// of a steer answer queues behind the very specialist sessions the round is
/// arbitrating (self-deadlock at `max_sessions = 1`).
#[tokio::test]
async fn test_steer_subtask_synthesis_call_site_runs_on_the_reserve() {
    let _serial = FAST_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let server = backend(
        "The 3 failing tests are in module foo.",
        Duration::from_millis(300),
    )
    .await;
    let backend = server.uri();
    let model = "model-steer-synth";

    // Every capped slot of this pair is held by ordinary session work.
    let held = hold_slot(backend.as_str(), model).await;

    let client = ChatClient::new_with_token(backend.clone(), model, "tok");
    let stats = HarnessStats::new();
    let deliverables = vec![(
        Agent::Researcher,
        "t-001".to_string(),
        Deliverable {
            marker: MissionMarker::Complete {
                task_id: Some("t-001".to_string()),
            },
            content: "Found 3 failing tests in module foo".to_string(),
            task_id: Some("t-001".to_string()),
        },
    )];

    let started = Instant::now();
    let answer = tokio::time::timeout(
        Duration::from_secs(5),
        // The ManagerTurn context is exactly what the caller has in force here;
        // if the call site did not override it, this call would queue and time out.
        pool::with_call_context(
            pool::CallContext::for_agent(pool::CallClass::ManagerTurn, "manager", None),
            synthesize_steer_subtask_response(
                &client,
                &stats,
                "Which tests are failing?",
                &deliverables,
                |_| {},
            ),
        ),
    )
    .await
    .expect("steer subtask synthesis must never queue behind a saturated cap")
    .expect("the synthesis produced an answer");
    let elapsed = started.elapsed();

    assert_eq!(answer, "The 3 failing tests are in module foo.");
    assert_eq!(hits(&server).await, 1, "the synthesis request went out");
    assert!(
        elapsed < Duration::from_secs(2),
        "a reserve call must start immediately, waited {elapsed:?}"
    );

    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        snapshot.available, 0,
        "the synthesis must not consume a capped slot"
    );
    assert_eq!(
        snapshot.waiting, 0,
        "the synthesis must never join the FIFO queue"
    );
    assert_eq!(
        snapshot.reserve_available,
        pool::INTERNAL_RESERVE_CAPACITY,
        "the reserve slot is handed back when the synthesis returns"
    );

    drop(held);
    deactivate();
}

/// A reserve call cannot self-deadlock. With **both** the capped slot and the
/// single reserve slot occupied, an internal fast call must (a) wait briefly and
/// take the reserve when it is handed back, and (b) when the reserve stays busy
/// past the bound, run un-metered with a warning — never wedge, never error.
#[tokio::test]
async fn test_reserve_calls_cannot_self_deadlock_and_fail_open_within_the_bound() {
    let _serial = FAST_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    // One body answers both internal calls below. It is deliberately the shared
    // shape the arbitrator and the reply evaluator both accept, so the only
    // thing this test varies is slot availability — not decision vocabulary.
    let reply_json = serde_json::json!({
        "decision": "RespondDirectly",
        "response": "Coder explains the retry policy.",
        "follow_up_prompt": null,
        "user_status": null
    })
    .to_string();
    let server = backend(&reply_json, Duration::from_millis(150)).await;
    let backend = server.uri();
    let model = "model-reserve-deadlock";

    let session = hold_slot(backend.as_str(), model).await;
    let reserve = hold_reserve(backend.as_str(), model).await;
    assert_eq!(
        reserve.reserve_available(),
        0,
        "this pair has exactly one reserve slot and it is held"
    );

    // (a) The reserve wait is a real wait, not an immediate give-up: the slot is
    // released under the waiting call, which must then take it instead of
    // burning its whole bound.
    let releaser = {
        let backend = backend.clone();
        let model = model.to_string();
        tokio::spawn(async move {
            // Touch the entry so the release cannot be reordered before the pool
            // for this key exists.
            let entry = pool::entry_for(&backend, &model).expect("a capped pair has a pool");
            assert_eq!(entry.reserve_available(), 0);
            drop(entry);
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(reserve);
        })
    };

    // The call site installs `NoticeEval` itself: no test-side context is needed,
    // which is the point.
    let client =
        ChatClient::new_with_token(backend.clone(), model, "tok").with_reserve_wait_secs(3);
    let stats = HarnessStats::new();
    let started = Instant::now();
    let evaluation = tokio::time::timeout(
        Duration::from_secs(5),
        evaluate_worker_reply(&client, &stats, &notice(), "coder-t-001", "reply", |_| {}),
    )
    .await
    .expect("an internal fast call must not wedge while the cap is saturated")
    .expect("the evaluation succeeds");
    let taken = started.elapsed();
    releaser.await.expect("the releaser ran");

    assert_eq!(evaluation.decision, "RespondDirectly");
    assert!(
        taken < Duration::from_secs(2),
        "the call took the reserve slot as soon as it freed ({taken:?}) instead of \
         waiting out its 3 s reserve bound"
    );

    // (b) Nothing is available anywhere: the capped slot is still held and this
    // reserve holder never lets go. The call must fail open — bounded wait, then
    // run un-metered — rather than sit on the reserve forever.
    let busy_reserve = hold_reserve(backend.as_str(), model).await;
    let failing_client =
        ChatClient::new_with_token(backend.clone(), model, "tok").with_reserve_wait_secs(1);
    let started = Instant::now();
    let decision = tokio::time::timeout(
        Duration::from_secs(5),
        arbitrate_steer_context_stream(&failing_client, &stats, steer_context(), |_| {}),
    )
    .await
    .expect("a fail-open reserve call must terminate, never wedge")
    .expect("the arbitrator still parsed a decision after running un-metered");
    let failed_open = started.elapsed();

    assert_eq!(decision.decision, "RespondDirectly");
    assert!(
        (Duration::from_millis(900)..Duration::from_secs(3)).contains(&failed_open),
        "the call must wait out its 1 s reserve bound and then proceed, waited {failed_open:?}"
    );
    assert_eq!(
        hits(&server).await,
        2,
        "both internal calls reached the backend"
    );

    // The failed-open call took nothing: the reserve stayed with its original
    // holder and the capped slot was never consumed.
    let entry = pool::entry_for(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        entry.reserve_available(),
        0,
        "a failed-open call must not steal the reserved slot"
    );
    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        snapshot.available, 0,
        "the capped slot still belongs to the other session"
    );
    assert_eq!(
        snapshot.waiting, 0,
        "a reserve call never joins the FIFO queue"
    );

    drop(busy_reserve);
    assert_eq!(
        entry.reserve_available(),
        pool::INTERNAL_RESERVE_CAPACITY,
        "the reserve is a Drop-released slot"
    );

    drop(session);
    deactivate();
}

/// The JIT prompt architect is a **capped** call: it queues like any other
/// session and must never borrow the steer/notice reserve. This drives the real
/// call site, `PromptBuilder::synthesize_with_llm`.
#[tokio::test]
async fn test_prompt_architect_call_site_is_capped_not_reserved() {
    let _serial = FAST_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let architect_reply = serde_json::json!({
        "role_name": "rust_test_specialist",
        "reasoning": "the brief is a focused test fix",
        "selected_skills": ["test engineering"],
        "allowed_tools": ["read_file", "run_command"],
        "system_prompt": "Fix the failing test with the smallest correct change."
    })
    .to_string();
    let server = backend(&architect_reply, Duration::from_millis(150)).await;
    let backend = server.uri();
    let model = "model-architect-site";

    // The only slot for this pair is busy — the architect has to queue for it.
    let held = hold_slot(backend.as_str(), model).await;

    let done = Arc::new(AtomicBool::new(false));
    let ok = Arc::new(AtomicBool::new(false));
    {
        let (done_flag, ok_flag) = (Arc::clone(&done), Arc::clone(&ok));
        let client = ChatClient::new_with_token(backend.clone(), model, "tok");
        let catalog = Catalog::new();
        let req = DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "make the failing suite green again".to_string(),
            snippets: Vec::new(),
            task_id: Some("t-001".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        tokio::spawn(async move {
            let out = PromptBuilder::synthesize_with_llm(&client, "m", &catalog, &req).await;
            ok_flag.store(out.is_ok(), Ordering::SeqCst);
            done_flag.store(true, Ordering::SeqCst);
        });
    }

    let queued = wait_until_queued(backend.as_str(), model, 1).await;
    assert_eq!(
        queued.available, 0,
        "the capped slot is taken, which is why the architect is queued"
    );
    assert_eq!(
        queued.reserve_available,
        pool::INTERNAL_RESERVE_CAPACITY,
        "a prompt-architect call must never draw on the steer/notice reserve"
    );

    // Still queued, and nothing sent: the cap — not the reserve — gates this call.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !done.load(Ordering::SeqCst),
        "the architect call must still be waiting for a capped slot"
    );
    assert_eq!(
        hits(&server).await,
        0,
        "a queued call must not have sent anything"
    );

    // An internal fast call on the same pair is unaffected by the queued
    // architect: the reserve is a separate slot.
    let fast_client = ChatClient::new_with_token(backend.clone(), model, "tok");
    let fast = tokio::time::timeout(
        Duration::from_secs(2),
        evaluate_worker_reply(
            &fast_client,
            &HarnessStats::new(),
            &notice(),
            "coder-t-001",
            "reply",
            |_| {},
        ),
    )
    .await
    .expect("a reserve call must not queue behind a queued architect")
    .expect("the evaluation succeeds");
    assert_eq!(fast.decision, "SynthesizeResponse");

    // Free the cap: the queued architect is next in line and completes.
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the architect call must complete once a capped slot frees"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        ok.load(Ordering::SeqCst),
        "blueprint synthesis must succeed once it gets its slot"
    );
    assert_eq!(
        hits(&server).await,
        2,
        "the architect request went out only after the slot freed"
    );

    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(snapshot.waiting, 0, "the queue ledger is empty again");
    assert_eq!(
        snapshot.reserve_available,
        pool::INTERNAL_RESERVE_CAPACITY,
        "the reserve was never touched by the architect"
    );

    deactivate();
}

/// Queueing must not create a new preemption surface (design §7 / G3, I5) and
/// must not disturb the existing handshake:
///
/// * a session parked on a concurrency slot is **not** in `ACTIVE_STREAMS` — a
///   steer round neither yields it nor cancels it, and it completes normally once
///   it gets its slot;
/// * the arbitrator's own reserve call is likewise not a preemption target;
/// * a genuinely running stream (a registered sink) still yields its slot and is
///   resumed through the handle, exactly as before this feature;
/// * the registry is left clean when the stream goes away.
#[tokio::test]
async fn test_queued_session_is_not_a_preemption_target_and_live_streams_still_yield() {
    let _serial = FAST_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let server = backend("queued specialist output", Duration::from_millis(100)).await;
    let backend = server.uri();
    let model = "model-queued-preempt";

    let held = hold_slot(backend.as_str(), model).await;

    // A capped specialist call parked on the slot, with its own worker token —
    // the shape a queued delegation has while the cap is saturated.
    let token = CancellationToken::new();
    let queued = {
        let token = token.clone();
        let client = ChatClient::new_with_token(backend.clone(), model, "tok");
        let req = request();
        tokio::spawn(async move {
            pool::with_call_context(
                pool::CallContext::for_agent(
                    pool::CallClass::Specialist,
                    "coder-t-001",
                    Some("t-001".to_string()),
                ),
                CURRENT_WORKER_TOKEN.scope(token, client.chat_stream(&req, |_| true)),
            )
            .await
        })
    };

    let snapshot = wait_until_queued(backend.as_str(), model, 1).await;
    assert_eq!(snapshot.available, 0);
    assert_eq!(snapshot.waiting, 1, "one session is parked on the slot");

    // The queued session is invisible to preemption: nothing is registered for
    // this model, so a steering round finds no conflicting stream to pause and
    // has no handle that could cancel the waiter.
    let handle = preempt_conflicting_stream(model, "user steering").await;
    assert!(
        matches!(handle, PreemptHandle::None),
        "a session queued for a slot must not be a preemption target"
    );
    assert!(
        !token.is_cancelled(),
        "a preemption round must not cancel a queued session"
    );
    assert!(
        !is_globally_cancelled(),
        "a preemption round must not cancel the process"
    );

    // The arbitrator itself (reserve class) runs while that session waits, and it
    // too registers no active stream.
    let arbitrator = ChatClient::new_with_token(backend.clone(), model, "tok");
    let decision = tokio::time::timeout(
        Duration::from_secs(5),
        arbitrate_steer_context_stream(&arbitrator, &HarnessStats::new(), steer_context(), |_| {}),
    )
    .await
    .expect("steering must not be stalled by a queued session")
    .expect("the arbitrator parsed a decision");
    assert_eq!(decision.decision, "RespondDirectly");
    assert!(
        matches!(
            preempt_conflicting_stream(model, "user steering").await,
            PreemptHandle::None
        ),
        "an internal fast call must not register itself as an active stream"
    );

    // The queued session was untouched, and completes once the slot frees.
    drop(held);
    let reply = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("the queued call completes after its slot frees")
        .expect("the queued task did not panic")
        .expect("chat_stream reports through its result channel");
    assert!(
        reply.is_success(),
        "a session that waited for a slot must still succeed, got {}",
        reply.outcome.as_str()
    );
    assert!(
        !token.is_cancelled(),
        "waiting for a slot must not have cancelled the worker token"
    );

    // Live-stream preemption is unregressed: a registered sink on a capped pair
    // still yields its slot and is resumed through the handle.
    let live_model = "model-live-preempt";
    let live_token = CancellationToken::new();
    let mut sink = PreemptibleStreamSink::register_full(
        "coder-t-002",
        Some("coder".to_string()),
        Some("t-002".to_string()),
        Some(live_token.clone()),
        live_model,
    );
    assert_eq!(
        sink.poll_control(),
        StreamControl::Continue,
        "no preemption is outstanding yet"
    );

    let handle_task = {
        let live_model = live_model.to_string();
        tokio::spawn(async move { preempt_conflicting_stream(&live_model, "steer now").await })
    };

    let pause_task = loop {
        if let StreamControl::Pause { user_input } = sink.poll_control() {
            assert_eq!(user_input, "steer now");
            // The sink moves into the pause task and unregisters when it returns,
            // exactly as the specialist fix loop hands it over.
            break tokio::spawn(async move { sink.on_pause("steer now").await });
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let handle = handle_task.await.expect("preemption task ran");
    assert!(
        matches!(handle, PreemptHandle::Active(_)),
        "a running stream must be preemptible"
    );
    assert!(
        !live_token.is_cancelled(),
        "a stream that acknowledges in time must not be cancelled"
    );
    handle.complete_all(PauseAction::Resume);
    let action = pause_task.await.expect("pause task ran");
    assert_eq!(
        action,
        PauseAction::Resume,
        "the stream resumes on the slot it yielded"
    );

    // Registry hygiene: once the stream is gone, nothing is preemptible.
    assert!(
        matches!(
            preempt_conflicting_stream(live_model, "steer now").await,
            PreemptHandle::None
        ),
        "the active-stream registry must not keep a finished stream"
    );

    deactivate();
}

/// Conditional stream pause on steering, end to end through the arbitrator's own
/// backend+model pair:
///
/// * no cap configured (unlimited) => pause, exactly as before;
/// * a capped pair with a free slot and nobody queued => **no pause at all**
///   (no `StreamControl::Pause`, no handshake, stream untouched);
/// * the same pair saturated => the original pause handshake, ack escalation
///   included;
/// * a queued session blocks the skip path even with a permit free.
#[tokio::test]
async fn test_pause_is_skipped_only_while_the_pair_has_spare_capacity() {
    let _serial = FAST_TEST_MUTEX.lock().await;
    reset_cancellation();

    let backend = "http://steer-conditional-pause:9101/v1";
    let model = "model-conditional-pause";

    // Drive one arbitration round that must pause, and prove the stream yields
    // its slot and is handed back.
    async fn pause_round(backend: &str, model: &str, agent_tag: &str) {
        let token = CancellationToken::new();
        let mut sink = PreemptibleStreamSink::register_full(
            agent_tag,
            Some(agent_tag.to_string()),
            Some(format!("t-{}", agent_tag)),
            Some(token.clone()),
            model,
        );
        let backend = backend.to_string();
        let model = model.to_string();
        let handle_task = tokio::spawn(async move {
            preempt_conflicting_stream_on_backend(&backend, &model, "steer now").await
        });
        let pause_task = loop {
            if let StreamControl::Pause { user_input } = sink.poll_control() {
                assert_eq!(user_input, "steer now");
                break tokio::spawn(async move { sink.on_pause("steer now").await });
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let handle = handle_task.await.expect("preemption task ran");
        assert!(
            matches!(handle, PreemptHandle::Active(_)),
            "a full or uncapped pair must still preempt the running stream"
        );
        assert!(
            !token.is_cancelled(),
            "a stream that acknowledges in time must not be cancelled"
        );
        handle.complete_all(PauseAction::Resume);
        assert_eq!(
            pause_task.await.expect("pause task ran"),
            PauseAction::Resume,
            "the stream resumes on the slot it yielded"
        );
    }

    // 1. `max_sessions` unset => unlimited => never the skip path.
    deactivate();
    assert!(
        !pool::has_spare_session_capacity(backend, model),
        "an unconfigured cap must never read as spare capacity"
    );
    pause_round(backend, model, "uncapped").await;

    // 2. A cap with two free slots => the arbitrator takes its own capacity and
    //    the running stream is never touched.
    activate(2);
    assert!(
        pool::has_spare_session_capacity(backend, model),
        "an untouched capped pair has spare capacity"
    );
    let spare_token = CancellationToken::new();
    let mut spare_sink = PreemptibleStreamSink::register_full(
        "coder-t-902",
        Some("coder".to_string()),
        Some("t-902".to_string()),
        Some(spare_token.clone()),
        model,
    );
    let handle = preempt_conflicting_stream_on_backend(backend, model, "steer now").await;
    assert!(
        matches!(handle, PreemptHandle::Running(_)),
        "spare capacity must leave the conflicting stream running instead of preempting it"
    );
    for _ in 0..5 {
        assert_eq!(
            spare_sink.poll_control(),
            StreamControl::Continue,
            "StreamControl::Pause must never be sent when pausing is skipped"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.complete_with_subtask_decision(None);
    assert!(
        !spare_token.is_cancelled(),
        "a decision with no Cancel subtask leaves the stream running"
    );
    drop(spare_sink);

    // 3. Same pair with both slots held => pause exactly as before.
    let entry = pool::entry_for(backend, model).expect("a configured cap opens a pool");
    let first = pool::wait_for_slot(&entry, None, Duration::from_secs(5), None)
        .await
        .expect("first slot");
    let second = pool::wait_for_slot(&entry, None, Duration::from_secs(5), None)
        .await
        .expect("second slot");
    assert!(
        !pool::has_spare_session_capacity(backend, model),
        "a saturated pair has no spare capacity"
    );
    pause_round(backend, model, "capped").await;

    // 4. A session queued for a freed slot still means "no spare capacity".
    drop(first);
    let ticket = pool::queue_ticket(&entry);
    assert!(
        !pool::has_spare_session_capacity(backend, model),
        "a queued session blocks the skip path even with a permit free"
    );
    drop(ticket);
    drop((second, entry));

    deactivate();
    reset_cancellation();
}
