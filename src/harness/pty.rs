//! PTY wrapper: shell execution with process-group isolation.
//!
//! REQ-TOOL-001: shell execution via `run_command` uses portable-pty.
//! Every command is wrapped as:
//! ```text
//! sh -c "stty -echo; ulimit -f 4194304 2>/dev/null || ulimit -f 2097152 2>/dev/null; <command>"
//! ```
//! - `stty -echo` stops the echoed command line from polluting stdout.
//! - `ulimit -f` caps each command's file write to 2 GiB (4194304 × 512 B),
//!   falling back to 1 GiB (2097152 × 512 B) on platforms where the larger
//!   limit is rejected. This matches marmennill-cli's local tool execution.
//!
//! On command completion, timeout (strict 300 s), or teardown, the manager
//! issues `libc::kill(-pid, libc::SIGKILL)` to the *entire process group*
//! (negative pid) followed by a child kill, so no lingering subshells,
//! debuggers, or REPLs survive.
//!
//! **Every spawned child is then reaped.** The `Child` handle lives in the
//! session struct (`ChildReaper`), and the kill path always ends in a bounded
//! `try_wait()` poll (plus a blocking `wait()` for one-shot commands), so no
//! `<defunct>` entry accumulates across a long session and the process table
//! cannot be exhausted (`EAGAIN` on the next spawn). Reaping happens exactly
//! once per child: completion, timeout, cancellation, eviction, `pty_close`,
//! and `Drop` all funnel through the same idempotent path, and a child that
//! outlives the teardown grace window is handed to the bounded
//! [`DeferredReapQueue`] drained by the single janitor task (never a thread per
//! command).
//!
//! **Interactive output is a bounded ring.** `SharedBuffer.output` retains at
//! most [`PTY_OUTPUT_BUFFER_CAP`] bytes (256 KiB): consumed bytes are reclaimed
//! on every push, and an over-cap unread tail loses its *head* at a UTF-8 char
//! boundary, with the loss counted and reported to the reader
//! ([`truncation_notice`], `dropped_bytes`/`truncated` in `pty_list`).
//!
//! Sandbox gating (t-034b) never depends on the executable's **name**: whether
//! the Landlock re-entry is applied is decided by [`should_apply_sandbox`] from
//! the resolved executable path (`current_exe()` canonicalized), platform
//! support, and an explicit opt-out. It is enabled by default (fail-closed); any
//! skip is warned about, and an unresolvable executable hard-fails rather than
//! degrading to a plain `sh -c`.
//!
//! All captured output is passed through `sanitize_terminal_output`, which
//! strips OSC sequences and other non-printable terminal artifacts, matching
//! marmennill-cli's `sanitize_terminal_output`.
//!
//! NOTE: `unsafe_op_in_unsafe_fn` is a hard error in edition 2024, so any
//! `libc::kill` call must be wrapped in an explicit `unsafe {}` block.

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

/// Hard byte cap of the interactive output ring (`SharedBuffer.output`), in
/// bytes: **256 KiB** (`256 * 1024` = 262 144 B).
///
/// The buffer is a ring window, not a log: bytes a reader already consumed are
/// reclaimed on every push, and if the *unread* tail still exceeds this cap the
/// oldest bytes are dropped (at a UTF-8 char boundary) and the loss is counted
/// and reported to the next reader. 256 KiB is orders of magnitude more output
/// than one `pty_read` round-trip realistically consumes, yet bounded, so a
/// chatty command (`yes`, `tail -f`, a build log) can no longer grow the process
/// without limit. Enforced in exactly one place: [`SharedBuffer::trim_to_cap`],
/// called from [`SharedBuffer::push`] (the PTY reader thread).
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

/// Strip OSC sequences and other non-printable terminal artifacts from output.
///
/// This is the exact implementation used by marmennill-cli's local tool execution
/// (`caesar/marmennill-cli/src/main.rs` and `caesar/src/agent/types.rs`):
/// - OSC sequences (`ESC ] <n> ; ... BEL` or `ESC ] <n> ; ... ESC \`) are removed.
/// - Bell (`\x07`) and backspace (`\x08`) are removed.
/// - Other control characters below ` ` (0x20) are removed, except `\n`, `\r`,
///   `\t`, and `\x1b` (ESC, which is preserved so CSI color codes survive).
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

// ---------------------------------------------------------------------------
// Child reaping — every spawned child is `wait()`ed exactly once
// ---------------------------------------------------------------------------

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

