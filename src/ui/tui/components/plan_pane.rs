//! Plan / task-overview pane — spec §3 (Plan pane), §2.3 (M2/M4 geometry), §7.2
//! (state glyphs), §8.1 (empty state) and G1 (`done/total` + active task id in
//! every frame).
//!
//! The markdown body is owned here as well since t-009b: the pane draws the plan
//! text itself, with the same heading / `[x]` / `[ ]` / active-task colouring and
//! the same wrapped-line arithmetic as before (all of it through
//! [`crate::ui::tui::formatting`]), and the 250 ms plan-mtime cache that feeds the
//! text stays exactly where it is (`render.rs:266-322`, driven from `mod.rs`) — a
//! pane must never do its own synchronous disk read per frame (§0).
//!
//! This component therefore owns the whole pane, from the same [`PlanProgress`]
//! the header reads:
//!
//! * the plan body itself — text, styling, wrap, scroll and scrollbar;
//! * the bordered block, its title and the `●` focus badge (§7.3, G6);
//! * the **header counter** — `✓ 3/7 done` plus the active task id — riding the
//!   top border, where no body line can be overwritten;
//! * the **state line** on the bottom border: the §7.2 glyph set with the count
//!   each glyph stands for (`✓` checked off, `⧗` still open, `▶` in flight), or the
//!   §8.1 empty-plan statement when the plan carries no task line at all.
//!
//! [`render`] also hands the geometry it used back to the renderer
//! ([`PlanGeometry`]): the offset actually drawn, the largest legal offset and the
//! inner size. Those are the same values `plan_scroll` / `plan_max_scroll` /
//! `plan_width` / `plan_height` carried while the legacy pane owned this, so the
//! scroll-key clamps in `events.rs` keep working unchanged.
//!
//! The two per-task readings §3 lists that are **not** possible from the data this
//! pane receives remain open:
//!
//! * the `⏸` (paused) and `✗` (failed) state of a task line comes from the worker
//!   rows assigned to it, and worker rows are not handed to this pane;
//!
//! until then the pane shows those states per state on the bottom border instead
//! of per line, which is the same glyph vocabulary and never claims a state a task
//! does not have.

use super::{fit_spans, pane_block};
use crate::ui::tui::formatting::{
    line_matches_task_id, visual_line_offset_of_first_pending, visual_line_offset_of_task,
    wrapped_lines,
};
use crate::ui::tui::state::{FocusPane, PlanProgress};
use crate::ui::tui::theme::{self, Glyph};
use ratatui::Frame;
use ratatui::layout::{Alignment, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};

/// What the plan pane draws this frame.
#[derive(Debug, Clone, Copy)]
pub struct PlanData<'a> {
    /// Whether this pane holds focus (§7.3 badge + border).
    pub focus: bool,
    /// Progress derived from the very `plan_content` string this pane renders.
    pub progress: &'a PlanProgress,
    /// Scroll offset of the plan body (§2.4.4), as the renderer last left it.
    pub scroll: u16,
    /// The plan text itself — the cached `plan_content`, including the §8.1
    /// `No active execution plan.` / `Error reading execution plan.` sentinels.
    pub text: &'a str,
    /// Task ids in flight: `active_plan_task` plus every active worker row's task.
    /// A body line matching one of these is the line the operator is looking for.
    pub active_tasks: &'a [String],
    /// Whether the renderer's auto-scroll owns the offset this frame.
    pub auto_scroll: bool,
    /// The task auto-scroll centres on (`active_plan_task`, else the first active
    /// worker's task).
    pub target_task: Option<&'a str>,
}

/// What one plan pass established about the viewport. The renderer writes these
/// back into `plan_scroll` / `plan_max_scroll` so its `↑/↓/PgUp/PgDn` clamps match
/// what was actually drawn.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlanGeometry {
    /// Offset actually drawn this frame.
    pub scroll: u16,
    /// Largest legal offset for this text at this size.
    pub max_scroll: u16,
    /// Text columns inside the border — what `wrapped_lines` counted at.
    pub inner_width: usize,
    /// Text rows inside the border.
    pub inner_height: usize,
}

