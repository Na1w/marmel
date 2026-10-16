//! Loop monitor: XML tool rescue, semantic repetition detection, cycle breaking
//! and text repetition breaking.
//!
//! The resilience harness that keeps an agent loop productive:
//!
//! * [`XMLToolRescue`] recovers tool calls the LLM emitted as plain-text XML
//!   instead of structured JSON.
//! * [`ToolRepetitionDetector`] tracks the last 50 executed tool calls using
//!   semantic JSON equality (ignoring argument key ordering and pagination
//!   fields) and blocks repetitions / cuts alternating cycles.
//! * [`RepetitionDetector`] watches a 16384-character rolling buffer of streamed
//!   assistant output for text that repeats itself ≥5 times.
//!
//! Every intervention is recorded atomically in the shared [`HarnessStats`]
//! registry.

mod code;
mod common;
mod repetition;
mod xml;

use super::HarnessStats;
use crate::types::{Message, ToolCall};

pub use common::{Intervention, ToolCallRecord};
pub use repetition::{RepetitionDetector, ToolRepetitionDetector, semantic_json_eq};
pub use xml::XMLToolRescue;

// Orphan tool-message pruning

/// Prune orphaned `role:"tool"` messages that have no matching assistant
/// tool_call_id.
pub fn prune_orphan_tool_messages(messages: Vec<Message>) -> Vec<Message> {
    let mut valid_ids = std::collections::HashSet::new();
    for m in &messages {
        if let Message::Assistant { tool_calls, .. } = m {
            for t in tool_calls {
                valid_ids.insert(t.id.clone());
            }
        }
    }

    messages
        .into_iter()
        .filter(|m| match m {
            Message::Tool { tool_call_id, .. } => valid_ids.contains(tool_call_id),
            _ => true,
        })
        .collect()
}

// Composed resilience monitor (runtime entry point)

/// Composed harness monitor binding all three detectors to a shared stats
/// registry, so the loop calls ONE object for every resilience intervention.
/// It is the runtime facade that makes the detectors *active* in the Manager
/// turn loop (`run_session`, `src/ui/session.rs`) and in delegated specialist
/// turns (`run_specialist_live_inner`, `src/agents/runner/execution.rs`):
/// [`HarnessMonitor::rescue_xml`] turns plain-text XML tool calls into
/// structured [`ToolCall`] JSON with `call_text_{uuid}` ids,
/// [`HarnessMonitor::observe_tool`] runs *before* each tool executes and returns
/// the [`Intervention`] that drives the SPEC error text, and
/// [`HarnessMonitor::feed_text`] feeds streamed output into the rolling text
/// buffer.
///
/// Ownership: the Manager-side monitor is rooted at the session-wide
/// [`HarnessStats`] carried by `OrchestratorManager::stats`; each specialist run
/// constructs its OWN monitor and threads it as `&mut` through the shared
/// dispatch core. No detector state (XML rescue, tool-repetition buffer,
/// text-repetition buffer) is shared between concurrent agents: stats are per
/// session, cognitive context is per agent.
#[derive(Debug)]

pub struct HarnessMonitor {
    /// XML tool rescue.
    xml: XMLToolRescue,
    /// Semantic repetition & cycle detector.
    tool_rep: ToolRepetitionDetector,
    /// Text repetition detector.
    text_rep: RepetitionDetector,
    /// Shared session intervention counters.
    stats: std::sync::Arc<HarnessStats>,
    /// True between the moment a text repetition break fires and the stream is
    /// restarted, so a single break increments the counter exactly once. Reset
    /// via [`HarnessMonitor::reset_text_break`].
    repetition_fired: bool,
}

impl HarnessMonitor {
    /// Create a monitor rooted at a shared stats registry, using the given
    /// resilience thresholds (caesar `[monitoring]` block). Pass an
    /// `Arc<HarnessStats>`; all intervention counters are recorded into it.
    pub fn new_with_config(
        stats: std::sync::Arc<HarnessStats>,
        monitoring: &crate::config::MonitoringConfig,
    ) -> Self {
        let threshold = monitoring.repetition_threshold;
        let min_len = monitoring.min_pattern_len;
        Self {
            xml: XMLToolRescue::with_stats(stats.clone()),
            tool_rep: ToolRepetitionDetector::new(threshold),
            text_rep: RepetitionDetector::new(threshold, min_len),
            stats,
            repetition_fired: false,
        }
    }

