//! Active specialist workers registry, context token tracking, and RAII guards.

use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};
use std::time::Instant;

/// Information about an active specialist worker currently executing a task.
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

static ACTIVE_WORKERS: LazyLock<RwLock<BTreeMap<String, ActiveWorkerInfo>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

static WORKER_CONTEXT_TOKENS: LazyLock<RwLock<BTreeMap<String, usize>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

static RECENT_COMPLETED_WORKERS: LazyLock<RwLock<Vec<CompletedWorkerInfo>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// RAII guard that automatically unregisters an active worker on drop and moves it to recently completed.
pub struct ActiveWorkerGuard(pub String);

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = ACTIVE_WORKERS.write()
            && let Some(info) = map.remove(&self.0)
        {
            if let Ok(mut last_map) = WORKER_CONTEXT_TOKENS.write() {
                last_map.insert(self.0.clone(), info.context_tokens);
            }
            if let Ok(mut completed) = RECENT_COMPLETED_WORKERS.write() {
                let duration = info.started_at.elapsed();
                completed.push(CompletedWorkerInfo {
                    task_id: info.task_id,
                    agent_name: info.agent_name,
                    prompt: info.prompt,
                    started_at: info.started_at,
                    started_wall: info.started_wall,
                    completed_at: Instant::now(),
                    completed_wall: chrono::Local::now(),
                    duration,
                    implementation_turns: info.implementation_turns,
                    validation_rounds: info.validation_rounds,
                    latest_validator_feedback: info.latest_validator_feedback,
                    status: info.status,
                });
                if completed.len() > 10 {
                    let remove_count = completed.len() - 10;
                    completed.drain(0..remove_count);
                }
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
    let key = if let Some(ref t) = task_id {
        if !t.trim().is_empty() {
            format!("{agent_name}-{t}")
        } else {
            format!("{agent_name}-{}", Instant::now().elapsed().as_nanos())
        }
    } else {
        format!("{agent_name}-{}", Instant::now().elapsed().as_nanos())
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
                started_at: Instant::now(),
                started_wall: chrono::Local::now(),
                context_tokens: initial_tokens,
                implementation_turns: 0,
                validation_rounds: 0,
                latest_validator_feedback: None,
                status: "In Progress".to_string(),
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

/// Update the active specialist worker's implementation turn, validation rounds, and latest validator critique/feedback.
pub fn update_active_worker_progress(
    key: &str,
    turns: usize,
    val_rounds: usize,
    feedback: Option<String>,
) {
    if let Ok(mut map) = ACTIVE_WORKERS.write()
        && let Some(info) = map.get_mut(key)
    {
        info.implementation_turns = turns;
        info.validation_rounds = val_rounds;
        if let Some(fb) = feedback {
            info.latest_validator_feedback = Some(fb);
        }
    }
}

/// Set the descriptive status of an active specialist worker (e.g. "Approved", "Revising", "Failed", "Aborted").
pub fn set_active_worker_status(key: &str, status: &str) {
    if let Ok(mut map) = ACTIVE_WORKERS.write()
        && let Some(info) = map.get_mut(key)
    {
        info.status = status.to_string();
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

/// Formats all currently active and recently completed subagent workers with their tool call ID, prompt, running time,
/// implementation turns, validation rounds, and latest validator feedback.
pub fn get_active_subtasks_str() -> String {
    let active_map = ACTIVE_WORKERS.read().ok();
    let completed_list = RECENT_COMPLETED_WORKERS.read().ok();

    let has_active = active_map.as_ref().map(|m| !m.is_empty()).unwrap_or(false);
    let has_completed = completed_list
        .as_ref()
        .map(|c| !c.is_empty())
        .unwrap_or(false);

    if !has_active && !has_completed {
        return "None".to_string();
    }

    let mut out = String::new();
    if let Some(map) = active_map
        && !map.is_empty()
    {
        out.push_str("Active Background Subagents:\n");
        for (id, info) in map.iter() {
            let elapsed_secs = info.started_at.elapsed().as_secs();
            let duration_str = format_duration_human(elapsed_secs);
            let task_id_str = info.task_id.as_deref().unwrap_or(id);
            let start_wall_str = info.started_wall.format("%H:%M:%S");
            out.push_str(&format!(
                "- Tool Call ID: {}\n  Subagent: {}\n  Status: {}\n  Task Prompt: {}\n  Started At: {} (running for {}, {elapsed_secs} total seconds)\n  Implementation Turns: {}\n  Validation Rounds: {}\n",
                task_id_str, info.agent_name, info.status, info.prompt, start_wall_str, duration_str, info.implementation_turns, info.validation_rounds
            ));
            if let Some(ref fb) = info.latest_validator_feedback {
                let trimmed = fb.trim();
                let summary = if trimmed.len() > 300 {
                    format!("{}...", &trimmed[..297])
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

    if let Some(completed) = completed_list
        && !completed.is_empty()
    {
        out.push_str("Recently Completed Subagents:\n");
        for info in completed.iter().rev() {
            let duration_str = format_duration_human(info.duration.as_secs());
            let task_id_str = info.task_id.as_deref().unwrap_or(&info.agent_name);
            let start_wall_str = info.started_wall.format("%H:%M:%S");
            let finish_wall_str = info.completed_wall.format("%H:%M:%S");
            out.push_str(&format!(
                "- Tool Call ID: {}\n  Subagent: {}\n  Status: {}\n  Task Prompt: {}\n  Started At: {}\n  Finished At: {} (total duration: {})\n  Implementation Turns: {}\n  Validation Rounds: {}\n",
                task_id_str, info.agent_name, info.status, info.prompt, start_wall_str, finish_wall_str, duration_str, info.implementation_turns, info.validation_rounds
            ));
            if let Some(ref fb) = info.latest_validator_feedback {
                let trimmed = fb.trim();
                let summary = if trimmed.len() > 300 {
                    format!("{}...", &trimmed[..297])
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
    let map = ACTIVE_WORKERS.read().ok()?;
    let (_k, info) = map.iter().find(|(k, v)| {
        v.task_id.as_deref().map(str::to_lowercase) == Some(tid.clone())
            || k.to_lowercase().contains(&tid)
            || v.prompt.to_lowercase().contains(&tid)
    })?;
    let running_time = format_duration_human(info.started_at.elapsed().as_secs());
    Some((info.agent_name.clone(), running_time))
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
}
