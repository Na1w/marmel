//! Concurrency-slot enforcement at the one LLM chokepoint, `ChatClient::chat_stream`.
//!
//! The gate is only observable end-to-end, so these tests drive the real client
//! against a wiremock backend with `[orchestration.concurrency]` caps installed.
//! The questions they pin:
//! * an uncapped `backend + model` pair never touches a semaphore at all;
//! * with `[orchestration.concurrency]` unset, concurrent calls on one pair stay
//!   parallel and unreported — nothing is queued, delayed, or metered;
//! * `max_sessions = 1` serializes a second call until the first holder drops;
//! * the slot is held across **every** retry attempt of one call, not re-taken
//!   per attempt;
//! * an expired queue wait is terminal: the request is never sent, never retried,
//!   and the failure names the cap and the key;
//! * cancellation during the wait exits with a `Cancelled` reply, not a hang and
//!   not an error, and hands the queue over: a caller queued **behind** the
//!   cancelled waiter is still served once the slot frees;
//! * a reserve-class call (steer arbitrator / notice evaluation) is never queued
//!   behind a saturated cap (design §5).
//!
//! The pool registry, the active config and the process-wide cancellation token
//! are all process-global, so every test here takes `POOL_TEST_MUTEX` and installs
//! the config it needs. This file is its own test binary, so it cannot disturb the
//! library unit tests' pool/config state.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use marmennill::config::{ConcurrencyConfig, Config, set_active};
use marmennill::llm::client::{ChatClient, ReplyOutcome, TerminalCause};
use marmennill::orchestrator::pool;
use marmennill::orchestrator::{CURRENT_WORKER_TOKEN, reset_cancellation, set_status_sender};
use marmennill::types::{ChatRequest, Message};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialize these tests: pool entries, the active config, and the global
/// cancellation token are process-global state.
static POOL_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// When each request landed on the mock backend. "Did these calls run at the
/// same time?" is answered from the wire — the spread between arrivals — not
/// from a pool counter.
type Arrivals = Arc<std::sync::Mutex<Vec<Instant>>>;

/// Install an active config whose global cap is `max_sessions`
/// (`None` ⇒ unlimited ⇒ no pool at all).
fn activate(max_sessions: Option<usize>) {
    let mut cfg = Config::default();
    cfg.orchestration.concurrency = ConcurrencyConfig {
        by_backend: HashMap::new(),
        max_sessions,
    };
    set_active(cfg);
}

/// Back to "no cap configured" once a test is done.
fn deactivate() {
    set_active(Config::default());
}

