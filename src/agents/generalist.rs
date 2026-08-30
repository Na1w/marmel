//! Specialist: **Generalist** — Supreme Polymath & Cross-Domain Intelligence.
//!
//! REQ-ORCH-002: granted the universal `"*"` tool namespace, enabling it to
//! orchestrate tools across all available domains (files, commands, PTY, search).

use crate::agents::{Agent, Specialist};
use async_trait::async_trait;

/// Supreme Polymath & Cross-Domain Intelligence.
#[derive(Debug, Default)]
pub struct Generalist;

/// Role system prompt for Generalist — statically embedded at compile time from prompts/generalist.md.
pub const GENERALIST_ROLE_PROMPT: &str = include_str!("../../prompts/generalist.md");

#[async_trait]
impl Specialist for Generalist {
    fn name(&self) -> Agent {
        Agent::Generalist
    }

    fn tool_namespaces(&self) -> &[&'static str] {
        &["*"]
    }

    fn may_recurse(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orchestr_generalist_role_and_namespaces() {
        let g = Generalist;
        assert_eq!(g.name(), Agent::Generalist);
        assert_eq!(g.tool_namespaces(), &["*"]);
        assert!(g.may_recurse());
    }
}
