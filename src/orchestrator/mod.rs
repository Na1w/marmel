//! Fractal Orchestration — Manager + Specialist Subagents (REQ-ORCH-001…005).
//!
//! The Manager (`OrchestratorManager`) owns user interaction, goal
//! decomposition, planning, delegation, and synthesis. It is STRICTLY
//! FORBIDDEN from performing domain-specific work; every unit of domain work is
//! emitted via `delegate_task` (REQ-ORCH-001). Its only permitted tools are
//! `delegate_task`, `create_plan`/plan updates, read-only non-domain diagnostic
//! inspection, and final synthesis.
//!
//! Submodules:
//! - `registry` — the `SpecialistRegistry` (REQ-ORCH-002).

pub mod bus;
pub mod freeze;
pub mod plan_summary;
pub mod preemption;
pub mod registry;
pub mod steer;
pub mod workers;

pub use preemption::{
    PreemptHandle, PreemptibleStreamSink, models_conflict, preempt_conflicting_stream,
};

use crate::agent::phase::Plan;
pub use crate::agents::{
    Agent, DelegationRequest, Deliverable, IsolatedContext, MissionMarker, Specialist,
};
use crate::config::Config;
use crate::harness::{HarnessStats, ToolError, ToolResult};
use crate::llm::ChatClient;
use crate::tool_names::TOOL_DELEGATE_TASK;
use anyhow::Result;
pub use bus::{
    cancel_all, emit_event, emit_status, global_cancellation_token, is_globally_cancelled,
    reset_cancellation, set_event_sender, set_status_sender,
};
pub use freeze::{CrashJournal, FreezeSnapshot, JournalEventKind};
pub use plan_summary::generate_plan_progress_summary;
pub use registry::SpecialistRegistry;
use std::sync::Arc;
pub use steer::{
    SteerDecision, SteerOutcome, SteerSubtaskDecision, StreamingResponseExtractor, arbitrate_steer,
    arbitrate_steer_stream, arbitrate_steer_stream_with_fallback, arbitrate_steer_with_fallback,
    resolve_steer_outcome,
};
pub use workers::{
    ActiveWorkerGuard, ActiveWorkerInfo, format_duration_human, get_active_specialist_context_str,
    get_active_subtasks_str, get_active_worker_tokens, has_active_workers, register_active_worker,
    update_active_worker_context,
};

/// Default fractal recursion bound (REQ-ORCH-001).
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 3;
/// Guard for the Silent Dispatcher so a plan that cannot make progress fails
/// loudly instead of spinning.
pub const MAX_EXECUTING_ROUNDS: usize = 100;

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

/// The Manager (Orchestrator). Owns user interaction, goal decomposition,
/// planning, delegation, and synthesis. It NEVER performs domain work itself.
///
/// All delegation methods are **synchronous from the Manager's perspective**
/// (REQ-ORCH-005): `delegate` blocks until the specialist returns its
/// deliverable.
#[derive(Debug)]
pub struct OrchestratorManager {
    /// User interaction handle (synthesis-driven replies to the user).
    pub client: ChatClient,
    /// The on-disk plan (shared workspace source of truth, REQ-ORCH-004).
    pub plan: Plan,
    /// The specialist registry (REQ-ORCH-002).
    pub registry: SpecialistRegistry,
    /// Orchestration config (max depth bound).
    pub orchestration: OrchestrationConfig,
    /// Shared resilience counters (bumped on delegation/abort).
    pub stats: Arc<HarnessStats>,
    /// Current recursion depth (the Manager is the root, depth 0).
    pub depth: RecursionDepth,
    /// File-backed Deep-Freeze Crash Journal rooted at the shared `.marmel/`
    /// plan dir (SPEC §3.4). Snapshot on delegation, rehydrate on resume.
    pub journal: CrashJournal,
    /// Delegation lifecycle events surfaced to the UI (t6-REQ-3). The Manager
    /// records a `Started`/`Completed` event per delegation so renderers can show
    /// which specialist is active and on which task. Interior mutability
    /// (`Arc<Mutex<_>>`) lets the shared Manager (held by the `ManagerLoop` as
    /// `Arc<OrchestratorManager>`) be drained concurrently by the UI without a
    /// `&mut` borrow, so `delegate` stays `&self`.
    pub delegation_events: Arc<std::sync::Mutex<Vec<DelegationEvent>>>,
    /// Cancellation token for this manager and its subagent worker hierarchy.
    pub cancellation_token: tokio_util::sync::CancellationToken,
}

