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
const TEXT_BUFFER_CAPACITY: usize = 16384;
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
// XML tool rescue (REQ-HARN-001)
// ---------------------------------------------------------------------------

/// Recovers tool calls the LLM emitted as plain-text XML instead of structured
/// JSON, converting them into valid [`ToolCall`] values with synthetic IDs of
/// the form `call_text_{uuid}`.
///
/// Supported encodings:
///
/// 1. JSON embedded in tags:
///    `<tool_call>{"function": "read_file", "arguments": {"path": "a"}}</tool_call>`
/// 2. Function-name attribute plus arguments in the body:
///    `<tool_call function="read_file">{"path": "a"}</tool_call>`
/// 3. SPEC legacy pattern:
///    `tool_call <function=read_file><parameter=path>a</parameter></function> tool_call`
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

/// Detects text repetition in a rolling 1000-character buffer of assistant
/// output. If a pattern of length ≥`min_len` repeats ≥`threshold` times
/// consecutively at the tail of the buffer, a repetition break fires
/// (REQ-HARN-003). Both thresholds are config-driven (caesar
/// `repetition_threshold` / `min_pattern_len`).
#[derive(Debug, Clone)]
pub struct RepetitionDetector {
    pub(super) buffer: VecDeque<char>,
    /// Number of consecutive repeats that triggers a text repetition break
    /// (caesar `repetition_threshold`).
    pub(super) threshold: usize,
    /// Minimum pattern length considered for text repetition detection
    /// (caesar `min_pattern_len`).
    pub(super) min_len: usize,
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
        }
    }

    /// Push a chunk of streamed text into the rolling buffer, trimming to the
    /// 1000-character capacity.
    pub fn push(&mut self, text: &str) {
        for ch in text.chars() {
            self.buffer.push_back(ch);
            if self.buffer.len() > TEXT_BUFFER_CAPACITY {
                self.buffer.pop_front();
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

    /// Returns `true` when the tail of the buffer contains a pattern of length
    /// ≥`self.min_len` repeated ≥`self.threshold` times consecutively, or when
    /// identical lines/sentences repeat ≥`self.threshold` times in the buffer.
    pub fn is_repeating(&self) -> bool {
        let n = self.buffer.len();
        let max_pattern = n / self.threshold;
        if max_pattern >= self.min_len {
            for pat_len in (self.min_len..=max_pattern).rev() {
                if self.tail_repeats(pat_len) {
                    return true;
                }
            }
        }
        self.line_or_phrase_repeats()
    }

    /// Check whether the last `self.threshold` groups of length `pat_len` at the
    /// tail of the buffer are all identical.
    fn tail_repeats(&self, pat_len: usize) -> bool {
        let n = self.buffer.len();
        if n < pat_len * self.threshold {
            return false;
        }
        let last_start = n - pat_len;
        // Reference pattern = final `pat_len` characters.
        let last: Vec<char> = self.buffer.range(last_start..n).copied().collect();
        // Ignore pure whitespace or formatting dividers (e.g. `---`, `   `)
        if !last.iter().any(|c| c.is_alphanumeric()) {
            return false;
        }
        let last_str: String = last.iter().collect();
        // If the pattern is short (< 16 chars) and is a code token or syntax construct, ignore it
        if pat_len < 16 && is_code_pattern(&last_str) {
            return false;
        }
        for group in 1..self.threshold {
            let start = last_start - (group * pat_len);
            for (i, ch) in self.buffer.range(start..start + pat_len).enumerate() {
                if *ch != last[i] {
                    return false;
                }
            }
        }
        true
    }

    /// Check whether the tail of lines forms a degenerate loop:
    /// 1. The exact same line repeated consecutively >= threshold times at the tail.
    /// 2. A 2-line sequence (bigram) repeated >= threshold times in the buffer.
    /// 3. Any non-trivial conversational line appearing >= threshold times in the buffer.
    fn line_or_phrase_repeats(&self) -> bool {
        let text: String = self.buffer.iter().collect();
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();

        let n = lines.len();
        let th = self.threshold.max(3);
        if n >= th {
            // 1. Consecutive identical line repeats at the tail (>= th times)
            let last = lines[n - 1];
            if last.len() >= 8
                && last.chars().any(|c| c.is_alphanumeric())
                && !is_markdown_divider(last)
            {
                let mut consecutive = 1;
                for i in 2..=th {
                    if lines[n - i] == last {
                        consecutive += 1;
                    } else {
                        break;
                    }
                }
                // Code data (such as repeated rows in matrix initialization) requires a much higher spam limit
                let limit = if is_code_line(last) { th * 3 } else { th };
                if consecutive >= limit {
                    return true;
                }
            }

            // 2. 2-line sequence (bigram) repeated >= th times in the buffer
            let mut bigrams = std::collections::HashMap::<(&str, &str), usize>::new();
            for w in lines.windows(2) {
                // Ignore code bigrams like `return Ok(());\n}` or `#[test]\nfn ...`
                if is_code_line(w[0]) || is_code_line(w[1]) {
                    continue;
                }
                if w[0].len() >= 6
                    && w[1].len() >= 6
                    && (w[0].chars().any(|c| c.is_alphanumeric())
                        || w[1].chars().any(|c| c.is_alphanumeric()))
                    && !is_markdown_divider(w[0])
                    && !is_markdown_divider(w[1])
                {
                    let cnt = bigrams.entry((w[0], w[1])).or_insert(0);
                    *cnt += 1;
                    if *cnt >= th {
                        return true;
                    }
                }
            }

            // 3. Any single non-trivial conversational line appearing >= th times
            let mut counts = std::collections::HashMap::<&str, usize>::new();
            for &l in &lines {
                // Ignore code lines: code repeating throughout a file (e.g. return Ok(())) is normal
                if is_code_line(l) {
                    continue;
                }
                // Must be a substantial natural language thought or sentence (>= 25 chars)
                if l.len() >= 25
                    && l.chars().any(|c| c.is_alphanumeric())
                    && !is_markdown_divider(l)
                {
                    let cnt = counts.entry(l).or_insert(0);
                    *cnt += 1;
                    if *cnt >= th * 2 {
                        return true;
                    }
                }
            }
        }

        // 4. Word 4-gram sequence repeated >= th times in the buffer
        let words: Vec<&str> = text
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|w| !w.is_empty())
            .collect();

        if words.len() >= 4 {
            let mut word_ngrams =
                std::collections::HashMap::<(&str, &str, &str, &str), usize>::new();
            for w in words.windows(4) {
                // Ignore n-grams with code keywords (e.g. `assert eq`, `pub fn`, `let mut`)
                if is_code_word(w[0])
                    || is_code_word(w[1])
                    || is_code_word(w[2])
                    || is_code_word(w[3])
                {
                    continue;
                }
                let total_len = w[0].len() + w[1].len() + w[2].len() + w[3].len();
                if total_len >= 12 {
                    let cnt = word_ngrams.entry((w[0], w[1], w[2], w[3])).or_insert(0);
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
