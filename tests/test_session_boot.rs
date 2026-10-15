//! t-022 — the boot path of `--resume`, end to end through the real binary.
//!
//! The unit-level pass in `src/ui/session_tests.rs` pins what the restore itself
//! installs. This file pins the two things only `main` can get wrong:
//!
//! * the **usage errors** (`src/main.rs:296`, `session_flag_error`): a
//!   contradictory `--resume --fresh`, and `--resume` while capture is off — both
//!   `eprintln!` + exit **2**, before any runtime, config publish or bundle read;
//! * the **exit-code mapping** (`src/main.rs:92-103`): the typed
//!   `session::restore::RestoreRefused` the boot pass returns is mapped, after
//!   teardown, onto `session::EXIT_RESTORE_REFUSED` (3) — and the trigger for that
//!   pass may come from the flag or from `MARMEL_RESUME`, while `--fresh` stays
//!   the escape hatch that never opens the bundle at all.
//!
//! Every case runs the real `marmel` binary with its working directory set to a
//! private temp directory, so the plan directory — and therefore the bundle it
//! looks for, `session::bundle_dir(&Plan::at(dir))` — is inside that temp dir.
//! Nothing here ever touches the workspace's own `.marmel/`, and every worker key
//! is prefixed `t022-e2e-`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marmennill::agents::Agent;
use marmennill::manager::phase::Plan;
use marmennill::session::commit::{CapturePayload, CommitLock, CommitPolicy, write_bundle};
use marmennill::session::format::TimePoint;
use marmennill::session::{
    DanglingToolCallState, HistorySource, IdentityFloors, ManagerSection, MessageRecord,
    PinWitness, ProcessEnding, ToolCallRecord, ToolFunctionRecord, UiJournalRecord, UiSection,
    WorkerSection, WorkerStatus,
};

/// Where the process under test will look for its bundle. `main` boots its
/// manager with `Plan::default()`, whose directory is `MARMEL_DIR` **relative to
/// the process working directory** — so with `current_dir(dir)` the bundle path is
/// `<dir>/.marmel/session`, exactly as the run's own log line spells it.
fn bundle_dir_in(dir: &Path) -> PathBuf {
    marmennill::session::bundle_dir(&Plan::at(dir.join(marmennill::manager::phase::MARMEL_DIR)))
}

// --------------------------------------------------------------------- fixtures

fn transcript() -> Vec<MessageRecord> {
    vec![
        MessageRecord::System {
            content: "system prompt for the end-to-end boot test".to_string(),
        },
        MessageRecord::User {
            content: "the end-to-end goal".to_string(),
        },
        MessageRecord::Assistant {
            content: None,
            reasoning_content: Some("thinking".to_string()),
            tool_calls: vec![ToolCallRecord {
                id: "call-e2e-1".to_string(),
                kind: "function".to_string(),
                function: ToolFunctionRecord {
                    name: marmennill::tool_names::TOOL_READ_FILE.to_string(),
                    arguments: "{\"path\":\"src/main.rs\"}".to_string(),
                },
            }],
        },
        MessageRecord::Tool {
            tool_call_id: "call-e2e-1".to_string(),
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

/// A sound bundle: the loader accepts it, so a refusal asserted elsewhere is
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
        workers: vec![worker("t022-e2e-worker")],
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
        "session-t022-e2e",
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

/// Raise the stored `format_version` past what this binary understands: the L3
/// gate refuses **before** any member is parsed, and because the manifest carries
/// no self-hash, every member keeps its own `byte_len`/`fingerprint`.
fn tamper_format_version(bundle: &Path) {
    let path = bundle.join(marmennill::session::MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).expect("manifest");
    let mut manifest: serde_json::Value = serde_json::from_str(&text).expect("manifest json");
    manifest["format_version"] = serde_json::json!(marmennill::session::FORMAT_VERSION + 5);
    std::fs::write(&path, serde_json::to_vec(&manifest).expect("reserialize")).expect("write");
}

/// `(relative path, bytes)` under `root`, sorted: the witness that a run neither
/// wrote nor repaired anything in the bundle.
fn tree_bytes(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                out.push((rel, bytes));
            }
        }
    }
    out.sort();
    out
}

// ------------------------------------------------------------------ the runner

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
        // test runner's `MARMEL_*` variables or its terminal.
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        // Nothing may reach a real model, and nothing may hang on connect.
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

