use marmennill::mcp::{McpManager, McpServerConfig};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

#[tokio::test]
async fn test_mcp_deep_research_server_end_to_end() {
    let mut bin_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    bin_path.push("target");
    bin_path.push("debug");
    bin_path.push("mcp-deep-research");

    assert!(
        bin_path.exists(),
        "mcp-deep-research binary should be built at {:?}",
        bin_path
    );

    let mut mcp_servers = HashMap::new();
    mcp_servers.insert(
        "deep_research".to_string(),
        McpServerConfig {
            command: Some(bin_path.to_string_lossy().to_string()),
            args: vec![],
            env: HashMap::new(),
            url: None,
        },
    );

    let manager = McpManager::boot(&mcp_servers)
        .await
        .expect("McpManager::boot should successfully boot mcp-deep-research");

    // 1. Tool discovery
    assert!(manager.has_tool("deep_research__deep_research"));
    assert!(manager.has_tool("deep_research__search"));
    assert!(manager.has_tool("deep_research__ketch_docs"));

    // 2. Role gating / tools_for_servers scoping
    let researcher_tools = manager.tools_for_servers(&["deep_research".to_string()]);
    assert_eq!(researcher_tools.len(), 3);
    assert!(
        researcher_tools
            .iter()
            .all(|t| t.server_name == "deep_research")
    );

    // 3. Graceful degradation when calling SearchWala search (when SearchWala daemon is offline)
    let search_res = manager
        .call_tool(
            "deep_research__search",
            &json!({"query": "Rust tokio tutorial"}),
        )
        .await
        .expect("search call should return Ok result with graceful degradation");

    assert!(
        search_res.contains("SearchWala is currently unavailable")
            || search_res.contains("SearchWala Search Results")
    );

    // 4. Graceful degradation when calling ketch_docs (when ketch CLI is not installed)
    let ketch_res = manager
        .call_tool(
            "deep_research__ketch_docs",
            &json!({
                "query": "router nesting",
                "library": "axum"
            }),
        )
        .await
        .expect("ketch_docs call should return Ok result with graceful degradation");

    assert!(
        ketch_res.contains("ketch CLI is not installed")
            || ketch_res.contains("Ketch Documentation")
    );

    // 5. Clean shutdown
    manager.shutdown().await;
}
