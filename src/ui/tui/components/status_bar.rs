//! Status bar — spec §3 (Status bar pane), §7.3 (armed prompt lives on row 2,
//! row 1 keeps counting), §8.3 (counts derived from the projected worker rows).
//!
//! Row 1 stays the metrics line (tokens, ctx, status prose) and is still written
//! by the existing metrics renderer; this component owns row 2, the persistent
//! state-chip echo of every toggle (G5) and the abort-arm prompt (§8.5).

use super::fit_spans;
use crate::ui::SubagentDetail;
use crate::ui::tui::theme::{self, Glyph};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// What the chip row may draw this frame.
#[derive(Debug, Clone, Copy)]
pub struct StatusData<'a> {
    /// Every live worker row; the counts below are derived from them, never from
    /// a second state owner (spec §8.3, DATA §2 step 8).
    pub workers: &'a [SubagentDetail],
    /// Transcript follow toggle (`Ctrl+F`, `/follow`) — G7.
    pub follow: bool,
    /// Thinking display (`Ctrl+T`, `/thought`) — P1.3.
    pub show_thought: bool,
    /// Plan pane toggle (`Ctrl+P`, `/plan`).
    pub plan_visible: bool,
    /// Workers pane toggle (`Ctrl+A`, `/workers`).
    pub workers_visible: bool,
    /// Abort armed inside the 3 s window (§8.5).
    pub abort_armed: bool,
}

/// Draw the chip row. Below 20 rows the ladder collapses the status bar to one
/// metrics row (§2.5), so the chip row — and with it the state echo — is
/// suppressed rather than squeezed into a zero-height rect.
pub fn render(frame: &mut Frame, rect: Rect, data: &StatusData<'_>) {
    if rect.height < 2 {
        return;
    }
    let chips_row = Rect {
        x: rect.x,
        y: rect.y + rect.height - 1,
        width: rect.width,
        height: 1,
    };

    let follow_glyph = if data.follow {
        Span::styled(
            format!(" {} LIVE ", theme::FOCUS_BADGE),
            theme::chip_done(),
        )
    } else {
        Span::styled(" ↓ follow off ", theme::chip_pending())
    };
    let running = data.workers.iter().filter(|row| row.is_active).count();
    let paused = data.workers.iter().filter(|row| row.paused).count();
    let queued = data.workers.iter().filter(|row| row.queue.is_some()).count();

    let mut spans = vec![
        Span::styled(" ".to_string(), theme::text_primary()),
        follow_glyph,
        Span::styled(
            format!(" plan:{} ", on_off(data.plan_visible)),
            theme::chip(),
        ),
        Span::styled(
            format!(" workers:{} ", on_off(data.workers_visible)),
            theme::chip(),
        ),
        Span::styled(
            format!(" think:{} ", on_off(data.show_thought)),
            theme::chip(),
        ),
        Span::styled(format!(" {running}a {paused}p {queued}q "), theme::chip()),
    ];
    if data.abort_armed {
        // The armed prompt no longer replaces the metrics row (replaces
        // `render.rs:1117-1125`): it sits here, and row 1 keeps counting (§7.3).
        spans.push(Glyph::Failed.tagged());
        spans.push(Span::styled(
            " abort armed — Esc again ".to_string(),
            theme::error(),
        ));
    }
    let line = Line::from(fit_spans(spans, chips_row.width));
    frame.render_widget(Paragraph::new(line), chips_row);
}

/// The `on`/`off` spelling used by every state chip.
fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}
