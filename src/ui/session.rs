//! Interactive session runner driving multi-turn manager and specialist execution.

use super::bridge::{
    RendererSink, SteerArbEvent, drain_steer_arbitration_events, spawn_steer_arbitration,
};
use super::helpers::*;
use super::{Event, Renderer, SubagentDetail};
use crate::config::Config;
use crate::llm::{ChatClient, StreamConfig, chat_client_turn};
use crate::manager::context::ContextEngine;
use crate::orchestrator::OrchestratorManager;
use crate::types::Message;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

/// Drive a full interactive session, dispatching events to `renderer`.
pub async fn run_session(
    cfg: &Config,
    renderer: &mut dyn Renderer,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
) -> Result<()> {
    renderer.clear_abort();
    renderer.init()?;

    let system = load_system_prompt(cfg)?;
    let mut ctx = ContextEngine::new(cfg.max_context_tokens);
    ctx.set_system_prompt(system);

    if let Some(mgr) = manager.as_ref()
        && let Ok(Some(deliverable)) = mgr.recover_frozen().await
    {
        let task_info = deliverable.task_id.as_deref().unwrap_or("recovered");
        renderer.on_event(&Event::ToolResult(format!(
            "[Recovered task {task_info}] {}",
            deliverable.content
        )));
        renderer.flush()?;
    }

    let plan = crate::agent::phase::Plan::default();
    let has_pending_plan = !plan.pending_tasks().is_empty();

    let goal = match initial {
        Some(g) => g,
        None => loop {
            match renderer.read_input() {
                Some(line) => {
                    if is_abort_command(&line) {
                        renderer.shutdown();
                        return Ok(());
                    }
                    if is_reset_command(&line) {
                        let plan = crate::agent::phase::Plan::default();
                        handle_reset_command(&plan, &mut *renderer, &mut ctx);
                        continue;
                    }
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        break trimmed.to_string();
                    } else if has_pending_plan {
                        break "Continue executing the active execution plan.".to_string();
                    }
                }
                None => {
                    renderer.shutdown();
                    return Ok(());
                }
            }
        },
    };
    ctx.set_goal(goal.clone());

    let client = ChatClient::from_config(cfg);
    let harness_stats = Arc::new(crate::harness::HarnessStats::new());
    let stream_cfg = StreamConfig::from_config(cfg);
    let mut steer_queue = Vec::<String>::new();
    let mut steer_abort_requested = false;
    let mut subagents = Vec::<SubagentDetail>::new();
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let (steer_arb_tx, mut steer_arb_rx) = tokio::sync::mpsc::unbounded_channel::<SteerArbEvent>();
    crate::orchestrator::set_status_sender(status_tx);
    crate::orchestrator::set_event_sender(event_tx);

    while !renderer.aborted() {
        while let Ok(msg) = status_rx.try_recv() {
            renderer.on_event(&Event::Status(msg));
        }
        while let Ok(ev) = event_rx.try_recv() {
            renderer.on_event(&ev);
        }

        drain_steer_arbitration_events(
            &mut steer_arb_rx,
            &mut *renderer,
            &mut steer_queue,
            &mut steer_abort_requested,
        );
        drain_delegation_events(manager.as_deref(), &mut *renderer, &mut subagents);

        if let Some(steer) = renderer.poll_input() {
            if is_abort_command(&steer) {
                renderer.request_abort();
                break;
            }
            if is_reset_command(&steer) {
                let plan = crate::agent::phase::Plan::default();
                handle_reset_command(&plan, &mut *renderer, &mut ctx);
                continue;
            }
            if !steer.trim().is_empty() {
                steer_queue.push(steer);
            }
        }
        for steer in steer_queue.drain(..) {
            ctx.append(Message::User { content: steer });
        }

        let mut turn_count = 0;
        let mut nudge_count = 0;
        loop {
            turn_count += 1;
            if turn_count > crate::agent::r#loop::MAX_TURNS || renderer.aborted() {
                break;
            }

            while let Ok(msg) = status_rx.try_recv() {
                renderer.on_event(&Event::Status(msg));
            }
            drain_steer_arbitration_events(
                &mut steer_arb_rx,
                &mut *renderer,
                &mut steer_queue,
                &mut steer_abort_requested,
            );
            drain_delegation_events(manager.as_deref(), &mut *renderer, &mut subagents);
            renderer.flush()?;

            for steer in steer_queue.drain(..) {
                ctx.append(Message::User { content: steer });
            }

            renderer.on_event(&Event::TokensIn(ctx.token_count()));
            renderer.on_event(&Event::Status(format!("Running ({})", stream_cfg.model)));
            renderer.flush()?;

            let mut bridge = RendererSink {
                renderer: &mut *renderer,
                steer_queue: &mut steer_queue,
                steer_abort_requested: &mut steer_abort_requested,
                arb_tx: &steer_arb_tx,
                arb_rx: &mut steer_arb_rx,
                client: &client,
                stats: harness_stats.clone(),
                goal: &goal,
                subagents: &subagents,
            };
            let assistant = match chat_client_turn(
                &client,
                ctx.messages().to_vec(),
                &stream_cfg,
                &mut bridge,
            )
            .await
            {
                Ok(m) => m,
                Err(e) => {
                    let category = classify_llm_error(&e);
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        category = %category,
                        "LLM turn failed"
                    );
                    renderer.on_event(&Event::Message(format!(
                            "\n[Error] LLM backend call failed ({category}): {e:#}\n(Check that your LLM server is running at {} for model `{}`)",
                            client.backend_url(),
                            stream_cfg.model
                        )));
                    renderer.on_event(&Event::Status(format!("LLM error: {category} (Ready)")));
                    renderer.flush()?;
                    break;
                }
            };

            if renderer.aborted() {
                break;
            }

            let tool_calls = match &assistant {
                Message::Assistant { tool_calls, .. } => tool_calls.clone(),
                _ => Vec::new(),
            };

            ctx.append(assistant);
            renderer.flush()?;

            if ctx.should_compact() {
                ctx.compact();
                renderer.on_event(&Event::TokensIn(ctx.token_count()));
                renderer.on_event(&Event::Status("context compacted".to_string()));
            }

            for steer in steer_queue.drain(..) {
                ctx.append(Message::User { content: steer });
            }

            if tool_calls.is_empty() {
                let current_plan = manager.as_ref().map(|m| m.plan.clone()).unwrap_or_default();
                let pending = current_plan.pending_tasks();
                if !steer_abort_requested && !pending.is_empty() && nudge_count < 5 {
                    nudge_count += 1;
                    let pending_str = pending.join(", ");
                    renderer.on_event(&Event::Status(format!(
                        "Auto-nudge ({nudge_count}/5): Plan incomplete (pending: {pending_str})"
                    )));
                    renderer.flush()?;
                    ctx.append(Message::User {
                        content: format!(
                            "(SYSTEM NOTICE: The execution plan is active on disk with pending tasks: [{pending_str}]. You are in the EXECUTING phase. You must call `delegate_task` to dispatch these pending tasks to specialists. Do NOT call `create_plan` again, and do NOT output conversational filler until all tasks are marked [x].)"
                        ),
                    });
                    continue;
                }
                break;
            }

            nudge_count = 0;

            let all_parallel = tool_calls.iter().all(|c| {
                matches!(
                    c.function.name.as_str(),
                    "delegate_task" | "read_file" | "grep_search" | "glob"
                )
            });

            if all_parallel && tool_calls.len() > 1 {
                let mut handles = Vec::new();
                for call in &tool_calls {
                    let name = call.function.name.clone();
                    let args_str = call.function.arguments.clone();
                    let args_val = serde_json::from_str::<serde_json::Value>(&args_str)
                        .unwrap_or_else(|_| serde_json::Value::String(args_str.clone()));

                    let is_delegate = name == "delegate_task";
                    let delegated_agent = if is_delegate {
                        args_val
                            .get("agent_name")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|s| s.parse::<crate::agents::Agent>().ok())
                    } else {
                        None
                    };
                    let delegated_task = if is_delegate {
                        args_val
                            .get("task_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    } else {
                        None
                    };

                    if let Some(agent) = delegated_agent {
                        let task_prompt = args_val
                            .get("prompt")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        update_subagent_lifecycle(
                            &mut subagents,
                            agent,
                            delegated_task.clone(),
                            Some(task_prompt),
                            true,
                        );
                        renderer.on_event(&Event::Delegation(
                            crate::orchestrator::DelegationEvent::Started {
                                agent,
                                task: delegated_task.clone(),
                            },
                        ));
                    } else {
                        renderer
                            .on_event(&Event::ToolCall(format_tool_call_display(&name, &args_val)));
                    }

                    let invocation = crate::harness::ToolInvocation {
                        name: name.clone(),
                        arguments: args_val,
                    };

                    let call_id = call.id.clone();
                    let handle = tokio::task::spawn_blocking(move || {
                        (
                            call_id,
                            delegated_agent,
                            delegated_task,
                            crate::harness::dispatch_for(
                                &invocation,
                                crate::harness::ToolCaller::Manager,
                            ),
                        )
                    });
                    handles.push(handle);
                }
                renderer.flush()?;

                for mut handle in handles {
                    let res = loop {
                        while let Ok(msg) = status_rx.try_recv() {
                            renderer.on_event(&Event::Status(msg));
                            let _ = renderer.flush();
                        }
                        while let Ok(ev) = event_rx.try_recv() {
                            renderer.on_event(&ev);
                            let _ = renderer.flush();
                        }
                        drain_steer_arbitration_events(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                            &mut steer_abort_requested,
                        );
                        drain_delegation_events(manager.as_deref(), &mut *renderer, &mut subagents);
                        if renderer.aborted() {
                            crate::orchestrator::cancel_all();
                            break (
                                String::new(),
                                None,
                                None,
                                Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                    "aborted"
                                ))),
                            );
                        }
                        match tokio::time::timeout(Duration::from_millis(20), &mut handle).await {
                            Ok(Ok(r)) => break r,
                            Ok(Err(e)) => {
                                break (
                                    String::new(),
                                    None,
                                    None,
                                    Err(crate::harness::ToolError::Execution(e.into())),
                                );
                            }
                            Err(_) => {
                                while let Ok(msg) = status_rx.try_recv() {
                                    renderer.on_event(&Event::Status(msg));
                                    let _ = renderer.flush();
                                }
                                while let Ok(ev) = event_rx.try_recv() {
                                    renderer.on_event(&ev);
                                    let _ = renderer.flush();
                                }
                                drain_steer_arbitration_events(
                                    &mut steer_arb_rx,
                                    &mut *renderer,
                                    &mut steer_queue,
                                    &mut steer_abort_requested,
                                );
                                drain_delegation_events(
                                    manager.as_deref(),
                                    &mut *renderer,
                                    &mut subagents,
                                );
                                if let Some(input) = renderer.poll_input() {
                                    if is_abort_command(&input) {
                                        renderer.request_abort();
                                    } else if is_reset_command(&input) {
                                        let plan = crate::agent::phase::Plan::default();
                                        handle_reset_command(&plan, &mut *renderer, &mut ctx);
                                    } else if !input.trim().is_empty() {
                                        spawn_steer_arbitration(
                                            &client,
                                            harness_stats.clone(),
                                            &goal,
                                            &subagents,
                                            input,
                                            &steer_arb_tx,
                                            &mut *renderer,
                                        );
                                    }
                                }
                            }
                        }
                    };

                    let (call_id, agent, task, tool_res) = res;
                    let result_content = match tool_res {
                        Ok(r) => r.content,
                        Err(e) => format!("ERROR: {e}"),
                    };
                    if let Some(ag) = agent {
                        update_subagent_lifecycle(&mut subagents, ag, task.clone(), None, false);
                        renderer.on_event(&Event::Delegation(
                            crate::orchestrator::DelegationEvent::Completed { agent: ag, task },
                        ));
                    } else {
                        renderer.on_event(&Event::ToolResult(result_content.clone()));
                    }
                    renderer.flush()?;

                    drain_delegation_events(manager.as_deref(), &mut *renderer, &mut subagents);

                    ctx.append(Message::Tool {
                        tool_call_id: call_id,
                        content: result_content,
                    });
                }
            } else {
                for call in tool_calls {
                    if renderer.aborted() {
                        break;
                    }

                    let name = call.function.name.clone();
                    let args_str = call.function.arguments.clone();
                    let args_val = serde_json::from_str::<serde_json::Value>(&args_str)
                        .unwrap_or_else(|_| serde_json::Value::String(args_str.clone()));

                    let is_delegate = name == "delegate_task";
                    let args_obj = args_val.as_object();
                    let delegated_agent = if is_delegate {
                        args_obj
                            .and_then(|o| o.get("agent_name"))
                            .and_then(serde_json::Value::as_str)
                            .and_then(|s| s.parse::<crate::agents::Agent>().ok())
                    } else {
                        None
                    };
                    let delegated_task = if is_delegate {
                        args_obj
                            .and_then(|o| o.get("task_id"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    } else {
                        None
                    };

                    if let Some(agent) = delegated_agent {
                        let task_prompt = args_obj
                            .and_then(|o| o.get("prompt"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        update_subagent_lifecycle(
                            &mut subagents,
                            agent,
                            delegated_task.clone(),
                            Some(task_prompt),
                            true,
                        );
                        renderer.on_event(&Event::Delegation(
                            crate::orchestrator::DelegationEvent::Started {
                                agent,
                                task: delegated_task.clone(),
                            },
                        ));
                    } else {
                        renderer
                            .on_event(&Event::ToolCall(format_tool_call_display(&name, &args_val)));
                    }
                    renderer.flush()?;

                    drain_delegation_events(manager.as_deref(), &mut *renderer, &mut subagents);

                    let invocation = crate::harness::ToolInvocation {
                        name: name.clone(),
                        arguments: args_val,
                    };

                    let mut handle = tokio::task::spawn_blocking(move || {
                        crate::harness::dispatch_for(
                            &invocation,
                            crate::harness::ToolCaller::Manager,
                        )
                    });

                    let result = loop {
                        while let Ok(msg) = status_rx.try_recv() {
                            renderer.on_event(&Event::Status(msg));
                            let _ = renderer.flush();
                        }
                        while let Ok(ev) = event_rx.try_recv() {
                            renderer.on_event(&ev);
                            let _ = renderer.flush();
                        }
                        drain_steer_arbitration_events(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                            &mut steer_abort_requested,
                        );
                        drain_delegation_events(manager.as_deref(), &mut *renderer, &mut subagents);
                        if renderer.aborted() {
                            crate::orchestrator::cancel_all();
                            break Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                "aborted"
                            )));
                        }
                        match tokio::time::timeout(Duration::from_millis(20), &mut handle).await {
                            Ok(res) => {
                                break res
                                    .map_err(|e| crate::harness::ToolError::Execution(e.into()))
                                    .and_then(|r| r);
                            }
                            Err(_) => {
                                while let Ok(msg) = status_rx.try_recv() {
                                    renderer.on_event(&Event::Status(msg));
                                    let _ = renderer.flush();
                                }
                                while let Ok(ev) = event_rx.try_recv() {
                                    renderer.on_event(&ev);
                                    let _ = renderer.flush();
                                }
                                drain_steer_arbitration_events(
                                    &mut steer_arb_rx,
                                    &mut *renderer,
                                    &mut steer_queue,
                                    &mut steer_abort_requested,
                                );
                                drain_delegation_events(
                                    manager.as_deref(),
                                    &mut *renderer,
                                    &mut subagents,
                                );
                                if let Some(input) = renderer.poll_input() {
                                    if is_abort_command(&input) {
                                        renderer.request_abort();
                                    } else if is_reset_command(&input) {
                                        let plan = crate::agent::phase::Plan::default();
                                        handle_reset_command(&plan, &mut *renderer, &mut ctx);
                                    } else if !input.trim().is_empty() {
                                        spawn_steer_arbitration(
                                            &client,
                                            harness_stats.clone(),
                                            &goal,
                                            &subagents,
                                            input,
                                            &steer_arb_tx,
                                            &mut *renderer,
                                        );
                                    }
                                }
                            }
                        }
                    };

                    let result_content = match result {
                        Ok(res) => res.content,
                        Err(e) => format!("ERROR: {e}"),
                    };
                    if let Some(agent) = delegated_agent {
                        update_subagent_lifecycle(
                            &mut subagents,
                            agent,
                            delegated_task.clone(),
                            None,
                            false,
                        );
                        renderer.on_event(&Event::Delegation(
                            crate::orchestrator::DelegationEvent::Completed {
                                agent,
                                task: delegated_task,
                            },
                        ));
                    } else {
                        renderer.on_event(&Event::ToolResult(result_content.clone()));
                    }
                    renderer.flush()?;

                    drain_delegation_events(manager.as_deref(), &mut *renderer, &mut subagents);

                    ctx.append(Message::Tool {
                        tool_call_id: call.id,
                        content: result_content,
                    });
                }
            }

            let current_plan = manager.as_ref().map(|m| m.plan.clone()).unwrap_or_default();
            if current_plan.is_complete() {
                ctx.append(Message::User {
                    content: "(SYSTEM NOTICE: All execution plan tasks are now COMPLETE [x]. Do NOT execute any more tools or re-delegate. Deliver your comprehensive final answer/synthesis to the user now.)".to_string(),
                });
            }
        }

        drain_steer_arbitration_events(
            &mut steer_arb_rx,
            &mut *renderer,
            &mut steer_queue,
            &mut steer_abort_requested,
        );
        if steer_abort_requested || renderer.aborted() {
            if steer_abort_requested {
                // Steer arbitrator requested AbortImmediately / RejectPlan -> reset abort state, reset subagents, and start next turn immediately
                steer_abort_requested = false;
                renderer.clear_abort();
                for s in subagents.iter_mut() {
                    if s.is_active {
                        s.is_active = false;
                        s.logs.push("[aborted by user]".to_string());
                    }
                }
                for steer in steer_queue.drain(..) {
                    ctx.append(Message::User { content: steer });
                }
                renderer.on_event(&Event::Status(
                    "Steering redirection: aborted current turn, starting next turn with updated context".to_string(),
                ));
                renderer.flush()?;
                continue;
            } else {
                break;
            }
        }

        // If user queued instructions during execution, start next turn immediately without blocking at read_input
        if !steer_queue.is_empty() {
            for steer in steer_queue.drain(..) {
                ctx.append(Message::User { content: steer });
            }
            continue;
        }

        match renderer.read_input() {
            Some(line) => {
                if is_abort_command(&line) {
                    renderer.request_abort();
                    break;
                }
                if is_reset_command(&line) {
                    let plan = crate::agent::phase::Plan::default();
                    handle_reset_command(&plan, &mut *renderer, &mut ctx);
                    continue;
                }
                if !line.trim().is_empty() {
                    ctx.append(Message::User { content: line });
                }
            }
            None => {
                break;
            }
        }
    }

    renderer.on_event(&Event::Done);
    renderer.flush().ok();
    renderer.shutdown();
    Ok(())
}
