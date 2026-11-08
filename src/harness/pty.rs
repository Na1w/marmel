//! PTY wrapper: shell execution with process-group isolation.
//!
//! `run_command` wraps every command as:
//! ```text
//! sh -c "stty -echo; ulimit -f 4194304 2>/dev/null || ulimit -f 2097152 2>/dev/null; <command>"
//! ```
//! `stty -echo` stops the echoed command line from polluting stdout; `ulimit -f` caps
//! each command's file write to 2 GiB (1 GiB fallback).
//!
//! On Windows a one-shot `run_command` does **not** use a PTY at all: ConPTY closes its
//! output pipe only during `ClosePseudoConsole`, i.e. after the read loop, so a PTY reader
//! there never sees EOF and every command could only ever time out with its output
//! discarded. The Windows branch spawns `%COMSPEC% /C <command>` with
//! `std::process::Command`, terminates on **child exit**, reports the exit status and
//! kills the whole tree with `taskkill /F /T` on timeout or cancellation. Interactive
//! `pty_spawn` sessions keep using portable-pty on both platforms.
//!
//! Completion, timeout (strict 300 s) and teardown all issue `libc::kill(-pid,
//! libc::SIGKILL)` to the *entire process group* (negative pid), then reap the child
//! exactly once through one idempotent, bounded `try_wait()` path (a child still running
//! after the grace window goes to the bounded [`DeferredReapQueue`]), so no `<defunct>`
//! entry can accumulate and exhaust the process table (`EAGAIN`). Interactive output is a
//! bounded ring of [`PTY_OUTPUT_BUFFER_CAP`] bytes (256 KiB): an over-cap unread tail loses
//! its *head* at a UTF-8 char boundary, counted and reported ([`truncation_notice`]).
//!
//! Whether the sandbox applies never depends on the executable's **name**: [`should_apply_sandbox`]
//! decides from the resolved executable path (`current_exe()` canonicalized), platform support and an
//! explicit opt-out; it is fail-closed, every skip is warned about, and an unresolvable exe hard-fails.

use crate::harness::{ToolError, ToolResult};
use crate::tool_names::{
    TOOL_PTY_CLOSE, TOOL_PTY_READ, TOOL_PTY_SPAWN, TOOL_PTY_WRITE, TOOL_RUN_COMMAND,
};
use portable_pty::{CommandBuilder, ExitStatus, PtySize, native_pty_system};
use regex::Regex;
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

/// Default per-command timeout in seconds (default 60 s, max 300 s).
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Hard byte cap of the interactive output ring (`SharedBuffer.output`), in bytes: **256 KiB**
/// (`256 * 1024` = 262 144 B). The ring is a window, not a log: consumed bytes are reclaimed on every push,
/// and if the *unread* tail still exceeds this cap the oldest bytes are dropped at a UTF-8 char boundary
/// and the loss is counted and reported to the next reader, so a chatty command (`yes`, `tail -f`, a build
/// log) cannot grow the process without limit. Enforced only by [`SharedBuffer::trim_to_cap`], called from
/// [`SharedBuffer::push`].
pub const PTY_OUTPUT_BUFFER_CAP: usize = 256 * 1024;

/// How long a `SIGKILL`ed one-shot command (the `run_command` path) is polled
/// with `try_wait()` before the reaper falls back to the blocking `wait()`.
pub const PTY_REAP_GRACE: Duration = Duration::from_millis(500);

/// Same grace for interactive-session teardown. Kept short because that reap
/// runs on an async worker thread while the session map lock is held; a child
/// that has not exited within it is handed to [`DeferredReapQueue`] instead of
/// blocking the runtime.
pub const PTY_REAP_GRACE_INTERACTIVE: Duration = Duration::from_millis(100);

/// Poll interval of the bounded `try_wait()` reap loop.
const PTY_REAP_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Upper bound of [`DeferredReapQueue`]: one entry per `SIGKILL`ed child that
/// refuses to report an exit status (in practice only a child stuck in
/// uninterruptible sleep). Bounded on purpose — memory must not grow with
/// session count; the oldest handle is retired with a warning on overflow.
const DEFERRED_REAP_QUEUE_CAP: usize = 64;

/// Hard file-size cap (`ulimit -f`) applied to every command, in 512-byte blocks.
/// 4194304 blocks × 512 B = 2 GiB ceiling; this matches marmennill-cli's `ulimit -f`.
pub const ULIMIT_FILE_BLOCKS: &str = "4194304";

/// Fallback file-size cap for platforms that reject the 2 GiB limit.
/// 2097152 blocks × 512 B = 1 GiB ceiling.
pub const ULIMIT_FILE_BLOCKS_FALLBACK: &str = "2097152";

/// Strip OSC sequences and other non-printable terminal artifacts from captured output:
/// OSC sequences (`ESC ] <n> ; ... BEL` or `ESC ] <n> ; ... ESC \`) and bell (`\x07`) /
/// backspace (`\x08`) are removed, as are other control characters below ` ` (0x20)
/// except `\n`, `\r`, `\t` and `\x1b` (ESC, preserved so CSI color codes survive).
static RE_OSC: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\x1b\][0-9]+;.*?(?:\x07|\x1b\\)").expect("valid OSC regex")
});

pub fn sanitize_terminal_output(text: &str) -> String {
    let cleaned = RE_OSC.replace_all(text, "");
    cleaned
        .chars()
        .filter(|&c| {
            c != '\x07'
                && c != '\x08'
                && (c >= ' ' || c == '\n' || c == '\r' || c == '\t' || c == '\x1b')
        })
        .collect()
}

// Child reaping — every spawned child is `wait()`ed exactly once

/// The wait/kill surface the reaper needs from a spawned child.
///
/// [`PtyChildHandle`] adapts the handle returned by `portable_pty` to it. The
/// test module implements it for a plain `std::process::Child` (spawned without
/// a pty) and for a fake, so the reap state machine can be unit-tested as pure
/// logic without a real `/dev/pts`.
pub trait ChildHandle {
    /// Non-blocking poll for the exit status; `Ok(None)` while still running.
    fn try_wait_child(&mut self) -> std::io::Result<Option<ExitStatus>>;
    /// Blocking wait for the exit status.
    fn wait_child(&mut self) -> std::io::Result<ExitStatus>;
    /// Terminate the child (`SIGKILL` on unix).
    fn kill_child(&mut self) -> std::io::Result<()>;
}

/// Production [`ChildHandle`]: the `portable_pty` child of a spawned session.
pub struct PtyChildHandle {
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl ChildHandle for PtyChildHandle {
    fn try_wait_child(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn wait_child(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait()
    }

    fn kill_child(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }
}

/// Exactly-once reap bookkeeping for one spawned child. A child is waited on at most once
/// (a second `wait` on a reaped pid could otherwise block forever or hit a recycled pid),
/// its exit status is recorded once, and a handle is never dropped while the child is
/// still unreaped: the session types either reap it via [`ChildReaper::kill_and_reap`] or
/// hand it to a [`DeferredReapQueue`] via [`ChildReaper::defer_to`].
pub struct ChildReaper {
    /// `None` once the child has been reaped or handed to a [`DeferredReapQueue`].
    handle: Option<Box<dyn ChildHandle + Send>>,
    /// Process-group leader pid on unix (our spawns make the child the leader),
    /// `None` when no group signalling is possible.
    pid: Option<i32>,
    /// Exit status recorded by the reaper; `None` while running or when the
    /// `wait` syscall itself failed.
    exit_status: Option<ExitStatus>,
    reaped: bool,
}

/// What [`ChildReaper::kill_and_reap`] did.
pub struct ReapOutcome {
    /// Result of the `SIGKILL` sent to the process group; `Ok` also when the
    /// group was already gone (`ESRCH`) or when the child was already reaped
    /// (then nothing is signalled again).
    pub group: Result<(), std::io::Error>,
    /// Exit status of the direct child, when known.
    pub status: Option<ExitStatus>,
    /// `true` when the child is reaped — it must never be signalled by pid or
    /// waited on again, because the kernel may already have recycled the pid.
    pub reaped: bool,
}

impl ChildReaper {
    pub fn new(handle: impl ChildHandle + Send + 'static, pid: Option<i32>) -> Self {
        ChildReaper {
            handle: Some(Box::new(handle)),
            pid,
            exit_status: None,
            reaped: false,
        }
    }

    /// `true` once the child has been reaped (idempotence guard for teardown).
    pub fn is_reaped(&self) -> bool {
        self.reaped
    }

    /// Exit status recorded by the reaper (`None` while running/unreaped).
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit_status.clone()
    }

    /// Non-blocking: reap the child now if it already exited.
    ///
    /// Called by the `PtyManager` janitor task, so interactive children that
    /// exit on their own (`exit`, Ctrl-D, a finished REPL) are removed from the
    /// process table right away instead of lingering as `<defunct>` until the
    /// session is closed.
    pub fn reap_if_exited(&mut self) -> Option<ExitStatus> {
        if self.reaped {
            return self.exit_status.clone();
        }
        let handle = self.handle.as_mut()?;
        match handle.try_wait_child() {
            Ok(Some(status)) => {
                self.reaped = true;
                self.exit_status = Some(status.clone());
                Some(status)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("reap: try_wait for pty child {:?} failed: {e}", self.pid);
                self.reaped = true;
                None
            }
        }
    }

    /// `SIGKILL` the whole process group (negative pid, so backgrounded subshells and REPLs die too), then
    /// the child, then reap the child exactly once. Already reaped ⇒ pure no-op: nothing is signalled (the
    /// pid may have been recycled) and nothing is waited on again, so repeated completion/timeout/
    /// cancellation/`Drop` teardown passes stay safe. The `try_wait()` poll is bounded by `grace`; afterwards
    /// `wait_after_grace` picks the blocking `wait()` (one-shot `run_command`, whose child is already
    /// `SIGKILL`ed) or `reaped: false` for the interactive path, deferring via [`ChildReaper::defer_to`].
    pub fn kill_and_reap(&mut self, grace: Duration, wait_after_grace: bool) -> ReapOutcome {
        if self.reaped {
            return ReapOutcome {
                group: Ok(()),
                status: self.exit_status.clone(),
                reaped: true,
            };
        }

        // `kill_process_group` is the single unix `kill(-pid, SIGKILL)` site (and
        // a no-op off unix); ESRCH — the group is already gone — maps to `Ok`.
        let group = self.pid.map(kill_process_group).unwrap_or_else(|| Ok(()));
        if let Err(e) = &group {
            tracing::warn!("reap: kill_process_group({:?}) failed: {e}", self.pid);
        }
        if let Some(handle) = self.handle.as_mut() {
            let _ = handle.kill_child();
        }

        let status = self.reap_bounded(grace, wait_after_grace);
        ReapOutcome {
            group,
            status,
            reaped: self.reaped,
        }
    }

    /// Bounded `try_wait()` poll with an optional blocking fallback. A `wait`
    /// error (e.g. `ECHILD`, the child is already gone from the process table)
    /// is treated as "reaped" so the handle is retired instead of waited on
    /// a second time.
    fn reap_bounded(&mut self, grace: Duration, wait_after_grace: bool) -> Option<ExitStatus> {
        if self.reaped {
            return self.exit_status.clone();
        }
        let deadline = std::time::Instant::now() + grace;
        loop {
            let Some(handle) = self.handle.as_mut() else {
                // Already handed off to a deferred queue: nothing left to wait on.
                return None;
            };
            match handle.try_wait_child() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    self.exit_status = Some(status.clone());
                    return Some(status);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("reap: try_wait for pty child {:?} failed: {e}", self.pid);
                    self.reaped = true;
                    return None;
                }
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(PTY_REAP_POLL_INTERVAL);
        }

        if !wait_after_grace {
            return None;
        }
        let handle = self.handle.as_mut()?;
        match handle.wait_child() {
            Ok(status) => {
                self.reaped = true;
                self.exit_status = Some(status.clone());
                Some(status)
            }
            Err(e) => {
                tracing::warn!("reap: wait for pty child {:?} failed: {e}", self.pid);
                self.reaped = true;
                None
            }
        }
    }

