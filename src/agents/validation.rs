//! Automated validator loop and verdict evaluation for specialist deliverables.

use crate::agents::Agent;
use crate::agents::runner::{format_tool_args_full, format_tool_args_preview};
use crate::tool_names::TOOL_LEAVE_VERDICT;

/// The outcome of an automated validation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationOutcome {
    Approved { comments: String },
    Rejected { critique: String },
    Aborted,
}

impl ValidationOutcome {
    pub fn is_approved(&self) -> bool {
        matches!(self, ValidationOutcome::Approved { .. })
    }

    pub fn critique(&self) -> Option<&str> {
        match self {
            ValidationOutcome::Rejected { critique } => Some(critique),
            _ => None,
        }
    }
}

/// Helper to check if a tool name refers to `leave_verdict`.
pub fn is_leave_verdict_tool(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == TOOL_LEAVE_VERDICT
        || name == "leaveVerdict"
        || lower.ends_with("__leave_verdict")
        || lower.ends_with("_leave_verdict")
        || lower == "leave_verdict_tool"
}

/// Robustly parse (approved, critique) from `leave_verdict` arguments.
pub fn parse_verdict_args(args: &serde_json::Value) -> Option<(bool, String)> {
    // 1. Try to extract approval status from string/boolean fields
    let approved_opt = if let Some(v_str) = args
        .get("verdict")
        .or_else(|| args.get("status"))
        .or_else(|| args.get("decision"))
        .or_else(|| args.get("result"))
        .or_else(|| args.get("assessment"))
        .and_then(serde_json::Value::as_str)
    {
        let s = v_str
            .trim()
            .trim_matches(|c| c == '\'' || c == '"' || c == '`' || c == '.');
        if s.eq_ignore_ascii_case("APPROVED")
            || s.eq_ignore_ascii_case("APPROVE")
            || s.eq_ignore_ascii_case("PASS")
            || s.eq_ignore_ascii_case("PASSED")
            || s.eq_ignore_ascii_case("ACCEPTED")
            || s.eq_ignore_ascii_case("ACCEPT")
            || s.eq_ignore_ascii_case("SUCCESS")
            || s.eq_ignore_ascii_case("OK")
        {
            Some(true)
        } else if s.eq_ignore_ascii_case("REJECTED")
            || s.eq_ignore_ascii_case("REJECT")
            || s.eq_ignore_ascii_case("FAIL")
            || s.eq_ignore_ascii_case("FAILED")
            || s.eq_ignore_ascii_case("DECLINED")
            || s.eq_ignore_ascii_case("DECLINE")
            || s.eq_ignore_ascii_case("DISAPPROVED")
        {
            Some(false)
        } else {
            None
        }
    } else {
        args.get("verdict")
            .or_else(|| args.get("approved"))
            .or_else(|| args.get("is_approved"))
            .and_then(serde_json::Value::as_bool)
    };

    // 2. Extract comments / critique
    let comments = args
        .get("comments")
        .or_else(|| args.get("comment"))
        .or_else(|| args.get("feedback"))
        .or_else(|| args.get("reason"))
        .or_else(|| args.get("critique"))
        .or_else(|| args.get("details"))
        .or_else(|| args.get("explanation"))
        .or_else(|| args.get("message"))
        .or_else(|| args.get("summary"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // 3. If approval status was not explicitly detected from verdict field, infer from comments
    let approved = match approved_opt {
        Some(b) => b,
        None => {
            let upper = comments.to_ascii_uppercase();
            if upper.contains("REJECT") || upper.contains("FAILED") || upper.contains("FAILURE") {
                false
            } else if upper.contains("APPROV") || upper.contains("PASS") || upper.contains("LGTM") {
                true
            } else {
                // If the tool `leave_verdict` was called, assume approved if no rejection indicated
                true
            }
        }
    };

    let critique = if !comments.is_empty() {
        comments
    } else if approved {
        "Deliverable verified and approved.".to_string()
    } else {
        "Deliverable rejected by validator without detailed comments.".to_string()
    };

    Some((approved, critique))
}

pub(crate) async fn run_automated_validation(
    client: &crate::llm::ChatClient,
    agent: Agent,
    task_id: Option<&str>,
    task_brief: &str,
    deliverable: &str,
    cfg: &crate::config::Config,
    token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<(bool, String)> {
    crate::orchestrator::CURRENT_WORKER_TOKEN
        .scope(
            token.clone(),
            run_automated_validation_inner(
                client,
                agent,
                task_id,
                task_brief,
                deliverable,
                cfg,
                token,
            ),
        )
        .await
}

async fn run_automated_validation_inner(
    _client: &crate::llm::ChatClient,
    agent: Agent,
    task_id: Option<&str>,
    task_brief: &str,
    deliverable: &str,
    cfg: &crate::config::Config,
    token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<(bool, String)> {
    let validator_prompt = match agent {
        Agent::Coder => crate::agents::validator::VALIDATOR_CODER_ROLE_PROMPT,
        Agent::Debugger => crate::agents::validator::VALIDATOR_DEBUGGER_ROLE_PROMPT,
        Agent::Researcher => crate::agents::validator::VALIDATOR_RESEARCHER_ROLE_PROMPT,
        Agent::Generalist => crate::agents::validator::VALIDATOR_GENERALIST_ROLE_PROMPT,
        _ => crate::agents::validator::VALIDATOR_ROLE_PROMPT,
    };

    let specialist_cfg = cfg.orchestration.specialists.get(agent.as_str());
    let validator_cfg = cfg.orchestration.specialists.get(Agent::Validator.as_str());
    let validator_backend = specialist_cfg
        .and_then(|sc| sc.validator_backend_url.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.backend_url.as_ref()))
        .unwrap_or(&cfg.backend_url);
    let validator_token = specialist_cfg
        .and_then(|sc| sc.validator_auth_token.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.auth_token.as_ref()))
        .unwrap_or(&cfg.auth_token);
    let validator_model = specialist_cfg
        .and_then(|sc| sc.validator_model.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.model.as_ref()))
        .cloned()
        .unwrap_or_else(|| cfg.model.clone());

    let val_client = crate::llm::ChatClient::new_with_token(
        validator_backend,
        &validator_model,
        validator_token,
    );

    let brief = format!(
        "Task Brief:\n{}\n\nSpecialist Deliverable:\n{}\n\n\
         Instructions:\n\
         1. Inspect the workspace and examine files using available inspection tools (`read_file`, `grep_search`, `glob`) or interactive terminal sessions (`pty_*`).\n\
         2. You are an auditor: you cannot execute direct shell commands (`run_command`) or modify files (`write_file`, `replace`). Solely analyze, inspect, and provide feedback.\n\
         3. When your verification is complete, you MUST call the `leave_verdict` tool with `verdict` ('APPROVED' or 'REJECTED') and detailed `comments`.\n\
         4. If advised or when context usage is high (>= 80%), call the `rebirth` tool with your intermediate findings, inspected files, and current offsets/line numbers to preserve continuity without restarting.",
        task_brief, deliverable
    );

    let mut engine = crate::manager::ContextEngineFactory::new(cfg.max_context_tokens)
        .specialist_context(validator_prompt.to_string(), brief);

    let registry = crate::orchestrator::SpecialistRegistry::canonical();
    let val_entry = registry
        .resolve(Agent::Validator)
        .expect("validator is registered");
    let mut tools = Vec::new();
    for tool in crate::types::ToolDef::default_tools() {
        if val_entry.allows(&tool.function.name) {
            tools.push(tool);
        }
    }
    if let Some(mcp) = crate::harness::get_mcp_manager() {
        let servers = validator_cfg
            .map(|vc| vc.mcp_servers.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| specialist_cfg.map(|sc| sc.mcp_servers.clone()))
            .unwrap_or_default();
        for tool in mcp.tools_for_servers(&servers) {
            tools.push(crate::types::ToolDef::from_mcp(&tool));
        }
    }

    let clean_task_id = task_id
        .map(|t| {
            t.trim_matches(|c| {
                c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
            })
            .trim()
            .to_string()
        })
        .filter(|t| !t.is_empty());

    let val_tag = match &clean_task_id {
        Some(tid) => format!("validator-{agent}-{tid}"),
        None => format!("validator-{agent}"),
    };

    let _active_guard = crate::orchestrator::register_active_worker_with_token(
        clean_task_id.clone(),
        format!("validator-{agent}"),
        format!("Auditing {agent} deliverable"),
        Some(token.clone()),
    );

    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let mut monitor = crate::harness::monitor::HarnessMonitor::new_with_config(
        std::sync::Arc::new(crate::harness::HarnessStats::new()),
        mon_cfg,
    );
    let mut rep_detector = crate::harness::monitor::RepetitionDetector::new(
        mon_cfg.repetition_threshold,
        mon_cfg.min_pattern_len,
    );
    let mut verdict_nudge_count = 0usize;
    let mut _turn = 0usize;
    loop {
        _turn += 1;
        if token.is_cancelled() {
            tracing::warn!("{val_tag}: aborted by cancellation token");
            return Err(anyhow::anyhow!(
                "Validation aborted by cancellation signal."
            ));
        }
        crate::orchestrator::update_active_worker_context(&_active_guard.0, engine.token_count());
        crate::orchestrator::emit_status(format!(
            "{val_tag}: evaluating test & inspection output (turn {_turn})...",
        ));
        let req = crate::types::ChatRequest {
            model: validator_model.clone(),
            messages: engine.messages().to_vec(),
            tools: Some(tools.clone()),
            stream: Some(true),
            enable_thinking: None,
            temperature: Some(0.0),
            top_p: Some(cfg.top_p),
            presence_penalty: Some(cfg.presence_penalty),
            frequency_penalty: Some(cfg.frequency_penalty),
        };

        let max_tokens = mon_cfg.max_stream_tokens.max(256);
        let max_thinking_tokens = mon_cfg.max_thinking_tokens.max(256);
        let mut sink = crate::orchestrator::PreemptibleStreamSink::register_full(
            &val_tag,
            Some(format!("validator-{agent}")),
            clean_task_id.clone(),
            Some(token.clone()),
            &validator_model,
        );
        let stream_out = crate::llm::chat_stream_resumable(
            &val_client,
            &req,
            &mut sink,
            max_tokens,
            max_thinking_tokens,
            &mut rep_detector,
            false,
            Some(token),
        )
        .await;

        let out = match stream_out {
            Ok(o) => o,
            Err(e) => {
                if token.is_cancelled() {
                    tracing::warn!("validator-{agent}: aborted during LLM call");
                    return Err(anyhow::anyhow!(
                        "Validation aborted by cancellation signal."
                    ));
                }
                tracing::error!("validator-{agent} LLM chat call error on turn {_turn}: {e:?}");
                break;
            }
        };

        if out.was_aborted_by_steer || token.is_cancelled() {
            tracing::warn!("validator-{agent}: aborted during LLM call");
            return Err(anyhow::anyhow!(
                "Validation aborted by cancellation signal."
            ));
        }

        let reply = out.reply;
        let budget_exceeded = out.budget_exceeded;
        let thinking_budget_exceeded = out.thinking_budget_exceeded;
        if budget_exceeded {
            tracing::warn!(
                "validator-{agent}: maximum single-turn output budget of {max_tokens} tokens exceeded"
            );
        }
        if thinking_budget_exceeded {
            tracing::warn!(
                "validator-{agent}: maximum single-turn reasoning budget of {max_thinking_tokens} tokens exceeded"
            );
        }

        let mut tool_calls = reply.tool_calls.clone();
        if tool_calls.is_empty() && cfg.enable_xml_rescue {
            let rescued = monitor.rescue_xml(&reply.content);
            if !rescued.is_empty() {
                tool_calls = rescued;
            }
        }

        let assistant_msg = crate::types::Message::Assistant {
            content: Some(reply.content.clone()),
            reasoning_content: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning.clone())
            },
            tool_calls: tool_calls.clone(),
        };
        engine.append(assistant_msg);

        for tc in &tool_calls {
            if is_leave_verdict_tool(&tc.function.name) {
                let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));

                let (approved, critique) = parse_verdict_args(&args_val)
                    .unwrap_or((true, "Deliverable verified and approved.".to_string()));

                crate::debug_log::log_validation_verdict(agent.as_str(), approved, &critique);

                tracing::info!(
                    "Validator recorded verdict for {agent} via leave_verdict: approved={}, critique:\n{}",
                    approved,
                    critique
                );
                return Ok((approved, critique));
            }
        }

        if tool_calls.is_empty() {
            if verdict_nudge_count < 3 {
                verdict_nudge_count += 1;
                engine.append(crate::types::Message::User {
                    content: format!(
                        "System: You have not submitted a verdict using the 'leave_verdict' tool (reminder {}/3). Do not output text. If your analysis and verification are complete, you MUST call the 'leave_verdict' tool with verdict ('APPROVED' or 'REJECTED') and comments. If you need to perform further verification, invoke the appropriate tools.",
                        verdict_nudge_count
                    ),
                });
                continue;
            } else {
                tracing::info!(
                    "Validator for {agent} did not invoke leave_verdict after 3 reminders; assuming approved."
                );
                return Ok((
                    true,
                    "Validator completed verification without calling leave_verdict after 3 reminders; assumed approved.".to_string(),
                ));
            }
        }

        for tc in tool_calls {
            if token.is_cancelled() || crate::orchestrator::is_current_or_global_cancelled() {
                tracing::warn!(
                    "validator-{agent}: aborted before tool {}",
                    tc.function.name
                );
                return Err(anyhow::anyhow!(
                    "Validation aborted by cancellation signal."
                ));
            }
            let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
            let desc = format_tool_args_preview(&tc.function.name, &args_val);
            crate::orchestrator::emit_status(format!(
                "validator-{agent}: running {}({desc})",
                tc.function.name
            ));
            let full_args = format_tool_args_full(&tc.function.name, &args_val);
            tracing::info!(
                "validator-{agent} invoking tool: {}({})",
                tc.function.name,
                full_args
            );

            let intervention = monitor.observe_tool(&tc.function.name, &args_val);
            let content = match intervention {
                crate::harness::monitor::Intervention::Block
                | crate::harness::monitor::Intervention::Cut => {
                    let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                        format!(
                            "ERROR: Tool repetition detected for '{}'. Conclude your review by calling leave_verdict.",
                            tc.function.name
                        )
                    });
                    tracing::warn!(
                        "validator-{agent} tool {} blocked by repetition detector",
                        tc.function.name
                    );
                    err_msg
                }
                crate::harness::monitor::Intervention::None => {
                    if token.is_cancelled() || crate::orchestrator::is_current_or_global_cancelled()
                    {
                        tracing::warn!(
                            "validator-{agent}: aborted before dispatching tool {}",
                            tc.function.name
                        );
                        return Err(anyhow::anyhow!(
                            "Validation aborted by cancellation signal."
                        ));
                    }
                    let invocation = crate::harness::ToolInvocation {
                        name: tc.function.name.clone(),
                        arguments: args_val,
                    };
                    let caller = crate::harness::ToolCaller::Specialist(Agent::Validator);
                    let tool_res = crate::harness::dispatch_for_async_with_engine(
                        &invocation,
                        caller,
                        Some(&mut engine),
                    )
                    .await;
                    match tool_res {
                        Ok(r) => {
                            tracing::info!(
                                "validator-{agent} tool {} completed with {} chars",
                                tc.function.name,
                                r.content.len()
                            );
                            r.content
                        }
                        Err(e) => {
                            tracing::warn!(
                                "validator-{agent} tool {} error: {e}",
                                tc.function.name
                            );
                            format!("ERROR: {e}")
                        }
                    }
                }
            };
            let is_rebirth = tc.function.name == crate::tool_names::TOOL_REBIRTH;
            let execution_succeeded = !content.starts_with("ERROR:");
            if !is_rebirth || !execution_succeeded {
                engine.append(crate::types::Message::Tool {
                    tool_call_id: tc.id,
                    content,
                });
            } else {
                engine.append(crate::types::Message::User {
                    content: "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. Do NOT call rebirth again. Continue your validation inspection and submit your verdict via leave_verdict.)".to_string(),
                });
            }
            crate::orchestrator::update_active_worker_context(
                &_active_guard.0,
                engine.token_count(),
            );
        }
        if engine.should_compact() {
            engine.compact();
        } else if engine.should_advise_rebirth() {
            engine.inject_rebirth_advisory();
        }
    }

    // If the loop finished all turns without an explicit leave_verdict tool call, assume approved:
    tracing::info!(
        "Validator for {agent} completed turns without calling leave_verdict; assuming approved."
    );
    Ok((
        true,
        "Validator completed turns without calling leave_verdict; assumed approved.".to_string(),
    ))
}

