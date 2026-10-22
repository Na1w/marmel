use super::*;
use crate::markers::{
    MARKER_COMPLETE, MARKER_FAILED, MARKER_REPLAN, decorated, has_complete_marker,
    has_failure_marker, has_replan_marker,
};
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

/// Plan existence is what the live paths actually consult.
///
/// What is certified here is the disk state itself: `exists()` follows the
/// plan file, and `clear()` also deletes a stale override file left behind by
/// an older release.
#[test]
fn test_plan_exists_tracks_the_plan_file() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);

    assert!(!plan.exists());

    // Writing a plan makes it the live plan.
    plan.create(PLAN_MD).unwrap();
    assert!(plan.exists());
    assert_eq!(
        plan.pending_tasks(),
        vec![
            "t-001".to_string(),
            "t-002".to_string(),
            "t-003".to_string()
        ]
    );

    // A stale plan-override file from an older release is deleted by `clear()`
    // together with the plan.
    fs::write(plan.forced_phase_path(), "Conversational\n").unwrap();
    plan.clear().unwrap();
    assert!(!plan.exists());
    assert!(!plan.forced_phase_path().exists());
    assert!(
        plan.pending_tasks().is_empty(),
        "cleared plan must report no pending work"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// Executing `[t-xxx]` successfully updates `.marmel/execution_plan.md`,
/// toggling `- [ ] [t-xxx]` to `- [x] [t-xxx]`.
///
/// The decision is the **structured** `ToolResult::is_error` flag at the call
/// site in `src/ui/session.rs`, never a re-classification of the output prose:
/// the witness below shows that a prose heuristic really does mis-judge
/// successful output that merely mentions `error`.
#[test]
fn test_agent_auto_plan_checkoff() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();

    assert_eq!(
        plan.pending_tasks(),
        vec![
            "t-001".to_string(),
            "t-002".to_string(),
            "t-003".to_string()
        ]
    );

    // A successful tool output triggers check-off: the tool reported
    // `is_error == false` and the harness holds the plan id explicitly, so the
    // wording of the output is irrelevant.
    let output = "Compiling thiserror v1.0.65 (src/error.rs) … test result: ok";
    assert!(
        legacy_substring_says_failure(output),
        "witness: the deleted substring heuristic called this output a failure"
    );
    let flipped = plan.check_off("t-001").unwrap();
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

/// A failed tool leaves `- [ ] [t-xxx]` unchecked: `src/ui/session.rs` calls
/// `check_off` only when the structured `ToolResult::is_error` flag is false, so
/// a failed result never reaches the plan. An error-shaped deliverable never
/// checks anything off either — `FAILED`, `REPLAN REQUIRED` and a body with no
/// terminal marker all resolve to a non-completion verdict.
///
/// Non-vacuity: the very same plan lines do flip for a decorated completion, so
/// the refusals above are about the verdict, not about a broken check-off path.
#[test]
fn test_failed_deliverables_leave_every_plan_line_unchecked() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();

    // Error-shaped deliverables in every marker shape the runtime produces, plus
    // a marker-less status update.
    let failed: Vec<(&str, String)> = vec![
        ("t-001", format!("{MARKER_FAILED}: build broke")),
        ("t-002", format!("test {MARKER_FAILED}")),
        ("t-003", format!("{MARKER_REPLAN}: new approach")),
        (
            "t-001",
            "still working on it, no terminal marker here".to_string(),
        ),
    ];
    for (tid, output) in &failed {
        let flipped = plan
            .check_plan_on_marker(Some(tid), output.as_str())
            .unwrap();
        assert!(!flipped, "error output must not check off {tid}");
        assert!(
            !plan
                .check_plan_on_deliverable(None, Some(tid), output.as_str())
                .unwrap(),
            "error output must not check off {tid} (structured entry point)"
        );
    }

    let on_disk = plan.read().unwrap().unwrap();
    for tid in ["t-001", "t-002", "t-003"] {
        assert!(
            on_disk.contains(&format!("- [ ] [{tid}]")),
            "{tid} must remain unchecked:\n{on_disk}"
        );
    }

    // No pending task changed at all.
    assert_eq!(
        plan.pending_tasks(),
        vec![
            "t-001".to_string(),
            "t-002".to_string(),
            "t-003".to_string()
        ]
    );
    assert!(
        !plan.is_complete(),
        "a failed round never completes the plan"
    );

    // Discriminating counterpart: with a completion verdict each of those exact
    // plan lines does flip.
    for tid in ["t-001", "t-002", "t-003"] {
        assert!(
            plan.check_plan_on_deliverable(None, Some(tid), &decorated(MARKER_COMPLETE, tid),)
                .unwrap(),
            "a decorated completion must check off {tid}"
        );
    }
    assert_eq!(plan.pending_tasks(), Vec::<String>::new());
    assert!(plan.is_complete(), "the same plan completes on real work");

    fs::remove_dir_all(&dir).unwrap();
}

/// The plan layer exposes exactly two facts to the live dispatcher: which items
/// are still pending, and when there are none left.
#[test]
fn test_pending_and_complete_are_the_dispatchers_input() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);

    // No plan at all: nothing to iterate, nothing complete.
    assert!(plan.pending_tasks().is_empty());
    assert!(!plan.is_complete());

    plan.create(PLAN_MD).unwrap();
    assert!(!plan.is_complete(), "an unchecked box means not complete");

    plan.check_off("t-001").unwrap();
    plan.check_off("t-002").unwrap();
    assert!(!plan.is_complete(), "t-003 is still pending");
    assert_eq!(plan.pending_tasks(), vec!["t-003".to_string()]);

    plan.check_off("t-003").unwrap();
    assert!(plan.pending_tasks().is_empty());
    assert!(plan.is_complete(), "all boxes ticked -> complete");

    fs::remove_dir_all(&dir).unwrap();
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

/// Newline handling has exactly one owner and happens once — `create` runs the
/// plan markdown through `crate::plan_parse::normalize_newlines` and decides the
/// trailing newline. `check_off` is a byte-faithful rewrite: it must not re-spell
/// the document's line endings as a side effect of ticking one box, so a plan
/// that reached disk with CRLF (hand-edited, outside `create`) keeps every CRLF
/// byte.
#[test]
fn test_newlines_normalised_once_and_check_off_is_byte_faithful() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);

    // CRLF input without a trailing newline -> normalized LF, one trailing newline.
    plan.create("# Execution Plan\r\n- [ ] [t-001] Step one\r\n- [ ] [t-002] Step two")
        .expect("create");
    let written = fs::read_to_string(plan.plan_path()).expect("read raw");
    assert!(
        !written.contains('\r'),
        "CRLF must be normalised once, at write time: {written:?}"
    );
    assert!(
        written.ends_with('\n') && !written.ends_with("\n\n"),
        "exactly one trailing newline: {written:?}"
    );
    assert_eq!(
        plan.pending_tasks(),
        vec!["t-001".to_string(), "t-002".to_string()]
    );

    // Ticking a box must not change the shape of anything else.
    assert!(plan.check_off("t-002").unwrap());
    let after = fs::read_to_string(plan.plan_path()).expect("read raw");
    assert_eq!(
        after.matches('\n').count(),
        written.matches('\n').count(),
        "line count must be unchanged:\nbefore={written:?}\nafter={after:?}"
    );
    assert!(
        after.ends_with('\n'),
        "trailing newline preserved: {after:?}"
    );
    assert!(
        after.contains("- [x] [t-002] Step two\n"),
        "flipped line keeps its terminator: {after:?}"
    );

    // A CRLF plan that never went through `create` (edited on disk directly) is
    // rewritten byte-for-byte apart from the one flipped box.
    fs::write(
        plan.plan_path(),
        "# Hand-edited plan\r\n- [ ] [t-003] CRLF task\r\n",
    )
    .expect("hand write");
    assert_eq!(
        plan.all_tasks(),
        vec!["t-003".to_string()],
        "a CRLF plan must still parse"
    );
    assert!(plan.check_off("t-003").unwrap());
    let crlf_after = fs::read_to_string(plan.plan_path()).expect("read raw");
    assert_eq!(
        crlf_after, "# Hand-edited plan\r\n- [x] [t-003] CRLF task\r\n",
        "check_off must preserve CRLF, not re-spell the document"
    );
    assert!(plan.is_complete(), "CRLF plan must still complete");

    fs::remove_dir_all(&dir).unwrap();
}

