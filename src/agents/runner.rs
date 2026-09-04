//! Specialist live execution runner, turn loops, and deliverable assembly.

use crate::agents::validation::run_automated_validation;
use crate::agents::{Agent, IsolatedContext};
use crate::tool_names::{
    TOOL_DELEGATE_TASK, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_LEAVE_VERDICT, TOOL_READ_FILE,
    TOOL_REPLACE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
};

pub(crate) async fn run_specialist_llm(
    agent: Agent,
    ctx: &IsolatedContext,
    token: &tokio_util::sync::CancellationToken,
) -> String {
    let snippet_block = if ctx.snippets.is_empty() {
        "(none)".to_string()
    } else {
        ctx.snippets.join("\n---\n")
    };
    let canned = format!(
        "Specialist role `{}` executed its isolated task to completion.\n\n\
         TASK BRIEF:\n{}\n\n\
         BOUNDED SNIPPETS ({count}):\n{snippet_block}\n\n\
         MISSION COMPLETE",
        ctx.role_system_prompt
            .trim()
            .lines()
            .next()
            .unwrap_or("specialist"),
        ctx.brief,
        count = ctx.snippets.len(),
        snippet_block = snippet_block,
    );

    if let Some(res) = try_run_specialist_live(agent, ctx, token).await {
        return res;
    }
    canned
}

pub(crate) async fn try_run_specialist_live(
    agent: Agent,
    ctx: &IsolatedContext,
    token: &tokio_util::sync::CancellationToken,
) -> Option<String> {
    if tokio::runtime::Handle::try_current().is_err() {
        return None;
    }
    // If running inside cargo test suite (integration tests binaries in target/.../deps/), bypass live network calls
    if std::env::current_exe()
        .map(|p| p.to_string_lossy().contains("/deps/"))
        .unwrap_or(false)
        && std::env::var("MARMEL_LIVE_TEST").is_err()
    {
        return None;
    }
    let cfg = crate::config::load(None).ok()?;
    let specialist_cfg = cfg.orchestration.specialists.get(agent.as_str());
    let backend_url = specialist_cfg
        .and_then(|sc| sc.backend_url.as_ref())
        .unwrap_or(&cfg.backend_url);
    if backend_url.is_empty() {
        return None;
    }
    let auth_token = specialist_cfg
        .and_then(|sc| sc.auth_token.as_ref())
        .unwrap_or(&cfg.auth_token);
    let model = specialist_cfg
        .and_then(|sc| sc.model.as_ref())
        .unwrap_or(&cfg.model);
    let client = crate::llm::ChatClient::new_with_token(backend_url, model, auth_token);
    let res = match run_specialist_live(&client, agent, ctx, &cfg, token).await {
        Ok(s) => s,
        Err(e) => format!("Specialist execution failed: {e}\n\nFAILED"),
    };
    Some(res)
}

pub(crate) fn format_tool_args_preview(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        TOOL_READ_FILE | TOOL_WRITE_FILE | TOOL_REPLACE => args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_RUN_COMMAND => {
            let cmd = args
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if cmd.len() > 40 {
                format!("{}…", &cmd[..37])
            } else {
                cmd.to_string()
            }
        }
        TOOL_GREP_SEARCH => args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_GLOB => args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => {
            let s = args.to_string();
            if s.len() > 30 {
                format!("{}…", &s[..27])
            } else {
                s
            }
        }
    }
}

pub(crate) fn format_tool_args_full(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        TOOL_READ_FILE | TOOL_WRITE_FILE | TOOL_REPLACE => {
            if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
                if tool == TOOL_WRITE_FILE || tool == TOOL_REPLACE {
                    let len = args
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map_or(0, str::len);
                    format!("{path} (content: {len} bytes)")
                } else {
                    path.to_string()
                }
            } else {
                args.to_string()
            }
        }
        TOOL_RUN_COMMAND => args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_GREP_SEARCH => {
            let q = args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if path.is_empty() {
                format!("query=\"{q}\"")
            } else {
                format!("query=\"{q}\", path=\"{path}\"")
            }
        }
        TOOL_GLOB => args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_DELEGATE_TASK => {
            let ag = args
                .get("agent_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let tid = args
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let pr = args
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("agent={ag}, task_id={tid}, prompt=\"{pr}\"")
        }
        TOOL_LEAVE_VERDICT => {
            let v = args
                .get("verdict")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let c = args
                .get("comments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("verdict={v}, comments=\"{c}\"")
        }
        _ => args.to_string(),
    }
}

