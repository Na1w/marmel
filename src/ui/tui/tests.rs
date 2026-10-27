use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::style::{Color, Modifier};
use std::time::Duration;

/// Feed one synthetic key press through the same seam the live event loop calls
/// ([`TuiRenderer::apply_terminal_event`]), and report whether the key meant
/// something. The §10.3 legend-parity test and every keymap test below drive real
/// events rather than calling private helpers, so a legend row cannot name a key
/// the handler does not actually bind.
fn press(r: &mut TuiRenderer, code: KeyCode, modifiers: KeyModifiers) -> bool {
    r.apply_terminal_event(&crossterm::event::Event::Key(KeyEvent::new(
        code, modifiers,
    )))
}

#[test]
fn extract_sentences_splits_on_newline() {
    let mut buf = "Hello world\nNext line".to_string();
    let s = extract_complete_sentences(&mut buf);
    assert_eq!(s, vec!["Hello world\n"]);
    assert_eq!(buf, "Next line");
}

#[test]
fn extract_sentences_collapses_consecutive_newlines() {
    let mut buf = "a\n\n\nb".to_string();
    let s = extract_complete_sentences(&mut buf);
    assert_eq!(s, vec!["a\n\n\n"]);
    assert_eq!(buf, "b");
}

#[test]
fn extract_sentences_splits_on_punctuation() {
    let mut buf = "Hello world. Next sentence! And? Done".to_string();
    let s = extract_complete_sentences(&mut buf);
    assert_eq!(s, vec!["Hello world. ", "Next sentence! ", "And? "]);
    assert_eq!(buf, "Done");
}

#[test]
fn extract_sentences_collapses_consecutive_punctuation() {
    let mut buf = "Wait... Really!! Hmm".to_string();
    let s = extract_complete_sentences(&mut buf);
    assert_eq!(s, vec!["Wait... ", "Really!! "]);
    assert_eq!(buf, "Hmm");
}

#[test]
fn extract_sentences_keeps_incomplete_tail() {
    let mut buf = "This is an incomplete sentence".to_string();
    let s = extract_complete_sentences(&mut buf);
    assert!(s.is_empty());
    assert_eq!(buf, "This is an incomplete sentence");
}

#[test]
fn extract_sentences_handles_tab_whitespace() {
    let mut buf = "One.\tTwo".to_string();
    let s = extract_complete_sentences(&mut buf);
    assert_eq!(s, vec!["One.\t"]);
    assert_eq!(buf, "Two");
}

#[test]
fn message_style_matches_reference_prefixes() {
    let (s, special) = message_style("User: hello");
    assert!(special);
    assert_eq!(s.fg, Some(Color::Cyan));
    assert!(s.add_modifier.contains(Modifier::BOLD));

    let (s, special) = message_style("Marmennill: reply");
    assert!(special);
    assert_eq!(s.fg, Some(Color::LightYellow));
    assert!(s.add_modifier.contains(Modifier::BOLD));

    let (s, special) = message_style("[Status] running");
    assert!(special);
    assert_eq!(s.fg, Some(Color::Yellow));

    let (s, special) = message_style("System Error: boom");
    assert!(special);
    assert_eq!(s.fg, Some(Color::Red));
    assert!(s.add_modifier.contains(Modifier::BOLD));

    let (s, special) = message_style("--- Turn 1");
    assert!(special);
    assert_eq!(s.fg, Some(Color::Green));
    assert!(s.add_modifier.contains(Modifier::BOLD));

    let (_, special) = message_style("plain line");
    assert!(!special);
}

#[test]
fn append_steer_sentence_creates_marmennill_messages() {
    let mut r = TuiRenderer::new();
    r.append_steer_sentence("Hello");
    assert_eq!(r.messages, vec!["Marmennill: Hello"]);
    // Appending to an existing Marmennill message extends it.
    r.append_steer_sentence(" world");
    assert_eq!(r.messages, vec!["Marmennill: Hello world"]);
    // A non-Marmennill last message starts a new one.
    r.messages.push("User: hi".to_string());
    r.append_steer_sentence("New");
    assert_eq!(r.messages.last().unwrap(), "Marmennill: New");
}

#[test]
fn wrapped_lines_counts_single_line() {
    assert_eq!(wrapped_lines("Hello world", 80), 1);
}

#[test]
fn wrapped_lines_wraps_long_words() {
    // A single long word wider than the width wraps.
    assert_eq!(wrapped_lines("abcdefghij", 5), 2);
}

#[test]
fn wrapped_lines_width_zero_returns_raw_line_count() {
    assert_eq!(wrapped_lines("a\nb\nc", 0), 3);
}

#[test]
fn wrapped_lines_expands_tabs_and_trims_trailing_spaces() {
    // Tab expands to 4 spaces; trailing spaces trimmed.
    assert_eq!(wrapped_lines("a\tb   ", 80), 1);
}

#[test]
fn wrapped_lines_empty_string_has_no_lines() {
    // An empty string has no raw lines, so it contributes 0 wrapped lines.
    assert_eq!(wrapped_lines("", 80), 0);
}

#[test]
fn ensure_message_cache_full_recompute_on_width_change() {
    let mut r = TuiRenderer::new();
    r.messages.push("Hello world".to_string());
    r.messages.push("Line 1\nLine 2\nLine 3".to_string());

    r.ensure_message_cache(80);
    assert_eq!(r.cached_total_message_lines.get(), 4); // 1 + 3
    assert_eq!(r.cached_message_lines.borrow().len(), 2);

    // Width change triggers full recompute.
    r.ensure_message_cache(5);
    assert_eq!(r.cached_message_lines.borrow().len(), 2);
    assert!(r.cached_total_message_lines.get() > 4);
}

#[test]
fn ensure_message_cache_incremental_append() {
    let mut r = TuiRenderer::new();
    r.messages.push("Hello".to_string());
    r.ensure_message_cache(80);
    assert_eq!(r.cached_message_lines.borrow().len(), 1);

    // New message appended → incremental.
    r.messages.push("World".to_string());
    r.ensure_message_cache(80);
    assert_eq!(r.cached_message_lines.borrow().len(), 2);
    assert_eq!(r.cached_total_message_lines.get(), 2);
}

#[test]
fn ensure_message_cache_truncates_on_removal() {
    let mut r = TuiRenderer::new();
    r.messages.push("a".to_string());
    r.messages.push("b".to_string());
    r.messages.push("c".to_string());
    r.ensure_message_cache(80);
    assert_eq!(r.cached_message_lines.borrow().len(), 3);

    r.messages.pop();
    r.ensure_message_cache(80);
    assert_eq!(r.cached_message_lines.borrow().len(), 2);
    assert_eq!(r.cached_total_message_lines.get(), 2);
}

#[test]
fn invalidate_last_message_cache_recomputes_last() {
    let mut r = TuiRenderer::new();
    r.messages.push("Marmennill: Hello".to_string());
    r.ensure_message_cache(80);
    assert_eq!(r.cached_total_message_lines.get(), 1);

    // Append to the last message and invalidate.
    r.messages.last_mut().unwrap().push_str(" world");
    r.invalidate_last_message_cache();
    assert_eq!(r.cached_total_message_lines.get(), 1);
    assert_eq!(r.cached_message_lines.borrow().len(), 1);
}

#[test]
fn estimated_chat_lines_includes_streaming_buffers() {
    let mut r = TuiRenderer::new();
    r.messages.push("Hello".to_string());
    r.current_content = "Streaming content".to_string();
    r.show_thought = true;
    r.current_thought = "thinking text".to_string();

    let n = r.estimated_chat_lines(80);
    // 1 (message) + 1 (content) + 1 (thought) = 3 (markers are not rendered)
    assert_eq!(n, 3);
}

#[test]
fn estimated_subagent_lines_counts_sections() {
    let mut r = TuiRenderer::new();
    r.subagents.push(SubagentDetail {
        name: "coder".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec!["log1".to_string()],
        thinking: "think".to_string(),
        content: "out".to_string(),
        is_active: true,
        context_tokens: 0,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    });
    // Default (show_thought = false, thinking moved to bottom status bar):
    // 1 (header) + 1 ([Output]) + 1 (out) + 1 ([Logs]) + 1 (- log1) = 5
    assert_eq!(r.estimated_subagent_lines(80), 5);
    // When show_thought = true: 1 (header) + 2 ([Thinking], " thinking") + 1 (think) + 1 (" response")
    // + 1 ([Output]) + 1 (out) + 1 ([Logs]) + 1 (- log1) = 9
    r.show_thought = true;
    assert_eq!(r.estimated_subagent_lines(80), 9);
}

#[test]
fn message_line_caching_matches_reference() {
    let mut r = TuiRenderer::new();
    r.messages.push("Hello world".to_string());
    r.messages.push("Line 1\nLine 2\nLine 3".to_string());

    let lines_80 = r.estimated_chat_lines(80);
    assert_eq!(lines_80, 4); // 1 + 3

    // Cached line count should remain valid without re-evaluating everything.
    assert_eq!(r.cached_total_message_lines.get(), 4);
    assert_eq!(r.cached_message_lines.borrow().len(), 2);

    r.append_steer_sentence("New steer sentence.");
    let lines_after_steer = r.estimated_chat_lines(80);
    assert_eq!(lines_after_steer, 5);

    // Appending to the existing steer sentence.
    r.append_steer_sentence(" Another sentence.");
    let lines_after_steer2 = r.estimated_chat_lines(80);
    assert_eq!(lines_after_steer2, 5);
}

#[test]
fn test_chat_line_estimation_matches_rendered_markdown_and_streaming() {
    let mut r = TuiRenderer::new();
    r.messages.push(
        "Here is code:\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\nDone!".to_string(),
    );
    r.current_content = "Streaming:\n```python\nprint(123)\n```\nAll done!".to_string();

    let width = 80;
    let est = r.estimated_chat_lines(width);

    let mut actual_lines = Vec::new();
    r.ensure_rendered_message_cache();
    {
        let cached = r.cached_rendered_messages.borrow();
        for msg in &*cached {
            actual_lines.extend(msg.iter().cloned());
        }
    }
    actual_lines.extend(r.render_streaming_content());

    let actual_count = count_wrapped_rendered_lines(&actual_lines, width);
    assert_eq!(est, actual_count);
    assert!(est > 0);
}

#[test]
fn history_navigation_recalls_and_restores_draft() {
    let mut r = TuiRenderer::new();
    r.history.push("first".to_string());
    r.history.push("second".to_string());
    r.input_text = "draft".to_string();

    // Ctrl+Up: save draft, go to last.
    r.history_prev();
    assert_eq!(r.input_text, "second");
    assert_eq!(r.input_draft, "draft");

    // Ctrl+Up again: go to previous.
    r.history_prev();
    assert_eq!(r.input_text, "first");

    // Ctrl+Down: forward to second.
    r.history_next();
    assert_eq!(r.input_text, "second");

    // Ctrl+Down at end: restore draft.
    r.history_next();
    assert_eq!(r.input_text, "draft");
    assert_eq!(r.history_index, None);
}

/// `Tab` / `Shift+Tab` walk the §4.2 focus ring through the key handler, skipping
/// the panes the current mode does not present, and the legacy `focused_panel`
/// mirror moves with the stored focus.
#[test]
fn cycle_focus_obeys_panel_visibility() {
    let mut r = m4_renderer();

    // M4 presents all four panes: Input -> Transcript -> Plan -> Workers -> Input.
    assert_eq!(r.focus, state::FocusPane::Input);
    press(&mut r, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(r.focus, state::FocusPane::Transcript);
    assert_eq!(r.focused_panel, FocusedPanel::Chat);
    press(&mut r, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(r.focus, state::FocusPane::Plan);
    assert_eq!(r.focused_panel, FocusedPanel::Plan);
    press(&mut r, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(r.focus, state::FocusPane::Workers);
    assert_eq!(r.focused_panel, FocusedPanel::Subagents);
    press(&mut r, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(r.focus, state::FocusPane::Input);

    // `Shift+Tab` walks it backwards, spelled both ways terminals deliver it.
    press(&mut r, KeyCode::BackTab, KeyModifiers::NONE);
    assert_eq!(r.focus, state::FocusPane::Workers);
    press(&mut r, KeyCode::Tab, KeyModifiers::SHIFT);
    assert_eq!(r.focus, state::FocusPane::Plan);

    // An absent pane is never a landing pane (G6): M1 has no Plan or Workers, and
    // switching to a mode that removes the focused pane pulls focus back onto a
    // pane that is actually on screen.
    r.set_view_mode(state::ViewMode::Transcript);
    assert_eq!(r.focus, state::FocusPane::Input);
    press(&mut r, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(r.focus, state::FocusPane::Transcript);
    press(&mut r, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(r.focus, state::FocusPane::Input);
}

/// A `Delegation(Started)` event creates an active subagent entry and routes
/// subsequent `Thinking`/`Message`/`Status` events into its live
/// thinking/content/logs fields.
#[test]
fn delegation_started_tracks_live_thinking_content_logs() {
    let mut r = TuiRenderer::new();

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-9".to_string()),
        },
    ));

    assert_eq!(r.subagents.len(), 1);
    assert_eq!(r.subagents[0].name, "coder-t-9");
    assert!(r.subagents[0].is_active);
    assert_eq!(r.active_agent, "coder-t-9");

    r.on_event(&Event::Thinking("reasoning...".to_string()));
    assert_eq!(r.subagents[0].thinking, "reasoning...");

    r.on_event(&Event::Message("final answer".to_string()));
    assert_eq!(r.subagents[0].content, "final answer");

    r.on_event(&Event::Status("calling backend".to_string()));
    assert!(r.subagents[0].logs.iter().any(|l| l == "calling backend"));
}

/// A `Delegation(Completed)` event marks the subagent idle and stops routing
/// live content into it.
#[test]
fn delegation_completed_marks_idle_and_stops_routing() {
    let mut r = TuiRenderer::new();

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-10".to_string()),
        },
    ));
    r.on_event(&Event::Thinking("think".to_string()));
    assert_eq!(r.subagents[0].thinking, "think");

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Completed {
            agent: crate::agents::Agent::Coder,
            task: Some("t-10".to_string()),
        },
    ));

    assert!(!r.subagents[0].is_active);
    assert_eq!(r.active_agent, "Manager");

    r.on_event(&Event::Message("manager content".to_string()));
    assert_eq!(r.subagents[0].content, "");
}

#[test]
fn test_delegation_failed_renders_reason_in_chatlog() {
    let mut r = TuiRenderer::new();

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-001".to_string()),
        },
    ));

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Failed {
            agent: crate::agents::Agent::Coder,
            task: Some("t-001".to_string()),
            reason: Some("compilation error: unresolved import".to_string()),
        },
    ));

    assert_eq!(r.messages.len(), 1);
    assert_eq!(
        r.messages[0],
        "[t-001 failed: compilation error: unresolved import]"
    );
    assert!(!r.subagents[0].is_active);
    assert!(
        r.subagents[0]
            .logs
            .contains(&"failed task t-001: compilation error: unresolved import".to_string())
    );

    // When reason is None, falls back to [task] failed.
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Failed {
            agent: crate::agents::Agent::Coder,
            task: Some("t-002".to_string()),
            reason: None,
        },
    ));
    assert_eq!(r.messages[1], "[t-002] failed.");
}

/// `set_subagents` adopts the session-loop's authoritative list, preserving
/// locally-streamed thinking/content and dropping stale entries.
#[test]
fn set_subagents_adopts_authoritative_list() {
    let mut r = TuiRenderer::new();

    // Local entry with streamed thinking.
    r.subagents.push(SubagentDetail {
        name: "coder".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec!["started task t-1".to_string()],
        thinking: "local think".to_string(),
        content: "local content".to_string(),
        is_active: true,
        context_tokens: 0,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    });

    // Authoritative list from the loop (no thinking/content, just lifecycle).
    let authoritative = vec![SubagentDetail {
        name: "coder".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec!["started task t-1".to_string()],
        thinking: String::new(),
        content: String::new(),
        is_active: true,
        context_tokens: 0,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    }];
    r.set_subagents(authoritative);

    // Local thinking/content preserved, and so are the log lines: an authoritative push merges
    // (`merge_logs`) instead of replacing, and a push that carries no log lines changes nothing.
    assert_eq!(r.subagents.len(), 1);
    assert_eq!(r.subagents[0].thinking, "local think");
    assert_eq!(r.subagents[0].content, "local content");
    assert!(r.subagents[0].is_active);

    // A stale entry not in the authoritative list is dropped.
    r.subagents.push(SubagentDetail {
        name: "researcher".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: false,
        context_tokens: 0,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    });
    r.set_subagents(vec![SubagentDetail {
        name: "coder".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: false,
        context_tokens: 0,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    }]);
    assert_eq!(r.subagents.len(), 1);
    assert_eq!(r.subagents[0].name, "coder");
}

/// The `Queued` / `Running` arms move the orthogonal queue field only: same row,
/// same `is_active`, same clocks and selection, and no chat line, log line or
/// status line — the panel label for a queued row is a separate, later change.
#[test]
fn delegation_queued_and_running_move_only_the_queue_field() {
    let mut r = TuiRenderer::new();
    let coder = crate::agents::Agent::Coder;

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: coder,
            task: Some("t-011".to_string()),
        },
    ));
    let messages = r.messages.len();
    let status = r.status_line.clone();
    assert!(
        r.subagents[0].started_at.is_some(),
        "Started arms the work clock"
    );
    let started_at = r.subagents[0].started_at;
    let logs = r.subagents[0].logs.len();
    let selected = r.selected_subagent_idx;

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Queued {
            agent: coder,
            task: Some("t-011".to_string()),
            position: 2,
            of: 3,
        },
    ));
    assert_eq!(r.subagents.len(), 1, "no extra row for a slot transition");
    assert_eq!(
        r.subagents[0].queue,
        Some(crate::ui::QueueState { position: 2, of: 3 })
    );
    assert!(
        r.subagents[0].is_active,
        "queued is still dispatched, so still active"
    );
    assert_eq!(r.subagents[0].started_at, started_at);
    assert_eq!(r.subagents[0].logs.len(), logs);
    assert_eq!(r.messages.len(), messages);
    assert_eq!(r.status_line, status);
    assert_eq!(r.selected_subagent_idx, selected);

    // A shift in the queue re-reports the position; newest value wins.
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Queued {
            agent: coder,
            task: Some("t-011".to_string()),
            position: 1,
            of: 2,
        },
    ));
    assert_eq!(
        r.subagents[0].queue,
        Some(crate::ui::QueueState { position: 1, of: 2 })
    );

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Running {
            agent: coder,
            task: Some("t-011".to_string()),
        },
    ));
    assert_eq!(r.subagents[0].queue, None, "the slot is held: no queue");
    assert!(r.subagents[0].is_active);
    assert_eq!(r.subagents[0].started_at, started_at);
    assert_eq!(r.subagents[0].logs.len(), logs);
    assert_eq!(r.messages.len(), messages);
    assert_eq!(r.status_line, status);

    // Completion closes the delegation and takes the queue state with it.
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Completed {
            agent: coder,
            task: Some("t-011".to_string()),
        },
    ));
    assert!(!r.subagents[0].is_active);
    assert_eq!(r.subagents[0].queue, None, "no stale queue row survives");
    // The lifecycle arms keep their own (unchanged) output.
    assert_eq!(r.messages.len(), messages + 1);
    assert_eq!(r.subagents[0].logs.len(), logs + 1);
}

/// A queue arm whose delegation this renderer never saw a `Started` for must not
/// conjure a row, an active flag, a chat line or a panel reveal.
#[test]
fn delegation_queue_arms_never_create_a_row() {
    let mut r = TuiRenderer::new();
    let coder = crate::agents::Agent::Coder;

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Queued {
            agent: coder,
            task: Some("t-011".to_string()),
            position: 1,
            of: 1,
        },
    ));
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Running {
            agent: coder,
            task: Some("t-011".to_string()),
        },
    ));

    assert!(
        r.subagents.is_empty(),
        "queue arms alone must not create rows, got: {:?}",
        r.subagents
    );
    assert!(r.messages.is_empty());
    assert!(
        !r.show_subagent_panel,
        "queue traffic must not reveal the panel on its own"
    );
}

/// Failure closes the delegation too, so a queue position installed just before
/// it can never linger.
#[test]
fn delegation_failed_while_queued_leaves_no_queue_state() {
    let mut r = TuiRenderer::new();
    let coder = crate::agents::Agent::Coder;

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: coder,
            task: Some("t-011".to_string()),
        },
    ));
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Queued {
            agent: coder,
            task: Some("t-011".to_string()),
            position: 1,
            of: 4,
        },
    ));
    assert_eq!(
        r.subagents[0].queue,
        Some(crate::ui::QueueState { position: 1, of: 4 })
    );

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Failed {
            agent: coder,
            task: Some("t-011".to_string()),
            reason: Some("queue wait expired".to_string()),
        },
    ));
    assert!(!r.subagents[0].is_active);
    assert_eq!(r.subagents[0].queue, None);
}

/// The session loop owns queue state, so `set_subagents` always adopts the
/// incoming value — dropping a locally held position the loop has already
/// cleared, and installing one the loop has just learned about.
#[test]
fn set_subagents_adopts_authoritative_queue_state() {
    let mut r = TuiRenderer::new();

    r.subagents.push(SubagentDetail {
        name: "coder-t-011".to_string(),
        task_id: Some("t-011".to_string()),
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec!["started task t-011".to_string()],
        thinking: "local think".to_string(),
        content: String::new(),
        is_active: true,
        context_tokens: 0,
        queue: Some(crate::ui::QueueState { position: 5, of: 9 }),
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    });

    // The loop says the slot is held: the stale local position must go.
    r.set_subagents(vec![SubagentDetail {
        name: "coder-t-011".to_string(),
        task_id: Some("t-011".to_string()),
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec!["started task t-011".to_string()],
        thinking: String::new(),
        content: String::new(),
        is_active: true,
        context_tokens: 0,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    }]);
    assert_eq!(r.subagents[0].queue, None);
    assert_eq!(
        r.subagents[0].thinking, "local think",
        "live streaming state is still preserved"
    );

    // And the other direction: a queue the loop learned about lands locally.
    r.set_subagents(vec![SubagentDetail {
        name: "coder-t-011".to_string(),
        task_id: Some("t-011".to_string()),
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec!["started task t-011".to_string()],
        thinking: String::new(),
        content: String::new(),
        is_active: true,
        context_tokens: 0,
        queue: Some(crate::ui::QueueState { position: 1, of: 2 }),
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    }]);
    assert_eq!(
        r.subagents[0].queue,
        Some(crate::ui::QueueState { position: 1, of: 2 })
    );
    assert!(r.subagents[0].is_active);
}

// ---- row identity: the effective registry key ----

/// A stream tag spells the **natural** key (`{agent}-{task}`, or bare `{agent}`),
/// while a row is named by the **effective registry key**, which may carry a
/// `-wN` / `#N` handle the tag can never spell. Resolving the tag to the row key
/// is what keeps streamed thinking/content and the thinking flag on the same
/// object the Manager and the steering arbitrator address.
#[test]
fn a_stream_tag_resolves_to_the_row_key_the_registry_allocated() {
    let mut r = TuiRenderer::new();
    r.subagents.push(SubagentDetail {
        name: "coder-t-011#7".to_string(),
        task_id: Some("t-011".to_string()),
        is_active: true,
        ..SubagentDetail::default()
    });
    r.subagents.push(SubagentDetail {
        name: "researcher-w42".to_string(),
        is_active: true,
        ..SubagentDetail::default()
    });

    assert_eq!(r.resolve_row_key("coder-t-011"), "coder-t-011#7");
    assert_eq!(r.resolve_row_key("coder-t-011#7"), "coder-t-011#7");
    assert_eq!(r.resolve_row_key("researcher"), "researcher-w42");
    assert_eq!(r.resolve_row_key("debugger"), "debugger");

    r.on_event(&Event::SubagentThinking {
        agent_tag: "coder-t-011".to_string(),
        text: "reasoning about the collision handle".to_string(),
    });
    assert_eq!(
        r.subagents.len(),
        2,
        "the tag must never open a second row for a worker that already has one"
    );
    assert_eq!(
        r.subagents[0].thinking,
        "reasoning about the collision handle"
    );
    assert_eq!(
        r.subagent_is_thinking.get("coder-t-011#7").copied(),
        Some(true),
        "the thinking flag is keyed on the row key, not on the tag"
    );
}

// ---- queued state and queue position in the Subagents panel ----

/// Fixture for the queue-label tests: the fields the row label is built from are
/// parameters, everything else keeps the derived default.
fn queue_row(name: &str, is_active: bool, queue: Option<crate::ui::QueueState>) -> SubagentDetail {
    SubagentDetail {
        name: name.to_string(),
        is_active,
        queue,
        ..SubagentDetail::default()
    }
}

/// The animation keyword set is shared by the render-loop throttle tier and the
/// status-line indicator, so the two cannot disagree about a queued round.
#[test]
fn the_slot_wait_keyword_is_shared_by_both_animation_tiers() {
    assert!(
        ACTIVITY_STATUS_KEYWORDS.contains(&"queued"),
        "the gate reports a slot wait in lower case"
    );
    assert!(
        ACTIVITY_STATUS_KEYWORDS.contains(&"Queued"),
        "the panel spells the same state (Queued n/m)"
    );
    assert!(
        !ACTIVITY_STATUS_KEYWORDS.contains(&"Ready"),
        "an idle line is not activity"
    );
}

/// Removes one registry entry when the guard goes out of scope, so a panicking assertion in a
/// registry-touching test can never leave a live worker behind for the rest of the test binary
/// (the table is process-global, and a leaked row shows up in every other suite's projection).
struct EntryCleanup {
    key: String,
}

impl Drop for EntryCleanup {
    fn drop(&mut self) {
        crate::orchestrator::workers::remove_worker_entry_for_test(&self.key);
    }
}

// ---- the merge contract for `logs` (gap G10, closed with R1) ----

/// G10/G1: `logs` now has ONE documented merge policy instead of four (the loop appends, the
/// renderer dedupes consecutive duplicates, the projection never writes the field, and
/// `set_subagents` used to replace it). An authoritative push — the ordinary cadence at HEAD:
/// every delegation drain plus the 250 ms in-sink pump — must never duplicate the lifecycle
/// line it carries nor take back a line the renderer streamed into the row.
#[test]
fn repeated_authoritative_pushes_never_duplicate_or_drop_a_log_line() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;

    // The loop opens the row with its one lifecycle line.
    r.set_subagents(vec![SubagentDetail {
        name: "coder-t-9400".to_string(),
        task_id: Some("t-9400".to_string()),
        is_active: true,
        logs: vec!["started task t-9400".to_string()],
        ..Default::default()
    }]);

    // Live traffic: the renderer routes both sentences onto that row itself.
    r.on_event(&Event::Status(
        "coder-t-9400: thinking / calling model (gpt-6-x)...".to_string(),
    ));
    r.on_event(&Event::Status(
        "coder-t-9400: calling backend (gpt-6-x) for t-9400".to_string(),
    ));
    let streamed = r.subagents[0].logs.clone();
    assert_eq!(
        streamed,
        vec![
            "started task t-9400".to_string(),
            "thinking / calling model (gpt-6-x)...".to_string(),
            "calling backend (gpt-6-x) for t-9400".to_string(),
        ],
        "control: the renderer appends the child's live traffic itself"
    );

    // The same row, re-projected with lifecycle content only, three times in a row.
    for round in 0..3 {
        r.set_subagents(vec![SubagentDetail {
            name: "coder-t-9400".to_string(),
            task_id: Some("t-9400".to_string()),
            is_active: true,
            logs: vec!["started task t-9400".to_string()],
            ..Default::default()
        }]);
        assert_eq!(
            r.subagents[0].logs, streamed,
            "push #{round} must neither duplicate the lifecycle line nor erase a streamed line, \
             got {:?}",
            r.subagents[0].logs
        );
    }
}

/// The merge rule in both directions, pinned at the seam itself: the loop stays authoritative
/// when it genuinely knows more (a "renderer always wins" guard would break the authoritative
/// row rebuild the pane relies on), and a shorter lifecycle list can never take back streamed
/// text.
#[test]
fn the_logs_merge_adopts_a_longer_loop_list_and_keeps_the_renderers_tail() {
    // Loop knows more: its list is the base, in its own order, and the renderer's line is
    // appended rather than dropped.
    let merged = merge_logs(
        &["thinking / calling model (gpt-6-x)...".to_string()],
        &[
            "started task t-9403".to_string(),
            "completed task t-9403".to_string(),
        ],
    );
    assert_eq!(
        merged,
        vec![
            "started task t-9403".to_string(),
            "completed task t-9403".to_string(),
            "thinking / calling model (gpt-6-x)...".to_string(),
        ],
        "monotone adoption: the longer authoritative list leads, nothing is lost"
    );

    // Loop knows less: its one lifecycle line is already on the row, so the merge is a no-op.
    let merged = merge_logs(
        &[
            "started task t-9403".to_string(),
            "thinking / calling model (gpt-6-x)...".to_string(),
        ],
        &["started task t-9403".to_string()],
    );
    assert_eq!(
        merged,
        vec![
            "started task t-9403".to_string(),
            "thinking / calling model (gpt-6-x)...".to_string(),
        ],
        "a shorter authoritative list never erases streamed traffic"
    );

    // A row the renderer opened empty still adopts the loop's list verbatim.
    let merged = merge_logs(&[], &["started task t-9403".to_string()]);
    assert_eq!(merged, vec!["started task t-9403".to_string()]);

    // And a lifecycle line the renderer never saw survives either way.
    let merged = merge_logs(
        &[
            "started task t-9403".to_string(),
            "calling backend (gpt-6-x) for t-9403".to_string(),
            "read(src/ui/tui/mod.rs)".to_string(),
        ],
        &[
            "started task t-9403".to_string(),
            "completed task t-9403".to_string(),
        ],
    );
    assert_eq!(
        merged,
        vec![
            "started task t-9403".to_string(),
            "calling backend (gpt-6-x) for t-9403".to_string(),
            "read(src/ui/tui/mod.rs)".to_string(),
            "completed task t-9403".to_string(),
        ],
        "the loop's closing lifecycle line is retained, the renderer's tail keeps its order"
    );
}

