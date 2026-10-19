//! Landlock LSM sandbox for Linux process isolation.
//!
//! Restricts child processes (such as spawned PTY shells) to the workspace root, /tmp and standard build
//! caches (~/.cargo, ~/.cache); system toolchains (/usr, /bin, /lib, ~/.rustup) and the package stores
//! (/nix/store, /gnu/store) stay strictly read-only, and sensitive directories (~/.ssh, ~/.gnupg, other
//! projects) are blocked outright. The stores are named explicitly because Landlock resolves symlinks before
//! it matches a rule: where /bin and /usr are symlink farms, a rule on /bin does not reach the target under
//! the store.
//!
//! On top of that built-in allow-list the operator can widen the **read-only** part of it with the
//! `sandbox_extra_readonly_paths` config key: the spawning process publishes the list in
//! [`SANDBOX_EXTRA_RO_PATHS_ENV`], and the re-entry child merges it into the same read+execute
//! grant as `/usr` and friends ([`apply_sandbox`], [`apply_sandbox_with_extra_readonly_paths`]).
//! An extra can only ever *add* read-only rights: entries that are relative, absent on this host,
//! or already covered by a built-in rule never become rules at all, and each such entry is
//! announced in one line on stderr ([`extra_path_skip_note`]).
//!
//! [`apply_sandbox`] returns `Ok(())` **only when Landlock enforcement is actually in effect for
//! the calling process**; every way an attempt falls short is a [`SandboxError`] naming the reason —
//! [`SandboxError::Unavailable`] (no Landlock on this build/kernel, a refused ruleset syscall, or
//! `restrict_self()` succeeding while the kernel still reports `NotEnforced`),
//! [`SandboxError::RuleSetup`] (a rule could not be added, so the requested policy cannot be built)
//! or [`SandboxError::Enforcement`] (a ruleset exists but enforcing it failed, so this process is
//! **not** restricted). "The kernel has no Landlock" and "enforcement failed" are hard errors, never
//! warnings, and [`reentry_action`] turns any failure into a refusal to exec: a stderr message plus a
//! non-zero exit.
//!
//! The only legitimate way to run unsandboxed is the operator's explicit opt-out
//! ([`SANDBOX_OPTOUT_ENV`], parsed fail-closed by [`is_optout_value`] and surfaced by
//! [`opt_out_from_env`]), honoured *before* any enforcement attempt and always warned
//! about ([`optout_warning`]).

use crate::harness::pty::{SANDBOX_OPTOUT_ENV, SandboxDecision, SandboxSkip, is_optout_value};
use std::collections::BTreeSet;
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
    /// Landlock does not exist on this platform, or creating a ruleset was rejected, or
    /// one was created and the kernel still reports that nothing is enforced.
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
/// `Ok(())` means a Landlock ruleset **is enforcing on this process** (a partially enforced one
/// counts, and is warned about). Anything else is a [`SandboxError`] — see the module docs for the
/// exact contract. This function deliberately does not consult the opt-out: *whether* a sandbox is
/// wanted is decided by the caller ([`crate::harness::pty::should_apply_sandbox`] on the spawn side,
/// [`reentry_action`] on the re-entry side), which is what makes an enforcement failure impossible
/// to mistake for success.
///
/// The operator's extra read-only paths are read here through [`extra_readonly_paths_from_env`]:
/// the sandbox is only ever established in the re-entry child, which gets them through the
/// environment. A caller that already parsed them uses [`apply_sandbox_with_extra_readonly_paths`].
pub fn apply_sandbox(workspace_root: &Path) -> Result<(), SandboxError> {
    apply_sandbox_with_extra_readonly_paths(workspace_root, &extra_readonly_paths_from_env())
}

