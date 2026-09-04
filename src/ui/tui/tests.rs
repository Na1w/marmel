use super::*;
use ratatui::Terminal;
use ratatui::layout::Rect;
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
fn centered_rect_is_centered() {
    let r = Rect::new(0, 0, 100, 100);
    let c = centered_rect(70, 60, r);
    // 70% width centered: x = 15, width = 70.
    assert_eq!(c.x, 15);
    assert_eq!(c.width, 70);
    // 60% height centered: y = 20, height = 60.
    assert_eq!(c.y, 20);
    assert_eq!(c.height, 60);
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
fn count_single_message_lines_excludes_think_when_hidden() {
    let msg = "User: hi\n<think>\nsecret\n</think>\nvisible";
    // show_thought = false → think block excluded: "User: hi" + "visible".
    assert_eq!(count_single_message_lines(msg, 80, false), 2);
    // show_thought = true → think block included without raw tag lines: "User: hi" + "secret" + "visible".
    assert_eq!(count_single_message_lines(msg, 80, true), 3);
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
        last_activity_at: None,
        logs: vec!["log1".to_string()],
        thinking: "think".to_string(),
        content: "out".to_string(),
        is_active: true,
        context_tokens: 0,
    });
    // Default (show_thought = false): 1 (header) + 1 ([Logs]) + 1 (- log1) = 3
    assert_eq!(r.estimated_subagent_lines(80), 3);
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

    // Appending via append_steer_sentence.
    r.append_steer_sentence("New steer sentence.");
    let lines_after_steer = r.estimated_chat_lines(80);
    assert_eq!(lines_after_steer, 5);

    // Appending to the existing steer sentence.
    r.append_steer_sentence(" Another sentence.");
    let lines_after_steer2 = r.estimated_chat_lines(80);
    assert_eq!(lines_after_steer2, 5);
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
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: true,
        context_tokens: 0,
    });

    r.focused_panel = FocusedPanel::Chat;
    r.cycle_focus();
    assert_eq!(r.focused_panel, FocusedPanel::Plan);
    r.cycle_focus();
    assert_eq!(r.focused_panel, FocusedPanel::Subagents);
    r.cycle_focus();
    assert_eq!(r.focused_panel, FocusedPanel::Chat);
}

/// t-c304: a `Delegation(Started)` event creates an active subagent entry
/// and routes subsequent `Thinking`/`Message`/`Status` events into its
/// live thinking/content/logs fields.
#[test]
fn delegation_started_tracks_live_thinking_content_logs() {
    let mut r = TuiRenderer::new();

    r.on_event(&Event::Delegation(
        crate::orchestrator::DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-9".to_string()),
        },
    ));

    // The subagent entry exists and is active.
    assert_eq!(r.subagents.len(), 1);
    assert_eq!(r.subagents[0].name, "coder-t-9");
    assert!(r.subagents[0].is_active);
    assert_eq!(r.active_agent, "coder-t-9");

    // Live thinking streams into the subagent's thinking field.
    r.on_event(&Event::Thinking("reasoning...".to_string()));
    assert_eq!(r.subagents[0].thinking, "reasoning...");

    // Live content streams into the subagent's content field.
    r.on_event(&Event::Message("final answer".to_string()));
    assert_eq!(r.subagents[0].content, "final answer");

    // Status updates append to the subagent's logs.
    r.on_event(&Event::Status("calling backend".to_string()));
    assert!(r.subagents[0].logs.iter().any(|l| l == "calling backend"));
}

/// t-c304: a `Delegation(Completed)` event marks the subagent idle and
/// stops routing live content into it.
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

    // After completion, live content no longer routes into the subagent.
    r.on_event(&Event::Message("manager content".to_string()));
    assert_eq!(r.subagents[0].content, "");
}

