//! Help / keymap surfaces — spec §5 (one table, three surfaces), §3 (Help
//! overlay), §10.3 (the legend, the footer text and the `/help` output are
//! byte-identical, and the legend names no key the handler does not bind),
//! §7.3 (overlays draw last, centred, sized to their content clamped to 80 %),
//! §2.5 (short legend below 70 columns).
//!
//! [`legend`] is the single source of truth for the keymap: [`footer_legend`]
//! and [`help_text`] (the `/help` transcript output, and the body of the `F1`
//! overlay) are both derived from those rows, so neither can go stale the way
//! the hard-coded legend at `events.rs:789-813` did. Bindings that §5 deletes
//! are not rows — the legend may only name keys the handler binds — they are
//! listed by [`removed`] under the table so an operator with the old keymap in
//! muscle memory learns what replaced them.
//!
//! The confirm body is plumbed (t-013): `HelpData` carries the `ConfirmRequest`
//! the renderer's pending `events::ConfirmAction` projects onto — the action's
//! name and its consequence — and [`confirm_lines`] pairs those with the
//! overlay-scope rows of the same table, so the surface always names what `y`
//! will run.

use super::{fit_spans, pane_block};
use crate::ui::tui::components::input::slash_commands;
use crate::ui::tui::state::OverlayState;
use crate::ui::tui::theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

/// Below this many columns or rows an overlay cannot hold a bordered table, so
/// nothing is drawn rather than a degenerate rect (§2.4 zero-rect rule).
const MIN_OVERLAY_COLS: u16 = 8;
/// See [`MIN_OVERLAY_COLS`].
const MIN_OVERLAY_ROWS: u16 = 4;

/// Section headings of the help body, spelled once.
const SECTION_KEYS: &str = "Keys";
/// See [`SECTION_KEYS`].
const SECTION_COMMANDS: &str = "Slash commands";
/// See [`SECTION_KEYS`].
const SECTION_REMOVED: &str = "Removed bindings";

/// The close hint printed on the overlay's bottom border (§5: `Esc` closes, no
/// arming).
const CLOSE_HINT: &str = " Esc close ";

/// The hint that the clipped remainder of the table is one `/help` away.
const MORE_HINT: &str = " /help prints the full table ";

/// The keys the wide footer legend prints, in the order §2.2 prints them. The
/// bare-letter rows are gone with the bindings (operator request), so the footer
/// carries the chord that replaced them and no longer advertises the recall —
/// which lives in the `/error` row of the command table instead.
const FOOTER_KEYS: [&str; 7] = [
    "F1 / ?",
    "Tab / Shift+Tab",
    "F2",
    "F3 / F4 / F5",
    "Ctrl+X",
    "Esc",
    "Ctrl+P / Ctrl+A",
];

/// The keys the `width < 70` short legend keeps, in the order §2.5 prints them.
const COMPACT_KEYS: [&str; 3] = ["F1 / ?", "Esc", "Enter"];

/// One binding of the single keymap table (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegendRow {
    /// The key or chord, as printed. Unique across the table — `handle_events`
    /// matches the same strings, so a key appears in exactly one row.
    pub keys: &'static str,
    /// What it does.
    pub action: &'static str,
    /// Which surface interprets it: `composer`, `pane`, `global` or `overlay`,
    /// joined with ` / ` when a key means different things on two surfaces.
    pub scope: &'static str,
    /// Short footer label; empty means the row is not shown in the footer.
    pub footer: &'static str,
}