/// Exactly-once reap bookkeeping for one spawned child.
///
/// A child is waited on at most once (a second `wait` on a reaped pid could
/// otherwise block forever or hit a recycled pid), its exit status is recorded
/// once, and a handle is never dropped while the child is still unreaped: the
/// session types either reap it via [`ChildReaper::kill_and_reap`] or hand it to
/// a [`DeferredReapQueue`] via [`ChildReaper::defer_to`].
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

    /// `SIGKILL` the whole process group (negative pid, so backgrounded
    /// subshells, debuggers and REPLs die too), then the child, then reap the
    /// child exactly once.
    ///
    /// * Already reaped ⇒ pure no-op: nothing is signalled (the pid may have
    ///   been recycled) and nothing is waited on again. This makes the repeated
    ///   completion/timeout/cancellation/`Drop` teardown passes safe.
    /// * The `try_wait()` poll is bounded by `grace`. If the child is still
    ///   alive afterwards, `wait_after_grace` chooses between the blocking
    ///   `wait()` — correct for the one-shot `run_command` path, where the child
    ///   has been `SIGKILL`ed so this returns promptly and no zombie can
    ///   survive — and returning `reaped: false`, for the interactive path that
    ///   must not block the async runtime and defers via [`ChildReaper::defer_to`].
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
                // waitpid failed: the child is gone, retire the handle.
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

// ---------------------------------------------------------------------------
// Sandbox gating — decided from authoritative inputs, never from the
// executable's NAME (t-034b)
//
// The previous gate compared `file_name(current_exe())` against the literal
// name `marmel`, which silently disabled Landlock for every
// renamed/copied/installed binary (`marmel-dev`, `target/debug/marmel-x`, …)
// and enabled it for unrelated programs that merely happened to be called
// `marmel`. Gating now comes from the **resolved** executable path
// (`current_exe()` canonicalized), platform support, and an explicit opt-out.
//
// The re-entry protocol is implemented by the `marmel` **bin target**
// (`src/main.rs`), not by the library, so a Cargo test-harness binary
// (`cfg!(test)`) cannot re-enter — that one skip is compile-time, cannot be
// reached by renaming anything, and is logged as a warning like every other
// skip. Follow-up (hand-off): apply Landlock in-process via `pre_exec` so no
// self-exec is needed at all.
// ---------------------------------------------------------------------------

/// argv sentinel understood by `src/main.rs`: re-executing the **same resolved
/// binary** with it applies Landlock in the child and then execs `sh -c <cmd>`.
///
/// HAND-OFF (`src/main.rs` is outside this task's file ownership): `main.rs`
/// still spells this literal out; it should reference [`SANDBOX_EXEC_ARG`] so
/// the two ends of the protocol can never drift apart.
pub const SANDBOX_EXEC_ARG: &str = "--internal-sandbox-exec";

/// The environment fallback for the Landlock sandbox opt-out, deliberately strict.
///
/// The crate has a typed knob for the same switch — `sandbox_disabled = true` in
/// `marmel.toml` ([`crate::config::Config::sandbox_disabled`], read through
/// [`crate::config::sandbox_disabled`]) — and **either source opts out**: the env
/// value parsed here and the config value are merged by
/// [`crate::config::sandbox_opt_out`] and handed to [`SandboxInputs::opt_out`]
/// where the per-process decision is taken. The env var remains as an equivalent
/// escape hatch for opting one process out without editing a config file, and
/// every skip it produces is warned about by [`log_sandbox_decision`].
///
/// Only `1`, `true`, `yes`, `on` (case-insensitive, surrounding blanks ignored)
/// disable the sandbox. **Every other value — including `0`, `false`, `off`,
/// `no`, an empty value, or a typo — leaves it enabled (fail-closed).**
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
/// to know, and nothing else (no executable *name*, no Landlock probe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInputs<'a> {
    /// `true` when the platform can enforce Landlock at all (Linux).
    pub landlock_supported: bool,
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
    /// This platform has no Landlock (macOS/Windows/…): not a choice, but it
    /// must still be visible.
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
            SandboxSkip::UnsupportedPlatform => "platform has no Landlock".to_string(),
            SandboxSkip::TestHarnessExe => {
                format!("test-harness binary does not implement {SANDBOX_EXEC_ARG}")
            }
        }
    }
}

/// The outcome of [`should_apply_sandbox`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxDecision {
    /// Apply the Landlock re-entry by exec-ing `resolved_exe` with
    /// [`SANDBOX_EXEC_ARG`].
    Apply { resolved_exe: std::path::PathBuf },
    /// Run the command through plain `sh -c` — **never silently**: always
    /// accompanied by a `WARN` from [`log_sandbox_decision`].
    Skip {
        resolved_exe: Option<std::path::PathBuf>,
        skip: SandboxSkip,
    },
    /// Landlock is required here but the executable could not be resolved, so
    /// the command must **hard-fail** instead of degrading to `sh -c`.
    UnresolvedExe,
}

