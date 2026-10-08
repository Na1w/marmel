//! Semantic JSON helpers and repetition detectors (REQ-HARN-002 / REQ-HARN-003).

use std::collections::VecDeque;

use super::code::{is_code_line, is_code_pattern, is_code_word, is_markdown_divider};
use super::common::{Intervention, ToolCallRecord};
use crate::tool_names::{TOOL_GREP_SEARCH, TOOL_READ_FILE};

/// Buffer length (number of tool-call records) for the repetition detector.
const TOOL_BUFFER_CAPACITY: usize = 50;
/// Default number of consecutive identical calls that blocks execution
/// (caesar `repetition_threshold` default).
const DEFAULT_REPETITION_THRESHOLD: usize = 5;
/// Rolling buffer length (in characters) for the text repetition detector.
pub(super) const TEXT_BUFFER_CAPACITY: usize = 16384;
/// Hard bound on how many tail characters the tail-period scan inspects for a
/// single [`RepetitionDetector::is_repeating`] call (recon item **M3**: the scan
/// used to walk the whole rolling buffer for every streamed delta).
pub(super) const TAIL_SCAN_WINDOW: usize = 4096;
/// Hard bound on how many recent lines the line/bigram rules keep in their
/// rolling window.
pub(super) const LINE_WINDOW: usize = 256;
/// Hard bound on how many recent words the word 4-gram rule keeps in its
/// rolling window.
pub(super) const WORD_WINDOW: usize = 512;
/// Default minimum pattern length considered for text repetition detection
/// (caesar `min_pattern_len` default).
const DEFAULT_MIN_PATTERN_LEN: usize = 5;

pub(crate) fn semantic_json_value_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    strip_pagination(a) == strip_pagination(b)
}

/// True when the record represents a call to a pagination-capable tool for
/// which offset/page differences are progress, not repetition.
pub(crate) fn is_pagination_tool(name: &str) -> bool {
    matches!(name, TOOL_READ_FILE | TOOL_GREP_SEARCH)
}

/// True when `a` and `b` differ *only* by pagination keys (`offset`, `page`),
/// i.e. they have identical stripped forms. Used for the pagination exemption.
pub(crate) fn pagination_only_differs(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    a != b && strip_pagination(a) == strip_pagination(b)
}

/// Decide whether `prev` followed by `record` constitutes a consecutive repeat
/// for the ≥3-block threshold. Pagination-only variation is exempt.
fn is_consecutive_repeat(prev: &ToolCallRecord, record: &ToolCallRecord) -> bool {
    if prev.name != record.name {
        return false;
    }
    if is_pagination_tool(&prev.name) && pagination_only_differs(&prev.arguments, &record.arguments)
    {
        return false;
    }
    prev.arguments == record.arguments
}

/// Recursively remove pagination-only keys (`offset`, `page`) from a JSON
/// value so that consecutive paginated reads are not mistaken for repetition.
fn strip_pagination(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if k == "offset" || k == "page" {
                    continue;
                }
                out.insert(k.clone(), strip_pagination(v));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(strip_pagination).collect())
        }
        other => other.clone(),
    }
}

/// Normalize a tool-call JSON payload for semantic equality, dropping
/// pagination fields at the top level (legacy helper used by older callers).
pub fn semantic_json_eq(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> serde_json::Value {
        match serde_json::from_str::<serde_json::Value>(s) {
            Ok(mut v) => {
                if let Some(obj) = v.as_object_mut() {
                    let _ = obj.remove("offset");
                    let _ = obj.remove("page");
                }
                v
            }
            Err(_) => serde_json::Value::String(s.to_string()),
        }
    };
    norm(a) == norm(b)
}

// ---------------------------------------------------------------------------
// Tool-call repetition detector (REQ-HARN-002)
// ---------------------------------------------------------------------------

/// Semantic repetition guard over the recent tool-call window (REQ-HARN-002).
#[derive(Debug, Clone)]