fn sse_body(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-pool",
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

/// Hold the only slot for `backend + model` from outside the client, the way a
/// concurrent session would.
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

/// Poll the arrival stamps until `want` requests have landed (test-owned lock,
/// so this never contends with the backend's own bookkeeping).
async fn wait_until_arrivals(arrivals: &Arrivals, want: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        {
            let landed = arrivals
                .lock()
                .expect("test-owned lock, never held across I/O");
            if landed.len() >= want {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// No cap ⇒ no pool. The unlimited path must be untouched by this feature: not
/// one pool entry is created, and the call completes normally.
#[tokio::test]
async fn test_uncapped_pair_never_touches_the_pool() {
    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(None);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("unmetered answer")))
        .mount(&server)
        .await;

    let backend = server.uri();
    let model = "model-uncapped";
    let pools_before = pool::open_pool_count();

    let client = ChatClient::new(backend.clone(), model);
    let reply = client
        .chat_stream(&request(), |_| true)
        .await
        .expect("an uncapped call must complete exactly as before");
    assert!(
        reply.is_success(),
        "got {} / {:?}",
        reply.outcome.as_str(),
        reply.cause
    );
    assert_eq!(hits(&server).await, 1);

    // Nothing was metered: no entry was opened, and the "do I meter this call at
    // all" answer is still "no".
    assert_eq!(
        pool::open_pool_count(),
        pools_before,
        "an uncapped call must not open a pool entry"
    );
    assert!(pool::pool_for(&backend, model).is_none());
    assert!(pool::pool_snapshot(&backend, model).is_none());

    deactivate();
}

/// Acceptance (t-018): **unset means unlimited, and unlimited means untouched.**
///
/// Three concurrent `chat_stream` calls on one `backend + model` pair with no
/// `[orchestration.concurrency]` block installed at all, against a backend that
/// keeps every request busy for `BUSY` (`ResponseTemplate::set_delay`) and stamps
/// the instant each request landed. The spread between arrivals is the evidence:
///
/// * all three requests are on the wire inside **one** busy window — under any
///   cap, request 2 could not land before request 1's window closed, so the
///   first→last spread would be ≥ 2 × `BUSY`;
/// * the trio returns in roughly one busy window, not three: no call is queued
///   or delayed by the concurrency machinery;
/// * each call got its own answer, one request per call (no queueing, no retry);
/// * the status bus carries no `queued` line — an uncapped call has no queue
///   state to report;
/// * none of it opened a pool entry, and the pair is still unmetered afterwards.
#[tokio::test]
async fn test_unset_concurrency_keeps_calls_parallel_and_never_queued() {
    /// How long the mock backend keeps each request busy.
    const BUSY: Duration = Duration::from_millis(900);
    const CALLERS: usize = 3;

    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(None);

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
                .set_delay(BUSY)
                .set_body_string(sse_body(&format!("unmetered answer-{index}")))
        })
        .mount(&server)
        .await;

    let backend = server.uri();
    let model = "model-unlimited";
    let pools_before = pool::open_pool_count();

    // Queue transitions travel as status lines (design §6): install the bus so
    // "nothing was ever reported as queued" is a measurement, not an assumption.
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    let started = Instant::now();
    let mut callers = Vec::new();
    for _ in 0..CALLERS {
        let client = ChatClient::new(backend.clone(), model);
        callers.push(tokio::spawn(async move {
            client.chat_stream(&request(), |_| true).await
        }));
    }

    // Every request must land while the first one is still busy — measured on the
    // wire, not inferred from a counter.
    assert!(
        wait_until_arrivals(&arrivals, CALLERS).await,
        "with no cap configured, all {CALLERS} calls on one pair must reach the backend \
         inside one {BUSY:?} busy window"
    );
    let spread = {
        let landed = arrivals
            .lock()
            .expect("test-owned lock, never held across I/O");
        assert_eq!(
            landed.len(),
            CALLERS,
            "one request per call — no queueing, no retries"
        );
        landed[landed.len() - 1].duration_since(landed[0])
    };
    assert!(
        spread < BUSY,
        "first and last arrival must sit inside one {BUSY:?} busy window, i.e. all \
         {CALLERS} uncapped calls ran at once; measured a {spread:?} spread, which means \
         a call was queued behind another"
    );

    let mut replies = Vec::new();
    for caller in callers {
        let reply = tokio::time::timeout(Duration::from_secs(10), caller)
            .await
            .expect("an uncapped call must never be held up by the pool")
            .expect("the call task must not panic")
            .expect("an uncapped call must not fail");
        assert!(
            reply.is_success(),
            "got {} / {:?}",
            reply.outcome.as_str(),
            reply.cause
        );
        replies.push(reply);
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < BUSY.mul_f64(2.0),
        "three uncapped calls against a {BUSY:?} backend must finish in ~one busy \
         window, not {CALLERS} serialized ones (>= {:?}); took {elapsed:?}",
        BUSY.mul_f64(CALLERS as f64)
    );
    assert_eq!(hits(&server).await, CALLERS, "exactly one request per call");

    let answers: Vec<String> = replies.iter().map(|reply| reply.content.clone()).collect();
    let distinct: std::collections::HashSet<&str> = answers.iter().map(String::as_str).collect();
    assert_eq!(
        distinct.len(),
        CALLERS,
        "every parallel call got its own answer from the backend, got {answers:?}"
    );

    let statuses = drain_statuses(&mut status_rx);
    assert!(
        statuses.iter().all(|line| !line.contains("queued")),
        "an uncapped call has no queue to report, got: {statuses:?}"
    );

    // Parallel does not mean managed: nothing anywhere was metered.
    assert_eq!(
        pool::open_pool_count(),
        pools_before,
        "uncapped calls must not open a pool entry"
    );
    assert!(pool::pool_for(&backend, model).is_none());
    assert!(pool::pool_snapshot(&backend, model).is_none());

    deactivate();
}

