//! Formatting, text parsing, terminal math, and word wrapping utilities for TUI.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Choose the message-level style by prefix matching (reference §4.3).
/// Returns `(style, has_special_style)`.
pub fn message_style(msg: &str) -> (Style, bool) {
    let first = msg.lines().next().unwrap_or("");
    if first.starts_with("--- Turn") || first.starts_with("=== Turn") {
        (
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            true,
        )
    } else if first.starts_with("[Status] ") || first.starts_with("[CLI] ") {
        (Style::default().fg(Color::Yellow), true)
    } else if first.starts_with("System Error: ") {
        (
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            true,
        )
    } else if first.starts_with("User: ") || first.starts_with("User (Steer): ") {
        (
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            true,
        )
    } else if first.starts_with("Marmennill: ")
        || first.starts_with("Kvaser: ")
        || first.starts_with("[Steer Arbitrator]")
    {
        (
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            true,
        )
    } else {
        // Orchestrator / Model content defaults to white text.
        (Style::default().fg(Color::White), false)
    }
}

/// Segment of a rendered line representing either reasoning thought or visible content.
#[derive(Debug, PartialEq, Eq)]
pub enum LineSegment<'a> {
    Thought(&'a str),
    Content(&'a str),
}

/// Parse segments of a line according to the running `in_think` state, tracking transitions across `<think>` and `</think>` tags.
pub fn parse_line_segments<'a>(line: &'a str, in_think: &mut bool) -> Vec<LineSegment<'a>> {
    let mut segments = Vec::new();
    let mut cursor = 0;
    while cursor < line.len() {
        if *in_think {
            // Looking for close tag: </think> or </thought>
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
            // Looking for open tag: <think> or <thought>
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

/// Strip XML thinking tags from a rendered line.
pub fn strip_think_tags(line: &str) -> String {
    line.replace("<think>", "")
        .replace("</think>", "")
        .replace("<thought>", "")
        .replace("</thought>", "")
}

/// Format LaTeX math expressions ($$...$$, $...$, and common math symbols)
/// into clean, readable terminal Unicode text.
pub fn format_terminal_math(text: &str) -> String {
    let mut s = text.to_string();
    if !s.contains('$') && !s.contains('\\') && !s.contains('^') && !s.contains('_') {
        return s;
    }

    // Replace display math $$...$$
    while let Some(start) = s.find("$$") {
        if let Some(end_rel) = s[start + 2..].find("$$") {
            let end = start + 2 + end_rel;
            let inner = &s[start + 2..end];
            let formatted = format_math_expr(inner.trim());
            s.replace_range(start..end + 2, &format!("  {formatted}"));
        } else {
            let inner = &s[start + 2..];
            let formatted = format_math_expr(inner.trim());
            s.replace_range(start.., &format!("  {formatted}"));
            break;
        }
    }

    // Replace inline math $...$
    let mut i = 0;
    while i < s.len() {
        if let Some(start_rel) = s[i..].find('$') {
            let start = i + start_rel;
            if start + 1 < s.len() && s.as_bytes()[start + 1] == b'$' {
                i = start + 2;
                continue;
            }
            if let Some(end_rel) = s[start + 1..].find('$') {
                let end = start + 1 + end_rel;
                let inner = &s[start + 1..end];
                if !inner.trim().is_empty()
                    && !inner
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
                {
                    let formatted = format_math_expr(inner);
                    s.replace_range(start..end + 1, &formatted);
                    i = start + formatted.len();
                    continue;
                }
                i = end + 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    format_math_expr(&s)
}

pub fn format_math_expr(expr: &str) -> String {
    let mut out = expr
        .replace(r"\|", "|")
        .replace(r"\cdot", "·")
        .replace(r"\times", "×")
        .replace(r"\pm", "±")
        .replace(r"\mp", "∓")
        .replace(r"\leq", "≤")
        .replace(r"\le", "≤")
        .replace(r"\geq", "≥")
        .replace(r"\ge", "≥")
        .replace(r"\neq", "≠")
        .replace(r"\ne", "≠")
        .replace(r"\approx", "≈")
        .replace(r"\to", "→")
        .replace(r"\rightarrow", "→")
        .replace(r"\leftarrow", "←")
        .replace(r"\dots", "…")
        .replace(r"\cdots", "…")
        .replace(r"\quad", "  ")
        .replace(r"\qquad", "    ")
        .replace(r"\alpha", "α")
        .replace(r"\beta", "β")
        .replace(r"\gamma", "γ")
        .replace(r"\theta", "θ")
        .replace(r"\lambda", "λ")
        .replace(r"\mu", "μ")
        .replace(r"\pi", "π")
        .replace(r"\sigma", "σ")
        .replace(r"\omega", "ω")
        .replace(r"\Delta", "Δ")
        .replace(r"\sum", "∑")
        .replace(r"\prod", "∏")
        .replace(r"\int", "∫")
        .replace(r"\infty", "∞");

    // Replace superscripts
    out = out
        .replace("^2", "²")
        .replace("^3", "³")
        .replace("^0", "⁰")
        .replace("^1", "¹")
        .replace("^4", "⁴")
        .replace("^5", "⁵")
        .replace("^6", "⁶")
        .replace("^7", "⁷")
        .replace("^8", "⁸")
        .replace("^9", "⁹")
        .replace("^{+}", "⁺")
        .replace("^{-}", "⁻")
        .replace("^{2}", "²")
        .replace("^{3}", "³")
        .replace("^{0}", "⁰")
        .replace("^{1}", "¹")
        .replace("^{n}", "ⁿ")
        .replace("^{t}", "ᵗ")
        .replace("^{T}", "ᵀ")
        .replace("^n", "ⁿ")
        .replace("^t", "ᵗ")
        .replace("^T", "ᵀ");

    // Replace subscripts
    out = out
        .replace("_{0}", "₀")
        .replace("_{1}", "₁")
        .replace("_{2}", "₂")
        .replace("_{3}", "₃")
        .replace("_{4}", "₄")
        .replace("_{5}", "₅")
        .replace("_{6}", "₆")
        .replace("_{7}", "₇")
        .replace("_{8}", "₈")
        .replace("_{9}", "₉")
        .replace("_{i}", "ᵢ")
        .replace("_{n}", "ₙ")
        .replace("_{x}", "ₓ")
        .replace("_{y}", "ᵧ")
        .replace("_{z}", "₂")
        .replace("_0", "₀")
        .replace("_1", "₁")
        .replace("_2", "₂")
        .replace("_3", "₃")
        .replace("_4", "₄")
        .replace("_5", "₅")
        .replace("_6", "₆")
        .replace("_7", "₇")
        .replace("_8", "₈")
        .replace("_9", "₉")
        .replace("_i", "ᵢ")
        .replace("_n", "ₙ")
        .replace("_x", "ₓ")
        .replace("_y", "ᵧ");

    while let Some(start) = out.find(r"\text{") {
        if let Some(end_rel) = out[start + 6..].find('}') {
            let end = start + 6 + end_rel;
            let inner = out[start + 6..end].to_string();
            out.replace_range(start..end + 1, &inner);
        } else {
            break;
        }
    }

    while let Some(start) = out.find(r"\sqrt{") {
        if let Some(end_rel) = out[start + 6..].find('}') {
            let end = start + 6 + end_rel;
            let inner = out[start + 6..end].to_string();
            out.replace_range(start..end + 1, &format!("√({inner})"));
        } else {
            break;
        }
    }

    while let Some(start) = out.find(r"\frac{") {
        if let Some(mid_rel) = out[start + 6..].find("}{") {
            let mid = start + 6 + mid_rel;
            if let Some(end_rel) = out[mid + 2..].find('}') {
                let end = mid + 2 + end_rel;
                let num = out[start + 6..mid].to_string();
                let den = out[mid + 2..end].to_string();
                out.replace_range(start..end + 1, &format!("({num})/({den})"));
                continue;
            }
        }
        break;
    }

    out
}

/// Count the wrapped lines a single message occupies at `width`, excluding
/// think-block lines when `show_thought` is `false` (reference §11.2).
pub fn count_single_message_lines(msg: &str, width: usize, show_thought: bool) -> usize {
    let mut n = 0;
    let mut in_think = false;
    for line in msg.lines() {
        let segments = parse_line_segments(line, &mut in_think);
        for seg in segments {
            match seg {
                LineSegment::Thought(t) => {
                    if !show_thought {
                        continue;
                    }
                    let cleaned = format_terminal_math(t.trim());
                    if !cleaned.is_empty() {
                        n += wrapped_lines(&cleaned, width);
                    }
                }
                LineSegment::Content(c) => {
                    let cleaned = format_terminal_math(c.trim());
                    if !cleaned.is_empty() {
                        n += wrapped_lines(&cleaned, width);
                    }
                }
            }
        }
    }
    n
}

/// Grapheme-aware word-wrap line counter (reference §11.1).
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
pub fn visual_line_offset_of_first_pending(text: &str, width: usize) -> Option<usize> {
    static UNCHECKED_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = UNCHECKED_RE.get_or_init(|| {
        regex::Regex::new(r"^\s*(?:[-*]|\d+\.)?\s*(\[\s*\]|\(\s*\))")
            .expect("valid unchecked regex")
    });
    let mut visual_offset = 0;
    for raw_line in text.lines() {
        if re.is_match(raw_line) {
            return Some(visual_offset);
        }
        visual_offset += wrapped_lines(raw_line, width).max(1);
    }
    None
}

/// Compute a centered rectangle of `percent_x`% width and `percent_y`% height
/// within `r` (reference §9.1).
#[allow(dead_code)]
pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Extract complete sentences from a streaming buffer (reference §10).
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

pub fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)
}
