//! Wire types for the OpenAI-compatible chat completions API and tool calls.

use serde::{Deserialize, Serialize};

use super::tools::ToolDef;

/// A message in the chat transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    #[serde(rename = "system")]
    System { content: String },
    #[serde(rename = "user")]
    User { content: String },
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default, serialize_with = "serialize_assistant_content")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    #[serde(rename = "tool")]
    Tool {
        tool_call_id: String,
        content: String,
    },
}

fn serialize_assistant_content<S>(
    content: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match content {
        Some(s) => serializer.serialize_str(s),
        None => serializer.serialize_str(""),
    }
}

impl Message {
    pub fn role(&self) -> &'static str {
        match self {
            Message::System { .. } => "system",
            Message::User { .. } => "user",
            Message::Assistant { .. } => "assistant",
            Message::Tool { .. } => "tool",
        }
    }

    pub fn content(&self) -> Option<&str> {
        match self {
            Message::System { content } => Some(content.as_str()),
            Message::User { content } => Some(content.as_str()),
            Message::Assistant { content, .. } => content.as_deref(),
            Message::Tool { content, .. } => Some(content.as_str()),
        }
    }

    pub fn reasoning_content(&self) -> Option<&str> {
        match self {
            Message::Assistant {
                reasoning_content, ..
            } => reasoning_content.as_deref(),
            _ => None,
        }
    }
}

/// A single tool invocation requested by the assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(default = "default_tool_type")]
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

fn default_tool_type() -> String {
    "function".to_string()
}

/// Ensure tool arguments are valid JSON so OpenAI-compatible backends never reject
/// the request with HTTP 400 (e.g. Jinja/Python `json.loads` on conversation history).
pub fn ensure_valid_json_arguments(raw: &str) -> (String, bool) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ("{}".to_string(), false);
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Object(map)) => {
            if map.contains_key("_error")
                && map.get("_error").and_then(serde_json::Value::as_str)
                    == Some("malformed_arguments")
            {
                (trimmed.to_string(), true)
            } else {
                (trimmed.to_string(), false)
            }
        }
        Ok(other) => {
            let wrapped = serde_json::json!({ "_raw_value": other }).to_string();
            (wrapped, false)
        }
        Err(_) => {
            let fallback = serde_json::json!({
                "_error": "malformed_arguments",
                "_raw": raw
            })
            .to_string();
            (fallback, true)
        }
    }
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        let (valid_args, _) = ensure_valid_json_arguments(&arguments.into());
        Self {
            id: id.into(),
            kind: "function".to_string(),
            function: ToolFunction {
                name: name.into(),
                arguments: valid_args,
            },
        }
    }

    /// Ensure the arguments field is valid JSON so OpenAI backends never fail on conversation history.
    pub fn sanitize_arguments(&mut self) {
        let (valid_args, _) = ensure_valid_json_arguments(&self.function.arguments);
        self.function.arguments = valid_args;
    }

    /// Check whether the tool arguments were truncated or malformed JSON.
    pub fn is_malformed(&self) -> bool {
        if let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(&self.function.arguments)
        {
            map.contains_key("_error")
                && map.get("_error").and_then(serde_json::Value::as_str)
                    == Some("malformed_arguments")
        } else {
            true
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

/// Request body sent to `/chat/completions`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// `enable_thinking=false` forces the backend to suppress reasoning tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    pub id: Option<String>,
    pub choices: Vec<ChunkChoice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, alias = "reasoning")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkToolCall {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChunkToolFunction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkToolFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_serializes_with_type_function() {
        let tc = ToolCall::new(
            "call_123",
            crate::tool_names::TOOL_CREATE_PLAN,
            r#"{"plan_markdown":"test"}"#,
        );
        let val = serde_json::to_value(&tc).unwrap();
        assert_eq!(val["type"], "function");
        assert_eq!(val["id"], "call_123");
        assert_eq!(val["function"]["name"], crate::tool_names::TOOL_CREATE_PLAN);
    }

    #[test]
    fn tool_call_deserializes_missing_type_as_function() {
        let json_str = r#"{"id":"call_456","function":{"name":"read_file","arguments":"{}"}}"#;
        let tc: ToolCall = serde_json::from_str(json_str).unwrap();
        assert_eq!(tc.kind, "function");
        assert_eq!(tc.id, "call_456");
    }

    #[test]
    fn test_ensure_valid_json_arguments_valid() {
        let (res, malformed) = ensure_valid_json_arguments(r#"{"path":"src/main.rs"}"#);
        assert_eq!(res, r#"{"path":"src/main.rs"}"#);
        assert!(!malformed);
    }

    #[test]
    fn test_ensure_valid_json_arguments_empty() {
        let (res, malformed) = ensure_valid_json_arguments("");
        assert_eq!(res, "{}");
        assert!(!malformed);
    }

    #[test]
    fn test_ensure_valid_json_arguments_truncated() {
        let (res, malformed) = ensure_valid_json_arguments(r#"{"command":"cd /tmp && cat"#);
        assert!(malformed);
        let parsed: serde_json::Value = serde_json::from_str(&res).expect("must be valid json");
        assert_eq!(parsed["_error"], "malformed_arguments");
        assert_eq!(parsed["_raw"], r#"{"command":"cd /tmp && cat"#);
    }

    #[test]
    fn test_tool_call_new_sanitizes_truncated() {
        let tc = ToolCall::new(
            "call_bad",
            crate::tool_names::TOOL_RUN_COMMAND,
            r#"{"command":"echo 'hello"#,
        );
        assert!(tc.is_malformed());
        let parsed: serde_json::Value =
            serde_json::from_str(&tc.function.arguments).expect("must be valid json");
        assert_eq!(parsed["_error"], "malformed_arguments");
    }

    #[test]
    fn test_tool_call_new_valid() {
        let tc = ToolCall::new(
            "call_ok",
            crate::tool_names::TOOL_RUN_COMMAND,
            r#"{"command":"echo hello"}"#,
        );
        assert!(!tc.is_malformed());
        assert_eq!(tc.function.arguments, r#"{"command":"echo hello"}"#);
    }
}
