use super::*;
use crate::agents::DelegationRequest;

/// Build a manager rooted at a fresh temp plan dir for tests.
fn test_manager(dir: &tempfile::TempDir) -> OrchestratorManager {
    OrchestratorManager::new(
        ChatClient::new("http://localhost:9999/v1", "test-model"),
        Plan::at(dir.path()),
        Arc::new(HarnessStats::new()),
    )
}

#[test]
fn test_format_duration_human_minutes_and_seconds() {
    assert_eq!(format_duration_human(0), "0s");
    assert_eq!(format_duration_human(45), "45s");
    assert_eq!(format_duration_human(60), "1m 0s");
    assert_eq!(format_duration_human(135), "2m 15s");
    assert_eq!(format_duration_human(3665), "61m 5s");
}

/// REQ-ORCH-003: a delegated IsolatedContext contains ONLY the specialist's
/// role system prompt + task brief + bounded snippets — never the Manager's
/// `messages[]`. The produced context engine must start with exactly two
/// messages (`[0]` = role system prompt, `[1]` = brief) and must not expose
/// any Manager transcript or the Manager's own conversation history.
#[tokio::test]
async fn test_orchestr_context_isolation() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);

    // The Manager has a client with a backend URL and model; a well-behaved
    // orchestrator must never forward that (or any Manager transcript) into
    // a delegated subagent's isolated context.
    let req = DelegationRequest {
        agent_name: Agent::Coder,
        prompt: "Implement the widget parser.".to_string(),
        snippets: vec!["src/widget.rs".to_string()],
        task_id: Some("t-101".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let d = m.delegate(req.clone()).await.expect("delegation succeeds");
    // The deliverable's content must reference the isolated role/brief and
    // never contain any "Manager transcript" (there is none to leak).
    assert!(matches!(d.marker, MissionMarker::Complete { .. }));
    assert!(d.content.contains("Implement the widget parser."));

    // Build the exact engine the specialist would receive from this request
    // and prove the isolation invariant at the message level.
    let entry = m.registry.resolve(req.agent_name).unwrap();
    let ctx = IsolatedContext::from_request(m.role_prompt_for(entry.agent), &req);
    let engine = ctx.into_engine(4096);
    let msgs = engine.messages();
    // Exactly two messages: the specialist's role system prompt and the brief.
    assert_eq!(msgs.len(), 2, "isolated context has exactly 2 messages");
    match &msgs[0] {
        crate::types::Message::System { content } => {
            assert!(
                content.contains("Coder") || content.contains("Software Engineer"),
                "messages[0] is the role system prompt"
            );
        }
        other => panic!("messages[0] must be a System role prompt, got {other:?}"),
    }
    match &msgs[1] {
        crate::types::Message::User { content } => {
            assert_eq!(content, "Implement the widget parser.");
        }
        other => panic!("messages[1] must be the brief, got {other:?}"),
    }
}

/// REQ-ORCH-005 / REQ-PLAN-002: MISSION COMPLETE (t-xxx) flips `[t-xxx]` →
/// `[x]`; a FAILED marker leaves it unchecked.
#[tokio::test]
async fn test_orchestr_task_checkoff_complete_flips() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    m.create_plan("- [ ] [t-101] Build the parser.\n- [ ] [t-102] Test the parser.\n")
        .expect("plan written");

    let req = DelegationRequest {
        agent_name: Agent::Coder,
        prompt: "Implement the parser.".to_string(),
        snippets: vec![],
        task_id: Some("t-101".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let _ = m.delegate(req).await.unwrap();
    let remaining = m.plan.pending_tasks();
    assert_eq!(remaining, vec!["t-102".to_string()]);
}

/// REQ-ORCH-005 / REQ-PLAN-002: a FAILED terminal marker leaves the item
/// unchecked.
#[tokio::test]
async fn test_orchestr_task_checkoff_failed_leaves_unchecked() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    m.create_plan("- [ ] [t-200] Do the thing.\n").unwrap();

    // Simulate a deliverable that parses to FAILED.
    let d = Deliverable {
        marker: MissionMarker::Failed {
            reason: "blocked".to_string(),
        },
        content: "FAILED: blocked".to_string(),
        task_id: Some("t-200".to_string()),
    };
    let d = m.apply_check_off(d, Some("t-200".to_string()));
    assert_eq!(d.task_id.as_deref(), Some("t-200"));
    assert!(m.plan.pending_tasks().contains(&"t-200".to_string()));
}

/// REQ-ORCH-005 / REQ-PLAN-002: a `REPLAN REQUIRED` terminal marker also
/// leaves the plan item unchecked — only `MISSION COMPLETE` flips `[ ]`→`[x]`.
#[tokio::test]
async fn test_orchestr_task_checkoff_replan_leaves_unchecked() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    m.create_plan("- [ ] [t-300] Re-architect the module.\n")
        .unwrap();

    // Simulate a deliverable that parses to REPLAN REQUIRED.
    let d = Deliverable {
        marker: MissionMarker::Replan {
            reason: "goal needs revisiting".to_string(),
        },
        content: "REPLAN REQUIRED: schema changed".to_string(),
        task_id: Some("t-300".to_string()),
    };
    let d = m.apply_check_off(d, Some("t-300".to_string()));
    assert_eq!(d.task_id.as_deref(), Some("t-300"));
    assert!(
        m.plan.pending_tasks().contains(&"t-300".to_string()),
        "REPLAN REQUIRED must leave the item unchecked"
    );
}

