//! t-023 — the periodic commit triggers: does a crash actually leave a bundle?
//!
//! Flow `docs/design_session_restore_flow.md` §1 fixes **when** a session bundle
//! is written and **who** writes it. This file pins the non-abort, non-panic half
//! of that contract (Ctrl-C / abort / panic-hook flush belongs to t-024):
//!
//! * **T-1 turn boundary** — a bundle with a manifest appears after one round of
//!   the manager loop (a) — that bundle is what a `--resume` can open at all;
//! * **R-TRIG-2/3 coalescing** — the ~250 ms floor (`session_commit_min_interval_ms`)
//!   really folds repeated T-4/T-5 requests into one write, and mandatory triggers
//!   (T-1/T-2/T-3/T-9) ignore the floor (b);
//! * **T-2 pre-dispatch** — a commit is requested *before* a tool batch is
//!   dispatched, and the committed manager section is provably **dangling** at
//!   that moment: an assistant `tool_calls` batch with no results yet, which is
//!   what the loader's normalizer (L8) needs to find after a hard kill (c);
//! * **T-9 loop tail** — the tail commit records `ProcessEnding::CleanTail`, which
//!   is the only thing that lets a later boot classify `CleanTail` apart from
//!   `HardKillInferred` (d);
//! * `session_enabled = false` writes **nothing**, ever (e), and
//!   `session_commit_on_tool_result = false` suppresses T-3 only.
//!
//! The two end-to-end cases drive the **real** session loop
//! ([`marmennill::ui::run_session`]) against a wiremock backend, so the triggers
//! are observed as the loop actually reaches them, not as a mock pump.
//!
//! State discipline: the commit ledger and the commit-diff cache are
//! process-global, so every case takes the mutex and asserts **deltas** of the
//! ledger rather than absolute counts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use marmennill::config::Config;
use marmennill::manager::context::ContextEngine;
use marmennill::manager::phase::Plan;
use marmennill::session::{MANAGER_FILE, MANIFEST_FILE, ProcessEnding, UI_FILE};
use marmennill::types::{Message, ToolCall, ToolFunction};
use marmennill::ui::session::{
    CommitDecision, CommitTrigger, SessionCommitLedger, SessionCommitPump,
};
use marmennill::ui::{Event, InputState, Renderer};

static TEST_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

// --------------------------------------------------------------------- helpers

/// Delta of one ledger counter across a case (the ledger is process-global).
fn diff(
    after: &SessionCommitLedger,
    before: &SessionCommitLedger,
    field: fn(&SessionCommitLedger) -> u64,
) -> u64 {
    field(after).saturating_sub(field(before))
}

/// A renderer with a pane model, i.e. one that has something honest to
/// `capture_ui`: the UI member of the bundle is only interesting if the capture
/// is non-empty.
struct MirrorRenderer {
    input_state: InputState,
    chat: Vec<String>,
}

impl MirrorRenderer {
    fn new() -> Self {
        Self {
            input_state: InputState::default(),
            chat: vec![
                "User: t023 goal".to_string(),
                "[Tool Call] read_file".to_string(),
            ],
        }
    }
}

impl Renderer for MirrorRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, _event: &Event) {}
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn input_state(&mut self) -> &mut InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &InputState {
        &self.input_state
    }
    fn shutdown(&mut self) {}
    fn capture_ui(&self) -> marmennill::session::UiSection {
        marmennill::session::UiSection {
            chat_messages: self.chat.clone(),
            status_line: "t023 status".to_string(),
            journal: vec![marmennill::session::UiJournalRecord::User {
                text: "t023 goal".to_string(),
            }],
            ..marmennill::session::UiSection::default()
        }
    }
}

/// A minimal but legal transcript: the pinned `[0]`/`[1]` pair every member header
/// and every L1–L2 gate expects.
fn engine() -> ContextEngine {
    let mut ctx = ContextEngine::new(200_000);
    ctx.append(Message::System {
        content: "t023 system prompt".to_string(),
    });
    ctx.append(Message::User {
        content: "t023 goal: prove the commit triggers".to_string(),
    });
    ctx
}

/// An assistant turn that asked for `call-t023-dangling` and whose result was
/// never appended — the exact shape a hard kill leaves behind.
fn engine_with_dangling_call() -> ContextEngine {
    let mut ctx = engine();
    ctx.append(Message::Assistant {
        content: None,
        reasoning_content: Some("I will read the manifest first".to_string()),
        tool_calls: vec![ToolCall {
            id: "call-t023-dangling".to_string(),
            kind: "function".to_string(),
            function: ToolFunction {
                name: "read_file".to_string(),
                arguments: r#"{"path":"Cargo.toml"}"#.to_string(),
            },
        }],
    });
    ctx
}