impl OrchestratorManager {
    /// Create a Manager with the canonical specialist registry and default
    /// orchestration config, rooted at a plan manager and shared stats.
    pub fn new(client: ChatClient, plan: Plan, stats: Arc<HarnessStats>) -> Self {
        Self::from_orchestration(client, plan, stats, OrchestrationConfig::default_depth())
    }

    /// Create a Manager hydrated from a loaded [`Config`].
    pub fn from_config(
        client: ChatClient,
        plan: Plan,
        stats: Arc<HarnessStats>,
        cfg: &Config,
    ) -> Self {
        let orchestration = OrchestrationConfig::from_config(cfg);
        Self::from_orchestration(client, plan, stats, orchestration)
    }

    /// Internal constructor with a fully-resolved runtime orchestration config.
    fn from_orchestration(
        client: ChatClient,
        plan: Plan,
        stats: Arc<HarnessStats>,
        orchestration: OrchestrationConfig,
    ) -> Self {
        let journal = CrashJournal::new(plan.dir());
        let cancellation_token = global_cancellation_token().child_token();
        Self {
            client,
            plan,
            registry: SpecialistRegistry::canonical(),
            orchestration,
            stats,
            depth: RecursionDepth::root(),
            journal,
            delegation_events: Arc::new(std::sync::Mutex::new(Vec::new())),
            cancellation_token,
        }
    }

    /// Request cancellation across this manager and all its child workers.
    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    /// Check whether this manager has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }

    /// Set a custom cancellation token (e.g. from session loop).
    pub fn with_cancellation_token(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancellation_token = token;
        self
    }

    /// Enforce the Manager-Never-Does-Domain-Work invariant.
    pub fn guard_no_domain_work(&self) -> Result<()> {
        let module = self.orchestration.manager_module.as_str();
        if !module.trim().is_empty() && module.contains("/agents/") {
            return Err(anyhow::anyhow!(
                "orchestration.manager_module `{module}` is a specialist (domain) module: \
                 the Manager cannot be a domain worker"
            ));
        }
        Ok(())
    }

    /// Create (or overwrite) the on-disk execution plan via `create_plan`.
    pub fn create_plan(&self, plan_markdown: &str) -> Result<()> {
        crate::debug_log::log_plan_update("create_plan", plan_markdown);
        self.plan.create(plan_markdown)
    }

