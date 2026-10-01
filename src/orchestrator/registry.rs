//! Specialist Registry — maps an `agent_name` role to a concrete worker and
//! its tool namespace (REQ-ORCH-002).
//!
//! The registry is the single authority on which role ids are resolvable at
//! runtime. The Manager resolves every `delegate_task` against it.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::agents::{Agent, Specialist};
use crate::tool_names::TERMINAL_PREFIX;

/// Static descriptor for one registered specialist.
#[derive(Debug, Clone)]
pub struct SpecialistEntry {
    /// The role enum.
    pub agent: Agent,
    /// Source module path (informational; e.g. "src/agents/coder.rs").
    pub module: String,
    /// Allowed tool namespaces (used to build the specialist tool schema and
    /// to gate `delegate_task` dispatch).
    pub tool_namespaces: Vec<String>,
    /// Optional model override for this specialist.
    pub model: Option<String>,
}

/// The runtime registry. The Manager owns this and uses it to resolve every
/// `delegate_task`. `Arc` entries let shared workers and the dispatcher look up
/// without cloning.
#[derive(Debug, Clone, Default)]
pub struct SpecialistRegistry {
    entries: BTreeMap<Agent, Arc<SpecialistEntry>>,
}

impl SpecialistRegistry {
    /// Build the canonical registry (REQ-ORCH-002). The five canonical roles
    /// MUST be present; the Manager validates against this set.
    pub fn canonical() -> Self {
        let mut reg = Self::default();
        let canonical_specs: [(Agent, &str); 6] = [
            (Agent::Coder, "src/agents/coder.rs"),
            (Agent::Researcher, "src/agents/researcher.rs"),
            (Agent::Debugger, "src/agents/debugger.rs"),
            (Agent::Validator, "src/agents/validator.rs"),
            (Agent::Generalist, "src/agents/generalist.rs"),
            (Agent::Planner, "src/agents/planner.rs"),
        ];
        for (agent, module) in canonical_specs {
            let worker = reg.worker(agent);
            let tools = worker.tool_namespaces();
            reg.register(agent, module, tools, None);
        }
        reg
    }

    /// Register one specialist (used for the canonical build + tests + extension).
    pub fn register(
        &mut self,
        agent: Agent,
        module: impl Into<String>,
        tools: &[&str],
        model: Option<String>,
    ) {
        self.entries.insert(
            agent,
            Arc::new(SpecialistEntry {
                agent,
                module: module.into(),
                tool_namespaces: tools.iter().map(|s| s.to_string()).collect(),
                model,
            }),
        );
    }

    /// Resolve an `Agent` to its registered entry. Returns `None` when the role
    /// is not registered (the Manager rejects unknown names — the registry is
    /// the authority).
    pub fn resolve(&self, agent: Agent) -> Option<Arc<SpecialistEntry>> {
        self.entries.get(&agent).cloned()
    }

    /// The concrete worker instance for an agent. A well-behaved Manager routes
    /// ONLY to registered roles, so this never panics at runtime.
    pub fn worker(&self, agent: Agent) -> Arc<dyn Specialist> {
        match agent {
            Agent::Coder => Arc::new(crate::agents::coder::Coder),
            Agent::Researcher => Arc::new(crate::agents::researcher::Researcher),
            Agent::Debugger => Arc::new(crate::agents::debugger::Debugger),
            Agent::Validator => Arc::new(crate::agents::validator::Validator),
            Agent::Generalist => Arc::new(crate::agents::generalist::Generalist),
            Agent::Planner => Arc::new(crate::agents::planner::Planner),
        }
    }

    /// All registered role ids (used for the `delegate_task` JSON `enum` list).
    pub fn agent_ids(&self) -> Vec<String> {
        self.entries
            .keys()
            .map(|a| a.as_str().to_string())
            .collect()
    }
}

