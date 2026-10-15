//! MCP HTTP + SSE transport for remote (external) MCP servers.
//!
//! Implements the MCP "Streamable HTTP" transport: JSON-RPC 2.0 requests are
//! POSTed to a remote endpoint, and responses are delivered either directly in
//! the HTTP response body (`application/json`) or as Server-Sent Events
//! (`text/event-stream`). The session id advertised by the server via the
//! `Mcp-Session-Id` header is captured on `initialize` and echoed back on every
//! subsequent request.
//!
//! The transport shares its network plumbing with the LLM chat client
//! (`docs/recon_duplication_net.md`, clusters C3/C4/C2):
//! * the reqwest client comes from [`build_http_client_with`], fed with the
//!   transport's own [`HttpClientOptions`] by `http_client_options`;
//! * the SSE read loop is the shared pump skeleton [`pump_next`];
//! * every request runs through the shared retry policy [`retry_with_backoff`],
//!   with the crate-internal `McpError` carrying the per-transport
//!   retryability classification.
//!
//! Since cluster C7 the transport owns *no* protocol knowledge: envelopes,
//! request ids, method names, param shapes and the error/result decoding all
//! come from [`super::protocol`], which it consumes through its
//! [`JsonRpcTransport`] implementation. What is left here is the HTTP/SSE wire:
//! the client, the headers, the session id, the SSE pump and the retry policy.

use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use serde_json::Value;
use std::time::{Duration, Instant};

use super::client::{McpServerConfig, McpTool};
use super::protocol::{
    self, JsonRpcNotification, JsonRpcRequest, JsonRpcTransport, RequestIds, decode_reply,
    decode_response,
};
use crate::net::http::{HttpClientOptions, build_http_client_with};
use crate::net::{
    BACKOFF_BASE_MS, MAX_ATTEMPTS, PumpNext, RetryOp, Retryable, pump_next, retry_with_backoff,
};

/// Flat request/response deadline of the MCP transports: the per-event SSE
/// timeout of this transport and the per-line read timeout of the stdio
/// transport. Hoisted out of both hand-rolled poll loops so there is one
/// number to tune (cluster C4).
pub(crate) const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Options the MCP transport hands to the shared client builder (cluster C3).
///
/// These are exactly the client settings the transport used to hand-roll: a
/// total request timeout of [`MCP_REQUEST_TIMEOUT`] and no connect timeout.
/// Request-scoped headers (`Content-Type`, `Accept`, `Mcp-Session-Id`) are
/// intentionally *not* client options — they are set per request.
pub(crate) fn http_client_options(request_timeout: Duration) -> HttpClientOptions {
    HttpClientOptions {
        connect_timeout: None,
        total_timeout: Some(request_timeout),
    }
}

