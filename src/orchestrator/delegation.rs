//! Delegation value types: runtime config, lifecycle events, recursion depth.

use crate::agents::{Agent, DelegationRequest, Deliverable};
use crate::config::Config;

/// Default fractal recursion bound (REQ-ORCH-001).
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 3;

/// Runtime orchestration configuration.
///
/// Hydrated from the `[orchestration]` TOML block via [`OrchestrationConfig::from_config`].
/// It carries the fractal recursion bound, the Manager module path, and the
/// per-specialist tool-allowlist table that the [`SpecialistRegistry`] is built from.
#[derive(Debug, Clone, Default)]
pub struct OrchestrationConfig {
    /// Fractal delegation depth bound. Default 3.
    pub max_recursion_depth: usize,
    /// The Manager module path (e.g. `src/orchestrator/mod.rs`).
    pub manager_module: String,
    /// Specialists table: role id -> allowed tool namespaces.
    pub specialists: std::collections::BTreeMap<String, Vec<String>>,
}

impl OrchestrationConfig {
    /// Default orchestration configuration with the canonical recursion bound.
    pub fn default_depth() -> Self {
        Self {
            max_recursion_depth: DEFAULT_MAX_RECURSION_DEPTH,
            manager_module: "src/orchestrator/mod.rs".to_string(),
            specialists: std::collections::BTreeMap::new(),
        }
    }

    /// Hydrate the runtime orchestration config from the loaded [`Config`].
    pub fn from_config(cfg: &Config) -> Self {
        let src = &cfg.orchestration;
        Self {
            max_recursion_depth: if src.max_recursion_depth == 0 {
                DEFAULT_MAX_RECURSION_DEPTH
            } else {
                src.max_recursion_depth
            },
            manager_module: if src.manager_module.is_empty() {
                "src/orchestrator/mod.rs".to_string()
            } else {
                src.manager_module.clone()
            },
            specialists: src
                .specialists
                .iter()
                .map(|(k, v)| (k.clone(), v.tools.clone()))
                .collect(),
        }
    }
}

/// A delegation lifecycle event surfaced to the UI.
///
/// The [`OrchestratorManager`] emits these as it routes work to specialists so
/// the TUI / raw renderers can show which specialist is active and on which
/// task. The Manager never performs domain work; it only reports it.
#[derive(Debug, Clone)]
pub enum DelegationEvent {
    /// A specialist has been dispatched to work on a task.
    Started { agent: Agent, task: Option<String> },
    /// A specialist has returned its deliverable.
    Completed { agent: Agent, task: Option<String> },
    /// A specialist failed to complete its task.
    Failed { agent: Agent, task: Option<String> },
}

/// A depth counter passed down a delegation chain. The Manager is depth 0; each
/// nested `delegate_task` increments it; a call that would exceed the bound is
/// rejected (REQ-ORCH-001 / REQ-ORCH-003 fractal isolation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecursionDepth(pub usize);

impl RecursionDepth {
    /// The root depth (the Manager).
    pub fn root() -> Self {
        RecursionDepth(0)
    }

    /// Increment toward a bound. Returns `None` when `depth+1` would exceed
    /// `max` (i.e. the nested delegation must be rejected).
    pub fn step(self, max: usize) -> Option<RecursionDepth> {
        if self.0 < max {
            Some(RecursionDepth(self.0 + 1))
        } else {
            None
        }
    }
}

/// A single delegation routed to a worker (used by the Silent Dispatcher to
/// track in-flight / returned subtasks).
#[derive(Debug, Clone)]
pub struct Delegation {
    pub agent: Agent,
    pub request: DelegationRequest,
    /// The depth at which this delegation is executing.
    pub depth: RecursionDepth,
    pub result: Option<Deliverable>,
}
