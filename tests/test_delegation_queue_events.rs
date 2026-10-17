//! End-to-end proof that the concurrency gate's **queued / running** transitions
//! reach the delegation event stream the session loop drains — with the right
//! attribution, and nowhere else.
//!
//! The questions pinned here (design §5/§6/§7):
//! * a specialist backend call that has to wait for a slot appends
//!   `Queued { agent, task, position, of }` and, once it holds the slot, exactly
//!   one `Running`, in that order, to the stream currently being drained;
//! * with `max_sessions = 1`, **two** delegations on one `backend + model` pair run
//!   one at a time: the second is queued (and says so, with position and
//!   attribution) until the first releases its permit, and the two requests never
//!   overlap at the backend;
//! * those events name **that** delegation (its agent and plan task), taken from
//!   the installed [`pool::CallContext`];
//! * a call that never had to wait emits neither arm — an uncapped pair leaves
//!   queue state untouched, so the panel never shows a queue for work that did
//!   not queue;
//! * a call with no delegation row of its own (a manager turn, a validator pass,
//!   a reserve-class steer decision) may wait behind the very same cap and still
//!   emits nothing — otherwise the UI gains a row no lifecycle event will ever
//!   close;
//! * a cancel that lands while the call is queued produces a `Queued` and **no**
//!   `Running`: the delegation closes through the lifecycle `Failed` arm, which
//!   is what clears the queue state;
//! * with no stream registered, appending is a no-op rather than an error.
//!
//! The pool registry, the active config, the cancellation token, the delegation
//! event sink and the status sender are all process-global state, so every test
//! here takes `SINK_TEST_MUTEX`; each test uses its own mock server and model name
//! so pool entries never collide either.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use marmennill::agents::Agent;
use marmennill::config::{ConcurrencyConfig, Config, set_active};
use marmennill::llm::client::{ChatClient, ReplyOutcome, StreamedReply};
use marmennill::orchestrator::pool;
use marmennill::orchestrator::{
    CURRENT_WORKER_TOKEN, DelegationEvent, clear_delegation_event_sink, delegation_event_sink,
    push_delegation_event, reset_cancellation, set_delegation_event_sink, set_status_sender,
};
use marmennill::types::{ChatRequest, Message};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialize these tests: the pool registry, the active config, the global
/// cancellation token and the delegation event sink are process-global state.
static SINK_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// When each request landed on the mock backend. A test that needs to prove two
/// calls never overlapped reads the wire, not a counter: the gap between arrivals
/// is the evidence.
type Arrivals = Arc<std::sync::Mutex<Vec<Instant>>>;

/// Install an active config whose global cap is `max_sessions` per
/// `backend + model` pair (`None` ⇒ unlimited ⇒ no pool at all).
fn activate(max_sessions: Option<usize>) {
    let mut cfg = Config::default();
    cfg.orchestration.concurrency = ConcurrencyConfig {
        by_backend: HashMap::new(),
        max_sessions,
    };
    set_active(cfg);
}

/// Back to "no cap configured".
fn deactivate() {
    set_active(Config::default());
    clear_delegation_event_sink();
}

/// Register a fresh, test-owned delegation stream — the stand-in for the
/// `OrchestratorManager::delegation_events` vector the session loop drains.
fn install_sink() -> Arc<std::sync::Mutex<Vec<DelegationEvent>>> {
    let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
    set_delegation_event_sink(Arc::clone(&sink));
    sink
}

/// What has landed in the stream so far.
fn seen(sink: &Arc<std::sync::Mutex<Vec<DelegationEvent>>>) -> Vec<DelegationEvent> {
    sink.lock()
        .expect("the stream lock is never held across I/O")
        .clone()
}

fn sse_body(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-delegation",
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

/// A mounted backend that answers anything with `text`.
async fn backend(text: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(text)))
        .mount(&server)
        .await;
    server
}

/// Hold one slot of `backend + model` from outside the client, the way a
/// competing session would.
async fn hold_slot(backend: &str, model: &str) -> pool::SessionPermit {
    pool::acquire_session_with_wait(backend, model, None, Duration::from_secs(5), None)
        .await
        .expect("the first slot is free")
        .expect("a cap is configured, so this pair is metered")
}

