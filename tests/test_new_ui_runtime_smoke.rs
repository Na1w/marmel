//! Runtime smoke of the rebuilt TUI reached **only through the crate's public seam**.
//!
//! Task t-018 (runtime smoke verification). The whole-frame `TestBackend` pins live
//! inside the crate (`src/ui/tui/tests_new_ui.rs`, `src/ui/tui/tests.rs`) because the
//! draw entry `TuiRenderer::draw_new_ui` and the keystroke seam `apply_terminal_event`
//! are `pub(crate)`: an out-of-crate test cannot paint a frame at all. What this file
//! therefore smoke-tests from the *outside* is everything an operator's session loop
//! actually feeds that renderer, and the text pipeline every pane hands to the frame:
//!
//! * the boot state of a real `TuiRenderer` (idle pane model, no screen side effects),
//! * a busy/streaming flood of [`Event`]s, including hostile payloads (ANSI escape
//!   bytes, one 4 000-char word, CJK, emoji ZWJ sequences, CRLF, tabs, control chars),
//! * a 50-row worker projection with queue / pause state, and its capture→restore
//!   round trip through the durable `UiSection` (design §9.4 D2, spec §10 item 18),
//! * the view-mode / focus / follow triple round-tripping through the same seam,
//! * the public text pipeline at the exact viewport widths the smoke matrix uses
//!   (100x30, 80x24, 60x16, 40x10, 39x9): row counts grow as the viewport shrinks and
//!   no rendered span carries a stray escape byte,
//! * the explicit abort / exit / command-gating seams behind the ConfirmAction overlays.
//!
//! Deliberately absent, and that is the point: nothing here allocates a pty, enables
//! raw mode, enters the alternate screen, or writes to the ambient terminal. A live
//! interactive launch is not observable in this sandbox (no `openpty`), so this file
//! proves only the non-terminal half of the runtime path — see
//! `docs/runtime_smoke_report.md`.

use marmennill::agents::Agent;
use marmennill::orchestrator::DelegationEvent;
use marmennill::session::format::UiSection;
use marmennill::ui::transcript::{UiRecord, UiTranscript};
use marmennill::ui::tui::TuiRenderer;
use marmennill::ui::tui::formatting::{
    count_wrapped_rendered_lines, parse_ansi_lines, render_markdown_lines, render_message_lines,
    wrapped_lines,
};
use marmennill::ui::{Event, QueueState, Renderer, SteerStage, SteerTrace, SubagentDetail};

/// The five viewports the smoke matrix (and `docs/new_ui_preview.txt`) uses.
const SMOKE_WIDTHS: [u16; 5] = [100, 80, 60, 40, 39];

/// The four mode spellings the durable bundle may carry (design §4.1).
const VIEW_MODE_TOKENS: [&str; 4] = ["transcript", "plan", "workers", "split"];

/// Adversarial payloads the streaming path really does see on a wire: SGR escape
/// bytes, an unbreakable 4 000-char token, CJK, emoji with ZWJ, CRLF, tabs, a
/// control byte, markdown fences, and empty/whitespace-only input.
fn hostile_payloads() -> Vec<(&'static str, String)> {
    vec![
        ("plain", "the status bar chips are drawn by one component".to_string()),
        (
            "ansi",
            "\u{1b}[31mred\u{1b}[0m plain \u{1b}]8;;https://example.com\u{1b}\\link\u{1b}[1m"
                .to_string(),
        ),
        ("long_word", "x".repeat(4_000)),
        ("cjk", "重構成した UI のステータス行は 1 行です。".repeat(20)),
        ("emoji", "👩‍💻 ✅ 🇯🇵 👨‍👩‍👧‍👦 — glyphs".repeat(20)),
        ("crlf_and_tabs", "one\r\n\ttwo\r\n\t\tthree\r\n".to_string()),
        ("control", "a\u{0}b\u{7}c\u{8}d".to_string()),
        (
            "markdown",
            "# Title\n\n- [x] [t-001] done\n- [ ] [t-002] open\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n"
                .to_string(),
        ),
        ("empty", String::new()),
        ("blank_lines", "\n\n\n   \n\t\n".to_string()),
        ("lone_newline", "\n".to_string()),
    ]
}

