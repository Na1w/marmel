//! Deliverable assembly and revision tracking.

pub fn update_revision(final_content: &mut String, revised: &str) {
    if !revised.is_empty() {
        if final_content.is_empty() {
            *final_content = revised.to_string();
        } else if !final_content.contains(revised) {
            final_content.push_str("\n\n");
            final_content.push_str(revised);
        }
    }
}

pub fn assemble_final_deliverable(
    validation_passed: bool,
    validator_critique: Option<&str>,
    final_content: &str,
    task_id: Option<&str>,
) -> String {
    let upper = final_content.to_ascii_uppercase();
    let has_complete = upper.contains("MISSION COMPLETE");
    let has_replan = upper.contains("REPLAN REQUIRED");

    if validation_passed {
        if has_replan {
            return final_content.to_string();
        }
        if final_content.trim().is_empty() {
            return "Specialist terminated without deliverable.\n\nFAILED (incomplete)".to_string();
        }
        let mut res = final_content.to_string();
        if let Some(tid) = task_id.filter(|t| !t.trim().is_empty()) {
            if !res.contains(&format!("MISSION COMPLETE ({tid})")) {
                res.push_str(&format!("\n\nMISSION COMPLETE ({tid})"));
            }
        } else if !has_complete {
            res.push_str("\n\nMISSION COMPLETE");
        }
        return res;
    }

    let mut rejected = String::new();
    if let Some(critique) = validator_critique {
        rejected.push_str(&format!(
            "VALIDATOR REJECTION: {critique}\n---------------\n"
        ));
    }
    let revision = final_content
        .replace("MISSION COMPLETE", "REVOKED")
        .replace("mission complete", "REVOKED");
    rejected.push_str(&revision);
    if !rejected.contains("FAILED") && !rejected.contains("REPLAN REQUIRED") {
        if validator_critique.is_some() {
            rejected.push_str("\n\nFAILED (Validator rejected deliverable)");
        } else {
            rejected.push_str("\n\nFAILED (Task incomplete or terminated prematurely)");
        }
    }
    rejected
}
