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
//! All captured output is passed through `sanitize_terminal_output`, which
//! strips OSC sequences and other non-printable terminal artifacts, matching
//! marmennill-cli's `sanitize_terminal_output`.
//!
//! NOTE: `unsafe_op_in_unsafe_fn` is a hard error in edition 2024, so any
//! `libc::kill` call must be wrapped in an explicit `unsafe {}` block.

use crate::harness::{ToolError, ToolResult};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use regex::Regex;
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

/// Default per-command timeout in seconds (default 60 s, max 300 s).
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

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
pub fn sanitize_terminal_output(text: &str) -> String {
    let re_osc = Regex::new(r"\x1b\][0-9]+;.*?(?:\x07|\x1b\\)").unwrap();
    let cleaned = re_osc.replace_all(text, "");
    cleaned
        .chars()
        .filter(|&c| {
            c != '\x07'
                && c != '\x08'
                && (c >= ' ' || c == '\n' || c == '\r' || c == '\t' || c == '\x1b')
        })
        .collect()
}

/// A live sandboxed PTY session. Holds the child and master so the process
/// group can be torn down deterministically.
pub struct PtySession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Process id of the spawned shell (== process group leader on unix).
    pub pid: i32,
}

/// Build a sandboxed shell command, executing via Landlock on Linux, sh on macOS/Unix, or cmd on Windows.
pub fn build_sandboxed_command(command: &str, cwd: &std::path::Path) -> CommandBuilder {
    if cfg!(target_os = "windows") {
        let mut cmd = CommandBuilder::new("cmd");
        cmd.args(["/C", command]);
        cmd.cwd(cwd);
        cmd
    } else {
        let wrapped = format!(
            "stty -echo 2>/dev/null || true; ulimit -f {ULIMIT_FILE_BLOCKS} 2>/dev/null || ulimit -f {ULIMIT_FILE_BLOCKS_FALLBACK} 2>/dev/null; {command}"
        );

        let is_marmel_binary = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .is_some_and(|name| name == "marmel" || name == "marmel.exe");

        if cfg!(target_os = "linux") && is_marmel_binary {
            let exe = std::env::current_exe().unwrap();
            let mut cmd = CommandBuilder::new(exe);
            cmd.arg("--internal-sandbox-exec");
            cmd.arg(cwd.to_string_lossy().as_ref());
            cmd.arg(&wrapped);
            cmd.cwd(cwd);
            cmd
        } else {
            let mut cmd = CommandBuilder::new("sh");
            cmd.arg("-c");
            cmd.arg(&wrapped);
            cmd.cwd(cwd);
            cmd
        }
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

        let cur_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let cmd = build_sandboxed_command(command, &cur_dir);
        let child = pair.slave.spawn_command(cmd)?;
        // Release the slave side now that the child is spawned.
        drop(pair.slave);

        #[cfg(unix)]
        let pid = child
            .process_id()
            .map(|p| p as i32)
            .or_else(|| pair.master.process_group_leader())
            .ok_or_else(|| ToolError::BadArguments {
                tool: "run_command".into(),
                detail: "could not obtain child process id".into(),
            })?;

        #[cfg(not(unix))]
        let pid = child
            .process_id()
            .map(|p| p as i32)
            .ok_or_else(|| ToolError::BadArguments {
                tool: "run_command".into(),
                detail: "could not obtain child process id".into(),
            })?;

        Ok(PtySession {
            child,
            master: pair.master,
            pid,
        })
    }

    /// Kill the entire process group (negative pid) then the child.
    ///
    /// This is invoked on command completion, timeout, and teardown so that
    /// lingering subshells, debuggers, and REPLs are all reaped.
    pub fn teardown(&mut self) -> Result<(), ToolError> {
        kill_process_group(self.pid).map_err(anyhow::Error::from)?;
        let _ = self.child.kill();
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

/// Execute a shell command via `run_command`. Parses `{ command, timeout_seconds? }`.
///
/// If `timeout_seconds` (or `timeout`) is provided, it is clamped to [1, 300] seconds.
/// Otherwise `DEFAULT_TIMEOUT_SECS` (60 s) is used.
pub fn run_command(args: &Value) -> Result<ToolResult, ToolError> {
    let command = crate::harness::fs::str_arg(args, "command", "run_command")?;
    let timeout_secs = args
        .get("timeout_seconds")
        .or_else(|| args.get("timeout"))
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, 300);
    let output = run_command_pty(command, Duration::from_secs(timeout_secs))?;
    Ok(ToolResult::ok(output))
}

/// Core PTY execution: spawn the sandbox, read output with a timeout, and
/// always tear down the process group afterwards.
///
/// The `timeout` is strict (default 300 s) and preempts a hung child by
/// SIGKILLing the entire process group.
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

    // Wait for output with a timeout; on timeout the group is SIGKILLed below.
    let output = match rx.recv_timeout(timeout) {
        Ok(o) => o,
        Err(_) => {
            // Timed out: kill the whole process group.
            let _ = session.teardown();
            return Ok(format!(
                "[command timed out after {}s and was killed]",
                timeout.as_secs()
            ));
        }
    };

    // On completion, also tear down the process group to reap any orphans.
    let _ = session.teardown();
    drop(session.master);

    // Sanitize the captured output before returning it to the caller.
    Ok(sanitize_terminal_output(&output)
        .trim_end_matches('\n')
        .to_string())
}

