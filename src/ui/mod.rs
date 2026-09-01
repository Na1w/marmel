//! User interface: renderer abstraction, session event loop, TUI and raw modes.

pub mod raw;
pub mod tui;

use crate::agent::context::ContextEngine;
use crate::config::Config;
use crate::llm::{ChatClient, StreamConfig, StreamEvent, StreamSink, chat_client_turn};
use crate::orchestrator::{DelegationEvent, OrchestratorManager};
use crate::types::Message;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

/// A specialist subagent shown in the bottom-right panel.
#[derive(Debug, Clone, Default)]
pub struct SubagentDetail {
    /// The specialist role id (e.g. `coder`, `researcher`).
    pub name: String,
    pub task_id: Option<String>,
    pub prompt: String,
    pub started_at: Option<std::time::Instant>,
    /// Instant of the most recent activity (streaming chunk, log, status, or lifecycle).
    pub last_activity_at: Option<std::time::Instant>,
    /// Ordered log lines for this subagent (status / tool activity).
    pub logs: Vec<String>,
    /// Streaming "thinking" block for this subagent.
    pub thinking: String,
    /// Streaming final-answer content for this subagent.
    pub content: String,
    /// Whether this subagent is currently running (between Started/Completed).
    pub is_active: bool,
    /// The context tokens used by this subagent.
    pub context_tokens: usize,
}

/// A single agent event dispatched to the active renderer.
#[derive(Debug, Clone)]
pub enum Event {
    /// A chunk of visible assistant content (streamed as it arrives).
    Message(String),
    /// A steer response chunk or completed message.
    SteerResponse(String),
    /// A chunk of reasoning / thinking-channel content.
    Thinking(String),
    /// A tool invocation (rendered as `name(arguments)`).
    ToolCall(String),
    /// The textual result of a tool execution.
    ToolResult(String),
    /// A status / phase update (e.g. "calling backend…", "aborting…").
    Status(String),
    /// A delegation lifecycle event.
    Delegation(DelegationEvent),
    /// The session has finished.
    Done,
    /// Estimated input/prompt tokens added.
    TokensIn(usize),
    /// Estimated output/completion tokens added.
    TokensOut(usize),
}

/// Abstraction over the interactive TUI and the headless raw mode.
pub trait Renderer: Send {
    fn init(&mut self) -> Result<()>;
    fn on_event(&mut self, event: &Event);
    fn flush(&mut self) -> Result<()>;
    fn poll_input(&mut self) -> Option<String>;
    fn read_input(&mut self) -> Option<String>;
    fn request_abort(&mut self);
    fn aborted(&self) -> bool;
    fn clear_abort(&mut self) {}
    fn shutdown(&mut self);
    fn set_subagents(&mut self, _subagents: Vec<SubagentDetail>) {}
}

