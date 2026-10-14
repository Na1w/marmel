//! t-024 — the paths that make an **interrupted** session restorable.
//!
//! Flow `docs/design_session_restore_flow.md` §1.5–§1.9 pins what a session must
//! leave behind when it does **not** finish normally. This file pins those four
//! paths end to end, off the disk (never off an in-process cache):
//!
//! * **(a) T-6/T-7 user abort** — a user exit taken *inside* a round still gets a
//!   **mandatory** (never throttled) bundle commit whose manifest says
//!   `ending.kind == "userAbort"`;
//! * **T-8 panic** — a panic anywhere in the process (session thread **or** another
//!   thread) leaves the last committed bundle readable and re-marked
//!   `panicHook`, with the member bytes kept from the last successful commit; if
//!   the flush cannot be honest it writes **nothing** and says why;
//! * **(c) hard-kill inference** — a bundle whose last manifest says `InProgress`
//!   (the writer never recorded its own death) is classified
//!   `HardKillInferred` by the next boot **and said out loud** (status line,
//!   journal, N-MGR-1 notice), while the bundle bytes stay `inProgress` (A D6);
//! * **(d) T-10 `/reset`** — the bundle is marked `reset` **before** the plan sweep
//!   runs, `Plan::clear()` → `sweep_session_state()` deletes it, and the
//!   post-sweep confirmation reports the truth.
//!
//! State discipline: the commit ledger, the commit-diff cache and the panic-flush
//! target are process-global, so every case takes the mutex, and the ledger
//! assertions are **deltas**.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use marmennill::config::Config;
use marmennill::manager::context::ContextEngine;
use marmennill::manager::phase::Plan;
use marmennill::session::{MANAGER_FILE, ProcessEnding, RestoreOutcome, UI_FILE};
use marmennill::types::Message;
use marmennill::ui::session::{
    BootSink, CommitDecision, CommitTrigger, SessionCommitLedger, SessionCommitPump,
};
use marmennill::ui::{Event, InputState, Renderer, UiRecord, UiTranscript};

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

fn ledger() -> SessionCommitLedger {
    marmennill::ui::session::session_commit_ledger()
}

/// A renderer with a pane model (so `capture_ui` has something honest to give)
/// that also records every event, which is how "said out loud" is asserted.
struct LoudRenderer {
    input_state: InputState,
    events: Vec<Event>,
    chat: Vec<String>,
}

impl LoudRenderer {
    fn new() -> Self {
        Self {
            input_state: InputState::default(),
            events: Vec::new(),
            chat: vec![
                "User: t024 goal".to_string(),
                "[Tool Call] read_file".to_string(),
            ],
        }
    }

    /// Every `Event::Status` text this renderer was given.
    fn statuses(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Status(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Renderer for LoudRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        self.events.push(event.clone());
    }
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
            status_line: "t024 status".to_string(),
            journal: vec![marmennill::session::UiJournalRecord::User {
                text: "t024 goal".to_string(),
            }],
            ..marmennill::session::UiSection::default()
        }
    }
}

/// Renderer that asks for the user exit **inside** the round, the instant the
/// first `Event::ToolCall` is rendered (the deterministic shape of "the user hit
/// Ctrl-C / `/abort` mid-round"; the two-stage confirm in `ui/tui/events.rs`
/// collapses to exactly this flag flip).
struct ExitOnFirstToolCallRenderer {
    loud: LoudRenderer,
    exit_fired: bool,
}

impl ExitOnFirstToolCallRenderer {
    fn new() -> Self {
        Self {
            loud: LoudRenderer::new(),
            exit_fired: false,
        }
    }
}

impl Renderer for ExitOnFirstToolCallRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        self.loud.on_event(event);
        if !self.exit_fired && matches!(event, Event::ToolCall(_)) {
            self.exit_fired = true;
            self.request_user_exit();
        }
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_input(&mut self) -> Option<String> {
        None
    }
    fn input_state(&mut self) -> &mut InputState {
        &mut self.loud.input_state
    }
    fn input_state_shared(&self) -> &InputState {
        &self.loud.input_state
    }
    fn shutdown(&mut self) {}
    fn capture_ui(&self) -> marmennill::session::UiSection {
        self.loud.capture_ui()
    }
}

