//! Loop monitor: XML tool rescue, semantic repetition detection, cycle breaking
//! and text repetition breaking (REQ-HARN-001…004).
//!
//! This module is responsible for the resilience harness that keeps an agent
//! loop productive:
//!
//! * [`XMLToolRescue`] recovers tool calls the LLM emitted as plain-text XML
//!   instead of structured JSON (REQ-HARN-001).
//! * [`ToolRepetitionDetector`] tracks the last 50 executed tool calls using
//!   semantic JSON equality (ignoring argument key ordering and pagination
//!   fields) and blocks repetitions / cuts alternating cycles (REQ-HARN-002).
//! * [`RepetitionDetector`] watches a 1000-character rolling buffer of streamed
//!   assistant output for text that repeats itself ≥5 times (REQ-HARN-003).
//!
//! Every intervention is recorded atomically in the shared [`HarnessStats`]
//! registry (REQ-HARN-004).

use std::collections::VecDeque;

use crate::harness::HarnessStats;
use crate::tool_names::{TOOL_GREP_SEARCH, TOOL_READ_FILE};
use crate::types::{Message, ToolCall};

/// Buffer length (number of tool-call records) for the repetition detector.
const TOOL_BUFFER_CAPACITY: usize = 50;
/// Default number of consecutive identical calls that blocks execution
/// (caesar `repetition_threshold` default).
const DEFAULT_REPETITION_THRESHOLD: usize = 5;
/// Rolling buffer length (in characters) for the text repetition detector.
const TEXT_BUFFER_CAPACITY: usize = 1000;
/// Default minimum pattern length considered for text repetition detection
/// (caesar `min_pattern_len` default).
const DEFAULT_MIN_PATTERN_LEN: usize = 5;

/// A single tool call record tracked by the semantic repetition detector
/// (REQ-HARN-002). Equality is *semantic*: argument key ordering is ignored
/// and pagination fields (`offset`, `page`) are excluded.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub arguments: serde_json::Value,
}

impl ToolCallRecord {
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }

    /// Convert from a wire [`ToolCall`] (arguments arrive as a JSON string).
    pub fn from_tool_call(call: &ToolCall) -> Self {
        let args = serde_json::from_str(&call.function.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.function.arguments.clone()));
        Self::new(call.function.name.clone(), args)
    }

    /// Semantic equality against another record: same name and
    /// argument-JSON equality with pagination fields ignored.
    pub fn semantically_eq(&self, other: &ToolCallRecord) -> bool {
        self.name == other.name && semantic_json_value_eq(&self.arguments, &other.arguments)
    }

    /// True when the two records represent the exact same operation identity
    /// (same name and raw-identical arguments). A pagination-only difference is
    /// *not* the same operation (REQ-HARN-002 pagination exemption).
    pub fn same_operation(&self, other: &ToolCallRecord) -> bool {
        if self.name != other.name {
            return false;
        }
        // Pagination-only variation (offset/page) is explicitly exempt: it is
        // progress, not repetition.
        if is_pagination_tool(&self.name)
            && pagination_only_differs(&self.arguments, &other.arguments)
        {
            return false;
        }
        self.arguments == other.arguments
    }
}

/// Outcome of evaluating a freshly recorded tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intervention {
    /// No intervention; the call is allowed to proceed.
    None,
    /// The call was an identical repetition ≥3 times: execution is blocked.
    Block,
    /// An alternating cycle repeated ≥3 times: execution is cut.
    Cut,
}

/// Semantic JSON equality that ignores object key ordering and, for known
/// pagination-capable tools, drops the `offset`/`page` arguments
/// (REQ-HARN-002).
fn semantic_json_value_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    strip_pagination(a) == strip_pagination(b)
}

/// True when the record represents a call to a pagination-capable tool for
/// which offset/page differences are progress, not repetition.
fn is_pagination_tool(name: &str) -> bool {
    matches!(name, TOOL_READ_FILE | TOOL_GREP_SEARCH)
}

