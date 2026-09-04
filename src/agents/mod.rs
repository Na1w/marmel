//! Specialist Subagents — isolated-context, run-to-completion workers.

pub mod coder;
pub mod debugger;
pub mod generalist;
pub mod researcher;
pub mod runner;
pub mod validation;
pub mod validator;

pub use coder::Coder;
pub use debugger::Debugger;
pub use generalist::Generalist;
pub use researcher::Researcher;
pub use runner::run_specialist_live;
pub(crate) use runner::run_specialist_llm;
pub use validation::ValidationOutcome;
pub use validator::Validator;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Canonical specialist role ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    /// Lead Software Engineer — system architecture, refactoring, code implementation.
    Coder,
    /// Deep Knowledge & Archival — ZIM, PDF, historical archives, statistics.
    Researcher,
    /// Low-Level Systems Debugger — crash forensics, PTY GDB/LLDB, ABI/codegen.
    Debugger,
    /// Independent Quality Auditor — verification, test suites, formal verdicts.
    Validator,
    /// Supreme Polymath — dense reasoning and cross-domain logic.
    Generalist,
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Agent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Agent::Coder => "coder",
            Agent::Researcher => "researcher",
            Agent::Debugger => "debugger",
            Agent::Validator => "validator",
            Agent::Generalist => "generalist",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "coder" => Some(Self::Coder),
            "researcher" => Some(Self::Researcher),
            "debugger" => Some(Self::Debugger),
            "validator" => Some(Self::Validator),
            "generalist" | "deepbrain" => Some(Self::Generalist),
            _ => None,
        }
    }
}

impl std::str::FromStr for Agent {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_str(s).ok_or(())
    }
}

/// Terminal outcome a specialist returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionMarker {
    /// Task fully satisfied. `task_id` matches the plan line to auto-check.
    Complete { task_id: Option<String> },
    /// Task could not be completed; report reason + partial result.
    Failed { reason: String },
    /// Task could not be completed AND the plan/goal needs revisiting.
    Replan { reason: String },
}

static TASK_ID_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

fn find_task_id(text: &str) -> Option<&str> {
    let re = TASK_ID_RE.get_or_init(|| {
        regex::Regex::new(r"\(?\[?(t-[A-Za-z0-9_-]+)\]?\)?").expect("valid task regex")
    });
    re.captures(text).map(|c| c.get(1).unwrap().as_str())
}

impl MissionMarker {
    pub fn parse(text: &str) -> Option<MissionMarker> {
        let upper = text.to_ascii_uppercase();
        if upper.contains("REPLAN REQUIRED") {
            return Some(MissionMarker::Replan {
                reason: text.to_string(),
            });
        }
        if upper.contains("FAILED") {
            return Some(MissionMarker::Failed {
                reason: text.to_string(),
            });
        }
        if upper.contains("MISSION COMPLETE") {
            let task_id = find_task_id(text).map(|t| t.to_string());
            return Some(MissionMarker::Complete { task_id });
        }
        None
    }
}

/// The `delegate_task` argument payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRequest {
    /// Specialist role.
    pub agent_name: Agent,
    /// Self-contained task brief in English.
    pub prompt: String,
    /// Bounded list of relevant excerpts or file paths.
    #[serde(default)]
    pub snippets: Vec<String>,
    /// Optional execution plan task id enabling automatic check-off.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Optional image references for multimodal specialists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_urls: Option<Vec<String>>,
    /// Optional audio references for audio specialists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_urls: Option<Vec<String>>,
    /// Whether the task brief explicitly grants recursion.
    #[serde(default)]
    pub recursion_granted: bool,
}

/// The isolated context handed to a specialist.
#[derive(Debug, Clone)]
pub struct IsolatedContext {
    pub role_system_prompt: String,
    pub brief: String,
    pub task_id: Option<String>,
    pub snippets: Vec<String>,
    pub image_urls: Vec<String>,
    pub audio_urls: Vec<String>,
}

