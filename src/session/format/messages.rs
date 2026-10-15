//! The loss-proof transcript record (Design A §3.7).
//!
//! The wire type `types::wire::Message` is **not** reusable for persistence:
//! `serialize_assistant_content` (`src/types/wire.rs:30-41`) writes `None` as
//! `""`, and `""` reads back as `Some("")`, so a tool-only assistant turn cannot
//! survive a JSON round-trip through it. `MessageRecord` mirrors the wire tag
//! layout (`#[serde(tag = "role", rename_all = "lowercase")]`) but serializes
//! `content` **without** `skip_serializing_if` and without any `serialize_with`,
//! so `None` is JSON `null` and `""` is JSON `""` — permanently distinct facts.

use serde::{Deserialize, Serialize};

use crate::types::{Message, ToolCall, ToolFunction};

/// Durable mirror of the wire `Message` tag layout that survives a JSON
/// round-trip **exactly**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase", deny_unknown_fields)]
pub enum MessageRecord {
    #[serde(rename = "system")]
    System { content: String },
    #[serde(rename = "user")]
    User { content: String },
    #[serde(rename = "assistant")]
    Assistant {
        /// `null` = the model produced no content (tool-only turn).
        /// `""`  = the model produced an empty content string. Not the same fact.
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCallRecord>,
    },
    #[serde(rename = "tool")]
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// Verbatim mirror of `ToolCall` (`src/types/wire.rs:73-80`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallRecord {
    pub id: String,
    /// Wire key is `"type"`, default `"function"` — same spelling as the wire
    /// type so a bundle diff against the legacy transcript reads cleanly.
    #[serde(rename = "type", default = "default_tool_type")]
    pub kind: String,
    pub function: ToolFunctionRecord,
}

fn default_tool_type() -> String {
    "function".to_string()
}

/// `arguments` is an **opaque JSON string** (`src/types/wire.rs:154-159`) and is
/// persisted verbatim. Two prohibited operations, both of which silently rewrite
/// history: `serde_json::from_str` → re-serialize (re-canonicalizes), and any
/// route through `ensure_valid_json_arguments` / `ToolCall::sanitize_arguments`,
/// which *mutates* malformed arguments into `{"_error":"malformed_arguments",…}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFunctionRecord {
    pub name: String,
    pub arguments: String,
}

impl MessageRecord {
    /// In-memory conversion, field by field.
    ///
    /// MUST NOT be implemented by serializing a `Message` to JSON and reading it
    /// back — that path runs through `serialize_assistant_content` and destroys
    /// the `None`/`""` distinction on the way in.
    pub fn from_wire(msg: &Message) -> Self {
        match msg {
            Message::System { content } => MessageRecord::System {
                content: content.clone(),
            },
            Message::User { content } => MessageRecord::User {
                content: content.clone(),
            },
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => MessageRecord::Assistant {
                content: content.clone(),
                reasoning_content: reasoning_content.clone(),
                tool_calls: tool_calls.iter().map(ToolCallRecord::from_wire).collect(),
            },
            Message::Tool {
                tool_call_id,
                content,
            } => MessageRecord::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
            },
        }
    }

    /// Reverse conversion for the request builder. `None` stays `None`, so the
    /// wire bytes a restored session sends are the bytes the pre-crash process
    /// would have sent. Arguments are copied verbatim — no sanitizing, no
    /// re-canonicalizing (`ensure_valid_json_arguments` mutates history).
    pub fn into_wire(self) -> Message {
        match self {
            MessageRecord::System { content } => Message::System { content },
            MessageRecord::User { content } => Message::User { content },
            MessageRecord::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => Message::Assistant {
                content,
                reasoning_content,
                tool_calls: tool_calls
                    .into_iter()
                    .map(ToolCallRecord::into_wire)
                    .collect(),
            },
            MessageRecord::Tool {
                tool_call_id,
                content,
            } => Message::Tool {
                tool_call_id,
                content,
            },
        }
    }

    /// Role tag as spelled on the wire (`"system"` / `"user"` / `"assistant"` /
    /// `"tool"`), used by pin verification and by report text.
    pub fn role(&self) -> &'static str {
        match self {
            MessageRecord::System { .. } => "system",
            MessageRecord::User { .. } => "user",
            MessageRecord::Assistant { .. } => "assistant",
            MessageRecord::Tool { .. } => "tool",
        }
    }

    /// The record's text content, `None` for a content-less assistant turn.
    pub fn content(&self) -> Option<&str> {
        match self {
            MessageRecord::System { content } | MessageRecord::User { content } => {
                Some(content.as_str())
            }
            MessageRecord::Assistant { content, .. } => content.as_deref(),
            MessageRecord::Tool { content, .. } => Some(content.as_str()),
        }
    }
}

