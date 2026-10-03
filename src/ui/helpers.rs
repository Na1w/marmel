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
    cfg: &Config,
    plan: &crate::manager::phase::Plan,
) -> Result<String> {
    let content = if cfg.system_prompt_path.exists() {
        std::fs::read_to_string(&cfg.system_prompt_path)
            .unwrap_or_else(|_| include_str!("../../prompts/system.md").to_string())
    } else {
        include_str!("../../prompts/system.md").to_string()
    };
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
    load_system_prompt_with_plan(cfg, &crate::manager::phase::Plan::default())
}

pub(crate) fn format_tool_call_display(name: &str, args_val: &serde_json::Value) -> String {
    match name {
        crate::tool_names::TOOL_CREATE_PLAN => {
            let len = args_val
                .get("plan_markdown")
                .and_then(serde_json::Value::as_str)
                .map_or(0, str::len);
            format!("create_plan(plan_markdown: {len} chars)")
        }
        crate::tool_names::TOOL_DELEGATE_TASK => {
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
        crate::tool_names::TOOL_WRITE_FILE
        | crate::tool_names::TOOL_READ_FILE
        | crate::tool_names::TOOL_REPLACE => {
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
        crate::tool_names::TOOL_RUN_COMMAND => {
            let cmd = args_val
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if cmd.len() > 60 {
                let cut = cmd.floor_char_boundary(57);
                format!("run_command({}…)", &cmd[..cut])
            } else {
                format!("run_command({cmd})")
            }
        }
        crate::tool_names::TOOL_GREP_SEARCH => {
            let query = args_val
                .get("pattern")
                .or_else(|| args_val.get("query"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if let Some(path) = args_val.get("path").and_then(serde_json::Value::as_str) {
                format!("grep_search({query} in {path})")
            } else {
                format!("grep_search({query})")
            }
        }
        crate::tool_names::TOOL_GLOB => {
            let pattern = args_val
                .get("pattern")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("glob({pattern})")
        }
        _ => {
            let s = args_val.to_string();
            if s.len() > 60 {
                let cut = s.floor_char_boundary(57);
                format!("{name}({}…)", &s[..cut])
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
        || t.eq_ignore_ascii_case("abort")
        || t.eq_ignore_ascii_case("/exit")
        || t.eq_ignore_ascii_case("exit")
        || t.eq_ignore_ascii_case("/quit")
        || t.eq_ignore_ascii_case("quit")
        || t.eq_ignore_ascii_case("/q")
        || t.eq_ignore_ascii_case(":q")
        || t.eq_ignore_ascii_case(":q!")
        || t.eq_ignore_ascii_case("/stop")
        || t.eq_ignore_ascii_case("stop")
        || t.eq_ignore_ascii_case("/cancel")
        || t.eq_ignore_ascii_case("cancel")
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
    plan: &crate::manager::phase::Plan,
    renderer: &mut dyn Renderer,
    mut ctx: Option<&mut ContextEngine>,
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
    if let Some(ref mut ctx) = ctx {
        ctx.append(Message::User {
            content: "[System] User executed /reset. The execution plan has been removed from disk. Return to Conversational phase."
                .to_string(),
        });
    }
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
    let clean_task = task
        .as_ref()
        .map(|t| {
            t.trim_matches(|c| {
                c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
            })
            .trim()
        })
        .filter(|t| !t.is_empty());
    let name = match clean_task {
        Some(t) => format!("{}-{t}", agent.as_str()),
        None => agent.as_str().to_string(),
    };
    let task_str = clean_task.unwrap_or("").to_string();
    let log_entry = if started {
        format!("started task {task_str}")
    } else {
        format!("completed task {task_str}")
    };
    let active_tokens = crate::orchestrator::get_active_worker_tokens(&name).unwrap_or(0);
    let now = std::time::Instant::now();
    if let Some(existing) = subagents.iter_mut().find(|s| s.name == name) {
        if existing.is_active && !started {
            if let Some(st) = existing.started_at.take() {
                existing.worked_duration += st.elapsed();
            }
        } else if !existing.is_active && started {
            existing.started_at = Some(now);
        }
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
        }
        existing.logs.push(log_entry);
    } else {
        subagents.push(SubagentDetail {
            name,
            task_id: task,
            prompt: prompt.unwrap_or_default(),
            started_at: if started { Some(now) } else { None },
            worked_duration: std::time::Duration::ZERO,
            last_activity_at: Some(now),
            logs: vec![log_entry],
            thinking: String::new(),
            content: String::new(),
            is_active: started,
            context_tokens: active_tokens,
        });
    }
}

#[allow(dead_code)]
pub(crate) fn drain_delegation_events(
    manager: Option<&OrchestratorManager>,
    renderer: &mut dyn Renderer,
    subagents: &mut Vec<SubagentDetail>,
) {
    drain_delegation_events_with_transcript(manager, renderer, subagents, None);
}

pub(crate) fn drain_delegation_events_with_transcript(
    manager: Option<&OrchestratorManager>,
    renderer: &mut dyn Renderer,
    subagents: &mut Vec<SubagentDetail>,
    mut ui_transcript: Option<&mut crate::ui::UiTranscript>,
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
                if let Some(ref mut tr) = ui_transcript {
                    let task_id = task.clone().unwrap_or_else(|| agent.to_string());
                    tr.append(crate::ui::UiRecord::TaskCompleted { task_id });
                }
            }
            DelegationEvent::Failed { agent, task } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, false);
                changed = true;
                if let Some(ref mut tr) = ui_transcript {
                    let task_id = task.clone().unwrap_or_else(|| agent.to_string());
                    tr.append(crate::ui::UiRecord::TaskFailed { task_id });
                }
            }
        }
        renderer.on_event(&Event::Delegation(event));
    }
    if changed {
        renderer.set_subagents(subagents.clone());
    }
}

/// Sanitize task id string by stripping markdown enclosing characters.
pub fn clean_task_id(tid: &str) -> String {
    tid.trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\'')
        .trim()
        .to_string()
}

