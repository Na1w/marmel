//! t-035a — plan check-off integrity, end to end at the live call sites.
//!
//! What is pinned here is the *boundary* the UI and the orchestrator share:
//!
//! 1. a deliverable **without** a terminal completion marker never ticks a plan
//!    box, no matter that its tool result carried `is_error == false`;
//! 2. a deliverable **with** a valid marker (body-decorated, or the structured
//!    marker the orchestrator threads through) checks its own box off;
//! 3. a marker that names a **different** task id never ticks the other box;
//! 4. a plan that cannot be read or parsed is **surfaced** as a warning and is
//!    never treated as "all tasks done" (bug M8 at the UI boundary).
//!
//! Every test is pty-free, asserts no process-global counter, and roots its plan
//! in a private temporary workspace (`harness::with_workspace_root`) so the
//! repository's real `.marmel/` is never touched. Run them serially:
//! `cargo test --test test_plan_checkoff -- --test-threads=1`.

use marmennill::manager::Plan;
use marmennill::markers::{MARKER_COMPLETE, MARKER_FAILED, decorated};
use marmennill::ui::session::{
    CheckOffOutcome, PlanGate, check_off_warning, marker_gated_check_off, read_plan_gate,
};

/// Two-task plan used by the check-off tests.
const PLAN_MD: &str = "# Execution Plan\n\
- [ ] [t-001] Close the marker gate\n\
- [ ] [t-002] Surface plan parse errors\n";

/// Plan rooted inside the caller's temporary workspace — never the repo's.
fn plan_at(root: &std::path::Path) -> Plan {
    Plan::at(root.join(".marmel"))
}

fn on_disk(plan: &Plan) -> String {
    std::fs::read_to_string(plan.plan_path()).expect("plan file must exist")
}

/// The gate exactly as both live session loops use it: run the marker-gated
/// check-off, then ask whether the outcome deserves a user-visible warning.
/// Returns the classified outcome plus the warning text (if any).
fn gate(
    plan: &Plan,
    task_id: Option<&str>,
    deliverable: &str,
) -> (CheckOffOutcome, Option<String>) {
    let outcome = marker_gated_check_off(plan, task_id, deliverable);
    (outcome.clone(), check_off_warning(task_id, &outcome))
}

/// (a) A deliverable with **no** terminal marker must not check off a task —
/// even though the session reaches this path with `is_error == false` and a
/// task id bound from the `delegate_task` arguments.
#[tokio::test]
async fn deliverable_without_a_completion_marker_never_checks_off() {
    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root.clone(), async move {
        let plan = plan_at(&root);
        plan.create(PLAN_MD).expect("create plan");

        // Plausibly successful prose: it merely *mentions* the task id and
        // reads like a pass, but it carries no verdict at all.
        let deliverable = "Reworked t-001, ran cargo test --lib: 0 failed. Ship it.";

        // The shared session entry point (used by both live loops).
        let (outcome, warning) = gate(&plan, Some("t-001"), deliverable);
        assert_eq!(
            outcome,
            CheckOffOutcome::NoMarker,
            "a marker-less deliverable must not check off its task"
        );
        let warning = warning.expect("a missing marker is surfaced to the user");
        assert!(
            warning.contains(MARKER_COMPLETE) && warning.contains("t-001"),
            "the warning must name the missing marker and the task: {warning}"
        );
        // The orchestrator's entry point is the same gate, not a second one.
        assert!(
            !plan
                .check_plan_on_deliverable(None, Some("t-001"), deliverable)
                .expect("not an IO error"),
            "the plan layer must reach the same verdict"
        );

        let disk = on_disk(&plan);
        assert!(
            disk.contains("- [ ] [t-001]"),
            "the box must stay unchecked:\n{disk}"
        );
        assert_eq!(
            plan.pending_tasks(),
            vec!["t-001".to_string(), "t-002".to_string()],
            "both tasks remain pending"
        );
        assert!(!plan.is_complete(), "the plan must not self-complete");

        // Non-vacuity witness: the raw (tool-only) mutation *would* have ticked
        // the box — the marker gate is what stopped it. Nothing is written.
        let (rewritten, flipped) = marmennill::plan_parse::check_off_content(&disk, "t-001");
        assert!(
            flipped && rewritten.contains("- [x] [t-001]"),
            "witness: the pre-fix `check_off` path ticks the box regardless:\n{rewritten}"
        );
    })
    .await;
}

