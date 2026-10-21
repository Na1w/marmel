use super::*;
use ratatui::Terminal;
use ratatui::style::{Color, Modifier};
use std::time::Duration;

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

#[test]
fn cycle_focus_obeys_panel_visibility() {
    let mut r = TuiRenderer::new();
    r.show_plan_panel = true;
    r.plan_content = "plan".to_string();
    r.show_subagent_panel = true;
    r.subagents.push(SubagentDetail {
        name: "coder".to_string(),
        task_id: None,
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
    });

    r.focused_panel = FocusedPanel::Chat;
    r.cycle_focus();
    assert_eq!(r.focused_panel, FocusedPanel::Plan);
    r.cycle_focus();
    assert_eq!(r.focused_panel, FocusedPanel::Subagents);
    r.cycle_focus();
    assert_eq!(r.focused_panel, FocusedPanel::Chat);
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
    }];
    r.set_subagents(authoritative);

    // Local thinking/content preserved; logs adopted.
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

/// Draw the Subagents panel into a `width x height` test terminal and hand back
/// both the text of every terminal row and the style of every cell.
fn draw_subagent_panel(
    r: &mut TuiRenderer,
    width: u16,
    height: u16,
) -> (Vec<String>, Vec<Vec<ratatui::style::Style>>) {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, width, height);
            r.render_subagents(frame, area);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let rows = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect();
    let styles = (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].style()).collect())
        .collect();
    (rows, styles)
}

/// Draw one status line into a 100-column test terminal.
fn draw_status_line(r: &mut TuiRenderer, width: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, 1);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            r.render_status(frame, ratatui::layout::Rect::new(0, 0, width, 1));
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..width)
        .map(|x| buffer[(x, 0)].symbol().to_string())
        .collect()
}

/// The frame set both spinners cycle through.
const SPINNER_FRAMES: [char; 4] = ['-', '\\', '|', '/'];

/// A queued row is labelled with its FIFO position and nothing about the row is
/// otherwise reinterpreted: the ctx count and the work clock keep their place in
/// the label, a row holding its slot still reads `(Active)`, and a finished row
/// still reads `(Idle)`.
#[test]
fn queued_rows_are_labelled_with_their_position_in_the_list() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    let now = std::time::Instant::now();

    let mut queued = queue_row(
        "coder-t-1",
        true,
        Some(crate::ui::QueueState { position: 2, of: 4 }),
    );
    queued.task_id = Some("t-1".to_string());
    queued.context_tokens = 3500;
    queued.started_at = Some(now - Duration::from_secs(5));
    let mut running = queue_row("coder-t-2", true, None);
    running.task_id = Some("t-2".to_string());
    running.context_tokens = 1200;
    let mut idle = queue_row("researcher-t-3", false, None);
    idle.task_id = Some("t-3".to_string());
    idle.context_tokens = 400;
    r.subagents.extend([queued, running, idle]);

    let (rows, _) = draw_subagent_panel(&mut r, 100, 20);
    let text = rows.join("\n");
    assert!(
        text.contains("coder-t-1 (Queued 2/4, 3.5k ctx, 5s)"),
        "expected 'coder-t-1 (Queued 2/4, 3.5k ctx, 5s)' in the subagents panel, got:\n{text}"
    );
    assert!(
        text.contains("coder-t-2 (Active, 1.2k ctx)"),
        "a row that holds its slot must still read (Active), got:\n{text}"
    );
    assert!(
        text.contains("researcher-t-3 (Idle, 400 ctx)"),
        "a finished row must still read (Idle), got:\n{text}"
    );

    let queued_row = rows
        .iter()
        .find(|line| line.contains("coder-t-1"))
        .expect("the queued row is drawn");
    assert!(
        !queued_row.contains("(Active"),
        "a queued row must not claim it holds a slot, got: {queued_row:?}"
    );
    assert!(
        !queued_row.contains("(Idle"),
        "a queued row is still part of the round, got: {queued_row:?}"
    );
}

/// The queued row stays green/bold — it belongs to the current round, so making
/// it grey would drop it out of the panel's whole notion of liveness — while the
/// slot label carries its own colour so the position is impossible to miss.
#[test]
fn a_queued_row_stays_visibly_active_and_colours_only_its_slot_label() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    r.subagents.push(queue_row(
        "coder-t-1",
        true,
        Some(crate::ui::QueueState { position: 2, of: 4 }),
    ));
    r.subagents.push(queue_row("coder-t-2", true, None));

    let (rows, styles) = draw_subagent_panel(&mut r, 100, 20);
    // Row 0 of the list is the first line inside the bordered block (y = 1).
    let row_y = rows
        .iter()
        .position(|line| line.contains("coder-t-1"))
        .expect("the queued row is drawn");
    let name_x = rows[row_y].find("coder-t-1").unwrap();
    let label_x = rows[row_y].find("(Queued").unwrap();

    let name = styles[row_y][name_x];
    assert_eq!(
        name.fg,
        Some(Color::Green),
        "the name of a queued row stays green, so the row still reads as live"
    );
    assert!(
        name.add_modifier.contains(Modifier::BOLD),
        "the name of a queued row stays bold"
    );

    let label = styles[row_y][label_x];
    assert_eq!(
        label.fg,
        Some(Color::LightYellow),
        "the slot label gets its own colour, got: {:?}",
        rows[row_y]
    );

    // The row holding its slot keeps the plain green label: the queued colour is
    // never applied to a neighbour just because a queue exists.
    let running_y = rows
        .iter()
        .position(|line| line.contains("coder-t-2"))
        .expect("the running row is drawn");
    let running_x = rows[running_y].find("(Active").unwrap();
    assert_eq!(styles[running_y][running_x].fg, Some(Color::LightGreen));
}

/// The detail pane says what the selected row is waiting on, with the same
/// position, instead of the generic "waiting for model response" line — and the
/// spinner keeps turning through the wait.
#[test]
fn the_detail_pane_spells_out_the_slot_wait() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    r.subagents.push(queue_row(
        "coder-t-1",
        true,
        Some(crate::ui::QueueState { position: 3, of: 5 }),
    ));

    let (rows, _) = draw_subagent_panel(&mut r, 100, 20);
    let text = rows.join("\n");
    const WAIT: &str = "[Status: Queued 3/5 - waiting for a backend slot...]";
    assert!(
        text.contains(WAIT),
        "expected {WAIT:?} in the subagent status bar, got:\n{text}"
    );
    assert!(
        !text.contains("waiting for model response"),
        "a row waiting for a slot must not claim it is waiting on the model:\n{text}"
    );

    let bar = rows
        .iter()
        .find(|line| line.contains(WAIT))
        .expect("the status bar row");
    let spinner = bar[bar.find(WAIT).unwrap() + WAIT.len()..]
        .trim_start()
        .chars()
        .next()
        .unwrap_or(' ');
    assert!(
        SPINNER_FRAMES.contains(&spinner),
        "a queued row keeps a live spinner, got {spinner:?} in {bar:?}"
    );
}

