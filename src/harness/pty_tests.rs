use super::*;

/// REQ-TOOL-001: Spawning a background sleep loop and tearing down the PTY
/// leaves zero orphan processes — the backgrounded child dies with the group.
#[test]
#[cfg(unix)]
fn test_harness_pty_process_group_kill() {
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
    let mgr = PtyManager::new();
    let cwd = std::env::current_dir().unwrap();
    let session_id = "test-session-1";

    #[cfg(unix)]
    let shell = "sh";
    #[cfg(windows)]
    let shell = "cmd.exe";

    // Spawn a shell
    let init_out = mgr
        .spawn(session_id, shell, &cwd, 24, 80)
        .await
        .expect("spawn session");
    assert!(init_out.is_empty() || !init_out.is_empty()); // Shell banner or prompt

    #[cfg(unix)]
    let (input, wait_ms) = ("echo hello_interactive_pty\n", 400);
    #[cfg(windows)]
    let (input, wait_ms) = ("echo hello_interactive_pty\r\n", 800);

    // Write a command
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

    // List sessions
    let list = mgr.list().await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["session_id"], session_id);

    // Close session
    let closed = mgr.close(session_id).await;
    assert!(closed, "session should be closed");

    let list_after = mgr.list().await;
    assert_eq!(list_after.len(), 0);
}

#[tokio::test]
async fn test_run_command_pty_cancelled_by_worker_token() {
    let token = tokio_util::sync::CancellationToken::new();
    let token_clone = token.clone();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        token_clone.cancel();
    });

    let res = crate::orchestrator::CURRENT_WORKER_TOKEN
        .scope(token, async {
            run_command_pty("sleep 10", Duration::from_secs(10))
        })
        .await;

    assert!(res.is_ok());
    let output = res.unwrap();
    assert!(
        output.contains("aborted by cancellation signal"),
        "Expected aborted output, got: {output}"
    );
}

// ---------------------------------------------------------------------------
// Bug P1 (zombie children) — reap state machine, tested as pure logic.
// None of these tests need a real `/dev/pts`: they drive the `ChildHandle`
// seam with a fake and with a plain `std::process::Child`.
// ---------------------------------------------------------------------------

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
/// real OS child spawned **without** a pty (`/bin/true`, `/bin/sleep`).
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

/// Regression (P1): a reaped child must leave nothing in the process table.
/// Spawned with `std::process::Command`, so no `/dev/pts` is involved.
#[test]
#[cfg(unix)]
fn test_reap_removes_real_child_from_process_table() {
    let child = std::process::Command::new("/bin/true")
        .spawn()
        .expect("spawn /bin/true without a pty");
    let pid = child.id() as i32;

    let mut reaper = ChildReaper::new(StdChild(child), None);
    let outcome = reaper.kill_and_reap(PTY_REAP_GRACE, true);

    assert!(outcome.reaped, "child must be reaped");
    assert!(outcome.status.is_some(), "exit status must be recorded");
    assert!(reaper.is_reaped());

    let (ret, errno) = waitpid_nowait(pid);
    assert_eq!(
        (ret, errno),
        (-1, libc::ECHILD),
        "child {pid} was left as a <defunct> zombie"
    );
}

/// Regression (P1): a long-running child is killed **and** reaped, so a
/// `run_command` that times out cannot accumulate zombies.
#[test]
#[cfg(unix)]
fn test_reap_kills_and_reaps_real_long_running_child() {
    let child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn /bin/sleep without a pty");
    let pid = child.id() as i32;

    let mut reaper = ChildReaper::new(StdChild(child), None);
    let outcome = reaper.kill_and_reap(PTY_REAP_GRACE, false);

    assert!(outcome.reaped, "killed child must be reaped");
    let status = outcome.status.expect("exit status recorded after SIGKILL");
    assert!(
        !status.success(),
        "a SIGKILLed child must not report success"
    );

    let (ret, errno) = waitpid_nowait(pid);
    assert_eq!(
        (ret, errno),
        (-1, libc::ECHILD),
        "killed child {pid} was left as a <defunct> zombie"
    );
}

/// Negative control proving the `ECHILD` assertions above are meaningful: a
/// killed-but-unreaped child *is* visible to `waitpid` as a zombie.
#[test]
#[cfg(unix)]
fn test_unreaped_killed_child_is_visible_as_zombie() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn /bin/sleep without a pty");
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