    /// Hand a still-running (already `SIGKILL`ed) handle to `queue`, which the
    /// single `PtyManager` janitor task drains. Guarantees the child is still
    /// reaped exactly once instead of being dropped unreaped — and does it
    /// without spawning a reaper thread per command.
    pub fn defer_to(&mut self, queue: &mut DeferredReapQueue) -> bool {
        let Some(handle) = self.handle.take() else {
            return false;
        };
        queue.push(handle);
        true
    }
}

impl Drop for ChildReaper {
    fn drop(&mut self) {
        // Tripwire for a future regression: the session types must always reap
        // or defer before their reaper goes away.
        if self.handle.is_some() && !self.reaped {
            tracing::warn!(
                "pty child {:?} handle dropped without being reaped",
                self.pid
            );
        }
    }
}

/// Bounded holding area for `SIGKILL`ed children that had not reported an exit
/// status within the interactive reap grace window. Drained by the already
/// existing single `PtyManager` janitor task, so no extra thread is created per
/// command and the retained handles stay bounded.
pub struct DeferredReapQueue {
    entries: Vec<Box<dyn ChildHandle + Send>>,
}

impl Default for DeferredReapQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl DeferredReapQueue {
    pub fn new() -> Self {
        DeferredReapQueue {
            entries: Vec::new(),
        }
    }

    /// Queue a handle. Already-exited handles are reaped inline (never queued),
    /// and the rest of the queue is drained first, so in practice it stays
    /// (almost) empty. Returns how many queued children were reaped as a side
    /// effect.
    pub fn push(&mut self, mut handle: Box<dyn ChildHandle + Send>) -> usize {
        match handle.try_wait_child() {
            // Already exited (or gone from the process table): reap inline.
            Ok(Some(_)) | Err(_) => return 1,
            Ok(None) => {}
        }

        let reaped = self.drain();
        if self.entries.len() >= DEFERRED_REAP_QUEUE_CAP {
            // Bounded by design: an unreaped handle is retired (warned) instead
            // of letting the queue grow with session count.
            self.entries.remove(0);
            tracing::warn!(
                "pty deferred-reap queue overfull ({DEFERRED_REAP_QUEUE_CAP} entries); retired an unreaped child handle"
            );
        }
        self.entries.push(handle);
        reaped
    }

    /// `try_wait()` every queued handle and retire the ones that reported an
    /// exit status. Returns the number of children reaped (exactly once each:
    /// a retired handle is never waited on again).
    pub fn drain(&mut self) -> usize {
        let mut reaped = 0;
        self.entries.retain_mut(|handle| {
            match handle.try_wait_child() {
                Ok(Some(_)) => {
                    reaped += 1;
                    false
                }
                Ok(None) => true,
                // waitpid failed: the child is already reaped, retire the handle.
                Err(_) => false,
            }
        });
        reaped
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A live sandboxed PTY session. Holds the reaper (child handle + exit status)
/// and the master so the process group can be torn down deterministically **and
/// the child is always reaped** — no `<defunct>` entries pile up across a long
/// session and exhaust the process table (`EAGAIN` on the next spawn).
pub struct PtySession {
    reaper: ChildReaper,
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Process id of the spawned shell (== process group leader on unix).
    pub pid: i32,
}

// Sandbox decision inputs — authoritative ones, never the executable's NAME: the
// **resolved** executable path (`current_exe()` canonicalized), backend support, and an
// explicit opt-out. A renamed, copied or installed binary is sandboxed exactly like any
// other; comparing `file_name(current_exe())` against a name literal would silently
// disable the sandbox for those binaries and enable it for unrelated programs sharing the
// name. The re-entry protocol lives in the `marmel` **bin target** (`src/main.rs`), not
// the library, so a Cargo test-harness binary (`cfg!(test)`) cannot re-enter — a
// compile-time skip that renaming can never reach, warned about like any other skip.

/// argv sentinel understood by `src/main.rs`: re-executing the **same resolved
/// binary** with it applies the backend this host has (Landlock on Linux, Seatbelt on
/// macOS, AppContainer on Windows) and then runs the pending command under it: on unix
/// by `exec()`-ing `sh -c <cmd>` in this process, on Windows by launching the command
/// **inside** the prepared AppContainer (an AppContainer identity is carried by a process
/// at creation, so the re-entry child is the confinement site there — see
/// `harness::sandbox_windows::run_in_appcontainer`).
/// `main.rs` matches it through [`SANDBOX_EXEC_ARG`], so the two ends of the
/// protocol cannot drift apart.
pub const SANDBOX_EXEC_ARG: &str = "--internal-sandbox-exec";

/// The environment fallback for the sandbox opt-out, deliberately strict.
///
/// The typed config knob `sandbox_disabled = true` in `marmel.toml` is the primary switch
/// ([`crate::config::Config::sandbox_disabled`] via [`crate::config::sandbox_disabled`]);
/// **either source opts out** — both are merged by [`crate::config::sandbox_opt_out`] and
/// handed to [`SandboxInputs::opt_out`], and every skip is warned by [`log_sandbox_decision`].
/// Only `1`, `true`, `yes`, `on` (case-insensitive, blanks ignored) disable the sandbox;
/// **every other value — `0`, `false`, `off`, `no`, empty or a typo — keeps it on (fail-closed).**
pub const SANDBOX_OPTOUT_ENV: &str = "MARMEL_DISABLE_SANDBOX";

/// Values of [`SANDBOX_OPTOUT_ENV`] that count as an explicit opt-out.
const SANDBOX_OPTOUT_TRUTHY: [&str; 4] = ["1", "true", "yes", "on"];

/// `true` only for an unambiguous opt-out value (see [`SANDBOX_OPTOUT_ENV`]).
pub fn is_optout_value(raw: &str) -> bool {
    SANDBOX_OPTOUT_TRUTHY.contains(&raw.trim().to_ascii_lowercase().as_str())
}

/// How (and whether) the operator opted out of the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OptOut {
    /// No opt-out: the sandbox stays on (default, fail-closed).
    #[default]
    None,
    /// An explicit opt-out, with the setting that expressed it
    /// (e.g. `MARMEL_DISABLE_SANDBOX=1`) so it is always attributable in logs.
    Explicit { setting: String },
}

/// Injected inputs of the sandbox decision — everything the decision is allowed
/// to know, and nothing else (no executable *name*, no backend probe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInputs<'a> {
    /// `true` when this platform has a sandbox backend at all: Landlock on Linux,
    /// Seatbelt on macOS, AppContainer on Windows. Every production call site takes it
    /// from
    /// [`crate::harness::sandbox::backend_supports_current_platform`], never from a
    /// `target_os` test of its own, so a backend that exists cannot be gated out twice.
    pub backend_supported: bool,
    /// `true` when this process is a Cargo test-harness binary (`cfg!(test)`):
    /// it does not implement [`SANDBOX_EXEC_ARG`], so it cannot re-enter.
    pub test_harness_exe: bool,
    /// The **resolved** executable path (`current_exe()` canonicalized).
    pub resolved_exe: Option<&'a std::path::Path>,
    /// Explicit user opt-out, if any.
    pub opt_out: OptOut,
}

/// Why the sandbox was skipped. Every variant is logged as a `WARN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxSkip {
    /// The operator opted out explicitly (attributable via `setting`).
    OptOut { setting: String },
    /// This platform has no sandbox backend (the BSDs, and anything that is not Linux,
    /// macOS or Windows): not a choice, but it must still be visible. Linux, macOS and
    /// Windows each have one (Landlock, Seatbelt, AppContainer), so they never skip here.
    UnsupportedPlatform,
    /// Running inside a Cargo test-harness binary, which cannot re-enter.
    TestHarnessExe,
}

impl SandboxSkip {
    /// Human-readable reason, used verbatim in the log line.
    pub fn label(&self) -> String {
        match self {
            SandboxSkip::OptOut { setting } => {
                format!("explicit opt-out ({setting})")
            }
            SandboxSkip::UnsupportedPlatform => "platform has no sandbox backend".to_string(),
            SandboxSkip::TestHarnessExe => {
                format!("test-harness binary does not implement {SANDBOX_EXEC_ARG}")
            }
        }
    }
}

/// The outcome of [`should_apply_sandbox`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxDecision {
    /// Apply the sandbox re-entry by exec-ing `resolved_exe` with
    /// [`SANDBOX_EXEC_ARG`]: the child enforces the backend this host has (Landlock on
    /// Linux, Seatbelt on macOS, AppContainer on Windows).
    Apply { resolved_exe: std::path::PathBuf },
    /// Run the command through the plain shell of the platform — `sh -c` on unix,
    /// `%COMSPEC% /C` on Windows — **never silently**: always accompanied by a `WARN`
    /// from [`log_sandbox_decision`].
    Skip {
        resolved_exe: Option<std::path::PathBuf>,
        skip: SandboxSkip,
    },
    /// The sandbox is required here but the executable could not be resolved, so
    /// the command must **hard-fail** instead of degrading to the plain shell.
    UnresolvedExe,
}

impl SandboxDecision {
    /// The chosen mode, stated in the log line at the decision point.
    pub fn mode(&self) -> &'static str {
        match self {
            SandboxDecision::Apply { .. } => "sandbox-re-entry",
            SandboxDecision::Skip { .. } => "unsandboxed",
            SandboxDecision::UnresolvedExe => "hard-fail",
        }
    }
}

/// Decide whether the sandbox **must** apply to the next command: pure logic over injected inputs,
/// fail-closed and name-blind — a renamed, copied, installed or coincidentally-named binary is sandboxed from
/// its resolved exe path like any other. Only an explicit, unambiguous [`OptOut::Explicit`] disables it: the
/// typed `sandbox_disabled` knob ([`crate::config::Config::sandbox_disabled`]) or the `MARMEL_DISABLE_SANDBOX`
/// env fallback ([`SANDBOX_OPTOUT_ENV`]) merged by [`crate::config::sandbox_opt_out`]. An unresolved exe is a
/// hard [`SandboxDecision::UnresolvedExe`], never a plain `sh -c`; [`SandboxDecision::Skip`] covers only no
/// backend on this platform, an explicit opt-out, or a Cargo test-harness binary — and
/// [`log_sandbox_decision`] warns about every skip.
pub fn should_apply_sandbox(inputs: &SandboxInputs<'_>) -> SandboxDecision {
    let resolved_exe = inputs.resolved_exe.map(std::path::Path::to_path_buf);

    // 1. Explicit user intent first: it is attributable and unambiguous.
    if let OptOut::Explicit { setting } = &inputs.opt_out {
        return SandboxDecision::Skip {
            resolved_exe,
            skip: SandboxSkip::OptOut {
                setting: setting.clone(),
            },
        };
    }

    // 2. No sandbox backend on this platform (Landlock, Seatbelt and AppContainer are all
    //    absent, so there is nothing for the re-entry child to apply).
    if !inputs.backend_supported {
        return SandboxDecision::Skip {
            resolved_exe,
            skip: SandboxSkip::UnsupportedPlatform,
        };
    }

    // 3. A test-harness binary cannot speak the re-entry protocol; everything
    //    else (any name, any path) can.
    if inputs.test_harness_exe {
        return SandboxDecision::Skip {
            resolved_exe,
            skip: SandboxSkip::TestHarnessExe,
        };
    }

    // 4. Fail-closed default: sandbox through the resolved exe, or hard-fail if
    //    there is nothing authoritative to exec.
    match resolved_exe {
        Some(resolved_exe) => SandboxDecision::Apply { resolved_exe },
        None => SandboxDecision::UnresolvedExe,
    }
}