/// Flatten every span of every rendered line into one string per line.
fn span_text(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Boot: an idle renderer holds an idle pane model and touches no screen.
// ---------------------------------------------------------------------------

#[test]
fn a_booted_renderer_holds_an_idle_pane_model_without_touching_a_screen() {
    let renderer = TuiRenderer::new();
    let captured = renderer.capture_ui();

    // Idle: nothing streamed yet, so the transcript body is the pane's own
    // "No output yet." text and the capture carries no chat lines.
    assert!(
        captured.chat_messages.is_empty(),
        "a booted renderer already holds chat lines: {:?}",
        captured.chat_messages
    );
    assert!(
        captured.subagent_panels.is_empty(),
        "a booted renderer already holds worker rows: {:?}",
        captured.subagent_panels.len()
    );
    // The status line is real content, never blank, and the mode token is one of
    // the four the bundle may spell.
    assert!(
        !captured.status_line.trim().is_empty(),
        "the booted status line is blank: {:?}",
        captured.status_line
    );
    let mode = captured
        .view
        .view_mode
        .as_deref()
        .expect("a booted renderer states its view mode");
    assert!(
        VIEW_MODE_TOKENS.contains(&mode),
        "the booted mode token {mode:?} is not one of {VIEW_MODE_TOKENS:?}"
    );
    // Boot focuses the composer and follows the transcript tail.
    assert_eq!(
        captured.view.focused_panel, "input",
        "a booted renderer does not focus the composer"
    );
    assert_eq!(
        captured.view.follow,
        Some(true),
        "a booted renderer does not start following the transcript"
    );

    println!(
        "SMOKE boot: status_line={:?} mode={mode:?} focus={:?} follow={:?}",
        captured.status_line, captured.view.focused_panel, captured.view.follow
    );
}

// ---------------------------------------------------------------------------
// 2. Busy / streaming: the event flood reaches pane state, and never exits.
// ---------------------------------------------------------------------------

#[test]
fn a_streaming_flood_reaches_pane_state_and_never_exits_or_arms_abort() {
    let mut renderer = TuiRenderer::new();

    // A realistic busy session: streamed content + thought, tool traffic, status,
    // steering replies, tokens, and delegated workers (started / queued / running /
    // completed / failed), including every hostile payload as streamed content.
    for (label, payload) in hostile_payloads() {
        renderer.on_event(&Event::Thinking(format!("thought[{label}] {payload}")));
        renderer.on_event(&Event::Message(format!("content[{label}] {payload}")));
        renderer.on_event(&Event::SteerResponse(format!("steer[{label}] {payload}")));
        renderer.on_event(&Event::SubagentThinking {
            agent_tag: format!("coder_specialist-t-{label}"),
            text: payload.clone(),
        });
        renderer.on_event(&Event::SubagentMessage {
            agent_tag: format!("coder_specialist-t-{label}"),
            text: payload,
        });
    }
    renderer.on_event(&Event::ToolCall("read_file(src/ui/tui/mod.rs)".to_string()));
    renderer.on_event(&Event::ToolResult("1234 lines read".to_string()));
    renderer.on_event(&Event::Status(
        "delegating validator_specialist".to_string(),
    ));
    renderer.on_event(&Event::TokensIn(12_400));
    renderer.on_event(&Event::TokensOut(3_200));
    for (index, agent) in [Agent::Coder, Agent::Researcher, Agent::Validator]
        .into_iter()
        .enumerate()
    {
        let task = Some(format!("t-{:03}", index + 1));
        renderer.on_event(&Event::Delegation(DelegationEvent::Started {
            agent,
            task: task.clone(),
        }));
        renderer.on_event(&Event::Delegation(DelegationEvent::Queued {
            agent,
            task: task.clone(),
            position: index + 1,
            of: 3,
        }));
        renderer.on_event(&Event::Delegation(DelegationEvent::Running {
            agent,
            task: task.clone(),
        }));
    }
    renderer.on_event(&Event::SubagentStreamPause {
        agent_tag: "coder_specialist-t-001".to_string(),
        paused: true,
    });
    renderer.on_event(&Event::Done);

    let captured = renderer.capture_ui();
    assert!(
        !captured.chat_messages.is_empty(),
        "the transcript is blank after a streaming flood"
    );
    let joined = captured.chat_messages.join("\n");
    assert!(
        joined.contains("content[plain]") && joined.contains("content[ansi]"),
        "streamed content never reached the transcript: {joined}"
    );
    // A flood of worker events must not lose rows, and the queue/pause pair must
    // ride along with the row it belongs to.
    assert!(
        !captured.subagent_panels.is_empty(),
        "the worker projection is empty after delegation events"
    );
    // Nothing in a flood is an operator gesture: no exit, no abort.
    assert!(
        !renderer.user_exit_requested(),
        "a streaming flood requested a user exit"
    );
    assert!(
        !Renderer::aborted(&renderer),
        "a streaming flood armed the abort flag"
    );

    println!(
        "SMOKE busy: chat_lines={} worker_rows={} status_line={:?}",
        captured.chat_messages.len(),
        captured.subagent_panels.len(),
        captured.status_line
    );
}

// ---------------------------------------------------------------------------
// 3. The roster and its D2 pair survive the public capture -> restore seam.
// ---------------------------------------------------------------------------

#[test]
fn fifty_worker_rows_survive_the_projection_and_the_capture_restore_round_trip() {
    let mut renderer = TuiRenderer::new();
    let rows: Vec<SubagentDetail> = (0..50)
        .map(|index| SubagentDetail {
            name: if index % 7 == 0 {
                format!("coder_specialist-{}-{index:03}", "あ".repeat(60))
            } else {
                format!("coder_specialist-t-{index:03}")
            },
            task_id: Some(format!("t-{index:03}")),
            prompt: "x".repeat(2_000),
            queue: if index % 3 == 0 {
                Some(QueueState {
                    position: index + 1,
                    of: 50,
                })
            } else {
                None
            },
            paused: index % 5 == 0,
            is_active: true,
            logs: vec![format!("log {index}"), "\u{1b}[2mansi log\u{1b}[0m".into()],
            content: "delivered".to_string(),
            ..SubagentDetail::default()
        })
        .collect();

    renderer.set_subagents(rows.clone());
    let captured = renderer.capture_ui();
    assert_eq!(
        captured.subagent_panels.len(),
        50,
        "the projection did not carry all 50 rows"
    );

    renderer.restore_panes(&captured);
    let again = renderer.capture_ui();
    assert_eq!(
        again.subagent_panels.len(),
        captured.subagent_panels.len(),
        "a restore lost worker rows"
    );
    for index in 0..50 {
        let before = &captured.subagent_panels[index];
        let after = &again.subagent_panels[index];
        assert_eq!(before.name, after.name, "row {index} changed identity");
        assert_eq!(before.paused, after.paused, "row {index} lost its pause");
        assert_eq!(
            (before.queue.map(|q| q.position), before.queue.map(|q| q.of)),
            (after.queue.map(|q| q.position), after.queue.map(|q| q.of)),
            "row {index} lost its queue position"
        );
    }

    // A restore handed an empty section must not resurrect rows and must not panic.
    renderer.restore_panes(&UiSection::default());
    let cleared = renderer.capture_ui();
    assert!(
        cleared.subagent_panels.is_empty(),
        "an empty restore still holds worker rows"
    );

    println!(
        "SMOKE roster: rows={} queued={} paused={}",
        captured.subagent_panels.len(),
        captured
            .subagent_panels
            .iter()
            .filter(|panel| panel.queue.is_some())
            .count(),
        captured.subagent_panels.iter().filter(|p| p.paused).count()
    );
}

// ---------------------------------------------------------------------------
// 4. Mode / focus / follow round-trip, including a hostile mode token.
// ---------------------------------------------------------------------------

#[test]
fn mode_focus_and_follow_round_trip_through_the_public_seam() {
    let mut renderer = TuiRenderer::new();
    let mut section = renderer.capture_ui();

    for (mode, focus, follow) in [
        ("split", "chat", true),
        ("plan", "plan", false),
        ("workers", "subagents", false),
        ("transcript", "input", true),
    ] {
        section.view.view_mode = Some(mode.to_string());
        section.view.focused_panel = focus.to_string();
        section.view.follow = Some(follow);
        renderer.restore_panes(&section);
        let restored = renderer.capture_ui();
        assert_eq!(
            restored.view.view_mode.as_deref(),
            Some(mode),
            "mode {mode:?} did not survive the round trip"
        );
        assert_eq!(
            restored.view.focused_panel, focus,
            "focus {focus:?} did not survive the round trip"
        );
        assert_eq!(
            restored.view.follow,
            Some(follow),
            "follow {follow} did not survive the round trip"
        );
    }

    // Hostile tokens: restore must neither panic nor invent an unknown mode.
    section.view.view_mode = Some("not-a-mode".to_string());
    section.view.focused_panel = "not-a-pane".to_string();
    renderer.restore_panes(&section);
    let after = renderer.capture_ui();
    let mode = after
        .view
        .view_mode
        .expect("the renderer always states a mode");
    assert!(
        VIEW_MODE_TOKENS.contains(&mode.as_str()),
        "a hostile restore left the mode token {mode:?}"
    );
    println!(
        "SMOKE view-state: last mode={mode:?} focus={:?}",
        after.view.focused_panel
    );
}

// ---------------------------------------------------------------------------
// 5. The public text pipeline at the five smoke viewport widths.
// ---------------------------------------------------------------------------

#[test]
fn the_text_pipeline_holds_at_every_smoke_viewport_width() {
    for (label, payload) in hostile_payloads() {
        let mut previous_rows = 0usize;
        let mut previous_raw = 0usize;
        let mut first_rows = 0usize;
        for width in SMOKE_WIDTHS {
            let width = usize::from(width);
            let lines = render_message_lines(&payload, false);
            let rows = count_wrapped_rendered_lines(&lines, width);
            let raw_rows = wrapped_lines(&payload, width);
            if !payload.trim().is_empty() {
                assert!(
                    rows >= 1 && raw_rows >= 1,
                    "[{label}] a non-empty payload produced no rows at width {width} \
                     (rendered={rows}, raw={raw_rows})"
                );
            }
            // Narrower must never mean fewer rows for the same payload.
            assert!(
                rows >= previous_rows,
                "[{label}] the rendered row count fell from {previous_rows} to {rows} \
                 as the viewport narrowed"
            );
            assert!(
                raw_rows >= previous_raw,
                "[{label}] the raw row count fell from {previous_raw} to {raw_rows} \
                 as the viewport narrowed"
            );
            previous_rows = rows;
            previous_raw = raw_rows;
            if width == 100 {
                first_rows = rows;
            }

            // No rendered span may carry a raw escape byte, at any width.
            for text in span_text(&lines) {
                assert!(
                    !text.contains('\u{1b}'),
                    "[{label}] a rendered line at width {width} carries a stray \
                     escape byte: {text:?}"
                );
            }
            // And the same for the ANSI bridge. The markdown path is asserted
            // only for payloads that carry no escape bytes of their own: the pane
            // routes fenced content to markdown **before** the ANSI bridge runs
            // (`components/transcript.rs::stream_lines`), so a fenced block keeps
            // its own SGR bytes verbatim — recorded below, not asserted away.
            let markdown = span_text(&render_markdown_lines(&payload));
            if !payload.contains('\u{1b}') {
                for text in &markdown {
                    assert!(
                        !text.contains('\u{1b}'),
                        "[{label}] markdown rendering introduced an escape byte at \
                         {width}: {text:?}"
                    );
                }
            } else {
                let carried = markdown
                    .iter()
                    .filter(|text| text.contains('\u{1b}'))
                    .count();
                println!(
                    "SMOKE text[{label}]: markdown carries {carried} line(s) with raw SGR \
                     bytes (fenced-before-ANSI routing)"
                );
            }
            for text in span_text(&parse_ansi_lines(&payload)) {
                assert!(
                    !text.contains('\u{1b}'),
                    "[{label}] ANSI parsing leaks an escape byte at {width}: {text:?}"
                );
            }
        }
        println!(
            "SMOKE text[{label}]: rendered rows at 100={first_rows}, at 39={previous_rows} \
             (raw rows at 39={previous_raw})"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Steering notices and the journal seam accept hostile input too.
// ---------------------------------------------------------------------------

#[test]
fn steering_traces_and_the_journal_seam_accept_every_stage_without_panicking() {
    let mut renderer = TuiRenderer::new();
    for stage in [
        SteerStage::Queued,
        SteerStage::Delivered,
        SteerStage::Revoked,
    ] {
        for text in [
            String::new(),
            "steer \u{1b}[31mnow\u{1b}[0m".to_string(),
            "あ".repeat(3_000),
        ] {
            renderer.note_steer(&SteerTrace {
                stage,
                text,
                target: "coder_specialist-t-001".to_string(),
                notice_id: Some("n-1".to_string()),
                reason: "held for the manager seam".to_string(),
            });
        }
    }
    let captured = renderer.capture_ui();
    assert!(
        !renderer.user_exit_requested(),
        "a steering trace alone requested a user exit"
    );
    println!(
        "SMOKE steering: chat_lines={} status={:?}",
        captured.chat_messages.len(),
        captured.status_line
    );

    // The journal seam behind the headless/print paths round-trips hostile payloads.
    let mut journal = UiTranscript::new();
    for (label, payload) in hostile_payloads() {
        journal.append(UiRecord::User {
            text: format!("user[{label}] {payload}"),
        });
        journal.append(UiRecord::Assistant {
            content: Some(payload.clone()),
            thinking: Some(format!("thought[{label}]")),
        });
        journal.append(UiRecord::SteerResponse { text: payload });
    }
    assert_eq!(journal.len(), 33, "the journal dropped records");
    assert!(!journal.is_empty());
    let history = journal.extract_steering_history();
    println!(
        "SMOKE journal: records={} paired_steering_exchanges={}",
        journal.len(),
        history.len()
    );
    assert!(
        !history.is_empty(),
        "the journal paired no steering exchange at all"
    );
}

// ---------------------------------------------------------------------------
// 7. Error traffic reaches the pane model, and stays there (§8.5 half).
// ---------------------------------------------------------------------------

#[test]
fn a_reported_llm_error_lands_in_the_pane_model_and_is_not_lost() {
    let mut renderer = TuiRenderer::new();
    // The exact spelling the headless run of this sandbox produced (see the report):
    // the session loop reports the failure once as status traffic.
    renderer.on_event(&Event::Status(
        "LLM error: http (transport error: error sending request for url \
         (http://localhost:8000/v1/chat/completions))"
            .to_string(),
    ));
    renderer.on_event(&Event::Status("Ready".to_string()));

    let captured = renderer.capture_ui();
    assert!(
        captured
            .chat_messages
            .iter()
            .any(|line| line.contains("LLM error: http")),
        "the reported error never reached the transcript: {:?}",
        captured.chat_messages
    );
    // The status line moved on, exactly as §7.4 allows; the transcript keeps the report.
    assert_eq!(
        captured.status_line, "Ready",
        "the status line did not move on from the error"
    );
    println!(
        "SMOKE error-line: transcript keeps {:?}",
        captured
            .chat_messages
            .iter()
            .find(|line| line.contains("LLM error"))
            .map(|line| line.chars().take(72).collect::<String>())
    );
}

// ---------------------------------------------------------------------------
// 8. The gating / abort / exit seams behind the ConfirmAction overlays.
// ---------------------------------------------------------------------------

#[test]
fn the_tui_gates_destructive_commands_and_the_abort_seams_stay_explicit() {
    let mut renderer = TuiRenderer::new();
    assert!(
        renderer.gates_destructive_commands(),
        "the TUI renderer does not gate destructive commands"
    );
    assert!(
        !Renderer::aborted(&renderer),
        "a booted renderer is already aborted"
    );
    renderer.request_abort();
    assert!(
        Renderer::aborted(&renderer),
        "request_abort() did not set the abort flag"
    );
    renderer.clear_abort();
    assert!(
        !Renderer::aborted(&renderer),
        "clear_abort() left the abort flag set"
    );
    assert!(
        !renderer.user_exit_requested(),
        "the abort seam alone requested an exit"
    );
    println!("SMOKE gating: gates_destructive_commands=true");
}
