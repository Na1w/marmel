//! Shared harness value types: tool invocations, results, stats, caller roles, errors.

/// A single tool execution request as parsed from a ToolCall.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// The result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    /// Whether this was an error result.
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Resilience intervention counters tracked across the session.
#[derive(Debug, Default)]
pub struct HarnessStats {
    /// Number of text repetition loops truncated.
    pub repetition_breaks: std::sync::atomic::AtomicU64,
    /// Number of empty model responses recovered by nudge.
    pub empty_prods: std::sync::atomic::AtomicU64,
    /// Number of automated context prunings executed.
    pub context_compactions: std::sync::atomic::AtomicU64,
    /// Number of plain-text XML tool calls converted to JSON.
    pub xml_tool_rescues: std::sync::atomic::AtomicU64,
    /// Number of HTTP 503/502 retries performed.
    pub backend_retries: std::sync::atomic::AtomicU64,
    /// Number of rebirth checkpoints generated.
    pub session_rebirths: std::sync::atomic::AtomicU64,
    /// Number of steer-arbitrator decisions produced.
    pub steer_arbitrations: std::sync::atomic::AtomicU64,
}

impl HarnessStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_compaction(&self) {
        self.context_compactions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_rebirth(&self) {
        self.session_rebirths
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_repetition_break(&self) {
        self.repetition_breaks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_empty_prod(&self) {
        self.empty_prods
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_xml_rescue(&self) {
        self.xml_tool_rescues
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_backend_retry(&self) {
        self.backend_retries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_steer_arbitration(&self) {
        self.steer_arbitrations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The role requesting a tool execution, used to enforce the orchestration tool policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCaller {
    /// The Manager.
    Manager,
    /// A specialist identified by its role, using default registry allowlist.
    Specialist(crate::agents::Agent),
    /// A specialist whose allowed tools are governed explicitly by its prompt/blueprint.
    SpecialistWithTools {
        agent: crate::agents::Agent,
        allowed_tools: Vec<String>,
    },
}

impl ToolCaller {
    pub fn role_name(&self) -> String {
        match self {
            ToolCaller::Manager => "Manager".to_string(),
            ToolCaller::Specialist(a) => a.as_str().to_string(),
            ToolCaller::SpecialistWithTools { agent, .. } => agent.as_str().to_string(),
        }
    }

    pub fn agent(&self) -> Option<crate::agents::Agent> {
        match self {
            ToolCaller::Manager => None,
            ToolCaller::Specialist(a) => Some(*a),
            ToolCaller::SpecialistWithTools { agent, .. } => Some(*agent),
        }
    }

    pub fn allowed_tools(&self) -> Option<&[String]> {
        match self {
            ToolCaller::SpecialistWithTools { allowed_tools, .. } => Some(allowed_tools.as_slice()),
            _ => None,
        }
    }
}

impl std::fmt::Display for ToolCaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolCaller::Manager => write!(f, "Manager"),
            ToolCaller::Specialist(a) => write!(f, "Specialist({a:?})"),
            ToolCaller::SpecialistWithTools {
                agent,
                allowed_tools,
            } => {
                write!(f, "SpecialistWithTools({agent:?}, tools={allowed_tools:?})")
            }
        }
    }
}

/// Errors that can occur while dispatching a tool.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments for {tool}: {detail}")]
    BadArguments { tool: String, detail: String },
    #[error("tool `{tool}` is forbidden for caller `{caller}` by orchestration policy")]
    Forbidden { tool: String, caller: String },
    #[error("{0}")]
    Execution(#[from] anyhow::Error),
}
