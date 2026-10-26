//! Worker activity feed — spec §3 (Worker activity pane), §2.3 (M3 roster
//! `Min(4)` / detail `Min(3)`), §2.5 (detail absent below 60 columns), §7.2
//! (state glyphs), §8.1 (empty state), §8.3 (every live row reachable, counts
//! derived from the registry-projected rows) and G8 (no 3-row silent cap).
//!
//! The pane owns its whole inner area: the roster, the per-row metrics, the
//! selection marker, the roster scrollbar, the selected worker's
//! `[Thinking]/[Output]/[Logs]` detail half and the chip row. Row existence,
//! liveness, queue and pause state stay registry-projected — the rows handed in
//! by `TuiRenderer` are the only state owner (spec §8.3), and the per-frame
//! [`orchestrator::worker_entries`] snapshot is read only for the fields the row
//! does not carry (`agent_name`, `elapsed_secs`, `implementation_turns`, the two
//! token counters, and the `Audit`/`Dup` badge via `helpers::subagent_row_badge`),
//! exactly as the legacy subagents pane does.
//!
//! Rendered from the row: `val N` is the row's own `validation_rounds` and `turn N`
//! its `implementation_turns` — both landed with plumbing delta **D1** (spec §9.3), so
//! the numbers are the registry's counters and not a guess. What is still never
//! rendered is an arbitration-round number: `round_started(round)` discards its
//! argument in both hosts, so that counter is not data (spec §8.4, §2.2).
//!
//! ## The row model after D1
//!
//! [`ActivityData`] is built by `TuiRenderer::draw_new_ui` and carries `focus` /
//! `rows` / `selected` / `detail_present` / `scroll` / `list_scroll` /
//! `show_thought` / `thinking_budget`. The rows now carry the four lifecycle fields
//! D1 added to [`SubagentDetail`] (`status_label`, `implementation_turns`,
//! `validation_rounds`, `latest_validator_feedback`), mirrored from
//! [`orchestrator::WorkerEntry`] by `TuiRenderer::set_subagents`.
//!
//! * `status_label` is what lets an aborted or validator-rejected worker read
//!   `✗ failed` instead of `✓ done`: with only `is_active` / `paused` / `queue`
//!   projected, "not running" was all the row could honestly say (§7.2, DATA §4.1
//!   gap 1). A row with no projected label still falls back to the liveness glyph —
//!   the pane never guesses a terminal state from prose.
//! * `latest_validator_feedback` is printed verbatim under `[Validator]` in the
//!   detail half, so a rejected row states why it was rejected.
//!
//! With `show_thought` plumbed, the detail half prints the buffered reasoning under
//! the `[Thinking]` header when the toggle is on, and keeps the size-only line when
//! it is off — so `Ctrl+T` is honoured instead of bypassed.

use super::transcript::verdict_marks;
use super::{fit_spans, pane_block, spans_width, title_width};
use crate::orchestrator::WorkerEntry;
use crate::ui::SubagentDetail;
use crate::ui::helpers::subagent_row_badge;
use crate::ui::tui::formatting::{
    count_wrapped_rendered_lines, format_terminal_math, parse_ansi_lines, render_markdown_lines,
};
use crate::ui::tui::state::FocusPane;
use crate::ui::tui::theme::{self, Glyph};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

/// The §8.1 workers empty state, spelled once — reached by `Ctrl+A`/`F5` with
/// zero rows, which is what fixes the silent no-op (G5, UX friction 3).
pub const EMPTY_STATE: &str = "No specialist launched yet.";

/// The one selection marker of the roster (§2.3). It is deliberately *not* the
/// `●` focus badge and not painted in `frame_focus`: §7.3 reserves that colour
/// for the focused border, so a selected row in an unfocused pane can never be
/// mistaken for focus.
const SELECTION_MARKER: &str = " > ";
/// Blank gutter of the same width on unselected rows, so the names line up.
const SELECTION_GUTTER: &str = "   ";

/// Roster floor of §2.3 (`vertical([Min(4), Min(3)])`).
const ROSTER_MIN: u16 = 4;
/// Detail floor of §2.3.
const DETAIL_MIN: u16 = 3;

/// What the worker pane draws this frame.
#[derive(Debug, Clone, Copy)]
pub struct ActivityData<'a> {
    /// Whether this pane holds focus.
    pub focus: bool,
    /// The projected worker rows (`SubagentDetail`, the single state owner).
    pub rows: &'a [SubagentDetail],
    /// Selected roster row index (`←`/`→`, `x` cancels this row).
    pub selected: usize,
    /// Whether the detail half may be drawn at all (§2.5, `width < 60`).
    pub detail_present: bool,
    /// Scroll offset of the detail half.
    pub scroll: u16,
    /// The roster's own scroll offset (`subagent_list_scroll`), so `↑`/`↓` can
    /// scroll past the selection instead of the selection re-deriving the view.
    pub list_scroll: u16,
    /// The `Ctrl+T` / `/thought` toggle (§8.2): whether buffered reasoning may be
    /// printed at all.
    pub show_thought: bool,
    /// The selected worker's thinking budget, for the `~N tokens of M budget`
    /// header under `[Thinking]`.
    pub thinking_budget: Option<usize>,
}

/// Where the two scrollable halves landed this frame. The renderer writes these
/// back into `subagent_list_scroll` / `subagent_scroll` / `subagent_width` /
/// `subagent_height` so its `↑`/`↓`/`PgUp` clamps (`events.rs`) and its autoscroll
/// estimate match what was drawn.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkerGeometry {
    /// Offset the roster was drawn at.
    pub roster_scroll: u16,
    /// Offset the detail half was drawn at (0 when there is no detail half).
    pub detail_scroll: u16,
    /// Largest legal detail offset.
    pub detail_max: usize,
    /// Whether a detail half was drawn at all (§2.5 narrow column, §2.3 floors).
    pub detail_present: bool,
    /// Detail text columns the body was counted at.
    pub detail_width: usize,
    /// Detail text rows the body was counted at.
    pub detail_height: usize,
}

/// Where the detail half landed this frame — what the chip row prints, and what
/// the renderer's line-count estimate is re-measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DetailReport {
    /// Whether a detail half was drawn at all.
    pub present: bool,
    /// Offset actually drawn.
    pub scroll: u16,
    /// Largest legal offset for this detail body at this size.
    pub max: usize,
    /// Detail columns the body was counted at (the legacy `subagent_width`).
    pub width: usize,
    /// Detail rows the body was counted at (the legacy `subagent_height`).
    pub height: usize,
}

