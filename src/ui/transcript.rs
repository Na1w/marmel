//! Persistent UI transcript journal for interactive sessions.
//!
//! Separates the user-facing chat and event stream from internal LLM context
//! mechanics (e.g. synthetic system prompts, auto-nudges, deep-freeze prompts).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A single structured record persisted in the UI transcript journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum UiRecord {
    /// A line or prompt submitted by the human user.
    User { text: String },
    /// An assistant response turn (reasoning/thinking and/or visible content).
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
    /// A steer arbitrator response delivered directly to the user (e.g. Marmennill: ...).
    SteerResponse { text: String },
    /// A tool invocation rendered in the UI (e.g. `glob(src/auth*.rs)`).
    ToolCall { display: String },
    /// The textual result of a tool execution.
    ToolResult { display: String },
    /// Completion event for a delegated task.
    TaskCompleted { task_id: String },
    /// Failure event for a delegated task.
    TaskFailed { task_id: String },
    /// System, rebirth, or status message.
    Status { text: String },
}

/// Ordered journal of UI events representing the session's visible chat history.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiTranscript {
    records: Vec<UiRecord>,
}

impl UiTranscript {
    /// Create an empty transcript.
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// Access the underlying records slice.
    pub fn records(&self) -> &[UiRecord] {
        &self.records
    }

    /// Mutable access to the underlying records.
    pub fn records_mut(&mut self) -> &mut Vec<UiRecord> {
        &mut self.records
    }

    /// Check if the transcript has no records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Number of records in the transcript.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Append a single record to the transcript.
    pub fn append(&mut self, record: UiRecord) {
        self.records.push(record);
    }