/// The **resolved** path of the running executable: `current_exe()` with
/// symlinks resolved, so an installed/symlinked binary re-execs the real image.
/// `None` only when `current_exe()` itself is unavailable.
pub fn resolved_exe_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    match exe.canonicalize() {
        Ok(canonical) => Some(canonical),
        Err(err) => {
            // Keep the raw path (still authoritative); the failure is only about
            // symlink resolution, and the log line records which one is used.
            tracing::debug!("could not canonicalize {}: {err}", exe.display());
            Some(exe)
        }
    }
}

/// Read the explicit opt-out from the environment (see [`SANDBOX_OPTOUT_ENV`]).
fn opt_out_from_env() -> OptOut {
    match std::env::var(SANDBOX_OPTOUT_ENV) {
        Ok(raw) if is_optout_value(&raw) => OptOut::Explicit {
            setting: format!("{SANDBOX_OPTOUT_ENV}={raw}"),
        },
        // Unset, empty, malformed or falsy value: stay sandboxed (fail-closed).
        _ => OptOut::None,
    }
}

/// Make the decision observable: one line naming the resolved exe and the chosen
/// mode, plus a `WARN` on **every** skip and an `ERROR` on the fail-closed path.
/// Running a command unsandboxed is never silent.
pub fn log_sandbox_decision(decision: &SandboxDecision) {
    let resolved_exe = match decision {
        SandboxDecision::Apply { resolved_exe } => resolved_exe.display().to_string(),
        SandboxDecision::Skip { resolved_exe, .. } => resolved_exe
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unresolved>".to_string()),
        SandboxDecision::UnresolvedExe => "<unresolved>".to_string(),
    };
    tracing::debug!(
        resolved_exe = %&resolved_exe,
        mode = decision.mode(),
        "pty sandbox decision"
    );
    match decision {
        SandboxDecision::Apply { .. } => {
            tracing::info!(
                resolved_exe = %&resolved_exe,
                mode = decision.mode(),
                "terminal commands are sandboxed via the re-entry child (Landlock on Linux, Seatbelt on macOS, AppContainer confinement on Windows)"
            );
        }
        SandboxDecision::Skip { skip, .. } => {
            let reason = skip.label();
            tracing::warn!(
                resolved_exe = %&resolved_exe,
                mode = decision.mode(),
                reason = %&reason,
                "pty sandbox SKIPPED: this command runs WITHOUT the sandbox ({reason})"
            );
        }
        SandboxDecision::UnresolvedExe => {
            tracing::error!(
                resolved_exe = "<unresolved>",
                mode = decision.mode(),
                "pty sandbox REQUIRED but the executable path could not be resolved; refusing to run unsandboxed"
            );
        }
    }
}

/// The decision for **this** process, from real (authoritative) inputs, logged.
fn sandbox_decision_for_current_process() -> SandboxDecision {
    let resolved_exe = resolved_exe_path();
    let inputs = SandboxInputs {
        backend_supported: crate::harness::sandbox::backend_supports_current_platform(),
        test_harness_exe: cfg!(test),
        resolved_exe: resolved_exe.as_deref(),
        // Either source opts out: the env fallback (parsed here, unchanged) and
        // the typed config knob `sandbox_disabled` (via `ACTIVE_CONFIG`).
        opt_out: crate::config::sandbox_opt_out(opt_out_from_env()),
    };
    let decision = should_apply_sandbox(&inputs);
    log_sandbox_decision(&decision);
    decision
}

/// Turn a [`SandboxDecision`] into the argv (`program`, `args`) of the command to spawn.
/// Pure, so the re-entry argv is unit-testable without a PTY.
///
/// [`SandboxDecision::UnresolvedExe`] is a **hard error**: an unresolved exe never falls
/// back to a plain shell, because that would run the command unsandboxed without saying
/// so. The message is shared with the Windows branch through [`unresolved_exe_error`].
fn sandbox_command_argv(
    decision: &SandboxDecision,
    command: &str,
    cwd: &std::path::Path,
) -> Result<(String, Vec<String>), ToolError> {
    let wrapped = format!(
        "stty -echo 2>/dev/null || true; ulimit -f {ULIMIT_FILE_BLOCKS} 2>/dev/null || ulimit -f {ULIMIT_FILE_BLOCKS_FALLBACK} 2>/dev/null; {command}"
    );
    match decision {
        SandboxDecision::Apply { resolved_exe } => Ok((
            resolved_exe.to_string_lossy().into_owned(),
            vec![
                SANDBOX_EXEC_ARG.to_string(),
                cwd.to_string_lossy().into_owned(),
                wrapped,
            ],
        )),
        SandboxDecision::Skip { .. } => Ok(("sh".to_string(), vec!["-c".to_string(), wrapped])),
        SandboxDecision::UnresolvedExe => Err(unresolved_exe_error(command)),
    }
}

/// The fail-closed answer of both spawn families when the sandbox is required but the
/// executable could not be resolved: one message, so the unix and Windows branches cannot
/// start spelling the requirement differently.
fn unresolved_exe_error(command: &str) -> ToolError {
    ToolError::Execution(anyhow::anyhow!(
        "refusing to run `{command}` unsandboxed: the sandbox re-entry ({SANDBOX_EXEC_ARG}) needs a resolved executable path, but `current_exe()` is unavailable on this host. Run `marmel` from a resolvable path, or opt out explicitly with {SANDBOX_OPTOUT_ENV}=1."
    ))
}

/// The Windows half of the re-entry argv: the same three slots as [`sandbox_command_argv`]
/// (`[SANDBOX_EXEC_ARG, cwd, command]`), decided from its inputs only so every rule here is
/// assertable with literal Windows inputs on a Linux host. The two Windows-specific
/// differences are the ones that platform demands:
///
/// * **the cwd slot is de-verbatimised.** It is the string the re-entry child hands to the
///   confined command as its current directory, and `cmd.exe` refuses a `\\?\`-verbatim cwd
///   (it reads a leading `\\` as a UNC location and quietly runs elsewhere), so the prefix
///   `canonicalize()` produces is stripped by [`windows_child_cwd`] before it reaches argv —
///   the same sanitising [`build_sandboxed_command`] applies to the builder's own `cwd`.
/// * **the command slot is the payload verbatim.** The `stty`/`ulimit` preamble of the unix
///   wrapper is a pair of POSIX shell builtins with no `cmd.exe` equivalent, so it is not
///   re-introduced here: rule R5 of [`windows_shell_argv`] survives the re-entry, and the
///   confined launch takes the payload byte for byte. What it *does* with it is the
///   hand-off's decision, not this one: `reentry_handoff` keeps a normal payload behind
///   `/C` and an R1 interactive-shell request as the bare shell it already is.
///
/// A `Skip` decision never carries [`SANDBOX_EXEC_ARG`]: it runs the plain
/// [`windows_shell_argv`] argv instead (R1-R5, i.e. the interactive-shell form too), and is
/// warned about by [`log_sandbox_decision`] at the call site. [`SandboxDecision::UnresolvedExe`]
/// hard-fails exactly as on unix.
fn windows_sandbox_command_argv(
    decision: &SandboxDecision,
    command: &str,
    cwd: &std::path::Path,
    comspec: Option<&str>,
) -> Result<(String, Vec<String>), ToolError> {
    match decision {
        SandboxDecision::Apply { resolved_exe } => Ok((
            resolved_exe.to_string_lossy().into_owned(),
            vec![
                SANDBOX_EXEC_ARG.to_string(),
                windows_child_cwd(cwd).to_string_lossy().into_owned(),
                command.to_string(),
            ],
        )),
        SandboxDecision::Skip { .. } => Ok(windows_shell_argv(command, comspec)),
        SandboxDecision::UnresolvedExe => Err(unresolved_exe_error(command)),
    }
}

/// The active config's extra read-only sandbox paths, encoded for
/// [`crate::harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV`], or `None` when there is
/// nothing to hand over (no active config, or a key that is unset or empty).
///
/// The ruleset is built by the re-entry child, and that child never loads a config
/// file — it reads argv and the environment only, so a config value has to travel as
/// an environment variable. The argv contract is left exactly as it is: the sentinel,
/// the cwd and the wrapped command, three slots, unchanged.
///
/// This is the **only** source of that value: the config list decides what the sandbox
/// trusts, and [`wire_extra_readonly_paths_env`] makes sure nothing else can reach the
/// child under the same name. The encoding itself — in particular the entry separator,
/// which cannot be the same on every platform this sandbox runs on — is decided by
/// [`crate::harness::sandbox::extra_readonly_paths_to_env`]; both ends of the channel read
/// that one definition, so the publisher here never spells the separator itself.
fn extra_readonly_paths_env_value() -> Option<String> {
    crate::config::get_active().and_then(|cfg| {
        crate::harness::sandbox::extra_readonly_paths_to_env(&cfg.sandbox_extra_readonly_paths)
    })
}

/// Wire the extras channel of [`crate::harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV`]
/// into a builder according to the decision that produced it — the publish is
/// **decision-scoped**, and the config is the single source of truth:
///
/// * re-entry (`Apply`): publish the configured list when there is one;
/// * anything else (`Skip`, i.e. a plain `sh -c` with no re-entry and therefore no
///   consumer, or the hard-fail branch): **remove** the variable.
///
/// Removal is not cosmetic. The builder starts from `std::env::vars_os()`, so a value
/// that merely happens to sit in the parent's environment is inherited by whatever this
/// builder spawns — the re-entry child (which would widen its own ruleset from it) and,
/// on the skip branch, an unsandboxed shell and any nested invocation of this program
/// that does load a config later. Leaving an ambient value in place would let the
/// environment decide how much the sandbox trusts, which is exactly what the config key
/// exists to prevent. So an empty/unset list **clears** the variable instead of leaving
/// it alone, and a configured list overwrites whatever was inherited.
fn wire_extra_readonly_paths_env(
    cmd: &mut CommandBuilder,
    decision: &SandboxDecision,
    configured: Option<String>,
) {
    match decision {
        SandboxDecision::Apply { .. } => match configured {
            Some(paths) => cmd.env(crate::harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV, paths),
            None => cmd.env_remove(crate::harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV),
        },
        SandboxDecision::Skip { .. } | SandboxDecision::UnresolvedExe => {
            cmd.env_remove(crate::harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV)
        }
    }
}

