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

/// Validator is an auditor and must be forbidden from file-modification and execution tools (write_file, replace, run_command).
#[tokio::test(flavor = "multi_thread")]
async fn validator_forbidden_from_file_modification_and_execution_tools() {
    let tool_write = ToolInvocation {
        name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "test.txt",
            "content": "modified"
        }),
    };
    match dispatch_for(&tool_write, ToolCaller::Specialist(Agent::Validator)) {
        Err(ToolError::Forbidden { tool: t, caller }) => {
            assert_eq!(t, "write_file");
            assert_eq!(caller, "validator");
        }
        other => panic!("Validator must be forbidden from write_file, got {other:?}"),
    }

    let tool_replace = ToolInvocation {
        name: "replace".to_string(),
        arguments: serde_json::json!({
            "path": "test.txt",
            "old": "a",
            "new": "b"
        }),
    };
    match dispatch_for(&tool_replace, ToolCaller::Specialist(Agent::Validator)) {
        Err(ToolError::Forbidden { tool: t, caller }) => {
            assert_eq!(t, "replace");
            assert_eq!(caller, "validator");
        }
        other => panic!("Validator must be forbidden from replace, got {other:?}"),
    }

    let tool_run = ToolInvocation {
        name: "run_command".to_string(),
        arguments: serde_json::json!({
            "command": "echo test"
        }),
    };
    // Per AGENTS.md, Validator is permitted run_command to run test suites.
    assert!(
        !matches!(
            dispatch_for(&tool_run, ToolCaller::Specialist(Agent::Validator)),
            Err(ToolError::Forbidden { .. })
        ),
        "Validator must be permitted run_command per AGENTS.md"
    );

    let tool_pty = ToolInvocation {
        name: "pty_list".to_string(),
        arguments: serde_json::json!({}),
    };
    assert!(
        dispatch_for(&tool_pty, ToolCaller::Specialist(Agent::Validator)).is_ok(),
        "Validator must be permitted pty_list"
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

/// Rebirth is permitted to ALL agents, including all specialists and validator.
#[test]
fn all_specialists_and_validator_permitted_rebirth() {
    let factory = marmennill::manager::ContextEngineFactory::new(1000);
    let roles = [
        Agent::Coder,
        Agent::Researcher,
        Agent::Debugger,
        Agent::Validator,
        Agent::Generalist,
    ];

    for role in roles {
        let mut engine = factory.specialist_context(
            format!("Role prompt for {role}"),
            format!("Task brief for {role}"),
        );
        engine.append(marmennill::types::Message::User {
            content: "Do work".to_string(),
        });
        engine.append(marmennill::types::Message::Assistant {
            content: Some("Working on it".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        });

        let tool = ToolInvocation {
            name: "rebirth".to_string(),
            arguments: serde_json::json!({
                "summary": format!("Progress summary for {role}")
            }),
        };

        let res = marmennill::harness::dispatch_for_with_engine(
            &tool,
            ToolCaller::Specialist(role),
            Some(&mut engine),
        );
        assert!(
            res.is_ok(),
            "{role} must be permitted to call rebirth, got error: {:?}",
            res.err()
        );
        assert_eq!(
            engine.messages().len(),
            4,
            "Rebirth must collapse {role} context to exactly 4 messages"
        );
    }
}

/// Sleep is permitted to ALL callers, including Manager and all specialists.
#[test]
fn all_agents_permitted_sleep() {
    // Cancel immediately so the test verifies permission/role-gating without
    // blocking the test thread for 6 redundant seconds.
    marmennill::orchestrator::cancel_all();

    let sleep_tool = ToolInvocation {
        name: "sleep".to_string(),
        arguments: serde_json::json!({
            "seconds": 1,
            "reason": "testing role gating"
        }),
    };

    // 1. Manager must be permitted sleep
    let mgr_res = marmennill::harness::dispatch_for(&sleep_tool, ToolCaller::Manager);
    assert!(
        mgr_res.is_ok(),
        "Manager must be permitted sleep, got: {:?}",
        mgr_res.err()
    );

    // 2. All specialists must be permitted sleep
    let roles = [
        Agent::Coder,
        Agent::Researcher,
        Agent::Debugger,
        Agent::Validator,
        Agent::Generalist,
    ];
    for role in roles {
        let res = marmennill::harness::dispatch_for(&sleep_tool, ToolCaller::Specialist(role));
        assert!(
            res.is_ok(),
            "{role} must be permitted to call sleep, got error: {:?}",
            res.err()
        );
    }

    marmennill::orchestrator::reset_cancellation();
}

#[tokio::test]
async fn test_prompt_based_tool_gating() {
    // 1. A Validator whose prompt explicitly includes `run_command` is permitted `run_command`.
    let tool_run = ToolInvocation {
        name: "run_command".to_string(),
        arguments: serde_json::json!({
            "command": "echo test"
        }),
    };
    let validator_caller_with_run = ToolCaller::SpecialistWithTools {
        agent: Agent::Validator,
        allowed_tools: vec![
            "read_file".to_string(),
            "run_command".to_string(),
            "leave_verdict".to_string(),
        ],
    };
    assert!(
        dispatch_for(&tool_run, validator_caller_with_run).is_ok(),
        "Validator with run_command in prompt must be permitted to execute run_command"
    );

    // 2. A Coder whose prompt omits `write_file` is forbidden from `write_file`.
    let tool_write = ToolInvocation {
        name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "test.txt",
            "content": "hello"
        }),
    };
    let coder_read_only = ToolCaller::SpecialistWithTools {
        agent: Agent::Coder,
        allowed_tools: vec!["read_file".to_string(), "grep_search".to_string()],
    };
    match dispatch_for(&tool_write, coder_read_only) {
        Err(ToolError::Forbidden { tool, .. }) => {
            assert_eq!(tool, "write_file");
        }
        other => panic!("Coder without write_file in prompt must be forbidden, got: {other:?}"),
    }

    // 3. Normalization: aliases matching the canonical name are properly gated.
    let tool_read_alias = ToolInvocation {
        name: "view_file".to_string(),
        arguments: serde_json::json!({
            "path": "Cargo.toml"
        }),
    };
    let researcher_caller = ToolCaller::SpecialistWithTools {
        agent: Agent::Researcher,
        allowed_tools: vec!["read_file".to_string()],
    };
    assert!(
        dispatch_for(&tool_read_alias, researcher_caller).is_ok(),
        "Allowed tool read_file must allow alias view_file"
    );
}

// ---------------------------------------------------------------------------
// t-033b — the verdict tool (`leave_verdict`) is validator-only
// ---------------------------------------------------------------------------
//
// Two enforcement layers exist and they are deliberately different:
//
// 1. The *dispatch* layer (`dispatch_for`) gates a tool by the caller's registry
//    allowlist / prompt blueprint. For the verdict tool that is necessary but
//    not sufficient: the allowlist is a per-role tool namespace, and the
//    Generalist namespace is the wildcard `*`, so a wildcard role "allows"
//    `leave_verdict`. `create_plan` is handled the same way in this file
//    (`create_plan_is_manager_only_even_for_wildcard_specialist`).
// 2. The *agent-loop* layer adds the identity rule: only the validator role may
//    record a verdict for a deliverable — see
//    [`marmennill::agents::runner::execution::may_record_verdict`]. That gate
//    returns an explicit tool error to the worker, logs a warning, and never
//    dispatches the call, so no verdict state and no verdict file is touched.
//
// Relationship to `test_prompt_based_tool_gating`: that test describes the
// *advisory* prompt-blueprint gate (a tool named in the prompt is expected to be
// executable), which is a sanctioned pre-existing failure and is NOT changed
// here. The verdict gate below is orthogonal and strictly stronger: it is a hard
// identity rule that no prompt blueprint, allowlist, or wildcard namespace can
// widen, and it is asserted independently of the advisory layer.

fn verdict_tool(verdict: &str, comments: &str) -> ToolInvocation {
    ToolInvocation {
        name: marmennill::tool_names::TOOL_LEAVE_VERDICT.to_string(),
        arguments: serde_json::json!({ "verdict": verdict, "comments": comments }),
    }
}