impl SandboxDecision {
    /// The chosen mode, stated in the log line at the decision point.
    pub fn mode(&self) -> &'static str {
        match self {
            SandboxDecision::Apply { .. } => "landlock-re-entry",
            SandboxDecision::Skip { .. } => "unsandboxed",
            SandboxDecision::UnresolvedExe => "hard-fail",
        }
    }
}

/// Decide whether the Landlock sandbox **must** be applied for the next command.
///
/// Contract — this is the entire point of the function:
///
/// * **Enabled by default (fail-closed).** Nothing about the executable's *name*
///   is consulted: a renamed / copied / installed binary (`marmel-dev`,
///   `target/debug/marmel-x`, `/usr/local/bin/mm`) is sandboxed exactly like one
///   named `marmel`, and an unrelated program that merely happens to be named
///   `marmel` is treated like any other host of this code.
/// * It is disabled **only** by an explicit, unambiguous opt-out
///   ([`OptOut::Explicit`]): the typed config knob `sandbox_disabled`
///   ([`crate::config::Config::sandbox_disabled`], attributed as
///   `sandbox_disabled=true`) or the `MARMEL_DISABLE_SANDBOX` env fallback
///   ([`SANDBOX_OPTOUT_ENV`]) — the two are merged by
///   [`crate::config::sandbox_opt_out`] and **either** source opts out. Empty,
///   malformed or falsy-looking values do **not** disable it.
/// * A Linux process that cannot resolve its own executable yields
///   [`SandboxDecision::UnresolvedExe`]; the caller must fail the command rather
///   than quietly run it unsandboxed.
/// * [`SandboxDecision::Skip`] exists only for a platform without Landlock, an
///   explicit opt-out, or a Cargo test-harness binary. Every skip is warned
///   about by [`log_sandbox_decision`] — running unsandboxed is never silent.
///
/// All inputs are injected, so this is pure logic: no `current_exe()`, no
/// Landlock availability probe, no `/dev/pts`, no environment mutation.
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

    // 2. No Landlock on this platform (the re-entry only exists on Linux).
    if !inputs.landlock_supported {
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
                "terminal commands are sandboxed via Landlock re-entry"
            );
        }
        SandboxDecision::Skip { skip, .. } => {
            let reason = skip.label();
            tracing::warn!(
                resolved_exe = %&resolved_exe,
                mode = decision.mode(),
                reason = %&reason,
                "pty sandbox SKIPPED: this command runs WITHOUT Landlock ({reason})"
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
        landlock_supported: cfg!(target_os = "linux"),
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

/// Turn a [`SandboxDecision`] into the argv (`program`, `args`) of the command
/// to spawn. Pure, so the re-entry argv is unit-testable without a PTY.
///
/// [`SandboxDecision::UnresolvedExe`] is deliberately a **hard error**: the
/// previous code fell back to a plain `sh -c` here, which is exactly the silent
/// fail-open this task removes.
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
        SandboxDecision::UnresolvedExe => Err(ToolError::Execution(anyhow::anyhow!(
            "refusing to run `{command}` unsandboxed: Landlock re-entry ({SANDBOX_EXEC_ARG}) needs a resolved executable path, but `current_exe()` is unavailable on this host. Run `marmel` from a resolvable path, or opt out explicitly with {SANDBOX_OPTOUT_ENV}=1."
        ))),
    }
}

/// Build the shell command for one `run_command`/PTY execution.
///
/// Landlock is applied on Linux through [`SANDBOX_EXEC_ARG`] re-entry decided by
/// [`should_apply_sandbox`] — from the resolved exe path, platform support and an
/// explicit opt-out, **never** from the executable's name. `Err` is returned when
/// the sandbox is required but cannot be applied (fail-closed). On Windows the
/// command is built as before (no Landlock exists there; the skip is logged).
pub fn build_sandboxed_command(
    command: &str,
    cwd: &std::path::Path,
) -> Result<CommandBuilder, ToolError> {
    if cfg!(target_os = "windows") {
        let trimmed = command.trim();
        tracing::warn!(
            resolved_exe = %resolved_exe_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<unresolved>".to_string()),
            mode = "unsandboxed",
            reason = "platform has no Landlock",
            "pty sandbox SKIPPED: Windows has no Landlock, this command runs unsandboxed"
        );
        if trimmed.eq_ignore_ascii_case("cmd")
            || trimmed.eq_ignore_ascii_case("cmd.exe")
            || trimmed.eq_ignore_ascii_case("powershell")
            || trimmed.eq_ignore_ascii_case("powershell.exe")
        {
            let mut cmd = CommandBuilder::new(trimmed);
            cmd.cwd(cwd);
            Ok(cmd)
        } else {
            let mut cmd = CommandBuilder::new("cmd");
            cmd.args(["/C", command]);
            cmd.cwd(cwd);
            Ok(cmd)
        }
    } else {
        let (program, args) =
            sandbox_command_argv(&sandbox_decision_for_current_process(), command, cwd)?;
        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
        Ok(cmd)
    }
}

