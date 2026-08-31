#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::http::{JsonRpcRequest, JsonRpcResponse, JsonRpcError};
    use crate::mcp::McpTool;
    use serde_json::{json, Value};

    #[test]
    fn test_json_rpc_request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 123,
            method: "tools/list".to_string(),
            params: None,
        };
        let serialized = serde_json::to_value(&req).unwrap();
        assert_eq!(serialized["jsonrpc"], "2.0");
        assert_eq!(serialized["id"], 123);
        assert_eq!(serialized["method"], "tools/list");
        assert!(serialized.get("params").is_none());

        let req_with_params = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 124,
            method: "tools/call".to_string(),
            params: Some(json!({"name": "test", "arguments": {}})),
        };
        let serialized_params = serde_json::to_value(&req_with_params).unwrap();
        assert_eq!(serialized_params["params"]["name"], "test");
    }

    #[test]
    fn test_json_rpc_response_id_matching() {
        let resp_num = JsonRpcResponse {
            jsonrpc: Some("2.0".to_string()),
            id: Some(json!(123)),
            result: Some(json!({})),
            error: None,
        };
        assert!(resp_num.id_matches(123));
        assert!(!resp_num.id_matches(456));

        let resp_str = JsonRpcResponse {
            jsonrpc: Some("2.0".to_string()),
            id: Some(json!("123")),
            result: Some(json!({})),
            error: None,
        };
        assert!(resp_str.id_matches(123));
    }

    #[test]
    fn test_json_rpc_response_into_result() {
        let resp_ok = JsonRpcResponse {
            jsonrpc: Some("2.0".to_string()),
            id: Some(json!(1)),
            result: Some(json!({"foo": "bar"})),
            error: None,
        };
        let result = resp_ok.into_result("test-server").unwrap();
        assert_eq!(result["foo"], "bar");

        let resp_err = JsonRpcResponse {
            jsonrpc: Some("2.0".to_string()),
            id: Some(json!(1)),
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: "Method not found".to_string(),
                data: Some(json!({"detail": "oops"})),
            }),
        };
        let result_err = resp_err.into_result("test-server");
        assert!(result_err.is_err());
        let err_msg = result_err.unwrap_err().to_string();
        assert!(err_msg.contains("-32601"));
        assert!(err_msg.contains("Method not found"));
        assert!(err_msg.contains("test-server"));
    }

    #[tokio::test]
    async fn test_mcp_tool_parsing() {
        // Mock the logic inside list_tools
        let server_name = "test-server";
        let result = json!({
            "tools": [
                {
                    "name": "tool1",
                    "description": "desc1",
                    "inputSchema": {"type": "object", "properties": {"a": {"type": "string"}}}
                },
                {
                    "name": "tool2",
                    "inputSchema": null // should fallback to default
                }
            ]
        });

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
                        .filter(|v| !v.is_null())
                        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
                    tools.push(McpTool {
                        name: name.to_string(),
                        description,
                        input_schema,
                        server_name: server_name.to_string(),
                    });
                }
            }
        }

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "tool1");
        assert_eq!(tools[0].description, Some("desc1".to_string()));
        assert_eq!(tools[1].name, "tool2");
        assert_eq!(tools[1].description, None);
        assert_eq!(tools[1].input_schema, json!({"type": "object"}));
    }

    #[tokio::test]
    async fn test_call_tool_result_parsing() {
        let server_name = "test-server";
        
        // Case 1: Standard content array
        let result1 = json!({
            "content": [
                {"type": "text", "text": "Hello "},
                {"type": "text", "text": "World"}
            ],
            "isError": false
        });

        let mut output1 = String::new();
        if let Some(content_arr) = result1.get("content").and_then(Value::as_array) {
            for item in content_arr {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    output1.push_str(text);
                } else {
                    output1.push_str(&item.to_string());
                }
                output1.push('\n');
            }
        }
        let final_out1 = output1.trim().to_string();
        assert_eq!(final_out1, "Hello \nWorld");

        // Case 2: Simple text field
        let result2 = json!({
            "text": "Simple response",
            "isError": false
        });
        let mut output2 = String::new();
        if let Some(text) = result2.get("text").and_then(Value::as_str) {
            output2.push_str(text);
        }
        assert_eq!(output2.trim(), "Simple response");

        // Case 3: isError = true
        let result3 = json!({
            "content": [{"text": "Critical failure"}],
            "isError": true
        });
        let is_error = result3.get("isError").and_then(Value::as_bool).unwrap_or(false);
        assert!(is_error);
    }
}