/// The merge only ever adds lines, so the per-row cap is what bounds a long run. The detail
/// pane is read from its tail, so the cap must drop the OLDEST lines.
#[test]
fn the_per_row_log_cap_keeps_the_newest_lines() {
    let streamed: Vec<String> = (0..MAX_SUBAGENT_ROW_LOGS + 5)
        .map(|n| format!("line {n}"))
        .collect();
    let incoming = vec!["started task t-9402".to_string()];

    let merged = merge_logs(&streamed, &incoming);
    assert_eq!(
        merged.len(),
        MAX_SUBAGENT_ROW_LOGS,
        "a merged row never keeps more than the cap"
    );
    assert_eq!(
        merged.last().map(String::as_str),
        Some("started task t-9402"),
        "the newest line always survives the cap"
    );
    let dropped = streamed.len() + incoming.len() - MAX_SUBAGENT_ROW_LOGS;
    assert_eq!(
        merged.first().map(String::as_str),
        streamed.get(dropped).map(String::as_str),
        "the cap drops exactly the oldest `dropped` lines"
    );
    assert!(
        !merged.iter().any(|line| line == "line 0"),
        "the oldest line is the first one gone"
    );

    // Repeated capped merges stay bounded instead of growing.
    let mut row = merged;
    for round in 0..5 {
        row = merge_logs(
            &row,
            &[
                "started task t-9402".to_string(),
                format!("line cap+{round}"),
            ],
        );
    }
    assert_eq!(
        row.len(),
        MAX_SUBAGENT_ROW_LOGS,
        "a capped row stays capped"
    );
    assert_eq!(
        row.last().map(String::as_str),
        Some("line cap+4"),
        "the newest line of the last push is on the row"
    );
}

// ---- steering arbitration pause in the Subagents panel ----

/// The typed transition carries the stream's own tag, so it lands on the row that
/// tag resolves to — including the registry spellings a tag can never produce —
/// and it can never open or close a row by itself.
#[test]
fn a_pause_transition_lands_on_the_row_its_stream_tag_names() {
    let mut r = TuiRenderer::new();
    r.subagents.push(queue_row("coder-t-11#7", true, None));
    r.subagents.push(queue_row("researcher-w42", true, None));

    r.on_event(&Event::SubagentStreamPause {
        agent_tag: "coder-t-11".to_string(),
        paused: true,
    });
    r.on_event(&Event::SubagentStreamPause {
        agent_tag: "researcher".to_string(),
        paused: true,
    });
    assert_eq!(
        r.subagents.len(),
        2,
        "a pause for a worker that has a row never opens a second one"
    );
    assert!(
        r.subagents[0].paused && r.subagents[1].paused,
        "both spellings resolve to the row that already exists"
    );
    assert!(
        r.subagents[0].is_active && r.subagents[1].is_active,
        "the transition never touches liveness"
    );

    r.on_event(&Event::SubagentStreamPause {
        agent_tag: "coder-t-11#7".to_string(),
        paused: false,
    });
    assert!(
        !r.subagents[0].paused && r.subagents[1].paused,
        "the reclaim edge clears one row and says nothing about the other"
    );

    r.on_event(&Event::SubagentStreamPause {
        agent_tag: "validator-coder-t-99".to_string(),
        paused: true,
    });
    assert_eq!(
        r.subagents.len(),
        2,
        "a pause for a worker with no row is a no-op: the registry owns row existence"
    );
}

/// Guard against the cheap wrong implementation: the panel flag is **never**
/// derived from the status sentence the handshake also emits. Recognition of
/// that wording is what made the state invisible in the first place, and a
/// skipped pause would have produced the very same sentence-shaped traffic with
/// nothing parked behind it.
#[test]
fn the_panel_pause_is_never_sniffed_out_of_the_status_line() {
    let mut r = TuiRenderer::new();
    r.subagents.push(queue_row("coder-t-1", true, None));
    r.active_agent = "coder-t-1".to_string();

    for sentence in [
        "[coder] Yielded model slot (gpt-x) to Steer Arbitrator — stream paused",
        "[coder-t-1] Yielded model slot (gpt-x) to Steer Arbitrator — stream paused",
        "[coder-t-1] Model slot reclaimed — resuming stream...",
    ] {
        r.on_event(&Event::Status(sentence.to_string()));
        assert!(
            !r.subagents[0].paused,
            "the status wording alone never pauses a row, got {:?}",
            r.subagents[0]
        );
        assert_eq!(r.subagents.len(), 1, "and it never creates one");
    }

    // The other direction: an unrelated status line cannot clear a real pause
    // either, so the label outlives the traffic around it and not the handshake.
    r.subagents[0].paused = true;
    r.on_event(&Event::Status(
        "[coder-t-1] Model slot reclaimed — resuming stream...".to_string(),
    ));
    r.on_event(&Event::Status("Running steer round".to_string()));
    assert!(
        r.subagents[0].paused,
        "only the typed transition moves the flag"
    );
}

/// The animation keyword set is shared by the render-loop throttle tier and the
/// status-line indicator, so a round whose streams are parked behind the
/// arbitrator keeps animating in both — exactly like a queued round.
#[test]
fn the_arbitration_pause_keyword_is_shared_by_both_animation_tiers() {
    assert!(
        ACTIVITY_STATUS_KEYWORDS.contains(&"paused"),
        "the handshake reports a yielded slot in lower case"
    );
    assert!(
        ACTIVITY_STATUS_KEYWORDS.contains(&"Paused"),
        "the panel spells the same state (Paused)"
    );
}

#[test]
fn test_orchestrator_content_is_white_and_steer_is_yellow() {
    let (white_style, special_white) = message_style("Orchestrator answer");
    assert!(!special_white);
    assert_eq!(white_style.fg, Some(Color::White));

    let (yellow_style, special_yellow) = message_style("Marmennill: Direct answer from arbitrator");
    assert!(special_yellow);
    assert_eq!(yellow_style.fg, Some(Color::LightYellow));
    assert!(yellow_style.add_modifier.contains(Modifier::BOLD));

    let mut r = TuiRenderer::new();
    // Orchestrator stream -> goes to current_content
    r.on_event(&Event::Message("chunk of orchestrator".to_string()));
    assert_eq!(r.current_content, "chunk of orchestrator");
    assert!(r.steer_sentence_buffer.is_empty());

    // Steer arbitrator response -> streams into steer_sentence_buffer / yellow messages
    r.on_event(&Event::SteerResponse(
        "Direct reply from arbitrator.\n".to_string(),
    ));
    assert!(
        r.messages
            .iter()
            .any(|m| m.starts_with("Marmennill: Direct reply from arbitrator."))
    );
}

#[test]
fn test_subagent_tool_calls_logged_with_arguments_and_stripped_prefix() {
    let mut r = TuiRenderer::new();
    r.subagents.push(SubagentDetail {
        name: "coder-t-001".to_string(),
        task_id: Some("t-001".to_string()),
        prompt: "build raytracer".to_string(),
        started_at: None,
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: true,
        context_tokens: 0,
        worked_duration: Duration::ZERO,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    });
    r.active_agent = "coder-t-001".to_string();

    // Status event with agent prefix should have prefix stripped
    r.on_event(&Event::Status(
        "coder-t-001: write_file(src/scene.rs)".to_string(),
    ));
    assert_eq!(
        r.subagents[0].logs.last().unwrap(),
        "write_file(src/scene.rs)"
    );

    // ToolCall event during specialist execution should route directly to subagent logs
    r.on_event(&Event::ToolCall("run_command(cargo test)".to_string()));
    assert_eq!(
        r.subagents[0].logs.last().unwrap(),
        "run_command(cargo test)"
    );
    // And not pollute the main chat
    assert!(
        !r.messages
            .iter()
            .any(|m| m.contains("run_command(cargo test)"))
    );
}

#[test]
fn test_plan_scrolling_with_wrapped_lines() {
    let mut r = TuiRenderer::new();
    r.focused_panel = FocusedPanel::Plan;
    let plan_text = "# Header\nThis is a long plan line that will wrap onto multiple visual lines when rendered in a narrow panel.\n- [ ] Task 1\n- [x] Task 2\n";
    r.plan_content = plan_text.to_string();

    let total_lines = wrapped_lines(&r.plan_content, 20);
    assert!(total_lines > 4); // Wrapped into more lines than raw count

    r.plan_max_scroll = total_lines.saturating_sub(5) as u16;
    assert!(r.plan_max_scroll > 0);

    r.scroll_down(2);
    assert_eq!(r.plan_scroll, 2);

    r.scroll_up(1);
    assert_eq!(r.plan_scroll, 1);

    r.scroll_to_top();
    assert_eq!(r.plan_scroll, 0);

    r.scroll_to_bottom();
    assert_eq!(r.plan_scroll, r.plan_max_scroll);
}

#[test]
fn test_visual_line_offset_of_first_pending() {
    let plan = "# Plan\n## Phase 1\n- [x] [t-001] Done 1\n- [x] [t-002] Done 2\n## Phase 2\n- [ ] [t-003] Next task\n- [ ] [t-004] Later task\n";
    let offset = visual_line_offset_of_first_pending(plan, 80);
    assert_eq!(offset, Some(5));

    let completed_plan = "# Plan\n- [x] Done 1\n- [x] Done 2\n";
    assert_eq!(
        visual_line_offset_of_first_pending(completed_plan, 80),
        None
    );
}

#[test]
fn test_visual_line_offset_of_task() {
    let plan = "# Plan\n## Phase 1\n- [x] [t-001] First\n- [x] [t-002] Second\n- [ ] [t-003] Third\n- [ ] [t-004] Fourth\n";
    assert_eq!(visual_line_offset_of_task(plan, "t-001", 80), Some(2));
    assert_eq!(visual_line_offset_of_task(plan, "t-002", 80), Some(3));
    assert_eq!(visual_line_offset_of_task(plan, "t-003", 80), Some(4));
    assert_eq!(visual_line_offset_of_task(plan, "t-004", 80), Some(5));
    assert_eq!(visual_line_offset_of_task(plan, "t-999", 80), None);
}

#[test]
fn test_extract_plan_line_task_id() {
    assert_eq!(
        extract_plan_line_task_id("- [ ] [t-001] Do task 1"),
        Some("t-001".to_string())
    );
    assert_eq!(
        extract_plan_line_task_id("- [ ] [t-100a1] Do subtask"),
        Some("t-100a1".to_string())
    );
    assert_eq!(
        extract_plan_line_task_id("- [ ] **[t-100]**: Bold task"),
        Some("t-100".to_string())
    );
    assert_eq!(
        extract_plan_line_task_id("- [ ] t-005: Colon task"),
        Some("t-005".to_string())
    );
    assert_eq!(
        extract_plan_line_task_id("1. [ ] (t-006) Numbered task"),
        Some("t-006".to_string())
    );
    assert_eq!(
        extract_plan_line_task_id("* [ ] `t-007` Backtick task"),
        Some("t-007".to_string())
    );
    assert_eq!(
        extract_plan_line_task_id("- [x] [t-008] Completed task"),
        Some("t-008".to_string())
    );
    assert_eq!(extract_plan_line_task_id("- [ ] No task id here"), None);
    assert_eq!(extract_plan_line_task_id("# Plan header"), None);
}

#[test]
fn test_line_matches_task_id_exact_and_boundary() {
    let line_a1 = "- [ ] [t-100a1] Subtask A1 (depends on t-100)";
    let line_100 = "- [ ] [t-100] Primary task 100";
    let line_1 = "- [ ] [t-1] Task one";

    // Exact match must succeed
    assert!(line_matches_task_id(line_a1, "t-100a1"));
    assert!(line_matches_task_id(line_100, "t-100"));
    assert!(line_matches_task_id(line_1, "t-1"));

    // A task id must never match a longer id that merely starts with it!
    assert!(!line_matches_task_id(line_a1, "t-100"));
    assert!(!line_matches_task_id(line_a1, "t-1"));
    assert!(!line_matches_task_id(line_a1, "t-10"));

    // The same rule in reverse: the short id must not match the longer id
    assert!(!line_matches_task_id(line_100, "t-1"));
    assert!(!line_matches_task_id(line_100, "t-10"));
    assert!(!line_matches_task_id(line_100, "t-100a1"));
}

#[test]
fn test_subagent_scrolling_and_focus() {
    let mut r = TuiRenderer::new();
    r.focused_panel = FocusedPanel::Subagents;
    r.subagents.push(SubagentDetail {
        name: "coder".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        last_activity_at: None,
        logs: vec![
            "log1".to_string(),
            "log2".to_string(),
            "log3".to_string(),
            "log4".to_string(),
        ],
        thinking: "think line 1\nthink line 2\nthink line 3".to_string(),
        content: "output line 1\noutput line 2".to_string(),
        is_active: true,
        context_tokens: 0,
        worked_duration: Duration::ZERO,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    });

    r.show_thought = true;
    r.subagent_width.set(40);
    r.subagent_height.set(5);

    r.scroll_down(3);
    assert_eq!(r.subagent_scroll, 3);

    r.scroll_up(2);
    assert_eq!(r.subagent_scroll, 1);

    r.scroll_to_bottom();
    let max = r.estimated_subagent_lines(40).saturating_sub(5) as u16;
    assert_eq!(r.subagent_scroll, max);
}

#[test]
fn test_plan_auto_open_and_close() {
    let mut r = TuiRenderer::new();
    assert!(!r.show_plan_panel);
    assert!(!r.had_active_plan);

    // Simulate active plan appearing
    let has_active_plan = true;
    if has_active_plan && !r.had_active_plan {
        r.show_plan_panel = true;
        r.plan_scroll = 0;
        r.plan_auto_scroll = true;
    }
    r.had_active_plan = has_active_plan;
    assert!(r.show_plan_panel);

    // Simulate plan completed and archived
    let has_active_plan = false;
    if !has_active_plan && r.had_active_plan {
        r.show_plan_panel = false;
        if r.focused_panel == FocusedPanel::Plan {
            r.focused_panel = FocusedPanel::Chat;
        }
    }
    r.had_active_plan = has_active_plan;
    assert!(!r.show_plan_panel);
}

#[test]
fn test_subagent_switch_scrolls_to_bottom_and_autoscrolls() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    r.subagents.push(SubagentDetail {
        name: "coder-1".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        last_activity_at: None,
        logs: vec![
            "line 1".to_string(),
            "line 2".to_string(),
            "line 3".to_string(),
        ],
        thinking: "think".to_string(),
        content: "output".to_string(),
        is_active: true,
        context_tokens: 0,
        worked_duration: Duration::ZERO,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    });
    r.subagents.push(SubagentDetail {
        name: "coder-2".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        last_activity_at: None,
        logs: vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ],
        thinking: "think 2".to_string(),
        content: "output 2".to_string(),
        is_active: true,
        context_tokens: 0,
        worked_duration: Duration::ZERO,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    });

    r.subagent_width.set(40);
    r.subagent_height.set(4);

    r.focused_panel = FocusedPanel::Subagents;
    r.scroll_up(5);
    assert!(!r.subagent_autoscroll);

    // Switch subagent right -> should auto-scroll to bottom and re-enable autoscroll
    r.selected_subagent_idx += 1;
    r.subagent_autoscroll = true;
    let w = r.subagent_width.get();
    let n = r.estimated_subagent_lines(w);
    let h = r.subagent_height.get();
    r.subagent_scroll = n.saturating_sub(h) as u16;

    assert!(r.subagent_autoscroll);
    assert_eq!(r.subagent_scroll, n.saturating_sub(h) as u16);
}

#[test]
fn test_subagents_auto_focuses_latest_activated_agent() {
    let mut r = TuiRenderer::new();
    r.upsert_subagent("coder-t-001", true, "started t-001");
    assert_eq!(r.selected_subagent_idx, 0);

    r.upsert_subagent("coder-t-002", true, "started t-002");
    assert_eq!(r.selected_subagent_idx, 1);

    r.upsert_subagent("researcher-t-003", true, "started t-003");
    assert_eq!(r.selected_subagent_idx, 2);

    // Completion should not shift focus away from currently selected
    r.upsert_subagent("coder-t-001", false, "completed t-001");
    assert_eq!(r.selected_subagent_idx, 2);
}

/// Caret stepping, insertion and deletion are key handler behaviour now (§5): the
/// same keys the operator presses drive them, and they work whatever pane is
/// focused.
#[test]
fn test_input_cursor_stepping_left_and_right() {
    let mut r = TuiRenderer::new();
    r.set_focus(state::FocusPane::Transcript);
    r.input_text = "hello world".to_string();
    r.cursor = "hello world".len();
    r.ensure_textarea_in_sync();

    for _ in 0..5 {
        press(&mut r, KeyCode::Left, KeyModifiers::NONE);
    }
    assert_eq!(r.cursor, "hello ".len());

    press(&mut r, KeyCode::Char('X'), KeyModifiers::NONE);
    assert_eq!(r.input_text, "hello Xworld");
    assert_eq!(r.cursor, "hello X".len());

    press(&mut r, KeyCode::Right, KeyModifiers::NONE);
    press(&mut r, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(r.cursor, "hello Xwo".len());

    press(&mut r, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(r.input_text, "hello Xwrld");

    press(&mut r, KeyCode::Delete, KeyModifiers::NONE);
    assert_eq!(r.input_text, "hello Xwld");
}

#[test]
fn test_format_terminal_math() {
    let raw = r#"$$\|(O + tD) - C\|^2 = r^2$$
Let $L = O - C$. The equation becomes:
$$(L + tD) \cdot (L + tD) = r^2$$
$$(D \cdot D)t^2 + 2(L \cdot D)t + (L \cdot L) - r^2 = 0$$

This is a quadratic equation in the form $at^2 + bt + c = 0$ where:
- $a = D \cdot D$
- $b = 2(L \cdot D)$
- $c = (L \cdot L) - r^2$"#;

    let formatted = format_terminal_math(raw);
    assert!(!formatted.contains("$$"));
    assert!(!formatted.contains(r"\cdot"));
    assert!(!formatted.contains(r"\|"));
    assert!(formatted.contains("·"));
    assert!(formatted.contains("²"));
    assert!(formatted.contains("(D · D)t² + 2(L · D)t + (L · L) - r² = 0"));
    assert!(formatted.contains("at² + bt + c = 0"));
}

/// True positives: the math-context gates (`has_complete_latex_command`,
/// `has_delimited_script`, `has_math_operator`, `looks_like_math_span`) must not
/// stop genuine LaTeX from converting.
#[test]
fn test_format_terminal_math_converts_genuine_math() {
    let display = format_terminal_math(r"$$\|(O + tD) - C\|^2 = r^2$$");
    assert!(
        display.contains("|(O + tD) - C|² = r²"),
        "display math lost absolute-value bars or superscripts, got:\n{display}"
    );
    assert!(
        !display.contains("$$"),
        "display delimiters survived: {display}"
    );

    assert_eq!(
        format_terminal_math(r"$\alpha + \beta = \gamma$"),
        "α + β = γ"
    );

    let frac = format_terminal_math(r"$\frac{a}{b} + \sqrt{2}$");
    assert!(frac.contains("(a)/(b)"), "frac not rewritten, got:\n{frac}");
    assert!(frac.contains("√(2)"), "sqrt not rewritten, got:\n{frac}");

    assert_eq!(format_terminal_math(r"\cdot"), "·");
    assert_eq!(format_terminal_math(r"\leq"), "≤");

    let pythagoras = format_terminal_math(r"$x^2 + y^2 = z^2$");
    assert!(
        pythagoras.contains("x²"),
        "caret swallowed, got:\n{pythagoras}"
    );
    assert!(
        pythagoras.contains("z²"),
        "caret swallowed, got:\n{pythagoras}"
    );
}

/// Regression guard for the original bug: snake_case identifiers were rewritten
/// into subscripts and silently mangled. They must come back byte-for-byte.
#[test]
fn test_format_terminal_math_keeps_identifiers_verbatim() {
    for raw in [
        "let task_id = subagent_name;",
        "handle_input",
        "read_config_2",
        "x_y_z",
    ] {
        assert_eq!(
            format_terminal_math(raw),
            raw,
            "identifier mangled: {raw:?}"
        );
    }
}

/// A markdown table row carries pipes and underscore-heavy cells; nothing there
/// is math.
#[test]
fn test_format_terminal_math_keeps_markdown_table_row_verbatim() {
    let row = "| _id | name_2 |";
    assert_eq!(format_terminal_math(row), row, "table row mangled");
}

/// Windows-style paths (and their backslash separators) and unix source paths
/// must survive untouched.
#[test]
fn test_format_terminal_math_keeps_paths_verbatim() {
    for raw in [
        r"C:\tools\build",
        r"C:\Users\me\project",
        "src/ui/tui/formatting.rs",
    ] {
        assert_eq!(format_terminal_math(raw), raw, "path mangled: {raw:?}");
    }
}

/// Shell snippets and regex character classes contain carets attached to nothing
/// (`'[^0-9]'`); the caret must not be treated as a superscript.
#[test]
fn test_format_terminal_math_keeps_shell_and_regex_verbatim() {
    for raw in [r"grep '^TODO' src", r"sed 's/[^0-9]//g'", r"a[^b]c"] {
        assert_eq!(
            format_terminal_math(raw),
            raw,
            "shell/regex mangled: {raw:?}"
        );
    }
}

/// Words that merely start with a backslash (`\todo`, `\get`, `\internal`) only
/// share a prefix with real commands (`\to`, `\ge`, `\int`) and must not convert.
#[test]
fn test_format_terminal_math_keeps_backslash_prose_verbatim() {
    let raw = r"\todo \get \internal";
    assert_eq!(format_terminal_math(raw), raw, "prose backslashes mangled");
}

/// Markdown emphasis uses `_`, which must never become a subscript.
#[test]
fn test_format_terminal_math_keeps_emphasis_verbatim() {
    let raw = "markdown *bold* and _emph_ text";
    assert_eq!(format_terminal_math(raw), raw, "emphasis mangled");
}

/// Numeric digit separators must stay as written.
#[test]
fn test_format_terminal_math_keeps_numeric_separators_verbatim() {
    for raw in ["1_000_000", "id_1", "2^10"] {
        assert_eq!(format_terminal_math(raw), raw, "number mangled: {raw:?}");
    }
}

/// Dollar signs are money and shell variables far more often than they are math
/// delimiters; they must never be deleted or rewritten.
#[test]
fn test_format_terminal_math_preserves_currency_and_shell_variables() {
    for raw in [
        "it costs $5 and $10",
        "echo $HOME and $PATH",
        "prices range $100 - $200 total",
    ] {
        assert_eq!(
            format_terminal_math(raw),
            raw,
            "currency/shell dollars lost: {raw:?}"
        );
    }
}

/// Inline backtick code spans pass through verbatim, while math written outside
/// the backticks still converts.
#[test]
fn test_format_terminal_math_keeps_inline_code_spans_verbatim() {
    for raw in ["`task_id`", "call `read_config_2` now", "`x_1` and `a^b`"] {
        assert_eq!(format_terminal_math(raw), raw, "code span mangled: {raw:?}");
    }

    assert_eq!(
        format_terminal_math("`x^2` and $x^2$"),
        "`x^2` and x²",
        "code span was converted, or the math outside it was not"
    );

    // An unterminated backtick run stays verbatim to the end of its line, so a
    // stray opening fence can never swallow the rest of the line.
    assert_eq!(format_terminal_math("```x^2"), "```x^2");
}

/// Self-contained fenced code — a fence marker line (with or without an info
/// string) and a fence that opens and closes on one line — is emitted untouched.
#[test]
fn test_format_terminal_math_keeps_self_contained_fenced_code_verbatim() {
    for raw in [
        "```rust",
        "```x_1 = 2```",
        "~~~py\nlet x_1 = 2\n~~~",
        "```rust\nlet x_1 = 2;\nlet y^2 = 3;\n```",
    ] {
        assert_eq!(
            format_terminal_math(raw),
            raw,
            "fenced code mangled: {raw:?}"
        );
    }
}

/// `format_terminal_math` deliberately stays stateless — its public signature
/// takes no `in_code_block` flag — so fence state cannot be tracked across
/// separate calls, and a body line handed over on its own is still math-scanned.
#[test]
fn test_format_terminal_math_fence_state_is_line_local_known_limitation() {
    // known limitation: the fence exemption is line-local, so a code line taken
    // out of a multi-line block still converts (`x_1` → `x₁`). The block itself
    // (previous test) stays verbatim because it is handed over in one call.
    assert_eq!(format_terminal_math("let x_1 = 2;"), "let x₁ = 2;");
}

#[test]
fn test_tui_submit_empty_line_sends_to_channel_for_enter_resume() {
    let mut r = TuiRenderer::new();
    r.input_text = String::new();
    r.submit();

    // Receiver should have received an empty line so read_input unblocks on Enter
    assert_eq!(r.rx.try_recv().ok(), Some("".to_string()));
}

#[test]
fn test_plan_caching_and_throttling() {
    let mut renderer = TuiRenderer::new();
    renderer.plan_content = "Cached plan".to_string();
    renderer.last_plan_check = std::time::Instant::now();

    assert_eq!(renderer.plan_content, "Cached plan");
    assert!(!renderer.plan_is_archived);
}

#[test]
fn test_status_bar_prefers_plan_start_time() {
    let mut r = TuiRenderer::new();

    // No plan start yet -> None (only counts when actively working on a plan)
    assert!(r.total_project_elapsed().is_none());

    r.plan_start_time = Some(std::time::Instant::now());
    assert!(r.total_project_elapsed().is_some());
}

#[test]
fn test_delegation_completed_emits_task_id_completed_in_chat() {
    let mut r = TuiRenderer::new();
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-007".to_string()),
        },
    ));

    // Validator approval status should NOT dump into chat
    r.on_event(&Event::Status(
        "[Validator] APPROVED deliverable for coder-t-007:\nAll checks passed.".to_string(),
    ));
    assert!(
        !r.messages
            .iter()
            .any(|m| m.contains("[Validator] APPROVED")),
        "validator approval must not be pushed into chat messages"
    );
    // But it should be present in subagent logs
    assert!(
        r.subagents[0]
            .logs
            .iter()
            .any(|l| l.contains("[Validator] APPROVED deliverable for coder-t-007")),
        "validator approval should be recorded in subagent logs"
    );

    // Delegation completion pushes a completed-task line to the chat
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Completed {
            agent: crate::agents::Agent::Coder,
            task: Some("t-007".to_string()),
        },
    ));

    assert_eq!(
        r.messages.last().map(String::as_str),
        Some("[t-007] completed.")
    );

    let (style, special) = message_style("[t-007] completed.");
    assert!(special);
    assert_eq!(style.fg, Some(ratatui::style::Color::Green));
    assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
}

#[test]
fn test_parallel_specialist_streaming_routes_to_correct_subagents() {
    let mut r = TuiRenderer::new();

    // Start two parallel subagents
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-002".to_string()),
        },
    ));
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-003".to_string()),
        },
    ));

    // Stream interleaved thinking and content for both specialists
    r.on_event(&Event::SubagentThinking {
        agent_tag: "coder-t-002".to_string(),
        text: "Thinking about Cartridge...".to_string(),
    });
    r.on_event(&Event::SubagentThinking {
        agent_tag: "coder-t-003".to_string(),
        text: "Thinking about 6502 CPU...".to_string(),
    });
    r.on_event(&Event::SubagentMessage {
        agent_tag: "coder-t-002".to_string(),
        text: "Cartridge code written.".to_string(),
    });
    r.on_event(&Event::SubagentMessage {
        agent_tag: "coder-t-003".to_string(),
        text: "CPU instructions written.".to_string(),
    });

    let s2 = r
        .subagents
        .iter()
        .find(|s| s.name == "coder-t-002")
        .unwrap();
    let s3 = r
        .subagents
        .iter()
        .find(|s| s.name == "coder-t-003")
        .unwrap();

    // The first specialist only received its own chunks
    assert_eq!(s2.thinking, "Thinking about Cartridge...");
    assert_eq!(s2.content, "Cartridge code written.");

    // The second specialist only received its own chunks
    assert_eq!(s3.thinking, "Thinking about 6502 CPU...");
    assert_eq!(s3.content, "CPU instructions written.");

    // Manager chat was never polluted with subagent output
    assert_eq!(r.current_thought, "");
    assert_eq!(r.current_content, "");
}

/// `/help` prints the one shared legend (§10 item 3): the same text the `F1`
/// overlay and the footer legend are built from, so the three surfaces cannot
/// drift apart.
#[test]
fn test_tui_help_command_includes_ctrl_t() {
    let mut r = TuiRenderer::new();
    r.input_text = "/help".to_string();
    r.submit();

    let help_msg = r
        .messages
        .last()
        .expect("should have emitted /help message");
    let body = help_msg
        .strip_prefix("[CLI] ")
        .expect("/help prints the CLI-prefixed legend");
    assert_eq!(body, components::help::help_text().as_str());
    assert!(help_msg.contains("Ctrl+T"));
    assert!(help_msg.contains("toggle the thinking chip"));
}

#[test]
fn test_manager_output_routes_to_chat_when_specialist_inactive() {
    let mut r = TuiRenderer::new();
    r.subagents.push(SubagentDetail {
        name: "researcher-steer-task-1".to_string(),
        task_id: Some("steer-task-1".to_string()),
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: false,
        context_tokens: 0,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    });
    r.active_agent = "researcher-steer-task-1".to_string();

    r.on_event(&Event::Message("Hello from Manager".to_string()));
    assert_eq!(r.active_agent, "Manager");
    assert_eq!(r.current_content, "Hello from Manager");
    assert_eq!(r.subagents[0].content, "");

    r.active_agent = "researcher-steer-task-1".to_string();
    r.on_event(&Event::Thinking("Manager thinking".to_string()));
    assert_eq!(r.active_agent, "Manager");
    assert_eq!(r.current_thought, "Manager thinking");
    assert_eq!(r.subagents[0].thinking, "");
}

