//! Behavioural tests for the Windows AppContainer backend: they drive the sandbox re-entry child
//! and assert what the AppContainer + DACL policy actually enforces.
//!
//! The backend is invisible to a Linux host — `src/harness/sandbox_windows.rs` is gated by
//! `#[cfg(target_os = "windows")]` and there is no Landlock or Seatbelt on Windows — so neither the
//! profile nor the ACEs the policy applies are handed to a real Win32 anywhere but the CI
//! `Test (windows-latest)` leg, which runs `cargo test --all-targets --all-features` natively on
//! `x86_64-pc-windows-msvc`. The `cross-check` leg only *compiles* these tests for
//! `x86_64-pc-windows-gnu`. These tests are what that native leg asserts.
//!
//! What they prove, end to end rather than by inspecting the ACEs:
//!
//! 1. a confined child may write inside the workspace root, which is the one **required** policy
//!    entry — proved with the workspace root living outside every built-in grant, i.e. outside the
//!    temporary directories the policy also grants read-write;
//! 2. a confined child may write into the temporary directories, which the Windows policy grants
//!    read-write because a toolchain puts its output there;
//! 3. a write outside every granted root (a fixture under `%USERPROFILE%`, the one directory the
//!    policy deliberately never grants) is denied, creates nothing, and is named as a denial on the
//!    command's own output;
//! 4. `%USERPROFILE%\.ssh` — one of the sensitive directories the policy denies — stays
//!    unreachable, and an operator grant of the directory around it cannot reach it, because the
//!    user-profile root is never granted;
//! 5. an operator-supplied extra read-only path becomes a live grant, through the **Windows**
//!    encoding of the extras channel (line-feed separated, see
//!    `SANDBOX_EXTRA_RO_PATHS_SEP_WINDOWS` in `src/harness/sandbox.rs`: a Windows path carries `:`
//!    in every drive prefix, so the unix separator would split `C:\Tools` into `C` and `\Tools`);
//! 6. a run whose AppContainer cannot be established refuses: exit
//!    [`SANDBOX_REFUSE_EXIT_CODE`], the operator-facing refusal, and **nothing** run;
//! 7. the confined child can still print on stdout and stderr — the inherited-handle capability
//!    grant is what makes the anonymous pipes of a one-shot run usable from inside an AppContainer;
//! 8. a bare interactive-shell payload stays a bare shell through the re-entry, instead of being
//!    turned into `<comspec> /C cmd`;
//! 9. the whole re-entry chain runs **as production runs it**: the built `marmel` bin target, a
//!    `tempfile` fixture workspace, exit 0, the payload's own text on stdout, and the nested
//!    in-workspace write the confined shell has to be able to perform — the leg that turns a host
//!    where **every** command costs [`SANDBOX_REFUSE_EXIT_CODE`] into a failing test;
//! 10. the failure contract of that same chain: a workspace whose policy cannot be built exits
//!     [`SANDBOX_REFUSE_EXIT_CODE`], runs nothing, and carries [`SANDBOX_REFUSAL_MARKER`], the step
//!     it happened at and the class it is diagnosed by;
//! 11. the runner legs: real `cmd.exe` payloads driven straight through the backend's own entry
//!     points (`run_in_appcontainer`/`spawn_in_appcontainer`) — exit-code propagation, confined
//!     stdout and stderr through the inherited standard streams, writes inside the workspace and into
//!     `%TEMP%`, denials outside them, a read of a key in the sensitive directory, a toolchain lookup
//!     through the read-only grant, and the `internetClient` capability the launch declares;
//! 12. the regression guard for the universal-73 shape: on a healthy fixture the attempt has to come
//!     back **applied** (`sandbox_outcome` accepts it) and the command launched inside it has to run,
//!     so the one failure mode that costs every command on a desktop is reported by this test, with
//!     the refusal reason in its message;
//! 13. the pre-existing-content legs (t-034): a confined command reads a file that already existed in
//!     the workspace **before** the sandbox was set up (at the root and inside a pre-existing
//!     subdirectory) and writes a new file into a pre-existing subdirectory, and the workspace root
//!     is verified to carry the grant for this run's own AppContainer SID — the shape the
//!     handle-based root write of the backend depends on, because a descendant that existed before
//!     the grant is covered by inheritance and propagation alone.
//!
//! Fixtures: the fixture a denial is proved **in** never sits under a temporary directory, because
//! the Windows policy grants those read-write on its own and a denial there would prove nothing —
//! the helper that picks such a fixture prints `SKIP <test>: …` when it cannot. The confined
//! process is always a **subprocess**: confining the test process itself would confine the harness
//! for the rest of the run, so confinement is applied inside the re-entry child. Nothing is ever
//! written into the real user profile: `%USERPROFILE%` is probed by reading it only.
//!
//! The runner legs (11) drive the backend library directly, the way the re-entry child of a
//! production run does. The one leg of that group that has to **read** what the confined child wrote
//! on its standard streams re-runs this very test binary with `--exact` on its own name and with
//! `MARMEL_WINDOWS_RUNNER_CHILD` set: the process that launches a confined child is the process
//! whose streams that child writes on, and a leg can only read a pipe it owns itself.
//!
//! One Windows-specific point of contract matters below: an explicit opt-out read by the
//! **re-entry child** runs the pending command **unconfined** — the shell argv of the spawn side,
//! as a plain child of that process — because running it inside an AppContainer anyway would not
//! be an opt-out. That is what makes the opt-out leg below the direct counterpart of the confined
//! denial leg rather than a documentation of it. The attribution leg is the same payload run by
//! `cmd.exe` straight from the test process, i.e. the one leg this file runs without the re-entry
//! child at all. Both kinds are asserted, and each says which one it is.

// A test file whose every item is Windows-only must still compile cleanly on Linux, where it holds
// nothing: the crate-wide `cfg` below is what makes that true rather than an empty `mod`.
#![cfg(target_os = "windows")]

use marmennill::harness::pty::{SANDBOX_EXEC_ARG, SANDBOX_OPTOUT_ENV};
use marmennill::harness::sandbox::{
    Grant, SANDBOX_EXTRA_RO_PATHS_ENV, SANDBOX_REFUSAL_MARKER, SANDBOX_REFUSE_EXIT_CODE,
    SandboxAttempt, sandbox_outcome,
};
use marmennill::harness::sandbox_windows::{
    AclCoverage, CapabilitySids, acl_coverage, attempt_windows_sandbox, prepare_windows_policy,
    run_in_appcontainer, spawn_in_appcontainer,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The marker a confined leg writes, so the file system check after a leg can tell a real write
/// from an empty file.
const WRITTEN_MARK: &str = "written-by-the-confined-child";
/// The marker of a run that must never happen: it must not appear on stdout of a refused run.
const REFUSE_MARK: &str = "MUST-NOT-RUN";
/// The marker of the stdout/stderr regression legs.
const STDOUT_MARK: &str = "CONFINED-STDOUT-OK";
/// As [`STDOUT_MARK`], on the stream a confined one-shot run has to be granted separately.
const STDERR_MARK: &str = "CONFINED-STDERR-OK";
/// The content of the file an operator grant is supposed to make readable.
const GRANTED_MARK: &str = "EXTRA-READ-ONLY-CONTENT";
/// The separator of the Windows extras encoding, spelled out here because the constant in
/// `src/harness/sandbox.rs` (`SANDBOX_EXTRA_RO_PATHS_SEP_WINDOWS`) is private to this crate: a
/// line feed cannot occur in a Win32 name, while `:` is part of every absolute Windows path.
const WINDOWS_EXTRA_RO_PATHS_SEP: &str = "\n";

/// The re-entry binary, as reported to an integration test by Cargo (`CARGO_BIN_EXE_<bin>`), i.e.
/// a path built from Cargo's own knowledge of the package rather than from an executable name.
fn reentry_child() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_marmel"))
}

/// One leg of a Windows sandbox run: `<exe> --internal-sandbox-exec <workspace> <command>`, with
/// the knobs that leg needs.
///
/// Every knob variable is cleared first, so a leg that wants no opt-out and no extra read-only
/// path gets exactly what its name says even when the test process was started with one of them
/// set. `extras` is the value of the operator channel the spawn side would normally publish, in
/// the Windows encoding ([`WINDOWS_EXTRA_RO_PATHS_SEP`]); `opt_out` switches on
/// [`SANDBOX_OPTOUT_ENV`].
///
/// Unlike the macOS file there is no injected-failure knob here: the Windows backend has no
/// test-only env var that makes a stage fail, so the refusal legs below drive the failure through
/// the **policy** instead — a workspace root the policy refuses to grant.
fn run_child(workspace: &Path, command: &str, extras: Option<&str>, opt_out: bool) -> Output {
    let exe = reentry_child();
    let mut cmd = Command::new(&exe);
    cmd.arg(SANDBOX_EXEC_ARG)
        .arg(workspace)
        .arg(command)
        .env_remove(SANDBOX_OPTOUT_ENV)
        .env_remove(SANDBOX_EXTRA_RO_PATHS_ENV);
    if let Some(extra) = extras {
        cmd.env(SANDBOX_EXTRA_RO_PATHS_ENV, extra);
    }
    if opt_out {
        cmd.env(SANDBOX_OPTOUT_ENV, "1");
    }
    cmd.output()
        .expect("the re-entry child must be runnable for every sandbox leg")
}