pub struct ToolRepetitionDetector {
    pub(super) buffer: VecDeque<ToolCallRecord>,
    /// Number of consecutive identical calls / alternating cycles that triggers
    /// an intervention (caesar `repetition_threshold`).
    pub(super) threshold: usize,
}

impl Default for ToolRepetitionDetector {
    fn default() -> Self {
        Self::new(DEFAULT_REPETITION_THRESHOLD)
    }
}

impl ToolRepetitionDetector {
    /// Create a detector with the given repetition threshold (caesar
    /// `repetition_threshold`). The threshold is clamped to ≥2 to keep the
    /// cycle detector meaningful, matching caesar's `threshold.max(2)`.
    pub fn new(threshold: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(TOOL_BUFFER_CAPACITY),
            threshold: threshold.max(2),
        }
    }

    /// Number of records currently buffered.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Evaluate a freshly produced tool call: return the intervention, then
    /// record it in the sliding buffer.
    pub fn evaluate(&mut self, record: ToolCallRecord) -> Intervention {
        let result = self.classify(&record);
        self.record(record);
        result
    }

    /// Append a record to the sliding buffer, trimming to capacity.
    pub fn record(&mut self, record: ToolCallRecord) {
        if self.buffer.len() == TOOL_BUFFER_CAPACITY {
            self.buffer.pop_front();
        }
        self.buffer.push_back(record);
    }

    /// Classify `record` against the current buffer without mutating it.
    fn classify(&self, record: &ToolCallRecord) -> Intervention {
        if record.name == crate::tool_names::TOOL_REBIRTH
            && self
                .buffer
                .back()
                .is_some_and(|b| b.name == crate::tool_names::TOOL_REBIRTH)
        {
            return Intervention::Block;
        }
        if self.detect_consecutive(record) {
            return Intervention::Block;
        }
        if self.detect_cycle(record) {
            return Intervention::Cut;
        }
        Intervention::None
    }

    /// True if `record` completes ≥`self.threshold` consecutive
    /// repetition-equivalent calls (counting `record` itself). Pagination-only
    /// differences are exempt (REQ-HARN-002).
    fn detect_consecutive(&self, record: &ToolCallRecord) -> bool {
        let mut count = 1usize;
        for existing in self.buffer.iter().rev() {
            if is_consecutive_repeat(existing, record) {
                count += 1;
                if count >= self.threshold {
                    return true;
                }
            } else {
                break;
            }
        }
        false
    }

    /// True if the buffer (ending with `record`) forms an alternating two-call
    /// cycle repeated ≥`self.threshold` full cycles.
    ///
    /// An A→B→A→B→A→B pattern is detected by scanning backward and requiring
    /// the two distinct calls to alternate for at least `2 * self.threshold`
    /// calls (i.e. ≥`self.threshold` full A→B cycles).
    fn detect_cycle(&self, record: &ToolCallRecord) -> bool {
        let mut all: Vec<&ToolCallRecord> = self.buffer.iter().collect();
        all.push(record);

        let n = all.len();
        let needed = self.threshold * 2; // `threshold` full cycles of 2 calls
        if n < needed {
            return false;
        }

        let a = all[n - 1];
        let b = all[n - 2];
        if a.semantically_eq(b) {
            return false;
        }

        // Walk back from the tail, requiring strict alternation: position at
        // even distance from the tail must equal `a`, odd distance must equal `b`.
        for i in (0..n - 1).rev() {
            let expected = if (n - 1 - i).is_multiple_of(2) { a } else { b };
            if !all[i].semantically_eq(expected) {
                // Alternation broke at distance d = n-1-i from the tail.
                let d = n - 1 - i;
                let full_cycles = d / 2;
                return full_cycles >= self.threshold;
            }
        }

        // The entire tail alternated; the buffer itself contains enough cycles.
        n / 2 >= self.threshold
    }
}

