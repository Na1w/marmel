//! Shared validator/fix-loop driver.
//!
//! **Consolidation (duplicates.md §6b):** three near-identical copies of the
//! "LLM turn → tool dispatch → verdict" loop existed:
//! `run_automated_validation_inner` and `run_plan_validation_inner`
//! (validation.rs) plus the specialist streaming turn in `runner/execution.rs`.
//! They had already drifted semantically (abort → `Ok(false, …)` vs `Err(…)`),
//! which is exactly the class of bug this consolidation eliminates.
//!
//! ## Abort-semantics decision (deliberate, documented)
//!
//! **All abort/cancellation paths now return `Err` (strict failure), never
//! `Ok(false, …)` (soft rejection).** Rationale:
//! - A cancelled validation is NOT a verdict. Returning `Ok(false)` ("rejected")
//!   would let downstream logic treat a user-initiated abort as an authoritative
//!   negative verdict, potentially triggering revision loops or rejections that
//!   the user never asked for.
//! - `Err` propagates to the caller, which already special-cases cancellation
//!   (e.g. `harness/plan.rs` checks `token.is_cancelled()` before treating an
//!   `Err` as a skip) — so the strict path is fully supported by all consumers.
//! - The deliverable loop and the plan loop previously disagreed on this; the
//!   stricter `Err` behavior (previously used by the deliverable loop) wins.
//!
//! Likewise, a hard LLM/backend error now always returns `Err` (previously the
//! deliverable loop `break`-ed out of its turn loop and the post-loop fallback
//! *assumed approved* — the unsafe direction for a validator).

use crate::agents::runner::formatting::{format_tool_args_full, format_tool_args_preview};
use crate::agents::validation::{is_leave_verdict_tool, parse_verdict_args};
use crate::harness::monitor::{HarnessMonitor, Intervention, RepetitionDetector};
use crate::manager::ContextEngine;
use crate::orchestrator::{
    ActiveWorkerGuard, PreemptibleStreamSink, emit_status, is_current_or_global_cancelled,
    register_active_worker_with_token, update_active_worker_context,
};
use crate::types::{ChatRequest, Message, ToolCall, ToolDef};
use crate::{harness, llm, tool_names};

/// Outcome of one fix-loop run, before the caller maps it to its own
/// `(bool, String)` contract or error type.
pub enum FixLoopResult {
    /// The model submitted a `leave_verdict` (or the caller's verdict callback
    /// fired): `(approved, critique)`.
    Verdict { approved: bool, critique: String },
    /// Turn budget exhausted without an explicit verdict.
    Exhausted,
    /// Aborted by cancellation / steer preemption. Callers MUST surface this as
    /// a failure (see module docs for the strict-abort decision).
    Aborted,
}

/// Everything the shared driver needs to run the turn loop.
///
/// Built by thin wrappers (validation.rs / execution.rs) so each call site
/// keeps only its prompt/brief/status specifics.
pub struct LoopParams<'a> {
    pub client: &'a llm::ChatClient,
    pub model: String,
    /// Stream/worker tag, e.g. `validator-coder-t-xxx` or `coder-t-xxx`.
    pub tag: String,
    /// Worker name shown in the stream registry, e.g. `validator-coder`.
    pub worker_name: String,
    pub task_id: Option<String>,
    /// Active-worker registry key (the `ActiveWorkerGuard.0` value).
    pub worker_key: String,
    pub engine: &'a mut ContextEngine,
    pub tools: Vec<ToolDef>,
    pub token: &'a tokio_util::sync::CancellationToken,
    pub mon_cfg: &'a crate::config::MonitoringConfig,
    pub cfg: &'a crate::config::Config,
    /// Sampling temperature (0.0 for validators, cfg.temperature for specialists).
    pub temperature: f32,
    /// Status line emitted once per turn, e.g.
    /// `"{tag}: evaluating test & inspection output (turn {turn})..."`.
    /// The driver appends ` (turn N)` automatically; keep the trailing `...`.
    pub status_template: String,
    /// Critique used when `leave_verdict` args cannot be parsed.
    pub default_verdict_critique: String,
    /// Role label for `debug_log::log_validation_verdict`.
    pub verdict_log_role: String,
    /// Whether to dispatch `leave_verdict` as a regular tool (deliverable loop)
    /// or skip it (plan loop, which treats it as terminal-only).
    pub dispatch_verdict_tools: bool,
    /// Prefix for the "aborted before tool {name}" warn log.
    pub abort_log_prefix: String,
    /// Whether to emit a per-tool `emit_status` line (deliverable/specialist
    /// loops do; the plan auditor loop historically did not).
    pub emit_tool_status: bool,
    /// User-level notice injected when a `rebirth` call succeeds (wording
    /// differs between validator and specialist loops).
    pub rebirth_notice: String,
}

