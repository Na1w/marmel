//! The harness-level `delegate_task` handler and per-specialist tool allowlist.

use super::OrchestratorManager;
use super::pool;
use super::registry::SpecialistRegistry;
use crate::agents::Agent;
use crate::agents::{DelegationRequest, Deliverable, MissionMarker};
use crate::harness::{HarnessStats, ToolError, ToolResult};
use crate::llm::ChatClient;
use crate::manager::phase::Plan;
use crate::markers::{
    ABORT_REASON, MARKER_COMPLETE, MARKER_FAILED, MARKER_REPLAN, aborted_deliverable, decorated,
    has_failure_marker, has_replan_marker,
};
use crate::tool_names::TOOL_DELEGATE_TASK;
use std::sync::Arc;

/// The harness-level `delegate_task` handler: parses the tool arguments
/// (`agent_name`, `prompt`, `snippets`, `task_id?`, `image_urls?`,
/// `audio_urls?`), validates the role against the registry, and builds one
/// self-contained `DelegationRequest` per call. Routing through
/// `OrchestratorManager::delegate` blocks (via `block_on`) until the
/// deliverable returns, and the `ToolResult` outcome reflects its terminal
/// marker. The Manager is rooted at the shared `.marmel` plan dir, so the
/// bound `task_id` auto-checks off that on-disk plan.
pub fn handle_delegate_task(args: &serde_json::Value) -> Result<ToolResult, ToolError> {
    // 1. Parse the payload. `agent_name` deserializes through `Agent`'s
    //    snake_case enum, so an unknown role is rejected here with a clear
    //    error rather than panicking.
    let req: DelegationRequest =
        serde_json::from_value(args.clone()).map_err(|e| ToolError::BadArguments {
            tool: TOOL_DELEGATE_TASK.to_string(),
            detail: e.to_string(),
        })?;

    // 2. One task per call: the brief MUST be self-contained and non-empty.
    //    The subagent sees only this brief + snippets.
    if req.prompt.trim().is_empty() {
        return Err(ToolError::BadArguments {
            tool: TOOL_DELEGATE_TASK.to_string(),
            detail: "`prompt` must be a non-empty, self-contained task brief".to_string(),
        });
    }

    // 2b. task_id is mandatory: it must identify which execution plan task is
    //     being delegated, because auto check-off is keyed on that binding.
    let raw_task_id = req.task_id.as_deref().unwrap_or("").trim();
    if raw_task_id.is_empty() {
        return Err(ToolError::BadArguments {
            tool: TOOL_DELEGATE_TASK.to_string(),
            detail: "`task_id` is mandatory: you must specify the execution_plan.md task id (e.g. 't-001') to delegate work".to_string(),
        });
    }
    let clean_task_id = crate::task_id::normalize_task_id_ref(raw_task_id).to_string();
    if clean_task_id.is_empty() {
        return Err(ToolError::BadArguments {
            tool: TOOL_DELEGATE_TASK.to_string(),
            detail: "`task_id` cannot be empty".to_string(),
        });
    }
    let mut req = req;
    req.task_id = Some(clean_task_id);

    // 2c. Guard: reject re-delegation of tasks already checked off in the plan.
    {
        let plan = Plan::default();
        if let Some(ref tid) = req.task_id
            && let Ok(Some(content)) = plan.read()
        {
            let clean_tid = crate::task_id::normalize_task_id_ref(tid);
            // Same grammar as the on-disk check-off in `manager::phase`, so a
            // plan written as `- [x] (t-xxx) …` is recognised here too.
            let is_checked = crate::plan_parse::is_checked_task(&content, clean_tid);
            if is_checked {
                tracing::warn!("Rejecting re-delegation of already completed task [{clean_tid}]");
                return Ok(ToolResult::err(format!(
                    "Task '{clean_tid}' is already completed and checked off in the execution plan. Do not re-delegate completed tasks. Proceed with your final report synthesis."
                )));
            }
        }
    }

    // 3. Route through a Manager rooted at the shared `.marmel` plan dir.
    //    The build URL/model are unused by the deterministic Phase-O
    //    `run_specialist_llm` driver, so a placeholder client is fine.
    let cfg = crate::config::get_active()
        .or_else(|| crate::config::load(None).ok())
        .unwrap_or_default();
    let stats = Arc::new(HarnessStats::new());
    let manager = OrchestratorManager::from_config(
        ChatClient::new_with_token(&cfg.backend_url, &cfg.model, &cfg.auth_token),
        Plan::default(),
        stats,
        &cfg,
    );

    // 4. Call context for the nested delegation. Whatever dispatched this tool
    //    call installed its own context — a Manager turn, or a parent specialist
    //    whose allowlist granted recursion — and that task-local does **not**
    //    cross the `thread::scope` + `block_on` boundary below by itself. So it is
    //    carried explicitly and then **overwritten**: every backend call this
    //    nested delegation makes (worker turn, JIT prompt synthesis, validator
    //    passes) must be attributed to this agent and task id, not to the parent
    //    that triggered it. The sync scope restores the previous context on the way
    //    out — including on unwind — so nothing leaks back to the caller when this
    //    delegation returns.
    let parent_context = pool::current_call_context();
    let nested_context = pool::CallContext::for_agent(
        pool::CallClass::Specialist,
        req.agent_name.as_str(),
        req.task_id.clone().or_else(|| {
            parent_context
                .as_ref()
                .and_then(|parent| parent.task.clone())
        }),
    );
    if let Some(parent) = parent_context.as_ref() {
        tracing::debug!(
            parent = %parent.describe(),
            nested = %nested_context.describe(),
            "nested delegate_task overwrites the inherited call context"
        );
    }

    let deliverable = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let join_res = std::thread::scope(|s| {
            s.spawn(|| {
                pool::with_call_context_sync(nested_context.clone(), || {
                    if crate::orchestrator::is_globally_cancelled() {
                        return Ok(Deliverable {
                            marker: MissionMarker::Failed {
                                reason: ABORT_REASON.to_string(),
                            },
                            content: aborted_deliverable("aborted by user instruction"),
                            task_id: req.task_id.clone(),
                        });
                    }
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle.block_on(manager.delegate(req))
                    }))
                    .unwrap_or_else(|_| {
                        Ok(Deliverable {
                            marker: MissionMarker::Failed {
                                reason: "task interrupted or runtime shutting down".to_string(),
                            },
                            content: aborted_deliverable(
                                "execution interrupted or runtime shutting down",
                            ),
                            task_id: None,
                        })
                    })
                })
            })
            .join()
        });
        match join_res {
            Ok(res) => res,
            Err(_) => Ok(Deliverable {
                marker: MissionMarker::Failed {
                    reason: "task thread interrupted or runtime shutting down".to_string(),
                },
                content: aborted_deliverable(
                    "execution thread interrupted or runtime shutting down",
                ),
                task_id: None,
            }),
        }
    } else {
        pool::with_call_context_sync(nested_context, || {
            futures::executor::block_on(manager.delegate(req))
        })
    }
    .map_err(ToolError::Execution)?;

    // 5. Encode the terminal marker into the ToolResult so the loop's
    //    check-off and the Manager's synthesis can observe it.
    let tid = deliverable.task_id.as_deref().unwrap_or("unknown");
    match &deliverable.marker {
        MissionMarker::Complete { .. } => {
            let mut res = deliverable.content.trim().to_string();
            let complete_token = decorated(MARKER_COMPLETE, tid);
            if !res.contains(&complete_token) {
                res.push_str("\n\n");
                res.push_str(&complete_token);
            }
            Ok(ToolResult::ok(res))
        }
        MissionMarker::Failed { reason } => {
            let content = deliverable.content.trim();
            if has_failure_marker(content) {
                Ok(ToolResult::err(content.to_string()))
            } else {
                Ok(ToolResult::err(format!(
                    "{content}\n\n{MARKER_FAILED}: {reason}"
                )))
            }
        }
        MissionMarker::Replan { reason } => {
            let content = deliverable.content.trim();
            if has_replan_marker(content) {
                Ok(ToolResult::err(content.to_string()))
            } else {
                Ok(ToolResult::err(format!(
                    "{content}\n\n{MARKER_REPLAN}: {reason}"
                )))
            }
        }
    }
}

