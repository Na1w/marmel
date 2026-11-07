//! The **resume precedence matrix**, end to end through the real binary.
//!
//! `src/ui/session_tests.rs` pins the unit-level resolution (`resume_intent`) and
//! `tests/test_session_boot.rs` pins the boot pass itself. This file is the one
//! place that pins the *whole* precedence order as an operator observes it: for
//! every combination of on-disk plan state, CLI flag, environment variable and
//! `session_enabled`, it asserts **whether the previous session was restored**
//! (the restored pane verdict on stdout) and **the exit code**, in one table of
//! rows that is easy to read against the documented order:
//!
//! ```text
//!   1. --fresh                                        -> Fresh (escape hatch)
//!   2. --resume / MARMEL_RESUME / session_resume=true -> Explicit
//!   3. session_enabled && plan is Unfinished          -> Implicit (the new default)
//!   4. everything else                                -> Fresh (fail closed)
//! ```
//!
//! The two restore intents differ only in their failure policy: an explicit
//! request ends on `EXIT_RESTORE_REFUSED` (3) when the bundle exists but is
//! refused, while the implicit default degrades to a fresh start and can never
//! produce exit 3 — rows 14 and 15 pin exactly that pair.
//!
//! Every case runs the real `marmel` binary with its working directory set to a
//! private temp directory, so `Plan::default()`'s `.marmel/` — and therefore the
//! bundle and the plan the decision reads — is inside that temp dir. Nothing
//! here touches the workspace's own `.marmel/`, the network or the user's home.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marmennill::agents::Agent;
use marmennill::manager::phase::{MARMEL_DIR, PLAN_FILE, Plan};
use marmennill::session::commit::{CapturePayload, CommitLock, CommitPolicy, write_bundle};
use marmennill::session::format::TimePoint;
use marmennill::session::{
    DanglingToolCallState, HistorySource, IdentityFloors, ManagerSection, MessageRecord,
    PinWitness, ProcessEnding, ToolCallRecord, ToolFunctionRecord, UiJournalRecord, UiSection,
    WorkerSection, WorkerStatus,
};

// ---------------------------------------------------------------- constants

/// The first half of the restored-session verdict, as the panes show it. It is
/// the only line in the whole matrix that proves a bundle was really rehydrated:
/// it is emitted after the loader installed the session, so no fresh start can
/// print it.
const RESTORE_VERDICT: &str = "Session restored (session id session-matrix-e2e";

/// The loud fresh start an **explicit** request gets when there is no bundle at
/// all (`no_bundle_report`). A default nobody asked for must never say it.
const NO_BUNDLE_VERDICT: &str = "No session bundle at";

/// Capture on, no resume requested in the file: the plain configuration every
/// plan-driven row starts from.
const CAPTURE_ON: &str = "session_enabled = true\nsession_resume = false\n";

/// Capture off. `--resume` under it is a usage error (row 11), while the
/// plan-driven default must not even be considered (rows 12 and 13).
const CAPTURE_OFF: &str = "session_enabled = false\nsession_resume = false\n";

/// A plan with one open task: the only state that may drive an implicit resume.
const UNFINISHED_PLAN: &str = "- [ ] [t-022] the task the previous run left open\n";

/// The same task, checked: a finished plan is nothing to continue.
const COMPLETE_PLAN: &str = "- [x] [t-022] the task the previous run finished\n";

/// Non-empty content the task grammar recognises nothing in. It is neither
/// "done" nor "nothing was written", so the state is `Unknown` and the default
/// must fail closed rather than guess (row 5).
const UNRECOGNISED_PLAN: &str = "# Plan\n\nprose the task grammar does not understand\n";

/// Exit status Rust produces when a panic unwinds `main`. The runner clears the
/// environment, so on a host without a usable TLS trust store the child must
/// still reach the boot path instead of aborting in HTTP-client construction.
const PANIC_EXIT_CODE: i32 = 101;

// ---------------------------------------------------------------- fixtures

/// Where the process under test looks for its bundle: `main` boots its manager
/// with `Plan::default()`, whose directory is [`MARMEL_DIR`] **relative to the
/// process working directory**.
fn bundle_dir_in(dir: &Path) -> PathBuf {
    marmennill::session::bundle_dir(&Plan::at(dir.join(MARMEL_DIR)))
}