/// The keymap: every binding the rebuilt TUI has, in the order of spec §5.
///
/// The bindings are those of the current handler (UX baseline §1) plus the two
/// surfaces this rebuild introduces — `?`/`F1` for the legend and the confirm
/// overlay's `y`/`n`. Nothing else is claimed here: a row for a key nobody
/// binds is exactly the staleness this table exists to prevent.
pub fn legend() -> Vec<LegendRow> {
    vec![
        LegendRow {
            keys: "Enter",
            action: "submit the composer",
            scope: "composer",
            footer: "Enter send",
        },
        LegendRow {
            keys: "Shift+Enter / Alt+Enter",
            action: "insert a newline",
            scope: "composer",
            footer: "",
        },
        LegendRow {
            keys: "Ctrl+Enter / Ctrl+J",
            action: "insert a newline",
            scope: "composer",
            footer: "",
        },
        LegendRow {
            keys: "Printable / paste",
            action: "insert in the composer, any focus",
            scope: "composer",
            footer: "",
        },
        LegendRow {
            keys: "Backspace / Delete",
            action: "edit the composer, any focus",
            scope: "composer",
            footer: "",
        },
        LegendRow {
            keys: "Ctrl+Z / Ctrl+Y",
            action: "undo / redo the draft",
            scope: "composer",
            footer: "",
        },
        // The scroll-conflict repair (audit defects 1, 2, 3, 7, 10, 14) plus the
        // reported follow-up: one predicate decides the owner of these keys —
        // `FocusPane::Input` holds focus, or it does not — so the legend states the
        // owner first and the gesture second. Inside the composer the draft only ever
        // chooses where the caret goes; the history walk belongs to the chord row
        // above and to nothing else, so no arrow row can advertise it.
        LegendRow {
            keys: "Ctrl+Up / Ctrl+Down",
            action: "walk input history, in any focus",
            scope: "composer",
            footer: "",
        },
        LegendRow {
            keys: "Up / Down",
            action: "caret in a multi-line draft · pane scroll by 1",
            scope: "composer / pane",
            footer: "",
        },
        LegendRow {
            keys: "Left / Right",
            action: "Workers roster selection · else caret",
            scope: "composer / pane",
            footer: "",
        },
        LegendRow {
            keys: "Home / End",
            action: "composer caret start/end · pane top/tail",
            scope: "composer / pane",
            footer: "",
        },
        LegendRow {
            keys: "PageUp / PageDown",
            action: "page the focused pane by 10, never the composer",
            scope: "pane",
            footer: "",
        },
        LegendRow {
            keys: "Tab / Shift+Tab",
            action: "cycle focus, and accept a completion",
            scope: "global",
            footer: "Tab focus",
        },
        LegendRow {
            keys: "F1 / ?",
            action: "open this overlay (`?` when empty)",
            scope: "global",
            footer: "F1 help",
        },
        LegendRow {
            keys: "F2",
            action: "cycle mode M4 → M1 → M2 → M3 (plan left)",
            scope: "global",
            footer: "F2 mode",
        },
        LegendRow {
            keys: "F3 / F4 / F5",
            action: "go to Transcript / Plan / Workers",
            scope: "global",
            footer: "F3/F4/F5 panes",
        },
        LegendRow {
            keys: "Ctrl+P / Ctrl+A",
            action: "show the Plan / Workers pane",
            scope: "global",
            footer: "Ctrl+A/P panes",
        },
        LegendRow {
            keys: "Ctrl+T",
            action: "toggle the thinking chip",
            scope: "global",
            footer: "",
        },
        LegendRow {
            keys: "Ctrl+F",
            action: "toggle transcript follow",
            scope: "global",
            footer: "",
        },
        LegendRow {
            keys: "Ctrl+X",
            action: "cancel the selected worker (confirm)",
            scope: "pane",
            footer: "Ctrl+X cancel-worker",
        },
        LegendRow {
            keys: "Esc",
            action: "close overlay, else arm, else exit",
            scope: "global",
            footer: "Esc arm",
        },
        LegendRow {
            keys: "Ctrl+C",
            action: "same as Esc",
            scope: "global",
            footer: "",
        },
        LegendRow {
            keys: "Ctrl+D",
            action: "open the Quit confirm, never un-armed",
            scope: "global",
            footer: "",
        },
        LegendRow {
            keys: "Printable while armed",
            action: "disarm abort",
            scope: "global",
            footer: "",
        },
        LegendRow {
            keys: "y / n",
            action: "confirm / dismiss the pending action",
            scope: "overlay",
            footer: "",
        },
        LegendRow {
            keys: "Resize",
            action: "clamp the scroll offsets and redraw",
            scope: "global",
            footer: "",
        },
    ]
}

