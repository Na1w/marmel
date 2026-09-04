//! JSON-RPC 2.0 MCP Client implementation for stdio and HTTP/SSE.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use super::http::{HttpSseConnection, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};

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
    request_id: AtomicU64,
}

impl StdioMcpConnection {
    pub async fn spawn(server_name: &str, cfg: &McpServerConfig) -> Result<Self> {
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
        cmd.stderr(Stdio::inherit());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn MCP server '{server_name}' ({cmd_str})"))?;

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
            request_id: AtomicU64::new(1),
        };

        conn.initialize().await?;
        Ok(conn)
    }

    async fn send_request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        crate::debug_log::log_mcp_request(&self.server_name, method, req.params.as_ref());
        let start_time = std::time::Instant::now();

        let mut req_str = serde_json::to_string(&req)?;
        req_str.push('\n');

        self.stdin
            .write_all(req_str.as_bytes())
            .await
            .with_context(|| format!("writing request to MCP server '{}'", self.server_name))?;
        self.stdin.flush().await?;

        let timeout_duration = std::time::Duration::from_secs(30);
        let mut line = String::new();
        loop {
            line.clear();
            let read_fut = self.stdout_reader.read_line(&mut line);
            let n = tokio::time::timeout(timeout_duration, read_fut)
                .await
                .map_err(|_| {
                    anyhow!(
                        "timeout waiting for MCP server '{}' response (30s)",
                        self.server_name
                    )
                })??;
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
            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
                if !resp.id_matches(id) {
                    continue;
                }
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

    async fn send_notification(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
        };
        let mut notif_str = serde_json::to_string(&notif)?;
        notif_str.push('\n');

        self.stdin.write_all(notif_str.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn initialize(&mut self) -> Result<()> {
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "clientInfo": {
                "name": "marmel",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let _result = self.send_request("initialize", Some(init_params)).await?;
        self.send_notification("notifications/initialized", None)
            .await?;
        Ok(())
    }

    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        let result = self.send_request("tools/list", None).await?;
        let mut tools = Vec::new();
        if let Some(tools_arr) = result.get("tools").and_then(Value::as_array) {
            for t in tools_arr {
                if let Some(name) = t.get("name").and_then(Value::as_str) {
                    let description = t
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let input_schema = t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
                    tools.push(McpTool {
                        name: name.to_string(),
                        description,
                        input_schema,
                        server_name: self.server_name.clone(),
                    });
                }
            }
        }
        Ok(tools)
    }

    pub async fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<String> {
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments
        });

        let result = self.send_request("tools/call", Some(params)).await?;

        let mut output = String::new();
        if let Some(content_arr) = result.get("content").and_then(Value::as_array) {
            for item in content_arr {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    output.push_str(text);
                } else {
                    output.push_str(&item.to_string());
                }
                output.push('\n');
            }
        } else if let Some(text) = result.get("text").and_then(Value::as_str) {
            output.push_str(text);
        } else if !result.is_null() {
            output.push_str(&result.to_string());
        }

        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_error {
            Err(anyhow!(output.trim().to_string()))
        } else {
            Ok(output.trim().to_string())
        }
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
}
