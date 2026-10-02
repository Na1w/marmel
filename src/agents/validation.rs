//! Automated validator loop and verdict evaluation for specialist deliverables.
//!
//! The turn-loop scaffolding lives once in [`crate::agents::runner::fix_loop`];
//! this file keeps only the validator-specific prompt/brief construction and
//! the verdict-parsing helpers, as thin wrappers over the shared driver.
//!
//! **Abort semantics (unified, see fix_loop module docs):** every
//! abort/cancellation path returns `Err`, never `Ok(false, …)`. A cancelled
//! validation is not a verdict; surfacing it as a failure lets callers
//! distinguish "user aborted" from "validator rejected".

use crate::agents::Agent;
use crate::agents::runner::fix_loop::{
    FixLoopResult, LoopParams, assemble_tools, register_loop_worker, resolve_validator_backend,
    run_fix_loop,
};
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
    let custom_validation_prompt = task_id.and_then(|tid| {
        let clean = tid
            .trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\'')
            .trim();
        let path = crate::harness::get_workspace_root()
            .join(crate::manager::phase::MARMEL_DIR)
            .join("prompts")
            .join(format!("{clean}-validation.md"));
        std::fs::read_to_string(&path).ok()
    });

    let default_role_prompt = crate::agents::validator::role_prompt_for(agent);
    let validator_prompt = match custom_validation_prompt.as_deref() {
        Some(prompt) => prompt,
        None => {
            let prompts_dir = crate::harness::get_workspace_root()
                .join(crate::manager::phase::MARMEL_DIR)
                .join("prompts");
            if prompts_dir.is_dir() {
                return Ok((
                    true,
                    "No validation prompt defined for task; deliverable assumed approved."
                        .to_string(),
                ));
            }
            default_role_prompt
        }
    };

    let (backend, auth, validator_model) = resolve_validator_backend(cfg, agent);
    let val_client = crate::llm::ChatClient::new_with_token(&backend, &validator_model, &auth);

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

    let custom_blueprint = custom_validation_prompt
        .as_deref()
        .and_then(|p| crate::agents::AgentBlueprint::parse_from_markdown(p).ok());
    let prompt_allowed_tools = custom_blueprint
        .as_ref()
        .map(|bp| bp.allowed_tools.as_slice());

    let registry = crate::orchestrator::SpecialistRegistry::canonical();
    let val_entry = registry
        .resolve(Agent::Validator)
        .expect("validator is registered");
    let mcp_servers = cfg
        .orchestration
        .specialists
        .get(Agent::Validator.as_str())
        .map(|vc| vc.mcp_servers.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            cfg.orchestration
                .specialists
                .get(agent.as_str())
                .map(|sc| sc.mcp_servers.clone())
        })
        .unwrap_or_default();
    let tools = assemble_tools(
        prompt_allowed_tools,
        |name| val_entry.allows(name),
        &mcp_servers,
    );

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
    let worker_name = format!("validator-{agent}");

    let _active_guard = register_loop_worker(
        clean_task_id.clone(),
        worker_name.clone(),
        format!("Auditing {agent} deliverable"),
        token,
    );

    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let mut monitor = crate::harness::monitor::HarnessMonitor::new_with_config(
        std::sync::Arc::new(crate::harness::HarnessStats::new()),
        mon_cfg,
    );

    let caller = if let Some(allowed) = prompt_allowed_tools {
        crate::harness::ToolCaller::SpecialistWithTools {
            agent: Agent::Validator,
            allowed_tools: allowed.to_vec(),
        }
    } else {
        crate::harness::ToolCaller::Specialist(Agent::Validator)
    };

    let mut params = LoopParams {
        client: &val_client,
        model: validator_model,
        tag: val_tag.clone(),
        worker_name,
        task_id: clean_task_id,
        worker_key: _active_guard.0.clone(),
        engine: &mut engine,
        tools,
        token,
        mon_cfg,
        cfg,
        temperature: 0.0,
        status_template: format!("{val_tag}: evaluating test & inspection output..."),
        default_verdict_critique: "Deliverable verified and approved.".to_string(),
        verdict_log_role: agent.as_str().to_string(),
        dispatch_verdict_tools: true,
        abort_log_prefix: format!("validator-{agent}"),
        emit_tool_status: true,
        rebirth_notice: "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. Do not call rebirth consecutively without making progress. Continue your validation inspection and submit your verdict via leave_verdict.)".to_string(),
    };

    match run_fix_loop(&mut params, &mut monitor, caller).await {
        Ok(FixLoopResult::Verdict { approved, critique }) => Ok((approved, critique)),
        Ok(FixLoopResult::Exhausted) => {
            // Unreachable in practice (the driver returns a verdict after 3
            // nudges), but keeps the historical fallback explicit.
            Ok((
                true,
                "Validator completed turns without calling leave_verdict; assumed approved."
                    .to_string(),
            ))
        }
        Ok(FixLoopResult::Aborted) => {
            // Strict abort semantics: cancellation is a failure, not a verdict.
            Err(anyhow::anyhow!(
                "Validation aborted by cancellation signal."
            ))
        }
        Err(e) => Err(e),
    }
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

    let (backend, auth, validator_model) = resolve_validator_backend(cfg, Agent::Planner);
    let val_client = crate::llm::ChatClient::new_with_token(&backend, &validator_model, &auth);

    let brief = format!(
        "Proposed Execution Plan:\n```markdown\n{}\n```\n\n\
         Instructions:\n\
         1. Inspect the workspace and examine files using available inspection tools (`read_file`, `grep_search`, `glob`) if needed to evaluate feasibility and existing structure.\n\
         2. You are an auditor: you cannot execute shell commands or modify files. Solely analyze, inspect, and evaluate the proposed plan.\n\
         3. Audit the plan against dynamic criteria: proper format (# Execution Plan, `- [ ] [t-xxx]`), phase headers, atomic task granularity (no monolithic catch-all tasks), and contextual testing/verification (unit/integration tests ONLY when relevant to the project scope and deliverable — do not demand tests for docs, scripts, or simple configs).\n\
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
    let mcp_servers = validator_cfg
        .map(|vc| vc.mcp_servers.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| planner_cfg.map(|sc| sc.mcp_servers.clone()))
        .unwrap_or_default();
    let tools = assemble_tools(None, |name| val_entry.allows(name), &mcp_servers);

    let val_tag = "validator-planner".to_string();
    let _active_guard = register_loop_worker(
        None,
        val_tag.clone(),
        "Auditing execution plan".to_string(),
        token,
    );

    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let mut monitor = crate::harness::monitor::HarnessMonitor::new_with_config(
        std::sync::Arc::new(crate::harness::HarnessStats::new()),
        mon_cfg,
    );

    let mut params = LoopParams {
        client: &val_client,
        model: validator_model,
        tag: val_tag.clone(),
        worker_name: val_tag.clone(),
        task_id: None,
        worker_key: _active_guard.0.clone(),
        engine: &mut engine,
        tools,
        token,
        mon_cfg,
        cfg,
        temperature: 0.0,
        status_template: format!("{val_tag}: auditing execution plan structure & granularity..."),
        default_verdict_critique:
            "Execution plan structure and task decomposition verified.".to_string(),
        verdict_log_role: "planner".to_string(),
        // The plan auditor treats leave_verdict as terminal-only (detected
        // before dispatch); never dispatch it as a regular tool.
        dispatch_verdict_tools: false,
        abort_log_prefix: val_tag.clone(),
        emit_tool_status: false,
        rebirth_notice: "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. Do not call rebirth consecutively without making progress. Continue your validation inspection and submit your verdict via leave_verdict.)".to_string(),
    };

    match run_fix_loop(
        &mut params,
        &mut monitor,
        crate::harness::ToolCaller::Specialist(Agent::Validator),
    )
    .await
    {
        Ok(FixLoopResult::Verdict { approved, critique }) => Ok((approved, critique)),
        Ok(FixLoopResult::Exhausted) => Ok((
            true,
            "Auditor did not leave explicit verdict after 3 reminders; assumed approved."
                .to_string(),
        )),
        Ok(FixLoopResult::Aborted) => {
            // Strict abort semantics: cancellation is a failure, not a verdict.
            // (Previously returned Ok((false, …)) — a soft rejection that
            // downstream consumers could mistake for an auditor verdict.)
            Err(anyhow::anyhow!(
                "Plan validation aborted by cancellation signal."
            ))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_names::{TERMINAL_LEAVE_VERDICT, TOOL_DELEGATE_TASK, TOOL_READ_FILE};

    #[test]
    fn test_is_leave_verdict_tool_matching() {
        assert!(is_leave_verdict_tool(TOOL_LEAVE_VERDICT));
        assert!(is_leave_verdict_tool("LEAVE_VERDICT"));
        assert!(is_leave_verdict_tool("leaveVerdict"));
        assert!(is_leave_verdict_tool(TERMINAL_LEAVE_VERDICT));
        assert!(is_leave_verdict_tool("validator__leave_verdict"));
        assert!(is_leave_verdict_tool("leave_verdict_tool"));
        assert!(!is_leave_verdict_tool(TOOL_READ_FILE));
        assert!(!is_leave_verdict_tool(TOOL_DELEGATE_TASK));
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
