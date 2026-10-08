use super::notice::*;
use crate::agents::Agent;
use crate::harness::{HarnessStats, ToolCaller, ToolInvocation, dispatch_for_async};
use crate::llm::ChatClient;
use crate::orchestrator::SpecialistRegistry;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_notice_id_generation_and_posting() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice1 = post_notice_to_worker("coder", "Why did you use SQLite?", None);
    let notice2 = post_notice_to_worker("debugger", "Check stack trace on line 42", None);

    assert!(notice1.notice_id.starts_with("notice-"));
    assert!(notice2.notice_id.starts_with("notice-"));
    assert_ne!(notice1.notice_id, notice2.notice_id);

    assert_eq!(notice1.user_inquiry, "Why did you use SQLite?");
    assert_eq!(notice1.target_worker, "coder");

    assert_eq!(notice2.user_inquiry, "Check stack trace on line 42");
    assert_eq!(notice2.target_worker, "debugger");

    assert!(get_pending_notice(&notice1.notice_id).is_some());
    assert!(get_pending_notice(&notice2.notice_id).is_some());
}

#[tokio::test]
async fn test_worker_inbox_draining_with_exact_role_matching() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let n1 = post_notice_to_worker("coder", "Question 1 for coder", None);
    let _n2 = post_notice_to_worker("debugger", "Question for debugger", None);

    // Draining for worker key "coder-t-001" matches target "coder" because the
    // worker's authoritative agent name is exactly "coder".
    let drained = drain_worker_notices("coder-t-001");
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].notice_id, n1.notice_id);
    assert_eq!(drained[0].user_inquiry, "Question 1 for coder");

    // Second drain should be empty
    let empty = drain_worker_notices("coder-t-001");
    assert!(empty.is_empty());

    // Draining for debugger
    let drained_dbg = drain_worker_notices("debugger");
    assert_eq!(drained_dbg.len(), 1);
    assert_eq!(drained_dbg[0].user_inquiry, "Question for debugger");
}

// ---------------------------------------------------------------------------
// Routing regression tests — recon H1 (substring/prefix routing) and the
// disambiguated worker keys `{base}#{n}` introduced for recon H5.
// ---------------------------------------------------------------------------

/// H1: a notice addressed to the `coder` role must NOT be drained by the
/// validator worker auditing that coder — its registry key
/// `validator-coder-t-001` merely *contains* the substring `coder`.
#[tokio::test]
async fn test_notice_for_coder_is_not_stolen_by_validator_worker() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice = post_notice_to_worker("coder", "Please switch to async I/O", None);

    // The validator loop registers `validator-{agent}` + task id
    // (src/agents/validation.rs), i.e. key `validator-coder-t-001`.
    assert!(
        drain_worker_notices("validator-coder-t-001").is_empty(),
        "validator-coder-t-001 must not drain a notice addressed to the coder role"
    );
    assert!(
        drain_worker_notices("validator-coder").is_empty(),
        "validator-coder (task-less key) must not drain a notice addressed to coder"
    );
    assert!(
        drain_worker_notices("validator-coder-t-001#4").is_empty(),
        "disambiguated validator must not drain a notice addressed to coder"
    );

    // The notice is still pending (nothing was silently consumed) and it does
    // reach the coder itself.
    assert!(get_pending_notice(&notice.notice_id).is_some());
    let drained = drain_worker_notices("coder-t-001");
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].notice_id, notice.notice_id);
}

/// Symmetric guard: a notice addressed to the validator worker must not be
/// stolen by the coder whose name is embedded in that validator's key.
#[tokio::test]
async fn test_notice_for_validator_worker_is_not_stolen_by_coder() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let by_key = post_notice_to_worker("validator-coder-t-001", "Re-audit the JIT fix", None);
    let by_role = post_notice_to_worker("validator-coder", "Re-check the test suite", None);

    assert!(
        drain_worker_notices("coder-t-001").is_empty(),
        "coder-t-001 must not drain notices addressed to validator-coder-t-001"
    );

    let drained = drain_worker_notices("validator-coder-t-001");
    assert_eq!(drained.len(), 2);
    let ids: Vec<&str> = drained.iter().map(|n| n.notice_id.as_str()).collect();
    assert!(ids.contains(&by_key.notice_id.as_str()));
    assert!(ids.contains(&by_role.notice_id.as_str()));
}

/// A role-family address (`validator`) still reaches the nested validator
/// worker — this is the harness follow-up path, where the caller tag is the
/// bare role name from `ToolCaller::role_name()`.
#[tokio::test]
async fn test_role_family_address_reaches_nested_validator_worker() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice = post_notice_to_worker("validator", "Please re-audit the deliverable", None);

    assert!(drain_worker_notices("coder-t-001").is_empty());
    assert!(drain_worker_notices("planner-t-002").is_empty());

    let drained = drain_worker_notices("validator-planner-t-002");
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].notice_id, notice.notice_id);
}

/// H1 + disambiguated keys: the key `coder-t-001#7` is routed only by addresses
/// that equal the worker's identity exactly — never by a `coder-t-00` prefix and
/// never by a sibling worker sharing the same natural key.
#[tokio::test]
async fn test_disambiguated_worker_key_matches_exact_worker_only() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice = post_notice_to_worker("coder-t-001#7", "Follow-up on the exact worker", None);

    // Unanchored prefixes of the key must not route (this was `contains`).
    assert!(drain_worker_notices("coder-t-00").is_empty());
    assert!(drain_worker_notices("coder-t-0").is_empty());
    assert!(drain_worker_notices("coder").is_empty());
    assert!(drain_worker_notices("coder-t-001").is_empty());

    // A sibling worker disambiguated with a different counter is a different
    // worker and must not receive an address spelling out `#7`.
    assert!(drain_worker_notices("coder-t-001#9").is_empty());
    assert!(drain_worker_notices("coder-t-001#3").is_empty());
    // A longer task id sharing the same textual prefix must not match either.
    assert!(drain_worker_notices("coder-t-0010").is_empty());

    assert!(get_pending_notice(&notice.notice_id).is_some());

    // Only the exact effective key routes it.
    let drained = drain_worker_notices("coder-t-001#7");
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].notice_id, notice.notice_id);
}

/// A notice addressed with the *natural* key is delivered to the worker that
/// holds it, even when the registry had to disambiguate that worker's key.
#[tokio::test]
async fn test_disambiguated_worker_drains_notice_addressed_to_natural_key() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice = post_notice_to_worker("coder-t-001", "Addressed by natural key", None);
    let other = post_notice_to_worker("t-001", "Addressed by task id", None);

    // Prefix/extension of the task id must not route.
    assert!(drain_worker_notices("coder-t-00").is_empty());
    assert!(drain_worker_notices("coder-t-0010").is_empty());
    // A worker for a different task must not route it either.
    assert!(drain_worker_notices("coder-t-002").is_empty());

    let drained = drain_worker_notices("coder-t-001#7");
    assert_eq!(drained.len(), 2);
    let ids: Vec<&str> = drained.iter().map(|n| n.notice_id.as_str()).collect();
    assert!(ids.contains(&notice.notice_id.as_str()));
    assert!(ids.contains(&other.notice_id.as_str()));
}

