//! Frame-level pins for the rebuilt TUI: what one `TestBackend` pass must show.
//!
//! Spec `docs/ui_design_spec.md` §2 (layout + degradation ladder), §4 (modes and
//! focus), §5 (keymap), §7 (palette/glyphs), §8 (states) and — binding — the §10
//! acceptance checklist items **16–21**, each pinned by at least one test named
//! after it.
//!
//! This module is deliberately self-contained: it builds its own fixtures and its
//! own draw helpers rather than reaching into `tests.rs`, so the two suites can be
//! read and changed independently. Assertions are on **stable substrings, glyph
//! presence and pane geometry**, never on a whole-frame snapshot, so an unrelated
//! pixel cannot break a pin. Nothing here sleeps or waits on a clock: the 3 s
//! abort window is only ever read through `is_abort_armed()` immediately after the
//! key that armed it.
//!
//! Two things are never done, because both are process-wide side effects rather
//! than a UI assertion: `y` is never pressed on a **Quit** or **CancelAll** request
//! (`request_user_exit` and `cancel_every_worker` cancel work — and the
//! process-wide cancellation token — across the whole process, including registry
//! entries owned by suites that take no lock; the executed broadcast is pinned in
//! `tests/test_session_restore_panel_abort.rs` instead), and a frame is never
//! painted through the *crossterm* backend (`render_terminal` would write escape
//! codes into the test runner's stdout). A **single-worker** `y` *is* pressed: it
//! addresses one registry entry by name, so it is pinned against live entries the
//! test itself plants (§10 item 17, `CancellationLedger` below).

use super::*;
use crate::orchestrator::workers;
use crate::ui::QueueState;
use crate::ui::Renderer as _;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::style::{Color, Style};
use tokio_util::sync::CancellationToken;

/// One frame pass: the text of every row, the cell grid, and the rects it used.
type Frame = (Vec<String>, Grid, layout::PaneRects);
/// The same, with the style of every cell instead of the symbol grid.
type StyledFrame = (Vec<String>, Vec<Vec<Style>>, layout::PaneRects);
/// The symbol of every cell, one `Vec` per row.
type Grid = Vec<Vec<String>>;

// ---- local fixtures ---------------------------------------------------------

/// The preview scenario of §2.2 / `docs/new_ui_preview.txt`: a 7-task plan with 3
/// checked and three live worker rows (running / queued / paused). One fixture for
/// every frame test, so no geometry assertion is ever accidentally about an empty
/// screen.
const MIXED_PLAN: &str = "# Plan\n\n\
    - [x] [t-001] research UX baseline\n\
    - [x] [t-002] audit TUI architecture\n\
    - [x] [t-003] draft the design spec\n\
    - [ ] [t-004] scaffold state, layout, theme\n\
    - [ ] [t-005] pane chrome and chips\n\
    - [ ] [t-006] wire the roster rank into the status row\n\
    - [ ] [t-007] pin the frame at four sizes\n";

/// M4 (both side panes on) with the plan, the transcript and three worker rows.
/// The mode is set through the transition helper because it is stored view state.
fn m4_renderer() -> TuiRenderer {
    let mut r = TuiRenderer::new();
    r.plan_content = MIXED_PLAN.to_string();
    r.messages = vec![
        "User: rebuild the TUI so plan, workers and progress stay legible".to_string(),
        "Marmennill: I split the work into seven tasks and started with the audit.".to_string(),
        "[Status] delegating validator_specialist (t-003)".to_string(),
    ];
    r.subagents = vec![
        SubagentDetail {
            name: "coder_specialist-t-004".to_string(),
            task_id: Some("t-004".to_string()),
            is_active: true,
            ..SubagentDetail::default()
        },
        SubagentDetail {
            name: "researcher_specialist-t-009".to_string(),
            task_id: Some("t-009".to_string()),
            is_active: true,
            queue: Some(QueueState { position: 2, of: 3 }),
            ..SubagentDetail::default()
        },
        SubagentDetail {
            name: "validator_specialist-t-003".to_string(),
            task_id: Some("t-003".to_string()),
            is_active: true,
            paused: true,
            ..SubagentDetail::default()
        },
    ];
    r.set_view_mode(state::ViewMode::Split);
    r
}

// ---- local draw helpers ----------------------------------------------------

/// Feed one synthetic key press through the same seam the live loop calls.
fn press(r: &mut TuiRenderer, code: KeyCode, modifiers: KeyModifiers) -> bool {
    r.apply_terminal_event(&crossterm::event::Event::Key(KeyEvent::new(
        code, modifiers,
    )))
}

/// A single unmodified keystroke of the §5 keymap.
fn key(r: &mut TuiRenderer, code: KeyCode) -> bool {
    press(r, code, KeyModifiers::NONE)
}