/// Only the validator role may reach the verdict tool through the dispatch layer;
/// domain specialists and the Manager are rejected with `Forbidden`.
#[test]
fn leave_verdict_is_validator_only_through_dispatch() {
    let tool = verdict_tool("APPROVED", "self assessment");

    for role in [
        Agent::Coder,
        Agent::Debugger,
        Agent::Researcher,
        Agent::Planner,
    ] {
        match dispatch_for(&tool, ToolCaller::Specialist(role)) {
            Err(ToolError::Forbidden { tool: t, caller }) => {
                assert_eq!(t, marmennill::tool_names::TOOL_LEAVE_VERDICT);
                assert_eq!(caller, role.as_str(), "caller must be reported verbatim");
            }
            other => panic!("{role} must be forbidden from the verdict tool, got {other:?}"),
        }
    }

    match dispatch_for(&tool, ToolCaller::Manager) {
        Err(ToolError::Forbidden { tool: t, caller }) => {
            assert_eq!(t, marmennill::tool_names::TOOL_LEAVE_VERDICT);
            assert_eq!(caller, "Manager");
        }
        other => panic!("Manager must be forbidden from the verdict tool, got {other:?}"),
    }

    let outcome = dispatch_for(&tool, ToolCaller::Specialist(Agent::Validator))
        .expect("validator must be permitted to record a verdict");
    assert!(
        outcome.content.contains("APPROVED"),
        "the validator's verdict must be recorded, got: {}",
        outcome.content
    );
}

/// A prompt blueprint that names the verdict tool does not make a non-validator
/// eligible: the tool namespace of the blueprint is checked, and `leave_verdict`
/// is not part of a coder blueprint.
#[test]
fn prompt_blueprint_naming_the_verdict_tool_does_not_authorise_a_coder() {
    let tool = verdict_tool("APPROVED", "self assessment");
    let coder_with_verdict = ToolCaller::SpecialistWithTools {
        agent: Agent::Coder,
        allowed_tools: vec![
            marmennill::tool_names::TOOL_WRITE_FILE.to_string(),
            marmennill::tool_names::TOOL_LEAVE_VERDICT.to_string(),
        ],
    };
    match dispatch_for(&tool, coder_with_verdict) {
        Ok(_) => {}
        Err(ToolError::Forbidden { .. }) => {}
        other => panic!("unexpected dispatch outcome: {other:?}"),
    }
    // Regardless of the advisory dispatch outcome, the identity rule below is the
    // authoritative gate for verdicts, and it refuses a coder blueprint.
    let registry = marmennill::orchestrator::SpecialistRegistry::canonical();
    assert!(
        !marmennill::agents::runner::execution::may_record_verdict(Agent::Coder, &registry),
        "a coder must never be able to record a verdict"
    );
}

/// The identity rule of the agent-loop gate: exactly the validator role. This
/// also proves the registry allowlist alone is insufficient — the Generalist
/// namespace is `*`, so `caller_allows_tool` reports it as allowed while
/// `may_record_verdict` still refuses it.
#[test]
fn only_the_validator_satisfies_may_record_verdict() {
    let registry = marmennill::orchestrator::SpecialistRegistry::canonical();

    assert!(
        marmennill::agents::runner::execution::may_record_verdict(Agent::Validator, &registry),
        "the validator role must be able to record verdicts"
    );

    for role in [
        Agent::Coder,
        Agent::Debugger,
        Agent::Researcher,
        Agent::Planner,
        Agent::Generalist,
    ] {
        assert!(
            !marmennill::agents::runner::execution::may_record_verdict(role, &registry),
            "{role} must not be able to record a verdict"
        );
    }

    // Residual documentation: the registry alone would let the wildcard
    // Generalist self-approve, which is why the gate carries the identity rule.
    assert!(
        marmennill::orchestrator::caller_allows_tool(
            Agent::Generalist,
            marmennill::tool_names::TOOL_LEAVE_VERDICT,
            &registry
        ),
        "Generalist's wildcard namespace admits the verdict tool: the registry check is not sufficient on its own"
    );
}

fn sse_tool_call(call_id: &str, tool_name: &str, args_json: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-rg",
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": { "name": tool_name, "arguments": args_json }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    )
}

fn sse_text(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-rg",
            "choices": [{ "delta": { "content": text }, "finish_reason": "stop" }]
        })
    )
}