/// The attribution leg: the very same payload run by `cmd.exe` straight from the test process, i.e.
/// with no AppContainer identity anywhere in the picture. This is what proves a denial below is the
/// sandbox's doing rather than a fixture the shell could not reach.
fn run_unconfined(workspace: &Path, command: &str) -> Output {
    let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    let mut cmd = Command::new(comspec);
    cmd.arg("/C")
        .arg(command)
        .current_dir(workspace)
        .env_remove(SANDBOX_OPTOUT_ENV)
        .env_remove(SANDBOX_EXTRA_RO_PATHS_ENV);
    cmd.output()
        .expect("cmd.exe must be runnable for the unconfined leg")
}

/// The stderr of a leg, as text, for the assertion messages that must be self-explanatory.
fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The stdout of a leg, as text.
fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Whether a leg said out loud that it was refused access: `cmd.exe` spells a failed redirection or
/// a failed read `Access is denied.`, and spells a *traversal* refusal on an ancestor of the path it
/// was asked for as `The system cannot find the path specified.` Both are the sandbox's answer, and
/// the legs that use this helper also pin the exit code, the file system state and an unconfined run
/// in the same fixture, so this text check is corroboration rather than the only evidence.
fn names_denial(out: &Output) -> bool {
    names_denial_in(&format!("{} {}", stdout_of(out), stderr_of(out)))
}

/// The same vocabulary as [`names_denial`], applied to a body of text rather than to the streams of
/// a leg: the runner legs read a confined child's own text out of a file the payload was told to
/// capture it into, because the streams of the process that launches the confined child belong to
/// that process, and cannot be read back by the leg.
fn names_denial_in(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("access is denied")
        || lower.contains("denied")
        || lower.contains("cannot create")
        || lower.contains("cannot find the path")
        || lower.contains("file not found")
}

/// One path, spelled the way `cmd.exe` needs it inside a payload: quoted, because every fixture and
/// every temporary directory on a host could contain a space.
fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// The payload that writes [`WRITTEN_MARK`] into `path`: an `echo` of the marker into a redirection
/// to the quoted path, i.e. the form `cmd.exe` re-parses with its own metacharacters, which is what
/// the confined launch hands over verbatim.
fn write_payload(path: &Path) -> String {
    format!("echo {WRITTEN_MARK} > {}", quoted(path))
}

/// The payload that reads `path`s: `type "<path>"` per entry, so one leg can show that an operator
/// grant reaches several paths at once.
fn read_payload(paths: &[&Path]) -> String {
    let mut payload = String::from("type");
    for path in paths {
        payload.push_str(&format!(" {}", quoted(path)));
    }
    payload
}

/// The payload that lists a directory: `dir /b "<dir>"`, which needs read **and** traverse
/// permission on the directory it names, and is how the sensitive-directory legs are probed.
fn list_payload(dir: &Path) -> String {
    format!("dir /b {}", quoted(dir))
}

/// The operator channel, encoded the way the Windows half of this crate encodes it: the entries in
/// order, joined by a line feed.
fn windows_extras(paths: &[&Path]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<String>>()
        .join(WINDOWS_EXTRA_RO_PATHS_SEP)
}

/// A directory the confined child has no grant for, or `None` (with a visible `SKIP`) when this
/// host has no such place.
///
/// `tempfile::tempdir()` is deliberately **not** the fixture root here: on Windows it lands under
/// `%TEMP%`, and the policy grants the temporary directories read-write to the confined process on
/// its own, so a denial there would prove nothing about the ACEs. Cargo's own scratch dir sits
/// inside the package, which is not an allow-list entry.
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

/// The temporary directories the Windows policy grants read-write, read the same way the policy
/// reads them: `%TEMP%`, `%TMP%` and `%TMPDIR%` when set, and the Win32 temp path as a fallback.
///
/// A fixture is only a denial fixture when it is **not** under one of these: the policy grants each
/// existing entry read-write, so a write denial inside a temporary directory says nothing about the
/// ACEs.
fn granted_temp_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["TEMP", "TMP", "TMPDIR"]
        .iter()
        .filter_map(|name| {
            std::env::var_os(*name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .collect();
    let fallback = std::env::temp_dir();
    if !roots.contains(&fallback) {
        roots.push(fallback);
    }
    roots
}

/// `%USERPROFILE%`, the one directory the Windows policy refuses to grant and therefore the honest
/// place to look for a denial, or `None` (with a visible `SKIP`) when this host does not name it.
fn user_profile(test: &str) -> Option<PathBuf> {
    let profile = std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;
    if !profile.is_dir() {
        eprintln!(
            "SKIP {test}: USERPROFILE ({}) does not name an existing directory",
            profile.display()
        );
        return None;
    }
    Some(profile)
}

/// Whether `path` sits under one of the temporary directories the policy grants read-write.
fn under_granted_temp(path: &Path) -> bool {
    granted_temp_roots()
        .iter()
        .any(|root| path.starts_with(root))
}

/// The workspace root of every leg below lives outside the temporary directories, so that a write
/// denial inside them is not mistaken for the workspace-root grant at work (and a write inside the
/// workspace is not mistaken for the temporary-directory grant). `None` comes with the visible
/// `SKIP` of [`unlisted_scratch`].
fn workspace_outside_temp(test: &str) -> Option<tempfile::TempDir> {
    let scratch = unlisted_scratch(test)?;
    match tempfile::tempdir_in(scratch) {
        Ok(workspace) => Some(workspace),
        Err(err) => {
            eprintln!("SKIP {test}: the workspace fixture could not be created: {err}");
            None
        }
    }
}

/// The workspace root is the one entry the policy marks **required**, so a confined child must be
/// able to write there — proved with the workspace root living outside every built-in grant, which
/// is what makes this about the workspace root rather about `%TEMP%`.
#[test]
fn test_windows_writes_inside_the_workspace_root_are_allowed() {
    let name = "test_windows_writes_inside_the_workspace_root_are_allowed";
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };
    let inside = workspace.path().join("inside-marker");

    let out = run_child(workspace.path(), &write_payload(&inside), None, false);
    let stderr = stderr_of(&out);

    assert_ne!(
        out.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: the sandbox must have been established for this leg to say anything about the \
         rules, stderr: {stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{name}: a write inside the workspace root must succeed, stdout: {} stderr: {stderr}",
        stdout_of(&out)
    );
    let written = std::fs::read_to_string(&inside)
        .unwrap_or_else(|err| format!("cannot read the workspace marker: {err}"));
    assert!(
        written.contains(WRITTEN_MARK),
        "{name}: the confined child must have created the workspace marker, got: {written}"
    );
}

/// The temporary directories are read-write entries of the Windows policy (a toolchain puts its
/// output, locks and swap files there), so a confined child must be able to write there too.
#[test]
fn test_windows_writes_in_the_temporary_directory_are_allowed() {
    let name = "test_windows_writes_in_the_temporary_directory_are_allowed";
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };
    // The fixture is the one thing here that *has* to sit under a temporary directory: the leg is
    // about the temporary-directory grant, so it needs no unlisted-scratch escape.
    let temp_fixture = granted_temp_roots()
        .into_iter()
        .find(|root| root.is_dir())
        .and_then(|root| tempfile::tempdir_in(root).ok());
    let Some(temp_fixture) = temp_fixture else {
        eprintln!("SKIP {name}: this host exposes no temporary directory to write into");
        return;
    };
    let target = temp_fixture.path().join("temp-marker");

    let out = run_child(workspace.path(), &write_payload(&target), None, false);
    let stderr = stderr_of(&out);

    assert_eq!(
        out.status.code(),
        Some(0),
        "{name}: a write into the temporary directories must succeed, stdout: {} stderr: {stderr}",
        stdout_of(&out)
    );
    assert!(
        target.is_file(),
        "{name}: the confined child must have created the temporary-directory marker"
    );
}

/// A write outside every granted root is denied, creates nothing, and is named as a denial — and
/// `%USERPROFILE%`, the one directory the policy refuses to grant, is where that is proved. The
/// three legs are the confined denial, the unconfined run that must succeed in the same fixture,
/// and the opt-out leg, which must **succeed** in that same fixture too: an honoured
/// `MARMEL_DISABLE_SANDBOX=1` means no AppContainer is involved in the run at all.
#[test]
fn test_windows_denies_writes_outside_the_granted_roots() {
    let name = "test_windows_denies_writes_outside_the_granted_roots";
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };
    let Some(profile) = user_profile(name) else {
        return;
    };
    // A uniquely named fixture inside the user profile, removed with the fixture itself when the
    // test ends: nothing here writes into the user's own files.
    let fixture = match tempfile::tempdir_in(&profile) {
        Ok(fixture) => fixture,
        Err(err) => {
            eprintln!("SKIP {name}: cannot create a probe fixture under {profile:?}: {err}");
            return;
        }
    };
    let marker = fixture.path().join("child-wrote-this");
    if under_granted_temp(&marker) || marker.starts_with(workspace.path()) {
        eprintln!(
            "SKIP {name}: the fixture {marker} sits inside a directory the policy grants on its \
             own, so a denial there would prove nothing",
            marker = marker.display()
        );
        return;
    }

    // 1. The confined leg: denied, and nothing created.
    let confined = run_child(workspace.path(), &write_payload(&marker), None, false);
    assert_ne!(
        confined.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: the sandbox must have been established for this leg to mean anything, stderr: {}",
        stderr_of(&confined)
    );
    assert!(
        !confined.status.success(),
        "{name}: a write outside the granted roots must be denied, stdout: {} stderr: {}",
        stdout_of(&confined),
        stderr_of(&confined)
    );
    assert!(
        !marker.exists(),
        "{name}: the denied write must not have created a file"
    );
    assert!(
        names_denial(&confined),
        "{name}: the denied leg must name the refusal on its own output, stdout: {} stderr: {}",
        stdout_of(&confined),
        stderr_of(&confined)
    );

    // 2. The attribution leg, run by cmd.exe outside any AppContainer: the very same payload writes
    //    the very same marker, so leg 1 denied a reachable write rather than an unreachable one.
    let unconfined = run_unconfined(workspace.path(), &write_payload(&marker));
    assert!(
        unconfined.status.success(),
        "{name}: an unconfined run must be able to write where a confined one cannot, stdout: \
         {} stderr: {}",
        stdout_of(&unconfined),
        stderr_of(&unconfined)
    );
    assert!(
        marker.is_file(),
        "{name}: the unconfined run must have created the fixture marker"
    );

    // 3. The opt-out leg, in two parts. `MARMEL_DISABLE_SANDBOX=1` is the operator's one way to
    //    run without the backend, so the re-entry child has to take the payload out of any
    //    AppContainer on that path — and the warning that says so must be printed, because an
    //    opt-out nobody can see is an opt-out nobody can trust.
    let opted = run_child(workspace.path(), "echo opted-out", None, true);
    assert_eq!(
        opted.status.code(),
        Some(0),
        "{name}: an explicit opt-out must still run the command, stderr: {}",
        stderr_of(&opted)
    );
    assert!(
        stdout_of(&opted).contains("opted-out"),
        "{name}: the pending command must have run, got: {}",
        stdout_of(&opted)
    );
    let opted_stderr = stderr_of(&opted);
    assert!(
        opted_stderr.contains("DISABLED") && opted_stderr.contains("AppContainer"),
        "{name}: a skipped backend must be warned about by name, got: {opted_stderr}"
    );
    assert!(
        opted_stderr.contains(SANDBOX_OPTOUT_ENV),
        "{name}: the opt-out warning must name the variable the operator set, got: {opted_stderr}"
    );

    // 3b. And the proof leg 1 could not give: the very same kind of write, into the very same
    //      fixture the sandbox denied, by an opted-out run. This marker is one no earlier leg
    //      touched, so its creation is the opt-out's doing and nothing else's — which is what an
    //      honoured opt-out has to mean: the command runs, and it runs in the clear.
    let opted_marker = fixture.path().join("opt-out-wrote-this");
    let opted_write = run_child(workspace.path(), &write_payload(&opted_marker), None, true);
    assert_eq!(
        opted_write.status.code(),
        Some(0),
        "{name}: an opted-out run must be able to write where the confined run could not, \
         stdout: {} stderr: {}",
        stdout_of(&opted_write),
        stderr_of(&opted_write)
    );
    assert!(
        opted_marker.is_file(),
        "{name}: the opted-out run must have created the marker the confined run could not"
    );
    let written = std::fs::read_to_string(&opted_marker)
        .expect("the opted-out marker must be readable after the leg that wrote it");
    assert!(
        written.contains(WRITTEN_MARK),
        "{name}: the marker must hold what the payload wrote, got: {written}"
    );
}

