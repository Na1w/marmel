//! Real-time execution plan progress and active worker correlation summary.
//!
//! The task-line grammar is **not** re-implemented here: every classification
//! (checked / unchecked / task id / list-marker strip) comes from
//! [`crate::plan_parse`], the single owner, so these counts agree with
//! [`crate::manager::phase::Plan::pending_tasks`] and
//! [`crate::manager::phase::Plan::all_tasks`] for the same plan on disk
//! (dedup cluster C3; bug M1 in `docs/recon_bugs_orchestrator.md`).

use super::workers::has_active_workers;

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
        let clean_line = crate::plan_parse::strip_list_marker(task.raw.trim());
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

    /// The summary must count exactly what the plan grammar on disk counts —
    /// including the parenthesised / bold / star / numbered spellings that this
    /// module's old private regexes silently dropped (`(x)` in particular).
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
