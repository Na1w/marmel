//! Header row — spec §3 (Header pane), §2.2 chip order, §7.4 drop order.
//!
//! `done/total` and the active task id are computed once per frame in
//! [`crate::ui::tui::state::PlanProgress`] from the same `plan_content` the plan
//! pane draws, which is what makes G1 (plan progress visible in every frame at
//! 40x10, plan pane open or not) true.

use super::fit_spans;
use crate::ui::tui::layout::HeaderBudget;
use crate::ui::tui::state::{PlanProgress, ViewMode};
use crate::ui::tui::theme::{self, Glyph};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// What the header may draw this frame.
#[derive(Debug, Clone, Copy)]
pub struct HeaderData<'a> {
    /// The mode actually drawn (`compute_layout` decides after the §2.5 ladder).
    pub mode: ViewMode,
    /// Derived plan progress.
    pub plan: &'a PlanProgress,
    /// Which optional chips survive the current width (§7.4 drop order).
    pub budget: HeaderBudget,
    /// Orchestrator context tokens — the numerator of `ctx %` (spec §2.2). The
    /// percentage against `config.max_context_tokens` lands with t-008.
    pub ctx_tokens: usize,
}

/// Draw the one header row (no border: `Length(1)` in §2.1 leaves no room for
/// one) with the chip order of §2.2.
pub fn render(frame: &mut Frame, rect: Rect, data: &HeaderData<'_>) {
    let mut spans = vec![Span::styled(" Marmel".to_string(), theme::chip_done())];
    if data.budget.show_mode {
        spans.push(Span::styled(
            format!(" {}", data.mode.label()),
            theme::text_dim(),
        ));
    }
    // Never dropped, at any width (G1).
    spans.push(Glyph::Done.badge());
    spans.push(Span::styled(
        format!(" {} ", data.plan.progress_label()),
        theme::chip_done(),
    ));
    if data.budget.show_val {
        // Gated on plumbing delta D1 (spec §9.3): per-worker `validation_rounds`
        // is not projected into the UI row yet, so no round number is printed.
        spans.push(Span::styled(" val —".to_string(), theme::text_dim()));
    }
    if data.budget.show_ctx && data.ctx_tokens > 0 {
        spans.push(Span::styled(
            format!(" ctx {}t", data.ctx_tokens),
            theme::text_dim(),
        ));
    }
    if data.budget.show_elapsed {
        // The elapsed clock is drawn by t-008; the chip slot is reserved now so
        // the drop order of §7.4 is exercised by this frame.
        spans.push(Span::styled(" elapsed —".to_string(), theme::text_dim()));
    }
    if data.budget.show_task
        && let Some(task) = data.plan.active_task.as_ref()
    {
        spans.push(Glyph::Active.badge());
        spans.push(Span::styled(format!(" {task}"), theme::chip_active()));
    }
    let line = Line::from(fit_spans(spans, rect.width));
    frame.render_widget(Paragraph::new(line), rect);
}
