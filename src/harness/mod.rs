//! Tool harness: dispatcher and built-in tool implementations.

use crate::tool_names::TOOL_LIST_DIRECTORY;
use crate::tool_names::{
    TERMINAL_GLOB, TERMINAL_GREP_SEARCH, TERMINAL_LIST_DIRECTORY, TERMINAL_READ_FILE,
    TERMINAL_REPLACE, TERMINAL_RUN_COMMAND, TERMINAL_SLEEP, TERMINAL_WRITE_FILE, TOOL_ARCHIVE_PLAN,
    TOOL_CREATE_PLAN, TOOL_DELEGATE_TASK, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_LEAVE_VERDICT,
    TOOL_PTY_CLOSE, TOOL_PTY_LIST, TOOL_PTY_READ, TOOL_PTY_SPAWN, TOOL_PTY_WRITE, TOOL_READ_FILE,
    TOOL_REBIRTH, TOOL_REPLACE, TOOL_REPLY_TO_ARBITRATOR, TOOL_RUN_COMMAND, TOOL_SLEEP,
    TOOL_WRITE_FILE,
};
use std::sync::Arc;

pub mod common;
pub mod fs;
pub mod monitor;
pub mod plan;
pub mod pty;
pub mod sandbox;
pub mod search;
pub mod sleep;
pub mod workspace;

pub use common::{HarnessStats, ToolCaller, ToolError, ToolInvocation, ToolResult};
use plan::{archive_plan, write_plan};
use sleep::{handle_sleep, handle_sleep_async};

static MCP_MANAGER: std::sync::RwLock<Option<Arc<crate::mcp::McpManager>>> =
    std::sync::RwLock::new(None);
static WORKSPACE_ROOT: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

tokio::task_local! {
    static SCOPED_WORKSPACE_ROOT: std::path::PathBuf;
}

/// Run an async future scoped to a specific workspace root (ideal for isolated tests & sub-workspaces).
pub async fn with_workspace_root<F, R>(root: impl Into<std::path::PathBuf>, f: F) -> R
where
    F: std::future::Future<Output = R>,
{
    let p = root.into();
    let abs = p.canonicalize().unwrap_or(p);
    SCOPED_WORKSPACE_ROOT.scope(abs, f).await
}

/// Explicitly configure the global workspace root directory.
pub fn set_workspace_root(root: impl Into<std::path::PathBuf>) {
    let p = root.into();
    let abs = p.canonicalize().unwrap_or(p);
    if let Ok(mut lock) = WORKSPACE_ROOT.write() {
        *lock = Some(abs);
    }
}

/// Retrieve the active workspace root directory (task-local if set, or global fallback).
pub fn get_workspace_root() -> std::path::PathBuf {
    if let Ok(scoped) = SCOPED_WORKSPACE_ROOT.try_with(|p| p.clone()) {
        return scoped;
    }
    if let Ok(lock) = WORKSPACE_ROOT.read()
        && let Some(ref p) = *lock
        && p.exists()
    {
        return p.clone();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let canonical = cwd.canonicalize().unwrap_or(cwd);
    if canonical.exists() {
        set_workspace_root(&canonical);
    }
    canonical
}

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

pub fn block_on_safe<F, R>(f: F) -> R
where
    F: std::future::Future<Output = R> + Send,
    R: Send,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(f))
            }
            _ => std::thread::scope(|s| {
                s.spawn(|| handle.block_on(f))
                    .join()
                    .expect("worker thread panicked during block_on_safe")
            }),
        }
    } else if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        rt.block_on(f)
    } else {
        panic!("failed to initialize tokio runtime for synchronous bridge");
    }
}

/// The primary dispatcher entry point, shared by the Manager and specialists.
pub fn dispatch(tool: &ToolInvocation) -> Result<ToolResult, ToolError> {
    if let Some(mcp) = get_mcp_manager()
        && mcp.has_tool(&tool.name)
    {
        let mcp_res = block_on_safe(mcp.call_tool(&tool.name, &tool.arguments));
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
        TOOL_SLEEP | TERMINAL_SLEEP | "wait" => handle_sleep(&tool.arguments),
        TOOL_REBIRTH => Err(ToolError::BadArguments {
            tool: TOOL_REBIRTH.to_string(),
            detail: "rebirth requires a live ContextEngine; use dispatch_with_engine".to_string(),
        }),
        other => Err(ToolError::UnknownTool(other.to_string())),
    };
    res.map(apply_tool_output_length_limit)
}