/// t-c304: `set_subagents` adopts the session-loop's authoritative list,
/// preserving locally-streamed thinking/content and dropping stale entries.
#[test]
fn set_subagents_adopts_authoritative_list() {
    let mut r = TuiRenderer::new();

    // Local entry with streamed thinking.
    r.subagents.push(SubagentDetail {
        name: "coder".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        last_activity_at: None,
        logs: vec!["started task t-1".to_string()],
        thinking: "local think".to_string(),
        content: "local content".to_string(),
        is_active: true,
        context_tokens: 0,
    });

    // Authoritative list from the loop (no thinking/content, just lifecycle).
    let authoritative = vec![SubagentDetail {
        name: "coder".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        last_activity_at: None,
        logs: vec!["started task t-1".to_string()],
        thinking: String::new(),
        content: String::new(),
        is_active: true,
        context_tokens: 0,
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
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: false,
        context_tokens: 0,
    });
    r.set_subagents(vec![SubagentDetail {
        name: "coder".to_string(),
        task_id: None,
        prompt: String::new(),
        started_at: None,
        last_activity_at: None,
        logs: vec![],
        thinking: String::new(),
        content: String::new(),
        is_active: false,
        context_tokens: 0,
    }]);
    assert_eq!(r.subagents.len(), 1);
    assert_eq!(r.subagents[0].name, "coder");
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

    let (kvaser_style, special_kvaser) = message_style("Kvaser: Direct answer");
    assert!(special_kvaser);
    assert_eq!(kvaser_style.fg, Some(Color::LightYellow));

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
        last_activity_at: None,
        logs: vec!["log step".to_string()],
        thinking: "secret subagent thoughts".to_string(),
        content: "deliverable code".to_string(),
        is_active: true,
        context_tokens: 0,
    });

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
        "subagent [Thinking] should be hidden when show_thought=false"
    );
    assert!(
        !text_off.contains("secret subagent thoughts"),
        "subagent thoughts should be hidden when show_thought=false"
    );
    assert!(
        !text_off.contains("[Output]"),
        "subagent [Output] should be hidden when show_thought=false"
    );
    assert!(
        !text_off.contains("deliverable code"),
        "subagent output content should be hidden when show_thought=false"
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

    // Scroll to bottom
    r.scroll_to_bottom();
    assert_eq!(r.chat_scroll, 42); // 50 - 8

    let backend = ratatui::backend::TestBackend::new(80, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    // 1. Render bottom view
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

    // 2. Scroll up to top
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

    // 3. Scroll down by 5 lines
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

    // Scroll down
    r.scroll_down(2);
    assert_eq!(r.plan_scroll, 2);

    // Scroll up
    r.scroll_up(1);
    assert_eq!(r.plan_scroll, 1);

    // Scroll to top
    r.scroll_to_top();
    assert_eq!(r.plan_scroll, 0);

    // Scroll to bottom
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
    });

    r.show_thought = true;
    r.subagent_width.set(40);
    r.subagent_height.set(5);

    // Scroll down in subagents pane
    r.scroll_down(3);
    assert_eq!(r.subagent_scroll, 3);

    // Scroll up
    r.scroll_up(2);
    assert_eq!(r.subagent_scroll, 1);

    // Scroll to bottom
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
    });

    r.subagent_width.set(40);
    r.subagent_height.set(4);

    // Manually scroll up
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

    // Step left 5 times
    for _ in 0..5 {
        r.cursor_left();
    }
    assert_eq!(r.cursor, "hello ".len());

    // Insert character at cursor
    r.insert_char('X');
    assert_eq!(r.input_text, "hello Xworld");
    assert_eq!(r.cursor, "hello X".len());

    // Step right 2 times
    r.cursor_right();
    r.cursor_right();
    assert_eq!(r.cursor, "hello Xwo".len());

    // Delete backward
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
        full_text.contains("coder-t-1 (Active, 3.5k ctx, 5s ago)"),
        "expected 'coder-t-1 (Active, 3.5k ctx, 5s ago)' in subagents panel, got:\n{full_text}"
    );
    assert!(
        full_text.contains("researcher-t-2 (Idle, 1.2k ctx, 1m 25s ago)"),
        "expected 'researcher-t-2 (Idle, 1.2k ctx, 1m 25s ago)' in subagents panel, got:\n{full_text}"
    );
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

#[test]
fn test_rehydrate_subagents_enables_panel_and_selects_latest() {
    let mut r = TuiRenderer::new();
    assert!(!r.show_subagent_panel);
    assert_eq!(r.subagents.len(), 0);

    let subagents = vec![
        SubagentDetail {
            name: "coder-t-001".to_string(),
            task_id: Some("t-001".to_string()),
            prompt: "task 1".to_string(),
            started_at: None,
            last_activity_at: None,
            logs: vec![
                "started task t-001".to_string(),
                "completed task t-001".to_string(),
            ],
            thinking: String::new(),
            content: "output 1".to_string(),
            is_active: false,
            context_tokens: 100,
        },
        SubagentDetail {
            name: "researcher-t-002".to_string(),
            task_id: Some("t-002".to_string()),
            prompt: "task 2".to_string(),
            started_at: None,
            last_activity_at: None,
            logs: vec![
                "started task t-002".to_string(),
                "completed task t-002".to_string(),
            ],
            thinking: String::new(),
            content: "output 2".to_string(),
            is_active: false,
            context_tokens: 200,
        },
    ];

    r.rehydrate_subagents(&subagents);

    assert!(
        r.show_subagent_panel,
        "Subagents panel must be visible after rehydration"
    );
    assert_eq!(r.subagents.len(), 2);
    assert_eq!(r.selected_subagent_idx, 1, "Should select latest subagent");
    assert_eq!(r.subagents[0].name, "coder-t-001");
    assert_eq!(r.subagents[1].name, "researcher-t-002");
}

#[test]
fn test_rehydrate_messages_summarizes_delegation_results() {
    let mut r = TuiRenderer::new();
    let messages = vec![
        crate::types::Message::System {
            content: "system".to_string(),
        },
        crate::types::Message::User {
            content: "goal".to_string(),
        },
        crate::types::Message::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: vec![crate::types::ToolCall::new(
                "call-del-1",
                "delegate_task",
                r#"{"agent_name": "coder", "task_id": "t-001", "prompt": "build feature"}"#,
            )],
        },
        crate::types::Message::Tool {
            tool_call_id: "call-del-1".to_string(),
            content: "MISSION COMPLETE (t-001):\nline 1\nline 2\nline 3\n... 2000 lines ..."
                .to_string(),
        },
    ];

    r.rehydrate_messages(&messages);

    assert!(
        r.messages
            .iter()
            .any(|m| m.contains("[Tool Call] delegate_task(agent: coder, task_id: t-001)"))
    );
    // Should NOT dump the multi-line deliverable into chat
    assert!(
        !r.messages
            .iter()
            .any(|m| m.contains("line 1") || m.contains("2000 lines"))
    );
    // Should contain the summarized mission marker
    assert!(
        r.messages
            .iter()
            .any(|m| m == "[Tool Result] MISSION COMPLETE (t-001):")
    );
}