/// H1 second scenario: a task-id address is matched by exact task id only, so
/// `t-1` is no longer drained by `coder-t-10`.
#[tokio::test]
async fn test_task_id_address_matches_exact_task_id_only() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let short = post_notice_to_worker("t-1", "Notice for task t-1", None);
    let exact = post_notice_to_worker("t-001", "Notice for task t-001", None);

    // `coder-t-10` contains `t-1`, and `coder-t-0010` contains `t-001` — neither
    // may route.
    assert!(drain_worker_notices("coder-t-10").is_empty());
    assert!(drain_worker_notices("coder-t-0010").is_empty());
    assert!(drain_worker_notices("validator-coder-t-0010").is_empty());

    assert!(get_pending_notice(&short.notice_id).is_some());

    let drained = drain_worker_notices("coder-t-001");
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].notice_id, exact.notice_id);
}

/// Unroutable notices keep the existing no-target behaviour: nothing is drained,
/// nothing is silently dropped (the notice stays pending and can still be
/// replied to by id).
#[tokio::test]
async fn test_unroutable_notice_is_not_delivered_or_dropped() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice = post_notice_to_worker(
        "archaeologist-t-4242",
        "This target has no matching worker",
        None,
    );

    assert!(drain_worker_notices("coder-t-001").is_empty());
    assert!(drain_worker_notices("validator-coder-t-001").is_empty());
    assert!(drain_worker_notices("archaeologist").is_empty());
    assert!(drain_worker_notices("t-4242").is_empty());
    assert!(drain_worker_notices("").is_empty());

    // A truncated prefix of a real worker key addresses nobody: the worker whose
    // key merely *starts with* that prefix must not pull the notice in (this was
    // the `contains` rule against disambiguated keys such as `coder-t-001#7`).
    let truncated = post_notice_to_worker("coder-t-0", "Prefix of coder-t-001#7", None);
    assert!(drain_worker_notices("coder-t-001#7").is_empty());
    assert!(drain_worker_notices("coder-t-001").is_empty());
    assert!(drain_worker_notices("coder").is_empty());
    assert!(get_pending_notice(&truncated.notice_id).is_some());
    // It is still deliverable to a worker genuinely keyed `coder-t-0`.
    let drained_prefix = drain_worker_notices("coder-t-0");
    assert_eq!(drained_prefix.len(), 1);
    assert_eq!(drained_prefix[0].notice_id, truncated.notice_id);

    // Not consumed, not evicted: still pending. Under the strict reply contract
    // (recon H6) it is *not* replyable by a worker it never addressed either —
    // the deterministic outcome is that it stays pending until the TTL sweep
    // drops it, instead of being silently consumed by an unrelated reply.
    let pending = get_pending_notice(&notice.notice_id);
    assert!(pending.is_some());
    assert_eq!(pending.unwrap().target_worker, "archaeologist-t-4242");

    let replied =
        record_worker_reply_for_notice("coder-t-001", &notice.notice_id, "Answered out of band");
    assert_eq!(
        replied,
        Err(NoticeReplyRejection::WorkerMismatch {
            notice_id: notice.notice_id.clone(),
            worker_tag: "coder-t-001".to_string(),
            addressed_to: "archaeologist-t-4242".to_string(),
        }),
        "a reply from a worker the notice never addressed must be rejected, never guessed"
    );
    assert!(get_pending_notice(&notice.notice_id).is_some());
    assert!(get_worker_reply(&notice.notice_id).is_none());
}

/// A target that normalizes to nothing (pure decoration) must not match every
/// worker — the old `contains(&target)` rule matched the empty string against
/// every key.
#[tokio::test]
async fn test_empty_target_is_not_drained_by_every_worker() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice = post_notice_to_worker("[]", "Undecorable target", None);

    assert!(drain_worker_notices("coder-t-001").is_empty());
    assert!(drain_worker_notices("validator-coder-t-001").is_empty());
    assert!(get_pending_notice(&notice.notice_id).is_some());
}

/// Broadcast steering (`*` / `worker`) is unchanged by the exact-match routing.
#[tokio::test]
async fn test_broadcast_targets_still_route_to_any_worker() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let star = post_notice_to_worker("*", "All workers: wrap up now", None);
    let drained_validator = drain_worker_notices("validator-coder-t-001");
    assert_eq!(drained_validator.len(), 1);
    assert_eq!(drained_validator[0].notice_id, star.notice_id);

    let worker = post_notice_to_worker("worker", "Generic steering instruction", None);
    let drained_coder = drain_worker_notices("coder-t-001");
    assert_eq!(drained_coder.len(), 1);
    assert_eq!(drained_coder[0].notice_id, worker.notice_id);
}

/// The identity fields driving the routing rules are split out of the key by
/// structure, exactly mirroring the fields the registry stores in
/// `WorkerState.info`.
#[test]
fn test_worker_routing_identity_resolves_authoritative_fields() {
    let disambiguated = worker_routing_identity("Coder-T-001#7");
    assert_eq!(disambiguated.effective_key, "coder-t-001#7");
    assert_eq!(disambiguated.natural_key, "coder-t-001");
    assert_eq!(disambiguated.agent_name, "coder");
    assert_eq!(disambiguated.task_id.as_deref(), Some("t-001"));

    let validator = worker_routing_identity("validator-coder-t-001");
    assert_eq!(validator.natural_key, "validator-coder-t-001");
    assert_eq!(validator.agent_name, "validator-coder");
    assert_eq!(validator.task_id.as_deref(), Some("t-001"));

    let taskless = worker_routing_identity("validator-coder");
    assert_eq!(taskless.agent_name, "validator-coder");
    assert_eq!(taskless.task_id, None);

    // Synthetic worker id (`{agent}-w{n}`) is not a task id.
    let synthetic = worker_routing_identity("coder-w3");
    assert_eq!(synthetic.agent_name, "coder");
    assert_eq!(synthetic.task_id, None);

    // UUID fallback keys still reduce to their natural identity.
    let uuid_key = worker_routing_identity("researcher-t-004#u9f2c1a7");
    assert_eq!(uuid_key.natural_key, "researcher-t-004");
    assert_eq!(uuid_key.agent_name, "researcher");
    assert_eq!(uuid_key.task_id.as_deref(), Some("t-004"));

    // Exact-match routing decisions, asserted directly on the identity.
    assert!(validator.routes("validator"));
    assert!(validator.routes("validator-coder"));
    assert!(validator.routes("validator-coder-t-001"));
    assert!(validator.routes("t-001"));
    assert!(!validator.routes("coder"));
    assert!(!validator.routes("coder-t-001"));
    assert!(!validator.routes("t-00"));
    assert!(!validator.routes("validator-coder-t-00"));
    assert!(!validator.routes(""));

    let disambiguated = worker_routing_identity("coder-t-001#7");
    assert!(disambiguated.routes("coder-t-001#7"));
    assert!(disambiguated.routes("coder-t-001"));
    assert!(disambiguated.routes("coder"));
    assert!(!disambiguated.routes("coder-t-00"));
    assert!(!disambiguated.routes("coder-t-001#9"));
    assert!(!disambiguated.routes("coder-t-0010"));
}

#[tokio::test]
async fn test_record_worker_reply_matches_pending_notice() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let n = post_notice_to_worker("coder", "Why SQLite?", Some("notice-custom-1"));
    assert_eq!(n.notice_id, "notice-custom-1");

    let res = record_worker_reply(
        "coder-t-001",
        "notice-custom-1",
        "Because it requires no external server setup.",
    );

    assert!(res.is_ok());
    let original = res.unwrap();
    assert_eq!(original.user_inquiry, "Why SQLite?");
    assert_eq!(original.notice_id, "notice-custom-1");

    // Pending notice is removed once replied
    assert!(get_pending_notice("notice-custom-1").is_none());

    // Reply is recorded in store
    let reply = get_worker_reply("notice-custom-1");
    assert!(reply.is_some());
    let rep = reply.unwrap();
    assert_eq!(rep.notice_id, "notice-custom-1");
    assert_eq!(rep.worker_tag, "coder-t-001");
    assert_eq!(
        rep.reply_message,
        "Because it requires no external server setup."
    );
}

