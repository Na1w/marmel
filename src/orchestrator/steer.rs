//! Steer Arbitrator — fast, real-time user steering & inquiry handler during execution.
//!
//! Aligned with Marmennill's `call_steer_arbitrator` contract
//! (REFERENCE_ORCHESTRATION_CONTRACT.md §5): when a user sends a message
//! mid-flight while subagents or tools are running, the Steer Arbitrator
//! evaluates the user's intent in real time against the active execution plan,
//! current goal, and running subtasks, and returns a `SteerDecision` JSON
//! matching the caesar `SteerDecisionResponse` shape.
//!
//! The decision vocabulary (caesar §5.1) is `RespondDirectly`,
//! `AbortImmediately`, `QueueAndContinue`, `ForwardToWorker`, `ApprovePlan`,
//! `RejectPlan`, `DelegateTask`, `SwitchTier`, `SwitchModel`. This module
//! focuses on the three core branches (`RespondDirectly`, `AbortImmediately`,
//! `QueueAndContinue`) while keeping the JSON shape fully compatible with the
//! caesar `SteerDecisionResponse` (including `tier`, `model`, and `subtasks`).

use crate::agents::{Agent, DelegationRequest, Deliverable};
use crate::harness::HarnessStats;
use crate::llm::ChatClient;
use crate::manager::phase::Plan;
use crate::orchestrator::OrchestratorManager;
use crate::types::{ChatRequest, Message};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const STEER_ARBITRATOR_SYSTEM_PROMPT: &str = include_str!("../../prompts/steer_arbitrator.md");

/// A per-subtask decision, matching caesar `SteerSubtaskDecision`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteerSubtaskDecision {
    pub tool_call_id: String,
    /// `"ForwardNotice"` | `"Cancel"` | `"DelegateTask"` | `"Sleep"`.
    pub action: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub sleep_seconds: Option<u64>,
}

/// The steer decision JSON, matching caesar `SteerDecisionResponse`.
///
/// `decision` is one of `RespondDirectly`, `AbortImmediately`,
/// `QueueAndContinue`, `ForwardToWorker`, `ApprovePlan`, `RejectPlan`,
/// `DelegateTask`, `Sleep`, `SwitchTier`, `SwitchModel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteerDecision {
    pub decision: String,
    pub response: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub subtasks: Vec<SteerSubtaskDecision>,
    #[serde(default)]
    pub sleep_seconds: Option<u64>,
}

/// Outcome of a steer arbitration, including the unavailable-arbitrator
/// fallback (caesar §5.3).
#[derive(Debug, Clone)]
pub enum SteerOutcome {
    /// The arbitrator produced a decision.
    Decided(SteerDecision),
    /// The arbitrator was unavailable and active subtasks are running: queue
    /// the instruction to preserve ongoing jobs (caesar §5.3).
    QueueInstruction,
    /// The arbitrator was unavailable and no subtasks are active: steer the
    /// orchestrator immediately (caesar §5.3).
    SteerImmediately,
}

pub use super::steer_extractor::StreamingResponseExtractor;

#[derive(Debug, Clone, Default)]
pub struct SteerContext<'a> {
    pub main_goal: &'a str,
    pub orchestrator_status: &'a str,
    pub pending_approval: &'a str,
    pub plan_progress: &'a str,
    pub plan_content: &'a str,
    pub available_agents: &'a str,
    pub steering_history: &'a str,
    pub user_message: &'a str,
    pub active_subtasks: &'a str,
}

