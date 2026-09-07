use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, error, info};

use crate::ketch::KetchClient;
use crate::models::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::searchwala::SearchWalaClient;

pub struct McpServer {
    searchwala: SearchWalaClient,
    ketch: KetchClient,
}

impl McpServer {
    pub fn new(searchwala: SearchWalaClient, ketch: KetchClient) -> Self {
        Self { searchwala, ketch }
    }

    /// Run the MCP server over an async reader and writer (typically stdin and stdout).
    pub async fn run<R, W>(&self, mut reader: R, mut writer: W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        info!("MCP Deep Research server started");
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                info!("MCP client closed input stream; terminating server");
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            debug!(line = %trimmed, "Incoming MCP message");
            let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(e) => {
                    error!(error = %e, raw = %trimmed, "Failed to parse JSON-RPC request");
                    continue;
                }
            };

            if let Some(resp) = self.handle_request(req).await {
                let mut resp_str = serde_json::to_string(&resp)?;
                resp_str.push('\n');
                writer.write_all(resp_str.as_bytes()).await?;
                writer.flush().await?;
            }
        }

        Ok(())
    }

    pub async fn handle_request(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let id = req.id.clone();

        match req.method.as_str() {
            "initialize" => {
                let resp_id = id.unwrap_or(serde_json::json!(1));
                Some(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: resp_id,
                    result: Some(serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {
                            "tools": {}
                        },
                        "serverInfo": {
                            "name": "mcp-deep-research",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    })),
                    error: None,
                })
            }
            "notifications/initialized" => {
                // MCP notification: no response needed
                None
            }
            "tools/list" => {
                let resp_id = id.unwrap_or(serde_json::json!(1));
                Some(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: resp_id,
                    result: Some(serde_json::json!({
                        "tools": self.tool_definitions()
                    })),
                    error: None,
                })
            }
            "tools/call" => {
                let resp_id = id.unwrap_or(serde_json::json!(1));
                let params = req.params.unwrap_or(Value::Null);
                let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));

                let (text, is_err) = match self.dispatch_tool(tool_name, &arguments).await {
                    Ok(out) => (out, false),
                    Err(e) => (e.to_string(), true),
                };

                Some(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: resp_id,
                    result: Some(serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": text
                            }
                        ],
                        "isError": is_err
                    })),
                    error: None,
                })
            }
            "ping" => {
                let resp_id = id.unwrap_or(serde_json::json!(1));
                Some(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: resp_id,
                    result: Some(serde_json::json!({})),
                    error: None,
                })
            }
            unknown => id.map(|resp_id| JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: resp_id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {unknown}"),
                    data: None,
                }),
            }),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        vec![
            serde_json::json!({
                "name": "deep_research",
                "description": "Execute an iterative, multi-source Deep Research workflow across 90+ search engines via SearchWala. Collects, extracts, and iteratively synthesizes findings into a structured research report with preserved source URLs and numbered citations.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The research query or decomposed research question"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Max number of sources to explore and scrape (default: 50)"
                        },
                        "focus_mode": {
                            "type": "string",
                            "description": "Research focus mode: 'research', 'tech', 'science', 'academic', 'finance', 'news'"
                        }
                    },
                    "required": ["query"]
                }
            }),
            serde_json::json!({
                "name": "search",
                "description": "Execute a broad meta-search query across 90+ search engines in parallel via SearchWala. Returns ranked results with titles, snippets, source URLs, and engine consensus.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query string"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Max number of results to return (default: 20)"
                        },
                        "focus_mode": {
                            "type": "string",
                            "description": "Optional focus mode: 'auto', 'tech', 'science', 'academic', 'news'"
                        }
                    },
                    "required": ["query"]
                }
            }),
            serde_json::json!({
                "name": "ketch_docs",
                "description": "Query authoritative, version-aware software library and framework documentation via Context7 using ketch. Retrieve official API signatures, guides, and implementation examples.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The documentation query or API symbol to search"
                        },
                        "library": {
                            "type": "string",
                            "description": "Context7 library identifier (e.g. '/charmbracelet/glamour', 'tokio', 'axum')"
                        },
                        "tokens": {
                            "type": "integer",
                            "description": "Context7 token budget for documentation snippets (default: 4000)"
                        }
                    },
                    "required": ["query"]
                }
            }),
        ]
    }

    async fn dispatch_tool(&self, name: &str, args: &Value) -> Result<String> {
        match name {
            "deep_research" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("'query' parameter is required"))?;
                let max_results = args
                    .get("max_results")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
                let focus_mode = args.get("focus_mode").and_then(Value::as_str);

                self.searchwala
                    .deep_research(query, max_results, focus_mode, None)
                    .await
            }
            "search" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("'query' parameter is required"))?;
                let max_results = args
                    .get("max_results")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
                let focus_mode = args.get("focus_mode").and_then(Value::as_str);

                self.searchwala.search(query, max_results, focus_mode).await
            }
            "ketch_docs" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("'query' parameter is required"))?;
                let library = args.get("library").and_then(Value::as_str);
                let tokens = args.get("tokens").and_then(Value::as_u64).map(|n| n as u32);

                self.ketch.docs(query, library, tokens).await
            }
            unknown => Err(anyhow::anyhow!("Unknown tool: {unknown}")),
        }
    }
}