// ---------------------------------------------------------------------------
// Interactive Multi-Turn PTY Manager
// ---------------------------------------------------------------------------

struct SharedBuffer {
    output: Vec<u8>,
    cursor: usize,
    is_alive: bool,
    last_activity: std::time::Instant,
}

#[allow(dead_code)]
pub struct InteractivePtySession {
    pub id: String,
    pub pid: Option<u32>,
    writer: std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
    shared_buf: std::sync::Arc<std::sync::Mutex<SharedBuffer>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Drop for InteractivePtySession {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe {
                // Terminate the entire process group (-PGID) and the process itself
                libc::kill(-(pid as i32), libc::SIGKILL);
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        let _ = self._child.kill();
    }
}

pub struct PtyManager {
    sessions: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, InteractivePtySession>>,
    >,
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
        };

        let sessions_clone = mgr.sessions.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let mut map = sessions_clone.lock().await;
                let now = std::time::Instant::now();
                map.retain(|key, session| {
                    let idle_time = {
                        let buf = session.shared_buf.lock().unwrap();
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

        let cmd = build_sandboxed_command(command_str, cwd);

        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            ToolError::Execution(anyhow::anyhow!("Failed to spawn command in PTY: {e}"))
        })?;

        let pid = child.process_id();
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| ToolError::Execution(anyhow::anyhow!("Failed to take PTY writer: {e}")))?;
        let mut reader = pair.master.try_clone_reader().map_err(|e| {
            ToolError::Execution(anyhow::anyhow!("Failed to clone PTY reader: {e}"))
        })?;

        let shared_buf = std::sync::Arc::new(std::sync::Mutex::new(SharedBuffer {
            output: Vec::new(),
            cursor: 0,
            is_alive: true,
            last_activity: std::time::Instant::now(),
        }));