/// Find a subagent in the list by matching name, task id, or suffix.
pub fn find_subagent_mut<'a>(
    subagents: &'a mut [SubagentDetail],
    name: &str,
    task_id: Option<&str>,
) -> Option<&'a mut SubagentDetail> {
    subagents.iter_mut().find(|s| {
        if s.name == name {
            return true;
        }
        if let (Some(a), Some(b)) = (s.task_id.as_deref(), task_id) {
            let a_clean = clean_task_id(a);
            let b_clean = clean_task_id(b);
            if !a_clean.is_empty() && a_clean == b_clean {
                return true;
            }
        }
        if let Some(tid) = task_id {
            let clean = clean_task_id(tid);
            if !clean.is_empty() && s.name.ends_with(&clean) {
                return true;
            }
        }
        if let Some(s_tid) = s.task_id.as_deref() {
            let clean = clean_task_id(s_tid);
            if !clean.is_empty() && name.ends_with(&clean) {
                return true;
            }
        }
        false
    })
}

/// Load synthesized prompt blueprint from disk (`.marmel/prompts/<task_id>.md`).
pub fn load_prompt_from_disk(
    plan: Option<&crate::manager::phase::Plan>,
    task_id: Option<&str>,
) -> String {
    let Some(tid) = task_id else {
        return String::new();
    };
    let clean = clean_task_id(tid);
    if clean.is_empty() {
        return String::new();
    }
    let prompt_file = if let Some(p) = plan {
        p.dir().join("prompts").join(format!("{clean}.md"))
    } else {
        std::path::Path::new(".marmel")
            .join("prompts")
            .join(format!("{clean}.md"))
    };
    if !prompt_file.exists() {
        return String::new();
    }
    if let Ok(bp) = crate::agents::AgentBlueprint::load_from_disk(&prompt_file)
        && !bp.system_prompt.is_empty()
    {
        return bp.system_prompt;
    }
    std::fs::read_to_string(&prompt_file).unwrap_or_default()
}