/// What §5 deletes, with the reason — bindings and behaviours both. They stay
/// out of [`legend()`], because the legend may only name keys the handler binds
/// (§10.3), and are printed under the table so the old muscle memory is told
/// what replaced it.
pub fn removed() -> Vec<&'static str> {
    vec![
        "mouse scroll / click — removed: mouse capture is never enabled, so the handler block was unreachable",
        "Ctrl+D as immediate exit — removed: Ctrl+D now opens the Quit confirm overlay",
        "bare stop / cancel / exit / quit / q / abort — removed: ordinary steering text; the slash forms open a confirm overlay",
        "Esc re-focusing Chat — removed: Esc closes an overlay or arms abort, it never changes focus",
        "3-row worker roster clamp — removed: every live worker is reachable",
        "bare x — removed: it armed on an empty draft in the Workers pane and swallowed the letter; Ctrl+X, or /cancel <worker key or task id>, now asks to cancel the selected worker (confirm)",
        "bare e — removed: it armed on an empty draft over a pane focus and swallowed the letter; /error prints the sticky last error from any focus",
        "plain Up / Down history recall — removed: it fired while the operator was scrolling a pane and replaced the draft; Ctrl+Up / Ctrl+Down walks the input history in every focus",
    ]
}

/// The legend as two-column lines: the key column padded to the widest key
/// string, then the action and its scope in parentheses. The `F1` overlay
/// renders these lines and `/help` prints them, which is what makes the two
/// surfaces identical (§10.3).
pub fn legend_table() -> Vec<String> {
    let rows = legend();
    let column = rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.keys))
        .max()
        .unwrap_or(0)
        + 1;
    rows.iter()
        .map(|row| format!("{:<column$}{} ({})", row.keys, row.action, row.scope))
        .collect()
}

/// The §6 slash-command table as lines, from the same list the composer's
/// autocomplete reads.
pub fn command_table() -> Vec<String> {
    let commands = slash_commands();
    let names: Vec<String> = commands
        .iter()
        .map(|command| {
            if command.args.is_empty() {
                command.name.to_string()
            } else {
                format!("{} {}", command.name, command.args)
            }
        })
        .collect();
    let column = names
        .iter()
        .map(|name| UnicodeWidthStr::width(name.as_str()))
        .max()
        .unwrap_or(0)
        + 1;
    commands
        .iter()
        .zip(names)
        .map(|(command, name)| format!("{:<column$}{}", name, command.effect))
        .collect()
}

/// Every line both help surfaces print, in order: the keymap table, the slash
/// command table and the removed bindings.
pub fn help_text() -> String {
    let mut lines = vec![SECTION_KEYS.to_string()];
    lines.extend(legend_table());
    lines.push(String::new());
    lines.push(SECTION_COMMANDS.to_string());
    lines.extend(command_table());
    lines.push(String::new());
    lines.push(SECTION_REMOVED.to_string());
    lines.extend(
        removed()
            .into_iter()
            .map(|binding| format!("{} {binding}", theme::ELLIPSIS)),
    );
    lines.join("\n")
}

/// The request a confirm overlay is holding (spec §3, §7.3): the **name** of the
/// destructive action and the **consequence** the operator is being asked to
/// accept, as one-line strings. The renderer builds one from its
/// [`ConfirmAction`](super::super::events::ConfirmAction) so the surface can never
/// describe an action other than the one `y` will run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmRequest {
    /// What the request is, e.g. `Cancel worker 'coder_specialist-t-004' (Ctrl+X)`.
    pub action: String,
    /// What running it does, e.g. `Stops that one worker; the rest keep running.`
    pub consequence: String,
}