/// Whether an HTTP status is worth another attempt.
///
/// Server-side (5xx) failures plus 408 (request timeout) and 429 (rate
/// limited) — the same class the LLM client treats as transient. A 4xx other
/// than those means the server understood and rejected the request, so
/// replaying it cannot help.
pub(crate) fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Transport-level error of the MCP HTTP/SSE transport.
///
/// It exists to carry the retryability classification consumed by [`Retryable`]
/// (the shared policy in `crate::net::retry`). Every variant is converted back
/// into `anyhow::Error` at the transport's public boundary, with the same
/// message text the transport produced before, so MCP consumers are unaffected.
#[derive(Debug)]
pub(crate) enum McpError {
    /// The POST never produced response headers: DNS/connect refused/reset, or
    /// the reqwest connect/total timeout fired.
    Transport {
        server: String,
        method: String,
        source: reqwest::Error,
    },
    /// Response headers arrived but reading the body failed.
    BodyRead {
        server: String,
        method: String,
        status: reqwest::StatusCode,
        source: reqwest::Error,
    },
    /// The server answered with a non-success HTTP status.
    HttpStatus {
        server: String,
        method: String,
        status: reqwest::StatusCode,
        body: String,
    },
    /// Nothing arrived on the SSE stream before the request deadline elapsed.
    /// `saw_event` records whether this attempt had already consumed an event.
    SseTimeout {
        server: String,
        method: String,
        saw_event: bool,
    },
    /// SSE framing error while reading the response stream.
    Sse {
        server: String,
        method: String,
        reason: String,
        saw_event: bool,
    },
    /// The SSE stream ended before a response for the requested id arrived.
    SseClosed {
        server: String,
        method: String,
        saw_event: bool,
    },
    /// The response body was not parseable JSON-RPC.
    Decode {
        server: String,
        method: String,
        source: serde_json::Error,
    },
    /// Application-level JSON-RPC error (the `error` member of the envelope).
    JsonRpc {
        server: String,
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// The shared pump's abort hook stopped the read. MCP has no abort callback
    /// today; the variant exists so no pump outcome is silently swallowed.
    Aborted { server: String, method: String },
    /// The request envelope could not be serialized (local, deterministic).
    Encode(serde_json::Error),
}

impl Retryable for McpError {
    /// Retryable classes: transport/connection failures (including timeouts),
    /// retryable HTTP statuses (5xx/408/429), and SSE failures that happened
    /// *before* the stream delivered its first event.
    ///
    /// Never retried: JSON-RPC application errors, non-retryable statuses,
    /// undecodable payloads, aborted reads, local serialization failures — and
    /// any stream failure after an event was already consumed, because a
    /// partially delivered MCP response must not be replayed (a replay could
    /// execute a non-idempotent `tools/call` twice).
    fn is_retryable(&self) -> bool {
        match self {
            McpError::Transport { .. } => true,
            McpError::BodyRead { status, .. } | McpError::HttpStatus { status, .. } => {
                is_retryable_status(*status)
            }
            McpError::SseTimeout { saw_event, .. }
            | McpError::Sse { saw_event, .. }
            | McpError::SseClosed { saw_event, .. } => !*saw_event,
            McpError::Decode { .. }
            | McpError::JsonRpc { .. }
            | McpError::Aborted { .. }
            | McpError::Encode(_) => false,
        }
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Transport {
                server,
                method,
                source,
            } => write!(
                f,
                "HTTP request to MCP server '{server}' for '{method}' failed: {source}"
            ),
            McpError::BodyRead {
                server,
                method,
                source,
                ..
            } => write!(
                f,
                "failed to read body from '{server}' for '{method}': {source}"
            ),
            McpError::HttpStatus {
                server,
                method,
                status,
                body,
            } => write!(f, "HTTP {status} from '{server}' for '{method}': {body}"),
            McpError::SseTimeout { server, method, .. } => write!(
                f,
                "timeout waiting for SSE response from '{server}' for '{method}'"
            ),
            McpError::Sse {
                server,
                method,
                reason,
                ..
            } => write!(f, "SSE error from '{server}' for '{method}': {reason}"),
            McpError::SseClosed { server, method, .. } => write!(
                f,
                "SSE stream from '{server}' closed before response for '{method}'"
            ),
            McpError::Decode {
                server,
                method,
                source,
            } => write!(
                f,
                "invalid JSON-RPC response from '{server}' for '{method}': {source}"
            ),
            McpError::JsonRpc {
                server,
                code,
                message,
                data,
            } => write!(
                f,
                "MCP error ({code}) from '{server}': {message} (data: {data:?})"
            ),
            McpError::Aborted { server, method } => {
                write!(f, "MCP request to '{server}' for '{method}' aborted")
            }
            McpError::Encode(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for McpError {}

/// Active connection to a remote MCP server over HTTP + SSE.
pub struct HttpSseConnection {
    server_name: String,
    endpoint: String,
    client: reqwest::Client,
    session_id: Option<String>,
    /// The protocol layer's monotonic JSON-RPC id allocator
    /// ([`super::protocol::RequestIds`]); the transport only carries it.
    request_ids: RequestIds,
    request_timeout: Duration,
}

/// One JSON-RPC request attempt, run once per retry by [`retry_with_backoff`].
///
/// The connection is the operation's mutable receiver (it captures the session
/// id and performs the POST), while the envelope was built by
/// [`super::protocol`] and is only borrowed here — an attempt never rebuilds it,
/// so a replay resends exactly the bytes of the original request.
///
/// The shared helper's `state` is unused (`()`), mirroring how
/// `crate::llm::client::ChatAttempt` borrows `on_delta`.
struct PostAttempt<'a> {
    conn: &'a mut HttpSseConnection,
    request: &'a JsonRpcRequest,
}

impl RetryOp<()> for PostAttempt<'_> {
    type Error = McpError;
    type Output = Value;

    async fn attempt(&mut self, _state: &mut ()) -> Result<Value, McpError> {
        self.conn.post_once(self.request).await
    }
}

/// One JSON-RPC notification attempt (fire-and-forget, no response body).
struct NotifyAttempt<'a> {
    conn: &'a mut HttpSseConnection,
    notification: &'a JsonRpcNotification,
}

impl RetryOp<()> for NotifyAttempt<'_> {
    type Error = McpError;
    type Output = ();

