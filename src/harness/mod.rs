//! Tool harness: dispatcher and built-in tool implementations.

use crate::tool_names::{
    TERMINAL_GLOB, TERMINAL_GREP_SEARCH, TERMINAL_READ_FILE, TERMINAL_REPLACE,
    TERMINAL_RUN_COMMAND, TERMINAL_SLEEP, TERMINAL_WRITE_FILE, TOOL_ARCHIVE_PLAN, TOOL_CREATE_PLAN,
    TOOL_DELEGATE_TASK, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_LEAVE_VERDICT, TOOL_PTY_CLOSE,
    TOOL_PTY_LIST, TOOL_PTY_READ, TOOL_PTY_SPAWN, TOOL_PTY_WRITE, TOOL_READ_FILE, TOOL_REBIRTH,
    TOOL_REPLACE, TOOL_REPLY_TO_ARBITRATOR, TOOL_RUN_COMMAND, TOOL_SLEEP, TOOL_WRITE_FILE,
    is_sleep_tool_name,
};
use std::collections::HashSet;
use std::sync::Arc;

pub mod common;
#[cfg(test)]
#[path = "dispatch_integrity_tests.rs"]
mod dispatch_integrity_tests;
pub mod fs;
pub mod monitor;
pub mod plan;
pub mod pty;
pub mod sandbox;
pub mod search;
pub mod sleep;
#[cfg(test)]
mod tool_resolution_tests;
pub mod workspace;

pub use common::{HarnessStats, ToolCaller, ToolError, ToolInvocation, ToolResult};
use plan::{archive_plan, write_plan};
use sleep::{handle_sleep, handle_sleep_async};

/// The policy-filtered MCP registry (see [`McpDispatchGate`]); the single source of
/// truth for both the raw manager handle and the dispatch-allowed tool names.
static MCP_GATE: std::sync::RwLock<Option<Arc<McpDispatchGate>>> = std::sync::RwLock::new(None);
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

/// Every wire name the built-in dispatcher claims: the canonical spellings and
/// every alias spelling of the **one** tool-alias vocabulary
/// ([`crate::tool_names::TOOL_ALIAS_TABLE`]).
///
/// This is a re-export of a table *derived* from that vocabulary
/// ([`crate::tool_names::BUILTIN_TOOL_NAME_TABLE`] = canonical names ∪ alias
/// keys), so the dispatcher arms, the alias normalizer and the MCP name policy can
/// no longer disagree about which names are built-ins (security t-034c; vocabulary
/// dedup t-061). The MCP name policy consults this table so no MCP server can
/// publish a name that a built-in handler would answer to.
pub const BUILTIN_TOOL_NAMES: &[&str] = crate::tool_names::BUILTIN_TOOL_NAME_TABLE;

/// Resolve a wire tool name to the canonical spelling the dispatch arms below
/// match on, through the single alias table in [`crate::tool_names`]. Unknown
/// names (MCP composed names, typos) are returned verbatim, exactly as the
/// dispatcher's fall-through arms have always treated them.
fn canonical_dispatch_name(name: &str) -> &str {
    crate::tool_names::canonical_tool_spelling(name)
}

/// `true` when `name` is spelled by the built-in table verbatim.
pub fn is_builtin_tool_name(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name)
}

/// `true` when the built-in dispatcher would claim `name` — either verbatim or
/// after alias normalization (`normalize_tool_name` maps every accepted alias onto
/// its canonical form). Anything this predicate accepts must never be routed to an
/// MCP tool (t-034c: built-ins resolve first).
pub fn resolves_to_builtin_handler(name: &str) -> bool {
    is_builtin_tool_name(name) || is_builtin_tool_name(&normalize_tool_name(name))
}