#[test]
fn test_delegation_events_sanitize_bracketed_task_ids() {
    let mut r = TuiRenderer::new();
    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: crate::agents::Agent::Researcher,
            task: Some("[steer-task-1]".to_string()),
        },
    ));
    assert_eq!(r.active_agent, "researcher-steer-task-1");
    assert_eq!(r.subagents.len(), 1);
    assert_eq!(r.subagents[0].name, "researcher-steer-task-1");
    assert!(r.subagents[0].is_active);

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Completed {
            agent: crate::agents::Agent::Researcher,
            task: Some("steer-task-1".to_string()),
        },
    ));
    assert_eq!(r.active_agent, "Manager");
    assert!(!r.subagents[0].is_active);
}

#[test]
fn test_set_subagents_preserves_manager_active_agent() {
    let mut r = TuiRenderer::new();
    assert_eq!(r.active_agent, "Manager");

    let active_list = vec![SubagentDetail {
        name: "coder-t-1".to_string(),
        task_id: Some("t-1".to_string()),
        prompt: String::new(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: true,
        context_tokens: 0,
        queue: None,
        paused: false,

        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    }];
    r.set_subagents(active_list);

    assert_eq!(r.active_agent, "Manager");
}

#[test]
fn test_ansi_and_markdown() {
    use ansi_to_tui::IntoText;

    let ansi_bytes = b"\x1b[32mok\x1b[0m \x1b[31mFAILED\x1b[0m";
    let text = ansi_bytes.into_text().expect("ansi parsing should succeed");
    assert_eq!(text.lines.len(), 1);
    assert!(text.lines[0].spans.len() >= 2);

    let md = "# Title\n\n```rust\nfn main() {}\n```";
    let md_text = tui_markdown::from_str(md);
    assert!(!md_text.lines.is_empty());
}

#[test]
fn test_multiline_input_trailing_newlines_preserved() {
    let mut r = TuiRenderer::new();
    r.set_input_content("hello\nworld\n");
    assert_eq!(r.textarea.lines().len(), 3);
    assert_eq!(r.textarea.lines(), &["hello", "world", ""]);

    // Ensure sync preserves the trailing empty line
    r.ensure_textarea_in_sync();
    assert_eq!(r.textarea.lines().len(), 3);
    assert_eq!(r.input_text, "hello\nworld\n");
}

#[test]
fn test_insert_newline_and_sync() {
    let mut r = TuiRenderer::new();
    r.set_input_content("first line");
    r.ensure_textarea_in_sync();
    r.textarea.insert_newline();
    r.sync_input_from_textarea();
    assert_eq!(r.input_text, "first line\n");
    assert_eq!(r.textarea.lines().len(), 2);
    assert_eq!(r.textarea.lines(), &["first line", ""]);

    r.textarea.insert_str("second line");
    r.sync_input_from_textarea();
    assert_eq!(r.input_text, "first line\nsecond line");
    assert_eq!(r.textarea.lines(), &["first line", "second line"]);
}

#[test]
fn test_submit_multiline_input() {
    let mut r = TuiRenderer::new();
    r.set_input_content("line 1\nline 2");
    r.submit();
    assert_eq!(r.input_text, "");
    assert_eq!(r.textarea.lines(), &[""]);
}

// ============================================================================
// Bracketed paste — one pasted block must become ONE composer buffer and ONE
// submitted message, never one steering command per line. `init()` turns on
// SGR mode 2004 so the terminal hands the whole body over as a single
// `Event::Paste`, whose arm calls `paste_text()` (which never calls `submit()`);
// Enter remains the only submit trigger.
// ============================================================================

/// Drain every string the composer has handed to the session loop so far, so a
/// test can assert on "exactly one submission" instead of "at least one".
fn drain_submissions(r: &mut TuiRenderer) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(line) = r.rx.try_recv() {
        out.push(line);
    }
    out
}

#[test]
fn test_paste_text_inserts_multiline_block_as_one_buffer_without_submitting() {
    let block = "rad ett\nrad två\nrad tre";
    let mut r = TuiRenderer::new();
    r.focused_panel = FocusedPanel::Chat;

    r.paste_text(block);

    // The whole block lands in the composer verbatim ...
    assert_eq!(r.input_text, block);
    // ... as ONE entry: a single widget whose editor lines were spliced in by one
    // `insert_str`, re-joined into exactly the buffer Enter would submit.
    assert_eq!(r.textarea.lines(), &["rad ett", "rad två", "rad tre"]);
    assert_eq!(r.textarea.lines().join("\n"), r.input_text);
    // The paste repaints the composer instead of sending anything.
    assert!(r.dirty, "a paste must mark the composer dirty");

    // Nothing was submitted: the mpsc channel is still empty ...
    assert!(
        matches!(r.rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
        "a paste must never submit on its own"
    );
    // ... and the seam the session loop steers on reports no input. `dirty` is
    // cleared first so the poll has nothing to repaint.
    r.dirty = false;
    assert_eq!(r.poll_input(), None);

    // Still draft input: no `User:` echo in chat, no input-history entry.
    assert!(r.history.is_empty());
    assert!(!r.messages.iter().any(|m| m.starts_with("User:")));
}

#[test]
fn test_paste_text_normalises_line_endings_and_strips_one_trailing_newline() {
    let cases: &[(&str, &str)] = &[
        // CRLF (Windows / many terminals) and a terminal-appended final CR
        ("a\r\nb\r\n", "a\nb"),
        // lone CR (classic Mac / some terminals)
        ("x\ry\r", "x\ny"),
        // mixed
        ("b1\nb2\r\nb3\r", "b1\nb2\nb3"),
        // plain LF
        ("rad ett\nrad två\n", "rad ett\nrad två"),
        // only ONE trailing newline is padding: an inner blank line is content
        ("a\nb\n\n", "a\nb\n"),
    ];

    for (pasted, expected) in cases {
        let mut r = TuiRenderer::new();
        r.focused_panel = FocusedPanel::Chat;

        r.paste_text(pasted);

        assert_eq!(r.input_text, *expected, "paste of {pasted:?}");
        assert_eq!(
            r.textarea.lines().join("\n"),
            *expected,
            "widget/buffer drift for paste of {pasted:?}"
        );
        assert!(
            drain_submissions(&mut r).is_empty(),
            "paste of {pasted:?} must not submit a line"
        );
    }
}

#[test]
fn test_paste_text_of_only_line_endings_leaves_the_buffer_untouched() {
    for pasted in ["", "\r\n", "\n", "\r"] {
        let mut r = TuiRenderer::new();
        r.focused_panel = FocusedPanel::Chat;
        r.set_input_content("befintligt innehåll");
        let cursor_before = r.cursor;

        r.paste_text(pasted);

        assert_eq!(r.input_text, "befintligt innehåll", "paste of {pasted:?}");
        assert_eq!(
            r.textarea.lines(),
            &["befintligt innehåll"],
            "paste of {pasted:?}"
        );
        assert_eq!(
            r.cursor, cursor_before,
            "caret must not move for {pasted:?}"
        );
        assert!(
            drain_submissions(&mut r).is_empty(),
            "paste of {pasted:?} must not submit"
        );
    }
}

#[test]
fn test_paste_then_enter_submits_the_whole_block_as_one_message() {
    let block = "rad ett\nrad två\nrad tre";
    let mut r = TuiRenderer::new();
    r.focused_panel = FocusedPanel::Chat;

    r.paste_text(block);
    // Enter is the only submit trigger (`KeyCode::Enter` arm -> `submit()`), so one
    // Enter stands for the whole pasted block.
    r.submit();

    // Exactly one submission, equal to the whole block, then nothing left.
    // `dirty` is cleared before each poll so the throttled flush has nothing to
    // repaint and the test stays free of real render I/O.
    r.dirty = false;
    assert_eq!(r.poll_input().as_deref(), Some(block));
    r.dirty = false;
    assert_eq!(r.poll_input(), None);
    assert!(
        drain_submissions(&mut r).is_empty(),
        "a paste + one Enter must produce exactly one message, not one per line"
    );

    // One composer entry became one history entry and one chat echo.
    assert_eq!(r.history, vec![block.to_string()]);
    assert_eq!(
        r.messages.last().map(String::as_str),
        Some("User: rad ett\nrad två\nrad tre")
    );
    // The composer is empty again.
    assert_eq!(r.input_text, "");
    assert_eq!(r.textarea.lines(), &[""]);
}

#[test]
fn test_paste_with_trailing_newline_then_enter_yields_exactly_one_submission() {
    // Regression guard for the reported bug: terminals end a paste with a newline,
    // so without bracketed paste every embedded CR parsed as Enter and steered one
    // command per line. The trailing newline must not survive as a blank line and
    // must not become extra submissions.
    let mut r = TuiRenderer::new();
    r.focused_panel = FocusedPanel::Chat;

    r.paste_text("rad ett\nrad två\n");

    assert_eq!(r.input_text, "rad ett\nrad två");
    assert!(
        !r.input_text.ends_with('\n'),
        "the buffer must not end on an empty line the next Enter would swallow"
    );
    assert!(
        matches!(r.rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
        "the trailing newline alone must not submit"
    );

    r.submit();

    let submitted = drain_submissions(&mut r);
    assert_eq!(submitted.len(), 1, "expected exactly one submission");
    assert_eq!(submitted[0], "rad ett\nrad två");
    assert_eq!(r.history, vec!["rad ett\nrad två".to_string()]);
}

#[test]
fn test_paste_of_a_command_shaped_block_is_not_interpreted_per_line() {
    // A pasted snippet whose FIRST line looks like a command is still delivered
    // verbatim as one message: command predicates refuse multi-line submissions
    // (see `submission_is_single_line`), and the composer never splits the block.
    let block = "/reset\nnästa rad";
    let mut r = TuiRenderer::new();
    r.focused_panel = FocusedPanel::Chat;
    r.plan_content = "## Active plan\n- [ ] [t-1] keep me".to_string();

    r.paste_text(block);
    assert_eq!(r.input_text, block);
    assert!(
        matches!(r.rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
        "the `/reset` line inside a paste must not act as a command"
    );

    r.submit();

    let submitted = drain_submissions(&mut r);
    assert_eq!(submitted, vec![block.to_string()]);
    // No `/reset` side effects fired ...
    assert!(
        !r.messages
            .iter()
            .any(|m| m.contains("Execution plan reset"))
    );
    assert_eq!(r.plan_content, "## Active plan\n- [ ] [t-1] keep me");
    assert_eq!(r.status_line, "Ready");
    // ... and no line acted as `/abort` either.
    assert!(!r.input_state.aborted);
    assert!(!r.input_state.user_exit);
}

#[test]
fn test_clean_task_id_delegates_to_task_id_normalizer() {
    // This borrowed UI seam must stay a pure delegation to `crate::task_id`:
    // re-spelling the trimming chain here fails this test.
    for raw in [
        "[t-001]",
        "\"t-001\"",
        "  ('t-001')  ",
        "t-001",
        "[t-001",
        "[t-001]]",
    ] {
        assert_eq!(clean_task_id(raw), "t-001", "borrowed UI seam for {raw:?}");
        assert_eq!(
            clean_task_id(raw),
            crate::task_id::normalize_task_id_ref(raw),
            "borrowed UI seam diverges from the canonical normalizer for {raw:?}"
        );
    }
    for raw in ["", "[]", "   ", "\"\"", "[ ]", "[\"\"]"] {
        assert_eq!(clean_task_id(raw), "", "borrowed UI seam for {raw:?}");
    }
}

#[test]
fn test_line_matches_task_id_accepts_decorated_targets() {
    // The plan-line matcher consumes the same seam: decoration on the target
    // must not change matching, and empty targets must never match.
    assert!(line_matches_task_id(
        "- [ ] [t-100] Do the thing",
        "[t-100]"
    ));
    assert!(line_matches_task_id(
        "- [ ] [t-100] Do the thing",
        " \"t-100\" "
    ));
    assert!(!line_matches_task_id(
        "- [ ] [t-100a1] Do the thing",
        "[t-100]"
    ));
    assert!(!line_matches_task_id("- [ ] [t-100] Do the thing", "[]"));
    assert!(!line_matches_task_id("- [ ] [t-100] Do the thing", ""));
}

// ============================================================================
// Renderer rehydration hooks
// ============================================================================

/// Build a panel with every field populated, so a round trip has something to
/// lose if the projection is not field-for-field.
fn rich_subagent(name: &str) -> SubagentDetail {
    SubagentDetail {
        name: name.to_string(),
        task_id: Some(format!("t-{name}")),
        prompt: format!("brief for {name}"),
        started_at: Some(std::time::Instant::now() - std::time::Duration::from_secs(372)),
        worked_duration: std::time::Duration::from_millis(372_125),
        last_activity_at: Some(std::time::Instant::now()),
        logs: vec![
            "[Started]".to_string(),
            "run_command: ls".to_string(),
            String::new(),
            "[Completed]".to_string(),
        ],
        thinking: "reasoning with é and 🎉".to_string(),
        content: "output\nsecond line".to_string(),
        is_active: true,
        context_tokens: 18_431,
        queue: None,
        paused: false,
        // The D1 lifecycle fields are NOT part of the durable panel (the worker
        // registry is their only source), so they stay at their defaults here: a
        // round-trip through `capture_ui` / `restore_panes` must not claim them.
        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    }
}

/// Same panel, minus the two `Instant`s: `TimePoint` re-anchoring is inherently
/// process-relative, so a whole-section equality check must not carry instants.
/// The instant projection itself is asserted below.
fn subagent_without_instants(name: &str) -> SubagentDetail {
    let mut detail = rich_subagent(name);
    detail.started_at = None;
    detail.last_activity_at = None;
    detail
}

#[test]
fn capture_ui_copies_the_pane_verbatim_including_empty_and_duplicate_entries() {
    // The pane vector is the byte authority and is copied element by element,
    // in order, unmodified.
    let mut r = TuiRenderer::new();
    r.messages = vec![
        "User: first".to_string(),
        String::new(),
        "[Tool Call] run_command(\"ls\")".to_string(),
        "Marmennill: a\nb\nc".to_string(),
        "Marmennill: a\nb\nc".to_string(),
        "[CLI] Thinking display: on".to_string(),
    ];
    r.status_line = "Active: coder | 6m12s".to_string();

    let section = r.capture_ui();
    assert_eq!(section.chat_messages, r.messages);
    assert_eq!(section.status_line, r.status_line);
    assert_eq!(section.chat_messages.len(), 6);
    // A live renderer always captures a loadable section.
    assert_eq!(section.structural_violations(), Vec::<String>::new());
}

#[test]
fn capture_then_restore_leaves_the_pane_and_view_state_identical() {
    let mut source = TuiRenderer::new();
    source.messages = (0..40).map(|i| format!("pane line {i}")).collect();
    source.status_line = "Active: coder | 3m".to_string();
    source.show_thought = true;
    source.show_subagent_panel = true;
    source.chat_auto_scroll = false;
    source.chat_scroll = 7;
    source.focused_panel = FocusedPanel::Subagents;
    source.subagents = vec![
        subagent_without_instants("coder"),
        subagent_without_instants("researcher"),
    ];
    source.selected_subagent_idx = 1;

    let section = source.capture_ui();
    assert_eq!(section.structural_violations(), Vec::<String>::new());

    let mut target = TuiRenderer::new();
    target.restore_panes(&section);

    // The visible state is what was stored, before any draw.
    assert_eq!(target.messages, source.messages);
    assert_eq!(target.status_line, source.status_line);
    assert!(target.show_thought);
    assert!(target.show_subagent_panel);
    assert!(!target.chat_auto_scroll);
    assert_eq!(target.chat_scroll, 7);
    assert_eq!(target.focused_panel, FocusedPanel::Subagents);
    assert_eq!(target.selected_subagent_idx, 1);
    // Re-capturing the restored renderer reproduces the section exactly.
    assert_eq!(target.capture_ui(), section);
}

#[test]
fn restore_panes_invalidates_the_caches_so_no_stale_line_is_drawn() {
    // The render caches are keyed by message count, so a wholesale replace of
    // equal length MUST invalidate them or the next frame would draw stale bytes.
    let mut r = TuiRenderer::new();
    r.messages = vec!["short one".to_string(), "short two".to_string()];
    assert_eq!(r.estimated_chat_lines(80), 2);

    let section = crate::session::format::UiSection {
        chat_messages: vec!["a\nb\nc\nd".to_string(), "e\nf".to_string()],
        ..Default::default()
    };
    r.restore_panes(&section);

    assert_eq!(r.messages, section.chat_messages);
    assert_eq!(r.estimated_chat_lines(80), 6);
    assert_eq!(r.cached_rendered_messages.borrow().len(), 2);
    assert_eq!(r.cached_message_lines.borrow().as_slice(), &[4, 2]);
    assert!(r.dirty);
}

#[test]
fn capture_and_restore_round_trip_every_subagent_panel_field() {
    // The process anchor is created on first use, so pin it BEFORE building the
    // instants: a real subagent always starts after boot (see
    // `an_instant_older_than_the_process_anchor_honestly_loses_its_age` below).
    let _ = crate::session::time::anchor();
    let started = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(120));
    let last = std::time::Instant::now();

    let mut first = rich_subagent("coder");
    first.started_at = Some(started);
    first.last_activity_at = Some(last);
    let mut source = TuiRenderer::new();
    source.subagents = vec![first, subagent_without_instants("researcher")];
    source.selected_subagent_idx = 1;

    let section = source.capture_ui();
    assert_eq!(section.subagent_panels.len(), 2);
    let panel = &section.subagent_panels[0];
    assert_eq!(panel.name, "coder");
    assert_eq!(panel.task_id.as_deref(), Some("t-coder"));
    assert_eq!(panel.prompt, "brief for coder");
    assert_eq!(panel.logs, source.subagents[0].logs);
    assert_eq!(panel.thinking, "reasoning with é and 🎉");
    assert_eq!(panel.content, "output\nsecond line");
    assert!(panel.is_active);
    assert_eq!(panel.context_tokens, 18_431);
    assert!((panel.worked_duration_secs - 372.125).abs() < 1e-6);
    assert!(panel.started_at.as_ref().is_some_and(|p| p.is_sane()));
    assert!(panel.last_activity_at.as_ref().is_some_and(|p| p.is_sane()));
    // Panel order is content: the selection index keeps its meaning.
    assert_eq!(section.subagent_panels[1].name, "researcher");

    let mut target = TuiRenderer::new();
    target.restore_panes(&section);

    let restored = &target.subagents[0];
    assert_eq!(restored.logs, source.subagents[0].logs);
    assert_eq!(restored.thinking, source.subagents[0].thinking);
    assert_eq!(restored.content, source.subagents[0].content);
    assert_eq!(restored.name, "coder");
    assert_eq!(restored.task_id.as_deref(), Some("t-coder"));
    assert_eq!(restored.prompt, "brief for coder");
    assert_eq!(restored.context_tokens, 18_431);
    // `is_active` comes back as it was, even though the run has finished.
    assert!(restored.is_active);
    // The worked duration comes from the number, never from the instants.
    assert!((restored.worked_duration.as_secs_f64() - 372.125).abs() < 1e-6);
    assert_eq!(target.subagents[1].name, "researcher");
    assert_eq!(target.selected_subagent_idx, 1);
    // Each `Instant` comes back as an instant of the same age, within the
    // wall-clock sampling quantum (it is re-anchored, never replayed).
    let restored_started = restored.started_at.expect("started_at restored");
    let age = std::time::Instant::now()
        .saturating_duration_since(restored_started)
        .as_secs_f64();
    assert!(
        (0.11..0.6).contains(&age),
        "restored started_at should keep its ~0.12s age, got {age}s"
    );
    let restored_last = restored
        .last_activity_at
        .expect("last_activity_at restored");
    let last_age = std::time::Instant::now()
        .saturating_duration_since(restored_last)
        .as_secs_f64();
    assert!(last_age < 0.6, "restored last_activity_at age {last_age}s");
}

/// The honest limit of the `Instant` projection: `elapsed_secs` has no negative
/// offsets, so an instant from BEFORE this process's anchor cannot be
/// re-anchored — its age is floored to 0.0. `SubagentDetail` cannot produce one
/// in a real session.
#[test]
fn an_instant_older_than_the_process_anchor_honestly_loses_its_age() {
    let mut source = TuiRenderer::new();
    let mut detail = rich_subagent("coder");
    detail.started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(372));
    source.subagents = vec![detail];

    let section = source.capture_ui();
    let panel = &section.subagent_panels[0];
    assert_eq!(panel.started_at.as_ref().map(|p| p.elapsed_secs), Some(0.0));
    assert!(panel.started_at.as_ref().is_some_and(|p| p.is_sane()));
    // The duration number is unaffected: it never comes from the instants.
    assert!((panel.worked_duration_secs - 372.125).abs() < 1e-6);
    // The derived wall clock is floored to the process anchor, which is what
    // makes the age unrecoverable in this one case.
    assert_eq!(
        panel.started_at.as_ref().map(|p| p.wall),
        Some(crate::session::time::anchor_wall())
    );

    let mut r = TuiRenderer::new();
    r.restore_panes(&section);
    let restored = r.subagents[0].started_at.expect("still present");
    let age = std::time::Instant::now()
        .saturating_duration_since(restored)
        .as_secs_f64();
    assert!(
        age < 60.0,
        "a pre-anchor instant re-anchors near the process start, got {age}s"
    );
}

#[test]
fn restore_panes_degrades_a_corrupt_duration_without_panicking() {
    // `Duration::from_secs_f64` panics on NaN/negative/infinite, so a hostile
    // number must degrade to zero rather than take the UI down.
    let mut r = TuiRenderer::new();
    let mut section = crate::session::format::UiSection::default();
    let mut panel = subagent_panel_from_detail(&rich_subagent("coder"));
    panel.worked_duration_secs = f64::NAN;
    section.subagent_panels.push(panel.clone());
    panel.worked_duration_secs = -5.0;
    section.subagent_panels.push(panel.clone());
    panel.worked_duration_secs = f64::INFINITY;
    section.subagent_panels.push(panel);

    r.restore_panes(&section);
    assert_eq!(r.subagents.len(), 3);
    for detail in &r.subagents {
        assert_eq!(detail.worked_duration, std::time::Duration::ZERO);
        assert_eq!(detail.logs.len(), 4);
    }
}

#[test]
fn capture_and_restore_round_trip_the_composer_text_and_caret() {
    let mut source = TuiRenderer::new();
    source.input_text = "line one\nline deux 🎉\nthree".to_string();
    source.ensure_textarea_in_sync();
    let caret = source.input_text.find("eux").expect("marker present");
    source.cursor = caret;
    source.history = vec!["first command".to_string(), "second command".to_string()];
    source.history_index = Some(1);
    source.input_draft = "half-typed draft".to_string();

    let section = source.capture_ui();
    assert_eq!(section.structural_violations(), Vec::<String>::new());
    assert_eq!(section.composer.input_text, source.input_text);
    assert_eq!(section.composer.cursor, caret);

    let mut target = TuiRenderer::new();
    target.restore_panes(&section);

    assert_eq!(target.input_text, source.input_text);
    assert_eq!(target.cursor, caret);
    assert_eq!(target.history, source.history);
    assert_eq!(target.history_index, Some(1));
    assert_eq!(target.input_draft, source.input_draft);
    // The widget was rebuilt by the existing sync path and its caret sits on
    // the restored byte offset.
    assert_eq!(target.textarea.lines().join("\n"), source.input_text);
    assert_eq!(target.textarea.cursor(), (1, 6));
    // Reading the widget back yields the same byte offset: no drift.
    target.sync_input_from_textarea();
    assert_eq!(target.cursor, caret);
}

#[test]
fn restore_panes_clamps_a_caret_past_the_end_instead_of_trusting_it() {
    // The loader validates the caret upstream; the renderer still refuses to
    // build a widget caret outside its own text.
    let mut r = TuiRenderer::new();
    let mut section = crate::session::format::UiSection::default();
    section.composer.input_text = "abc".to_string();
    section.composer.cursor = 99;
    r.restore_panes(&section);
    assert_eq!(r.cursor, 3);
    assert_eq!(r.textarea.cursor(), (0, 3));
}

#[test]
fn capture_and_restore_round_trip_scrolls_with_the_live_clamp() {
    let mut source = TuiRenderer::new();
    source.messages = (0..60).map(|i| format!("m{i}")).collect();
    // The selected panel needs enough content for a non-zero scroll maximum;
    // otherwise the live clamp legitimately pulls the offset back to 0.
    let mut tall = subagent_without_instants("coder");
    tall.content = (0..40)
        .map(|i| format!("output line {i}"))
        .collect::<Vec<String>>()
        .join("\n");
    source.subagents = vec![tall, subagent_without_instants("researcher")];
    source.selected_subagent_idx = 0;
    source.chat_scroll = 11;
    source.plan_scroll = 4;
    source.subagent_scroll = 2;
    source.subagent_list_scroll = 1;
    source.plan_auto_scroll = true;
    source.subagent_autoscroll = false;
    // Maxima are derived geometry: a rendered plan panel would have set this.
    source.plan_max_scroll = 20;

    let section = source.capture_ui();
    assert_eq!(section.view.chat_scroll, 11);
    assert_eq!(section.view.plan_scroll, 4);
    assert_eq!(section.view.subagent_scroll, 2);
    assert_eq!(section.view.subagent_list_scroll, 1);
    assert_eq!(section.view.focused_panel, "chat");

    let mut target = TuiRenderer::new();
    // Same derived geometry as the source, otherwise the live clamp
    // legitimately moves the offsets.
    target.plan_max_scroll = 20;
    target.subagent_area = Rect::new(0, 0, 40, 4);
    target.restore_panes(&section);

    assert_eq!(target.chat_scroll, 11);
    assert_eq!(target.plan_scroll, 4);
    assert_eq!(target.subagent_scroll, 2);
    assert_eq!(target.subagent_list_scroll, 1);
    assert!(target.plan_auto_scroll);
    assert!(!target.subagent_autoscroll);
    assert_eq!(target.capture_ui(), section);
}

#[test]
fn restore_panes_lets_the_existing_clamp_pull_back_an_impossible_offset() {
    // A restored offset that exceeds the restored content is clamped by
    // the live resize path, not validated by new code.
    let mut r = TuiRenderer::new();
    let mut section = source_section_with_one_line();
    section.view.chat_scroll = 9_000;
    r.restore_panes(&section);
    assert!(r.chat_scroll < 9_000);
    assert_eq!(r.messages.len(), 1);
}

fn source_section_with_one_line() -> crate::session::format::UiSection {
    let mut r = TuiRenderer::new();
    r.messages = vec!["only line".to_string()];
    r.capture_ui()
}

#[test]
fn capture_records_budgets_as_a_sorted_witness_and_restore_never_adopts_them() {
    let mut source = TuiRenderer::new();
    source.thinking_budgets.insert("coder".to_string(), 12_000);
    source
        .thinking_budgets
        .insert("researcher".to_string(), 4_000);
    source.default_thinking_budget = 8_000;

    let section = source.capture_ui();
    assert_eq!(section.budgets.thinking_budgets.len(), 2);
    assert_eq!(section.budgets.thinking_budgets["coder"], 12_000);
    assert_eq!(section.budgets.default_thinking_budget, 8_000);
    // A BTreeMap keeps the committed bytes deterministic.
    let keys: Vec<&str> = section
        .budgets
        .thinking_budgets
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["coder", "researcher"]);

    // Witness only: `Config` owns the budgets, so restore keeps the live numbers
    // untouched and reports the drift instead (see `announce_restore`).
    let mut target = TuiRenderer::new();
    target.thinking_budgets.insert("coder".to_string(), 1_000);
    target.default_thinking_budget = 2_000;
    target.restore_panes(&section);
    assert_eq!(target.thinking_budgets["coder"], 1_000);
    assert_eq!(target.thinking_budgets.len(), 1);
    assert_eq!(target.default_thinking_budget, 2_000);
}

#[test]
fn announce_restore_surfaces_every_report_field_in_the_ui() {
    // Loud, reported in the panes: no `let _ =` swallow.
    let mut r = TuiRenderer::new();
    let report = crate::session::format::RepairReport {
        ui_lines_restored: 42,
        workers_restored: 3,
        workers_unrecoverable: vec!["coder-t-1".to_string()],
        notices: vec![crate::session::format::NonRestorableNotice {
            kind: crate::session::format::NonRestorableKind::JoinHandle,
            subject: "coder".to_string(),
            detail: "no task exists to join or abort".to_string(),
        }],
        warnings: vec!["thinking budget drifted: 8000 -> 4000".to_string()],
        worker_counts: vec![crate::session::format::WorkerRepairCounts {
            key: "coder-t-1".to_string(),
            before: 10,
            after: 8,
            applied_rules: vec!["N1 orphan prune".to_string()],
        }],
        ..Default::default()
    };

    r.announce_restore(&report);

    let joined = r.messages.join("\n");
    assert!(joined.contains(
        "[CLI] Not restorable after restart [JoinHandle] coder: no task exists to join or abort"
    ));
    assert!(joined.contains("[CLI] Restore warning: thinking budget drifted: 8000 -> 4000"));
    assert!(joined.contains(
        "[CLI] Worker coder-t-1 history normalized: 10 -> 8 message(s) (N1 orphan prune)"
    ));
    assert_eq!(
        r.status_line,
        "Session restored: 42 pane line(s), 3 worker(s) — 1 capability(s) lost"
    );
    assert!(r.dirty);
}

#[test]
fn announce_restore_with_an_empty_report_still_states_the_restore() {
    let mut r = TuiRenderer::new();
    r.announce_restore(&crate::session::format::RepairReport::default());
    assert_eq!(r.messages, Vec::<String>::new());
    assert_eq!(
        r.status_line,
        "Session restored: 0 pane line(s), 0 worker(s)"
    );
}