/// How many requests actually reached the mock backend.
async fn hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("wiremock records its requests")
        .len()
}

/// Poll the queue ledger until `want` sessions are in line, so "this call is
/// genuinely waiting" is a fact and not a race.
async fn wait_until_queued(backend: &str, model: &str, want: usize) -> pool::PoolSnapshot {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = pool::pool_snapshot(backend, model).expect("a capped pair has a pool");
        if snapshot.waiting >= want || Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Poll until the FIFO queue for this key is empty: a waiter that was cancelled
/// or whose future was dropped must have handed its seat back.
async fn wait_until_queue_clear(backend: &str, model: &str) -> pool::PoolSnapshot {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = pool::pool_snapshot(backend, model).expect("a capped pair has a pool");
        if snapshot.waiting == 0 || Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One specialist delegation against `backend + model`, installed with the
/// [`pool::CallContext`] a real specialist turn installs for itself. This is the
/// shape a delegated call takes on the way into [`ChatClient::chat_stream`] —
/// used where a test needs **two genuine calls** rather than an artificially
/// parked slot.
async fn delegate(backend: String, model: String, task: String) -> StreamedReply {
    let client = ChatClient::new(backend, model).with_queue_wait_secs(20);
    let req = request();
    let context = pool::CallContext::for_agent(pool::CallClass::Specialist, "coder", Some(task));
    pool::with_call_context(context, client.chat_stream(&req, |_| true))
        .await
        .expect("a delegation queued for a slot must be served, never failed")
}

/// Poll until `want` requests have landed on the mock backend. "The first call is
/// genuinely executing there" has to be a fact before any statement about the
/// second call means anything.
async fn wait_until_arrivals(arrivals: &Arrivals, want: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        {
            let stamp = arrivals
                .lock()
                .expect("test-owned lock, never held across I/O");
            if stamp.len() >= want {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A specialist call that must wait for the only slot reports its place in line,
/// keeps that position honest as the queue moves, and then reports the
/// queued → running edge — in that order, with the delegation's own attribution.
#[tokio::test]
async fn queued_specialist_call_emits_queued_then_running() {
    let _serial = SINK_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = backend("queued specialist answer").await;
    let url = server.uri();
    let model = "model-deleg-queued";
    let held = hold_slot(url.as_str(), model).await;
    let sink = install_sink();

    let client = ChatClient::new(url.clone(), model).with_queue_wait_secs(20);
    let req = request();
    let context = pool::CallContext::for_agent(
        pool::CallClass::Specialist,
        "coder",
        Some("t-011".to_string()),
    );
    let call = pool::with_call_context(context, client.chat_stream(&req, |_| true));
    tokio::pin!(call);

    let early = tokio::time::timeout(Duration::from_millis(400), &mut call).await;
    assert!(
        early.is_err(),
        "the call must still be waiting for the slot"
    );
    assert_eq!(hits(&server).await, 0, "a queued call must not be sent");
    let queued = wait_until_queued(url.as_str(), model, 1).await;
    assert_eq!(queued.waiting, 1, "the call is in line, got {queued:?}");

    let first = seen(&sink);
    assert!(
        !first.is_empty(),
        "a call that waits must report its place in line"
    );
    for event in &first {
        match event {
            DelegationEvent::Queued {
                agent,
                task,
                position,
                of,
            } => {
                assert_eq!(*agent, Agent::Coder, "the event names the delegation");
                assert_eq!(task.as_deref(), Some("t-011"));
                assert!(
                    *of >= 1 && *position >= 1 && *position <= *of,
                    "position {position} of {of} must be a real FIFO rank"
                );
            }
            other => panic!("only Queued arms may precede the slot, got {other:?}"),
        }
    }

    // RAII: releasing the holder hands the slot to the session in line.
    drop(held);
    let reply = tokio::time::timeout(Duration::from_secs(15), &mut call)
        .await
        .expect("the queued call must run once the slot is free")
        .expect("the queued call must succeed");
    assert!(reply.is_success(), "got {}", reply.outcome.as_str());
    assert_eq!(hits(&server).await, 1, "exactly one request was sent");

    let all = seen(&sink);
    let running_at = all
        .iter()
        .position(|event| matches!(event, DelegationEvent::Running { .. }))
        .expect("a call that waited must also report the queued → running edge");
    let last_queued = all
        .iter()
        .rposition(|event| matches!(event, DelegationEvent::Queued { .. }))
        .expect("the Running arm is only emitted after a Queued one");
    assert!(
        running_at > last_queued,
        "the queue state a Queued arm installed must be taken back down after it: {all:?}"
    );
    assert_eq!(
        all.iter()
            .filter(|event| matches!(event, DelegationEvent::Running { .. }))
            .count(),
        1,
        "exactly one running edge per call, got {all:?}"
    );
    if let DelegationEvent::Running { agent, task } = &all[running_at] {
        assert_eq!(*agent, Agent::Coder);
        assert_eq!(task.as_deref(), Some("t-011"));
    }
    assert!(
        !all.iter().any(|event| {
            matches!(
                event,
                DelegationEvent::Started { .. }
                    | DelegationEvent::Completed { .. }
                    | DelegationEvent::Failed { .. }
            )
        }),
        "the gate reports slot state only; the lifecycle arms belong to delegate(): {all:?}"
    );

    deactivate();
}

/// AC1 measured end to end: **two real delegations** hitting one
/// `backend + model` pair at `max_sessions = 1`.
///
/// Every other test in this file parks a slot from the outside with
/// [`hold_slot`]. Here there is no artificial holder: the session occupying the
/// only slot is itself a `chat_stream` delegation, so this pins the whole claim —
/// the second delegation waits for the first one's permit, exposes its queued
/// state (position and attribution) while it waits, and the two completions never
/// overlap on the wire.
#[tokio::test]
async fn two_delegations_on_one_capped_pair_never_overlap_and_the_second_shows_queued() {
    let _serial = SINK_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    // How long the backend keeps each request busy (delayed response), and the
    // instant every request landed. "Did these two run at the same time?" is then
    // a measurement of the wire, not an inference from pool counters.
    const EXEC: Duration = Duration::from_millis(1500);

    let server = MockServer::start().await;
    let arrivals: Arrivals = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stamp = Arc::clone(&arrivals);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_req: &wiremock::Request| {
            let index = {
                let mut landed = stamp
                    .lock()
                    .expect("test-owned lock, never held across I/O");
                landed.push(Instant::now());
                landed.len()
            };
            ResponseTemplate::new(200)
                .set_delay(EXEC)
                .set_body_string(sse_body(&format!("answer-{index}")))
        })
        .mount(&server)
        .await;

    let url = server.uri();
    let model = "model-two-delegations";
    let sink = install_sink();

    // Queue transitions travel as status traffic too (design §6); capture them so
    // the queued line is checked for the delegation it names, not just its presence.
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    let first = tokio::spawn(delegate(
        url.clone(),
        model.to_string(),
        "t-014-first".to_string(),
    ));
    assert!(
        wait_until_arrivals(&arrivals, 1).await,
        "the first delegation must reach the backend and occupy the only slot"
    );

    let second = tokio::spawn(delegate(
        url.clone(),
        model.to_string(),
        "t-014-second".to_string(),
    ));

    // The second delegation has to wait: no slot, no request, a seat in the FIFO —
    // all observed while the first is still executing (its response is still
    // ~1.5 s away).
    let queued = wait_until_queued(url.as_str(), model, 1).await;
    assert_eq!(
        queued.waiting, 1,
        "the second delegation must be queued for this pair, got {queued:?}"
    );
    assert_eq!(
        queued.available, 0,
        "the first delegation must still hold the only slot of the capped pair"
    );
    assert_eq!(
        hits(&server).await,
        1,
        "a queued delegation must not have reached the backend"
    );
    assert_eq!(
        arrivals
            .lock()
            .expect("test-owned lock, never held across I/O")
            .len(),
        1,
        "the capped pair must be executing one delegation, not two"
    );

    // Queued state is exposed on the delegation stream, attributed to the
    // delegation that is actually waiting — and to nothing else.
    let while_queued = seen(&sink);
    assert!(
        !while_queued.is_empty(),
        "a delegation waiting for a slot must expose its queued state"
    );
    for event in &while_queued {
        match event {
            DelegationEvent::Queued {
                agent,
                task,
                position,
                of,
            } => {
                assert_eq!(*agent, Agent::Coder, "the event names the delegation");
                assert_eq!(
                    task.as_deref(),
                    Some("t-014-second"),
                    "queue state belongs to the delegation that is waiting"
                );
                assert_eq!(
                    (*position, *of),
                    (1, 1),
                    "one delegation in line, next up, got {event:?}"
                );
            }
            other => panic!("a waiting delegation reports Queued and nothing else, got {other:?}"),
        }
    }

    let mut statuses = Vec::new();
    while let Ok(status) = status_rx.try_recv() {
        statuses.push(status);
    }
    assert!(
        statuses
            .iter()
            .any(|status| status.contains("queued") && status.contains("t-014-second")),
        "the queue line must name the delegation that is waiting, got {statuses:?}"
    );

    // The first delegation ends on its own; its RAII release is what lets the
    // queued one through. Serving order is visible in the answer the backend gave
    // each call, so a broken cap shows up as swapped content too.
    let first_reply = tokio::time::timeout(Duration::from_secs(10), first)
        .await
        .expect("the first delegation must finish")
        .expect("the first delegation must not panic");
    assert!(
        first_reply.is_success(),
        "got {}",
        first_reply.outcome.as_str()
    );
    assert_eq!(
        first_reply.content, "answer-1",
        "the delegation that took the slot first was served first"
    );

    let second_reply = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .expect("the queued delegation must run once the first permit is released")
        .expect("the second delegation must not panic");
    assert!(
        second_reply.is_success(),
        "got {}",
        second_reply.outcome.as_str()
    );
    assert_eq!(
        second_reply.content, "answer-2",
        "the queued delegation was served second, after the slot came free"
    );

    // The non-concurrency proof, on the wire: the second request landed only
    // after the first request's whole busy window had closed. An unserialized pair
    // would land request 2 within milliseconds of request 1.
    let landed = arrivals
        .lock()
        .expect("test-owned lock, never held across I/O")
        .clone();
    assert_eq!(landed.len(), 2, "both delegations must have been served");
    let gap = landed[1].duration_since(landed[0]);
    assert!(
        gap >= EXEC,
        "the two delegations ran concurrently on the capped pair: request 2 landed \
         {gap:?} after request 1, which the backend held busy for {EXEC:?}",
    );
    assert_eq!(
        hits(&server).await,
        2,
        "exactly one request per delegation — no overlap and no retry"
    );

    let all = seen(&sink);
    assert!(
        all.iter()
            .any(|event| matches!(event, DelegationEvent::Queued { .. })),
        "the waiting delegation must have reported its place in line, got {all:?}"
    );
    assert_eq!(
        all.iter()
            .filter(|event| matches!(event, DelegationEvent::Running { .. }))
            .count(),
        1,
        "exactly one queued → running edge, for the delegation that waited: {all:?}"
    );
    assert!(
        !all.iter().any(|event| match event {
            DelegationEvent::Queued { task, .. } | DelegationEvent::Running { task, .. } => {
                task.as_deref() == Some("t-014-first")
            }
            _ => false,
        }),
        "a delegation that never waited must gain no queue state, got {all:?}"
    );

    let settled = pool::pool_snapshot(url.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        settled.available, 1,
        "both permits are back in the pool once both delegations returned"
    );
    assert_eq!(settled.waiting, 0, "no queue seat may linger");

    deactivate();
}

/// No cap ⇒ no queueing ⇒ no queue events at all, even for an attributed
/// specialist call. Queue state is never invented for a call that did not wait.
#[tokio::test]
async fn uncapped_specialist_call_emits_no_queue_events() {
    let _serial = SINK_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(None);

    let server = backend("unmetered specialist answer").await;
    let url = server.uri();
    let model = "model-deleg-uncapped";
    let sink = install_sink();

    let client = ChatClient::new(url.clone(), model);
    let req = request();
    let context = pool::CallContext::for_agent(
        pool::CallClass::Specialist,
        "coder",
        Some("t-011".to_string()),
    );
    let reply = pool::with_call_context(context, client.chat_stream(&req, |_| true))
        .await
        .expect("an uncapped call must complete");
    assert!(reply.is_success(), "got {}", reply.outcome.as_str());

    assert!(
        seen(&sink).is_empty(),
        "a call that never waited must not touch queue state, got {:?}",
        seen(&sink)
    );

    deactivate();
}

/// A call that has no delegation row of its own must never produce a queue
/// event — even when it waits behind the very same saturated cap. Manager turns
/// and validator passes are capped classes, so they really do queue here; the
/// reserve classes must not emit either.
#[tokio::test]
async fn a_call_without_a_delegation_row_queues_without_emitting_events() {
    let _serial = SINK_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = backend("untracked answer").await;
    let url = server.uri();

    // (class, agent label, whether this class must queue behind the cap)
    let cases: [(pool::CallClass, &str, bool); 3] = [
        (pool::CallClass::ManagerTurn, pool::MANAGER_AGENT, true),
        (pool::CallClass::Validator, "validator-coder", true),
        (pool::CallClass::SteerArbitrator, "coder", false),
    ];

    for (class, agent, must_queue) in cases {
        let model = format!("model-deleg-{}", class.label());
        let held = hold_slot(url.as_str(), model.as_str()).await;
        let sink = install_sink();

        let client = ChatClient::new(url.clone(), model.as_str()).with_queue_wait_secs(20);
        let req = request();
        let context = pool::CallContext::for_agent(class, agent, Some("t-011".to_string()));

        if must_queue {
            let token = CancellationToken::new();
            let canceller = {
                let token = token.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    token.cancel();
                })
            };
            let call = CURRENT_WORKER_TOKEN.scope(
                token.clone(),
                pool::with_call_context(context, client.chat_stream(&req, |_| true)),
            );
            tokio::pin!(call);

            let early = tokio::time::timeout(Duration::from_millis(200), &mut call).await;
            assert!(
                early.is_err(),
                "a capped {class:?} call must wait behind the saturated cap"
            );
            let queued = wait_until_queued(url.as_str(), model.as_str(), 1).await;
            assert_eq!(
                queued.waiting, 1,
                "{class:?} really is queued for {model}, got {queued:?}"
            );
            assert!(
                seen(&sink).is_empty(),
                "no queue event for a {class:?} call: it has no delegation row of its own, got {:?}",
                seen(&sink)
            );

            let reply = tokio::time::timeout(Duration::from_secs(10), &mut call)
                .await
                .expect("the cancelled wait must exit instead of hanging")
                .expect("cancellation is a reply, never an error");
            canceller.await.expect("the canceller ran");
            assert_eq!(reply.outcome, ReplyOutcome::Cancelled);
            assert!(
                seen(&sink).is_empty(),
                "a waiter that never held a slot must report nothing — no Queued, \
                 no Running — for a {class:?} call, got {:?}",
                seen(&sink)
            );
            assert_eq!(
                wait_until_queue_clear(url.as_str(), model.as_str())
                    .await
                    .waiting,
                0,
                "a cancelled waiter must leave the FIFO queue for the next caller"
            );
        } else {
            // A reserve-class call draws on the internal reserve instead of the
            // cap, and must emit nothing whichever way that wait goes.
            let call = pool::with_call_context(context, client.chat_stream(&req, |_| true));
            tokio::pin!(call);
            let reply = tokio::time::timeout(Duration::from_secs(10), &mut call)
                .await
                .expect("a reserve-class call must not block behind capped work")
                .expect("the call must not fail");
            assert!(reply.is_success(), "got {}", reply.outcome.as_str());
            assert!(
                seen(&sink).is_empty(),
                "a reserve-class {class:?} call must emit no queue events, got {:?}",
                seen(&sink)
            );
        }

        drop(held);
    }

    deactivate();
}

/// A cancel that lands while the call is queued: the stream keeps the `Queued`
/// arm (the delegation was waiting) and gains **no** `Running` arm — the
/// delegation closes through the lifecycle `Failed` arm, which is what clears
/// the queue state in the panel.
#[tokio::test]
async fn cancel_while_queued_emits_no_running_edge() {
    let _serial = SINK_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = backend("never sent").await;
    let url = server.uri();
    let model = "model-deleg-cancel";
    let held = hold_slot(url.as_str(), model).await;
    let sink = install_sink();

    let token = CancellationToken::new();
    let canceller = {
        let token = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            token.cancel();
        })
    };

    let client = ChatClient::new(url.clone(), model).with_queue_wait_secs(20);
    let req = request();
    let context = pool::CallContext::for_agent(
        pool::CallClass::Specialist,
        "coder",
        Some("t-011".to_string()),
    );
    let reply = tokio::time::timeout(
        Duration::from_secs(10),
        CURRENT_WORKER_TOKEN.scope(
            token.clone(),
            pool::with_call_context(context, client.chat_stream(&req, |_| true)),
        ),
    )
    .await
    .expect("a cancelled wait must exit instead of hanging")
    .expect("cancellation is a reply, never an error");
    canceller.await.expect("the canceller ran");

    assert_eq!(
        reply.outcome,
        ReplyOutcome::Cancelled,
        "a cancelled wait is reported as cancelled"
    );
    assert_eq!(hits(&server).await, 0, "nothing was ever sent");

    let all = seen(&sink);
    assert!(
        all.iter()
            .any(|event| matches!(event, DelegationEvent::Queued { .. })),
        "the delegation was queued, and that must be visible"
    );
    assert!(
        !all.iter()
            .any(|event| matches!(event, DelegationEvent::Running { .. })),
        "a call that never held a slot must never report Running, got {all:?}"
    );
    assert_eq!(
        wait_until_queue_clear(url.as_str(), model).await.waiting,
        0,
        "a cancelled waiter must have left the queue"
    );

    drop(held);
    deactivate();
}

