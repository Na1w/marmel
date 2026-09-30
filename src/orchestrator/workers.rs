//! Active specialist workers registry, context token tracking, and RAII guards.
//!
//! All worker state lives in a single sharded [`DashMap`] (`WORKERS`). A worker
//! key survives completion: on drop the entry is flipped to a "completed"
//! phase in place (preserving the last-seen context token count), and a
//! bounded 10-entry completed cap evicts the oldest completed entry. No
//! function in this module ever holds more than one map guard at a time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Instant;

use dashmap::DashMap;

/// Information about a specialist worker currently executing a task.
#[derive(Debug, Clone)]
pub struct ActiveWorkerInfo {
    pub task_id: Option<String>,
    pub agent_name: String,
    pub prompt: String,
    pub started_at: Instant,
    pub started_wall: chrono::DateTime<chrono::Local>,
    pub context_tokens: usize,
    pub implementation_turns: usize,
    pub validation_rounds: usize,
    pub latest_validator_feedback: Option<String>,
    pub status: String,
    pub cancel_token: Option<tokio_util::sync::CancellationToken>,
}

/// Information about a recently completed specialist task.
#[derive(Debug, Clone)]
pub struct CompletedWorkerInfo {
    pub task_id: Option<String>,
    pub agent_name: String,
    pub prompt: String,
    pub started_at: Instant,
    pub started_wall: chrono::DateTime<chrono::Local>,
    pub completed_at: Instant,
    pub completed_wall: chrono::DateTime<chrono::Local>,
    pub duration: std::time::Duration,
    pub implementation_turns: usize,
    pub validation_rounds: usize,
    pub latest_validator_feedback: Option<String>,
    pub status: String,
}

/// Merged per-worker state: the active worker info plus the persisted
/// after-completion context token count and completion timestamps.
#[derive(Debug, Clone)]
pub struct WorkerState {
    pub info: ActiveWorkerInfo,
    /// Last-seen context token count. Always in sync with
    /// [`WorkerState::info.context_tokens`] while the worker is active, and
    /// preserved after completion so `get_active_worker_tokens` keeps
    /// returning the last value for recently-finished workers.
    pub last_tokens: usize,
    /// Monotonic completion sequence (None while active); used to order the
    /// recently-completed list and to evict the oldest completed entry.
    pub completed_seq: Option<u64>,
    pub completed_at: Option<Instant>,
    pub completed_wall: Option<chrono::DateTime<chrono::Local>>,
}

impl WorkerState {
    /// True if this worker is still active (not yet completed).
    pub fn is_active(&self) -> bool {
        self.completed_seq.is_none()
    }

    /// Project this state into the legacy completed-worker view.
    pub fn completed_info(&self) -> Option<CompletedWorkerInfo> {
        let completed_at = self.completed_at?;
        Some(CompletedWorkerInfo {
            task_id: self.info.task_id.clone(),
            agent_name: self.info.agent_name.clone(),
            prompt: self.info.prompt.clone(),
            started_at: self.info.started_at,
            started_wall: self.info.started_wall,
            completed_at,
            completed_wall: self.completed_wall?,
            duration: completed_at - self.info.started_at,
            implementation_turns: self.info.implementation_turns,
            validation_rounds: self.info.validation_rounds,
            latest_validator_feedback: self.info.latest_validator_feedback.clone(),
            status: self.info.status.clone(),
        })
    }
}

/// Bound on how many completed worker entries survive in the map.
const MAX_RECENT_COMPLETED: usize = 10;

/// Single sharded map holding every worker (active and recently completed).
static WORKERS: LazyLock<DashMap<String, WorkerState>> = LazyLock::new(DashMap::new);

/// Monotonic sequence handed out to completed entries (oldest = smallest).
static COMPLETED_SEQ: LazyLock<std::sync::atomic::AtomicU64> =
    LazyLock::new(|| AtomicU64::new(0));

#[cfg(test)]
pub static TEST_WORKERS_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