/// Type a string, one printable per event.
fn type_text(r: &mut TuiRenderer, text: &str) {
    for c in text.chars() {
        press(r, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

/// Draw one whole frame and return its rows, its cell grid, and the pane rects the
/// pass established — so one test can assert pixels and geometry from one pass.
fn draw_frame(r: &mut TuiRenderer, width: u16, height: u16) -> Frame {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut painted: Option<layout::PaneRects> = None;
    terminal
        .draw(|frame| {
            painted = Some(r.draw_new_ui(frame));
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let grid: Grid = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        })
        .collect();
    let rows = grid.iter().map(|row| row.concat()).collect();
    (
        rows,
        grid,
        painted.expect("one frame pass reports its rects"),
    )
}

/// Just the text rows of one frame.
fn frame_text(r: &mut TuiRenderer, width: u16, height: u16) -> Vec<String> {
    draw_frame(r, width, height).0
}

/// One frame with the style of every cell, for the §7.1/§7.3 colour contract.
fn draw_styled(r: &mut TuiRenderer, width: u16, height: u16) -> StyledFrame {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut painted: Option<layout::PaneRects> = None;
    terminal
        .draw(|frame| {
            painted = Some(r.draw_new_ui(frame));
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
    (
        rows,
        styles,
        painted.expect("one frame pass reports its rects"),
    )
}

/// The colour a pane's border was drawn with, read at its top-left corner cell.
fn corner_colour(styles: &[Vec<Style>], pane: Rect) -> Option<Color> {
    styles
        .get(usize::from(pane.y))?
        .get(usize::from(pane.x))
        .and_then(|style| style.fg)
}

/// Every leaf pane rect of a frame, labelled. `None` panes (§2.4.1) are absent
/// rather than zero-sized, so they never appear here.
fn leaf_rects(rects: &layout::PaneRects) -> Vec<(&'static str, Rect)> {
    let mut out: Vec<(&'static str, Rect)> = Vec::new();
    for (label, rect) in [
        ("header", rects.header),
        ("status", rects.status),
        ("input", rects.input),
        ("footer", rects.footer),
        ("transcript", rects.transcript),
        ("plan", rects.plan),
        ("workers", rects.workers),
        ("notice", rects.notice),
    ] {
        if let Some(rect) = rect {
            out.push((label, rect));
        }
    }
    out
}

/// True when two rects share at least one cell.
fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

/// The §10 item 19 pixel rule: no rendered cell may fall outside every pane rect
/// the frame produced (the frame-level form of "no drawn line exceeds its rect").
fn cells_outside_every_pane(grid: &Grid, rects: &layout::PaneRects) -> Vec<(usize, usize, String)> {
    let panes = leaf_rects(rects);
    let mut offenders = Vec::new();
    for (y, row) in grid.iter().enumerate() {
        for (x, symbol) in row.iter().enumerate() {
            if symbol == " " || symbol.is_empty() {
                continue;
            }
            let inside = panes.iter().any(|(_, rect)| {
                x >= usize::from(rect.x)
                    && x < usize::from(rect.x) + usize::from(rect.width)
                    && y >= usize::from(rect.y)
                    && y < usize::from(rect.y) + usize::from(rect.height)
            });
            if !inside {
                offenders.push((x, y, symbol.clone()));
            }
        }
    }
    offenders
}

/// Drain everything a submit handed to the session loop.
fn drain_submissions(r: &mut TuiRenderer) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(line) = r.rx.try_recv() {
        out.push(line);
    }
    out
}

/// The composer draft as the panes see it.
fn draft(r: &mut TuiRenderer) -> String {
    r.ensure_textarea_in_sync();
    r.textarea.lines().join("\n")
}

/// The rows a rect covers, so a test can compare the body of a frame between two
/// passes without the live `elapsed …` clock being part of the comparison.
fn rows_within(rows: &[String], rect: Rect) -> Vec<String> {
    let start = usize::from(rect.y).min(rows.len());
    let end = usize::from(rect.y + rect.height).min(rows.len());
    rows[start..end].to_vec()
}

/// A frame with the two live fields a second pass of the *same* renderer may
/// legitimately differ on taken out: the session clock (`elapsed …`) and the token
/// counters. The clock is this renderer's own wall clock, so a frame pin must not be
/// a stopwatch. The counters are per-renderer state: the process-wide llm totals
/// reach the status row only as the snapshot the paint path stores once per frame
/// (audit D5), which a `TestBackend` pin never injects — but a pin that fed this
/// renderer an `Event::TokensIn` / `TokensOut` still sees its own totals move
/// between two draws, so the field stays stripped here as well. The D5 pins at the
/// end of this file assert the injected form directly.
fn snapshot(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let row = match row.find("elapsed") {
                Some(at) => &row[..at],
                None => row.as_str(),
            };
            match (row.find("Tokens"), row.find(" out")) {
                (Some(at), Some(end)) if end > at => format!("{}{}", &row[..at], &row[end..]),
                _ => row.to_string(),
            }
        })
        .collect()
}

/// The rows of a frame joined into one string, for "is this text on screen".
fn on_screen(rows: &[String]) -> String {
    rows.join("\n")
}

/// The cells strictly inside one pane rect — its body, border excluded — with the
/// scrollbar glyphs a pane draws on its inner right edge stripped off.
fn pane_body(rows: &[String], pane: Rect) -> Vec<String> {
    let x = usize::from(pane.x) + 1;
    let width = usize::from(pane.width).saturating_sub(2);
    let y = usize::from(pane.y) + 1;
    let height = usize::from(pane.height).saturating_sub(2);
    rows[y..y + height]
        .iter()
        .map(|row| {
            row.chars()
                .skip(x)
                .take(width)
                .collect::<String>()
                .trim_end()
                .trim_end_matches(['│', '║', '▲', '▼', '█'])
                .to_string()
        })
        .collect()
}

/// What one pane says, its body rows folded back into a single line. A pane wraps
/// at its own width, and in M4 the transcript is the narrow side column, so a sentence
/// can straddle two rows: folding keeps an assertion about the pane's message
/// independent of the width it happens to be drawn at.
fn pane_says(rows: &[String], pane: Rect) -> String {
    pane_body(rows, pane)
        .join(" ")
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

/// The horizontal reading order of the three body panes of one frame: leftmost
/// first, and within one column the higher pane first. This is the order an
/// operator's eye walks, and it is what the single slot rule of t-024b pins: wherever
/// a frame draws both the Plan and the Transcript, the plan is the first entry.
fn reading_order(rects: &layout::PaneRects) -> Vec<&'static str> {
    let mut panes: Vec<(&'static str, Rect)> = Vec::new();
    for (label, rect) in [
        ("transcript", rects.transcript),
        ("plan", rects.plan),
        ("workers", rects.workers),
    ] {
        if let Some(rect) = rect {
            panes.push((label, rect));
        }
    }
    panes.sort_by_key(|(_, rect)| (rect.x, rect.y));
    panes.iter().map(|(label, _)| *label).collect()
}

/// The single global slot rule of t-024b as one assertion, so every geometry pin can
/// state the same thing instead of a shape-specific trade: where a frame draws both
/// the Plan and the Transcript, the **plan owns the leading slot** and the transcript
/// the trailing one. Leading is the leftmost column of a columns split, the topmost
/// row of a stack and the wide column of a fallback whose side column cannot stack —
/// and it is always flush with the body's own left edge, which is what stops the plan
/// from jumping sides between modes at the same size. The slot widths stay the §2.3
/// pair: the leading slot is never the narrower one, and the two columns of a split
/// are a split and not a gap.
///
/// A frame that draws no Plan pane (M1, M3, and the ladder shapes that keep the
/// roster instead of the plan so the focus is never stranded) is the rule's **vacuous**
/// case, not an exception to it, and returns unchecked.
fn assert_plan_leads_the_transcript(label: &str, rects: &layout::PaneRects) {
    let (Some(plan), Some(transcript)) = (rects.plan, rects.transcript) else {
        return;
    };
    let body = rects.body.expect("a frame that draws two panes has a body");
    assert_eq!(
        plan.x, body.x,
        "{label}: the plan owns the leading edge of the body {body:?}: {rects:?}"
    );
    if plan.x == transcript.x {
        assert!(
            plan.y < transcript.y,
            "{label}: one column, so the plan's rows come first: {rects:?}"
        );
    } else {
        assert!(
            plan.x < transcript.x,
            "{label}: the plan's column comes first: {rects:?}"
        );
        assert_eq!(
            transcript.x,
            plan.x + plan.width,
            "{label}: the two columns are one split, not a gap: {rects:?}"
        );
    }
    assert!(
        plan.width >= transcript.width,
        "{label}: the leading slot is never the narrower one: plan {plan:?} \
         transcript {transcript:?}"
    );
    assert_eq!(
        reading_order(rects).first(),
        Some(&"plan"),
        "{label}: the eye reaches the plan before the transcript: {rects:?}"
    );
}

// ===========================================================================
// §10 item 16 — an overlay closes on Esc without arming abort; y/n are honoured
// only inside the confirm overlay.
// ===========================================================================

/// Item 16, first half: the `?`/`F1` legend closes on `Esc` and that `Esc` neither
/// arms abort nor exits, and the body underneath comes back unchanged.
#[test]
fn an_open_help_overlay_closes_on_esc_without_arming_abort_item16() {
    let mut r = m4_renderer();
    let before = snapshot(&frame_text(&mut r, 100, 30));

    // `?` on an empty composer opens the legend (§5).
    assert!(key(&mut r, KeyCode::Char('?')));
    assert_eq!(r.overlay, state::OverlayState::Help);
    let open = frame_text(&mut r, 100, 30);
    assert!(
        on_screen(&open).contains("Keys") && on_screen(&open).contains(" Esc close "),
        "the legend names itself and its own close key:\n{}",
        on_screen(&open)
    );

    // `Esc` spends itself on the surface: nothing is armed, nothing exits.
    assert!(key(&mut r, KeyCode::Esc));
    assert_eq!(r.overlay, state::OverlayState::None);
    assert!(
        !r.confirm_abort,
        "closing the legend must not arm abort (item 16)"
    );
    assert!(!r.is_abort_armed());
    assert!(!r.user_exit_requested(), "and it must not exit either");

    // The frame comes back exactly as it was before the overlay opened.
    let after = snapshot(&frame_text(&mut r, 100, 30));
    assert_eq!(before, after, "the body is restored, not repainted askew");

    // `F1` opens the same surface and `Ctrl+C` (the §5 spelling of `Esc`) closes it.
    assert!(key(&mut r, KeyCode::F(1)));
    assert_eq!(r.overlay, state::OverlayState::Help);
    assert!(press(&mut r, KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(r.overlay, state::OverlayState::None);
    assert!(!r.is_abort_armed(), "Ctrl+C out of the legend is also safe");
}

/// Item 16, second half: `y` / `n` belong to the confirm overlay and to nothing
/// else. Outside it — no surface, the legend, or the armed-abort prompt, which
/// holds no request — they are ordinary characters.
#[test]
fn y_and_n_are_honoured_only_inside_the_confirm_overlay_item16() {
    // (a) No overlay at all: `y` and `n` type.
    let mut r = m4_renderer();
    assert!(key(&mut r, KeyCode::Char('y')));
    assert!(key(&mut r, KeyCode::Char('n')));
    assert_eq!(draft(&mut r), "yn", "typing `no` must still type");
    assert_eq!(r.overlay, state::OverlayState::None);

    // (b) The legend open: they still type, and the surface stays open.
    let mut r = m4_renderer();
    key(&mut r, KeyCode::F(1));
    assert!(key(&mut r, KeyCode::Char('y')));
    assert_eq!(draft(&mut r), "y");
    assert_eq!(
        r.overlay,
        state::OverlayState::Help,
        "`y` is not an answer to the legend"
    );

    // (c) The armed-abort prompt occupies the same surface but holds no request,
    // so a printable `y` is typing that disarms — never consent (§5, §8.5).
    let mut r = m4_renderer();
    assert!(key(&mut r, KeyCode::Esc), "arm");
    assert!(r.is_abort_armed());
    assert_eq!(r.overlay, state::OverlayState::Confirm);
    assert!(
        r.confirm_action.is_none(),
        "the armed prompt holds no request"
    );
    let armed = frame_text(&mut r, 100, 30);
    assert!(
        on_screen(&armed).contains("Abort armed — Esc / Ctrl+C again to abort"),
        "the surface states what it is:\n{}",
        on_screen(&armed)
    );
    assert!(key(&mut r, KeyCode::Char('y')));
    assert_eq!(draft(&mut r), "y");
    assert!(
        !r.is_abort_armed(),
        "a printable cancels the arm instead of obeying it"
    );
    assert!(!r.user_exit_requested());
    assert_eq!(r.overlay, state::OverlayState::None);

    // (d) Inside a real confirm: `n` is honoured, the session stays alive and
    // un-armed, and the body comes back.
    let mut r = m4_renderer();
    let before = snapshot(&frame_text(&mut r, 100, 30));
    assert!(press(&mut r, KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(r.overlay, state::OverlayState::Confirm);
    assert_eq!(r.confirm_action, Some(events::ConfirmAction::Quit));
    assert!(key(&mut r, KeyCode::Char('n')));
    assert_eq!(r.confirm_action, None);
    assert_eq!(r.overlay, state::OverlayState::None);
    assert_eq!(r.status_line, "Quit cancelled.");
    assert!(!r.is_abort_armed(), "`n` must not arm abort either");
    assert!(!r.user_exit_requested());
    assert_eq!(snapshot(&frame_text(&mut r, 100, 30)), before);
}

/// Item 16, drawing half: every `ConfirmAction` puts its own request on one
/// surface, drawn last over the **body only** — the header, the status rows, the
/// composer and the footer stay on screen — and `Esc` leaves the state as it was.
#[test]
fn every_confirm_action_draws_its_surface_last_over_the_body_item16() {
    /// One destructive path: the state to establish before the request, the
    /// keystrokes that make the request, and the two lines the surface must print
    /// about it.
    struct ConfirmCase {
        label: &'static str,
        setup: fn(&mut TuiRenderer),
        request: fn(&mut TuiRenderer),
        action: &'static str,
        consequence: &'static str,
    }

    let requests = [
        ConfirmCase {
            label: "Quit (Ctrl+D)",
            setup: |_| {},
            request: |r| {
                press(r, KeyCode::Char('d'), KeyModifiers::CONTROL);
            },
            action: "Quit the session (Ctrl+D)",
            consequence: "Ends the session and cancels every running worker.",
        },
        ConfirmCase {
            label: "CancelWorker (Ctrl+X)",
            // `Ctrl+X` only arms from the roster, so the *before* frame already has
            // the roster focused: the compare below is then about the overlay alone,
            // never about the focus ring.
            setup: |r| {
                r.set_focus(state::FocusPane::Workers);
            },
            request: |r| {
                press(r, KeyCode::Char('x'), KeyModifiers::CONTROL);
            },
            action: "Cancel worker 'coder_specialist-t-004'",
            consequence: "Stops that one worker; every other worker keeps running.",
        },
        ConfirmCase {
            label: "CancelAll (/cancel-all)",
            setup: |_| {},
            request: |r| {
                type_text(r, "/cancel-all");
                key(r, KeyCode::Enter);
            },
            action: "Cancel every worker (/cancel-all)",
            consequence: "Cancels every running worker and every in-flight stream.",
        },
        ConfirmCase {
            label: "Command (/reset)",
            setup: |_| {},
            request: |r| {
                type_text(r, "/reset");
                key(r, KeyCode::Enter);
            },
            action: "Reset the execution plan (/reset)",
            consequence: concat!(
                "Clears plan, session bundle, steering history and transcript, and ",
                "tells the model."
            ),
        },
    ];

    for case in requests {
        let ConfirmCase {
            label,
            setup,
            request,
            action,
            consequence,
        } = case;
        let mut r = m4_renderer();
        setup(&mut r);
        let before = frame_text(&mut r, 100, 30);
        request(&mut r);
        assert_eq!(
            r.overlay,
            state::OverlayState::Confirm,
            "{label}: every destructive path lands on the confirm surface"
        );
        let (rows, grid, rects) = draw_frame(&mut r, 100, 30);
        let screen = on_screen(&rows);

        // The surface names itself, the pending request and the keys it honours.
        assert!(
            screen.contains("Confirm"),
            "{label}: title missing:\n{screen}"
        );
        assert!(
            screen.contains(action),
            "{label}: the action the surface will run is missing:\n{screen}"
        );
        assert!(
            screen.contains("y / n — confirm / dismiss the pending action"),
            "{label}: the overlay advertises only keys it honours:\n{screen}"
        );
        // The consequence may be clipped by the 80 % clamp at 100 columns; its
        // leading words must still be there, so the prompt cannot describe a
        // different action.
        let head: String = consequence.chars().take(20).collect();
        assert!(
            screen.contains(head.as_str()),
            "{label}: the consequence line is missing its head {head:?}:\n{screen}"
        );
        // §7.3: the surface is drawn into the body rect, last, and it never spills
        // past the body — its own box is inside the body and it composites over the
        // painted panes rather than replacing them.
        let body = rects.body.expect("a 100x30 frame has a body");
        let transcript = rects.transcript.expect("M4 draws the transcript");
        let title_row = rows
            .iter()
            .position(|row| row.contains("Confirm ─") || row.contains("Confirm"))
            .expect("the surface titles itself");
        assert!(
            usize::from(body.y) < title_row && title_row < usize::from(body.y + body.height) - 1,
            "{label}: the title row {title_row} is inside the body {body:?}"
        );
        let left = grid[title_row]
            .iter()
            .position(|cell| cell == "┌")
            .expect("a bordered surface");
        let right = grid[title_row]
            .iter()
            .rposition(|cell| cell == "┐")
            .expect("the box closes on the same row");
        assert!(
            usize::from(body.x) <= left && right < usize::from(body.x + body.width) && left < right,
            "{label}: the box spans {left}..{right} and must stay inside the body {body:?}"
        );
        assert!(
            left < usize::from(transcript.x + transcript.width),
            "{label}: the box starts over the pane underneath it, so it is drawn last"
        );
        // The covered panes keep their own borders on that row: `Clear` plus the box
        // only reach the body's middle, never a pane's outer frame. Under the global
        // slot rule the pane whose left border sits on the body's left edge is the
        // Plan — the Transcript heads the side column, which the box does reach.
        let left_column = rects
            .plan
            .expect("M4 draws the plan in the wide left column");
        assert_eq!(
            left_column.x, body.x,
            "{label}: the plan pane holds the body's left edge"
        );
        assert_eq!(
            grid[title_row][usize::from(left_column.x)].as_str(),
            "│",
            "{label}: the covered pane keeps its left border"
        );
        assert_eq!(
            grid[title_row][usize::from(body.x + body.width) - 1].as_str(),
            "│",
            "{label}: the body's far pane keeps its right border"
        );
        assert!(
            rows[usize::from(transcript.y)].contains("Transcript"),
            "{label}: the covered pane is still titled above the box"
        );

        // Chrome survives the overlay: every chrome row is still itself, which is
        // what keeps the metrics counting behind a confirm (§7.3, G4).
        let header_row = rows[usize::from(rects.header.expect("header").y)].trim_end();
        assert!(
            header_row.contains("Marmel") && header_row.contains("3/7 done"),
            "{label}: the header keeps counting under the overlay: {header_row:?}"
        );
        let status_row = rows[usize::from(rects.status.expect("status").y)].trim_end();
        assert!(
            status_row.contains("Tokens"),
            "{label}: status row 1 keeps counting: {status_row:?}"
        );
        let input_row = rows[usize::from(rects.input.expect("input").y)].trim_end();
        assert!(
            input_row.contains("Input"),
            "{label}: the composer stays drawn: {input_row:?}"
        );
        let footer = rects.footer.expect("30 rows keep the footer");
        let footer_row = rows[usize::from(footer.y)].trim_end();
        assert!(
            footer_row.contains("F1 help"),
            "{label}: the footer legend stays: {footer_row:?}"
        );

        // Inside the body the overlay replaced what the panes drew.
        assert_ne!(
            rows_within(&rows, body),
            rows_within(&before, body),
            "{label}: the overlay must actually cover part of the body"
        );

        // `Esc` closes it without arming, and the body is exactly what it was.
        assert!(key(&mut r, KeyCode::Esc));
        assert_eq!(
            r.overlay,
            state::OverlayState::None,
            "{label}: Esc closed it"
        );
        assert!(
            r.confirm_action.is_none(),
            "{label}: the request is dropped"
        );
        assert!(!r.is_abort_armed(), "{label}: and nothing was armed");
        assert!(!r.user_exit_requested(), "{label}: and nothing exited");
        let after = frame_text(&mut r, 100, 30);
        assert_eq!(
            rows_within(&after, body),
            rows_within(&before, body),
            "{label}: the body comes back exactly as it was"
        );
        let screen = on_screen(&after);
        assert!(
            !screen.contains("Confirm") && !screen.contains(" Esc close "),
            "{label}: no surface is left on screen:\n{screen}"
        );
    }
}

/// Item 16, stacking half: a confirm opened on top of the legend returns to the
/// legend on `Esc`, and neither press arms abort.
#[test]
fn a_confirm_over_the_help_legend_returns_to_the_legend_on_esc_item16() {
    let mut r = m4_renderer();
    key(&mut r, KeyCode::F(1));
    assert_eq!(r.overlay, state::OverlayState::Help);
    press(&mut r, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(
        r.overlay,
        state::OverlayState::Confirm,
        "a pending confirm wins the one surface per frame (§7.3)"
    );

    key(&mut r, KeyCode::Esc);
    assert_eq!(
        r.overlay,
        state::OverlayState::Help,
        "the legend the operator opened comes back underneath"
    );
    assert!(r.confirm_action.is_none());
    assert!(!r.is_abort_armed());
    let rows = frame_text(&mut r, 100, 30);
    assert!(on_screen(&rows).contains("Keys"), "legend body: {rows:?}");

    key(&mut r, KeyCode::Esc);
    assert_eq!(r.overlay, state::OverlayState::None);
    assert!(
        !r.is_abort_armed(),
        "the second Esc was spent on the legend too"
    );
}

// ===========================================================================
// §10 item 17 — cancel-one-worker: exactly one worker path, and the broadcast
// cancellation is not reachable from it.
// ===========================================================================

// ---- the cancellation seam --------------------------------------------------
//
// A confirm answer does not *return* a cancellation, it causes one: `y` calls
// `orchestrator::cancel_active_worker` for one row (`events.rs:950`) or
// `orchestrator::cancel_all` for the broadcast (`events.rs:991`), and the effect
// of both lands in the process-global worker registry. The registry is therefore
// the honest place to observe **which action the UI requested and which worker it
// targeted**, so these tests plant live entries there instead of reading
// `events.rs` as text and counting call sites:
//
// * one `CancellationToken` per planted row — the token that gets fired is the
//   identity of the worker the UI actually addressed;
// * the entry's `status` — `"In Progress"` until a cancel path labels it
//   `"Aborted"` (`workers.rs:1470` for one worker, `:1494` for the broadcast);
// * the process-wide token, cloned at plant time — the broadcast's own
//   fingerprint (`bus.rs:38`), which a single-worker answer must never touch.
//
// `workers::TEST_WORKERS_MUTEX` is the lock every other suite in this binary
// takes before it touches the registry or the global token: the arbitration tests
// reset the token under it (`bridge/tests.rs:1319`) and the worker suite runs the
// only other broadcast call site under it (`workers.rs:1652`). Holding it for a
// whole test is what turns "and nothing else was cancelled" into a decidable
// fact rather than a race against a parallel suite.

/// Three roster rows private to this file. `{agent}-{task}` is the natural
/// registry key (`workers.rs:816`), so a planted entry's effective key is
/// byte-identical to the row name the UI puts into `ConfirmAction::CancelWorker`,
/// and no other suite's key can collide with these.
const ITEM17_ROWS: [(&str, &str); 3] = [
    ("item17_alpha_specialist", "t-9317"),
    ("item17_beta_specialist", "t-9318"),
    ("item17_gamma_specialist", "t-9319"),
];

/// The registry key of one fixture row, spelled the way the registry spells it.
fn item17_key(agent: &str, task: &str) -> String {
    format!("{agent}-{task}")
}

/// The roster half of the fixture: the three rows above, live, in row order, so
/// selection index `n` and planted key `n` name the same worker.
fn item17_renderer() -> TuiRenderer {
    let mut r = TuiRenderer::new();
    r.plan_content = MIXED_PLAN.to_string();
    r.subagents = ITEM17_ROWS
        .iter()
        .map(|(agent, task)| SubagentDetail {
            name: item17_key(agent, task),
            task_id: Some((*task).to_string()),
            is_active: true,
            ..SubagentDetail::default()
        })
        .collect();
    r.set_view_mode(state::ViewMode::Split);
    r
}

/// One planted registry entry, together with the token a cancellation must fire.
struct PlantedWorker {
    key: String,
    token: CancellationToken,
    _guard: workers::ActiveWorkerGuard,
}

/// The recording half of the seam: live workers in the process-global registry,
/// planted under the exact keys the roster rows carry, plus the three
/// observations that decide item 17.
struct CancellationLedger {
    rows: Vec<PlantedWorker>,
}

impl CancellationLedger {
    /// Register one live worker per roster row, and pin the fixture's own
    /// precondition: the effective key IS the row name, so every key the UI can
    /// emit is an address this ledger can be read back against.
    fn plant() -> Self {
        let rows = ITEM17_ROWS
            .iter()
            .map(|(agent, task)| {
                let token = CancellationToken::new();
                let guard = workers::register_active_worker_with_token(
                    Some((*task).to_string()),
                    (*agent).to_string(),
                    "item 17 cancellation fixture".to_string(),
                    Some(token.clone()),
                );
                let key = item17_key(agent, task);
                assert_eq!(
                    guard.0, key,
                    "the row's key must be the entry's effective registry key"
                );
                PlantedWorker {
                    key,
                    token,
                    _guard: guard,
                }
            })
            .collect();
        Self { rows }
    }

    /// The rows whose own cancellation token has been fired, in row order.
    fn cancelled(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|row| row.token.is_cancelled())
            .map(|row| row.key.clone())
            .collect()
    }

    /// The rows the registry itself labels `Aborted`, in row order — what a
    /// cancellation leaves behind for every other reader of the roster.
    fn aborted(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|row| {
                workers::worker_state_snapshot(&row.key)
                    .is_some_and(|state| state.info.status == "Aborted")
            })
            .map(|row| row.key.clone())
            .collect()
    }

    /// The rows still running: token armed, entry live, not labelled aborted.
    fn untouched(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|row| {
                !row.token.is_cancelled()
                    && workers::worker_state_snapshot(&row.key)
                        .is_some_and(|state| state.is_active() && state.info.status != "Aborted")
            })
            .map(|row| row.key.clone())
            .collect()
    }

    /// The status the registry holds for one row.
    fn status(&self, key: &str) -> String {
        workers::worker_state_snapshot(key)
            .map(|state| state.info.status)
            .unwrap_or_else(|| panic!("the fixture lost its entry for {key}"))
    }
}

impl Drop for CancellationLedger {
    fn drop(&mut self) {
        // Leave the shared table exactly as this test found it: no completed
        // entries, no completed-ring eviction left behind.
        for row in &self.rows {
            let _ = workers::remove_worker_entry_for_test(&row.key);
        }
    }
}

/// Take the registry lock for a whole test (see the seam note above).
fn registry_gate() -> std::sync::MutexGuard<'static, ()> {
    workers::TEST_WORKERS_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Item 17: `Ctrl+X` on a selected roster row asks about **that** row and nothing
/// else — the request carries that row's key and task id, the surface names and
/// promises only that one worker, and `n` leaves all three planted workers
/// running: no token fired, no entry labelled `Aborted`, broadcast token armed.
#[test]
fn ctrl_x_asks_about_the_selected_row_and_only_that_row_item17() {
    let _registry = registry_gate();
    let ledger = CancellationLedger::plant();
    let broadcast = crate::orchestrator::global_cancellation_token();
    let (agent, task) = ITEM17_ROWS[1];
    let selected = item17_key(agent, task);

    let mut r = item17_renderer();
    r.set_focus(state::FocusPane::Workers);
    r.selected_subagent_idx = 1;
    let rows_before = r.subagents.len();

    assert!(press(&mut r, KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert_eq!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker {
            key: selected.clone(),
            task_id: Some(task.to_string()),
        }),
        "the identity is captured at request time (§5)"
    );
    assert!(
        ledger.cancelled().is_empty(),
        "asking cancels nothing: {:?}",
        ledger.cancelled()
    );

    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let screen = on_screen(&rows);
    assert!(
        screen.contains(&format!("Cancel worker '{selected}'")),
        "the surface names the selected row only:\n{screen}"
    );
    assert!(
        screen.contains("Stops that one worker"),
        "and promises only that one:\n{screen}"
    );
    for forbidden in [
        "Cancel every worker",
        "Cancels every running worker",
        "Quit the session",
    ] {
        assert!(
            !screen.contains(forbidden),
            "a single-worker request must not read like {forbidden:?}:\n{screen}"
        );
    }
    // The overlay lives in the body: the roster row it is about is still on
    // screen under it in the pane's own row order.
    let body = rects.body.expect("body");
    assert!(contains(body, rects.workers.expect("M4 draws Workers")));

    // `n`: the refusal is not a cancellation. Every planted worker keeps its
    // armed token, its live entry and its running label.
    assert!(key(&mut r, KeyCode::Char('n')));
    assert_eq!(
        r.status_line,
        format!("Worker '{selected}' was left running.")
    );
    assert_eq!(r.subagents.len(), rows_before);
    assert!(r.subagents.iter().any(|row| row.name == selected));
    assert!(!r.user_exit_requested());
    assert!(
        drain_submissions(&mut r).is_empty(),
        "nothing was sent to the loop"
    );
    assert_eq!(
        ledger.cancelled(),
        Vec::<String>::new(),
        "no row's token was fired"
    );
    assert_eq!(
        ledger.aborted(),
        Vec::<String>::new(),
        "no row was labelled aborted"
    );
    assert_eq!(ledger.untouched().len(), 3, "every row is still live");
    assert!(
        !broadcast.is_cancelled(),
        "declining never reaches the broadcast"
    );
}

/// Item 17: `y` on a single-worker request cancels **exactly that worker** — one
/// token fired, one registry entry labelled `Aborted`, the other planted rows
/// untouched and still labelled running — and never touches the process-wide
/// cancellation token the broadcast owns. The session keeps running, arms
/// nothing and sends nothing.
#[test]
fn confirming_one_worker_never_reaches_the_broadcast_cancellation_item17() {
    let _registry = registry_gate();
    let ledger = CancellationLedger::plant();
    let broadcast = crate::orchestrator::global_cancellation_token();
    let (agent, task) = ITEM17_ROWS[2];
    let target = item17_key(agent, task);

    let mut r = item17_renderer();
    r.set_focus(state::FocusPane::Workers);
    r.selected_subagent_idx = 2;
    assert!(press(&mut r, KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(matches!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker { .. })
    ));
    assert!(
        ledger.cancelled().is_empty(),
        "the request alone cancels nothing"
    );

    assert!(key(&mut r, KeyCode::Char('y')));
    // The effect of the answer, read off the registry the answer acts on: one
    // worker, and it is the row `Ctrl+X` was standing on.
    assert_eq!(
        ledger.cancelled(),
        vec![target.clone()],
        "exactly one worker was cancelled"
    );
    assert_eq!(
        ledger.aborted(),
        vec![target.clone()],
        "and exactly one roster entry carries the cancellation"
    );
    assert_eq!(ledger.status(&target), "Aborted");
    for (other_agent, other_task) in [ITEM17_ROWS[0], ITEM17_ROWS[1]] {
        let other = item17_key(other_agent, other_task);
        assert_eq!(
            ledger.status(&other),
            "In Progress",
            "{other} was swept by a broadcast"
        );
    }
    assert_eq!(
        ledger.untouched().len(),
        2,
        "the rows the request did not name keep running"
    );
    // The broadcast owns the process-wide token (`bus.rs:38`): an armed one is
    // the proof that a single-worker answer never reached it.
    assert!(
        !broadcast.is_cancelled(),
        "a single cancellation is not a broadcast"
    );

    assert_eq!(r.status_line, format!("Cancelled worker '{target}'."));
    assert!(
        !r.user_exit_requested(),
        "a single cancel never ends the session"
    );
    assert!(!r.is_abort_armed());
    assert_eq!(r.overlay, state::OverlayState::None);
    assert!(
        drain_submissions(&mut r).is_empty(),
        "and nothing was sent to the loop"
    );

    // THE one source-text assertion this suite keeps, and why. What is pinned
    // above is everything a test can observe: which request the keymap emitted,
    // which worker it addressed, and that the broadcast token stayed armed. What
    // no test in THIS binary can observe is the effect of the broadcast itself —
    // pressing `y` on `CancelAll` would run `orchestrator::cancel_all()` for the
    // whole test process, sweeping registry entries belonging to suites that do
    // not hold `TEST_WORKERS_MUTEX` (`ui/helpers.rs`, `ui/session.rs`,
    // `harness/dispatch_integrity_tests.rs`) — so the executed broadcast is
    // pinned in its own binary instead:
    // `tests/test_session_restore_panel_abort.rs`, ac-a4. That leaves exactly one
    // link unpinned from here, the one inside the confirm answer: that the bulk
    // cancellation is reachable from the `CancelAll` arm and from no other arm.
    let answer = fn_body(
        include_str!("events.rs"),
        "fn confirm_pending(&mut self) -> bool",
    );
    let (singles, broadcast_arm) = answer
        .split_once("Some(ConfirmAction::CancelAll)")
        .expect("the confirm answer dispatches on CancelAll");
    assert!(
        !singles.contains("cancel_every_worker")
            && broadcast_arm.contains("self.cancel_every_worker()"),
        "the bulk cancellation is reachable from the `CancelAll` arm and from no \
         other arm of the confirm answer"
    );
}

/// Item 17, the other spellings: `/cancel <task id>` and `/cancel <whole key>`
/// each resolve to the one worker they name — and the confirmed one cancels that
/// row and no other — while `/cancel-all` is a distinct request that neither
/// `Esc` nor `n` ever runs. End to end: three requests, one cancelled worker.
#[test]
fn the_named_and_the_broadcast_spellings_stay_apart_item17() {
    let _registry = registry_gate();
    let ledger = CancellationLedger::plant();
    let broadcast = crate::orchestrator::global_cancellation_token();
    let (alpha_agent, alpha_task) = ITEM17_ROWS[0];
    let alpha = item17_key(alpha_agent, alpha_task);
    let (gamma_agent, gamma_task) = ITEM17_ROWS[2];
    let gamma = item17_key(gamma_agent, gamma_task);

    // `/cancel <task id>` → the named worker, and `y` cancels that one only.
    let mut r = item17_renderer();
    type_text(&mut r, &format!("/cancel {gamma_task}"));
    key(&mut r, KeyCode::Enter);
    assert_eq!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker {
            key: gamma.clone(),
            task_id: Some(gamma_task.to_string()),
        }),
        "the argument resolves against the roster"
    );
    assert!(
        drain_submissions(&mut r).is_empty(),
        "and nothing was sent while it waits on the surface"
    );
    let rows = on_screen(&frame_text(&mut r, 100, 30));
    assert!(rows.contains(&format!("Cancel worker '{gamma}'")), "{rows}");
    assert!(!rows.contains("Cancels every running worker"), "{rows}");
    assert!(key(&mut r, KeyCode::Char('y')));
    assert_eq!(
        ledger.cancelled(),
        vec![gamma.clone()],
        "the named spelling cancels the named row only"
    );
    assert_eq!(ledger.aborted(), vec![gamma.clone()]);
    assert!(!broadcast.is_cancelled(), "and never reaches the broadcast");
    assert!(
        drain_submissions(&mut r).is_empty(),
        "a confirmed worker cancel is never handed to the loop as a command"
    );

    // `/cancel <whole key>` names the same single worker. `Esc` drops it, and a
    // dropped request is not a cancellation.
    let mut r = item17_renderer();
    type_text(&mut r, &format!("/cancel {alpha}"));
    key(&mut r, KeyCode::Enter);
    assert_eq!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker {
            key: alpha.clone(),
            task_id: Some(alpha_task.to_string()),
        }),
        "the whole-key spelling resolves to the same one row"
    );
    assert!(key(&mut r, KeyCode::Esc));
    assert_eq!(r.confirm_action, None, "Esc drops the request");
    assert!(!r.is_abort_armed(), "and arms nothing");
    assert!(
        r.status_line
            .contains(&format!("Confirm cancel of worker '{alpha}'")),
        "the status line states the pending request: {:?}",
        r.status_line
    );
    assert!(drain_submissions(&mut r).is_empty(), "and nothing was sent");
    assert_eq!(
        ledger.cancelled(),
        vec![gamma.clone()],
        "the dropped request cancelled nothing"
    );

    // `/cancel-all` is the broadcast request — a different request, not the named
    // one wearing another label — and it too runs only on `y`.
    let mut r = item17_renderer();
    type_text(&mut r, "/cancel-all");
    key(&mut r, KeyCode::Enter);
    assert_eq!(r.confirm_action, Some(events::ConfirmAction::CancelAll));
    assert_ne!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker {
            key: gamma.clone(),
            task_id: Some(gamma_task.to_string()),
        }),
        "the broadcast is not the named request re-labelled"
    );
    let rows = on_screen(&frame_text(&mut r, 100, 30));
    assert!(rows.contains("Cancel every worker (/cancel-all)"), "{rows}");
    assert!(
        drain_submissions(&mut r).is_empty(),
        "even the broadcast waits for `y`"
    );
    assert!(key(&mut r, KeyCode::Esc));
    assert_eq!(r.confirm_action, None, "Esc drops the broadcast");
    assert!(!r.is_abort_armed(), "and arms nothing");
    assert!(
        r.status_line
            .contains("Confirm cancel of every worker — y to cancel them"),
        "the status line states the pending request: {:?}",
        r.status_line
    );
    assert!(drain_submissions(&mut r).is_empty(), "and nothing was sent");
    assert!(
        !broadcast.is_cancelled(),
        "Esc on the broadcast cancels nothing"
    );

    // `n` is the explicit refusal, and it says so on the status line.
    let mut r = item17_renderer();
    type_text(&mut r, "/cancel-all");
    key(&mut r, KeyCode::Enter);
    assert!(key(&mut r, KeyCode::Char('n')));
    assert_eq!(r.status_line, "Every worker was left running.");
    assert!(!r.user_exit_requested());

    // The whole run, end to end: three requests, one confirmed single-worker
    // cancellation, and the two rows no request named are still running.
    assert_eq!(ledger.cancelled(), vec![gamma.clone()]);
    assert_eq!(ledger.aborted(), vec![gamma]);
    assert_eq!(
        ledger.untouched().len(),
        2,
        "a named cancellation never swept the roster"
    );
    assert!(
        !broadcast.is_cancelled(),
        "the broadcast was never run from a keystroke"
    );
}

// ===========================================================================
// §10 item 18 — capture_ui → restore_panes round-trips the view state, D2's
// queue/paused pair, and states a witness disagreement in the transcript.
// ===========================================================================

