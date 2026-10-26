//! Transcript pane — spec §3 (Transcript pane), §7.4 (wrap + scrollbar + chip),
//! §8.1 (empty state) and G7 (scrollback honesty: `● LIVE` / `↓ N new`).
//!
//! The pane owns the whole body pipeline: message rendering (markdown through
//! [`formatting::render_markdown_lines`] and ANSI through
//! [`formatting::parse_ansi_lines`], both reached via
//! [`formatting::render_message_lines`]), the streaming thought / content / steer
//! regions, verdict-marker emphasis, the wrapped-line height, the scroll clamp,
//! the scrollbar and the G7 chip. Nothing here reimplements markdown, ANSI or
//! wrapping — spec §9.1 keeps those in `formatting` and has components reuse them.
//!
//! ## Needs plumb for t-009b (data that is not in the frozen signature)
//!
//! [`TranscriptData`] is built by `TuiRenderer::draw_new_ui` (`mod.rs:484-490`) and
//! carries only `focus` / `follow` / `scroll`. Everything below is therefore a
//! seam for t-009b, and **none of it may be solved by editing `mod.rs`**:
//!
//! 1. Conversation strings — `messages`, `current_thought`, `current_content`,
//!    `steer_sentence_buffer` (`mod.rs:81-85,135`) — plus the `show_thought`
//!    toggle (`mod.rs:235`). [`available_body`] hands the pipeline an empty body,
//!    so the pane cannot paint what it does not hold; [`draw_body`] leaves the
//!    pixels the existing chat renderer already put down. Once those fields reach
//!    the signature, `available_body` is the only function that has to change:
//!    every stage below already runs on production paths and is covered here.
//! 2. Scroll write-back. The pinned/clamped offset is reported by [`BodyReport`]
//!    but cannot leave `render(&mut Frame, Rect, &TranscriptData)`, so
//!    `chat_scroll` stays owned by the renderer until the signature returns it.
//!    The chip is computed from the offset this pane *drew*, so it never claims a
//!    count it did not honour.
//! 3. The §8.5 sticky error line (`✗ <cause>` + `see .marmel/marmel.log`) and the
//!    `e`/`/error` recall of `last_error`: they arrive as transcript lines, so the
//!    moment messages are plumbed the verdict emphasis here already badges them.
//! 4. The §8.6 restore report and the single `token_witness_warnings` line — same
//!    route: they are message text, and `announce_restore` (`mod.rs:1560`) owns
//!    emitting them.
//! 5. `active_agent`, which would label the streaming content region (§3). Until
//!    it is plumbed the streaming region is drawn unlabelled rather than guessed.

use super::{fit_spans, pane_block, title_width};
use crate::markers;
use crate::ui::tui::formatting::{
    count_wrapped_rendered_lines, format_terminal_math, parse_ansi_lines, render_markdown_lines,
    render_message_lines, strip_think_tags,
};
use crate::ui::tui::state::FocusPane;
use crate::ui::tui::theme::{self, Glyph};
use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};

/// The §8.1 empty state, spelled once so the pane and its tests cannot drift.
pub const EMPTY_STATE: &str = "No output yet. Type an instruction and press Enter (? for keys).";

/// What the transcript draws this frame: the pane owns the whole body, so every
/// conversation string it paints arrives here (spec §3, Transcript row).
#[derive(Debug, Clone, Copy)]
pub struct TranscriptData<'a> {
    /// Whether this pane holds focus.
    pub focus: bool,
    /// Follow mode: new output moves the viewport only while this is on (G7).
    pub follow: bool,
    /// Current scroll offset of the body.
    pub scroll: u16,
    /// The conversation itself — messages plus the three streaming regions.
    pub body: TranscriptBody<'a>,
    /// Wrapped-line count of the body the last time the viewport sat at its
    /// bottom, recorded by the renderer (`TuiRenderer::transcript_baseline_lines`).
    /// The `↓ N new` count is measured against it, so output that arrived while
    /// the operator was scrolled away still counts as new even if the offset has
    /// since been re-pinned by a clamp. `None` = no baseline recorded yet, in
    /// which case only the distance below the viewport is claimed.
    pub unseen_baseline: Option<usize>,
}

/// The conversation body the pane renders, in the shapes the renderer actually
/// holds: finished messages, then the three in-flight streaming buffers.
#[derive(Debug, Clone, Copy, Default)]
pub struct TranscriptBody<'a> {
    /// Completed messages, oldest first — the renderer's `messages` field.
    pub messages: &'a [String],
    /// The renderer's styled-line cache for those messages, one entry per message
    /// (`TuiRenderer::cached_rendered_messages`), so a frame never re-parses the
    /// whole scrollback's markdown and ANSI. `None`, or a cache that is not in
    /// step with `messages`, makes the pane render every message itself.
    pub rendered: Option<&'a [Vec<Line<'static>>]>,
    /// In-flight reasoning stream (`current_thought`), drawn only while
    /// `show_thought` is on (§8.2).
    pub thought: &'a str,
    /// In-flight visible content (`current_content`).
    pub content: &'a str,
    /// In-flight steer sentence (`steer_sentence_buffer`).
    pub steer: &'a str,
    /// The `Ctrl+T` / `/thought` toggle (§8.2).
    pub show_thought: bool,
}

