//! Specialist: **Researcher** — Deep Knowledge & Archival Specialist.
//!
//! Domain (REQ-ORCH-002): exhaustive research — ZIM encyclopedias, PDFs,
//! historical archives, statistics. Must prioritize local ZIM before external
//! APIs and cite verifiable references.
//!
//! Allowed tool namespaces: `delegate_task`, `kiwix__*`, `pdf__*`,
//! `riksarkivet__*`, `terminal__*`, `memory__*`, `scb__*`, `brave_search__*`.

use crate::agents::{Agent, Specialist};
use async_trait::async_trait;

/// Deep Knowledge & Archival — exhaustive research, ZIM/PDF/archives/statistics.
#[derive(Debug, Default)]
pub struct Researcher;

/// Role system prompt for Researcher — statically embedded at compile time from prompts/researcher.md.
pub const RESEARCHER_ROLE_PROMPT: &str = include_str!("../../prompts/researcher.md");

#[async_trait]
impl Specialist for Researcher {
    fn name(&self) -> Agent {
        Agent::Researcher
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
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orchestr_researcher_role_and_namespaces() {
        let r = Researcher;
        assert_eq!(r.name(), Agent::Researcher);
        assert!(r.tool_namespaces().contains(&"read_file"));
        assert!(!r.may_recurse());
    }
}
