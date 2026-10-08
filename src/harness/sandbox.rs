//! Landlock LSM sandbox for Linux process isolation.
//!
//! Restricts child processes (such as spawned PTY shells) to the workspace
//! root, /tmp, and standard build caches (~/.cargo, ~/.cache), while keeping
//! system toolchains (/usr, /bin, /lib, ~/.rustup) strictly read-only and
//! blocking all access to sensitive user directories (~/.ssh, ~/.gnupg, other projects).
//!
//! ## Fail-closed contract (t-052)
//!
//! [`apply_sandbox`] returns `Ok(())` **only when Landlock enforcement is actually in
//! effect for the calling process**. Every way an attempt can fall short is a
//! [`SandboxError`] that carries the reason:
//!
//! * [`SandboxError::Unavailable`] — Landlock cannot be used here: this build has no
//!   Landlock (non-Linux target), the running kernel has no Landlock LSM, the ruleset
//!   syscall was refused (`ENOSYS`/`EACCES`), or `restrict_self()` succeeded while the
//!   kernel still reported `NotEnforced` (nothing is restricted).
//! * [`SandboxError::RuleSetup`] — a ruleset exists but a rule could not be added, so
//!   the policy that was asked for cannot be built.
//! * [`SandboxError::Enforcement`] — a ruleset exists but enforcing it failed, so this
//!   process is **not** restricted.
//!
//! Before t-052 the "kernel has no Landlock" and "enforcement failed" cases only
//! logged a warning and returned `Ok(())`, and the re-entry path in `src/main.rs`
//! discarded the result — so a failed setup exec-ed the command completely
//! unsandboxed. `Ok`/`Err` is now the whole story, and [`reentry_action`] turns any
//! failure into a refusal to exec (stderr message + non-zero exit).
//!
//! The only legitimate way to run unsandboxed is still t-034b's explicit opt-out:
//! [`SANDBOX_OPTOUT_ENV`] parsed fail-closed by [`is_optout_value`] and surfaced by
//! [`opt_out_from_env`]. It is honoured *before* any enforcement attempt and is always
//! warned about ([`optout_warning`]).

use crate::harness::pty::{SANDBOX_OPTOUT_ENV, SandboxDecision, SandboxSkip, is_optout_value};
use std::path::Path;

#[cfg(target_os = "linux")]
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated,
    RulesetCreatedAttr, RulesetStatus,
};
#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// Exit code used when the sandbox re-entry path refuses to exec a command.
///
/// Non-zero by construction, and deliberately not `1` so a refusal is
/// distinguishable from a command that simply failed. It is `EX_UNAVAILABLE` from
/// `sysexits.h`: the security facility the command requires is not available.
pub const SANDBOX_REFUSE_EXIT_CODE: i32 = 73;

/// Why Landlock enforcement could not be established.
///
/// Every variant is terminal for the caller: the command must not be run. The
/// reason string is always non-empty and is what the caller logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// Landlock does not exist on this platform, or the kernel refused to create a
    /// ruleset, or it created one and still reports that nothing is enforced.
    #[error("Landlock is unavailable here: {reason}")]
    Unavailable { reason: String },
    /// A ruleset was created but the rule for `path` was rejected, so the requested
    /// policy is not the policy that would be enforced.
    #[error("Landlock policy could not be built for {path}: {reason}")]
    RuleSetup { path: String, reason: String },
    /// A ruleset was created but enforcing it failed: this process is **not**
    /// restricted at all.
    #[error("Landlock enforcement failed, this process is NOT restricted: {reason}")]
    Enforcement { reason: String },
}

impl SandboxError {
    /// Machine-readable class, for structured logging.
    pub fn kind(&self) -> &'static str {
        match self {
            SandboxError::Unavailable { .. } => "unavailable",
            SandboxError::RuleSetup { .. } => "rule-setup",
            SandboxError::Enforcement { .. } => "enforcement",
        }
    }

    /// The underlying reason, without the wrapper sentence.
    pub fn reason(&self) -> &str {
        match self {
            SandboxError::Unavailable { reason }
            | SandboxError::RuleSetup { reason, .. }
            | SandboxError::Enforcement { reason } => reason,
        }
    }
}

/// What one Landlock attempt actually achieved, step by step.
///
/// This is the injection seam of the fail-closed contract: [`sandbox_outcome`] maps it
/// to the public `Result`, and tests can feed every variant without a Landlock kernel,
/// a PTY, or a real `restrict_self()` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxAttempt {
    /// A ruleset was created, every rule was accepted, and the kernel reports full
    /// enforcement.
    FullyEnforced,
    /// As [`SandboxAttempt::FullyEnforced`], except that the running Landlock ABI covers
    /// only part of the requested rights. Restriction **is** in effect, but weaker than
    /// requested — which is exactly why the attempt site logs a warning about it instead
    /// of staying quiet.
    PartiallyEnforced { reason: String },
    /// `restrict_self()` returned `Ok`, yet the kernel reports that nothing is enforced:
    /// the platform has no usable Landlock.
    NotEnforced,
    /// Landlock cannot be used at all (non-Linux build, no Landlock LSM, or the ruleset
    /// syscall was refused).
    Unavailable { reason: String },
    /// A ruleset exists but the rule for `path` was rejected.
    RuleFailed { path: String, reason: String },
    /// A ruleset exists but enforcing it failed: nothing restricts this process.
    EnforcementFailed { reason: String },
}