/// Lines inside a fenced code block are documentation, not tasks.
/// End-to-end through the on-disk authority: no phantom pending task, no
/// phantom check-off, and the example cannot hold the plan open forever.
#[test]
fn test_fenced_plan_examples_are_not_live_tasks() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    let markdown = r#"# Execution Plan

The plan format is:

```text
- [ ] [t-phantom] example line
- [x] [t-phantom-done] completed example
```

- [ ] [t-200] real task
"#;
    plan.create(markdown).expect("create");

    assert_eq!(
        plan.all_tasks(),
        vec!["t-200".to_string()],
        "fenced ids must not be reported as tasks"
    );
    assert_eq!(plan.pending_tasks(), vec!["t-200".to_string()]);

    // The example is not addressable work: check-off refuses it and leaves the
    // fenced bytes untouched.
    assert!(
        !plan.check_off("t-phantom").unwrap(),
        "a fenced example id must not be checkable"
    );
    let disk = plan.read().unwrap().unwrap();
    assert!(
        disk.contains("- [ ] [t-phantom] example line"),
        "fenced example must stay verbatim:\n{disk}"
    );

    // The real task completes the plan: the fenced `- [ ]` cannot hold it open.
    assert!(!plan.is_complete());
    assert!(plan.check_off("t-200").unwrap());
    assert!(plan.pending_tasks().is_empty());
    assert!(
        plan.is_complete(),
        "a fenced checklist example must not keep the plan incomplete"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// Every read of the active plan file goes through the same `PLAN_MUTEX` that
/// already guards `create`/`check_off`/`clear`/`archive`, so a reader can never
/// observe the file mid-rewrite (check_off rewrites the whole document). The
/// reader thread below must queue behind a held guard; and `check_off`/
/// `archive`/`is_complete` must not self-deadlock, i.e. they read through the
/// unlocked internals once they hold the guard.
#[test]
fn test_read_participates_in_the_plan_guard() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).expect("create");

    // Self-deadlock check first: these take the guard and must not re-lock it.
    assert!(plan.check_off("t-001").unwrap());
    assert!(!plan.is_complete());
    assert!(
        plan.archive().unwrap().is_none(),
        "incomplete plan archives nothing"
    );

    let guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let reader = {
        let plan = plan.clone();
        std::thread::spawn(move || plan.read().map(|opt| opt.unwrap_or_default()))
    };
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(
        !reader.is_finished(),
        "Plan::read() bypassed PLAN_MUTEX and raced a concurrent writer"
    );
    drop(guard);
    let content = reader
        .join()
        .expect("reader thread must not panic")
        .expect("read");
    assert!(
        content.contains("- [x] [t-001]"),
        "reader must see the committed state:\n{content}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// The plan layer owns no marker parser: `crate::manager::phase::MissionMarker`
/// *is* `crate::markers::MissionMarker`, and the plan-side entry point recognizes
/// the canonical markers with the single shared precedence table (FAIL-first)
/// and binds the `(t-xxx)` token.
#[test]
fn test_shared_marker_parser_binds_the_decorated_id_and_is_fail_first() {
    // One grammar, three import paths: the plan module re-exports the single
    // owner, so the same deliverable resolves to the same verdict through
    // `crate::agents`, `crate::markers` and this module.
    let verdict = format!("{MARKER_FAILED}: could not finish because of x");
    let from_agents = crate::agents::MissionMarker::parse(&verdict).expect("a verdict");
    let from_markers = crate::markers::MissionMarker::parse(&verdict).expect("a verdict");
    let from_plan = MissionMarker::parse(&verdict).expect("a verdict");
    assert_eq!(
        from_plan, from_markers,
        "the plan module must not resolve markers on its own"
    );
    assert_eq!(
        from_agents, from_markers,
        "the agents re-export must be the same type and grammar"
    );
    assert!(from_plan.is_failure(), "a FAILED verdict is a failure");

    let m = MissionMarker::parse(&format!(
        "deliverable ... {}",
        decorated(MARKER_COMPLETE, "t-007")
    ))
    .unwrap();
    match m {
        MissionMarker::Complete { task_id } => {
            assert_eq!(task_id.as_deref(), Some("t-007"))
        }
        _ => panic!("expected Complete"),
    }

    let m =
        MissionMarker::parse(&format!("could not finish: {MARKER_FAILED} because of x")).unwrap();
    assert!(matches!(m, MissionMarker::Failed { .. }));
    assert!(!m.is_complete());

    let m = MissionMarker::parse(&format!("{MARKER_REPLAN}: new approach")).unwrap();
    assert!(matches!(m, MissionMarker::Replan { .. }));
    assert!(!m.is_complete());

    // FAIL-first: REPLAN REQUIRED outranks a trailing FAILED.
    let m = MissionMarker::parse(&format!("{MARKER_REPLAN} ... {MARKER_FAILED} anyway")).unwrap();
    assert!(matches!(m, MissionMarker::Replan { .. }));

    // An authoritatively placed completion still wins over narrative mentions of
    // an older failure.
    let m = MissionMarker::parse(&format!(
        "Previous attempt failed with error. {}",
        decorated(MARKER_COMPLETE, "t-008")
    ))
    .unwrap();
    assert!(m.is_complete());

    // Benign test counters like "0 failed" must not be parsed as Failed.
    assert!(MissionMarker::parse("test result: ok. 10 passed; 0 failed").is_none());
    let m = MissionMarker::parse(&format!(
        "test result: ok. 10 passed; 0 failed\n\n{}",
        decorated(MARKER_COMPLETE, "t-009")
    ))
    .unwrap();
    assert!(m.is_complete());

    // No marker -> None (never auto-check).
    assert!(MissionMarker::parse("just a status update").is_none());

    // `resolve` honours a structured verdict built through another import path
    // over whatever the body quotes.
    let structured = crate::agents::MissionMarker::Failed {
        reason: "validator rejected".to_string(),
    };
    let resolved = MissionMarker::resolve(Some(&structured), &decorated(MARKER_COMPLETE, "t-042"));
    assert!(
        matches!(resolved, Some(MissionMarker::Failed { .. })),
        "the structured verdict must win, got {resolved:?}"
    );
}

/// A `FAILED` deliverable that only *quotes* the completion marker resolves to
/// Failed at the parse boundary. A substring-first reading — completion marker
/// tested before the failure marker — returns `Complete` here, which is what
/// flipped the plan checkbox.
#[test]
fn test_failed_body_quoting_the_completion_marker_is_not_a_completion() {
    let deliverable = format!(
        "{MARKER_FAILED}: replace tool rejected; I did not emit {} because the build broke",
        decorated(MARKER_COMPLETE, "t-014")
    );
    let m = MissionMarker::parse(&deliverable).expect("a terminal marker");
    assert!(
        matches!(m, MissionMarker::Failed { .. }),
        "expected Failed, got {m:?}"
    );
    assert!(!m.is_complete());

    // (c) REPLAN REQUIRED is never a success either.
    let replan = MissionMarker::parse(&format!(
        "{} from the last round; {MARKER_REPLAN}: split t-015",
        decorated(MARKER_COMPLETE, "t-015")
    ))
    .expect("a terminal marker");
    assert!(!replan.is_complete());
    assert!(matches!(replan, MissionMarker::Replan { .. }));
}

/// A deliverable carrying `MISSION COMPLETE (t-xxx)` flips `- [ ] [t-xxx]` to
/// `- [x] [t-xxx]` on disk; `FAILED` / `REPLAN REQUIRED` leave it unchecked.
#[test]
fn test_check_plan_on_marker_complete_flips() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();

    let flipped = plan
        .check_plan_on_marker(
            Some("t-001"),
            &format!("done {}", decorated(MARKER_COMPLETE, "t-001")),
        )
        .unwrap();
    assert!(flipped, "{MARKER_COMPLETE} must check off the task");

    let on_disk = plan.read().unwrap().unwrap();
    assert!(on_disk.contains("- [x] [t-001]"), "toggled:\n{on_disk}");
    assert!(
        on_disk.contains("- [ ] [t-002]"),
        "t-002 untouched:\n{on_disk}"
    );

    // Marker-supplied task id (no explicit override) also works.
    let flipped = plan
        .check_plan_on_marker(
            None,
            &format!("done {}", decorated(MARKER_COMPLETE, "t-002")),
        )
        .unwrap();
    assert!(flipped);
    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [x] [t-002]"),
        "marker-bound:\n{on_disk}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// A completion whose marker has no task id and no explicit override cannot be
/// bound to a plan line -> no check-off.
#[test]
fn test_check_plan_on_marker_no_task_id_leaves_unchecked() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();

    let flipped = plan
        .check_plan_on_marker(None, &format!("{MARKER_COMPLETE} without an id"))
        .unwrap();
    assert!(!flipped, "unbound completion must not check anything");
    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [ ] [t-001]"),
        "t-001 untouched:\n{on_disk}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// End-to-end at the plan boundary: a FAILED subagent deliverable that quotes the
/// literal text `MISSION COMPLETE` in its prose must NOT check the plan line off.
/// On disk the line stays `- [ ]`, `pending_tasks()` still lists the task, and
/// the plan never looks complete. A substring-first parser fails this witness:
/// its completion substring yields a `Complete` marker and flips the box to
/// `- [x]`.
#[test]
fn test_check_plan_on_marker_failed_body_quoting_mission_complete_keeps_box_unchecked() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Execution Plan\n- [ ] [t-014] Fix the replace tool\n")
        .unwrap();

    let deliverable = format!(
        "{MARKER_FAILED}: replace tool rejected; I did not emit {} because the build broke",
        decorated(MARKER_COMPLETE, "t-014")
    );
    let flipped = plan
        .check_plan_on_marker(Some("t-014"), &deliverable)
        .expect("plan io");
    assert!(!flipped, "a FAILED verdict must never check off");

    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [ ] [t-014]"),
        "plan line must stay unchecked on disk:\n{on_disk}"
    );
    assert!(
        !on_disk.contains("- [x] [t-014]"),
        "plan line must not be checked off:\n{on_disk}"
    );
    assert_eq!(plan.pending_tasks(), vec!["t-014".to_string()]);
    assert!(!plan.is_complete(), "plan must not self-complete");

    // Same body without an explicit override: the `(t-xxx)` token quoted by the
    // failed deliverable must not bind a check-off on its own either.
    let flipped = plan
        .check_plan_on_marker(None, &deliverable)
        .expect("plan io");
    assert!(!flipped);
    assert!(plan.read().unwrap().unwrap().contains("- [ ] [t-014]"));

    fs::remove_dir_all(&dir).unwrap();
}