/// What one body pass established about the viewport — what the chip row prints,
/// and what the renderer writes back so its scroll keys match what was drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BodyReport {
    /// Offset actually drawn this frame (clamped, and pinned by follow).
    pub scroll: u16,
    /// Largest legal offset for this body at this size.
    pub max_scroll: usize,
    /// Lines below the viewport the operator has not seen (G7): exactly the count
    /// of new output while they sit at the bottom, and otherwise the distance
    /// they would have to scroll to catch up.
    pub unseen: usize,
    /// Wrapped lines the whole body occupies at this width; the renderer keeps it
    /// as the `↓ N new` baseline.
    pub total_lines: usize,
}

/// Draw the transcript pane — body first, then the block, so the body keeps its
/// cells while the chrome keeps its title and chips, the ordering the Subagents
/// block already uses at `render.rs:1030` — and report the viewport this pass
/// drew, which is what `chat_scroll` and the `↓ N new` baseline are written back
/// from.
pub fn render(frame: &mut Frame, rect: Rect, data: &TranscriptData<'_>) -> BodyReport {
    let report = draw_body(
        frame,
        rect,
        &data.body,
        data.follow,
        data.scroll,
        data.unseen_baseline,
    );

    let border = if data.focus {
        theme::frame_focus()
    } else {
        theme::frame_idle()
    };
    let block = pane_block(FocusPane::Transcript.label(), data.focus, border).title_bottom(
        Line::from(fit_spans(chip_row(data.follow, report), title_width(rect))),
    );
    frame.render_widget(block, rect);
    report
}

/// Draw the conversation region of `rect` and report where the viewport landed.
///
/// G7 in code: with follow on the viewport is pinned to the bottom, so new output
/// moves it; with follow off the offset is only ever *lowered* to stay legal,
/// never raised, so new output cannot move the viewport and instead shows up as
/// an unseen-line count.
pub(crate) fn draw_body(
    frame: &mut Frame,
    rect: Rect,
    body: &TranscriptBody<'_>,
    follow: bool,
    scroll: u16,
    unseen_baseline: Option<usize>,
) -> BodyReport {
    let inner = rect.inner(Margin::new(1, 1));
    let height = usize::from(inner.height);
    let lines = body_lines(body);
    // The scrollbar owns the last column of `inner`, so it is decided before the
    // body is counted: when the body wraps to more rows than fit, the text column
    // shrinks by one and the row count is re-measured at that width. Counting and
    // drawing must use the *same* width or `max_scroll` is a lie.
    let total = count_wrapped_rendered_lines(&lines, usize::from(inner.width));
    let scrollbar = total > height && inner.width > 1;
    let text_width = if scrollbar {
        inner.width - 1
    } else {
        inner.width
    };
    let total = if scrollbar {
        count_wrapped_rendered_lines(&lines, usize::from(text_width))
    } else {
        total
    };
    let max_scroll = total.saturating_sub(height);
    let offset = if follow {
        max_scroll
    } else {
        usize::from(scroll).min(max_scroll)
    };
    // Two readings of "unseen": the distance still below the viewport, and the
    // output that arrived since the viewport last sat at its bottom. The larger
    // is the honest count — an offset re-pinned by a resize clamp must never turn
    // `↓ 3 new` into a silent "you are caught up" (G7).
    let unseen = if follow {
        0
    } else {
        let below = max_scroll - offset;
        let grown = unseen_baseline.map_or(0, |base| total.saturating_sub(base));
        below.max(grown)
    };

    if height > 0 && inner.width > 0 {
        if lines.is_empty() {
            // §8.1 empty state — but never over pixels another pass already drew.
            // A pane that is handed no conversation at all leaves the frame to its
            // owner rather than painting over it.
            if !occupied(frame, inner) {
                let paragraph = Paragraph::new(Line::styled(EMPTY_STATE, theme::text_dim()))
                    .wrap(Wrap { trim: true });
                frame.render_widget(paragraph, inner);
            }
        } else {
            let text = Rect {
                width: text_width,
                ..inner
            };
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((offset as u16, 0));
            frame.render_widget(paragraph, text);
            if scrollbar {
                let mut state = ScrollbarState::new(max_scroll).position(offset);
                frame.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight),
                    inner,
                    &mut state,
                );
            }
        }
    }

    BodyReport {
        scroll: offset as u16,
        max_scroll,
        unseen,
        total_lines: total,
    }
}

/// Render every message and streaming region into styled lines with verdict
/// markers emphasised. Role presentation (user / assistant / steer / tool) comes
/// from `formatting::render_message_lines`, the same path the chat pane uses.
fn body_lines(body: &TranscriptBody<'_>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // The renderer caches per-message lines (`cached_rendered_messages`) whenever
    // a transcript-irrelevant frame is drawn; when the cache is fresh it already
    // holds the same styled lines `render_message_lines` would produce, so the
    // hot path re-uses it instead of re-parsing markdown every 30 ms.
    if let Some(rendered) = body.rendered
        && rendered.len() == body.messages.len()
    {
        for line in rendered {
            lines.extend(verdict_marks(line.clone()));
        }
    } else {
        for message in body.messages {
            lines.extend(verdict_marks(render_message_lines(
                message,
                body.show_thought,
            )));
        }
    }
    if body.show_thought && !body.thought.is_empty() {
        lines.extend(verdict_marks(thought_lines(body.thought)));
    }
    if !body.content.is_empty() {
        lines.extend(verdict_marks(stream_lines(body.content)));
    }
    if !body.steer.is_empty() {
        lines.extend(verdict_marks(steer_lines(body.steer, body.messages)));
    }
    lines
}