impl ConfirmRequest {
    /// Build the two lines the surface prints.
    pub fn new(action: impl Into<String>, consequence: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            consequence: consequence.into(),
        }
    }
}

/// The wording of the confirm surface while it holds **no** request: the armed
/// abort prompt of §8.5 occupies the same surface, and it honours no `y`/`n` (§5
/// disarms on printables instead of treating them as answers).
const ARMED_LINES: [&str; 2] = [
    "Abort armed — Esc / Ctrl+C again to abort",
    "any other key cancels the arm; Esc once closes this surface",
];

/// The confirm overlay body: the pending request's name and consequence (or the
/// armed-abort wording when the surface holds no request), then the keys the
/// overlay honours read from the same table as the legend — so the prompt cannot
/// drift from the binding (§5: `y` / `n` are honoured only inside this surface).
fn confirm_lines(request: Option<&ConfirmRequest>) -> Vec<String> {
    let mut lines: Vec<String> = match request {
        Some(request) => vec![request.action.clone(), request.consequence.clone()],
        None => ARMED_LINES.iter().map(|line| line.to_string()).collect(),
    };
    lines.extend(
        legend()
            .into_iter()
            .filter(|row| row.scope == "overlay")
            .map(|row| format!("{} — {}", row.keys, row.action)),
    );
    lines
}

/// `value` cells as a percentage of `of` (§7.3 clamps overlays to 80 %).
const fn percent(of: u16, part: u16) -> u16 {
    ((of as usize * part as usize) / 100) as u16
}

/// Clip one body line to `cells` with the palette ellipsis (§7.4), routed
/// through [`fit_spans`] so the overlay clips exactly like the single-row panes.
fn clip(line: &str, cells: u16) -> String {
    fit_spans(
        vec![Span::styled(line.to_string(), theme::text_primary())],
        cells,
    )
    .into_iter()
    .map(|span| span.content.into_owned())
    .collect()
}

/// The centred overlay rect, sized to the body lines and clamped to 80 % of the
/// area it is drawn into (§7.3). Width and height are never below a bordered
/// box and never above the area, so the overlay can't produce a degenerate rect
/// (§2.4).
fn overlay_rect(area: Rect, lines: &[String]) -> Rect {
    let widest = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(line.as_str()))
        .max()
        .unwrap_or(0);
    let width = (widest + 2)
        .min(usize::from(percent(area.width, 80)).max(3))
        .min(usize::from(area.width)) as u16;
    let height = (lines.len() + 2)
        .min(usize::from(percent(area.height, 80)).max(3))
        .min(usize::from(area.height)) as u16;
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// The footer legend: the §2.2 line, or its §2.5 short form below 70 columns.
///
/// Both are assembled from the [`legend()`] rows the selected keys belong to, so
/// the footer is a projection of the table and cannot go stale the way the
/// hard-coded legend at `events.rs:789-813` did (§5, UX defect 1). A row whose
/// footer label is empty is not advertised in the footer — the overlay and
/// `/help` still document it.
pub fn footer_legend(compact: bool) -> String {
    let rows = legend();
    let keys: &[&str] = if compact { &COMPACT_KEYS } else { &FOOTER_KEYS };
    let labels: Vec<&str> = keys
        .iter()
        .filter_map(|key| {
            rows.iter()
                .find(|row| row.keys == *key)
                .map(|row| row.footer)
        })
        .filter(|label| !label.is_empty())
        .collect();
    // One leading cell of padding, no trailing cell: the §2.2 line then fits a
    // 100-column terminal exactly, and shorter terminals clip it with `…`.
    format!(" {}", labels.join("  "))
}