/// The sensitive directories stay out of reach: `%USERPROFILE%\.ssh` is not readable from inside an
/// AppContainer, and an operator grant of the directory around it cannot reach it, because the
/// Windows policy refuses to grant the user-profile root at all (that is the invariant behind
/// `SENSITIVE_DENIED_BASENAMES` on this platform).
#[test]
fn test_windows_denies_the_sensitive_ssh_directory() {
    let name = "test_windows_denies_the_sensitive_ssh_directory";
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };
    let Some(profile) = user_profile(name) else {
        return;
    };
    let ssh = profile.join(".ssh");

    // 1. A read of the real sensitive directory, confined. Reading is all this does: the test never
    //    creates anything under the user's own profile.
    if ssh.is_dir() {
        let confined = run_child(workspace.path(), &list_payload(&ssh), None, false);
        assert_ne!(
            confined.status.code(),
            Some(SANDBOX_REFUSE_EXIT_CODE),
            "{name}: the sandbox must have been established for this leg to mean anything, \
             stderr: {}",
            stderr_of(&confined)
        );
        assert!(
            !confined.status.success(),
            "{name}: a read of the sensitive directory must be denied, stdout: {} stderr: {}",
            stdout_of(&confined),
            stderr_of(&confined)
        );
        assert!(
            names_denial(&confined),
            "{name}: the denied read must name the refusal on its own output, stdout: {} stderr: \
             {}",
            stdout_of(&confined),
            stderr_of(&confined)
        );

        // Attribution: an unconfined run in the same directory reads it fine, so the denial above is
        // the sandbox's, not a fixture the shell could not reach.
        let unconfined = run_unconfined(workspace.path(), &list_payload(&ssh));
        assert!(
            unconfined.status.success(),
            "{name}: an unconfined run must be able to list what a confined one cannot, stdout: \
             {} stderr: {}",
            stdout_of(&unconfined),
            stderr_of(&unconfined)
        );
    } else {
        eprintln!(
            "SKIP {name}: {} does not exist on this host, and the probe must not create it, \
             because writing into the real user profile is off limits and a fixture under a \
             temporary directory would be read-write granted on its own",
            ssh.display()
        );
    }

    // 2. An operator grant of the user-profile root cannot reach it: the entry is refused by name,
    //    the note the policy prints says why, and the sensitive directory is still unreachable.
    let extras = windows_extras(&[&profile]);
    let granted_run = run_child(workspace.path(), &list_payload(&ssh), Some(&extras), false);
    assert_ne!(
        granted_run.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: the sandbox must have been established for this leg to mean anything, stderr: {}",
        stderr_of(&granted_run)
    );
    let granted_stderr = stderr_of(&granted_run);
    assert!(
        granted_stderr.contains("user profile directory itself is never granted"),
        "{name}: the policy must say out loud that it refuses to grant the user profile root, \
         got: {granted_stderr}"
    );
    assert!(
        !granted_run.status.success(),
        "{name}: an operator grant of the profile root must not make the sensitive directory \
         readable, stdout: {} stderr: {granted_stderr}",
        stdout_of(&granted_run)
    );
}

/// An operator-supplied extra read-only path becomes a live grant through the extras channel —
/// proved by a confined read that fails without the entry and succeeds with it, in the Windows
/// encoding of that channel (line-feed separated, because `:` is part of every absolute Windows
/// path and would split the value inside the drive prefix).
#[test]
fn test_windows_honours_an_operator_extra_read_only_path() {
    let name = "test_windows_honours_an_operator_extra_read_only_path";
    let Some(scratch) = unlisted_scratch(name) else {
        return;
    };
    if under_granted_temp(scratch.as_path()) {
        eprintln!(
            "SKIP {name}: the fixture root {} sits inside a temporary directory the policy grants \
             read-write on its own, so the ungranted read below would prove nothing",
            scratch.display()
        );
        return;
    }
    // Two fixture roots, published as a two-entry list in the Windows encoding, so one leg covers
    // both the separator and the grant.
    let first = tempfile::tempdir_in(&scratch).expect("the first extras fixture must be creatable");
    let second =
        tempfile::tempdir_in(&scratch).expect("the second extras fixture must be creatable");
    let first_file = first.path().join("readable");
    let second_file = second.path().join("readable");
    for file in [&first_file, &second_file] {
        std::fs::write(file, format!("{GRANTED_MARK}\n"))
            .expect("the extras fixture file must be writable");
    }
    let workspace = match tempfile::tempdir_in(&scratch) {
        Ok(workspace) => workspace,
        Err(err) => {
            eprintln!("SKIP {name}: cannot create a workspace fixture in {scratch:?}: {err}");
            return;
        }
    };
    let payload = read_payload(&[first_file.as_path(), second_file.as_path()]);

    // 1. Without the entry, the confined read fails: the fixture is not an allow-list entry.
    let ungranted = run_child(workspace.path(), &payload, None, false);
    assert_ne!(
        ungranted.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: the sandbox must have been established for this leg to mean anything, stderr: {}",
        stderr_of(&ungranted)
    );
    assert!(
        !ungranted.status.success(),
        "{name}: an ungranted path must stay unreadable, stderr: {}",
        stderr_of(&ungranted)
    );

    // 2. With the entry published in the Windows encoding, the same payload reads both fixtures.
    let extras = windows_extras(&[first.path(), second.path()]);
    let granted = run_child(workspace.path(), &payload, Some(&extras), false);
    assert_eq!(
        granted.status.code(),
        Some(0),
        "{name}: the published paths must be readable under their own grant, stdout: {} stderr: \
         {}",
        stdout_of(&granted),
        stderr_of(&granted)
    );
    let granted_stdout = stdout_of(&granted);
    assert!(
        granted_stdout.contains(GRANTED_MARK) && granted_stdout.matches(GRANTED_MARK).count() >= 2,
        "{name}: the operator grant must reach both paths it names, got: {granted_stdout}"
    );

    // 3. Attribution: an unconfined run reads the fixtures with no entry at all, so leg 1 denied a
    //    readable file rather than an unreachable one.
    let unconfined = run_unconfined(workspace.path(), &payload);
    assert!(
        unconfined.status.success()
            && stdout_of(&unconfined).contains(GRANTED_MARK)
            && stderr_of(&unconfined).is_empty(),
        "{name}: the fixtures must be readable when nothing is enforced, stdout: {} stderr: {}",
        stdout_of(&unconfined),
        stderr_of(&unconfined)
    );
}