    async fn attempt(&mut self, _state: &mut ()) -> Result<(), McpError> {
        self.conn.send_notification_once(self.notification).await
    }
}

/// The raw send/receive half of the [`super::protocol`] seam for this transport.
///
/// The transport supplies the HTTP/SSE wire and nothing else: the envelope, the
/// request id, the method name, the param shape and the reply decoding are the
/// protocol layer's business, so `initialize`/`list_tools`/`call_tool` are the
/// shared generic operations from [`super::protocol`].
impl JsonRpcTransport for HttpSseConnection {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn request_ids(&self) -> &RequestIds {
        &self.request_ids
    }

    /// POST a JSON-RPC request envelope and await its response (from the JSON
    /// body or from an SSE stream).
    ///
    /// The wire-level attempt ([`Self::post_once`]) runs through the shared
    /// retry policy in `crate::net::retry` (cluster C2): connection failures,
    /// reqwest timeouts, 5xx/408/429 statuses and stream failures that happen
    /// before the first SSE event are retried with the shared linear backoff.
    /// Anything else — 4xx statuses, JSON-RPC application errors, undecodable
    /// payloads — fails immediately, as does any stream failure once an event
    /// has already been consumed (no partial-stream replay).
    async fn send_request(&mut self, request: &JsonRpcRequest) -> Result<Value> {
        let tag = format!(
            "MCP call to '{}' for '{}'",
            self.server_name, request.method
        );
        let mut state = ();
        let value = retry_with_backoff(
            MAX_ATTEMPTS,
            BACKOFF_BASE_MS,
            &tag,
            &mut state,
            PostAttempt {
                conn: self,
                request,
            },
        )
        .await?;
        Ok(value)
    }

    /// POST a JSON-RPC notification envelope (no response expected), retried
    /// through the shared policy like every other MCP request.
    async fn send_notification(&mut self, notification: &JsonRpcNotification) -> Result<()> {
        let tag = format!(
            "MCP notification to '{}' for '{}'",
            self.server_name, notification.method
        );
        let mut state = ();
        retry_with_backoff(
            MAX_ATTEMPTS,
            BACKOFF_BASE_MS,
            &tag,
            &mut state,
            NotifyAttempt {
                conn: self,
                notification,
            },
        )
        .await?;
        Ok(())
    }
}

impl HttpSseConnection {
    /// Connect to a remote MCP server configured via `url` and run `initialize`.
    pub async fn connect(server_name: &str, cfg: &McpServerConfig) -> Result<Self> {
        let url = cfg
            .url
            .as_ref()
            .ok_or_else(|| anyhow!("missing `url` for HTTP MCP server '{server_name}'"))?;

        Self::establish(server_name, url, MCP_REQUEST_TIMEOUT).await
    }

    /// Test seam: same handshake with an explicit request deadline, so the
    /// shared builder's total timeout and the retry schedule can be exercised
    /// without waiting 30 s per attempt.
    #[cfg(test)]
    pub(crate) async fn connect_with_timeout(
        server_name: &str,
        url: &str,
        request_timeout: Duration,
    ) -> Result<Self> {
        Self::establish(server_name, url, request_timeout).await
    }

    /// Build the reqwest client through the shared builder (cluster C3) and run
    /// the `initialize` handshake through the shared protocol layer.
    async fn establish(server_name: &str, url: &str, request_timeout: Duration) -> Result<Self> {
        let client = build_http_client_with(http_client_options(request_timeout))
            .with_context(|| format!("failed to build HTTP client for '{server_name}'"))?;

        let mut conn = Self {
            server_name: server_name.to_string(),
            endpoint: url.to_string(),
            client,
            session_id: None,
            request_ids: RequestIds::new(),
            request_timeout,
        };

        protocol::initialize(&mut conn).await?;
        Ok(conn)
    }

