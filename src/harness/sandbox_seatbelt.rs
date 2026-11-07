//! macOS Seatbelt backend: the libSystem FFI seam, the single enforcement attempt, and the
//! post-init probe that turns "Seatbelt accepted the profile" into "Seatbelt is enforcing".
//!
//! This module is the macOS half of the same fail-closed sandbox contract
//! ([`crate::harness::sandbox`] owns the platform-neutral surface). Everything in it lives
//! behind `#[cfg(target_os = "macos")]` — see the module declaration in [`crate::harness`]: a
//! Linux build must not see a single line of it, because this crate denies `dead_code` and
//! `unused` per target, and there is no Landlock on a Mac and no Seatbelt on a Linux box.
//!
//! ## Mechanism, and why it is written by hand
//!
//! `painless-belt` (`.marmel/research/painless-belt.md`) calls `sandbox_init` with `flags = 0`
//! and no parameter list. We call **`sandbox_init_with_parameters`** with `flags = 0` and an
//! empty, NULL-terminated parameter list, for two reasons.
//!
//! The first is that the profile the emitter builds embeds **literal** paths (SBPL filters match
//! path text, not resolved descriptors — gotcha 1 of `.marmel/research/seatbelt-macos.md`), so
//! there is no `$()` substitution to do, and an empty list states that explicitly instead of
//! relying on the parameter slot being optional.
//!
//! The second is that `sandbox_init_with_parameters` is the same libSystem entry point with the
//! same process-once semantics, and the variant every browser sandbox uses, so keeping the
//! parameter slot open is the cheaper future (per-name filters, a `TMPDIR` passed in rather than
//! baked into the text).
//!
//! The bindings are declared here rather than depended on: `libc` is already a `cfg(unix)`
//! dependency, so this needs **no** `Cargo.toml` change, and the alternatives
//! (`painless-belt`, `macos-sandbox-sys`) either drag in a CLI's argv parsing or exist only to
//! hand over these two declarations (`.marmel/research/seatbelt-feasibility.md` §2). Apple's
//! API is undocumented and deprecated-but-live; that is a duty to re-verify per macOS major
//! release, not an argument for a dependency.
//!
//! ## The two things the API does not tell us, and how they are handled
//!
//! Process-once. The first `sandbox_init*` call in a process installs the profile, and every
//! later call answers `EPERM` without touching the existing restriction. So [`attempt_seatbelt`]
//! calls it **exactly once** and never retries: profile layering (policy plus extras plus
//! operation block) is text composition *before* compilation, done by
//! [`crate::harness::sbpl::render_profile`]. A second call from this process is a bug, and the
//! `EPERM` it returns is mapped to a refusal, never to success.
//!
//! No `RulesetStatus` analogue. Landlock's `restrict_self()` reports what the kernel actually
//! enforces, while Seatbelt's `rc == 0` only says the profile compiled and was installed.
//! [`probe_open`] supplies the missing oracle: it opens a path the allow-list never grants
//! (`$HOME/.ssh/config`, falling back to `$HOME/.ssh`) with a raw `open(2)`. A denied open
//! (`EPERM`/`EACCES`) is positive evidence that the profile is active, and is reported as
//! [`SandboxAttempt::FullyEnforced`]; a **succeeded** open means the profile is not restricting
//! this process and is reported as [`SandboxAttempt::EnforcementFailed`], which the caller turns
//! into a refusal to exec. An absent probe path proves nothing, and is reported as
//! [`SandboxAttempt::PartiallyEnforced`] — the shape Landlock's partial status has, so the
//! caller accepts it and warns, and "enforcement" is never claimed without evidence.
//!
//! ## What is still `[verify-on-Mac]`
//!
//! The probe paths must actually be denied by the emitted profile. The allow-list grants
//! `$HOME/.cargo`, `.cache`, `.npm`, `.rustup`, `.config`, `.local` and `.gitconfig`, not
//! `$HOME` wholesale, so `~/.ssh` is outside it; the probe battery in
//! `.marmel/research/seatbelt-feasibility.md` §8 confirms that on a real host.
//!
//! The breadth of the non-file allow-block (the emitter's `[verify-on-Mac]` section) is a floor,
//! not a tuned policy: too narrow shows up as `EPERM` inside the confined command, which this
//! probe cannot see.
//!
//! The FFI signatures themselves (`char **errorbuf`, `uint64_t flags`, `int rc`) come from
//! Apple's header plus two independent consumers of it, and are exercised only by CI's
//! `Test (macos-latest)` leg — the Linux legs never compile this file.

