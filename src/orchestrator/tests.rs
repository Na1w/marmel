use super::*;
use crate::agents::DelegationRequest;
use crate::harness::{ToolError, ToolResult};
use crate::tool_names::TOOL_DELEGATE_TASK;
use std::path::Path;

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
        "task_id": "t-001",
        "snippets": [],
    });
    let err = handle_delegate_task(&args).expect_err("empty prompt rejected");
    assert!(matches!(err, ToolError::BadArguments { .. }));
}

#[test]
fn test_handler_rejects_missing_or_empty_task_id() {
    // Missing task_id
    let args_missing = serde_json::json!({
        "agent_name": "coder",
        "prompt": "Implement widget.",
        "snippets": [],
    });
    let err = handle_delegate_task(&args_missing).expect_err("missing task_id rejected");
    assert!(matches!(err, ToolError::BadArguments { .. }));

    // Empty task_id
    let args_empty = serde_json::json!({
        "agent_name": "coder",
        "prompt": "Implement widget.",
        "task_id": "   ",
        "snippets": [],
    });
    let err = handle_delegate_task(&args_empty).expect_err("empty task_id rejected");
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

// --- M8: the Silent Dispatcher must never read "the plan could not be read" as
//     "the mission is finished" (`docs/recon_bugs_manager.md` M8). The gate now
//     uses the fallible plan API added in t-031h (`Plan::try_pending_tasks`), so
//     a read failure is an Err, a successfully-read empty pending list is the
//     only completion signal, and dispatch of real pending tasks is unchanged.

/// Minimal ERROR-level log capture (same shape as the WARN counter in
/// `steer_tests.rs`): proves the plan-read failure is *logged* loudly, not only
/// returned as a `Result`.
#[derive(Clone, Debug, Default)]
struct ErrorLogCapture {
    messages: Arc<std::sync::Mutex<Vec<String>>>,
}

impl ErrorLogCapture {
    fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
}

struct MessageVisitor<'a> {
    found: &'a mut Vec<String>,
}

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.found.push(format!("{value:?}"));
        }
    }
}

impl tracing::Subscriber for ErrorLogCapture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.level() == &tracing::Level::ERROR
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().level() != &tracing::Level::ERROR {
            return;
        }
        let mut messages = self.messages.lock().unwrap();
        event.record(&mut MessageVisitor {
            found: &mut messages,
        });
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// M8 (i): an execution plan that exists but **cannot be read** must abort the
/// Silent Dispatcher with an explicit, logged error — it must never return `Ok`
/// (which the Manager reads as "the Executing phase is done"), and it must never
/// delegate anything or touch the plan on disk.
#[tokio::test]
async fn test_orchestr_run_executing_plan_read_error_is_not_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = test_manager(&tmp);
    std::fs::create_dir_all(m.plan.dir()).unwrap();
    // A plan file that exists but is not decodable: `fs::read_to_string` fails.
    let undecodable: [u8; 12] = [
        0xff, 0xfe, 0x00, b'-', b' ', b'[', b' ', b']', b' ', b'[', b't', b'-',
    ];
    std::fs::write(m.plan.plan_path(), undecodable).unwrap();

    // Precondition at the plan layer: the state is UNKNOWN. The old Vec-shaped
    // wrapper still degrades to an empty list — which is exactly the lie this
    // call site used to act on.
    assert!(m.plan.exists(), "the plan file is on disk");
    assert!(
        m.plan.try_pending_tasks().is_err(),
        "the fallible API must surface the read failure"
    );
    assert!(
        m.plan.pending_tasks().is_empty(),
        "the compatibility wrapper still swallows the failure"
    );
    assert!(
        !m.plan.is_complete(),
        "an unreadable plan is never complete (M8)"
    );

    let capture = ErrorLogCapture::default();
    let guard = tracing::subscriber::set_default(capture.clone());
    let res = m.run_executing(&|_| Agent::Coder).await;
    drop(guard);

    let err =
        res.expect_err("M8: a plan read failure must NOT return Ok — the mission is not complete");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("could not be read") && msg.contains("execution_plan.md"),
        "the error must name the plan and the failure, got: {msg}"
    );
    assert!(
        msg.contains("UNKNOWN") && msg.contains("NOT complete"),
        "the error must state that the plan state is unknown and the mission is \
         NOT complete, got: {msg}"
    );

    // Nothing was dispatched and the plan bytes on disk are untouched: no work
    // was silently dropped.
    assert!(
        m.delegation_events.lock().unwrap().is_empty(),
        "a plan read failure must not dispatch (or drop) any task"
    );
    assert_eq!(
        std::fs::read(m.plan.plan_path()).expect("plan file still there"),
        undecodable,
        "the failed run must not mutate the plan"
    );
    assert!(
        m.plan.try_pending_tasks().is_err(),
        "the plan state is still UNKNOWN after the failed run"
    );

    // The failure is logged as an error, not merely returned.
    let logged = capture.messages();
    assert!(
        logged.iter().any(|entry| {
            entry.contains("Silent Dispatcher stopped") && entry.contains("could not be read")
        }),
        "expected an explicit ERROR log naming the failure, got: {logged:?}"
    );
}