/// RAII guard that automatically unregisters an active worker on drop and moves it to recently completed.
pub struct ActiveWorkerGuard(pub String);

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        // Single entry mutation: flip this worker to completed in place,
        // preserving the key and its last-seen token count.
        let seq = COMPLETED_SEQ.fetch_add(1, Ordering::SeqCst);
        if let Some(mut entry) = WORKERS.get_mut(&self.0) {
            entry.last_tokens = entry.info.context_tokens;
            entry.completed_seq = Some(seq);
            entry.completed_at = Some(Instant::now());
            entry.completed_wall = Some(chrono::Local::now());
        }
        // Bounded completed ring: evict oldest completed entries while over cap.
        loop {
            let mut completed: Vec<(String, u64)> = WORKERS
                .iter()
                .filter_map(|e| e.value().completed_seq.map(|s| (e.key().clone(), s)))
                .collect();
            if completed.len() <= MAX_RECENT_COMPLETED {
                break;
            }
            completed.sort_by_key(|(_, s)| *s);
            let to_remove = completed.len().saturating_sub(MAX_RECENT_COMPLETED);
            let mut removed_any = false;
            for (key, _) in completed.into_iter().take(to_remove) {
                if WORKERS.remove(&key).is_some() {
                    removed_any = true;
                }
            }
            if !removed_any {
                break;
            }
        }
    }
}

/// Register a subagent worker as active with start timestamp and task prompt.
pub fn register_active_worker(
    task_id: Option<String>,
    agent_name: String,
    prompt: String,
) -> ActiveWorkerGuard {
    register_active_worker_with_token(task_id, agent_name, prompt, None)
}

/// Register a subagent worker as active with start timestamp, task prompt, and optional cancellation token.
pub fn register_active_worker_with_token(
    task_id: Option<String>,
    agent_name: String,
    prompt: String,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
) -> ActiveWorkerGuard {
    let clean_task_id = task_id
        .as_deref()
        .map(|t| {
            t.trim_matches(|c| {
                c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
            })
            .trim()
            .to_string()
        })
        .filter(|t| !t.is_empty());

    let key = if let Some(ref t) = clean_task_id {
        format!("{agent_name}-{t}")
    } else {
        format!("{agent_name}-{}", Instant::now().elapsed().as_nanos())
    };

    let effective_token = cancel_token
        .or_else(|| Some(crate::orchestrator::bus::global_cancellation_token().child_token()));

    // Reuse the last-seen token count for a previously completed worker with
    // the same key (single per-entry lookup, released before the insert).
    let initial_tokens = WORKERS.get(&key).map(|e| e.last_tokens).unwrap_or(0);

    WORKERS.insert(
        key.clone(),
        WorkerState {
            info: ActiveWorkerInfo {
                task_id: clean_task_id,
                agent_name,
                prompt,
                started_at: Instant::now(),
                started_wall: chrono::Local::now(),
                context_tokens: initial_tokens,
                implementation_turns: 0,
                validation_rounds: 0,
                latest_validator_feedback: None,
                status: "In Progress".to_string(),
                cancel_token: effective_token,
            },
            last_tokens: initial_tokens,
            completed_seq: None,
            completed_at: None,
            completed_wall: None,
        },
    );
    ActiveWorkerGuard(key)
}

/// Update the active specialist worker's context token count.
pub fn update_active_worker_context(key: &str, tokens: usize) {
    if let Some(mut entry) = WORKERS.get_mut(key) {
        entry.last_tokens = tokens;
        if entry.is_active() {
            entry.info.context_tokens = tokens;
        }
    } else {
        // Preserve the legacy behavior of persisting the token count for keys
        // with no live entry (e.g. updated after completion or eviction).
        WORKERS.insert(
            key.to_string(),
            WorkerState {
                info: ActiveWorkerInfo {
                    task_id: None,
                    agent_name: key.to_string(),
                    prompt: String::new(),
                    started_at: Instant::now(),
                    started_wall: chrono::Local::now(),
                    context_tokens: tokens,
                    implementation_turns: 0,
                    validation_rounds: 0,
                    latest_validator_feedback: None,
                    status: "Completed".to_string(),
                    cancel_token: None,
                },
                last_tokens: tokens,
                completed_seq: Some(COMPLETED_SEQ.fetch_add(1, Ordering::SeqCst)),
                completed_at: Some(Instant::now()),
                completed_wall: Some(chrono::Local::now()),
            },
        );
    }
}