pub fn handle_rebirth(
    engine: &mut crate::manager::ContextEngine,
    arguments: &serde_json::Value,
) -> Result<ToolResult, ToolError> {
    if engine.consecutive_rebirths() > 0 {
        return Ok(ToolResult::err(
            "Rebirth checkpoint was already applied. You cannot invoke rebirth consecutively. You must make progress on your tasks before invoking rebirth again.",
        ));
    }
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
    engine: &mut crate::manager::ContextEngine,
) -> Result<ToolResult, ToolError> {
    if tool.name.as_str() == TOOL_REBIRTH {
        return handle_rebirth(engine, &tool.arguments);
    }
    engine.reset_consecutive_rebirths();
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
    dispatch_for_with_engine(tool, caller, None)
}

pub fn dispatch_for_with_engine(
    tool: &ToolInvocation,
    caller: ToolCaller,
    engine: Option<&mut crate::manager::ContextEngine>,
) -> Result<ToolResult, ToolError> {
    let caller_str = match &caller {
        ToolCaller::Manager => "Manager".to_string(),
        ToolCaller::Specialist(a) => format!("Specialist({a:?})"),
        ToolCaller::SpecialistWithTools { agent, .. } => format!("Specialist({agent:?})"),
    };
    crate::debug_log::log_tool_invocation(&caller_str, &tool.name, &tool.arguments);
    let start = std::time::Instant::now();

    let res = match caller {
        ToolCaller::Manager => dispatch_manager(tool, engine),
        specialist => dispatch_specialist(tool, specialist, engine),
    };

    let elapsed = start.elapsed().as_millis();
    match &res {
        Ok(r) => crate::debug_log::log_tool_result(
            &caller_str,
            &tool.name,
            elapsed,
            &r.content,
            r.is_error,
        ),
        Err(e) => crate::debug_log::log_tool_result(
            &caller_str,
            &tool.name,
            elapsed,
            &e.to_string(),
            true,
        ),
    }

    res.map(apply_tool_output_length_limit)
}

pub async fn dispatch_for_async(
    tool: &ToolInvocation,
    caller: ToolCaller,
) -> Result<ToolResult, ToolError> {
    dispatch_for_async_with_engine(tool, caller, None).await
}

pub async fn dispatch_for_async_with_engine(
    tool: &ToolInvocation,
    caller: ToolCaller,
    engine: Option<&mut crate::manager::ContextEngine>,
) -> Result<ToolResult, ToolError> {
    let caller_str = match &caller {
        ToolCaller::Manager => "Manager".to_string(),
        ToolCaller::Specialist(a) => format!("Specialist({a:?})"),
        ToolCaller::SpecialistWithTools { agent, .. } => format!("Specialist({agent:?})"),
    };
    crate::debug_log::log_tool_invocation(&caller_str, &tool.name, &tool.arguments);
    let start = std::time::Instant::now();

    let res = match caller {
        ToolCaller::Manager => dispatch_manager_async(tool, engine).await,
        specialist => dispatch_specialist_async(tool, specialist, engine).await,
    };

    let elapsed = start.elapsed().as_millis();
    match &res {
        Ok(r) => crate::debug_log::log_tool_result(
            &caller_str,
            &tool.name,
            elapsed,
            &r.content,
            r.is_error,
        ),
        Err(e) => crate::debug_log::log_tool_result(
            &caller_str,
            &tool.name,
            elapsed,
            &e.to_string(),
            true,
        ),
    }

    res.map(apply_tool_output_length_limit)
}

