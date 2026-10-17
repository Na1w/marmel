//! End-to-end proof that the **call-context plumbing** installed on the manager,
//! specialist and nested-delegation paths actually reaches the one place that
//! meters LLM traffic: the pool gate inside `ChatClient::chat_stream`.
//!
//! A context that is installed but never observed by the pool is decoration, so
//! every test here asserts against something the pool or the UI can see:
//! * a context-scoped call is metered against the **cap** (`waiting` grows while
//!   the cap is saturated — the manager/specialist classes must never take the
//!   reserve),
//! * the queue traffic the UI renders **names the delegation** the queued call
//!   belongs to (`… — specialist coder [t-042]`),
//! * a nested delegation **overwrites** the inherited attribution — a queued
//!   nested call is never blamed on the parent that dispatched it,
//! * the context is **discharged** by the scope: no attribution leaks into the
//!   next call made by the same task, and nothing crosses a `spawn_blocking` /
//!   `thread::scope` boundary unless it is propagated explicitly, which is what
//!   `ui::session`'s tool round and `orchestrator::delegate` do,
//! * two delegations running concurrently keep **their own** attribution.
//!
//! The pool registry, the active config and the status sender are process-global,
//! so every test here takes `CTX_TEST_MUTEX`. Each test uses its own mock server
//! (unique URL) and its own model name, so pool entries never collide either.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use marmennill::config::{ConcurrencyConfig, Config, set_active};
use marmennill::llm::client::ChatClient;
use marmennill::orchestrator::pool;
use marmennill::orchestrator::{reset_cancellation, set_status_sender};
use marmennill::types::{ChatRequest, Message};
use tokio::sync::{Mutex, mpsc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialize these tests: pool entries, the active config and the status sender
/// are process-global state.
static CTX_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Install an active config with a global cap of `max_sessions` per
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
            "id": "chatcmpl-ctx",
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

/// Poll the queue ledger until `want` sessions are in line, or give up.
///
/// Polling instead of sleeping keeps the assertion deterministic: registration
/// happens on the waiter's first poll, and the timing of that poll is up to the
/// runtime, not to the test.
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

/// Everything the pool has said about queueing so far.
fn drained(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }
    lines
}

/// A manager/UI turn (the class `src/ui/session.rs` installs) is metered against
/// the cap and is attributed as a manager turn — and the scope leaves nothing
/// behind on the task that ran it.
#[tokio::test]
async fn manager_turn_context_is_capped_attributed_and_discharged() {
    let _serial = CTX_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let server = backend("manager answer").await;
    let url = server.uri();
    let model = "model-ctx-manager";
    let held = hold_slot(url.as_str(), model).await;

    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    let client = ChatClient::new(url.clone(), model);
    let req = request();
    let context =
        pool::CallContext::for_agent(pool::CallClass::ManagerTurn, pool::MANAGER_AGENT, None);

    let call = pool::with_call_context(context.clone(), client.chat_stream(&req, |_| true));
    tokio::pin!(call);

    // A manager turn is a capped class: it must queue behind the saturated cap.
    let stalled = tokio::time::timeout(Duration::from_millis(400), &mut call).await;
    assert!(
        stalled.is_err(),
        "a manager turn must be metered against the cap, not skip it"
    );
    let queued = wait_until_queued(url.as_str(), model, 1).await;
    assert_eq!(
        queued.waiting, 1,
        "the queued manager turn must be in line, got {queued:?}"
    );

    let statuses = drained(&mut status_rx);
    assert!(
        statuses
            .iter()
            .any(|line| line.contains("queued for a backend slot")
                && line.contains("manager_turn manager")),
        "a queued manager turn must name itself on the status bus, got: {statuses:?}"
    );

    drop(held);
    let reply = tokio::time::timeout(Duration::from_secs(10), &mut call)
        .await
        .expect("the queued manager turn must run once a slot frees")
        .expect("the manager turn succeeds");
    assert!(reply.is_success(), "reply: {}", reply.outcome.as_str());

    // No leak: the scope that installed the context has returned, so this very
    // task (the session task, in production) must be context-free again. The
    // next call on this task is therefore unclassified, not a manager turn.
    assert_eq!(
        pool::current_call_context(),
        None,
        "a finished manager turn must leave no context behind"
    );

    deactivate();
}

