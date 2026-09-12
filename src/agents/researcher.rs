//! Specialist: **Researcher** — Deep Knowledge & Research Specialist.
//!
//! Domain (REQ-ORCH-002): comprehensive codebase exploration, information
//! retrieval, architecture analysis, and documentation.
//!
//! Allowed tool namespaces: `delegate_task`, `write_file`, `replace`,
//! `read_file`, `run_command`, `grep_search`, `glob`, `rebirth`, `sleep`.

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
            crate::tool_names::TOOL_DELEGATE_TASK,
            crate::tool_names::TOOL_WRITE_FILE,
            crate::tool_names::TOOL_REPLACE,
            crate::tool_names::TOOL_READ_FILE,
            crate::tool_names::TOOL_RUN_COMMAND,
            crate::tool_names::TOOL_GREP_SEARCH,
            crate::tool_names::TOOL_GLOB,
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
    fn test_orchestr_researcher_role_and_namespaces() {
        let r = Researcher;
        assert_eq!(r.name(), Agent::Researcher);
        assert!(r.tool_namespaces().contains(&"read_file"));
        assert!(r.tool_namespaces().contains(&"rebirth"));
        assert!(!r.may_recurse());
    }
}