async fn dispatch_manager_async(
    tool: &ToolInvocation,
    mut engine: Option<&mut crate::manager::ContextEngine>,
) -> Result<ToolResult, ToolError> {
    let name = tool.name.as_str();
    if let Some(mcp) = get_mcp_manager()
        && mcp.has_tool(name)
    {
        return match mcp.call_tool(name, &tool.arguments).await {
            Ok(content) => Ok(ToolResult::ok(content)),
            Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
        };
    }

    if name != TOOL_REBIRTH
        && let Some(ref mut eng) = engine
    {
        eng.reset_consecutive_rebirths();
    }

    match name {
        TOOL_DELEGATE_TASK => crate::orchestrator::handle_delegate_task(&tool.arguments),
        TOOL_CREATE_PLAN => match tool
            .arguments
            .get("plan")
            .or_else(|| tool.arguments.get("plan_markdown"))
            .and_then(serde_json::Value::as_str)
        {
            Some(md) => Box::pin(plan::write_plan_async(md)).await,
            None => Ok(ToolResult::err(
                "create_plan requires a `plan` or `plan_markdown` string argument",
            )),
        },
        TOOL_ARCHIVE_PLAN => archive_plan(),
        TOOL_REBIRTH => {
            if let Some(eng) = engine {
                handle_rebirth(eng, &tool.arguments)
            } else {
                Err(ToolError::BadArguments {
                    tool: TOOL_REBIRTH.to_string(),
                    detail: "rebirth requires a live ContextEngine; use dispatch_with_engine"
                        .to_string(),
                })
            }
        }
        TOOL_READ_FILE => fs::read_file(&tool.arguments),
        TOOL_GREP_SEARCH => search::grep_search(&tool.arguments),
        TOOL_GLOB => search::glob(&tool.arguments),
        TOOL_SLEEP | TERMINAL_SLEEP | "wait" => handle_sleep_async(&tool.arguments).await,
        TOOL_REPLY_TO_ARBITRATOR => {
            handle_reply_to_arbitrator_async("Manager", &tool.arguments).await
        }
        other => Err(ToolError::Forbidden {
            tool: other.to_string(),
            caller: "Manager".to_string(),
        }),
    }
}

async fn dispatch_specialist_async(
    tool: &ToolInvocation,
    caller: ToolCaller,
    mut engine: Option<&mut crate::manager::ContextEngine>,
) -> Result<ToolResult, ToolError> {
    let name = tool.name.as_str();
    let caller_str = caller.role_name();
    if name == TOOL_CREATE_PLAN {
        return Err(ToolError::Forbidden {
            tool: name.to_string(),
            caller: caller_str,
        });
    }

    if let Some(mcp) = get_mcp_manager()
        && mcp.has_tool(name)
    {
        return match mcp.call_tool(name, &tool.arguments).await {
            Ok(content) => Ok(ToolResult::ok(content)),
            Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
        };
    }

    let gate_name = normalize_tool_name(name);
    let is_allowed = if gate_name == TOOL_REPLY_TO_ARBITRATOR {
        true
    } else {
        match &caller {
            ToolCaller::SpecialistWithTools { allowed_tools, .. } => {
                allowed_tools.iter().any(|t| {
                    let norm = normalize_tool_name(t);
                    norm == gate_name || t == name || t == &gate_name || norm == name
                })
            }
            ToolCaller::Specialist(agent) => {
                let registry = crate::orchestrator::SpecialistRegistry::canonical();
                crate::orchestrator::caller_allows_tool(*agent, &gate_name, &registry)
            }
            ToolCaller::Manager => false,
        }
    };

    if !is_allowed {
        return Err(ToolError::Forbidden {
            tool: name.to_string(),
            caller: caller_str,
        });
    }

    if name != TOOL_REBIRTH
        && let Some(ref mut eng) = engine
    {
        eng.reset_consecutive_rebirths();
    }

    match name {
        TOOL_READ_FILE | TERMINAL_READ_FILE | "view_file" | "get_file" | "read" => {
            fs::read_file(&tool.arguments)
        }
        TOOL_WRITE_FILE | TERMINAL_WRITE_FILE | "create_file" | "write_to_file" | "save_file"
        | "write" => fs::write_file(&tool.arguments),
        TOOL_REPLACE | TERMINAL_REPLACE | "replace_file_content" | "edit_file" => {
            fs::replace(&tool.arguments)
        }
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
        TOOL_DELEGATE_TASK => crate::orchestrator::handle_delegate_task(&tool.arguments),
        name if crate::agents::validation::is_leave_verdict_tool(name) => {
            let (approved, comments) = crate::agents::validation::parse_verdict_args(
                &tool.arguments,
            )
            .ok_or_else(|| ToolError::BadArguments {
                tool: TOOL_LEAVE_VERDICT.to_string(),
                detail: "missing mandatory string field `verdict` ('APPROVED' or 'REJECTED')"
                    .to_string(),
            })?;
            let verdict = if approved { "APPROVED" } else { "REJECTED" };
            Ok(ToolResult::ok(format!(
                "Verdict recorded via leave_verdict: {verdict} with comments: {comments}"
            )))
        }
        TOOL_SLEEP | TERMINAL_SLEEP | "wait" => handle_sleep_async(&tool.arguments).await,
        TOOL_REPLY_TO_ARBITRATOR => {
            handle_reply_to_arbitrator_async(&caller_str, &tool.arguments).await
        }
        TOOL_REBIRTH => {
            if let Some(eng) = engine {
                handle_rebirth(eng, &tool.arguments)
            } else {
                Err(ToolError::BadArguments {
                    tool: TOOL_REBIRTH.to_string(),
                    detail: "rebirth requires a live ContextEngine; use dispatch_with_engine"
                        .to_string(),
                })
            }
        }
        other => Err(ToolError::UnknownTool(other.to_string())),
    }
}