/// t-302 (a): `apply_check_off` leaves the task UNCHECKED even when the
/// content body contains the literal string `MISSION COMPLETE`, as long as
/// the authoritative `Deliverable.marker` is `Failed` or `Replan`. The
/// marker — not the free-form body — is the gate keeper.
#[tokio::test]
async fn test_orchestr_apply_checkoff_marker_failed_overrides_body_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    let plan = "- [ ] [t-401] Step one\n- [ ] [t-402] Step two\n";
    m.create_plan(plan).unwrap();

    // Marker FAILED, but the body leaks a stale MISSION COMPLETE token from a
    // pre-validation draft. This must NOT check the task off.
    let d = Deliverable {
        marker: MissionMarker::Failed {
            reason: "validator rejected".to_string(),
        },
        content: "MISSION COMPLETE (t-401) — actually the validator rejected this.".to_string(),
        task_id: Some("t-401".to_string()),
    };
    let d = m.apply_check_off(d, Some("t-401".to_string()));
    assert_eq!(d.task_id.as_deref(), Some("t-401"));
    assert!(
        m.plan.pending_tasks().contains(&"t-401".to_string()),
        "Failed marker with stale MISSION COMPLETE body must stay unchecked"
    );

    // Same invariant for the Replan marker.
    let d2 = Deliverable {
        marker: MissionMarker::Replan {
            reason: "goal changed".to_string(),
        },
        content: "MISSION COMPLETE (t-402) — just a stale draft.".to_string(),
        task_id: Some("t-402".to_string()),
    };
    let d2 = m.apply_check_off(d2, Some("t-402".to_string()));
    assert_eq!(d2.task_id.as_deref(), Some("t-402"));
    assert!(
        m.plan.pending_tasks().contains(&"t-402".to_string()),
        "Replan marker with stale MISSION COMPLETE body must stay unchecked"
    );
}

/// t-302 (b): `apply_check_off` checks off a task ONLY when the deliverable
/// carries a genuine `MissionMarker::Complete` whose content body also
/// retains the `MISSION COMPLETE` terminal marker.
#[tokio::test]
async fn test_orchestr_apply_checkoff_only_checks_on_genuine_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    m.create_plan("- [ ] [t-501] Step one\n- [ ] [t-502] Step two\n")
        .unwrap();

    // Genuine Complete marker + body marker -> checked off.
    let d = Deliverable {
        marker: MissionMarker::Complete { task_id: None },
        content: "all done MISSION COMPLETE (t-501)".to_string(),
        task_id: Some("t-501".to_string()),
    };
    let d = m.apply_check_off(d, Some("t-501".to_string()));
    assert_eq!(d.task_id.as_deref(), Some("t-501"));
    assert!(
        !m.plan.pending_tasks().contains(&"t-501".to_string()),
        "genuine Complete must check t-501 off"
    );

    // A Complete marker whose content was later revoked (stale) must NOT
    // check off, because check_plan_on_marker re-parses the *content*.
    let d2 = Deliverable {
        marker: MissionMarker::Complete { task_id: None },
        content: "REVOKED before finalization".to_string(),
        task_id: Some("t-502".to_string()),
    };
    let d2 = m.apply_check_off(d2, Some("t-502".to_string()));
    assert_eq!(d2.task_id.as_deref(), Some("t-502"));
    assert!(
        m.plan.pending_tasks().contains(&"t-502".to_string()),
        "Complete marker without a content-side completion token must not check off"
    );
}