pub(crate) fn update_revision(final_content: &mut String, revised: &str) {
    if !revised.is_empty() {
        if final_content.is_empty() {
            *final_content = revised.to_string();
        } else if !final_content.contains(revised) {
            final_content.push_str("\n\n");
            final_content.push_str(revised);
        }
    }
}

pub(crate) fn assemble_final_deliverable(
    validation_passed: bool,
    validator_critique: Option<&str>,
    final_content: &str,
) -> String {
    let upper = final_content.to_ascii_uppercase();
    let has_complete = upper.contains("MISSION COMPLETE");
    let has_failed = upper.contains("FAILED");
    let has_replan = upper.contains("REPLAN REQUIRED");

    if validation_passed {
        if has_complete || has_failed || has_replan {
            return final_content.to_string();
        }
        let mut res = final_content.to_string();
        if res.trim().is_empty() {
            res = "Specialist terminated without deliverable.\n\nFAILED (incomplete)".to_string();
        } else {
            res.push_str("\n\nFAILED (task concluded without explicit completion)");
        }
        return res;
    }

    let mut rejected = String::new();
    if let Some(critique) = validator_critique {
        rejected.push_str(&format!(
            "VALIDATOR REJECTION: {critique}\n---------------\n"
        ));
    }
    let revision = final_content
        .replace("MISSION COMPLETE", "REVOKED")
        .replace("mission complete", "REVOKED");
    rejected.push_str(&revision);
    if !rejected.contains("FAILED") && !rejected.contains("REPLAN REQUIRED") {
        rejected.push_str("\n\nFAILED (Validator rejected deliverable)");
    }
    rejected
}