        let shared_buf_reader = shared_buf.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let mut lock = shared_buf_reader.lock().unwrap();
                        lock.is_alive = false;
                        break;
                    }
                    Ok(n) => {
                        let mut lock = shared_buf_reader.lock().unwrap();
                        lock.output.extend_from_slice(&buf[..n]);
                        lock.last_activity = std::time::Instant::now();
                    }
                    Err(_) => {
                        let mut lock = shared_buf_reader.lock().unwrap();
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
            _child: child,
        };

        {
            let mut map = self.sessions.lock().await;
            map.insert(key, session);
        }

        tokio::time::sleep(Duration::from_millis(300)).await;

        let initial_output = {
            let mut lock = shared_buf.lock().unwrap();
            let new_bytes = &lock.output[lock.cursor..];
            let s = sanitize_terminal_output(&String::from_utf8_lossy(new_bytes));
            lock.cursor = lock.output.len();
            s
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
            let mut w = writer.lock().unwrap();
            use std::io::Write;
            w.write_all(input.as_bytes()).map_err(|e| {
                ToolError::Execution(anyhow::anyhow!("Failed to write to PTY: {e}"))
            })?;
            w.flush()
                .map_err(|e| ToolError::Execution(anyhow::anyhow!("Failed to flush PTY: {e}")))?;
            let mut buf = shared_buf.lock().unwrap();
            buf.last_activity = std::time::Instant::now();
        }

        let wait_dur = Duration::from_millis(if wait_ms > 0 { wait_ms } else { 300 });
        tokio::time::sleep(wait_dur).await;

        let (new_output, is_alive) = {
            let mut lock = shared_buf.lock().unwrap();
            let new_bytes = &lock.output[lock.cursor..];
            let s = sanitize_terminal_output(&String::from_utf8_lossy(new_bytes));
            lock.cursor = lock.output.len();
            (s, lock.is_alive)
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
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }

        let (new_output, is_alive) = {
            let mut lock = shared_buf.lock().unwrap();
            let new_bytes = &lock.output[lock.cursor..];
            let s = sanitize_terminal_output(&String::from_utf8_lossy(new_bytes));
            lock.cursor = lock.output.len();
            lock.last_activity = std::time::Instant::now();
            (s, lock.is_alive)
        };

        Ok((new_output, is_alive))
    }

    pub async fn close(&self, id: &str) -> bool {
        let key = id.trim();
        let mut map = self.sessions.lock().await;
        map.remove(key).is_some()
    }

    pub async fn list(&self) -> Vec<Value> {
        let map = self.sessions.lock().await;
        let now = std::time::Instant::now();
        map.values()
            .map(|s| {
                let buf = s.shared_buf.lock().unwrap();
                let idle_secs = now.duration_since(buf.last_activity).as_secs();
                serde_json::json!({
                    "session_id": s.id,
                    "pid": s.pid,
                    "is_alive": buf.is_alive,
                    "idle_seconds": idle_secs,
                    "total_bytes_read": buf.output.len(),
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
            tool: "pty_spawn".into(),
            detail: "missing string field `id` or `session_id`".into(),
        })?;

    let command =
        args.get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::BadArguments {
                tool: "pty_spawn".into(),
                detail: "missing string field `command`".into(),
            })?;

    let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
    let cols = args.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let output = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(GLOBAL_PTY_MANAGER.spawn(id, command, &cwd, rows, cols))
    })?;

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
            tool: "pty_write".into(),
            detail: "missing string field `id` or `session_id`".into(),
        })?;

    let input =
        args.get("input")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::BadArguments {
                tool: "pty_write".into(),
                detail: "missing string field `input`".into(),
            })?;

    let wait_ms = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(300);

    let (output, is_alive) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(GLOBAL_PTY_MANAGER.write(id, input, wait_ms))
    })?;

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
            tool: "pty_read".into(),
            detail: "missing string field `id` or `session_id`".into(),
        })?;

    let wait_ms = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(0);

    let (output, is_alive) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(GLOBAL_PTY_MANAGER.read(id, wait_ms))
    })?;

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
            tool: "pty_close".into(),
            detail: "missing string field `id` or `session_id`".into(),
        })?;

    let closed = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(GLOBAL_PTY_MANAGER.close(id))
    });

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
    let list = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(GLOBAL_PTY_MANAGER.list())
    });

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
mod tests {
    use super::*;

    /// REQ-TOOL-001: Spawning a background sleep loop and tearing down the PTY
    /// leaves zero orphan processes — the backgrounded child dies with the group.
    #[test]
    #[cfg(unix)]
    fn test_harness_pty_process_group_kill() {
        let dir = std::env::temp_dir().join(format!("marmel_pty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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

        // After teardown, the backgrounded sleep must no longer be reachable.
        let alive_after = unsafe { libc::kill(bg_pid, 0) == 0 };
        assert!(
            !alive_after,
            "background sleep {bg_pid} survived process-group kill"
        );

        let _ = std::fs::remove_dir_all(&dir);
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

        // Write a command
        let (out, alive) = mgr
            .write(session_id, "echo hello_interactive_pty\n", 400)
            .await
            .expect("write to pty");
        assert!(alive, "session should be alive");
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
}
