//! Header row — spec §3 (Header pane), §2.2 chip order, §7.4 drop ladder, §8.1
//! (plan empty state) and G1: `done/total` plus the active task id are rendered in
//! every frame at 40x10, plan pane open or not.
//!
//! The counters come from [`PlanProgress`], derived once per frame from the very
//! `plan_content` string the plan pane draws, so the header and the pane can never
//! disagree about the state of the plan.
//!
//! `HeaderData` reaches this pane with six values. The two §2.2 fields that were
//! not plumbed when this pane was first written landed in t-009b:
//!
//! * **`ctx %`** — `ctx_tokens` (the numerator) and `ctx_limit`
//!   (`config.max_context_tokens`, spec §2.2) both arrive, so the chip prints the
//!   real percentage. Only if the renderer never saw a config (`ctx_limit == 0`)
//!   does it fall back to the token count, because a percentage of an unknown
//!   budget would be an invented number.
//! * **elapsed** — the renderer hands over its own session clock
//!   ([`HeaderData::elapsed`]); the pane no longer reaches for a process-wide or
//!   plan-file clock, so it can never print a duration that outlived the session.
//!
//! `val N` is absent by rule, not by truncation: plumbing delta D1 (§9.3) has not
//! landed, and an arbitration round counter is not data at all (spec §8.4).

use super::fit_spans;
use super::status_bar::format_count;
use crate::ui::tui::layout::HeaderBudget;
use crate::ui::tui::state::{PlanProgress, ViewMode};
use crate::ui::tui::theme::{self, Glyph};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

/// What the header may draw this frame.
#[derive(Debug, Clone, Copy)]
pub struct HeaderData<'a> {
    /// The mode actually drawn (`compute_layout` decides after the §2.5 ladder).
    pub mode: ViewMode,
    /// Derived plan progress.
    pub plan: &'a PlanProgress,
    /// Which optional chips survive the current width (§7.4 drop order).
    pub budget: HeaderBudget,
    /// Orchestrator context tokens — the numerator of `ctx %` (spec §2.2).
    pub ctx_tokens: usize,
    /// `config.max_context_tokens` — the denominator (spec §2.2). `0` means the
    /// renderer never saw a config, in which case the chip prints the raw count
    /// rather than inventing a percentage.
    pub ctx_limit: usize,
    /// The renderer-owned session clock in whole seconds (`t-009b`), `None` while
    /// no session clock exists yet.
    pub elapsed: Option<u64>,
}

/// The §7.4 drop ladder, lowest priority first: `val` → `ctx` → `elapsed` → mode
/// name → `> t-NNN`.
///
/// `val` has no variant because it is never on the line to be dropped (D1 gate,
/// §9.3). `done/total` and the app name are never dropped at any width (G1), so
/// they carry no rank at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Drop {
    /// `ctx …`.
    Ctx,
    /// The project clock.
    Elapsed,
    /// The mode name (`M4 Split`).
    Mode,
    /// The active task id (`▶ t-004`).
    Task,
}

/// One header field: the spans it contributes, and its place in the ladder
/// (`None` = never dropped).
#[derive(Debug, Clone)]
struct Field {
    drop: Option<Drop>,
    spans: Vec<Span<'static>>,
}

/// Draw the one header row (no border: `Length(1)` in §2.1 leaves no room for
/// one) with the chip order of §2.2 and the drop ladder of §7.4.
pub fn render(frame: &mut Frame, rect: Rect, data: &HeaderData<'_>) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    // The clock is the renderer's own session clock (`t-009b`): the pane no longer
    // reaches for a process-wide or plan-file clock, so a frame can never print a
    // duration that outlived the session it belongs to.
    let elapsed = if data.budget.show_elapsed {
        data.elapsed
    } else {
        None
    };
    let line = Line::from(header_line(data, elapsed, rect.width));
    frame.render_widget(Paragraph::new(line), rect);
}

/// The header spans for `width`, with the clock supplied by the caller.
///
/// Split out from [`render`] so the ladder is a pure function of the values that
/// reach the pane — which is what makes the §7.4 order testable at 100, 90 and 70
/// columns without driving the whole render loop.
pub(crate) fn header_line(
    data: &HeaderData<'_>,
    elapsed: Option<u64>,
    width: u16,
) -> Vec<Span<'static>> {
    let mut fields = fields(data, elapsed);
    drop_to_fit(&mut fields, usize::from(width));
    let spans: Vec<Span<'static>> = fields.into_iter().flat_map(|field| field.spans).collect();
    // Whatever is left after the ladder still has to fit the rect: a clipped line
    // ends in `…` rather than spilling past the frame edge (§7.4).
    fit_spans(spans, width)
}

