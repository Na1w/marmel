//! Specialist execution loop and tool turn state machine.

use super::assembly::{assemble_final_deliverable, update_revision};
use super::formatting::{format_tool_args_full, format_tool_args_preview};
use crate::agents::validation::{
    is_leave_verdict_tool, parse_verdict_args, run_automated_validation,
};
use crate::agents::{Agent, IsolatedContext};

pub async fn run_specialist_live(
    client: &crate::llm::ChatClient,
    agent: Agent,
    ctx: &IsolatedContext,
    cfg: &crate::config::Config,
    token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<String> {
    let env_block = crate::prompts::format_environment_block();

    let enhanced_system_prompt = format!(
        "{}\n\n{}\n- Tools available: `write_file`, `replace`, `read_file`, `run_command`, `grep_search`, `glob`, `rebirth`.\n- You MUST save files and execute real work to complete the task.\n- Context Preservation: If context usage is high (>= 80%) or advised, call `rebirth` with a detailed summary capturing all pertinent state (including active file paths, current read offsets or line numbers reached in `read_file`, intermediate findings, and next actions) so work resumes seamlessly without starting over.",
        ctx.role_system_prompt, env_block
    );

    let mut engine = crate::manager::ContextEngineFactory::new(cfg.max_context_tokens)
        .specialist_context(enhanced_system_prompt, ctx.brief.clone());

    if !ctx.snippets.is_empty() {
        let snippet_text = format!("Snippets:\n{}", ctx.snippets.join("\n---\n"));
        engine.append(crate::types::Message::User {
            content: snippet_text,
        });
    }

    let specialist_cfg = cfg.orchestration.specialists.get(agent.as_str());
    let specialist_model = specialist_cfg
        .and_then(|sc| sc.model.as_ref())
        .cloned()
        .unwrap_or_else(|| cfg.model.clone());

    let clean_task_id = ctx
        .task_id
        .as_deref()
        .map(|t| {
            t.trim_matches(|c| {
                c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
            })
            .trim()
            .to_string()
        })
        .filter(|t| !t.is_empty());

    let agent_tag = match &clean_task_id {
        Some(t) => format!("{agent}-{t}"),
        None => format!("{agent}"),
    };

    let registry = crate::orchestrator::SpecialistRegistry::canonical();
    let reg_entry = registry.resolve(agent).expect("agent is registered");
    let mut tools = Vec::new();
    for tool in crate::types::ToolDef::default_tools() {
        if reg_entry.allows(&tool.function.name) {
            tools.push(tool);
        }
    }
    if let Some(mcp) = crate::harness::get_mcp_manager()
        && let Some(sc) = specialist_cfg
    {
        for tool in mcp.tools_for_servers(&sc.mcp_servers) {
            tools.push(crate::types::ToolDef::from_mcp(&tool));
        }
    }

    let mut final_content = String::new();
    let mut nudge_count = 0u32;
    let mut consecutive_thinking_nudges = 0u32;

    let _active_guard = crate::orchestrator::register_active_worker(
        clean_task_id.clone(),
        agent.as_str().to_string(),
        ctx.brief.clone(),
    );

    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let mut monitor = crate::harness::monitor::HarnessMonitor::new_with_config(
        std::sync::Arc::new(crate::harness::HarnessStats::new()),
        mon_cfg,
    );
    let mut tools_executed_count = 0usize;
    let mut _turn = 0usize;

    let auto_validate_enabled = specialist_cfg
        .and_then(|sc| sc.enable_validator)
        .unwrap_or(true);
    let max_val_iterations = specialist_cfg
        .and_then(|sc| sc.max_validator_iterations)
        .unwrap_or(5);

    let mut validation_passed =
        !auto_validate_enabled || max_val_iterations == 0 || agent == Agent::Validator;
    let mut validator_critique: Option<String> = None;
    let mut val_iter = 0usize;

    loop {
        _turn += 1;
        if token.is_cancelled() {
            tracing::warn!("{agent_tag}: aborted by cancellation signal");
            crate::orchestrator::set_active_worker_status(&_active_guard.0, "Aborted");
            return Ok("Task aborted by user instruction.\n\nFAILED (aborted)".to_string());
        }
        let mut rep_detector = crate::harness::monitor::RepetitionDetector::new(
            mon_cfg.repetition_threshold,
            mon_cfg.min_pattern_len,
        );
        crate::orchestrator::update_active_worker_progress(
            &_active_guard.0,
            _turn,
            val_iter,
            validator_critique.clone(),
        );
        crate::orchestrator::update_active_worker_context(&_active_guard.0, engine.token_count());
        crate::orchestrator::emit_status(format!(
            "{agent_tag}: thinking / calling model ({specialist_model})..."
        ));
        let req = crate::types::ChatRequest {
            model: specialist_model.clone(),
            messages: engine.messages().to_vec(),
            tools: Some(tools.clone()),
            stream: Some(true),
            enable_thinking: None,
            temperature: Some(cfg.temperature),
            top_p: Some(cfg.top_p),
            presence_penalty: Some(cfg.presence_penalty),
            frequency_penalty: Some(cfg.frequency_penalty),
        };

        let max_tokens = mon_cfg.max_stream_tokens.max(256);
        let max_thinking_tokens = specialist_cfg
            .and_then(|s| s.max_thinking_tokens)
            .unwrap_or(cfg.max_thinking_tokens)
            .max(256);
        let mut sink = crate::orchestrator::PreemptibleStreamSink::register_full(
            &agent_tag,
            Some(agent.as_str().to_string()),
            clean_task_id.clone(),
            Some(token.clone()),
            &specialist_model,
        );
        let stream_out = crate::llm::chat_stream_resumable(
            client,
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
                    tracing::warn!("{agent_tag}: aborted during LLM call");
                    return Ok("Task aborted by user instruction.\n\nFAILED (aborted)".to_string());
                }
                return Err(e);
            }
        };

        if out.was_aborted_by_steer || token.is_cancelled() {
            tracing::warn!("{agent_tag}: aborted during LLM call");
            return Ok("Task aborted by user instruction.\n\nFAILED (aborted)".to_string());
        }

        let reply = out.reply;
        let budget_exceeded = out.budget_exceeded;
        let thinking_budget_exceeded = out.thinking_budget_exceeded;
        let rep_triggered = out.rep_triggered;
        if budget_exceeded {
            tracing::warn!(
                "{agent_tag}: maximum single-turn output budget of {max_tokens} tokens exceeded — cutting stream"
            );
            crate::orchestrator::emit_status(format!(
                "{agent_tag}: single-turn output budget ({max_tokens} tokens) reached"
            ));
        }
        if thinking_budget_exceeded {
            tracing::warn!(
                "{agent_tag}: maximum single-turn reasoning budget of {max_thinking_tokens} tokens exceeded — cutting stream"
            );
            crate::orchestrator::emit_status(format!(
                "{agent_tag}: single-turn reasoning budget ({max_thinking_tokens} tokens) reached"
            ));
        }
        update_revision(&mut final_content, &reply.content);

        let mut tool_calls = reply.tool_calls.clone();
        if tool_calls.is_empty() && cfg.enable_xml_rescue {
            let rescued = monitor.rescue_xml(&reply.content);
            if !rescued.is_empty() {
                tool_calls = rescued;
            }
        }

        let assistant_content = if reply.content.is_empty() {
            if tool_calls.is_empty() && !reply.reasoning.is_empty() {
                Some("[Thinking completed without content or tool calls]".to_string())
            } else {
                Some(String::new())
            }
        } else {
            Some(reply.content.clone())
        };
        let assistant_msg = crate::types::Message::Assistant {
            content: assistant_content,
            reasoning_content: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning.clone())
            },
            tool_calls: tool_calls.clone(),
        };
        engine.append(assistant_msg);

        let is_repeating = rep_triggered || monitor.feed_text(&reply.content);

        if tool_calls.is_empty() {
            if thinking_budget_exceeded {
                consecutive_thinking_nudges += 1;
                if consecutive_thinking_nudges >= 2 {
                    tracing::warn!(
                        "{agent_tag}: thinking budget exceeded twice consecutively — returning REPLAN REQUIRED"
                    );
                    crate::orchestrator::emit_status(format!(
                        "{agent_tag}: reasoning budget exceeded twice consecutively — task too complex, requesting replan"
                    ));
                    crate::orchestrator::set_active_worker_status(
                        &_active_guard.0,
                        "Replan Required (task too complex)",
                    );
                    let task_ref = ctx.task_id.as_deref().unwrap_or("task");
                    let replan_msg = format!(
                        "REPLAN REQUIRED ({task_ref}): task too complex — exceeded single-turn reasoning budget of {max_thinking_tokens} tokens twice consecutively without completing work."
                    );
                    return Ok(replan_msg);
                }
                nudge_count += 1;
                tracing::warn!(
                    "{agent_tag}: thinking budget exceeded — injecting reasoning cutoff nudge ({consecutive_thinking_nudges}/2)"
                );
                crate::orchestrator::emit_status(format!(
                    "{agent_tag}: reasoning budget ({max_thinking_tokens} tokens) reached — nudging out of thinking"
                ));
                engine.replace_last(crate::types::Message::Assistant {
                    content: if reply.content.trim().is_empty() {
                        Some(format!(
                            "[Reasoning budget reached: exceeded {max_thinking_tokens} token limit]"
                        ))
                    } else {
                        Some(reply.content.clone())
                    },
                    reasoning_content: if reply.reasoning.is_empty() {
                        None
                    } else {
                        Some(reply.reasoning.clone())
                    },
                    tool_calls: Vec::new(),
                });
                engine.append(crate::types::Message::User {
                    content: format!(
                        "SYSTEM NOTICE: Maximum reasoning budget of {max_thinking_tokens} tokens reached for this turn. Stop internal thinking immediately. Proceed directly to output your deliverables or execute required tools (such as `read_file`, `write_file`, `replace`, `run_command`, etc.)."
                    ),
                });
                continue;
            } else {
                consecutive_thinking_nudges = 0;
            }

            if budget_exceeded && nudge_count < 2 {
                nudge_count += 1;
                tracing::warn!(
                    "{agent_tag}: output budget exceeded — injecting corrective nudge ({nudge_count}/2)"
                );
                engine.replace_last(crate::types::Message::Assistant {
                    content: Some(format!(
                        "[Generation truncated: exceeded {max_tokens} token single-turn limit]"
                    )),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                });
                engine.append(crate::types::Message::User {
                    content: format!(
                        "SYSTEM NOTICE: Your response exceeded the single-turn output budget limit ({max_tokens} tokens) and was truncated. Please be concise, call your required tools (such as `read_file`, `write_file`, `replace`, `run_command`, etc.) to perform the work, or conclude with 'MISSION COMPLETE'."
                    ),
                });
                continue;
            }

            if is_repeating {
                if nudge_count < 2 {
                    nudge_count += 1;
                    tracing::warn!(
                        "{agent_tag}: repetitive generation loop detected in specialist output — injecting corrective nudge ({nudge_count}/2)"
                    );
                    engine.replace_last(crate::types::Message::Assistant {
                        content: Some(
                            "[Generation interrupted due to repetitive loop]".to_string(),
                        ),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                    });
                    rep_detector = crate::harness::monitor::RepetitionDetector::new(
                        mon_cfg.repetition_threshold,
                        mon_cfg.min_pattern_len,
                    );
                    engine.append(crate::types::Message::User {
                        content: "SYSTEM NOTICE: Repetitive generation loop detected in your responses. Terminate conversational debate immediately and invoke your required tools (such as `read_file`, `write_file`, `run_command`, etc.) to perform the required work, or conclude with 'MISSION COMPLETE'.".to_string(),
                    });
                    continue;
                } else {
                    tracing::warn!(
                        "{agent_tag}: repetitive generation loop persisted across turns — terminating specialist loop"
                    );
                    break;
                }
            }

            let upper = reply.content.to_ascii_uppercase();
            let is_terminal = upper.contains("MISSION COMPLETE")
                || upper.contains("FAILED")
                || upper.contains("REPLAN REQUIRED");
            if !is_terminal {
                if nudge_count < 2 {
                    nudge_count += 1;
                    let nudge_msg = if reply.content.trim().is_empty()
                        && !reply.reasoning.is_empty()
                    {
                        "SYSTEM NOTICE: Your thoughts completed but you produced 0 output text and 0 tool calls. Do not remain silent in thoughts. You MUST execute your required tools (such as `read_file`, `write_file`, `replace`, `run_command`, etc.) to write files to disk and perform the task, or conclude with 'MISSION COMPLETE'.".to_string()
                    } else {
                        "SYSTEM NOTICE: You did not call any tools or output MISSION COMPLETE. Do not output conversational prose. Immediately use your tools (such as `read_file`, `write_file`, `replace`, `run_command`, etc.) to perform the required work, create/update any requested files in the workspace, and conclude with 'MISSION COMPLETE'.".to_string()
                    };
                    engine.append(crate::types::Message::User { content: nudge_msg });
                    continue;
                } else {
                    tracing::warn!(
                        "{agent_tag}: specialist produced no tool calls after {nudge_count} nudges — terminating"
                    );
                    break;
                }
            }

            let has_terminal_marker = upper.contains("MISSION COMPLETE")
                || upper.contains("FAILED")
                || upper.contains("REPLAN REQUIRED");

            if tools_executed_count == 0 && !has_terminal_marker {
                tracing::warn!(
                    "{agent_tag}: specialist produced no tool executions or terminal marker — failing deliverable without validation"
                );
                crate::orchestrator::set_active_worker_status(
                    &_active_guard.0,
                    "Failed (no tools executed)",
                );
                return Ok(assemble_final_deliverable(
                    false,
                    Some("Specialist generated conversational text without executing any tools."),
                    &final_content,
                    ctx.task_id.as_deref(),
                ));
            }

            if final_content.trim().is_empty() {
                if !reply.reasoning.trim().is_empty() {
                    final_content = reply.reasoning.clone();
                } else if tools_executed_count > 0 {
                    final_content = format!(
                        "Specialist executed {tools_executed_count} tool operations to complete the task."
                    );
                }
            }

            if auto_validate_enabled
                && agent != Agent::Validator
                && !final_content.is_empty()
                && (tools_executed_count > 0 || has_terminal_marker)
                && !upper.contains("REPLAN REQUIRED")
            {
                if val_iter < max_val_iterations {
                    val_iter += 1;
                    if token.is_cancelled() {
                        tracing::warn!("{agent_tag}: aborted before validation pass");
                        return Ok(
                            "Task aborted by user instruction.\n\nFAILED (aborted)".to_string()
                        );
                    }
                    crate::orchestrator::emit_status(format!(
                        "validator-{agent_tag}: testing deliverable (pass {val_iter}/{max_val_iterations})..."
                    ));
                    match run_automated_validation(
                        client,
                        agent,
                        clean_task_id.as_deref(),
                        &ctx.brief,
                        &final_content,
                        cfg,
                        token,
                    )
                    .await
                    {
                        Ok((approved, critique)) => {
                            if approved {
                                let feedback = if critique.trim().is_empty() {
                                    "All verification checks passed.".to_string()
                                } else {
                                    critique.clone()
                                };
                                crate::orchestrator::emit_status(format!(
                                    "[Validator] APPROVED deliverable for {agent_tag}:\n{feedback}"
                                ));
                                tracing::info!(
                                    "Automated validator APPROVED specialist deliverable for {}: {}",
                                    agent_tag,
                                    feedback
                                );
                                validation_passed = true;
                                validator_critique = Some(feedback.clone());
                                crate::orchestrator::update_active_worker_progress(
                                    &_active_guard.0,
                                    _turn,
                                    val_iter,
                                    Some(feedback),
                                );
                                crate::orchestrator::set_active_worker_status(
                                    &_active_guard.0,
                                    "Approved",
                                );
                                break;
                            } else {
                                let feedback = if critique.trim().is_empty() {
                                    "Deliverable failed verification checks.".to_string()
                                } else {
                                    critique.clone()
                                };
                                validator_critique = Some(feedback.clone());
                                crate::orchestrator::emit_status(format!(
                                    "[Validator] REJECTED deliverable for {agent_tag} (pass {val_iter}/{max_val_iterations}):\n{feedback}"
                                ));
                                tracing::warn!(
                                    "Automated validator REJECTED specialist deliverable for {}: {}",
                                    agent_tag,
                                    feedback
                                );
                                crate::orchestrator::update_active_worker_progress(
                                    &_active_guard.0,
                                    _turn,
                                    val_iter,
                                    Some(feedback.clone()),
                                );
                                crate::orchestrator::set_active_worker_status(
                                    &_active_guard.0,
                                    &format!(
                                        "Revising (rejected pass {val_iter}/{max_val_iterations})"
                                    ),
                                );
                                let feedback_msg = format!(
                                    "Validation feedback: The validator tested your changes and found issues:\n{}\n\n\
                                     Please address all validator critique points, verify your work with available tools, and conclude with 'MISSION COMPLETE'.",
                                    feedback
                                );
                                engine.append(crate::types::Message::User {
                                    content: feedback_msg,
                                });
                                nudge_count = 0;
                                continue;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Automated validator encountered error: {e}");
                            break;
                        }
                    }
                } else {
                    tracing::warn!(
                        "{agent_tag}: maximum validator iterations ({max_val_iterations}) reached without approval"
                    );
                    validation_passed = false;
                    crate::orchestrator::set_active_worker_status(
                        &_active_guard.0,
                        "Failed (max validator iterations exceeded)",
                    );
                    break;
                }
            } else {
                break;
            }
        }

        let mut leave_verdict_called = false;
        for tc in tool_calls {
            if token.is_cancelled() {
                tracing::warn!(
                    "{agent_tag}: aborted before executing tool {}",
                    tc.function.name
                );
                return Ok("Task aborted by user instruction.\n\nFAILED (aborted)".to_string());
            }
            let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));

            if is_leave_verdict_tool(&tc.function.name) {
                let (approved, critique) = parse_verdict_args(&args_val)
                    .unwrap_or((true, "Deliverable verified.".to_string()));

                validation_passed = approved;
                validator_critique = Some(critique.clone());

                let verdict_str = if approved { "APPROVED" } else { "REJECTED" };
                crate::orchestrator::emit_status(format!(
                    "[{agent_tag}] {verdict_str} via leave_verdict:\n{critique}"
                ));
                if approved {
                    crate::orchestrator::set_active_worker_status(&_active_guard.0, "Approved");
                } else {
                    crate::orchestrator::set_active_worker_status(&_active_guard.0, "Rejected");
                }
                crate::debug_log::log_validation_verdict(agent.as_str(), approved, &critique);

                let verdict_summary = if approved {
                    format!("Verdict: APPROVED\n\nComments:\n{critique}")
                } else {
                    format!("Verdict: REJECTED\n\nCritique:\n{critique}")
                };
                if final_content.trim().is_empty() {
                    final_content = verdict_summary;
                } else if !final_content.contains(&critique) {
                    final_content.push_str("\n\n");
                    final_content.push_str(&verdict_summary);
                }

                let invocation = crate::harness::ToolInvocation {
                    name: tc.function.name.clone(),
                    arguments: args_val,
                };
                let caller = crate::harness::ToolCaller::Specialist(agent);
                let _ = crate::harness::dispatch_for_async_with_engine(
                    &invocation,
                    caller,
                    Some(&mut engine),
                )
                .await;

                tools_executed_count += 1;
                leave_verdict_called = true;
                break;
            }

            let preview = format_tool_args_preview(&tc.function.name, &args_val);
            if preview.is_empty() {
                crate::orchestrator::emit_status(format!("{agent_tag}: {}", tc.function.name));
            } else {
                crate::orchestrator::emit_status(format!(
                    "{agent_tag}: {}({preview})",
                    tc.function.name
                ));
            }
            let full_args = format_tool_args_full(&tc.function.name, &args_val);
            tracing::info!(
                "{agent_tag} invoking tool: {}({})",
                tc.function.name,
                full_args
            );

            let intervention = monitor.observe_tool(&tc.function.name, &args_val);
            let content = match intervention {
                crate::harness::monitor::Intervention::Block
                | crate::harness::monitor::Intervention::Cut => {
                    let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                        format!(
                            "ERROR: Tool repetition detected for '{}'. Do not repeat identical calls — proceed with your task or save deliverables with write_file.",
                            tc.function.name
                        )
                    });
                    tracing::warn!(
                        "{agent_tag} tool {} blocked by repetition detector",
                        tc.function.name
                    );
                    err_msg
                }
                crate::harness::monitor::Intervention::None => {
                    let invocation = crate::harness::ToolInvocation {
                        name: tc.function.name.clone(),
                        arguments: args_val,
                    };
                    let caller = crate::harness::ToolCaller::Specialist(agent);
                    let tool_res = crate::harness::dispatch_for_async_with_engine(
                        &invocation,
                        caller,
                        Some(&mut engine),
                    )
                    .await;
                    match tool_res {
                        Ok(r) => {
                            tools_executed_count += 1;
                            nudge_count = 0;
                            tracing::info!(
                                "{agent_tag} tool {} completed with {} chars output",
                                tc.function.name,
                                r.content.len()
                            );
                            r.content
                        }
                        Err(e) => {
                            tracing::warn!("{agent_tag} tool {} error: {e}", tc.function.name);
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
        if leave_verdict_called {
            tracing::info!(
                "{agent_tag}: leave_verdict concluded specialist inspection loop (approved={validation_passed})"
            );
            break;
        }
    }

    let has_terminal_marker = {
        let upper = final_content.to_ascii_uppercase();
        upper.contains("MISSION COMPLETE")
            || upper.contains("FAILED")
            || upper.contains("REPLAN REQUIRED")
    };
    if tools_executed_count == 0 && !has_terminal_marker {
        tracing::warn!(
            "{agent_tag}: specialist produced no tool executions or terminal marker — failing deliverable without validation"
        );
        crate::orchestrator::set_active_worker_status(
            &_active_guard.0,
            "Failed (no tools executed)",
        );
        return Ok(assemble_final_deliverable(
            false,
            Some("Specialist generated conversational text without executing any tools."),
            &final_content,
            ctx.task_id.as_deref(),
        ));
    }

    if validation_passed {
        crate::orchestrator::set_active_worker_status(&_active_guard.0, "Approved");
    } else if validator_critique.is_some() {
        crate::orchestrator::set_active_worker_status(&_active_guard.0, "Rejected");
    } else {
        crate::orchestrator::set_active_worker_status(&_active_guard.0, "Completed");
    }

    if !final_content.is_empty() {
        let assembled = assemble_final_deliverable(
            validation_passed,
            validator_critique.as_deref(),
            &final_content,
            ctx.task_id.as_deref(),
        );
        Ok(assembled)
    } else if tools_executed_count > 0 {
        let synth = format!(
            "Specialist executed {tools_executed_count} tool operations to complete the task."
        );
        let assembled = assemble_final_deliverable(
            validation_passed,
            validator_critique.as_deref(),
            &synth,
            ctx.task_id.as_deref(),
        );
        Ok(assembled)
    } else {
        Ok(assemble_final_deliverable(
            false,
            Some("Specialist produced no output deliverable or tool executions"),
            &final_content,
            ctx.task_id.as_deref(),
        ))
    }
}
