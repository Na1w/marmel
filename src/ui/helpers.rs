//! Formatting, command inspection, subagent lifecycle, and error classification helpers.

use super::{Event, Renderer, SubagentDetail};
use crate::config::Config;
use crate::manager::context::ContextEngine;
use crate::orchestrator::{DelegationEvent, OrchestratorManager};
use crate::types::Message;
use anyhow::Result;

pub fn format_active_subtasks(subagents: &[SubagentDetail]) -> String {
    let global_active = crate::orchestrator::get_active_subtasks_str();
    if global_active != "None" && !global_active.trim().is_empty() {
        return global_active;
    }
    let active: Vec<_> = subagents.iter().filter(|s| s.is_active).collect();
    if active.is_empty() {
        return "None".to_string();
    }
    let mut out = String::new();
    for s in active {
        let elapsed_secs = s.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let task_id_str = s.task_id.as_deref().unwrap_or(&s.name);
        let prompt_str = if s.prompt.is_empty() {
            "None"
        } else {
            &s.prompt
        };
        out.push_str(&format!(
            "- Tool Call ID: {}\n  Subagent: {}\n  Task Prompt: {}\n  Running For: {} seconds\n\n",
            task_id_str, s.name, prompt_str, elapsed_secs
        ));
    }
    out
}

pub fn format_plan_progress_summary(plan_content: &str) -> String {
    crate::orchestrator::generate_plan_progress_summary(plan_content)
}
pub(crate) fn load_system_prompt_with_plan(
    _cfg: &Config,
    plan: &crate::agent::phase::Plan,
) -> Result<String> {
    let content = include_str!("../../prompts/system.md");
    let env_block = crate::prompts::format_environment_block();
    let mut prompt = format!("{content}\n\n{env_block}\n");
    if let Ok(Some(plan_content)) = plan.read()
        && !plan_content.trim().is_empty()
    {
        prompt.push_str(&format!(
            "\n## Active Execution Plan (`.marmel/execution_plan.md`)\nThere is an existing execution plan already active on disk:\n```markdown\n{}\n```\nDo NOT call `create_plan` unless you explicitly intend to overwrite the plan. Proceed directly with `delegate_task` to execute any remaining unchecked `- [ ] [t-xxx]` tasks.\n",
            plan_content.trim()
        ));
    }
    Ok(prompt)
}

#[allow(dead_code)]
pub(crate) fn load_system_prompt(cfg: &Config) -> Result<String> {
    load_system_prompt_with_plan(cfg, &crate::agent::phase::Plan::default())
}

pub(crate) fn format_tool_call_display(name: &str, args_val: &serde_json::Value) -> String {
    match name {
        "create_plan" => {
            let len = args_val
                .get("plan_markdown")
                .and_then(serde_json::Value::as_str)
                .map_or(0, str::len);
            format!("create_plan(plan_markdown: {len} chars)")
        }
        "delegate_task" => {
            let agent = args_val
                .get("agent_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("specialist");
            let task_id = args_val
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if task_id.is_empty() {
                format!("delegate_task(agent: {agent})")
            } else {
                format!("delegate_task(agent: {agent}, task_id: {task_id})")
            }
        }
        "write_file" | "read_file" | "replace" => {
            let path = args_val
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if path.is_empty() {
                format!("{name}()")
            } else {
                format!("{name}({path})")
            }
        }
        "run_command" => {
            let cmd = args_val
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if cmd.len() > 60 {
                format!("run_command({}…)", &cmd[..57])
            } else {
                format!("run_command({cmd})")
            }
        }
        "grep_search" => {
            let query = args_val
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if let Some(path) = args_val.get("path").and_then(serde_json::Value::as_str) {
                format!("grep_search({query} in {path})")
            } else {
                format!("grep_search({query})")
            }
        }
        "glob" => {
            let pattern = args_val
                .get("pattern")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("glob({pattern})")
        }
        _ => {
            let s = args_val.to_string();
            if s.len() > 60 {
                format!("{name}({}…)", &s[..57])
            } else {
                format!("{name}({s})")
            }
        }
    }
}
pub fn chunk_utf8(s: &str, max: usize) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let bytes = s.as_bytes();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut end = (start + max).min(bytes.len());
        while end > start && !s.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = start + s[start..].chars().next().map_or(1, |c| c.len_utf8());
        }
        chunks.push(&s[start..end]);
        start = end;
    }
    chunks
}