/// The nested `delegate_task` shape: the tool round that runs a delegation leaves
/// the async task, the inherited context must be re-installed on that other
/// thread, and it is then **overwritten** — so the nested delegation's queued
/// calls are blamed on the nested agent and task, never on the parent. (`src/
/// orchestrator/delegate.rs` crosses the same boundary through
/// `std::thread::scope` + `block_on`; the pool unit test
/// `nested_sync_scope_overwrites_and_clears_on_a_foreign_thread` pins that exact
/// spelling, and `src/ui/session.rs` reaches it through `spawn_blocking`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_delegate_overwrites_the_inherited_attribution_across_threads() {
    let _serial = CTX_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let server = backend("nested deliverable").await;
    let url = server.uri();
    let model = "model-ctx-nested";
    let held = hold_slot(url.as_str(), model).await;

    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    let client = ChatClient::new(url.clone(), model);
    let req = request();

    // The parent: a specialist turn for agent `coder`, task `t-PARENT`, exactly
    // what `agents::runner` installs.
    let parent = pool::CallContext::for_agent(
        pool::CallClass::Specialist,
        "coder",
        Some("t-PARENT".to_string()),
    );
    // The nested delegation `delegate_task` builds for agent `debugger` / `t-CHILD`.
    let nested = pool::CallContext::for_agent(
        pool::CallClass::Specialist,
        "debugger",
        Some("t-CHILD".to_string()),
    );
    // Copies kept outside the future, for the assertions once it has run.
    let parent_expect = parent.clone();
    let nested_expect = nested.clone();

    let outer = tokio::spawn(async move {
        let inside = pool::with_call_context(parent.clone(), async move {
            // The parent context is in force here, as it is inside `handle_delegate_task`.
            let inherited = pool::current_call_context().expect("the parent context is installed");

            // The boundary `delegate_task` actually crosses: the tool round leaves
            // the async task, the context is carried across explicitly and
            // **overwritten** for the nested delegation, and `block_on` drives the
            // nested call on that other thread.
            let nested_for_thread = nested.clone();
            let client_for_thread = client;
            let seen_inside = tokio::task::spawn_blocking(move || {
                // Nothing was inherited by this thread — which is exactly why the
                // propagation below is mandatory rather than decorative.
                let unprompted = pool::current_call_context();
                pool::with_call_context_sync(nested_for_thread, || {
                    let seen = pool::current_call_context();
                    let handle = tokio::runtime::Handle::current();
                    let ran = handle
                        .block_on(client_for_thread.chat_stream(&req, |_| true))
                        .map(|reply| reply.is_success())
                        .unwrap_or(false);
                    (unprompted, seen, ran)
                })
            })
            .await
            .expect("the nested thread ran");

            // After the delegation returns, the *parent* is back in force on this
            // task: the nested scope discharged itself, it did not overwrite
            // outward.
            let restored = pool::current_call_context();
            (inherited, seen_inside, restored)
        })
        .await;
        // And once the parent turn's own scope has returned, this task is
        // context-free again — nothing survives the turn.
        (inside, pool::current_call_context())
    });

    // The queue registration happens on the spawned task, so wait on the ledger
    // instead of driving the future here.
    let queued = wait_until_queued(url.as_str(), model, 1).await;
    assert_eq!(queued.waiting, 1, "one nested call in line, got {queued:?}");

    let statuses = drained(&mut status_rx);
    assert!(
        statuses
            .iter()
            .any(|line| line.contains("queued for a backend slot")
                && line.contains("specialist debugger [t-CHILD]")
                && !line.contains("t-PARENT")),
        "a queued nested delegation must be attributed to itself, not its parent, got: {statuses:?}"
    );

    drop(held);
    let ((inherited, (unprompted, seen_inside, ran), restored), after_the_turn) =
        tokio::time::timeout(Duration::from_secs(20), outer)
            .await
            .expect("the nested delegation must finish once a slot frees")
            .expect("the parent task must not panic");

    assert!(ran, "the nested delegation's call must have run");
    assert_eq!(
        inherited, parent_expect,
        "the parent turn's context must be what `delegate_task` inherits"
    );
    assert_eq!(
        unprompted, None,
        "the context must not cross the boundary by itself"
    );
    assert_eq!(
        seen_inside.as_ref(),
        Some(&nested_expect),
        "the pool gate must see the nested context, not the inherited one"
    );
    assert_eq!(
        restored.as_ref(),
        Some(&parent_expect),
        "the parent context must survive its child's delegation"
    );
    assert_eq!(
        after_the_turn, None,
        "and the parent scope must discharge when the parent turn ends"
    );

    deactivate();
}