/// M8 (ii): a plan that **was read successfully** and has nothing pending —
/// every box ticked, or no plan file at all — keeps the pre-existing completion
/// path: `Ok(..)`, no delegation, and no error logged. This is what distinguishes
/// "nothing pending" from "cannot read" in the migrated gate.
#[tokio::test]
async fn test_orchestr_run_executing_empty_pending_list_still_completes() {
    let capture = ErrorLogCapture::default();
    let guard = tracing::subscriber::set_default(capture.clone());

    // (a) A readable plan whose tasks are all checked off.
    let tmp = tempfile::tempdir().unwrap();
    let mut m = test_manager(&tmp);
    m.create_plan("- [x] [t-800] Already done\n- [x] (t-801) Also done\n")
        .unwrap();
    assert!(
        m.plan
            .try_pending_tasks()
            .expect("readable plan")
            .is_empty(),
        "the pending list is legitimately empty"
    );
    let done = m
        .run_executing(&|_| Agent::Coder)
        .await
        .expect("a successfully-read empty pending list still completes");
    assert!(done.is_empty(), "there was nothing to dispatch");
    assert!(
        m.delegation_events.lock().unwrap().is_empty(),
        "no delegation for an already finished plan"
    );

    // (b) No plan file at all: also a successful read with nothing pending.
    let tmp2 = tempfile::tempdir().unwrap();
    let mut m2 = test_manager(&tmp2);
    assert!(!m2.plan.exists());
    assert!(
        m2.plan
            .try_pending_tasks()
            .expect("no plan file is not an error")
            .is_empty()
    );
    let done2 = m2
        .run_executing(&|_| Agent::Coder)
        .await
        .expect("a missing plan file is a legitimate empty pending set");
    assert!(done2.is_empty());

    drop(guard);
    let logged = capture.messages();
    assert!(
        logged.is_empty(),
        "the completion path must not log an error, got: {logged:?}"
    );
}

