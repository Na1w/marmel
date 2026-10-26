//! Worker activity feed chrome — spec §3 (Worker activity pane), §2.3 (M3
//! roster `Min(4)` / detail `Min(3)`), §2.5 (detail absent below 60 columns),
//! §7.2 (state glyphs), §8.3 (every live row reachable, counts derived from the
//! registry-projected rows).

use super::{fit_spans, pane_block};
use crate::ui::SubagentDetail;
use crate::ui::tui::state::FocusPane;
use crate::ui::tui::theme::{self, Glyph};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

/// What the worker pane chrome draws this frame.
#[derive(Debug, Clone, Copy)]
pub struct ActivityData<'a> {
    /// Whether this pane holds focus.
    pub focus: bool,
    /// The projected worker rows (`SubagentDetail`, the single state owner).
    pub rows: &'a [SubagentDetail],
    /// Selected roster row index (`←`/`→`, `x` cancels this row).
    pub selected: usize,
    /// Whether the detail half may be drawn at all (§2.5, `width < 60`).
    pub detail_present: bool,
    /// Scroll offset of the detail half.
    pub scroll: u16,
}

/// Draw the bordered worker pane: the roster `n/total` marker, the per-state
/// glyph tally (§7.2) and the detail-pane visibility marker. The roster rows and
/// the `[Thinking]/[Output]/[Logs]` detail body are still produced by the
/// existing subagent renderer; t-008 moves them behind this function and removes
/// the 3-row clamp (G8).
pub fn render(frame: &mut Frame, rect: Rect, data: &ActivityData<'_>) {
    let border = if data.focus {
        theme::frame_focus()
    } else {
        theme::frame_idle()
    };

    let mut running = 0usize;
    let mut paused = 0usize;
    let mut queued = 0usize;
    let mut idle = 0usize;
    for row in data.rows {
        match theme::worker_glyph(row.is_active, row.paused, row.queue.is_some()) {
            Glyph::Active => running += 1,
            Glyph::Paused => paused += 1,
            Glyph::Pending => queued += 1,
            Glyph::Done => idle += 1,
            Glyph::Failed => {}
        }
    }

    let mut chips = vec![
        Glyph::Active.tagged(),
        Span::styled(format!(" {running} "), theme::chip_active()),
        Glyph::Paused.tagged(),
        Span::styled(format!(" {paused} "), theme::chip()),
        Glyph::Pending.tagged(),
        Span::styled(format!(" {queued} "), theme::chip_pending()),
        Glyph::Done.tagged(),
        Span::styled(format!(" {idle} "), theme::chip_done()),
    ];
    let (selected, total) = if data.rows.is_empty() {
        (0, 0)
    } else {
        (
            data.selected.min(data.rows.len() - 1) + 1,
            data.rows.len(),
        )
    };
    chips.push(Span::styled(
        format!(" {selected}/{total}, "),
        theme::text_dim(),
    ));
    if data.detail_present {
        chips.push(Span::styled(
            format!(" detail scroll {} ", data.scroll),
            theme::text_dim(),
        ));
    } else {
        chips.push(Span::styled(" detail hidden ".to_string(), theme::text_dim()));
    }

    let block = pane_block(FocusPane::Workers.label(), data.focus, border)
        .title_bottom(Line::from(fit_spans(chips, rect.width)));
    frame.render_widget(block, rect);
}
