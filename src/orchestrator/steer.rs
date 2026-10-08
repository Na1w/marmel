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
    /// Raw action string from the arbitrator. Canonicalized through
    /// [`SteerSubtaskAction`] / [`normalize_steer_subtask_action`] — the vocabulary is
    /// `"ForwardNotice"` | `"Cancel"` | `"DelegateTask"` | `"Sleep"`; anything else is
    /// rejected and logged.
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

/// Build the `Sleep` steer decision from the arguments of a sleep-shaped tool
/// call in the arbitrator's reply.
///
/// Gate t-068: this module used to carry a **fourth, divergent copy** of the
/// `sleep`-argument extraction — `seconds` / `duration` / `duration_seconds`,
/// `as_u64` plus a string parse, `unwrap_or(5)` — and applied **no clamp at
/// all**, so a steered `sleep` with `1e9` or `999999` could request an
/// unbounded wait. The whole canonicalization (duration keys, every JSON number
/// shape, the fallback default and the min/max clamp) belongs to the single
/// owner [`crate::tool_args::sleep_duration_secs`], which the sync and async
/// `sleep` tool handlers already delegate to; a steered sleep and a tool-call
/// sleep therefore resolve identical arguments to identical seconds by
/// construction. Only the user-facing wording stays in this module.
///
/// The arbitrator's own wait budget (`ui::bridge::arbiter::MAX_SINGLE_SLEEP_SECONDS`)
/// is a **separate knob** and is deliberately not consulted here.
pub(crate) fn sleep_steer_decision(arguments: &str) -> SteerDecision {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let secs = crate::tool_args::sleep_duration_secs(&args);
    let reason = args
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let response = if reason.is_empty() {
        format!("Sleeping for {secs} seconds...")
    } else {
        format!("Sleeping for {secs} seconds ({reason})...")
    };
    SteerDecision {
        decision: "Sleep".to_string(),
        response: Some(response),
        tier: None,
        model: None,
        subtasks: Vec::new(),
        sleep_seconds: Some(secs),
    }
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

    let parsed_decision = if let Ok(mut decision) = serde_json::from_str::<SteerDecision>(json_text)
    {
        if decision.decision.eq_ignore_ascii_case("Sleep") && decision.sleep_seconds.is_none() {
            // gate t-068: the fallback duration is the owner's constant, not a
            // steer-local literal re-typed next to the extraction it used to pair
            // with (`Some(5)` + `unwrap_or(5)`, no clamp anywhere).
            decision.sleep_seconds = Some(crate::tool_args::SLEEP_DEFAULT_SECS);
        }
        Some(decision)
    } else if let Some(tc) = reply
        .tool_calls
        .iter()
        .find(|tc| crate::tool_names::is_sleep_tool_name(&tc.function.name))
    {
        Some(sleep_steer_decision(&tc.function.arguments))
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
                crate::tool_names::TOOL_SLEEP => "Sleep",
                "delegatetask" | "delegate" => "DelegateTask",
                "queueandcontinue" | "queue" => "QueueAndContinue",
                _ => "Unknown",
            }
        }
    }
}

/// Canonical comparison key for a per-subtask `action` string.
///
/// Same folding rules as [`normalize_steer_decision`]: trim, ASCII lowercase,
/// and drop `_`, `-` and spaces, so `Cancel Task`, `cancel_task`, `CANCEL-task`
/// and `CancelTask` are one and the same action.
fn steer_action_key(action: &str) -> String {
    action
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-', ' '], "")
}

/// Canonical form of a per-subtask steer `action` (H3).
///
/// [`SteerSubtaskDecision`] is the single owner of the subtask action vocabulary
/// — `ForwardNotice` | `Cancel` | `DelegateTask` | `Sleep` — so its canonical
/// form lives in this module. Every consumer (the preemption handle,
/// [`extract_tasks_to_delegate`], the UI bridge subtask loops) must route the raw
/// JSON string through [`normalize_steer_subtask_action`] instead of comparing
/// the raw string, otherwise a differently-cased or underscored spelling silently
/// selects the default branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SteerSubtaskAction {
    /// Post the steering notice to the running worker and await its reply.
    ForwardNotice,
    /// Cancel the running worker/subtask (fires its cancellation token).
    Cancel,
    /// Delegate a new ad-hoc subtask.
    DelegateTask,
    /// Sleep for `sleep_seconds`.
    Sleep,
    /// Outside the vocabulary: rejected and logged, never silently defaulted.
    Unknown,
}

