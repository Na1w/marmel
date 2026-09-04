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
         1. Inspect the workspace, verify files, compile, and run tests as needed using available tools.\n\
         2. When your verification is complete, you MUST call the `leave_verdict` tool with `verdict` ('APPROVED' or 'REJECTED') and detailed `comments`.\n\
         3. If advised or when context usage is high (>= 80%), call the `rebirth` tool with your intermediate findings to preserve context.",
        task_brief, deliverable
    );

    let mut engine = crate::agent::ContextEngineFactory::new(cfg.max_context_tokens)
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

    let _active_guard = crate::orchestrator::register_active_worker(
        None,
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
    for _turn in 0..50 {
        if token.is_cancelled() {
            tracing::warn!("validator-{agent}: aborted by cancellation token");
            return Ok((
                false,
                "Validation aborted by cancellation signal.".to_string(),
            ));
        }
        crate::orchestrator::update_active_worker_context(&_active_guard.0, engine.token_count());
        crate::orchestrator::emit_status(format!(
            "validator-{agent}: evaluating test & inspection output (turn {}/50)...",
            _turn + 1
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
        let mut sink = crate::orchestrator::PreemptibleStreamSink::register(
            format!("validator-{agent}"),
            &validator_model,
        );
        let stream_out = crate::llm::chat_stream_resumable(
            &val_client,
            &req,
            &mut sink,
            max_tokens,
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
        if budget_exceeded {
            tracing::warn!(
                "validator-{agent}: maximum single-turn output budget of {max_tokens} tokens exceeded"
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
            if tc.function.name == TOOL_LEAVE_VERDICT {
                let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
                let verdict = args_val
                    .get("verdict")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("APPROVED");
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

        if tool_calls.is_empty() {
            // Validator responded with text without calling leave_verdict -> prompt it (matching Caesar orchestrator.rs:1510).
            engine.append(crate::types::Message::User {
                content: "System: You have not submitted a verdict. If you need to perform further verification, please invoke the appropriate tools (e.g., running commands or reading files). If your analysis is complete, you must call the 'leave_verdict' tool to submit your final verdict (APPROVED or REJECTED).".to_string(),
            });
            continue;
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
                    let tool_res = crate::harness::dispatch_for_with_engine(
                        &invocation,
                        crate::harness::ToolCaller::Specialist(Agent::Validator),
                        Some(&mut engine),
                    );
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

    // If the loop finished all turns without an explicit leave_verdict tool call (matching Caesar executor.rs:741):
    tracing::warn!("Validator for {agent} completed turns without calling leave_verdict.");
    Ok((
        false,
        "The validator failed to submit a verdict using the 'leave_verdict' tool.".to_string(),
    ))
}
