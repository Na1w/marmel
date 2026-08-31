//! Tool harness: dispatcher and built-in tool implementations.

use crate::tool_names::{
    TERMINAL_GLOB, TERMINAL_GREP_SEARCH, TERMINAL_LIST_DIRECTORY, TERMINAL_READ_FILE,
    TERMINAL_REPLACE, TERMINAL_RUN_COMMAND, TERMINAL_WRITE_FILE, TOOL_ARCHIVE_PLAN,
    TOOL_CREATE_PLAN, TOOL_DELEGATE_TASK, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_LEAVE_VERDICT,
    TOOL_PTY_CLOSE, TOOL_PTY_LIST, TOOL_PTY_READ, TOOL_PTY_SPAWN, TOOL_PTY_WRITE, TOOL_READ_FILE,
    TOOL_REBIRTH, TOOL_REPLACE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
};
use std::sync::Arc;

pub mod fs;
pub mod monitor;
pub mod pty;
pub mod sandbox;
pub mod search;
pub mod workspace;

static MCP_MANAGER: std::sync::RwLock<Option<Arc<crate::mcp::McpManager>>> =
    std::sync::RwLock::new(None);

/// Register the global MCP manager for tool dispatch.
pub fn set_mcp_manager(manager: Arc<crate::mcp::McpManager>) {
    if let Ok(mut lock) = MCP_MANAGER.write() {
        *lock = Some(manager);
    }
}

/// Retrieve the active global MCP manager if available.
pub fn get_mcp_manager() -> Option<Arc<crate::mcp::McpManager>> {
    MCP_MANAGER.read().ok().and_then(|lock| lock.clone())
}

/// A single tool execution request as parsed from a ToolCall.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// The result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    /// Whether this was an error result.
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Resilience intervention counters tracked across the session.
#[derive(Debug, Default)]
pub struct HarnessStats {
    /// Number of text repetition loops truncated.
    pub repetition_breaks: std::sync::atomic::AtomicU64,
    /// Number of empty model responses recovered by nudge.
    pub empty_prods: std::sync::atomic::AtomicU64,
    /// Number of automated context prunings executed.
    pub context_compactions: std::sync::atomic::AtomicU64,
    /// Number of plain-text XML tool calls converted to JSON.
    pub xml_tool_rescues: std::sync::atomic::AtomicU64,
    /// Number of HTTP 503/502 retries performed.
    pub backend_retries: std::sync::atomic::AtomicU64,
    /// Number of rebirth checkpoints generated.
    pub session_rebirths: std::sync::atomic::AtomicU64,
    /// Number of steer-arbitrator decisions produced.
    pub steer_arbitrations: std::sync::atomic::AtomicU64,
}