/// `max_sessions = 1`: the second call queues instead of running, sends nothing
/// while queued, then runs the moment the holder drops its permit (RAII release).
#[tokio::test]
async fn test_max_sessions_one_serializes_second_call_until_slot_drops() {
    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("queued answer")))
        .mount(&server)
        .await;

    let backend = server.uri();
    let model = "model-cap";
    let held = hold_slot(backend.as_str(), model).await;
    assert_eq!(
        held.available_permits(),
        0,
        "the cap is 1, so nothing is free"
    );

    // Queue transitions travel as status traffic so a waiting session does not
    // falsely trip the idle watchdogs (design §6).
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    let client = ChatClient::new(backend.clone(), model);
    let req = request();
    let call = client.chat_stream(&req, |_| true);
    tokio::pin!(call);

    let early = tokio::time::timeout(Duration::from_millis(250), &mut call).await;
    assert!(
        early.is_err(),
        "the second call must wait for the slot, not run alongside the holder"
    );
    assert_eq!(
        hits(&server).await,
        0,
        "a queued session must not have reached the backend"
    );

    let mut statuses = Vec::new();
    while let Ok(status) = status_rx.try_recv() {
        statuses.push(status);
    }
    assert!(
        statuses.iter().any(|status| status.contains("queued")),
        "queueing must be reported on the status bus, got: {statuses:?}"
    );

    // RAII: releasing the holder hands the slot to the session in line.
    drop(held);
    let reply = tokio::time::timeout(Duration::from_secs(10), &mut call)
        .await
        .expect("the queued call must run once the slot is free")
        .expect("the queued call must succeed");
    assert!(
        reply.is_success(),
        "got {} / {:?}",
        reply.outcome.as_str(),
        reply.cause
    );
    assert_eq!(hits(&server).await, 1, "exactly one request was sent");

    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        snapshot.available, 1,
        "the slot must be back in the pool once chat_stream returned"
    );
    assert_eq!(snapshot.waiting, 0, "no waiter may linger in the queue");

    deactivate();
}

/// One permit per `chat_stream` call, covering the whole retry loop: at the
/// moment attempt 2 reaches the backend, the same call must still be holding its
/// slot rather than having released it between attempts.
#[tokio::test]
async fn test_slot_is_held_across_every_attempt_of_one_call() {
    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    // Free permits observed from inside attempt 2 (`usize::MAX` = never observed).
    let free_during_retry = Arc::new(AtomicUsize::new(usize::MAX));
    let backend = server.uri();
    let model = "model-retry".to_string();

    let attempts_hook = Arc::clone(&attempts);
    let recorded_hook = Arc::clone(&free_during_retry);
    let backend_hook = backend.clone();
    let model_hook = model.clone();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_req: &wiremock::Request| {
            let attempt = attempts_hook.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                // Transient: the shared retry policy does take another attempt.
                return ResponseTemplate::new(503).set_body_string("backend busy");
            }
            let free = pool::pool_snapshot(&backend_hook, &model_hook)
                .map(|snapshot| snapshot.available)
                .unwrap_or(usize::MAX);
            recorded_hook.store(free, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_string(sse_body("answer after retry"))
        })
        .mount(&server)
        .await;

    let client = ChatClient::new(backend.clone(), model.clone());
    let reply = client
        .chat_stream(&request(), |_| true)
        .await
        .expect("the retry must still complete the call");
    assert!(
        reply.is_success(),
        "got {} / {:?}",
        reply.outcome.as_str(),
        reply.cause
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "one retry was taken");
    assert_eq!(
        free_during_retry.load(Ordering::SeqCst),
        0,
        "the retrying call must still hold its slot on attempt 2 — a per-attempt \
         acquire would let it (and anyone else) leapfrog the queue"
    );
    assert_eq!(
        pool::pool_snapshot(&backend, &model)
            .expect("a capped pair has a pool")
            .available,
        1,
        "the slot returns to the pool when chat_stream returns"
    );

    deactivate();
}