/// Which spawn rules apply to the `run_command`/PTY path: the Windows rules
/// (`%COMSPEC%`, `/C`, an extension-qualified program, a `cmd.exe`-safe current
/// directory) or the Unix rules (Landlock re-entry, else `sh -c`).
///
/// The spawn builders take this as a parameter instead of reading `cfg!` inline so the
/// **Windows** decision can be asserted on a macOS or Linux host with literal Windows
/// inputs — CI's `cross-check` job only *compiles* the Windows target, so a
/// `#[cfg(windows)]`-only body would be a guard that never runs. Production always
/// passes [`SpawnFamily::current`], so live behaviour is exactly what the platform
/// requires. Same idiom as `PathFamily` in `src/harness/fs.rs`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SpawnFamily {
    Unix,
    Windows,
}

impl SpawnFamily {
    /// The family of the platform this binary was **built** for — the value every
    /// production call site passes.
    pub(crate) const fn current() -> Self {
        if cfg!(windows) {
            SpawnFamily::Windows
        } else {
            SpawnFamily::Unix
        }
    }
}

/// The shell spellings that make a bare command an *interactive shell request* (R1 of
/// [`windows_shell_argv`]), compared ASCII-case-insensitively against the trimmed input.
const WINDOWS_INTERACTIVE_SHELLS: &[&str] = &["cmd", "cmd.exe", "powershell", "powershell.exe"];

/// Rule R1 read back from the other side of the re-entry: whether `command` is exactly an
/// interactive-shell request.
///
/// The Windows backend asks this instead of restating the list, so the spawn side and the
/// confined launch can never disagree about what "an interactive shell request" means — which is
/// exactly what the `<comspec> /C cmd` hazard was: a rule one half of the re-entry knew and the
/// other half did not ask about.
pub(crate) fn is_windows_interactive_shell(command: &str) -> bool {
    let trimmed = command.trim();
    WINDOWS_INTERACTIVE_SHELLS
        .iter()
        .any(|shell| trimmed.eq_ignore_ascii_case(shell))
}

/// Whether an interactive-shell request names the `cmd` family (rule R2) rather than another
/// shell (rule R3). The two have to be resolved differently by the confined launch, which needs
/// an absolute program path for the `cmd` family and the caller's own token for anything else.
pub(crate) fn is_windows_cmd_shell(command: &str) -> bool {
    let trimmed = command.trim();
    trimmed.eq_ignore_ascii_case("cmd") || trimmed.eq_ignore_ascii_case("cmd.exe")
}

/// The `cmd.exe` program token of R2: a non-empty trimmed `%COMSPEC%`, else the literal
/// `cmd.exe`. An empty or whitespace-only `comspec` never wins.
fn windows_cmd_program(comspec: Option<&str>) -> String {
    comspec
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "cmd.exe".to_string())
}

/// Program + arguments for one spawn on Windows, decided from its inputs only so every
/// rule below is assertable with literal Windows inputs on a macOS host.
///
/// Rules:
///
/// * **R1 — a bare shell is an interactive request.** When `command.trim()` is exactly
///   `cmd`, `cmd.exe`, `powershell` or `powershell.exe` (ASCII case-insensitive), the
///   program *is* that shell and there is **no** `/C` payload: the child starts an
///   interactive shell whose commands arrive from the PTY. That is the form
///   `pty_spawn` needs, because it writes into the session afterwards. A **one-shot**
///   caller must not use it: it never writes to the PTY and never closes a writer, so a
///   payload-less shell can only sit at its prompt until the timeout kills it. Removing
///   that hazard belongs to the one-shot rewrite (t-009); [`build_sandboxed_command`]
///   keeps the interactive form because it serves both callers and cannot tell them
///   apart. The sandbox re-entry honours the same rule: the confined launch runs the
///   bare shell instead of putting `/C` back in front of it, so `pty_spawn("cmd")`
///   stays an interactive `cmd.exe` inside the AppContainer (`reentry_handoff`).
/// * **R2 — `cmd` is always extension-qualified.** `CreateProcessW` does not append
///   `.exe` to `lpApplicationName` and cannot launch a `.CMD`/`.BAT` at all, so a bare
///   `cmd` token is a latent `ERROR_FILE_NOT_FOUND` (or a wrongly resolved `.CMD`). The
///   program is therefore `%COMSPEC%` when `comspec` is `Some` and non-empty after
///   trimming, and the literal `cmd.exe` otherwise.
/// * **R3 — other shells are passed through as written.** `powershell`/`powershell.exe`
///   (any casing) are used verbatim (trimmed): `%COMSPEC%` names `cmd.exe`, so
///   substituting it would silently run a different shell than the caller asked for.
/// * **R4 — everything else is `cmd /C <command>`.** A normal command is one `/C`
///   payload argument, i.e. `[program, "/C", command]` with the program from R2.
/// * **R5 — the payload is verbatim.** `command` reaches the child byte for byte: no
///   trimming, no re-quoting, no POSIX preamble. Quoting/escaping of the payload
///   (portable-pty's `append_quoted` applies MSVCRT backslash rules while `cmd.exe` has
///   no backslash escape) is deliberately out of scope here and belongs to t-009.
pub(crate) fn windows_shell_argv(command: &str, comspec: Option<&str>) -> (String, Vec<String>) {
    let trimmed = command.trim();
    if is_windows_interactive_shell(command) {
        let program = if is_windows_cmd_shell(command) {
            windows_cmd_program(comspec)
        } else {
            trimmed.to_string()
        };
        return (program, Vec::new());
    }
    (
        windows_cmd_program(comspec),
        vec!["/C".to_string(), command.to_string()],
    )
}

/// How the sandbox re-entry child hands one payload to a shell, as the **value** the platform is
/// dispatched on rather than a `cfg!` branch, so the rule can be asserted on a Linux host.
///
/// THE INTERACTIVE SHELL UNDER THE RE-ENTRY (the rule, decided once here). On unix the re-entry
/// child execs `sh -c <payload>` with the payload wrapped in the `stty`/`ulimit` preamble, and a
/// payload that is just `sh` therefore becomes a **nested** shell reading the stdin it inherited:
/// the confinement applies and the session stays interactive — the unix path neither refuses nor
/// skips an interactive-shell request, it confines it. The Windows re-entry has to reach that same
/// outcome, so rule R1 of [`windows_shell_argv`] survives the re-entry: an interactive-shell
/// request is handed to the confined launch as `InteractiveShell` (launch the bare shell, no
/// `/C`) instead of being turned into `<comspec> /C cmd`, which would start `cmd.exe` looking for
/// a program called `cmd` and end the session. Normal payloads keep `Execute` on both families.
/// The one-shot path is untouched either way: it refuses a payload-less shell for every decision
/// ([`windows_oneshot_sandbox_argv`]), so an interactive request can only ever arrive from a PTY
/// run, and a `Skip` decision never reaches the confined launch at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(windows, test))]
pub(crate) enum ReentryHandoff {
    /// The payload is executed as a shell command: `sh -c <payload>` on unix,
    /// `<program> /C <payload>` in the Windows confined launch.
    Execute,
    /// The payload is an interactive-shell request (rule R1): the confined child **is** the shell,
    /// reading the stdin it inherits, and no `/C` is added in front of it.
    InteractiveShell,
}

/// The hand-off one payload gets, for the spawn family that runs it.
///
/// Production passes [`SpawnFamily::current`]; a host test passes each variant explicitly, which
/// is why the family is a parameter and not a `cfg!` read here. The `cfg(any(windows, test))`
/// gate matches the rest of the Windows-only decision surface in this file: the only production
/// caller is the Windows confined launch (`sandbox_windows::confined_command_line`), so on a
/// non-Windows production build nothing can call it, and the `unused`/`dead_code` lints this
/// crate denies would say so.
#[cfg(any(windows, test))]
#[must_use]
pub(crate) fn reentry_handoff(command: &str, family: SpawnFamily) -> ReentryHandoff {
    match family {
        // The unix payload is always executed by `sh -c`, and the interactive-ness of a bare
        // `sh` request comes from the nested shell that request runs, not from the hand-off.
        SpawnFamily::Unix => ReentryHandoff::Execute,
        SpawnFamily::Windows => {
            if is_windows_interactive_shell(command) {
                ReentryHandoff::InteractiveShell
            } else {
                ReentryHandoff::Execute
            }
        }
    }
}

/// Drop `prefix` from `raw` when it matches ASCII-case-insensitively, else `None`.
/// Every prefix used here is ASCII, so the returned tail always starts on a `char`
/// boundary.
fn strip_ascii_prefix<'a>(raw: &'a str, prefix: &str) -> Option<&'a str> {
    let bytes = raw.as_bytes();
    let wanted = prefix.as_bytes();
    (bytes.len() >= wanted.len() && bytes[..wanted.len()].eq_ignore_ascii_case(wanted))
        .then(|| &raw[wanted.len()..])
}

/// The current directory to hand to a Windows child, decided from its input only.
///
/// [`PtySession::spawn`] and [`PtyManager::spawn`] pass
/// [`crate::harness::get_workspace_root`], and `set_workspace_root` canonicalises it,
/// which on Windows yields a `\\?\`-verbatim path (`\\?\C:\work\proj`).
/// `CreateProcessW` accepts that spelling, but `cmd.exe` reads a leading `\\` as a UNC
/// path and refuses to use one as its current directory — the command then quietly runs
/// in `C:\Windows`. Rules:
///
/// * **C1 — a verbatim UNC path loses only the verbatim prefix.** `\\?\UNC\srv\share\p`
///   becomes `\\srv\share\p`, the plain UNC spelling `cmd.exe` understands.
/// * **C2 — a verbatim volume path loses only the prefix.** `\\?\C:\work\proj` becomes
///   `C:\work\proj`.
/// * **C3 — every other path is returned untouched**, byte for byte: a plain
///   `C:\work`, a relative path, a Unix path, and a degenerate `\\?\` (or `\\?\UNC\`)
///   with nothing behind it, which is left as-is instead of collapsing to an empty path.
/// * **C4 — never a substitution.** A directory that does not exist is *not* replaced by
///   another one (portable-pty would otherwise fall back to `%USERPROFILE%`); the caller
///   decides, and the path it asked for is the path it gets.
///
/// The prefix match is ASCII-case-insensitive because the Windows namespace prefix is.
/// `\\.\` and `\\localhost\` are deliberately *not* rewritten: they are device
/// spellings, not verbatim ones (unlike `fs::WINDOWS_PATH_PREFIXES`, which serves
/// comparison keys rather than a child's working directory). A de-verbatimised path is
/// rebuilt from its string spelling — the removed prefix is pure ASCII, so only a path
/// carrying an unpaired surrogate would not round-trip — while every untouched path is
/// returned as the original `PathBuf`.
pub(crate) fn windows_child_cwd(cwd: &std::path::Path) -> std::path::PathBuf {
    const VERBATIM_UNC: &str = r"\\?\UNC\";
    const VERBATIM: &str = r"\\?\";
    let raw = cwd.as_os_str().to_string_lossy();
    let spelled: &str = &raw;
    if let Some(rest) = strip_ascii_prefix(spelled, VERBATIM_UNC) {
        if !rest.is_empty() {
            return std::path::PathBuf::from(format!(r"\\{rest}"));
        }
        return cwd.to_path_buf();
    }
    if let Some(rest) = strip_ascii_prefix(spelled, VERBATIM)
        && !rest.is_empty()
    {
        return std::path::PathBuf::from(rest);
    }
    cwd.to_path_buf()
}

