//! Named constants for the wire-visible tool names used across the codebase.
//!
//! Centralizing tool names as `pub const` strings prevents typos and keeps the
//! canonical set of tool names in a single, auditable location. Every module
//! that matches on, dispatches, or documents a tool name should reference these
//! constants rather than hardcoding the string literal, so the values stay in
//! lockstep (e.g. the harness dispatcher, the specialist `tool_namespaces`
//! allowlists, and the registry all agree).

/// `delegate_task` — dispatch a bounded unit of domain work to a specialist
/// subagent (REQ-ORCH-005).
pub const TOOL_DELEGATE_TASK: &str = "delegate_task";
/// `read_file` — read paginated lines of UTF-8 text from a file.
pub const TOOL_READ_FILE: &str = "read_file";
/// `write_file` — create a new file or completely overwrite an existing file.
pub const TOOL_WRITE_FILE: &str = "write_file";
/// `replace` — replace an exact, unique block of text within a file.
pub const TOOL_REPLACE: &str = "replace";
/// `run_command` — execute a command line inside a dedicated PTY.
pub const TOOL_RUN_COMMAND: &str = "run_command";
/// `grep_search` — search for a regex pattern across workspace files.
pub const TOOL_GREP_SEARCH: &str = "grep_search";
/// `glob` — find files matching a glob pattern.
pub const TOOL_GLOB: &str = "glob";
/// `create_plan` — write or overwrite the workspace execution plan.
pub const TOOL_CREATE_PLAN: &str = "create_plan";
/// `archive_current_plan` — archive the current execution plan to `.marmel/archive/`.
pub const TOOL_ARCHIVE_PLAN: &str = "archive_current_plan";
/// `rebirth` — compact conversation history into a structured checkpoint summary.
pub const TOOL_REBIRTH: &str = "rebirth";
/// `pty_spawn` — spawn an interactive persistent PTY terminal session.
pub const TOOL_PTY_SPAWN: &str = "pty_spawn";
/// `pty_write` — send input text or commands to an active PTY session.
pub const TOOL_PTY_WRITE: &str = "pty_write";
/// `pty_read` — read unread buffer output from an active PTY session.
pub const TOOL_PTY_READ: &str = "pty_read";
/// `pty_close` — close an active PTY session and kill its process group.
pub const TOOL_PTY_CLOSE: &str = "pty_close";
/// `pty_list` — list all active interactive PTY sessions.
pub const TOOL_PTY_LIST: &str = "pty_list";
/// `leave_verdict` — submit a formal validation verdict (APPROVED / REJECTED) with comments.
pub const TOOL_LEAVE_VERDICT: &str = "leave_verdict";

/// `terminal__read_file` — caesar-style namespaced variant of `read_file`.
pub const TERMINAL_READ_FILE: &str = "terminal__read_file";
/// `terminal__write_file` — caesar-style namespaced variant of `write_file`.
pub const TERMINAL_WRITE_FILE: &str = "terminal__write_file";
/// `terminal__replace` — caesar-style namespaced variant of `replace`.
pub const TERMINAL_REPLACE: &str = "terminal__replace";
/// `terminal__run_command` — caesar-style namespaced variant of `run_command`.
pub const TERMINAL_RUN_COMMAND: &str = "terminal__run_command";
/// `terminal__grep_search` — caesar-style namespaced variant of `grep_search`.
pub const TERMINAL_GREP_SEARCH: &str = "terminal__grep_search";
/// `terminal__glob` — caesar-style namespaced variant of `glob`.
pub const TERMINAL_GLOB: &str = "terminal__glob";
/// `terminal__list_directory` — caesar-style namespaced variant of `list_directory`.
pub const TERMINAL_LIST_DIRECTORY: &str = "terminal__list_directory";
