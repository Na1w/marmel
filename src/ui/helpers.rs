//! Formatting, command inspection, subagent lifecycle, and error classification helpers.

use super::{Event, Renderer, SubagentDetail};
use crate::config::Config;
use crate::manager::context::ContextEngine;
use crate::orchestrator::{DelegationEvent, OrchestratorManager};
use crate::types::Message;
use anyhow::Result;

pub fn format_active_subtasks(subagents: &[SubagentDetail]) -> String {
    let global_active = crate::orchestrator::get_active_subtasks_str();
    if global_active != "None" && !global_active.trim().is_empty() {
        return global_active;
    }
    let active: Vec<_> = subagents.iter().filter(|s| s.is_active).collect();
    if active.is_empty() {
        return "None".to_string();
    }
    let mut out = String::new();
    for s in active {
        let elapsed_secs = s.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let task_id_str = s.task_id.as_deref().unwrap_or(&s.name);
        let prompt_str = if s.prompt.is_empty() {
            "None"
        } else {
            &s.prompt
        };
        out.push_str(&format!(
            "- Tool Call ID: {}\n  Subagent: {}\n  Task Prompt: {}\n  Running For: {} seconds\n\n",
            task_id_str, s.name, prompt_str, elapsed_secs
        ));
    }
    out
}

pub fn format_plan_progress_summary(plan_content: &str) -> String {
    crate::orchestrator::generate_plan_progress_summary(plan_content)
}
/// Build the execution-plan section of the system prompt — **fail-closed** (t-054).
///
/// The verdict is delegated to the single plan-read gate that t-035a added in
/// [`super::session::read_plan_gate`]; this helper invents no plan grammar of its
/// own, so the prompt and the session's nudge/completion notices can never
/// disagree about what the plan file said.
///
/// * [`super::session::PlanGate::Read`] — the plan really was read and parsed, so
///   its markdown is embedded verbatim (the body itself comes from the plan
///   layer's own reader, [`crate::manager::phase::Plan::read`], never from a
///   second reader here).
/// * [`super::session::PlanGate::Unknown`] — the plan exists but could not be
///   read or parsed. The old code folded that into "no plan block at all", which
///   is exactly what the model and the UI read as *no work pending / all
///   complete*. Instead the plan block is replaced by an explicit "plan state
///   UNKNOWN" section and the warning is returned for the caller to surface.
///
/// Returns `(prompt_section, warning)`; an empty section with no warning means
/// there is genuinely no plan file on disk.
fn plan_prompt_section(plan: &crate::manager::phase::Plan) -> (String, Option<String>) {
    match super::session::read_plan_gate(plan) {
        super::session::PlanGate::Unknown { warning } => (String::new(), Some(warning)),
        super::session::PlanGate::Read { .. } => {
            match plan.read() {
                Ok(Some(content)) if !content.trim().is_empty() => (
                    format!(
                        "\n## Active Execution Plan (`.marmel/execution_plan.md`)\nThere is an existing execution plan already active on disk:\n```markdown\n{}\n```\nDo NOT call `create_plan` unless you explicitly intend to overwrite the plan. Proceed directly with `delegate_task` to execute any remaining unchecked `- [ ] [t-xxx]` tasks.\n",
                        content.trim()
                    ),
                    None,
                ),
                // No plan file, or an empty one: nothing pending, nothing to warn about.
                Ok(_) => (String::new(), None),
                // TOCTOU only — the gate just read the same file successfully. Even
                // so it must fail closed: no plan block, and a loud warning.
                Err(e) => (
                    String::new(),
                    Some(format!(
                        "execution plan at {} could not be read: {e:#} — plan state is UNKNOWN, \
                         NOT 'all tasks done'",
                        plan.plan_path().display()
                    )),
                ),
            }
        }
    }
}

/// Rendered in place of the plan block when the plan state is unknown, so an
/// unreadable plan is never presented to the model as a finished plan.
fn plan_unknown_section(plan: &crate::manager::phase::Plan, warning: &str) -> String {
    format!(
        "\n## Execution Plan State: UNKNOWN (fail-closed)\n\
         The on-disk execution plan at `{}` could not be read or parsed:\n\
         {warning}\n\
         The plan state is UNKNOWN: do NOT treat the plan as complete, do NOT assume no work is \
         pending, and do NOT re-run finished work. Re-read that plan file directly (or ask the \
         user) before deciding what to do next.\n",
        plan.plan_path().display(),
    )
}