/// The streaming reasoning region (§8.2), think-tag free, dimmed and italic.
fn thought_lines(thought: &str) -> Vec<Line<'static>> {
    let style = theme::text_dim().add_modifier(ratatui::style::Modifier::ITALIC);
    let mut lines = Vec::new();
    for raw in thought.lines() {
        let line = raw.replace('\t', "    ");
        if is_think_tag(&line) {
            continue;
        }
        let cleaned = format_terminal_math(&strip_think_tags(&line));
        if cleaned.trim().is_empty() && !line.trim().is_empty() {
            continue;
        }
        lines.push(Line::from(Span::styled(cleaned, style)));
    }
    lines
}

/// The streaming content region: fenced text becomes markdown, anything else
/// stays literal text with the terminal-math and ANSI bridges applied.
fn stream_lines(content: &str) -> Vec<Line<'static>> {
    if content.contains("```") {
        return render_markdown_lines(&strip_think_tags(content));
    }
    let mut lines = Vec::new();
    for raw in content.lines() {
        let line = raw.replace('\t', "    ");
        if is_think_tag(&line) {
            continue;
        }
        let cleaned = format_terminal_math(&strip_think_tags(&line));
        if cleaned.trim().is_empty() && !line.trim().is_empty() {
            continue;
        }
        if cleaned.contains('\x1b') {
            lines.extend(parse_ansi_lines(&cleaned));
        } else {
            lines.push(Line::from(Span::styled(cleaned, theme::text_primary())));
        }
    }
    lines
}

/// The streaming steer sentence, labelled once so a growing `Marmennill:` block
/// never grows a second label.
fn steer_lines(steer: &str, messages: &[String]) -> Vec<Line<'static>> {
    let already_labelled = messages
        .last()
        .is_some_and(|msg| msg.starts_with("Marmennill: "));
    let prefix = if already_labelled { "" } else { "Marmennill: " };
    let text = format!("{prefix}{steer}");
    text.lines()
        .map(|line| {
            Line::from(Span::styled(
                line.replace('\t', "    "),
                theme::chip_active(),
            ))
        })
        .collect()
}

/// Bare `<think>` / `<thought>` tags carry no information and are dropped.
fn is_think_tag(line: &str) -> bool {
    let trimmed = line.trim();
    matches!(
        trimmed,
        "<think>"
            | "</think>"
            | "<thought>"
            | "</thought>"
            | "<think></think>"
            | "<thought></thought>"
    )
}

/// Emphasise every line carrying a terminal verdict — `MISSION COMPLETE`,
/// `REVOKED`, `FAILED`, `REPLAN REQUIRED` must stay visually distinguishable from
/// each other and from body text. Detection is delegated to [`crate::markers`],
/// so the marker vocabulary stays owned there.
pub(crate) fn verdict_marks(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines.into_iter().map(verdict_mark).collect()
}

/// Which verdict a line states, and how it is painted. The four verdicts of §8
/// each get their own palette slot so a glance tells them apart; the mapping
/// never names a `Color` (spec §7.1 keeps those in `theme`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The worker claims the task is done.
    Complete,
    /// A claimed completion was taken back by the validator/arbiter, so the line
    /// reads as a verdict but *not* as a success.
    Revoked,
    /// The task did not complete.
    Failed,
    /// The task did not complete and the plan itself must be revisited.
    Replan,
}

impl Verdict {
    /// The §7.2 glyph that flags this verdict.
    const fn glyph(self) -> Glyph {
        match self {
            Self::Complete => Glyph::Done,
            Self::Revoked | Self::Failed => Glyph::Failed,
            Self::Replan => Glyph::Pending,
        }
    }

    /// The palette slot this verdict is painted in — four verdicts, four slots,
    /// so `REVOKED` can never be mistaken for the worker's own `FAILED`.
    fn style(self) -> Style {
        match self {
            Self::Complete => theme::chip_done(),
            Self::Revoked => theme::chip(),
            Self::Failed => theme::error(),
            Self::Replan => theme::chip_pending(),
        }
    }

    /// The badge painted in front of the line.
    fn badge(self) -> Span<'static> {
        Span::styled(self.glyph().symbol().to_string(), self.style())
    }
}

