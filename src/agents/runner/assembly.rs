//! Deliverable assembly and revision tracking.
//!
//! The mission-marker grammar is owned by [`crate::markers`]; this module only
//! composes deliverables out of that vocabulary (gate t-030). It never spells a
//! marker literal itself.

use crate::markers::{
    MARKER_COMPLETE, decorated, failed_trailer, has_complete_marker, has_failure_marker,
    has_replan_marker, revoke_completion,
};

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
    let has_complete = has_complete_marker(final_content);
    let has_replan = has_replan_marker(final_content);

    if validation_passed {
        if has_replan {
            return final_content.to_string();
        }
        if final_content.trim().is_empty() {
            return format!(
                "Specialist terminated without deliverable.\n\n{}",
                failed_trailer("incomplete")
            );
        }
        let mut res = final_content.to_string();
        if let Some(tid) = task_id.filter(|t| !t.trim().is_empty()) {
            let token = decorated(MARKER_COMPLETE, tid);
            if !res.contains(&token) {
                res.push_str(&format!("\n\n{token}"));
            }
        } else if !has_complete {
            res.push_str(&format!("\n\n{MARKER_COMPLETE}"));
        }
        return res;
    }

    let mut rejected = String::new();
    if let Some(critique) = validator_critique {
        rejected.push_str(&format!(
            "VALIDATOR REJECTION: {critique}\n---------------\n"
        ));
    }
    // Revocation is case-correct: a rejected deliverable must not keep a
    // completion marker in ANY casing, because the parser reads markers
    // case-insensitively and would still check the plan line off.
    rejected.push_str(&revoke_completion(final_content));
    if !has_failure_marker(&rejected) && !has_replan_marker(&rejected) {
        if validator_critique.is_some() {
            rejected.push_str(&format!(
                "\n\n{}",
                failed_trailer("Validator rejected deliverable")
            ));
        } else {
            rejected.push_str(&format!(
                "\n\n{}",
                failed_trailer("Task incomplete or terminated prematurely")
            ));
        }
    }
    rejected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::MissionMarker;

    /// Regression (gate t-030): the revocation rewrite used to be two
    /// case-sensitive `replace()` calls, so `Mission Complete` / `mission
    /// COMPLETE` survived a rejection — and because `MissionMarker::parse` is
    /// case-insensitive, an aborted/rejected run still reported a completion.
    #[test]
    fn regression_differently_cased_marker_is_revoked_in_a_rejected_run() {
        let variants: Vec<String> = vec![
            MARKER_COMPLETE.to_string(),
            MARKER_COMPLETE.to_lowercase(),
            "Mission Complete".to_string(),
            "Mission COMPLETE".to_string(),
            "mIsSiOn CoMpLeTe".to_string(),
        ];
        for variant in &variants {
            let out = assemble_final_deliverable(
                false,
                Some("abort requested"),
                &format!("Work stopped.\n\n{variant} (t-077)"),
                Some("t-077"),
            );
            assert!(
                out.contains(crate::markers::REVOCATION_TOKEN),
                "revocation token missing for {variant}, got: {out}"
            );
            assert!(
                !has_complete_marker(&out),
                "marker {variant} must be revoked, got: {out}"
            );
            assert!(
                MissionMarker::parse(&out).is_some_and(|m| m.is_failure()),
                "a rejected run must never parse as complete, got: {out}"
            );
        }
    }

    /// The revocation keeps the rest of the deliverable byte-for-byte and only
    /// overwrites the marker token.
    #[test]
    fn revocation_keeps_surrounding_text_and_decoration() {
        let out = assemble_final_deliverable(
            false,
            None,
            "line one\nMission Complete (t-9)\nline three",
            Some("t-9"),
        );
        assert!(out.contains(&format!(
            "line one\n{} (t-9)\nline three",
            crate::markers::REVOCATION_TOKEN
        )));
        assert!(out.contains(&failed_trailer("Task incomplete or terminated prematurely")));
    }

    /// A completion in an approved run is decorated with the task id, and the
    /// decoration comes from the shared [`decorated`] helper.
    #[test]
    fn approved_deliverable_is_decorated_once() {
        let out = assemble_final_deliverable(
            true,
            None,
            &format!("Done.\n\n{}", MARKER_COMPLETE),
            Some("t-031"),
        );
        let token = decorated(MARKER_COMPLETE, "t-031");
        assert!(out.contains(&token));
        assert_eq!(
            MissionMarker::parse(&out),
            Some(MissionMarker::Complete {
                task_id: Some("t-031".to_string())
            })
        );

        // Already decorated -> not appended twice.
        let twice =
            assemble_final_deliverable(true, None, &format!("Done. {token}"), Some("t-031"));
        assert_eq!(twice.matches(&token).count(), 1);
    }

    /// A replan verdict is passed through untouched, whatever its casing.
    #[test]
    fn replan_verdict_is_passed_through() {
        let out = assemble_final_deliverable(
            true,
            None,
            "Replan Required: the decomposition is wrong",
            Some("t-032"),
        );
        assert!(has_replan_marker(&out));
        assert!(!has_complete_marker(&out));
    }
}