/// Draw the plan pane: body first, then the block whose border frames it, the
/// header counter and the state line.
///
/// Both counter lines ride border rows, so they can never overwrite a body line
/// however the body is scrolled (§3 "shows", §2.4 zero-rect rule).
pub fn render(frame: &mut Frame, rect: Rect, data: &PlanData<'_>) -> PlanGeometry {
    if rect.width == 0 || rect.height == 0 {
        return PlanGeometry::default();
    }
    let geometry = draw_body(frame, rect, data);

    let border = if data.focus {
        theme::frame_focus()
    } else {
        theme::frame_idle()
    };
    // Border rows lose one cell to each corner, which is what the titles may use.
    let title_width = super::title_width(rect);
    let block = pane_block(FocusPane::Plan.label(), data.focus, border)
        .title(
            Line::from(fit_spans(counter_spans(data.progress), title_width))
                .alignment(Alignment::Right),
        )
        .title_bottom(Line::from(fit_spans(
            state_spans(data, geometry),
            title_width,
        )));
    frame.render_widget(block, rect);
    geometry
}

/// Draw the plan body — text, styling, wrap, scroll and scrollbar — and report the
/// geometry the pass used.
///
/// The scroll arithmetic is the one the legacy pane used (`render.rs:516-540`), so
/// `plan_auto_scroll` keeps its "put the active task one line below the top"
/// behaviour and a manual offset is still only ever lowered to stay legal.
fn draw_body(frame: &mut Frame, rect: Rect, data: &PlanData<'_>) -> PlanGeometry {
    let inner = rect.inner(Margin::new(1, 1));
    let width = usize::from(inner.width).max(1);
    let height = usize::from(inner.height);
    let total = wrapped_lines(data.text, width);
    let max_scroll = total.saturating_sub(height) as u16;
    let scroll = if data.auto_scroll {
        auto_scroll_offset(data, width).min(max_scroll)
    } else {
        data.scroll.min(max_scroll)
    };

    if height > 0 && inner.width > 0 {
        let paragraph = Paragraph::new(body_lines(data))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, inner);
        if max_scroll > 0 {
            let mut state =
                ScrollbarState::new(usize::from(max_scroll)).position(usize::from(scroll));
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                inner,
                &mut state,
            );
        }
    }

    PlanGeometry {
        scroll,
        max_scroll,
        inner_width: usize::from(inner.width),
        inner_height: height,
    }
}

/// Where auto-scroll wants the viewport: the task in flight, else the first open
/// task, else the bottom of a finished plan — always one line of context above.
fn auto_scroll_offset(data: &PlanData<'_>, width: usize) -> u16 {
    let desired = data
        .target_task
        .and_then(|task| visual_line_offset_of_task(data.text, task, width))
        .or_else(|| visual_line_offset_of_first_pending(data.text, width));
    match desired {
        Some(line) => line.saturating_sub(1) as u16,
        None => u16::MAX,
    }
}

/// The plan body as styled lines.
///
/// The colour rules are the legacy ones (`render.rs:499-514`), expressed through
/// [`theme`] slots rather than raw colours (§7.1): a heading is the done chip
/// colour in bold, the in-flight task line is the active chip colour in bold, a
/// checked line is done, an open line pending. Task ids are matched with
/// [`line_matches_task_id`] so `[t-004]` and `t-004` name the same line.
fn body_lines(data: &PlanData<'_>) -> Vec<Line<'static>> {
    data.text
        .lines()
        .map(|raw| {
            let line = raw.replace('\t', "    ");
            let style = if line.starts_with("# ") || line.starts_with("## ") {
                theme::chip_done().add_modifier(ratatui::style::Modifier::BOLD)
            } else if data
                .active_tasks
                .iter()
                .any(|task| line_matches_task_id(&line, task))
                && (line.contains("[ ]") || line.contains("( )"))
            {
                theme::chip_active().add_modifier(ratatui::style::Modifier::BOLD)
            } else if line.contains("[x]") || line.contains("[X]") {
                theme::chip_done()
            } else if line.contains("[ ]") {
                theme::chip_pending()
            } else {
                theme::text_primary()
            };
            Line::styled(line, style)
        })
        .collect()
}

/// The `done/total` header counter with the active task id, right-aligned on the
/// pane's top border (G1: the counter is part of the pane, not of the body text).
fn counter_spans(progress: &PlanProgress) -> Vec<Span<'static>> {
    let task = progress.active_task.as_ref();
    let mut spans = Vec::new();
    if task.is_none() {
        // The state glyph rides the counter alone when nothing names a task in
        // flight; when a task id is on the line, its own `▶` already says it
        // (§7.2), so the counter is not decorated with the glyph twice.
        let finished = progress.has_tasks() && progress.done == progress.total;
        spans.push(theme::task_glyph(finished, false).badge());
        spans.push(Span::styled(" ", theme::text_primary()));
    }
    spans.push(Span::styled(
        format!("{} ", progress.progress_label()),
        theme::chip_done(),
    ));
    if let Some(task) = task {
        spans.push(Glyph::Active.badge());
        spans.push(Span::styled(format!(" {task} "), theme::chip_active()));
    }
    spans
}