/// A minimal but legal transcript: the pinned `[0]`/`[1]` pair every member
/// header and every L1–L2 gate expects.
fn engine() -> ContextEngine {
    let mut ctx = ContextEngine::new(200_000);
    ctx.append(Message::System {
        content: "t024 system prompt".to_string(),
    });
    ctx.append(Message::User {
        content: "t024 goal: prove the abort and crash paths".to_string(),
    });
    ctx
}

fn pump_at(dir: &Path, cfg: &Config, session_id: &str) -> SessionCommitPump {
    SessionCommitPump::new(&Plan::at(dir), cfg, session_id)
}

fn bundle_in(dir: &Path) -> PathBuf {
    marmennill::session::bundle_dir(&Plan::at(dir))
}

fn read_bundle_file(bundle: &Path, rel: &str) -> String {
    std::fs::read_to_string(bundle.join(rel))
        .unwrap_or_else(|err| panic!("{} must exist in {}: {err}", rel, bundle.display()))
}

fn manifest_on_disk(bundle: &Path) -> serde_json::Value {
    let text = read_bundle_file(bundle, marmennill::session::MANIFEST_FILE);
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

fn config_for_backend(backend: &str) -> Config {
    Config {
        backend_url: format!("{backend}/v1"),
        system_prompt_path: PathBuf::from("prompts/system.md"),
        ui_mode: "tui".to_string(),
        ..Config::default()
    }
}

fn sse(body: serde_json::Value) -> String {
    format!("data: {}\n\ndata: [DONE]\n\n", body)
}

fn text_sse(text: &str) -> String {
    sse(serde_json::json!({
        "id": "chatcmpl-t024",
        "choices": [{ "delta": { "content": text }, "finish_reason": null }]
    }))
}

fn one_tool_call_sse() -> String {
    sse(serde_json::json!({
        "id": "chatcmpl-t024-tool",
        "choices": [{
            "delta": { "tool_calls": [{
                "index": 0,
                "id": "call-t024-abort",
                "type": "function",
                "function": { "name": "read_file", "arguments": "{\"path\": \"Cargo.toml\"}" }
            }]},
            "finish_reason": null
        }]
    }))
}

// --------------------------------------------------------- (a) T-6/T-7 abort

/// A whole session that the user stops mid-round: the abort break must still run
/// a **mandatory** commit, and the manifest on disk must name the ending.
#[tokio::test]
async fn a_user_abort_mid_round_leaves_a_manifest_marked_user_abort() {
    let _lock = TEST_MUTEX.lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let tmp = tempfile::tempdir().expect("tempdir");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(one_tool_call_sse()))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(text_sse("after the abort")))
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());
    let plan = Plan::at(tmp.path());
    let manager = Arc::new(marmennill::orchestrator::OrchestratorManager::new(
        marmennill::llm::ChatClient::new(server.uri(), "marmel-manager"),
        plan,
        Arc::new(marmennill::harness::HarnessStats::new()),
    ));

    let before = ledger();
    let mut renderer = ExitOnFirstToolCallRenderer::new();
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        marmennill::ui::run_session(
            &cfg,
            &mut renderer,
            Some("t024 goal: prove the abort and crash paths".to_string()),
            Some(manager),
        ),
    )
    .await
    .expect("the session loop must terminate after the user exit");
    outcome.expect("the aborted session loop must still return Ok");
    let after = ledger();

    assert!(
        renderer.exit_fired,
        "the fixture must really have asked for the exit mid-round"
    );
    assert!(
        diff(&after, &before, |l| l.user_abort) >= 1,
        "an abort must be witnessed as a UserAbort commit"
    );
    assert_eq!(
        after.last_ending,
        Some(ProcessEnding::UserAbort),
        "the LAST committed ending of an aborted session is userAbort"
    );

    let bundle = bundle_in(tmp.path());
    assert_eq!(
        ending_on_disk(&bundle),
        "userAbort",
        "flow §1.5: the manifest must say the user stopped this run"
    );
    // Mandatory: a UserAbort request is never answered with a throttle, and the
    // bundle carries the transcript of the run that was stopped.
    let members = members_on_disk(&bundle);
    assert!(members.contains(&MANAGER_FILE.to_string()), "{members:?}");
    assert!(members.contains(&UI_FILE.to_string()), "{members:?}");
    assert!(
        read_bundle_file(&bundle, MANAGER_FILE).contains("prove the abort and crash paths"),
        "the aborted run's transcript must be in the member a --resume reads"
    );
}

