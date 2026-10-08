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
pub mod notice;
#[cfg(test)]
mod notice_tests;
pub mod plan_summary;
pub mod preemption;
pub mod registry;
pub mod steer;
pub mod steer_extractor;
#[cfg(test)]
mod steer_tests;
pub mod workers;

pub use notice::{
    NoticeReplyRejection, SteerNotice, SteerNoticeReply, WorkerReplyEvaluation, clear_all_notices,
    drain_worker_notices, drain_worker_notices_mid_turn, drain_worker_notices_report,
    evaluate_worker_reply, get_pending_notice, get_pending_notice_for_worker, get_worker_reply,
    next_notice_id, notice_addresses_worker, post_notice_to_worker, record_worker_reply,
    record_worker_reply_for_notice, render_notice_for_worker,
};

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
#[cfg(test)]
pub use bus::clear_steering_history;
pub use bus::{
    CURRENT_WORKER_TOKEN, SharedSteeringHistory, cancel_all, emit_event, emit_status,
    get_steering_history, global_cancellation_token, is_current_or_global_cancelled,
    is_globally_cancelled, record_steering_exchange, reset_cancellation, set_event_sender,
    set_status_sender, set_steering_history,
};
pub use delegate::{brief_for_task, caller_allows_tool, handle_delegate_task};
pub use delegation::{Delegation, DelegationEvent, OrchestrationConfig, RecursionDepth};
pub use plan_summary::generate_plan_progress_summary;
pub use registry::SpecialistRegistry;
use std::path::{Path, PathBuf};
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
    /// Delegation lifecycle events surfaced to the UI (t6-REQ-3). The Manager
    /// records a `Started`/`Completed` event per delegation so renderers can show
    /// which specialist is active and on which task. Interior mutability
    /// (`Arc<Mutex<_>>`) lets the shared Manager — built in `boot_manager`
    /// (`src/main.rs`) and handed to the live session runner as
    /// `Arc<OrchestratorManager>` (`run_session` in `src/ui/session.rs`, which
    /// drives the Manager turn loop while delegated specialist turns run in
    /// `src/agents/runner/*`) — be drained concurrently by the UI
    /// (`drain_delegation_events_with_transcript`) without a `&mut` borrow, so
    /// `delegate` stays `&self`.
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
        let cancellation_token = global_cancellation_token().child_token();
        Self {
            client,
            plan,
            registry: SpecialistRegistry::canonical(),
            orchestration,
            stats,
            depth: RecursionDepth::root(),
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

        // 4. Build the ISOLATED context (REQ-ORCH-003): Disk-first lookup!
        //    If a prompt already exists for this task on disk (from plan pregeneration),
        //    load it immediately for zero-latency startup. Fall back to JIT synthesis
        //    for ad-hoc or un-planned tasks.
        //
        //    Gate t-055: the disk-first path is built through
        //    [`saved_prompt_path_for_task`] so the task id passes
        //    [`crate::task_id::validate_task_id`] before it is ever joined onto the
        //    prompts directory. A rejected id is reported and the read is skipped —
        //    exactly the shape of a missing prompt file — so the delegation falls
        //    back to JIT prompt synthesis instead of reading (or deriving) some
        //    other file name.
        let prompts_dir = self.plan.dir().join("prompts");
        let ws_root = self.plan.dir().parent().unwrap_or_else(|| self.plan.dir());

        let saved_prompt_path = match saved_prompt_path_for_task(
            &prompts_dir,
            req.task_id.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!(
                    "Rejected task id {:?} while looking up its synthesized prompt under {}: {err}. \
                     The prompt is treated as unavailable, so no read outside the prompts \
                     directory is attempted and JIT prompt synthesis is used for this delegation.",
                    req.task_id,
                    prompts_dir.display()
                );
                None
            }
        };

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
                    reason: crate::markers::ABORT_REASON.to_string(),
                },
                // gate t-070: the whole aborted-deliverable body — sentence and
                // verdict trailer — is spelled by the single owner
                // `crate::markers::aborted_deliverable`, the same call
                // `orchestrator::delegate::handle_delegate_task`,
                // `agents::Generalist::run` and
                // `agents::runner::execution::aborted_deliverable` make. This
                // site used to hand-build `"…\n\nFAILED (aborted)"` through
                // `failed_trailer`, which is the one marker vocabulary copy the
                // conservative single-owner scan could not see (this file
                // declares `#[cfg(test)] mod …` items *before* its production
                // code, so the line-by-line scan stops at line 18) — the reason
                // `HAND_OFF_MARKER_SITES` existed and the reason it is now empty.
                content: crate::markers::aborted_deliverable("aborted by user instruction"),
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

        // Surface the delegation completion to the UI.
        if let Ok(mut ev) = self.delegation_events.lock() {
            if matches!(deliverable.marker, MissionMarker::Failed { .. }) {
                let reason = match &deliverable.marker {
                    MissionMarker::Failed { reason } => {
                        Some(crate::ui::helpers::extract_failure_reason(reason))
                    }
                    _ => Some(crate::ui::helpers::extract_failure_reason(
                        &deliverable.content,
                    )),
                };
                ev.push(DelegationEvent::Failed {
                    agent: entry.agent,
                    task: req.task_id.clone(),
                    reason,
                });
            } else {
                ev.push(DelegationEvent::Completed {
                    agent: entry.agent,
                    task: req.task_id.clone(),
                });
            }
        }

        // Bind task_id & auto check-off.
        Ok(self.apply_check_off(deliverable, req.task_id.clone()))
    }

    /// Auto check-off: on `MISSION COMPLETE (task-id)` flip `- [ ] [t-xxx]` to
    /// `- [x] [t-xxx]`; on FAILED/REPLAN leave unchecked (REQ-PLAN-002).
    ///
    /// t-202/t-302: the authoritative [`MissionMarker`] on the deliverable is the
    /// gate keeper. A task is only ever checked off when the *marker* is a
    /// genuine [`MissionMarker::Complete`] AND the *content* still carries a
    /// `MISSION COMPLETE (t-xxx)` terminal marker. This double gate guarantees
    /// that a `FAILED` / `REPLAN` deliverable — or a REJECTED deliverable that
    /// carries a stale completion token from a pre-validation draft in its
    /// content body — stays unchecked (REQ-PLAN-002 / REQ-ORCH-005). The
    /// resolved `task_id` override is passed as the explicit binding so
    /// check-off still works when the subagent omits the parenthesized id.
    ///
    /// t-035a: the already-parsed `d.marker` is **threaded into**
    /// [`Plan::check_plan_on_deliverable`] instead of being dropped, so the plan
    /// layer resolves the verdict and the task id from the structured marker
    /// (the single authority) rather than re-deriving them from the body. The
    /// body scan below stays as the second gate and is produced by the same
    /// owner (`crate::markers` through [`MissionMarker::parse`]) — it can never
    /// diverge from the plan layer's parse, and a marker naming a **different**
    /// task id aborts the check-off there.
    fn apply_check_off(&self, d: Deliverable, task_id: Option<String>) -> Deliverable {
        let tid = d.task_id.clone().or(task_id);
        if matches!(d.marker, MissionMarker::Complete { .. }) {
            // Second gate: the *content* must still carry a terminal completion
            // marker, judged by the marker owner — no second parser here.
            let body_says_complete = matches!(
                MissionMarker::parse(&d.content),
                Some(MissionMarker::Complete { .. })
            );
            if body_says_complete
                && let Ok(true) =
                    self.plan
                        .check_plan_on_deliverable(Some(&d.marker), tid.as_deref(), &d.content)
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
    ///
    /// The loop gate uses the **fallible** plan API ([`Plan::try_pending_tasks`]),
    /// so an unreadable plan is an error, never a completion (bug M8):
    /// * `Ok(non-empty)` — dispatch those tasks (the normal path);
    /// * `Ok(empty)` — the plan **was read successfully** and nothing is pending
    ///   (all boxes ticked, or no plan file at all): the completion path;
    /// * `Err(_)` — the plan state is **UNKNOWN**: logged and propagated, so no
    ///   caller can read this run as "the mission is finished".
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
        //
        // M8 (t-031h fallible API; folded hand-off): this gate used to be
        // `!is_complete() && pending_tasks()`. `pending_tasks()` swallows a read
        // failure (it logs, then yields an EMPTY `Vec`), and the `is_empty()`
        // break below returned `Ok(results)` — byte-for-byte what a finished plan
        // returns. An unreadable plan therefore silently dropped the remaining
        // work and let the mission look complete. The gate now reads the plan
        // exactly ONCE per round through `try_pending_tasks()`, which propagates
        // the failure (it also removes the old second `PLAN_MUTEX` acquisition per
        // round made by `is_complete()`; nothing else on this path holds the
        // guard, so there is no second lock path and no re-entrant deadlock).
        while attempts < MAX_EXECUTING_ROUNDS {
            let pending = match self.plan.try_pending_tasks() {
                Ok(pending) => pending,
                Err(e) => {
                    let e = e.context(
                        "Silent Dispatcher stopped: the execution plan state is UNKNOWN, so no \
                         task can be dispatched and the mission is NOT complete",
                    );
                    tracing::error!("{e:#}");
                    return Err(e);
                }
            };
            if pending.is_empty() {
                // A successfully read plan with nothing pending (every box ticked,
                // or no plan file on disk): the pre-existing completion path.
                break;
            }
            attempts += 1;
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

/// Disk-first prompt path for a delegation request, gated on the canonical
/// task-id grammar.
///
/// Gate t-055 (single grammar authority): `.marmel/prompts/<task_id>.md` is an
/// on-disk path derived from a task id that originates as LLM output
/// (`delegate_task` argument / plan checkbox), so the id must pass
/// [`crate::task_id::validate_task_id`] before it is joined onto `prompts_dir`.
/// That function is the only place in the crate that defines the task-id
/// grammar — this helper does not re-implement, extend or bypass it.
///
/// Contract (same shape as
/// [`crate::harness::workspace::Workspace::prompt_path_for_task`]):
/// * the raw id is normalized first (decoration stripping only, order unchanged)
///   and the **normalized** value is validated and joined byte-for-byte — never
///   sanitized, trimmed, clamped or mended into a different file name;
/// * `Ok(None)` means "no task id was supplied" (the JIT-synthesis case);
/// * `Err(TaskIdError)` means the id was refused: the caller must skip the read
///   and degrade exactly like a missing prompt file, which is what makes the
///   JIT fallback observable rather than a silent path escape.
fn saved_prompt_path_for_task(
    prompts_dir: &Path,
    raw_task_id: Option<&str>,
) -> Result<Option<PathBuf>, crate::task_id::TaskIdError> {
    let Some(raw) = raw_task_id else {
        return Ok(None);
    };
    let clean = crate::task_id::normalize_task_id_ref(raw);
    let id = crate::task_id::validate_task_id(clean)?;
    Ok(Some(prompts_dir.join(format!("{id}.md"))))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
