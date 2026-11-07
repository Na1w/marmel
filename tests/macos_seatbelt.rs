//! Behavioural tests for the macOS Seatbelt backend: they drive the sandbox re-entry child
//! and assert what Seatbelt actually enforces.
//!
//! The Seatbelt backend is invisible to a Linux host — `src/harness/sandbox_seatbelt.rs` is gated
//! by `#[cfg(target_os = "macos")]` and there is no Landlock on a Mac — so the emitted SBPL text
//! is never handed to a real libSystem anywhere but the CI `macos-latest` leg, which runs
//! `cargo test --all-targets --all-features` natively. These tests are what that leg asserts.
//!
//! What they prove, end to end rather than by inspecting the emitted text:
//!
//! 1. the profile the shared emitter renders **compiles on a real macOS host** and confines the
//!    re-entry child, because the child execs the pending command instead of refusing it;
//! 2. a confined child may write inside the workspace root, and may **not** write to a directory
//!    the policy does not grant — with the same write succeeding once the sandbox is opted out, so
//!    the denial is attributable to Seatbelt rather than to the fixture or to the shell;
//! 3. the operator's `sandbox_extra_readonly_paths` grant cannot reach the `.ssh` home it is told
//!    about: the explicit denial wins over a grant of the directory around it, and the grant is
//!    scoped to the path it names;
//! 4. a profile libSystem refuses to compile ends the run in the refusal exit code, with the
//!    operator-facing refusal on stderr and **nothing** exec-ed;
//! 5. the explicit opt-out is honoured before Seatbelt is attempted, so an opted-out run is both
//!    unsandboxed and warned about by name.
//!
//! Fixtures: every fixture lives under Cargo's own scratch dir (never under `$TMPDIR`, which the
//! policy grants read-write on its own, and never under the real `$HOME`) and the confined process
//! is always a **subprocess** — confining the test process itself would confine the test harness
//! for the rest of the run, so the profile is applied inside the re-entry child.

// A test file whose every item is macOS-only must still compile cleanly on Linux, where it holds
// nothing: the crate-wide `cfg` below is what makes that true rather than an empty `mod`.
#![cfg(target_os = "macos")]

use marmennill::harness::pty::{SANDBOX_EXEC_ARG, SANDBOX_OPTOUT_ENV};
use marmennill::harness::sandbox::{SANDBOX_EXTRA_RO_PATHS_ENV, SANDBOX_REFUSE_EXIT_CODE};
use marmennill::harness::sandbox_seatbelt::SEATBELT_TEST_BAD_PROFILE_ENV;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The re-entry binary, as reported to an integration test by Cargo (`CARGO_BIN_EXE_<bin>`), i.e.
/// a path built from Cargo's own knowledge of the package rather than from an executable name.
fn reentry_child() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_marmel"))
}

/// One leg of a Seatbelt run: `<exe> --internal-sandbox-exec <workspace> <command>`, with the
/// knobs that leg needs.
///
/// Every knob variable is cleared first, so a leg that wants no opt-out, no extra read-only path
/// and no test knob gets exactly what its name says even when the test process was started with
/// one of them set. `extras` is the value of the operator channel the spawn side would normally
/// publish; `bad_profile` switches on [`SEATBELT_TEST_BAD_PROFILE_ENV`], the test-only knob that
/// makes the Seatbelt backend hand libSystem deliberately unparsable profile text.
fn run_child(
    workspace: &Path,
    command: &str,
    extras: Option<&str>,
    opt_out: bool,
    bad_profile: bool,
) -> Output {
    let exe = reentry_child();
    let mut cmd = Command::new(&exe);
    cmd.arg(SANDBOX_EXEC_ARG)
        .arg(workspace)
        .arg(command)
        .env_remove(SANDBOX_OPTOUT_ENV)
        .env_remove(SANDBOX_EXTRA_RO_PATHS_ENV)
        .env_remove(SEATBELT_TEST_BAD_PROFILE_ENV);
    if let Some(extra) = extras {
        cmd.env(SANDBOX_EXTRA_RO_PATHS_ENV, extra);
    }
    if opt_out {
        cmd.env(SANDBOX_OPTOUT_ENV, "1");
    }
    if bad_profile {
        cmd.env(SEATBELT_TEST_BAD_PROFILE_ENV, "1");
    }
    cmd.output()
        .expect("the re-entry child must be runnable for every Seatbelt leg")
}