#[tokio::test]
async fn test_handle_reply_to_arbitrator_success_and_validation() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let server = MockServer::start().await;
    let response_json = serde_json::json!({
        "decision": "SynthesizeResponse",
        "response": "Coder bekräftar att async I/O har implementerats.",
        "follow_up_prompt": null,
        "user_status": null
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(sse_delta(&response_json.to_string())),
        )
        .mount(&server)
        .await;

    let cfg = crate::config::Config {
        backend_url: server.uri(),
        ..Default::default()
    };
    crate::config::set_active(cfg);

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::orchestrator::set_event_sender(event_tx);

    let steering_history = std::sync::Arc::new(std::sync::RwLock::new(vec![(
        "Please use async I/O".to_string(),
        "Forwarded notice notice-async-1 to coder (awaiting specialist reply)".to_string(),
    )]));
    crate::orchestrator::set_steering_history(std::sync::Arc::clone(&steering_history));

    let notice = post_notice_to_worker("coder", "Please use async I/O", Some("notice-async-1"));

    // Missing notice_id should fail with BadArguments
    let tool_missing_id = ToolInvocation {
        name: crate::tool_names::TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "message": "Done"
        }),
    };
    let err_missing_id =
        dispatch_for_async(&tool_missing_id, ToolCaller::Specialist(Agent::Coder)).await;
    assert!(err_missing_id.is_err());

    // Missing message should fail with BadArguments
    let tool_missing_msg = ToolInvocation {
        name: crate::tool_names::TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-async-1"
        }),
    };
    let err_missing_msg =
        dispatch_for_async(&tool_missing_msg, ToolCaller::Specialist(Agent::Coder)).await;
    assert!(err_missing_msg.is_err());

    // Valid reply
    let tool_valid = ToolInvocation {
        name: crate::tool_names::TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-async-1",
            "message": "Switched to tokio::fs for all file operations."
        }),
    };
    let ok_res = dispatch_for_async(&tool_valid, ToolCaller::Specialist(Agent::Coder)).await;
    assert!(ok_res.is_ok());
    let out = ok_res.unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("recorded successfully"));
    assert!(out.content.contains(&notice.user_inquiry));

    // Notice is now resolved
    assert!(get_pending_notice("notice-async-1").is_none());
    assert!(get_worker_reply("notice-async-1").is_some());

    // Verify Event::SteerResponse was emitted to the user
    let ev = event_rx
        .try_recv()
        .expect("Event::SteerResponse should be emitted");
    match ev {
        crate::ui::Event::SteerResponse(text) => {
            assert!(text.contains("Coder bekräftar att async I/O har implementerats."));
        }
        other => panic!("Expected Event::SteerResponse, got {other:?}"),
    }

    // Verify steering history was updated with specialist details and synthesized response
    {
        let hist = steering_history.read().unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].0, "Please use async I/O");
        assert!(
            hist[0]
                .1
                .contains("Coder bekräftar att async I/O har implementerats.")
        );
        assert!(
            hist[0]
                .1
                .contains("Switched to tokio::fs for all file operations.")
        );
        assert!(!hist[0].1.contains("awaiting specialist reply"));
    }
}

#[tokio::test]
async fn test_handle_reply_to_arbitrator_ask_followup() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let server = MockServer::start().await;
    let response_json = serde_json::json!({
        "decision": "AskFollowUp",
        "response": null,
        "follow_up_prompt": "What about backward compatibility with older files?",
        "user_status": "Ställer följdfråga om bakåtkompatibilitet..."
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(sse_delta(&response_json.to_string())),
        )
        .mount(&server)
        .await;

    let cfg = crate::config::Config {
        backend_url: server.uri(),
        ..Default::default()
    };
    crate::config::set_active(cfg);

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::orchestrator::set_event_sender(event_tx);

    let steering_history = std::sync::Arc::new(std::sync::RwLock::new(vec![(
        "Please migrate to new format".to_string(),
        "Forwarded notice notice-follow-1 to coder (awaiting specialist reply)".to_string(),
    )]));
    crate::orchestrator::set_steering_history(std::sync::Arc::clone(&steering_history));

    let _notice = post_notice_to_worker(
        "coder",
        "Please migrate to new format",
        Some("notice-follow-1"),
    );
    let initial_notices = drain_worker_notices("coder");
    assert_eq!(initial_notices.len(), 1);

    let tool_reply = ToolInvocation {
        name: crate::tool_names::TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-follow-1",
            "message": "Migrated to new binary format."
        }),
    };

    let res = dispatch_for_async(&tool_reply, ToolCaller::Specialist(Agent::Coder)).await;
    assert!(res.is_ok());
    let out = res.unwrap();
    assert!(out.content.contains("follow-up question"));

    // Check user received status notice
    let ev = event_rx
        .try_recv()
        .expect("SteerResponse emitted for user status");
    match ev {
        crate::ui::Event::SteerResponse(text) => {
            assert!(text.contains("Ställer följdfråga om bakåtkompatibilitet"));
        }
        other => panic!("Expected SteerResponse, got {other:?}"),
    }

    // Check worker's inbox received the follow-up notice
    let drained = drain_worker_notices("coder");
    assert_eq!(drained.len(), 1);
    assert_eq!(
        drained[0].user_inquiry,
        "What about backward compatibility with older files?"
    );

    // Verify steering history recorded the follow-up state awaiting next reply
    {
        let hist = steering_history.read().unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].0, "Please migrate to new format");
        assert!(
            hist[0]
                .1
                .contains("What about backward compatibility with older files?")
        );
        assert!(hist[0].1.contains("awaiting specialist reply"));
        assert!(hist[0].1.contains("Migrated to new binary format."));
    }
}