/// A genuine `MISSION COMPLETE (t-xxx)` deliverable still checks its plan line
/// off on disk.
#[test]
fn test_check_plan_on_marker_genuine_completion_still_checks_off() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Execution Plan\n- [ ] [t-007] Add marker grammar\n")
        .unwrap();

    let deliverable = format!(
        "Implemented src/markers.rs and ran the suite.\n\n{}",
        decorated(MARKER_COMPLETE, "t-007")
    );
    let flipped = plan
        .check_plan_on_marker(Some("t-007"), &deliverable)
        .expect("plan io");
    assert!(flipped, "a genuine completion must check off");

    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [x] [t-007]"),
        "plan line must be checked off:\n{on_disk}"
    );
    assert!(plan.pending_tasks().is_empty());
    assert!(plan.is_complete());

    fs::remove_dir_all(&dir).unwrap();
}

/// End-to-end: `REPLAN REQUIRED` is never a success, even when the same
/// deliverable quotes a completion marker from an earlier round.
#[test]
fn test_check_plan_on_marker_replan_never_checks_off() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Execution Plan\n- [ ] [t-021] Wire the runner\n")
        .unwrap();

    let deliverable = format!(
        "{} was emitted last round but the decomposition is wrong.\n\n{MARKER_REPLAN}: split t-021",
        decorated(MARKER_COMPLETE, "t-021")
    );
    let flipped = plan
        .check_plan_on_marker(Some("t-021"), &deliverable)
        .expect("plan io");
    assert!(!flipped, "{MARKER_REPLAN} must not check off");

    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [ ] [t-021]"),
        "{MARKER_REPLAN} must leave the box unchecked:\n{on_disk}"
    );
    assert_eq!(plan.pending_tasks(), vec!["t-021".to_string()]);

    fs::remove_dir_all(&dir).unwrap();
}

/// The structured marker field wins over body text: a structured `Complete` is
/// not vetoed by the word `FAILED` in the body, and a structured `Failed` is
/// never rescued by a completion sentence.
#[test]
fn test_check_plan_on_deliverable_structured_marker_wins_over_body() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(
        "# Execution Plan\n- [ ] [t-005] Land markers module\n- [ ] [t-006] Audit call sites\n",
    )
    .unwrap();

    // Structured Complete + a body that mentions FAILED (stale narrative).
    let complete = MissionMarker::Complete {
        task_id: Some("t-005".to_string()),
    };
    let flipped = plan
        .check_plan_on_deliverable(
            Some(&complete),
            Some("t-005"),
            &format!("the first attempt {MARKER_FAILED}; this final revision is clean"),
        )
        .expect("plan io");
    assert!(flipped, "the structured marker is authoritative");
    let on_disk = plan.read().unwrap().unwrap();
    assert!(on_disk.contains("- [x] [t-005]"), "toggled:\n{on_disk}");

    // Structured Failed + a body that quotes MISSION COMPLETE.
    let failed = MissionMarker::Failed {
        reason: "validator rejected".to_string(),
    };
    let flipped = plan
        .check_plan_on_deliverable(
            Some(&failed),
            Some("t-006"),
            &decorated(MARKER_COMPLETE, "t-006"),
        )
        .expect("plan io");
    assert!(!flipped, "a structured FAILED must not check off");
    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [ ] [t-006]"),
        "t-006 must stay unchecked:\n{on_disk}"
    );
    assert_eq!(plan.pending_tasks(), vec!["t-006".to_string()]);

    fs::remove_dir_all(&dir).unwrap();
}