/// Update the active specialist worker's implementation turn, validation rounds, and latest validator critique/feedback.
pub fn update_active_worker_progress(
    key: &str,
    turns: usize,
    val_rounds: usize,
    feedback: Option<String>,
) {
    if let Some(mut entry) = WORKERS.get_mut(key)
        && entry.is_active()
    {
        entry.info.implementation_turns = turns;
        entry.info.validation_rounds = val_rounds;
        if let Some(fb) = feedback {
            entry.info.latest_validator_feedback = Some(fb);
        }
    }
}

/// Set the descriptive status of an active specialist worker (e.g. "Approved", "Revising", "Failed", "Aborted").
pub fn set_active_worker_status(key: &str, status: &str) {
    if let Some(mut entry) = WORKERS.get_mut(key)
        && entry.is_active()
    {
        entry.info.status = status.to_string();
    }
}

/// Get the context token count for an active specialist worker by its key (e.g. `coder-t-001`).
pub fn get_active_worker_tokens(key: &str) -> Option<usize> {
    WORKERS.get(key).map(|e| e.last_tokens)
}

/// Format the active specialist context tokens for display in the status bar.
/// Returns None if no specialist workers are active or context is 0.
pub fn get_active_specialist_context_str() -> Option<String> {
    let mut active: Vec<(String, WorkerState)> = WORKERS
        .iter()
        .filter(|e| e.value().is_active())
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    active.sort_by(|a, b| a.0.cmp(&b.0));
    if active.is_empty() {
        return None;
    }
    let entries: Vec<String> = active
        .iter()
        .filter(|(_, w)| w.info.context_tokens > 0)
        .map(|(_, w)| {
            let count_str = if w.info.context_tokens >= 1_000_000 {
                format!("{:.1}M", w.info.context_tokens as f64 / 1_000_000.0)
            } else if w.info.context_tokens >= 1_000 {
                format!("{:.1}k", w.info.context_tokens as f64 / 1_000.0)
            } else {
                format!("{}", w.info.context_tokens)
            };
            if let Some(ref tid) = w.info.task_id {
                format!("{}-{}: {}", w.info.agent_name, tid, count_str)
            } else {
                format!("{}: {}", w.info.agent_name, count_str)
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
    WORKERS.iter().any(|e| e.value().is_active())
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

/// Formats all currently active and recently completed subagent workers with their tool call ID, prompt, running time,
/// implementation turns, validation rounds, and latest validator feedback.
pub fn get_active_subtasks_str() -> String {
    // Active workers, in key order (matches the previous BTreeMap ordering).
    let mut active: Vec<(String, WorkerState)> = WORKERS
        .iter()
        .filter(|e| e.value().is_active())
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    active.sort_by(|a, b| a.0.cmp(&b.0));

    // Recently completed workers, most recent first (matches the previous
    // Vec push-order rendered in reverse).
    let mut completed: Vec<(String, WorkerState)> = WORKERS
        .iter()
        .filter(|e| !e.value().is_active())
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    completed.sort_by(|a, b| {
        b.1
            .completed_seq
            .unwrap_or(u64::MAX)
            .cmp(&a.1.completed_seq.unwrap_or(u64::MAX))
    });

    if active.is_empty() && completed.is_empty() {
        return "None".to_string();
    }

    let mut out = String::new();
    if !active.is_empty() {
        out.push_str("Active Background Subagents:\n");
        for (id, state) in &active {
            let info = &state.info;
            let elapsed_secs = info.started_at.elapsed().as_secs();
            let duration_str = format_duration_human(elapsed_secs);
            let task_id_str = info.task_id.as_deref().unwrap_or(id);
            let start_wall_str = info.started_wall.format("%H:%M:%S");
            out.push_str(&format!(
                "- Tool Call ID: {}\n  Subagent Tag: {}\n  Subagent: {}\n  Status: {}\n  Task Prompt: {}\n  Started At: {} (running for {}, {elapsed_secs} total seconds)\n  Implementation Turns: {}\n  Validation Rounds: {}\n",
                task_id_str, id, info.agent_name, info.status, info.prompt, start_wall_str,
                duration_str, info.implementation_turns, info.validation_rounds
            ));
            let clean_tid = info.task_id.as_deref().map(|t| {
                t.trim_matches(|c| {
                    c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
                })
                .trim()
            });
            if let Some(tid) = clean_tid {
                let prompt_file = crate::harness::get_workspace_root()
                    .join(crate::manager::phase::MARMEL_DIR)
                    .join("prompts")
                    .join(format!("{tid}.md"));
                if let Ok(bp) = crate::agents::AgentBlueprint::load_from_disk(&prompt_file) {
                    out.push_str(&format!(
                        "  Assigned Role: {}\n  Allowed Tools: {}\n  Assigned Skills: {}\n",
                        bp.role_name,
                        bp.allowed_tools.join(", "),
                        if bp.selected_skills.is_empty() {
                            "None".to_string()
                        } else {
                            bp.selected_skills.join(", ")
                        }
                    ));
                }
            }
            if let Some(ref fb) = info.latest_validator_feedback {
                let trimmed = fb.trim();
                let summary = if trimmed.len() > 300 {
                    let cut = trimmed.floor_char_boundary(297);
                    format!("{}...", &trimmed[..cut])
                } else {
                    trimmed.to_string()
                };
                out.push_str(&format!(
                    "  Latest Validator Feedback: \"{}\"\n",
                    summary.replace('\n', " ")
                ));
            } else {
                out.push_str("  Latest Validator Feedback: None\n");
            }
            out.push('\n');
        }
    }

    if !completed.is_empty() {
        out.push_str("Recently Completed Subagents:\n");
        for (_id, state) in &completed {
            let info = &state.info;
            let duration_str = format_duration_human(
                state
                    .completed_at
                    .map(|c| c - info.started_at)
                    .unwrap_or_default()
                    .as_secs(),
            );
            let task_id_str = info.task_id.as_deref().unwrap_or(&info.agent_name);
            let start_wall_str = info.started_wall.format("%H:%M:%S");
            let finish_wall_str = state
                .completed_wall
                .unwrap_or(info.started_wall)
                .format("%H:%M:%S");
            out.push_str(&format!(
                "- Tool Call ID: {}\n  Subagent: {}\n  Status: {}\n  Task Prompt: {}\n  Started At: {}\n  Finished At: {} (total duration: {})\n  Implementation Turns: {}\n  Validation Rounds: {}\n",
                task_id_str, info.agent_name, info.status, info.prompt, start_wall_str, finish_wall_str, duration_str, info.implementation_turns, info.validation_rounds
            ));
            if let Some(ref fb) = info.latest_validator_feedback {
                let trimmed = fb.trim();
                let summary = if trimmed.len() > 300 {
                    let cut = trimmed.floor_char_boundary(297);
                    format!("{}...", &trimmed[..cut])
                } else {
                    trimmed.to_string()
                };
                out.push_str(&format!(
                    "  Latest Validator Feedback: \"{}\"\n",
                    summary.replace('\n', " ")
                ));
            } else {
                out.push_str("  Latest Validator Feedback: None\n");
            }
            out.push('\n');
        }
    }

    out
}

/// Helper to find an active worker matching a given task id substring.
pub fn get_active_subtask_by_id(task_id: &str) -> Option<(String, String)> {
    let tid = task_id.to_lowercase();
    let mut active: Vec<(String, WorkerState)> = WORKERS
        .iter()
        .filter(|e| e.value().is_active())
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    active.sort_by(|a, b| a.0.cmp(&b.0));
    let (_key, state) = active.iter().find(|(k, s)| {
        s.info
            .task_id
            .as_deref()
            .map(str::to_lowercase)
            .as_deref()
            == Some(tid.as_str())
            || k.to_lowercase().contains(&tid)
            || s.info.prompt.to_lowercase().contains(&tid)
    })?;
    let running_time = format_duration_human(state.info.started_at.elapsed().as_secs());
    Some((state.info.agent_name.clone(), running_time))
}

/// Helper to check if an active worker matches a target agent and/or task ID.
fn worker_matches(
    info: &ActiveWorkerInfo,
    key: &str,
    target_agent: Option<&str>,
    target_task: Option<&str>,
) -> bool {
    let clean_agent = target_agent.unwrap_or("").trim().to_ascii_lowercase();
    let clean_task = target_task
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\'')
        .trim()
        .to_ascii_lowercase();

    if clean_agent.is_empty() && clean_task.is_empty() {
        return false;
    }

    let key_lower = key.to_ascii_lowercase();
    let worker_agent = info.agent_name.trim().to_ascii_lowercase();
    let worker_task = info
        .task_id
        .as_deref()
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\'')
        .trim()
        .to_ascii_lowercase();

    let task_matches = !clean_task.is_empty()
        && (key_lower.contains(&clean_task)
            || (!worker_task.is_empty()
                && (worker_task == clean_task
                    || worker_task.contains(&clean_task)
                    || clean_task.contains(&worker_task)))
            || (!worker_agent.is_empty()
                && (worker_agent == clean_task
                    || worker_agent.contains(&clean_task)
                    || clean_task.contains(&worker_agent))));

    let agent_matches = !clean_agent.is_empty()
        && (key_lower.contains(&clean_agent)
            || worker_agent == clean_agent
            || worker_agent.contains(&clean_agent)
            || clean_agent.contains(&worker_agent));

    if !clean_task.is_empty() && !clean_agent.is_empty() {
        task_matches && agent_matches
    } else if !clean_task.is_empty() {
        task_matches
    } else if !clean_agent.is_empty() {
        agent_matches
    } else {
        false
    }
}

/// Cancel an active specialist worker matching target_agent and/or target_task_id.
/// Returns true if at least one matching active worker was found and cancelled.
pub fn cancel_active_worker(target_agent: Option<&str>, target_task_id: Option<&str>) -> bool {
    let mut to_cancel = Vec::new();
    for e in WORKERS.iter() {
        let key = e.key();
        let state = e.value();
        if state.is_active() && worker_matches(&state.info, key, target_agent, target_task_id) {
            to_cancel.push((key.clone(), state.info.cancel_token.clone()));
        }
    }

    let found = !to_cancel.is_empty();
    for (key, token_opt) in to_cancel {
        if let Some(token) = token_opt {
            token.cancel();
        }
        set_active_worker_status(&key, "Aborted");
        crate::orchestrator::emit_status(format!(
            "[Steering] Cancelled active specialist worker '{key}'"
        ));
    }
    found
}

/// Cancel all active specialist workers across the registry.
pub fn cancel_all_active_workers() -> usize {
    let mut to_cancel = Vec::new();
    for e in WORKERS.iter() {
        let key = e.key();
        let state = e.value();
        if state.is_active() {
            to_cancel.push((key.clone(), state.info.cancel_token.clone()));
        }
    }

    let count = to_cancel.len();
    for (key, token_opt) in to_cancel {
        if let Some(token) = token_opt {
            token.cancel();
        }
        set_active_worker_status(&key, "Aborted");
        crate::orchestrator::emit_status(format!(
            "[Steering] Aborted active specialist worker '{key}'"
        ));
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_progress_and_completed_history() {
        let guard = register_active_worker(
            Some("task-test-1".to_string()),
            "coder".to_string(),
            "Write parser tests".to_string(),
        );

        update_active_worker_progress(&guard.0, 3, 1, Some("Missing edge case".to_string()));
        set_active_worker_status(&guard.0, "Revising");

        let status_str = get_active_subtasks_str();
        assert!(status_str.contains("Active Background Subagents:"));
        assert!(status_str.contains("Implementation Turns: 3"));
        assert!(status_str.contains("Validation Rounds: 1"));
        assert!(status_str.contains("Missing edge case"));
        assert!(status_str.contains("Status: Revising"));
        assert!(status_str.contains("Started At:"));

        set_active_worker_status(&guard.0, "Approved");
        update_active_worker_progress(&guard.0, 4, 2, Some("All checks passed".to_string()));
        drop(guard);

        let completed_str = get_active_subtasks_str();
        assert!(completed_str.contains("Recently Completed Subagents:"));
        assert!(completed_str.contains("Implementation Turns: 4"));
        assert!(completed_str.contains("Validation Rounds: 2"));
        assert!(completed_str.contains("Status: Approved"));
        assert!(completed_str.contains("All checks passed"));
        assert!(completed_str.contains("Started At:"));
        assert!(completed_str.contains("Finished At:"));
    }

    #[test]
    fn test_cancel_active_worker_and_cancel_all() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let token1 = tokio_util::sync::CancellationToken::new();
        let token2 = tokio_util::sync::CancellationToken::new();

        let guard1 = register_active_worker_with_token(
            Some("t-001".to_string()),
            "coder".to_string(),
            "Writing parser".to_string(),
            Some(token1.clone()),
        );

        let guard2 = register_active_worker_with_token(
            Some("t-002".to_string()),
            "researcher".to_string(),
            "Investigating bug".to_string(),
            Some(token2.clone()),
        );

        assert!(!token1.is_cancelled());
        assert!(!token2.is_cancelled());

        // Cancel specific worker by task_id
        let cancelled = cancel_active_worker(None, Some("t-001"));
        assert!(cancelled);
        assert!(token1.is_cancelled());
        assert!(!token2.is_cancelled());

        // Cancel remaining workers with cancel_all_active_workers
        let count = cancel_all_active_workers();
        assert!(count >= 1);
        assert!(token2.is_cancelled());

        drop(guard1);
        drop(guard2);
    }

    #[test]
    fn test_format_workers_summary_utf8_char_boundary_no_panic() {
        let prefix = "e".repeat(296);
        let fb = format!(
            "{prefix}—feedback text that exceeds 300 characters easily and has em-dash right at the cut boundary"
        );
        let guard = register_active_worker(
            Some("task-utf8".to_string()),
            "coder".to_string(),
            "UTF8 test".to_string(),
        );
        update_active_worker_progress(&guard.0, 1, 1, Some(fb));

        let status_str = get_active_subtasks_str();
        assert!(status_str.contains("Latest Validator Feedback:"));
        assert!(status_str.contains("..."));

        drop(guard);
        let completed_str = get_active_subtasks_str();
        assert!(completed_str.contains("Latest Validator Feedback:"));
        assert!(completed_str.contains("..."));
    }

    /// Concurrency regression (Phase 2): 50 concurrent workers hammer the
    /// single sharded `WORKERS` DashMap — each registers via
    /// `register_active_worker_with_token`, interleaves
    /// `update_active_worker_context` + `set_active_worker_status`, then drops
    /// its guard (single-entry mutation + bounded-ring eviction). The whole
    /// batch must finish well within a generous wall-clock bound (no
    /// deadlock), and the completed ring must stay capped at 10 entries.
    #[tokio::test]
    async fn test_workers_stress_concurrent_register_update_drop() {
        let n = 50;
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            handles.push(tokio::spawn(async move {
                let guard = register_active_worker_with_token(
                    Some(format!("stress-{i}")),
                    format!("agent-{i}"),
                    format!("stress prompt {i}"),
                    None,
                );
                // Interleave context-token updates and status changes on the
                // same entry while other workers churn the map concurrently.
                for j in 0..5 {
                    update_active_worker_context(&guard.0, 100 * (j + 1));
                    set_active_worker_status(&guard.0, &format!("status-{j}"));
                }
                drop(guard);
            }));
        }
        // Generous bound: a correct implementation finishes in <1s; a
        // deadlock would hang until this fires.
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            for h in handles {
                h.await.expect("worker task panicked");
            }
        })
        .await
        .expect("50 concurrent worker register/update/drop tasks must complete (no deadlock)");

        // Every one of OUR 50 workers must be completed (not active) after its
        // guard dropped. (We check our own keys, not `has_active_workers()`,
        // because other lib tests share the process-global map concurrently.)
        let my_active = (0..n)
            .filter(|i| {
                WORKERS
                    .get(&format!("agent-{i}-stress-{i}"))
                    .is_some_and(|e| e.is_active())
            })
            .count();
        assert_eq!(
            my_active, 0,
            "all 50 stress workers must have completed after their guards dropped"
        );
        // Bounded completed ring: the cap is a hard invariant of the drop
        // path. Other lib tests churn the shared global map concurrently, so
        // a *momentary* count of 11 can be observed mid-drop; poll until the
        // ring settles at or below the cap (it must, given every drop evicts).
        let mut settled = false;
        for _ in 0..100 {
            let completed_count = WORKERS
                .iter()
                .filter(|e| !e.value().is_active())
                .count();
            if completed_count <= MAX_RECENT_COMPLETED {
                settled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            settled,
            "completed ring must settle at <= {MAX_RECENT_COMPLETED} entries"
        );
    }
}
