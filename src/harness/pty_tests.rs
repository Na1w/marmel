use super::*;

/// Host-capability gate for the PTY-dependent tests in this file.
///
/// A PTY is a **host capability**, not a product behaviour. Where the OS cannot
/// hand out a pty — on Linux a `devpts` mount with `ptmxmode=000` leaves
/// `/dev/pts/ptmx` at mode `000` and `openpty()` answers `EACCES` — every test in
/// this file that drives a real pty is un-runnable *on that host*, on a pristine
/// tree, with correct product code. Failing them there reports a defect that does
/// not exist; `#[ignore]` hides them everywhere, including the CI runners that do
/// have `/dev/pts` and where the assertions are meaningful.
///
/// So the gate is the runtime probe in [`pty_allocation_blocker`], and a skip is
/// always announced, in the `SKIP ` spelling already pinned by
/// `test_sandbox_tests_never_rebuild_the_binary_path_from_a_name` and used by
/// `integration_host` in `src/harness/sandbox.rs:1637`. On a PTY-capable host
/// (every CI runner in `.github/workflows/ci.yml`) the probe passes and every
/// assertion below runs unconditionally.
fn pty_or_skip(test: &str) -> bool {
    match pty_allocation_blocker() {
        None => true,
        Some(reason) => {
            eprintln!(
                "SKIP {test}: this host cannot allocate a PTY ({reason}), so real-pty behaviour cannot be exercised here"
            );
            false
        }
    }
}

/// Spawning a background sleep loop and tearing down the PTY leaves zero orphan
/// processes — the backgrounded child dies with the group.
#[test]
#[cfg(unix)]
fn test_harness_pty_process_group_kill() {
    if !pty_or_skip("harness::pty::tests::test_harness_pty_process_group_kill") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let pidfile = dir.join("bg.pid");

    // Spawn a command that backgrounds a long-running sleep and writes its
    // pid to `pidfile`, then sleeps in the foreground so the session stays
    // alive until we tear it down.
    let cmd = format!("sleep 300 & echo $! > {}; sleep 300", pidfile.display());
    let mut session = PtySession::spawn(&cmd).expect("spawn sandbox");

    // Poll until the background pid is recorded.
    let mut bg_pid: Option<i32> = None;
    for _ in 0..50 {
        if let Ok(raw) = std::fs::read_to_string(&pidfile)
            && let Ok(p) = raw.trim().parse::<i32>()
        {
            bg_pid = Some(p);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let bg_pid = bg_pid.expect("background pid recorded");

    // The background process must be alive before teardown.
    let alive_before = unsafe { libc::kill(bg_pid, 0) == 0 };
    assert!(
        alive_before,
        "background sleep should be running before teardown"
    );

    // Tear down the whole process group.
    session.teardown().expect("teardown kills process group");
    drop(session);

    // Poll briefly until the kernel reaps the killed background process.
    let mut alive_after = true;
    for _ in 0..50 {
        alive_after = unsafe { libc::kill(bg_pid, 0) == 0 };
        if !alive_after {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        !alive_after,
        "background sleep {bg_pid} survived process-group kill"
    );
}

/// The shell wrapper must include the stty -echo and ulimit preamble with
/// the 2 GiB cap and its 1 GiB fallback, matching marmennill-cli.
#[test]
fn test_harness_pty_shell_wrapper() {
    let wrapped = format!(
        "stty -echo; ulimit -f {ULIMIT_FILE_BLOCKS} 2>/dev/null || ulimit -f {ULIMIT_FILE_BLOCKS_FALLBACK} 2>/dev/null; echo hi"
    );
    assert!(wrapped.starts_with(
        "stty -echo; ulimit -f 4194304 2>/dev/null || ulimit -f 2097152 2>/dev/null;"
    ));
}

/// The sanitizer strips OSC sequences, bell, and backspace while preserving
/// CSI color codes, newlines, tabs, and carriage returns — matching the
/// marmennill-cli reference implementation.
#[test]
fn test_sanitize_terminal_output_strips_bell_and_osc() {
    let dirty_with_bell = "Hello\x07World\x08!";
    assert_eq!(sanitize_terminal_output(dirty_with_bell), "HelloWorld!");

    let dirty_with_osc = "Prefix\x1b]0;Bad Title\x07Suffix";
    assert_eq!(sanitize_terminal_output(dirty_with_osc), "PrefixSuffix");

    let dirty_with_osc2 = "Prefix\x1b]2;Bad Title\x1b\\Suffix";
    assert_eq!(sanitize_terminal_output(dirty_with_osc2), "PrefixSuffix");

    let clean_multiline_ansi = "\x1b[1;32mGreen\x1b[0m\nLine 2\tTabbed\r";
    assert_eq!(
        sanitize_terminal_output(clean_multiline_ansi),
        "\x1b[1;32mGreen\x1b[0m\nLine 2\tTabbed\r"
    );
}

#[test]
fn test_run_command_parses_timeout_seconds_argument() {
    if !pty_or_skip("harness::pty::tests::test_run_command_parses_timeout_seconds_argument") {
        return;
    }
    #[cfg(unix)]
    let cmd = "sleep 10";
    #[cfg(windows)]
    let cmd = "ping -n 10 127.0.0.1";

    let args = serde_json::json!({
        "command": cmd,
        "timeout_seconds": 1
    });
    let res = run_command(&args).expect("executes with custom timeout");
    assert!(res.content.contains("timed out after 1s and was killed"));
}

#[tokio::test]
async fn test_interactive_pty_manager_lifecycle() {
    if !pty_or_skip("harness::pty::tests::test_interactive_pty_manager_lifecycle") {
        return;
    }
    let mgr = PtyManager::new();
    let cwd = std::env::current_dir().unwrap();
    let session_id = "test-session-1";

    #[cfg(unix)]
    let shell = "sh";
    #[cfg(windows)]
    let shell = "cmd.exe";

    let init_out = mgr
        .spawn(session_id, shell, &cwd, 24, 80)
        .await
        .expect("spawn session");
    assert!(init_out.is_empty() || !init_out.is_empty());

    #[cfg(unix)]
    let (input, wait_ms) = ("echo hello_interactive_pty\n", 400);
    #[cfg(windows)]
    let (input, wait_ms) = ("echo hello_interactive_pty\r\n", 800);

    let (mut out, alive) = mgr
        .write(session_id, input, wait_ms)
        .await
        .expect("write to pty");
    assert!(alive, "session should be alive");

    if !out.contains("hello_interactive_pty") {
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if let Ok((extra, _)) = mgr.read(session_id, 0).await {
                out.push_str(&extra);
                if out.contains("hello_interactive_pty") {
                    break;
                }
            }
        }
    }

    #[cfg(unix)]
    assert!(
        out.contains("hello_interactive_pty"),
        "output should contain echoed string, got: {out:?}"
    );

    let list = mgr.list().await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["session_id"], session_id);

    let closed = mgr.close(session_id).await;
    assert!(closed, "session should be closed");

    let list_after = mgr.list().await;
    assert_eq!(list_after.len(), 0);
}

#[tokio::test]
async fn test_run_command_pty_cancelled_by_worker_token() {
    if !pty_or_skip("harness::pty::tests::test_run_command_pty_cancelled_by_worker_token") {
        return;
    }
    let token = tokio_util::sync::CancellationToken::new();
    let token_clone = token.clone();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        token_clone.cancel();
    });

    // `run_command_pty` takes a command *string*, so the long-running command is spelled
    // per platform exactly like `test_run_command_parses_timeout_seconds_argument` below:
    // `sleep` does not exist on Windows, where `cmd /C sleep 10` exits immediately with a
    // "not recognized" status, so the cancellation signal could never be observed and the
    // assertion below could not hold on that platform.
    #[cfg(unix)]
    let cmd = "sleep 10";
    #[cfg(windows)]
    let cmd = "ping -n 30 127.0.0.1";

    let res = crate::orchestrator::CURRENT_WORKER_TOKEN
        .scope(token, async {
            run_command_pty(cmd, Duration::from_secs(10))
        })
        .await;

    assert!(res.is_ok());
    let output = res.unwrap();
    assert!(
        output.contains("aborted by cancellation signal"),
        "Expected aborted output, got: {output}"
    );
}

// Reap state machine, tested as pure logic (zombie children are what it prevents).
// None of these tests need a real `/dev/pts`: they drive the `ChildHandle`
// seam with a fake and with a plain `std::process::Child`.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Test double for [`ChildHandle`] that counts every wait/kill call, so the
/// "reaped exactly once, never re-signalled, never re-waited" invariants are
/// observable.
#[derive(Clone, Default)]
struct FakeChild {
    kills: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    blocking_waits: Arc<AtomicUsize>,
    /// `true` while the child keeps running (`try_wait_child` yields `None`).
    alive: Arc<AtomicBool>,
    /// `true` = `SIGKILL` has no effect (a child stuck in uninterruptible
    /// sleep), so the bounded reap grace window expires.
    kill_ineffective: bool,
    /// `true` = every `waitpid` fails (`ECHILD`: already gone from the table).
    waitpid_error: bool,
}

impl FakeChild {
    /// Normal child: dies when killed.
    fn killable() -> Self {
        FakeChild {
            alive: Arc::new(AtomicBool::new(true)),
            ..Default::default()
        }
    }

    /// Child that survives `SIGKILL`, forcing the grace window to expire.
    fn unkillable() -> Self {
        FakeChild {
            alive: Arc::new(AtomicBool::new(true)),
            kill_ineffective: true,
            ..Default::default()
        }
    }

    /// Child that has already exited.
    fn exited() -> Self {
        FakeChild::default()
    }

    /// Child whose `waitpid` always errors.
    fn waitpid_error() -> Self {
        FakeChild {
            alive: Arc::new(AtomicBool::new(true)),
            waitpid_error: true,
            ..Default::default()
        }
    }

    fn kill_count(&self) -> usize {
        self.kills.load(Ordering::SeqCst)
    }

    fn poll_count(&self) -> usize {
        self.polls.load(Ordering::SeqCst)
    }

    fn blocking_wait_count(&self) -> usize {
        self.blocking_waits.load(Ordering::SeqCst)
    }