/// `archive()` succeeds (returns `Some(dest)`), writes the completed plan to the
/// timestamped snapshot and to `execution_plan_archive.md`, and removes the
/// active `.marmel/execution_plan.md` — but only when `is_complete()` is true.
#[test]
fn test_archive_removes_active_when_complete() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    // A complete plan: every task is checked, so nothing is pending.
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
        dest.parent() == Some(dir.join("archive").as_path()),
        "the snapshot must live under `.marmel/archive/`"
    );
    assert!(
        std::fs::read_to_string(&dest)
            .expect("snapshot readable")
            .contains("- [x] [t-001]"),
        "the snapshot must carry the completed plan, not an empty stub"
    );
    assert!(
        !plan.exists(),
        "active execution plan must be removed once archived"
    );
    assert!(dir.join("archive").is_dir());
    assert!(dir.join("execution_plan_archive.md").exists());
    assert!(
        std::fs::read_to_string(dir.join("execution_plan_archive.md"))
            .expect("latest archive readable")
            .contains("- [x] [t-001]"),
        "the latest-archive copy must carry the same completed plan"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// `archive()` returns `Ok(None)` and writes NO files when the plan is
/// incomplete (has at least one unchecked task). The incomplete plan is the
/// working checkpoint and must be left entirely untouched on disk.
#[test]
fn test_archive_noop_when_incomplete() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Plan\n- [ ] [t-001] Step 1\n- [ ] [t-002] Step 2\n")
        .unwrap();

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

/// After archiving a complete plan, `read()`, `pending_tasks()`, `all_tasks()`
/// and `is_complete()` all reflect the ABSENCE of an active plan — there must be
/// NO stale-archive fallback to `.marmel/execution_plan_archive.md`: an archived
/// plan must never resurrect a pending-task set (see [`Plan`] docs for the
/// archive semantics).
#[test]
fn test_archive_no_stale_read_fallback() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Plan\n- [x] [t-001] Step 1\n- [x] [t-002] Step 2\n")
        .unwrap();
    assert!(plan.is_complete());

    let result = plan.archive().unwrap();
    assert!(result.is_some());
    // Archive artifacts exist on disk; the active plan file does not.
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

    fs::remove_dir_all(&dir).unwrap();
}

/// `check_off` auto-archives a completed plan snapshot to `.marmel/archive/` the
/// moment the final pending task is checked off, while keeping the active
/// `.marmel/execution_plan.md` readable until an explicit `archive()`.
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

