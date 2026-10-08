//! Tool argument formatting for previews and logs.
//!
//! The rendering itself has a single owner: [`crate::tool_args`] (it replaced
//! the twin implementations that used to live here and in `ui::helpers`). These
//! thin wrappers keep the runner-facing names used by `runner::fix_loop` and add
//! no formatting logic of their own — see `crate::tool_args` for the per-tool
//! argument keys and the clipping budgets.

/// Compact bare-value preview of a tool call's arguments (status line text).
pub fn format_tool_args_preview(tool: &str, args: &serde_json::Value) -> String {
    crate::tool_args::preview_tool_args(tool, args)
}

/// Verbose argument rendering used by the `invoking tool:` log line.
pub fn format_tool_args_full(tool: &str, args: &serde_json::Value) -> String {
    crate::tool_args::full_tool_args(tool, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_names::{TOOL_GREP_SEARCH, TOOL_REPLACE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE};
    use serde_json::json;

    #[test]
    fn test_preview_delegates_to_tool_args_with_pattern_key() {
        // The runner preview must see the schema key (regression guard for the
        // empty-preview bug) and must never panic on multi-byte payloads.
        assert_eq!(
            format_tool_args_preview(TOOL_GREP_SEARCH, &json!({ "pattern": "TODO" })),
            "TODO"
        );
        let cmd = format!("{}—cargo test", "b".repeat(36));
        assert!(
            format_tool_args_preview(TOOL_RUN_COMMAND, &json!({ "command": cmd })).ends_with('…')
        );
    }

    #[test]
    fn test_full_delegates_to_tool_args() {
        let replace = json!({
            "path": "cpu/ppc/jit/jit.cpp",
            "old_str": "int a = 1;",
            "new_str": "int a = 2; int b = 3;"
        });
        assert_eq!(
            format_tool_args_full(TOOL_REPLACE, &replace),
            "cpu/ppc/jit/jit.cpp (replace: 10b -> 21b)"
        );
        assert_eq!(
            format_tool_args_full(
                TOOL_WRITE_FILE,
                &json!({ "path": "t.txt", "content": "hello" })
            ),
            "t.txt (content: 5 bytes)"
        );
    }
}
