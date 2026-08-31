//! MCP HTTP + SSE transport for remote (external) MCP servers.
//!
//! Implements the MCP "Streamable HTTP" transport: JSON-RPC 2.0 requests are
//! POSTed to a remote endpoint, and responses are delivered either directly in
//! the HTTP response body (`application/json`) or as Server-Sent Events
//! (`text/event-stream`). The session id advertised by the server via the
//! `Mcp-Session-Id` header is captured on `initialize` and echoed back on every
//! subsequent request.

use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::client::{McpServerConfig, McpTool};

/// Shared JSON-RPC 2.0 request envelope.
#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Shared JSON-RPC 2.0 notification envelope (no id).
#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Shared JSON-RPC 2.0 response envelope.
#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Whether this response carries the given request id.
    pub fn id_matches(&self, id: u64) -> bool {
        match &self.id {
            Some(Value::Number(n)) => n.as_u64() == Some(id),
            Some(Value::String(s)) => s.parse::<u64>().ok() == Some(id),
            _ => false,
        }
    }

    /// Convert the response into its `result` value, surfacing any JSON-RPC error.
    pub fn into_result(self, server_name: &str) -> Result<Value> {
        if let Some(err) = self.error {
            return Err(anyhow!(
                "MCP error ({}) from '{}': {} (data: {:?})",
                err.code,
                server_name,
                err.message,
                err.data
            ));
        }
        Ok(self.result.unwrap_or(Value::Null))
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// Active connection to a remote MCP server over HTTP + SSE.
pub struct HttpSseConnection {
    server_name: String,
    endpoint: String,
    client: reqwest::Client,
    session_id: Option<String>,
    request_id: AtomicU64,
    request_timeout: Duration,
}

impl HttpSseConnection {
    /// Connect to a remote MCP server configured via `url` and run `initialize`.
    pub async fn connect(server_name: &str, cfg: &McpServerConfig) -> Result<Self> {
        let url = cfg.url.as_ref().ok_or_else(|| {
            anyhow!("missing `url` for HTTP MCP server '{server_name}'")
        })?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .with_context(|| format!("failed to build HTTP client for '{server_name}'"))?;

        let mut conn = Self {
            server_name: server_name.to_string(),
            endpoint: url.clone(),
            client,
            session_id: None,
            request_id: AtomicU64::new(1),
            request_timeout: Duration::from_secs(30),
        };

        conn.initialize().await?;
        Ok(conn)
    }

    /// POST a JSON-RPC request and await its response (from body or SSE stream).
    async fn post(&mut self, method: &str, params: Option<Value>, id: u64) -> Result<Value> {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };
        let body = serde_json::to_string(&req)?;

        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(sid) = &self.session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }

        let resp = builder
            .body(body)
            .send()
            .await
            .with_context(|| {
                format!(
                    "HTTP request to MCP server '{}' for '{method}' failed",
                    self.server_name
                )
            })?;

        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }

        let content_type = resp
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if content_type.contains("text/event-stream") {
            self.read_sse_response(resp, method, id).await
        } else {
            let status = resp.status();
            let text = resp
                .text()
                .await
                .with_context(|| format!("failed to read body from '{}' for '{method}'", self.server_name))?;
            if !status.is_success() {
                return Err(anyhow!(
                    "HTTP {status} from '{}' for '{method}': {text}",
                    self.server_name
                ));
            }
            let parsed: JsonRpcResponse = serde_json::from_str(&text).with_context(|| {
                format!(
                    "invalid JSON-RPC response from '{}' for '{method}'",
                    self.server_name
                )
            })?;
            parsed.into_result(&self.server_name)
        }
    }

    /// Read a JSON-RPC response out of a Server-Sent Events stream.
    async fn read_sse_response(
        &mut self,
        resp: reqwest::Response,
        method: &str,
        id: u64,
    ) -> Result<Value> {
        let mut stream = resp.bytes_stream().eventsource();
        loop {
            let ev = tokio::time::timeout(self.request_timeout, stream.next())
                .await
                .map_err(|_| {
                    anyhow!(
                        "timeout waiting for SSE response from '{}' for '{method}'",
                        self.server_name
                    )
                })?;
            match ev {
                Some(Ok(event)) => {
                    if let Ok(parsed) = serde_json::from_str::<JsonRpcResponse>(&event.data) {
                        if parsed.id_matches(id) {
                            return parsed.into_result(&self.server_name);
                        }
                    }
                }
                Some(Err(e)) => {
                    return Err(anyhow!(
                        "SSE error from '{}' for '{method}': {e}",
                        self.server_name
                    ));
                }
                None => {
                    return Err(anyhow!(
                        "SSE stream from '{}' closed before response for '{method}'",
                        self.server_name
                    ));
                }
            }
        }
    }

    /// POST a JSON-RPC notification (no response expected).
    async fn send_notification(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
        };
        let body = serde_json::to_string(&notif)?;

        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(sid) = &self.session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }

        let resp = builder
            .body(body)
            .send()
            .await
            .with_context(|| {
                format!(
                    "HTTP request to MCP server '{}' for notification '{method}' failed",
                    self.server_name
                )
            })?;

        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "HTTP {status} from '{}' for notification '{method}': {text}",
                self.server_name
            ));
        }
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

        let _result = self.post("initialize", Some(init_params), 0).await?;
        self.send_notification("notifications/initialized", None)
            .await?;
        Ok(())
    }

    /// Discover the tools exposed by the remote MCP server.
    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let result = self.post("tools/list", None, id).await?;

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

    /// Invoke a tool on the remote MCP server.
    pub async fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<String> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments
        });

        let result = self.post("tools/call", Some(params), id).await?;

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

    /// Shut down the connection. HTTP/SSE has no explicit shutdown handshake;
    /// the underlying connection is simply dropped.
    pub async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}