#[tokio::test]
async fn test_steering_history_multiple_notices_resolved_out_of_order() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let server = MockServer::start().await;
    let eval_coder = serde_json::json!({
        "decision": "SynthesizeResponse",
        "response": "Coder har fixat PPC JIT.",
        "follow_up_prompt": null,
        "user_status": null
    });
    let eval_dbg = serde_json::json!({
        "decision": "SynthesizeResponse",
        "response": "Debugger har isolerat minnesläckan.",
        "follow_up_prompt": null,
        "user_status": null
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            let body = String::from_utf8_lossy(&req.body).to_string();
            if body.contains("Debugger") || body.contains("debugger") {
                ResponseTemplate::new(200).set_body_string(sse_delta(&eval_dbg.to_string()))
            } else {
                ResponseTemplate::new(200).set_body_string(sse_delta(&eval_coder.to_string()))
            }
        })
        .mount(&server)
        .await;

    let cfg = crate::config::Config {
        backend_url: server.uri(),
        ..Default::default()
    };
    crate::config::set_active(cfg);

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::orchestrator::set_event_sender(event_tx);

    let steering_history = std::sync::Arc::new(std::sync::RwLock::new(vec![
        (
            "Vad gör codern?".to_string(),
            "Forwarded notice notice-1 to coder (awaiting specialist reply)".to_string(),
        ),
        (
            "Vad gör debuggern?".to_string(),
            "Forwarded notice notice-2 to debugger (awaiting specialist reply)".to_string(),
        ),
    ]));
    crate::orchestrator::set_steering_history(std::sync::Arc::clone(&steering_history));

    let _n1 = post_notice_to_worker("coder", "Vad gör codern?", Some("notice-1"));
    let _n2 = post_notice_to_worker("debugger", "Vad gör debuggern?", Some("notice-2"));

    // Debugger replies FIRST (out-of-order)
    let tool_dbg = ToolInvocation {
        name: crate::tool_names::TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-2",
            "message": "Minnesläckan berodde på en saknad free i malloc-wrapper."
        }),
    };
    let res_dbg = dispatch_for_async(&tool_dbg, ToolCaller::Specialist(Agent::Debugger)).await;
    assert!(res_dbg.is_ok());

    // Verify: notice-2 (debugger) is updated, notice-1 (coder) is still pending
    {
        let hist = steering_history.read().unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].0, "Vad gör codern?");
        assert!(hist[0].1.contains("awaiting specialist reply"));

        assert_eq!(hist[1].0, "Vad gör debuggern?");
        assert!(hist[1].1.contains("Debugger har isolerat minnesläckan."));
        assert!(hist[1].1.contains("malloc-wrapper"));
        assert!(!hist[1].1.contains("awaiting specialist reply"));
    }

    // Coder replies SECOND
    let tool_coder = ToolInvocation {
        name: crate::tool_names::TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-1",
            "message": "Implementerat JIT-instruktionerna för PowerPC 604e."
        }),
    };
    let res_coder = dispatch_for_async(&tool_coder, ToolCaller::Specialist(Agent::Coder)).await;
    assert!(res_coder.is_ok());

    // Verify: BOTH notices are now updated with their respective specialist details!
    {
        let hist = steering_history.read().unwrap();
        assert_eq!(hist.len(), 2);

        assert_eq!(hist[0].0, "Vad gör codern?");
        assert!(hist[0].1.contains("Coder har fixat PPC JIT."));
        assert!(hist[0].1.contains("PowerPC 604e"));
        assert!(!hist[0].1.contains("awaiting specialist reply"));

        assert_eq!(hist[1].0, "Vad gör debuggern?");
        assert!(hist[1].1.contains("Debugger har isolerat minnesläckan."));
        assert!(hist[1].1.contains("malloc-wrapper"));
        assert!(!hist[1].1.contains("awaiting specialist reply"));
    }
}

fn sse_delta(content: &str) -> String {
    let obj = serde_json::json!({
        "id": "chatcmpl-1",
        "choices": [{
            "delta": {
                "content": content
            },
            "finish_reason": null
        }]
    });
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::to_string(&obj).unwrap()
    )
}

#[tokio::test]
async fn test_evaluate_worker_reply_synthesize_response() {
    let server = MockServer::start().await;

    let response_json = serde_json::json!({
        "decision": "SynthesizeResponse",
        "response": "Coder förklarar att SQLite valdes för att minimera externa beroenden.",
        "follow_up_prompt": null,
        "user_status": null
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(sse_delta(&response_json.to_string())),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new_with_token(server.uri(), "mock-model", "tok");
    let stats = HarnessStats::new();

    let notice = SteerNotice {
        notice_id: "notice-eval-1".to_string(),
        user_inquiry: "Varför valde du SQLite?".to_string(),
        target_worker: "coder".to_string(),
        created_at_ms: 100,
    };

    let mut streamed_deltas = Vec::new();
    let eval = evaluate_worker_reply(
        &client,
        &stats,
        &notice,
        "coder-t-001",
        "SQLite har noll konfiguration och fungerar utmärkt med WAL-mode.",
        |delta| streamed_deltas.push(delta.to_string()),
    )
    .await
    .expect("evaluation succeeds");

    assert_eq!(eval.decision, "SynthesizeResponse");
    assert_eq!(
        eval.response.as_deref(),
        Some("Coder förklarar att SQLite valdes för att minimera externa beroenden.")
    );
    assert!(eval.follow_up_prompt.is_none());
    assert!(!streamed_deltas.is_empty());
}

#[tokio::test]
async fn test_evaluate_worker_reply_ask_followup() {
    let server = MockServer::start().await;

    let response_json = serde_json::json!({
        "decision": "AskFollowUp",
        "response": null,
        "follow_up_prompt": "Will switching to SQLite break the transaction isolation requirements in requirement t-003?",
        "user_status": "Ställer en följdfråga till Coder gällande transaktionsisolering..."
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(sse_delta(&response_json.to_string())),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new_with_token(server.uri(), "mock-model", "tok");
    let stats = HarnessStats::new();

    let notice = SteerNotice {
        notice_id: "notice-eval-2".to_string(),
        user_inquiry: "Kan vi byta till SQLite?".to_string(),
        target_worker: "coder".to_string(),
        created_at_ms: 200,
    };

    let mut streamed_deltas = Vec::new();
    let eval = evaluate_worker_reply(
        &client,
        &stats,
        &notice,
        "coder-t-001",
        "Jag kan byta databas.",
        |delta| streamed_deltas.push(delta.to_string()),
    )
    .await
    .expect("evaluation succeeds");

    assert_eq!(eval.decision, "AskFollowUp");
    assert!(eval.response.is_none());
    assert_eq!(
        eval.follow_up_prompt.as_deref(),
        Some(
            "Will switching to SQLite break the transaction isolation requirements in requirement t-003?"
        )
    );
    assert_eq!(
        eval.user_status.as_deref(),
        Some("Ställer en följdfråga till Coder gällande transaktionsisolering...")
    );
    // When follow-up is requested, no final response is streamed to the user yet
    assert!(streamed_deltas.is_empty());
}

#[tokio::test]
async fn test_evaluate_worker_reply_fallback_on_plain_text() {
    let server = MockServer::start().await;

    // LLM outputs non-JSON plain text
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_delta(
            "Specialist Coder explains that timeout is already set to 15s.",
        )))
        .mount(&server)
        .await;

    let client = ChatClient::new_with_token(server.uri(), "mock-model", "tok");
    let stats = HarnessStats::new();

    let notice = SteerNotice {
        notice_id: "notice-eval-3".to_string(),
        user_inquiry: "Did you set a timeout?".to_string(),
        target_worker: "coder".to_string(),
        created_at_ms: 300,
    };

    let mut streamed_deltas = Vec::new();
    let eval = evaluate_worker_reply(
        &client,
        &stats,
        &notice,
        "coder-t-001",
        "Timeout is 15s.",
        |delta| streamed_deltas.push(delta.to_string()),
    )
    .await
    .expect("fallback succeeds");

    assert_eq!(eval.decision, "SynthesizeResponse");
    assert!(eval.response.is_some());
    assert!(eval.response.unwrap().contains("timeout is already set"));
    assert!(!streamed_deltas.is_empty());
}

#[test]
fn test_all_agent_archetypes_allow_reply_to_arbitrator() {
    let registry = SpecialistRegistry::canonical();
    let all_agents = [
        Agent::Coder,
        Agent::Researcher,
        Agent::Debugger,
        Agent::Validator,
        Agent::Generalist,
        Agent::Planner,
    ];

    for agent in all_agents {
        assert!(
            crate::orchestrator::caller_allows_tool(
                agent,
                crate::tool_names::TOOL_REPLY_TO_ARBITRATOR,
                &registry
            ),
            "Agent {:?} must be allowed to invoke reply_to_arbitrator",
            agent
        );
    }
}

#[test]
fn test_assemble_tools_includes_reply_to_arbitrator_unconditionally() {
    // Even with a restricted allowed_tools list, reply_to_arbitrator is always included
    let restricted = vec![crate::tool_names::TOOL_READ_FILE.to_string()];
    let tools = crate::agents::runner::fix_loop::assemble_tools(Some(&restricted), |_| false, &[]);

    let has_reply = tools
        .iter()
        .any(|t| t.function.name == crate::tool_names::TOOL_REPLY_TO_ARBITRATOR);
    assert!(
        has_reply,
        "assemble_tools must always include reply_to_arbitrator"
    );
}

// ---------------------------------------------------------------------------
// Storage lifecycle regression tests — recon H4 (no TTL, unbounded inboxes,
// drained-empty entries never reclaimed, drain only at turn boundaries).
//
// Expiry is driven through the module's injectable clock
// ([`advance_notice_clock_ms`]); no test sleeps, and `clear_all_notices()`
// resets the offset so a shifted clock never leaks into the next test.
// ---------------------------------------------------------------------------

/// Minimal `tracing` subscriber that counts WARN events whose message contains
/// a needle — used to prove that drops are reported, and reported *bounded*
/// (once per sweep, not once per entry).
#[derive(Debug)]
struct WarnMessageCounter {
    needle: &'static str,
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

struct MessageVisitor<'a> {
    found: &'a mut Option<String>,
}

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.found = Some(format!("{value:?}"));
        }
    }
}

