//! The single MCP JSON-RPC 2.0 protocol layer, shared by both transports.
//!
//! Extracted from `src/mcp/client.rs` (stdio) and `src/mcp/http.rs` (HTTP + SSE)
//! per `docs/recon_duplication_net.md` §2 clusters **C7** (the two transports
//! carried their own copies of the request envelopes, the id allocator, the
//! method-name literals, the `initialize` handshake and the `tools/list` /
//! `tools/call` result parsing) and **C8** (the MCP test file carried a *third*
//! copy of the result parsing, already drifted from production).
//!
//! Everything protocol-shaped lives here exactly once:
//! * the envelope types and their wire form — [`JsonRpcRequest`],
//!   [`JsonRpcNotification`], [`JsonRpcResponse`], [`JsonRpcError`];
//! * the monotonic JSON-RPC id allocator [`RequestIds`];
//! * the method-name and protocol-version constants;
//! * the param shapes ([`initialize_params`], [`call_tool_params`]);
//! * response id matching / skipping of unrelated events ([`decode_reply`],
//!   [`JsonRpcResponse::id_matches`]);
//! * the error-envelope (`{"error":{code,message,data}}`) decoding
//!   ([`JsonRpcResponse::into_mcp_result`] → [`McpError::JsonRpc`]);
//! * the result decoding into the crate's own types ([`parse_tools`],
//!   [`render_call_tool_result`]).
//!
//! A transport supplies nothing but raw send/receive by implementing
//! [`JsonRpcTransport`]; the operations themselves ([`initialize`],
//! [`list_tools`], [`call_tool`]) are generic over that seam, so both
//! transports emit byte-identical protocol traffic:
//!
//! ```text
//! {"jsonrpc":"2.0","id":0,"method":"initialize","params":{...}}   <- handshake, reserved id
//! {"jsonrpc":"2.0","method":"notifications/initialized"}          <- notification, no id
//! {"jsonrpc":"2.0","id":1,"method":"tools/list"}                  <- allocator from FIRST_REQUEST_ID
//! ```
//!
//! The `initialize` envelope carries the reserved [`HANDSHAKE_REQUEST_ID`] and
//! therefore does *not* consume an id from the allocator. That is the HTTP/SSE
//! transport's pre-existing behaviour (it POSTed `id: 0` for the handshake) and
//! it is what lets both transports agree on the bytes of the handshake
//! envelope; the stdio transport's numeric ids shift by one as a result, which
//! is invisible on the wire — ids are opaque to the server and the client only
//! matches replies against them ([`JsonRpcResponse::id_matches`]).
//!
//! Layering note: the transport error type [`McpError`] stays where it is (it
//! owns the retryability classification consumed by [`crate::net::Retryable`],
//! including the reqwest-specific variants the stdio transport cannot have).
//! This module decodes *into* it so there is exactly one error-envelope
//! decoder, and no transport repeats the decoding.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

use super::client::McpTool;
use super::http::McpError;

/// JSON-RPC protocol version carried in every envelope of both transports.
pub(crate) const JSONRPC_VERSION: &str = "2.0";

/// MCP specification version both transports handshake with.
pub(crate) const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// `clientInfo.name` advertised to MCP servers.
pub(crate) const CLIENT_INFO_NAME: &str = "marmel";

// ── Method names (single owner; transports must never spell them out) ──────

/// Handshake request.
pub(crate) const METHOD_INITIALIZE: &str = "initialize";
/// Post-handshake notification announcing the client is ready.
pub(crate) const METHOD_INITIALIZED: &str = "notifications/initialized";
/// Tool discovery.
pub(crate) const METHOD_TOOLS_LIST: &str = "tools/list";
/// Tool invocation.
pub(crate) const METHOD_TOOLS_CALL: &str = "tools/call";

// ── Request ids ────────────────────────────────────────────────────────────

/// Reserved id of the `initialize` handshake request.
///
/// The handshake precedes the allocator, so it is addressed with a fixed id
/// and every call a transport makes afterwards gets a fresh id from
/// [`FIRST_REQUEST_ID`].
pub(crate) const HANDSHAKE_REQUEST_ID: u64 = 0;
/// First id handed out by [`RequestIds::next`].
pub(crate) const FIRST_REQUEST_ID: u64 = 1;