    /// Create a monitor rooted at a shared stats registry with caesar-default
    /// thresholds (`repetition_threshold = 5`, `min_pattern_len = 5`).
    pub fn new(stats: std::sync::Arc<HarnessStats>) -> Self {
        Self::new_with_config(stats, &crate::config::MonitoringConfig::default())
    }

    /// Create a monitor with a fresh, isolated stats registry (convenience for
    /// standalone / test use).
    pub fn with_new_stats() -> Self {
        Self::new(std::sync::Arc::new(HarnessStats::new()))
    }

    /// Intercept any plain-text XML tool calls in `text` and convert them into
    /// structured [`ToolCall`] JSON with `call_text_{uuid}` ids, routing them to
    /// execution. Increments `xml_tool_rescues` in the shared stats. Returns the
    /// rescued calls (empty when none).
    pub fn rescue_xml(&self, text: &str) -> Vec<ToolCall> {
        self.xml.rescue(text)
    }

    /// Record a tool call (by name + JSON arguments) and return the
    /// intervention. Call this immediately before dispatching so a ≥5 identical
    /// repetition returns [`Intervention::Block`] and an ≥5 alternating cycle
    /// returns [`Intervention::Cut`]. The caller maps the intervention to the
    /// exact SPEC error string.
    pub fn observe_tool(&mut self, name: &str, arguments: &serde_json::Value) -> Intervention {
        let record = ToolCallRecord::new(name.to_string(), arguments.clone());
        self.tool_rep.evaluate(record)
    }

    /// The exact SPEC error payload for an [`Intervention`]. Returns `None` for
    /// [`Intervention::None`]. The message reflects the configured repetition
    /// threshold.
    pub fn intervention_error(&self, intervention: Intervention) -> Option<String> {
        match intervention {
            Intervention::Block => {
                let last_is_rebirth = self
                    .tool_rep
                    .buffer
                    .back()
                    .is_some_and(|b| b.name == crate::tool_names::TOOL_REBIRTH);
                if last_is_rebirth {
                    Some(
                        "TOOL REPETITION DETECTED: Rebirth checkpoint was already applied. You cannot invoke rebirth consecutively. Make progress using other tools."
                            .to_string(),
                    )
                } else {
                    Some(format!(
                        "TOOL REPETITION DETECTED: You have called this tool with identical \
                         arguments {} times in a row. Stop looping and try an alternative approach.",
                        self.tool_rep.threshold
                    ))
                }
            }
            Intervention::Cut => Some(
                "TOOL CYCLE DETECTED: You are repeating a loop of tool calls. Step back \
                 and re-evaluate your plan."
                    .to_string(),
            ),
            Intervention::None => None,
        }
    }

    /// Feed a chunk of streamed assistant output into the rolling 16384-char
    /// buffer. Returns `true` when the stream must be terminated because a
    /// pattern of length ≥5 repeated ≥5 times continuously at the tail. When it
    /// fires, the repeated block is truncated back to a single instance (buffer
    /// reset) and `repetition_breaks` is incremented exactly once.
    pub fn feed_text(&mut self, chunk: &str) -> bool {
        self.text_rep.push(chunk);
        if !self.repetition_fired && self.text_rep.is_repeating() {
            self.repetition_fired = true;
            self.stats.record_repetition_break();
            // Truncate the repeated block down to a single instance so a fresh
            // generation can resume from a clean tail. Preserve the configured
            // thresholds.
            let threshold = self.text_rep.threshold;
            let min_len = self.text_rep.min_len;
            self.text_rep = RepetitionDetector::new(threshold, min_len);
            return true;
        }
        false
    }

    /// Re-arm the text-repetition breaker after the stream has been restarted,
    /// allowing a new independent pattern to be detected later in the session.
    pub fn reset_text_break(&mut self) {
        self.repetition_fired = false;
    }

    /// Number of tool-call records currently in the repetition buffer.
    pub fn tool_buffer_len(&self) -> usize {
        self.tool_rep.len()
    }

    /// Access the shared stats registry (for reporting / tests).
    pub fn stats(&self) -> &std::sync::Arc<HarnessStats> {
        &self.stats
    }
}

// Unit tests (Phase B checkpoint: `cargo test --lib test_monitor_`)

// The suite itself lives in `monitor_tests.rs`, included below.

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;
