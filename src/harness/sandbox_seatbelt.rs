//! macOS Seatbelt backend: the libSystem FFI seam, the single enforcement attempt, and the
//! probe that turns "Seatbelt accepted the profile" into "Seatbelt is enforcing".
//!
//! This module is the macOS half of the same fail-closed sandbox contract
//! ([`crate::harness::sandbox`] owns the platform-neutral surface). Everything in it lives
//! behind `#[cfg(target_os = "macos")]` — see the module declaration in [`crate::harness`]: a
//! Linux build must not see a single line of it, because this crate denies `dead_code` and
//! `unused` per target, and there is no Landlock on a Mac and no Seatbelt on a Linux box.
//!
//! ## Mechanism, and why it is written by hand
//!
//! The upstream `painless-belt` project calls `sandbox_init` with `flags = 0`
//! and no parameter list. We call **`sandbox_init_with_parameters`** with `flags = 0` and an
//! empty, NULL-terminated parameter list, for two reasons.
//!
//! The first is that the profile the emitter builds embeds **literal** paths, because SBPL
//! filters match path text rather than the resolved file descriptors Landlock matches, so
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
//! hand over these declarations. Apple's API is
//! undocumented and deprecated-but-live; that is a duty to re-verify per macOS major release,
//! not an argument for a dependency.
//!
//! ## What the documented API says about failure, and what it does not
//!
//! `sandbox_init(3)` documents exactly one return contract, shared by `sandbox_init` and
//! `sandbox_init_with_parameters`: on success `rc == 0` **and** the callee sets `*errorbuf` to
//! `NULL`; on failure `rc == -1` and the callee allocates a NUL-terminated diagnostic (which may
//! contain embedded newlines) into `*errorbuf`, and that pointer belongs to `sandbox_free_error`.
//! Nothing else is documented: no `errno` on this path, and no `SANDBOX_PROFILE_*` constant or
//! `sandbox_error_buffer` function exists in the SPI header, in Chromium's hand-declared copy of
//! these functions, or in any man page. So [`attempt_seatbelt`] maps `rc != 0` straight to a
//! refusal and reads the errorbuf as the only diagnostic the call gives, and nothing here ever
//! decodes `rc` as an errno or as a profile error code.
//!
//! Process-once. The same contract is also what a second call in one process would have to
//! report — failure, with a diagnostic in the errorbuf. The claim this module used to make, that
//! a later call "answers `EPERM`", is not backed by any retrieved document and has been dropped;
//! what is verified is that a Seatbelt profile applies once per process and cannot be widened or
//! replaced afterwards (the only documented widening mechanism is the sandbox-extension family).
//! [`attempt_seatbelt`] therefore calls the entry point **exactly once** and never retries:
//! profile layering (policy plus extras plus operation block) is text composition *before*
//! compilation, done by [`crate::harness::sbpl::render_profile`].
//!
//! The `flags` argument is a `uint64_t`. The only flag documented for this entry point
//! (`SANBOX_NAMED`) makes the argument name one of Apple's **built-in** profiles and is the
//! opposite of a string profile, and every real consumer passes `0`, so `SEATBELT_INIT_FLAGS`
//! is 0. The `SANDBOX_INIT_CHALLENGING_COMPAT` and `SANDBOX_INIT_DEBUGGING_COMPAT` names this
//! file used to cite appear in no man page, in no SPI header and in no consumer of either:
//! unverified, so they are no longer claimed anywhere.
//!
//! ## The oracle: `sandbox_check`, not a file probe
//!
//! Landlock's `restrict_self()` reports what the kernel actually enforces, while Seatbelt's
//! `rc == 0` only says the profile compiled and was installed. The missing evidence comes from
//! **`sandbox_check`** — the non-deprecated "is this operation allowed for this process" call,
//! the same one Chromium uses for its `IsSandboxed()`:
//!
//! * [`confinement`] asks the canonical question, `sandbox_check(getpid(), NULL, 0)`: `1` means
//!   this process is confined, `0` means it is not, anything else is no evidence at all.
//! * [`path_answer`] asks whether a read of each path the rendered profile denies explicitly —
//!   the SSH and GnuPG homes, from [`crate::harness::sbpl::SENSITIVE_DENIED_BASENAMES`] joined
//!   onto the same `$HOME` the profile was rendered with — by passing `SANDBOX_FILTER_PATH` and
//!   the path itself. `0` means the read is allowed, `1` means it is denied.
//! * [`verdict`] turns those two answers into the [`SandboxAttempt`]: confined **and** denied is
//!   `FullyEnforced`; confined but the read comes back allowed means the policy enforcing this
//!   run is not the policy we rendered, which is `EnforcementFailed` and therefore a refusal; an
//!   answer of "not confined" is the same refusal; and a `sandbox_check` return that is neither
//!   `1` nor `0` is `PartiallyEnforced`, accepted **plus a warning**, so enforcement is never
//!   claimed on an unrecognised value.
//!
//! The `$HOME/.ssh/config` `open(2)` probe this file used to rely on is gone, and with it the
//! three false paths it had: `HOME` unset, probe file absent, and an operator granting `$HOME`
//! through `MARMEL_SANDBOX_EXTRA_RO_PATHS` — a granted probe path used to flip a working profile
//! into `EnforcementFailed` and the run into exit 73. `sandbox_check` answers from the compiled
//! profile rather than from the filesystem, so no probe path has to exist and no operator grant
//! can reach a path the profile denies by name.
//!
//! ## What is still `[verify-on-Mac]`
//!
//! The path half of `sandbox_check`: `SANDBOX_FILTER_PATH` is the second value of Apple's
//! `enum sandbox_filter_type` and the path is one `char *` vararg. That spelling comes from the
//! SPI header and from Chromium's copy of the declaration — it was never observed on a Mac from
//! this repository, and CI's `Test (macos-latest)` leg is the first place it runs. A `0`/`1`
//! answer is what the verdict needs and everything else is treated as no evidence.
//!
//! Whether the emitted profile is actually the one in force is decided by the probe at runtime;
//! the breadth of the emitter's non-file allow-block is a floor, not a tuned policy, and too
//! narrow shows up as a denial **inside** the confined command, which this probe cannot see.
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