#[test]
fn test_check_off_avoids_substring_collision() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    let plan_content = "# Execution Plan\n- [ ] [t-010] Tenth task\n- [ ] [t-01] First task\n";
    plan.create(plan_content).unwrap();

    // Flipping a short id must not flip the longer id that contains it, even
    // though the longer one appears first in the file.
    let flipped = plan.check_off("t-01").unwrap();
    assert!(flipped, "t-01 should flip");

    let disk = plan.read().unwrap().unwrap();
    assert!(
        disk.contains("- [ ] [t-010] Tenth task"),
        "t-010 must remain unchecked:\n{disk}"
    );
    assert!(
        disk.contains("- [x] [t-01] First task"),
        "t-01 must be checked:\n{disk}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_check_off_does_not_flip_task_mentioned_in_other_description() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    let plan_content = "# Execution Plan\n\
- [ ] [t-006] Verify build/compile\n\
- [ ] [t-007] Verify unit test\n\
- [ ] [t-011] Produce report summarizing (t-006...t-008)\n";
    plan.create(plan_content).unwrap();

    assert!(plan.check_off("t-006").unwrap());
    // Checking off an id a second time must NOT check off a different id
    assert!(!plan.check_off("t-006").unwrap());

    let disk = plan.read().unwrap().unwrap();
    assert!(disk.contains("- [x] [t-006] Verify build/compile"));
    assert!(disk.contains("- [ ] [t-007] Verify unit test"));
    assert!(
        disk.contains("- [ ] [t-011] Produce report summarizing (t-006...t-008)"),
        "t-011 must remain unchecked:\n{disk}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

// Regression guards for task-id binding and the deliverable verdict.
//
// Each witness below re-states a legacy rule *inside the test* (the same
// technique `src/markers.rs` uses for its `legacy_substring_first_parse`
// witness) so every guard here is provably non-vacuous: the legacy rule would
// have flipped the box, the live path refuses.

/// Witness of a legacy task-id fallback: a regex scan over an arbitrary blob (the
/// serialized tool arguments, or any deliverable prose).
fn legacy_blob_scan_task_id(blob: &str) -> Option<String> {
    regex::Regex::new(r"\(?\[?(t-[A-Za-z0-9_-]+)\]?\)?")
        .expect("witness regex")
        .captures(blob)
        .map(|c| c[1].to_string())
}

/// Witness of a legacy check-off test: a substring heuristic handed a canned
/// token, so the test was satisfied unconditionally for any such token and never
/// consulted real deliverable text.
fn legacy_canned_token_passes_gate(token: &str) -> bool {
    let upper = token.to_ascii_uppercase();
    !(upper.contains(MARKER_REPLAN) || upper.contains("ERROR") || upper.contains(MARKER_FAILED))
}

/// Witness of a legacy verdict heuristic that read *output prose* instead of the
/// structured tool-result flag: successful output that merely contains `error` /
/// `failed` (`thiserror`, `src/error.rs`, `errors.log`) was reported as a failure
/// and the plan line was never checked off.
fn legacy_substring_says_failure(output: &str) -> bool {
    let upper = output.to_ascii_uppercase();
    if upper.contains(MARKER_REPLAN) {
        return true;
    }
    let sanitized = upper
        .replace("0 FAILED", "")
        .replace("0 TESTS FAILED", "")
        .replace("0 TEST FAILED", "")
        .replace("NO ERROR", "")
        .replace("NO ERRORS", "")
        .replace("WITHOUT ERROR", "")
        .replace("0 ERRORS", "");
    sanitized.contains("ERROR") || sanitized.contains(MARKER_FAILED)
}

/// Witness of a legacy `Plan::check_off` line grammar:
/// `^\s*(?:[-*]|\d+\.)\s*\[\s*\]\s*\*{0,2}\[?<tid>\]?\*{0,2}\b`
/// — it only ever accepted a *square-bracket* id, so a parenthesised plan line
/// `- [ ] (t-xxx) migrate schema` could never be checked off.
fn legacy_check_off_line(line: &str, task_id: &str) -> Option<String> {
    let pat = format!(r"^\s*(?:[-*]|\d+\.)\s*\[\s*\]\s*\*{{0,2}}\[?{task_id}\]?\*{{0,2}}\b");
    if regex::Regex::new(&pat)
        .expect("witness regex")
        .is_match(line)
    {
        Some(line.replace("[ ]", "[x]"))
    } else {
        None
    }
}

/// Guard (i): a deliverable that merely *mentions* a path whose name contains a
/// `t-…` substring must never check off the task whose id that substring spells.
/// `src/chart-notes.md` → `t-notes` is the fixture used below: it looks like a
/// task id and is not one.
#[test]
fn test_path_substring_never_becomes_a_checked_off_task_id() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Execution Plan\n- [ ] [t-notes] Chart notes doc\n- [ ] [t-001] Real task\n")
        .unwrap();

    // Witness: the legacy arguments-JSON scan really did produce `t-notes`
    // from this payload, and would then have checked the line off.
    let args_json = r##"{"path":"src/chart-notes.md","content":"# notes"}"##;
    assert_eq!(
        legacy_blob_scan_task_id(args_json).as_deref(),
        Some("t-notes"),
        "witness must reproduce the H6 mis-derivation"
    );

    // (a) Live path, no terminal marker at all: the deliverable is never
    // scanned for id tokens, so nothing is bound and nothing flips.
    let no_marker = "Rewrote the renderer; design notes live in src/chart-notes.md.";
    assert!(
        !plan.check_plan_on_marker(None, no_marker).unwrap(),
        "marker-less deliverable must not check anything off"
    );
    assert!(
        !plan
            .check_plan_on_deliverable(None, None, no_marker)
            .unwrap(),
        "marker-less deliverable must not check anything off"
    );

    // (b) Live path with a genuine completion whose *only* `t-notes` spelling
    // is a substring inside a file path: the id is not a boundary-safe token,
    // so the check-off is refused.
    let bogus = format!("Notes consolidated.\n\n{MARKER_COMPLETE} — see src/chart-notes.md");
    assert_eq!(
        legacy_blob_scan_task_id(&bogus).as_deref(),
        Some("t-notes"),
        "witness: the deleted prose scan binds the path substring"
    );
    assert!(
        !plan.check_plan_on_marker(None, &bogus).unwrap(),
        "an id that only exists inside a path may not bind a check-off"
    );

    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [ ] [t-notes]"),
        "t-notes must stay unchecked:\n{on_disk}"
    );
    assert!(
        on_disk.contains("- [ ] [t-001]"),
        "t-001 must stay unchecked:\n{on_disk}"
    );
    assert_eq!(
        plan.pending_tasks(),
        vec!["t-notes".to_string(), "t-001".to_string()]
    );
    assert!(!plan.is_complete(), "plan must not self-complete");

    // The same plan line DOES flip when the id comes from the marker
    // decoration — the guard rejects the wrong id, not the right one.
    assert!(
        plan.check_plan_on_deliverable(
            None,
            None,
            &format!(
                "Notes consolidated.\n\n{}",
                decorated(MARKER_COMPLETE, "t-notes")
            )
        )
        .unwrap(),
        "a decorated marker id must check off"
    );
    assert!(
        plan.read().unwrap().unwrap().contains("- [x] [t-notes]"),
        "t-notes must flip for a decorated marker"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// Guard (ii): a real `MISSION COMPLETE (t-xxx)` deliverable checks off exactly
/// that task — never an id merely named in the body prose, which is what a
/// first-hit regex scan would have bound.
#[test]
fn test_real_marker_checks_off_exactly_the_decorated_task() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(
        "# Execution Plan\n- [ ] [t-006] Retire old matcher\n\
         - [ ] [t-007] Land marker grammar\n\
         - [ ] [t-008] Document call sites\n",
    )
    .unwrap();

    let deliverable = format!(
        "Supersedes t-006; the old copy is deleted. Implemented src/markers.rs and ran \
         cargo test --lib markers.\n\n{}",
        decorated(MARKER_COMPLETE, "t-007")
    );

    // Witness: a first-hit scan of the same text binds the *prose* id.
    assert_eq!(
        legacy_blob_scan_task_id(&deliverable).as_deref(),
        Some("t-006"),
        "witness must show the deleted scan bound the wrong task"
    );

    let flipped = plan
        .check_plan_on_deliverable(None, None, &deliverable)
        .expect("plan io");
    assert!(flipped, "a real completion marker must check off its task");

    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [x] [t-007]"),
        "t-007 must be checked off:\n{on_disk}"
    );
    assert!(
        on_disk.contains("- [ ] [t-006]"),
        "t-006 (named in prose only) must stay unchecked:\n{on_disk}"
    );
    assert!(
        on_disk.contains("- [ ] [t-008]"),
        "t-008 must stay untouched:\n{on_disk}"
    );
    assert_eq!(
        plan.pending_tasks(),
        vec!["t-006".to_string(), "t-008".to_string()]
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// Guard (iii): a canned success token handed to the check-off path is not
/// evidence. A text-based substring test accepted the literal `"ok"`
/// unconditionally, while the live path requires the real deliverable to carry a
/// terminal marker.
#[test]
fn test_canned_success_token_is_not_accepted_as_evidence() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Execution Plan\n- [ ] [t-009] Harden the gate\n")
        .unwrap();

    // Witness: every canned token cleared the legacy substring test, which then
    // checked the bound task off.
    for canned in ["ok", "OK", "success", "done", "completed successfully"] {
        assert!(
            legacy_canned_token_passes_gate(canned),
            "witness: the deleted gate accepted {canned:?}"
        );
    }

    // Live path: none of those tokens is a verdict, with or without a bound id.
    for canned in [
        "ok",
        "OK",
        "success",
        "done",
        "completed successfully",
        "no errors, all tests passed",
    ] {
        assert!(
            !plan.check_plan_on_marker(Some("t-009"), canned).unwrap(),
            "canned token {canned:?} must not check off"
        );
        assert!(
            !plan
                .check_plan_on_deliverable(None, Some("t-009"), canned)
                .unwrap(),
            "canned token {canned:?} must not check off (structured entry point)"
        );
    }

    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [ ] [t-009]"),
        "t-009 must stay unchecked:\n{on_disk}"
    );
    assert_eq!(plan.pending_tasks(), vec!["t-009".to_string()]);

    // The very same line flips as soon as the *real* deliverable text carries
    // the terminal marker — proof the refusal above is about the canned token.
    let real = format!(
        "Added the token guard in Plan::check_plan_on_deliverable.\n\n{}",
        decorated(MARKER_COMPLETE, "t-009")
    );
    assert!(
        plan.check_plan_on_deliverable(None, Some("t-009"), &real)
            .unwrap(),
        "a real deliverable must check off"
    );
    assert!(
        plan.read().unwrap().unwrap().contains("- [x] [t-009]"),
        "t-009 must flip on a real deliverable"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// Guard (iv): the live check-off path uses the single plan-line grammar owner, so
/// a parenthesised task id (`- [ ] (t-xxx) …`) is listed as pending and checked
/// off, preserving its on-disk spelling.
#[test]
fn test_parenthesised_task_id_flips_through_the_live_path() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Execution Plan\n- [ ] (t-002) migrate schema\n- [ ] [t-003] backfill rows\n")
        .unwrap();

    // The shared grammar sees the parenthesised line as pending.
    assert_eq!(
        plan.pending_tasks(),
        vec!["t-002".to_string(), "t-003".to_string()],
        "parenthesised id must be listed as pending"
    );

    // Witness: a legacy per-call grammar could not rewrite that line at all.
    assert!(
        legacy_check_off_line("- [ ] (t-002) migrate schema", "t-002").is_none(),
        "witness: the pre-C3 grammar could not check off a parenthesised id"
    );

    // (a) id taken from the marker decoration.
    let flipped = plan
        .check_plan_on_marker(
            None,
            &format!(
                "Schema migrated.\n\n{}",
                decorated(MARKER_COMPLETE, "t-002")
            ),
        )
        .expect("plan io");
    assert!(flipped, "parenthesised plan id must be checkable");

    // (b) id taken from the delegated `task_id` argument, still decorated.
    let flipped = plan
        .check_plan_on_deliverable(
            None,
            Some("[t-003]"),
            &format!(
                "Rows backfilled.\n\n{}",
                decorated(MARKER_COMPLETE, "t-003")
            ),
        )
        .expect("plan io");
    assert!(
        flipped,
        "a decorated delegated task_id must normalize and flip"
    );

    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [x] (t-002)"),
        "parenthesised spelling must be preserved and ticked:\n{on_disk}"
    );
    assert!(
        on_disk.contains("- [x] [t-003]"),
        "t-003 must be ticked:\n{on_disk}"
    );
    assert!(plan.pending_tasks().is_empty(), "no pending tasks remain");
    assert!(plan.is_complete(), "the plan must reach completion");

    fs::remove_dir_all(&dir).unwrap();
}

// An unreadable execution plan must never look like "nothing pending" or
// "plan complete": an arm that swallows the `Err` from `fs::read_to_string` and
// returns an empty list is read by the executing dispatcher
// (`src/orchestrator/mod.rs::run_executing`) and by the UI auto-nudge
// (`src/ui/session.rs`) as "the plan is finished".

