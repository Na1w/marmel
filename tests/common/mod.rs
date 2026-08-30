//! Shared wiremock helpers and fixture loaders for integration tests.
//!
//! These helpers are NOT run under `cargo test --lib`; they are compiled only
//! into the `tests/` integration targets only.

/// Construct a mock backend URL for wiremock.
pub fn mock_backend() -> String {
    "http://localhost:8000/v1".to_string()
}

/// A canned OpenAI-style SSE payload for a single completion.
pub fn completion_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": { "content": text },
                "finish_reason": null
            }]
        })
    )
}