/// Two delegations queued at the same instant on one pool must each be
/// attributed to **their own** agent and task. This is precisely the cross-talk a
/// thread-local context would produce, because one tokio worker thread polls both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_delegations_keep_their_own_attribution() {
    let _serial = CTX_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let server = backend("two answers").await;
    let url = server.uri();
    let model = "model-ctx-concurrent";
    let held = hold_slot(url.as_str(), model).await;

    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    let first = {
        let client = ChatClient::new(url.clone(), model);
        let req = request();
        tokio::spawn(async move {
            pool::with_call_context(
                pool::CallContext::for_agent(
                    pool::CallClass::Specialist,
                    "coder",
                    Some("t-ONE".to_string()),
                ),
                client.chat_stream(&req, |_| true),
            )
            .await
        })
    };
    let second = {
        let client = ChatClient::new(url.clone(), model);
        let req = request();
        tokio::spawn(async move {
            pool::with_call_context(
                pool::CallContext::for_agent(
                    pool::CallClass::Specialist,
                    "researcher",
                    Some("t-TWO".to_string()),
                ),
                client.chat_stream(&req, |_| true),
            )
            .await
        })
    };

    let queued = wait_until_queued(url.as_str(), model, 2).await;
    assert_eq!(
        queued.waiting, 2,
        "both delegations in line, got {queued:?}"
    );

    let statuses = drained(&mut status_rx);
    assert!(
        statuses.iter().any(|line| line.contains("coder [t-ONE]")),
        "the coder's queued call must name the coder, got: {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .any(|line| line.contains("researcher [t-TWO]")),
        "the researcher's queued call must name the researcher, got: {statuses:?}"
    );
    assert!(
        !statuses.iter().any(|line| {
            line.contains("coder") && (line.contains("t-TWO") || line.contains("researcher"))
        }),
        "no queued line may mix two delegations' identities, got: {statuses:?}"
    );

    drop(held);
    for handle in [first, second] {
        let reply = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("both queued calls must run")
            .expect("the task must not panic")
            .expect("the call must not fail");
        assert!(reply.is_success(), "reply: {}", reply.outcome.as_str());
    }

    let snapshot = pool::pool_snapshot(url.as_str(), model).expect("a capped pair has a pool");
    assert_eq!(
        snapshot.waiting, 0,
        "no waiter may linger after its call ends"
    );
    assert_eq!(snapshot.available, 1, "and no permit may leak either");

    deactivate();
}