/// Run the steer arbitrator against the LLM backend with real-time SSE delta streaming and full contextual visibility.
pub async fn arbitrate_steer_context_stream<F>(
    client: &ChatClient,
    stats: &HarnessStats,
    ctx: SteerContext<'_>,
    mut on_delta: F,
) -> Option<SteerDecision>
where
    F: FnMut(&str),
{
    let default_agents = "\
- coder: Implementation, writing/editing files, creating documents, refactoring, running builds and tests.
- debugger: Bug forensics, fixing failing tests, diagnosing crash traces, investigating tool errors.
- researcher: Codebase inspection, searching documentation, factual research.
- generalist: High-level planning, synthesis, multi-domain evaluation.";

    let available_agents = if ctx.available_agents.is_empty() {
        default_agents
    } else {
        ctx.available_agents
    };

    let env_block = crate::prompts::format_environment_block();

    let user_prompt = format!(
        "Available Specialist Agents:\n{}\n\n{}\n\nMain Session Goal: \"{}\"\n\nActive Execution Plan (Full Text):\n{}\n\nExecution Plan Progress Breakdown:\n{}\n\nOrchestrator Status: {}\n\nPending Approval Request:\n{}\n\nActive Subtasks:\n{}\n\nSteering Conversation History:\n{}\n\nNew User Instruction: \"{}\"\n\nPlease output your decision JSON.",
        available_agents,
        env_block,
        ctx.main_goal,
        if ctx.plan_content.is_empty() {
            "None"
        } else {
            ctx.plan_content
        },
        if ctx.plan_progress.is_empty() {
            "None"
        } else {
            ctx.plan_progress
        },
        if ctx.orchestrator_status.is_empty() {
            "Active"
        } else {
            ctx.orchestrator_status
        },
        if ctx.pending_approval.is_empty() {
            "None"
        } else {
            ctx.pending_approval
        },
        if ctx.active_subtasks.is_empty() {
            "None"
        } else {
            ctx.active_subtasks
        },
        if ctx.steering_history.is_empty() {
            "None"
        } else {
            ctx.steering_history
        },
        ctx.user_message,
    );

    let req = ChatRequest {
        model: String::new(),
        messages: vec![
            Message::System {
                content: STEER_ARBITRATOR_SYSTEM_PROMPT.to_string(),
            },
            Message::User {
                content: user_prompt,
            },
        ],
        temperature: Some(0.0),
        top_p: Some(0.9),
        frequency_penalty: None,
        presence_penalty: None,
        stream: Some(true),
        enable_thinking: Some(false),
        tools: Some(vec![crate::types::ToolDef::sleep()]),
    };

    let mut extractor = StreamingResponseExtractor::new();
    let mut did_stream_response = false;
    let mut raw_stream_accum = String::new();

    let reply = match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        client.chat_stream(&req, |chunk| {
            raw_stream_accum.push_str(chunk);
            let (delta, _) = extractor.push_chunk(chunk);
            if !delta.is_empty() {
                did_stream_response = true;
                on_delta(&delta);
            }
            true
        }),
    )
    .await
    {
        Ok(Ok(r)) => r,
        _ => return None,
    };

    let mut raw = reply.content.trim().to_string();
    if raw.is_empty() && !reply.raw.is_empty() {
        raw = reply.raw.trim().to_string();
    }
    if raw.is_empty() && !raw_stream_accum.is_empty() {
        raw = raw_stream_accum.trim().to_string();
    }

    // Strip markdown code fences if present
    let json_text = if let Some(stripped) = raw.strip_prefix("```json") {
        stripped.trim_end_matches("```").trim()
    } else if let Some(stripped) = raw.strip_prefix("```") {
        stripped.trim_end_matches("```").trim()
    } else if let Some(start) = raw.find('{') {
        if let Some(end) = raw.rfind('}') {
            &raw[start..=end]
        } else {
            &raw
        }
    } else {
        &raw
    };

    let parsed_decision = if let Ok(mut decision) = serde_json::from_str::<SteerDecision>(json_text)
    {
        if decision.decision.eq_ignore_ascii_case("Sleep") && decision.sleep_seconds.is_none() {
            decision.sleep_seconds = Some(5);
        }
        Some(decision)
    } else if let Some(tc) = reply
        .tool_calls
        .iter()
        .find(|tc| tc.function.name == "sleep" || tc.function.name == "terminal__sleep")
    {
        let args: serde_json::Value =
            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
        let secs = args
            .get("seconds")
            .or_else(|| args.get("duration"))
            .or_else(|| args.get("duration_seconds"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(5);
        let reason = args
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let resp = if reason.is_empty() {
            format!("Sleeping for {secs} seconds...")
        } else {
            format!("Sleeping for {secs} seconds ({reason})...")
        };
        Some(SteerDecision {
            decision: "Sleep".to_string(),
            response: Some(resp),
            tier: None,
            model: None,
            subtasks: Vec::new(),
            sleep_seconds: Some(secs),
        })
    } else if did_stream_response || !raw.is_empty() {
        let resp = if !raw.is_empty() {
            raw
        } else {
            raw_stream_accum
        };
        Some(SteerDecision {
            decision: "RespondDirectly".to_string(),
            response: Some(resp),
            tier: None,
            model: None,
            subtasks: Vec::new(),
            sleep_seconds: None,
        })
    } else {
        None
    };

    if let Some(decision) = parsed_decision {
        stats.record_steer_arbitration();
        if !did_stream_response && let Some(ref resp) = decision.response {
            on_delta(resp);
        }
        Some(decision)
    } else {
        None
    }
}

/// Normalize a steering decision string to a canonical decision name.
pub fn normalize_steer_decision(decision: Option<&str>) -> &'static str {
    match decision {
        None => "None",
        Some(s) => {
            let norm = s.trim().to_ascii_lowercase().replace(['_', '-', ' '], "");
            match norm.as_str() {
                "responddirectly" | "respond" | "directresponse" | "direct" => "RespondDirectly",
                "abortimmediately" | "abort" => "AbortImmediately",
                "forwardtoworker" | "forward" | "forwardnotice" => "ForwardToWorker",
                "approveplan" | "approve" => "ApprovePlan",
                "rejectplan" | "reject" => "RejectPlan",
                "sleep" => "Sleep",
                "delegatetask" | "delegate" => "DelegateTask",
                "queueandcontinue" | "queue" => "QueueAndContinue",
                _ => "Unknown",
            }
        }
    }
}

