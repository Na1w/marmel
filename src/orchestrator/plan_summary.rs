//! Real-time execution plan progress and active worker correlation summary.

use super::workers::has_active_workers;

/// Dynamically parses the markdown execution plan and correlates each task with active background workers.
/// Generates real-time breakdown of Completed, Currently In Progress, and Pending steps matching Kvaser.
pub fn generate_plan_progress_summary(plan_content: &str) -> String {
    if plan_content.trim().is_empty() || plan_content == "None" {
        return "No active execution plan on disk.".to_string();
    }

    let mut completed_tasks = Vec::new();
    let mut in_progress_tasks = Vec::new();
    let mut pending_tasks = Vec::new();

    let re_id = regex::Regex::new(r"\b(t-[a-zA-Z0-9_\-]+)\b").unwrap();
    let re_checkbox_done = regex::Regex::new(r"\[[xX]\]").unwrap();
    let re_checkbox_pending = regex::Regex::new(r"\[\s*\]|\(\s*\)").unwrap();

    let _has_workers = has_active_workers();

    for line in plan_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let is_done = re_checkbox_done.is_match(trimmed);
        let is_pending = re_checkbox_pending.is_match(trimmed)
            || (!is_done && re_id.is_match(trimmed) && !trimmed.starts_with('#'));

        if is_done {
            let clean_line = trimmed
                .trim_start_matches(|c: char| {
                    c == '-'
                        || c == '*'
                        || c == '+'
                        || c == '>'
                        || c == '|'
                        || c == ' '
                        || c == '.'
                        || c.is_ascii_digit()
                })
                .trim();
            completed_tasks.push(clean_line.to_string());
        } else if is_pending {
            let clean_line = trimmed
                .trim_start_matches(|c: char| {
                    c == '-'
                        || c == '*'
                        || c == '+'
                        || c == '>'
                        || c == '|'
                        || c == ' '
                        || c == '.'
                        || c.is_ascii_digit()
                })
                .trim();

            let matched_active = if let Some(cap) = re_id.captures(trimmed) {
                let tid = cap.get(1).unwrap().as_str().to_lowercase();
                super::workers::get_active_subtask_by_id(&tid)
            } else {
                None
            };

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

    let total_tasks = completed_tasks.len() + in_progress_tasks.len() + pending_tasks.len();
    if total_tasks == 0 {
        return "Execution plan contains no checklist items ([ ] or [x]).".to_string();
    }

    let completion_pct = (completed_tasks.len() as f64 / total_tasks as f64 * 100.0).round() as u64;

    let mut summary = format!(
        "Overall Progress: {}/{} tasks completed ({}%)\n\n",
        completed_tasks.len(),
        total_tasks,
        completion_pct
    );

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