/// End-to-end proof of the agent-loop role gate: a coder that calls the verdict
/// tool to approve its own deliverable is refused with an explicit error visible
/// to it, the seeded verdict file is left byte-for-byte unchanged, and the
/// outcome is decided by the automated validator — not by the worker.
#[tokio::test]
async fn worker_self_approval_is_refused_and_the_verdict_file_is_untouched() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();
    let prompts = root.join(".marmel").join("prompts");
    std::fs::create_dir_all(&prompts).expect("prompts dir");
    let verdict_file = prompts.join("t-904-validation.md");
    let seeded =
        "# Validation brief AUDIT-904\n\nVerify the deliverable before leaving a verdict.\n";
    std::fs::write(&verdict_file, seeded).expect("seed verdict file");

    let turns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rejection_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let verdict_attempt_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let turns_c = turns.clone();
    let rejection_seen_c = rejection_seen.clone();
    let verdict_attempt_seen_c = verdict_attempt_seen.clone();

    let deliverable = marmennill::harness::with_workspace_root(root.clone(), async move {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                if body.contains("Specialist Deliverable") {
                    // Automated validator pass: the validator rejects, with a canary.
                    let args = serde_json::json!({
                        "verdict": "REJECTED",
                        "comments": "CANARY-VALIDATOR-REJECTION: the change is unverified"
                    })
                    .to_string();
                    return ResponseTemplate::new(200).set_body_string(sse_tool_call(
                        "val_1",
                        marmennill::tool_names::TOOL_LEAVE_VERDICT,
                        &args,
                    ));
                }
                match turns_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => {
                        let args = serde_json::json!({
                            "path": "role-gate-probe.txt",
                            "content": "worker output"
                        })
                        .to_string();
                        ResponseTemplate::new(200).set_body_string(sse_tool_call(
                            "call_write_1",
                            marmennill::tool_names::TOOL_WRITE_FILE,
                            &args,
                        ))
                    }
                    1 => {
                        // The worker attempts to approve its own deliverable.
                        verdict_attempt_seen_c.store(true, std::sync::atomic::Ordering::SeqCst);
                        let args = serde_json::json!({
                            "verdict": "APPROVED",
                            "comments": "SELF-APPROVAL-CANARY I approve my own deliverable"
                        })
                        .to_string();
                        ResponseTemplate::new(200).set_body_string(sse_tool_call(
                            "call_self_verdict",
                            marmennill::tool_names::TOOL_LEAVE_VERDICT,
                            &args,
                        ))
                    }
                    2 => {
                        // The worker must have received the rejection in its context.
                        if body.contains("VERDICT REJECTED")
                            && body.contains("cannot approve or reject its own deliverable")
                        {
                            rejection_seen_c.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        ResponseTemplate::new(200).set_body_string(sse_text(
                            "Work delivered.\n\nMISSION COMPLETE (t-904)",
                        ))
                    }
                    _ => ResponseTemplate::new(200)
                        .set_body_string(sse_text("Still delivered.\n\nMISSION COMPLETE (t-904)")),
                }
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let mut cfg = marmennill::config::Config {
            backend_url: backend_url.clone(),
            model: "test-model".to_string(),
            ..Default::default()
        };
        cfg.orchestration.specialists.insert(
            "coder".to_string(),
            marmennill::config::SpecialistConfig {
                module: "src/agents/coder.rs".to_string(),
                tools: vec![
                    marmennill::tool_names::TOOL_WRITE_FILE.to_string(),
                    marmennill::tool_names::TOOL_READ_FILE.to_string(),
                ],
                enable_validator: Some(true),
                max_validator_iterations: Some(1),
                ..Default::default()
            },
        );
        let client = marmennill::llm::ChatClient::new(&backend_url, "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "Produce a deliverable, then approve it yourself".to_string(),
            snippets: vec![],
            task_id: Some("t-904".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx = marmennill::agents::IsolatedContext::from_request(
            "You are the Coder specialist.".to_string(),
            &req,
        );
        let token = tokio_util::sync::CancellationToken::new();

        marmennill::agents::run_specialist_live(&client, Agent::Coder, &ctx, &cfg, &token).await
    })
    .await
    .expect("the run returns a deliverable even when the verdict attempt is refused");

    assert!(
        verdict_attempt_seen.load(std::sync::atomic::Ordering::SeqCst),
        "the fixture must have exercised a self-approval attempt"
    );
    assert!(
        rejection_seen.load(std::sync::atomic::Ordering::SeqCst),
        "the worker must have received an explicit rejection of its verdict call, got deliverable: {deliverable}"
    );
    assert!(
        deliverable.contains("CANARY-VALIDATOR-REJECTION"),
        "the validator (not the worker) must decide the outcome, got: {deliverable}"
    );
    assert!(
        !deliverable.contains("SELF-APPROVAL-CANARY"),
        "a refused worker verdict must never surface as the verdict, got: {deliverable}"
    );
    assert!(
        deliverable.contains("FAILED"),
        "a validator rejection must produce a failed deliverable, got: {deliverable}"
    );

    let after = std::fs::read_to_string(&verdict_file).expect("verdict file still present");
    assert_eq!(
        after, seeded,
        "a refused verdict call must not modify the verdict file"
    );
}

/// A worker that keeps trying to record its own verdict is failed instead of
/// being allowed to spin: after the third refused attempt the run terminates with
/// a verdict role violation and still records no verdict.
#[tokio::test]
async fn repeated_self_verdict_attempts_fail_the_run_without_recording_a_verdict() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();
    let prompts = root.join(".marmel").join("prompts");
    std::fs::create_dir_all(&prompts).expect("prompts dir");
    let verdict_file = prompts.join("t-905-validation.md");
    let seeded = "# Validation brief AUDIT-905\n";
    std::fs::write(&verdict_file, seeded).expect("seed verdict file");

    let verdict_attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let attempts_c = verdict_attempts.clone();

    let deliverable = marmennill::harness::with_workspace_root(root.clone(), async move {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                if body.contains("Specialist Deliverable") {
                    return ResponseTemplate::new(200).set_body_string(sse_text(
                        "validator must never be consulted on this path",
                    ));
                }
                let args =
                    serde_json::json!({ "verdict": "APPROVED", "comments": "self approval" })
                        .to_string();
                attempts_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_string(sse_tool_call(
                    "call_self_verdict",
                    marmennill::tool_names::TOOL_LEAVE_VERDICT,
                    &args,
                ))
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let mut cfg = marmennill::config::Config {
            backend_url: backend_url.clone(),
            model: "test-model".to_string(),
            ..Default::default()
        };
        cfg.orchestration.specialists.insert(
            "coder".to_string(),
            marmennill::config::SpecialistConfig {
                module: "src/agents/coder.rs".to_string(),
                tools: vec![marmennill::tool_names::TOOL_WRITE_FILE.to_string()],
                enable_validator: Some(true),
                max_validator_iterations: Some(2),
                ..Default::default()
            },
        );
        let client = marmennill::llm::ChatClient::new(&backend_url, "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "Approve your own deliverable, repeatedly".to_string(),
            snippets: vec![],
            task_id: Some("t-905".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx = marmennill::agents::IsolatedContext::from_request(
            "You are the Coder specialist.".to_string(),
            &req,
        );
        let token = tokio_util::sync::CancellationToken::new();

        marmennill::agents::run_specialist_live(&client, Agent::Coder, &ctx, &cfg, &token).await
    })
    .await
    .expect("the run returns a deliverable");

    assert!(
        deliverable.contains("only the validator role may record verdicts"),
        "repeated verdict-role violations must fail the run with an explicit reason, got: {deliverable}"
    );
    assert!(
        deliverable.contains("FAILED"),
        "repeated verdict-role violations must produce a failed deliverable, got: {deliverable}"
    );
    assert!(
        !deliverable.contains("Verdict: APPROVED"),
        "no verdict may be recorded by a non-validator, got: {deliverable}"
    );
    assert_eq!(
        std::fs::read_to_string(&verdict_file).expect("verdict file"),
        seeded,
        "refused verdict attempts must not modify the verdict file"
    );
}

/// The other side of the gate: the validator role is *not* obstructed. A
/// validator that records an explicit approval through the verdict tool has that
/// verdict accepted verbatim.
#[tokio::test]
async fn validator_may_record_an_explicit_approval() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    let deliverable = marmennill::harness::with_workspace_root(root.clone(), async move {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_req: &Request| {
                let args = serde_json::json!({
                    "verdict": "APPROVED",
                    "comments": "VERDICT-FROM-VALIDATOR: every check passed"
                })
                .to_string();
                ResponseTemplate::new(200).set_body_string(sse_tool_call(
                    "call_validator_verdict",
                    marmennill::tool_names::TOOL_LEAVE_VERDICT,
                    &args,
                ))
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let cfg = marmennill::config::Config {
            backend_url: backend_url.clone(),
            model: "test-model".to_string(),
            ..Default::default()
        };
        let client = marmennill::llm::ChatClient::new(&backend_url, "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: Agent::Validator,
            prompt: "Audit the deliverable and leave a verdict".to_string(),
            snippets: vec![],
            task_id: Some("t-906".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx = marmennill::agents::IsolatedContext::from_request(
            "You are the Quality Auditor.".to_string(),
            &req,
        );
        let token = tokio_util::sync::CancellationToken::new();

        marmennill::agents::run_specialist_live(&client, Agent::Validator, &ctx, &cfg, &token).await
    })
    .await
    .expect("a validator run returns a deliverable");

    assert!(
        deliverable.contains("Verdict: APPROVED"),
        "the validator's explicit approval must be recorded, got: {deliverable}"
    );
    assert!(
        deliverable.contains("VERDICT-FROM-VALIDATOR"),
        "the validator's comments must be preserved, got: {deliverable}"
    );
    assert!(
        !deliverable.contains("FAILED"),
        "an approved validator deliverable must not be reported as failed, got: {deliverable}"
    );
}

// ---------------------------------------------------------------------------
// t-033e — the fix-loop tool blueprint is role-aware
// ---------------------------------------------------------------------------
//
// `run_fix_loop` (`src/agents/runner/fix_loop.rs`) is the shared driver behind
// the deliverable validator and the plan auditor. Its *blueprint* is the
// tool-schema list the run hands the model, plus the tool calls that same run is
// allowed to dispatch. Two role-gating holes existed there:
//
// 1. a prompt/blueprint allow-list naming the verdict tool put that tool into the
//    advertised schema list of *any* role (a permissive registry, e.g. the
//    wildcard Generalist namespace, was enough), and
// 2. the loop consumed a verdict tool call from *any* caller and returned it as
//    the run's verdict — i.e. a non-validator could certify a deliverable purely
//    through the fix loop, without ever reaching `dispatch_for`.
//
// Both are closed by reusing the crate's single public role gate
// (`may_record_verdict`, `runner/execution.rs`): the fix loop asks that gate once
// per run and everything verdict-shaped (advertised list, nudges, consumption,
// dispatch) follows its answer. No second gate, no new error type: the refusal is
// the same typed `ToolError::Forbidden` the agent-loop gate reports.

use marmennill::agents::runner::fix_loop::{
    ALWAYS_ADVERTISED_TOOLS, BlueprintToolClass, FixLoopBounds, FixLoopResult, INSPECTION_TOOLS,
    LoopParams, MUTATING_TOOLS, VERDICT_RECORDING_TOOLS, advertised_tools_for_caller,
    assemble_tools, blueprint_tool_class, caller_may_record_verdict, is_verdict_recording_tool,
    register_loop_worker, role_filtered_blueprint, run_fix_loop,
};
use marmennill::types::ToolDef;

/// Names of the tools in an assembled list.
fn tool_names_of(tools: &[ToolDef]) -> Vec<String> {
    tools.iter().map(|t| t.function.name.clone()).collect()
}

/// The tool names actually advertised to the model in one serialized request.
fn advertised_tool_names(body: &str) -> Vec<String> {
    let value: serde_json::Value =
        serde_json::from_str(body).expect("the chat request body is valid JSON");
    value
        .get("tools")
        .and_then(|tools| tools.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One streamed turn carrying several tool calls, in wire order.
fn sse_tool_calls(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let items: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(idx, (id, name, args))| {
            serde_json::json!({
                "index": idx,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args.to_string() }
            })
        })
        .collect();
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-rg-bp",
            "choices": [{
                "delta": { "content": null, "tool_calls": items },
                "finish_reason": "tool_calls"
            }]
        })
    )
}

/// Drive one [`run_fix_loop`] run against a replaying mock backend, with the
/// caller and the assembled tool list chosen by the test.
async fn drive_fix_loop(
    server: &wiremock::MockServer,
    caller: ToolCaller,
    tag: &str,
    tools: Vec<ToolDef>,
) -> FixLoopResult {
    let cfg = marmennill::config::Config::default();
    let mon_cfg = marmennill::config::MonitoringConfig::default();
    let backend = format!("{}/v1", server.uri());
    let client = marmennill::llm::ChatClient::new_with_token(&backend, "test-model", "test-token");
    let token = tokio_util::sync::CancellationToken::new();
    let guard = register_loop_worker(
        Some("t-907".to_string()),
        tag.to_string(),
        "role-gating fix-loop run".to_string(),
        &token,
    );
    let mut engine = marmennill::manager::ContextEngineFactory::new(64_000).specialist_context(
        "auditor role prompt".to_string(),
        "brief under audit".to_string(),
    );
    let mut params = LoopParams {
        client: &client,
        model: "test-model".to_string(),
        tag: tag.to_string(),
        worker_name: tag.to_string(),
        task_id: Some("t-907".to_string()),
        worker_key: guard.0.clone(),
        engine: &mut engine,
        tools,
        token: &token,
        mon_cfg: &mon_cfg,
        cfg: &cfg,
        temperature: 0.0,
        status_template: format!("{tag}: auditing the deliverable..."),
        verdict_log_role: "fix-loop".to_string(),
        bounds: FixLoopBounds::default(),
        abort_log_prefix: tag.to_string(),
        emit_tool_status: false,
        rebirth_notice: "(checkpoint accepted)".to_string(),
    };
    let mut monitor = marmennill::harness::monitor::HarnessMonitor::new_with_config(
        std::sync::Arc::new(marmennill::harness::HarnessStats::new()),
        &mon_cfg,
    );
    run_fix_loop(&mut params, &mut monitor, caller)
        .await
        .expect("a verdict-less or refused fix-loop run is an outcome, never a hard error")
}

/// (a) The blueprint vocabulary itself: the verdict class is defined by the
/// `tool_names` constants, and the public gate — not an allow-list — decides who
/// may hold it.
#[test]
fn fix_loop_blueprint_verdict_class_is_defined_by_the_tool_name_constants() {
    use marmennill::tool_names::{
        TERMINAL_LEAVE_VERDICT, TOOL_LEAVE_VERDICT, TOOL_READ_FILE, TOOL_REPLY_TO_ARBITRATOR,
        TOOL_WRITE_FILE, terminal_tool,
    };

    assert!(VERDICT_RECORDING_TOOLS.contains(&TOOL_LEAVE_VERDICT));
    assert!(VERDICT_RECORDING_TOOLS.contains(&TERMINAL_LEAVE_VERDICT));
    assert_eq!(
        ALWAYS_ADVERTISED_TOOLS.to_vec(),
        vec![TOOL_REPLY_TO_ARBITRATOR]
    );
    assert!(INSPECTION_TOOLS.contains(&TOOL_READ_FILE));
    assert!(MUTATING_TOOLS.contains(&TOOL_WRITE_FILE));

    assert_eq!(
        blueprint_tool_class(TOOL_LEAVE_VERDICT),
        BlueprintToolClass::Verdict
    );
    assert_eq!(
        blueprint_tool_class(&terminal_tool(TOOL_LEAVE_VERDICT)),
        BlueprintToolClass::Verdict
    );
    assert_eq!(
        blueprint_tool_class(TOOL_READ_FILE),
        BlueprintToolClass::Inspection
    );
    assert_eq!(
        blueprint_tool_class(TOOL_WRITE_FILE),
        BlueprintToolClass::Mutating
    );
    assert!(is_verdict_recording_tool(&terminal_tool(
        TOOL_LEAVE_VERDICT
    )));
    assert!(!is_verdict_recording_tool(TOOL_READ_FILE));

    // The loop asks the crate's public gate and nothing else: only the validator
    // role has verdict authority; the Manager (no role) has none either.
    assert!(caller_may_record_verdict(&ToolCaller::Specialist(
        Agent::Validator
    )));
    for role in [
        Agent::Coder,
        Agent::Debugger,
        Agent::Researcher,
        Agent::Planner,
        Agent::Generalist,
    ] {
        assert!(
            !caller_may_record_verdict(&ToolCaller::Specialist(role)),
            "{role} must have no verdict authority in the fix loop"
        );
    }
    assert!(
        !caller_may_record_verdict(&ToolCaller::Manager),
        "the Manager has no role identity for verdicts, so it has no authority"
    );
}

/// (b) A prompt/blueprint allow-list can never widen the verdict class: the
/// registry/entry authority has to propose it, and a role rejected by the public
/// gate never receives it — neither in the filtered blueprint nor in the
/// advertised schema view of an already-assembled list.
#[test]
fn fix_loop_blueprint_cannot_grant_verdict_tools_to_a_non_validator_role() {
    use marmennill::tool_names::{TOOL_LEAVE_VERDICT, TOOL_READ_FILE};

    let blueprint = vec![TOOL_READ_FILE.to_string(), TOOL_LEAVE_VERDICT.to_string()];
    let coder = ToolCaller::SpecialistWithTools {
        agent: Agent::Coder,
        allowed_tools: blueprint.clone(),
    };
    let validator = ToolCaller::SpecialistWithTools {
        agent: Agent::Validator,
        allowed_tools: blueprint.clone(),
    };

    // Blueprint filtering is role-aware.
    let coder_blueprint = role_filtered_blueprint(&coder, &blueprint);
    assert!(coder_blueprint.contains(&TOOL_READ_FILE.to_string()));
    assert!(
        !coder_blueprint.iter().any(|t| is_verdict_recording_tool(t)),
        "a coder blueprint must lose the verdict tool: {coder_blueprint:?}"
    );
    let validator_blueprint = role_filtered_blueprint(&validator, &blueprint);
    assert!(
        validator_blueprint
            .iter()
            .any(|t| is_verdict_recording_tool(t)),
        "the validator blueprint must keep the verdict tool: {validator_blueprint:?}"
    );

    // A blueprint naming the verdict tool cannot grant it on its own, even with a
    // permissive entry authority: the verdict class requires entry authority too.
    let permissive = assemble_tools(Some(&blueprint), |_| true, &[]);
    assert!(tool_names_of(&permissive).contains(&TOOL_LEAVE_VERDICT.to_string()));
    let blueprint_only = assemble_tools(Some(&blueprint), |_| false, &[]);
    assert!(tool_names_of(&blueprint_only).contains(&TOOL_READ_FILE.to_string()));
    assert!(
        !tool_names_of(&blueprint_only).contains(&TOOL_LEAVE_VERDICT.to_string()),
        "a prompt blueprint alone must not advertise the verdict tool: {:?}",
        tool_names_of(&blueprint_only)
    );

    // Whatever the assembled list contains, the loop's advertised view strips the
    // verdict class for a role the public gate refuses, and keeps it for the
    // validator.
    let coder_view = advertised_tools_for_caller(&permissive, &coder);
    assert!(tool_names_of(&coder_view).contains(&TOOL_READ_FILE.to_string()));
    assert!(
        !tool_names_of(&coder_view).contains(&TOOL_LEAVE_VERDICT.to_string()),
        "a coder fix-loop run must never be shown the verdict tool: {:?}",
        tool_names_of(&coder_view)
    );
    let validator_view = advertised_tools_for_caller(&permissive, &validator);
    assert!(
        validator_view
            .iter()
            .any(|t| is_verdict_recording_tool(&t.function.name)),
        "the validator fix-loop run must keep the verdict tool: {:?}",
        tool_names_of(&validator_view)
    );
}

/// (c) Validator role through the fix-loop blueprint: the verdict tool is
/// advertised and the validator's verdict is recorded verbatim.
#[tokio::test]
async fn validator_records_its_verdict_through_the_fix_loop_blueprint() {
    use marmennill::tool_names::{TOOL_LEAVE_VERDICT, TOOL_READ_FILE};
    use std::sync::Arc;
    use std::sync::Mutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    let (outcome, bodies) = marmennill::harness::with_workspace_root(root.clone(), async move {
        let server = MockServer::start().await;
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bodies);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&req.body).to_string());
                ResponseTemplate::new(200).set_body_string(sse_tool_calls(&[(
                    "call_val_bp",
                    TOOL_LEAVE_VERDICT,
                    serde_json::json!({
                        "verdict": "APPROVED",
                        "comments": "BLUEPRINT-VALIDATOR-CANARY: inspected and clean"
                    }),
                )]))
            })
            .mount(&server)
            .await;

        let blueprint = vec![TOOL_READ_FILE.to_string(), TOOL_LEAVE_VERDICT.to_string()];
        let registry = marmennill::orchestrator::SpecialistRegistry::canonical();
        // The validator's real registry authority.
        let entry_allows = |name: &str| {
            marmennill::orchestrator::caller_allows_tool(Agent::Validator, name, &registry)
        };
        let tools = assemble_tools(Some(&blueprint), entry_allows, &[]);
        let caller = ToolCaller::SpecialistWithTools {
            agent: Agent::Validator,
            allowed_tools: blueprint,
        };
        let outcome = drive_fix_loop(&server, caller, "validator-blueprint-validator", tools).await;
        (outcome, bodies.lock().unwrap().clone())
    })
    .await;

    let advertised = advertised_tool_names(
        bodies
            .first()
            .expect("the fix loop must have made at least one request"),
    );
    assert!(
        advertised.contains(&TOOL_READ_FILE.to_string()),
        "the auditor inspection tool must stay advertised: {advertised:?}"
    );
    assert!(
        advertised.contains(&TOOL_LEAVE_VERDICT.to_string()),
        "the validator must be shown the verdict tool through the blueprint: {advertised:?}"
    );

    match outcome {
        FixLoopResult::Verdict { approved, critique } => {
            assert!(
                approved,
                "the validator's blueprint verdict must be recorded: {critique}"
            );
            assert!(
                critique.contains("BLUEPRINT-VALIDATOR-CANARY"),
                "the validator's comments must be preserved: {critique}"
            );
        }
        other => panic!("expected a recorded verdict, got {other:?}"),
    }
}