/// True when `a` and `b` differ *only* by pagination keys (`offset`, `page`),
/// i.e. they have identical stripped forms. Used for the pagination exemption.
fn pagination_only_differs(a: &serde_json::Value, b: &serde_json::Value) -> bool {
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
pub struct XMLToolRescue {
    stats: Option<std::sync::Arc<HarnessStats>>,
}

impl Default for XMLToolRescue {
    fn default() -> Self {
        Self::new()
    }
}

impl XMLToolRescue {
    /// Create a rescue that does not attach to any stats registry.
    pub fn new() -> Self {
        Self { stats: None }
    }

    /// Create a rescue that increments `xml_tool_rescues` in `stats`.
    pub fn with_stats(stats: std::sync::Arc<HarnessStats>) -> Self {
        Self { stats: Some(stats) }
    }

    /// Attach (or replace) the stats registry used for intervention counting.
    pub fn set_stats(&mut self, stats: std::sync::Arc<HarnessStats>) {
        self.stats = Some(stats);
    }

    /// Scan `text` for any XML-style tool calls and return them as [`ToolCall`]s.
    ///
    /// Every successfully rescued call is assigned a synthetic id
    /// `call_text_{uuid}` and increments `xml_tool_rescues` in the attached
    /// stats (if any).
    pub fn rescue(&self, text: &str) -> Vec<ToolCall> {
        let mut calls = Vec::new();
        let mut scan_from = 0usize;

        while let Some((start, end)) = find_next_tool_call_block(text, scan_from) {
            let block = &text[start..end];
            if let Some(call) = parse_tool_call_block(block) {
                calls.push(call);
            }
            scan_from = end;
        }

        if !calls.is_empty()
            && let Some(stats) = &self.stats
        {
            stats.record_xml_rescue();
        }
        calls
    }
}

/// Locate the next tool-call block in `text` starting at `from`, returning the
/// byte range of the block. Returns `None` when no further block exists.
///
/// Handles both encodings:
/// * `<tool_call ...> ... </tool_call>` (JSON or attribute style), and
/// * the SPEC legacy `tool_call <function=...>...</function> tool_call`.
fn find_next_tool_call_block(text: &str, from: usize) -> Option<(usize, usize)> {
    let rest = &text[from..];

    // Angle-bracket style: `<tool_call ...> ... </tool_call>`.
    if let Some(rel) = rest.find("<tool_call") {
        let start = from + rel;
        if let Some(close_rel) = rest[rel..].find("</tool_call>") {
            let end = start + close_rel + "</tool_call>".len();
            return Some((start, end));
        }
    }

    // Legacy SPEC style: `tool_call <function=NAME>...</function> tool_call`.
    // Search for `tool_call <function=` after the current position.
    let mut search = from;
    loop {
        let rest2 = &text[search..];
        let idx = rest2.find("tool_call")?;
        let candidate = search + idx;
        // Require the legacy marker: `tool_call` followed by optional whitespace
        // then `<function=`.
        let after = candidate + "tool_call".len();
        let after_ws = &text[after..];
        let ws_len = after_ws
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(|c| c.len_utf8())
            .sum::<usize>();
        let after_ws_pos = after + ws_len;
        if text[after_ws_pos..].starts_with("<function=") {
            // Find the closing `</function>`.
            if let Some(fc_rel) = text[after_ws_pos..].find("</function>") {
                let end = after_ws_pos + fc_rel + "</function>".len();
                return Some((candidate, end));
            }
            return None;
        }
        search = candidate + "tool_call".len();
    }
}

/// Parse a single `<tool_call ...> ... </tool_call>` block body into a
/// [`ToolCall`]. Returns `None` if the block cannot be understood.
fn parse_tool_call_block(block: &str) -> Option<ToolCall> {
    // Pattern 1: `<tool_call>{"function": "...", "arguments": {...}}</tool_call>`
    if let Some(json) = try_embedded_json(block) {
        return Some(make_rescued_call(json.0, json.1));
    }

    // Pattern 2: `<tool_call function="read_file">{"path": ...}</tool_call>`
    if let Some((name, body)) = try_function_attr(block) {
        let args_json = extract_inner_text(body);
        let arguments = if let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json.trim()) {
            v
        } else {
            serde_json::Value::String(args_json.trim().to_string())
        };
        return Some(make_rescued_call(name, arguments));
    }

    // Legacy SPEC pattern with `<function=name>...</function>` + `<parameter>`.
    try_legacy_function_block(block)
}

/// Try to interpret `block` as `<tool_call>` containing a JSON object with
/// `function` and `arguments` keys. Returns `(name, arguments_value)`.
fn try_embedded_json(block: &str) -> Option<(String, serde_json::Value)> {
    let inner = extract_inner_text(block);
    let value: serde_json::Value = serde_json::from_str(inner.trim()).ok()?;
    let obj = value.as_object()?;

    // Accept both "function" and "name" keys for the function name.
    let name = obj
        .get("function")
        .or_else(|| obj.get("name"))
        .and_then(|v| v.as_str())
        .map(String::from)?;

    let arguments = obj
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Some((name, arguments))
}

/// Try to interpret `block` as `<tool_call function="NAME">BODY</tool_call>`.
fn try_function_attr(block: &str) -> Option<(String, &str)> {
    let name = extract_attribute(block, "function")?;
    let body = extract_inner_text(block);
    Some((name, body))
}