/// Load the system prompt and report any plan-integrity problem found on the way.
///
/// Returns `(prompt, plan_warning)`. `plan_warning` is the text the caller must
/// surface through the session warning channel (`super::session::surface_plan_warning`:
/// a renderer `Status` event plus a `Status` record in the UI transcript). The
/// helper holds no renderer, so it also (a) logs the warning through
/// `tracing::warn!` — the first half of that same channel — and (b) embeds it in
/// the prompt via [`plan_unknown_section`], so a plan that cannot be read can
/// never reach the model as "no work pending / all complete" even if a caller
/// ignores the returned warning.
pub(crate) fn load_system_prompt_with_plan_and_warning(
    cfg: &Config,
    plan: &crate::manager::phase::Plan,
) -> Result<(String, Option<String>)> {
    let content = if cfg.system_prompt_path.exists() {
        std::fs::read_to_string(&cfg.system_prompt_path)
            .unwrap_or_else(|_| include_str!("../../prompts/system.md").to_string())
    } else {
        include_str!("../../prompts/system.md").to_string()
    };
    let env_block = crate::prompts::format_environment_block();
    let mut prompt = format!("{content}\n\n{env_block}\n");
    if let Some(ref base_prompt) = cfg.base_prompt {
        let trimmed = base_prompt.trim();
        if !trimmed.is_empty() {
            if trimmed.starts_with('#') {
                prompt.push_str(&format!("\n{trimmed}\n"));
            } else {
                prompt.push_str(&format!("\n## Machine Specific Guidance\n{trimmed}\n"));
            }
        }
    }
    let (plan_section, warning) = plan_prompt_section(plan);
    if let Some(warning) = warning.as_ref() {
        tracing::warn!("{warning}");
        prompt.push_str(&plan_unknown_section(plan, warning));
    } else {
        prompt.push_str(&plan_section);
    }
    Ok((prompt, warning))
}

/// One-line decorated `name(args)` rendering for the tool-call row and transcript.
///
/// The argument knowledge and clipping live in [`crate::tool_args`] (single
/// owner, shared with the specialist runner preview); this is a thin wrapper so
/// the UI keeps a stable local name.
pub(crate) fn format_tool_call_display(name: &str, args_val: &serde_json::Value) -> String {
    crate::tool_args::preview_tool_call(name, args_val)
}
pub fn chunk_utf8(s: &str, max: usize) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let bytes = s.as_bytes();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut end = (start + max).min(bytes.len());
        while end > start && !s.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = start + s[start..].chars().next().map_or(1, |c| c.len_utf8());
        }
        chunks.push(&s[start..end]);
        start = end;
    }
    chunks
}

pub(crate) fn is_abort_command(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("/abort")
        || t.eq_ignore_ascii_case("abort")
        || t.eq_ignore_ascii_case("/exit")
        || t.eq_ignore_ascii_case("exit")
        || t.eq_ignore_ascii_case("/quit")
        || t.eq_ignore_ascii_case("quit")
        || t.eq_ignore_ascii_case("/q")
        || t.eq_ignore_ascii_case(":q")
        || t.eq_ignore_ascii_case(":q!")
        || t.eq_ignore_ascii_case("/stop")
        || t.eq_ignore_ascii_case("stop")
        || t.eq_ignore_ascii_case("/cancel")
        || t.eq_ignore_ascii_case("cancel")
}

pub(crate) fn is_reset_command(line: &str) -> bool {
    let t = line.trim();
    t.eq_ignore_ascii_case("/reset")
        || t.eq_ignore_ascii_case("/reset_plan")
        || t.eq_ignore_ascii_case("/reset-plan")
        || t.eq_ignore_ascii_case("/clear_plan")
        || t.eq_ignore_ascii_case("/clear-plan")
        || t.eq_ignore_ascii_case("/reset_execution_plan")
}

pub(crate) fn handle_reset_command(
    plan: &crate::manager::phase::Plan,
    renderer: &mut dyn Renderer,
    mut ctx: Option<&mut ContextEngine>,
) {
    let _ = plan.clear();
    let transcript = plan.transcript_path();
    if transcript.exists() {
        let _ = std::fs::remove_file(&transcript);
    }
    renderer.on_event(&Event::Message(
        "Execution plan has been cleared and reset by user.".to_string(),
    ));
    renderer.on_event(&Event::Status("Execution plan reset".to_string()));
    let _ = renderer.flush();
    if let Some(ref mut ctx) = ctx {
        ctx.append(Message::User {
            content: "[System] User executed /reset. The execution plan has been removed from disk. Return to Conversational phase."
                .to_string(),
        });
    }
}

pub(crate) fn classify_llm_error(e: &anyhow::Error) -> &'static str {
    let msg = format!("{e:#}").to_lowercase();
    if msg.contains("http") || msg.contains("status") || msg.contains("503") || msg.contains("429")
    {
        "http"
    } else if msg.contains("transport") || msg.contains("connection") {
        "connectivity"
    } else if msg.contains("timeout") {
        "timeout"
    } else if msg.contains("stream") || msg.contains("sse") {
        "stream"
    } else {
        "unknown"
    }
}

