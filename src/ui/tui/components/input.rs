//! Input / composer pane — spec §3 (Input pane), §4.4 (the composer rule), §6
//! (slash commands + autocomplete), §7.3 (border colours), §2.1 + §2.5 (the
//! frame grows with the draft and marks the lines it hides).
//!
//! `render` keeps the signature `mod.rs` calls and draws the composer frame:
//! the border/focus colour, the §4.4 key hint in the title, the §6 ghost
//! suggestion row on the top right, the command-mode / history / draft-length
//! chips and the `+n` overflow marker on the bottom border.
//!
//! Everything that depends on the *draft text* is routed through
//! [`ComposerState`], which t-009b finally handed over: `InputData` now carries the
//! buffer string, the history position **and** the `TextArea` itself, so `render`
//! draws the draft body (widget, caret) inside the frame and the frame owns every
//! border row. Nothing in this pane is drawn by another pass any more.
//!
//! Still open by design rather than by plumbing: the placeholder policy for an
//! empty draft (§8.1 has no input empty state) stays unrendered — a hint sentence
//! is not data the spec pins down, and inventing one in the draft area would read
//! as operator text.

use super::{fit_spans, pane_block, title_width};
use crate::ui::tui::state::FocusPane;
use crate::ui::tui::theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

/// The keys the composer answers to, spelled once for the border title.
///
/// This is the §4.4 rule made visible: only composer-scoped keys are
/// advertised. A letter mnemonic (`x`, `e`, `?`) in this title would imply that
/// those keys do something other than insert a character, and while the
/// composer is the typing target — which is every mode and every focus state —
/// they insert characters.
pub const KEY_HINT: &str = "Enter send, Alt/Shift+Enter newline, Ctrl+Z undo";

/// Draft length past which the frame carries a character count as well as a
/// line count: below this the draft is judgeable by eye.
const LONG_DRAFT_CHARS: usize = 80;

/// What the composer frame draws this frame.
#[derive(Debug, Clone, Copy)]
pub struct InputData<'a> {
    /// Whether the composer pane holds focus (§4.2 badge).
    pub focus: bool,
    /// Whether the composer is the typing target (§4.4: printable keys reach it
    /// in every mode, so its border never reads as unfocused).
    pub active: bool,
    /// Abort armed: the border turns `frame_alert` (replaces `render.rs:1183-1187`).
    pub armed: bool,
    /// Draft line count, so the border can mark the lines the rect hides.
    pub draft_lines: usize,
    /// The draft text, all lines (`self.textarea.lines().join("\n")`): feeds the
    /// command chip, the ghost suggestion row and the character half of the draft
    /// indicator.
    pub buffer: Option<&'a str>,
    /// Position in the input history while browsing it (§5).
    pub history: Option<HistoryPos>,
    /// The `TextArea` widget itself, so the frame owns the body (caret, draft
    /// text) instead of a second pass painting its own block into the same rect.
    pub textarea: Option<&'a ratatui_textarea::TextArea<'static>>,
}

/// The part of the composer state the frame draws that the view model cannot
/// express yet — see the module docs ("needs plumb for t-009b").
#[derive(Debug, Clone, Copy, Default)]
pub struct ComposerState<'a> {
    /// The draft text, all lines. `None` while the buffer is not plumbed.
    pub buffer: Option<&'a str>,
    /// Position in the input history while browsing it; `None` while the
    /// operator is editing their own draft.
    pub history: Option<HistoryPos>,
}

/// Where the composer is standing in the input history (`Ctrl+Up` / `Ctrl+Down`,
/// spec §5). `entry` is 1-based so the chip reads `history 3/12`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryPos {
    /// The entry currently shown, 1-based.
    pub entry: usize,
    /// Entries in the history list.
    pub len: usize,
}

/// A command of spec §6 — the one table the composer's autocomplete, the help
/// overlay's command list and the TUI's command dispatch all read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    /// The spelling exactly as the operator types it: `/`-prefixed, except for
    /// the two accepted vim spellings of the quit family.
    pub name: &'static str,
    /// Argument hint shown behind the name; `""` when the command takes none.
    pub args: &'static str,
    /// One-line effect, as the help table prints it.
    pub effect: &'static str,
}

