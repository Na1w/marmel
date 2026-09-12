//! Specialist: **Validator** — Independent Quality Auditor.
//!
//! Domain (REQ-ORCH-002): verifies accuracy/logic/fact-checking; inspects
//! deliverables and source files; issues formal verdicts. Rejects
//! incomplete work with actionable feedback; never inflates pass status.
//!
//! Allowed tool namespaces: `delegate_task`, `read_file`, `grep_search`,
//! `glob`, `pty_*`, `leave_verdict`, `rebirth`, `sleep`.

use crate::agents::{Agent, Specialist};
use async_trait::async_trait;

/// Independent Quality Auditor — verification, inspection, formal verdicts.
#[derive(Debug, Default)]
pub struct Validator;

/// Role system prompt for Validator — statically embedded at compile time from prompts/validator.md.
pub const VALIDATOR_ROLE_PROMPT: &str = include_str!("../../prompts/validator.md");
/// Specialist prompt for code auditing — statically embedded from prompts/validator_coder.md.
pub const VALIDATOR_CODER_ROLE_PROMPT: &str = include_str!("../../prompts/validator_coder.md");
/// Specialist prompt for debugger auditing — statically embedded from prompts/validator_debugger.md.
pub const VALIDATOR_DEBUGGER_ROLE_PROMPT: &str =
    include_str!("../../prompts/validator_debugger.md");
/// Specialist prompt for researcher auditing — statically embedded from prompts/validator_researcher.md.
pub const VALIDATOR_RESEARCHER_ROLE_PROMPT: &str =
    include_str!("../../prompts/validator_researcher.md");
/// Specialist prompt for generalist auditing — statically embedded from prompts/validator_generalist.md.
pub const VALIDATOR_GENERALIST_ROLE_PROMPT: &str =
    include_str!("../../prompts/validator_generalist.md");
/// Specialist prompt for planner auditing — statically embedded from prompts/validator_planner.md.
pub const VALIDATOR_PLANNER_ROLE_PROMPT: &str = include_str!("../../prompts/validator_planner.md");

#[async_trait]
impl Specialist for Validator {
    fn name(&self) -> Agent {
        Agent::Validator
    }

    fn tool_namespaces(&self) -> &[&'static str] {
        &[
            crate::tool_names::TOOL_DELEGATE_TASK,
            crate::tool_names::TOOL_READ_FILE,
            crate::tool_names::TOOL_GREP_SEARCH,
            crate::tool_names::TOOL_GLOB,
            crate::tool_names::TOOL_PTY_SPAWN,
            crate::tool_names::TOOL_PTY_WRITE,
            crate::tool_names::TOOL_PTY_READ,
            crate::tool_names::TOOL_PTY_CLOSE,
            crate::tool_names::TOOL_PTY_LIST,
            "pty__*",
            "pty_*",
            crate::tool_names::TOOL_LEAVE_VERDICT,
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
    fn test_orchestr_validator_role_and_namespaces() {
        let v = Validator;
        assert_eq!(v.name(), Agent::Validator);
        assert!(v.tool_namespaces().contains(&"read_file"));
        assert!(!v.tool_namespaces().contains(&"run_command"));
        assert!(v.tool_namespaces().contains(&"grep_search"));
        assert!(v.tool_namespaces().contains(&"glob"));
        assert!(v.tool_namespaces().contains(&"pty_spawn"));
        assert!(v.tool_namespaces().contains(&"pty_read"));
        assert!(v.tool_namespaces().contains(&"leave_verdict"));
        assert!(v.tool_namespaces().contains(&"rebirth"));
        assert!(!v.tool_namespaces().contains(&"write_file"));
        assert!(!v.tool_namespaces().contains(&"replace"));
        assert!(!v.may_recurse());
    }
}