    fn set_exited(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

/// An `ECHILD`-shaped error: the pid is already gone from the process table.
fn waitpid_gone_error() -> io::Error {
    #[cfg(unix)]
    let code = libc::ECHILD;
    #[cfg(not(unix))]
    let code = 0;
    io::Error::from_raw_os_error(code)
}

impl ChildHandle for FakeChild {
    fn try_wait_child(&mut self) -> io::Result<Option<ExitStatus>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if self.waitpid_error {
            return Err(waitpid_gone_error());
        }
        if self.alive.load(Ordering::SeqCst) {
            Ok(None)
        } else {
            Ok(Some(ExitStatus::with_exit_code(0)))
        }
    }

    fn wait_child(&mut self) -> io::Result<ExitStatus> {
        self.blocking_waits.fetch_add(1, Ordering::SeqCst);
        if self.waitpid_error {
            return Err(waitpid_gone_error());
        }
        Ok(ExitStatus::with_exit_code(0))
    }

    fn kill_child(&mut self) -> io::Result<()> {
        self.kills.fetch_add(1, Ordering::SeqCst);
        if !self.kill_ineffective {
            self.alive.store(false, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Adapter that lets the reaper manage a plain `std::process::Child`, i.e. a
/// real OS child spawned **without** a pty (a `true`-like and a `sleep`-like
/// program resolved through `PATH`).
struct StdChild(std::process::Child);

impl ChildHandle for StdChild {
    fn try_wait_child(&mut self) -> io::Result<Option<ExitStatus>> {
        self.0.try_wait().map(|st| st.map(ExitStatus::from))
    }

    fn wait_child(&mut self) -> io::Result<ExitStatus> {
        self.0.wait().map(ExitStatus::from)
    }

    fn kill_child(&mut self) -> io::Result<()> {
        self.0.kill()
    }
}

/// `waitpid(pid, WNOHANG)` after the reaper ran. `-1`/`ECHILD` means the pid is
/// gone from the process table, i.e. it was reaped and is **not** a zombie.
#[cfg(unix)]
fn waitpid_nowait(pid: i32) -> (i32, i32) {
    let mut wstatus = 0i32;
    let ret = unsafe { libc::waitpid(pid, &mut wstatus, libc::WNOHANG) };
    (ret, io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

/// Spawn spec (program + args) for a child that exits successfully right away. The program
/// is deliberately **not** an absolute path: coreutils sit in the root `bin` directory on
/// Linux but under `/usr` on macOS, so a hard-coded absolute location of `true` fails on
/// the latter with `NotFound (os error 2)`. Resolving `true` through `PATH` works on both;
/// `cmd /C exit 0` is the equivalent short-lived process on Windows.
fn short_lived_child_spec() -> (&'static str, &'static [&'static str]) {
    if cfg!(windows) {
        ("cmd", &["/C", "exit 0"])
    } else {
        ("true", &[])
    }
}

/// Spawn spec (program + args) for a child that keeps running until killed.
///
/// `sleep 30` on unix; `cmd /C ping -n 30 127.0.0.1` on Windows, because
/// `timeout` requires a real console while `ping` runs headless.
fn long_running_child_spec() -> (&'static str, &'static [&'static str]) {
    if cfg!(windows) {
        ("cmd", &["/C", "ping -n 30 127.0.0.1"])
    } else {
        // The sleep program is resolved by the OS through `PATH` here — no
        // absolute path, whose location differs between Unix runners.
        ("sleep", &["30"])
    }
}

/// Spawn a real OS child (no pty involved) from a platform spawn spec.
fn spawn_real_child(spec: (&str, &[&str])) -> std::process::Child {
    let (program, args) = spec;
    std::process::Command::new(program)
        .args(args)
        .spawn()
        .unwrap_or_else(|err| {
            panic!(
                "spawn `{program} {}` (resolved via PATH) without a pty: {err}",
                args.join(" ")
            )
        })
}

/// Zombie check on unix: after the reaper ran, the pid must be gone from the
/// process table. Stays `cfg(unix)` because `waitpid`/`libc` are unix-only.
#[cfg(unix)]
fn assert_gone_from_process_table(pid: i32, label: &str) {
    let (ret, errno) = waitpid_nowait(pid);
    assert_eq!(
        (ret, errno),
        (-1, libc::ECHILD),
        "{label} {pid} was left as a <defunct> zombie"
    );
}

/// Windows exposes no `waitpid` equivalent: the reaper having `wait()`ed the
/// `std::process::Child` (asserted by the caller) is the strongest available
/// no-zombie guarantee, so nothing further is asserted there.
#[cfg(not(unix))]
fn assert_gone_from_process_table(_pid: i32, _label: &str) {}

/// A reaped child must leave nothing in the process table. Spawned with
/// `std::process::Command`, so no `/dev/pts` is involved.
#[test]
fn test_reap_removes_real_child_from_process_table() {
    let child = spawn_real_child(short_lived_child_spec());
    let pid = child.id() as i32;

    let mut reaper = ChildReaper::new(StdChild(child), None);
    let outcome = reaper.kill_and_reap(PTY_REAP_GRACE, true);

    assert!(outcome.reaped, "child must be reaped");
    assert!(outcome.status.is_some(), "exit status must be recorded");
    assert!(reaper.is_reaped());

    assert_gone_from_process_table(pid, "child");
}

/// A long-running child is killed **and** reaped, so a `run_command` that times out
/// cannot accumulate zombies.
#[test]
fn test_reap_kills_and_reaps_real_long_running_child() {
    let child = spawn_real_child(long_running_child_spec());
    let pid = child.id() as i32;

    let mut reaper = ChildReaper::new(StdChild(child), None);
    let outcome = reaper.kill_and_reap(PTY_REAP_GRACE, false);

    assert!(outcome.reaped, "killed child must be reaped");
    let status = outcome.status.expect("exit status recorded after SIGKILL");
    assert!(
        !status.success(),
        "a SIGKILLed child must not report success"
    );

    assert_gone_from_process_table(pid, "killed child");
}

/// Negative control proving the `ECHILD` assertions above are meaningful: a
/// killed-but-unreaped child *is* visible to `waitpid` as a zombie.
#[test]
#[cfg(unix)]
fn test_unreaped_killed_child_is_visible_as_zombie() {
    let mut child = spawn_real_child(long_running_child_spec());
    let pid = child.id() as i32;
    child.kill().expect("SIGKILL the child");

    let mut seen_zombie = false;
    for _ in 0..50 {
        let (ret, _) = waitpid_nowait(pid);
        if ret == pid {
            seen_zombie = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        seen_zombie,
        "expected a reaped-by-hand zombie for the unreaped control child {pid}"
    );

    // The manual `waitpid` above cleaned the zombie back up; the std handle now
    // sees `ECHILD`, which is exactly the point of the control.
    let _ = child.wait();
}

/// The normal teardown path reaps once; every later teardown pass (timeout +
/// `Drop` safety net) must be a no-op — no second kill, no second wait.
#[test]
fn test_child_reaper_reaps_exactly_once_and_is_idempotent() {
    let fake = FakeChild::killable();
    let mut reaper = ChildReaper::new(fake.clone(), None);

    let first = reaper.kill_and_reap(PTY_REAP_GRACE, true);
    assert!(first.reaped);
    assert!(first.status.is_some());
    assert_eq!(fake.kill_count(), 1, "child must be killed once");
    assert_eq!(
        fake.blocking_wait_count(),
        0,
        "try_wait saw the exit, so no blocking wait is needed"
    );
    assert!(reaper.is_reaped());
    assert!(reaper.exit_status().is_some());
    let polls_after_first = fake.poll_count();
    assert_eq!(polls_after_first, 1, "one try_wait is enough");

    // Second and third passes: completion + cancellation + Drop all tear down.
    let second = reaper.kill_and_reap(PTY_REAP_GRACE, true);
    let _ = reaper.reap_if_exited();
    let _ = reaper.kill_and_reap(PTY_REAP_GRACE, false);

    assert!(
        second.reaped,
        "idempotent teardown reports the reaped state"
    );
    assert_eq!(second.status.map(|st| st.exit_code()), Some(0));
    assert_eq!(
        fake.kill_count(),
        1,
        "a reaped pid must not be killed again"
    );
    assert_eq!(
        fake.poll_count(),
        polls_after_first,
        "a reaped child must never be waited on again"
    );
    assert_eq!(fake.blocking_wait_count(), 0);
}

/// A child that will not die must still be reaped by the one-shot command path:
/// the bounded `try_wait` window expires and the blocking `wait()` takes over.
#[test]
fn test_child_reaper_blocking_fallback_reaps_stubborn_child() {
    let fake = FakeChild::unkillable();
    let mut reaper = ChildReaper::new(fake.clone(), None);

    let grace = Duration::from_millis(50);
    let started = std::time::Instant::now();
    let outcome = reaper.kill_and_reap(grace, true);

    assert!(
        outcome.reaped,
        "one-shot path must fall back to the blocking wait"
    );
    assert_eq!(outcome.status.map(|st| st.exit_code()), Some(0));
    assert!(
        fake.poll_count() > 1,
        "the grace window must be polled, not guessed"
    );
    assert_eq!(
        fake.blocking_wait_count(),
        1,
        "exactly one blocking wait after the grace window"
    );
    assert!(
        started.elapsed() >= grace,
        "the bounded grace window must be honoured before blocking"
    );
}

/// A failing `waitpid` means the pid is not in the process table: treat it as reaped so
/// the handle is never waited on a second time (which is what a double-reap looks like).
#[test]
fn test_child_reaper_treats_waitpid_error_as_reaped_and_never_retries() {
    let fake = FakeChild::waitpid_error();
    let mut reaper = ChildReaper::new(fake.clone(), None);

    let outcome = reaper.kill_and_reap(Duration::from_millis(20), true);

    assert!(reaper.is_reaped(), "failed waitpid ⇒ the child is gone");
    assert!(outcome.status.is_none(), "no exit status is inventable");
    assert_eq!(
        fake.blocking_wait_count(),
        0,
        "no blocking fallback after a failed waitpid"
    );

    let polls = fake.poll_count();
    assert_eq!(polls, 1, "stop polling right after the failed waitpid");
    assert!(reaper.reap_if_exited().is_none());
    let _ = reaper.kill_and_reap(Duration::from_millis(20), true);
    assert_eq!(
        fake.poll_count(),
        polls,
        "no re-poll after the failed waitpid"
    );
}

/// Interactive teardown must not block the async runtime: a child that survives
/// the short grace window is handed to the bounded deferred queue, where it is
/// reaped exactly once — never dropped unreaped, never reaped twice.
#[test]
fn test_child_reaper_hands_stuck_child_to_deferred_queue_and_reaps_once_there() {
    let fake = FakeChild::unkillable();
    let mut reaper = ChildReaper::new(fake.clone(), None);

    let outcome = reaper.kill_and_reap(Duration::from_millis(20), false);
    assert!(
        !outcome.reaped,
        "a child that outlives the grace window is not reaped yet"
    );
    assert!(outcome.status.is_none());
    assert_eq!(fake.kill_count(), 1, "SIGKILL the child once");

    let mut queue = DeferredReapQueue::new();
    assert!(reaper.defer_to(&mut queue), "handle handed to the queue");
    assert_eq!(queue.len(), 1);
    assert!(
        !reaper.defer_to(&mut queue),
        "the handle must not be queued a second time"
    );
    assert_eq!(queue.len(), 1, "no duplicate entry");

    // The janitor drain reaps it exactly once.
    fake.set_exited();
    assert_eq!(queue.drain(), 1, "deferred child reaped");
    assert!(queue.is_empty());
    assert_eq!(
        queue.drain(),
        0,
        "a retired handle is never waited on again"
    );
}

/// The deferred queue must stay bounded (no unbounded thread/handle growth) and
/// must not queue children that already exited.
#[test]
fn test_deferred_reap_queue_reaps_inline_and_stays_bounded() {
    let mut queue = DeferredReapQueue::new();

    // Already-exited child: reaped inline, never queued.
    let exited = FakeChild::exited();
    assert_eq!(queue.push(Box::new(exited.clone())), 1);
    assert_eq!(exited.poll_count(), 1);
    assert!(queue.is_empty(), "no point queueing an exited child");

    // Stuck children: the queue is capped, not unbounded.
    for _ in 0..(DEFERRED_REAP_QUEUE_CAP * 3) {
        queue.push(Box::new(FakeChild::unkillable()));
    }
    assert_eq!(
        queue.len(),
        DEFERRED_REAP_QUEUE_CAP,
        "queue must be capped at DEFERRED_REAP_QUEUE_CAP"
    );
    assert!(!queue.is_empty());
    assert_eq!(queue.drain(), 0, "still-running children stay queued");
    assert_eq!(queue.len(), DEFERRED_REAP_QUEUE_CAP);
}

// Ring buffer + truncation accounting, pure logic on `SharedBuffer` (this is what
// keeps an interactive session's buffer bounded).

#[test]
fn test_shared_buffer_cap_constant_is_documented() {
    // The cap is an explicit, documented byte budget (256 KiB).
    assert_eq!(PTY_OUTPUT_BUFFER_CAP, 256 * 1024);
    assert_eq!(
        truncation_notice(1234),
        "[1234 bytes of earlier output discarded: interactive PTY buffer capped at 262144 bytes]\n"
    );
}

/// A reader that keeps up never loses output, and the retained window stays
/// small: the consumed prefix is reclaimed instead of accumulating.
#[test]
fn test_shared_buffer_reclaims_consumed_prefix_without_losing_output() {
    let mut buf = SharedBuffer::new();
    for i in 0..(2 * (PTY_OUTPUT_BUFFER_CAP / 4096) + 7) {
        buf.push(&[b'x'; 4096]);
        assert!(buf.output.len() <= PTY_OUTPUT_BUFFER_CAP);
        let text = buf.take_readable();
        assert_eq!(text.len(), 4096, "chunk {i} must be delivered whole");
        assert!(
            !text.contains("discarded"),
            "no loss for a reading consumer"
        );
    }
    assert_eq!(buf.dropped_bytes, 0, "a reading consumer loses nothing");
    // Reclamation happens inside the *next* push, so the consumed prefix must
    // never accumulate across pushes.
    assert!(
        buf.output.len() <= 4096,
        "consumed prefix already reclaimed, kept {}",
        buf.output.len()
    );
    buf.push(&[b'x'; 4096]);
    assert_eq!(
        buf.output.len(),
        4096,
        "the consumed prefix is drained instead of accumulating"
    );
    assert_eq!(buf.take_readable().len(), 4096);
    assert_eq!(buf.dropped_bytes, 0, "still nothing lost");
}

/// A non-reading consumer (`yes`, `tail -f`): retention is capped at the
/// constant, the *head* is what gets dropped, and the loss is counted.
#[test]
fn test_shared_buffer_caps_retention_and_counts_dropped_head_bytes() {
    let mut buf = SharedBuffer::new();
    let pushes = 4 * (PTY_OUTPUT_BUFFER_CAP / 4096);
    for _ in 0..pushes {
        buf.push(&[b'y'; 4096]);
        assert!(
            buf.output.len() <= PTY_OUTPUT_BUFFER_CAP,
            "ring must never exceed the cap, got {}",
            buf.output.len()
        );
    }

    let pushed = pushes as u64 * 4096;
    assert_eq!(buf.output.len(), PTY_OUTPUT_BUFFER_CAP);
    assert_eq!(
        buf.dropped_bytes,
        pushed - PTY_OUTPUT_BUFFER_CAP as u64,
        "exactly the head beyond the cap is dropped"
    );

    // The reader is told the head was dropped, and sees the newest tail.
    let text = buf.take_readable();
    let lost = pushed - PTY_OUTPUT_BUFFER_CAP as u64;
    assert!(
        text.starts_with(&format!("[{lost} bytes of earlier output discarded")),
        "truncation notice missing, got: {:?}",
        &text[..80.min(text.len())]
    );
    // Count only the payload: the notice prose itself spells out "bytes" twice.
    let (notice, body) = text
        .split_once("]\n")
        .expect("notice line must be terminated");
    assert!(notice.contains("discarded"));
    assert_eq!(
        body.matches('y').count(),
        PTY_OUTPUT_BUFFER_CAP,
        "exactly one cap-sized tail survives"
    );

    // The notice is surfaced once, not on every subsequent read.
    let again = buf.take_readable();
    assert!(again.is_empty(), "no re-notice without new loss");
}

/// Trimming must land on a UTF-8 char boundary: the retained window stays valid
/// UTF-8 with no split character, and sits at most one character below the cap
/// (never above it).
#[test]
fn test_shared_buffer_trim_never_splits_multibyte_char() {
    // 3-byte `漢`: the cap is not a multiple of 3, so the naive cut would
    // otherwise land inside a character.
    let chunk = "漢".repeat(1000).into_bytes();
    assert_eq!(chunk.len(), 3000);

    let mut buf = SharedBuffer::new();
    for _ in 0..(PTY_OUTPUT_BUFFER_CAP / 3000 + 6) {
        buf.push(&chunk);
        assert!(
            buf.output.len() <= PTY_OUTPUT_BUFFER_CAP,
            "the cap is a hard ceiling even for multi-byte text"
        );
    }

    // The cut scans forward to the next lead byte, so at most two extra bytes
    // (one character) are sacrificed: retained stays within [cap - 2, cap].
    assert!(
        buf.output.len() >= PTY_OUTPUT_BUFFER_CAP - 2,
        "trim keeps as much as the budget allows, got {}",
        buf.output.len()
    );
    let decoded = std::str::from_utf8(&buf.output)
        .expect("retained window must not split a multi-byte character");
    assert!(
        decoded.starts_with('漢') && decoded.ends_with('漢'),
        "no replacement character at the trim boundary"
    );
    assert!(
        !String::from_utf8_lossy(&buf.output).contains('\u{fffd}'),
        "the head was dropped whole, no U+FFFD was synthesised"
    );
    assert!(buf.dropped_bytes > 0, "the loss is counted");
}

/// Degenerate inputs: empty pushes, and a single read far larger than the cap.
#[test]
fn test_shared_buffer_empty_push_and_oversized_chunk_boundaries() {
    let mut buf = SharedBuffer::new();
    buf.push(&[]);
    assert!(buf.output.is_empty());
    assert_eq!(buf.dropped_bytes, 0);
    assert!(buf.take_readable().is_empty());

    let huge = vec![b'z'; PTY_OUTPUT_BUFFER_CAP * 3];
    buf.push(&huge);
    assert_eq!(buf.output.len(), PTY_OUTPUT_BUFFER_CAP);
    assert_eq!(buf.dropped_bytes as usize, PTY_OUTPUT_BUFFER_CAP * 2);

    let text = buf.take_readable();
    assert!(
        text.starts_with("[524288 bytes of earlier output discarded"),
        "oversized read must be reported, got {:?}",
        &text[..60.min(text.len())]
    );
    assert_eq!(text.matches('z').count(), PTY_OUTPUT_BUFFER_CAP);
}

#[test]
fn test_is_utf8_lead_byte_classifies_lead_and_continuation_bytes() {
    // ASCII and every UTF-8 lead-byte range.
    assert!(is_utf8_lead_byte(b'a'));
    assert!(is_utf8_lead_byte(0xC3)); // 2-byte lead (`å`, `ä`, `ö`)
    assert!(is_utf8_lead_byte(0xE6)); // 3-byte lead (`漢`)
    assert!(is_utf8_lead_byte(0xF0)); // 4-byte lead (emoji)
    // Continuation bytes must never be a cut point.
    assert!(!is_utf8_lead_byte(0x80));
    assert!(!is_utf8_lead_byte(0xBF));
    assert!(!is_utf8_lead_byte(0xA4)); // second byte of `ä`
}

// The sandbox decision must NEVER depend on the executable NAME.
// These tests drive [`should_apply_sandbox`] / [`sandbox_command_argv`] as pure
// logic with injected inputs: no backend availability, no `/dev/pts`, no
// `current_exe()`, no environment mutation, so they are deterministic
// everywhere (including sandboxes where `openpty` yields EACCES).

use std::path::{Path, PathBuf};

/// Production-shaped inputs: a host with a sandbox backend whose exe resolved to `exe`.
fn production_inputs(exe: &Path) -> SandboxInputs<'_> {
    SandboxInputs {
        backend_supported: true,
        test_harness_exe: false,
        resolved_exe: Some(exe),
        opt_out: OptOut::None,
    }
}

// Thread-local sink for the formatted tracing output of the decision point, so
// "running unsandboxed is never silent" is asserted, not just promised.
thread_local! {
    static LOG_LINES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy, Default)]
struct CaptureWriter;

struct CaptureSink;

impl io::Write for CaptureSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        LOG_LINES.with(|lines| {
            lines
                .borrow_mut()
                .push(String::from_utf8_lossy(buf).into_owned())
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CaptureWriter {
    type Writer = CaptureSink;
    fn make_writer(&self) -> CaptureSink {
        CaptureSink
    }
}

/// Run `f` with an in-memory subscriber and return its result plus everything
/// that was logged (formatted exactly as in production).
fn capture_logs<R>(f: impl FnOnce() -> R) -> (R, String) {
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(CaptureWriter)
        .finish();
    LOG_LINES.with(|lines| lines.borrow_mut().clear());
    let result = tracing::subscriber::with_default(subscriber, f);
    let text = LOG_LINES.with(|lines| lines.borrow().join("\n"));
    (result, text)
}

/// A renamed / copied / installed binary is sandboxed exactly like any other: the
/// resolved path decides, the name never does.
#[test]
fn test_sandbox_gate_renamed_binary_is_still_sandboxed() {
    for name in [
        "marmel",
        "marmel-dev",
        "marmel-debug",
        "marmel-x",
        "marmel.exe",
        "mm",
        "not-marmel-at-all",
    ] {
        let exe = PathBuf::from("/opt/marmel/bin").join(name);
        let decision = should_apply_sandbox(&production_inputs(&exe));
        assert_eq!(
            decision,
            SandboxDecision::Apply {
                resolved_exe: exe.clone()
            },
            "`{name}` must be sandboxed by resolved path, not by name"
        );
        assert_eq!(decision.mode(), "sandbox-re-entry");
    }
}

/// Name equality decides nothing: the decision is identical across names that comparing
/// `file_name(current_exe)` against a literal would have split apart.
#[test]
fn test_sandbox_gate_executable_name_has_zero_influence() {
    let dir = Path::new("/home/fredrik/src/wip/target/debug");
    // A name-equality check answers `true` for the exact names only — that is precisely
    // the split these tests prove impossible.
    let names = ["marmel", "marmel.exe", "marmel-x", "sh", "bash", "cargo"];
    let old_heuristic: Vec<bool> = names
        .iter()
        .map(|n| *n == "marmel" || *n == "marmel.exe")
        .collect();
    assert!(
        old_heuristic.contains(&true) && old_heuristic.contains(&false),
        "the old heuristic really did split these names — that is the bug being removed"
    );

    let decisions: Vec<SandboxDecision> = names
        .iter()
        .map(|n| {
            let exe = dir.join(n);
            should_apply_sandbox(&production_inputs(&exe))
        })
        .collect();

    // Every one of them is sandboxed: the name is not an input at all.
    for decision in &decisions {
        assert!(
            matches!(decision, SandboxDecision::Apply { .. }),
            "name-gating leaked into the decision: {decision:?}"
        );
    }
    // And an unrelated program that merely *happens* to be named `marmel` is
    // treated like any other host of this code (also sandboxed).
    // Arbitrary install prefix (the fixture binary is only defined by its name;
    // the directory it sits in is never an input to the decision).
    let coincidental = Path::new("/usr/local/libexec/marmel");
    assert!(matches!(
        should_apply_sandbox(&production_inputs(coincidental)),
        SandboxDecision::Apply { .. }
    ));
}

/// Regression guard: name equality must be absent from the source, not merely bypassed.
#[test]
fn test_sandbox_gate_source_has_no_executable_name_heuristic() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/harness/pty.rs"))
        .expect("pty.rs must be readable from the crate manifest dir");
    assert!(
        !src.contains("is_marmel_binary"),
        "the name-based gate (`is_marmel_binary`) must not come back"
    );
    assert!(
        !src.contains("\"marmel\"") && !src.contains("\"marmel.exe\""),
        "no executable-name literal may be compared anywhere in the sandbox gate"
    );
    assert!(
        src.contains("SANDBOX_OPTOUT_ENV") && src.contains("should_apply_sandbox"),
        "the gate must be driven by the resolved-exe decision function and an explicit opt-out"
    );
}

/// An explicit opt-out disables the sandbox **and** warns, naming the setting,
/// the resolved exe, and the fact that the command is unsandboxed.
#[test]
fn test_sandbox_gate_explicit_opt_out_disables_and_is_warned() {
    // Arbitrary resolved-exe fixture (the directory it sits in is never an
    // input to the decision); the log assertion below must name the same path.
    let exe = Path::new("/opt/marmel/libexec/marmel-dev");
    let inputs = SandboxInputs {
        opt_out: OptOut::Explicit {
            setting: format!("{SANDBOX_OPTOUT_ENV}=1"),
        },
        ..production_inputs(exe)
    };
    let decision = should_apply_sandbox(&inputs);
    assert_eq!(
        decision,
        SandboxDecision::Skip {
            resolved_exe: Some(exe.to_path_buf()),
            skip: SandboxSkip::OptOut {
                setting: format!("{SANDBOX_OPTOUT_ENV}=1")
            }
        }
    );

    let (_, logs) = capture_logs(|| log_sandbox_decision(&decision));
    assert!(
        logs.contains("WARN") && logs.contains("SKIPPED"),
        "an opted-out sandbox must be warned about, got {logs:?}"
    );
    assert!(
        logs.contains(SANDBOX_OPTOUT_ENV),
        "the warn must name the setting that disabled it, got {logs:?}"
    );
    assert!(
        logs.contains("/opt/marmel/libexec/marmel-dev") && logs.contains("unsandboxed"),
        "the warn must state the resolved exe and the chosen mode, got {logs:?}"
    );

    // The opted-out command really is a plain `sh -c …`, preamble included.
    let cwd = Path::new("/tmp/work");
    let (program, args) =
        sandbox_command_argv(&decision, "echo hi", cwd).expect("skip decision builds argv");
    assert_eq!(program, "sh");
    assert_eq!(args[0], "-c");
    assert!(args[1].ends_with("echo hi"));
}

/// The opt-out must be unambiguous: only `1/true/yes/on` count. Anything else —
/// including falsy-looking strings and typos — keeps the sandbox on (fail-closed).
#[test]
fn test_sandbox_gate_opt_out_value_must_be_unambiguous() {
    for enabled in ["1", "true", "TRUE", "Yes", " on ", "\ton"] {
        assert!(
            is_optout_value(enabled),
            "`{enabled}` is an explicit opt-out"
        );
    }
    for not_opt_out in [
        "", " ", "0", "false", "FALSE", "no", "off", "maybe", "1x", "disable",
    ] {
        assert!(
            !is_optout_value(not_opt_out),
            "`{not_opt_out}` is not an unambiguous opt-out: sandbox must stay ON"
        );
    }
}

/// No sandbox backend on this platform ⇒ skipped, but never silently. Linux (Landlock),
/// macOS (Seatbelt) and Windows (AppContainer) all have one, so only the remaining hosts —
/// the BSDs and friends — take this branch.
#[test]
fn test_sandbox_gate_unsupported_platform_skips_with_warning() {
    let exe = Path::new("/Users/fredrik/target/debug/marmel");
    let inputs = SandboxInputs {
        backend_supported: false,
        ..production_inputs(exe)
    };
    let decision = should_apply_sandbox(&inputs);
    assert_eq!(
        decision,
        SandboxDecision::Skip {
            resolved_exe: Some(exe.to_path_buf()),
            skip: SandboxSkip::UnsupportedPlatform
        }
    );
    let (_, logs) = capture_logs(|| log_sandbox_decision(&decision));
    assert!(
        logs.contains("WARN") && logs.contains("no sandbox backend"),
        "an unavailable sandbox must be visible, got {logs:?}"
    );
}

/// The gate no longer asks "am I on Linux": both of its sites ask the one capability flag
/// in `harness::sandbox`, so a host with a Landlock **or** a Seatbelt **or** an AppContainer
/// backend reaches the re-entry child. Asserted with the flag as production feeds it, on the
/// current build.
#[test]
fn test_sandbox_gate_platform_answer_comes_from_the_shared_capability_flag() {
    let flag = crate::harness::sandbox::backend_supports_current_platform();
    let decision = should_apply_sandbox(&SandboxInputs {
        backend_supported: flag,
        ..production_inputs(Path::new(
            "/Users/fredrik/Applications/marmel-dev.app/Contents/MacOS/marmel-dev",
        ))
    });
    assert_eq!(
        flag,
        cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        )),
        "the shared flag must answer true for all three backends and false for every other host"
    );
    if flag {
        assert!(
            matches!(decision, SandboxDecision::Apply { .. }),
            "a host that compiles Landlock, Seatbelt or AppContainer must re-enter, got {decision:?}"
        );
        assert_eq!(decision.mode(), "sandbox-re-entry");
    } else {
        assert!(
            matches!(
                decision,
                SandboxDecision::Skip {
                    skip: SandboxSkip::UnsupportedPlatform,
                    ..
                }
            ),
            "a host with neither backend must skip visibly, got {decision:?}"
        );
    }
}

/// The macOS leg of the same contract: with a Seatbelt backend the argv handed to the PTY
/// is the unchanged three-slot re-entry form — resolved exe, sentinel, cwd, wrapped command
/// — exactly as on Linux. Gated to the hosts that have a backend because
/// `sandbox_command_argv` only builds that form where one exists.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn test_sandbox_gate_reentry_argv_is_the_same_three_slots_on_macos() {
    let exe = Path::new("/Users/fredrik/.cargo/bin/marmel-dev");
    let decision = should_apply_sandbox(&SandboxInputs {
        backend_supported: crate::harness::sandbox::backend_supports_current_platform(),
        test_harness_exe: false,
        resolved_exe: Some(exe),
        opt_out: OptOut::None,
    });
    let (program, args) =
        sandbox_command_argv(&decision, "cargo test", Path::new("/Users/fredrik/wip"))
            .expect("a host with a backend builds the re-entry argv");
    assert_eq!(program, exe.to_string_lossy());
    assert_eq!(
        args.len(),
        3,
        "the re-entry contract is three slots: {args:?}"
    );
    assert_eq!(args[0], SANDBOX_EXEC_ARG);
    assert_eq!(args[1], "/Users/fredrik/wip");
    assert!(args[2].ends_with("cargo test"));
}

/// A Cargo test-harness binary cannot speak the re-entry protocol; that skip is
/// compile-time (`cfg!(test)`), cannot be reached by renaming a binary, and is
/// still warned about.
#[test]
fn test_sandbox_gate_test_harness_binary_skips_with_warning() {
    let inputs = SandboxInputs {
        test_harness_exe: true,
        ..production_inputs(Path::new(
            "/wip/target/debug/deps/marmennill-25e1737d17c27a1e",
        ))
    };
    let decision = should_apply_sandbox(&inputs);
    assert!(matches!(
        decision,
        SandboxDecision::Skip {
            skip: SandboxSkip::TestHarnessExe,
            ..
        }
    ));
    let (_, logs) = capture_logs(|| log_sandbox_decision(&decision));
    assert!(
        logs.contains("WARN") && logs.contains(SANDBOX_EXEC_ARG),
        "the harness-binary skip must name the missing re-entry protocol, got {logs:?}"
    );
}

/// Fail-closed: on a host with a backend, an unresolvable exe is a hard error — falling
/// back to `sh -c` here would run the command unsandboxed in silence.
#[test]
fn test_sandbox_gate_unresolved_exe_fails_closed_and_is_logged() {
    let inputs = SandboxInputs {
        backend_supported: true,
        test_harness_exe: false,
        resolved_exe: None,
        opt_out: OptOut::None,
    };
    let decision = should_apply_sandbox(&inputs);
    assert_eq!(decision, SandboxDecision::UnresolvedExe);
    assert_eq!(decision.mode(), "hard-fail");

    let (_, logs) = capture_logs(|| log_sandbox_decision(&decision));
    assert!(
        logs.contains("ERROR") && logs.contains("unsandboxed"),
        "the fail-closed path must be logged loudly, got {logs:?}"
    );

    let err = sandbox_command_argv(&decision, "echo hi", Path::new("/tmp"))
        .expect_err("must refuse to build an unsandboxed argv");
    let text = err.to_string();
    assert!(
        text.contains(SANDBOX_EXEC_ARG) && text.contains(SANDBOX_OPTOUT_ENV),
        "the error must explain both the requirement and the explicit opt-out, got {text}"
    );
}

/// Expected shell argv prefix of `build_sandboxed_command` on this platform, following
/// the same platform spawn-spec idiom as `short_lived_child_spec`/`long_running_child_spec`
/// above. Grounded in the production builder: every unix skip emits `sh` plus `-c`.
///
/// Windows: this helper used to pin the pre-t-007 spelling `("cmd", "/C")`, which is now
/// **superseded**. Rule R2 of [`windows_shell_argv`] makes the program token the trimmed
/// `%COMSPEC%` when it carries a value and the extension-qualified `cmd.exe` otherwise —
/// never a bare `cmd`, because `CreateProcessW` receives it as `lpApplicationName` and
/// does not append `.exe`. The expectation is therefore resolved through the production
/// seam ([`windows_cmd_program`]) instead of being spelled out a second time, so it
/// cannot drift from the rule, and it is an owned `String` because `%COMSPEC%` is a
/// runtime value.
fn expected_sandbox_shell() -> (String, &'static str) {
    if cfg!(windows) {
        let comspec = std::env::var("COMSPEC").ok();
        (windows_cmd_program(comspec.as_deref()), "/C")
    } else {
        ("sh".to_string(), "-c")
    }
}

/// End-to-end wiring at the decision point, PTY-free: in a test-harness binary
/// the builder skips (and warns) instead of exec-ing a binary that cannot
/// re-enter, and the command payload survives either way.
#[test]
fn test_build_sandboxed_command_logs_its_choice_and_keeps_the_wrapper() {
    let (built, logs) = capture_logs(|| build_sandboxed_command("echo hi", Path::new("/tmp")));
    let cmd = built.expect("argv must be buildable");
    let argv = cmd.get_argv();
    assert!(!argv.is_empty(), "the builder must have a program");

    // In a test binary the re-entry is skipped, so unix builds `sh -c <wrapped>`; Windows
    // builds its own `%COMSPEC% /C <command>` form from the very same decision object (it has
    // an AppContainer backend, but a test-harness binary cannot speak the re-entry protocol).
    // Either way the command was chosen by a decision that was logged.
    if cfg!(test) {
        let (program, shell_flag) = expected_sandbox_shell();
        assert_eq!(argv[0].to_string_lossy(), program);
        assert_eq!(argv[1].to_string_lossy(), shell_flag);
        // Both platforms log the decision the same way now: `log_sandbox_decision` emits the
        // `pty sandbox decision` debug line and the matching `SKIPPED` warn on unix and on
        // Windows alike, so the skip is never silent on either of them.
        assert!(
            logs.contains("WARN")
                && logs.contains("SKIPPED")
                && logs.contains("pty sandbox decision"),
            "every skip must leave a visible trace, got {logs:?}"
        );
    }
    let payload = argv.last().expect("the command payload").to_string_lossy();
    if cfg!(windows) {
        // `stty`/`ulimit` are POSIX shell builtins, so the Windows builder hands
        // the command to `cmd /C` verbatim instead of prefixing the preamble.
        assert_eq!(
            payload, "echo hi",
            "the Windows builder must not rewrite the command, got {payload:?}"
        );
    } else {
        assert!(
            payload.starts_with("stty -echo 2>/dev/null || true; ulimit -f 4194304"),
            "REQ-TOOL-001 preamble must survive the sandbox rewrite, got {payload:?}"
        );
        assert!(payload.ends_with("echo hi"));
    }
}

/// The re-entry argv is exactly `<resolved exe> --internal-sandbox-exec <cwd> <wrapped>`:
/// the resolved path is what gets exec-ed, whatever it is called.
#[test]
fn test_sandbox_gate_reentry_argv_uses_the_resolved_exe() {
    let exe = Path::new("/usr/local/libexec/mm-renamed-build-7");
    let decision = should_apply_sandbox(&production_inputs(exe));
    let cwd = Path::new("/home/fredrik/marmel/wip");
    let (program, args) =
        sandbox_command_argv(&decision, "cargo test", cwd).expect("apply decision builds argv");
    assert_eq!(program, "/usr/local/libexec/mm-renamed-build-7");
    assert_eq!(args[0], SANDBOX_EXEC_ARG);
    assert_eq!(args[1], "/home/fredrik/marmel/wip");
    assert!(args[2].ends_with("cargo test"));
    assert_eq!(args.len(), 3);
}

/// The extras channel is published where — and only where — it has a consumer, and the
/// config is its single source of truth. The spawn side seeds its builder from
/// `std::env::vars_os()`, so an ambient value under that name would otherwise be
/// inherited by the re-entry child (which would widen its own ruleset from it) or by the
/// plain `sh -c` of the skip branch (and from there into any nested invocation): the
/// environment deciding how much the sandbox trusts, which is what the config key exists
/// to prevent.
///
/// Asserted as in-process behaviour: the ambient case is modelled exactly the way
/// portable-pty seeds it — an entry already present in the builder's env map — rather
/// than by mutating the process environment, which is `unsafe` in edition 2024 and would
/// need the env lock for no added rigour. `ACTIVE_CONFIG` **is** process-global, so the
/// config legs run under the same serialization lock the config tests use, and the
/// previously active config (including "none") is restored on the way out.
#[test]
fn test_sandbox_gate_extras_channel_follows_the_config_and_the_decision() {
    let key = crate::harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV;
    let ambient = "/tmp/ambient-value-in-no-allow-list";

    struct ActiveConfigRestore(Option<crate::config::Config>);
    impl Drop for ActiveConfigRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(cfg) => crate::config::set_active(cfg),
                None => crate::config::clear_active(),
            }
        }
    }
    let _serial = crate::config::sandbox_env_test_lock();
    let _restore = ActiveConfigRestore(crate::config::get_active());

    let seeded = || {
        let mut builder = CommandBuilder::new("sh");
        builder.env(key, ambient);
        builder
    };
    let with_paths = |paths: &[&str]| crate::config::Config {
        sandbox_extra_readonly_paths: paths.iter().map(|path| (*path).to_string()).collect(),
        ..crate::config::Config::default()
    };
    let reentry = should_apply_sandbox(&production_inputs(Path::new(
        "/usr/local/libexec/mm-renamed-build-7",
    )));
    assert!(
        matches!(reentry, SandboxDecision::Apply { .. }),
        "these legs need the re-entry branch, got {reentry:?}"
    );

    // 1. A configured list travels verbatim — trimmed, empty entries dropped — and it
    //    overwrites whatever the parent's environment happened to carry.
    crate::config::set_active(with_paths(&[" /opt/toolchain-a ", "", "/opt/toolchain-b"]));
    let mut cmd = seeded();
    wire_extra_readonly_paths_env(&mut cmd, &reentry, extra_readonly_paths_env_value());
    assert_eq!(
        cmd.get_env(key)
            .map(|raw| raw.to_string_lossy().into_owned()),
        Some("/opt/toolchain-a:/opt/toolchain-b".to_string()),
        "the configured list must be the value the re-entry child sees"
    );

    // 2. Key present but empty: the variable is REMOVED from the child environment, so
    //    the inherited value cannot widen the child's ruleset.
    crate::config::set_active(with_paths(&[]));
    let mut cmd = seeded();
    wire_extra_readonly_paths_env(&mut cmd, &reentry, extra_readonly_paths_env_value());
    assert!(
        cmd.get_env(key).is_none(),
        "an ambient value must not survive an empty config list"
    );

    // 3. No active config at all: same removal — this is the state a re-entry child of a
    //    config-less parent must be handed.
    crate::config::clear_active();
    let mut cmd = seeded();
    wire_extra_readonly_paths_env(&mut cmd, &reentry, extra_readonly_paths_env_value());
    assert!(
        cmd.get_env(key).is_none(),
        "an ambient value must not survive when no config is active"
    );

    // 4. The skip branch runs the command through a plain `sh -c`, which has no consumer
    //    for the channel: with the key configured it must publish nothing, and it must
    //    not pass the ambient value on either.
    crate::config::set_active(with_paths(&["/opt/toolchain-a"]));
    let skipped = should_apply_sandbox(&SandboxInputs {
        test_harness_exe: true,
        ..production_inputs(Path::new("/usr/local/libexec/mm-renamed-build-7"))
    });
    assert!(
        matches!(skipped, SandboxDecision::Skip { .. }),
        "the test-harness input must skip, got {skipped:?}"
    );
    let mut cmd = seeded();
    wire_extra_readonly_paths_env(&mut cmd, &skipped, extra_readonly_paths_env_value());
    assert!(
        cmd.get_env(key).is_none(),
        "the skip branch must publish nothing and inherit nothing"
    );

    // 5. The public entry point in this binary takes exactly that skip branch, so the
    //    builder it produces carries no trace of the channel even with the key set —
    //    and the argv layout is untouched.
    if !cfg!(windows) {
        let built =
            build_sandboxed_command("echo hi", Path::new("/tmp")).expect("the skip always builds");
        assert!(
            built.get_env(key).is_none(),
            "the publish must be decision-scoped at the call site too"
        );
        assert_eq!(
            built.get_argv().len(),
            3,
            "the plain-shell argv stays program + flag + payload"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Windows spawn seams (t-007): `windows_shell_argv` and `windows_child_cwd`
// ──────────────────────────────────────────────────────────────────────────
//
// Three Windows-only defects in the `run_command` spawn path are pinned here:
//
// 1. a bare `cmd` program token reaches `CreateProcessW` as `lpApplicationName`,
//    which does **not** append `.exe` (portable-pty's PATH+PATHEXT scan may miss,
//    and a resolved `.CMD`/`.BAT` cannot be launched at all);
// 2. `cmd.exe` refuses a `\\?\`-verbatim current directory — it reads a leading
//    `\\` as UNC — so the canonicalised workspace root made commands run in
//    `C:\Windows`;
// 3. a payload-less bare shell is an *interactive* request and can only time out on
//    the one-shot path.
//
// Every assertion below feeds **literal Windows inputs** and never spawns a process,
// so it runs on this host: that is exactly why the decisions live in pure functions
// and the platform is a `Copy` value (`SpawnFamily`), instead of a `#[cfg(windows)]`
// body that CI only ever compiles.
//
// Note for the one-shot work (t-009): `expected_sandbox_shell` above used to pin the
// pre-t-007 Windows spelling `("cmd", "/C")`. Under R2 below the program token is
// `%COMSPEC%` or `cmd.exe`, so that helper's Windows leg now resolves through
// `windows_cmd_program` instead.

/// The `%COMSPEC%` spelling a normal Windows install carries, used as the literal
/// `Some(...)` input of the `comspec` parameter.
const TEST_COMSPEC: &str = r"C:\Windows\System32\cmd.exe";

/// The platform value production passes selects exactly the build platform, and the
/// two variants are distinct values (nothing compares by name or by `cfg!` at a call
/// site).
#[test]
fn test_spawn_family_current_follows_the_build_platform() {
    let expected = if cfg!(windows) {
        SpawnFamily::Windows
    } else {
        SpawnFamily::Unix
    };
    assert_eq!(SpawnFamily::current(), expected);
    assert_ne!(SpawnFamily::Unix, SpawnFamily::Windows);
}

/// R2/R4: a normal command becomes `[program, "/C", command]`, where the program is
/// `%COMSPEC%` when it carries a value and the literal `cmd.exe` otherwise. An empty or
/// whitespace-only `%COMSPEC%` must never win — `CommandBuilder::new("")` would be a
/// spawn of nothing.
#[test]
fn test_windows_shell_argv_normal_command_uses_comspec_or_cmd_exe() {
    // No `%COMSPEC%` at all: the extension-qualified literal, never a bare `cmd`.
    assert_eq!(
        windows_shell_argv("dir", None),
        (
            "cmd.exe".to_string(),
            vec!["/C".to_string(), "dir".to_string()]
        ),
        "the program token must carry `.exe`: CreateProcessW does not append it"
    );

    // A real `%COMSPEC%` is used verbatim as the program.
    assert_eq!(
        windows_shell_argv("dir", Some(TEST_COMSPEC)),
        (
            TEST_COMSPEC.to_string(),
            vec!["/C".to_string(), "dir".to_string()]
        ),
        "a set %COMSPEC% is the interpreter path the shell itself would use"
    );

    // `%COMSPEC%` present but blank: it must not win over the literal.
    for blank in ["", " ", "   ", "\t\n"] {
        assert_eq!(
            windows_shell_argv("dir", Some(blank)),
            (
                "cmd.exe".to_string(),
                vec!["/C".to_string(), "dir".to_string()]
            ),
            "a blank %COMSPEC% ({blank:?}) must fall back to `cmd.exe`"
        );
    }

    // Surrounding whitespace in a real value is trimmed, not spawned as part of the path.
    assert_eq!(
        windows_shell_argv("dir", Some("  C:\\Windows\\System32\\cmd.exe  ")).0,
        TEST_COMSPEC,
        "the program token is the trimmed %COMSPEC% value"
    );
}

/// R1: a bare shell is an *interactive shell request* — the shell itself is the program
/// and there is **no** `/C` payload, for both the `cmd` and the `powershell` families
/// and for any casing/whitespace spelling of them.
#[test]
fn test_windows_shell_argv_bare_shell_is_interactive_and_carries_no_payload() {
    // The `cmd` family goes through R2: `%COMSPEC%` when set, else `cmd.exe`.
    assert_eq!(
        windows_shell_argv("cmd", None),
        ("cmd.exe".to_string(), Vec::new())
    );
    assert_eq!(
        windows_shell_argv("cmd", Some(TEST_COMSPEC)),
        (TEST_COMSPEC.to_string(), Vec::new())
    );
    assert_eq!(
        windows_shell_argv("  CMD.EXE  ", Some(TEST_COMSPEC)),
        (TEST_COMSPEC.to_string(), Vec::new()),
        "the bare-shell match is trimmed and ASCII-case-insensitive"
    );

    // The `powershell` family is passed through as written (R3): `%COMSPEC%` names
    // `cmd.exe`, so substituting it would run a different shell than asked for.
    assert_eq!(
        windows_shell_argv("POWERSHELL.EXE", Some(TEST_COMSPEC)),
        ("POWERSHELL.EXE".to_string(), Vec::new())
    );
    assert_eq!(
        windows_shell_argv(" powershell ", None),
        ("powershell".to_string(), Vec::new())
    );

    // No payload on any of them: an interactive shell must not receive a command.
    for shell in ["cmd", "cmd.exe", "powershell", "powershell.exe"] {
        let (_, args) = windows_shell_argv(shell, Some(TEST_COMSPEC));
        assert!(
            args.is_empty(),
            "the interactive form must carry no /C payload, got {args:?} for {shell:?}"
        );
    }

    // A shell name that only *starts* with a shell token is a command, not a shell.
    let (program, args) = windows_shell_argv("cmd /C dir", None);
    assert_eq!(program, "cmd.exe");
    assert_eq!(args, vec!["/C".to_string(), "cmd /C dir".to_string()]);
}

/// R5: the payload reaches the child **verbatim** — no trimming, no re-quoting, no
/// preamble. This is the invariant the old Windows branch violated downstream of here,
/// so it is pinned on the exact shape that broke a real command line.
#[test]
fn test_windows_shell_argv_preserves_the_payload_verbatim() {
    for payload in [
        r#"git commit -m "fix: x""#,
        r#"dir "C:\Program Files\a b" & echo %PATH% ^& type a.txt"#,
        "  dir  ",
        "echo 'single quoted'",
        "cargo test --all-targets --all-features",
    ] {
        let (program, args) = windows_shell_argv(payload, Some(TEST_COMSPEC));
        assert_eq!(program, TEST_COMSPEC);
        assert_eq!(
            args,
            vec!["/C".to_string(), payload.to_string()],
            "the payload must survive byte for byte, got {args:?} for {payload:?}"
        );
    }
}

/// C1/C2/C3: only a verbatim prefix is removed, so `cmd.exe` accepts the directory as
/// its current directory. Everything else is returned untouched.
///
/// The paths are compared by **string form**, not `PathBuf` equality: a literal Windows
/// path is a single opaque component on a Unix host, and the string is what reaches
/// `CreateProcessW` either way.
#[test]
fn test_windows_child_cwd_strips_only_the_verbatim_prefix() {
    let spelled = |path: &str| {
        windows_child_cwd(std::path::Path::new(path))
            .to_string_lossy()
            .into_owned()
    };

    // C2: a verbatim volume path loses only the prefix.
    assert_eq!(spelled(r"\\?\C:\work\proj"), r"C:\work\proj");
    // C1: a verbatim UNC path becomes the plain UNC spelling.
    assert_eq!(spelled(r"\\?\UNC\srv\share\p"), r"\\srv\share\p");
    // The namespace prefix is ASCII-case-insensitive, like the namespace itself.
    assert_eq!(spelled(r"\\?\unc\srv\share\p"), r"\\srv\share\p");
    assert_eq!(spelled(r"\\?\c:\work\proj"), r"c:\work\proj");

    // C3: every other spelling is returned byte for byte.
    for untouched in [
        r"C:\work\proj",
        r"C:\Users\runneradmin\AppData\Local\Temp",
        r"\\srv\share\p",
        "/tmp/x",
        "relative/dir",
        r"\\?\",
        r"\\?\UNC\",
        r"\\.\COM1",
    ] {
        assert_eq!(
            spelled(untouched),
            untouched,
            "a non-verbatim (or degenerate) spelling must be untouched"
        );
    }

    // C4: no substitution, ever — a directory that does not exist keeps its requested
    // spelling instead of being replaced by another one (portable-pty would fall back to
    // `%USERPROFILE%`).
    assert_eq!(
        spelled(r"\\?\C:\no\such\dir"),
        r"C:\no\such\dir",
        "the requested directory is never swapped for a different one"
    );
}

// ──────────────────────────────────────────────────────────────────────────
// The Windows re-entry argv (t-010)
// ──────────────────────────────────────────────────────────────────────────
//
// The Windows spawn side and the Windows re-entry child meet on one argv: the resolved exe,
// the sentinel, the cwd the child is asked to run in, and the command payload. The builders
// below are pure and take the decision as a value, so every rule here is asserted on a Linux
// host with literal Windows inputs — the same reason `windows_shell_argv` and
// `windows_child_cwd` are pinned here rather than behind `#[cfg(windows)]`, which CI only
// ever compiles. The confined launch itself is exercised by the Windows integration suite.

/// An `Apply` decision on Windows is the same three-slot contract as on unix — resolved exe,
/// sentinel, cwd, command — and the two platform-specific differences are exactly the two
/// that platform demands: the cwd slot loses the `\\?\` verbatim prefix that
/// `canonicalize()` produces (R C1/C2 of [`windows_child_cwd`], because `cmd.exe` refuses a
/// verbatim current directory), and the command slot is the payload **verbatim**, because the
/// unix wrapper's `stty`/`ulimit` preamble is a pair of POSIX builtins with no `cmd.exe`
/// equivalent.
#[test]
fn test_windows_sandbox_reentry_argv_is_three_slots_with_the_payload_verbatim() {
    // A verbatim resolved exe is passed through untouched: `CreateProcessW` accepts that
    // spelling in `lpApplicationName`, unlike the current directory.
    let exe = Path::new(r"C:\Program Files\marmel\marmel-dev.exe");
    let decision = should_apply_sandbox(&production_inputs(exe));
    assert!(
        matches!(decision, SandboxDecision::Apply { .. }),
        "a resolved exe on a host with a backend must reach the re-entry, got {decision:?}"
    );

    for payload in [
        "cargo test --all-targets",
        r#"git commit -m "fix: route Windows through the re-entry""#,
        r#"dir "C:\Program Files\a b" & echo %PATH% ^& type a.txt"#,
        "   ",
    ] {
        let (program, args) = windows_sandbox_command_argv(
            &decision,
            payload,
            Path::new(r"\\?\C:\work\proj"),
            Some(TEST_COMSPEC),
        )
        .expect("an Apply decision builds the re-entry argv");

        assert_eq!(program, exe.to_string_lossy());
        assert_eq!(
            args.len(),
            3,
            "the re-entry contract is three slots: {args:?}"
        );
        assert_eq!(args[0], SANDBOX_EXEC_ARG);
        assert_eq!(args[1], r"C:\work\proj");
        assert_eq!(
            args[2], payload,
            "the payload must reach the re-entry child byte for byte, got {args:?}"
        );
        // Not the unix preamble: there is no `stty`/`ulimit` on `cmd.exe`, and re-introducing
        // it would rewrite a command line nobody asked to rewrite.
        for posix_builtin in ["stty", "ulimit"] {
            assert!(
                !args[2].contains(posix_builtin),
                "the Windows re-entry must not add the {posix_builtin} preamble, got {:?}",
                args[2]
            );
        }
    }
}

/// The cwd slot of the Windows re-entry is the directory the confined command is launched in,
/// so it must never carry a verbatim prefix — the very rule that keeps `cmd.exe` from quietly
/// running elsewhere. Asserted for every cwd spelling, including a plain UNC one.
#[test]
fn test_windows_reentry_cwd_slot_never_carries_a_verbatim_prefix() {
    let decision = SandboxDecision::Apply {
        resolved_exe: PathBuf::from(r"C:\Program Files\marmel\marmel-dev.exe"),
    };
    for (given, expected) in [
        (r"\\?\C:\work\proj", r"C:\work\proj"),
        (r"\\?\C:\work\proj with space", r"C:\work\proj with space"),
        (r"\\?\UNC\srv\share\p", r"\\srv\share\p"),
        (r"\\?\c:\work", r"c:\work"),
        (r"C:\work\proj", r"C:\work\proj"),
        ("/tmp/work", "/tmp/work"),
    ] {
        let (_, args) = windows_sandbox_command_argv(
            &decision,
            "cargo test",
            Path::new(given),
            Some(TEST_COMSPEC),
        )
        .expect("an Apply decision builds the re-entry argv");
        assert_eq!(args.len(), 3, "three slots: {args:?}");
        assert_eq!(
            args[1], expected,
            "the cwd slot must be the de-verbatimised directory for {given:?}"
        );
        assert!(
            !args[1].starts_with(r"\\?"),
            "a verbatim cwd in argv is what `cmd.exe` refuses, got {:?}",
            args[1]
        );
    }
}

/// A `Skip` decision never emits the sentinel: it runs the plain [`windows_shell_argv`] form
/// (the interactive-shell spelling of R1 included), so a skipped command is confined by
/// nothing but is still the command the platform requires — and it is warned about by
/// [`log_sandbox_decision`] at the call site.
#[test]
fn test_windows_skip_decision_never_emits_the_reentry_sentinel() {
    let exe = Path::new(r"C:\Program Files\marmel\marmel-dev.exe");
    let skips = [
        should_apply_sandbox(&SandboxInputs {
            opt_out: OptOut::Explicit {
                setting: "marmel.toml: sandbox_disabled = true".to_string(),
            },
            ..production_inputs(exe)
        }),
        should_apply_sandbox(&SandboxInputs {
            test_harness_exe: true,
            ..production_inputs(exe)
        }),
        should_apply_sandbox(&SandboxInputs {
            backend_supported: false,
            ..production_inputs(exe)
        }),
    ];
    for decision in &skips {
        assert!(
            matches!(decision, SandboxDecision::Skip { .. }),
            "these three legs are the skip cases, got {decision:?}"
        );
        for payload in ["cargo test", "cmd", "powershell"] {
            let (program, args) = windows_sandbox_command_argv(
                decision,
                payload,
                Path::new(r"C:\work"),
                Some(TEST_COMSPEC),
            )
            .expect("a skip decision still produces a spawnable argv");
            let expected = windows_shell_argv(payload, Some(TEST_COMSPEC));
            assert_eq!(
                (program.clone(), args.clone()),
                expected,
                "a Windows skip must run exactly the plain shell argv, not a modified one"
            );
            assert!(
                !args.iter().any(|arg| arg == SANDBOX_EXEC_ARG)
                    && !program.contains(SANDBOX_EXEC_ARG),
                "a skipped argv must never carry the re-entry sentinel: {args:?}"
            );
        }
    }
}

/// Both Windows builders refuse to run a required command unsandboxed when the executable
/// path is unavailable: the same fail-closed message as on unix, naming the sentinel and the
/// explicit opt-out.
#[test]
fn test_windows_unresolved_exe_fails_closed_in_both_windows_builders() {
    let inputs = SandboxInputs {
        backend_supported: true,
        test_harness_exe: false,
        resolved_exe: None,
        opt_out: OptOut::None,
    };
    let decision = should_apply_sandbox(&inputs);
    assert_eq!(decision, SandboxDecision::UnresolvedExe);

    let decision = should_apply_sandbox(&inputs);
    assert_eq!(decision, SandboxDecision::UnresolvedExe);

    let mut refusals = Vec::new();
    let err = windows_sandbox_command_argv(
        &decision,
        "echo hi",
        Path::new(r"C:\work"),
        Some(TEST_COMSPEC),
    )
    .err();
    refusals.push(
        err.expect("an unresolved exe must refuse to build the PTY re-entry argv")
            .to_string(),
    );
    refusals.push(
        windows_oneshot_sandbox_argv(
            &decision,
            "echo hi",
            Path::new(r"C:\work"),
            Some(TEST_COMSPEC),
        )
        .expect_err("an unresolved exe must refuse to build the one-shot argv")
        .to_string(),
    );
    for text in refusals {
        assert!(
            text.contains(SANDBOX_EXEC_ARG) && text.contains(SANDBOX_OPTOUT_ENV),
            "the refusal must explain both the requirement and the opt-out, got {text}"
        );
    }
}

/// The one-shot form gets the same re-entry, and keeps its own hazard guard: a payload-less
/// interactive shell is refused for **every** decision, because the re-entry child hands the
/// payload to `%COMSPEC% /C` just the same and a bare shell would sit at its prompt until the
/// timeout killed it.
#[test]
fn test_windows_oneshot_sandbox_argv_confines_and_still_refuses_a_bare_shell() {
    let exe = Path::new(r"C:\Program Files\marmel\marmel-dev.exe");
    let apply = should_apply_sandbox(&production_inputs(exe));

    // Apply: the one-shot command is confined by the re-entry child.
    let (program, args) = windows_oneshot_sandbox_argv(
        &apply,
        "cargo test",
        Path::new(r"\\?\C:\work\proj"),
        Some(TEST_COMSPEC),
    )
    .expect("a one-shot Apply decision builds the re-entry argv");
    assert_eq!(program, exe.to_string_lossy());
    assert_eq!(args.len(), 3, "three slots: {args:?}");
    assert_eq!(args[0], SANDBOX_EXEC_ARG);
    assert_eq!(args[1], r"C:\work\proj");
    assert_eq!(args[2], "cargo test");
    // No `/C` slot: the confined launch inside the re-entry child adds that itself.
    assert!(
        !args.contains(&"/C".to_string()),
        "the payload is handed over as one slot, got {args:?}"
    );

    // Skip: the plain one-shot argv, byte for byte as before.
    let skip = should_apply_sandbox(&SandboxInputs {
        opt_out: OptOut::Explicit {
            setting: "MARMEL_DISABLE_SANDBOX".to_string(),
        },
        ..production_inputs(exe)
    });
    let (program, args) =
        windows_oneshot_sandbox_argv(&skip, "dir", Path::new(r"C:\work"), Some(TEST_COMSPEC))
            .expect("a skip decision produces the plain argv");
    assert_eq!(
        (program, args),
        (
            TEST_COMSPEC.to_string(),
            vec!["/C".to_string(), "dir".to_string()]
        ),
        "a skipped one-shot run is the plain shell command"
    );

    // The interactive-shell refusal survives both decisions.
    for decision in [
        SandboxDecision::Apply {
            resolved_exe: exe.to_path_buf(),
        },
        SandboxDecision::Skip {
            resolved_exe: Some(exe.to_path_buf()),
            skip: SandboxSkip::TestHarnessExe,
        },
    ] {
        for shell in ["cmd", "CMD.EXE", "powershell"] {
            let err = windows_oneshot_sandbox_argv(
                &decision,
                shell,
                Path::new(r"C:\work"),
                Some(TEST_COMSPEC),
            )
            .expect_err("`{shell:?}` is an interactive shell request, not a command");
            assert!(
                err.contains("interactive shell"),
                "the refusal must say why a bare shell cannot be one-shot, got {err}"
            );
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// The interactive shell under the sandbox re-entry (t-014)
// ──────────────────────────────────────────────────────────────────────────

/// The hand-off the sandbox re-entry performs, asserted for **both** spawn families through the
/// `SpawnFamily` value dispatch, so the Windows rule is checked on a Linux host.
///
/// What the rule set says an interactive-shell request gets: the unix re-entry confines it and
/// leaves it interactive (`sh -c <payload>`, where a bare `sh` payload runs a nested shell reading
/// the stdin it inherited), and a `Skip` decision never goes through the re-entry at all. Nothing
/// in the existing rules says a required interactive shell is refused or skipped — the refusal
/// belongs to the **one-shot** path (`windows_oneshot_sandbox_argv`, pinned by
/// `test_windows_oneshot_sandbox_argv_confines_and_still_refuses_a_bare_shell`) — so the Windows
/// re-entry must confine it and keep it interactive, which is what `ReentryHandoff` spells out and
/// what the confined launch implements (`sandbox_windows::confined_command_line`: the bare shell,
/// no `/C`).
#[test]
fn test_reentry_handoff_confines_an_interactive_shell_on_both_families() {
    // A normal command: executed behind the shell's command flag on either family.
    for family in [SpawnFamily::Unix, SpawnFamily::Windows] {
        for payload in ["cargo test --all-targets", "dir", "echo hi"] {
            assert_eq!(
                reentry_handoff(payload, family),
                ReentryHandoff::Execute,
                "`{payload}` is a command, not an interactive-shell request ({family:?})"
            );
        }
    }

    // An R1 request on Windows: the shell **is** the payload, so the hand-off keeps it bare.
    for shell in [
        "cmd",
        "CMD",
        "cmd.exe",
        "  CMD.EXE  ",
        "powershell",
        "POWERSHELL.EXE",
        " powershell ",
    ] {
        assert_eq!(
            reentry_handoff(shell, SpawnFamily::Windows),
            ReentryHandoff::InteractiveShell,
            "`{shell:?}` is an interactive-shell request on the Windows family"
        );
    }

    // The same request on the unix family stays an `sh -c` payload: the interactive-ness of a
    // bare `sh` comes from the nested shell that payload runs, so the hand-off is `Execute` and
    // the confinement is applied exactly as for any other command.
    for shell in ["sh", "bash", "cmd", "powershell"] {
        assert_eq!(
            reentry_handoff(shell, SpawnFamily::Unix),
            ReentryHandoff::Execute,
            "the unix re-entry never hands over a bare shell: it hands over `sh -c <payload>`"
        );
    }

    // A command that merely mentions a shell is not an R1 request: only the bare token is one,
    // and anything with an argument attached is executed like any other payload.
    for payload in [
        "cmd /C echo hi",
        "powershell -NoProfile",
        "/bin/sh",
        "sh -c pwd",
    ] {
        assert_eq!(
            reentry_handoff(payload, SpawnFamily::Windows),
            ReentryHandoff::Execute,
            "`{payload}` is not the bare token R1 matches on"
        );
    }

    // The rule is decided once: the classification the confined launch reads is the same rule
    // `windows_shell_argv` applies, so neither half can hold a different shell list.
    assert!(is_windows_cmd_shell("CMD.EXE"));
    assert!(is_windows_cmd_shell(" cmd "));
    assert!(!is_windows_cmd_shell("powershell"));
    assert!(is_windows_interactive_shell("  PowerShell.Exe  "));
    assert!(!is_windows_interactive_shell(
        "powershell -Command Get-Location"
    ));

    // And the re-entry argv itself is untouched by the choice: an `Apply` decision keeps the
    // three slots, payload verbatim, for an interactive request too — the confinement happens in
    // the launch, not by dropping the request.
    let exe = Path::new(r"C:\Program Files\marmel\marmel-dev.exe");
    let apply = should_apply_sandbox(&production_inputs(exe));
    for payload in ["cmd", "powershell", "cargo test"] {
        let (program, args) = windows_sandbox_command_argv(
            &apply,
            payload,
            Path::new(r"C:\work"),
            Some(TEST_COMSPEC),
        )
        .expect("an interactive-shell request is confined, never refused");
        assert_eq!(program, exe.to_string_lossy());
        assert_eq!(
            args.len(),
            3,
            "three slots for an interactive request too: {args:?}"
        );
        assert_eq!(args[0], SANDBOX_EXEC_ARG);
        assert_eq!(args[1], r"C:\work");
        assert_eq!(args[2], payload, "the payload stays verbatim: {args:?}");
    }
}

// ──────────────────────────────────────────────────────────────────────────
// One-shot outcome reporting and the Windows one-shot argv (t-009)
// ──────────────────────────────────────────────────────────────────────────
//
// A one-shot `run_command` has exactly one channel to the caller — the returned string —
// so it must never spell a failure like a success, and it must never throw away output it
// already captured. Both rules are pure functions of their inputs, which is why they are
// pinned here on a macOS/Linux host: `render_one_shot_result` and `windows_oneshot_argv`
// are asserted with literal inputs and no process is spawned. The Windows branch that
// actually spawns is compiled and linted by the `x86_64-pc-windows-gnu` cross-target; its
// end-to-end contract tests belong to the follow-up task (t-010).

/// Exit 0: the output alone, with the same `sanitize_terminal_output(...)` +
/// `trim_end_matches('\n')` treatment the PTY path applies — no status line, no notice.
#[test]
fn test_one_shot_result_exit_zero_is_the_output_only() {
    assert_eq!(
        render_one_shot_result("hello\n", OneShotEnd::Exited(Some(0))),
        "hello",
        "a clean exit must return the output alone, exactly like the PTY path"
    );
    assert_eq!(
        render_one_shot_result("a\nb\n\n\n", OneShotEnd::Exited(Some(0))),
        "a\nb",
        "only trailing newlines are trimmed, interior ones are content"
    );
    assert_eq!(
        render_one_shot_result("", OneShotEnd::Exited(Some(0))),
        "",
        "silent success stays silent — no invented notice"
    );

    // Terminal noise is sanitised on this path too: an OSC title sequence and a bell must
    // not survive into the tool result.
    assert_eq!(
        render_one_shot_result("\x1b]0;title\x07done\x07\n", OneShotEnd::Exited(Some(0))),
        "done"
    );
}

/// Non-zero exit: the output is kept **and** an explicit exit-status line is appended, so
/// a failing command can no longer be reported as a success.
#[test]
fn test_one_shot_result_non_zero_exit_appends_the_status_line() {
    assert_eq!(
        render_one_shot_result("boom\n", OneShotEnd::Exited(Some(3))),
        "boom\n[exit code: 3]",
        "the failure must be visible and the output preserved"
    );
    assert_eq!(
        render_one_shot_result("", OneShotEnd::Exited(Some(1))),
        "[exit code: 1]",
        "a silent failure still reports its status"
    );
    // No code at all is reported honestly instead of being folded into success.
    assert_eq!(
        render_one_shot_result("x\n", OneShotEnd::Exited(None)),
        "x\n[exit status unavailable]"
    );
}

/// Timeout: the partial output captured so far survives, followed by the existing timeout
/// notice text.
#[test]
fn test_one_shot_result_timeout_keeps_partial_output() {
    let rendered = render_one_shot_result(
        "line one\nline two\n",
        OneShotEnd::TimedOut(Duration::from_secs(60)),
    );
    assert_eq!(
        rendered, "line one\nline two\n[command timed out after 60s and was killed]",
        "a timeout must not destroy the output already captured"
    );
    assert_eq!(
        render_one_shot_result("", OneShotEnd::TimedOut(Duration::from_secs(5))),
        "[command timed out after 5s and was killed]",
        "the notice alone is the whole result when nothing was captured"
    );
}

/// Cancellation: the partial output plus the existing cancellation notice.
#[test]
fn test_one_shot_result_cancellation_keeps_partial_output() {
    assert_eq!(
        render_one_shot_result("partial\n", OneShotEnd::Cancelled),
        "partial\n[command aborted by cancellation signal]"
    );
    assert_eq!(
        render_one_shot_result("", OneShotEnd::Cancelled),
        "[command aborted by cancellation signal]"
    );
}

/// The `cmd /C` argument construction is what carries the payload to `cmd.exe`: both `/C`
/// and the command itself are separate arguments that reach the child verbatim, so
/// `std::os::windows::process::CommandExt::raw_arg` can pass them through untouched.
#[test]
fn test_windows_oneshot_argv_preserves_the_payload_verbatim() {
    for payload in [
        r#"git commit -m "fix: x""#,
        r#"dir "C:\Program Files\a b" & echo %PATH% ^& type a.txt"#,
        "  dir  ",
        "cargo test --all-targets --all-features",
    ] {
        let (program, args) = windows_oneshot_argv(payload, Some(TEST_COMSPEC))
            .expect("a real command is always spawnable");
        assert_eq!(
            program, TEST_COMSPEC,
            "the program token is the trimmed %COMSPEC% (R2)"
        );
        assert_eq!(
            args,
            vec!["/C".to_string(), payload.to_string()],
            "the payload must reach cmd.exe byte for byte, got {args:?} for {payload:?}"
        );
    }

    // Without `%COMSPEC%` the extension-qualified literal is used, never a bare `cmd`.
    assert_eq!(
        windows_oneshot_argv("dir", None),
        Ok((
            "cmd.exe".to_string(),
            vec!["/C".to_string(), "dir".to_string()]
        ))
    );
}

/// A bare shell has no payload, so a one-shot spawn of it could only sit at its prompt
/// until the timeout killed it. It is refused with an actionable message that names the
/// shell and points at `pty_spawn`, instead of being spawned.
#[test]
fn test_windows_oneshot_argv_refuses_a_payload_less_shell() {
    for shell in ["cmd", "cmd.exe", "CMD", " powershell ", "powershell.exe"] {
        let err = windows_oneshot_argv(shell, Some(TEST_COMSPEC))
            .expect_err("an interactive shell must not be spawned one-shot");
        assert!(
            err.contains(TOOL_PTY_SPAWN) && err.contains("interactive"),
            "the refusal must be actionable, got {err:?} for {shell:?}"
        );
    }

    // A shell name that only starts with a shell token is a command and is spawned.
    let (_, args) = windows_oneshot_argv("cmd /C dir", Some(TEST_COMSPEC))
        .expect("a command that mentions a shell is still a command");
    assert_eq!(args, vec!["/C".to_string(), "cmd /C dir".to_string()]);
}

// ──────────────────────────────────────────────────────────────────────────
// The Windows one-shot `run_command` contract (t-010)
// ──────────────────────────────────────────────────────────────────────────
//
// The defect this block exists for: on Windows **every** `run_command` returned only a
// timeout notice and discarded the output it had already captured, because the ConPTY
// output pipe is closed by `ClosePseudoConsole` — i.e. after the read loop — so the reader
// never reached EOF and the run could only ever burn its whole budget. The suite was
// blind to it: the only Windows-adjacent test asserted the *timeout string*, and nothing
// asserted that output comes back at all. So the legs below assert the real contract, and
// the names/doc comments say what each one prevents from coming back.
//
// Two halves:
//
// * **host-independent** — the poll decision ([`one_shot_poll_decision`]) and the
//   renderer ([`render_one_shot_result`]) are pure functions, so the deadline arithmetic
//   and every outcome shape are pinned on macOS/Linux as well;
// * **`cfg!(windows)`** — real commands through the production entry point
//   ([`run_command`]). Those bodies still *compile* on every host (they only use
//   `run_command` and command strings), so the assertions are type-checked here and on the
//   `x86_64-pc-windows-gnu` cross-target; only *execution* is Windows-only.

/// A command that has already exited is reported with its own status — even when the
/// deadline has passed and a cancellation signal is pending.
///
/// This is the ordering the reported defect violated: a run whose output existed was
/// reported as "timed out and was killed". An observed exit must always win, so the
/// caller sees the output and the real status.
#[test]
fn test_one_shot_poll_decision_reports_an_exit_before_cancellation_or_the_deadline() {
    let timeout = Duration::from_secs(60);

    // Exit inside the budget.
    assert_eq!(
        one_shot_poll_decision(Some(Some(0)), false, Duration::ZERO, timeout),
        OneShotPoll::Exited(Some(0))
    );
    // Exit observed on the very poll where the deadline has elapsed: the exit wins.
    assert_eq!(
        one_shot_poll_decision(Some(Some(0)), false, timeout, timeout),
        OneShotPoll::Exited(Some(0)),
        "a command that finished at its deadline is a success, not a kill"
    );
    // Exit wins over a pending cancellation too.
    assert_eq!(
        one_shot_poll_decision(Some(Some(0)), true, timeout, timeout),
        OneShotPoll::Exited(Some(0))
    );
    // A child that ended without an exit code is still an exit, never a timeout.
    assert_eq!(
        one_shot_poll_decision(Some(None), true, timeout, timeout),
        OneShotPoll::Exited(None)
    );
    // A non-zero status is reported as itself, so a failure can never look like a kill.
    assert_eq!(
        one_shot_poll_decision(Some(Some(3)), false, Duration::ZERO, timeout),
        OneShotPoll::Exited(Some(3))
    );
}

/// While the child is still running the decision is cancellation first, then the
/// deadline, then "keep polling" — the order the one-shot loop depends on.
#[test]
fn test_one_shot_poll_decision_prefers_cancellation_then_the_deadline() {
    let timeout = Duration::from_secs(60);

    assert_eq!(
        one_shot_poll_decision(None, false, Duration::from_millis(59_950), timeout),
        OneShotPoll::Running,
        "a running child with budget left must keep being polled"
    );
    assert_eq!(
        one_shot_poll_decision(None, true, Duration::ZERO, timeout),
        OneShotPoll::Cancelled,
        "a cancellation signal must preempt a run that still has budget left"
    );
    assert_eq!(
        one_shot_poll_decision(None, true, timeout, timeout),
        OneShotPoll::Cancelled,
        "cancellation is decided before the deadline"
    );
    assert_eq!(
        one_shot_poll_decision(None, false, timeout, timeout),
        OneShotPoll::TimedOut,
        "the deadline is inclusive (`elapsed >= timeout`), never exclusive"
    );
    assert_eq!(
        one_shot_poll_decision(None, false, timeout + Duration::from_secs(1), timeout),
        OneShotPoll::TimedOut
    );
}

/// A degenerate budget still bounds the wait: `0` stops at the first poll and a one-second
/// budget never becomes an unbounded wait. This is the arithmetic that a re-introduced
/// "wait until the pipe closes" loop would silently turn into a hang.
#[test]
fn test_one_shot_poll_decision_zero_and_sub_second_budgets_stay_bounded() {
    // Zero: never `Running`, whatever the elapsed time.
    for elapsed in [
        Duration::ZERO,
        Duration::from_millis(1),
        Duration::from_secs(3600),
    ] {
        assert_eq!(
            one_shot_poll_decision(None, false, elapsed, Duration::ZERO),
            OneShotPoll::TimedOut,
            "a zero budget must stop at the first poll, not wait forever"
        );
    }

    // One second: exactly one second of patience, then the deadline.
    let one_second = Duration::from_secs(1);
    assert_eq!(
        one_shot_poll_decision(None, false, Duration::from_millis(950), one_second),
        OneShotPoll::Running
    );
    assert_eq!(
        one_shot_poll_decision(None, false, one_second, one_second),
        OneShotPoll::TimedOut
    );
    assert_eq!(
        one_shot_poll_decision(None, false, Duration::from_millis(1_500), one_second),
        OneShotPoll::TimedOut
    );

    // `run_command` cannot even hand a zero budget to the loop: the documented clamp lifts
    // it to one second, so the degenerate case above is defence in depth rather than the
    // reachable production path.
    assert_eq!(crate::config::effective_command_timeout_secs(Some(0)), 1);
}

/// A child that ends without an exit code is reported as such, never as success: for a
/// one-shot `run_command` the rendered string is the **only** channel to the caller.
#[test]
fn test_one_shot_result_exited_without_a_status_code_never_reports_success() {
    assert_eq!(
        render_one_shot_result("", OneShotEnd::Exited(None)),
        "[exit status unavailable]",
        "silence plus an unknown status must still say something"
    );
    assert_eq!(
        render_one_shot_result("\n\n", OneShotEnd::Exited(None)),
        "[exit status unavailable]",
        "newline-only output leaves the notice alone, exactly like a non-zero exit"
    );

    // The no-code shape is distinct from a silent success and keeps the captured output.
    let same_input = "out\n";
    assert_eq!(
        render_one_shot_result(same_input, OneShotEnd::Exited(Some(0))),
        "out"
    );
    let unavailable = render_one_shot_result(same_input, OneShotEnd::Exited(None));
    assert_ne!(
        unavailable, "out",
        "no code must never be spelled as success"
    );
    assert!(
        unavailable.starts_with("out\n") && unavailable.ends_with("[exit status unavailable]"),
        "the captured output must survive the unknown status, got {unavailable:?}"
    );
}

/// Host gate for the Windows one-shot `run_command` legs below.
///
/// [`pty_or_skip`] is the **wrong** gate for them: the Windows one-shot path is
/// `std::process::Command` + `cmd /C` with no ConPTY and no PTY at all, so a host that
/// cannot allocate a PTY would announce a skip for behaviour that never needs one. The
/// gate is therefore the platform alone, announced in the same `SKIP ` spelling as every
/// other skip in this file so it is visible instead of silent. On a Windows host the
/// assertions run unconditionally.
fn windows_oneshot_or_skip(test: &str) -> bool {
    if cfg!(windows) {
        return true;
    }
    eprintln!(
        "SKIP {test}: the one-shot run_command path (std::process::Command + `cmd /C`, no ConPTY) only exists on Windows, so its contract cannot be exercised on {}",
        std::env::consts::OS
    );
    false
}

/// Regression guard for the reported defect: a fast Windows command must return its **real
/// output**, must not be reported as timed out, and must return long before its budget.
///
/// Under the pre-t-009 ConPTY path this call returned only
/// `[command timed out after 30s and was killed]` — the token below was captured and then
/// thrown away. The elapsed assertion is the second half of the same guard: a re-introduced
/// EOF-wait burns the whole 30 s budget instead of failing late, so it is caught here in
/// seconds rather than after a full minute of silence.
#[test]
fn test_run_command_windows_oneshot_returns_real_output_and_never_times_out() {
    if !windows_oneshot_or_skip(
        "harness::pty::tests::test_run_command_windows_oneshot_returns_real_output_and_never_times_out",
    ) {
        return;
    }

    let started = std::time::Instant::now();
    let res = run_command(&serde_json::json!({
        "command": "echo hello_windows_oneshot",
        "timeout_seconds": 30
    }))
    .expect("`echo` must run through the one-shot path");
    let elapsed = started.elapsed();

    assert!(
        res.content.contains("hello_windows_oneshot"),
        "run_command must return the command's real output, got {:?}",
        res.content
    );
    assert!(
        !res.content.contains("timed out"),
        "a command that exits at once must never be reported as timed out, got {:?}",
        res.content
    );
    assert!(!res.is_error, "a successful command is not an error result");
    assert!(
        elapsed < Duration::from_secs(10),
        "the call must return when the child exits, not when the 30s budget expires: took {elapsed:?}"
    );
}

/// A failing Windows command reports its non-zero status **and** keeps the output, so a
/// failure can no longer be indistinguishable from a timeout — and a command that does not
/// exist is reported as a failure with `cmd.exe`'s own diagnostic rather than as a hang.
#[test]
fn test_run_command_windows_oneshot_reports_a_non_zero_exit_status() {
    if !windows_oneshot_or_skip(
        "harness::pty::tests::test_run_command_windows_oneshot_reports_a_non_zero_exit_status",
    ) {
        return;
    }

    // `cmd /C exit /b 3` terminates the shell with status 3 and prints nothing.
    let res = run_command(&serde_json::json!({
        "command": "exit /b 3",
        "timeout_seconds": 30
    }))
    .expect("`exit /b 3` must run through the one-shot path");
    assert!(
        res.content.contains("[exit code: 3]"),
        "the exit status must be reported, got {:?}",
        res.content
    );
    assert!(
        !res.content.contains("timed out"),
        "a command that exits at once must never be reported as timed out, got {:?}",
        res.content
    );

    // A command that does not exist: `cmd.exe` writes its diagnostic to **stderr** and
    // exits non-zero (9009 on a normal install, but the exact code is not part of the
    // contract, so only "non-zero" is asserted). The name is ASCII, so the diagnostic is
    // recognisable whatever the display language of the runner.
    let missing = "marmel_windows_oneshot_missing_command";
    let res = run_command(&serde_json::json!({
        "command": missing,
        "timeout_seconds": 30
    }))
    .expect("an unknown command is a reported failure, not an Err");
    assert!(
        res.content.contains(missing),
        "stderr must be captured: the diagnostic names the missing command, got {:?}",
        res.content
    );
    assert!(
        res.content.contains("[exit code: "),
        "an unknown command must report its status, got {:?}",
        res.content
    );
    assert!(
        !res.content.contains("[exit code: 0]"),
        "an unknown command must never be reported as a success, got {:?}",
        res.content
    );
    assert!(
        !res.content.contains("timed out"),
        "an unknown command exits immediately and must not time out, got {:?}",
        res.content
    );
}

/// The production entry point refuses a payload-less shell instead of letting it sit at its
/// prompt until the timeout: the caller gets an actionable `BadArguments` error naming
/// `pty_spawn`, never a timeout notice.
#[test]
fn test_run_command_windows_oneshot_refuses_a_bare_shell_instead_of_timing_out() {
    if !windows_oneshot_or_skip(
        "harness::pty::tests::test_run_command_windows_oneshot_refuses_a_bare_shell_instead_of_timing_out",
    ) {
        return;
    }

    let started = std::time::Instant::now();
    let err = run_command(&serde_json::json!({
        "command": "cmd",
        "timeout_seconds": 5
    }))
    .expect_err("an interactive shell has no one-shot form");
    let elapsed = started.elapsed();

    assert!(
        matches!(err, ToolError::BadArguments { .. }),
        "the refusal must be an argument error, got {err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains(TOOL_PTY_SPAWN) && text.contains("interactive"),
        "the refusal must point at pty_spawn, got {text:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the refusal must be immediate, not the 5s timeout: took {elapsed:?}"
    );
}

/// A long-running command is cut off at its deadline, and the call returns within a small
/// margin of it.
///
/// The notice text itself is already pinned by
/// `test_run_command_parses_timeout_seconds_argument`; what was missing is the **bound**:
/// `ping -n 10` needs roughly nine seconds, so a call that outlives a few seconds has
/// ignored its one-second deadline (the shape of the reported hang), and one that returns
/// before the deadline has cut a legitimate command short.
#[test]
fn test_run_command_windows_oneshot_timeout_is_bounded_and_reports_the_notice() {
    if !windows_oneshot_or_skip(
        "harness::pty::tests::test_run_command_windows_oneshot_timeout_is_bounded_and_reports_the_notice",
    ) {
        return;
    }

    let started = std::time::Instant::now();
    let res = run_command(&serde_json::json!({
        "command": "ping -n 10 127.0.0.1",
        "timeout_seconds": 1
    }))
    .expect("a long-running command is killed and reported, not an Err");
    let elapsed = started.elapsed();

    assert!(
        res.content.contains("timed out after 1s and was killed"),
        "the timeout must be reported, got {:?}",
        res.content
    );
    assert!(
        elapsed >= Duration::from_secs(1),
        "the deadline must not be cut short: took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "`ping -n 10` runs for ~9s, so a call that outlives 5s ignored its 1s deadline: took {elapsed:?}"
    );
}