/// t-302 (c): a REJECTED deliverable's content retains the validator critique
/// in a `VALIDATOR REJECTION` block, so the downstream consumer can act on it.
#[tokio::test]
async fn test_orchestr_apply_checkoff_rejected_retains_critique_content() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    m.create_plan("- [ ] [t-601] Verify module\n").unwrap();

    // A REJECTED deliverable: Failed marker, body carries the structured
    // VALIDATOR REJECTION block produced by the validator feedback loop.
    let critique = "VALIDATOR REJECTION: assertions failed on line 12\n---------------\nlast revision body MISSION COMPLETE REVOKED";
    let d = Deliverable {
        marker: MissionMarker::Failed {
            reason: "validator rejected".to_string(),
        },
        content: critique.to_string(),
        task_id: Some("t-601".to_string()),
    };
    let d = m.apply_check_off(d, Some("t-601".to_string()));
    // The returned deliverable retains the FULL critique block verbatim.
    assert!(d.content.contains("VALIDATOR REJECTION"));
    assert!(d.content.contains("assertions failed on line 12"));
    // And the REJECTED marker leaves the task unchecked (parity with t-302 a).
    assert!(m.plan.pending_tasks().contains(&"t-601".to_string()));
}

/// REQ-ORCH-001 fractal: nested delegation beyond max_recursion_depth is
/// rejected.
#[tokio::test]
async fn test_orchestr_fractal_depth_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = test_manager(&tmp);
    m.orchestration.max_recursion_depth = 3;
    // Descend three levels (0→1→2→3 is allowed); the fourth (depth 3 + 1)
    // must be rejected because step(3) with max 3 returns None.
    let mut depth = RecursionDepth::root();
    for _ in 0..3 {
        depth = depth.step(m.orchestration.max_recursion_depth).unwrap();
    }
    assert_eq!(depth.0, 3);
    assert!(depth.step(m.orchestration.max_recursion_depth).is_none());
    m.depth = depth;
    let req = DelegationRequest {
        agent_name: Agent::Generalist,
        prompt: "nested".to_string(),
        snippets: vec![],
        task_id: None,
        image_urls: None,
        audio_urls: None,
        recursion_granted: true,
    };
    let res = m.delegate(req).await;
    assert!(res.is_err());
    let err = res.err().unwrap().to_string();
    assert!(err.contains("exceeds max") || err.contains("recursion"));
}

#[test]
fn test_orchestr_recursion_depth_step_boundary() {
    assert_eq!(RecursionDepth::root().step(3), Some(RecursionDepth(1)));
    let d = RecursionDepth(3);
    assert_eq!(d.step(3), None);
}

#[test]
fn test_orchestr_synthesize_joins_deliverables() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    let a = Deliverable {
        marker: MissionMarker::Complete { task_id: None },
        content: "first".to_string(),
        task_id: None,
    };
    let b = Deliverable {
        marker: MissionMarker::Complete { task_id: None },
        content: "second".to_string(),
        task_id: None,
    };
    let out = m.synthesize(&[a, b]);
    assert!(out.contains("first"));
    assert!(out.contains("second"));
}

/// Deep-Freeze: `delegate()` snapshots the in-flight delegation to the
/// Crash Journal and clears it once the worker returns (SPEC §3.4). After a
/// clean run there is nothing frozen left on disk.
#[tokio::test]
async fn test_orchestr_delegate_snapshots_and_clears() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    let req = DelegationRequest {
        agent_name: Agent::Coder,
        prompt: "Build the parser.".to_string(),
        snippets: vec![],
        task_id: None,
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    assert!(!m.journal.is_frozen());
    let d = m.delegate(req).await.expect("delegation succeeds");
    assert!(matches!(d.marker, MissionMarker::Complete { .. }));
    // Clean termination leaves no frozen checkpoint behind.
    assert!(!m.journal.is_frozen());
    // The journal logged at least a Frozen + Resolved pair.
    let log = m.journal.journal().unwrap();
    assert!(log.iter().any(|e| e.kind == JournalEventKind::Frozen));
    assert!(log.iter().any(|e| e.kind == JournalEventKind::Resolved));
}