/// A run whose AppContainer cannot be established must fail closed: exit
/// [`SANDBOX_REFUSE_EXIT_CODE`], the operator-facing refusal naming the only way out, and nothing
/// run. There is no test-only knob on this backend, so the failures are driven through the policy:
/// a workspace root the policy refuses to grant (it *contains* the sensitive `.ssh` directory), and
/// a workspace root that cannot be resolved at all.
#[test]
fn test_windows_refuses_when_the_appcontainer_cannot_be_established() {
    let name = "test_windows_refuses_when_the_appcontainer_cannot_be_established";

    // Leg A: the workspace root is the user profile itself, i.e. an entry that contains a denied
    // sensitive directory. The policy has no honest list for it, so it refuses rather than grants.
    let Some(profile) = user_profile(name) else {
        return;
    };
    let payload = format!("echo {REFUSE_MARK}");
    let refused = run_child(&profile, &payload, None, false);
    let refused_stderr = stderr_of(&refused);
    assert!(
        !stdout_of(&refused).contains(REFUSE_MARK),
        "{name}: a refused run must run nothing, got stdout: {}",
        stdout_of(&refused)
    );
    assert_eq!(
        refused.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: a workspace root the policy cannot grant must exit with the refusal code, \
         stderr: {refused_stderr}"
    );
    assert!(
        refused_stderr.contains("refusing to run the command")
            && refused_stderr.contains(SANDBOX_OPTOUT_ENV),
        "{name}: the refusal must be the operator-facing one and name the only way out, got: \
         {refused_stderr}"
    );
    assert!(
        refused_stderr.contains("sandbox policy could not be built for")
            && refused_stderr.contains("contains the sensitive directory")
            && refused_stderr.contains(".ssh"),
        "{name}: the refusal must name the policy failure and the denied directory behind it, \
         got: {refused_stderr}"
    );

    // Leg B: a workspace root that cannot be resolved at all (a directory that no longer exists).
    let gone = tempfile::tempdir().expect("workspace tempdir");
    let gone_path = gone.path().to_path_buf();
    drop(gone);
    let unresolved = run_child(&gone_path, &payload, None, false);
    let unresolved_stderr = stderr_of(&unresolved);
    assert!(
        !stdout_of(&unresolved).contains(REFUSE_MARK),
        "{name}: a refused run must run nothing, got stdout: {}",
        stdout_of(&unresolved)
    );
    assert_eq!(
        unresolved.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: an unresolvable workspace root must exit with the refusal code, stderr: {unresolved_stderr}"
    );
    assert!(
        unresolved_stderr.contains("could not be resolved")
            && unresolved_stderr.contains("refusing to run the command"),
        "{name}: the refusal must name the workspace root it could not resolve, got: \
         {unresolved_stderr}"
    );

    // Leg C: the opt-out third leg, on a workspace the policy *can* grant: an explicit opt-out is
    // honoured before anything is attempted, runs the command, and is warned about by name.
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };
    let opted = run_child(workspace.path(), "echo opted-out", None, true);
    assert_eq!(
        opted.status.code(),
        Some(0),
        "{name}: an explicit opt-out must still run the command, stderr: {}",
        stderr_of(&opted)
    );
    assert!(
        stdout_of(&opted).contains("opted-out") && !stdout_of(&opted).contains(REFUSE_MARK),
        "{name}: the pending command must have run, got: {}",
        stdout_of(&opted)
    );
    let opted_stderr = stderr_of(&opted);
    assert!(
        opted_stderr.contains("DISABLED") && opted_stderr.contains("AppContainer"),
        "{name}: a skipped backend must be warned about by name, got: {opted_stderr}"
    );
    assert!(
        !opted_stderr.contains("refusing to run the command"),
        "{name}: an honoured opt-out is never a refusal, got: {opted_stderr}"
    );
}

/// Regression for the inherited-handle capability grant: the one-shot run hands the confined child
/// the anonymous pipes of the re-entry process, and an AppContainer may only use an inherited kernel
/// object once that object carries an ACE for its identity. Without the grant the child runs and
/// dies silently, because `CreateProcessW` succeeded and `cmd.exe` could not write anything.
#[test]
fn test_windows_confined_child_prints_on_stdout_and_stderr() {
    let name = "test_windows_confined_child_prints_on_stdout_and_stderr";
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let stdout_leg = run_child(
        workspace.path(),
        &format!("echo {STDOUT_MARK}"),
        None,
        false,
    );
    assert_eq!(
        stdout_leg.status.code(),
        Some(0),
        "{name}: the confined leg must not be a refusal, stderr: {}",
        stderr_of(&stdout_leg)
    );
    assert!(
        stdout_of(&stdout_leg).contains(STDOUT_MARK),
        "{name}: the confined child must be able to write on stdout, got: {}",
        stdout_of(&stdout_leg)
    );

    // The same shape, aimed at stderr: `1>&2` is how cmd.exe redirects to the other stream, and the
    // two std objects are granted separately by the backend.
    let stderr_leg = run_child(
        workspace.path(),
        &format!("echo {STDERR_MARK} 1>&2"),
        None,
        false,
    );
    assert_eq!(
        stderr_leg.status.code(),
        Some(0),
        "{name}: the stderr leg must not be a refusal, stdout: {}",
        stdout_of(&stderr_leg)
    );
    assert!(
        stderr_of(&stderr_leg).contains(STDERR_MARK),
        "{name}: the confined child must be able to write on stderr, got: {}",
        stderr_of(&stderr_leg)
    );
}

/// A bare `cmd` payload is an interactive-shell request (rule R1 of `pty::windows_shell_argv`), and
/// the confined launch has to honour it: `cmd.exe /C cmd` starts a `cmd.exe` looking for a program
/// called `cmd`, ends the session, and makes an interactive request look like a failure. With stdin
/// closed the bare shell prints its own banner and leaves, so the payload staying bare is visible
/// from both what it prints and what it does not.
#[test]
fn test_windows_confined_reentry_keeps_a_bare_interactive_shell() {
    let name = "test_windows_confined_reentry_keeps_a_bare_interactive_shell";
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    let out = run_child(workspace.path(), "cmd", None, false);
    let combined = format!("stdout: {} stderr: {}", stdout_of(&out), stderr_of(&out));

    assert_ne!(
        out.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: an interactive request must not be turned into a refusal, {combined}"
    );
    assert!(
        out.status.success(),
        "{name}: a bare confined shell must not report a failure, {combined}"
    );
    // The banner is what a bare `cmd.exe` prints when it is started as a shell; a payload that had
    // been turned into `<comspec> /C cmd` would have resolved `cmd` through PATH instead, and any
    // error vocabulary on either side of that lookup shows up in the same output.
    assert!(
        stdout_of(&out).contains("Microsoft Windows"),
        "{name}: the confined child must have run the bare shell, {combined}"
    );
    let combined_lower = combined.to_lowercase();
    assert!(
        !combined_lower.contains("not recognized")
            && !combined_lower.contains("cannot find")
            && !combined_lower.contains("syntax of the command"),
        "{name}: the payload must have stayed a bare shell, with no shell-lookup error in the \
         output, {combined}"
    );
}

// ---------------------------------------------------------------------------
// The functional legs: what a real confined spawn has to prove
// ---------------------------------------------------------------------------
//
// Everything above answers "what does the AppContainer policy enforce?". The legs below answer the
// question a host with a broken sandbox asks instead: does a real confined command run at all, or
// does every command cost [`SANDBOX_REFUSE_EXIT_CODE`]? They are shaped like a production run — the
// built `[[bin]]` target of this package (`marmel`, reported to an integration test as
// `CARGO_BIN_EXE_marmel`) driven the way `run_command` drives it, and the backend's own entry points
// driven with real `cmd.exe` payloads in `tempfile` fixtures — so a host on which the sandbox never
// gets past its own attempt fails here, loudly, while every leg above it passes.

