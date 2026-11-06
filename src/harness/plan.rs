//! Execution-plan tool handlers (`create_plan` / plan archiving).

use super::block_on_safe;
use super::common::{ToolError, ToolResult};

pub(crate) fn write_plan(md: &str) -> Result<ToolResult, ToolError> {
    block_on_safe(write_plan_async(md))
}

pub(crate) async fn write_plan_async(md: &str) -> Result<ToolResult, ToolError> {
    write_plan_internal(md, None).await
}

pub(crate) async fn write_plan_internal(
    md: &str,
    custom_plan: Option<crate::manager::phase::Plan>,
) -> Result<ToolResult, ToolError> {
    let cfg = crate::config::get_active()
        .or_else(|| crate::config::load(None).ok())
        .unwrap_or_default();
    let token = if custom_plan.is_some() {
        tokio_util::sync::CancellationToken::new()
    } else {
        crate::orchestrator::global_cancellation_token()
    };

    let planner_cfg = cfg.orchestration.specialists.get("planner");
    let validator_cfg = cfg
        .orchestration
        .specialists
        .get(crate::agents::Agent::Validator.as_str());
    let auto_validate_enabled = planner_cfg
        .and_then(|sc| sc.enable_validator)
        .or_else(|| validator_cfg.and_then(|vc| vc.enable_validator))
        .unwrap_or(true);

    if auto_validate_enabled {
        let has_execution_plan_header = md
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case("# Execution Plan"));
        if !has_execution_plan_header {
            let critique = "Missing required `# Execution Plan` top-level heading. Start the plan with `# Execution Plan`.";
            tracing::warn!("create_plan rejected by deterministic pre-check: {critique}");
            return Ok(ToolResult::err(format!(
                "Execution plan rejected by Strategic Plan Auditor:\n{critique}\n\nPlease revise the execution plan addressing the auditor's critique and call create_plan again."
            )));
        }
        if crate::plan_parse::all_task_ids(md).is_empty() {
            let critique =
                "Plan contains no recognizable task items (`- [ ] [t-xxx] Description (role)`).";
            tracing::warn!("create_plan rejected by deterministic pre-check: {critique}");
            return Ok(ToolResult::err(format!(
                "Execution plan rejected by Strategic Plan Auditor:\n{critique}\n\nPlease revise the execution plan addressing the auditor's critique and call create_plan again."
            )));
        }
        match crate::agents::validation::run_plan_validation(md, &cfg, &token).await {
            Ok((approved, critique)) => {
                if !approved {
                    tracing::warn!("create_plan rejected by Strategic Plan Auditor: {critique}");
                    if token.is_cancelled() {
                        return Ok(ToolResult::err(format!(
                            "Execution plan rejected by Strategic Plan Auditor:\n{critique}"
                        )));
                    }
                    return Ok(ToolResult::err(format!(
                        "Execution plan rejected by Strategic Plan Auditor:\n{critique}\n\nPlease revise the execution plan addressing the auditor's critique and call create_plan again."
                    )));
                }
                tracing::info!("create_plan approved by Strategic Plan Auditor: {critique}");
            }
            Err(e) => {
                let msg = e.to_string();
                if token.is_cancelled() || msg.contains("aborted by cancellation") {
                    // Strict abort semantics: an aborted validation surfaces as
                    // `Err`, never as an `Ok((false, …))` soft rejection. The abort
                    // is reported through the same auditor-rejection message.
                    tracing::warn!("create_plan aborted during plan validation: {msg}");
                    return Ok(ToolResult::err(format!(
                        "Execution plan rejected by Strategic Plan Auditor:\n{msg}"
                    )));
                }
                tracing::warn!("Plan validation skipped due to error: {e:#}");
            }
        }
    }

    let plan = custom_plan.unwrap_or_default();
    plan.create(md)
        .map(|_| {
            let prompts_dir = plan.dir().join("prompts");
            let ws_root = plan.dir().parent().unwrap_or_else(|| plan.dir());
            let catalog = crate::agents::Catalog::discover(ws_root);
            crate::agents::PromptBuilder::pregenerate_for_plan_offline(md, &catalog, &prompts_dir);

            let pending = plan.pending_tasks();
            let pending_str = if pending.is_empty() {
                "none".to_string()
            } else {
                pending.join(", ")
            };
            let dispatch_hint = if crate::config::resolve_max_sessions(
                &cfg.orchestration.concurrency,
                &cfg.backend_url,
            ) == Some(1)
            {
                "Phase is now EXECUTING. Because `max_sessions = 1` on the active backend, you MUST schedule tasks sequentially: emit ONLY ONE `delegate_task` tool call per turn (starting with the first pending task) and wait for it to finish before delegating the next task. Do NOT call create_plan again unless you explicitly intend to overwrite the plan."
            } else {
                "Phase is now EXECUTING. You must proceed immediately to emit `delegate_task` tool calls for the first pending task(s). Do NOT call create_plan again unless you explicitly intend to overwrite the plan."
            };
            ToolResult::ok(format!(
                "Execution plan written to .marmel/execution_plan.md.\nRecognized pending tasks: [{pending_str}].\n{dispatch_hint}"
            ))
        })
        .map_err(ToolError::Execution)
}