/// The fail-closed mapping from what was achieved to what the caller may believe.
///
/// Pure and total: **only** an established ruleset is `Ok(())`. A partially enforced
/// ruleset is `Ok(())` (restriction is real, merely narrower than requested) and is
/// warned about by [`apply_sandbox`]; everything else is an error naming the reason.
#[must_use = "the sandbox outcome decides whether the command may run at all"]
pub fn sandbox_outcome(attempt: &SandboxAttempt) -> Result<(), SandboxError> {
    match attempt {
        SandboxAttempt::FullyEnforced => Ok(()),
        SandboxAttempt::PartiallyEnforced { .. } => Ok(()),
        SandboxAttempt::NotEnforced => Err(SandboxError::Unavailable {
            reason: "the kernel accepted the ruleset but reports that nothing is enforced"
                .to_string(),
        }),
        SandboxAttempt::Unavailable { reason } => Err(SandboxError::Unavailable {
            reason: reason.clone(),
        }),
        SandboxAttempt::RuleFailed { path, reason } => Err(SandboxError::RuleSetup {
            path: path.clone(),
            reason: reason.clone(),
        }),
        SandboxAttempt::EnforcementFailed { reason } => Err(SandboxError::Enforcement {
            reason: reason.clone(),
        }),
    }
}

/// Restrict the current process with Landlock, or report why that did not happen.
///
/// `Ok(())` means a Landlock ruleset **is enforcing on this process** (a partially
/// enforced one counts, and is warned about). Anything else is a [`SandboxError`] —
/// see the module docs for the exact contract.
///
/// This function deliberately does not consult the opt-out: *whether* a sandbox is
/// wanted is decided by the caller ([`crate::harness::pty::should_apply_sandbox`] on the
/// spawn side, [`reentry_action`] on the re-entry side). Keeping the decision out of
/// the enforcement step is what makes an enforcement failure impossible to mistake for
/// success.
pub fn apply_sandbox(workspace_root: &Path) -> Result<(), SandboxError> {
    let attempt = attempt_landlock(workspace_root);
    let outcome = sandbox_outcome(&attempt);

    match &outcome {
        Ok(()) => match attempt {
            SandboxAttempt::PartiallyEnforced { reason } => {
                tracing::warn!(
                    "Landlock is only PARTIALLY enforced for {}: {reason}",
                    workspace_root.display()
                );
            }
            _ => {
                tracing::debug!(
                    "Landlock sandbox established for {}",
                    workspace_root.display()
                );
            }
        },
        Err(err) => {
            tracing::error!(
                kind = err.kind(),
                workspace_root = %workspace_root.display(),
                "Landlock sandbox NOT established: {err}"
            );
        }
    }

    outcome
}

/// The operator's explicit opt-out, read from the environment with t-034b's
/// fail-closed parsing. Call sites must use this instead of re-reading the variable.
#[must_use]
pub fn opt_out_from_env() -> crate::harness::pty::OptOut {
    opt_out_from_value(std::env::var(SANDBOX_OPTOUT_ENV).ok().as_deref())
}

/// Pure half of [`opt_out_from_env`]: only an unambiguous truthy value opts out;
/// unset, empty, malformed or falsy-looking values keep the sandbox on.
#[must_use]
pub fn opt_out_from_value(raw: Option<&str>) -> crate::harness::pty::OptOut {
    match raw {
        Some(value) if is_optout_value(value) => crate::harness::pty::OptOut::Explicit {
            setting: format!("{SANDBOX_OPTOUT_ENV}={value}"),
        },
        _ => crate::harness::pty::OptOut::None,
    }
}

/// What the enforcement step achieved on the re-entry path, as reported back to
/// [`reentry_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enforcement {
    /// Not attempted, because the operator explicitly opted out (attributable
    /// `setting`, e.g. `MARMEL_DISABLE_SANDBOX=1`).
    Skipped { setting: String },
    /// A Landlock ruleset is enforcing on this process.
    Established,
    /// Attempted and failed — this process is **not** restricted.
    Failed { reason: String },
}

/// What the sandbox re-entry path must do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReentryAction {
    /// Exec the pending command: enforcement is established, or an explicit opt-out
    /// was honoured (and warned about).
    Exec,
    /// Refuse: print `reason` on stderr and exit with `exit_code`. Nothing is exec-ed.
    Refuse { reason: String, exit_code: i32 },
}

/// Decide the re-entry action from injected inputs — no exec, no Landlock, no
/// environment — so the fail-closed choice (exit over exec) is unit-testable.
///
/// The rules are the security contract of the re-entry protocol:
///
/// * exec only when the decision demanded Landlock **and** it was established, or when
///   the decision and the enforcement step agree on an explicit opt-out;
/// * everything else — a failed attempt, a decision that demands Landlock without an
///   established ruleset, a platform that cannot provide it, an unresolved executable —
///   is a refusal with a non-zero exit code.
#[must_use]
pub fn reentry_action(decision: &SandboxDecision, enforcement: &Enforcement) -> ReentryAction {
    match (decision, enforcement) {
        // 1. Explicit opt-out: allowed to run, but only when that is genuinely why
        //    nothing was enforced (the caller has already warned about it).
        (
            SandboxDecision::Skip {
                skip: SandboxSkip::OptOut { setting },
                ..
            },
            Enforcement::Skipped { setting: skipped },
        ) if setting == skipped => ReentryAction::Exec,

        // 2. Landlock was required and it is in effect.
        (SandboxDecision::Apply { .. }, Enforcement::Established) => ReentryAction::Exec,

        // 3. Everything else refuses to run the command.
        (_, Enforcement::Failed { reason }) => refuse(reason.clone()),
        (SandboxDecision::Apply { ..}, other) => refuse(format!(
            "the sandbox decision requires Landlock, but enforcement did not establish it ({other:?})"
        )),
        (SandboxDecision::Skip { skip, .. }, _) => refuse(format!(
            "sandbox re-entry was requested, but this host cannot apply Landlock ({})",
            skip.label()
        )),
        (SandboxDecision::UnresolvedExe, _) => refuse(
            "sandbox re-entry needs a resolved executable path; refusing to run the command unsandboxed"
                .to_string(),
        ),
    }
}

