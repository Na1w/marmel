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
                    // Strict abort semantics (fix_loop module docs): an aborted
                    // validation now surfaces as `Err` (never the old
                    // `Ok((false, …))` soft rejection). To preserve the
                    // pre-consolidation observable output, the abort is
                    // reported through the same auditor-rejection message.
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
            ToolResult::ok(format!(
                "Execution plan written to .marmel/execution_plan.md.\nRecognized pending tasks: [{pending_str}].\nPhase is now EXECUTING. You must proceed immediately to emit `delegate_task` tool calls for the first pending task(s). Do NOT call create_plan again unless you explicitly intend to overwrite the plan."
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
                Ok(ToolResult::err(
                    "plan is not complete and cannot be archived yet",
                ))
            } else {
                Ok(ToolResult::ok("no plan file to archive"))
            }
        }
        Err(e) => Err(ToolError::Execution(e)),
    }
}