/// Draw the footer legend, or the modal keymap / confirm overlay.
pub fn render(frame: &mut Frame, rect: Rect, data: &HelpData) {
    match data.kind {
        HelpKind::Legend => {
            // §F3: the footer legend is the guidance a first-run operator reads, so
            // it never sits in the gutter tier.
            let spans = fit_spans(
                vec![Span::styled(
                    footer_legend(data.compact),
                    theme::text_secondary(),
                )],
                rect.width,
            );
            frame.render_widget(Paragraph::new(Line::from(spans)), rect);
        }
        HelpKind::Overlay(overlay) => {
            // A closed overlay draws nothing: the panes keep the frame (§4.4 —
            // `Esc` closes the overlay without arming abort).
            if !overlay.is_open() {
                return;
            }
            let (title, border, body) = match overlay {
                // A modal being open is not a focus change, so the keymap overlay
                // takes the notice frame rather than the Cyan the focused pane owns.
                OverlayState::Help => ("Keys", theme::frame_notice(), help_text()),
                OverlayState::Confirm => (
                    "Confirm",
                    theme::frame_confirm(),
                    confirm_lines(data.confirm.as_ref()).join("\n"),
                ),
                OverlayState::None => return,
            };
            if rect.width < MIN_OVERLAY_COLS || rect.height < MIN_OVERLAY_ROWS {
                return;
            }
            let source: Vec<String> = body.lines().map(str::to_string).collect();
            let area = overlay_rect(rect, &source);
            // Content-sized, then clamped: each line is clipped to the inner
            // width and the tail the clamp cut is marked, so the drawn body
            // always fits the rect it is given (§2.4, §7.4). The mark is keyed on
            // the **source** length rather than on the drawn count: a body that
            // fills the room exactly is not clipped, and printing the hint there
            // would drop a real line to advertise a remainder that does not exist.
            let inner_width = area.width.saturating_sub(2);
            let room = usize::from(area.height).saturating_sub(2);
            let mut lines: Vec<String> = source
                .iter()
                .map(|line| clip(line, inner_width))
                .take(room)
                .collect();
            if source.len() > room && room > 1 {
                lines.truncate(room - 1);
                lines.push(format!("{} see /help for the full table", theme::ELLIPSIS));
            }
            let text: Vec<Line<'static>> = lines.into_iter().map(Line::from).collect();
            frame.render_widget(Clear, area);
            let block = pane_block(title, false, border)
                .title_bottom(Line::from(CLOSE_HINT))
                .title_bottom(
                    Line::from(Span::styled(MORE_HINT, theme::text_secondary()))
                        .alignment(Alignment::Right),
                );
            frame.render_widget(Paragraph::new(text).block(block), area);
        }
    }
}

/// Which help surface this call draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpKind {
    /// The unbordered one-row legend in the footer (§2.2).
    Legend,
    /// The centred modal overlay of §7.3, in the given overlay state.
    Overlay(OverlayState),
}

