//! Real-time execution plan progress and active worker correlation summary.
//!
//! The task-line grammar is **not** re-implemented here: every classification
//! (checked / unchecked / task id / list-marker strip) comes from
//! [`crate::plan_parse`], the single owner, so these counts agree with
//! [`crate::manager::phase::Plan::pending_tasks`] and
//! [`crate::manager::phase::Plan::all_tasks`] for the same plan on disk.

use super::workers::has_active_workers;

/// Render one task line as a step of the summary, rebuilt from the parsed
/// [`crate::plan_parse::TaskLine`] parts instead of the verbatim line text.
///
/// The step text is [`crate::plan_parse::TaskLine::description_body`] (the
/// description minus the trailing reserved directive group and minus the trailing
/// archetype role hint) with the task id and the role hint re-attached:
///
/// * The reserved validation directive — `(validate)` / `(no-validate)` — is
///   **dropped**. It is plan-line grammar honoured by the write side
///   (`agents::prompt_builder` derives the validation brief / skip record from
///   [`crate::plan_parse::TaskLine::validation_directive`]), never by prose.
///   This summary is model-facing (it becomes
///   [`crate::orchestrator::steer::SteerContext::plan_progress`], fed to the
///   arbiter by `ui::bridge::arbiter`), so echoing the token there only invites
///   the model to imitate or act on it.
/// * The role hint is **kept** — deliberate: the arbitration model has to know
///   *who owns which step* to judge overlap between a running worker and a
///   pending step and to re-delegate sensibly. A hint is a routing fact consumed
///   by the scheduler downstream, not a directive the model can trigger by
///   repeating a token, so the imitation hazard that disqualifies the reserved
///   directive does not apply. Spelling is normalised to the lower-cased token
///   produced by [`crate::plan_parse::split_role_hint`].
/// * Prose parentheticals survive untouched — they live inside
///   `description_body` (`(see notes)`, `(draft (v2))`, and a reserved-looking
///   token outside the grammar position, which the fail-closed grammar treats as
///   prose exactly as a brief would).
/// * The id is re-rendered in the canonical `[t-xxx]` spelling, so it stays
///   greppable/copyable regardless of the on-disk spelling (`(t-xxx)`, bare
///   `t-xxx`, bold-wrapped). A line whose id sits inside the text (no leading id
///   token) keeps the id where the plan wrote it, inside the body.
fn render_step(task: &crate::plan_parse::TaskLine) -> String {
    let mut step = String::new();
    if let Some(id) = task.task_id.as_deref() {
        step.push('[');
        step.push_str(id);
        step.push(']');
    }
    let body = task.description_body.trim();
    if !body.is_empty() {
        if !step.is_empty() {
            step.push(' ');
        }
        step.push_str(body);
    }
    if let Some(role) = task.role_hint.as_deref() {
        if !step.is_empty() {
            step.push(' ');
        }
        step.push('(');
        step.push_str(role);
        step.push(')');
    }
    step
}

