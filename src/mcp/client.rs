//! JSON-RPC 2.0 MCP Client implementation for stdio and HTTP/SSE.
//!
//! The stdio connection below is a raw transport only: the JSON-RPC envelopes,
//! ids, method names, param shapes and result/error decoding come from
//! [`super::protocol`] (cluster C7), exactly like the HTTP/SSE transport in
//! [`super::http`] consumes them. What is stdio-specific here is the child
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

use super::http::{HttpSseConnection, MCP_REQUEST_TIMEOUT};
use super::protocol::{
    self, JsonRpcNotification, JsonRpcRequest, JsonRpcTransport, RequestIds, decode_reply,
};

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
    /// (`crate::mcp::http::MCP_REQUEST_TIMEOUT`).
    request_timeout: std::time::Duration,
}

impl StdioMcpConnection {
    pub async fn spawn(server_name: &str, cfg: &McpServerConfig) -> Result<Self> {
        Self::spawn_with(server_name, cfg, MCP_REQUEST_TIMEOUT).await
    }

    /// Spawn with an explicit response deadline. Production callers go through
    /// [`Self::spawn`] (shared `MCP_REQUEST_TIMEOUT`); the seam keeps the
    /// deadline behaviour testable without waiting 30 s.
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
        if let Some(stderr) = stderr {
            let s_name = server_name.to_string();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut reader = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::debug!(target: "mcp", "[{s_name}] {line}");
                }
            });
        }

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
        };

        protocol::initialize(&mut conn).await?;
        Ok(conn)
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
            // Shared pump skeleton from `crate::net::sse` (cluster C4) replaces
            // the hand-rolled `tokio::time::timeout` around `read_line`. The
            // deadline is recomputed per read, preserving the flat per-response
            // timeout the previous code had.
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

    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.child.kill().await;
        Ok(())
    }
}

pub enum McpClient {
    Stdio(Box<Mutex<StdioMcpConnection>>),
    HttpSse(Mutex<HttpSseConnection>),
    /// Test-only mock used to assert the routing chain without a live server.
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
}