pub(crate) fn update_subagent_lifecycle(
    subagents: &mut Vec<SubagentDetail>,
    agent: crate::agents::Agent,
    task: Option<String>,
    prompt: Option<String>,
    started: bool,
) {
    let clean_task = task
        .as_ref()
        .map(|t| crate::task_id::normalize_task_id_ref(t))
        .filter(|t| !t.is_empty());
    let name = match clean_task {
        Some(t) => format!("{}-{t}", agent.as_str()),
        None => agent.as_str().to_string(),
    };
    let task_str = clean_task.unwrap_or("").to_string();
    let log_entry = if started {
        format!("started task {task_str}")
    } else {
        format!("completed task {task_str}")
    };
    let active_tokens = crate::orchestrator::get_active_worker_tokens(&name).unwrap_or(0);
    let now = std::time::Instant::now();
    if let Some(existing) = subagents.iter_mut().find(|s| s.name == name) {
        if existing.is_active && !started {
            if let Some(st) = existing.started_at.take() {
                existing.worked_duration += st.elapsed();
            }
        } else if !existing.is_active && started {
            existing.started_at = Some(now);
        }
        existing.is_active = started;
        existing.last_activity_at = Some(now);
        if active_tokens > 0 {
            existing.context_tokens = active_tokens;
        }
        if started {
            existing.task_id = task;
            if let Some(p) = prompt {
                existing.prompt = p;
            }
        }
        existing.logs.push(log_entry);
    } else {
        subagents.push(SubagentDetail {
            name,
            task_id: task,
            prompt: prompt.unwrap_or_default(),
            started_at: if started { Some(now) } else { None },
            worked_duration: std::time::Duration::ZERO,
            last_activity_at: Some(now),
            logs: vec![log_entry],
            thinking: String::new(),
            content: String::new(),
            is_active: started,
            context_tokens: active_tokens,
        });
    }
}

pub(crate) fn drain_delegation_events_with_transcript(
    manager: Option<&OrchestratorManager>,
    renderer: &mut dyn Renderer,
    subagents: &mut Vec<SubagentDetail>,
    mut ui_transcript: Option<&mut crate::ui::UiTranscript>,
) {
    let Some(manager) = manager else {
        return;
    };
    let Ok(mut events) = manager.delegation_events.lock() else {
        return;
    };
    let mut changed = false;
    for event in events.drain(..) {
        match &event {
            DelegationEvent::Started { agent, task } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, true);
                changed = true;
            }
            DelegationEvent::Completed { agent, task } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, false);
                changed = true;
                if let Some(ref mut tr) = ui_transcript {
                    let task_id = task.clone().unwrap_or_else(|| agent.to_string());
                    tr.append(crate::ui::UiRecord::TaskCompleted { task_id });
                }
            }
            DelegationEvent::Failed {
                agent,
                task,
                reason,
            } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, false);
                changed = true;
                if let Some(ref mut tr) = ui_transcript {
                    let task_id = task.clone().unwrap_or_else(|| agent.to_string());
                    tr.append(crate::ui::UiRecord::TaskFailed {
                        task_id,
                        reason: reason.clone(),
                    });
                }
            }
        }
        renderer.on_event(&Event::Delegation(event));
    }
    if changed {
        renderer.set_subagents(subagents.clone());
    }
}

/// Sanitize task id string by stripping markdown enclosing characters.
pub fn clean_task_id(tid: &str) -> String {
    crate::task_id::normalize_task_id_ref(tid).to_string()
}

/// Find a subagent in the list by matching name, task id, or suffix.
pub fn find_subagent_mut<'a>(
    subagents: &'a mut [SubagentDetail],
    name: &str,
    task_id: Option<&str>,
) -> Option<&'a mut SubagentDetail> {
    subagents.iter_mut().find(|s| {
        if s.name == name {
            return true;
        }
        if let (Some(a), Some(b)) = (s.task_id.as_deref(), task_id) {
            let a_clean = clean_task_id(a);
            let b_clean = clean_task_id(b);
            if !a_clean.is_empty() && a_clean == b_clean {
                return true;
            }
        }
        if let Some(tid) = task_id {
            let clean = clean_task_id(tid);
            if !clean.is_empty() && s.name.ends_with(&clean) {
                return true;
            }
        }
        if let Some(s_tid) = s.task_id.as_deref() {
            let clean = clean_task_id(s_tid);
            if !clean.is_empty() && name.ends_with(&clean) {
                return true;
            }
        }
        false
    })
}

