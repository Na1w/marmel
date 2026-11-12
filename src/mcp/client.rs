//! JSON-RPC 2.0 MCP Client implementation for stdio and HTTP/SSE.
//!
//! The stdio connection below is a raw transport only: the JSON-RPC envelopes,
//! ids, method names, param shapes and result/error decoding come from
//! [`super::protocol`], exactly as the HTTP/SSE transport in [`super::http`]
//! consumes them. What is stdio-specific here is the child
//! process, the newline framing and this transport's debug instrumentation.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use super::http::{DEFAULT_MCP_REQUEST_TIMEOUT, HttpSseConnection};
use super::protocol::{
    self, JsonRpcNotification, JsonRpcRequest, JsonRpcTransport, RequestIds, decode_reply,
};

/// Default idle duration before closing an inactive MCP connection (5 minutes).
pub const DEFAULT_MCP_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Server configuration entry for an MCP server in marmel.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// Command to spawn the MCP server executable (for stdio transport).
    pub command: Option<String>,
    /// Command arguments.
    pub args: Vec<String>,
    /// Environment variables for the spawned process.
    pub env: HashMap<String, String>,
    /// Remote URL endpoint (for HTTP/SSE transport).
    pub url: Option<String>,
    /// Optional prompt or usage instructions injected into the system prompt of
    /// every agent allowed to use this MCP server.
    #[serde(alias = "instructions", alias = "system_prompt")]
    pub prompt: Option<String>,
    /// Idle timeout in seconds before closing the inactive connection.
    /// Defaults to 300 seconds (5 minutes). Set to 0 to disable idle closing.
    pub idle_timeout_secs: Option<u64>,
    /// Request timeout in seconds for tool calls and protocol requests.
    /// Defaults to 180 seconds (3 minutes).
    #[serde(alias = "timeout_secs", alias = "timeout")]
    pub request_timeout_secs: Option<u64>,
}

impl McpServerConfig {
    /// Effective request timeout duration. Defaults to [`DEFAULT_MCP_REQUEST_TIMEOUT`] (180 seconds).
    pub fn request_timeout(&self) -> std::time::Duration {
        match self.request_timeout_secs {
            Some(secs) => std::time::Duration::from_secs(secs),
            None => DEFAULT_MCP_REQUEST_TIMEOUT,
        }
    }
}

/// Discovered MCP Tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    /// Original tool name reported by the MCP server.
    pub name: String,
    /// Human-readable description.
    pub description: Option<String>,
    /// JSON Schema for parameters.
    pub input_schema: Value,
    /// Which server provides this tool.
    pub server_name: String,
}

impl McpTool {
    /// Fully-qualified name combining the server name and the raw tool name,
    /// guaranteeing uniqueness across servers that expose identically-named tools.
    pub fn qualified_name(&self) -> String {
        format!("{}__{}", self.server_name, self.name)
    }
}

/// Active connection to an MCP server over stdio.
pub struct StdioMcpConnection {
    server_name: String,
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_reader: BufReader<tokio::process::ChildStdout>,
    /// The protocol layer's monotonic JSON-RPC id allocator
    /// ([`super::protocol::RequestIds`]); the transport only carries it.
    request_ids: RequestIds,
    /// Per-response deadline shared with the HTTP transport
    /// ([`DEFAULT_MCP_REQUEST_TIMEOUT`]).
    request_timeout: std::time::Duration,
    /// The detached stderr pump this connection spawned at
    /// [`spawn_with`](Self::spawn_with), kept as an owned handle so
    /// [`shutdown`](Self::shutdown) can end it instead of leaving it dangling on
    /// the runtime.
    ///
    /// The pump is a *task*, not an OS child: nothing drops it. Its read ends
    /// when the child's stderr pipe reaches EOF — which the child itself provides
    /// once it is killed — but that is a dependency on the child being killed *and*
    /// on the runtime still being alive to run the task, so a manager that is only
    /// `shutdown_timeout`-ed leaves the pump parked there forever. Owning the
    /// handle here makes its lifetime bounded by this connection's.
    stderr_pump: Option<tokio::task::JoinHandle<()>>,
    /// Optional server usage instructions returned during `initialize`.
    instructions: Option<String>,
}

impl StdioMcpConnection {
    pub async fn spawn(server_name: &str, cfg: &McpServerConfig) -> Result<Self> {
        Self::spawn_with(server_name, cfg, cfg.request_timeout()).await
    }

    /// Spawn with an explicit response deadline. Production callers go through
    /// [`Self::spawn`] (uses `cfg.request_timeout()`); the seam keeps the
    /// deadline behaviour testable without waiting 180 s.
    pub(crate) async fn spawn_with(
        server_name: &str,
        cfg: &McpServerConfig,
        request_timeout: std::time::Duration,
    ) -> Result<Self> {
        let cmd_str = cfg
            .command
            .as_ref()
            .ok_or_else(|| anyhow!("missing `command` for stdio MCP server '{server_name}'"))?;

        let mut cmd = Command::new(cmd_str);
        cmd.args(&cfg.args);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn MCP server '{server_name}' ({cmd_str})"))?;

        let stderr = child.stderr.take();
        // The pump is a task, not an OS child: nothing drops it, so the handle is
        // kept on this connection (see [`Self::stderr_pump`]) where
        // [`shutdown`](Self::shutdown) can end it. Left to its own devices the
        // read only ends at the child's stderr EOF — which needs the child to die
        // *and* the runtime to still be alive to run the task.
        let stderr_pump = stderr.map(|stderr| {
            let s_name = server_name.to_string();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut reader = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::debug!(target: "mcp", "[{s_name}] {line}");
                }
            })
        });

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open stdin for '{server_name}'"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to open stdout for '{server_name}'"))?;
        let stdout_reader = BufReader::new(stdout);

        let mut conn = Self {
            server_name: server_name.to_string(),
            child,
            stdin,
            stdout_reader,
            request_ids: RequestIds::new(),
            request_timeout,
            stderr_pump,
            instructions: None,
        };

        match protocol::initialize(&mut conn).await {
            Ok(instructions) => conn.instructions = instructions,
            Err(err) => {
                // The handshake never completed, and this connection is on its way out
                // as an `Err`: tear it down here instead of leaning on drop glue. A
                // dropped `Child` only *kills* (`kill_on_drop`) and hands the reap to
                // the runtime's orphan queue — which is a second thing that can go
                // wrong on a dying runtime, and is exactly the half-booted leak this
                // whole path is trying to avoid.
                let _ = conn.shutdown().await;
                return Err(err);
            }
        }
        Ok(conn)
    }

    /// Server instructions returned during the `initialize` handshake, if any.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn request_timeout(&self) -> std::time::Duration {
        self.request_timeout
    }
}

/// The raw send/receive half of the [`super::protocol`] seam: newline-delimited
/// JSON-RPC over the child's stdin/stdout. Envelope construction, id allocation,
/// method names, param shapes and the error-envelope decoding are all the
/// protocol layer's; this transport supplies the wire and its own instrumentation.
impl JsonRpcTransport for StdioMcpConnection {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn request_ids(&self) -> &RequestIds {
        &self.request_ids
    }