fn transcript() -> Vec<MessageRecord> {
    vec![
        MessageRecord::System {
            content: "system prompt for the resume-matrix test".to_string(),
        },
        MessageRecord::User {
            content: "the end-to-end goal".to_string(),
        },
        MessageRecord::Assistant {
            content: None,
            reasoning_content: Some("thinking".to_string()),
            tool_calls: vec![ToolCallRecord {
                id: "call-matrix-1".to_string(),
                kind: "function".to_string(),
                function: ToolFunctionRecord {
                    name: marmennill::tool_names::TOOL_READ_FILE.to_string(),
                    arguments: "{\"path\":\"src/main.rs\"}".to_string(),
                },
            }],
        },
        MessageRecord::Tool {
            tool_call_id: "call-matrix-1".to_string(),
            content: "file bytes".to_string(),
        },
        MessageRecord::Assistant {
            content: Some(String::new()),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ]
}

fn worker(key: &str) -> WorkerSection {
    WorkerSection {
        worker_key: key.to_string(),
        worker_id_seq_value: 1,
        role: Agent::Coder,
        task_id: Some("t-022".to_string()),
        status: WorkerStatus::InProgress,
        status_label: "running".to_string(),
        started: TimePoint::now(),
        completed: None,
        last_activity: None,
        worked_secs: 3.5,
        completed_seq: None,
        context_tokens: 32,
        last_tokens: 4,
        counters: Default::default(),
        brief: format!("brief for {key}"),
        snippets: vec![],
        image_urls: vec![],
        audio_urls: vec![],
        recursion_granted: false,
        history: vec![],
        history_source: HistorySource::LiveMirror,
        dangling: DanglingToolCallState::Balanced,
        unrecoverable_reason: None,
        non_restorable: vec![],
    }
}

/// A sound bundle: the loader accepts it, so a refusal asserted in rows 14/15 is
/// known to come from the tamper and not from the fixture.
fn payload() -> CapturePayload {
    let messages = transcript();
    let manager = ManagerSection {
        pinned_prefix_end: 2,
        pin_witness: PinWitness {
            system_prompt_len: messages[0].content().map_or(0, |c| c.len()) as u32,
            goal_len: messages[1].content().map_or(0, |c| c.len()) as u32,
        },
        max_context_tokens: 128,
        rebirth_advisory_emitted: false,
        consecutive_rebirths: 0,
        dangling: DanglingToolCallState::observe(&messages),
        messages,
    };
    CapturePayload {
        manager,
        ui: Some(UiSection {
            chat_messages: vec!["User: the end-to-end goal".to_string()],
            journal: vec![UiJournalRecord::User {
                text: "the end-to-end goal".to_string(),
            }],
            ..UiSection::default()
        }),
        workers: vec![worker("t022-matrix-worker")],
        identity_floors: IdentityFloors {
            worker_id_seq_floor: 3,
            completed_seq_floor: 1,
            notice_id_floor: 4,
        },
    }
}

fn write_bundle_at(dir: &Path) -> PathBuf {
    let bundle = bundle_dir_in(dir);
    std::fs::create_dir_all(&bundle).expect("bundle dir");
    let policy = CommitPolicy {
        min_interval: Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    let report = write_bundle(
        &bundle,
        "session-matrix-e2e",
        &payload(),
        ProcessEnding::HardKillInferred,
        &policy,
        &CommitLock::new(),
    )
    .expect("the fixture bundle must commit");
    assert!(
        !report.members_written.is_empty(),
        "the fixture bundle must be a real commit point"
    );
    bundle
}

/// Raise the stored `format_version` past what this binary understands: the
/// version check refuses **before** any member is parsed, so this is a bundle
/// that exists and is refused rather than one that is missing.
fn tamper_format_version(bundle: &Path) {
    let path = bundle.join(marmennill::session::MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).expect("manifest");
    let mut manifest: serde_json::Value = serde_json::from_str(&text).expect("manifest json");
    manifest["format_version"] = serde_json::json!(marmennill::session::FORMAT_VERSION + 5);
    std::fs::write(&path, serde_json::to_vec(&manifest).expect("reserialize")).expect("write");
}

/// Write the execution plan the child will find at `<dir>/.marmel/execution_plan.md`.
fn write_plan(dir: &Path, content: &str) {
    let plan_dir = dir.join(MARMEL_DIR);
    std::fs::create_dir_all(&plan_dir).expect("plan dir");
    std::fs::write(plan_dir.join(PLAN_FILE), content).expect("write the plan file");
}

/// A private workspace with a config file, so `config::load` never falls back to
/// the caller's tree.
fn workspace(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("marmel.toml"), config).expect("write config");
    dir
}

// ------------------------------------------------------------------- runner

/// One run of the real binary, in a private workspace, with stdout and stderr
/// captured to files so a watchdog kill cannot lose output.
#[derive(Debug)]
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn run_marmel(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
    let out_path = dir.join("capture.stdout");
    let err_path = dir.join("capture.stderr");
    let out_file = std::fs::File::create(&out_path).expect("stdout file");
    let err_file = std::fs::File::create(&err_path).expect("stderr file");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marmel"));
    cmd.current_dir(dir)
        .env_clear()
        // A minimal, deterministic environment: the binary must not inherit the
        // test runner's `MARMEL_*` variables or its terminal, and nothing may
        // reach a real model or hang on connect.
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("MARMEL_BACKEND_URL", "http://127.0.0.1:9/v1")
        .env("MARMEL_MODEL", "t022-nonexistent")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));
    for (key, value) in env {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().expect("spawn the marmel binary");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().expect("wait after the watchdog kill");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("wait on the marmel binary: {error}"),
        }
    };

    let read = |path: &Path| {
        let mut buffer = String::new();
        std::fs::File::open(path)
            .and_then(|mut file| file.read_to_string(&mut buffer))
            .expect("read capture");
        buffer
    };
    Run {
        code: status.code(),
        stdout: read(&out_path),
        stderr: read(&err_path),
        timed_out,
    }
}