fn refuse(reason: String) -> ReentryAction {
    ReentryAction::Refuse {
        reason,
        exit_code: SANDBOX_REFUSE_EXIT_CODE,
    }
}

/// The stderr line printed when a refusal happens — kept here so the message is
/// identical everywhere and assertable in tests.
#[must_use]
pub fn refusal_message(reason: &str) -> String {
    format!(
        "error: refusing to run the command: {reason}. \
         Landlock sandboxing is enabled by default; set {SANDBOX_OPTOUT_ENV}=1 to run it unsandboxed."
    )
}

/// The warning printed when an explicit opt-out is honoured. Running unsandboxed must
/// never be silent, and on the re-entry path the logging subscriber may not exist yet,
/// so this also goes to stderr.
#[must_use]
pub fn optout_warning(setting: &str) -> String {
    format!("warning: Landlock sandbox DISABLED by {setting}: this command runs WITHOUT Landlock.")
}

#[cfg(target_os = "linux")]
fn attempt_landlock(workspace_root: &Path) -> SandboxAttempt {
    let abi = ABI::V5;

    let ruleset = match build_ruleset(workspace_root, abi) {
        Ok(ruleset) => ruleset,
        Err(failure) => return failure,
    };

    // 6. Enforce. `Ok` alone is *not* success: the kernel reports what it actually
    //    enforces, and a ruleset it declined to honour restricts nothing.
    match ruleset.restrict_self() {
        Ok(status) => match status.ruleset {
            RulesetStatus::FullyEnforced => SandboxAttempt::FullyEnforced,
            RulesetStatus::PartiallyEnforced => SandboxAttempt::PartiallyEnforced {
                reason: format!(
                    "the running kernel enforces only part of the requested Landlock ABI (status {status:?})"
                ),
            },
            RulesetStatus::NotEnforced => SandboxAttempt::NotEnforced,
        },
        Err(err) => SandboxAttempt::EnforcementFailed {
            reason: error_chain(&err),
        },
    }
}

/// Build the allow-list ruleset.
///
/// A rule that the policy **requires** is added with `?`: if it cannot be added the
/// whole attempt fails, because a partially built policy is not the policy the caller
/// asked for. The workspace root is such a rule — silently dropping it would enforce a
/// policy nobody chose. Rules for paths that simply do not exist on this host (no
/// `~/.npm`, no `/var/tmp`, …) stay optional: an absent allow-list entry widens nothing.
#[cfg(target_os = "linux")]
fn build_ruleset(workspace_root: &Path, abi: ABI) -> Result<RulesetCreated, SandboxAttempt> {
    let ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|err| SandboxAttempt::Unavailable {
            reason: format!("configuring Landlock access rights: {}", error_chain(&err)),
        })?
        .create()
        .map_err(|err| SandboxAttempt::Unavailable {
            reason: format!("creating Landlock ruleset: {}", error_chain(&err)),
        })?;

    // 1. Full Read/Write/Execute/Create/Delete rights for the workspace (required).
    let workspace_fd = PathFd::new(workspace_root).map_err(|err| SandboxAttempt::RuleFailed {
        path: workspace_root.display().to_string(),
        reason: format!("opening workspace root: {}", error_chain(&err)),
    })?;
    let mut ruleset = add_rule(
        ruleset,
        workspace_root,
        PathBeneath::new(workspace_fd, AccessFs::from_all(abi)),
    )?;

    // 2. Full Read/Write for /tmp and /var/tmp
    for tmp_dir in ["/tmp", "/var/tmp"] {
        if Path::new(tmp_dir).exists()
            && let Ok(fd) = PathFd::new(tmp_dir)
        {
            ruleset = add_rule(
                ruleset,
                Path::new(tmp_dir),
                PathBeneath::new(fd, AccessFs::from_all(abi)),
            )?;
        }
    }

    // 3. User build caches and toolchains in HOME
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        // Read/Write caches for build tools (cargo, pip, npm)
        for dir_name in [".cargo", ".cache", ".npm"] {
            let p = home.join(dir_name);
            if p.exists()
                && let Ok(fd) = PathFd::new(&p)
            {
                ruleset = add_rule(ruleset, &p, PathBeneath::new(fd, AccessFs::from_all(abi)))?;
            }
        }
        // Read-only user toolchains and configurations (rustup, local binaries, gitconfig, config)
        for entry in [".rustup", ".config", ".local", ".gitconfig"] {
            let p = home.join(entry);
            if p.exists()
                && let Ok(fd) = PathFd::new(&p)
            {
                ruleset = add_rule(ruleset, &p, PathBeneath::new(fd, AccessFs::from_read(abi)))?;
            }
        }
    }

    // Custom CARGO_HOME / RUSTUP_HOME if set outside ~/.cargo or ~/.rustup
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME").map(PathBuf::from)
        && cargo_home.exists()
        && let Ok(fd) = PathFd::new(&cargo_home)
    {
        ruleset = add_rule(
            ruleset,
            &cargo_home,
            PathBeneath::new(fd, AccessFs::from_all(abi)),
        )?;
    }
    if let Some(rustup_home) = std::env::var_os("RUSTUP_HOME").map(PathBuf::from)
        && rustup_home.exists()
        && let Ok(fd) = PathFd::new(&rustup_home)
    {
        ruleset = add_rule(
            ruleset,
            &rustup_home,
            PathBeneath::new(fd, AccessFs::from_read(abi)),
        )?;
    }

    // 4. Essential device nodes with read/write access (/dev/null, /dev/zero, /dev/full, /dev/tty, /dev/pts, /dev/shm)
    let rw_devs = [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/urandom",
        "/dev/random",
        "/dev/pts",
        "/dev/shm",
    ];
    for p in rw_devs {
        if Path::new(p).exists()
            && let Ok(fd) = PathFd::new(p)
        {
            ruleset = add_rule(
                ruleset,
                Path::new(p),
                PathBeneath::new(fd, AccessFs::from_all(abi)),
            )?;
        }
    }

    // 5. System toolchains, device nodes, runtime files (DNS /run/systemd/resolve), and binaries (Read-Only + Execute)
    let ro_paths = [
        "/usr", "/bin", "/lib", "/lib64", "/opt", "/etc", "/dev", "/proc", "/sys", "/run", "/var",
    ];
    for p in ro_paths {
        if Path::new(p).exists()
            && let Ok(fd) = PathFd::new(p)
        {
            ruleset = add_rule(
                ruleset,
                Path::new(p),
                PathBeneath::new(fd, AccessFs::from_read(abi)),
            )?;
        }
    }

    Ok(ruleset)
}