impl IsolatedContext {
    pub fn from_request(role_system_prompt: String, req: &DelegationRequest) -> Self {
        Self {
            role_system_prompt,
            brief: req.prompt.clone(),
            task_id: req.task_id.clone(),
            snippets: req.snippets.clone(),
            image_urls: req.image_urls.clone().unwrap_or_default(),
            audio_urls: req.audio_urls.clone().unwrap_or_default(),
        }
    }

    pub fn into_engine(&self, max_context_tokens: usize) -> crate::agent::ContextEngine {
        let factory = crate::agent::ContextEngineFactory::new(max_context_tokens);
        factory.specialist_context(self.role_system_prompt.clone(), self.brief.clone())
    }
}

/// A specialist's single returned deliverable.
#[derive(Debug, Clone)]
pub struct Deliverable {
    pub marker: MissionMarker,
    pub content: String,
    pub task_id: Option<String>,
}

/// The canonical Specialist trait.
#[async_trait]
pub trait Specialist: Send + Sync + fmt::Debug {
    fn name(&self) -> Agent;
    fn tool_namespaces(&self) -> &[&'static str];

    async fn run(
        &self,
        ctx: &IsolatedContext,
        token: &tokio_util::sync::CancellationToken,
    ) -> Deliverable {
        if token.is_cancelled() {
            return Deliverable {
                marker: MissionMarker::Failed {
                    reason: "aborted".to_string(),
                },
                content: "Task aborted by user instruction.\n\nFAILED (aborted)".to_string(),
                task_id: ctx.task_id.clone(),
            };
        }
        let content = run_specialist_llm(self.name(), ctx, token).await;
        let marker = MissionMarker::parse(&content).unwrap_or_else(|| MissionMarker::Failed {
            reason: "no terminal marker".to_string(),
        });
        Deliverable {
            marker,
            content,
            task_id: ctx.task_id.clone(),
        }
    }

    fn may_recurse(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::runner::assemble_final_deliverable;
    use super::*;

    #[test]
    fn test_agent_display_and_parse() {
        assert_eq!(Agent::Coder.to_string(), "coder");
        assert_eq!(Agent::from_str("debugger"), Some(Agent::Debugger));
        assert_eq!(Agent::from_str("researcher"), Some(Agent::Researcher));
        assert_eq!(Agent::from_str("validator"), Some(Agent::Validator));
        assert_eq!(Agent::from_str("generalist"), Some(Agent::Generalist));
        assert_eq!(Agent::from_str("deepbrain"), Some(Agent::Generalist));
        assert_eq!(Agent::from_str("unknown"), None);
    }

    #[test]
    fn test_mission_marker_parse() {
        assert_eq!(
            MissionMarker::parse("MISSION COMPLETE (t-001)"),
            Some(MissionMarker::Complete {
                task_id: Some("t-001".to_string())
            })
        );
        assert_eq!(
            MissionMarker::parse("REPLAN REQUIRED: missing deps"),
            Some(MissionMarker::Replan {
                reason: "REPLAN REQUIRED: missing deps".to_string()
            })
        );
        assert!(matches!(
            MissionMarker::parse("FAILED because of test errors"),
            Some(MissionMarker::Failed { .. })
        ));
    }

    #[test]
    fn test_assemble_final_deliverable() {
        let complete = assemble_final_deliverable(true, None, "Code completed. MISSION COMPLETE");
        assert!(complete.contains("MISSION COMPLETE"));

        let incomplete = assemble_final_deliverable(true, None, "Code partially written.");
        assert!(!incomplete.contains("MISSION COMPLETE"));
        assert!(incomplete.contains("FAILED"));

        let rejected = assemble_final_deliverable(
            false,
            Some("Syntax error in line 10"),
            "Code done. MISSION COMPLETE",
        );
        assert!(rejected.contains("VALIDATOR REJECTION: Syntax error in line 10"));
        assert!(!rejected.contains("MISSION COMPLETE"));
        assert!(rejected.contains("FAILED"));
    }
}
