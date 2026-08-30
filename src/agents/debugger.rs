//! Specialist: **Debugger** — Low-Level Systems Debugger.
//!
//! Domain (REQ-ORCH-002): crash forensics, interactive PTY GDB/LLDB, Capstone
//! disassembly, ABI/codegen root-cause analysis.
//!
//! Allowed tool namespaces: `delegate_task`, `terminal__*`, `pty__*`,
//! `kiwix__*`, `memory__*`.

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
            "delegate_task",
            "write_file",
            "replace",
            "read_file",
            "run_command",
            "grep_search",
            "glob",
            "pty_spawn",
            "pty_write",
            "pty_read",
            "pty_close",
            "pty_list",
            "pty__*",
            "pty_*",
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
        assert!(!d.may_recurse());
    }
}
