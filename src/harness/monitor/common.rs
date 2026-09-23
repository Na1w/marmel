//! Shared monitor record types.

use super::repetition::{is_pagination_tool, pagination_only_differs, semantic_json_value_eq};
use crate::types::ToolCall;

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

    /// Convert from a wire [`ToolCall`] (arguments arrive as a JSON string).
    pub fn from_tool_call(call: &ToolCall) -> Self {
        let args = serde_json::from_str(&call.function.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.function.arguments.clone()));
        Self::new(call.function.name.clone(), args)
    }

    /// Semantic equality against another record: same name and
    /// argument-JSON equality with pagination fields ignored.
    pub fn semantically_eq(&self, other: &ToolCallRecord) -> bool {
        self.name == other.name && semantic_json_value_eq(&self.arguments, &other.arguments)
    }

    /// True when the two records represent the exact same operation identity
    /// (same name and raw-identical arguments). A pagination-only difference is
    /// *not* the same operation (REQ-HARN-002 pagination exemption).
    pub fn same_operation(&self, other: &ToolCallRecord) -> bool {
        if self.name != other.name {
            return false;
        }
        // Pagination-only variation (offset/page) is explicitly exempt: it is
        // progress, not repetition.
        if is_pagination_tool(&self.name)
            && pagination_only_differs(&self.arguments, &other.arguments)
        {
            return false;
        }
        self.arguments == other.arguments
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
