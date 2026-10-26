//! Plan / task-overview pane chrome — spec §3 (Plan pane), §2.3 (M2/M4
//! geometry), §7.2 (state glyphs), §8.1 (empty state).

use super::{fit_spans, pane_block};
use crate::ui::tui::state::{FocusPane, PlanProgress};
use crate::ui::tui::theme::{self, Glyph};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

/// What the plan pane chrome draws this frame.
#[derive(Debug, Clone, Copy)]
pub struct PlanData<'a> {
    /// Whether this pane holds focus (§7.3 badge + border).
    pub focus: bool,
    /// Progress derived from the very `plan_content` string this pane renders.
    pub progress: &'a PlanProgress,
    /// Scroll offset of the plan body (§2.4.4).
    pub scroll: u16,
}

/// Draw the bordered plan pane. The plan text body itself is still produced by
/// the existing plan renderer (t-008 moves it here); the progress line rides the
/// bottom border so it can never overwrite body text.
pub fn render(frame: &mut Frame, rect: Rect, data: &PlanData<'_>) {
    let border = if data.focus {
        theme::frame_focus()
    } else {
        theme::frame_idle()
    };
    let progress = data.progress;
    let active = progress.active_task.is_some();
    let glyph = theme::task_glyph(progress.has_tasks() && progress.done == progress.total, active);

    let mut chips = vec![
        Span::styled(" ".to_string(), theme::text_primary()),
        glyph.badge(),
        Span::styled(
            format!(" {} ", progress.progress_label()),
            theme::chip_done(),
        ),
        Glyph::Pending.badge(),
        Span::styled(
            format!(" {} open ", progress.pending()),
            theme::chip_pending(),
        ),
    ];
    if let Some(task) = progress.active_task.as_ref() {
        chips.push(Span::styled(format!(" > {task} "), theme::chip_active()));
    }
    chips.push(Span::styled(
        format!(" scroll {} ", data.scroll),
        theme::text_dim(),
    ));

    let block =
        pane_block(FocusPane::Plan.label(), data.focus, border)
            .title_bottom(Line::from(fit_spans(chips, rect.width)));
    frame.render_widget(block, rect);
}