/// The marker of the end-to-end legs: a pending command that ran, printed it.
const E2E_MARK: &str = "SANDBOX-OK";
/// The marker of the confined command a runner leg launches inside a healthy setup.
const RUNNER_MARK: &str = "RUNNER-PROBE-OK";
/// As [`STDOUT_MARK`], on the standard stream a runner-level confined child inherits.
const RUNNER_STDOUT_MARK: &str = "RUNNER-STDOUT-OK";
/// As [`STDERR_MARK`], on the standard stream a runner-level confined child inherits.
const RUNNER_STDERR_MARK: &str = "RUNNER-STDERR-OK";
/// The knob that tells a test process it is the **worker** of the confined-streams leg: the process
/// that launches a confined child is the process whose standard streams that child writes on, so the
/// leg has to drive a process whose output it can read — see
/// [`test_windows_runner_confined_child_writes_on_the_inherited_streams`].
const RUNNER_CHILD_ENV: &str = "MARMEL_WINDOWS_RUNNER_CHILD";

/// The text of a file a runner payload was told to write its own streams into. At runner level the
/// payload's stdout and stderr belong to the **test process**, which no leg can read back, so the
/// payload redirects them into the (granted) workspace, where the leg can read them instead.
fn captured(file: &Path) -> String {
    std::fs::read_to_string(file).unwrap_or_else(|err| {
        panic!(
            "the runner leg must be able to read {}: {err}",
            file.display()
        )
    })
}

/// The payload of a runner leg, with the streams of the confined child captured into `stdout_file`
/// and `stderr_file` inside the workspace the leg can read.
fn runner_captured_payload(command: &str, stdout_file: &Path, stderr_file: &Path) -> String {
    format!(
        "{command} > {} 2> {}",
        quoted(stdout_file),
        quoted(stderr_file)
    )
}

/// The lower-cased text of a path, for the comparisons Windows itself does case-insitionally.
fn path_text_lower(path: &Path) -> String {
    path.display().to_string().to_lowercase()
}

/// The end-to-end leg, on the exact shape a production `run_command` uses: the built bin target, a
/// `tempfile` fixture workspace and one pending command. It proves the whole chain at once — the
/// re-entry protocol, the Windows attempt, the parked hand-off setup, the confined launch, the exit
/// code the harness reports, and the write the confined shell performs inside the workspace.
#[test]
fn test_windows_reentry_end_to_end_run_in_a_fixture_workspace_succeeds() {
    let name = "test_windows_reentry_end_to_end_run_in_a_fixture_workspace_succeeds";
    let workspace = tempfile::tempdir().expect("fixture workspace");

    // Leg 1: a trivial command, confined, in a fixture workspace. The exit code is the point: a host
    // whose sandbox cannot be established answers 73 here and runs nothing, which is exactly the
    // desktop failure these legs exist to catch.
    let run = run_child(workspace.path(), &format!("echo {E2E_MARK}"), None, false);
    let run_stderr = stderr_of(&run);
    assert_eq!(
        run.status.code(),
        Some(0),
        "{name}: a confined command in a fixture workspace must run and exit 0 — a refusal means the \
         sandbox itself is broken on this host, not the command — stdout: {} stderr: {run_stderr}",
        stdout_of(&run)
    );
    assert!(
        stdout_of(&run).contains(E2E_MARK),
        "{name}: the pending command must have run and printed what it echoed, got: {}",
        stdout_of(&run)
    );
    assert!(
        !run_stderr.contains(SANDBOX_REFUSAL_MARKER),
        "{name}: a healthy run must not print a refusal line, got: {run_stderr}"
    );

    // Leg 2: the in-workspace write a real tool call performs — a nested path inside the workspace,
    // written by the confined shell through its own redirection. The workspace root is the one
    // **required** policy entry, so a confined command that cannot write there cannot run a build, a
    // test run or a `git` invocation at all.
    let nested = workspace.path().join("nested");
    std::fs::create_dir_all(&nested).expect("nested directory inside the fixture workspace");
    let inside = nested.join("inside.txt");
    let write = run_child(workspace.path(), &write_payload(&inside), None, false);
    assert_eq!(
        write.status.code(),
        Some(0),
        "{name}: a nested write inside the workspace must succeed, stdout: {} stderr: {}",
        stdout_of(&write),
        stderr_of(&write)
    );
    let written = std::fs::read_to_string(&inside)
        .unwrap_or_else(|err| format!("cannot read the nested workspace marker: {err}"));
    assert!(
        written.contains(WRITTEN_MARK),
        "{name}: the confined shell must have created the nested workspace marker, got: {written}"
    );
}

/// The failure contract of the same chain: a workspace whose policy cannot be built — here a cwd
/// that no longer names a directory — has to cost [`SANDBOX_REFUSE_EXIT_CODE`], run nothing, and say
/// so with the marker, the step and the class a refusal is diagnosed by.
#[test]
fn test_windows_reentry_refuses_an_unresolvable_workspace_root_by_name() {
    let name = "test_windows_reentry_refuses_an_unresolvable_workspace_root_by_name";
    let gone = tempfile::tempdir().expect("workspace tempdir");
    let gone_path = gone.path().to_path_buf();
    drop(gone);

    let refused = run_child(&gone_path, &format!("echo {E2E_MARK}"), None, false);
    let refused_stderr = stderr_of(&refused);
    assert_eq!(
        refused.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: an unresolvable workspace root must exit with the refusal code, stderr: {refused_stderr}"
    );
    assert!(
        !stdout_of(&refused).contains(E2E_MARK),
        "{name}: a refused run runs nothing, got: {}",
        stdout_of(&refused)
    );
    assert!(
        refused_stderr.contains(SANDBOX_REFUSAL_MARKER),
        "{name}: the refusal line must carry the marker the harness looks for, got: {refused_stderr}"
    );
    assert!(
        refused_stderr.contains("refusing to run the command")
            && refused_stderr.contains("could not be resolved"),
        "{name}: the refusal must name the workspace root it could not resolve, got: {refused_stderr}"
    );
    assert!(
        refused_stderr.contains("step = sandbox attempt")
            && refused_stderr.contains("class = rule-setup"),
        "{name}: the refusal must carry the step and the class it is diagnosed by, got: {refused_stderr}"
    );
}

/// The exit code a confined command reports back through the backend's own runner: verbatim, whether
/// it is the success of a command that ran or the status of one that asked to fail. This is the leg
/// the helper-level unit tests in `src/harness/sandbox_windows.rs` are not: a real `cmd.exe` process,
/// launched inside a real AppContainer, over a real policy built for a fixture workspace.
#[test]
fn test_windows_runner_reports_the_exit_code_of_a_confined_command() {
    let name = "test_windows_runner_reports_the_exit_code_of_a_confined_command";
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let setup = prepare_windows_policy(workspace.path(), &[])
        .unwrap_or_else(|err| panic!("{name}: the policy pass must build a setup: {err}"));

    for (payload, expected) in [("exit 0", 0), ("exit 7", 7)] {
        let code = spawn_in_appcontainer(&setup, workspace.path(), payload)
            .unwrap_or_else(|err| panic!("{name}: `{payload}` must launch confined: {err}"));
        assert_eq!(
            code, expected,
            "{name}: `{payload}` must report exit code {expected}, got {code}"
        );
    }

    // A command `cmd.exe` cannot resolve is reported as the failure it is, never as a success.
    let missing =
        spawn_in_appcontainer(&setup, workspace.path(), "this-command-is-not-on-the-path")
            .unwrap_or_else(|err| {
                panic!("{name}: an unknown command must still launch confined: {err}")
            });
    assert_ne!(
        missing, 0,
        "{name}: an unknown command must not look like a success, got exit code {missing}"
    );
}

/// The confined-streams leg, in two halves: a confined child inherits and writes on the standard
/// streams of the process that launched it, and no leg can read those back out of its own process, so
/// this test drives **itself** as a worker — re-running this test binary with only itself selected
/// (`--exact`) — and reads what the confined children wrote into the worker's own pipes.
#[test]
fn test_windows_runner_confined_child_writes_on_the_inherited_streams() {
    let name = "test_windows_runner_confined_child_writes_on_the_inherited_streams";

    // Half 1, the worker: this process launches the confined children, so its standard streams — the
    // pipes the driving leg below owns and reads — are exactly the streams they write on.
    if std::env::var_os(RUNNER_CHILD_ENV).is_some() {
        let workspace = tempfile::tempdir().expect("worker workspace");
        let setup = prepare_windows_policy(workspace.path(), &[])
            .unwrap_or_else(|err| panic!("{name}: the worker must build a policy: {err}"));
        for payload in [
            format!("echo {RUNNER_STDOUT_MARK}"),
            format!("echo {RUNNER_STDERR_MARK} 1>&2"),
        ] {
            let code = spawn_in_appcontainer(&setup, workspace.path(), &payload)
                .unwrap_or_else(|err| panic!("{name}: {payload} must launch confined: {err}"));
            assert_eq!(
                code, 0,
                "{name}: {payload} must exit 0 in the worker, got {code}"
            );
        }
        return;
    }

    // Half 2, the driving leg: run the worker, and assert that the confined writes came back through
    // the worker's own streams. `--nocapture` is what keeps the worker's own harness from capturing
    // the very output this leg is reading, and `--exact` that only the worker leg runs there.
    let worker = std::env::current_exe().expect("the test binary is the worker");
    let out = Command::new(&worker)
        .arg("--exact")
        .arg(name)
        .arg("--nocapture")
        .env(RUNNER_CHILD_ENV, "1")
        .env_remove(SANDBOX_OPTOUT_ENV)
        .env_remove(SANDBOX_EXTRA_RO_PATHS_ENV)
        .output()
        .expect("the worker leg must be runnable");
    let worker_stdout = stdout_of(&out);
    let worker_stderr = stderr_of(&out);
    assert!(
        out.status.success(),
        "{name}: the worker leg must finish green, exit: {:?}, stdout: {worker_stdout} stderr: {worker_stderr}",
        out.status.code()
    );
    assert!(
        worker_stdout.contains("running 1 test"),
        "{name}: `--exact` must have selected exactly the worker leg, got: {worker_stdout}"
    );
    assert!(
        worker_stdout.contains(RUNNER_STDOUT_MARK),
        "{name}: the confined child must have written on the stdout it inherited, got: {worker_stdout}"
    );
    assert!(
        worker_stderr.contains(RUNNER_STDERR_MARK),
        "{name}: the confined child must have written on the stderr it inherited, got: {worker_stderr}"
    );
}

