//! Input / composer pane chrome — spec §3 (Input pane), §7.3 (border colours),
//! §2.1 (`input_h` grows with the draft), §2.5 (`+n` overflow marker below 20
//! rows), §4.4 (printable keys always reach the composer).

use super::pane_block;
use crate::ui::tui::state::FocusPane;
use crate::ui::tui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;

/// What the composer frame draws this frame.
#[derive(Debug, Clone, Copy)]
pub struct InputData {
    /// Whether the composer pane holds focus (§4.2 badge).
    pub focus: bool,
    /// Whether the composer is the typing target (§4.4: printable keys reach it
    /// in every mode, so its border never reads as unfocused).
    pub active: bool,
    /// Abort armed: the border turns `frame_alert` (replaces `render.rs:1183-1187`).
    pub armed: bool,
    /// Draft line count, so the border can mark the lines the rect hides.
    pub draft_lines: usize,
}

/// Draw the bordered composer frame. The `TextArea` widget itself — caret,
/// undo history, paste — is still rendered by the existing input renderer
/// (`ratatui_textarea::TextArea`), which is what keeps typing live while the
/// components are being filled in.
pub fn render(frame: &mut Frame, rect: Rect, data: &InputData) {
    let border = if data.armed {
        theme::frame_alert()
    } else if data.focus || data.active {
        theme::frame_focus()
    } else {
        theme::frame_idle()
    };
    let title = format!(
        "{} (Enter send, Alt/Shift+Enter newline, Ctrl+Z undo)",
        FocusPane::Input.label()
    );
    let mut block = pane_block(&title, data.focus, border);
    let visible = usize::from(rect.height.saturating_sub(2));
    if data.draft_lines > visible {
        block = block.title_bottom(Line::from(format!(
            " +{} ",
            data.draft_lines - visible
        )));
    }
    frame.render_widget(block, rect);
}
