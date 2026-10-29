//! Formatting, text parsing, terminal math, and word wrapping utilities for TUI.

use crate::ui::tui::theme;
use ansi_to_tui::IntoText;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Choose the message-level style by prefix matching.
/// Returns `(style, has_special_style)`.
///
/// This is the transcript **role palette** (§B.1): it used to name raw
/// `Color::` values here, which put a second, competing copy of the palette
/// outside `theme.rs`. Every slot is now a role/text token, so a hue moves in
/// one file only, and the body branch stays fg-less on purpose (§F2: forcing
/// `White` is white-on-white on a light profile).
pub fn message_style(msg: &str) -> (Style, bool) {
    let first = msg.lines().next().unwrap_or("");
    if first.starts_with("--- Turn") || first.starts_with("=== Turn") {
        (theme::role_turn(), true)
    } else if first.starts_with("[Status] ") || first.starts_with("[CLI] ") {
        (theme::role_status(), true)
    } else if first.starts_with("System Error: ") {
        (theme::state_failed(), true)
    } else if first.starts_with("User: ") || first.starts_with("User (Steer): ") {
        (theme::role_user(), true)
    } else if first.starts_with("Marmennill: ") || first.starts_with("[Steer Arbitrator]") {
        (theme::role_manager(), true)
    } else if first.starts_with('[') && first.ends_with("completed.") {
        (theme::state_done(), true)
    } else if first.starts_with('[')
        && (first.ends_with("failed.")
            || first.ends_with("failed]")
            || first.contains(" failed:")
            || first.contains(" failed]"))
    {
        (theme::state_failed(), true)
    } else {
        // Orchestrator / Model content: body text inherits the operator's own
        // terminal foreground (`text_primary`, research §F2).
        (theme::text_primary(), false)
    }
}

/// The reasoning tier: the palette's [`theme::text_thought`] slot **plus a
/// weight step** (§F7). Italics alone are not a signal — a terminal that drops
/// `ITALIC` collapsed the tier into [`theme::text_muted`] chrome, which is how
/// reasoning silently turned into gutter text. `DIM` is the weight counterpart
/// of the `BOLD` the role tokens already carry, so the tier stays readable
/// without italics, stays distinct from `role_turn` (`DarkGray` + `BOLD`), and
/// borrows no hue. The three thought sites (message lines, the streaming
/// buffer, the transcript region) share this one helper so the tier cannot drift
/// into three different shapes again.
pub(crate) fn thought_style() -> Style {
    theme::text_thought().add_modifier(Modifier::DIM)
}

/// Segment of a rendered line representing either reasoning thought or visible content.
#[derive(Debug, PartialEq, Eq)]
pub enum LineSegment<'a> {
    Thought(&'a str),
    Content(&'a str),
}

/// Parse segments of a line according to the running `in_think` state, tracking transitions across `<think>` and `</think>` tags.
pub fn parse_line_segments<'a>(line: &'a str, in_think: &mut bool) -> Vec<LineSegment<'a>> {
    if line.is_empty() {
        return if *in_think {
            vec![LineSegment::Thought("")]
        } else {
            vec![LineSegment::Content("")]
        };
    }
    let mut segments = Vec::new();
    let mut cursor = 0;
    while cursor < line.len() {
        if *in_think {
            let close_tag = [("</think>", 8), ("</thought>", 10)]
                .iter()
                .filter_map(|(tag, len)| line[cursor..].find(tag).map(|idx| (cursor + idx, *len)))
                .min_by_key(|(idx, _)| *idx);

            if let Some((idx, len)) = close_tag {
                let thought_part = &line[cursor..idx];
                if !thought_part.is_empty() {
                    segments.push(LineSegment::Thought(thought_part));
                }
                *in_think = false;
                cursor = idx + len;
            } else {
                let thought_part = &line[cursor..];
                if !thought_part.is_empty() {
                    segments.push(LineSegment::Thought(thought_part));
                }
                break;
            }
        } else {
            let open_tag = [("<think>", 7), ("<thought>", 9)]
                .iter()
                .filter_map(|(tag, len)| line[cursor..].find(tag).map(|idx| (cursor + idx, *len)))
                .min_by_key(|(idx, _)| *idx);

            if let Some((idx, len)) = open_tag {
                let content_part = &line[cursor..idx];
                if !content_part.is_empty() {
                    segments.push(LineSegment::Content(content_part));
                }
                *in_think = true;
                cursor = idx + len;
            } else {
                let content_part = &line[cursor..];
                if !content_part.is_empty() {
                    segments.push(LineSegment::Content(content_part));
                }
                break;
            }
        }
    }
    segments
}

pub fn strip_think_tags(line: &str) -> String {
    line.replace("<think>", "")
        .replace("</think>", "")
        .replace("<thought>", "")
        .replace("</thought>", "")
}

/// Byte that can act as the operand a superscript/subscript attaches to.
/// Byte-wise checks below are UTF-8 safe: continuation bytes are >= 0x80 and
/// never match these ASCII values.
fn is_math_operand_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b')' | b'}' | b']')
}

/// True if `s` contains a *complete* LaTeX command: a backslash followed by a
/// run of at least two ASCII letters terminated by a non-letter (`\cdot`,
/// `\frac`, `\alpha`). A bare escape (`\n` in code, a path separator in
/// `C:\Users\me`, the `\|` pipe escape) is deliberately **not** evidence.
fn has_complete_latex_command(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
            end += 1;
        }
        if end - (i + 1) >= 2 {
            return true;
        }
        i = end.max(i + 1);
    }
    false
}

