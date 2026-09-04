use super::*;
use std::fs;

/// Create an isolated, unique temp marmel directory.
fn temp_marmel() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "marmel_plan_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

const PLAN_MD: &str = "# Execution Plan\n\
- [ ] [t-001] Read configuration module\n\
- [ ] [t-002] Implement strict replace tool\n\
- [ ] [t-003] Run cargo test\n";

/// REQ-PLAN-001/004: verify phase transitions (Conversational -> Executing when
/// a plan exists) and that `.marmel/forced_phase.txt` overrides the internal
/// calculation on every turn.
#[test]
fn test_agent_phase_gating() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);

    // No plan exists yet -> Conversational.
    assert_eq!(plan.determine_phase(), MissionPhase::Conversational);

    // Writing a plan transitions to Executing.
    plan.create(PLAN_MD).unwrap();
    assert!(plan.exists());
    assert_eq!(plan.determine_phase(), MissionPhase::Executing);

    // A forced override back to Conversational wins over the existing plan.
    fs::write(plan.forced_phase_path(), "Conversational\n").unwrap();
    assert_eq!(plan.determine_phase(), MissionPhase::Conversational);

    // Forcing Executing also wins.
    fs::write(plan.forced_phase_path(), "EXECUTING").unwrap();
    assert_eq!(plan.determine_phase(), MissionPhase::Executing);

    // Removing the override returns to plan-based gating (plan exists -> Executing).
    fs::remove_file(plan.forced_phase_path()).unwrap();
    assert_eq!(plan.determine_phase(), MissionPhase::Executing);

    // Without a plan and without an override -> Conversational.
    fs::remove_file(plan.plan_path()).unwrap();
    assert_eq!(plan.determine_phase(), MissionPhase::Conversational);

    fs::remove_dir_all(&dir).unwrap();
}

