use serde::{Deserialize, Serialize};

// --- SearchWala Models ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchWalaLlmConfig {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchWalaRequest {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_results: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm: Option<SearchWalaLlmConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_copilot: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchWalaResponse {
    pub query: String,
    #[serde(default)]
    pub sources_found: usize,
    #[serde(default)]
    pub sources_processed: usize,
    #[serde(default)]
    pub results: Vec<SourceResult>,
    #[serde(default)]
    pub search_results: Vec<SearchHit>,
    #[serde(default)]
    pub copilot_query: Option<String>,
    #[serde(default)]
    pub llm_answer: Option<String>,
    #[serde(default)]
    pub llm_error: Option<String>,
    #[serde(default)]
    pub elapsed_seconds: f64,
    #[serde(default)]
    pub engine_stats: EngineStats,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SourceResult {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub extracted_text: String,
    #[serde(default)]
    pub char_count: usize,
    #[serde(default)]
    pub engine: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchHit {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub engine: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EngineStats {
    #[serde(default)]
    pub engines_queried: Vec<String>,
    #[serde(default)]
    pub total_raw_results: usize,
    #[serde(default)]
    pub deduplicated_urls: usize,
}

// --- MCP Protocol Models ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}