/// Build the canonical `ChatRequest` shared by all fix-loop turn calls
/// (previously a 10-line literal duplicated ×3).
pub fn build_turn_request(
    model: &str,
    engine: &ContextEngine,
    tools: &[ToolDef],
    cfg: &crate::config::Config,
    temperature: f32,
) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: engine.messages().to_vec(),
        tools: Some(tools.to_vec()),
        stream: Some(true),
        enable_thinking: None,
        temperature: Some(temperature),
        top_p: Some(cfg.top_p),
        presence_penalty: Some(cfg.presence_penalty),
        frequency_penalty: Some(cfg.frequency_penalty),
    }
}

/// Assemble the tool list: `ToolDef::default_tools()` filtered by either an
/// explicit prompt-blueprint allow-list or the registry entry's namespaces,
/// plus MCP tools for the given servers. (Previously duplicated ×3.)
pub fn assemble_tools(
    allowed_tools: Option<&[String]>,
    entry_allows: impl Fn(&str) -> bool,
    mcp_servers: &[String],
) -> Vec<ToolDef> {
    let mut tools = Vec::new();
    for tool in ToolDef::default_tools() {
        let is_allowed = if tool.function.name == crate::tool_names::TOOL_REPLY_TO_ARBITRATOR {
            true
        } else if let Some(allowed) = allowed_tools {
            allowed.iter().any(|t| {
                let norm = harness::normalize_tool_name(t);
                norm == tool.function.name || t == &tool.function.name
            })
        } else {
            entry_allows(&tool.function.name)
        };
        if is_allowed {
            tools.push(tool);
        }
    }
    if let Some(mcp) = harness::get_mcp_manager() {
        for tool in mcp.tools_for_servers(mcp_servers) {
            tools.push(ToolDef::from_mcp(&tool));
        }
    }
    tools
}

/// Resolve the validator backend/token/model fallback chain
/// (specialist override → validator entry → global config).
/// Previously duplicated ×2 in validation.rs.
pub fn resolve_validator_backend(
    cfg: &crate::config::Config,
    specialist: crate::agents::Agent,
) -> (String, String, String) {
    let specialist_cfg = cfg.orchestration.specialists.get(specialist.as_str());
    let validator_cfg = cfg
        .orchestration
        .specialists
        .get(crate::agents::Agent::Validator.as_str());
    let backend = specialist_cfg
        .and_then(|sc| sc.validator_backend_url.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.backend_url.as_ref()))
        .unwrap_or(&cfg.backend_url)
        .to_string();
    let token = specialist_cfg
        .and_then(|sc| sc.validator_auth_token.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.auth_token.as_ref()))
        .unwrap_or(&cfg.auth_token)
        .to_string();
    let model = specialist_cfg
        .and_then(|sc| sc.validator_model.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.model.as_ref()))
        .cloned()
        .unwrap_or_else(|| cfg.model.clone());
    (backend, token, model)
}

