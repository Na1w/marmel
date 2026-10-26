//! Pane components of the rebuilt TUI — spec §3 (pane inventory) and §7.3
//! (borders, titles, focus badge).
//!
//! Each submodule owns one pane and exposes `render(&mut Frame, Rect, &Data)`.
//! In this scaffold they draw the pane *chrome*: the bordered block, its title,
//! the colour-independent `●` focus badge, and the state chips that must be
//! present in every frame. The text bodies of the transcript / plan / worker
//! panes are still produced by the existing `render.rs` + `formatting.rs`
//! machinery, which `TuiRenderer::draw_new_ui` calls into before this chrome is
//! drawn; t-007..t-009 move that content behind these `render` functions.

// `pub(crate)` so `TuiRenderer::draw_new_ui` (in the parent module) can reach the
// pane renderers; the modules themselves stay inside this private subtree, so
// every symbol still has to be reached by the render path or a test (spec §0).
pub(crate) mod activity;
pub(crate) mod header;
pub(crate) mod help;
pub(crate) mod input;
pub(crate) mod plan_pane;
pub(crate) mod status_bar;
pub(crate) mod transcript;

use super::layout::{MIN_COLS, MIN_ROWS};
use super::theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A single-line bordered pane block with its title and, when focused, the `●`
/// badge that makes focus readable without colour (§7.3, G6).
pub fn pane_block(title: &str, focused: bool, border: Style) -> Block<'static> {
    let (title, title_style) = if focused {
        (
            format!(" {title} {} ", theme::FOCUS_BADGE),
            theme::frame_focus(),
        )
    } else {
        (format!(" {title} "), theme::text_dim())
    };
    Block::bordered()
        .border_style(border)
        .title(Line::from(Span::styled(title, title_style)))
}

/// The cells a title may use on a bordered block's border row.
///
/// A `Block` border row is `rect.width - 2` cells wide: the two corner cells are
/// border, not title. A chip row fitted to `rect.width` is therefore 2 cells too
/// long, and ratatui cuts those 2 cells *silently* — the `…` that `fit_spans`
/// appended is one of them, so the operator sees a chip row that simply stops
/// mid-word with no mark that anything was lost. Every pane that puts spans on a
/// border row fits against this width instead.
pub const fn title_width(rect: Rect) -> u16 {
    rect.width.saturating_sub(2)
}

/// Terminal cells a run of spans occupies, measured with `unicode-width` (§7.2:
/// glyph widths are measured, never assumed to be one cell).
pub fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Clip a run of styled spans to `width` terminal cells, marking the cut with
/// `…` (§7.4: header/status/roster lines are single-row and never exceed the
/// rect; widths are measured with `unicode-width`, never assumed to be 1 cell).
///
/// The cut is *cell*-granular, not span-granular: a span that only partly fits
/// is split at the last whole grapheme that still fits, so a long chip in the
/// middle of a line degrades instead of deleting everything after it.
pub fn fit_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Span<'static>> {
    let limit = usize::from(width);
    if limit == 0 {
        return Vec::new();
    }
    let total: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if total <= limit {
        return spans;
    }

    let ellipsis = theme::ELLIPSIS.to_string();
    let ellipsis_width = UnicodeWidthStr::width(ellipsis.as_str());
    // Cells left for content once the ellipsis has taken its own width.
    let budget = limit.saturating_sub(ellipsis_width);

    let mut kept: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    'spans: for span in spans {
        let style = span.style;
        let mut piece = String::new();
        let mut piece_width = 0usize;
        for ch in span.content.as_ref().chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + piece_width + ch_width > budget {
                // Out of room: keep what this span contributed and stop.
                kept.push(Span::styled(std::mem::take(&mut piece), style));
                break 'spans;
            }
            piece.push(ch);
            piece_width += ch_width;
        }
        used += piece_width;
        kept.push(Span::styled(piece, style));
    }
    kept.retain(|span| !span.content.is_empty());
    kept.push(Span::styled(ellipsis, theme::text_dim()));
    kept
}

/// The one pane the `width < 40 || rows < 10` rule draws instead of everything
/// else (§2.5): header, status, input and footer are all absent, and key
/// handling keeps working.
pub fn render_notice(frame: &mut Frame, rect: Rect, cols: u16, rows: u16) {
    let text = format!(
        "Terminal too small — Marmel needs at least {MIN_COLS}x{MIN_ROWS} (currently {cols}x{rows}). Resize to continue."
    );
    let paragraph = Paragraph::new(text)
        .block(pane_block("Marmel", false, theme::frame_alert()))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, rect);
}