/// Monotonically increasing JSON-RPC request id allocator.
///
/// One allocator per live connection (mirroring the `AtomicU64` each transport
/// used to carry itself). Never reused, never reset: a reply is matched to its
/// request by id, so reusing an id could pair a stale event with a new call.
#[derive(Debug)]
pub(crate) struct RequestIds {
    next: AtomicU64,
}

impl RequestIds {
    /// An allocator whose first id is [`FIRST_REQUEST_ID`].
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(FIRST_REQUEST_ID),
        }
    }

    /// The next request id.
    pub fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }
}

impl Default for RequestIds {
    fn default() -> Self {
        Self::new()
    }
}

// ── Envelopes ──────────────────────────────────────────────────────────────

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Build the envelope for `method`. Field order and the omitted-`params`
    /// rule are what the wire sees, so they are pinned here once.
    pub fn new(id: u64, method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            method: method.to_string(),
            params,
        }
    }

    /// The `initialize` handshake envelope (reserved id, shared by both
    /// transports).
    pub fn initialize() -> Self {
        Self::new(
            HANDSHAKE_REQUEST_ID,
            METHOD_INITIALIZE,
            Some(initialize_params()),
        )
    }

    /// A `tools/list` request.
    pub fn tools_list(id: u64) -> Self {
        Self::new(id, METHOD_TOOLS_LIST, None)
    }

    /// A `tools/call` request.
    pub fn tools_call(id: u64, name: &str, arguments: &Value) -> Self {
        Self::new(
            id,
            METHOD_TOOLS_CALL,
            Some(call_tool_params(name, arguments)),
        )
    }

    /// The compact one-line JSON body a transport puts on the wire.
    pub fn body(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// The params as the transports' instrumentation sees them.
    pub fn params_ref(&self) -> Option<&Value> {
        self.params.as_ref()
    }
}

