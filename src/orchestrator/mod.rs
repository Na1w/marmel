//! Fractal Orchestration — Manager + Specialist Subagents.
//!
//! The Manager (`OrchestratorManager`) owns user interaction, goal
//! decomposition, planning, delegation, and synthesis. It is STRICTLY
//! FORBIDDEN from performing domain-specific work; every unit of domain work is
//! emitted via `delegate_task`. Its only permitted tools are `delegate_task`,
//! `create_plan`/plan updates, read-only non-domain diagnostic inspection, and
//! final synthesis.
//!
//! Submodules:
//! - `registry` — the `SpecialistRegistry` of resolvable role ids and their
//!   tool namespaces.

pub mod bus;
pub mod delegate;
pub mod delegation;
pub mod notice;
#[cfg(test)]
mod notice_tests;
pub mod plan_summary;
pub mod pool;
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
pub use delegation::{
    Delegation, DelegationEvent, OrchestrationConfig, RecursionDepth, clear_delegation_event_sink,
    delegation_event_sink, delegation_identity, push_delegation_event, set_delegation_event_sink,
};
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
    ActiveWorkerGuard, ActiveWorkerInfo, CompletedWorkerInfo, WorkerCallClass, WorkerEntry,
    active_worker_entries, cancel_active_worker, cancel_all_active_workers, format_duration_human,
    get_active_specialist_context_str, get_active_subtasks_str, get_active_worker_tokens,
    has_active_workers, recent_completed_entries, register_active_worker,
    register_active_worker_with_token, set_active_worker_status, update_active_worker_context,
    update_active_worker_progress, worker_entries,
};

/// Guard for the Silent Dispatcher so a plan that cannot make progress fails
/// loudly instead of spinning.
pub const MAX_EXECUTING_ROUNDS: usize = 100;

/// The Manager (Orchestrator). Owns user interaction, goal decomposition,
/// planning, delegation, and synthesis. It NEVER performs domain work itself.
///
/// All delegation methods are **synchronous from the Manager's perspective**:
/// `delegate` blocks until the specialist returns its deliverable.
#[derive(Debug)]
pub struct OrchestratorManager {
    /// User interaction handle (synthesis-driven replies to the user).
    pub client: ChatClient,
    /// The on-disk plan (shared workspace source of truth).
    pub plan: Plan,
    /// The specialist registry.
    pub registry: SpecialistRegistry,
    /// Orchestration config (max depth bound).
    pub orchestration: OrchestrationConfig,
    /// Shared resilience counters (bumped on delegation/abort).
    pub stats: Arc<HarnessStats>,
    /// Current recursion depth (the Manager is the root, depth 0).
    pub depth: RecursionDepth,
    /// Delegation lifecycle events surfaced to the UI: the Manager records a
    /// `Started`/`Completed`/`Failed` event per delegation so renderers can show
    /// which specialist is active and on which task, and the concurrency gate
    /// appends the `Queued`/`Running` slot transitions for the same delegation
    /// (see [`delegation::push_delegation_event`]). Interior mutability
    /// (`Arc<Mutex<_>>`) lets the shared `Arc<OrchestratorManager>` — built in
    /// `boot_manager` (`src/main.rs`) and handed to `run_session`
    /// (`src/ui/session.rs`) — be drained by the UI
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

    /// Emit a `delegate_task` and **block** until the specialist
    /// returns. This is synchronous from the Manager's perspective.
    pub async fn delegate(&self, req: DelegationRequest) -> Result<Deliverable> {
        // 1. Resolve the agent against the registry.
        let entry = self
            .registry
            .resolve(req.agent_name)
            .ok_or_else(|| anyhow::anyhow!("unknown specialist: {}", req.agent_name))?;

        // 2. Fractal depth bound: nested delegation beyond the bound is
        //    rejected UNCONDITIONALLY — the bound is enforced regardless of
        //    `recursion_granted` — and before any worker is spawned or any
        //    `Started` event is surfaced, so a rejected delegation never emits a
        //    spurious lifecycle event.
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

        // 2b. Surface the delegation start to the UI. Emitted only
        //     after the depth check passes, so a rejected delegation never
        //     surfaces a `Started` event: it returns before spawning a worker.
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

        // 4. Build the ISOLATED context: disk-first lookup. A prompt that already
        //    exists on disk for this task (from plan pregeneration) is loaded
        //    immediately for zero-latency startup; ad-hoc or un-planned tasks
        //    fall back to JIT synthesis.
        //
        //    The path is built through [`saved_prompt_path_for_task`], so the task
        //    id passes [`crate::task_id::validate_task_id`] before it is ever
        //    joined onto the prompts directory. A rejected id is reported and the
        //    read skipped — the same shape as a missing prompt file — so the
        //    delegation falls back to JIT synthesis rather than reading (or
        //    deriving) some other file name.
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
                // The whole aborted-deliverable body — sentence and verdict
                // trailer — is spelled by the single owner
                // `crate::markers::aborted_deliverable`, the same call made by
                // `orchestrator::delegate::handle_delegate_task`,
                // `agents::Generalist::run` and
                // `agents::runner::execution::aborted_deliverable`; no marker
                // text is hand-built at this site.
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

        Ok(self.apply_check_off(deliverable, req.task_id.clone()))
    }