/// M8 (iii): the normal path is unchanged — pending tasks are dispatched once
/// each, checked off on completion, and the loop terminates via the empty-pending
/// completion path.
#[tokio::test]
async fn test_orchestr_run_executing_dispatches_pending_tasks_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = test_manager(&tmp);
    m.create_plan("- [ ] [t-810] Build the parser\n- [ ] [t-811] Write the tests\n")
        .unwrap();
    assert_eq!(
        m.plan.try_pending_tasks().unwrap(),
        vec!["t-810".to_string(), "t-811".to_string()]
    );

    let dispatched: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let scheduler = {
        let dispatched = dispatched.clone();
        move |task_id: &str| {
            dispatched.lock().unwrap().push(task_id.to_string());
            if task_id == "t-811" {
                Agent::Debugger
            } else {
                Agent::Coder
            }
        }
    };

    let results = m
        .run_executing(&scheduler)
        .await
        .expect("pending tasks dispatch normally");
    assert_eq!(results.len(), 2, "one deliverable per plan task");
    assert_eq!(
        *dispatched.lock().unwrap(),
        vec!["t-810".to_string(), "t-811".to_string()],
        "each pending task dispatched exactly once"
    );
    assert!(
        results.iter().all(|d| d.task_id.is_some()),
        "deliverables stay bound to their task id"
    );

    // The same completion signal as before the migration: everything checked off.
    assert!(
        m.plan.try_pending_tasks().unwrap().is_empty(),
        "all tasks checked off by the dispatcher"
    );
    assert!(m.plan.is_complete(), "the plan reached completion");
    let events = m.delegation_events.lock().unwrap().clone();
    assert_eq!(events.len(), 4, "Started + Completed per delegation");
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

#[test]
fn test_handle_delegate_task_terminal_marker_deduplication() {
    let fail_deliverable = Deliverable {
        marker: MissionMarker::Failed {
            reason: "Syntax error\n\nFAILED (Validator rejected deliverable)".to_string(),
        },
        content: "Syntax error\n\nFAILED (Validator rejected deliverable)".to_string(),
        task_id: Some("t-123".to_string()),
    };

    let content = fail_deliverable.content.trim();
    let res = if content.contains("FAILED") {
        ToolResult::err(content.to_string())
    } else {
        ToolResult::err(format!("{content}\n\nFAILED: {}", fail_deliverable.content))
    };

    assert!(res.is_error);
    let count = res.content.matches("FAILED").count();
    assert_eq!(
        count, 1,
        "Failure message must not duplicate FAILED marker: {}",
        res.content
    );
}

/// Concurrency regression (Phase 2): 20 concurrent emitters hammer
/// `emit_status` / `emit_event` (clone-the-sender-out-and-drop pattern) while
/// a SINGLE drainer task consumes the channels. No panic, no deadlock, fast
/// completion. (Unbounded channels + a single drainer — multiple drainers
/// would race over message ownership.)
#[tokio::test]
async fn test_bus_hammer_concurrent_emitters() {
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<crate::ui::Event>();
    set_status_sender(status_tx);
    set_event_sender(event_tx);

    let n_emitters = 20;
    let per_emitter = 50;
    let mut handles = Vec::with_capacity(n_emitters);
    for i in 0..n_emitters {
        handles.push(tokio::spawn(async move {
            for j in 0..per_emitter {
                emit_status(format!("status-{i}-{j}"));
                emit_event(crate::ui::Event::Message(format!("event-{i}-{j}")));
            }
        }));
    }
    for h in handles {
        h.await.expect("emitter task panicked");
    }

    // Single drainer: drain both channels to exhaustion (bounded by a
    // generous wall-clock guard so a hang fails the test instead of hanging).
    // Count only OUR messages by pattern — other lib tests (e.g. preemption)
    // may emit into the process-global bus concurrently, and those must be
    // tolerated, not counted.
    let drained = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut status_count = 0usize;
        while let Ok(msg) = status_rx.try_recv() {
            if msg.starts_with("status-") {
                status_count += 1;
            }
        }
        let mut event_count = 0usize;
        while let Ok(ev) = event_rx.try_recv() {
            if let crate::ui::Event::Message(text) = ev
                && text.starts_with("event-")
            {
                event_count += 1;
            }
        }
        (status_count, event_count)
    })
    .await
    .expect("drain must complete quickly (no deadlock)");
    let (status_count, event_count) = drained;

    // Every emit from our 20 emitters must have landed in the channels
    // (unbounded: no drops).
    let expected = n_emitters * per_emitter;
    assert_eq!(
        status_count, expected,
        "expected exactly {expected} status messages, got {status_count}"
    );
    assert_eq!(
        event_count, expected,
        "expected exactly {expected} events, got {event_count}"
    );
}

