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

pub mod freeze;
pub mod registry;
pub mod steer;

pub use crate::agents::{
    Agent, DelegationRequest, Deliverable, IsolatedContext, MissionMarker, Specialist,
};
pub use freeze::{CrashJournal, FreezeSnapshot, JournalEventKind};
pub use registry::SpecialistRegistry;
pub use steer::{
    SteerDecision, SteerOutcome, SteerSubtaskDecision, StreamingResponseExtractor, arbitrate_steer,
    arbitrate_steer_stream, arbitrate_steer_stream_with_fallback, arbitrate_steer_with_fallback,
    resolve_steer_outcome,
};

static STATUS_SENDER: std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<String>>> =
    std::sync::RwLock::new(None);

static EVENT_SENDER: std::sync::RwLock<
    Option<tokio::sync::mpsc::UnboundedSender<crate::ui::Event>>,
> = std::sync::RwLock::new(None);

static GLOBAL_CANCELLATION_TOKEN: std::sync::LazyLock<
    std::sync::RwLock<tokio_util::sync::CancellationToken>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(tokio_util::sync::CancellationToken::new()));

/// Get the current session-wide cancellation token.
pub fn global_cancellation_token() -> tokio_util::sync::CancellationToken {
    GLOBAL_CANCELLATION_TOKEN
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Cancel all active subagents, workers, LLM streams, and operations across the entire process.
pub fn cancel_all() {
    if let Ok(guard) = GLOBAL_CANCELLATION_TOKEN.read() {
        guard.cancel();
    }
}

/// Returns true if a global cancellation / abort signal has been requested.
pub fn is_globally_cancelled() -> bool {
    GLOBAL_CANCELLATION_TOKEN
        .read()
        .map(|guard| guard.is_cancelled())
        .unwrap_or(false)
}

/// Reset the global cancellation token for a fresh turn / prompt cycle.
pub fn reset_cancellation() {
    if let Ok(mut guard) = GLOBAL_CANCELLATION_TOKEN.write() {
        *guard = tokio_util::sync::CancellationToken::new();
    }
}

/// Register an unbounded channel to receive real-time status updates across all agents and specialists.
pub fn set_status_sender(tx: tokio::sync::mpsc::UnboundedSender<String>) {
    if let Ok(mut lock) = STATUS_SENDER.write() {
        *lock = Some(tx);
    }
}

/// Register an unbounded channel to receive real-time UI events across all agents and specialists.
pub fn set_event_sender(tx: tokio::sync::mpsc::UnboundedSender<crate::ui::Event>) {
    if let Ok(mut lock) = EVENT_SENDER.write() {
        *lock = Some(tx);
    }
}

/// Emit a status update to the active UI renderer.
pub fn emit_status(msg: impl Into<String>) {
    if let Ok(lock) = STATUS_SENDER.read()
        && let Some(tx) = lock.as_ref()
    {
        let _ = tx.send(msg.into());
    }
}

/// Emit a UI event directly to the active UI renderer.
pub fn emit_event(ev: crate::ui::Event) {
    if let Ok(lock) = EVENT_SENDER.read()
        && let Some(tx) = lock.as_ref()
    {
        let _ = tx.send(ev);
    }
}

/// Information about an active specialist worker currently executing a task.
#[derive(Debug, Clone)]
pub struct ActiveWorkerInfo {
    pub task_id: Option<String>,
    pub agent_name: String,
    pub prompt: String,
    pub started_at: std::time::Instant,
    pub context_tokens: usize,
}

static ACTIVE_WORKERS: std::sync::LazyLock<
    std::sync::RwLock<std::collections::BTreeMap<String, ActiveWorkerInfo>>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::BTreeMap::new()));

static WORKER_CONTEXT_TOKENS: std::sync::LazyLock<
    std::sync::RwLock<std::collections::BTreeMap<String, usize>>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::BTreeMap::new()));

/// RAII guard that automatically unregisters an active worker on drop.
pub struct ActiveWorkerGuard(pub String);

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = ACTIVE_WORKERS.write()
            && let Some(info) = map.remove(&self.0)
            && let Ok(mut last_map) = WORKER_CONTEXT_TOKENS.write()
        {
            last_map.insert(self.0.clone(), info.context_tokens);
        }
    }
}