/// A row that is not running never advertises a slot position: the queue belongs
/// to a live delegation only.
#[test]
fn a_row_that_is_not_running_never_shows_a_queue_position() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    r.subagents.push(queue_row(
        "coder-t-1",
        false,
        Some(crate::ui::QueueState { position: 3, of: 5 }),
    ));

    let (rows, _) = draw_subagent_panel(&mut r, 100, 20);
    let text = rows.join("\n");
    assert!(
        text.contains("coder-t-1 (Idle)"),
        "the row reads Idle whatever it once held, got:\n{text}"
    );
    assert!(
        !text.contains("Queued"),
        "no queue label for a row that is not running, got:\n{text}"
    );
}

/// A queued delegation is still dispatched, so it still counts in the
/// "[N active: …]" activity group — the clocks and the activity count keyed on
/// `is_active` must not move when a row waits for a slot.
#[test]
fn a_queued_row_still_counts_as_active_in_the_main_status_line() {
    let mut r = TuiRenderer::new();
    r.subagents.push(queue_row(
        "coder-t-1",
        true,
        Some(crate::ui::QueueState { position: 1, of: 3 }),
    ));
    r.subagents.push(queue_row("coder-t-2", true, None));
    r.status_line = "Delegating to specialist coder-t-1 for t-1".to_string();

    let content = draw_status_line(&mut r, 160);
    assert!(
        content.contains("[2 active: coder-t-1, coder-t-2]"),
        "the queued row must stay in the active group, got: {content:?}"
    );
}

/// A round in which every session is waiting for a backend slot has to animate
/// exactly like a streaming round: the gate reports the wait as status traffic,
/// and that traffic is what holds the spinner up.
#[test]
fn a_queued_only_round_keeps_the_status_line_animating() {
    let mut r = TuiRenderer::new();
    // The exact sentence the concurrency gate emits for a slot wait.
    r.on_event(&Event::Status(
        "queued for a backend slot (2 of 4) — Manager".to_string(),
    ));
    assert!(
        r.waiting_for_token_since.is_some(),
        "waiting for a slot counts as waiting on the backend"
    );

    let content = draw_status_line(&mut r, 160);
    assert!(
        content.contains("queued for a backend slot (2 of 4)"),
        "the slot wait is surfaced in the status line, got: {content:?}"
    );
    let tail = content.trim_end();
    assert!(
        tail.ends_with(SPINNER_FRAMES),
        "the activity spinner must keep turning for a queued-only round, got: {tail:?}"
    );

    // Control: the spinner is not simply always on.
    let mut idle = TuiRenderer::new();
    idle.on_event(&Event::Status("Ready".to_string()));
    let idle_tail = draw_status_line(&mut idle, 80).trim_end().to_string();
    assert!(
        !idle_tail.ends_with(SPINNER_FRAMES),
        "an idle status line must not animate, got: {idle_tail:?}"
    );
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

/// Queue positions are re-reported every time the line ahead moves, so the label
/// changes several times a second. Nothing else may move with it: not the
/// selected row, not the scrolled list window, not the other rows.
#[test]
fn queue_position_updates_never_move_the_selection_or_the_list_window() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    for n in 1..=5 {
        r.subagents
            .push(queue_row(&format!("coder-t-{n}"), true, None));
    }
    // Five rows in a three-line window: the selection has to be scrolled into
    // view, which is exactly the state a reselect would silently destroy.
    r.selected_subagent_idx = 4;

    let (before, _) = draw_subagent_panel(&mut r, 100, 20);
    assert_eq!(
        r.subagent_list_scroll, 2,
        "the selected row is scrolled into view once"
    );
    let selected_line = before
        .iter()
        .position(|line| line.contains("> coder-t-5"))
        .expect("the selected row is visible");
    assert!(
        !before.iter().any(|line| line.contains("coder-t-1")),
        "the first rows are scrolled out of the window"
    );

    // The list window, in draw order: exactly the rows carrying a slot or
    // lifecycle label (the detail header and the detail status bar never do).
    // Panel borders are trimmed off both ends so the expectation is the row text.
    let window = |lines: &[String]| -> Vec<String> {
        lines
            .iter()
            .filter(|line| {
                line.contains("(Queued ") || line.contains("(Active)") || line.contains("(Idle")
            })
            .map(|line| line.trim_matches('│').trim_end().to_string())
            .collect()
    };
    assert_eq!(
        window(&before),
        [
            "  coder-t-3 (Active)",
            "  coder-t-4 (Active)",
            "> coder-t-5 (Active)",
        ],
        "the window holds the tail of the list, selected on its last row"
    );

    for (position, of) in [(4usize, 6usize), (2, 6), (1, 3)] {
        // Half of the round queues: once through the renderer's own delegation
        // arm, once through the session loop's authoritative push.
        for task in ["t-3", "t-5"] {
            r.on_event(&Event::Delegation(
                crate::orchestrator::DelegationEvent::Queued {
                    agent: crate::agents::Agent::Coder,
                    task: Some(task.to_string()),
                    position,
                    of,
                },
            ));
        }
        let mut authoritative = r.subagents.clone();
        for detail in &mut authoritative {
            detail.queue = match detail.name.as_str() {
                "coder-t-3" | "coder-t-5" => Some(crate::ui::QueueState { position, of }),
                _ => None,
            };
        }
        r.set_subagents(authoritative);

        assert_eq!(
            r.selected_subagent_idx, 4,
            "a slot transition never reselects a row"
        );
        assert_eq!(
            r.subagent_list_scroll, 2,
            "a slot transition never scrolls the list window"
        );

        let (after, _) = draw_subagent_panel(&mut r, 100, 20);
        assert_eq!(
            window(&after),
            vec![
                format!("  coder-t-3 (Queued {position}/{of})"),
                "  coder-t-4 (Active)".to_string(),
                format!("> coder-t-5 (Queued {position}/{of})"),
            ],
            "only the two queued labels change, and only their digits"
        );
        assert_eq!(
            after.iter().position(|line| line.contains("> coder-t-5")),
            Some(selected_line),
            "the selected row stays on the same terminal line"
        );
    }
}

/// A restart keeps the panel rows but not the pool: no semaphore survives, so no
/// position may survive either. The restored row comes back as a running row.
#[test]
fn a_restored_row_never_shows_a_stale_queue_position() {
    let mut live = TuiRenderer::new();
    live.show_subagent_panel = true;
    let mut queued = queue_row(
        "coder-t-1",
        true,
        Some(crate::ui::QueueState { position: 7, of: 9 }),
    );
    queued.task_id = Some("t-1".to_string());
    live.subagents.push(queued);

    let section = live.capture_ui();
    let mut restored = TuiRenderer::new();
    restored.restore_panes(&section);

    assert_eq!(restored.subagents.len(), 1);
    assert_eq!(
        restored.subagents[0].queue, None,
        "the queue ledger does not survive a restart, so the panel may not claim a position"
    );
    assert!(
        restored.subagents[0].is_active,
        "the delegation was still dispatched when the session was captured"
    );

    let (rows, _) = draw_subagent_panel(&mut restored, 100, 20);
    let text = rows.join("\n");
    assert!(
        text.contains("coder-t-1 (Active)"),
        "a restored row comes back as a running row, got:\n{text}"
    );
    assert!(
        !text.contains("Queued"),
        "no stale queue position may survive a restore, got:\n{text}"
    );
}