/// Build the shell command for one `run_command`/PTY execution.
///
/// The sandbox is applied through [`SANDBOX_EXEC_ARG`] re-entry decided by [`should_apply_sandbox`] —
/// from the resolved exe path, the backend the host has (Landlock on Linux, Seatbelt on macOS,
/// AppContainer on Windows) and an explicit opt-out, **never** from the executable's name.
/// `Err` is returned when the sandbox is required but cannot be applied (fail-closed).
///
/// Both spawn families go through the very same decision object. On Windows an `Apply`
/// decision produces the re-entry invocation built by [`windows_sandbox_command_argv`]
/// (resolved exe, sentinel, de-verbatimised cwd, payload verbatim), and only a `Skip`
/// decision runs the plain [`windows_shell_argv`] argv — never silently, because
/// [`log_sandbox_decision`] warns about every skip. In both cases the current directory
/// handed to the child is the [`windows_child_cwd`]-sanitised one.
pub fn build_sandboxed_command(
    command: &str,
    cwd: &std::path::Path,
) -> Result<CommandBuilder, ToolError> {
    // The Unix branch is returned early so its argv, its extras-channel wiring and its
    // cwd stay byte-identical to the pre-value version; only the branch *selector* moved
    // from `cfg!` to [`SpawnFamily::current`].
    if SpawnFamily::current() == SpawnFamily::Unix {
        let decision = sandbox_decision_for_current_process();
        let (program, args) = sandbox_command_argv(&decision, command, cwd)?;
        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        // The extras channel is wired from the decision that produced this argv, never
        // unconditionally: it is published only for the re-entry child that reads it,
        // and cleared otherwise so nothing ambient is inherited.
        wire_extra_readonly_paths_env(&mut cmd, &decision, extra_readonly_paths_env_value());
        cmd.cwd(cwd);
        return Ok(cmd);
    }

    // Windows has an AppContainer backend, so this branch consults the very same decision
    // object the unix branch uses: `Apply` re-enters through the sentinel, and only a `Skip`
    // runs the plain `%COMSPEC% /C` command. A skip is never silent — [`log_sandbox_decision`]
    // has already warned about it, with the reason that actually applies on this host.
    let decision = sandbox_decision_for_current_process();
    // The program token is decided here and never left as a bare `cmd`: portable-pty
    // resolves argv[0] through PATH+PATHEXT but passes the result to `CreateProcessW` as
    // `lpApplicationName`, which does not append `.exe`. For a re-entry the token is the
    // resolved exe, so this only decides the program of the skip branch.
    let comspec = std::env::var("COMSPEC").ok();
    let (program, args) =
        windows_sandbox_command_argv(&decision, command, cwd, comspec.as_deref())?;
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    // The extras channel is decision-scoped on Windows exactly as on unix: published only
    // for the re-entry child that reads it, cleared for everything else.
    wire_extra_readonly_paths_env(&mut cmd, &decision, extra_readonly_paths_env_value());
    // `cmd.exe` treats a `\\`-prefixed path as UNC and refuses it as its current
    // directory, so the canonicalised (verbatim) workspace root is de-verbatimised before
    // it reaches `CreateProcessW`. The cwd slot of the re-entry argv gets the same
    // sanitising treatment, for the same reason.
    cmd.cwd(windows_child_cwd(cwd));
    Ok(cmd)
}

impl PtySession {
    /// Wrap `command` in the sandbox preamble and spawn it into a PTY.
    pub fn spawn(command: &str) -> Result<Self, ToolError> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let cur_dir = crate::harness::get_workspace_root();
        let cmd = build_sandboxed_command(command, &cur_dir)?;
        let child = pair.slave.spawn_command(cmd)?;
        // Release the slave side now that the child is spawned.
        drop(pair.slave);

        #[cfg(unix)]
        let pid = pair
            .master
            .process_group_leader()
            .or_else(|| child.process_id().map(|p| p as i32))
            .ok_or_else(|| ToolError::BadArguments {
                tool: TOOL_RUN_COMMAND.into(),
                detail: "could not obtain child process id".into(),
            })?;

        #[cfg(not(unix))]
        let pid = child
            .process_id()
            .map(|p| p as i32)
            .ok_or_else(|| ToolError::BadArguments {
                tool: TOOL_RUN_COMMAND.into(),
                detail: "could not obtain child process id".into(),
            })?;

        Ok(PtySession {
            reaper: ChildReaper::new(PtyChildHandle { child }, Some(pid)),
            master: pair.master,
            pid,
        })
    }

    /// Kill the entire process group (negative pid), then the child, and **reap
    /// the child** so it does not linger as a zombie.
    ///
    /// This is invoked on command completion, timeout, cancellation, and
    /// teardown, and is idempotent: the `Drop` safety net and the repeated
    /// teardown passes never signal or `wait()` an already reaped pid again.
    pub fn teardown(&mut self) -> Result<(), ToolError> {
        let outcome = self.reaper.kill_and_reap(PTY_REAP_GRACE, true);
        outcome.group.map_err(anyhow::Error::from)?;
        Ok(())
    }

    /// Read all pending output from the PTY master until EOF.
    pub fn read_output(&mut self) -> Result<String, ToolError> {
        let mut reader = self.master.try_clone_reader()?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Safety net for the error/`?` paths (`try_clone_reader`, argument
        // validation, …) that never reach `teardown()`: the child is always
        // killed and reaped, never silently dropped as a zombie.
        if !self.reaper.is_reaped() {
            let _ = self.reaper.kill_and_reap(PTY_REAP_GRACE, true);
        }
    }
}

/// Capability probe: can this host allocate a PTY at all?
///
/// Returns `None` when a PTY opens normally, or `Some(reason)` naming the failure
/// when the host cannot provide one. This is a **host capability**, not a product
/// behaviour: `openpty()` needs a usable `/dev/pts/ptmx` on Linux (a devpts mount
/// with `ptmxmode=000`, or an equivalent device hole, makes every allocation return
/// `EACCES`), and the same applies to the platform equivalent elsewhere.
///
/// The verdict is probed once and cached, mirroring `kernel_has_landlock()` in
/// `src/harness/sandbox.rs`: it cannot change within a process, and every caller
/// must see the same answer. Callers in tests announce a visible `SKIP ` rather
/// than reaching for `#[ignore]`, for the reason spelled out at
/// `src/harness/sandbox.rs:1765-1770` — "an invisible skip is the failure mode
/// these tests exist to avoid". A PTY-capable host (every CI runner) keeps running
/// the full assertions, so green never means "not checked".
pub fn pty_allocation_blocker() -> Option<&'static str> {
    static CACHED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            match native_pty_system().openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                Ok(_pair) => None,
                Err(err) => Some(format!("{err}")),
            }
        })
        .as_deref()
}

/// Execute a shell command via `run_command`. Parses `{ command, timeout_seconds? }`.
///
/// If `timeout_seconds` (or `timeout`) is provided it is the per-call override;
/// otherwise the configured `command_timeout_secs` from the active config is used
/// (default 60 s). Both are clamped into the documented range 1..=300 seconds by
/// [`crate::config::effective_command_timeout_secs`].
pub fn run_command(args: &Value) -> Result<ToolResult, ToolError> {
    let command = crate::harness::fs::str_arg(args, "command", TOOL_RUN_COMMAND)?;
    let timeout_secs = crate::config::effective_command_timeout_secs(
        args.get("timeout_seconds")
            .or_else(|| args.get("timeout"))
            .and_then(|v| v.as_u64()),
    );
    let output = run_command_pty(command, Duration::from_secs(timeout_secs))?;
    Ok(ToolResult::ok(output))
}

// One-shot outcome reporting — pure, so it is asserted on every host
//
// `run_command` has one channel to the caller, the returned string, so a failure must
// never be spelled like a success: a non-zero exit carries an explicit status line and a
// timeout/cancellation keeps the output it already captured. Both rules are functions of
// their arguments only, which is why they are unit-tested on a macOS/Linux host; the
// `cfg(any(windows, test))` gate keeps a production build on those hosts free of the
// otherwise unused code (the only production caller is the Windows one-shot path).

/// How a one-shot execution ended, reduced to what the result string must say.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OneShotEnd {
    /// The child exited on its own; `None` when the platform reported no exit code.
    Exited(Option<i32>),
    /// The deadline elapsed first; the whole child tree was killed.
    TimedOut(Duration),
    /// A cancellation signal preempted the run; the whole child tree was killed.
    Cancelled,
}

/// Render the string `run_command` returns for `captured` output that ended as `end`.
///
/// The shape mirrors the PTY path: the captured bytes get [`sanitize_terminal_output`]
/// and lose their trailing newlines, so a clean exit returns the output alone, exactly
/// like the Unix path does today. Anything else appends **one** bracketed line — the exit
/// status, the timeout notice or the cancellation notice — so the outcome can no longer be
/// mistaken for success and the captured output is never thrown away.
#[cfg(any(windows, test))]
pub(crate) fn render_one_shot_result(captured: &str, end: OneShotEnd) -> String {
    let body = sanitize_terminal_output(captured)
        .trim_end_matches('\n')
        .to_string();
    let notice = match end {
        OneShotEnd::Exited(Some(0)) => return body,
        OneShotEnd::Exited(Some(code)) => format!("[exit code: {code}]"),
        OneShotEnd::Exited(None) => "[exit status unavailable]".to_string(),
        OneShotEnd::TimedOut(timeout) => format!(
            "[command timed out after {}s and was killed]",
            timeout.as_secs()
        ),
        OneShotEnd::Cancelled => "[command aborted by cancellation signal]".to_string(),
    };
    if body.is_empty() {
        notice
    } else {
        format!("{body}\n{notice}")
    }
}

/// Program + arguments of a **one-shot** Windows spawn.
///
/// A one-shot command has no terminal and writes nothing to the child, so R1 of
/// [`windows_shell_argv`] — the payload-less interactive shell — can only sit at its
/// prompt until the timeout kills it. That form is refused here with an actionable
/// detail instead of being spawned; everything else is [`windows_shell_argv`] unchanged
/// (R2-R5), so a normal command is `[program, "/C", command]` with the payload reaching
/// the child byte for byte.
#[cfg(any(windows, test))]
pub(crate) fn windows_oneshot_argv(
    command: &str,
    comspec: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    let (program, args) = windows_shell_argv(command, comspec);
    if args.is_empty() {
        return Err(format!(
            "`{program}` is an interactive shell, not a command: a one-shot run_command has \
             no terminal to type into, so the shell would wait for input until the timeout. \
             Use pty_spawn for an interactive session, or pass the command to run"
        ));
    }
    Ok((program, args))
}

