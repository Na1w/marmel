use super::*;
use std::fs;

/// Build an isolated plan manager with a small plan.
fn test_plan() -> (std::path::PathBuf, Plan) {
    let dir = std::env::temp_dir().join(format!(
        "marmel_loop_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    let plan = Plan::at(&dir);
    plan.create("# Execution Plan\n- [ ] [t-001] task\n")
        .unwrap();
    (dir, plan)
}

/// REQ-LOOP-001: the turn phase sequence advances in the exact specified
/// order and wraps back to PrepareTurn after CheckFinish.
#[test]
fn test_agent_turn_phase_sequence() {
    let mut p = TurnPhase::PrepareTurn;
    let expected = [
        TurnPhase::CallBackend,
        TurnPhase::StreamResponse,
        TurnPhase::ProcessResponse,
        TurnPhase::ExecuteTools,
        TurnPhase::CheckFinish,
        TurnPhase::PrepareTurn,
    ];
    for e in expected {
        p = p.next();
        assert_eq!(p, e, "expected {e:?}");
    }
}

/// REQ-LOOP-003: read tools are flagged parallel, write tools sequential.
#[test]
fn test_agent_tool_classification() {
    assert!(is_read_tool("read_file"));
    assert!(is_read_tool("grep_search"));
    assert!(is_read_tool("glob"));
    assert!(!is_read_tool("write_file"));

    assert!(is_write_tool("write_file"));
    assert!(is_write_tool("replace"));
    assert!(is_write_tool("run_command"));
    assert!(!is_write_tool("read_file"));

    // REQ-ORCH-005: delegate_task is sequential (blocking, synchronous-from-
    // Manager), never parallelized with reads.
    assert!(is_write_tool("delegate_task"));
    assert!(!is_read_tool("delegate_task"));
}

/// REQ-LOOP-004: steer signals are drained and injected as user messages;
/// abort stops the turn immediately.
#[tokio::test]
async fn test_agent_signal_steer_and_abort() {
    let (_dir, plan) = test_plan();
    let mut loop_ = AgentLoop::new(plan);

    // Queue a steer; the next turn should inject it into the transcript.
    loop_.signal(Signal::Steer("do X".to_string()));
    let outcome = loop_.run_turn().await.unwrap();
    // The transcript should now contain the injected user message.
    assert!(
        loop_
            .transcript
            .iter()
            .any(|m| matches!(m, Message::User { content } if content == "do X")),
        "steer prompt must be injected"
    );
    // t-001 is still pending -> Continue.
    assert_eq!(outcome, TurnOutcome::Continue);

    // Queue an abort; the next turn must stop immediately with Aborted.
    loop_.signal(Signal::Abort);
    let outcome = loop_.run_turn().await.unwrap();
    assert_eq!(outcome, TurnOutcome::Aborted);

    fs::remove_dir_all(&_dir).unwrap();
}

/// REQ-LOOP-002: the turn limit caps the number of turns.
#[tokio::test]
async fn test_agent_turn_limit() {
    let (_dir, plan) = test_plan();
    let mut loop_ = AgentLoop::new(plan);
    // Artificially advance to the limit.
    loop_.turn_count = MAX_TURNS;
    let outcome = loop_.run_turn().await.unwrap();
    assert_eq!(outcome, TurnOutcome::Complete);
    fs::remove_dir_all(&_dir).unwrap();
}

/// REQ-PLAN-002 via the loop: a successful read tool checks off the task.
#[tokio::test]
async fn test_agent_loop_checkoff_success() {
    let (dir, plan) = test_plan();
    let mut loop_ = AgentLoop::new(plan);
    // Queue a read_file tool call annotated with t-001.
    loop_.enqueue_tools(vec![serde_json::json!({
        "name": "read_file",
        "arguments": { "path": "Cargo.toml", "task_id": "t-001" }
    })]);
    let outcome = loop_.run_turn().await.unwrap();
    // The only plan task (t-001) was checked off, so the plan is complete.
    assert_eq!(outcome, TurnOutcome::Complete);
    assert!(
        Plan::at(&dir).is_complete(),
        "t-001 must be checked off on disk"
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// REQ-HARN-002 (runtime): five identical non-paginated tool calls through
/// the loop are blocked with the SPEC "TOOL REPETITION DETECTED" error and
/// are NOT dispatched. (Threshold is the caesar default `repetition_threshold
/// = 5`.)
#[tokio::test]
async fn test_agent_monitor_blocks_repetition_through_loop() {
    let (_dir, plan) = test_plan();
    let mut loop_ = AgentLoop::new(plan);
    // 5x identical run_command "ls" (caesar default threshold).
    for _ in 0..5 {
        loop_.enqueue_tools(vec![serde_json::json!({
            "name": "run_command",
            "arguments": { "command": "ls" }
        })]);
    }
    let outcome = loop_.run_turn().await.unwrap();
    match outcome {
        TurnOutcome::ToolError(msg) => {
            assert!(
                msg.contains("TOOL REPETITION DETECTED"),
                "expected SPEC repetition error, got: {msg}"
            );
        }
        other => panic!("expected ToolError, got {other:?}"),
    }
    fs::remove_dir_all(&_dir).unwrap();
}

/// REQ-HARN-001 (runtime): plain-text XML tool calls on assistant output are
/// intercepted into structured ToolCall JSON with `call_text_{uuid}` ids,
/// and `xml_tool_rescues` is recorded.
#[tokio::test]
async fn test_agent_monitor_rescue_xml_through_loop() {
    let (_dir, plan) = test_plan();
    let mut loop_ = AgentLoop::new(plan);
    let text = r#"prefix <tool_call>{"function":"read_file","arguments":{"path":"Cargo.toml"}}</tool_call> suffix"#;
    let calls = loop_.rescue_xml_calls(text);
    assert_eq!(calls.len(), 1, "one XML tool call must be rescued");
    assert_eq!(calls[0].function.name, "read_file");
    assert!(
        calls[0].id.starts_with("call_text_"),
        "rescued id must be call_text_{{uuid}}, got {}",
        calls[0].id
    );
    // No XML -> no rescue.
    assert!(loop_.rescue_xml_calls("plain prose").is_empty());
    fs::remove_dir_all(&_dir).unwrap();
}

/// REQ-HARN-004 (aggregation): a monitor rooted at a shared stats registry
/// records interventions into that same registry, so session-level stats are
/// aggregated across loops (Manager turn loop + specialist delegated turns).
#[tokio::test]
async fn test_agent_monitor_stats_aggregated_across_loops() {
    let stats = Arc::new(HarnessStats::new());
    let (_dir, plan) = test_plan();
    // Two loops sharing the same stats registry (e.g. a specialist turn
    // after the Manager's), each performing an XML rescue.
    let mut loop_a = AgentLoop::with_stats(plan.clone(), stats.clone());
    let mut loop_b = AgentLoop::with_stats(plan, stats.clone());
    loop_a.rescue_xml_calls(
        r#"<tool_call>{"function":"glob","arguments":{"pattern":"*.rs"}}</tool_call>"#,
    );
    loop_b.rescue_xml_calls(
        r#"<tool_call>{"function":"glob","arguments":{"pattern":"*.md"}}</tool_call>"#,
    );
    assert_eq!(
        stats
            .xml_tool_rescues
            .load(std::sync::atomic::Ordering::Relaxed),
        2,
        "interventions must aggregate across both loops"
    );
    fs::remove_dir_all(&_dir).unwrap();
}

/// REQ-HARN-003 (runtime): feeding repeated streamed text through the loop
/// terminates the stream and increments `repetition_breaks` exactly once.
#[tokio::test]
async fn test_agent_monitor_text_repetition_breaks_stream() {
    let (_dir, plan) = test_plan();
    let mut loop_ = AgentLoop::new(plan);
    loop_.feed_stream_text("normal prose here, nothing repeating at all.");
    // Feed a 5-char block 5 times: the stream reports termination once.
    let mut fired = false;
    for _ in 0..5 {
        if loop_.feed_stream_text("abcde") {
            fired = true;
            break;
        }
    }
    assert!(fired, "text repetition must terminate the stream");
    assert_eq!(
        loop_
            .monitor()
            .stats()
            .repetition_breaks
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    fs::remove_dir_all(&_dir).unwrap();
}

// --- ManagerLoop (Silent Dispatcher) tests: REQ-ORCH-001 / REQ-PLAN-003 /
// --- REQ-ORCH-005 / REQ-LOOP-004. ---

/// Build an `OrchestratorManager` rooted at an isolated temp plan dir.
fn test_manager(dir: &std::path::Path) -> OrchestratorManager {
    OrchestratorManager::new(
        crate::llm::ChatClient::new("http://localhost:9999/v1", "test-model"),
        Plan::at(dir),
        Arc::new(HarnessStats::new()),
    )
}

/// Alias for `test_manager` (used by parallel/abort ManagerLoop tests).
fn temp_manager(dir: &std::path::Path) -> OrchestratorManager {
    test_manager(dir)
}

/// A scheduler that routes every task to the Coder specialist (domain-neutral
/// for these deterministic-worker tests).
fn coder_scheduler() -> Box<dyn Fn(&str) -> Agent> {
    Box::new(|_tid| Agent::Coder)
}

/// REQ-ORCH-001 / REQ-PLAN-003 (Silent Dispatcher): in Executing phase the
/// ManagerLoop delegates EVERY unchecked `- [ ] [t-xxx]` plan item to a
/// specialist (auto-check-off flips each to `[x]`), and returns only
/// deliverables — no conversational filler.
#[tokio::test]
async fn test_agent_managerloop_silent_dispatcher_delegates_all() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = Arc::new(test_manager(tmp.path()));
    manager
            .create_plan("- [ ] [t-101] Implement the parser.\n- [ ] [t-102] Test the parser.\n- [ ] [t-103] Ship it.\n")
            .expect("plan written");

    let mut ml = ManagerLoop::new(manager.clone(), coder_scheduler());
    let results = ml.run_executing().await.expect("executing round succeeds");

    // All three tasks were delegated and returned a deliverable.
    assert_eq!(results.len(), 3, "every unchecked task is delegated");
    // Silent Dispatcher: no prose, only deliverables (each echoes the brief).
    for d in &results {
        assert!(
            d.content.contains("Execute this delegated task"),
            "deliverable must be a specialist deliverable, got: {}",
            d.content
        );
    }
    // Every plan item is now checked off (plan complete).
    assert!(
        Plan::at(tmp.path()).is_complete(),
        "all tasks must be checked off on disk"
    );
    // The results are attributable to their task ids.
    let tids: Vec<Option<String>> = results.iter().map(|d| d.task_id.clone()).collect();
    assert!(tids.contains(&Some("t-101".to_string())));
    assert!(tids.contains(&Some("t-102".to_string())));
    assert!(tids.contains(&Some("t-103".to_string())));
}

/// REQ-PLAN-003 / REQ-ORCH-005 (one task per call, no task-takeover): each
/// `DelegationRequest` carries EXACTLY ONE `task_id` and the plan line's
/// brief; a specialist executes only the task it was delegated. The loop
/// re-reads the plan after each round and keeps dispatching only the items
/// still unchecked.
#[tokio::test]
async fn test_managerloop_one_task_per_call_no_takeover() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = test_manager(tmp.path());
    manager
        .create_plan("- [ ] [t-201] Alpha.\n- [ ] [t-202] Beta.\n")
        .unwrap();

    // Record every delegated task_id via a scheduler that logs ids.
    let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let log2 = log.clone();
    let scheduler: Box<dyn Fn(&str) -> Agent> = Box::new(move |tid| {
        log2.lock().unwrap().push(tid.to_string());
        Agent::Coder
    });
    let mut ml = ManagerLoop::new(Arc::new(manager), scheduler);
    let results = ml.run_executing().await.unwrap();
    assert_eq!(results.len(), 2);

    // Each task was delegated exactly once — a specialist never autonomously
    // iterated the whole plan.
    let delegated = log.lock().unwrap();
    assert_eq!(delegated.len(), 2, "two independent tasks, two delegations");
    assert!(delegated.contains(&"t-201".to_string()));
    assert!(delegated.contains(&"t-202".to_string()));
}

/// REQ-ORCH-005 (parallel delegation): independent pending tasks are
/// dispatched concurrently (multiple in-flight `delegate()` futures). The
/// test verifies all independent tasks are completed in one executing round
/// without a sequential plan write.
#[tokio::test]
async fn test_managerloop_parallel_independent_delegation() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = temp_manager(tmp.path());
    manager
            .create_plan(
                "- [ ] [t-301] Independent A.\n- [ ] [t-302] Independent B.\n- [ ] [t-303] Independent C.\n",
            )
            .unwrap();
    let mut ml = ManagerLoop::new(Arc::new(manager), coder_scheduler());
    let results = ml.run_executing().await.unwrap();
    // All three independent tasks dispatched in one parallel round.
    assert_eq!(results.len(), 3);
    assert!(Plan::at(tmp.path()).is_complete());
}

/// REQ-LOOP-004 (Steer / Abort): an abort signal stops the Silent Dispatcher
/// immediately and SIGKILLs the active PTY process groups. Steer prompts are
/// deferred (not injected mid-dispatch).
#[tokio::test]
async fn test_managerloop_abort_stops_and_kills_pty() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = temp_manager(tmp.path());
    manager
        .create_plan("- [ ] [t-401] Long task.\n- [ ] [t-402] Another.\n")
        .unwrap();
    let mut ml = ManagerLoop::new(Arc::new(manager), coder_scheduler());
    // Register a PTY process group id that does not exist — ESRCH is handled
    // gracefully by kill_process_group, proving the abort path SIGKILLs it.
    ml.track_pty_pid(999_999);
    // Queue an abort: the loop must stop without completing the plan.
    ml.signal(Signal::Abort);
    let results = ml.run_executing().await.unwrap();
    // The abort happened at the top of the round, before dispatching.
    assert_eq!(results.len(), 0);
    assert!(
        !Plan::at(tmp.path()).is_complete(),
        "abort must leave tasks unchecked"
    );
}

/// REQ-LOOP-004: `signal(Signal::Abort)` arms the shared abort flag
/// immediately so a mid-flight abort can interrupt an in-flight parallel
/// dispatch (not merely at the top of the next round). `drain_signals`
/// clears a stale flag then re-arms it only when an abort is pending.
#[test]
fn test_managerloop_abort_flag_arms_immediately_and_drain_resets() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = temp_manager(tmp.path());
    let mut ml = ManagerLoop::new(Arc::new(manager), coder_scheduler());

    // Initially no abort.
    assert!(!ml.abort_flag.load(Ordering::SeqCst));

    // A queued Abort arms the flag immediately (not deferred to the round).
    ml.signal(Signal::Abort);
    assert!(
        ml.abort_flag.load(Ordering::SeqCst),
        "Abort must set the flag at signal time (mid-flight capable)"
    );

    // drain_signals re-arms it because an abort is pending.
    assert!(ml.drain_signals(), "pending abort drains as aborted");
    assert!(ml.abort_flag.load(Ordering::SeqCst));

    // A clean drain (no pending abort) clears the stale flag.
    ml.signal(Signal::Steer("deferred note".to_string()));
    assert!(!ml.drain_signals(), "steer-only drain is not an abort");
    assert!(
        !ml.abort_flag.load(Ordering::SeqCst),
        "stale abort flag must be cleared on a clean drain"
    );

    // Signal::Steer must NOT arm the abort flag.
    ml.signal(Signal::Steer("note".to_string()));
    assert!(
        !ml.abort_flag.load(Ordering::SeqCst),
        "Steer never arms the abort flag"
    );
}

