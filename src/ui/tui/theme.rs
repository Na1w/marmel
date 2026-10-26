//! Palette slots, state glyphs and border/focus styles — spec §7 (visual
//! language).
//!
//! One source of truth for colour and glyph decisions: every pane component
//! asks for a *slot* (`frame_focus`, `chip_active`, …) or a [`Glyph`] and never
//! names a `Color` itself. Colours keep today's values (ARCH §4) so the rebuild
//! is a pure re-arrangement of the visual language.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Badge marking the focused pane in its title, independent of colour (§7.3, G6).
pub const FOCUS_BADGE: char = '●';

/// Mark a run of text clipped to a rect (§7.4).
pub const ELLIPSIS: char = '…';

/// Focused pane border (keeps today's Cyan, `render.rs:426-430`).
pub fn frame_focus() -> Style {
    Style::default().fg(Color::Cyan)
}

/// Unfocused pane border (keeps today's DarkGray).
pub fn frame_idle() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Alert border: abort-armed composer (`render.rs:1183-1187`) and the too-small
/// notice.
pub fn frame_alert() -> Style {
    Style::default().fg(Color::Red)
}

/// Confirm-overlay border (§7.1).
pub fn frame_confirm() -> Style {
    Style::default().fg(Color::Yellow)
}

/// Neutral status row-2 state chip (§7.1).
pub fn chip() -> Style {
    Style::default().fg(Color::White).bg(Color::DarkGray)
}

/// `▶` running / active plan task (the `render.rs:548-569` pattern).
pub fn chip_active() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// `✓`, `[x]`, plan headers.
pub fn chip_done() -> Style {
    Style::default().fg(Color::Green)
}

/// `[ ]`, pending task.
pub fn chip_pending() -> Style {
    Style::default().fg(Color::LightRed)
}

/// Body text.
pub fn text_primary() -> Style {
    Style::default()
}

/// Secondary header/status fields.
pub fn text_dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Sticky error line / `LLM error:` text.
pub fn error() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

/// The five state glyphs of §7.2. They replace the prose-only
/// `(Active | Paused | Queued n/m | Idle)` labels, but [`Glyph::label`] is kept
/// beside the glyph whenever the rect has room, so a terminal without these
/// codepoints still reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// `✓` done / completed / idle.
    Done,
    /// `▶` running / active.
    Active,
    /// `⧗` queued or validating.
    Pending,
    /// `⏸` paused.
    Paused,
    /// `✗` failed / aborted.
    Failed,
}

impl Glyph {
    /// The glyph itself. Measured with `unicode-width` by the caller that clips
    /// it (spec §7.2); never assumed to be one cell.
    pub const fn symbol(self) -> char {
        match self {
            Self::Done => '✓',
            Self::Active => '▶',
            Self::Pending => '⧗',
            Self::Paused => '⏸',
            Self::Failed => '✗',
        }
    }

    /// The word label kept next to the glyph.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Active => "running",
            Self::Pending => "queued",
            Self::Paused => "paused",
            Self::Failed => "failed",
        }
    }

    /// The palette slot this glyph is drawn with.
    pub fn style(self) -> Style {
        match self {
            Self::Done => chip_done(),
            Self::Active => chip_active(),
            Self::Pending => chip_pending(),
            Self::Paused => chip(),
            Self::Failed => error(),
        }
    }

    /// Bare glyph as a styled span.
    pub fn badge(self) -> Span<'static> {
        Span::styled(self.symbol().to_string(), self.style())
    }

    /// Glyph plus word label as a styled span.
    pub fn tagged(self) -> Span<'static> {
        Span::styled(format!("{} {}", self.symbol(), self.label()), self.style())
    }
}

/// Glyph for a plan task line: checked-off tasks read `✓`, the task in flight
/// reads `▶`, everything else is pending `⧗` (spec §7.2).
pub const fn task_glyph(completed: bool, active: bool) -> Glyph {
    if completed {
        Glyph::Done
    } else if active {
        Glyph::Active
    } else {
        Glyph::Pending
    }
}

/// Glyph for a specialist roster row, derived from the registry-projected row
/// state (`is_active` / `paused` / `queue`, spec §8.3). A row with nothing in
/// flight reads `✓` (idle = finished, never `▶`).
pub const fn worker_glyph(is_active: bool, paused: bool, queued: bool) -> Glyph {
    if paused {
        Glyph::Paused
    } else if queued {
        Glyph::Pending
    } else if is_active {
        Glyph::Active
    } else {
        Glyph::Done
    }
}
