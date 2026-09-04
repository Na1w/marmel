//! Specialist: **Validator** — Independent Quality Auditor.
//!
//! Domain (REQ-ORCH-002): verifies accuracy/logic/fact-checking; runs test
//! suites, linters, checks deliverables; issues formal verdicts. Rejects
//! incomplete work with actionable feedback; never inflates pass status.
//!
//! Allowed tool namespaces: `delegate_task`, `terminal__*`, `kiwix__*`,
//! `gedcom__*`, `memory__*`, `leave_verdict`.

use crate::agents::{Agent, Specialist};
use async_trait::async_trait;

/// Independent Quality Auditor — verification, test suites, formal verdicts.
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

#[async_trait]
impl Specialist for Validator {
    fn name(&self) -> Agent {
        Agent::Validator
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
            "leave_verdict",
            "rebirth",
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
        assert!(v.tool_namespaces().contains(&"run_command"));
        assert!(v.tool_namespaces().contains(&"rebirth"));
        assert!(!v.may_recurse());
    }
}