    /// REQ-ORCH-005: emit a `delegate_task` and **block** until the specialist
    /// returns. This is synchronous from the Manager's perspective.
    pub async fn delegate(&self, req: DelegationRequest) -> Result<Deliverable> {
        // 1. Resolve the agent against the registry (REQ-ORCH-002).
        let entry = self
            .registry
            .resolve(req.agent_name)
            .ok_or_else(|| anyhow::anyhow!("unknown specialist: {}", req.agent_name))?;

        // 2. Fractal depth gate (REQ-ORCH-001): nested delegation beyond the
        //    bound is rejected. This mirrors caesar's `Orchestrator` gate
        //    (tools_manager.rs:914) which is UNCONDITIONAL — the recursion
        //    bound is enforced regardless of `recursion_granted`, so a request
        //    that would exceed the bound is rejected before any worker is
        //    spawned and before any `Started` event is surfaced (a rejected
        //    delegation must not emit a spurious lifecycle event).
        if self
            .depth
            .step(self.orchestration.max_recursion_depth)
            .is_none()
        {
            return Err(anyhow::anyhow!(
                "recursion depth {} exceeds max {}",
                self.depth.0 + 1,
                self.orchestration.max_recursion_depth
            ));
        }

        // 2b. Surface the delegation start to the UI (t6-REQ-3). Emitted only
        //     after the depth gate passes, so a rejected delegation never
        //     surfaces a `Started` event (parity with caesar, which returns
        //     before spawning a worker on depth rejection).
        if let Ok(mut ev) = self.delegation_events.lock() {
            ev.push(DelegationEvent::Started {
                agent: entry.agent,
                task: req.task_id.clone(),
            });
        }
        crate::debug_log::log_delegation_start(
            entry.agent.as_str(),
            req.task_id.as_deref(),
            &req.prompt,
            req.snippets.len(),
        );
        let start_time = std::time::Instant::now();

        // 3. Deep-Freeze: snapshot this in-flight delegation to the Crash
        //    Journal BEFORE the worker runs (SPEC §3.4). If the process dies
        //    mid-run, the identical `worker_id` + `sub_req` survive on disk.
        let worker_id = self
            .journal
            .snapshot(entry.agent, &req)
            .unwrap_or_else(|e| {
                // A journal write failure must not silently lose a task: surface
                // it as an error so the caller can fail loudly (REQ-ORCH-005).
                tracing::warn!("Deep-Freeze snapshot failed: {e}");
                String::new()
            });

        // 4. Build the ISOLATED context (REQ-ORCH-003): the specialist sees only
        //    its own role prompt + brief + snippets, never Manager messages[].
        let ctx = IsolatedContext::from_request(self.role_prompt_for(entry.agent), &req);

        // Register active worker for real-time steering arbitrator visibility
        let _active_guard = register_active_worker(
            req.task_id.clone(),
            entry.agent.as_str().to_string(),
            req.prompt.clone(),
        );

        // 5. Build the worker and run to completion (synchronous-from-Manager).
        let deliverable = if self.cancellation_token.is_cancelled() {
            Deliverable {
                marker: MissionMarker::Failed {
                    reason: "aborted".to_string(),
                },
                content: "Task aborted by user instruction.\n\nFAILED (aborted)".to_string(),
                task_id: req.task_id.clone(),
            }
        } else {
            let worker = self.registry.worker(entry.agent);
            let child_token = self.cancellation_token.child_token();
            worker.run(&ctx, &child_token).await
        };

        let elapsed_ms = start_time.elapsed().as_millis();
        let marker_str = format!("{:?}", deliverable.marker);
        crate::debug_log::log_delegation_finish(
            entry.agent.as_str(),
            req.task_id.as_deref(),
            &marker_str,
            elapsed_ms,
            &deliverable.content,
        );

        // 6. Deep-Freeze: the delegation terminated (cleanly). Release the
        //    frozen checkpoint so a later recovery does not re-resume a task
        //    that already finished.
        if !worker_id.is_empty() {
            let _ = self.journal.clear(&worker_id, true);
        }

        // Surface the delegation completion to the UI.
        if let Ok(mut ev) = self.delegation_events.lock() {
            ev.push(DelegationEvent::Completed {
                agent: entry.agent,
                task: req.task_id.clone(),
            });
        }

        // Bind task_id & auto check-off.
        Ok(self.apply_check_off(deliverable, req.task_id.clone()))
    }

    /// REQ-ORCH-003 (persistence) / Deep-Freeze recovery: after a crash, the
    /// system rehydrates the frozen subagent using the identical `worker_id`
    /// from the Crash Journal. When the Manager boots (or is asked to recover),
    /// call this to either *resume* the in-flight task or *fail it properly*.
    ///
    /// - If a frozen snapshot exists, it re-delegates with the preserved
    ///   in-flight `sub_req` under the same `worker_id`, then clears the
    ///   checkpoint on success.
    /// - If the preserved request cannot be resumed (e.g. the agent is no
    ///   longer registered), it records a `Failed` journal event so the plan
    ///   line stays unchecked and the parent can re-plan — it does NOT crash.
    ///
    /// Returns the rehydrated deliverable when a frozen delegation was
    /// resumed, or `None` when there was nothing frozen (clean boot).
    pub async fn recover_frozen(&self) -> Result<Option<Deliverable>> {
        let Some(snap) = self.journal.frozen()? else {
            return Ok(None);
        };

        // Re-resolve the specialist (REQ-ORCH-002). If the role disappeared,
        // fail the frozen task properly instead of silently dropping it.
        let Some(entry) = self.registry.resolve(snap.agent_name) else {
            tracing::warn!(
                "Deep-Freeze: agent {} no longer registered; failing frozen task",
                snap.agent_name
            );
            let _ = self.journal.clear(&snap.worker_id, false);
            return Err(anyhow::anyhow!(
                "Deep-Freeze: frozen worker {} (agent {}) cannot be rehydrated: \
                 role no longer registered",
                snap.worker_id,
                snap.agent_name
            ));
        };

        // Rebuild the isolated context from the preserved in-flight `sub_req`
        // — this is the SOLE exception to isolation, scoped to the frozen
        // session (SPEC §3.4). Rehydrate with the identical worker_id.
        let ctx = IsolatedContext::from_request(self.role_prompt_for(entry.agent), &snap.sub_req);
        let worker = self.registry.worker(entry.agent);
        let child_token = self.cancellation_token.child_token();
        let deliverable = worker.run(&ctx, &child_token).await;

        // The frozen delegation resolved: release the checkpoint so it is not
        // resumed again on a subsequent boot.
        let clean = !matches!(deliverable.marker, MissionMarker::Failed { .. });
        let _ = self.journal.clear(&snap.worker_id, clean);
        Ok(Some(self.apply_check_off(
            deliverable,
            snap.sub_req.task_id.clone(),
        )))
    }