impl ToolCallRecord {
    pub fn from_wire(tc: &ToolCall) -> Self {
        ToolCallRecord {
            id: tc.id.clone(),
            kind: tc.kind.clone(),
            function: ToolFunctionRecord {
                name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
            },
        }
    }

    pub fn into_wire(self) -> ToolCall {
        ToolCall {
            id: self.id,
            kind: self.kind,
            function: ToolFunction {
                name: self.function.name,
                arguments: self.function.arguments,
            },
        }
    }

    /// `true` when the arguments carry the `{"_error":"malformed_arguments",…}`
    /// payload. A **read-out**, never a repair: a record that already carries it
    /// keeps it unchanged (A §3.7).
    pub fn is_malformed(&self) -> bool {
        match serde_json::from_str::<serde_json::Value>(&self.function.arguments) {
            Ok(serde_json::Value::Object(map)) => {
                map.contains_key("_error")
                    && map.get("_error").and_then(serde_json::Value::as_str)
                        == Some("malformed_arguments")
            }
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(content: Option<&str>, calls: &[(&str, &str, &str)]) -> MessageRecord {
        MessageRecord::Assistant {
            content: content.map(str::to_string),
            reasoning_content: None,
            tool_calls: calls
                .iter()
                .map(|(id, name, args)| ToolCallRecord {
                    id: (*id).to_string(),
                    kind: "function".to_string(),
                    function: ToolFunctionRecord {
                        name: (*name).to_string(),
                        arguments: (*args).to_string(),
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn assistant_none_and_empty_string_are_different_facts() {
        let none = record(None, &[]);
        let empty = record(Some(""), &[]);

        let none_json = serde_json::to_string(&none).expect("serializes");
        let empty_json = serde_json::to_string(&empty).expect("serializes");

        assert!(
            none_json.contains("\"content\":null"),
            "`None` must be JSON `null`, got {none_json}"
        );
        assert!(
            empty_json.contains("\"content\":\"\""),
            "`Some(\"\")` must be JSON `\"\"`, got {empty_json}"
        );
        assert_ne!(none_json, empty_json, "the two facts must not collide");

        let back: MessageRecord = serde_json::from_str(&none_json).expect("parses");
        assert_eq!(back, none, "`None` must read back as `None`");
        let back: MessageRecord = serde_json::from_str(&empty_json).expect("parses");
        assert_eq!(back, empty, "`Some(\"\")` must read back as `Some(\"\")`");
    }

    #[test]
    fn wire_round_trip_degrades_none_but_record_does_not() {
        // The trap this type exists to close: the wire type collapses `None`.
        let none = record(None, &[]);
        let wire = none.clone().into_wire();
        let wire_json = serde_json::to_string(&wire).expect("serializes");
        assert!(
            wire_json.contains("\"content\":\"\""),
            "the wire serializer collapses `None` to `\"\"` (wire.rs:30-41): {wire_json}"
        );

        // The record path keeps it.
        let back = MessageRecord::from_wire(&wire);
        match back {
            MessageRecord::Assistant { content, .. } => {
                assert!(
                    content.is_none(),
                    "field-by-field conversion must keep `None`"
                );
            }
            other => panic!("expected an assistant record, got {other:?}"),
        }
    }

    #[test]
    fn arguments_are_opaque_and_never_rewritten() {
        // Malformed arguments stay verbatim: no sanitize, no re-canonicalize.
        let raw = "{ not json ";
        let rec = record(
            Some("thinking"),
            &[("call_1", crate::tool_names::TOOL_RUN_COMMAND, raw)],
        );
        let json = serde_json::to_string(&rec).expect("serializes");
        let back: MessageRecord = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, rec, "arguments must round-trip byte-exactly");
        if let MessageRecord::Assistant { tool_calls, .. } = &back {
            assert_eq!(tool_calls[0].function.arguments, raw);
            assert!(tool_calls[0].is_malformed());
            assert_eq!(tool_calls[0].kind, "function");
        } else {
            panic!("expected an assistant record");
        }
    }

    #[test]
    fn unknown_field_is_a_hard_parse_error() {
        // A §7.3 L3: `deny_unknown_fields` on every durable struct means a
        // bundle written by a newer release is refused, never partially read.
        let bad = r#"{"role":"user","content":"hi","extra":1}"#;
        let err = serde_json::from_str::<MessageRecord>(bad)
            .expect_err("an unknown field must not be silently ignored");
        assert!(
            err.to_string().contains("unknown field"),
            "expected an unknown-field error, got {err}"
        );
    }
}