/// Item 18: the durable round trip, read back off a redrawn frame. The mode, the
/// focused pane — the composer, which is the one pane the pre-rebuild spellings
/// had no name for — and the follow state come back as pane state, D2's
/// queue/paused pair comes back as row state, and the focus marker is on the
/// composer in the repaint, not on the body pane the legacy `Chat` tag named.
#[test]
fn capture_and_restore_repaints_mode_focus_follow_and_the_d2_pair_item18() {
    use crate::ui::Renderer;

    // The view the operator left: M4, composer focused, follow off.
    let mut source = m4_renderer();
    source.set_view_mode(state::ViewMode::Split);
    source.set_focus(state::FocusPane::Input);
    source.set_follow(false);
    let section = source.capture_ui();
    assert_eq!(
        section.structural_violations(),
        Vec::<String>::new(),
        "a captured view state must be a loadable one"
    );
    // D2 (spec §9.4): the bundle states the view directly — the mode token, the
    // composer token and an explicit off — so there is nothing left to fold.
    assert_eq!(
        section.view.view_mode.as_deref(),
        Some("split"),
        "the mode is stored as a mode"
    );
    assert_eq!(
        section.view.focused_panel, "input",
        "the composer has its own token"
    );
    assert_eq!(
        section.view.follow,
        Some(false),
        "follow off is recorded, never defaulted"
    );

    let mut restored = TuiRenderer::new();
    restored.restore_panes(&section);

    // The view state, read back as stated rather than re-derived from a pane
    // toggle, and moved through the same transition helpers the keys use.
    assert_eq!(restored.view_mode, state::ViewMode::Split);
    assert_eq!(
        restored.focus,
        state::FocusPane::Input,
        "the composer focus survives the round trip"
    );
    assert!(!restored.follow, "follow off survives the round trip");
    assert!(
        !restored.chat_auto_scroll,
        "the pre-rebuild scroll mirror moves with the flag"
    );
    assert_eq!(
        restored.focused_panel,
        FocusedPanel::Chat,
        "the legacy mirror keeps its own three-variant spelling"
    );
    let view = state::UiView::from_renderer(&restored);
    assert_eq!(view.mode, state::ViewMode::Split);
    assert_eq!(view.focus, state::FocusPane::Input);
    assert!(!view.follow);
    // And nothing is lost a second time: the restored renderer re-captures the
    // very section it was handed.
    assert_eq!(restored.capture_ui(), section);

    // D2: the queue rank and the arbitration pause are durable row state.
    let queued = restored
        .subagents
        .iter()
        .find(|row| row.name == "researcher_specialist-t-009")
        .expect("the queued row is back");
    assert_eq!(
        queued.queue,
        Some(QueueState { position: 2, of: 3 }),
        "the queue pair round-trips"
    );
    let paused = restored
        .subagents
        .iter()
        .find(|row| row.name == "validator_specialist-t-003")
        .expect("the paused row is back");
    assert!(paused.paused, "the held stream is still marked held");

    // And it is all on the redrawn frame: mode chip, roster rank, pause glyph,
    // the transcript's follow-off chip, and the focus badge over the composer.
    let (text, styles, rects) = draw_styled(&mut restored, 100, 30);
    let screen = on_screen(&text);
    assert!(screen.contains("M4 Split"), "mode chip: {screen}");
    assert!(
        screen.contains("plan:on") && screen.contains("workers:on"),
        "chips: {screen}"
    );
    assert!(
        screen.contains("queued 2/3 researcher_specialist"),
        "the restored queue rank is drawn on the row: {screen}"
    );
    assert!(
        screen.contains("⏸ paused validator_specialist"),
        "the restored pause is drawn on the row: {screen}"
    );
    assert!(
        screen.contains("↓ follow off"),
        "the follow state reads back on the transcript border: {screen}"
    );

    // The focus marker, which is the half D2 fixed (spec §4.2, item 12 and item
    // 18): exactly one pane is badgeable — follow is off, so the transcript's
    // `● LIVE` chip is not on the frame — and it is the composer. Its title
    // carries its own hint text, so its badge rides the end of that title row
    // rather than sitting after a pane label.
    assert_eq!(
        screen.matches(theme::FOCUS_BADGE).count(),
        1,
        "exactly one pane carries the focus badge:\n{screen}"
    );
    assert!(
        text.iter()
            .any(|row| row.contains("Input (") && row.contains(theme::FOCUS_BADGE)),
        "the restored focus badge sits in the composer's own title: {screen}"
    );
    let composer = rects.input.expect("the composer is drawn in every mode");
    assert_eq!(
        corner_colour(&styles, composer),
        Some(theme::frame_focus().fg.unwrap()),
        "the restore repaints the composer's frame in `frame_focus`"
    );
    for (label, pane) in [
        ("transcript", rects.transcript),
        ("plan", rects.plan),
        ("workers", rects.workers),
    ] {
        let rect = pane.expect("M4 draws every body pane at 100x30");
        assert_eq!(
            corner_colour(&styles, rect),
            Some(theme::frame_idle().fg.unwrap()),
            "{label} is not the restored focus, so its frame stays `frame_idle`"
        );
    }
}

/// Item 18, the backwards-compatibility half: a `ui.json` written before D2 holds
/// no `view_mode`, no `follow`, and no composer token. It must still load — the
/// loader refuses, it never repairs, and a field the writer never had is not a
/// violation — and it must restore to a sane view derived from the fields it
/// **does** carry: the mode from the pane toggles, the focus from the body-pane
/// token, follow from its auto-scroll mirror.
#[test]
fn a_pre_d2_view_state_restores_from_the_fields_it_actually_has_item18() {
    use crate::ui::Renderer;

    // A bundle exactly as a pre-D2 release wrote it: take a real capture and take
    // back out the three things D2 added.
    let mut source = m4_renderer();
    source.set_focus(state::FocusPane::Plan);
    source.set_follow(false);
    let mut value = serde_json::to_value(source.capture_ui()).expect("a capture serialises");
    {
        let view = value["view"].as_object_mut().expect("a view object");
        for field in ["view_mode", "follow"] {
            assert!(
                view.remove(field).is_some(),
                "the capture must really carry {field} before it is stripped"
            );
        }
        // The composer token is D2's too: an older bundle can only name the body
        // pane, and that older spelling has to stay legal.
        view.insert("focused_panel".to_string(), serde_json::json!("chat"));
    }
    let older: crate::session::format::UiSection =
        serde_json::from_value(value).expect("a pre-D2 bundle must still load");
    assert_eq!(
        older.view.view_mode, None,
        "no mode was recorded, none is claimed"
    );
    assert_eq!(
        older.view.follow, None,
        "no follow flag was recorded, none is claimed"
    );
    assert_eq!(
        older.structural_violations(),
        Vec::<String>::new(),
        "an older bundle must not become a refusal because it predates a field"
    );

    let mut restored = TuiRenderer::new();
    restored.restore_panes(&older);

    // Sane defaults, each derived from a field the older bundle has, never
    // invented: M4 from its toggles, the body pane from the `chat` token, and the
    // follow flag from the auto-scroll mirror.
    assert_eq!(
        restored.view_mode,
        state::ViewMode::Split,
        "the toggles still name the mode an older bundle had"
    );
    assert_eq!(
        restored.focus,
        state::FocusPane::Transcript,
        "`chat` is read as the body pane it always named"
    );
    assert!(
        !restored.follow,
        "the auto-scroll mirror is the older spelling of follow off"
    );
    assert_eq!(restored.focused_panel, FocusedPanel::Chat);
    assert!(!restored.chat_auto_scroll);

    // And the frame says the same thing: the mode chip, the follow-off chip and
    // one focus badge, on the transcript, where the old `Chat` focus sat.
    let (text, styles, rects) = draw_styled(&mut restored, 100, 30);
    let screen = on_screen(&text);
    assert!(screen.contains("M4 Split"), "mode chip: {screen}");
    assert!(screen.contains("↓ follow off"), "follow chip: {screen}");
    assert_eq!(
        screen.matches(theme::FOCUS_BADGE).count(),
        1,
        "one focus badge, and it is the restored one:\n{screen}"
    );
    assert!(
        screen.contains(&format!(
            "{} {}",
            state::FocusPane::Transcript.label(),
            theme::FOCUS_BADGE
        )),
        "the transcript carries the badge the `chat` token restores: {screen}"
    );
    let transcript = rects.transcript.expect("M4 draws the transcript at 100x30");
    assert_eq!(
        corner_colour(&styles, transcript),
        Some(theme::frame_focus().fg.unwrap()),
        "and its frame is repainted in `frame_focus`"
    );
}

/// Item 18, the witness half: a stored token counter that disagrees with this
/// process is reported in the transcript as one line per disagreeing counter, and
/// never adopted silently.
#[test]
fn a_disagreeing_token_witness_arrives_as_one_transcript_line_item18() {
    use crate::ui::Renderer;

    let mut source = m4_renderer();
    source.tokens_in = 900;
    let section = source.capture_ui();

    let mut restored = TuiRenderer::new();
    restored.restore_panes(&section);

    let warnings = restored.token_witness_warnings(&section);
    assert_eq!(
        warnings.len(),
        1,
        "one sentence per disagreeing counter: {warnings:?}"
    );
    assert!(warnings[0].contains("tokens_in"), "{warnings:?}");

    let lines: Vec<&String> = restored
        .messages
        .iter()
        .filter(|line| line.starts_with("[CLI] Restore warning:"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "the warning appears as one transcript line: {:?}",
        restored.messages
    );
    assert!(lines[0].contains("tokens_in witness 900"), "{lines:?}");
    assert_eq!(
        restored.tokens_in, 0,
        "the live counter is kept, never adopted"
    );

    // And it is on screen, not just in the vector: the transcript pane says it.
    // In M4 that pane is the side column, whose width wraps the
    // sentence, so the pane's own body rows are folded back into one line.
    let (rows, _, rects) = draw_frame(&mut restored, 100, 30);
    let transcript = rects.transcript.expect("M4 draws the transcript at 100x30");
    let said = pane_says(&rows, transcript);
    assert!(
        said.contains("[CLI] Restore warning: tokens_in witness 900"),
        "the transcript states the disagreement:\n{}",
        on_screen(&rows)
    );
}

// ===========================================================================
// §10 item 19 — layout: no zero-sized pane rect at the supported sizes, the
// too-small notice below the floor, and no drawn line outside its rect.
// ===========================================================================

/// Item 19: at 100x30, 90x24, 80x24, 60x16 and 40x10 every pane that is drawn has
/// a non-zero rect, panes never overlap, the body panes fit inside the body, and
/// no rendered cell falls outside every pane rect.
#[test]
fn no_pane_is_zero_sized_overlapping_or_outside_its_rect_item19() {
    for (width, height) in [(100u16, 30u16), (90, 24), (80, 24), (60, 16), (40, 10)] {
        for mode in [
            state::ViewMode::Transcript,
            state::ViewMode::Plan,
            state::ViewMode::Workers,
            state::ViewMode::Split,
        ] {
            let mut r = m4_renderer();
            r.set_view_mode(mode);
            let (rows, grid, rects) = draw_frame(&mut r, width, height);
            let panes = leaf_rects(&rects);
            assert!(
                !panes.is_empty(),
                "{width}x{height} {mode:?}: a supported size draws something"
            );
            for (label, rect) in &panes {
                assert!(
                    rect.width > 0 && rect.height > 0,
                    "{width}x{height} {mode:?}: pane {label} is zero-sized: {rect:?}"
                );
                assert!(
                    usize::from(rect.x) + usize::from(rect.width) <= usize::from(width)
                        && usize::from(rect.y) + usize::from(rect.height) <= usize::from(height),
                    "{width}x{height} {mode:?}: pane {label} leaves the screen: {rect:?}"
                );
            }
            for (index, (first, a)) in panes.iter().enumerate() {
                for (second, b) in panes.iter().skip(index + 1) {
                    assert!(
                        !overlaps(*a, *b),
                        "{width}x{height} {mode:?}: {first} overlaps {second}: {a:?} {b:?}"
                    );
                }
            }
            if let Some(body) = rects.body {
                for (label, rect) in [
                    ("transcript", rects.transcript),
                    ("plan", rects.plan),
                    ("workers", rects.workers),
                ] {
                    if let Some(rect) = rect {
                        assert!(
                            contains(body, rect),
                            "{width}x{height} {mode:?}: {label} leaves the body: \
                             {rect:?} vs {body:?}"
                        );
                    }
                }
            }
            let offenders = cells_outside_every_pane(&grid, &rects);
            assert!(
                offenders.is_empty(),
                "{width}x{height} {mode:?}: {} cells were drawn outside every pane rect \
                 (first offenders {offenders:?})\n{}",
                offenders.len(),
                on_screen(&rows)
            );
        }
    }
}

/// Item 19: at 39x9 — one cell below the floor — exactly one pane is drawn: the
/// too-small notice. At 40x10 the floor itself is supported.
#[test]
fn below_the_floor_only_the_too_small_notice_is_drawn_item19() {
    let mut r = m4_renderer();
    let (rows, grid, rects) = draw_frame(&mut r, 39, 9);
    assert_eq!(
        rects.notice,
        Some(Rect::new(0, 0, 39, 9)),
        "the notice owns the whole area"
    );
    for pane in [
        rects.header,
        rects.body,
        rects.status,
        rects.input,
        rects.footer,
        rects.transcript,
        rects.plan,
        rects.workers,
    ] {
        assert!(
            pane.is_none(),
            "and nothing else is split at all: {rects:?}"
        );
    }
    assert_eq!(
        rects.mode,
        state::ViewMode::Transcript,
        "the ladder overrides the mode rather than inventing geometry"
    );

    let screen = on_screen(&rows);
    assert!(
        screen.contains("Terminal too small")
            && screen.contains("40x10")
            && screen.contains("currently 39x9"),
        "the notice states the floor and the current size:\n{screen}"
    );
    for absent in ["Input", "Tokens", "Transcript", "Execution Plan", "Workers"] {
        assert!(
            !screen.contains(absent),
            "no pane chrome may survive below the floor: {absent}\n{screen}"
        );
    }
    assert!(cells_outside_every_pane(&grid, &rects).is_empty());

    // The floor itself is supported: no notice at 40x10 or 39x10-with-enough-width.
    let mut r = m4_renderer();
    let (rows, _, rects) = draw_frame(&mut r, 40, 10);
    assert!(rects.notice.is_none(), "40x10 is exactly at the floor");
    assert!(rects.header.is_some() && rects.status.is_some() && rects.input.is_some());
    let screen = on_screen(&rows);
    assert!(!screen.contains("Terminal too small"), "{screen}");
}

/// Item 19 (the §2.5 ladder read literally): which panes drop at which size, in
/// spec order — columns, then the stacked pair, then the short legend and header,
/// then the second status row and the footer, then the too-small notice.
#[test]
fn the_degradation_ladder_drops_panes_in_spec_order_item19() {
    // 100 wide: M4 is three panes — the plan in the wide column, the transcript
    // over the roster in the side column (the global slot rule).
    let mut r = m4_renderer();
    let (_, _, wide) = draw_frame(&mut r, 100, 30);
    assert!(
        wide.transcript.is_some() && wide.plan.is_some() && wide.workers.is_some(),
        "100 wide draws all three body panes: {wide:?}"
    );
    assert!(wide.footer.is_some() && wide.status.is_some());
    assert_eq!(wide.status.map(|rect| rect.height), Some(2));

    // 90 is the boundary: still columns at exactly 90, stacked at 89.
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Plan);
    let (_, _, at_90) = draw_frame(&mut r, 90, 30);
    assert!(
        at_90.transcript.is_some() && at_90.plan.is_some() && at_90.workers.is_some(),
        "90 columns: {at_90:?}"
    );
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Plan);
    let (rows_89, _, at_89) = draw_frame(&mut r, 89, 30);
    assert!(
        at_89.transcript.is_some() && at_89.plan.is_some() && at_89.workers.is_none(),
        "width < 90 stacks the focused side pane and the transcript alone: {at_89:?}"
    );
    let screen = on_screen(&rows_89);
    assert!(
        screen.contains("workers:narrow"),
        "the status row must not announce a pane the ladder removed:\n{screen}"
    );

    // 70 is the boundary for the short legend and the header's optional chips
    // (`val` → `ctx` → `elapsed` → mode name, §7.4). A real ctx numerator is needed
    // to make the chip observable at all — a zero one is never invented.
    let mut r = m4_renderer();
    r.orchestrator_context_tokens = 12_400;
    let (rows_70, _, at_70) = draw_frame(&mut r, 70, 30);
    let header_70 = rows_70[usize::from(at_70.header.expect("a header at 70").y)]
        .trim_end()
        .to_string();
    assert!(
        header_70.contains("ctx")
            && header_70.contains("elapsed")
            && header_70.contains("M4 Split"),
        "70 columns keeps the optional chips: {header_70:?}"
    );
    let mut r = m4_renderer();
    r.orchestrator_context_tokens = 12_400;
    let (rows_69, _, at_69) = draw_frame(&mut r, 69, 30);
    let header_69 = rows_69[usize::from(at_69.header.expect("a header at 69").y)]
        .trim_end()
        .to_string();
    assert!(
        !header_69.contains("ctx")
            && !header_69.contains("elapsed")
            && !header_69.contains("M4 Split"),
        "69 columns drops the whole optional band (drop order §7.4): {header_69:?}"
    );
    assert!(
        header_69.contains("Marmel")
            && header_69.contains("3/7 done")
            && header_69.contains("t-004"),
        "brand, the never-dropped counter (G1) and the task id stay: {header_69:?}"
    );
    assert!(
        at_69.footer.is_some(),
        "the footer is still there at 30 rows"
    );
    let footer = at_69.footer.expect("footer");
    let footer_row = rows_69[usize::from(footer.y)].trim_end();
    assert!(
        footer_row.contains("F1 help")
            && footer_row.contains("Esc arm")
            && footer_row.contains("Enter send"),
        "short legend at {footer_row:?}"
    );
    assert!(
        !footer_row.contains("Tab focus"),
        "the compact legend keeps only its three keys: {footer_row:?}"
    );

    // 20 rows is the boundary for the second status row and the footer.
    let mut r = m4_renderer();
    let (_, _, at_20_rows) = draw_frame(&mut r, 100, 20);
    assert_eq!(at_20_rows.status.map(|rect| rect.height), Some(2));
    assert!(at_20_rows.footer.is_some());
    let mut r = m4_renderer();
    let (rows_19, _, at_19_rows) = draw_frame(&mut r, 100, 19);
    assert_eq!(
        at_19_rows.status.map(|rect| rect.height),
        Some(1),
        "19 rows collapses the status bar to one row: {at_19_rows:?}"
    );
    assert!(at_19_rows.footer.is_none(), "and drops the footer entirely");
    let status = at_19_rows.status.expect("status");
    let status_row = rows_19[usize::from(status.y)].trim_end();
    assert!(
        status_row.contains("Tokens"),
        "the single row keeps counting tokens: {status_row:?}"
    );
    let input = at_19_rows.input.expect("input");
    assert!(
        input.height <= 3,
        "the composer clamps to (1,3) with a `+n` marker: {input:?}"
    );

    // 60 is the boundary for the worker detail half and the M2/M3 columns.
    assert!(layout::workers_detail_present(60));
    assert!(!layout::workers_detail_present(59));
    let mut r = m4_renderer();
    r.set_view_mode(state::ViewMode::Workers);
    let (rows_59, _, at_59) = draw_frame(&mut r, 59, 30);
    assert!(
        at_59.workers.is_some() && at_59.transcript.is_none(),
        "width < 60 makes M3 a single column: {at_59:?}"
    );
    let body = at_59.body.expect("a body at 59 columns");
    let workers = at_59.workers.expect("the roster is still drawn");
    assert_eq!(
        workers.width, body.width,
        "width < 60 gives the roster the whole body — no detail half: {at_59:?}"
    );
    assert!(
        !on_screen(&rows_59).contains("Nothing streamed yet"),
        "and the detail half's body is absent:\n{}",
        on_screen(&rows_59)
    );
}