/// Build the header fields in the display order of §2.2: brand, mode name,
/// `done/total`, active task id, (`val N`), `ctx …`, elapsed.
fn fields(data: &HeaderData<'_>, elapsed: Option<u64>) -> Vec<Field> {
    let mut fields = vec![Field {
        drop: None,
        spans: vec![Span::styled(" Marmel".to_string(), theme::chip_done())],
    }];
    if data.budget.show_mode {
        fields.push(Field {
            drop: Some(Drop::Mode),
            spans: vec![Span::styled(
                format!(" {}", data.mode.label()),
                theme::text_dim(),
            )],
        });
    }
    fields.push(Field {
        // G1: `done/total` is never dropped, at any width.
        drop: None,
        spans: vec![
            Span::raw(" "),
            Glyph::Done.badge(),
            Span::styled(
                format!(" {} ", data.plan.progress_label()),
                theme::chip_done(),
            ),
        ],
    });
    if data.budget.show_task
        && let Some(task) = data.plan.active_task.as_ref()
    {
        fields.push(Field {
            drop: Some(Drop::Task),
            spans: vec![
                Glyph::Active.badge(),
                Span::styled(format!(" {task}"), theme::chip_active()),
            ],
        });
    }
    // `val N` is deliberately not emitted here: per-worker `validation_rounds` is
    // still not projected into the UI row (plumbing delta D1, §9.3), and
    // `layout::header_budget` keeps `show_val` false for the same reason. The
    // arbitration round counter is never a candidate either (§8.4).
    if data.budget.show_ctx
        && let Some(chip) = ctx_chip(data)
    {
        fields.push(Field {
            drop: Some(Drop::Ctx),
            spans: vec![Span::styled(format!(" {chip}"), theme::text_dim())],
        });
    }
    if data.budget.show_elapsed
        && let Some(secs) = elapsed
    {
        fields.push(Field {
            drop: Some(Drop::Elapsed),
            spans: vec![Span::styled(
                format!(
                    " elapsed {}",
                    crate::orchestrator::workers::format_duration_human(secs)
                ),
                theme::text_dim(),
            )],
        });
    }
    fields
}

/// The `ctx …` chip (spec §2.2 line 82): `orchestrator_context_tokens` ÷
/// `config.max_context_tokens` as a percentage. Without the denominator the chip
/// degrades to the raw token count it printed before t-009b — a percentage of an
/// unknown budget would be an invented number, and no ctx chip at all is printed
/// when there is no context to report.
fn ctx_chip(data: &HeaderData<'_>) -> Option<String> {
    if data.ctx_tokens == 0 {
        return None;
    }
    if data.ctx_limit == 0 {
        return Some(format!("ctx {}", format_count(data.ctx_tokens)));
    }
    let pct = (data.ctx_tokens.saturating_mul(100) / data.ctx_limit).min(100);
    Some(format!("ctx {pct}%"))
}

/// Drop fields in ladder order until the line fits `budget`, so a narrow header
/// loses one chip at a time instead of collapsing into an ellipsis (§7.4, §2.5).
fn drop_to_fit(fields: &mut Vec<Field>, budget: usize) {
    while fields_width(fields) > budget {
        let Some(worst) = fields.iter().filter_map(|field| field.drop).min() else {
            break;
        };
        fields.retain(|field| field.drop != Some(worst));
    }
}