/// A private workspace with a config file, so `config::load` never falls back to
/// the caller's tree.
fn workspace(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("marmel.toml"), config).expect("write config");
    dir
}

const PLAIN_CONFIG: &str = "session_enabled = true\nsession_resume = false\n";
const CAPTURE_OFF_CONFIG: &str = "session_enabled = false\nsession_resume = true\n";

// ----------------------------------------------------------------------- tests

/// `--resume --fresh` is a usage error, resolved in `main` before anything is
/// started: exit **2**, one sentence on stderr, and — because the bundle was
/// never opened — no refusal verdict anywhere in the output.
#[test]
fn resume_plus_fresh_is_a_usage_error_that_exits_two_before_any_bundle_is_read() {
    let dir = workspace(PLAIN_CONFIG);
    let bundle = write_bundle_at(dir.path());
    tamper_format_version(&bundle);
    let bundle_before = tree_bytes(&bundle);

    let run = run_marmel(
        dir.path(),
        &["--config", "marmel.toml", "--resume", "--fresh"],
        &[],
    );
    assert!(!run.timed_out, "a usage error returns immediately: {run:?}");
    assert_eq!(
        run.code,
        Some(2),
        "usage error exits 2, not 1 or 3: {run:?}"
    );
    assert!(
        run.stderr
            .contains("--resume and --fresh are mutually exclusive"),
        "the sentence names the contradiction, got {:?}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("session restore refused")
            && !run.stdout.contains("session restore refused"),
        "a usage error never reaches the restore pass: {:?}",
        run.stderr
    );
    assert!(
        !run.stdout.contains("Session restored"),
        "and never restores: {:?}",
        run.stdout
    );
    assert_eq!(
        tree_bytes(&bundle),
        bundle_before,
        "the usage error is resolved before the bundle is touched"
    );
}

/// `--resume` while capture is off asks for a bundle this run could never have
/// written — the other documented usage error, same exit **2**.
#[test]
fn resume_with_capture_disabled_is_a_usage_error_that_exits_two() {
    let dir = workspace(CAPTURE_OFF_CONFIG);
    let bundle = write_bundle_at(dir.path());
    let bundle_before = tree_bytes(&bundle);

    let run = run_marmel(dir.path(), &["--config", "marmel.toml", "--resume"], &[]);
    assert!(!run.timed_out, "a usage error returns immediately: {run:?}");
    assert_eq!(run.code, Some(2), "{run:?}");
    assert!(
        run.stderr.contains("session capture is disabled"),
        "the sentence says why, got {:?}",
        run.stderr
    );
    assert!(
        !run.stdout.contains("Session restored") && !run.stderr.contains("session restore refused"),
        "capture-off never opens the bundle it would not have written: {:?}",
        run.stderr
    );
    assert_eq!(tree_bytes(&bundle), bundle_before);
}

/// The exit-3 mapping in `src/main.rs:92-103`: the boot pass prints the refusal
/// (code, member, both versions, "was NOT modified") to panes, journal and log,
/// returns the typed `RestoreRefused`, and `main` maps it — after teardown — onto
/// `session::EXIT_RESTORE_REFUSED`.
#[test]
fn a_refused_resume_exits_with_the_named_refusal_code_and_says_so_three_times() {
    let dir = workspace(PLAIN_CONFIG);
    let bundle = write_bundle_at(dir.path());
    tamper_format_version(&bundle);
    let bundle_before = tree_bytes(&bundle);

    let run = run_marmel(dir.path(), &["--config", "marmel.toml", "--resume"], &[]);
    assert!(!run.timed_out, "a refusal stops the run: {run:?}");
    assert_eq!(
        run.code,
        Some(marmennill::session::EXIT_RESTORE_REFUSED),
        "the typed refusal must reach the named exit code, and that constant is 3: {run:?}"
    );
    assert_eq!(marmennill::session::EXIT_RESTORE_REFUSED, 3);

    // stderr: what `main` repeats after the screen is gone.
    assert!(
        run.stderr.contains("session restore refused")
            && run.stderr.contains("was NOT modified")
            && run.stderr.contains(marmennill::session::MANIFEST_FILE),
        "main's stderr line is the refusal sentence, got {:?}",
        run.stderr
    );
    assert!(
        run.stderr.contains(&format!(
            "format_version {}",
            marmennill::session::FORMAT_VERSION + 5
        )) && run
            .stderr
            .contains(&format!("({})", marmennill::session::FORMAT_VERSION)),
        "both version numbers are quoted, got {:?}",
        run.stderr
    );
    // stdout/panes: the loud verdict that nothing was installed and no fresh
    // session was started.
    assert!(
        run.stdout.contains("session restore refused") && run.stdout.contains("exit code 3"),
        "the panes state the verdict and the exit code, got {:?}",
        run.stdout
    );
    // The bundle is left exactly as it was found: a refusal never repairs in place.
    assert_eq!(
        tree_bytes(&bundle),
        bundle_before,
        "the refused bundle must not be touched by a refused run"
    );
}