/// The single global slot rule (t-024b): in **every** shape that draws both the Plan
/// and the Transcript, the Execution Plan pane owns the **leading** slot — the
/// leftmost column of a columns split, the topmost row of a stack, the wide column of
/// the fallback in which the side column cannot stack — and the Transcript pane owns
/// the **trailing** one. This supersedes the t-024 pin
/// (`the_plan_and_the_transcript_hold_each_others_slot_t024`), which traded each
/// shape's own slots and left the two modes incoherent: M2 read Transcript-left /
/// Plan-right while M4 read Plan-left / Transcript-right. The rects are pinned as
/// literals again, so no slot can drift back to the other pane silently, and the rule
/// is then swept across every shape and terminal width the §2.5 ladder knows —
/// including the narrow fallbacks — so it is one rule, not four.
///
/// The shapes that draw no Plan pane are pinned as the rule's **vacuous** cases, never
/// as exceptions to it: M3 keeps the roster in the wide left column, and where the
/// ladder keeps the roster instead of the Plan (the focused pane is never stranded)
/// there is no Plan on screen to lead.
#[test]
fn the_plan_holds_the_leading_slot_and_the_transcript_the_trailing_one_t024b() {
    // ---- M4 Split, columns: the plan owns the 60 % column, the transcript heads the
    // 40 % side column over the roster.
    let mut r = m4_renderer();
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let body = rects.body.expect("100x30 clears every floor");
    assert_eq!(body, Rect::new(0, 1, 100, 23), "the body of these frames");
    assert_eq!(
        rects.plan,
        Some(Rect::new(0, 1, 60, 23)),
        "the plan owns the leading (wide) column: {rects:?}"
    );
    assert_eq!(
        rects.transcript,
        Some(Rect::new(60, 1, 40, 12)),
        "the transcript owns the head of the side column: {rects:?}"
    );
    assert_eq!(
        rects.workers,
        Some(Rect::new(60, 13, 40, 11)),
        "the roster stacks under the transcript in the same side column: {rects:?}"
    );
    assert_eq!(
        reading_order(&rects),
        vec!["plan", "transcript", "workers"],
        "the eye walks Plan, Transcript, Workers: {rects:?}"
    );

    // The pixels agree with the geometry: the body's first row carries the plan's
    // title at the body's left edge and the transcript's title at the side column.
    let cells: Vec<char> = rows[usize::from(body.y)].chars().collect();
    let left: String = cells[..usize::from(body.width) / 2].iter().collect();
    let side: String = cells[usize::from(body.width) * 3 / 5..].iter().collect();
    assert!(
        left.starts_with("┌ Execution Plan"),
        "the plan pane titles the left column: {left:?}"
    );
    assert!(
        side.starts_with("┌ Transcript"),
        "the transcript pane titles the side column: {side:?}"
    );
    assert!(
        !left.contains("Transcript"),
        "and the old order is gone from the left column: {left:?}"
    );

    // ---- M2 Plan, columns: the plan owns the leading (wide) 55 % column and the
    // transcript the trailing 45 % one — the same reading order M4 gives its own
    // columns, which is the point of having one rule instead of a per-shape trade.
    let mut r = m4_renderer();
    r.set_view_mode(state::ViewMode::Plan);
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let body = rects.body.expect("100x30 clears every floor");
    let plan = rects.plan.expect("M2 draws the plan pane");
    let transcript = rects.transcript.expect("M2 draws the transcript pane");
    assert_eq!(
        plan,
        Rect::new(0, 1, 55, 23),
        "M2: the plan owns the wide left column: {rects:?}"
    );
    assert_eq!(
        transcript,
        Rect::new(55, 1, 45, 23),
        "M2: the transcript owns the right column: {rects:?}"
    );
    assert_eq!(
        reading_order(&rects),
        vec!["plan", "transcript"],
        "M2 reads Plan, Transcript: {rects:?}"
    );
    // The pixels agree with the geometry: the plan titles the body's own left edge in
    // M2 exactly as it does in M4, and the transcript titles the column to its right.
    let row: Vec<char> = rows[usize::from(body.y)].chars().collect();
    let m2_left: String = row[..usize::from(plan.x + plan.width)].iter().collect();
    let m2_right: String = row[usize::from(transcript.x)..].iter().collect();
    assert!(
        m2_left.starts_with("┌ Execution Plan"),
        "M2: the plan pane titles the left column: {m2_left:?}"
    );
    assert!(
        m2_right.starts_with("┌ Transcript"),
        "M2: the transcript pane titles the right column: {m2_right:?}"
    );
    assert!(
        !m2_left.contains("Transcript"),
        "M2: the transcript no longer leads the frame: {m2_left:?}"
    );

    // ---- M3 Workers draws no plan pane, so the rule has nothing to place there.
    let mut r = m4_renderer();
    r.set_view_mode(state::ViewMode::Workers);
    let (_, _, rects) = draw_frame(&mut r, 100, 30);
    assert_eq!(
        rects.workers,
        Some(Rect::new(0, 1, 55, 23)),
        "M3 keeps the roster in the wide left column: {rects:?}"
    );
    assert_eq!(
        rects.transcript,
        Some(Rect::new(55, 1, 45, 23)),
        "and the transcript in the right one: {rects:?}"
    );

    // ---- The `width < 90` stacked fallback: the same rule read vertically, the plan
    // in the top row and the transcript in the bottom one.
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Plan);
    let (_, _, rects) = draw_frame(&mut r, 89, 30);
    let body = rects.body.expect("89x30 clears the floors");
    let plan = rects
        .plan
        .expect("the focused plan pane survives the ladder");
    let transcript = rects
        .transcript
        .expect("the transcript survives in every mode");
    assert_eq!(rects.workers, None, "only the focused side pane is stacked");
    assert_eq!(
        plan,
        Rect::new(0, 1, 89, 12),
        "the plan leads the stack — the top row is its slot: {rects:?}"
    );
    assert_eq!(
        transcript,
        Rect::new(0, 13, 89, 11),
        "and the transcript takes the row under it: {rects:?}"
    );
    assert_eq!(
        (plan.x, plan.y),
        (body.x, body.y),
        "the plan heads the stack"
    );
    assert_eq!((plan.width, transcript.width), (body.width, body.width));
    assert_eq!(
        transcript.y,
        plan.y + plan.height,
        "the transcript takes the slot under the plan: {rects:?}"
    );
    assert_eq!(
        plan.height + transcript.height,
        body.height,
        "and the two fill the body: {rects:?}"
    );
    assert_eq!(
        reading_order(&rects),
        vec!["plan", "transcript"],
        "a stacked frame is read top-down, plan first: {rects:?}"
    );

    // With the roster focused the ladder keeps the roster instead of the plan, and
    // there is no plan pane on screen for the rule to place — the transcript leads.
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Workers);
    let (_, _, rects) = draw_frame(&mut r, 89, 30);
    let body = rects.body.expect("89x30 clears the floors");
    let transcript = rects.transcript.expect("the transcript survives");
    let workers = rects.workers.expect("the focused roster survives");
    assert_eq!(
        rects.plan, None,
        "the ladder removes the unfocused side pane"
    );
    assert_eq!(
        transcript,
        Rect::new(0, 1, 89, 12),
        "no plan on screen, so the transcript keeps the leading row: {rects:?}"
    );
    assert_eq!(
        workers,
        Rect::new(0, 13, 89, 11),
        "and the roster takes the trailing one: {rects:?}"
    );
    assert_eq!((transcript.x, transcript.y), (body.x, body.y));
    assert_eq!(workers.y, transcript.y + transcript.height);

    // ---- The short-terminal fallback: the side column cannot stack two panes, so
    // one side pane is dropped — and where the survivor is the plan, the rule gives it
    // the wide (leading) column and leaves the transcript the side one.
    let mut r = m4_renderer();
    let (_, _, rects) = draw_frame(&mut r, 100, 16);
    let body = rects.body.expect("100x16 clears the floors");
    assert!(
        body.height < 12,
        "this frame is the one the side column cannot stack: {body:?}"
    );
    assert_eq!(
        rects.plan,
        Some(Rect::new(0, 1, 60, 11)),
        "the plan owns the wide column here too: {rects:?}"
    );
    assert_eq!(
        rects.transcript,
        Some(Rect::new(60, 1, 40, 11)),
        "and the transcript the side column: {rects:?}"
    );
    assert_eq!(rects.workers, None, "and the roster drops out, as before");
    assert_eq!(
        reading_order(&rects),
        vec!["plan", "transcript"],
        "read left to right, the plan leads the short-terminal frame too: {rects:?}"
    );

    // Focused roster in the same frame: the roster survives instead of the plan, the
    // transcript keeps the wide column, and the rule has no plan to lead with.
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Workers);
    let (_, _, rects) = draw_frame(&mut r, 100, 16);
    assert_eq!(
        rects.transcript,
        Some(Rect::new(0, 1, 60, 11)),
        "no plan on screen, so the transcript holds the wide column: {rects:?}"
    );
    assert_eq!(
        rects.workers,
        Some(Rect::new(60, 1, 40, 11)),
        "and the focused roster the side one: {rects:?}"
    );
    assert_eq!(
        rects.plan, None,
        "the unfocused plan pane drops out, as before"
    );

    // ---- The rule as one sweep, not four shape pins: every mode, every focus, at
    // widths straddling each §2.5 ladder step — 120 wide, the 100-column preview
    // width, 95, the `width < 90` stack, the `width < 70` compact band, the
    // `width < 60` detail floor — and at heights that reach the short-terminal
    // fallback and the single-row footer band. Wherever a plan is drawn it is the
    // leading pane.
    for (w, h) in [
        (120u16, 40),
        (100, 30),
        (95, 30),
        (90, 30),
        (89, 30),
        (80, 30),
        (70, 30),
        (60, 30),
        (100, 20),
        (100, 16),
        (100, 12),
    ] {
        for focus in [
            state::FocusPane::Input,
            state::FocusPane::Transcript,
            state::FocusPane::Plan,
            state::FocusPane::Workers,
        ] {
            for mode in [
                state::ViewMode::Transcript,
                state::ViewMode::Plan,
                state::ViewMode::Workers,
                state::ViewMode::Split,
            ] {
                let mut r = m4_renderer();
                r.set_view_mode(mode);
                r.set_focus(focus);
                let view = state::UiView::from_renderer(&r);
                let rects = layout::compute_layout(Rect::new(0, 0, w, h), &view);
                let label = format!("{w}x{h} {mode:?} focus {focus:?}");
                assert_plan_leads_the_transcript(&label, &rects);
            }
        }

        // One rule means one reading order: at the same size the mode that isolates
        // the plan and the mode that splits the frame lead with the same pane, at the
        // same edge of the body, so no `F2` press carries the plan across the frame.
        let mut sides: Vec<(&'static str, u16, u16, &'static str)> = Vec::new();
        for mode in [state::ViewMode::Plan, state::ViewMode::Split] {
            let mut r = m4_renderer();
            r.set_view_mode(mode);
            let view = state::UiView::from_renderer(&r);
            let rects = layout::compute_layout(Rect::new(0, 0, w, h), &view);
            if let Some(plan) = rects.plan {
                let body = rects.body.expect("a frame that draws a plan has a body");
                sides.push((mode.label(), plan.x, body.x, reading_order(&rects)[0]));
            }
        }
        assert!(
            sides
                .iter()
                .all(|(_, x, body_x, head)| *x == *body_x && *head == "plan"),
            "{w}x{h}: the plan leads from the body's own edge in both plan-drawing \
             modes: {sides:?}"
        );
        assert_eq!(
            sides.len(),
            2,
            "{w}x{h}: both plan-drawing modes draw the plan pane: {sides:?}"
        );
        let (m2, m4) = (sides[0].1, sides[1].1);
        assert_eq!(
            m2, m4,
            "{w}x{h}: the plan sits at the same edge of the frame in M2 and in M4 — \
             it never jumps sides between modes: {sides:?}"
        );
    }
}

/// The `F2` walk is untouched by the slot rule of t-024b — it still steps
/// M4 → M1 → M2 → M3 → M4 — and the rule is what makes the walk read forward in every
/// shape: the pane holding the **leading** slot (the left column of a columns split,
/// the top row of a stack, the whole body in M1) is the Transcript in M1, the Plan in
/// both modes that draw a Plan (M2 and M4), and the roster in M3. The per-shape trade
/// of t-024 made that walk incoherent — it led with the transcript in M2 and the plan
/// in M4, so the plan crossed the frame under the operator's eye. One rule removes
/// that, and this pin asserts the walk, the leading pane of each stop, and the fact
/// that the two plan-drawing modes lead identically.
#[test]
fn the_f2_mode_walk_still_steps_forward_under_the_global_slot_rule_t024b() {
    let mut r = m4_renderer();
    let split_order = {
        let (_, _, rects) = draw_frame(&mut r, 100, 30);
        assert_eq!(rects.mode, state::ViewMode::Split);
        reading_order(&rects)
    };
    assert_eq!(
        split_order,
        vec!["plan", "transcript", "workers"],
        "M4 lays its panes out plan-first, then the transcript, then the roster"
    );

    let mut modes: Vec<state::ViewMode> = Vec::new();
    let mut leading: Vec<&'static str> = Vec::new();
    for _ in 0..4 {
        assert!(key(&mut r, KeyCode::F(2)));
        let (_, _, rects) = draw_frame(&mut r, 100, 30);
        assert_plan_leads_the_transcript(&format!("F2 walked to {:?}", rects.mode), &rects);
        modes.push(rects.mode);
        leading.push(reading_order(&rects)[0]);
    }
    assert_eq!(
        modes,
        vec![
            state::ViewMode::Transcript,
            state::ViewMode::Plan,
            state::ViewMode::Workers,
            state::ViewMode::Split,
        ],
        "`F2` still walks M1 → M2 → M3 → M4 from M4"
    );
    assert_eq!(
        leading,
        vec!["transcript", "plan", "workers", "plan"],
        "the leading slot walks Transcript → Plan → Workers → Plan: {leading:?}"
    );

    // The invariant the global rule is for: the two modes that draw a plan lead with
    // the same pane, at the same edge of the body, so the plan never jumps sides as
    // the walk (or `F4`/`F5`, or a pane toggle) moves between them.
    let mut leads: Vec<(&'static str, u16, u16, &'static str)> = Vec::new();
    for mode in [state::ViewMode::Plan, state::ViewMode::Split] {
        let mut r = m4_renderer();
        r.set_view_mode(mode);
        let (_, _, rects) = draw_frame(&mut r, 100, 30);
        let body = rects.body.expect("100x30 clears every floor");
        let plan = rects.plan.expect("both of these modes draw the plan pane");
        leads.push((mode.label(), plan.x, body.x, reading_order(&rects)[0]));
    }
    assert_eq!(
        leads,
        vec![("M2 Plan", 0, 0, "plan"), ("M4 Split", 0, 0, "plan")],
        "M2 and M4 both lead with the plan at the body's own left edge: {leads:?}"
    );
}

/// Item 19 (§2.4.1): a pane the mode or the ladder removes is not drawn at all —
/// never a zero-sized rect, never a title, never a chip that lies about it.
#[test]
fn an_absent_pane_is_never_split_never_drawn_and_never_announced() {
    for mode in [
        state::ViewMode::Transcript,
        state::ViewMode::Plan,
        state::ViewMode::Workers,
        state::ViewMode::Split,
    ] {
        let mut r = m4_renderer();
        r.set_view_mode(mode);
        let (rows, _, rects) = draw_frame(&mut r, 100, 30);
        let screen = on_screen(&rows);
        if mode.plan_pane() {
            assert!(rects.plan.is_some() && screen.contains("Execution Plan"));
        } else {
            assert!(
                rects.plan.is_none(),
                "{mode:?} must not split a plan pane: {rects:?}"
            );
            assert!(
                !screen.contains("Execution Plan"),
                "{mode:?} draws no plan title"
            );
        }
        if mode.workers_pane() {
            assert!(rects.workers.is_some() && screen.contains("Workers"));
        } else {
            assert!(
                rects.workers.is_none(),
                "{mode:?} must not split a workers pane: {rects:?}"
            );
        }
        assert!(
            rects.transcript.is_some(),
            "the transcript is in every mode"
        );
    }
}

// ===========================================================================
// §4 view modes and §4.2 focus.
// ===========================================================================

/// Every mode draws its own pane inventory and says which one it is, switching
/// with the mapped keys `F2`–`F5`.
#[test]
fn every_view_mode_shows_its_own_pane_inventory() {
    let mut r = m4_renderer();

    // `F3` isolates the transcript.
    assert!(key(&mut r, KeyCode::F(3)));
    assert_eq!(r.view_mode, state::ViewMode::Transcript);
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    assert!(rects.plan.is_none() && rects.workers.is_none());
    assert!(
        on_screen(&rows).contains("M1 Transcript"),
        "the header names the mode: {rows:?}"
    );

    // `F4` adds the plan pane: the plan keeps the leading (wide) 55 % column and the
    // transcript the trailing 45 % one — the global slot rule, same as M4.
    assert!(key(&mut r, KeyCode::F(4)));
    assert_eq!(r.view_mode, state::ViewMode::Plan);
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let screen = on_screen(&rows);
    assert!(screen.contains("M2 Plan"));
    assert!(rects.plan.is_some() && rects.transcript.is_some() && rects.workers.is_none());
    assert!(screen.contains("Execution Plan") && screen.contains("Transcript"));
    assert!(
        rects.plan.expect("M2 draws the plan").x
            < rects.transcript.expect("M2 draws the transcript").x,
        "the plan sits left of the transcript in M2, as it does in M4: {rects:?}"
    );
    assert_plan_leads_the_transcript("F4 → M2", &rects);

    // `F5` swaps in the Workers pane.
    assert!(key(&mut r, KeyCode::F(5)));
    assert_eq!(r.view_mode, state::ViewMode::Workers);
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let screen = on_screen(&rows);
    assert!(screen.contains("M3 Workers"));
    assert!(rects.workers.is_some() && rects.plan.is_none());
    assert!(
        screen.contains("▶ running coder_specialist"),
        "the roster states each row's state: {screen}"
    );

    // `F2` cycles M3 → M4 and back round the four.
    assert!(key(&mut r, KeyCode::F(2)));
    assert_eq!(r.view_mode, state::ViewMode::Split);
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    assert!(on_screen(&rows).contains("M4 Split"));
    for mode in [
        state::ViewMode::Transcript,
        state::ViewMode::Plan,
        state::ViewMode::Workers,
        state::ViewMode::Split,
    ] {
        assert!(key(&mut r, KeyCode::F(2)));
        assert_eq!(r.view_mode, mode, "F2 walks the four modes");
    }

    // The status chips always agree with the pane inventory (§G5).
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    let screen = on_screen(&rows);
    assert!(screen.contains("plan:on") && screen.contains("workers:on"));
    key(&mut r, KeyCode::F(3));
    let screen = on_screen(&frame_text(&mut r, 100, 30));
    assert!(
        screen.contains("plan:off") && screen.contains("workers:off"),
        "the chips follow the mode: {screen}"
    );
}

/// §4.2: `Tab` walks the ring `Input → Transcript → Plan → Workers` and wraps back
/// to the composer. The focused pane is marked twice — a `●` in its title and the
/// `frame_focus` border colour — and exactly one pane carries the badge (G6).
#[test]
fn the_tab_focus_ring_walks_wraps_and_marks_the_focused_pane() {
    let mut r = m4_renderer();
    // Follow off, so the transcript's `● LIVE` chip cannot be mistaken for a second
    // focus badge: the badge count in a frame is then exactly the number of focused
    // panes.
    r.set_follow(false);
    r.set_focus(state::FocusPane::Input);

    let ring = [
        state::FocusPane::Transcript,
        state::FocusPane::Plan,
        state::FocusPane::Workers,
        state::FocusPane::Input,
    ];
    for expected in ring {
        assert!(key(&mut r, KeyCode::Tab));
        assert_eq!(r.focus, expected, "Tab walks the §4.2 ring");

        let (rows, styles, rects) = draw_styled(&mut r, 100, 30);
        let screen = on_screen(&rows);
        assert_eq!(
            screen.matches(theme::FOCUS_BADGE).count(),
            1,
            "exactly one pane is badgeable, and it is the focused one:\n{screen}"
        );
        assert!(
            match expected {
                // The composer's title carries its own hint text, so its badge
                // rides at the end of that title row rather than after the label.
                state::FocusPane::Input => rows
                    .iter()
                    .any(|row| row.contains("Input (") && row.contains(theme::FOCUS_BADGE)),
                other => screen.contains(&format!("{} {}", other.label(), theme::FOCUS_BADGE)),
            },
            "the focused pane carries the badge in its own title: {screen}"
        );
        for pane in [
            state::FocusPane::Transcript,
            state::FocusPane::Plan,
            state::FocusPane::Workers,
        ] {
            let rect = match pane {
                state::FocusPane::Transcript => rects.transcript,
                state::FocusPane::Plan => rects.plan,
                state::FocusPane::Workers => rects.workers,
                _ => None,
            }
            .expect("M4 draws every body pane at 100x30");
            let colour = corner_colour(&styles, rect);
            if pane == expected {
                assert_eq!(
                    colour,
                    Some(theme::frame_focus().fg.unwrap()),
                    "the focused pane's frame is `frame_focus`"
                );
            } else {
                assert_eq!(
                    colour,
                    Some(theme::frame_idle().fg.unwrap()),
                    "an unfocused pane's frame stays `frame_idle`"
                );
            }
        }
    }
}

