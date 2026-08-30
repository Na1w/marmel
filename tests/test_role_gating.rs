//! Integration tests for the role-gated `dispatch_for`/`ToolCaller` path
//! (execution plan task t-d401).
//!
//! These tests prove that the orchestration tool policy (REQ-ORCH-001
//! Manager-only vs REQ-ORCH-002 per-specialist allowlist) is enforced in
//! production routing — i.e. that a tool invoked through the *incorrect* role
//! is rejected with `ToolError::Forbidden`, while the same tool invoked through
//! its *correct* role succeeds.
//!
//! They exercise the public `dispatch_for` entry point exactly as the
//! production call sites (`ui/mod.rs`, `agents/mod.rs`, `agent/loop.rs`) do,
//! so a regression in the gating logic is caught at the integration boundary.

use marmennill::agents::Agent;
use marmennill::harness::{ToolCaller, ToolError, ToolInvocation, dispatch_for};

/// REQ-ORCH-001: the Manager is forbidden from domain tools (`write_file`,
/// `replace`, `run_command`) — a domain tool invoked through the Manager role
/// must be rejected with `Forbidden`.
#[test]
fn manager_forbidden_from_domain_tools() {
    for name in ["write_file", "replace", "run_command"] {
        let tool = ToolInvocation {
            name: name.to_string(),
            arguments: serde_json::json!({}),
        };
        match dispatch_for(&tool, ToolCaller::Manager) {
            Err(ToolError::Forbidden { tool: t, caller }) => {
                assert_eq!(t, name, "forbidden tool name must match");
                assert_eq!(caller, "Manager", "caller must be reported as Manager");
            }
            other => panic!("Manager must be forbidden from `{name}`, got {other:?}"),
        }
    }
}

/// REQ-ORCH-001: the Manager IS permitted `delegate_task`, plan management
/// (`create_plan`, `archive_current_plan`), `rebirth`, and read-only
/// diagnostics (`read_file`, `grep_search`, `glob`).
#[test]
fn manager_permitted_planning_and_readonly_diagnostics() {
    // delegate_task is Manager-permitted (REQ-ORCH-005).
    let delegate = ToolInvocation {
        name: "delegate_task".to_string(),
        arguments: serde_json::json!({
            "agent_name": "coder",
            "prompt": "Implement the widget parser.",
            "task_id": "t-900"
        }),
    };
    assert!(
        dispatch_for(&delegate, ToolCaller::Manager).is_ok(),
        "Manager must be permitted delegate_task"
    );

    // Read-only diagnostic inspection is Manager-permitted.
    let read = ToolInvocation {
        name: "read_file".to_string(),
        arguments: serde_json::json!({ "path": "Cargo.toml" }),
    };
    assert!(
        dispatch_for(&read, ToolCaller::Manager).is_ok(),
        "Manager must be permitted read_file (read-only diagnostic)"
    );
}

/// REQ-ORCH-002: a specialist may NOT call a tool outside its registry
/// allowlist. Researcher has no `gedcom__*` namespace (only Validator does), so
/// a genealogy tool invoked through the Researcher role must be rejected.
#[test]
fn specialist_rejects_tool_outside_allowlist() {
    let tool = ToolInvocation {
        name: "gedcom__search".to_string(),
        arguments: serde_json::json!({ "query": "Smith" }),
    };
    match dispatch_for(&tool, ToolCaller::Specialist(Agent::Researcher)) {
        Err(ToolError::Forbidden { tool: t, caller }) => {
            assert_eq!(t, "gedcom__search");
            assert_eq!(caller, "researcher");
        }
        other => panic!("Researcher must be forbidden from gedcom__search, got {other:?}"),
    }
}

/// REQ-ORCH-002: a specialist IS permitted the tools its `terminal__*`
/// allowlist grants (bare `write_file` normalizes to `terminal__write_file`).
/// Coder's allowlist includes `terminal__*`, so `write_file` must succeed.
#[test]
fn specialist_permitted_allowlisted_terminal_tool() {
    let tmp = tempfile::tempdir().expect("creates tempdir");
    let test_file = tmp.path().join("test_write.rs");
    let tool = ToolInvocation {
        name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": test_file.to_str().unwrap(),
            "content": "fn main(){}"
        }),
    };
    assert!(
        dispatch_for(&tool, ToolCaller::Specialist(Agent::Coder)).is_ok(),
        "Coder must be permitted write_file via its terminal__* allowlist"
    );
}

/// REQ-ORCH-001: `create_plan` is Manager-only. Even DeepBrain (allowlist `*`)
/// must be forbidden from authoring the plan.
#[test]
fn create_plan_is_manager_only_even_for_wildcard_specialist() {
    let tool = ToolInvocation {
        name: "create_plan".to_string(),
        arguments: serde_json::json!({}),
    };
    match dispatch_for(&tool, ToolCaller::Specialist(Agent::Generalist)) {
        Err(ToolError::Forbidden { tool: t, .. }) => {
            assert_eq!(t, "create_plan");
        }
        other => {
            panic!("Generalist must be forbidden from create_plan (Manager-only), got {other:?}")
        }
    }
}

/// REQ-ORCH-002: a specialist may delegate only when its allowlist grants
/// `delegate_task` (all canonical roles grant it). Researcher's allowlist
/// includes `delegate_task`, so delegation must succeed.
#[test]
fn specialist_delegate_task_allowed_by_allowlist() {
    let tool = ToolInvocation {
        name: "delegate_task".to_string(),
        arguments: serde_json::json!({
            "agent_name": "validator",
            "prompt": "Verify the audit.",
            "task_id": "t-901"
        }),
    };
    assert!(
        dispatch_for(&tool, ToolCaller::Specialist(Agent::Researcher)).is_ok(),
        "Researcher must be permitted delegate_task via its allowlist"
    );
}