/// Registration-time verdict for one discovered MCP tool (t-034c).
///
/// Layer 1 — the MCP name grammar plus the built-in namespace prefix check
/// ([`crate::mcp::validate_composed_registration`]).
/// Layer 2 — alias normalization: a composed name that *normalizes* onto a built-in
/// name is refused too, even when it is not spelled in the table.
///
/// Returns the composed wire name on success, or the reason for refusal. Nothing is
/// ever renamed or sanitized silently.
pub fn mcp_registration_verdict(
    server_name: &str,
    raw_tool_name: &str,
) -> Result<String, crate::mcp::McpNameError> {
    let qualified =
        crate::mcp::validate_composed_registration(server_name, raw_tool_name, BUILTIN_TOOL_NAMES)?;
    if resolves_to_builtin_handler(&qualified) {
        let builtin = normalize_tool_name(&qualified);
        return Err(crate::mcp::McpNameError::BuiltInCollision { qualified, builtin });
    }
    Ok(qualified)
}

/// An MCP tool that the registration policy refused, with the reason to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolRejection {
    /// The wire name the server proposed.
    pub qualified_name: String,
    /// Why the name was refused.
    pub reason: String,
}

/// The dispatch-visible view of the MCP registry: only names that survived the name
/// policy can be routed (t-034c).
pub struct McpDispatchGate {
    manager: Arc<crate::mcp::McpManager>,
    allowed: HashSet<String>,
    rejected: Vec<McpToolRejection>,
}

impl std::fmt::Debug for McpDispatchGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpDispatchGate")
            .field("allowed", &self.allowed)
            .field("rejected", &self.rejected)
            .finish_non_exhaustive()
    }
}

impl McpDispatchGate {
    /// Build the gate from a booted manager, refusing every tool whose name the
    /// policy rejects. Refusals are recorded (and logged by [`set_mcp_manager`]),
    /// never renamed.
    fn build(manager: &Arc<crate::mcp::McpManager>) -> Self {
        let mut allowed = HashSet::new();
        let mut rejected = Vec::new();
        for tool in manager.tools() {
            match mcp_registration_verdict(&tool.server_name, &tool.name) {
                Ok(qualified) => {
                    allowed.insert(qualified);
                }
                Err(error) => rejected.push(McpToolRejection {
                    qualified_name: tool.qualified_name(),
                    reason: error.to_string(),
                }),
            }
        }
        Self {
            manager: Arc::clone(manager),
            allowed,
            rejected,
        }
    }

    /// Route `name` to the MCP manager, if the policy allows it.
    ///
    /// Built-in names always lose: a name the built-in dispatcher claims is never
    /// routed to MCP, even if it somehow reached the allowed set. That is the
    /// dispatch-time half of the t-034c defence; [`set_mcp_manager`] is the primary,
    /// registration-time half.
    pub fn route(&self, name: &str) -> Option<&Arc<crate::mcp::McpManager>> {
        if resolves_to_builtin_handler(name) {
            if self.manager.has_tool(name) {
                tracing::warn!(
                    "MCP tool '{name}' is ignored at dispatch: the name is owned by the built-in dispatcher, which resolves first"
                );
            }
            return None;
        }
        if self.allowed.contains(name) {
            return Some(&self.manager);
        }
        None
    }

    /// The names that survived registration, sorted for stable assertions.
    pub fn allowed_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.allowed.iter().cloned().collect();
        names.sort();
        names
    }

    /// The tools the policy refused at registration.
    pub fn rejected(&self) -> &[McpToolRejection] {
        &self.rejected
    }
}

/// Register the global MCP manager for tool dispatch.
///
/// Registration is the primary t-034c defence: every discovered tool is checked
/// against the MCP name policy and against the built-in tool namespace. Refused
/// tools are reported with `tracing::warn!` and are never dispatchable — they are
/// rejected outright, never silently renamed.
pub fn set_mcp_manager(manager: Arc<crate::mcp::McpManager>) {
    let gate = Arc::new(McpDispatchGate::build(&manager));
    for rejection in gate.rejected() {
        tracing::warn!(
            "Refusing MCP tool '{}': {}",
            rejection.qualified_name,
            rejection.reason
        );
    }
    if let Ok(mut lock) = MCP_GATE.write() {
        *lock = Some(gate);
    }
}