/// `try_pending_tasks` / `try_all_tasks` distinguish "no plan file" from
/// "plan file cannot be read", while `is_complete` can never report completion
/// for an unreadable plan.
#[test]
fn test_m8_unreadable_plan_is_not_reported_as_no_pending_tasks() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);

    // No plan file at all: genuinely nothing pending, and that must be `Ok`.
    assert!(!plan.exists());
    assert_eq!(
        plan.try_pending_tasks()
            .expect("no plan file is not an error"),
        Vec::<String>::new(),
        "a missing plan file is a legitimate empty pending set"
    );
    assert_eq!(
        plan.try_all_tasks().expect("no plan file is not an error"),
        Vec::<String>::new()
    );
    assert!(!plan.is_complete(), "no plan file means not complete");

    // A plan file that exists but cannot be decoded must surface an error.
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        plan.plan_path(),
        [
            0xff_u8, 0xfe, 0x00, b'-', b'[', b']', b't', b'-', b'0', b'0', b'1',
        ],
    )
    .expect("write undecodable plan");
    assert!(plan.exists(), "the plan file is on disk");

    let err = plan
        .try_pending_tasks()
        .expect_err("M8: an undecodable plan must be an error, not an empty pending set");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("could not be read") && msg.contains("execution_plan.md"),
        "the error must name the plan and the failure, got: {msg}"
    );
    assert!(
        plan.try_all_tasks().is_err(),
        "all_tasks must propagate the same failure"
    );

    // The checks the dispatcher actually consults must stay on the safe side.
    assert!(
        !plan.is_complete(),
        "an unreadable plan is NEVER complete (otherwise the dispatcher stops)"
    );

    // The Vec-shaped compatibility wrappers still degrade to empty for callers
    // that cannot handle a `Result` yet — but that empty list is logged as an
    // error and is never paired with `is_complete() == true`.
    assert!(plan.pending_tasks().is_empty());
    assert!(plan.all_tasks().is_empty());
    assert!(!plan.is_complete());

    fs::remove_dir_all(&dir).ok();
}

/// The two error-propagating readers answer different questions, and the
/// compatibility wrappers must keep answering them faithfully for a readable plan
/// (a wrapper that swallows the plan and returns an empty list fails here even
/// though the plan is on disk and readable).
#[test]
fn test_try_all_tasks_lists_checked_ids_while_try_pending_tasks_omits_them() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(PLAN_MD).unwrap();
    let all = || {
        vec![
            "t-001".to_string(),
            "t-002".to_string(),
            "t-003".to_string(),
        ]
    };

    // Nothing done yet: "all" and "pending" coincide.
    assert_eq!(plan.try_pending_tasks().unwrap(), all());
    assert_eq!(plan.try_all_tasks().unwrap(), all());

    plan.check_off("t-001").unwrap();
    // They must diverge: a ticked id leaves the pending set but stays listed.
    assert_eq!(
        plan.try_pending_tasks().unwrap(),
        vec!["t-002".to_string(), "t-003".to_string()],
        "a checked-off task must disappear from the pending set"
    );
    assert_eq!(
        plan.try_all_tasks().unwrap(),
        all(),
        "a checked-off task must still be listed by try_all_tasks"
    );
    assert!(!plan.is_complete());

    // The Vec-shaped wrappers carry the same state (the log-and-empty path is
    // pinned by the unreadable-plan test above, not by these happy-path values).
    assert_eq!(
        plan.pending_tasks(),
        vec!["t-002".to_string(), "t-003".to_string()]
    );
    assert_eq!(plan.all_tasks(), all());

    plan.check_off("t-002").unwrap();
    plan.check_off("t-003").unwrap();
    assert_eq!(
        plan.try_pending_tasks().unwrap(),
        Vec::<String>::new(),
        "a fully ticked plan has nothing pending"
    );
    assert_eq!(
        plan.try_all_tasks().unwrap(),
        all(),
        "the completed ids must still be recoverable from the plan"
    );
    assert!(plan.is_complete(), "a fully checked plan is complete");

    fs::remove_dir_all(&dir).unwrap();
}

// The success/failure decision behind check-off has exactly one owner:
// `crate::markers` (`MissionMarker::resolve`, `has_failure_marker`,
// `has_replan_marker`). `src/manager/phase.rs` declares no matcher of its own.

/// Every check-off decision is taken by the single marker matcher, including the
/// payloads that fool a substring matcher pair: a benign `0 failed` test counter,
/// prose that *quotes* `MISSION COMPLETE` inside a failure, a negated mention
/// ("not … MISSION COMPLETE"), a canned success token and a replan that embeds
/// the completion words.
#[test]
fn test_m9_checkoff_is_decided_by_the_single_marker_matcher() {
    struct Case {
        label: &'static str,
        body: String,
        check_off: bool,
    }
    let cases = [
        Case {
            label: "benign `0 failed` counter + real completion marker",
            body: format!(
                "Compiling thiserror v1.0.65 (src/error.rs)\nerror: 0 warnings, 0 errors\n\
                 test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured\n\n\
                 {}",
                decorated(MARKER_COMPLETE, "t-001")
            ),
            check_off: true,
        },
        Case {
            label: "failure verdict that quotes the success token in prose",
            body: format!(
                "{MARKER_FAILED}: the validator rejected the deliverable\nIt contained the \
                 words {MARKER_COMPLETE}, but two tests still fail."
            ),
            check_off: false,
        },
        Case {
            label: "negated mention only (`not ok`, no verdict emitted)",
            body: format!("not ok — I did not emit {MARKER_COMPLETE} because the build is broken."),
            check_off: false,
        },
        Case {
            label: "canned success token (the pre-M9 gate input `loop.rs:524` fed in)",
            body: "ok".to_string(),
            check_off: false,
        },
        Case {
            label: "replan that embeds the completion words",
            body: format!(
                "The plan needs revisiting: {MARKER_COMPLETE} is unreachable while\n\
                 {MARKER_REPLAN}: the crate has no test harness."
            ),
            check_off: false,
        },
    ];

    for case in &cases {
        // (1) The matcher predicates and the parser must agree with each other —
        // they are the same owner, so a call site can never re-decide.
        let resolved = MissionMarker::resolve(None, &case.body);
        let marker_says_complete = resolved.as_ref().is_some_and(MissionMarker::is_complete);
        assert_eq!(
            marker_says_complete, case.check_off,
            "marker layer disagrees for case [{}]: {resolved:?}",
            case.label
        );
        if !case.check_off {
            assert!(
                has_replan_marker(&case.body)
                    || has_failure_marker(&case.body)
                    || resolved.is_none(),
                "a deliverable that must not check off has neither a benign-counter pass nor a \
                 completion marker: [{}]",
                case.label
            );
        }

        // (2) … and the plan boundary must reach the same verdict.
        let dir = temp_marmel();
        let plan = Plan::at(&dir);
        plan.create(PLAN_MD).unwrap();
        let flipped = plan
            .check_plan_on_deliverable(None, Some("t-001"), &case.body)
            .expect("check_plan_on_deliverable must not fail on IO");
        assert_eq!(
            flipped, case.check_off,
            "plan boundary disagrees for [{}]",
            case.label
        );
        let on_disk = plan.read().unwrap().unwrap();
        assert_eq!(
            on_disk.contains("- [x] [t-001]"),
            case.check_off,
            "disk state disagrees for [{}]:\n{on_disk}",
            case.label
        );
        fs::remove_dir_all(&dir).ok();
    }

    // (3) Non-vacuity: a substring pair would have decided the benign counter
    // case the other way round, and accepted the canned token.
    let benign = cases[0].body.as_str();
    assert!(
        legacy_substring_says_failure(benign),
        "witness: the deleted substring heuristic called the benign-counter success a failure"
    );
    assert!(
        legacy_canned_token_passes_gate("ok"),
        "witness: the deleted gate accepted the canned success token"
    );
    assert!(
        has_complete_marker(&cases[1].body) && has_failure_marker(&cases[1].body),
        "witness: a plain substring scan sees both markers in the failure prose; only the \
         single-owner precedence rule resolves it as a failure"
    );
}