// ---- steering arbitration pause in the Subagents panel ----

/// A row whose stream yielded its model slot is labelled `Paused` and is
/// otherwise untouched: the name keeps the live green/bold spelling, the ctx
/// count and the work clock keep their place in the label, and a neighbour is
/// never relabelled because one row is held. A stale flag on a closed row still
/// reads `(Idle)` — the label word is gated on liveness, never on the flag alone.
#[test]
fn a_held_stream_is_labelled_paused_and_keeps_its_live_shape() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    let now = std::time::Instant::now();

    let mut held = queue_row("coder-t-1", true, None);
    held.paused = true;
    held.task_id = Some("t-1".to_string());
    held.context_tokens = 3500;
    held.started_at = Some(now - Duration::from_secs(5));
    let mut running = queue_row("coder-t-2", true, None);
    running.task_id = Some("t-2".to_string());
    let mut closed = queue_row("researcher-t-3", false, None);
    closed.task_id = Some("t-3".to_string());
    closed.paused = true;
    r.subagents.extend([held, running, closed]);

    let (rows, styles) = draw_subagent_panel(&mut r, 100, 20);
    let text = rows.join("\n");
    assert!(
        text.contains("coder-t-1 (Paused, 3.5k ctx, 5s)"),
        "expected 'coder-t-1 (Paused, 3.5k ctx, 5s)' in the subagents panel, got:\n{text}"
    );
    assert!(
        text.contains("coder-t-2 (Active)"),
        "the row that kept its slot still reads (Active), got:\n{text}"
    );

    let held_line = rows
        .iter()
        .find(|line| line.contains("coder-t-1"))
        .expect("the held row is drawn");
    assert!(
        !held_line.contains("(Active"),
        "a row parked inside the arbitration handshake must not claim it is streaming, got: {held_line:?}"
    );
    assert!(
        !held_line.contains("(Queued"),
        "a yielded model slot is not a wait behind the concurrency cap, got: {held_line:?}"
    );
    assert!(
        !held_line.contains("(Idle"),
        "a paused stream is still part of the round, got: {held_line:?}"
    );

    // A flag that outlived its stream must never produce the label word.
    let closed_line = rows
        .iter()
        .find(|line| line.contains("researcher-t-3"))
        .expect("the closed row is drawn");
    assert!(
        closed_line.contains("(Idle)"),
        "a finished row reads Idle whatever flag it still carries, got: {closed_line:?}"
    );
    assert!(
        !closed_line.contains("Paused"),
        "the pause label is gated on liveness, got: {closed_line:?}"
    );

    // The name stays green/bold — the worker is still live — and only the state
    // word changes colour, so the pause cannot be mistaken for a dead row.
    let held_y = rows
        .iter()
        .position(|line| line.contains("coder-t-1"))
        .expect("the held row is drawn");
    let name_x = rows[held_y].find("coder-t-1").unwrap();
    let label_x = rows[held_y].find("(Paused").unwrap();
    let name = styles[held_y][name_x];
    assert_eq!(
        name.fg,
        Some(Color::Green),
        "a held row keeps its live name colour"
    );
    assert!(
        name.add_modifier.contains(Modifier::BOLD),
        "a held row keeps its bold name"
    );
    assert_eq!(
        styles[held_y][label_x].fg,
        Some(Color::LightBlue),
        "the pause state word gets its own colour"
    );

    let running_y = rows
        .iter()
        .position(|line| line.contains("coder-t-2"))
        .expect("the running row is drawn");
    let running_x = rows[running_y].find("(Active").unwrap();
    assert_eq!(
        styles[running_y][running_x].fg,
        Some(Color::LightGreen),
        "the pause colour is never applied to a neighbour"
    );
}

/// The detail pane says which slot the row handed up instead of the generic
/// "waiting for model response" line — and it wins over both the queue position
/// and the previous turn's thinking bar, which are both stale once the stream is
/// parked. The spinner keeps turning because the worker is still live.
#[test]
fn the_detail_pane_spells_out_the_held_model_slot() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    let mut held = queue_row(
        "coder-t-1",
        true,
        Some(crate::ui::QueueState { position: 1, of: 2 }),
    );
    held.paused = true;
    r.subagents.push(held);
    r.subagent_is_thinking.insert("coder-t-1".to_string(), true);
    r.subagent_turn_thinking.insert(
        "coder-t-1".to_string(),
        "reasoning from before the yield".to_string(),
    );

    let (rows, _) = draw_subagent_panel(&mut r, 100, 20);
    let text = rows.join("\n");
    const HELD: &str = "[Status: Paused - model slot yielded to the steering arbitrator...]";
    assert!(
        text.contains(HELD),
        "expected {HELD:?} in the subagent status bar, got:\n{text}"
    );
    assert!(
        !text.contains("waiting for model response"),
        "a row whose slot is on loan to the arbitrator is not waiting on the model:\n{text}"
    );
    assert!(
        !text.contains("[Status: Queued"),
        "the handshake wins the bar over a stale queue position:\n{text}"
    );
    assert!(
        !text.contains("[Thinking:"),
        "thinking text from before the yield is stale, so the thinking bar must go:\n{text}"
    );

    let bar = rows
        .iter()
        .find(|line| line.contains(HELD))
        .expect("the status bar row");
    let spinner = bar[bar.find(HELD).unwrap() + HELD.len()..]
        .trim_start()
        .chars()
        .next()
        .unwrap_or(' ');
    assert!(
        SPINNER_FRAMES.contains(&spinner),
        "a held stream keeps a live spinner, got {spinner:?} in {bar:?}"
    );
}

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

/// The session loop holds the authoritative row set, so its view of the pause
/// wins in both directions — including the teardown it performs when the registry
/// closes the row, which is what stops a `Paused` from lingering.
#[test]
fn the_authoritative_rows_from_the_loop_own_the_pause_label() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    r.subagents.push(queue_row("coder-t-1", true, None));

    // A renderer-local pause ...
    r.on_event(&Event::SubagentStreamPause {
        agent_tag: "coder-t-1".to_string(),
        paused: true,
    });
    assert!(r.subagents[0].paused);
    // ... cannot outlive a loop push that says otherwise.
    let mut authoritative = r.subagents.clone();
    authoritative[0].paused = false;
    r.set_subagents(authoritative);
    assert!(
        !r.subagents[0].paused,
        "the loop folds the typed transition, so its resume edge wins"
    );

    let mut authoritative = r.subagents.clone();
    authoritative[0].paused = true;
    r.set_subagents(authoritative);
    assert!(
        r.subagents[0].paused,
        "and adopting a pause works the same way"
    );

    // The loop's teardown of a finished worker takes the flag down with it, and a
    // renderer that missed the closing edge still loses its stale `Paused`.
    let mut authoritative = r.subagents.clone();
    authoritative[0].is_active = false;
    authoritative[0].paused = false;
    r.set_subagents(authoritative);
    assert!(!r.subagents[0].is_active);
    let (rows, _) = draw_subagent_panel(&mut r, 100, 20);
    let text = rows.join("\n");
    assert!(
        text.contains("coder-t-1 (Idle)"),
        "the closed row reads Idle, got:\n{text}"
    );
    assert!(
        !text.contains("Paused"),
        "no stale pause survives the loop's teardown, got:\n{text}"
    );
}

