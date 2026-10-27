//! Pane geometry — spec §2 (layout spec), §2.4 (zero-rect rule) and §2.5 (the
//! narrow-terminal degradation ladder).
//!
//! [`compute_layout`] is a pure function over the frame area plus the
//! [`UiView`]: it never touches the terminal and never returns a zero-sized pane
//! rect. A pane that the mode (or the ladder) removes is `None` — never split,
//! never drawn, never hit-tested (§2.4.1).
//!
//! The mode and the focus it reads are the renderer's **stored** view state
//! (`TuiRenderer::view_mode` / `focus`, moved only by the transition helpers),
//! never a reading of what a pane is showing — so a mode change is a geometry
//! change on the very next frame, and the [`PaneRects::mode`] a frame reports is
//! the mode the frame was built from, including where the §2.5 ladder overrides
//! it.
//!
//! **Slot assignment — one global rule (t-024b).** In every shape that draws both
//! the Plan and the Transcript, the Execution Plan pane owns the **leading** slot
//! and the Transcript pane the **trailing** one. Leading is the leftmost column of
//! a columns split, the topmost row of a stacked split, and the wide column of the
//! short-terminal fallback in which the side column cannot stack; trailing is the
//! slot on the far side of the same split. So M2 draws the Plan in the wide left
//! column and the Transcript in the right one, M4 draws the Plan in the wide column
//! with the Transcript at the head of the side column over the roster, and the
//! `width < 90` stack puts the Plan on top. The rule is about slots only: a slot
//! keeps its own §2.3 width (`55/45`, `60/40`), and the Plan never jumps sides
//! between modes — one geometry, read the same way in every shape.
//!
//! Where a shape draws no Plan pane the rule has nothing to place and the older
//! arrangement stands: M3 keeps the roster in the wide left column over the
//! Transcript, a stacked or side-column fallback that keeps the roster instead of
//! the Plan heads with the Transcript, and the shapes the §2.5 ladder reduces to a
//! single pane (`width < 60`, and `width < 90` with no side pane on screen) place
//! nothing at all.

use super::state::{FocusPane, UiView, ViewMode};
use ratatui::layout::{Constraint, Layout, Rect};

/// Below this width nothing but the too-small notice is drawn (§2.5).
pub const MIN_COLS: u16 = 40;
/// Below this height nothing but the too-small notice is drawn (§2.5).
pub const MIN_ROWS: u16 = 10;
/// The body floor of §2.1 (`Constraint::Min(6)`).
const BODY_MIN: u16 = 6;
/// A column narrower than this is not worth splitting (§2.4.2).
const COLUMN_MIN: u16 = 20;
/// M4 collapses to a single stacked column below this width (§2.5).
const NARROW_COLS: u16 = 90;
/// Header/status drop fields and the footer legend shortens below this (§2.5).
const COMPACT_COLS: u16 = 70;
/// The worker detail pane is absent below this width (§2.5).
const DETAIL_COLS: u16 = 60;
/// Below this row count the status bar collapses to one row and the footer is
/// dropped (§2.5).
const SHORT_ROWS: u16 = 20;

/// Which of the §2.3 body splits is in effect for this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// One pane fills the body.
    Full,
    /// Side-by-side columns (`Percentage` pair of §2.3).
    Columns,
    /// Two panes stacked (`vertical([Min(6), Min(6)])`). With the Plan on screen it
    /// holds the leading (top) slot and the Transcript the trailing (bottom) one —
    /// the global rule read vertically. When the §2.5 ladder keeps the roster
    /// instead of the Plan there is no Plan to place and the Transcript stays on top.
    Stacked,
}

/// The header chip budget for a frame: which optional fields survive the
/// current width. Drop order is `val` → `ctx` → `elapsed` → mode name →
/// `> t-NNN`; `done/total` is never dropped (§7.4, G1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderBudget {
    /// Per-worker validation round — the field plumbing delta **D1** (§9.3)
    /// unblocked, so the budget keeps it at every width §2.5 has not already taken
    /// the optional chips away at. The pane still withholds the chip when the
    /// registry recorded no round: a budget that says `true` is a licence to print a
    /// real number, never an instruction to print one.
    pub show_val: bool,
    /// `ctx %`.
    pub show_ctx: bool,
    /// `elapsed`.
    pub show_elapsed: bool,
    /// The mode name (`M4 Split`).
    pub show_mode: bool,
    /// The active task id (`> t-004`).
    pub show_task: bool,
}

/// Resolve the header budget for a frame of `cols` columns (§2.5, §7.4).
pub const fn header_budget(cols: u16) -> HeaderBudget {
    let compact = cols < COMPACT_COLS;
    HeaderBudget {
        // D1 (§9.3) landed, so the budget keeps the validation-round chip in the
        // same band the other optional chips live in; §7.4 still makes it the first
        // one the ladder takes back.
        show_val: !compact,
        // §2.5 `width < 70`: the header drops `val`, then `ctx`, then `elapsed`
        // (drop order §7.4). All optional chips leave together at that band, so
        // what remains is brand + `done/total` (never dropped, G1) + task id,
        // which is where §7.4's tail (mode name, then `> t-NNN`) converges
        // anyway; the 40-col too-small floor is reached before the task id can
        // be dropped.
        show_ctx: !compact,
        show_elapsed: !compact,
        show_mode: !compact,
        show_task: true,
    }
}