/// Legacy SPEC pattern:
/// `tool_call <function=name><parameter=key>val</parameter>...</function> tool_call`
fn try_legacy_function_block(block: &str) -> Option<ToolCall> {
    let func_open = "<function=";
    let func_start = block.find(func_open)?;
    let name_start = func_start + func_open.len();
    let name_end = block[name_start..].find('>')? + name_start;
    let name = block[name_start..name_end].trim().to_string();

    let body_start = name_end + 1;
    let close_tag = "</function>";
    let body_end_rel = block[body_start..].find(close_tag)?;
    let body = &block[body_start..body_start + body_end_rel];

    let mut map = serde_json::Map::new();
    // Parse `<parameter=key>val</parameter>` pairs.
    let mut cursor = 0usize;
    while let Some(rel) = body[cursor..].find("<parameter=") {
        let p_start = cursor + rel;
        let key_start = p_start + "<parameter=".len();
        let key_end_rel = body[key_start..].find('>')?;
        let key = body[key_start..key_start + key_end_rel].trim().to_string();
        let val_start = key_start + key_end_rel + 1;
        if let Some(val_end_rel) = body[val_start..].find("</parameter>") {
            let val = body[val_start..val_start + val_end_rel].trim().to_string();
            map.insert(key, serde_json::Value::String(val));
        }
        cursor = val_start;
    }

    let arguments = if map.is_empty() {
        serde_json::Value::String(body.trim().to_string())
    } else {
        serde_json::Value::Object(map)
    };

    Some(make_rescued_call(name, arguments))
}

/// Extract the text between the opening `<tool_call ...>` and `</tool_call>`.
fn extract_inner_text(block: &str) -> &str {
    let open_end = match block.find('>') {
        Some(i) => i + 1,
        None => return block,
    };
    match block.find("</tool_call>") {
        Some(close) => &block[open_end..close],
        None => &block[open_end..],
    }
}

/// Read the value of an attribute like `function="read_file"` from the opening
/// tag of a `<tool_call ...>` element.
fn extract_attribute(block: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=");
    let idx = block.find(&needle)?;
    let after = &block[idx + needle.len()..];
    let value = after.trim_start();
    if let Some(stripped) = value.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = value.find(|c: char| c == '>' || c.is_whitespace())?;
        Some(value[..end].to_string())
    }
}

/// Build a [`ToolCall`] with a synthetic `call_text_{uuid}` id.
fn make_rescued_call(name: String, arguments: serde_json::Value) -> ToolCall {
    let arguments = match arguments {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    };
    ToolCall::new(format!("call_text_{}", uuid_v4()), name, arguments)
}

/// Generate a UUID v4 string without external runtime deps beyond `uuid`.
fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Tool repetition & cycle detector (REQ-HARN-002)
// ---------------------------------------------------------------------------