/// [`apply_sandbox`] with the extra read-only paths handed in instead of read back from the
/// environment, so the re-entry child reads the variable exactly once.
///
/// The contract is identical to [`apply_sandbox`]. Extras are merged into the read-only grant of
/// the built-in FHS entries — they can add read+execute rights and nothing else — and an entry
/// that cannot become a rule (relative, absent on this host, or already covered by a built-in
/// entry) is announced in one stderr line rather than silently dropped, because no logging
/// subscriber exists on the re-entry path yet. A rule the kernel refuses for an extra keeps the
/// fail-closed behaviour of every other required rule: [`SandboxError::RuleSetup`], never a
/// partially built policy that would still let the command run.
pub fn apply_sandbox_with_extra_readonly_paths(
    workspace_root: &Path,
    extra_readonly_paths: &[String],
) -> Result<(), SandboxError> {
    let (granted, skipped) =
        extra_readonly_rule_paths(extra_readonly_paths, BUILTIN_READONLY_PATHS);
    for (path, skip) in &skipped {
        eprintln!("{}", extra_path_skip_note(path, *skip));
    }
    if !granted.is_empty() {
        tracing::debug!(
            count = granted.len(),
            "Landlock: extending the read-only allow-list with operator-configured paths"
        );
    }

    let attempt = attempt_landlock(workspace_root, &granted);
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

/// The operator's explicit opt-out, read from the environment with fail-closed parsing.
/// Call sites must use this instead of re-reading the variable.
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

/// The environment channel carrying the operator's extra read-only sandbox paths
/// from the spawning process to the re-entry child, which never loads a config file.
///
/// The value is the configured list joined by `SANDBOX_EXTRA_RO_PATHS_SEP` in the
/// configured order, published by `crate::harness::pty::build_sandboxed_command` and
/// read back by [`extra_readonly_paths_from_env`]. Unset or empty means "no
/// additions", so a run without the key sees no variable at all. The argv contract is
/// deliberately untouched: it stays the sentinel, the cwd and the wrapped command,
/// because a new slot would change the arity both ends of the protocol depend on.
/// Note the separator limitation: a configured path that itself contains `:` cannot
/// be transported intact — it arrives split into two entries.
pub const SANDBOX_EXTRA_RO_PATHS_ENV: &str = "MARMEL_SANDBOX_EXTRA_RO_PATHS";

/// Separator between the entries of [`SANDBOX_EXTRA_RO_PATHS_ENV`]: the same one
/// `PATH` uses on every platform this sandbox runs on.
const SANDBOX_EXTRA_RO_PATHS_SEP: &str = ":";

/// The extra read-only paths published by the spawning process, read from the
/// environment. Call sites must use this instead of re-reading the variable.
#[must_use]
pub fn extra_readonly_paths_from_env() -> Vec<String> {
    extra_readonly_paths_from_value(std::env::var(SANDBOX_EXTRA_RO_PATHS_ENV).ok().as_deref())
}

/// Pure half of [`extra_readonly_paths_from_env`]: split on
/// `SANDBOX_EXTRA_RO_PATHS_SEP`, trim each entry and drop the empty ones, keeping
/// the configured order. Unset, empty or a value made only of separators and blanks
/// yields no paths at all.
#[must_use]
pub fn extra_readonly_paths_from_value(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(SANDBOX_EXTRA_RO_PATHS_SEP)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

/// Pure half of the publish side, the inverse of [`extra_readonly_paths_from_value`]:
/// the value to publish for `paths`, trimmed and with empty entries dropped, or `None`
/// when there is nothing to publish — the common case, in which the variable must stay
/// unset so the child behaves exactly as it did before this channel existed.
#[must_use]
pub fn extra_readonly_paths_to_env(paths: &[String]) -> Option<String> {
    let joined = paths
        .iter()
        .map(|path| path.trim())
        .filter(|path| !path.is_empty())
        .collect::<Vec<&str>>()
        .join(SANDBOX_EXTRA_RO_PATHS_SEP);
    (!joined.is_empty()).then_some(joined)
}

/// The built-in read-only allow-list: system toolchains, device nodes, runtime files (the DNS
/// socket under `/run/systemd/resolve`), binaries, and the package stores (Nix `/nix/store`, Guix
/// `/gnu/store`).
///
/// The stores are listed explicitly because Landlock resolves symlinks before it matches a rule,
/// and these are `PathBeneath` rules anchored on an opened directory fd: on a host where `/bin`
/// and `/usr` are symlink farms a rule on `/bin` does not cover the real target below the store
/// and every execve there fails with EACCES. A host without a store simply skips those entries,
/// exactly like `/lib64`.
///
/// This is also the single source of truth for what an operator-supplied extra path duplicates,
/// which is why it lives here rather than inside [`build_ruleset`].
const BUILTIN_READONLY_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/lib",
    "/lib64",
    "/opt",
    "/etc",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/var",
    "/nix/store",
    "/gnu/store",
];

/// Why one operator-supplied extra read-only path does not become a Landlock rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtraPathSkip {
    /// The entry is relative. A Landlock rule is anchored on an opened directory, so resolving
    /// a relative entry against whatever cwd the command happens to inherit would grant rights
    /// nobody asked for.
    NotAbsolute,
    /// The entry is already covered by `BUILTIN_READONLY_PATHS`, or repeats an entry accepted
    /// earlier in the same list, so a second rule for it would change nothing.
    Duplicate,
    /// Nothing exists at that path on this host — treated exactly like a missing `/lib64`.
    Missing,
}

impl ExtraPathSkip {
    /// The operator-facing reason: one short phrase, no trailing punctuation.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            ExtraPathSkip::NotAbsolute => "not absolute",
            ExtraPathSkip::Duplicate => "already covered by the built-in allow-list",
            ExtraPathSkip::Missing => "does not exist",
        }
    }
}

/// The single-line note printed for one skipped extra path. Like [`optout_warning`] it goes to
/// stderr rather than to `tracing`, because the re-entry child has no logging subscriber yet
/// when the ruleset is built, and it names the config key the entry came from so the operator
/// can trace it back to their own settings.
#[must_use]
pub fn extra_path_skip_note(path: &str, skip: ExtraPathSkip) -> String {
    format!(
        "warning: ignoring sandbox_extra_readonly_paths entry \"{path}\": {}",
        skip.reason()
    )
}