/// A nested write inside the workspace, by a confined command launched straight through the runner:
/// the workspace root is the one **required** entry of the policy, and the grant the policy applies
/// for it is inherited over the tree, so a subdirectory has to be writable too. Proved with the
/// workspace living outside every built-in grant, i.e. outside the temporary directories the policy
/// grants read-write on its own.
#[test]
fn test_windows_runner_allows_a_nested_write_inside_the_workspace() {
    let name = "test_windows_runner_allows_a_nested_write_inside_the_workspace";
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };
    let nested = workspace.path().join("nested");
    std::fs::create_dir_all(&nested).expect("nested directory inside the workspace fixture");
    let inside = nested.join("inside-marker");

    let setup = prepare_windows_policy(workspace.path(), &[])
        .unwrap_or_else(|err| panic!("{name}: the policy pass must build a setup: {err}"));
    let code = spawn_in_appcontainer(&setup, workspace.path(), &write_payload(&inside))
        .unwrap_or_else(|err| panic!("{name}: the in-workspace write must launch confined: {err}"));

    assert_eq!(
        code, 0,
        "{name}: a write into a subdirectory of the workspace must exit 0 confined, {name} saw the \
         exit code {code}"
    );
    let written = std::fs::read_to_string(&inside)
        .unwrap_or_else(|err| format!("cannot read the workspace marker: {err}"));
    assert!(
        written.contains(WRITTEN_MARK),
        "{name}: the confined command must have written the marker it was asked to, got: {written}"
    );
}

/// The content of a file that existed in the workspace **before** the sandbox was set up: the shape
/// no leg used to ask about, and the one a root-only DACL write has to keep working (t-034).
const PRE_EXISTING_MARK: &str = "PRE-EXISTING-WORKSPACE-CONTENT";

/// A confined child has to read what was **already in the workspace** when the sandbox was set up,
/// both at the root and inside a directory that already existed. This is the case the handle-based
/// workspace-root write (t-034) could regress: the grant on the root is authoritative for the root
/// and for everything created afterwards, while a descendant that already had a DACL of its own is
/// reached by inheritance and propagation alone — and no other leg in this file ever asked a confined
/// command to read a file the run did not create.
#[test]
fn test_windows_reads_a_file_that_already_existed_when_the_sandbox_was_set_up() {
    let name = "test_windows_reads_a_file_that_already_existed_when_the_sandbox_was_set_up";
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };

    // Every fixture below is created **before** any sandbox work happens, which is the whole point of
    // the leg: the confinement never saw these objects being created.
    let existing = workspace.path().join("pre-existing.txt");
    let nested_dir = workspace.path().join("pre-existing-dir");
    let nested = nested_dir.join("pre-existing-nested.txt");
    std::fs::create_dir_all(&nested_dir)
        .unwrap_or_else(|err| panic!("{name}: the pre-existing nested directory: {err}"));
    for file in [&existing, &nested] {
        std::fs::write(file, format!("{PRE_EXISTING_MARK}\n")).unwrap_or_else(|err| {
            panic!("{name}: the pre-existing fixture {}: {err}", file.display())
        });
    }
    let payload = read_payload(&[existing.as_path(), nested.as_path()]);

    let out = run_child(workspace.path(), &payload, None, false);
    let stdout = stdout_of(&out);
    let stderr = stderr_of(&out);
    assert_ne!(
        out.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: the sandbox has to be established for this leg to say anything about the rules — a \
         workspace whose existing contents the confinement cannot reach is exactly the shape that \
         exits {SANDBOX_REFUSE_EXIT_CODE} on every command; stderr: {stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{name}: reading a file that existed before the sandbox was set up must succeed confined; \
         stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        stdout.matches(PRE_EXISTING_MARK).count() >= 2,
        "{name}: both pre-existing files — the one in the workspace root and the one in the \
         pre-existing subdirectory — have to be readable confined, got: {stdout}"
    );

    // Attribution: the same payload and the same fixtures with no AppContainer in the picture, so the
    // leg above measures the confinement rather than a fixture the shell could not read anyway.
    let plain = run_unconfined(workspace.path(), &payload);
    assert_eq!(
        plain.status.code(),
        Some(0),
        "{name}: the fixtures have to be readable unconfined too; stderr: {}",
        stderr_of(&plain)
    );
    assert!(
        stdout_of(&plain).contains(PRE_EXISTING_MARK),
        "{name}: the unconfined run reads the same content, otherwise the confined leg above proves \
         nothing: {}",
        stdout_of(&plain)
    );
}

/// A confined child has to create a **new** file inside a directory that already existed when the
/// sandbox was set up — the everyday shape of a build writing into a source or test directory, and
/// the shape that depends on the workspace-root grant covering objects this run did not create.
#[test]
fn test_windows_writes_a_new_file_into_a_pre_existing_nested_directory() {
    let name = "test_windows_writes_a_new_file_into_a_pre_existing_nested_directory";
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };
    let nested_dir = workspace.path().join("pre-existing-dir");
    let sibling = nested_dir.join("pre-existing.txt");
    std::fs::create_dir_all(&nested_dir)
        .unwrap_or_else(|err| panic!("{name}: the pre-existing nested directory: {err}"));
    std::fs::write(&sibling, format!("{PRE_EXISTING_MARK}\n"))
        .unwrap_or_else(|err| panic!("{name}: the pre-existing sibling: {err}"));
    let inside = nested_dir.join("written-by-the-confined-child.txt");

    let out = run_child(workspace.path(), &write_payload(&inside), None, false);
    let stdout = stdout_of(&out);
    let stderr = stderr_of(&out);
    assert_ne!(
        out.status.code(),
        Some(SANDBOX_REFUSE_EXIT_CODE),
        "{name}: the sandbox has to be established for this leg to be about the rules at all; \
         stderr: {stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{name}: creating a new file inside a directory that existed before the sandbox was set up \
         must succeed confined; stdout: {stdout} stderr: {stderr}"
    );
    let written = std::fs::read_to_string(&inside).unwrap_or_else(|err| {
        format!("cannot read the marker in the pre-existing directory: {err}")
    });
    assert!(
        written.contains(WRITTEN_MARK),
        "{name}: the confined child must have created the new file in the pre-existing directory, \
         got: {written}"
    );
    // And the pre-existing content is still there afterwards: the grant added for this run does not
    // rewrite or replace what the directory already held.
    let still_there = std::fs::read_to_string(&sibling)
        .unwrap_or_else(|err| format!("cannot read the pre-existing sibling: {err}"));
    assert!(
        still_there.contains(PRE_EXISTING_MARK),
        "{name}: the pre-existing file in that directory has to survive the run untouched, got: \
         {still_there}"
    );
}

