//! Delegation value types: runtime config, lifecycle events, recursion depth.

use crate::agents::{Agent, DelegationRequest, Deliverable};
use crate::config::Config;
use std::sync::Arc;

/// Default fractal recursion bound.
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
///
/// The authoritative sequence per delegation is
/// `Started -> Queued* -> Running -> Completed | Failed`: the `Queued` /
/// `Running` pair is the concurrency-slot state inside an already-dispatched
/// delegation and never replaces or reorders the bracketing lifecycle arms.
#[derive(Debug, Clone)]
pub enum DelegationEvent {
    /// A specialist has been dispatched to work on a task.
    Started { agent: Agent, task: Option<String> },
    /// A backend call for this delegation is waiting in line for a concurrency
    /// slot (`[orchestration.concurrency] max_sessions`). Orthogonal to the
    /// lifecycle arms: a queued delegation is still dispatched, still visible,
    /// and still **active** — only its *slot state* changes.
    ///
    /// `position` is the delegation's 1-based rank in the key's FIFO and `of`
    /// is the queue depth including this caller; both come from the pool's
    /// [`crate::orchestrator::pool::QueueTicket`], never from map iteration
    /// order.
    Queued {
        agent: Agent,
        task: Option<String>,
        position: usize,
        of: usize,
    },
    /// A delegation that had been queued now holds a slot and is running.
    /// Emitted only after a [`DelegationEvent::Queued`] for the same call, so
    /// the queue state an earlier event installed is always taken back down.
    Running { agent: Agent, task: Option<String> },
    /// A specialist has returned its deliverable.
    Completed { agent: Agent, task: Option<String> },
    /// A specialist failed to complete its task.
    Failed {
        agent: Agent,
        task: Option<String>,
        reason: Option<String>,
    },
}

impl DelegationEvent {
    /// The agent this event belongs to.
    pub fn agent(&self) -> Agent {
        match self {
            DelegationEvent::Started { agent, .. }
            | DelegationEvent::Queued { agent, .. }
            | DelegationEvent::Running { agent, .. }
            | DelegationEvent::Completed { agent, .. }
            | DelegationEvent::Failed { agent, .. } => *agent,
        }
    }

    /// The plan task id this event belongs to, when one is known.
    pub fn task(&self) -> Option<&str> {
        match self {
            DelegationEvent::Started { task, .. }
            | DelegationEvent::Queued { task, .. }
            | DelegationEvent::Running { task, .. }
            | DelegationEvent::Completed { task, .. }
            | DelegationEvent::Failed { task, .. } => task.as_deref(),
        }
    }
}

/// The delegation event queue the UI is currently draining.
///
/// The [`OrchestratorManager`] vector is the single authoritative lifecycle
/// stream (design §7), and only the session loop drains it. Deep call paths —
/// the concurrency gate in `ChatClient::acquire_slot` — cannot reach the
/// manager through their own signatures, so the drain registers this vector as
/// the process-wide sink and those paths append through
/// [`push_delegation_event`]. Appending to the *same* vector is what keeps the
/// authoritative sequence honest (`Started -> Queued* -> Running ->
/// Completed|Failed`): a queue event can never be observed out of order with
/// the `Started`/`Completed` that brackets it.
///
/// Swap pattern (same shape as [`crate::orchestrator::bus::set_event_sender`]):
/// the guard is cloned out under the read lock and released before any other
/// lock is touched.
static DELEGATION_EVENT_SINK: std::sync::RwLock<
    Option<Arc<std::sync::Mutex<Vec<DelegationEvent>>>>,
> = std::sync::RwLock::new(None);

/// Register the delegation event queue the session loop is draining.
pub fn set_delegation_event_sink(queue: Arc<std::sync::Mutex<Vec<DelegationEvent>>>) {
    if let Ok(mut lock) = DELEGATION_EVENT_SINK.write() {
        *lock = Some(queue);
    }
}

/// The queue currently registered, if any.
pub fn delegation_event_sink() -> Option<Arc<std::sync::Mutex<Vec<DelegationEvent>>>> {
    DELEGATION_EVENT_SINK
        .read()
        .ok()
        .and_then(|guard| guard.clone())
}

/// Append one event to the active delegation stream.
///
/// Returns `false` when no session is draining a stream (headless runs, unit
/// tests without a UI), which is a no-op rather than an error: queue traffic is
/// presentation state, never control flow.
pub fn push_delegation_event(event: DelegationEvent) -> bool {
    let Some(queue) = delegation_event_sink() else {
        return false;
    };
    // The drain holds this lock only for in-memory work (folding panel state and
    // handing the event to the renderer), so this brief wait can never sit
    // behind I/O or an LLM call.
    match queue.lock() {
        Ok(mut events) => {
            events.push(event);
            true
        }
        Err(_) => false,
    }
}

/// Drop the registered sink (test isolation, and the honest teardown for a
/// session whose queue this sink was registered from: the sink is
/// process-global, so a finished session must not leave a dangling queue
/// behind for unrelated later calls to append to).
pub fn clear_delegation_event_sink() {
    if let Ok(mut lock) = DELEGATION_EVENT_SINK.write() {
        *lock = None;
    }
}