// ---------------------------------------------------------------------------
// Text repetition detector (REQ-HARN-003)
// ---------------------------------------------------------------------------

/// Detects text repetition in a rolling buffer of streamed assistant output.
/// If a pattern of length ≥`min_len` repeats ≥`threshold` times consecutively
/// at the tail of the buffer, a repetition break fires (REQ-HARN-003). Both
/// thresholds are config-driven (caesar `repetition_threshold` /
/// `min_pattern_len`).
///
/// Cost discipline (recon item **M3**, `docs/recon_bugs_agents_monitor.md`):
/// `is_repeating` is invoked for **every streamed delta**
/// (`src/llm/stream.rs`, `HarnessMonitor::feed_text`), so it must never walk the
/// whole rolling buffer. The detector therefore keeps bounded rolling state that
/// [`RepetitionDetector::push`] updates incrementally in the same single pass it
/// already needs for the character buffer:
///
/// * the tail-period scan looks at the last [`TAIL_SCAN_WINDOW`] characters at
///   most and rejects each candidate pattern length on its first mismatching
///   character (O(window) per delta instead of the previous O(buffer²));
/// * the line rules look at the last [`LINE_WINDOW`] completed lines plus the
///   line currently being streamed;
/// * the word 4-gram rule looks at the last [`WORD_WINDOW`] words.
///
/// Memory is bounded by `TEXT_BUFFER_CAPACITY` characters for the buffer itself
/// plus a character budget per rolling window, independent of how much text is
/// streamed through the detector.
#[derive(Debug, Clone)]
pub struct RepetitionDetector {
    pub(super) buffer: VecDeque<char>,
    /// Number of consecutive repeats that triggers a text repetition break
    /// (caesar `repetition_threshold`).
    pub(super) threshold: usize,
    /// Minimum pattern length considered for text repetition detection
    /// (caesar `min_pattern_len`).
    pub(super) min_len: usize,
    /// Rolling window of recent **completed** lines (trimmed, non-empty) in
    /// stream order — the same view `text.lines().map(trim).filter(non-empty)`
    /// produced, maintained incrementally instead of re-derived per delta. Each
    /// entry caches the per-line heuristics so they are evaluated once per line
    /// rather than once per streamed delta.
    lines: VecDeque<LineEntry>,
    /// Characters of the line currently being streamed (no `\n` seen yet).
    /// `str::lines()` reported a trailing partial line as well, so it is part of
    /// the line view.
    partial_line: String,
    /// Rolling window of recent whitespace-separated words with surrounding
    /// punctuation stripped — the form the 4-gram rule compares.
    words: VecDeque<WordEntry>,
    /// The word currently being streamed (not yet whitespace-terminated).
    partial_word: String,
    /// Characters stored in `lines` (window character budget).
    lines_chars: usize,
    /// Characters stored in `words` (window character budget).
    words_chars: usize,
}

impl Default for RepetitionDetector {
    fn default() -> Self {
        Self::new(DEFAULT_REPETITION_THRESHOLD, DEFAULT_MIN_PATTERN_LEN)
    }
}