// Check-off integrity: one marker decision, one task id, and a plan that
// cannot be read or parsed never reports itself as finished.

/// A completion marker that names a **different** plan task than the id the
/// caller bound aborts the check-off instead of silently preferring either side:
/// the live session loops bind the `delegate_task` task id, so a completion for
/// one task can never tick another task's box.
#[test]
fn test_bound_id_and_marker_id_disagreement_aborts_check_off() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(
        "# Execution Plan\n- [ ] [t-010] Land the gate\n- [ ] [t-011] Audit the call sites\n",
    )
    .unwrap();

    // (1) Body-derived marker id vs the caller-bound id.
    let body = format!(
        "Audited both session loops.\n\n{}",
        decorated(MARKER_COMPLETE, "t-011")
    );
    assert!(
        !plan
            .check_plan_on_deliverable(None, Some("t-010"), &body)
            .expect("no IO error"),
        "a marker for another task must not check off the bound task"
    );

    // (2) The same rule for the structured marker threaded by the orchestrator.
    let structured = MissionMarker::Complete {
        task_id: Some("t-011".to_string()),
    };
    assert!(
        !plan
            .check_plan_on_deliverable(Some(&structured), Some("t-010"), "notes, no marker text")
            .expect("no IO error"),
        "a structured marker for another task must not check off the bound task"
    );

    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [ ] [t-010]"),
        "t-010 must stay unchecked:\n{on_disk}"
    );
    assert!(
        on_disk.contains("- [ ] [t-011]"),
        "the aborted check-off leaves t-011 pending as well (fail-closed):\n{on_disk}"
    );
    assert_eq!(
        plan.pending_tasks(),
        vec!["t-010".to_string(), "t-011".to_string()]
    );
    assert!(!plan.is_complete(), "the plan must not self-complete");

    // Non-vacuity: the identical call succeeds once the ids agree, so the
    // refusal above is about the disagreement and not about the body text.
    assert!(
        plan.check_plan_on_deliverable(None, Some("t-011"), &body)
            .expect("no IO error"),
        "an agreeing bound id must check off"
    );
    let on_disk = plan.read().unwrap().unwrap();
    assert!(
        on_disk.contains("- [x] [t-011]"),
        "t-011 must flip when the ids agree:\n{on_disk}"
    );
    assert!(
        on_disk.contains("- [ ] [t-010]"),
        "t-010 stays pending for its own delegation:\n{on_disk}"
    );

    fs::remove_dir_all(&dir).ok();
}

/// A plan file that exists but holds no parseable task line is a plan whose state
/// is **unknown**, never "all tasks done": `is_complete()` stays false and
/// nothing can be ticked. The session surfaces this (`read_plan_gate` in
/// `src/ui/session.rs`) instead of folding it into an empty pending list.
#[test]
fn test_unparseable_plan_is_never_reported_as_complete() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        plan.plan_path(),
        "# Notes\n\nprose only, no checkbox task lines\n",
    )
    .unwrap();

    assert!(plan.exists(), "the plan file exists on disk");
    assert!(
        plan.try_all_tasks().expect("readable file").is_empty(),
        "the grammar finds no task id at all"
    );
    assert!(
        plan.try_pending_tasks().expect("readable file").is_empty(),
        "so the pending list is empty too — which is exactly why an empty list \
         must not be read as completion"
    );
    assert!(
        !plan.is_complete(),
        "an unparseable plan is NOT complete (fail-closed)"
    );

    // No marker, no bound id, no grammar hit: there is no box to tick.
    assert!(
        !plan.check_off("t-001").expect("no IO error"),
        "check_off must not invent a plan line"
    );
    assert!(
        !plan
            .check_plan_on_deliverable(None, Some("t-001"), &decorated(MARKER_COMPLETE, "t-001"))
            .expect("no IO error"),
        "the marker gate cannot conjure a plan line either"
    );

    fs::remove_dir_all(&dir).ok();
}

/// An unreadable plan (here: invalid UTF-8, so the parse cannot even start) is an
/// **error** through the fallible API and an incomplete plan through the
/// completion check — the two facts the UI turns into a visible warning instead
/// of silence.
#[test]
fn test_unreadable_plan_reports_an_error_and_is_not_complete() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    fs::create_dir_all(&dir).unwrap();
    // `- [ ] ` followed by two bytes that are not valid UTF-8.
    fs::write(
        plan.plan_path(),
        [
            0x2d, 0x20, 0x5b, 0x20, 0x5d, 0x20, 0x74, 0x2d, 0x30, 0x30, 0x31, 0xff, 0xfe,
        ],
    )
    .unwrap();

    let err = plan
        .try_pending_tasks()
        .expect_err("an unreadable plan must be an Err, never an empty Vec");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("UNKNOWN"),
        "the error must name the state as unknown: {msg}"
    );
    assert!(
        msg.contains(&plan.plan_path().display().to_string()),
        "the error must name the plan path the user has to fix: {msg}"
    );

    // The compatibility wrapper still degrades to an empty list — the witness
    // that a caller which ignores the error sees "nothing pending".
    assert!(plan.pending_tasks().is_empty());
    assert!(
        !plan.is_complete(),
        "an unreadable plan must never be complete (M8)"
    );

    // Check-off surfaces the IO failure rather than pretending success.
    let res =
        plan.check_plan_on_deliverable(None, Some("t-001"), &decorated(MARKER_COMPLETE, "t-001"));
    assert!(
        res.is_err(),
        "a plan that cannot be read must report an error, got {res:?}"
    );
    assert!(
        !plan.is_complete(),
        "and it must still not count as complete afterwards"
    );

    fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// t-021 archive regression probes (defects D1-D8 report,
// `docs/investigation/archive_plan_mechanism.md`).
//
// These reproduce the operator-reported "archive plan does not work" symptom
// against the real `Plan` code paths in isolated temp dirs. Each probe names the
// defect it falsifies. They are kept as the seed of the regression suite.
// ---------------------------------------------------------------------------

/// The archived-snapshot file names currently living under `dir/archive/`.
fn snapshot_names(dir: &std::path::Path) -> Vec<String> {
    let archive = dir.join("archive");
    if !archive.is_dir() {
        return Vec::new();
    }
    let mut names: Vec<String> = fs::read_dir(&archive)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// D1 — the completion gate must judge **task-checkbox syntax**, not prose.
///
/// A real plan spells function names (`archive_plan()`, `Plan::archive()`) and
/// bracket placeholders in its Context and task text. Every task here is ticked,
/// so `pending_tasks()` is empty and the plan must be archivable; the
/// whole-document `\[\s*\]|\(\s*\)` scan used to see that prose as pending boxes
/// and refuse forever (witness W8/W9 of the report).
#[test]
fn d1_archive_succeeds_when_prose_holds_parens_and_brackets() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(
        "# Plan\n\
Context: `archive_plan()` calls `Plan::archive()` (see notes) and keeps [] placeholders.\n\
- [x] [t-001] Fix the gate () so prose is ignored\n\
- [x] [t-002] Prove [] prose never holds the plan open\n",
    )
    .unwrap();

    assert!(
        plan.pending_tasks().is_empty(),
        "every task line is ticked, so nothing is pending"
    );
    assert!(
        plan.is_complete(),
        "D1: prose `()` / `[]` must not make a fully ticked plan incomplete"
    );

    let dest = plan
        .archive()
        .unwrap()
        .expect("D1: a completed plan must archive, not return Ok(None)");
    assert!(dest.exists(), "the snapshot must exist");
    assert!(
        !plan.exists(),
        "D1: the active plan must be removed by a successful archive"
    );

    fs::remove_dir_all(&dir).ok();
}