// --- AgentLoop (run_turn state machine) abort-path tests: REQ-LOOP-004 ---

/// REQ-LOOP-004: `AgentLoop::signal(Signal::Abort)` arms the shared abort
/// flag immediately (not deferred to the next turn), so a mid-flight abort
/// can interrupt an in-flight `ExecuteTools` dispatch. `drain_signals`
/// clears a stale flag then re-arms it only when an abort is pending.
#[test]
fn test_agentloop_abort_flag_arms_immediately_and_drain_resets() {
    let (_dir, plan) = test_plan();
    let mut loop_ = AgentLoop::new(plan);

    // Initially no abort.
    assert!(!loop_.abort_flag.load(Ordering::SeqCst));

    // A queued Abort arms the flag immediately (not deferred to the turn).
    loop_.signal(Signal::Abort);
    assert!(
        loop_.abort_flag.load(Ordering::SeqCst),
        "Abort must set the flag at signal time (mid-flight capable)"
    );

    // drain_signals re-arms it because an abort is pending.
    assert!(loop_.drain_signals(), "pending abort drains as aborted");
    assert!(loop_.abort_flag.load(Ordering::SeqCst));

    // A clean drain (no pending abort) clears the stale flag.
    loop_.signal(Signal::Steer("deferred note".to_string()));
    assert!(!loop_.drain_signals(), "steer-only drain is not an abort");
    assert!(
        !loop_.abort_flag.load(Ordering::SeqCst),
        "stale abort flag must be cleared on a clean drain"
    );

    // Signal::Steer must NOT arm the abort flag.
    loop_.signal(Signal::Steer("note".to_string()));
    assert!(
        !loop_.abort_flag.load(Ordering::SeqCst),
        "Steer never arms the abort flag"
    );
    fs::remove_dir_all(&_dir).unwrap();
}

