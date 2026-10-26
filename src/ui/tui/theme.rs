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
///
/// This is the label-free half of the rule: with only liveness projected, "not
/// running" is all a row can honestly say. [`worker_state_glyph`] is the entry
/// point a caller uses once the registry's own `status_label` is on the row
/// (plumbing delta D1, design §9.3), because liveness alone cannot tell an aborted
/// worker from an approved one.
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

/// Terminal status labels that mean "this worker did not deliver" (§7.2 `✗`).
/// Matched against the leading word of `WorkerEntry::status_label`, the vocabulary
/// the runner and the review loop actually write: `Aborted`, `Rejected`, `Failed`,
/// `Replan Required (…)`.
const FAILED_STATUS_LABELS: [&str; 4] = ["aborted", "failed", "rejected", "replan"];
/// Labels that mean "still working, held by the review loop" (§7.2 `⧗`, which the
/// spec spells "queued **or validating**").
const PENDING_STATUS_LABELS: [&str; 3] = ["revising", "validating", "queued"];
/// Labels that mean the worker delivered and was accepted (§7.2 `✓`).
const DONE_STATUS_LABELS: [&str; 2] = ["approved", "completed"];

/// The §7.2 glyph for a worker whose registry label is projected (D1).
///
/// The slot state comes first — a stream parked behind the arbitrator, or a
/// delegation waiting for a backend slot, is in those states regardless of what
/// its last label said. Then the label decides a **terminal** state, which is the
/// whole point of D1: `is_active == false` says the stream closed, and only the
/// label says whether it closed approved or aborted. An aborted or
/// validator-rejected worker therefore reads `✗ failed` and never `✓ done`.
///
/// A label the vocabulary does not know (`In Progress`, `Running`) decides
/// nothing, and the caller falls back to [`worker_glyph`]: guessing a terminal state
/// from prose would be inventing a state the registry never reported.
pub fn worker_state_glyph(
    is_active: bool,
    paused: bool,
    queued: bool,
    status_label: Option<&str>,
) -> Glyph {
    if paused || queued {
        return worker_glyph(is_active, paused, queued);
    }
    if let Some(label) = status_label
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        // Only the leading word decides: `Replan Required (…)` is a replan verdict,
        // and a label that merely mentions a verdict word later is not one.
        let head = label
            .split([' ', '-', '(', ':'])
            .next()
            .unwrap_or(label)
            .trim()
            .to_ascii_lowercase();
        if FAILED_STATUS_LABELS.contains(&head.as_str()) {
            return Glyph::Failed;
        }
        if PENDING_STATUS_LABELS.contains(&head.as_str()) {
            return Glyph::Pending;
        }
        if DONE_STATUS_LABELS.contains(&head.as_str()) {
            return Glyph::Done;
        }
    }
    worker_glyph(is_active, paused, queued)
}