/// The runner itself, not a hand-installed context: `agents::runner::run_specialist_live`
/// is the entry point every specialist delegation goes through, so the
/// attribution a queued specialist call carries must come from the runner.
///
/// Driven exactly as `tests/test_specialist_stream.rs` drives it, but with the
/// `backend + model` cap saturated, so the runner's first backend call queues and
/// the queue traffic it emits must name *this* agent and *this* plan task.
#[tokio::test]
async fn the_specialist_runner_installs_attribution_for_its_own_calls() {
    let _serial = CTX_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|req: &wiremock::Request| {
            let body = String::from_utf8_lossy(&req.body).to_string();
            if body.contains("leave_verdict") || body.contains("Specialist Deliverable") {
                ResponseTemplate::new(200).set_body_string(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"val_1\",\"type\":\"function\",\"function\":{\"name\":\"leave_verdict\",\"arguments\":\"{\\\"verdict\\\":\\\"APPROVED\\\",\\\"comments\\\":\\\"ok\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n",
                )
            } else {
                ResponseTemplate::new(200).set_body_string(sse_body(
                    "Implemented the change.\n\nMISSION COMPLETE (t-0CTX)",
                ))
            }
        })
        .mount(&server)
        .await;

    // Hermeticity: an isolated workspace root, so no real `.marmel` state is read.
    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let held_key = format!("{}/v1", server.uri());
    let model = "model-ctx-runner";

    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);
    let held = hold_slot(held_key.as_str(), model).await;

    let ran = marmennill::harness::with_workspace_root(tmp.path().to_path_buf(), async move {
        let delegation = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "Create hello.txt".to_string(),
            snippets: vec![],
            task_id: Some("t-0CTX".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx = marmennill::agents::IsolatedContext::from_request(
            "You are the Coder specialist.".to_string(),
            &delegation,
        );
        let cfg = Config {
            backend_url: held_key.clone(),
            model: model.to_string(),
            ..Config::default()
        };
        let client = ChatClient::new(held_key.as_str(), model);
        let token = tokio_util::sync::CancellationToken::new();

        let call = marmennill::agents::run_specialist_live(
            &client,
            marmennill::agents::Agent::Coder,
            &ctx,
            &cfg,
            &token,
        );
        tokio::pin!(call);

        // Saturated cap ⇒ the runner's first backend call must queue.
        let stalled = tokio::time::timeout(Duration::from_millis(600), &mut call).await;
        assert!(
            stalled.is_err(),
            "the specialist call must wait for the cap, not skip it"
        );
        let queued = wait_until_queued(held_key.as_str(), model, 1).await;
        assert_eq!(
            queued.waiting, 1,
            "the specialist call is in line, got {queued:?}"
        );

        let statuses = drained(&mut status_rx);
        assert!(
            statuses
                .iter()
                .any(|line| line.contains("queued for a backend slot")
                    && line.contains("specialist coder [t-0CTX]")),
            "the runner's queued call must be attributed to its agent and task, got: {statuses:?}"
        );

        drop(held);
        let deliverable = tokio::time::timeout(Duration::from_secs(30), &mut call)
            .await
            .expect("the specialist must finish once a slot frees")
            .expect("the specialist run must not error");
        assert!(
            deliverable.contains("MISSION COMPLETE (t-0CTX)"),
            "deliverable: {deliverable}"
        );

        // Discharged: the runner's context never survives the delegation.
        pool::current_call_context()
    })
    .await;

    assert_eq!(
        ran, None,
        "the specialist runner must leave no context behind when it returns"
    );

    deactivate();
}

/// An unclassified call — no context installed, the pre-feature shape — is still
/// metered, still queued, and reports **no** attribution. This is the negative
/// control for the leak test above: nothing from an earlier call crept into it.
#[tokio::test]
async fn an_unclassified_call_queues_without_any_attribution() {
    let _serial = CTX_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let server = backend("unclassified answer").await;
    let url = server.uri();
    let model = "model-ctx-unclassified";
    let held = hold_slot(url.as_str(), model).await;

    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    let client = ChatClient::new(url.clone(), model);
    let req = request();
    let call = client.chat_stream(&req, |_| true);
    tokio::pin!(call);

    let stalled = tokio::time::timeout(Duration::from_millis(400), &mut call).await;
    assert!(stalled.is_err(), "an unclassified call is still metered");
    let queued = wait_until_queued(url.as_str(), model, 1).await;
    assert_eq!(queued.waiting, 1, "got {queued:?}");

    let statuses = drained(&mut status_rx);
    let queue_lines: Vec<&String> = statuses
        .iter()
        .filter(|line| line.contains("queued for a backend slot"))
        .collect();
    assert!(
        !queue_lines.is_empty(),
        "queueing must still be reported, got: {statuses:?}"
    );
    assert!(
        queue_lines.iter().all(|line| !line.contains('—')),
        "an unclassified call must invent no attribution, got: {queue_lines:?}"
    );

    drop(held);
    let reply = tokio::time::timeout(Duration::from_secs(10), &mut call)
        .await
        .expect("the queued call runs once the slot frees")
        .expect("the call succeeds");
    assert!(reply.is_success(), "reply: {}", reply.outcome.as_str());

    deactivate();
}