/// Draw the bordered worker pane: body first, then the block, so the chrome keeps
/// its title and chips while the body keeps its cells.
pub fn render(frame: &mut Frame, rect: Rect, data: &ActivityData<'_>) -> WorkerGeometry {
    // One registry snapshot per frame, the same read model the rows were
    // projected from (spec §3).
    let entries = crate::orchestrator::worker_entries();
    let inner = rect.inner(Margin::new(1, 1));
    let (detail, roster_scroll) = draw_body(frame, inner, data, &entries);

    let border = if data.focus {
        theme::frame_focus()
    } else {
        theme::frame_idle()
    };
    let chips = chip_row(data, detail, title_width(rect));
    let block = pane_block(FocusPane::Workers.label(), data.focus, border)
        .title_bottom(Line::from(fit_spans(chips, title_width(rect))));
    frame.render_widget(block, rect);
    WorkerGeometry {
        roster_scroll,
        detail_scroll: detail.scroll,
        detail_max: detail.max,
        detail_present: detail.present,
        detail_width: detail.width,
        detail_height: detail.height,
    }
}

/// Draw the roster and the detail half inside `inner`, and report the detail
/// scroll position together with the offset the roster was drawn at.
fn draw_body(
    frame: &mut Frame,
    inner: Rect,
    data: &ActivityData<'_>,
    entries: &[WorkerEntry],
) -> (DetailReport, u16) {
    if inner.width == 0 || inner.height == 0 {
        return (
            DetailReport {
                present: false,
                scroll: 0,
                max: 0,
                width: 0,
                height: 0,
            },
            0,
        );
    }
    // The pane owns this area outright and repaints all of it — roster, detail and
    // separator — so an earlier pass's pixels must not survive as ghosts. Unlike
    // the transcript, which probes before painting its empty state so it can never
    // overpaint a body handed to it by another pass, this pane *is* handed the rows
    // (`ActivityData::rows`), so rebuilding the area is strictly better than
    // overlaying it.
    frame.render_widget(Clear, inner);

    if data.rows.is_empty() {
        let paragraph =
            Paragraph::new(Line::styled(EMPTY_STATE, theme::text_dim())).wrap(Wrap { trim: true });
        frame.render_widget(paragraph, inner);
        return (
            DetailReport {
                present: false,
                scroll: 0,
                max: 0,
                width: 0,
                height: 0,
            },
            0,
        );
    }

    let selected = data.selected.min(data.rows.len() - 1);
    let (roster, detail) = split_inner(inner, data.detail_present);

    let row = &data.rows[selected];
    let entry = entries.iter().find(|candidate| candidate.key == row.name);
    let report = draw_detail(frame, detail, row, entry, data.scroll, data);
    let roster_scroll = draw_roster(frame, roster, data, entries, selected);
    (report, roster_scroll)
}

/// The §2.3 split: roster over detail, with the detail half absent when the
/// column is too narrow for it (§2.5) or the pane is too short for both floors.
fn split_inner(inner: Rect, detail_present: bool) -> (Rect, Rect) {
    if !detail_present || inner.height < ROSTER_MIN + DETAIL_MIN {
        return (inner, Rect::default());
    }
    let chunks =
        Layout::vertical([Constraint::Min(ROSTER_MIN), Constraint::Min(DETAIL_MIN)]).split(inner);
    (chunks[0], chunks[1])
}

/// The roster: one row per live worker, **every** row reachable. The legacy pane
/// clamped the list to three rows (`render.rs:647-653`) and dropped the rest in
/// silence; here the roster gets its own chunk, its own scrollbar, and a scroll
/// offset that starts from the renderer's `subagent_list_scroll` and is only moved
/// far enough to keep the selected row on screen.
fn draw_roster(
    frame: &mut Frame,
    roster: Rect,
    data: &ActivityData<'_>,
    entries: &[WorkerEntry],
    selected: usize,
) -> u16 {
    if roster.width == 0 || roster.height == 0 {
        return 0;
    }
    let height = usize::from(roster.height);
    let max_scroll = data.rows.len().saturating_sub(height);
    // The renderer owns the offset (`↑`/`↓` scroll the list past the selection);
    // this pass only pulls it back into range and, as a last resort, reveals the
    // selected row — the same minimal reveal the legacy pane used at
    // `render.rs:619-625`, so selecting never jumps the view more than it must.
    let mut scroll = usize::from(data.list_scroll).min(max_scroll);
    if selected < scroll {
        scroll = selected;
    } else if selected >= scroll + height {
        scroll = selected + 1 - height;
    }
    let scroll = scroll.min(max_scroll);
    // The scrollbar takes the last column, so rows are clipped to what is left.
    let text_width = roster.width.saturating_sub(u16::from(max_scroll > 0));

    let lines: Vec<Line<'static>> = data
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let entry = entries.iter().find(|candidate| candidate.key == row.name);
            roster_row(row, entry, index == selected, text_width)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), roster);
    if max_scroll > 0 {
        let mut state = ScrollbarState::new(max_scroll).position(scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            roster,
            &mut state,
        );
    }
    scroll as u16
}

/// One roster row, clipped to a single line (§7.4: roster lines are single-row and
/// never exceed the rect).
fn roster_row(
    row: &SubagentDetail,
    entry: Option<&WorkerEntry>,
    selected: bool,
    width: u16,
) -> Line<'static> {
    let marker = if selected {
        Span::styled(SELECTION_MARKER.to_string(), theme::chip_active())
    } else {
        Span::styled(SELECTION_GUTTER.to_string(), theme::text_dim())
    };
    let mut spans = vec![marker];
    spans.extend(row_spans(row, entry));
    Line::from(fit_spans(spans, width))
}