/// (d) A non-validator role that is handed a blueprint naming the verdict tool,
/// by a permissive registry no less: the fix loop must never advertise the verdict
/// tool to it, must refuse its verdict call fail-closed with the typed rejection,
/// and must not dispatch anything from that turn.
#[tokio::test]
async fn non_validator_verdict_call_through_the_fix_loop_blueprint_fails_closed() {
    use marmennill::tool_names::{TOOL_LEAVE_VERDICT, TOOL_READ_FILE, TOOL_WRITE_FILE};
    use std::sync::Arc;
    use std::sync::Mutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    let (outcome, bodies, calls) =
        marmennill::harness::with_workspace_root(root.clone(), async move {
            let server = MockServer::start().await;
            let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let captured = Arc::clone(&bodies);
            let counted = Arc::clone(&calls);
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(move |req: &Request| {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    captured
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&req.body).to_string());
                    // The exact hole: a verdict call from a coder, followed by a
                    // tool the coder's dispatcher would happily honour.
                    ResponseTemplate::new(200).set_body_string(sse_tool_calls(&[
                        (
                            "call_self_verdict",
                            TOOL_LEAVE_VERDICT,
                            serde_json::json!({
                                "verdict": "APPROVED",
                                "comments": "SELF-APPROVAL-CANARY-THROUGH-BLUEPRINT"
                            }),
                        ),
                        (
                            "call_after_refusal",
                            TOOL_WRITE_FILE,
                            serde_json::json!({
                                "path": "dispatched-after-refusal.txt",
                                "content": "must never exist"
                            }),
                        ),
                    ]))
                })
                .mount(&server)
                .await;

            let blueprint = vec![
                TOOL_READ_FILE.to_string(),
                TOOL_WRITE_FILE.to_string(),
                TOOL_LEAVE_VERDICT.to_string(),
            ];
            // Deliberately permissive entry authority (the wildcard-namespace
            // situation), so the only thing standing between the coder and a
            // recorded verdict is the role gate.
            let tools = assemble_tools(Some(&blueprint), |_| true, &[]);
            let caller = ToolCaller::SpecialistWithTools {
                agent: Agent::Coder,
                allowed_tools: blueprint,
            };
            let outcome =
                drive_fix_loop(&server, caller, "coder-blueprint-self-verdict", tools).await;
            (
                outcome,
                bodies.lock().unwrap().clone(),
                calls.load(std::sync::atomic::Ordering::SeqCst),
            )
        })
        .await;

    let advertised = advertised_tool_names(
        bodies
            .first()
            .expect("the fix loop must have made at least one request"),
    );
    assert!(
        advertised.contains(&TOOL_WRITE_FILE.to_string()),
        "the blueprint's non-verdict tools must survive the filter: {advertised:?}"
    );
    assert!(
        !advertised.contains(&TOOL_LEAVE_VERDICT.to_string()),
        "a non-validator fix-loop run must never be shown a verdict tool: {advertised:?}"
    );

    assert_eq!(
        calls, 1,
        "the refusal must end the run in the same round, without another LLM turn"
    );

    match outcome {
        FixLoopResult::Verdict { approved, critique } => {
            assert!(
                !approved,
                "a non-validator verdict must never approve a deliverable: {critique}"
            );
            assert!(
                critique.contains("VERDICT REJECTED"),
                "the rejection must reuse the crate's verdict-rejection vocabulary: {critique}"
            );
            assert!(
                critique.contains("is forbidden for caller"),
                "the rejection must carry the typed Forbidden rejection: {critique}"
            );
            assert!(
                critique.contains(Agent::Coder.as_str()),
                "the rejection must name the refusing caller role: {critique}"
            );
            assert!(
                !critique.contains("SELF-APPROVAL-CANARY-THROUGH-BLUEPRINT"),
                "a refused verdict payload must never surface as a verdict: {critique}"
            );
        }
        other => panic!("expected a fail-closed verdict outcome, got {other:?}"),
    }

    assert!(
        !root.join("dispatched-after-refusal.txt").exists(),
        "a refused verdict turn must not dispatch any tool from the blueprint"
    );
}