impl RepetitionDetector {
    /// Create a detector with the given repeat threshold and minimum pattern
    /// length (caesar `repetition_threshold` / `min_pattern_len`).
    pub fn new(threshold: usize, min_len: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(TEXT_BUFFER_CAPACITY),
            threshold: threshold.max(2),
            min_len: min_len.max(1),
            lines: VecDeque::with_capacity(LINE_WINDOW),
            lines_chars: 0,
            partial_line: String::new(),
            words: VecDeque::with_capacity(WORD_WINDOW),
            words_chars: 0,
            partial_word: String::new(),
        }
    }

    /// Push a chunk of streamed text into the rolling buffer, trimming to the
    /// [`TEXT_BUFFER_CAPACITY`] character capacity, and refresh the bounded
    /// line/word windows in the same single pass.
    pub fn push(&mut self, text: &str) {
        for ch in text.chars() {
            self.buffer.push_back(ch);
            if self.buffer.len() > TEXT_BUFFER_CAPACITY {
                let _ = self.buffer.pop_front();
            }
            if ch == '\n' {
                let raw = std::mem::take(&mut self.partial_line);
                commit_line(&mut self.lines, &mut self.lines_chars, &raw);
            } else {
                self.partial_line.push(ch);
                trim_to_char_budget(&mut self.partial_line);
            }
            if ch.is_whitespace() {
                let raw = std::mem::take(&mut self.partial_word);
                commit_word(&mut self.words, &mut self.words_chars, &raw);
            } else {
                self.partial_word.push(ch);
                trim_to_char_budget(&mut self.partial_word);
            }
        }
    }

    /// Current number of buffered characters.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Number of characters the tail-period scan inspects for the current state
    /// — bounded by [`TAIL_SCAN_WINDOW`] no matter how full the rolling buffer is.
    pub fn tail_scan_chars(&self) -> usize {
        self.buffer.len().min(TAIL_SCAN_WINDOW)
    }

    /// Number of candidate pattern lengths the tail-period scan will try for the
    /// current state — bounded by `TAIL_SCAN_WINDOW / threshold`, so it stops
    /// growing once the buffer is fuller than the scan window.
    pub fn tail_scan_candidates(&self) -> usize {
        let max_pattern = self.tail_scan_chars() / self.threshold;
        if max_pattern >= self.min_len {
            max_pattern - self.min_len + 1
        } else {
            0
        }
    }

    /// Number of lines currently held in the bounded line window.
    pub fn line_window_len(&self) -> usize {
        self.lines.len()
    }

    /// Number of words currently held in the bounded word window.
    pub fn word_window_len(&self) -> usize {
        self.words.len()
    }

    /// Returns `true` when the tail of the buffer contains a pattern of length
    /// ≥`self.min_len` repeated ≥`self.threshold` times consecutively, or when
    /// identical lines/sentences repeat ≥`self.threshold` times in the rolling
    /// window.
    pub fn is_repeating(&self) -> bool {
        self.tail_repeats_any_length() || self.line_or_phrase_repeats()
    }

    /// Bounded tail scan: walk the candidate pattern lengths from the longest
    /// inside [`TAIL_SCAN_WINDOW`] down to `min_len` (the previous order and
    /// semantics, restricted to a fixed window) and return on the first match.
    fn tail_repeats_any_length(&self) -> bool {
        let max_pattern = self.tail_scan_chars() / self.threshold;
        (self.min_len..=max_pattern)
            .rev()
            .any(|pat_len| self.tail_repeats(pat_len))
    }

    /// Check whether the last `self.threshold` groups of length `pat_len` at the
    /// tail of the buffer are all identical.
    ///
    /// The comparison is ordered so that a non-matching candidate length is
    /// rejected after a single character read (no pattern copy, no heuristic
    /// call); the pattern-level filters run only for lengths that really match.
    fn tail_repeats(&self, pat_len: usize) -> bool {
        let n = self.buffer.len();
        if n < pat_len * self.threshold {
            return false;
        }
        // Cheap gate: the last character of every candidate group must equal the
        // tail character. Most candidate lengths die here in O(1).
        let tail_ch = self.buffer[n - 1];
        for group in 1..self.threshold {
            if self.buffer[n - group * pat_len - 1] != tail_ch {
                return false;
            }
        }
        for offset in 0..pat_len {
            let ch = self.buffer[n - pat_len + offset];
            for group in 1..self.threshold {
                if self.buffer[n - pat_len + offset - group * pat_len] != ch {
                    return false;
                }
            }
        }
        // Reference pattern = final `pat_len` characters, materialised only for
        // an actual match.
        let last_str: String = self.buffer.range(n - pat_len..n).collect();
        // Ignore pure whitespace or formatting dividers (e.g. `---`, `   `)
        if !last_str.chars().any(|c| c.is_alphanumeric()) {
            return false;
        }
        // If the pattern is short (< 16 chars) and is a code token or syntax construct, ignore it
        if pat_len < 16 && is_code_pattern(&last_str) {
            return false;
        }
        true
    }

    /// Check whether the rolling line window forms a degenerate loop:
    /// 1. The exact same line repeated consecutively >= threshold times at the tail.
    /// 2. A 2-line sequence (bigram) repeated >= threshold times in the window.
    /// 3. Any non-trivial conversational line appearing >= threshold times in the window.
    fn line_or_phrase_repeats(&self) -> bool {
        // Bounded rolling view (recon M3) instead of re-collecting the whole
        // buffer per delta: the recent completed lines, plus the line currently
        // being streamed, which `str::lines()` also reported as a trailing
        // partial line.
        let mut lines: Vec<&LineEntry> = self.lines.iter().collect();
        let trailing_line = trimmed_line(&self.partial_line);
        if let Some(entry) = &trailing_line {
            lines.push(entry);
        }

        let n = lines.len();
        let th = self.threshold.max(3);
        if n >= th {
            // 1. Consecutive identical line repeats at the tail (>= th times)
            let last = lines[n - 1];
            if last.text.len() >= 8 && last.alphanumeric && !last.divider {
                let mut consecutive = 1;
                for i in 2..=th {
                    if lines[n - i].text == last.text {
                        consecutive += 1;
                    } else {
                        break;
                    }
                }
                // Code data (such as repeated rows in matrix initialization) requires a much higher spam limit
                let limit = if last.is_code { th * 3 } else { th };
                if consecutive >= limit {
                    return true;
                }
            }

            // 2. 2-line sequence (bigram) repeated >= th times in the window
            let mut bigrams = std::collections::HashMap::<(&str, &str), usize>::new();
            for pair in lines.windows(2) {
                // Ignore code bigrams like `return Ok(());\n}` or `#[test]\nfn ...`
                let (first, second) = (pair[0], pair[1]);
                if first.is_code || second.is_code {
                    continue;
                }
                if first.text.len() >= 6
                    && second.text.len() >= 6
                    && (first.alphanumeric || second.alphanumeric)
                    && !first.divider
                    && !second.divider
                {
                    let cnt = bigrams
                        .entry((first.text.as_str(), second.text.as_str()))
                        .or_insert(0);
                    *cnt += 1;
                    if *cnt >= th {
                        return true;
                    }
                }
            }

            // 3. Any single non-trivial conversational line appearing >= th times
            let mut counts = std::collections::HashMap::<&str, usize>::new();
            for entry in &lines {
                // Ignore code lines: code repeating throughout a file (e.g. return Ok(())) is normal
                if entry.is_code {
                    continue;
                }
                // Must be a substantial natural language thought or sentence (>= 25 chars)
                if entry.text.len() >= 25 && entry.alphanumeric && !entry.divider {
                    let cnt = counts.entry(entry.text.as_str()).or_insert(0);
                    *cnt += 1;
                    if *cnt >= th * 2 {
                        return true;
                    }
                }
            }
        }

        // 4. Word 4-gram sequence repeated >= th times in the rolling word window
        let mut words: Vec<&WordEntry> = self.words.iter().collect();
        let trailing_word = trimmed_word(&self.partial_word);
        if let Some(entry) = &trailing_word {
            words.push(entry);
        }

        if words.len() >= 4 {
            let mut word_ngrams =
                std::collections::HashMap::<(&str, &str, &str, &str), usize>::new();
            for gram in words.windows(4) {
                // Ignore n-grams with code keywords (e.g. `assert eq`, `pub fn`, `let mut`)
                if gram[0].is_code || gram[1].is_code || gram[2].is_code || gram[3].is_code {
                    continue;
                }
                let total_len = gram[0].text.len()
                    + gram[1].text.len()
                    + gram[2].text.len()
                    + gram[3].text.len();
                if total_len >= 12 {
                    let cnt = word_ngrams
                        .entry((
                            gram[0].text.as_str(),
                            gram[1].text.as_str(),
                            gram[2].text.as_str(),
                            gram[3].text.as_str(),
                        ))
                        .or_insert(0);
                    *cnt += 1;
                    if *cnt >= th {
                        return true;
                    }
                }
            }
        }

        false
    }
}

