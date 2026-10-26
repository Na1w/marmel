//! Help / keymap surfaces — spec §3 (Help overlay), §5 (one legend source for
//! the footer, the `F1` overlay and `/help`), §7.3 (overlays draw last, centred),
//! §2.5 (short legend below 70 columns).
//!
//! The legend text here is the placeholder for `keymap::legend()`, which t-010
//! generates from the single table the key handler matches on — until then the
//! footer and the overlay read from this one module, so they cannot drift apart.

use super::{fit_spans, pane_block};
use crate::ui::tui::state::OverlayState;
use crate::ui::tui::theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

/// Which help surface this call draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpKind {
    /// The unbordered one-row legend in the footer (§2.2).
    Legend,
    /// The centred modal overlay of §7.3, in the given overlay state.
    Overlay(OverlayState),
}

/// What this help surface draws.
#[derive(Debug, Clone, Copy)]
pub struct HelpData {
    /// Legend row or modal overlay.
    pub kind: HelpKind,
    /// Short legend form (`width < 70`, §2.5).
    pub compact: bool,
}

/// Draw the footer legend, or the modal keymap/confirm placeholder.
pub fn render(frame: &mut Frame, rect: Rect, data: &HelpData) {
    match data.kind {
        HelpKind::Legend => {
            let legend = if data.compact {
                " F1 help  Esc arm  Enter send "
            } else {
                " F1 help  Tab focus  F2 mode  F3/F4/F5 panes  x cancel-worker  e last error  \
                 Esc arm  Ctrl+A/P/T panes "
            };
            let spans = fit_spans(vec![Span::styled(legend.to_string(), theme::text_dim())], rect.width);
            frame.render_widget(Paragraph::new(Line::from(spans)), rect);
        }
        HelpKind::Overlay(overlay) => {
            // A closed overlay draws nothing: the panes keep the frame (§4.4 —
            // `Esc` closes the overlay without arming abort).
            if !overlay.is_open() {
                return;
            }
            let border = match overlay {
                OverlayState::Confirm => theme::frame_confirm(),
                OverlayState::Help | OverlayState::None => theme::frame_focus(),
            };
            let title = match overlay {
                OverlayState::Help => "Keys",
                OverlayState::Confirm => "Confirm",
                OverlayState::None => "Overlay",
            };
            let body = Paragraph::new(
                "keymap legend and confirm text land in t-009/t-010 (spec §5: one table, \
                 three surfaces)",
            )
            .block(pane_block(title, false, border))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
            frame.render_widget(body, rect);
        }
    }
}