/// The identity/state/metric spans of one worker, shared by the roster row and
/// the detail header so the two can never disagree.
///
/// The order is the §2.5 ladder read literally: a roster row is clipped from the
/// right, so the fields the operator needs are the ones that must lead. State
/// glyph and label come first, and the queue rank rides straight behind them —
/// the §2.2 sample row does the same thing (`validator ⏸ (Queued 2/3)`) — because
/// as a tail field `(2/3)` was the part that never survived a clipped row. Then
/// the role and the task id, and only after them the optional metrics (badge,
/// turns, elapsed, ctx), which are the fields the ladder is allowed to eat.
fn row_spans(row: &SubagentDetail, entry: Option<&WorkerEntry>) -> Vec<Span<'static>> {
    // The glyph is driven by the row's own projected state; the registry entry is
    // read only as the fallback for a row the projection has not folded a label onto
    // yet (a row restored from a bundle, or one opened by stream traffic).
    let label = row_status_label(row).or_else(|| entry.map(|entry| entry.status_label.as_str()));
    let glyph = theme::worker_state_glyph(row.is_active, row.paused, row.queue.is_some(), label);
    let mut spans = vec![glyph.tagged()];
    // A queue rank is only meaningful while the row is part of the round.
    if let Some(queue) = row.queue.filter(|_| row.is_active) {
        spans.push(Span::styled(
            format!(" {}/{}", queue.position, queue.of),
            theme::chip_pending(),
        ));
    }
    let name_style = if row.is_active {
        theme::text_primary()
    } else {
        theme::text_dim()
    };
    let role = role_label(row, entry);
    spans.push(Span::styled(format!(" {role}"), name_style));
    if let Some(badge) = subagent_row_badge(entry, &row.name) {
        spans.push(Span::styled(format!(" {badge}"), theme::chip()));
    }
    if let Some(task) = task_id(row, entry) {
        spans.push(Span::styled(format!(" {task}"), theme::text_dim()));
    }
    // `turn N` and `val N` are the registry's own turn/round counters: the row
    // carries `validation_rounds` and `implementation_turns` since D1 (§9.3), and
    // the per-frame entry is the fallback for a row the projection has not folded
    // yet. Both are printed only when a number was actually recorded, and `val` is a
    // **validation** round — never an arbitration round (§8.4).
    let turns = if row.implementation_turns > 0 {
        row.implementation_turns
    } else {
        entry.map(|entry| entry.implementation_turns).unwrap_or(0)
    };
    if turns > 0 {
        spans.push(Span::styled(format!(" turn {turns}"), theme::text_dim()));
    }
    if row.validation_rounds > 0 {
        spans.push(Span::styled(
            format!(" val {}", row.validation_rounds),
            theme::chip_pending(),
        ));
    }
    let elapsed = elapsed_secs(row, entry);
    if elapsed > 0 {
        spans.push(Span::styled(format_elapsed(elapsed), theme::text_dim()));
    }
    let tokens = token_count(row, entry);
    if tokens > 0 {
        spans.push(Span::styled(
            format!(" {} ctx", format_count(tokens)),
            theme::text_dim(),
        ));
    }
    spans
}

/// Detail half for the selected row: header, `[Thinking]` size line, `[Output]`,
/// `[Logs]`, and its own scroll + scrollbar.
fn draw_detail(
    frame: &mut Frame,
    area: Rect,
    row: &SubagentDetail,
    entry: Option<&WorkerEntry>,
    scroll: u16,
    data: &ActivityData<'_>,
) -> DetailReport {
    if area.width == 0 || area.height < 2 {
        return DetailReport {
            present: false,
            scroll: 0,
            max: 0,
            width: 0,
            height: 0,
        };
    }
    frame.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(theme::frame_idle()),
        area,
    );
    let body = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height - 1,
    };
    // Whether the scrollbar is needed is decided at the full width; once it is,
    // the text column loses that last cell and the body is re-counted at it. Only
    // the header line has to be rebuilt: it is the single-row line §7.4 pins to
    // the rect, while every other line wraps inside the paragraph.
    let height = usize::from(body.height);
    let mut lines = detail_lines(
        row,
        entry,
        body.width,
        data.show_thought,
        data.thinking_budget,
    );
    let total = count_wrapped_rendered_lines(&lines, usize::from(body.width));
    let scrollbar = total > height && body.width > 1;
    let text_width = if scrollbar {
        let width = body.width - 1;
        if let Some(header) = lines.first_mut() {
            *header = Line::from(fit_spans(row_spans(row, entry), width));
        }
        width
    } else {
        body.width
    };
    let total = count_wrapped_rendered_lines(&lines, usize::from(text_width));
    let max = total.saturating_sub(height);
    // The offset is renderer state (`subagent_scroll`, already autoscroll-clamped
    // there); it is never raised from here.
    let offset = usize::from(scroll).min(max);
    let text = Rect {
        width: text_width,
        ..body
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((offset as u16, 0)),
        text,
    );
    if scrollbar {
        let mut state = ScrollbarState::new(max).position(offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            body,
            &mut state,
        );
    }
    DetailReport {
        present: true,
        scroll: offset as u16,
        max,
        width: usize::from(area.width.saturating_sub(1)),
        height: usize::from(area.height),
    }
}