/// Shared tool-dispatch core: monitor intervention check → cancellation check
/// → `dispatch_for_async_with_engine`. Returns `(content, execution_succeeded)`
/// where `content` starts with `"ERROR: "` on failure. Previously duplicated
/// across the deliverable loop, the plan loop, and the specialist loop.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_tool_call(
    monitor: &mut HarnessMonitor,
    tc: &ToolCall,
    caller: harness::ToolCaller,
    engine: &mut ContextEngine,
    token: &tokio_util::sync::CancellationToken,
    abort_log_prefix: &str,
    emit_tool_status: bool,
    error_prefix: &str,
    intervention_fallback: &str,
) -> Option<(String, bool)> {
    let args_val: serde_json::Value = serde_json::from_str(&tc.function.arguments)
        .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
    if emit_tool_status {
        let desc = format_tool_args_preview(&tc.function.name, &args_val);
        emit_status(format!(
            "{}: running {}({desc})",
            abort_log_prefix, tc.function.name
        ));
    }
    let full_args = format_tool_args_full(&tc.function.name, &args_val);
    tracing::info!(
        "{abort_log_prefix} invoking tool: {}({full_args})",
        tc.function.name
    );

    let intervention = monitor.observe_tool(&tc.function.name, &args_val);
    let content = match intervention {
        Intervention::Block | Intervention::Cut => {
            // `intervention_fallback` is a template containing `{tool}`.
            let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                format!(
                    "{error_prefix}{}",
                    intervention_fallback.replace("{tool}", &tc.function.name)
                )
            });
            tracing::warn!(
                "{abort_log_prefix} tool {} blocked by repetition detector",
                tc.function.name
            );
            err_msg
        }
        Intervention::None => {
            if token.is_cancelled() || is_current_or_global_cancelled() {
                tracing::warn!(
                    "{abort_log_prefix}: aborted before dispatching tool {}",
                    tc.function.name
                );
                return None;
            }
            let invocation = harness::ToolInvocation {
                name: tc.function.name.clone(),
                arguments: args_val,
            };
            match harness::dispatch_for_async_with_engine(&invocation, caller, Some(engine)).await {
                Ok(r) => {
                    tracing::info!(
                        "{abort_log_prefix} tool {} completed with {} chars",
                        tc.function.name,
                        r.content.len()
                    );
                    r.content
                }
                Err(e) => {
                    tracing::warn!("{abort_log_prefix} tool {} error: {e}", tc.function.name);
                    format!("{error_prefix}{e}")
                }
            }
        }
    };
    let execution_succeeded = !content.starts_with(error_prefix);
    Some((content, execution_succeeded))
}

/// Append a tool result to the engine with the shared `rebirth` special-case:
/// a *successful* rebirth replaces the tool result with a user-level
/// checkpoint notice instead of a `Tool` message (a failed rebirth is kept as
/// a normal `Tool` error result).
pub fn append_tool_result(
    engine: &mut ContextEngine,
    tc: &ToolCall,
    content: String,
    execution_succeeded: bool,
    rebirth_notice: &str,
) {
    let is_rebirth = tc.function.name == tool_names::TOOL_REBIRTH;
    if is_rebirth && execution_succeeded {
        engine.append(Message::User {
            content: rebirth_notice.to_string(),
        });
    } else {
        engine.append(Message::Tool {
            tool_call_id: tc.id.clone(),
            content,
        });
    }
}