    /// Write one request envelope and return the `result` of the reply that
    /// carries its id.
    async fn send_request(&mut self, request: &JsonRpcRequest) -> Result<Value> {
        let method = request.method.as_str();
        crate::debug_log::log_mcp_request(&self.server_name, method, request.params_ref());
        let start_time = std::time::Instant::now();

        let mut body = request.body()?;
        body.push('\n');

        self.stdin
            .write_all(body.as_bytes())
            .await
            .with_context(|| format!("writing request to MCP server '{}'", self.server_name))?;
        self.stdin.flush().await?;

        let mut line = String::new();
        loop {
            line.clear();
            // Shared pump skeleton from `crate::net::sse`: the deadline is
            // recomputed per read, which keeps this transport's flat
            // per-response timeout.
            let n = {
                let read_fut = self.stdout_reader.read_line(&mut line);
                tokio::pin!(read_fut);
                let deadline = std::time::Instant::now() + self.request_timeout;
                match crate::net::pump_future(&mut read_fut, Some(deadline), &mut |_| true).await {
                    crate::net::PumpNext::Item(Some(res)) => res?,
                    crate::net::PumpNext::Item(None) => {
                        unreachable!("read_line always resolves to a value")
                    }
                    crate::net::PumpNext::IdleTimeout => {
                        return Err(anyhow!(
                            "timeout waiting for MCP server '{}' response ({}s)",
                            self.server_name,
                            self.request_timeout.as_secs()
                        ));
                    }
                    crate::net::PumpNext::Aborted => {
                        return Err(anyhow!("MCP request to '{}' aborted", self.server_name));
                    }
                }
            };
            if n == 0 {
                let err_msg = format!("MCP server '{}' closed stdout stream", self.server_name);
                crate::debug_log::log_mcp_response(
                    &self.server_name,
                    method,
                    start_time.elapsed().as_millis(),
                    &err_msg,
                    true,
                );
                return Err(anyhow!(err_msg));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Which stdout line answers *this* request (and which is noise, a
            // log line, or somebody else's event) is decided by the protocol
            // layer, not by a transport-local rule.
            if let Some(resp) = decode_reply(trimmed, request.id) {
                let res = resp.into_result(&self.server_name);
                let elapsed_ms = start_time.elapsed().as_millis();
                let res_str = match &res {
                    Ok(v) => serde_json::to_string(v).unwrap_or_else(|_| v.to_string()),
                    Err(e) => e.to_string(),
                };
                crate::debug_log::log_mcp_response(
                    &self.server_name,
                    method,
                    elapsed_ms,
                    &res_str,
                    res.is_err(),
                );
                return res;
            }
        }
    }

    /// Write one notification envelope; no reply is expected.
    async fn send_notification(&mut self, notification: &JsonRpcNotification) -> Result<()> {
        let mut body = notification.body()?;
        body.push('\n');

        self.stdin.write_all(body.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

impl StdioMcpConnection {
    /// Discover the tools exposed by this server. The envelope, the request id
    /// and the decoding come from [`super::protocol`].
    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        protocol::list_tools(self).await
    }

    /// Invoke a tool on this server.
    pub async fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<String> {
        protocol::call_tool(self, name, arguments).await
    }

    /// Kill-and-reap budget of this transport's teardown. `Child::kill()` is
    /// SIGKILL *followed by* `wait()` — and that `wait()` is what actually reaps
    /// the child — so the bound applies to the reap, not to the signal: the kill
    /// itself is sent on the first poll of the future, whatever the budget.
    const CHILD_KILL_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);

    /// Bounded wait for the aborted stderr pump inside [`shutdown`](Self::shutdown).
    const PUMP_JOIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(30);

    /// The transport's only teardown step: kill the child and reap it, then end
    /// the stderr pump. It deliberately touches **no** JSON-RPC read — waiting on
    /// a parked transport read here is how a half-connected server would hold the
    /// whole process open.
    ///
    /// Both halves are bounded ([`Self::CHILD_KILL_BUDGET`] /
    /// [`Self::PUMP_JOIN_BUDGET`]), and both are idempotent: after a reap the
    /// child is fused to its cached exit status, so a second `kill()` answers
    /// `Ok(())` without touching the process table, and the pump handle is
    /// `take`n so a second call sees nothing to abort. Never errors: teardown
    /// must always be allowed to proceed.
    pub async fn shutdown(&mut self) -> Result<()> {
        // First the child, so the pump gets its natural EOF if the kill lands.
        let kill = tokio::time::timeout(Self::CHILD_KILL_BUDGET, self.child.kill()).await;
        match kill {
            Err(_elapsed) => tracing::debug!(
                target: "mcp",
                "MCP server '{}' child was killed but not reaped within {} ms",
                self.server_name,
                Self::CHILD_KILL_BUDGET.as_millis()
            ),
            Ok(Err(e)) => tracing::debug!(
                target: "mcp",
                "MCP server '{}' child teardown failed: {e}",
                self.server_name
            ),
            Ok(Ok(())) => {}
        }

        // Then the pump: aborting is what bounds its lifetime, and awaiting the
        // handle (bounded) proves the task is really gone rather than detached.
        if let Some(pump) = self.stderr_pump.take() {
            pump.abort();
            if tokio::time::timeout(Self::PUMP_JOIN_BUDGET, pump)
                .await
                .is_err()
            {
                tracing::debug!(
                    target: "mcp",
                    "MCP server '{}' stderr pump did not exit within its abort budget",
                    self.server_name
                );
            }
        }
        Ok(())
    }

    /// Check if the child process is still alive.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// The one piece of teardown that a drop glue may lawfully do: nothing awaiting,
/// nothing blocking.
///
/// There is deliberately **no** `Drop` for [`McpManager`](super::McpManager) or
/// [`McpClient`]: the manager is parked in the process-global MCP gate
/// (`harness::set_mcp_manager`) for the life of the process, so those objects are
/// never dropped at all, and a `Drop` that had to `await` a kill and a reap could
/// not run on a runtime that is already on its way out. That is why
/// [`McpManager::shutdown`](super::McpManager::shutdown) is the single teardown
/// entry point.
///
/// What a drop *can* settle is the pump task: the child is already covered by
/// `kill_on_drop(true)`, but a pump whose handle is merely dropped stays detached
/// (the case where the boot future is aborted mid-handshake, for instance, where
/// no `shutdown` ever runs). Aborting it here is a synchronous flag set, and the
/// task then unwinds at its next cancellation point — never in this thread.
impl Drop for StdioMcpConnection {
    fn drop(&mut self) {
        if let Some(pump) = self.stderr_pump.take() {
            pump.abort();
        }
    }
}

pub enum McpClient {
    Stdio(Box<Mutex<StdioMcpConnection>>),
    HttpSse(Mutex<HttpSseConnection>),
    /// Test-only mock for asserting the routing chain without a live server.
    #[cfg(test)]
    Mock(Mutex<MockMcpConnection>),
}

/// Test-only mock connection that records the tool names dispatched to it.
#[cfg(test)]
pub struct MockMcpConnection {
    /// Names passed to `call_tool`, in call order.
    pub called_names: Arc<Mutex<Vec<String>>>,
}

impl McpClient {
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        match self {
            McpClient::Stdio(lock) => {
                let mut conn = lock.lock().await;
                conn.list_tools().await
            }
            McpClient::HttpSse(lock) => {
                let mut conn = lock.lock().await;
                conn.list_tools().await
            }
            #[cfg(test)]
            McpClient::Mock(lock) => {
                let _conn = lock.lock().await;
                Ok(Vec::new())
            }
        }
    }

    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<String> {
        match self {
            McpClient::Stdio(lock) => {
                let mut conn = lock.lock().await;
                conn.call_tool(name, arguments).await
            }
            McpClient::HttpSse(lock) => {
                let mut conn = lock.lock().await;
                conn.call_tool(name, arguments).await
            }
            #[cfg(test)]
            McpClient::Mock(lock) => {
                let conn = lock.lock().await;
                conn.called_names.lock().await.push(name.to_string());
                Ok("mock-result".to_string())
            }
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        match self {
            McpClient::Stdio(lock) => {
                let mut conn = lock.lock().await;
                conn.shutdown().await
            }
            McpClient::HttpSse(lock) => {
                let mut conn = lock.lock().await;
                conn.shutdown().await
            }
            #[cfg(test)]
            McpClient::Mock(lock) => {
                let _conn = lock.lock().await;
                Ok(())
            }
        }
    }

    /// Check if the underlying connection has died (for stdio: child process exited).
    pub async fn is_dead(&self) -> bool {
        match self {
            McpClient::Stdio(lock) => {
                let mut conn = lock.lock().await;
                !conn.is_alive()
            }
            McpClient::HttpSse(_) => false,
            #[cfg(test)]
            McpClient::Mock(_) => false,
        }
    }
}

/// Helper function to establish an active connection to an MCP server.
pub(crate) async fn connect_client(
    name: &str,
    cfg: &McpServerConfig,
) -> Result<(Arc<McpClient>, Option<String>), String> {
    if cfg.command.is_some() {
        match StdioMcpConnection::spawn(name, cfg).await {
            Ok(conn) => {
                let inst = conn.instructions().map(str::to_string);
                Ok((Arc::new(McpClient::Stdio(Box::new(Mutex::new(conn)))), inst))
            }
            Err(e) => {
                tracing::warn!("Failed to start MCP server '{name}': {e:#}");
                Err(format!("failed to start MCP server '{name}': {e:#}"))
            }
        }
    } else if cfg.url.is_some() {
        match HttpSseConnection::connect(name, cfg).await {
            Ok(conn) => {
                let inst = conn.instructions().map(str::to_string);
                Ok((Arc::new(McpClient::HttpSse(Mutex::new(conn))), inst))
            }
            Err(e) => {
                tracing::warn!("Failed to connect to MCP server '{name}': {e:#}");
                Err(format!("failed to connect to MCP server '{name}': {e:#}"))
            }
        }
    } else {
        Err(format!(
            "MCP server '{name}' has neither command nor url configured"
        ))
    }
}

/// Connection slot for a configured MCP server, supporting on-demand connection
/// and automatic idle timeout teardown.
pub struct ServerSlot {
    pub name: String,
    pub config: McpServerConfig,
    state: Mutex<ServerSlotState>,
}

struct ServerSlotState {
    client: Option<Arc<McpClient>>,
    last_used: std::time::Instant,
}

impl ServerSlot {
    pub fn new(name: String, config: McpServerConfig, client: Option<Arc<McpClient>>) -> Self {
        Self {
            name,
            config,
            state: Mutex::new(ServerSlotState {
                client,
                last_used: std::time::Instant::now(),
            }),
        }
    }

    /// Effective idle timeout duration. If `idle_timeout_secs` is `Some(0)`, idle closing is disabled.
    /// If `None`, defaults to [`DEFAULT_MCP_IDLE_TIMEOUT`] (300 seconds).
    pub fn effective_idle_timeout(&self) -> Option<std::time::Duration> {
        match self.config.idle_timeout_secs {
            Some(0) => None,
            Some(secs) => Some(std::time::Duration::from_secs(secs)),
            None => Some(DEFAULT_MCP_IDLE_TIMEOUT),
        }
    }

    /// Effective request timeout duration.
    /// Defaults to [`DEFAULT_MCP_REQUEST_TIMEOUT`] (180 seconds).
    pub fn effective_request_timeout(&self) -> std::time::Duration {
        self.config.request_timeout()
    }

    /// Returns whether this server currently has an active connection.
    pub async fn is_connected(&self) -> bool {
        let state = self.state.lock().await;
        state.client.is_some()
    }

    /// Explicitly disconnect and shut down the active client, if any.
    pub async fn disconnect(&self) {
        let old_client = {
            let mut state = self.state.lock().await;
            state.client.take()
        };
        if let Some(client) = old_client {
            let _ = client.shutdown().await;
        }
    }

    /// Touch the slot's `last_used` timestamp.
    pub async fn touch(&self) {
        let mut state = self.state.lock().await;
        state.last_used = std::time::Instant::now();
    }

    /// Close the connection if it has been idle for longer than the configured timeout
    /// or if the child process has exited. Returns true if a connection was closed.
    pub async fn reap_if_idle(&self) -> bool {
        let timeout = match self.effective_idle_timeout() {
            Some(t) => t,
            None => return false,
        };

        let mut state = self.state.lock().await;
        if let Some(ref client) = state.client {
            let is_dead = client.is_dead().await;
            let is_idle = state.last_used.elapsed() >= timeout;

            if is_dead || is_idle {
                let reason = if is_dead {
                    "child process exited"
                } else {
                    "idle timeout"
                };
                tracing::info!(
                    target: "mcp",
                    "MCP server '{}' disconnecting ({reason}, last used {:?} ago)",
                    self.name,
                    state.last_used.elapsed()
                );
                let old = state.client.take();
                drop(state);
                if let Some(client) = old {
                    let _ = client.shutdown().await;
                }
                return true;
            }
        }
        false
    }

    /// Retrieve the live client or connect on demand.
    pub async fn get_or_connect(&self) -> Result<Arc<McpClient>> {
        let mut state = self.state.lock().await;

        if let Some(ref client) = state.client {
            let is_dead = client.is_dead().await;
            let is_expired = match self.effective_idle_timeout() {
                Some(timeout) => state.last_used.elapsed() >= timeout,
                None => false,
            };

            if is_dead || is_expired {
                let reason = if is_dead {
                    "child process exited"
                } else {
                    "idle timeout"
                };
                tracing::info!(
                    target: "mcp",
                    "MCP server '{}' connection expired ({reason}, {:?} since last use), reconnecting",
                    self.name,
                    state.last_used.elapsed()
                );
                let old = state.client.take();
                if let Some(client) = old {
                    let _ = client.shutdown().await;
                }
            }
        }

        if state.client.is_none() {
            tracing::info!(
                target: "mcp",
                "Connecting to MCP server '{}' on-demand",
                self.name
            );
            let (client, _) = connect_client(&self.name, &self.config)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            state.client = Some(client);
        }

        state.last_used = std::time::Instant::now();
        Ok(Arc::clone(state.client.as_ref().expect("client was set")))
    }
}

/// The live half of the MCP registry: one server slot per booted server, plus every tool
/// discovered from it (keyed by [`McpTool::qualified_name`]).
///
/// It is wrapped in a `std::sync::RwLock` (see the `inner` field of [`McpManager`]): readers
/// copy what they need under the guard and release it before they await, so a guard is never
/// held across an await.
#[derive(Default)]
pub struct McpRegistry {
    /// Server connection slots, keyed by configured server name.
    server_slots: HashMap<String, Arc<ServerSlot>>,
    /// Discovered tools, keyed by `tool.qualified_name()` (unchanged).
    tools: BTreeMap<String, McpTool>,
    /// Server usage instructions reported in the `initialize` handshake, keyed by server name.
    server_instructions: HashMap<String, String>,
}

/// How far the boot of an [`McpManager`] has come: the readiness signal behind
/// [`McpManager::status`] and [`McpManager::await_ready`].
///
/// `Booting` means the boot task exists and is still working — and, unlike a blocking
/// boot, means the registry may already hold clients and tools that landed incrementally.
/// The `Failed` payload is one reason string assembled from the per-server failures;
/// `boot_one` still emits exactly the two `tracing::warn!` texts it always did
/// (`Failed to start MCP server '<name>' …` for a stdio entry, `Failed to connect to MCP
/// server '<name>' …` for an HTTP/SSE one) and the payload is the same text assembled from
/// those per-server reasons.
///
/// One payload is **not** a per-server reason: [`Self::BOOT_ABORT_REASON`], published by
/// [`McpManager::shutdown_boot_task`] when the boot task is cancelled while still working.
/// That one says "the process is going away", not "the servers are broken", and callers that
/// log the verdict must tell the two apart (`src/main.rs` does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpBootStatus {
    /// The boot task exists and has not reached its end.
    Booting,
    /// The boot finished; the registry (possibly empty) is final.
    Ready,
    /// The boot could not bring up a single configured server.
    Failed(String),
}

impl McpBootStatus {
    /// The exact `Failed` payload that [`McpManager::shutdown_boot_task`] publishes for a
    /// boot that was cancelled mid-flight (i.e. because the process is exiting). It is a
    /// sentinel, not a diagnosis: the servers it never reached may all be fine.
    pub const BOOT_ABORT_REASON: &'static str = "MCP boot task aborted";

    /// True when a `Failed` payload is the abort sentinel of [`Self::BOOT_ABORT_REASON`]
    /// rather than the real per-server failure list. Lets the exit path report "still
    /// starting at exit" instead of claiming a boot failure that never happened.
    pub fn is_boot_abort_reason(reason: &str) -> bool {
        reason == Self::BOOT_ABORT_REASON
    }

    /// True once the boot verdict is final — readiness must never wait past one of these.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Ready | Self::Failed(_))
    }
}

/// Global registry and lifecycle manager for all active MCP clients.
///
/// Boot is lazy: [`spawn_boot`](Self::spawn_boot) returns an `Arc<Self>` that is still
/// filling, and only the consumers that need a settled registry wait
/// ([`await_ready`](Self::await_ready)). The eager [`boot`](Self::boot) entry point stays
/// source-compatible for existing callers.
///
/// `inner` is a `std::sync::RwLock` — not a `tokio::sync::RwLock` — precisely because every
/// advertiser is a synchronous free function today (`harness::allowed_mcp_tools`,
/// `prompts::format_mcp_prompt_block`, `manager::context`, `execution.rs`), so they cannot
/// `.await` a lock. The guard is therefore always dropped before any await.
pub struct McpManager {
    inner: std::sync::RwLock<McpRegistry>,
    /// Boot-status publisher of the `watch` channel every reader of readiness subscribes to.
    boot_tx: tokio::sync::watch::Sender<McpBootStatus>,
    /// The same channel's cheap clone: kept on the struct so the public readiness API can
    /// stay `&self` (`await_ready` clones it per call site).
    boot_rx: tokio::sync::watch::Receiver<McpBootStatus>,
    /// The spawned boot task, so it can be cancelled synchronously
    /// ([`shutdown_boot_task`](Self::shutdown_boot_task)) **and** then owned and
    /// awaited (with a bounded wait) by [`shutdown`](Self::shutdown), which is what
    /// makes the abort observable: an aborted task's future — with the
    /// half-booted `StdioMcpConnection` and its `kill_on_drop` child inside — is
    /// dropped by the runtime only when the task is next polled, and awaiting its
    /// `JoinHandle` is what waits for that to have happened.
    /// `None` for an eager [`boot`](Self::boot) manager, which spawns nothing.
    boot_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Background reaper task that monitors idle MCP servers and closes connections.
    reaper_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    /// An empty, **final** registry: nothing is booting, so readiness is already `Ready`.
    pub fn new() -> Self {
        Self::with_status(McpBootStatus::Ready)
    }

    fn with_status(status: McpBootStatus) -> Self {
        let (boot_tx, boot_rx) = tokio::sync::watch::channel(status);
        Self {
            inner: std::sync::RwLock::new(McpRegistry::default()),
            boot_tx,
            boot_rx,
            boot_task: std::sync::Mutex::new(None),
            reaper_task: std::sync::Mutex::new(None),
        }
    }

    /// Start the boot of every configured server on `handle` and hand back the
    /// (still-filling) manager **without** awaiting anything: `main.rs` can register the
    /// gate and continue straight into the UI while boot proceeds in the background.
    ///
    /// `handle` must be the runtime that later dispatches MCP calls — boot and dispatch
    /// share one runtime on purpose, because a client whose transport task lives on a
    /// different runtime than its caller is a client nothing can talk to any more
    /// (template: `GLOBAL_PTY_MANAGER` + `handle.spawn`, `harness::pty.rs:1151-1194, 1433`).
    pub fn spawn_boot(
        handle: &tokio::runtime::Handle,
        servers: &HashMap<String, McpServerConfig>,
    ) -> Arc<Self> {
        let manager = Arc::new(Self::with_status(McpBootStatus::Booting));
        let owned = servers.clone();
        let task = handle.spawn(Self::boot_all(Arc::clone(&manager), owned));
        {
            let mut slot = manager
                .boot_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = Some(task);
        }
        manager.start_reaper(handle);
        manager
    }

    /// The spawned boot work: one [`boot_one`](Self::boot_one) future per configured server,
    /// all of them polled to completion **concurrently** by this task, and then the final
    /// status published. Concurrency is the point: with a sequential loop, the slowest (or
    /// silent) server — the one whose handshake sits inside the transport's 30 s
    /// `MCP_REQUEST_TIMEOUT` — pushed every other server's connect *and* its diagnosis behind
    /// it, so an already-exec-failed server only warned at t=30 s.
    ///
    /// Nothing about the per-server semantics changes: each future runs the unchanged
    /// `boot_one`, which registers its own client and tools the moment its own awaits are
    /// done, so per-server registrations keep landing in `inner` incrementally while the
    /// overall status is still `Booting`. The registry write lock is taken only after a
    /// server's awaits complete and never held across one, so the guard rule
    /// (no guard across an await) is untouched; and because these are inline futures and not
    /// spawned tasks, cancelling this task drops every half-booted connection right where it
    /// stands (see [`shutdown_boot_task`](Self::shutdown_boot_task)).
    ///
    /// `Failed` is published when *not one* configured server came up (every entry was a
    /// connect failure). A partial boot is `Ready`, because that is what today's eager
    /// `boot` reports too: per-server failures only ever warned, and the manager was still
    /// handed to the caller.
    async fn boot_all(manager: Arc<Self>, servers: HashMap<String, McpServerConfig>) {
        // `join_all` (already a dependency through `futures`) drives every server's boot
        // inside this one task: each parked handshake yields, so a slow server delays nobody.
        let results = futures::future::join_all(servers.into_iter().map(|(name, cfg)| {
            let manager = Arc::clone(&manager);
            async move { manager.boot_one(&name, &cfg).await }
        }))
        .await;

        let mut failures: Vec<String> = Vec::new();
        let mut connected = 0usize;
        for result in results {
            match result {
                Ok(true) => connected += 1,
                Ok(false) => {}
                Err(reason) => failures.push(reason),
            }
        }
        let status = if connected == 0 && !failures.is_empty() {
            McpBootStatus::Failed(failures.join("; "))
        } else {
            McpBootStatus::Ready
        };
        let _ = manager.boot_tx.send(status);
    }

    /// Boot one configured server, keeping today's per-server behaviour verbatim: transport
    /// pick (`command` → stdio, `url` → HTTP/SSE), `tracing::warn!` on connect failure,
    /// swallowed `tools/list` failure, tool registration.
    ///
    /// `Ok(true)` = a client (and whatever tools it reported) was registered; `Ok(false)` =
    /// the entry names neither `command` nor `url`, so nothing happens and nothing is
    /// warned (unchanged); `Err(reason)` = the connection failed, with the reason in the
    /// payload form the `Failed` status wants.
    async fn boot_one(&self, name: &str, cfg: &McpServerConfig) -> Result<bool, String> {
        if cfg.command.is_none() && cfg.url.is_none() {
            return Ok(false);
        }

        let (client, instructions) = connect_client(name, cfg).await?;

        // `tools/list` failures stay swallowed, exactly as the original `if let Ok(…)` did:
        // a server that answers `initialize` but not `tools/list` still contributes its
        // client. `unwrap_or_default()` is that same swallow (empty tool list).
        let tools = client.list_tools().await.unwrap_or_default();

        let slot = Arc::new(ServerSlot::new(name.to_string(), cfg.clone(), Some(client)));

        // Registration happens in one write, with no await inside the guard.
        let mut registry = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for tool in &tools {
            registry.tools.insert(tool.qualified_name(), tool.clone());
        }
        if let Some(inst) = instructions {
            registry.server_instructions.insert(name.to_string(), inst);
        }
        registry.server_slots.insert(name.to_string(), slot);
        drop(registry);
        Ok(true)
    }

    /// Boot all configured MCP servers and discover their tools.
    ///
    /// Source-compatible with the original entry point: eager, sequential, permissive, and
    /// returns a fully-populated `Self` (no background task, so `boot_task` stays `None`).
    pub async fn boot(servers: &HashMap<String, McpServerConfig>) -> Result<Self> {
        let manager = Self::new();
        let mut failures: Vec<String> = Vec::new();
        let mut connected = 0usize;
        for (name, cfg) in servers {
            match manager.boot_one(name, cfg).await {
                Ok(true) => connected += 1,
                Ok(false) => {}
                Err(reason) => failures.push(reason),
            }
        }
        // Per-server failures used to be invisible to the caller (warn only, `boot` always
        // returned `Ok`), so an all-failed boot is the only case that publishes `Failed`.
        if connected == 0 && !failures.is_empty() {
            let _ = manager
                .boot_tx
                .send(McpBootStatus::Failed(failures.join("; ")));
        }
        Ok(manager)
    }

    // ── Readiness API (call sites are wired by the follow-up tasks) ──────────

    /// The current boot verdict — a synchronous read of the `watch` value, never a wait.
    pub fn status(&self) -> McpBootStatus {
        self.boot_rx.borrow().clone()
    }

    /// True once the boot verdict is final (`Ready` or `Failed`): the registry may then be
    /// treated as settled.
    pub fn is_ready(&self) -> bool {
        self.status().is_terminal()
    }

    /// Await the final boot verdict.
    ///
    /// Driven by `watch::Receiver::changed()`, so it parks instead of polling: there is no
    /// sleep loop and no busy-spin. A clone of the receiver is taken per call so several
    /// waiters (and the tool advertisers that read [`status`](Self::status)) each see every
    /// published verdict.
    ///
    /// No timeout lives here: the bounded wait (`MCP_READY_TIMEOUT`) belongs to the dispatch
    /// gate, which turns a still-`Booting` registry into a clean error string.
    pub async fn await_ready(&self) -> McpBootStatus {
        let mut rx = self.boot_rx.clone();
        loop {
            // Read the current value first: it is the only way to observe a verdict that
            // was already published before this call existed.
            let current = rx.borrow_and_update().clone();
            if current.is_terminal() {
                return current;
            }
            if rx.changed().await.is_err() {
                // The publisher went away (only possible if the manager is being torn down
                // mid-wait); the snapshot we already hold is the answer.
                return self.status();
            }
        }
    }

    /// Cancel the background boot task, synchronously and idempotently.
    ///
    /// Aborting the task drops the future where it stands, which is what lets the
    /// `kill_on_drop(true)` child of a half-booted stdio server (`client.rs:103`) fire
    /// instead of leaking the process. It also publishes a terminal `Failed` verdict, so a
    /// concurrent [`await_ready`](Self::await_ready) cannot hang on a boot that no longer
    /// exists. Managers with no boot task (eager [`boot`](Self::boot), [`new`](Self::new))
    /// are untouched.
    ///
    /// The `JoinHandle` is deliberately **kept** in the slot rather than consumed
    /// by this call, so the bounded [`shutdown`](Self::shutdown) that follows it
    /// can still await the abort — see [`Self::boot_task`]. That is the difference
    /// between "the boot task was told to stop" and "the half-booted child has
    /// actually been dropped and reaped".
    pub fn shutdown_boot_task(&self) {
        let slot = self
            .boot_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(task) = slot.as_ref() else {
            return; // eager boot, or a manager that never started a boot
        };
        // `abort()` is idempotent, and the handle stays put for `shutdown` to take.
        task.abort();
        drop(slot);
        // Only an in-flight boot gets a verdict published: a settled verdict is
        // never rewritten, and no waiter can be left hanging either way. The payload is
        // the sentinel [`McpBootStatus::BOOT_ABORT_REASON`] — it deliberately does **not**
        // claim a server failure, because nothing here looked at the servers.
        if self.status() == McpBootStatus::Booting {
            let _ = self.boot_tx.send(McpBootStatus::Failed(
                McpBootStatus::BOOT_ABORT_REASON.to_string(),
            ));
        }
    }

    /// How many configured servers actually came up, i.e. how many live clients the
    /// registry holds. Synchronous and cheap (one read guard, no await), so the exit path
    /// in `src/main.rs` can state a **real** count instead of claiming a boot failure it
    /// never observed: an aborted boot says "the process is going away", and the servers it
    /// never reached may all be fine.
    pub fn booted_server_count(&self) -> usize {
        let registry = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.server_slots.len()
    }

    /// Returns the number of servers that currently have an active (connected) process.
    pub async fn active_server_count(&self) -> usize {
        let slots: Vec<Arc<ServerSlot>> = {
            let registry = self
                .inner
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.server_slots.values().map(Arc::clone).collect()
        };
        let mut count = 0;
        for slot in slots {
            if slot.is_connected().await {
                count += 1;
            }
        }
        count
    }

    /// Inspect and reap all servers whose connections have exceeded their idle timeout.
    pub async fn reap_idle_servers(&self) {
        let slots: Vec<Arc<ServerSlot>> = {
            let registry = self
                .inner
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.server_slots.values().map(Arc::clone).collect()
        };
        for slot in slots {
            slot.reap_if_idle().await;
        }
    }

    /// Start the idle reaper background task if not already running.
    pub fn start_reaper(self: &Arc<Self>, handle: &tokio::runtime::Handle) {
        let mut slot = self
            .reaper_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            let weak = Arc::downgrade(self);
            let task = handle.spawn(async move {
                let check_interval = std::time::Duration::from_secs(10);
                loop {
                    tokio::time::sleep(check_interval).await;
                    let Some(mgr) = weak.upgrade() else {
                        break;
                    };
                    mgr.reap_idle_servers().await;
                }
            });
            *slot = Some(task);
        }
    }

    pub fn tools(&self) -> Vec<McpTool> {
        let registry = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.tools.values().cloned().collect()
    }

    /// Returns only the tools whose `server_name` is contained in `servers`.
    /// If `servers` is empty, returns an empty Vec.
    pub fn tools_for_servers(&self, servers: &[String]) -> Vec<McpTool> {
        if servers.is_empty() {
            return Vec::new();
        }
        let registry = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry
            .tools
            .values()
            .filter(|tool| servers.iter().any(|s| s == &tool.server_name))
            .cloned()
            .collect()
    }

    pub fn has_tool(&self, name: &str) -> bool {
        let registry = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.tools.contains_key(name)
    }

    /// Return the server usage `instructions` reported by `server_name` during its
    /// `initialize` handshake, if any.
    pub fn server_instructions(&self, server_name: &str) -> Option<String> {
        let registry = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.server_instructions.get(server_name).cloned()
    }

    /// Record or override server usage `instructions` for `server_name` in the
    /// registry (used by in-memory test fixtures and direct registrations).
    pub fn set_server_instructions(&self, server_name: &str, instructions: impl Into<String>) {
        let text = instructions.into();
        let trimmed = text.trim();
        let mut registry = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if trimmed.is_empty() {
            registry.server_instructions.remove(server_name);
        } else {
            registry
                .server_instructions
                .insert(server_name.to_string(), trimmed.to_string());
        }
    }

    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<String> {
        // Clone out of the registry under the read guard, then release it: a guard held
        // across the `tools/call` await would block every synchronous advertiser in the
        // process for the lifetime of the round trip.
        let (tool, slot) = {
            let registry = self
                .inner
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let tool = registry
                .tools
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("unknown MCP tool '{name}'"))?;
            let slot = registry
                .server_slots
                .get(&tool.server_name)
                .map(Arc::clone)
                .ok_or_else(|| anyhow!("MCP server '{}' is not running", tool.server_name))?;
            (tool, slot)
        };
        let client = slot.get_or_connect().await?;
        let res = client.call_tool(&tool.name, arguments).await;
        if res.is_ok() {
            slot.touch().await;
        } else {
            let should_disconnect = if client.is_dead().await {
                true
            } else if let Err(ref e) = res {
                let err_str = e.to_string();
                err_str.contains("closed stdout stream") || err_str.contains("Broken pipe")
            } else {
                false
            };
            if should_disconnect {
                slot.disconnect().await;
            }
        }
        res
    }

