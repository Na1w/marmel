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

/// Rehydrate the list of specialist subagents from historical session artifacts:
/// 1. Messages in the transcript (`delegate_task` tool calls and matching `tool` results).
/// 2. Crash journal entries in `.session_journal.json`.
/// 3. Recovered deliverable from deep-freeze checkpoint (if any).
/// 4. Completed tasks in the on-disk execution plan.
pub fn rehydrate_subagents(
    messages: &[Message],
    journal: Option<&crate::orchestrator::freeze::CrashJournal>,
    recovered_deliverable: Option<&(String, String)>,
    plan: Option<&crate::agent::phase::Plan>,
) -> Vec<SubagentDetail> {
    let mut subagents = Vec::<SubagentDetail>::new();

    // 1. Rehydrate from messages in transcript: look for delegate_task tool calls and results
    for msg in messages {
        if let Message::Assistant { tool_calls, .. } = msg {
            for call in tool_calls {
                if call.function.name == "delegate_task" {
                    let args_val =
                        serde_json::from_str::<serde_json::Value>(&call.function.arguments).ok();
                    let agent_name = args_val
                        .as_ref()
                        .and_then(|v| v.get("agent_name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("specialist");
                    let task_id = args_val
                        .as_ref()
                        .and_then(|v| v.get("task_id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let prompt = args_val
                        .as_ref()
                        .and_then(|v| v.get("prompt"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let name = match &task_id {
                        Some(tid) if !tid.trim().is_empty() => format!("{agent_name}-{tid}"),
                        _ => agent_name.to_string(),
                    };

                    // Find matching tool result
                    let matching_tool = messages.iter().find_map(|m| match m {
                        Message::Tool {
                            tool_call_id,
                            content,
                        } if tool_call_id == &call.id => Some(content.clone()),
                        _ => None,
                    });

                    let task_str = task_id.as_deref().unwrap_or("");
                    let mut logs = vec![format!("started task {task_str}")];
                    let content = if let Some(c) = matching_tool {
                        logs.push(format!("completed task {task_str}"));
                        c
                    } else if let Some((rec_task, rec_content)) = recovered_deliverable {
                        if task_id.as_deref() == Some(rec_task) || name.ends_with(rec_task) {
                            logs.push(format!("completed task {task_str}"));
                            rec_content.clone()
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };

                    let context_tokens = if !content.is_empty() {
                        tiktoken_rs::cl100k_base_singleton()
                            .encode_ordinary(&content)
                            .len()
                    } else {
                        0
                    };

                    if let Some(existing) = subagents.iter_mut().find(|s| s.name == name) {
                        if existing.content.is_empty() && !content.is_empty() {
                            existing.content = content;
                            existing.context_tokens = context_tokens;
                        }
                        if existing.prompt.is_empty() && !prompt.is_empty() {
                            existing.prompt = prompt;
                        }
                        if existing.task_id.is_none() && task_id.is_some() {
                            existing.task_id = task_id;
                        }
                        for log in logs {
                            if !existing.logs.contains(&log) {
                                existing.logs.push(log);
                            }
                        }
                    } else {
                        subagents.push(SubagentDetail {
                            name,
                            task_id,
                            prompt,
                            started_at: None,
                            last_activity_at: None,
                            logs,
                            thinking: String::new(),
                            content,
                            is_active: false,
                            context_tokens,
                        });
                    }
                }
            }
        }
    }

    // 2. Incorporate crash journal events (if any delegation was logged)
    if let Some(j) = journal
        && let Ok(entries) = j.journal()
    {
        for entry in entries {
            let name = match &entry.task_id {
                Some(tid) if !tid.trim().is_empty() => {
                    format!("{}-{tid}", entry.agent.as_str())
                }
                _ => entry.agent.as_str().to_string(),
            };
            let task_str = entry.task_id.as_deref().unwrap_or("");
            let log_entry = match entry.kind {
                crate::orchestrator::freeze::JournalEventKind::Resolved => {
                    format!("completed task {task_str}")
                }
                crate::orchestrator::freeze::JournalEventKind::Failed => {
                    format!("failed task {task_str}")
                }
                crate::orchestrator::freeze::JournalEventKind::Frozen => {
                    format!("started task {task_str}")
                }
            };
            if let Some(existing) = subagents.iter_mut().find(|s| s.name == name) {
                if !existing.logs.contains(&log_entry) {
                    existing.logs.push(log_entry);
                }
            } else {
                let mut logs = vec![format!("started task {task_str}")];
                if entry.kind != crate::orchestrator::freeze::JournalEventKind::Frozen {
                    logs.push(log_entry);
                }
                subagents.push(SubagentDetail {
                    name,
                    task_id: entry.task_id,
                    prompt: String::new(),
                    started_at: None,
                    last_activity_at: None,
                    logs,
                    thinking: String::new(),
                    content: String::new(),
                    is_active: false,
                    context_tokens: 0,
                });
            }
        }
    }

    // 3. Fold in recovered_deliverable if not already present
    if let Some((rec_task, rec_content)) = recovered_deliverable {
        let matched = subagents
            .iter_mut()
            .find(|s| s.task_id.as_deref() == Some(rec_task) || s.name.ends_with(rec_task));
        if let Some(existing) = matched {
            if existing.content.is_empty() {
                existing.content = rec_content.clone();
                existing.context_tokens = tiktoken_rs::cl100k_base_singleton()
                    .encode_ordinary(rec_content)
                    .len();
            }
            let completed_log = format!("completed task {rec_task}");
            if !existing.logs.contains(&completed_log) {
                existing.logs.push(completed_log);
            }
        } else {
            let name = format!("specialist-{rec_task}");
            let context_tokens = tiktoken_rs::cl100k_base_singleton()
                .encode_ordinary(rec_content)
                .len();
            subagents.push(SubagentDetail {
                name,
                task_id: Some(rec_task.clone()),
                prompt: String::new(),
                started_at: None,
                last_activity_at: None,
                logs: vec![
                    format!("started task {rec_task}"),
                    format!("completed task {rec_task}"),
                ],
                thinking: String::new(),
                content: rec_content.clone(),
                is_active: false,
                context_tokens,
            });
        }
    }

    // 4. Check off tasks from execution plan that might have completed
    if let Some(p) = plan {
        let all = p.all_tasks();
        let pending = p.pending_tasks();
        let completed: Vec<String> = all.into_iter().filter(|t| !pending.contains(t)).collect();
        let plan_content = p.read().ok().flatten().unwrap_or_default();

        for tid in completed {
            if !subagents
                .iter()
                .any(|s| s.task_id.as_deref() == Some(&tid) || s.name.ends_with(&tid))
            {
                // Detect role if specified on the task line, e.g. (coder)
                let matching_line = plan_content
                    .lines()
                    .find(|line| line.contains(&tid))
                    .unwrap_or("");
                let role = if matching_line.contains("(coder)") {
                    "coder"
                } else if matching_line.contains("(researcher)") {
                    "researcher"
                } else if matching_line.contains("(validator)") {
                    "validator"
                } else if matching_line.contains("(debugger)") {
                    "debugger"
                } else {
                    "specialist"
                };
                subagents.push(SubagentDetail {
                    name: format!("{role}-{tid}"),
                    task_id: Some(tid.clone()),
                    prompt: String::new(),
                    started_at: None,
                    last_activity_at: None,
                    logs: vec![
                        format!("started task {tid}"),
                        format!("completed task {tid}"),
                    ],
                    thinking: String::new(),
                    content: String::new(),
                    is_active: false,
                    context_tokens: 0,
                });
            }
        }
    }

    subagents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ToolCall};

    #[test]
    fn test_rehydrate_subagents_from_messages() {
        let messages = vec![
            Message::System {
                content: "sys".to_string(),
            },
            Message::User {
                content: "goal".to_string(),
            },
            Message::Assistant {
                content: Some("I will delegate task 1".to_string()),
                reasoning_content: None,
                tool_calls: vec![ToolCall::new(
                    "call_1",
                    "delegate_task",
                    serde_json::json!({
                        "agent_name": "coder",
                        "task_id": "t-001",
                        "prompt": "write scene.rs"
                    })
                    .to_string(),
                )],
            },
            Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "MISSION COMPLETE (t-001):\nwrote scene.rs successfully".to_string(),
            },
        ];

        let subagents = rehydrate_subagents(&messages, None, None, None);
        assert_eq!(subagents.len(), 1);
        let s = &subagents[0];
        assert_eq!(s.name, "coder-t-001");
        assert_eq!(s.task_id.as_deref(), Some("t-001"));
        assert_eq!(s.prompt, "write scene.rs");
        assert_eq!(
            s.content,
            "MISSION COMPLETE (t-001):\nwrote scene.rs successfully"
        );
        assert!(!s.is_active);
        assert_eq!(s.logs, vec!["started task t-001", "completed task t-001"]);
    }

    #[test]
    fn test_rehydrate_subagents_with_recovered_deliverable() {
        let messages = vec![
            Message::System {
                content: "sys".to_string(),
            },
            Message::User {
                content: "goal".to_string(),
            },
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall::new(
                    "call_frozen",
                    "delegate_task",
                    serde_json::json!({
                        "agent_name": "researcher",
                        "task_id": "t-002",
                        "prompt": "study docs"
                    })
                    .to_string(),
                )],
            },
        ];

        let recovered = ("t-002".to_string(), "recovered report".to_string());
        let subagents = rehydrate_subagents(&messages, None, Some(&recovered), None);
        assert_eq!(subagents.len(), 1);
        let s = &subagents[0];
        assert_eq!(s.name, "researcher-t-002");
        assert_eq!(s.content, "recovered report");
        assert!(s.logs.contains(&"completed task t-002".to_string()));
    }

    #[test]
    fn test_rehydrate_subagents_from_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = crate::agent::phase::Plan::at(tmp.path());
        plan.create("# Plan\n\n- [x] [t-001] Setup project (coder)\n- [ ] [t-002] Write tests\n")
            .unwrap();

        let subagents = rehydrate_subagents(&[], None, None, Some(&plan));
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].name, "coder-t-001");
        assert_eq!(subagents[0].task_id.as_deref(), Some("t-001"));
        assert!(!subagents[0].is_active);
    }
}