#[test]
fn test_stream_identity_matches_decorated_task_ids() {
    // Dedup cluster C1 part 2: `StreamIdentity::matches` normalizes all three
    // inputs through `crate::task_id`, and decoration-only / empty targets stay
    // "absent" (never a wildcard match).
    let identity = StreamIdentity {
        agent_tag: "coder-t-001".to_string(),
        agent_name: Some("coder".to_string()),
        task_id: Some("[t-001]".to_string()),
        cancel_token: None,
    };
    assert!(identity.matches(Some("[Coder]"), "\"t-001\""));
    assert!(identity.matches(None, "(t-001)"));
    assert!(identity.matches(Some("'coder'"), ""));
    assert!(!identity.matches(None, ""));
    assert!(!identity.matches(None, "[]"));
    assert!(!identity.matches(None, "t-999"));
}

/// Re-export guard (t-048): the live worker loops in `src/agents/runner/*` reach
/// the notice API through the crate-level `crate::orchestrator::…` path. Pinning
/// those paths here means a rename inside `notice.rs` fails at compile time
/// instead of silently leaving a turn loop without a drain seam.
#[test]
fn test_notice_api_is_reachable_through_the_crate_level_path() {
    let _drain_mid_turn =
        crate::orchestrator::drain_worker_notices_mid_turn as fn(&str) -> Vec<SteerNotice>;
    let _drain_report = crate::orchestrator::drain_worker_notices_report
        as fn(&str) -> crate::orchestrator::notice::NoticeDrainOutcome;
    let _render = crate::orchestrator::render_notice_for_worker as fn(&SteerNotice) -> String;
    let _addresses = crate::orchestrator::notice_addresses_worker as fn(&SteerNotice, &str) -> bool;
    let _get_for_worker =
        crate::orchestrator::get_pending_notice_for_worker as fn(&str, &str) -> Option<SteerNotice>;
    let _record_reply = crate::orchestrator::record_worker_reply_for_notice
        as fn(&str, &str, &str) -> Result<SteerNotice, crate::orchestrator::NoticeReplyRejection>;

    let notice = SteerNotice {
        notice_id: "notice-reexport-probe".to_string(),
        user_inquiry: "probe the re-export".to_string(),
        target_worker: "probeagent-t-reexport-probe".to_string(),
        created_at_ms: 0,
    };
    // The rendering carries the notice id verbatim — that is what makes the
    // strict reply contract satisfiable for a worker.
    assert!(
        crate::orchestrator::render_notice_for_worker(&notice).contains("notice-reexport-probe")
    );
    assert!(crate::orchestrator::notice_addresses_worker(
        &notice,
        "probeagent-t-reexport-probe"
    ));
    assert!(!crate::orchestrator::notice_addresses_worker(
        &notice,
        "probeagent-t-reexport-other"
    ));
}

/// Single-rendering guard (t-048): the transcript text of a steering notice is
/// produced in exactly ONE place in `src/` — `render_notice_for_worker`.
///
/// Before t-048 there were three copies of that format string (the canonical
/// renderer, one inline copy in the specialist loop, one in the shared fix
/// loop), and the inline copies had already drifted from the reply contract the
/// renderer promises. Two live loops each with their own rendering is how a
/// worker gets told one reply protocol by one turn and another by the next.
#[test]
fn test_notice_transcript_is_rendered_in_exactly_one_place() {
    // Assembled rather than spelled out, so this file cannot match its own
    // needle (the same trick the raw-tool-name literal guard plays).
    let needle = format!("[Steering {} Arbitrator", "Notice from");

    let mut hits: Vec<(String, usize)> = Vec::new();
    walk_src_for(Path::new("src"), &needle, &mut hits);

    let total: usize = hits.iter().map(|(_, count)| count).sum();
    assert_eq!(
        hits.len(),
        1,
        "the notice transcript rendering must live in exactly one src file, found {hits:?}"
    );
    assert_eq!(
        total, 1,
        "exactly one rendering of a steering notice may exist in src/, found {total}: {hits:?}"
    );
    assert!(
        hits[0].0.ends_with("orchestrator/notice.rs"),
        "the single notice rendering must belong to `render_notice_for_worker` in \
         src/orchestrator/notice.rs, got {}",
        hits[0].0
    );
}

