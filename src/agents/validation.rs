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
    FixLoopBounds, FixLoopResult, LoopParams, assemble_tools, register_loop_worker,
    resolve_validator_backend, run_fix_loop,
};
use crate::markers::{has_failure_verdict_cue, is_failure_verdict_word};
use crate::tool_names::is_leave_verdict_tool_name;

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

/// `true` when a tool name refers to the verdict tool (`leave_verdict`).
///
/// **There is no verdict vocabulary in this module** (t-069): the matcher is a thin
/// wrapper over [`crate::tool_names::is_leave_verdict_tool_name`], so the accepted
/// spellings are exactly
/// * the verdict rows of [`crate::tool_names::TOOL_ALIAS_TABLE`]
///   (`leave_verdict`, `terminal__leave_verdict`, `leaveVerdict`,
///   `leave_verdict_tool`), and
/// * the one namespaced-suffix rule
///   [`crate::tool_names::LEAVE_VERDICT_NAME_SUFFIXES`]
///   (`validator__leave_verdict`, `…_leave_verdict`).
///
/// Adding a verdict spelling means adding a table row — never a comparison here.
/// The harness dispatch arms, the tool-argument preview and the agent-loop verdict
/// handling all agree because they all end up in that one table.
pub fn is_leave_verdict_tool(name: &str) -> bool {
    is_leave_verdict_tool_name(name)
}

/// Reason recorded when no explicit approval was recorded anywhere in a verdict
/// (gate t-033b). An absent, ambiguous or unparseable verdict marker is *not* an
/// approval, so the deliverable is treated as not approved.
pub const NO_EXPLICIT_APPROVAL_REASON: &str = "No explicit verdict recorded: the verdict file does not record an explicit approval, so the deliverable is treated as not approved.";

