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
//!    turned into `<comspec> /C cmd`.
//!
//! Fixtures: the fixture a denial is proved **in** never sits under a temporary directory, because
//! the Windows policy grants those read-write on its own and a denial there would prove nothing —
//! the helper that picks such a fixture prints `SKIP <test>: …` when it cannot. The confined
//! process is always a **subprocess**: confining the test process itself would confine the harness
//! for the rest of the run, so confinement is applied inside the re-entry child. Nothing is ever
//! written into the real user profile: `%USERPROFILE%` is probed by reading it only.
//!
//! One Windows-specific asymmetry with the macOS file matters below: an explicit opt-out read by
//! the **re-entry child** still launches the pending command inside an AppContainer (the confined
//! launch in `src/main.rs` is where every Windows payload is run), so `MARMEL_DISABLE_SANDBOX=1`
//! cannot be the attribution leg here. The attribution leg is the same payload run by `cmd.exe`
//! straight from the test process, which is the only leg this file runs with no AppContainer at
//! all. Both kinds are asserted, and each says which one it is.

// A test file whose every item is Windows-only must still compile cleanly on Linux, where it holds
// nothing: the crate-wide `cfg` below is what makes that true rather than an empty `mod`.
#![cfg(target_os = "windows")]

use marmennill::harness::pty::{SANDBOX_EXEC_ARG, SANDBOX_OPTOUT_ENV};
use marmennill::harness::sandbox::{SANDBOX_EXTRA_RO_PATHS_ENV, SANDBOX_REFUSE_EXIT_CODE};
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
    let combined = format!("{} {}", stdout_of(out), stderr_of(out)).to_lowercase();
    combined.contains("access is denied")
        || combined.contains("denied")
        || combined.contains("cannot create")
        || combined.contains("cannot find the path")
        || combined.contains("file not found")
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
/// and the opt-out leg, which on Windows documents the opt-out rather than attributing the denial.
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

    // 3. The opt-out leg. On Windows the re-entry child still runs the pending command through the
    //    confined launch, so this leg does not undo leg 1: what it asserts is that the opt-out is
    //    honoured at the re-entry level and named out loud, backend and all.
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
