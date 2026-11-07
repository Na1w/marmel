//! Tool harness: dispatcher and built-in tool implementations.

use crate::mcp::McpBootStatus;
use crate::tool_names::{
    TERMINAL_GLOB, TERMINAL_GREP_SEARCH, TERMINAL_READ_FILE, TERMINAL_REPLACE,
    TERMINAL_RUN_COMMAND, TERMINAL_SLEEP, TERMINAL_WRITE_FILE, TOOL_ARCHIVE_PLAN, TOOL_CREATE_PLAN,
    TOOL_DELEGATE_TASK, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_LEAVE_VERDICT, TOOL_PAUSE_EXECUTION,
    TOOL_PTY_CLOSE, TOOL_PTY_LIST, TOOL_PTY_READ, TOOL_PTY_SPAWN, TOOL_PTY_WRITE, TOOL_READ_FILE,
    TOOL_REBIRTH, TOOL_REPLACE, TOOL_REPLY_TO_ARBITRATOR, TOOL_RUN_COMMAND, TOOL_SLEEP,
    TOOL_WRITE_FILE, is_sleep_tool_name,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

pub mod common;
#[cfg(test)]
#[path = "dispatch_integrity_tests.rs"]
mod dispatch_integrity_tests;
pub mod fs;
pub mod monitor;
pub mod plan;
pub mod pty;
pub mod sandbox;
#[cfg(target_os = "macos")]
pub mod sandbox_seatbelt;
#[cfg(target_os = "windows")]
pub mod sandbox_windows;
pub mod sbpl;
pub mod search;
pub mod sleep;
#[cfg(test)]
mod tool_resolution_tests;
pub mod workspace;

use crate::tool_names::{TOOL_PAUSE_SUBAGENT, TOOL_RESUME_SUBAGENT};
pub use common::{HarnessStats, ToolCaller, ToolError, ToolInvocation, ToolResult};
use plan::{archive_plan, handle_pause_execution, write_plan};
use sleep::{handle_sleep, handle_sleep_async};

/// The policy-filtered MCP registry (see [`McpDispatchGate`]); the single source of
/// truth for both the raw manager handle and the dispatch-allowed tool names.
static MCP_GATE: std::sync::RwLock<Option<Arc<McpDispatchGate>>> = std::sync::RwLock::new(None);
static WORKSPACE_ROOT: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

tokio::task_local! {
    static SCOPED_WORKSPACE_ROOT: std::path::PathBuf;
    /// Per-task override of the MCP dispatch gate (see [`with_mcp_gate`]).
    static SCOPED_MCP_GATE: Arc<McpDispatchGate>;
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
/// ([`crate::tool_names::TOOL_ALIAS_TABLE`]). The table itself is re-exported
/// *derived* from that vocabulary
/// ([`crate::tool_names::BUILTIN_TOOL_NAME_TABLE`] = canonical names ∪ alias
/// keys): the dispatcher arms, the alias normalizer and the MCP name policy all
/// read one set, so no MCP server can publish a name a built-in handler would
/// answer to.
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
/// its canonical form). Anything this predicate accepts is never routed to an MCP
/// tool: built-ins resolve first.
pub fn resolves_to_builtin_handler(name: &str) -> bool {
    is_builtin_tool_name(name) || is_builtin_tool_name(&normalize_tool_name(name))
}

/// Registration-time verdict for one discovered MCP tool.
///
/// Layer 1 — the MCP name grammar plus the built-in namespace prefix check
/// ([`crate::mcp::validate_composed_registration`]). Layer 2 — alias normalization:
/// a composed name that *normalizes* onto a built-in name is also refused, even when
/// it is not spelled in the table. Returns the composed wire name, or the reason for
/// refusal; nothing is renamed or sanitized silently.
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

/// How long a single MCP tool call is allowed to wait for the MCP servers to come
/// up before it gives up and answers the model with a clean tool error.
///
/// **10 s.** A stdio server (`command = …`) costs one fork/exec plus the
/// `initialize` + `tools/list` round trip, and a localhost HTTP/SSE endpoint costs
/// one connect + two POSTs: milliseconds to a couple of seconds. 10 s absorbs the
/// cold-start cases that are real on this host (a JVM/Node runtime warming up, a
/// first container start, a network mount) without turning a tool call into an
/// open-ended stall, and it stays **far below the transport's own 30 s request
/// deadline** (`MCP_REQUEST_TIMEOUT`, `src/mcp/http.rs:42` — pinned by
/// `src/mcp/http_tests.rs:414-419`, and untouched here), so a wedged registry costs
/// the caller one bounded wait instead of the whole request budget twice: one tool
/// call is at worst `MCP_READY_TIMEOUT` (boot) + `MCP_REQUEST_TIMEOUT` (the call
/// itself), never a 60 s "is it still going".
///
/// It is never paid by startup or by advertising: the wait lives on the dispatch
/// path only ([`McpDispatchGate::gate_is_ready`]), and [`allowed_mcp_tools`] stays
/// synchronous and non-blocking (advertise-late, `docs/mcp_async_design.md`
/// Decision 4).
pub(crate) const MCP_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// The tool-error text for a registry whose boot has not settled in time.
fn mcp_still_starting_error(name: &str) -> String {
    // Name the *server* the model asked about when the wire name parses, so the text
    // is actionable, and fall back to the requested name verbatim otherwise.
    let server = crate::mcp::split_composed_name(name)
        .map(|(server, _)| server)
        .unwrap_or(name);
    format!(
        "MCP server '{server}' is still starting after {}s",
        MCP_READY_TIMEOUT.as_secs()
    )
}

/// The tool-error text for a boot that never brought a single configured server up.
fn mcp_boot_failed_error(reason: &str) -> String {
    format!("MCP boot failed: {reason}")
}

/// The dispatch-visible view of the MCP registry: only names that survived the name
/// policy can be routed.
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
    /// Build the dispatch view from a booted manager, refusing every tool whose
    /// name the policy rejects. Refusals are recorded (and logged by
    /// [`set_mcp_manager`]), never renamed.
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
    /// **The veto stays first, always:** built-in names always lose — a name the
    /// built-in dispatcher claims is never routed to MCP, even if it somehow reached
    /// the allowed set. [`set_mcp_manager`] refuses such names at registration; this
    /// is the dispatch-time half. Everything below widens *inside* the MCP-owns-the-name
    /// question and can never re-admit a vetoed name.
    ///
    /// `allowed` is a **snapshot** taken when the gate was built, and while the boot is
    /// in flight that snapshot is stale by construction: `src/main.rs` registers the
    /// gate of the still-filling registry (its `allowed` is empty) and only re-registers
    /// a recomputed gate once the verdict is `Ready`. Name resolution therefore falls
    /// back through three steps — the recommended **live-registry** policy (a), chosen
    /// because re-registration after readiness is itself a race: a request built
    /// against the *old* advertising can arrive after the old gate was published and
    /// before the new one is.
    ///
    /// 1. the frozen snapshot — the settled case, and the only one that ever applies
    ///    once the verdict is `Ready` (readiness re-publishes the snapshot);
    /// 2. the **live registry** (`McpManager::has_tool`), re-checked against the
    ///    registration verdict, so a tool that landed after this snapshot was taken is
    ///    dispatchable immediately, and a name the policy refused stays undispatchable;
    /// 3. while the verdict is *not* settled — `Booting`, or `Failed` (whose gate is
    ///    never re-published: `src/main.rs` re-registers only on `Ready`) — accept any
    ///    **policy-valid composed name**. The bounded readiness wait
    ///    ([`McpDispatchGate::gate_is_ready`]) then either lands the tool or answers a
    ///    clean tool error, instead of the `UnknownTool`/`Forbidden` dead end this
    ///    window used to produce.
    pub fn route(&self, name: &str) -> Option<&Arc<crate::mcp::McpManager>> {
        if resolves_to_builtin_handler(name) {
            if self.manager.has_tool(name) {
                tracing::warn!(
                    "MCP tool '{name}' is ignored at dispatch: the name is owned by the built-in dispatcher, which resolves first"
                );
            }
            return None;
        }
        if self.allowed.contains(name) || self.live_registry_serves(name) {
            return Some(&self.manager);
        }
        // Step 3: an unsettled verdict, and a name that only the composed-name
        // grammar can describe. Both verdicts below mean the same thing for routing:
        // the snapshot on this gate predates (or gave up on) the boot.
        if !matches!(self.manager.status(), McpBootStatus::Ready)
            && Self::composed_name_is_policy_valid(name)
        {
            return Some(&self.manager);
        }
        None
    }

    /// `true` when `name` is a composed MCP name the name policy would accept: it
    /// parses under the composition grammar and its verdict is `Ok` — which includes
    /// the built-in veto, because `mcp_registration_verdict` re-runs it.
    fn composed_name_is_policy_valid(name: &str) -> bool {
        let Some((server, raw)) = crate::mcp::split_composed_name(name) else {
            return false;
        };
        match mcp_registration_verdict(server, raw) {
            // The recomposition must be exact: `name` is the wire name a policy-valid
            // registration of that server/tool pair publishes, nothing else.
            Ok(qualified) => qualified == name,
            Err(_) => false,
        }
    }

    /// `true` when the **live** registry can answer `name` right now: a policy-valid
    /// composed name whose server has been booted and registered (see
    /// `McpManager::boot_one`, which registers the client only after the handshake and
    /// `tools/list` both succeeded).
    fn live_registry_serves(&self, name: &str) -> bool {
        Self::composed_name_is_policy_valid(name) && self.manager.has_tool(name)
    }

    /// Await this manager's final boot verdict, bounded by [`MCP_READY_TIMEOUT`].
    ///
    /// `None` = the budget elapsed with the registry still `Booting`. A
    /// `Ok(McpBootStatus::Booting)` (the publisher closed mid-boot without publishing a
    /// verdict; `shutdown_boot_task` publishes `Failed` instead) is treated exactly
    /// like the elapsed case.
    async fn await_verdict_within_ready_budget(&self) -> Option<McpBootStatus> {
        match tokio::time::timeout(MCP_READY_TIMEOUT, self.manager.await_ready()).await {
            // `Ok(McpBootStatus::Booting)` (the publisher closed mid-boot without ever
            // publishing a verdict; `shutdown_boot_task` publishes `Failed` instead)
            // means the same thing as the elapsed case: no verdict inside the budget.
            Ok(status @ (McpBootStatus::Ready | McpBootStatus::Failed(_))) => Some(status),
            _ => None,
        }
    }

    /// The readiness gate every async MCP dispatch goes through, **before** the tool
    /// call is made.
    ///
    /// `Err(text)` is not an exception: the callers hand the text straight back to the
    /// model as a `ToolResult::err`, the same channel every other MCP failure uses
    /// (`MCP tool error: …`), so a still-booting registry reads to the model as a tool
    /// error and never as a panic, a crash or an `UnknownTool`/`Forbidden` dead end.
    ///
    /// Never a wait where there is nothing to wait for:
    /// * a `Ready` verdict ends the question synchronously — the settled registry is
    ///   exactly what the snapshot already offers;
    /// * a `Failed` verdict answers with the boot's own assembled reason;
    /// * `Booting` **and** the tool already live: its server is up by construction, so
    ///   the call goes out without waiting on the *global* verdict (one slow server
    ///   must not stall a call to a server that is already answering).
    pub(crate) async fn gate_is_ready(&self, name: &str) -> Result<(), String> {
        match self.manager.status() {
            McpBootStatus::Ready => return Ok(()),
            McpBootStatus::Failed(reason) => return Err(mcp_boot_failed_error(&reason)),
            McpBootStatus::Booting => {}
        }
        if self.live_registry_serves(name) {
            return Ok(());
        }
        match self.await_verdict_within_ready_budget().await {
            Some(McpBootStatus::Ready) => Ok(()),
            Some(McpBootStatus::Failed(reason)) => Err(mcp_boot_failed_error(&reason)),
            Some(McpBootStatus::Booting) => Err(mcp_still_starting_error(name)),
            None => Err(mcp_still_starting_error(name)),
        }
    }

    /// Sync twin of [`McpDispatchGate::gate_is_ready`]: the same verdicts and the same
    /// budget, with the bounded wait driven through [`block_on_safe`] — the bridge
    /// every synchronous MCP call in this file already uses, and the only way a
    /// `spawn_blocking` thread (`src/ui/session.rs:2649-2660`, `:3304-3311`) gets a
    /// reactor *and* a timer for `tokio::time::timeout`: with no runtime around,
    /// `block_on_safe` builds a current-thread one via `enable_all()`, and inside a
    /// running runtime it re-enters the live handle. `tokio::time::timeout` on a plain
    /// thread with no reactor would panic, which is why this half never calls it
    /// directly.
    fn gate_is_ready_blocking(&self, name: &str) -> Result<(), String> {
        match self.manager.status() {
            McpBootStatus::Ready => return Ok(()),
            McpBootStatus::Failed(reason) => return Err(mcp_boot_failed_error(&reason)),
            McpBootStatus::Booting => {}
        }
        if self.live_registry_serves(name) {
            return Ok(());
        }
        let waited = block_on_safe(self.await_verdict_within_ready_budget());
        match waited {
            Some(McpBootStatus::Ready) => Ok(()),
            Some(McpBootStatus::Failed(reason)) => Err(mcp_boot_failed_error(&reason)),
            Some(McpBootStatus::Booting) | None => Err(mcp_still_starting_error(name)),
        }
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

/// Build the policy-filtered dispatch view of `manager` — the same gate
/// [`set_mcp_manager`] installs globally, without touching the process global.
/// Refusals are reported the same way (`tracing::warn!`), never renamed.
pub fn build_mcp_gate(manager: &Arc<crate::mcp::McpManager>) -> Arc<McpDispatchGate> {
    let gate = Arc::new(McpDispatchGate::build(manager));
    for rejection in gate.rejected() {
        tracing::warn!(
            "Refusing MCP tool '{}': {}",
            rejection.qualified_name,
            rejection.reason
        );
    }
    gate
}

/// Register the global MCP manager for tool dispatch.
///
/// Registration is the primary defence: every discovered tool is checked against
/// the MCP name policy and against the built-in tool namespace. Refusals are
/// reported with `tracing::warn!` and are never dispatchable — such a tool is
/// rejected outright, never silently renamed.
pub fn set_mcp_manager(manager: Arc<crate::mcp::McpManager>) {
    let gate = build_mcp_gate(&manager);
    if let Ok(mut lock) = MCP_GATE.write() {
        *lock = Some(gate);
    }
}

/// Run `f` with `gate` as the **calling task's** MCP dispatch gate.
///
/// [`set_mcp_manager`] writes a process-global slot, so every dispatcher in the
/// process resolves the same registry: a second writer re-points the first writer's
/// calls, and a tool that was dispatchable a moment ago answers
/// [`ToolError::Forbidden`] ("tool `…` is forbidden for caller `Manager` by
/// orchestration policy") or `UnknownTool`. In production there is exactly one
/// writer (`src/main.rs`, at startup), so this changes no product behaviour; it adds
/// the scoping seam the crate already ships for the sibling process global
/// ([`with_workspace_root`] / [`SCOPED_WORKSPACE_ROOT`]), so a caller — notably a
/// test in a parallel test binary, where several modules install their own registry
/// (`src/llm/client_tests.rs`) — can own the gate it dispatches against for the
/// duration of its own round trip. See `docs/fix_notice_reply_flakiness.md` §7.
///
/// Reads inside `f` see `gate`; on exit the previous state (scoped or global) is
/// restored by the task-local scope itself.
pub async fn with_mcp_gate<F, R>(gate: Arc<McpDispatchGate>, f: F) -> R
where
    F: std::future::Future<Output = R>,
{
    SCOPED_MCP_GATE.scope(gate, f).await
}

/// The gate that governs the current call site: this task's scoped gate when one is
/// installed, otherwise the process-global registry.
fn current_mcp_gate() -> Option<Arc<McpDispatchGate>> {
    SCOPED_MCP_GATE
        .try_with(Arc::clone)
        .ok()
        .or_else(get_mcp_gate)
}

/// Retrieve the active global MCP manager if available.
pub fn get_mcp_manager() -> Option<Arc<crate::mcp::McpManager>> {
    current_mcp_gate().map(|gate| Arc::clone(&gate.manager))
}

/// Retrieve the policy-filtered MCP registry — the dispatch view of the manager.
///
/// This is the **process-global** slot; a task running under [`with_mcp_gate`]
/// dispatches through its scoped gate instead (see [`current_mcp_gate`]).
pub fn get_mcp_gate() -> Option<Arc<McpDispatchGate>> {
    MCP_GATE.read().ok().and_then(|lock| lock.clone())
}

/// The MCP tools that survived the name policy, restricted to `servers`.
///
/// Tool *advertising* (the schema list handed to a model) should prefer this over
/// `McpManager::tools_for_servers`, so a name the policy refused is never even
/// offered to a model, let alone dispatched.
pub fn allowed_mcp_tools(servers: &[String]) -> Vec<crate::mcp::McpTool> {
    let Some(gate) = current_mcp_gate() else {
        return Vec::new();
    };
    gate.manager
        .tools_for_servers(servers)
        .into_iter()
        .filter(|tool| gate.allowed.contains(tool.qualified_name().as_str()))
        .collect()
}

/// Resolve the (`gate`, `manager`) pair that owns `name` for dispatch (see
/// [`McpDispatchGate::route`]).
///
/// The gate is returned with the manager on purpose: both halves of one MCP round
/// trip — name resolution *and* the bounded readiness wait — must be decided against
/// the *same* gate, not against two `current_mcp_gate()` lookups that a concurrent
/// re-registration could disagree with.
fn mcp_route(name: &str) -> Option<(Arc<McpDispatchGate>, Arc<crate::mcp::McpManager>)> {
    let gate = current_mcp_gate()?;
    let manager = Arc::clone(&gate.manager);
    gate.route(name)?;
    Some((gate, manager))
}

/// Execute an MCP tool call, or `None` when no MCP tool is routed for the invocation.
///
/// The readiness gate runs on the resolved name **before any `tools/call` goes out**:
/// a registry that is still booting must not answer as `UnknownTool`/`Forbidden`, and
/// a boot that failed must say so. Both answers are ordinary tool errors, exactly
/// like the `MCP tool error: …` of a failing round trip.
fn dispatch_mcp_sync(tool: &ToolInvocation) -> Option<Result<ToolResult, ToolError>> {
    let (gate, manager) = mcp_route(&tool.name)?;
    if let Err(reason) = gate.gate_is_ready_blocking(&tool.name) {
        return Some(Ok(ToolResult::err(reason)));
    }
    let mcp_res = block_on_safe(manager.call_tool(&tool.name, &tool.arguments));
    Some(match mcp_res {
        Ok(content) => Ok(ToolResult::ok(content)),
        Err(e) => Ok(ToolResult::err(format!("MCP tool error: {e}"))),
    })
}

/// Async counterpart of [`dispatch_mcp_sync`] — same resolution, same readiness gate
/// (the awaitable form, [`McpDispatchGate::gate_is_ready`]), same tool-error channel.
async fn dispatch_mcp_async(tool: &ToolInvocation) -> Option<Result<ToolResult, ToolError>> {
    let (gate, manager) = mcp_route(&tool.name)?;
    if let Err(reason) = gate.gate_is_ready(&tool.name).await {
        return Some(Ok(ToolResult::err(reason)));
    }
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
/// Resolution order: the built-in match runs FIRST. MCP tools are consulted only
/// for names the built-in dispatcher does not own, and only through the
/// registration policy ([`McpDispatchGate::route`]), so an MCP server can never
/// claim a built-in name and bypass the sandboxed built-in handlers. The wire name
/// is folded through the **one** tool-alias vocabulary
/// ([`crate::tool_names::TOOL_ALIAS_TABLE`]) by [`canonical_dispatch_name`], and the
/// arms below name canonical spellings only: one table, three call sites.
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
        TOOL_PAUSE_EXECUTION => handle_pause_execution(&tool.arguments),
        TOOL_PAUSE_SUBAGENT => plan::control_subagent(&tool.arguments, true),
        TOOL_RESUME_SUBAGENT => plan::control_subagent(&tool.arguments, false),
        // Every accepted sleep spelling (the whole sleep family of the shared
        // table) normalizes onto `TERMINAL_SLEEP`, so one arm covers them all.
        TOOL_SLEEP | TERMINAL_SLEEP => handle_sleep(&tool.arguments),
        TOOL_REBIRTH => Err(ToolError::BadArguments {
            tool: TOOL_REBIRTH.to_string(),
            detail: "rebirth requires a live ContextEngine; use dispatch_with_engine".to_string(),
        }),
        // Built-in match exhausted: only now is MCP consulted, and only for names
        // the built-in dispatcher does not own. The *wire* name is reported, never
        // the normalized spelling, so callers keep the exact error text they got.
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

/// Clip an oversized tool result to a head + tail window. The cut keeps the
/// **first** [`MAX_TOOL_OUTPUT_CHARS`]-ish head and the **last** 2 000 characters,
/// so a footer that `read_file` appends when it hits its byte ceiling —
/// [`crate::harness::fs::READ_FILE_TRUNCATION_MARKER`] with `truncated: true`, the
/// offsets and the re-run hint — always survives at the very end of the payload. If
/// a footer ever starts inside the dropped region, the tail window widens to begin
/// at the marker, so the clip never swallows the *reason* a result was clipped.
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
    // No MCP lookup ahead of the built-in match: built-in names are matched first
    // and MCP is consulted only in the fall-through arm, through the policy.
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
        TOOL_PAUSE_EXECUTION => handle_pause_execution(&tool.arguments),
        TOOL_PAUSE_SUBAGENT => plan::control_subagent(&tool.arguments, true),
        TOOL_RESUME_SUBAGENT => plan::control_subagent(&tool.arguments, false),
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
        // intentionally not alias-folded; its sleep arm still asks the shared alias
        // table, so the sleep vocabulary is identical on every path.
        n if is_sleep_tool_name(n) => handle_sleep_async(&tool.arguments).await,
        TOOL_REPLY_TO_ARBITRATOR => {
            handle_reply_to_arbitrator_async("Manager", &tool.arguments).await
        }
        // Built-in match exhausted: MCP is consulted last, and only through the
        // registration policy.
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
    if matches!(
        name,
        TOOL_CREATE_PLAN | TOOL_PAUSE_SUBAGENT | TOOL_RESUME_SUBAGENT
    ) {
        return Err(ToolError::Forbidden {
            tool: name.to_string(),
            caller: caller_str,
        });
    }

    if !crate::orchestrator::worker_pause::wait_for_current_worker().await {
        return Err(ToolError::Execution(anyhow::anyhow!(
            "Worker cancelled while paused"
        )));
    }

    // `mcp_route` refuses any name the built-in dispatcher owns (verbatim or via
    // alias normalization), so a built-in handler can never be shadowed by an MCP
    // server. Names the built-ins do not own are served by the MCP tool only if it
    // survived the registration policy (`set_mcp_manager` -> `McpDispatchGate`), and
    // stay reachable for specialists. The readiness gate runs on the resolved name
    // before the call, so a specialist's MCP call made while the servers are still
    // booting waits (bounded) instead of dying as `Forbidden`.
    if let Some((gate, manager)) = mcp_route(name) {
        if name != TOOL_REBIRTH
            && let Some(ref mut eng) = engine
        {
            eng.reset_consecutive_rebirths();
        }
        if let Err(reason) = gate.gate_is_ready(name).await {
            return Ok(ToolResult::err(reason));
        }
        return match manager.call_tool(name, &tool.arguments).await {
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

    // The arms below are matched on the canonical spelling produced by the one
    // shared alias table, never on re-typed alias arms.
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
            // The allowlist admitted this call; the identity rule may not.
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
        // all onto `TERMINAL_SLEEP`.
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

/// The worker identity a specialist reply is attributed to.
///
/// A notice is filed under the **effective registry key** the runner drains its inbox with
/// (`coder-t-001`, or the collision-resolved `coder-t-001#7`), so a reply has to be attributed
/// to that same key for the strict reply contract to hold: [`crate::orchestrator::notice::notice_addresses_worker`]
/// then matches exact key to exact key instead of relying on its bidirectional role-name
/// tolerance. That tolerance is what made two workers of the **same role** able to answer the
/// same dialogue id, so it must stay a compatibility path, not the only thing keeping replies
/// attached to the right worker.
///
/// [`crate::orchestrator::active_worker_key_for_reply`] resolves that key: first from the
/// task-local worker token the runner scoped around the whole turn
/// ([`crate::orchestrator::CURRENT_WORKER_TOKEN`], registered by
/// [`crate::orchestrator::register_active_worker_with_token`]) — exactly how the runner itself
/// identifies the worker when it drains notices — and, for a registration that carries no token
/// of the running turn, from the single **active** registry entry the caller's role name
/// addresses. Both answers are the key the notice was filed under, so reply identity and notice
/// addressing cannot disagree.
///
/// Falls back to the caller's role name when neither step can pin one entry (no worker token in
/// scope — a Manager turn, the UI bridge, a unit test — or two live entries sharing one role
/// name): identity dispatch could see before, and
/// [`crate::orchestrator::notice::WorkerRoutingIdentity::routes`] still judges it by whole
/// fields only, never by a substring.
fn reply_worker_identity(caller: &str) -> String {
    crate::orchestrator::active_worker_key_for_reply(caller)
        .unwrap_or_else(|| caller.trim().to_string())
}

/// Handle one `reply_to_arbitrator` call under the strict reply contract: a reply
/// must name the notice it answers, and that notice must address the replying
/// worker. [`crate::orchestrator::record_worker_reply_for_notice`] makes the record,
/// and **every** rejection ([`crate::orchestrator::NoticeReplyRejection`]) is
/// propagated to the calling specialist as a `ToolError` naming the rejection kind, so
/// it can re-send with the right id. There is no fallback lookup and no synthesized
/// notice: a rejected reply records nothing, resolves nothing, emits no
/// `Event::SteerResponse` and never reaches the arbitrator — a notice id is never guessed.
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
                // Round two of one dialogue must reach **the worker that was addressed**, not
                // the bare role name. A follow-up filed under `coder` is drained by every
                // coder worker in the process, so with two of them the question goes to a
                // worker that never saw the first round — and that worker can then answer the
                // reused dialogue id, because the notice it drained addresses it. The store
                // already handed back the notice with the key it was filed under, so that key
                // is the addressing token for the follow-up too.
                let follow_up_target = if notice.target_worker.trim().is_empty() {
                    caller
                } else {
                    notice.target_worker.as_str()
                };
                let follow_up_notice = crate::orchestrator::post_notice_to_worker(
                    follow_up_target,
                    &follow_up,
                    Some(notice_id),
                );
                if let Some(user_status) = eval.user_status {
                    crate::orchestrator::emit_event(crate::ui::Event::SteerResponse(format!(
                        "\n[Arbitrator]: {user_status}\n\n"
                    )));
                } else {
                    crate::orchestrator::emit_status(format!(
                        "[Arbitrator]: Follow-up question sent to {follow_up_target} ({})",
                        follow_up_notice.notice_id
                    ));
                }
                let follow_up_record = format!(
                    "Forwarded follow-up {} to {follow_up_target}: \"{follow_up}\" (awaiting specialist reply)\n[Specialist {caller} prior reply]: {message}",
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
        TOOL_PAUSE_EXECUTION => handle_pause_execution(&tool.arguments),
        TOOL_PAUSE_SUBAGENT => plan::control_subagent(&tool.arguments, true),
        TOOL_RESUME_SUBAGENT => plan::control_subagent(&tool.arguments, false),
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
        other => match dispatch_mcp_sync(tool) {
            Some(mcp_res) => mcp_res,
            None => Err(ToolError::Forbidden {
                tool: other.to_string(),
                caller: "Manager".to_string(),
            }),
        },
    }
}

/// Fold a tool name onto the spelling the dispatcher compares against.
///
/// The single source is the table in [`crate::tool_names::TOOL_ALIAS_TABLE`]: every
/// site that needs "which tool does this spelling mean" — the harness dispatcher, the
/// specialist allowlist, the MCP name policy and the steer grammar — asks that table.
/// Unknown names (MCP composed names, typos) are returned verbatim.
pub(crate) fn normalize_tool_name(name: &str) -> String {
    crate::tool_names::normalize_tool_alias(name)
        .unwrap_or(name)
        .to_string()
}

/// Verdict identity rule at the **dispatch** layer. The allowlist decides *which
/// tools a role may touch*, not *who may certify a deliverable*: the generalist's
/// registry namespace is literally `"*"` ([`crate::orchestrator::registry`]), so the
/// allowlist check in [`dispatch_specialist`] admits `leave_verdict` for a Generalist,
/// and a prompt/blueprint allowlist admits it for any role the model was told about.
/// This rule mirrors the agent loop's verdict check by calling the existing `pub`
/// predicate [`crate::agents::runner::may_record_verdict`], so the identity rule lives
/// in one place. A refusal is a `ToolError::Forbidden` plus a `tracing::warn!`, raised
/// *before* verdict arguments are parsed or any verdict state is touched — no partial
/// write, no silent success.
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
    if matches!(
        name,
        TOOL_CREATE_PLAN | TOOL_PAUSE_SUBAGENT | TOOL_RESUME_SUBAGENT
    ) {
        return Err(ToolError::Forbidden {
            tool: name.to_string(),
            caller: caller_str,
        });
    }

    if !block_on_safe(crate::orchestrator::worker_pause::wait_for_current_worker()) {
        return Err(ToolError::Execution(anyhow::anyhow!(
            "Worker cancelled while paused"
        )));
    }

    // Resolution order is the one stated on `dispatch`: built-ins first, MCP last.
    // The bounded readiness gate runs on the resolved name before the call, like every
    // other MCP dispatch site in this file.
    if let Some((gate, manager)) = mcp_route(name) {
        if name != TOOL_REBIRTH
            && let Some(ref mut eng) = engine
        {
            eng.reset_consecutive_rebirths();
        }
        if let Err(reason) = gate.gate_is_ready_blocking(name) {
            return Ok(ToolResult::err(reason));
        }
        let mcp_res = block_on_safe(manager.call_tool(name, &tool.arguments));
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

    // Canonical spelling from the one shared alias table.
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
        _ if crate::agents::validation::is_leave_verdict_tool(name) => {
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
        // One arm covers every accepted sleep spelling.
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
        // The *wire* name is reported, never the normalized spelling, so error text
        // for names that never resolved stays byte-identical.
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
        // so the collapse is to THREE messages: the `[2]` instruction slot is
        // omitted rather than filled with a copy of the goal.
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

        let result3 = handle_rebirth(&mut engine, &args).unwrap();
        assert!(!result3.is_error);
    }

    /// Take the process-global worker/cancellation serialisation lock — the same
    /// `workers::TEST_WORKERS_MUTEX` convention this crate already uses
    /// (`orchestrator::tests.rs:22`, `orchestrator::pool.rs:1029`,
    /// `ui::bridge::tests.rs:1316`).
    ///
    /// A `delegate_task` round trip resolves its cancellation state out of two process
    /// globals: `handle_delegate_task` short-circuits on
    /// `orchestrator::is_globally_cancelled()` (`orchestrator/delegate.rs:145`), and the
    /// Manager it builds draws `global_cancellation_token().child_token()`
    /// (`orchestrator/mod.rs:147`), so a token that is already cancelled hands the
    /// delegation an immediately-cancelled child. Either way the deliverable comes back
    /// as `MissionMarker::Failed` — "Task aborted by user instruction.\n\nFAILED
    /// (aborted)" — and this test's `!res.is_error` is charged **another test's**
    /// abort. Instrumenting `cancel_all()` (logging the calling thread over a full lib
    /// run) found exactly two runtime writers of that token: the two `ui::bridge`
    /// arbitration drivers, and both run inside `with_arbitration_test_lock`
    /// (`ui::bridge::tests.rs:1315-1327`), which holds this lock and restores the token
    /// on both sides. Holding it across the round trip therefore excludes them;
    /// `test_harness_cancellation_writers_share_the_worker_serialisation_lock` pins that
    /// they keep doing so. See `docs/fix_notice_reply_flakiness.md` §7.
    fn worker_registry_serial() -> std::sync::MutexGuard<'static, ()> {
        crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn test_harness_delegate_task_manager_routes_and_returns_deliverable() {
        let _serial = worker_registry_serial();
        let invocation = ToolInvocation {
            name: TOOL_DELEGATE_TASK.to_string(),
            arguments: serde_json::json!({
                "agent_name": "coder",
                "prompt": "write hello world",
                "task_id": "t-del-999"
            }),
        };
        let res = dispatch_for(&invocation, ToolCaller::Manager)
            .unwrap_or_else(|e| panic!("delegate_task must not hard-error: {e}"));
        assert!(
            !res.is_error,
            "delegate_task must return the deliverable (an aborted deliverable here \
             means a foreign process-global cancel landed inside this round trip): {}",
            res.content
        );
        assert!(
            res.content.contains("MISSION COMPLETE"),
            "the deliverable must carry the completion marker: {}",
            res.content
        );
    }

    /// Pins the invariant `test_harness_delegate_task_manager_routes_and_returns_deliverable`
    /// depends on (§7): the serialisation lock it holds is the one every
    /// cancellation-writing test in this binary already holds. If a future cancellation
    /// driver aborts outside that lock, this guard goes red instead of the delegate
    /// test flaking under load.
    #[test]
    fn test_harness_cancellation_writers_share_the_worker_serialisation_lock() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/ui/bridge/tests.rs"
        ))
        .expect("the arbitration test drivers are part of this crate");
        let start = source
            .find("fn with_arbitration_test_lock")
            .expect("the arbitration test driver must exist");
        let body = &source[start..];
        let body = &body[..body.find("\n}").unwrap_or(body.len())];
        assert!(
            body.contains("workers::TEST_WORKERS_MUTEX"),
            "the arbitration drivers cancel the process globally (`cancel_all`) and must \
             stay serialised behind `workers::TEST_WORKERS_MUTEX`, which the delegate_task \
             round trip also takes: {body}"
        );
        assert!(
            body.contains("reset_cancellation()"),
            "the arbitration drivers must restore the global cancellation token on both \
             sides of their round trip: {body}"
        );
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

    #[test]
    fn test_harness_write_plan_internal_rejects_missing_execution_plan_header_instantly() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = crate::manager::phase::Plan::at(tmp.path().join(".marmel"));
        let res = block_on_safe(async {
            write_plan_internal(
                "# Plan: Wrong Header\n## Phase 1\n- [ ] [t-001] Step one (coder)\n",
                Some(plan.clone()),
            )
            .await
        })
        .unwrap();

        assert!(res.is_error);
        assert!(res.content.contains("Strategic Plan Auditor"));
        assert!(res.content.contains("# Execution Plan"));
        assert!(!plan.plan_path().exists());
    }
}
