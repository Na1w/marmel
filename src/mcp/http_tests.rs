//! Tests for the MCP HTTP/SSE transport and the protocol layer it consumes.
//!
//! Two families:
//! * the protocol guards call the *production* code in `crate::mcp::protocol`
//!   (envelope bytes, error-envelope decoding, id matching, result decoding) and
//!   assert what actually travels on the wire. They replace the old copies of the
//!   production parsing bodies, which had already drifted from it (cluster C8).
//! * the transport guards exercise `HttpSseConnection` against a mock MCP
//!   endpoint: shared reqwest builder (C3), shared SSE pump (C4), shared retry
//!   policy (C2) and the protocol seam (C7).

#[cfg(test)]
mod tests {

    use crate::mcp::http::{
        HttpSseConnection, MCP_REQUEST_TIMEOUT, McpError, http_client_options, is_retryable_status,
    };
    use crate::mcp::protocol::{
        HANDSHAKE_REQUEST_ID, JSONRPC_VERSION, JsonRpcRequest, MCP_PROTOCOL_VERSION,
        METHOD_INITIALIZE, METHOD_INITIALIZED, METHOD_TOOLS_CALL, METHOD_TOOLS_LIST, decode_reply,
        decode_response,
    };
    use crate::net::{BACKOFF_BASE_MS, MAX_ATTEMPTS, Retryable};
    use reqwest::StatusCode;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// The `initialize` envelope byte-for-byte as the protocol layer serializes
    /// it (serde sorts object keys, so this is deterministic). Pinned here so a
    /// silent change to the protocol layer fails loudly.
    fn canonical_initialize_body() -> String {
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{"tools":{}},"clientInfo":{"name":"marmel","version":"@VERSION@"},"protocolVersion":"2024-11-05"}}"#
            .replace("@VERSION@", env!("CARGO_PKG_VERSION"))
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Protocol guards — these call production code (`crate::mcp::protocol`).
    // ──────────────────────────────────────────────────────────────────────────

    /// The request envelope keeps the wire shape both transports have always
    /// sent: `jsonrpc`, `id`, `method`, and `params` omitted entirely when there
    /// are none.
    #[test]
    fn request_envelope_keeps_its_wire_shape() {
        let req = JsonRpcRequest::tools_list(123);
        let serialized = serde_json::to_value(&req).expect("envelope must serialize");
        assert_eq!(serialized["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(serialized["id"], 123);
        assert_eq!(serialized["method"], METHOD_TOOLS_LIST);
        assert!(serialized.get("params").is_none());

        let req = JsonRpcRequest::tools_call(124, "test", &json!({}));
        let serialized = serde_json::to_value(&req).expect("envelope must serialize");
        assert_eq!(serialized["method"], METHOD_TOOLS_CALL);
        assert_eq!(serialized["params"]["name"], "test");
        assert_eq!(
            req.body().expect("body"),
            r#"{"jsonrpc":"2.0","id":124,"method":"tools/call","params":{"arguments":{},"name":"test"}}"#
        );
    }

    /// A reply is accepted only when it carries the pending request's id —
    /// numerically or stringified. Everything else is somebody else's event.
    #[test]
    fn reply_id_matching_accepts_numeric_and_string_ids() {
        let numeric =
            decode_response(r#"{"jsonrpc":"2.0","id":123,"result":{}}"#).expect("decodes");
        assert!(numeric.id_matches(123));
        assert!(!numeric.id_matches(456));

        let textual =
            decode_response(r#"{"jsonrpc":"2.0","id":"123","result":{}}"#).expect("decodes");
        assert!(textual.id_matches(123));

        // The handshake id (0) is matched like any other id.
        let handshake =
            decode_response(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#).expect("decodes");
        assert!(handshake.id_matches(HANDSHAKE_REQUEST_ID));
        assert!(
            decode_response(&JsonRpcRequest::initialize().body().expect("body"))
                .expect("the request text must also parse as an envelope")
                .id_matches(HANDSHAKE_REQUEST_ID)
        );
    }

    /// (a) A JSON-RPC error envelope decodes into the right `McpError` variant,
    /// with the code, message, data and server name the callers always saw.
    #[test]
    fn json_rpc_error_envelope_decodes_into_the_mcp_error_variant() {
        let raw = concat!(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found","#,
            r#""data":{"detail":"oops"}}}"#,
        );
        let parsed = decode_response(raw).expect("an error envelope is a valid response");
        assert!(parsed.result.is_none());

        let err = parsed
            .into_mcp_result("test-server")
            .expect_err("an error envelope is never a result");
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
        // The message text surfaced to MCP consumers is unchanged.
        let text = err.to_string();
        assert!(text.contains("-32601"), "{text}");
        assert!(text.contains("Method not found"), "{text}");
        assert!(text.contains("test-server"), "{text}");

        // An application-level JSON-RPC error is never replayed.
        assert!(!err.is_retryable());
    }

    /// A result envelope decodes into its `result` value; a bare envelope is a
    /// null result rather than an error.
    #[test]
    fn result_envelope_decodes_into_its_result_value() {
        let ok =
            decode_response(r#"{"jsonrpc":"2.0","id":1,"result":{"foo":"bar"}}"#).expect("decodes");
        assert_eq!(
            ok.into_result("test-server").expect("result"),
            json!({"foo": "bar"})
        );

        let bare = decode_response(r#"{"jsonrpc":"2.0","id":1}"#).expect("decodes");
        assert_eq!(
            bare.into_result("test-server").expect("result"),
            Value::Null
        );
    }

    /// (b) A payload that is not the reply for this id — a foreign id, a
    /// foreign error, or plain noise — is skipped, and only the matching reply
    /// resolves.
    #[test]
    fn payloads_for_other_ids_are_skipped() {
        assert!(decode_reply(r#"{"jsonrpc":"2.0","id":99,"result":{"tools":[]}}"#, 1).is_none());
        assert!(
            decode_reply(
                r#"{"jsonrpc":"2.0","id":99,"error":{"code":-32601,"message":"Method not found"}}"#,
                1
            )
            .is_none()
        );
        assert!(
            decode_reply(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                1
            )
            .is_none()
        );
        assert!(decode_reply("2026-10-06 server: listening", 1).is_none());

        let reply = decode_reply(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#, 1)
            .expect("the matching reply resolves");
        assert_eq!(
            reply.into_result("mock-server").expect("result"),
            json!({"ok": true})
        );
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Mock MCP endpoint.
    // ──────────────────────────────────────────────────────────────────────────

    /// How the mock MCP endpoint answers.
    #[derive(Clone, Copy)]
    enum EndpointBehaviour {
        JsonToolsList,
        Fail503Once,
        Fail503Always,
        Fail400,
        JsonRpcError,
        SseUnrelatedThenMatch,
        /// An *error* envelope for another id, then the matching result: the
        /// error must be skipped, not surfaced.
        SseForeignErrorThenMatch,
        SseUnrelatedOnly,
        /// Rich `tools/list` result for the decoding guard.
        ToolsListRich,
        /// `tools/call` results for the reply-rendering guards.
        CallToolContent,
        CallToolIsError,
    }

    const TOOLS_LIST_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"alpha","description":"mock tool"}]}}"#;

    /// An SSE body whose first two events are noise for another request (one
    /// JSON-RPC event with a foreign id, one non-JSON event) and whose last one
    /// answers the pending `tools/list`.
    const SSE_UNRELATED_THEN_MATCH: &str = concat!(
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"unrelated\":true}}\n\n",
        "data: not-json-at-all\n\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"sse-alpha\"}]}}\n\n",
    );

    /// An SSE body that delivers a foreign *error* event and then the answer.
    const SSE_FOREIGN_ERROR_THEN_MATCH: &str = concat!(
        "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"error\":{\"code\":-32601,\"message\":\"somebody else's problem\"}}\n\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"sse-beta\"}]}}\n\n",
    );

    /// An SSE body that delivers one unrelated event and then ends: the stream
    /// has already delivered, so the request must not be replayed.
    const SSE_UNRELATED_ONLY: &str =
        "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"unrelated\":true}}\n\n";

    /// Response router for the mock MCP endpoint: it completes the
    /// `initialize` + `notifications/initialized` handshake and answers
    /// everything else per [`EndpointBehaviour`].
    ///
    /// Returns the router plus request counters: `(all requests, non-handshake
    /// requests)` — the counters are what prove the retry behaviour.
    fn endpoint_router(
        behaviour: EndpointBehaviour,
    ) -> (
        impl Fn(&Request) -> ResponseTemplate + Send + Sync + 'static,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let all = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let all_ctr = all.clone();
        let calls_ctr = calls.clone();
        let router = move |req: &Request| {
            all_ctr.fetch_add(1, Ordering::SeqCst);
            let body = String::from_utf8_lossy(&req.body).to_string();
            if body.contains("\"method\":\"notifications/initialized\"") {
                return ResponseTemplate::new(202);
            }
            if body.contains("\"method\":\"initialize\"") {
                return ResponseTemplate::new(200).set_body_string(
                    r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05"}}"#,
                );
            }
            let attempt = calls_ctr.fetch_add(1, Ordering::SeqCst);
            match behaviour {
                EndpointBehaviour::Fail503Once if attempt == 0 => ResponseTemplate::new(503),
                EndpointBehaviour::Fail503Once => {
                    ResponseTemplate::new(200).set_body_string(TOOLS_LIST_BODY)
                }
                EndpointBehaviour::Fail503Always => ResponseTemplate::new(503),
                EndpointBehaviour::Fail400 => {
                    ResponseTemplate::new(400).set_body_string("bad request")
                }
                EndpointBehaviour::JsonRpcError => ResponseTemplate::new(200).set_body_string(
                    r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#,
                ),
                EndpointBehaviour::SseUnrelatedThenMatch => ResponseTemplate::new(200)
                    .set_body_raw(SSE_UNRELATED_THEN_MATCH, "text/event-stream"),
                EndpointBehaviour::SseForeignErrorThenMatch => ResponseTemplate::new(200)
                    .set_body_raw(SSE_FOREIGN_ERROR_THEN_MATCH, "text/event-stream"),
                EndpointBehaviour::SseUnrelatedOnly => {
                    ResponseTemplate::new(200).set_body_raw(SSE_UNRELATED_ONLY, "text/event-stream")
                }
                EndpointBehaviour::ToolsListRich => ResponseTemplate::new(200).set_body_string(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[
                        {"name":"full","description":"desc1","inputSchema":{"type":"object","properties":{"a":{"type":"string"}}}},
                        {"name":"no-schema"},
                        {"name":"null-schema","inputSchema":null},
                        {"description":"nameless entries are skipped"}
                    ]}}"#,
                ),
                EndpointBehaviour::CallToolContent => ResponseTemplate::new(200).set_body_string(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"Hello "},{"type":"text","text":"World"}],"isError":false}}"#,
                ),
                EndpointBehaviour::CallToolIsError => ResponseTemplate::new(200).set_body_string(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"text":"Critical failure"}],"isError":true}}"#,
                ),
                EndpointBehaviour::JsonToolsList => {
                    ResponseTemplate::new(200).set_body_string(TOOLS_LIST_BODY)
                }
            }
        };
        (router, all, calls)
    }

    /// Mount the router on `POST /mcp` and hand back the mock server.
    async fn mount_endpoint(
        behaviour: EndpointBehaviour,
    ) -> (MockServer, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let server = MockServer::start().await;
        let (router, all_calls, call_calls) = endpoint_router(behaviour);
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(router)
            .mount(&server)
            .await;
        (server, all_calls, call_calls)
    }

    /// Mount the same endpoint but record the raw body of every request, so the
    /// bytes the transport actually sends can be asserted.
    async fn mount_recording_endpoint(
        behaviour: EndpointBehaviour,
    ) -> (MockServer, Arc<Mutex<Vec<String>>>) {
        let server = MockServer::start().await;
        let (router, _all, _calls) = endpoint_router(behaviour);
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = bodies.clone();
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(move |req: &Request| {
                recorder
                    .lock()
                    .expect("recorder lock")
                    .push(String::from_utf8_lossy(&req.body).to_string());
                router(req)
            })
            .mount(&server)
            .await;
        (server, bodies)
    }

    /// Connect to the mock endpoint with a short, test-friendly deadline.
    async fn connect_mock(server: &MockServer, timeout: Duration) -> HttpSseConnection {
        HttpSseConnection::connect_with_timeout(
            "mock-server",
            &format!("{}/mcp", server.uri()),
            timeout,
        )
        .await
        .expect("initialize handshake should succeed")
    }

    // ──────────────────────────────────────────────────────────────────────────
    // C7: both transports speak the protocol layer's envelopes on the wire.
    // ──────────────────────────────────────────────────────────────────────────

    /// The HTTP transport puts exactly the envelopes `crate::mcp::protocol`
    /// builds — nothing of its own: same `jsonrpc`, same method strings, same
    /// param shape, same handshake id and allocator sequence. The stdio
    /// transport's counterpart (`mcp::client`'s wire capture) asserts the same
    /// bytes, which is what makes the two transports agree.
    #[tokio::test]
    async fn http_wire_bodies_are_the_protocol_layers_envelopes() {
        let (server, bodies) = mount_recording_endpoint(EndpointBehaviour::JsonToolsList).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;
        conn.list_tools().await.expect("tools/list");

        let sent = bodies.lock().expect("recorder lock").clone();
        assert_eq!(sent.len(), 3, "initialize + notification + tools/list");

        // The handshake envelope: identical to what the protocol layer builds,
        // and pinned to its literal text.
        assert_eq!(sent[0], JsonRpcRequest::initialize().body().expect("body"));
        assert_eq!(sent[0], canonical_initialize_body());
        let handshake: Value = serde_json::from_str(&sent[0]).expect("handshake envelope parses");
        assert_eq!(handshake["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(handshake["method"], METHOD_INITIALIZE);
        assert_eq!(handshake["id"], HANDSHAKE_REQUEST_ID);
        assert_eq!(handshake["params"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(handshake["params"]["capabilities"], json!({"tools": {}}));
        assert_eq!(handshake["params"]["clientInfo"]["name"], "marmel");

        // A notification carries no id at all.
        assert_eq!(
            sent[1],
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        );

        // The handshake did not consume an id: `tools/list` is id 1.
        assert_eq!(sent[2], r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        assert_eq!(sent[2], JsonRpcRequest::tools_list(1).body().expect("body"));
    }

    /// A `tools/call` envelope carries the protocol layer's param shape.
    #[tokio::test]
    async fn http_call_tool_envelope_carries_the_protocol_params() {
        let (server, bodies) = mount_recording_endpoint(EndpointBehaviour::CallToolContent).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;
        conn.call_tool("alpha", &json!({"city": "Oslo"}))
            .await
            .expect("tools/call");

        let sent = bodies.lock().expect("recorder lock").clone();
        assert_eq!(sent.len(), 3);
        assert_eq!(
            sent[2],
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","#,
                r#""params":{"arguments":{"city":"Oslo"},"name":"alpha"}}"#
            )
        );
    }

    // ──────────────────────────────────────────────────────────────────────────
    // C3 + C4 + C2 wiring: shared reqwest builder, shared SSE pump, shared
    // retry policy (see docs/recon_duplication_net.md).
    // ──────────────────────────────────────────────────────────────────────────

    /// Cluster C3: the transport must construct its `reqwest::Client` through
    /// the shared builder — never with a hand-rolled `reqwest::Client::builder()`
    /// — and must pass its own (unchanged) client-level settings into it.
    #[test]
    fn mcp_http_builds_its_client_through_the_shared_builder() {
        let src = include_str!("http.rs");
        assert!(
            src.contains("build_http_client_with("),
            "the MCP HTTP transport must build its client via net::http::build_http_client_with"
        );
        assert!(
            !src.contains("reqwest::Client::builder"),
            "the MCP HTTP transport must not hand-roll a reqwest client"
        );

        // The MCP-specific settings survive, as parameters of the shared builder.
        assert_eq!(MCP_REQUEST_TIMEOUT, Duration::from_secs(30));
        let opts = http_client_options(MCP_REQUEST_TIMEOUT);
        assert_eq!(
            opts.total_timeout,
            Some(Duration::from_secs(30)),
            "MCP keeps its own 30 s total request timeout"
        );
        assert_eq!(
            opts.connect_timeout, None,
            "MCP never had a connect timeout — behaviour preserved"
        );
        assert!(
            crate::net::http::build_http_client_with(opts).is_ok(),
            "the shared builder must accept the MCP options"
        );

        // Cluster C2: the retry policy is consumed from its single owner.
        assert_eq!(MAX_ATTEMPTS, 3, "MCP must use the shared attempt count");
        assert_eq!(
            BACKOFF_BASE_MS, 1000,
            "MCP must use the shared backoff base"
        );
    }

    /// Cluster C7: the transport must not own a second copy of the protocol.
    /// Only the *production* half of each transport file is inspected — the test
    /// modules deliberately quote wire literals to pin them.
    #[test]
    fn mcp_http_transport_keeps_no_protocol_of_its_own() {
        fn production_only(src: &str) -> &str {
            src.split("#[cfg(test)]\nmod tests {").next().unwrap_or(src)
        }
        for src in [
            production_only(include_str!("http.rs")),
            production_only(include_str!("client.rs")),
        ] {
            assert!(
                !src.contains("jsonrpc:\"2.0\"") && !src.contains("jsonrpc: \"2.0\""),
                "the transports must not build JSON-RPC envelopes themselves"
            );
            for method in [
                METHOD_INITIALIZE,
                METHOD_INITIALIZED,
                METHOD_TOOLS_LIST,
                METHOD_TOOLS_CALL,
            ] {
                assert!(
                    !src.contains(&format!("\"{method}\"")),
                    "the transports must not spell out the method name '{method}'"
                );
            }
            assert!(
                !src.contains("MCP_PROTOCOL_VERSION") && !src.contains("2024-11-05"),
                "the transports must not carry the initialize params"
            );
            assert!(
                !src.contains("AtomicU64"),
                "the transports must not own a request-id allocator"
            );
            assert!(
                !src.contains("\"inputSchema\"") && !src.contains("\"isError\""),
                "the transports must not decode results themselves"
            );
        }
    }

    /// Cluster C2 retry classes: connection/timeout/5xx-class failures are
    /// retried, everything else fails immediately.
    #[test]
    fn mcp_retryable_error_classes() {
        for code in [408u16, 429, 500, 501, 502, 503, 504] {
            assert!(
                is_retryable_status(StatusCode::from_u16(code).unwrap()),
                "HTTP {code} must be retryable"
            );
        }
        for code in [400u16, 401, 403, 404, 409, 413, 422] {
            assert!(
                !is_retryable_status(StatusCode::from_u16(code).unwrap()),
                "HTTP {code} must fail immediately"
            );
        }

        let server = "srv".to_string();
        let method = "tools/call".to_string();
        assert!(
            McpError::HttpStatus {
                server: server.clone(),
                method: method.clone(),
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: String::new(),
            }
            .is_retryable()
        );
        assert!(
            !McpError::HttpStatus {
                server: server.clone(),
                method: method.clone(),
                status: StatusCode::BAD_REQUEST,
                body: String::new(),
            }
            .is_retryable()
        );
        assert!(
            !McpError::JsonRpc {
                server: server.clone(),
                code: -32601,
                message: "Method not found".to_string(),
                data: None,
            }
            .is_retryable(),
            "application-level JSON-RPC errors are never replayed"
        );
        assert!(
            !McpError::Decode {
                server: server.clone(),
                method: method.clone(),
                source: serde_json::from_str::<Value>("{").unwrap_err(),
            }
            .is_retryable()
        );
        assert!(
            !McpError::Aborted {
                server: server.clone(),
                method: method.clone(),
            }
            .is_retryable()
        );
    }

    /// Cluster C2/C4: stream failures are replayable only while the stream has
    /// delivered nothing — never after an event reached the caller.
    #[test]
    fn mcp_stream_failures_retry_only_before_the_first_event() {
        let server = "srv".to_string();
        let method = "tools/call".to_string();

        assert!(
            McpError::SseClosed {
                server: server.clone(),
                method: method.clone(),
                saw_event: false,
            }
            .is_retryable(),
            "a stream that closed without delivering anything is a connection failure"
        );
        assert!(
            !McpError::SseClosed {
                server: server.clone(),
                method: method.clone(),
                saw_event: true,
            }
            .is_retryable(),
            "no partial-stream replay once an event was delivered"
        );
        assert!(
            McpError::Sse {
                server: server.clone(),
                method: method.clone(),
                reason: "framing error".to_string(),
                saw_event: false,
            }
            .is_retryable()
        );
        assert!(
            !McpError::Sse {
                server: server.clone(),
                method: method.clone(),
                reason: "framing error".to_string(),
                saw_event: true,
            }
            .is_retryable()
        );
        assert!(
            McpError::SseTimeout {
                server: server.clone(),
                method: method.clone(),
                saw_event: false,
            }
            .is_retryable()
        );
        assert!(
            !McpError::SseTimeout {
                server: server.clone(),
                method: method.clone(),
                saw_event: true,
            }
            .is_retryable()
        );
    }

    /// End-to-end proof of the retry wiring: a 503 is retried and the reply of
    /// the successful attempt is returned.
    #[tokio::test]
    async fn mcp_http_retries_5xx_and_recovers() {
        let (server, all_calls, call_calls) = mount_endpoint(EndpointBehaviour::Fail503Once).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let tools = conn
            .list_tools()
            .await
            .expect("tools/list must succeed after one retry");

        assert_eq!(
            call_calls.load(Ordering::SeqCst),
            2,
            "a 503 must be retried exactly once before the success"
        );
        assert_eq!(
            all_calls.load(Ordering::SeqCst),
            4,
            "initialize + initialized-notification + two tools/list attempts"
        );
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "alpha");
        assert_eq!(tools[0].server_name, "mock-server");
    }

    /// A persistently retryable failure stops at exactly `net::MAX_ATTEMPTS`.
    #[tokio::test]
    async fn mcp_http_retries_5xx_at_most_max_attempts_times() {
        let (server, _all_calls, call_calls) =
            mount_endpoint(EndpointBehaviour::Fail503Always).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let err = conn.list_tools().await.unwrap_err().to_string();

        assert!(
            err.contains("HTTP 503 Service Unavailable") && err.contains("mock-server"),
            "expected the transport's own status message, got: {err}"
        );
        assert_eq!(
            call_calls.load(Ordering::SeqCst),
            MAX_ATTEMPTS as usize,
            "retryable failures must stop at exactly the shared MAX_ATTEMPTS"
        );
    }

    /// A 4xx is not the transient class: exactly one request is made.
    #[tokio::test]
    async fn mcp_http_does_not_retry_a_client_error() {
        let (server, _all_calls, call_calls) = mount_endpoint(EndpointBehaviour::Fail400).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let err = conn.list_tools().await.unwrap_err().to_string();

        assert!(
            err.contains("HTTP 400 Bad Request") && err.contains("bad request"),
            "expected the unchanged 4xx message, got: {err}"
        );
        assert_eq!(
            call_calls.load(Ordering::SeqCst),
            1,
            "a 4xx must fail immediately (no retry)"
        );
    }

    /// (a) An application-level JSON-RPC error envelope on the wire is decoded
    /// into `McpError::JsonRpc` with the unchanged message, and is never
    /// retried.
    #[tokio::test]
    async fn mcp_http_does_not_retry_a_json_rpc_error() {
        let (server, _all_calls, call_calls) =
            mount_endpoint(EndpointBehaviour::JsonRpcError).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let err = conn.list_tools().await.unwrap_err();

        assert!(
            err.to_string().contains("-32601") && err.to_string().contains("Method not found"),
            "expected the unchanged JSON-RPC error message, got: {err}"
        );
        assert_eq!(
            call_calls.load(Ordering::SeqCst),
            1,
            "JSON-RPC errors must fail immediately (no retry)"
        );
    }

    /// (b) Cluster C4/C7: `text/event-stream` responses are consumed through the
    /// shared pump and the protocol's reply matching — events that answer
    /// another id (and non-JSON events) are skipped in place, and the matching
    /// event still resolves the call.
    #[tokio::test]
    async fn mcp_http_skips_unrelated_events_and_resolves_the_matching_one() {
        let (server, _all_calls, call_calls) =
            mount_endpoint(EndpointBehaviour::SseUnrelatedThenMatch).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let tools = conn
            .list_tools()
            .await
            .expect("the matching SSE event must answer the request");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "sse-alpha");
        assert_eq!(
            call_calls.load(Ordering::SeqCst),
            1,
            "the unrelated event is skipped in-place, the request is not resent"
        );
    }

    /// (b) A foreign *error* event on the same stream must not hijack the
    /// pending call: it is skipped and the matching result is returned.
    #[tokio::test]
    async fn mcp_http_ignores_an_error_event_for_another_request() {
        let (server, _all_calls, call_calls) =
            mount_endpoint(EndpointBehaviour::SseForeignErrorThenMatch).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let tools = conn
            .list_tools()
            .await
            .expect("the foreign error must not fail the call");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "sse-beta");
        assert_eq!(call_calls.load(Ordering::SeqCst), 1);
    }

    /// Cluster C2 safety rule: once the SSE stream has delivered an event, the
    /// request is never replayed — even though the caller got no answer.
    #[tokio::test]
    async fn mcp_http_never_replays_a_stream_that_already_delivered_an_event() {
        let (server, _all_calls, call_calls) =
            mount_endpoint(EndpointBehaviour::SseUnrelatedOnly).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let err = conn.list_tools().await.unwrap_err().to_string();

        assert!(
            err.contains("SSE stream from 'mock-server' closed before response for 'tools/list'"),
            "expected the unchanged stream-closed message, got: {err}"
        );
        assert_eq!(
            call_calls.load(Ordering::SeqCst),
            1,
            "a partially delivered stream must not be replayed"
        );
    }

    /// (c) A `tools/list` result decodes into the tool list type through the
    /// production path: description is optional, a missing `inputSchema` falls
    /// back to the default object schema, an explicit `null` schema is passed
    /// through (the old hand-copied test filtered it out — that copy was the
    /// C8 drift), and a nameless entry is skipped.
    #[tokio::test]
    async fn tools_list_result_decodes_into_the_tool_list_type() {
        let (server, _all_calls, _call_calls) =
            mount_endpoint(EndpointBehaviour::ToolsListRich).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let tools = conn.list_tools().await.expect("tools/list must decode");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["full", "no-schema", "null-schema"]);

        assert_eq!(tools[0].description.as_deref(), Some("desc1"));
        assert_eq!(
            tools[0].input_schema,
            json!({"type": "object", "properties": {"a": {"type": "string"}}})
        );

        assert_eq!(tools[1].description, None);
        assert_eq!(tools[1].input_schema, json!({"type": "object"}));

        assert_eq!(tools[2].input_schema, Value::Null);

        // Tools are stamped with the configured server and qualify their names.
        assert!(tools.iter().all(|t| t.server_name == "mock-server"));
        assert_eq!(tools[0].qualified_name(), "mock-server__full");
    }

    /// (d) A `tools/call` result renders through the production path: the
    /// `content[]` items are joined one per line and trimmed.
    #[tokio::test]
    async fn call_tool_result_content_decodes_into_reply_text() {
        let (server, _all_calls, _call_calls) =
            mount_endpoint(EndpointBehaviour::CallToolContent).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let reply = conn
            .call_tool("alpha", &json!({"city": "Oslo"}))
            .await
            .expect("tools/call must succeed");
        assert_eq!(reply, "Hello \nWorld");
    }

    /// (d) `isError: true` turns the rendered content into an error at the
    /// caller boundary, with the content as its message.
    #[tokio::test]
    async fn call_tool_error_result_surfaces_as_an_error() {
        let (server, _all_calls, _call_calls) =
            mount_endpoint(EndpointBehaviour::CallToolIsError).await;
        let mut conn = connect_mock(&server, Duration::from_secs(5)).await;

        let err = conn
            .call_tool("alpha", &json!({}))
            .await
            .expect_err("isError must surface as an error");
        assert_eq!(err.to_string(), "Critical failure");
    }

    /// Cluster C3 end-to-end: the total timeout handed to the shared builder is
    /// the one enforced by the transport, and such a timeout is retried as a
    /// transport failure.
    #[tokio::test]
    async fn mcp_http_enforces_the_shared_builder_timeout_and_retries_it() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let responder = {
            let calls = calls.clone();
            move |_req: &Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200)
                    .set_body_string("{}")
                    .set_delay(Duration::from_millis(500))
            }
        };
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(responder)
            .mount(&server)
            .await;

        let err = match HttpSseConnection::connect_with_timeout(
            "mock-server",
            &format!("{}/mcp", server.uri()),
            Duration::from_millis(100),
        )
        .await
        {
            Ok(_) => panic!("the shared builder's total timeout must fail the handshake"),
            Err(e) => e.to_string(),
        };

        assert!(
            err.contains("HTTP request to MCP server 'mock-server' for 'initialize' failed"),
            "expected the unchanged transport message, got: {err}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            MAX_ATTEMPTS as usize,
            "a reqwest total timeout (from the shared builder) is a retryable transport failure"
        );
    }
}