/// The shared fix-loop driver: cancel check → worker context update → status
/// emit → streaming LLM turn → abort/steer handling → XML rescue → assistant
/// folding → verdict detection → nudge-on-silence → tool dispatch →
/// compaction. One implementation for the deliverable validator, the plan
/// auditor, and (via `stream_single_turn`) the specialist turn.
///
/// Returns `Err` on hard LLM errors and maps aborts to `FixLoopResult::Aborted`
/// (strict semantics — see module docs).
pub async fn run_fix_loop(
    p: &mut LoopParams<'_>,
    monitor: &mut HarnessMonitor,
    caller: harness::ToolCaller,
) -> Result<FixLoopResult, anyhow::Error> {
    let mut rep_detector =
        RepetitionDetector::new(p.mon_cfg.repetition_threshold, p.mon_cfg.min_pattern_len);
    let mut verdict_nudge_count = 0usize;
    let mut turn = 0usize;

    loop {
        turn += 1;
        // Behavior-preserving: the original validator loops checked only their
        // own token here (not the global token). The stricter abort *semantics*
        // (Err, not Ok(false)) is applied below; the *trigger* stays token-only
        // to avoid new aborts from unrelated global cancels in parallel runs.
        if p.token.is_cancelled() {
            tracing::warn!("{}: aborted by cancellation token", p.tag);
            return Ok(FixLoopResult::Aborted);
        }
        update_active_worker_context(&p.worker_key, p.engine.token_count());
        emit_status(format!("{} (turn {turn})", p.status_template));

        let notices = crate::orchestrator::drain_worker_notices(&p.worker_key);
        for notice in notices {
            p.engine.append(Message::User {
                content: format!(
                    "[Steering Notice from Arbitrator — ID: {}]:\n\"{}\"\n\n\
                    To reply to the Arbitrator regarding this notice, invoke the 'reply_to_arbitrator' tool with `notice_id: \"{}\"` and your `message`.",
                    notice.notice_id, notice.user_inquiry, notice.notice_id
                ),
            });
        }

        let req = build_turn_request(&p.model, p.engine, &p.tools, p.cfg, p.temperature);

        let max_tokens = p.mon_cfg.max_stream_tokens.max(256);
        let max_thinking_tokens = p.mon_cfg.max_thinking_tokens.max(256);
        let mut sink = PreemptibleStreamSink::register_full(
            &p.tag,
            Some(p.worker_name.clone()),
            p.task_id.clone(),
            Some(p.token.clone()),
            &p.model,
        );
        let stream_out = llm::chat_stream_resumable(
            p.client,
            &req,
            &mut sink,
            max_tokens,
            max_thinking_tokens,
            &mut rep_detector,
            false,
            Some(p.token),
        )
        .await;

        let out = match stream_out {
            Ok(o) => o,
            Err(e) => {
                // Strict semantics: a cancelled stream is an abort; any other
                // transport/backend error is a hard failure (previously the
                // deliverable loop broke out and *assumed approved* — unsafe).
                if p.token.is_cancelled() || is_current_or_global_cancelled() {
                    tracing::warn!("{}: aborted during LLM call", p.tag);
                    return Ok(FixLoopResult::Aborted);
                }
                tracing::error!("{} LLM chat call error on turn {turn}: {e:?}", p.tag);
                return Err(e);
            }
        };

        // Behavior-preserving: steer + own-token only (matches both original
        // validator loops); global cancel is honored at the tool-dispatch step.
        if out.was_aborted_by_steer || p.token.is_cancelled() {
            tracing::warn!("{}: aborted during LLM call", p.tag);
            return Ok(FixLoopResult::Aborted);
        }

        let reply = out.reply;
        if out.budget_exceeded {
            tracing::warn!(
                "{}: maximum single-turn output budget of {max_tokens} tokens exceeded",
                p.tag
            );
        }
        if out.thinking_budget_exceeded {
            tracing::warn!(
                "{}: maximum single-turn reasoning budget of {max_thinking_tokens} tokens exceeded",
                p.tag
            );
            crate::orchestrator::emit_status(format!(
                "{}: reasoning budget ({max_thinking_tokens} tokens) reached — nudging out of thinking",
                p.tag
            ));
        }

        let mut tool_calls = reply.tool_calls.clone();
        if tool_calls.is_empty() && p.cfg.enable_xml_rescue {
            let rescued = monitor.rescue_xml(&reply.content);
            if !rescued.is_empty() {
                tool_calls = rescued;
            }
        }

        p.engine.append(Message::Assistant {
            content: Some(reply.content.clone()),
            reasoning_content: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning.clone())
            },
            tool_calls: tool_calls.clone(),
        });

        for tc in &tool_calls {
            if is_leave_verdict_tool(&tc.function.name) {
                let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
                let (approved, critique) = parse_verdict_args(&args_val)
                    .unwrap_or((true, p.default_verdict_critique.clone()));
                crate::debug_log::log_validation_verdict(&p.verdict_log_role, approved, &critique);
                tracing::info!(
                    "Validator recorded verdict via leave_verdict: approved={}, critique:\n{}",
                    approved,
                    critique
                );
                return Ok(FixLoopResult::Verdict { approved, critique });
            }
        }

        if tool_calls.is_empty() {
            if verdict_nudge_count < 3 {
                verdict_nudge_count += 1;
                let notice = if out.thinking_budget_exceeded {
                    format!(
                        "SYSTEM NOTICE: Maximum reasoning budget of {max_thinking_tokens} tokens reached for this turn. Stop internal reasoning immediately. You have not submitted a verdict using the 'leave_verdict' tool (reminder {verdict_nudge_count}/3). Proceed directly to call the 'leave_verdict' tool with verdict ('APPROVED' or 'REJECTED') and comments, or invoke required inspection tools."
                    )
                } else {
                    format!(
                        "System: You have not submitted a verdict using the 'leave_verdict' tool (reminder {verdict_nudge_count}/3). Do not output text. If your analysis and verification are complete, you MUST call the 'leave_verdict' tool with verdict ('APPROVED' or 'REJECTED') and comments. If you need to perform further verification, invoke the appropriate tools."
                    )
                };
                p.engine.append(Message::User { content: notice });
                continue;
            } else {
                tracing::info!(
                    "{} did not invoke leave_verdict after 3 reminders; assuming approved.",
                    p.tag
                );
                return Ok(FixLoopResult::Verdict {
                    approved: true,
                    critique: "Validator completed verification without calling leave_verdict after 3 reminders; assumed approved.".to_string(),
                });
            }
        }

        for tc in &tool_calls {
            if !p.dispatch_verdict_tools && is_leave_verdict_tool(&tc.function.name) {
                continue;
            }
            if p.token.is_cancelled() || is_current_or_global_cancelled() {
                tracing::warn!(
                    "{}: aborted before tool {}",
                    p.abort_log_prefix,
                    tc.function.name
                );
                return Ok(FixLoopResult::Aborted);
            }
            match dispatch_tool_call(
                monitor,
                tc,
                caller.clone(),
                p.engine,
                p.token,
                &p.abort_log_prefix,
                p.emit_tool_status,
                "ERROR: ",
                "Tool repetition detected for '{tool}'. Conclude by calling leave_verdict or proceed with the task.",
            )
            .await
            {
                Some((content, succeeded)) => {
                    append_tool_result(p.engine, tc, content, succeeded, &p.rebirth_notice);
                }
                None => {
                    return Ok(FixLoopResult::Aborted);
                }
            }
            update_active_worker_context(&p.worker_key, p.engine.token_count());
        }
        if p.engine.should_compact() {
            p.engine.compact();
        } else if p.engine.should_advise_rebirth() {
            p.engine.inject_rebirth_advisory();
        }
    }
}