pub(crate) fn archive_plan() -> Result<ToolResult, ToolError> {
    if crate::orchestrator::has_active_workers() {
        return Ok(ToolResult::err(
            "Cannot archive plan while background specialist workers are still actively running.",
        ));
    }
    let plan = crate::manager::phase::Plan::default();
    match plan.archive() {
        Ok(Some(dest)) => Ok(ToolResult::ok(format!(
            "plan archived to {}",
            dest.display()
        ))),
        Ok(None) => {
            if plan.plan_path().exists() {
                // Actionable refusal: name what is still open, instead of the
                // same opaque sentence for every plan state.
                let pending = plan.pending_tasks();
                if pending.is_empty() {
                    return Ok(ToolResult::err(
                        "plan is not complete and cannot be archived yet: no task is checked off \
                         `[x]`, so nothing in this plan has been completed",
                    ));
                }
                Ok(ToolResult::err(format!(
                    "plan is not complete and cannot be archived yet — {} task(s) still pending: {}",
                    pending.len(),
                    pending.join(", ")
                )))
            } else {
                Ok(ToolResult::ok("no plan file to archive"))
            }
        }
        Err(e) => Err(ToolError::Execution(e)),
    }
}

pub(crate) fn handle_pause_execution(_args: &serde_json::Value) -> Result<ToolResult, ToolError> {
    // Keep the legacy name reserved so stale model calls cannot reach an MCP
    // replacement or cancel workers. Pausing is controlled by user input only.
    Err(ToolError::Forbidden {
        tool: crate::tool_names::TOOL_PAUSE_EXECUTION.to_string(),
        caller: "model (execution control belongs to the user)".to_string(),
    })
}

pub(crate) fn control_subagent(
    args: &serde_json::Value,
    paused: bool,
) -> Result<ToolResult, ToolError> {
    let tool = if paused {
        crate::tool_names::TOOL_PAUSE_SUBAGENT
    } else {
        crate::tool_names::TOOL_RESUME_SUBAGENT
    };
    let key = args
        .get("worker_key")
        .and_then(serde_json::Value::as_str)
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| ToolError::BadArguments {
            tool: tool.to_string(),
            detail: "worker_key must name exactly one live subagent registry key".to_string(),
        })?;
    match crate::orchestrator::worker_pause::set_paused(key, paused) {
        Ok(()) => Ok(ToolResult::ok(if paused {
            format!(
                "Pause requested for {key}; current tools may finish first. Task and context are preserved. Call resume_subagent to continue it."
            )
        } else {
            format!("Resume requested for {key}; the same task continues.")
        })),
        Err(message) => Ok(ToolResult::err(message)),
    }
}