impl SteerSubtaskAction {
    /// The canonical spelling used on the wire and in logs.
    pub fn canonical(self) -> &'static str {
        match self {
            Self::ForwardNotice => "ForwardNotice",
            Self::Cancel => "Cancel",
            Self::DelegateTask => "DelegateTask",
            Self::Sleep => "Sleep",
            Self::Unknown => "Unknown",
        }
    }

    /// `true` for a vocabulary action, `false` for a rejected spelling.
    pub fn is_known(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

impl std::fmt::Display for SteerSubtaskAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.canonical())
    }
}

/// Rejection returned by [`SteerSubtaskAction::from_str`] for an action spelling
/// outside the subtask vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSteerSubtaskAction {
    /// The raw spelling that was rejected.
    pub raw: String,
}

impl std::fmt::Display for UnknownSteerSubtaskAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unrecognized steer subtask action {:?} (expected one of: ForwardNotice, Cancel, DelegateTask, Sleep)",
            self.raw
        )
    }
}

impl std::error::Error for UnknownSteerSubtaskAction {}

impl std::str::FromStr for SteerSubtaskAction {
    type Err = UnknownSteerSubtaskAction;

    fn from_str(action: &str) -> Result<Self, Self::Err> {
        match steer_action_key(action).as_str() {
            "cancel" | "canceltask" | "cancelsubtask" | "cancelworker" | "cancelstream"
            | "abort" | "aborttask" | "abortimmediately" | "abortstream" | "stop" | "stoptask"
            | "terminate" | "terminatetask" => Ok(Self::Cancel),
            "forwardnotice" | "forward" | "forwardtoworker" | "forwardmessage" | "sendnotice"
            | "postnotice" | "notice" | "notifyworker" | "replytoarbitrator"
            | "answerarbitrator" => Ok(Self::ForwardNotice),
            "delegatetask" | "delegate" | "delegatenewtask" | "newtask" | "newsubtask"
            | "createtask" | "starttask" | "spawntask" => Ok(Self::DelegateTask),
            // t-061: the *tool* spelling of the Sleep action is not a steer-private
            // vocabulary — it comes from the one tool-alias table in
            // `crate::tool_names` (see `TOOL_ALIAS_TABLE`), through the
            // grammar-tolerant matcher, because this grammar folds case and
            // `_`/`-`/space. The other arms are subtask **actions**, which are a
            // separate vocabulary and deliberately do not consult the alias table.
            key if crate::tool_names::is_sleep_tool_grammar_spelling(key) => Ok(Self::Sleep),
            _ => Err(UnknownSteerSubtaskAction {
                raw: action.to_string(),
            }),
        }
    }
}

/// Normalize a per-subtask steer `action` (H3): the one shared entry point every
/// consumer must use.
///
/// Recognized spellings fold to a [`SteerSubtaskAction`] variant; anything
/// outside the vocabulary is an **explicit rejection** — a `WARN` naming the raw
/// action, the affected `tool_call_id` and the accepted vocabulary — and yields
/// [`SteerSubtaskAction::Unknown`], which no action branch matches. Callers must
/// treat `Unknown` as their documented fallback (do nothing to that subtask)
/// rather than letting the raw string fall through to a default branch.
pub fn normalize_steer_subtask_action(action: &str, tool_call_id: &str) -> SteerSubtaskAction {
    match action.parse() {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::warn!(
                action = action,
                tool_call_id = tool_call_id,
                accepted = "ForwardNotice | Cancel | DelegateTask | Sleep",
                reason = %err,
                "Rejecting steer subtask action outside the vocabulary — no action branch is taken"
            );
            SteerSubtaskAction::Unknown
        }
    }
}

/// Prose markers that mark a recorded steering entry as **pending**, i.e. still
/// awaiting a specialist reply. Owned here because the entries carrying them are
/// produced by the steering/arbitration path (`ui::bridge` records
/// "Forwarded notice … (awaiting specialist reply)"; the arbitrator records
/// "Decision: …") and only the steering-history correlation consumes them.
const PENDING_ENTRY_MARKERS: [&str; 3] =
    ["awaiting specialist reply", "ForwardToWorker", "follow-up"];

/// True while a recorded steering entry is still awaiting a specialist reply and
/// is therefore a legitimate target for a `record_steering_exchange` update.
pub(crate) fn steering_entry_is_pending(entry_response: &str) -> bool {
    entry_response.starts_with("Decision:")
        || PENDING_ENTRY_MARKERS
            .iter()
            .any(|marker| entry_response.contains(*marker))
}

/// Characters that continue an identifier token such as `t-1`, `notice-10` or
/// `coder-t-001`. Used to anchor [`entry_names_notice_id`] at token boundaries.
///
/// `.` counts as part of an identifier because the canonical task-id grammar
/// allows an inner dot (`t-val-01`, `task-t-001`); the hazard being closed here
/// is one id being a prefix of another (`t-1` / `t-10`), which is a letter or
/// digit boundary problem, not a punctuation one.
fn is_identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')
}