/// Detects semantic repetition and alternating cycles across a sliding buffer
/// of the last [`TOOL_BUFFER_CAPACITY`] executed tool calls (REQ-HARN-002).
///
/// The intervention threshold is config-driven (caesar `repetition_threshold`,
/// default 5): `threshold` consecutive identical calls block, and `threshold`
/// full alternating cycles cut.
#[derive(Debug, Clone)]
pub struct ToolRepetitionDetector {
    buffer: VecDeque<ToolCallRecord>,
    /// Number of consecutive identical calls / alternating cycles that triggers
    /// an intervention (caesar `repetition_threshold`).
    threshold: usize,
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
    buffer: VecDeque<char>,
    /// Number of consecutive repeats that triggers a text repetition break
    /// (caesar `repetition_threshold`).
    threshold: usize,
    /// Minimum pattern length considered for text repetition detection
    /// (caesar `min_pattern_len`).
    min_len: usize,
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
    /// ≥`self.min_len` repeated ≥`self.threshold` times consecutively.
    pub fn is_repeating(&self) -> bool {
        let n = self.buffer.len();
        let max_pattern = n / self.threshold;
        if max_pattern < self.min_len {
            return false;
        }

        for pat_len in (self.min_len..=max_pattern).rev() {
            if self.tail_repeats(pat_len) {
                return true;
            }
        }
        false
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
}

// ---------------------------------------------------------------------------
// Orphan tool-message pruning (Part 7 checklist item 5)
// ---------------------------------------------------------------------------

/// Prune orphaned `role:"tool"` messages that have no matching assistant
/// tool_call_id.
pub fn prune_orphan_tool_messages(messages: Vec<Message>) -> Vec<Message> {
    let mut valid_ids = std::collections::HashSet::new();
    for m in &messages {
        if let Message::Assistant { tool_calls, .. } = m {
            for t in tool_calls {
                valid_ids.insert(t.id.clone());
            }
        }
    }

    messages
        .into_iter()
        .filter(|m| match m {
            Message::Tool { tool_call_id, .. } => valid_ids.contains(tool_call_id),
            _ => true,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Composed resilience monitor (runtime entry point)
// ---------------------------------------------------------------------------

/// Composed harness monitor binding all three detectors to a shared stats
/// registry, so the loop calls ONE object for every resilience intervention.
///
/// This is the runtime facade that makes REQ-HARN-001…004 *active* in the
/// live loop (Manager turn loop and specialist delegated turns alike) rather
/// than standalone, unit-tested-only components:
///
/// * [`HarnessMonitor::rescue_xml`] — REQ-HARN-001. Run on the raw assistant
///   text so plain-text XML tool calls are intercepted and reconstructed as
///   structured [`ToolCall`] JSON with `call_text_{uuid}` ids; increments
///   `xml_tool_rescues`.
/// * [`HarnessMonitor::observe_tool`] — REQ-HARN-002. Run *before* a tool
///   executes so a ≥3 identical repetition blocks, or an ≥3 alternating cycle
///   cuts, the call; the returned [`Intervention`] drives the SPEC error text.
/// * [`HarnessMonitor::feed_text`] — REQ-HARN-003. Feed streamed assistant
///   output into the 1000-char rolling buffer; returns `true` (terminating the
///   stream) when a ≥5-length pattern repeats ≥5 times, and truncates the
///   repeated block back to a single instance while incrementing
///   `repetition_breaks`.
///
/// The same stats registry is shared so interventions are aggregated across
/// the whole session (REQ-HARN-004); per-agent isolation is preserved because
/// each specialist's `AgentLoop` holds its own monitor instance rooted at the
/// shared `HarnessStats` (aggregate session counters), matching the "stats
/// aggregated per session, cognitive context isolated per agent" model.
#[derive(Debug)]
pub struct HarnessMonitor {
    /// XML tool rescue (REQ-HARN-001).
    xml: XMLToolRescue,
    /// Semantic repetition & cycle detector (REQ-HARN-002).
    tool_rep: ToolRepetitionDetector,
    /// Text repetition detector (REQ-HARN-003).
    text_rep: RepetitionDetector,
    /// Shared session intervention counters (REQ-HARN-004).
    stats: std::sync::Arc<HarnessStats>,
    /// True between the moment a text repetition break fires and the stream is
    /// restarted, so a single break increments the counter exactly once
    /// (REQ-HARN-003). Reset via [`HarnessMonitor::reset_text_break`].
    repetition_fired: bool,
}

impl HarnessMonitor {
    /// Create a monitor rooted at a shared stats registry, using the given
    /// resilience thresholds (caesar `[monitoring]` block). Pass an
    /// `Arc<HarnessStats>`; all intervention counters are recorded into it.
    pub fn new_with_config(
        stats: std::sync::Arc<HarnessStats>,
        monitoring: &crate::config::MonitoringConfig,
    ) -> Self {
        let threshold = monitoring.repetition_threshold;
        let min_len = monitoring.min_pattern_len;
        Self {
            xml: XMLToolRescue::with_stats(stats.clone()),
            tool_rep: ToolRepetitionDetector::new(threshold),
            text_rep: RepetitionDetector::new(threshold, min_len),
            stats,
            repetition_fired: false,
        }
    }

    /// Create a monitor rooted at a shared stats registry with caesar-default
    /// thresholds (`repetition_threshold = 5`, `min_pattern_len = 5`).
    pub fn new(stats: std::sync::Arc<HarnessStats>) -> Self {
        Self::new_with_config(stats, &crate::config::MonitoringConfig::default())
    }

    /// Create a monitor with a fresh, isolated stats registry (convenience for
    /// standalone / test use).
    pub fn with_new_stats() -> Self {
        Self::new(std::sync::Arc::new(HarnessStats::new()))
    }

    /// REQ-HARN-001: intercept any plain-text XML tool calls in `text` and
    /// convert them into structured [`ToolCall`] JSON with `call_text_{uuid}`
    /// ids, routing them to execution. Increments `xml_tool_rescues` in the
    /// shared stats. Returns the rescued calls (empty when none).
    pub fn rescue_xml(&self, text: &str) -> Vec<ToolCall> {
        self.xml.rescue(text)
    }

    /// REQ-HARN-002: record a tool call (by name + JSON arguments) and return
    /// the intervention. Call this immediately before dispatching so a ≥3
    /// identical repetition returns [`Intervention::Block`] and an ≥3
    /// alternating cycle returns [`Intervention::Cut`]. The caller maps the
    /// intervention to the exact SPEC error string.
    pub fn observe_tool(&mut self, name: &str, arguments: &serde_json::Value) -> Intervention {
        let record = ToolCallRecord::new(name.to_string(), arguments.clone());
        self.tool_rep.evaluate(record)
    }

    /// The exact SPEC error payload for an [`Intervention`] (REQ-HARN-002).
    /// Returns `None` for [`Intervention::None`]. The message reflects the
    /// configured repetition threshold.
    pub fn intervention_error(&self, intervention: Intervention) -> Option<String> {
        match intervention {
            Intervention::Block => Some(format!(
                "TOOL REPETITION DETECTED: You have called this tool with identical \
                 arguments {} times in a row. Stop looping and try an alternative approach.",
                self.tool_rep.threshold
            )),
            Intervention::Cut => Some(
                "TOOL CYCLE DETECTED: You are repeating a loop of tool calls. Step back \
                 and re-evaluate your plan."
                    .to_string(),
            ),
            Intervention::None => None,
        }
    }

    /// REQ-HARN-003: feed a chunk of streamed assistant output into the rolling
    /// 1000-char buffer. Returns `true` when the stream must be terminated
    /// because a pattern of length ≥5 repeated ≥5 times continuously at the
    /// tail. When it fires, the repeated block is truncated back to a single
    /// instance (buffer reset) and `repetition_breaks` is incremented exactly
    /// once.
    pub fn feed_text(&mut self, chunk: &str) -> bool {
        self.text_rep.push(chunk);
        if !self.repetition_fired && self.text_rep.is_repeating() {
            self.repetition_fired = true;
            self.stats.record_repetition_break();
            // Truncate the repeated block down to a single instance so a fresh
            // generation can resume from a clean tail (REQ-HARN-003). Preserve
            // the configured thresholds.
            let threshold = self.text_rep.threshold;
            let min_len = self.text_rep.min_len;
            self.text_rep = RepetitionDetector::new(threshold, min_len);
            return true;
        }
        false
    }

    /// Re-arm the text-repetition breaker after the stream has been restarted,
    /// allowing a new independent pattern to be detected later in the session.
    pub fn reset_text_break(&mut self) {
        self.repetition_fired = false;
    }

    /// Number of tool-call records currently in the repetition buffer.
    pub fn tool_buffer_len(&self) -> usize {
        self.tool_rep.len()
    }

    /// Access the shared stats registry (for reporting / tests).
    pub fn stats(&self) -> &std::sync::Arc<HarnessStats> {
        &self.stats
    }
}

// ---------------------------------------------------------------------------
// Unit tests (Phase B checkpoint: `cargo test --lib test_monitor_`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Helper: build a ToolCallRecord for the given name and JSON args.
    fn rec(name: &str, args: &str) -> ToolCallRecord {
        let arguments = serde_json::from_str(args).unwrap();
        ToolCallRecord::new(name, arguments)
    }

    #[test]
    fn test_monitor_xml_tool_rescue() {
        // Pattern 1: JSON embedded in <tool_call> tags.
        let json_style = r#"prefix <tool_call>{"function": "read_file", "arguments": {"path": "src/main.rs", "offset": 10}}</tool_call> suffix"#;
        let rescue = XMLToolRescue::new();
        let calls = rescue.rescue(json_style);
        assert_eq!(calls.len(), 1, "JSON-embedded tool call must be rescued");
        assert_eq!(calls[0].function.name, "read_file");
        assert!(calls[0].id.starts_with("call_text_"));
        // serde_json serializes with sorted keys; compare semantically.
        let parsed: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({"path": "src/main.rs", "offset": 10})
        );

        // Pattern 2: function attribute + JSON body.
        let attr_style =
            r#"<tool_call function="write_file">{"path": "a.txt", "content": "hi"}</tool_call>"#;
        let calls = rescue.rescue(attr_style);
        assert_eq!(calls.len(), 1, "attribute-style tool call must be rescued");
        assert_eq!(calls[0].function.name, "write_file");

        // Legacy SPEC pattern: <function=name> with <parameter> pairs.
        let legacy = r#"tool_call <function=replace><parameter=path>src/main.rs</parameter><parameter=old_str>foo</parameter><parameter=new_str>bar</parameter></function> tool_call"#;
        let calls = rescue.rescue(legacy);
        assert_eq!(calls.len(), 1, "legacy function block must be rescued");
        assert_eq!(calls[0].function.name, "replace");
        assert!(calls[0].function.arguments.contains("src/main.rs"));

        // Caesar's exact XML pattern: <tool_call><function=name><parameter=key>val</parameter></function></tool_call>.
        let caesar_style = r#"<tool_call><function=read_file><parameter=path>src/main.rs</parameter><parameter=offset>10</parameter></function></tool_call>"#;
        let calls = rescue.rescue(caesar_style);
        assert_eq!(calls.len(), 1, "caesar XML pattern must be rescued");
        assert_eq!(calls[0].function.name, "read_file");
        assert!(calls[0].function.arguments.contains("src/main.rs"));
        assert!(calls[0].function.arguments.contains("10"));
    }

    #[test]
    fn test_monitor_xml_tool_rescue_increments_stats() {
        let stats = Arc::new(HarnessStats::new());
        let rescue = XMLToolRescue::with_stats(stats.clone());
        let text =
            r#"<tool_call>{"function": "glob", "arguments": {"pattern": "*.rs"}}</tool_call>"#;
        let calls = rescue.rescue(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            stats
                .xml_tool_rescues
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn test_monitor_tool_cycle_detection() {
        // Consecutive identical calls: 3x A blocks (threshold 3).
        let mut det = ToolRepetitionDetector::new(3);
        assert_eq!(det.evaluate(rec("A", r#"{"x": 1}"#)), Intervention::None);
        assert_eq!(det.evaluate(rec("A", r#"{"x": 1}"#)), Intervention::None);
        assert_eq!(det.evaluate(rec("A", r#"{"x": 1}"#)), Intervention::Block);

        // Alternating cycle A→B→A→B→A→B cuts (threshold 3).
        let mut det = ToolRepetitionDetector::new(3);
        let a = rec("A", r#"{"x": 1}"#);
        let b = rec("B", r#"{"x": 2}"#);
        // Sequence: A,B,A,B,A,B (6 calls = 3 full cycles).
        let seq = [&a, &b, &a, &b, &a, &b];
        let mut last = Intervention::None;
        for call in seq {
            last = det.evaluate(call.clone());
        }
        assert_eq!(last, Intervention::Cut, "A→B alternating cycle must be cut");
    }

    #[test]
    fn test_monitor_json_semantic_equality() {
        // Key ordering must be ignored.
        let mut det = ToolRepetitionDetector::new(3);
        assert_eq!(
            det.evaluate(rec("read_file", r#"{"a":1,"b":2}"#)),
            Intervention::None
        );
        assert_eq!(
            det.evaluate(rec("read_file", r#"{"b":2,"a":1}"#)),
            Intervention::None
        );
        assert_eq!(
            det.evaluate(rec("read_file", r#"{"a":1,"b":2}"#)),
            Intervention::Block,
            "swapped argument keys must still be detected as repetition"
        );
    }

    #[test]
    fn test_monitor_pagination_exemption() {
        // Consecutive read_file calls varying only by offset must NOT block.
        let mut det = ToolRepetitionDetector::new(3);
        assert_eq!(
            det.evaluate(rec("read_file", r#"{"path":"f","offset":0}"#)),
            Intervention::None
        );
        assert_eq!(
            det.evaluate(rec("read_file", r#"{"path":"f","offset":10}"#)),
            Intervention::None
        );
        assert_eq!(
            det.evaluate(rec("read_file", r#"{"path":"f","offset":20}"#)),
            Intervention::None,
            "offset-only variation must be exempt from repetition"
        );

        // Consecutive grep_search calls varying only by page must NOT block.
        let mut det = ToolRepetitionDetector::new(3);
        assert_eq!(
            det.evaluate(rec("grep_search", r#"{"pattern":"x","page":1}"#)),
            Intervention::None
        );
        assert_eq!(
            det.evaluate(rec("grep_search", r#"{"pattern":"x","page":2}"#)),
            Intervention::None
        );
        assert_eq!(
            det.evaluate(rec("grep_search", r#"{"pattern":"x","page":3}"#)),
            Intervention::None
        );

        // But identical non-paginated repetition still blocks.
        let mut det = ToolRepetitionDetector::new(3);
        assert_eq!(
            det.evaluate(rec("run_command", r#"{"command":"ls"}"#)),
            Intervention::None
        );
        assert_eq!(
            det.evaluate(rec("run_command", r#"{"command":"ls"}"#)),
            Intervention::None
        );
        assert_eq!(
            det.evaluate(rec("run_command", r#"{"command":"ls"}"#)),
            Intervention::Block
        );
    }

    #[test]
    fn test_monitor_sliding_buffer_capacity() {
        let mut det = ToolRepetitionDetector::new(3);
        for i in 0..100 {
            det.record(rec(
                "run_command",
                &format!(r#"{{"command":"echo {}"}}"#, i),
            ));
        }
        assert!(det.len() <= 50, "sliding buffer must be capped at 50");
    }

    #[test]
    fn test_monitor_repetition_detector() {
        let mut det = RepetitionDetector::new(5, 5);
        // No repetition yet.
        det.push("hello world this is normal text");
        assert!(!det.is_repeating());

        // Feed a repeated block ≥5 times at the tail.
        let block = "abcde"; // length 5
        for _ in 0..5 {
            det.push(block);
        }
        assert!(
            det.is_repeating(),
            "pattern repeated 5 times at the tail must trigger"
        );

        // Interrupt the pattern: no longer repeating.
        det.push("zzz");
        assert!(!det.is_repeating());
    }

    #[test]
    fn test_monitor_repetition_detector_length_bounds() {
        // Use a non-periodic filler so the buffer itself never forms a longer
        // repeating pattern by coincidence.
        fn non_periodic_fill(det: &mut RepetitionDetector) {
            det.push("The quick brown fox jumps over the lazy dog near the river bank.");
            det.push("Zanzibar's jaguars quickly vex the faithful emu and camel.");
            det.push("Pack my box with five dozen liquor jugs for the party.");
        }

        // A 5-char pattern repeated exactly 4 times must NOT trigger (needs ≥5).
        let mut det = RepetitionDetector::new(5, 5);
        non_periodic_fill(&mut det);
        for _ in 0..4 {
            det.push("abcde");
        }
        assert!(!det.is_repeating(), "4 repeats must not trigger");

        // A short fundamental period is still caught via its length-≥5 period:
        // "ab"×30 forms "ababab" (length 6) repeated 5×, so it correctly fires.
        let mut det = RepetitionDetector::new(5, 5);
        non_periodic_fill(&mut det);
        for _ in 0..30 {
            det.push("ab");
        }
        assert!(
            det.is_repeating(),
            "length-6 period repeated ≥5× must be detected"
        );
    }

    #[test]
    fn test_monitor_semantic_json_eq_helper() {
        assert!(semantic_json_eq(r#"{"a":1,"b":2}"#, r#"{"b":2,"a":1}"#));
        // Pagination fields ignored.
        assert!(semantic_json_eq(
            r#"{"path":"f","offset":0}"#,
            r#"{"path":"f","offset":99}"#
        ));
        assert!(semantic_json_eq(
            r#"{"path":"f","page":1}"#,
            r#"{"path":"f","page":2}"#
        ));
        // Different content is not equal.
        assert!(!semantic_json_eq(r#"{"path":"f"}"#, r#"{"path":"g"}"#));
    }

    // --- Composed HarnessMonitor (runtime facade) ---

    #[test]
    fn test_monitor_composed_rescue_xml_records_stats() {
        let mon = HarnessMonitor::with_new_stats();
        let text =
            r#"<tool_call>{"function": "read_file", "arguments": {"path": "a.rs"}}</tool_call>"#;
        let calls = mon.rescue_xml(text);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].id.starts_with("call_text_"));
        assert_eq!(
            mon.stats()
                .xml_tool_rescues
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        // No XML -> no rescue, no stat bump.
        let calls = mon.rescue_xml("plain prose, no tools");
        assert!(calls.is_empty());
        assert_eq!(
            mon.stats()
                .xml_tool_rescues
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn test_monitor_composed_observe_tool_block_and_cut() {
        // Use a threshold-3 config so 3x identical blocks and 3 full cycles cut.
        let cfg = crate::config::MonitoringConfig {
            enabled: true,
            repetition_threshold: 3,
            min_pattern_len: 5,
        };
        let mut mon =
            HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
        let args = serde_json::json!({"x": 1});
        // 3x identical -> Block.
        assert_eq!(mon.observe_tool("A", &args), Intervention::None);
        assert_eq!(mon.observe_tool("A", &args), Intervention::None);
        assert_eq!(mon.observe_tool("A", &args), Intervention::Block);
        // SPEC error payload is present for Block.
        let err = mon.intervention_error(Intervention::Block).unwrap();
        assert!(err.contains("TOOL REPETITION DETECTED"));

        // Alternating cycle A→B→A→B→A→B -> Cut.
        let mut mon =
            HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
        let a = serde_json::json!({"x": 1});
        let b = serde_json::json!({"x": 2});
        let mut last = Intervention::None;
        for call in [&a, &b, &a, &b, &a, &b] {
            last = mon.observe_tool(if call == &a { "A" } else { "B" }, call);
        }
        assert_eq!(last, Intervention::Cut);
        let err = mon.intervention_error(Intervention::Cut).unwrap();
        assert!(err.contains("TOOL CYCLE DETECTED"));
        assert!(mon.intervention_error(Intervention::None).is_none());
    }

    #[test]
    fn test_monitor_composed_text_repetition_truncates_and_counts() {
        let mut mon = HarnessMonitor::with_new_stats();
        // Normal text: no break.
        mon.feed_text("This is a perfectly normal sentence for the model to output.");
        assert!(!mon.repetition_fired);

        // Feed a 5-char pattern repeated 5x: break fires exactly once.
        for _ in 0..5 {
            let fired = mon.feed_text("abcde");
            if fired {
                break;
            }
        }
        assert!(mon.repetition_fired);
        assert_eq!(
            mon.stats()
                .repetition_breaks
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "one break must increment repetition_breaks once"
        );

        // The repeated block was truncated: a follow-up feed on the reset buffer
        // does not double-count.
        mon.feed_text("still fine now");
        assert_eq!(
            mon.stats()
                .repetition_breaks
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        // After re-arming, a fresh independent pattern can break again.
        mon.reset_text_break();
        for _ in 0..5 {
            if mon.feed_text("xyzzy") {
                break;
            }
        }
        assert_eq!(
            mon.stats()
                .repetition_breaks
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }

    // --- Config-driven thresholds (t-d403 parity with caesar) ---

    #[test]
    fn test_monitor_config_driven_tool_threshold() {
        // A threshold-4 config must block only on the 4th identical call.
        let cfg = crate::config::MonitoringConfig {
            enabled: true,
            repetition_threshold: 4,
            min_pattern_len: 5,
        };
        let mut mon =
            HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
        let args = serde_json::json!({"x": 1});
        assert_eq!(mon.observe_tool("A", &args), Intervention::None);
        assert_eq!(mon.observe_tool("A", &args), Intervention::None);
        assert_eq!(mon.observe_tool("A", &args), Intervention::None);
        assert_eq!(
            mon.observe_tool("A", &args),
            Intervention::Block,
            "4th identical call must block with threshold 4"
        );
        // The SPEC error reflects the configured threshold.
        let err = mon.intervention_error(Intervention::Block).unwrap();
        assert!(
            err.contains("4 times in a row"),
            "error must reflect threshold 4"
        );
    }

    #[test]
    fn test_monitor_config_driven_cycle_threshold() {
        // A threshold-2 config cuts after 2 full A→B cycles (4 calls).
        let cfg = crate::config::MonitoringConfig {
            enabled: true,
            repetition_threshold: 2,
            min_pattern_len: 5,
        };
        let mut mon =
            HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
        let a = serde_json::json!({"x": 1});
        let b = serde_json::json!({"x": 2});
        let mut last = Intervention::None;
        for call in [&a, &b, &a, &b] {
            last = mon.observe_tool(if call == &a { "A" } else { "B" }, call);
        }
        assert_eq!(
            last,
            Intervention::Cut,
            "2 full A→B cycles must cut with threshold 2"
        );
    }

    #[test]
    fn test_monitor_config_driven_text_min_len() {
        // A min_pattern_len of 7 means a 5-char pattern repeated 5x does NOT
        // trigger, but a 7-char pattern repeated 5x does.
        let cfg = crate::config::MonitoringConfig {
            enabled: true,
            repetition_threshold: 5,
            min_pattern_len: 7,
        };
        let mut mon =
            HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
        // 5-char pattern repeated 5x: below min_len 7, must NOT fire.
        for _ in 0..5 {
            mon.feed_text("abcde");
        }
        assert!(
            !mon.repetition_fired,
            "5-char pattern must not fire with min_len 7"
        );

        // 7-char pattern repeated 5x: meets min_len, must fire.
        let mut mon =
            HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
        for _ in 0..5 {
            if mon.feed_text("abcdefg") {
                break;
            }
        }
        assert!(mon.repetition_fired, "7-char pattern repeated 5x must fire");
    }

    #[test]
    fn test_monitor_config_defaults_match_caesar() {
        // caesar defaults: repetition_threshold = 5, min_pattern_len = 5.
        let cfg = crate::config::MonitoringConfig::default();
        assert_eq!(cfg.repetition_threshold, 5);
        assert_eq!(cfg.min_pattern_len, 5);
        assert!(cfg.enabled);
    }

    // --- Orphan-message pruning (t-d403 parity with caesar) ---

    #[test]
    fn test_monitor_prune_orphan_tool_messages() {
        let assistant = Message::Assistant {
            content: Some("calling".to_string()),
            reasoning_content: None,
            tool_calls: vec![ToolCall::new("call_1", "read_file", r#"{"path":"a.rs"}"#)],
        };
        let orphan = Message::Tool {
            tool_call_id: "call_orphan".to_string(),
            content: "no matching assistant".to_string(),
        };
        let matched = Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: "fn main() {}".to_string(),
        };
        let user = Message::User {
            content: "hi".to_string(),
        };

        let pruned = prune_orphan_tool_messages(vec![user, assistant, orphan, matched]);
        // The orphan tool message is dropped; the matched one and others survive.
        assert_eq!(pruned.len(), 3);
        // Exactly one tool message remains, and it is the matched one.
        let tool_msgs: Vec<&Message> = pruned
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .collect();
        assert_eq!(tool_msgs.len(), 1, "only the matched tool message survives");
        match tool_msgs[0] {
            Message::Tool { tool_call_id, .. } => {
                assert_eq!(tool_call_id, "call_1");
            }
            _ => unreachable!(),
        }
        // The user message survives.
        assert!(pruned.iter().any(|m| matches!(m, Message::User { .. })));
    }

    #[test]
    fn test_monitor_prune_orphan_keeps_all_when_all_matched() {
        let assistant = Message::Assistant {
            content: Some("calling".to_string()),
            reasoning_content: None,
            tool_calls: vec![
                ToolCall::new("call_1", "read_file", r#"{"path":"a.rs"}"#),
                ToolCall::new("call_2", "grep_search", r#"{"pattern":"x"}"#),
            ],
        };
        let t1 = Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: "a".to_string(),
        };
        let t2 = Message::Tool {
            tool_call_id: "call_2".to_string(),
            content: "b".to_string(),
        };
        let pruned = prune_orphan_tool_messages(vec![assistant, t1, t2]);
        assert_eq!(pruned.len(), 3, "all tool messages matched, none pruned");
        let tool_msgs: Vec<&Message> = pruned
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .collect();
        assert_eq!(tool_msgs.len(), 2, "both tool messages survive");
    }
}