/// The stream the drain registered is the one the gate appends to, order is
/// preserved, and with no stream registered an append is a no-op rather than an
/// error — queue traffic is presentation state, never control flow.
#[tokio::test]
async fn the_registered_stream_receives_events_in_order_and_absence_is_a_no_op() {
    let _serial = SINK_TEST_MUTEX.lock().await;
    clear_delegation_event_sink();
    assert!(delegation_event_sink().is_none());
    assert!(
        !push_delegation_event(DelegationEvent::Queued {
            agent: Agent::Coder,
            task: Some("t-011".to_string()),
            position: 1,
            of: 1,
        }),
        "no registered stream means a no-op, not an error"
    );

    let sink = install_sink();
    assert!(
        Arc::ptr_eq(
            &delegation_event_sink().expect("the registered stream is observable"),
            &sink
        ),
        "callers must append to the stream the session is draining"
    );

    // The authoritative sequence the UI fold depends on.
    for event in [
        DelegationEvent::Started {
            agent: Agent::Coder,
            task: Some("t-011".to_string()),
        },
        DelegationEvent::Queued {
            agent: Agent::Coder,
            task: Some("t-011".to_string()),
            position: 2,
            of: 3,
        },
        DelegationEvent::Queued {
            agent: Agent::Coder,
            task: Some("t-011".to_string()),
            position: 1,
            of: 2,
        },
        DelegationEvent::Running {
            agent: Agent::Coder,
            task: Some("t-011".to_string()),
        },
        DelegationEvent::Completed {
            agent: Agent::Coder,
            task: Some("t-011".to_string()),
        },
    ] {
        assert!(
            push_delegation_event(event),
            "a registered stream must accept every arm"
        );
    }

    let all = seen(&sink);
    assert!(
        matches!(
            all.as_slice(),
            [
                DelegationEvent::Started { .. },
                DelegationEvent::Queued { .. },
                DelegationEvent::Queued { .. },
                DelegationEvent::Running { .. },
                DelegationEvent::Completed { .. }
            ]
        ),
        "the stream must preserve the authoritative order, got {all:?}"
    );

    clear_delegation_event_sink();
    assert!(delegation_event_sink().is_none());
    assert!(
        !push_delegation_event(DelegationEvent::Running {
            agent: Agent::Coder,
            task: Some("t-011".to_string()),
        }),
        "a finished session must not leave a dangling stream behind"
    );
}
