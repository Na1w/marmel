//! Tool-call dispatch: parallel and sequential execution of the assistant's tool calls,
//! including `delegate_task` lifecycle, repetition monitoring, and mid-flight input handling.

use crate::harness::monitor::HarnessMonitor;
use crate::llm::ChatClient;
use crate::manager::context::ContextEngine;
use crate::manager::phase::Plan;
use crate::orchestrator::OrchestratorManager;
use crate::types::{Message, ToolCall};
use crate::ui::bridge::{
    SteerArbEvent, drain_steer_arbitration_events_with_transcript, spawn_steer_arbitration,
};
use crate::ui::helpers::{
    drain_delegation_events_with_transcript, format_tool_call_display, handle_reset_command,
    is_abort_command, is_reset_command, update_subagent_lifecycle,
};
use crate::ui::{Event, Renderer, SubagentDetail, UiRecord, UiTranscript};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Bundles the mutable session state the tool-dispatch step needs.
pub(crate) struct DispatchState<'a> {
    pub renderer: &'a mut dyn Renderer,
    pub status_rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<String>,
    pub event_rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    pub steer_arb_rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<SteerArbEvent>,
    pub steer_arb_tx: &'a tokio::sync::mpsc::UnboundedSender<SteerArbEvent>,
    pub steer_queue: &'a mut Vec<String>,
    pub steer_abort_requested: &'a mut bool,
    pub subagents: &'a mut Vec<SubagentDetail>,
    pub ui_transcript: &'a mut UiTranscript,
    pub ui_transcript_path: &'a PathBuf,
    pub transcript_path: &'a PathBuf,
    pub ctx: &'a mut ContextEngine,
    pub manager: Option<Arc<OrchestratorManager>>,
    pub client: &'a ChatClient,
    pub harness_stats: &'a Arc<crate::harness::HarnessStats>,
    pub goal: &'a str,
    pub steering_history: &'a Arc<std::sync::RwLock<Vec<(String, String)>>>,
    pub plan: &'a Plan,
    pub monitor: &'a mut HarnessMonitor,
}