/// The detail body: header line plus whichever streamed sections the row carries.
fn detail_lines(
    row: &SubagentDetail,
    entry: Option<&WorkerEntry>,
    width: u16,
    show_thought: bool,
    thinking_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(fit_spans(row_spans(row, entry), width))];
    let mut sections = Vec::new();

    // The registry's latest validator feedback, printed verbatim (plumbing delta D1,
    // §9.3). A `Rejected` row is the case this exists for: the glyph says the
    // deliverable was refused, this says why, and neither is a paraphrase of the
    // other. Nothing is printed when the registry carries no feedback.
    if let Some(feedback) = row
        .latest_validator_feedback
        .as_deref()
        .or_else(|| entry.and_then(|entry| entry.latest_validator_feedback.as_deref()))
        .map(str::trim)
        .filter(|feedback| !feedback.is_empty())
    {
        sections.push(Line::styled("[Validator]".to_string(), theme::error()));
        for line in feedback.lines() {
            sections.push(Line::styled(format!(" {line}"), theme::text_primary()));
        }
    }

    if !row.thinking.is_empty() {
        sections.push(Line::styled("[Thinking]".to_string(), theme::text_dim()));
        if show_thought {
            // §8.2: the reasoning stream is printed while the toggle is on. The
            // budget the worker was launched with rides the header line, from
            // `TuiRenderer::get_thinking_budget` for this row's task.
            let header = match thinking_budget {
                Some(budget) if budget > 0 => {
                    let tokens = thinking_tokens(&row.thinking);
                    format!(
                        " ~{} tokens of {} budget ({} remaining):",
                        format_count(tokens),
                        format_count(budget),
                        format_count(budget.saturating_sub(tokens))
                    )
                }
                _ => " shown (Ctrl+T to hide):".to_string(),
            };
            sections.push(Line::styled(header, theme::text_dim()));
            let style = theme::text_dim().add_modifier(ratatui::style::Modifier::ITALIC);
            for line in row.thinking.lines() {
                sections.push(Line::styled(
                    format!(" {}", crate::ui::tui::formatting::strip_think_tags(line)),
                    style,
                ));
            }
        } else {
            // Toggle off: the pane reports the size of the reasoning buffer and
            // never its text, so a stream the operator switched off with `Ctrl+T`
            // cannot leak through this pane (§8.2).
            sections.push(Line::styled(
                format!(" {} chars — hidden (Ctrl+T)", row.thinking.chars().count()),
                theme::text_dim(),
            ));
        }
    }

    if !row.content.is_empty() {
        sections.push(Line::styled("[Output]".to_string(), theme::frame_focus()));
        if row.content.contains("```") {
            sections.extend(render_markdown_lines(&row.content));
        } else {
            for line in row.content.lines() {
                if line.contains('\x1b') {
                    sections.extend(parse_ansi_lines(line));
                } else {
                    sections.push(Line::styled(
                        format_terminal_math(line),
                        theme::text_primary(),
                    ));
                }
            }
        }
    }

    if !row.logs.is_empty() {
        sections.push(Line::styled("[Logs]".to_string(), theme::chip_done()));
        for log in &row.logs {
            if log.contains('\x1b') {
                for parsed in parse_ansi_lines(log) {
                    let mut spans = vec![Span::raw("- ")];
                    spans.extend(parsed.spans);
                    sections.push(Line::from(spans));
                }
            } else {
                sections.push(Line::styled(format!("- {log}"), theme::text_dim()));
            }
        }
    }

    if sections.is_empty() {
        let role = role_label(row, entry);
        sections.push(Line::styled(
            format!("Nothing streamed yet for {role}."),
            theme::text_dim(),
        ));
    }

    lines.extend(verdict_marks(sections));
    lines
}

/// The chars→tokens estimate behind the `[Thinking]` budget readout: the same
/// `chars.div_ceil(4)` the legacy subagents pane used (`render.rs:917`), so the
/// number the operator sees did not move when the pane changed owner.
fn thinking_tokens(thinking: &str) -> usize {
    thinking.chars().count().div_ceil(4)
}

/// The bottom tally of the workers pane: the four state counts, the roster
/// position and where the detail half stands.
///
/// The tally rides the bottom border row, so it is fitted against [`title_width`]
/// and not against `rect.width`: a border row is 2 cells narrower than its rect
/// (one cell per corner), and a tally measured against the rect is therefore 2
/// cells too long — ratatui cuts those 2 cells with no mark at all, and the `…`
/// that `fit_spans` appended to say "this row was cut" was one of them. That is
/// how `✓ done N`, the `1/3` roster position and the detail chip used to vanish
/// from the row without anything on screen saying so.
///
/// When the full spelling (glyph + word label, §7.2) does not fit, the counts fall
/// back to glyph + number: §7.2 keeps a word label only "whenever the rect has
/// room", and the counts, the position and the detail chip are the part the
/// operator reads, so those are what survives the narrow pane.
fn chip_row(data: &ActivityData<'_>, detail: DetailReport, width: u16) -> Vec<Span<'static>> {
    let counts = state_counts(data.rows);
    let counts_spans = |labelled: bool| {
        let mut spans = vec![Span::styled(" ".to_string(), theme::text_primary())];
        // The four §7.2 states are always spelled; a fifth `✗ failed N` segment joins
        // only when a row really is in that state, so an aborted worker is never folded
        // into the `✓ done` count (D1 put the label on the row that says so).
        for (glyph, n) in [
            (Glyph::Active, counts.running),
            (Glyph::Paused, counts.paused),
            (Glyph::Pending, counts.queued),
            (Glyph::Done, counts.done),
        ] {
            if labelled {
                spans.push(glyph.tagged());
                spans.push(Span::styled(format!(" {n} "), glyph.style()));
            } else {
                spans.push(Span::styled(
                    format!("{} {n} ", glyph.symbol()),
                    glyph.style(),
                ));
            }
        }
        if counts.failed > 0 {
            let glyph = Glyph::Failed;
            if labelled {
                spans.push(glyph.tagged());
                spans.push(Span::styled(format!(" {} ", counts.failed), glyph.style()));
            } else {
                spans.push(Span::styled(
                    format!("{} {} ", glyph.symbol(), counts.failed),
                    glyph.style(),
                ));
            }
        }
        spans
    };
    let (position, total) = if data.rows.is_empty() {
        (0, 0)
    } else {
        (data.selected.min(data.rows.len() - 1) + 1, data.rows.len())
    };
    // The detail chip states which of the §2.5 rules took the detail half away, so
    // a missing half is never a silent one (G5).
    let detail_chip = if detail.present {
        Span::styled(
            format!(" detail {}/{} ", detail.scroll, detail.max),
            theme::text_dim(),
        )
    } else if data.detail_present {
        Span::styled(" detail too short ".to_string(), theme::text_dim())
    } else {
        Span::styled(" detail hidden ".to_string(), theme::text_dim())
    };
    let tail = vec![
        Span::styled(format!(" {position}/{total} "), theme::text_dim()),
        detail_chip,
    ];

    let labelled = counts_spans(true);
    if spans_width(&labelled) + spans_width(&tail) <= usize::from(width) {
        let mut chips = labelled;
        chips.extend(tail);
        return chips;
    }
    let mut chips = counts_spans(false);
    chips.extend(tail);
    chips
}

/// The registry's own status label as projected onto the row (plumbing delta D1,
/// §9.3). `None` means the projection has not folded one onto this row yet, which
/// is a real state: the glyph then falls back to plain liveness.
fn row_status_label(row: &SubagentDetail) -> Option<&str> {
    match row.status_label.as_deref() {
        Some(label) if !label.is_empty() => Some(label),
        _ => None,
    }
}

/// The state tally of the roster, keyed by the §7.2 glyph each row actually drew.
///
/// `done` counts the rows that finished with no verdict against them; `failed` counts
/// the rows the registry labelled `Aborted` / `Rejected` / `Failed` / `Replan`, which
/// used to be folded into the done count because liveness alone could not tell the two
/// apart (plumbing delta D1, §9.3).
#[derive(Debug, Default, Clone, Copy)]
struct StateCounts {
    running: usize,
    paused: usize,
    queued: usize,
    done: usize,
    failed: usize,
}

/// The state counts of the roster, derived from the same glyph mapping the
/// rows are drawn with (spec §8.3) so the tally can never disagree with the rows.
fn state_counts(rows: &[SubagentDetail]) -> StateCounts {
    let mut counts = StateCounts::default();
    for row in rows {
        match theme::worker_state_glyph(
            row.is_active,
            row.paused,
            row.queue.is_some(),
            row_status_label(row),
        ) {
            Glyph::Active => counts.running += 1,
            Glyph::Paused => counts.paused += 1,
            Glyph::Pending => counts.queued += 1,
            Glyph::Done => counts.done += 1,
            Glyph::Failed => counts.failed += 1,
        }
    }
    counts
}

/// The specialist role: the registry's own `agent_name` when the row resolves,
/// otherwise the structural head of the registry key (`{agent}-{task}`).
fn role_label(row: &SubagentDetail, entry: Option<&WorkerEntry>) -> String {
    if let Some(entry) = entry {
        let name = entry.agent_name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    let key = row.name.split('#').next().unwrap_or(&row.name);
    match key.find("-t-") {
        Some(position) if position > 0 => key[..position].to_string(),
        _ => key.to_string(),
    }
}

/// The task id, from the row first (registry projection) and the registry entry
/// as the fallback.
fn task_id(row: &SubagentDetail, entry: Option<&WorkerEntry>) -> Option<String> {
    row.task_id
        .as_deref()
        .or_else(|| entry.and_then(|entry| entry.task_id.as_deref()))
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .map(str::to_string)
}

/// Seconds this worker has been at it: the live clock while the row is open, the
/// accumulated duration once it closed, and the registry's own number as the
/// fallback for a restored row.
fn elapsed_secs(row: &SubagentDetail, entry: Option<&WorkerEntry>) -> u64 {
    if row.is_active {
        let since_start = row
            .started_at
            .map_or(0, |started| started.elapsed().as_secs());
        row.worked_duration.as_secs().saturating_add(since_start)
    } else {
        let worked = row.worked_duration.as_secs();
        if worked > 0 {
            worked
        } else {
            entry.map_or(0, |entry| entry.elapsed_secs)
        }
    }
}

/// Token count when there is one: the row's ctx mirror first, then whichever of
/// the registry's two counters carries a number.
fn token_count(row: &SubagentDetail, entry: Option<&WorkerEntry>) -> usize {
    if row.context_tokens > 0 {
        return row.context_tokens;
    }
    entry.map_or(0, |entry| entry.context_tokens.max(entry.last_tokens))
}

/// `12345` reads `12.3k`, as the roster has always spelled a token count.
fn format_count(count: usize) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{count}")
    }
}

