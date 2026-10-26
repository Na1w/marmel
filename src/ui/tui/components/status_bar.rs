//! Status bar — spec §3 (Status bar pane), §7.3 (armed prompt lives on row 2,
//! row 1 keeps counting), §7.4 (drop ladders), §8.3 (counts derived from the
//! projected worker rows) and §8.5 (abort arming).
//!
//! Row 1 is the metrics line and is drawn by this component on **every** frame
//! (t-009b retired the legacy metrics renderer from the live path). The legacy
//! defect this component fixes is `render.rs:1117-1125`: while an abort was armed
//! the banner replaced the whole metrics line, so the operator lost every live
//! counter at the exact moment they were deciding whether to abort (UX friction
//! 11, G4). Here the armed prompt moves to row 2 and row 1 keeps counting.
//!
//! §2.2 and §3 name the row-1 fields: tokens in/out, `ctx %`, elapsed and the
//! active/paused/queued counts. All of them now arrive through [`StatusData`] —
//! the renderer hands over its own session token pair, the context budget
//! (`config.max_context_tokens`), its session clock and the transcript's unseen-line
//! count — so nothing here reads a process-global counter or a plan-file clock any
//! more, and an invented number is impossible by construction.
//!
//! The one §3 field still absent is the legacy free-text `status_line` prose (with
//! its 125 ms spinner tier): it is not part of the §2.2 field set and no component
//! input carries it, so the row prints the counters instead of guessing at prose.
//! `val N` is never printed: plumbing delta D1 (§9.3) has not landed. An
//! arbitration round counter is never printed either — it is not data at all
//! (spec §8.4), so both absences are rules, not truncation.
//!
//! Two rules this component owns:
//!
//! * **The row-1 ladder clips, it does not delete.** The token pair and the
//!   active/paused/queued counts are mandatory (§2.5), the active-name list is the
//!   row's flexible field and is cut into whatever the counters leave behind with an
//!   ellipsis marking the cut (§7.4 "active names (counts kept)"), and a ranked
//!   field only leaves once the mandatory part on its own no longer fits.
//! * **Every chip states what is on screen this frame.** The pane chips are driven
//!   by `layout::PaneRects` rather than by the mode or the toggle, and the
//!   scroll-state chip is drawn exactly once — by the transcript pane that owns the
//!   viewport — instead of being echoed here a second time.

use super::{fit_spans, spans_width};
use crate::ui::SubagentDetail;
use crate::ui::tui::theme::{self, Glyph};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

/// What the status bar may draw this frame.
#[derive(Debug, Clone, Copy)]
pub struct StatusData<'a> {
    /// Every live worker row; the counts below are derived from them, never from
    /// a second state owner (spec §8.3, DATA §2 step 8).
    pub workers: &'a [SubagentDetail],
    /// Thinking display (`Ctrl+T`, `/thought`) — P1.3.
    pub show_thought: bool,
    /// Plan pane toggle (`Ctrl+P`, `/plan`).
    pub plan_visible: bool,
    /// Workers pane toggle (`Ctrl+A`, `/workers`).
    pub workers_visible: bool,
    /// Whether the plan pane actually got a rect this frame — read off
    /// [`super::super::layout::PaneRects`], not off the toggle. The presence chips
    /// are driven by what is drawn: the §2.5 ladder can take a pane away while its
    /// toggle stays on, and a chip that still reads `plan:on` for a pane nobody can
    /// see is a chip that lies (G5).
    pub plan_present: bool,
    /// Whether the workers pane actually got a rect this frame.
    pub workers_present: bool,
    /// Abort armed inside the 3 s window (§8.5).
    pub abort_armed: bool,
    /// Session tokens in — the renderer merges its own `tokens_in` with the
    /// process counter before handing this over (§3 Status bar row).
    pub tokens_in: usize,
    /// Session tokens out.
    pub tokens_out: usize,
    /// `orchestrator_context_tokens`: the numerator of `ctx %` (§2.2 line 82).
    pub ctx_tokens: usize,
    /// `config.max_context_tokens`: the denominator. `0` means unknown, and the
    /// `ctx` field is then withheld rather than computed from a guessed budget.
    pub ctx_limit: usize,
    /// The renderer's session clock in whole seconds (`session_start`).
    pub elapsed: Option<u64>,
    // There is deliberately no `follow` / `unseen` pair here any more: the
    // scroll-state chip (`● LIVE` / `↓ N new`) belongs to the transcript pane that
    // owns the viewport, and drawing it here as well put the same chip on screen
    // twice per frame, in two places that could drift apart. One chip, one owner.
}