/// Retrieve the active global MCP manager if available.
pub fn get_mcp_manager() -> Option<Arc<crate::mcp::McpManager>> {
    get_mcp_gate().map(|gate| Arc::clone(&gate.manager))
}

/// Retrieve the policy-filtered MCP registry — the dispatch view of the manager.
pub fn get_mcp_gate() -> Option<Arc<McpDispatchGate>> {
    MCP_GATE.read().ok().and_then(|lock| lock.clone())
}

/// The MCP tools that survived the name policy, restricted to `servers`.
///
/// Tool *advertising* (the schema list handed to a model) should prefer this over
/// `McpManager::tools_for_servers`, so a name the policy refused is never even
/// offered to a model, let alone dispatched (t-034c).
pub fn allowed_mcp_tools(servers: &[String]) -> Vec<crate::mcp::McpTool> {
    let Some(gate) = get_mcp_gate() else {
        return Vec::new();
    };
    gate.manager
        .tools_for_servers(servers)
        .into_iter()
        .filter(|tool| gate.allowed.contains(tool.qualified_name().as_str()))
        .collect()
}

/// Resolve the manager that is allowed to serve `name` (see [`McpDispatchGate::route`]).
fn mcp_route(name: &str) -> Option<Arc<crate::mcp::McpManager>> {
    get_mcp_gate().and_then(|gate| gate.route(name).map(Arc::clone))
}

/// Execute an MCP tool call, or `None` when no MCP tool is routed for the invocation.
fn dispatch_mcp_sync(tool: &ToolInvocation) -> Option<Result<ToolResult, ToolError>> {
    let manager = mcp_route(&tool.name)?;
    let mcp_res = block_on_safe(manager.call_tool(&tool.name, &tool.arguments));
    Some(match mcp_res {
        Ok(content) => Ok(ToolResult::ok(content)),
        Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
    })
}