/// A restart drops the arbitration handshake exactly like it drops the queue
/// ledger: a journaled pane records a round that was interrupted, never a stream
/// the arbitrator is holding right now.
#[test]
fn a_restored_row_never_shows_a_stale_pause() {
    let mut live = TuiRenderer::new();
    live.show_subagent_panel = true;
    let mut held = queue_row("coder-t-1", true, None);
    held.paused = true;
    held.task_id = Some("t-1".to_string());
    live.subagents.push(held);

    let section = live.capture_ui();
    let mut restored = TuiRenderer::new();
    restored.restore_panes(&section);

    assert_eq!(restored.subagents.len(), 1);
    assert!(
        !restored.subagents[0].paused,
        "the preemption handshake does not survive a restart, so the panel may not claim a pause"
    );
    let (rows, _) = draw_subagent_panel(&mut restored, 100, 20);
    let text = rows.join("\n");
    assert!(
        text.contains("coder-t-1 (Active)"),
        "a restored row comes back as a running row, got:\n{text}"
    );
    assert!(
        !text.contains("Paused"),
        "no stale pause may survive a restore, got:\n{text}"
    );
}

/// A held stream stays in the `[N active: …]` group — it really is part of the
/// running round — and is additionally named in its own segment, derived from the
/// same rows the panel label reads.
#[test]
fn the_main_status_line_names_held_streams_without_dropping_them_from_the_round() {
    let mut r = TuiRenderer::new();
    let mut held = queue_row("coder-t-1", true, None);
    held.paused = true;
    r.subagents
        .extend([held, queue_row("coder-t-2", true, None)]);
    r.status_line = "Delegating to specialist coder-t-1 for t-1".to_string();

    let content = draw_status_line(&mut r, 160);
    assert!(
        content.contains("[2 active: coder-t-1, coder-t-2]"),
        "a parked stream is still part of the round, got: {content:?}"
    );
    assert!(
        content.contains("[1 paused: coder-t-1]"),
        "the held stream is named in its own segment, got: {content:?}"
    );

    let mut plain = TuiRenderer::new();
    plain.subagents.push(queue_row("coder-t-2", true, None));
    plain.status_line = "Delegating to specialist coder-t-2 for t-2".to_string();
    let plain_line = draw_status_line(&mut plain, 160);
    assert!(
        plain_line.contains("[1 active: coder-t-2]"),
        "the control group is unchanged, got: {plain_line:?}"
    );
    assert!(
        !plain_line.contains("paused"),
        "a round with nothing parked shows no pause segment, got: {plain_line:?}"
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

/// The exact sentence the preemption handshake puts on the status channel has to
/// hold the spinner up: a stream parked inside the handshake produces no tokens,
/// and that deliberate silence must not read as a stalled turn.
#[test]
fn a_round_with_a_parked_stream_keeps_the_status_line_animating() {
    let mut r = TuiRenderer::new();
    r.on_event(&Event::Status(
        "[coder] Yielded model slot (gpt-x) to Steer Arbitrator — stream paused".to_string(),
    ));
    assert!(
        r.waiting_for_token_since.is_some(),
        "a stream held by the arbitrator counts as waiting on the backend"
    );

    let content = draw_status_line(&mut r, 160);
    assert!(
        content.contains("stream paused"),
        "the yielded slot is surfaced in the status line, got: {content:?}"
    );
    let tail = content.trim_end();
    assert!(
        tail.ends_with(SPINNER_FRAMES),
        "the activity spinner must keep turning while a stream is parked, got: {tail:?}"
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
fn test_chat_renders_white_content_and_gates_thought() {
    let mut r = TuiRenderer::new();
    assert!(!r.show_thought); // Default is false (OFF)

    r.messages
        .push("<think>\nsecret thoughts\n</think>\nVisible answer to user".to_string());
    r.current_thought = "streaming thoughts".to_string();
    r.current_content = "streaming visible answer".to_string();

    let backend = ratatui::backend::TestBackend::new(80, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    // 1. When show_thought is false: thoughts must NOT appear in chat
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 10);
            r.render_chat(frame, area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let mut lines = Vec::new();
    for y in 0..10 {
        let line: String = (0..80)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        lines.push(line);
    }
    let text_off = lines.join("\n");
    assert!(
        !text_off.contains("secret thoughts"),
        "secret thoughts should be hidden when show_thought=false"
    );
    assert!(
        !text_off.contains("streaming thoughts"),
        "streaming thoughts should be hidden when show_thought=false"
    );
    assert!(text_off.contains("Visible answer to user"));
    assert!(text_off.contains("streaming visible answer"));

    // 2. Toggle thought ON via /thought
    r.input_text = "/thought".to_string();
    r.submit();
    assert!(r.show_thought);

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 10);
            r.render_chat(frame, area);
        })
        .unwrap();

    let buffer2 = terminal.backend().buffer();
    let mut lines2 = Vec::new();
    for y in 0..10 {
        let line: String = (0..80)
            .map(|x| buffer2[(x, y)].symbol().to_string())
            .collect();
        lines2.push(line);
    }
    let text_on = lines2.join("\n");
    assert!(
        text_on.contains("secret thoughts"),
        "thoughts should be shown when show_thought=true"
    );
    assert!(
        text_on.contains("streaming thoughts"),
        "streaming thoughts should be shown when show_thought=true"
    );
    assert!(!text_off.contains("<think>"));
    assert!(!text_off.contains("</think>"));
    assert!(!text_on.contains("<think>"));
    assert!(!text_on.contains("</think>"));
}

#[test]
fn test_subagent_thinking_visibility_controlled_by_show_thought() {
    let backend = ratatui::backend::TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut r = TuiRenderer::new();
    r.subagents.push(SubagentDetail {
        name: "coder".to_string(),
        task_id: Some("t-001".to_string()),
        prompt: "task brief".to_string(),
        started_at: None,
        worked_duration: std::time::Duration::ZERO,
        last_activity_at: None,
        logs: vec!["log step".to_string()],
        thinking: "secret subagent thoughts".to_string(),
        content: "deliverable code".to_string(),
        is_active: true,
        context_tokens: 0,
        queue: None,
        paused: false,
    });

    // The pane's thinking flag is set explicitly here, not inferred from the
    // `thinking` buffer: inferring liveness from leftover pane text is the defect
    // removed, so the collapsed bar now needs the flag the stream sets
    // (`Event::SubagentThinking`) or a live registry entry. Everything this
    // test pins — the `show_thought` gate — is unchanged.
    r.subagent_is_thinking.insert("coder".to_string(), true);

    // 1. By default, show_thought is false -> [Thinking] must not appear
    assert!(!r.show_thought);
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 20);
            r.render_subagents(frame, area);
        })
        .unwrap();

    let buffer1 = terminal.backend().buffer();
    let mut lines1 = Vec::new();
    for y in 0..20 {
        let line: String = (0..80)
            .map(|x| buffer1[(x, y)].symbol().to_string())
            .collect();
        lines1.push(line);
    }
    let text_off = lines1.join("\n");
    assert!(
        !text_off.contains("[Thinking]"),
        "subagent full [Thinking] block should be hidden when show_thought=false"
    );
    assert!(
        !text_off.contains("secret subagent thoughts"),
        "subagent raw thoughts should be hidden when show_thought=false"
    );
    assert!(
        text_off.contains("[Thinking:"),
        "subagent collapsed [Thinking: ~...] indicator should be shown when show_thought=false"
    );
    assert!(
        text_off.contains("[Output]"),
        "subagent [Output] header should be visible when show_thought=false"
    );
    assert!(
        text_off.contains("deliverable code"),
        "subagent output content should be visible when show_thought=false"
    );
    assert!(text_off.contains("[Logs]"));
    assert!(text_off.contains("log step"));

    // 2. Toggle thought ON via /thought
    r.input_text = "/thought".to_string();
    r.submit();
    assert!(r.show_thought);

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 20);
            r.render_subagents(frame, area);
        })
        .unwrap();

    let buffer2 = terminal.backend().buffer();
    let mut lines2 = Vec::new();
    for y in 0..20 {
        let line: String = (0..80)
            .map(|x| buffer2[(x, y)].symbol().to_string())
            .collect();
        lines2.push(line);
    }
    let text_on = lines2.join("\n");
    assert!(
        text_on.contains("[Thinking]"),
        "subagent [Thinking] should be shown when show_thought=true"
    );
    assert!(
        text_on.contains("secret subagent thoughts"),
        "subagent thoughts should be shown when show_thought=true"
    );
    assert!(
        text_on.contains("[Output]"),
        "subagent [Output] should be shown when show_thought=true"
    );
    assert!(
        text_on.contains("deliverable code"),
        "subagent output content should be shown when show_thought=true"
    );
}