pub async fn handle_reply_to_arbitrator_async(
    caller: &str,
    args: &serde_json::Value,
) -> Result<ToolResult, ToolError> {
    let notice_id = args
        .get("notice_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::BadArguments {
            tool: TOOL_REPLY_TO_ARBITRATOR.to_string(),
            detail: "missing mandatory string field `notice_id`".to_string(),
        })?;
    let message = args
        .get("message")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::BadArguments {
            tool: TOOL_REPLY_TO_ARBITRATOR.to_string(),
            detail: "missing mandatory string field `message`".to_string(),
        })?;

    let record_res = crate::orchestrator::record_worker_reply(caller, notice_id, message);
    let notice = match record_res {
        Ok(n) => n,
        Err(_) => {
            if let Some(r) = crate::orchestrator::get_pending_notice(notice_id) {
                r
            } else {
                crate::orchestrator::SteerNotice {
                    notice_id: notice_id.to_string(),
                    user_inquiry: "Status update from specialist worker".to_string(),
                    target_worker: caller.to_string(),
                    created_at_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                }
            }
        }
    };

    crate::orchestrator::emit_status(format!(
        "[{caller}] Replied to notice {notice_id} — Steer Arbitrator evaluating..."
    ));

    let cfg = crate::config::get_active()
        .or_else(|| crate::config::load(None).ok())
        .unwrap_or_default();
    let client = crate::llm::ChatClient::from_config(&cfg);
    let stats = Arc::new(HarnessStats::new());

    let eval_res = crate::orchestrator::evaluate_worker_reply(
        &client,
        &stats,
        &notice,
        caller,
        message,
        |_delta| {},
    )
    .await;

    match eval_res {
        Ok(eval) => {
            if eval.decision.eq_ignore_ascii_case("AskFollowUp")
                && let Some(follow_up) = eval.follow_up_prompt
            {
                let follow_up_notice = crate::orchestrator::post_notice_to_worker(
                    caller,
                    &follow_up,
                    Some(notice_id),
                );
                if let Some(user_status) = eval.user_status {
                    crate::orchestrator::emit_event(crate::ui::Event::SteerResponse(format!(
                        "\n[Arbitrator]: {user_status}\n\n"
                    )));
                } else {
                    crate::orchestrator::emit_status(format!(
                        "[Arbitrator]: Follow-up question sent to {caller} ({})",
                        follow_up_notice.notice_id
                    ));
                }
                Ok(ToolResult::ok(format!(
                    "Reply to notice '{notice_id}' received by Arbitrator. Arbitrator has posted follow-up question: \"{follow_up}\". Check notices and reply when ready."
                )))
            } else {
                let resp = eval
                    .response
                    .unwrap_or_else(|| format!("Specialist [{caller}] explains: {message}"));
                crate::orchestrator::emit_event(crate::ui::Event::SteerResponse(format!(
                    "\n[Arbitrator]: {resp}\n\n"
                )));
                Ok(ToolResult::ok(format!(
                    "Reply to Arbitrator for notice '{notice_id}' recorded successfully (original inquiry: \"{}\").",
                    notice.user_inquiry
                )))
            }
        }
        Err(e) => {
            let fallback_resp = format!("Specialist [{caller}] explains: {message}");
            crate::orchestrator::emit_event(crate::ui::Event::SteerResponse(format!(
                "\n[Arbitrator]: {fallback_resp}\n\n"
            )));
            Ok(ToolResult::ok(format!(
                "Reply to Arbitrator for notice '{notice_id}' recorded successfully (original inquiry: \"{}\", note: {e}).",
                notice.user_inquiry
            )))
        }
    }
}