/// (b) A deliverable carrying a valid `MISSION COMPLETE (t-001)` marker checks
/// off exactly that task — through the session path (body marker) and through
/// the orchestrator path (structured marker threaded into the plan layer).
#[tokio::test]
async fn deliverable_with_a_valid_marker_checks_off_its_own_task() {
    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root.clone(), async move {
        let plan = plan_at(&root);
        plan.create(PLAN_MD).expect("create plan");

        // Session path: the tool result content carries the decorated marker.
        let deliverable = format!(
            "Gated both session call sites and ran the suite.\n\n{}",
            decorated(MARKER_COMPLETE, "t-001")
        );
        let (outcome, warning) = gate(&plan, Some("t-001"), &deliverable);
        assert_eq!(
            outcome,
            CheckOffOutcome::Flipped,
            "a real completion marker must check off the bound task"
        );
        assert!(
            warning.is_none(),
            "a successful check-off needs no warning: {warning:?}"
        );
        let disk = on_disk(&plan);
        assert!(
            disk.contains("- [x] [t-001]"),
            "t-001 must be flipped:\n{disk}"
        );
        assert!(
            disk.contains("- [ ] [t-002]"),
            "the other task is untouched:\n{disk}"
        );
        assert_eq!(plan.pending_tasks(), vec!["t-002".to_string()]);
        assert!(!plan.is_complete(), "one task still pending");

        // Orchestrator path: the structured `Deliverable.marker` is threaded in
        // rather than dropped, so the check-off works even when the body text
        // of this round carries no marker of its own.
        let structured = marmennill::agents::MissionMarker::Complete {
            task_id: Some("t-002".to_string()),
        };
        assert!(
            plan.check_plan_on_deliverable(
                Some(&structured),
                Some("t-002"),
                "revision round produced no marker text of its own"
            )
            .expect("not an IO error"),
            "the structured marker must drive the check-off"
        );
        let disk = on_disk(&plan);
        assert!(
            disk.contains("- [x] [t-002]"),
            "t-002 must be flipped by the structured marker:\n{disk}"
        );
        assert!(plan.pending_tasks().is_empty());
        assert!(plan.is_complete(), "both boxes are now ticked");
    })
    .await;
}