fn pump_at(dir: &Path, cfg: &Config, session_id: &str) -> SessionCommitPump {
    SessionCommitPump::new(&Plan::at(dir), cfg, session_id)
}

/// The bundle the loop (or the pump) is expected to have written under `dir`.
fn bundle_in(dir: &Path) -> PathBuf {
    marmennill::session::bundle_dir(&Plan::at(dir))
}

fn read_bundle_file(bundle: &Path, rel: &str) -> String {
    std::fs::read_to_string(bundle.join(rel))
        .unwrap_or_else(|err| panic!("{} must exist in {}: {err}", rel, bundle.display()))
}

/// The manifest as JSON on disk — the atomicity point, read straight off disk so
/// these assertions never depend on the in-process commit cache.
fn manifest_on_disk(bundle: &Path) -> serde_json::Value {
    let text = read_bundle_file(bundle, MANIFEST_FILE);
    serde_json::from_str(&text).expect("manifest.json is JSON")
}

fn ending_on_disk(bundle: &Path) -> String {
    manifest_on_disk(bundle)["ending"]["kind"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn members_on_disk(bundle: &Path) -> Vec<String> {
    manifest_on_disk(bundle)["members"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|member| member["rel_path"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn is_written(decision: CommitDecision) -> bool {
    matches!(decision, CommitDecision::Written { .. })
}

/// A `Config` pointed at `backend`, mirroring `tests/test_ui_session.rs`.
fn config_for_backend(backend: &str) -> Config {
    Config {
        backend_url: format!("{backend}/v1"),
        system_prompt_path: PathBuf::from("prompts/system.md"),
        ui_mode: "tui".to_string(),
        ..Config::default()
    }
}

/// One canned OpenAI-style SSE body.
fn sse(body: serde_json::Value) -> String {
    format!("data: {}\n\ndata: [DONE]\n\n", body)
}

fn text_sse(text: &str) -> String {
    sse(serde_json::json!({
        "id": "chatcmpl-t023",
        "choices": [{ "delta": { "content": text }, "finish_reason": null }]
    }))
}

/// One `read_file` tool call — the batch whose pre-dispatch commit must be dangling.
fn one_tool_call_sse() -> String {
    sse(serde_json::json!({
        "id": "chatcmpl-t023-tool",
        "choices": [{
            "delta": { "tool_calls": [{
                "index": 0,
                "id": "call-t023-pre",
                "type": "function",
                "function": { "name": "read_file", "arguments": "{\"path\": \"Cargo.toml\"}" }
            }]},
            "finish_reason": null
        }]
    }))
}

// ------------------------------------------------------------------ (a) T-1

#[tokio::test]
async fn turn_boundary_commit_leaves_a_manifest_bearing_bundle() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    assert!(cfg.session_enabled, "capture is on by default");
    let mut pump = pump_at(tmp.path(), &cfg, "t023-turn-boundary");
    let ctx = engine();
    let renderer = MirrorRenderer::new();

    let before = marmennill::ui::session::session_commit_ledger();
    let decision = pump.request(CommitTrigger::ManagerTurnBoundary, &ctx, &renderer);
    let after = marmennill::ui::session::session_commit_ledger();

    assert!(is_written(decision), "T-1 must write: {decision:?}");
    assert_eq!(diff(&after, &before, |l| l.turn_boundary), 1);
    assert_eq!(diff(&after, &before, |l| l.written), 1);
    assert_eq!(after.last_ending, Some(ProcessEnding::InProgress));
    assert!(
        marmennill::session::commit::last_manifest(pump.bundle()).is_some(),
        "the commit cache must hold the manifest that was just written"
    );

    let bundle = bundle_in(tmp.path());
    for rel in [MANIFEST_FILE, MANAGER_FILE, UI_FILE] {
        assert!(
            bundle.join(rel).is_file(),
            "{rel} must exist after a turn-boundary commit ({})",
            bundle.display()
        );
    }
    let members = members_on_disk(&bundle);
    assert!(members.contains(&MANAGER_FILE.to_string()));
    assert!(members.contains(&UI_FILE.to_string()));
    let manifest = manifest_on_disk(&bundle);
    assert_eq!(manifest["session_id"].as_str(), Some(pump.session_id()));
    assert_eq!(ending_on_disk(&bundle), "inProgress");

    // The UI member carries the renderer capture, not an empty placeholder.
    let ui = read_bundle_file(&bundle, UI_FILE);
    assert!(
        ui.contains("t023 status"),
        "capture_ui must reach the ui member: {ui}"
    );
}

// --------------------------------------------------------------------- (b)

#[tokio::test]
async fn throttled_triggers_coalesce_and_mandatory_triggers_ignore_the_floor() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config {
        session_commit_min_interval_ms: 250,
        ..Config::default()
    };
    let mut pump = pump_at(tmp.path(), &cfg, "t023-throttle");
    let ctx = engine();
    let renderer = MirrorRenderer::new();

    let before = marmennill::ui::session::session_commit_ledger();
    let mut written = 0usize;
    let mut throttled = 0usize;
    for _ in 0..25 {
        match pump.request(CommitTrigger::UiEvent, &ctx, &renderer) {
            CommitDecision::Written { .. } => written += 1,
            CommitDecision::Throttled => throttled += 1,
            other => panic!("T-4 must write or throttle, got {other:?}"),
        }
    }
    assert_eq!(
        written, 1,
        "25 UI requests inside one 250 ms floor must fold into ONE write (R-TRIG-2)"
    );
    assert_eq!(throttled, 24);
    assert!(pump.pending(), "the throttled debt is still owed");

    // R-TRIG-3: a mandatory trigger wins inside the same window and clears the debt.
    assert!(
        is_written(pump.request(CommitTrigger::PreDispatch, &ctx, &renderer)),
        "T-2 is never throttled"
    );
    assert!(
        !pump.pending(),
        "a mandatory commit pays the throttled debt"
    );
    // T-3 is mandatory too, and back-to-back T-2/T-3 must both write.
    assert!(
        is_written(pump.request(CommitTrigger::ToolResult, &ctx, &renderer)),
        "T-3 is never throttled"
    );
    assert!(
        is_written(pump.request(CommitTrigger::PreDispatch, &ctx, &renderer)),
        "T-2 is never throttled, even immediately after another write"
    );

    // Once the floor has elapsed, the throttled class writes again.
    tokio::time::sleep(Duration::from_millis(320)).await;
    assert!(
        is_written(pump.request(CommitTrigger::WorkerEdge, &ctx, &renderer)),
        "T-5 writes once the floor has elapsed"
    );

    let after = marmennill::ui::session::session_commit_ledger();
    assert_eq!(
        diff(&after, &before, |l| l.written),
        5,
        "exactly 5 bundles for 29 requests"
    );
    assert_eq!(diff(&after, &before, |l| l.throttled), 24);
    assert_eq!(diff(&after, &before, |l| l.requests), 29);
    assert_eq!(
        std::fs::read_dir(bundle_in(tmp.path()))
            .map(|entries| entries.count())
            .unwrap_or(0),
        3,
        "the bundle holds manifest + manager + ui, nothing else"
    );
}

// --------------------------------------------------------------------- (c)

#[tokio::test]
async fn pre_dispatch_commit_captures_the_dangling_tool_call_batch() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let mut pump = pump_at(tmp.path(), &cfg, "t023-predispatch");
    let ctx = engine_with_dangling_call();
    let renderer = MirrorRenderer::new();

    // Precondition: the engine really is mid-round — a call with no result.
    assert!(
        matches!(
            ctx.capture_manager_section().dangling,
            marmennill::session::DanglingToolCallState::Dangling { calls } if calls.len() == 1
        ),
        "the fixture must be dangling before dispatch"
    );

    let before = marmennill::ui::session::session_commit_ledger();
    let decision = pump.request(CommitTrigger::PreDispatch, &ctx, &renderer);
    let after = marmennill::ui::session::session_commit_ledger();
    assert!(is_written(decision), "T-2 must write: {decision:?}");

    assert_eq!(diff(&after, &before, |l| l.pre_dispatch), 1);
    assert!(
        diff(&after, &before, |l| l.dangling_commits) >= 1,
        "the pre-dispatch commit must be witnessed as dangling"
    );
    assert_eq!(after.last_dangling_calls, 1);

    // The dangling batch is actually IN the member the loader would read.
    let bundle = bundle_in(tmp.path());
    let manager = read_bundle_file(&bundle, MANAGER_FILE);
    assert!(
        manager.contains("call-t023-pre") || manager.contains("call-t023-dangling"),
        "the dispatched call id must be persisted before dispatch: {manager}"
    );
    assert_eq!(ending_on_disk(&bundle), "inProgress");
}

// --------------------------------------------------------------------- (d)

#[tokio::test]
async fn clean_tail_trigger_records_the_clean_tail_ending() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut pump = pump_at(tmp.path(), &Config::default(), "t023-cleantail");
    let ctx = engine();
    let renderer = MirrorRenderer::new();

    // Mid-run commits say InProgress; the tail says CleanTail. Mandatory, so the
    // floor never skips the last word of a session.
    assert!(is_written(pump.request(
        CommitTrigger::ManagerTurnBoundary,
        &ctx,
        &renderer
    )));
    assert_eq!(ending_on_disk(&bundle_in(tmp.path())), "inProgress");
    assert!(is_written(pump.request(
        CommitTrigger::CleanTail,
        &ctx,
        &renderer
    )));

    let bundle = bundle_in(tmp.path());
    assert_eq!(ending_on_disk(&bundle), "cleanTail");
    assert_eq!(
        marmennill::session::commit::last_manifest(&bundle).map(|m| m.ending),
        Some(ProcessEnding::CleanTail)
    );
    assert_eq!(
        marmennill::ui::session::session_commit_ledger().last_ending,
        Some(ProcessEnding::CleanTail)
    );
}

// --------------------------------------------------------------------- (e)

#[tokio::test]
async fn session_disabled_writes_nothing_at_any_trigger() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config {
        session_enabled: false,
        ..Config::default()
    };
    let mut pump = pump_at(tmp.path(), &cfg, "t023-disabled");
    let ctx = engine();
    let renderer = MirrorRenderer::new();

    let before = marmennill::ui::session::session_commit_ledger();
    for trigger in [
        CommitTrigger::ManagerTurnBoundary,
        CommitTrigger::PreDispatch,
        CommitTrigger::ToolResult,
        CommitTrigger::UiEvent,
        CommitTrigger::WorkerEdge,
        CommitTrigger::CleanTail,
    ] {
        assert_eq!(
            pump.request(trigger, &ctx, &renderer),
            CommitDecision::Disabled,
            "{trigger:?} must be disabled"
        );
    }
    let after = marmennill::ui::session::session_commit_ledger();
    assert_eq!(diff(&after, &before, |l| l.disabled), 6);
    assert_eq!(diff(&after, &before, |l| l.written), 0);
    assert!(
        !bundle_in(tmp.path()).exists(),
        "session_enabled = false must not create the bundle directory"
    );
}

