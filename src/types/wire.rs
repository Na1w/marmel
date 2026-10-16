//! Wire types for the OpenAI-compatible chat completions API and tool calls.

use serde::{Deserialize, Serialize};

use super::tools::ToolDef;

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
    /// Optional reasoning-effort dial (free-form; backends use different vocabularies).
    /// Emitted only when set. `max_thinking_tokens` remains the independent hard cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
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

    /// `ChatRequest` has no `Default`, so every literal stays exhaustive; this is
    /// the minimal body (every optional field `None`) the wire tests start from.
    fn minimal_request() -> ChatRequest {
        ChatRequest {
            model: "model-x".to_string(),
            messages: vec![Message::User {
                content: "hi".to_string(),
            }],
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stream: None,
            enable_thinking: None,
            reasoning_effort: None,
            tools: None,
        }
    }

    /// Sorted key inventory of a serialized body, for exact-shape assertions.
    fn body_keys(body: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = body
            .as_object()
            .expect("the request body is a JSON object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn chat_request_omits_reasoning_effort_when_none() {
        let req = minimal_request();
        let body = serde_json::to_value(&req).expect("request body serializes");
        assert!(
            body.get("reasoning_effort").is_none(),
            "unset reasoning_effort must not appear in the body: {body}"
        );
        assert_eq!(
            body_keys(&body),
            vec!["messages".to_string(), "model".to_string()],
            "only the always-present keys remain"
        );
        let raw = serde_json::to_string(&req).expect("request body serializes");
        assert!(
            !raw.contains("reasoning_effort"),
            "the key must not appear in the serialized string: {raw}"
        );
    }

    #[test]
    fn chat_request_emits_reasoning_effort_verbatim() {
        let mut req = minimal_request();
        req.reasoning_effort = Some("low".to_string());
        let raw = serde_json::to_string(&req).expect("request body serializes");
        assert!(
            raw.contains(r#""reasoning_effort":"low""#),
            "expected verbatim key/value pair in {raw}"
        );

        let body = serde_json::to_value(&req).expect("request body serializes");
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(
            body_keys(&body),
            vec![
                "messages".to_string(),
                "model".to_string(),
                "reasoning_effort".to_string(),
            ]
        );

        // Free-form pass-through: no enum mapping, no casing normalisation, no
        // validation of the vocabulary.
        for effort in [
            "none",
            "minimal",
            "low",
            "medium",
            "high",
            "xhigh",
            "VERY HIGH",
        ] {
            let mut req = minimal_request();
            req.reasoning_effort = Some(effort.to_string());
            assert_eq!(
                serde_json::to_value(&req).expect("serializes")["reasoning_effort"],
                effort,
                "reasoning_effort must be passed through unchanged"
            );
        }
    }

    #[test]
    fn chat_request_reasoning_effort_is_independent_of_sibling_optionals() {
        // All other optionals unset: exactly `model` + `messages`.
        let bare = minimal_request();
        let bare_body = serde_json::to_value(&bare).expect("request body serializes");
        assert_eq!(
            body_keys(&bare_body),
            vec!["messages".to_string(), "model".to_string()],
            "no optional key leaks into the body"
        );

        // `stream` is the only extra key when just it is set.
        let mut streamed = minimal_request();
        streamed.stream = Some(true);
        let streamed_body = serde_json::to_value(&streamed).expect("request body serializes");
        assert_eq!(
            body_keys(&streamed_body),
            vec![
                "messages".to_string(),
                "model".to_string(),
                "stream".to_string(),
            ]
        );

        // The binary off switch and the new dial are emitted independently.
        let mut both = minimal_request();
        both.enable_thinking = Some(false);
        both.reasoning_effort = Some("high".to_string());
        let both_body = serde_json::to_value(&both).expect("request body serializes");
        assert_eq!(both_body["enable_thinking"], false);
        assert_eq!(both_body["reasoning_effort"], "high");
        assert_eq!(
            body_keys(&both_body),
            vec![
                "enable_thinking".to_string(),
                "messages".to_string(),
                "model".to_string(),
                "reasoning_effort".to_string(),
            ]
        );
    }
}