// ------------------------------------------------------------------- T-8 panic

/// A crash leaves a bundle a later `--resume` can open — marked `panicHook`, from
/// a panic on the session thread **and** from a panic on another thread — and an
/// honest flush that cannot be sure of its bytes writes nothing at all.
#[tokio::test]
async fn a_panic_re_marks_the_last_committed_bundle_panic_hook() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let mut pump = pump_at(tmp.path(), &cfg, "t024-panic");
    let ctx = engine();
    let renderer = LoudRenderer::new();

    // The run gets far enough to have committed at least once: that is exactly the
    // state a crash is interesting in (before the first commit there are no bytes).
    assert!(is_written(pump.request(
        CommitTrigger::ManagerTurnBoundary,
        &ctx,
        &renderer
    )));
    let bundle = bundle_in(tmp.path());
    assert_eq!(ending_on_disk(&bundle), "inProgress");

    // The hook `src/main.rs` installs, minus the default printer: flush, never
    // block, never build a runtime.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_info| {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            marmennill::ui::session::commit_from_panic,
        ));
    }));

    let before = ledger();
    let on_session_thread = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        panic!("t024: crash inside the session loop");
    }));
    assert!(on_session_thread.is_err(), "the panic must be reachable");

    // And the same crash on a thread that is NOT the session thread: the flush may
    // only touch process-global state, which is what makes it legal there.
    let worker = std::thread::spawn(|| {
        let crashed = std::panic::catch_unwind(|| panic!("t024: crash on a worker thread"));
        assert!(crashed.is_err());
    });
    worker.join().expect("the worker thread panic was caught");
    std::panic::set_hook(previous_hook);

    let after = ledger();
    assert!(
        diff(&after, &before, |l| l.panic_hook) >= 2,
        "both panics must reach the flush"
    );
    assert_eq!(
        diff(&after, &before, |l| l.failed),
        0,
        "neither flush may fail: the lock is free and the cache holds this bundle"
    );

    assert_eq!(
        ending_on_disk(&bundle),
        "panicHook",
        "flow §1.8: a crashed run must not leave a manifest claiming inProgress"
    );
    assert_eq!(
        after.last_ending,
        Some(ProcessEnding::PanicHook),
        "the ledger records the ending the hook wrote"
    );

    // The re-marked bundle is still a bundle: every member it lists is on disk and
    // the loader does not refuse it.
    let members = members_on_disk(&bundle);
    assert!(members.contains(&MANAGER_FILE.to_string()), "{members:?}");
    assert!(members.contains(&UI_FILE.to_string()), "{members:?}");
    assert!(
        read_bundle_file(&bundle, MANAGER_FILE).contains("prove the abort and crash paths"),
        "the member bytes are kept from the last successful commit (A §7.1 C2)"
    );
    assert!(
        !matches!(
            marmennill::session::restore::load_bundle(&bundle),
            RestoreOutcome::Refused { .. } | RestoreOutcome::NoBundle { .. }
        ),
        "a panicHook bundle must stay loadable"
    );

    // ---------------------------------------- the degradation, also said out loud
    // A second session that never committed cannot be marked from another
    // session's cached bytes: the flush refuses, writes nothing, and explains.
    let other = tempfile::tempdir().expect("tempdir");
    let other_pump = pump_at(other.path(), &cfg, "t024-panic-nothing-committed");
    let outcome = marmennill::ui::session::commit_from_panic();
    assert!(
        !outcome.written,
        "a flush with no commit for its own bundle must write nothing: {outcome:?}"
    );
    assert!(
        outcome
            .detail
            .contains("refusing to mark a bundle that is not this session's"),
        "the degradation must name the reason: {}",
        outcome.detail
    );
    assert!(
        !bundle_in(other.path()).exists(),
        "a refused flush creates no bundle at all"
    );
    assert_eq!(
        ending_on_disk(&bundle),
        "panicHook",
        "a refused flush must not touch the real bundle either"
    );
    assert!(
        diff(&ledger(), &after, |l| l.failed) >= 1,
        "A D6: a failed flush is counted, never swallowed"
    );
    // The pump itself is untouched by any of this: it still has nothing committed,
    // so the ending its own writer would write is the one a boot reads.
    assert!(
        !other_pump.has_committed(),
        "a refused flush must not count as a commit for its pump"
    );
}

