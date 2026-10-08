//! Formatting, text parsing, terminal math, and word wrapping utilities for TUI.

use ansi_to_tui::IntoText;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
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
    } else if first.starts_with("Marmennill: ") || first.starts_with("[Steer Arbitrator]") {
        (
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            true,
        )
    } else if first.starts_with('[') && first.ends_with("completed.") {
        (
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            true,
        )
    } else if first.starts_with('[')
        && (first.ends_with("failed.")
            || first.ends_with("failed]")
            || first.contains(" failed:")
            || first.contains(" failed]"))
    {
        (
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
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
    // Fast path: if there are no math delimiters or symbols, return unmodified text immediately.
    if !text.contains('$') && !text.contains('\\') && !text.contains('^') && !text.contains('_') {
        return text.to_string();
    }

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
                    if !inner.trim().is_empty()
                        && !inner
                            .chars()
                            .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
                    {
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
            if chunk.contains('\\') || chunk.contains('^') || chunk.contains('_') {
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
                if rest.starts_with(cmd) {
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
            for &(pattern, repl) in SUPERSCRIPTS {
                if rest.starts_with(pattern) {
                    out.push_str(repl);
                    cursor += pattern.len();
                    matched = true;
                    break;
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
            for &(pattern, repl) in SUBSCRIPTS {
                if rest.starts_with(pattern) {
                    out.push_str(repl);
                    cursor += pattern.len();
                    matched = true;
                    break;
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
                    chat_lines.push(Line::from(Span::styled(
                        cleaned,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
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
                    chat_lines.push(Line::from(Span::styled(
                        cleaned,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
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
                            chat_lines.push(Line::from(Span::styled(
                                cleaned,
                                Style::default().fg(Color::Magenta),
                            )));
                        }
                    } else if cleaned.contains('\x1b') {
                        chat_lines.extend(parse_ansi_lines(&cleaned));
                    } else {
                        // Orchestrator / Model content: WHITE
                        chat_lines.push(Line::from(Span::styled(
                            cleaned,
                            Style::default().fg(Color::White),
                        )));
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
///
/// "What counts as an uncompleted task line" is decided by
/// [`crate::plan_parse`] (dedup cluster C3) — the same rule the on-disk plan
/// authority uses — and only the wrapped-line arithmetic stays here.
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
/// The grammar lives in [`crate::plan_parse::task_id_of_line`] (dedup cluster
/// C3) so the plan panel, the delegation guard and the on-disk check-off all
/// agree. Recognized formats include:
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

/// Compute the visual (wrapped) line offset of a specific task in the execution plan text (e.g. `t-001`).
/// Returns `None` if the task ID is not found.
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
    /// including the parenthesised task id form the plan panel used to miss.
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
}