impl tracing::Subscriber for WarnMessageCounter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.level() == &tracing::Level::WARN
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().level() != &tracing::Level::WARN {
            return;
        }
        let mut found = None;
        event.record(&mut MessageVisitor { found: &mut found });
        if let Some(message) = found
            && message.contains(self.needle)
        {
            self.count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// (a) A notice older than the TTL is never delivered, it is removed from the
/// inbox *and* from `PENDING_NOTICES`, and the drop is counted and reported.
#[tokio::test]
async fn test_expired_notice_is_not_delivered_and_is_reported() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let stale = post_notice_to_worker("coder-t-001", "Instruction that arrived too late", None);
    assert!(!is_notice_expired(&stale));
    let expired_before = notice_lifecycle_stats().expired_dropped_total;

    // Move the injectable clock past the TTL boundary (inclusive), no sleeping.
    advance_notice_clock_ms(NOTICE_TTL_MS + 60_000);
    assert!(is_notice_expired(&stale));
    assert!(notice_age_ms(&stale) >= NOTICE_TTL_MS);
    assert!(notice_now_ms_for_test() >= stale.created_at_ms + NOTICE_TTL_MS);

    let outcome = drain_worker_notices_report("coder-t-001");
    assert!(
        !outcome
            .delivered
            .iter()
            .any(|n| n.notice_id == stale.notice_id),
        "an expired notice must never reach a worker"
    );
    assert!(
        outcome.expired_dropped >= 1,
        "the expired notice must be counted in the drain report"
    );
    assert!(outcome.expired_ids.contains(&stale.notice_id));

    let stats = notice_lifecycle_stats();
    assert!(
        stats.expired_dropped_total > expired_before,
        "the drop must show up in the observable totals"
    );
    assert_eq!(
        worker_inbox_len("coder-t-001"),
        0,
        "the expired notice must not stay queued"
    );
    assert!(
        !has_worker_inbox("coder-t-001"),
        "the emptied inbox entry must be reclaimed, not left as an empty queue"
    );
    assert!(
        get_pending_notice(&stale.notice_id).is_none(),
        "the pending copy must not survive expiry"
    );
}

/// The expiry warning is emitted once per sweep (with the ids), not once per
/// expired notice.
#[tokio::test]
async fn test_expired_notice_warning_is_bounded_not_per_entry() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    for i in 0..3 {
        post_notice_to_worker("coder-t-001", &format!("stale instruction {i}"), None);
    }
    advance_notice_clock_ms(NOTICE_TTL_MS + 60_000);

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tracing::subscriber::with_default(
        WarnMessageCounter {
            needle: "steering notices expired",
            count: counter.clone(),
        },
        || {
            let outcome = drain_worker_notices_report("coder-t-001");
            assert!(
                outcome.expired_dropped >= 3,
                "all three queued notices were past the TTL"
            );
            assert_eq!(
                counter.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "one bounded warning per sweep, never per entry"
            );
        },
    );
}

/// (b) A notice inside the TTL window is delivered normally (and nothing is
/// reported as expired).
#[tokio::test]
async fn test_notice_inside_ttl_is_delivered() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let fresh = post_notice_to_worker("coder-t-001", "Still actionable instruction", None);
    // Stay a full minute clear of the boundary so wall-clock jitter inside the
    // test cannot flip the verdict.
    advance_notice_clock_ms(NOTICE_TTL_MS - 60_000);
    assert!(!is_notice_expired(&fresh));

    let outcome = drain_worker_notices_report("coder-t-001");
    assert!(
        !outcome.expired_ids.contains(&fresh.notice_id),
        "a notice inside the TTL must never be reported as expired"
    );
    assert!(
        outcome
            .delivered
            .iter()
            .any(|n| n.notice_id == fresh.notice_id),
        "a notice inside the TTL must be delivered"
    );
    let delivered = outcome
        .delivered
        .iter()
        .find(|n| n.notice_id == fresh.notice_id)
        .expect("the fresh notice must be delivered");
    assert_eq!(delivered.user_inquiry, "Still actionable instruction");
    // Delivered but unanswered notices stay pending until they expire/reply.
    assert!(get_pending_notice(&fresh.notice_id).is_some());
}

/// (c) An over-capacity inbox applies the documented **drop-oldest** policy,
/// observably: the evicted notices come back on the outcome, they are counted,
/// they are dropped from `PENDING_NOTICES`, and a warning names them.
#[tokio::test]
async fn test_over_capacity_inbox_drops_oldest_observably() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let flood = INBOX_CAPACITY + 6;
    let evicted_before = notice_lifecycle_stats().capacity_evicted_total;
    let mut posted: Vec<SteerNotice> = Vec::new();
    let mut evicted_ids: Vec<String> = Vec::new();

    for i in 0..flood {
        let outcome =
            post_notice_to_worker_tracked("coder-t-9142", &format!("instruction {i}"), None);
        posted.push(outcome.notice.clone());
        if i < INBOX_CAPACITY {
            assert!(
                outcome.evicted_older.is_empty(),
                "no eviction while the inbox is within capacity"
            );
        } else {
            // Drop-oldest: the notice evicted by post `i` is the one posted
            // `INBOX_CAPACITY` posts earlier.
            assert_eq!(outcome.evicted_older.len(), 1);
            assert_eq!(
                outcome.evicted_older[0].notice_id,
                posted[i - INBOX_CAPACITY].notice_id,
                "overflow must evict the oldest queued notice, never the newest"
            );
            evicted_ids.push(outcome.evicted_older[0].notice_id.clone());
        }
    }

    let stats = notice_lifecycle_stats();
    assert_eq!(
        worker_inbox_len("coder-t-9142"),
        INBOX_CAPACITY,
        "the inbox stays bounded at `INBOX_CAPACITY` no matter how chatty the poster"
    );
    assert!(
        stats.inbox_keys >= 1,
        "the flooded worker keeps exactly one inbox entry"
    );
    assert_eq!(
        stats.capacity_evicted_total - evicted_before,
        (flood - INBOX_CAPACITY) as u64,
        "every overflow must be counted"
    );

    for id in &evicted_ids {
        assert!(
            get_pending_notice(id).is_none(),
            "an evicted notice must not linger in `PENDING_NOTICES` — losing it has to leave a trace, not a phantom entry"
        );
    }

    // Only the newest `INBOX_CAPACITY` notices survive, in chronological order.
    let drained = drain_worker_notices("coder-t-9142");
    let expected: Vec<&str> = posted[flood - INBOX_CAPACITY..]
        .iter()
        .map(|n| n.notice_id.as_str())
        .collect();
    let mine: Vec<&str> = drained
        .iter()
        .map(|n| n.notice_id.as_str())
        .filter(|id| posted.iter().any(|n| n.notice_id == **id))
        .collect();
    assert_eq!(
        mine, expected,
        "the inbox keeps exactly the newest `INBOX_CAPACITY` notices, oldest delivery first"
    );
    for id in &evicted_ids {
        assert!(
            !mine.contains(&id.as_str()),
            "an evicted notice must never be delivered"
        );
    }
}