/// Per-specialist tool-allowlist enforcement.
///
/// Returns `true` when the named caller role is permitted to invoke `tool`.
/// Specialists are restricted by their registry allowlist; `create_plan` is a
/// Manager-only tool (no specialist allowlist grants it), and `delegate_task`
/// is permitted to a specialist only when its granted tool set includes it
/// (bounded fractal recursion).
pub fn caller_allows_tool(agent: Agent, tool: &str, registry: &SpecialistRegistry) -> bool {
    match registry.resolve(agent) {
        Some(entry) => entry.allows(tool),
        None => false,
    }
}

// NOTE: `brief_for_task` reads a plan line's text to build the delegation
// brief: one task per call, and the brief is self-contained so the subagent
// never needs the Manager's context. It is `pub` so the Manager turn loop in
// `src/manager/loop.rs` reuses the same plan-line → brief builder. The lookup
// goes through [`crate::plan_parse`], so bracketed, parenthesised, starred and
// upper-case-checkbox spellings all yield a real brief.
pub fn brief_for_task(plan: &Plan, task_id: &str) -> String {
    // Read-only diagnostic: build a self-contained brief from the plan task
    // text. If the plan line is present, its descriptive text becomes the
    // brief; otherwise fall back to a deterministic generic instruction.
    if let Ok(Some(content)) = plan.read()
        && let Some(task) = crate::plan_parse::find_task_line(&content, task_id)
        && !task.description.trim().is_empty()
    {
        let desc = task.description.trim();
        return format!(
            "{desc}\n\nExecute this delegated task to completion and return your \
             deliverable, ending with {MARKER_COMPLETE} ({task_id})."
        );
    }
    format!(
        "Execute the delegated task described by the plan line, producing the\ndeliverable and ending with {MARKER_COMPLETE} (task-id)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The brief must come from the plan line for every accepted spelling, and
    /// must keep the historical generic fallback for unknown ids.
    #[test]
    fn brief_for_task_reads_every_accepted_task_line_form() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plan = Plan::at(dir.path());
        let content = "\
# Execution Plan
- [ ] [t-c3del-1] Build the parser
- [ ] (t-c3del-2) Migrate schema
* [ ] t-c3del-3 Add tests
- [x] **[t-c3del-4]** Docs refreshed
";
        plan.create(content).expect("create plan");

        assert!(brief_for_task(&plan, "t-c3del-1").starts_with("Build the parser"));
        assert!(brief_for_task(&plan, "t-c3del-2").starts_with("Migrate schema"));
        assert!(brief_for_task(&plan, "t-c3del-3").starts_with("Add tests"));
        assert!(brief_for_task(&plan, "t-c3del-4").starts_with("Docs refreshed"));
        assert!(
            brief_for_task(&plan, "[t-c3del-2]").starts_with("Migrate schema"),
            "decorated task ids must resolve"
        );
        assert!(
            brief_for_task(&plan, "t-c3del-404")
                .starts_with("Execute the delegated task described by the plan line"),
            "unknown ids keep the generic fallback brief"
        );
    }

    /// A plan line that is already checked off still yields its own brief (the
    /// guard against re-delegation is separate, see `handle_delegate_task`).
    #[test]
    fn re_delegation_guard_uses_the_shared_grammar() {
        let content = "# Execution Plan\n- [x] (t-c3del-9) Done in paren form\n";
        assert!(crate::plan_parse::is_checked_task(content, "t-c3del-9"));
        assert!(crate::plan_parse::is_checked_task(content, "[t-c3del-9]"));
        assert!(!crate::plan_parse::is_checked_task(content, "t-c3del-8"));
        let pending = "# Execution Plan\n- [ ] (t-c3del-7) Not done\n";
        assert!(!crate::plan_parse::is_checked_task(pending, "t-c3del-7"));
        assert!(crate::plan_parse::is_pending_task(pending, "t-c3del-7"));
    }
}
