//! In-memory UI transcript journal for interactive sessions.
//!
//! Separates the user-facing chat and event stream from internal LLM context
//! mechanics (e.g. synthetic system prompts, auto-nudges).
//!
//! t-025b deleted this journal's persistence owner: `UiTranscript::save`,
//! `UiTranscript::load`, `UiTranscript::from_legacy_messages` and the
//! `.ui_transcript.json` file name are gone, and nothing writes a UI journal file
//! any more. The session bundle's `ui` member ([`crate::session::format::ui`]) is
//! the only durable copy; this journal stays the live in-memory chat record and the
//! steering-replay source. The serde derives survive because the bundle's
//! `journal` section ([`crate::session::format::ui::UiJournalRecord`]) is the
//! one-for-one persistence twin of [`UiRecord`].

use serde::{Deserialize, Serialize};

/// A single structured UI event in the journal.
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
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    pub fn records(&self) -> &[UiRecord] {
        &self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn append(&mut self, record: UiRecord) {
        self.records.push(record);
    }

    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// Extract paired user steering queries and their corresponding arbitrator responses.
    ///
    /// Ignores synthetic internal messages and pairs each user prompt with
    /// subsequent `SteerResponse`s, updating pending decisions when resolution occurs.
    ///
    /// "Is this entry still pending?" is **not** decided here: it is delegated to
    /// [`crate::orchestrator::steer::steering_entry_is_pending`], the predicate the
    /// live steering-history owner (`orchestrator::bus::record_steering_exchange`)
    /// applies to the very same text. One vocabulary therefore governs both the
    /// live history and this journal replay, and none of its markers is tied to a
    /// natural language (t-013 removed the Swedish marker that used to sit here,
    /// which only matched a Swedish production prompt).
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
                            if crate::orchestrator::steer::steering_entry_is_pending(last_resp) {
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
    fn test_extract_steering_history_pairs_and_resolves_pending() {
        let mut transcript = UiTranscript::new();
        transcript.append(UiRecord::User {
            text: "Build PPC JIT".to_string(),
        });
        transcript.append(UiRecord::Assistant {
            content: Some("I will implement PPC JIT.".to_string()),
            thinking: None,
        });

        // The pending placeholder below is what makes the merge arm fire: the
        // resolved answer that follows replaces it, so the question keeps a
        // single history entry.
        transcript.append(UiRecord::User {
            text: "How is the coder doing?".to_string(),
        });
        transcript.append(UiRecord::SteerResponse {
            text:
                "\n[Arbitrator]: Forwarded notice notice-1 to coder — awaiting specialist reply.\n"
                    .to_string(),
        });

        transcript.append(UiRecord::ToolCall {
            display: "run_command(ctest)".to_string(),
        });
        transcript.append(UiRecord::ToolResult {
            display: "7/7 tests passed".to_string(),
        });

        transcript.append(UiRecord::SteerResponse {
            text: "\n[Arbitrator]: The coder removed jit_invalidate_all and all 7 tests pass.\n"
                .to_string(),
        });

        transcript.append(UiRecord::User {
            text: "What is the next step?".to_string(),
        });
        transcript.append(UiRecord::SteerResponse {
            text: "Run the real-ROM test with a timeout.".to_string(),
        });

        let history = transcript.extract_steering_history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, "How is the coder doing?");
        assert_eq!(
            history[0].1,
            "The coder removed jit_invalidate_all and all 7 tests pass."
        );
        assert_eq!(history[1].0, "What is the next step?");
        assert_eq!(history[1].1, "Run the real-ROM test with a timeout.");
    }

    /// Pin (t-013): the pending-decision merge keys off the orchestrator's
    /// language-neutral marker vocabulary, not off a wording. The placeholder fed
    /// in here is the `user_status` example that the arbitrator prompt in
    /// `orchestrator::notice` actually invites the model to emit, read back off
    /// that prompt — producer and consumer are pinned together, so re-wording the
    /// prompt out of the vocabulary fails here instead of silently duplicating
    /// steering-history entries.
    #[test]
    fn test_extract_steering_history_merges_the_pending_status_the_prompt_invites() {
        let pending = crate::orchestrator::notice::worker_reply_user_status_example();
        assert!(
            crate::orchestrator::steer::steering_entry_is_pending(pending),
            "the prompt's own user_status example must read as a pending decision"
        );

        let mut transcript = UiTranscript::new();
        transcript.append(UiRecord::User {
            text: "Is the file-format migration safe?".to_string(),
        });
        transcript.append(UiRecord::SteerResponse {
            text: format!("\n[Arbitrator]: {pending}\n"),
        });
        transcript.append(UiRecord::SteerResponse {
            text: "\n[Arbitrator]: The coder confirmed older files still parse.\n".to_string(),
        });

        let history = transcript.extract_steering_history();
        assert_eq!(history.len(), 1, "one question keeps one entry");
        assert_eq!(history[0].0, "Is the file-format migration safe?");
        assert_eq!(
            history[0].1, "The coder confirmed older files still parse.",
            "the resolved answer replaces the pending placeholder"
        );
    }

    /// Regression (t-013): the merge used to fire on a Swedish literal in the
    /// arbitrator status line. The vocabulary is language-neutral now, so a
    /// Swedish-only placeholder is no longer treated as pending and the two
    /// answers are concatenated instead — the accepted, cosmetic cost of dropping
    /// that arm, pinned so it cannot silently widen.
    ///
    /// The Swedish is spelled with `\u{…}` escapes so this file stays free of
    /// non-ASCII literals; the escapes compile to the same byte string.
    #[test]
    fn test_extract_steering_history_does_not_merge_a_swedish_only_pending_status() {
        let pending =
            "St\u{e4}ller en f\u{f6}ljdfra\u{e5}ga till Coder f\u{f6}r att reda ut detaljerna...";
        assert!(
            !crate::orchestrator::steer::steering_entry_is_pending(pending),
            "the pending vocabulary must not match Swedish prose"
        );

        let mut transcript = UiTranscript::new();
        transcript.append(UiRecord::User {
            text: "Is the file-format migration safe?".to_string(),
        });
        transcript.append(UiRecord::SteerResponse {
            text: format!("\n[Arbitrator]: {pending}\n"),
        });
        transcript.append(UiRecord::SteerResponse {
            text: "\n[Arbitrator]: The coder confirmed older files still parse.\n".to_string(),
        });

        let history = transcript.extract_steering_history();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].1,
            format!("{pending}\nThe coder confirmed older files still parse.")
        );
    }
}