impl HarnessStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_compaction(&self) {
        self.context_compactions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_rebirth(&self) {
        self.session_rebirths
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_repetition_break(&self) {
        self.repetition_breaks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_empty_prod(&self) {
        self.empty_prods
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_xml_rescue(&self) {
        self.xml_tool_rescues
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_backend_retry(&self) {
        self.backend_retries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_steer_arbitration(&self) {
        self.steer_arbitrations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The role requesting a tool execution, used to enforce the orchestration tool policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCaller {
    /// The Manager.
    Manager,
    /// A specialist identified by its role.
    Specialist(crate::agents::Agent),
}

/// Errors that can occur while dispatching a tool.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments for {tool}: {detail}")]
    BadArguments { tool: String, detail: String },
    #[error("tool `{tool}` is forbidden for caller `{caller}` by orchestration policy")]
    Forbidden { tool: String, caller: String },
    #[error("{0}")]
    Execution(#[from] anyhow::Error),
}

fn write_plan(md: &str) -> Result<ToolResult, ToolError> {
    crate::agent::phase::Plan::default()
        .create(md)
        .map(|_| ToolResult::ok("plan written to .marmel/execution_plan.md"))
        .map_err(ToolError::Execution)
}

fn archive_plan() -> Result<ToolResult, ToolError> {
    if crate::orchestrator::has_active_workers() {
        return Ok(ToolResult::err(
            "Cannot archive plan while background specialist workers are still actively running.",
        ));
    }
    let plan = crate::agent::phase::Plan::default();
    match plan.archive() {
        Ok(Some(dest)) => Ok(ToolResult::ok(format!(
            "plan archived to {}",
            dest.display()
        ))),
        Ok(None) => {
            if plan.plan_path().exists() {
                Ok(ToolResult::err(
                    "plan is not complete and cannot be archived yet",
                ))
            } else {
                Ok(ToolResult::ok("no plan file to archive"))
            }
        }
        Err(e) => Err(ToolError::Execution(e)),
    }
}

/// The primary dispatcher entry point, shared by the Manager and specialists.
pub fn dispatch(tool: &ToolInvocation) -> Result<ToolResult, ToolError> {
    if let Some(mcp) = get_mcp_manager()
        && mcp.has_tool(&tool.name)
    {
        let mcp_res = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(mcp.call_tool(&tool.name, &tool.arguments))
        });
        return match mcp_res {
            Ok(content) => Ok(ToolResult::ok(content)),
            Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
        };
    }

    let res = match tool.name.as_str() {
        TOOL_DELEGATE_TASK => crate::orchestrator::handle_delegate_task(&tool.arguments),
        TOOL_READ_FILE | TERMINAL_READ_FILE | "view_file" | "get_file" | "read" => {
            fs::read_file(&tool.arguments)
        }
        TOOL_REPLACE | TERMINAL_REPLACE | "replace_file_content" | "edit_file" => {
            fs::replace(&tool.arguments)
        }
        TOOL_WRITE_FILE | TERMINAL_WRITE_FILE | "create_file" | "write_to_file" | "save_file"
        | "write" => fs::write_file(&tool.arguments),
        TOOL_RUN_COMMAND | TERMINAL_RUN_COMMAND | "execute_command" | "run" | "exec" | "bash"
        | "sh" | "cmd" => pty::run_command(&tool.arguments),
        TOOL_GREP_SEARCH | TERMINAL_GREP_SEARCH | "grep" | "search" => {
            search::grep_search(&tool.arguments)
        }
        TOOL_GLOB | TERMINAL_GLOB | "find_files" | "glob_search" => search::glob(&tool.arguments),
        TOOL_PTY_SPAWN | "pty__spawn" => pty::pty_spawn(&tool.arguments),
        TOOL_PTY_WRITE | "pty__write" => pty::pty_write(&tool.arguments),
        TOOL_PTY_READ | "pty__read" => pty::pty_read(&tool.arguments),
        TOOL_PTY_CLOSE | "pty__close" => pty::pty_close(&tool.arguments),
        TOOL_PTY_LIST | "pty__list" => pty::pty_list(&tool.arguments),
        TOOL_CREATE_PLAN => {
            let plan_str = tool
                .arguments
                .get("plan_markdown")
                .or_else(|| tool.arguments.get("plan"))
                .and_then(serde_json::Value::as_str);
            match plan_str {
                Some(md) => write_plan(md),
                None => Ok(ToolResult::err(
                    "create_plan requires a `plan_markdown` or `plan` string argument",
                )),
            }
        }
        TOOL_ARCHIVE_PLAN => archive_plan(),
        TOOL_REBIRTH => Err(ToolError::BadArguments {
            tool: TOOL_REBIRTH.to_string(),
            detail: "rebirth requires a live ContextEngine; use dispatch_with_engine".to_string(),
        }),
        other => Err(ToolError::UnknownTool(other.to_string())),
    };
    res.map(apply_tool_output_length_limit)
}

pub fn handle_rebirth(
    engine: &mut crate::agent::ContextEngine,
    arguments: &serde_json::Value,
) -> Result<ToolResult, ToolError> {
    let summary = arguments
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::BadArguments {
            tool: TOOL_REBIRTH.to_string(),
            detail: "rebirth requires a `summary` string argument".to_string(),
        })?;
    engine.perform_rebirth(&summary);
    Ok(ToolResult::ok(
        "Rebirth: compacted history and summarized progress.",
    ))
}

pub fn dispatch_with_engine(
    tool: &ToolInvocation,
    engine: &mut crate::agent::ContextEngine,
) -> Result<ToolResult, ToolError> {
    if tool.name.as_str() == TOOL_REBIRTH {
        return handle_rebirth(engine, &tool.arguments);
    }
    dispatch(tool)
}

pub const MAX_TOOL_OUTPUT_CHARS: usize = 10_000;

pub fn apply_tool_output_length_limit(mut result: ToolResult) -> ToolResult {
    // If this is an execution plan, do NOT truncate so the orchestrator can read it
    let upper = result.content.to_ascii_uppercase();
    if upper.contains("# EXECUTION PLAN") || upper.contains("IMPLEMENTATION PLAN") {
        return result;
    }

    if result.content.len() > MAX_TOOL_OUTPUT_CHARS {
        let full_len = result.content.len();
        let head_len = 7_000;
        let tail_len = 2_000;
        let mut head_end = head_len;
        while head_end > 0 && !result.content.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let mut tail_start = full_len.saturating_sub(tail_len);
        while tail_start < full_len && !result.content.is_char_boundary(tail_start) {
            tail_start += 1;
        }

        let head = &result.content[..head_end];
        let tail = if tail_start > head_end {
            &result.content[tail_start..]
        } else {
            ""
        };
        let omitted = full_len.saturating_sub(head.len() + tail.len());

        result.content = format!(
            "{head}\n\n[... TRUNCATED {omitted} CHARACTERS (total: {full_len} chars). Use specific commands, filters, or paginated tools to inspect specific sections ...]\n\n{tail}"
        );
    }
    result
}

pub fn dispatch_for(tool: &ToolInvocation, caller: ToolCaller) -> Result<ToolResult, ToolError> {
    let res = if caller == ToolCaller::Manager {
        dispatch_manager(tool)
    } else {
        let ToolCaller::Specialist(agent) = caller else {
            unreachable!("non-Manager caller is a specialist");
        };
        dispatch_specialist(tool, agent)
    };
    res.map(apply_tool_output_length_limit)
}

fn dispatch_manager(tool: &ToolInvocation) -> Result<ToolResult, ToolError> {
    let name = tool.name.as_str();
    if let Some(mcp) = get_mcp_manager()
        && mcp.has_tool(name)
    {
        let mcp_res = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(mcp.call_tool(name, &tool.arguments))
        });
        return match mcp_res {
            Ok(content) => Ok(ToolResult::ok(content)),
            Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
        };
    }

    match name {
        TOOL_DELEGATE_TASK => crate::orchestrator::handle_delegate_task(&tool.arguments),
        TOOL_CREATE_PLAN => match tool
            .arguments
            .get("plan")
            .or_else(|| tool.arguments.get("plan_markdown"))
            .and_then(serde_json::Value::as_str)
        {
            Some(md) => write_plan(md),
            None => Ok(ToolResult::err(
                "create_plan requires a `plan` or `plan_markdown` string argument",
            )),
        },
        TOOL_ARCHIVE_PLAN => archive_plan(),
        TOOL_REBIRTH => Err(ToolError::BadArguments {
            tool: TOOL_REBIRTH.to_string(),
            detail: "rebirth requires a live ContextEngine; use dispatch_with_engine".to_string(),
        }),
        TOOL_READ_FILE => fs::read_file(&tool.arguments),
        TOOL_GREP_SEARCH => search::grep_search(&tool.arguments),
        TOOL_GLOB => search::glob(&tool.arguments),
        other => Err(ToolError::Forbidden {
            tool: other.to_string(),
            caller: "Manager".to_string(),
        }),
    }
}

fn normalize_tool_name(name: &str) -> String {
    match name {
        TOOL_READ_FILE | "view_file" | "get_file" | "read" => TERMINAL_READ_FILE.to_string(),
        TOOL_WRITE_FILE | "create_file" | "write_to_file" | "save_file" | "write" => {
            TERMINAL_WRITE_FILE.to_string()
        }
        TOOL_REPLACE | "replace_file_content" | "edit_file" => TERMINAL_REPLACE.to_string(),
        TOOL_RUN_COMMAND | "execute_command" | "run" | "exec" | "bash" | "sh" | "cmd" => {
            TERMINAL_RUN_COMMAND.to_string()
        }
        TOOL_GREP_SEARCH | "grep" | "search" => TERMINAL_GREP_SEARCH.to_string(),
        TOOL_GLOB | "find_files" | "glob_search" => TERMINAL_GLOB.to_string(),
        "list_directory" | "ls" | "list_files" => TERMINAL_LIST_DIRECTORY.to_string(),
        other => other.to_string(),
    }
}

fn dispatch_specialist(
    tool: &ToolInvocation,
    agent: crate::agents::Agent,
) -> Result<ToolResult, ToolError> {
    let name = tool.name.as_str();
    if name == TOOL_CREATE_PLAN {
        return Err(ToolError::Forbidden {
            tool: name.to_string(),
            caller: agent.as_str().to_string(),
        });
    }

    if let Some(mcp) = get_mcp_manager()
        && mcp.has_tool(name)
    {
        let mcp_res = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(mcp.call_tool(name, &tool.arguments))
        });
        return match mcp_res {
            Ok(content) => Ok(ToolResult::ok(content)),
            Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
        };
    }