// ------------------------------------------- T-8 degradation: lock already held

/// The one case the hook must never turn into a hang: the panic unwound *inside*
/// a commit, so the session thread already holds the commit lock. The hook tries
/// the lock, fails, and leaves the last committed manifest (`inProgress`) alone —
/// which is precisely the state the next boot classifies as a hard kill.
#[tokio::test]
async fn a_panic_while_the_commit_lock_is_held_degrades_instead_of_blocking() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let mut pump = pump_at(tmp.path(), &cfg, "t024-panic-locked");
    let ctx = engine();
    let renderer = LoudRenderer::new();

    assert!(is_written(pump.request(
        CommitTrigger::ManagerTurnBoundary,
        &ctx,
        &renderer
    )));
    let bundle = bundle_in(tmp.path());
    assert_eq!(ending_on_disk(&bundle), "inProgress");

    // Stand in for "the commit was in flight when the panic started unwinding".
    // `CommitLock::try_guard` is non-blocking by contract, so this case proves the
    // hook cannot deadlock against its own owner: it comes back with an outcome,
    // not a hang.
    let in_flight = pump
        .commit_lock()
        .try_guard()
        .expect("the commit lock starts free");
    let before = ledger();
    let outcome = marmennill::ui::session::commit_from_panic();
    drop(in_flight);
    let after = ledger();

    assert!(
        !outcome.written,
        "the hook must not force its way past an in-flight commit: {outcome:?}"
    );
    assert!(
        outcome.detail.contains("left as it was last committed")
            && outcome.detail.contains("HardKillInferred"),
        "the degradation must say what happens next: {}",
        outcome.detail
    );
    assert_eq!(
        ending_on_disk(&bundle),
        "inProgress",
        "a degraded flush rewrites nothing"
    );
    assert_eq!(
        diff(&after, &before, |l| l.panic_hook),
        1,
        "the attempt is still counted and logged (A D6: no swallowed outcome)"
    );
    assert!(
        diff(&after, &before, |l| l.failed) >= 1,
        "and counted as a failure, never as a success"
    );
}

// -------------------------------------------------- (c) hard kill inference