// --------------------------------------------------------------- assertions

/// The child must reach the session boot path rather than die in TLS setup: no
/// panic banner, and never the panic exit status. The runner clears the
/// environment, so this holds on a host with or without a trust store.
fn assert_no_startup_panic(run: &Run, case: &str) {
    assert!(
        !run.stderr.contains("panicked"),
        "{case}: the run must not abort in a panic; stderr: {:?}",
        run.stderr
    );
    assert_ne!(
        run.code,
        Some(PANIC_EXIT_CODE),
        "{case}: a missing TLS trust store is a stored client verdict, not an abort in `main`: \
         {run:?}"
    );
}

/// The row resolved to a restore: the bundle was rehydrated, the run is an
/// ordinary one, and nothing was refused.
fn assert_restored(run: &Run, case: &str) {
    assert!(
        !run.timed_out,
        "{case}: the boot path returns promptly: {run:?}"
    );
    assert_no_startup_panic(run, case);
    assert_eq!(
        run.code,
        Some(0),
        "{case}: a restore that succeeds is an ordinary run: {run:?}"
    );
    assert!(
        run.stdout.contains(RESTORE_VERDICT),
        "{case}: the panes must say the previous session was restored, got {:?}",
        run.stdout
    );
    assert!(
        !run.stderr.contains("session restore refused"),
        "{case}: a successful restore refuses nothing: {:?}",
        run.stderr
    );
}

/// The row resolved to a fresh start: the bundle was never installed. This also
/// pins that the *explicit* no-bundle verdict is absent — an implicit decision
/// must not speak to a user who asked for nothing — and that the run-stopping
/// refusal verdict never reached the panes.
fn assert_not_restored(run: &Run, case: &str) {
    assert!(
        !run.timed_out,
        "{case}: a fresh start returns promptly: {run:?}"
    );
    assert_no_startup_panic(run, case);
    assert_eq!(
        run.code,
        Some(0),
        "{case}: a fresh start is an ordinary run: {run:?}"
    );
    assert!(
        !run.stdout.contains(RESTORE_VERDICT),
        "{case}: nothing may be restored, got {:?}",
        run.stdout
    );
    assert!(
        !run.stdout.contains(NO_BUNDLE_VERDICT),
        "{case}: a decision nobody asked for must not narrate a bundle it did not open: {:?}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("session restore refused") && !run.stdout.contains("exit code 3"),
        "{case}: the run-stopping verdict is never printed to the panes: {:?}",
        run.stdout
    );
}