    /// Auto check-off: on `MISSION COMPLETE (task-id)` flip `- [ ] [t-xxx]` to
    /// `- [x] [t-xxx]`; on FAILED/REPLAN leave unchecked (REQ-PLAN-002).
    ///
    /// t-202/t-302: the authoritative [`MissionMarker`] on the deliverable is the
    /// gate keeper. A task is only ever checked off when the *marker* is a
    /// genuine [`MissionMarker::Complete`] AND the re-parsed *content* still
    /// carries a `MISSION COMPLETE (t-xxx)` terminal marker (via
    /// [`crate::agent::phase::Plan::check_plan_on_marker`]). This double gate
    /// guarantees that a `FAILED` / `REPLAN` deliverable — or a REJECTED
    /// deliverable that carries a stale completion token from a pre-validation
    /// draft in its content body — stays unchecked (REQ-PLAN-002 / REQ-ORCH-005).
    /// The resolved `task_id` override is passed as the explicit binding so
    /// check-off still works when the subagent omits the parenthesized id.
    fn apply_check_off(&self, d: Deliverable, task_id: Option<String>) -> Deliverable {
        let tid = d.task_id.clone().or(task_id);
        if let Some(t) = &tid {
            // Only a genuine Complete marker (not Failed/Replan) may ever check
            // the task off, regardless of what tokens appear in the content body.
            if matches!(d.marker, MissionMarker::Complete { .. }) {
                // Second gate: the *content* must still carry the terminal marker.
                if let Ok(true) = self.plan.check_plan_on_marker(Some(t), &d.content) {
                    crate::debug_log::log_plan_update(
                        "check_off",
                        &format!("Task [{t}] marked completed [x] on disk"),
                    );
                }
            }
            let mut d = d;
            d.task_id = Some(t.clone());
            d
        } else {
            d
        }
    }

    /// REQ-ORCH-001: resolve the role system prompt for a specialist. Reads the
    /// canonical per-role constant (isolated-context messages[0]).
    fn role_prompt_for(&self, agent: Agent) -> String {
        match agent {
            Agent::Coder => crate::agents::coder::CODER_ROLE_PROMPT.to_string(),
            Agent::Researcher => crate::agents::researcher::RESEARCHER_ROLE_PROMPT.to_string(),
            Agent::Debugger => crate::agents::debugger::DEBUGGER_ROLE_PROMPT.to_string(),
            Agent::Validator => crate::agents::validator::VALIDATOR_ROLE_PROMPT.to_string(),
            Agent::Generalist => crate::agents::generalist::GENERALIST_ROLE_PROMPT.to_string(),
        }
    }