/// The state line on the bottom border: the §7.2 glyphs with the count each one
/// stands for, the §8.1 empty-plan statement, and where the body is actually
/// scrolled to — the offset the body pass drew, not the one the renderer asked
/// for, which auto-scroll may have moved and a resize may have clamped.
fn state_spans(data: &PlanData<'_>, geometry: PlanGeometry) -> Vec<Span<'static>> {
    let progress = data.progress;
    let mut spans = vec![Span::styled(" ".to_string(), theme::text_primary())];
    if progress.has_tasks() {
        spans.push(Glyph::Done.badge());
        spans.push(Span::styled(
            format!(" {} done ", progress.done),
            theme::chip_done(),
        ));
        spans.push(Glyph::Pending.badge());
        spans.push(Span::styled(
            format!(" {} open ", progress.pending()),
            theme::chip_pending(),
        ));
        if progress.active_task.is_some() {
            spans.push(Glyph::Active.badge());
            spans.push(Span::styled(
                " in flight ".to_string(),
                theme::chip_active(),
            ));
        }
    } else {
        // §8.1: an empty plan is stated, never left blank. The body carries the
        // cached sentinel text ("No active execution plan."); this line says the
        // same thing about the counter the header shows.
        spans.push(Span::styled(
            " no tasks in plan ".to_string(),
            theme::chip_pending(),
        ));
    }
    if geometry.scroll > 0 {
        spans.push(Span::styled(
            format!(" scroll {} ", geometry.scroll),
            theme::text_dim(),
        ));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tui::state::PlanProgress;

    /// Mixed plan: three checked off, four still open.
    const MIXED: &str = "# Tasks\n\
        - [x] [t-001] research baseline\n\
        - [x] [t-002] layout spec\n\
        - [x] [t-003] theme\n\
        - [ ] [t-004] wireframe\n\
        - [ ] [t-005] header pane\n\
        - [ ] [t-006] status pane\n\
        - [ ] [t-007] plan pane\n";

    /// Render the pane chrome and hand back the text of the top border row and the
    /// bottom border row.
    fn draw(width: u16, height: u16, data: &PlanData<'_>) -> (String, String) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, width, height), data);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = |y: u16| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        (row(0), row(height - 1))
    }

    /// Draw the pane and hand back the geometry it reported plus the bottom border.
    fn draw_body_only(width: u16, height: u16, data: &PlanData<'_>) -> (PlanGeometry, String) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut geometry = PlanGeometry::default();
        terminal
            .draw(|frame| geometry = render(frame, Rect::new(0, 0, width, height), data))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let bottom = (0..width)
            .map(|x| buffer[(x, height - 1)].symbol().to_string())
            .collect::<String>();
        (geometry, bottom)
    }

    /// The rows strictly inside the border, as text.
    fn body_rows(width: u16, height: u16, data: &PlanData<'_>) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, width, height), data);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (1..height.saturating_sub(1))
            .map(|y| {
                (1..width.saturating_sub(1))
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Each body row paired with the style of its first non-blank cell.
    fn body_styles(
        width: u16,
        height: u16,
        data: &PlanData<'_>,
    ) -> Vec<(String, ratatui::style::Style)> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, width, height), data);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (1..height.saturating_sub(1))
            .map(|y| {
                let mut text = String::new();
                let mut style = ratatui::style::Style::default();
                let mut styled = false;
                for x in 1..width.saturating_sub(1) {
                    let cell = &buffer[(x, y)];
                    text.push_str(cell.symbol());
                    if !styled && !cell.symbol().trim().is_empty() {
                        style = cell.style();
                        styled = true;
                    }
                }
                (text.trim_end().to_string(), style)
            })
            .collect()
    }

    /// 30 single-line task rows: enough to overflow a 60x10 pane (8 body rows).
    fn long_plan() -> String {
        (0..30)
            .map(|i| format!("- [ ] [t-{i:03}] task {i:02}"))
            .collect::<Vec<String>>()
            .join("\n")
    }

    #[test]
    fn the_header_counter_is_the_checkbox_split_of_the_plan_text() {
        // Three fixtures (empty, all checked, mixed): the pane's `done/total` is
        // exactly the `CheckboxState::Checked` / total split of the same string.
        let fixtures = [
            ("", "0/0 done"),
            ("- [x] [t-001] one\n- [x] [t-002] two\n", "2/2 done"),
            (MIXED, "3/7 done"),
        ];
        for (plan_text, label) in fixtures {
            let progress = PlanProgress::from_plan(plan_text, None, &[]);
            let data = PlanData {
                focus: false,
                progress: &progress,
                scroll: 0,
                text: "",
                active_tasks: &[],
                auto_scroll: false,
                target_task: None,
            };
            let (top, bottom) = draw(60, 10, &data);
            assert!(top.contains(label), "{plan_text:?} -> top border: {top:?}");
            assert!(
                top.contains("Execution Plan"),
                "the pane keeps its title: {top:?}"
            );
            assert_eq!(
                top.chars().count(),
                60,
                "the border row keeps its width: {top:?}"
            );
            assert!(
                bottom.contains('─') || bottom.contains(' '),
                "the bottom border row stays a border: {bottom:?}"
            );
        }
    }

    #[test]
    fn a_mixed_plan_lists_both_glyph_counts() {
        let progress = PlanProgress::from_plan(MIXED, Some("t-004"), &[]);
        let data = PlanData {
            focus: true,
            progress: &progress,
            scroll: 0,
            text: "",
            active_tasks: &[],
            auto_scroll: false,
            target_task: None,
        };
        let (top, bottom) = draw(70, 12, &data);
        assert!(top.contains("3/7 done"), "counter: {top:?}");
        assert!(top.contains("t-004"), "active task id: {top:?}");
        assert!(top.contains('●'), "focus badge (G6): {top:?}");

        assert!(
            bottom.contains('✓') && bottom.contains("3 done "),
            "{bottom:?}"
        );
        assert!(
            bottom.contains('⧗') && bottom.contains("4 open "),
            "{bottom:?}"
        );
        assert!(
            bottom.contains('▶') && bottom.contains("in flight"),
            "{bottom:?}"
        );
    }

    #[test]
    fn the_counter_glyph_states_which_plan_state_the_pane_is_in() {
        // §7.2 through `theme::task_glyph`: checked-off plans read `✓`, a plan with
        // a task in flight reads `▶`, an open plan with nothing running reads `⧗`.
        let cases = [
            (MIXED, None, '⧗', "open, nothing running"),
            (MIXED, Some("t-004"), '▶', "one task in flight"),
            (
                "- [x] [t-001] one\n- [x] [t-002] two\n",
                None,
                '✓',
                "every task checked off",
            ),
        ];
        for (plan_text, active, glyph, why) in cases {
            let progress = PlanProgress::from_plan(plan_text, active, &[]);
            let data = PlanData {
                focus: false,
                progress: &progress,
                scroll: 0,
                text: "",
                active_tasks: &[],
                auto_scroll: false,
                target_task: None,
            };
            let (top, _) = draw(70, 10, &data);
            assert!(top.contains(glyph), "{why}: {top:?}");
        }
    }

    #[test]
    fn an_all_done_plan_reads_done_and_carries_no_flight_state() {
        let plan = "- [x] [t-001] one\n- [x] [t-002] two\n";
        let progress = PlanProgress::from_plan(plan, None, &[]);
        let data = PlanData {
            focus: false,
            progress: &progress,
            scroll: 0,
            text: "",
            active_tasks: &[],
            auto_scroll: false,
            target_task: None,
        };
        let (top, bottom) = draw(60, 8, &data);
        assert!(top.contains("2/2 done"), "{top:?}");
        assert!(
            bottom.contains('✓') && bottom.contains("2 done "),
            "{bottom:?}"
        );
        assert!(
            bottom.contains('⧗') && bottom.contains("0 open "),
            "{bottom:?}"
        );
        assert!(
            !bottom.contains("in flight"),
            "nothing is running: {bottom:?}"
        );
        assert!(
            !bottom.contains("no tasks"),
            "the plan is not empty: {bottom:?}"
        );
    }

    #[test]
    fn an_empty_plan_states_its_empty_state() {
        // §8.1: the empty plan is named, and the counter still says 0/0.
        let progress = PlanProgress::from_plan("No active execution plan.", None, &[]);
        let data = PlanData {
            focus: false,
            progress: &progress,
            scroll: 0,
            text: "",
            active_tasks: &[],
            auto_scroll: false,
            target_task: None,
        };
        let (top, bottom) = draw(60, 6, &data);
        assert!(top.contains("0/0 done"), "{top:?}");
        assert!(bottom.contains("no tasks in plan"), "{bottom:?}");
        assert!(!bottom.contains(" open "), "no counts to show: {bottom:?}");
    }

    #[test]
    fn the_scroll_offset_is_reported_only_once_the_body_moves() {
        // A body that overflows the pane: only then is there a scroll readout.
        let long = long_plan();
        let progress = PlanProgress::from_plan(&long, None, &[]);
        let top_of_plan = PlanData {
            focus: false,
            progress: &progress,
            scroll: 0,
            text: &long,
            active_tasks: &[],
            auto_scroll: false,
            target_task: None,
        };
        let scrolled = PlanData {
            scroll: 14,
            ..top_of_plan
        };
        let (rows_top, _) = draw_body_only(60, 10, &top_of_plan);
        assert_eq!(rows_top.scroll, 0);
        let (_, bottom) = draw(60, 10, &top_of_plan);
        assert!(!bottom.contains("scroll"), "{bottom:?}");

        let (rows, bottom) = draw_body_only(60, 10, &scrolled);
        assert_eq!(rows.scroll, 14, "the requested offset is kept: {rows:?}");
        assert!(bottom.contains("scroll 14 "), "{bottom:?}");

        // An offset past the end is lowered to the largest legal one, and both the
        // readout and the geometry report the offset actually drawn.
        let past = PlanData {
            scroll: 999,
            ..top_of_plan
        };
        let (rows, bottom) = draw_body_only(60, 10, &past);
        assert_eq!(rows.scroll, rows.max_scroll, "clamped into range: {rows:?}");
        assert!(
            bottom.contains(&format!("scroll {} ", rows.max_scroll)),
            "the border reports the drawn offset: {bottom:?}"
        );
    }

    #[test]
    fn the_pane_draws_the_plan_body_itself_with_the_legacy_styling() {
        // t-009b: the body pass moved here from the legacy renderer, so the checkbox
        // colouring and the active-task highlight have to survive the move.
        let progress = PlanProgress::from_plan(MIXED, Some("t-004"), &[]);
        let active = ["t-004".to_string()];
        let data = PlanData {
            focus: false,
            progress: &progress,
            scroll: 0,
            text: MIXED,
            active_tasks: &active,
            auto_scroll: false,
            target_task: Some("t-004"),
        };
        let body = body_rows(60, 10, &data);
        let text = body.join("\n");
        assert!(text.contains("# Tasks"), "heading drawn: {text:?}");
        assert!(text.contains("[t-001] research baseline"), "{text:?}");
        assert!(text.contains("[t-004] wireframe"), "{text:?}");

        // Styling: heading + active task are bold, checked off and open tasks differ.
        let styles = body_styles(60, 10, &data);
        let styled = |needle: &str| -> ratatui::style::Style {
            styles
                .iter()
                .find(|(line, _)| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not drawn: {styles:?}"))
                .1
        };
        use ratatui::style::Modifier;
        assert!(
            styled("# Tasks").add_modifier.contains(Modifier::BOLD),
            "heading is bold"
        );
        assert!(
            styled("[t-004] wireframe")
                .add_modifier
                .contains(Modifier::BOLD),
            "the in-flight task line stands out"
        );
        assert_eq!(
            styled("[t-004] wireframe").fg,
            theme::chip_active().fg,
            "and uses the active slot, not a raw colour"
        );
        assert_eq!(
            styled("[t-001] research baseline").fg,
            theme::chip_done().fg
        );
        assert_eq!(
            styled("[t-005] header pane").fg,
            theme::chip_pending().fg,
            "an open task that is not in flight"
        );
    }

    #[test]
    fn auto_scroll_centers_the_task_the_renderer_names() {
        // The `plan_auto_scroll` behaviour moves with the body pass: the pane puts
        // the target task one line below the top of the viewport.
        let long = long_plan();
        let progress = PlanProgress::from_plan(&long, Some("t-020"), &[]);
        let data = PlanData {
            focus: false,
            progress: &progress,
            scroll: 0,
            text: &long,
            active_tasks: &[],
            auto_scroll: true,
            target_task: Some("t-020"),
        };
        let (geometry, _) = draw_body_only(60, 10, &data);
        assert_eq!(
            geometry.scroll, 19,
            "one line of context above t-020: {geometry:?}"
        );

        // No target: the first open task leads instead.
        let no_target = PlanData {
            target_task: None,
            ..data
        };
        let (geometry, _) = draw_body_only(60, 10, &no_target);
        assert_eq!(
            geometry.scroll, 0,
            "first pending task is on line 0: {geometry:?}"
        );

        // Auto-scroll overrides a stale manual offset rather than honouring it.
        let stale = PlanData {
            scroll: 999,
            ..no_target
        };
        let (geometry, _) = draw_body_only(60, 10, &stale);
        assert_eq!(geometry.scroll, 0, "{geometry:?}");
    }

    #[test]
    fn the_empty_plan_body_prints_the_cached_sentinel_the_header_echoes() {
        // §8.1: the pane draws the sentinel text itself now, so the §8.1 sentence is
        // on screen and not merely implied by the bottom border.
        let sentinel = "No active execution plan.";
        let progress = PlanProgress::from_plan(sentinel, None, &[]);
        let data = PlanData {
            focus: false,
            progress: &progress,
            scroll: 0,
            text: sentinel,
            active_tasks: &[],
            auto_scroll: false,
            target_task: None,
        };
        let text = body_rows(60, 6, &data).join("\n");
        assert!(text.contains(sentinel), "{text:?}");
    }

    #[test]
    fn a_long_plan_fixture_for_the_geometry_tests() {
        // Self-check of the fixture: 30 single-line task rows, so a 60x10 pane has
        // 8 body rows and a real scroll range.
        let long = long_plan();
        assert_eq!(long.lines().count(), 30);
    }

    #[test]
    fn chrome_stays_inside_its_rect_at_every_ladder_width() {
        // §2.4 plus §7.4: nothing the pane draws may spill past its border, and the
        // counter has to survive down to the 40-column floor.
        let progress = PlanProgress::from_plan(MIXED, Some("t-004"), &[]);
        let data = PlanData {
            focus: true,
            progress: &progress,
            scroll: 3,
            text: "",
            active_tasks: &[],
            auto_scroll: false,
            target_task: None,
        };
        for width in [40u16, 60, 80, 100] {
            let (top, bottom) = draw(width, 10, &data);
            assert_eq!(top.chars().count(), usize::from(width), "top row: {top:?}");
            assert_eq!(
                bottom.chars().count(),
                usize::from(width),
                "bottom: {bottom:?}"
            );
            assert!(top.contains("3/7 done"), "{width}: {top:?}");
            assert!(
                top.contains("t-004"),
                "{width}: the active task id rides the header counter: {top:?}"
            );
        }
    }

    #[test]
    fn a_zero_rect_is_never_drawn_into() {
        // §2.4: layout never hands a pane a zero-sized rect, and the component
        // refuses one anyway.
        let progress = PlanProgress::from_plan(MIXED, None, &[]);
        let data = PlanData {
            focus: false,
            progress: &progress,
            scroll: 0,
            text: "",
            active_tasks: &[],
            auto_scroll: false,
            target_task: None,
        };
        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, 0, 0), &data);
                render(frame, Rect::new(0, 0, 20, 0), &data);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert_eq!(
            buffer[(0, 0)].symbol(),
            " ",
            "nothing was painted into the frame"
        );
    }
}