/// An expired queue wait is terminal: the failure names the cap and the key, the
/// request is never sent (so never retried), and no slot or queue seat leaks.
#[tokio::test]
async fn test_queue_timeout_fails_terminal_and_never_sends() {
    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("never sent")))
        .mount(&server)
        .await;

    let backend = server.uri();
    let model = "model-timeout";
    let held = hold_slot(backend.as_str(), model).await;

    // The 900 s production bound, overridden so the test can exercise it.
    let client = ChatClient::new(backend.clone(), model).with_queue_wait_secs(0);
    let started = Instant::now();
    let error = client
        .chat_stream(&request(), |_| true)
        .await
        .expect_err("an expired queue wait must fail this backend call");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the queue bound must be honoured, waited {:?}",
        started.elapsed()
    );

    let message = error.to_string();
    for expected in ["cap 1", backend.as_str(), model] {
        assert!(
            message.contains(expected),
            "a queue timeout must name {expected:?} so the failed call is \
             attributable, got: {message}"
        );
    }
    assert_eq!(
        hits(&server).await,
        0,
        "a queue timeout must never reach the backend and must never be retried"
    );

    // Releasing the competing holder first: what must stay true is that the
    // timed-out call neither left a seat in the queue nor took a slot.
    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        snapshot.waiting, 0,
        "a timed-out waiter must have left the FIFO queue"
    );
    drop(held);
    let released = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        released.available, 1,
        "a timed-out wait must not have taken a slot"
    );

    deactivate();
}

/// Acceptance (t-019): **the queue clock and the per-stream read clock never merge.**
///
/// A call that spends real time waiting for a slot must still get its **full**
/// per-stream read budget once it runs: the initial-response watchdog is armed at
/// send time inside the attempt, not at queue entry. The timings are chosen so that
/// a shared clock is unambiguously fatal rather than merely slow:
///
/// * the only slot is held for `HELD` (2 s) ⇒ the call is genuinely queued (pinned
///   by `waiting == 1` on the pool snapshot and by zero hits on the backend);
/// * the mock backend then stays silent for `SILENCE` (2.6 s) before the first byte;
/// * the per-stream initial-response watchdog is `READ_SECS` (3 s), and the queue
///   bound is deliberately far larger (30 s), so **only** a stream watchdog can
///   explain a failure here.
///
/// Charged onto one clock: 2 s + 2.6 s = 4.6 s > 3 s ⇒ first-event timeout, a retry,
/// and no clean answer. Kept separate: 2.6 s < 3 s ⇒ exactly one request and a
/// complete reply. The wait is also reported as its own measured duration
/// (`backend slot acquired after Ns`), which is what keeps a queued session from
/// falsely tripping the idle watchdogs (design §6).
#[tokio::test]
async fn test_queue_wait_does_not_consume_the_stream_read_watchdog() {
    const HELD: Duration = Duration::from_millis(2_000);
    const SILENCE: Duration = Duration::from_millis(2_600);
    const READ_SECS: u64 = 3;

    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(SILENCE)
                .set_body_string(sse_body("answer after queueing")),
        )
        .mount(&server)
        .await;

    let backend = server.uri();
    let model = "model-read-budget";
    let held = hold_slot(backend.as_str(), model).await;
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    let started = Instant::now();
    // The holder is released from its own task so the waiter genuinely sits in the
    // queue for ~HELD before it can send.
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(HELD).await;
        drop(held);
    });

    let client = ChatClient::new(backend.clone(), model)
        .with_queue_wait_secs(30)
        .with_initial_timeout_secs(READ_SECS);
    let req = request();
    let call = client.chat_stream(&req, |_| true);
    tokio::pin!(call);

    // Still queued, nothing sent: the read watchdog must not be burning yet.
    let early = tokio::time::timeout(Duration::from_millis(600), &mut call).await;
    assert!(
        early.is_err(),
        "the call must wait for the slot, not run alongside the holder"
    );
    assert_eq!(
        pool::pool_snapshot(backend.as_str(), model)
            .expect("a capped pair has a pool")
            .waiting,
        1,
        "the call must be sitting in the queue at this point"
    );
    assert_eq!(
        hits(&server).await,
        0,
        "a queued session must not have reached the backend"
    );

    let reply = tokio::time::timeout(Duration::from_secs(15), &mut call)
        .await
        .expect(
            "the queued call must run once the slot is free: a queue wait must not \
             eat the per-stream read window",
        )
        .expect("a queue wait must not fail the backend call");
    let elapsed = started.elapsed();
    releaser.await.expect("the releasing task must not panic");

    assert!(
        reply.is_success(),
        "a call that waited for a slot must still get its full {READ_SECS}s read \
         budget: got {} / {:?}",
        reply.outcome.as_str(),
        reply.cause
    );
    assert_eq!(
        reply.content, "answer after queueing",
        "the queued call must get its own answer"
    );
    assert_eq!(
        hits(&server).await,
        1,
        "no stream watchdog may have fired while queued, so there must be exactly \
         one request and no retry"
    );

    // The wait is accounted for on its own clock: the acquisition line reports the
    // queue duration separately from any stream deadline, and the queue wait plus
    // the backend silence together exceed the read bound.
    let statuses = drain_statuses(&mut status_rx);
    let acquired = statuses
        .iter()
        .find(|status| status.contains("backend slot acquired after"))
        .unwrap_or_else(|| {
            panic!("the queue wait must be reported on its own clock, got: {statuses:?}")
        });
    assert!(
        acquired.contains("after 1s") || acquired.contains("after 2s"),
        "the reported wait must be the queue duration (~{:?}), got {acquired:?}",
        HELD
    );
    assert!(
        HELD + SILENCE > Duration::from_secs(READ_SECS),
        "test arithmetic must exceed a shared clock: {:?} + {:?}",
        HELD,
        SILENCE
    );
    assert!(
        elapsed > Duration::from_secs(READ_SECS),
        "the reply can only be clean if the {READ_SECS}s read window started at \
         send time, not at queue entry: total call took {elapsed:?} \
         (queue ~{HELD:?} + backend silence ~{SILENCE:?})"
    );

    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(snapshot.available, 1, "the slot is back in the pool");
    assert_eq!(snapshot.waiting, 0, "no waiter may linger in the queue");

    deactivate();
}

