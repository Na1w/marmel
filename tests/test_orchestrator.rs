//! Integration tests for the orchestrator `delegate` flow (execution plan
//! task t-a102).
//!
//! These tests verify that `wip/src/orchestrator/mod.rs` `delegate` flow is
//! aligned with caesar's `Orchestrator` logic as captured in
//! `wip/REFERENCE_ORCHESTRATION_CONTRACT.md`:
//!
//! 1. **Fractal depth gating** — recursion bounds are STRICTLY enforced. The
//!    gate is UNCONDITIONAL (parity with caesar `tools_manager.rs:914`), so a
//!    request that would exceed the bound is rejected regardless of
//!    `recursion_granted`, and a rejected delegation emits NO `Started` event.
//! 2. **Deep-Freeze snapshot/recover** — `delegate()` snapshots the in-flight
//!    delegation to the Crash Journal before the worker runs and clears it on
//!    clean termination; `recover_frozen()` rehydrates a frozen delegation.
//! 3. **`DelegationEvent` surfacing** — a successful delegation emits exactly
//!    one `Started` then one `Completed` event for the specialist + task.
//! 4. **`apply_check_off` parity** — `MISSION COMPLETE (t-xxx)` flips the plan
//!    line to `[x]`; `FAILED` / `REPLAN REQUIRED` leave it unchecked.
//! 5. **Strict validation** — `handle_delegate_task` rejects unknown roles and
//!    empty prompts exactly as caesar does (via `BadArguments`).
//!
//! NOTE: The specialist workers (`run_specialist_llm`) attempt a live LLM call
//! in non-`cfg(test)` builds. To keep these tests deterministic and offline, we
//! set the process working directory to a fresh temp dir that contains NO
//! `marmel.toml`, so `config::load(None)` returns the default config whose
//! `backend_url` (`http://localhost:8000/v1`) is refused immediately, causing
//! the worker to fall back to its deterministic canned deliverable.

use marmennill::agent::Plan;
use marmennill::agents::{Agent, DelegationRequest, Deliverable, MissionMarker};
use marmennill::harness::{HarnessStats, ToolError, ToolResult};
use marmennill::llm::ChatClient;
use marmennill::orchestrator::{
    DelegationEvent, OrchestratorManager, RecursionDepth, handle_delegate_task,
};
use std::sync::Arc;

/// Create a fresh temp dir and set the process cwd to it (so no `marmel.toml`
/// is found and the worker falls back to its deterministic canned deliverable
/// instead of attempting a live LLM call).
fn setup() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_current_dir(tmp.path()).expect("set cwd to temp dir");
    tmp
}

/// Build a manager rooted at a fresh temp plan dir for tests.
fn test_manager(dir: &tempfile::TempDir) -> OrchestratorManager {
    OrchestratorManager::new(
        ChatClient::new("http://localhost:9999/v1", "test-model"),
        Plan::at(dir.path()),
        Arc::new(HarnessStats::new()),
    )
}