/// The boot pass was **never entered**: neither the refusal log line the loader
/// emits when it refuses a bundle, nor the degradation line the implicit pass
/// logs when it answers a refusal with a fresh session. This is what separates
/// "resolved to fresh, so the bundle was not even opened" (rows 2, 3, 4, 5, 12,
/// 13) from "opened, refused and degraded" (row 14).
fn assert_bundle_never_opened(run: &Run, case: &str) {
    assert!(
        !run.stderr.contains("session restore refused")
            && !run.stderr.contains("implicit resume refused"),
        "{case}: nothing may open the bundle, so no refusal may be logged: {:?}",
        run.stderr
    );
}

/// A usage error is resolved in `main` before anything is started: exit `2`, one
/// sentence on stderr, and no bundle read, no restore and no refusal verdict.
fn assert_usage_error(run: &Run, case: &str, sentence: &str) {
    assert!(
        !run.timed_out,
        "{case}: a usage error returns immediately: {run:?}"
    );
    assert_eq!(run.code, Some(2), "{case}: usage errors exit 2: {run:?}");
    assert!(
        run.stderr.contains(sentence),
        "{case}: the sentence must name the contradiction, got {:?}",
        run.stderr
    );
    assert!(
        !run.stdout.contains(RESTORE_VERDICT)
            && !run.stderr.contains("session restore refused")
            && !run.stdout.contains("session restore refused"),
        "{case}: a usage error never reaches the restore pass: {run:?}"
    );
}

/// The boot flags every case passes, so the config under test is the only thing
/// that differs between rows.
const CONFIG_ARG: [&str; 2] = ["--config", "marmel.toml"];

fn run_with_config(dir: &Path, extra: &[&str], env: &[(&str, &str)]) -> Run {
    let mut args: Vec<&str> = CONFIG_ARG.to_vec();
    args.extend_from_slice(extra);
    run_marmel(dir, &args, env)
}

// ------------------------------------------------------------------- rows

/// Row 1 — unfinished plan, **no flag**, capture on: the new default continues
/// the previous run exactly as `--resume` would, and exits `0`.
#[test]
fn row01_unfinished_plan_without_any_flag_resumes_implicitly() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());
    write_plan(dir.path(), UNFINISHED_PLAN);

    let run = run_with_config(dir.path(), &[], &[]);
    assert_restored(&run, "row 1 (unfinished plan, no flag)");
}

/// Row 2 — unfinished plan **plus `--fresh`**: the documented escape hatch wins
/// over the plan-driven default, so the bundle is never installed.
#[test]
fn row02_fresh_beats_the_unfinished_plan_default() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());
    write_plan(dir.path(), UNFINISHED_PLAN);

    let run = run_with_config(dir.path(), &["--fresh"], &[]);
    assert_not_restored(&run, "row 2 (unfinished plan, --fresh)");
    assert_bundle_never_opened(&run, "row 2 (unfinished plan, --fresh)");
}

/// Row 3 — a **complete** plan and no flag: there is no open work to continue,
/// so the run starts fresh.
#[test]
fn row03_a_complete_plan_does_not_resume() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());
    write_plan(dir.path(), COMPLETE_PLAN);

    let run = run_with_config(dir.path(), &[], &[]);
    assert_not_restored(&run, "row 3 (complete plan, no flag)");
    assert_bundle_never_opened(&run, "row 3 (complete plan, no flag)");
}

/// Row 4 — **no plan file** at all and no flag: nothing on disk justifies a
/// restore, so the run starts fresh.
#[test]
fn row04_a_missing_plan_does_not_resume() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());

    let run = run_with_config(dir.path(), &[], &[]);
    assert_not_restored(&run, "row 4 (no plan, no flag)");
    assert_bundle_never_opened(&run, "row 4 (no plan, no flag)");
}

