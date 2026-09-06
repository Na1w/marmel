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

use crate::agent::phase::Plan;
use crate::agents::{Agent, DelegationRequest, Deliverable};
use crate::harness::HarnessStats;
use crate::llm::ChatClient;
use crate::orchestrator::OrchestratorManager;
use crate::types::{ChatRequest, Message};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const STEER_ARBITRATOR_SYSTEM_PROMPT: &str = include_str!("../../prompts/steer_arbitrator.md");

/// A per-subtask decision, matching caesar `SteerSubtaskDecision`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteerSubtaskDecision {
    pub tool_call_id: String,
    /// `"ForwardNotice"` | `"Cancel"` | `"DelegateTask"`.
    pub action: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// The steer decision JSON, matching caesar `SteerDecisionResponse`.
///
/// `decision` is one of `RespondDirectly`, `AbortImmediately`,
/// `QueueAndContinue`, `ForwardToWorker`, `ApprovePlan`, `RejectPlan`,
/// `DelegateTask`, `SwitchTier`, `SwitchModel`.
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

#[derive(Default)]
pub struct StreamingResponseExtractor {
    buffer: String,
    pub in_response_field: bool,
    pub finished: bool,
    escape_next: bool,
    unicode_buffer: Option<String>,
}

impl StreamingResponseExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_chunk(&mut self, chunk: &str) -> (String, bool) {
        if self.finished {
            return (String::new(), false);
        }

        let mut output = String::new();
        let mut just_finished = false;

        if !self.in_response_field {
            self.buffer.push_str(chunk);
            // Look for "response" : "
            if let Some(pos) = self.buffer.find("\"response\"") {
                let rest = &self.buffer[pos + "\"response\"".len()..];
                if let Some(colon_pos) = rest.find(':') {
                    let after_colon = &rest[colon_pos + 1..];
                    let trimmed = after_colon.trim_start();
                    if trimmed.starts_with('"') {
                        let quote_pos = after_colon.find('"').unwrap();
                        self.in_response_field = true;
                        let content_after_quote = &after_colon[quote_pos + 1..];
                        let chars: Vec<char> = content_after_quote.chars().collect();
                        self.buffer.clear();
                        for ch in chars {
                            if self.process_char(ch, &mut output) {
                                just_finished = true;
                                break;
                            }
                        }
                        return (output, just_finished);
                    } else if trimmed.starts_with("null")
                        || (!trimmed.is_empty()
                            && !trimmed.starts_with('"')
                            && !trimmed.starts_with('n'))
                    {
                        self.finished = true;
                        self.buffer.clear();
                        return (String::new(), false);
                    }
                }
            }
            return (String::new(), false);
        }

        // Already in response field
        for ch in chunk.chars() {
            if self.process_char(ch, &mut output) {
                just_finished = true;
                break;
            }
        }