pub fn handle_reply_to_arbitrator(
    caller: &str,
    args: &serde_json::Value,
) -> Result<ToolResult, ToolError> {
    block_on_safe(handle_reply_to_arbitrator_async(caller, args))
}

fn dispatch_manager(
    tool: &ToolInvocation,
    mut engine: Option<&mut crate::manager::ContextEngine>,
) -> Result<ToolResult, ToolError> {
    let name = tool.name.as_str();
    if let Some(mcp) = get_mcp_manager()
        && mcp.has_tool(name)
    {
        let mcp_res = block_on_safe(mcp.call_tool(name, &tool.arguments));
        return match mcp_res {
            Ok(content) => Ok(ToolResult::ok(content)),
            Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
        };
    }

    if name != TOOL_REBIRTH
        && let Some(ref mut eng) = engine
    {
        eng.reset_consecutive_rebirths();
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
        TOOL_REBIRTH => {
            if let Some(eng) = engine {
                handle_rebirth(eng, &tool.arguments)
            } else {
                Err(ToolError::BadArguments {
                    tool: TOOL_REBIRTH.to_string(),
                    detail: "rebirth requires a live ContextEngine; use dispatch_with_engine"
                        .to_string(),
                })
            }
        }
        TOOL_READ_FILE => fs::read_file(&tool.arguments),
        TOOL_GREP_SEARCH => search::grep_search(&tool.arguments),
        TOOL_GLOB => search::glob(&tool.arguments),
        TOOL_SLEEP | TERMINAL_SLEEP | "wait" => handle_sleep(&tool.arguments),
        TOOL_REPLY_TO_ARBITRATOR => handle_reply_to_arbitrator("Manager", &tool.arguments),
        other => Err(ToolError::Forbidden {
            tool: other.to_string(),
            caller: "Manager".to_string(),
        }),
    }
}

pub(crate) fn normalize_tool_name(name: &str) -> String {
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
        TOOL_SLEEP | TERMINAL_SLEEP | "wait" => TERMINAL_SLEEP.to_string(),
        TOOL_LIST_DIRECTORY | "ls" | "list_files" => TERMINAL_LIST_DIRECTORY.to_string(),
        other => other.to_string(),
    }
}