/// Register a subagent worker as active with start timestamp and task prompt.
pub fn register_active_worker(
    task_id: Option<String>,
    agent_name: String,
    prompt: String,
) -> ActiveWorkerGuard {
    let key = if let Some(ref t) = task_id {
        if !t.trim().is_empty() {
            format!("{agent_name}-{t}")
        } else {
            format!(
                "{agent_name}-{}",
                std::time::Instant::now().elapsed().as_nanos()
            )
        }
    } else {
        format!(
            "{agent_name}-{}",
            std::time::Instant::now().elapsed().as_nanos()
        )
    };

    if let Ok(mut map) = ACTIVE_WORKERS.write() {
        let initial_tokens = WORKER_CONTEXT_TOKENS
            .read()
            .ok()
            .and_then(|m| m.get(&key).copied())
            .unwrap_or(0);
        map.insert(
            key.clone(),
            ActiveWorkerInfo {
                task_id,
                agent_name,
                prompt,
                started_at: std::time::Instant::now(),
                context_tokens: initial_tokens,
            },
        );
    }
    ActiveWorkerGuard(key)
}

/// Update the active specialist worker's context token count.
pub fn update_active_worker_context(key: &str, tokens: usize) {
    if let Ok(mut map) = ACTIVE_WORKERS.write()
        && let Some(info) = map.get_mut(key)
    {
        info.context_tokens = tokens;
    }
    if let Ok(mut last_map) = WORKER_CONTEXT_TOKENS.write() {
        last_map.insert(key.to_string(), tokens);
    }
}

/// Get the context token count for an active specialist worker by its key (e.g. `coder-t-001`).
pub fn get_active_worker_tokens(key: &str) -> Option<usize> {
    if let Ok(map) = ACTIVE_WORKERS.read()
        && let Some(w) = map.get(key)
    {
        return Some(w.context_tokens);
    }
    if let Ok(map) = WORKER_CONTEXT_TOKENS.read()
        && let Some(&tokens) = map.get(key)
    {
        return Some(tokens);
    }
    None
}

/// Format the active specialist context tokens for display in the status bar.
/// Returns None if no specialist workers are active or context is 0.
pub fn get_active_specialist_context_str() -> Option<String> {
    let map = ACTIVE_WORKERS.read().ok()?;
    if map.is_empty() {
        return None;
    }
    let entries: Vec<String> = map
        .values()
        .filter(|w| w.context_tokens > 0)
        .map(|w| {
            let count_str = if w.context_tokens >= 1_000_000 {
                format!("{:.1}M", w.context_tokens as f64 / 1_000_000.0)
            } else if w.context_tokens >= 1_000 {
                format!("{:.1}k", w.context_tokens as f64 / 1_000.0)
            } else {
                format!("{}", w.context_tokens)
            };
            if let Some(ref tid) = w.task_id {
                format!("{}-{}: {}", w.agent_name, tid, count_str)
            } else {
                format!("{}: {}", w.agent_name, count_str)
            }
        })
        .collect();

    if entries.is_empty() {
        None
    } else {
        Some(entries.join(", "))
    }
}

/// Returns true if there are currently any active background specialist workers.
pub fn has_active_workers() -> bool {
    let Ok(map) = ACTIVE_WORKERS.read() else {
        return false;
    };
    !map.is_empty()
}

/// Helper to format elapsed durations into human-readable minutes and seconds (e.g. "2m 15s" or "45s").
pub fn format_duration_human(secs: u64) -> String {
    let mins = secs / 60;
    let rem_secs = secs % 60;
    if mins == 0 {
        format!("{rem_secs}s")
    } else {
        format!("{mins}m {rem_secs}s")
    }
}

/// Formats all currently active subagent workers with their tool call ID, prompt, and running time.
pub fn get_active_subtasks_str() -> String {
    let Ok(map) = ACTIVE_WORKERS.read() else {
        return "None".to_string();
    };
    if map.is_empty() {
        return "None".to_string();
    }
    let mut out = String::new();
    for (id, info) in map.iter() {
        let elapsed_secs = info.started_at.elapsed().as_secs();
        let duration_str = format_duration_human(elapsed_secs);
        let task_id_str = info.task_id.as_deref().unwrap_or(id);
        out.push_str(&format!(
            "- Tool Call ID: {}\n  Subagent: {}\n  Task Prompt: {}\n  Running For: {} ({elapsed_secs} total seconds)\n\n",
            task_id_str, info.agent_name, info.prompt, duration_str
        ));
    }
    out
}