/// Execute the assistant's `tool_calls`, updating the renderer, transcripts, subagents, and context.
pub(crate) async fn dispatch_tool_calls(
    state: &mut DispatchState<'_>,
    tool_calls: Vec<ToolCall>,
) -> Result<(), anyhow::Error> {
    let DispatchState {
        renderer,
        status_rx,
        event_rx,
        steer_arb_rx,
        steer_arb_tx,
        steer_queue,
        steer_abort_requested,
        subagents,
        ui_transcript,
        ui_transcript_path,
        transcript_path,
        ctx,
        manager,
        client,
        harness_stats,
        goal,
        steering_history,
        plan,
        monitor,
    } = state;

    let all_parallel = tool_calls.iter().all(|c| {
        c.function.name == crate::tool_names::TOOL_DELEGATE_TASK || crate::manager::is_read_tool(&c.function.name)
    });

    if all_parallel && tool_calls.len() > 1 {
        ctx.reset_consecutive_rebirths();
        let mut handles = Vec::new();
        for call in &tool_calls {
            let name = call.function.name.clone();
            let args_str = call.function.arguments.clone();
            let args_val = serde_json::from_str::<serde_json::Value>(&args_str)
                .unwrap_or_else(|_| serde_json::Value::String(args_str.clone()));

            let is_delegate = name == crate::tool_names::TOOL_DELEGATE_TASK;
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
                    .map(|s| {
                        s.trim_matches(|c| {
                            c == '['
                                || c == ']'
                                || c == '('
                                || c == ')'
                                || c == '"'
                                || c == '\''
                        })
                        .trim()
                        .to_string()
                    })
                    .filter(|s| !s.is_empty())
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
                renderer.set_subagents(subagents.clone());
                renderer.on_event(&Event::Delegation(
                    crate::orchestrator::DelegationEvent::Started {
                        agent,
                        task: delegated_task.clone(),
                    },
                ));
            } else {
                let display = format_tool_call_display(&name, &args_val);
                renderer.on_event(&Event::ToolCall(display.clone()));
                ui_transcript.append(UiRecord::ToolCall { display });
                let _ = ui_transcript.save(&ui_transcript_path);
            }

            if call.is_malformed() {
                let call_id = call.id.clone();
                let err_msg = format!(
                    "ERROR: Invalid or truncated arguments for tool '{name}': output was cut off or contained unterminated JSON. Please reissue the tool call with complete, valid JSON arguments."
                );
                let handle = tokio::task::spawn_blocking(move || {
                    (
                        call_id,
                        delegated_agent,
                        delegated_task,
                        Ok(crate::harness::ToolResult::err(err_msg)),
                    )
                });
                handles.push(handle);
                continue;
            }

            let intervention = monitor.observe_tool(&name, &args_val);
            if matches!(
                intervention,
                crate::harness::monitor::Intervention::Block
                    | crate::harness::monitor::Intervention::Cut
            ) {
                let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                    format!(
                        "ERROR: Tool repetition detected for '{}'. Do not repeat identical calls.",
                        call.function.name
                    )
                });
                tracing::warn!(
                    "Manager tool {} blocked by repetition detector",
                    call.function.name
                );
                let call_id = call.id.clone();
                let handle = tokio::task::spawn_blocking(move || {
                    (
                        call_id,
                        delegated_agent,
                        delegated_task,
                        Ok(crate::harness::ToolResult::err(err_msg)),
                    )
                });
                handles.push(handle);
                continue;
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
                let mut had_events = false;
                while let Ok(msg) = status_rx.try_recv() {
                    renderer.on_event(&Event::Status(msg));
                    had_events = true;
                }
                while let Ok(ev) = event_rx.try_recv() {
                    renderer.on_event(&ev);
                    had_events = true;
                }
                if had_events {
                    let _ = renderer.flush();
                }
                drain_steer_arbitration_events_with_transcript(
                    &mut steer_arb_rx,
                    &mut *renderer,
                    &mut steer_queue,
                    &mut steer_abort_requested,
                    Some(&mut subagents),
                    Some(&mut ui_transcript),
                );
                drain_delegation_events_with_transcript(
                    manager.as_deref(),
                    &mut *renderer,
                    &mut subagents,
                    Some(&mut ui_transcript),
                );
                if renderer.aborted()
                    || steer_abort_requested
                    || crate::orchestrator::is_globally_cancelled()
                {
                    crate::orchestrator::cancel_all();
                    let _ =
                        tokio::time::timeout(Duration::from_millis(500), &mut handle).await;
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
                        let mut had_events = false;
                        while let Ok(msg) = status_rx.try_recv() {
                            renderer.on_event(&Event::Status(msg));
                            had_events = true;
                        }
                        while let Ok(ev) = event_rx.try_recv() {
                            renderer.on_event(&ev);
                            had_events = true;
                        }
                        if had_events {
                            let _ = renderer.flush();
                        }
                        drain_steer_arbitration_events_with_transcript(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                            &mut steer_abort_requested,
                            Some(&mut subagents),
                            Some(&mut ui_transcript),
                        );
                        drain_delegation_events_with_transcript(
                            manager.as_deref(),
                            &mut *renderer,
                            &mut subagents,
                            Some(&mut ui_transcript),
                        );
                        if let Some(input) = renderer.poll_input() {
                            if is_abort_command(&input) {
                                crate::debug_log::log_user_input("command", &input);
                                renderer.request_user_exit();
                            } else if is_reset_command(&input) {
                                handle_reset_command(&plan, &mut *renderer, Some(&mut ctx));
                                ui_transcript.clear();
                                if let Ok(mut hist) = steering_history.write() {
                                    hist.clear();
                                }
                                let _ = std::fs::remove_file(&ui_transcript_path);
                            } else if !input.trim().is_empty() {
                                spawn_steer_arbitration(
                                    &client,
                                    harness_stats.clone(),
                                    &goal,
                                    &subagents,
                                    input,
                                    &steer_arb_tx,
                                    &mut *renderer,
                                    Some(Arc::clone(&steering_history)),
                                );
                            }
                        }
                    }
                }
            };

            let (call_id, agent, task, tool_res) = res;
            let (result_content, is_error) = match tool_res {
                Ok(r) => (r.content, r.is_error),
                Err(e) => (format!("ERROR: {e}"), true),
            };
            if let Some(ag) = agent {
                update_subagent_lifecycle(&mut subagents, ag, task.clone(), None, false);
                let tid = task.clone().unwrap_or_else(|| ag.to_string());
                if is_error {
                    let reason = crate::ui::helpers::extract_failure_reason(&result_content);
                    renderer.on_event(&Event::Delegation(
                        crate::orchestrator::DelegationEvent::Failed {
                            agent: ag,
                            task,
                            reason: Some(reason.clone()),
                        },
                    ));
                    ui_transcript.append(UiRecord::TaskFailed {
                        task_id: tid,
                        reason: Some(reason),
                    });
                } else {
                    if let Some(ref tid_task) = task {
                        let plan = crate::manager::phase::Plan::default();
                        let _ = plan.check_off(tid_task);
                    }
                    renderer.on_event(&Event::Delegation(
                        crate::orchestrator::DelegationEvent::Completed { agent: ag, task },
                    ));
                    ui_transcript.append(UiRecord::TaskCompleted { task_id: tid });
                }
                let _ = ui_transcript.save(&ui_transcript_path);
                renderer.set_subagents(subagents.clone());
            } else {
                renderer.on_event(&Event::ToolResult(result_content.clone()));
                ui_transcript.append(UiRecord::ToolResult {
                    display: result_content.clone(),
                });
                let _ = ui_transcript.save(&ui_transcript_path);
            }
            renderer.flush()?;

            drain_delegation_events_with_transcript(
                manager.as_deref(),
                &mut *renderer,
                &mut subagents,
                Some(&mut ui_transcript),
            );

            ctx.append(Message::Tool {
                tool_call_id: call_id,
                content: result_content,
            });
            let _ = ctx.save_transcript(&transcript_path);
        }
    } else {
        for call in &tool_calls {
            if renderer.aborted() {
                break;
            }

            let name = call.function.name.clone();
            let args_str = call.function.arguments.clone();
            let args_val = serde_json::from_str::<serde_json::Value>(&args_str)
                .unwrap_or_else(|_| serde_json::Value::String(args_str.clone()));

            let is_delegate = name == crate::tool_names::TOOL_DELEGATE_TASK;
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
                    .map(|s| {
                        s.trim_matches(|c| {
                            c == '['
                                || c == ']'
                                || c == '('
                                || c == ')'
                                || c == '"'
                                || c == '\''
                        })
                        .trim()
                        .to_string()
                    })
                    .filter(|s| !s.is_empty())
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
                renderer.set_subagents(subagents.clone());
                renderer.on_event(&Event::Delegation(
                    crate::orchestrator::DelegationEvent::Started {
                        agent,
                        task: delegated_task.clone(),
                    },
                ));
            } else {
                let display = format_tool_call_display(&name, &args_val);
                renderer.on_event(&Event::ToolCall(display.clone()));
                ui_transcript.append(UiRecord::ToolCall { display });
                let _ = ui_transcript.save(&ui_transcript_path);
            }
            renderer.flush()?;

            drain_delegation_events_with_transcript(
                manager.as_deref(),
                &mut *renderer,
                &mut subagents,
                Some(&mut ui_transcript),
            );

            if name == crate::tool_names::TOOL_REBIRTH {
                renderer
                    .on_event(&Event::ToolCall(format_tool_call_display(&name, &args_val)));
                renderer.flush()?;
                let res = crate::harness::handle_rebirth(&mut ctx, &args_val);
                match res {
                    Ok(r) => {
                        renderer.on_event(&Event::ToolResult(r.content.clone()));
                        if !r.is_error {
                            ui_transcript.append(UiRecord::Status {
                                text: format!("Rebirth checkpoint applied: {}", r.content),
                            });
                            let _ = ui_transcript.save(&ui_transcript_path);
                            let _ = ctx.save_transcript(&transcript_path);
                            renderer.on_event(&Event::TokensIn(ctx.token_count()));
                            renderer.on_event(&Event::Status(
                                "rebirth checkpoint applied".to_string(),
                            ));
                            let current_plan = manager
                                .as_ref()
                                .map(|m| m.plan.clone())
                                .unwrap_or_default();
                            let pending = current_plan.pending_tasks();
                            let continuation_msg = if !pending.is_empty() {
                                let pending_str = pending.join(", ");
                                format!(
                                    "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. You have active pending tasks in your plan: [{pending_str}]. Do not call rebirth consecutively without making progress. Proceed immediately with delegating or executing these pending tasks.)"
                                )
                            } else {
                                "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. Do not call rebirth consecutively without making progress. Proceed immediately with delivering your final synthesis to the user.)".to_string()
                            };
                            ctx.append(Message::User {
                                content: continuation_msg,
                            });
                            let _ = ctx.save_transcript(&transcript_path);
                        } else {
                            ctx.append(Message::Tool {
                                tool_call_id: call.id.clone(),
                                content: format!("ERROR: {}", r.content),
                            });
                            let _ = ctx.save_transcript(&transcript_path);
                        }
                    }
                    Err(e) => {
                        renderer.on_event(&Event::ToolResult(format!("ERROR: {e}")));
                        ctx.append(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: format!("ERROR: {e}"),
                        });
                        let _ = ctx.save_transcript(&transcript_path);
                    }
                }
                renderer.flush()?;
                continue;
            }

            ctx.reset_consecutive_rebirths();

            if call.is_malformed() {
                let err_msg = format!(
                    "ERROR: Invalid or truncated arguments for tool '{name}': output was cut off or contained unterminated JSON. Please reissue the tool call with complete, valid JSON arguments."
                );
                renderer.on_event(&Event::ToolResult(err_msg.clone()));
                renderer.flush()?;
                ctx.append(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: err_msg,
                });
                let _ = ctx.save_transcript(&transcript_path);
                continue;
            }

            let intervention = monitor.observe_tool(&name, &args_val);
            if matches!(
                intervention,
                crate::harness::monitor::Intervention::Block
                    | crate::harness::monitor::Intervention::Cut
            ) {
                let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                    format!(
                        "ERROR: Tool repetition detected for '{}'. Do not repeat identical calls.",
                        call.function.name
                    )
                });
                tracing::warn!(
                    "Manager tool {} blocked by repetition detector",
                    call.function.name
                );
                renderer.on_event(&Event::ToolResult(format!("ERROR: {err_msg}")));
                renderer.flush()?;
                ctx.append(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: format!("ERROR: {err_msg}"),
                });
                let _ = ctx.save_transcript(&transcript_path);
                continue;
            }

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
                let mut had_events = false;
                while let Ok(msg) = status_rx.try_recv() {
                    renderer.on_event(&Event::Status(msg));
                    had_events = true;
                }
                while let Ok(ev) = event_rx.try_recv() {
                    renderer.on_event(&ev);
                    had_events = true;
                }
                if had_events {
                    let _ = renderer.flush();
                }
                drain_steer_arbitration_events_with_transcript(
                    &mut steer_arb_rx,
                    &mut *renderer,
                    &mut steer_queue,
                    &mut steer_abort_requested,
                    Some(&mut subagents),
                    Some(&mut ui_transcript),
                );
                drain_delegation_events_with_transcript(
                    manager.as_deref(),
                    &mut *renderer,
                    &mut subagents,
                    Some(&mut ui_transcript),
                );
                if renderer.aborted()
                    || steer_abort_requested
                    || crate::orchestrator::is_globally_cancelled()
                {
                    crate::orchestrator::cancel_all();
                    let _ =
                        tokio::time::timeout(Duration::from_millis(500), &mut handle).await;
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
                        let mut had_events = false;
                        while let Ok(msg) = status_rx.try_recv() {
                            renderer.on_event(&Event::Status(msg));
                            had_events = true;
                        }
                        while let Ok(ev) = event_rx.try_recv() {
                            renderer.on_event(&ev);
                            had_events = true;
                        }
                        if had_events {
                            let _ = renderer.flush();
                        }
                        drain_steer_arbitration_events_with_transcript(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                            &mut steer_abort_requested,
                            Some(&mut subagents),
                            Some(&mut ui_transcript),
                        );
                        drain_delegation_events_with_transcript(
                            manager.as_deref(),
                            &mut *renderer,
                            &mut subagents,
                            Some(&mut ui_transcript),
                        );
                        if let Some(input) = renderer.poll_input() {
                            if is_abort_command(&input) {
                                crate::debug_log::log_user_input("command", &input);
                                renderer.request_user_exit();
                            } else if is_reset_command(&input) {
                                handle_reset_command(&plan, &mut *renderer, Some(&mut ctx));
                                ui_transcript.clear();
                                if let Ok(mut hist) = steering_history.write() {
                                    hist.clear();
                                }
                                let _ = std::fs::remove_file(&ui_transcript_path);
                            } else if !input.trim().is_empty() {
                                spawn_steer_arbitration(
                                    &client,
                                    harness_stats.clone(),
                                    &goal,
                                    &subagents,
                                    input,
                                    &steer_arb_tx,
                                    &mut *renderer,
                                    Some(Arc::clone(&steering_history)),
                                );
                            }
                        }
                    }
                }
            };

            let (result_content, is_error) = match result {
                Ok(res) => (res.content, res.is_error),
                Err(e) => (format!("ERROR: {e}"), true),
            };
            if let Some(agent) = delegated_agent {
                update_subagent_lifecycle(
                    &mut subagents,
                    agent,
                    delegated_task.clone(),
                    None,
                    false,
                );
                let tid = delegated_task.clone().unwrap_or_else(|| agent.to_string());
                if is_error {
                    let reason = crate::ui::helpers::extract_failure_reason(&result_content);
                    renderer.on_event(&Event::Delegation(
                        crate::orchestrator::DelegationEvent::Failed {
                            agent,
                            task: delegated_task,
                            reason: Some(reason.clone()),
                        },
                    ));
                    ui_transcript.append(UiRecord::TaskFailed {
                        task_id: tid,
                        reason: Some(reason),
                    });
                } else {
                    if let Some(ref tid) = delegated_task {
                        let plan = crate::manager::phase::Plan::default();
                        let _ = plan.check_off(tid);
                    }
                    renderer.on_event(&Event::Delegation(
                        crate::orchestrator::DelegationEvent::Completed {
                            agent,
                            task: delegated_task,
                        },
                    ));
                    ui_transcript.append(UiRecord::TaskCompleted { task_id: tid });
                }
                let _ = ui_transcript.save(&ui_transcript_path);
                renderer.set_subagents(subagents.clone());
            } else {
                renderer.on_event(&Event::ToolResult(result_content.clone()));
                ui_transcript.append(UiRecord::ToolResult {
                    display: result_content.clone(),
                });
                let _ = ui_transcript.save(&ui_transcript_path);
            }
            renderer.flush()?;

            drain_delegation_events_with_transcript(
                manager.as_deref(),
                &mut *renderer,
                &mut subagents,
                Some(&mut ui_transcript),
            );

            ctx.append(Message::Tool {
                tool_call_id: call.id.clone(),
                content: result_content,
            });
            let _ = ctx.save_transcript(&transcript_path);
        }
    }