/// A tool round leaves the async task: `ui::session` dispatches manager tools on
/// `spawn_blocking`, and nothing inherits a task-local there. So the manager
/// context must be re-installed explicitly — and the child must see the manager's
/// identity, while the parent task keeps its own context intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manager_tool_round_reinstalls_the_context_on_a_blocking_thread() {
    let _serial = CTX_TEST_MUTEX.lock().await;
    reset_cancellation();
    activate(1);

    let server = backend("tool answer").await;
    let url = server.uri();
    let model = "model-ctx-toolround";
    let held = hold_slot(url.as_str(), model).await;

    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    set_status_sender(status_tx);

    let manager =
        pool::CallContext::for_agent(pool::CallClass::ManagerTurn, pool::MANAGER_AGENT, None);
    let manager_expect = manager.clone();
    let url_for_turn = url.clone();

    let context = pool::with_call_context(manager.clone(), async move {
        let inherited_by_mistake = tokio::task::spawn_blocking(pool::current_call_context)
            .await
            .expect("the blocking task ran");

        // The explicit propagation `ui::session` performs for every tool round.
        let propagated = {
            let manager = manager.clone();
            let client = ChatClient::new(url_for_turn.clone(), model);
            let req = request();
            tokio::task::spawn_blocking(move || {
                pool::with_call_context_sync(manager, || {
                    let seen = pool::current_call_context();
                    let handle = tokio::runtime::Handle::current();
                    let ran = handle
                        .block_on(client.chat_stream(&req, |_| true))
                        .map(|reply| reply.is_success())
                        .unwrap_or(false);
                    (seen, ran)
                })
            })
            .await
            .expect("the blocking tool round ran")
        };

        // Nothing was left behind on this task either: the tool round's scope
        // belongs to the blocking thread, and this task still carries its own.
        let after = pool::current_call_context();
        (inherited_by_mistake, propagated, after)
    });
    tokio::pin!(context);

    // Drive the turn until the call its tool round made joins the queue. Polling
    // the ledger alone would never register anything: the turn only advances when
    // it is polled.
    let queued = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = tokio::time::timeout(Duration::from_millis(50), &mut context).await;
            let snapshot =
                pool::pool_snapshot(url.as_str(), model).expect("a capped pair has a pool");
            if snapshot.waiting >= 1 {
                break snapshot;
            }
        }
    })
    .await
    .expect("the tool round's call must join the queue, not skip the cap");
    assert_eq!(queued.waiting, 1, "got {queued:?}");

    let statuses = drained(&mut status_rx);
    assert!(
        statuses
            .iter()
            .any(|line| line.contains("queued for a backend slot")
                && line.contains("manager_turn manager")),
        "a queued call made from a tool round must be attributed to the manager turn, got: {statuses:?}"
    );

    drop(held);
    let (inherited_by_mistake, (seen_on_blocking_thread, ran), after) =
        tokio::time::timeout(Duration::from_secs(15), &mut context)
            .await
            .expect("the manager turn completes once a slot frees");

    assert_eq!(
        inherited_by_mistake, None,
        "a task-local must not leak into `spawn_blocking` implicitly"
    );
    assert_eq!(
        seen_on_blocking_thread.as_ref(),
        Some(&manager_expect),
        "but an explicitly propagated context must be visible at the pool gate"
    );
    assert!(ran, "the propagated tool-round call must have run");
    assert_eq!(
        after.as_ref(),
        Some(&manager_expect),
        "the manager turn's own context must be untouched by the tool round"
    );
    assert_eq!(
        pool::current_call_context(),
        None,
        "and it must be gone once the manager turn returns"
    );

    deactivate();
}
