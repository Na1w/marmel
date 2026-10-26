//! Pane geometry — spec §2 (layout spec), §2.4 (zero-rect rule) and §2.5 (the
//! narrow-terminal degradation ladder).
//!
//! [`compute_layout`] is a pure function over the frame area plus the
//! [`UiView`]: it never touches the terminal and never returns a zero-sized pane
//! rect. A pane that the mode (or the ladder) removes is `None` — never split,
//! never drawn, never hit-tested (§2.4.1).

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
    /// Transcript above, the focused side pane below (`vertical([Min(6), Min(6)])`).
    Stacked,
}

/// The header chip budget for a frame: which optional fields survive the
/// current width. Drop order is `val` → `ctx` → `elapsed` → mode name →
/// `> t-NNN`; `done/total` is never dropped (§7.4, G1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderBudget {
    /// Per-worker validation round — gated on plumbing delta **D1** (§9.3), so
    /// it is false until that lands (spec §10 gate (c)).
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
        // Gated on D1 (§9.3): no pane may print a round number yet.
        show_val: false,
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
            let cols = Layout::horizontal([
                Constraint::Percentage(55),
                Constraint::Percentage(45),
            ])
            .split(body);
            rects.plan = Some(cols[0]);
            rects.transcript = Some(cols[1]);
        }
        (ViewMode::Workers, _) => {
            let cols = Layout::horizontal([
                Constraint::Percentage(55),
                Constraint::Percentage(45),
            ])
            .split(body);
            rects.workers = Some(cols[0]);
            rects.transcript = Some(cols[1]);
        }
        (ViewMode::Split, Shape::Stacked) => {
            let stack = Layout::vertical([
                Constraint::Min(BODY_MIN),
                Constraint::Min(BODY_MIN),
            ])
            .split(body);
            rects.transcript = Some(stack[0]);
            match side_pane(view) {
                Some(FocusPane::Plan) => rects.plan = Some(stack[1]),
                Some(FocusPane::Workers) => rects.workers = Some(stack[1]),
                _ => rects.transcript = Some(body),
            }
        }
        (ViewMode::Split, _) => {
            let cols = Layout::horizontal([
                Constraint::Percentage(60),
                Constraint::Percentage(40),
            ])
            .split(body);
            rects.transcript = Some(cols[0]);
            let side = cols[1];
            match (view.plan_visible, view.workers_visible) {
                (true, true) if side.height >= 2 * BODY_MIN => {
                    let stack = Layout::vertical([
                        Constraint::Min(BODY_MIN),
                        Constraint::Min(BODY_MIN),
                    ])
                    .split(side);
                    rects.plan = Some(stack[0]);
                    rects.workers = Some(stack[1]);
                }
                (true, true) => match side_pane(view) {
                    Some(FocusPane::Workers) => rects.workers = Some(side),
                    _ => rects.plan = Some(side),
                },
                (true, false) => rects.plan = Some(side),
                (false, true) => rects.workers = Some(side),
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
        // §2.5 `width < 90`: M4 stacks the transcript over only the focused side
        // pane; a mode that already isolates one pane needs no second column.
        return match view.mode {
            ViewMode::Split if side_pane(view).is_some() && body.height >= 2 * BODY_MIN => {
                (ViewMode::Split, Shape::Stacked)
            }
            ViewMode::Split => (ViewMode::Transcript, Shape::Full),
            ViewMode::Plan | ViewMode::Workers => (view.mode, Shape::Columns),
            ViewMode::Transcript => (ViewMode::Transcript, Shape::Full),
        };
    }
    (view.mode, match view.mode {
        ViewMode::Transcript => Shape::Full,
        ViewMode::Plan | ViewMode::Workers | ViewMode::Split => Shape::Columns,
    })
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
