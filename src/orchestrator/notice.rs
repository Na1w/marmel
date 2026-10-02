//! Steer notice correlation, worker inbox queuing, and arbitrator-mediated dialogue.
//!
//! Enables bi-directional steering: when the Steer Arbitrator forwards a user
//! inquiry to an active worker, a unique `notice_id` is assigned and queued to the
//! worker's inbox. The worker can inspect the notice and reply using the
//! `reply_to_arbitrator` tool. The Arbitrator mediates the reply (synthesizing
//! the response for the user or posing internal follow-ups) before presenting it.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::harness::HarnessStats;
use crate::llm::ChatClient;
use crate::types::{ChatRequest, Message};

static NEXT_NOTICE_ID: AtomicU64 = AtomicU64::new(1);

/// Generate the next unique steering notice identifier.
pub fn next_notice_id() -> String {
    let id = NEXT_NOTICE_ID.fetch_add(1, Ordering::SeqCst);
    format!("notice-{id}")
}

/// A steering notice dispatched by the Steer Arbitrator to an active specialist worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SteerNotice {
    pub notice_id: String,
    pub user_inquiry: String,
    pub target_worker: String,
    pub created_at_ms: u64,
}

/// A specialist worker's reply back to the Steer Arbitrator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SteerNoticeReply {
    pub notice_id: String,
    pub worker_tag: String,
    pub reply_message: String,
    pub replied_at_ms: u64,
}

/// The Arbitrator's evaluation of a specialist's reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerReplyEvaluation {
    /// "SynthesizeResponse" | "AskFollowUp"
    pub decision: String,
    /// Direct, factual response synthesized for the user (in user's language).
    pub response: Option<String>,
    /// Targeted follow-up question for the worker (in English) if more info is needed before answering the user.
    pub follow_up_prompt: Option<String>,
    /// Optional status notice to display to the user while follow-up is in flight.
    pub user_status: Option<String>,
}

static PENDING_NOTICES: LazyLock<DashMap<String, SteerNotice>> = LazyLock::new(DashMap::new);
static WORKER_INBOXES: LazyLock<DashMap<String, Vec<SteerNotice>>> = LazyLock::new(DashMap::new);
static REPLIED_NOTICES: LazyLock<DashMap<String, SteerNoticeReply>> = LazyLock::new(DashMap::new);

#[cfg(test)]
pub static TEST_NOTICE_MUTEX: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Post a steering notice destined for a worker.
pub fn post_notice_to_worker(
    target_worker: &str,
    user_inquiry: &str,
    notice_id_override: Option<&str>,
) -> SteerNotice {
    let notice_id = notice_id_override
        .map(str::to_string)
        .unwrap_or_else(next_notice_id);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let clean_target = target_worker.trim().to_ascii_lowercase();
    let notice = SteerNotice {
        notice_id: notice_id.clone(),
        user_inquiry: user_inquiry.to_string(),
        target_worker: clean_target.clone(),
        created_at_ms: now_ms,
    };

    PENDING_NOTICES.insert(notice_id, notice.clone());

    WORKER_INBOXES
        .entry(clean_target)
        .or_default()
        .push(notice.clone());

    notice
}

/// Drain all pending notices matching the given worker key, tag, or role name.
pub fn drain_worker_notices(worker_key: &str) -> Vec<SteerNotice> {
    let key_lower = worker_key.trim().to_ascii_lowercase();
    let mut collected = Vec::new();

    for mut entry in WORKER_INBOXES.iter_mut() {
        let target = entry.key().to_ascii_lowercase();
        let matches = target == key_lower
            || target == "*"
            || target == "worker"
            || key_lower.starts_with(&format!("{target}-"))
            || key_lower.contains(&target);

        if matches {
            let drained: Vec<SteerNotice> = std::mem::take(entry.value_mut());
            collected.extend(drained);
        }
    }

    collected
}

/// Record a specialist worker's reply to a notice.
pub fn record_worker_reply(
    worker_tag: &str,
    notice_id: &str,
    message: &str,
) -> Result<SteerNotice, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let reply = SteerNoticeReply {
        notice_id: notice_id.to_string(),
        worker_tag: worker_tag.to_string(),
        reply_message: message.to_string(),
        replied_at_ms: now_ms,
    };

    REPLIED_NOTICES.insert(notice_id.to_string(), reply);

    if let Some((_, notice)) = PENDING_NOTICES.remove(notice_id) {
        Ok(notice)
    } else if PENDING_NOTICES.len() == 1 {
        let first_key = PENDING_NOTICES.iter().next().map(|e| e.key().clone());
        if let Some(k) = first_key
            && let Some((_, notice)) = PENDING_NOTICES.remove(&k)
        {
            Ok(notice)
        } else {
            Err(format!(
                "Notice '{notice_id}' was not found in pending notices"
            ))
        }
    } else {
        Err(format!(
            "Notice '{notice_id}' was not found in pending notices"
        ))
    }
}