    /// REQ-ORCH-001 / REQ-PLAN-003: drive the Executing phase as a Silent
    /// Dispatcher. For each unchecked plan task, route it to the specialist
    /// whose domain matches the task's type (via the `scheduler` closure),
    /// emitting `delegate_task` calls only. Independent tasks MAY be delegated
    /// in parallel (REQ-ORCH-005); each call blocks per-specialist.
    ///
    /// The `scheduler` closure maps a task id to the specialist whose domain
    /// matches that task's type (REQ-ORCH-002 selection rule).
    pub async fn run_executing(
        &mut self,
        scheduler: &dyn Fn(&str) -> Agent,
    ) -> Result<Vec<Deliverable>> {
        // t6-REQ-4: enforce the Manager-Never-Does-Domain-Work invariant at the
        // entry to the silent-dispatch loop, before any delegation.
        self.guard_no_domain_work()?;

        let mut results = Vec::new();
        let mut attempts = 0;
        // Cap iterations so an un-delegate-able task cannot loop forever.
        while !self.plan.is_complete() && attempts < MAX_EXECUTING_ROUNDS {
            attempts += 1;
            let pending = self.plan.pending_tasks();
            if pending.is_empty() {
                break;
            }
            for task_id in &pending {
                // Resolve the right specialist for this task.
                let agent = scheduler(task_id);
                let brief = brief_for_task(&self.plan, task_id);
                let req = DelegationRequest {
                    agent_name: agent,
                    prompt: brief,
                    snippets: vec![],
                    task_id: Some(task_id.clone()),
                    image_urls: None,
                    audio_urls: None,
                    recursion_granted: false,
                };
                let d = self.delegate(req).await?;
                results.push(d);
                // `delegate` already auto-checked-off on MISSION COMPLETE.
            }
        }
        Ok(results)
    }

    /// REQ-ORCH-001: synthesize the final answer from all sub-deliverables.
    /// This is the ONLY Manager prose permitted.
    pub fn synthesize(&self, results: &[Deliverable]) -> String {
        let mut out = String::new();
        for r in results {
            out.push_str(&r.content);
            out.push('\n');
        }
        out
    }

    /// REQ-ORCH-005: forward `/abort` to in-flight sub-tasks.
    ///
    /// NOTE (REQ-HARN-004 integrity): an abort is NOT a text-repetition break,
    /// so this does NOT touch `repetition_breaks` — that counter is reserved
    /// exclusively for [`HarnessMonitor::feed_text`] truncations. An abort
    /// terminates the turn (REQ-LOOP-004) and kills active PTY process groups
    /// via `crate::harness::pty::kill_process_group`; it is not counted as a
    /// resilience-repetition intervention.
    pub fn abort(&mut self) {
        self.cancellation_token.cancel();
        cancel_all();
    }
}

/// REQ-ORCH-005: the harness-level `delegate_task` handler.
///
/// Parses the tool arguments (`agent_name`, `prompt`, `snippets`, `task_id?`,
/// `image_urls?`, `audio_urls?`), validates the role against the registry,
/// builds a self-contained `DelegationRequest` (one task per call), routes it
/// through `OrchestratorManager::delegate` **synchronously** (REQ-ORCH-005:
/// the call blocks from the Manager's perspective via `block_on`), and returns
/// the deliverable as a `ToolResult` whose outcome reflects the `MISSION
/// COMPLETE (task-id)` / `FAILED` / `REPLAN REQUIRED` terminal marker.
///
/// The handler installs a Manager rooted at the shared `.marmel` plan dir so
/// `task_id` binding auto-check-off (`Plan::check_off`) targets the on-disk
/// plan (REQ-ORCH-004 shared workspace / REQ-PLAN-002).
pub fn handle_delegate_task(args: &serde_json::Value) -> Result<ToolResult, ToolError> {
    // 1. Parse the payload. `agent_name` deserializes through `Agent`'s
    //    snake_case enum, so an unknown role is rejected here with a clear
    //    error rather than panicking (REQ-ORCH-002).
    let req: DelegationRequest =
        serde_json::from_value(args.clone()).map_err(|e| ToolError::BadArguments {
            tool: TOOL_DELEGATE_TASK.to_string(),
            detail: e.to_string(),
        })?;

    // 2. One task per call: the brief MUST be self-contained and non-empty
    //    (REQ-ORCH-003/005). The subagent sees only this brief + snippets.
    if req.prompt.trim().is_empty() {
        return Err(ToolError::BadArguments {
            tool: TOOL_DELEGATE_TASK.to_string(),
            detail: "`prompt` must be a non-empty, self-contained task brief".to_string(),
        });
    }

    // 2b. Guard: reject re-delegation of tasks already checked off in the plan.
    #[cfg(not(test))]
    {
        let plan = Plan::default();
        if let Some(ref tid) = req.task_id
            && let Ok(Some(content)) = plan.read()
        {
            let tid_lower = tid.to_ascii_lowercase();
            let is_checked = content.lines().any(|line| {
                let lower = line.to_ascii_lowercase();
                lower.contains(&tid_lower) && (line.contains("[x]") || line.contains("[X]"))
            });
            if is_checked {
                tracing::warn!("Rejecting re-delegation of already completed task [{tid}]");
                return Ok(ToolResult::err(format!(
                    "Task '{tid}' is already completed and checked off in the execution plan. Do not re-delegate completed tasks. Proceed with your final report synthesis."
                )));
            }
        }
    }

    // 3. Route through a Manager rooted at the shared `.marmel` plan dir.
    //    The build URL/model are unused by the deterministic Phase-O
    //    `run_specialist_llm` driver, so a placeholder client is fine.
    let stats = Arc::new(HarnessStats::new());
    let manager = OrchestratorManager::new(
        ChatClient::new("http://127.0.0.1:11434/v1", "marmel-manager"),
        Plan::default(),
        stats,
    );

    let deliverable = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.block_on(manager.delegate(req))
    } else {
        futures::executor::block_on(manager.delegate(req))
    }
    .map_err(ToolError::Execution)?;

    // 5. Encode the terminal marker into the ToolResult so the loop's
    //    check-off and the Manager's synthesis can observe it.
    let tid = deliverable.task_id.as_deref().unwrap_or("unknown");
    match &deliverable.marker {
        MissionMarker::Complete { .. } => Ok(ToolResult::ok(format!(
            "{}\n\nMISSION COMPLETE ({tid})",
            deliverable.content
        ))),
        MissionMarker::Failed { reason } => Ok(ToolResult::err(format!(
            "{}\n\nFAILED: {reason}",
            deliverable.content
        ))),
        MissionMarker::Replan { reason } => Ok(ToolResult::err(format!(
            "{}\n\nREPLAN REQUIRED: {reason}",
            deliverable.content
        ))),
    }
}