/// JSON-RPC 2.0 notification envelope (never carries an id).
#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    pub fn new(method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method: method.to_string(),
            params,
        }
    }

    /// The `notifications/initialized` notification sent after the handshake.
    pub fn initialized() -> Self {
        Self::new(METHOD_INITIALIZED, None)
    }

    /// The compact one-line JSON body a transport puts on the wire.
    pub fn body(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// JSON-RPC 2.0 response envelope.
///
/// The `jsonrpc` version member is deliberately *not* a field: nothing in either
/// transport validates it, and a stored-but-unread field is dead weight that a
/// waiver would only hide. Serde ignores it on the wire, so an envelope without
/// a `jsonrpc` member (or with an unexpected one) decodes exactly as before.
#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcResponse {
    pub id: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Whether this response carries the given request id.
    ///
    /// Servers are allowed to answer with a stringified id, and streams carry
    /// events for other (or no) ids at all — both are handled here so no
    /// transport has its own version of the rule.
    pub fn id_matches(&self, id: u64) -> bool {
        match &self.id {
            Some(Value::Number(n)) => n.as_u64() == Some(id),
            Some(Value::String(s)) => s.parse::<u64>().ok() == Some(id),
            _ => false,
        }
    }

    /// Convert the response into its `result` value, surfacing any JSON-RPC
    /// error. Anyhow-shaped wrapper (used by the stdio transport) over
    /// [`Self::into_mcp_result`].
    pub fn into_result(self, server_name: &str) -> Result<Value> {
        self.into_mcp_result(server_name)
            .map_err(anyhow::Error::from)
    }

    /// Typed form of [`Self::into_result`] carrying the retryability class.
    /// An application-level JSON-RPC error is always immediate: the server
    /// processed the request and said no.
    pub(crate) fn into_mcp_result(self, server_name: &str) -> Result<Value, McpError> {
        if let Some(err) = self.error {
            return Err(McpError::JsonRpc {
                server: server_name.to_string(),
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }
        Ok(self.result.unwrap_or(Value::Null))
    }
}

/// The `error` member of a JSON-RPC error envelope.
#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// Parse one raw payload into a response envelope (used where the transport
/// knows the payload *is* the answer, e.g. a JSON HTTP response body).
pub(crate) fn decode_response(raw: &str) -> Result<JsonRpcResponse, serde_json::Error> {
    serde_json::from_str(raw)
}

/// Decode one raw payload from a stream (SSE event data, a stdout line) into the
/// reply for `id`.
///
/// `None` means "not an answer for this request": either the payload is not
/// JSON-RPC at all, or it belongs to another id. Both transports use this so the
/// skip-unrelated-events rule exists once, and neither can accidentally act on
/// somebody else's event.
pub(crate) fn decode_reply(raw: &str, id: u64) -> Option<JsonRpcResponse> {
    let parsed = decode_response(raw).ok()?;
    parsed.id_matches(id).then_some(parsed)
}

// ── Param shapes ───────────────────────────────────────────────────────────

/// The `params` of the `initialize` request: identical for both transports
/// (they used to spell this literal out twice).
pub(crate) fn initialize_params() -> Value {
    serde_json::json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "clientInfo": {
            "name": CLIENT_INFO_NAME,
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

/// The `params` of a `tools/call` request.
pub(crate) fn call_tool_params(name: &str, arguments: &Value) -> Value {
    serde_json::json!({
        "name": name,
        "arguments": arguments
    })
}

/// The fallback JSON Schema for a tool that reports no `inputSchema`.
fn default_input_schema() -> Value {
    serde_json::json!({"type": "object"})
}

// ── Result decoding ────────────────────────────────────────────────────────

/// Decode a `tools/list` result into the crate's tool list.
///
/// A tool entry without a name is skipped; `description` is optional; a missing
/// `inputSchema` falls back to an empty object schema. An `inputSchema` that is
/// present but `null` stays `null` — production has always passed it through,
/// and the C8 test copy that filtered it out was wrong (that is the drift the
/// recon flagged at `src/mcp/http_tests.rs:110` vs `src/mcp/http.rs:304`).
pub(crate) fn parse_tools(result: &Value, server_name: &str) -> Vec<McpTool> {
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
                    .unwrap_or_else(default_input_schema);
                tools.push(McpTool {
                    name: name.to_string(),
                    description,
                    input_schema,
                    server_name: server_name.to_string(),
                });
            }
        }
    }
    tools
}

/// Render a `tools/call` result into the text handed back to the model.
///
/// `content[]` items contribute their `text` (non-text items are dumped as JSON,
/// one line each); a result without `content` falls back to a `text` field and
/// then to the whole result. `isError: true` turns the rendered text into an
/// error, unchanged from what both transports did.
pub(crate) fn render_call_tool_result(result: &Value) -> Result<String> {
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

// ── Transport seam ─────────────────────────────────────────────────────────

/// What a transport must supply: raw send/receive of an envelope that this
/// module has already built, plus its server name and id allocator.
///
/// The two send operations return `impl Future` explicitly instead of being
/// `async fn`s, for the same reason as [`crate::net::retry::RetryOp`]: the
/// returned future has to borrow `self` (the live connection) *and* the borrowed
/// envelope for the whole attempt — an HTTP/SSE attempt replays the exact bytes
/// of `request` on every retry — and spelling the return type out makes that
/// capture part of the contract instead of an `async_fn_in_trait` waiver. The
/// seam is implemented and consumed only inside this crate (`mcp::client`,
/// `mcp::http`, and the protocol tests), so it never needs `dyn` compatibility;
/// implementors may still write the `async fn` form, which lowers to the same
/// hidden future.
pub(crate) trait JsonRpcTransport {
    /// Configured server name: appears in error messages and on discovered tools.
    fn server_name(&self) -> &str;

    /// The connection's protocol-owned id allocator.
    fn request_ids(&self) -> &RequestIds;

    /// Put `request` on the wire and return the `result` member of the reply
    /// (a JSON-RPC `error` envelope becomes an error here, with the transport's
    /// own error class — the message text is the shared one from
    /// [`McpError`]).
    fn send_request(
        &mut self,
        request: &JsonRpcRequest,
    ) -> impl std::future::Future<Output = Result<Value>>;

    /// Put `notification` on the wire. No reply is expected.
    fn send_notification(
        &mut self,
        notification: &JsonRpcNotification,
    ) -> impl std::future::Future<Output = Result<()>>;
}

// ── The protocol operations, written once ─────────────────────────────────

/// Run the MCP handshake: `initialize` (reserved id) followed by
/// `notifications/initialized`.
pub(crate) async fn initialize<T: JsonRpcTransport>(transport: &mut T) -> Result<()> {
    let _result = transport
        .send_request(&JsonRpcRequest::initialize())
        .await?;
    transport
        .send_notification(&JsonRpcNotification::initialized())
        .await
}

/// `tools/list`: allocate an id, send the envelope, decode the tool list.
pub(crate) async fn list_tools<T: JsonRpcTransport>(transport: &mut T) -> Result<Vec<McpTool>> {
    let request = JsonRpcRequest::tools_list(transport.request_ids().next());
    let result = transport.send_request(&request).await?;
    Ok(parse_tools(&result, transport.server_name()))
}

/// `tools/call`: allocate an id, send the envelope, render the reply text.
pub(crate) async fn call_tool<T: JsonRpcTransport>(
    transport: &mut T,
    name: &str,
    arguments: &Value,
) -> Result<String> {
    let request = JsonRpcRequest::tools_call(transport.request_ids().next(), name, arguments);
    let result = transport.send_request(&request).await?;
    render_call_tool_result(&result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// In-memory transport: records the exact bytes of every envelope the
    /// protocol layer builds and answers with a canned result. Proves the
    /// protocol layer — not a transport — owns envelopes, ids, method names and
    /// decoding, and gives the generic operations something to run on.
    struct RecordingTransport {
        envelopes: Vec<String>,
        request_ids: RequestIds,
        reply: Value,
    }

    impl RecordingTransport {
        fn new(reply: Value) -> Self {
            Self {
                envelopes: Vec::new(),
                request_ids: RequestIds::new(),
                reply,
            }
        }
    }

    impl JsonRpcTransport for RecordingTransport {
        fn server_name(&self) -> &str {
            "recorder"
        }

        fn request_ids(&self) -> &RequestIds {
            &self.request_ids
        }

        async fn send_request(&mut self, request: &JsonRpcRequest) -> Result<Value> {
            self.envelopes.push(request.body()?);
            Ok(self.reply.clone())
        }

        async fn send_notification(&mut self, notification: &JsonRpcNotification) -> Result<()> {
            self.envelopes.push(notification.body()?);
            Ok(())
        }
    }

    /// The exact envelope text both transports must put on the wire, spelled out
    /// here so a change to the protocol layer cannot slip through unnoticed.
    fn canonical_initialize_body() -> String {
        format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",",
                "\"params\":{{\"capabilities\":{{\"tools\":{{}}}},",
                "\"clientInfo\":{{\"name\":\"marmel\",\"version\":\"{}\"}},",
                "\"protocolVersion\":\"2024-11-05\"}}}}"
            ),
            env!("CARGO_PKG_VERSION")
        )
    }

    #[test]
    fn both_transports_share_one_initialize_envelope() {
        // The handshake envelope is built by the protocol layer alone, so the
        // stdio and the HTTP/SSE transport cannot disagree about it: they both
        // serialize exactly this value (asserted on the real wires by
        // `mcp::client`'s and `mcp::http`'s own wire-capture tests).
        let envelope = JsonRpcRequest::initialize();
        assert_eq!(envelope.id, HANDSHAKE_REQUEST_ID);
        assert_eq!(envelope.method, METHOD_INITIALIZE);
        assert_eq!(
            envelope.body().expect("envelope must serialize"),
            canonical_initialize_body()
        );

        // The params keep the capability fields the transports always sent.
        assert_eq!(
            initialize_params(),
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "clientInfo": {
                    "name": "marmel",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })
        );
    }

    #[tokio::test]
    async fn handshake_envelopes_and_id_policy() {
        let mut transport = RecordingTransport::new(json!({"protocolVersion": "2024-11-05"}));
        initialize(&mut transport).await.expect("handshake");

        assert_eq!(transport.envelopes.len(), 2);
        assert_eq!(transport.envelopes[0], canonical_initialize_body());
        // A notification never carries an id.
        assert_eq!(
            transport.envelopes[1],
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        );

        // The handshake did not consume an id: the next call starts at
        // FIRST_REQUEST_ID on every transport.
        let mut list = JsonRpcRequest::tools_list(transport.request_ids().next());
        assert_eq!(
            list.body().expect("envelope must serialize"),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
        );
        list =
            JsonRpcRequest::tools_call(transport.request_ids().next(), "alpha", &json!({"a": 1}));
        assert_eq!(
            list.body().expect("envelope must serialize"),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"arguments":{"a":1},"name":"alpha"}}"#
        );
    }

    #[test]
    fn request_ids_are_monotonic_and_never_repeat() {
        let ids = RequestIds::new();
        let got: Vec<u64> = (0..4).map(|_| ids.next()).collect();
        assert_eq!(got, vec![1, 2, 3, 4]);
        assert!(!got.contains(&HANDSHAKE_REQUEST_ID));
    }

    #[test]
    fn json_rpc_error_envelope_decodes_into_the_mcp_error_variant() {
        let raw = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"Method not found","data":{"detail":"oops"}}}"#;
        let parsed = decode_response(raw).expect("error envelope must decode");
        assert!(parsed.id_matches(7));
        assert!(!parsed.id_matches(8));

        let err = parsed
            .into_mcp_result("test-server")
            .expect_err("an error envelope is not a result");
        match &err {
            McpError::JsonRpc {
                server,
                code,
                message,
                data,
            } => {
                assert_eq!(server, "test-server");
                assert_eq!(*code, -32601);
                assert_eq!(message, "Method not found");
                assert_eq!(*data, Some(json!({"detail": "oops"})));
            }
            other => panic!("expected the JsonRpc variant, got {other:?}"),
        }
        // The text surfaced to MCP consumers is unchanged.
        let text = err.to_string();
        assert!(text.contains("-32601"), "{text}");
        assert!(text.contains("Method not found"), "{text}");
        assert!(text.contains("test-server"), "{text}");
    }

    #[test]
    fn a_result_envelope_decodes_into_its_result_value() {
        let parsed =
            decode_response(r#"{"jsonrpc":"2.0","id":1,"result":{"foo":"bar"}}"#).expect("decodes");
        assert_eq!(
            parsed.into_result("test-server").expect("result"),
            json!({"foo": "bar"})
        );

        // A bare envelope without a result member is a null result.
        let bare = decode_response(r#"{"jsonrpc":"2.0","id":1}"#).expect("decodes");
        assert_eq!(
            bare.into_result("test-server").expect("result"),
            Value::Null
        );
    }

    #[test]
    fn response_envelope_decoding_ignores_the_jsonrpc_member() {
        // The response envelope has no `jsonrpc` field (nothing consumed it), so
        // the member is irrelevant to decoding in every shape servers emit:
        // present, wrong, or missing.
        let with_version =
            decode_response(r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#).expect("decodes");
        let odd_version =
            decode_response(r#"{"jsonrpc":"1.0","id":3,"result":{"ok":true}}"#).expect("decodes");
        let without_version = decode_response(r#"{"id":3,"result":{"ok":true}}"#).expect("decodes");

        for parsed in [with_version, odd_version, without_version] {
            assert!(parsed.id_matches(3));
            assert_eq!(
                parsed.into_result("test-server").expect("result"),
                json!({"ok": true})
            );
        }

        // An error envelope keeps decoding the same way whichever shape it
        // arrives in.
        let err = decode_response(r#"{"id":3,"error":{"code":-1,"message":"nope"}}"#)
            .expect("decodes")
            .into_mcp_result("test-server")
            .expect_err("error envelope");
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn decode_reply_skips_unrelated_and_unparseable_payloads() {
        // Unrelated id → no reply (the pending call keeps waiting).
        assert!(decode_reply(r#"{"jsonrpc":"2.0","id":99,"result":{"tools":[]}}"#, 1).is_none());
        // Notification echo (no id at all) → no reply.
        assert!(
            decode_reply(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                1
            )
            .is_none()
        );
        // Not JSON-RPC → no reply.
        assert!(decode_reply("server: listening on 0.0.0.0", 1).is_none());
        // Stringified ids still match.
        assert!(decode_reply(r#"{"jsonrpc":"2.0","id":"1","result":{}}"#, 1).is_some());
        // The matching payload resolves.
        let reply = decode_reply(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#, 1)
            .expect("the matching reply resolves");
        assert_eq!(
            reply.into_result("srv").expect("result"),
            json!({"ok": true})
        );
    }

    #[test]
    fn tools_list_result_decodes_into_the_tool_list_type() {
        let result = json!({
            "tools": [
                {
                    "name": "tool1",
                    "description": "desc1",
                    "inputSchema": {"type": "object", "properties": {"a": {"type": "string"}}}
                },
                { "name": "tool2" },
                { "name": "tool3", "inputSchema": null },
                { "description": "nameless entries are skipped" }
            ]
        });

        let tools = parse_tools(&result, "test-server");
        assert_eq!(tools.len(), 3);

        assert_eq!(tools[0].name, "tool1");
        assert_eq!(tools[0].description, Some("desc1".to_string()));
        assert_eq!(
            tools[0].input_schema,
            json!({"type": "object", "properties": {"a": {"type": "string"}}})
        );

        // Missing schema → the default object schema.
        assert_eq!(tools[1].name, "tool2");
        assert_eq!(tools[1].description, None);
        assert_eq!(tools[1].input_schema, json!({"type": "object"}));

        // Production passes an explicit null through (the old test copy did not).
        assert_eq!(tools[2].name, "tool3");
        assert_eq!(tools[2].input_schema, Value::Null);

        // Every tool is stamped with the server it came from.
        assert!(tools.iter().all(|t| t.server_name == "test-server"));
        assert_eq!(tools[0].qualified_name(), "test-server__tool1");

        // A result without a `tools` array is an empty tool list, not an error.
        assert!(parse_tools(&json!({}), "test-server").is_empty());
        assert!(parse_tools(&Value::Null, "test-server").is_empty());
    }

    #[test]
    fn call_tool_result_content_decodes_into_reply_text() {
        // content[] items are concatenated, one per line.
        let text = render_call_tool_result(&json!({
            "content": [
                {"type": "text", "text": "Hello "},
                {"type": "text", "text": "World"}
            ],
            "isError": false
        }))
        .expect("not an error");
        assert_eq!(text, "Hello \nWorld");

        // A non-text content item is dumped as JSON.
        let text = render_call_tool_result(&json!({
            "content": [{"type": "image", "data": "base64"}]
        }))
        .expect("not an error");
        assert_eq!(text, r#"{"data":"base64","type":"image"}"#);

        // No content array → `text` field, then the whole result.
        assert_eq!(
            render_call_tool_result(&json!({"text": "Simple response"})).expect("text field"),
            "Simple response"
        );
        assert_eq!(
            render_call_tool_result(&json!({"odd": true})).expect("whole result"),
            r#"{"odd":true}"#
        );
        assert_eq!(
            render_call_tool_result(&Value::Null).expect("null result"),
            ""
        );

        // `isError` turns the rendered text into an error at the caller boundary.
        let err = render_call_tool_result(&json!({
            "content": [{"text": "Critical failure"}],
            "isError": true
        }))
        .expect_err("isError must surface as an error");
        assert_eq!(err.to_string(), "Critical failure");
    }

    #[tokio::test]
    async fn generic_operations_drive_the_transport_seam() {
        let mut transport = RecordingTransport::new(json!({
            "tools": [{"name": "alpha", "description": "mock tool"}]
        }));
        let tools = list_tools(&mut transport).await.expect("tools/list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "alpha");
        assert_eq!(tools[0].server_name, "recorder");
        assert_eq!(
            transport.envelopes.last().expect("one request"),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
        );

        transport.reply = json!({"content": [{"text": "done"}]});
        let reply = call_tool(&mut transport, "alpha", &json!({"city": "Oslo"}))
            .await
            .expect("tools/call");
        assert_eq!(reply, "done");
        assert_eq!(
            transport.envelopes.last().expect("one request"),
            concat!(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","#,
                r#""params":{"arguments":{"city":"Oslo"},"name":"alpha"}}"#
            )
        );
    }
}
