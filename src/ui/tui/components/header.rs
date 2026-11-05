//! Header row — spec §3 (Header pane), §2.2 chip order, §7.4 drop ladder, §8.1
//! (plan empty state) and G1: `done/total` plus the active task id are rendered in
//! every frame at 40x10, plan pane open or not.
//!
//! The counters come from [`PlanProgress`], derived once per frame from the very
//! `plan_content` string the plan pane draws, so the header and the pane can never
//! disagree about the state of the plan.
//!
//! `HeaderData` reaches this pane with seven values. The two §2.2 fields that were
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
//! `val N` is the registry's validation round and appears only once plumbing delta
//! D1 (§9.3) has landed — the renderer folds `WorkerEntry::validation_rounds` onto
//! the worker row and hands the maximum over the rows to [`HeaderData::val_rounds`].
//! With no round recorded (`val_rounds == 0`) the chip is withheld. An arbitration
//! round counter is never a candidate for it (spec §8.4).

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
    /// The highest `validation_rounds` across the projected worker rows — the
    /// `val N` chip plumbing delta D1 (§9.3) unblocked. `0` means the registry
    /// recorded no validation round, and the chip is withheld instead of printing
    /// `val 0`. Never an arbitration round (§8.4).
    pub val_rounds: usize,
}

/// The field the post-budget width-fit pass drops next, lowest priority first:
/// `val` → `ctx` → `elapsed` → mode name → `> t-NNN`.
///
/// `Val` is on the ladder now that plumbing delta D1 (§9.3) put `validation_rounds`
/// on the worker row: it is the first chip to go, exactly as §7.4 orders the row.
/// `done/total` and the app name are never dropped at any width (G1), so
/// they carry no rank at all.
///
/// **The budget ladder stops at the mode name.** `layout::header_budget` keeps
/// `show_task: true` at every width down to the 40-column floor (§7.4 tail, §10.1), so
/// `Drop::Task` only ever fires here, in [`drop_to_fit`], and only for an id that
/// cannot share the row with the never-dropped chips: an ordinary `t-NNN` id is on the
/// header line at 40 columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Drop {
    /// `val N` — the registry's validation round (D1).
    Val,
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
        // The wordmark owns its own slot: painting it with the success hue made a
        // green corner mean "done" and "the logo" at the same time (§F5).
        spans: vec![Span::styled(" Marmel".to_string(), theme::brand())],
    }];
    if data.budget.show_mode {
        fields.push(Field {
            drop: Some(Drop::Mode),
            spans: vec![Span::styled(
                format!(" {}", data.mode.label()),
                theme::text_muted(),
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
                theme::state_done(),
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
                Span::styled(format!(" {task}"), theme::state_running()),
            ],
        });
    }
    // `val N` — the registry's validation round, max over the projected rows
    // (plumbing delta D1, §9.3). Two gates: the §7.4 budget has to keep the chip, and
    // the registry has to have recorded a round. An arbitration round counter is never
    // a candidate here (§8.4) — this number comes from `WorkerEntry::validation_rounds`
    // and nowhere else.
    if data.budget.show_val && data.val_rounds > 0 {
        fields.push(Field {
            drop: Some(Drop::Val),
            spans: vec![Span::styled(
                format!(" val {}", data.val_rounds),
                theme::state_replan(),
            )],
        });
    }
    if data.budget.show_ctx
        && let Some(chip) = ctx_chip(data)
    {
        fields.push(Field {
            drop: Some(Drop::Ctx),
            spans: vec![Span::styled(format!(" {chip}"), theme::text_muted())],
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
                theme::text_muted(),
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
///
/// This is the post-budget pass: `layout::header_budget` has already had its say, and
/// at every supported width it kept the task id. The ladder here therefore ends with
/// [`Drop::Task`], and reaches it only when the assembled row still overruns the rect
/// after `val`, `ctx`, `elapsed` and the mode name have all gone.
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

            val_rounds: 0,
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

            val_rounds: 0,
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

    /// §7.4 (as corrected in `c6794a6`) and §10.1: the ladder is **exactly**
    /// `val` → `ctx` → `elapsed` → mode name, and it stops there. `done/total` and the
    /// active task id are never ladder casualties; the only thing that can still take
    /// the id away is the width-*fit* pass, for an id too long to sit beside the
    /// never-dropped chips — pinned by
    /// [`an_over_long_task_id_is_the_only_fit_pass_casualty`].
    #[test]
    fn the_header_ladder_sheds_val_then_ctx_then_elapsed_then_the_mode_name() {
        // The fit pass re-runs the §7.4 order *after* the budget, so the row is built
        // with the 70-column budget (every chip licensed) and then fitted to narrower
        // rects — that is the only way the ladder's order is observable at all, since
        // the budget itself takes the whole optional band away below 70 columns at
        // once. An id long enough that the mode name has to pay for it at 40 columns,
        // a 7-figure ctx count and a recorded validation round put every droppable
        // chip on the row.
        let id = "t-017d-header-pane";
        let plan = progress(MIXED, Some(id));
        let budget = header_budget(70);
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget,
            ctx_tokens: 1_234_567,
            ctx_limit: 0,
            elapsed: None,

            val_rounds: 2,
        };
        assert!(
            budget.show_val && budget.show_ctx && budget.show_elapsed && budget.show_mode,
            "the fit pass, not the budget, is under test: {budget:?}",
        );
        assert!(
            budget.show_task,
            "§7.4: `show_task` is true at every width down to the 40-column floor",
        );

        // Read the ladder off the sweep: the widest terminal at which each chip was
        // still on the row. The chip dropped first has the *largest* such width.
        let ladder = [
            ("val", "val 2"),
            ("ctx", "ctx "),
            ("elapsed", "elapsed"),
            ("mode", "M4 Split"),
        ];
        let mut left_at: Vec<(&str, u16)> = Vec::new();
        let mut gone: Vec<&str> = Vec::new();
        for width in (40..=80u16).rev() {
            let line = text(&header_line(&data, Some(723), width));
            assert!(line.contains("Marmel"), "{width}: {line:?}");
            assert!(
                line.contains("3/7 done"),
                "{width}: done/total is never dropped (G1): {line:?}",
            );
            assert!(
                line.contains('▶') && line.contains(id),
                "{width}: the active task id is never a ladder casualty (§7.4, §10.1): {line:?}",
            );
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= usize::from(width),
                "{width}: header exceeds its rect: {line:?}",
            );
            // Ladder invariant while narrowing: a chip that has left never comes back.
            for (chip, probe) in ladder {
                if line.contains(probe) {
                    assert!(
                        !gone.contains(&chip),
                        "{chip} came back at {width} columns: {line:?}",
                    );
                } else if !gone.contains(&chip) {
                    gone.push(chip);
                    left_at.push((chip, width));
                }
            }
        }
        assert_eq!(
            left_at.iter().map(|(chip, _)| *chip).collect::<Vec<_>>(),
            ["val", "ctx", "elapsed", "mode"],
            "§7.4 drop order, exactly, with the width each chip left at: {left_at:?}",
        );
        for pair in left_at.windows(2) {
            assert!(
                pair[0].1 > pair[1].1,
                "one chip at a time, in ladder order: {left_at:?}",
            );
        }

        // The named widths, spelled out.
        let at_60 = text(&header_line(&data, Some(723), 60));
        assert!(
            !at_60.contains("val") && !at_60.contains("ctx ") && !at_60.contains("elapsed"),
            "the round, the context and the clock are out by 60: {at_60:?}",
        );
        assert!(
            at_60.contains("M4 Split") && at_60.contains(id),
            "the mode name and the task id outlive all three: {at_60:?}",
        );

        // The 40-column floor: the mode name is the ladder's last casualty, and the
        // counter plus the task id are what is left.
        let at_40 = text(&header_line(&data, Some(723), 40));
        assert!(
            !at_40.contains("M4 Split"),
            "the mode name is the last thing the ladder takes: {at_40:?}",
        );
        assert!(
            at_40.contains("Marmel") && at_40.contains("3/7 done") && at_40.contains("▶ t-017d"),
            "brand, counter and task id carry the 40-column header (§10.1): {at_40:?}",
        );
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

                val_rounds: 0,
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

            val_rounds: 0,
        };
        let line = text(&header_line(&data, None, 40));
        assert!(line.contains("0/0 done"), "{line:?}");
        assert!(!line.contains('▶'), "no task id to show: {line:?}");
    }

    /// The §2.2 `val N` chip once plumbing delta D1 (§9.3) landed: the number comes
    /// from the projection and from nowhere else, and the arbitration round is still
    /// never a candidate (§8.4, DATA gap 9).
    #[test]
    fn the_val_chip_prints_the_projected_round_and_never_a_round_word() {
        let plan = progress(MIXED, Some("t-004"));
        let reviewed = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(120),
            ctx_tokens: 12_400,
            ctx_limit: 0,
            elapsed: None,
            val_rounds: 2,
        };
        let line = text(&header_line(&reviewed, Some(723), 120));
        assert!(line.contains("val 2"), "{line:?}");

        // No round recorded: the chip is withheld rather than printed as `val 0`.
        let unreviewed = HeaderData {
            val_rounds: 0,
            ..reviewed
        };
        for width in [40u16, 60, 90, 120] {
            let line = text(&header_line(&unreviewed, Some(723), width));
            assert!(!line.contains("val"), "{width}: {line:?}");
        }

        // Neither spelling of "round" reaches the header at any width, with a round
        // on the row or without one.
        for data in [&reviewed, &unreviewed] {
            for width in [40u16, 60, 70, 90, 120] {
                let line = text(&header_line(data, Some(723), width));
                assert!(!line.contains("round"), "{width}: {line:?}");
            }
        }
    }

    /// §7.4 after D1: `val` is the first chip the narrow header sheds — ahead of
    /// `ctx`, then the clock — while `done/total` and the brand stay (G1).
    #[test]
    fn the_val_chip_is_the_first_to_leave_the_narrow_header() {
        // A long task id and a 7-figure context count are what push the row past
        // the width where the ladder has to start dropping.
        let plan = progress(MIXED, Some("t-004-wireframe-pane-rebuild"));
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(120),
            ctx_tokens: 1_234_567,
            ctx_limit: 0,
            elapsed: None,
            val_rounds: 2,
        };

        let at_120 = text(&header_line(&data, Some(723), 120));
        assert!(
            at_120.contains("val 2") && at_120.contains("ctx ") && at_120.contains("elapsed"),
            "{at_120:?}"
        );

        // Read the ladder off the sweep: the widest terminal at which each chip was
        // still on the row. A chip dropped first has the *largest* such width.
        let mut left_at: Vec<(&str, u16)> = Vec::new();
        for width in (40..=120u16).rev() {
            let line = text(&header_line(&data, Some(723), width));
            assert!(line.contains("Marmel"), "{width}: {line:?}");
            assert!(line.contains("3/7 done"), "{width}: {line:?}");
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= usize::from(width),
                "{width}: header exceeds its rect: {line:?}"
            );
            for (chip, probe) in [
                ("val", "val 2"),
                ("ctx", "ctx "),
                ("elapsed", "elapsed"),
                ("mode", "M4 Split"),
            ] {
                if !line.contains(probe) && !left_at.iter().any(|(seen, _)| *seen == chip) {
                    left_at.push((chip, width));
                }
            }
        }
        assert_eq!(
            left_at.iter().map(|(chip, _)| *chip).collect::<Vec<_>>(),
            ["val", "ctx", "elapsed", "mode"],
            "§7.4 header ladder: {left_at:?}"
        );
        for pair in left_at.windows(2) {
            assert!(pair[0].1 > pair[1].1, "one chip at a time: {left_at:?}");
        }
        // §7.4's tail: the mode name is the last chip the ladder touches. The id here
        // is 28 characters long, so the width-*fit* pass — never the budget — takes it
        // below 49 columns; see `an_over_long_task_id_is_the_only_fit_pass_casualty`.
        let at_50 = text(&header_line(&data, Some(723), 50));
        assert!(
            !at_50.contains("M4 Split") && at_50.contains("t-004-wireframe-pane-rebuild"),
            "the mode name pays for the row before the id does: {at_50:?}"
        );
    }

    #[test]
    fn done_total_and_the_active_task_survive_into_a_40x1_frame() {
        // G1 plus §10.1 as corrected in `c6794a6`, asserted through the actual widget
        // rather than the span builder: at the 40-column floor the header carries the
        // counter **and** the active task id, and only the optional band is gone.
        let plan = progress(MIXED, Some("t-004"));
        let budget = header_budget(40);
        assert!(
            budget.show_task,
            "§7.4: `header_budget` never drops the task id, not even at the 40-col floor"
        );
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget,
            ctx_tokens: 12_400,
            ctx_limit: 0,
            elapsed: None,

            val_rounds: 0,
        };
        let row = draw(40, &data);
        assert!(row.contains("Marmel"), "{row:?}");
        assert!(row.contains("3/7 done"), "{row:?}");
        assert!(
            row.contains('▶') && row.contains("t-004"),
            "the active task id is on the 40-column header line (§10.1): {row:?}"
        );
        assert!(
            !row.contains("ctx "),
            "40 cols is below the ctx band: {row:?}"
        );
    }

    /// §7.4/§10.1 as corrected in `c6794a6`, read through the **real** per-width
    /// budget: at every supported width down to the 40-column floor the header carries
    /// both `done/total` and the active task id, and what the budget gives up is the
    /// optional band (`val`, `ctx`, `elapsed`, mode name) — never the id.
    #[test]
    fn the_task_id_is_on_the_header_line_at_every_supported_width() {
        let plan = progress(MIXED, Some("t-004"));
        for width in [40u16, 48, 60, 69, 70, 90, 100, 120] {
            let budget = header_budget(width);
            assert!(
                budget.show_task,
                "{width}: `header_budget` keeps the task id (§7.4)",
            );
            let data = HeaderData {
                mode: ViewMode::Split,
                plan: &plan,
                budget,
                ctx_tokens: 12_400,
                ctx_limit: 30_000,
                elapsed: None,

                val_rounds: 2,
            };
            let line = text(&header_line(&data, Some(723), width));
            assert!(
                line.contains('▶') && line.contains("t-004"),
                "{width} cols: the active task id is never a budget casualty (§10.1): {line:?}",
            );
            assert!(
                line.contains("Marmel") && line.contains("3/7 done"),
                "{width} cols: brand and counter are never dropped (G1): {line:?}",
            );
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= usize::from(width),
                "{width}: header exceeds its rect: {line:?}",
            );
            // The chips the budget may give up appear exactly when it licenses them,
            // so the ladder never silently outruns `header_budget`.
            for (chip, probe, allowed) in [
                ("val", "val 2", budget.show_val),
                ("ctx", "ctx 41%", budget.show_ctx),
                ("elapsed", "elapsed", budget.show_elapsed),
                ("mode", "M4 Split", budget.show_mode),
            ] {
                assert_eq!(
                    line.contains(probe),
                    allowed,
                    "{width} cols: {chip} is not the budget's to drop: {line:?}",
                );
            }
        }
    }

    /// §7.4's tail, stated the way `c6794a6` settled it: the *budget* never drops the
    /// task id, so its absence at a narrow width can only come from the post-budget
    /// width-fit pass, and only for an id too long to sit beside the never-dropped
    /// chips. The `header_budget` field set is asserted pre-fit to prove the budget
    /// is not the culprit.
    #[test]
    fn an_over_long_task_id_is_the_only_fit_pass_casualty() {
        let long_id = "t-004-wireframe-pane-rebuild";
        let plan = progress(MIXED, Some(long_id));
        let data = HeaderData {
            mode: ViewMode::Split,
            plan: &plan,
            budget: header_budget(40),
            ctx_tokens: 12_400,
            ctx_limit: 0,
            elapsed: None,

            val_rounds: 2,
        };

        // Pre-fit, at the 40-column budget: the whole optional band is already gone and
        // the id is still on the row. Nothing dropped the task id but the fit pass.
        let pre_fit = fields(&data, Some(723));
        assert_eq!(
            pre_fit.iter().map(|field| field.drop).collect::<Vec<_>>(),
            vec![None, None, Some(Drop::Task)],
            "brand and counter carry no rank; the id keeps the last one",
        );
        let needed = u16::try_from(fields_width(&pre_fit)).expect("a header row within u16");
        assert!(
            needed > 40,
            "the fixture has to overrun the floor for this test to mean anything: {needed}",
        );

        // The fit pass is a pure width decision: one cell enough and the id stays.
        let exactly_enough = text(&header_line(&data, Some(723), needed));
        assert!(
            exactly_enough.contains("▶ ") && exactly_enough.contains(long_id),
            "{needed} cols is enough for this id: {exactly_enough:?}",
        );
        let one_cell_short = text(&header_line(&data, Some(723), needed - 1));
        assert!(
            one_cell_short.contains("Marmel") && one_cell_short.contains("3/7 done"),
            "the never-dropped chips stay regardless: {one_cell_short:?}",
        );
        assert!(
            !one_cell_short.contains(long_id),
            "an id that cannot share the row is the one casualty of the fit pass: {one_cell_short:?}",
        );

        // And the fit pass still reaches the id last: with the optional band licensed,
        // the mode name is taken before the id is, at a width this id does fit at.
        let licensed = HeaderData {
            budget: header_budget(70),
            ..data
        };
        let at_50 = text(&header_line(&licensed, Some(723), 50));
        assert!(
            !at_50.contains("M4 Split") && at_50.contains(long_id),
            "the mode name pays before the id: {at_50:?}",
        );

        // An ordinary `t-NNN` id at the very same 40-column budget: present, and not
        // even clipped. This is the case §10.1 and the 40x10 frame of
        // `docs/new_ui_preview.txt` describe.
        let ordinary = progress(MIXED, Some("t-004"));
        let short = HeaderData {
            plan: &ordinary,
            ..data
        };
        let at_40 = text(&header_line(&short, Some(723), 40));
        assert!(
            at_40.contains("▶ t-004") && at_40.contains("3/7 done") && at_40.contains("Marmel"),
            "an ordinary task id survives the 40-column floor: {at_40:?}",
        );
        assert!(
            !at_40.contains(theme::ELLIPSIS),
            "nothing had to be clipped: {at_40:?}",
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

            val_rounds: 0,
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

            val_rounds: 0,
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

            val_rounds: 0,
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