/// The stderr of a leg, as text, for the assertion messages that must be self-explanatory.
fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The stdout of a leg, as text.
fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A directory the confined child has no rule for, or `None` (with a visible `SKIP`) when this
/// host has no such place.
///
/// `tempfile::tempdir()` is deliberately **not** the fixture root here: on macOS it lands under
/// `$TMPDIR`, and the policy grants that root read-write to the confined process on its own, so a
/// denial there would prove nothing about the profile. Cargo's own scratch dir sits inside the
/// package, which is not an allow-list entry on either platform.
fn unlisted_scratch(test: &str) -> Option<PathBuf> {
    let scratch = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .or_else(|| {
            let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
            target.is_dir().then_some(target)
        })?;

    if scratch.starts_with(std::env::temp_dir()) {
        eprintln!(
            "SKIP {test}: {} sits inside the temporary directory, which the policy grants \
             read-write to the confined process on its own",
            scratch.display()
        );
        return None;
    }
    Some(scratch)
}

/// The Seatbelt entry point must accept the profile the shared emitter renders — and confine the
/// child it is applied to — before any of the denial tests below mean anything.
///
/// A profile libSystem will not compile answers `rc != 0`, which the backend maps to a refusal:
/// the command is never exec-ed and the exit code is [`SANDBOX_REFUSE_EXIT_CODE`]. So a run that
/// execs the command and exits 0 proves both halves at once: the emitted SBPL text parsed on a
/// real macOS host, and the child reached the exec (i.e. the enforcement probe agreed that the
/// profile in force is the one that was rendered).
#[test]
fn test_seatbelt_profile_compiles_on_the_host_and_confines_the_child() {
    let name = "test_seatbelt_profile_compiles_on_the_host_and_confines_the_child";
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let out = run_child(workspace.path(), "printf %s confined", None, false, false);
    let stderr = stderr_of(&out);

    assert_eq!(
        out.status.code(),
        Some(0),
        "{name}: the re-entry child must exec the command under a rendered profile, \
         got exit {:?} with stderr: {stderr}",
        out.status.code()
    );
    assert_eq!(
        stdout_of(&out),
        "confined",
        "{name}: the pending command must have been exec-ed"
    );
    assert!(
        !stderr.contains("refusing to run the command"),
        "{name}: a confined child must not be refused, got: {stderr}"
    );
    assert!(
        !stderr.contains(SANDBOX_EXTRA_RO_PATHS_ENV),
        "{name}: no extra read-only path was published, so none may be announced as skipped: {stderr}"
    );
}

/// Inside the workspace root a confined process creates files; outside every granted root the same
/// write is denied and creates nothing; and with the sandbox opted out the same write succeeds.
///
/// That third leg is what attributes the denial to Seatbelt: without it, a fixture the shell
/// simply could not reach would make the denial look like enforcement.
#[test]
fn test_seatbelt_allows_workspace_writes_and_denies_writes_outside() {
    let name = "test_seatbelt_allows_workspace_writes_and_denies_writes_outside";
    let Some(scratch) = unlisted_scratch(name) else {
        return;
    };
    let unlisted =
        tempfile::tempdir_in(scratch).expect("the unlisted fixture dir must be creatable");
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    // 1. The workspace root is the one required policy entry: writing there must work.
    let inside = workspace.path().join("inside-marker");
    let inside_run = run_child(
        workspace.path(),
        &format!("touch '{}'", inside.display()),
        None,
        false,
        false,
    );
    assert_eq!(
        inside_run.status.code(),
        Some(0),
        "{name}: a write inside the workspace root must succeed, stderr: {}",
        stderr_of(&inside_run)
    );
    assert!(
        inside.is_file(),
        "{name}: the confined child must have created the workspace marker"
    );

    // 2. The same write in a directory no policy entry covers: denied, and nothing created.
    let outside = unlisted.path().join("child-wrote-this");
    let outside_run = run_child(
        workspace.path(),
        &format!("touch '{}'", outside.display()),
        None,
        false,
        false,
    );
    assert_ne!(
        outside_run.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: the sandbox must have been established for this leg to say anything about the \
         rules, stderr: {}",
        stderr_of(&outside_run)
    );
    assert!(
        !outside_run.status.success(),
        "{name}: a write outside the granted roots must be denied, stderr: {}",
        stderr_of(&outside_run)
    );
    assert!(
        !outside.exists(),
        "{name}: the denied write must not have created a file"
    );

    // 3. Opted out, the very same write succeeds — so leg 2 denied it because Seatbelt was
    //    enforcing, not because the fixture or the shell could not reach it.
    let opted_run = run_child(
        workspace.path(),
        &format!("touch '{}'", outside.display()),
        None,
        true,
        false,
    );
    assert!(
        opted_run.status.success(),
        "{name}: an opted-out run must be able to write where an enforced one cannot, stderr: {}",
        stderr_of(&opted_run)
    );
    assert!(
        outside.exists(),
        "{name}: the opted-out run must have created the fixture marker"
    );
}