    /// Bounded wait for the (already aborted) boot task inside
    /// [`shutdown`](Self::shutdown).
    const BOOT_TASK_SHUTDOWN_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);

    /// Whole budget of the client pass inside [`shutdown`](Self::shutdown), shared
    /// as a deadline between the clients so one wedged transport lock cannot stop
    /// the clients that come after it from being killed.
    const CLIENT_SWEEP_BUDGET: std::time::Duration = std::time::Duration::from_millis(140);

    /// The single teardown entry point of the MCP registry — and the reason there is
    /// no `Drop` anywhere on `McpManager`, `McpClient` or `StdioMcpConnection`:
    ///
    /// A `Drop` cannot do this job. The manager is parked in the process-global MCP
    /// gate (`harness::set_mcp_manager`) for the life of the process, so its clients
    /// are never dropped by anything; and a drop glue that had to `await` a kill and a
    /// reap would either block (unsafe, and useless on a runtime that is already gone)
    /// or silently not run. Teardown therefore has to be an explicit, *bounded* call,
    /// and the ordering contract is:
    ///
    /// 1. [`shutdown_boot_task`](Self::shutdown_boot_task) — synchronous abort plus a
    ///    terminal verdict, so a boot that is still connecting stops right where it
    ///    stands;
    /// 2. `shutdown()` — bounded, self-sufficient (it re-runs step 1 when the caller
    ///    already did it, which is free), killing every client that did come up.
    ///
    /// Both run inside the runtime's own shutdown window in `main.rs`:
    /// `orchestrator::cancel_all()` → `shutdown_boot_task()` →
    /// `timeout(250 ms, shutdown())` → `rt.shutdown_timeout(300 ms)`. That window is
    /// what makes the reaping real: tokio reaps children through its process
    /// driver, so a runtime that is only timed out and dropped forgets children
    /// instead of reaping them.
    ///
    /// Never panics, never deadlocks, and safe to call twice, on an empty registry,
    /// on an eager [`boot`](Self::boot) manager (`boot_task` is `None`), or after
    /// [`shutdown_boot_task`](Self::shutdown_boot_task): the abort is idempotent,
    /// and after a reap the child is fused to its cached exit status, so a second
    /// `kill()` is `Ok(())` without touching the process table.
    ///
    /// It also never awaits an unbounded transport read: the only work it does is
    /// `Child::kill()` (SIGKILL + reap) per client, each bounded by
    /// [`Self::CLIENT_SWEEP_BUDGET`]. Caveat worth knowing: killing a client needs its
    /// transport lock, so a lock held by a round trip to a wedged server eats that
    /// client's slice of the budget and the child is only SIGKILLed late (or, if the
    /// outer 250 ms window runs out, left to the `kill_on_drop` of the pinned
    /// `Arc<McpManager>` at process exit).
    ///
    /// The registry itself stays in place, so a second call is a no-op rather than a
    /// lookup failure, and the tool advertisers keep answering until the process ends.
    pub async fn shutdown(&self) {
        // (1) Stop the boot where it stands. Free to repeat, and self-sufficient:
        // `main.rs` does it first, but a caller that forgets does not orphan anything.
        self.shutdown_boot_task();

        // Stop the idle reaper task.
        let reaper = {
            let mut slot = self
                .reaper_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.take()
        };
        if let Some(task) = reaper {
            task.abort();
        }

        // (2) Take the already-aborted handle and await it, bounded. Awaiting is the
        // point: the aborted future — including any half-booted stdio connection and
        // its `kill_on_drop(true)` child — is dropped by the runtime *before* this
        // call resolves, so nothing is left dangling for the registry pass below.
        let boot = {
            let mut slot = self
                .boot_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.take()
        };
        if let Some(task) = boot
            && tokio::time::timeout(Self::BOOT_TASK_SHUTDOWN_BUDGET, task)
                .await
                .is_err()
        {
            tracing::debug!(
                "MCP boot task did not unwind within {} ms",
                Self::BOOT_TASK_SHUTDOWN_BUDGET.as_millis()
            );
        }

        // (3) Every connected client. One shared deadline keeps the whole
        // pass bounded instead of spending one budget per server.
        let slots: Vec<Arc<ServerSlot>> = {
            let registry = self
                .inner
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.server_slots.values().map(Arc::clone).collect()
        };
        let sweep = std::time::Instant::now() + Self::CLIENT_SWEEP_BUDGET;
        for slot in slots {
            let left = sweep.saturating_duration_since(std::time::Instant::now());
            if tokio::time::timeout(left, slot.disconnect()).await.is_err() {
                tracing::debug!(
                    "MCP client teardown exceeded its {} ms slice; the child was \
                    killed but may not have been reaped",
                    Self::CLIENT_SWEEP_BUDGET.as_millis()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_tool(server_name: &str, name: &str) -> McpTool {
        McpTool {
            name: name.to_string(),
            description: Some(format!("{name} from {server_name}")),
            input_schema: json!({"type": "object"}),
            server_name: server_name.to_string(),
        }
    }

    /// Seed one tool the way `boot_one` registers it (the maps now live behind
    /// `inner: RwLock<McpRegistry>`, which the tests may touch directly).
    fn seed_tool(manager: &McpManager, tool: McpTool) {
        let mut registry = manager.inner.write().expect("registry write lock");
        registry.tools.insert(tool.qualified_name(), tool);
    }

    /// Seed one client under `server`, mirroring the other half of `boot_one`.
    fn seed_client(manager: &McpManager, server: &str, client: Arc<McpClient>) {
        let mut registry = manager.inner.write().expect("registry write lock");
        let slot = Arc::new(ServerSlot::new(
            server.to_string(),
            McpServerConfig {
                idle_timeout_secs: Some(0),
                ..Default::default()
            },
            Some(client),
        ));
        registry.server_slots.insert(server.to_string(), slot);
    }

    #[test]
    fn qualified_name_joins_server_and_tool() {
        let tool = sample_tool("alpha", "get_weather");
        assert_eq!(tool.qualified_name(), "alpha__get_weather");
    }

    #[test]
    fn qualified_name_disambiguates_colliding_tool_names() {
        let a = sample_tool("alpha", "get_weather");
        let b = sample_tool("beta", "get_weather");
        assert_ne!(a.qualified_name(), b.qualified_name());
        assert_eq!(a.qualified_name(), "alpha__get_weather");
        assert_eq!(b.qualified_name(), "beta__get_weather");
    }

    #[tokio::test]
    async fn routing_chain_resolves_qualified_and_dispatches_raw() {
        let called_names = Arc::new(Mutex::new(Vec::<String>::new()));
        let mock = Arc::new(McpClient::Mock(Mutex::new(MockMcpConnection {
            called_names: called_names.clone(),
        })));

        let manager = McpManager::new();
        // Two servers exposing the same raw tool name must not collide.
        let tool_alpha = sample_tool("alpha", "get_weather");
        let tool_beta = sample_tool("beta", "get_weather");
        seed_tool(&manager, tool_alpha);
        seed_tool(&manager, tool_beta);
        seed_client(&manager, "alpha", Arc::clone(&mock));
        seed_client(&manager, "beta", Arc::clone(&mock));

        // has_tool matches the qualified name.
        assert!(manager.has_tool("alpha__get_weather"));
        assert!(manager.has_tool("beta__get_weather"));
        // The raw name alone is not a valid key.
        assert!(!manager.has_tool("get_weather"));

        // call_tool resolves the qualified name and dispatches the RAW name.
        let result = manager
            .call_tool("alpha__get_weather", &json!({"city": "Oslo"}))
            .await
            .expect("call should succeed");
        assert_eq!(result, "mock-result");

        let names = called_names.lock().await.clone();
        assert_eq!(names, vec!["get_weather".to_string()]);
    }

    #[tokio::test]
    async fn call_tool_unknown_qualified_name_errors() {
        let manager = McpManager::new();
        let err = manager
            .call_tool("nope__missing", &json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown MCP tool"));
    }

    #[test]
    fn tools_for_servers_filters_by_server_name() {
        let manager = McpManager::new();
        seed_tool(&manager, sample_tool("alpha", "a"));
        seed_tool(&manager, sample_tool("alpha", "b"));
        seed_tool(&manager, sample_tool("beta", "c"));

        let alpha_only = manager.tools_for_servers(&["alpha".to_string()]);
        assert_eq!(alpha_only.len(), 2);
        assert!(alpha_only.iter().all(|t| t.server_name == "alpha"));

        let alpha_and_beta = manager.tools_for_servers(&["alpha".to_string(), "beta".to_string()]);
        assert_eq!(alpha_and_beta.len(), 3);

        let none = manager.tools_for_servers(&["gamma".to_string()]);
        assert!(none.is_empty());

        let empty = manager.tools_for_servers(&[]);
        assert!(empty.is_empty());
    }

    #[test]
    fn tools_iteration_is_deterministic_and_sorted() {
        let manager = McpManager::new();
        // Insert in non-alphabetical order
        seed_tool(&manager, sample_tool("zeta", "tool"));
        seed_tool(&manager, sample_tool("alpha", "tool"));
        seed_tool(&manager, sample_tool("beta", "tool"));

        let tools = manager.tools();
        let names: Vec<_> = tools.iter().map(|t| t.qualified_name()).collect();
        assert_eq!(
            names,
            vec![
                "alpha__tool".to_string(),
                "beta__tool".to_string(),
                "zeta__tool".to_string(),
            ]
        );
    }

    // ── Lazy boot: the handle is usable before the registry has settled ──────

    /// A registry created directly is final by definition: it has no boot task, so
    /// readiness is already true and cancelling is an idempotent no-op.
    #[test]
    fn new_registry_is_ready_without_a_boot_task() {
        let manager = McpManager::new();
        assert_eq!(manager.status(), McpBootStatus::Ready);
        assert!(manager.is_ready());
        manager.shutdown_boot_task();
        manager.shutdown_boot_task();
        assert_eq!(manager.status(), McpBootStatus::Ready);
        assert!(manager.tools().is_empty());
    }

    /// Every configured entry failed to connect, so the published verdict is the `Failed`
    /// payload assembled from the per-server reasons — and `await_ready` returns it instead
    /// of hanging. `boot` itself stays permissive (it always returned `Ok` before, and does
    /// now), which is what the assertion on `Ok` below pins.
    #[tokio::test]
    async fn eager_boot_of_only_unreachable_servers_publishes_the_failed_verdict() {
        let mut servers = HashMap::new();
        servers.insert(
            "gone".to_string(),
            McpServerConfig {
                command: Some("marmel-no-such-mcp-server".to_string()),
                ..Default::default()
            },
        );

        let manager = McpManager::boot(&servers)
            .await
            .expect("boot is permissive");
        let verdict = manager.status();
        match &verdict {
            McpBootStatus::Failed(reason) => {
                assert!(
                    reason.contains("'gone'"),
                    "reason must name the server: {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(
            manager.is_ready(),
            "a terminal verdict settles the registry"
        );
    }

    /// The lazy entry point: `spawn_boot` awaits nothing, so it hands back a `Booting`
    /// handle in bounded time. Here every server is unreachable, so the verdict arrives as
    /// soon as the boot task has walked them.
    #[tokio::test]
    async fn spawn_boot_hands_back_a_handle_and_then_the_verdict() {
        let mut servers = HashMap::new();
        servers.insert(
            "nosuchserver".to_string(),
            McpServerConfig {
                command: Some("marmel-no-such-mcp-server".to_string()),
                ..Default::default()
            },
        );

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        // Nothing was awaited on the way in: the handle exists, and the registry is empty.
        assert!(manager.tools().is_empty());

        let verdict = manager.await_ready().await;
        assert!(
            matches!(verdict, McpBootStatus::Failed(_)),
            "an all-failed boot must report why: {verdict:?}"
        );
        assert_eq!(manager.status(), verdict);
        assert_eq!(
            manager.await_ready().await,
            verdict,
            "readiness is re-observable"
        );
        manager.shutdown().await;
    }

    /// A scripted stdio server that answers the handshake and `tools/list`: boot runs in
    /// the background, tools land in `inner`, and the verdict is `Ready`.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_boot_fills_the_registry_and_publishes_ready() {
        let mut servers = HashMap::new();
        servers.insert(
            "lazy".to_string(),
            scripted_server(&[
                r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#,
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"w1","description":"from lazy"}]}}"#,
            ]),
        );

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        let verdict =
            tokio::time::timeout(std::time::Duration::from_secs(10), manager.await_ready())
                .await
                .expect("the scripted boot must settle");
        assert_eq!(verdict, McpBootStatus::Ready);
        assert!(manager.has_tool("lazy__w1"));
        assert!(!manager.has_tool("lazy__nope"));
        assert_eq!(manager.tools().len(), 1);
        assert_eq!(manager.tools_for_servers(&["lazy".to_string()]).len(), 1);
        assert!(manager.tools_for_servers(&[]).is_empty());

        manager.shutdown_boot_task();
        manager.shutdown_boot_task();
        manager.shutdown().await;
    }

    /// A server that never answers keeps the boot task parked inside `initialize`.
    /// `shutdown_boot_task` cancels it from a synchronous context, publishes a terminal
    /// verdict so no waiter hangs, and is idempotent.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_boot_task_aborts_a_hanging_boot() {
        let mut servers = HashMap::new();
        servers.insert(
            "silent".to_string(),
            McpServerConfig {
                command: Some("sh".to_string()),
                args: vec!["-c".to_string(), "sleep 30".to_string()],
                ..Default::default()
            },
        );

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        // Let the boot task park inside the handshake.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.status(), McpBootStatus::Booting);
        assert!(!manager.is_ready());

        manager.shutdown_boot_task();
        manager.shutdown_boot_task();

        let verdict =
            tokio::time::timeout(std::time::Duration::from_secs(2), manager.await_ready())
                .await
                .expect("an aborted boot must publish a terminal verdict");
        assert_eq!(
            verdict,
            McpBootStatus::Failed("MCP boot task aborted".to_string())
        );
    }

    /// A partial boot is `Ready`: per-server failures only ever warned before this change,
    /// and they still do, so one unreachable server must not turn the whole verdict into a
    /// failure. Only an all-failed boot publishes `Failed` (see [`boot_all`](Self::boot_all)).
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_boot_publishes_ready_for_a_partial_boot() {
        let mut servers = HashMap::new();
        servers.insert(
            "dead".to_string(),
            McpServerConfig {
                command: Some("marmel-no-such-mcp-server".to_string()),
                ..Default::default()
            },
        );
        servers.insert(
            "live".to_string(),
            scripted_server(&[
                r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#,
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"w1","description":"from live"}]}}"#,
            ]),
        );

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        let verdict =
            tokio::time::timeout(std::time::Duration::from_secs(10), manager.await_ready())
                .await
                .expect("a boot with one live server must settle");
        assert_eq!(verdict, McpBootStatus::Ready);
        assert!(manager.has_tool("live__w1"));
        assert!(
            !manager.has_tool("dead__w1"),
            "the failed server contributes nothing"
        );

        manager.shutdown_boot_task();
        manager.shutdown().await;
    }

    /// Boot is **concurrent**: one server's slowness may not delay another server's tools,
    /// and the whole boot must not pay the sum of the servers' handshakes. Two of the three
    /// fixture servers hold their canned frames back behind the same long `sleep`, so this
    /// test pins both halves of the requirement:
    ///
    /// * the fast server's tool is live **while both slow boots are still in flight** —
    ///   which is exactly what the sequential loop got wrong: nothing at all (not another
    ///   server's tools, not an already-exec-failed server's diagnosis) could happen until
    ///   the slowest server's 30 s handshake window was over;
    /// * the verdict lands in about **one** hold and not two, which is the order-independent
    ///   proof of concurrency: a sequential loop pays 2 x the hold whatever order it walks
    ///   the configured servers in.
    #[cfg(unix)]
    #[tokio::test]
    async fn boot_is_concurrent_so_a_slow_server_cannot_hold_up_another() {
        const SLOW_HOLD_SECS: u64 = 2;
        const INIT: &str = r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#;
        let frames = [
            INIT,
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"w1","description":"from fast"}]}}"#,
        ];

        let mut servers = HashMap::new();

        // Two servers whose handshake — and therefore registration — lands late: the replay
        // script is preceded by a long hold, so their canned frames only reach the transport
        // once it has elapsed.
        let mut slow_a = scripted_server(&[
            INIT,
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"w2","description":"from slow-a"}]}}"#,
        ]);
        let mut slow_b = scripted_server(&[
            INIT,
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"w3","description":"from slow-b"}]}}"#,
        ]);
        for cfg in [&mut slow_a, &mut slow_b] {
            let script = cfg
                .args
                .get(1)
                .expect("the scripted server carries its replay script")
                .clone();
            cfg.args[1] = format!("sleep {SLOW_HOLD_SECS} ; {script}");
        }
        servers.insert("slow-a".to_string(), slow_a);
        servers.insert("slow-b".to_string(), slow_b);

        // One server that answers as soon as it is asked.
        servers.insert("fast".to_string(), scripted_server(&frames));

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);

        // (1) The fast server's tool becomes live strictly inside the slow servers' hold:
        // the poll loop below is bounded to half a hold, so a fast tool that had to wait
        // behind a slow server (the old sequential loop) blows that bound.
        let started = std::time::Instant::now();
        let fast_landed = tokio::time::timeout(
            std::time::Duration::from_millis(SLOW_HOLD_SECS * 1000 / 2),
            async {
                loop {
                    if manager.has_tool("fast__w1") {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            },
        )
        .await
        .is_ok();
        let waited = started.elapsed();

        // (2) Still inside the hold: neither slow server has landed, so the verdict is
        // still `Booting` — an advertiser sees the fast tool before either slow server got
        // a turn in the old sequential loop.
        assert!(
            fast_landed,
            "the fast server's tool must land inside half of one slow server's hold \
             (waited {waited:?}, hold is {SLOW_HOLD_SECS}s)"
        );
        assert!(
            manager.has_tool("fast__w1"),
            "the fast server's tool must be live"
        );
        assert!(
            !manager.has_tool("slow-a__w2") && !manager.has_tool("slow-b__w3"),
            "both slow servers must still be inside their hold when the fast tool lands, \
             otherwise this test observed nothing"
        );
        assert_eq!(
            manager.status(),
            McpBootStatus::Booting,
            "one server landed, the others are still connecting"
        );
        assert_eq!(
            manager.booted_server_count(),
            1,
            "only the fast server is live so far"
        );

        // (3) The slow servers then land on their own schedule, and in parallel with each
        // other: the whole boot costs one hold plus slack — never the sum of both holds.
        // A sequential per-server loop pays 2 x 2s = 4s here, in every iteration order.
        let verdict =
            tokio::time::timeout(std::time::Duration::from_secs(10), manager.await_ready())
                .await
                .expect("the concurrent boot must settle once the slow servers answer");
        let total = started.elapsed();
        assert!(
            total < std::time::Duration::from_millis(SLOW_HOLD_SECS * 1000 + 1000),
            "the whole boot must cost about one hold, not the sum of both holds \
             ({total:?} for {SLOW_HOLD_SECS}s holds on two servers)"
        );
        assert!(
            total >= std::time::Duration::from_millis(SLOW_HOLD_SECS * 1000),
            "the slow servers must really have been held back, or this test proved nothing"
        );
        assert_eq!(verdict, McpBootStatus::Ready);
        assert!(manager.has_tool("fast__w1"));
        assert!(manager.has_tool("slow-a__w2"));
        assert!(manager.has_tool("slow-b__w3"));
        assert_eq!(
            manager.booted_server_count(),
            3,
            "every configured server came up"
        );

        manager.shutdown_boot_task();
        manager.shutdown().await;
    }

    /// A boot task parked inside its handshake holds no registry guard, so every
    /// synchronous read still answers. Had `boot_one` kept a `std::sync::RwLock` guard
    /// across an await, this single-threaded runtime would deadlock right here.
    #[cfg(unix)]
    #[tokio::test]
    async fn registry_reads_stay_available_while_boot_is_parked() {
        let mut servers = HashMap::new();
        servers.insert(
            "parked".to_string(),
            McpServerConfig {
                command: Some("sh".to_string()),
                args: vec!["-c".to_string(), "sleep 30".to_string()],
                ..Default::default()
            },
        );

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(manager.status(), McpBootStatus::Booting);

        // None of these may block: the guard behind them must be out of the way while the
        // boot task sits inside `initialize`.
        assert!(manager.tools().is_empty());
        assert!(!manager.has_tool("parked__w1"));
        assert!(
            manager
                .tools_for_servers(&["parked".to_string()])
                .is_empty()
        );
        assert!(!manager.is_ready());

        manager.shutdown_boot_task();
        let _ = manager.await_ready().await;
    }

    /// `call_tool` copies the tool and the client Arc out of the registry and releases the
    /// read guard before it awaits the round trip: a write lock on `inner` still resolves
    /// while a call is in flight.
    #[tokio::test]
    async fn call_tool_releases_the_registry_guard_before_the_round_trip() {
        let called_names = Arc::new(Mutex::new(Vec::<String>::new()));
        let mock = Arc::new(McpClient::Mock(Mutex::new(MockMcpConnection {
            called_names: called_names.clone(),
        })));
        let manager = Arc::new(McpManager::new());
        seed_tool(&manager, sample_tool("alpha", "slow"));
        seed_client(&manager, "alpha", Arc::clone(&mock));

        // Park the transport behind its own lock, so the spawned `call_tool` stays inside
        // its await while we probe the registry.
        let transport = match mock.as_ref() {
            McpClient::Mock(lock) => lock,
            _ => panic!("the mock client is expected"),
        };
        let held = transport.lock().await;

        let manager_ref = Arc::clone(&manager);
        let call = tokio::task::spawn(async move {
            manager_ref
                .call_tool("alpha__slow", &json!({"when": "then"}))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let probe = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let registry = manager.inner.write().expect("registry write lock");
            drop(registry);
        })
        .await;
        drop(held);
        assert!(
            probe.is_ok(),
            "a registry write must be possible while a `call_tool` is in flight; the read \
             guard may never be held across an await"
        );
        call.await
            .expect("the call task must not panic")
            .expect("the mock call should succeed");
        assert_eq!(
            *called_names.lock().await,
            vec!["slow".to_string()],
            "the raw tool name is what reaches the client"
        );
    }

    // ── The stdio transport reads responses through the shared pump ────────

    /// `cat` — looked up on `PATH` instead of hard-coding its absolute path,
    /// which differs between runners (`/usr/bin` on macOS) — echoes every line
    /// written to it, which is enough to drive the `initialize` handshake and a
    /// `tools/list` round-trip through `crate::net::pump_future`.
    ///
    /// Unix-only: the point of the test is the echoed byte stream, and Windows
    /// has no equivalent that mirrors a piped stdin back out (`more` paginates
    /// and buffers instead of echoing).
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_response_is_read_through_the_shared_pump() {
        let cfg = McpServerConfig {
            command: Some("cat".to_string()),
            ..Default::default()
        };

        let mut conn =
            StdioMcpConnection::spawn_with("echo-server", &cfg, std::time::Duration::from_secs(5))
                .await
                .expect("the cat-backed handshake should succeed");

        let tools = conn
            .list_tools()
            .await
            .expect("the echoed request must be accepted as an empty answer");
        assert!(tools.is_empty(), "the echo carries no tool list");

        conn.shutdown().await.expect("shutdown should succeed");
    }

    /// The pump's idle deadline still surfaces the transport's own timeout error.
    ///
    /// Cross-platform by design: the only requirement is a child that stays
    /// alive and never writes to stdout, so on Windows `cmd /C ping …` (output
    /// sent to `NUL`) stands in for `sleep`. Program names come from `PATH`;
    /// their absolute paths differ between runners (`/usr/bin` on macOS).
    #[tokio::test]
    async fn stdio_read_hits_the_shared_pump_deadline() {
        // A "server" that never writes anything back.
        let (command, args) = if cfg!(windows) {
            (
                "cmd".to_string(),
                vec!["/C".to_string(), "ping -n 30 127.0.0.1 > NUL".to_string()],
            )
        } else {
            // The sleep program is looked up through `PATH` here — no absolute
            // path, whose location differs between Unix runners.
            ("sleep".to_string(), vec!["30".to_string()])
        };
        let cfg = McpServerConfig {
            command: Some(command),
            args,
            ..Default::default()
        };

        let err = match StdioMcpConnection::spawn_with(
            "silent-server",
            &cfg,
            std::time::Duration::from_millis(300),
        )
        .await
        {
            Ok(_) => panic!("a server that never answers must hit the deadline"),
            Err(e) => e.to_string(),
        };

        assert!(
            err.contains("timeout waiting for MCP server 'silent-server' response"),
            "expected the unchanged deadline message, got: {err}"
        );
    }

    // ── The stdio transport speaks the shared protocol layer ───────────────

    #[cfg(unix)]
    use crate::mcp::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcTransport};

    /// Read `path` until it holds at least `want` lines, bounded so a server
    /// that never writes cannot hang the suite.
    #[cfg(unix)]
    async fn read_capture_lines(path: &std::path::Path, want: usize) -> Vec<String> {
        for _ in 0..40 {
            let lines = std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            if lines.len() >= want {
                return lines;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A scripted "server" on stdout: emits the lines and then idles so its
    /// stdout stays open while the transport drives a round-trip.
    ///
    /// Unix-only: the script is POSIX shell (`printf` + `sleep`). `sh` is
    /// resolved through `PATH` rather than hard-coding its absolute location,
    /// which differs between Unix runners.
    #[cfg(unix)]
    fn scripted_server(lines: &[&str]) -> McpServerConfig {
        let mut script = String::from("printf '%s\\n'");
        for line in lines {
            script.push_str(" '");
            script.push_str(line);
            script.push('\'');
        }
        script.push_str("; sleep 30");
        McpServerConfig {
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), script],
            ..Default::default()
        }
    }

    /// What the stdio transport puts on the wire is exactly the envelope text
    /// `crate::mcp::protocol` builds — the very same bytes the HTTP/SSE transport
    /// sends (see `http_wire_bodies_are_the_protocol_layers_envelopes` in
    /// `mcp::http_tests`), which is what makes this one protocol layer and not
    /// two.
    ///
    /// Unix-only: the capture needs a real `tee` (stdin mirrored to both the
    /// capture file and the pipe back to us), and Windows ships no such program.
    /// `tee` is resolved through `PATH`.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_wire_bodies_are_the_protocol_layers_envelopes() {
        let capture = tempfile::NamedTempFile::new().expect("capture file");
        let cfg = McpServerConfig {
            command: Some("tee".to_string()),
            args: vec![capture.path().to_string_lossy().into_owned()],
            ..Default::default()
        };

        let mut conn =
            StdioMcpConnection::spawn_with("wire-server", &cfg, std::time::Duration::from_secs(5))
                .await
                .expect("the tee-backed handshake should succeed");
        conn.list_tools().await.expect("tools/list round-trip");

        let lines = read_capture_lines(capture.path(), 3).await;
        assert_eq!(
            lines.len(),
            3,
            "initialize + notifications/initialized + tools/list, got {lines:?}"
        );

        // The handshake envelope: built by the protocol layer and pinned to its
        // literal bytes — identical to the HTTP/SSE transport's first request.
        assert_eq!(lines[0], JsonRpcRequest::initialize().body().expect("body"));
        // Pinned to the same literal the HTTP/SSE guard pins
        // (`canonical_initialize_body` in `mcp::http_tests`): byte-identical
        // handshakes from the two transports prove one protocol layer, not two.
        assert_eq!(
            lines[0],
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{"tools":{}},"clientInfo":{"name":"marmel","version":"@VERSION@"},"protocolVersion":"2024-11-05"}}"#
                .replace("@VERSION@", env!("CARGO_PKG_VERSION"))
        );

        // A notification carries no id, and the handshake consumed none, so
        // `tools/list` is id 1 here exactly as it is over HTTP/SSE.
        assert_eq!(
            lines[1],
            JsonRpcNotification::initialized()
                .body()
                .expect("notification body")
        );
        assert_eq!(
            lines[1],
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        );
        assert_eq!(
            lines[2],
            JsonRpcRequest::tools_list(1).body().expect("body")
        );
        assert_eq!(
            lines[2],
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
        );

        conn.shutdown().await.expect("shutdown should succeed");
    }

    /// A stdout line that is not a reply for the pending request — a foreign id,
    /// or not JSON at all — is skipped by the protocol layer, and the matching
    /// reply still resolves.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_skips_unrelated_lines_and_resolves_the_matching_reply() {
        let cfg = scripted_server(&[
            r#"{"jsonrpc":"2.0","id":99,"result":{"noise":true}}"#,
            "not json at all",
            r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"w1","description":"from fake"},{"name":"w2","inputSchema":{"type":"object"}}]}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"Hello "},{"type":"text","text":"World"}]}}"#,
        ]);

        let mut conn =
            StdioMcpConnection::spawn_with("fake-stdio", &cfg, std::time::Duration::from_secs(5))
                .await
                .expect("the handshake must match the id 0 line");

        let tools = conn
            .list_tools()
            .await
            .expect("the noise must be skipped and id 1 must resolve");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "w1");
        assert_eq!(tools[0].description.as_deref(), Some("from fake"));
        assert_eq!(tools[0].input_schema, json!({"type": "object"}));
        assert_eq!(tools[1].name, "w2");
        assert_eq!(tools[1].input_schema, json!({"type": "object"}));
        assert!(tools.iter().all(|t| t.server_name == "fake-stdio"));

        // The allocator continued where `tools/list` left off, so the scripted
        // id 2 reply is the answer to this `tools/call`.
        let reply = conn
            .call_tool("w1", &json!({}))
            .await
            .expect("the id 2 reply must resolve");
        assert_eq!(reply, "Hello \nWorld");

        conn.shutdown().await.expect("shutdown should succeed");
    }

    /// A JSON-RPC error envelope on stdout becomes the shared `McpError::JsonRpc`
    /// message at the caller boundary — the transport does not decode it itself.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_error_envelope_surfaces_as_the_shared_mcp_error() {
        let cfg = scripted_server(&[
            r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#,
        ]);

        let mut conn =
            StdioMcpConnection::spawn_with("fake-stdio", &cfg, std::time::Duration::from_secs(5))
                .await
                .expect("the handshake should succeed");

        let err = conn
            .list_tools()
            .await
            .expect_err("an error envelope fails");
        assert_eq!(
            err.to_string(),
            "MCP error (-32601) from 'fake-stdio': Method not found (data: None)"
        );

        conn.shutdown().await.expect("shutdown should succeed");
    }

    /// The transport seam is implemented (not shadowed) by both transports, and
    /// the two share the protocol layer's id policy: a fresh connection starts at
    /// the first request id, and the handshake does not consume one.
    ///
    /// Unix-only (the echo-backed `cat` server above): `cat` is looked up on
    /// `PATH` rather than pinned to an absolute path.
    #[cfg(unix)]
    #[tokio::test]
    async fn both_transports_share_the_protocols_id_allocator_policy() {
        let cfg = McpServerConfig {
            command: Some("cat".to_string()),
            ..Default::default()
        };
        let conn =
            StdioMcpConnection::spawn_with("echo-server", &cfg, std::time::Duration::from_secs(5))
                .await
                .expect("the cat-backed handshake should succeed");

        use crate::mcp::protocol::{FIRST_REQUEST_ID, HANDSHAKE_REQUEST_ID};
        assert_eq!(
            conn.request_ids().next(),
            FIRST_REQUEST_ID,
            "the handshake must not consume a request id"
        );
        assert_ne!(HANDSHAKE_REQUEST_ID, FIRST_REQUEST_ID);
    }

    // ── Teardown: what `shutdown` must leave behind ─────────────────────────
    //
    // The fixtures below record their own pid so the assertion is about the one
    // process that matters, and park on stdin instead of the sleep program, so no
    // grandchild is ever spawned that the test could not see or account for.

    /// A stdio server that records its own pid at `pid_path`, optionally prints
    /// `lines` to stdout, and then blocks reading stdin forever.
    ///
    /// Empty `lines` is the parked handshake: the child is alive, answers nothing,
    /// and is therefore exactly the half-booted server no registry entry exists for.
    #[cfg(unix)]
    fn parked_server(pid_path: &std::path::Path, lines: &[&str]) -> McpServerConfig {
        let mut script = format!("echo $$ > '{}';", pid_path.display());
        if lines.is_empty() {
            script.push_str(" :;"); // nothing ever reaches stdout
        } else {
            script.push_str(" printf '%s\\n'");
            for line in lines {
                script.push_str(" '");
                script.push_str(line);
                script.push('\'');
            }
            script.push(';');
        }
        script.push_str(" while :; do read _ || break; done");
        McpServerConfig {
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), script],
            ..Default::default()
        }
    }

    /// The pid the fixture wrote to `pid_path`, bounded so a server that never got
    /// that far cannot hang the suite.
    #[cfg(unix)]
    async fn fixture_pid(pid_path: &std::path::Path) -> u32 {
        for _ in 0..80 {
            if let Ok(text) = std::fs::read_to_string(pid_path)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return pid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the fixture never wrote its pid to {}", pid_path.display());
    }

    /// Is `pid` still an entry in the process table that this test is allowed to
    /// signal? `kill` is asked of `sh` because it is a shell builtin, not a program
    /// on `PATH`.
    ///
    /// The check is strict on purpose: signalling a zombie succeeds, so "not live"
    /// here means the child was killed **and reaped**, not merely killed.
    #[cfg(unix)]
    fn pid_is_live(pid: u32) -> bool {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(true)
    }

    /// The child must be gone from the process table, bounded so a lingering one
    /// fails the test instead of hanging it.
    #[cfg(unix)]
    async fn assert_reaped(pid: u32, label: &str) {
        for _ in 0..80 {
            if !pid_is_live(pid) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!(
            "{label}: pid {pid} is still in the process table after teardown — \
             killed and reaped means gone"
        );
    }

    /// The half-booted server no registry entry exists for: `shutdown` must abort
    /// the boot task **and** await it, because the child lives inside the future
    /// the boot task is parked in, and its `kill_on_drop(true)` is what reaps it.
    /// Awaiting (bounded) is also what makes the abort observable — dropping the
    /// future only happens when the runtime next polls that task.
    ///
    /// `shutdown_boot_task()` is deliberately *not* called first: `shutdown` has to
    /// be self-sufficient, and it must return in bounded time no matter how long
    /// the parked handshake is prepared to wait (30 s here).
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_reaps_a_boot_that_never_answered_the_handshake() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let pid_path = dir.path().join("pid");
        let mut servers = HashMap::new();
        servers.insert("parked".to_string(), parked_server(&pid_path, &[]));

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        let pid = fixture_pid(&pid_path).await;
        assert_eq!(manager.status(), McpBootStatus::Booting);
        // The client never made it into the registry, so the registry pass of
        // `shutdown` could not have seen this child even to kill it.
        assert!(manager.tools().is_empty());

        let started = std::time::Instant::now();
        manager.shutdown().await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "a boot parked in a 30 s handshake must not keep teardown open \
             ({elapsed:?})"
        );
        assert_reaped(pid, "the half-booted stdio child").await;
        // The abort still publishes the terminal verdict, for any waiter.
        assert_eq!(
            manager.status(),
            McpBootStatus::Failed("MCP boot task aborted".to_string())
        );

        // …and a second teardown is a no-op, not a panic or a hang.
        let started = std::time::Instant::now();
        manager.shutdown().await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "the second teardown must be free ({:?})",
            started.elapsed()
        );
    }

    /// The `main.rs` order, called twice over: the synchronous abort, the bounded
    /// `shutdown` that awaits what it aborted, and `shutdown` again on top. The
    /// handle must be taken by the first `shutdown`, so the second one awaits
    /// nothing at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_is_idempotent_across_the_documented_teardown_order() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let pid_path = dir.path().join("pid");
        let mut servers = HashMap::new();
        servers.insert("parked".to_string(), parked_server(&pid_path, &[]));

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        let pid = fixture_pid(&pid_path).await;

        manager.shutdown_boot_task();
        manager.shutdown_boot_task();
        manager.shutdown().await;
        assert_reaped(pid, "the aborted stdio child").await;

        // The handle the first `shutdown` awaited has been taken out of the slot,
        // so the second and third calls await nothing at all. Scoped, because the
        // guard must not exist across the awaits below.
        {
            let slot = manager
                .boot_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                slot.is_none(),
                "`shutdown` must take the handle of the boot task it awaited"
            );
        }

        let started = std::time::Instant::now();
        manager.shutdown().await;
        manager.shutdown().await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(400),
            "an already-settled teardown stays cheap ({:?})",
            started.elapsed()
        );
        // The registry survives teardown for the advertisers' sake.
        assert!(manager.tools().is_empty());
    }

    /// An eager [`boot`](Self::boot) manager has no boot task to abort at all, and
    /// its client *is* in the registry: the kill has to come from the registry pass.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_the_child_of_an_eager_boot_with_no_boot_task() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let pid_path = dir.path().join("pid");
        let mut servers = HashMap::new();
        servers.insert(
            "eager".to_string(),
            parked_server(
                &pid_path,
                &[
                    r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#,
                    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"w1","description":"from eager"}]}}"#,
                ],
            ),
        );

        let manager = McpManager::boot(&servers)
            .await
            .expect("boot is permissive");
        assert!(manager.has_tool("eager__w1"));
        let pid = fixture_pid(&pid_path).await;

        let started = std::time::Instant::now();
        manager.shutdown().await;
        assert_reaped(pid, "the eager-boot child").await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(900),
            "kill and reap of a live server is fast ({:?})",
            started.elapsed()
        );

        // Twice, and the tools are still readable after it.
        manager.shutdown().await;
        assert!(manager.has_tool("eager__w1"));
    }

    /// Empty registry, no boot task, eager `new()`: teardown is a bounded no-op.
    #[tokio::test]
    async fn shutdown_on_an_empty_registry_is_a_bounded_no_op() {
        let manager = McpManager::new();
        let started = std::time::Instant::now();
        manager.shutdown().await;
        manager.shutdown().await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "nothing to tear down must mean nothing to wait for ({:?})",
            started.elapsed()
        );
        // The settled verdict of a manager that never had a boot is not rewritten.
        assert_eq!(manager.status(), McpBootStatus::Ready);
    }

    /// A client whose transport lock is held by a round trip a wedged server is
    /// never going to answer must not stall the sweep: each client gets a slice of
    /// the shared budget and the sweep ends when the budget is gone.
    #[tokio::test]
    async fn shutdown_is_bounded_while_a_transport_lock_is_held() {
        let mock = Arc::new(McpClient::Mock(Mutex::new(MockMcpConnection {
            called_names: Arc::new(Mutex::new(Vec::new())),
        })));
        let manager = McpManager::new();
        seed_client(&manager, "alpha", Arc::clone(&mock));

        let transport = match mock.as_ref() {
            McpClient::Mock(lock) => lock,
            _ => panic!("the mock client is expected"),
        };
        let held = transport.lock().await;

        let started = std::time::Instant::now();
        manager.shutdown().await;
        drop(held);

        assert!(
            started.elapsed() < std::time::Duration::from_millis(1000),
            "a held transport lock must not be able to hold teardown open ({:?})",
            started.elapsed()
        );
    }

    /// The detached stderr pump is a task nothing owns: it is spawned into the
    /// runtime, and left to itself its only exit is the child's stderr EOF. The
    /// connection therefore keeps the handle, and `shutdown` aborts it, so the
    /// pump can never outlive the connection whose logs it reads.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_ends_the_detached_stderr_pump() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let pid_path = dir.path().join("pid");
        let init_reply = r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#;
        let pid_file = pid_path.display();
        // Writes to stderr forever, which is the shape of a pump that would
        // otherwise stay busy (and alive) indefinitely.
        let script = format!(
            "echo $$ > '{pid_file}'; echo pump-noise >&2; printf '%s\\n' '{init_reply}'; while :; do echo pump-noise >&2; read _ || break; done",
        );
        let cfg = McpServerConfig {
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), script],
            ..Default::default()
        };

        let mut conn =
            StdioMcpConnection::spawn_with("pumpy", &cfg, std::time::Duration::from_secs(5))
                .await
                .expect("the handshake should succeed");
        let pid = fixture_pid(&pid_path).await;

        let pump = conn
            .stderr_pump
            .as_ref()
            .expect("the connection must own the pump handle");
        let abort = pump.abort_handle();
        assert!(!abort.is_finished(), "the pump is live while the server is");

        conn.shutdown().await.expect("shutdown succeeds");
        assert!(
            conn.stderr_pump.is_none(),
            "`shutdown` must take the pump handle it aborts"
        );
        // The pump terminated: it was aborted, it was not left detached on the
        // runtime reading a dead server's stderr.
        for _ in 0..40 {
            if abort.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            abort.is_finished(),
            "no detached pump task may outlive the connection it reads for"
        );
        assert_reaped(pid, "the chatty stderr server").await;

        // A second shutdown takes a `None` pump handle and an already-reaped
        // child, i.e. does nothing at all.
        conn.shutdown().await.expect("second shutdown");
    }

    /// The other half-booted case: a handshake that fails on its own deadline
    /// tears its own connection down on the way out of `spawn_with`, instead of
    /// returning a child that only drop glue would (partially) clean up.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_handshake_kills_and_reaps_its_own_child() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let pid_path = dir.path().join("pid");
        let cfg = parked_server(&pid_path, &[]);

        let spawned =
            StdioMcpConnection::spawn_with("silent", &cfg, std::time::Duration::from_millis(300))
                .await;
        assert!(
            spawned
                .err()
                .is_some_and(|e| e.to_string().contains("timeout")),
            "the parked handshake must fail on its deadline"
        );
        let pid = fixture_pid(&pid_path).await;
        assert_reaped(pid, "the failed-handshake child").await;
    }

    #[cfg(unix)]
    fn responsive_server(tool_name: &str) -> McpServerConfig {
        let script = format!(
            r#"while IFS= read -r line; do
  case "$line" in
    *\"method\":\"initialize\"*)
      echo '{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":"2024-11-05"}}}}'
      ;;
    *\"method\":\"tools/list\"*)
      id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
      echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"tools\":[{{\"name\":\"{tool_name}\",\"description\":\"desc\"}}]}}}}"
      ;;
    *\"method\":\"tools/call\"*)
      id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
      echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"success_from_{tool_name}\"}}]}}}}"
      ;;
  esac
done"#
        );
        McpServerConfig {
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), script],
            ..Default::default()
        }
    }

    /// An idle server connection is reaped after its configured idle timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn idle_server_is_reaped_after_timeout() {
        let mut servers = HashMap::new();
        let mut cfg = responsive_server("my_tool");
        cfg.idle_timeout_secs = Some(1); // 1 second timeout
        servers.insert("ondemand_srv".to_string(), cfg);

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        let verdict = manager.await_ready().await;
        assert_eq!(verdict, McpBootStatus::Ready);
        assert_eq!(manager.booted_server_count(), 1);
        assert_eq!(manager.active_server_count().await, 1);

        // Before timeout, server remains active
        manager.reap_idle_servers().await;
        assert_eq!(manager.active_server_count().await, 1);

        // After timeout has passed, reaping drops the connection
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        manager.reap_idle_servers().await;
        assert_eq!(manager.active_server_count().await, 0);
        // The server is still registered in the registry
        assert_eq!(manager.booted_server_count(), 1);
        assert!(manager.has_tool("ondemand_srv__my_tool"));

        manager.shutdown().await;
    }

    /// When a reaped server's tool is invoked, it reconnects on-demand and succeeds.
    #[cfg(unix)]
    #[tokio::test]
    async fn reaped_server_reconnects_on_demand_and_answers() {
        let mut servers = HashMap::new();
        let mut cfg = responsive_server("calc");
        cfg.idle_timeout_secs = Some(1);
        servers.insert("calc_srv".to_string(), cfg);

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        let verdict = manager.await_ready().await;
        assert_eq!(verdict, McpBootStatus::Ready);
        assert_eq!(manager.active_server_count().await, 1);

        // Sleep to let it expire and reap it
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        manager.reap_idle_servers().await;
        assert_eq!(manager.active_server_count().await, 0);

        // Call tool on the disconnected server — should reconnect on demand!
        let res = manager
            .call_tool("calc_srv__calc", &json!({"expr": "2+2"}))
            .await
            .expect("on-demand call should succeed");
        assert_eq!(res, "success_from_calc");
        assert_eq!(manager.active_server_count().await, 1);

        // Subsequent rapid call reuses the connection without reconnecting
        let res2 = manager
            .call_tool("calc_srv__calc", &json!({"expr": "3+3"}))
            .await
            .expect("second call should reuse connection");
        assert_eq!(res2, "success_from_calc");
        assert_eq!(manager.active_server_count().await, 1);

        // Another idle period reaps it again
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        manager.reap_idle_servers().await;
        assert_eq!(manager.active_server_count().await, 0);

        // And it can reconnect yet again!
        let res3 = manager
            .call_tool("calc_srv__calc", &json!({"expr": "4+4"}))
            .await
            .expect("reconnecting after second reap");
        assert_eq!(res3, "success_from_calc");
        assert_eq!(manager.active_server_count().await, 1);

        manager.shutdown().await;
    }

    /// Setting idle_timeout_secs to Some(0) disables idle closing completely.
    #[cfg(unix)]
    #[tokio::test]
    async fn idle_timeout_zero_keeps_server_connected() {
        let mut servers = HashMap::new();
        let mut cfg = responsive_server("pin");
        cfg.idle_timeout_secs = Some(0); // 0 = disabled
        servers.insert("pinned_srv".to_string(), cfg);

        let manager = McpManager::spawn_boot(&tokio::runtime::Handle::current(), &servers);
        let verdict = manager.await_ready().await;
        assert_eq!(verdict, McpBootStatus::Ready);
        assert_eq!(manager.active_server_count().await, 1);

        // Sleep and reap — connection must remain open
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        manager.reap_idle_servers().await;
        assert_eq!(manager.active_server_count().await, 1);

        manager.shutdown().await;
    }

    #[test]
    fn mcp_server_config_request_timeout_parsing_and_defaults() {
        let default_cfg = McpServerConfig::default();
        assert_eq!(default_cfg.request_timeout(), DEFAULT_MCP_REQUEST_TIMEOUT);

        let toml_str = r#"
            command = "echo"
            request_timeout_secs = 60
        "#;
        let cfg: McpServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.request_timeout(), std::time::Duration::from_secs(60));

        let toml_str2 = r#"
            command = "echo"
            timeout_secs = 45
        "#;
        let cfg2: McpServerConfig = toml::from_str(toml_str2).unwrap();
        assert_eq!(cfg2.request_timeout(), std::time::Duration::from_secs(45));

        let toml_str3 = r#"
            command = "echo"
            timeout = 90
        "#;
        let cfg3: McpServerConfig = toml::from_str(toml_str3).unwrap();
        assert_eq!(cfg3.request_timeout(), std::time::Duration::from_secs(90));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_connection_respects_config_request_timeout() {
        let mut cfg = responsive_server("my_tool");
        cfg.request_timeout_secs = Some(15);
        let conn = StdioMcpConnection::spawn("custom-timeout", &cfg)
            .await
            .expect("spawn");
        assert_eq!(conn.request_timeout(), std::time::Duration::from_secs(15));
    }
}
