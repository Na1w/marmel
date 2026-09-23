//! XML tool-call rescue (REQ-HARN-001).

use super::HarnessStats;
use crate::tool_names::{
    TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_READ_FILE, TOOL_REPLACE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
};
use crate::types::ToolCall;

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
    // or `<tool_call function="write_file" path="foo.md"># Content...</tool_call>`
    if let Some((name, body)) = try_function_attr(block) {
        let args_json = extract_inner_text(body);
        let parsed = serde_json::from_str::<serde_json::Value>(args_json.trim());
        let mut map = match parsed {
            Ok(serde_json::Value::Object(m)) => m,
            _ => {
                let mut m = serde_json::Map::new();
                let inner = args_json.trim();
                if !inner.is_empty() {
                    if name == TOOL_WRITE_FILE || name == TOOL_REPLACE {
                        m.insert(
                            "content".to_string(),
                            serde_json::Value::String(inner.to_string()),
                        );
                    } else if name == TOOL_READ_FILE {
                        m.insert(
                            "path".to_string(),
                            serde_json::Value::String(inner.to_string()),
                        );
                    } else if name == TOOL_RUN_COMMAND {
                        m.insert(
                            "command".to_string(),
                            serde_json::Value::String(inner.to_string()),
                        );
                    } else if name == TOOL_GREP_SEARCH {
                        m.insert(
                            "query".to_string(),
                            serde_json::Value::String(inner.to_string()),
                        );
                    } else if name == TOOL_GLOB {
                        m.insert(
                            "pattern".to_string(),
                            serde_json::Value::String(inner.to_string()),
                        );
                    }
                }
                m
            }
        };

        // Extract opening tag attributes like path="...", file="...", command="...", query="...", pattern="..."
        for attr in &[
            "path",
            "file",
            "file_path",
            "filepath",
            "filename",
            "command",
            "query",
            "pattern",
            "doc",
            "destination",
            "dest",
            "target",
        ] {
            if let Some(val) = extract_attribute(block, attr) {
                map.entry((*attr).to_string())
                    .or_insert_with(|| serde_json::Value::String(val));
            }
        }

        let arguments = if map.is_empty() {
            serde_json::Value::String(args_json.trim().to_string())
        } else {
            serde_json::Value::Object(map)
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
