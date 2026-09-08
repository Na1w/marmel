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
                format!("{}…", &cmd[..37])
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
                format!("{}…", &s[..27])
            } else {
                s
            }
        }
    }
}

pub fn format_tool_args_full(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        TOOL_READ_FILE | TOOL_WRITE_FILE | TOOL_REPLACE => {
            if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
                if tool == TOOL_WRITE_FILE || tool == TOOL_REPLACE {
                    let len = args
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map_or(0, str::len);
                    format!("{path} (content: {len} bytes)")
                } else {
                    path.to_string()
                }
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