/// Check whether a subagent task was in-flight when the session stopped (e.g. frozen,
/// pending in crash journal, or pending in plan without matching tool result).
fn is_subagent_in_flight(
    journal: Option<&crate::orchestrator::freeze::CrashJournal>,
    plan: Option<&crate::manager::phase::Plan>,
    task_id: Option<&str>,
    name: &str,
) -> bool {
    // 1. If currently frozen in .session_frozen.json -> definitely in-flight
    if let Some(j) = journal
        && let Ok(frozen) = j.frozen_all()
    {
        for snap in frozen {
            if let Some(tid) = task_id {
                let clean = clean_task_id(tid);
                if snap.sub_req.task_id.as_deref().map(clean_task_id) == Some(clean) {
                    return true;
                }
            }
            let snap_name = match snap.sub_req.task_id.as_deref().map(clean_task_id) {
                Some(tid) if !tid.is_empty() => format!("{}-{tid}", snap.agent_name.as_str()),
                _ => snap.agent_name.as_str().to_string(),
            };
            if name == snap_name || name.ends_with(&snap.worker_id) {
                return true;
            }
        }
    }

    // 2. If CrashJournal has a Frozen event without subsequent Resolved/Failed
    if let Some(j) = journal
        && let Ok(entries) = j.journal()
    {
        let mut latest_kind = None;
        for entry in entries {
            let matches = if let Some(tid) = task_id {
                let clean = clean_task_id(tid);
                entry.task_id.as_deref().map(clean_task_id) == Some(clean)
            } else {
                name.starts_with(entry.agent.as_str())
            };
            if matches {
                latest_kind = Some(entry.kind);
            }
        }
        if latest_kind == Some(crate::orchestrator::freeze::JournalEventKind::Frozen) {
            return true;
        }
    }

    // 3. If plan has this task_id as pending
    if let Some(p) = plan
        && let Some(tid) = task_id
    {
        let pending = p.pending_tasks();
        let clean = clean_task_id(tid);
        if pending.iter().any(|pt| clean_task_id(pt) == clean) {
            return true;
        }
    }

    false
}

/// Rehydrate the list of specialist subagents from historical session artifacts:
/// 1. Frozen in-flight workers from `.session_frozen.json`.
/// 2. Messages in the transcript (`delegate_task` tool calls and matching `tool` results).
/// 3. Crash journal entries in `.session_journal.json`.
/// 4. UI transcript records (`TaskCompleted` / `TaskFailed`).
/// 5. Recovered deliverable from deep-freeze checkpoint (if any).
/// 6. Tasks in the on-disk execution plan.
/// 7. Synthesized prompts on disk in `.marmel/prompts/`.
#[allow(dead_code)]
pub fn rehydrate_subagents(
    messages: &[Message],
    journal: Option<&crate::orchestrator::freeze::CrashJournal>,
    recovered_deliverable: Option<&(String, String)>,
    plan: Option<&crate::manager::phase::Plan>,
) -> Vec<SubagentDetail> {
    rehydrate_subagents_with_ui(messages, journal, recovered_deliverable, plan, None)
}