/// The overflow eviction is logged with the dropped ids — a chatty manager can
/// never overflow an inbox without a trace in the log.
#[tokio::test]
async fn test_inbox_overflow_eviction_is_logged() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();
    let evicted_before = notice_lifecycle_stats().capacity_evicted_total;

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tracing::subscriber::with_default(
        WarnMessageCounter {
            needle: "exceeded its capacity",
            count: counter.clone(),
        },
        || {
            for i in 0..INBOX_CAPACITY + 1 {
                post_notice_to_worker("coder-t-9143", &format!("instruction {i}"), None);
            }
        },
    );
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the drop-oldest eviction must be warned about exactly once"
    );
    assert_eq!(
        notice_lifecycle_stats().capacity_evicted_total - evicted_before,
        1,
        "the eviction must be counted, not just logged"
    );
}

/// (d) Inboxes of workers that are gone do not leak map entries: a fully
/// drained inbox is removed, and the inbox of a worker that never drains is
/// reclaimed together with its expired notice. The explicit teardown hook never
/// discards queued notices.
#[tokio::test]
async fn test_gone_worker_inbox_entries_are_reclaimed() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    // 1. Draining empties the queue *and* drops the map entry.
    let reclaimed_before = notice_lifecycle_stats().inboxes_reclaimed_total;
    let answered_later = post_notice_to_worker("gone-worker-t-900", "do the thing", None);
    assert_eq!(worker_inbox_len("gone-worker-t-900"), 1);
    assert!(has_worker_inbox("gone-worker-t-900"));
    assert_eq!(drain_worker_notices("gone-worker-t-900").len(), 1);
    assert!(
        !has_worker_inbox("gone-worker-t-900"),
        "a drained inbox entry must not linger in the map"
    );
    assert_eq!(worker_inbox_len("gone-worker-t-900"), 0);

    // 2. A worker that never drains: once its notice ages out, the entry that
    //    held it is reclaimed in the same sweep.
    let stale = post_notice_to_worker("dead-worker-t-901", "never drained", None);
    assert_eq!(worker_inbox_len("dead-worker-t-901"), 1);
    advance_notice_clock_ms(NOTICE_TTL_MS + 60_000);
    let report = prune_expired_notices();
    assert!(
        report.expired_dropped >= 2,
        "the queued notice and the delivered-but-unanswered one both age out"
    );
    assert!(report.expired_ids.contains(&stale.notice_id));
    assert!(report.expired_ids.contains(&answered_later.notice_id));
    assert!(report.reclaimed_inboxes >= 1);
    assert!(
        !has_worker_inbox("dead-worker-t-901"),
        "the dead worker's inbox entry must be reclaimed"
    );
    assert_eq!(worker_inbox_len("dead-worker-t-901"), 0);
    assert!(
        notice_lifecycle_stats().inboxes_reclaimed_total > reclaimed_before,
        "reclamation must be observable in the totals"
    );

    // 3. The teardown hook reclaims bookkeeping only — it never loses a queued
    //    notice, so a replacement worker for the same key can still be steered.
    let queued = post_notice_to_worker("coder-t-9144", "answer me before you finish", None);
    let reclaim = reclaim_worker_inbox("coder-t-9144");
    assert_eq!(reclaim.entries_removed, 0);
    assert!(reclaim.retained_queued_notices >= 1);
    let drained = drain_worker_notices("coder-t-9144");
    assert!(
        drained.iter().any(|n| n.notice_id == queued.notice_id),
        "reclaiming must never discard a queued notice"
    );
    assert_eq!(reclaim_worker_inbox("coder-t-9144").entries_removed, 0);
}

/// (3) The mid-turn drain API: exact routing, idempotent, self-cleaning, and it
/// reports (rather than silently loses) an expired notice found mid-turn.
#[tokio::test]
async fn test_mid_turn_drain_routes_exactly_and_is_idempotent() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let exact = post_notice_to_worker("coder-t-001", "Only for the exact worker", None);

    // Routing stays exact mid-turn: a validator whose key merely *contains* the
    // coder role must not pick up the coder's notice (recon H1), a longer task
    // id sharing the same text prefix addresses nobody, a different agent
    // sharing the task id does not answer an address that spells out another
    // agent's key, and the bare role address is not the exact key.
    assert!(drain_worker_notices_mid_turn("validator-coder-t-001").is_empty());
    assert!(drain_worker_notices_mid_turn("coder-t-0010").is_empty());
    assert!(drain_worker_notices_mid_turn("planner-t-001").is_empty());
    assert!(drain_worker_notices_mid_turn("coder-t-00").is_empty());

    // A role-family address reaches the worker that owns that role, mid-turn.
    let role = post_notice_to_worker("coder", "Wrap up before you finish", None);

    // The worker itself picks both up in the middle of its turn.
    let mid = drain_worker_notices_mid_turn("coder-t-001");
    assert!(
        mid.len() >= 2,
        "both notices addressed to this worker arrive mid-turn"
    );
    let ids: Vec<&str> = mid.iter().map(|n| n.notice_id.as_str()).collect();
    assert!(ids.contains(&role.notice_id.as_str()));
    assert!(ids.contains(&exact.notice_id.as_str()));

    // Idempotent: nothing is delivered twice, and an empty inbox leaves no entry.
    assert!(drain_worker_notices_mid_turn("coder-t-001").is_empty());
    assert!(
        !has_worker_inbox("coder-t-001"),
        "a drained mid-turn inbox leaves no map entry behind"
    );
}

/// A mid-turn drain that finds an expired notice drops it deterministically and
/// says so.
#[tokio::test]
async fn test_mid_turn_drain_drops_expired_notice_with_report() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let stale = post_notice_to_worker("coder-t-001", "Instruction from a previous round", None);
    advance_notice_clock_ms(NOTICE_TTL_MS + 60_000);

    let outcome = drain_worker_notices_report("coder-t-001");
    assert!(
        !outcome
            .delivered
            .iter()
            .any(|n| n.notice_id == stale.notice_id),
        "an expired notice must never be delivered, mid-turn or otherwise"
    );
    assert!(outcome.expired_dropped >= 1);
    assert!(outcome.expired_ids.contains(&stale.notice_id));
    assert!(outcome.reclaimed_inboxes >= 1);
    assert!(!has_worker_inbox("coder-t-001"));
}

