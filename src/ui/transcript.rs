//! Persistent UI transcript journal for interactive sessions.
//!
//! Separates the user-facing chat and event stream from internal LLM context
//! mechanics (e.g. synthetic system prompts, auto-nudges).

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
    TaskFailed {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
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

    /// Extract paired user steering queries and their corresponding arbitrator responses.
    ///
    /// Ignores synthetic internal messages and pairs each user prompt with
    /// subsequent `SteerResponse`s, updating pending decisions when resolution occurs.
    pub fn extract_steering_history(&self) -> Vec<(String, String)> {
        let mut history: Vec<(String, String)> = Vec::new();
        let mut current_user: Option<String> = None;

        for record in &self.records {
            match record {
                UiRecord::User { text } => {
                    let trimmed = text.trim();
                    let is_synthetic = trimmed.starts_with("(SYSTEM NOTICE:")
                        || trimmed.starts_with("(SYSTEM:")
                        || trimmed.starts_with("SYSTEM:")
                        || trimmed.starts_with("[System]")
                        || trimmed.starts_with("(User steering resulted in subtask");
                    if !is_synthetic {
                        current_user = Some(text.clone());
                    }
                }
                UiRecord::SteerResponse { text } => {
                    let clean_resp = text
                        .trim()
                        .strip_prefix("[Arbitrator]:")
                        .unwrap_or(text.trim())
                        .trim()
                        .to_string();

                    if clean_resp.is_empty() {
                        continue;
                    }

                    if let Some(ref user_q) = current_user {
                        if let Some((last_q, last_resp)) = history.last_mut()
                            && last_q == user_q
                        {
                            if last_resp.contains("awaiting specialist reply")
                                || last_resp.starts_with("Decision:")
                                || last_resp.contains("följdfråga")
                                || last_resp.contains("follow-up")
                            {
                                *last_resp = clean_resp;
                            } else {
                                last_resp.push('\n');
                                last_resp.push_str(&clean_resp);
                            }
                        } else {
                            history.push((user_q.clone(), clean_resp));
                        }
                    } else {
                        history.push(("User inquiry".to_string(), clean_resp));
                    }
                }
                _ => {}
            }
        }
        history
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
                            // Gate t-059: the failure heuristic is the marker
                            // owner's stem table (`markers::has_failure_word`),
                            // not a hand-typed substring, so it sits on the same
                            // footing as `markers::starts_with_complete` next to
                            // it. `error` is tooling vocabulary, not marker
                            // vocabulary, so it stays local.
                            if crate::markers::starts_with_complete(first_line)
                                || first_line.to_lowercase().contains("error")
                                || crate::markers::has_failure_word(first_line)
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
            reason: Some("syntax error on line 42".to_string()),
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

        // Verify backwards compatibility when reason is omitted in JSON
        let legacy_json = r#"{"records":[{"type":"TaskFailed","task_id":"t-legacy"}]}"#;
        let legacy_loaded: UiTranscript = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(
            legacy_loaded.records()[0],
            UiRecord::TaskFailed {
                task_id: "t-legacy".to_string(),
                reason: None,
            }
        );
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

    #[test]
    fn test_extract_steering_history_pairs_and_resolves_pending() {
        let mut transcript = UiTranscript::new();
        transcript.append(UiRecord::User {
            text: "Build PPC JIT".to_string(),
        });
        transcript.append(UiRecord::Assistant {
            content: Some("I will implement PPC JIT.".to_string()),
            thinking: None,
        });

        // User asks steering question
        transcript.append(UiRecord::User {
            text: "Hur går det för codern?".to_string(),
        });
        transcript.append(UiRecord::SteerResponse {
            text:
                "\n[Arbitrator]: Forwarded notice notice-1 to coder — awaiting specialist reply.\n"
                    .to_string(),
        });

        // Intermediate tool logs while worker is working
        transcript.append(UiRecord::ToolCall {
            display: "run_command(ctest)".to_string(),
        });
        transcript.append(UiRecord::ToolResult {
            display: "7/7 tests passed".to_string(),
        });

        // Specialist reply synthesized for the user
        transcript.append(UiRecord::SteerResponse {
            text: "\n[Arbitrator]: Codern har tagit bort jit_invalidate_all och alla 7 tester passerar.\n"
                .to_string(),
        });

        // Second steering turn
        transcript.append(UiRecord::User {
            text: "Vad är nästa steg?".to_string(),
        });
        transcript.append(UiRecord::SteerResponse {
            text: "Kör real-ROM-test med timeout.".to_string(),
        });

        let history = transcript.extract_steering_history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, "Hur går det för codern?");
        assert_eq!(
            history[0].1,
            "Codern har tagit bort jit_invalidate_all och alla 7 tester passerar."
        );
        assert_eq!(history[1].0, "Vad är nästa steg?");
        assert_eq!(history[1].1, "Kör real-ROM-test med timeout.");
    }

    /// Regression (gate t-030): the delegated-result summary tested the first
    /// line against a hard-coded `"MISSION COMPLETE"` prefix, so every other
    /// casing of the (case-insensitive) marker degraded to "Task completed".
    /// The prefix test now comes from `crate::markers`.
    #[test]
    fn test_delegate_summary_recognises_any_casing_of_the_completion_marker() {
        let marker = crate::markers::MARKER_COMPLETE;
        for first_line in [
            format!("{marker} (t-60)"),
            format!("{} (t-60)", marker.to_lowercase()),
            format!("{} (t-60)", "Mission Complete"),
        ] {
            let messages = vec![
                crate::types::Message::System {
                    content: "system prompt".to_string(),
                },
                crate::types::Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![crate::types::ToolCall::new(
                        "call-delegate",
                        crate::tool_names::TOOL_DELEGATE_TASK,
                        "{}",
                    )],
                },
                crate::types::Message::Tool {
                    tool_call_id: "call-delegate".to_string(),
                    content: format!("{first_line}\nrest of the deliverable"),
                },
            ];
            let transcript = UiTranscript::from_legacy_messages(&messages);
            let summary = transcript
                .records()
                .iter()
                .find_map(|record| match record {
                    UiRecord::ToolResult { display } => Some(display.clone()),
                    _ => None,
                })
                .expect("a tool result record");
            assert_eq!(summary, first_line);
        }

        // A marker that is not on the first line keeps the generic summary.
        let messages = vec![
            crate::types::Message::System {
                content: "system prompt".to_string(),
            },
            crate::types::Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![crate::types::ToolCall::new(
                    "call-delegate",
                    crate::tool_names::TOOL_DELEGATE_TASK,
                    "{}",
                )],
            },
            crate::types::Message::Tool {
                tool_call_id: "call-delegate".to_string(),
                content: format!("Work finished.\n\n{marker} (t-61)"),
            },
        ];
        let transcript = UiTranscript::from_legacy_messages(&messages);
        let summary = transcript
            .records()
            .iter()
            .find_map(|record| match record {
                UiRecord::ToolResult { display } => Some(display.clone()),
                _ => None,
            })
            .expect("a tool result record");
        assert_eq!(summary, "Task completed");
    }

    /// Gate t-059 byte-pin: the failure half of the delegated-result summary is
    /// the marker owner's stem table (`crate::markers::has_failure_word`) rather
    /// than a hand-typed substring, and the rendered summary is unchanged — a
    /// failure-shaped first line is echoed verbatim, anything else collapses to
    /// `"Task completed"`.
    #[test]
    fn test_delegate_summary_failure_heuristic_is_marker_owned_and_byte_pinned() {
        let summary_for = |first_line: &str| -> String {
            let messages = vec![
                crate::types::Message::System {
                    content: "system prompt".to_string(),
                },
                crate::types::Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![crate::types::ToolCall::new(
                        "call-delegate",
                        crate::tool_names::TOOL_DELEGATE_TASK,
                        "{}",
                    )],
                },
                crate::types::Message::Tool {
                    tool_call_id: "call-delegate".to_string(),
                    content: format!("{first_line}\nrest of the deliverable"),
                },
            ];
            UiTranscript::from_legacy_messages(&messages)
                .records()
                .iter()
                .find_map(|record| match record {
                    UiRecord::ToolResult { display } => Some(display.clone()),
                    _ => None,
                })
                .expect("a tool result record")
        };

        // Failure- and error-shaped first lines are echoed byte-for-byte.
        for first_line in [
            "FAILED (aborted)",
            "FAILED: build broke",
            "The delegation failed after three retries",
            "failure: validator rejected",
            "ERROR: upstream transport closed",
        ] {
            assert_eq!(summary_for(first_line), first_line, "line: {first_line}");
        }
        // Anything else keeps the generic summary. A `REPLAN REQUIRED` verdict is
        // one of them: this summary heuristic predates the marker grammar and only
        // ever looked at the completion prefix, `error` and the `fail` stem, so
        // widening it (e.g. to `markers::has_replan_marker`) would change rendered
        // output and is deliberately out of scope here.
        for first_line in [
            "Task finished cleanly",
            "Mission report",
            "REPLAN REQUIRED: the decomposition is wrong",
            "",
        ] {
            assert_eq!(
                summary_for(first_line),
                "Task completed",
                "line: {first_line}"
            );
        }
        // The transcript asks the *stem* predicate on purpose: it is weaker than
        // the FAILED marker, and pinning the difference stops a later "cleanup"
        // from swapping in `has_failure_marker` and changing this output.
        assert!(crate::markers::has_failure_word(
            "failure: validator rejected"
        ));
        assert!(!crate::markers::has_failure_marker(
            "failure: validator rejected"
        ));
    }
}