/// Whether the footer legend uses its short form (§2.5, `width < 70`).
pub const fn compact_legend(cols: u16) -> bool {
    cols < COMPACT_COLS
}

/// Whether the worker detail pane may be drawn at all (§2.5, `width < 60`).
pub const fn workers_detail_present(cols: u16) -> bool {
    cols >= DETAIL_COLS
}

/// Every pane rect of one frame. `None` means the pane is absent this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRects {
    /// The mode actually drawn after the §2.5 ladder — what the header prints.
    pub mode: ViewMode,
    /// Header row (`Length(1)`).
    pub header: Option<Rect>,
    /// Body: the remainder absorbed by `Min` (§2.1).
    pub body: Option<Rect>,
    /// Status bar (`Length(2)`, `Length(1)` below 20 rows).
    pub status: Option<Rect>,
    /// Composer (`Length((draft + 2).clamp(3, 9))`).
    pub input: Option<Rect>,
    /// Footer legend row (absent below 20 rows).
    pub footer: Option<Rect>,
    /// Transcript pane.
    pub transcript: Option<Rect>,
    /// Plan pane.
    pub plan: Option<Rect>,
    /// Worker activity pane.
    pub workers: Option<Rect>,
    /// The single too-small notice pane (§2.5).
    pub notice: Option<Rect>,
}

/// Compute every pane rect for `area` under `view` (§2.1 root split + §2.3 body
/// split + §2.5 ladder, applied in order, first match wins).
pub fn compute_layout(area: Rect, view: &UiView) -> PaneRects {
    let mut rects = PaneRects {
        mode: view.mode,
        header: None,
        body: None,
        status: None,
        input: None,
        footer: None,
        transcript: None,
        plan: None,
        workers: None,
        notice: None,
    };

    if area.width < MIN_COLS || area.height < MIN_ROWS {
        // §2.5: exactly one pane, chrome and metrics all absent (never zero-sized).
        rects.mode = ViewMode::Transcript;
        rects.notice = Some(area);
        return rects;
    }

    let rows = area.height;
    let status_h = if rows < SHORT_ROWS { 1 } else { 2 };
    let footer_h = if rows < SHORT_ROWS { 0 } else { 1 };
    let input_h = input_height(rows, view.input_lines);
    let chrome = 1u16 + status_h + footer_h;
    // §2.4.3: the composer can never starve the body — reserve one row for it.
    let input_h = input_h.min(rows.saturating_sub(chrome).saturating_sub(1));
    let body_avail = rows - chrome - input_h;
    let body_floor = BODY_MIN.min(body_avail);

    let root = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(body_floor),
        Constraint::Length(status_h),
        Constraint::Length(input_h),
        Constraint::Length(footer_h),
    ])
    .split(area);

    let body = root[1];
    rects.header = Some(root[0]);
    rects.body = Some(body);
    rects.status = Some(root[2]);
    rects.input = Some(root[3]);
    // `Length(0)` at the root level is the §2.5 rule for the dropped footer, not
    // a zero-height pane rect.
    if footer_h > 0 {
        rects.footer = Some(root[4]);
    }

    let (mode, shape) = resolve_body(body, view);
    rects.mode = mode;
    match (mode, shape) {
        (ViewMode::Transcript, _) => rects.transcript = Some(body),
        (ViewMode::Plan, Shape::Full) => rects.plan = Some(body),
        (ViewMode::Workers, Shape::Full) => rects.workers = Some(body),
        (ViewMode::Plan, _) => {
            // M2 under the global rule: the Plan owns the leading (left) column and
            // the Transcript the trailing (right) one — the same reading order M4
            // gives its columns, so the Plan never moves to the far side of a split.
            // The `55/45` pair is the §2.3 pair unchanged; the rule fixes which pane
            // owns which slot, never a width.
            let cols = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(body);
            rects.plan = Some(cols[0]);
            rects.transcript = Some(cols[1]);
        }
        (ViewMode::Workers, _) => {
            // M3 has no Plan pane, so the global rule has nothing to place here: the
            // roster keeps the wide left column and the Transcript the right one.
            let cols = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(body);
            rects.workers = Some(cols[0]);
            rects.transcript = Some(cols[1]);
        }
        (ViewMode::Split, Shape::Stacked) => {
            // §2.5 `width < 90`: the body stacks two panes, and the focused side
            // pane stands in for one of the three. Where the survivor is the Plan the
            // global rule places it exactly as it does in the columns below — the Plan
            // leads the stack (top row) and the Transcript takes the row under it.
            // Where the roster survives there is no Plan on screen to place, and the
            // Transcript keeps the leading row.
            let stack = Layout::vertical([Constraint::Min(BODY_MIN), Constraint::Min(BODY_MIN)])
                .split(body);
            match side_pane(view) {
                Some(FocusPane::Plan) => {
                    rects.plan = Some(stack[0]);
                    rects.transcript = Some(stack[1]);
                }
                Some(FocusPane::Workers) => {
                    rects.transcript = Some(stack[0]);
                    rects.workers = Some(stack[1]);
                }
                _ => rects.transcript = Some(body),
            }
        }
        (ViewMode::Split, _) => {
            // M4 under the global rule: the Plan owns the leading (wide) column and
            // the Transcript the head of the side column, over the roster. The
            // `60/40` pair is the §2.3 pair unchanged.
            let cols = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(body);
            let (wide, side) = (cols[0], cols[1]);
            match (view.plan_visible, view.workers_visible) {
                (true, true) if side.height >= 2 * BODY_MIN => {
                    let stack =
                        Layout::vertical([Constraint::Min(BODY_MIN), Constraint::Min(BODY_MIN)])
                            .split(side);
                    rects.plan = Some(wide);
                    rects.transcript = Some(stack[0]);
                    rects.workers = Some(stack[1]);
                }
                // The side column cannot stack two panes: only one of the two side
                // panes is drawn, and [`side_pane`] picks the one that keeps the
                // focus on screen. Where that is the Plan, the global rule puts it in
                // the leading (wide) column with the Transcript in the side one.
                (true, true) => match side_pane(view) {
                    Some(FocusPane::Workers) => {
                        rects.transcript = Some(wide);
                        rects.workers = Some(side);
                    }
                    _ => {
                        rects.plan = Some(wide);
                        rects.transcript = Some(side);
                    }
                },
                (true, false) => {
                    rects.plan = Some(wide);
                    rects.transcript = Some(side);
                }
                (false, true) => {
                    rects.transcript = Some(wide);
                    rects.workers = Some(side);
                }
                (false, false) => rects.transcript = Some(body),
            }
        }
    }
    rects
}