/// Dynamically parses the markdown execution plan and correlates each task with active background workers.
/// Generates real-time breakdown of Completed, Currently In Progress, and Pending steps matching Kvaser.
pub fn generate_plan_progress_summary(plan_content: &str) -> String {
    if plan_content.trim().is_empty() || plan_content == "None" {
        return "No active execution plan on disk.".to_string();
    }

    let mut completed_tasks = Vec::new();
    let mut in_progress_tasks = Vec::new();
    let mut pending_tasks = Vec::new();

    let re_id = regex::Regex::new(r"\b(t-[a-zA-Z0-9_\-]+)\b").unwrap();
    let re_checkbox_done = regex::Regex::new(r"\[[xX]\]").unwrap();
    let re_checkbox_pending = regex::Regex::new(r"\[\s*\]|\(\s*\)").unwrap();

    let active_map = ACTIVE_WORKERS.read().ok();

    for line in plan_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let is_done = re_checkbox_done.is_match(trimmed);
        let is_pending = re_checkbox_pending.is_match(trimmed)
            || (!is_done && re_id.is_match(trimmed) && !trimmed.starts_with('#'));

        if is_done {
            let clean_line = trimmed
                .trim_start_matches(|c: char| {
                    c == '-'
                        || c == '*'
                        || c == '+'
                        || c == '>'
                        || c == '|'
                        || c == ' '
                        || c == '.'
                        || c.is_ascii_digit()
                })
                .trim();
            completed_tasks.push(clean_line.to_string());
        } else if is_pending {
            let clean_line = trimmed
                .trim_start_matches(|c: char| {
                    c == '-'
                        || c == '*'
                        || c == '+'
                        || c == '>'
                        || c == '|'
                        || c == ' '
                        || c == '.'
                        || c.is_ascii_digit()
                })
                .trim();

            let matched_active = if let Some(cap) = re_id.captures(trimmed) {
                let tid = cap.get(1).unwrap().as_str().to_lowercase();
                active_map.as_ref().and_then(|map| {
                    map.iter().find(|(k, v)| {
                        v.task_id.as_deref().map(str::to_lowercase) == Some(tid.clone())
                            || k.to_lowercase().contains(&tid)
                            || v.prompt.to_lowercase().contains(&tid)
                    })
                })
            } else {
                None
            };

            if let Some((_k, info)) = matched_active {
                let running_time = format_duration_human(info.started_at.elapsed().as_secs());
                in_progress_tasks.push(format!(
                    "{} (Assigned to: {}, Running: {})",
                    clean_line, info.agent_name, running_time
                ));
            } else {
                pending_tasks.push(clean_line.to_string());
            }
        }
    }

    let total_tasks = completed_tasks.len() + in_progress_tasks.len() + pending_tasks.len();
    if total_tasks == 0 {
        return "Execution plan contains no checklist items ([ ] or [x]).".to_string();
    }

    let completion_pct = (completed_tasks.len() as f64 / total_tasks as f64 * 100.0).round() as u64;

    let mut summary = format!(
        "Overall Progress: {}/{} tasks completed ({}%)\n\n",
        completed_tasks.len(),
        total_tasks,
        completion_pct
    );

    if !completed_tasks.is_empty() {
        summary.push_str(&format!(
            "### Completed Steps ({}/{}):\n",
            completed_tasks.len(),
            total_tasks
        ));
        for task in &completed_tasks {
            summary.push_str(&format!("- {}\n", task));
        }
        summary.push('\n');
    }

    if !in_progress_tasks.is_empty() {
        summary.push_str(&format!(
            "### Currently In Progress ({}/{}):\n",
            in_progress_tasks.len(),
            total_tasks
        ));
        for task in &in_progress_tasks {
            summary.push_str(&format!("- {}\n", task));
        }
        summary.push('\n');
    }

    if !pending_tasks.is_empty() {
        summary.push_str(&format!(
            "### Pending Steps ({}/{}):\n",
            pending_tasks.len(),
            total_tasks
        ));
        for task in &pending_tasks {
            summary.push_str(&format!("- {}\n", task));
        }
    }

    summary.trim().to_string()
}

use crate::agent::phase::Plan;
use crate::config::Config;
use crate::harness::{HarnessStats, ToolError, ToolResult};
use crate::llm::ChatClient;
use crate::tool_names::TOOL_DELEGATE_TASK;
use anyhow::Result;
use std::sync::Arc;

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
        let _ = self.journal.clear(&snap.worker_id, true);
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
                let _ = self.plan.check_plan_on_marker(Some(t), &d.content);
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
mod tests {
    use super::*;
    use crate::agents::DelegationRequest;