/// Deep-Freeze recovery: a manually frozen delegation is rehydrated by
/// `recover_frozen()` using the identical worker_id and preserved sub_req.
#[tokio::test]
async fn test_orchestr_recover_frozen_rehydrates_identical_worker() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    let req = DelegationRequest {
        agent_name: Agent::Generalist,
        prompt: "Resume the analysis.".to_string(),
        snippets: vec!["notes.md".to_string()],
        task_id: Some("t-777".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    // Simulate a crash: freeze the delegation by hand (as `delegate()` would
    // at start) and do NOT clear — as if the process died mid-run.
    let worker_id = m
        .journal
        .snapshot(Agent::Generalist, &req)
        .expect("snapshot written");
    assert!(m.journal.is_frozen());

    // Rehydrate on a "restarted" Manager rooted at the same plan dir.
    let m2 = test_manager(&tmp);
    let recovered = m2
        .recover_frozen()
        .await
        .expect("recovery succeeds")
        .expect("a frozen delegation existed");
    assert!(matches!(recovered.marker, MissionMarker::Complete { .. }));
    // The preserved brief is what was re-executed.
    assert!(recovered.content.contains("Resume the analysis."));
    // The frozen checkpoint was released after rehydration.
    assert!(!m2.journal.is_frozen());
    let _ = worker_id;
}

/// Deep-Freeze recovery: with nothing frozen, `recover_frozen()` is a clean
/// no-op (returns `None`).
#[tokio::test]
async fn test_orchestr_recover_frozen_none_when_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    assert!(!m.journal.is_frozen());
    let res = m.recover_frozen().await.expect("no error on clean boot");
    assert!(res.is_none());
}

/// Deep-Freeze recovery: a frozen delegation whose agent is no longer
/// registered fails loudly (the frozen task is marked Failed, not silently
/// dropped), satisfying "rehydrate OR properly fail".
#[tokio::test]
async fn test_orchestr_recover_frozen_fails_when_role_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    // Freeze a delegation into the journal dir directly.
    let m = test_manager(&tmp);
    let req = DelegationRequest {
        agent_name: Agent::Coder,
        prompt: "orphan task".to_string(),
        snippets: vec![],
        task_id: None,
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let wid = m.journal.snapshot(Agent::Coder, &req).unwrap();

    // A manager with an EMPTY registry cannot rehydrate the frozen role.
    let mut m2 = test_manager(&tmp);
    m2.registry = SpecialistRegistry::default();
    let err = m2
        .recover_frozen()
        .await
        .expect_err("fails loudly when role unknown");
    assert!(err.to_string().contains("cannot be rehydrated"));
    // The frozen checkpoint is released with a Failed journal event.
    assert!(!m2.journal.is_frozen());
    assert!(
        m2.journal
            .journal()
            .unwrap()
            .iter()
            .any(|e| e.kind == JournalEventKind::Failed && e.worker_id == wid)
    );
}

// --- REQ-ORCH-005: `handle_delegate_task` handler-level tests ---

/// REQ-ORCH-005 canonical signature: `handle_delegate_task` accepts the full
/// payload `(agent_name, prompt, snippets, task_id?, image_urls?, audio_urls?)`
/// where `agent_name` is the snake_case enum. It returns the specialist's
/// deliverable as a `ToolResult` whose content carries the `MISSION COMPLETE
/// (task-id)` terminal marker, and is synchronous-from-Manager (blocks until
/// the specialist returns — this call itself completes in-line).
#[test]
fn test_orchestr_handler_full_signature_and_success_marker() {
    let args = serde_json::json!({
        "agent_name": "coder",
        "prompt": "Implement the widget parser.",
        "snippets": ["src/widget.rs"],
        "task_id": "t-500",
        "image_urls": ["marmennill-media://diagram.png"],
        "audio_urls": ["marmennill-media://note.wav"],
    });
    let result = handle_delegate_task(&args).expect("handler succeeds");
    assert!(!result.is_error, "MISSION COMPLETE is a success result");
    assert!(result.content.contains("MISSION COMPLETE (t-500)"));
    assert!(result.content.contains("Implement the widget parser."));
}

/// REQ-ORCH-002: an unknown `agent_name` is rejected at parse time with a
/// `BadArguments` ToolError (the snake_case enum rejects it), never a panic.
#[test]
fn test_handler_rejects_unknown_agent() {
    let args = serde_json::json!({
        "agent_name": "planner",
        "prompt": "Nope.",
        "snippets": [],
    });
    let err = handle_delegate_task(&args).expect_err("unknown role rejected");
    assert!(matches!(err, ToolError::BadArguments { .. }));
}