/// Global registry and lifecycle manager for all active MCP clients.
#[derive(Default)]
pub struct McpManager {
    clients: HashMap<String, Arc<McpClient>>,
    tools: BTreeMap<String, McpTool>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Boot all configured MCP servers and discover their tools.
    pub async fn boot(servers: &HashMap<String, McpServerConfig>) -> Result<Self> {
        let mut manager = Self::new();
        for (name, cfg) in servers {
            if cfg.command.is_some() {
                match StdioMcpConnection::spawn(name, cfg).await {
                    Ok(conn) => {
                        let client = Arc::new(McpClient::Stdio(Box::new(Mutex::new(conn))));
                        if let Ok(tools) = client.list_tools().await {
                            for tool in tools {
                                manager.tools.insert(tool.qualified_name(), tool);
                            }
                        }
                        manager.clients.insert(name.clone(), client);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to start MCP server '{name}': {e:#}");
                    }
                }
            } else if cfg.url.is_some() {
                match HttpSseConnection::connect(name, cfg).await {
                    Ok(conn) => {
                        let client = Arc::new(McpClient::HttpSse(Mutex::new(conn)));
                        if let Ok(tools) = client.list_tools().await {
                            for tool in tools {
                                manager.tools.insert(tool.qualified_name(), tool);
                            }
                        }
                        manager.clients.insert(name.clone(), client);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to connect to MCP server '{name}': {e:#}");
                    }
                }
            }
        }
        Ok(manager)
    }

    pub fn tools(&self) -> Vec<McpTool> {
        self.tools.values().cloned().collect()
    }

    /// Returns only the tools whose `server_name` is contained in `servers`.
    /// If `servers` is empty, returns an empty Vec.
    pub fn tools_for_servers(&self, servers: &[String]) -> Vec<McpTool> {
        if servers.is_empty() {
            return Vec::new();
        }
        self.tools
            .values()
            .filter(|tool| servers.iter().any(|s| s == &tool.server_name))
            .cloned()
            .collect()
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow!("unknown MCP tool '{name}'"))?;
        let client = self
            .clients
            .get(&tool.server_name)
            .ok_or_else(|| anyhow!("MCP server '{}' is not running", tool.server_name))?;
        client.call_tool(&tool.name, arguments).await
    }

    pub async fn shutdown(&self) {
        for client in self.clients.values() {
            let _ = client.shutdown().await;
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

        let mut manager = McpManager::new();
        // Two servers exposing the same raw tool name must not collide.
        let tool_alpha = sample_tool("alpha", "get_weather");
        let tool_beta = sample_tool("beta", "get_weather");
        manager
            .tools
            .insert(tool_alpha.qualified_name(), tool_alpha);
        manager.tools.insert(tool_beta.qualified_name(), tool_beta);
        manager.clients.insert("alpha".to_string(), mock.clone());
        manager.clients.insert("beta".to_string(), mock.clone());

        // has_tool matches the qualified name.
        assert!(manager.has_tool("alpha__get_weather"));
        assert!(manager.has_tool("beta__get_weather"));
        // The raw name alone is no longer a valid key.
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
        let mut manager = McpManager::new();
        manager
            .tools
            .insert("alpha__a".to_string(), sample_tool("alpha", "a"));
        manager
            .tools
            .insert("alpha__b".to_string(), sample_tool("alpha", "b"));
        manager
            .tools
            .insert("beta__c".to_string(), sample_tool("beta", "c"));

        let alpha_only = manager.tools_for_servers(&["alpha".to_string()]);
        assert_eq!(alpha_only.len(), 2);
        assert!(alpha_only.iter().all(|t| t.server_name == "alpha"));

        let alpha_and_beta = manager.tools_for_servers(&["alpha".to_string(), "beta".to_string()]);
        assert_eq!(alpha_and_beta.len(), 3);

        let none = manager.tools_for_servers(&["gamma".to_string()]);
        assert!(none.is_empty());

        // Empty input list yields an empty result.
        let empty = manager.tools_for_servers(&[]);
        assert!(empty.is_empty());
    }

    #[test]
    fn tools_iteration_is_deterministic_and_sorted() {
        let mut manager = McpManager::new();
        // Insert in non-alphabetical order
        manager
            .tools
            .insert("zeta__tool".to_string(), sample_tool("zeta", "tool"));
        manager
            .tools
            .insert("alpha__tool".to_string(), sample_tool("alpha", "tool"));
        manager
            .tools
            .insert("beta__tool".to_string(), sample_tool("beta", "tool"));

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

    // ── net-C4: the stdio transport reads responses through the shared pump ──

    /// `/bin/cat` echoes every line written to it, which is enough to drive the
    /// `initialize` handshake and a `tools/list` round-trip through
    /// `crate::net::pump_future` — the shared replacement for the hand-rolled
    /// `tokio::time::timeout` around `read_line`.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_response_is_read_through_the_shared_pump() {
        let cfg = McpServerConfig {
            command: Some("/bin/cat".to_string()),
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
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_read_hits_the_shared_pump_deadline() {
        // A "server" that never writes anything back.
        let cfg = McpServerConfig {
            command: Some("/bin/sleep".to_string()),
            args: vec!["30".to_string()],
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

    // ── net-C7: the stdio transport speaks the shared protocol layer ────────

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
            command: Some("/bin/sh".to_string()),
            args: vec!["-c".to_string(), script],
            ..Default::default()
        }
    }

    /// Cluster C7 guard: what the stdio transport puts on the wire is exactly
    /// the envelope text `crate::mcp::protocol` builds — the very same bytes the
    /// HTTP/SSE transport sends (see `http_wire_bodies_are_the_protocol_layers_envelopes`
    /// in `mcp::http_tests`), which is what makes this one protocol layer and
    /// not two.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_wire_bodies_are_the_protocol_layers_envelopes() {
        let capture = tempfile::NamedTempFile::new().expect("capture file");
        let cfg = McpServerConfig {
            command: Some("/bin/tee".to_string()),
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
    #[cfg(unix)]
    #[tokio::test]
    async fn both_transports_share_the_protocols_id_allocator_policy() {
        let cfg = McpServerConfig {
            command: Some("/bin/cat".to_string()),
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
}