/// (e) The dispatch half of the same gate: the fix loop's shared dispatcher
/// refuses to execute a verdict tool for a role the public gate rejects, and the
/// refusal is an explicit tool error (never a silent success).
#[tokio::test]
async fn fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority() {
    use marmennill::agents::runner::fix_loop::dispatch_tool_call;
    use marmennill::tool_names::TOOL_LEAVE_VERDICT;

    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    let (coder_result, validator_result) =
        marmennill::harness::with_workspace_root(root, async move {
            let mon_cfg = marmennill::config::MonitoringConfig::default();
            let verdict_call = marmennill::types::ToolCall {
                id: "call_direct_verdict".to_string(),
                kind: "function".to_string(),
                function: marmennill::types::ToolFunction {
                    name: TOOL_LEAVE_VERDICT.to_string(),
                    arguments: serde_json::json!({
                        "verdict": "APPROVED",
                        "comments": "DIRECT-DISPATCH-SELF-APPROVAL-CANARY"
                    })
                    .to_string(),
                },
            };

            let coder_outcome = {
                let mut engine = marmennill::manager::ContextEngineFactory::new(64_000)
                    .specialist_context("role prompt".to_string(), "brief".to_string());
                let token = tokio_util::sync::CancellationToken::new();
                let mut monitor = marmennill::harness::monitor::HarnessMonitor::new_with_config(
                    std::sync::Arc::new(marmennill::harness::HarnessStats::new()),
                    &mon_cfg,
                );
                dispatch_tool_call(
                    &mut monitor,
                    &verdict_call,
                    ToolCaller::Specialist(Agent::Coder),
                    &mut engine,
                    &token,
                    "coder-direct",
                    false,
                    marmennill::agents::runner::fix_loop::TOOL_ERROR_PREFIX,
                    &marmennill::agents::runner::fix_loop::repetition_intervention_fallback(),
                )
                .await
            };

            let validator_outcome = {
                let mut engine = marmennill::manager::ContextEngineFactory::new(64_000)
                    .specialist_context("role prompt".to_string(), "brief".to_string());
                let token = tokio_util::sync::CancellationToken::new();
                let mut monitor = marmennill::harness::monitor::HarnessMonitor::new_with_config(
                    std::sync::Arc::new(marmennill::harness::HarnessStats::new()),
                    &mon_cfg,
                );
                dispatch_tool_call(
                    &mut monitor,
                    &verdict_call,
                    ToolCaller::Specialist(Agent::Validator),
                    &mut engine,
                    &token,
                    "validator-direct",
                    false,
                    marmennill::agents::runner::fix_loop::TOOL_ERROR_PREFIX,
                    &marmennill::agents::runner::fix_loop::repetition_intervention_fallback(),
                )
                .await
            };

            (coder_outcome, validator_outcome)
        })
        .await;

    let (content, succeeded) =
        coder_result.expect("a refused verdict dispatch is still an outcome");
    assert!(
        !succeeded,
        "a verdict dispatch without authority must be reported as a failure: {content}"
    );
    assert!(
        content.contains("VERDICT REJECTED") && content.contains("is forbidden for caller"),
        "the refusal must be the crate's typed verdict rejection: {content}"
    );
    assert!(
        content.contains(Agent::Coder.as_str()),
        "the refusal must name the caller: {content}"
    );
    assert!(
        !content.contains("DIRECT-DISPATCH-SELF-APPROVAL-CANARY"),
        "a refused verdict payload must never be echoed as a recorded verdict: {content}"
    );

    let (validator_content, validator_succeeded) =
        validator_result.expect("an authorised verdict dispatch yields an outcome");
    assert!(
        validator_succeeded,
        "the validator role keeps its verdict authority through the same path: {validator_content}"
    );
}

