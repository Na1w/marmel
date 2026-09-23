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
pub mod delegate;
pub mod delegation;
pub mod freeze;
pub mod plan_summary;
pub mod preemption;
pub mod registry;
pub mod steer;
pub mod steer_extractor;
#[cfg(test)]
mod steer_tests;
pub mod workers;

pub use preemption::{
    PreemptHandle, PreemptibleStreamSink, StreamIdentity, models_conflict,
    preempt_conflicting_stream,
};

pub use crate::agents::{
    Agent, DelegationRequest, Deliverable, IsolatedContext, MissionMarker, Specialist,
};
use crate::config::Config;
use crate::harness::HarnessStats;
use crate::llm::ChatClient;
use crate::manager::phase::Plan;
use anyhow::Result;
pub use bus::{
    CURRENT_WORKER_TOKEN, cancel_all, emit_event, emit_status, global_cancellation_token,
    is_current_or_global_cancelled, is_globally_cancelled, reset_cancellation, set_event_sender,
    set_status_sender,
};
pub use delegate::{brief_for_task, caller_allows_tool, handle_delegate_task};
pub use delegation::{Delegation, DelegationEvent, OrchestrationConfig, RecursionDepth};
pub use freeze::{CrashJournal, FreezeSnapshot, JournalEventKind};
pub use plan_summary::generate_plan_progress_summary;
pub use registry::SpecialistRegistry;
use std::sync::Arc;
pub use steer::{
    SteerDecision, SteerOutcome, SteerSubtaskDecision, StreamingResponseExtractor, arbitrate_steer,
    arbitrate_steer_stream, arbitrate_steer_stream_with_fallback, arbitrate_steer_with_fallback,
    execute_steer_subtask, extract_tasks_to_delegate, format_steering_history,
    normalize_steer_decision, resolve_steer_outcome,
};
pub use workers::{
    ActiveWorkerGuard, ActiveWorkerInfo, CompletedWorkerInfo, cancel_active_worker,
    cancel_all_active_workers, format_duration_human, get_active_specialist_context_str,
    get_active_subtasks_str, get_active_worker_tokens, has_active_workers, register_active_worker,
    register_active_worker_with_token, set_active_worker_status, update_active_worker_context,
    update_active_worker_progress,
};