/// (c) A marker naming a **different** task id must never tick the other box —
/// neither when the caller binds its own id (session/orchestrator binding) nor
/// when the id is taken from the marker alone.
#[tokio::test]
async fn marker_for_a_different_task_does_not_check_off_the_bound_box() {
    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root.clone(), async move {
        let plan = plan_at(&root);
        plan.create(PLAN_MD).expect("create plan");

        // The deliverable completed t-002; the tool call had bound t-001.
        let deliverable = format!(
            "Added the parse-error surfacing helper.\n\n{}",
            decorated(MARKER_COMPLETE, "t-002")
        );
        let (outcome, warning) = gate(&plan, Some("t-001"), &deliverable);
        assert_eq!(
            outcome,
            CheckOffOutcome::IdMismatch {
                bound: "t-001".to_string(),
                marker: "t-002".to_string(),
            },
            "a completion for t-002 must not tick the box bound to t-001"
        );
        let warning = warning.expect("an id mismatch is surfaced to the user");
        assert!(
            warning.contains("t-001")
                && warning.contains("t-002")
                && warning.contains(MARKER_COMPLETE),
            "the warning must name both ids and the marker: {warning}"
        );
        let disk = on_disk(&plan);
        assert!(
            disk.contains("- [ ] [t-001]") && disk.contains("- [ ] [t-002]"),
            "a refused check-off is fail-closed: neither box moves:\n{disk}"
        );
        assert_eq!(
            plan.pending_tasks(),
            vec!["t-001".to_string(), "t-002".to_string()],
            "both tasks stay pending after the refusal"
        );

        // With no bound id the marker decides, and it only ever moves its own
        // line: t-001 stays pending.
        let (outcome, warning) = gate(&plan, None, &deliverable);
        assert_eq!(
            outcome,
            CheckOffOutcome::Flipped,
            "the marker's own task must be checked off"
        );
        assert!(
            warning.is_none(),
            "a bound id is not required for a clean check-off: {warning:?}"
        );
        let disk = on_disk(&plan);
        assert!(
            disk.contains("- [x] [t-002]"),
            "t-002 flips for its own marker:\n{disk}"
        );
        assert!(
            disk.contains("- [ ] [t-001]"),
            "t-001 is untouched:\n{disk}"
        );
        assert_eq!(plan.pending_tasks(), vec!["t-001".to_string()]);

        // Non-vacuity: the very same call with a bound id that agrees with the
        // marker is accepted (proof the refusal above is about the mismatch).
        let other = format!(
            "closed the gate.\n\n{}",
            decorated(MARKER_COMPLETE, "t-001")
        );
        let (outcome, _warning) = gate(&plan, Some("t-001"), &other);
        assert_eq!(
            outcome,
            CheckOffOutcome::Flipped,
            "an agreeing bound id must be honoured"
        );
        assert!(on_disk(&plan).contains("- [x] [t-001]"));
    })
    .await;
}