/// `75s` reads `1m 15s`.
fn format_elapsed(secs: u64) -> String {
    if secs >= 60 {
        format!(" {}m {}s", secs / 60, secs % 60)
    } else {
        format!(" {secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use unicode_width::UnicodeWidthStr;

    /// A roster row with the minimum the pane needs to name it.
    fn row(name: &str, task: &str, is_active: bool) -> SubagentDetail {
        SubagentDetail {
            name: name.to_string(),
            task_id: Some(task.to_string()),
            is_active,
            ..SubagentDetail::default()
        }
    }

    fn rows(count: usize) -> Vec<SubagentDetail> {
        (1..=count)
            .map(|i| row(&format!("roster-test-t-{i:03}"), &format!("t-{i:03}"), true))
            .collect()
    }

    fn rows_text(width: u16, height: u16, data: &ActivityData<'_>) -> Vec<String> {
        let backend =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut terminal = backend;
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, width, height), data);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn roster_shows_every_worker_beyond_the_three_row_cap() {
        let workers = rows(12);
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 30, &data).join("\n");
        for index in 1..=12 {
            let task = format!("t-{index:03}");
            assert!(text.contains(&task), "worker {index} missing from: {text}");
        }
        assert!(
            text.contains("1/12"),
            "roster position chip expected: {text}"
        );
    }

    #[test]
    fn every_row_stays_reachable_when_the_roster_is_clipped() {
        let workers = rows(12);
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 11,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let seen = rows_text(70, 12, &data);
        let text = seen.join("\n");
        // The selected row is kept on screen, and the scrollbar says more exist.
        assert!(
            text.contains("t-012"),
            "selected row must be visible: {text}"
        );
        assert!(!text.contains("t-001"), "older rows scroll out: {text}");
        assert!(
            seen.iter().any(|row| row.ends_with('\u{2502}')),
            "roster scrollbar expected: {seen:?}"
        );
    }

    #[test]
    fn paused_worker_carries_the_pause_glyph() {
        let mut workers = rows(3);
        workers[1].paused = true;
        let data = ActivityData {
            focus: false,
            rows: &workers,
            selected: 1,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 14, &data).join("\n");
        assert!(text.contains('\u{23f8}'), "paused glyph expected: {text}");
        assert!(text.contains("paused"), "word label expected: {text}");
        assert!(text.contains('\u{25b6}'), "running glyph expected: {text}");
    }

    #[test]
    fn queued_worker_carries_the_queue_glyph_and_rank() {
        let mut workers = rows(1);
        workers[0].queue = Some(crate::ui::QueueState { position: 2, of: 3 });
        let data = ActivityData {
            focus: false,
            rows: &workers,
            selected: 0,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 10, &data).join("\n");
        assert!(text.contains('\u{29d7}'), "queue glyph expected: {text}");
        assert!(text.contains("2/3"), "queue rank expected: {text}");
    }

    #[test]
    fn empty_roster_shows_the_spec_empty_state() {
        let workers: Vec<SubagentDetail> = Vec::new();
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(60, 12, &data).join("\n");
        assert!(text.contains("No specialist launched yet."), "got: {text}");
        assert!(text.contains("0/0"), "chip still echoes: {text}");
    }

    #[test]
    fn row_metrics_show_tokens_and_elapsed() {
        let mut workers = rows(1);
        workers[0].context_tokens = 12_400;
        workers[0].worked_duration = Duration::from_secs(125);
        let data = ActivityData {
            focus: false,
            rows: &workers,
            selected: 0,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 8, &data).join("\n");
        assert!(text.contains("12.4k"), "token count expected: {text}");
        assert!(text.contains("2m 5s"), "elapsed expected: {text}");
    }

    #[test]
    fn no_round_counter_is_invented() {
        let workers = rows(2);
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 20, &data).join("\n");
        assert!(
            !text.contains("val "),
            "no validation round may be printed: {text}"
        );
        assert!(!text.contains("round"), "no round counter: {text}");
    }

    #[test]
    fn exactly_one_row_is_marked_selected() {
        let workers = rows(5);
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 2,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let marked = rows_text(70, 12, &data)
            .iter()
            .filter(|line| line.contains("> "))
            .count();
        assert_eq!(marked, 1, "one selection marker expected");
    }

    #[test]
    fn detail_half_renders_output_logs_and_verdicts() {
        let mut workers = rows(1);
        workers[0].content = "MISSION COMPLETE\nall tests pass".to_string();
        workers[0].logs = vec![
            "started task t-001".to_string(),
            "\x1b[32mgreen log line\x1b[0m".to_string(),
        ];
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 20, &data).join("\n");
        assert!(text.contains('\u{2713}'), "verdict glyph expected: {text}");
        assert!(text.contains("all tests pass"), "output expected: {text}");
        assert!(text.contains("[Logs]"), "logs header expected: {text}");
        assert!(text.contains("started task t-001"), "log expected: {text}");
        assert!(text.contains("green log line"), "ansi log expected: {text}");
        assert!(
            !text.contains('\x1b'),
            "escape bytes never reach the screen"
        );
    }

    #[test]
    fn thinking_text_never_leaks_without_the_toggle() {
        let mut workers = rows(1);
        workers[0].thinking = "private chain of thought".to_string();
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 20, &data).join("\n");
        assert!(!text.contains("private chain of thought"), "got: {text}");
        assert!(
            text.contains("[Thinking]"),
            "presence still reported: {text}"
        );
    }

    #[test]
    fn detail_half_is_absent_in_a_narrow_column() {
        let workers = rows(2);
        let data = ActivityData {
            focus: false,
            rows: &workers,
            selected: 0,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(40, 10, &data).join("\n");
        assert!(text.contains("t-001"), "roster still drawn: {text}");
        // The chip row is clipped to the pane's *border* row (rect width minus the
        // two corners), so the marker is asserted at its source rather than in a
        // 40-column buffer.
        let chips = chip_row(
            &data,
            DetailReport {
                present: false,
                scroll: 0,
                max: 0,
                width: 0,
                height: 0,
            },
            title_width(Rect::new(0, 0, 40, 10)),
        );
        assert_eq!(
            chips.last().map(|chip| chip.content.as_ref()),
            Some(" detail hidden ")
        );
    }

    #[test]
    fn a_pane_too_short_for_both_halves_says_so() {
        let workers = rows(2);
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        // inner height 6 cannot hold `Min(4)` + `Min(3)`, so the roster takes the
        // whole pane and the chip row reports why the detail half is gone.
        let text = rows_text(70, 8, &data).join("\n");
        assert!(text.contains("detail too short"), "got: {text}");
        assert!(text.contains("t-002"), "roster keeps every row: {text}");
    }

    #[test]
    fn selection_index_out_of_range_clamps_instead_of_panicking() {
        let workers = rows(3);
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 99,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 16, &data).join("\n");
        assert!(text.contains("3/3"), "chip clamped: {text}");
        assert!(text.contains("t-003"), "last row selected: {text}");
    }

    #[test]
    fn degenerate_rects_are_ignored_without_panicking() {
        let workers = rows(2);
        let data = ActivityData {
            focus: false,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 7,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let backend = ratatui::Terminal::new(ratatui::backend::TestBackend::new(6, 6)).unwrap();
        let mut terminal = backend;
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, 0, 0), &data);
                render(frame, Rect::new(0, 0, 4, 4), &data);
            })
            .unwrap();
    }

    #[test]
    fn chip_row_never_spills_the_pane_width() {
        let workers = rows(12);
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let seen = rows_text(40, 20, &data);
        for (y, line) in seen.iter().enumerate() {
            assert!(
                line.chars().count() <= 40,
                "row {y} spills its rect: {line:?}"
            );
        }
    }

    #[test]
    fn roster_rows_and_chips_measured_in_cells_stay_inside_the_rect() {
        // §7.4: widths are measured with `unicode-width`, because the roster
        // prints `▶`/`⧗`/`⏸`/`✓` glyphs that must not be assumed to be one cell.
        let workers = rows(12);
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 3,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        for width in [16u16, 20, 30, 40, 60, 90, 120] {
            for (index, worker) in workers.iter().enumerate() {
                let line = roster_row(worker, None, index == 3, width);
                let cells: usize = line
                    .spans
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum();
                assert!(
                    cells <= usize::from(width),
                    "roster row {index} spills {cells} cells into {width}: {line:?}"
                );
            }
            let chips = fit_spans(
                chip_row(
                    &data,
                    DetailReport {
                        present: true,
                        scroll: 0,
                        max: 0,
                        width: 0,
                        height: 0,
                    },
                    width,
                ),
                width,
            );
            let cells: usize = chips
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum();
            assert!(
                cells <= usize::from(width),
                "chip row spills {cells} cells into {width}: {chips:?}"
            );
            let text: String = chips.iter().map(|span| span.content.as_ref()).collect();
            // A tally too long for the border row is *marked*, never silently cut.
            assert!(
                !text.contains(theme::ELLIPSIS) || text.ends_with(theme::ELLIPSIS),
                "clipped tally loses its marker at width {width}: {text:?}"
            );
        }
    }

    #[test]
    fn every_worker_in_a_clipped_roster_is_reachable_by_selection() {
        // G8 / §10.15: with 12 workers and 10 roster rows, each worker must be
        // on screen while it is the selected one — the scroll offset follows the
        // selection, so no row is silently unreachable. The detail half is off so
        // the name can only come from the roster itself.
        let workers = rows(12);
        for selected in 0..12 {
            let data = ActivityData {
                focus: true,
                rows: &workers,
                selected,
                detail_present: false,
                scroll: 0,
                list_scroll: 0,
                show_thought: false,
                thinking_budget: None,
            };
            let seen = rows_text(70, 12, &data);
            let text = seen.join("\n");
            let task = format!("t-{:03}", selected + 1);
            assert!(
                text.contains(&task),
                "worker {selected} unreachable: {text}"
            );
            let markers = seen
                .iter()
                .filter(|line| line.contains(SELECTION_MARKER.trim()))
                .count();
            assert_eq!(markers, 1, "one selection marker expected: {seen:?}");
        }
        // And the roster really is clipped at this height: the oldest row is off
        // screen while the newest one is selected.
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 11,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 12, &data).join("\n");
        assert!(!text.contains("t-001"), "clipped head leaked: {text}");
    }

    #[test]
    fn the_selection_marker_never_borrows_the_focus_signal() {
        // §7.3: focus is the border colour plus the `●` title badge. The `>`
        // roster marker keeps its own slot in both states, so a selected row in
        // an unfocused pane cannot be read as focus (G6).
        let workers = rows(3);
        for focus in [true, false] {
            let data = ActivityData {
                focus,
                rows: &workers,
                selected: 1,
                detail_present: false,
                scroll: 0,
                list_scroll: 0,
                show_thought: false,
                thinking_budget: None,
            };
            let backend =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 10)).unwrap();
            let mut terminal = backend;
            terminal
                .draw(|frame| {
                    render(frame, Rect::new(0, 0, 70, 10), &data);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut markers = 0usize;
            for y in 0..10u16 {
                for x in 0..70u16 {
                    if buffer[(x, y)].symbol() == ">" {
                        markers += 1;
                        assert_eq!(
                            buffer[(x, y)].style().fg,
                            theme::chip_active().fg,
                            "selection marker style drifted at ({x},{y})"
                        );
                    }
                }
            }
            assert_eq!(markers, 1, "exactly one `>` marker expected");
            let title: String = (0..70u16).map(|x| buffer[(x, 0)].symbol()).collect();
            assert_eq!(
                title.contains('\u{25cf}'),
                focus,
                "the focus badge tracks focus only: {title}"
            );
            assert_eq!(
                buffer[(0, 1)].style().fg,
                if focus {
                    theme::frame_focus().fg
                } else {
                    theme::frame_idle().fg
                },
                "border colour tracks focus only"
            );
        }
    }

    #[test]
    fn the_detail_scrollbar_does_not_cover_detail_text() {
        // The detail half is the long body here; its scrollbar owns the last
        // column, so no log character may be painted under the thumb.
        let mut workers = rows(1);
        workers[0].logs = (0..40)
            .map(|i| format!("log line {i:02} {}", "y".repeat(40)))
            .collect();
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let backend = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 24)).unwrap();
        let mut terminal = backend;
        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, 60, 24), &data);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let column: Vec<String> = (0..24u16)
            .map(|y| buffer[(58, y)].symbol().to_string())
            .collect();
        assert!(
            !column.iter().any(|cell| cell == "y"),
            "detail text under the scrollbar: {column:?}"
        );
        assert!(
            column
                .iter()
                .any(|cell| cell == "\u{2588}" || cell == "\u{25bc}"),
            "detail scrollbar thumb expected: {column:?}"
        );
    }

    #[test]
    fn the_roster_offset_is_the_renderers_and_comes_back_for_write_back() {
        // t-009b: `subagent_list_scroll` now reaches the pane, so `up/down` can
        // scroll the list past the selection, and the offset actually drawn is
        // handed back so the event clamps match the picture.
        let workers = rows(12);
        let scrolled = ActivityData {
            focus: true,
            rows: &workers,
            selected: 5,
            detail_present: true,
            scroll: 0,
            list_scroll: 5,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(70, 12, &scrolled).join("\n");
        assert!(text.contains("t-006"), "the list moved: {text}");
        assert!(!text.contains("t-001"), "and the head scrolled out: {text}");
        assert_eq!(geometry(70, 12, &scrolled).roster_scroll, 5);

        // An offset past the end is lowered, never wrapped.
        let past = ActivityData {
            selected: 11,
            list_scroll: 999,
            ..scrolled
        };
        let seen = geometry(70, 12, &past);
        assert_eq!(
            seen.roster_scroll, 7,
            "clamped to `rows - height`: {seen:?}"
        );
        let text = rows_text(70, 12, &past).join("\n");
        assert!(text.contains("t-012"), "the tail is on screen: {text}");

        // The minimal reveal survives: an offset that would hide the selection is
        // pulled back only as far as it must be.
        let hidden = ActivityData {
            selected: 11,
            list_scroll: 0,
            ..scrolled
        };
        let seen = geometry(70, 12, &hidden);
        assert_eq!(
            seen.roster_scroll, 7,
            "selection pulls just enough: {seen:?}"
        );
        assert!(rows_text(70, 12, &hidden).join("\n").contains("t-012"));
    }

    #[test]
    fn the_detail_offset_comes_back_clamped_to_the_body() {
        let mut workers = rows(1);
        workers[0].content = "line\n".repeat(60);
        let data = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 999,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let seen = geometry(70, 24, &data);
        assert!(seen.detail_max > 0, "the body must overflow: {seen:?}");
        assert_eq!(seen.detail_scroll, seen.detail_max as u16, "{seen:?}");
    }

    #[test]
    fn the_thinking_block_is_printed_when_the_plumbed_toggle_is_on() {
        // §8.2 with `show_thought` plumbed: the reasoning stream is shown, with the
        // budget the worker was launched with on the header line.
        let mut workers = rows(1);
        workers[0].thinking = "first I read the plan\nthen I edit".to_string();
        let shown = ActivityData {
            focus: true,
            rows: &workers,
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: true,
            thinking_budget: Some(4000),
        };
        let text = rows_text(70, 24, &shown).join("\n");
        assert!(text.contains("[Thinking]"), "{text}");
        assert!(text.contains("first I read the plan"), "{text}");
        assert!(text.contains("then I edit"), "{text}");
        assert!(text.contains("~9 tokens of 4.0k budget"), "{text}");
        assert!(!text.contains("hidden (Ctrl+T)"), "{text}");

        // No budget for this row: still shown, without a fabricated budget figure.
        let unbudged = ActivityData {
            thinking_budget: None,
            ..shown
        };
        let text = rows_text(70, 24, &unbudged).join("\n");
        assert!(text.contains("first I read the plan"), "{text}");
        assert!(!text.contains("budget ("), "{text}");
    }

    /// Draw the pane and hand back the geometry it reported.
    fn geometry(width: u16, height: u16, data: &ActivityData<'_>) -> WorkerGeometry {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut seen = WorkerGeometry::default();
        terminal
            .draw(|frame| seen = render(frame, Rect::new(0, 0, width, height), data))
            .unwrap();
        seen
    }

    /// The half D1 unblocked (§9.3, DATA §4.1 gap 1): the registry's own `status_label`
    /// is on the row, so a worker that ended aborted or validator-rejected reads
    /// `✗ failed`, and only an approved/completed one reads `✓ done`. Liveness alone
    /// cannot tell those apart, which is exactly the lie this pins down.
    #[test]
    fn an_aborted_or_rejected_worker_does_not_read_done() {
        for label in [
            "Aborted",
            "Rejected",
            "Failed",
            "Replan Required (too complex)",
        ] {
            let mut closed = row("coder-t-900", "t-900", false);
            closed.status_label = Some(label.to_string());
            let data = ActivityData {
                focus: true,
                rows: &[closed],
                selected: 0,
                detail_present: false,
                scroll: 0,
                list_scroll: 0,
                show_thought: false,
                thinking_budget: None,
            };
            // 90 columns, not 70: the tally spells its state counts with word labels
            // only while the border row has room (§7.2), and the extra `✗ failed N`
            // segment is exactly what tips a 70-column border row into the compact
            // `✗ N` spelling. The row itself is labelled at either width.
            let lines = rows_text(90, 12, &data);
            let roster = roster_line(&lines, "t-900");
            assert!(
                roster.contains("✗ failed"),
                "{label}: a closed terminal row must read failed, got: {roster}"
            );
            assert!(
                !roster.contains("✓ done"),
                "{label}: a row that did not deliver must not badge done, got: {roster}"
            );
            // The tally agrees with the row: the failed count carries the row and the
            // done count stays empty.
            let tally = lines.join("\n");
            assert!(
                tally.contains("✗ failed 1"),
                "the tally must count the row as failed, got:\n{tally}"
            );
            assert!(
                tally.contains("✓ done 0"),
                "and not fold it into the done count, got:\n{tally}"
            );
        }

        // The approved case keeps its `✓ done`, so the change is not "everything is
        // red": it is the label deciding between the two.
        let mut approved = row("coder-t-901", "t-901", false);
        approved.status_label = Some("Approved".to_string());
        let data = ActivityData {
            focus: true,
            rows: &[approved],
            selected: 0,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let lines = rows_text(70, 12, &data);
        let roster = roster_line(&lines, "t-901");
        assert!(roster.contains("✓ done"), "{roster}");
        assert!(!roster.contains('✗'), "{roster}");
        assert!(
            lines.join("\n").contains("✓ done 1"),
            "{}",
            lines.join("\n")
        );
    }

    /// The roster line that names `task`, so an assertion can talk about one row
    /// without tripping over the state tally on the border row.
    fn roster_line(lines: &[String], task: &str) -> String {
        lines
            .iter()
            .find(|line| line.contains(task))
            .cloned()
            .unwrap_or_else(|| panic!("no roster line naming {task}: {}", lines.join("\n")))
            .trim()
            .to_string()
    }

    /// A `Revising` worker is mid-review, not idle and not running: §7.2 spells the
    /// validating state `⧗`, the same glyph the queue state uses.
    #[test]
    fn a_revising_worker_reads_the_validating_glyph() {
        let mut revising = row("coder-t-902", "t-902", true);
        revising.status_label = Some("Revising".to_string());
        let data = ActivityData {
            focus: true,
            rows: &[revising],
            selected: 0,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let lines = rows_text(70, 12, &data);
        let roster = roster_line(&lines, "t-902");
        assert!(roster.contains("⧗ queued"), "{roster}");
        assert!(!roster.contains("✓ done"), "{roster}");
        assert!(!roster.contains("▶ running"), "{roster}");
    }

    /// `val N` on the roster row: the registry's `validation_rounds` is projected onto
    /// the row (D1), so the pane prints it — and prints nothing at all when no round
    /// ran. An arbitration round is never a candidate (§8.4).
    #[test]
    fn the_roster_prints_the_validation_round_carried_on_the_row() {
        let mut reviewed = row("coder-t-903", "t-903", true);
        reviewed.validation_rounds = 2;
        reviewed.implementation_turns = 3;
        let data = ActivityData {
            focus: true,
            rows: &[reviewed],
            selected: 0,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(90, 12, &data).join("\n");
        assert!(text.contains("val 2"), "{text}");
        assert!(text.contains("turn 3"), "{text}");

        let plain = row("coder-t-904", "t-904", true);
        let data = ActivityData {
            focus: true,
            rows: &[plain],
            selected: 0,
            detail_present: false,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(90, 12, &data).join("\n");
        assert!(!text.contains("val"), "{text}");
        assert!(!text.contains("round"), "{text}");
    }

    /// A rejected deliverable says why: the registry's `latest_validator_feedback` is
    /// projected onto the row and the detail half prints it verbatim.
    #[test]
    fn the_detail_prints_the_registrys_validator_feedback() {
        let mut rejected = row("coder-t-905", "t-905", false);
        rejected.status_label = Some("Rejected".to_string());
        rejected.latest_validator_feedback =
            Some("definition not used in a test\nclippy: unused import".to_string());
        let data = ActivityData {
            focus: true,
            rows: &[rejected],
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(80, 20, &data).join("\n");
        assert!(text.contains("[Validator]"), "{text}");
        assert!(text.contains("definition not used in a test"), "{text}");
        assert!(text.contains("clippy: unused import"), "{text}");

        // No feedback recorded: the section is absent, not empty.
        let clean = row("coder-t-906", "t-906", false);
        let data = ActivityData {
            focus: true,
            rows: &[clean],
            selected: 0,
            detail_present: true,
            scroll: 0,
            list_scroll: 0,
            show_thought: false,
            thinking_budget: None,
        };
        let text = rows_text(80, 20, &data).join("\n");
        assert!(!text.contains("[Validator]"), "{text}");
    }
}