#[tokio::test]
async fn tool_result_commits_are_suppressed_when_the_switch_is_off() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config {
        session_commit_on_tool_result: false,
        ..Config::default()
    };
    let mut pump = pump_at(tmp.path(), &cfg, "t023-switch");
    let ctx = engine();
    let renderer = MirrorRenderer::new();

    assert_eq!(
        pump.request(CommitTrigger::ToolResult, &ctx, &renderer),
        CommitDecision::Suppressed,
        "session_commit_on_tool_result = false gates T-3"
    );
    // The mandatory triggers keep working — the switch must not disable the loop.
    assert!(is_written(pump.request(
        CommitTrigger::ManagerTurnBoundary,
        &ctx,
        &renderer
    )));
    assert_eq!(ending_on_disk(&bundle_in(tmp.path())), "inProgress");
}

// ------------------------------------------------- worker lifecycle edge (4)

#[tokio::test]
async fn worker_lifecycle_edge_is_harvested_on_the_session_thread() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let before = marmennill::ui::session::session_commit_ledger();

    // A worker edge is recorded (as `orchestrator::workers` does from a worker
    // thread) and consumed exactly once (R-TRIG-0: no write off-thread, ever).
    marmennill::ui::session::mark_worker_lifecycle_edge();
    assert!(marmennill::ui::session::take_worker_lifecycle_edge());
    assert!(
        !marmennill::ui::session::take_worker_lifecycle_edge(),
        "the edge flag must be consumed, not left latching"
    );

    // A live worker reaches the next commit as a workers/* member.
    let guard = marmennill::orchestrator::register_active_worker_with_token(
        Some("t023-edge".to_string()),
        "coder".to_string(),
        "prove the worker mirror is harvested".to_string(),
        None,
    );
    let mut pump = pump_at(tmp.path(), &Config::default(), "t023-worker");
    let ctx = engine();
    let renderer = MirrorRenderer::new();
    assert!(
        is_written(pump.request(CommitTrigger::WorkerEdge, &ctx, &renderer)),
        "T-5 outside the floor must write"
    );
    let members = members_on_disk(&bundle_in(tmp.path()));
    assert!(
        members.iter().any(|rel| rel.starts_with("workers/")),
        "the live worker must appear as a workers/* member: {members:?}"
    );
    drop(guard);
    assert_eq!(
        diff(
            &marmennill::ui::session::session_commit_ledger(),
            &before,
            |l| l.worker_edge
        ),
        1
    );
}