/// Get a pending notice by ID.
pub fn get_pending_notice(notice_id: &str) -> Option<SteerNotice> {
    PENDING_NOTICES.get(notice_id).map(|e| e.clone())
}

/// Get a recorded worker reply by notice ID.
pub fn get_worker_reply(notice_id: &str) -> Option<SteerNoticeReply> {
    REPLIED_NOTICES.get(notice_id).map(|e| e.clone())
}

/// Clear all notices, inboxes, and replies (primarily for test isolation).
pub fn clear_all_notices() {
    PENDING_NOTICES.clear();
    WORKER_INBOXES.clear();
    REPLIED_NOTICES.clear();
}

/// Evaluate a specialist worker's reply through the Steer Arbitrator model:
/// either synthesize a factual direct answer for the user, or formulate an internal
/// follow-up question for the worker before presenting anything to the user.
pub async fn evaluate_worker_reply<F>(
    client: &ChatClient,
    stats: &HarnessStats,
    notice: &SteerNotice,
    worker_tag: &str,
    worker_reply: &str,
    mut on_delta: F,
) -> Result<WorkerReplyEvaluation, anyhow::Error>
where
    F: FnMut(&str) + Send,
{
    let system_prompt = "\
You are Marmel's Steer Arbitrator mediating between the user and active specialist workers.
The user previously sent a steering inquiry or instruction to a specialist worker.
The specialist worker has now replied back to YOU (the Arbitrator).

Your role:
Evaluate the worker's reply:
1. If the worker's reply directly and satisfactorily addresses the user's inquiry, choose 'SynthesizeResponse'.
   - In 'response', formulate a direct, factual, and concise answer to the user in the EXACT SAME LANGUAGE as the user's inquiry.
   - Be clear, polite, and factual. Zero filler, no conversational meta-disclaimers.
2. If the worker's reply is incomplete, ambiguous, contradictory, or raises a concern that requires clarification from the worker before answering the user, choose 'AskFollowUp'.
   - In 'follow_up_prompt', specify the clear, targeted question for the worker in English.
   - In 'user_status', provide a brief status notice in the user's language (e.g. 'Ställer en följdfråga till Coder för att reda ut detaljerna...').

You MUST output ONLY a valid JSON object matching:
{
  \"decision\": \"SynthesizeResponse\" | \"AskFollowUp\",
  \"response\": \"Direct answer to the user in their language (required if decision is SynthesizeResponse)\",
  \"follow_up_prompt\": \"Follow-up question for the worker in English (required if decision is AskFollowUp)\",
  \"user_status\": \"Brief status notice for the user (optional for AskFollowUp)\"
}";

    let user_prompt = format!(
        "Original User Inquiry:\n\"{}\"\n\nSpecialist Worker [{}] Reply to Arbitrator:\n\"{}\"\n\nPlease output your evaluation JSON.",
        notice.user_inquiry, worker_tag, worker_reply
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

    let mut full_accum = String::new();
    let reply_res = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        client.chat_stream(&req, |chunk| {
            full_accum.push_str(chunk);
            true
        }),
    )
    .await;

    let raw = match reply_res {
        Ok(Ok(r)) => {
            if !full_accum.trim().is_empty() {
                full_accum
            } else {
                r.content
            }
        }
        _ => String::new(),
    };

    let raw_trimmed = raw.trim();
    let json_text = if let Some(stripped) = raw_trimmed.strip_prefix("```json") {
        stripped.trim_end_matches("```").trim()
    } else if let Some(stripped) = raw_trimmed.strip_prefix("```") {
        stripped.trim_end_matches("```").trim()
    } else if let Some(start) = raw_trimmed.find('{') {
        if let Some(end) = raw_trimmed.rfind('}') {
            &raw_trimmed[start..=end]
        } else {
            raw_trimmed
        }
    } else {
        raw_trimmed
    };

    let parsed: Option<WorkerReplyEvaluation> = serde_json::from_str(json_text).ok();
    if let Some(eval) = parsed {
        stats.record_steer_arbitration();
        if eval.decision.eq_ignore_ascii_case("SynthesizeResponse")
            && let Some(ref resp) = eval.response
        {
            on_delta(resp);
        }
        return Ok(eval);
    }

    let fallback_resp = if !raw_trimmed.is_empty() && !raw_trimmed.starts_with('{') {
        raw_trimmed.to_string()
    } else {
        format!("Specialist [{worker_tag}] explains: {worker_reply}")
    };
    on_delta(&fallback_resp);
    stats.record_steer_arbitration();

    Ok(WorkerReplyEvaluation {
        decision: "SynthesizeResponse".to_string(),
        response: Some(fallback_resp),
        follow_up_prompt: None,
        user_status: None,
    })
}