// Ladder ranks. A field carries `Some(rank)` and the row drops the lowest rank
// still present until it fits, so a narrow bar loses one field at a time instead
// of being cut mid-counter (§7.4). Ranks are only compared within their own row.
//
// Row 1 — §7.4 spells the counter part as `queued` → `paused` → active names
// (counts kept) → `val` → `ctx`; the clock goes first because §2.5 (`width < 70`)
// makes row 1 keep tokens plus active/paused/queued at every width the ladder
// reaches. `val` has no rank: it is gated on D1 (§9.3), so it is never on the line
// to be dropped. The active-name list has no rank either: it is the flexible
// field (§7.4 "counts kept") and is clipped into the space left for it rather than
// deleted, so a wide bar never throws the roster away to save a cell.
const DROP_CLOCK: u8 = 0;
const DROP_QUEUED: u8 = 1;
const DROP_PAUSED: u8 = 2;
const DROP_CTX: u8 = 3;

// Row 2 — the chips echo toggles (G5), but the armed prompt outranks all of them:
// it is the time-boxed signal, so when row 2 cannot hold both, the least important
// chip goes first and the prompt stays whole.
const DROP_THOUGHT_CHIP: u8 = 0;
const DROP_WORKERS_CHIP: u8 = 1;
const DROP_PLAN_CHIP: u8 = 2;
const DROP_COUNTS_CHIP: u8 = 3;

/// One field of a single-row bar and its place in that row's ladder
/// (`None` = never dropped).
#[derive(Debug, Clone)]
struct Field {
    /// Ladder rank within its own row; `None` marks a mandatory field that stays
    /// whatever the width does.
    drop: Option<u8>,
    /// The row's flexible field: it degrades by clipping into the cells the other
    /// fields leave it (marked with `…`), not by vanishing.
    flex: bool,
    spans: Vec<Span<'static>>,
}

/// The two rows of a status rect: metrics on top, chips at the bottom (§7.3).
struct Rows {
    metrics: Option<Rect>,
    chips: Option<Rect>,
}

/// Draw the status bar.
///
/// Row 1 (metrics) is drawn by this component on every frame — it is the pane's
/// owner, so there is no second writer to defer to. At `height >= 2` row 2 is the
/// chip row (§7.3, G5). At `height == 1` the §2.5 ladder has collapsed the bar to
/// a metrics-only row, so the armed prompt joins that row — ahead of the counters,
/// so the time-boxed signal is never the part that gets cut — instead of being
/// dropped along with the chip row.
pub fn render(frame: &mut Frame, rect: Rect, data: &StatusData<'_>) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    if rect.height == 1 {
        // Metrics stay; the armed prompt shares the row when it is armed (§2.5).
        paint_row(frame, rect, metrics_line(data, rect.width, data.abort_armed));
        return;
    }

    let rows = status_rows(rect);
    if let Some(metrics_row) = rows.metrics {
        paint_row(frame, metrics_row, metrics_line(data, metrics_row.width, false));
    }
    if let Some(chip_row) = rows.chips {
        paint_row(frame, chip_row, chip_line(data, chip_row.width));
    }
}

/// Paint one row of the bar: the spans are clipped to the row, then padded with
/// spaces so every cell of the row is rewritten.
///
/// The legacy bar writes this row as a full-width paragraph, so an armed banner
/// longer than the counters would otherwise leave its tail standing behind the
/// shorter metrics line — the same defect §7.3 removes. The bar keeps its
/// DarkGray band (`theme::chip()`, §7.1) on whatever row this component repaints,
/// armed or not, and the armed signal is carried by the prompt's own `error()`
/// style plus the `✗` glyph rather than by a red row that hides the counters.
fn paint_row(frame: &mut Frame, row: Rect, spans: Vec<Span<'static>>) {
    let spans = pad_to_width(spans, row.width);
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme::chip()), row);
}

/// Pad a fitted span run with spaces up to `width` cells (§7.4).
fn pad_to_width(mut spans: Vec<Span<'static>>, width: u16) -> Vec<Span<'static>> {
    let free = usize::from(width).saturating_sub(spans_width(&spans));
    if free > 0 {
        spans.push(Span::styled(" ".repeat(free), theme::chip()));
    }
    spans
}

/// Split the status rect into its metrics and chip rows. A rect the §2.5 ladder
/// collapsed to one row has no separate metrics row — the chip row *is* the
/// metrics row there, which is why the armed prompt joins it rather than being
/// dropped.
fn status_rows(rect: Rect) -> Rows {
    if rect.height < 2 {
        return Rows {
            metrics: None,
            chips: Some(Rect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: 1,
            }),
        };
    }
    Rows {
        metrics: Some(Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height - 1,
        }),
        chips: Some(Rect {
            x: rect.x,
            y: rect.y + rect.height - 1,
            width: rect.width,
            height: 1,
        }),
    }
}