/// The §2.3 body split for `body`, after the §2.5 ladder has been applied — the
/// mode is changed *before* splitting so a split never receives an area smaller
/// than its floors (§2.4.2).
fn resolve_body(body: Rect, view: &UiView) -> (ViewMode, Shape) {
    // Too narrow or too short for any floor: one pane, the transcript.
    if body.width < 2 * COLUMN_MIN || body.height < BODY_MIN {
        return (ViewMode::Transcript, Shape::Full);
    }
    if body.width < DETAIL_COLS && matches!(view.mode, ViewMode::Plan | ViewMode::Workers) {
        // §2.5 `width < 60`: M2/M3 become a single column holding the side pane.
        return (view.mode, Shape::Full);
    }
    if body.width < NARROW_COLS {
        // §2.5 `width < 90`: M4 stacks two panes — under the global rule the Plan in
        // the leading (top) row over the Transcript — holding only the focused side
        // pane in place of the other one; a mode that already isolates one pane
        // needs no second column.
        return match view.mode {
            ViewMode::Split if side_pane(view).is_some() && body.height >= 2 * BODY_MIN => {
                (ViewMode::Split, Shape::Stacked)
            }
            ViewMode::Split => (ViewMode::Transcript, Shape::Full),
            ViewMode::Plan | ViewMode::Workers => (view.mode, Shape::Columns),
            ViewMode::Transcript => (ViewMode::Transcript, Shape::Full),
        };
    }
    (
        view.mode,
        match view.mode {
            ViewMode::Transcript => Shape::Full,
            ViewMode::Plan | ViewMode::Workers | ViewMode::Split => Shape::Columns,
        },
    )
}

/// The side pane a frame singles out when the ladder removes the other one: the
/// focused pane if it is toggled on, otherwise whichever side pane is on.
fn side_pane(view: &UiView) -> Option<FocusPane> {
    match view.focus {
        FocusPane::Plan if view.plan_visible => return Some(FocusPane::Plan),
        FocusPane::Workers if view.workers_visible => return Some(FocusPane::Workers),
        _ => {}
    }
    if view.plan_visible {
        Some(FocusPane::Plan)
    } else if view.workers_visible {
        Some(FocusPane::Workers)
    } else {
        None
    }
}

/// `input_h` of §2.1: `(draft_lines + 2).clamp(3, 9)`, clamped to
/// `(draft_lines + 2).clamp(1, 3)` with a `+n` overflow marker below 20 rows
/// (§2.5).
fn input_height(rows: u16, draft_lines: usize) -> u16 {
    let draft = u16::try_from(draft_lines.max(1)).unwrap_or(u16::MAX);
    let wanted = draft.saturating_add(2);
    if rows < SHORT_ROWS {
        wanted.clamp(1, 3)
    } else {
        wanted.clamp(3, 9)
    }
}