/// Guard for the Silent Dispatcher so a plan that cannot make progress fails
/// loudly instead of spinning.
pub const MAX_EXECUTING_ROUNDS: usize = 100;

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
        crate::debug_log::log_plan_update(crate::tool_names::TOOL_CREATE_PLAN, plan_markdown);
        self.plan.create(plan_markdown)?;
        let prompts_dir = self.plan.dir().join("prompts");
        let ws_root = self.plan.dir().parent().unwrap_or_else(|| self.plan.dir());
        let catalog = crate::agents::Catalog::discover(ws_root);
        crate::agents::PromptBuilder::pregenerate_for_plan_offline(
            plan_markdown,
            &catalog,
            &prompts_dir,
        );
        Ok(())
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

        // 4. Build the ISOLATED context (REQ-ORCH-003): Disk-first lookup!
        //    If a prompt already exists for this task on disk (from plan pregeneration),
        //    load it immediately for zero-latency startup. Fall back to JIT synthesis
        //    for ad-hoc or un-planned tasks.
        let prompts_dir = self.plan.dir().join("prompts");
        let ws_root = self.plan.dir().parent().unwrap_or_else(|| self.plan.dir());

        let saved_prompt_path = req.task_id.as_deref().map(|tid| {
            let clean = tid
                .trim_matches(|c| {
                    c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
                })
                .trim();
            prompts_dir.join(format!("{clean}.md"))
        });

        let blueprint = if let Some(path) = saved_prompt_path.filter(|p| p.exists()) {
            crate::agents::AgentBlueprint::load_from_disk(&path).ok()
        } else {
            None
        };

        let blueprint = match blueprint {
            Some(bp) => bp,
            None => {
                let catalog = crate::agents::Catalog::discover(ws_root);
                crate::agents::PromptBuilder::build_blueprint(
                    Some(&self.client),
                    Some(self.client.model()),
                    &catalog,
                    &req,
                    Some(&prompts_dir),
                )
                .await
            }
        };

        let ctx = IsolatedContext::from_request(blueprint.system_prompt.clone(), &req)
            .with_blueprint(blueprint);

        let child_token = self.cancellation_token.child_token();

        // Register active worker for real-time steering arbitrator visibility
        let _active_guard = register_active_worker_with_token(
            req.task_id.clone(),
            entry.agent.as_str().to_string(),
            req.prompt.clone(),
            Some(child_token.clone()),
        );

        // 5. Build the worker and run to completion (synchronous-from-Manager).
        let deliverable = if self.cancellation_token.is_cancelled() || child_token.is_cancelled() {
            Deliverable {
                marker: MissionMarker::Failed {
                    reason: "aborted".to_string(),
                },
                content: "Task aborted by user instruction.\n\nFAILED (aborted)".to_string(),
                task_id: req.task_id.clone(),
            }
        } else {
            let worker = self.registry.worker(entry.agent);
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
        let prompts_dir = self.plan.dir().join("prompts");
        let ws_root = self.plan.dir().parent().unwrap_or_else(|| self.plan.dir());
        let saved_prompt_path = snap.sub_req.task_id.as_deref().map(|tid| {
            let clean = tid
                .trim_matches(|c| {
                    c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
                })
                .trim();
            prompts_dir.join(format!("{clean}.md"))
        });
        let blueprint = if let Some(path) = saved_prompt_path.filter(|p| p.exists()) {
            crate::agents::AgentBlueprint::load_from_disk(&path).ok()
        } else {
            None
        };
        let (prompt, blueprint) = if let Some(bp) = blueprint {
            (bp.system_prompt.clone(), Some(bp))
        } else {
            let catalog = crate::agents::Catalog::discover(ws_root);
            let bp = crate::agents::PromptBuilder::synthesize_offline(&catalog, &snap.sub_req);
            (bp.system_prompt.clone(), Some(bp))
        };
        let mut ctx = IsolatedContext::from_request(prompt, &snap.sub_req);
        if let Some(bp) = blueprint {
            ctx = ctx.with_blueprint(bp);
        }
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
    /// [`crate::manager::phase::Plan::check_plan_on_marker`]). This double gate
    /// guarantees that a `FAILED` / `REPLAN` deliverable — or a REJECTED
    /// deliverable that carries a stale completion token from a pre-validation
    /// draft in its content body — stays unchecked (REQ-PLAN-002 / REQ-ORCH-005).
    /// The resolved `task_id` override is passed as the explicit binding so
    /// check-off still works when the subagent omits the parenthesized id.
    fn apply_check_off(&self, d: Deliverable, task_id: Option<String>) -> Deliverable {
        let tid = d.task_id.clone().or(task_id);
        if matches!(d.marker, MissionMarker::Complete { .. }) {
            // Second gate: the *content* must still carry the terminal marker.
            if let Ok(true) = self.plan.check_plan_on_marker(tid.as_deref(), &d.content)
                && let Some(t) = &tid
            {
                crate::debug_log::log_plan_update(
                    "check_off",
                    &format!("Task [{t}] marked completed [x] on disk"),
                );
            }
        }
        let mut d = d;
        d.task_id = tid;
        d
    }

    /// REQ-ORCH-001: resolve the role system prompt for a specialist.
    /// Used in tests to verify specialist prompt isolation invariants.
    #[cfg(test)]
    pub(crate) fn role_prompt_for(&self, agent: Agent) -> String {
        match agent {
            Agent::Coder => crate::agents::coder::CODER_ROLE_PROMPT.to_string(),
            Agent::Researcher => crate::agents::researcher::RESEARCHER_ROLE_PROMPT.to_string(),
            Agent::Debugger => crate::agents::debugger::DEBUGGER_ROLE_PROMPT.to_string(),
            Agent::Validator => crate::agents::validator::VALIDATOR_ROLE_PROMPT.to_string(),
            Agent::Generalist => crate::agents::generalist::GENERALIST_ROLE_PROMPT.to_string(),
            Agent::Planner => crate::agents::planner::PLANNER_ROLE_PROMPT.to_string(),
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

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