/// A line as tracked by the bounded line window. The heuristics the line rules
/// need ([`is_code_line`], [`is_markdown_divider`], alphanumeric presence) are
/// evaluated **once** when the line completes, so a per-delta scan does not
/// re-run them for every already-seen line.
#[derive(Debug, Clone)]
struct LineEntry {
    text: String,
    is_code: bool,
    divider: bool,
    alphanumeric: bool,
}

impl LineEntry {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            is_code: is_code_line(text),
            divider: is_markdown_divider(text),
            alphanumeric: text.chars().any(|c| c.is_alphanumeric()),
        }
    }
}

/// A word as tracked by the bounded word window (punctuation already stripped),
/// with [`is_code_word`] precomputed for the same reason as [`LineEntry`].
#[derive(Debug, Clone)]
struct WordEntry {
    text: String,
    is_code: bool,
}

impl WordEntry {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            is_code: is_code_word(text),
        }
    }
}

/// The trailing (not yet newline-terminated) line of the rolling buffer in the
/// same trimmed/non-empty form `str::lines()` reported it.
fn trimmed_line(raw: &str) -> Option<LineEntry> {
    let line = raw.trim();
    (!line.is_empty()).then(|| LineEntry::new(line))
}

/// The trailing (not yet whitespace-terminated) word of the rolling buffer in
/// the same punctuation-stripped form the 4-gram rule compares.
fn trimmed_word(raw: &str) -> Option<WordEntry> {
    let word = raw.trim_matches(|c: char| !c.is_alphanumeric());
    (!word.is_empty()).then(|| WordEntry::new(word))
}

