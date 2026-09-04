//! Specialist: **Coder** — Lead Software Engineer.
//!
//! Domain (REQ-ORCH-002): system architecture, refactoring, and code
//! implementation. Forbidden from genealogy / kinship.
//!
//! Allowed tool namespaces: `delegate_task`, `cli_*`, `terminal__*`,
//! `kiwix__*`, `pdf__*`, `memory__*`, `puppeteer__*`.

use crate::agents::{Agent, Specialist};
use async_trait::async_trait;

/// Lead Software Engineer — architecture, refactoring, code implementation.
#[derive(Debug, Default)]
pub struct Coder;

/// Role system prompt that fixes this specialist's role and tool set
/// (REQ-ORCH-002 role discipline) — statically embedded at compile time from prompts/coder.md.
pub const CODER_ROLE_PROMPT: &str = include_str!("../../prompts/coder.md");

#[async_trait]
impl Specialist for Coder {
    fn name(&self) -> Agent {
        Agent::Coder
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
            "rebirth",
        ]
    }

    /// The coder may sub-delegate to generalist/validator for dense reasoning or
    /// independent verification of its own output.
    fn may_recurse(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orchestr_coder_role_and_namespaces() {
        let c = Coder;
        assert_eq!(c.name(), Agent::Coder);
        assert!(c.tool_namespaces().contains(&"write_file"));
        assert!(c.tool_namespaces().contains(&"rebirth"));
        assert!(c.may_recurse());
    }
}