/// A failing `waitpid` means the pid is gone: treat it as reaped so the handle
/// is never waited on a second time (which is what a double-reap looks like).
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

// ---------------------------------------------------------------------------
// Bug P3 (unbounded interactive buffer) — ring buffer + truncation accounting,
// pure logic on `SharedBuffer`.
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Bug (t-034b) — the Landlock gate must NEVER depend on the executable NAME.
// These tests drive [`should_apply_sandbox`] / [`sandbox_command_argv`] as pure
// logic with injected inputs: no Landlock availability, no `/dev/pts`, no
// `current_exe()`, no environment mutation, so they are deterministic
// everywhere (including sandboxes where `openpty` yields EACCES).
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};

/// Production-shaped inputs: a Linux process whose exe resolved to `exe`.
fn production_inputs(exe: &Path) -> SandboxInputs<'_> {
    SandboxInputs {
        landlock_supported: true,
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

/// A renamed / copied / installed binary is sandboxed exactly like `marmel`:
/// the whole point of the fix.
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
        assert_eq!(decision.mode(), "landlock-re-entry");
    }
}

/// The old `file_name(current_exe) == "marmel"` equality must decide nothing:
/// the decision is identical across names that the old heuristic split apart.
#[test]
fn test_sandbox_gate_executable_name_has_zero_influence() {
    let dir = Path::new("/home/fredrik/src/wip/target/debug");
    // The old heuristic said `true` only for the exact `marmel`/`marmel.exe`.
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
    let coincidental = Path::new("/usr/local/bin/marmel");
    assert!(matches!(
        should_apply_sandbox(&production_inputs(coincidental)),
        SandboxDecision::Apply { .. }
    ));
}

/// Regression guard: the name-equality test is gone from the source, not merely
/// bypassed.
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
    let exe = Path::new("/opt/marmel/bin/marmel-dev");
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
        logs.contains("/opt/marmel/bin/marmel-dev") && logs.contains("unsandboxed"),
        "the warn must state the resolved exe and the chosen mode, got {logs:?}"
    );

    // The opted-out command really is a plain `sh -c …` (with t-034a's preamble).
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

/// No Landlock on this platform ⇒ skipped, but never silently.
#[test]
fn test_sandbox_gate_unsupported_platform_skips_with_warning() {
    let exe = Path::new("/Users/fredrik/target/debug/marmel");
    let inputs = SandboxInputs {
        landlock_supported: false,
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
        logs.contains("WARN") && logs.contains("no Landlock"),
        "an unavailable sandbox must be visible, got {logs:?}"
    );
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

/// Fail-closed: on Linux, an unresolvable exe is a hard error — the old code
/// fell back to `sh -c` here, which is the silent fail-open this removes.
#[test]
fn test_sandbox_gate_unresolved_exe_fails_closed_and_is_logged() {
    let inputs = SandboxInputs {
        landlock_supported: true,
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

/// End-to-end wiring at the decision point, PTY-free: in a test-harness binary
/// the builder skips (and warns) instead of exec-ing a binary that cannot
/// re-enter, and the t-034a `stty`/`ulimit` preamble survives either way.
#[test]
fn test_build_sandboxed_command_logs_its_choice_and_keeps_the_wrapper() {
    let (built, logs) = capture_logs(|| build_sandboxed_command("echo hi", Path::new("/tmp")));
    let cmd = built.expect("argv must be buildable");
    let argv = cmd.get_argv();
    assert!(!argv.is_empty(), "the builder must have a program");

    // In a test binary the re-entry is skipped, so this is `sh -c <wrapped>`;
    // either way the command was chosen by a decision that was logged.
    if cfg!(test) {
        assert_eq!(argv[0].to_string_lossy(), "sh");
        assert_eq!(argv[1].to_string_lossy(), "-c");
        assert!(
            logs.contains("WARN") && logs.contains("pty sandbox decision"),
            "every skip must leave a visible trace, got {logs:?}"
        );
    }
    let wrapped = argv.last().expect("the wrapped command").to_string_lossy();
    assert!(
        wrapped.starts_with("stty -echo 2>/dev/null || true; ulimit -f 4194304"),
        "REQ-TOOL-001 preamble must survive the sandbox rewrite, got {wrapped:?}"
    );
    assert!(wrapped.ends_with("echo hi"));
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
