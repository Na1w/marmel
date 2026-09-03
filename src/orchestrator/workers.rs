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
    pub context_tokens: usize,
}

static ACTIVE_WORKERS: LazyLock<RwLock<BTreeMap<String, ActiveWorkerInfo>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

static WORKER_CONTEXT_TOKENS: LazyLock<RwLock<BTreeMap<String, usize>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

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