/// REQ-PLAN-002: executing `[t-001]` with non-error output updates
/// `.marmel/execution_plan.md`, toggling `- [ ] [t-001]` to `- [x] [t-001]`.
#[test]
fn test_agent_auto_plan_checkoff() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();

    // Initially all three tasks are pending.
    assert_eq!(
        plan.pending_tasks(),
        vec![
            "t-001".to_string(),
            "t-002".to_string(),
            "t-003".to_string()
        ]
    );

    // A successful tool output triggers check-off of t-001.
    let flipped = plan
        .check_off_on_success("t-001", "compiled successfully")
        .unwrap();
    assert!(flipped, "successful tool must check off the task");

    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [x] [t-001]"),
        "toggled on disk:\n{on_disk}"
    );
    assert!(
        on_disk.contains("- [ ] [t-002]"),
        "t-002 untouched:\n{on_disk}"
    );
    assert_eq!(
        plan.pending_tasks(),
        vec!["t-002".to_string(), "t-003".to_string()]
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// REQ-PLAN-002 / SPEC pass criterion: a tool output containing "ERROR"
/// leaves `- [ ] [t-001]` unchecked.
#[test]
fn test_agent_failed_tool_leaves_plan_unchecked() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();

    // "ERROR" -> no check-off.
    let flipped = plan
        .check_off_on_success("t-001", "ERROR: build failed")
        .unwrap();
    assert!(!flipped, "error output must not check off");
    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [ ] [t-001]"),
        "t-001 must remain unchecked:\n{on_disk}"
    );

    // "FAILED" -> no check-off.
    let flipped = plan.check_off_on_success("t-002", "test FAILED").unwrap();
    assert!(!flipped);
    let on_disk = plan.read().unwrap().unwrap();
    assert!(on_disk.contains("- [ ] [t-002]"));

    // "REPLAN REQUIRED" -> no check-off.
    let flipped = plan
        .check_off_on_success("t-003", "REPLAN REQUIRED: new approach")
        .unwrap();
    assert!(!flipped);
    let on_disk = plan.read().unwrap().unwrap();
    assert!(on_disk.contains("- [ ] [t-003]"));

    // No pending task changed at all.
    assert_eq!(
        plan.pending_tasks(),
        vec![
            "t-001".to_string(),
            "t-002".to_string(),
            "t-003".to_string()
        ]
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// REQ-PLAN-003: Executing phase implies silent-dispatcher behavior.
#[test]
fn test_agent_silent_dispatcher_executing() {
    assert!(Plan::is_silent_dispatcher(MissionPhase::Executing));
    assert!(!Plan::is_silent_dispatcher(MissionPhase::Conversational));
}

/// Plan `create` writes the `.marmel` directory when missing.
#[test]
fn test_agent_create_plan_creates_dir() {
    let dir = temp_marmel();
    assert!(!dir.exists());
    let plan = Plan::at(&dir);
    plan.create("# Execution Plan\n- [ ] [t-001] task\n")
        .unwrap();
    assert!(plan.plan_path().exists());
    assert!(dir.is_dir());
    fs::remove_dir_all(&dir).unwrap();
}

/// REQ-ORCH-005: the plan-side `MissionMarker::parse` recognizes the
/// canonical terminal markers, with `REPLAN REQUIRED` matched before
/// `FAILED`, and extracts the parenthesized task id from `MISSION COMPLETE`.
#[test]
fn test_mission_marker_parse() {
    let m = MissionMarker::parse("deliverable ... MISSION COMPLETE (t-007)").unwrap();
    match m {
        MissionMarker::Complete { task_id } => {
            assert_eq!(task_id.as_deref(), Some("t-007"))
        }
        _ => panic!("expected Complete"),
    }

    let m = MissionMarker::parse("could not finish: FAILED because of x").unwrap();
    assert!(matches!(m, MissionMarker::Failed { .. }));
    assert!(!m.is_complete());

    let m = MissionMarker::parse("REPLAN REQUIRED: new approach").unwrap();
    assert!(matches!(m, MissionMarker::Replan { .. }));
    assert!(!m.is_complete());

    // REPLAN REQUIRED must win over a trailing FAILED.
    let m = MissionMarker::parse("REPLAN REQUIRED ... FAILED anyway").unwrap();
    assert!(matches!(m, MissionMarker::Replan { .. }));

    // No marker -> None (never auto-check).
    assert!(MissionMarker::parse("just a status update").is_none());
}

/// REQ-PLAN-002 + REQ-ORCH-005: a deliverable carrying `MISSION COMPLETE
/// (t-xxx)` flips `- [ ] [t-xxx]` to `- [x] [t-xxx]` on disk; `FAILED` /
/// `REPLAN REQUIRED` leave it unchecked.
#[test]
fn test_check_plan_on_marker_complete_flips() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();

    let flipped = plan
        .check_plan_on_marker(Some("t-001"), "done MISSION COMPLETE (t-001)")
        .unwrap();
    assert!(flipped, "MISSION COMPLETE must check off the task");

    let on_disk = plan.read().unwrap().unwrap();
    assert!(on_disk.contains("- [x] [t-001]"), "toggled:\n{on_disk}");
    assert!(
        on_disk.contains("- [ ] [t-002]"),
        "t-002 untouched:\n{on_disk}"
    );

    // Marker-supplied task id (no explicit override) also works.
    let flipped = plan
        .check_plan_on_marker(None, "done MISSION COMPLETE (t-002)")
        .unwrap();
    assert!(flipped);
    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [x] [t-002]"),
        "marker-bound:\n{on_disk}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// REQ-PLAN-002 + REQ-ORCH-005: `FAILED` and `REPLAN REQUIRED` markers must
/// leave the plan line unchecked.
#[test]
fn test_check_plan_on_marker_failed_and_replan_leave_unchecked() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();

    // Every one of these carries a FAILED / REPLAN / non-marker payload.
    for (tid, deliverable) in [
        ("t-001", "FAILED: build broke"),
        ("t-002", "REPLAN REQUIRED: new approach"),
        ("t-003", "just a status update, no marker"),
    ] {
        let flipped = plan.check_plan_on_marker(Some(tid), deliverable).unwrap();
        assert!(!flipped, "marker for {tid} must not check off");
    }

    let on_disk = plan.read().unwrap().unwrap();
    for tid in ["t-001", "t-002", "t-003"] {
        assert!(
            on_disk.contains(&format!("- [ ] [{tid}]")),
            "{tid} must stay unchecked:\n{on_disk}"
        );
    }

    // None of the tasks were checked off.
    assert_eq!(
        plan.pending_tasks(),
        vec![
            "t-001".to_string(),
            "t-002".to_string(),
            "t-003".to_string()
        ]
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// REQ-PLAN-002 + REQ-ORCH-005: a completion whose marker has no task id and
/// no explicit override cannot be bound to a plan line -> no check-off.
#[test]
fn test_check_plan_on_marker_no_task_id_leaves_unchecked() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();

    let flipped = plan
        .check_plan_on_marker(None, "MISSION COMPLETE without an id")
        .unwrap();
    assert!(!flipped, "unbound completion must not check anything");
    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [ ] [t-001]"),
        "t-001 untouched:\n{on_disk}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// Test archiving execution plan to `.marmel/archive/`.
#[test]
fn test_plan_archive_on_disk() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Plan\n- [ ] [t-001] Step 1\n").unwrap();

    assert!(!plan.is_complete());
    assert!(!dir.join("archive").exists());

    let flipped = plan.check_off("t-001").unwrap();
    assert!(flipped);
    assert!(plan.is_complete());
    assert!(plan.exists());

    let archived = plan.archive().unwrap();
    assert!(archived.is_some());

    // Archive directory must exist and contain the archived execution plan.
    let archive_dir = dir.join("archive");
    assert!(archive_dir.is_dir());
    let archived_files: Vec<_> = std::fs::read_dir(&archive_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(!archived_files.is_empty());
    let archive_content = std::fs::read_to_string(&archived_files[0]).unwrap();
    assert!(archive_content.contains("- [x] [t-001]"));

    // Latest archive file also created, and active plan is no longer in-flight.
    assert!(dir.join("execution_plan_archive.md").exists());
    assert!(!plan.exists());

    fs::remove_dir_all(&dir).unwrap();
}

/// t-301 (a): `archive()` returns `Ok(None)` and writes NO files when the
/// plan is incomplete (has at least one unchecked task). The incomplete plan
/// is the working checkpoint and must be left entirely untouched on disk.
#[test]
fn test_archive_noop_when_incomplete() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Plan\n- [ ] [t-001] Step 1\n- [ ] [t-002] Step 2\n")
        .unwrap();

    // Sanity: not complete (has unchecked boxes).
    assert!(!plan.is_complete());

    let result = plan.archive().unwrap();
    assert!(result.is_none(), "incomplete plan must not archive");

    // No archive directory, no latest snapshot, active plan untouched.
    assert!(
        !dir.join("archive").exists(),
        "no archive/ dir may be created for an incomplete plan"
    );
    assert!(
        !dir.join("execution_plan_archive.md").exists(),
        "no latest snapshot may be written for an incomplete plan"
    );
    assert!(
        plan.exists(),
        "active plan must survive an incomplete archive()"
    );
    // On-disk content is byte-for-byte unchanged.
    assert_eq!(
        plan.read().unwrap().unwrap(),
        "# Plan\n- [ ] [t-001] Step 1\n- [ ] [t-002] Step 2\n"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// t-301 (b): `archive()` succeeds (returns `Some(dest)`) and removes the
/// active `.marmel/execution_plan.md` file only when `is_complete()` is true.
#[test]
fn test_archive_removes_active_when_complete() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    // A pre-complete plan (all tasks checked) with no pending checkboxes.
    plan.create("# Plan\n- [x] [t-001] Step 1\n").unwrap();
    assert!(plan.is_complete());

    let result = plan.archive().unwrap();
    assert!(
        result.is_some(),
        "complete plan must archive to a real path"
    );
    let dest = result.unwrap();
    assert!(dest.exists(), "archived snapshot must exist on disk");
    assert!(
        !plan.exists(),
        "active execution plan must be removed once archived"
    );
    assert!(dir.join("archive").is_dir());
    assert!(dir.join("execution_plan_archive.md").exists());

    fs::remove_dir_all(&dir).unwrap();
}

/// t-301 (c): after archiving a complete plan, `read()`, `pending_tasks()`,
/// `all_tasks()`, and `determine_phase()` all reflect the ABSENCE of an
/// active plan — there must be NO stale-archive fallback to
/// `.marmel/execution_plan_archive.md` (an archived plan must never resurrect
/// the `Executing` phase or a pending-task set).
#[test]
fn test_archive_no_stale_read_fallback() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Plan\n- [x] [t-001] Step 1\n- [x] [t-002] Step 2\n")
        .unwrap();
    assert!(plan.is_complete());

    let result = plan.archive().unwrap();
    assert!(result.is_some());
    // Archive artifacts now exist on disk, but the active file is gone.
    assert!(dir.join("archive").is_dir());
    assert!(dir.join("execution_plan_archive.md").exists());
    assert!(!plan.exists());

    // None of these may fall back to the archived snapshot.
    assert!(plan.read().unwrap().is_none(), "read() must return None");
    assert!(
        plan.pending_tasks().is_empty(),
        "no pending tasks after archive"
    );
    assert!(
        plan.all_tasks().is_empty(),
        "no tasks visible after archive"
    );
    assert!(!plan.is_complete(), "no active plan means not complete");
    assert_eq!(
        plan.determine_phase(),
        MissionPhase::Conversational,
        "archived plan must not resurrect the Executing phase"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// t-301 (d): `check_off` auto-archives a completed plan snapshot to
/// `.marmel/archive/` the moment the final pending task is checked off, while
/// keeping the active `.marmel/execution_plan.md` readable until an explicit
/// `archive()`.
#[test]
fn test_checkoff_auto_archives_completed_snapshot() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Plan\n- [ ] [t-001] Step 1\n- [ ] [t-002] Step 2\n")
        .unwrap();
    assert!(!dir.join("archive").exists());

    // First flip does NOT complete the plan -> no snapshot yet.
    let flipped = plan.check_off("t-001").unwrap();
    assert!(flipped);
    assert!(!plan.is_complete());
    assert!(
        !dir.join("archive").exists(),
        "no snapshot before the plan is complete"
    );

    // Final flip completes the plan -> auto snapshot written to archive/.
    let flipped = plan.check_off("t-002").unwrap();
    assert!(flipped);
    assert!(plan.is_complete());

    let archive_dir = dir.join("archive");
    assert!(
        archive_dir.is_dir(),
        "completion must auto-write an archive snapshot"
    );
    let entries: Vec<_> = std::fs::read_dir(&archive_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1, "exactly one auto snapshot expected");
    let snapshot = std::fs::read_to_string(&entries[0]).unwrap();
    assert!(
        snapshot.contains("- [x] [t-001]"),
        "snapshot has t-001 checked"
    );
    assert!(
        snapshot.contains("- [x] [t-002]"),
        "snapshot has t-002 checked"
    );

    // Active file is still present & readable (check_off must not remove it).
    assert!(plan.exists());
    assert_eq!(plan.pending_tasks().len(), 0, "no pending tasks remain");

    fs::remove_dir_all(&dir).unwrap();
}