/// The row-1 metrics line, ladder-fitted to `width`, reading only what the
/// renderer handed over this frame.
///
/// `prompt_first` is the §2.5 one-row case, where the armed prompt shares the row
/// with the counters and therefore leads it: the counters degrade into whatever
/// width is left rather than the actionable signal being clipped.
fn metrics_line(data: &StatusData<'_>, width: u16, prompt_first: bool) -> Vec<Span<'static>> {
    build_metrics_line(
        data,
        (data.tokens_in, data.tokens_out),
        data.elapsed,
        width,
        prompt_first,
    )
}

/// The same line with every value supplied by the caller, so the ladder is a pure
/// function of `(tokens, elapsed, width)` — which is what makes the drop order
/// testable without touching process-global counters.
fn build_metrics_line(
    data: &StatusData<'_>,
    tokens: (usize, usize),
    elapsed: Option<u64>,
    width: u16,
    prompt_first: bool,
) -> Vec<Span<'static>> {
    let prompt = if prompt_first { armed_prompt() } else { Vec::new() };
    let mut fields = metrics(data, tokens, elapsed);
    // The prompt reserves its cells before the ladder runs, so the counters are
    // what degrade when the row cannot hold both.
    fit_fields(&mut fields, usize::from(width).saturating_sub(spans_width(&prompt)));
    let mut spans = prompt;
    spans.extend(fields.into_iter().flat_map(|field| field.spans));
    fit_spans(spans, width)
}

/// Build the row-1 fields in display order: tokens, active/paused/queued counts,
/// the active worker names, the project clock. Counts are spelled `Na Np Nq` as in
/// the §2.2 sample row.
///
/// Only the token pair and the active count carry `drop: None`: §2.5 makes row 1
/// keep tokens plus the active/paused/queued counts at every width its ladder
/// reaches, so those are mandatory. The name list is the row's `flex` field —
/// clipped, not deleted — and everything else has a rank, so it leaves one field at
/// a time and only once the mandatory part genuinely cannot fit.
fn metrics(data: &StatusData<'_>, tokens: (usize, usize), elapsed: Option<u64>) -> Vec<Field> {
    let active_names: Vec<&str> = data
        .workers
        .iter()
        .filter(|row| row.is_active)
        .map(|row| row.name.as_str())
        .collect();
    let active = active_names.len();
    let paused = data.workers.iter().filter(|row| row.paused).count();
    let queued = data.workers.iter().filter(|row| row.queue.is_some()).count();

    let mut fields = vec![
        Field {
            // §2.5 keeps tokens at every width, so this one is never dropped.
            drop: None,
            flex: false,
            spans: vec![
                Span::styled(" Tokens ", theme::chip()),
                Span::styled(
                    format!(
                        "{} in / {} out",
                        format_count(tokens.0),
                        format_count(tokens.1)
                    ),
                    theme::text_primary(),
                ),
            ],
        },
        Field {
            // The active count is the other mandatory field (§2.5: row 1 keeps
            // "tokens + active/paused/queued").
            drop: None,
            flex: false,
            spans: vec![
                Span::styled(" | ", theme::text_dim()),
                Span::styled(format!("{active}a"), theme::chip()),
            ],
        },
    ];
    // `ctx %` (spec §2.2 line 82, §2.2 sample row): the orchestrator's context
    // tokens over `config.max_context_tokens`. It sits next to the token pair it
    // is derived from, and §7.4 puts it last in the drop ladder — so it is only
    // the first thing cut once the counters themselves no longer fit. A `ctx_limit`
    // of 0 means the renderer never saw a config: printing a percentage of an
    // unknown budget would be an invented number, so the field is withheld.
    if data.ctx_limit > 0 && data.ctx_tokens > 0 {
        let pct = (data.ctx_tokens.saturating_mul(100) / data.ctx_limit).min(100);
        fields.push(Field {
            drop: Some(DROP_CTX),
            flex: false,
            spans: vec![
                Span::styled(" | ", theme::text_dim()),
                Span::styled(format!("ctx {pct}%"), theme::text_dim()),
            ],
        });
    }
    if paused > 0 {
        fields.push(Field {
            drop: Some(DROP_PAUSED),
            flex: false,
            spans: vec![
                Span::styled(" | ", theme::text_dim()),
                Span::styled(format!("{paused}p"), theme::chip()),
            ],
        });
    }
    if queued > 0 {
        fields.push(Field {
            drop: Some(DROP_QUEUED),
            flex: false,
            spans: vec![
                Span::styled(" | ", theme::text_dim()),
                Span::styled(format!("{queued}q"), theme::chip()),
            ],
        });
    }
    if !active_names.is_empty() {
        // One span per name, so a narrow bar cuts the *list* — whole names in, the
        // rest behind an ellipsis — rather than the whole field or a name's tail.
        let mut spans = vec![Span::styled(" | ", theme::text_dim())];
        for (index, name) in active_names.iter().enumerate() {
            let lead = if index == 0 { "" } else { ", " };
            spans.push(Span::styled(format!("{lead}{name}"), theme::text_dim()));
        }
        fields.push(Field {
            drop: None,
            flex: true,
            spans,
        });
    }
    if let Some(secs) = elapsed {
        fields.push(Field {
            drop: Some(DROP_CLOCK),
            flex: false,
            spans: vec![
                Span::styled(" | ", theme::text_dim()),
                Span::styled(
                    format!(
                        "elapsed {}",
                        crate::orchestrator::workers::format_duration_human(secs)
                    ),
                    theme::text_dim(),
                ),
            ],
        });
    }
    fields
}