/// The trigger is the flag **or** the environment: with no `--resume` on the
/// command line, `MARMEL_RESUME=1` reaches the same pass, and a refused bundle
/// therefore ends the run with the same exit code.
#[test]
fn marmel_resume_env_alone_reaches_the_same_refusal_and_the_same_exit_code() {
    let dir = workspace(PLAIN_CONFIG);
    let bundle = write_bundle_at(dir.path());
    tamper_format_version(&bundle);
    let bundle_before = tree_bytes(&bundle);

    let run = run_marmel(
        dir.path(),
        &["--config", "marmel.toml"],
        &[("MARMEL_RESUME", "1")],
    );
    assert!(!run.timed_out, "the env trigger stops the run too: {run:?}");
    assert_eq!(
        run.code,
        Some(marmennill::session::EXIT_RESTORE_REFUSED),
        "{run:?}"
    );
    assert!(
        run.stderr.contains("session restore refused")
            && run.stdout.contains("session restore refused"),
        "the env spelling is as loud as the flag: {:?}",
        run.stderr
    );
    assert_eq!(tree_bytes(&bundle), bundle_before);
}

/// `--fresh` is the escape hatch, and it outranks the environment: the same
/// refused bundle is never opened, the run starts normally and exits 0, and no
/// refusal verdict appears anywhere.
#[test]
fn fresh_beats_the_resume_env_and_the_refused_bundle_is_never_opened() {
    let dir = workspace("session_enabled = true\nsession_resume = true\n");
    let bundle = write_bundle_at(dir.path());
    tamper_format_version(&bundle);
    let bundle_before = tree_bytes(&bundle);

    let run = run_marmel(
        dir.path(),
        &["--config", "marmel.toml", "--fresh"],
        &[("MARMEL_RESUME", "1")],
    );
    assert!(!run.timed_out, "a fresh start is not a refusal: {run:?}");
    assert_eq!(
        run.code,
        Some(0),
        "--fresh exits 0 even with a bundle the resume path would refuse and \
         session_resume = true in the file: {run:?}"
    );
    assert!(
        !run.stderr.contains("session restore refused")
            && !run.stdout.contains("session restore refused"),
        "--fresh never opens the bundle: {:?}",
        run.stderr
    );
    assert!(
        !run.stdout.contains("Session restored"),
        "--fresh installs nothing: {:?}",
        run.stdout
    );
    assert_eq!(tree_bytes(&bundle), bundle_before);
}

/// The positive end-to-end: a sound bundle plus `--resume` really does rehydrate
/// through the CLI, in the process's own plan directory, and the boot report is
/// spoken to the panes. The turn that follows is pointed at a dead port on
/// purpose, so this test asserts only what the boot path owns: the restore
/// verdict, and that the run was never classified as a refusal or a usage error.
#[test]
fn a_sound_resume_rehydrates_through_the_cli_and_announces_the_restore() {
    let dir = workspace(PLAIN_CONFIG);
    write_bundle_at(dir.path());

    let run = run_marmel(dir.path(), &["--config", "marmel.toml", "--resume"], &[]);
    assert!(!run.timed_out, "the boot path returns promptly: {run:?}");
    assert!(
        run.code != Some(marmennill::session::EXIT_RESTORE_REFUSED) && run.code != Some(2),
        "a sound bundle is neither refused nor a usage error: {run:?}"
    );
    assert!(
        run.stdout
            .contains("Session restored (session id session-t022-e2e"),
        "the restore verdict reaches the panes, got {:?}",
        run.stdout
    );
    assert!(
        run.stdout.contains("1 worker(s)") || run.stdout.contains("capability(s) not restorable"),
        "the report counts what it installed, got {:?}",
        run.stdout
    );
    assert!(
        !run.stderr.contains("session restore refused"),
        "and nothing was refused: {:?}",
        run.stderr
    );
}
