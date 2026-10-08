//! Specialist Subagents — isolated-context, run-to-completion workers.

pub mod catalog;
pub mod coder;
pub mod debugger;
pub mod generalist;
pub mod planner;
pub mod prompt_builder;
pub mod researcher;
pub mod runner;
pub mod validation;
pub mod validator;

/// Terminal outcome a specialist returns. The marker grammar — marker set,
/// precedence rules and parser — has a single owner in [`crate::markers`]
/// (dedup cluster C4, `docs/recon_duplication_helpers.md` §2.4). It is
/// re-exported under the name the agents layer always published, so no
/// consumer outside these files needed editing.
pub use crate::markers::MissionMarker;
pub use catalog::{AgentArchetype, Catalog, Skill, SkillSource};
pub use coder::Coder;
pub use debugger::Debugger;
pub use generalist::Generalist;
pub use planner::Planner;
pub use prompt_builder::{AgentBlueprint, PromptBuilder};
pub use researcher::Researcher;
pub use runner::run_specialist_live;
pub(crate) use runner::run_specialist_llm;
pub use validation::ValidationOutcome;
pub use validator::Validator;

use crate::markers::{ABORT_REASON, aborted_deliverable};
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
    /// Strategic Planner — mission architecture, task decomposition, and execution plan creation.
    Planner,
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
            Agent::Planner => "planner",
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
            "planner" => Some(Self::Planner),
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

// `MissionMarker`, its `parse` and the `contains_failed_marker` sanitiser used
// to be declared here (and, byte-for-byte, again in `src/manager/phase.rs`).
// Dedup cluster C4 collapsed them into the single owner `crate::markers`, which
// is re-exported above under the historical `crate::agents::MissionMarker` path.
// Deleting this copy also removed the second substring-first parser responsible
// for bug C1 (`docs/recon_bugs_manager.md`): a FAILED deliverable that merely
// mentions `MISSION COMPLETE` in its prose was reported as `Complete`.

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
    pub blueprint: Option<AgentBlueprint>,
}

impl IsolatedContext {
    pub fn from_request(role_system_prompt: String, req: &DelegationRequest) -> Self {
        let blueprint = AgentBlueprint::parse_from_markdown(&role_system_prompt).ok();
        Self {
            role_system_prompt,
            brief: req.prompt.clone(),
            task_id: req.task_id.clone(),
            snippets: req.snippets.clone(),
            image_urls: req.image_urls.clone().unwrap_or_default(),
            audio_urls: req.audio_urls.clone().unwrap_or_default(),
            blueprint,
        }
    }

    pub fn with_blueprint(mut self, blueprint: AgentBlueprint) -> Self {
        self.blueprint = Some(blueprint);
        self
    }

    /// Retrieve the explicit list of allowed tools defined in the prompt blueprint, if any.
    pub fn allowed_tools(&self) -> Option<&[String]> {
        self.blueprint
            .as_ref()
            .map(|bp| bp.allowed_tools.as_slice())
    }