/// Regression guard: the collapsed `[Thinking: …]` bar and its spinner are a claim about
/// the **worker**, so the inferred form requires the registry entry behind the row to be
/// live. A cancel, a preemption or a guard drop emits no stream event at all, so the pane
/// would otherwise keep a stale spinner over a worker the Manager and the arbitrator no
/// longer consider running.
#[test]
fn a_finished_worker_with_leftover_thinking_stops_claiming_to_be_thinking() {
    use crate::orchestrator::workers;
    // Registration touches the process-global table other suites assert over.
    let _workers = workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let worker = workers::register_active_worker(
        Some("t-9503".to_string()),
        "coder".to_string(),
        "brief".to_string(),
    );
    let key = worker.0.clone();

    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    r.subagents.push(SubagentDetail {
        name: key.clone(),
        task_id: Some("t-9503".to_string()),
        thinking: "half a thought, still on screen".to_string(),
        is_active: true,
        ..Default::default()
    });

    let backend = ratatui::backend::TestBackend::new(100, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let draw = |terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
                r: &mut TuiRenderer| {
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 100, 20);
                r.render_subagents(frame, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..20)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let while_running = draw(&mut terminal, &mut r);
    assert!(
        while_running.contains("[Thinking:"),
        "a live worker with streamed thinking shows the thinking bar, got:\n{while_running}"
    );

    // The guard drops: the registry closes the entry. Nothing else in the pane changed —
    // the row still claims `is_active` and still holds its text — and that is the defect.
    drop(worker);
    let after_done = draw(&mut terminal, &mut r);
    assert!(
        !after_done.contains("[Thinking:"),
        "a completed registry entry must never be drawn as still thinking, got:\n{after_done}"
    );
    assert!(
        after_done.contains(&key),
        "the row itself stays visible: the completed ring still holds the entry"
    );

    workers::remove_worker_entry_for_test(&key);
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
fn test_chat_scrolling_renders_windowed_view() {
    let mut r = TuiRenderer::new();
    r.focused_panel = FocusedPanel::Chat;
    for i in 0..50 {
        r.messages.push(format!("Message line {i}"));
    }

    r.chat_width.set(80);
    r.chat_height.set(8); // inner area height of 80x10 bordered box

    let total_lines = r.estimated_chat_lines(80);
    assert_eq!(total_lines, 50);

    r.scroll_to_bottom();
    assert_eq!(r.chat_scroll, 42); // 50 - 8

    let backend = ratatui::backend::TestBackend::new(80, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 10);
            r.render_chat(frame, area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let mut bottom_text = String::new();
    for y in 0..10 {
        let line: String = (0..80)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        bottom_text.push_str(&line);
        bottom_text.push('\n');
    }
    assert!(bottom_text.contains("Message line 49"));
    assert!(!bottom_text.contains("Message line 0"));

    r.scroll_to_top();
    assert_eq!(r.chat_scroll, 0);

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 10);
            r.render_chat(frame, area);
        })
        .unwrap();

    let buffer2 = terminal.backend().buffer();
    let mut top_text = String::new();
    for y in 0..10 {
        let line: String = (0..80)
            .map(|x| buffer2[(x, y)].symbol().to_string())
            .collect();
        top_text.push_str(&line);
        top_text.push('\n');
    }
    assert!(top_text.contains("Message line 0"));
    assert!(!top_text.contains("Message line 49"));

    r.scroll_down(5);
    assert_eq!(r.chat_scroll, 5);
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
fn test_plan_auto_scrolls_to_first_pending_task() {
    let mut r = TuiRenderer::new();
    let plan_text = "# Plan\n## Phase 1\n- [x] Task 1\n- [x] Task 2\n- [x] Task 3\n- [x] Task 4\n- [x] Task 5\n- [x] Task 6\n- [x] Task 7\n- [x] Task 8\n## Phase 2\n- [ ] Task 9\n- [ ] Task 10\n";
    r.plan_content = plan_text.to_string();
    r.show_plan_panel = true;
    r.plan_auto_scroll = true;

    let backend = ratatui::backend::TestBackend::new(80, 8);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 8);
            r.render_plan(frame, area, plan_text, false);
        })
        .unwrap();

    assert_eq!(r.plan_scroll, 7);
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
fn test_plan_auto_scrolls_to_started_task() {
    let mut r = TuiRenderer::new();
    let plan_text = "# Plan\n## Phase 1\n- [x] [t-001] Task 1\n- [x] [t-002] Task 2\n- [x] [t-003] Task 3\n- [ ] [t-004] Task 4\n- [ ] [t-005] Task 5\n- [ ] [t-006] Task 6\n- [ ] [t-007] Task 7\n- [ ] [t-008] Task 8\n- [ ] [t-009] Task 9\n- [ ] [t-010] Task 10\n";
    r.plan_content = plan_text.to_string();

    r.on_event(&crate::ui::Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-008".to_string()),
        },
    ));

    assert_eq!(r.active_plan_task.as_deref(), Some("t-008"));
    assert!(r.plan_auto_scroll);
    assert!(r.show_plan_panel);

    let backend = ratatui::backend::TestBackend::new(80, 6);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 6);
            r.render_plan(frame, area, plan_text, false);
        })
        .unwrap();

    // The focused task's plan line sits at line offset 9, leaving 1 line of
    // context above it -> desired scroll is 8
    assert_eq!(r.plan_scroll, 8);
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
fn test_render_plan_highlights_all_concurrent_active_tasks_and_avoids_substring_collision() {
    let mut r = TuiRenderer::new();
    let plan_text = "\
# Plan
- [ ] [t-100] Task 100
- [ ] [t-100a1] Task 100a1
- [ ] [t-100b] Task 100b
- [ ] [t-200] Task 200
- [x] [t-050] Task 50 completed
";
    r.plan_content = plan_text.to_string();

    // Two agents active in parallel: the coder on the first task, the
    // researcher on the second
    r.subagents = vec![
        SubagentDetail {
            name: "coder-t-100".to_string(),
            task_id: Some("t-100".to_string()),
            prompt: "Task 100".to_string(),
            is_active: true,
            ..Default::default()
        },
        SubagentDetail {
            name: "researcher-t-200".to_string(),
            task_id: Some("t-200".to_string()),
            prompt: "Task 200".to_string(),
            is_active: true,
            ..Default::default()
        },
    ];
    // active_plan_task records the latest started task
    r.active_plan_task = Some("t-200".to_string());

    let backend = ratatui::backend::TestBackend::new(80, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 10);
            r.render_plan(frame, area, plan_text, false);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();

    // Helper to find a rendered line in the buffer and get the color of its first non-space cell
    let find_line_fg = |needle: &str| -> Option<Color> {
        for y in 0..10 {
            let row_str: String = (0..80)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            if row_str.contains(needle) {
                // Find first cell belonging to the task text
                for x in 1..79 {
                    if buffer[(x, y)].symbol() == "[" {
                        return buffer[(x, y)].style().fg;
                    }
                }
            }
        }
        None
    };

    // Both active tasks MUST be highlighted in Yellow
    assert_eq!(
        find_line_fg("[t-100]"),
        Some(Color::Yellow),
        "Active task t-100 must be highlighted in Yellow"
    );
    assert_eq!(
        find_line_fg("[t-200]"),
        Some(Color::Yellow),
        "Active task t-200 must be highlighted in Yellow"
    );

    // Inactive tasks sharing a substring with an active one MUST NOT be Yellow
    // (must be LightRed)
    assert_eq!(
        find_line_fg("[t-100a1]"),
        Some(Color::LightRed),
        "Inactive task t-100a1 must be LightRed, not Yellow"
    );
    assert_eq!(
        find_line_fg("[t-100b]"),
        Some(Color::LightRed),
        "Inactive task t-100b must be LightRed, not Yellow"
    );

    // The completed task must be Green
    assert_eq!(
        find_line_fg("[t-050]"),
        Some(Color::Green),
        "Completed task t-050 must be Green"
    );
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

#[test]
fn test_input_cursor_stepping_left_and_right() {
    let mut r = TuiRenderer::new();
    r.focused_panel = FocusedPanel::Chat;
    r.input_text = "hello world".to_string();
    r.cursor = "hello world".len();

    for _ in 0..5 {
        r.cursor_left();
    }
    assert_eq!(r.cursor, "hello ".len());

    r.insert_char('X');
    assert_eq!(r.input_text, "hello Xworld");
    assert_eq!(r.cursor, "hello X".len());

    r.cursor_right();
    r.cursor_right();
    assert_eq!(r.cursor, "hello Xwo".len());

    r.delete_backward();
    assert_eq!(r.input_text, "hello Xwrld");
}

#[test]
fn test_token_counting_and_formatting_in_status_bar() {
    assert_eq!(
        TuiRenderer::format_token_counts(0, 0),
        "0 in / 0 out (0 total)"
    );
    assert_eq!(
        TuiRenderer::format_token_counts(450, 120),
        "450 in / 120 out (570 total)"
    );
    assert_eq!(
        TuiRenderer::format_token_counts(1500, 320),
        "1.5k in / 320 out (1.8k total)"
    );
    assert_eq!(
        TuiRenderer::format_token_counts(25400, 1200),
        "25.4k in / 1.2k out (26.6k total)"
    );
    assert_eq!(
        TuiRenderer::format_token_counts(1_200_000, 350_000),
        "1.2M in / 350.0k out (1.6M total)"
    );

    let mut r = TuiRenderer::new();
    r.on_event(&Event::TokensIn(1000));
    r.on_event(&Event::Message("Hello world response".to_string()));
    assert_eq!(r.tokens_in, 1000);
    assert_eq!(r.orchestrator_context_tokens, 1000);
    assert!(r.tokens_out > 0);
}

#[test]
fn test_orchestrator_context_tokens_in_status_bar() {
    let mut r = TuiRenderer::new();
    r.on_event(&Event::TokensIn(14500));
    assert_eq!(r.orchestrator_context_tokens, 14500);

    let backend = ratatui::backend::TestBackend::new(100, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 9, 100, 1);
            r.render_status(frame, area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let content: String = (0..100)
        .map(|x| buffer[(x, 9)].symbol().to_string())
        .collect();
    assert!(
        content.contains("Ctx: 14.5k"),
        "expected 'Ctx: 14.5k' in status bar, got: {content}"
    );
}

#[test]
fn test_waiting_for_token_elapsed_time_in_status_bar() {
    let mut r = TuiRenderer::new();
    r.on_event(&Event::Status("Running (test-model)".to_string()));
    assert!(r.waiting_for_token_since.is_some());

    let backend = ratatui::backend::TestBackend::new(100, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 9, 100, 1);
            r.render_status(frame, area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let content: String = (0..100)
        .map(|x| buffer[(x, 9)].symbol().to_string())
        .collect();
    assert!(
        content.contains("Running (test-model)"),
        "expected 'Running (test-model)' in status bar, got: {content}"
    );
    assert!(
        content.contains("s)"),
        "expected elapsed seconds counter '(X.Xs)' in status bar, got: {content}"
    );

    // First token arrives -> timer clears
    r.on_event(&Event::Message("token 1".to_string()));
    assert!(r.waiting_for_token_since.is_none());
}

#[test]
fn test_subagent_context_tokens_rendering_in_list() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    r.subagents.push(SubagentDetail {
        name: "coder-t-1".to_string(),
        task_id: Some("t-1".to_string()),
        prompt: String::new(),
        started_at: None,
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: true,
        context_tokens: 3500,
        worked_duration: Duration::ZERO,
        queue: None,
        paused: false,
    });
    r.subagents.push(SubagentDetail {
        name: "researcher-t-2".to_string(),
        task_id: Some("t-2".to_string()),
        prompt: String::new(),
        started_at: None,
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: false,
        context_tokens: 1200,
        worked_duration: Duration::ZERO,
        queue: None,
        paused: false,
    });
    r.subagents.push(SubagentDetail {
        name: "validator".to_string(),
        task_id: None,
        prompt: String::new(),
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
    });

    let backend = ratatui::backend::TestBackend::new(100, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 100, 20);
            r.render_subagents(frame, area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let mut lines = Vec::new();
    for y in 0..20 {
        let line: String = (0..100)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        lines.push(line);
    }
    let full_text = lines.join("\n");
    assert!(
        full_text.contains("coder-t-1 (Active, 3.5k ctx)"),
        "expected 'coder-t-1 (Active, 3.5k ctx)' in subagents panel, got:\n{full_text}"
    );
    assert!(
        full_text.contains("researcher-t-2 (Idle, 1.2k ctx)"),
        "expected 'researcher-t-2 (Idle, 1.2k ctx)' in subagents panel, got:\n{full_text}"
    );
    assert!(
        full_text.contains("validator (Active)"),
        "expected 'validator (Active)' with no ctx string when 0, got:\n{full_text}"
    );
}

#[test]
fn test_subagent_time_counter_rendering_in_list() {
    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    let now = std::time::Instant::now();
    r.subagents.push(SubagentDetail {
        name: "coder-t-1".to_string(),
        task_id: Some("t-1".to_string()),
        prompt: String::new(),
        started_at: Some(now - Duration::from_secs(5)),
        last_activity_at: Some(now - Duration::from_secs(5)),
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: true,
        context_tokens: 3500,
        worked_duration: Duration::ZERO,
        queue: None,
        paused: false,
    });
    r.subagents.push(SubagentDetail {
        name: "researcher-t-2".to_string(),
        task_id: Some("t-2".to_string()),
        prompt: String::new(),
        started_at: None,
        last_activity_at: Some(now - Duration::from_secs(85)),
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: false,
        context_tokens: 1200,
        worked_duration: Duration::from_secs(85),
        queue: None,
        paused: false,
    });

    let backend = ratatui::backend::TestBackend::new(100, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 100, 20);
            r.render_subagents(frame, area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let mut lines = Vec::new();
    for y in 0..20 {
        let line: String = (0..100)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        lines.push(line);
    }
    let full_text = lines.join("\n");
    assert!(
        full_text.contains("coder-t-1 (Active, 3.5k ctx, 5s)"),
        "expected 'coder-t-1 (Active, 3.5k ctx, 5s)' in subagents panel, got:\n{full_text}"
    );
    assert!(
        full_text.contains("researcher-t-2 (Idle, 1.2k ctx, 1m 25s)"),
        "expected 'researcher-t-2 (Idle, 1.2k ctx, 1m 25s)' in subagents panel, got:\n{full_text}"
    );
}

/// Regression guard: every fact on a list line is resolved through the row's
/// **effective registry key**, and the two registry facts a viewer needs to tell rows
/// apart are spelled in the label.
///
/// The collided `{base}#N` row is built with `context_tokens: 0`, so its ctx can only
/// come from the registry entry behind that key — which is exactly the lookup the old
/// name-based read (`get_active_worker_tokens(&sa.name)`) missed.
#[test]
fn a_collided_row_renders_from_its_registry_entry_and_the_label_says_what_it_is() {
    use crate::orchestrator::workers;
    // Registration touches the process-global table other suites assert over.
    let _workers = workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Private task ids keep these keys out of every other test's way.
    let first = workers::register_active_worker(
        Some("t-9501".to_string()),
        "coder".to_string(),
        "brief one".to_string(),
    );
    let second = workers::register_active_worker(
        Some("t-9501".to_string()),
        "coder".to_string(),
        "brief two".to_string(),
    );
    let audit = workers::register_active_worker(
        Some("t-9502".to_string()),
        "validator-coder".to_string(),
        "audit brief".to_string(),
    );
    let (plain, collided, audit_key) = (first.0.clone(), second.0.clone(), audit.0.clone());
    workers::update_active_worker_context(&collided, 4321);
    assert_ne!(plain, collided, "the second worker took a collision key");

    let mut r = TuiRenderer::new();
    r.show_subagent_panel = true;
    for key in [&plain, &collided, &audit_key] {
        r.subagents.push(SubagentDetail {
            name: key.clone(),
            is_active: true,
            ..Default::default()
        });
    }

    let backend = ratatui::backend::TestBackend::new(120, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 120, 20);
            r.render_subagents(frame, area);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let full_text = (0..20)
        .map(|y| {
            (0..120)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        full_text.contains(&format!("{collided} (Active, 4.3k ctx)")),
        "the ctx of a `{collided}` row comes from the entry holding that exact key, got:\n{full_text}"
    );
    let handle = collided.split_once('#').expect("collision key").1;
    assert!(
        full_text.contains(&format!("Dup #{handle}")),
        "a duplicate-key row labels its collision handle, got:\n{full_text}"
    );
    assert!(
        full_text.contains(&format!("{audit_key} (Active) Audit")),
        "a validator-family row reads as an audit run, got:\n{full_text}"
    );
    assert!(
        full_text.contains(&format!("{plain} (Active)")),
        "a plain key carries no badge and no invented ctx, got:\n{full_text}"
    );

    drop(first);
    drop(second);
    drop(audit);
    for key in [&plain, &collided, &audit_key] {
        workers::remove_worker_entry_for_test(key);
    }
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
fn test_status_bar_displays_right_aligned_elapsed_time() {
    let mut r = TuiRenderer::new();
    let start = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(135))
        .unwrap();
    r.plan_start_time = Some(start);
    r.plan_completed_time = Some(start + std::time::Duration::from_secs(135));

    let backend = ratatui::backend::TestBackend::new(100, 5);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 4, 100, 1);
            r.render_status(frame, area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let content: String = (0..100)
        .map(|x| buffer[(x, 4)].symbol().to_string())
        .collect();

    assert!(
        content.contains("Tokens:"),
        "expected 'Tokens:' on the left, got: {content}"
    );
    assert!(
        content.contains("Elapsed: 2m 15s"),
        "expected 'Elapsed: 2m 15s' in status bar, got: {content}"
    );
    assert!(
        content.ends_with("Elapsed: 2m 15s "),
        "expected elapsed time to be right-aligned at the edge of the status bar, got: '{content}'"
    );
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
fn test_chat_auto_scroll_shows_final_lines_of_multiline_markdown_response() {
    let mut r = TuiRenderer::new();
    let text = "## Summary\n\nParagraph 1.\n\nParagraph 2.\n\n### Section 1\nLine 1\nLine 2\n\n### Section 2\n- Item 1\n- Item 2\n\n### Section 3\n- Item 3\n- Item 4\n\n### Final Section\nBottom line 1\nBottom line 2\n";
    r.on_event(&Event::Message(text.to_string()));

    let backend = ratatui::backend::TestBackend::new(80, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 10);
            r.render_chat(frame, area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let mut rendered = String::new();
    for y in 0..10 {
        let line: String = (0..80)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        rendered.push_str(&line);
        rendered.push('\n');
    }

    assert!(
        rendered.contains("Bottom line 2"),
        "expected bottom line to be visible in chat viewport, got:\n{rendered}"
    );
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

#[test]
fn test_tui_renders_subagent_thinking_progress_and_output_during_streaming() {
    let backend = ratatui::backend::TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut r = TuiRenderer::new();
    r.set_subagents(vec![SubagentDetail {
        name: "researcher-t-001".to_string(),
        task_id: Some("t-001".to_string()),
        prompt: "Research NES CPU".to_string(),
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
    }]);

    // 1. When subagent is active but has not produced tokens yet:
    // It must render an active waiting indicator (never blank).
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 100, 30);
            r.render_subagents(frame, area);
        })
        .unwrap();

    let mut screen1 = String::new();
    let buf1 = terminal.backend().buffer();
    for y in 0..30 {
        let line: String = (0..100)
            .map(|x| buf1[(x, y)].symbol().to_string())
            .collect();
        screen1.push_str(&line);
        screen1.push('\n');
    }
    assert!(
        screen1.contains("researcher-t-001"),
        "subagent header should be visible"
    );
    assert!(
        screen1.contains("[Status: Active - waiting for model response...]"),
        "active waiting indicator must be shown when subagent is active with 0 tokens, got:\n{screen1}"
    );

    // 2. Stream tokens of reasoning/thinking:
    r.on_event(&Event::SubagentThinking {
        agent_tag: "researcher-t-001".to_string(),
        text: "Analyzing 6502 addressing modes and cycle timings in detail.".to_string(),
    });

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 100, 30);
            r.render_subagents(frame, area);
        })
        .unwrap();

    let mut screen2 = String::new();
    let buf2 = terminal.backend().buffer();
    for y in 0..30 {
        let line: String = (0..100)
            .map(|x| buf2[(x, y)].symbol().to_string())
            .collect();
        screen2.push_str(&line);
        screen2.push('\n');
    }
    // With show_thought = false, the collapsed thinking indicator is rendered:
    assert!(
        screen2.contains("[Thinking: ~"),
        "collapsed thinking indicator must be visible when thinking is non-empty, got:\n{screen2}"
    );
    assert!(
        !screen2.contains("Analyzing 6502 addressing modes"),
        "raw thinking text should be collapsed when show_thought=false"
    );

    // 3. Stream assistant output content:
    r.on_event(&Event::SubagentMessage {
        agent_tag: "researcher-t-001".to_string(),
        text: "# NES Architecture Research Deliverable\n".to_string(),
    });

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 100, 30);
            r.render_subagents(frame, area);
        })
        .unwrap();

    let mut screen3 = String::new();
    let buf3 = terminal.backend().buffer();
    for y in 0..30 {
        let line: String = (0..100)
            .map(|x| buf3[(x, y)].symbol().to_string())
            .collect();
        screen3.push_str(&line);
        screen3.push('\n');
    }
    // Output MUST be unconditionally visible regardless of show_thought:
    assert!(
        screen3.contains("[Output]"),
        "[Output] header must be visible even when show_thought=false, got:\n{screen3}"
    );
    assert!(
        screen3.contains("# NES Architecture Research Deliverable"),
        "subagent content must be visible even when show_thought=false, got:\n{screen3}"
    );

    // 4. Toggle thought display ON via /thought:
    r.input_text = "/thought".to_string();
    r.submit();
    assert!(r.show_thought);

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 100, 30);
            r.render_subagents(frame, area);
        })
        .unwrap();

    let mut screen4 = String::new();
    let buf4 = terminal.backend().buffer();
    for y in 0..30 {
        let line: String = (0..100)
            .map(|x| buf4[(x, y)].symbol().to_string())
            .collect();
        screen4.push_str(&line);
        screen4.push('\n');
    }
    assert!(
        screen4.contains("[Thinking]"),
        "full [Thinking] block must be visible when show_thought=true, got:\n{screen4}"
    );
    assert!(
        screen4.contains("Analyzing 6502 addressing modes and cycle timings in detail."),
        "raw thinking text must be visible when show_thought=true, got:\n{screen4}"
    );
    assert!(
        screen4.contains("# NES Architecture Research Deliverable"),
        "output content remains visible when show_thought=true, got:\n{screen4}"
    );
}