/// Cancellation while queued: the wait selects on the enclosing worker token, so
/// the call exits promptly with a `Cancelled` reply — never a hang, never a hard
/// error, and nothing leaks from the pool or the queue.
#[tokio::test]
async fn test_cancellation_while_queued_exits_with_cancelled_reply() {
    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("never sent")))
        .mount(&server)
        .await;

    let backend = server.uri();
    let model = "model-cancel";
    let held = hold_slot(backend.as_str(), model).await;

    let token = CancellationToken::new();
    let canceller = {
        let token = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            token.cancel();
        })
    };

    let client = ChatClient::new(backend.clone(), model);
    let req = request();
    let started = Instant::now();
    // The worker token the pool wait selects on, installed exactly as a real
    // worker installs it.
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        CURRENT_WORKER_TOKEN.scope(token.clone(), client.chat_stream(&req, |_| true)),
    )
    .await
    .expect("a cancelled wait must exit instead of hanging")
    .expect("cancellation is a reply on the success channel, never an error");
    let waited = started.elapsed();
    canceller.await.expect("canceller task ran");

    assert!(
        waited < Duration::from_secs(4),
        "the waiter must exit on cancellation, waited {waited:?}"
    );
    assert_eq!(
        reply.outcome,
        ReplyOutcome::Cancelled,
        "a cancelled wait is never an empty success"
    );
    assert_eq!(reply.cause, TerminalCause::CallerAbort);
    assert!(!reply.is_success());
    assert_eq!(hits(&server).await, 0, "nothing was ever sent");

    // Queue accounting while the competing holder is still alive: the cancelled
    // waiter must already have left the FIFO queue, and dropping its future must
    // have taken no slot.
    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        snapshot.waiting, 0,
        "a cancelled waiter must have left the FIFO queue"
    );
    drop(held);
    let released = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        released.available, 1,
        "a cancelled wait must not have taken a slot"
    );

    deactivate();
}