/// Rehydrate the list of specialist subagents with optional UI transcript support.
pub fn rehydrate_subagents_with_ui(
    messages: &[Message],
    journal: Option<&crate::orchestrator::freeze::CrashJournal>,
    recovered_deliverable: Option<&(String, String)>,
    plan: Option<&crate::manager::phase::Plan>,
    ui_transcript: Option<&crate::ui::UiTranscript>,
) -> Vec<SubagentDetail> {
    let mut subagents = Vec::<SubagentDetail>::new();

    // 1. Rehydrate frozen in-flight workers from `.session_frozen.json`
    if let Some(j) = journal
        && let Ok(snapshots) = j.frozen_all()
    {
        for snap in snapshots {
            let task_id = snap.sub_req.task_id.as_deref().map(clean_task_id);
            let task_str = task_id.as_deref().unwrap_or("");
            let name = match &task_id {
                Some(tid) if !tid.is_empty() => format!("{}-{tid}", snap.agent_name.as_str()),
                _ => snap.agent_name.as_str().to_string(),
            };
            let is_recovered = recovered_deliverable.is_some_and(|(rec_task, _)| {
                let clean_rec = clean_task_id(rec_task);
                task_id.as_deref() == Some(&clean_rec) || name.ends_with(&clean_rec)
            });
            let is_active = !is_recovered;
            let mut logs = vec![format!("started task {task_str}")];
            if is_active {
                logs.push("recovering interrupted task".to_string());
            } else {
                logs.push(format!("completed task {task_str}"));
            }

            let (content, context_tokens) = if is_recovered {
                if let Some((_, rec_content)) = recovered_deliverable {
                    let toks = tiktoken_rs::cl100k_base_singleton()
                        .encode_ordinary(rec_content)
                        .len();
                    (rec_content.clone(), toks)
                } else {
                    (String::new(), 0)
                }
            } else {
                (String::new(), 0)
            };

            let prompt = if !snap.sub_req.prompt.is_empty() {
                snap.sub_req.prompt.clone()
            } else {
                load_prompt_from_disk(plan, task_id.as_deref())
            };

            let now = std::time::Instant::now();
            if let Some(existing) = find_subagent_mut(&mut subagents, &name, task_id.as_deref()) {
                if existing.prompt.is_empty() && !prompt.is_empty() {
                    existing.prompt = prompt;
                }
                if existing.task_id.is_none() && task_id.is_some() {
                    existing.task_id = task_id;
                }
                if is_active {
                    existing.is_active = true;
                    existing.started_at = Some(now);
                    existing.last_activity_at = Some(now);
                } else if !content.is_empty() {
                    existing.content = content;
                    existing.context_tokens = context_tokens;
                    existing.is_active = false;
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
                    started_at: if is_active { Some(now) } else { None },
                    worked_duration: std::time::Duration::ZERO,
                    last_activity_at: if is_active { Some(now) } else { None },
                    logs,
                    thinking: String::new(),
                    content,
                    is_active,
                    context_tokens,
                });
            }
        }
    }

    // 2. Rehydrate from messages in transcript (`delegate_task` tool calls and matching results)
    for msg in messages {
        if let Message::Assistant { tool_calls, .. } = msg {
            for call in tool_calls {
                if call.function.name == crate::tool_names::TOOL_DELEGATE_TASK {
                    let args_val =
                        serde_json::from_str::<serde_json::Value>(&call.function.arguments).ok();
                    let agent_name = args_val
                        .as_ref()
                        .and_then(|v| v.get("agent_name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("specialist");
                    let raw_task_id = args_val
                        .as_ref()
                        .and_then(|v| v.get("task_id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let task_id = raw_task_id.as_deref().map(clean_task_id);
                    let prompt_val = args_val
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
                    let (content, is_active) = if let Some(c) = matching_tool {
                        logs.push(format!("completed task {task_str}"));
                        (c, false)
                    } else if let Some((rec_task, rec_content)) = recovered_deliverable {
                        let clean_rec = clean_task_id(rec_task);
                        if task_id.as_deref() == Some(&clean_rec) || name.ends_with(&clean_rec) {
                            logs.push(format!("completed task {task_str}"));
                            (rec_content.clone(), false)
                        } else {
                            let active =
                                is_subagent_in_flight(journal, plan, task_id.as_deref(), &name);
                            (String::new(), active)
                        }
                    } else {
                        let active =
                            is_subagent_in_flight(journal, plan, task_id.as_deref(), &name);
                        (String::new(), active)
                    };

                    let prompt = if !prompt_val.is_empty() {
                        prompt_val
                    } else {
                        load_prompt_from_disk(plan, task_id.as_deref())
                    };

                    let context_tokens = if !content.is_empty() {
                        tiktoken_rs::cl100k_base_singleton()
                            .encode_ordinary(&content)
                            .len()
                    } else {
                        0
                    };

                    let now = std::time::Instant::now();
                    if let Some(existing) =
                        find_subagent_mut(&mut subagents, &name, task_id.as_deref())
                    {
                        if existing.content.is_empty() && !content.is_empty() {
                            existing.content = content;
                            existing.context_tokens = context_tokens;
                            existing.is_active = false;
                        }
                        if existing.prompt.is_empty() && !prompt.is_empty() {
                            existing.prompt = prompt;
                        }
                        if existing.task_id.is_none() && task_id.is_some() {
                            existing.task_id = task_id;
                        }
                        if is_active && !existing.is_active && existing.content.is_empty() {
                            existing.is_active = true;
                            existing.started_at = Some(now);
                            existing.last_activity_at = Some(now);
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
                            started_at: if is_active { Some(now) } else { None },
                            worked_duration: std::time::Duration::ZERO,
                            last_activity_at: if is_active { Some(now) } else { None },
                            logs,
                            thinking: String::new(),
                            content,
                            is_active,
                            context_tokens,
                        });
                    }
                }
            }
        }
    }

    // 3. Incorporate crash journal events (.session_journal.json)
    if let Some(j) = journal
        && let Ok(entries) = j.journal()
    {
        for entry in entries {
            let task_id = entry.task_id.as_deref().map(clean_task_id);
            let name = match &task_id {
                Some(tid) if !tid.trim().is_empty() => {
                    format!("{}-{tid}", entry.agent.as_str())
                }
                _ => entry.agent.as_str().to_string(),
            };
            let task_str = task_id.as_deref().unwrap_or("");
            let (log_entry, is_active) = match entry.kind {
                crate::orchestrator::freeze::JournalEventKind::Resolved => {
                    (format!("completed task {task_str}"), false)
                }
                crate::orchestrator::freeze::JournalEventKind::Failed => {
                    (format!("failed task {task_str}"), false)
                }
                crate::orchestrator::freeze::JournalEventKind::Frozen => {
                    let active = is_subagent_in_flight(journal, plan, task_id.as_deref(), &name);
                    (format!("started task {task_str}"), active)
                }
            };
            let prompt = load_prompt_from_disk(plan, task_id.as_deref());
            let now = std::time::Instant::now();
            if let Some(existing) = find_subagent_mut(&mut subagents, &name, task_id.as_deref()) {
                if !existing.logs.contains(&log_entry) {
                    existing.logs.push(log_entry);
                }
                if existing.prompt.is_empty() && !prompt.is_empty() {
                    existing.prompt = prompt;
                }
                if entry.kind == crate::orchestrator::freeze::JournalEventKind::Resolved
                    || entry.kind == crate::orchestrator::freeze::JournalEventKind::Failed
                {
                    existing.is_active = false;
                } else if is_active && existing.content.is_empty() {
                    existing.is_active = true;
                    if existing.started_at.is_none() {
                        existing.started_at = Some(now);
                    }
                    existing.last_activity_at = Some(now);
                }
            } else {
                let mut logs = vec![format!("started task {task_str}")];
                if entry.kind != crate::orchestrator::freeze::JournalEventKind::Frozen {
                    logs.push(log_entry);
                }
                subagents.push(SubagentDetail {
                    name,
                    task_id,
                    prompt,
                    started_at: if is_active { Some(now) } else { None },
                    worked_duration: std::time::Duration::ZERO,
                    last_activity_at: if is_active { Some(now) } else { None },
                    logs,
                    thinking: String::new(),
                    content: String::new(),
                    is_active,
                    context_tokens: 0,
                });
            }
        }
    }

    // 4. Incorporate UI transcript events (TaskCompleted, TaskFailed)
    if let Some(tr) = ui_transcript {
        for record in tr.records() {
            match record {
                crate::ui::UiRecord::TaskCompleted { task_id } => {
                    let clean = clean_task_id(task_id);
                    if let Some(sa) = find_subagent_mut(&mut subagents, &clean, Some(&clean)) {
                        sa.is_active = false;
                        let comp_log = format!("completed task {clean}");
                        if !sa.logs.contains(&comp_log) {
                            sa.logs.push(comp_log);
                        }
                    }
                }
                crate::ui::UiRecord::TaskFailed { task_id } => {
                    let clean = clean_task_id(task_id);
                    if let Some(sa) = find_subagent_mut(&mut subagents, &clean, Some(&clean)) {
                        sa.is_active = false;
                        let fail_log = format!("failed task {clean}");
                        if !sa.logs.contains(&fail_log) {
                            sa.logs.push(fail_log);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // 5. Fold in recovered_deliverable if not already present
    if let Some((rec_task, rec_content)) = recovered_deliverable {
        let clean_rec = clean_task_id(rec_task);
        if let Some(existing) = find_subagent_mut(&mut subagents, &clean_rec, Some(&clean_rec)) {
            existing.content = rec_content.clone();
            existing.context_tokens = tiktoken_rs::cl100k_base_singleton()
                .encode_ordinary(rec_content)
                .len();
            existing.is_active = false;
            let completed_log = format!("completed task {clean_rec}");
            if !existing.logs.contains(&completed_log) {
                existing.logs.push(completed_log);
            }
        } else {
            let name = format!("specialist-{clean_rec}");
            let context_tokens = tiktoken_rs::cl100k_base_singleton()
                .encode_ordinary(rec_content)
                .len();
            let prompt = load_prompt_from_disk(plan, Some(&clean_rec));
            subagents.push(SubagentDetail {
                name,
                task_id: Some(clean_rec.clone()),
                prompt,
                started_at: None,
                worked_duration: std::time::Duration::ZERO,
                last_activity_at: None,
                logs: vec![
                    format!("started task {clean_rec}"),
                    format!("completed task {clean_rec}"),
                ],
                thinking: String::new(),
                content: rec_content.clone(),
                is_active: false,
                context_tokens,
            });
        }
    }

    // 6. Check off tasks from execution plan that might have completed
    if let Some(p) = plan {
        let all = p.all_tasks();
        let pending = p.pending_tasks();
        let completed: Vec<String> = all.into_iter().filter(|t| !pending.contains(t)).collect();
        let plan_content = p.read().ok().flatten().unwrap_or_default();

        for tid in completed {
            let clean = clean_task_id(&tid);
            if !subagents
                .iter()
                .any(|s| s.task_id.as_deref() == Some(&clean) || s.name.ends_with(&clean))
            {
                // Detect role if specified on the task line, e.g. (coder)
                let matching_line = plan_content
                    .lines()
                    .find(|line| line.contains(&clean) || line.contains(&tid))
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
                let prompt = load_prompt_from_disk(plan, Some(&clean));
                subagents.push(SubagentDetail {
                    name: format!("{role}-{clean}"),
                    task_id: Some(clean.clone()),
                    prompt,
                    started_at: None,
                    worked_duration: std::time::Duration::ZERO,
                    last_activity_at: None,
                    logs: vec![
                        format!("started task {clean}"),
                        format!("completed task {clean}"),
                    ],
                    thinking: String::new(),
                    content: String::new(),
                    is_active: false,
                    context_tokens: 0,
                });
            }
        }
    }

    // 7. Final pass: populate any missing prompts from disk for all subagents
    for sa in &mut subagents {
        if sa.prompt.is_empty() {
            let tid = sa
                .task_id
                .as_deref()
                .or_else(|| sa.name.rsplit_once('-').map(|(_, t)| t));
            let prompt = load_prompt_from_disk(plan, tid);
            if !prompt.is_empty() {
                sa.prompt = prompt;
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
                    crate::tool_names::TOOL_DELEGATE_TASK,
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
                    crate::tool_names::TOOL_DELEGATE_TASK,
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
        let plan = crate::manager::phase::Plan::at(tmp.path());
        plan.create("# Plan\n\n- [x] [t-001] Setup project (coder)\n- [ ] [t-002] Write tests\n")
            .unwrap();

        let subagents = rehydrate_subagents(&[], None, None, Some(&plan));
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].name, "coder-t-001");
        assert_eq!(subagents[0].task_id.as_deref(), Some("t-001"));
        assert!(!subagents[0].is_active);
    }

    #[test]
    fn test_is_abort_command_recognizes_variants() {
        assert!(is_abort_command("/abort"));
        assert!(is_abort_command("abort"));
        assert!(is_abort_command("ABORT"));
        assert!(is_abort_command("/exit"));
        assert!(is_abort_command("exit"));
        assert!(is_abort_command("/quit"));
        assert!(is_abort_command("quit"));
        assert!(is_abort_command("/q"));
        assert!(is_abort_command(":q"));
        assert!(is_abort_command(":q!"));
        assert!(is_abort_command("/stop"));
        assert!(is_abort_command("stop"));
        assert!(is_abort_command("/cancel"));
        assert!(is_abort_command("cancel"));
        assert!(!is_abort_command("continue"));
        assert!(!is_abort_command("status"));
    }

    #[test]
    fn test_format_tool_call_utf8_char_boundary_no_panic() {
        let cmd_prefix = "c".repeat(56);
        let cmd = format!("{cmd_prefix}—cargo check");
        let args = serde_json::json!({ "command": cmd });
        let formatted = format_tool_call_display(crate::tool_names::TOOL_RUN_COMMAND, &args);
        assert!(formatted.starts_with("run_command("));
        assert!(formatted.ends_with("…)"));

        let custom_prefix = "d".repeat(56);
        let custom_args = serde_json::json!({ "arg": format!("{custom_prefix}—value") });
        let custom_formatted = format_tool_call_display("custom", &custom_args);
        assert!(custom_formatted.starts_with("custom("));
        assert!(custom_formatted.ends_with("…)"));
    }

    #[test]
    fn test_rehydrate_subagents_active_when_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = crate::manager::phase::Plan::at(tmp.path());
        plan.create("# Plan\n\n- [ ] [t-001] In-flight task (coder)\n")
            .unwrap();

        // Delegation started, but NO matching tool response in messages
        let messages = vec![
            Message::System {
                content: "sys".to_string(),
            },
            Message::Assistant {
                content: Some("Delegating to coder".to_string()),
                reasoning_content: None,
                tool_calls: vec![ToolCall::new(
                    "call_in_flight",
                    crate::tool_names::TOOL_DELEGATE_TASK,
                    serde_json::json!({
                        "agent_name": "coder",
                        "task_id": "t-001",
                        "prompt": "finish this task"
                    })
                    .to_string(),
                )],
            },
        ];

        let subagents = rehydrate_subagents(&messages, None, None, Some(&plan));
        assert_eq!(subagents.len(), 1);
        let s = &subagents[0];
        assert_eq!(s.name, "coder-t-001");
        assert_eq!(s.task_id.as_deref(), Some("t-001"));
        assert!(
            s.is_active,
            "in-flight subagent must be rehydrated as active"
        );
        assert!(s.started_at.is_some(), "started_at must be populated");
        assert!(
            s.last_activity_at.is_some(),
            "last_activity_at must be populated"
        );
        assert_eq!(s.logs, vec!["started task t-001"]);
    }

    #[test]
    fn test_rehydrate_subagents_from_crash_journal_frozen() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = crate::orchestrator::freeze::CrashJournal::new(tmp.path());

        let req = crate::agents::DelegationRequest {
            agent_name: crate::agents::Agent::Debugger,
            prompt: "Diagnose crash dump".to_string(),
            snippets: vec![],
            task_id: Some("t-debug-1".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let _ = journal
            .snapshot(crate::agents::Agent::Debugger, &req)
            .unwrap();
        assert!(journal.is_frozen());

        let subagents = rehydrate_subagents(&[], Some(&journal), None, None);
        assert_eq!(subagents.len(), 1);
        let s = &subagents[0];
        assert_eq!(s.name, "debugger-t-debug-1");
        assert_eq!(s.task_id.as_deref(), Some("t-debug-1"));
        assert_eq!(s.prompt, "Diagnose crash dump");
        assert!(s.is_active, "frozen subagent must be rehydrated as active");
        assert!(s.logs.contains(&"recovering interrupted task".to_string()));
    }

    #[test]
    fn test_rehydrate_subagents_loads_prompt_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = crate::manager::phase::Plan::at(tmp.path());
        plan.create("# Plan\n\n- [x] [t-005] Completed task (coder)\n")
            .unwrap();

        // Write a synthesized prompt to .marmel/prompts/t-005.md
        let prompts_dir = plan.dir().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        let prompt_file = prompts_dir.join("t-005.md");
        std::fs::write(
            &prompt_file,
            "# Synthesized Prompt for Coder\nYou are Coder.",
        )
        .unwrap();

        let subagents = rehydrate_subagents(&[], None, None, Some(&plan));
        assert_eq!(subagents.len(), 1);
        let s = &subagents[0];
        assert_eq!(s.name, "coder-t-005");
        assert_eq!(s.prompt, "# Synthesized Prompt for Coder\nYou are Coder.");
    }

    #[test]
    fn test_rehydrate_subagents_ui_transcript_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = crate::manager::phase::Plan::at(tmp.path());
        plan.create("# Plan\n\n- [ ] [t-001] Task 1\n").unwrap();

        let messages = vec![
            Message::System {
                content: "sys".to_string(),
            },
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall::new(
                    "call_1",
                    crate::tool_names::TOOL_DELEGATE_TASK,
                    serde_json::json!({
                        "agent_name": "coder",
                        "task_id": "t-001",
                        "prompt": "work"
                    })
                    .to_string(),
                )],
            },
        ];

        let mut ui_transcript = crate::ui::UiTranscript::new();
        ui_transcript.append(crate::ui::UiRecord::TaskCompleted {
            task_id: "t-001".to_string(),
        });

        let subagents =
            rehydrate_subagents_with_ui(&messages, None, None, Some(&plan), Some(&ui_transcript));
        assert_eq!(subagents.len(), 1);
        let s = &subagents[0];
        assert!(
            !s.is_active,
            "subagent completed in ui_transcript must not be active"
        );
        assert!(s.logs.contains(&"completed task t-001".to_string()));
    }
}
