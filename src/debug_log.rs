//! Detailed debug logging for all input/output traffic when `--debug` is active.
//!
//! Logs all LLM HTTP traffic (full requests with messages/tools/options, streaming chunks,
//! full replies, reasoning, token counts, errors, latencies) and all tool invocations
//! with their arguments and return outputs to a dedicated `debug.log` file.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static DEBUG_LOG_ENABLED: AtomicBool = AtomicBool::new(false);
static DEBUG_LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
static DEBUG_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub const DEFAULT_DEBUG_LOG_NAME: &str = "debug.log";

/// Initializes debug logging to the specified file path (or `.marmel/debug.log`).
pub fn init(path: Option<PathBuf>) {
    let log_path = path.unwrap_or_else(|| {
        let ws = crate::harness::workspace::Workspace::new();
        ws.map(|w| w.root().join(DEFAULT_DEBUG_LOG_NAME))
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_DEBUG_LOG_NAME))
    });

    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = DEBUG_LOG_PATH.set(log_path.clone());
        let _ = DEBUG_LOG_FILE.set(Mutex::new(file));
        DEBUG_LOG_ENABLED.store(true, Ordering::SeqCst);

        log_raw(&format!(
            "\n\n{}\n=== MARMEL DEBUG LOG STARTED [{}] (PID: {}) ===\n{}\n",
            "=".repeat(80),
            chrono::Utc::now().to_rfc3339(),
            std::process::id(),
            "=".repeat(80),
        ));
    }
}

/// Returns true if `--debug` logging is enabled.
pub fn is_enabled() -> bool {
    DEBUG_LOG_ENABLED.load(Ordering::Relaxed)
}

/// Explicitly enable or disable debug logging.
pub fn set_enabled(enabled: bool) {
    DEBUG_LOG_ENABLED.store(enabled, Ordering::SeqCst);
}

/// Returns the path to the active debug log file, if enabled.
pub fn debug_log_path() -> Option<&'static Path> {
    DEBUG_LOG_PATH.get().map(|p| p.as_path())
}

/// Write raw text safely to the debug log file.
pub fn log_raw(text: &str) {
    if !is_enabled() {
        return;
    }
    if let Some(mutex) = DEBUG_LOG_FILE.get()
        && let Ok(mut file) = mutex.lock()
    {
        let _ = file.write_all(text.as_bytes());
        let _ = file.flush();
    }
}

/// Log an outgoing LLM request with full payload.
pub fn log_llm_request<T: serde::Serialize>(url: &str, model: &str, payload: &T) {
    if !is_enabled() {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let json_pretty = serde_json::to_string_pretty(payload)
        .unwrap_or_else(|_| "<unserializable payload>".to_string());

    let entry = format!(
        "\n{}\n[{now}] >>> [LLM REQUEST] POST {url}\nModel: {model}\nPayload:\n{json_pretty}\n{}\n",
        "-".repeat(80),
        "-".repeat(80)
    );
    log_raw(&entry);
}

/// Log an incoming LLM response with timing and full details.
pub fn log_llm_response(
    url: &str,
    model: &str,
    status: u16,
    elapsed_ms: u128,
    reply: &crate::llm::StreamedReply,
) {
    if !is_enabled() {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let tool_calls_str = if reply.tool_calls.is_empty() {
        "None".to_string()
    } else {
        serde_json::to_string_pretty(&reply.tool_calls).unwrap_or_default()
    };

    let entry = format!(
        "\n{}\n[{now}] <<< [LLM RESPONSE] HTTP {status} (elapsed: {elapsed_ms}ms)\nURL: {url} | Model: {model}\nReasoning / Thinking:\n{}\nContent:\n{}\nTool Calls:\n{}\n{}\n",
        "-".repeat(80),
        if reply.reasoning.is_empty() {
            "(none)"
        } else {
            &reply.reasoning
        },
        if reply.content.is_empty() {
            "(none)"
        } else {
            &reply.content
        },
        tool_calls_str,
        "-".repeat(80)
    );
    log_raw(&entry);
}

/// Log an LLM error with timing and error details.
pub fn log_llm_error(url: &str, model: &str, elapsed_ms: u128, err_str: &str) {
    if !is_enabled() {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let entry = format!(
        "\n{}\n[{now}] <<< [LLM ERROR] (elapsed: {elapsed_ms}ms)\nURL: {url} | Model: {model}\nError:\n{err_str}\n{}\n",
        "-".repeat(80),
        "-".repeat(80)
    );
    log_raw(&entry);
}

/// Log an outgoing tool invocation with caller and arguments.
pub fn log_tool_invocation(caller: &str, tool_name: &str, args: &serde_json::Value) {
    if !is_enabled() {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let args_pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
    let entry = format!(
        "\n{}\n[{now}] >>> [TOOL INVOCATION]\nCaller: {caller}\nTool: {tool_name}\nArguments:\n{args_pretty}\n{}\n",
        ".".repeat(80),
        ".".repeat(80)
    );
    log_raw(&entry);
}

/// Log a tool result with elapsed time and return content.
pub fn log_tool_result(
    caller: &str,
    tool_name: &str,
    elapsed_ms: u128,
    result: &str,
    is_err: bool,
) {
    if !is_enabled() {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let status_tag = if is_err { "ERROR" } else { "OK" };
    let entry = format!(
        "\n{}\n[{now}] <<< [TOOL RESULT: {status_tag}] (elapsed: {elapsed_ms}ms)\nCaller: {caller}\nTool: {tool_name}\nOutput:\n{result}\n{}\n",
        ".".repeat(80),
        ".".repeat(80)
    );
    log_raw(&entry);
}

/// Log an arbitrary custom debug entry.
pub fn log_custom(tag: &str, message: &str) {
    if !is_enabled() {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let entry = format!("[{now}] [{tag}] {message}\n");
    log_raw(&entry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_log_disabled_by_default() {
        assert!(!is_enabled());
    }

    #[test]
    fn test_debug_log_writing_when_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let log_file = temp.path().join("test_debug.log");
        init(Some(log_file.clone()));
        assert!(is_enabled());

        log_custom("TEST", "Hello debug log");
        log_tool_invocation(
            "Manager",
            "delegate_task",
            &serde_json::json!({"prompt": "do something"}),
        );
        log_tool_result("Manager", "delegate_task", 42, "Done!", false);

        let content = std::fs::read_to_string(&log_file).unwrap();
        assert!(content.contains("=== MARMEL DEBUG LOG STARTED"));
        assert!(content.contains("[TEST] Hello debug log"));
        assert!(content.contains(">>> [TOOL INVOCATION]"));
        assert!(content.contains("delegate_task"));
        assert!(content.contains("<<< [TOOL RESULT: OK]"));
        set_enabled(false);
    }
}