pub async fn run_specialist_live(
    client: &crate::llm::ChatClient,
    agent: Agent,
    ctx: &IsolatedContext,
    cfg: &crate::config::Config,
    token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<String> {
    let env_block = crate::prompts::format_environment_block();

    let enhanced_system_prompt = format!(
        "{}\n\n{}\n- Tools available: `write_file`, `replace`, `read_file`, `run_command`, `grep_search`, `glob`, `rebirth`.\n- You MUST save files and execute real work to complete the task.",
        ctx.role_system_prompt, env_block
    );

    let mut engine = crate::agent::ContextEngineFactory::new(cfg.max_context_tokens)
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

    let agent_tag = match &ctx.task_id {
        Some(t) if !t.trim().is_empty() => format!("{agent}-{t}"),
        _ => format!("{agent}"),
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

    let _active_guard = crate::orchestrator::register_active_worker(
        ctx.task_id.clone(),
        agent.as_str().to_string(),
        ctx.brief.clone(),
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
    let mut tools_executed_count = 0usize;

    for _turn in 0..100 {
        if token.is_cancelled() {
            tracing::warn!("{agent_tag}: aborted by cancellation signal");
            return Ok("Task aborted by user instruction.\n\nFAILED (aborted)".to_string());
        }
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
        let mut sink =
            crate::orchestrator::PreemptibleStreamSink::register(&agent_tag, &specialist_model);
        let stream_out = crate::llm::chat_stream_resumable(
            client,
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
        let rep_triggered = out.rep_triggered;
        if budget_exceeded {
            tracing::warn!(
                "{agent_tag}: maximum single-turn output budget of {max_tokens} tokens exceeded — cutting stream"
            );
            crate::orchestrator::emit_status(format!(
                "{agent_tag}: single-turn output budget ({max_tokens} tokens) reached"
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

        let assistant_content = if reply.content.is_empty() && !reply.reasoning.is_empty() {
            Some("[Thinking completed without content or tool calls]".to_string())
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

        let full_text = if reply.reasoning.is_empty() {
            reply.content.clone()
        } else {
            format!("{}\n{}", reply.reasoning, reply.content)
        };
        let is_repeating = rep_triggered || monitor.feed_text(&full_text);

        if tool_calls.is_empty() {
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
                        content: "SYSTEM NOTICE: Repetitive generation loop detected in your thoughts or responses. Terminate conversational debate immediately and invoke your required tools (such as `read_file`, `write_file`, `run_command`, etc.) to perform the required work, or conclude with 'MISSION COMPLETE'.".to_string(),
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
            break;
        }

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
                    let tool_res = crate::harness::dispatch_for_with_engine(
                        &invocation,
                        crate::harness::ToolCaller::Specialist(agent),
                        Some(&mut engine),
                    );
                    match tool_res {
                        Ok(r) => {
                            tools_executed_count += 1;
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
        return Ok(assemble_final_deliverable(
            false,
            Some("Specialist generated conversational text without executing any tools."),
            &final_content,
        ));
    }

    let auto_validate_enabled = specialist_cfg
        .and_then(|sc| sc.enable_validator)
        .unwrap_or(true);
    let max_val_iterations = specialist_cfg
        .and_then(|sc| sc.max_validator_iterations)
        .unwrap_or(5);

    let mut validation_passed =
        !auto_validate_enabled || max_val_iterations == 0 || agent == Agent::Validator;
    let mut validator_critique: Option<String> = None;

    if auto_validate_enabled
        && max_val_iterations > 0
        && agent != Agent::Validator
        && !final_content.is_empty()
    {
        for val_iter in 0..max_val_iterations {
            if token.is_cancelled() {
                tracing::warn!("{agent_tag}: aborted before validation pass");
                return Ok("Task aborted by user instruction.\n\nFAILED (aborted)".to_string());
            }
            crate::orchestrator::emit_status(format!(
                "validator-{agent_tag}: testing deliverable (pass {}/{})...",
                val_iter + 1,
                max_val_iterations
            ));
            if let Ok((approved, critique)) =
                run_automated_validation(client, agent, &ctx.brief, &final_content, cfg, token)
                    .await
            {
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
                    break;
                } else {
                    let feedback = if critique.trim().is_empty() {
                        "Deliverable failed verification checks.".to_string()
                    } else {
                        critique.clone()
                    };
                    validator_critique = Some(feedback.clone());
                    crate::orchestrator::emit_status(format!(
                        "[Validator] REJECTED deliverable for {agent_tag} (pass {}/{}):\n{feedback}",
                        val_iter + 1,
                        max_val_iterations
                    ));
                    tracing::warn!(
                        "Automated validator REJECTED specialist deliverable for {}: {}",
                        agent_tag,
                        feedback
                    );
                    let feedback_msg = format!(
                        "Validation feedback: The validator tested your changes and found issues:\n{}\n\n\
                         Please address all validator critique points, verify your work with available tools, and conclude with 'MISSION COMPLETE'.",
                        feedback
                    );
                    engine.append(crate::types::Message::User {
                        content: feedback_msg,
                    });

                    let mut latest_revision = String::new();
                    let mut rev_nudge_count = 0usize;
                    let mut rev_rep_detector = crate::harness::monitor::RepetitionDetector::new(
                        mon_cfg.repetition_threshold,
                        mon_cfg.min_pattern_len,
                    );
                    for rev_turn in 0..25 {
                        if token.is_cancelled() {
                            tracing::warn!("{agent_tag}: aborted during revision");
                            return Ok(
                                "Task aborted by user instruction.\n\nFAILED (aborted)".to_string()
                            );
                        }
                        crate::orchestrator::emit_status(format!(
                            "{agent_tag}: revising code per validator critique (step {}/25)...",
                            rev_turn + 1
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
                        let mut sink = crate::orchestrator::PreemptibleStreamSink::register(
                            &agent_tag,
                            &specialist_model,
                        );
                        let stream_out = crate::llm::chat_stream_resumable(
                            client,
                            &req,
                            &mut sink,
                            max_tokens,
                            &mut rev_rep_detector,
                            false,
                            Some(token),
                        )
                        .await;

                        let out = match stream_out {
                            Ok(o) => o,
                            Err(e) => {
                                if token.is_cancelled() {
                                    tracing::warn!("{agent_tag}: aborted during LLM revision call");
                                    return Ok(
                                        "Task aborted by user instruction.\n\nFAILED (aborted)"
                                            .to_string(),
                                    );
                                }
                                tracing::error!(
                                    "{agent_tag}: LLM chat call error on revision step {rev_turn}: {e:?}"
                                );
                                break;
                            }
                        };

                        if out.was_aborted_by_steer || token.is_cancelled() {
                            tracing::warn!("{agent_tag}: aborted during LLM revision call");
                            return Ok(
                                "Task aborted by user instruction.\n\nFAILED (aborted)".to_string()
                            );
                        }

                        let reply = out.reply;
                        let budget_exceeded = out.budget_exceeded;
                        let rep_triggered = out.rep_triggered;
                        if budget_exceeded {
                            tracing::warn!(
                                "{agent_tag}: maximum single-turn output budget of {max_tokens} tokens exceeded during revision"
                            );
                        }
                        if !reply.content.is_empty() {
                            latest_revision = reply.content.clone();
                        }

                        let mut tool_calls = reply.tool_calls.clone();
                        if tool_calls.is_empty() && cfg.enable_xml_rescue {
                            let monitor = crate::harness::monitor::HarnessMonitor::with_new_stats();
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

                        let full_rev_text = if reply.reasoning.is_empty() {
                            reply.content.clone()
                        } else {
                            format!("{}\n{}", reply.reasoning, reply.content)
                        };
                        let is_repeating = rep_triggered || monitor.feed_text(&full_rev_text);

                        if tool_calls.is_empty() {
                            if budget_exceeded && rev_nudge_count < 2 {
                                rev_nudge_count += 1;
                                tracing::warn!(
                                    "{agent_tag}: output budget exceeded during revision — injecting corrective nudge"
                                );
                                engine.append(crate::types::Message::User {
                                    content: format!(
                                        "SYSTEM NOTICE: Your revision response exceeded the single-turn output budget limit ({max_tokens} tokens) and was truncated. Please be concise and call your required tools (such as `write_file`, `replace`, `run_command`, etc.) to apply the necessary fixes."
                                    ),
                                });
                                continue;
                            }

                            if is_repeating {
                                if rev_nudge_count < 2 {
                                    rev_nudge_count += 1;
                                    tracing::warn!(
                                        "{agent_tag}: repetitive generation loop detected during revision — injecting corrective nudge ({rev_nudge_count}/2)"
                                    );
                                    engine.replace_last(crate::types::Message::Assistant {
                                        content: Some(
                                            "[Generation interrupted due to repetitive loop]"
                                                .to_string(),
                                        ),
                                        reasoning_content: None,
                                        tool_calls: Vec::new(),
                                    });
                                    rev_rep_detector =
                                        crate::harness::monitor::RepetitionDetector::new(
                                            mon_cfg.repetition_threshold,
                                            mon_cfg.min_pattern_len,
                                        );
                                    engine.append(crate::types::Message::User {
                                        content: "SYSTEM NOTICE: Repetitive generation loop detected in your revision output. Terminate conversational debate immediately and invoke your required tools to apply the necessary fixes, or conclude with 'MISSION COMPLETE'.".to_string(),
                                    });
                                    continue;
                                } else {
                                    tracing::warn!(
                                        "{agent_tag}: repetitive generation loop persisted during revision — breaking revision loop"
                                    );
                                    break;
                                }
                            }

                            let upper = reply.content.to_ascii_uppercase();
                            let is_terminal = upper.contains("MISSION COMPLETE")
                                || upper.contains("FAILED")
                                || upper.contains("REPLAN REQUIRED");
                            if !is_terminal {
                                if rev_nudge_count < 2 {
                                    rev_nudge_count += 1;
                                    engine.append(crate::types::Message::User {
                                        content: "SYSTEM NOTICE: You did not call any tools to address the validator critique or output MISSION COMPLETE. Do not output conversational prose. Please use your tools (such as `read_file`, `write_file`, `replace`, `run_command`, etc.) to apply the necessary fixes, verify with tests, and conclude with 'MISSION COMPLETE'.".to_string(),
                                    });
                                    continue;
                                } else {
                                    tracing::warn!(
                                        "{agent_tag}: specialist produced no tool calls during revision after {rev_nudge_count} nudges — terminating revision"
                                    );
                                    break;
                                }
                            }
                            break;
                        }

                        rev_nudge_count = 0;

                        for tc in tool_calls {
                            let args_val =
                                serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                                    .unwrap_or_else(|_| {
                                        serde_json::Value::String(tc.function.arguments.clone())
                                    });
                            let desc = format_tool_args_preview(&tc.function.name, &args_val);
                            crate::orchestrator::emit_status(format!(
                                "{agent_tag}: running {}({desc})",
                                tc.function.name
                            ));
                            let full_args = format_tool_args_full(&tc.function.name, &args_val);
                            tracing::info!(
                                "{agent_tag} (revision) invoking tool: {}({})",
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
                                        "{agent_tag} (revision) tool {} blocked by repetition detector",
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
                                        crate::harness::ToolCaller::Specialist(agent),
                                        Some(&mut engine),
                                    );
                                    match tool_res {
                                        Ok(r) => {
                                            tracing::info!(
                                                "{agent_tag} (revision) tool {} completed with {} chars",
                                                tc.function.name,
                                                r.content.len()
                                            );
                                            r.content
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "{agent_tag} (revision) tool {} error: {e}",
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
                    if !latest_revision.is_empty() {
                        final_content = latest_revision;
                    }
                }
            } else {
                break;
            }
        }
    }

    if !final_content.is_empty() {
        let assembled = assemble_final_deliverable(
            validation_passed,
            validator_critique.as_deref(),
            &final_content,
        );
        Ok(assembled)
    } else {
        Ok(assemble_final_deliverable(
            false,
            Some("Specialist produced no output deliverable or tool executions"),
            &final_content,
        ))
    }
}
