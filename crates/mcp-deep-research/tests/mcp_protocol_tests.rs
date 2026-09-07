use mcp_deep_research::{KetchClient, McpServer, SearchWalaClient};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_mcp_initialize_and_tools_list() {
    let searchwala = SearchWalaClient::new(Some("http://127.0.0.1:9999".to_string()));
    let ketch = KetchClient::new(Some("non_existent_ketch_bin".to_string()));
    let server = McpServer::new(searchwala, ketch);

    let (client_read, server_write) = tokio::io::duplex(4096);
    let (server_read, client_write) = tokio::io::duplex(4096);

    let server_handle = tokio::spawn(async move {
        let server_buf_reader = tokio::io::BufReader::new(server_read);
        let _ = server.run(server_buf_reader, server_write).await;
    });

    let mut client_lines = tokio::io::BufReader::new(client_read);
    let mut client_write = client_write;

    // 1. Send initialize
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05"
        }
    });
    client_write
        .write_all(format!("{init_req}\n").as_bytes())
        .await
        .unwrap();
    client_write.flush().await.unwrap();

    use tokio::io::AsyncBufReadExt;
    let mut resp_line = String::new();
    client_lines.read_line(&mut resp_line).await.unwrap();
    let init_resp: Value = serde_json::from_str(&resp_line).unwrap();
    assert_eq!(init_resp["id"], 1);
    assert_eq!(
        init_resp["result"]["serverInfo"]["name"],
        "mcp-deep-research"
    );

    // 2. Send initialized notification
    let notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    client_write
        .write_all(format!("{notif}\n").as_bytes())
        .await
        .unwrap();
    client_write.flush().await.unwrap();

    // 3. Send tools/list
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });
    client_write
        .write_all(format!("{list_req}\n").as_bytes())
        .await
        .unwrap();
    client_write.flush().await.unwrap();

    resp_line.clear();
    client_lines.read_line(&mut resp_line).await.unwrap();
    let list_resp: Value = serde_json::from_str(&resp_line).unwrap();
    assert_eq!(list_resp["id"], 2);

    let tools = list_resp["result"]["tools"].as_array().unwrap();
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    assert!(tool_names.contains(&"deep_research"));
    assert!(tool_names.contains(&"search"));
    assert!(tool_names.contains(&"ketch_docs"));

    drop(client_write);
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_searchwala_deep_research_with_mock() {
    let mock_server = MockServer::start().await;

    let mock_response = serde_json::json!({
        "query": "Rust tokio 1.44 features",
        "sources_found": 15,
        "sources_processed": 5,
        "results": [
            {
                "url": "https://tokio.rs/blog/2026-03-tokio-release",
                "title": "Tokio 1.44 Release Notes",
                "extracted_text": "Tokio 1.44 introduces enhanced cooperative scheduling and optimized I/O drivers.",
                "char_count": 82,
                "engine": "duckduckgo"
            }
        ],
        "search_results": [],
        "copilot_query": null,
        "llm_answer": "### Tokio 1.44 Features\n\nTokio 1.44 improves async runtime throughput [1].",
        "llm_error": null,
        "elapsed_seconds": 1.25,
        "engine_stats": {
            "engines_queried": ["duckduckgo", "google"],
            "total_raw_results": 20,
            "deduplicated_urls": 15
        }
    });

    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&mock_response))
        .mount(&mock_server)
        .await;

    let searchwala = SearchWalaClient::new(Some(mock_server.uri()));
    let ketch = KetchClient::new(None);
    let server = McpServer::new(searchwala, ketch);

    let call_req = mcp_deep_research::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(10)),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "deep_research",
            "arguments": {
                "query": "Rust tokio 1.44 features"
            }
        })),
    };

    let resp = server.handle_request(call_req).await.unwrap();
    assert_eq!(resp.id, 10);
    let result = resp.result.unwrap();
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().unwrap();

    assert!(text.contains("Deep Research Report: \"Rust tokio 1.44 features\""));
    assert!(text.contains("Tokio 1.44 improves async runtime throughput [1]."));
    assert!(text.contains("https://tokio.rs/blog/2026-03-tokio-release"));
}

#[tokio::test]
async fn test_searchwala_graceful_degradation_when_offline() {
    // Port 19999 has no running server
    let searchwala = SearchWalaClient::new(Some("http://127.0.0.1:19999".to_string()));
    let ketch = KetchClient::new(None);
    let server = McpServer::new(searchwala, ketch);

    let call_req = mcp_deep_research::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(11)),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "deep_research",
            "arguments": {
                "query": "Distributed transactions"
            }
        })),
    };

    let resp = server.handle_request(call_req).await.unwrap();
    assert_eq!(resp.id, 11);
    let result = resp.result.unwrap();
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().unwrap();

    assert!(text.contains("SearchWala is currently unavailable"));
    assert!(text.contains("Graceful Degradation"));
    assert!(text.contains("cargo run --release"));
}

#[tokio::test]
async fn test_ketch_docs_graceful_degradation_when_missing() {
    let searchwala = SearchWalaClient::new(None);
    let ketch = KetchClient::new(Some("ketch_nonexistent_cli_path_123".to_string()));
    let server = McpServer::new(searchwala, ketch);

    let call_req = mcp_deep_research::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(12)),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "ketch_docs",
            "arguments": {
                "query": "render word wrap",
                "library": "/charmbracelet/glamour"
            }
        })),
    };

    let resp = server.handle_request(call_req).await.unwrap();
    assert_eq!(resp.id, 12);
    let result = resp.result.unwrap();
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().unwrap();

    assert!(text.contains("ketch CLI is not installed"));
    assert!(text.contains("brew install 1broseidon/tap/ketch"));
    assert!(text.contains("/charmbracelet/glamour"));
}