    pub fn into_engine(&self, max_context_tokens: usize) -> crate::manager::ContextEngine {
        let factory = crate::manager::ContextEngineFactory::new(max_context_tokens);
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
        if token.is_cancelled()
            || (!cfg!(test) && crate::orchestrator::is_globally_cancelled())
            || crate::orchestrator::CURRENT_WORKER_TOKEN
                .try_with(|t| t.is_cancelled())
                .unwrap_or(false)
        {
            return Deliverable {
                marker: MissionMarker::Failed {
                    reason: ABORT_REASON.to_string(),
                },
                // Gate t-070: the aborted-deliverable body has exactly one owner,
                // `crate::markers::aborted_deliverable` — the same call the
                // runner's own abort path (`runner::execution::aborted_deliverable`),
                // the `delegate_task` handler and the orchestrator worker loop
                // make, so the abort text keeps one byte-for-byte spelling
                // everywhere (it is asserted elsewhere and read by
                // `ui::helpers::extract_failure_reason`).
                content: aborted_deliverable("aborted by user instruction"),
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

/// Shared assertion helper for per-specialist role/namespace tests
/// (duplicates.md §6a): the six specialist files previously each embedded a
/// near-identical `#[cfg(test)]` block; they now delegate to this helper.
#[cfg(test)]
pub(crate) fn assert_specialist_role(
    agent: &dyn Specialist,
    expected: Agent,
    must_contain: &[&str],
    must_not_contain: &[&str],
    may_recurse: bool,
) {
    assert_eq!(agent.name(), expected);
    for ns in must_contain {
        assert!(
            agent.tool_namespaces().contains(ns),
            "{:?} must grant namespace `{ns}`",
            expected
        );
    }
    for ns in must_not_contain {
        assert!(
            !agent.tool_namespaces().contains(ns),
            "{:?} must NOT grant namespace `{ns}`",
            expected
        );
    }
    assert_eq!(
        agent.may_recurse(),
        may_recurse,
        "{:?} may_recurse mismatch",
        expected
    );
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
        assert_eq!(Agent::from_str("planner"), Some(Agent::Planner));
        assert_eq!(Agent::Planner.to_string(), "planner");
        assert_eq!(Agent::from_str("unknown"), None);
    }

    /// Dedup cluster C4: the agents layer no longer owns a `MissionMarker`
    /// copy — `crate::agents::MissionMarker`, `crate::markers::MissionMarker`
    /// and `crate::manager::phase::MissionMarker` are one and the same type,
    /// so the plan layer and the specialist layer can never drift apart again.
    #[test]
    fn mission_marker_has_one_owner_reexported_by_both_layers() {
        let agents_side: MissionMarker = MissionMarker::Complete { task_id: None };
        let module_side: crate::markers::MissionMarker = agents_side.clone();
        let plan_side: crate::manager::phase::MissionMarker = module_side.clone();
        assert_eq!(agents_side, plan_side);
    }

    /// Gate t-059 byte-pin: the abort path builds its trailer through
    /// `markers::failed_trailer`, and the rendered deliverable text is still
    /// **exactly** the historical bytes (this string is asserted elsewhere and
    /// read by `ui::helpers::extract_failure_reason`).
    #[tokio::test]
    async fn aborted_deliverable_keeps_its_exact_bytes() {
        let ctx = IsolatedContext {
            role_system_prompt: String::new(),
            brief: "pin the abort trailer".to_string(),
            task_id: Some("t-059".to_string()),
            snippets: Vec::new(),
            image_urls: Vec::new(),
            audio_urls: Vec::new(),
            blueprint: None,
        };
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();

        let deliverable = Generalist.run(&ctx, &token).await;
        assert_eq!(
            deliverable.content,
            "Task aborted by user instruction.\n\nFAILED (aborted)"
        );
        assert_eq!(
            deliverable.marker,
            MissionMarker::Failed {
                reason: "aborted".to_string()
            }
        );
        assert_eq!(deliverable.task_id.as_deref(), Some("t-059"));
        // The marker owner reads its own trailer back: emit and parse agree.
        assert_eq!(
            crate::markers::failure_reason(&deliverable.content),
            Some("aborted")
        );
        assert!(crate::markers::has_failure_marker(&deliverable.content));
        assert!(
            !crate::markers::MissionMarker::parse(&deliverable.content)
                .is_some_and(|m| m.is_complete())
        );
    }

    /// Bug C1 regression (a), seen from the specialist layer: a FAILED
    /// deliverable whose prose mentions `MISSION COMPLETE` must not be
    /// reported as a completion.
    #[test]
    fn regression_failed_deliverable_mentioning_mission_complete_stays_failed() {
        let content = "FAILED: replace tool rejected; I did not emit \
                       MISSION COMPLETE (t-014) because the build broke";
        let marker = MissionMarker::parse(content).expect("a terminal marker");
        assert!(
            matches!(marker, MissionMarker::Failed { .. }),
            "expected Failed, got {marker:?}"
        );
        assert!(!marker.is_complete());
    }

    /// Bug C1 regression (b)/(c) at the deliverable boundary: a genuine
    /// completion still checks off logically, and `REPLAN REQUIRED` is never a
    /// success.
    #[test]
    fn regression_completion_and_replan_verdicts_at_agents_layer() {
        let done = "Implemented src/markers.rs and ran cargo test.\n\nMISSION COMPLETE (t-007)";
        assert!(matches!(
            MissionMarker::parse(done),
            Some(MissionMarker::Complete { task_id: Some(id) }) if id == "t-007"
        ));

        let replan = MissionMarker::parse("REPLAN REQUIRED: the decomposition is wrong")
            .expect("a terminal marker");
        assert!(!replan.is_complete());
        assert!(replan.is_failure());
    }

    /// Bug C1 regression (d): the structured `Deliverable.marker` field is
    /// authoritative over the body text; the positional body parse is only the
    /// fallback when no structured marker exists.
    #[test]
    fn regression_structured_deliverable_marker_wins_over_body_text() {
        let d = Deliverable {
            marker: MissionMarker::Complete {
                task_id: Some("t-005".to_string()),
            },
            content: "the first attempt FAILED; this final revision is clean".to_string(),
            task_id: Some("t-005".to_string()),
        };
        let resolved = MissionMarker::resolve(Some(&d.marker), &d.content).expect("resolved");
        assert_eq!(resolved, d.marker);

        // A structured FAILED is not rescued by a stale completion sentence.
        let rejected = Deliverable {
            marker: MissionMarker::Failed {
                reason: "validator rejected".to_string(),
            },
            content: "MISSION COMPLETE (t-006) — revoked before finalization".to_string(),
            task_id: Some("t-006".to_string()),
        };
        assert!(
            MissionMarker::resolve(Some(&rejected.marker), &rejected.content)
                .is_some_and(|m| m.is_failure())
        );

        // Without a structured marker the positional parse decides.
        assert_eq!(
            MissionMarker::resolve(None, "done\n\nMISSION COMPLETE (t-009)"),
            Some(MissionMarker::Complete {
                task_id: Some("t-009".to_string())
            })
        );
    }

    #[test]
    fn test_assemble_final_deliverable() {
        let complete = assemble_final_deliverable(
            true,
            None,
            "Code completed. MISSION COMPLETE",
            Some("t-001"),
        );
        assert!(complete.contains("MISSION COMPLETE"));

        // When validation passed without explicit MISSION COMPLETE, deliverable is approved and concluded with MISSION COMPLETE
        let approved_implicit = assemble_final_deliverable(
            true,
            None,
            "Implemented feature and verified tests pass.",
            Some("t-002"),
        );
        assert!(approved_implicit.contains("MISSION COMPLETE (t-002)"));
        assert!(!approved_implicit.contains("FAILED"));

        // When validation passed but specialist explicitly requested replan, keep replan
        let replan = assemble_final_deliverable(
            true,
            None,
            "REPLAN REQUIRED: need different database schema",
            Some("t-003"),
        );
        assert!(replan.contains("REPLAN REQUIRED"));
        assert!(!replan.contains("MISSION COMPLETE"));

        // When validation failed (rejected), revoke MISSION COMPLETE and append FAILED
        let rejected = assemble_final_deliverable(
            false,
            Some("Syntax error in line 10"),
            "Code done. MISSION COMPLETE",
            Some("t-004"),
        );
        assert!(rejected.contains("VALIDATOR REJECTION: Syntax error in line 10"));
        assert!(!rejected.contains("MISSION COMPLETE"));
        assert!(rejected.contains("FAILED (Validator rejected deliverable)"));

        // When deliverable failed without validator critique (e.g. premature loop termination)
        let premature =
            assemble_final_deliverable(false, None, "Specialist hit token limit", Some("t-005"));
        assert!(premature.contains("FAILED (Task incomplete or terminated prematurely)"));
        assert!(!premature.contains("VALIDATOR REJECTION"));
    }
}