/// (d) A plan that cannot be read, or cannot be parsed into task lines, is
/// surfaced through the session's warning channel and is never reported as
/// "all tasks done". The empty pending list the old call sites saw would have
/// looked exactly like completion.
#[tokio::test]
async fn plan_parse_errors_are_surfaced_and_never_mean_complete() {
    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root.clone(), async move {
        let plan = plan_at(&root);

        // --- unreadable plan: invalid UTF-8 where the plan file lives -------
        std::fs::create_dir_all(root.join(".marmel")).expect("marmel dir");
        std::fs::write(
            plan.plan_path(),
            [
                0x2d, 0x20, 0x5b, 0x20, 0x5d, 0x20, 0x74, 0x2d, 0x30, 0x30, 0x31, 0xff, 0xfe,
            ],
        )
        .expect("write undecodable plan");

        // Witness of the bug being closed: the plain list-shaped API the session
        // used to call reports "nothing pending" for this file.
        assert!(
            plan.pending_tasks().is_empty(),
            "the old call site would have read this as an empty plan"
        );
        match read_plan_gate(&plan) {
            PlanGate::Unknown { warning } => {
                assert!(
                    warning.contains("could not be read"),
                    "the warning must name the failure: {warning}"
                );
                assert!(
                    warning.contains("UNKNOWN"),
                    "the warning must state the state is unknown, not complete: {warning}"
                );
                assert!(
                    warning.contains(&plan.plan_path().display().to_string()),
                    "the warning must name the file the user has to fix: {warning}"
                );
            }
            other => panic!("an undecodable plan must surface as Unknown, got {other:?}"),
        }
        assert!(!plan.is_complete(), "an unreadable plan is never complete");
        assert!(
            plan.check_plan_on_deliverable(
                None,
                Some("t-001"),
                &decorated(MARKER_COMPLETE, "t-001")
            )
            .is_err(),
            "the plan layer reports the read failure instead of a silent success"
        );

        // --- readable but unparseable plan ---------------------------------
        std::fs::write(plan.plan_path(), "# Notes\n\nprose only, no task boxes\n")
            .expect("rewrite plan");
        match read_plan_gate(&plan) {
            PlanGate::Unknown { warning } => {
                assert!(
                    warning.contains("could not be parsed"),
                    "the warning must say the plan failed to parse: {warning}"
                );
                assert!(
                    warning.contains("NOT 'all tasks done'"),
                    "the warning must refuse the completion reading: {warning}"
                );
            }
            other => panic!("a plan with no task line must surface as Unknown, got {other:?}"),
        }
        assert!(
            plan.try_pending_tasks().expect("readable file").is_empty(),
            "nothing parses as pending — which must not be read as completion"
        );
        assert!(!plan.is_complete(), "an unparseable plan is never complete");
        let (outcome, warning) = gate(&plan, Some("t-001"), &decorated(MARKER_COMPLETE, "t-001"));
        assert_eq!(
            outcome,
            CheckOffOutcome::Unparseable {
                plan_path: plan.plan_path().display().to_string(),
            },
            "no box can be ticked on a plan the grammar cannot read"
        );
        let warning = warning.expect("an unparseable plan is surfaced, never swallowed");
        assert!(
            warning.contains("UNKNOWN") && warning.contains("NOT 'all tasks done'"),
            "the warning must refuse the completion reading: {warning}"
        );

        // --- control: a well-formed plan reads normally ---------------------
        plan.create(PLAN_MD).expect("create plan");
        match read_plan_gate(&plan) {
            PlanGate::Read { pending, complete } => {
                assert_eq!(
                    pending,
                    vec!["t-001".to_string(), "t-002".to_string()],
                    "a readable plan reports its real pending ids"
                );
                assert!(
                    !complete,
                    "a plan with two open boxes must not read as complete"
                );
            }
            other => panic!("a well-formed plan must read as Read, got {other:?}"),
        }

        // A failed deliverable never turns a readable plan into a completed one,
        // whatever the plan layer logs on the way.
        let failed = format!(
            "{MARKER_FAILED}: the gate is still open\n{}",
            decorated(MARKER_COMPLETE, "t-001")
        );
        let (outcome, warning) = gate(&plan, Some("t-001"), &failed);
        assert_eq!(
            outcome,
            CheckOffOutcome::NotComplete {
                verdict: MARKER_FAILED.to_string(),
            },
            "a FAILED deliverable quoting the completion token must not check off"
        );
        let warning = warning.expect("a non-completion verdict is surfaced");
        assert!(
            warning.contains(MARKER_FAILED),
            "the warning must name the verdict it saw: {warning}"
        );
        assert!(
            matches!(read_plan_gate(&plan), PlanGate::Read { .. }),
            "the plan is still readable after the failed delegation"
        );
        assert_eq!(
            plan.pending_tasks(),
            vec!["t-001".to_string(), "t-002".to_string()],
            "nothing moved"
        );
    })
    .await;
}