#![cfg(target_os = "macos")]

use crate::harness::sandbox::SandboxAttempt;
use crate::harness::sbpl;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The `flags` argument of `sandbox_init_with_parameters`, zero because the profile states its
/// own grants.
///
/// Seatbelt's other two values (`SANDBOX_INIT_CHALLENGING_COMPAT`,
/// `SANDBOX_INIT_DEBUGGING_COMPAT`) widen the profile for Apple's own compatibility and debug
/// machinery — painless-belt passes 0 too — and both work against the deny-default baseline
/// this profile is built on.
const SEATBELT_INIT_FLAGS: u64 = 0;

/// The primary enforcement oracle: a path the shared allow-list never grants.
///
/// [`crate::harness::sandbox::policy_paths`] grants `$HOME/.cargo`, `.cache`, `.npm`,
/// `.rustup`, `.config`, `.local` and `.gitconfig`, but not `$HOME/.ssh`, so an `open` of
/// `.ssh/config` that is *not* refused means the profile is not in effect.
const PROBE_SSH_CONFIG: &str = ".ssh/config";

/// The directory half of the oracle, used when [`PROBE_SSH_CONFIG`] does not exist.
const PROBE_SSH_DIR: &str = ".ssh";

// The two libSystem symbols this backend needs: the entry point (rc `0` means the profile
// compiled and was installed, a non-zero rc means it was not) and the deallocator for the
// diagnostic it hands back, which libSystem allocates and the caller must not drop.
//
// Declared by hand because `libc` does not ship these names: `Cargo.toml` needs no change,
// since `libc` is already a `cfg(unix)` dependency and libSystem is the default link on macOS.
unsafe extern "C" {
    fn sandbox_init_with_parameters(
        profile: *const libc::c_char,
        flags: u64,
        parameters: *const *const libc::c_char,
        errorbuf: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

/// Confine the current process with a Seatbelt profile built from the shared policy list.
///
/// The macOS sibling of `crate::harness::sandbox::attempt_landlock`: the same inputs
/// (a canonicalised workspace root plus the pre-filtered operator extras), the same
/// [`SandboxAttempt`] vocabulary, and the same fail-closed mapping on top of it
/// ([`crate::harness::sandbox::sandbox_outcome`]). Reached only from the re-entry child, and
/// only once per process, because Seatbelt cannot be re-applied and cannot be widened
/// afterwards.
///
/// The attempt is deliberately sequenced: render the whole profile as one flat string, hand it
/// to libSystem **once**, then prove enforcement by probe.
#[must_use = "the Seatbelt attempt decides whether the re-entry child may exec at all"]
pub fn attempt_seatbelt(workspace_root: &Path, extra_readonly_paths: &[String]) -> SandboxAttempt {
    // 1. One flat profile. Seatbelt has no reliable `import` across a string boundary, and the
    //    call below cannot be repeated, so every grant has to be in the text already.
    let profile = match CString::new(sbpl::render_profile(workspace_root, extra_readonly_paths)) {
        Ok(profile) => profile,
        // A `NUL` anywhere in the rendered profile means the policy text is not a valid C
        // string, i.e. the requested policy cannot be handed to the kernel: the Seatbelt
        // analogue of a Landlock rule that could not be added.
        Err(err) => {
            return SandboxAttempt::RuleFailed {
                path: workspace_root.display().to_string(),
                reason: format!("rendering the Seatbelt profile as a C string: {err}"),
            };
        }
    };

    // 2. Install it, exactly once. The parameter list is a NULL-terminated array holding no
    //    entries: the profile embeds literal paths, so there is nothing to substitute.
    let mut errorbuf: *mut libc::c_char = std::ptr::null_mut();
    let parameters: [*const libc::c_char; 1] = [std::ptr::null()];
    let rc = unsafe {
        // SAFETY: `profile` is a live `CString` whose pointer stays valid for the duration of
        // the call, `parameters` is a NULL-terminated array of zero strings, `errorbuf` is an
        // address the callee is allowed to fill, and `flags` is the documented zero value.
        sandbox_init_with_parameters(
            profile.as_ptr(),
            SEATBELT_INIT_FLAGS,
            parameters.as_ptr(),
            &mut errorbuf,
        )
    };
    if rc != 0 {
        let diagnostic = take_error_text(errorbuf);
        return SandboxAttempt::EnforcementFailed {
            reason: init_failure_reason(rc, diagnostic.as_deref()),
        };
    }

    // 3. `rc == 0` proves the profile compiled and was installed — not that it is enforcing.
    //    Landlock reports that itself and Seatbelt has no equivalent, so the probe below
    // decides it.
    let Some((config, dir)) = probe_seatbelt() else {
        let reason = "Seatbelt installed the profile, but the enforcement probe could not run: \
                      HOME is unset, so neither ~/.ssh/config nor ~/.ssh could be opened and \
                      enforcement rests on sandbox_init_with_parameters alone"
            .to_string();
        tracing::warn!("Seatbelt: {reason}");
        return SandboxAttempt::PartiallyEnforced { reason };
    };

    let attempt = verdict(&config, &dir);
    match &attempt {
        SandboxAttempt::FullyEnforced => tracing::debug!(
            "Seatbelt: profile active ({} bytes), enforcement confirmed by a denied open of {}",
            profile.as_bytes().len(),
            dir.path.display()
        ),
        SandboxAttempt::PartiallyEnforced { reason } => tracing::warn!("Seatbelt: {reason}"),
        _ => {}
    }
    attempt
}

/// One probe of the enforcement oracle: the path that was opened, and what that open did.
#[derive(Debug, PartialEq, Eq)]
struct Probe {
    path: PathBuf,
    outcome: ProbeOutcome,
}

/// What one probe open achieved, classified from the syscall's own evidence.
#[derive(Debug, PartialEq, Eq)]
enum ProbeOutcome {
    /// The open succeeded: the path exists and Seatbelt let it through.
    Allowed,
    /// The open was refused by the sandbox (`EPERM`) or by file permissions (`EACCES`) — for a
    /// path the allow-list never grants, the signature of an active profile.
    Denied,
    /// The path is not on this host (`ENOENT`, `ENOTDIR`): the probe proves nothing.
    Absent,
    /// The open failed some other way, or the path could not be handed to `open(2)`.
    Unknown { reason: String },
}

/// The two oracle paths, opened in the order the fallback is meant to be read.
///
/// `None` only when `$HOME` is unset or empty — the one case in which the oracle is
/// unreachable at all, which is why it is reported separately from an absent probe path.
fn probe_seatbelt() -> Option<(Probe, Probe)> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;

    let config_path = home.join(PROBE_SSH_CONFIG);
    let dir_path = home.join(PROBE_SSH_DIR);
    let config = Probe {
        outcome: probe_open(&config_path),
        path: config_path,
    };
    let dir = Probe {
        outcome: probe_open(&dir_path),
        path: dir_path,
    };
    Some((config, dir))
}

/// Open `path` for reading with a raw `open(2)` and classify what happened.
///
/// Raw rather than `std::fs::File::open` because the answer has to come from the same mediated
/// syscall the confined command will make, and because `std::fs` would bury the errno the
/// verdict depends on. A successful open is closed again immediately so the probe leaks no
/// descriptor into the confined process.
fn probe_open(path: &Path) -> ProbeOutcome {
    let cpath = match CString::new(path.as_os_str().as_bytes()) {
        Ok(cpath) => cpath,
        Err(err) => {
            return ProbeOutcome::Unknown {
                reason: format!("{}: cannot be passed to open(2): {err}", path.display()),
            };
        }
    };

    let fd = unsafe {
        // SAFETY: `cpath` is a live `CString` naming the path to open, and `O_RDONLY` takes no
        // mode argument.
        libc::open(cpath.as_ptr(), libc::O_RDONLY)
    };

    if fd >= 0 {
        unsafe {
            // SAFETY: `fd` is the descriptor `open` just returned.
            libc::close(fd);
        }
        return ProbeOutcome::Allowed;
    }

    // The errno has to be read here and not later: anything done in between can overwrite it.
    classify_errno(std::io::Error::last_os_error())
}

/// The errno-to-verdict half, so the classification is testable without a sandbox.
///
/// `EPERM` is what Seatbelt returns for a denied operation and `EACCES` is what an explicit
/// `(deny …)` clause or plain file permissions return. Both mean "restricted", neither means
/// "absent". Anything else — an interrupted call, an exhausted descriptor table — proves
/// nothing and is never allowed to masquerade as a denial.
#[must_use]
fn classify_errno(err: std::io::Error) -> ProbeOutcome {
    match err.raw_os_error().unwrap_or(0) {
        code if code == libc::EPERM || code == libc::EACCES => ProbeOutcome::Denied,
        code if code == libc::ENOENT || code == libc::ENOTDIR => ProbeOutcome::Absent,
        _ => ProbeOutcome::Unknown {
            reason: err.to_string(),
        },
    }
}

/// What the two probes together say about this process.
///
/// Fail-closed and evidence-first: a `Denied` on either probe is positive proof of an active
/// profile; an `Allowed` on either is proof of the opposite (an allow-list that lets `~/.ssh`
/// through is not the allow-list we emitted, and a profile that did not install would read
/// exactly the same); and neither `Absent` nor `Unknown` may claim enforcement, so they degrade
/// to `PartiallyEnforced` — which the caller turns into `Ok(())` **plus a warning**, never into
/// a silent claim of a restricted process.
#[must_use]
fn verdict(config: &Probe, dir: &Probe) -> SandboxAttempt {
    match config.outcome {
        ProbeOutcome::Denied => SandboxAttempt::FullyEnforced,
        ProbeOutcome::Allowed => not_enforced(config),
        ProbeOutcome::Absent | ProbeOutcome::Unknown { .. } => match &dir.outcome {
            ProbeOutcome::Denied => SandboxAttempt::FullyEnforced,
            ProbeOutcome::Allowed => not_enforced(dir),
            ProbeOutcome::Absent => SandboxAttempt::PartiallyEnforced {
                reason: format!(
                    "Seatbelt installed the profile, but enforcement was not confirmed by probe: \
                     neither {} nor {} exists on this host, so enforcement rests on \
                     sandbox_init_with_parameters alone",
                    config.path.display(),
                    dir.path.display(),
                ),
            },
            ProbeOutcome::Unknown { reason } => SandboxAttempt::PartiallyEnforced {
                reason: format!(
                    "Seatbelt installed the profile, but enforcement was not confirmed by probe: \
                     probing {} ({}) — {}",
                    config.path.display(),
                    dir.outcome.label(),
                    reason.as_str(),
                ),
            },
        },
    }
}

/// The refusal half of the verdict: the oracle path opened, so this process is not restricted.
#[must_use]
fn not_enforced(probe: &Probe) -> SandboxAttempt {
    SandboxAttempt::EnforcementFailed {
        reason: format!(
            "Seatbelt reported success, but {} was readable after the profile was applied: this \
             process is NOT restricted by Seatbelt",
            probe.path.display()
        ),
    }
}

impl ProbeOutcome {
    /// A one-word label for a probe outcome, for the reason strings the verdict builds.
    fn label(&self) -> &'static str {
        match self {
            ProbeOutcome::Allowed => "readable",
            ProbeOutcome::Denied => "denied",
            ProbeOutcome::Absent => "absent",
            ProbeOutcome::Unknown { .. } => "unclassifiable",
        }
    }
}