/// A read of the sensitive `.ssh` home stays denied **even when an operator grant covers the
/// directory around it**, and the grant is scoped to the path it names.
///
/// Leg by leg: the fake home is published through `sandbox_extra_readonly_paths`, so the rendered
/// profile carries `(allow file-read* (subpath <fake home>)) alongside the explicit
/// `(deny file-read* (subpath <fake home>/.ssh))`. Reading a plain file in that granted home must
/// then succeed (the grant is really there), while reading the `.ssh/config` sibling is denied by
/// the explicit clause — and a file that sits next to the granted home, not under it, is denied as
/// always. Opting out reads them all, which is what proves the denials came from Seatbelt.
#[test]
fn test_seatbelt_denies_the_ssh_home_even_under_an_operator_grant() {
    let name = "test_seatbelt_denies_the_ssh_home_even_under_an_operator_grant";
    let Some(scratch) = unlisted_scratch(name) else {
        return;
    };
    let fixture = tempfile::tempdir_in(scratch).expect("the ssh fixture dir must be creatable");
    let fake_home = fixture.path().join("fake-home");
    let ssh_home = fake_home.join(".ssh");
    std::fs::create_dir_all(&ssh_home).expect("the fake ssh home must be creatable");

    let ssh_config = ssh_home.join("config");
    let ssh_secret = "SSH-CONFIG-SECRET";
    std::fs::write(&ssh_config, format!("IdentityFile {ssh_secret}\n"))
        .expect("the fake ssh config must be writable");
    let granted_file = fake_home.join("readable");
    std::fs::write(&granted_file, "GRANTED-HOME-CONTENT")
        .expect("the granted home must be writable");
    let ungranted = fixture.path().join("payload");
    std::fs::write(&ungranted, "UNGRANTED-PAYLOAD")
        .expect("the ungranted payload must be writable");

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let extras = fake_home.to_string_lossy().into_owned();

    // 1. The grant itself is live: a plain file in the published home is readable.
    let granted_read = run_child(
        workspace.path(),
        &format!("cat '{}'", granted_file.display()),
        Some(extras.as_str()),
        false,
        false,
    );
    assert_eq!(
        granted_read.status.code(),
        Some(0),
        "{name}: the published home must be readable under its own grant, stderr: {}",
        stderr_of(&granted_read)
    );
    assert!(
        stdout_of(&granted_read).contains("GRANTED-HOME-CONTENT"),
        "{name}: the operator grant must reach the home it names"
    );

    // 2. The `.ssh` home inside that same granted directory must still be denied, and its content
    //    must never appear: the explicit denial outranks the grant, it is not undone by it.
    let ssh_read = run_child(
        workspace.path(),
        &format!("cat '{}'", ssh_config.display()),
        Some(extras.as_str()),
        false,
        false,
    );
    assert_ne!(
        ssh_read.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: the sandbox must have been established for this leg to mean anything, stderr: {}",
        stderr_of(&ssh_read)
    );
    let ssh_stdout = stdout_of(&ssh_read);
    assert!(
        !ssh_read.status.success(),
        "{name}: reading the ssh home of a granted directory must be denied, stderr: {}",
        stderr_of(&ssh_read)
    );
    assert!(
        !ssh_stdout.contains(ssh_secret) && !ssh_stdout.contains("IdentityFile"),
        "{name}: the ssh config content must not reach the confined command, got: {ssh_stdout}"
    );

    // 3. A file next to the granted home rather than under it stays unreadable: the grant covers
    //    the path it names and nothing around it.
    let ungranted_read = run_child(
        workspace.path(),
        &format!("cat '{}'", ungranted.display()),
        Some(extras.as_str()),
        false,
        false,
    );
    assert!(
        !ungranted_read.status.success()
            && !stdout_of(&ungranted_read).contains("UNGRANTED-PAYLOAD"),
        "{name}: an ungranted path beside a granted one must stay unreadable, stderr: {}",
        stderr_of(&ungranted_read)
    );

    // 4. Opted out, all three reads succeed: the denials above are Seatbelt's doing.
    let both = format!("cat '{}' '{}'", ssh_config.display(), ungranted.display());
    let opted = run_child(workspace.path(), &both, None, true, false);
    let opted_stdout = stdout_of(&opted);
    assert!(
        opted.status.success()
            && opted_stdout.contains(ssh_secret)
            && opted_stdout.contains("UNGRANTED-PAYLOAD"),
        "{name}: the fixture must be readable when nothing is enforced, stderr: {}",
        stderr_of(&opted)
    );
}