/// What this help surface draws.
#[derive(Debug, Clone)]
pub struct HelpData {
    /// Legend row or modal overlay.
    pub kind: HelpKind,
    /// Short legend form (`width < 70`, §2.5).
    pub compact: bool,
    /// The request a confirm overlay is holding (§3, §7.3): `Some` names the
    /// action and its consequence, `None` means the surface on screen is the
    /// armed-abort prompt of §8.5, which honours no `y`/`n` (§5). Ignored by the
    /// legend and the `F1` overlay.
    pub confirm: Option<ConfirmRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// Draw the help surface into a `width x height` test terminal and hand
    /// back the text of every row.
    fn draw(width: u16, height: u16, data: &HelpData) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, width, height), data))
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
    fn legend_is_non_empty_with_unique_keys() {
        let rows = legend();
        assert!(!rows.is_empty());
        let mut keys: Vec<&str> = rows.iter().map(|row| row.keys).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "legend has duplicate key strings");
        for row in &rows {
            assert!(!row.action.is_empty(), "{} has no action", row.keys);
            assert!(!row.scope.is_empty(), "{} has no scope", row.keys);
        }
        assert_eq!(rows.len(), 25);
        // §4.4 + the operator's keymap request: no binding is a bare printable
        // character except `?`, which is a row of the "F1 / ?" pair and never a
        // single-character key string of its own.
        for row in &rows {
            assert!(
                row.keys.chars().count() > 1,
                "the legend binds the bare character {:?} again",
                row.keys
            );
        }
    }

    #[test]
    fn legend_scopes_are_the_spec_scopes() {
        const SCOPES: [&str; 4] = ["composer", "pane", "global", "overlay"];
        for row in legend() {
            for part in row.scope.split(" / ") {
                assert!(
                    SCOPES.contains(&part),
                    "{}: unknown scope {part:?}",
                    row.keys
                );
            }
        }
    }

    #[test]
    fn legend_names_every_current_binding() {
        let rows = legend();
        let find = |keys: &str| rows.iter().find(|row| row.keys == keys);
        for keys in [
            "Enter",
            "Shift+Enter / Alt+Enter",
            "Ctrl+Enter / Ctrl+J",
            "Printable / paste",
            "Backspace / Delete",
            "Ctrl+Z / Ctrl+Y",
            "Ctrl+Up / Ctrl+Down",
            "Up / Down",
            "Left / Right",
            "Home / End",
            "PageUp / PageDown",
            "Tab / Shift+Tab",
            "F1 / ?",
            "F2",
            "F3 / F4 / F5",
            "Ctrl+P / Ctrl+A",
            "Ctrl+T",
            "Ctrl+F",
            "Ctrl+X",
            "Esc",
            "Ctrl+C",
            "Ctrl+D",
            "Printable while armed",
            "y / n",
            "Resize",
        ] {
            assert!(find(keys).is_some(), "legend is missing {keys}");
        }
        // §4.4: printable keys are never swallowed, whatever the focus.
        assert_eq!(find("Printable / paste").unwrap().scope, "composer");
        // §5: `Ctrl+D` is a confirm, not an un-armed exit.
        assert!(find("Ctrl+D").unwrap().action.contains("confirm"));
        assert!(find("Ctrl+D").unwrap().action.contains("un-armed"));
    }

    #[test]
    fn legend_advertises_no_removed_binding() {
        // §10.3: the legend names no key the handler does not bind, so the
        // deleted bindings live in `removed()`, never in the table.
        for row in legend() {
            assert!(
                !row.keys.to_ascii_lowercase().contains("mouse"),
                "legend still binds the mouse"
            );
            assert!(!row.keys.contains("stop"), "legend still binds bare words");
        }
        let removed = removed().join("\n");
        assert!(removed.contains("mouse scroll / click"));
        assert!(removed.contains("Ctrl+D"));
        assert!(removed.contains("bare stop / cancel / exit / quit / q / abort"));
        assert!(removed.contains("Esc re-focusing Chat"));
        // The two bare-letter mnemonics the operator asked to lose are not rows of
        // the table either: they are listed here, naming what replaced them.
        assert!(removed.contains("bare x — removed"), "{removed}");
        assert!(removed.contains("bare e — removed"), "{removed}");
        for row in legend() {
            assert_ne!(row.keys, "x", "the bare x mnemonic came back");
            assert_ne!(row.keys, "e", "the bare e mnemonic came back");
        }
    }

    #[test]
    fn legend_table_is_a_two_column_table() {
        let rows = legend();
        let table = legend_table();
        assert_eq!(table.len(), rows.len());
        let column = rows
            .iter()
            .map(|row| row.keys.chars().count())
            .max()
            .unwrap()
            + 1;
        for (row, line) in rows.iter().zip(&table) {
            assert!(line.starts_with(row.keys), "line: {line}");
            // The key column is padded, so every action starts on the same cell
            // and the cell before it is padding.
            assert_eq!(&line[column - 1..column], " ", "line: {line}");
            assert!(line[column..].starts_with(row.action), "line: {line}");
            assert!(line.ends_with(&format!("({})", row.scope)), "line: {line}");
        }
    }

    #[test]
    fn footer_legend_comes_from_the_table() {
        let rows = legend();
        let wide = footer_legend(false);
        // The §2.2 order, assembled from the row labels of the table.
        let labels: Vec<&str> = wide.trim().split("  ").collect();
        assert_eq!(
            labels,
            vec![
                "F1 help",
                "Tab focus",
                "F2 mode",
                "F3/F4/F5 panes",
                "Ctrl+X cancel-worker",
                "Esc arm",
                "Ctrl+A/P panes"
            ]
        );
        // Nothing in the footer is invented: every label is a row of the table.
        for label in &labels {
            assert!(
                rows.iter().any(|row| row.footer == *label),
                "{label:?} is not a legend row label"
            );
        }
        // §2.5 short form: the three keys that survive below 70 columns.
        assert_eq!(footer_legend(true), " F1 help  Esc arm  Enter send");
        // The wide form fits the §2.2 100-column budget without clipping.
        assert!(
            UnicodeWidthStr::width(wide.as_str()) <= 100,
            "wide footer is {} cells",
            UnicodeWidthStr::width(wide.as_str())
        );
        // A row with no footer label is still documented in the overlay.
        let think = rows
            .iter()
            .find(|row| row.keys == "Ctrl+T")
            .expect("Ctrl+T");
        assert!(think.footer.is_empty());
        assert!(!wide.contains("Ctrl+T"));
        assert!(legend_table().iter().any(|line| line.contains("Ctrl+T")));
    }

    #[test]
    fn help_text_carries_the_table_the_commands_and_the_removals() {
        let text = help_text();
        for row in legend() {
            assert!(text.contains(row.keys), "help text missing {}", row.keys);
            assert!(text.contains(row.action), "help text missing action");
        }
        for command in slash_commands() {
            assert!(
                text.contains(command.name),
                "help text missing {}",
                command.name
            );
        }
        for binding in removed() {
            assert!(text.contains(binding), "help text missing removal");
        }
        assert!(text.contains(SECTION_KEYS));
        assert!(text.contains(SECTION_COMMANDS));
        assert!(text.contains(SECTION_REMOVED));
    }

    #[test]
    fn command_table_pairs_each_command_with_its_effect() {
        let table = command_table();
        let commands = slash_commands();
        assert_eq!(table.len(), commands.len());
        assert!(
            table
                .iter()
                .any(|line| line.contains("/cancel <worker key or task id>"))
        );
        assert!(table.iter().any(|line| line.starts_with("/help")));
    }

    #[test]
    fn overlay_prints_the_table_and_the_close_hint() {
        let data = HelpData {
            kind: HelpKind::Overlay(OverlayState::Help),
            compact: false,
            confirm: None,
        };
        let rows = draw(120, 40, &data);
        let frame: String = rows.concat();
        assert!(frame.contains("Keys"), "title missing");
        assert!(frame.contains("Enter"), "key table missing");
        assert!(
            frame.contains("submit the composer"),
            "action column missing"
        );
        assert!(frame.contains("Esc close"), "close hint missing");
        // The overlay and `/help` read from one source (§10.3).
        assert!(frame.contains(&legend_table()[0]));
    }

    #[test]
    fn closed_overlay_draws_nothing() {
        let data = HelpData {
            kind: HelpKind::Overlay(OverlayState::None),
            compact: false,
            confirm: None,
        };
        let rows = draw(80, 20, &data);
        assert!(rows.iter().all(|row| row.trim().is_empty()));
    }

    #[test]
    fn confirm_overlay_prompts_the_overlay_keys_only() {
        let data = HelpData {
            kind: HelpKind::Overlay(OverlayState::Confirm),
            compact: false,
            confirm: None,
        };
        let rows = draw(120, 12, &data);
        let frame: String = rows.concat();
        assert!(frame.contains("Confirm"), "title missing");
        assert!(frame.contains("y / n"), "y/n prompt missing");
        // The surface holding no request is the armed-abort prompt of §8.5, and it
        // says so — it honours no `y`/`n`, so an armed `y` must read as typing.
        assert!(
            frame.contains("Abort armed"),
            "the armed wording is missing: {frame}"
        );
        // The keys of other scopes are not prompt text for this overlay.
        assert!(
            !frame.contains("PageUp"),
            "wrong scope in the confirm prompt"
        );
    }

    #[test]
    fn confirm_overlay_names_the_pending_action_and_its_consequence() {
        // §3, §7.3, G3: the surface states which action is pending, what running it
        // does, and the `y` / `n` affordance — all three, or `y` is a blind press.
        let data = HelpData {
            kind: HelpKind::Overlay(OverlayState::Confirm),
            compact: false,
            confirm: Some(ConfirmRequest::new(
                "Cancel worker 'coder_specialist-t-004' (x)",
                "Stops that one worker; every other worker keeps running.",
            )),
        };
        let rows = draw(120, 12, &data);
        let frame: String = rows.concat();
        assert!(frame.contains("Confirm"), "title missing");
        assert!(
            frame.contains("Cancel worker 'coder_specialist-t-004'"),
            "action name missing: {frame}"
        );
        assert!(
            frame.contains("every other worker keeps running"),
            "consequence missing: {frame}"
        );
        assert!(frame.contains("y / n"), "y/n affordance missing");
        // A named request is not the armed prompt.
        assert!(!frame.contains("Abort armed"), "wrong surface body");
    }

    #[test]
    fn overlay_is_centred_and_clamped_to_80_percent() {
        let area = Rect::new(0, 0, 100, 30);
        let lines = help_text()
            .lines()
            .map(str::to_string)
            .collect::<Vec<String>>();
        let rect = overlay_rect(area, &lines);
        assert!(rect.width <= 80);
        assert!(rect.height <= 24);
        assert!(rect.x + rect.width <= area.x + area.width);
        assert!(rect.y + rect.height <= area.y + area.height);
        // Centred on both axes.
        assert_eq!(rect.x, (100 - rect.width) / 2);
        assert_eq!(rect.y, (30 - rect.height) / 2);
        // A short body keeps its own size instead of the clamp.
        let line = String::from("y / n - confirm");
        let small = overlay_rect(area, std::slice::from_ref(&line));
        assert_eq!(
            usize::from(small.width),
            UnicodeWidthStr::width(line.as_str()) + 2
        );
        assert_eq!(small.height, 3);
    }

    #[test]
    fn overlay_is_skipped_when_the_rect_cannot_hold_it() {
        // §2.4: no degenerate rects — a rect too small for a bordered table
        // draws nothing at all.
        let data = HelpData {
            kind: HelpKind::Overlay(OverlayState::Help),
            compact: false,
            confirm: None,
        };
        let rows = draw(6, 20, &data);
        assert!(rows.iter().all(|row| row.trim().is_empty()));
    }

    #[test]
    fn clipped_overlay_points_at_the_full_table() {
        let data = HelpData {
            kind: HelpKind::Overlay(OverlayState::Help),
            compact: false,
            confirm: None,
        };
        let rows = draw(100, 12, &data);
        let frame: String = rows.concat();
        assert!(frame.contains("see /help for the full table"));
        assert!(frame.contains("Esc close"));
    }

    #[test]
    fn legend_rows_are_the_only_source_of_the_footer_and_the_help_body() {
        // Byte-identity check of §10.3: the overlay body *is* `help_text()`,
        // clipped to the rect, so a row added here shows up on both surfaces.
        let text = help_text();
        let rows: Vec<String> = text.lines().map(str::to_string).collect();
        let data = HelpData {
            kind: HelpKind::Overlay(OverlayState::Help),
            compact: false,
            confirm: None,
        };
        let drawn = draw(160, 60, &data).concat();
        let inner = overlay_rect(Rect::new(0, 0, 160, 60), &rows).width - 2;
        for line in rows.iter().take(10) {
            let clipped = clip(line, inner);
            assert!(drawn.contains(&clipped), "overlay missing {line:?}");
        }
    }
}
