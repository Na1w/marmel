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

pub(crate) async fn run_automated_validation(
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

    let _active_guard = crate::orchestrator::register_active_worker(
        clean_task_id.clone(),
        format!("validator-{agent}"),
        format!("Auditing {agent} deliverable"),
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
            return Ok((
                false,
                "Validation aborted by cancellation signal.".to_string(),
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
                    return Ok((
                        false,
                        "Validation aborted by cancellation signal.".to_string(),
                    ));
                }
                tracing::error!("validator-{agent} LLM chat call error on turn {_turn}: {e:?}");
                break;
            }
        };

        if out.was_aborted_by_steer || token.is_cancelled() {
            tracing::warn!("validator-{agent}: aborted during LLM call");
            return Ok((
                false,
                "Validation aborted by cancellation signal.".to_string(),
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

        let mut had_invalid_verdict = false;
        for tc in &tool_calls {
            if tc.function.name == TOOL_LEAVE_VERDICT {
                let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
                let verdict_opt = args_val.get("verdict").and_then(serde_json::Value::as_str);

                let verdict = match verdict_opt {
                    Some(v)
                        if v.eq_ignore_ascii_case("APPROVED")
                            || v.eq_ignore_ascii_case("REJECTED") =>
                    {
                        v
                    }
                    _ => {
                        let err_msg = "ERROR: Missing or invalid mandatory argument 'verdict'. You MUST specify verdict as either 'APPROVED' or 'REJECTED'.";
                        engine.append(crate::types::Message::Tool {
                            tool_call_id: tc.id.clone(),
                            content: err_msg.to_string(),
                        });
                        tracing::warn!(
                            "Validator for {agent} omitted or passed invalid verdict: {args_val:?}"
                        );
                        had_invalid_verdict = true;
                        continue;
                    }
                };
                let comments = args_val
                    .get("comments")
                    .or_else(|| args_val.get("comment"))
                    .or_else(|| args_val.get("feedback"))
                    .or_else(|| args_val.get("reason"))
                    .or_else(|| args_val.get("critique"))
                    .or_else(|| args_val.get("details"))
                    .or_else(|| args_val.get("explanation"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();

                let approved = verdict.eq_ignore_ascii_case("APPROVED");
                let critique = if !comments.is_empty() {
                    comments
                } else if approved {
                    "Deliverable verified and approved.".to_string()
                } else {
                    "Deliverable rejected by validator without detailed comments.".to_string()
                };

                crate::debug_log::log_validation_verdict(agent.as_str(), approved, &critique);

                tracing::info!(
                    "Validator recorded verdict for {agent} via leave_verdict: approved={}, critique:\n{}",
                    approved,
                    critique
                );
                return Ok((approved, critique));
            }
        }

        if had_invalid_verdict {
            continue;
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
            }
            if engine.should_compact() {
                engine.compact();
            } else if engine.should_advise_rebirth() {
                engine.inject_rebirth_advisory();
            }
            crate::orchestrator::update_active_worker_context(
                &_active_guard.0,
                engine.token_count(),
            );
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
    let _active_guard = crate::orchestrator::register_active_worker(
        None,
        val_tag.clone(),
        "Auditing execution plan".to_string(),
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

        let mut had_invalid_verdict = false;
        for tc in &tool_calls {
            if tc.function.name == TOOL_LEAVE_VERDICT {
                let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
                let verdict_opt = args_val.get("verdict").and_then(serde_json::Value::as_str);

                let verdict = match verdict_opt {
                    Some(v)
                        if v.eq_ignore_ascii_case("APPROVED")
                            || v.eq_ignore_ascii_case("REJECTED") =>
                    {
                        v
                    }
                    _ => {
                        let err_msg = "ERROR: Missing or invalid mandatory argument 'verdict'. You MUST specify verdict as either 'APPROVED' or 'REJECTED'.";
                        engine.append(crate::types::Message::Tool {
                            tool_call_id: tc.id.clone(),
                            content: err_msg.to_string(),
                        });
                        tracing::warn!("{val_tag} omitted or passed invalid verdict: {args_val:?}");
                        had_invalid_verdict = true;
                        continue;
                    }
                };

                let comments = args_val
                    .get("comments")
                    .or_else(|| args_val.get("comment"))
                    .or_else(|| args_val.get("feedback"))
                    .or_else(|| args_val.get("reason"))
                    .or_else(|| args_val.get("critique"))
                    .or_else(|| args_val.get("details"))
                    .or_else(|| args_val.get("explanation"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();

                let approved = verdict.eq_ignore_ascii_case("APPROVED");
                let critique = if !comments.is_empty() {
                    comments
                } else if approved {
                    "Execution plan structure and task decomposition verified.".to_string()
                } else {
                    "Execution plan rejected by validator without detailed comments.".to_string()
                };

                crate::debug_log::log_validation_verdict("planner", approved, &critique);
                tracing::info!(
                    "Validator recorded verdict for plan via leave_verdict: approved={}, critique:\n{}",
                    approved,
                    critique
                );
                return Ok((approved, critique));
            }
        }

        if had_invalid_verdict {
            continue;
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