    /// One wire-level POST: send the protocol-built envelope, then read the
    /// response either from the JSON body or from an SSE stream.
    async fn post_once(&mut self, request: &JsonRpcRequest) -> Result<Value, McpError> {
        let body = request.body().map_err(McpError::Encode)?;

        let resp =
            self.post_builder()
                .body(body)
                .send()
                .await
                .map_err(|e| McpError::Transport {
                    server: self.server_name.clone(),
                    method: request.method.clone(),
                    source: e,
                })?;

        self.capture_session_id(&resp);

        let content_type = resp
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if content_type.contains("text/event-stream") {
            self.read_sse_response(resp, request).await
        } else {
            let status = resp.status();
            let text = resp.text().await.map_err(|e| McpError::BodyRead {
                server: self.server_name.clone(),
                method: request.method.clone(),
                status,
                source: e,
            })?;
            if !status.is_success() {
                return Err(McpError::HttpStatus {
                    server: self.server_name.clone(),
                    method: request.method.clone(),
                    status,
                    body: text,
                });
            }
            let parsed = decode_response(&text).map_err(|e| McpError::Decode {
                server: self.server_name.clone(),
                method: request.method.clone(),
                source: e,
            })?;
            parsed.into_mcp_result(&self.server_name)
        }
    }

    /// The POST every MCP envelope travels on: the request-scoped headers plus
    /// the session id echo. Requests and notifications share it (cluster C7
    /// removed the second, hand-copied version of these headers).
    fn post_builder(&self) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(sid) = &self.session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }
        builder
    }

    /// Remember the session id the server advertises so it is echoed back on
    /// every subsequent request.
    fn capture_session_id(&mut self, resp: &reqwest::Response) {
        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }
    }

    /// Read a JSON-RPC response out of a Server-Sent Events stream using the
    /// shared pump skeleton (cluster C4).
    ///
    /// The deadline is recomputed per event, which keeps the transport's
    /// existing flat *per-event* request timeout (`tokio::time::timeout
    /// (self.request_timeout, stream.next())` was exactly this). `saw_event`
    /// gates replayability: once an event has been consumed the request is never
    /// resent. Which event answers this request (and which is somebody else's)
    /// is decided by the protocol layer's [`decode_reply`].
    async fn read_sse_response(
        &mut self,
        resp: reqwest::Response,
        request: &JsonRpcRequest,
    ) -> Result<Value, McpError> {
        let mut stream = resp.bytes_stream().eventsource();
        let mut saw_event = false;
        loop {
            let deadline = Instant::now() + self.request_timeout;
            match pump_next(&mut stream, Some(deadline), &mut |_| true).await {
                PumpNext::Item(Some(Ok(event))) => {
                    saw_event = true;
                    if let Some(parsed) = decode_reply(&event.data, request.id) {
                        return parsed.into_mcp_result(&self.server_name);
                    }
                }
                PumpNext::Item(Some(Err(e))) => {
                    return Err(McpError::Sse {
                        server: self.server_name.clone(),
                        method: request.method.clone(),
                        reason: e.to_string(),
                        saw_event,
                    });
                }
                PumpNext::Item(None) => {
                    return Err(McpError::SseClosed {
                        server: self.server_name.clone(),
                        method: request.method.clone(),
                        saw_event,
                    });
                }
                PumpNext::IdleTimeout => {
                    return Err(McpError::SseTimeout {
                        server: self.server_name.clone(),
                        method: request.method.clone(),
                        saw_event,
                    });
                }
                PumpNext::Aborted => {
                    return Err(McpError::Aborted {
                        server: self.server_name.clone(),
                        method: request.method.clone(),
                    });
                }
            }
        }
    }

    /// One wire-level notification POST.
    async fn send_notification_once(
        &mut self,
        notification: &JsonRpcNotification,
    ) -> Result<(), McpError> {
        let body = notification.body().map_err(McpError::Encode)?;

        let resp =
            self.post_builder()
                .body(body)
                .send()
                .await
                .map_err(|e| McpError::Transport {
                    server: self.server_name.clone(),
                    method: notification.method.clone(),
                    source: e,
                })?;

        self.capture_session_id(&resp);

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(McpError::HttpStatus {
                server: self.server_name.clone(),
                method: notification.method.clone(),
                status,
                body: text,
            });
        }
        Ok(())
    }

    /// Discover the tools exposed by the remote MCP server. The envelope, the
    /// request id and the decoding come from [`super::protocol`].
    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        protocol::list_tools(self).await
    }

    /// Invoke a tool on the remote MCP server.
    pub async fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<String> {
        protocol::call_tool(self, name, arguments).await
    }

    /// Shut down the connection. HTTP/SSE has no explicit shutdown handshake;
    /// the underlying connection is simply dropped.
    pub async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}