/// A Seatbelt profile that will not compile must fail the run closed: exit 73, the operator-facing
/// refusal on stderr, and nothing exec-ed.
///
/// `sandbox.rs` pins the pure half of this spine (`reentry_action` maps a failed enforcement to
/// exit 73, and the exit code itself is asserted to be 73). What is pinned here is the macOS half:
/// that `sandbox_init_with_parameters` answering non-zero for the rendered text really does become
/// that refusal — i.e. the backend's `EnforcementFailed` and the gate's exit code are wired to the
/// same failure, on the platform where the failure can actually happen.
#[test]
fn test_seatbelt_unparsable_profile_refuses_with_exit_73_and_execs_nothing() {
    let name = "test_seatbelt_unparsable_profile_refuses_with_exit_73_and_execs_nothing";
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let out = run_child(workspace.path(), "echo MUST-NOT-RUN", None, false, true);
    let stderr = stderr_of(&out);

    assert!(
        !stdout_of(&out).contains("MUST-NOT-RUN"),
        "{name}: a refused run must exec nothing, got stdout: {}",
        stdout_of(&out)
    );
    assert_eq!(
        out.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: a profile libSystem refuses must exit with the refusal code, stderr: {stderr}"
    );
    assert!(
        stderr.contains("refusing to run the command") && stderr.contains(SANDBOX_OPTOUT_ENV),
        "{name}: the refusal must be the operator-facing one and name the only way out, got: {stderr}"
    );
    assert!(
        stderr.contains("sandbox_init_with_parameters"),
        "{name}: the refusal must name the Seatbelt entry point that rejected the profile, got: {stderr}"
    );
}

/// The macOS twin of the Linux opt-out test: the explicit opt-out must be honoured **before**
/// Seatbelt is attempted, so the command runs unsandboxed and the opt-out is named out loud.
#[test]
fn test_seatbelt_honours_an_explicit_opt_out_before_any_attempt() {
    let name = "test_seatbelt_honours_an_explicit_opt_out_before_any_attempt";
    let Some(scratch) = unlisted_scratch(name) else {
        return;
    };
    let fixture = tempfile::tempdir_in(scratch).expect("the opt-out fixture dir must be creatable");
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let marker = fixture.path().join("opt-out-marker");

    let out = run_child(
        workspace.path(),
        &format!(
            "printf %s opted-out > '{}' && printf %s opted-out",
            marker.display()
        ),
        None,
        true,
        false,
    );
    let stderr = stderr_of(&out);

    assert!(
        out.status.success(),
        "{name}: an explicit opt-out must still run the command, stderr: {stderr}"
    );
    assert_eq!(
        stdout_of(&out).trim(),
        "opted-out",
        "{name}: the pending command must have been exec-ed"
    );
    assert!(
        marker.exists(),
        "{name}: an opted-out run must be able to write outside the granted roots, so Seatbelt \
         was not attempted"
    );
    assert!(
        stderr.contains("DISABLED") && stderr.contains("Seatbelt"),
        "{name}: a skipped backend must be warned about by name, got: {stderr}"
    );
    assert!(
        !stderr.contains("refusing to run the command"),
        "{name}: an honoured opt-out is never a refusal, got: {stderr}"
    );
}