/// The SIGKILL shape: the repo installs no signal handler, so the only trace of a
/// killed run is a manifest still saying `InProgress`. The next boot must classify
/// that as `HardKillInferred` and **say so in all three places** — while the
/// bundle bytes keep the ending the writer actually wrote (A D6).
#[tokio::test]
async fn a_run_left_in_progress_is_classified_hard_kill_and_said_out_loud() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let plan = Plan::at(tmp.path());
    let mut pump = SessionCommitPump::new(&plan, &cfg, "t024-hardkill");
    let ctx = engine();
    let renderer = LoudRenderer::new();

    assert!(is_written(pump.request(
        CommitTrigger::ManagerTurnBoundary,
        &ctx,
        &renderer
    )));
    let bundle = bundle_in(tmp.path());
    assert_eq!(
        ending_on_disk(&bundle),
        "inProgress",
        "the writer never got to record its own death"
    );

    // The next process boots.
    let mut boot_ctx = marmennill::ui::session::build_restored_context(&cfg);
    let mut loud = LoudRenderer::new();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let mut sink = BootSink {
        ui_transcript: &mut transcript,
    };
    let report = marmennill::ui::session::boot(
        &plan,
        &cfg,
        &mut boot_ctx,
        &mut loud,
        &mut session_id,
        &mut sink,
    )
    .expect("an inProgress bundle is restorable, not a refusal");

    assert!(report.restored, "the run must actually be rehydrated");
    assert_eq!(
        report.previous_ending,
        Some(ProcessEnding::HardKillInferred),
        "flow §1.9: inProgress + no terminal marker = HardKillInferred"
    );

    // Loud #1 — the renderer's status line.
    let statuses = loud.statuses();
    assert!(
        statuses
            .iter()
            .any(|line| line.contains("HardKillInferred")),
        "the status line must say it: {statuses:?}"
    );
    // Loud #2 — the journaled transcript (third destination of A §8.4).
    let journaled: Vec<String> = transcript
        .records()
        .iter()
        .filter_map(|record| match record {
            UiRecord::Status { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        journaled
            .iter()
            .any(|line| line.contains("HardKillInferred")),
        "the journal must say it: {journaled:?}"
    );
    // Loud #3 — the N-MGR-1 notice handed to the manager.
    let notice = report.restore_notice.clone().expect("N-MGR-1 ran");
    assert!(
        notice.contains("previous ending: HardKillInferred")
            && notice.contains("stored ending: InProgress"),
        "the manager is told derived + stored ending, not a guess: {notice}"
    );

    // A D6: inferring is reporting, never rewriting. The bundle still carries the
    // ending its own writer recorded.
    assert_eq!(
        ending_on_disk(&bundle),
        "inProgress",
        "the loader must not rewrite bundle bytes to say HardKillInferred"
    );
    assert_eq!(session_id, report.session_id, "identity continuity");
}

// ------------------------------------------------------------- (d) T-10 reset

/// `/reset` is the one ending that must **not** be restorable. The order is the
/// contract: mark the bundle, then let the plan sweep delete it, then confirm the
/// delete really happened.
#[tokio::test]
async fn reset_marks_the_bundle_and_the_plan_sweep_sweeps_it_away() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let plan = Plan::at(tmp.path());
    let mut pump = SessionCommitPump::new(&plan, &cfg, "t024-reset");
    let ctx = engine();
    let renderer = LoudRenderer::new();

    let before = ledger();
    assert!(is_written(pump.request(
        CommitTrigger::ManagerTurnBoundary,
        &ctx,
        &renderer
    )));
    let bundle = bundle_in(tmp.path());
    assert!(bundle.is_dir(), "there is a bundle to end");

    // Step 1 — mark BEFORE the sweep, so a crash in between leaves a bundle that
    // already says `reset`, never one still claiming `inProgress`.
    let decision = marmennill::ui::session::mark_ending_commit(
        &mut pump,
        CommitTrigger::Reset,
        &ctx,
        &renderer,
    );
    assert!(
        is_written(decision),
        "an ending commit is mandatory: {decision:?}"
    );
    assert_eq!(
        ending_on_disk(&bundle),
        "reset",
        "flow §1.5: /reset must stamp the manifest before anything is deleted"
    );
    assert_eq!(ledger().last_ending, Some(ProcessEnding::Reset));

    // Step 2 — the plan sweep the loop runs for `/reset`.
    plan.create("# Plan\n\n- [ ] [t-999] prove the reset sweep\n")
        .expect("plan create");
    plan.clear().expect("Plan::clear");
    assert!(
        !plan.exists(),
        "Plan::clear deletes the active plan file it is asked to clear"
    );
    assert!(
        !bundle.exists(),
        "sweep_session_state must delete the bundle (A D3: a reset session is gone)"
    );

    // Step 3 — the confirmation the loop runs after the sweep.
    assert!(
        marmennill::ui::session::confirm_bundle_swept(
            &mut pump,
            CommitTrigger::Reset,
            &plan,
            &ctx,
            &renderer,
        ),
        "the sweep really removed the bundle"
    );
    let after = ledger();
    assert!(
        diff(&after, &before, |l| l.reset) >= 1,
        "the reset ending must be witnessed in the ledger"
    );
    assert!(
        diff(&after, &before, |l| l.failed) == 0,
        "nothing on this path may fail: {after:?}"
    );
    assert!(
        !bundle.exists(),
        "confirmation must not resurrect a swept bundle"
    );

    // And the user-visible consequence: the next boot finds nothing to restore.
    let mut boot_ctx = marmennill::ui::session::build_restored_context(&cfg);
    let mut loud = LoudRenderer::new();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let mut sink = BootSink {
        ui_transcript: &mut transcript,
    };
    let report = marmennill::ui::session::boot(
        &plan,
        &cfg,
        &mut boot_ctx,
        &mut loud,
        &mut session_id,
        &mut sink,
    )
    .expect("no bundle is a normal outcome, not an error");
    assert!(
        !report.restored,
        "a reset session must never be resurrected"
    );
    assert!(
        !report.no_bundle_lines.is_empty(),
        "and saying so is part of the contract (flow §3.5)"
    );
}