/// Cells the field set occupies, measured with `unicode-width` (§7.2: glyph widths
/// are measured, never assumed to be one cell).
fn fields_width(fields: &[Field]) -> usize {
    fields
        .iter()
        .flat_map(|field| &field.spans)
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tui::layout::header_budget;
    use crate::ui::tui::state::PlanProgress;

    /// Mixed plan fixture: three checked, four unchecked, one task in flight.
    const MIXED: &str = "# Tasks\n\
        - [x] [t-001] research baseline\n\
        - [x] [t-002] layout spec\n\
        - [x] [t-003] theme\n\
        - [ ] [t-004] wireframe\n\
        - [ ] [t-005] header pane\n\
        - [ ] [t-006] status pane\n\
        - [ ] [t-007] plan pane\n";

    fn progress(text: &str, active: Option<&str>) -> PlanProgress {
        PlanProgress::from_plan(text, active, &[])
    }

    /// Join a span run back into plain text for assertions.
    fn text(spans: &[Span<'static>]) -> String {
        spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    /// Render the header into a `width x 1` test terminal and return the row.
    fn draw(width: u16, data: &HeaderData<'_>) -> String {
        let backend = ratatui::backend::TestBackend::new(width, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, width, 1), data))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn a_hundred_columns_show_the_widest_chip_set() {
        // §2.2: brand, mode, done/total, active task, ctx, elapsed.
        let plan = progress(MIXED, Some("t-004"));
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(100),
            ctx_tokens: 12_400,
            ctx_limit: 0,
            elapsed: None,
        };
        let line = text(&header_line(&data, Some(723), 100));
        assert!(line.contains("Marmel"), "{line:?}");
        assert!(line.contains("M4 Split"), "{line:?}");
        assert!(line.contains('✓'), "{line:?}");
        assert!(line.contains("3/7 done"), "{line:?}");
        assert!(line.contains('▶') && line.contains("t-004"), "{line:?}");
        assert!(line.contains("ctx 12.4k"), "{line:?}");
        assert!(line.contains("elapsed 12m 3s"), "{line:?}");
        assert!(
            UnicodeWidthStr::width(line.as_str()) <= 100,
            "100 columns: {line:?}"
        );
    }

    #[test]
    fn ninety_columns_still_show_everything_that_fits() {
        let plan = progress(MIXED, Some("t-004"));
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(90),
            ctx_tokens: 12_400,
            ctx_limit: 0,
            elapsed: None,
        };
        let line = text(&header_line(&data, Some(723), 90));
        assert!(
            line.contains("M4 Split") && line.contains("3/7 done"),
            "{line:?}"
        );
        assert!(
            line.contains("t-004") && line.contains("ctx 12.4k"),
            "{line:?}"
        );
        assert!(line.contains("elapsed 12m 3s"), "{line:?}");
    }

    #[test]
    fn seventy_columns_degrade_one_chip_at_a_time_in_ladder_order() {
        // §2.5 (`width < 90` collapses the body) plus §7.4: the header sheds `ctx`,
        // then the clock, then the mode name, then the task id — and never the
        // progress counter. A long task id and a 7-figure ctx count are what push
        // the line past 70 columns.
        let plan = progress(MIXED, Some("t-004-wireframe-pane-rebuild"));
        let budget = header_budget(70);
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget,
            ctx_tokens: 1_234_567,
            ctx_limit: 0,
            elapsed: None,
        };
        assert!(budget.show_ctx && budget.show_elapsed && budget.show_mode);

        let at_70 = text(&header_line(&data, Some(723), 70));
        assert!(!at_70.contains("ctx "), "ctx is first out: {at_70:?}");
        assert!(!at_70.contains("elapsed"), "then the clock: {at_70:?}");
        assert!(
            at_70.contains("M4 Split"),
            "the mode name outlives both: {at_70:?}"
        );
        assert!(
            at_70.contains("t-004"),
            "the task id outlives the mode name: {at_70:?}"
        );
        assert!(
            UnicodeWidthStr::width(at_70.as_str()) <= 70,
            "70 columns: {at_70:?}"
        );

        let at_56 = text(&header_line(&data, Some(723), 56));
        assert!(
            !at_56.contains("M4 Split"),
            "the mode name goes next: {at_56:?}"
        );
        assert!(at_56.contains("t-004"), "{at_56:?}");

        let at_40 = text(&header_line(&data, Some(723), 40));
        assert!(
            !at_40.contains("t-004"),
            "the task id is last out: {at_40:?}"
        );
        assert!(
            at_40.contains("3/7 done"),
            "done/total is never dropped: {at_40:?}"
        );

        // Ladder invariant while narrowing: a chip that has left never comes back.
        let mut previous: Option<String> = None;
        for width in [70u16, 64, 56, 48, 40] {
            let line = text(&header_line(&data, Some(723), width));
            assert!(line.contains("Marmel"), "{width}: {line:?}");
            assert!(
                line.contains("3/7 done"),
                "{width}: done/total is never dropped (G1): {line:?}"
            );
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= usize::from(width),
                "{width}: header exceeds its rect: {line:?}"
            );
            if let Some(older) = &previous {
                for chip in ["ctx ", "elapsed", "M4 Split", "t-004"] {
                    if older.contains(chip) {
                        continue;
                    }
                    assert!(
                        !line.contains(chip),
                        "{chip} came back at {width} columns: {older:?} -> {line:?}"
                    );
                }
            }
            previous = Some(line);
        }
    }

    #[test]
    fn progress_counter_matches_the_checkbox_split_of_the_plan_text() {
        // Acceptance criterion 2: `done/total` is exactly the checked/total split
        // of the same plan string the plan pane draws.
        let fixtures = [
            ("", 0, 0),
            ("- [x] [t-001] done one\n- [x] [t-002] done two\n", 2, 2),
            (MIXED, 3, 7),
        ];
        for (plan_text, done, total) in fixtures {
            let plan = progress(plan_text, None);
            let data = HeaderData {
                mode: ViewMode::Transcript,
                plan: &plan,
                budget: header_budget(100),
                ctx_tokens: 0,
                ctx_limit: 0,
                elapsed: None,
            };
            let line = text(&header_line(&data, None, 100));
            assert_eq!(plan.done, done);
            assert_eq!(plan.total, total);
            assert!(
                line.contains(&format!("{done}/{total} done")),
                "{plan_text:?} -> {line:?}"
            );
        }
    }

    #[test]
    fn an_empty_plan_still_carries_the_counter_and_no_task_chip() {
        // §8.1 plus G1: an absent plan is stated, never hidden.
        let plan = progress("", None);
        let data = HeaderData {
            mode: ViewMode::Transcript,
            plan: &plan,
            budget: header_budget(40),
            ctx_tokens: 0,
            ctx_limit: 0,
            elapsed: None,
        };
        let line = text(&header_line(&data, None, 40));
        assert!(line.contains("0/0 done"), "{line:?}");
        assert!(!line.contains('▶'), "no task id to show: {line:?}");
    }

    #[test]
    fn the_header_never_prints_a_validation_or_arbitration_round() {
        // Gate (c): `val N` waits for D1, and the arbitration round is not data.
        let plan = progress(MIXED, Some("t-004"));
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(120),
            ctx_tokens: 12_400,
            ctx_limit: 0,
            elapsed: None,
        };
        for width in [40u16, 60, 90, 120] {
            let line = text(&header_line(&data, Some(723), width));
            assert!(!line.contains("val"), "{width}: {line:?}");
            assert!(!line.contains("round"), "{width}: {line:?}");
        }
    }

    #[test]
    fn done_total_and_the_active_task_survive_into_a_40x1_frame() {
        // G1, asserted through the actual widget rather than the span builder.
        let plan = progress(MIXED, Some("t-004"));
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(40),
            ctx_tokens: 12_400,
            ctx_limit: 0,
            elapsed: None,
        };
        let row = draw(40, &data);
        assert!(row.contains("Marmel"), "{row:?}");
        assert!(row.contains("3/7 done"), "{row:?}");
        assert!(
            !row.contains("ctx "),
            "40 cols is below the ctx band: {row:?}"
        );
    }

    #[test]
    fn the_ctx_chip_is_a_real_percentage_once_the_budget_is_plumbed() {
        // §2.2 line 82: `ctx %` is `orchestrator_context_tokens` /
        // `config.max_context_tokens`. t-009b plumbed the denominator, so the
        // chip must print the ratio and never a made-up one.
        let plan = progress(MIXED, Some("t-004"));
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(120),
            ctx_tokens: 12_400,
            ctx_limit: 30_000,
            elapsed: None,
        };
        let line = text(&header_line(&data, None, 120));
        assert!(line.contains("ctx 41%"), "{line:?}");
        assert!(!line.contains("12.4k"), "count is not a percent: {line:?}");

        // A context larger than the budget still cannot claim more than 100%.
        let over = HeaderData {
            ctx_tokens: 99_000,
            ctx_limit: 30_000,
            ..data
        };
        assert!(text(&header_line(&over, None, 120)).contains("ctx 100%"));

        // Without a budget the chip degrades to the honest token count.
        let blind = HeaderData {
            ctx_limit: 0,
            ..data
        };
        assert!(text(&header_line(&blind, None, 120)).contains("ctx 12.4k"));
    }

    #[test]
    fn the_elapsed_chip_prints_the_renderer_session_clock_it_is_handed() {
        // t-009b: the renderer owns the clock; the pane must draw exactly what it
        // receives, and print nothing when there is no session clock.
        let plan = progress(MIXED, Some("t-004"));
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(120),
            ctx_tokens: 0,
            ctx_limit: 0,
            elapsed: Some(723),
        };
        let row = draw(120, &data);
        assert!(row.contains("elapsed 12m 3s"), "{row:?}");

        let idle = HeaderData {
            elapsed: None,
            ..data
        };
        assert!(!draw(120, &idle).contains("elapsed"), "no clock, no chip");
    }

    #[test]
    fn a_zero_rect_is_never_drawn_into() {
        // §2.4: no component renders into a zero-sized rect.
        let plan = progress(MIXED, Some("t-004"));
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(100),
            ctx_tokens: 0,
            ctx_limit: 0,
            elapsed: None,
        };
        let backend = ratatui::backend::TestBackend::new(20, 1);
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