/// Robustly parse (approved, critique) from `leave_verdict` arguments.
///
/// Fail-closed (gate t-033b): an explicitly recorded approval approves, an
/// explicitly recorded rejection rejects, and an ambiguous / unparseable /
/// marker-less payload resolves to **not approved** carrying
/// [`NO_EXPLICIT_APPROVAL_REASON`]. It never returns `None`, so callers cannot
/// paper over a missing verdict with an `unwrap_or(true)`-style default.
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
        } else if is_failure_verdict_word(s) {
            // The negative verdict words are owned by `crate::markers`
            // (gate t-059); this site used to chain seven
            // `eq_ignore_ascii_case("<literal>")` comparisons of its own.
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

    // 3. Fail-closed verdict resolution (gate t-033b). An explicit verdict field
    //    wins; otherwise explicit approval/rejection wording inside the comments
    //    is honoured; otherwise NO verdict was recorded and the deliverable is
    //    not approved. The historical fallback defaulted to `true` ("assume
    //    approved if no rejection indicated"), which turned an absent, ambiguous
    //    or unparseable verdict marker into evidence of success.
    let upper = comments.to_ascii_uppercase();
    // The failure cues come from the marker owner's table (gate t-059); the
    // approval side of the wording stays local to the validator.
    let explicit_verdict_wording = has_failure_verdict_cue(&upper)
        || upper.contains("APPROV")
        || upper.contains("PASS")
        || upper.contains("LGTM");

    let approved = match approved_opt {
        Some(b) => b,
        None => {
            if has_failure_verdict_cue(&upper) {
                false
            } else if upper.contains("APPROV") || upper.contains("PASS") || upper.contains("LGTM") {
                true
            } else {
                // No verdict marker at all: fail closed, never assume approval.
                false
            }
        }
    };

    let critique = if approved_opt.is_none() && !explicit_verdict_wording {
        // Nothing in the payload says "approved" and nothing says "rejected":
        // surface why the deliverable is nonetheless treated as not approved.
        if comments.is_empty() {
            NO_EXPLICIT_APPROVAL_REASON.to_string()
        } else {
            format!("{comments}\n\n{NO_EXPLICIT_APPROVAL_REASON}")
        }
    } else if !comments.is_empty() {
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
    // Gate t-046: `{tid}-validation.md` is an on-disk path derived from a task id,
    // so the id is validated as a single path segment before any join. A rejected
    // id is recorded as a hard verdict gap — the same fail-closed outcome as a
    // missing, unreadable or blank verdict file — and the id is never sanitized,
    // trimmed or clamped into some other file name.
    let validation_verdict_file = match task_id {
        Some(raw) => {
            let normalized = crate::task_id::normalize_task_id_ref(raw);
            match crate::task_id::validate_task_id(normalized) {
                Ok(id) => Some(format!("{id}-validation.md")),
                Err(err) => {
                    let reason = format!(
                        "Rejected task id {raw:?}: {err}. No `-validation.md` verdict file could be located for this task, so the deliverable was not validated.",
                    );
                    tracing::error!("{reason}");
                    return Ok((false, reason));
                }
            }
        }
        None => None,
    };
    let custom_validation_prompt = validation_verdict_file.as_deref().and_then(|name| {
        let path = crate::harness::get_workspace_root()
            .join(crate::manager::phase::MARMEL_DIR)
            .join("prompts")
            .join(name);
        // A blank verdict file records no verdict: treat it exactly like an
        // absent one instead of auditing against an empty brief.
        std::fs::read_to_string(&path)
            .ok()
            .filter(|text| !text.trim().is_empty())
    });

    let default_role_prompt = crate::agents::validator::role_prompt_for(agent);
    let validator_prompt = match custom_validation_prompt.as_deref() {
        Some(prompt) => prompt,
        None => {
            let prompts_dir = crate::harness::get_workspace_root()
                .join(crate::manager::phase::MARMEL_DIR)
                .join("prompts");
            if prompts_dir.is_dir() {
                // Inverted fallback fixed (H4 in docs/recon_bugs_agents_monitor.md,
                // gate t-033a): the prompts directory exists, yet this task has no
                // readable, non-empty `{tid}-validation.md` verdict file. That used
                // to resolve to an *approved* verdict ("deliverable assumed
                // approved"), i.e. missing validation configuration counted as
                // evidence of success. Absence of a recorded verdict is not an
                // approval, so it now maps to not-approved and the caller leaves the
                // plan line unchecked. An explicitly recorded verdict (the
                // `Some(prompt)` arm above) behaves exactly as before, and a
                // workspace without a prompts directory still falls back to the
                // generic validator role prompt.
                let verdict_ref = validation_verdict_file
                    .as_deref()
                    .unwrap_or("<task-id>-validation.md");
                let reason = format!(
                    "No validation verdict recorded for {} under {}: the deliverable was not validated.",
                    verdict_ref,
                    prompts_dir.display()
                );
                tracing::error!("{reason}");
                return Ok((false, reason));
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

    let clean_task_id = task_id.and_then(crate::task_id::normalize_task_id);

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
        verdict_log_role: agent.as_str().to_string(),
        // Bounds of this validation pass (gate t-033c): the round cap and the
        // wall-clock deadline end the pass with an explicit failed outcome.
        //
        // `dispatch_verdict_tools` was removed by t-033c: the verdict tool is
        // terminal-only and is consumed by the shared driver *before* dispatch,
        // so this loop never dispatches it for any worker — and a switch here
        // could only ever have widened the t-033b identity gate
        // (`runner/execution.rs::may_record_verdict`).
        bounds: FixLoopBounds::default(),
        abort_log_prefix: format!("validator-{agent}"),
        emit_tool_status: true,
        rebirth_notice: "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. Do not call rebirth consecutively without making progress. Continue your validation inspection and submit your verdict via leave_verdict.)".to_string(),
    };

    match run_fix_loop(&mut params, &mut monitor, caller).await {
        Ok(FixLoopResult::Verdict { approved, critique }) => Ok((approved, critique)),
        Ok(FixLoopResult::Exhausted { reason }) => {
            // The pass ran out of rounds or wall-clock budget without recording a
            // verdict. Fail closed: NOT approved, with the bound as the recorded
            // critique (never `unwrap_or(true)`-style approval).
            tracing::warn!("validator-{agent} validation pass exhausted its bounds: {reason}");
            Ok((false, reason))
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
        verdict_log_role: "planner".to_string(),
        // Bounds of this audit pass (gate t-033c). The former
        // `dispatch_verdict_tools: false` switch is gone: the shared driver
        // always treats the verdict tool as terminal-only (consumed before any
        // dispatch), so both call sites now behave identically — that difference
        // used to be a documentation-only switch over dead code.
        bounds: FixLoopBounds::default(),
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
        Ok(FixLoopResult::Exhausted { reason }) => {
            // Out of rounds / wall-clock budget without a verdict: fail closed,
            // the plan is reported as NOT approved with the bound as critique.
            tracing::warn!("plan audit exhausted its bounds: {reason}");
            Ok((false, reason))
        }
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
    use crate::tool_names::{
        LEAVE_VERDICT_NAME_SUFFIXES, TERMINAL_LEAVE_VERDICT, TOOL_DELEGATE_TASK,
        TOOL_LEAVE_VERDICT, TOOL_READ_FILE, is_leave_verdict_tool_name, normalize_tool_alias,
        tool_spellings_for,
    };

    /// The historical matcher fixtures, kept verbatim because they are **wire
    /// input**: each raw spelling below is exactly what a model or an MCP-style
    /// namespace emits, which is the point of the test. They are not a second
    /// vocabulary — `test_validation_module_enumerates_no_verdict_spelling_of_its_own`
    /// below proves the production half of this file spells none of them, and
    /// `test_verdict_spellings_resolve_identically_through_the_table_and_the_matcher`
    /// pins that the table accepts the same set.
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

    /// (t-069 item 1, test (a)) Table-driven equivalence: the validator's matcher
    /// and the shared alias table must accept **exactly** the same verdict
    /// spellings. The spellings are read from the table at runtime, so a newly
    /// added verdict row is covered by this test the moment it exists — and a
    /// spelling the table does not know is rejected by both sides.
    #[test]
    fn test_verdict_spellings_resolve_identically_through_the_table_and_the_matcher() {
        // The table's own verdict family: every alias row pointing at the terminal
        // verdict spelling, plus the canonical spelling itself.
        let family = tool_spellings_for(TERMINAL_LEAVE_VERDICT);
        assert!(
            family.contains(&TOOL_LEAVE_VERDICT) && family.contains(&TERMINAL_LEAVE_VERDICT),
            "the table must own both the bare and the terminal verdict spelling, got {family:?}"
        );
        for spelling in &family {
            assert_eq!(
                normalize_tool_alias(spelling),
                Some(TERMINAL_LEAVE_VERDICT),
                "'{spelling}' must normalize onto the verdict tool"
            );
            assert_eq!(
                is_leave_verdict_tool_name(spelling),
                is_leave_verdict_tool(spelling),
                "the table predicate and the validator matcher must agree on '{spelling}'"
            );
            assert!(
                is_leave_verdict_tool(spelling),
                "'{spelling}' is a table row and must be accepted by is_leave_verdict_tool"
            );
        }

        // The namespaced form is one suffix rule, not a private list: both sides
        // must agree for every namespace.
        for namespace in ["terminal", "validator", "auditor", "planner", "legacy"] {
            for suffix in LEAVE_VERDICT_NAME_SUFFIXES {
                let namespaced = format!("{namespace}{suffix}");
                assert_eq!(
                    is_leave_verdict_tool_name(&namespaced),
                    is_leave_verdict_tool(&namespaced),
                    "both predicates must agree on the namespaced spelling '{namespaced}'"
                );
                assert!(
                    is_leave_verdict_tool(&namespaced),
                    "'{namespaced}' must name the verdict tool through the suffix rule"
                );
            }
        }

        // Case tolerance is shared, not re-implemented in this module.
        for folded in [
            "LEAVE_VERDICT",
            "Leave_Verdict_Tool",
            "TERMINAL__LEAVE_VERDICT",
        ] {
            assert!(
                is_leave_verdict_tool(folded) && is_leave_verdict_tool_name(folded),
                "'{folded}' must be accepted through the shared (case-tolerant) predicate"
            );
        }

        // Near-misses stay unknown to both: no prefix matching, no fuzzy verdict.
        for near_miss in [
            "verdict",
            "leave",
            "leave_verdicts",
            "leaveverdict_tool",
            "read_verdict",
            "submit_verdict",
            "",
        ] {
            assert!(
                !is_leave_verdict_tool_name(near_miss),
                "the table predicate must reject {near_miss:?}"
            );
            assert!(
                !is_leave_verdict_tool(near_miss),
                "the validator matcher must reject {near_miss:?}"
            );
        }
    }

    /// (t-069 item 1) Source guard: this module must not enumerate a verdict-name
    /// spelling of its own. Needles are built at runtime from the table and from
    /// the suffix rule, so the guard covers spellings added later. Only the
    /// production part is scanned — the tests above legitimately *call* the matcher
    /// with raw wire spellings, which is what a fixture for an accepted alias is.
    #[test]
    fn test_validation_module_enumerates_no_verdict_spelling_of_its_own() {
        let src = include_str!("validation.rs");
        let production = src
            .split(&["#[cfg(", "test)", "]"].concat())
            .next()
            .expect("validation.rs contains the test attribute");

        let mut needles: Vec<String> = tool_spellings_for(TERMINAL_LEAVE_VERDICT)
            .into_iter()
            .map(|spelling| format!("\"{spelling}\""))
            .collect();
        for suffix in LEAVE_VERDICT_NAME_SUFFIXES {
            needles.push(format!("\"{suffix}\""));
        }
        assert!(
            needles.len() >= 4,
            "the verdict vocabulary must be non-trivial for this guard to mean anything"
        );

        for needle in &needles {
            assert!(
                !production.contains(needle.as_str()),
                "src/agents/validation.rs still spells the verdict name {needle} itself — \
                 verdict spellings belong to crate::tool_names::TOOL_ALIAS_TABLE / \
                 LEAVE_VERDICT_NAME_SUFFIXES, and is_leave_verdict_tool must stay a wrapper"
            );
        }

        // The delegation itself: the matcher calls the shared predicate, nothing else.
        assert!(
            production.contains("is_leave_verdict_tool_name("),
            "is_leave_verdict_tool must delegate to crate::tool_names::is_leave_verdict_tool_name"
        );
        assert!(
            production.contains("pub fn is_leave_verdict_tool(name: &str) -> bool {"),
            "the public matcher must stay a single-line wrapper over the shared predicate"
        );
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

    /// Gate t-059 (dedup sweep A): the negative verdict vocabulary is owned by
    /// `crate::markers` (`FAILURE_VERDICT_WORDS` for whole verdict fields,
    /// `FAILURE_PROSE_CUES` for free-form comments). Table driven, and pinned
    /// to the exact rendered critique bytes so the migration cannot change any
    /// observable output.
    #[test]
    fn test_parse_verdict_args_uses_the_marker_owned_failure_vocabulary() {
        // (1) whole-value verdict fields — every word of the owner's table,
        //     every field alias, both casings, pinned to the exact critique.
        for word in crate::markers::FAILURE_VERDICT_WORDS {
            let lowered = word.to_ascii_lowercase();
            for alias in ["verdict", "status", "decision", "result", "assessment"] {
                for spelling in [*word, lowered.as_str()] {
                    let args = serde_json::json!({alias: spelling, "comments": "Fix errors"});
                    let (approved, critique) = parse_verdict_args(&args)
                        .unwrap_or_else(|| panic!("{alias}={spelling} must yield a verdict"));
                    assert!(!approved, "{alias}={spelling} must not be approved");
                    assert_eq!(critique, "Fix errors", "{alias}={spelling}");
                }
            }
        }
        // An empty verdict field carries no verdict word at all: fail closed.
        let (approved, critique) =
            parse_verdict_args(&serde_json::json!({"verdict": ""})).expect("fail-closed payload");
        assert!(!approved);
        assert_eq!(
            critique, NO_EXPLICIT_APPROVAL_REASON,
            "an empty verdict field must keep the fail-closed reason verbatim"
        );

        // (2) prose cues: no verdict field, rejection wording only in the
        //     comments — the comments are surfaced byte-for-byte.
        for comment in [
            "the build FAILED at link time",
            "build FAILURE: missing symbol",
            "REJECTED: no regression test",
        ] {
            let args = serde_json::json!({"comments": comment});
            let (approved, critique) = parse_verdict_args(&args)
                .unwrap_or_else(|| panic!("{comment} must yield a verdict"));
            assert!(!approved, "{comment}");
            assert_eq!(critique, comment, "{comment}");
        }
        // A comment with neither cue nor approval wording keeps the fail-closed
        // reason, unchanged by the migration.
        let (approved, critique) =
            parse_verdict_args(&serde_json::json!({"comments": "no wording of note"}))
                .expect("payload");
        assert!(!approved);
        assert_eq!(
            critique,
            format!("no wording of note\n\n{NO_EXPLICIT_APPROVAL_REASON}")
        );
    }

    /// Gate t-059: the validator's prose cue is deliberately **weaker** than
    /// `markers::has_failure_marker` (a `0 failed` test counter is a benign
    /// counter for the marker grammar but still reads as a rejection cue in a
    /// validator comment). Preserving that asymmetry is what keeps this
    /// migration output-neutral; pinning it stops a later "cleanup" from
    /// silently merging the two predicates.
    #[test]
    fn test_verdict_prose_cue_is_weaker_than_the_failure_marker() {
        let benign_counter = "test result: ok. 15 passed; 0 failed";
        assert!(
            crate::markers::has_failure_verdict_cue(&benign_counter.to_ascii_uppercase()),
            "the validator cue still fires on a `0 failed` counter"
        );
        assert!(
            !crate::markers::has_failure_marker(benign_counter),
            "the marker grammar exempts the same text"
        );
        let args = serde_json::json!({"comments": benign_counter});
        let (approved, critique) = parse_verdict_args(&args).expect("payload");
        assert!(!approved);
        assert_eq!(critique, benign_counter);
    }

    /// Gate t-033b: the parsed verdict used to DEFAULT TO APPROVED whenever the
    /// payload carried no recognizable verdict marker ("assume approved if no
    /// rejection indicated"). An absent, ambiguous or unparseable marker must now
    /// resolve to *not approved* with an explicit reason.
    #[test]
    fn test_parse_verdict_args_ambiguous_payload_is_not_approved() {
        let ambiguous = [
            serde_json::json!({}),
            serde_json::json!({"comments": "I looked at the diff."}),
            serde_json::json!({"verdict": "maybe"}),
            serde_json::json!({"verdict": "undecided", "comments": "Needs a second look."}),
            serde_json::json!({"status": "OK-ish"}),
            serde_json::json!({"decision": null}),
            serde_json::json!({"verdict": 7, "comments": "not a marker at all"}),
            serde_json::json!({"verdict": [], "assessment": ""}),
        ];

        for case in ambiguous {
            let parsed = parse_verdict_args(&case);
            assert!(
                parsed.is_some(),
                "a verdict payload must resolve to a verdict, never to a caller-side default: {case:?}"
            );
            let (approved, reason) = parsed.unwrap();
            assert!(
                !approved,
                "an ambiguous payload must not be approved: {case:?}"
            );
            assert!(
                reason.contains("does not record an explicit approval"),
                "reason must name the missing explicit approval: {reason}"
            );
            assert!(
                reason.contains("treated as not approved"),
                "reason must state the deliverable is treated as not approved: {reason}"
            );
        }

        // Comments without any verdict wording are kept, but the fail-closed
        // reason is appended so the recorded verdict explains the outcome.
        let (approved, reason) =
            parse_verdict_args(&serde_json::json!({"comments": "I looked at the diff."})).unwrap();
        assert!(!approved);
        assert!(
            reason.starts_with("I looked at the diff."),
            "recorded comments must be preserved: {reason}"
        );

        // The reason text itself is the documented fail-closed wording.
        assert!(
            NO_EXPLICIT_APPROVAL_REASON
                .contains("the verdict file does not record an explicit approval, so the deliverable is treated as not approved"),
            "fail-closed reason must use the documented wording: {NO_EXPLICIT_APPROVAL_REASON}"
        );
    }

    /// Gate t-033b (counterpart): failing closed must not break the explicit
    /// direction — an explicitly recorded approve still approves and an explicit
    /// reject still rejects, with the recorded comments untouched.
    #[test]
    fn test_parse_verdict_args_explicit_direction_is_preserved() {
        let (approved, comments) =
            parse_verdict_args(&serde_json::json!({"verdict": "APPROVED", "comments": "SHIP IT"}))
                .unwrap();
        assert!(approved, "an explicit approval must still approve");
        assert_eq!(
            comments, "SHIP IT",
            "an explicit verdict keeps its recorded comments verbatim"
        );

        let (approved, comments) =
            parse_verdict_args(&serde_json::json!({"verdict": "REJECTED", "comments": "FIX IT"}))
                .unwrap();
        assert!(!approved, "an explicit rejection must still reject");
        assert_eq!(comments, "FIX IT");

        // Explicit approval expressed only in prose still counts as explicit.
        let (approved, _) =
            parse_verdict_args(&serde_json::json!({"comments": "All checks passed, LGTM."}))
                .unwrap();
        assert!(
            approved,
            "explicit approval wording in comments is still an approval"
        );
    }

    /// H4 (gate t-033a): a workspace that pre-generates task prompts but has no
    /// `{tid}-validation.md` verdict file for this task used to come back as an
    /// **approved** verdict ("deliverable assumed approved"). Absence of a
    /// recorded verdict must map to not-approved.
    #[tokio::test]
    async fn missing_validation_verdict_file_is_not_an_approval() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(
            tmp.path()
                .join(crate::manager::phase::MARMEL_DIR)
                .join("prompts"),
        )
        .expect("prompts dir");

        let (approved, reason) = verdict_for(tmp.path(), "t-900").await;

        assert!(!approved, "a missing verdict file must not be approved");
        assert!(
            reason.contains("No validation verdict recorded"),
            "reason must name the missing verdict: {reason}"
        );
        assert!(
            reason.contains("not validated"),
            "reason must state the deliverable was not validated: {reason}"
        );
    }

    /// H4 (gate t-033a): an unreadable (invalid UTF-8 garbage) or blank verdict
    /// file is not an approval either — the fallback must fail closed.
    #[tokio::test]
    async fn unreadable_or_blank_validation_verdict_file_is_not_an_approval() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prompts = tmp
            .path()
            .join(crate::manager::phase::MARMEL_DIR)
            .join("prompts");
        std::fs::create_dir_all(&prompts).expect("prompts dir");
        std::fs::write(
            prompts.join("t-901-validation.md"),
            [0xFFu8, 0xFE, 0x00, 0x80],
        )
        .expect("garbage verdict file");
        std::fs::write(prompts.join("t-902-validation.md"), "   \n\t\n")
            .expect("blank verdict file");

        let (unreadable_approved, unreadable_reason) = verdict_for(tmp.path(), "t-901").await;
        assert!(
            !unreadable_approved,
            "an unreadable verdict file must not be approved: {unreadable_reason}"
        );
        assert!(unreadable_reason.contains("No validation verdict recorded"));

        let (blank_approved, blank_reason) = verdict_for(tmp.path(), "t-902").await;
        assert!(
            !blank_approved,
            "a blank verdict file must not be approved: {blank_reason}"
        );
        assert!(blank_reason.contains("No validation verdict recorded"));
    }

    /// Drive the validation entry point for one task id inside `root`. The
    /// fail-closed paths return before any backend is contacted, so the dead
    /// client url is never used.
    async fn verdict_for(root: &std::path::Path, task_id: &str) -> (bool, String) {
        let root = root.to_path_buf();
        crate::harness::with_workspace_root(root, async {
            let cfg = crate::config::Config::default();
            let client = crate::llm::ChatClient::new("http://127.0.0.1:9/v1", "test-model");
            let token = tokio_util::sync::CancellationToken::new();
            run_automated_validation(
                &client,
                Agent::Coder,
                Some(task_id),
                "brief under test",
                "deliverable under test",
                &cfg,
                &token,
            )
            .await
            .expect("a missing/unreadable verdict is a verdict, not an error")
        })
        .await
    }

    /// Gate t-046: the task id is validated as a single path segment before it is
    /// joined onto the prompts directory.
    ///
    /// The fixture plants a verdict file **one directory above** the prompts
    /// directory, i.e. only reachable through `../`. Without the gate the id
    /// `../escape` would read it, the auditor would run against that planted
    /// brief and the run would reach the (dead) validator backend — so the
    /// escape is visible here as an error from `verdict_for`'s `expect`, never as
    /// a silent success. With the gate the id is refused and reported as a hard
    /// verdict gap, exactly like a missing verdict file.
    #[tokio::test]
    async fn hostile_task_id_cannot_reach_a_verdict_file_outside_the_prompts_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prompts = tmp
            .path()
            .join(crate::manager::phase::MARMEL_DIR)
            .join("prompts");
        std::fs::create_dir_all(&prompts).expect("prompts dir");
        std::fs::write(
            tmp.path()
                .join(crate::manager::phase::MARMEL_DIR)
                .join("escape-validation.md"),
            "planted verdict one directory above the prompts directory",
        )
        .expect("planted verdict file");

        for hostile in ["../escape", "../../escape", "a/b", "..", ".", "", "   "] {
            let (approved, reason) = verdict_for(tmp.path(), hostile).await;
            assert!(
                !approved,
                "a rejected task id must never be approved: {hostile:?} → {reason}"
            );
            assert!(
                reason.contains("Rejected task id"),
                "the failure must name the rejected id: {reason}"
            );
            assert!(
                reason.contains("was not validated"),
                "the failure must be a hard verdict gap: {reason}"
            );
            assert!(
                !reason.contains("planted verdict"),
                "the escaped verdict file must never be used as the brief: {reason}"
            );
        }

        // Nothing was created in the prompts directory by any of these runs, and
        // the planted fixture stayed where it was (one directory above).
        let created: Vec<String> = std::fs::read_dir(&prompts)
            .expect("prompts dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            created.is_empty(),
            "no verdict file may appear in the prompts directory: {created:?}"
        );
        assert!(
            tmp.path()
                .join(crate::manager::phase::MARMEL_DIR)
                .join("escape-validation.md")
                .is_file(),
            "the planted fixture must be untouched"
        );
    }
}