    /// Build a manager rooted at a fresh temp plan dir for tests.
    fn test_manager(dir: &tempfile::TempDir) -> OrchestratorManager {
        OrchestratorManager::new(
            ChatClient::new("http://localhost:9999/v1", "test-model"),
            Plan::at(dir.path()),
            Arc::new(HarnessStats::new()),
        )
    }

    #[test]
    fn test_format_duration_human_minutes_and_seconds() {
        assert_eq!(format_duration_human(0), "0s");
        assert_eq!(format_duration_human(45), "45s");
        assert_eq!(format_duration_human(60), "1m 0s");
        assert_eq!(format_duration_human(135), "2m 15s");
        assert_eq!(format_duration_human(3665), "61m 5s");
    }

    /// REQ-ORCH-003: a delegated IsolatedContext contains ONLY the specialist's
    /// role system prompt + task brief + bounded snippets — never the Manager's
    /// `messages[]`. The produced context engine must start with exactly two
    /// messages (`[0]` = role system prompt, `[1]` = brief) and must not expose
    /// any Manager transcript or the Manager's own conversation history.
    #[tokio::test]
    async fn test_orchestr_context_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);

        // The Manager has a client with a backend URL and model; a well-behaved
        // orchestrator must never forward that (or any Manager transcript) into
        // a delegated subagent's isolated context.
        let req = DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "Implement the widget parser.".to_string(),
            snippets: vec!["src/widget.rs".to_string()],
            task_id: Some("t-101".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let d = m.delegate(req.clone()).await.expect("delegation succeeds");
        // The deliverable's content must reference the isolated role/brief and
        // never contain any "Manager transcript" (there is none to leak).
        assert!(matches!(d.marker, MissionMarker::Complete { .. }));
        assert!(d.content.contains("Implement the widget parser."));

        // Build the exact engine the specialist would receive from this request
        // and prove the isolation invariant at the message level.
        let entry = m.registry.resolve(req.agent_name).unwrap();
        let ctx = IsolatedContext::from_request(m.role_prompt_for(entry.agent), &req);
        let engine = ctx.into_engine(4096);
        let msgs = engine.messages();
        // Exactly two messages: the specialist's role system prompt and the brief.
        assert_eq!(msgs.len(), 2, "isolated context has exactly 2 messages");
        match &msgs[0] {
            crate::types::Message::System { content } => {
                assert!(
                    content.contains("Coder") || content.contains("Software Engineer"),
                    "messages[0] is the role system prompt"
                );
            }
            other => panic!("messages[0] must be a System role prompt, got {other:?}"),
        }
        match &msgs[1] {
            crate::types::Message::User { content } => {
                assert_eq!(content, "Implement the widget parser.");
            }
            other => panic!("messages[1] must be the brief, got {other:?}"),
        }
    }

    /// REQ-ORCH-005 / REQ-PLAN-002: MISSION COMPLETE (t-xxx) flips `[t-xxx]` →
    /// `[x]`; a FAILED marker leaves it unchecked.
    #[tokio::test]
    async fn test_orchestr_task_checkoff_complete_flips() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        m.create_plan("- [ ] [t-101] Build the parser.\n- [ ] [t-102] Test the parser.\n")
            .expect("plan written");

        let req = DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "Implement the parser.".to_string(),
            snippets: vec![],
            task_id: Some("t-101".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let _ = m.delegate(req).await.unwrap();
        let remaining = m.plan.pending_tasks();
        assert_eq!(remaining, vec!["t-102".to_string()]);
    }

    /// REQ-ORCH-005 / REQ-PLAN-002: a FAILED terminal marker leaves the item
    /// unchecked.
    #[tokio::test]
    async fn test_orchestr_task_checkoff_failed_leaves_unchecked() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        m.create_plan("- [ ] [t-200] Do the thing.\n").unwrap();

        // Simulate a deliverable that parses to FAILED.
        let d = Deliverable {
            marker: MissionMarker::Failed {
                reason: "blocked".to_string(),
            },
            content: "FAILED: blocked".to_string(),
            task_id: Some("t-200".to_string()),
        };
        let d = m.apply_check_off(d, Some("t-200".to_string()));
        assert_eq!(d.task_id.as_deref(), Some("t-200"));
        assert!(m.plan.pending_tasks().contains(&"t-200".to_string()));
    }

    /// REQ-ORCH-005 / REQ-PLAN-002: a `REPLAN REQUIRED` terminal marker also
    /// leaves the plan item unchecked — only `MISSION COMPLETE` flips `[ ]`→`[x]`.
    #[tokio::test]
    async fn test_orchestr_task_checkoff_replan_leaves_unchecked() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        m.create_plan("- [ ] [t-300] Re-architect the module.\n")
            .unwrap();

        // Simulate a deliverable that parses to REPLAN REQUIRED.
        let d = Deliverable {
            marker: MissionMarker::Replan {
                reason: "goal needs revisiting".to_string(),
            },
            content: "REPLAN REQUIRED: schema changed".to_string(),
            task_id: Some("t-300".to_string()),
        };
        let d = m.apply_check_off(d, Some("t-300".to_string()));
        assert_eq!(d.task_id.as_deref(), Some("t-300"));
        assert!(
            m.plan.pending_tasks().contains(&"t-300".to_string()),
            "REPLAN REQUIRED must leave the item unchecked"
        );
    }

    /// t-302 (a): `apply_check_off` leaves the task UNCHECKED even when the
    /// content body contains the literal string `MISSION COMPLETE`, as long as
    /// the authoritative `Deliverable.marker` is `Failed` or `Replan`. The
    /// marker — not the free-form body — is the gate keeper.
    #[tokio::test]
    async fn test_orchestr_apply_checkoff_marker_failed_overrides_body_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        let plan = "- [ ] [t-401] Step one\n- [ ] [t-402] Step two\n";
        m.create_plan(plan).unwrap();

        // Marker FAILED, but the body leaks a stale MISSION COMPLETE token from a
        // pre-validation draft. This must NOT check the task off.
        let d = Deliverable {
            marker: MissionMarker::Failed {
                reason: "validator rejected".to_string(),
            },
            content: "MISSION COMPLETE (t-401) — actually the validator rejected this.".to_string(),
            task_id: Some("t-401".to_string()),
        };
        let d = m.apply_check_off(d, Some("t-401".to_string()));
        assert_eq!(d.task_id.as_deref(), Some("t-401"));
        assert!(
            m.plan.pending_tasks().contains(&"t-401".to_string()),
            "Failed marker with stale MISSION COMPLETE body must stay unchecked"
        );

        // Same invariant for the Replan marker.
        let d2 = Deliverable {
            marker: MissionMarker::Replan {
                reason: "goal changed".to_string(),
            },
            content: "MISSION COMPLETE (t-402) — just a stale draft.".to_string(),
            task_id: Some("t-402".to_string()),
        };
        let d2 = m.apply_check_off(d2, Some("t-402".to_string()));
        assert_eq!(d2.task_id.as_deref(), Some("t-402"));
        assert!(
            m.plan.pending_tasks().contains(&"t-402".to_string()),
            "Replan marker with stale MISSION COMPLETE body must stay unchecked"
        );
    }

    /// t-302 (b): `apply_check_off` checks off a task ONLY when the deliverable
    /// carries a genuine `MissionMarker::Complete` whose content body also
    /// retains the `MISSION COMPLETE` terminal marker.
    #[tokio::test]
    async fn test_orchestr_apply_checkoff_only_checks_on_genuine_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        m.create_plan("- [ ] [t-501] Step one\n- [ ] [t-502] Step two\n")
            .unwrap();

        // Genuine Complete marker + body marker -> checked off.
        let d = Deliverable {
            marker: MissionMarker::Complete { task_id: None },
            content: "all done MISSION COMPLETE (t-501)".to_string(),
            task_id: Some("t-501".to_string()),
        };
        let d = m.apply_check_off(d, Some("t-501".to_string()));
        assert_eq!(d.task_id.as_deref(), Some("t-501"));
        assert!(
            !m.plan.pending_tasks().contains(&"t-501".to_string()),
            "genuine Complete must check t-501 off"
        );

        // A Complete marker whose content was later revoked (stale) must NOT
        // check off, because check_plan_on_marker re-parses the *content*.
        let d2 = Deliverable {
            marker: MissionMarker::Complete { task_id: None },
            content: "REVOKED before finalization".to_string(),
            task_id: Some("t-502".to_string()),
        };
        let d2 = m.apply_check_off(d2, Some("t-502".to_string()));
        assert_eq!(d2.task_id.as_deref(), Some("t-502"));
        assert!(
            m.plan.pending_tasks().contains(&"t-502".to_string()),
            "Complete marker without a content-side completion token must not check off"
        );
    }

    /// t-302 (c): a REJECTED deliverable's content retains the validator critique
    /// in a `VALIDATOR REJECTION` block, so the downstream consumer can act on it.
    #[tokio::test]
    async fn test_orchestr_apply_checkoff_rejected_retains_critique_content() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        m.create_plan("- [ ] [t-601] Verify module\n").unwrap();

        // A REJECTED deliverable: Failed marker, body carries the structured
        // VALIDATOR REJECTION block produced by the validator feedback loop.
        let critique = "VALIDATOR REJECTION: assertions failed on line 12\n---------------\nlast revision body MISSION COMPLETE REVOKED";
        let d = Deliverable {
            marker: MissionMarker::Failed {
                reason: "validator rejected".to_string(),
            },
            content: critique.to_string(),
            task_id: Some("t-601".to_string()),
        };
        let d = m.apply_check_off(d, Some("t-601".to_string()));
        // The returned deliverable retains the FULL critique block verbatim.
        assert!(d.content.contains("VALIDATOR REJECTION"));
        assert!(d.content.contains("assertions failed on line 12"));
        // And the REJECTED marker leaves the task unchecked (parity with t-302 a).
        assert!(m.plan.pending_tasks().contains(&"t-601".to_string()));
    }

    /// REQ-ORCH-001 fractal: nested delegation beyond max_recursion_depth is
    /// rejected.
    #[tokio::test]
    async fn test_orchestr_fractal_depth_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = test_manager(&tmp);
        m.orchestration.max_recursion_depth = 3;
        // Descend three levels (0→1→2→3 is allowed); the fourth (depth 3 + 1)
        // must be rejected because step(3) with max 3 returns None.
        let mut depth = RecursionDepth::root();
        for _ in 0..3 {
            depth = depth.step(m.orchestration.max_recursion_depth).unwrap();
        }
        assert_eq!(depth.0, 3);
        assert!(depth.step(m.orchestration.max_recursion_depth).is_none());
        m.depth = depth;
        let req = DelegationRequest {
            agent_name: Agent::Generalist,
            prompt: "nested".to_string(),
            snippets: vec![],
            task_id: None,
            image_urls: None,
            audio_urls: None,
            recursion_granted: true,
        };
        let res = m.delegate(req).await;
        assert!(res.is_err());
        let err = res.err().unwrap().to_string();
        assert!(err.contains("exceeds max") || err.contains("recursion"));
    }

    #[test]
    fn test_orchestr_recursion_depth_step_boundary() {
        assert_eq!(RecursionDepth::root().step(3), Some(RecursionDepth(1)));
        let d = RecursionDepth(3);
        assert_eq!(d.step(3), None);
    }

    #[test]
    fn test_orchestr_synthesize_joins_deliverables() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        let a = Deliverable {
            marker: MissionMarker::Complete { task_id: None },
            content: "first".to_string(),
            task_id: None,
        };
        let b = Deliverable {
            marker: MissionMarker::Complete { task_id: None },
            content: "second".to_string(),
            task_id: None,
        };
        let out = m.synthesize(&[a, b]);
        assert!(out.contains("first"));
        assert!(out.contains("second"));
    }

    /// Deep-Freeze: `delegate()` snapshots the in-flight delegation to the
    /// Crash Journal and clears it once the worker returns (SPEC §3.4). After a
    /// clean run there is nothing frozen left on disk.
    #[tokio::test]
    async fn test_orchestr_delegate_snapshots_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        let req = DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "Build the parser.".to_string(),
            snippets: vec![],
            task_id: None,
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        assert!(!m.journal.is_frozen());
        let d = m.delegate(req).await.expect("delegation succeeds");
        assert!(matches!(d.marker, MissionMarker::Complete { .. }));
        // Clean termination leaves no frozen checkpoint behind.
        assert!(!m.journal.is_frozen());
        // The journal logged at least a Frozen + Resolved pair.
        let log = m.journal.journal().unwrap();
        assert!(log.iter().any(|e| e.kind == JournalEventKind::Frozen));
        assert!(log.iter().any(|e| e.kind == JournalEventKind::Resolved));
    }

    /// Deep-Freeze recovery: a manually frozen delegation is rehydrated by
    /// `recover_frozen()` using the identical worker_id and preserved sub_req.
    #[tokio::test]
    async fn test_orchestr_recover_frozen_rehydrates_identical_worker() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        let req = DelegationRequest {
            agent_name: Agent::Generalist,
            prompt: "Resume the analysis.".to_string(),
            snippets: vec!["notes.md".to_string()],
            task_id: Some("t-777".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        // Simulate a crash: freeze the delegation by hand (as `delegate()` would
        // at start) and do NOT clear — as if the process died mid-run.
        let worker_id = m
            .journal
            .snapshot(Agent::Generalist, &req)
            .expect("snapshot written");
        assert!(m.journal.is_frozen());

        // Rehydrate on a "restarted" Manager rooted at the same plan dir.
        let m2 = test_manager(&tmp);
        let recovered = m2
            .recover_frozen()
            .await
            .expect("recovery succeeds")
            .expect("a frozen delegation existed");
        assert!(matches!(recovered.marker, MissionMarker::Complete { .. }));
        // The preserved brief is what was re-executed.
        assert!(recovered.content.contains("Resume the analysis."));
        // The frozen checkpoint was released after rehydration.
        assert!(!m2.journal.is_frozen());
        let _ = worker_id;
    }

    /// Deep-Freeze recovery: with nothing frozen, `recover_frozen()` is a clean
    /// no-op (returns `None`).
    #[tokio::test]
    async fn test_orchestr_recover_frozen_none_when_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        assert!(!m.journal.is_frozen());
        let res = m.recover_frozen().await.expect("no error on clean boot");
        assert!(res.is_none());
    }

    /// Deep-Freeze recovery: a frozen delegation whose agent is no longer
    /// registered fails loudly (the frozen task is marked Failed, not silently
    /// dropped), satisfying "rehydrate OR properly fail".
    #[tokio::test]
    async fn test_orchestr_recover_frozen_fails_when_role_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        // Freeze a delegation into the journal dir directly.
        let m = test_manager(&tmp);
        let req = DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "orphan task".to_string(),
            snippets: vec![],
            task_id: None,
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let wid = m.journal.snapshot(Agent::Coder, &req).unwrap();

        // A manager with an EMPTY registry cannot rehydrate the frozen role.
        let mut m2 = test_manager(&tmp);
        m2.registry = SpecialistRegistry::default();
        let err = m2
            .recover_frozen()
            .await
            .expect_err("fails loudly when role unknown");
        assert!(err.to_string().contains("cannot be rehydrated"));
        // The frozen checkpoint is released with a Failed journal event.
        assert!(!m2.journal.is_frozen());
        assert!(
            m2.journal
                .journal()
                .unwrap()
                .iter()
                .any(|e| e.kind == JournalEventKind::Failed && e.worker_id == wid)
        );
    }

    // --- REQ-ORCH-005: `handle_delegate_task` handler-level tests ---

    /// REQ-ORCH-005 canonical signature: `handle_delegate_task` accepts the full
    /// payload `(agent_name, prompt, snippets, task_id?, image_urls?, audio_urls?)`
    /// where `agent_name` is the snake_case enum. It returns the specialist's
    /// deliverable as a `ToolResult` whose content carries the `MISSION COMPLETE
    /// (task-id)` terminal marker, and is synchronous-from-Manager (blocks until
    /// the specialist returns — this call itself completes in-line).
    #[test]
    fn test_orchestr_handler_full_signature_and_success_marker() {
        let args = serde_json::json!({
            "agent_name": "coder",
            "prompt": "Implement the widget parser.",
            "snippets": ["src/widget.rs"],
            "task_id": "t-500",
            "image_urls": ["marmennill-media://diagram.png"],
            "audio_urls": ["marmennill-media://note.wav"],
        });
        let result = handle_delegate_task(&args).expect("handler succeeds");
        assert!(!result.is_error, "MISSION COMPLETE is a success result");
        assert!(result.content.contains("MISSION COMPLETE (t-500)"));
        assert!(result.content.contains("Implement the widget parser."));
    }

    /// REQ-ORCH-002: an unknown `agent_name` is rejected at parse time with a
    /// `BadArguments` ToolError (the snake_case enum rejects it), never a panic.
    #[test]
    fn test_handler_rejects_unknown_agent() {
        let args = serde_json::json!({
            "agent_name": "planner",
            "prompt": "Nope.",
            "snippets": [],
        });
        let err = handle_delegate_task(&args).expect_err("unknown role rejected");
        assert!(matches!(err, ToolError::BadArguments { .. }));
    }

    /// REQ-ORCH-003/005: the brief MUST be self-contained and non-empty — a
    /// blank prompt is rejected (one task per call; the subagent cannot see the
    /// Manager's context, so the brief must stand alone).
    #[test]
    fn test_handler_rejects_empty_prompt() {
        let args = serde_json::json!({
            "agent_name": "coder",
            "prompt": "   ",
            "snippets": [],
        });
        let err = handle_delegate_task(&args).expect_err("empty prompt rejected");
        assert!(matches!(err, ToolError::BadArguments { .. }));
    }

    /// t6-REQ-1 / t6-REQ-2: `OrchestrationConfig::from_config` hydrates the
    /// runtime orchestration config (recursion bound + manager module + tool
    /// table) from the loaded `[orchestration]` TOML block, and
    /// `OrchestratorManager::from_config` threads it through.
    #[test]
    fn test_orchestr_config_threads_into_manager() {
        use crate::config::Config;
        let mut cfg = Config::default();
        cfg.orchestration.max_recursion_depth = 5;
        cfg.orchestration.manager_module = "src/orchestrator/mod.rs".to_string();
        cfg.orchestration.specialists.insert(
            "coder".to_string(),
            crate::config::SpecialistConfig {
                module: "src/agents/coder.rs".to_string(),
                tools: vec![TOOL_DELEGATE_TASK.into(), "terminal__*".into()],
                model: None,
                ..Default::default()
            },
        );

        let tmp = tempfile::tempdir().unwrap();
        let m = OrchestratorManager::from_config(
            ChatClient::new("http://localhost:9999/v1", "test-model"),
            Plan::at(tmp.path()),
            Arc::new(HarnessStats::new()),
            &cfg,
        );
        assert_eq!(m.orchestration.max_recursion_depth, 5);
        assert_eq!(m.orchestration.manager_module, "src/orchestrator/mod.rs");
        assert_eq!(m.orchestration.specialists.get("coder").unwrap().len(), 2);
    }

    #[test]
    fn test_orchestr_guard_rejects_domain_module() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = test_manager(&tmp);
        m.orchestration.manager_module = "src/agents/coder.rs".to_string();
        let err = m.guard_no_domain_work().expect_err("agent module rejected");
        assert!(err.to_string().contains("domain"));

        // Correct orchestrator module passes.
        m.orchestration.manager_module = "src/orchestrator/mod.rs".to_string();
        m.guard_no_domain_work()
            .expect("orchestrator module is fine");
    }

    #[tokio::test]
    async fn test_orchestr_run_executing_rejects_domain_module() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = test_manager(&tmp);
        m.orchestration.manager_module = "src/agents/researcher.rs".to_string();
        m.create_plan("- [ ] [t-101] Research the topic.\n")
            .unwrap();
        let res = m.run_executing(&|_| Agent::Researcher).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_orchestr_delegation_events_emitted() {
        let tmp = tempfile::tempdir().unwrap();
        let m = test_manager(&tmp);
        let req = DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "Implement the widget.".to_string(),
            snippets: vec![],
            task_id: Some("t-77".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let _ = m.delegate(req).await.unwrap();
        let events = m.delegation_events.lock().unwrap().clone();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0],
            DelegationEvent::Started { agent: Agent::Coder, task: Some(t) } if t == "t-77"));
        assert!(matches!(&events[1],
            DelegationEvent::Completed { agent: Agent::Coder, task: Some(t) } if t == "t-77"));
    }

    #[test]
    fn test_active_specialist_context_formatting() {
        let guard = register_active_worker(
            Some("t-123".to_string()),
            "coder".to_string(),
            "Do coding".to_string(),
        );
        update_active_worker_context(&guard.0, 3450);
        let formatted = get_active_specialist_context_str();
        assert!(
            formatted
                .as_deref()
                .is_some_and(|s| s.contains("coder-t-123: 3.5k"))
        );
        assert_eq!(get_active_worker_tokens("coder-t-123"), Some(3450));
        drop(guard);
        // Last known tokens are preserved after drop for Idle subagent rendering
        assert_eq!(get_active_worker_tokens("coder-t-123"), Some(3450));
    }

    #[test]
    fn test_active_worker_context_tokens_rebirth_reduction() {
        let guard = register_active_worker(
            Some("t-456".to_string()),
            "coder".to_string(),
            "Do large work".to_string(),
        );
        // Before rebirth: large context
        update_active_worker_context(&guard.0, 8500);
        assert_eq!(get_active_worker_tokens("coder-t-456"), Some(8500));

        // After rebirth or compaction: context count drops
        update_active_worker_context(&guard.0, 450);
        assert_eq!(get_active_worker_tokens("coder-t-456"), Some(450));
    }
}