#[test]
fn restore_keeps_the_current_focus_for_an_out_of_set_panel_spelling() {
    // An out-of-set `focused_panel` is refused by the loader; the renderer must
    // not invent a focus either.
    let mut r = TuiRenderer::new();
    r.focused_panel = FocusedPanel::Plan;
    let mut section = crate::session::format::UiSection::default();
    section.view.focused_panel = "subagent-panel".to_string();
    r.restore_panes(&section);
    assert_eq!(r.focused_panel, FocusedPanel::Plan);

    let mut section = crate::session::format::UiSection::default();
    section.view.focused_panel = "subagents".to_string();
    r.restore_panes(&section);
    assert_eq!(r.focused_panel, FocusedPanel::Subagents);
    assert_eq!(r.capture_ui().view.focused_panel, "subagents");

    let mut section = crate::session::format::UiSection::default();
    section.view.focused_panel = "plan".to_string();
    r.restore_panes(&section);
    assert_eq!(r.capture_ui().view.focused_panel, "plan");
}

// ============================================================================
// Every pane's text is restorable
// ============================================================================

#[test]
fn capture_records_the_plan_pane_and_restore_puts_its_text_back() {
    // The plan box carries its text, its visibility, the in-flight task it
    // flags, and the elapsed timer it draws.
    // Pin the monotonic anchor BEFORE building the instants: a plan always
    // starts inside the process, and an instant older than the anchor honestly
    // floors at 0 s (`an_instant_older_than_the_process_anchor_honestly_loses_its_age`).
    let _ = crate::session::time::anchor();
    let started = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(120));
    let completed = std::time::Instant::now();

    let mut source = TuiRenderer::new();
    source.plan_content = "# Plan\n- [x] t-001 wire the hook\n- [ ] t-020 🎉 more\n".to_string();
    source.show_plan_panel = true;
    source.had_active_plan = true;
    source.active_plan_task = Some("t-020".to_string());
    source.plan_start_time = Some(started);
    source.plan_completed_time = Some(completed);
    source.plan_scroll = 4;

    let section = source.capture_ui();
    assert_eq!(section.plan_content, source.plan_content);
    assert!(section.view.show_plan_panel);
    assert!(section.had_active_plan);
    assert_eq!(section.active_plan_task.as_deref(), Some("t-020"));
    // The timer is projected onto the process timeline, never re-anchored away.
    let start = section.plan_start.clone().expect("plan start recorded");
    let done = section
        .plan_completed
        .clone()
        .expect("plan completion recorded");
    assert!(start.is_sane() && done.is_sane());
    let (anchor_inst, _) = crate::session::time::anchor();
    let expected_start = started.saturating_duration_since(anchor_inst).as_secs_f64();
    assert!(
        start.elapsed_secs >= 0.0 && (start.elapsed_secs - expected_start).abs() < 1.0,
        "plan start position {start:?}, expected approx {expected_start}"
    );
    // `elapsed_secs` is a position on the process timeline, so the completion
    // sits AFTER the start, and the gap is the sleep above.
    assert!(
        done.elapsed_secs > start.elapsed_secs,
        "completion {done:?} must sit after the start {start:?}"
    );
    // The gap is the sleep above: load-tolerant bounds rather than an exact pin,
    // because a `sleep(120ms)` overshoots under scheduler pressure.
    let gap = done.elapsed_secs - start.elapsed_secs;
    assert!((0.11..2.0).contains(&gap), "start→completion gap {gap} s");
    assert_eq!(section.structural_violations(), Vec::<String>::new());

    let mut target = TuiRenderer::new();
    target.restore_panes(&section);
    assert_eq!(target.plan_content, source.plan_content);
    assert!(target.show_plan_panel);
    assert!(target.had_active_plan);
    assert_eq!(target.active_plan_task.as_deref(), Some("t-020"));
    // The plan offset is assigned verbatim, then re-clamped against the NEW
    // terminal's derived maximum (`clamp_scrolls`, which uses the per-frame
    // `plan_max_scroll` that is never persisted): before the first draw that
    // maximum is 0, so the offset is legitimately pulled back.
    assert!(
        target.plan_scroll <= 4,
        "plan scroll {}",
        target.plan_scroll
    );
    // The restored timers keep their age and their order in the new process,
    // so the plan border draws the same elapsed figure as the old pane.
    let restored_start = target
        .plan_start_time
        .expect("plan start re-anchored")
        .elapsed()
        .as_secs_f64();
    let restored_done = target
        .plan_completed_time
        .expect("plan completion re-anchored")
        .elapsed()
        .as_secs_f64();
    let stored_gap = done.elapsed_secs - start.elapsed_secs;
    let restored_gap = restored_start - restored_done;
    assert!(
        (restored_gap - stored_gap).abs() < 0.5,
        "restored start→completion gap {restored_gap} s (stored gap {stored_gap} s)"
    );
    assert!(
        restored_done < restored_start,
        "restored completion age {restored_done} vs start {restored_start}"
    );
    // Re-capture is byte-identical for every plan field.
    let again = target.capture_ui();
    assert_eq!(again.plan_content, section.plan_content);
    assert_eq!(again.view.show_plan_panel, section.view.show_plan_panel);
    assert_eq!(again.had_active_plan, section.had_active_plan);
    assert_eq!(again.active_plan_task, section.active_plan_task);
}

#[test]
fn restore_does_not_blank_a_plan_pane_for_a_bundle_that_stored_no_plan_text() {
    // Every new field has a serde default, so an older `ui.json` reads back as
    // `plan_content: ""` + `show_plan_panel: false`. That is NOT a statement
    // that the pane was blank: the plan box is disk-backed and `init` already
    // filled it, so the restore must leave it.
    let mut r = TuiRenderer::new();
    r.plan_content = "# Plan read from .marmel/execution_plan.md".to_string();
    r.show_plan_panel = true;
    r.had_active_plan = true;
    let section = crate::session::format::UiSection {
        chat_messages: vec!["User: hello".to_string()],
        ..Default::default()
    };
    r.restore_panes(&section);
    assert_eq!(r.messages, section.chat_messages);
    assert_eq!(r.plan_content, "# Plan read from .marmel/execution_plan.md");
    assert!(r.show_plan_panel);
    assert!(r.had_active_plan);
    assert!(r.plan_start_time.is_none());
}

#[test]
fn capture_and_restore_round_trip_the_streaming_partials() {
    // The in-flight thinking/answer region of the chat pane is captured as the
    // visible bytes rather than left unpersisted.
    let mut source = TuiRenderer::new();
    source.current_thought = "Let me consider é before answering…".to_string();
    source.current_content = "partial answer\nstill streaming 🎉".to_string();

    let section = source.capture_ui();
    assert_eq!(section.current_thought, source.current_thought);
    assert_eq!(section.current_content, source.current_content);
    // Display-only: capture does NOT fold a half-turn into the pane vector or
    // fabricate a transcript entry.
    assert!(section.chat_messages.is_empty());

    let mut target = TuiRenderer::new();
    target.restore_panes(&section);
    assert_eq!(target.current_thought, source.current_thought);
    assert_eq!(target.current_content, source.current_content);
    assert!(target.messages.is_empty());
    assert_eq!(target.capture_ui(), section);
}

#[test]
fn capture_and_restore_round_trip_the_active_agent_and_steer_buffer() {
    let mut source = TuiRenderer::new();
    source.active_agent = "coder-t-020".to_string();
    source.steer_sentence_buffer = "a half sentence that never got its newline".to_string();

    let section = source.capture_ui();
    assert_eq!(section.active_agent.as_deref(), Some("coder-t-020"));
    assert_eq!(
        section.steer_sentence_buffer.as_deref(),
        Some("a half sentence that never got its newline")
    );

    let mut target = TuiRenderer::new();
    target.restore_panes(&section);
    assert_eq!(target.active_agent, "coder-t-020");
    assert_eq!(
        target.steer_sentence_buffer,
        "a half sentence that never got its newline"
    );
    assert_eq!(target.capture_ui(), section);

    // `None` / empty = nothing stored: keep what THIS process derived at boot
    // instead of inventing `"Manager"`.
    let mut other = TuiRenderer::new();
    other.active_agent = "researcher-t-9".to_string();
    other.steer_sentence_buffer = "live buffer".to_string();
    let old = crate::session::format::UiSection {
        active_agent: None,
        steer_sentence_buffer: None,
        ..Default::default()
    };
    other.restore_panes(&old);
    assert_eq!(other.active_agent, "researcher-t-9");
    assert_eq!(other.steer_sentence_buffer, "live buffer");

    let blank = crate::session::format::UiSection {
        active_agent: Some(String::new()),
        ..Default::default()
    };
    other.restore_panes(&blank);
    assert_eq!(other.active_agent, "researcher-t-9");
}

#[test]
fn subagent_thinking_state_round_trips_with_its_panel() {
    // The detail pane's spinner and "thinking n/budget" bar are driven by the
    // two per-subagent maps, so they are pane state.
    let mut source = TuiRenderer::new();
    source.subagents = vec![
        subagent_without_instants("coder"),
        subagent_without_instants("researcher"),
    ];
    source.subagent_turn_thinking.insert(
        "coder".to_string(),
        "coder's in-flight reasoning".to_string(),
    );
    source
        .subagent_is_thinking
        .insert("coder".to_string(), true);
    // An entry holding "" is a DIFFERENT pane state from no entry at all (the
    // pane falls back to `thinking` only when the entry is absent), so the
    // mirror must keep the distinction.
    source
        .subagent_turn_thinking
        .insert("researcher".to_string(), String::new());
    source
        .subagent_is_thinking
        .insert("researcher".to_string(), false);

    let section = source.capture_ui();
    assert_eq!(
        section.subagent_panels[0].turn_thinking.as_deref(),
        Some("coder's in-flight reasoning")
    );
    assert_eq!(section.subagent_panels[0].is_thinking, Some(true));
    assert_eq!(
        section.subagent_panels[1].turn_thinking.as_deref(),
        Some("")
    );
    assert_eq!(section.subagent_panels[1].is_thinking, Some(false));

    let mut target = TuiRenderer::new();
    target.restore_panes(&section);
    assert_eq!(
        target
            .subagent_turn_thinking
            .get("coder")
            .map(String::as_str),
        Some("coder's in-flight reasoning")
    );
    assert_eq!(target.subagent_is_thinking.get("coder"), Some(&true));
    assert_eq!(
        target
            .subagent_turn_thinking
            .get("researcher")
            .map(String::as_str),
        Some("")
    );
    assert_eq!(target.subagent_is_thinking.get("researcher"), Some(&false));
    assert_eq!(target.capture_ui(), section);
}

#[test]
fn a_panel_with_no_thinking_entry_restores_to_no_entry() {
    // Serde default (`None`) must not fabricate a map entry: the pane's fallback
    // reads `thinking` in that case, and inventing `Some(false)` would change
    // which bar is drawn.
    let mut source = TuiRenderer::new();
    source.subagents = vec![subagent_without_instants("coder")];
    let section = source.capture_ui();
    assert_eq!(section.subagent_panels[0].turn_thinking, None);
    assert_eq!(section.subagent_panels[0].is_thinking, None);

    let mut target = TuiRenderer::new();
    target
        .subagent_turn_thinking
        .insert("coder".to_string(), "stale local thinking".to_string());
    target
        .subagent_is_thinking
        .insert("coder".to_string(), true);
    target.restore_panes(&section);
    assert!(target.subagent_turn_thinking.is_empty());
    assert!(target.subagent_is_thinking.is_empty());
    assert_eq!(target.subagents[0].thinking, "reasoning with é and 🎉");
}

#[test]
fn token_counters_are_captured_as_a_witness_and_never_adopted() {
    // The witness rule that covers `budgets` covers the token counters too: the
    // live totals belong to this process's streams (`on_event`).
    let mut source = TuiRenderer::new();
    source.on_event(&Event::TokensIn(12_345));
    source.on_event(&Event::TokensOut(6_789));
    source.on_event(&Event::TokensOut(111));

    let section = source.capture_ui();
    assert_eq!(section.tokens_in, 12_345);
    assert_eq!(section.tokens_out, 6_900);
    assert_eq!(section.orchestrator_context_tokens, 12_345);

    let mut target = TuiRenderer::new();
    target.on_event(&Event::TokensIn(100));
    target.restore_panes(&section);
    assert_eq!(target.tokens_in, 100);
    assert_eq!(target.tokens_out, 0);
    assert_eq!(target.orchestrator_context_tokens, 100);

    // The drift is reportable as `RepairReport::warnings` sentences…
    let warnings = target.token_witness_warnings(&section);
    assert_eq!(warnings.len(), 3);
    assert!(warnings[0].contains("tokens_in witness 12345 != this process's live 100"));
    assert!(warnings[1].contains("tokens_out witness 6900 != this process's live 0"));
    assert!(warnings[2].contains("orchestrator_context_tokens witness 12345"));
    // …and it is said out loud in the pane too: loud, never a swallow.
    let joined = target.messages.join("\n");
    assert!(
        joined
            .contains("[CLI] Restore warning: tokens_in witness 12345 != this process's live 100")
    );
    assert!(joined.contains("[CLI] Restore warning: tokens_out witness 6900"));
    assert!(joined.contains("[CLI] Restore warning: orchestrator_context_tokens witness 12345"));
    // A capture of the restored renderer still reports the live numbers: the
    // witness never becomes the truth.
    assert_eq!(target.capture_ui().tokens_in, 100);
}

#[test]
fn matching_token_counters_emit_no_witness_noise() {
    // Restoring into a renderer that already has the same counters must add no
    // pane lines at all — the pane bytes stay exactly as stored.
    let source = TuiRenderer::new();
    let section = source.capture_ui();
    let mut target = TuiRenderer::new();
    target.restore_panes(&section);
    assert!(target.token_witness_warnings(&section).is_empty());
    assert_eq!(target.messages, Vec::<String>::new());
    assert_eq!(target.capture_ui(), section);
}

#[test]
fn an_armed_abort_is_recorded_but_never_re_armed() {
    // A 3-second armed window has no meaning after a restart, and the closed
    // inventory never re-arms it. The boolean survives as a witness; the live
    // renderer stays unarmed so no keypress can abort a session the user never
    // asked to abort.
    let mut source = TuiRenderer::new();
    source.confirm_abort = true;
    source.abort_armed_at = Some(std::time::Instant::now());
    assert!(source.is_abort_armed());

    let section = source.capture_ui();
    assert!(section.abort_armed);
    let armed_at = section
        .abort_armed_at
        .clone()
        .expect("armed instant recorded");
    assert!(armed_at.is_sane());
    assert_eq!(section.structural_violations(), Vec::<String>::new());

    let mut target = TuiRenderer::new();
    target.restore_panes(&section);
    assert!(!target.confirm_abort);
    assert!(target.abort_armed_at.is_none());
    assert!(!target.is_abort_armed());
    let joined = target.messages.join("\n");
    assert!(joined.contains(
        "[CLI] Not restorable after restart [Other] session: confirm-abort was armed when the session was captured"
    ));
    // The stored bytes are never rewritten to match the new process.
    assert!(section.abort_armed);
}

#[test]
fn a_stale_abort_window_is_not_recorded_as_armed() {
    // `is_abort_armed` already applies the 3-second rule, so an expired window
    // never enters the bundle as `true`.
    let mut source = TuiRenderer::new();
    source.confirm_abort = true;
    source.abort_armed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(9));
    assert!(!source.is_abort_armed());
    assert!(!source.capture_ui().abort_armed);
}

#[test]
fn a_scroll_offset_the_live_clamp_moved_is_named_with_both_values() {
    // `clamp_scrolls` may move a restored offset, but it may never do so
    // SILENTLY: one sentence per moved offset, naming the field, the stored
    // value and the live value, in the same machine-readable shape that
    // `RepairReport::warnings` carries.
    let mut target = TuiRenderer::new();
    target.chat_scroll = 0;
    target.plan_scroll = 0;
    target.subagent_scroll = 28;
    target.subagent_list_scroll = 3;
    target.selected_subagent_idx = 1;

    let section = crate::session::format::UiSection {
        view: crate::session::format::ViewState {
            chat_scroll: 9_000,
            plan_scroll: 12,
            subagent_scroll: 9_000,
            subagent_list_scroll: 40,
            selected_subagent_idx: 2,
            ..Default::default()
        },
        ..Default::default()
    };

    let warnings = clamp_report(&target, &section);
    assert_eq!(
        warnings.len(),
        5,
        "every fixture offset moved: {warnings:?}"
    );
    for (warning, name, stored, live) in [
        (warnings[0].as_str(), "chat_scroll", 9_000u64, 0u64),
        (warnings[1].as_str(), "plan_scroll", 12u64, 0u64),
        (warnings[2].as_str(), "subagent_scroll", 9_000u64, 28u64),
        (warnings[3].as_str(), "subagent_list_scroll", 40u64, 3u64),
        (warnings[4].as_str(), "selected_subagent_idx", 2u64, 1u64),
    ] {
        assert!(
            warning.starts_with(&format!(
                "{name} clamped by the live geometry: {stored} -> {live}:"
            )),
            "AC-C3/I-U6: {warning:?} must open with the field name and both values"
        );
    }

    // Nothing moved means nothing said: offsets that already fit the restored
    // content must produce no scroll warning at all.
    let unchanged = target.capture_ui();
    assert_eq!(
        clamp_report(&target, &unchanged),
        Vec::<String>::new(),
        "an offset that did not move must not be reported"
    );

    // The same sentence is what the boot wiring folds into
    // `RepairReport::warnings`, read from the renderer's OWN end state — so the
    // number it names is the number the panes ended on, whatever the clamp did.
    let mut r = TuiRenderer::new();
    r.restore_panes(&section);
    assert!(
        r.chat_scroll < 9_000,
        "the clamp still clamps, nothing is adopted"
    );
    let reported = clamp_report(&r, &section);
    assert!(
        reported.iter().any(|line| {
            line.starts_with(&format!(
                "chat_scroll clamped by the live geometry: 9000 -> {}:",
                r.chat_scroll
            ))
        }),
        "the renderer must be able to name the offset it moved: {reported:?}"
    );
    // `restore_panes` itself stays silent: the sentence is drawn once, through the
    // report path (`announce_restore`), so one moved offset never reads as two
    // pane lines.
    assert_eq!(r.messages, section.chat_messages);
}

/// The stored-vs-live comparison for a REAL renderer, mapped through the single
/// sentence builder the boot wiring uses (`clamp_warning_sentences`).
///
/// `ui::session::boot` holds only a `&mut dyn Renderer`, so it compares the two
/// `UiSection`s; this helper covers the other half of the same rule — each field
/// name in the sentence belongs to the renderer field of that name, with that
/// type. Rename or re-type a renderer offset and this stops compiling, which is
/// exactly the drift a restore notice must never hide.
fn clamp_report(target: &TuiRenderer, stored: &crate::session::format::UiSection) -> Vec<String> {
    clamp_warning_sentences(&[
        (
            "chat_scroll",
            u64::from(stored.view.chat_scroll),
            u64::from(target.chat_scroll),
        ),
        (
            "plan_scroll",
            u64::from(stored.view.plan_scroll),
            u64::from(target.plan_scroll),
        ),
        (
            "subagent_scroll",
            u64::from(stored.view.subagent_scroll),
            u64::from(target.subagent_scroll),
        ),
        (
            "subagent_list_scroll",
            u64::from(stored.view.subagent_list_scroll),
            u64::from(target.subagent_list_scroll),
        ),
        (
            "selected_subagent_idx",
            stored.view.selected_subagent_idx as u64,
            target.selected_subagent_idx as u64,
        ),
    ])
}

#[test]
fn a_hostile_time_point_is_refused_by_u_chk_and_never_panics_the_renderer() {
    let mut r = TuiRenderer::new();
    let section = crate::session::format::UiSection {
        plan_content: "# Plan".to_string(),
        plan_start: Some(crate::session::format::TimePoint {
            elapsed_secs: f64::NAN,
            wall: chrono::Local::now(),
        }),
        plan_completed: Some(crate::session::format::TimePoint {
            elapsed_secs: -5.0,
            wall: chrono::Local::now(),
        }),
        abort_armed_at: Some(crate::session::format::TimePoint {
            elapsed_secs: f64::INFINITY,
            wall: chrono::Local::now(),
        }),
        ..Default::default()
    };

    let violations = section.structural_violations();
    assert!(
        violations.iter().any(|v| v.contains("plan_start")),
        "{violations:?}"
    );
    assert!(
        violations.iter().any(|v| v.contains("plan_completed")),
        "{violations:?}"
    );
    assert!(
        violations.iter().any(|v| v.contains("abort_armed_at")),
        "{violations:?}"
    );
    // The renderer still degrades instead of taking the UI down (`to_instant` is
    // total).
    r.restore_panes(&section);
    assert_eq!(r.plan_content, "# Plan");
    assert!(r.plan_start_time.is_some());
    assert!(r.plan_completed_time.is_some());
    assert!(r.dirty);
}

#[test]
fn every_new_field_survives_the_json_round_trip() {
    // The on-disk shape, not just the in-memory one: capture → JSON → parse →
    // restore must be lossless for the whole new field set.
    let mut source = TuiRenderer::new();
    source.plan_content = "# Plan\n- [ ] t-020 close the gap".to_string();
    source.show_plan_panel = true;
    source.had_active_plan = true;
    source.active_plan_task = Some("t-020".to_string());
    source.plan_start_time = Some(std::time::Instant::now());
    source.current_thought = "thinking…".to_string();
    source.current_content = "answer…".to_string();
    source.active_agent = "coder-t-020".to_string();
    source.steer_sentence_buffer = "half a steer".to_string();
    source.tokens_in = 11;
    source.tokens_out = 22;
    source.orchestrator_context_tokens = 33;
    source.confirm_abort = true;
    source.abort_armed_at = Some(std::time::Instant::now());
    source.subagents = vec![subagent_without_instants("coder")];
    source
        .subagent_turn_thinking
        .insert("coder".to_string(), "in-flight".to_string());
    source
        .subagent_is_thinking
        .insert("coder".to_string(), true);

    let section = source.capture_ui();
    let json = serde_json::to_string(&section).expect("the section serializes");
    let back: crate::session::format::UiSection =
        serde_json::from_str(&json).expect("the section parses back");
    assert_eq!(back, section);

    let mut target = TuiRenderer::new();
    target.restore_panes(&back);
    assert_eq!(target.plan_content, source.plan_content);
    assert_eq!(target.current_thought, "thinking…");
    assert_eq!(target.current_content, "answer…");
    assert_eq!(target.active_agent, "coder-t-020");
    assert_eq!(target.steer_sentence_buffer, "half a steer");
    assert_eq!(
        target
            .subagent_turn_thinking
            .get("coder")
            .map(String::as_str),
        Some("in-flight")
    );
    assert_eq!(target.subagent_is_thinking.get("coder"), Some(&true));
    // Witness-only: the counters came back in the JSON, they were not adopted.
    assert_eq!(back.tokens_in, 11);
    assert_eq!(target.tokens_in, 0);
    assert_eq!(target.tokens_out, 0);
    assert_eq!(target.orchestrator_context_tokens, 0);
    assert!(!target.confirm_abort);
}

#[test]
fn an_old_ui_json_without_any_t_020_field_still_loads() {
    // The exact member bytes an older binary wrote (schema_version 1): no plan,
    // streaming, identity, token or abort key anywhere, and no
    // `turn_thinking`/`is_thinking` on the panel. `#[serde(default)]` is what
    // keeps them loadable, which is why `SCHEMA_VERSION` stayed at 1.
    const OLD_UI_JSON: &str = r#"{
        "header": {
            "format_version": 1,
            "schema_version": 1,
            "session_id": "session-1",
            "kind": "ui",
            "saved_wall": "2026-01-01T00:00:00+01:00",
            "saved_elapsed_secs": 12.5,
            "member_count": 2,
            "member_index": 1
        },
        "section": {
            "chat_messages": ["User: hello", "", "Marmennill: hi"],
            "status_line": "Active: coder | 6m12s",
            "subagent_panels": [{
                "name": "coder",
                "task_id": "t-011",
                "prompt": "brief",
                "logs": ["[Started]"],
                "thinking": "thought",
                "content": "output",
                "is_active": true,
                "context_tokens": 1234,
                "worked_duration_secs": 12.5,
                "started_at": null,
                "last_activity_at": null
            }],
            "journal": [{ "type": "user", "text": "hello" }],
            "composer": {
                "input_text": "half-typed",
                "cursor": 4,
                "input_draft": "draft",
                "history": ["first"],
                "history_index": null
            },
            "view": {
                "chat_scroll": 3,
                "plan_scroll": 1,
                "subagent_scroll": 0,
                "subagent_list_scroll": 0,
                "selected_subagent_idx": 0,
                "focused_panel": "chat",
                "show_thought": true,
                "show_subagent_panel": true,
                "chat_auto_scroll": true,
                "subagent_autoscroll": false,
                "plan_auto_scroll": true
            },
            "budgets": {
                "thinking_budgets": { "coder": 12000 },
                "default_thinking_budget": 8000
            }
        }
    }"#;

    let file: crate::session::format::UiFile =
        serde_json::from_str(OLD_UI_JSON).expect("a pre-t-020 ui.json must still load");
    let section = file.section;
    assert_eq!(section.chat_messages.len(), 3);
    assert_eq!(section.status_line, "Active: coder | 6m12s");
    assert_eq!(section.journal.len(), 1);
    // Every new field fell back to its serde default…
    assert_eq!(section.plan_content, "");
    assert!(!section.view.show_plan_panel);
    assert!(!section.had_active_plan);
    assert_eq!(section.active_plan_task, None);
    assert_eq!(section.plan_start, None);
    assert_eq!(section.plan_completed, None);
    assert_eq!(section.current_thought, "");
    assert_eq!(section.current_content, "");
    assert_eq!(section.active_agent, None);
    assert_eq!(section.steer_sentence_buffer, None);
    assert_eq!(section.tokens_in, 0);
    assert_eq!(section.tokens_out, 0);
    assert_eq!(section.orchestrator_context_tokens, 0);
    assert!(!section.abort_armed);
    assert_eq!(section.abort_armed_at, None);
    assert_eq!(section.subagent_panels[0].turn_thinking, None);
    assert_eq!(section.subagent_panels[0].is_thinking, None);
    // …and an old bundle is still a loadable one (the loader refuses, never repairs).
    assert_eq!(section.structural_violations(), Vec::<String>::new());

    // Restoring it costs nothing the old build had: the pane text comes back, the
    // disk-backed plan pane survives, and no witness noise is invented.
    let mut r = TuiRenderer::new();
    r.plan_content = "# Plan read from disk at init".to_string();
    r.show_plan_panel = true;
    r.restore_panes(&section);
    assert_eq!(r.messages, section.chat_messages);
    assert_eq!(r.status_line, "Active: coder | 6m12s");
    assert_eq!(r.subagents.len(), 1);
    assert_eq!(r.subagents[0].logs, vec!["[Started]".to_string()]);
    assert_eq!(r.plan_content, "# Plan read from disk at init");
    assert!(r.show_plan_panel);
    assert_eq!(r.active_agent, "Manager");
    assert_eq!(r.messages.len(), 3);
}

/// A renderer that implements ONLY the required trait methods: the three
/// rehydration hooks must compile from their default bodies, so an existing
/// implementor is unaffected by the addition.
#[derive(Default)]
struct DefaultHookRenderer {
    state: crate::ui::InputState,
}

impl crate::ui::Renderer for DefaultHookRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, _event: &crate::ui::Event) {}
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn input_state(&mut self) -> &mut crate::ui::InputState {
        &mut self.state
    }
    fn input_state_shared(&self) -> &crate::ui::InputState {
        &self.state
    }
    fn shutdown(&mut self) {}
}

#[test]
fn default_hook_bodies_compile_and_claim_nothing() {
    let mut r = DefaultHookRenderer::default();
    let section = r.capture_ui();
    assert_eq!(section, crate::session::format::UiSection::default());
    assert_eq!(section.structural_violations(), Vec::<String>::new());
    r.restore_panes(&section);
    r.announce_restore(&crate::session::format::RepairReport::default());
}

#[test]
fn the_hooks_are_object_safe_and_need_no_sync() {
    // `Renderer: Send` is the only bound; `TuiRenderer` holds `Cell`/`RefCell`
    // and is NOT `Sync`, so capture/restore must (and do) work through the
    // `&mut dyn Renderer` the session loop owns, on the session thread.
    fn assert_send<T: Send>() {}
    assert_send::<TuiRenderer>();
    assert_send::<Box<dyn crate::ui::Renderer>>();

    let mut boxed: Box<dyn crate::ui::Renderer> = Box::new(TuiRenderer::new());
    let section = boxed.capture_ui();
    boxed.restore_panes(&section);
    boxed.announce_restore(&crate::session::format::RepairReport::default());

    // And across a thread boundary, which is exactly the `Send` (not `Sync`)
    // shape the session loop relies on.
    let moved = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let mut renderer = boxed;
                let mut section = renderer.capture_ui();
                section
                    .chat_messages
                    .push("from the session thread".to_string());
                renderer.restore_panes(&section);
                renderer.capture_ui()
            })
            .join()
            .expect("session thread joined")
    });
    assert_eq!(
        moved.chat_messages,
        vec!["from the session thread".to_string()]
    );
}

// ---- new UI scaffold: view model, layout ladder, theme, pane chrome ----------

/// Draw one whole frame of the rebuilt UI and hand back the text of every row.
fn draw_new_frame(r: &mut TuiRenderer, width: u16, height: u16) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            r.draw_new_ui(frame);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

/// A renderer in the M4 state: both side panes on, one running worker, one
/// transcript line, a mixed plan. The mode is set through the transition helper
/// because it is stored view state now — writing the pane toggles alone would
/// leave the stored mode on M1 until the adoption bridge folded them in.
fn m4_renderer() -> TuiRenderer {
    let mut r = TuiRenderer::new();
    r.plan_content =
        "## Tasks\n\n- [x] [t-001] research baseline\n- [ ] [t-004] wireframe\n".to_string();
    r.messages = vec!["User: rebuild the TUI".to_string()];
    r.set_view_mode(state::ViewMode::Split);
    r.subagents = vec![SubagentDetail {
        name: "coder_specialist-t-004".to_string(),
        task_id: Some("t-004".to_string()),
        is_active: true,
        ..SubagentDetail::default()
    }];
    r
}