/// The `flags` argument of `sandbox_init_with_parameters`: zero, the value every real consumer
/// of this entry point passes (Chromium's `Seatbelt::InitWithParams`, painless-belt's
/// `sandbox_init`), because the profile states its own grants.
///
/// The only flag the man page documents for this entry point (`SANBOX_NAMED`) makes the
/// `profile` argument name one of Apple's built-in profiles (`kSBXProfileNoInternet` and
/// `kSBXProfileNoWriteExceptTemporary`), which is the opposite of the string profile this
/// backend hands over. The two compatibility-style names this constant used to carry
/// (`SANDBOX_INIT_CHALLENGING_COMPAT` and `SANDBOX_INIT_DEBUGGING_COMPAT`) are unverified:
/// absent from the man page, from the SPI header and from every consumer of it, so they are
/// deleted rather than merely unused. `0` is therefore the only value here that is supported.
const SEATBELT_INIT_FLAGS: u64 = 0;

/// `SANDBOX_FILTER_NONE` from Apple's `enum sandbox_filter_type`: the filter type the canonical
/// "am I sandboxed" call passes, and the one the callee ignores for that question.
const SANDBOX_FILTER_NONE: libc::c_int = 0;

/// `SANDBOX_FILTER_PATH`: the second value of the same enum, and the only filter type that makes
/// `sandbox_check` take a path. It is what turns "is this profile enforcing?" into "does the
/// profile in force deny reads of this path?".
const SANDBOX_FILTER_PATH: libc::c_int = 1;

/// The operation class the probe asks about: the same class the emitter uses when it denies the
/// sensitive directories, so an answer of `0` means "denied exactly as rendered".
const PROBE_OPERATION: &str = "file-read*";