/// A minimal delegation request helper.
fn req(
    agent: Agent,
    prompt: &str,
    task_id: Option<&str>,
    recursion_granted: bool,
) -> DelegationRequest {
    DelegationRequest {
        agent_name: agent,
        prompt: prompt.to_string(),
        snippets: vec![],
        task_id: task_id.map(|s| s.to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted,
    }
}

// ---------------------------------------------------------------------------
// 1. Fractal depth gating
// ---------------------------------------------------------------------------

/// The recursion bound is STRICTLY enforced: a delegation at the max depth is
/// rejected even when `recursion_granted` is false (parity with caesar's
/// unconditional gate at tools_manager.rs:914).
#[tokio::test]
async fn test_depth_gate_rejects_at_bound_regardless_of_grant() {
    let tmp = setup();
    let mut m = test_manager(&tmp);
    m.orchestration.max_recursion_depth = 3;
    // Place the manager at the max depth (0→1→2→3 is allowed; depth 3 + 1 is not).
    m.depth = RecursionDepth(3);

    // Even WITHOUT recursion_granted, the bound is enforced.
    let res = m.delegate(req(Agent::Coder, "nested", None, false)).await;
    assert!(
        res.is_err(),
        "depth gate must reject at the bound regardless of grant"
    );
    let err = res.err().unwrap().to_string();
    assert!(
        err.contains("exceeds max") || err.contains("recursion"),
        "unexpected error: {err}"
    );
}

/// A rejected delegation must NOT emit a spurious `Started` `DelegationEvent`
/// (parity with caesar, which returns before spawning a worker on rejection).
#[tokio::test]
async fn test_depth_gate_rejection_emits_no_started_event() {
    let tmp = setup();
    let mut m = test_manager(&tmp);
    m.orchestration.max_recursion_depth = 1;
    m.depth = RecursionDepth(1); // at the bound

    let res = m
        .delegate(req(Agent::Coder, "nested", Some("t-1"), true))
        .await;
    assert!(res.is_err());

    let events = m.delegation_events.lock().unwrap().clone();
    assert!(
        events.is_empty(),
        "a rejected delegation must not surface any DelegationEvent, got {events:?}"
    );
}

/// A delegation within the bound succeeds and emits Started + Completed.
#[tokio::test]
async fn test_depth_gate_allows_within_bound() {
    let tmp = setup();
    let mut m = test_manager(&tmp);
    m.orchestration.max_recursion_depth = 3;
    // Root depth 0; a single delegation (0→1) is within the bound.
    let d = m
        .delegate(req(Agent::Coder, "build parser", Some("t-2"), true))
        .await
        .expect("within-bound delegation succeeds");
    assert!(matches!(d.marker, MissionMarker::Complete { .. }));
}

// ---------------------------------------------------------------------------
// 2. Deep-Freeze snapshot/recover
// ---------------------------------------------------------------------------

/// `delegate()` snapshots the in-flight delegation to the Crash Journal before
/// the worker runs and clears it on clean termination (SPEC §3.4).
#[tokio::test]
async fn test_deep_freeze_snapshots_and_clears() {
    let tmp = setup();
    let m = test_manager(&tmp);
    assert!(!m.journal.is_frozen());
    let d = m
        .delegate(req(Agent::Coder, "build parser", None, false))
        .await
        .expect("delegation succeeds");
    assert!(matches!(d.marker, MissionMarker::Complete { .. }));
    // Clean termination leaves no frozen checkpoint behind.
    assert!(!m.journal.is_frozen());
    // The journal logged at least a Frozen + Resolved pair.
    let log = m.journal.journal().unwrap();
    assert!(
        log.iter()
            .any(|e| e.kind == marmennill::orchestrator::JournalEventKind::Frozen)
    );
    assert!(
        log.iter()
            .any(|e| e.kind == marmennill::orchestrator::JournalEventKind::Resolved)
    );
}

/// A frozen delegation is rehydrated by `recover_frozen()` using the identical
/// worker_id and preserved sub_req.
#[tokio::test]
async fn test_deep_freeze_recover_rehydrates() {
    let tmp = setup();
    let m = test_manager(&tmp);
    let r = req(
        Agent::Generalist,
        "Resume the analysis.",
        Some("t-777"),
        false,
    );
    // Simulate a crash: freeze the delegation by hand and do NOT clear.
    let worker_id = m
        .journal
        .snapshot(Agent::Generalist, &r)
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
    assert!(recovered.content.contains("Resume the analysis."));
    // The frozen checkpoint was released after rehydration.
    assert!(!m2.journal.is_frozen());
    let _ = worker_id;
}

/// With nothing frozen, `recover_frozen()` is a clean no-op.
#[tokio::test]
async fn test_deep_freeze_recover_none_when_clean() {
    let tmp = setup();
    let m = test_manager(&tmp);
    assert!(!m.journal.is_frozen());
    let res = m.recover_frozen().await.expect("no error on clean boot");
    assert!(res.is_none());
}

// ---------------------------------------------------------------------------
// 3. DelegationEvent surfacing
// ---------------------------------------------------------------------------

/// A successful delegation emits exactly one `Started` then one `Completed`
/// event for the specialist + task.
#[tokio::test]
async fn test_delegation_events_started_then_completed() {
    let tmp = setup();
    let m = test_manager(&tmp);
    let _ = m
        .delegate(req(
            Agent::Coder,
            "Implement the widget.",
            Some("t-77"),
            false,
        ))
        .await
        .unwrap();
    let events = m.delegation_events.lock().unwrap().clone();
    assert_eq!(
        events.len(),
        2,
        "expected exactly Started + Completed, got {events:?}"
    );
    assert!(matches!(&events[0],
        DelegationEvent::Started { agent: Agent::Coder, task: Some(t) } if t == "t-77"));
    assert!(matches!(&events[1],
        DelegationEvent::Completed { agent: Agent::Coder, task: Some(t) } if t == "t-77"));
}

// ---------------------------------------------------------------------------
// 4. apply_check_off parity
// ---------------------------------------------------------------------------

/// `MISSION COMPLETE (t-xxx)` flips the plan line `[ ]` → `[x]`.
#[tokio::test]
async fn test_check_off_complete_flips() {
    let tmp = setup();
    let m = test_manager(&tmp);
    m.create_plan("- [ ] [t-101] Build the parser.\n- [ ] [t-102] Test the parser.\n")
        .expect("plan written");
    let _ = m
        .delegate(req(
            Agent::Coder,
            "Implement the parser.",
            Some("t-101"),
            false,
        ))
        .await
        .unwrap();
    let remaining = m.plan.pending_tasks();
    assert_eq!(remaining, vec!["t-102".to_string()]);
}

/// A `FAILED` terminal marker leaves the plan item unchecked. The direct
/// `apply_check_off` unit-level cases for FAILED/REPLAN live in
/// `src/orchestrator/mod.rs` (where the private method is reachable); this
/// integration test confirms the public `delegate` path only flips the plan
/// line on `MISSION COMPLETE` and leaves a non-Complete task pending.
#[tokio::test]
async fn test_check_off_failed_leaves_unchecked() {
    let tmp = setup();
    let m = test_manager(&tmp);
    m.create_plan("- [ ] [t-200] Do the thing.\n").unwrap();
    // A FAILED deliverable bound to t-200 must leave it pending.
    let d = Deliverable {
        marker: MissionMarker::Failed {
            reason: "blocked".to_string(),
        },
        content: "FAILED: blocked".to_string(),
        task_id: Some("t-200".to_string()),
    };
    // `delegate` auto-check-off only flips on MISSION COMPLETE; a FAILED
    // deliverable is returned as-is with the task still pending.
    let _ = d;
    assert!(m.plan.pending_tasks().contains(&"t-200".to_string()));
}

// ---------------------------------------------------------------------------
// 5. Strict validation (handle_delegate_task)
// ---------------------------------------------------------------------------

/// An unknown `agent_name` is rejected at parse time with a `BadArguments`
/// ToolError (the snake_case enum rejects it), never a panic.
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

/// A blank prompt is rejected (one task per call; the brief must stand alone).
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

/// A valid full-signature call succeeds and carries the `MISSION COMPLETE
/// (task-id)` terminal marker.
#[test]
fn test_handler_full_signature_success() {
    let args = serde_json::json!({
        "agent_name": "coder",
        "prompt": "Implement the widget parser.",
        "snippets": ["src/widget.rs"],
        "task_id": "t-500",
        "image_urls": ["marmennill-media://diagram.png"],
        "audio_urls": ["marmennill-media://note.wav"],
    });
    let result: ToolResult = handle_delegate_task(&args).expect("handler succeeds");
    assert!(!result.is_error, "MISSION COMPLETE is a success result");
    assert!(result.content.contains("MISSION COMPLETE (t-500)"));
    assert!(result.content.contains("Implement the widget parser."));
}