/// t-035a regression guard: `handle_delegate_task` runs the orchestrator's own
/// marker-gated check-off (`apply_check_off`) before the session ever sees the
/// deliverable, so by the time the live loop reaches its gate the box is already
/// `[x]`. A `false` return there must **not** be reported as "no completion
/// marker" — that message would be a lie on every healthy delegation, and the
/// user would be trained to ignore plan warnings.
#[tokio::test]
async fn a_box_the_orchestrator_already_ticked_never_warns_about_a_missing_marker() {
    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root.clone(), async move {
        let plan = plan_at(&root);
        plan.create(PLAN_MD).expect("create plan");

        let deliverable = format!(
            "Routed both session sites through the shared gate.\n\n{}",
            decorated(MARKER_COMPLETE, "t-001")
        );

        // Step 1 — the orchestrator's path: the structured `Deliverable.marker`
        // is threaded into the plan layer and ticks the box.
        let structured = marmennill::agents::MissionMarker::Complete {
            task_id: Some("t-001".to_string()),
        };
        assert!(
            plan.check_plan_on_deliverable(Some(&structured), Some("t-001"), &deliverable)
                .expect("not an IO error"),
            "the orchestrator's check-off must succeed"
        );
        assert!(
            on_disk(&plan).contains("- [x] [t-001]"),
            "the orchestrator flipped the box"
        );

        // Step 2 — the session's gate now runs on the very same deliverable.
        let (outcome, warning) = gate(&plan, Some("t-001"), &deliverable);
        assert_eq!(
            outcome,
            CheckOffOutcome::AlreadyChecked,
            "the idempotent second pass must be recognised as already checked"
        );
        assert!(
            warning.is_none(),
            "REGRESSION: a healthy delegation must not print a missing-marker \
             warning (got {warning:?})"
        );
        assert!(
            on_disk(&plan).contains("- [x] [t-001]") && on_disk(&plan).contains("- [ ] [t-002]"),
            "the second pass writes nothing"
        );
        assert_eq!(
            plan.pending_tasks(),
            vec!["t-002".to_string()],
            "the pending set is unchanged by the second pass"
        );

        // A delegation bound to an id the plan never listed is silent too: there
        // is no box to tick and nothing is wrong.
        let (outcome, warning) = gate(&plan, Some("t-099"), &decorated(MARKER_COMPLETE, "t-099"));
        assert_eq!(outcome, CheckOffOutcome::NotInPlan);
        assert!(
            warning.is_none(),
            "an unbound delegation must stay silent: {warning:?}"
        );
        assert_eq!(
            plan.pending_tasks(),
            vec!["t-002".to_string()],
            "no phantom task was created"
        );
    })
    .await;
}

/// Every outcome that signals a real plan-integrity problem must produce its own
/// honest warning; the three benign outcomes must produce none. `Io` is pinned
/// with an undecodable plan file so no branch of the classifier is dead.
#[tokio::test]
async fn each_check_off_reason_classifies_and_only_the_bad_ones_warn() {
    let tmp = tempfile::tempdir().expect("isolated workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root.clone(), async move {
        let plan = plan_at(&root);

        // Io: the plan file exists but cannot be decoded.
        std::fs::create_dir_all(root.join(".marmel")).expect("marmel dir");
        std::fs::write(plan.plan_path(), [0x2d, 0x20, 0x5b, 0x20, 0x5d, 0xff, 0xfe])
            .expect("write undecodable plan");
        let (outcome, warning) = gate(&plan, Some("t-001"), &decorated(MARKER_COMPLETE, "t-001"));
        assert!(
            matches!(outcome, CheckOffOutcome::Io { .. }),
            "an undecodable plan is an IO failure, not a silent false: {outcome:?}"
        );
        assert!(
            warning.is_some(),
            "an IO failure in the check-off must be surfaced"
        );

        plan.create(PLAN_MD).expect("create plan");

        // NoMarker / NotComplete warn; the wording must not claim success.
        let (outcome, warning) = gate(&plan, Some("t-001"), "tests green, shipping.");
        assert_eq!(outcome, CheckOffOutcome::NoMarker);
        assert!(
            warning
                .expect("missing marker is a warning")
                .contains(MARKER_COMPLETE)
        );

        let failed = format!("{MARKER_FAILED}: gate still open");
        let (outcome, warning) = gate(&plan, Some("t-001"), &failed);
        assert_eq!(
            outcome,
            CheckOffOutcome::NotComplete {
                verdict: MARKER_FAILED.to_string()
            }
        );
        assert!(
            warning
                .expect("a verdict is worth showing")
                .contains(MARKER_FAILED)
        );

        // A clean flip is silent.
        let (outcome, warning) = gate(&plan, Some("t-001"), &decorated(MARKER_COMPLETE, "t-001"));
        assert_eq!(outcome, CheckOffOutcome::Flipped);
        assert!(warning.is_none());

        // And none of the warnings above ever ticked a box.
        assert!(
            on_disk(&plan).contains("- [x] [t-001]") && on_disk(&plan).contains("- [ ] [t-002]"),
            "only the clean flip moved a box"
        );
        assert_eq!(plan.pending_tasks(), vec!["t-002".to_string()]);
    })
    .await;
}