// ------------------------------------------------------- plan archive (T-6)

/// `archive_plan` runs as a **tool**, possibly on another thread, and it sweeps
/// the bundle with the plan. Only the session thread may write, so the loop
/// detects the archive at the round boundary and does the same ordered pair:
/// mark `planArchived`, then confirm the sweep.
#[tokio::test]
async fn an_archived_plan_ends_the_session_bundle_with_plan_archived() {
    let _lock = TEST_MUTEX.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::default();
    let plan = Plan::at(tmp.path());
    let mut pump = SessionCommitPump::new(&plan, &cfg, "t024-archive");
    let ctx = engine();
    let renderer = LoudRenderer::new();

    let before = ledger();
    assert!(is_written(pump.request(
        CommitTrigger::ManagerTurnBoundary,
        &ctx,
        &renderer
    )));
    let bundle = bundle_in(tmp.path());
    assert!(bundle.is_dir());

    // The plan is complete, so `archive()` really archives (t-203: an incomplete
    // plan is the working checkpoint and is never archived).
    plan.create("# Plan\n\n- [x] [t-998] prove the archive ending\n")
        .expect("plan create");
    let decision = marmennill::ui::session::mark_ending_commit(
        &mut pump,
        CommitTrigger::PlanArchived,
        &ctx,
        &renderer,
    );
    assert!(is_written(decision), "mandatory: {decision:?}");
    assert_eq!(
        ending_on_disk(&bundle),
        "planArchived",
        "flow §1.5: an archived plan is a finished session, not an interrupted one"
    );

    let archived = plan.archive().expect("archive").expect("plan is complete");
    assert!(archived.is_file(), "the archive landed: {archived:?}");
    assert!(
        !plan.exists(),
        "the active plan file is gone: this is the fingerprint the loop looks for"
    );
    assert!(
        marmennill::ui::session::confirm_bundle_swept(
            &mut pump,
            CommitTrigger::PlanArchived,
            &plan,
            &ctx,
            &renderer,
        ),
        "Plan::archive swept the bundle, and the confirmation reports it"
    );
    let after = ledger();
    assert!(
        diff(&after, &before, |l| l.plan_archived) >= 1,
        "the ending must be witnessed in the ledger"
    );
    assert!(!bundle.exists(), "a finished session is not resurrectable");
}