pub(crate) async fn run_plan_validation(
    plan_markdown: &str,
    cfg: &crate::config::Config,
    token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<(bool, String)> {
    crate::orchestrator::CURRENT_WORKER_TOKEN
        .scope(
            token.clone(),
            run_plan_validation_inner(plan_markdown, cfg, token),
        )
        .await
}

async fn run_plan_validation_inner(
    plan_markdown: &str,
    cfg: &crate::config::Config,
    token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<(bool, String)> {
    let planner_cfg = cfg.orchestration.specialists.get("planner");
    let validator_cfg = cfg.orchestration.specialists.get(Agent::Validator.as_str());

    let auto_validate_enabled = planner_cfg
        .and_then(|sc| sc.enable_validator)
        .or_else(|| validator_cfg.and_then(|vc| vc.enable_validator))
        .unwrap_or(true);

    if !auto_validate_enabled {
        return Ok((true, "Plan validation skipped (disabled).".to_string()));
    }

    let validator_prompt = crate::agents::validator::VALIDATOR_PLANNER_ROLE_PROMPT;

    let validator_backend = planner_cfg
        .and_then(|sc| sc.validator_backend_url.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.backend_url.as_ref()))
        .unwrap_or(&cfg.backend_url);
    let validator_token = planner_cfg
        .and_then(|sc| sc.validator_auth_token.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.auth_token.as_ref()))
        .unwrap_or(&cfg.auth_token);
    let validator_model = planner_cfg
        .and_then(|sc| sc.validator_model.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.model.as_ref()))
        .cloned()
        .unwrap_or_else(|| cfg.model.clone());

    let val_client = crate::llm::ChatClient::new_with_token(
        validator_backend,
        &validator_model,
        validator_token,
    );

    let brief = format!(
        "Proposed Execution Plan:\n```markdown\n{}\n```\n\n\
         Instructions:\n\
         1. Inspect the workspace and examine files using available inspection tools (`read_file`, `grep_search`, `glob`) if needed to evaluate feasibility and existing structure.\n\
         2. You are an auditor: you cannot execute shell commands or modify files. Solely analyze, inspect, and evaluate the proposed plan.\n\
         3. Audit the plan against the criteria: format (# Execution Plan, `- [ ] [t-xxx]`), phase headers, no monolithic tasks, decomposed research tasks (no single catch-all research tasks), unit & integration tests, and feasibility.\n\
         4. When your verification is complete, you MUST call the `leave_verdict` tool with `verdict` ('APPROVED' or 'REJECTED') and detailed `comments`.\n\
         5. If rejected, provide clear, actionable critique explaining what must be decomposed or fixed so the Manager can revise the plan.",
        plan_markdown
    );

    let mut engine = crate::manager::ContextEngineFactory::new(cfg.max_context_tokens)
        .specialist_context(validator_prompt.to_string(), brief);

    let registry = crate::orchestrator::SpecialistRegistry::canonical();
    let val_entry = registry
        .resolve(Agent::Validator)
        .expect("validator is registered");
    let mut tools = Vec::new();
    for tool in crate::types::ToolDef::default_tools() {
        if val_entry.allows(&tool.function.name) {
            tools.push(tool);
        }
    }
    if let Some(mcp) = crate::harness::get_mcp_manager() {
        let servers = validator_cfg
            .map(|vc| vc.mcp_servers.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| planner_cfg.map(|sc| sc.mcp_servers.clone()))
            .unwrap_or_default();
        for tool in mcp.tools_for_servers(&servers) {
            tools.push(crate::types::ToolDef::from_mcp(&tool));
        }
    }

    let val_tag = "validator-planner".to_string();
    let _active_guard = crate::orchestrator::register_active_worker_with_token(
        None,
        val_tag.clone(),
        "Auditing execution plan".to_string(),
        Some(token.clone()),
    );

    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let monitor = crate::harness::monitor::HarnessMonitor::new_with_config(
        std::sync::Arc::new(crate::harness::HarnessStats::new()),
        mon_cfg,
    );
    let mut rep_detector = crate::harness::monitor::RepetitionDetector::new(
        mon_cfg.repetition_threshold,
        mon_cfg.min_pattern_len,
    );
    let mut verdict_nudge_count = 0usize;
    let mut _turn = 0usize;

    loop {
        _turn += 1;
        if token.is_cancelled() {
            tracing::warn!("{val_tag}: aborted by cancellation token");
            return Ok((
                false,
                "Plan validation aborted by cancellation signal.".to_string(),
            ));
        }
        crate::orchestrator::update_active_worker_context(&_active_guard.0, engine.token_count());
        crate::orchestrator::emit_status(format!(
            "{val_tag}: auditing execution plan structure & granularity (turn {_turn})...",
        ));

        let req = crate::types::ChatRequest {
            model: validator_model.clone(),
            messages: engine.messages().to_vec(),
            tools: Some(tools.clone()),
            stream: Some(true),
            enable_thinking: None,
            temperature: Some(0.0),
            top_p: Some(cfg.top_p),
            presence_penalty: Some(cfg.presence_penalty),
            frequency_penalty: Some(cfg.frequency_penalty),
        };

        let max_tokens = mon_cfg.max_stream_tokens.max(256);
        let max_thinking_tokens = mon_cfg.max_thinking_tokens.max(256);
        let mut sink = crate::orchestrator::PreemptibleStreamSink::register_full(
            &val_tag,
            Some("validator-planner".to_string()),
            None,
            Some(token.clone()),
            &validator_model,
        );
        let stream_out = crate::llm::chat_stream_resumable(
            &val_client,
            &req,
            &mut sink,
            max_tokens,
            max_thinking_tokens,
            &mut rep_detector,
            false,
            Some(token),
        )
        .await;

        let out = match stream_out {
            Ok(o) => o,
            Err(e) => {
                if token.is_cancelled() {
                    tracing::warn!("{val_tag}: aborted during LLM call");
                    return Ok((
                        false,
                        "Plan validation aborted by cancellation signal.".to_string(),
                    ));
                }
                tracing::error!("{val_tag} LLM chat call error on turn {_turn}: {e:?}");
                return Err(e);
            }
        };

        if out.was_aborted_by_steer || token.is_cancelled() {
            tracing::warn!("{val_tag}: aborted during LLM call");
            return Ok((
                false,
                "Plan validation aborted by cancellation signal.".to_string(),
            ));
        }

        let reply = out.reply;
        let mut tool_calls = reply.tool_calls.clone();
        if tool_calls.is_empty() && cfg.enable_xml_rescue {
            let rescued = monitor.rescue_xml(&reply.content);
            if !rescued.is_empty() {
                tool_calls = rescued;
            }
        }

        let assistant_msg = crate::types::Message::Assistant {
            content: Some(reply.content.clone()),
            reasoning_content: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning.clone())
            },
            tool_calls: tool_calls.clone(),
        };
        engine.append(assistant_msg);

        for tc in &tool_calls {
            if is_leave_verdict_tool(&tc.function.name) {
                let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));

                let (approved, critique) = parse_verdict_args(&args_val).unwrap_or((
                    true,
                    "Execution plan structure and task decomposition verified.".to_string(),
                ));

                crate::debug_log::log_validation_verdict("planner", approved, &critique);
                tracing::info!(
                    "Validator recorded verdict for plan via leave_verdict: approved={}, critique:\n{}",
                    approved,
                    critique
                );
                return Ok((approved, critique));
            }
        }

        if tool_calls.is_empty() {
            if verdict_nudge_count < 3 {
                verdict_nudge_count += 1;
                engine.append(crate::types::Message::User {
                    content: format!(
                        "System: You have not submitted a verdict using the 'leave_verdict' tool (reminder {}/3). Do not output text. If your analysis and verification are complete, you MUST call the 'leave_verdict' tool with verdict ('APPROVED' or 'REJECTED') and comments. If you need to perform further verification, invoke the appropriate tools.",
                        verdict_nudge_count
                    ),
                });
                continue;
            } else {
                tracing::info!(
                    "{val_tag} did not invoke leave_verdict after 3 reminders; assuming approved."
                );
                return Ok((
                    true,
                    "Auditor did not leave explicit verdict after 3 reminders; assumed approved."
                        .to_string(),
                ));
            }
        }

        for tc in &tool_calls {
            if tc.function.name == TOOL_LEAVE_VERDICT {
                continue;
            }
            if token.is_cancelled() || crate::orchestrator::is_current_or_global_cancelled() {
                tracing::warn!(
                    "{val_tag}: aborted before executing tool {}",
                    tc.function.name
                );
                return Err(anyhow::anyhow!(
                    "Plan validation aborted by cancellation signal."
                ));
            }
            let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
            let invocation = crate::harness::ToolInvocation {
                name: tc.function.name.clone(),
                arguments: args,
            };
            let tool_res = crate::harness::dispatch_for_async_with_engine(
                &invocation,
                crate::harness::ToolCaller::Specialist(Agent::Validator),
                Some(&mut engine),
            )
            .await;
            let content = match tool_res {
                Ok(r) => r.content,
                Err(e) => format!("Tool error: {e}"),
            };
            engine.append(crate::types::Message::Tool {
                tool_call_id: tc.id.clone(),
                content,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_leave_verdict_tool_matching() {
        assert!(is_leave_verdict_tool("leave_verdict"));
        assert!(is_leave_verdict_tool("LEAVE_VERDICT"));
        assert!(is_leave_verdict_tool("leaveVerdict"));
        assert!(is_leave_verdict_tool("terminal__leave_verdict"));
        assert!(is_leave_verdict_tool("validator__leave_verdict"));
        assert!(is_leave_verdict_tool("leave_verdict_tool"));
        assert!(!is_leave_verdict_tool("read_file"));
        assert!(!is_leave_verdict_tool("delegate_task"));
    }

    #[test]
    fn test_parse_verdict_args_approved_variations() {
        let cases = vec![
            serde_json::json!({"verdict": "APPROVED", "comments": "Good job"}),
            serde_json::json!({"verdict": "approved", "comments": "Good job"}),
            serde_json::json!({"verdict": "APPROVE", "feedback": "Good job"}),
            serde_json::json!({"verdict": " PASS ", "critique": "Good job"}),
            serde_json::json!({"status": "PASSED", "details": "Good job"}),
            serde_json::json!({"decision": "ACCEPTED", "reason": "Good job"}),
            serde_json::json!({"verdict": true, "comments": "Good job"}),
            serde_json::json!({"approved": true, "comments": "Good job"}),
        ];

        for case in cases {
            let res = parse_verdict_args(&case);
            assert!(res.is_some(), "Failed for case: {case:?}");
            let (approved, comments) = res.unwrap();
            assert!(approved, "Expected approved for case: {case:?}");
            assert_eq!(comments, "Good job");
        }
    }

    #[test]
    fn test_parse_verdict_args_rejected_variations() {
        let cases = vec![
            serde_json::json!({"verdict": "REJECTED", "comments": "Fix errors"}),
            serde_json::json!({"verdict": "rejected", "comments": "Fix errors"}),
            serde_json::json!({"verdict": "REJECT", "critique": "Fix errors"}),
            serde_json::json!({"verdict": " FAIL ", "feedback": "Fix errors"}),
            serde_json::json!({"status": "FAILED", "reason": "Fix errors"}),
            serde_json::json!({"decision": "DECLINED", "explanation": "Fix errors"}),
            serde_json::json!({"verdict": false, "comments": "Fix errors"}),
            serde_json::json!({"approved": false, "comments": "Fix errors"}),
        ];

        for case in cases {
            let res = parse_verdict_args(&case);
            assert!(res.is_some(), "Failed for case: {case:?}");
            let (approved, comments) = res.unwrap();
            assert!(!approved, "Expected rejected for case: {case:?}");
            assert_eq!(comments, "Fix errors");
        }
    }

    #[test]
    fn test_parse_verdict_args_defaults() {
        let empty_approved = serde_json::json!({"verdict": "APPROVED"});
        let (approved, comments) = parse_verdict_args(&empty_approved).unwrap();
        assert!(approved);
        assert_eq!(comments, "Deliverable verified and approved.");

        let empty_rejected = serde_json::json!({"verdict": "REJECTED"});
        let (approved, comments) = parse_verdict_args(&empty_rejected).unwrap();
        assert!(!approved);
        assert_eq!(
            comments,
            "Deliverable rejected by validator without detailed comments."
        );
    }
}