/// Row 2: the persistent state-chip echo of every toggle (G5) plus the armed
/// prompt (§8.5). The worker counts ride this row only while nothing is armed,
/// because the armed case puts them back on row 1 where the banner used to be.
///
/// The scroll-state chip is deliberately absent: `● LIVE` / `↓ N new` belongs to
/// the transcript pane that owns the viewport (G7), and echoing it here as well put
/// the same chip on screen twice per frame, in two places that could disagree.
/// Every chip that stays here states something only this row can state.
fn chips(data: &StatusData<'_>) -> Vec<Field> {
    let mut chips = vec![Field {
        drop: Some(DROP_PLAN_CHIP),
        flex: false,
        spans: vec![Span::styled(
            format!(" plan:{} ", presence(data.plan_visible, data.plan_present)),
            theme::chip(),
        )],
    }];
    chips.push(Field {
        drop: Some(DROP_WORKERS_CHIP),
        flex: false,
        spans: vec![Span::styled(
            format!(
                " workers:{} ",
                presence(data.workers_visible, data.workers_present)
            ),
            theme::chip(),
        )],
    });
    chips.push(Field {
        drop: Some(DROP_THOUGHT_CHIP),
        flex: false,
        spans: vec![Span::styled(
            format!(" think:{} ", on_off(data.show_thought)),
            theme::chip(),
        )],
    });
    if !data.abort_armed {
        let active = data.workers.iter().filter(|row| row.is_active).count();
        let paused = data.workers.iter().filter(|row| row.paused).count();
        let queued = data.workers.iter().filter(|row| row.queue.is_some()).count();
        chips.push(Field {
            drop: Some(DROP_COUNTS_CHIP),
            flex: false,
            spans: vec![Span::styled(
                format!(" {active}a {paused}p {queued}q "),
                theme::chip(),
            )],
        });
    }
    chips
}

/// The three honest states of a pane-presence chip: drawn this frame (`on`),
/// toggled off (`off`), or toggled on but taken away by the §2.5 ladder before the
/// split ever gave it a rect (`narrow`). The third is what stops `workers:on` from
/// promising a pane the operator is not looking at, and states why it went.
fn presence(visible: bool, present: bool) -> &'static str {
    match (visible, present) {
        // A pane with a rect is on screen whatever the toggle says — the ladder
        // never hands a rect to a pane that is off, so this arm is the honest one.
        (true, true) | (false, true) => "on",
        (true, false) => "narrow",
        (false, false) => "off",
    }
}

/// The chip row, fitted so the armed prompt is never the part that gets cut: the
/// prompt reserves its cells first and the chips drop in ladder order (§7.3).
fn chip_line(data: &StatusData<'_>, width: u16) -> Vec<Span<'static>> {
    let prompt = if data.abort_armed { armed_prompt() } else { Vec::new() };
    let mut fields = chips(data);
    fit_fields(&mut fields, usize::from(width).saturating_sub(spans_width(&prompt)));
    let mut spans: Vec<Span<'static>> = fields.into_iter().flat_map(|field| field.spans).collect();
    spans.extend(prompt);
    fit_spans(spans, width)
}

/// Fit one bar row into `budget` cells: the flexible field is *clipped* into the
/// space the other fields leave it, and a ranked field is dropped only once the
/// unclipped part of the row no longer fits at all.
///
/// The old behaviour was a plain drop loop that deleted the widest field — the
/// roster of active names — first: on a 100-column bar with ~58 columns free the
/// row came out as `Tokens 12.4k in / 3.2k out | 3a | ctx 45%`, having thrown away
/// `1p`, `1q` and every worker name because deleting a field was cheaper than
/// shrinking one. §7.4 orders it the other way round: `active names (counts kept)`.
/// The counts are what the row must keep, so the name list is the field that gets
/// cut, and a ranked field only leaves when the mandatory fields on their own
/// cannot fit — which is exactly where the §2.5 ladder says they may.
fn fit_fields(fields: &mut Vec<Field>, budget: usize) {
    // A name list with less than this left is a separator, an ellipsis and nobody's
    // name: below the floor the field goes and the row stops being cut further.
    const FLEX_MIN: usize = 10;

    loop {
        let fixed = fields_width(fields.iter().filter(|field| !field.flex));
        if fixed > budget {
            // The mandatory fields plus the ranked ones do not fit: give up the
            // lowest rank still on the row, one field at a time.
            let Some(worst) = fields
                .iter()
                .filter(|field| !field.flex)
                .filter_map(|field| field.drop)
                .min()
            else {
                break;
            };
            fields.retain(|field| field.flex || field.drop != Some(worst));
            continue;
        }
        let left = budget - fixed;
        if left < FLEX_MIN {
            fields.retain(|field| !field.flex);
            break;
        }
        for field in fields.iter_mut().filter(|field| field.flex) {
            let clipped = clip_names(std::mem::take(&mut field.spans), left);
            field.spans = clipped;
        }
        break;
    }
}

/// Clip the active-name list into `budget` cells, whole names first: the ones that
/// still fit stay, and everything past them becomes a single `…` (§7.4: a clipped
/// line ends in `…`). When not even one whole name fits, the cut falls back to the
/// cell-granular clip every single-row line uses, so the row still shows who is in
/// flight instead of a bare separator.
fn clip_names(spans: Vec<Span<'static>>, budget: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= budget {
        return spans;
    }
    let ellipsis = theme::ELLIPSIS.to_string();
    let ellipsis_width = UnicodeWidthStr::width(ellipsis.as_str());
    let mut kept: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in &spans {
        let width = spans_width(std::slice::from_ref(span));
        if used + width + ellipsis_width > budget {
            break;
        }
        kept.push(span.clone());
        used += width;
    }
    // Only the leading separator fit, so a name-granular cut would leave the field
    // saying nothing: clip inside the first name instead.
    if kept.len() < 2 {
        return fit_spans(spans, u16::try_from(budget).unwrap_or(u16::MAX));
    }
    kept.push(Span::styled(ellipsis, theme::text_dim()));
    kept
}