/// Row 5 — a plan file that exists but holds **no recognised task line**: the
/// grammar can see neither "done" nor "empty", so the state is `Unknown` and the
/// default fails closed. The reason is logged (stderr is `tracing`'s sink in raw
/// mode) and never printed to the user, who asked for nothing.
#[test]
fn row05_a_plan_with_no_recognised_task_line_fails_closed() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());
    write_plan(dir.path(), UNRECOGNISED_PLAN);

    let run = run_with_config(dir.path(), &[], &[]);
    assert_not_restored(&run, "row 5 (unrecognised plan)");
    assert_bundle_never_opened(&run, "row 5 (unrecognised plan)");
    assert!(
        run.stderr.contains("resume-by-default")
            && run
                .stderr
                .contains("starting fresh instead of resuming on a guess"),
        "the fail-closed decision must be logged with its reason, got {:?}",
        run.stderr
    );
}

/// Row 6 — explicit `--resume` with a **complete** plan: the explicit request is
/// not gated on the plan state, so it restores.
#[test]
fn row06_explicit_resume_ignores_a_complete_plan() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());
    write_plan(dir.path(), COMPLETE_PLAN);

    let run = run_with_config(dir.path(), &["--resume"], &[]);
    assert_restored(&run, "row 6 (--resume, complete plan)");
}

/// Row 7 — explicit `--resume` with **no plan**: still an explicit attempt,
/// unchanged. With a bundle it restores; with no bundle it is the loud fresh
/// start an explicit request has always produced, not a silent one.
#[test]
fn row07_explicit_resume_without_a_plan_is_still_an_explicit_attempt() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());

    let run = run_with_config(dir.path(), &["--resume"], &[]);
    assert_restored(&run, "row 7a (--resume, no plan, bundle present)");

    // The other half of "unchanged": no bundle at all is answered loudly.
    let empty = workspace(CAPTURE_ON);
    let run = run_with_config(empty.path(), &["--resume"], &[]);
    assert!(
        !run.timed_out,
        "row 7b: an explicit request with no bundle returns promptly: {run:?}"
    );
    assert_no_startup_panic(&run, "row 7b (--resume, no plan, no bundle)");
    assert_eq!(run.code, Some(0), "row 7b: {run:?}");
    assert!(
        run.stdout.contains(NO_BUNDLE_VERDICT),
        "row 7b: an explicit request is told there is no bundle, got {:?}",
        run.stdout
    );
    assert!(
        !run.stdout.contains(RESTORE_VERDICT),
        "row 7b: and nothing was restored: {:?}",
        run.stdout
    );
}

/// Row 8 — `MARMEL_RESUME=1` with a **complete** plan: the environment outranks
/// the plan state, so it restores exactly as `--resume` would.
#[test]
fn row08_resume_env_outranks_a_complete_plan() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());
    write_plan(dir.path(), COMPLETE_PLAN);

    let run = run_with_config(dir.path(), &[], &[("MARMEL_RESUME", "1")]);
    assert_restored(&run, "row 8 (MARMEL_RESUME=1, complete plan)");
}

/// Row 9 — `MARMEL_RESUME=0` with an **unfinished** plan: a falsy environment
/// value only declines the *explicit* request, so the plan-driven default still
/// applies and the run restores implicitly.
#[test]
fn row09_a_falsy_resume_env_does_not_disable_the_plan_default() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());
    write_plan(dir.path(), UNFINISHED_PLAN);

    let run = run_with_config(dir.path(), &[], &[("MARMEL_RESUME", "0")]);
    assert_restored(&run, "row 9 (MARMEL_RESUME=0, unfinished plan)");
}

/// Row 10 — `--resume` **and** `--fresh`: contradictory, exit `2`, unchanged.
#[test]
fn row10_resume_with_fresh_is_a_usage_error() {
    let dir = workspace(CAPTURE_ON);
    write_bundle_at(dir.path());
    write_plan(dir.path(), UNFINISHED_PLAN);

    let run = run_with_config(dir.path(), &["--resume", "--fresh"], &[]);
    assert_usage_error(
        &run,
        "row 10 (--resume --fresh)",
        "--resume and --fresh are mutually exclusive",
    );
}

/// Row 11 — `--resume` while capture is off asks for a bundle this run could
/// never have written: exit `2`, unchanged.
#[test]
fn row11_resume_with_capture_disabled_is_a_usage_error() {
    let dir = workspace(CAPTURE_OFF);
    write_bundle_at(dir.path());
    write_plan(dir.path(), UNFINISHED_PLAN);

    let run = run_with_config(dir.path(), &["--resume"], &[]);
    assert_usage_error(
        &run,
        "row 11 (--resume, capture off)",
        "session capture is disabled",
    );
}