// ------------------------------------------------------- end-to-end (c) + (d)

/// A whole session against a mock backend: turn boundary, pre-dispatch (with a
/// genuinely dangling batch), tool result, and the loop tail.
async fn run_one_session(dir: &Path, first: String, second: String) -> serde_json::Value {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(first))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(second))
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());
    let plan = Plan::at(dir);
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    // `read_input()` is exhausted after the goal, so the loop falls to its tail.
    let mut renderer = MirrorRenderer::new();
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        marmennill::ui::run_session(
            &cfg,
            &mut renderer,
            Some("goal: prove the commit triggers".to_string()),
            Some(manager),
        ),
    )
    .await
    .expect("the session loop must terminate");
    outcome.expect("the session loop must succeed");
    eprintln!(
        "t023 e2e run finished in {:?}, backend calls seen: {}",
        started.elapsed(),
        server.received_requests().await.unwrap_or_default().len()
    );

    manifest_on_disk(&bundle_in(dir))
}

#[tokio::test]
async fn a_real_turn_writes_a_bundle_and_the_loop_tail_writes_clean_tail() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let before = marmennill::ui::session::session_commit_ledger();

    let manifest = run_one_session(
        tmp.path(),
        text_sse("done, nothing to call"),
        text_sse("fin"),
    )
    .await;

    let after = marmennill::ui::session::session_commit_ledger();
    assert!(
        diff(&after, &before, |l| l.turn_boundary) >= 1,
        "T-1 must fire at a turn boundary"
    );
    assert_eq!(
        diff(&after, &before, |l| l.clean_tail),
        1,
        "T-9 must fire exactly once, at the loop tail"
    );
    assert!(
        diff(&after, &before, |l| l.written) >= 2,
        "turn boundary + tail"
    );
    assert!(
        diff(&after, &before, |l| l.failed) == 0,
        "no commit may fail on the happy path"
    );

    // The whole point of T-9: the manifest survives as `cleanTail`, so a later
    // boot can classify this apart from `HardKillInferred`.
    assert_eq!(manifest["ending"]["kind"].as_str(), Some("cleanTail"));
    let members = manifest["members"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|m| m["rel_path"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert!(members.contains(&MANAGER_FILE.to_string()), "{members:?}");
    assert!(members.contains(&UI_FILE.to_string()), "{members:?}");
    let manager = read_bundle_file(&bundle_in(tmp.path()), MANAGER_FILE);
    assert!(
        manager.contains("done, nothing to call"),
        "the manager member must hold the transcript of the run"
    );
}

#[tokio::test]
async fn a_tool_round_is_committed_pre_dispatch_while_the_batch_is_dangling() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let before = marmennill::ui::session::session_commit_ledger();

    let manifest = run_one_session(tmp.path(), one_tool_call_sse(), text_sse("all done")).await;
    let after = marmennill::ui::session::session_commit_ledger();

    assert!(
        diff(&after, &before, |l| l.pre_dispatch) >= 1,
        "a commit must be requested before the batch is dispatched"
    );
    assert_eq!(
        manifest["ending"]["kind"].as_str(),
        Some("cleanTail"),
        "the run still ended on a clean tail"
    );

    // The T-2 witness: at least one committed manager section contained an
    // assistant `tool_calls` batch with no matching tool result yet, i.e. the
    // dangling state a crash below this point would leave for L8 to normalize.
    let dangling_witness = diff(&after, &before, |l| l.dangling_commits);
    let dangling_calls = after.last_dangling_calls;
    let manager = read_bundle_file(&bundle_in(tmp.path()), MANAGER_FILE);
    let call_persisted = manager.contains("call-t023-pre");
    assert!(
        call_persisted && (dangling_witness >= 1 || dangling_calls >= 1),
        "pre-dispatch must persist the batch while it is dangling \
         (dangling_commits delta {dangling_witness}, last_dangling_calls {dangling_calls}, \
         call persisted {call_persisted})"
    );
}