/// The libSystem diagnostic that came with a failed init, read out of the buffer it allocated.
///
/// The buffer is libSystem's to free, so it is read first and given to `sandbox_free_error`
/// afterwards, and no code may touch it past that point. A `NULL` buffer is tolerated (the API
/// is documented to fill one, and a non-zero rc with no diagnostic is a real possibility), and
/// a non-UTF-8 message is kept as far as `to_string_lossy` allows rather than dropped.
fn take_error_text(errorbuf: *mut libc::c_char) -> Option<String> {
    if errorbuf.is_null() {
        return None;
    }
    let text = unsafe {
        // SAFETY: `errorbuf` is non-NULL and points at the NUL-terminated C string libSystem
        // allocated for this call.
        CStr::from_ptr(errorbuf)
    }
    .to_string_lossy()
    .trim()
    .to_string();
    unsafe {
        // SAFETY: `errorbuf` is the buffer libSystem allocated and has not been handed to
        // `sandbox_free_error` yet; from here on it is freed and must not be read again.
        sandbox_free_error(errorbuf);
    }
    if text.is_empty() { None } else { Some(text) }
}

/// The reason string for a failed init: never empty, always attributable to the call that
/// failed, and embedding libSystem's own wording when it gave any.
#[must_use]
fn init_failure_reason(rc: i32, diagnostic: Option<&str>) -> String {
    match diagnostic {
        Some(text) => {
            format!("Seatbelt rejected the profile (sandbox_init_with_parameters rc {rc}): {text}")
        }
        None => format!(
            "Seatbelt rejected the profile (sandbox_init_with_parameters rc {rc}); libSystem \
             returned no diagnostic. A Seatbelt profile applies once per process, so an already \
             confined process cannot be narrowed again."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(path: &str, outcome: ProbeOutcome) -> Probe {
        Probe {
            path: PathBuf::from(path),
            outcome,
        }
    }

    fn unknown() -> ProbeOutcome {
        ProbeOutcome::Unknown {
            reason: "not my sandbox".to_string(),
        }
    }

    /// The oracle: a denied probe of a path the allow-list never grants is what proves the
    /// profile is enforcing, on either half of the probe pair.
    #[test]
    fn test_denied_probe_proves_enforcement() {
        assert_eq!(
            verdict(
                &probe("/home/ann/.ssh/config", ProbeOutcome::Denied),
                &probe("/home/ann/.ssh", ProbeOutcome::Allowed),
            ),
            SandboxAttempt::FullyEnforced,
            "a denied config probe must be read as enforcement even if the directory is readable"
        );
        assert_eq!(
            verdict(
                &probe("/home/ann/.ssh/config", ProbeOutcome::Absent),
                &probe("/home/ann/.ssh", ProbeOutcome::Denied),
            ),
            SandboxAttempt::FullyEnforced,
            "the fallback probe must be enough when the config file is absent"
        );
        assert_eq!(
            ProbeOutcome::Denied.label(),
            "denied",
            "the labels the reason strings are built from must stay distinguishable"
        );
    }

    /// A succeeded probe is the fail-closed case: Seatbelt reported `rc == 0` and this process
    /// is still unrestricted, which must be an error and never a warning.
    #[test]
    fn test_allowed_probe_is_an_enforcement_failure() {
        let cases = [
            (ProbeOutcome::Allowed, ProbeOutcome::Denied),
            (ProbeOutcome::Absent, ProbeOutcome::Allowed),
            (unknown(), ProbeOutcome::Allowed),
        ];
        for (config, dir) in cases {
            let attempt = verdict(
                &probe("/home/ann/.ssh/config", config),
                &probe("/home/ann/.ssh", dir),
            );
            match attempt {
                SandboxAttempt::EnforcementFailed { reason } => {
                    assert!(!reason.trim().is_empty(), "a refusal must explain itself");
                    assert!(
                        reason.contains(".ssh"),
                        "the refusal must name the probe that failed: {reason}"
                    );
                }
                other => panic!("must refuse, got {other:?}"),
            }
        }
    }

    /// Probe paths that do not exist, or that could not be classified, prove nothing: the
    /// attempt must be the one the caller reports as accepted **with a warning**, never
    /// `FullyEnforced` and never a silent `FullyEnforced`.
    #[test]
    fn test_absent_or_unknown_probe_never_claims_enforcement() {
        let cases = [
            (ProbeOutcome::Absent, ProbeOutcome::Absent),
            (ProbeOutcome::Absent, unknown()),
            (unknown(), ProbeOutcome::Absent),
        ];
        for (config, dir) in cases {
            let attempt = verdict(
                &probe("/home/ann/.ssh/config", config),
                &probe("/home/ann/.ssh", dir),
            );
            match attempt {
                SandboxAttempt::PartiallyEnforced { reason } => {
                    assert!(
                        reason.contains(".ssh"),
                        "the reason must name the probe it could not carry out: {reason}"
                    );
                    assert!(
                        reason.contains("not confirmed"),
                        "an unverified attempt must say so: {reason}"
                    );
                }
                other => panic!("must be an accepted-but-warning attempt, got {other:?}"),
            }
        }
    }

    /// Errno classification: Seatbelt's `EPERM` and permission `EACCES` both read as denied,
    /// absence never reads as a denial, and anything else is inconclusive.
    #[test]
    fn test_errno_classification_is_fail_closed() {
        for code in [libc::EPERM, libc::EACCES] {
            assert_eq!(
                classify_errno(std::io::Error::from_raw_os_error(code)),
                ProbeOutcome::Denied,
                "errno {code} must read as a denial"
            );
        }
        for code in [libc::ENOENT, libc::ENOTDIR] {
            assert_eq!(
                classify_errno(std::io::Error::from_raw_os_error(code)),
                ProbeOutcome::Absent,
                "errno {code} must read as absent"
            );
        }
        for code in [libc::EINTR, libc::ELOOP, libc::EMFILE] {
            assert!(
                matches!(
                    classify_errno(std::io::Error::from_raw_os_error(code)),
                    ProbeOutcome::Unknown { .. }
                ),
                "errno {code} must be inconclusive, never a denial"
            );
        }
    }

    /// A failed init always carries a non-empty reason naming the call, with or without a
    /// libSystem diagnostic.
    #[test]
    fn test_init_failure_reason_is_never_empty() {
        for rc in [1, -1, 17] {
            let without = init_failure_reason(rc, None);
            assert!(
                !without.trim().is_empty(),
                "rc {rc} must still be explained"
            );
            assert!(
                without.contains("sandbox_init_with_parameters"),
                "the reason must name the call that failed: {without}"
            );
            assert!(
                without.contains("once"),
                "the reason must say why retrying cannot help: {without}"
            );
        }
        let with = init_failure_reason(1, Some("syntax error at line 4"));
        assert!(with.contains("syntax error at line 4"), "{with}");
        assert!(with.contains("rc 1"), "{with}");
    }

    /// The diagnostic is read before it is freed, and an empty or absent message still leaves a
    /// usable reason rather than `None` pretending to be success.
    #[test]
    fn test_error_text_is_read_out_of_the_buffer_it_frees() {
        let owned = CString::new("  syntax error at line 4  ")
            .expect("test diagnostic without an embedded NUL");
        let ptr = owned.as_ptr().cast_mut();
        // `take_error_text` gives the buffer to `sandbox_free_error`, so `owned` must not free
        // it a second time on the way out of this test.
        std::mem::forget(owned);
        assert_eq!(
            take_error_text(ptr).as_deref(),
            Some("syntax error at line 4")
        );
        assert_eq!(take_error_text(std::ptr::null_mut()), None);
    }

    /// `attempt_seatbelt` must feed the shared emitter rather than a copy of the policy: one
    /// source of truth for the allow-list, pinned here because no Linux host can ever exercise
    /// this backend. It also pins the process-once constraint: exactly one call site.
    #[test]
    fn test_attempt_is_wired_to_the_shared_policy_and_calls_init_once() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/harness/sandbox_seatbelt.rs"
        ))
        .expect("the Seatbelt backend must be readable from the crate manifest dir");
        assert!(
            src.contains("sbpl::render_profile("),
            "the Seatbelt profile must come from the shared emitter"
        );
        assert!(
            src.contains("sandbox_init_with_parameters(") && src.contains("sandbox_free_error("),
            "both libSystem symbols must stay declared and used"
        );

        let production = src.split("#[cfg(test)]").next().unwrap_or(&src);
        assert_eq!(
            production.matches("sandbox_init_with_parameters(").count(),
            2,
            "sandbox_init must be declared once and called exactly once per attempt \
             (Seatbelt is process-once)"
        );
        assert!(
            !production.contains("landlock::"),
            "the Seatbelt backend must not grow a Landlock dependency"
        );
    }

    /// The oracle paths are relative to `$HOME` and spelled once each, so the probe cannot
    /// drift away from what the policy does or does not grant.
    #[test]
    fn test_probe_paths_stay_relative_to_home() {
        assert_eq!(PROBE_SSH_CONFIG, ".ssh/config");
        assert_eq!(PROBE_SSH_DIR, ".ssh");
        for path in [PROBE_SSH_CONFIG, PROBE_SSH_DIR] {
            assert!(
                !path.starts_with('/') && !path.contains("$HOME"),
                "{path} must be joinable onto $HOME rather than hard-coded to one user"
            );
        }
    }

    /// The text the attempt hands to libSystem is the rendered policy, not a hard-coded profile:
    /// its head and its workspace clause come from the shared emitter.
    #[test]
    fn test_profile_handed_to_libsystem_is_the_rendered_policy() {
        let root = Path::new("/work/sandbox root");
        let profile = sbpl::render_profile(root, &[]);
        assert!(profile.starts_with("(version 1)"), "head: {profile}");
        assert!(profile.contains("(deny default)"), "baseline: {profile}");
        assert!(
            profile.contains("\"/work/sandbox root\""),
            "the workspace root must be granted in the profile handed to sandbox_init: {profile}"
        );
    }
}