// ---------------------------------------------------------------------------
// Reply-targeting regression tests — recon H6 (`record_worker_reply` used to
// *guess* the notice: `else if PENDING_NOTICES.len() == 1 { take the first }`).
//
// A reply must name the notice it answers. Every assertion below is keyed on
// the concrete notice ids / per-key deltas, never on absolute process-global
// counters, because other modules post notices concurrently in parallel runs.
// ---------------------------------------------------------------------------

/// (a) A reply carrying the explicit id resolves **exactly** that notice: the
/// right one is consumed and recorded, the worker's other notices are untouched.
#[tokio::test]
async fn test_reply_with_explicit_id_resolves_exactly_that_notice() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let first = post_notice_to_worker("coder-t-9201", "First instruction", None);
    let second = post_notice_to_worker("coder-t-9201", "Second instruction", None);

    let resolved = record_worker_reply_for_notice(
        "coder-t-9201",
        &second.notice_id,
        "Answering the second one first.",
    )
    .expect("an explicit id naming one of this worker's own pending notices is accepted");

    assert_eq!(resolved.notice_id, second.notice_id);
    assert_eq!(resolved.user_inquiry, "Second instruction");
    assert_eq!(resolved.target_worker, "coder-t-9201");

    // Exactly the named notice is resolved; its sibling is untouched.
    assert!(get_pending_notice(&second.notice_id).is_none());
    assert!(
        get_pending_notice(&first.notice_id).is_some(),
        "the sibling notice of the same worker must not be consumed"
    );

    // The reply is recorded under the id it named, and nowhere else.
    let reply =
        get_worker_reply(&second.notice_id).expect("the reply is recorded under its own id");
    assert_eq!(reply.notice_id, second.notice_id);
    assert_eq!(reply.worker_tag, "coder-t-9201");
    assert_eq!(reply.reply_message, "Answering the second one first.");
    assert!(get_worker_reply(&first.notice_id).is_none());
}

/// (b) A reply naming **another worker's** notice id is rejected deterministically
/// and leaves both notices pending — no reply is recorded, nothing is consumed.
#[tokio::test]
async fn test_reply_naming_another_workers_notice_id_is_rejected() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let coder_notice =
        post_notice_to_worker("coder-t-9201", "Instruction for the first coder", None);
    let sibling_notice =
        post_notice_to_worker("coder-t-9202", "Instruction for the other coder", None);

    // The first coder tries to answer the *other* coder's notice.
    let mismatch = record_worker_reply_for_notice(
        "coder-t-9201",
        &sibling_notice.notice_id,
        "Wrong worker answers",
    )
    .expect_err("a notice addressed to another worker is not resolvable by this worker");
    assert_eq!(
        mismatch,
        NoticeReplyRejection::WorkerMismatch {
            notice_id: sibling_notice.notice_id.clone(),
            worker_tag: "coder-t-9201".to_string(),
            addressed_to: "coder-t-9202".to_string(),
        }
    );
    // The rejection names both the id and the worker, in prose and by kind.
    assert_eq!(mismatch.kind(), "worker-mismatch");
    assert!(mismatch.to_string().contains(&sibling_notice.notice_id));
    assert!(mismatch.to_string().contains("coder-t-9201"));
    assert!(mismatch.to_string().contains("coder-t-9202"));

    // Nothing was consumed, nothing was recorded: both stay pending.
    assert!(get_pending_notice(&sibling_notice.notice_id).is_some());
    assert!(get_pending_notice(&coder_notice.notice_id).is_some());
    assert!(get_worker_reply(&sibling_notice.notice_id).is_none());
    assert!(get_worker_reply(&coder_notice.notice_id).is_none());

    // The mirror image is rejected too.
    let mirror = record_worker_reply_for_notice(
        "coder-t-9202",
        &coder_notice.notice_id,
        "Also the wrong worker",
    );
    assert_eq!(
        mirror.expect_err("mirror mismatch").kind(),
        "worker-mismatch"
    );

    // Every identity that does not route to the notice's addressee is rejected by
    // the same rule the router uses: a validator whose key *contains* `coder`, a
    // truncated prefix, an extended task id, another agent on the same task, and
    // the Manager.
    for impostor in [
        "validator-coder-t-9201",
        "coder-t-920",
        "coder-t-92010",
        "planner-t-9201",
        "Manager",
    ] {
        let res = record_worker_reply_for_notice(impostor, &coder_notice.notice_id, "guess");
        assert_eq!(
            res.expect_err("non-owner must be rejected").kind(),
            "worker-mismatch",
            "{impostor} must not resolve a notice addressed to coder-t-9201"
        );
    }
    assert!(get_pending_notice(&coder_notice.notice_id).is_some());

    // The scoped lookup exposes the same verdict as the reply path.
    assert!(get_pending_notice_for_worker(&coder_notice.notice_id, "coder-t-9202").is_none());
    assert!(get_pending_notice_for_worker(&coder_notice.notice_id, "coder-t-9201").is_some());

    // The owner still resolves it, right after all those rejections.
    let owner = record_worker_reply_for_notice("coder-t-9201", &coder_notice.notice_id, "Mine")
        .expect("the addressed worker resolves its own notice");
    assert_eq!(owner.notice_id, coder_notice.notice_id);
    assert!(get_pending_notice(&sibling_notice.notice_id).is_some());
    assert!(
        record_worker_reply_for_notice("coder-t-9202", &sibling_notice.notice_id, "Mine now")
            .is_ok()
    );
}

/// (c) A reply with no id, or an unknown id, is rejected — **no guess-the-notice
/// fallback** — and every rejection is logged naming the id and the worker.
#[tokio::test]
async fn test_reply_without_explicit_id_is_rejected_and_logged() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    // The exact precondition the deleted fallback keyed on: a single pending
    // notice exists, so "take the first pending one" would have resolved it.
    let mine = post_notice_to_worker("coder-t-9201", "The only instruction in flight", None);

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tracing::subscriber::with_default(
        WarnMessageCounter {
            needle: "rejected (",
            count: counter.clone(),
        },
        || {
            for bogus in ["", "   ", "notice-never-posted", "notice_1"] {
                let rejection =
                    record_worker_reply_for_notice("coder-t-9201", bogus, "Untargeted reply")
                        .expect_err(
                            "a reply that does not name one of this worker's notices is refused",
                        );
                let expected = if bogus.trim().is_empty() {
                    "missing-notice-id"
                } else {
                    "unknown-notice-id"
                };
                assert_eq!(rejection.kind(), expected, "for id {bogus:?}");
            }
        },
    );
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        4,
        "each rejection is warned about exactly once"
    );

    // No fallback fired: the worker's pending notice is neither consumed nor answered.
    assert!(
        get_pending_notice(&mine.notice_id).is_some(),
        "the pending notice must survive a reply that did not name it"
    );
    assert!(get_worker_reply(&mine.notice_id).is_none());

    // The source-compatible wrapper surfaces the same strictness as text.
    let missing = record_worker_reply("coder-t-9201", "", "x").expect_err("empty id rejected");
    assert!(missing.contains("notice_id"));
    let unknown = record_worker_reply("coder-t-9201", "notice-never-posted", "x")
        .expect_err("unknown id rejected");
    assert!(unknown.contains("notice-never-posted"));
    assert!(unknown.contains("coder-t-9201"));
    assert!(get_pending_notice(&mine.notice_id).is_some());
}

