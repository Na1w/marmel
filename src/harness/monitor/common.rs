//! Shared monitor record types.

use super::repetition::semantic_json_value_eq;

/// A single tool call record tracked by the semantic repetition detector
/// (REQ-HARN-002). Equality is *semantic*: argument key ordering is ignored
/// and pagination fields (`offset`, `page`) are excluded.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub arguments: serde_json::Value,
}

impl ToolCallRecord {
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }

    /// Semantic equality against another record: same name and
    /// argument-JSON equality with pagination fields ignored.
    pub fn semantically_eq(&self, other: &ToolCallRecord) -> bool {
        self.name == other.name && semantic_json_value_eq(&self.arguments, &other.arguments)
    }
}

/// Outcome of evaluating a freshly recorded tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intervention {
    /// No intervention; the call is allowed to proceed.
    None,
    /// The call was an identical repetition ≥3 times: execution is blocked.
    Block,
    /// An alternating cycle repeated ≥3 times: execution is cut.
    Cut,
}