fn walk_src_for(dir: &Path, needle: &str, out: &mut Vec<(String, usize)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_src_for(&path, needle, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let count = content.matches(needle).count();
            if count > 0 {
                out.push((path.to_string_lossy().replace('\\', "/"), count));
            }
        }
    }
}

// --- Gate t-055: the disk-first prompt lookup in `OrchestratorManager::delegate`
//     (`saved_prompt_path_for_task`) is grammar-gated by
//     `crate::task_id::validate_task_id` — the single grammar authority for task
//     ids. A rejected id is a typed refusal, so the join never happens, the read
//     is never attempted, and the delegation degrades exactly like a missing
//     prompt file: JIT prompt synthesis.

/// WARN-level capture for the rejected-task-id boundary (same shape as
/// [`ErrorLogCapture`] above, at WARN instead of ERROR).
#[derive(Clone, Default)]
struct RejectedTaskIdWarns(Arc<std::sync::atomic::AtomicUsize>);

struct WarnMessageVisitor<'a> {
    found: &'a mut Option<String>,
}

impl tracing::field::Visit for WarnMessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.found = Some(format!("{value:?}"));
        }
    }
}

impl tracing::Subscriber for RejectedTaskIdWarns {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.level() == &tracing::Level::WARN
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _record: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().level() != &tracing::Level::WARN {
            return;
        }
        let mut found = None;
        event.record(&mut WarnMessageVisitor { found: &mut found });
        if let Some(message) = found
            && message.contains("Rejected task id")
        {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Every hostile task-id spelling is refused with its own typed variant, and no
/// path is ever returned — so the read that used to follow the join is never
/// attempted, and nothing is created in the prompts directory either.
#[test]
fn test_saved_prompt_path_refuses_hostile_task_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let plan_dir = tmp.path().join("plan");
    let prompts_dir = plan_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir).unwrap();
    // Exactly where the un-gated `prompts/{clean}.md` join resolved for the id
    // `../escape`: one level ABOVE the prompts directory.
    let planted = plan_dir.join("escape.md");
    std::fs::write(&planted, "planted blueprint outside the prompts dir").unwrap();
    let long = format!("t-{}", "y".repeat(70));

    assert_eq!(
        saved_prompt_path_for_task(&prompts_dir, Some("../escape")).err(),
        Some(crate::task_id::TaskIdError::DotDotSegment)
    );
    assert_eq!(
        saved_prompt_path_for_task(&prompts_dir, Some("a/b")).err(),
        Some(crate::task_id::TaskIdError::PathSeparator { ch: '/' })
    );
    assert_eq!(
        saved_prompt_path_for_task(&prompts_dir, Some("..")).err(),
        Some(crate::task_id::TaskIdError::DotDotSegment)
    );
    assert_eq!(
        saved_prompt_path_for_task(&prompts_dir, Some("")).err(),
        Some(crate::task_id::TaskIdError::Empty)
    );
    assert_eq!(
        saved_prompt_path_for_task(&prompts_dir, Some(long.as_str())).err(),
        Some(crate::task_id::TaskIdError::TooLong {
            len: long.chars().count(),
            max: crate::task_id::MAX_TASK_ID_LEN,
        })
    );
    assert!(
        saved_prompt_path_for_task(&prompts_dir, None)
            .unwrap()
            .is_none(),
        "a request without a task id simply has no disk prompt to look up"
    );