/// Row 12 — capture off, unfinished plan, **no flag**: the plan-driven default
/// is only allowed when capture is enabled, so the run starts fresh — and, the
/// user never having asked, there is no usage error either.
#[test]
fn row12_capture_disabled_never_resumes_on_an_unfinished_plan() {
    let dir = workspace(CAPTURE_OFF);
    write_bundle_at(dir.path());
    write_plan(dir.path(), UNFINISHED_PLAN);

    let run = run_with_config(dir.path(), &[], &[]);
    assert_not_restored(&run, "row 12 (capture off, unfinished plan, no flag)");
    assert_bundle_never_opened(&run, "row 12 (capture off, unfinished plan, no flag)");
    assert!(
        !run.stderr.contains("session capture is disabled"),
        "nobody asked for a resume, so there is no usage error to report: {:?}",
        run.stderr
    );
}

/// Row 13 — capture off **plus `--fresh`** over an unfinished plan: fresh, exit
/// `0`, and no usage error (`--fresh` alone is never an error).
#[test]
fn row13_capture_disabled_and_fresh_starts_fresh() {
    let dir = workspace(CAPTURE_OFF);
    write_bundle_at(dir.path());
    write_plan(dir.path(), UNFINISHED_PLAN);

    let run = run_with_config(dir.path(), &["--fresh"], &[]);
    assert_not_restored(&run, "row 13 (capture off, --fresh)");
    assert_bundle_never_opened(&run, "row 13 (capture off, --fresh)");
    assert!(
        !run.stderr.contains("session capture is disabled"),
        "--fresh is never a usage error: {:?}",
        run.stderr
    );
}

/// Row 14 — the implicit default over a **refused** bundle: the degradation is
/// logged, the run continues fresh, and it must never end on
/// `EXIT_RESTORE_REFUSED` (3) or print the run-stopping verdict.
#[test]
fn row14_implicit_resume_of_a_refused_bundle_degrades_to_fresh() {
    let dir = workspace(CAPTURE_ON);
    let bundle = write_bundle_at(dir.path());
    tamper_format_version(&bundle);
    write_plan(dir.path(), UNFINISHED_PLAN);

    let run = run_with_config(dir.path(), &[], &[]);
    assert_not_restored(&run, "row 14 (implicit, refused bundle)");
    assert_ne!(
        run.code,
        Some(marmennill::session::EXIT_RESTORE_REFUSED),
        "an implicit resume must never end on exit 3: {run:?}"
    );
    assert!(
        run.stderr.contains("implicit resume refused"),
        "the degradation is logged, got {:?}",
        run.stderr
    );
}

/// Row 15 — the same refused bundle under an explicit `--resume`: the hard stop,
/// unchanged, mapped onto `EXIT_RESTORE_REFUSED` (3) after teardown.
#[test]
fn row15_explicit_resume_of_a_refused_bundle_exits_three() {
    let dir = workspace(CAPTURE_ON);
    let bundle = write_bundle_at(dir.path());
    tamper_format_version(&bundle);
    write_plan(dir.path(), UNFINISHED_PLAN);

    let run = run_with_config(dir.path(), &["--resume"], &[]);
    assert!(!run.timed_out, "row 15: a refusal stops the run: {run:?}");
    assert_no_startup_panic(&run, "row 15 (--resume, refused bundle)");
    assert_eq!(
        run.code,
        Some(marmennill::session::EXIT_RESTORE_REFUSED),
        "the typed refusal must reach the named exit code: {run:?}"
    );
    assert_eq!(marmennill::session::EXIT_RESTORE_REFUSED, 3);
    assert!(
        run.stderr.contains("session restore refused") && run.stderr.contains("was NOT modified"),
        "main's stderr line is the refusal sentence, got {:?}",
        run.stderr
    );
    assert!(
        run.stdout.contains("session restore refused") && run.stdout.contains("exit code 3"),
        "the panes state the verdict and the exit code, got {:?}",
        run.stdout
    );
    assert!(
        !run.stdout.contains(RESTORE_VERDICT),
        "a refused bundle installs nothing: {:?}",
        run.stdout
    );
}