        (output, just_finished)
    }

    fn process_char(&mut self, ch: char, output: &mut String) -> bool {
        if let Some(ref mut ubuf) = self.unicode_buffer {
            ubuf.push(ch);
            if ubuf.len() == 4 {
                if let Ok(code) = u32::from_str_radix(ubuf, 16)
                    && let Some(unicode_char) = char::from_u32(code)
                {
                    output.push(unicode_char);
                }
                self.unicode_buffer = None;
            }
            return false;
        }

        if self.escape_next {
            self.escape_next = false;
            match ch {
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                't' => output.push('\t'),
                '\\' => output.push('\\'),
                '"' => output.push('"'),
                '/' => output.push('/'),
                'u' => self.unicode_buffer = Some(String::new()),
                other => {
                    output.push('\\');
                    output.push(other);
                }
            }
            return false;
        }

        if ch == '\\' {
            self.escape_next = true;
            return false;
        }

        if ch == '"' {
            self.finished = true;
            self.in_response_field = false;
            return true;
        }

        output.push(ch);
        false
    }
}

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
        tools: None,
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

    if let Ok(decision) = serde_json::from_str::<SteerDecision>(json_text) {
        stats.record_steer_arbitration();
        if !did_stream_response && let Some(ref resp) = decision.response {
            on_delta(resp);
        }
        Some(decision)
    } else {
        None
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
    } else if decision.decision.eq_ignore_ascii_case("DelegateTask") {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(decision: &str, response: Option<&str>) -> SteerDecision {
        SteerDecision {
            decision: decision.to_string(),
            response: response.map(str::to_string),
            tier: None,
            model: None,
            subtasks: Vec::new(),
        }
    }

    #[test]
    fn test_respond_directly_json() {
        let json =
            r#"{"decision": "RespondDirectly", "response": "Executing step 2 in the plan."}"#;
        let d: SteerDecision = serde_json::from_str(json).unwrap();
        assert_eq!(d.decision, "RespondDirectly");
        assert_eq!(d.response.as_deref(), Some("Executing step 2 in the plan."));
        assert!(d.subtasks.is_empty());
    }

    #[test]
    fn test_abort_immediately_json() {
        let json = r#"{"decision": "AbortImmediately", "response": null}"#;
        let d: SteerDecision = serde_json::from_str(json).unwrap();
        assert_eq!(d.decision, "AbortImmediately");
        assert_eq!(d.response, None);
    }

    #[test]
    fn test_queue_and_continue_json() {
        let json = r#"{"decision": "QueueAndContinue", "response": null}"#;
        let d: SteerDecision = serde_json::from_str(json).unwrap();
        assert_eq!(d.decision, "QueueAndContinue");
        assert_eq!(d.response, None);
    }

    #[test]
    fn test_full_caesar_shape_with_subtasks() {
        // The full caesar `SteerDecisionResponse` shape, including tier/model/subtasks.
        let json = r#"{
            "decision": "ForwardToWorker",
            "response": "Forwarding feedback.",
            "tier": "cloud",
            "model": "ollama:Deepseek4Flash",
            "subtasks": [
                {
                    "tool_call_id": "call_123",
                    "action": "ForwardNotice",
                    "message": "use -O3",
                    "agent_name": null,
                    "prompt": null
                }
            ]
        }"#;
        let d: SteerDecision = serde_json::from_str(json).unwrap();
        assert_eq!(d.decision, "ForwardToWorker");
        assert_eq!(d.tier.as_deref(), Some("cloud"));
        assert_eq!(d.model.as_deref(), Some("ollama:Deepseek4Flash"));
        assert_eq!(d.subtasks.len(), 1);
        assert_eq!(d.subtasks[0].tool_call_id, "call_123");
        assert_eq!(d.subtasks[0].action, "ForwardNotice");
        assert_eq!(d.subtasks[0].message.as_deref(), Some("use -O3"));
    }

    #[test]
    fn test_fallback_queues_when_active_subtasks() {
        // Arbitrator unavailable + active subtasks → queue to preserve ongoing jobs.
        let outcome = resolve_steer_outcome(None, true);
        assert!(matches!(outcome, SteerOutcome::QueueInstruction));
    }

    #[test]
    fn test_fallback_steers_when_no_subtasks() {
        // Arbitrator unavailable + no active subtasks → steer immediately.
        let outcome = resolve_steer_outcome(None, false);
        assert!(matches!(outcome, SteerOutcome::SteerImmediately));
    }

    #[test]
    fn test_decided_when_arbitrator_available() {
        let d = decision("RespondDirectly", Some("hello"));
        let outcome = resolve_steer_outcome(Some(d.clone()), true);
        match outcome {
            SteerOutcome::Decided(dec) => assert_eq!(dec.decision, "RespondDirectly"),
            _ => panic!("expected Decided"),
        }
    }

    #[test]
    fn test_three_decision_branches_roundtrip() {
        // Serialize + deserialize each of the three core branches.
        for (decision_name, response) in [
            ("RespondDirectly", Some("reply_text")),
            ("AbortImmediately", None),
            ("QueueAndContinue", None),
        ] {
            let d = decision(decision_name, response);
            let json = serde_json::to_string(&d).unwrap();
            let back: SteerDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(back.decision, decision_name);
            assert_eq!(back.response, response.map(str::to_string));
        }
    }

    #[test]
    fn test_streaming_response_extractor_basic() {
        let mut extractor = StreamingResponseExtractor::new();
        let chunk1 = "{\"decision\": \"RespondDirectly\", \"response\": \"Hello ";
        let (out1, finished1) = extractor.push_chunk(chunk1);
        assert_eq!(out1, "Hello ");
        assert!(!finished1);

        let chunk2 = "there!\\nThis is ";
        let (out2, finished2) = extractor.push_chunk(chunk2);
        assert_eq!(out2, "there!\nThis is ");
        assert!(!finished2);

        let chunk3 = "ready.\", \"subtasks\": []}";
        let (out3, finished3) = extractor.push_chunk(chunk3);
        assert_eq!(out3, "ready.");
        assert!(finished3);

        // After finished, nothing more is emitted
        let (out4, finished4) = extractor.push_chunk(" extra stuff");
        assert_eq!(out4, "");
        assert!(!finished4);
    }

    #[test]
    fn test_streaming_response_extractor_no_response_field() {
        let mut extractor = StreamingResponseExtractor::new();
        let chunk = "{\"decision\": \"AbortImmediately\", \"subtasks\": []}";
        let (out, finished) = extractor.push_chunk(chunk);
        assert_eq!(out, "");
        assert!(!finished);
        assert!(!extractor.in_response_field);
    }

    #[test]
    fn test_streaming_response_extractor_escapes() {
        let mut extractor = StreamingResponseExtractor::new();
        let chunk = "{\"decision\": \"RespondDirectly\", \"response\": \"\\\"Citat\\\" och \\\\backslashes\\\\ samt \\t tabb och \\u0041\"}";
        let (out, finished) = extractor.push_chunk(chunk);
        assert_eq!(out, "\"Citat\" och \\backslashes\\ samt \t tabb och A");
        assert!(finished);
    }

    #[test]
    fn test_streaming_response_extractor_null_response_field() {
        let mut extractor = StreamingResponseExtractor::new();
        let chunk =
            "{\"decision\": \"QueueAndContinue\", \"response\": null, \"subtasks\": [\"task1\"]}";
        let (out, _finished) = extractor.push_chunk(chunk);
        assert_eq!(out, "");
        assert!(!extractor.in_response_field);
    }

    #[test]
    fn test_extract_tasks_to_delegate_explicit_subtasks() {
        let json = r#"{
            "decision": "DelegateTask",
            "response": "I will run the tests and check files.",
            "subtasks": [
                {
                    "tool_call_id": "steer-task-1",
                    "action": "DelegateTask",
                    "agent_name": "coder",
                    "prompt": "Run cargo test"
                },
                {
                    "tool_call_id": "steer-task-2",
                    "action": "DelegateTask",
                    "agent_name": "researcher",
                    "prompt": "Search codebase for foo"
                }
            ]
        }"#;
        let d: SteerDecision = serde_json::from_str(json).unwrap();
        let tasks = extract_tasks_to_delegate(&d, "check things");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].0, Agent::Coder);
        assert_eq!(tasks[0].1, "steer-task-1");
        assert_eq!(tasks[0].2, "Run cargo test");
        assert_eq!(tasks[1].0, Agent::Researcher);
        assert_eq!(tasks[1].1, "steer-task-2");
        assert_eq!(tasks[1].2, "Search codebase for foo");
    }

    #[test]
    fn test_extract_tasks_to_delegate_toplevel_fallback() {
        let json = r#"{
            "decision": "DelegateTask",
            "response": "Starting coder task directly.",
            "subtasks": []
        }"#;
        let d: SteerDecision = serde_json::from_str(json).unwrap();
        let tasks = extract_tasks_to_delegate(&d, "run cargo check");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].0, Agent::Coder);
        assert_eq!(tasks[0].1, "steer-task-1");
        assert_eq!(tasks[0].2, "run cargo check");
    }

    #[test]
    fn test_extract_tasks_to_delegate_no_tasks() {
        let json = r#"{
            "decision": "RespondDirectly",
            "response": "Currently on step 1.",
            "subtasks": []
        }"#;
        let d: SteerDecision = serde_json::from_str(json).unwrap();
        let tasks = extract_tasks_to_delegate(&d, "status?");
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn test_execute_steer_subtask_runs_specialist() {
        let client = ChatClient::new_with_token("http://127.0.0.1:11434", "mock", "tok");
        let stats = Arc::new(HarnessStats::new());
        let res = execute_steer_subtask(
            &client,
            stats,
            Agent::Coder,
            Some("steer-test-1".to_string()),
            "Inspect git status",
        )
        .await;
        assert!(res.is_ok());
        let d = res.unwrap();
        assert!(matches!(
            d.marker,
            crate::agents::MissionMarker::Complete { .. }
        ));
        assert_eq!(d.task_id.as_deref(), Some("steer-test-1"));
        assert!(d.content.contains("Inspect git status"));
    }

    #[tokio::test]
    async fn test_synthesize_steer_subtask_response_streams_answer() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"De 3 testerna som misslyckas är i modul foo.\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
                ),
            )
            .mount(&server)
            .await;

        let client = ChatClient::new_with_token(&server.uri(), "mock", "tok");
        let stats = HarnessStats::new();
        let deliverable = Deliverable {
            marker: crate::agents::MissionMarker::Complete {
                task_id: Some("steer-test-1".to_string()),
            },
            content: "Found 3 failing tests in module foo".to_string(),
            task_id: Some("steer-test-1".to_string()),
        };
        let deliverables = vec![(Agent::Researcher, "steer-test-1".to_string(), deliverable)];
        let mut streamed = Vec::new();
        let res = synthesize_steer_subtask_response(
            &client,
            &stats,
            "Vilka tester misslyckas?",
            &deliverables,
            |delta| streamed.push(delta.to_string()),
        )
        .await;
        assert!(res.is_ok());
        let final_text = res.unwrap();
        assert_eq!(final_text, "De 3 testerna som misslyckas är i modul foo.");
        assert_eq!(
            streamed.join(""),
            "De 3 testerna som misslyckas är i modul foo."
        );
    }
}