/// The argv of a one-shot Windows spawn, with the sandbox decision folded in — decided from
/// its inputs only, so it is assertable on a Linux host with literal Windows inputs.
///
/// The interactive-shell refusal of [`windows_oneshot_argv`] is applied **first**, and to
/// every decision: a one-shot `run_command` has no terminal, and the re-entry child hands
/// the payload to `%COMSPEC% /C` just the same, so a payload-less shell request would sit at
/// its prompt until the timeout killed it whether or not it is confined. Then:
///
/// * `Apply` → the three re-entry slots of [`windows_sandbox_command_argv`], so the one-shot
///   command runs inside the AppContainer the re-entry child prepares;
/// * `Skip` → the plain [`windows_oneshot_argv`] argv, byte for byte as before;
/// * `UnresolvedExe` → the shared fail-closed message, reported as the same kind of refusal.
#[cfg(any(windows, test))]
pub(crate) fn windows_oneshot_sandbox_argv(
    decision: &SandboxDecision,
    command: &str,
    cwd: &std::path::Path,
    comspec: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    let plain = windows_oneshot_argv(command, comspec)?;
    if matches!(decision, SandboxDecision::Skip { .. }) {
        return Ok(plain);
    }
    // `Apply` builds the re-entry triple; `UnresolvedExe` contributes the fail-closed
    // message, which reaches the caller as a one-shot refusal either way.
    windows_sandbox_command_argv(decision, command, cwd, comspec).map_err(|err| err.to_string())
}

/// What one iteration of the one-shot poll loop decides.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OneShotPoll {
    /// The child exited on its own; `None` when the platform reported no exit code.
    Exited(Option<i32>),
    /// A cancellation signal preempted the run.
    Cancelled,
    /// The deadline elapsed first.
    TimedOut,
    /// Nothing has happened yet: keep polling.
    Running,
}

/// The one-shot poll loop's decision, decided from its inputs only.
///
/// `exit_code` is `Some(..)` once the child has exited (`Some(None)` when the platform
/// reported no code), `elapsed` is the time since the spawn. The **order** is the
/// contract, not an implementation detail:
///
/// * an observed exit wins over everything — a command that finished inside (or exactly
///   at) its budget is reported with its own status and its output, never as
///   "timed out and was killed";
/// * a cancellation signal wins over the deadline — the caller asked for it;
/// * the deadline is `elapsed >= timeout`, so a degenerate `timeout` of zero means "stop
///   at the first poll" and can never mean "wait forever".
///
/// The loop lives in `#[cfg(windows)]` code that no non-Windows host can execute, so this
/// decision is a pure function: the arithmetic (including a zero/sub-second budget) is
/// assertable on every host instead of only where the loop runs.
#[cfg(any(windows, test))]
pub(crate) fn one_shot_poll_decision(
    exit_code: Option<Option<i32>>,
    cancelled: bool,
    elapsed: Duration,
    timeout: Duration,
) -> OneShotPoll {
    if let Some(code) = exit_code {
        return OneShotPoll::Exited(code);
    }
    if cancelled {
        return OneShotPoll::Cancelled;
    }
    if elapsed >= timeout {
        return OneShotPoll::TimedOut;
    }
    OneShotPoll::Running
}

/// One-shot execution of a `run_command` command with a strict `timeout`.
///
/// A one-shot command needs no terminal, and on Windows the PTY *is* ConPTY: its output
/// pipe is closed only as part of `ClosePseudoConsole`, which `portable-pty` runs when the
/// session is dropped — i.e. **after** the read loop — so a PTY reader there never sees
/// EOF, and every command could only ever burn its full timeout and discard the bytes it
/// had already captured. The Windows branch therefore spawns the shell directly with
/// `std::process::Command` (`run_command_windows_oneshot`) and terminates on **child
/// exit**, while interactive sessions (`pty_spawn`) keep using portable-pty on both
/// platforms.
///
/// The Unix branch is the PTY implementation, unchanged (`run_command_via_pty`).
pub fn run_command_pty(command: &str, timeout: Duration) -> Result<String, ToolError> {
    #[cfg(windows)]
    {
        run_command_windows_oneshot(command, timeout)
    }

    #[cfg(not(windows))]
    {
        run_command_via_pty(command, timeout)
    }
}

/// Unix one-shot execution: spawn the sandbox into a PTY, read output with a timeout, and
/// always tear down the process group **and reap the child** afterwards. The `timeout` is
/// strict (default 300 s) and preempts a hung child by SIGKILLing the entire process
/// group. Every exit path — normal completion, timeout, cancellation signal, abrupt
/// disconnect, or an error from `?` before the loop even starts — ends in
/// [`PtySession::teardown`] / `Drop as PtySession`, so no `<defunct>` child survives a
/// `run_command`.
#[cfg(not(windows))]
fn run_command_via_pty(command: &str, timeout: Duration) -> Result<String, ToolError> {
    let mut session = PtySession::spawn(command)?;

    // Spawn the writer so EOF can be generated on drop (avoids deadlock).
    let _writer = session.master.take_writer();

    let mut reader = session.master.try_clone_reader()?;

    // Read output in a separate thread so the timeout can preempt a hung child.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });

    // Wait for output with a timeout and periodic cancellation checks; on timeout or cancel the group is SIGKILLed.
    let start = std::time::Instant::now();
    let poll_interval = std::time::Duration::from_millis(50);
    let output = loop {
        match rx.recv_timeout(poll_interval) {
            Ok(o) => break o,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if crate::orchestrator::is_current_or_global_cancelled() {
                    let _ = session.teardown();
                    return Ok("[command aborted by cancellation signal]".to_string());
                }
                if start.elapsed() >= timeout {
                    let _ = session.teardown();
                    return Ok(format!(
                        "[command timed out after {}s and was killed]",
                        timeout.as_secs()
                    ));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = session.teardown();
                return Ok("[command process terminated abruptly]".to_string());
            }
        }
    };

    // On completion, tear down the process group (killing any orphans) and reap
    // the child, then release the PTY master.
    let _ = session.teardown();
    drop(session);

    // Sanitize the captured output before returning it to the caller.
    Ok(sanitize_terminal_output(&output)
        .trim_end_matches('\n')
        .to_string())
}

// Windows one-shot execution: no ConPTY
//
// A one-shot command has no terminal to talk to, so the Windows branch spawns it with
// `std::process::Command` and keeps portable-pty for interactive sessions only.

/// How much of the command the one-shot `tracing::debug!` line keeps: enough to identify
/// what ran, bounded so a multi-KiB heredoc cannot flood the log.
#[cfg(windows)]
const ONESHOT_LOG_COMMAND_CHARS: usize = 200;

/// How long the pipe readers get to drain once the child is gone. Bounded on purpose: a
/// `run_command` must never block indefinitely on a pipe that a stray grandchild still
/// holds open.
#[cfg(windows)]
const ONESHOT_DRAIN_GRACE: Duration = Duration::from_millis(300);

/// One-shot Windows execution with `std::process::Command` — no ConPTY anywhere.
///
/// Why not the PTY path (see [`run_command_pty`]): under ConPTY the output pipe is closed
/// only by `ClosePseudoConsole`, which `portable-pty` invokes in `PseudoConsole::drop`,
/// i.e. after the read loop — so the reader never reaches EOF and a `run_command` could
/// only ever time out, discarding the output it had already captured and never reporting
/// the exit code.
///
/// Properties this path guarantees:
///
/// * the payload reaches `cmd.exe` verbatim (`raw_arg`, never `args()`, whose MSVCRT
///   quoting mangles `"` — `cmd.exe` has no backslash escape);
/// * `stdin` is null, so a command can never block waiting for input;
/// * `stdout` and `stderr` are drained concurrently into one shared buffer, so a chatty
///   child cannot deadlock on a full pipe and partial output survives a timeout;
/// * the child is polled with `try_wait()` against the same deadline and 50 ms cadence as
///   the Unix path, and cancellation still preempts;
/// * a timeout or cancellation kills the whole **tree** (`taskkill /F /T`), then reaps
///   the direct child, so nothing is left running or unreaped.
#[cfg(windows)]
fn run_command_windows_oneshot(command: &str, timeout: Duration) -> Result<String, ToolError> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    let cwd = windows_child_cwd(&crate::harness::get_workspace_root());
    let comspec = std::env::var("COMSPEC").ok();
    // The same decision object the PTY path consults: this path is sandboxed by re-entering
    // this program with the sentinel, exactly like a PTY spawn, so `run_command` on Windows
    // is confined too instead of running in the clear. `windows_oneshot_sandbox_argv` keeps
    // the interactive-shell refusal for every decision, and a `Skip` returns the plain argv.
    let decision = sandbox_decision_for_current_process();
    let confined = matches!(decision, SandboxDecision::Apply { .. });
    let (program, args) =
        windows_oneshot_sandbox_argv(&decision, command, &cwd, comspec.as_deref()).map_err(
            |detail| ToolError::BadArguments {
                tool: TOOL_RUN_COMMAND.into(),
                detail,
            },
        )?;

    // One line, so the next Windows report is diagnosable from the log alone.
    tracing::debug!(
        platform = std::env::consts::OS,
        shell = %program,
        cwd = %cwd.display(),
        mode = decision.mode(),
        command = %crate::text_util::truncate_chars(command, ONESHOT_LOG_COMMAND_CHARS),
        "run_command one-shot spawn (std::process::Command, no ConPTY)"
    );

    let mut cmd = Command::new(&program);
    cmd.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The extras channel is decision-scoped here too, and this builder starts from the
    // **inherited** environment (plain `std::process::Command`), so an ambient value would
    // otherwise reach the re-entry child and widen its ruleset without the config saying so.
    match &decision {
        SandboxDecision::Apply { .. } => {
            if let Some(paths) = extra_readonly_paths_env_value() {
                cmd.env(crate::harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV, paths);
            } else {
                cmd.env_remove(crate::harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV);
            }
        }
        SandboxDecision::Skip { .. } | SandboxDecision::UnresolvedExe => {
            cmd.env_remove(crate::harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV);
        }
    }
    // `raw_arg` for the plain shell argv (`/C` plus the payload): the argument reaches
    // `cmd.exe` as written, which is the whole point of not going through `args()`.
    //
    // For the re-entry triple that is the wrong encoding: argv is then re-parsed by the
    // child's C runtime into three slots, and a cwd containing a space (a normal thing under
    // `C:\Program Files\...`) must survive as **one** slot. `args()` applies exactly that
    // MSVCRT quoting, and the payload slot round-trips through it unchanged, so the confined
    // launch inside the re-entry child still gets the payload byte for byte.
    if confined {
        cmd.args(&args);
    } else {
        for arg in &args {
            cmd.raw_arg(arg);
        }
    }
    let mut child = cmd.spawn().map_err(|err| {
        // A spawn failure is a real error, never a notice string: name the shell, the
        // directory it was given and the OS error.
        ToolError::Execution(anyhow::anyhow!(
            "failed to spawn `{program}` in `{}`: {err}",
            cwd.display()
        ))
    })?;

    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_pipe_reader(stdout, std::sync::Arc::clone(&captured)));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_pipe_reader(stderr, std::sync::Arc::clone(&captured)));
    }

    // Terminate on child exit, not on pipe EOF — the same deadline and cadence as the
    // Unix poll loop, and cancellation still preempts the run.
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(50);
    let end = loop {
        let exit_code = match child.try_wait() {
            Ok(Some(status)) => Some(status.code()),
            Ok(None) => None,
            Err(err) => {
                // The child's state is unknown from here on: kill and reap it rather
                // than leak it, and report the exit status as unavailable.
                tracing::warn!(error = %err, "run_command could not poll the child");
                kill_windows_tree(&mut child);
                break OneShotEnd::Exited(None);
            }
        };
        match one_shot_poll_decision(
            exit_code,
            crate::orchestrator::is_current_or_global_cancelled(),
            start.elapsed(),
            timeout,
        ) {
            OneShotPoll::Exited(code) => break OneShotEnd::Exited(code),
            OneShotPoll::Cancelled => {
                kill_windows_tree(&mut child);
                break OneShotEnd::Cancelled;
            }
            OneShotPoll::TimedOut => {
                kill_windows_tree(&mut child);
                break OneShotEnd::TimedOut(timeout);
            }
            OneShotPoll::Running => std::thread::sleep(poll_interval),
        }
    };

    // Bounded grace: both pipes close once the child (and the killed tree) is gone, so
    // this is normally immediate. It exists only so that output already written is not
    // lost to a reader thread that has not been scheduled yet.
    let grace_start = std::time::Instant::now();
    while grace_start.elapsed() < ONESHOT_DRAIN_GRACE && readers.iter().any(|h| !h.is_finished()) {
        std::thread::sleep(Duration::from_millis(5));
    }
    for handle in readers {
        if handle.is_finished() {
            if handle.join().is_err() {
                tracing::warn!("a run_command pipe reader panicked; output may be truncated");
            }
        } else {
            // A stray grandchild still holds a pipe end. Detach the reader instead of
            // joining it: `run_command` must never block indefinitely.
            tracing::warn!("a run_command pipe reader did not finish within the drain grace");
        }
    }

    let text = match captured.lock() {
        Ok(buf) => String::from_utf8_lossy(&buf).into_owned(),
        Err(poisoned) => {
            // A reader panicked mid-push; the bytes it already stored are still valid,
            // so recover them instead of discarding the output.
            tracing::warn!("run_command output buffer was poisoned; recovering captured bytes");
            String::from_utf8_lossy(&poisoned.into_inner()).into_owned()
        }
    };
    Ok(render_one_shot_result(&text, end))
}

