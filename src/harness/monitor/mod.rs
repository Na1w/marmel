//! Loop monitor: XML tool rescue, semantic repetition detection, cycle breaking
//! and text repetition breaking (REQ-HARN-001…004).
//!
//! This module is responsible for the resilience harness that keeps an agent
//! loop productive:
//!
//! * [`XMLToolRescue`] recovers tool calls the LLM emitted as plain-text XML
//!   instead of structured JSON (REQ-HARN-001).
//! * [`ToolRepetitionDetector`] tracks the last 50 executed tool calls using
//!   semantic JSON equality (ignoring argument key ordering and pagination
//!   fields) and blocks repetitions / cuts alternating cycles (REQ-HARN-002).
//! * [`RepetitionDetector`] watches a 16384-character rolling buffer of streamed
//!   assistant output for text that repeats itself ≥5 times (REQ-HARN-003).
//!
//! Every intervention is recorded atomically in the shared [`HarnessStats`]
//! registry (REQ-HARN-004).

mod code;
mod common;
mod repetition;
mod xml;

use super::HarnessStats;
use crate::types::{Message, ToolCall};

pub use common::{Intervention, ToolCallRecord};
pub use repetition::{RepetitionDetector, ToolRepetitionDetector, semantic_json_eq};
pub use xml::XMLToolRescue;

// ---------------------------------------------------------------------------
// Orphan tool-message pruning (Part 7 checklist item 5)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Composed resilience monitor (runtime entry point)
// ---------------------------------------------------------------------------

/// Composed harness monitor binding all three detectors to a shared stats
/// registry, so the loop calls ONE object for every resilience intervention.
///
/// This is the runtime facade that makes REQ-HARN-001…004 *active* in the
/// live loop (Manager turn loop and specialist delegated turns alike) rather
/// than standalone, unit-tested-only components:
///
/// * [`HarnessMonitor::rescue_xml`] — REQ-HARN-001. Run on the raw assistant
///   text so plain-text XML tool calls are intercepted and reconstructed as
///   structured [`ToolCall`] JSON with `call_text_{uuid}` ids; increments
///   `xml_tool_rescues`.
/// * [`HarnessMonitor::observe_tool`] — REQ-HARN-002. Run *before* a tool
///   executes so a ≥3 identical repetition blocks, or an ≥3 alternating cycle
///   cuts, the call; the returned [`Intervention`] drives the SPEC error text.
/// * [`HarnessMonitor::feed_text`] — REQ-HARN-003. Feed streamed assistant
///   output into the 1000-char rolling buffer; returns `true` (terminating the
///   stream) when a ≥5-length pattern repeats ≥5 times, and truncates the
///   repeated block back to a single instance while incrementing
///   `repetition_breaks`.
///
/// The same stats registry is shared so interventions are aggregated across
/// the whole session (REQ-HARN-004); per-agent isolation is preserved because
/// each specialist's `AgentLoop` holds its own monitor instance rooted at the
/// shared `HarnessStats` (aggregate session counters), matching the "stats
/// aggregated per session, cognitive context isolated per agent" model.
#[derive(Debug)]

pub struct HarnessMonitor {
    /// XML tool rescue (REQ-HARN-001).
    xml: XMLToolRescue,
    /// Semantic repetition & cycle detector (REQ-HARN-002).
    tool_rep: ToolRepetitionDetector,
    /// Text repetition detector (REQ-HARN-003).
    text_rep: RepetitionDetector,
    /// Shared session intervention counters (REQ-HARN-004).
    stats: std::sync::Arc<HarnessStats>,
    /// True between the moment a text repetition break fires and the stream is
    /// restarted, so a single break increments the counter exactly once
    /// (REQ-HARN-003). Reset via [`HarnessMonitor::reset_text_break`].
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

    /// REQ-HARN-001: intercept any plain-text XML tool calls in `text` and
    /// convert them into structured [`ToolCall`] JSON with `call_text_{uuid}`
    /// ids, routing them to execution. Increments `xml_tool_rescues` in the
    /// shared stats. Returns the rescued calls (empty when none).
    pub fn rescue_xml(&self, text: &str) -> Vec<ToolCall> {
        self.xml.rescue(text)
    }

    /// REQ-HARN-002: record a tool call (by name + JSON arguments) and return
    /// the intervention. Call this immediately before dispatching so a ≥3
    /// identical repetition returns [`Intervention::Block`] and an ≥3
    /// alternating cycle returns [`Intervention::Cut`]. The caller maps the
    /// intervention to the exact SPEC error string.
    pub fn observe_tool(&mut self, name: &str, arguments: &serde_json::Value) -> Intervention {
        let record = ToolCallRecord::new(name.to_string(), arguments.clone());
        self.tool_rep.evaluate(record)
    }

    /// The exact SPEC error payload for an [`Intervention`] (REQ-HARN-002).
    /// Returns `None` for [`Intervention::None`]. The message reflects the
    /// configured repetition threshold.
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

    /// REQ-HARN-003: feed a chunk of streamed assistant output into the rolling
    /// 1000-char buffer. Returns `true` when the stream must be terminated
    /// because a pattern of length ≥5 repeated ≥5 times continuously at the
    /// tail. When it fires, the repeated block is truncated back to a single
    /// instance (buffer reset) and `repetition_breaks` is incremented exactly
    /// once.
    pub fn feed_text(&mut self, chunk: &str) -> bool {
        self.text_rep.push(chunk);
        if !self.repetition_fired && self.text_rep.is_repeating() {
            self.repetition_fired = true;
            self.stats.record_repetition_break();
            // Truncate the repeated block down to a single instance so a fresh
            // generation can resume from a clean tail (REQ-HARN-003). Preserve
            // the configured thresholds.
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

// ---------------------------------------------------------------------------
// Unit tests (Phase B checkpoint: `cargo test --lib test_monitor_`)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Unit tests (Phase B checkpoint: `cargo test --lib test_monitor_`)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;
