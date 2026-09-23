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

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: "function".to_string(),
            function: ToolFunction {
                name: name.into(),
                arguments: arguments.into(),
            },
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
}