/// True if `s` contains a properly delimited superscript/subscript, i.e. a `^`
/// or `_` that
/// 1. hangs off an operand (`x^2`, `2^3`, `(a+b)^2`, `x_1`), and
/// 2. is followed either by a closed brace group (`x^{ij}`) or by exactly one
///    alphanumeric script character terminated by a non-word character
///    (`x^2 `, trailing `x_1`).
///
/// The single-character requirement is what keeps snake_case identifiers such as
/// `subagent_name` or `handle_input` out of the math path: their `_` is followed
/// by a word run, not by a delimited script body.
fn has_delimited_script(s: &str) -> bool {
    let bytes = s.as_bytes();
    for (i, &byte) in bytes.iter().enumerate() {
        if byte != b'^' && byte != b'_' {
            continue;
        }
        if i == 0 || !is_math_operand_byte(bytes[i - 1]) {
            continue; // nothing for the script to attach to
        }
        let body = &s[i + 1..];
        let Some(first) = body.chars().next() else {
            continue;
        };
        if first == '{' {
            if body[1..].contains('}') {
                return true;
            }
            continue;
        }
        if first.is_ascii_alphanumeric() {
            // `first` is ASCII here, so a one-byte offset is a char boundary.
            match body[1..].chars().next() {
                // Script body ends the span, or is followed by a delimiter.
                None => return true,
                Some(next) if !next.is_ascii_alphanumeric() && next != '_' && next != '{' => {
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}

/// True if `s` contains an explicit relational or arithmetic operator token.
fn has_math_operator(s: &str) -> bool {
    // Relational/typographic operators are unambiguous math evidence.
    if s.chars().any(|c| {
        matches!(
            c,
            '=' | '≤'
                | '≥'
                | '≈'
                | '≠'
                | '→'
                | '←'
                | '×'
                | '÷'
                | '·'
                | '±'
                | '∓'
                | '∑'
                | '∏'
                | '∫'
                | '√'
                | '∞'
        )
    }) {
        return true;
    }
    // ASCII arithmetic only counts when it sits directly between two operands,
    // so prose dashes and money ranges ("cost - estimate", "100 - ") stay prose.
    let bytes = s.as_bytes();
    bytes.iter().enumerate().any(|(i, &byte)| {
        matches!(byte, b'+' | b'-' | b'*' | b'/')
            && i > 0
            && i + 1 < bytes.len()
            && is_math_operand_byte(bytes[i - 1])
            && is_math_operand_byte(bytes[i + 1])
    })
}

/// Decide whether the inside of a `$...$` span is genuine math.
///
/// Dollar signs must never be deleted from currency, shell variables or ordinary
/// prose, so a span only qualifies as math when it carries positive evidence: a
/// complete LaTeX command, a delimited superscript/subscript, or an explicit
/// operator token. Pure prose carries none of these.
fn looks_like_math_span(inner: &str) -> bool {
    has_complete_latex_command(inner) || has_delimited_script(inner) || has_math_operator(inner)
}

/// True when offset `at` in `s` is where a LaTeX command *ends*: end of string, or
/// a non-alphanumeric delimiter (space, punctuation, `$`, `{`). A letter or digit
/// means the table entry is only the prefix of a longer word (`\todo`, `\get`,
/// `\internal`, `\product`), which must be emitted verbatim.
fn is_command_end(s: &str, at: usize) -> bool {
    match s.as_bytes().get(at) {
        None => true,
        Some(&byte) => !byte.is_ascii_alphanumeric(),
    }
}

/// True when offset `at` in `s` is where a bare script body ends (`x^2 `, a
/// trailing `x_1`). A letter, digit, `_` or `{` means the body keeps going
/// (`2^10`, `x_y_z`), so the caret/underscore is ordinary text, not a script.
fn is_script_body_end(s: &str, at: usize) -> bool {
    match s.as_bytes().get(at) {
        None => true,
        Some(&byte) => !byte.is_ascii_alphanumeric() && byte != b'_' && byte != b'{',
    }
}

/// Whether the text written so far ends with something a `^` or `_` can attach
/// to. The rendered absolute-value bar counts: by the time `C\|^2` reaches the
/// caret branch, `\|` has already been rewritten to `|`.
fn ends_with_math_operand(out: &str) -> bool {
    match out.chars().next_back() {
        Some(ch) => ch.is_alphanumeric() || matches!(ch, ')' | '}' | ']' | '|'),
        None => false,
    }
}

/// Whether `out` ends with a *standalone* operand symbol: a lone letter or digit,
/// or a closing group such as `)`. The tail of a longer word never qualifies, so
/// `snake_case` words (`task_id`, `read_config_2`, `id_1`) stay out of the
/// subscript table while `x_1`, `\sigma_1` and `(a+b)_{2}` still convert.
fn ends_with_lone_math_operand(out: &str) -> bool {
    let Some(last) = out.chars().next_back() else {
        return false;
    };
    if matches!(last, ')' | '}' | ']' | '|') {
        return true;
    }
    if !last.is_alphanumeric() {
        return false;
    }
    let before = &out[..out.len() - last.len_utf8()];
    !before
        .chars()
        .next_back()
        .is_some_and(|prev| prev.is_alphanumeric())
}

/// Length of the backtick run that starts at byte `at` (0 when `at` is not a
/// backtick). Backticks are ASCII, so byte indexing stays on char boundaries.
fn backtick_run_at(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    if bytes.get(at) != Some(&b'`') {
        return 0;
    }
    let mut end = at;
    while end < bytes.len() && bytes[end] == b'`' {
        end += 1;
    }
    end - at
}

/// Split `text` into `(is_code_span, chunk)` pieces, alternating between prose
/// (safe to math-convert) and inline code spans (must stay verbatim, backticks
/// included).
///
/// A span is a run of N backticks closed by a run of exactly N backticks *on the
/// same line*, so both `` `task_id` `` and `` ``task_id`` `` are protected while
/// markdown bold (`**word**`) is never mistaken for code because it uses
/// asterisks, not backticks. An unterminated span stays verbatim up to the end of
/// its line: every character survives and the rest of the line is never
/// swallowed by the opening backtick.
fn split_on_inline_code_spans(text: &str) -> Vec<(bool, &str)> {
    let bytes = text.as_bytes();
    let mut parts: Vec<(bool, &str)> = Vec::new();
    let mut plain_start = 0;
    let mut i = 0;

    while i < bytes.len() {
        let open_len = backtick_run_at(text, i);
        if open_len == 0 {
            i += 1;
            continue;
        }
        let line_end = text[i..].find('\n').map_or(bytes.len(), |rel| i + rel);
        let marker = &text[i..i + open_len];

        // Look for a closing run of the *same* length on this line only.
        let mut search = i + open_len;
        let mut span_end = None;
        while let Some(rel) = text[search..line_end].find(marker) {
            let close = search + rel;
            let run = backtick_run_at(text, close);
            if run == open_len {
                span_end = Some(close + run);
                break;
            }
            search = close + run;
        }
        let span_end = span_end.unwrap_or(line_end);

        if i > plain_start {
            parts.push((false, &text[plain_start..i]));
        }
        parts.push((true, &text[i..span_end]));
        i = span_end;
        plain_start = i;
    }

    if plain_start < bytes.len() {
        parts.push((false, &text[plain_start..]));
    }
    parts
}

/// If `line` is a fenced-code block marker — ``` or ~~~ with an optional info
/// string such as `rust` — return the length of the fence run. Anything carrying
/// other trailing text (`x^2`, `~~~ broken`) is not a marker.
fn fence_marker_len(line: &str) -> Option<usize> {
    let trimmed = line.trim_start_matches(' ');
    let bytes = trimmed.as_bytes();
    let fence = *bytes.first()?;
    if fence != b'`' && fence != b'~' {
        return None;
    }
    let run = bytes.iter().take_while(|&&b| b == fence).count();
    if run < 3 {
        return None;
    }
    let rest = trimmed[run..].trim();
    let info_string_ok = rest
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (rest.is_empty() || info_string_ok).then_some(run)
}

/// True when one single line opens *and* closes a fence, e.g. ```` ```x_1 = 2``` ````.
fn is_single_line_fenced_code(line: &str) -> bool {
    let bytes = line.as_bytes();
    let fence = match bytes.first() {
        Some(&b'`') | Some(&b'~') => bytes[0],
        _ => return false,
    };
    let open = bytes.iter().take_while(|&&b| b == fence).count();
    if open < 3 {
        return false;
    }
    let body = line[open..].trim_end();
    let close = body.bytes().rev().take_while(|&b| b == fence).count();
    close >= open && close < body.len()
}

/// Stateless, line-local fenced-code guard.
///
/// The message-wide fence exemption (see `render_message_lines`, which detours to
/// `render_markdown_lines` when the message contains ```` ``` ````) is skipped
/// entirely for `has_special_style` messages (`User: `, `[Status] `,
/// `Marmennill: `), so those lines still reach `format_terminal_math`. This guard
/// therefore handles what a single call can recognise on its own: a fence marker
/// line (together with any code content handed over with it) and a fence that
/// opens and closes within one line.
fn looks_like_fenced_code(text: &str) -> bool {
    let Some(first_line) = text.lines().next() else {
        return false;
    };
    fence_marker_len(first_line).is_some() || is_single_line_fenced_code(first_line)
}

/// Format LaTeX math expressions ($$...$$, $...$, and common math symbols)
/// into clean, readable terminal Unicode text.
///
/// Code is protected before any math heuristic runs, so every production call
/// site (thinking/tool/CLI lines here and `render.rs`) inherits the exemption:
/// * inline code spans delimited by backticks are emitted byte-for-byte, and
/// * self-contained fenced code (a fence marker line, or a fence that opens and
///   closes on one line) is emitted untouched.
///
/// Known limitation: `format_terminal_math` is called once per line and is kept
/// deliberately stateless — the public signature takes no `in_code_block` flag,
/// so a *multi-line* fenced block cannot be tracked across calls. A line inside
/// such a block (e.g. `x_1 = 2`) is still math-scanned; only the fence markers
/// themselves and single-line fences are guaranteed verbatim. Extending this
/// would require threading fence state through the seven call sites.
pub fn format_terminal_math(text: &str) -> String {
    // Fast path: if there are no math delimiters or symbols, return unmodified text immediately.
    if !text.contains('$') && !text.contains('\\') && !text.contains('^') && !text.contains('_') {
        return text.to_string();
    }
    // Fenced code never becomes math.
    if looks_like_fenced_code(text) {
        return text.to_string();
    }
    // Everything outside inline code spans keeps the math treatment.
    let mut out = String::with_capacity(text.len());
    for (is_code_span, chunk) in split_on_inline_code_spans(text) {
        if is_code_span {
            out.push_str(chunk);
        } else {
            out.push_str(&format_math_outside_code(chunk));
        }
    }
    out
}

/// The `$`-driven math conversion, applied to a slice known to hold no code.
fn format_math_outside_code(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut i = 0;
    let bytes = text.as_bytes();

    while i < text.len() {
        if bytes[i] == b'$' {
            if i + 1 < text.len() && bytes[i + 1] == b'$' {
                // Display math: $$...$$
                let start = i + 2;
                if let Some(end_rel) = text[start..].find("$$") {
                    let end = start + end_rel;
                    let inner = &text[start..end];
                    let formatted = format_math_expr(inner.trim());
                    result.push_str("  ");
                    result.push_str(&formatted);
                    i = end + 2;
                } else {
                    let inner = &text[start..];
                    let formatted = format_math_expr(inner.trim());
                    result.push_str("  ");
                    result.push_str(&formatted);
                    break;
                }
            } else {
                // Inline math: $...$
                let start = i + 1;
                if let Some(end_rel) = text[start..].find('$') {
                    let end = start + end_rel;
                    let inner = &text[start..end];
                    let is_plain_number = !inner.trim().is_empty()
                        && inner
                            .chars()
                            .all(|c| c.is_ascii_digit() || c == '.' || c == ',');
                    // Only rewrite spans that actually look like math; currency, shell
                    // variables and prose keep their dollar signs.
                    if !is_plain_number && looks_like_math_span(inner) {
                        let formatted = format_math_expr(inner);
                        result.push_str(&formatted);
                        i = end + 1;
                        continue;
                    }
                    // Currency or numbers: keep verbatim $inner$
                    result.push('$');
                    result.push_str(inner);
                    result.push('$');
                    i = end + 1;
                } else {
                    result.push('$');
                    i += 1;
                }
            }
        } else {
            let next_dollar = text[i..].find('$').map(|rel| i + rel).unwrap_or(text.len());
            let chunk = &text[i..next_dollar];
            // Only hand the chunk to the converter when it really contains math the
            // converter can rewrite: a complete LaTeX command or a delimited
            // `^`/`_` construct. A stray `\`, `^` or `_` in prose, paths or
            // snake_case identifiers must pass through untouched.
            if has_complete_latex_command(chunk) || has_delimited_script(chunk) {
                result.push_str(&format_math_expr(chunk));
            } else {
                result.push_str(chunk);
            }
            i = next_dollar;
        }
    }

    result
}

pub fn format_math_expr(expr: &str) -> String {
    if !expr.contains('\\') && !expr.contains('^') && !expr.contains('_') && !expr.contains('|') {
        return expr.to_string();
    }

    let mut out = String::with_capacity(expr.len());
    let mut cursor = 0;

    while cursor < expr.len() {
        let rest = &expr[cursor..];
        let first_byte = rest.as_bytes()[0];

        if first_byte == b'\\' {
            if rest.starts_with(r"\|") {
                out.push('|');
                cursor += 2;
                continue;
            }
            if let Some(stripped) = rest.strip_prefix(r"\text{")
                && let Some(end_rel) = stripped.find('}')
            {
                out.push_str(&stripped[..end_rel]);
                cursor += 6 + end_rel + 1;
                continue;
            }
            if let Some(stripped) = rest.strip_prefix(r"\sqrt{")
                && let Some(end_rel) = stripped.find('}')
            {
                let inner = &stripped[..end_rel];
                out.push('√');
                out.push('(');
                out.push_str(&format_math_expr(inner));
                out.push(')');
                cursor += 6 + end_rel + 1;
                continue;
            }
            if let Some(stripped) = rest.strip_prefix(r"\frac{")
                && let Some(mid_rel) = stripped.find("}{")
                && let Some(end_rel) = stripped[mid_rel + 2..].find('}')
            {
                let num = &stripped[..mid_rel];
                let den = &stripped[mid_rel + 2..mid_rel + 2 + end_rel];
                out.push('(');
                out.push_str(&format_math_expr(num));
                out.push_str(")/(");
                out.push_str(&format_math_expr(den));
                out.push(')');
                cursor += 6 + mid_rel + 2 + end_rel + 1;
                continue;
            }

            const SYMBOLS: &[(&str, &str)] = &[
                (r"\rightarrow", "→"),
                (r"\leftarrow", "←"),
                (r"\approx", "≈"),
                (r"\cdots", "…"),
                (r"\qquad", "    "),
                (r"\lambda", "λ"),
                (r"\Delta", "Δ"),
                (r"\infty", "∞"),
                (r"\alpha", "α"),
                (r"\gamma", "γ"),
                (r"\theta", "θ"),
                (r"\sigma", "σ"),
                (r"\omega", "ω"),
                (r"\times", "×"),
                (r"\cdot", "·"),
                (r"\dots", "…"),
                (r"\quad", "  "),
                (r"\beta", "β"),
                (r"\prod", "∏"),
                (r"\leq", "≤"),
                (r"\geq", "≥"),
                (r"\neq", "≠"),
                (r"\sum", "∑"),
                (r"\int", "∫"),
                (r"\pm", "±"),
                (r"\mp", "∓"),
                (r"\le", "≤"),
                (r"\ge", "≥"),
                (r"\ne", "≠"),
                (r"\to", "→"),
                (r"\mu", "μ"),
                (r"\pi", "π"),
            ];

            let mut matched = false;
            for &(cmd, repl) in SYMBOLS {
                // Only a *complete* command may convert: the byte right after it
                // must be a delimiter. Table order keeps the longer spelling first
                // (`\leftarrow` before `\le`, `\cdots` before `\cdot`, `\prod`
                // before `\pi`), so an unfinished match such as `\todo`, `\get`,
                // `\internal` or `\product` falls through and the backslash is
                // emitted verbatim.
                if rest.starts_with(cmd) && is_command_end(rest, cmd.len()) {
                    out.push_str(repl);
                    cursor += cmd.len();
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
        } else if first_byte == b'^' {
            const SUPERSCRIPTS: &[(&str, &str)] = &[
                ("^{+}", "⁺"),
                ("^{-}", "⁻"),
                ("^{0}", "⁰"),
                ("^{1}", "¹"),
                ("^{2}", "²"),
                ("^{3}", "³"),
                ("^{4}", "⁴"),
                ("^{5}", "⁵"),
                ("^{6}", "⁶"),
                ("^{7}", "⁷"),
                ("^{8}", "⁸"),
                ("^{9}", "⁹"),
                ("^{n}", "ⁿ"),
                ("^{t}", "ᵗ"),
                ("^{T}", "ᵀ"),
                ("^0", "⁰"),
                ("^1", "¹"),
                ("^2", "²"),
                ("^3", "³"),
                ("^4", "⁴"),
                ("^5", "⁵"),
                ("^6", "⁶"),
                ("^7", "⁷"),
                ("^8", "⁸"),
                ("^9", "⁹"),
                ("^n", "ⁿ"),
                ("^t", "ᵗ"),
                ("^T", "ᵀ"),
            ];
            let mut matched = false;
            // A caret is only a superscript when it hangs off an operand and its
            // body is delimited: either a closed brace group (every `^{…}` entry
            // carries its own `}`) or a single script character followed by a
            // delimiter. Anything else is emitted verbatim: `2^10` (body keeps
            // going) or a caret attached to nothing, as in `'TODO` and `[^0-9]`.
            if ends_with_math_operand(&out) {
                for &(pattern, repl) in SUPERSCRIPTS {
                    if rest.starts_with(pattern)
                        && (pattern.ends_with('}') || is_script_body_end(rest, pattern.len()))
                    {
                        out.push_str(repl);
                        cursor += pattern.len();
                        matched = true;
                        break;
                    }
                }
            }
            if matched {
                continue;
            }
        } else if first_byte == b'_' {
            const SUBSCRIPTS: &[(&str, &str)] = &[
                ("_{0}", "₀"),
                ("_{1}", "₁"),
                ("_{2}", "₂"),
                ("_{3}", "₃"),
                ("_{4}", "₄"),
                ("_{5}", "₅"),
                ("_{6}", "₆"),
                ("_{7}", "₇"),
                ("_{8}", "₈"),
                ("_{9}", "₉"),
                ("_{i}", "ᵢ"),
                ("_{n}", "ₙ"),
                ("_{x}", "ₓ"),
                ("_{y}", "ᵧ"),
                ("_{z}", "₂"),
                ("_0", "₀"),
                ("_1", "₁"),
                ("_2", "₂"),
                ("_3", "₃"),
                ("_4", "₄"),
                ("_5", "₅"),
                ("_6", "₆"),
                ("_7", "₇"),
                ("_8", "₈"),
                ("_9", "₉"),
                ("_i", "ᵢ"),
                ("_n", "ₙ"),
                ("_x", "ₓ"),
                ("_y", "ᵧ"),
            ];
            let mut matched = false;
            // Underscores are overwhelmingly identifier punctuation, so they are
            // held to the stricter rule: the script must attach to a *standalone*
            // operand (`x_1`, `\sigma_1`, `(a+b)_{2}`) and carry a delimited body.
            // The tail of a word does not qualify, which leaves `task_id`,
            // `subagent_name`, `handle_input`, `read_config_2`, `1_000_000`,
            // `_italic_` and `x_y_z` byte-for-byte untouched.
            if ends_with_lone_math_operand(&out) {
                for &(pattern, repl) in SUBSCRIPTS {
                    if rest.starts_with(pattern)
                        && (pattern.ends_with('}') || is_script_body_end(rest, pattern.len()))
                    {
                        out.push_str(repl);
                        cursor += pattern.len();
                        matched = true;
                        break;
                    }
                }
            }
            if matched {
                continue;
            }
        }

        let ch = rest.chars().next().unwrap();
        out.push(ch);
        cursor += ch.len_utf8();
    }

    out
}

/// Render a single message into styled lines with markdown, thinking tags, and ANSI handling.
pub fn render_message_lines(msg: &str, show_thought: bool) -> Vec<Line<'static>> {
    let mut chat_lines = Vec::new();
    let (msg_style, has_special_style) = message_style(msg);
    if !has_special_style && msg.contains("```") {
        let (thought_opt, content) = extract_thought_and_content(msg);
        if show_thought && let Some(t) = thought_opt {
            for line in t.lines() {
                let cleaned = format_terminal_math(line.trim());
                if !cleaned.is_empty() {
                    chat_lines.push(Line::from(Span::styled(cleaned, thought_style())));
                }
            }
        }
        if !content.trim().is_empty() {
            chat_lines.extend(render_markdown_lines(&content));
        }
        return chat_lines;
    }

    let mut in_think = false;
    for raw_line in msg.lines() {
        let line = raw_line.replace('\t', "    ");
        let segments = parse_line_segments(&line, &mut in_think);
        for seg in segments {
            match seg {
                LineSegment::Thought(t) => {
                    if !show_thought {
                        continue;
                    }
                    let trimmed = t.trim();
                    if trimmed.is_empty() {
                        chat_lines.push(Line::from(""));
                        continue;
                    }
                    let cleaned = format_terminal_math(trimmed);
                    chat_lines.push(Line::from(Span::styled(cleaned, thought_style())));
                }
                LineSegment::Content(c) => {
                    let trimmed = c.trim();
                    if trimmed.is_empty() {
                        chat_lines.push(Line::from(""));
                        continue;
                    }
                    let cleaned = format_terminal_math(trimmed);
                    if has_special_style {
                        if cleaned.contains('\x1b') {
                            chat_lines.extend(parse_ansi_lines(&cleaned));
                        } else {
                            chat_lines.push(Line::from(Span::styled(cleaned, msg_style)));
                        }
                    } else if trimmed.starts_with("[Tool Call] ")
                        || trimmed.starts_with("[Tool Result] ")
                    {
                        if cleaned.contains('\x1b') {
                            chat_lines.extend(parse_ansi_lines(&cleaned));
                        } else {
                            chat_lines.push(Line::from(Span::styled(cleaned, theme::role_tool())));
                        }
                    } else if cleaned.contains('\x1b') {
                        chat_lines.extend(parse_ansi_lines(&cleaned));
                    } else {
                        chat_lines.push(Line::from(Span::styled(cleaned, theme::text_primary())));
                    }
                }
            }
        }
    }
    chat_lines
}

/// Count the total wrapped lines occupied by rendered `Line`s at `width`.
pub fn count_wrapped_rendered_lines(lines: &[Line<'_>], width: usize) -> usize {
    let mut total = 0;
    for line in lines {
        let line_len: usize = line.spans.iter().map(|s| s.content.len()).sum();
        if line_len == 0 {
            total += 1;
        } else if line.spans.len() == 1 {
            total += wrapped_lines(&line.spans[0].content, width).max(1);
        } else {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            total += wrapped_lines(&text, width).max(1);
        }
    }
    total
}

/// Grapheme-aware word-wrap line counter.
///
/// - `width == 0` → returns the raw line count (no wrapping).
/// - Tabs → 4 spaces; trailing spaces trimmed.
/// - Zero-width space `\u{200B}` is treated as whitespace; non-breaking space
///   `\u{00A0}` is **not** treated as whitespace.
/// - Word-wrap breaks when `line_width >= max_line_width` or when a pending
///   word would overflow.
/// - Each raw line contributes at least 1 wrapped line.
pub fn wrapped_lines(text: &str, width: usize) -> usize {
    if width == 0 {
        return text.lines().count();
    }

    let mut total_lines = 0;

    for raw_line in text.lines() {
        let line = raw_line.replace('\t', "    ");
        let line = line.trim_end_matches(' ');

        let mut line_symbols = Vec::new();
        for grapheme in line.graphemes(true) {
            let is_whitespace = grapheme == "\u{200B}"
                || (grapheme.chars().all(char::is_whitespace) && grapheme != "\u{00A0}");
            let symbol_width = grapheme.width() as u16;
            line_symbols.push((is_whitespace, symbol_width));
        }

        let mut wrapped_lines_count = 0;
        let mut pending_line_empty = true;
        let mut line_width = 0;

        let mut pending_word_width = 0;
        let mut pending_word_len = 0;

        let mut pending_whitespace_widths = std::collections::VecDeque::new();
        let mut whitespace_width = 0;

        let mut non_whitespace_previous = false;
        let max_line_width = width as u16;
        let trim = false;

        for (is_whitespace, symbol_width) in line_symbols {
            if symbol_width > max_line_width {
                continue;
            }

            let word_found = non_whitespace_previous && is_whitespace;
            let trimmed_overflow =
                pending_line_empty && trim && pending_word_width + symbol_width > max_line_width;
            let whitespace_overflow =
                pending_line_empty && trim && whitespace_width + symbol_width > max_line_width;
            let untrimmed_overflow = pending_line_empty
                && !trim
                && pending_word_width + whitespace_width + symbol_width > max_line_width;

            if word_found || trimmed_overflow || whitespace_overflow || untrimmed_overflow {
                if !pending_line_empty || !trim {
                    line_width += whitespace_width;
                }
                line_width += pending_word_width;
                pending_line_empty = false;

                pending_whitespace_widths.clear();
                whitespace_width = 0;
                pending_word_width = 0;
                pending_word_len = 0;
            }

            let line_full = line_width >= max_line_width;
            let pending_word_overflow = symbol_width > 0
                && line_width + whitespace_width + pending_word_width >= max_line_width;

            if line_full || pending_word_overflow {
                let mut remaining_width = max_line_width.saturating_sub(line_width);
                wrapped_lines_count += 1;
                line_width = 0;
                pending_line_empty = true;

                while let Some(&w) = pending_whitespace_widths.front() {
                    if w > remaining_width {
                        break;
                    }
                    whitespace_width -= w;
                    remaining_width -= w;
                    pending_whitespace_widths.pop_front();
                }

                if is_whitespace && pending_whitespace_widths.is_empty() {
                    continue;
                }
            }

            if is_whitespace {
                whitespace_width += symbol_width;
                pending_whitespace_widths.push_back(symbol_width);
            } else {
                pending_word_width += symbol_width;
                pending_word_len += 1;
            }
            non_whitespace_previous = !is_whitespace;
        }

        let mut final_pending_exists = false;
        if pending_line_empty && pending_word_len == 0 && !pending_whitespace_widths.is_empty() {
            wrapped_lines_count += 1;
            final_pending_exists = true;
        }
        if !final_pending_exists {
            let mut final_line_empty = pending_line_empty;
            if !pending_line_empty || (!trim && !pending_whitespace_widths.is_empty()) {
                final_line_empty = false;
            }
            if pending_word_len > 0 {
                final_line_empty = false;
            }
            if !final_line_empty {
                wrapped_lines_count += 1;
            }
        }

        if wrapped_lines_count == 0 {
            wrapped_lines_count = 1;
        }

        total_lines += wrapped_lines_count;
    }
    total_lines
}

/// Compute the visual (wrapped) line offset of the first uncompleted task checkbox (`- [ ]`)
/// in the execution plan text. Returns `None` if no uncompleted task is present.
///
/// "What counts as an uncompleted task line" is decided by
/// [`crate::plan_parse`] — the same rule the on-disk plan authority uses — and
/// only the wrapped-line arithmetic stays here.
pub fn visual_line_offset_of_first_pending(text: &str, width: usize) -> Option<usize> {
    let target = crate::plan_parse::first_unchecked_line_index(text)?;
    let mut visual_offset = 0;
    for (idx, raw_line) in text.lines().enumerate() {
        if idx == target {
            return Some(visual_offset);
        }
        visual_offset += wrapped_lines(raw_line, width).max(1);
    }
    None
}

/// Trims surrounding brackets, parentheses, and quotes from a task identifier.
pub fn clean_task_id(tid: &str) -> &str {
    crate::task_id::normalize_task_id_ref(tid)
}

/// Extracts the task ID for a checklist item line in an execution plan.
///
/// The grammar lives in [`crate::plan_parse::task_id_of_line`] so the plan
/// panel, the delegation guard and the on-disk check-off all agree. Recognized
/// formats include:
/// - `- [ ] [t-001] Description`
/// - `- [ ] [t-100a1] Description`
/// - `- [ ] **[t-001]** Description`
/// - `- [ ] t-001: Description`
/// - `- [ ] (t-002) Description`
/// - `1. [ ] (t-001) Description`
/// - `* [ ] `t-001` Description`
/// - `  - [X] (t-003) Description` (indented / blockquoted)
pub fn extract_plan_line_task_id(line: &str) -> Option<String> {
    crate::plan_parse::task_id_of_line(line)
}

/// Checks if an execution plan line matches a specific task ID with strict boundary semantics,
/// preventing partial substring collisions (e.g. `t-100` must not match `t-100a1`, and `t-1` must not match `t-100`).
pub fn line_matches_task_id(line: &str, task_id: &str) -> bool {
    crate::plan_parse::line_matches_task_id(line, task_id)
}

/// Compute the visual (wrapped) line offset of a specific task in the execution
/// plan text. Returns `None` if the task ID is not found.
pub fn visual_line_offset_of_task(text: &str, task_id: &str, width: usize) -> Option<usize> {
    let clean_tid = clean_task_id(task_id);
    if clean_tid.is_empty() {
        return None;
    }
    let mut visual_offset = 0;
    let mut candidate_offset = None;

    for raw_line in text.lines() {
        if line_matches_task_id(raw_line, clean_tid) {
            // If this line is a checklist item of the shared grammar, it is
            // definitively the task line (otherwise keep it as a weak candidate).
            if crate::plan_parse::parse_task_line(raw_line).is_some() {
                return Some(visual_offset);
            }
            if candidate_offset.is_none() {
                candidate_offset = Some(visual_offset);
            }
        }
        visual_offset += wrapped_lines(raw_line, width).max(1);
    }
    candidate_offset
}

/// Extract complete sentences from a streaming buffer.
///
/// Splits on newlines or on sentence-ending punctuation (`.`, `!`, `?`)
/// followed by whitespace. Trailing punctuation and delimiters are preserved
/// in the extracted chunk so paragraph breaks and spaces survive.
pub fn extract_complete_sentences(buffer: &mut String) -> Vec<String> {
    let mut sentences = Vec::new();
    loop {
        let mut split_idx = None;
        let bytes = buffer.as_bytes();
        let len = bytes.len();

        for i in 0..len {
            let b = bytes[i];
            if b == b'\n' {
                let mut next_start = i + 1;
                while next_start < len && bytes[next_start] == b'\n' {
                    next_start += 1;
                }
                split_idx = Some((next_start, next_start));
                break;
            }
            if (b == b'.' || b == b'!' || b == b'?') && i + 1 < len {
                let next = bytes[i + 1];
                if next == b' ' || next == b'\t' || next == b'\n' {
                    let mut end_punc = i;
                    while end_punc + 1 < len
                        && (bytes[end_punc + 1] == b'.'
                            || bytes[end_punc + 1] == b'!'
                            || bytes[end_punc + 1] == b'?')
                    {
                        end_punc += 1;
                    }
                    if end_punc + 1 < len
                        && (bytes[end_punc + 1] == b' '
                            || bytes[end_punc + 1] == b'\t'
                            || bytes[end_punc + 1] == b'\n')
                    {
                        let mut next_start = end_punc + 1;
                        while next_start < len
                            && (bytes[next_start] == b' '
                                || bytes[next_start] == b'\t'
                                || bytes[next_start] == b'\n')
                        {
                            next_start += 1;
                        }
                        split_idx = Some((next_start, next_start));
                        break;
                    }
                }
            }
        }

        if let Some((cut_end, next_start)) = split_idx {
            let sentence = buffer[..cut_end].to_string();
            buffer.drain(..next_start);
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
        } else {
            break;
        }
    }
    sentences
}

/// Parse a string that may contain ANSI escape sequences into Ratatui `Line`s.
/// If ANSI sequences are present and successfully parsed, returns styled lines.
/// Otherwise returns plain text lines.
pub fn parse_ansi_lines(input: &str) -> Vec<Line<'static>> {
    if input.contains('\x1b')
        && let Ok(text) = input.as_bytes().into_text()
    {
        return text.lines;
    }
    input.lines().map(|l| Line::raw(l.to_string())).collect()
}

/// Render a markdown string into Ratatui `Line`s with syntax-highlighted code blocks.
pub fn render_markdown_lines(input: &str) -> Vec<Line<'static>> {
    let text = tui_markdown::from_str(input);
    text.lines
        .into_iter()
        .map(|line| {
            let spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.to_string(), span.style))
                .collect();
            Line::from(spans)
        })
        .collect()
}

/// Extract thought blocks and remaining content from a message.
pub fn extract_thought_and_content(msg: &str) -> (Option<String>, String) {
    let mut thought = String::new();
    let mut content = String::new();
    let mut in_think = false;

    for raw_line in msg.lines() {
        let line = raw_line.replace('\t', "    ");
        let segments = parse_line_segments(&line, &mut in_think);
        for seg in segments {
            match seg {
                LineSegment::Thought(t) => {
                    thought.push_str(t);
                    thought.push('\n');
                }
                LineSegment::Content(c) => {
                    content.push_str(c);
                    content.push('\n');
                }
            }
        }
    }

    let thought_opt = if thought.trim().is_empty() {
        None
    } else {
        Some(thought.trim().to_string())
    };
    (thought_opt, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI seams must report exactly what the single grammar owner reports,
    /// including the parenthesised task-id form.
    #[test]
    fn plan_line_seams_delegate_to_plan_parse() {
        const PAREN: &str = "- [ ] (t-c3ui-2) Migrate schema";
        const PAREN_DONE: &str = "- [x] (t-c3ui-1) Baseline";
        const INDENT: &str = "    * [ ] [t-c3ui-3] Indented star bullet";
        const BOLD: &str = "- [X] **[t-c3ui-4]** Bold decoration";
        const PROSE: &str = "Prose that quotes (t-c3ui-2) without a checkbox";
        const HEADER: &str = "### Phase 1";

        for line in [PAREN, PAREN_DONE, INDENT, BOLD] {
            assert_eq!(
                extract_plan_line_task_id(line),
                crate::plan_parse::task_id_of_line(line),
                "UI extraction diverges for {line:?}"
            );
        }
        assert_eq!(
            extract_plan_line_task_id(PAREN).as_deref(),
            Some("t-c3ui-2")
        );
        assert_eq!(
            extract_plan_line_task_id(PAREN_DONE).as_deref(),
            Some("t-c3ui-1")
        );
        assert_eq!(
            extract_plan_line_task_id(INDENT).as_deref(),
            Some("t-c3ui-3")
        );
        assert_eq!(extract_plan_line_task_id(BOLD).as_deref(), Some("t-c3ui-4"));
        assert_eq!(extract_plan_line_task_id(PROSE), None);
        assert_eq!(extract_plan_line_task_id(HEADER), None);

        assert!(line_matches_task_id(PAREN, "t-c3ui-2"));
        assert!(line_matches_task_id(PAREN, "[t-c3ui-2]"));
        assert!(!line_matches_task_id(PAREN, "t-c3ui-20"));
        assert!(!line_matches_task_id(PAREN, ""));
        assert!(!line_matches_task_id(PROSE, "t-c3ui-9"));
    }

    /// Plan-panel scroll offsets must agree with the shared grammar's line index
    /// for wide panels (no wrapping), and must skip already-checked tasks.
    #[test]
    fn plan_offsets_agree_with_shared_grammar() {
        let plan = "\
# Plan
- [x] (t-c3ui-1) Baseline
- [ ] [t-c3ui-2] Pending in bracket form
- [ ] t-c3ui-3 Pending in bare form
";
        assert_eq!(
            visual_line_offset_of_first_pending(plan, 500),
            crate::plan_parse::first_unchecked_line_index(plan)
        );
        assert_eq!(visual_line_offset_of_first_pending(plan, 500), Some(2));
        assert_eq!(visual_line_offset_of_task(plan, "t-c3ui-1", 500), Some(1));
        assert_eq!(visual_line_offset_of_task(plan, "t-c3ui-3", 500), Some(3));
        assert_eq!(visual_line_offset_of_task(plan, "t-c3ui-404", 500), None);
        // Wrapped panels still use the UI's own wrapped-line accounting.
        let wrapped = visual_line_offset_of_first_pending(
            &plan.replacen(
                "- [ ] t-c3ui-3 Pending in bare form",
                "- [ ] t-c3ui-3 a very long pending description that will wrap at twenty columns",
                1,
            ),
            20,
        );
        assert!(
            wrapped.is_some_and(|off| off >= 2),
            "wrapped offset: {wrapped:?}"
        );
    }

    /// The transcript role palette belongs to `theme.rs`, not to this file: a
    /// `[Tool Call]` line takes [`theme::role_tool`] and a plain body line takes
    /// [`theme::text_primary`] — an explicit **no** foreground, so a light
    /// terminal still reads the transcript (research §F2, which is exactly what
    /// the hard-coded `Color::White` used to break).
    #[test]
    fn tool_and_body_lines_take_a_palette_slot_never_a_raw_colour() {
        let tool = render_message_lines("[Tool Call] read_file(src/main.rs)", false);
        assert_eq!(tool.len(), 1, "got: {tool:?}");
        assert_eq!(tool[0].spans[0].style, theme::role_tool());

        let body = render_message_lines("plain orchestrator body", false);
        assert_eq!(body.len(), 1, "got: {body:?}");
        assert_eq!(body[0].spans[0].style, theme::text_primary());
        assert_eq!(
            body[0].spans[0].style.fg, None,
            "body text must stay fg-less and inherit the operator's profile"
        );
    }

    /// The reasoning tier keeps `ITALIC` **and** carries a weight step, so a
    /// terminal that ignores italics still separates reasoning from the dim
    /// chrome it used to collapse into (§F7). The tier never falls back to a
    /// colour-only signal.
    #[test]
    fn the_thought_tier_keeps_italic_and_a_weight_step() {
        let style = thought_style();
        assert!(style.add_modifier.contains(Modifier::ITALIC));
        assert!(
            style.add_modifier.contains(Modifier::DIM),
            "the tier needs a step that survives a terminal without italics"
        );
        assert_ne!(style, theme::text_muted(), "reasoning vs gutter chrome");
        assert_ne!(style, theme::role_turn(), "reasoning vs a turn separator");

        let lines = render_message_lines("<think>weigh the seam</think>\nvisible answer", true);
        assert_eq!(lines.len(), 2, "got: {lines:?}");
        assert_eq!(lines[0].spans[0].content, "weigh the seam");
        assert_eq!(lines[0].spans[0].style, style, "thought takes the tier");
        assert_eq!(lines[1].spans[0].style, theme::text_primary());
    }
}