/// Drive a full interactive session, dispatching events to `renderer`.
pub async fn run_session(
    cfg: &Config,
    renderer: &mut dyn Renderer,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
) -> Result<()> {
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
    let mut keep_going = true;
    let stream_cfg = StreamConfig::from_config(cfg);
    let mut steer_queue = Vec::<String>::new();
    let mut subagents = Vec::<SubagentDetail>::new();
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (steer_arb_tx, mut steer_arb_rx) = tokio::sync::mpsc::unbounded_channel::<SteerArbEvent>();
    crate::orchestrator::set_status_sender(status_tx);

    while keep_going && !renderer.aborted() {
        while let Ok(msg) = status_rx.try_recv() {
            renderer.on_event(&Event::Status(msg));
        }

        drain_steer_arbitration_events(&mut steer_arb_rx, &mut *renderer, &mut steer_queue);
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
            drain_steer_arbitration_events(&mut steer_arb_rx, &mut *renderer, &mut steer_queue);
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
                arb_tx: &steer_arb_tx,
                arb_rx: &mut steer_arb_rx,
                client: &client,
                stats: harness_stats.clone(),
                goal: &goal,
                subagents: &subagents,
            };
            let assistant =
                match chat_client_turn(&client, ctx.messages().to_vec(), &stream_cfg, &mut bridge)
                    .await
                {
                    Ok(m) => m,
                    Err(e) => {
                        let category = classify_llm_error(&e);
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            category = %category,
                            "LLM turn failed -> terminate session"
                        );
                        renderer.on_event(&Event::Status(format!("LLM error ({category}): {e:#}")));
                        keep_going = false;
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
                if !pending.is_empty() && nudge_count < 5 {
                    nudge_count += 1;
                    let pending_str = pending.join(", ");
                    renderer.on_event(&Event::Status(format!(
                        "Auto-nudge ({nudge_count}/5): Plan incomplete (pending: {pending_str})"
                    )));
                    renderer.flush()?;
                    ctx.append(Message::User {
                        content: format!(
                            "(SYSTEM NOTICE: The execution plan is NOT complete. Remaining tasks: [{pending_str}]. You must continue issuing tool calls to fulfill the remaining tasks. Do NOT stop or summarize to the user until all tasks are complete.)"
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
                        renderer.on_event(&Event::ToolCall(format!("{}({})", name, args_str)));
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
                        drain_steer_arbitration_events(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                        );
                        drain_delegation_events(manager.as_deref(), &mut *renderer, &mut subagents);
                        if renderer.aborted() {
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
                                drain_steer_arbitration_events(
                                    &mut steer_arb_rx,
                                    &mut *renderer,
                                    &mut steer_queue,
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
                        renderer.on_event(&Event::ToolCall(format!("{}({})", name, args_str)));
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
                        drain_steer_arbitration_events(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                        );
                        drain_delegation_events(manager.as_deref(), &mut *renderer, &mut subagents);
                        if renderer.aborted() {
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
                                drain_steer_arbitration_events(
                                    &mut steer_arb_rx,
                                    &mut *renderer,
                                    &mut steer_queue,
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

        drain_steer_arbitration_events(&mut steer_arb_rx, &mut *renderer, &mut steer_queue);
        if renderer.aborted() {
            if !steer_queue.is_empty() {
                // Steer arbitrator requested AbortImmediately -> clear abort flag and start next turn immediately
                renderer.clear_abort();
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

        if !keep_going {
            break;
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

pub fn format_active_subtasks(subagents: &[SubagentDetail]) -> String {
    let global_active = crate::orchestrator::get_active_subtasks_str();
    if global_active != "None" && !global_active.trim().is_empty() {
        return global_active;
    }
    let active: Vec<_> = subagents.iter().filter(|s| s.is_active).collect();
    if active.is_empty() {
        return "None".to_string();
    }
    let mut out = String::new();
    for s in active {
        let elapsed_secs = s.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let task_id_str = s.task_id.as_deref().unwrap_or(&s.name);
        let prompt_str = if s.prompt.is_empty() {
            "None"
        } else {
            &s.prompt
        };
        out.push_str(&format!(
            "- Tool Call ID: {}\n  Subagent: {}\n  Task Prompt: {}\n  Running For: {} seconds\n\n",
            task_id_str, s.name, prompt_str, elapsed_secs
        ));
    }
    out
}

pub fn format_plan_progress_summary(plan_content: &str) -> String {
    crate::orchestrator::generate_plan_progress_summary(plan_content)
}

enum SteerArbEvent {
    Delta(String),
    Finished {
        decision: Option<crate::orchestrator::SteerDecision>,
        user_msg: String,
    },
}

fn spawn_steer_arbitration(
    client: &ChatClient,
    stats: Arc<crate::harness::HarnessStats>,
    goal: &str,
    subagents: &[SubagentDetail],
    user_msg: String,
    arb_tx: &tokio::sync::mpsc::UnboundedSender<SteerArbEvent>,
    renderer: &mut dyn Renderer,
) {
    let client = client.clone();
    let goal = goal.to_string();
    let plan_content = crate::agent::phase::Plan::default()
        .read()
        .unwrap_or(None)
        .unwrap_or_default();
    let active_subtasks_str = format_active_subtasks(subagents);
    let plan_progress_str = format_plan_progress_summary(&plan_content);
    let tx = arb_tx.clone();
    let msg = user_msg.clone();

    renderer.on_event(&Event::Status(
        "Arbitrating user steering instruction...".to_string(),
    ));
    let _ = renderer.flush();

    tokio::spawn(async move {
        let delta_tx = tx.clone();
        let ctx = crate::orchestrator::steer::SteerContext {
            main_goal: &goal,
            orchestrator_status: if active_subtasks_str == "None" {
                "Active (planning/turn)"
            } else {
                "Active (subagents executing)"
            },
            pending_approval: "None",
            plan_progress: &plan_progress_str,
            plan_content: &plan_content,
            available_agents: "",
            steering_history: "None",
            user_message: &msg,
            active_subtasks: &active_subtasks_str,
        };
        let decision = crate::orchestrator::steer::arbitrate_steer_context_stream(
            &client,
            &stats,
            ctx,
            move |delta| {
                let _ = delta_tx.send(SteerArbEvent::Delta(delta.to_string()));
            },
        )
        .await;

        let _ = tx.send(SteerArbEvent::Finished {
            decision,
            user_msg: msg,
        });
    });
}

fn drain_steer_arbitration_events(
    arb_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SteerArbEvent>,
    renderer: &mut dyn Renderer,
    steer_queue: &mut Vec<String>,
) {
    while let Ok(ev) = arb_rx.try_recv() {
        match ev {
            SteerArbEvent::Delta(delta) => {
                renderer.on_event(&Event::SteerResponse(delta));
                let _ = renderer.flush();
            }
            SteerArbEvent::Finished { decision, user_msg } => {
                match decision.as_ref().map(|d| d.decision.as_str()) {
                    Some("RespondDirectly") => {
                        renderer.on_event(&Event::Status(
                            "Answered via direct steer response".to_string(),
                        ));
                        let _ = renderer.flush();
                    }
                    Some("AbortImmediately") => {
                        renderer.request_abort();
                        steer_queue.push(user_msg);
                    }
                    Some("ForwardToWorker") => {
                        steer_queue.push(user_msg);
                        renderer
                            .on_event(&Event::Status("Notice forwarded to specialist".to_string()));
                        let _ = renderer.flush();
                    }
                    Some("ApprovePlan") => {
                        steer_queue.push("User approved plan.".to_string());
                    }
                    Some("RejectPlan") => {
                        renderer.request_abort();
                        steer_queue.push(format!("User rejected plan: {user_msg}"));
                    }
                    _ => {
                        steer_queue.push(user_msg);
                        renderer.on_event(&Event::Status(
                            "Instruction queued for next turn".to_string(),
                        ));
                        let _ = renderer.flush();
                    }
                }
            }
        }
    }
}

struct RendererSink<'a> {
    renderer: &'a mut dyn Renderer,
    steer_queue: &'a mut Vec<String>,
    arb_tx: &'a tokio::sync::mpsc::UnboundedSender<SteerArbEvent>,
    arb_rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<SteerArbEvent>,
    client: &'a ChatClient,
    stats: Arc<crate::harness::HarnessStats>,
    goal: &'a str,
    subagents: &'a [SubagentDetail],
}

impl StreamSink for RendererSink<'_> {
    fn emit(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Content(text) => self.renderer.on_event(&Event::Message(text)),
            StreamEvent::Thinking(text) => self.renderer.on_event(&Event::Thinking(text)),
            StreamEvent::Status(text) => self.renderer.on_event(&Event::Status(text)),
        }
        let _ = self.renderer.flush();
    }

    fn is_aborted(&mut self) -> bool {
        let _ = self.renderer.flush();
        drain_steer_arbitration_events(self.arb_rx, self.renderer, self.steer_queue);
        if let Some(input) = self.renderer.poll_input() {
            if is_abort_command(&input) {
                self.renderer.request_abort();
            } else if is_reset_command(&input) {
                let plan = crate::agent::phase::Plan::default();
                let _ = plan.clear();
                self.renderer.on_event(&Event::Message(
                    "Execution plan has been cleared and reset by user.".to_string(),
                ));
                self.renderer
                    .on_event(&Event::Status("Execution plan reset".to_string()));
                let _ = self.renderer.flush();
            } else if !input.trim().is_empty() {
                spawn_steer_arbitration(
                    self.client,
                    self.stats.clone(),
                    self.goal,
                    self.subagents,
                    input,
                    self.arb_tx,
                    self.renderer,
                );
            }
        }
        self.renderer.aborted()
    }
}

fn load_system_prompt(_cfg: &Config) -> Result<String> {
    let content = include_str!("../../prompts/system.md");
    let env_block = crate::prompts::format_environment_block();
    let mut prompt = format!("{content}\n\n{env_block}\n");
    let plan = crate::agent::phase::Plan::default();
    if let Ok(Some(plan_content)) = plan.read()
        && !plan_content.trim().is_empty()
    {
        prompt.push_str(&format!(
            "\n## Active Execution Plan (`.marmel/execution_plan.md`)\nThere is an existing execution plan already active on disk:\n```markdown\n{}\n```\nDo NOT call `create_plan` unless you explicitly intend to overwrite the plan. Proceed directly with `delegate_task` to execute any remaining unchecked `- [ ] [t-xxx]` tasks.\n",
            plan_content.trim()
        ));
    }
    Ok(prompt)
}

pub fn chunk_utf8(s: &str, max: usize) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let bytes = s.as_bytes();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut end = (start + max).min(bytes.len());
        while end > start && !s.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = start + s[start..].chars().next().map_or(1, |c| c.len_utf8());
        }
        chunks.push(&s[start..end]);
        start = end;
    }
    chunks
}

fn is_abort_command(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("/abort")
        || t.eq_ignore_ascii_case("/exit")
        || t.eq_ignore_ascii_case("/quit")
        || t.eq_ignore_ascii_case("/q")
        || t.eq_ignore_ascii_case(":q")
        || t.eq_ignore_ascii_case(":q!")
}

fn is_reset_command(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("/reset")
        || t.eq_ignore_ascii_case("/reset_plan")
        || t.eq_ignore_ascii_case("/reset-plan")
        || t.eq_ignore_ascii_case("/clear_plan")
        || t.eq_ignore_ascii_case("/clear-plan")
        || t.eq_ignore_ascii_case("/reset_execution_plan")
}

fn handle_reset_command(
    plan: &crate::agent::phase::Plan,
    renderer: &mut dyn Renderer,
    ctx: &mut ContextEngine,
) {
    let _ = plan.clear();
    renderer.on_event(&Event::Message(
        "Execution plan has been cleared and reset by user.".to_string(),
    ));
    renderer.on_event(&Event::Status("Execution plan reset".to_string()));
    let _ = renderer.flush();
    ctx.append(Message::User {
        content: "[System] User executed /reset. The execution plan has been removed from disk. Return to Conversational phase."
            .to_string(),
    });
}

fn classify_llm_error(e: &anyhow::Error) -> &'static str {
    let msg = format!("{e:#}").to_lowercase();
    if msg.contains("http") || msg.contains("status") || msg.contains("503") || msg.contains("429")
    {
        "http"
    } else if msg.contains("transport") || msg.contains("connection") {
        "connectivity"
    } else if msg.contains("timeout") {
        "timeout"
    } else if msg.contains("stream") || msg.contains("sse") {
        "stream"
    } else {
        "unknown"
    }
}

fn update_subagent_lifecycle(
    subagents: &mut Vec<SubagentDetail>,
    agent: crate::agents::Agent,
    task: Option<String>,
    prompt: Option<String>,
    started: bool,
) {
    let name = match &task {
        Some(t) if !t.trim().is_empty() => format!("{}-{t}", agent.as_str()),
        _ => agent.as_str().to_string(),
    };
    let task_str = task.clone().unwrap_or_default();
    let log_entry = if started {
        format!("started task {task_str}")
    } else {
        format!("completed task {task_str}")
    };
    let active_tokens = crate::orchestrator::get_active_worker_tokens(&name).unwrap_or(0);
    let now = std::time::Instant::now();
    if let Some(existing) = subagents.iter_mut().find(|s| s.name == name) {
        existing.is_active = started;
        existing.last_activity_at = Some(now);
        if active_tokens > 0 {
            existing.context_tokens = active_tokens;
        }
        if started {
            existing.task_id = task;
            if let Some(p) = prompt {
                existing.prompt = p;
            }
            existing.started_at = Some(now);
        } else {
            existing.started_at = None;
        }
        existing.logs.push(log_entry);
    } else {
        subagents.push(SubagentDetail {
            name,
            task_id: task,
            prompt: prompt.unwrap_or_default(),
            started_at: if started { Some(now) } else { None },
            last_activity_at: Some(now),
            logs: vec![log_entry],
            thinking: String::new(),
            content: String::new(),
            is_active: started,
            context_tokens: active_tokens,
        });
    }
}

fn drain_delegation_events(
    manager: Option<&OrchestratorManager>,
    renderer: &mut dyn Renderer,
    subagents: &mut Vec<SubagentDetail>,
) {
    let Some(manager) = manager else {
        return;
    };
    let Ok(mut events) = manager.delegation_events.lock() else {
        return;
    };
    let mut changed = false;
    for event in events.drain(..) {
        match &event {
            DelegationEvent::Started { agent, task } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, true);
                changed = true;
            }
            DelegationEvent::Completed { agent, task } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, false);
                changed = true;
            }
        }
        renderer.on_event(&Event::Delegation(event));
    }
    if changed {
        renderer.set_subagents(subagents.clone());
    }
}

pub fn restore() {
    let _ = tui::leave_alt_screen();
    let _ = raw::restore();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::raw::RawRenderer;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptedRenderer {
        read_script: Vec<String>,
        read_cursor: usize,
        aborted: bool,
    }

    impl ScriptedRenderer {
        fn new(read_script: Vec<String>) -> Self {
            Self {
                read_script,
                read_cursor: 0,
                aborted: false,
            }
        }
    }

    impl Renderer for ScriptedRenderer {
        fn init(&mut self) -> Result<()> {
            Ok(())
        }
        fn on_event(&mut self, _event: &Event) {}
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
        fn poll_input(&mut self) -> Option<String> {
            None
        }
        fn read_input(&mut self) -> Option<String> {
            let line = self.read_script.get(self.read_cursor).cloned();
            self.read_cursor += 1;
            line
        }
        fn request_abort(&mut self) {
            self.aborted = true;
        }
        fn aborted(&self) -> bool {
            self.aborted
        }
        fn shutdown(&mut self) {}
    }

    fn config_for_backend(backend: &str) -> Config {
        Config {
            backend_url: format!("{backend}/v1"),
            system_prompt_path: std::path::PathBuf::from("prompts/system.md"),
            ui_mode: "tui".to_string(),
            ..Config::default()
        }
    }

    fn completion_sse(text: &str) -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": { "content": text },
                    "finish_reason": null
                }]
            })
        )
    }

    #[tokio::test]
    async fn test_ui_run_session_continues_after_first_turn() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let calls = Arc::new(AtomicUsize::new(0));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with({
                let calls = calls.clone();
                move |_req: &wiremock::Request| {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let body = if n == 0 {
                        completion_sse("first reply")
                    } else {
                        completion_sse("second reply")
                    };
                    ResponseTemplate::new(200).set_body_string(body)
                }
            })
            .mount(&server)
            .await;

        let cfg = config_for_backend(&server.uri());

        let mut renderer = ScriptedRenderer::new(vec![
            "goal".to_string(),
            "steer2".to_string(),
            "/abort".to_string(),
        ]);

        run_session(&cfg, &mut renderer, None, None)
            .await
            .expect("run_session should complete without error");

        let backend_calls = calls.load(Ordering::SeqCst);
        assert_eq!(backend_calls, 2);
        assert!(renderer.aborted());
    }

    #[tokio::test]
    async fn test_ui_run_session_raw_single_turn() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let calls = Arc::new(AtomicUsize::new(0));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with({
                let calls = calls.clone();
                move |_req: &wiremock::Request| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_string(completion_sse("single reply"))
                }
            })
            .mount(&server)
            .await;

        let cfg = config_for_backend(&server.uri());

        let mut renderer = RawRenderer::new();
        run_session(&cfg, &mut renderer, Some("goal".to_string()), None)
            .await
            .expect("run_session should complete without error");

        let backend_calls = calls.load(Ordering::SeqCst);
        assert_eq!(backend_calls, 1);
    }

    fn tool_call_sse(id: &str, name: &str, arguments: &str) -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": {
                        "content": null,
                        "tool_calls": [{
                            "index": 0,
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            })
        )
    }

    #[tokio::test]
    async fn test_ui_run_session_executes_tool_calls() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let calls = Arc::new(AtomicUsize::new(0));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with({
                let calls = calls.clone();
                move |req: &wiremock::Request| {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        ResponseTemplate::new(200).set_body_string(tool_call_sse(
                            "call-glob-1",
                            "glob",
                            r#"{"pattern": "Cargo.toml"}"#,
                        ))
                    } else {
                        let body_str = String::from_utf8_lossy(&req.body);
                        assert!(
                            body_str.contains("Cargo.toml") || body_str.contains("call-glob-1"),
                            "second LLM request must contain tool result payload"
                        );
                        ResponseTemplate::new(200)
                            .set_body_string(completion_sse("Analysis complete."))
                    }
                }
            })
            .mount(&server)
            .await;

        let cfg = config_for_backend(&server.uri());

        let mut renderer = RawRenderer::new();
        run_session(
            &cfg,
            &mut renderer,
            Some("analysera projektet".to_string()),
            None,
        )
        .await
        .expect("run_session should complete");

        let backend_calls = calls.load(Ordering::SeqCst);
        assert_eq!(backend_calls, 2);
    }

    struct RecordingRenderer {
        subagents: Vec<SubagentDetail>,
        delegation_events: Vec<DelegationEvent>,
        events: Vec<Event>,
    }

    impl RecordingRenderer {
        fn new() -> Self {
            Self {
                subagents: Vec::new(),
                delegation_events: Vec::new(),
                events: Vec::new(),
            }
        }
    }

    impl Renderer for RecordingRenderer {
        fn init(&mut self) -> Result<()> {
            Ok(())
        }
        fn on_event(&mut self, event: &Event) {
            self.events.push(event.clone());
            if let Event::Delegation(de) = event {
                self.delegation_events.push(de.clone());
            }
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
        fn poll_input(&mut self) -> Option<String> {
            None
        }
        fn read_input(&mut self) -> Option<String> {
            None
        }
        fn request_abort(&mut self) {}
        fn aborted(&self) -> bool {
            false
        }
        fn shutdown(&mut self) {}
        fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
            self.subagents = subagents;
        }
    }

    fn test_manager(dir: &tempfile::TempDir) -> OrchestratorManager {
        OrchestratorManager::new(
            crate::llm::ChatClient::new("http://localhost:9999/v1", "test-model"),
            crate::agent::phase::Plan::at(dir.path()),
            Arc::new(crate::harness::HarnessStats::new()),
        )
    }

    #[test]
    fn drain_delegation_events_folds_lifecycle_into_subagent_list() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = test_manager(&tmp);
        let mut renderer = RecordingRenderer::new();
        let mut subagents = Vec::<SubagentDetail>::new();

        {
            let mut ev = manager.delegation_events.lock().unwrap();
            ev.push(DelegationEvent::Started {
                agent: crate::agents::Agent::Coder,
                task: Some("t-1".to_string()),
            });
            ev.push(DelegationEvent::Completed {
                agent: crate::agents::Agent::Coder,
                task: Some("t-1".to_string()),
            });
        }

        drain_delegation_events(Some(&manager), &mut renderer, &mut subagents);

        assert_eq!(renderer.delegation_events.len(), 2);
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].name, "coder-t-1");
        assert!(!subagents[0].is_active);
    }

    #[test]
    fn test_is_reset_command_matches_aliases() {
        assert!(is_reset_command("/reset"));
        assert!(is_reset_command("  /reset  "));
        assert!(is_reset_command("/RESET"));
        assert!(is_reset_command("/reset_plan"));
        assert!(is_reset_command("/reset-plan"));
        assert!(is_reset_command("/clear_plan"));
        assert!(is_reset_command("/clear-plan"));
        assert!(is_reset_command("/reset_execution_plan"));
        assert!(!is_reset_command("/q"));
        assert!(!is_reset_command("reset"));
        assert!(!is_reset_command("hello world"));
    }

    #[test]
    fn test_handle_reset_command_clears_plan_and_notifies() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let plan = crate::agent::phase::Plan::at(tmp.path());
        plan.create("# Plan\n- [ ] [t-1] test\n").unwrap();
        assert!(plan.exists());

        let mut renderer = RecordingRenderer::new();
        let mut ctx = ContextEngine::new(4096);
        ctx.set_system_prompt("sys".to_string());
        ctx.set_goal("goal".to_string());

        handle_reset_command(&plan, &mut renderer, &mut ctx);

        assert!(!plan.exists());
        assert!(
            renderer
                .events
                .iter()
                .any(|ev| matches!(ev, Event::Message(m) if m.contains("cleared and reset")))
        );
        assert!(
            renderer
                .events
                .iter()
                .any(|ev| matches!(ev, Event::Status(s) if s.contains("reset")))
        );
    }
}