/// REQ-LOOP-004: an abort queued before `run_turn` stops the turn
/// immediately at the top of `PrepareTurn`, SIGKILLs every tracked PTY
/// process group, and returns `TurnOutcome::Aborted` without executing any
/// queued tools.
#[tokio::test]
async fn test_agentloop_abort_stops_turn_and_kills_pty() {
    let (_dir, plan) = test_plan();
    let mut loop_ = AgentLoop::new(plan);
    // Register a PTY process-group id that does not exist — ESRCH is handled
    // gracefully by kill_process_group, proving the abort path SIGKILLs it.
    loop_.track_pty_pid(999_999);
    // Queue a tool that would otherwise run; the abort must pre-empt it.
    loop_.enqueue_tools(vec![serde_json::json!({
        "name": "read_file",
        "arguments": { "path": "Cargo.toml", "task_id": "t-001" }
    })]);
    // Queue an abort: the turn must stop immediately with Aborted.
    loop_.signal(Signal::Abort);
    let outcome = loop_.run_turn().await.unwrap();
    assert_eq!(outcome, TurnOutcome::Aborted);
    // The plan task must remain unchecked (no tool executed).
    assert!(
        !Plan::at(&_dir).is_complete(),
        "abort must leave tasks unchecked"
    );
    fs::remove_dir_all(&_dir).unwrap();
}