/// Append one completed line (trimmed, non-empty) to the bounded line window,
/// evicting from the front so both the entry count and the stored characters
/// stay bounded.
fn commit_line(window: &mut VecDeque<LineEntry>, chars: &mut usize, raw: &str) {
    let line = raw.trim();
    if line.is_empty() {
        return;
    }
    let entry = LineEntry::new(line);
    *chars += entry.text.len();
    window.push_back(entry);
    while window.len() > LINE_WINDOW || *chars > TEXT_BUFFER_CAPACITY {
        match window.pop_front() {
            Some(evicted) => *chars = chars.saturating_sub(evicted.text.len()),
            None => break,
        }
    }
}

/// Append one completed word (surrounding punctuation stripped, exactly as the
/// 4-gram rule compares words) to the bounded word window.
fn commit_word(window: &mut VecDeque<WordEntry>, chars: &mut usize, raw: &str) {
    let word = raw.trim_matches(|c: char| !c.is_alphanumeric());
    if word.is_empty() {
        return;
    }
    let entry = WordEntry::new(word);
    *chars += entry.text.len();
    window.push_back(entry);
    while window.len() > WORD_WINDOW || *chars > TEXT_BUFFER_CAPACITY {
        match window.pop_front() {
            Some(evicted) => *chars = chars.saturating_sub(evicted.text.len()),
            None => break,
        }
    }
}

/// Keep an in-progress line/word bounded: a single token longer than the whole
/// rolling character buffer is already truncated by the buffer itself, so the
/// stored copy is trimmed to the same budget (on a `char` boundary).
fn trim_to_char_budget(partial: &mut String) {
    if partial.len() <= TEXT_BUFFER_CAPACITY {
        return;
    }
    let mut cut = partial.len() - TEXT_BUFFER_CAPACITY;
    while cut < partial.len() && !partial.is_char_boundary(cut) {
        cut += 1;
    }
    partial.drain(..cut);
}