/// Format the accumulated steering conversation history into a readable transcript for SteerContext.
pub fn format_steering_history(history: &[(String, String)]) -> String {
    if history.is_empty() {
        "None".to_string()
    } else {
        history
            .iter()
            .map(|(user, resp)| {
                format!("User: \"{}\"\nArbitrator: \"{}\"", user.trim(), resp.trim())
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Run the steer arbitrator against the LLM backend with real-time SSE delta streaming.
pub async fn arbitrate_steer_stream<F>(
    client: &ChatClient,
    stats: &HarnessStats,
    main_goal: &str,
    plan_content: &str,
    active_subtask: &str,
    user_message: &str,
    on_delta: F,
) -> Option<SteerDecision>
where
    F: FnMut(&str),
{
    let ctx = SteerContext {
        main_goal,
        orchestrator_status: "Active",
        pending_approval: "None",
        plan_progress: "",
        plan_content,
        available_agents: "",
        steering_history: "None",
        user_message,
        active_subtasks: active_subtask,
    };
    arbitrate_steer_context_stream(client, stats, ctx, on_delta).await
}

/// Run the steer arbitrator without streaming callback.
pub async fn arbitrate_steer(
    client: &ChatClient,
    stats: &HarnessStats,
    main_goal: &str,
    plan_content: &str,
    active_subtask: &str,
    user_message: &str,
) -> Option<SteerDecision> {
    arbitrate_steer_stream(
        client,
        stats,
        main_goal,
        plan_content,
        active_subtask,
        user_message,
        |_| {},
    )
    .await
}

/// Resolve the steer outcome from an optional arbitrator decision and the
/// active-subtask state. This is the pure fallback logic (caesar §5.3),
/// factored out so it can be unit-tested without an LLM backend.
pub fn resolve_steer_outcome(
    decision: Option<SteerDecision>,
    has_active_subtasks: bool,
) -> SteerOutcome {
    match decision {
        Some(d) => SteerOutcome::Decided(d),
        None if has_active_subtasks => SteerOutcome::QueueInstruction,
        None => SteerOutcome::SteerImmediately,
    }
}

/// Steer with streaming delta callback and the unavailable-arbitrator fallback (caesar §5.3).
pub async fn arbitrate_steer_stream_with_fallback<F>(
    client: &ChatClient,
    stats: &HarnessStats,
    ctx: SteerContext<'_>,
    has_active_subtasks: bool,
    on_delta: F,
) -> SteerOutcome
where
    F: FnMut(&str),
{
    let decision = arbitrate_steer_context_stream(client, stats, ctx, on_delta).await;
    resolve_steer_outcome(decision, has_active_subtasks)
}

/// Steer with the unavailable-arbitrator fallback (caesar §5.3): if the
/// arbitrator is unavailable and active subtasks are running, the instruction
/// is queued to preserve ongoing jobs.
pub async fn arbitrate_steer_with_fallback(
    client: &ChatClient,
    stats: &HarnessStats,
    ctx: SteerContext<'_>,
    has_active_subtasks: bool,
) -> SteerOutcome {
    arbitrate_steer_stream_with_fallback(client, stats, ctx, has_active_subtasks, |_| {}).await
}

/// Helper to extract all subtasks that should be delegated from a SteerDecision.
pub fn extract_tasks_to_delegate(
    decision: &SteerDecision,
    user_msg: &str,
) -> Vec<(Agent, String, String)> {
    let mut tasks = Vec::new();
    let has_explicit_subtask_delegations = decision
        .subtasks
        .iter()
        .any(|s| s.action.eq_ignore_ascii_case("DelegateTask"));

    if has_explicit_subtask_delegations {
        let mut idx = 1;
        for s in &decision.subtasks {
            if s.action.eq_ignore_ascii_case("DelegateTask") {
                let agent = s
                    .agent_name
                    .as_deref()
                    .and_then(Agent::from_str)
                    .unwrap_or(Agent::Coder);
                let tid = if !s.tool_call_id.trim().is_empty() {
                    s.tool_call_id.clone()
                } else {
                    format!("steer-task-{idx}")
                };
                idx += 1;
                let prompt = s
                    .prompt
                    .as_deref()
                    .filter(|p| !p.trim().is_empty())
                    .unwrap_or(user_msg);
                tasks.push((agent, tid, prompt.to_string()));
            }
        }
    } else if normalize_steer_decision(Some(&decision.decision)) == "DelegateTask" {
        let agent = decision
            .subtasks
            .iter()
            .find_map(|s| s.agent_name.as_deref().and_then(Agent::from_str))
            .unwrap_or(Agent::Coder);
        let tid = decision
            .subtasks
            .first()
            .filter(|s| !s.tool_call_id.trim().is_empty())
            .map(|s| s.tool_call_id.clone())
            .unwrap_or_else(|| "steer-task-1".to_string());
        let prompt = decision
            .subtasks
            .first()
            .and_then(|s| s.prompt.as_deref())
            .filter(|p| !p.trim().is_empty())
            .unwrap_or(user_msg);
        tasks.push((agent, tid, prompt.to_string()));
    }
    tasks
}

/// Execute an ad-hoc delegated subtask triggered by steering arbitration.
pub async fn execute_steer_subtask(
    client: &ChatClient,
    stats: Arc<HarnessStats>,
    agent: Agent,
    task_id: Option<String>,
    prompt: &str,
) -> Result<Deliverable, anyhow::Error> {
    let plan = Plan::default();
    let mut manager = OrchestratorManager::new(client.clone(), plan, stats);
    manager.cancellation_token = crate::orchestrator::bus::global_cancellation_token();
    let req = DelegationRequest {
        agent_name: agent,
        prompt: prompt.to_string(),
        snippets: Vec::new(),
        task_id,
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    manager.delegate(req).await
}

/// Synthesize a direct response to the user's inquiry after delegated steering subtasks complete.
pub async fn synthesize_steer_subtask_response<F>(
    client: &ChatClient,
    _stats: &HarnessStats,
    user_msg: &str,
    deliverables: &[(Agent, String, Deliverable)],
    mut on_delta: F,
) -> Result<String, anyhow::Error>
where
    F: FnMut(&str) + Send,
{
    let mut findings = String::new();
    for (agent, task_id, deliverable) in deliverables {
        findings.push_str(&format!(
            "### Specialist [{}] (Subtask: {}):\n{}\n\n",
            agent.as_str(),
            task_id,
            deliverable.content
        ));
    }

    let system_prompt = "\
You are Marmel's Steer Arbitrator. The user asked a question or sent an inquiry mid-flight while the session was executing.
You delegated specialist subtask(s) to inspect the workspace, run diagnostics, or research the answer.
The specialist findings and deliverables are provided below.

Your task: Formulate a direct, helpful, and concise answer to the user in the EXACT SAME LANGUAGE as the user's message.
- Answer the user's inquiry directly using the specialist findings.
- Be factual, surgical, and clear. Zero filler, no conversational boilerplate or meta-disclaimers.";

    let user_prompt = format!(
        "User Message/Question:\n\"{}\"\n\nSpecialist Findings:\n{}\nPlease answer the user's question directly based on the findings above.",
        user_msg, findings
    );

    let req = ChatRequest {
        model: String::new(),
        messages: vec![
            Message::System {
                content: system_prompt.to_string(),
            },
            Message::User {
                content: user_prompt,
            },
        ],
        temperature: Some(0.0),
        top_p: Some(0.9),
        frequency_penalty: None,
        presence_penalty: None,
        stream: Some(true),
        enable_thinking: Some(false),
        tools: None,
    };

    let mut full_response = String::new();
    let reply = match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        client.chat_stream(&req, |chunk| {
            full_response.push_str(chunk);
            on_delta(chunk);
            true
        }),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e),
        Err(e) => return Err(e.into()),
    };

    let final_text = if !full_response.trim().is_empty() {
        full_response.trim().to_string()
    } else {
        reply.content.trim().to_string()
    };

    Ok(final_text)
}