    let registry = crate::orchestrator::SpecialistRegistry::canonical();
    let gate_name = normalize_tool_name(name);
    if !crate::orchestrator::caller_allows_tool(agent, &gate_name, &registry) {
        return Err(ToolError::Forbidden {
            tool: name.to_string(),
            caller: agent.as_str().to_string(),
        });
    }

    match name {
        TOOL_DELEGATE_TASK => crate::orchestrator::handle_delegate_task(&tool.arguments),
        TOOL_READ_FILE | TERMINAL_READ_FILE | "view_file" | "get_file" | "read" => {
            fs::read_file(&tool.arguments)
        }
        TOOL_REPLACE | TERMINAL_REPLACE | "replace_file_content" | "edit_file" => {
            fs::replace(&tool.arguments)
        }
        TOOL_WRITE_FILE | TERMINAL_WRITE_FILE | "create_file" | "write_to_file" | "save_file"
        | "write" => fs::write_file(&tool.arguments),
        TOOL_RUN_COMMAND | TERMINAL_RUN_COMMAND | "execute_command" | "run" | "exec" | "bash"
        | "sh" | "cmd" => pty::run_command(&tool.arguments),
        TOOL_GREP_SEARCH | TERMINAL_GREP_SEARCH | "grep" | "search" => {
            search::grep_search(&tool.arguments)
        }
        TOOL_GLOB | TERMINAL_GLOB | "find_files" | "glob_search" => search::glob(&tool.arguments),
        TOOL_PTY_SPAWN | "pty__spawn" => pty::pty_spawn(&tool.arguments),
        TOOL_PTY_WRITE | "pty__write" => pty::pty_write(&tool.arguments),
        TOOL_PTY_READ | "pty__read" => pty::pty_read(&tool.arguments),
        TOOL_PTY_CLOSE | "pty__close" => pty::pty_close(&tool.arguments),
        TOOL_PTY_LIST | "pty__list" => pty::pty_list(&tool.arguments),
        TOOL_LEAVE_VERDICT => {
            let verdict = tool
                .arguments
                .get("verdict")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("APPROVED");
            let comments = tool
                .arguments
                .get("comments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            Ok(ToolResult::ok(format!(
                "Verdict recorded via leave_verdict: {verdict} with comments: {comments}"
            )))
        }
        TOOL_REBIRTH => Err(ToolError::BadArguments {
            tool: TOOL_REBIRTH.to_string(),
            detail: "rebirth requires a live ContextEngine; use dispatch_with_engine".to_string(),
        }),
        other => Err(ToolError::UnknownTool(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ContextEngine;
    use crate::types::Message;

    #[test]
    fn test_harness_rebirth_collapses_to_four_messages() {
        let mut engine = ContextEngine::new(2048);
        engine.set_system_prompt("You are a coding assistant.".to_string());
        engine.set_goal("Refactor the parser.".to_string());
        engine.append(Message::User {
            content: "First instruction.".to_string(),
        });
        engine.append(Message::Assistant {
            content: Some("Working...".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        });
        engine.append(Message::User {
            content: "Final instruction distinct from the goal.".to_string(),
        });

        let args = serde_json::json!({
            "summary": "Completed initial refactoring steps."
        });
        let result = handle_rebirth(&mut engine, &args).unwrap();
        assert!(!result.is_error);
        assert_eq!(engine.messages().len(), 4);
    }

    #[test]
    fn test_harness_delegate_task_manager_routes_and_returns_deliverable() {
        let invocation = ToolInvocation {
            name: TOOL_DELEGATE_TASK.to_string(),
            arguments: serde_json::json!({
                "agent_name": "coder",
                "prompt": "write hello world",
                "task_id": "t-001"
            }),
        };
        let res = dispatch_for(&invocation, ToolCaller::Manager).unwrap();
        assert!(!res.is_error);
        assert!(res.content.contains("MISSION COMPLETE"));
    }
}