/// The delegation a metered backend call belongs to, when the UI tracks one.
///
/// Only a [`CallClass::Specialist`] turn has a delegation row of its own: that
/// is the row `delegate()` brackets with `Started`/`Completed`, so it is the
/// only identity a `Queued`/`Running` transition can be attached to without
/// inventing a panel row that no lifecycle event would ever close (a validator
/// pass, a JIT blueprint, a manager turn, a steer arbitration or a notice
/// evaluation have no `delegate_task` lifecycle of their own, and their queue
/// state is already covered by status traffic).
///
/// `None` also covers a specialist context whose agent label is not a canonical
/// role id, so no event is emitted on partial attribution.
pub fn delegation_identity(
    context: &crate::orchestrator::pool::CallContext,
) -> Option<(Agent, Option<String>)> {
    if context.class != crate::orchestrator::pool::CallClass::Specialist {
        return None;
    }
    let agent = Agent::from_str(context.agent.as_deref()?)?;
    Some((agent, context.task.clone()))
}

/// A depth counter passed down a delegation chain. The Manager is depth 0; each
/// nested `delegate_task` increments it; a call that would exceed the bound is
/// rejected before any worker is spawned, so delegation stays bounded and each
/// level keeps its own isolated context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecursionDepth(pub usize);

impl RecursionDepth {
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
    pub depth: RecursionDepth,
    pub result: Option<Deliverable>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::pool::{CallClass, CallContext, MANAGER_AGENT};

    /// Only a specialist turn has a delegation row of its own, so only a
    /// specialist turn can carry a queue transition. Every other class must
    /// resolve to `None`, or the UI would gain a panel row no lifecycle event
    /// ever closes.
    #[test]
    fn delegation_identity_resolves_only_specialist_turns_with_a_canonical_agent() {
        let specialist =
            CallContext::for_agent(CallClass::Specialist, "coder", Some("t-011".to_string()));
        assert_eq!(
            delegation_identity(&specialist),
            Some((Agent::Coder, Some("t-011".to_string())))
        );

        // A specialist label that is not a canonical role id is partial
        // attribution: no event rather than a row the panel cannot name.
        let odd = CallContext::for_agent(CallClass::Specialist, "coder-senior", None);
        assert_eq!(delegation_identity(&odd), None);

        // A specialist context with no agent label at all.
        let bare = CallContext::new(CallClass::Specialist);
        assert_eq!(delegation_identity(&bare), None);

        // Every non-specialist class, including the one whose label *would*
        // resolve (`validator` is a real role) and the reserve classes.
        let untracked = [
            CallContext::for_agent(CallClass::ManagerTurn, MANAGER_AGENT, None),
            CallContext::for_agent(
                CallClass::Validator,
                "validator-coder",
                Some("t-011".to_string()),
            ),
            CallContext::for_agent(CallClass::Validator, "validator", Some("t-011".to_string())),
            CallContext::for_agent(CallClass::PromptArchitect, "coder", None),
            CallContext::for_agent(
                CallClass::SteerArbitrator,
                "coder",
                Some("t-011".to_string()),
            ),
            CallContext::for_agent(CallClass::NoticeEval, "coder", Some("t-011".to_string())),
        ];
        for context in untracked {
            assert_eq!(
                delegation_identity(&context),
                None,
                "class {:?} must never produce a delegation queue event",
                context.class
            );
        }

        // Nesting keeps attribution but changes metering: the class the call is
        // metered as decides, not the agent label.
        let nested = specialist.with_class(CallClass::ManagerTurn);
        assert_eq!(delegation_identity(&nested), None);
    }

    /// Every arm answers `agent()` / `task()`, so callers that route an event to
    /// a panel row never have to match on the arm.
    #[test]
    fn agent_and_task_accessors_cover_every_arm() {
        let events = [
            DelegationEvent::Started {
                agent: Agent::Coder,
                task: Some("t-1".to_string()),
            },
            DelegationEvent::Queued {
                agent: Agent::Researcher,
                task: None,
                position: 1,
                of: 2,
            },
            DelegationEvent::Running {
                agent: Agent::Debugger,
                task: Some("t-3".to_string()),
            },
            DelegationEvent::Completed {
                agent: Agent::Validator,
                task: None,
            },
            DelegationEvent::Failed {
                agent: Agent::Planner,
                task: Some("t-5".to_string()),
                reason: None,
            },
        ];
        assert_eq!(
            events.iter().map(|event| event.agent()).collect::<Vec<_>>(),
            vec![
                Agent::Coder,
                Agent::Researcher,
                Agent::Debugger,
                Agent::Validator,
                Agent::Planner
            ]
        );
        assert_eq!(
            events.iter().map(|event| event.task()).collect::<Vec<_>>(),
            vec![Some("t-1"), None, Some("t-3"), None, Some("t-5")]
        );
    }

    // The sink itself (`set_delegation_event_sink` / `push_delegation_event`) is
    // process-global, so it is exercised end-to-end in
    // `tests/test_delegation_queue_events.rs`, where a test-local mutex owns the
    // whole binary — the library test binary runs UI drain tests concurrently,
    // and every drain re-registers the sink, so a unit test here could never
    // assert exclusivity.
}