/// One streaming LLM turn (request build + sink registration +
/// `chat_stream_resumable` + abort classification). Used by the specialist
/// execution loop, which keeps its own tool-dispatch/revision state machine
/// but shares the streaming plumbing with the validator loops.
///
/// Returns `Err` on hard errors; `Ok(None)` on abort/cancellation.
#[allow(clippy::too_many_arguments)]
pub async fn stream_single_turn(
    client: &llm::ChatClient,
    req: &ChatRequest,
    tag: &str,
    worker_name: &str,
    model: &str,
    task_id: Option<String>,
    token: &tokio_util::sync::CancellationToken,
    max_tokens: usize,
    max_thinking_tokens: usize,
    rep_detector: &mut RepetitionDetector,
) -> Result<Option<llm::ResumableStreamOutput>, anyhow::Error> {
    let mut sink = PreemptibleStreamSink::register_full(
        tag,
        Some(worker_name.to_string()),
        task_id,
        Some(token.clone()),
        model,
    );
    let out = llm::chat_stream_resumable(
        client,
        req,
        &mut sink,
        max_tokens,
        max_thinking_tokens,
        rep_detector,
        false,
        Some(token),
    )
    .await;
    match out {
        Ok(o) => {
            if o.was_aborted_by_steer || token.is_cancelled() || is_current_or_global_cancelled() {
                tracing::warn!("{tag}: aborted during LLM call");
                Ok(None)
            } else {
                Ok(Some(o))
            }
        }
        Err(e) => {
            if token.is_cancelled() || is_current_or_global_cancelled() {
                tracing::warn!("{tag}: aborted during LLM call");
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// Register the active worker for a fix-loop run (shared scaffolding).
pub fn register_loop_worker(
    task_id: Option<String>,
    worker_name: String,
    description: String,
    token: &tokio_util::sync::CancellationToken,
) -> ActiveWorkerGuard {
    register_active_worker_with_token(task_id, worker_name, description, Some(token.clone()))
}