/// The §6 command table in spec order — and the single source of the command
/// vocabulary: the composer autocompletes from it, `/help` and the `F1` overlay
/// print it, and `events::command_intent` resolves every row in it to an owner.
/// A row that no owner ran would advertise a command that does nothing, so the
/// advertised set and the handled set are the same set by construction, and the
/// argument column is the argument that owner really accepts.
///
/// Aliases are rows, because every spelling in that table is something the
/// operator may type. `/h` and `/f` are *not* rows: they are prefix matches
/// resolved by [`suggest`]. The two vim spellings are rows but are **not**
/// reachable through the completion, because the composer only switches into
/// command mode on a leading `/` ([`command_token`]): they are complete
/// spellings of the quit family that the confirm gate accepts, listed here so
/// `/help` says so out loud.
pub fn slash_commands() -> Vec<SlashCommand> {
    [
        ("/help", "", "print the key legend and this command table"),
        ("/thought", "", "toggle the thinking block (state chip)"),
        (
            "/follow",
            "on|off",
            "toggle transcript follow (Ctrl+F, LIVE / ↓ N new)",
        ),
        ("/plan", "on|off", "toggle the Execution Plan pane (Ctrl+P)"),
        ("/workers", "on|off", "toggle the Workers pane (Ctrl+A)"),
        (
            "/mode",
            "transcript|plan|workers|split",
            "set the view mode (F3-F5); bare form prints it",
        ),
        (
            "/error",
            "",
            "print the sticky last error and the log path (key e)",
        ),
        (
            "/cancel",
            "<worker key or task id>",
            "cancel one worker (confirm)",
        ),
        ("/cancel-all", "", "cancel every worker (confirm)"),
        ("/exit", "", "quit the session (confirm)"),
        ("/quit", "", "quit the session (confirm)"),
        ("/q", "", "quit the session (confirm)"),
        ("/abort", "", "quit the session (confirm)"),
        ("/stop", "", "quit the session (confirm)"),
        (":q", "", "quit the session (confirm), as /quit does"),
        (":q!", "", "quit the session (confirm), as /quit does"),
        (
            "/reset",
            "",
            "clear plan, session bundle, steering history and transcript (confirm)",
        ),
        ("/reset_plan", "", "same as /reset"),
        ("/reset-plan", "", "same as /reset"),
        ("/clear_plan", "", "same as /reset"),
        ("/clear-plan", "", "same as /reset"),
        ("/reset_execution_plan", "", "same as /reset"),
    ]
    .into_iter()
    .map(|(name, args, effect)| SlashCommand { name, args, effect })
    .collect()
}

/// The command token on line 1 of `buffer`, or `None` when the draft is not in
/// command mode (spec §6: recognised after trimming, on line 1 only).
///
/// Command mode is a leading `/`, and nothing else: the two vim spellings the
/// command gate accepts (`:q`, `:q!`) are complete spellings, so they are listed
/// by `/help` but never autocompleted and never chip the composer as a command.
pub fn command_token(buffer: &str) -> Option<&str> {
    let line = buffer.lines().next()?.trim_start();
    if !line.starts_with('/') {
        return None;
    }
    line.split_whitespace().next()
}

/// `haystack` starts with `needle`, ASCII-case-insensitively and on char
/// boundaries — a pasted non-ASCII draft must never split a grapheme.
fn has_prefix(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    for needle in needle.chars() {
        match chars.next() {
            Some(seen) if seen.eq_ignore_ascii_case(&needle) => {}
            _ => return false,
        }
    }
    true
}

/// Every command whose name starts with `token`, case-insensitively, in table
/// order (spec §6).
pub fn candidates(token: &str) -> Vec<SlashCommand> {
    slash_commands()
        .into_iter()
        .filter(|command| has_prefix(command.name, token))
        .collect()
}

/// The longest common prefix of two strings, on char boundaries.
fn shared_prefix<'a>(left: &'a str, right: &str) -> &'a str {
    let mut bytes = 0usize;
    for (l, r) in left.chars().zip(right.chars()) {
        if l != r {
            break;
        }
        bytes += l.len_utf8();
    }
    &left[..bytes]
}

/// The longest common prefix shared by every name in `names`.
fn common_prefix(names: &[&str]) -> String {
    names
        .iter()
        .copied()
        .reduce(|acc, name| shared_prefix(acc, name))
        .unwrap_or_default()
        .to_string()
}