/// §4.2: `Shift+Tab` walks the ring backwards, and in a mode that presents no Plan
/// or Workers pane `Tab` skips them — the ring never lands on a pane the frame does
/// not draw (§2.4.1), so a focus orphaned by a mode change is repaired.
#[test]
fn shift_tab_walks_backwards_and_the_ring_skips_panes_the_mode_removed() {
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Transcript);
    assert!(press(&mut r, KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(r.focus, state::FocusPane::Input, "Shift+Tab steps back");
    assert!(key(&mut r, KeyCode::Tab));
    assert_eq!(r.focus, state::FocusPane::Transcript);

    // M1 presents only the composer and the transcript: a focus orphaned by the
    // mode change is repaired, and the ring never lands on an absent pane.
    let mut r = m4_renderer();
    r.set_follow(false);
    r.set_focus(state::FocusPane::Workers);
    r.set_view_mode(state::ViewMode::Transcript);
    assert!(
        matches!(
            r.focus,
            state::FocusPane::Input | state::FocusPane::Transcript
        ),
        "the mode repaired a focus its pane inventory removed: {:?}",
        r.focus
    );
    r.set_focus(state::FocusPane::Input);
    for expected in [
        state::FocusPane::Transcript,
        state::FocusPane::Input,
        state::FocusPane::Transcript,
        state::FocusPane::Input,
    ] {
        assert!(key(&mut r, KeyCode::Tab));
        assert_eq!(r.focus, expected, "Tab never lands on an absent pane");
    }
    for _ in 0..6 {
        key(&mut r, KeyCode::Tab);
        assert!(
            matches!(
                r.focus,
                state::FocusPane::Input | state::FocusPane::Transcript
            ),
            "six presses still never focus a pane M1 does not draw"
        );
    }
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    assert!(rects.plan.is_none() && rects.workers.is_none());
    let screen = on_screen(&rows);
    assert_eq!(
        screen.matches(theme::FOCUS_BADGE).count(),
        1,
        "and the badge sits on a pane that is on screen:\n{screen}"
    );
}

// ===========================================================================
// §6 autocomplete in the composer: a ghost completion, `Tab` accepts it, and `Tab`
// with nothing to accept is the §4.2 focus ring instead.
// ===========================================================================

/// §6: a unique prefix shows its completion on the composer's own frame, and `Tab`
/// accepts it into the draft.
#[test]
fn the_composer_shows_a_ghost_completion_and_tab_accepts_it() {
    let mut r = m4_renderer();
    type_text(&mut r, "/tho");
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let input = rects.input.expect("the composer is drawn in every mode");
    let title_row = rows[usize::from(input.y)].trim_end();
    assert!(
        title_row.contains("/thought"),
        "the ghost completion names the command: {title_row:?}"
    );
    assert!(
        title_row.contains("Input"),
        "it rides the composer's own frame: {title_row:?}"
    );

    assert!(key(&mut r, KeyCode::Tab));
    assert_eq!(draft(&mut r), "/thought", "Tab inserts the remainder");
    assert_eq!(
        r.focus,
        state::FocusPane::Input,
        "and `Tab` was spent on the completion, not on focus"
    );
    let rows = on_screen(&frame_text(&mut r, 100, 30));
    assert!(
        rows.contains("/thought"),
        "the accepted command is on screen:\n{rows}"
    );
}

/// §6: an ambiguous prefix completes no further than the longest common prefix and
/// says how many candidates are still in play.
#[test]
fn an_ambiguous_prefix_completes_only_to_the_common_prefix() {
    let mut r = m4_renderer();
    type_text(&mut r, "/rese");
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let input = rects.input.expect("composer");
    let title_row = rows[usize::from(input.y)].trim_end();
    assert!(
        title_row.contains("/reset") && title_row.contains("more)"),
        "the row offers the common prefix and the remaining count: {title_row:?}"
    );
    assert!(
        !title_row.contains("/reset_plan"),
        "it never guesses a command the operator did not type: {title_row:?}"
    );

    assert!(key(&mut r, KeyCode::Tab));
    assert_eq!(
        draft(&mut r),
        "/reset",
        "only the shared prefix was inserted"
    );
    type_text(&mut r, " ");
    let rows = on_screen(&frame_text(&mut r, 100, 30));
    assert!(
        rows.contains("command /reset"),
        "command mode is marked on the composer frame:\n{rows}"
    );
}

/// §6 / §4.2: a token that matches nothing is said so on the composer frame, and
/// `Tab` with nothing to accept falls through to the focus ring.
#[test]
fn an_unknown_slash_token_says_so_and_tab_cycles_focus_instead() {
    let mut r = m4_renderer();
    type_text(&mut r, "/zz");
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let input = rects.input.expect("composer");
    let title_row = rows[usize::from(input.y)].trim_end();
    assert!(
        title_row.contains("/zz is not a command"),
        "the composer states the miss rather than inventing a completion: {title_row:?}"
    );

    let before = r.focus;
    assert!(key(&mut r, KeyCode::Tab));
    assert_eq!(r.focus, state::FocusPane::Transcript);
    assert_ne!(
        r.focus, before,
        "Tab cycled focus because nothing was acceptable"
    );
    assert_eq!(draft(&mut r), "/zz", "and the draft was left alone");
}

// ===========================================================================
// §8 pane states: the plan pane in its three disk states.
// ===========================================================================

/// §8.1: with no plan on disk the pane states so — in the body and on its own
/// counter line — instead of drawing a blank box.
#[test]
fn the_plan_pane_states_the_absent_plan() {
    let mut r = m4_renderer();
    r.plan_content = "No active execution plan on disk.".to_string();
    r.set_view_mode(state::ViewMode::Plan);
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let plan = rects.plan.expect("M2 draws the plan pane");
    let body = &rows[usize::from(plan.y) + 1..usize::from(plan.y + plan.height) - 1];
    let text = on_screen(body);
    assert!(
        text.contains("No active execution plan on disk."),
        "the body states the absence:\n{text}"
    );
    let bottom = &rows[usize::from(plan.y + plan.height) - 1];
    assert!(
        bottom.contains("no tasks in plan"),
        "the counter line states it too: {bottom:?}"
    );
    let top = &rows[usize::from(plan.y)];
    assert!(
        top.contains("0/0 done"),
        "and the counter is honest: {top:?}"
    );
}

/// §7.2 / §8.1: an active plan draws its done / in-flight / pending rows with the
/// state glyphs, and its counter chip agrees with the body.
#[test]
fn the_plan_pane_draws_done_active_and_pending_rows() {
    let mut r = m4_renderer();
    r.set_view_mode(state::ViewMode::Plan);
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let plan = rects.plan.expect("M2 draws the plan pane");
    let top = &rows[usize::from(plan.y)];
    assert!(
        top.contains("3/7 done") && top.contains("▶") && top.contains("t-004"),
        "the counter chip and the in-flight task ride the top border: {top:?}"
    );
    let body = on_screen(&rows[usize::from(plan.y) + 1..usize::from(plan.y + plan.height) - 1]);
    assert!(
        body.contains("- [x] [t-001]") && body.contains("- [ ] [t-007]"),
        "checked and open rows are drawn as written:\n{body}"
    );
    let bottom = &rows[usize::from(plan.y + plan.height) - 1];
    assert!(
        bottom.contains("✓ 3 done") && bottom.contains("⧗ 4 open") && bottom.contains("in flight"),
        "the bottom line counts each state with its glyph: {bottom:?}"
    );

    // The header counter is the same split of the same string (G1).
    let header = &rows[0];
    assert!(header.contains("✓ 3/7 done"), "{header:?}");
}

/// §8.1: an archived plan is still drawn — the archive banner is body text, and the
/// done/total counter keeps counting it.
#[test]
fn the_plan_pane_shows_an_archived_plan() {
    let mut r = m4_renderer();
    r.plan_content = "[Plan completed & archived to .marmel/archive/plan-2024-06-12.md]\n\n\
        - [x] [t-001] research UX baseline\n\
        - [x] [t-002] audit TUI architecture\n"
        .to_string();
    r.set_view_mode(state::ViewMode::Plan);
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let plan = rects.plan.expect("plan pane");
    let body = on_screen(&rows[usize::from(plan.y) + 1..usize::from(plan.y + plan.height) - 1]);
    assert!(
        body.contains("[Plan completed & archived to")
            && body.contains(".marmel/archive/plan-2024-06-12.md]"),
        "the archive banner is on screen (wrapped over rows is fine):\n{body}"
    );
    let top = &rows[usize::from(plan.y)];
    assert!(
        top.contains("2/2 done"),
        "an archived plan still counts: {top:?}"
    );
    let bottom = &rows[usize::from(plan.y + plan.height) - 1];
    assert!(bottom.contains("✓ 2 done"), "{bottom:?}");
}

// ===========================================================================
// §10 item 20 — source hygiene: no mouse arm and no mouse-event binding survives
// the rebuild, and a mouse report cannot move a pixel.
// ===========================================================================

/// Item 20: `events.rs` binds no mouse event and no source arms mouse capture;
/// `DisableMouseCapture` remains exactly at its teardown sites and nowhere else.
#[test]
fn no_mouse_arm_and_no_mouse_binding_survive_in_the_ui_sources_item20() {
    let events = include_str!("events.rs");
    for forbidden in ["MouseEvent", "EnableMouseCapture", "Mouse"] {
        assert!(
            !events.contains(forbidden),
            "the key handler must not mention {forbidden} at all"
        );
    }
    assert!(
        events.contains("_ => false"),
        "an unrecognised terminal event — a mouse report among them — is ignored, \
         not guessed at"
    );

    let module = include_str!("mod.rs");
    let code = uncommented(module);
    assert!(
        !code.contains("EnableMouseCapture"),
        "nothing in mod.rs arms mouse capture; only prose may say it never does"
    );
    assert_eq!(
        code.matches("DisableMouseCapture").count(),
        2,
        "the crossterm import plus the one reset burst the two teardown paths share, \
         and nowhere else"
    );
    for file in ["render.rs", "state.rs", "layout.rs"] {
        let source = match file {
            "render.rs" => include_str!("render.rs"),
            "state.rs" => include_str!("state.rs"),
            _ => include_str!("layout.rs"),
        };
        assert!(
            !source.contains("Mouse"),
            "{file} mentions the mouse at all"
        );
    }
}

/// Strip `//` / `///` comment lines so a prose mention cannot be mistaken for code.
fn uncommented(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<&str>>()
        .join("\n")
}

/// D11: the tests run headless, so nothing they reach may write the terminal state
/// of the process running them. Two invariants keep that true, and both are pinned
/// here rather than trusted:
///
/// * the reset burst has exactly one definition in `mod.rs`, reached from
///   `io::stdout()` by exactly two call sites — `shutdown()` (behind its
///   `terminal_owned` guard, so a renderer that never called `init()` writes
///   nothing) and the panic-hook free function;
/// * no test source reaches the terminal-taking entry points at all, so every
///   `TuiRenderer` a test builds is dropped with the guard closed.
#[test]
fn no_unit_test_reaches_the_ambient_terminal_teardown_item_d11() {
    let module = uncommented(include_str!("mod.rs"));
    assert_eq!(
        module.matches("write_terminal_resets").count(),
        3,
        "one burst definition and exactly two call sites: shutdown() and the panic hook"
    );
    assert!(
        module.contains("if !self.take_terminal_resets() {"),
        "shutdown() must ask before it writes, or every test-dropped renderer paints \
         resets into the ambient stdout"
    );
    assert!(
        module.contains("std::mem::take(&mut self.terminal_owned)"),
        "the answer is exactly whether this renderer took the screen over, and it is \
         consumed so a second teardown writes nothing"
    );
    assert!(
        module.contains("self.terminal_owned = true;"),
        "init() is what arms the burst, before any mode is switched on"
    );

    // The forbidden spellings are assembled from parts so this test's own text
    // cannot be mistaken for a call site.
    let take_over = [".", "init()"].concat();
    let panic_hook = ["leave_alt_", "screen"].concat();
    let raw_mode = ["enable_", "raw_mode"].concat();
    for (file, source) in [
        ("tests.rs", include_str!("tests.rs")),
        ("tests_new_ui.rs", include_str!("tests_new_ui.rs")),
    ] {
        let code = uncommented(source);
        for forbidden in [&take_over, &panic_hook, &raw_mode] {
            assert!(
                !code.contains(forbidden.as_str()),
                "{file} must never reach {forbidden}: the terminal of the process \
                 running the tests is not the renderer's to switch or reset"
            );
        }
    }
}

/// Item 20, frame level: a mouse report is not a binding. It changes no state, is
/// reported as unhandled, and leaves the frame cell-for-cell identical.
#[test]
fn a_mouse_report_changes_nothing_on_screen_item20() {
    let mut r = m4_renderer();
    let before = snapshot(&frame_text(&mut r, 100, 30));
    for kind in [
        MouseEventKind::Down(crossterm::event::MouseButton::Left),
        MouseEventKind::Up(crossterm::event::MouseButton::Left),
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollUp,
        MouseEventKind::Drag(crossterm::event::MouseButton::Left),
    ] {
        let report = crossterm::event::Event::Mouse(MouseEvent {
            kind,
            column: 12,
            row: 6,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            !r.apply_terminal_event(&report),
            "a mouse report is ignored rather than guessed at ({kind:?})"
        );
    }
    assert_eq!(snapshot(&frame_text(&mut r, 100, 30)), before);

    // A paste, by contrast, always reaches the composer (§4.4) — proof the frame
    // would have moved had the event meant something.
    assert!(r.apply_terminal_event(&crossterm::event::Event::Paste("x".to_string())));
    assert_eq!(draft(&mut r), "x");
    assert_ne!(snapshot(&frame_text(&mut r, 100, 30)), before);
}

// ===========================================================================
// §10 item 21 — the §0 cadence constants are unchanged, and an idle renderer
// performs no terminal.draw.
// ===========================================================================

/// Item 21: the five cadence constants of §0 — 30 ms poll, 25 ms dirty repaint,
/// 125 ms spinner repaint, 3 s abort window, 250 ms plan-file poll — are still the
/// numbers the code uses.
#[test]
fn the_cadence_constants_are_the_unchanged_spec_ones_item21() {
    let events = include_str!("events.rs");
    let module = include_str!("mod.rs");
    let render = include_str!("render.rs");

    assert!(
        events.contains("event::poll(Duration::from_millis(30))"),
        "the blocking poll budget is still 30 ms"
    );
    assert!(
        module.contains("self.last_render.elapsed() >= Duration::from_millis(25)"),
        "the dirty repaint gate is still 25 ms"
    );
    assert!(
        module
            .contains("active_spinner && self.last_render.elapsed() >= Duration::from_millis(125)"),
        "the spinner repaint gate is still 125 ms"
    );
    assert!(
        module.contains("t.elapsed() < Duration::from_secs(3)"),
        "the abort window is still 3 s"
    );
    assert!(
        render.contains("Duration::from_millis(250)"),
        "the plan-file poll is still 250 ms"
    );
    assert_eq!(
        events.matches("event::poll(Duration::").count(),
        2,
        "the two poll budgets (§0 30 ms blocking, ZERO non-blocking) are the only ones"
    );
}

/// Item 21: with nothing dirty, no live worker and no activity keyword in the
/// status line, one pass through the entry path performs no `terminal.draw` at all
/// (ARCH §2, 0 % CPU idle).
#[test]
fn an_idle_clean_renderer_performs_no_terminal_draw_item21() {
    let mut r = TuiRenderer::new();
    r.dirty = false;
    r.status_line = "Ready".to_string();
    assert!(
        !r.subagents.iter().any(|row| row.is_active),
        "the fixture is idle"
    );
    assert!(
        !ACTIVITY_STATUS_KEYWORDS
            .iter()
            .any(|keyword| r.status_line.contains(keyword)),
        "and the status line names no activity"
    );

    let painted = r.frame_counter;
    let last_paint = r.last_render;
    for _ in 0..8 {
        r.drain_and_throttle_flush(false);
    }
    assert_eq!(
        r.frame_counter, painted,
        "an idle renderer must not repaint"
    );
    assert_eq!(
        r.last_render, last_paint,
        "`last_render` is only moved by a real paint, and it stayed put"
    );
    assert!(
        !r.dirty,
        "the idle pass left nothing dirty for the next one"
    );

    // The structure behind the rule: every repaint in the entry path is behind a
    // gate, so there is no unconditional draw on the idle branch.
    let module = include_str!("mod.rs");
    let body = fn_body(
        module,
        "fn drain_and_throttle_flush(&mut self, blocking: bool)",
    );
    assert_eq!(
        body.matches("self.flush()").count(),
        2,
        "the dirty repaint and the spinner repaint, both guarded: {body}"
    );
    assert!(
        body.contains("if self.dirty {")
            && body.contains("active_spinner && self.last_render.elapsed()"),
        "the two repaints are the guarded ones, never an idle-branch draw: {body}"
    );
    assert!(
        !body.contains("self.force_flush()") || body.contains("if handled {"),
        "a forced repaint happens only for a handled event: {body}"
    );
}

/// The body of one function in a source file, braces balanced from its signature.
/// Used by the source-hygiene pins, which must read what the handler *does* rather
/// than what its doc comment promises.
fn fn_body(source: &str, signature: &str) -> String {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("no {signature:?} in this source"));
    let open = source[start..]
        .find('{')
        .unwrap_or_else(|| panic!("{signature:?} has no body"));
    let mut depth = 0usize;
    let mut end = open;
    for (offset, ch) in source[start + open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = open + offset + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    source[start..start + end].to_string()
}

// ===========================================================================
// §10 item 14 / audit defect D3 — an instruction never re-arms transcript
// follow: the operator's scroll-away-from-the-tail survives a submit, while
// follow **on** keeps pinning new output to the bottom and the `↓ N new` /
// `● LIVE` chip keeps saying what it says today.
// ===========================================================================

/// A conversation with real scrollback, M1 with the transcript focused: the
/// fixture these pins need, where a viewport that jumped is visible in pixels.
fn scrollback_renderer() -> TuiRenderer {
    let mut r = TuiRenderer::new();
    r.messages = (0..40)
        .map(|i| format!("line {i:02} of a long answer"))
        .collect();
    r.set_view_mode(state::ViewMode::Transcript);
    r.set_focus(state::FocusPane::Transcript);
    r
}

/// The body rows of the transcript pane: its two border rows carry the pane title
/// and the G7 chip row, which these pins compare separately.
fn transcript_body(rows: &[String], rect: Rect) -> Vec<String> {
    let start = (usize::from(rect.y) + 1).min(rows.len());
    let end = (usize::from(rect.y + rect.height) - 1).max(start);
    rows[start..end].to_vec()
}

/// The G7 chip row — the transcript pane's own bottom border.
fn transcript_chip(rows: &[String], rect: Rect) -> String {
    rows[usize::from(rect.y + rect.height) - 1].clone()
}

/// Item 14, the defect: scrolled away from the tail and then typed an instruction,
/// the operator used to land back at the bottom. Follow stays off, the body does
/// not move, and the chip keeps counting instead of claiming `LIVE`.
#[test]
fn a_submit_leaves_follow_off_and_the_body_exactly_where_the_operator_left_it_item14() {
    let mut r = scrollback_renderer();

    // Two `PageUp`s: the viewport comes off the tail and follow restates to off.
    assert!(key(&mut r, KeyCode::PageUp));
    assert!(key(&mut r, KeyCode::PageUp));
    assert!(
        !r.follow && !r.chat_auto_scroll,
        "the scroll gesture took the viewport off the tail"
    );
    let offset = r.chat_scroll;

    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let body_before = snapshot(&transcript_body(&rows, pane));
    let chip_before = transcript_chip(&rows, pane);
    assert!(
        on_screen(&body_before).contains("line 00"),
        "the window sits at the top of the answer:\n{}",
        on_screen(&body_before)
    );
    assert!(
        chip_before.contains("new") && !chip_before.contains("LIVE"),
        "the chip counts what is still below it: {chip_before:?}"
    );

    // Submit: the line reaches the session loop and is echoed into the transcript.
    type_text(&mut r, "keep going");
    assert!(key(&mut r, KeyCode::Enter));
    assert_eq!(
        drain_submissions(&mut r),
        vec!["keep going".to_string()],
        "the instruction still reached the session loop"
    );
    assert!(
        r.messages
            .last()
            .is_some_and(|line| line == "User: keep going"),
        "and the transcript grew: {:?}",
        r.messages.last()
    );

    // State: nobody moved the operator's decision, and the mirror is still its
    // projection rather than a flag of its own.
    assert!(
        !r.follow,
        "submitting re-armed transcript follow (item 14 / D3)"
    );
    assert!(
        !r.chat_auto_scroll,
        "and its legacy mirror was written behind `set_follow`"
    );
    assert_eq!(
        r.chat_scroll, offset,
        "the viewport offset did not move under the operator"
    );

    // Frame: the same body pixels, and a chip that still counts.
    let (rows_after, _, rects_after) = draw_frame(&mut r, 100, 30);
    let pane_after = rects_after.transcript.expect("M1 draws the transcript");
    assert_eq!(pane, pane_after, "the pane itself did not move");
    assert_eq!(
        snapshot(&transcript_body(&rows_after, pane_after)),
        body_before,
        "the submit moved the transcript window while follow was off"
    );
    let chip_after = transcript_chip(&rows_after, pane_after);
    assert!(
        chip_after.contains("new") && !chip_after.contains("LIVE"),
        "the chip must keep counting, never claim LIVE: {chip_after:?}"
    );
}

/// Item 14, other half: while follow IS on, an instruction still pins the
/// transcript to the bottom and the chip stays `● LIVE`.
#[test]
fn a_submit_while_following_still_pins_the_transcript_to_the_bottom_item14() {
    let mut r = scrollback_renderer();
    assert!(
        r.follow && r.chat_auto_scroll,
        "follow is on until the operator says otherwise"
    );

    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    assert!(
        transcript_chip(&rows, pane).contains("LIVE"),
        "a followed transcript says LIVE: {:?}",
        transcript_chip(&rows, pane)
    );
    let pinned_at = r.chat_scroll;
    assert!(
        pinned_at > 0,
        "the followed frame drew at the tail, got {pinned_at}"
    );

    type_text(&mut r, "keep going");
    assert!(key(&mut r, KeyCode::Enter));
    assert_eq!(
        drain_submissions(&mut r),
        vec!["keep going".to_string()],
        "the line was sent"
    );
    assert!(
        r.follow && r.chat_auto_scroll,
        "submitting kept follow on, mirror with it"
    );

    let (rows_after, _, rects_after) = draw_frame(&mut r, 100, 30);
    let pane_after = rects_after.transcript.expect("M1 draws the transcript");
    let body = on_screen(&transcript_body(&rows_after, pane_after));
    assert!(
        body.contains("User: keep going"),
        "the new line is on screen:\n{body}"
    );
    assert!(
        body.contains("line 39 of a long answer"),
        "and the newest answer line is still at the bottom:\n{body}"
    );
    assert!(
        !body.contains("line 00 "),
        "while the top of the answer stays above the fold:\n{body}"
    );
    assert!(
        transcript_chip(&rows_after, pane_after).contains("LIVE"),
        "the chip stayed LIVE: {:?}",
        transcript_chip(&rows_after, pane_after)
    );
    assert!(
        r.chat_scroll > pinned_at,
        "the pin moved down with the new line: {pinned_at} -> {}",
        r.chat_scroll
    );
}

/// Item 14, the way back: `Ctrl+F` and `/follow on` are the explicit actions
/// allowed to re-arm follow, and both re-pin the body to the tail.
#[test]
fn the_follow_toggle_and_the_follow_command_re_arm_follow_and_repin_the_body_item14() {
    let mut r = scrollback_renderer();
    assert!(key(&mut r, KeyCode::PageUp));
    assert!(key(&mut r, KeyCode::PageUp));
    assert!(!r.follow && !r.chat_auto_scroll, "off, away from the tail");

    // `Ctrl+F` re-arms and the frame repins.
    assert!(press(&mut r, KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert!(r.follow && r.chat_auto_scroll, "Ctrl+F re-armed follow");
    let (probe, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let body = on_screen(&transcript_body(&probe, pane));
    assert!(
        body.contains("line 39 of a long answer"),
        "the toggle re-pinned the body to the tail:\n{body}"
    );
    assert!(
        transcript_chip(&probe, pane).contains("LIVE"),
        "and the chip says LIVE: {:?}",
        transcript_chip(&probe, pane)
    );

    // Scrolling away turns it off again, and `/follow on` is the same decision for
    // an operator who never touches a chord.
    assert!(key(&mut r, KeyCode::PageUp));
    assert!(
        !r.follow && !r.chat_auto_scroll,
        "scrolling off the tail turned it off"
    );
    type_text(&mut r, "/follow on");
    assert!(key(&mut r, KeyCode::Enter));
    assert!(
        drain_submissions(&mut r).is_empty(),
        "`/follow on` was answered locally"
    );
    assert!(
        r.follow && r.chat_auto_scroll,
        "`/follow on` re-armed follow and its mirror"
    );
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let body = on_screen(&transcript_body(&rows, pane));
    assert!(
        body.contains("line 39 of a long answer"),
        "and re-pinned the body:\n{body}"
    );
    assert!(
        transcript_chip(&rows, pane).contains("LIVE"),
        "with the LIVE chip: {:?}",
        transcript_chip(&rows, pane)
    );

    // `/follow off` is the same decision spelled the other way — and the next
    // instruction does not quietly undo it.
    type_text(&mut r, "/follow off");
    assert!(key(&mut r, KeyCode::Enter));
    drain_submissions(&mut r);
    assert!(
        !r.follow && !r.chat_auto_scroll,
        "`/follow off` turned follow off"
    );
    type_text(&mut r, "and now keep going");
    assert!(key(&mut r, KeyCode::Enter));
    drain_submissions(&mut r);
    assert!(
        !r.follow,
        "the submit after `/follow off` re-armed it (item 14 / D3)"
    );
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let chip = transcript_chip(&rows, pane);
    assert!(
        !chip.contains("LIVE") && chip.contains("new"),
        "the chip stayed off and kept counting: {chip:?}"
    );
}

/// The structural half, asserted as behaviour: even a direct write of the legacy
/// mirror — exactly what the submit path used to do — cannot repin the frame,
/// because the bridge repairs the projection instead of adopting it.
#[test]
fn a_stray_write_of_the_auto_scroll_mirror_cannot_repin_the_frame_item14() {
    let mut r = scrollback_renderer();
    r.set_follow(false);
    r.chat_scroll = 2;
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let body_before = snapshot(&transcript_body(&rows, pane));
    assert!(!r.follow && !r.chat_auto_scroll);

    // The write the submit path used to make, behind `set_follow`'s back.
    r.chat_auto_scroll = true;
    let rows_after = frame_text(&mut r, 100, 30);

    assert!(
        !r.follow,
        "the legacy bridge turned a mirror write into follow (D3)"
    );
    assert!(
        !r.chat_auto_scroll,
        "and left the mirror disagreeing with follow instead of repairing it"
    );
    assert_eq!(r.chat_scroll, 2, "the offset stayed where it was put");
    assert_eq!(
        snapshot(&transcript_body(&rows_after, pane)),
        body_before,
        "the frame was repainted at a different offset"
    );
    let chip = transcript_chip(&rows_after, pane);
    assert!(!chip.contains("LIVE"), "the chip flipped to LIVE: {chip:?}");
}

// ===========================================================================
// Audit D5 — a frame must be a pure function of renderer state.
//
// The status row's token pair used to be pulled out of the process-wide llm
// counters *while the frame was being built*, so any test in the same process
// that streamed a turn changed the frame an unrelated frame pin was comparing,
// and a pass could flip on scheduling order alone. The value is now an injected
// input: the production paint path (`TuiRenderer::render_terminal`) stores one
// snapshot per frame and `draw_new_ui` reads only that, so a `TestBackend` frame
// has no ambient dependency left. The pins below are the regression guard.
// ===========================================================================

/// The rows with only the session clock removed. `elapsed …` ends the row it is on,
/// so cutting there leaves every other field — the token counters included — raw.
fn clockless(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|row| match row.find("elapsed") {
            Some(at) => row[..at].to_string(),
            None => row.clone(),
        })
        .collect()
}

/// The status row's token pair as drawn, i.e. the ` Tokens N in / N out ` field of
/// status row 1 (§2.2).
fn drawn_tokens(rows: &[String], status: Rect) -> String {
    let row = &rows[usize::from(status.y)];
    let at = row
        .find(" Tokens ")
        .expect("status row 1 always carries the token pair (§2.5)");
    let rest = &row[at..];
    let end = rest.find(" out ").expect("the pair spells `N in / N out`") + " out ".len();
    rest[..end].trim().to_string()
}

/// D5: two renderers built from one fixture draw one identical frame each, even
/// though the process-global counters moved between the two draws. This is the
/// reproduction of the defect: before the injection the two frames differed only
/// in the status row (`0 in / 0 out` against the counters another test had
/// reached), which is what made whole-frame assertions race with the rest of the
/// suite.
#[test]
fn a_frame_does_not_move_when_another_test_bumps_the_llm_token_globals_d5() {
    let mut first = m4_renderer();
    let before = clockless(&frame_text(&mut first, 100, 30));

    // Stands in for every other test in this process that streams a turn.
    crate::llm::record_tokens_in(4_812);
    crate::llm::record_tokens_out(9_217);

    let mut second = m4_renderer();
    let after = clockless(&frame_text(&mut second, 100, 30));
    assert_eq!(
        before, after,
        "the frame changed because the process-global counters did (D5)"
    );

    // Same fixture, same renderer, redrawn after the bump: still the same frame.
    let redrawn = clockless(&frame_text(&mut first, 100, 30));
    assert_eq!(
        before, redrawn,
        "a redraw of one renderer moved with the globals (D5)"
    );
}

/// D5, second half: the live figure is not lost, it is injected. The production
/// snapshot call — the one the paint path makes per frame — still carries the
/// process-wide counters into the frame, whatever value they happen to have reached
/// by the time this test runs.
#[test]
fn the_injected_snapshot_still_puts_the_live_global_counters_on_screen_d5() {
    let mut r = m4_renderer();
    crate::llm::record_tokens_in(311);
    crate::llm::record_tokens_out(723);
    r.refresh_global_token_snapshot();
    let (global_in, global_out) = (r.global_tokens_in, r.global_tokens_out);

    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let status = rects.status.expect("a 100x30 frame draws the status row");
    assert_eq!(
        drawn_tokens(&rows, status),
        format!(
            "Tokens {} in / {} out",
            components::status_bar::format_count(global_in),
            components::status_bar::format_count(global_out)
        ),
        "the status row stopped showing the snapshot the paint path injected"
    );
    assert!(
        global_in >= 311 && global_out >= 723,
        "the snapshot stopped tracking the process counters: {global_in}/{global_out}"
    );
}

/// D5 kept the merge semantics: the displayed pair is the **max** of this
/// renderer's own totals and the injected snapshot, in either direction, so the
/// counters on screen never run backwards.
#[test]
fn the_token_pair_stays_the_maximum_of_the_two_counters_either_way_d5() {
    let own_ahead = {
        let mut r = m4_renderer();
        r.tokens_in = 9_400;
        r.tokens_out = 2_300;
        r.global_tokens_in = 4_812;
        r.global_tokens_out = 9_217;
        let (rows, _, rects) = draw_frame(&mut r, 100, 30);
        drawn_tokens(&rows, rects.status.expect("status row"))
    };
    assert_eq!(
        own_ahead, "Tokens 9.4k in / 9.2k out",
        "the session counter stopped leading the snapshot"
    );

    let snapshot_ahead = {
        let mut r = m4_renderer();
        r.tokens_in = 12;
        r.tokens_out = 7;
        r.global_tokens_in = 1_200_000;
        r.global_tokens_out = 5_500;
        let (rows, _, rects) = draw_frame(&mut r, 100, 30);
        drawn_tokens(&rows, rects.status.expect("status row"))
    };
    assert_eq!(
        snapshot_ahead, "Tokens 1.2M in / 5.5k out",
        "the snapshot stopped leading the session counter"
    );
}

/// D5, the whole point: with no snapshot injected — which is the state every
/// frame pin in this file is built in — the frame shows this renderer's own
/// counters no matter what the rest of the process has streamed.
#[test]
fn an_uninjected_frame_shows_this_renderers_own_counters_only_d5() {
    crate::llm::record_tokens_in(64_000);
    crate::llm::record_tokens_out(32_000);
    let mut r = m4_renderer();
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    assert_eq!(
        drawn_tokens(&rows, rects.status.expect("status row")),
        "Tokens 0 in / 0 out",
        "the frame read ambient process state again (D5)"
    );
}

// ===========================================================================
// The scroll-conflict contract, drawn — who owns `Up` / `Down` / `PageUp` /
// `PageDown` / `Home` / `End` (audit defects 1-5, 7, 10: one predicate,
// `FocusPane::composer_has_focus`, and never the shape of the draft or the
// legacy `focused_panel` fold), and what the G7 chip may claim about the
// viewport the same frame painted (defect 9's visible half).
//
// The state-level half of each rule is already pinned inside `events.rs`
// (`up_and_down_follow_the_focus_and_never_cross_the_line`,
// `down_with_a_pane_in_focus_and_an_empty_draft_scrolls_the_pane`,
// `page_keys_are_pane_keys`, the `scroll_*_to` follow guards and
// `the_workers_arrows_walk_the_roster_window_and_leave_the_log_alone`), so every
// pin below is about the **frame**: the pixels of the pane that did or did not
// move, the pixels of the composer that did or did not change, and the chip row
// read back out of the drawn border instead of off the fields behind it.
// ===========================================================================

/// What one drawn transcript chip row states about the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowChip {
    /// `● LIVE`
    Live,
    /// `↓ N new`
    New(usize),
    /// `↓ follow off`
    FollowOff,
}

/// Read the follow chip and the `scroll a/b` readout **out of the drawn border
/// row**. Parsing the pixels rather than the fields is the point: a chip that
/// agrees with `follow` but not with the window it is drawn under is exactly the
/// defect these pins exist for.
fn read_chip_row(row: &str) -> (FollowChip, Option<(u16, u16)>) {
    let chip = if row.contains("LIVE") {
        FollowChip::Live
    } else if let Some(at) = row.find("\u{2193} ") {
        let rest = &row[at + "\u{2193} ".len()..];
        let run: String = rest.chars().take_while(char::is_ascii_digit).collect();
        match run.parse::<usize>() {
            Ok(unseen) => FollowChip::New(unseen),
            Err(_) => FollowChip::FollowOff,
        }
    } else {
        FollowChip::FollowOff
    };
    let position = row.find(" scroll ").and_then(|at| {
        let rest = &row[at + " scroll ".len()..];
        let (offset, maximum) = rest.split_once('/')?;
        let number = |part: &str| -> Option<u16> {
            let run: String = part.chars().take_while(char::is_ascii_digit).collect();
            run.parse::<u16>().ok()
        };
        Some((number(offset)?, number(maximum)?))
    });
    (chip, position)
}

/// Assert that one drawn frame tells **one** story about the transcript viewport:
/// the `scroll a/b` readout names the window the pane actually painted (the
/// first body row of that frame is body row `a`), the follow chip agrees with
/// that position, and the offset the renderer kept is the one it drew. Returns
/// the drawn offset and the drawn chip so a caller can also assert the gesture's
/// step.
///
/// `max` is the measure the first frame published: the fixtures here never grow
/// the conversation, so every later frame must publish the same one.
fn assert_frame_agrees_with_its_chip(
    label: &str,
    rows: &[String],
    pane: Rect,
    max: usize,
    r: &TuiRenderer,
) -> (usize, FollowChip) {
    let chip_row = transcript_chip(rows, pane);
    let (chip, position) = read_chip_row(&chip_row);
    let (at, measured) = position.unwrap_or_else(|| {
        panic!("{label}: the pane drew no `scroll a/b` readout in {chip_row:?}")
    });
    assert_eq!(
        usize::from(measured),
        max,
        "{label}: the readout's maximum is not the measure the pane published: {chip_row:?}"
    );
    assert_eq!(
        usize::from(r.chat_scroll),
        usize::from(at),
        "{label}: the offset the renderer kept ({}) is not the one it drew ({at})",
        r.chat_scroll
    );

    let body = transcript_body(rows, pane);
    let first = body.first().cloned().unwrap_or_default();
    assert!(
        first.contains(&format!("line {at:02}")),
        "{label}: the drawn window does not start at the row its own readout names \
         ({at}):\n{}",
        on_screen(&body)
    );
    let last = body.last().cloned().unwrap_or_default();
    let at_tail = usize::from(at) == usize::from(measured);
    match chip {
        FollowChip::Live => {
            assert!(
                r.follow && r.chat_auto_scroll,
                "{label}: `● LIVE` was drawn with follow off (mirror {})",
                r.chat_auto_scroll
            );
            assert!(
                at_tail,
                "{label}: `● LIVE` while the drawn window is short of its tail: {chip_row:?}"
            );
            assert!(
                last.contains("line 39"),
                "{label}: `● LIVE` but the newest line of the fixture is off screen: {last:?}"
            );
        }
        FollowChip::New(unseen) => {
            assert!(
                !r.follow && !r.chat_auto_scroll,
                "{label}: `↓ {unseen} new` was drawn while follow was on"
            );
            assert!(
                !at_tail,
                "{label}: `↓ {unseen} new` on a window already at its tail: {chip_row:?}"
            );
            assert_eq!(
                unseen,
                usize::from(measured) - usize::from(at),
                "{label}: the unseen count disagrees with the distance the frame drew: {chip_row:?}"
            );
            assert!(
                !last.contains("line 39"),
                "{label}: `↓ {unseen} new` but the newest line is on screen: {last:?}"
            );
        }
        FollowChip::FollowOff => {
            assert!(
                !r.follow && !r.chat_auto_scroll,
                "{label}: `↓ follow off` was drawn while follow was on"
            );
            assert!(
                at_tail,
                "{label}: `↓ follow off` while the drawn window is off its tail: {chip_row:?}"
            );
        }
    }
    (usize::from(at), chip)
}

/// Contract row (a): with the **composer** focused, `Up` / `Down` are the composer's
/// caret keys inside a multi-line draft, and belong to nothing at all when the caret
/// has nowhere to go — they never recall history (the reported conflict: the operator
/// was scrolling and the draft got replaced), they never move a drawn pane and they
/// never restate its follow chip. The multi-line case is pinned too, because it used
/// to be the draft shape, not the focus, that decided who got the key (defects 1, 2
/// and 5).
#[test]
fn arrows_behind_the_composer_leave_every_drawn_pane_where_it_stands() {
    let mut r = scrollback_renderer();
    r.set_focus(state::FocusPane::Input);
    r.history = vec!["first note".to_string(), "second note".to_string()];

    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let body = snapshot(&transcript_body(&rows, pane));
    let chip = transcript_chip(&rows, pane);
    let offset = r.chat_scroll;
    let follow = (r.follow, r.chat_auto_scroll);
    assert!(
        chip.contains("LIVE"),
        "the boot frame follows the tail: {chip:?}"
    );

    // One plain `Up`, with a two-entry history waiting behind an empty draft: the
    // press is refused. Nothing is recalled, nothing is scrolled, nothing is marked
    // for repaint — the history is reachable by the `Ctrl+Up` chord alone.
    r.dirty = false;
    assert!(
        !key(&mut r, KeyCode::Up),
        "a plain `Up` behind the composer is not claimed once it is no longer the \
         history fallback"
    );
    assert!(r.input_text.is_empty(), "the draft stayed the operator's");
    assert_eq!(
        r.history_index, None,
        "and no history walk was started behind their back"
    );
    assert!(!r.dirty, "an unclaimed press asked for no repaint");
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    assert_eq!(
        snapshot(&transcript_body(&rows, pane)),
        body,
        "a composer arrow scrolled the transcript"
    );
    assert_eq!(
        transcript_chip(&rows, pane),
        chip,
        "a composer arrow restated the follow chip"
    );
    assert_eq!(r.chat_scroll, offset, "and no viewport offset moved");
    assert_eq!(
        (r.follow, r.chat_auto_scroll),
        follow,
        "and no follow decision was stated for a gesture inside the composer"
    );

    // `Down` is refused on the same grounds: there is no walk to walk back.
    r.dirty = false;
    assert!(!key(&mut r, KeyCode::Down), "and so is `Down`");
    assert!(r.input_text.is_empty());
    assert!(r.history_index.is_none());
    assert!(!r.dirty, "again no repaint was requested");
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    assert_eq!(
        snapshot(&transcript_body(&rows, pane)),
        body,
        "a composer arrow scrolled the transcript"
    );
    assert_eq!(transcript_chip(&rows, pane), chip);
    assert_eq!(r.chat_scroll, offset);

    // The chord, from the very same focus, does walk — and still leaves every drawn
    // pane where it stands.
    r.dirty = false;
    assert!(
        press(&mut r, KeyCode::Up, KeyModifiers::CONTROL),
        "`Ctrl+Up` is the way into the history from the composer"
    );
    assert_eq!(r.input_text, "second note");
    assert_eq!(r.history_index, Some(1), "the walk is the composer's");
    assert!(
        r.dirty,
        "a press that changed the draft does ask for a repaint"
    );
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    assert_eq!(
        r.chat_scroll, offset,
        "and the chord moved no viewport offset"
    );
    assert_eq!(
        (r.follow, r.chat_auto_scroll),
        follow,
        "and stated no follow decision"
    );
    assert!(
        on_screen(&rows).contains("history 2/2"),
        "the composer names the browse it is in:\n{}",
        on_screen(&rows)
    );
    assert!(press(&mut r, KeyCode::Down, KeyModifiers::CONTROL));
    assert!(r.input_text.is_empty(), "and walks back out to the draft");
    assert!(r.history_index.is_none());

    // A multi-line draft: the arrows move the caret inside the draft, and the pane
    // viewport still does not move. A taller draft legitimately costs the body a
    // row (§2.1 `input_h`), so the baseline is re-taken for this part: the pin is
    // that the **arrows** change nothing in it.
    type_text(&mut r, "one");
    assert!(press(&mut r, KeyCode::Char('j'), KeyModifiers::CONTROL));
    type_text(&mut r, "two");
    assert_eq!(draft(&mut r), "one\ntwo");
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    let body = snapshot(&transcript_body(&rows, pane));
    let chip = transcript_chip(&rows, pane);
    let offset = r.chat_scroll;
    assert!(
        key(&mut r, KeyCode::Up),
        "the caret steps up inside the draft"
    );
    assert_eq!(r.textarea.cursor().0, 0, "and it is the caret that moved");
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    assert_eq!(
        snapshot(&transcript_body(&rows, pane)),
        body,
        "the caret case scrolled a pane"
    );
    assert_eq!(transcript_chip(&rows, pane), chip);
    assert_eq!(r.chat_scroll, offset, "and the caret case moved an offset");
    assert!(
        !key(&mut r, KeyCode::Up),
        "with the caret against the top edge of a multi-line draft the press is not \
         claimed, so no follow state is left describing a movement that never happened \
         and no history entry is swapped in"
    );
    assert_eq!(draft(&mut r), "one\ntwo", "and the draft stayed put");
    assert!(r.history_index.is_none(), "and the walk never began");
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    assert_eq!(snapshot(&transcript_body(&rows, pane)), body);
    assert_eq!(transcript_chip(&rows, pane), chip);
    assert_eq!(r.chat_scroll, offset);
}

/// The visible half of the reported conflict: the operator was trying to **scroll**
/// with `Up` while the composer held focus. The press must not recall a history
/// entry, must not move any region the frame draws, and must not schedule a repaint —
/// in a frame that draws the plan, the transcript and the roster at once.
#[test]
fn a_plain_arrow_behind_the_composer_recalls_nothing_and_repaints_nothing() {
    let mut r = m4_renderer();
    r.history = vec![
        "an old submission".to_string(),
        "a newer submission".to_string(),
    ];
    r.set_focus(state::FocusPane::Input);

    let (rows, _, rects) = draw_frame(&mut r, 120, 30);
    let panes = leaf_rects(&rects);
    for label in ["transcript", "plan", "workers", "input"] {
        assert!(
            panes.iter().any(|(name, _)| *name == label),
            "the M4 frame must draw {label} to prove it stayed put: {panes:?}"
        );
    }
    let before: Vec<(&'static str, Vec<String>)> = panes
        .iter()
        .map(|(label, rect)| (*label, snapshot(&rows_within(&rows, *rect))))
        .collect();
    let offsets = (r.chat_scroll, r.plan_scroll, r.subagent_list_scroll);
    let follow = (r.follow, r.chat_auto_scroll);

    for code in [KeyCode::Up, KeyCode::Down] {
        r.dirty = false;
        assert!(
            !key(&mut r, code),
            "a plain {code:?} behind the composer with a history waiting is not a \
             key press any more"
        );
        assert!(r.input_text.is_empty(), "the draft was not recalled over");
        assert_eq!(
            r.history_index, None,
            "and no history walk began behind the operator"
        );
        assert!(!r.dirty, "and the press asked for no repaint");
    }
    assert_eq!(
        (r.chat_scroll, r.plan_scroll, r.subagent_list_scroll),
        offsets,
        "a composer arrow moved a viewport offset"
    );
    assert_eq!(
        (r.follow, r.chat_auto_scroll),
        follow,
        "and a composer arrow stated a follow decision"
    );

    let (rows, _, after) = draw_frame(&mut r, 120, 30);
    for (label, rect) in leaf_rects(&after) {
        let earlier = before
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, cells)| cells.clone())
            .unwrap_or_default();
        assert_eq!(
            snapshot(&rows_within(&rows, rect)),
            earlier,
            "the refused composer arrow redrew {label}"
        );
    }
    // And the same frame says nothing about a browse the operator never started.
    assert!(
        !on_screen(&rows).contains("history "),
        "an unclaimed arrow drew a history readout:\n{}",
        on_screen(&rows)
    );
}

/// Contract row (b): with a **pane** focused, `Up` / `Down` are that pane's and
/// never reach the composer — not its draft, not its caret, and not a history
/// walk that is in progress. The drawn composer keeps its pixels and its
/// `history n/m` chip while the pane under it walks (defects 5 and 7). The run is
/// pinned press by press now that a plain arrow can never be a history route at all:
/// one row of window per press, and nothing at all on the composer's side.
#[test]
fn arrows_on_a_pane_leave_the_drawn_composer_and_its_history_walk_alone() {
    let mut r = scrollback_renderer();
    r.history = vec!["first note".to_string(), "line one\nline two".to_string()];

    // Start the walk from the composer, then hand the focus to the pane.
    r.set_focus(state::FocusPane::Input);
    assert!(press(&mut r, KeyCode::Up, KeyModifiers::CONTROL));
    assert_eq!(r.input_text, "line one\nline two");
    r.set_focus(state::FocusPane::Transcript);

    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let composer = rects.input.expect("M1 draws the composer");
    let composer_before = snapshot(&rows_within(&rows, composer));
    let body_before = snapshot(&transcript_body(&rows, pane));
    let draft_text = r.input_text.clone();
    let caret = r.textarea.cursor();
    let walk = r.history_index;
    assert_eq!(
        caret,
        (0, "line one".len()),
        "the recall parked the caret at the end of \
         its first line, where it cannot be turned into a scroll"
    );
    assert!(
        on_screen(&rows).contains("history 2/2"),
        "the browse is on screen while a pane holds the focus:\n{}",
        on_screen(&rows)
    );

    // A run of `Up` presses. After **every** press the pane has stepped one row and
    // the composer is pixel-identical, with its draft, its caret and its walk
    // bookkeeping untouched — the pane's arrows never reach across into the composer.
    let mut at = r.chat_scroll;
    for step in 1..=3 {
        assert!(
            key(&mut r, KeyCode::Up),
            "the pane refused the `Up` it owns at step {step}"
        );
        assert_eq!(
            r.chat_scroll,
            at.saturating_sub(1),
            "step {step} was not one row of the pane's own window"
        );
        at = r.chat_scroll;
        assert_eq!(r.input_text, draft_text, "step {step} edited the draft");
        assert_eq!(r.textarea.cursor(), caret, "step {step} moved the caret");
        assert_eq!(
            r.history_index, walk,
            "step {step} touched the walk bookkeeping"
        );
        let (rows, _, _) = draw_frame(&mut r, 100, 30);
        assert_eq!(
            snapshot(&rows_within(&rows, composer)),
            composer_before,
            "step {step} redrew the composer"
        );
        assert_ne!(
            snapshot(&transcript_body(&rows, pane)),
            body_before,
            "step {step} did not move the pane they own"
        );
        assert!(
            on_screen(&rows).contains("history 2/2"),
            "step {step} dropped the browse chip:\n{}",
            on_screen(&rows)
        );
    }
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    let window_before_downs = snapshot(&transcript_body(&rows, pane));

    // A run of `Down` presses over the same pane steps the same window back down —
    // they used to be the history's, and to be swallowed when no walk was running
    // (defect 3).
    for step in 1..=2 {
        assert!(
            key(&mut r, KeyCode::Down),
            "`Down` was swallowed at step {step} instead of stepping the pane"
        );
        assert_eq!(
            r.chat_scroll,
            at + 1,
            "step {step} of `Down` was not one row"
        );
        at = r.chat_scroll;
        assert_eq!(r.input_text, draft_text, "step {step} edited the draft");
        assert_eq!(r.textarea.cursor(), caret, "step {step} moved the caret");
        assert_eq!(r.history_index, walk, "step {step} touched the walk");
        let (rows, _, _) = draw_frame(&mut r, 100, 30);
        assert_eq!(
            snapshot(&rows_within(&rows, composer)),
            composer_before,
            "step {step} of `Down` redrew the composer"
        );
        assert!(
            on_screen(&rows).contains("history 2/2"),
            "step {step} of `Down` dropped the browse chip:\n{}",
            on_screen(&rows)
        );
    }
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    assert_ne!(
        snapshot(&transcript_body(&rows, pane)),
        window_before_downs,
        "the run of `Down` presses did not move the pane they own"
    );

    // The walk the operator started is still intact for the composer: the chord
    // picks it up and restores their own draft.
    r.set_focus(state::FocusPane::Input);
    assert!(press(&mut r, KeyCode::Down, KeyModifiers::CONTROL));
    assert!(
        r.input_text.is_empty(),
        "the draft the walk started from came back"
    );
    assert!(r.history_index.is_none());
}

/// Contract row (c), the plan half: with the **Plan** pane focused, a run of plain
/// `Up` / `Down` presses walks only the drawn plan window. The composer keeps its
/// pixels, its draft, its caret and its history bookkeeping press for press — the
/// plan's arrows are never a history recall, however many of them are pressed.
#[test]
fn plan_focus_arrows_walk_the_drawn_plan_window_and_leave_the_composer_alone() {
    let mut r = m4_renderer();
    r.plan_content = (1..=30)
        .map(|index| format!("- [ ] [t-{index:03}] plan row {index}"))
        .collect::<Vec<String>>()
        .join("\n");
    r.history = vec![
        "an old submission".to_string(),
        "a newer submission".to_string(),
    ];
    r.set_input_content("a draft with words");
    r.set_focus(state::FocusPane::Plan);

    let (rows, _, rects) = draw_frame(&mut r, 120, 30);
    let pane = rects.plan.expect("M4 Split draws the Plan pane");
    let composer = rects.input.expect("every mode draws the composer");
    let composer_before = snapshot(&rows_within(&rows, composer));
    let draft_text = draft(&mut r);
    let caret = r.textarea.cursor();
    let walk = r.history_index;
    assert_eq!(draft_text, "a draft with words", "the fixture draft");

    // Park the plan at its tail so there is a window to walk back up.
    assert!(
        key(&mut r, KeyCode::End),
        "`End` is a pane jump for the plan"
    );
    let tail = r.plan_scroll;
    assert!(tail > 3, "the plan fixture needs a window to walk: {tail}");
    let (rows, _, _) = draw_frame(&mut r, 120, 30);
    let window_before = snapshot(&pane_body(&rows, pane));

    let mut at = tail;
    for step in 1..=3 {
        assert!(
            key(&mut r, KeyCode::Up),
            "the plan refused the `Up` it owns at step {step}"
        );
        assert_eq!(r.plan_scroll, at - 1, "step {step} was not one plan row");
        at = r.plan_scroll;
        assert_eq!(r.input_text, draft_text, "step {step} edited the draft");
        assert_eq!(r.textarea.cursor(), caret, "step {step} moved the caret");
        assert_eq!(
            r.history_index, walk,
            "step {step} started or moved a history walk"
        );
        assert!(
            r.input_text.contains('a'),
            "step {step} reached the draft's content"
        );
        let (rows, _, _) = draw_frame(&mut r, 120, 30);
        assert_eq!(
            snapshot(&rows_within(&rows, composer)),
            composer_before,
            "step {step} redrew the composer"
        );
        assert_ne!(
            snapshot(&pane_body(&rows, pane)),
            window_before,
            "step {step} did not move the plan window it owns"
        );
    }
    let (rows, _, _) = draw_frame(&mut r, 120, 30);
    let window_after_ups = snapshot(&pane_body(&rows, pane));

    for step in 1..=2 {
        assert!(
            key(&mut r, KeyCode::Down),
            "the plan swallowed its `Down` at step {step}"
        );
        assert_eq!(r.plan_scroll, at + 1, "step {step} was not one plan row");
        at = r.plan_scroll;
        assert_eq!(r.input_text, draft_text, "step {step} edited the draft");
        assert_eq!(r.textarea.cursor(), caret, "step {step} moved the caret");
        assert_eq!(r.history_index, walk, "step {step} touched the walk");
    }
    let (rows, _, _) = draw_frame(&mut r, 120, 30);
    assert_ne!(
        snapshot(&pane_body(&rows, pane)),
        window_after_ups,
        "the run of `Down` presses did not move the plan window"
    );
    assert_eq!(
        snapshot(&rows_within(&rows, composer)),
        composer_before,
        "the plan's arrows redrew the composer"
    );
    assert!(
        on_screen(&rows).contains("a draft with words"),
        "the draft the pane arrows interrupted is not on screen:\n{}",
        on_screen(&rows)
    );
}

/// Contract row (c), the roster half: with the **Workers** pane focused, a run of
/// plain `Up` / `Down` presses walks only the drawn roster window. The composer keeps
/// its pixels, its draft, its caret and its history bookkeeping press for press, and
/// the detail log — the pane's other viewport — stays where its own keys left it.
#[test]
fn workers_focus_arrows_walk_the_drawn_roster_window_and_leave_the_composer_alone() {
    const ROWS: usize = 12;
    let mut r = roster_fixture(ROWS);
    r.history = vec![
        "an old submission".to_string(),
        "a newer submission".to_string(),
    ];
    r.set_input_content("a draft with words");
    assert_eq!(r.focus, state::FocusPane::Workers, "the fixture focus");

    let (rows, _, rects) = draw_frame(&mut r, 140, 24);
    let pane = rects.workers.expect("M3 draws the Workers pane");
    let composer = rects.input.expect("every mode draws the composer");
    // The rect the key path clamps the roster window against is the one the last
    // frame published (`render.rs::draw` stores it).
    r.subagent_area = pane;
    let composer_before = snapshot(&rows_within(&rows, composer));
    let draft_text = draft(&mut r);
    let caret = r.textarea.cursor();
    let walk = r.history_index;
    let selected = r.selected_subagent_idx;
    let window_before = drawn_roster_rows(&rows, pane, ROWS);
    assert!(
        window_before.len() >= 3,
        "the fixture needs a roster window with room to travel: {window_before:?}"
    );

    let mut at = usize::from(r.subagent_list_scroll);
    for step in 1..=3 {
        assert!(
            key(&mut r, KeyCode::Down),
            "the roster refused the `Down` it owns at step {step}"
        );
        assert_eq!(
            usize::from(r.subagent_list_scroll),
            at + 1,
            "step {step} was not one roster row"
        );
        at = usize::from(r.subagent_list_scroll);
        assert_eq!(r.input_text, draft_text, "step {step} edited the draft");
        assert_eq!(r.textarea.cursor(), caret, "step {step} moved the caret");
        assert_eq!(
            r.history_index, walk,
            "step {step} started or moved a history walk"
        );
        assert_eq!(
            r.selected_subagent_idx, selected,
            "step {step} moved the roster selection instead of its window"
        );
        let (rows, _, _) = draw_frame(&mut r, 140, 24);
        let pane = rects.workers.expect("M3 draws the Workers pane");
        assert_eq!(
            snapshot(&rows_within(&rows, composer)),
            composer_before,
            "step {step} redrew the composer"
        );
        assert_eq!(
            drawn_roster_rows(&rows, pane, ROWS),
            (at..at + window_before.len()).collect::<Vec<usize>>(),
            "step {step} did not reveal exactly the next roster row"
        );
    }

    for step in 1..=2 {
        assert!(
            key(&mut r, KeyCode::Up),
            "the roster swallowed its `Up` at step {step}"
        );
        assert_eq!(
            usize::from(r.subagent_list_scroll),
            at - 1,
            "step {step} was not one roster row"
        );
        at = usize::from(r.subagent_list_scroll);
        assert_eq!(r.input_text, draft_text, "step {step} edited the draft");
        assert_eq!(r.textarea.cursor(), caret, "step {step} moved the caret");
        assert_eq!(r.history_index, walk, "step {step} touched the walk");
    }
    let (rows, _, _) = draw_frame(&mut r, 140, 24);
    let pane = rects.workers.expect("M3 draws the Workers pane");
    assert_eq!(
        drawn_roster_rows(&rows, pane, ROWS),
        (at..at + window_before.len()).collect::<Vec<usize>>(),
        "the run of `Up` presses did not walk the roster window back"
    );
    assert_eq!(
        snapshot(&rows_within(&rows, composer)),
        composer_before,
        "the roster's arrows redrew the composer"
    );
    assert!(
        on_screen(&rows).contains("a draft with words"),
        "the draft the roster arrows interrupted is not on screen:\n{}",
        on_screen(&rows)
    );
}

/// Contract row (c): `Down` on an **empty draft** with a pane focused is a
/// viewport step, never a swallowed no-op — the next frame shows the window one
/// row lower, one line fewer counted below it, and a partial press toward the
/// tail states no follow decision at all (defects 3 and 9).
#[test]
fn down_on_an_empty_draft_over_a_scrollback_pane_steps_the_drawn_window() {
    let mut r = scrollback_renderer();
    assert!(
        r.input_text.is_empty(),
        "the default state is an empty draft, which is the state the defect swallowed"
    );

    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let max = r
        .transcript_measured_max
        .get()
        .expect("the pane published its measure");
    assert!(max >= 4, "the fixture needs room to walk: {max}");
    assert_eq!(
        assert_frame_agrees_with_its_chip("boot frame", &rows, pane, max, &r),
        (max, FollowChip::Live),
        "the boot frame sits at the tail it measured, and says LIVE"
    );
    for _ in 0..3 {
        assert!(key(&mut r, KeyCode::Up));
    }
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    let (at, chip) = assert_frame_agrees_with_its_chip("three rows up", &rows, pane, max, &r);
    assert_eq!(at, max - 3);
    assert_eq!(chip, FollowChip::New(3));
    let body_before = snapshot(&transcript_body(&rows, pane));

    // The press that used to be swallowed: empty draft, pane focused, no walk in
    // progress. It owns the key and moves the window exactly one row.
    assert!(
        key(&mut r, KeyCode::Down),
        "`Down` with an empty draft was swallowed instead of scrolling the pane"
    );
    assert!(
        r.history_index.is_none(),
        "and it invented no history walk on the way"
    );
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    let (at, chip) = assert_frame_agrees_with_its_chip("one row back down", &rows, pane, max, &r);
    assert_eq!(at, max - 2, "the step was not one row");
    assert_eq!(chip, FollowChip::New(2));
    let body_after = snapshot(&transcript_body(&rows, pane));
    assert_ne!(body_after, body_before, "the drawn window did not move");
    assert!(
        body_after[0].contains(&format!("line {:02}", max - 2)),
        "the window is not showing the row the press asked for:\n{}",
        on_screen(&body_after)
    );
    assert!(
        !r.follow,
        "a press that moved the viewport without reaching the tail stated a follow \
         decision it did not express"
    );

    // One more step is still short of the tail and still states no decision; the
    // press that **arrives** is the one allowed back on (spec §10 item 14).
    assert!(key(&mut r, KeyCode::Down));
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    let (at, chip) =
        assert_frame_agrees_with_its_chip("one step short of the tail", &rows, pane, max, &r);
    assert_eq!(at, max - 1);
    assert_eq!(chip, FollowChip::New(1));
    assert!(!r.follow, "a step short of the tail re-armed follow");
    assert!(key(&mut r, KeyCode::Down));
    let (rows, _, _) = draw_frame(&mut r, 100, 30);
    let (at, chip) = assert_frame_agrees_with_its_chip("arrival at the tail", &rows, pane, max, &r);
    assert_eq!(at, max);
    assert_eq!(chip, FollowChip::Live);
}

/// Contract row (d): after every scroll gesture the **same frame** states one
/// position — the `scroll a/b` readout, the follow chip and the window the pane
/// painted all agree with each other, with the offset the renderer keeps, and with
/// the follow flag its chip is supposed to echo. This is the visible half of
/// defect 9 and of the chip lag the reported overlap produced.
#[test]
fn the_follow_chip_and_the_drawn_window_agree_after_every_scroll_key() {
    let mut r = scrollback_renderer();
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let max = r
        .transcript_measured_max
        .get()
        .expect("the pane published its measure");
    assert!(max >= 12, "the walk needs a body with room: {max}");
    assert_eq!(
        assert_frame_agrees_with_its_chip("boot frame", &rows, pane, max, &r),
        (max, FollowChip::Live),
        "the boot frame follows its tail"
    );

    // One gesture per step, then exactly one frame, then the agreement check. The
    // expected offset and chip are what the contract says the gesture means.
    let gestures: Vec<(&str, (KeyCode, KeyModifiers), usize, FollowChip)> = vec![
        (
            "Up off the tail",
            (KeyCode::Up, KeyModifiers::NONE),
            max - 1,
            FollowChip::New(1),
        ),
        (
            "Up again",
            (KeyCode::Up, KeyModifiers::NONE),
            max - 2,
            FollowChip::New(2),
        ),
        (
            "Down, short of the tail",
            (KeyCode::Down, KeyModifiers::NONE),
            max - 1,
            FollowChip::New(1),
        ),
        (
            "Down, arriving at the tail",
            (KeyCode::Down, KeyModifiers::NONE),
            max,
            FollowChip::Live,
        ),
        (
            "PageUp off the tail",
            (KeyCode::PageUp, KeyModifiers::NONE),
            max - 10,
            FollowChip::New(10),
        ),
        (
            "Home to the top",
            (KeyCode::Home, KeyModifiers::NONE),
            0,
            FollowChip::New(max),
        ),
        (
            "End back to the tail",
            (KeyCode::End, KeyModifiers::NONE),
            max,
            FollowChip::Live,
        ),
        (
            "Ctrl+F off, at the tail",
            (KeyCode::Char('f'), KeyModifiers::CONTROL),
            max,
            FollowChip::FollowOff,
        ),
        (
            "Up off the tail again",
            (KeyCode::Up, KeyModifiers::NONE),
            max - 1,
            FollowChip::New(1),
        ),
        (
            "PageDown arriving",
            (KeyCode::PageDown, KeyModifiers::NONE),
            max,
            FollowChip::Live,
        ),
    ];
    for (label, (code, modifiers), expected_at, expected_chip) in gestures {
        press(&mut r, code, modifiers);
        let (rows, _, rects) = draw_frame(&mut r, 100, 30);
        let pane = rects.transcript.expect("M1 draws the transcript");
        let drawn = assert_frame_agrees_with_its_chip(label, &rows, pane, max, &r);
        assert_eq!(
            drawn,
            (expected_at, expected_chip),
            "{label}: the frame states a different position from the one the \
             contract gives it (chip row {:?})",
            transcript_chip(&rows, pane)
        );
    }
}

/// Contract row (e): `Ctrl+Up` / `Ctrl+Down` stay the history chord **in every
/// focus** — with a pane focused the recalled entry is drawn in the composer, the
/// `history n/m` chip follows the walk and leaves with it, and the pane under it
/// never moves (defects 6 and 7).
#[test]
fn the_history_chord_still_walks_while_a_pane_holds_the_focus() {
    let mut r = scrollback_renderer();
    r.history = vec![
        "entry one\nentry two".to_string(),
        "line one\nline two".to_string(),
    ];
    // A two-line draft on purpose: the composer's height (§2.1 `input_h`) is what
    // gives the body its rows, so holding the draft at two lines keeps the pane's
    // geometry fixed and makes "the chord never moved the pane" a pixel claim
    // rather than an offset claim.
    r.set_input_content("keep\nthis");

    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let pane = rects.transcript.expect("M1 draws the transcript");
    let body_before = snapshot(&transcript_body(&rows, pane));
    assert_eq!(
        r.focus,
        state::FocusPane::Transcript,
        "the fixture holds a pane focus, so every press here is a pane-focus press"
    );

    assert!(
        press(&mut r, KeyCode::Up, KeyModifiers::CONTROL),
        "`Ctrl+Up` is a key press in a pane focus too"
    );
    assert_eq!(r.input_text, "line one\nline two");
    assert_eq!(r.history_index, Some(1));
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let composer = rects.input.expect("M1 draws the composer");
    let drawn_composer = on_screen(&pane_body(&rows, composer));
    assert!(
        drawn_composer.contains("line one") && drawn_composer.contains("line two"),
        "the recalled entry is not drawn in the composer: {drawn_composer:?}"
    );
    assert!(
        on_screen(&rows).contains("history 2/2"),
        "the chip names the walk the chord started:\n{}",
        on_screen(&rows)
    );
    assert_eq!(
        snapshot(&transcript_body(
            &rows,
            rects.transcript.expect("transcript")
        )),
        body_before,
        "the chord moved the pane that was focused"
    );

    // A multi-line entry cannot stop the walk: the recalled caret sits at the end
    // of the entry's first line, so the next `Ctrl+Up` is the next entry and not a
    // caret move (defect 6).
    assert!(press(&mut r, KeyCode::Up, KeyModifiers::CONTROL));
    assert_eq!(r.input_text, "entry one\nentry two");
    assert_eq!(r.history_index, Some(0));

    // `Ctrl+Down` walks forward, then back out to the operator's own draft, and
    // the chip leaves with the walk.
    assert!(press(&mut r, KeyCode::Down, KeyModifiers::CONTROL));
    assert_eq!(r.input_text, "line one\nline two");
    assert_eq!(r.history_index, Some(1));
    assert!(press(&mut r, KeyCode::Down, KeyModifiers::CONTROL));
    assert_eq!(
        r.input_text, "keep\nthis",
        "the draft from before the walk came back"
    );
    assert!(r.history_index.is_none());
    let (rows, _, rects) = draw_frame(&mut r, 100, 30);
    let drawn_composer = on_screen(&pane_body(&rows, rects.input.expect("composer")));
    assert!(
        drawn_composer.contains("keep"),
        "the composer is not showing the operator's own draft: {drawn_composer:?}"
    );
    assert!(
        !drawn_composer.contains("line one"),
        "the composer is still showing the recalled entry: {drawn_composer:?}"
    );
    let screen = on_screen(&rows);
    assert!(
        !screen.contains("history 2/2") && !screen.contains("history 1/2"),
        "a finished walk still advertises itself:\n{screen}"
    );
    assert_eq!(
        snapshot(&transcript_body(
            &rows,
            rects.transcript.expect("transcript")
        )),
        body_before,
        "the history chord moved the pane"
    );
}

/// The chord is now the **only** door to the input history, so it has to answer in
/// every focus: `Ctrl+Up` recalls from the composer and from each of the three pane
/// focuses, the `history n/m` readout follows it in all four, the walk keeps walking
/// from the focus it started in, and no focus hands that recall to the plain arrows.
#[test]
fn the_history_chord_recalls_from_the_composer_and_from_every_pane_focus() {
    for focus in [
        state::FocusPane::Input,
        state::FocusPane::Transcript,
        state::FocusPane::Plan,
        state::FocusPane::Workers,
    ] {
        let mut r = m4_renderer();
        r.history = vec![
            "the oldest entry".to_string(),
            "the middle entry".to_string(),
            "the newest entry".to_string(),
        ];
        r.set_focus(focus);

        // One frame first: the panes publish the geometry the key path clamps
        // against. The pixel baseline is taken after the plain arrow below, so that
        // claim compares the frame the chord is pressed into.
        let (_, _, rects) = draw_frame(&mut r, 120, 30);
        r.subagent_area = rects.workers.expect("M4 Split draws the Workers pane");

        // The plain arrows come first, so the baseline below is taken with their
        // effect (a pane step, never a recall) already spent.
        let claimed = key(&mut r, KeyCode::Up);
        match focus {
            state::FocusPane::Input => assert!(
                !claimed,
                "{focus:?}: a plain `Up` with a history waiting behind an empty draft \
                 is still not a key press"
            ),
            pane => assert!(
                claimed,
                "{pane:?}: the pane owns the plain arrow, and refused to say so"
            ),
        }
        assert!(
            r.input_text.is_empty(),
            "{focus:?}: a plain `Up` recalled a draft out from under the operator"
        );
        assert_eq!(
            r.history_index, None,
            "{focus:?}: a plain `Up` started a history walk"
        );

        // Now the chord.
        let (rows, _, rects) = draw_frame(&mut r, 120, 30);
        let before: Vec<(&'static str, Vec<String>)> = leaf_rects(&rects)
            .iter()
            .filter(|(label, _)| *label != "input")
            .map(|(label, rect)| (*label, snapshot(&rows_within(&rows, *rect))))
            .collect();
        assert!(
            press(&mut r, KeyCode::Up, KeyModifiers::CONTROL),
            "{focus:?}: `Ctrl+Up` must recall from here"
        );
        assert_eq!(
            r.input_text, "the newest entry",
            "{focus:?}: the chord recalled the wrong entry"
        );
        assert_eq!(
            r.history_index,
            Some(2),
            "{focus:?}: the chord left the walk bookkeeping elsewhere"
        );

        let (rows, _, _) = draw_frame(&mut r, 120, 30);
        let composer = rects.input.expect("every mode draws the composer");
        let drawn_composer = on_screen(&pane_body(&rows, composer));
        assert!(
            drawn_composer.contains("the newest entry"),
            "{focus:?}: the recalled entry is not in the drawn composer: {drawn_composer:?}"
        );
        assert!(
            on_screen(&rows).contains("history 3/3"),
            "{focus:?}: the readout does not follow the chord:\n{}",
            on_screen(&rows)
        );
        for (label, rect) in leaf_rects(&rects) {
            if label == "input" {
                continue;
            }
            let earlier = before
                .iter()
                .find(|(name, _)| *name == label)
                .map(|(_, cells)| cells.clone())
                .expect("the baseline covers every drawn region");
            assert_eq!(
                snapshot(&rows_within(&rows, rect)),
                earlier,
                "{focus:?}: the history chord redrew {label}"
            );
        }

        // And the walk is not a one-press trick from a non-composer focus: it keeps
        // walking, then walks back out to the operator's own draft.
        assert!(
            press(&mut r, KeyCode::Up, KeyModifiers::CONTROL),
            "{focus:?}: the second `Ctrl+Up` did not walk"
        );
        assert_eq!(r.input_text, "the middle entry", "{focus:?}: wrong entry");
        assert_eq!(r.history_index, Some(1));
        assert!(press(&mut r, KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(
            r.input_text, "the newest entry",
            "{focus:?}: `Ctrl+Down` walked past an entry"
        );
        assert_eq!(r.history_index, Some(2));
        assert!(press(&mut r, KeyCode::Down, KeyModifiers::CONTROL));
        assert!(
            r.input_text.is_empty(),
            "{focus:?}: the walk did not end back at the operator's draft"
        );
        assert!(r.history_index.is_none());
        let (rows, _, _) = draw_frame(&mut r, 120, 30);
        assert!(
            !on_screen(&rows).contains("history "),
            "{focus:?}: a finished walk still advertises itself:\n{}",
            on_screen(&rows)
        );
    }
}

/// Twelve roster rows for the Workers pane, named and logged so every row and every
/// log line is individually addressable in the drawn frame. The role the pane prints
/// comes from the part of the name before `-t-` (`activity.rs::role_label`), and the
/// task id is printed right behind it, so `roster07` and `t-9207` both identify one
/// row on screen. The names are unique to this file so no other suite's registry key
/// can be mistaken for one of them.
fn roster_fixture(count: usize) -> TuiRenderer {
    let mut r = TuiRenderer::new();
    r.subagents = (0..count)
        .map(|index| SubagentDetail {
            name: format!("roster{index:02}-t-92{index:02}"),
            task_id: Some(format!("t-92{index:02}")),
            logs: (0..40)
                .map(|line| format!("- log line {index:02}/{line:02}"))
                .collect(),
            is_active: true,
            ..SubagentDetail::default()
        })
        .collect();
    r.selected_subagent_idx = count / 2;
    r.set_view_mode(state::ViewMode::Workers);
    r.set_focus(state::FocusPane::Workers);
    r
}

/// Which roster rows the frame actually painted, as row indices in order: the drawn
/// window read off the pixels rather than off `subagent_list_scroll`.
fn drawn_roster_rows(rows: &[String], pane: Rect, count: usize) -> Vec<usize> {
    let screen = on_screen(&rows_within(rows, pane));
    (0..count)
        .filter(|index| screen.contains(&format!("roster{index:02}")))
        .collect()
}

/// The detail log the frame actually painted, in order. Each row's log lines are
/// distinct, so this compares the selection **and** the log offset at once.
fn drawn_log(rows: &[String], pane: Rect) -> Vec<String> {
    pane_body(rows, pane)
        .into_iter()
        .filter(|row| row.contains("log line"))
        .collect()
}

/// Contract row (b), the second pane: the Workers pane has **two** viewports and the
/// keys never cross them. `Up`/`Down` walk the drawn roster window one row per press
/// and leave the drawn detail log pixel-identical; `PageUp`/`PageDown`/`Home`/`End`
/// walk the log and leave the drawn roster window pixel-identical; a press at the end
/// of the roster window moves nothing and asks for no repaint. The state-level half is
/// pinned in `events.rs`
/// (`the_workers_arrows_walk_the_roster_window_and_leave_the_log_alone`,
/// `the_detail_page_and_jump_keys_leave_the_roster_window_where_it_stands`); what was
/// never pinned is the frame — which names and which log lines are on screen after
/// each press (audit defect 14).
#[test]
fn the_workers_arrows_walk_the_drawn_roster_window_while_the_page_keys_walk_the_drawn_log() {
    const ROWS: usize = 12;
    let mut r = roster_fixture(ROWS);
    // The log has to be off its tail or "the page key moved the log" is not
    // observable, and nothing here may restate a follow flag.
    r.subagent_autoscroll = false;
    r.set_follow(false);

    let (rows, _, rects) = draw_frame(&mut r, 140, 24);
    let pane = rects.workers.expect("M3 draws the Workers pane");
    // The rect the frame reported, published the way the real pass publishes it
    // (`render.rs::draw` takes `draw_new_ui`'s `PaneRects` and stores
    // `rects.workers` into `subagent_area` after the draw). The key path clamps the
    // roster window against the rect the last frame published rather than estimating
    // it, so a frame-level pin of the walk has to hold that same rect.
    r.subagent_area = pane;
    let detail_present = layout::workers_detail_present(pane.width);
    let (lower, upper) = components::activity::roster_window(
        pane,
        detail_present,
        r.subagents.len(),
        r.selected_subagent_idx,
    );
    assert!(
        upper > lower + 1,
        "the fixture needs a roster window with room to travel: {lower}..{upper} \
         (pane={pane:?}, detail_present={detail_present}, inner={:?})",
        pane.inner(ratatui::layout::Margin::new(1, 1))
    );

    let drawn = drawn_roster_rows(&rows, pane, ROWS);
    let log = drawn_log(&rows, pane);
    assert!(
        !drawn.is_empty() && !log.is_empty(),
        "the pane drew neither roster nor log: {drawn:?}\n{}",
        on_screen(&rows)
    );
    assert_eq!(
        drawn.first().copied(),
        Some(usize::from(r.subagent_list_scroll)),
        "the drawn window does not start at the offset the renderer kept"
    );
    let selected = r.selected_subagent_idx;
    assert!(
        drawn.contains(&selected),
        "the window does not show its own selection: {drawn:?}"
    );

    // `Down` walks the window one row per press, and the log is untouched pixel for
    // pixel at every step.
    let mut step = usize::from(r.subagent_list_scroll);
    while step < upper {
        assert!(key(&mut r, KeyCode::Down), "Down was swallowed");
        let (rows, _, rects) = draw_frame(&mut r, 140, 24);
        let pane = rects.workers.expect("M3 draws the Workers pane");
        step += 1;
        assert_eq!(
            usize::from(r.subagent_list_scroll),
            step,
            "Down at {step} did not walk the window"
        );
        let length = drawn.len();
        assert_eq!(
            drawn_roster_rows(&rows, pane, ROWS),
            (step..step + length).collect::<Vec<_>>(),
            "Down did not reveal exactly the next roster row"
        );
        assert_eq!(
            drawn_log(&rows, pane),
            log,
            "the roster arrow moved the detail log"
        );
        assert_eq!(r.selected_subagent_idx, selected, "Down moved the row");
    }

    // Where the window stands at the end of its travel, read out of a fresh frame:
    // the baseline every later gesture here is compared against.
    let (rows, _, rects) = draw_frame(&mut r, 140, 24);
    let pane = rects.workers.expect("M3 draws the Workers pane");
    let at_end = drawn_roster_rows(&rows, pane, ROWS);

    // The press past the end of the window: claimed by the pane, but it moves
    // nothing and asks for no repaint.
    r.dirty = false;
    assert!(key(&mut r, KeyCode::Down), "Down is still a pane key");
    assert_eq!(
        usize::from(r.subagent_list_scroll),
        upper,
        "Down ran off the end of the window"
    );
    assert!(!r.dirty, "a press with nothing to move asked for a repaint");
    let (rows, _, rects) = draw_frame(&mut r, 140, 24);
    let pane = rects.workers.expect("M3 draws the Workers pane");
    assert_eq!(
        drawn_roster_rows(&rows, pane, ROWS),
        at_end,
        "a press at the end of the window moved the drawn roster"
    );
    assert_eq!(
        drawn_log(&rows, pane),
        log,
        "a press at the end of the window moved the detail log"
    );

    // `PageUp` / `PageDown` belong to the other viewport: the log moves, the drawn
    // window does not.
    assert!(key(&mut r, KeyCode::PageDown), "PageDown is a pane key");
    let (rows, _, rects) = draw_frame(&mut r, 140, 24);
    let pane = rects.workers.expect("M3 draws the Workers pane");
    assert_ne!(
        drawn_log(&rows, pane),
        log,
        "PageDown left the drawn log exactly where it was"
    );
    assert_eq!(
        drawn_roster_rows(&rows, pane, ROWS),
        at_end,
        "PageDown moved the drawn roster window"
    );
    assert!(key(&mut r, KeyCode::PageUp));
    let (rows, _, rects) = draw_frame(&mut r, 140, 24);
    let pane = rects.workers.expect("M3 draws the Workers pane");
    assert_eq!(
        drawn_log(&rows, pane),
        log,
        "PageUp did not walk the drawn log back to where it started"
    );
    assert_eq!(
        drawn_roster_rows(&rows, pane, ROWS),
        at_end,
        "PageUp moved the drawn roster window"
    );

    // `Home` / `End` jump the log and leave the window alone.
    assert!(key(&mut r, KeyCode::End));
    let (rows, _, rects) = draw_frame(&mut r, 140, 24);
    let pane = rects.workers.expect("M3 draws the Workers pane");
    assert_ne!(
        drawn_log(&rows, pane),
        log,
        "End did not take the drawn log to its tail"
    );
    assert_eq!(
        drawn_roster_rows(&rows, pane, ROWS),
        at_end,
        "End moved the drawn roster window"
    );
    assert!(key(&mut r, KeyCode::Home));
    let (rows, _, rects) = draw_frame(&mut r, 140, 24);
    let pane = rects.workers.expect("M3 draws the Workers pane");
    assert_eq!(
        drawn_log(&rows, pane),
        log,
        "Home did not bring the drawn log back"
    );
    assert_eq!(
        drawn_roster_rows(&rows, pane, ROWS),
        at_end,
        "Home moved the drawn roster window"
    );

    // And none of it reached the composer or the transcript's follow pair.
    assert!(
        r.input_text.is_empty() && r.history_index.is_none(),
        "a Workers-pane gesture reached the composer: {:#?}",
        r.input_text
    );
    assert!(
        !r.follow && !r.chat_auto_scroll,
        "a Workers key restated transcript follow"
    );
    assert_eq!(
        r.selected_subagent_idx, selected,
        "a viewport key moved the row"
    );
}

// ---- composer typing is never starved (§4.4) --------------------------------
//
// The reported regression: normal text could not be written in the composer, "at
// least not if one starts typing `e`". `e` was a **draft-scoped** mnemonic — it
// fired whenever the draft was empty, whichever pane held the focus — so the first
// keystroke of every draft that begins with `e` was spent recalling the sticky
// error instead of reaching the draft. The mnemonic is now **focus-scoped**: with
// the composer focused `e` is a printable (the `x` mnemonic already required the
// Workers pane, in its own guard), and the recall stays reachable from every body
// pane and from `/error`. `?` keeps its documented empty-composer binding (spec §5,
// §10.4 item 4), which is why the pins below walk letters, not punctuation.

/// Every letter of the alphabet reaches the composer as the **first** character of a
/// draft, `e` and `x` included (the fixture carries worker rows, so `x` has to prove
/// it needs the Workers pane and not merely an empty draft). No transcript traffic
/// and no overlay on the way: the press only types.
#[test]
fn every_letter_types_into_the_focused_composer_including_a_leading_e() {
    for c in 'a'..='z' {
        let mut r = m4_renderer();
        assert!(
            r.focus.composer_has_focus(),
            "boot focuses the composer, so this is the keystroke the operator types"
        );
        let messages_before = r.messages.clone();
        assert!(
            press(&mut r, KeyCode::Char(c), KeyModifiers::NONE),
            "`{c}` was not a key press at all"
        );
        assert_eq!(
            draft(&mut r),
            c.to_string(),
            "`{c}` never reached the draft"
        );
        assert_eq!(
            r.messages, messages_before,
            "`{c}` answered with transcript traffic instead of typing"
        );
        assert_eq!(
            r.overlay,
            state::OverlayState::None,
            "`{c}` opened an overlay instead of typing"
        );
    }
}

/// The user-visible shape of the regression: `e` on its own, and the reported
/// `test e`. Both land whole in the draft, and the draft still submits and still
/// walks its history, so nothing about the composer contract moved with the fix.
#[test]
fn a_draft_that_starts_with_e_types_submits_and_keeps_the_history_chord() {
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Input);
    let messages_before = r.messages.clone();

    type_text(&mut r, "e");
    assert_eq!(draft(&mut r), "e", "a draft may start with `e`");
    assert_eq!(
        r.messages, messages_before,
        "the leading `e` typed, it did not report"
    );

    type_text(&mut r, "rror: the e2e test failed");
    assert_eq!(
        draft(&mut r),
        "error: the e2e test failed",
        "every `e` of the draft reaches it, not just the first"
    );
    assert_eq!(
        r.messages, messages_before,
        "typing into the composer pushed transcript traffic"
    );
    assert!(
        r.history_index.is_none(),
        "typing must not start a history walk: {:?}",
        r.history_index
    );

    // `Ctrl+Up` still walks the history from the composer (§5), with a draft standing
    // in it, and the draft comes back whole.
    r.history = vec!["earlier note".to_string()];
    assert!(press(&mut r, KeyCode::Up, KeyModifiers::CONTROL));
    assert_eq!(draft(&mut r), "earlier note");
    assert_eq!(r.history_index, Some(0));
    assert!(press(&mut r, KeyCode::Down, KeyModifiers::CONTROL));
    assert_eq!(draft(&mut r), "error: the e2e test failed");

    // And Enter submits the whole line, which is all the operator ever wanted.
    assert!(key(&mut r, KeyCode::Enter));
    let mut sent = Vec::new();
    while let Ok(line) = r.rx.try_recv() {
        sent.push(line);
    }
    assert_eq!(
        sent,
        vec!["error: the e2e test failed".to_string()],
        "the typed draft was not submitted verbatim"
    );
    assert!(draft(&mut r).is_empty(), "and submitting cleared it");
}

/// The recall keeps its substance and loses its keystroke: `/error` replays the
/// sticky error from a **pane** focus and from the **composer** focus alike (so it is
/// never a silent command, G5), it types nothing and scrolls nothing (item 14), and
/// the bare letter that used to carry it inserts wherever the draft is.
#[test]
fn error_recalls_the_sticky_error_in_every_focus_and_the_bare_letter_types() {
    let mut r = scrollback_renderer();
    assert_eq!(
        r.focus,
        state::FocusPane::Transcript,
        "the fixture starts over a pane"
    );
    r.on_event(&Event::Status(
        "LLM error: http (Ready)\nthe backend refused the request".to_string(),
    ));
    let before = r.messages.len();

    // Over the pane: the recall is one submitted line, and it is the sticky buffer.
    type_text(&mut r, "/error");
    assert!(key(&mut r, KeyCode::Enter));
    assert_eq!(r.messages.len(), before + 1, "the recall is one new line");
    assert!(
        r.messages
            .last()
            .is_some_and(|m| m.starts_with("✗ http\nLLM error: http (Ready)")),
        "the sticky buffer is what got recalled: {:?}",
        r.messages.last()
    );
    assert!(draft(&mut r).is_empty(), "the recall does not type");
    assert!(
        drain_submissions(&mut r).is_empty(),
        "a command must never reach the session loop"
    );

    // The recall stated a notice, it did not scroll: the drawn window is still where
    // the arrow left it (item 14).
    draw_frame(&mut r, 100, 30);
    assert!(
        key(&mut r, KeyCode::Up),
        "the pane owns `Up` and steps one line"
    );
    let offset = r.chat_scroll;
    assert!(offset > 0, "the fixture has a window to step: {offset}");

    // From the composer focus the very same command recalls the very same line, and
    // the pane viewport under it does not move.
    r.set_focus(state::FocusPane::Input);
    let recalled_once = r.messages.len();
    type_text(&mut r, "/error");
    assert!(key(&mut r, KeyCode::Enter));
    assert_eq!(r.messages.len(), recalled_once + 1);
    assert!(
        r.messages
            .last()
            .is_some_and(|m| m.starts_with("✗ http\nLLM error: http (Ready)")),
        "the recall is the same from the composer: {:?}",
        r.messages.last()
    );
    assert_eq!(
        r.chat_scroll, offset,
        "recalling from the composer scrolled a pane"
    );

    // And the bare letter is a letter in both focuses — that is the whole point of
    // moving the capability into a slash command.
    for focus in [state::FocusPane::Input, state::FocusPane::Transcript] {
        r.set_focus(focus);
        assert!(key(&mut r, KeyCode::Char('e')), "{focus:?}: `e` is typing");
        assert_eq!(
            draft(&mut r),
            "e",
            "{focus:?}: the letter reached the draft"
        );
        assert!(
            r.messages.last().is_some_and(|m| m.starts_with('✗')),
            "{focus:?}: the typed letter recalled a second time"
        );
        r.set_input_content("");
        assert!(draft(&mut r).is_empty());
    }
}

// ---- the keymap rule the operator asked for: no bare letter is a mnemonic ----

/// The fixture the two removed bindings would have been **armed** for: three live
/// roster rows with a selection (so the old `x` had a subject the moment the Workers
/// pane held focus) and a sticky error in the §8.5 buffer (so the old `e` had
/// something to recall over any body pane). A letter typed into this renderer must
/// still be only a letter.
fn mnemonic_armed_renderer() -> TuiRenderer {
    let mut r = m4_renderer();
    r.on_event(&Event::Status(
        "LLM error: rate limit (Ready)\nthe backend said no".to_string(),
    ));
    r.selected_subagent_idx = 1;
    r
}

/// The rule, pinned letter by letter: a printable is the first character of a word
/// far more often than it is a command, so **every** letter a–z types into the
/// composer in every focus — over the roster included, where `x` used to open a
/// confirm, and over a body pane holding a recorded error, where `e` used to recall
/// it. Nothing else may happen on the way: no overlay, no transcript traffic, no
/// roster move, nothing sent to the loop.
#[test]
fn every_printable_letter_is_a_character_in_every_focus() {
    for focus in [
        state::FocusPane::Input,
        state::FocusPane::Transcript,
        state::FocusPane::Plan,
        state::FocusPane::Workers,
    ] {
        for letter in 'a'..='z' {
            let mut r = mnemonic_armed_renderer();
            r.set_focus(focus);
            let messages = r.messages.len();
            let selected = r.selected_subagent_idx;

            assert!(
                key(&mut r, KeyCode::Char(letter)),
                "{focus:?}: {letter} was dropped instead of typed"
            );
            assert_eq!(
                draft(&mut r),
                letter.to_string(),
                "{focus:?}: {letter} never became the first character of the draft"
            );
            assert_eq!(
                r.input_text,
                letter.to_string(),
                "{focus:?}: {letter} never reached the draft mirror"
            );
            assert_eq!(
                r.overlay,
                state::OverlayState::None,
                "{focus:?}: {letter} opened an overlay"
            );
            assert!(
                r.confirm_action.is_none(),
                "{focus:?}: {letter} armed a destructive request"
            );
            assert!(!r.help_open, "{focus:?}: {letter} opened the legend");
            assert_eq!(
                r.messages.len(),
                messages,
                "{focus:?}: {letter} pushed transcript traffic"
            );
            assert_eq!(
                r.selected_subagent_idx, selected,
                "{focus:?}: {letter} moved the roster selection"
            );
            assert!(
                drain_submissions(&mut r).is_empty(),
                "{focus:?}: {letter} reached the session loop"
            );
        }
    }
}

/// A real draft built out of the letters that used to be bindings — `x` as the very
/// first character, `x` and `e` mid-word — types whole and submits whole, including
/// in the focus where `x` used to open a confirm and over a pane holding an error the
/// `e` used to recall.
#[test]
fn a_draft_full_of_x_and_e_types_and_submits_verbatim() {
    const DRAFT: &str = "exit the x route, e.g. now";
    for focus in [
        state::FocusPane::Workers,
        state::FocusPane::Transcript,
        state::FocusPane::Input,
    ] {
        let mut r = mnemonic_armed_renderer();
        r.set_focus(focus);
        let messages = r.messages.len();

        type_text(&mut r, DRAFT);
        assert_eq!(
            draft(&mut r),
            DRAFT,
            "{focus:?}: a letter of the draft was intercepted"
        );
        assert_eq!(
            r.overlay,
            state::OverlayState::None,
            "{focus:?}: an overlay opened in the middle of a word"
        );
        assert!(
            r.confirm_action.is_none(),
            "{focus:?}: a request was armed mid-word"
        );
        assert_eq!(
            r.messages.len(),
            messages,
            "{focus:?}: typing pushed transcript traffic"
        );

        assert!(key(&mut r, KeyCode::Enter));
        assert_eq!(
            drain_submissions(&mut r),
            vec![DRAFT.to_string()],
            "{focus:?}: the draft was not submitted verbatim"
        );
        assert!(draft(&mut r).is_empty(), "{focus:?}: the draft lived on");
    }
}

/// A slash command is only a command when the draft **is** the command: the same
/// spellings mid-sentence stay ordinary steering text, so the two replacement routes
/// can never fire by accident while an operator writes about them.
#[test]
fn the_replacement_commands_run_only_when_the_draft_is_the_command() {
    const TEXT: &str = "the /error and /cancel lines in the log";
    let mut r = mnemonic_armed_renderer();
    r.set_focus(state::FocusPane::Workers);
    let messages = r.messages.len();

    type_text(&mut r, TEXT);
    assert_eq!(
        draft(&mut r),
        TEXT,
        "a command spelling mid-text typed itself"
    );
    assert!(key(&mut r, KeyCode::Enter));

    assert_eq!(
        drain_submissions(&mut r),
        vec![TEXT.to_string()],
        "the sentence was not delivered as text"
    );
    assert_eq!(
        r.overlay,
        state::OverlayState::None,
        "a mid-text /cancel opened a confirm"
    );
    assert!(
        r.confirm_action.is_none(),
        "a mid-text /cancel armed a request"
    );
    assert_eq!(
        r.messages.last().map(String::as_str),
        Some("User: the /error and /cancel lines in the log"),
        "no recall and no notice: the whole submission is one text line"
    );
    assert_eq!(r.messages.len(), messages + 1, "{:?}", r.messages);
}

/// The command that replaced the bare `x`: `/cancel` with no argument, typed while
/// the Workers pane holds the focus, reaches the confirm surface for the **selected**
/// roster row — same captured identity, same surface, same `y` — and `y` cancels
/// exactly that worker and no other.
#[test]
fn cancel_command_asks_about_the_selected_row_and_cancels_it_item17() {
    let _registry = registry_gate();
    let ledger = CancellationLedger::plant();
    let broadcast = crate::orchestrator::global_cancellation_token();
    let (agent, task) = ITEM17_ROWS[1];
    let selected = item17_key(agent, task);

    let mut r = item17_renderer();
    r.set_focus(state::FocusPane::Workers);
    r.selected_subagent_idx = 1;

    // Typed one printable at a time, exactly as the operator types it.
    type_text(&mut r, "/cancel");
    assert_eq!(
        draft(&mut r),
        "/cancel",
        "the command is a draft until Enter runs it"
    );
    assert!(key(&mut r, KeyCode::Enter));

    assert_eq!(r.overlay, state::OverlayState::Confirm);
    assert_eq!(
        r.confirm_action,
        Some(events::ConfirmAction::CancelWorker {
            key: selected.clone(),
            task_id: Some(task.to_string()),
        }),
        "the request names the selected row, the same identity the chord captured"
    );
    assert!(
        r.status_line.contains(&selected),
        "the status line names the row: {}",
        r.status_line
    );
    assert!(draft(&mut r).is_empty(), "submitting cleared the command");
    assert!(
        drain_submissions(&mut r).is_empty(),
        "a confirmed-command request never reaches the loop on its own"
    );
    assert!(
        ledger.cancelled().is_empty(),
        "asking cancels nothing: {:?}",
        ledger.cancelled()
    );

    // `y` runs it for that one row and no other.
    assert!(key(&mut r, KeyCode::Char('y')));
    assert_eq!(r.overlay, state::OverlayState::None);
    assert_eq!(
        ledger.cancelled(),
        vec![selected.clone()],
        "exactly the selected roster row was cancelled"
    );
    assert_eq!(
        ledger.aborted(),
        vec![selected.clone()],
        "and the registry labels that row and only that row"
    );
    assert_eq!(ledger.untouched().len(), 2, "the other rows keep running");
    assert!(
        !broadcast.is_cancelled(),
        "a one-worker cancel never reaches the broadcast token"
    );
    assert!(
        r.status_line.contains(&selected),
        "the outcome is stated: {}",
        r.status_line
    );
    assert!(drain_submissions(&mut r).is_empty());
}

/// The chord that replaced the bare `x` cannot collide with typing, so it also never
/// falls through silently: when its guard is not met it says what it would need, and
/// still puts no character in the draft and arms no request.
#[test]
fn ctrl_x_off_the_roster_states_why_and_never_types() {
    // Rows present, roster not focused.
    let mut r = m4_renderer();
    r.set_focus(state::FocusPane::Input);
    assert!(press(&mut r, KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(draft(&mut r).is_empty(), "a Ctrl chord never types");
    assert_eq!(r.overlay, state::OverlayState::None, "and arms nothing");
    assert!(
        r.messages
            .last()
            .is_some_and(|m| m.contains("Ctrl+X") && m.contains("Workers pane")),
        "the chord said what it needs: {:?}",
        r.messages.last()
    );
    assert!(
        r.status_line.contains("Ctrl+X"),
        "and echoed it: {}",
        r.status_line
    );

    // Roster focused, empty roster: the other reason, stated instead of silence.
    let mut empty = TuiRenderer::new();
    empty.set_focus(state::FocusPane::Workers);
    assert!(press(&mut empty, KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(empty.input_text.is_empty(), "and here too it typed nothing");
    assert!(
        empty
            .messages
            .last()
            .is_some_and(|m| m.contains("no worker is on the roster")),
        "{:?}",
        empty.messages
    );
    assert!(empty.confirm_action.is_none());
}

/// Audit pin — this records what `?` does today, it does not bless it. `?` is the one
/// printable the keymap still spends elsewhere: on an **empty** draft, in every focus,
/// it opens the legend and the character never reaches the draft, so a draft whose
/// first character would be `?` (a question typed as written) cannot be typed
/// directly. Any draft with one character in it types `?` normally. The collision-free
/// routes to the same overlay already exist and need no keystroke that can be typed:
/// `F1` and `/help`.
#[test]
fn a_leading_question_mark_opens_the_legend_in_every_focus_and_types_after_one_character() {
    for focus in [
        state::FocusPane::Input,
        state::FocusPane::Transcript,
        state::FocusPane::Plan,
        state::FocusPane::Workers,
    ] {
        let mut r = mnemonic_armed_renderer();
        r.set_focus(focus);

        assert!(key(&mut r, KeyCode::Char('?')));
        assert_eq!(
            r.overlay,
            state::OverlayState::Help,
            "{focus:?}: the empty-draft `?` did not open the legend"
        );
        assert!(
            r.help_open,
            "{focus:?}: the overlay state and the flag disagree"
        );
        assert!(
            draft(&mut r).is_empty(),
            "{focus:?}: the swallowed `?` reached the draft anyway"
        );
        // The character is gone, not queued: a second press re-opens the same
        // overlay and still types nothing.
        assert!(key(&mut r, KeyCode::Char('?')));
        assert!(
            draft(&mut r).is_empty(),
            "{focus:?}: the second `?` typed while the legend was open"
        );

        // One character down and the same key is only a character again.
        r.close_overlay();
        assert!(key(&mut r, KeyCode::Char('w')));
        assert!(key(&mut r, KeyCode::Char('?')));
        assert_eq!(
            draft(&mut r),
            "w?",
            "{focus:?}: `?` mid-draft was swallowed"
        );
        assert_eq!(
            r.overlay,
            state::OverlayState::None,
            "{focus:?}: a mid-draft `?` opened the legend"
        );
    }
}