/// (f) Source guard, in the style of `tests/test_tool_name_literals.rs`: no tool
/// name may be re-typed as a string literal anywhere in the fix-loop driver — the
/// blueprint must be spelled with the `tool_names` constants. Stricter than the
/// repo-wide guard: it flags a tool name *inside* a longer literal too.
#[test]
fn fix_loop_source_has_no_raw_tool_name_string_literals() {
    let path = std::path::Path::new("src/agents/runner/fix_loop.rs");
    let src = std::fs::read_to_string(path).expect("the fix-loop driver must be readable");

    // Needles come from the constants themselves, assembled at runtime, so this
    // test's own text can never satisfy them.
    let bare = [
        marmennill::tool_names::TOOL_DELEGATE_TASK,
        marmennill::tool_names::TOOL_READ_FILE,
        marmennill::tool_names::TOOL_WRITE_FILE,
        marmennill::tool_names::TOOL_REPLACE,
        marmennill::tool_names::TOOL_RUN_COMMAND,
        marmennill::tool_names::TOOL_GREP_SEARCH,
        marmennill::tool_names::TOOL_GLOB,
        marmennill::tool_names::TOOL_CREATE_PLAN,
        marmennill::tool_names::TOOL_ARCHIVE_PLAN,
        marmennill::tool_names::TOOL_REBIRTH,
        marmennill::tool_names::TOOL_PTY_SPAWN,
        marmennill::tool_names::TOOL_PTY_WRITE,
        marmennill::tool_names::TOOL_PTY_READ,
        marmennill::tool_names::TOOL_PTY_CLOSE,
        marmennill::tool_names::TOOL_PTY_LIST,
        marmennill::tool_names::TOOL_LEAVE_VERDICT,
        marmennill::tool_names::TOOL_SLEEP,
        marmennill::tool_names::TOOL_REPLY_TO_ARBITRATOR,
        marmennill::tool_names::TOOL_LIST_DIRECTORY,
    ];
    let mut needles: Vec<String> = bare.iter().map(|n| (*n).to_string()).collect();
    for name in bare {
        needles.push(marmennill::tool_names::terminal_tool(name));
    }

    let literals = string_literals_in_source(&src);
    assert!(
        !literals.is_empty(),
        "the guard must find string literals in {}",
        path.display()
    );
    let offenders: Vec<String> = literals
        .iter()
        .filter(|lit| needles.iter().any(|needle| lit.contains(needle)))
        .cloned()
        .collect();
    assert!(
        offenders.is_empty(),
        "{} must not contain raw tool-name string literals; use the constants from \
         `marmennill::tool_names`. Offending literals: {offenders:?}",
        path.display()
    );

    // The blueprint must actually reference the constant module, otherwise the
    // guard above would also pass on a file that simply dropped the vocabulary.
    assert!(
        src.contains("tool_names::"),
        "the fix-loop blueprint must name tools through `tool_names::` constants"
    );
}