#[cfg(target_os = "linux")]
fn add_rule(
    ruleset: RulesetCreated,
    path: &Path,
    rule: PathBeneath<PathFd>,
) -> Result<RulesetCreated, SandboxAttempt> {
    ruleset
        .add_rule(rule)
        .map_err(|err| SandboxAttempt::RuleFailed {
            path: path.display().to_string(),
            reason: error_chain(&err),
        })
}

/// Render an error together with its full cause chain, so a [`SandboxError`] carries a
/// self-contained reason across the crate boundary.
#[cfg(target_os = "linux")]
fn error_chain<E: std::error::Error>(err: &E) -> String {
    let mut chain = err.to_string();
    let mut source = err.source();
    while let Some(next) = source {
        let reason = next.to_string();
        // landlock's wrappers already embed their source's message, so appending it
        // again would only repeat the same sentence in the operator-facing error.
        if !reason.trim().is_empty() && !chain.ends_with(&reason) {
            chain.push_str(": ");
            chain.push_str(&reason);
        }
        source = next.source();
    }
    chain
}

#[cfg(not(target_os = "linux"))]
fn attempt_landlock(_workspace_root: &Path) -> SandboxAttempt {
    // No Landlock exists on this platform, so the sandbox cannot be established at
    // all. That is reported as a failure: the caller must refuse rather than pretend
    // the command ran protected.
    SandboxAttempt::Unavailable {
        reason: format!("{} has no Landlock support", std::env::consts::OS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::pty::SANDBOX_EXEC_ARG;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Log capture: "an enforcement failure is never silent" is asserted, not
    // promised.
    // -----------------------------------------------------------------------
    thread_local! {
        static CAPTURED: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    #[derive(Clone, Copy, Default)]
    struct CaptureWriter;

    struct CaptureSink;

    impl std::io::Write for CaptureSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            CAPTURED.with(|lines| {
                lines
                    .borrow_mut()
                    .push(String::from_utf8_lossy(buf).into_owned())
            });
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CaptureWriter {
        type Writer = CaptureSink;
        fn make_writer(&self) -> CaptureSink {
            CaptureSink
        }
    }

    fn capture_logs<R>(f: impl FnOnce() -> R) -> (R, String) {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(CaptureWriter)
            .finish();
        CAPTURED.with(|lines| lines.borrow_mut().clear());
        let result = tracing::subscriber::with_default(subscriber, f);
        let text = CAPTURED.with(|lines| lines.borrow().join("\n"));
        (result, text)
    }

    // -----------------------------------------------------------------------
    // The error contract, driven by injected outcomes (no Landlock kernel, no
    // PTY, no `/dev/pts`).
    // -----------------------------------------------------------------------

    /// `Ok(())` is reserved for an established ruleset: every other attempt must be an
    /// error. This is the exact hole t-052 closes.
    #[test]
    fn test_sandbox_outcome_is_ok_only_when_enforcement_is_established() {
        assert!(
            sandbox_outcome(&SandboxAttempt::FullyEnforced).is_ok(),
            "full enforcement must report success"
        );
        assert!(
            sandbox_outcome(&SandboxAttempt::PartiallyEnforced {
                reason: "older Landlock ABI".to_string()
            })
            .is_ok(),
            "partial enforcement is still enforcement (and is warned about)"
        );

        for attempt in [
            SandboxAttempt::NotEnforced,
            SandboxAttempt::Unavailable {
                reason: "NoFileOrNotSupported".to_string(),
            },
            SandboxAttempt::RuleFailed {
                path: "/gone/workspace".to_string(),
                reason: "ENOENT".to_string(),
            },
            SandboxAttempt::EnforcementFailed {
                reason: "RestrictSelfCall: EACCES".to_string(),
            },
        ] {
            let err = sandbox_outcome(&attempt)
                .expect_err("a shortfall in enforcement must never report success");
            assert!(
                !err.reason().is_empty(),
                "{attempt:?} must carry a reason, got {err}"
            );
        }
    }

    /// A kernel without Landlock (ruleset creation refused, or `Ok` + `NotEnforced`)
    /// is an `Unavailable` error, not a warning followed by `Ok(())`.
    #[test]
    fn test_sandbox_outcome_reports_kernel_unavailability_as_err() {
        let err = sandbox_outcome(&SandboxAttempt::Unavailable {
            reason: "creating Landlock ruleset: CreateRulesetCall: ENOSYS".to_string(),
        })
        .expect_err("a kernel without Landlock must fail the attempt");
        assert!(matches!(err, SandboxError::Unavailable { .. }));
        assert_eq!(err.kind(), "unavailable");
        assert!(
            err.to_string().contains("ENOSYS"),
            "reason must survive: {err}"
        );

        let err = sandbox_outcome(&SandboxAttempt::NotEnforced)
            .expect_err("`Ok` + NotEnforced means nothing is restricted");
        assert!(matches!(err, SandboxError::Unavailable { .. }));
        assert!(err.to_string().contains("nothing is enforced"), "{err}");
    }

    /// Enforcement that fails outright is reported as `Enforcement`, so the caller can
    /// tell it apart from "no Landlock here".
    #[test]
    fn test_sandbox_outcome_reports_enforcement_failure_as_err() {
        let err = sandbox_outcome(&SandboxAttempt::EnforcementFailed {
            reason: "restrict_self: SetNoNewPrivsCall".to_string(),
        })
        .expect_err("an unenforced process must not look sandboxed");
        assert!(matches!(err, SandboxError::Enforcement { .. }));
        assert_eq!(err.kind(), "enforcement");
        assert!(err.to_string().contains("NOT restricted"), "{err}");
    }

    /// A rejected rule means the policy is not the one requested: `RuleSetup`, naming
    /// the path.
    #[test]
    fn test_sandbox_outcome_reports_rule_failure_as_err_with_path() {
        let err = sandbox_outcome(&SandboxAttempt::RuleFailed {
            path: "/tmp/missing-root".to_string(),
            reason: "ENOENT".to_string(),
        })
        .expect_err("a policy that cannot be built must fail");
        assert!(matches!(err, SandboxError::RuleSetup { .. }));
        assert_eq!(err.kind(), "rule-setup");
        assert!(err.to_string().contains("/tmp/missing-root"), "{err}");
    }

    /// End-to-end on the real code path, without restricting the test process: an
    /// unusable workspace root must produce an `Err` (before t-052 it produced
    /// `Ok(())` after a warning, and the caller then exec-ed the command unsandboxed).
    ///
    /// Whether the host reports "no Landlock here" or "this rule could not be built"
    /// depends on the kernel; either way it must be an error with a reason, and it must
    /// be logged rather than swallowed.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_apply_sandbox_fails_closed_on_an_unusable_workspace_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("never-created");

        let (outcome, logs) = capture_logs(|| apply_sandbox(&missing));
        let err = outcome.expect_err("an unusable workspace root must not report success");
        assert!(
            matches!(
                err,
                SandboxError::RuleSetup { .. } | SandboxError::Unavailable { .. }
            ),
            "unexpected failure class: {err:?}"
        );
        assert!(
            !err.reason().is_empty(),
            "the reason must not be empty: {err}"
        );
        assert!(
            logs.contains("Landlock sandbox NOT established"),
            "an enforcement failure must be logged, got {logs:?}"
        );
    }

    // -----------------------------------------------------------------------
    // The re-entry decision: exit over exec.
    // -----------------------------------------------------------------------

    fn apply_decision() -> SandboxDecision {
        SandboxDecision::Apply {
            resolved_exe: PathBuf::from("/opt/libexec/sandbox-host-build-7"),
        }
    }

    /// A setup/enforcement failure means the re-entry path must exit instead of exec.
    #[test]
    fn test_reentry_refuses_exec_when_enforcement_failed() {
        let action = reentry_action(
            &apply_decision(),
            &Enforcement::Failed {
                reason: "Landlock is unavailable here: ENOSYS".to_string(),
            },
        );
        let ReentryAction::Refuse { reason, exit_code } = action else {
            panic!("a failed enforcement must refuse, got {action:?}");
        };
        assert_ne!(exit_code, 0, "a refusal must exit non-zero");
        assert_eq!(exit_code, SANDBOX_REFUSE_EXIT_CODE);
        assert!(reason.contains("ENOSYS"), "{reason}");

        let message = refusal_message(&reason);
        assert!(
            message.contains(SANDBOX_OPTOUT_ENV) && message.contains("refusing"),
            "the refusal must name the only way out: {message}"
        );
    }

    /// Exec happens in exactly two situations: established enforcement, or an
    /// attributable opt-out that the enforcement step agrees with.
    #[test]
    fn test_reentry_execs_only_when_established_or_consistently_opted_out() {
        assert_eq!(
            reentry_action(&apply_decision(), &Enforcement::Established),
            ReentryAction::Exec
        );

        let setting = format!("{SANDBOX_OPTOUT_ENV}=1");
        let decision = SandboxDecision::Skip {
            resolved_exe: Some(PathBuf::from("/opt/libexec/sandbox-host-build-7")),
            skip: SandboxSkip::OptOut {
                setting: setting.clone(),
            },
        };
        assert_eq!(
            reentry_action(&decision, &Enforcement::Skipped { setting }),
            ReentryAction::Exec
        );
    }

    /// Any disagreement between the decision and the enforcement step is a refusal:
    /// Landlock demanded but not established, opt-out claimed with a different
    /// setting, a platform that cannot provide Landlock, or an unresolved exe.
    #[test]
    fn test_reentry_refuses_on_any_inconsistency() {
        let cases = [
            (
                apply_decision(),
                Enforcement::Skipped {
                    setting: format!("{SANDBOX_OPTOUT_ENV}=1"),
                },
            ),
            (
                SandboxDecision::Skip {
                    resolved_exe: Some(PathBuf::from("/opt/libexec/mm")),
                    skip: SandboxSkip::OptOut {
                        setting: format!("{SANDBOX_OPTOUT_ENV}=yes"),
                    },
                },
                Enforcement::Skipped {
                    setting: format!("{SANDBOX_OPTOUT_ENV}=nope"),
                },
            ),
            (
                SandboxDecision::Skip {
                    resolved_exe: Some(PathBuf::from("/opt/libexec/mm")),
                    skip: SandboxSkip::UnsupportedPlatform,
                },
                Enforcement::Established,
            ),
            (
                SandboxDecision::Skip {
                    resolved_exe: None,
                    skip: SandboxSkip::TestHarnessExe,
                },
                Enforcement::Skipped {
                    setting: "n/a".to_string(),
                },
            ),
            (SandboxDecision::UnresolvedExe, Enforcement::Established),
        ];

        for (decision, enforcement) in cases {
            let action = reentry_action(&decision, &enforcement);
            let ReentryAction::Refuse { reason, exit_code } = action else {
                panic!("must refuse for {decision:?} + {enforcement:?}, got {action:?}");
            };
            assert_ne!(exit_code, 0, "{reason}");
            assert!(!reason.is_empty(), "a refusal must explain itself");
        }
    }

    /// Opt-out parsing stays fail-closed: only an unambiguous truthy value opts out.
    #[test]
    fn test_opt_out_parsing_is_fail_closed() {
        for truthy in ["1", "true", "TRUE", " yes ", "on"] {
            let opt = opt_out_from_value(Some(truthy));
            assert!(
                matches!(opt, crate::harness::pty::OptOut::Explicit { .. }),
                "{truthy} must opt out"
            );
        }
        for falsy in [
            Some("0"),
            Some("false"),
            Some("off"),
            Some(""),
            Some("maybe"),
            None,
        ] {
            assert_eq!(
                opt_out_from_value(falsy),
                crate::harness::pty::OptOut::None,
                "{falsy:?} must NOT opt out"
            );
        }
        assert_eq!(
            opt_out_from_value(Some("1")),
            crate::harness::pty::OptOut::Explicit {
                setting: format!("{SANDBOX_OPTOUT_ENV}=1")
            },
            "the opt-out must stay attributable to the setting that produced it"
        );
    }

    // -----------------------------------------------------------------------
    // Source guards.
    // -----------------------------------------------------------------------

    /// The re-entry path in `main.rs` must never discard the enforcement result and
    /// must never re-spell the protocol literal: both were the fail-open in t-052.
    #[test]
    fn test_main_rs_never_ignores_the_sandbox_result() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("main.rs must be readable from the crate manifest dir");

        assert!(
            !src.contains("let _ = harness::sandbox::apply_sandbox"),
            "the enforcement result must never be discarded"
        );
        assert!(
            !src.contains("drop(harness::sandbox::apply_sandbox"),
            "the enforcement result must never be dropped either"
        );
        assert!(
            src.contains("harness::sandbox::apply_sandbox"),
            "the re-entry path must still apply the sandbox"
        );
        assert!(
            src.contains("ReentryAction::Refuse") && src.contains("SANDBOX_REFUSE_EXIT_CODE"),
            "the re-entry path must exit instead of exec-ing when refused"
        );

        // Order matters: the opt-out must be honoured *before* enforcement is attempted.
        let opt_out_at = src
            .find("opt_out_from_env")
            .expect("main.rs must read the opt-out through the shared parser");
        let enforce_at = src
            .find("harness::sandbox::apply_sandbox")
            .expect("main.rs must attempt enforcement");
        assert!(
            opt_out_at < enforce_at,
            "the opt-out must be resolved before enforcement is attempted"
        );
    }

    /// `main.rs` must compare against the shared constant, not a private copy of it.
    /// The needle is assembled from parts so this file never contains it either.
    #[test]
    fn test_main_rs_uses_the_shared_sandbox_arg_constant() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("main.rs must be readable from the crate manifest dir");
        let protocol_literal = format!("--{}sandbox-exec", "internal-");

        assert!(
            !src.contains(&protocol_literal),
            "main.rs must not re-spell the re-entry sentinel; use SANDBOX_EXEC_ARG"
        );
        assert!(
            src.contains("SANDBOX_EXEC_ARG"),
            "main.rs must reference the shared SANDBOX_EXEC_ARG constant"
        );
        assert!(
            src.contains("canonicalize"),
            "the re-entry cwd must be derived canonically"
        );
    }

    /// The Landlock integration tests must not rebuild a binary path from a hard-coded
    /// executable name — the same class of assumption t-034b removed from `pty.rs`.
    #[test]
    fn test_sandbox_tests_never_rebuild_the_binary_path_from_a_name() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/harness/sandbox.rs"
        ))
        .expect("sandbox.rs must be readable from the crate manifest dir");

        // Built from parts so this source never contains the pattern it forbids.
        let quote = char::from(34);
        let needle = format!(".join({quote}{}{quote})", "marmel");
        assert!(
            !src.contains(&needle),
            "integration tests must locate the re-entry host by behaviour, not by name"
        );
        assert!(
            src.contains("discover_reentry_exe") && src.contains("implements_reentry"),
            "the re-entry host must be discovered by probing behaviour"
        );
        assert!(
            src.contains("SKIP "),
            "a skipped integration test must announce itself as a skip"
        );
    }

    // -----------------------------------------------------------------------
    // Landlock integration: the real re-entry protocol, driven end to end.
    // -----------------------------------------------------------------------

    /// Locate the executable that implements the re-entry protocol **without any
    /// assumption about its file name or layout** (renaming the bin target, a hashed
    /// deps name, or a different profile directory must all keep working).
    #[cfg(target_os = "linux")]
    fn reentry_exe() -> Option<&'static Path> {
        static CACHED: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        CACHED.get_or_init(discover_reentry_exe).as_deref()
    }

    /// Cargo builds this package's bin targets one level above the `deps/` directory
    /// that holds the running test binary. Candidates are accepted only on behaviour
    /// ([`implements_reentry`]), never on their name, and only when they are at least as
    /// new as the newest source file (a stale bin from a previous `cargo build` would
    /// otherwise exercise the very behaviour these tests are trying to catch).
    #[cfg(target_os = "linux")]
    fn discover_reentry_exe() -> Option<PathBuf> {
        let test_exe = std::env::current_exe().ok()?;
        let profile_dir = test_exe.parent()?.parent()?;

        let mut candidates: Vec<PathBuf> = std::fs::read_dir(profile_dir)
            .ok()?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path != &test_exe && path.is_file() && is_executable(path))
            .filter(|path| candidate_is_current(path))
            .collect();
        candidates.sort();
        candidates
            .into_iter()
            .find(|candidate| implements_reentry(candidate))
    }

    #[cfg(target_os = "linux")]
    fn is_executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
    }

    /// Best-effort freshness check: a candidate binary is only trusted when its mtime is
    /// not older than the newest `.rs` file in the package. Unknown metadata means "not
    /// current", which keeps the probe conservative rather than optimistic.
    #[cfg(target_os = "linux")]
    fn candidate_is_current(candidate: &Path) -> bool {
        let Ok(candidate_time) = candidate.metadata().and_then(|meta| meta.modified()) else {
            return false;
        };
        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut newest: Option<std::time::SystemTime> = None;
        let mut pending = vec![crate_root];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(|entry| entry.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                if !path.extension().is_none_or(|ext| ext == "rs") {
                    continue;
                }
                let Ok(mtime) = path.metadata().and_then(|meta| meta.modified()) else {
                    continue;
                };
                if newest.is_none_or(|seen| mtime > seen) {
                    newest = Some(mtime);
                }
            }
        }
        newest.is_none_or(|mtime| candidate_time >= mtime)
    }

    /// Behavioural probe: run the candidate with the sentinel arg (plus an explicit
    /// opt-out, so the probe does not depend on kernel Landlock) and require it to
    /// exec the given command.
    #[cfg(target_os = "linux")]
    fn implements_reentry(exe: &Path) -> bool {
        let Ok(tmp) = tempfile::tempdir() else {
            return false;
        };
        let marker = "reentry-probe-marker";
        std::process::Command::new(exe)
            .arg(SANDBOX_EXEC_ARG)
            .arg(tmp.path())
            .arg(format!("printf %s {marker}"))
            .env(SANDBOX_OPTOUT_ENV, "1")
            .output()
            .is_ok_and(|out| out.status.success() && out.stdout == marker.as_bytes())
    }

    /// The operator-facing error text must state the real cause exactly once — the
    /// refusal message is the only thing the user sees when the sandbox refuses.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_error_chain_names_the_cause_once() {
        let err = PathFd::new("/definitely-not-a-workspace-root").unwrap_err();
        let chain = error_chain(&err);
        assert!(chain.contains("No such file or directory"), "{chain}");
        assert_eq!(
            chain.matches("No such file or directory").count(),
            1,
            "the underlying cause must not be repeated: {chain}"
        );
    }

    /// Does this kernel have usable Landlock? Probed once, cheaply, and without
    /// restricting anything (the ruleset is created and dropped, never enforced).
    #[cfg(target_os = "linux")]
    fn kernel_has_landlock() -> bool {
        static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CACHED.get_or_init(|| {
            Ruleset::default()
                .handle_access(AccessFs::from_all(ABI::V5))
                .is_ok_and(|ruleset| ruleset.create().is_ok())
        })
    }
    /// Integration tests need a re-entry host and (when they assert enforced
    /// behaviour) a kernel with Landlock. Either way a missing prerequisite is printed
    /// as `SKIP` — a skipped integration test must never look like a passing one.
    #[cfg(target_os = "linux")]
    fn integration_host(test: &str, needs_landlock: bool) -> Option<PathBuf> {
        let Some(exe) = reentry_exe() else {
            eprintln!(
                "SKIP {test}: no current executable implementing {SANDBOX_EXEC_ARG} was found next to the test binary (run `cargo build` first, `cargo test --lib` does not rebuild bin targets)"
            );
            return None;
        };
        if needs_landlock && !kernel_has_landlock() {
            eprintln!(
                "SKIP {test}: the running kernel has no usable Landlock, so enforced behaviour cannot be checked here"
            );
            return None;
        }
        Some(exe.to_path_buf())
    }

    /// Spawn the re-entry host with an explicit opt-out: the command must run, and the
    /// opt-out must be visible on stderr. Needs no Landlock, so it never skips.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_internal_sandbox_exec_honours_explicit_opt_out_before_enforcement() {
        let name = "test_internal_sandbox_exec_honours_explicit_opt_out_before_enforcement";
        let Some(exe) = integration_host(name, false) else {
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");

        let out = std::process::Command::new(&exe)
            .arg(SANDBOX_EXEC_ARG)
            .arg(tmp.path())
            .arg("echo opted-out")
            .env(SANDBOX_OPTOUT_ENV, "1")
            .output()
            .expect("re-entry host must be runnable");

        assert!(
            out.status.success(),
            "an explicit opt-out must still run the command, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "opted-out",
            "the command must have been exec-ed"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(SANDBOX_OPTOUT_ENV) && stderr.contains("WITHOUT Landlock"),
            "an honoured opt-out must be warned about, got {stderr:?}"
        );
    }

    /// The fail-open regression guard: when the policy cannot be established (here a
    /// workspace root that does not exist), the host must exit non-zero with an
    /// explanation and must **not** run the command.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_internal_sandbox_exec_fails_closed_when_enforcement_is_impossible() {
        let name = "test_internal_sandbox_exec_fails_closed_when_enforcement_is_impossible";
        let Some(exe) = integration_host(name, false) else {
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing_root = tmp.path().join("never-created");

        let out = std::process::Command::new(&exe)
            .arg(SANDBOX_EXEC_ARG)
            .arg(&missing_root)
            .arg("echo MUST-NOT-RUN")
            .env_remove(SANDBOX_OPTOUT_ENV)
            .output()
            .expect("re-entry host must be runnable");

        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("MUST-NOT-RUN"),
            "the command must never be exec-ed when the sandbox cannot be established, got {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            !out.status.success(),
            "a failed sandbox setup must exit non-zero"
        );
        assert_eq!(
            out.status.code(),
            Some(SANDBOX_REFUSE_EXIT_CODE),
            "the refusal must be distinguishable by exit code, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("refusing") && stderr.contains(SANDBOX_OPTOUT_ENV),
            "the refusal must explain itself and name the only way out, got {stderr:?}"
        );
    }

    /// A falsy-looking opt-out value is not an opt-out (fail-closed parsing), even
    /// end to end: the command is still refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_internal_sandbox_exec_ignores_a_falsy_opt_out_value() {
        let name = "test_internal_sandbox_exec_ignores_a_falsy_opt_out_value";
        let Some(exe) = integration_host(name, false) else {
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing_root = tmp.path().join("never-created");

        let out = std::process::Command::new(&exe)
            .arg(SANDBOX_EXEC_ARG)
            .arg(&missing_root)
            .arg("echo MUST-NOT-RUN")
            .env(SANDBOX_OPTOUT_ENV, "false")
            .output()
            .expect("re-entry host must be runnable");

        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("MUST-NOT-RUN"),
            "`{SANDBOX_OPTOUT_ENV}=false` must not disable the sandbox, got {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert_eq!(
            out.status.code(),
            Some(SANDBOX_REFUSE_EXIT_CODE),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The allow-list itself still works: inside a real sandbox a shell may write to
    /// the workspace and `/dev/null` and read DNS config.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_internal_sandbox_exec_dev_null_and_dns() {
        let name = "test_internal_sandbox_exec_dev_null_and_dns";
        let Some(exe) = integration_host(name, true) else {
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");

        let status = std::process::Command::new(&exe)
            .arg(SANDBOX_EXEC_ARG)
            .arg(tmp.path())
            .arg("echo hello > /dev/null && cat /etc/resolv.conf > /dev/null")
            .env_remove(SANDBOX_OPTOUT_ENV)
            .status()
            .expect("re-entry host must be runnable");

        assert!(
            status.success(),
            "{SANDBOX_EXEC_ARG} must succeed writing to /dev/null and reading /etc/resolv.conf"
        );
    }

    /// Cross-directory rename inside the sandbox must work natively (no EXDEV).
    #[cfg(target_os = "linux")]
    #[test]
    fn test_internal_sandbox_cross_directory_rename() {
        let name = "test_internal_sandbox_cross_directory_rename";
        let Some(exe) = integration_host(name, true) else {
            return;
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let d1 = tmp.path().join("d1");
        let d2 = tmp.path().join("d2");
        std::fs::create_dir_all(&d1).expect("create d1");
        std::fs::create_dir_all(&d2).expect("create d2");
        std::fs::write(d1.join("test.txt"), "rename test payload").expect("write payload");

        // Direct rename syscall via python3 to ensure kernel rename() succeeds without EXDEV
        let script = format!(
            "import os; os.rename('{}/d1/test.txt', '{}/d2/test.txt')",
            tmp.path().display(),
            tmp.path().display()
        );
        let cmd = if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
        {
            format!("python3 -c \"{script}\"")
        } else {
            format!(
                "mv '{}/d1/test.txt' '{}/d2/test.txt'",
                tmp.path().display(),
                tmp.path().display()
            )
        };

        let status = std::process::Command::new(&exe)
            .arg(SANDBOX_EXEC_ARG)
            .arg(tmp.path())
            .arg(cmd)
            .env_remove(SANDBOX_OPTOUT_ENV)
            .status()
            .expect("re-entry host must be runnable");

        assert!(
            status.success(),
            "cross-directory rename inside the sandbox must succeed natively without EXDEV"
        );
        assert!(
            d2.join("test.txt").exists(),
            "renamed file must exist at destination"
        );
        assert!(
            !d1.join("test.txt").exists(),
            "original file must no longer exist in source"
        );
    }

    /// Ruleset construction with device nodes must still build (skips visibly when the
    /// kernel has no Landlock).
    #[cfg(target_os = "linux")]
    #[test]
    fn test_landlock_ruleset_builds_with_devices() {
        if !kernel_has_landlock() {
            eprintln!(
                "SKIP test_landlock_ruleset_builds_with_devices: the running kernel has no usable Landlock"
            );
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let abi = ABI::V5;
        let mut ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .expect("handle_access")
            .create()
            .expect("create");
        for dev in ["/dev/null", "/dev/zero", "/dev/tty"] {
            if let Ok(fd) = PathFd::new(dev) {
                ruleset = add_rule(
                    ruleset,
                    Path::new(dev),
                    PathBeneath::new(fd, AccessFs::from_all(abi)),
                )
                .expect("adding a device rule must succeed");
            }
        }
        let fd = PathFd::new(tmp.path()).expect("the tempdir must be openable");
        ruleset = add_rule(
            ruleset,
            tmp.path(),
            PathBeneath::new(fd, AccessFs::from_all(abi)),
        )
        .expect("adding the tempdir rule must succeed");
        // Nothing was ever enforced, so dropping the ruleset leaves the test process
        // unrestricted.
        drop(ruleset);
    }
}