/// The runner-level leg of the same contract, and the leg that pins the ACE itself: after the policy
/// pass the workspace root carries the grant for **this run's own** AppContainer SID, and a command
/// launched inside that setup still reads the files that existed before it was prepared. The
/// propagation this backend asks for after the authoritative root write is best effort, so a host
/// that declines it is reported here as a note rather than silently passing (t-034).
#[test]
fn test_windows_runner_reads_a_pre_existing_workspace_file_and_carries_the_root_ace() {
    let name = "test_windows_runner_reads_a_pre_existing_workspace_file_and_carries_the_root_ace";
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };
    let existing = workspace.path().join("pre-existing.txt");
    let nested_dir = workspace.path().join("pre-existing-dir");
    let nested = nested_dir.join("pre-existing-nested.txt");
    std::fs::create_dir_all(&nested_dir)
        .unwrap_or_else(|err| panic!("{name}: the pre-existing nested directory: {err}"));
    for file in [&existing, &nested] {
        std::fs::write(file, format!("{PRE_EXISTING_MARK}\n")).unwrap_or_else(|err| {
            panic!("{name}: the pre-existing fixture {}: {err}", file.display())
        });
    }

    let setup = prepare_windows_policy(workspace.path(), &[])
        .unwrap_or_else(|err| panic!("{name}: the policy pass must build a setup: {err}"));

    // The grant this run applied is visible on the required entry, naming this run's own SID.
    let coverage =
        acl_coverage(workspace.path(), setup.sid(), Grant::ReadWrite).unwrap_or_else(|err| {
            panic!("{name}: the DACL of the workspace root must be readable: {err}")
        });
    assert!(
        matches!(coverage, AclCoverage::Covered),
        "{name}: the workspace root must carry the AppContainer grant this run applied for its own \
         SID, got {coverage:?}"
    );
    if !setup.propagation_notes.is_empty() {
        eprintln!(
            "NOTE {name}: this host wrote the root grant but declined to propagate it onto \
             pre-existing descendants: {:?}",
            setup.propagation_notes
        );
    }

    // The payload's own streams belong to this process, so they are captured inside the granted
    // workspace, where the leg can read them back.
    let stdout_file = workspace.path().join("leg-stdout.txt");
    let stderr_file = workspace.path().join("leg-stderr.txt");
    let payload = runner_captured_payload(
        &read_payload(&[existing.as_path(), nested.as_path()]),
        &stdout_file,
        &stderr_file,
    );
    let code = spawn_in_appcontainer(&setup, workspace.path(), &payload)
        .unwrap_or_else(|err| panic!("{name}: the pre-existing read must launch confined: {err}"));
    let stdout = captured(&stdout_file);
    let stderr = captured(&stderr_file);
    assert_eq!(
        code, 0,
        "{name}: a confined command that only reads pre-existing workspace files must exit 0, got \
         {code}; stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        stdout.matches(PRE_EXISTING_MARK).count() >= 2,
        "{name}: the confined command must have read both pre-existing files, got: {stdout}"
    );
    assert!(
        !names_denial_in(&stderr),
        "{name}: reading what already existed must not answer with a denial: {stderr}"
    );
}

/// The temporary directories are read-write entries of the Windows policy (a toolchain puts its
/// output there), and the runner has to honour them for a confined command too.
#[test]
fn test_windows_runner_allows_a_write_into_the_temporary_directories() {
    let name = "test_windows_runner_allows_a_write_into_the_temporary_directories";
    let Some(workspace) = workspace_outside_temp(name) else {
        return;
    };
    // The write target is the one thing here that has to sit under a temporary directory: the leg is
    // about that grant, so it needs no unlisted-scratch escape.
    let temp_fixture = granted_temp_roots()
        .into_iter()
        .find(|root| root.is_dir())
        .and_then(|root| tempfile::tempdir_in(root).ok());
    let Some(temp_fixture) = temp_fixture else {
        eprintln!("SKIP {name}: this host exposes no temporary directory to write into");
        return;
    };
    let target = temp_fixture.path().join("runner-temp-marker");

    let setup = prepare_windows_policy(workspace.path(), &[])
        .unwrap_or_else(|err| panic!("{name}: the policy pass must build a setup: {err}"));
    let code = spawn_in_appcontainer(&setup, workspace.path(), &write_payload(&target))
        .unwrap_or_else(|err| {
            panic!("{name}: the temporary-directory write must launch confined: {err}")
        });

    assert_eq!(
        code, 0,
        "{name}: a write into the temporary directories must exit 0 confined, got {code}"
    );
    assert!(
        target.is_file(),
        "{name}: the confined command must have created the temporary-directory marker"
    );
}

/// A write outside every granted root is denied by the runner too, creates nothing, and says so on
/// the confined child's own output — proved in two fixtures the policy never grants: one in the
/// unlisted scratch directory, one inside the user profile, the directory the policy refuses to
/// grant. Each denial is paired with the attribution leg: the very same payload, run unconfined by
/// `cmd.exe`, writes the very same marker.
#[test]
fn test_windows_runner_denies_a_write_outside_the_granted_roots() {
    let name = "test_windows_runner_denies_a_write_outside_the_granted_roots";
    let Some(scratch) = unlisted_scratch(name) else {
        return;
    };
    if under_granted_temp(&scratch) {
        eprintln!(
            "SKIP {name}: the scratch root {} sits inside a temporary directory the policy grants \
             read-write on its own, so a denial there would prove nothing",
            scratch.display()
        );
        return;
    }
    let workspace = match tempfile::tempdir_in(&scratch) {
        Ok(workspace) => workspace,
        Err(err) => {
            eprintln!("SKIP {name}: the workspace fixture could not be created: {err}");
            return;
        }
    };
    let ungranted = tempfile::tempdir_in(&scratch).expect("ungranted fixture in the scratch root");
    let stderr_file = workspace.path().join("runner-denied-stderr");

    // The payloads: the confined one writes its denial text into the workspace, where this leg can
    // read it; the unconfined one keeps the same output shape without an AppContainer in the picture.
    let denied_marker = ungranted.path().join("runner-denied-marker");
    let payload = format!(
        "echo {WRITTEN_MARK} > {} 2> {}",
        quoted(&denied_marker),
        quoted(&stderr_file)
    );

    let setup = prepare_windows_policy(workspace.path(), &[])
        .unwrap_or_else(|err| panic!("{name}: the policy pass must build a setup: {err}"));
    let code = spawn_in_appcontainer(&setup, workspace.path(), &payload)
        .unwrap_or_else(|err| panic!("{name}: the denied write must launch confined: {err}"));
    assert_ne!(
        code,
        0,
        "{name}: a write outside the granted roots must be denied, denial text: {}",
        captured(&stderr_file)
    );
    assert!(
        !denied_marker.exists(),
        "{name}: the denied write must not have created a file"
    );
    let denial_text = captured(&stderr_file);
    assert!(
        names_denial_in(&denial_text),
        "{name}: the denied leg must name the refusal on its own output, got: {denial_text}"
    );

    // Attribution: the same payload, unconfined, writes the very same marker.
    let unconfined = run_unconfined(workspace.path(), &payload);
    assert!(
        unconfined.status.success() && denied_marker.is_file(),
        "{name}: an unconfined run must be able to write where a confined one cannot, stdout: {} \
         stderr: {}",
        stdout_of(&unconfined),
        stderr_of(&unconfined)
    );

    // The same leg inside the user profile — the one directory the policy refuses to grant — with the
    // fixture created and removed by the leg itself, so nothing lands in the user's own files.
    let Some(profile) = user_profile(name) else {
        return;
    };
    let profile_fixture = match tempfile::tempdir_in(&profile) {
        Ok(fixture) => fixture,
        Err(err) => {
            eprintln!(
                "SKIP {name}: cannot create a probe fixture under {:?}: {err}",
                profile
            );
            return;
        }
    };
    let profile_marker = profile_fixture.path().join("runner-profile-marker");
    if under_granted_temp(&profile_marker) || profile_marker.starts_with(workspace.path()) {
        eprintln!(
            "SKIP {name}: the profile fixture {} sits inside a directory the policy grants on its \
             own, so a denial there would prove nothing",
            profile_marker.display()
        );
        return;
    }
    let profile_payload = format!(
        "echo {WRITTEN_MARK} > {} 2> {}",
        quoted(&profile_marker),
        quoted(&stderr_file)
    );
    let profile_code = spawn_in_appcontainer(&setup, workspace.path(), &profile_payload)
        .unwrap_or_else(|err| panic!("{name}: the profile write must launch confined: {err}"));
    assert_ne!(
        profile_code,
        0,
        "{name}: a write inside the user profile must be denied, denial text: {}",
        captured(&stderr_file)
    );
    assert!(
        !profile_marker.exists(),
        "{name}: the denied profile write must not have created a file"
    );
    let profile_denial = captured(&stderr_file);
    assert!(
        names_denial_in(&profile_denial),
        "{name}: the denied profile leg must name the refusal, got: {profile_denial}"
    );
}

/// A key in the sensitive `.ssh` directory is unreachable from the runner too: the confined read
/// exits non-zero, produces nothing, and names the refusal, while the same payload read by an
/// unconfined `cmd.exe` reads it fine. The leg never creates anything in the user's own profile:
/// where the host has no key file to read, the same directory is probed by listing it.
#[test]
fn test_windows_runner_denies_a_key_in_the_sensitive_ssh_directory() {
    let name = "test_windows_runner_denies_a_key_in_the_sensitive_ssh_directory";
    let Some(scratch) = unlisted_scratch(name) else {
        return;
    };
    let workspace = match tempfile::tempdir_in(&scratch) {
        Ok(workspace) => workspace,
        Err(err) => {
            eprintln!("SKIP {name}: the workspace fixture could not be created: {err}");
            return;
        }
    };
    let Some(profile) = user_profile(name) else {
        return;
    };
    let ssh = profile.join(".ssh");
    if !ssh.is_dir() {
        eprintln!(
            "SKIP {name}: {} does not exist on this host, and this leg must not create it: writing \
             into the real user profile is off limits, and a fixture under a temporary directory \
             would be read-write granted on its own",
            ssh.display()
        );
        return;
    }
    let key = ssh.join("id_ed25519");
    // The subject depends on what the host has: a real key read **through** the sensitive directory —
    // the read that matters for a confined `ssh`/`git` — or, when this host holds no key to read, the
    // listing of the directory itself, which the policy denies for the same reason.
    let subject = if key.is_file() {
        read_payload(&[key.as_path()])
    } else {
        list_payload(&ssh)
    };
    let stdout_file = workspace.path().join("runner-ssh-stdout");
    let stderr_file = workspace.path().join("runner-ssh-stderr");
    let payload = runner_captured_payload(&subject, &stdout_file, &stderr_file);

    let setup = prepare_windows_policy(workspace.path(), &[])
        .unwrap_or_else(|err| panic!("{name}: the policy pass must build a setup: {err}"));
    let code = spawn_in_appcontainer(&setup, workspace.path(), &payload).unwrap_or_else(|err| {
        panic!("{name}: the sensitive-directory read must launch confined: {err}")
    });
    let denial_text = captured(&stderr_file);
    assert_ne!(
        code, 0,
        "{name}: a read of the sensitive directory must be denied, denial text: {denial_text}"
    );
    assert!(
        names_denial_in(&denial_text),
        "{name}: the denied read must name the refusal on its own output, got: {denial_text}"
    );
    let body = captured(&stdout_file);
    assert!(
        body.trim().is_empty(),
        "{name}: the denied read must not have produced anything, got: {body}"
    );

    // Attribution: the very same payload, run unconfined, reaches the same subject.
    let unconfined = run_unconfined(workspace.path(), &payload);
    assert!(
        unconfined.status.success(),
        "{name}: an unconfined run must be able to read where a confined one cannot, stdout: {} \
         stderr: {}",
        stdout_of(&unconfined),
        stderr_of(&unconfined)
    );
    if key.is_file() {
        let read_at_last = captured(&stdout_file);
        assert!(
            !read_at_last.trim().is_empty(),
            "{name}: the unconfined leg must have read the key a confined leg cannot, got: {read_at_last}"
        );
    }
}