// The libSystem symbols this backend needs: the entry point, the deallocator for the diagnostic
// it hands back (libSystem allocates it and the caller must not drop it), and the check call
// that answers what the entry point alone cannot.
//
// Declared by hand because `libc` does not ship these names: `Cargo.toml` needs no change,
// since `libc` is already a `cfg(unix)` dependency and libSystem is the default link on macOS.
//
// `rc` of the init entry point is `0` or `-1` only, with the diagnostic in the errorbuf; the
// `..._COMPAT` flags this file used to cite are unverified, and `flags = 0` is the only value
// used here. `sandbox_check` is variadic: its fixed arguments are as declared, and a path filter
// passes exactly one `char *` more.
unsafe extern "C" {
    fn sandbox_init_with_parameters(
        profile: *const libc::c_char,
        flags: u64,
        parameters: *const *const libc::c_char,
        errorbuf: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sandbox_free_error(errorbuf: *mut libc::c_char);
    fn sandbox_check(
        pid: libc::pid_t,
        operation: *const libc::c_char,
        filter_type: libc::c_int,
        ...
    ) -> libc::c_int;
}

/// Test-only knob: when set to an unambiguous truthy value, the next [`attempt_seatbelt`] hands
/// libSystem a deliberately unparsable profile instead of the rendered one.
///
/// It exists so a real macOS host can be driven into [`SandboxAttempt::EnforcementFailed`] by a
/// profile libSystem refuses to compile, and the end-to-end spine "profile rejected -> refusal ->
/// exit [`crate::harness::sandbox::SANDBOX_REFUSE_EXIT_CODE`]" can be exercised there rather than
/// asserted only from the pure mapping in `sandbox::reentry_action`. Nothing in production sets
/// it, and no other code reads it.
pub const SEATBELT_TEST_BAD_PROFILE_ENV: &str = "MARMEL_SEATBELT_TEST_BAD_PROFILE";

/// The clause that makes the knob's profile text unparsable: an unterminated `subpath` filter,
/// i.e. SBPL text the parser must reject instead of compile.
const BROKEN_PROFILE_TAIL: &str = "\n(deny file-read* (subpath \"unterminated\n";

/// `true` when the test knob is switched on: the raw value it carries, run through the same
/// fail-closed parsing as the sandbox opt-out ([`crate::harness::pty::is_optout_value`]), so
/// `0`, `false`, an empty value or a typo keep the real profile in place.
#[must_use]
fn bad_profile_requested() -> bool {
    bad_profile_from_value(std::env::var(SEATBELT_TEST_BAD_PROFILE_ENV).ok().as_deref())
}

/// Pure half of [`bad_profile_requested`], so the accepted values are testable without touching
/// the environment of a running test process.
#[must_use]
fn bad_profile_from_value(raw: Option<&str>) -> bool {
    raw.is_some_and(crate::harness::pty::is_optout_value)
}

/// The text handed to libSystem, with the deliberate break applied when `requested` says the
/// knob is on. The break is append-only, so the policy clauses the rendered profile carries stay
/// untouched; only the tail makes the text fail to compile.
#[must_use]
fn break_profile_for_test_env(text: String, requested: bool) -> String {
    if !requested {
        return text;
    }
    // The re-entry child has no logging subscriber yet, so a knob that changes what libSystem
    // sees must never be confused with a policy decision: name it out loud.
    tracing::warn!(
        "{SEATBELT_TEST_BAD_PROFILE_ENV} is set: handing libSystem a deliberately unparsable \
         Seatbelt profile (test knob only, no production run sets it)"
    );
    format!("{text}{BROKEN_PROFILE_TAIL}")
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
/// to libSystem **once**, then prove enforcement with `sandbox_check`.
#[must_use = "the Seatbelt attempt decides whether the re-entry child may exec at all"]
pub fn attempt_seatbelt(workspace_root: &Path, extra_readonly_paths: &[String]) -> SandboxAttempt {
    // The workspace root is the only required policy entry (see `policy_paths`): if it does not
    // exist or cannot be accessed, the policy cannot be granted. Fail closed before installing
    // anything, exactly like Landlock's `PathFd::new` check.
    if let Err(err) = std::fs::metadata(workspace_root) {
        return SandboxAttempt::RuleFailed {
            path: workspace_root.display().to_string(),
            reason: format!("opening workspace root: {err}"),
        };
    }

    // 1. One flat profile. Seatbelt has no reliable `import` across a string boundary, and the
    //    call below cannot be repeated, so every grant and every denial has to be in the text
    //    already. [`break_profile_for_test_env`] may replace this text with a deliberately
    //    unparsable variant, and only while its test-only knob is switched on.
    let rendered = sbpl::render_profile(workspace_root, extra_readonly_paths);
    let broken = bad_profile_requested();
    let profile = match CString::new(break_profile_for_test_env(rendered, broken)) {
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
        // The documented failure shape: `rc == -1` and a diagnostic in `errorbuf`. `rc` is never
        // decoded any further, because nothing documents it as anything else.
        let diagnostic = take_error_text(errorbuf);
        return SandboxAttempt::EnforcementFailed {
            reason: init_failure_reason(rc, diagnostic.as_deref()),
        };
    }

    // The documented success shape is `rc == 0` **and** `*errorbuf == NULL`, so there is nothing
    // to free here. A non-null buffer on a successful call is not documented but leaks the same
    // allocation libSystem owns, so it is read for whatever diagnostic it holds and released
    // exactly once, never touched again.
    if !errorbuf.is_null()
        && let Some(text) = take_error_text(errorbuf)
    {
        tracing::debug!("Seatbelt: libSystem left an errorbuf on a successful init: {text}");
    }

    // 3. `rc == 0` proves the profile compiled and was installed — not that it is enforcing.
    //    `sandbox_check` decides that, and decides it from the compiled profile rather than from
    //    anything on disk.
    let confined = confinement();
    let probes = probe_denials();
    let attempt = verdict(confined, &probes);
    match &attempt {
        SandboxAttempt::FullyEnforced => {
            let unproven = probes
                .iter()
                .filter(|probe| !matches!(probe.answer, PathAnswer::Denied))
                .count();
            if unproven == 0 {
                tracing::debug!(
                    "Seatbelt: profile active ({} bytes), confinement and the denied paths \
                     confirmed by sandbox_check",
                    profile.as_bytes().len(),
                );
            } else {
                tracing::warn!(
                    "Seatbelt: confinement confirmed by sandbox_check, but {unproven} of {} \
                     probe path(s) gave no usable answer (profile {} bytes)",
                    probes.len(),
                    profile.as_bytes().len(),
                );
            }
        }
        SandboxAttempt::PartiallyEnforced { reason } => tracing::warn!("Seatbelt: {reason}"),
        _ => {}
    }
    attempt
}

/// What one path probe answered, and which path it asked about.
#[derive(Debug, PartialEq, Eq)]
struct Probe {
    path: PathBuf,
    answer: PathAnswer,
}

/// The answer `sandbox_check` gave about one path and one operation.
#[derive(Debug, PartialEq, Eq)]
enum PathAnswer {
    /// The call answered `0`: the profile in force **allows** the read of that path.
    Allowed,
    /// The call answered `1`: the read is denied — the evidence the verdict wants to see.
    Denied,
    /// An answer that is neither `0` nor `1`, or a path that could not be handed to the callee.
    /// Never confused with a denial: this is the "no evidence" case.
    Unavailable { reason: String },
}

/// Whether this process is confined at all, from the canonical and non-deprecated form of
/// `sandbox_check`: `sandbox_check(getpid(), NULL, SANDBOX_FILTER_NONE)` answers `1` when the
/// process is sandboxed (the shape Chromium's `IsSandboxed()` uses) and `0` when it is not.
///
/// `None` means the call answered something other than `0` or `1`, which is not documented as
/// either answer and is therefore treated as no evidence rather than as a guess.
#[must_use]
fn confinement() -> Option<bool> {
    let rc = unsafe {
        // SAFETY: three fixed arguments and no varargs; a NULL `operation` is the documented
        // "am I sandboxed" form, and `filter_type` is ignored by the callee for that question.
        sandbox_check(libc::getpid(), std::ptr::null(), SANDBOX_FILTER_NONE)
    };
    match rc {
        1 => Some(true),
        0 => Some(false),
        other => {
            tracing::debug!("Seatbelt: sandbox_check confinement answer {other} is not 0 or 1");
            None
        }
    }
}

/// The paths the enforcement probe asks about: [`crate::harness::sbpl::SENSITIVE_DENIED_BASENAMES`]
/// joined onto the same `$HOME` the profile was rendered with, i.e. exactly the paths the
/// rendered profile denies. One table on both sides means the probe can never ask about a path
/// the profile does not deny; and because `sandbox_check` answers from the compiled profile, the
/// paths do not have to exist on the host.
#[must_use]
fn probe_denials() -> Vec<Probe> {
    let Some(home) = sbpl::home_from_env() else {
        // No home in the environment means no home-relative clause in the profile either, so
        // there is nothing to ask about — which can never demote the confinement answer.
        return Vec::new();
    };

    sbpl::SENSITIVE_DENIED_BASENAMES
        .iter()
        .map(|base| {
            let path = home.join(base);
            let answer = path_answer(&path);
            Probe { path, answer }
        })
        .collect()
}

/// Ask libSystem whether `path` may be read under the profile in force: the path is passed with
/// the `SANDBOX_FILTER_PATH` filter, as the single `char *` vararg the filter implies.
#[must_use]
fn path_answer(path: &Path) -> PathAnswer {
    let cpath = match CString::new(path.as_os_str().as_bytes()) {
        Ok(cpath) => cpath,
        Err(err) => {
            return PathAnswer::Unavailable {
                reason: format!(
                    "{}: cannot be passed to sandbox_check: {err}",
                    path.display()
                ),
            };
        }
    };
    let operation = match CString::new(PROBE_OPERATION) {
        Ok(operation) => operation,
        Err(err) => {
            return PathAnswer::Unavailable {
                reason: format!("{PROBE_OPERATION}: cannot be passed to sandbox_check: {err}"),
            };
        }
    };

    let rc = unsafe {
        // SAFETY: the fixed arguments are as declared, the path is the single `char *` vararg a
        // path filter takes, and both `CString`s stay alive across the call.
        sandbox_check(
            libc::getpid(),
            operation.as_ptr(),
            SANDBOX_FILTER_PATH,
            cpath.as_ptr(),
        )
    };

    match rc {
        0 => PathAnswer::Allowed,
        1 => PathAnswer::Denied,
        other => PathAnswer::Unavailable {
            reason: format!(
                "sandbox_check({PROBE_OPERATION}, {}, {}) returned {other}, which is neither 0 \
                 (allowed) nor 1 (denied)",
                SANDBOX_FILTER_PATH,
                path.display(),
            ),
        },
    }
}

/// What the confinement answer and the path answers together say about this process.
///
/// Fail-closed and evidence-first, in this order:
///
/// * a path the rendered profile denies comes back `Allowed` -> [`SandboxAttempt::EnforcementFailed`]:
///   the policy in force is not the policy that was rendered, which is a refusal and never a
///   warning (this is the only case that can reach exit 73 for a **policy** reason);
/// * `sandbox_check` answers that the process is **not** confined -> the same refusal, because a
///   profile that installed without confining anything is exactly the failure this attempt has to
///   catch;
/// * confined, with no `Allowed` among the answers -> [`SandboxAttempt::FullyEnforced`]: the
///   confinement answer is direct evidence, so a probe that could not be carried out (no `$HOME`,
///   no usable answer) neither demotes it nor refuses it;
/// * confinement unknown -> [`SandboxAttempt::PartiallyEnforced`]: accepted **plus a warning**,
///   because enforcement is never claimed on a value that is not `0` or `1`.
#[must_use]
fn verdict(confined: Option<bool>, probes: &[Probe]) -> SandboxAttempt {
    let allowed: Vec<String> = probes
        .iter()
        .filter(|probe| probe.answer == PathAnswer::Allowed)
        .map(|probe| probe.path.display().to_string())
        .collect();
    if !allowed.is_empty() {
        return SandboxAttempt::EnforcementFailed {
            reason: format!(
                "Seatbelt installed a profile that does not deny a read of \
                 {allowed} (sandbox_check({PROBE_OPERATION}, filter {SANDBOX_FILTER_PATH} \
                 answered 0): the policy enforcing this run is not the policy it rendered",
                allowed = allowed.join(", "),
            ),
        };
    }

    match confined {
        Some(true) => SandboxAttempt::FullyEnforced,
        Some(false) => SandboxAttempt::EnforcementFailed {
            reason: format!(
                "Seatbelt reported a successful init, but sandbox_check(getpid(), NULL, \
                 {SANDBOX_FILTER_NONE}) answered that this process is not confined: nothing \
                 restricts it"
            ),
        },
        None => {
            let unusable: Vec<String> = probes
                .iter()
                .filter(|probe| matches!(probe.answer, PathAnswer::Unavailable { .. }))
                .map(|probe| probe.path.display().to_string())
                .collect();
            let scope = if probes.is_empty() {
                "no probe path exists to ask about, because the profile carries no \
                 home-relative clause"
                    .to_string()
            } else if unusable.is_empty() {
                "every probe path answered that its read was denied".to_string()
            } else {
                format!("{} gave no usable answer", unusable.join(", "))
            };
            SandboxAttempt::PartiallyEnforced {
                reason: format!(
                    "Seatbelt installed the profile, but enforcement was not confirmed by probe: \
                     sandbox_check(getpid(), NULL, {SANDBOX_FILTER_NONE}) returned a value that \
                     is neither 1 (confined) nor 0 (not confined), and {scope}, so enforcement \
                     rests on sandbox_init_with_parameters alone",
                ),
            }
        }
    }
}

/// The libSystem diagnostic that came with a failed init, read out of the buffer it allocated.
///
/// The buffer is libSystem's to free, so it is read first and given to `sandbox_free_error`
/// afterwards, and no code may touch it past that point. A `NULL` buffer is tolerated (the API
/// sets the pointer to `NULL` on success, and a non-zero `rc` with no diagnostic is a real
/// possibility), and a non-UTF-8 message is kept as far as `to_string_lossy` allows rather than
/// dropped.
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
             returned no diagnostic in its errorbuf. A Seatbelt profile applies once per \
             process, so an already confined process cannot be narrowed or widened by a second \
             call."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(path: &str, answer: PathAnswer) -> Probe {
        Probe {
            path: PathBuf::from(path),
            answer,
        }
    }

    fn unavailable() -> PathAnswer {
        PathAnswer::Unavailable {
            reason: "not my sandbox".to_string(),
        }
    }

    fn partial_reason(attempt: SandboxAttempt) -> String {
        match attempt {
            SandboxAttempt::PartiallyEnforced { reason } => reason,
            other => panic!("must be an accepted-but-warning attempt, got {other:?}"),
        }
    }

    /// The primary oracle: the confinement answer decides enforcement on its own, and a denied
    /// path answer confirms it. Both halves of the evidence are read from `sandbox_check`, never
    /// from what happens to exist on disk.
    #[test]
    fn test_sandbox_check_decides_the_verdict() {
        assert_eq!(
            verdict(
                Some(true),
                &[
                    probe("/Users/ann/.ssh", PathAnswer::Denied),
                    probe("/Users/ann/.gnupg", PathAnswer::Denied),
                ],
            ),
            SandboxAttempt::FullyEnforced,
            "confined plus a denied path is the evidence enforcement wants"
        );
        assert_eq!(
            verdict(Some(true), &[]),
            SandboxAttempt::FullyEnforced,
            "the confinement answer alone is direct evidence: an absent probe never demotes it"
        );
        assert_eq!(
            verdict(
                Some(true),
                &[probe(
                    "/Users/ann/.ssh",
                    PathAnswer::Unavailable {
                        reason: "answered 42".to_string()
                    }
                )],
            ),
            SandboxAttempt::FullyEnforced,
            "an unusable path answer neither claims nor refuses enforcement"
        );
    }

    /// `sandbox_check` answering `0` for a path the profile denies is a policy failure, not a
    /// warning: the run must be refused, and the reason must say which path read as allowed and
    /// which call said so.
    #[test]
    fn test_allowed_path_answer_refuses_the_run() {
        for confined in [Some(true), Some(false), None] {
            let attempt = verdict(
                confined,
                &[
                    probe("/Users/ann/.ssh", PathAnswer::Denied),
                    probe("/Users/ann/.gnupg", PathAnswer::Allowed),
                ],
            );
            match attempt {
                SandboxAttempt::EnforcementFailed { reason } => {
                    assert!(
                        reason.contains("/Users/ann/.gnupg"),
                        "the refusal must name the path that read as allowed: {reason}"
                    );
                    assert!(
                        reason.contains("sandbox_check"),
                        "the refusal must name the call that answered: {reason}"
                    );
                }
                other => panic!("must refuse, got {other:?}"),
            }
        }
    }

    /// The other refusal: an answer of "not confined" is not a partial success, whatever the
    /// path probes said.
    #[test]
    fn test_unconfined_answer_is_an_enforcement_failure() {
        for probes in [
            Vec::new(),
            vec![probe("/Users/ann/.ssh", PathAnswer::Denied)],
            vec![probe("/Users/ann/.ssh", unavailable())],
        ] {
            let attempt = verdict(Some(false), &probes);
            match attempt {
                SandboxAttempt::EnforcementFailed { reason } => {
                    assert!(
                        reason.contains("not confined"),
                        "the refusal must say what was answered: {reason}"
                    );
                    assert!(
                        reason.contains("sandbox_check"),
                        "the refusal must name the call: {reason}"
                    );
                }
                other => panic!("must refuse, got {other:?}"),
            }
        }
    }

    /// A `sandbox_check` answer that is neither `0` nor `1` proves nothing, so it must land on
    /// the variant the caller accepts **with a warning** and never on `FullyEnforced`.
    #[test]
    fn test_unknown_confinement_answer_never_claims_enforcement() {
        let reason = partial_reason(verdict(
            None,
            &[
                probe("/Users/ann/.ssh", PathAnswer::Denied),
                probe("/Users/ann/.gnupg", PathAnswer::Denied),
            ],
        ));
        assert!(
            reason.contains("not confirmed"),
            "an unverified attempt must say so: {reason}"
        );

        // The reason also names whatever could not be asked about, and says so plainly when
        // there was nothing to ask.
        let with_paths = partial_reason(verdict(
            None,
            &[
                probe("/Users/ann/.ssh", unavailable()),
                probe("/Users/ann/.gnupg", PathAnswer::Denied),
            ],
        ));
        assert!(
            with_paths.contains("/Users/ann/.ssh"),
            "the reason must name the probe it could not carry out: {with_paths}"
        );
        let empty = partial_reason(verdict(None, &[]));
        assert!(
            empty.contains("home-relative"),
            "an absent probe list must be explained: {empty}"
        );
    }

    /// The probe paths are the emitter's denied basenames joined onto the same `$HOME`, so the
    /// probe can never ask about a path the rendered profile does not deny, and never hard-codes
    /// a user or a file name.
    #[test]
    fn test_probe_paths_are_the_profile_denied_basenames() {
        let home = Path::new("/Users/ann");
        let expected: Vec<PathBuf> = sbpl::SENSITIVE_DENIED_BASENAMES
            .iter()
            .map(|base| home.join(base))
            .collect();
        assert_eq!(
            expected,
            vec![
                PathBuf::from("/Users/ann/.ssh"),
                PathBuf::from("/Users/ann/.gnupg")
            ],
            "the shared table decides both sides of the probe: an .ssh/config file is not a \
             probe path and need not exist"
        );
        for path in sbpl::SENSITIVE_DENIED_BASENAMES {
            assert!(
                !path.starts_with('/') && !path.contains("$HOME"),
                "{path} must be joinable onto $HOME rather than hard-coded to one user"
            );
        }
        // The operation the probe asks about must be the operation the emitter denies, or the
        // answer means nothing.
        assert_eq!(PROBE_OPERATION, "file-read*");
    }

    /// The attempt must be built on the shared emitter, and the profile it hands to libSystem
    /// must contain the denials the probe then asks about: one source of truth for the allow-list
    /// and for the denial table, pinned here because no Linux host can exercise this backend.
    #[test]
    fn test_attempt_is_wired_to_the_shared_policy_and_asks_about_the_denied_paths() {
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
        assert!(
            src.contains("fn sandbox_check("),
            "the confinement and path probes must go through sandbox_check"
        );

        let production = src.split("#[cfg(test)]").next().unwrap_or(&src);
        assert_eq!(
            production.matches("sandbox_init_with_parameters(").count(),
            2,
            "sandbox_init must be declared once and called exactly once per attempt \
             (Seatbelt is process-once)"
        );
        assert_eq!(
            production.matches("sandbox_free_error(").count(),
            2,
            "the errorbuf must be released in exactly one place, so it can never be freed twice"
        );
        assert!(
            production.contains("fn confinement()")
                && production.contains("fn path_answer(")
                && production.contains("SANDBOX_FILTER_PATH"),
            "the confinement probe and the path probe must both stay in place, each one calling \
             through sandbox_check"
        );
        assert!(
            !production.contains("landlock::"),
            "the Seatbelt backend must not grow a Landlock dependency"
        );
        assert!(
            !production.contains("classify_errno") && !production.contains("probe_open"),
            "the file-existence oracle (and its errno classification) must stay gone: enforcement \
             rests on sandbox_check"
        );
    }

    /// The text the attempt hands to libSystem is the rendered policy, not a hard-coded profile:
    /// its head, its workspace clause and the denials the probe asks about all come from the
    /// shared emitter.
    #[test]
    fn test_profile_handed_to_libsystem_denies_the_probe_paths() {
        let root = Path::new("/work/sandbox root");
        let profile = sbpl::render_profile(root, &[]);
        assert!(profile.starts_with("(version 1)"), "head: {profile}");
        assert!(profile.contains("(deny default)"), "baseline: {profile}");
        assert!(
            profile.contains("(allow file-read* (subpath \"/work/sandbox root\"))"),
            "the workspace root must be granted in the profile handed to sandbox_init: {profile}"
        );

        let denials = profile
            .lines()
            .filter(|line| line.starts_with("(deny file-read* (subpath"))
            .collect::<Vec<_>>();
        let expected = sbpl::SENSITIVE_DENIED_BASENAMES.len();
        assert_eq!(
            denials.len(),
            expected,
            "one explicit denial per sensitive directory, or the probe asks about a path the \
             profile never denied: {profile}"
        );
        if let Some(home) = sbpl::home_from_env() {
            for base in sbpl::SENSITIVE_DENIED_BASENAMES {
                let clause = format!(
                    "(deny file-read* (subpath {}))",
                    sbpl::quote_path(&home.join(base))
                );
                assert!(
                    profile.contains(&clause),
                    "the probe premise is that the profile denies {clause}: {profile}"
                );
            }
        }
    }

    /// A failed init always carries a non-empty reason naming the call, with or without a
    /// libSystem diagnostic, and never decodes `rc` into anything more specific than what the
    /// documented contract supports (`0` or `-1` plus an errorbuf).
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
            assert!(
                !without.contains("EPERM"),
                "the reason must not claim an errno that the API does not document: {without}"
            );
        }
        let with = init_failure_reason(-1, Some("syntax error at line 4"));
        assert!(with.contains("syntax error at line 4"), "{with}");
        assert!(with.contains("rc -1"), "{with}");
    }

    /// The knob that drives a real macOS run into a refusal must stay off for anything but an
    /// unambiguous truthy value — the same fail-closed parsing the sandbox opt-out uses — so a
    /// typo or a falsy value can never silently hand libSystem a broken profile.
    #[test]
    fn test_bad_profile_knob_parses_like_the_optout() {
        for value in ["1", "true", "TRUE", "yes", "on", " 1 "] {
            assert!(
                bad_profile_from_value(Some(value)),
                "{value:?} must switch the knob on"
            );
        }
        for value in ["", "0", "false", "no", "off", "maybe", "1x", "test"] {
            assert!(
                !bad_profile_from_value(Some(value)),
                "{value:?} must leave the knob off"
            );
        }
        assert!(
            !bad_profile_from_value(None),
            "an unset knob must leave the rendered profile exactly as the emitter made it"
        );
    }

    /// The break must be append-only: the policy text the emitter rendered stays in place (so a
    /// refusal under the knob proves the *tail* is what libSystem rejected, never a missing
    /// clause), and the tail must be text the SBPL parser cannot accept.
    #[test]
    fn test_bad_profile_break_is_an_unparsable_tail_and_only_a_tail() {
        let rendered = "(version 1)\n(deny default)\n(allow file-read* (subpath \"/work\"))\n";
        let broken = break_profile_for_test_env(rendered.to_string(), true);

        assert_eq!(
            rendered,
            broken
                .strip_suffix(BROKEN_PROFILE_TAIL)
                .expect("the break must be an appended tail"),
            "the break must not touch the rendered policy text"
        );
        assert!(
            broken.matches('(').count() > broken.matches(')').count(),
            "the tail must leave an unbalanced clause behind: {broken}"
        );
        assert_eq!(
            broken.matches('"').count() % 2,
            1,
            "the tail must leave an unterminated string literal behind: {broken}"
        );

        assert_eq!(
            break_profile_for_test_env(rendered.to_string(), false),
            rendered,
            "an off knob must not change the profile one way or the other"
        );
    }

    /// The diagnostic for a failed init is read before it is freed, and an empty or absent message still leaves a
    /// usable reason rather than `None` pretending to be success. The buffer belongs to
    /// libSystem, so the test keeps its own allocation out of the way rather than freeing it
    /// twice.
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
        // The same hand-off rule for an empty diagnostic: read it, free it once, and keep the
        // test's own allocation from freeing it again.
        let empty = CString::new("").expect("empty diagnostic");
        let empty_ptr = empty.as_ptr().cast_mut();
        std::mem::forget(empty);
        assert_eq!(
            take_error_text(empty_ptr),
            None,
            "an empty diagnostic is no diagnostic"
        );
    }
}
