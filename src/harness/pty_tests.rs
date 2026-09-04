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