/// Extract a concise, human-readable failure reason from specialist or tool error outputs.
///
/// The marker-shaped part of the vocabulary (`REPLAN REQUIRED`, `FAILED (…)`,
/// `FAILED:`) is recognised by [`crate::markers::failure_reason`] — the single
/// owner of that grammar. Everything after it (`VALIDATOR REJECTION:`, `ERROR:`,
/// abort lines, first-line fallback) is UI wording, and the truncation budget is
/// presentation, so both stay here.
pub fn extract_failure_reason(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "unknown failure".to_string();
    }

    // 1. Look for VALIDATOR REJECTION: <critique>
    if let Some(pos) = trimmed.find("VALIDATOR REJECTION:") {
        let after = &trimmed[pos + "VALIDATOR REJECTION:".len()..];
        let first_line = after.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let clean = first_line.trim().trim_end_matches('.').trim();
        if !clean.is_empty() && clean != "---------------" {
            return truncate_reason(clean, 120);
        }
    }

    // 2-4. The marker-shaped verdicts (`REPLAN REQUIRED (<task>): …`,
    //      `FAILED (<reason>)`, `FAILED: <reason>`) are parsed by their single
    //      owner, `crate::markers`, which walks one failure-prefix table
    //      (gate t-059 — this site used to hand-roll three literal `find`s).
    //      The 120-char budget stays here: truncation is presentation, not
    //      grammar, and the rendered bytes are unchanged.
    if let Some(reason) = crate::markers::failure_reason(trimmed) {
        return truncate_reason(reason, 120);
    }

    // 3. Look for ERROR: <reason>
    if let Some(pos) = trimmed.find("ERROR:") {
        let after = &trimmed[pos + "ERROR:".len()..];
        let first_line = after.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let clean = first_line.trim();
        if !clean.is_empty() {
            return truncate_reason(clean, 120);
        }
    }

    // 4. Look for lines starting with "Task aborted"
    for line in trimmed.lines() {
        let l = line.trim();
        if l.to_ascii_lowercase().starts_with("task aborted") {
            return truncate_reason(l, 120);
        }
    }

    // 7. Fallback: first non-empty line
    let first_line = trimmed
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(trimmed);
    let clean = first_line
        .trim()
        .trim_matches(|c| c == '*' || c == '`' || c == '#')
        .trim();
    truncate_reason(clean, 120)
}