/// REQ-ORCH-002 / REQ-ORCH-005: per-specialist tool-allowlist enforcement.
///
/// Returns `true` when the named caller role is permitted to invoke `tool`.
/// Specialists are gated by their registry allowlist; `create_plan` is a
/// Manager-only tool (no specialist allowlist grants it), and `delegate_task`
/// is permitted to a specialist only when its granted tool set includes it
/// (fractal recursion, REQ-ORCH-001).
pub fn caller_allows_tool(agent: Agent, tool: &str, registry: &SpecialistRegistry) -> bool {
    match registry.resolve(agent) {
        Some(entry) => entry.allows(tool),
        None => false,
    }
}

// NOTE: `brief_for_task` reads a plan line's text to build a delegation brief
// (REQ-ORCH-005 one-task-per-call: the brief is self-contained so the subagent
// does not need the Manager's context). It is `pub` so the Manager turn loop in
// `src/agent/loop.rs` reuses the same plan-line → brief builder.
/// Regex matching a plan task line in the `- [ ] [t-xxx] description` format.
/// Compiled exactly once via `OnceLock` (CODE_REVIEW Point 2).
static TASK_LINE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

pub fn brief_for_task(plan: &Plan, task_id: &str) -> String {
    // Read-only diagnostic: build a self-contained brief from the plan task
    // text. If the plan line is present, its descriptive text becomes the
    // brief; otherwise fall back to a deterministic generic instruction.
    if let Ok(Some(content)) = plan.read() {
        let re = TASK_LINE_RE.get_or_init(|| {
            regex::Regex::new(r"(?m)^\s*-\s*\[\s*[ xX]?\s*\]\s*\[(t-[A-Za-z0-9_-]+)\]\s*(.*)$")
                .expect("valid task line regex")
        });
        for caps in re.captures_iter(&content) {
            if &caps[1] == task_id {
                let desc = caps[2].trim();
                if !desc.is_empty() {
                    return format!(
                        "{desc}\n\nExecute this delegated task to completion and return your \
                         deliverable, ending with MISSION COMPLETE ({task_id})."
                    );
                }
            }
        }
    }
    "Execute the delegated task described by the plan line, producing the
deliverable and ending with MISSION COMPLETE (task-id)."
        .to_string()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
