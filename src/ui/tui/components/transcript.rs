//! Transcript pane chrome — spec §3 (Transcript pane), §7.3 (borders/focus),
//! G7 (honest scrollback: `↓ N new` / `LIVE`).

use super::{fit_spans, pane_block};
use crate::ui::tui::state::FocusPane;
use crate::ui::tui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

/// What the transcript chrome draws this frame.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptData {
    /// Whether this pane holds focus.
    pub focus: bool,
    /// Follow mode: new output moves the viewport only while this is on (G7).
    pub follow: bool,
    /// Current scroll offset of the body.
    pub scroll: u16,
}

/// Draw the bordered transcript pane. The markdown/ANSI body is still produced
/// by the existing chat renderer over `formatting::render_markdown_lines` /
/// `parse_ansi_lines`; t-007 moves that behind this function, and t-010 turns
/// the `follow` flag into the unseen-line counter.
pub fn render(frame: &mut Frame, rect: Rect, data: &TranscriptData) {
    let border = if data.focus {
        theme::frame_focus()
    } else {
        theme::frame_idle()
    };
    let follow_chip = if data.follow {
        Span::styled(
            format!(" {} LIVE ", theme::FOCUS_BADGE),
            theme::chip_done(),
        )
    } else {
        Span::styled(
            format!(" ↓ {} scrolled ", data.scroll),
            theme::chip_pending(),
        )
    };
    let chips = vec![
        Span::styled(" ".to_string(), theme::text_primary()),
        follow_chip,
        Span::styled(
            format!(" line {} ", data.scroll),
            theme::chip(),
        ),
    ];
    let block = pane_block(FocusPane::Transcript.label(), data.focus, border)
        .title_bottom(Line::from(fit_spans(chips, rect.width)));
    frame.render_widget(block, rect);
}