/// The ghost completion for the command being typed (spec §6): a unique prefix
/// completes to that command, an ambiguous one completes only as far as the
/// longest common prefix of its candidates, and never further — guessing a
/// command the operator did not type is forbidden. `/` alone therefore yields
/// `None`: the common prefix of the whole table is just `/`.
///
/// `Tab` accepts the result by inserting the remainder after the caret; the key
/// binding lives in the key handler (t-011), which calls this pure function.
pub fn suggest(buffer: &str) -> Option<String> {
    let token = command_token(buffer)?;
    let matches = candidates(token);
    match matches.len() {
        0 => None,
        1 => (matches[0].name != token).then(|| matches[0].name.to_string()),
        _ => {
            let names: Vec<&str> = matches.iter().map(|command| command.name).collect();
            let prefix = common_prefix(&names);
            (prefix.chars().count() > token.chars().count()).then_some(prefix)
        }
    }
}

/// The single-line ghost suggestion row of §6: the completion plus its argument
/// hint and `(N more)` when the prefix is ambiguous, or the candidate count
/// when nothing can be completed without guessing. `None` when the draft is not
/// in command mode, so the row disappears instead of adding noise.
pub fn suggestion_row(buffer: &str) -> Option<String> {
    let token = command_token(buffer)?;
    let matches = candidates(token);
    if matches.is_empty() {
        return Some(format!("{token} is not a command"));
    }
    if matches.len() == 1 {
        let command = matches[0];
        return Some(if command.args.is_empty() {
            command.name.to_string()
        } else {
            format!("{} {}", command.name, command.args)
        });
    }
    match suggest(buffer) {
        Some(completion) => {
            let args = matches
                .iter()
                .find(|command| command.name == completion)
                .map_or("", |command| command.args);
            let more = matches.len() - 1;
            let args = if args.is_empty() {
                String::new()
            } else {
                format!(" {args}")
            };
            Some(format!("{completion}{args}  ({more} more)  Tab"))
        }
        // More than one candidate and nothing completable: the count is the
        // honest answer (spec §6: `/` alone shows the candidate count).
        None => Some(format!("{} commands match {token}", matches.len())),
    }
}

/// The command-mode chip, shown while line 1 of the draft is a slash command
/// (spec §6: the composer visibly switches into command mode).
pub fn command_chip(buffer: &str) -> Option<String> {
    command_token(buffer).map(|token| format!("command {token}"))
}

/// The history indicator, shown only while the composer is showing a history
/// entry rather than the draft (§5 `Ctrl+Up` / `Ctrl+Down`).
pub fn history_chip(position: Option<HistoryPos>) -> Option<String> {
    let position = position?;
    if position.len == 0 {
        return None;
    }
    Some(format!(
        "history {}/{}",
        position.entry.min(position.len),
        position.len
    ))
}

/// The character/line indicator: shown once the draft is longer than one line
/// or longer than [`LONG_DRAFT_CHARS`] characters, i.e. once its size is no
/// longer obvious at a glance. `chars` is `None` while the buffer text is not
/// plumbed into the view model, in which case only the line count is knowable.
pub fn draft_indicator(lines: usize, chars: Option<usize>) -> Option<String> {
    let long = chars.is_some_and(|chars| chars > LONG_DRAFT_CHARS);
    if lines <= 1 && !long {
        return None;
    }
    Some(match chars {
        Some(chars) => format!("lines {lines} · chars {chars}"),
        None => format!("lines {lines}"),
    })
}

/// Composer border colour (§7.3): armed is `frame_alert`, and the frame reads
/// as focused when it holds focus *or* when it is the typing target, because
/// §4.4 makes it the typing target in every mode and focus state.
fn border_style(data: &InputData<'_>) -> Style {
    if data.armed {
        theme::frame_alert()
    } else if data.focus || data.active {
        theme::frame_focus()
    } else {
        theme::frame_idle()
    }
}

/// Number of draft lines a frame of `height` rows can show (the frame spends
/// two rows on borders).
const fn visible_draft_lines(height: u16) -> usize {
    (height as usize).saturating_sub(2)
}