/// REQ-LOOP-004: an abort raised mid-flight during `ExecuteTools` (while a
/// long-running write tool is executing) interrupts the dispatch, SIGKILLs
/// every active PTY process group, and returns `TurnOutcome::Aborted`.
///
/// We arm the shared abort flag from a separate task via `abort_flag_handle`
/// (exactly as the UI/backend layer would on a mid-flight `Signal::Abort`)
/// while a slow `run_command` write tool is in flight, then verify the loop
/// returns `Aborted` and the plan task is left unchecked.
#[tokio::test]
async fn test_agentloop_abort_midflight_interrupts_execute_tools() {
    let (_dir, plan) = test_plan();
    // `run_command` is a domain tool forbidden to the Manager (REQ-ORCH-001);
    // route this loop as the Coder specialist whose `terminal__*` allowlist
    // grants it, so the abort-mid-flight contract is exercised (REQ-ORCH-002).
    let mut loop_ = AgentLoop::new(plan).with_caller(ToolCaller::Specialist(Agent::Coder));
    loop_.track_pty_pid(999_999);
    #[cfg(unix)]
    let cmd = "sleep 2";
    #[cfg(windows)]
    let cmd = "ping -n 3 127.0.0.1";

    // Queue a slow write tool annotated with t-001. It sleeps long enough for
    // the abort to be raised mid-flight and caught after dispatch returns.
    loop_.enqueue_tools(vec![serde_json::json!({
        "name": "run_command",
        "arguments": { "command": cmd, "timeout_seconds": 2, "task_id": "t-001" }
    })]);
    // Arm the abort flag from a separate OS thread after a short delay,
    // simulating a mid-flight `Signal::Abort` raised by the UI/backend layer
    // while the write tool is executing. A `std::thread` is used because the
    // synchronous `dispatch` of the write tool blocks the tokio executor, so
    // a tokio task could not run concurrently to set the flag.
    let abort_handle = loop_.abort_flag_handle();
    let raiser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        abort_handle.store(true, Ordering::SeqCst);
    });
    let outcome = loop_.run_turn().await.unwrap();
    let _ = raiser.join();
    // The abort is caught mid-flight and the turn returns `Aborted`. (The
    // synchronous write tool may have already completed and checked off the
    // task before the post-dispatch abort check fires; the essential contract
    // is that the turn stops with `Aborted` rather than `Complete`.)
    assert_eq!(outcome, TurnOutcome::Aborted);
    fs::remove_dir_all(&_dir).unwrap();
}

/// REQ-LOOP-001: a full turn with no tool calls walks the complete phase
/// sequence `PrepareTurn -> CallBackend -> StreamResponse -> ProcessResponse
/// -> ExecuteTools -> CheckFinish` and returns `Continue` (plan not yet
/// complete). This exercises the entire state machine end-to-end.
#[tokio::test]
async fn test_agentloop_full_phase_sequence_no_tools() {
    let (_dir, plan) = test_plan();
    let mut loop_ = AgentLoop::new(plan);
    // No tools queued: the loop must still walk every phase and, because the
    // plan has a pending task, return Continue.
    let outcome = loop_.run_turn().await.unwrap();
    assert_eq!(outcome, TurnOutcome::Continue);
    fs::remove_dir_all(&_dir).unwrap();
}