/// REQ-ORCH-003/005: the brief MUST be self-contained and non-empty — a
/// blank prompt is rejected (one task per call; the subagent cannot see the
/// Manager's context, so the brief must stand alone).
#[test]
fn test_handler_rejects_empty_prompt() {
    let args = serde_json::json!({
        "agent_name": "coder",
        "prompt": "   ",
        "snippets": [],
    });
    let err = handle_delegate_task(&args).expect_err("empty prompt rejected");
    assert!(matches!(err, ToolError::BadArguments { .. }));
}

/// t6-REQ-1 / t6-REQ-2: `OrchestrationConfig::from_config` hydrates the
/// runtime orchestration config (recursion bound + manager module + tool
/// table) from the loaded `[orchestration]` TOML block, and
/// `OrchestratorManager::from_config` threads it through.
#[test]
fn test_orchestr_config_threads_into_manager() {
    use crate::config::Config;
    let mut cfg = Config::default();
    cfg.orchestration.max_recursion_depth = 5;
    cfg.orchestration.manager_module = "src/orchestrator/mod.rs".to_string();
    cfg.orchestration.specialists.insert(
        "coder".to_string(),
        crate::config::SpecialistConfig {
            module: "src/agents/coder.rs".to_string(),
            tools: vec![TOOL_DELEGATE_TASK.into(), "terminal__*".into()],
            model: None,
            ..Default::default()
        },
    );

    let tmp = tempfile::tempdir().unwrap();
    let m = OrchestratorManager::from_config(
        ChatClient::new("http://localhost:9999/v1", "test-model"),
        Plan::at(tmp.path()),
        Arc::new(HarnessStats::new()),
        &cfg,
    );
    assert_eq!(m.orchestration.max_recursion_depth, 5);
    assert_eq!(m.orchestration.manager_module, "src/orchestrator/mod.rs");
    assert_eq!(m.orchestration.specialists.get("coder").unwrap().len(), 2);
}

#[test]
fn test_orchestr_guard_rejects_domain_module() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = test_manager(&tmp);
    m.orchestration.manager_module = "src/agents/coder.rs".to_string();
    let err = m.guard_no_domain_work().expect_err("agent module rejected");
    assert!(err.to_string().contains("domain"));

    // Correct orchestrator module passes.
    m.orchestration.manager_module = "src/orchestrator/mod.rs".to_string();
    m.guard_no_domain_work()
        .expect("orchestrator module is fine");
}

#[tokio::test]
async fn test_orchestr_run_executing_rejects_domain_module() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = test_manager(&tmp);
    m.orchestration.manager_module = "src/agents/researcher.rs".to_string();
    m.create_plan("- [ ] [t-101] Research the topic.\n")
        .unwrap();
    let res = m.run_executing(&|_| Agent::Researcher).await;
    assert!(res.is_err());
}

#[tokio::test]
async fn test_orchestr_delegation_events_emitted() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    let req = DelegationRequest {
        agent_name: Agent::Coder,
        prompt: "Implement the widget.".to_string(),
        snippets: vec![],
        task_id: Some("t-77".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };
    let _ = m.delegate(req).await.unwrap();
    let events = m.delegation_events.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0],
            DelegationEvent::Started { agent: Agent::Coder, task: Some(t) } if t == "t-77"));
    assert!(matches!(&events[1],
            DelegationEvent::Completed { agent: Agent::Coder, task: Some(t) } if t == "t-77"));
}

#[test]
fn test_active_specialist_context_formatting() {
    let guard = register_active_worker(
        Some("t-123".to_string()),
        "coder".to_string(),
        "Do coding".to_string(),
    );
    update_active_worker_context(&guard.0, 3450);
    let formatted = get_active_specialist_context_str();
    assert!(
        formatted
            .as_deref()
            .is_some_and(|s| s.contains("coder-t-123: 3.5k"))
    );
    assert_eq!(get_active_worker_tokens("coder-t-123"), Some(3450));
    drop(guard);
    // Last known tokens are preserved after drop for Idle subagent rendering
    assert_eq!(get_active_worker_tokens("coder-t-123"), Some(3450));
}

#[test]
fn test_active_worker_context_tokens_rebirth_reduction() {
    let guard = register_active_worker(
        Some("t-456".to_string()),
        "coder".to_string(),
        "Do large work".to_string(),
    );
    // Before rebirth: large context
    update_active_worker_context(&guard.0, 8500);
    assert_eq!(get_active_worker_tokens("coder-t-456"), Some(8500));

    // After rebirth or compaction: context count drops
    update_active_worker_context(&guard.0, 450);
    assert_eq!(get_active_worker_tokens("coder-t-456"), Some(450));
}