/// The composer frame: title with the §4.4 key hint and the `●` focus badge,
/// the ghost suggestion row top-right, the state chips on the bottom border.
///
/// Chip order is command mode → history position → draft length → `+n` overflow
/// (§2.5 puts the overflow marker on the border title), and the whole run is
/// clipped to the frame width by [`fit_spans`] (§7.4).
fn composer_frame(data: &InputData<'_>, state: &ComposerState<'_>, rect: Rect) -> Block<'static> {
    let title = format!("{} ({KEY_HINT})", FocusPane::Input.label());
    let mut frame = pane_block(&title, data.focus, border_style(data));
    if let Some(row) = state.buffer.and_then(suggestion_row) {
        frame = frame.title(
            Line::from(Span::styled(format!(" {row} "), theme::chip())).alignment(Alignment::Right),
        );
    }
    let mut chips: Vec<Span<'static>> = Vec::new();
    let buffer = state.buffer.unwrap_or("");
    if let Some(chip) = command_chip(buffer) {
        chips.push(Span::styled(format!(" {chip} "), theme::chip_active()));
    }
    if let Some(chip) = history_chip(state.history) {
        chips.push(Span::styled(format!(" {chip} "), theme::chip()));
    }
    let chars = state.buffer.map(|buffer| buffer.chars().count());
    if let Some(chip) = draft_indicator(data.draft_lines, chars) {
        chips.push(Span::styled(format!(" {chip} "), theme::text_dim()));
    }
    let hidden = data
        .draft_lines
        .saturating_sub(visible_draft_lines(rect.height));
    if hidden > 0 {
        chips.push(Span::styled(format!(" +{hidden} "), theme::chip_pending()));
    }
    if !chips.is_empty() {
        // The chips ride the bottom border row, which is 2 cells narrower than the
        // pane: fitted to the rect width, ratatui cut the last two cells and the
        // `…` that marks the cut was one of them.
        frame = frame.title_bottom(Line::from(fit_spans(chips, title_width(rect))));
    }
    frame
}

/// Draw the draft text first, then the bordered frame around it.
///
/// Since t-009b the pane owns the whole composer: the `TextArea` widget is rendered
/// into the frame's interior — caret, draft text, wrapping — and the border, title,
/// ghost suggestion and state chips are drawn over the border rows only. That is
/// what removes the double-draw, where a legacy pass set its own block on the same
/// `TextArea` and painted a second `Input (…)` title under this one.
pub fn render(frame: &mut Frame, rect: Rect, data: &InputData<'_>) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    draw_body(frame, rect, data);
    let state = ComposerState {
        buffer: data.buffer,
        history: data.history,
    };
    let block = composer_frame(data, &state, rect);
    frame.render_widget(block, rect);
}