    /// Clear all records.
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// Load the transcript from a JSON file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading UI transcript from {}", path.display()))?;
        let transcript: UiTranscript = serde_json::from_str(&content)
            .with_context(|| format!("parsing UI transcript from {}", path.display()))?;
        Ok(transcript)
    }

    /// Save the transcript to a JSON file.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serializing UI transcript")?;
        std::fs::write(path, json)
            .with_context(|| format!("writing UI transcript to {}", path.display()))?;
        Ok(())
    }

    /// Convert legacy LLM messages (`Message`) into `UiRecord`s for backward compatibility.
    ///
    /// This isolates legacy prefix checks and heuristic filtering into a single migration path.
    pub fn from_legacy_messages(messages: &[crate::types::Message]) -> Self {
        let mut records = Vec::new();
        let delegated_call_ids: std::collections::HashSet<&str> = messages
            .iter()
            .filter_map(|m| match m {
                crate::types::Message::Assistant { tool_calls, .. } => Some(tool_calls),
                _ => None,
            })
            .flatten()
            .filter(|call| call.function.name == crate::tool_names::TOOL_DELEGATE_TASK)
            .map(|call| call.id.as_str())
            .collect();

        for msg in messages.iter().skip(1) {
            match msg {
                crate::types::Message::System { content } => {
                    if content.starts_with("(SYSTEM: REBIRTH CHECKPOINT:") {
                        let summary = content
                            .strip_prefix("(SYSTEM: REBIRTH CHECKPOINT:")
                            .unwrap_or(content)
                            .trim_end_matches(')');
                        records.push(UiRecord::Status {
                            text: format!("Rebirth checkpoint:{summary}"),
                        });
                    }
                }
                crate::types::Message::User { content } => {
                    let trimmed = content.trim_start();
                    let is_synthetic = trimmed.starts_with("(SYSTEM NOTICE:")
                        || trimmed.starts_with("(SYSTEM:")
                        || trimmed.starts_with("SYSTEM:")
                        || trimmed.starts_with("[System]")
                        || trimmed.starts_with("(User steering resulted in subtask");
                    if !is_synthetic {
                        records.push(UiRecord::User {
                            text: content.clone(),
                        });
                    }
                }
                crate::types::Message::Assistant {
                    content,
                    reasoning_content,
                    tool_calls,
                } => {
                    let has_thought = reasoning_content
                        .as_ref()
                        .is_some_and(|r| !r.trim().is_empty());
                    let has_content = content.as_ref().is_some_and(|c| !c.trim().is_empty());
                    if has_thought || has_content {
                        records.push(UiRecord::Assistant {
                            content: content.clone(),
                            thinking: reasoning_content.clone(),
                        });
                    }
                    for call in tool_calls {
                        let args_val =
                            serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                                .unwrap_or_else(|_| {
                                    serde_json::Value::String(call.function.arguments.clone())
                                });
                        records.push(UiRecord::ToolCall {
                            display: crate::ui::helpers::format_tool_call_display(
                                &call.function.name,
                                &args_val,
                            ),
                        });
                    }
                }
                crate::types::Message::Tool {
                    tool_call_id,
                    content,
                } => {
                    if delegated_call_ids.contains(tool_call_id.as_str()) {
                        let summary = if let Some(first_line) = content.lines().next() {
                            if first_line.starts_with("MISSION COMPLETE")
                                || first_line.to_lowercase().contains("error")
                                || first_line.to_lowercase().contains("fail")
                            {
                                first_line.to_string()
                            } else {
                                "Task completed".to_string()
                            }
                        } else {
                            "Task completed".to_string()
                        };
                        records.push(UiRecord::ToolResult { display: summary });
                    } else {
                        records.push(UiRecord::ToolResult {
                            display: content.clone(),
                        });
                    }
                }
            }
        }

        Self { records }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ui_transcript_roundtrip_serialization() {
        let mut transcript = UiTranscript::new();
        transcript.append(UiRecord::User {
            text: "Hello world".to_string(),
        });
        transcript.append(UiRecord::Assistant {
            content: Some("I will help with that.".to_string()),
            thinking: Some("Let's analyze the problem.".to_string()),
        });
        transcript.append(UiRecord::ToolCall {
            display: "glob(pattern: \"src/*.rs\")".to_string(),
        });
        transcript.append(UiRecord::ToolResult {
            display: "src/main.rs\nsrc/lib.rs".to_string(),
        });
        transcript.append(UiRecord::TaskCompleted {
            task_id: "t-001".to_string(),
        });
        transcript.append(UiRecord::TaskFailed {
            task_id: "t-002".to_string(),
        });
        transcript.append(UiRecord::SteerResponse {
            text: "Arbitrator response".to_string(),
        });
        transcript.append(UiRecord::Status {
            text: "Rebirth checkpoint: 50%".to_string(),
        });

        let json = serde_json::to_string_pretty(&transcript).unwrap();
        let loaded: UiTranscript = serde_json::from_str(&json).unwrap();
        assert_eq!(transcript, loaded);
    }

    #[test]
    fn test_ui_transcript_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".marmel").join(".ui_transcript.json");

        let mut transcript = UiTranscript::new();
        transcript.append(UiRecord::User {
            text: "Build feature".to_string(),
        });
        transcript.save(&path).unwrap();

        assert!(path.exists());
        let loaded = UiTranscript::load(&path).unwrap();
        assert_eq!(transcript, loaded);
    }

    #[test]
    fn test_from_legacy_messages_filters_synthetic_and_preserves_structure() {
        let messages = vec![
            crate::types::Message::System {
                content: "system prompt".to_string(),
            },
            crate::types::Message::User {
                content: "Refactor auth".to_string(),
            },
            crate::types::Message::Assistant {
                content: Some("Looking into auth.".to_string()),
                reasoning_content: Some("Checking directory.".to_string()),
                tool_calls: vec![crate::types::ToolCall::new(
                    "call-1",
                    crate::tool_names::TOOL_GLOB,
                    r#"{"pattern": "src/auth/*.rs"}"#,
                )],
            },
            crate::types::Message::Tool {
                tool_call_id: "call-1".to_string(),
                content: "src/auth/mod.rs".to_string(),
            },
            // Synthetic system messages that should be filtered:
            crate::types::Message::User {
                content: "(SYSTEM NOTICE: Active execution plan detected...)".to_string(),
            },
            crate::types::Message::User {
                content: "[System] User executed /reset.".to_string(),
            },
            crate::types::Message::User {
                content:
                    "(User steering resulted in subtask 't-1' executed by specialist 'coder'.)"
                        .to_string(),
            },
            // Rebirth checkpoint:
            crate::types::Message::System {
                content: "(SYSTEM: REBIRTH CHECKPOINT: Completed auth refactor)".to_string(),
            },
        ];

        let transcript = UiTranscript::from_legacy_messages(&messages);
        let records = transcript.records();

        assert_eq!(records.len(), 5);
        assert_eq!(
            records[0],
            UiRecord::User {
                text: "Refactor auth".to_string()
            }
        );
        assert_eq!(
            records[1],
            UiRecord::Assistant {
                content: Some("Looking into auth.".to_string()),
                thinking: Some("Checking directory.".to_string()),
            }
        );
        assert_eq!(
            records[2],
            UiRecord::ToolCall {
                display: "glob(src/auth/*.rs)".to_string()
            }
        );
        assert_eq!(
            records[3],
            UiRecord::ToolResult {
                display: "src/auth/mod.rs".to_string()
            }
        );
        assert_eq!(
            records[4],
            UiRecord::Status {
                text: "Rebirth checkpoint: Completed auth refactor".to_string()
            }
        );
    }
}
