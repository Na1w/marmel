//! Tool argument formatting for previews and logs.

use crate::tool_names::{
    TOOL_DELEGATE_TASK, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_READ_FILE, TOOL_REPLACE,
    TOOL_RUN_COMMAND, TOOL_SLEEP, TOOL_WRITE_FILE,
};

pub fn format_tool_args_preview(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        TOOL_READ_FILE | TOOL_WRITE_FILE | TOOL_REPLACE => args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_RUN_COMMAND => {
            let cmd = args
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if cmd.len() > 40 {
                let cut = cmd.floor_char_boundary(37);
                format!("{}…", &cmd[..cut])
            } else {
                cmd.to_string()
            }
        }
        TOOL_GREP_SEARCH => args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_GLOB => args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_SLEEP => {
            let secs = args
                .get("seconds")
                .or_else(|| args.get("duration"))
                .or_else(|| args.get("duration_seconds"))
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .unwrap_or(5);
            format!("{secs}s")
        }
        _ => {
            let s = args.to_string();
            if s.len() > 30 {
                let cut = s.floor_char_boundary(27);
                format!("{}…", &s[..cut])
            } else {
                s
            }
        }
    }
}

pub fn format_tool_args_full(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        TOOL_WRITE_FILE => {
            if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
                let len = args
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .map_or(0, str::len);
                format!("{path} (content: {len} bytes)")
            } else {
                args.to_string()
            }
        }
        TOOL_REPLACE => {
            if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
                let old_len = args
                    .get("old_str")
                    .and_then(serde_json::Value::as_str)
                    .map_or(0, str::len);
                let new_len = args
                    .get("new_str")
                    .and_then(serde_json::Value::as_str)
                    .map_or(0, str::len);
                format!("{path} (replace: {old_len}b -> {new_len}b)")
            } else {
                args.to_string()
            }
        }
        TOOL_READ_FILE => {
            if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
                path.to_string()
            } else {
                args.to_string()
            }
        }
        TOOL_RUN_COMMAND => args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_GREP_SEARCH => {
            let q = args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if path.is_empty() {
                format!("query=\"{q}\"")
            } else {
                format!("query=\"{q}\", path=\"{path}\"")
            }
        }
        TOOL_GLOB => args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_DELEGATE_TASK => {
            let ag = args
                .get("agent_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let tid = args
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let pr = args
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("agent={ag}, task_id={tid}, prompt=\"{pr}\"")
        }
        name if crate::agents::validation::is_leave_verdict_tool(name) => {
            let v = args
                .get("verdict")
                .or_else(|| args.get("status"))
                .or_else(|| args.get("decision"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let c = args
                .get("comments")
                .or_else(|| args.get("comment"))
                .or_else(|| args.get("feedback"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("verdict={v}, comments=\"{c}\"")
        }
        TOOL_SLEEP => {
            let secs = args
                .get("seconds")
                .or_else(|| args.get("duration"))
                .or_else(|| args.get("duration_seconds"))
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .unwrap_or(5);
            let reason = args
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if reason.is_empty() {
                format!("seconds={secs}")
            } else {
                format!("seconds={secs}, reason=\"{reason}\"")
            }
        }
        _ => args.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_format_tool_args_preview_utf8_char_boundary_no_panic() {
        // Reproduce the user's exact panic:
        // '—' is 3 bytes (bytes 26..29 of the serialized JSON string), so byte index 27 lands inside '—'.
        let prefix = "a".repeat(24);
        let val = json!({ "k": format!("{prefix}—rest_of_string") });
        let res = format_tool_args_preview("custom_tool", &val);
        assert!(res.ends_with('…'));

        // Also test run_command with multi-byte character around byte 37
        let cmd_prefix = "b".repeat(36);
        let cmd_val = json!({ "command": format!("{cmd_prefix}—cargo test") });
        let cmd_res = format_tool_args_preview(TOOL_RUN_COMMAND, &cmd_val);
        assert!(cmd_res.ends_with('…'));
    }

    #[test]
    fn test_format_tool_args_full_replace_and_write_file() {
        let replace_val = json!({
            "path": "cpu/ppc/jit/jit.cpp",
            "old_str": "int a = 1;",
            "new_str": "int a = 2; int b = 3;"
        });
        let res = format_tool_args_full(TOOL_REPLACE, &replace_val);
        assert_eq!(res, "cpu/ppc/jit/jit.cpp (replace: 10b -> 21b)");

        let write_val = json!({
            "path": "test.txt",
            "content": "hello world"
        });
        let write_res = format_tool_args_full(TOOL_WRITE_FILE, &write_val);
        assert_eq!(write_res, "test.txt (content: 11 bytes)");
    }
}
