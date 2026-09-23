//! The harness-level `delegate_task` handler and per-specialist tool gating.

use super::OrchestratorManager;
use super::registry::SpecialistRegistry;
use crate::agents::Agent;
use crate::agents::{DelegationRequest, Deliverable, MissionMarker};
use crate::harness::{HarnessStats, ToolError, ToolResult};
use crate::llm::ChatClient;
use crate::manager::phase::Plan;
use crate::tool_names::TOOL_DELEGATE_TASK;
use std::sync::Arc;

/// REQ-ORCH-005: the harness-level `delegate_task` handler.
///
/// Parses the tool arguments (`agent_name`, `prompt`, `snippets`, `task_id?`,
/// `image_urls?`, `audio_urls?`), validates the role against the registry,
/// builds a self-contained `DelegationRequest` (one task per call), routes it
/// through `OrchestratorManager::delegate` **synchronously** (REQ-ORCH-005:
/// the call blocks from the Manager's perspective via `block_on`), and returns
/// the deliverable as a `ToolResult` whose outcome reflects the `MISSION
/// COMPLETE (task-id)` / `FAILED` / `REPLAN REQUIRED` terminal marker.
///
/// The handler installs a Manager rooted at the shared `.marmel` plan dir so
/// `task_id` binding auto-check-off (`Plan::check_off`) targets the on-disk
/// plan (REQ-ORCH-004 shared workspace / REQ-PLAN-002).
pub fn handle_delegate_task(args: &serde_json::Value) -> Result<ToolResult, ToolError> {
    // 1. Parse the payload. `agent_name` deserializes through `Agent`'s
    //    snake_case enum, so an unknown role is rejected here with a clear
    //    error rather than panicking (REQ-ORCH-002).
    let req: DelegationRequest =
        serde_json::from_value(args.clone()).map_err(|e| ToolError::BadArguments {
            tool: TOOL_DELEGATE_TASK.to_string(),
            detail: e.to_string(),
        })?;

    // 2. One task per call: the brief MUST be self-contained and non-empty
    //    (REQ-ORCH-003/005). The subagent sees only this brief + snippets.
    if req.prompt.trim().is_empty() {
        return Err(ToolError::BadArguments {
            tool: TOOL_DELEGATE_TASK.to_string(),
            detail: "`prompt` must be a non-empty, self-contained task brief".to_string(),
        });
    }

    // 2b. task_id is mandatory (REQ-ORCH-005): must identify which execution plan item is being delegated.
    let raw_task_id = req.task_id.as_deref().unwrap_or("").trim();
    if raw_task_id.is_empty() {
        return Err(ToolError::BadArguments {
            tool: TOOL_DELEGATE_TASK.to_string(),
            detail: "`task_id` is mandatory: you must specify the execution_plan.md task id (e.g. 't-001') to delegate work".to_string(),
        });
    }
    let clean_task_id = raw_task_id
        .trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\'')
        .trim()
        .to_string();
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
            let clean_tid = tid
                .trim()
                .trim_matches(|c| {
                    c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
                })
                .trim();
            let tid_lower = clean_tid.to_ascii_lowercase();
            let re_checked = regex::Regex::new(&format!(
                r"(?i)^\s*(?:[-*]|\d+\.)\s*\[\s*[xX]\s*\]\s*\*{{0,2}}\[?{}\]?\*{{0,2}}\b",
                regex::escape(&tid_lower)
            ))
            .ok();
            let is_checked = content.lines().any(|line| match &re_checked {
                Some(re) => re.is_match(line),
                None => {
                    let lower = line.to_ascii_lowercase();
                    lower.contains(&format!("[{tid_lower}]"))
                        && (line.contains("[x]") || line.contains("[X]"))
                }
            });
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

    let deliverable = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let join_res = std::thread::scope(|s| {
            s.spawn(|| {
                if crate::orchestrator::is_globally_cancelled() {
                    return Ok(Deliverable {
                        marker: MissionMarker::Failed {
                            reason: "aborted".to_string(),
                        },
                        content: "Task aborted by user instruction.\n\nFAILED (aborted)".to_string(),
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
                        content: "Task execution interrupted or runtime shutting down.\n\nFAILED (aborted)".to_string(),
                        task_id: None,
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
                content: "Task execution thread interrupted or runtime shutting down.\n\nFAILED (aborted)".to_string(),
                task_id: None,
            }),
        }
    } else {
        futures::executor::block_on(manager.delegate(req))
    }
    .map_err(ToolError::Execution)?;

    // 5. Encode the terminal marker into the ToolResult so the loop's
    //    check-off and the Manager's synthesis can observe it.
    let tid = deliverable.task_id.as_deref().unwrap_or("unknown");
    match &deliverable.marker {
        MissionMarker::Complete { .. } => {
            let mut res = deliverable.content.trim().to_string();
            let complete_token = format!("MISSION COMPLETE ({tid})");
            if !res.contains(&complete_token) {
                res.push_str("\n\n");
                res.push_str(&complete_token);
            }
            Ok(ToolResult::ok(res))
        }
        MissionMarker::Failed { reason } => {
            let content = deliverable.content.trim();
            if content.contains("FAILED") {
                Ok(ToolResult::err(content.to_string()))
            } else {
                Ok(ToolResult::err(format!("{content}\n\nFAILED: {reason}")))
            }
        }
        MissionMarker::Replan { reason } => {
            let content = deliverable.content.trim();
            if content.contains("REPLAN REQUIRED") {
                Ok(ToolResult::err(content.to_string()))
            } else {
                Ok(ToolResult::err(format!(
                    "{content}\n\nREPLAN REQUIRED: {reason}"
                )))
            }
        }
    }
}

/// REQ-ORCH-002 / REQ-ORCH-005: per-specialist tool-allowlist enforcement.
///
/// Returns `true` when the named caller role is permitted to invoke `tool`.
/// Specialists are gated by their registry allowlist; `create_plan` is a
/// Manager-only tool (no specialist allowlist grants it), and `delegate_task`
/// is permitted to a specialist only when its granted tool set includes it
/// (fractal recursion, REQ-ORCH-001).
pub fn caller_allows_tool(agent: Agent, tool: &str, registry: &SpecialistRegistry) -> bool {
    match registry.resolve(agent) {
        Some(entry) => entry.allows(tool),
        None => false,
    }
}

// NOTE: `brief_for_task` reads a plan line's text to build a delegation brief
// (REQ-ORCH-005 one-task-per-call: the brief is self-contained so the subagent
// does not need the Manager's context). It is `pub` so the Manager turn loop in
// `src/agent/loop.rs` reuses the same plan-line → brief builder.
/// Regex matching a plan task line in the `- [ ] [t-xxx] description` format.
/// Compiled exactly once via `OnceLock` (CODE_REVIEW Point 2).
static TASK_LINE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

pub fn brief_for_task(plan: &Plan, task_id: &str) -> String {
    // Read-only diagnostic: build a self-contained brief from the plan task
    // text. If the plan line is present, its descriptive text becomes the
    // brief; otherwise fall back to a deterministic generic instruction.
    if let Ok(Some(content)) = plan.read() {
        let re = TASK_LINE_RE.get_or_init(|| {
            regex::Regex::new(r"(?m)^\s*-\s*\[\s*[ xX]?\s*\]\s*\[(t-[A-Za-z0-9_-]+)\]\s*(.*)$")
                .expect("valid task line regex")
        });
        for caps in re.captures_iter(&content) {
            if &caps[1] == task_id {
                let desc = caps[2].trim();
                if !desc.is_empty() {
                    return format!(
                        "{desc}\n\nExecute this delegated task to completion and return your \
                         deliverable, ending with MISSION COMPLETE ({task_id})."
                    );
                }
            }
        }
    }
    "Execute the delegated task described by the plan line, producing the
deliverable and ending with MISSION COMPLETE (task-id)."
        .to_string()
}
