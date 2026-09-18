//! Specialist: **Planner** — Strategic Planner & Mission Architect.
//!
//! Domain: workspace reconnaissance, dependency-ordered phased planning, and
//! mission decomposition into `.marmel/execution_plan.md`.
//!
//! Allowed tool namespaces: `create_plan`, `read_file`, `grep_search`, `glob`,
//! `rebirth`, `sleep`.

use crate::agents::{Agent, Specialist};
use async_trait::async_trait;

/// Strategic Planner — mission architecture, task decomposition, and execution plan creation.
#[derive(Debug, Default)]
pub struct Planner;

/// Role system prompt statically embedded at compile time from prompts/planner.md.
pub const PLANNER_ROLE_PROMPT: &str = include_str!("../../prompts/planner.md");

#[async_trait]
impl Specialist for Planner {
    fn name(&self) -> Agent {
        Agent::Planner
    }

    fn tool_namespaces(&self) -> &[&'static str] {
        &[
            crate::tool_names::TOOL_CREATE_PLAN,
            crate::tool_names::TOOL_READ_FILE,
            crate::tool_names::TOOL_GREP_SEARCH,
            crate::tool_names::TOOL_GLOB,
            crate::tool_names::TOOL_REBIRTH,
            crate::tool_names::TOOL_SLEEP,
            crate::tool_names::TERMINAL_SLEEP,
        ]
    }

    fn may_recurse(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_planner_role_and_namespaces() {
        let p = Planner;
        assert_eq!(p.name(), Agent::Planner);
        assert!(p.tool_namespaces().contains(&"create_plan"));
        assert!(p.tool_namespaces().contains(&"read_file"));
        assert!(!p.may_recurse());
    }
}