#[test]
fn test_tui_help_command_includes_ctrl_t() {
    let mut r = TuiRenderer::new();
    r.input_text = "/help".to_string();
    r.submit();

    let help_msg = r
        .messages
        .last()
        .expect("should have emitted /help message");
    assert!(help_msg.contains("Ctrl+T - toggle the thinking block display"));
}

#[test]
fn test_subagent_thinking_shows_remaining_budget_countdown() {
    let mut r = TuiRenderer::new();
    r.default_thinking_budget = 10_000;
    r.subagents.push(SubagentDetail {
        name: "coder-t-001".to_string(),
        task_id: Some("t-001".to_string()),
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
    });

    // 1. Stream 4000 characters in Turn 1: ~1000 tokens. 10000 - 1000 = 9000 remaining (~9.0k remaining).
    r.on_event(&Event::SubagentThinking {
        agent_tag: "coder-t-001".to_string(),
        text: "a".repeat(4000),
    });

    let backend = ratatui::backend::TestBackend::new(100, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 100, 20);
            r.render_subagents(frame, area);
        })
        .unwrap();

    let mut screen = String::new();
    let buf = terminal.backend().buffer();
    for y in 0..20 {
        let line: String = (0..100).map(|x| buf[(x, y)].symbol().to_string()).collect();
        screen.push_str(&line);
        screen.push('\n');
    }

    assert!(
        screen.contains("9.0k remaining"),
        "expected countdown '9.0k remaining' in subagent status bar, got:\n{screen}"
    );
    assert!(
        screen.contains("[Thinking: ~1.0k tokens (9.0k remaining) / 4.0k chars]"),
        "expected full thinking countdown banner, got:\n{screen}"
    );

    // 2. Stream message content in Turn 1 — thinking countdown must be cleared from status bar
    r.on_event(&Event::SubagentMessage {
        agent_tag: "coder-t-001".to_string(),
        text: "Deliverable work".to_string(),
    });
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 100, 20);
            r.render_subagents(frame, area);
        })
        .unwrap();
    let mut screen2 = String::new();
    let buf2 = terminal.backend().buffer();
    for y in 0..20 {
        let line: String = (0..100)
            .map(|x| buf2[(x, y)].symbol().to_string())
            .collect();
        screen2.push_str(&line);
        screen2.push('\n');
    }
    assert!(
        screen2.contains("[Status: Active - streaming output...]"),
        "expected status to switch to streaming output when content arrives, got:\n{screen2}"
    );
    assert!(
        !screen2.contains("[Thinking: ~"),
        "thinking banner should not be displayed in status bar when output is streaming"
    );

    // 3. Turn 2 starts: streams 2000 chars of thinking.
    // Must count PER-TURN (2000 chars -> 500 tokens, 9.5k remaining), NOT accumulated (6000 chars).
    r.on_event(&Event::SubagentThinking {
        agent_tag: "coder-t-001".to_string(),
        text: "b".repeat(2000),
    });
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 100, 20);
            r.render_subagents(frame, area);
        })
        .unwrap();
    let mut screen3 = String::new();
    let buf3 = terminal.backend().buffer();
    for y in 0..20 {
        let line: String = (0..100)
            .map(|x| buf3[(x, y)].symbol().to_string())
            .collect();
        screen3.push_str(&line);
        screen3.push('\n');
    }
    assert!(
        screen3.contains("9.5k remaining"),
        "expected Turn 2 countdown '9.5k remaining' (per-turn reset), got:\n{screen3}"
    );
    assert!(
        screen3.contains("[Thinking: ~500 tokens (9.5k remaining) / 2.0k chars]"),
        "expected Turn 2 per-turn thinking countdown banner, got:\n{screen3}"
    );
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
