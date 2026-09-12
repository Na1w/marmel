//! Specialist: **Debugger** — Low-Level Systems Debugger.
//!
//! Domain (REQ-ORCH-002): crash forensics, interactive PTY execution,
//! diagnostics, and root-cause analysis.
//!
//! Allowed tool namespaces: `delegate_task`, `write_file`, `replace`,
//! `read_file`, `run_command`, `grep_search`, `glob`, `pty_*`, `rebirth`, `sleep`.

use crate::agents::{Agent, Specialist};
use async_trait::async_trait;

/// Low-Level Systems Debugger — crash forensics, PTY GDB/LLDB, ABI/codegen.
#[derive(Debug, Default)]
pub struct Debugger;

/// Role system prompt for Debugger — statically embedded at compile time from prompts/debugger.md.
pub const DEBUGGER_ROLE_PROMPT: &str = include_str!("../../prompts/debugger.md");

#[async_trait]
impl Specialist for Debugger {
    fn name(&self) -> Agent {
        Agent::Debugger
    }

    fn tool_namespaces(&self) -> &[&'static str] {
        &[
            crate::tool_names::TOOL_DELEGATE_TASK,
            crate::tool_names::TOOL_WRITE_FILE,
            crate::tool_names::TOOL_REPLACE,
            crate::tool_names::TOOL_READ_FILE,
            crate::tool_names::TOOL_RUN_COMMAND,
            crate::tool_names::TOOL_GREP_SEARCH,
            crate::tool_names::TOOL_GLOB,
            crate::tool_names::TOOL_PTY_SPAWN,
            crate::tool_names::TOOL_PTY_WRITE,
            crate::tool_names::TOOL_PTY_READ,
            crate::tool_names::TOOL_PTY_CLOSE,
            crate::tool_names::TOOL_PTY_LIST,
            "pty__*",
            "pty_*",
            crate::tool_names::TOOL_REBIRTH,
            crate::tool_names::TOOL_SLEEP,
            crate::tool_names::TERMINAL_SLEEP,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orchestr_debugger_role_and_namespaces() {
        let d = Debugger;
        assert_eq!(d.name(), Agent::Debugger);
        assert!(d.tool_namespaces().contains(&"run_command"));
        assert!(d.tool_namespaces().contains(&"rebirth"));
        assert!(!d.may_recurse());
    }
}