    let stray: Vec<_> = std::fs::read_dir(&prompts_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(
        stray.is_empty(),
        "refused ids must not create anything in the prompts dir: {stray:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&planted).unwrap(),
        "planted blueprint outside the prompts dir",
        "the file outside the prompts dir was neither read nor written"
    );
}

/// A normal `t-0NN` id — plain or decorated — still resolves to exactly one file
/// name inside the prompts directory and round-trips through the disk-first
/// lookup, so the gate does not change the happy path.
#[test]
fn test_saved_prompt_path_round_trips_a_normal_task_id() {
    let tmp = tempfile::tempdir().unwrap();
    let plan_dir = tmp.path().join("plan");
    let prompts_dir = plan_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir).unwrap();

    let bp = crate::agents::AgentBlueprint {
        role_name: "coder_specialist".to_string(),
        reasoning: "disk-first round trip".to_string(),
        selected_skills: vec!["testing".to_string()],
        allowed_tools: vec![TOOL_DELEGATE_TASK.to_string()],
        system_prompt: "Disk-first prompt round trip for t-055.".to_string(),
        task_id: Some("t-055".to_string()),
    };
    let written = bp.save_to_disk(&prompts_dir).expect("normal id persists");
    assert_eq!(written, prompts_dir.join("t-055.md"));

    for raw in ["t-055", "[t-055]", "\"t-055\""] {
        let path = saved_prompt_path_for_task(&prompts_dir, Some(raw))
            .unwrap_or_else(|e| panic!("{raw:?} must be accepted by the grammar authority: {e}"))
            .unwrap_or_else(|| panic!("{raw:?} must resolve to a prompt path"));
        assert_eq!(
            path,
            prompts_dir.join("t-055.md"),
            "{raw:?} must resolve to the pre-generated file inside the prompts dir"
        );
        let loaded = crate::agents::AgentBlueprint::load_from_disk(&path)
            .expect("the pre-generated prompt is readable");
        assert_eq!(loaded.role_name, "coder_specialist");
    }
}

/// End-to-end at the delegation boundary: a refused task id must not fail the
/// delegation (the JIT-synthesis fallback is the contract), must be logged as a
/// warning naming the rejected id, and must never read or write outside the
/// prompts directory.
#[test]
fn test_delegate_with_rejected_task_id_falls_back_to_jit_and_warns() {
    let tmp = tempfile::tempdir().unwrap();
    let m = test_manager(&tmp);
    let prompts_dir = tmp.path().join("prompts");
    std::fs::create_dir_all(&prompts_dir).unwrap();
    // The un-gated join for `../escape` resolved to `<plan dir>/escape.md`.
    let planted = tmp.path().join("escape.md");
    let planted_text = "---\nrole_name: \"planted_outside_prompts\"\n---\n\nplanted\n";
    std::fs::write(&planted, planted_text).unwrap();

    let req = DelegationRequest {
        agent_name: Agent::Coder,
        prompt: "Implement the escape probe.".to_string(),
        snippets: vec![],
        task_id: Some("../escape".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let deliverable =
        tracing::subscriber::with_default(RejectedTaskIdWarns(counter.clone()), || {
            rt.block_on(m.delegate(req))
        })
        .expect("a rejected task id must not fail the delegation: JIT synthesis is the fallback");
    assert_eq!(
        deliverable.task_id.as_deref(),
        Some("../escape"),
        "the deliverable stays bound to the id as supplied — it is never rewritten"
    );
    assert!(
        counter.load(std::sync::atomic::Ordering::Relaxed) >= 1,
        "the rejection must be reported with a tracing::warn! naming the rejected id"
    );

    let events = m.delegation_events.lock().unwrap().clone();
    assert!(
        events.iter().any(
            |e| matches!(e, DelegationEvent::Started { task: Some(t), .. } if t == "../escape")
        ),
        "the JIT fallback delegation still ran to completion: {events:?}"
    );

    assert!(
        !prompts_dir.join("escape.md").exists(),
        "no prompt file may be derived from the rejected id inside the prompts dir"
    );
    assert_eq!(
        std::fs::read_to_string(&planted).unwrap(),
        planted_text,
        "the file outside the prompts dir must be neither read into the run nor rewritten"
    );
}