/// Drain one child pipe into the shared capture buffer on its own thread.
///
/// Streaming rather than buffering until EOF is what keeps the output on a timeout, and
/// reading both pipes concurrently is what stops a chatty child from blocking on a full
/// pipe buffer. Chunks from `stdout` and `stderr` interleave, which is the same
/// best-effort interleaving a terminal gives.
#[cfg(windows)]
fn spawn_pipe_reader<R: Read + Send + 'static>(
    mut pipe: R,
    sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut tmp = [0u8; 4096];
        loop {
            match pipe.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => match sink.lock() {
                    Ok(mut buf) => buf.extend_from_slice(&tmp[..n]),
                    // A poisoned buffer means the other reader panicked while holding
                    // the lock, so the capture is already broken: stop here instead of
                    // spinning on it (the caller still recovers the stored bytes).
                    Err(_) => break,
                },
                Err(_) => break,
            }
        }
    })
}

/// Kill the whole child tree on Windows, then reap the direct child.
///
/// `portable-pty` creates no Job Object and `Child::kill` only `TerminateProcess`es the
/// direct child, so a grandchild of `cmd.exe` survives it. Without a Job Object the
/// dependency-free route to the tree is `taskkill /F /T /PID <pid>`, which walks the
/// parent-child relation. A non-zero `taskkill` exit normally just means the tree is
/// already gone, so it is not treated as a failure; `child.kill()` remains the fallback
/// for the direct child (and for a machine without `taskkill`), and `child.wait()` reaps
/// it so nothing is left unreaped.
#[cfg(windows)]
fn kill_windows_tree(child: &mut std::process::Child) {
    let pid = child.id().to_string();
    match std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", pid.as_str()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if !status.success() => {
            tracing::debug!(pid = %pid, code = ?status.code(), "taskkill did not report success");
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(pid = %pid, error = %err, "taskkill is unavailable"),
    }
    if let Err(err) = child.kill() {
        // An already-exited child fails here too; the `wait` below still reaps it.
        tracing::debug!(error = %err, "child.kill() after taskkill");
    }
    if let Err(err) = child.wait() {
        tracing::warn!(error = %err, "could not reap the killed run_command child");
    }
}

// Interactive Multi-Turn PTY Manager

/// `true` when `byte` may begin a UTF-8 code point, i.e. it is **not** a `10xxxxxx`
/// continuation byte. Byte-level char-boundary test for the ring buffer trim in
/// [`SharedBuffer::trim_to_cap`]: advancing the cut to the next lead byte means the
/// retained window never starts in the middle of a multi-byte character (never a split
/// `å`/`é`/`🙂`, and never a mid-character byte slice).
fn is_utf8_lead_byte(byte: u8) -> bool {
    byte & 0b1100_0000 != 0b1000_0000
}

/// Bounded retention window for one interactive PTY session. `output` is a ring window, not an append-only
/// log: bytes a reader has already consumed (`..cursor`) are reclaimed on every push, and if the *unread*
/// tail still exceeds [`PTY_OUTPUT_BUFFER_CAP`] the oldest bytes are dropped at a UTF-8 char boundary and
/// counted in `dropped_bytes`, with the amount since the last read queued in `pending_notice_bytes` so the
/// next reader sees that the head was discarded. Both steps live in [`SharedBuffer::trim_to_cap`], called
/// from the single writer side ([`SharedBuffer::push`], the PTY reader thread).
struct SharedBuffer {
    output: Vec<u8>,
    cursor: usize,
    is_alive: bool,
    last_activity: std::time::Instant,
    /// Total bytes dropped from the head because of the cap (cumulative).
    dropped_bytes: u64,
    /// Head bytes dropped since the last read; surfaced once as a notice.
    pending_notice_bytes: u64,
}

impl SharedBuffer {
    fn new() -> Self {
        SharedBuffer {
            output: Vec::new(),
            cursor: 0,
            is_alive: true,
            last_activity: std::time::Instant::now(),
            dropped_bytes: 0,
            pending_notice_bytes: 0,
        }
    }

    /// Writer side: append `bytes`, refresh the idle timestamp, enforce the cap.
    fn push(&mut self, bytes: &[u8]) {
        self.output.extend_from_slice(bytes);
        self.last_activity = std::time::Instant::now();
        self.trim_to_cap();
    }

    /// Enforce [`PTY_OUTPUT_BUFFER_CAP`]: 1. reclaim the already-delivered prefix `..cursor`; 2. if the tail is
    /// still over the cap, drop from the head, advancing the cut to the next UTF-8 char boundary with
    /// [`is_utf8_lead_byte`]. That is the byte-level counterpart of the `text_util::truncate_chars` /
    /// `truncate_with_ellipsis` guarantee: those clip a `&str` by *character* count, while this cap is a
    /// *byte* budget over raw `Vec<u8>` pty bytes (a character can straddle two 4 KiB reads), so they do not
    /// fit here; a cut never goes through a multi-byte character, and the dropped amount is recorded.
    fn trim_to_cap(&mut self) {
        if self.cursor > 0 {
            self.output.drain(..self.cursor);
            self.cursor = 0;
        }
        if self.output.len() <= PTY_OUTPUT_BUFFER_CAP {
            return;
        }

        let mut cut = self.output.len() - PTY_OUTPUT_BUFFER_CAP;
        while cut < self.output.len() && !is_utf8_lead_byte(self.output[cut]) {
            cut += 1;
        }
        // Nothing to drop without discarding the whole window (only reachable
        // for a cap smaller than one character); keep the buffer untouched.
        if cut == 0 {
            return;
        }

        self.output.drain(..cut);
        self.cursor = 0;
        self.dropped_bytes += cut as u64;
        self.pending_notice_bytes += cut as u64;
    }

    /// Reader side: take the unread window as sanitized text, prefixed (once)
    /// with a truncation notice when the cap discarded output from the head.
    fn take_readable(&mut self) -> String {
        let end = self.output.len();
        let text =
            sanitize_terminal_output(&String::from_utf8_lossy(&self.output[self.cursor..end]));
        self.cursor = end;

        let dropped = std::mem::take(&mut self.pending_notice_bytes);
        if dropped == 0 {
            text
        } else {
            format!("{}{text}", truncation_notice(dropped))
        }
    }
}

/// Notice prefixed to `pty_read`/`pty_write` output when the ring buffer had to
/// discard older bytes, so the caller can tell the head was dropped.
fn truncation_notice(dropped_bytes: u64) -> String {
    format!(
        "[{dropped_bytes} bytes of earlier output discarded: interactive PTY buffer capped at {PTY_OUTPUT_BUFFER_CAP} bytes]\n"
    )
}

pub struct InteractivePtySession {
    pub id: String,
    pub pid: Option<u32>,
    writer: std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
    shared_buf: std::sync::Arc<std::sync::Mutex<SharedBuffer>>,
    /// Kept so the pty master fd is closed when the session goes away; the
    /// reader thread then sees EOF and exits.
    _master: Box<dyn portable_pty::MasterPty + Send>,
    /// Child handle + exactly-once reap state (see [`ChildReaper`]).
    reaper: ChildReaper,
    /// Manager-owned holding area for children that survive the teardown grace
    /// window; the manager's janitor task reaps them later.
    deferred_reaps: std::sync::Arc<std::sync::Mutex<DeferredReapQueue>>,
}

impl InteractivePtySession {
    /// Kill the process group + child and reap it exactly once. Idempotent.
    ///
    /// The reap grace is deliberately short and non-blocking: this runs on an async worker thread (with the
    /// session map lock held for the `pty_close`/eviction paths), so a child that has not exited in time is
    /// handed to the manager's [`DeferredReapQueue`] instead of being waited on blocking — and is never
    /// simply dropped unreaped.
    fn shutdown_and_reap(&mut self) {
        let outcome = self.reaper.kill_and_reap(PTY_REAP_GRACE_INTERACTIVE, false);
        if let Some(status) = &outcome.status {
            tracing::debug!("pty session '{}' child reaped: {status}", self.id);
        }
        if !outcome.reaped {
            let mut queue = self
                .deferred_reaps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.reaper.defer_to(&mut queue) {
                tracing::warn!(
                    "pty session '{}' child still running after SIGKILL; reaping deferred (queue len {})",
                    self.id,
                    queue.len()
                );
            }
        }
    }
}

impl Drop for InteractivePtySession {
    fn drop(&mut self) {
        // Covers every removal path: `pty_close`, a re-`pty_spawn` on the same
        // id, the idle-session eviction in the janitor, and manager teardown.
        self.shutdown_and_reap();
    }
}

pub struct PtyManager {
    sessions: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, InteractivePtySession>>,
    >,
    /// Children whose teardown grace expired before they exited. Drained by the
    /// janitor task started in [`PtyManager::new`] — bounded, and no reaper
    /// thread is spawned per command.
    deferred_reaps: std::sync::Arc<std::sync::Mutex<DeferredReapQueue>>,
}