/// D2 — the documented end state must be reachable through the completion flow.
///
/// `create_plan` -> `check_off` (which auto-snapshots but deliberately keeps the
/// active plan) -> explicit `archive()`: afterwards the active plan is gone and
/// exactly one snapshot exists. The recorded session log shows this never
/// happened once: the gate refused every attempt and the completed plan was
/// later overwritten by the next `create_plan`.
#[test]
fn d2_completion_flow_reaches_the_documented_archived_end_state() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create(
        "# Plan\n\
- [ ] [t-001] Inspect archive_plan() and Plan::archive()\n\
- [ ] [t-002] Fix the gate, then archive the plan []\n",
    )
    .unwrap();

    assert!(plan.check_off("t-001").unwrap());
    assert!(plan.check_off("t-002").unwrap());
    assert!(
        plan.is_complete(),
        "D2: after the final check-off the plan is complete"
    );

    let dest = plan
        .archive()
        .unwrap()
        .expect("D2: the explicit archive of a completed plan must succeed");
    assert!(dest.exists());
    assert!(
        !plan.exists(),
        "D2: documented end state — active plan file removed"
    );
    assert_eq!(
        snapshot_names(&dir).len(),
        1,
        "D2: exactly one snapshot for one completed plan"
    );
    assert!(
        dir.join("execution_plan_archive.md").is_file(),
        "D2: the latest-archive convenience copy must exist"
    );

    fs::remove_dir_all(&dir).ok();
}

/// D2 — a completed plan must never be silently clobbered by the next
/// `create_plan`.
///
/// `create` writes unconditionally, so a plan that reached completion without a
/// successful `archive()` used to vanish without any archive artifact (log line
/// `create_plan` at 01:48:25 in the report's forensics).
#[test]
fn d2_create_preserves_a_completed_plan_it_overwrites() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    // Created fully ticked: no `check_off` ever fires, so no auto snapshot.
    plan.create("# Plan A\n- [x] [t-001] Finished work\n")
        .unwrap();
    assert!(plan.is_complete());

    plan.create("# Plan B\n- [ ] [t-002] Fresh mission\n")
        .unwrap();

    let names = snapshot_names(&dir);
    assert_eq!(
        names.len(),
        1,
        "D2: the completed plan A must survive as exactly one snapshot, got {names:?}"
    );
    let snapshot = fs::read_to_string(dir.join("archive").join(&names[0])).unwrap();
    assert!(
        snapshot.contains("[t-001]"),
        "D2: the preserved snapshot must be plan A:\n{snapshot}"
    );
    // The new mission is untouched: still the live, pending plan.
    assert_eq!(plan.pending_tasks(), vec!["t-002".to_string()]);

    fs::remove_dir_all(&dir).ok();
}

/// Guard on the D2 fix: `create` must still replace an **undecodable** active
/// plan. Preservation before an overwrite is best-effort — there is nothing
/// snapshot-worthy in bytes that cannot be decoded, and a corrupt checkpoint must
/// never block the documented recovery path (`create_plan` replacing it).
#[test]
fn d2_create_still_replaces_an_undecodable_plan() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    fs::create_dir_all(&dir).unwrap();
    // `- [ ]` followed by 0xff 0xfe: a plan file that cannot be decoded.
    fs::write(plan.plan_path(), [0x2d, 0x20, 0x5b, 0x20, 0x5d, 0xff, 0xfe]).unwrap();

    plan.create("# Plan B\n- [ ] [t-002] Recover the mission\n")
        .expect("create_plan must still replace an undecodable plan");

    assert_eq!(plan.pending_tasks(), vec!["t-002".to_string()]);
    assert!(
        snapshot_names(&dir).is_empty(),
        "nothing preservable was snapshotted"
    );

    fs::remove_dir_all(&dir).ok();
}

/// D3 — a snapshot name that is already taken by **different** bytes must never
/// be silently truncated. The writer has to pick a fresh, unique name.
#[test]
fn d3_snapshot_name_collision_never_overwrites_a_different_snapshot() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Plan\n- [x] [t-001] Done\n").unwrap();

    let archive = dir.join("archive");
    fs::create_dir_all(&archive).unwrap();
    // Reserve the names the second-resolution clock will produce in this test
    // (and one second later, to stay deterministic across a second boundary).
    let now = chrono::Utc::now();
    let ts0 = now.format("%Y%m%d_%H%M%S").to_string();
    let ts1 = (now + chrono::Duration::seconds(1))
        .format("%Y%m%d_%H%M%S")
        .to_string();
    let kept_a = archive.join(format!("execution_plan_{ts0}.md"));
    let kept_b = archive.join(format!("execution_plan_{ts1}.md"));
    fs::write(&kept_a, "EARLIER SNAPSHOT A").unwrap();
    fs::write(&kept_b, "EARLIER SNAPSHOT B").unwrap();

    let dest = plan
        .archive()
        .unwrap()
        .expect("a completed plan must archive");

    assert_eq!(
        fs::read_to_string(&kept_a).unwrap(),
        "EARLIER SNAPSHOT A",
        "D3: an existing snapshot must never be silently truncated"
    );
    assert_eq!(
        fs::read_to_string(&kept_b).unwrap(),
        "EARLIER SNAPSHOT B",
        "D3: an existing snapshot must never be silently truncated"
    );
    assert_eq!(
        snapshot_names(&dir).len(),
        3,
        "D3: the new snapshot must get its own unique name"
    );
    assert!(dest.is_file(), "the reported snapshot must exist");

    fs::remove_dir_all(&dir).ok();
}

/// D3 — the `check_off` auto-snapshot and the explicit archive of the **same**
/// completed plan must not accumulate byte-equivalent duplicates when the clock
/// moves past the second boundary.
#[test]
fn d3_identical_completed_plan_is_snapshotted_once() {
    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Plan\n- [x] [t-001] Done\n").unwrap();
    let bytes = plan.read().unwrap().unwrap();

    let archive = dir.join("archive");
    fs::create_dir_all(&archive).unwrap();
    // Stand in for an auto-snapshot taken at an earlier second (>1s apart is what
    // the report reproduces as two byte-equivalent snapshots).
    let prior = archive.join("execution_plan_20200101_000000.md");
    fs::write(&prior, &bytes).unwrap();

    let dest = plan.archive().unwrap().expect("must archive");

    assert_eq!(
        snapshot_names(&dir).len(),
        1,
        "D3: one completed plan must leave exactly one snapshot"
    );
    assert_eq!(
        dest, prior,
        "D3: the identical existing snapshot must be reused, not duplicated"
    );

    fs::remove_dir_all(&dir).ok();
}

/// D4 — when the active plan file cannot be removed, `archive()` must report the
/// failure instead of claiming success, and must not pretend the plan is gone.
///
/// The read-only marmel directory lets the snapshot inside `archive/` be written
/// while the `execution_plan_archive.md` copy and the active-plan removal fail.
#[cfg(unix)]
#[test]
fn d4_archive_reports_failure_when_the_active_plan_is_not_removed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_marmel();
    let plan = Plan::at(&dir);
    plan.create("# Plan\n- [x] [t-001] Done\n").unwrap();
    fs::create_dir_all(dir.join("archive")).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

    let result = plan.archive();

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

    let err = match result {
        Ok(other) => panic!(
            "D4: archive() must not report success while the active plan is still \
             on disk, got {other:?}"
        ),
        Err(err) => err,
    };
    assert!(
        plan.exists(),
        "the active plan is still on disk, so the verdict must have been an error"
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains("execution_plan"),
        "the error must name what could not be written/removed: {msg}"
    );

    fs::remove_dir_all(&dir).ok();
}