/// Async counterpart of [`dispatch_mcp_sync`].
async fn dispatch_mcp_async(tool: &ToolInvocation) -> Option<Result<ToolResult, ToolError>> {
    let manager = mcp_route(&tool.name)?;
    Some(match manager.call_tool(&tool.name, &tool.arguments).await {
        Ok(content) => Ok(ToolResult::ok(content)),
        Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
    })
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
///
/// Resolution order (t-034c): the built-in match runs FIRST. MCP tools are consulted
/// only for names the built-in dispatcher does not own, and only through the
/// registration gate ([`McpDispatchGate::route`]), so an MCP server can never claim
/// a built-in name and bypass the sandboxed built-in handlers.
///
/// Name resolution (t-061): the wire name is folded through the **one** tool-alias
/// vocabulary ([`crate::tool_names::TOOL_ALIAS_TABLE`]) by
/// [`canonical_dispatch_name`], and the arms below name canonical spellings only.
/// The alias arms that used to be re-typed in this function, in
/// `dispatch_specialist_async` and in `dispatch_specialist` are gone: one table,
/// three call sites.
pub fn dispatch(tool: &ToolInvocation) -> Result<ToolResult, ToolError> {
    let name = canonical_dispatch_name(&tool.name);
    let res = match name {
        TOOL_DELEGATE_TASK => crate::orchestrator::handle_delegate_task(&tool.arguments),
        TOOL_READ_FILE | TERMINAL_READ_FILE => fs::read_file(&tool.arguments),
        TOOL_REPLACE | TERMINAL_REPLACE => fs::replace(&tool.arguments),
        TOOL_WRITE_FILE | TERMINAL_WRITE_FILE => fs::write_file(&tool.arguments),
        TOOL_RUN_COMMAND | TERMINAL_RUN_COMMAND => pty::run_command(&tool.arguments),
        TOOL_GREP_SEARCH | TERMINAL_GREP_SEARCH => search::grep_search(&tool.arguments),
        TOOL_GLOB | TERMINAL_GLOB => search::glob(&tool.arguments),
        TOOL_PTY_SPAWN => pty::pty_spawn(&tool.arguments),
        TOOL_PTY_WRITE => pty::pty_write(&tool.arguments),
        TOOL_PTY_READ => pty::pty_read(&tool.arguments),
        TOOL_PTY_CLOSE => pty::pty_close(&tool.arguments),
        TOOL_PTY_LIST => pty::pty_list(&tool.arguments),
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
        // Every accepted sleep spelling (the whole sleep family of the shared
        // table) normalizes onto `TERMINAL_SLEEP`, so one arm covers them all.
        TOOL_SLEEP | TERMINAL_SLEEP => handle_sleep(&tool.arguments),
        TOOL_REBIRTH => Err(ToolError::BadArguments {
            tool: TOOL_REBIRTH.to_string(),
            detail: "rebirth requires a live ContextEngine; use dispatch_with_engine".to_string(),
        }),
        // Built-in match exhausted: only now is MCP consulted, and only for names
        // the built-in dispatcher does not own (t-034c ordering). The *wire* name is
        // reported, never the normalized spelling, so callers keep the exact error
        // text they got before.
        _ => match dispatch_mcp_sync(tool) {
            Some(mcp_res) => return mcp_res,
            None => Err(ToolError::UnknownTool(tool.name.clone())),
        },
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

/// Clip an oversized tool result to a head + tail window (t-051 ordering note).
///
/// The cut keeps the **first** [`MAX_TOOL_OUTPUT_CHARS`]-ish window head and the
/// **last** 2 000 characters, so a footer that `read_file` appends when it hits
/// its byte ceiling — [`crate::harness::fs::READ_FILE_TRUNCATION_MARKER`] with
/// `truncated: true`, the offsets, and the re-run hint — always survives the
/// generic clip: it is at the very end of the payload and is far shorter than the
/// tail window. The guarantee is made structural rather than incidental: if a
/// footer ever starts inside the region that would be dropped, the tail window is
/// widened to begin at the marker, so the *reason* a result was clipped can never
/// be the first thing the model loses.
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
        // Ordering rule: never clip a `read_file` truncation report out of the
        // tail (see the doc above). Byte indices of a marker are char boundaries.
        if let Some(marker_at) = result.content.find(fs::READ_FILE_TRUNCATION_MARKER)
            && marker_at > head_end
            && marker_at < tail_start
        {
            tail_start = marker_at;
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
    // t-034c: no MCP pre-lookup. Built-in names are matched first and MCP is
    // consulted only in the fall-through arm, through the registration gate.
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
        // The Manager path advertises bare canonical names only, so its arms are
        // intentionally not alias-folded; its sleep arm still asks the shared
        // alias table, so the sleep vocabulary is identical on every path (t-061).
        n if is_sleep_tool_name(n) => handle_sleep_async(&tool.arguments).await,
        TOOL_REPLY_TO_ARBITRATOR => {
            handle_reply_to_arbitrator_async("Manager", &tool.arguments).await
        }
        // Built-in match exhausted: MCP is consulted last, and only through the
        // registration gate (t-034c ordering).
        other => match dispatch_mcp_async(tool).await {
            Some(mcp_res) => mcp_res,
            None => Err(ToolError::Forbidden {
                tool: other.to_string(),
                caller: "Manager".to_string(),
            }),
        },
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

    // t-034c ordering: `mcp_route` refuses any name the built-in dispatcher owns
    // (verbatim or via alias normalization), so a built-in handler can never be
    // shadowed by an MCP server. Names the built-ins do not own are served by the
    // MCP tool only if it survived the registration policy
    // (`set_mcp_manager` -> `McpDispatchGate`), and stay reachable for specialists
    // exactly as before.
    if let Some(mcp) = mcp_route(name) {
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

    // t-061: the arms below are matched on the canonical spelling produced by the
    // one shared alias table, never on re-typed alias arms.
    let canon = canonical_dispatch_name(name);
    match canon {
        TOOL_READ_FILE | TERMINAL_READ_FILE => fs::read_file(&tool.arguments),
        TOOL_WRITE_FILE | TERMINAL_WRITE_FILE => fs::write_file(&tool.arguments),
        TOOL_REPLACE | TERMINAL_REPLACE => fs::replace(&tool.arguments),
        TOOL_RUN_COMMAND | TERMINAL_RUN_COMMAND => pty::run_command(&tool.arguments),
        TOOL_GREP_SEARCH | TERMINAL_GREP_SEARCH => search::grep_search(&tool.arguments),
        TOOL_GLOB | TERMINAL_GLOB => search::glob(&tool.arguments),
        TOOL_PTY_SPAWN => pty::pty_spawn(&tool.arguments),
        TOOL_PTY_WRITE => pty::pty_write(&tool.arguments),
        TOOL_PTY_READ => pty::pty_read(&tool.arguments),
        TOOL_PTY_CLOSE => pty::pty_close(&tool.arguments),
        TOOL_PTY_LIST => pty::pty_list(&tool.arguments),
        TOOL_DELEGATE_TASK => crate::orchestrator::handle_delegate_task(&tool.arguments),
        // The verdict vocabulary is owned by `crate::agents::validation`, not by the
        // tool-alias table, so this arm keeps matching the wire name.
        _ if crate::agents::validation::is_leave_verdict_tool(name) => {
            // t-051: the allowlist admitted this call; the identity rule may not.
            verdict_role_gate(name, &caller)?;
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
        // One arm covers every accepted sleep spelling: the shared table maps them
        // all onto `TERMINAL_SLEEP` (t-061).
        TOOL_SLEEP | TERMINAL_SLEEP => handle_sleep_async(&tool.arguments).await,
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
        _ => Err(ToolError::UnknownTool(tool.name.clone())),
    }
}

/// The worker identity a specialist reply is attributed to (t-051).
///
/// The notice store accepts **both** spellings of one worker's identity and
/// resolves them with the same routing rules used for delivery
/// ([`crate::orchestrator::notice_addresses_worker`]): the effective registry key
/// carried by `ActiveWorkerGuard` (`coder-t-001`, or the collision-resolved
/// `coder-t-001#7`) and the bare role name the harness can see (`coder`), judged
/// in both directions.
///
/// **Reported accessor gap (t-051):** dispatch is handed a [`ToolCaller`] — a
/// role, never a registry key — and `crate::orchestrator::workers` keeps its
/// registry (`WORKERS`) private while exposing only key-*in* readers
/// ([`crate::orchestrator::get_active_worker_tokens`],
/// [`crate::orchestrator::set_active_worker_status`],
/// `update_active_worker_context`) plus two *display-string* formatters
/// ([`crate::orchestrator::get_active_subtasks_str`],
/// `get_active_specialist_context_str`). There is **no public accessor that
/// enumerates active worker keys or maps a role to its effective
/// `ActiveWorkerGuard` key**, and `crate::orchestrator::bus` only carries a
/// task-local cancellation token (`CURRENT_WORKER_TOKEN`), not a key. The
/// harness therefore uses the best available identity — the caller's role name —
/// and relies on the notice layer's bidirectional routing to match it against
/// the addressed key. If a public `active_worker_key_for_role(role)` accessor is
/// ever added to `orchestrator::workers`, this is the single place to adopt it;
/// nothing else in the reply path depends on the spelling.
fn reply_worker_identity(caller: &str) -> String {
    caller.trim().to_string()
}

/// Handle one `reply_to_arbitrator` call under the strict reply contract (t-051).
///
/// A reply must name the notice it answers, and that notice must address the
/// replying worker. The record is made by
/// [`crate::orchestrator::record_worker_reply_for_notice`], and **every**
/// rejection ([`crate::orchestrator::NoticeReplyRejection`]) is propagated to the
/// calling specialist as a `ToolError` whose text names the rejection kind: the
/// specialist sees why its reply was refused and can re-send with the right id.
///
/// The historical `Err(_)` branch here re-read `get_pending_notice(notice_id)` and,
/// when that missed, **synthesized a `SteerNotice`** from the caller and the raw
/// id — reintroducing exactly the guess-the-notice behaviour t-032d deleted at the
/// notice layer (a reply could be answered against a notice nobody named, and the
/// arbitrator could then synthesize a user-facing answer from an unrelated
/// `user_inquiry`). That branch is gone: a rejected reply records nothing,
/// resolves nothing, emits no `Event::SteerResponse`, and never reaches the
/// arbitrator.
pub async fn handle_reply_to_arbitrator_async(
    caller: &str,
    args: &serde_json::Value,
) -> Result<ToolResult, ToolError> {
    // An absent id is not a schema-only complaint: it is fed to the strict
    // record call so the specialist gets the same machine-readable rejection
    // class (`missing-notice-id`) as an empty or whitespace-only one.
    let requested_notice_id = args
        .get("notice_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let message = args
        .get("message")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::BadArguments {
            tool: TOOL_REPLY_TO_ARBITRATOR.to_string(),
            detail: "missing mandatory string field `message`".to_string(),
        })?;

    let worker_key = reply_worker_identity(caller);
    let notice = match crate::orchestrator::record_worker_reply_for_notice(
        &worker_key,
        requested_notice_id,
        message,
    ) {
        Ok(notice) => notice,
        Err(rejection) => {
            // No fallback, no synthesized notice, no arbitrator turn: the
            // rejection is the tool result the specialist must act on.
            return Err(ToolError::BadArguments {
                tool: TOOL_REPLY_TO_ARBITRATOR.to_string(),
                detail: format!(
                    "reply rejected ({}): {rejection} — nothing was recorded and no notice was resolved; send the exact `notice_id` shown as `ID: <notice_id>` in the delivered notice",
                    rejection.kind()
                ),
            });
        }
    };
    // The id the store actually resolved (the request was trimmed by the store).
    let notice_id = notice.notice_id.as_str();

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
                let follow_up_notice =
                    crate::orchestrator::post_notice_to_worker(caller, &follow_up, Some(notice_id));
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
                let follow_up_record = format!(
                    "Forwarded follow-up {} to {caller}: \"{follow_up}\" (awaiting specialist reply)\n[Specialist {caller} prior reply]: {message}",
                    follow_up_notice.notice_id
                );
                crate::orchestrator::record_steering_exchange(
                    Some(notice_id),
                    &notice.user_inquiry,
                    &follow_up_record,
                );
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
                let full_history_resp = if resp.contains(message) {
                    resp.clone()
                } else {
                    format!("{resp}\n[Specialist {caller}]: {message}")
                };
                crate::orchestrator::record_steering_exchange(
                    Some(notice_id),
                    &notice.user_inquiry,
                    &full_history_resp,
                );
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
            crate::orchestrator::record_steering_exchange(
                Some(notice_id),
                &notice.user_inquiry,
                &fallback_resp,
            );
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
    // t-034c: no MCP pre-lookup. Built-in names are matched first and MCP is
    // consulted only in the fall-through arm, through the registration gate.
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
        // See the note in `dispatch_manager_async`: the Manager arms stay
        // canonical-only, but the sleep vocabulary comes from the shared table.
        n if is_sleep_tool_name(n) => handle_sleep(&tool.arguments),
        TOOL_REPLY_TO_ARBITRATOR => handle_reply_to_arbitrator("Manager", &tool.arguments),
        // Built-in match exhausted: MCP is consulted last, and only through the
        // registration gate (t-034c ordering).
        other => match dispatch_mcp_sync(tool) {
            Some(mcp_res) => mcp_res,
            None => Err(ToolError::Forbidden {
                tool: other.to_string(),
                caller: "Manager".to_string(),
            }),
        },
    }
}

/// Fold a tool name onto the spelling the dispatch **gate** compares against.
///
/// t-061: this is no longer a hand-typed copy of the alias vocabulary — it is the
/// single table in [`crate::tool_names::TOOL_ALIAS_TABLE`]. Every site that needs
/// "which tool does this spelling mean" now asks that table, which is what keeps
/// the harness dispatcher, the specialist allowlist gate, the MCP name policy and
/// the steer grammar in agreement. Unknown names (MCP composed names, typos) are
/// returned verbatim, exactly as before.
pub(crate) fn normalize_tool_name(name: &str) -> String {
    crate::tool_names::normalize_tool_alias(name)
        .unwrap_or(name)
        .to_string()
}

/// Verdict identity rule at the **dispatch** layer (t-051).
///
/// The allowlist is an authority on *which tools a role may touch*, not on *who
/// may certify a deliverable*: the generalist's registry namespace is literally
/// `"*"` ([`crate::orchestrator::registry`] / `Generalist::tool_namespaces`), so
/// the allowlist check in [`dispatch_specialist`] admits `leave_verdict` for a
/// Generalist, and a prompt/blueprint allowlist (`SpecialistWithTools`) admits it
/// for any role the model was told about. Left alone, that let a worker approve
/// its own deliverable **through dispatch** even though the agent loop refuses
/// the same call.
///
/// The rule mirrors the agent loop's verdict role gate: it calls the existing
/// `pub` predicate [`crate::agents::runner::may_record_verdict`] (gate t-033b,
/// `src/agents/runner/execution.rs`, re-exported by `pub use execution::*`) rather
/// than restating its logic, so the identity rule lives in exactly one place.
/// A refusal is a `ToolError::Forbidden` plus a `tracing::warn!`, raised *before*
/// the verdict arguments are parsed or any verdict state is touched — there is no
/// partial write and no silent success.
fn verdict_role_gate(name: &str, caller: &ToolCaller) -> Result<(), ToolError> {
    let registry = crate::orchestrator::SpecialistRegistry::canonical();
    let permitted = caller
        .agent()
        .is_some_and(|agent| crate::agents::runner::may_record_verdict(agent, &registry));
    if permitted {
        return Ok(());
    }
    let caller_str = caller.role_name();
    tracing::warn!(
        "dispatch refused '{name}': only the {} role may record a validation verdict (caller '{caller_str}')",
        crate::agents::Agent::Validator.as_str()
    );
    Err(ToolError::Forbidden {
        tool: name.to_string(),
        caller: caller_str,
    })
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

    // t-034c ordering: `mcp_route` refuses any name the built-in dispatcher owns
    // (verbatim or via alias normalization), so a built-in handler can never be
    // shadowed by an MCP server. Names the built-ins do not own are served by the
    // MCP tool only if it survived the registration policy
    // (`set_mcp_manager` -> `McpDispatchGate`), and stay reachable for specialists
    // exactly as before.
    if let Some(mcp) = mcp_route(name) {
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

    // t-061: canonical spelling from the one shared alias table — this block used
    // to be a third hand-typed copy of the alias arms.
    let canon = canonical_dispatch_name(name);
    match canon {
        TOOL_DELEGATE_TASK => crate::orchestrator::handle_delegate_task(&tool.arguments),
        TOOL_READ_FILE | TERMINAL_READ_FILE => fs::read_file(&tool.arguments),
        TOOL_REPLACE | TERMINAL_REPLACE => fs::replace(&tool.arguments),
        TOOL_WRITE_FILE | TERMINAL_WRITE_FILE => fs::write_file(&tool.arguments),
        TOOL_RUN_COMMAND | TERMINAL_RUN_COMMAND => pty::run_command(&tool.arguments),
        TOOL_GREP_SEARCH | TERMINAL_GREP_SEARCH => search::grep_search(&tool.arguments),
        TOOL_GLOB | TERMINAL_GLOB => search::glob(&tool.arguments),
        TOOL_PTY_SPAWN => pty::pty_spawn(&tool.arguments),
        TOOL_PTY_WRITE => pty::pty_write(&tool.arguments),
        TOOL_PTY_READ => pty::pty_read(&tool.arguments),
        TOOL_PTY_CLOSE => pty::pty_close(&tool.arguments),
        TOOL_PTY_LIST => pty::pty_list(&tool.arguments),
        // The verdict vocabulary is owned by `crate::agents::validation`, not by the
        // tool-alias table, so this arm keeps matching the wire name.
        _ if crate::agents::validation::is_leave_verdict_tool(name) => {
            // t-051: the allowlist admitted this call; the identity rule may not.
            verdict_role_gate(name, &caller)?;
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
        // One arm covers every accepted sleep spelling (t-061).
        TOOL_SLEEP | TERMINAL_SLEEP => handle_sleep(&tool.arguments),
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
        // The *wire* name is reported, never the normalized spelling (t-061 keeps
        // error text byte-identical for names that never resolved).
        _ => Err(ToolError::UnknownTool(tool.name.clone())),
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
        // First rebirth succeeds. This engine carries only the pinned system
        // prompt and the pinned goal — there is no distinct user instruction —
        // so the corrected M12 shape collapses to THREE messages: the `[2]`
        // instruction slot is omitted rather than filled with a copy of the goal.
        let result1 = handle_rebirth(&mut engine, &args).unwrap();
        assert!(!result1.is_error);
        assert_eq!(engine.messages().len(), 3);
        assert!(
            matches!(engine.messages()[0], Message::System { .. }),
            "REQ-CORE-001: the system prompt stays at [0]"
        );
        assert!(
            matches!(engine.messages()[1], Message::User { .. }),
            "REQ-CORE-002: the pinned goal stays at [1]"
        );
        assert_eq!(
            engine.messages()[1].content().unwrap_or_default(),
            "Refactor the parser."
        );
        assert!(
            matches!(engine.messages()[2], Message::User { .. }),
            "M12: the rebirth checkpoint is emitted as a User message"
        );
        assert!(
            engine.messages()[2]
                .content()
                .unwrap_or_default()
                .starts_with(crate::manager::context::REBIRTH_CHECKPOINT_PREFIX),
            "the checkpoint follows the pinned goal directly"
        );
        assert_eq!(
            engine
                .messages()
                .iter()
                .filter(|m| m.content().unwrap_or_default() == "Refactor the parser.")
                .count(),
            1,
            "the goal is stated exactly once — no duplicated instruction slot"
        );

        // Immediate consecutive rebirth is rejected
        let result2 = handle_rebirth(&mut engine, &args).unwrap();
        assert!(result2.is_error);
        assert!(
            result2
                .content
                .contains("cannot invoke rebirth consecutively")
        );
        assert_eq!(
            engine.messages().len(),
            3,
            "a rejected rebirth must leave the collapsed shape untouched"
        );
        assert!(
            matches!(engine.messages()[0], Message::System { .. }),
            "the rejected rebirth must not disturb the pinned system prompt"
        );
        assert_eq!(
            engine.messages()[1].content().unwrap_or_default(),
            "Refactor the parser.",
            "the pinned goal survives the rejected rebirth"
        );
        assert_eq!(
            engine
                .messages()
                .iter()
                .filter(|m| m.content().unwrap_or_default() == "Refactor the parser.")
                .count(),
            1,
            "the rejected rebirth must not restate the goal"
        );

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