/// One line, prefixed with the verdict glyph when it *leads* with a marker. The
/// whole line takes the verdict colour, which is what makes a verdict readable
/// apart from body text at a glance.
fn verdict_mark(line: Line<'static>) -> Line<'static> {
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let Some(verdict) = verdict_of(&text) else {
        return line;
    };

    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    spans.push(verdict.badge());
    spans.push(Span::styled(" ".to_string(), theme::text_dim()));
    for span in line.spans {
        spans.push(Span::styled(
            span.content.into_owned(),
            span.style.patch(verdict.style()),
        ));
    }
    Line::from(spans)
}

/// `true` when `head` opens with `token` as a whole word. Every verdict is
/// matched against the *first non-blank content of the line*, which is the
/// line-initial placement `markers` treats as authoritative; a marker word that
/// merely appears somewhere in the sentence is prose and stays body text.
fn leads_with(head: &str, token: &str) -> bool {
    let Some(seen) = head.get(..token.len()) else {
        return false;
    };
    if !seen.eq_ignore_ascii_case(token) {
        return false;
    }
    // The token must end at a word boundary: `FAILED:` and `FAILED (…)` count,
    // a longer word or identifier that merely starts with the marker does not
    // (the same underscore rule `markers` applies to its own literals).
    !head[token.len()..]
        .chars()
        .next()
        .is_some_and(|next| next.is_alphanumeric() || next == '_')
}

/// The verdict a line leads with. Verdicts are emitted in marker-led shapes
/// (`MISSION COMPLETE (t-007)`, `REVOKED (t-007)`, `FAILED: <reason>`,
/// `REPLAN REQUIRED (<task>): …`), and only that shape is emphasised: a prose
/// mention of a marker word in the middle of a sentence stays body text, so a
/// line like "I did not emit MISSION COMPLETE" can never be painted as a verdict.
fn verdict_of(text: &str) -> Option<Verdict> {
    let head = text.trim_start();
    // Order follows the `markers` precedence table: replan is the strongest
    // statement, then failure, then completion, then a revoked completion.
    if leads_with(head, markers::MARKER_REPLAN) {
        Some(Verdict::Replan)
    } else if leads_with(head, markers::MARKER_FAILED) && markers::has_failure_marker(head) {
        Some(Verdict::Failed)
    } else if markers::starts_with_complete(head) {
        Some(Verdict::Complete)
    } else if leads_with(head, markers::REVOCATION_TOKEN) {
        Some(Verdict::Revoked)
    } else {
        None
    }
}

/// The chip row of the pane's bottom border, in drop order for a narrow pane
/// (§7.4): the follow chip first because it is the state echo G5/G7 demand, the
/// scroll readout last because it is a measurement, not a state. The readout is
/// only printed when there is somewhere to scroll — a `0/0` on a body that fits
/// is noise, and it would cost the follow chip its width.
///
/// The row is fitted to [`title_width`] (the border row is `rect.width - 2`), not to
/// `rect.width`: fitting it to the rect let ratatui cut the last two cells silently,
/// ellipsis included. This is also the frame's **only** scroll-state chip — the pane
/// owns the viewport, so it is the only thing that can honestly report it, and the
/// status bar used to print the same `● LIVE` a second time per frame.
fn chip_row(follow: bool, report: BodyReport) -> Vec<Span<'static>> {
    let mut chips = vec![
        Span::styled(" ".to_string(), theme::text_primary()),
        scroll_chip(follow, report.unseen),
    ];
    if report.max_scroll > 0 {
        chips.push(position_chip(report));
    }
    chips
}

/// The G7 chip: `● LIVE` while following, otherwise how many lines sit below the
/// viewport. With follow off and nothing unseen the chip still names the state,
/// so no key press is ever silent (G5).
fn scroll_chip(follow: bool, unseen: usize) -> Span<'static> {
    if follow {
        Span::styled(format!(" {} LIVE ", theme::FOCUS_BADGE), theme::chip_done())
    } else if unseen > 0 {
        Span::styled(format!(" \u{2193} {unseen} new "), theme::chip_pending())
    } else {
        Span::styled(" \u{2193} follow off ".to_string(), theme::chip_pending())
    }
}

/// The scroll readout that makes the scrollbar's meaning legible in text.
fn position_chip(report: BodyReport) -> Span<'static> {
    let offset = report.scroll;
    let max = report.max_scroll;
    Span::styled(format!(" scroll {offset}/{max} "), theme::chip())
}