/// The draft body: the `TextArea` widget inside the frame, with no block of its
/// own. The renderer hands the widget over already synchronised and block-free
/// (`TuiRenderer::prepare_textarea`), because a widget carrying a second border
/// would re-create the very overlap this pass removes.
fn draw_body(frame: &mut Frame, rect: Rect, data: &InputData<'_>) {
    let Some(textarea) = data.textarea else {
        return;
    };
    let inner = rect.inner(Margin::new(1, 1));
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    frame.render_widget(textarea, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    /// Draw `block` into a `width x height` test terminal and hand back the
    /// text of every row plus the style of every cell.
    fn draw_block(width: u16, height: u16, block: Block<'_>) -> (Vec<String>, Vec<Vec<Style>>) {
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| f.render_widget(block, Rect::new(0, 0, width, height)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        let styles = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].style()).collect())
            .collect();
        (rows, styles)
    }

    fn data(focus: bool, active: bool, armed: bool, draft_lines: usize) -> InputData<'static> {
        InputData {
            focus,
            active,
            armed,
            draft_lines,
            buffer: None,
            history: None,
            textarea: None,
        }
    }

    /// Draw the whole pane (body + frame) into a test terminal.
    fn draw_pane(width: u16, height: u16, data: &InputData<'_>) -> Vec<String> {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
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
    fn suggest_completes_a_unique_prefix() {
        assert_eq!(suggest("/h").as_deref(), Some("/help"));
        assert_eq!(suggest("/tho").as_deref(), Some("/thought"));
        assert_eq!(suggest("/mo").as_deref(), Some("/mode"));
        assert_eq!(suggest("/mod").as_deref(), Some("/mode"));
        assert_eq!(suggest("/fol").as_deref(), Some("/follow"));
        assert_eq!(suggest("/err").as_deref(), Some("/error"));
        // The pane and mode commands the TUI now runs are completable too.
        assert_eq!(suggest("/pl").as_deref(), Some("/plan"));
        assert_eq!(suggest("/wor").as_deref(), Some("/workers"));
        assert_eq!(suggest("/cancel-al").as_deref(), Some("/cancel-all"));
        // Command mode is decided by line 1, so a multi-line draft still gets
        // its completion (spec §6: single-line submissions only, line 1 wins).
        assert_eq!(suggest("/mo\nsecond line").as_deref(), Some("/mode"));
    }

    #[test]
    fn suggest_is_case_insensitive() {
        assert_eq!(suggest("/HE").as_deref(), Some("/help"));
        assert_eq!(suggest("/Reset_P").as_deref(), Some("/reset_plan"));
    }

    #[test]
    fn suggest_is_empty_for_buffers_that_are_not_commands() {
        assert_eq!(suggest(""), None);
        assert_eq!(suggest("\n"), None);
        assert_eq!(suggest("hello"), None);
        // Bare words are ordinary steering text (spec §5): no completion.
        assert_eq!(suggest("stop"), None);
        assert_eq!(suggest("cancel"), None);
        assert_eq!(suggest("exit"), None);
        // An unknown spelling gets nothing rather than a wrong guess.
        assert_eq!(suggest("/zz"), None);
        // A fully typed command has nothing left to add.
        assert_eq!(suggest("/exit"), None);
        assert_eq!(suggest("/plan"), None);
        assert_eq!(suggest("/mode"), None);
        assert_eq!(suggest("/cancel <id>").as_deref(), None);
    }

    #[test]
    fn suggest_never_guesses_past_the_longest_common_prefix() {
        // `/` matches the whole slash table; the common prefix is `/` itself.
        assert_eq!(suggest("/"), None);
        // /cancel, /cancel-all, /clear_plan, /clear-plan share only `/c`.
        assert_eq!(suggest("/c"), None);
        assert_eq!(suggest("/ca").as_deref(), Some("/cancel"));
        assert_eq!(suggest("/cle").as_deref(), Some("/clear"));
        // /q and /quit share `/q`, and the colon spellings are out of reach of a
        // `/` token, so they never widen the candidate set here.
        assert_eq!(suggest("/q"), None);
        // `/re` used to be ambiguous with `/retry`, which the table no longer
        // carries (no retry action exists to route it to): the four `/reset`
        // spellings now share `/reset`, which is honest to complete to.
        assert_eq!(suggest("/re").as_deref(), Some("/reset"));
        assert_eq!(suggest("/reset").as_deref(), None);
    }

    #[test]
    fn candidates_come_from_the_command_table() {
        // Command mode is a leading `/`, so the completion sees the 20 slash rows
        // and never the two vim spellings.
        assert_eq!(candidates("/").len(), 20);
        assert_eq!(
            candidates("/").len(),
            slash_commands()
                .iter()
                .filter(|command| command.name.starts_with('/'))
                .count(),
            "every slash row is autocompletable"
        );
        assert_eq!(candidates("/c").len(), 4);
        assert_eq!(candidates("/re").len(), 4);
        assert_eq!(candidates("/mode")[0].name, "/mode");
        assert!(candidates("/zz").is_empty());
        assert_eq!(slash_commands().len(), 22);
        // The vim spellings are rows, so `/help` prints them; `candidates` only
        // reaches them if asked directly, which `command_token` never does.
        assert_eq!(candidates(":q").len(), 2);
        assert_eq!(command_token(":q"), None);
        assert_eq!(command_token(":q!"), None);
    }

    #[test]
    fn command_token_only_when_line_one_starts_with_a_slash() {
        assert_eq!(command_token("/cancel t-004"), Some("/cancel"));
        assert_eq!(command_token("  /help"), Some("/help"));
        assert_eq!(command_token("see /help"), None);
        assert_eq!(command_token(""), None);
    }

    #[test]
    fn suggestion_row_reports_the_candidate_count_when_ambiguous() {
        assert_eq!(suggestion_row("/").as_deref(), Some("20 commands match /"));
        assert_eq!(suggestion_row("/c").as_deref(), Some("4 commands match /c"));
        assert_eq!(
            suggestion_row("/ca").as_deref(),
            Some("/cancel <worker key or task id>  (1 more)  Tab")
        );
        assert_eq!(
            suggestion_row("/mode").as_deref(),
            Some("/mode transcript|plan|workers|split")
        );
        // The argument column is the argument the command accepts, so the ghost
        // row and the handler agree.
        assert_eq!(suggestion_row("/pl").as_deref(), Some("/plan on|off"));
        assert_eq!(suggestion_row("/fol").as_deref(), Some("/follow on|off"));
        assert_eq!(suggestion_row("/wor").as_deref(), Some("/workers on|off"));
        assert_eq!(suggestion_row("/err").as_deref(), Some("/error"));
        assert_eq!(
            suggestion_row("/zz").as_deref(),
            Some("/zz is not a command")
        );
        assert_eq!(suggestion_row("plain text"), None);
    }

    #[test]
    fn command_chip_marks_command_mode() {
        assert_eq!(command_chip("/ca").as_deref(), Some("command /ca"));
        assert_eq!(command_chip("/").as_deref(), Some("command /"));
        assert_eq!(command_chip("text"), None);
    }

    #[test]
    fn history_chip_only_while_browsing() {
        assert_eq!(
            history_chip(Some(HistoryPos { entry: 3, len: 12 })).as_deref(),
            Some("history 3/12")
        );
        // An out-of-range entry is clamped, an empty list shows nothing.
        assert_eq!(
            history_chip(Some(HistoryPos { entry: 9, len: 4 })).as_deref(),
            Some("history 4/4")
        );
        assert_eq!(history_chip(Some(HistoryPos { entry: 0, len: 0 })), None);
        assert_eq!(history_chip(None), None);
    }

    #[test]
    fn draft_indicator_appears_only_for_a_long_draft() {
        assert_eq!(draft_indicator(1, None), None);
        assert_eq!(draft_indicator(1, Some(10)), None);
        assert_eq!(draft_indicator(2, None).as_deref(), Some("lines 2"));
        assert_eq!(
            draft_indicator(1, Some(LONG_DRAFT_CHARS + 1)).as_deref(),
            Some("lines 1 · chars 81")
        );
        assert_eq!(
            draft_indicator(4, Some(120)).as_deref(),
            Some("lines 4 · chars 120")
        );
    }

    #[test]
    fn border_colour_differs_between_focused_and_unfocused() {
        let focused = draw_block(
            40,
            4,
            composer_frame(
                &data(true, true, false, 1),
                &ComposerState::default(),
                Rect::new(0, 0, 40, 4),
            ),
        );
        let unfocused = draw_block(
            40,
            4,
            composer_frame(
                &data(false, false, false, 1),
                &ComposerState::default(),
                Rect::new(0, 0, 40, 4),
            ),
        );
        assert_eq!(focused.1[1][0].fg, Some(Color::Cyan));
        assert_eq!(unfocused.1[1][0].fg, Some(Color::DarkGray));
        assert_ne!(focused.1[1][0], unfocused.1[1][0]);
    }

    #[test]
    fn border_colour_marks_typing_target_and_armed_abort() {
        // §4.4: the composer is the typing target even when another pane holds
        // focus, so its frame never reads as unfocused.
        let typing = draw_block(
            40,
            4,
            composer_frame(
                &data(false, true, false, 1),
                &ComposerState::default(),
                Rect::new(0, 0, 40, 4),
            ),
        );
        assert_eq!(typing.1[1][0].fg, Some(Color::Cyan));
        // Armed abort outranks both (§7.3, replaces render.rs:1183-1187).
        let armed = draw_block(
            40,
            4,
            composer_frame(
                &data(true, true, true, 1),
                &ComposerState::default(),
                Rect::new(0, 0, 40, 4),
            ),
        );
        assert_eq!(armed.1[1][0].fg, Some(Color::Red));
    }

    #[test]
    fn title_carries_the_composer_keys_and_focus_badge() {
        let (rows, _) = draw_block(
            80,
            4,
            composer_frame(
                &data(true, true, false, 1),
                &ComposerState::default(),
                Rect::new(0, 0, 80, 4),
            ),
        );
        let title = &rows[0];
        assert!(title.contains("Input"), "title: {title}");
        assert!(title.contains("Enter send"), "title: {title}");
        assert!(title.contains("Ctrl+Z undo"), "title: {title}");
        assert!(title.contains(theme::FOCUS_BADGE), "title: {title}");
    }

    #[test]
    fn composer_title_never_advertises_letter_mnemonics() {
        // §4.4: printable keys always reach the composer, so nothing on the
        // composer frame may imply that `x` / `e` / `?` are shortcuts here.
        for forbidden in ["x cancel", "e last error", "? for keys", " y ", " n "] {
            assert!(
                !KEY_HINT.contains(forbidden),
                "key hint advertises {forbidden:?}"
            );
        }
        let (rows, _) = draw_block(
            100,
            5,
            composer_frame(
                &data(true, true, false, 1),
                &ComposerState::default(),
                Rect::new(0, 0, 100, 5),
            ),
        );
        let frame: String = rows.concat();
        for forbidden in ["x cancel", "e last error", "? for keys"] {
            assert!(!frame.contains(forbidden), "frame advertises {forbidden:?}");
        }
    }

    #[test]
    fn overflow_marker_marks_the_hidden_draft_lines() {
        // A 4-row frame shows 2 draft lines; a 6-line draft hides 4.
        let (rows, _) = draw_block(
            60,
            4,
            composer_frame(
                &data(true, true, false, 6),
                &ComposerState::default(),
                Rect::new(0, 0, 60, 4),
            ),
        );
        assert!(rows[3].contains("+4"), "bottom row: {}", rows[3]);
        assert!(rows[3].contains("lines 6"), "bottom row: {}", rows[3]);
        // A draft that fits gets no marker and no indicator.
        let (rows, _) = draw_block(
            60,
            4,
            composer_frame(
                &data(true, true, false, 2),
                &ComposerState::default(),
                Rect::new(0, 0, 60, 4),
            ),
        );
        assert!(!rows[3].contains('+'), "bottom row: {}", rows[3]);
    }

    #[test]
    fn command_mode_draws_chip_and_ghost_suggestion_row() {
        let state = ComposerState {
            buffer: Some("/ca"),
            history: Some(HistoryPos { entry: 2, len: 5 }),
        };
        let (rows, _) = draw_block(
            100,
            4,
            composer_frame(&data(true, true, false, 1), &state, Rect::new(0, 0, 100, 4)),
        );
        let top = &rows[0];
        let bottom = &rows[3];
        assert!(top.contains("/cancel"), "top row: {top}");
        assert!(top.contains("(1 more)"), "top row: {top}");
        assert!(top.contains("Tab"), "top row: {top}");
        assert!(bottom.contains("command /ca"), "bottom row: {bottom}");
        assert!(bottom.contains("history 2/5"), "bottom row: {bottom}");
    }

    #[test]
    fn character_indicator_appears_once_the_buffer_is_known() {
        let long = "x".repeat(LONG_DRAFT_CHARS + 5);
        let state = ComposerState {
            buffer: Some(&long),
            history: None,
        };
        let (rows, _) = draw_block(
            100,
            3,
            composer_frame(&data(true, true, false, 1), &state, Rect::new(0, 0, 100, 3)),
        );
        assert!(
            rows[2].contains(&format!("chars {}", LONG_DRAFT_CHARS + 5)),
            "bottom row: {}",
            rows[2]
        );
    }

    #[test]
    fn the_draft_body_is_drawn_inside_the_frame_it_owns() {
        let textarea = ratatui_textarea::TextArea::new(vec![String::from("wireframe the header")]);
        let buffer = textarea.lines().join("\n");
        let data = InputData {
            focus: true,
            active: true,
            armed: false,
            draft_lines: 1,
            buffer: Some(&buffer),
            history: Some(HistoryPos { entry: 3, len: 9 }),
            textarea: Some(&textarea),
        };
        let seen = draw_pane(40, 3, &data);
        assert!(seen[1].contains("wireframe the header"), "{seen:?}");
        assert!(seen[2].contains("history 3/9"), "{seen:?}");
        assert!(seen[0].contains("Input ("), "{seen:?}");
    }

    #[test]
    fn render_entry_point_draws_the_frame() {
        let backend = TestBackend::new(60, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render(f, area, &data(true, true, false, 3));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "┌");
        assert_eq!(buffer[(0, 1)].style().fg, Some(Color::Cyan));
        let row: String = (0..60)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        assert!(row.contains("Input"), "top row: {row}");
    }

    #[test]
    fn visible_draft_lines_accounts_for_the_borders() {
        assert_eq!(visible_draft_lines(3), 1);
        assert_eq!(visible_draft_lines(9), 7);
        assert_eq!(visible_draft_lines(1), 0);
    }
}