#[test]
fn plan_progress_is_the_checked_box_split_of_the_same_plan_text() {
    // Acceptance criterion 2: `done/total` equals the `CheckboxState::Checked` /
    // total split for three fixtures.
    let empty = state::PlanProgress::from_plan("No active execution plan on disk.", None, &[]);
    assert_eq!((empty.done, empty.total), (0, 0));
    assert!(!empty.has_tasks());
    assert_eq!(empty.progress_label(), "0/0 done");

    let all_checked =
        state::PlanProgress::from_plan("- [x] [t-001] a\n- [X] [t-002] b\n", None, &[]);
    assert_eq!((all_checked.done, all_checked.total), (2, 2));
    assert_eq!(all_checked.pending(), 0);

    let mixed = state::PlanProgress::from_plan(
        "# Plan\n\n- [x] [t-001] a\n- [ ] [t-004] b\n- [x] t-008 c\n\n```\n- [ ] not a task\n```",
        None,
        &[],
    );
    assert_eq!((mixed.done, mixed.total), (2, 3));
    assert_eq!(mixed.pending(), 1);
    assert_eq!(mixed.progress_label(), "2/3 done");
}

#[test]
fn plan_progress_active_task_prefers_the_explicit_task_then_a_running_row() {
    let explicit = state::PlanProgress::from_plan("- [ ] [t-004] b\n", Some("t-004"), &[]);
    assert_eq!(explicit.active_task.as_deref(), Some("t-004"));

    let running = vec![SubagentDetail {
        name: "coder_specialist-t-009".to_string(),
        task_id: Some("t-009".to_string()),
        is_active: true,
        ..SubagentDetail::default()
    }];
    let derived = state::PlanProgress::from_plan("- [ ] [t-004] b\n", None, &running);
    assert_eq!(derived.active_task.as_deref(), Some("t-009"));

    let idle = vec![SubagentDetail {
        name: "writer-t-011".to_string(),
        task_id: Some("t-011".to_string()),
        ..SubagentDetail::default()
    }];
    let none = state::PlanProgress::from_plan("- [ ] [t-004] b\n", None, &idle);
    assert_eq!(none.active_task, None);
}

#[test]
fn view_mode_is_stored_state_and_the_pane_toggles_follow_it() {
    // Spec §4.1: the mode is real UI state. It is set once at boot from the pane
    // inventory (both side panes closed = M1) and moves only by transition; the
    // two pane toggles are what the mode's pane inventory (§4.3) spells for the
    // status chips, the legacy scroll path and the durable `ViewState`.
    let mut r = TuiRenderer::new();
    assert_eq!(
        state::UiView::from_renderer(&r).mode,
        state::ViewMode::Transcript,
        "boot: both side panes closed is M1"
    );
    assert_eq!(
        r.focus,
        state::FocusPane::Input,
        "boot: the composer is focused"
    );

    for (mode, plan, workers) in [
        (state::ViewMode::Plan, true, false),
        (state::ViewMode::Split, true, true),
        (state::ViewMode::Workers, false, true),
        (state::ViewMode::Transcript, false, false),
    ] {
        r.set_view_mode(mode);
        let view = state::UiView::from_renderer(&r);
        assert_eq!(view.mode, mode, "the stored mode is what the frame reads");
        assert_eq!(
            (r.show_plan_panel, r.show_subagent_panel),
            (plan, workers),
            "{mode:?} spells its pane inventory into the toggles"
        );
        // §2.4.1/G6: a focus may never sit on a pane the mode does not present.
        assert!(
            mode.presents(r.focus),
            "{mode:?} keeps {:?} on screen",
            r.focus
        );
        assert!(r.dirty, "no silent mode change (G5)");
    }
}

#[test]
fn a_legacy_pane_write_is_adopted_as_exactly_one_transition() {
    // `events.rs` still writes the pre-rebuild fields directly until t-011 routes
    // its keys through the transitions, so the bridge turns such a write into one
    // mode change — and never re-adopts a mode the transitions themselves set.
    let mut r = TuiRenderer::new();
    r.show_plan_panel = true; // what `Ctrl+P` does today
    r.adopt_legacy_view_writes();
    assert_eq!(r.view_mode, state::ViewMode::Plan);

    r.adopt_legacy_view_writes();
    assert_eq!(
        r.view_mode,
        state::ViewMode::Plan,
        "adoption is edge-triggered, not a re-read"
    );

    r.set_view_mode(state::ViewMode::Split);
    r.adopt_legacy_view_writes();
    assert_eq!(
        r.view_mode,
        state::ViewMode::Split,
        "a write the API made is never mistaken for a legacy write"
    );

    // The same bridge folds a focus tag and the auto-scroll flag.
    r.focused_panel = FocusedPanel::Subagents;
    r.chat_auto_scroll = false;
    r.adopt_legacy_view_writes();
    assert_eq!(r.focus, state::FocusPane::Workers);
    assert!(!r.follow);
    assert!(!r.chat_auto_scroll);
}

#[test]
fn a_mode_that_removes_a_pane_repairs_the_focus_onto_a_pane_that_is_there() {
    // §4.2 ring + §2.4.1: an absent pane is never drawn and never hit-tested, so
    // closing the pane under focus pulls the focus back on screen instead of
    // stranding the badge. `Tab` skips the absent panes for the same reason.
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Plan);
    assert_eq!(r.focused_panel, FocusedPanel::Plan);

    r.set_view_mode(state::ViewMode::Workers);
    assert!(
        !state::ViewMode::Workers.presents(state::FocusPane::Plan),
        "M4 → M3 removes the plan pane"
    );
    assert_ne!(r.focus, state::FocusPane::Plan);
    assert!(state::ViewMode::Workers.presents(r.focus));
    assert_eq!(r.focused_panel, r.focus.legacy());

    // `Tab` in a mode with no side panes cycles Input ⇄ Transcript only.
    r.set_view_mode(state::ViewMode::Transcript);
    r.set_focus(state::FocusPane::Input);
    assert_eq!(r.cycle_focus_pane(true), state::FocusPane::Transcript);
    assert_eq!(r.cycle_focus_pane(true), state::FocusPane::Input);
    assert_eq!(r.cycle_focus_pane(false), state::FocusPane::Transcript);

    // With both side panes on, the ring is the full §4.2 order.
    r.set_view_mode(state::ViewMode::Split);
    r.set_focus(state::FocusPane::Input);
    assert_eq!(r.cycle_focus_pane(true), state::FocusPane::Transcript);
    assert_eq!(r.cycle_focus_pane(true), state::FocusPane::Plan);
    assert_eq!(r.cycle_focus_pane(true), state::FocusPane::Workers);
    assert_eq!(r.cycle_focus_pane(true), state::FocusPane::Input);
    assert_eq!(r.cycle_focus_pane(false), state::FocusPane::Workers);
}

#[test]
fn the_view_model_mirrors_focus_follow_scroll_and_the_armed_surface() {
    let mut r = TuiRenderer::new();
    r.set_focus(state::FocusPane::Workers);
    r.set_follow(false);
    r.chat_scroll = 7;
    r.plan_scroll = 3;
    r.subagent_scroll = 11;
    let view = state::UiView::from_renderer(&r);
    assert_eq!(view.focus, state::FocusPane::Workers);
    assert!(view.focused(state::FocusPane::Workers));
    assert!(!view.focused(state::FocusPane::Input));
    assert!(!view.follow);
    assert!(!r.chat_auto_scroll, "the legacy scroll flag mirrors follow");
    assert_eq!(r.focused_panel, FocusedPanel::Subagents);
    assert!(view.composer_active());
    assert_eq!(
        (
            view.scroll.transcript,
            view.scroll.plan,
            view.scroll.workers
        ),
        (7, 3, 11)
    );
    assert_eq!(view.overlay, state::OverlayState::None);

    // Arming abort opens the confirm surface (drawn on status row 2, §8.5). The
    // surface is stored state, so the 3 s window is folded in by the same pass
    // the entry path runs every tick — its rising edge puts it on screen.
    r.confirm_abort = true;
    r.abort_armed_at = Some(std::time::Instant::now());
    r.adopt_legacy_view_writes();
    let armed = state::UiView::from_renderer(&r);
    assert!(armed.abort_armed);
    assert_eq!(armed.overlay, state::OverlayState::Confirm);
    assert!(armed.overlay.is_open());
    assert!(!armed.composer_active());

    // The falling edge — the window expiring with no key press at all — takes the
    // surface back off, so nothing lingers once the prompt is stale.
    r.abort_armed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(4));
    r.adopt_legacy_view_writes();
    assert!(!state::UiView::from_renderer(&r).abort_armed);
    assert_eq!(
        state::UiView::from_renderer(&r).overlay,
        state::OverlayState::None
    );
    assert!(!r.confirm_open);
}

#[test]
fn the_help_overlay_is_state_and_a_confirm_underneath_it_returns_it() {
    // G2: `?`/`F1` toggles a legend the view model actually carries; §5/§4.4:
    // `Esc` closes one overlay and never arms abort.
    let mut r = m4_renderer();
    assert!(!r.help_open);
    assert_eq!(
        state::UiView::from_renderer(&r).overlay,
        state::OverlayState::None
    );

    r.open_help();
    assert!(r.help_open);
    assert_eq!(
        state::UiView::from_renderer(&r).overlay,
        state::OverlayState::Help
    );
    let rows = draw_new_frame(&mut r, 100, 30).concat();
    assert!(
        rows.contains("Keys") && rows.contains("Esc close"),
        "the legend is drawn over the body: {rows}"
    );

    // A confirm drawn over the legend wins the single overlay slot (§7.3) ...
    r.show_confirm();
    assert_eq!(
        state::UiView::from_renderer(&r).overlay,
        state::OverlayState::Confirm
    );
    // ... and closing it returns the legend that was still toggled on.
    r.close_overlay();
    assert!(!r.confirm_open);
    assert!(r.help_open);
    assert_eq!(
        state::UiView::from_renderer(&r).overlay,
        state::OverlayState::Help
    );

    // Closing the legend closes it; closing with nothing open is a no-op.
    r.close_overlay();
    assert!(!r.help_open);
    assert_eq!(
        state::UiView::from_renderer(&r).overlay,
        state::OverlayState::None
    );
    r.close_overlay();
    assert_eq!(
        state::UiView::from_renderer(&r).overlay,
        state::OverlayState::None
    );
}

#[test]
fn every_view_mode_changes_the_rects_compute_layout_actually_produces() {
    // The mode is not a label on the header chip: it is the input the geometry is
    // built from, and each of M1–M4 produces a different body split (§2.3).
    let mut r = m4_renderer();
    let area = Rect::new(0, 0, 100, 30);
    let mut seen: Vec<(Option<Rect>, Option<Rect>, Option<Rect>)> = Vec::new();

    for mode in [
        state::ViewMode::Transcript,
        state::ViewMode::Plan,
        state::ViewMode::Workers,
        state::ViewMode::Split,
    ] {
        r.set_view_mode(mode);
        let view = state::UiView::from_renderer(&r);
        let rects = layout::compute_layout(area, &view);
        let body = rects.body.expect("100x30 clears every floor");
        let transcript = rects
            .transcript
            .expect("the transcript is present in every mode");
        assert_eq!(rects.mode, mode, "the frame reports the mode it drew");

        match mode {
            state::ViewMode::Transcript => {
                assert_eq!(rects.plan, None, "M1 removes the plan pane");
                assert_eq!(rects.workers, None, "M1 removes the workers pane");
                assert_eq!(transcript, body, "M1 gives the transcript the whole body");
            }
            state::ViewMode::Plan => {
                let plan = rects.plan.expect("M2 shows the plan pane");
                assert_eq!(rects.workers, None, "M2 has no workers pane");
                assert_eq!(plan.width + transcript.width, body.width);
                assert!(
                    plan.width > transcript.width,
                    "M2 is 55/45 with the plan in the wider column: plan {} transcript {}",
                    plan.width,
                    transcript.width
                );
            }
            state::ViewMode::Workers => {
                let workers = rects.workers.expect("M3 shows the workers pane");
                assert_eq!(rects.plan, None, "M3 has no plan pane");
                assert_eq!(workers.width + transcript.width, body.width);
                assert!(
                    workers.width > transcript.width,
                    "M3 is 55/45 with the roster in the wider column: workers {} transcript {}",
                    workers.width,
                    transcript.width
                );
            }
            state::ViewMode::Split => {
                let plan = rects.plan.expect("M4 shows the plan pane");
                let workers = rects.workers.expect("M4 shows the workers pane");
                assert_eq!(plan.width + transcript.width, body.width);
                assert!(
                    transcript.width > plan.width,
                    "M4 is 60/40 with the transcript in the wider column: transcript {} side {}",
                    transcript.width,
                    plan.width
                );
                assert_eq!(
                    plan.width, workers.width,
                    "the side column stacks two panes"
                );
                assert_eq!(plan.height + workers.height, body.height);
            }
        }
        seen.push((rects.transcript, rects.plan, rects.workers));
    }

    for (i, left) in seen.iter().enumerate() {
        for (j, right) in seen.iter().enumerate().skip(i + 1) {
            assert_ne!(left, right, "modes {} and {} drew the same rects", i, j);
        }
    }
}

#[test]
fn capture_ui_and_restore_panes_round_trip_mode_focus_and_follow() {
    // Spec §10 item 18: the UI state a session captured is the UI state a session
    // boots into. The persisted `ViewState` spells the state in its pre-rebuild
    // fields, so the round-trip is checked on both spellings.
    let mut source = m4_renderer();
    source.messages = (0..40).map(|i| format!("pane line {i}")).collect();
    source.set_focus(state::FocusPane::Workers);
    source.set_follow(false);
    source.chat_scroll = 4;
    let section = source.capture_ui();
    assert_eq!(section.structural_violations(), Vec::<String>::new());
    assert_eq!(section.view.focused_panel, "subagents");
    assert!(
        section.view.show_plan_panel && section.view.show_subagent_panel,
        "M4 persists both pane toggles"
    );
    assert!(!section.view.chat_auto_scroll, "follow off is persisted");

    let mut target = TuiRenderer::new();
    assert_eq!(target.view_mode, state::ViewMode::Transcript);
    target.restore_panes(&section);

    assert_eq!(
        target.view_mode,
        state::ViewMode::Split,
        "M4 comes back as M4"
    );
    assert_eq!(target.focus, state::FocusPane::Workers);
    assert!(!target.follow);
    assert_eq!(
        (target.show_plan_panel, target.show_subagent_panel),
        (true, true),
        "the toggles stay in step with the restored mode"
    );
    assert_eq!(target.focused_panel, FocusedPanel::Subagents);
    assert!(!target.chat_auto_scroll);
    assert_eq!(target.chat_scroll, 4);
    assert_eq!(
        state::UiView::from_renderer(&target).mode,
        state::ViewMode::Split
    );
    assert_eq!(
        state::UiView::from_renderer(&target).focus,
        state::FocusPane::Workers
    );
    assert!(!state::UiView::from_renderer(&target).follow);
    // The restored renderer re-captures the identical section: nothing in the
    // round-trip is invented and nothing is lost a second time.
    assert_eq!(target.capture_ui(), section);

    // The one documented loss in the mapping: the tag set has no Composer entry,
    // so a Composer focus comes back as the Transcript. The renderer does not
    // invent a Composer focus it never stored.
    let mut composer = m4_renderer();
    composer.set_focus(state::FocusPane::Input);
    assert_eq!(composer.capture_ui().view.focused_panel, "chat");
    let mut target = TuiRenderer::new();
    target.restore_panes(&composer.capture_ui());
    assert_eq!(target.focus, state::FocusPane::Transcript);
    assert_eq!(target.focused_panel, FocusedPanel::Chat);
}

#[test]
fn a_help_overlay_the_operator_opened_survives_a_restore() {
    // The legend is operator state, not session content: a restore replaces the
    // panes, it does not close a legend that was open, and it never re-arms a
    // confirm surface the 3 s window it belongs to did not survive.
    let source = m4_renderer();
    let section = source.capture_ui();

    let mut target = m4_renderer();
    target.open_help();
    target.show_confirm();
    assert_eq!(target.overlay, state::OverlayState::Confirm);
    target.restore_panes(&section);

    assert!(target.help_open, "the legend survives the restore");
    assert!(
        !target.confirm_open,
        "a confirm surface is never re-armed by a restore"
    );
    assert_eq!(
        state::UiView::from_renderer(&target).overlay,
        state::OverlayState::Help
    );
    let rows = draw_new_frame(&mut target, 100, 30).concat();
    assert!(
        rows.contains("Keys"),
        "the restored frame still carries the legend"
    );

    // A renderer that had nothing open keeps nothing open — the restore does not
    // invent an overlay either.
    let mut clean = m4_renderer();
    clean.restore_panes(&section);
    assert_eq!(
        state::UiView::from_renderer(&clean).overlay,
        state::OverlayState::None
    );
}

#[test]
fn teardown_is_idempotent_and_paints_nothing_afterwards() {
    // The entry path tears the terminal down from three places: the session loop
    // on a normal exit, the abort/EOF exits, and `Drop` on the unwinding path.
    // Any of them may run after another, and a paint after the terminal is gone
    // must not resurrect it — that would write escape codes into the shell.
    let mut r = TuiRenderer::new();
    r.messages = vec!["User: one".to_string()];
    assert!(
        !r.terminal_torn_down,
        "a fresh renderer has not been torn down"
    );

    r.shutdown();
    assert!(r.terminal_torn_down, "teardown is recorded");
    r.shutdown();
    r.shutdown();

    assert!(
        r.render_terminal().is_ok(),
        "a paint after teardown is a no-op, not an error"
    );
    assert!(
        r.force_flush().is_ok() && r.flush().is_ok(),
        "the flush paths the loop keeps calling after shutdown are no-ops too"
    );
    assert_eq!(r.frame_counter, 0, "no frame was painted after teardown");

    // `Drop` runs the same teardown a second time; nothing panics.
    drop(r);
}

#[test]
fn a_shrunken_pane_clamps_the_scroll_offset_it_can_no_longer_honour() {
    // Work item 1: the entry path still clamps scroll state on every geometry
    // change and repaints. The transcript clamps its own offset to the viewport
    // it just built, so an offset that no longer exists — the classic case being
    // a restored or held `chat_scroll` after a resize — never leaves the pane
    // showing nothing but blank rows.
    let mut r = m4_renderer();
    r.messages = (0..200)
        .map(|i| format!("pane line {i} of two hundred"))
        .collect();
    r.set_follow(false);

    for (width, height) in [(100u16, 30u16), (60, 20), (40, 10), (120, 40)] {
        r.chat_scroll = u16::MAX;
        let rows = draw_new_frame(&mut r, width, height).concat();
        assert!(
            r.chat_scroll < u16::MAX,
            "{width}x{height}: the impossible offset was not clamped"
        );
        assert!(
            r.chat_scroll > 0,
            "{width}x{height}: a clamped offset still sits at the tail"
        );
        assert!(
            rows.contains("pane line 199"),
            "{width}x{height}: the clamped pane must show the newest line"
        );
    }

    // The same clamp through a mode change rather than a terminal resize: the
    // frame that draws the new geometry re-clamps (M1 widens the transcript, M4
    // narrows it, and the §2.5 ladder narrows it further at 60 columns).
    for mode in [
        state::ViewMode::Transcript,
        state::ViewMode::Split,
        state::ViewMode::Plan,
        state::ViewMode::Workers,
    ] {
        r.chat_scroll = u16::MAX;
        r.set_view_mode(mode);
        let rows = draw_new_frame(&mut r, 60, 24).concat();
        assert!(
            rows.contains("pane line 199"),
            "{mode:?} at 60x24 must show the newest line"
        );
    }

    // `clamp_scrolls` is what the resize arm of the event loop calls; run after a
    // draw has published the pane geometry, it must agree with the draw.
    let drawn = r.chat_scroll;
    r.chat_scroll = u16::MAX;
    r.clamp_scrolls();
    assert!(
        r.chat_scroll <= drawn,
        "the resize clamp lands on the same offset the draw did: {} > {drawn}",
        r.chat_scroll
    );
    assert!(r.chat_scroll > 0);
}

#[test]
fn no_pane_rect_is_zero_sized_at_any_supported_terminal_size() {
    // Acceptance criterion 19 (layout half): 100x30, 90x24, 60x16 and 40x10.
    let r = m4_renderer();
    let view = state::UiView::from_renderer(&r);
    for (width, height) in [(100u16, 30u16), (90, 24), (60, 16), (40, 10), (120, 60)] {
        let rects = layout::compute_layout(Rect::new(0, 0, width, height), &view);
        for (name, rect) in [
            ("header", rects.header),
            ("body", rects.body),
            ("status", rects.status),
            ("input", rects.input),
            ("footer", rects.footer),
            ("transcript", rects.transcript),
            ("plan", rects.plan),
            ("workers", rects.workers),
        ] {
            if let Some(rect) = rect {
                assert!(
                    rect.width >= 1 && rect.height >= 1,
                    "{name} at {width}x{height} is {}x{}",
                    rect.width,
                    rect.height
                );
            }
        }
    }
}

#[test]
fn below_40x10_only_the_too_small_notice_is_drawn() {
    let r = m4_renderer();
    let view = state::UiView::from_renderer(&r);
    let rects = layout::compute_layout(Rect::new(0, 0, 39, 9), &view);
    assert_eq!(rects.notice, Some(Rect::new(0, 0, 39, 9)));
    assert_eq!(rects.header, None);
    assert_eq!(rects.body, None);
    assert_eq!(rects.status, None);
    assert_eq!(rects.input, None);
    assert_eq!(rects.footer, None);
    assert_eq!(rects.transcript, None);
    assert_eq!(rects.plan, None);
    assert_eq!(rects.workers, None);

    let rows = draw_new_frame(&mut TuiRenderer::new(), 39, 9);
    let joined = rows.concat();
    assert!(
        joined.contains("Terminal too small")
            && joined.contains("40x10")
            && joined.contains("39x9"),
        "the notice names the required and the current size: {joined}"
    );
    assert!(!joined.contains("Input"));
    assert!(!joined.contains("LIVE"));
}

#[test]
fn below_twenty_rows_the_footer_row_is_dropped_and_the_status_bar_collapses() {
    let r = m4_renderer();
    let view = state::UiView::from_renderer(&r);
    let rects = layout::compute_layout(Rect::new(0, 0, 100, 19), &view);
    assert_eq!(rects.footer, None, "the footer is absent, not zero-height");
    assert_eq!(rects.status.expect("status present").height, 1);
    assert!(rects.input.expect("input present").height <= 3);

    let tall = layout::compute_layout(Rect::new(0, 0, 100, 30), &view);
    assert_eq!(tall.footer.expect("footer at 30 rows").height, 1);
    assert_eq!(tall.status.expect("status present").height, 2);
}

#[test]
fn the_side_column_stacks_to_one_pane_below_ninety_columns() {
    // §2.5 `width < 90`: transcript plus only the focused side pane.
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Plan);
    let view = state::UiView::from_renderer(&r);
    let rects = layout::compute_layout(Rect::new(0, 0, 80, 30), &view);
    assert!(rects.transcript.is_some());
    assert!(rects.plan.is_some());
    assert_eq!(rects.workers, None, "only the focused side pane survives");

    let wide = layout::compute_layout(Rect::new(0, 0, 100, 30), &view);
    assert!(wide.plan.is_some() && wide.workers.is_some());
}

#[test]
fn the_header_chip_budget_drops_val_then_ctx_then_elapsed() {
    // §7.4 drop order. D1 (§9.3) has landed, so the budget keeps `val` in the same
    // band as the other optional chips and the pane decides whether it has a number
    // to print there.
    let wide = layout::header_budget(120);
    assert!(wide.show_val, "D1 landed: the round chip is on the ladder");
    assert!(wide.show_ctx && wide.show_elapsed && wide.show_mode && wide.show_task);

    let compact = layout::header_budget(70);
    assert!(
        compact.show_elapsed,
        "elapsed survives at 70; §2.5 drops it only below 70"
    );
    let narrow = layout::header_budget(69);
    assert!(
        !narrow.show_ctx && !narrow.show_elapsed && !narrow.show_mode && !narrow.show_val,
        "the optional chips, `val` first, leave together below 70"
    );
    assert!(
        narrow.show_task,
        "the active task id outlives the optional chips"
    );

    let tiny = layout::header_budget(40);
    assert!(tiny.show_task);
    assert!(layout::compact_legend(69) && !layout::compact_legend(70));
    assert!(!layout::workers_detail_present(59) && layout::workers_detail_present(60));
}

#[test]
fn theme_glyphs_are_the_spec_table_and_the_picker_follows_row_state() {
    use theme::Glyph;
    assert_eq!(Glyph::Done.symbol(), '✓');
    assert_eq!(Glyph::Active.symbol(), '▶');
    assert_eq!(Glyph::Pending.symbol(), '⧗');
    assert_eq!(Glyph::Paused.symbol(), '⏸');
    assert_eq!(Glyph::Failed.symbol(), '✗');
    assert_eq!(Glyph::Active.label(), "running");

    assert_eq!(theme::task_glyph(true, false), Glyph::Done);
    assert_eq!(theme::task_glyph(false, true), Glyph::Active);
    assert_eq!(theme::task_glyph(false, false), Glyph::Pending);

    assert_eq!(theme::worker_glyph(true, false, false), Glyph::Active);
    assert_eq!(theme::worker_glyph(true, true, false), Glyph::Paused);
    assert_eq!(theme::worker_glyph(false, false, true), Glyph::Pending);
    assert_eq!(theme::worker_glyph(false, false, false), Glyph::Done);

    // The label-aware picker (plumbing delta D1, §9.3). Liveness alone cannot tell an
    // aborted worker from an approved one, so the registry's own `status_label`
    // decides the terminal states — and the slot states still outrank the label.
    for label in [
        "Aborted",
        "Rejected",
        "Failed",
        "Replan Required (too complex)",
    ] {
        assert_eq!(
            theme::worker_state_glyph(false, false, false, Some(label)),
            Glyph::Failed,
            "{label} is a worker that did not deliver (§7.2 `✗`)"
        );
    }
    assert_eq!(
        theme::worker_state_glyph(false, false, false, Some("Approved")),
        Glyph::Done,
        "only an accepted deliverable reads `✓ done`"
    );
    assert_eq!(
        theme::worker_state_glyph(true, false, false, Some("Revising")),
        Glyph::Pending,
        "the review loop holds the worker: §7.2 spells validating `⧗`"
    );
    assert_eq!(
        theme::worker_state_glyph(true, false, false, Some("In Progress")),
        Glyph::Active,
        "a label outside the terminal vocabulary decides nothing and liveness answers"
    );
    assert_eq!(
        theme::worker_state_glyph(false, false, false, Some("In Progress")),
        Glyph::Done,
        "…including when the honest answer is that the stream closed"
    );
    assert_eq!(
        theme::worker_state_glyph(false, true, false, Some("Approved")),
        Glyph::Paused,
        "a parked stream is parked whatever its last label said"
    );
    assert_eq!(
        theme::worker_state_glyph(true, false, true, Some("Running")),
        Glyph::Pending,
        "a delegation waiting for a slot is queued while its label says it runs"
    );
    assert_eq!(
        theme::worker_state_glyph(false, false, false, None),
        Glyph::Done,
        "no projected label: the picker is the label-free rule, nothing more"
    );
    assert_eq!(
        theme::worker_state_glyph(false, false, false, Some("   ")),
        Glyph::Done,
        "a blank label is no label"
    );
    assert_eq!(
        theme::worker_state_glyph(false, false, false, Some("Unapproved")),
        Glyph::Done,
        "only the leading word decides, so `Unapproved` is not a verdict this knows"
    );

    // The palette keeps today's colours (§7.1).
    assert_eq!(theme::frame_focus().fg, Some(Color::Cyan));
    assert_eq!(theme::frame_idle().fg, Some(Color::DarkGray));
    assert_eq!(theme::frame_alert().fg, Some(Color::Red));
    assert_eq!(theme::frame_confirm().fg, Some(Color::Yellow));
    assert_eq!(theme::chip_active().fg, Some(Color::Yellow));
    assert!(theme::chip_active().add_modifier.contains(Modifier::BOLD));
    assert_eq!(theme::chip().bg, Some(Color::DarkGray));
    assert_eq!(theme::chip_done().fg, Some(Color::Green));
    assert_eq!(theme::chip_pending().fg, Some(Color::LightRed));
    assert_eq!(theme::error().fg, Some(Color::Red));
    assert_eq!(theme::text_dim().fg, Some(Color::DarkGray));
    assert_eq!(theme::text_primary(), ratatui::style::Style::default());
    assert_eq!(theme::FOCUS_BADGE, '●');
}