/// Design §5: steer-arbitrator and notice-evaluation calls draw on the per-key
/// reserve, so a cap saturated by other sessions must never stall them.
#[tokio::test]
async fn test_reserve_class_is_never_queued_behind_a_saturated_cap() {
    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("steer decision")))
        .mount(&server)
        .await;

    let backend = server.uri();
    let model = "model-steer";
    let held = hold_slot(backend.as_str(), model).await;

    let client = ChatClient::new(backend.clone(), model);
    let req = request();
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        pool::with_call_context(
            pool::CallContext::new(pool::CallClass::SteerArbitrator),
            client.chat_stream(&req, |_| true),
        ),
    )
    .await
    .expect("a reserve-class call must not queue behind a saturated cap")
    .expect("the reserve call completes");
    assert!(
        reply.is_success(),
        "got {} / {:?}",
        reply.outcome.as_str(),
        reply.cause
    );
    assert_eq!(hits(&server).await, 1);

    let snapshot = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        snapshot.available, 0,
        "the capped slot is still held by the other session"
    );
    assert_eq!(
        snapshot.reserve_available, 1,
        "the reserve slot is handed back when the call returns"
    );

    drop(held);
    deactivate();
}

/// Design §9 rows AC2 + Q2: the cap belongs to exactly one
/// `(backend_url, model)` pair — never to a host, never to the process. With
/// `max_sessions = 1` and one pair saturated, a call to a **different URL** and a
/// call to the **same URL with a different model** must both run straight through
/// (no queueing, no waiting, each hitting its own backend) while the saturated
/// pair stays held.
///
/// The normalization half is pinned where it is actually used: the two spellings
/// of one URL are deliberately *one* key, while URLs that differ in anything else
/// (path, port, scheme, host, query) must stay distinct keys, or two backends
/// would silently share one cap.
#[tokio::test]
async fn test_cap_one_is_isolated_per_backend_url_and_model() {
    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    for server in [&server_a, &server_b] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body("unblocked answer")))
            .mount(server)
            .await;
    }

    let url_a = server_a.uri();
    let url_b = server_b.uri();
    let model = "model-shared-name";
    assert_ne!(url_a, url_b, "two mock backends are two distinct URLs");

    // Saturate exactly one pair, the way a concurrent session would.
    let held = hold_slot(url_a.as_str(), model).await;
    assert_eq!(held.available_permits(), 0, "cap 1, so this pair is full");

    let started = Instant::now();
    let client_other_model = ChatClient::new(url_a.clone(), "model-other");
    let client_other_url = ChatClient::new(url_b.clone(), model);
    let req = request();
    // The per-call timeout is what makes a blocking bug fail instead of hanging:
    // an unblocked pair returns in milliseconds, a merged key would sit in the
    // pool's 900 s queue wait.
    let (other_model, other_url) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(5),
            client_other_model.chat_stream(&req, |_| true)
        ),
        tokio::time::timeout(
            Duration::from_secs(5),
            client_other_url.chat_stream(&req, |_| true)
        ),
    );
    let elapsed = started.elapsed();

    for reply in [other_model, other_url] {
        let reply = reply.expect("a call on a different key must not be blocked by cap 1");
        let reply = reply.unwrap_or_else(|error| panic!("the call must succeed, got {error:?}"));
        assert!(
            reply.is_success(),
            "got {} / {:?}",
            reply.outcome.as_str(),
            reply.cause
        );
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "neither call may wait behind the saturated pair, took {elapsed:?}"
    );
    assert_eq!(
        hits(&server_a).await,
        1,
        "the other model reached backend A"
    );
    assert_eq!(hits(&server_b).await, 1, "the other URL reached backend B");

    // Controls: the cap really was configured and really was saturated, so the
    // run-through is not vacuous — and neither caller joined that queue.
    let saturated = pool::pool_snapshot(url_a.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(saturated.limit, 1, "max_sessions = 1 is in force");
    assert_eq!(
        saturated.available, 0,
        "the saturated pair stayed saturated"
    );
    assert_eq!(saturated.waiting, 0, "no outsider queued behind it");
    for (backend, other) in [(url_a.as_str(), "model-other"), (url_b.as_str(), model)] {
        let snapshot = pool::pool_snapshot(backend, other).expect("a capped pair has a pool");
        assert_eq!(
            snapshot.available, 1,
            "{backend} + {other} drew its own slot, not {url_a} + {model}'s"
        );
        assert_eq!(snapshot.waiting, 0);
    }

    // Deliberate merge: one URL, two spellings, one cap.
    assert_eq!(
        pool::pool_key(&format!("{url_a}/"), model),
        pool::pool_key(url_a.as_str(), model),
        "a trailing slash must not fork the cap"
    );
    // Deliberate distinctness: nothing else may be folded away, or two different
    // backends would share one semaphore.
    let base = "http://iso.invalid:8080/v1";
    for distinct in [
        "http://iso.invalid:8080/v10",
        "http://iso.invalid:8080/v1/extra",
        "http://iso.invalid:8081/v1",
        "https://iso.invalid:8080/v1",
        "http://iso2.invalid:8080/v1",
        "http://iso.invalid:8080/v1?key=a",
    ] {
        assert_ne!(
            pool::pool_key(distinct, model).0,
            pool::pool_key(base, model).0,
            "{distinct} must not be normalized onto {base}"
        );
    }
    assert_eq!(
        pool::pool_key("  http://iso.invalid:8080/v1//  ", model).0,
        pool::pool_key(base, model).0,
        "only stray whitespace and trailing slashes are folded"
    );
    // The model half is matched exactly apart from trimming.
    assert_eq!(
        pool::pool_key(base, " model-a "),
        pool::pool_key(base, "model-a")
    );
    assert_ne!(
        pool::pool_key(base, "model-a"),
        pool::pool_key(base, "model-ab")
    );

    drop(held);
    assert_eq!(
        pool::pool_snapshot(url_a.as_str(), model)
            .expect("a capped pair has a pool")
            .available,
        1,
        "the slot comes back to its own pair"
    );

    deactivate();
}