pub(crate) fn is_abort_command(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("/abort")
        || t.eq_ignore_ascii_case("/exit")
        || t.eq_ignore_ascii_case("/quit")
        || t.eq_ignore_ascii_case("/q")
        || t.eq_ignore_ascii_case(":q")
        || t.eq_ignore_ascii_case(":q!")
}

pub(crate) fn is_reset_command(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("/reset")
        || t.eq_ignore_ascii_case("/reset_plan")
        || t.eq_ignore_ascii_case("/reset-plan")
        || t.eq_ignore_ascii_case("/clear_plan")
        || t.eq_ignore_ascii_case("/clear-plan")
        || t.eq_ignore_ascii_case("/reset_execution_plan")
}

pub(crate) fn handle_reset_command(
    plan: &crate::agent::phase::Plan,
    renderer: &mut dyn Renderer,
    ctx: &mut ContextEngine,
) {
    let _ = plan.clear();
    let transcript = plan.transcript_path();
    if transcript.exists() {
        let _ = std::fs::remove_file(&transcript);
    }
    renderer.on_event(&Event::Message(
        "Execution plan has been cleared and reset by user.".to_string(),
    ));
    renderer.on_event(&Event::Status("Execution plan reset".to_string()));
    let _ = renderer.flush();
    ctx.append(Message::User {
        content: "[System] User executed /reset. The execution plan has been removed from disk. Return to Conversational phase."
            .to_string(),
    });
}

pub(crate) fn classify_llm_error(e: &anyhow::Error) -> &'static str {
    let msg = format!("{e:#}").to_lowercase();
    if msg.contains("http") || msg.contains("status") || msg.contains("503") || msg.contains("429")
    {
        "http"
    } else if msg.contains("transport") || msg.contains("connection") {
        "connectivity"
    } else if msg.contains("timeout") {
        "timeout"
    } else if msg.contains("stream") || msg.contains("sse") {
        "stream"
    } else {
        "unknown"
    }
}

pub(crate) fn update_subagent_lifecycle(
    subagents: &mut Vec<SubagentDetail>,
    agent: crate::agents::Agent,
    task: Option<String>,
    prompt: Option<String>,
    started: bool,
) {
    let name = match &task {
        Some(t) if !t.trim().is_empty() => format!("{}-{t}", agent.as_str()),
        _ => agent.as_str().to_string(),
    };
    let task_str = task.clone().unwrap_or_default();
    let log_entry = if started {
        format!("started task {task_str}")
    } else {
        format!("completed task {task_str}")
    };
    let active_tokens = crate::orchestrator::get_active_worker_tokens(&name).unwrap_or(0);
    let now = std::time::Instant::now();
    if let Some(existing) = subagents.iter_mut().find(|s| s.name == name) {
        existing.is_active = started;
        existing.last_activity_at = Some(now);
        if active_tokens > 0 {
            existing.context_tokens = active_tokens;
        }
        if started {
            existing.task_id = task;
            if let Some(p) = prompt {
                existing.prompt = p;
            }
            existing.started_at = Some(now);
        } else {
            existing.started_at = None;
        }
        existing.logs.push(log_entry);
    } else {
        subagents.push(SubagentDetail {
            name,
            task_id: task,
            prompt: prompt.unwrap_or_default(),
            started_at: if started { Some(now) } else { None },
            last_activity_at: Some(now),
            logs: vec![log_entry],
            thinking: String::new(),
            content: String::new(),
            is_active: started,
            context_tokens: active_tokens,
        });
    }
}

pub(crate) fn drain_delegation_events(
    manager: Option<&OrchestratorManager>,
    renderer: &mut dyn Renderer,
    subagents: &mut Vec<SubagentDetail>,
) {
    let Some(manager) = manager else {
        return;
    };
    let Ok(mut events) = manager.delegation_events.lock() else {
        return;
    };
    let mut changed = false;
    for event in events.drain(..) {
        match &event {
            DelegationEvent::Started { agent, task } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, true);
                changed = true;
            }
            DelegationEvent::Completed { agent, task } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, false);
                changed = true;
            }
        }
        renderer.on_event(&Event::Delegation(event));
    }
    if changed {
        renderer.set_subagents(subagents.clone());
    }
}