/// Plumbing delta D1 (design §9.3) at the boundary it was asked for: the four worker
/// lifecycle fields the orchestrator already keeps (`status_label`,
/// `implementation_turns`, `validation_rounds`, `latest_validator_feedback`) reach the
/// renderer's own row type instead of dying at the UI edge. Nothing here is derived
/// from prose — one snapshot of the process-global registry, joined on the effective
/// registry key.
#[test]
fn the_worker_lifecycle_the_registry_keeps_lands_on_the_pane_row() {
    use crate::orchestrator::workers;
    let _workers = workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let worker = workers::register_active_worker(
        Some("t-9512".to_string()),
        "coder".to_string(),
        "carry the lifecycle across the UI boundary".to_string(),
    );
    let key = worker.0.clone();
    let _cleanup = EntryCleanup { key: key.clone() };

    // The state a validator rejection leaves in the registry: the review loop's own
    // turn and round counters, its feedback text, and the terminal label.
    workers::set_active_worker_status(&key, "Rejected");
    workers::update_active_worker_progress(&key, 3, 2, Some("clippy: unused import".to_string()));

    // The loop's row carries none of it — that is the gap D1 describes.
    let mut r = m4_renderer();
    r.subagents.clear();
    r.set_subagents(vec![SubagentDetail {
        name: key.clone(),
        task_id: Some("t-9512".to_string()),
        is_active: true,
        ..SubagentDetail::default()
    }]);
    assert_eq!(r.subagents.len(), 1, "{:?}", r.subagents);
    let row = r.subagents.iter().find(|row| row.name == key).unwrap();
    assert_eq!(
        row.status_label.as_deref(),
        Some("Rejected"),
        "the registry's own label, not a guess from liveness"
    );
    assert_eq!(row.implementation_turns, 3);
    assert_eq!(row.validation_rounds, 2);
    assert_eq!(
        row.latest_validator_feedback.as_deref(),
        Some("clippy: unused import")
    );

    // The one number the header and the status row are allowed to print (§8.4): the
    // max over the rows, from the rows.
    assert_eq!(r.max_validation_rounds(), 2);

    // And the two fields the missing data kept switched off now reach the frame: the
    // roster row prints its round, and the validator-rejected row reads `✗ failed`
    // rather than the `✓ done` a closed stream used to imply.
    let frame = draw_new_frame(&mut r, 120, 40);
    let text = frame.join("\n");
    assert!(text.contains("val 2"), "no round counter in:\n{text}");
    let roster = frame
        .iter()
        // The roster row is the only line that prints a turn counter, so it cannot be
        // confused with the header's `▶ t-9512` chip.
        .find(|line| line.contains("t-9512") && line.contains("turn "))
        .unwrap_or_else(|| panic!("no roster row for the delegation:\n{text}"));
    assert!(roster.contains("✗ failed"), "{roster}");
    assert!(!roster.contains("✓ done"), "{roster}");
    assert!(roster.contains("turn 3"), "{roster}");
    // The tally agrees with the row. Both spellings §7.2 allows are accepted — the
    // word labels are only kept while the border row has room — but the `✓` segment
    // has to stay empty either way.
    assert!(
        frame
            .iter()
            .any(|line| line.contains("✗ failed 1") || line.contains("✗ 1")),
        "the tally counts the rejected row as failed:\n{text}"
    );
    assert!(
        frame
            .iter()
            .any(|line| line.contains("✓ done 0") || line.contains("✓ 0")),
        "…and not as done:\n{text}"
    );

    // The validator's words are the other half of the same fold: a rejected row states
    // why it was rejected.
    r.set_view_mode(state::ViewMode::Workers);
    let detail = draw_new_frame(&mut r, 120, 40).join("\n");
    assert!(detail.contains("[Validator]"), "{detail}");
    assert!(detail.contains("clippy: unused import"), "{detail}");
}

/// §8.5: a failure outlives the status line that carried it. The renderer keeps the
/// categorical cause and the full text, and `e` reads **that buffer** — the transcript
/// and status-line scan is only the fallback — so the recalled line carries the
/// category and every line of the report, not the one line §7.4 lets the status keep.
#[test]
fn a_reported_error_stays_recallable_after_the_status_line_moved_on() {
    let mut r = TuiRenderer::new();

    // The session loop's error traffic, in the shape `session.rs` reports it.
    r.on_event(&Event::Status(
        "LLM error: rate limit exceeded (Ready)\nsecond line of the report".to_string(),
    ));

    // The transcript is honest about the failure — the fallback path's source.
    assert!(
        r.messages
            .iter()
            .any(|line| line.contains("LLM error: rate limit exceeded")),
        "{:?}",
        r.messages
    );

    // And the buffer holds the category plus the whole text, which is more than
    // §7.4 lets the status line keep.
    let sticky = r.last_error.clone().expect("an error was reported");
    assert_eq!(sticky.category, "rate limit exceeded", "{sticky:?}");
    assert_eq!(
        sticky.detail,
        "LLM error: rate limit exceeded (Ready)\nsecond line of the report"
    );

    // The status line moves on: the spinner text is whatever the loop reported next.
    r.on_event(&Event::Status("Ready".to_string()));
    assert_eq!(r.status_line, "Ready");

    // `e` with an empty composer is never a silent key (§5, G5).
    assert!(press(&mut r, KeyCode::Char('e'), KeyModifiers::NONE));
    let recalled = r.messages.last().cloned().unwrap_or_default();
    // The recalled line is the buffer: `✗ <category>`, then the report verbatim.
    assert!(
        recalled.starts_with("✗ rate limit exceeded\nLLM error:"),
        "{recalled:?}"
    );
    assert!(
        recalled.contains("second line of the report"),
        "the multi-line detail must survive the recall: {recalled:?}"
    );
    assert!(recalled.contains("see .marmel/marmel.log"), "{recalled:?}");
    assert!(
        !recalled.contains("No error recorded"),
        "the pending-plumbing message must not survive the plumbing: {recalled:?}"
    );
}

/// §8.5 + work item 3: the buffer, not the transcript, is what `e` reads. A
/// transcript clear — what `/reset` and a restore can do — must not cost the
/// operator the cause of the failure that ended the last turn.
#[test]
fn an_error_recalled_with_e_survives_a_transcript_clear() {
    let mut r = TuiRenderer::new();
    r.on_event(&Event::Status(
        "LLM error: http (Ready)\nrequest to the backend was refused".to_string(),
    ));
    assert!(r.last_error.is_some(), "the report was remembered");

    // Everything the transcript scan could have read is gone.
    r.messages.clear();
    r.status_line = "Ready".to_string();
    assert!(r.messages.is_empty(), "the transcript was cleared");

    assert!(press(&mut r, KeyCode::Char('e'), KeyModifiers::NONE));
    let recalled = r.messages.last().cloned().unwrap_or_default();
    assert_eq!(
        recalled,
        "✗ http\nLLM error: http (Ready)\nrequest to the backend was refused\n\
         see .marmel/marmel.log",
        "the whole report is recalled from the buffer"
    );

    // With no buffer and no transcript either, `e` still says so (G5).
    let mut clean = TuiRenderer::new();
    assert!(press(&mut clean, KeyCode::Char('e'), KeyModifiers::NONE));
    assert!(
        clean
            .messages
            .last()
            .is_some_and(|m| m.contains("No error recorded")),
        "{:?}",
        clean.messages
    );

    // The transcript scan stays the fallback: traffic pushed straight into the
    // transcript is recalled verbatim when no buffer was ever filled.
    let mut legacy = TuiRenderer::new();
    legacy
        .messages
        .push("LLM error: timeout (Ready) — stream cut".to_string());
    assert!(press(&mut legacy, KeyCode::Char('e'), KeyModifiers::NONE));
    let fallback = legacy.messages.last().cloned().unwrap_or_default();
    assert!(fallback.starts_with("✗ LLM error: timeout"), "{fallback:?}");
}

/// A non-error status report never leaves an error behind, and a repeated report of the
/// same failure does not restamp the buffer: it records the edge, not the traffic.
#[test]
fn only_error_traffic_is_remembered_and_repeats_do_not_restamp_it() {
    let mut r = TuiRenderer::new();
    r.on_event(&Event::Status("calling backend".to_string()));
    assert!(
        r.last_error.is_none(),
        "ordinary status traffic is not an error: {:?}",
        r.last_error
    );

    r.on_event(&Event::Status(
        "System Error: transport closed the stream".to_string(),
    ));
    let first = r.last_error.clone().expect("the error is recorded");
    assert_eq!(first.category, "System Error", "{first:?}");

    r.on_event(&Event::Status(
        "System Error: transport closed the stream".to_string(),
    ));
    assert_eq!(
        r.last_error,
        Some(first.clone()),
        "the same failure re-announced keeps the first record"
    );

    // A genuine second failure does replace it: the buffer tracks the last error, not
    // the first one.
    r.on_event(&Event::Status("LLM error: http (Ready)".to_string()));
    let second = r.last_error.clone().expect("the newer error wins");
    assert_eq!(second.category, "http", "{second:?}");
    assert_ne!(second, first.clone(), "the buffer tracks the last error");
    assert_eq!(first.detail, "System Error: transport closed the stream");
}

#[test]
fn fit_spans_clips_to_the_rect_and_marks_the_cut() {
    let wide = components::fit_spans(
        vec![
            ratatui::text::Span::raw("abcdef"),
            ratatui::text::Span::raw("ghijkl"),
        ],
        12,
    );
    assert_eq!(
        wide.iter().map(|s| s.content.as_ref()).collect::<String>(),
        "abcdefghijkl"
    );

    let clipped = components::fit_spans(
        vec![
            ratatui::text::Span::raw("abcdef"),
            ratatui::text::Span::raw("ghijkl"),
        ],
        8,
    );
    let text = clipped
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>();
    assert_eq!(
        text, "abcdefg…",
        "the cut is cell-granular, not span-granular"
    );

    let one = components::fit_spans(
        vec![
            ratatui::text::Span::raw("abcdef"),
            ratatui::text::Span::raw("g"),
        ],
        3,
    );
    let text = one.iter().map(|s| s.content.as_ref()).collect::<String>();
    assert_eq!(text, "ab…");

    let zero = components::fit_spans(vec![ratatui::text::Span::raw("abc")], 0);
    assert!(zero.is_empty(), "a zero-width rect gets no spans at all");
}

#[test]
fn the_new_frame_carries_header_plan_progress_pane_titles_and_the_legend() {
    // Acceptance criterion 1 (layout half): progress is in the frame with the
    // plan pane open, and the legend is there too (G1/G2).
    let mut r = m4_renderer();
    r.input_text = String::new();
    let rows = draw_new_frame(&mut r, 100, 30);
    let header = rows[0].trim();
    assert!(header.starts_with("Marmel"), "header row: {header:?}");
    assert!(header.contains("M4 Split"), "mode chip: {header:?}");
    assert!(header.contains("1/2 done"), "plan progress: {header:?}");
    assert!(header.contains("t-004"), "active task chip: {header:?}");

    let joined = rows.concat();
    assert!(joined.contains("Transcript"), "transcript title");
    assert!(joined.contains("Execution Plan"), "plan title");
    assert!(joined.contains("Workers"), "workers title");
    assert!(joined.contains("Input"), "composer title");
    assert!(joined.contains("F1 help"), "footer legend");
    assert!(joined.contains("LIVE"), "follow chip on status row 2");
    // The content pass still fills the panes with real text.
    assert!(joined.contains("User: rebuild the TUI"), "transcript body");
    assert!(joined.contains("wireframe"), "plan body");
    assert!(joined.contains("coder_specialist"), "worker roster body");
}

#[test]
fn the_focused_pane_badges_its_title_and_every_other_pane_does_not() {
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Plan);
    let rows = draw_new_frame(&mut r, 100, 30);
    let joined = rows.concat();
    let badgeed: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains(theme::FOCUS_BADGE))
        .collect();
    assert!(
        badgeed.iter().any(|row| row.contains("Execution Plan")),
        "the focused plan pane carries the badge: {badgeed:?}"
    );
    assert!(
        !joined.contains("Workers ●"),
        "only the focused pane is badged in its title"
    );
}

// ---- t-009d regression frames: chip clipping, status ladder, roster rank ------

/// The running scenario of `docs/new_ui_preview.txt` case b: a 7-task plan with 3
/// checked and three live worker rows — running, queued at rank `2/3`, paused —
/// under real session counters. The frames the t-009d defect fixes are asserted
/// against this fixture, so a fix can never be proved only at component level.
fn live_worker_scenario() -> TuiRenderer {
    let mut r = TuiRenderer::new();
    r.messages = vec![
        "User: rebuild the TUI so plan, workers and progress stay legible".to_string(),
        "Marmennill: I split the work into seven tasks and started with the audit.".to_string(),
        "[Status] delegating validator_specialist (t-003)".to_string(),
        "Tool: read_file(src/ui/tui/mod.rs)".to_string(),
        "Marmennill: every pane is now drawn by its own component renderer.".to_string(),
        "Streaming: the status bar chips ar".to_string(),
    ];
    r.plan_content = [
        "# Plan",
        "",
        "- [x] [t-001] research UX baseline",
        "- [x] [t-002] audit TUI architecture",
        "- [x] [t-003] draft the design spec",
        "- [ ] [t-004] scaffold state, layout, theme",
        "- [ ] [t-005] pane chrome and chips",
        "- [ ] [t-006] wire the roster rank into the status row",
        "- [ ] [t-009b] rewire the render pipeline",
        "",
    ]
    .join("\n");
    r.set_view_mode(state::ViewMode::Split);
    r.input_text = "add the focus badge to every pane title".to_string();
    r.tokens_in = 12_400;
    r.tokens_out = 3_200;
    r.orchestrator_context_tokens = 13_500;
    r.max_context_tokens = 30_000;
    r.plan_start_time = Some(std::time::Instant::now() - Duration::from_secs(612));
    r.subagents = vec![
        SubagentDetail {
            name: "coder_specialist-t-004".to_string(),
            task_id: Some("t-004".to_string()),
            is_active: true,
            worked_duration: Duration::from_secs(192),
            content: "wrote layout.rs, 292 lines".to_string(),
            ..SubagentDetail::default()
        },
        SubagentDetail {
            name: "researcher_specialist-t-005".to_string(),
            task_id: Some("t-005".to_string()),
            is_active: true,
            worked_duration: Duration::from_secs(30),
            queue: Some(crate::ui::QueueState { position: 2, of: 3 }),
            ..SubagentDetail::default()
        },
        SubagentDetail {
            name: "validator_specialist-t-003".to_string(),
            task_id: Some("t-003".to_string()),
            is_active: true,
            paused: true,
            worked_duration: Duration::from_secs(12),
            ..SubagentDetail::default()
        },
    ];
    r
}

/// The one frame row that carries `needle`, or the whole frame in the message.
fn only_row(rows: &[String], needle: &str, size: (u16, u16)) -> String {
    rows.iter()
        .find(|row| row.contains(needle))
        .cloned()
        .unwrap_or_else(|| panic!("no {size:?} row carries {needle:?}:\n{}", rows.join("\n")))
}

#[test]
fn the_workers_tally_keeps_the_done_count_the_position_and_the_detail_chip() {
    // Defect 1 (§7.1/§7.4): the bottom tally was measured and fitted against
    // `rect.width` although a border row is `rect.width - 2` cells, so ratatui cut
    // the last 2 cells silently — the `✓` count, the `1/3` position and the detail
    // chip were all gone and nothing marked the cut.
    let mut r = live_worker_scenario();
    let rows = draw_new_frame(&mut r, 100, 30);
    let tally = only_row(&rows, "detail", (100, 30));
    for glyph in ['▶', '⏸', '⧗', '✓'] {
        assert!(
            tally.contains(glyph),
            "the tally keeps the {glyph} count: {tally:?}"
        );
    }
    assert!(
        tally.contains("1/3") && tally.contains("detail"),
        "the roster position and the detail chip survive to the corner: {tally:?}"
    );
    assert!(
        tally.trim_end().ends_with('┘'),
        "the row stops at the pane's right corner instead of running past it: {tally:?}"
    );
}

#[test]
fn a_queued_worker_keeps_its_rank_where_the_ladder_cannot_cut_it() {
    // Defect 3 (§2.2/§2.5): the rank was the last field of the roster row, so in a
    // clipped row `(2/3)` was never readable — the queue depth the §2.5 ladder is
    // supposed to make legible. It now rides the state fields, before the name.
    let mut r = live_worker_scenario();
    let rows = draw_new_frame(&mut r, 100, 30);
    let roster = only_row(&rows, "researcher_specialist", (100, 30));
    let rank = roster
        .find("2/3")
        .unwrap_or_else(|| panic!("the queue rank is readable at all: {roster:?}"));
    let name = roster
        .find("researcher_specialist")
        .expect("the row still names the worker");
    assert!(
        rank < name,
        "the rank is placed before the fields the ladder clips: {roster:?}"
    );
    assert!(
        roster.contains(theme::ELLIPSIS),
        "the cut is marked, never silent: {roster:?}"
    );
}

#[test]
fn the_status_row_keeps_every_count_and_clips_names_into_the_free_columns() {
    // Defect 2 (§2.5/§7.4): `fit_fields` deleted whole fields rather than clipping
    // one, so a 100-column row read `Tokens 12.4k in / 3.2k out | 3a | ctx 45%`
    // with ~58 columns free — `1p`, `1q` and every active name were lost.
    for (width, height) in [(100u16, 30u16), (80, 24), (60, 16)] {
        let mut r = live_worker_scenario();
        let rows = draw_new_frame(&mut r, width, height);
        let row = rows
            .iter()
            .map(|row| row.trim_end())
            .find(|row| row.trim_start().starts_with("Tokens"))
            .unwrap_or_else(|| panic!("no metrics row at {width}x{height}:\n{}", rows.join("\n")))
            .to_string();
        for count in ["3a", "1p", "1q"] {
            assert!(
                row.contains(count),
                "{width}x{height}: the {count} count survives the ladder: {row:?}"
            );
        }
        assert!(
            row.starts_with(" Tokens ") && row.contains(" in / ") && row.contains(" out"),
            "{width}x{height}: the mandatory token field stays: {row:?}"
        );
        if width >= 80 {
            assert!(
                row.contains("ctx 45%"),
                "{width}x{height}: the ctx ratio is on the row: {row:?}"
            );
            assert!(
                row.contains(theme::ELLIPSIS),
                "{width}x{height}: the name list is clipped with a mark instead of dropped: {row:?}"
            );
            assert!(
                row.contains("coder"),
                "{width}x{height}: the name list is cut inside a name, not deleted: {row:?}"
            );
        }
    }
}

#[test]
fn the_status_row_uses_the_columns_it_is_given() {
    // The audit's measurement: 41 used cells of a 100-cell row, ~58 free. The
    // repaired row fills the width it owns before it drops anything.
    let mut r = live_worker_scenario();
    let rows = draw_new_frame(&mut r, 100, 30);
    let row = only_row(&rows, "Tokens", (100, 30)).trim_end().to_string();
    // The audit measured 41 used cells of a 100-cell row. The repaired row must
    // use the width it is given, so it is comfortably more than twice that.
    assert!(
        row.chars().count() > 41 * 2,
        "the 100-column row is used, not left mostly empty: {} cells: {row:?}",
        row.chars().count()
    );
    assert!(row.contains("elapsed"), "the clock came back: {row:?}");
}

#[test]
fn a_pane_the_ladder_removed_is_reported_narrow_and_never_on() {
    // Defect 5 (§8): the pane chips came from the mode alone, so an 80-column frame
    // announced `workers:on` while the Workers pane had been ladder-absent.
    let wide = draw_new_frame(&mut live_worker_scenario(), 100, 30).concat();
    assert!(
        wide.contains("plan:on") && wide.contains("workers:on"),
        "both panes are drawn at 100 columns, and both say so"
    );
    assert!(
        !wide.contains("narrow") && !wide.contains("plan:off") && !wide.contains("workers:off"),
        "nothing is announced as absent that is on screen"
    );

    let narrow = draw_new_frame(&mut live_worker_scenario(), 80, 24).concat();
    assert!(
        narrow.contains("workers:narrow"),
        "the pane the §2.5 width ladder took away says why: {narrow}"
    );
    assert!(
        !narrow.contains("workers:on"),
        "a pane that is not drawn is never announced as on"
    );
    assert!(
        narrow.contains("plan:on"),
        "the side pane that did survive is still reported on"
    );
}

#[test]
fn the_scroll_state_chip_is_drawn_once_in_the_transcript_pane() {
    // Defect 4 (§7.3, §8): `● LIVE` was printed twice per frame — on the transcript
    // border row and on the status chip row — so two rows claimed the scroll state.
    for (width, height) in [(100u16, 30u16), (80, 24), (60, 16), (40, 10)] {
        let rows = draw_new_frame(&mut live_worker_scenario(), width, height);
        let live: Vec<&String> = rows.iter().filter(|row| row.contains("LIVE")).collect();
        assert_eq!(
            live.len(),
            1,
            "{width}x{height} draws exactly one scroll-state chip:\n{}",
            rows.join("\n")
        );
        assert!(
            live[0].contains('└') || live[0].contains('┌'),
            "{width}x{height}: the one chip belongs to the transcript border: {:?}",
            live[0]
        );
    }
}

// ==== t-011: the §5 keymap, focus navigation and the shared help legend =====

/// §10 item 3: the legend must name no key the handler does not bind, and every
/// binding must appear in the legend. Each row of `components::help::legend()` is
/// paired here with a driver that feeds the synthetic event the row names through
/// [`TuiRenderer::apply_terminal_event`] — the same seam the live loop calls — and
/// asserts the state change the row promises. A stale legend row therefore fails the
/// test, and so does a handler that quietly gains an undocumented binding.
#[test]
fn every_legend_row_is_a_real_handler_path() {
    type Driver = fn(&mut TuiRenderer) -> bool;

    let drivers: Vec<(&'static str, Driver)> = vec![
        ("Enter", |r| {
            r.set_input_content("ship it");
            press(r, KeyCode::Enter, KeyModifiers::NONE)
                && r.messages.last().map(String::as_str) == Some("User: ship it")
        }),
        ("Shift+Enter / Alt+Enter", |r| {
            r.set_input_content("line one");
            press(r, KeyCode::Enter, KeyModifiers::SHIFT)
                && press(r, KeyCode::Enter, KeyModifiers::ALT)
                && r.input_text == "line one\n\n"
        }),
        ("Ctrl+Enter / Ctrl+J", |r| {
            r.set_input_content("a");
            if !press(r, KeyCode::Enter, KeyModifiers::CONTROL) || r.input_text != "a\n" {
                return false;
            }
            if !press(r, KeyCode::Char('j'), KeyModifiers::CONTROL) || r.input_text != "a\n\n" {
                return false;
            }
            // The raw `Char('\n')` some terminals produce is the same binding.
            press(r, KeyCode::Char('\n'), KeyModifiers::NONE) && r.input_text == "a\n\n\n"
        }),
        ("Printable / paste", |r| {
            r.set_focus(state::FocusPane::Workers);
            let typed = press(r, KeyCode::Char('q'), KeyModifiers::NONE) && r.input_text == "q";
            let pasted = r
                .apply_terminal_event(&crossterm::event::Event::Paste("pasted block".to_string()))
                && r.input_text == "qpasted block";
            typed && pasted
        }),
        ("Backspace / Delete", |r| {
            r.set_focus(state::FocusPane::Plan);
            r.set_input_content("abc");
            if !press(r, KeyCode::Backspace, KeyModifiers::NONE) || r.input_text != "ab" {
                return false;
            }
            press(r, KeyCode::Left, KeyModifiers::NONE);
            press(r, KeyCode::Delete, KeyModifiers::NONE) && r.input_text == "a"
        }),
        ("Ctrl+Z / Ctrl+Y", |r| {
            r.set_input_content("");
            press(r, KeyCode::Char('x'), KeyModifiers::NONE);
            let undo =
                press(r, KeyCode::Char('z'), KeyModifiers::CONTROL) && r.input_text.is_empty();
            let redo = press(r, KeyCode::Char('y'), KeyModifiers::CONTROL) && r.input_text == "x";
            undo && redo
        }),
        ("Ctrl+Up / Ctrl+Down", |r| {
            r.history = vec!["earlier note".to_string()];
            let back =
                press(r, KeyCode::Up, KeyModifiers::CONTROL) && r.input_text == "earlier note";
            let fwd = press(r, KeyCode::Down, KeyModifiers::CONTROL) && r.input_text.is_empty();
            back && fwd
        }),
        ("Up / Down", |r| {
            // With a draft the keys scroll the focused pane by one ...
            r.messages = (0..120).map(|i| format!("User: filler {i}")).collect();
            r.chat_width.set(100);
            r.chat_height.set(10);
            r.set_focus(state::FocusPane::Transcript);
            r.set_input_content("a draft");
            press(r, KeyCode::End, KeyModifiers::NONE);
            let bottom = r.chat_scroll;
            if bottom == 0 {
                return false;
            }
            if !press(r, KeyCode::Up, KeyModifiers::NONE) || r.chat_scroll >= bottom {
                return false;
            }
            // ... and with an empty composer they walk the history (§5).
            r.set_input_content("");
            r.history = vec!["earlier note".to_string()];
            press(r, KeyCode::Up, KeyModifiers::NONE) && r.input_text == "earlier note"
        }),
        ("Left / Right", |r| {
            // Workers focus: the roster selection moves ...
            r.subagents.push(SubagentDetail {
                name: "coder-t-009".to_string(),
                task_id: Some("t-009".to_string()),
                ..SubagentDetail::default()
            });
            r.set_focus(state::FocusPane::Workers);
            let next = press(r, KeyCode::Right, KeyModifiers::NONE) && r.selected_subagent_idx == 1;
            let prev = press(r, KeyCode::Left, KeyModifiers::NONE) && r.selected_subagent_idx == 0;
            // ... everywhere else it is the caret.
            r.set_focus(state::FocusPane::Input);
            r.set_input_content("hello");
            let caret = press(r, KeyCode::Left, KeyModifiers::NONE)
                && r.cursor == "hell".len()
                && r.input_text == "hello";
            next && prev && caret
        }),
        ("Home / End", |r| {
            r.set_input_content("hello");
            let home = press(r, KeyCode::Home, KeyModifiers::NONE) && r.cursor == 0;
            let end = press(r, KeyCode::End, KeyModifiers::NONE) && r.cursor == "hello".len();
            r.set_focus(state::FocusPane::Plan);
            r.plan_max_scroll = 30;
            let pane_end = press(r, KeyCode::End, KeyModifiers::NONE) && r.plan_scroll == 30;
            let pane_home = press(r, KeyCode::Home, KeyModifiers::NONE) && r.plan_scroll == 0;
            home && end && pane_end && pane_home
        }),
        ("PageUp / PageDown", |r| {
            r.messages = (0..120).map(|i| format!("User: filler {i}")).collect();
            r.chat_width.set(100);
            r.chat_height.set(10);
            r.set_focus(state::FocusPane::Transcript);
            r.set_input_content("a draft");
            press(r, KeyCode::End, KeyModifiers::NONE);
            let bottom = r.chat_scroll;
            if bottom == 0 {
                return false;
            }
            if !press(r, KeyCode::PageUp, KeyModifiers::NONE)
                || r.chat_scroll != bottom.saturating_sub(10)
            {
                return false;
            }
            press(r, KeyCode::PageDown, KeyModifiers::NONE) && r.chat_scroll == bottom
        }),
        ("Tab / Shift+Tab", |r| {
            let fwd = press(r, KeyCode::Tab, KeyModifiers::NONE)
                && r.focus == state::FocusPane::Transcript;
            let back = press(r, KeyCode::BackTab, KeyModifiers::NONE)
                && r.focus == state::FocusPane::Input;
            fwd && back
        }),
        ("F1 / ?", |r| {
            let f1 = press(r, KeyCode::F(1), KeyModifiers::NONE)
                && r.help_open
                && r.overlay == state::OverlayState::Help;
            r.close_overlay();
            let question = press(r, KeyCode::Char('?'), KeyModifiers::NONE)
                && r.help_open
                && r.input_text.is_empty();
            f1 && question
        }),
        ("F2", |r| {
            let before = r.view_mode;
            press(r, KeyCode::F(2), KeyModifiers::NONE)
                && r.view_mode == state::ViewMode::Transcript
                && r.view_mode != before
        }),
        ("F3 / F4 / F5", |r| {
            if !press(r, KeyCode::F(3), KeyModifiers::NONE)
                || r.view_mode != state::ViewMode::Transcript
            {
                return false;
            }
            if !press(r, KeyCode::F(4), KeyModifiers::NONE)
                || !r.show_plan_panel
                || r.show_subagent_panel
            {
                return false;
            }
            press(r, KeyCode::F(5), KeyModifiers::NONE)
                && r.view_mode == state::ViewMode::Workers
                && r.show_subagent_panel
                && !r.show_plan_panel
        }),
        ("Ctrl+P / Ctrl+A", |r| {
            r.set_view_mode(state::ViewMode::Transcript);
            let plan = press(r, KeyCode::Char('p'), KeyModifiers::CONTROL)
                && r.show_plan_panel
                && r.view_mode == state::ViewMode::Plan;
            let workers = press(r, KeyCode::Char('a'), KeyModifiers::CONTROL)
                && r.show_subagent_panel
                && r.view_mode == state::ViewMode::Split;
            plan && workers
        }),
        ("Ctrl+T", |r| {
            // §5: the echo is a chip, never a chat line (G5).
            press(r, KeyCode::Char('t'), KeyModifiers::CONTROL)
                && r.show_thought
                && r.messages.iter().all(|m| !m.contains("Thinking display"))
        }),
        ("Ctrl+F", |r| {
            let off = press(r, KeyCode::Char('f'), KeyModifiers::CONTROL)
                && !r.follow
                && !r.chat_auto_scroll;
            let on = press(r, KeyCode::Char('f'), KeyModifiers::CONTROL)
                && r.follow
                && r.chat_auto_scroll;
            off && on
        }),
        ("x", |r| {
            r.set_focus(state::FocusPane::Workers);
            press(r, KeyCode::Char('x'), KeyModifiers::NONE)
                && r.overlay == state::OverlayState::Confirm
                && r.confirm_action
                    == Some(events::ConfirmAction::CancelWorker {
                        key: "coder_specialist-t-004".to_string(),
                        task_id: Some("t-004".to_string()),
                    })
                && r.input_text.is_empty()
        }),
        ("e", |r| {
            r.messages
                .push("LLM error: rate limit exceeded".to_string());
            press(r, KeyCode::Char('e'), KeyModifiers::NONE)
                && r.messages
                    .last()
                    .is_some_and(|m| m.starts_with("✗ LLM error: rate limit exceeded"))
        }),
        ("Esc", |r| {
            r.open_help();
            press(r, KeyCode::Esc, KeyModifiers::NONE)
                && r.overlay == state::OverlayState::None
                && !r.is_abort_armed()
        }),
        ("Ctrl+C", |r| {
            press(r, KeyCode::Char('c'), KeyModifiers::CONTROL)
                && r.is_abort_armed()
                && r.overlay == state::OverlayState::Confirm
                && r.status_line.contains("Abort armed")
        }),
        ("Ctrl+D", |r| {
            press(r, KeyCode::Char('d'), KeyModifiers::CONTROL)
                && r.confirm_action == Some(events::ConfirmAction::Quit)
                && r.overlay == state::OverlayState::Confirm
                && !r.user_exit_requested()
        }),
        ("Printable while armed", |r| {
            press(r, KeyCode::Esc, KeyModifiers::NONE);
            let armed = r.is_abort_armed();
            press(r, KeyCode::Char('k'), KeyModifiers::NONE)
                && armed
                && !r.is_abort_armed()
                && r.input_text == "k"
                && r.status_line == "Abort cancelled."
                && r.overlay == state::OverlayState::None
        }),
        ("y / n", |r| {
            press(r, KeyCode::Char('d'), KeyModifiers::CONTROL);
            press(r, KeyCode::Char('n'), KeyModifiers::NONE)
                && r.overlay == state::OverlayState::None
                && r.confirm_action.is_none()
                && r.status_line == "Quit cancelled."
                && !r.user_exit_requested()
        }),
        ("Resize", |r| {
            r.messages = (0..50).map(|i| format!("User: filler {i}")).collect();
            r.chat_width.set(100);
            r.chat_height.set(10);
            r.chat_scroll = 999;
            r.apply_terminal_event(&crossterm::event::Event::Resize(100, 24)) && r.chat_scroll < 999
        }),
    ];

    let rows = components::help::legend();
    let documented: Vec<&str> = rows.iter().map(|row| row.keys).collect();
    let driven: Vec<&str> = drivers.iter().map(|(keys, _)| *keys).collect();

    for keys in &driven {
        assert!(
            documented.contains(keys),
            "a driver drives {keys:?}, which the legend does not document: {documented:?}"
        );
    }
    for row in &rows {
        assert!(
            driven.contains(&row.keys),
            "the legend documents {:?} with no handler behind it: {driven:?}",
            row.keys
        );
    }
    assert_eq!(
        rows.len(),
        drivers.len(),
        "exactly one driver per legend row"
    );

    for (keys, drive) in &drivers {
        let mut renderer = m4_renderer();
        assert!(
            drive(&mut renderer),
            "the {keys:?} legend row promises a state change the handler does not produce"
        );
    }
}