impl PtySession {
    /// Wrap `command` in the REQ-TOOL-001 sandbox and spawn it into a PTY.
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

/// Core PTY execution: spawn the sandbox, read output with a timeout, and
/// always tear down the process group **and reap the child** afterwards.
///
/// The `timeout` is strict (default 300 s) and preempts a hung child by
/// SIGKILLing the entire process group. Every exit path — normal completion,
/// timeout, cancellation signal, abrupt disconnect, or an error from `?` before
/// the loop even starts — ends in [`PtySession::teardown`] /
/// `Drop as PtySession`, which kills the group, kills the child, and `wait()`s
/// it exactly once, so no `<defunct>` child survives a `run_command`.
pub fn run_command_pty(command: &str, timeout: Duration) -> Result<String, ToolError> {
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

// ---------------------------------------------------------------------------
// Interactive Multi-Turn PTY Manager
// ---------------------------------------------------------------------------

/// `true` when `byte` may begin a UTF-8 code point, i.e. it is **not** a
/// `10xxxxxx` continuation byte.
///
/// Byte-level char-boundary test for the ring buffer trim in
/// [`SharedBuffer::trim_to_cap`]: advancing the cut to the next lead byte means
/// the retained window never starts in the middle of a multi-byte character
/// (never a split `å`/`é`/`🙂`, and never a mid-character byte slice).
fn is_utf8_lead_byte(byte: u8) -> bool {
    byte & 0b1100_0000 != 0b1000_0000
}

/// Bounded retention window for one interactive PTY session.
///
/// `output` is a ring window, not an append-only log:
/// * bytes a reader has already consumed (`..cursor`) are reclaimed on every
///   push — once handed out they are pure duplication;
/// * if the *unread* tail still exceeds [`PTY_OUTPUT_BUFFER_CAP`], the oldest
///   bytes are dropped at a UTF-8 char boundary and counted in `dropped_bytes`,
///   with the amount since the last read queued in `pending_notice_bytes` so
///   the next reader sees that the head was discarded.
///
/// Both trimming steps live in [`SharedBuffer::trim_to_cap`], called from the
/// single writer side ([`SharedBuffer::push`], i.e. the PTY reader thread), so
/// `output.len()` never meaningfully exceeds the cap no matter how chatty the
/// command is (`yes`, `tail -f`, a build log).
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

    /// Enforce [`PTY_OUTPUT_BUFFER_CAP`].
    ///
    /// 1. Reclaim the already-delivered prefix `..cursor`.
    /// 2. If the tail is still over the cap, drop from the head. The cut point
    ///    is advanced to the next UTF-8 char boundary with [`is_utf8_lead_byte`],
    ///    the byte-level counterpart of the `text_util::truncate_chars` /
    ///    `truncate_with_ellipsis` guarantee: those clip a `&str` by *character*
    ///    count, while this cap is a *byte* budget over raw `Vec<u8>` pty bytes
    ///    (bytes arrive in 4 KiB reads and may split a character across reads),
    ///    so they do not fit here. A cut is never taken through a multi-byte
    ///    character, and the dropped amount is recorded so the loss is visible
    ///    instead of silent.
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
    /// with a truncation notice when the cap discarded earlier output.
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
    /// The reap grace is deliberately short and non-blocking: this runs on an
    /// async worker thread (with the session map lock held for the
    /// `pty_close`/eviction paths), so a child that has not exited in time is
    /// handed to the manager's [`DeferredReapQueue`] instead of being waited on
    /// blocking — and is never simply dropped unreaped.
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
        tokio::spawn(async move {
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
                // Reap explicitly instead of relying on `Drop` alone, so the
                // child is verifiably gone from the process table before
                // `close` returns (`Drop` stays the safety net for the
                // eviction/replace paths).
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
                    // Retained ring window, not a cumulative counter: the
                    // cumulative amount is `dropped_bytes` (head bytes discarded
                    // because of PTY_OUTPUT_BUFFER_CAP). `total_bytes_read` is
                    // kept as the historical alias of `retained_bytes`.
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