    /// Auto check-off: on `MISSION COMPLETE (task-id)` flip `- [ ] [t-xxx]` to
    /// `- [x] [t-xxx]`; on FAILED/REPLAN leave unchecked. The authoritative
    /// [`MissionMarker`] decides: only a genuine [`MissionMarker::Complete`]
    /// **and** a body that [`MissionMarker::parse`] still reads as complete may
    /// check a task off, so a deliverable carrying a stale completion token from
    /// an earlier draft stays unchecked. The parsed marker plus the resolved
    /// `task_id` binding are threaded into [`Plan::check_plan_on_deliverable`],
    /// which aborts when the marker names a **different** task id.
    fn apply_check_off(&self, d: Deliverable, task_id: Option<String>) -> Deliverable {
        let tid = d.task_id.clone().or(task_id);
        if matches!(d.marker, MissionMarker::Complete { .. }) {
            // Second check: the *content* must still carry a terminal completion
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

    /// Resolve the role system prompt for a specialist.
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

    /// Drive the Executing phase as a Silent Dispatcher: every unchecked plan
    /// task is routed by the `scheduler` closure (task id → matching specialist)
    /// to `delegate_task` calls only, each blocking per-specialist.
    ///
    /// Each round reads the plan exactly once through the **fallible**
    /// [`Plan::try_pending_tasks`]: `Ok(non-empty)` dispatches, `Ok(empty)` is the
    /// completion path, and `Err(_)` means the plan state is **UNKNOWN** — logged
    /// and propagated, never readable as "the mission is finished".
    pub async fn run_executing(
        &mut self,
        scheduler: &dyn Fn(&str) -> Agent,
    ) -> Result<Vec<Deliverable>> {
        // Enforce the Manager-Never-Does-Domain-Work invariant at the
        // entry to the silent-dispatch loop, before any delegation.
        self.guard_no_domain_work()?;

        let mut results = Vec::new();
        let mut attempts = 0;
        // Cap iterations so an un-delegate-able task cannot loop forever.
        //
        // The plan is read exactly ONCE per round through `try_pending_tasks()`,
        // which propagates a read failure instead of yielding an empty list: an
        // unreadable plan can therefore never be mistaken for a finished one, and
        // no second per-round `PLAN_MUTEX` acquisition exists on this path.
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
                // or no plan file on disk): the completion path.
                break;
            }
            attempts += 1;
            for task_id in &pending {
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

    /// Synthesize the final answer from all sub-deliverables.
    /// This is the ONLY Manager prose permitted.
    pub fn synthesize(&self, results: &[Deliverable]) -> String {
        let mut out = String::new();
        for r in results {
            out.push_str(&r.content);
            out.push('\n');
        }
        out
    }

    /// Forward `/abort` to in-flight sub-tasks.
    ///
    /// INTEGRITY: an abort is NOT a text-repetition break, so this does NOT touch
    /// `repetition_breaks` — that counter is reserved exclusively for
    /// [`HarnessMonitor::feed_text`] truncations. An abort terminates the turn and
    /// kills active PTY process groups via
    /// `crate::harness::pty::kill_process_group`; it is never counted as a
    /// resilience-repetition intervention.
    pub fn abort(&mut self) {
        self.cancellation_token.cancel();
        cancel_all();
    }
}

/// Disk-first prompt path for a delegation request: `.marmel/prompts/<task_id>.md`
/// derives from an id that originates as LLM output (`delegate_task` argument /
/// plan checkbox), so it must pass [`crate::task_id::validate_task_id`] — the single
/// task-id authority, never re-implemented or bypassed here — before it is joined
/// onto `prompts_dir`. Same shape as
/// [`crate::harness::workspace::Workspace::prompt_path_for_task`]: no id yields
/// `Ok(None)`, a refused id makes the caller skip the read and degrade like a
/// missing prompt file, and a valid id is joined byte-for-byte — never sanitized.
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