fn truncate_reason(s: &str, max_len: usize) -> String {
    let s = s.trim();
    // Char-boundary-safe prefix (shared helper): the old `s[..max_len]` byte
    // slice could panic mid-character on multi-byte text (å/ä/ö, emoji, …).
    let head = crate::text_util::truncate_chars(s, max_len);
    if head.len() == s.len() {
        s.to_string()
    } else {
        let mut truncated = head.to_string();
        if let Some(last_space) = truncated.rfind(' ')
            && truncated[..last_space].chars().count() > max_len * 2 / 3
        {
            truncated.truncate(last_space);
        }
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_abort_command_recognizes_variants() {
        assert!(is_abort_command("/abort"));
        assert!(is_abort_command("abort"));
        assert!(is_abort_command("ABORT"));
        assert!(is_abort_command("/exit"));
        assert!(is_abort_command("exit"));
        assert!(is_abort_command("/quit"));
        assert!(is_abort_command("quit"));
        assert!(is_abort_command("/q"));
        assert!(is_abort_command(":q"));
        assert!(is_abort_command(":q!"));
        assert!(is_abort_command("/stop"));
        assert!(is_abort_command("stop"));
        assert!(is_abort_command("/cancel"));
        assert!(is_abort_command("cancel"));
        assert!(!is_abort_command("continue"));
        assert!(!is_abort_command("status"));
    }

    #[test]
    fn test_format_tool_call_utf8_char_boundary_no_panic() {
        let cmd_prefix = "c".repeat(56);
        let cmd = format!("{cmd_prefix}—cargo check");
        let args = serde_json::json!({ "command": cmd });
        let formatted = format_tool_call_display(crate::tool_names::TOOL_RUN_COMMAND, &args);
        assert!(formatted.starts_with("run_command("));
        assert!(formatted.ends_with("…)"));

        let custom_prefix = "d".repeat(56);
        let custom_args = serde_json::json!({ "arg": format!("{custom_prefix}—value") });
        let custom_formatted = format_tool_call_display("custom", &custom_args);
        assert!(custom_formatted.starts_with("custom("));
        assert!(custom_formatted.ends_with("…)"));
    }

    #[test]
    fn test_extract_failure_reason() {
        assert_eq!(
            extract_failure_reason(
                "VALIDATOR REJECTION: Specialist generated conversational text without executing any tools.\n---------------\nFAILED"
            ),
            "Specialist generated conversational text without executing any tools"
        );
        assert_eq!(
            extract_failure_reason(
                "REPLAN REQUIRED (t-002): task too complex — exceeded reasoning budget"
            ),
            "task too complex — exceeded reasoning budget"
        );
        assert_eq!(
            extract_failure_reason("REPLAN REQUIRED: missing dependency foo"),
            "missing dependency foo"
        );
        assert_eq!(
            extract_failure_reason("Task aborted by user instruction.\n\nFAILED (aborted)"),
            "aborted"
        );
        assert_eq!(
            extract_failure_reason("FAILED: compilation error on line 42"),
            "compilation error on line 42"
        );
        assert_eq!(
            extract_failure_reason("ERROR: timeout waiting for response"),
            "timeout waiting for response"
        );
        assert_eq!(
            extract_failure_reason("Custom single-line failure message"),
            "Custom single-line failure message"
        );
        assert_eq!(extract_failure_reason("   "), "unknown failure");
    }

    /// Gate t-059 byte-pin (table driven): the marker-shaped verdicts are read
    /// through [`crate::markers::failure_reason`], and the **rendered output of
    /// this UI helper is unchanged** — same reason text, same 120-char budget.
    #[test]
    fn test_extract_failure_reason_marker_shapes_are_byte_pinned() {
        let cases: &[(&str, &str)] = &[
            // `FAILED (<reason>)`
            (
                "Task aborted by user instruction.\n\nFAILED (aborted)",
                "aborted",
            ),
            (
                "Report\n\nFAILED (Validator rejected deliverable)",
                "Validator rejected deliverable",
            ),
            ("FAILED (nested (inner) outer)", "nested (inner"),
            // `FAILED: <reason>`
            (
                "FAILED: compilation error on line 42",
                "compilation error on line 42",
            ),
            ("FAILED: first line\nignored second line", "first line"),
            ("FAILED:\n\n  wrapped reason\n", "wrapped reason"),
            // `REPLAN REQUIRED …`
            (
                "REPLAN REQUIRED (t-002): task too complex — exceeded reasoning budget",
                "task too complex — exceeded reasoning budget",
            ),
            (
                "REPLAN REQUIRED: missing dependency foo",
                "missing dependency foo",
            ),
            ("REPLAN REQUIRED bare verdict text", "bare verdict text"),
            // a replan verdict wins over a FAILED shape in the same body
            (
                "FAILED: stale note\nREPLAN REQUIRED: the decomposition is wrong",
                "the decomposition is wrong",
            ),
            // whitespace around the whole body must not move the reason
            ("  \n FAILED ( padded )  \n", "padded"),
        ];
        for (input, expected) in cases {
            assert_eq!(extract_failure_reason(input), *expected, "input: {input:?}");
        }

        // Verdict shapes with no reason clause fall through to the UI's own
        // non-marker heuristics, exactly as before the migration.
        let fallthrough: &[(&str, &str)] = &[
            ("FAILED ()", "FAILED ()"),
            ("REPLAN REQUIRED (t-003)", "REPLAN REQUIRED (t-003)"),
            (
                "ERROR: timeout waiting for response",
                "timeout waiting for response",
            ),
            (
                "Task aborted by user instruction.",
                "Task aborted by user instruction.",
            ),
        ];
        for (input, expected) in fallthrough {
            assert_eq!(extract_failure_reason(input), *expected, "input: {input:?}");
        }
    }

    /// Gate t-059: for any marker-shaped input the UI's answer is exactly the
    /// marker owner's reason, truncated by the UI's own budget — the two layers
    /// cannot drift apart silently.
    #[test]
    fn test_extract_failure_reason_delegates_to_the_marker_owner() {
        let inputs = [
            "FAILED (aborted)",
            "Task aborted by user instruction.\n\nFAILED (aborted)",
            "FAILED: build broke",
            "REPLAN REQUIRED (t-002): plan is wrong",
            "REPLAN REQUIRED: plan is wrong",
            "preface\n\nFAILED (Validator rejected deliverable)\ntrailer",
        ];
        for input in inputs {
            let owner = crate::markers::failure_reason(input.trim())
                .unwrap_or_else(|| panic!("{input:?} must yield a marker reason"));
            assert_eq!(
                extract_failure_reason(input),
                truncate_reason(owner, 120),
                "input: {input:?}"
            );
        }
        // The budget is UI-side: a long marker reason is still truncated here,
        // never by the marker owner.
        let long = "FAILED: ".to_string() + &"problem ".repeat(40);
        let out = extract_failure_reason(&long);
        assert!(out.ends_with("..."), "{out}");
        assert!(out.chars().count() <= 123, "{out}");
    }

    #[test]
    fn test_truncate_reason_multibyte_no_panic() {
        // Regression (C7): the old `&s[..max_len]` byte slice panicked with
        // "byte index is not a char boundary" on multi-byte reasons crossing
        // the slice point. These inputs cross it deliberately.
        // "x" + 60x 'ä' = 121 bytes; byte index 120 falls inside an 'ä'.
        let s = format!("{}{}", "x", "ä".repeat(60));
        assert_eq!(s.len(), 121);
        assert!(!s.is_char_boundary(120));
        assert_eq!(truncate_reason(&s, 120), s);

        // "a" + 30x emoji = 121 bytes; byte index 120 falls inside a 🙂.
        let e = format!("{}{}", "a", "🙂".repeat(30));
        assert_eq!(e.len(), 121);
        assert!(!e.is_char_boundary(120));
        assert_eq!(truncate_reason(&e, 120), e);

        // Long enough to also exercise the truncation branch with multi-byte text.
        let long = format!("{}{}", "x", "ä".repeat(150));
        let out = truncate_reason(&long, 120);
        assert_eq!(out.chars().count(), 123); // 120 chars + "..."
        assert!(out.ends_with("..."));
        assert!(!out.contains(std::char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn test_truncate_reason_ascii_word_break_unchanged() {
        // Visible output preserved for ASCII: word break at the last space
        // past 2/3 of the limit, then "...".
        assert_eq!(
            truncate_reason("the quick brown fox jumps over", 20),
            "the quick brown fox..."
        );
        assert_eq!(truncate_reason(&"a".repeat(20), 20), "a".repeat(20));
        assert_eq!(
            truncate_reason(&"ä".repeat(21), 20),
            format!("{}...", "ä".repeat(20))
        );
    }

    #[test]
    fn test_extract_failure_reason_multibyte_no_panic() {
        // End-to-end at the call site: a >120-char multi-byte FAILED reason.
        let reason = format!(
            "{}{}{}",
            "problem ",
            "självråkdiskriminering 👍 ",
            "wäsen".repeat(30)
        );
        let out = extract_failure_reason(&format!("FAILED: {reason}"));
        assert!(out.ends_with("..."));
        assert!(!out.contains(std::char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn test_load_system_prompt_without_base_prompt() {
        let cfg = Config::default();
        let plan = crate::manager::phase::Plan::default();
        let prompt = load_system_prompt_with_plan_and_warning(&cfg, &plan)
            .map(|(prompt, _warning)| prompt)
            .expect("load success");
        assert!(!prompt.contains("## Machine Specific Guidance"));
    }

    #[test]
    fn test_load_system_prompt_with_base_prompt() {
        let cfg = Config {
            base_prompt: Some("Schemalägg inga tasks parallellt.".to_string()),
            ..Default::default()
        };
        let plan = crate::manager::phase::Plan::default();
        let prompt = load_system_prompt_with_plan_and_warning(&cfg, &plan)
            .map(|(prompt, _warning)| prompt)
            .expect("load success");
        assert!(prompt.contains("## Machine Specific Guidance"));
        assert!(prompt.contains("Schemalägg inga tasks parallellt."));
    }

    #[test]
    fn test_load_system_prompt_with_heading_base_prompt() {
        let cfg = Config {
            base_prompt: Some("### Custom Instructions\nSingle worker only.".to_string()),
            ..Default::default()
        };
        let plan = crate::manager::phase::Plan::default();
        let prompt = load_system_prompt_with_plan_and_warning(&cfg, &plan)
            .map(|(prompt, _warning)| prompt)
            .expect("load success");
        assert!(!prompt.contains("## Machine Specific Guidance"));
        assert!(prompt.contains("### Custom Instructions\nSingle worker only."));
    }

    #[test]
    fn test_load_system_prompt_with_whitespace_base_prompt() {
        let cfg = Config {
            base_prompt: Some("   \n\t  ".to_string()),
            ..Default::default()
        };
        let plan = crate::manager::phase::Plan::default();
        let prompt = load_system_prompt_with_plan_and_warning(&cfg, &plan)
            .map(|(prompt, _warning)| prompt)
            .expect("load success");
        assert!(!prompt.contains("## Machine Specific Guidance"));
    }

    /// Dedup cluster C1 part 2: this owned UI seam is a pure delegation to
    /// `crate::task_id` — re-spelling the trimming chain here fails this test.
    #[test]
    fn test_clean_task_id_delegates_to_task_id_normalizer() {
        for raw in [
            "[t-001]",
            "\"t-001\"",
            "  ('t-001')  ",
            "t-001",
            "[t-001",
            "[t-001]  ",
        ] {
            assert_eq!(clean_task_id(raw), "t-001", "owned UI seam for {raw:?}");
            assert_eq!(
                clean_task_id(raw),
                crate::task_id::normalize_task_id_ref(raw),
                "owned UI seam diverges from the canonical normalizer for {raw:?}"
            );
        }
        for raw in ["", "[]", "   ", "\"\"", "[ ]", "[\"\"]"] {
            assert_eq!(clean_task_id(raw), "", "owned UI seam for {raw:?}");
        }
    }

    /// The `find_subagent_mut` matching layer on top of the seam must keep
    /// decorated and plain spellings interchangeable, and must never let an
    /// empty normalization match everything.
    #[test]
    fn test_find_subagent_mut_matches_decorated_task_ids() {
        let mut subs = vec![SubagentDetail {
            name: "coder-t-001".to_string(),
            ..Default::default()
        }];
        assert!(find_subagent_mut(&mut subs, "coder", Some("[t-001]")).is_some());
        assert!(find_subagent_mut(&mut subs, "coder", Some("t-001")).is_some());
        assert!(find_subagent_mut(&mut subs, "coder", Some("[]")).is_none());
    }

    // ---------------------------------------------------------------------
    // t-054 — the system prompt must read the plan fail-closed
    // (regression guard for the swallowed `if let Ok(Some(..))` read).
    //
    // Every test below roots its plan in a private temporary workspace through
    // `harness::with_workspace_root`, so the repository's real `.marmel/` is
    // never touched, and none of them asserts on a process-global counter.
    // ---------------------------------------------------------------------

    /// A two-task plan with multi-byte text, nested markdown and a tab-indented
    /// line: used to prove the readable-plan path embeds the body byte for byte.
    const PLAN_MD: &str = "# Execution Plan\n\
- [x] [t-001] Normalisera plan-läsningen — `åäö` ✅\n\
- [ ] [t-002] Surface plan-read errors\n\
\n\tindented\ttab + trailing spaces   \n";

    /// Run `f` against a plan rooted inside a private temporary workspace.
    /// The `TempDir` and the plan path are handed back so assertions can still
    /// inspect the real file while the workspace is alive.
    fn with_isolated_plan<T, F: std::future::Future<Output = T>>(
        f: impl FnOnce(crate::manager::phase::Plan) -> F,
    ) -> (tempfile::TempDir, std::path::PathBuf, T) {
        let tmp = tempfile::tempdir().expect("isolated workspace root");
        let root = tmp.path().to_path_buf();
        // `harness::with_workspace_root` is the repo's own scoping seam; drive it
        // from a plain pty-free unit test without spinning up a runtime.
        let (path, out) = futures::executor::block_on(crate::harness::with_workspace_root(
            root.clone(),
            async move {
                let plan = crate::manager::phase::Plan::at(root.join(".marmel"));
                std::fs::create_dir_all(plan.dir()).expect("create isolated .marmel dir");
                let path = plan.plan_path();
                (path, f(plan).await)
            },
        ));
        (tmp, path, out)
    }

    /// (1) A plan that exists but cannot be **read** must hand back a surfaced
    /// warning, and the prompt must never be presented as a finished plan.
    #[test]
    fn test_load_system_prompt_unreadable_plan_is_surfaced_and_not_complete() {
        let (_tmp, plan_path, (prompt, warning)) = with_isolated_plan(|plan| async move {
            // Not valid UTF-8: `Plan::read` reports an error instead of folding
            // the failure into "no plan".
            std::fs::write(plan.plan_path(), b"\xff\xfe\x00 not a plan").expect("write plan bytes");
            assert!(plan.exists(), "the plan file must exist on disk");
            assert!(
                plan.read().is_err(),
                "non-UTF-8 plan bytes must make the plan read fail"
            );
            load_system_prompt_with_plan_and_warning(&Config::default(), &plan)
                .expect("the prompt itself must still load")
        });

        let warning = warning.expect("an unreadable plan must hand back a warning to surface");
        assert!(
            warning.contains("plan state is UNKNOWN"),
            "the warning must say the plan state is unknown: {warning}"
        );
        assert!(
            warning.contains(&plan_path.display().to_string()),
            "the warning must name the plan file the user can act on: {warning}"
        );

        // Fail-closed: the prompt carries the warning visibly, and must not
        // present a plan block (the old silent omission read as "no work
        // pending / all complete").
        assert!(
            prompt.contains("## Execution Plan State: UNKNOWN"),
            "the prompt itself must carry the UNKNOWN section:\n{prompt}"
        );
        assert!(
            prompt.contains("plan state is UNKNOWN"),
            "the prompt must repeat the surfaced warning text:\n{prompt}"
        );
        assert!(
            prompt.contains(&plan_path.display().to_string()),
            "the prompt must name the unreadable plan file:\n{prompt}"
        );
        assert!(
            !prompt.contains("## Active Execution Plan"),
            "no plan block may be rendered for an unreadable plan:\n{prompt}"
        );
        assert!(
            !prompt.contains("tasks are now COMPLETE"),
            "an unreadable plan must never be presented as complete:\n{prompt}"
        );
        assert!(
            !prompt.contains("All execution plan tasks"),
            "an unreadable plan must never be reported as all-complete:\n{prompt}"
        );
    }

    /// (1b) A plan that **is** readable but carries no parseable task line is
    /// equally unknown — the shared gate owns that verdict, not this helper.
    #[test]
    fn test_load_system_prompt_unparseable_plan_is_surfaced_and_not_complete() {
        let (_tmp, _plan_path, (prompt, warning)) = with_isolated_plan(|plan| async move {
            std::fs::write(plan.plan_path(), "just prose, no task boxes at all\n").expect("write");
            load_system_prompt_with_plan_and_warning(&Config::default(), &plan)
                .expect("prompt loads")
        });
        let warning = warning.expect("an unparseable plan must hand back a warning");
        assert!(warning.contains("plan state is UNKNOWN"), "{warning}");
        assert!(prompt.contains("## Execution Plan State: UNKNOWN"));
        assert!(!prompt.contains("## Active Execution Plan"));
    }

    /// (1c) No plan file on disk is genuinely "nothing pending" and must **not**
    /// warn — the gate keeps that case apart from a failed read.
    #[test]
    fn test_load_system_prompt_missing_plan_does_not_warn() {
        let (_tmp, _plan_path, (prompt, warning)) = with_isolated_plan(|plan| async move {
            assert!(!plan.exists(), "no plan file in the fresh workspace");
            load_system_prompt_with_plan_and_warning(&Config::default(), &plan)
                .expect("prompt loads")
        });
        assert!(warning.is_none(), "a missing plan file must not warn");
        assert!(!prompt.contains("UNKNOWN"), "no UNKNOWN section: {prompt}");
        assert!(!prompt.contains("## Active Execution Plan"), "{prompt}");
    }

    /// (2) A readable plan still embeds the plan block byte-faithfully, and the
    /// loader's prompt projection stays identical across calls (the projection the
    /// removed `load_system_prompt_with_plan` wrapper used to provide).
    #[test]
    fn test_load_system_prompt_readable_plan_embeds_plan_block_byte_faithfully() {
        let cfg = Config::default();
        let (_tmp, plan_path, (prompt, warning, projected_prompt)) =
            with_isolated_plan(|plan| async move {
                let cfg = cfg.clone();
                plan.create(PLAN_MD)
                    .expect("create plan in isolated workspace");
                let (prompt, warning) =
                    load_system_prompt_with_plan_and_warning(&cfg, &plan).expect("prompt loads");
                // Wrapper-equivalent projection: what the deleted
                // `load_system_prompt_with_plan` returned was exactly this call with the
                // warning discarded, so re-projecting keeps that behaviour covered.
                let projected = load_system_prompt_with_plan_and_warning(&cfg, &plan)
                    .map(|(prompt, _warning)| prompt)
                    .expect("prompt loads");
                (prompt, warning, projected)
            });

        assert!(warning.is_none(), "a readable plan must not warn");
        assert!(
            prompt.contains("## Active Execution Plan"),
            "the plan block must be present:\n{prompt}"
        );
        assert!(
            !prompt.contains("UNKNOWN"),
            "a readable plan must not carry the UNKNOWN section"
        );

        // Byte-faithful against the bytes actually on disk: no re-spelling of
        // newlines, tabs, markdown or multi-byte text.
        let on_disk = std::fs::read_to_string(&plan_path).expect("plan file on disk");
        assert!(
            prompt.contains(on_disk.trim()),
            "the plan body must be embedded verbatim\n--- on disk ---\n{on_disk}\n--- prompt ---\n{prompt}"
        );
        assert!(
            prompt.contains("`åäö` ✅") && prompt.contains("\tindented\ttab + trailing spaces"),
            "multi-byte text and tabs must survive verbatim:\n{prompt}"
        );
        assert_eq!(
            projected_prompt, prompt,
            "the wrapper-equivalent projection must return the same prompt as the warning-carrying loader"
        );
    }

    /// Non-vacuity witness: the pre-fix shape (`if let Ok(Some(..))`) would have
    /// produced a prompt with **no** plan section at all for the unreadable plan
    /// above — i.e. exactly the fail-open behaviour this task repairs.
    #[test]
    fn test_load_system_prompt_fail_open_shape_would_have_been_silent() {
        let (_tmp, _plan_path, ()) = with_isolated_plan(|plan| async move {
            std::fs::write(plan.plan_path(), b"\xff\xfe\x00 not a plan").expect("write plan bytes");
            let swallowed: Option<String> = plan.read().ok().flatten();
            assert!(
                swallowed.is_none(),
                "the old `if let Ok(Some(..))` shape must be provably unable to see this plan"
            );
        });
    }
}