/// Cells a set of fields occupies, measured with `unicode-width` (§7.2: glyph
/// widths are measured, never assumed to be one cell).
fn fields_width<'a>(fields: impl Iterator<Item = &'a Field>) -> usize {
    fields.map(|field| spans_width(&field.spans)).sum()
}

/// The armed-abort prompt: on row 2 so the metrics survive (§7.3), or leading the
/// single row when the ladder collapsed the bar (§2.5).
fn armed_prompt() -> Vec<Span<'static>> {
    vec![
        Glyph::Failed.badge(),
        Span::styled(" abort armed — Esc again ".to_string(), theme::error()),
    ]
}

/// The `on`/`off` spelling used by every state chip.
fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// Compact token count, spelled exactly like the legacy status bar so a number
/// never changes shape mid-rebuild (`1234` → `1.2k`, `1_500_000` → `1.5M`). Shared
/// with the header so both chrome panes print the same shape.
pub(crate) fn format_count(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::QueueState;

    /// Draw the status bar into a `width x height` test terminal and hand back the
    /// text of every row, right-trimmed.
    fn draw(width: u16, height: u16, data: &StatusData<'_>) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, width, height), data))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// A worker row shaped like a registry projection: only the state fields the
    /// status bar reads are set.
    fn worker(name: &str, is_active: bool, paused: bool, queued: bool) -> SubagentDetail {
        SubagentDetail {
            name: name.to_string(),
            is_active,
            paused,
            queue: queued.then_some(QueueState { position: 1, of: 3 }),
            ..SubagentDetail::default()
        }
    }

    fn data<'a>(workers: &'a [SubagentDetail], abort_armed: bool) -> StatusData<'a> {
        StatusData {
            workers,
            show_thought: false,
            plan_visible: true,
            workers_visible: true,
            plan_present: true,
            workers_present: true,
            abort_armed,
            tokens_in: 0,
            tokens_out: 0,
            ctx_tokens: 0,
            ctx_limit: 0,
            elapsed: None,
        }
    }

    /// The same bar with the session metrics t-009b plumbs, for the tests that
    /// assert on the `ctx %` ratio or the project clock.
    fn metered<'a>(workers: &'a [SubagentDetail], tokens: (usize, usize), ctx: (usize, usize)) -> StatusData<'a> {
        StatusData {
            tokens_in: tokens.0,
            tokens_out: tokens.1,
            ctx_tokens: ctx.0,
            ctx_limit: ctx.1,
            elapsed: Some(723),
            ..data(workers, false)
        }
    }

    /// Join a span run back into plain text for assertions.
    fn text(spans: Vec<Span<'static>>) -> String {
        spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn metrics_stay_visible_while_abort_is_armed() {
        // The legacy defect (`render.rs:1117-1125`): the armed banner erased the
        // whole metrics row.
        let workers = [
            worker("coder_specialist", true, false, false),
            worker("writer", true, true, false),
            worker("researcher", false, false, true),
        ];
        let rows = draw(100, 2, &data(&workers, true));
        assert_eq!(rows.len(), 2, "a two-row rect keeps two rows: {rows:?}");
        assert!(rows[0].contains("Tokens"), "row 1 keeps counting: {rows:?}");
        assert!(rows[0].contains("2a"), "active count on row 1: {rows:?}");
        assert!(rows[0].contains("1p"), "paused count on row 1: {rows:?}");
        assert!(rows[0].contains("1q"), "queued count on row 1: {rows:?}");
        assert!(
            rows[0].contains("coder_specialist") && rows[0].contains("writer"),
            "the running roster is named on row 1: {rows:?}"
        );
        assert!(
            !rows[0].contains("abort"),
            "row 1 is metrics, not the old banner: {rows:?}"
        );
        assert!(
            rows[1].contains('✗') && rows[1].contains("abort armed"),
            "the armed prompt lives on row 2: {rows:?}"
        );

        let joined = rows.join("\n");
        assert!(
            !joined.contains("LIVE") && !joined.contains("follow"),
            "the scroll-state chip is the transcript's, not this row's: {rows:?}"
        );
        assert!(joined.contains("plan:on") && joined.contains("workers:on"));
        assert!(joined.contains("think:off"), "{rows:?}");
    }

    #[test]
    fn chip_row_echoes_every_toggle_and_the_counts_when_not_armed() {
        // G5: a toggle never leaves a silent frame behind.
        let workers = [worker("coder_specialist", true, false, false)];
        let rows = draw(100, 2, &data(&workers, false));
        assert!(
            !rows[1].contains("LIVE") && !rows[1].contains('\u{2193}'),
            "no scroll-state chip on this row: {rows:?}"
        );
        assert!(rows[1].contains("plan:on"), "{rows:?}");
        assert!(rows[1].contains("workers:on"), "{rows:?}");
        assert!(rows[1].contains("think:off"), "{rows:?}");
        assert!(rows[1].contains("1a 0p 0q"), "counts on row 2: {rows:?}");
        assert!(
            !rows[1].contains("abort"),
            "no armed prompt while unarmed: {rows:?}"
        );

        let flipped = StatusData {
            workers: &[],
            show_thought: true,
            plan_visible: false,
            workers_visible: false,
            plan_present: false,
            workers_present: false,
            abort_armed: false,
            ..data(&[], false)
        };
        let rows = draw(100, 2, &flipped);
        assert!(rows[1].contains("plan:off"), "{rows:?}");
        assert!(rows[1].contains("workers:off"), "{rows:?}");
        assert!(rows[1].contains("think:on"), "{rows:?}");
        assert!(rows[1].contains("0a 0p 0q"), "G5 with zero rows: {rows:?}");
    }

    #[test]
    fn the_armed_repaint_overwrites_the_whole_legacy_banner_row() {
        // The legacy bar wrote the row as a full-width paragraph, so the counters
        // have to repaint every cell: a banner longer than the metrics line must
        // not leave its tail standing behind them.
        let banner = " ⚠ [ABORT ARMED] Press ESC or Ctrl+C again to abort | Any other key to cancel ";
        let workers = [worker("coder_specialist", true, false, false)];
        let d = data(&workers, true);
        let backend = ratatui::backend::TestBackend::new(80, 2);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let rect = Rect::new(0, 0, 80, 2);
                frame.render_widget(Paragraph::new(Line::from(banner)), rect);
                render(frame, rect, &d);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = |y: u16| {
            (0..80)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        assert!(row(0).contains("Tokens"), "row 1 counts again: {}", row(0));
        assert!(
            !row(0).contains("ABORT ARMED") && !row(0).contains("Any other key"),
            "no banner tail survives: {}",
            row(0)
        );
        assert!(
            row(1).contains("abort armed") && !row(1).contains("Any other key"),
            "row 2 carries only this component's prompt: {}",
            row(1)
        );
    }

    #[test]
    fn the_armed_prompt_stays_whole_on_a_narrow_chip_row() {
        // §7.3 puts the prompt on row 2; when the row cannot hold every chip too, a
        // chip goes instead of the prompt being clipped. One column narrower than
        // this test's width is the last one that still holds all three chips, so the
        // width is chosen where the ladder has to decide.
        let workers = [worker("coder_specialist", true, false, false)];
        let rows = draw(56, 2, &data(&workers, true));
        assert!(rows[1].ends_with("Esc again"), "prompt intact: {rows:?}");
        assert!(rows[1].contains('✗'), "{rows:?}");
        assert!(rows[1].contains("plan:on"), "{rows:?}");
        assert!(
            !rows[1].contains("think:"),
            "a chip gave way to keep the prompt whole: {rows:?}"
        );
    }

    #[test]
    fn single_row_status_keeps_metrics_and_the_armed_prompt() {
        // §2.5 `rows < 20`: one metrics-only row, armed prompt on it.
        let workers = [worker("coder_specialist", true, false, false)];
        let rows = draw(90, 1, &data(&workers, true));
        assert_eq!(rows.len(), 1, "one row for a one-row rect: {rows:?}");
        assert!(rows[0].starts_with('✗'), "prompt leads the row: {rows:?}");
        assert!(rows[0].contains("abort armed"), "prompt on the row: {rows:?}");
        assert!(rows[0].contains("Tokens"), "metrics survive: {rows:?}");
        assert!(rows[0].contains("1a"), "counts survive: {rows:?}");

        // Un-armed single row: this component is the pane's only owner now
        // (t-009b retired the legacy metrics renderer), so it paints its own
        // metrics row instead of leaving the row to another pass.
        let rows = draw(90, 1, &data(&workers, false));
        assert!(
            rows[0].contains("Tokens") && rows[0].contains("1a"),
            "metrics are drawn by the component: {rows:?}"
        );
        assert!(
            !rows[0].contains('✗') && !rows[0].contains("abort armed"),
            "nothing is armed: {rows:?}"
        );
    }

    #[test]
    fn the_ctx_field_is_the_plumbed_ratio_and_the_last_ranked_field_dropped() {
        // §2.2 line 82 and §7.4: `ctx` is `orchestrator_context_tokens` /
        // `config.max_context_tokens`, and it is the last ranked field the row-1
        // ladder gives up.
        let workers = [worker("coder_specialist", true, false, false)];
        let metered = metered(&workers, (12_400, 3_100), (12_300, 30_000));
        let rows = draw(100, 2, &metered);
        assert!(rows[0].contains("ctx 41%"), "{rows:?}");
        assert!(rows[0].contains("12.4k in / 3.1k out"), "{rows:?}");
        assert!(rows[0].contains("elapsed 12m 3s"), "{rows:?}");

        // Without the denominator the field is withheld rather than guessed.
        let blind = StatusData {
            ctx_limit: 0,
            ..metered
        };
        assert!(!draw(100, 2, &blind)[0].contains("ctx "), "{blind:?}");

        // §7.4 on a squeezed row: the name list is *cut*, the counts and `ctx`
        // stay — the old bar deleted the roster instead, and took the counters with
        // it as soon as the roster was the widest field on the row.
        let squeezed = draw(52, 2, &metered);
        assert!(squeezed[0].contains("ctx 41%"), "{squeezed:?}");
        assert!(squeezed[0].contains("1a"), "{squeezed:?}");
        assert!(
            squeezed[0].contains("coder_") && squeezed[0].contains(theme::ELLIPSIS),
            "the name list is clipped and the cut is marked: {squeezed:?}"
        );
    }

    #[test]
    fn a_pane_the_ladder_took_away_is_not_announced_as_on() {
        // The presence chips read `layout::PaneRects`, not the mode or the toggle:
        // at 80 columns the §2.5 ladder stacks the transcript over one side pane,
        // so the other toggle is still on while its pane has no rect. A chip that
        // said `workers:on` for a pane nobody can see is a chip that lies (G5).
        let workers = [worker("coder_specialist", true, false, false)];
        let squeezed = StatusData {
            plan_present: true,
            workers_present: false,
            ..data(&workers, false)
        };
        let rows = draw(80, 2, &squeezed);
        assert!(rows[1].contains("plan:on"), "{rows:?}");
        assert!(
            rows[1].contains("workers:narrow") && !rows[1].contains("workers:on"),
            "toggled on, drawn nowhere: {rows:?}"
        );

        // A pane the operator switched off is spelled `off`, and a pane the ladder
        // took away never claims to be on: the two states stay distinguishable.
        let off = StatusData {
            plan_visible: false,
            plan_present: false,
            workers_visible: false,
            workers_present: false,
            ..data(&workers, false)
        };
        let rows = draw(80, 2, &off);
        assert!(rows[1].contains("plan:off") && rows[1].contains("workers:off"), "{rows:?}");
    }

    #[test]
    fn this_row_carries_no_scroll_state_of_its_own() {
        // The `● LIVE` chip used to be printed twice per frame: once on the
        // transcript's bottom border (the pane that owns the viewport) and once
        // here. One chip, one owner — the transcript's.
        let workers = [worker("coder_specialist", true, false, false)];
        for armed in [false, true] {
            let rows = draw(100, 2, &data(&workers, armed));
            let joined = rows.join("\n");
            assert!(!joined.contains("LIVE"), "{rows:?}");
            assert!(!joined.contains('\u{2193}'), "{rows:?}");
            assert!(!joined.contains("new"), "{rows:?}");
        }
    }

    #[test]
    fn row_one_keeps_the_counts_and_clips_the_names_then_drops_by_rank() {
        // §7.4 spells the counter part as `queued` → `paused` → active names
        // (counts kept); §2.5 makes row 1 keep tokens plus active/paused/queued at
        // every width its ladder reaches. So the name list is the field that gives
        // way — by being cut, not deleted — and only after it does a ranked field
        // leave, one at a time.
        let workers = [
            worker("coder_specialist", true, true, false),
            worker("documentation_writer", true, false, true),
        ];
        let d = data(&workers, true);
        let line = |width: u16| text(build_metrics_line(&d, (0, 0), Some(75), width, false));

        // The defect this replaces: on a 100-column bar with ~58 columns free the
        // row read `Tokens 0 in / 0 out | 2a | ctx …`, because the widest field was
        // deleted first and took the roster, the paused count and the queued count
        // down with it.
        let at_100 = line(100);
        assert!(
            at_100.contains("2a") && at_100.contains("1p") && at_100.contains("1q"),
            "counts survive a 100-column row: {at_100:?}"
        );
        assert!(at_100.contains("elapsed 1m 15s"), "{at_100:?}");
        assert!(at_100.contains("documentation_writer"), "{at_100:?}");

        // §2.5: the `Na Np Nq` counts are on the row at 80 columns and wider.
        for width in [80u16, 90, 100, 120] {
            let row = line(width);
            assert!(
                row.contains("2a") && row.contains("1p") && row.contains("1q"),
                "{width}: {row:?}"
            );
        }

        // The name list is cut first, and the cut is marked.
        let at_80 = line(80);
        assert!(
            at_80.contains("coder_specialist")
                && !at_80.contains("documentation_writer")
                && at_80.contains(theme::ELLIPSIS),
            "the roster is clipped, not deleted: {at_80:?}"
        );

        // Read the ladder off the row rather than pinning it to fixed widths: each
        // field goes at a strictly narrower terminal than the one before it.
        let mut disappeared: Vec<(&str, u16)> = Vec::new();
        for width in (20..=120u16).rev() {
            let row = line(width);
            for (field, probe) in [
                ("names", "documentation_writer"),
                ("clock", "elapsed"),
                ("queued", "1q"),
                ("paused", "1p"),
            ] {
                if !row.contains(probe) && !disappeared.iter().any(|(seen, _)| *seen == field) {
                    disappeared.push((field, width));
                }
            }
        }
        assert_eq!(
            disappeared.iter().map(|(field, _)| *field).collect::<Vec<_>>(),
            ["names", "clock", "queued", "paused"],
            "row-1 ladder: {disappeared:?}"
        );
        for pair in disappeared.windows(2) {
            assert!(
                pair[0].1 > pair[1].1,
                "one field at a time, never two at once: {disappeared:?}"
            );
        }

        // Invariant at every width the ladder can be asked for: nothing exceeds
        // its rect, and the two never-dropped fields are always on the line.
        for width in [16u16, 24, 32, 48, 64, 96, 128] {
            let row = line(width);
            assert!(row.contains("Tokens"), "{width}: {row:?}");
            if width >= 32 {
                assert!(row.contains("2a"), "{width}: {row:?}");
            }
            assert!(
                UnicodeWidthStr::width(row.as_str()) <= usize::from(width),
                "{width} overflows the rect: {row:?}"
            );
        }
    }

    #[test]
    fn armed_prompt_survives_a_narrow_single_row_bar() {
        // The prompt reserves its cells before the ladder runs and leads the row, so
        // the counters degrade instead of the actionable signal being cut.
        let workers = [worker("coder_specialist_with_a_long_name", true, true, true)];
        let d = data(&workers, true);
        for width in [40u16, 60, 80] {
            let row = text(build_metrics_line(&d, (0, 0), Some(75), width, true));
            assert!(row.starts_with('✗'), "{width}: {row:?}");
            assert!(row.contains("abort armed"), "{width}: {row:?}");
            assert!(row.contains("Tokens"), "{width}: {row:?}");
            assert!(
                UnicodeWidthStr::width(row.as_str()) <= usize::from(width),
                "{width} overflows the rect: {row:?}"
            );
        }
    }

    #[test]
    fn no_validation_or_arbitration_round_number_is_ever_printed() {
        // Spec §8.4 plus gate (c): `val N` waits for D1 and the arbitration round
        // is not data, so neither may appear at any width or row count.
        let workers = [worker("coder_specialist", true, true, true)];
        for (width, height, armed) in [(120, 2, false), (120, 2, true), (70, 1, true), (64, 2, true)] {
            let joined = draw(width, height, &data(&workers, armed)).join("\n");
            assert!(!joined.contains("val"), "{joined:?}");
            assert!(!joined.contains("round"), "{joined:?}");
        }
    }

    #[test]
    fn count_formatting_matches_the_legacy_status_bar_shape() {
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1.0k");
        assert_eq!(format_count(12_400), "12.4k");
        assert_eq!(format_count(1_500_000), "1.5M");
    }

    #[test]
    fn a_zero_rect_is_never_drawn_into_and_a_one_row_bar_never_panics() {
        // §2.4: a pane rect is never zero-sized, and a component never draws into
        // one that is anyway.
        draw(0, 0, &data(&[], true));
        draw(1, 1, &data(&[], true));
        draw(40, 1, &data(&[], false));
    }
}