/// Decide which operator-supplied extra read-only paths can become rules: the grantable list —
/// trimmed, absolute, existing on this host, deduplicated and lexicographically ordered — plus
/// one reason per entry that cannot, in the order the operator wrote them.
///
/// The de-duplication uses a `BTreeSet`, which gives de-duplication and a deterministic order for
/// free, so the Landlock rule order never depends on the order of the environment value. Nothing
/// here can widen the policy: the caller grants the result with the same read+execute rights as
/// the built-in FHS entries, and every rejected entry stays rejected.
#[must_use]
fn extra_readonly_rule_paths(
    extras: &[String],
    builtin: &[&str],
) -> (Vec<String>, Vec<(String, ExtraPathSkip)>) {
    let builtin: BTreeSet<&str> = builtin.iter().copied().collect();
    let mut granted: BTreeSet<String> = BTreeSet::new();
    let mut skipped: Vec<(String, ExtraPathSkip)> = Vec::new();

    for raw in extras {
        let entry = raw.trim();
        if entry.is_empty() {
            continue;
        }

        let reason = if !Path::new(entry).is_absolute() {
            Some(ExtraPathSkip::NotAbsolute)
        } else if builtin.contains(entry) {
            Some(ExtraPathSkip::Duplicate)
        } else if !Path::new(entry).exists() {
            Some(ExtraPathSkip::Missing)
        } else {
            None
        };

        if let Some(reason) = reason {
            skipped.push((entry.to_string(), reason));
            continue;
        }
        if !granted.insert(entry.to_string()) {
            skipped.push((entry.to_string(), ExtraPathSkip::Duplicate));
        }
    }

    (granted.into_iter().collect(), skipped)
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

/// Decide the re-entry action from injected inputs — no exec, no Landlock, no environment — so the
/// fail-closed choice (exit over exec) is unit-testable. Its rules are the security contract of the
/// re-entry protocol:
///
/// * exec only when the decision demanded Landlock **and** it was established, or when the decision
///   and the enforcement step agree on an explicit opt-out;
/// * everything else — a failed attempt, a decision demanding Landlock without an established ruleset,
///   a platform that cannot provide it, an unresolved executable — is a refusal with a non-zero exit code.
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
fn attempt_landlock(workspace_root: &Path, extra_readonly_paths: &[String]) -> SandboxAttempt {
    let abi = ABI::V5;

    let ruleset = match build_ruleset(workspace_root, abi, extra_readonly_paths) {
        Ok(ruleset) => ruleset,
        Err(failure) => return failure,
    };

    // 7. Enforce. `Ok` alone is *not* success: the kernel reports what it actually
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
/// A rule that the policy **requires** is added with `?`: if it cannot be added the whole attempt fails,
/// because a partially built policy is not the policy the caller asked for — the workspace root is such a
/// rule, and silently dropping it would enforce a policy nobody chose. Rules for paths that simply do not
/// exist on this host (no `~/.npm`, no `/var/tmp`, …) stay optional: an absent allow-list entry widens nothing.
///
/// `extra_readonly_paths` is the operator's own additions, already filtered and deduplicated by
/// [`extra_readonly_rule_paths`]: they join the read-only grant of [`BUILTIN_READONLY_PATHS`] and can
/// therefore never widen anything beyond read+execute, and never the workspace read/write rule.
#[cfg(target_os = "linux")]
fn build_ruleset(
    workspace_root: &Path,
    abi: ABI,
    extra_readonly_paths: &[String],
) -> Result<RulesetCreated, SandboxAttempt> {
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

    // 5. System toolchains, device nodes, runtime files (DNS /run/systemd/resolve), and binaries (Read-Only + Execute),
    //    including the package stores — see [`BUILTIN_READONLY_PATHS`] for why the stores are named explicitly.
    for p in BUILTIN_READONLY_PATHS {
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

    // 6. The operator's extra read-only paths, merged with exactly the same grant as the
    //    entries above, so a configured path can only ever add read+execute rights. Entries
    //    that are relative, absent here, or duplicates were filtered out by
    //    [`extra_readonly_rule_paths`] and announced on stderr; an entry that cannot be
    //    opened stays optional (like `/lib64` above) rather than silently widening anything.
    for p in extra_readonly_paths {
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
fn attempt_landlock(_workspace_root: &Path, _extra_readonly_paths: &[String]) -> SandboxAttempt {
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
    #[cfg(target_os = "linux")]
    use crate::harness::pty::SANDBOX_EXEC_ARG;
    use std::path::PathBuf;

    // Log capture: "an enforcement failure is never silent" is asserted, not
    // promised.
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

    // The error contract, driven by injected outcomes (no Landlock kernel, no
    // PTY, no `/dev/pts`).

    /// `Ok(())` is reserved for an established ruleset: every other attempt must be an
    /// error, so a failed setup can never look like success.
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

    /// End-to-end on the real code path, without restricting the test process: an unusable workspace
    /// root must produce an `Err`, never `Ok(())` after a warning, so the caller can never exec the
    /// command unsandboxed. Whether the host reports "no Landlock here" or "this rule could not be
    /// built" depends on the kernel; either way it must be an error with a reason, logged not swallowed.
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

    // The re-entry decision: exit over exec.

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

    /// The extra read-only path list survives the environment channel unchanged:
    /// nothing in is nothing out, and separators, blanks and padding never invent an
    /// entry.
    #[test]
    fn test_extra_readonly_paths_parsing_of_an_absent_or_empty_value_yields_nothing() {
        for raw in [None, Some(""), Some("   "), Some(":"), Some(":: :")] {
            assert_eq!(
                extra_readonly_paths_from_value(raw),
                Vec::<String>::new(),
                "{raw:?} must not produce a single path"
            );
        }
    }

    /// One entry, several entries, and the order the operator wrote them in.
    #[test]
    fn test_extra_readonly_paths_parsing_keeps_every_entry_in_order() {
        assert_eq!(
            extra_readonly_paths_from_value(Some("/nix/store")),
            vec!["/nix/store".to_string()],
            "a single entry is a single path"
        );
        assert_eq!(
            extra_readonly_paths_from_value(Some("/nix/store:/gnu/store")),
            vec!["/nix/store".to_string(), "/gnu/store".to_string()],
            "entries must keep their order, not be sorted"
        );
        assert_eq!(
            extra_readonly_paths_from_value(Some("/gnu/store:/nix/store")),
            vec!["/gnu/store".to_string(), "/nix/store".to_string()],
            "order must follow the value, whatever it is"
        );
        assert_eq!(
            extra_readonly_paths_from_value(Some("/opt/a:/opt/b:/opt/c")),
            vec![
                "/opt/a".to_string(),
                "/opt/b".to_string(),
                "/opt/c".to_string()
            ],
            "no entry may be dropped between the first and the last"
        );
    }

    /// Padding, a leading or trailing separator and an empty middle entry are all
    /// tolerated without producing an empty path that the ruleset would choke on.
    #[test]
    fn test_extra_readonly_paths_parsing_trims_and_drops_empty_entries() {
        assert_eq!(
            extra_readonly_paths_from_value(Some("  /nix/store  :  /gnu/store  ")),
            vec!["/nix/store".to_string(), "/gnu/store".to_string()],
            "surrounding blanks are padding, not part of the path"
        );
        assert_eq!(
            extra_readonly_paths_from_value(Some("/nix/store:")),
            vec!["/nix/store".to_string()],
            "a trailing separator must not add an empty entry"
        );
        assert_eq!(
            extra_readonly_paths_from_value(Some(":/gnu:/nix/store::/gnu/store")),
            vec![
                "/gnu".to_string(),
                "/nix/store".to_string(),
                "/gnu/store".to_string()
            ],
            "an empty entry anywhere is dropped, the rest is kept in order"
        );
    }

    /// The publish side is the exact inverse: what the spawn side encodes is what the
    /// child parses, and an empty list publishes nothing at all.
    #[test]
    fn test_extra_readonly_paths_env_encoding_round_trips() {
        for empty in [&[] as &[String], &[String::new()], &["  ".to_string()]] {
            assert_eq!(
                extra_readonly_paths_to_env(empty),
                None,
                "nothing to say must leave the variable unset"
            );
        }
        let paths = vec!["/nix/store".to_string(), "/gnu/store".to_string()];
        let encoded = extra_readonly_paths_to_env(&paths).expect("a non-empty list is published");
        assert_eq!(encoded, "/nix/store:/gnu/store");
        assert_eq!(
            extra_readonly_paths_from_value(Some(&encoded)),
            paths,
            "the value must parse back to exactly the list that produced it"
        );
    }

    /// The variable name is a fixed contract between the two processes, and it must
    /// stay distinguishable from the opt-out channel.
    #[test]
    fn test_extra_readonly_paths_env_name_is_the_published_contract() {
        assert_eq!(SANDBOX_EXTRA_RO_PATHS_ENV, "MARMEL_SANDBOX_EXTRA_RO_PATHS");
        assert_ne!(
            SANDBOX_EXTRA_RO_PATHS_ENV, SANDBOX_OPTOUT_ENV,
            "the path channel must never be confused with the opt-out channel"
        );
    }

    // Which extras actually become Landlock rules.

    /// An absolute path to a directory that really exists, under a tempdir the caller
    /// must keep alive for as long as the path is used.
    fn existing_extra_path(name: &str) -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(name);
        std::fs::create_dir(&dir).expect("the fixture directory must be creatable");
        (tmp, dir.to_string_lossy().into_owned())
    }

    /// Accepted extras are trimmed, deduplicated and ordered by the path itself, never by
    /// the order they arrived in: the resulting rule order must be the same on every host.
    #[test]
    fn test_extra_readonly_paths_are_accepted_trimmed_and_deterministically_ordered() {
        let (_tmp_a, a) = existing_extra_path("extra-a");
        let (_tmp_b, b) = existing_extra_path("extra-b");

        let extras = vec![
            format!("  {b}  "),
            a.clone(),
            b.clone(),
            "   ".to_string(),
            "".to_string(),
        ];
        let (granted, skipped) = extra_readonly_rule_paths(&extras, BUILTIN_READONLY_PATHS);

        let mut expected = vec![a.clone(), b.clone()];
        expected.sort();
        assert_eq!(
            granted, expected,
            "both real entries are granted exactly once, in lexicographic order"
        );
        assert_eq!(
            skipped,
            vec![(b.clone(), ExtraPathSkip::Duplicate)],
            "a repeated entry is reported once, as a duplicate"
        );
        assert!(
            granted.iter().all(|path| Path::new(path).is_absolute()),
            "only absolute paths may reach the ruleset: {granted:?}"
        );
    }

    /// A relative entry can never be granted: it would be resolved against whatever cwd the
    /// command inherits, so it is skipped even when it names a directory that exists there.
    #[test]
    fn test_extra_readonly_paths_relative_entries_are_skipped() {
        let relatives = [
            "relative/dir".to_string(),
            "./relative".to_string(),
            "../relative".to_string(),
            // Exists relative to the test's own working directory, and still must not be
            // granted: absoluteness is decided before any existence check.
            "src".to_string(),
        ];
        let (granted, skipped) = extra_readonly_rule_paths(&relatives, BUILTIN_READONLY_PATHS);

        assert!(
            granted.is_empty(),
            "no relative entry may become a rule: {granted:?}"
        );
        assert_eq!(
            skipped
                .iter()
                .map(|(_, reason)| *reason)
                .collect::<Vec<_>>(),
            vec![ExtraPathSkip::NotAbsolute; relatives.len()],
            "every relative entry is skipped, and says why"
        );
    }

    /// An extra that repeats a built-in allow-list entry — every entry, including the package
    /// stores — is skipped as a duplicate instead of granted twice, and the built-in grant
    /// itself is untouched.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_extra_readonly_paths_repeating_a_builtin_entry_are_skipped() {
        assert!(
            BUILTIN_READONLY_PATHS.contains(&"/nix/store")
                && BUILTIN_READONLY_PATHS.contains(&"/gnu/store"),
            "the stores stay built-in: an operator must not have to repeat them"
        );
        for builtin in BUILTIN_READONLY_PATHS {
            let (granted, skipped) =
                extra_readonly_rule_paths(&[(*builtin).to_string()], BUILTIN_READONLY_PATHS);
            assert!(
                granted.is_empty(),
                "{builtin} is already granted by the built-in list, twice is not more allowed"
            );
            assert_eq!(
                skipped,
                vec![((*builtin).to_string(), ExtraPathSkip::Duplicate)],
                "{builtin} must be reported as a duplicate, whatever this host has at that path"
            );
        }
    }

    /// A path that does not exist on this host is skipped, exactly like a missing `/lib64`:
    /// it never becomes a rule and never fails the whole attempt.
    #[test]
    fn test_extra_readonly_paths_that_do_not_exist_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp
            .path()
            .join("never-created")
            .to_string_lossy()
            .into_owned();

        let (granted, skipped) =
            extra_readonly_rule_paths(std::slice::from_ref(&missing), BUILTIN_READONLY_PATHS);
        assert!(
            granted.is_empty(),
            "a missing path must not reach the ruleset: {granted:?}"
        );
        assert_eq!(skipped, vec![(missing, ExtraPathSkip::Missing)]);
    }

    /// Every skipped entry gets exactly one single-line, attributable note: naming the entry,
    /// the config key it came from, and the reason.
    #[test]
    fn test_extra_path_skip_notes_name_the_entry_and_the_reason_in_one_line() {
        let cases = [
            ("relative/dir", ExtraPathSkip::NotAbsolute),
            ("/usr", ExtraPathSkip::Duplicate),
            ("/definitely/not/a/path", ExtraPathSkip::Missing),
        ];
        for (path, reason) in cases {
            let note = extra_path_skip_note(path, reason);
            assert!(
                note.starts_with("warning: ignoring sandbox_extra_readonly_paths entry "),
                "the note must read like the other sandbox warnings: {note}"
            );
            assert!(
                note.contains(&format!("\"{path}\"")),
                "the note must name the entry the operator wrote: {note}"
            );
            assert!(
                note.contains(reason.reason()),
                "the note must say why: {note}"
            );
            assert_eq!(
                note.lines().count(),
                1,
                "one line per skipped entry: {note}"
            );
        }

        let reasons = [
            ExtraPathSkip::NotAbsolute,
            ExtraPathSkip::Duplicate,
            ExtraPathSkip::Missing,
        ];
        let texts: Vec<&str> = reasons.iter().map(|reason| reason.reason()).collect();
        assert!(
            texts.iter().all(|text| !text.is_empty())
                && texts.windows(2).all(|pair| pair[0] != pair[1]),
            "the reasons must be distinct and non-empty: {texts:?}"
        );
    }

    /// The real ruleset builder must accept an extra read-only path alongside the built-in
    /// ones (skips visibly when this kernel has no Landlock).
    #[cfg(target_os = "linux")]
    #[test]
    fn test_landlock_ruleset_builds_with_an_extra_readonly_path() {
        if !kernel_has_landlock() {
            eprintln!(
                "SKIP test_landlock_ruleset_builds_with_an_extra_readonly_path: the running kernel has no usable Landlock"
            );
            return;
        }

        let (tmp, extra) = existing_extra_path("extra-read-only");
        let extras = vec![extra.clone(), "/usr".to_string()];
        let (granted, skipped) = extra_readonly_rule_paths(&extras, BUILTIN_READONLY_PATHS);
        assert_eq!(
            granted,
            vec![extra.clone()],
            "the extra is granted, the built-in repeat is not"
        );
        assert_eq!(
            skipped,
            vec![("/usr".to_string(), ExtraPathSkip::Duplicate)]
        );

        let abi = ABI::V5;
        let ruleset = build_ruleset(tmp.path(), abi, &granted)
            .expect("a ruleset with an extra read-only path must build");
        // Nothing was ever enforced, so dropping the ruleset leaves the test process
        // unrestricted.
        drop(ruleset);
    }

    // Source guards.

    /// The re-entry path in `main.rs` must never discard the enforcement result and must
    /// never re-spell the protocol literal: either would let a failed setup run the
    /// command unsandboxed.
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
        // Scoped to the call site: `canonicalize` on its own is matched by unrelated code
        // (`resolve_program_path` canonicalizes the exec target), and a needle that any
        // helper satisfies proves nothing about the cwd contract.
        assert!(
            src.contains("let cwd = canonical_sandbox_cwd("),
            "the re-entry cwd must be derived canonically, at the one place the protocol reads it"
        );
    }

    /// The one line a user sees when the allow-list misses the program the sandboxed shell
    /// tries to exec. Nothing on this host can reproduce it — there is no package store here,
    /// so no ruleset test can ever reach an exec denial — so the diagnostic promised in
    /// `bug.md` is pinned at the source: it must name Landlock, print the **resolved**
    /// program rather than the bare token, attribute a refusal to the allow-list, and keep
    /// terminating the child. The pre-fix wording named neither Landlock nor a path and is
    /// asserted away; it is spelled only here, never in `main.rs`, whose text is the scanned
    /// side.
    #[test]
    fn test_main_rs_exec_failure_names_the_sandbox_and_the_resolved_program() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("main.rs must be readable from the crate manifest dir");
        let exec_step = src
            .find("fn exec_pending_command(")
            .map(|from| {
                let rest = &src[from..];
                rest[..rest.find("fn boot_manager(").unwrap_or(rest.len())].to_string()
            })
            .expect("the exec step must stay one function");

        assert!(
            exec_step.contains("under the Landlock sandbox"),
            "the failure must say which sandbox refused the exec"
        );
        assert!(
            exec_step.contains("resolve_program_path(\"sh\")"),
            "the message must name the resolved image, not the bare token the shell was asked for"
        );
        assert!(
            exec_step.contains("not covered by the sandbox allow-list")
                && exec_step.contains("std::io::ErrorKind::PermissionDenied"),
            "a refusal must be attributed to the allow-list, and only a refusal may be"
        );
        assert!(
            exec_step.contains("std::process::exit(1)"),
            "a failed exec must terminate the re-entry child, never fall through"
        );
        assert!(
            !exec_step.contains("shell in sandbox"),
            "the unactionable pre-fix message must not come back"
        );
    }

    /// The Landlock integration tests must not rebuild a binary path from a hard-coded executable
    /// name: sandboxing is decided by the resolved exe path, never by a name.
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

    /// The package stores are the one part of the allow-list this host cannot exercise: with no
    /// `/nix/store` on disk, `build_ruleset` simply skips the entry exactly like a missing
    /// `/lib64`, so no ruleset test here can notice the entry being dropped — and dropping it is
    /// what turns every exec into EACCES on a NixOS or Guix host. That guarantee is therefore
    /// pinned at the source, in the style of the guards above: the store entries live inside the
    /// built-in declaration itself, and the ruleset builder keeps walking that same constant.
    ///
    /// Deliberately *not* re-asserted here: that the extras channel treats the stores as built-in
    /// entries (already pinned by
    /// `test_extra_readonly_paths_repeating_a_builtin_entry_are_skipped`), and the read-only grant
    /// itself (asserted as behaviour by `test_internal_sandbox_extra_readonly_paths_never_grant_write`).
    #[test]
    #[cfg(target_os = "linux")]
    fn test_builtin_readonly_paths_stay_declared_and_wired_into_the_ruleset() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/harness/sandbox.rs"
        ))
        .expect("sandbox.rs must be readable from the crate manifest dir");

        // Quoted needles are assembled from parts, like the executable-name guard above, so this
        // file never contains the needle it is looking for. The declaration anchor is assembled for
        // the same reason: spelled out here it would be the first match in the file, and the cut
        // block would then be test text that mentions the stores itself.
        let quote = char::from(34);
        let declaration = format!("const BUILTIN_READONLY_{} &[&str] = &[", "PATHS:");
        let block = src
            .find(&declaration)
            .map(|from| {
                let rest = &src[from..];
                rest[..rest.find("];").unwrap_or(rest.len())].to_string()
            })
            .expect("the built-in allow-list must stay one declared constant");
        for store in ["/nix/store", "/gnu/store"] {
            assert!(
                block.contains(&format!("{quote}{store}{quote}")),
                "{store} must stay an entry of the built-in read-only list: this host has no \
                 package store, so nothing else catches it being dropped"
            );
        }
        // The same constant must still be what the ruleset builder walks. Scoped to the builder
        // body because test loops iterate the constant too, and a needle matched by a test proves
        // nothing about the policy.
        let builder = src
            .find("fn build_ruleset(")
            .map(|from| {
                let rest = &src[from..];
                rest[..rest.find("fn add_rule(").unwrap_or(rest.len())].to_string()
            })
            .expect("the ruleset builder must stay one function");
        assert!(
            builder.contains("in BUILTIN_READONLY_PATHS {"),
            "the ruleset builder must keep granting the built-in list — an entry that nothing \
             walks grants nothing"
        );

        // Constant-level well-formedness, host-independent: every entry is an absolute anchor and
        // no entry is listed twice.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for entry in BUILTIN_READONLY_PATHS {
            assert!(
                !entry.is_empty() && Path::new(entry).is_absolute(),
                "a built-in rule must be a non-empty absolute path: {entry}"
            );
            assert!(seen.insert(*entry), "{entry} must not be listed twice");
        }
    }

    // Landlock integration: the real re-entry protocol, driven end to end.

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

    // Decision note for the two enforced-behaviour tests below (exec, then rename, inside a real
    // sandbox). Their skip logic is deliberately unchanged: `#[cfg(target_os = "linux")]` at
    // compile time plus the runtime `integration_host(name, true)` probe; no `#[ignore]`, no
    // environment gate. An invisible skip is the failure mode these tests exist to avoid, and a
    // skip is always announced as `SKIP ` (spelling pinned by the self-scan guard above).
    //
    // What the bug was: on a NixOS or Guix host these two tests did not skip, they failed. Both
    // drive the re-entry host, which execs the bare token `sh` *after* `restrict_self()`; Landlock
    // matches the resolved inode, and there `/bin` and `/usr` are symlink farms into the package
    // store, so the exec target was covered by no rule, `exec()` returned EACCES, the host exited
    // 1, and the `status.success()` assertions failed. A FHS host already covers `/bin/sh`, which
    // is why CI never saw it.
    //
    // What the fix changes for them: `/nix/store` and `/gnu/store` are now built-in entries of the
    // read-only grant (`BUILTIN_READONLY_PATHS`), and an operator can add further store roots via
    // `sandbox_extra_readonly_paths`. The exec half of both tests therefore now runs on a
    // store-based host: a Nixpkgs maintainer can un-skip the exec without editing either test.
    //
    // What a Nixpkgs maintainer should still expect:
    // - the `/etc/resolv.conf` half of the DNS test is a fixture question, not a policy one: `/etc`
    //   is allow-listed, but a network-less build sandbox may have no readable `/etc/resolv.conf`,
    //   so that half may still need a fixture (or an existence check splitting it out into its own
    //   test) even though the exec itself succeeds;
    // - the rename test probes `python3` and falls back to `mv`, and both live in the store, so
    //   that leg depends on the same store rule; a package set exposing neither is a fixture
    //   problem, not a Landlock one;
    // - both tests exec the re-entry *binary* discovered next to the test binary and require it to
    //   be at least as new as the newest source file (`candidate_is_current`), so a checkPhase that
    //   builds the bin in another phase, or from a copied tree, yields a visible `SKIP` instead of
    //   a pass: build the bin and run the tests from the same source tree.

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

    /// The negative security assertion for the operator channel, as behaviour rather than as a
    /// promise in a comment: an extra read-only path can only ever **add read access**, and the
    /// value the spawning process encodes is the value the re-entry child understands, because
    /// both ends key on [`SANDBOX_EXTRA_RO_PATHS_ENV`] — the producer here is
    /// [`extra_readonly_paths_to_env`], the consumer is [`extra_readonly_paths_from_env`] in the
    /// re-entry child, and a mismatch on either the name or the encoding breaks leg 2 below.
    ///
    /// One fixture directory, four legs: a fresh tempdir under Cargo's own scratch dir, i.e. inside
    /// this package and therefore outside every built-in allow-list entry, and outside the run-time
    /// workspace rule as well (that one is anchored on a separate tempdir), so nothing can reach it
    /// unless the extras channel grants it.
    ///
    /// 1. nothing published → the sandboxed read is denied: the fixture really is unlisted, so a
    ///    later success is attributable to the channel and not to the fixture;
    /// 2. published → the same read succeeds through the real ruleset;
    /// 3. published, write attempted → denied, and no file appears: the grant for a configured
    ///    path is the same read-only grant as `/usr` or the package stores, never the workspace
    ///    grant, so a configured path can never widen the policy;
    /// 4. the same write with an explicit opt-out (no sandbox at all) → succeeds, which proves
    ///    the denials in 1 and 3 came from Landlock and not from the fixture or the shell.
    ///
    /// Every leg also insists the exit code is not [`SANDBOX_REFUSE_EXIT_CODE`]: a refusal to run
    /// the command is not evidence about the ruleset. Skips visibly, like the tests above, when
    /// this host has no usable Landlock or no current re-entry binary.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_internal_sandbox_extra_readonly_paths_never_grant_write() {
        let name = "test_internal_sandbox_extra_readonly_paths_never_grant_write";
        let Some(exe) = integration_host(name, true) else {
            return;
        };
        // The fixture needs a place this test process can create that the child's ruleset has no
        // rule for. Cargo's own scratch dir under `target/` is inside the package, and the package
        // is deliberately not part of the built-in allow-list — unlike `/tmp`, `/var/tmp` or the
        // HOME build caches, which the policy grants read-write on its own and which would
        // therefore make every leg below meaningless.
        let scratch = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .or_else(|| {
                let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
                target.is_dir().then_some(target)
            });
        let Some(scratch) = scratch else {
            eprintln!("SKIP {name}: no scratch directory outside the allow-list is available");
            return;
        };
        let Ok(unlisted) = tempfile::tempdir_in(scratch.as_path()) else {
            eprintln!(
                "SKIP {name}: {} is not writable, so the fixture cannot be created",
                scratch.display()
            );
            return;
        };
        let workspace = tempfile::tempdir().expect("tempdir");

        // A fixture the built-in policy already reaches would make legs 2 and 3 meaningless.
        if BUILTIN_READONLY_PATHS
            .iter()
            .any(|builtin| unlisted.path().starts_with(builtin))
            || unlisted.path().starts_with(workspace.path())
        {
            eprintln!(
                "SKIP {name}: {} is already covered by the built-in allow-list",
                unlisted.path().display()
            );
            return;
        }

        let extra = unlisted.path().to_string_lossy().into_owned();
        let payload = unlisted.path().join("payload.txt");
        let child_write = unlisted.path().join("child-wrote-this.txt");
        std::fs::write(&payload, "unlisted payload").expect("the fixture payload must be writable");
        let published = extra_readonly_paths_to_env(std::slice::from_ref(&extra))
            .expect("one absolute entry must be publishable");

        let run = |command: &str, extras: Option<&str>, opt_out: bool| {
            let mut cmd = std::process::Command::new(&exe);
            cmd.arg(SANDBOX_EXEC_ARG)
                .arg(workspace.path())
                .arg(command)
                .env_remove(SANDBOX_OPTOUT_ENV)
                .env_remove(SANDBOX_EXTRA_RO_PATHS_ENV);
            if let Some(value) = extras {
                cmd.env(SANDBOX_EXTRA_RO_PATHS_ENV, value);
            }
            if opt_out {
                cmd.env(SANDBOX_OPTOUT_ENV, "1");
            }
            cmd.output().expect("re-entry host must be runnable")
        };
        let stderr_of =
            |out: &std::process::Output| String::from_utf8_lossy(&out.stderr).into_owned();

        // 1. Unlisted: not readable, and not because the host refused to run anything.
        let unlisted_read = run(&format!("cat '{}'", payload.display()), None, false);
        assert_ne!(
            unlisted_read.status.code(),
            Some(SANDBOX_REFUSE_EXIT_CODE),
            "the sandbox must be established for this leg to mean anything, stderr: {}",
            stderr_of(&unlisted_read)
        );
        assert!(
            !unlisted_read.status.success()
                && !String::from_utf8_lossy(&unlisted_read.stdout).contains("unlisted payload"),
            "a path outside the allow-list must not be readable inside the sandbox, stderr: {}",
            stderr_of(&unlisted_read)
        );

        // 2. Published by the spawn side, honoured by the child through the shared constant.
        let listed_read = run(
            &format!("cat '{}'", payload.display()),
            Some(&published),
            false,
        );
        assert!(
            listed_read.status.success()
                && String::from_utf8_lossy(&listed_read.stdout) == "unlisted payload",
            "an extra read-only path must be readable in the re-entry child, stderr: {}",
            stderr_of(&listed_read)
        );

        // 3. Same published entry, write attempted: still read-only.
        let attempt = format!("printf %s not-allowed > '{}'", child_write.display());
        let listed_write = run(&attempt, Some(&published), false);
        assert_ne!(
            listed_write.status.code(),
            Some(SANDBOX_REFUSE_EXIT_CODE),
            "the command must have run under the sandbox for the denial to mean anything, stderr: {}",
            stderr_of(&listed_write)
        );
        assert!(
            !listed_write.status.success(),
            "an extra read-only path must never grant write access, stderr: {}",
            stderr_of(&listed_write)
        );
        assert!(
            !child_write.exists(),
            "the sandboxed command must not have been able to create {} in the extra path",
            child_write.display()
        );

        // 4. Control: without any sandbox the same write succeeds, so 1 and 3 measured Landlock.
        let unsandboxed_write = run(&attempt, Some(&published), true);
        assert!(
            unsandboxed_write.status.success() && child_write.exists(),
            "the fixture must be writable unsandboxed, otherwise the denials above prove nothing: \
             stderr {}",
            stderr_of(&unsandboxed_write)
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