/// Acceptance (t-016): a cancel that lands while a call is **queued** must hand
/// the queue back, never wedge it.
///
/// Three callers on one capped pair (`max_sessions = 1`): an externally parked
/// holder saturating the cap, waiter B queued at rank 1, and waiter C queued
/// **behind** it at rank 2. B is then cancelled mid-wait. What must stay true:
///
/// * B exits promptly with a `Cancelled` / `CallerAbort` reply and gives its FIFO
///   seat back — queue depth drops from 2 to 1, and the cancelled waiter never
///   took a permit (`available` stays 0 while the holder lives);
/// * the surviving waiter's rank moves up (`2 of 2` → `1 of 1`), so the cancelled
///   waiter leaves no stale position in line behind which everyone stalls;
/// * when the holder releases, the slot reaches C — a caller that queued behind
///   the cancelled one — which then reaches the backend and returns. A permit
///   swallowed by the dead waiter, or a semaphore waiter that was never
///   unregistered, shows up here as a timeout instead;
/// * a brand-new call afterwards acquires the released slot too, and the pool
///   settles at `available = 1, waiting = 0`.
#[tokio::test]
async fn test_cancelling_a_queued_waiter_hands_the_queue_to_the_next_call() {
    let _serial = POOL_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(Some(1));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(sse_body("served after the cancel")),
        )
        .mount(&server)
        .await;

    let backend = server.uri();
    let model = "model-queue-handoff";
    let held = hold_slot(backend.as_str(), model).await;
    assert_eq!(held.available_permits(), 0, "cap 1, so this pair is full");

    // Ranks travel as status traffic (design §6), which is where a stale
    // position left behind by a cancelled waiter would surface.
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    /// Poll the FIFO ledger until at least `want` callers are in line, so
    /// "this call is genuinely queued" is a measurement and not a race.
    async fn depth_at_least(backend: &str, model: &str, want: usize) -> pool::PoolSnapshot {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = pool::pool_snapshot(backend, model).expect("a capped pair has a pool");
            if snapshot.waiting >= want || Instant::now() >= deadline {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // A `chat_stream` under a worker token, the shape a cancellable session call
    // takes: the pool wait selects on exactly this token.
    let waiter = |token: CancellationToken| {
        let client = ChatClient::new(backend.clone(), model);
        let req = request();
        tokio::spawn(async move {
            CURRENT_WORKER_TOKEN
                .scope(token, client.chat_stream(&req, |_| true))
                .await
        })
    };

    // B queues first (rank 1) and is the one that gets cancelled.
    let token_b = CancellationToken::new();
    let waiter_b = waiter(token_b.clone());
    let first = depth_at_least(backend.as_str(), model, 1).await;
    assert_eq!(first.waiting, 1, "B must be in line, got {first:?}");

    // C queues behind B (rank 2) and is never cancelled.
    let token_c = CancellationToken::new();
    let waiter_c = waiter(token_c);
    let both = depth_at_least(backend.as_str(), model, 2).await;
    assert_eq!(
        both.waiting, 2,
        "both waiters must be in line, got {both:?}"
    );
    assert_eq!(
        hits(&server).await,
        0,
        "a queued session must not reach the backend"
    );

    let ranks: Vec<String> = drain_statuses(&mut status_rx);
    assert!(
        ranks.iter().any(|line| line.contains("(2 of 2)")),
        "the waiter that joined second must report rank 2 of 2, got {ranks:?}"
    );

    // Cancel B while it is still queued.
    let cancelled_at = Instant::now();
    token_b.cancel();
    let reply_b = tokio::time::timeout(Duration::from_secs(5), waiter_b)
        .await
        .expect("a cancelled waiter must exit instead of hanging")
        .expect("the waiter task must not panic")
        .expect("cancellation while queued is a reply, never an error");
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(4),
        "the cancelled waiter must exit promptly, took {:?}",
        cancelled_at.elapsed()
    );
    assert_eq!(reply_b.outcome, ReplyOutcome::Cancelled);
    assert_eq!(reply_b.cause, TerminalCause::CallerAbort);
    assert_eq!(
        hits(&server).await,
        0,
        "the cancelled call never sent anything"
    );

    // No leak on either ledger: the seat is gone, the permit was never taken,
    // and the waiter behind it is still legitimately queued.
    let after = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        after.waiting, 1,
        "the cancelled waiter must have left the FIFO queue, leaving only C"
    );
    assert_eq!(
        after.available, 0,
        "the cancelled waiter must not have taken the holder's slot"
    );

    // The survivor's rank must follow the queue, not the memory of a dead seat.
    // The pool refreshes positions every 250 ms while a reporter is installed.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let promoted: Vec<String> = drain_statuses(&mut status_rx);
    assert!(
        promoted.iter().any(|line| line.contains("(1 of 1)")),
        "C must be re-ranked to 1 of 1 once B is gone, got {promoted:?}"
    );
    assert!(
        !promoted.iter().any(|line| line.contains("(2 of 2)")),
        "no caller may still be reported behind the cancelled waiter: {promoted:?}"
    );

    // The release must reach the caller that queued behind the cancelled one:
    // this is the deadlock detector — a permit stranded on the dead waiter, or a
    // stale queue seat blocking the handoff, fails here on timeout.
    drop(held);
    let reply_c = tokio::time::timeout(Duration::from_secs(10), waiter_c)
        .await
        .expect("the caller queued behind a cancelled waiter must be served, not deadlock")
        .expect("the waiter task must not panic")
        .expect("the queued call must succeed");
    assert!(
        reply_c.is_success(),
        "got {} / {:?}",
        reply_c.outcome.as_str(),
        reply_c.cause
    );
    assert_eq!(
        hits(&server).await,
        1,
        "exactly one request reached the backend: the cancelled call sent nothing"
    );

    // And a fresh call afterwards is not blocked by the ghost of the cancelled
    // waiter either.
    let fresh = tokio::time::timeout(
        Duration::from_secs(5),
        ChatClient::new(backend.clone(), model).chat_stream(&request(), |_| true),
    )
    .await
    .expect("a new call must acquire the released slot")
    .expect("the new call must not fail");
    assert!(
        fresh.is_success(),
        "got {} / {:?}",
        fresh.outcome.as_str(),
        fresh.cause
    );
    assert_eq!(hits(&server).await, 2, "one request per served call");

    let settled = pool::pool_snapshot(backend.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(settled.available, 1, "the slot is back in the pool");
    assert_eq!(settled.waiting, 0, "no queue seat may linger");

    deactivate();
}

/// Everything the status bus has carried since the last drain.
fn drain_statuses(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(status) = rx.try_recv() {
        lines.push(status);
    }
    lines
}