/// (d) A duplicate reply for an already-resolved notice is rejected; it cannot
/// overwrite the recorded reply nor consume anything else.
#[tokio::test]
async fn test_duplicate_reply_for_resolved_notice_is_rejected() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice = post_notice_to_worker("coder-t-9201", "Instruction answered twice", None);

    let first = record_worker_reply_for_notice("coder-t-9201", &notice.notice_id, "First answer")
        .expect("the first reply resolves the notice it names");
    assert_eq!(first.notice_id, notice.notice_id);

    let duplicate = record_worker_reply_for_notice(
        "coder-t-9201",
        &notice.notice_id,
        "Second answer to an already-resolved notice",
    )
    .expect_err("a second reply to a resolved notice resolves nothing");
    assert_eq!(
        duplicate,
        NoticeReplyRejection::AlreadyReplied {
            notice_id: notice.notice_id.clone(),
            worker_tag: "coder-t-9201".to_string(),
        }
    );
    assert_eq!(duplicate.kind(), "already-replied");
    assert!(duplicate.to_string().contains(&notice.notice_id));

    // The duplicate left no trace: the first reply is still the recorded one.
    let recorded = get_worker_reply(&notice.notice_id).expect("first reply still recorded");
    assert_eq!(recorded.reply_message, "First answer");
    assert!(get_pending_notice(&notice.notice_id).is_none());
}

/// The harness follow-up round re-posts the *same* id (`src/harness/mod.rs:562`);
/// after that the id is pending again and answerable once more — which is the
/// only way a "duplicate" can legitimately reappear.
#[tokio::test]
async fn test_follow_up_repost_of_same_notice_id_is_answerable_again() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice = post_notice_to_worker(
        "coder-t-9205",
        "Please migrate the format",
        Some("notice-h6-fu"),
    );

    record_worker_reply_for_notice("coder-t-9205", &notice.notice_id, "Migrated.")
        .expect("first round of the dialogue");

    let reposted = post_notice_to_worker(
        "coder",
        "What about backward compatibility?",
        Some("notice-h6-fu"),
    );
    assert_eq!(reposted.notice_id, notice.notice_id);

    let second_round =
        record_worker_reply_for_notice("coder-t-9205", &notice.notice_id, "Kept a reader shim.")
            .expect("the re-posted notice is pending again, so it is answerable");
    assert_eq!(
        second_round.user_inquiry,
        "What about backward compatibility?"
    );
    assert_eq!(
        get_worker_reply(&notice.notice_id)
            .expect("latest reply")
            .reply_message,
        "Kept a reader shim."
    );

    // Answering it twice in a row is still rejected.
    assert_eq!(
        record_worker_reply_for_notice("coder-t-9205", &notice.notice_id, "again")
            .expect_err("duplicate")
            .kind(),
        "already-replied"
    );
}

/// The identity tolerance mirrors the router: the bare role name the harness
/// reports (`ToolCaller::role_name()`) addresses the same worker as its full
/// registry key, a broadcast address is answerable by any worker, and a
/// different role is not.
#[tokio::test]
async fn test_reply_identity_matches_the_same_routing_rules() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let task_keyed = post_notice_to_worker("coder-t-9201", "Addressed by registry key", None);
    let disambiguated =
        post_notice_to_worker("coder-t-9202#7", "Addressed by disambiguated key", None);
    let validator =
        post_notice_to_worker("validator-coder-t-9203", "Re-audit the deliverable", None);
    let validator_round_two =
        post_notice_to_worker("validator-coder-t-9204", "Re-audit round two", None);

    // Same worker, different spelling of its identity.
    assert!(
        record_worker_reply_for_notice("coder", &task_keyed.notice_id, "role name replies").is_ok()
    );
    assert!(
        record_worker_reply_for_notice(
            "coder-t-9202",
            &disambiguated.notice_id,
            "natural key replies"
        )
        .is_ok()
    );
    assert!(
        record_worker_reply_for_notice(
            "validator-coder",
            &validator.notice_id,
            "role family replies"
        )
        .is_ok()
    );

    // A different role can never answer it.
    assert_eq!(
        record_worker_reply_for_notice("coder", &validator_round_two.notice_id, "wrong role")
            .expect_err("coder must not answer a validator notice")
            .kind(),
        "worker-mismatch"
    );
    assert!(get_pending_notice(&validator_round_two.notice_id).is_some());
    assert!(get_worker_reply(&validator_round_two.notice_id).is_none());

    // The predicate is also asserted directly, without the store involved —
    // including the broadcast wildcards, which are answered by any worker.
    assert!(notice_addresses_worker(&validator, "validator"));
    assert!(notice_addresses_worker(
        &validator,
        "validator-coder-t-9203"
    ));
    assert!(!notice_addresses_worker(&validator, "coder"));
    assert!(!notice_addresses_worker(&validator, "coder-t-9203"));
    assert!(!notice_addresses_worker(&validator, ""));

    for wildcard in ["*", "worker"] {
        let broadcast = SteerNotice {
            notice_id: format!("notice-h6-{wildcard}"),
            user_inquiry: "Everyone: wrap up".to_string(),
            target_worker: wildcard.to_string(),
            created_at_ms: notice_now_ms_for_test(),
        };
        assert!(notice_addresses_worker(&broadcast, "coder-t-9201"));
        assert!(notice_addresses_worker(
            &broadcast,
            "validator-coder-t-9203"
        ));
    }
}

/// A reply also retires the queued (not yet delivered) copy of the notice it
/// resolved, so a worker is never prompted to answer something it already
/// answered.
#[tokio::test]
async fn test_reply_retires_the_queued_copy_of_the_resolved_notice() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let queued_before = worker_inbox_len("coder-t-9201");
    let resolved_by_reply =
        post_notice_to_worker("coder-t-9201", "Answered before it was delivered", None);
    let still_queued = post_notice_to_worker("coder-t-9201", "Genuinely still queued", None);
    assert_eq!(
        worker_inbox_len("coder-t-9201"),
        queued_before + 2,
        "both notices are queued for this exact worker"
    );

    record_worker_reply_for_notice(
        "coder-t-9201",
        &resolved_by_reply.notice_id,
        "Already handled.",
    )
    .expect("the worker may answer a notice it has seen but not yet drained");

    let delivered = drain_worker_notices_report("coder-t-9201").delivered;
    let ids: Vec<&str> = delivered.iter().map(|n| n.notice_id.as_str()).collect();
    assert!(
        !ids.contains(&resolved_by_reply.notice_id.as_str()),
        "a notice resolved by an explicit reply must never be delivered again"
    );
    assert!(ids.contains(&still_queued.notice_id.as_str()));
}

/// The rendering is what makes the strict contract satisfiable: it surfaces the
/// exact id a reply has to name.
#[test]
fn test_rendered_notice_surfaces_the_id_the_reply_must_name() {
    let notice = SteerNotice {
        notice_id: "notice-render-1".to_string(),
        user_inquiry: "Which allocator did you pick?".to_string(),
        target_worker: "coder-t-9201".to_string(),
        created_at_ms: 0,
    };

    let rendered = render_notice_for_worker(&notice);
    assert!(rendered.contains("notice-render-1"));
    assert!(rendered.contains("ID: notice-render-1"));
    assert!(rendered.contains("notice_id: \"notice-render-1\""));
    assert!(rendered.contains(crate::tool_names::TOOL_REPLY_TO_ARBITRATOR));
    assert!(rendered.contains("Which allocator did you pick?"));
}