impl Default for PtyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyManager {
    pub fn new() -> Self {
        let mgr = Self {
            sessions: std::sync::Arc::new(
                tokio::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            deferred_reaps: std::sync::Arc::new(std::sync::Mutex::new(DeferredReapQueue::new())),
        };

        let sessions_clone = mgr.sessions.clone();
        let deferred_clone = mgr.deferred_reaps.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    {
                        let mut map = sessions_clone.lock().await;
                        let now = std::time::Instant::now();
                        map.retain(|key, session| {
                            // Reap children that exited on their own right away, so
                            // they do not sit in the process table as `<defunct>`
                            // until the session is closed.
                            if let Some(status) = session.reaper.reap_if_exited() {
                                tracing::debug!("pty session '{key}' child exited: {status}");
                            }
                            let idle_time = {
                                let buf = session
                                    .shared_buf
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                now.duration_since(buf.last_activity)
                            };
                            if idle_time > Duration::from_secs(300) {
                                tracing::warn!(
                                    "PTY session '{}' timed out after 300s of inactivity. Reaping.",
                                    key
                                );
                                false
                            } else {
                                true
                            }
                        });
                    }

                    // Same single task reaps the deferred handles: no extra threads.
                    let reaped = {
                        let mut queue = deferred_clone
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        queue.drain()
                    };
                    if reaped > 0 {
                        tracing::debug!("reaped {reaped} deferred pty child(ren)");
                    }
                }
            });
        }

        mgr
    }

    pub async fn spawn(
        &self,
        id: &str,
        command_str: &str,
        cwd: &std::path::Path,
        rows: u16,
        cols: u16,
    ) -> Result<String, ToolError> {
        let key = id.trim().to_string();
        self.close(&key).await;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: if rows > 0 { rows } else { 24 },
                cols: if cols > 0 { cols } else { 80 },
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ToolError::Execution(anyhow::anyhow!("Failed to create PTY: {e}")))?;

        let cmd = build_sandboxed_command(command_str, cwd)?;

        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            ToolError::Execution(anyhow::anyhow!("Failed to spawn command in PTY: {e}"))
        })?;

        let pid = child.process_id();
        drop(pair.slave);
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| ToolError::Execution(anyhow::anyhow!("Failed to take PTY writer: {e}")))?;
        let mut reader = pair.master.try_clone_reader().map_err(|e| {
            ToolError::Execution(anyhow::anyhow!("Failed to clone PTY reader: {e}"))
        })?;

        let shared_buf = std::sync::Arc::new(std::sync::Mutex::new(SharedBuffer::new()));

        let shared_buf_reader = shared_buf.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let mut lock = shared_buf_reader
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        lock.is_alive = false;
                        break;
                    }
                    Ok(n) => {
                        // Single writer side: `push` is where the ring cap
                        // (PTY_OUTPUT_BUFFER_CAP) is enforced.
                        let mut lock = shared_buf_reader
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        lock.push(&buf[..n]);
                    }
                    Err(_) => {
                        let mut lock = shared_buf_reader
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        lock.is_alive = false;
                        break;
                    }
                }
            }
        });

        let session = InteractivePtySession {
            id: key.clone(),
            pid,
            writer: std::sync::Arc::new(std::sync::Mutex::new(writer)),
            shared_buf: shared_buf.clone(),
            _master: pair.master,
            reaper: ChildReaper::new(PtyChildHandle { child }, pid.map(|p| p as i32)),
            deferred_reaps: self.deferred_reaps.clone(),
        };

        {
            let mut map = self.sessions.lock().await;
            map.insert(key, session);
        }

        tokio::time::sleep(Duration::from_millis(300)).await;

        let initial_output = {
            let mut lock = shared_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.take_readable()
        };

        Ok(initial_output)
    }

    pub async fn write(
        &self,
        id: &str,
        input: &str,
        wait_ms: u64,
    ) -> Result<(String, bool), ToolError> {
        let key = id.trim();
        let (writer, shared_buf) = {
            let map = self.sessions.lock().await;
            let session = map.get(key).ok_or_else(|| {
                ToolError::Execution(anyhow::anyhow!(
                    "PTY session '{}' not found or was terminated. Please call pty_spawn to start a new terminal session.",
                    id
                ))
            })?;
            (session.writer.clone(), session.shared_buf.clone())
        };

        {
            let mut w = writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            use std::io::Write;
            w.write_all(input.as_bytes()).map_err(|e| {
                ToolError::Execution(anyhow::anyhow!("Failed to write to PTY: {e}"))
            })?;
            w.flush()
                .map_err(|e| ToolError::Execution(anyhow::anyhow!("Failed to flush PTY: {e}")))?;
            let mut buf = shared_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            buf.last_activity = std::time::Instant::now();
        }

        let wait_dur = Duration::from_millis(if wait_ms > 0 { wait_ms } else { 300 });
        tokio::time::sleep(wait_dur).await;

        let (new_output, is_alive) = {
            let mut lock = shared_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let out = lock.take_readable();
            (out, lock.is_alive)
        };

        Ok((new_output, is_alive))
    }

    pub async fn read(&self, id: &str, wait_ms: u64) -> Result<(String, bool), ToolError> {
        let key = id.trim();
        let shared_buf = {
            let map = self.sessions.lock().await;
            let session = map.get(key).ok_or_else(|| {
                ToolError::Execution(anyhow::anyhow!(
                    "PTY session '{}' not found or was terminated. Please call pty_spawn to start a new terminal session.",
                    id
                ))
            })?;
            session.shared_buf.clone()
        };

        if wait_ms > 0 {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
            while tokio::time::Instant::now() < deadline {
                if crate::orchestrator::is_current_or_global_cancelled() {
                    return Err(ToolError::Execution(anyhow::anyhow!(
                        "PTY read aborted by cancellation signal"
                    )));
                }
                let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
                let step = rem.min(Duration::from_millis(50));
                tokio::time::sleep(step).await;
            }
        }

        let (new_output, is_alive) = {
            let mut lock = shared_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let out = lock.take_readable();
            lock.last_activity = std::time::Instant::now();
            (out, lock.is_alive)
        };

        Ok((new_output, is_alive))
    }

    /// Close a session: kill the process group, reap the child exactly once,
    /// and drop the session. Repeated calls are harmless (idempotent).
    pub async fn close(&self, id: &str) -> bool {
        let key = id.trim();
        let mut map = self.sessions.lock().await;
        match map.remove(key) {
            Some(mut session) => {
                // Reap explicitly instead of relying on `Drop` alone, so the child is
                // verifiably reaped (absent from the process table) before `close` returns;
                // `Drop` stays the safety net for the eviction/replace paths.
                session.shutdown_and_reap();
                true
            }
            None => false,
        }
    }

    pub async fn list(&self) -> Vec<Value> {
        let map = self.sessions.lock().await;
        let now = std::time::Instant::now();
        map.values()
            .map(|s| {
                let buf = s
                    .shared_buf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let idle_secs = now.duration_since(buf.last_activity).as_secs();
                serde_json::json!({
                    "session_id": s.id,
                    "pid": s.pid,
                    "is_alive": buf.is_alive,
                    "idle_seconds": idle_secs,
                    // Retained ring window, not a cumulative counter: the cumulative amount
                    // is `dropped_bytes` (head bytes discarded by PTY_OUTPUT_BUFFER_CAP), and
                    // `total_bytes_read` reports the same retained window.
                    "retained_bytes": buf.output.len(),
                    "total_bytes_read": buf.output.len(),
                    "dropped_bytes": buf.dropped_bytes,
                    "truncated": buf.dropped_bytes > 0,
                    "child_reaped": s.reaper.is_reaped(),
                    "child_exit_status": s.reaper.exit_status().map(|st| st.exit_code()),
                })
            })
            .collect()
    }
}

pub static GLOBAL_PTY_MANAGER: std::sync::LazyLock<PtyManager> =
    std::sync::LazyLock::new(PtyManager::new);

/// Tool handler: `pty_spawn`.
pub fn pty_spawn(args: &Value) -> Result<ToolResult, ToolError> {
    let id = args
        .get("id")
        .or_else(|| args.get("session_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArguments {
            tool: TOOL_PTY_SPAWN.into(),
            detail: "missing string field `id` or `session_id`".into(),
        })?;

    let command =
        args.get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::BadArguments {
                tool: TOOL_PTY_SPAWN.into(),
                detail: "missing string field `command`".into(),
            })?;

    let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
    let cols = args.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
    let cwd = crate::harness::get_workspace_root();

    let output =
        crate::harness::block_on_safe(GLOBAL_PTY_MANAGER.spawn(id, command, &cwd, rows, cols))?;

    Ok(ToolResult::ok(format!(
        "PTY session '{id}' started.\nOutput:\n{output}"
    )))
}

/// Tool handler: `pty_write`.
pub fn pty_write(args: &Value) -> Result<ToolResult, ToolError> {
    let id = args
        .get("id")
        .or_else(|| args.get("session_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArguments {
            tool: TOOL_PTY_WRITE.into(),
            detail: "missing string field `id` or `session_id`".into(),
        })?;

    let input =
        args.get("input")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::BadArguments {
                tool: TOOL_PTY_WRITE.into(),
                detail: "missing string field `input`".into(),
            })?;

    let wait_ms = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(300);

    let (output, is_alive) =
        crate::harness::block_on_safe(GLOBAL_PTY_MANAGER.write(id, input, wait_ms))?;

    Ok(ToolResult::ok(format!(
        "Status: alive={is_alive}\nOutput:\n{output}"
    )))
}

/// Tool handler: `pty_read`.
pub fn pty_read(args: &Value) -> Result<ToolResult, ToolError> {
    let id = args
        .get("id")
        .or_else(|| args.get("session_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArguments {
            tool: TOOL_PTY_READ.into(),
            detail: "missing string field `id` or `session_id`".into(),
        })?;

    let wait_ms = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(0);

    let (output, is_alive) = crate::harness::block_on_safe(GLOBAL_PTY_MANAGER.read(id, wait_ms))?;

    Ok(ToolResult::ok(format!(
        "Status: alive={is_alive}\nOutput:\n{output}"
    )))
}

/// Tool handler: `pty_close`.
pub fn pty_close(args: &Value) -> Result<ToolResult, ToolError> {
    let id = args
        .get("id")
        .or_else(|| args.get("session_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArguments {
            tool: TOOL_PTY_CLOSE.into(),
            detail: "missing string field `id` or `session_id`".into(),
        })?;

    let closed = crate::harness::block_on_safe(GLOBAL_PTY_MANAGER.close(id));

    if closed {
        Ok(ToolResult::ok(format!("PTY session '{id}' closed.")))
    } else {
        Ok(ToolResult::ok(format!(
            "PTY session '{id}' was not running."
        )))
    }
}

/// Tool handler: `pty_list`.
pub fn pty_list(_args: &Value) -> Result<ToolResult, ToolError> {
    let list = crate::harness::block_on_safe(GLOBAL_PTY_MANAGER.list());

    Ok(ToolResult::ok(
        serde_json::to_string_pretty(&list).unwrap_or_else(|_| "[]".to_string()),
    ))
}

/// Kill an entire process group with SIGKILL using a negative pid.
///
/// A negative pid targets the process group whose group id equals `|pid|`,
/// guaranteeing that all children (subshells, debuggers, REPLs) die too.
#[cfg(unix)]
pub fn kill_process_group(pid: i32) -> Result<(), std::io::Error> {
    let ret = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if ret == 0 {
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        // ESRCH just means the group is already gone — that's fine.
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(err)
        }
    }
}

#[cfg(not(unix))]
pub fn kill_process_group(_pid: i32) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(test)]
#[path = "pty_tests.rs"]
mod tests;