fn dispatch_specialist(
    tool: &ToolInvocation,
    caller: ToolCaller,
    mut engine: Option<&mut crate::manager::ContextEngine>,
) -> Result<ToolResult, ToolError> {
    let name = tool.name.as_str();
    let caller_str = caller.role_name();
    if name == TOOL_CREATE_PLAN {
        return Err(ToolError::Forbidden {
            tool: name.to_string(),
            caller: caller_str,
        });
    }

    if let Some(mcp) = get_mcp_manager()
        && mcp.has_tool(name)
    {
        let mcp_res = block_on_safe(mcp.call_tool(name, &tool.arguments));
        return match mcp_res {
            Ok(content) => Ok(ToolResult::ok(content)),
            Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
        };
    }

    let gate_name = normalize_tool_name(name);
    let is_allowed = if gate_name == TOOL_REPLY_TO_ARBITRATOR {
        true
    } else {
        match &caller {
            ToolCaller::SpecialistWithTools { allowed_tools, .. } => {
                allowed_tools.iter().any(|t| {
                    let norm = normalize_tool_name(t);
                    norm == gate_name || t == name || t == &gate_name || norm == name
                })
            }
            ToolCaller::Specialist(agent) => {
                let registry = crate::orchestrator::SpecialistRegistry::canonical();
                crate::orchestrator::caller_allows_tool(*agent, &gate_name, &registry)
            }
            ToolCaller::Manager => false,
        }
    };

    if !is_allowed {
        return Err(ToolError::Forbidden {
            tool: name.to_string(),
            caller: caller_str,
        });
    }

    if name != TOOL_REBIRTH
        && let Some(ref mut eng) = engine
    {
        eng.reset_consecutive_rebirths();
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
        name if crate::agents::validation::is_leave_verdict_tool(name) => {
            let (approved, comments) = crate::agents::validation::parse_verdict_args(
                &tool.arguments,
            )
            .ok_or_else(|| ToolError::BadArguments {
                tool: TOOL_LEAVE_VERDICT.to_string(),
                detail: "missing mandatory string field `verdict` ('APPROVED' or 'REJECTED')"
                    .to_string(),
            })?;
            let verdict = if approved { "APPROVED" } else { "REJECTED" };
            Ok(ToolResult::ok(format!(
                "Verdict recorded via leave_verdict: {verdict} with comments: {comments}"
            )))
        }
        TOOL_SLEEP | TERMINAL_SLEEP | "wait" => handle_sleep(&tool.arguments),
        TOOL_REPLY_TO_ARBITRATOR => handle_reply_to_arbitrator(&caller_str, &tool.arguments),
        TOOL_REBIRTH => {
            if let Some(eng) = engine {
                handle_rebirth(eng, &tool.arguments)
            } else {
                Err(ToolError::BadArguments {
                    tool: TOOL_REBIRTH.to_string(),
                    detail: "rebirth requires a live ContextEngine; use dispatch_with_engine"
                        .to_string(),
                })
            }
        }
        other => Err(ToolError::UnknownTool(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::ContextEngine;
    use crate::types::Message;
    use plan::write_plan_internal;

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
    fn test_harness_consecutive_rebirth_rejected() {
        let mut engine = ContextEngine::new(2048);
        engine.set_system_prompt("You are a coding assistant.".to_string());
        engine.set_goal("Refactor the parser.".to_string());
        let args = serde_json::json!({
            "summary": "Completed initial refactoring steps."
        });
        // First rebirth succeeds
        let result1 = handle_rebirth(&mut engine, &args).unwrap();
        assert!(!result1.is_error);
        assert_eq!(engine.messages().len(), 4);

        // Immediate consecutive rebirth is rejected
        let result2 = handle_rebirth(&mut engine, &args).unwrap();
        assert!(result2.is_error);
        assert!(
            result2
                .content
                .contains("cannot invoke rebirth consecutively")
        );
        assert_eq!(engine.messages().len(), 4);

        // Another tool resets the consecutive tracker
        let inv = ToolInvocation {
            name: TOOL_GLOB.to_string(),
            arguments: serde_json::json!({ "pattern": "*.rs" }),
        };
        let _ = dispatch_with_engine(&inv, &mut engine);
        assert_eq!(engine.consecutive_rebirths(), 0);

        // Now rebirth succeeds again
        let result3 = handle_rebirth(&mut engine, &args).unwrap();
        assert!(!result3.is_error);
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

    #[test]
    fn test_run_plan_validation_skipped_when_disabled() {
        let mut cfg = crate::config::Config::default();
        let planner_spec = crate::config::SpecialistConfig {
            enable_validator: Some(false),
            ..Default::default()
        };
        cfg.orchestration
            .specialists
            .insert("planner".to_string(), planner_spec);

        let token = tokio_util::sync::CancellationToken::new();
        let res = block_on_safe(crate::agents::validation::run_plan_validation(
            "# Execution Plan\n- [ ] [t-001] Step one\n",
            &cfg,
            &token,
        ));
        assert!(res.is_ok());
        let (approved, comments) = res.unwrap();
        assert!(approved);
        assert!(comments.contains("skipped"));
    }

    #[test]
    fn test_harness_create_plan_missing_arguments_returns_error() {
        let invocation = ToolInvocation {
            name: TOOL_CREATE_PLAN.to_string(),
            arguments: serde_json::json!({}),
        };
        let res = dispatch_for(&invocation, ToolCaller::Manager).unwrap();
        assert!(res.is_error);
        assert!(res.content.contains("requires a `plan` or `plan_markdown`"));
    }

    #[test]
    fn test_harness_write_plan_internal_writes_to_custom_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = crate::manager::phase::Plan::at(tmp.path().join(".marmel"));
        let res = block_on_safe(async {
            write_plan_internal(
                "# Execution Plan\n## Phase 1\n- [ ] [t-001] Step one\n",
                Some(plan.clone()),
            )
            .await
        })
        .unwrap();

        if !res.is_error {
            assert!(res.content.contains("Execution plan written"));
            assert!(plan.plan_path().exists());
        } else {
            assert!(res.content.contains("Strategic Plan Auditor"));
        }
    }
}