/// Dynamically parses the markdown execution plan and correlates each task with active background workers.
/// Generates real-time breakdown of Completed, Currently In Progress, and Pending steps.
pub fn generate_plan_progress_summary(plan_content: &str) -> String {
    if plan_content.trim().is_empty() || plan_content == "None" {
        return "No active execution plan on disk.".to_string();
    }

    let mut completed_tasks = Vec::new();
    let mut in_progress_tasks = Vec::new();
    let mut pending_tasks = Vec::new();

    let _has_workers = has_active_workers();

    for task in crate::plan_parse::parse_tasks(plan_content) {
        let clean_line = render_step(&task);
        match task.checkbox {
            crate::plan_parse::CheckboxState::Checked => {
                completed_tasks.push(clean_line.to_string());
            }
            crate::plan_parse::CheckboxState::Unchecked => {
                let matched_active = task
                    .task_id
                    .as_deref()
                    .map(|id| id.to_ascii_lowercase())
                    .and_then(|lower| super::workers::get_active_subtask_by_id(&lower));

                if let Some((agent_name, running_time)) = matched_active {
                    in_progress_tasks.push(format!(
                        "{} (Assigned to: {}, Running: {})",
                        clean_line, agent_name, running_time
                    ));
                } else {
                    pending_tasks.push(clean_line.to_string());
                }
            }
        }
    }

    let total_tasks = completed_tasks.len() + in_progress_tasks.len() + pending_tasks.len();
    if total_tasks == 0 {
        return "Execution plan contains no checklist items ([ ] or [x]).".to_string();
    }

    let completion_pct = (completed_tasks.len() as f64 / total_tasks as f64 * 100.0).round() as u64;

    let mut summary = String::new();
    if let Some((inst, wall)) = crate::manager::phase::get_plan_start_time() {
        let elapsed_secs = inst.elapsed().as_secs();
        let elapsed_str = super::workers::format_duration_human(elapsed_secs);
        let wall_str = wall.format("%H:%M:%S");
        summary.push_str(&format!(
            "Plan Started At: {} (running for {}, {} total seconds)\n",
            wall_str, elapsed_str, elapsed_secs
        ));
    }

    summary.push_str(&format!(
        "Overall Progress: {}/{} tasks completed ({}%)\n\n",
        completed_tasks.len(),
        total_tasks,
        completion_pct
    ));

    if !completed_tasks.is_empty() {
        summary.push_str(&format!(
            "### Completed Steps ({}/{}):\n",
            completed_tasks.len(),
            total_tasks
        ));
        for task in &completed_tasks {
            summary.push_str(&format!("- {}\n", task));
        }
        summary.push('\n');
    }

    if !in_progress_tasks.is_empty() {
        summary.push_str(&format!(
            "### Currently In Progress ({}/{}):\n",
            in_progress_tasks.len(),
            total_tasks
        ));
        for task in &in_progress_tasks {
            summary.push_str(&format!("- {}\n", task));
        }
        summary.push('\n');
    }

    if !pending_tasks.is_empty() {
        summary.push_str(&format!(
            "### Pending Steps ({}/{}):\n",
            pending_tasks.len(),
            total_tasks
        ));
        for task in &pending_tasks {
            summary.push_str(&format!("- {}\n", task));
        }
    }

    summary.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_progress_summary_includes_plan_start_time() {
        crate::manager::phase::record_plan_start();
        let plan =
            "- [ ] [t-summary-unique-101] First task\n- [x] [t-summary-unique-102] Second task\n";
        let summary = generate_plan_progress_summary(plan);
        assert!(summary.contains("Plan Started At:"));
        assert!(summary.contains("Overall Progress: 1/2 tasks completed (50%)"));
        assert!(summary.contains("### Completed Steps (1/2):"));
        assert!(summary.contains("### Pending Steps (1/2):"));
        crate::manager::phase::clear_plan_start();
    }

    /// The summary counts exactly what the plan grammar on disk counts,
    /// including parenthesised, bold, star and numbered task-line spellings.
    #[test]
    fn test_plan_progress_summary_agrees_with_plan_parse_counts() {
        let plan = "\
# Execution Plan
- [x] [t-c3sum-1] Baseline
- [ ] (t-c3sum-2) Migrate schema
- [X] **[t-c3sum-3]** Docs
* [x] (t-c3sum-4) Star bullet, paren tick
1. [ ] [t-c3sum-5] Numbered bullet
Prose line mentioning t-c3sum-2 must not be counted.
";
        let all = crate::plan_parse::all_task_ids(plan);
        let pending = crate::plan_parse::unchecked_task_ids(plan);
        assert_eq!(all.len(), 5, "five task lines expected: {all:?}");
        assert_eq!(pending, vec!["t-c3sum-2", "t-c3sum-5"]);

        let summary = generate_plan_progress_summary(plan);
        assert!(
            summary.contains("Overall Progress: 3/5 tasks completed (60%)"),
            "counts diverge from plan_parse:\n{summary}"
        );
        assert!(summary.contains("### Pending Steps (2/5):"), "\n{summary}");
        for id in &pending {
            assert!(summary.contains(id), "pending {id} missing:\n{summary}");
        }
        assert!(
            !summary.contains("Prose line mentioning"),
            "non-task prose must not be reported as a step:\n{summary}"
        );
    }

    /// The summary is model-facing: it becomes
    /// `orchestrator::steer::SteerContext::plan_progress` for the arbiter
    /// (`ui::bridge::arbiter`). A reserved validation token echoed there is an
    /// imitation hazard, so the step is rendered from `description_body`: the
    /// `(validate)` / `(no-validate)` token is gone while the task text, the task
    /// id and the owning role remain, and a non-reserved parenthetical such as
    /// `(see notes)` survives untouched.
    #[test]
    fn test_plan_progress_summary_never_echoes_reserved_validation_tokens() {
        let plan = "\
- [x] [t-sumdir-01] Ship the release (validate) (coder)
- [ ] [t-sumdir-02] Investigate library X (no-validate) (researcher)
- [ ] [t-sumdir-03] Fix the crash (see notes)
- [ ] [t-sumdir-04] Re-audit the schema (NO-VALIDATE) (Coder)
";
        let summary = generate_plan_progress_summary(plan);
        let lower = summary.to_ascii_lowercase();

        assert!(
            !lower.contains("(validate)"),
            "reserved directive leaked into the arbitration context:\n{summary}"
        );
        assert!(
            !lower.contains("(no-validate)"),
            "reserved directive leaked into the arbitration context:\n{summary}"
        );
        assert!(
            !lower.contains("validate"),
            "no spelling of the reserved vocabulary may survive:\n{summary}"
        );

        // The step is still identifiable: task text + id + owning role.
        for expected in [
            "[t-sumdir-01] Ship the release (coder)",
            "[t-sumdir-02] Investigate library X (researcher)",
            "[t-sumdir-03] Fix the crash (see notes)",
            "[t-sumdir-04] Re-audit the schema (coder)",
        ] {
            assert!(
                summary.contains(expected),
                "step rendered as expected {expected:?}:\n{summary}"
            );
        }

        // Rendering changed, counting did not.
        assert!(
            summary.contains("Overall Progress: 1/4 tasks completed (25%)"),
            "counts must be unaffected by the directive strip:\n{summary}"
        );
    }

    /// A line without a leading id keeps its id inside the body, and a
    /// directive-free line renders exactly as the plan words it.
    #[test]
    fn test_plan_progress_summary_keeps_decorated_and_plain_lines() {
        let plan = "\
- [x] **[t-sumdir-05]** Docs refreshed
* [ ] t-sumdir-6 Ship it (draft (v2))
";
        let summary = generate_plan_progress_summary(plan);
        assert!(
            summary.contains("[t-sumdir-05] Docs refreshed"),
            "bold/bracketed id normalised, text kept:\n{summary}"
        );
        assert!(
            summary.contains("[t-sumdir-6] Ship it (draft (v2))"),
            "nested prose parenthetical must survive:\n{summary}"
        );
    }

    /// Empty / header-only plans keep the historical "no checklist items" text.
    #[test]
    fn test_plan_progress_summary_without_checklist_items() {
        assert_eq!(
            generate_plan_progress_summary("# Execution Plan\n\nprose only\n"),
            "Execution plan contains no checklist items ([ ] or [x])."
        );
        assert_eq!(
            generate_plan_progress_summary("   "),
            "No active execution plan on disk."
        );
    }
}