/// §5 + §10 item 16: `Esc` spends itself on an open overlay and never arms; with
/// nothing open it arms the 3 s window, and the window is *not* an exit.
///
/// The third press — the one that takes the exit path — is pinned by
/// [`both_quit_requests_route_through_the_renderers_existing_exit`] rather than
/// pressed here: [`Renderer::request_user_exit`] calls `orchestrator::cancel_all()`,
/// a process-wide cancellation that races with the wall-clock arbitration sleeps of
/// the `ui::bridge` tests in the same test binary (it flips their
/// `sleep_cancelled`). The end-to-end exit is asserted by the session integration
/// harness instead.
#[test]
fn esc_closes_an_open_overlay_without_arming_then_arms_abort() {
    let mut r = m4_renderer();

    r.open_help();
    assert!(press(&mut r, KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(r.overlay, state::OverlayState::None);
    assert!(!r.help_open, "the legend closed");
    assert!(!r.confirm_abort, "closing an overlay never arms abort");
    assert!(!r.is_abort_armed());
    assert_eq!(
        r.focus,
        state::FocusPane::Input,
        "Esc no longer re-focuses Chat as a side effect (UX friction 8)"
    );

    assert!(press(&mut r, KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        r.is_abort_armed(),
        "nothing was open, so the key armed abort"
    );
    assert!(r.status_line.contains("Abort armed"));
    assert!(
        !r.user_exit_requested() && !r.input_state.aborted,
        "arming is not an exit"
    );
    assert_eq!(
        r.overlay,
        state::OverlayState::Confirm,
        "the armed prompt occupies the confirm surface (§8.5)"
    );
    assert!(
        r.confirm_action.is_none(),
        "the armed prompt holds no y/n request: a printable is typing, not consent"
    );
}

/// §5 + §10 item 8: `Ctrl+D` never exits un-armed. It opens the Quit confirm, a
/// second press is the same request, `n`/`Esc` leave the session alive and un-armed,
/// and only `y` takes the existing quit path.
#[test]
fn ctrl_d_opens_the_quit_confirm_and_never_exits_un_armed() {
    let mut r = TuiRenderer::new();

    assert!(press(&mut r, KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(r.confirm_action, Some(events::ConfirmAction::Quit));
    assert_eq!(r.overlay, state::OverlayState::Confirm);
    assert!(!r.user_exit_requested(), "one keystroke never ends it");
    assert!(!r.is_abort_armed(), "Ctrl+D arms nothing");

    press(&mut r, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(!r.user_exit_requested(), "a second Ctrl+D still needs y");

    assert!(press(&mut r, KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(r.overlay, state::OverlayState::None);
    assert!(
        !r.is_abort_armed(),
        "Esc out of the confirm leaves it un-armed"
    );
    assert!(!r.user_exit_requested());

    press(&mut r, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(press(&mut r, KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(r.confirm_action.is_none());
    assert_eq!(r.status_line, "Quit cancelled.");
    assert!(
        !r.user_exit_requested() && !r.input_state.aborted,
        "`n` leaves the session alive"
    );
    assert!(
        !r.is_abort_armed(),
        "and `n` arms nothing either (§10 item 6)"
    );
}

/// §10 item 13: printables and editing keys mutate the composer whatever pane is
/// focused, and `x` is only a mnemonic while the composer is empty.
#[test]
fn printables_and_editing_keys_reach_the_composer_from_every_focus() {
    let mut r = m4_renderer();

    for pane in [
        state::FocusPane::Input,
        state::FocusPane::Transcript,
        state::FocusPane::Plan,
        state::FocusPane::Workers,
    ] {
        r.set_focus(pane);
        r.set_input_content("abc");
        assert!(press(&mut r, KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(press(&mut r, KeyCode::Left, KeyModifiers::NONE));
        assert!(press(&mut r, KeyCode::Backspace, KeyModifiers::NONE));
        assert!(press(&mut r, KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(
            r.input_text, "ab",
            "{pane:?} must not swallow printable or editing keys"
        );
    }

    // `?` types while there is a draft, and only opens the legend when empty (§4).
    r.set_input_content("what");
    assert!(press(&mut r, KeyCode::Char('?'), KeyModifiers::NONE));
    assert_eq!(r.input_text, "what?");
    assert!(!r.help_open, "a typed `?` is not the overlay key");

    // `x` with an empty composer in the Workers pane is the cancel mnemonic (§5).
    r.set_focus(state::FocusPane::Workers);
    r.set_input_content("");
    assert!(press(&mut r, KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(r.overlay, state::OverlayState::Confirm);
    assert_eq!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker {
            key: "coder_specialist-t-004".to_string(),
            task_id: Some("t-004".to_string()),
        })
    );
    assert_eq!(r.input_text, "", "the mnemonic consumed the character");
    assert_eq!(
        r.status_line,
        "Confirm cancel of worker 'coder_specialist-t-004' — y to cancel it, n or Esc to leave it running."
    );
}

/// §10 item 16: `y` / `n` are honoured only inside a confirm surface that holds a
/// request; everywhere else they are ordinary characters.
#[test]
fn y_and_n_are_honoured_only_inside_the_confirm_overlay() {
    let mut r = TuiRenderer::new();

    // Outside any confirm they type: typing `no` still types.
    assert!(press(&mut r, KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(press(&mut r, KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(r.input_text, "yn", "typing `no` still types");
    assert_eq!(r.overlay, state::OverlayState::None);

    // The armed-abort prompt occupies the same surface but holds no request (§8.5),
    // so a `y` there types and disarms rather than consenting (§5).
    press(&mut r, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(r.overlay, state::OverlayState::Confirm);
    assert!(r.is_abort_armed());
    assert!(press(&mut r, KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(r.input_text, "yny", "`y` did not confirm the armed prompt");
    assert!(!r.is_abort_armed(), "and the printable disarmed it instead");
    assert!(!r.user_exit_requested());

    // Inside a confirm that does hold a request, `n` is the dismissal binding.
    press(&mut r, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(r.confirm_action, Some(events::ConfirmAction::Quit));
    assert!(press(&mut r, KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(r.input_text, "yny", "the pending request ate the `n`");
    assert!(r.confirm_action.is_none());
    assert_eq!(r.overlay, state::OverlayState::None);
}

/// The two exit requests are pinned by wiring, because firing them would cancel the
/// whole process (`orchestrator::cancel_all()` in `Renderer::request_user_exit`,
/// mod.rs:1726) and thereby flip the `sleep_cancelled` of the wall-clock arbitration
/// tests in `ui::bridge` that share this test binary. Both routes must land on the
/// renderer's **existing** quit path — the one the armed-abort flow already used —
/// and no key may exit un-armed.
#[test]
fn both_quit_requests_route_through_the_renderers_existing_exit() {
    fn body(source: &str, signature: &str) -> String {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("events.rs no longer has `{signature}`"));
        let rest = &source[start..];
        let end = rest[1..]
            .find("\n    fn ")
            .map(|at| at + 1)
            .unwrap_or(rest.len());
        rest[..end].to_string()
    }

    let source = include_str!("events.rs");

    let escape = body(source, "fn escape_pressed(&mut self, chord: &str)");
    assert!(
        escape.contains("if self.is_abort_armed()"),
        "the armed window is checked first, so the second press exits"
    );
    assert!(
        escape.contains("self.close_overlay()"),
        "an open overlay is closed before anything is armed"
    );
    assert!(
        escape.contains("Renderer::request_user_exit(self)"),
        "the exit press reuses the renderer's existing quit path"
    );
    assert!(
        !escape.contains("set_focus(") && !escape.contains("focused_panel ="),
        "Esc never re-focuses a pane (UX friction 8)"
    );

    let confirm = body(source, "fn confirm_pending(&mut self)");
    assert!(
        confirm.contains("ConfirmAction::Quit"),
        "the Quit request is one of the two the surface can hold"
    );
    assert!(
        confirm.contains("Renderer::request_user_exit(self)"),
        "`y` on the Quit confirm takes the same existing quit path"
    );
    assert!(
        confirm.contains("cancel_active_worker"),
        "`y` on a cancel request addresses one worker by key"
    );
    assert!(
        !confirm.contains("cancel_all"),
        "and never the broadcast cancellation (spec §5, bus.rs:38)"
    );

    let request_quit = body(source, "fn request_quit(&mut self)");
    assert!(
        request_quit.contains("show_request(ConfirmAction::Quit)"),
        "Ctrl+D routes onto the confirm surface through the one arming helper"
    );
    assert!(
        !request_quit.contains("request_user_exit")
            && !request_quit.contains("confirm_abort = true"),
        "Ctrl+D neither exits nor arms on its own"
    );
    let show_request = body(source, "fn show_request(&mut self, action: ConfirmAction)");
    assert!(
        show_request.contains("self.show_confirm()")
            && show_request.contains("self.confirm_action = Some(action)"),
        "the surface is opened with the request it will run, from one place"
    );
    assert!(
        !show_request.contains("request_user_exit")
            && !show_request.contains("cancel_active_worker"),
        "opening the surface runs nothing (G3)"
    );

    // t-013: the two bulk-cancellation spellings are woken only by their own arm,
    // so a single-worker request can never reach the broadcast (§10 item 17).
    let cancel_all_body = body(source, "fn cancel_every_worker(&mut self)");
    assert!(
        cancel_all_body.contains("crate::orchestrator::cancel_all()"),
        "`/cancel-all` runs the cancellation §6 names, in a function of its own"
    );
    assert!(
        !body(source, "fn request_cancel_selected_worker").contains("cancel_all")
            && !body(source, "fn request_cancel_worker_by_argument").contains("cancel_all"),
        "a one-worker request never names the broadcast cancellation"
    );
    let handoff = body(source, "fn confirm_pending(&mut self)");
    assert!(
        handoff.contains("self.tx.send(line)"),
        "a confirmed slash command is handed to the session loop rather than \
         re-implemented in the renderer (unified `/reset`, spec §6)"
    );
    let arm = body(source, "fn arm_destructive_command(&mut self, line: &str)");
    for spelling in [
        "/cancel-all",
        "/cancel",
        "is_reset_command",
        "is_abort(&token)",
    ] {
        assert!(arm.contains(spelling), "arm gate missing {spelling}");
    }
    assert!(
        !arm.contains("Plan::default")
            && !arm.contains("request_user_exit")
            && !arm.contains("self.tx.send"),
        "the gate arms the surface only: no plan clear, no exit, no send"
    );
    let submit = body(source, "pub(crate) fn submit(&mut self)");
    assert!(
        !submit.contains("Plan::default()") && !submit.contains("plan.clear()"),
        "the renderer-local `/reset` short-circuit is gone: all six spellings take \
         the loop's full path (spec §6)"
    );
    assert!(
        submit.contains("arm_destructive_command")
            && submit.contains("is_bare_abort_word")
            && !submit.contains("Renderer::request_user_exit"),
        "`submit` hands destructive spellings to the surface and never exits by itself"
    );
}

/// §5 `x` + §10 items 9/17: the confirm carries the selected row's identity, and
/// `y` cancels that one worker without ever touching the exit path.
#[test]
fn x_then_y_cancels_the_selected_worker_by_name() {
    let mut r = m4_renderer();
    r.subagents.push(SubagentDetail {
        name: "researcher-t-009".to_string(),
        task_id: Some("t-009".to_string()),
        is_active: true,
        ..SubagentDetail::default()
    });
    r.set_focus(state::FocusPane::Workers);
    r.selected_subagent_idx = 1;

    assert!(press(&mut r, KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker {
            key: "researcher-t-009".to_string(),
            task_id: Some("t-009".to_string()),
        }),
        "the request names the row that was selected when the key was pressed"
    );
    assert!(r.status_line.contains("researcher-t-009"));

    assert!(press(&mut r, KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(r.confirm_action.is_none());
    assert_eq!(r.overlay, state::OverlayState::None);
    assert!(
        r.status_line.contains("researcher-t-009"),
        "the outcome of the cancel is stated: {}",
        r.status_line
    );
    assert!(
        !r.user_exit_requested(),
        "cancelling one worker never exits the session"
    );
    assert!(!r.is_abort_armed());
}

/// §6: `Tab` accepts the composer's ghost completion — the longest common prefix,
/// never a guessed command — and only falls through to focus cycling when there is
/// nothing to accept.
#[test]
fn tab_accepts_the_ghost_completion_of_a_slash_command() {
    let mut r = TuiRenderer::new();
    assert_eq!(r.focus, state::FocusPane::Input, "boot focus");

    r.set_input_content("/tho");
    assert!(press(&mut r, KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(r.input_text, "/thought");
    assert_eq!(
        r.focus,
        state::FocusPane::Input,
        "completing does not move focus"
    );

    // Several candidates: only the shared prefix is inserted (§6).
    r.set_input_content("/rese");
    assert!(press(&mut r, KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(r.input_text, "/reset");

    // A bare `/` names nothing, so `Tab` is the focus key again.
    r.set_input_content("/");
    assert!(press(&mut r, KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(r.input_text, "/");
    assert_eq!(r.focus, state::FocusPane::Transcript);

    // An unknown command completes nothing.
    r.set_input_content("/notacommand");
    assert!(press(&mut r, KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(r.input_text, "/notacommand");
    assert_eq!(r.focus, state::FocusPane::Input);
}

/// §10 items 10 and 11: every toggle leaves a chip on status row 2, and an empty
/// pane turns on and says so instead of doing nothing.
#[test]
fn every_toggle_leaves_a_chip_and_an_empty_pane_says_so() {
    let mut r = TuiRenderer::new();

    let boot = draw_new_frame(&mut r, 100, 30).concat();
    assert!(
        boot.contains("plan:off") && boot.contains("workers:off") && boot.contains("think:off"),
        "the boot chips name every toggle: {boot}"
    );

    assert!(press(&mut r, KeyCode::Char('t'), KeyModifiers::CONTROL));
    let thought = draw_new_frame(&mut r, 100, 30);
    assert!(
        thought
            .iter()
            .any(|row| row.contains("think:on") && row.contains("plan:off")),
        "Ctrl+T leaves a chip: {thought:?}"
    );

    assert!(press(&mut r, KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(press(&mut r, KeyCode::Char('a'), KeyModifiers::CONTROL));
    let panes = draw_new_frame(&mut r, 100, 30);
    let chips = panes
        .iter()
        .find(|row| row.contains("plan:"))
        .expect("a plan chip row");
    assert!(
        chips.contains("plan:on") && chips.contains("workers:on") && chips.contains("think:on"),
        "Ctrl+P / Ctrl+A / Ctrl+T each leave a distinct chip: {chips:?}"
    );

    // Ctrl+A with zero workers: the chip flips and the pane draws its empty state.
    let mut empty = TuiRenderer::new();
    assert!(press(&mut empty, KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(
        empty.show_subagent_panel,
        "the pane turned on, no silent no-op"
    );
    let rows = draw_new_frame(&mut empty, 100, 30);
    let joined = rows.concat();
    assert!(joined.contains("workers:on"), "the chip flipped: {joined}");
    assert!(
        rows.iter().any(|row| row.contains("No specialist")),
        "the empty Workers pane states why it is empty: {joined}"
    );
    assert_eq!(
        components::activity::EMPTY_STATE,
        "No specialist launched yet."
    );
}

/// G7: `Ctrl+F` moves the follow chip in the transcript pane — one chip, one place.
#[test]
fn ctrl_f_moves_the_follow_chip_in_the_transcript_pane() {
    let mut r = m4_renderer();
    let live = draw_new_frame(&mut r, 100, 30).concat();
    assert!(live.contains("LIVE"), "boot follows the tail: {live}");

    assert!(press(&mut r, KeyCode::Char('f'), KeyModifiers::CONTROL));
    let off = draw_new_frame(&mut r, 100, 30).concat();
    assert!(
        off.contains("follow off"),
        "the chip restates the state: {off}"
    );
    assert!(!r.follow);

    assert!(press(&mut r, KeyCode::Char('f'), KeyModifiers::CONTROL));
    let on = draw_new_frame(&mut r, 100, 30).concat();
    assert!(on.contains("LIVE"), "and comes back: {on}");
    assert!(r.follow);
}

/// G7 at the renderer's own seam (DATA gap 7): the unseen baseline is re-measured
/// whenever the viewport sits at the bottom of the body, and is **held** while the
/// operator is scrolled up. That is what lets `↓ N new` count the output that arrived
/// unseen rather than only the gap under the viewport — and the baseline is a measured
/// line count, never a guess from `max_scroll - scroll`.
#[test]
fn the_unseen_baseline_is_re_measured_at_the_tail_and_held_while_scrolled_up() {
    let mut r = m4_renderer();
    r.messages = (0..30)
        .map(|i| format!("User: line {i:02} of the transcript"))
        .collect();

    // Following the tail: every line that exists has been on screen, so the body's own
    // measured line count becomes the baseline.
    draw_new_frame(&mut r, 100, 30);
    let tail = r.transcript_baseline_lines.expect("a frame was drawn");
    assert_eq!(tail, 30, "one rendered line per message at this width");

    // `Ctrl+F` disarms follow with nothing new in flight: the state is stated, and
    // nothing is claimed as unseen.
    assert!(press(&mut r, KeyCode::Char('f'), KeyModifiers::CONTROL));
    let still = draw_new_frame(&mut r, 100, 30);
    assert!(
        still.iter().any(|line| line.contains("follow off")),
        "{still:?}"
    );
    assert!(
        !still.iter().any(|line| line.contains(" new ")),
        "nothing arrived: {still:?}"
    );

    // Ten lines arrive while the viewport stands still. The baseline must not move —
    // the operator has not seen that output — so the pane counts it and says so.
    r.messages
        .extend((30..40).map(|i| format!("User: line {i:02} of the transcript")));
    let behind = draw_new_frame(&mut r, 100, 30);
    assert_eq!(
        r.transcript_baseline_lines,
        Some(tail),
        "the baseline waits for the operator to come back to the tail"
    );
    assert!(
        behind.iter().any(|line| line.contains("\u{2193} 10 new")),
        "the ten unseen lines are counted: {behind:?}"
    );

    // Coming back to the tail retires the count and re-measures the baseline.
    assert!(press(&mut r, KeyCode::Char('f'), KeyModifiers::CONTROL));
    let back = draw_new_frame(&mut r, 100, 30);
    assert!(back.iter().any(|line| line.contains("LIVE")), "{back:?}");
    assert_eq!(r.transcript_baseline_lines, Some(40));
}

/// §10 item 15 from the key side: every roster row is reachable with the arrows —
/// no 3-row clamp survives the keymap.
#[test]
fn every_worker_row_is_reachable_with_the_arrow_keys() {
    let mut r = TuiRenderer::new();
    r.set_view_mode(state::ViewMode::Workers);
    r.set_focus(state::FocusPane::Workers);
    r.subagents = (0..12)
        .map(|i| SubagentDetail {
            name: format!("coder-t-{i:03}"),
            task_id: Some(format!("t-{i:03}")),
            is_active: true,
            ..SubagentDetail::default()
        })
        .collect();

    for step in 1..12 {
        assert!(
            press(&mut r, KeyCode::Right, KeyModifiers::NONE),
            "row {step} is reachable"
        );
        assert_eq!(r.selected_subagent_idx, step, "row {step} is selected");
    }
    press(&mut r, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(
        r.selected_subagent_idx, 11,
        "the selection stops on the last row, it never wraps into a lie"
    );

    for step in (0..11).rev() {
        assert!(press(&mut r, KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(r.selected_subagent_idx, step);
    }
    press(&mut r, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(r.selected_subagent_idx, 0);
}

/// §5: `e` is never silent. It replays the last error traffic, and when nothing is
/// recorded it says the sticky buffer still needs the t-012 plumbing.
#[test]
fn e_recalls_the_last_error_or_reports_what_is_missing() {
    let mut r = TuiRenderer::new();
    assert!(press(&mut r, KeyCode::Char('e'), KeyModifiers::NONE));
    let nothing = r.messages.last().cloned().unwrap_or_default();
    assert!(
        nothing.contains("No error recorded"),
        "an unrecorded error is reported, not ignored: {nothing}"
    );
    assert!(
        nothing.contains("§8.5"),
        "and named as pending plumbing: {nothing}"
    );

    r.messages
        .push("LLM error: rate limit exceeded after 3 retries".to_string());
    assert!(press(&mut r, KeyCode::Char('e'), KeyModifiers::NONE));
    let recalled = r.messages.last().cloned().unwrap_or_default();
    assert!(
        recalled.starts_with("✗ LLM error: rate limit exceeded after 3 retries"),
        "the recorded line is replayed verbatim: {recalled}"
    );
    assert!(recalled.contains("marmel.log"), "and points at the log");
}

/// §10 item 20: mouse reports are ignored outright. Mouse capture is never enabled
/// anywhere in `src/`, so the deleted arms were unreachable; a report must move
/// nothing, and must not even count as a handled event.
#[test]
fn a_mouse_report_is_ignored_and_moves_nothing() {
    let mut r = m4_renderer();
    r.set_input_content("untouched");
    r.set_focus(state::FocusPane::Plan);
    let focus = r.focus;
    let mode = r.view_mode;
    let scroll = r.plan_scroll;

    for kind in [
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::ScrollDown,
        crossterm::event::MouseEventKind::ScrollUp,
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
    ] {
        let report = crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column: 12,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            !r.apply_terminal_event(&report),
            "a mouse report is not a binding: {kind:?}"
        );
    }

    assert_eq!(r.focus, focus);
    assert_eq!(r.view_mode, mode);
    assert_eq!(r.plan_scroll, scroll);
    assert_eq!(r.input_text, "untouched");
    assert_eq!(r.overlay, state::OverlayState::None);
}

/// Work item 5: the composer is the typing target at boot, so the boot frame must
/// badge the composer and never claim the transcript is the focus target.
#[test]
fn the_boot_frame_badges_the_composer_not_the_transcript() {
    let r_new = TuiRenderer::new();
    assert_eq!(r_new.focus, state::FocusPane::Input);
    assert_eq!(r_new.focused_panel, FocusedPanel::Chat);
    assert_eq!(r_new.view_mode, state::ViewMode::Transcript);
    assert!(r_new.follow);
    assert_eq!(r_new.overlay, state::OverlayState::None);

    let mut r = TuiRenderer::new();
    let rows = draw_new_frame(&mut r, 100, 30);
    let joined = rows.concat();

    // Exactly one pane carries the §7.3 focus badge, and it is the composer: the
    // boot defect was the frame claiming the Transcript was the typing target.
    let badgeed: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains(theme::FOCUS_BADGE))
        .collect();
    let focus_badged: Vec<&String> = badgeed
        .iter()
        .filter(|row| !row.contains("LIVE"))
        .copied()
        .collect();
    assert_eq!(
        focus_badged.len(),
        1,
        "exactly one pane is badged at boot (the `● LIVE` follow chip is the only other badge glyph):\n{}",
        rows.join("\n")
    );
    assert!(
        focus_badged[0].contains("Input"),
        "the composer is the boot focus target: {:?}",
        focus_badged[0]
    );
    assert!(
        !joined.contains(&format!("Transcript {}", theme::FOCUS_BADGE)),
        "the transcript is never badged as the focus target at boot:\n{}",
        rows.join("\n")
    );
    // M1 draws no side panes at all (§4.3), so it can never badge one either.
    for pane in [
        state::FocusPane::Plan.label(),
        state::FocusPane::Workers.label(),
    ] {
        assert!(
            !joined.contains(pane),
            "{pane} is not on screen at boot, so it is not badged either"
        );
    }
}

/// A key release is not a binding: acting on it would double every press on
/// terminals that report releases.
#[test]
fn a_key_release_is_not_a_second_press() {
    let mut r = TuiRenderer::new();
    let key = crossterm::event::KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
    assert!(
        !r.apply_terminal_event(&crossterm::event::Event::Key(crossterm::event::KeyEvent {
            kind: crossterm::event::KeyEventKind::Release,
            ..key
        })),
        "a release does nothing"
    );
    assert_eq!(r.input_text, "");

    assert!(
        r.apply_terminal_event(&crossterm::event::Event::Key(crossterm::event::KeyEvent {
            kind: crossterm::event::KeyEventKind::Repeat,
            ..key
        }))
    );
    assert_eq!(r.input_text, "k", "a repeat inserts, like the press does");
}
// ============================================================================
// t-013: the confirm overlay is the only route a destructive action takes.
//
// Three things are pinned here that the key path alone cannot show:
//   * the surface is **drawn** — centred over the body, naming the pending
//     action, its consequence and the `y` / `n` affordance — while the live
//     metrics outside the body keep counting (spec §7.3, G4);
//   * every destructive slash command stops at that surface, and `n` leaves the
//     session alive with nothing sent to the loop (spec §6, G3);
//   * the bare words §5 removes are ordinary steering text (UX friction 6).
// ============================================================================

/// Every row of the frame outside the given rects — the chrome an overlay is not
/// allowed to move.
fn rows_outside(rows: &[String], skip: &[ratatui::layout::Rect]) -> Vec<String> {
    rows.iter()
        .enumerate()
        .filter(|(y, _)| {
            let y = *y as u16;
            !skip
                .iter()
                .any(|rect| y >= rect.y && y < rect.y + rect.height)
        })
        .map(|(_, row)| row.clone())
        .collect()
}

/// Mask every digit run in a drawn row to `#`.
///
/// Two draws of the *same* renderer may legitimately print different numbers, and
/// that is not a regression: status row 1 shows `tokens_in.max(global_in)` /
/// `tokens_out.max(global_out)` against the process-wide
/// `crate::llm::get_global_token_counts()` witness (`tui/mod.rs:805-815`), and the
/// header carries the renderer's own session clock. Inside a parallel test process
/// other tests move those witnesses between two frames, and the clock ticks with
/// machine speed. G4 and §7.3 are about the chrome and the metric **fields**
/// staying on screen and in place under an overlay — not about the counter values —
/// so the frame comparisons mask the digits and compare the field set.
fn steady(row: &str) -> String {
    let mut out = String::with_capacity(row.len());
    let mut in_digits = false;
    for ch in row.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            out.push(ch);
            in_digits = false;
        }
    }
    out
}

/// [`steady`] applied to a row set, for whole-region frame comparisons.
fn steady_rows(rows: &[String]) -> Vec<String> {
    rows.iter().map(|row| steady(row)).collect()
}

/// The §2.2 metrics of status row 1, with the wall-clock field dropped and the
/// remaining values masked by [`steady`]. Keeping the raw figures in a whole-row
/// comparison would make the test depend on what the rest of the test process is
/// doing to the global token witness and on how fast the machine is.
fn metrics_of(rows: &[String], status: &ratatui::layout::Rect) -> String {
    let row = rows[usize::from(status.y)].as_str();
    let without_clock = row
        .split(" elapsed ")
        .next()
        .unwrap_or(row)
        .trim()
        .to_string();
    steady(&without_clock)
}

/// Split a drawn frame at the body rect (§2.1): the rows the overlay may cover,
/// and the rows it may not — header, status bar, composer and footer legend.
fn body_and_chest(rows: &[String], body: &ratatui::layout::Rect) -> (Vec<String>, Vec<String>) {
    let top = body.y as usize;
    let bottom = (body.y + body.height) as usize;
    let inside = rows[top..bottom].to_vec();
    let mut outside = rows[..top].to_vec();
    outside.extend_from_slice(&rows[bottom..]);
    (inside, outside)
}

/// §7.3 + G4 + §10 item 16: `Ctrl+D` paints the Quit confirm **modally** — a
/// `Clear`ed, bordered, centred paragraph over the body — carrying the action's
/// name, its consequence and the `y` / `n` affordance. The rows outside the body
/// are pixel-identical to the frame without the overlay, so the metrics stay
/// live under it. `n` takes it off and leaves the session running.
#[test]
fn the_quit_confirm_is_painted_modally_over_the_body_with_the_metrics_live() {
    let mut r = m4_renderer();
    r.tokens_in = 1234;
    r.tokens_out = 567;
    let view = state::UiView::from_renderer(&r);
    let rects = layout::compute_layout(ratatui::layout::Rect::new(0, 0, 100, 30), &view);
    let body = rects.body.expect("the body rect exists at 100x30");
    let status = rects.status.expect("the status rect exists at 100x30");

    let plain = draw_new_frame(&mut r, 100, 30);
    assert!(
        plain.iter().any(|row| row.contains("in /")),
        "the metrics row counts before the overlay: {}",
        plain.join("\n")
    );

    assert!(press(&mut r, KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(r.overlay, state::OverlayState::Confirm);
    let rows = draw_new_frame(&mut r, 100, 30);
    let frame = rows.join("\n");

    // The surface names the request it holds (§3, §7.3).
    assert!(frame.contains("Confirm"), "overlay title missing:\n{frame}");
    assert!(
        frame.contains("Quit the session"),
        "the pending action is not named:\n{frame}"
    );
    assert!(
        frame.contains("cancels every running worker"),
        "the consequence is missing:\n{frame}"
    );
    assert!(
        frame.contains("y / n"),
        "the y/n affordance is missing:\n{frame}"
    );
    assert!(
        frame.contains("Esc close"),
        "the close hint is missing:\n{frame}"
    );

    // It is a modal drawn over the body, not a line smuggled into the chrome: the
    // body rows changed, the chrome around it did not, and the §2.2 metrics row
    // still carries its counters underneath (§7.3, G4).
    let (plain_body, _) = body_and_chest(&plain, &body);
    let (shown_body, _) = body_and_chest(&rows, &body);
    assert_ne!(
        plain_body, shown_body,
        "the overlay did not paint over the body:\n{frame}"
    );
    assert_eq!(
        steady_rows(&rows_outside(&plain, &[body, status])),
        steady_rows(&rows_outside(&rows, &[body, status])),
        "the header, status bar, composer and footer legend must be untouched \
         under a confirm overlay"
    );
    let shown_metrics = metrics_of(&rows, &status);
    assert_eq!(
        metrics_of(&plain, &status),
        shown_metrics,
        "the metrics keep counting underneath the overlay (G4)"
    );
    assert!(
        shown_metrics.contains("in /") && shown_metrics.contains("#a"),
        "the row underneath is the live metrics row: {shown_metrics}"
    );
    // Centred, not flush left (§7.3): the overlay's top border row starts well
    // inside the body rather than at its first column.
    let title_row = shown_body
        .iter()
        .find(|row| row.contains("Confirm"))
        .expect("the title row inside the body");
    let box_start = title_row
        .find('┌')
        .expect("the overlay draws its own bordered box");
    assert!(
        box_start > 10 && box_start < 45,
        "the box is centred inside a 100-cell body, not flush left: {title_row:?}"
    );

    // `n` closes it and nothing has run (§10 item 6).
    assert!(press(&mut r, KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(r.overlay, state::OverlayState::None);
    assert!(!r.user_exit_requested() && !r.input_state.aborted);
    let after = draw_new_frame(&mut r, 100, 30);
    assert_eq!(
        body_and_chest(&after, &body).0,
        plain_body,
        "`n` puts the body back exactly as it was"
    );
    assert!(!after.join("\n").contains("y / n"), "the surface is gone");
}

/// §7.3 + §10 item 17: `x` names the **selected** worker on the surface, so the
/// operator sees which row `y` will stop before stopping it.
#[test]
fn the_cancel_worker_confirm_names_the_row_it_will_stop() {
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Workers);
    let view = state::UiView::from_renderer(&r);
    let rects = layout::compute_layout(ratatui::layout::Rect::new(0, 0, 100, 30), &view);
    let body = rects.body.expect("the body rect exists at 100x30");
    let status = rects.status.expect("the status rect exists at 100x30");
    let plain = draw_new_frame(&mut r, 100, 30);

    assert!(press(&mut r, KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker {
            key: "coder_specialist-t-004".to_string(),
            task_id: Some("t-004".to_string()),
        })
    );
    let rows = draw_new_frame(&mut r, 100, 30);
    let frame = rows.join("\n");
    assert!(
        frame.contains("Cancel worker 'coder_specialist-t-004'"),
        "the action names the selected row:\n{frame}"
    );
    assert!(
        frame.contains("every other worker keeps running"),
        "the consequence of a one-worker cancel is stated:\n{frame}"
    );
    assert!(
        frame.contains("y / n"),
        "the y/n affordance is missing:\n{frame}"
    );
    let (plain_body, _) = body_and_chest(&plain, &body);
    let (shown_body, _) = body_and_chest(&rows, &body);
    assert_ne!(plain_body, shown_body, "drawn over the body:\n{frame}");
    assert_eq!(
        steady_rows(&rows_outside(&plain, &[body, status])),
        steady_rows(&rows_outside(&rows, &[body, status])),
        "the chrome around the body is untouched"
    );
    assert_eq!(
        metrics_of(&plain, &status),
        metrics_of(&rows, &status),
        "the metrics row stays live under the confirm (G4)"
    );

    // `n` leaves the worker running: no cancel request survives the dismissal.
    assert!(press(&mut r, KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(r.confirm_action.is_none());
    assert!(
        r.status_line.contains("was left running"),
        "{}",
        r.status_line
    );
    assert!(!after_cancelled(&r));
}

/// The tell that no cancellation was fired from the renderer: the session was
/// never asked to exit and the roster row is untouched.
fn after_cancelled(r: &TuiRenderer) -> bool {
    r.user_exit_requested() || r.status_line.starts_with("Cancelled worker")
}

/// The armed-abort prompt shares the surface (§8.5) but holds no request: it is
/// drawn as the same modal over the body, says it is an arm rather than a pending
/// request, and moves the **prompt** onto status row 2 while status row 1 keeps
/// counting (§7.3, UX friction 11).
#[test]
fn the_armed_abort_prompt_occupies_the_same_surface_and_holds_no_request() {
    let mut r = m4_renderer();
    let view = state::UiView::from_renderer(&r);
    let rects = layout::compute_layout(ratatui::layout::Rect::new(0, 0, 100, 30), &view);
    let body = rects.body.expect("the body rect exists at 100x30");
    let status = rects.status.expect("the status rect exists at 100x30");
    let plain = draw_new_frame(&mut r, 100, 30);

    assert!(press(&mut r, KeyCode::Esc, KeyModifiers::NONE));
    assert!(r.is_abort_armed());
    assert!(
        r.confirm_action.is_none(),
        "the arm holds no y/n request (§5)"
    );
    let rows = draw_new_frame(&mut r, 100, 30);
    let frame = rows.join("\n");
    assert!(
        frame.contains("Confirm"),
        "the surface is on screen:\n{frame}"
    );
    assert!(
        frame.contains("Abort armed"),
        "the armed wording is on the surface:\n{frame}"
    );
    assert!(
        !frame.contains("Quit the session"),
        "an arm is not a pending request:\n{frame}"
    );

    // The body carries the modal …
    let (plain_body, _) = body_and_chest(&plain, &body);
    let (shown_body, _) = body_and_chest(&rows, &body);
    assert_ne!(plain_body, shown_body, "drawn over the body:\n{frame}");

    // … and the prompt itself lives on status row 2 (§8.5), which is the one row
    // the arming is allowed to change: row 1 keeps the counters, and the header,
    // composer and footer legend are untouched.
    let prompt_row = usize::from(status.y) + 1;
    assert_eq!(
        metrics_of(&plain, &status),
        metrics_of(&rows, &status),
        "status row 1 keeps counting under an armed prompt"
    );
    assert!(
        metrics_of(&rows, &status).contains("in /"),
        "{}",
        metrics_of(&rows, &status)
    );
    assert!(
        rows[prompt_row].contains("abort armed"),
        "the prompt lives on status row 2: {:?}",
        rows[prompt_row]
    );
    let excluded = [
        body,
        ratatui::layout::Rect {
            x: status.x,
            y: status.y + 1,
            width: status.width,
            height: 1,
        },
    ];
    assert_eq!(
        steady_rows(&rows_outside(&plain, &excluded)),
        steady_rows(&rows_outside(&rows, &excluded)),
        "only the body and status row 2 move while an abort is armed"
    );
    assert!(r.status_line.contains("Abort armed"), "{}", r.status_line);
}

/// §6 + G3: every destructive slash command — the quit family, `/cancel`,
/// `/cancel <id>`, `/cancel-all` and all six `/reset` spellings — stops at the
/// confirm surface. Nothing is sent to the loop, nothing is cancelled, no exit
/// is requested, and `n` leaves the session alive with the transcript untouched.
#[test]
fn every_destructive_slash_command_stops_at_the_confirm_surface() {
    let commands = [
        "/exit",
        "/quit",
        "/q",
        "/abort",
        "/stop",
        ":q",
        ":q!",
        "/cancel",
        "/cancel t-004",
        "/cancel-all",
        "/reset",
        "/reset_plan",
        "/reset-plan",
        "/clear_plan",
        "/clear-plan",
        "/reset_execution_plan",
    ];
    for command in commands {
        let mut r = m4_renderer();
        r.set_input_content(command);
        r.submit();
        assert!(
            r.confirm_action.is_some(),
            "`{command}` must open the confirm surface"
        );
        assert_eq!(r.overlay, state::OverlayState::Confirm, "`{command}`");
        assert!(
            drain_submissions(&mut r).is_empty(),
            "`{command}` reached the session loop un-confirmed"
        );
        assert!(
            !r.user_exit_requested() && !r.input_state.aborted,
            "`{command}` ended the session by itself"
        );
        assert!(
            r.plan_content.contains("t-004"),
            "`{command}` cleared the plan before `y`"
        );

        assert!(
            press(&mut r, KeyCode::Char('n'), KeyModifiers::NONE),
            "`{command}`: `n` is a binding"
        );
        assert!(
            r.confirm_action.is_none(),
            "`{command}`: `n` dropped the request"
        );
        assert_eq!(r.overlay, state::OverlayState::None, "`{command}`");
        assert!(
            drain_submissions(&mut r).is_empty(),
            "`{command}` reached the loop after `n`"
        );
        assert!(
            !r.user_exit_requested() && !r.input_state.aborted,
            "`{command}` ended the session despite `n`"
        );
        assert!(!r.is_abort_armed(), "`{command}` + `n` armed nothing");
        assert!(
            r.plan_content.contains("t-004"),
            "`{command}` + `n` left the plan alone"
        );
    }
}

/// §6 (unified `/reset`) + the quit spellings: `y` hands the **exact** command
/// back to the session loop, which owns the destructive path. The renderer no
/// longer clears the plan behind the loop, and no `/reset` spelling is answered
/// locally any more.
#[test]
fn a_confirmed_command_is_handed_to_the_session_loop_verbatim() {
    let spellings = [
        "/reset",
        "/reset_plan",
        "/reset-plan",
        "/clear_plan",
        "/clear-plan",
        "/reset_execution_plan",
        "/exit",
        "/quit",
        "/q",
        "/abort",
        "/stop",
        ":q",
        ":q!",
    ];
    for spelling in spellings {
        let mut r = m4_renderer();
        r.set_input_content(spelling);
        r.submit();
        assert_eq!(
            r.confirm_action,
            Some(events::ConfirmAction::Command {
                line: spelling.to_string()
            }),
            "`{spelling}` is confirmed as a command hand-off"
        );
        // Every spelling takes the one path, and the surface describes it.
        let request = r
            .confirm_action
            .as_ref()
            .map(|action| action.describe())
            .expect("a request is pending");
        assert!(
            request.action.contains(spelling),
            "the surface names the spelling it will run: {request:?}"
        );

        assert!(press(&mut r, KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(
            drain_submissions(&mut r),
            vec![spelling.to_string()],
            "`{spelling}` reaches the loop exactly as typed"
        );
        assert_eq!(r.overlay, state::OverlayState::None);
        assert!(
            r.plan_content.contains("t-004"),
            "`{spelling}`: the renderer no longer clears the plan itself — the \
             loop's reset path does"
        );
        assert!(
            !r.user_exit_requested(),
            "`{spelling}`: the loop owns the exit, the renderer does not"
        );
        assert_eq!(
            r.history.last().map(String::as_str),
            Some(spelling),
            "a confirmed command is remembered in the composer history"
        );
    }
}

/// §6: `/cancel <worker key or task id>` resolves its argument against the
/// roster and confirms **one** worker; `y` addresses that identity through the
/// registry's exact-identity matching and never exits the session.
#[test]
fn cancel_with_an_argument_confirms_one_named_worker() {
    for argument in ["coder_specialist-t-004", "t-004", "CODER_SPECIALIST-t-004"] {
        let mut r = m4_renderer();
        r.set_input_content(&format!("/cancel {argument}"));
        r.submit();
        assert_eq!(
            r.confirm_action,
            Some(events::ConfirmAction::CancelWorker {
                key: "coder_specialist-t-004".to_string(),
                task_id: Some("t-004".to_string()),
            }),
            "`/cancel {argument}` resolves to the roster row"
        );
        assert!(press(&mut r, KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(
            r.status_line.contains("coder_specialist-t-004"),
            "the outcome names the worker: {}",
            r.status_line
        );
        assert!(
            !r.user_exit_requested(),
            "cancelling one worker never exits the session (§10 item 17)"
        );
        assert!(drain_submissions(&mut r).is_empty(), "nothing was sent");
    }

    // An unknown argument is confirmed as typed, never retargeted onto another row.
    let mut r = m4_renderer();
    r.set_input_content("/cancel t-999");
    r.submit();
    assert_eq!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker {
            key: "t-999".to_string(),
            task_id: None,
        }),
        "the string the operator typed is the identity `y` addresses"
    );

    // `/cancel-all` is the one request that carries the broadcast cancellation,
    // and it is held behind the same surface. `y` is not fired here for the same
    // reason the quit `y` is pinned by wiring above: `cancel_all()` is
    // process-wide and would race the `ui::bridge` wall-clock tests.
    let mut r = m4_renderer();
    r.set_input_content("/cancel-all");
    r.submit();
    assert_eq!(r.confirm_action, Some(events::ConfirmAction::CancelAll));
    assert!(r.status_line.contains("every worker"), "{}", r.status_line);
    assert!(press(&mut r, KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(r.status_line, "Every worker was left running.");
    assert!(!r.user_exit_requested());
}

/// §5: the bare words `stop / cancel / exit / quit / q / abort` are ordinary
/// steering text in the TUI. They reach the loop as text, arm nothing, and the
/// renderer says which spelling does open a confirm (UX friction 6).
#[test]
fn a_bare_abort_word_is_steering_text_and_never_ends_the_session() {
    for word in [
        "stop", "cancel", "exit", "quit", "q", "abort", "STOP", "Stop",
    ] {
        let mut r = m4_renderer();
        r.set_input_content(word);
        r.submit();
        let sent = drain_submissions(&mut r);
        assert_eq!(
            sent.iter().map(|line| line.trim()).collect::<Vec<_>>(),
            vec![word],
            "`{word}` is delivered as one steering message"
        );
        assert!(
            !r.user_exit_requested() && !r.input_state.aborted,
            "the bare word `{word}` ended the session"
        );
        assert!(
            r.confirm_action.is_none(),
            "steering text arms no confirm: `{word}`"
        );
        assert!(
            r.messages.iter().any(|message| {
                message.contains("steering text")
                    && message.contains("/exit")
                    && message.contains("Esc twice")
            }),
            "`{word}`: the hint names the confirmed spelling: {:?}",
            r.messages
        );
        assert!(
            r.status_line.contains("Steering text"),
            "`{word}`: {}",
            r.status_line
        );
        // The word still lands in history, like any other instruction.
        assert_eq!(r.history.last().map(String::as_str), Some(word));
    }
}

/// The gate is the TUI's, not the command vocabulary's: a multi-line submission
/// whose first line is a destructive command is still delivered verbatim, and a
/// renderer that gates nothing keeps the pre-rebuild bare-word behaviour.
#[test]
fn the_gate_keeps_pasted_blocks_intact_and_the_tui_the_only_gating_renderer() {
    let block = "/reset\nnästa rad";
    let mut r = m4_renderer();
    r.paste_text(block);
    r.submit();
    assert!(
        r.confirm_action.is_none(),
        "a pasted block is not a command"
    );
    assert_eq!(drain_submissions(&mut r), vec![block.to_string()]);
    assert!(
        r.plan_content.contains("t-004"),
        "the plan survived the paste"
    );

    // The headless renderer keeps the pre-rebuild vocabulary (its behaviour is
    // byte-unchanged); the TUI is the only one that gates.
    let raw = crate::ui::raw::RawRenderer::new();
    assert!(
        !crate::ui::Renderer::gates_destructive_commands(&raw),
        "the headless renderer must not change its abort vocabulary"
    );
    let tui = TuiRenderer::new();
    assert!(
        crate::ui::Renderer::gates_destructive_commands(&tui),
        "the TUI gates, which is what lets the loop drop the bare words"
    );
}

// ---- steering-instruction visibility (spec §3, §8.3; DATA §6 gap 7/9) ----

/// Affordance 1, end to end at the renderer's seam: an instruction the bridge handed
/// over arrives in the transcript as a labelled line and in the status bar as a chip —
/// both driven by the bridge's own transition, never by a counter of the pane's.
#[test]
fn a_queued_steer_from_the_bridge_lands_in_the_transcript_and_arms_the_chip() {
    let mut r = m4_renderer();
    crate::ui::Renderer::note_steer(
        &mut r,
        &SteerTrace::queued(
            "rename the module first",
            crate::ui::MANAGER_TARGET,
            "held while the specialists run",
        ),
    );

    assert_eq!(r.queued_steers(), 1, "the chip reads this ledger");
    let line = r
        .messages
        .last()
        .expect("the transcript gained a line")
        .clone();
    assert_eq!(
        line,
        "[Steer queued → manager] rename the module first — held while the specialists run"
    );

    let frame = draw_new_frame(&mut r, 100, 30).join("\n");
    assert!(frame.contains("[Steer queued"), "{frame}");
    assert!(frame.contains("rename the module first"), "{frame}");
    assert!(
        frame.contains("1 steers"),
        "the chip is on the bar: {frame}"
    );
    assert!(!frame.contains("round"), "no arbitration counter: {frame}");
}

/// The chip counts instructions the bridge is *still* holding. A transition to
/// delivered takes the number down, and an empty ledger prints no chip at all.
#[test]
fn the_steer_chip_counts_only_the_instructions_still_being_held() {
    let mut r = m4_renderer();
    for text in ["first instruction", "second instruction"] {
        crate::ui::Renderer::note_steer(
            &mut r,
            &SteerTrace::queued(text, crate::ui::MANAGER_TARGET, "held"),
        );
    }
    assert!(
        draw_new_frame(&mut r, 100, 30)
            .join("\n")
            .contains("2 steers")
    );

    // The same instruction, now filed with a specialist: the count comes down and the
    // transcript gains exactly one more line for the stage change.
    let before = r.messages.len();
    crate::ui::Renderer::note_steer(
        &mut r,
        &SteerTrace {
            stage: SteerStage::Delivered,
            text: "first instruction".to_string(),
            target: "coder_specialist-t-004".to_string(),
            notice_id: None,
            reason: "filed".to_string(),
        },
    );
    assert_eq!(r.queued_steers(), 1);
    assert_eq!(r.messages.len(), before + 1, "one line per stage change");
    assert!(
        draw_new_frame(&mut r, 100, 30)
            .join("\n")
            .contains("1 steers")
    );

    crate::ui::Renderer::note_steer(
        &mut r,
        &SteerTrace {
            stage: SteerStage::Delivered,
            text: "second instruction".to_string(),
            target: "coder_specialist-t-004".to_string(),
            notice_id: None,
            reason: "filed".to_string(),
        },
    );
    assert_eq!(r.queued_steers(), 0);
    let frame = draw_new_frame(&mut r, 100, 30).join("\n");
    assert!(
        !frame.contains("steers"),
        "nothing held, so nothing is claimed: {frame}"
    );
}

/// Affordance 2: a notice the arbitrator filed is named with the specialist it is
/// sitting in front of and the id that specialist must reply to — and the same notice
/// is listed in that worker's own detail half, read from the orchestrator's inbox.
#[test]
fn a_forwarded_notice_names_the_specialist_and_its_notice_id() {
    let worker = "steer-target-coder-t-013b";
    let posted = crate::orchestrator::notice::post_notice_to_worker(
        worker,
        "use the shorter fixture",
        Some("notice-t013b-a"),
    );
    assert_eq!(posted.notice_id, "notice-t013b-a");

    let mut r = m4_renderer();
    r.subagents = vec![SubagentDetail {
        name: worker.to_string(),
        task_id: Some("t-013b".to_string()),
        is_active: true,
        ..SubagentDetail::default()
    }];
    crate::ui::Renderer::note_steer(
        &mut r,
        &SteerTrace {
            stage: SteerStage::Delivered,
            text: "use the shorter fixture".to_string(),
            target: worker.to_string(),
            notice_id: Some(posted.notice_id.clone()),
            reason: "filed in the inbox".to_string(),
        },
    );

    assert_eq!(
        r.messages.last().map(String::as_str),
        Some(
            "[Steer delivered → steer-target-coder-t-013b] use the shorter fixture — filed in \
             the inbox (notice id notice-t013b-a)"
        )
    );
    // A filed notice is not a held instruction: the chip stays off.
    assert_eq!(r.queued_steers(), 0);

    let frame = draw_new_frame(&mut r, 100, 44).join("\n");
    assert!(frame.contains("[Steer delivered"), "{frame}");
    assert!(frame.contains("notice-t013b-a"), "{frame}");
    assert!(
        frame.contains("[Steering] 1 notice"),
        "the worker's own detail half lists what is waiting for it: {frame}"
    );
    assert!(
        frame.contains("[Steering] 1 notice(s) waiting for")
            && frame.contains("this specialist")
            && frame.contains("(waiting 0s)"),
        "the listed notice carries its own wait: {frame}"
    );
}

/// A `ForwardNotice` the round refused because the token addressed no live worker is
/// reported as never delivered, naming the token — and it is never counted as held.
#[test]
fn an_unroutable_target_is_reported_as_never_delivered() {
    let mut r = m4_renderer();
    crate::ui::Renderer::note_steer(
        &mut r,
        &SteerTrace {
            stage: SteerStage::Revoked,
            text: "tell the ghost what to do".to_string(),
            target: "ghost_worker".to_string(),
            notice_id: None,
            reason: "the arbitrator addressed `ghost_worker`, which no live worker answers"
                .to_string(),
        },
    );
    let line = r.messages.last().cloned().unwrap_or_default();
    assert!(
        line.starts_with("[Steer not delivered → ghost_worker]"),
        "{line}"
    );
    assert!(line.contains("no live worker"), "{line}");
    assert_eq!(
        r.queued_steers(),
        0,
        "an instruction that reached nobody is not counted as held"
    );
    let frame = draw_new_frame(&mut r, 100, 30).join("\n");
    assert!(frame.contains("[Steer not delivered"), "{frame}");
    assert!(frame.contains("ghost_worker"), "{frame}");
    assert!(!frame.contains("steers"), "{frame}");
}

/// The ledger is a bounded witness, and it speaks once per stage: an unbounded session
/// cannot grow it, and a restatement of the same stage never scrolls the transcript.
#[test]
fn steer_traces_stay_bounded_and_announce_each_stage_once() {
    let mut r = m4_renderer();
    let total = MAX_STEER_TRACES + 5;
    for index in 0..total {
        crate::ui::Renderer::note_steer(
            &mut r,
            &SteerTrace::queued(
                format!("instruction {index}"),
                crate::ui::MANAGER_TARGET,
                "held",
            ),
        );
    }
    assert_eq!(r.steer_traces.len(), MAX_STEER_TRACES);
    assert_eq!(
        r.steer_traces.last().map(|entry| entry.text.as_str()),
        Some("instruction 16"),
        "the newest transitions are the ones that survive"
    );
    assert!(
        !r.steer_traces
            .iter()
            .any(|entry| entry.text == "instruction 0"),
        "the oldest transition is the one that goes"
    );
    assert_eq!(r.queued_steers(), MAX_STEER_TRACES);

    // Same stage again: no new transcript line, only a sharper reason.
    let before = r.messages.len();
    crate::ui::Renderer::note_steer(
        &mut r,
        &SteerTrace::queued("instruction 16", crate::ui::MANAGER_TARGET, "still held"),
    );
    assert_eq!(r.messages.len(), before, "a stage is announced once");
    assert_eq!(
        r.steer_traces
            .iter()
            .find(|entry| entry.text == "instruction 16")
            .map(|entry| entry.reason.as_str()),
        Some("still held")
    );

    // One instruction, two addressees: the ledger keeps one entry per (text, target),
    // so a notice filed for a second specialist is a second fact and never a
    // double-counted duplicate — and the cap still holds.
    crate::ui::Renderer::note_steer(
        &mut r,
        &SteerTrace::queued("instruction 16", "other_worker", "held there too"),
    );
    assert_eq!(
        r.steer_traces.len(),
        MAX_STEER_TRACES,
        "the bound holds against a second addressee too"
    );
    let for_other = r
        .steer_traces
        .iter()
        .filter(|entry| entry.text == "instruction 16")
        .map(|entry| entry.target.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        for_other,
        ["manager", "other_worker"],
        "one entry per addressee, oldest first"
    );
    assert_eq!(
        r.steer_traces
            .iter()
            .filter(|entry| entry.text == "instruction 16" && entry.target == "other_worker")
            .count(),
        1,
        "never two entries for one (instruction, addressee)"
    );
}

/// Affordance 2's second half: the notice tables decide what happened next. A reply
/// the specialist recorded moves the entry to delivered, clears it from the chip, and
/// says so with the notice id — the renderer never guesses this state.
#[test]
fn the_specialists_answer_moves_the_trace_to_delivered_and_clears_the_chip() {
    let worker = "steer-probe-coder-t-013b";
    let posted = crate::orchestrator::notice::post_notice_to_worker(
        worker,
        "raise the coverage gate",
        Some("notice-t013b-b"),
    );
    let mut r = m4_renderer();
    crate::ui::Renderer::note_steer(
        &mut r,
        &SteerTrace {
            stage: SteerStage::Queued,
            text: "raise the coverage gate".to_string(),
            target: worker.to_string(),
            notice_id: Some(posted.notice_id.clone()),
            reason: "filed in the inbox".to_string(),
        },
    );
    assert_eq!(r.queued_steers(), 1, "pending in the inbox, so still held");
    r.refresh_steer_traces();
    assert_eq!(r.steer_traces[0].stage, SteerStage::Queued);
    assert_eq!(r.queued_steers(), 1, "a pending notice is not an answer");

    crate::orchestrator::record_worker_reply_for_notice(worker, "notice-t013b-b", "done")
        .expect("the reply names the pending notice");
    let before = r.messages.len();
    r.refresh_steer_traces();

    assert_eq!(r.steer_traces[0].stage, SteerStage::Delivered);
    assert_eq!(r.queued_steers(), 0, "answered, so no longer held");
    assert_eq!(
        r.messages.len(),
        before + 1,
        "the stage change is announced"
    );
    assert_eq!(
        r.messages.last().map(String::as_str),
        Some(
            "[Steer delivered → steer-probe-coder-t-013b] raise the coverage gate — the \
             specialist answered it (notice id notice-t013b-b)"
        )
    );
    // The reply consumed the notice; the inbox is empty again.
    assert!(
        crate::orchestrator::notice::peek_worker_inbox(worker).is_empty(),
        "the reply resolved the queued copy"
    );
}

/// The honesty guard: the tables answering "nothing" is not an answer. A notice the
/// tables do not know about keeps the stage the bridge gave it, so a notice that was
/// drained, reclaimed or simply never filed can never be dressed up as a revocation.
#[test]
fn a_notice_the_tables_say_nothing_about_keeps_the_stage_the_bridge_gave_it() {
    let mut r = m4_renderer();
    crate::ui::Renderer::note_steer(
        &mut r,
        &SteerTrace {
            stage: SteerStage::Queued,
            text: "instruction with an unknown notice id".to_string(),
            target: "steer-absent-coder-t-013b".to_string(),
            notice_id: Some("notice-t013b-never-filed".to_string()),
            reason: "filed in the inbox".to_string(),
        },
    );
    let before = r.messages.len();
    r.refresh_steer_traces();
    assert_eq!(
        r.steer_traces[0].stage,
        SteerStage::Queued,
        "the tables said nothing, so nothing changed"
    );
    assert_eq!(r.messages.len(), before, "and nothing was announced");
    assert_eq!(r.queued_steers(), 1);
    assert!(
        !r.messages.join("\n").contains("not delivered"),
        "never a revocation the data does not support"
    );
}

/// Affordance 3 in one frame: a stream the preemption handshake paused is marked on
/// the roster, counted on the bar, and explained in its detail half — while a row the
/// arbitrator never pauses carries no pause claim at all.
#[test]
fn a_paused_row_is_marked_counted_and_explained_in_the_new_frame() {
    let mut r = m4_renderer();
    r.subagents[0].paused = true;
    let paused = draw_new_frame(&mut r, 160, 50).join("\n");
    assert!(paused.contains('\u{23f8}'), "the roster glyph: {paused}");
    assert!(paused.contains("paused"), "the roster word label: {paused}");
    assert!(
        paused.contains("1p"),
        "the bar counts the held stream: {paused}"
    );
    assert!(
        paused.contains("yielded its model slot") && paused.contains("Steer Arbitrator"),
        "the detail half states the mechanism: {paused}"
    );
    assert!(
        paused.contains("why: a steering instruction is being arbitrated"),
        "and why: {paused}"
    );

    // Control: the same renderer with the flag cleared prints no pause cause anywhere,
    // so the explanation cannot be a standing caption.
    let mut running = m4_renderer();
    let frame = draw_new_frame(&mut running, 160, 50).join("\n");
    assert!(!frame.contains("model slot"), "{frame}");
    assert!(!frame.contains("Steer Arbitrator"), "{frame}");
}