/// The read-only entries of the policy reach a real toolchain: a confined command reads the `PATH`
/// the lookup is decided by, and `where cmd.exe` walks every entry and reads the file it finds. This
/// is the leg a confined `cargo`, `cl` or `git` depends on, and it is the one that fails when a
/// toolchain directory stops being granted.
#[test]
fn test_windows_runner_reads_a_system_binary_through_the_read_only_grant() {
    let name = "test_windows_runner_reads_a_system_binary_through_the_read_only_grant";
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let setup = prepare_windows_policy(workspace.path(), &[])
        .unwrap_or_else(|err| panic!("{name}: the policy pass must build a setup: {err}"));

    // 1. The environment of the confined shell: what a toolchain lookup is decided by.
    let path_file = workspace.path().join("runner-path.txt");
    let path_stderr = workspace.path().join("runner-path-stderr.txt");
    let env_code = spawn_in_appcontainer(
        &setup,
        workspace.path(),
        &runner_captured_payload("set PATH", &path_file, &path_stderr),
    )
    .unwrap_or_else(|err| panic!("{name}: the PATH read must launch confined: {err}"));
    assert_eq!(
        env_code,
        0,
        "{name}: a confined command must be able to read its own PATH, stderr: {}",
        captured(&path_stderr)
    );
    assert!(
        captured(&path_file).to_lowercase().contains("path"),
        "{name}: the confined shell must have printed its PATH"
    );

    // 2. The lookup itself: `where` probes every PATH entry, reading the file it is after.
    let where_file = workspace.path().join("runner-where.txt");
    let where_stderr = workspace.path().join("runner-where-stderr.txt");
    let lookup_code = spawn_in_appcontainer(
        &setup,
        workspace.path(),
        &runner_captured_payload("where cmd.exe", &where_file, &where_stderr),
    )
    .unwrap_or_else(|err| panic!("{name}: the toolchain lookup must launch confined: {err}"));
    assert_eq!(
        lookup_code,
        0,
        "{name}: a confined lookup of a system binary must succeed, stderr: {}",
        captured(&where_stderr)
    );
    let found = captured(&where_file);
    let found_lower = found.to_lowercase();
    assert!(
        found_lower.contains("cmd.exe"),
        "{name}: the confined lookup must have resolved a system binary, got: {found}"
    );
    if let Some(system_root) = std::env::var_os("SystemRoot").filter(|value| !value.is_empty()) {
        let system_root = PathBuf::from(system_root);
        assert!(
            found_lower.contains(&path_text_lower(&system_root)),
            "{name}: the resolved system binary must live under the system root {system_root:?} \
             this host reports, got: {found}"
        );
    }
}

/// The capability the launch declares, as the policy builds it: an AppContainer with no capabilities
/// is a filesystem-only identity, and `internetClient` (`S-1-15-3-1`) is what lets a confined
/// `cargo` or `curl` talk to the network. Asserted on the setup the policy pass built, and on the
/// block the launch layer attaches — no outbound connection required, and none attempted.
#[test]
fn test_windows_policy_declares_the_internet_client_capability() {
    let name = "test_windows_policy_declares_the_internet_client_capability";
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let setup = prepare_windows_policy(workspace.path(), &[])
        .unwrap_or_else(|err| panic!("{name}: the policy pass must build a setup: {err}"));

    let built = setup.profile.capabilities();
    assert_eq!(
        built.count(),
        1,
        "{name}: the capability list the policy built holds exactly one entry"
    );
    let strings = built
        .strings()
        .unwrap_or_else(|err| panic!("{name}: the capability SIDs must render as strings: {err}"));
    assert_eq!(
        strings,
        vec!["S-1-15-3-1".to_string()],
        "{name}: the capability the policy builds must be the documented `internetClient` SID"
    );
    // The list the policy built is the list the backend declares, not one the profile silently
    // dropped: the declared capability, built on its own, has to read the same way.
    let declared = CapabilitySids::internet_client()
        .unwrap_or_else(|err| panic!("{name}: the declared capability must build: {err}"));
    assert_eq!(
        declared.strings().unwrap_or_else(|err| {
            panic!("{name}: the declared capability must render as a string: {err}")
        }),
        strings,
        "{name}: the capability list of the profile must be the capability list the backend declares"
    );

    // The block the launch layer attaches to `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`.
    let block = setup.profile.security_capabilities();
    assert_eq!(
        block.CapabilityCount, 1,
        "{name}: the launch block must declare the one capability the profile was created with, got \
         {} capabilities",
        block.CapabilityCount
    );
    assert!(
        !block.Capabilities.is_null(),
        "{name}: the capability array the launch block points at must be the one the profile owns"
    );
    assert_eq!(
        block.AppContainerSid,
        setup.sid(),
        "{name}: the launch block must declare the AppContainer SID of this run's profile"
    );
}

/// The regression guard for the shape of the desktop failure: on a **healthy** fixture workspace the
/// attempt has to come back applied, and the command launched inside it has to run. This is the leg
/// that turns "every command costs [`SANDBOX_REFUSE_EXIT_CODE`]" into a failing test on a plain
/// desktop, because it asks for the verdict the dispatch asks for on every `run_command`, and then
/// runs a pending command through the composite entry point that verdict is decided on.
#[test]
fn test_windows_a_healthy_fixture_yields_an_applied_attempt_and_a_running_command() {
    let name = "test_windows_a_healthy_fixture_yields_an_applied_attempt_and_a_running_command";
    let workspace = tempfile::tempdir().expect("workspace tempdir");

    // 1. The verdict the dispatch asks the backend for. A verdict that is not *applied* is what costs
    //    a host every command it runs, because the re-entry contract refuses on it.
    let attempt = attempt_windows_sandbox(workspace.path(), &[]);
    if let SandboxAttempt::PartiallyEnforced { reason } = &attempt {
        eprintln!("NOTE {name}: the confinement this host produced is partial: {reason}");
    }
    assert!(
        matches!(
            attempt,
            SandboxAttempt::FullyEnforced | SandboxAttempt::PartiallyEnforced { .. }
        ),
        "{name}: a healthy fixture workspace must produce an applied sandbox attempt, got: {attempt:?}"
    );
    sandbox_outcome(&attempt).unwrap_or_else(|err| {
        panic!("{name}: the attempt must be accepted by the re-entry contract: {err}")
    });

    // 2. The command that verdict was made about: [`run_in_appcontainer`] picks the parked setup up —
    //    the confinement the attempt just judged — and launches the pending command inside it,
    //    releasing the guards only once the child is gone.
    let inside = workspace.path().join("runner-probe-marker");
    let code = run_in_appcontainer(
        workspace.path(),
        &[],
        workspace.path(),
        &write_payload(&inside),
    )
    .unwrap_or_else(|err| {
        panic!("{name}: the pending command must be launchable inside the attempt: {err}")
    });
    assert_eq!(
        code, 0,
        "{name}: the confined command must exit 0, got {code}"
    );
    let written = std::fs::read_to_string(&inside)
        .unwrap_or_else(|err| format!("cannot read the runner probe marker: {err}"));
    assert!(
        written.contains(WRITTEN_MARK),
        "{name}: the confined command must have written the probe marker it was asked to, got: {written}"
    );

    // 3. And the same on the command's own output, so a confined child that ran but could not write
    //    anywhere is not mistaken for a green run.
    let echo = run_in_appcontainer(
        workspace.path(),
        &[],
        workspace.path(),
        &format!("echo {RUNNER_MARK}"),
    )
    .unwrap_or_else(|err| panic!("{name}: a second confined command must be launchable: {err}"));
    assert_eq!(
        echo, 0,
        "{name}: the second confined command must exit 0, got {echo}"
    );
}