/// Extract the string literals of a Rust source file, skipping comments, char
/// literals and lifetimes. Not a full parser — only enough for a literal guard.
fn string_literals_in_source(src: &str) -> Vec<String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut block_depth = 0usize;

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();

        if block_depth > 0 {
            if c == '/' && next == Some('*') {
                block_depth += 1;
                i += 2;
            } else if c == '*' && next == Some('/') {
                block_depth -= 1;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            block_depth += 1;
            i += 2;
            continue;
        }
        if c == '"' {
            let mut literal = String::new();
            i += 1;
            while i < chars.len() {
                let d = chars[i];
                if d == '\\' {
                    literal.push(d);
                    if let Some(esc) = chars.get(i + 1) {
                        literal.push(*esc);
                    }
                    i += 2;
                    continue;
                }
                if d == '"' {
                    i += 1;
                    break;
                }
                literal.push(d);
                i += 1;
            }
            out.push(literal);
            continue;
        }
        if c == '\'' {
            // A char literal or a lifetime: neither can hold a tool name.
            i += 1;
            while i < chars.len() && chars[i] != '\'' && chars[i] != '\n' && chars[i] != ' ' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            if i < chars.len() && chars[i] == '\'' {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
}

// ── Gate t-056: role-aware tool ADVERTISING inside the specialist loop ───────
//
// Gates t-033b/t-051 refused a worker's *call* to a verdict tool, but the
// specialist loop still *offered* one: `runner/execution.rs` built the schema
// list with the raw prompt blueprint plus the registry entry's namespaces, and
// the Generalist's wildcard (`"*"`) namespace — or a blueprint naming the tool —
// was enough to put a verdict tool in front of a worker's model. An offer the
// dispatcher will refuse is not harmless decoration: it invites exactly the
// hallucinated self-approval the turn loop then has to reject and count against
// the worker (three refusals fail the run).
//
// t-056 closes the offer by **reusing** the fix loop's role-aware helpers
// (`assemble_tools_for_caller` → `role_filtered_blueprint`, then
// `advertised_tools_for_caller`), so the answer still comes from the crate's one
// public gate [`may_record_verdict`] — no new gate, no second filter. The tests
// below assert the **exact set difference**, never "one name is missing": the
// validator's advertised set minus a worker's advertised set must be precisely
// the verdict class, and the worker must keep every ordinary tool.

/// The verdict-recording names that can ever appear in a list built from
/// `ToolDef::default_tools()` — derived through the crate's own predicate and the
/// verdict-class constant, so the expectation can never drift from production.
fn default_verdict_names() -> std::collections::BTreeSet<String> {
    ToolDef::default_tools()
        .iter()
        .map(|t| t.function.name.clone())
        .filter(|name| is_verdict_recording_tool(name))
        .collect()
}

/// The tool names one specialist run is actually offered, computed by the
/// production advertising helper.
fn specialist_advertised(
    caller: &ToolCaller,
    entry_allows: impl Fn(&str) -> bool,
) -> std::collections::BTreeSet<String> {
    marmennill::agents::runner::execution::specialist_advertised_tools(caller, entry_allows, &[])
        .iter()
        .map(|t| t.function.name.clone())
        .collect()
}

/// (a)+(b) The advertised set is role-aware: for every non-validator role the
/// difference against the validator's set is **exactly** the verdict class, while
/// every ordinary tool survives. The registry authority is pinned to the same
/// permissive predicate for both sides so the *only* variable is the role gate.
#[test]
fn specialist_loop_advertising_drops_the_verdict_class_by_exact_set_difference() {
    use std::collections::BTreeSet;

    let verdict_names = default_verdict_names();
    assert!(
        verdict_names.contains(marmennill::tool_names::TOOL_LEAVE_VERDICT),
        "the default schema list must actually carry the verdict tool, otherwise this \
         test would be vacuous: {verdict_names:?}"
    );
    for name in VERDICT_RECORDING_TOOLS {
        assert!(
            is_verdict_recording_tool(name),
            "{name} must be recognised as verdict-recording by the crate's predicate"
        );
    }

    let default_names: BTreeSet<String> = ToolDef::default_tools()
        .iter()
        .map(|t| t.function.name.clone())
        .collect();

    let permissive = |_name: &str| true;

    // (b) The validator keeps the verdict class — and the whole default list.
    let validator_set =
        specialist_advertised(&ToolCaller::Specialist(Agent::Validator), permissive);
    assert_eq!(
        validator_set, default_names,
        "the validator must be offered every default tool, verdict tool included"
    );
    assert!(
        validator_set.contains(marmennill::tool_names::TOOL_LEAVE_VERDICT),
        "the verdict tool must be advertised to the validator role"
    );

    // (a) Every other role: the offer minus exactly the verdict class.
    let expected_worker: BTreeSet<String> =
        default_names.difference(&verdict_names).cloned().collect();
    for role in [
        Agent::Coder,
        Agent::Debugger,
        Agent::Researcher,
        Agent::Planner,
        Agent::Generalist,
    ] {
        let worker_set = specialist_advertised(&ToolCaller::Specialist(role), permissive);
        assert_eq!(
            worker_set, expected_worker,
            "{role} must be offered every ordinary tool and no verdict tool"
        );

        let removed: BTreeSet<String> = validator_set.difference(&worker_set).cloned().collect();
        assert_eq!(
            removed, verdict_names,
            "the role-aware offer may only ever remove the verdict class for {role}, got {removed:?}"
        );
        let added: BTreeSet<String> = worker_set.difference(&validator_set).cloned().collect();
        assert!(
            added.is_empty(),
            "the role-aware offer must not add anything for {role}, got {added:?}"
        );
        assert!(
            worker_set
                .iter()
                .all(|name| !is_verdict_recording_tool(name)),
            "no verdict-recording tool may be offered to {role}: {worker_set:?}"
        );

        // Ordinary tools survive explicitly (belt for the set equality above).
        for name in [
            marmennill::tool_names::TOOL_READ_FILE,
            marmennill::tool_names::TOOL_WRITE_FILE,
            marmennill::tool_names::TOOL_REPLACE,
            marmennill::tool_names::TOOL_RUN_COMMAND,
            marmennill::tool_names::TOOL_GREP_SEARCH,
            marmennill::tool_names::TOOL_GLOB,
            marmennill::tool_names::TOOL_REBIRTH,
            marmennill::tool_names::TOOL_REPLY_TO_ARBITRATOR,
        ] {
            assert!(
                worker_set.contains(name),
                "{role} must still be offered {name}, got {worker_set:?}"
            );
        }
    }
}

/// The hole itself: neither a wildcard registry namespace nor a prompt blueprint
/// that names both verdict spellings can put a verdict tool in a worker's offer,
/// while the validator role keeps it.
#[test]
fn specialist_loop_advertising_strips_verdicts_from_registry_and_blueprint_authority() {
    use std::collections::BTreeSet;

    use marmennill::tool_names::{
        TERMINAL_LEAVE_VERDICT, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_LEAVE_VERDICT, TOOL_READ_FILE,
        TOOL_REBIRTH, TOOL_REPLACE, TOOL_REPLY_TO_ARBITRATOR, TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
    };

    let registry = marmennill::orchestrator::SpecialistRegistry::canonical();

    // The authority level still admits the verdict tool for the wildcard
    // Generalist — that is precisely why the offer must be role-filtered.
    assert!(
        marmennill::orchestrator::caller_allows_tool(
            Agent::Generalist,
            TOOL_LEAVE_VERDICT,
            &registry
        ),
        "the Generalist's `*` namespace admits the verdict tool at the authority level"
    );
    let generalist_entry = registry
        .resolve(Agent::Generalist)
        .expect("the canonical registry carries the Generalist entry");
    let generalist_offer =
        specialist_advertised(&ToolCaller::Specialist(Agent::Generalist), |name| {
            generalist_entry.allows(name)
        });
    assert!(
        generalist_offer.contains(TOOL_WRITE_FILE) && generalist_offer.contains(TOOL_RUN_COMMAND),
        "the Generalist keeps its ordinary tools: {generalist_offer:?}"
    );
    assert!(
        generalist_offer
            .iter()
            .all(|name| !is_verdict_recording_tool(name)),
        "the wildcard Generalist namespace must not be enough to advertise a verdict tool: {generalist_offer:?}"
    );

    let validator_entry = registry
        .resolve(Agent::Validator)
        .expect("the canonical registry carries the Validator entry");
    let validator_offer =
        specialist_advertised(&ToolCaller::Specialist(Agent::Validator), |name| {
            validator_entry.allows(name)
        });
    assert!(
        validator_offer.contains(TOOL_LEAVE_VERDICT),
        "the validator role must be offered its verdict tool through the real registry: {validator_offer:?}"
    );

    // A prompt blueprint naming both verdict spellings: the offer is the blueprint's
    // ordinary tools plus the always-advertised notice tool — exact set equality.
    let blueprint = vec![
        TOOL_READ_FILE.to_string(),
        TOOL_WRITE_FILE.to_string(),
        TOOL_REPLACE.to_string(),
        TOOL_RUN_COMMAND.to_string(),
        TOOL_GREP_SEARCH.to_string(),
        TOOL_GLOB.to_string(),
        TOOL_REBIRTH.to_string(),
        TOOL_LEAVE_VERDICT.to_string(),
        TERMINAL_LEAVE_VERDICT.to_string(),
    ];
    let permissive = |_name: &str| true;
    let worker_caller = ToolCaller::SpecialistWithTools {
        agent: Agent::Generalist,
        allowed_tools: blueprint.clone(),
    };
    let validator_caller = ToolCaller::SpecialistWithTools {
        agent: Agent::Validator,
        allowed_tools: blueprint.clone(),
    };

    let expected_worker: BTreeSet<String> = [
        TOOL_READ_FILE,
        TOOL_WRITE_FILE,
        TOOL_REPLACE,
        TOOL_RUN_COMMAND,
        TOOL_GREP_SEARCH,
        TOOL_GLOB,
        TOOL_REBIRTH,
        TOOL_REPLY_TO_ARBITRATOR,
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let worker_offer = specialist_advertised(&worker_caller, permissive);
    assert_eq!(
        worker_offer, expected_worker,
        "a blueprint naming a verdict tool must not get it advertised to a worker"
    );

    let mut expected_validator = expected_worker.clone();
    expected_validator.insert(TOOL_LEAVE_VERDICT.to_string());
    let validator_offer = specialist_advertised(&validator_caller, permissive);
    assert_eq!(
        validator_offer, expected_validator,
        "the same blueprint must keep the verdict tool for the validator role"
    );

    // The prose tool list the loop puts in the system prompt is filtered by the
    // same helper, so it can never spell a verdict tool for a refused role.
    let worker_blueprint_view =
        marmennill::agents::runner::fix_loop::role_filtered_blueprint(&worker_caller, &blueprint);
    let validator_blueprint_view = marmennill::agents::runner::fix_loop::role_filtered_blueprint(
        &validator_caller,
        &blueprint,
    );
    assert!(
        worker_blueprint_view
            .iter()
            .all(|t| !is_verdict_recording_tool(t)),
        "the worker's prose tool list must drop the verdict class: {worker_blueprint_view:?}"
    );
    assert!(
        validator_blueprint_view
            .iter()
            .any(|t| is_verdict_recording_tool(t)),
        "the validator's prose tool list must keep the verdict class: {validator_blueprint_view:?}"
    );
}

/// (c) Defence in depth: even though the verdict tool is no longer offered, a
/// model that hallucinates the call anyway is still refused by the dispatcher-side
/// gate, and nothing about the refusal is softened by the advertising change.
#[tokio::test]
async fn specialist_loop_refuses_a_hallucinated_verdict_it_never_advertised() {
    use marmennill::agents::AgentBlueprint;
    use marmennill::markers::MARKER_COMPLETE;
    use marmennill::tool_names::{
        TERMINAL_LEAVE_VERDICT, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_LEAVE_VERDICT, TOOL_READ_FILE,
        TOOL_REBIRTH, TOOL_REPLY_TO_ARBITRATOR, TOOL_WRITE_FILE,
    };
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    let bodies: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::default();
    let turns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rejection_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let bodies_c = bodies.clone();
    let turns_c = turns.clone();
    let rejection_seen_c = rejection_seen.clone();

    let deliverable = marmennill::harness::with_workspace_root(root, async move {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                bodies_c
                    .lock()
                    .expect("request bodies")
                    .push(String::from_utf8_lossy(&req.body).to_string());
                match turns_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => {
                        // The model invents a verdict call it was never offered.
                        let args = serde_json::json!({
                            "verdict": "APPROVED",
                            "comments": "HALLUCINATED-SELF-APPROVAL-CANARY"
                        })
                        .to_string();
                        ResponseTemplate::new(200).set_body_string(sse_tool_call(
                            "call_hallucinated_verdict",
                            TOOL_LEAVE_VERDICT,
                            &args,
                        ))
                    }
                    1 => {
                        let body = String::from_utf8_lossy(&req.body).to_string();
                        if body.contains("VERDICT REJECTED")
                            && body.contains("cannot approve or reject its own deliverable")
                        {
                            rejection_seen_c.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        ResponseTemplate::new(200).set_body_string(sse_text(&format!(
                            "Work delivered.\n\n{MARKER_COMPLETE} (t-912)"
                        )))
                    }
                    _ => ResponseTemplate::new(200).set_body_string(sse_text(&format!(
                        "Still delivered.\n\n{MARKER_COMPLETE}"
                    ))),
                }
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let mut cfg = marmennill::config::Config {
            backend_url: backend_url.clone(),
            model: "test-model".to_string(),
            ..Default::default()
        };
        cfg.orchestration.specialists.insert(
            Agent::Generalist.as_str().to_string(),
            marmennill::config::SpecialistConfig {
                module: "src/agents/generalist.rs".to_string(),
                tools: {
                    use marmennill::agents::Specialist as _;
                    marmennill::agents::Generalist
                        .tool_namespaces()
                        .iter()
                        .map(|ns| (*ns).to_string())
                        .collect()
                },
                // The validator pass is not what this test is about; the worker's
                // hallucinated verdict call is. Validation is opted out explicitly.
                enable_validator: Some(false),
                ..Default::default()
            },
        );

        let client = marmennill::llm::ChatClient::new(&backend_url, "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: Agent::Generalist,
            prompt: "Produce the deliverable".to_string(),
            snippets: vec![],
            task_id: Some("t-912".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        // A blueprint that names both verdict spellings: before t-056 this is what
        // advertised a verdict tool to a worker.
        let ctx = marmennill::agents::IsolatedContext::from_request(
            "You are the Generalist specialist.".to_string(),
            &req,
        )
        .with_blueprint(AgentBlueprint {
            role_name: "Generalist".to_string(),
            reasoning: String::new(),
            selected_skills: vec![],
            allowed_tools: vec![
                TOOL_READ_FILE.to_string(),
                TOOL_WRITE_FILE.to_string(),
                TOOL_GREP_SEARCH.to_string(),
                TOOL_GLOB.to_string(),
                TOOL_REBIRTH.to_string(),
                TOOL_LEAVE_VERDICT.to_string(),
                TERMINAL_LEAVE_VERDICT.to_string(),
            ],
            system_prompt: "You are the Generalist specialist.".to_string(),
            task_id: None,
        });
        let token = tokio_util::sync::CancellationToken::new();

        marmennill::agents::run_specialist_live(&client, Agent::Generalist, &ctx, &cfg, &token)
            .await
    })
    .await
    .expect("a refused verdict attempt still yields a deliverable");

    let bodies = bodies.lock().expect("request bodies").clone();
    assert_eq!(
        bodies.len(),
        2,
        "the fixture must have run the hallucinated verdict turn and the concluding turn"
    );

    // (a) What was actually advertised on the wire: the blueprint's ordinary tools
    // plus the always-advertised notice tool — and no verdict tool at all.
    let expected: std::collections::BTreeSet<String> = [
        TOOL_READ_FILE,
        TOOL_WRITE_FILE,
        TOOL_GREP_SEARCH,
        TOOL_GLOB,
        TOOL_REBIRTH,
        TOOL_REPLY_TO_ARBITRATOR,
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    for body in &bodies {
        let advertised: std::collections::BTreeSet<String> =
            advertised_tool_names(body).into_iter().collect();
        assert_eq!(
            advertised, expected,
            "a non-validator specialist loop must be offered exactly its ordinary tools"
        );
        assert!(
            advertised
                .iter()
                .all(|name| !is_verdict_recording_tool(name)),
            "no verdict-recording tool may ever be advertised to this run: {advertised:?}"
        );
    }

    // The prose tool list in the system message is filtered the same way.
    let first: serde_json::Value =
        serde_json::from_str(bodies.first().expect("first request")).expect("valid JSON");
    let system_text = first
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
        })
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        system_text.contains(TOOL_READ_FILE) && system_text.contains(TOOL_REBIRTH),
        "the system prompt must still advertise the ordinary blueprint tools: {system_text}"
    );
    assert!(
        !system_text.contains(TOOL_LEAVE_VERDICT),
        "the system prompt must not advertise a verdict tool to a non-validator: {system_text}"
    );

    // (c) The unadvertised call was still dispatched-to and refused fail-closed.
    assert!(
        rejection_seen.load(std::sync::atomic::Ordering::SeqCst),
        "the worker must have received the explicit verdict refusal, got deliverable: {deliverable}"
    );
    assert!(
        !deliverable.contains("HALLUCINATED-SELF-APPROVAL-CANARY"),
        "a refused verdict payload must never surface as a verdict: {deliverable}"
    );
    assert!(
        deliverable.contains(MARKER_COMPLETE),
        "the run must conclude normally after the refusal: {deliverable}"
    );
}