impl SpecialistEntry {
    /// Whether this specialist is allowed to call a given tool, per its
    /// allowlist (REQ-ORCH-002). A `"*"` namespace grants every tool. Namespace
    /// prefixes match by `<ns>*` (e.g. `terminal__*`); a bare exact tool name
    /// matches directly.
    pub fn allows(&self, tool: &str) -> bool {
        let bare = tool.strip_prefix(TERMINAL_PREFIX).unwrap_or(tool);
        self.tool_namespaces.iter().any(|ns| {
            let ns_bare = ns.strip_prefix(TERMINAL_PREFIX).unwrap_or(ns);
            if ns == "*" || ns_bare == "*" {
                return true;
            }
            if let Some(p) = ns.strip_suffix('*')
                && (tool.starts_with(p)
                    || bare.starts_with(p.strip_prefix(TERMINAL_PREFIX).unwrap_or(p)))
            {
                return true;
            }
            ns == tool || ns_bare == bare || ns == bare || ns_bare == tool
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_names::{
        TERMINAL_RUN_COMMAND, TOOL_DELEGATE_TASK, TOOL_PTY_READ, TOOL_PTY_SPAWN, TOOL_READ_FILE,
        TOOL_REPLACE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
    };

    #[test]
    fn test_orchestr_registry_every_role_resolves() {
        let reg = SpecialistRegistry::canonical();
        for role in [
            Agent::Coder,
            Agent::Researcher,
            Agent::Debugger,
            Agent::Validator,
            Agent::Generalist,
            Agent::Planner,
        ] {
            let entry = reg
                .resolve(role)
                .unwrap_or_else(|| panic!("{} must be registered", role));
            assert!(!entry.module.is_empty(), "{} has a module path", role);
            assert!(
                !entry.tool_namespaces.is_empty(),
                "{} has tool namespaces",
                role
            );
            // The worker must construct cleanly.
            let worker = reg.worker(role);
            assert_eq!(worker.name(), role);

            // REQ-ORCH-002: the registry entry's allowlist MUST be the SAME as
            // the worker's declared `tool_namespaces()` — the registry is the
            // single authority the Manager routes against, so a divergence would
            // let a dispatched worker use tools the registry would gate.
            let worker_ns: Vec<&str> = worker.tool_namespaces().to_vec();
            let entry_ns: Vec<&str> = entry.tool_namespaces.iter().map(|s| s.as_str()).collect();
            assert_eq!(
                entry_ns, worker_ns,
                "registry allowlist must match worker namespace for {}",
                role
            );
        }
        assert_eq!(reg.agent_ids().len(), 6);
    }

    /// REQ-ORCH-002: `SpecialistEntry::allows` honors the tool allowlist —
    /// matching bare and namespaced tools, and `*` grants everything.
    #[test]
    fn test_orchestr_registry_allowlist_enforces_namespace() {
        let reg = SpecialistRegistry::canonical();
        let coder = reg.resolve(Agent::Coder).unwrap();
        assert!(coder.allows(TOOL_RUN_COMMAND), "run_command matches");
        assert!(coder.allows(TERMINAL_RUN_COMMAND), "terminal__* matches");
        assert!(coder.allows(TOOL_WRITE_FILE), "write_file matches");
        assert!(coder.allows(TOOL_DELEGATE_TASK), "exact tool name matches");
        assert!(
            !coder.allows("unknown_phantom_tool"),
            "unregistered tools are forbidden to the Coder"
        );

        // Generalist has the universal `*` namespace.
        let brain = reg.resolve(Agent::Generalist).unwrap();
        assert!(brain.allows("anything_at_all"));

        // Validator allowlist checks (per AGENTS.md, Validator allows run_command for running test suites).
        let validator = reg.resolve(Agent::Validator).unwrap();
        assert!(validator.allows(TOOL_READ_FILE));
        assert!(validator.allows(TOOL_PTY_SPAWN));
        assert!(validator.allows(TOOL_PTY_READ));
        assert!(validator.allows(TOOL_RUN_COMMAND));
        assert!(!validator.allows(TOOL_WRITE_FILE));
        assert!(!validator.allows(TOOL_REPLACE));
        assert!(!validator.allows("bogus_tool"));
    }

    #[test]
    fn test_orchestr_registry_unknown_role_unresolved() {
        let reg = SpecialistRegistry::default();
        assert!(reg.resolve(Agent::Coder).is_none());
    }
}