/// True when `notice_id` occurs in a recorded entry's arbitrator text as a
/// **whole identifier token** — the exact keyed test that replaced
/// `entry_text.contains(notice_id)` (recon M3, task t-071).
///
/// The boundary anchoring is the whole point: an unanchored `contains` let a
/// reply for `notice-1` rewrite the entry recorded for `notice-10` — the same
/// class of mis-attribution that the exact routing rules in
/// `WorkerRoutingIdentity::routes` and `workers::worker_matches` removed for
/// worker keys. Matching is case- and byte-exact, because notice ids are stored
/// verbatim (`PENDING_NOTICES` is keyed by the id itself).
pub(crate) fn entry_names_notice_id(entry_response: &str, notice_id: &str) -> bool {
    let notice_id = notice_id.trim();
    if notice_id.is_empty() {
        return false;
    }
    entry_response
        .match_indices(notice_id)
        .any(|(index, matched)| {
            let before = entry_response[..index].chars().next_back();
            let after = entry_response[index + matched.len()..].chars().next();
            !before.is_some_and(is_identifier_char) && !after.is_some_and(is_identifier_char)
        })
}

/// True when `incoming` and `recorded` are the **same** steering inquiry.
///
/// The inquiry is genuinely part of the steering-history contract (an entry is
/// keyed by the user message that created it), so it is compared as one whole
/// string with an explicitly documented normalization — trim, then ASCII
/// case-fold — and nothing else. No containment in either direction: the old
/// `inq.contains(q) || q.contains(inq)` test made `t-1` the same inquiry as
/// `t-10`, made any empty inquiry match every entry (`contains("")`), and let a
/// prose prefix such as "Deploy the new" absorb the entry recorded for
/// "Deploy the new allocator".
pub(crate) fn same_steering_inquiry(incoming: &str, recorded: &str) -> bool {
    let incoming = incoming.trim().to_ascii_lowercase();
    let recorded = recorded.trim().to_ascii_lowercase();
    !incoming.is_empty() && !recorded.is_empty() && incoming == recorded
}

/// Format the accumulated steering conversation history into a readable transcript for SteerContext.
///
/// Pure rendering: an entry's `(inquiry, arbitrator response)` pair is emitted
/// verbatim (trimmed for display) and this function performs **no** correlation
/// or de-duplication of its own. Which recorded entry an incoming exchange
/// belongs to is decided once, by identity key, in
/// `orchestrator::bus::record_steering_exchange` — using
/// [`entry_names_notice_id`] / [`same_steering_inquiry`] — so the transcript
/// cannot silently merge two steers on a partial text overlap.
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
///
/// The per-subtask `action` is routed through [`normalize_steer_subtask_action`]
/// (H3): only a canonical [`SteerSubtaskAction::DelegateTask`] delegates, and a
/// rejected action never silently selects the implicit top-level fallback.
pub fn extract_tasks_to_delegate(
    decision: &SteerDecision,
    user_msg: &str,
) -> Vec<(Agent, String, String)> {
    let mut tasks = Vec::new();
    let actions: Vec<SteerSubtaskAction> = decision
        .subtasks
        .iter()
        .map(|s| normalize_steer_subtask_action(&s.action, &s.tool_call_id))
        .collect();
    let has_explicit_subtask_delegations = actions.contains(&SteerSubtaskAction::DelegateTask);

    if has_explicit_subtask_delegations {
        let mut idx = 1;
        for (s, action) in decision.subtasks.iter().zip(actions.iter()) {
            if *action == SteerSubtaskAction::DelegateTask {
                let agent = s
                    .agent_name
                    .as_deref()
                    .and_then(Agent::from_str)
                    .unwrap_or(Agent::Coder);
                let cleaned = crate::task_id::normalize_task_id_ref(&s.tool_call_id).to_string();
                let tid = if !cleaned.is_empty() {
                    cleaned
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
        if let Some(rejected) = decision
            .subtasks
            .iter()
            .zip(actions.iter())
            .find(|(_, action)| !action.is_known())
        {
            // H3: an action outside the vocabulary is a rejection, not a signal to
            // build a delegation out of the first subtask. Refuse the fallback.
            tracing::warn!(
                action = %rejected.0.action,
                tool_call_id = %rejected.0.tool_call_id,
                "Rejected steer subtask action — skipping implicit top-level DelegateTask fallback"
            );
            return tasks;
        }
        let agent = decision
            .subtasks
            .iter()
            .find_map(|s| s.agent_name.as_deref().and_then(Agent::from_str))
            .unwrap_or(Agent::Coder);
        let tid = decision
            .subtasks
            .first()
            .and_then(|s| crate::task_id::normalize_task_id(&s.tool_call_id))
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