/// Whether `rect` already carries pixels drawn by an earlier pass of this frame.
fn occupied(frame: &mut Frame, rect: Rect) -> bool {
    let buffer = frame.buffer_mut();
    let end_x = rect.x.saturating_add(rect.width);
    let end_y = rect.y.saturating_add(rect.height);
    for y in rect.y..end_y {
        for x in rect.x..end_x {
            if let Some(cell) = buffer.cell((x, y))
                && !cell.symbol().trim().is_empty()
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::ops::Range;
    use unicode_width::UnicodeWidthStr;

    /// Messages with greppable, distinct labels.
    fn fixture(range: Range<usize>) -> Vec<String> {
        range
            .map(|i| format!("User: line {i:02} of the transcript"))
            .collect()
    }

    fn body_of(messages: &[String]) -> TranscriptBody<'_> {
        TranscriptBody {
            messages,
            rendered: None,
            thought: "",
            content: "",
            steer: "",
            show_thought: false,
        }
    }

    fn streamed<'a>(
        messages: &'a [String],
        thought: &'a str,
        content: &'a str,
        steer: &'a str,
        show_thought: bool,
    ) -> TranscriptBody<'a> {
        TranscriptBody {
            messages,
            rendered: None,
            thought,
            content,
            steer,
            show_thought,
        }
    }

    fn rows(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Vec<String> {
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    /// Draw the body of one transcript frame and hand back rows plus report.
    fn draw_body_rows(
        width: u16,
        height: u16,
        body: &TranscriptBody<'_>,
        follow: bool,
        scroll: u16,
    ) -> (Vec<String>, BodyReport) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut report = BodyReport {
            scroll: 0,
            max_scroll: 0,
            unseen: 0,
            total_lines: 0,
        };
        terminal
            .draw(|frame| {
                report = draw_body(
                    frame,
                    Rect::new(0, 0, width, height),
                    body,
                    follow,
                    scroll,
                    None,
                );
            })
            .unwrap();
        (rows(terminal.backend().buffer(), width, height), report)
    }

    /// Draw the whole pane (body plus chrome) with production data.
    fn draw_pane(width: u16, height: u16, data: &TranscriptData) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, width, height), data);
            })
            .unwrap();
        rows(terminal.backend().buffer(), width, height)
    }

    #[test]
    fn empty_transcript_shows_the_spec_empty_state() {
        let data = TranscriptData {
            focus: true,
            follow: true,
            scroll: 0,
            body: TranscriptBody::default(),
            unseen_baseline: None,
        };
        let text = draw_pane(60, 8, &data).join("\n");
        assert!(text.contains("No output yet"), "got: {text}");
        assert!(text.contains("press Enter"), "got: {text}");
    }

    #[test]
    fn an_existing_chat_body_is_never_painted_over() {
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                // Stand-in for the legacy chat pass, which already put text down.
                frame.render_widget(
                    Paragraph::new("pre-existing chat body"),
                    Rect::new(1, 1, 38, 4),
                );
                let data = TranscriptData {
                    focus: false,
                    follow: true,
                    scroll: 0,
                    body: TranscriptBody::default(),
                    unseen_baseline: None,
                };
                render(frame, Rect::new(0, 0, 40, 6), &data);
            })
            .unwrap();
        let text = rows(terminal.backend().buffer(), 40, 6).join("\n");
        assert!(text.contains("pre-existing chat body"), "got: {text}");
        assert!(!text.contains("No output yet"), "got: {text}");
    }

    #[test]
    fn follow_on_pins_the_viewport_to_the_newest_line() {
        let messages = fixture(0..30);
        let (seen, report) = draw_body_rows(50, 12, &body_of(&messages), true, 0);
        let text = seen.join("\n");
        assert!(text.contains("line 29"), "got: {text}");
        assert!(!text.contains("line 00"), "got: {text}");
        assert_eq!(report.unseen, 0);
    }

    #[test]
    fn follow_off_keeps_the_viewport_still_and_counts_new_lines() {
        let messages = fixture(0..30);
        // The operator is at the bottom with follow switched off: the offset is
        // clamped to the tail and nothing is unseen.
        let (first_seen, first) = draw_body_rows(50, 12, &body_of(&messages), false, 999);
        assert_eq!(first.scroll, 20);
        assert_eq!(first.unseen, 0);

        // Five more messages arrive while follow is off.
        let mut grown = messages.clone();
        grown.extend(fixture(30..35));
        let (second_seen, second) = draw_body_rows(50, 12, &body_of(&grown), false, first.scroll);

        // The text columns are identical: with follow off the viewport must not
        // move. Only the scrollbar thumb on the last column re-proportions.
        let body_text = |rows: &[String]| -> Vec<String> {
            rows.iter()
                .map(|row| row.chars().take(48).collect())
                .collect::<Vec<String>>()
        };
        assert_eq!(
            body_text(&first_seen),
            body_text(&second_seen),
            "follow off must not move the body"
        );
        assert_eq!(first.scroll, second.scroll);
        assert_eq!(second.unseen, 5);
        assert!(
            second.max_scroll > first.max_scroll,
            "scroll range must grow"
        );
        let chip = scroll_chip(false, second.unseen);
        assert_eq!(chip.content.as_ref(), " \u{2193} 5 new ");
    }

    /// The honesty half of G7 with a **real** baseline. The distance-below reading
    /// alone (`max_scroll - scroll`) reports 0 here: the pane grew taller, so the
    /// offset the renderer handed back is now the bottom of a longer body, and the
    /// chip would say "you are caught up" about five lines nobody has seen. The
    /// baseline the renderer records when the viewport last sat at the bottom is what
    /// makes the count come out right.
    #[test]
    fn a_baseline_keeps_the_new_count_truthful_when_the_viewport_is_repinned() {
        let messages = fixture(0..30);
        // Frame 1: 12 rows, follow off, the operator is at the tail of the body.
        let (_, first) = draw_body_rows(50, 12, &body_of(&messages), false, 999);
        assert_eq!(first.scroll, 20);
        assert_eq!(first.total_lines, 30);
        assert_eq!(
            first.unseen, 0,
            "at the tail with nothing below: nothing unseen"
        );

        // Frame 2: five lines arrive and the pane is 19 rows tall, so the very same
        // offset is now pinned to the bottom of a longer body.
        let mut grown = messages.clone();
        grown.extend(fixture(30..35));
        let repinned = |baseline: Option<usize>| {
            let backend = TestBackend::new(50, 19);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut report = BodyReport {
                scroll: 0,
                max_scroll: 0,
                unseen: 0,
                total_lines: 0,
            };
            terminal
                .draw(|frame| {
                    report = draw_body(
                        frame,
                        Rect::new(0, 0, 50, 19),
                        &body_of(&grown),
                        false,
                        first.scroll,
                        baseline,
                    );
                })
                .unwrap();
            report
        };
        let repinned_at_tail = repinned(None);
        assert_eq!(
            usize::from(repinned_at_tail.scroll),
            repinned_at_tail.max_scroll
        );
        // Without the baseline the pane would claim the operator is caught up.
        assert_eq!(repinned_at_tail.unseen, 0, "the lie this exists to fix");

        // With the baseline frame 1 left behind, the five unseen lines are counted
        // and the chip spells them.
        let honest = repinned(Some(first.total_lines));
        assert_eq!(honest.unseen, 5);
        assert_eq!(
            scroll_chip(false, honest.unseen).content.as_ref(),
            " \u{2193} 5 new "
        );
    }

    #[test]
    fn follow_off_scroll_is_only_ever_lowered_to_stay_legal() {
        let messages = fixture(0..30);
        let (_, report) = draw_body_rows(50, 12, &body_of(&messages), false, 999);
        assert_eq!(report.scroll, 20);
        assert_eq!(report.unseen, 0);
    }

    #[test]
    fn chips_name_live_and_behind_states_never_silently() {
        assert_eq!(
            scroll_chip(true, 0).content.as_ref(),
            format!(" {} LIVE ", theme::FOCUS_BADGE)
        );
        assert_eq!(scroll_chip(false, 7).content.as_ref(), " \u{2193} 7 new ");
        assert_eq!(
            scroll_chip(false, 0).content.as_ref(),
            " \u{2193} follow off "
        );
        let text = draw_pane(
            60,
            6,
            &TranscriptData {
                focus: false,
                follow: false,
                scroll: 0,
                body: TranscriptBody::default(),
                unseen_baseline: None,
            },
        )
        .join("\n");
        assert!(text.contains("follow off"), "got: {text}");
    }

    #[test]
    fn scrollbar_and_report_track_the_wrapped_body_height() {
        let messages = fixture(0..30);
        let (seen, report) = draw_body_rows(50, 12, &body_of(&messages), true, 0);
        // 10 usable rows, 30 lines: everything above the tail is scrollback.
        assert_eq!(report.max_scroll, 20);
        assert_eq!(report.scroll, 20);
        assert!(
            seen.iter()
                .any(|row| row.ends_with('\u{2502}') || row.contains('\u{25bc}')),
            "scrollbar expected: {seen:?}"
        );
        assert_eq!(position_chip(report).content.as_ref(), " scroll 20/20 ");
    }

    #[test]
    fn the_scrollbar_never_covers_body_text() {
        // One message long enough to wrap, in a pane narrow enough to need the
        // scrollbar: the text column and the counted column count must agree, so
        // the thumb may not be painted on top of body characters.
        let long = vec![format!("User: {}", "x ".repeat(200))];
        let (seen, report) = draw_body_rows(30, 8, &body_of(&long), true, 0);
        assert!(report.max_scroll > 0, "body must overflow: {report:?}");
        // `inner` spans x = 1..=28; the scrollbar owns x = 28 and text stops at 27.
        let scrollbar_column: Vec<char> = seen
            .iter()
            .skip(1)
            .take(6)
            .map(|row| row.chars().nth(28).expect("column 28 exists"))
            .collect();
        assert!(
            !scrollbar_column.contains(&'x'),
            "body text under the scrollbar: {scrollbar_column:?} in {seen:?}"
        );
        assert!(
            scrollbar_column
                .iter()
                .any(|cell| matches!(cell, '\u{25b2}' | '\u{25bc}' | '\u{2588}')),
            "scrollbar thumb expected in {scrollbar_column:?}"
        );
        let text_row = seen[1].chars().skip(1).take(27).collect::<String>();
        assert!(
            text_row.contains('x'),
            "text column lost a column: {text_row:?} in {seen:?}"
        );
    }

    #[test]
    fn the_four_verdicts_are_distinguishable_from_each_other_and_from_body() {
        let backend = TestBackend::new(50, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let marks = verdict_marks(vec![
                    Line::raw(markers::MARKER_COMPLETE),
                    Line::raw(format!("{} (t-007)", markers::REVOCATION_TOKEN)),
                    Line::raw(format!("{}: build is red", markers::MARKER_FAILED)),
                    Line::raw(markers::MARKER_REPLAN),
                    Line::raw("ordinary body text"),
                ]);
                frame.render_widget(Paragraph::new(marks), Rect::new(1, 1, 48, 7));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = rows(buffer, 50, 9).join("\n");
        assert!(text.contains('\u{2713}'), "got: {text}");
        assert!(text.contains('\u{2717}'), "got: {text}");
        assert!(text.contains('\u{29d7}'), "got: {text}");

        let complete = buffer[(1, 1)].style();
        let revoked = buffer[(1, 2)].style();
        let failed = buffer[(1, 3)].style();
        let replan = buffer[(1, 4)].style();
        let plain = buffer[(1, 5)].style();
        // Each verdict keeps its own palette slot, and none of them is body text.
        assert_eq!(complete.fg, theme::chip_done().fg);
        assert_eq!(revoked.fg, theme::chip().fg);
        assert_eq!(revoked.bg, theme::chip().bg);
        assert_eq!(failed.fg, theme::error().fg);
        assert!(failed.add_modifier.contains(ratatui::style::Modifier::BOLD));
        assert_eq!(replan.fg, theme::chip_pending().fg);
        // Body text keeps whatever colour the message carried, which is none of
        // the four verdict slots.
        assert_ne!(plain.fg, complete.fg);
        assert_ne!(plain.fg, revoked.fg);
        assert_ne!(plain.fg, failed.fg);
        assert_ne!(plain.fg, replan.fg);
        let styles = [complete, revoked, failed, replan];
        for left in 0..styles.len() {
            for right in (left + 1)..styles.len() {
                assert_ne!(
                    styles[left].fg, styles[right].fg,
                    "verdict colours collide: {styles:?}"
                );
            }
        }
    }

    #[test]
    fn a_revoked_completion_is_a_verdict_but_never_a_completion() {
        let revoked = format!("{} (t-007)", markers::REVOCATION_TOKEN);
        assert_eq!(verdict_of(&revoked), Some(Verdict::Revoked));
        assert_eq!(
            verdict_of(markers::MARKER_COMPLETE),
            Some(Verdict::Complete)
        );
        // A revoked claim is badged with the failure glyph in its own colour, so
        // it can never be read as the green completion it used to claim.
        let line = verdict_mark(Line::raw(revoked));
        assert_eq!(
            line.spans.first().map(|span| span.content.as_ref()),
            Some(Glyph::Failed.symbol().to_string().as_str()),
            "revoked line must not carry the done glyph: {line:?}"
        );
        assert_ne!(Verdict::Revoked.style(), Verdict::Complete.style());
        assert_ne!(Verdict::Revoked.style(), Verdict::Failed.style());
        assert_ne!(Verdict::Revoked.style(), Verdict::Replan.style());
        assert_ne!(Verdict::Complete.style(), Verdict::Failed.style());
        assert_ne!(Verdict::Complete.style(), Verdict::Replan.style());
        assert_ne!(Verdict::Failed.style(), Verdict::Replan.style());
    }

    #[test]
    fn verdict_glyphs_are_only_ever_badged_at_the_line_start() {
        // Indentation does not move a verdict away from the line start — the
        // `markers` grammar calls the first non-blank content `LineStart`.
        assert_eq!(
            verdict_of("    FAILED: build is red"),
            Some(Verdict::Failed)
        );
        // The same words anywhere else in the sentence are prose.
        for prose in [
            "the run FAILED somewhere and MISSION COMPLETE was never emitted",
            "see the REVOKED notice further up the transcript",
            "we logged a REPLAN REQUIRED warning once",
            "0 failed",
        ] {
            assert_eq!(verdict_of(prose), None, "{prose} must stay body text");
        }
        // A longer identifier that merely starts with a token is not a verdict.
        assert_eq!(verdict_of("FAILED_TO_CONNECT is a constant name"), None);
        assert_eq!(verdict_of("COMPLETELY unrelated prose"), None);
        // And a bare line is not a verdict either.
        assert_eq!(verdict_of(""), None);
    }

    #[test]
    fn the_pane_chrome_never_invents_a_round_counter() {
        // Spec §8.4 and gate (c): neither a validation round nor an arbitration
        // round is data today, so nothing this pane writes by itself may count
        // one. (Message *text* is echoed verbatim — that prose belongs to the
        // session, and suppressing it would be its own kind of lie.)
        let reports = [
            BodyReport {
                scroll: 0,
                max_scroll: 0,
                unseen: 0,
                total_lines: 0,
            },
            BodyReport {
                scroll: 3,
                max_scroll: 40,
                unseen: 37,
                total_lines: 45,
            },
        ];
        for follow in [true, false] {
            for report in reports {
                let text: String = chip_row(follow, report)
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                for forbidden in ["val", "round", "Round", "review", "turn"] {
                    assert!(
                        !text.contains(forbidden),
                        "invented counter {forbidden:?} in chip row {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_scroll_readout_only_appears_when_there_is_somewhere_to_scroll() {
        let fits = BodyReport {
            scroll: 0,
            max_scroll: 0,
            unseen: 0,
            total_lines: 0,
        };
        assert_eq!(chip_row(true, fits).len(), 2, "no readout on a fit");
        let overflowing = BodyReport {
            scroll: 4,
            max_scroll: 12,
            unseen: 8,
            total_lines: 18,
        };
        assert_eq!(
            chip_row(false, overflowing).len(),
            3,
            "readout when clipped"
        );
    }

    #[test]
    fn chip_row_fits_every_pane_width_it_is_drawn_into() {
        // Measured in terminal cells, not characters: the chips carry the `●`
        // badge and the arrow glyph, and §7.4 forbids a clipped line spilling.
        let report = BodyReport {
            scroll: 7,
            max_scroll: 12,
            unseen: 5,
            total_lines: 17,
        };
        for follow in [true, false] {
            for width in [20u16, 30, 40, 60, 90, 120] {
                let fitted = fit_spans(chip_row(follow, report), width);
                let cells: usize = fitted
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum();
                assert!(
                    cells <= usize::from(width),
                    "chips spill {cells} cells into {width}: {fitted:?}"
                );
                // The follow chip is the first thing dropped from the right, so
                // the state echo survives at every width (§7.4 drop order).
                let text: String = fitted.iter().map(|span| span.content.as_ref()).collect();
                assert!(
                    text.contains(if follow { "LIVE" } else { "new" }),
                    "follow chip lost at width {width}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn negated_marker_mentions_do_not_become_verdicts() {
        let line = verdict_mark(Line::raw(
            "I did not emit MISSION COMPLETE, so nothing is done.",
        ));
        assert!(
            line.spans
                .first()
                .is_some_and(|span| span.content.starts_with('I')),
            "verdict badge must not be added"
        );
    }

    #[test]
    fn thinking_respects_the_visible_toggle() {
        let messages: Vec<String> = Vec::new();
        let hidden = draw_body_rows(
            50,
            10,
            &streamed(&messages, "secret reasoning stream", "", "", false),
            true,
            0,
        )
        .0
        .join("\n");
        assert!(!hidden.contains("secret reasoning"), "got: {hidden}");

        let shown = draw_body_rows(
            50,
            10,
            &streamed(&messages, "secret reasoning stream", "", "", true),
            true,
            0,
        )
        .0
        .join("\n");
        assert!(shown.contains("secret reasoning"), "got: {shown}");
    }

    #[test]
    fn streaming_content_and_steer_regions_are_drawn() {
        let messages: Vec<String> = Vec::new();
        let text = draw_body_rows(
            50,
            10,
            &streamed(
                &messages,
                "",
                "streaming delta text",
                "steered sentence",
                false,
            ),
            true,
            0,
        )
        .0
        .join("\n");
        assert!(text.contains("streaming delta text"), "got: {text}");
        assert!(text.contains("Marmennill: steered sentence"), "got: {text}");
    }

    #[test]
    fn markdown_and_ansi_bodies_render_through_the_existing_bridge() {
        let messages: Vec<String> = vec![
            "here is code:\n```rust\nfn main() {}\n```".to_string(),
            "\x1b[31mred ansi text\x1b[0m".to_string(),
        ];
        let text = draw_body_rows(50, 12, &body_of(&messages), true, 0)
            .0
            .join("\n");
        assert!(text.contains("fn main() {}"), "got: {text}");
        assert!(text.contains("red ansi text"), "got: {text}");
        assert!(
            !text.contains('\x1b'),
            "escape bytes must never reach the screen"
        );
    }

    #[test]
    fn role_labels_survive_the_shared_bridge() {
        // §3/§5: user, assistant, steer and status lines all arrive as plain
        // message strings; the pane presents them through the same bridge the chat
        // pane uses instead of inventing its own role grammar.
        let messages: Vec<String> = vec![
            "User: rebuild the TUI".to_string(),
            "Marmennill: here is what I did".to_string(),
            "User (Steer): focus the roster instead".to_string(),
            "[Status] 1a 0p 2q (Ready)".to_string(),
        ];
        let (seen, report) = draw_body_rows(50, 12, &body_of(&messages), true, 0);
        let text = seen.join("\n");
        assert_eq!(report.max_scroll, 0, "four rows fit: {report:?}");
        for label in [
            "User: rebuild the TUI",
            "Marmennill: here is what I did",
            "User (Steer): focus the roster instead",
            "[Status] 1a 0p 2q (Ready)",
        ] {
            assert!(text.contains(label), "role line missing: {text}");
        }
    }

    #[test]
    fn tool_call_and_tool_result_lines_stay_marked() {
        let messages: Vec<String> = vec![
            "[Tool Call] read_file(\"src/main.rs\")".to_string(),
            "[Tool Result] 42 lines".to_string(),
        ];
        let text = draw_body_rows(50, 8, &body_of(&messages), true, 0)
            .0
            .join("\n");
        assert!(text.contains("[Tool Call] read_file"), "got: {text}");
        assert!(text.contains("[Tool Result] 42 lines"), "got: {text}");
    }

    #[test]
    fn chrome_lines_never_spill_their_rect() {
        let seen = draw_pane(
            30,
            6,
            &TranscriptData {
                focus: true,
                follow: false,
                scroll: 3,
                body: TranscriptBody::default(),
                unseen_baseline: None,
            },
        );
        for (y, row) in seen.iter().enumerate() {
            assert!(
                row.chars().count() <= 30,
                "row {y} spills its rect: {row:?}"
            );
        }
        assert!(
            seen[5].contains('\u{2193}'),
            "follow chip expected on the chip row: {:?}",
            seen[5]
        );
    }

    #[test]
    fn the_focus_badge_and_border_only_ever_track_focus() {
        // §10.12 for this pane: the `●` badge and the border colour move together
        // with `focus`, and nothing else in the pane hints at focus.
        for focus in [true, false] {
            let backend = TestBackend::new(40, 8);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        Rect::new(0, 0, 40, 8),
                        &TranscriptData {
                            focus,
                            follow: true,
                            scroll: 0,
                            body: TranscriptBody::default(),
                            unseen_baseline: None,
                        },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let title: String = (0..40u16).map(|x| buffer[(x, 0)].symbol()).collect();
            assert_eq!(
                title.contains(theme::FOCUS_BADGE),
                focus,
                "badge follows focus only: {title}"
            );
            assert_eq!(
                buffer[(0, 1)].style().fg,
                if focus {
                    theme::frame_focus().fg
                } else {
                    theme::frame_idle().fg
                },
                "border colour follows focus only"
            );
        }
    }

    #[test]
    fn degenerate_rects_are_ignored_without_panicking() {
        let backend = TestBackend::new(4, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let empty = TranscriptBody {
                    messages: &[],
                    rendered: None,
                    thought: "",
                    content: "",
                    steer: "",
                    show_thought: false,
                };
                draw_body(frame, Rect::new(0, 0, 0, 0), &empty, true, 0, None);
                draw_body(frame, Rect::new(0, 0, 1, 2), &empty, false, 9, None);
                render(
                    frame,
                    Rect::new(0, 0, 2, 2),
                    &TranscriptData {
                        focus: false,
                        follow: true,
                        scroll: 0,
                        body: TranscriptBody::default(),
                        unseen_baseline: None,
                    },
                );
            })
            .unwrap();
    }
}
