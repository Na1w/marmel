//! Thinking demuxer and reasoning-suppression policy.
//!
//! REQ-LLM-002: real-time `[thinking]`/`[/thinking]` tag demuxing — thought
//! content is routed to a dedicated thinking channel and stripped from the
//! assistant payload unless `preserve_thinking` is enabled.
//! REQ-LLM-003: recovery turns force `enable_thinking=false`,
//! `frequency_penalty += 0.5`, `temperature += 0.1` for exactly one turn.
//! REQ-LLM-004: empty productions are nudged up to 3 attempts.

use crate::types::{ChatRequest, Message};

/// How a raw delta was classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    /// Visible assistant content.
    Content,
    /// Reasoning/thinking channel content.
    Thinking,
}

/// Recovery adjustments applied to a request for exactly one turn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecoveryAdjustment {
    /// Frequency-penalty delta applied on recovery (+0.5).
    pub frequency_penalty_delta: f32,
    /// Temperature delta applied on recovery (+0.1).
    pub temperature_delta: f32,
}

impl Default for RecoveryAdjustment {
    fn default() -> Self {
        Self {
            frequency_penalty_delta: 0.5,
            temperature_delta: 0.1,
        }
    }
}

/// Streaming demuxer that separates `[thinking]…[/thinking]` content from the
/// visible assistant payload on-the-fly. Tags may be split across arbitrary
/// delta boundaries (a single char per push is fine).
#[derive(Debug, Default)]
pub struct ThinkingDemuxer {
    content: String,
    thinking: String,
    in_thinking: bool,
    /// Uncommitted tail that may contain a partial `[thinking]` / `[/thinking]`
    /// tag straddling the current delta boundary.
    pending: String,
    /// Whether to keep the raw tags in the payload (`preserve_thinking`).
    preserve_thinking: bool,
}

const TAG_PAIRS: &[(&str, &str)] = &[
    ("<think>", "</think>"),
    ("[thinking]", "[/thinking]"),
    ("<thought>", "</thought>"),
];

impl ThinkingDemuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a demuxer honouring the `preserve_thinking` config flag.
    pub fn with_preserve(preserve_thinking: bool) -> Self {
        Self {
            preserve_thinking,
            ..Self::default()
        }
    }

    /// Push a raw delta and classify it. Content inside a thinking block is
    /// routed to the thinking channel; everything else to the payload channel.
    /// Tags are consumed as state and stripped unless `preserve_thinking` is set.
    pub fn push(&mut self, delta: &str) -> DeltaKind {
        self.push_delta(delta, |_, _| {})
    }

    /// Push a raw delta, calling `emit(kind, chunk)` for every piece of content
    /// or thinking text that becomes committed as a result of this delta.
    pub fn push_delta<F>(&mut self, delta: &str, mut emit: F) -> DeltaKind
    where
        F: FnMut(DeltaKind, &str),
    {
        if delta.is_empty() {
            return self.current_kind();
        }

        self.pending.push_str(delta);

        let mut kind = self.current_kind();
        loop {
            let found = if self.in_thinking {
                TAG_PAIRS
                    .iter()
                    .filter_map(|(_, close)| self.pending.find(close).map(|idx| (idx, *close)))
                    .min_by_key(|(idx, _)| *idx)
            } else {
                TAG_PAIRS
                    .iter()
                    .filter_map(|(open, _)| self.pending.find(open).map(|idx| (idx, *open)))
                    .min_by_key(|(idx, _)| *idx)
            };

            match found {
                Some((idx, tag)) => {
                    // Commit content before the tag into the current channel.
                    let pre = self.pending[..idx].to_string();
                    if !pre.is_empty() {
                        self.append(&pre, self.in_thinking);
                        emit(self.current_kind(), &pre);
                    }

                    // Handle the tag itself (preserve in payload if configured, but do not emit raw tag to stream sink).
                    if self.preserve_thinking {
                        self.content.push_str(tag);
                    }
                    self.in_thinking = !self.in_thinking;
                    kind = self.current_kind();

                    self.pending = self.pending[idx + tag.len()..].to_string();
                }
                None => {
                    let candidate_tags: Vec<&str> = if self.in_thinking {
                        TAG_PAIRS.iter().map(|(_, c)| *c).collect()
                    } else {
                        TAG_PAIRS.iter().map(|(o, _)| *o).collect()
                    };

                    let min_split = candidate_tags
                        .iter()
                        .filter_map(|t| partial_prefix_split(&self.pending, t))
                        .min();

                    if let Some(split) = min_split {
                        let committed = self.pending[..split].to_string();
                        if !committed.is_empty() {
                            self.append(&committed, self.in_thinking);
                            emit(self.current_kind(), &committed);
                        }
                        self.pending = self.pending[split..].to_string();
                    } else {
                        let committed = self.pending.clone();
                        if !committed.is_empty() {
                            self.append(&committed, self.in_thinking);
                            emit(self.current_kind(), &committed);
                        }
                        self.pending.clear();
                    }
                    return kind;
                }
            }
        }
    }

    /// Commit any remaining pending characters at the end of the stream.
    pub fn finish_delta<F>(&mut self, mut emit: F)
    where
        F: FnMut(DeltaKind, &str),
    {
        if !self.pending.is_empty() {
            let committed = std::mem::take(&mut self.pending);
            self.append(&committed, self.in_thinking);
            emit(self.current_kind(), &committed);
        }
    }

    /// Commit any remaining pending characters (no-op callback).
    pub fn finish(&mut self) {
        self.finish_delta(|_, _| {});
    }

    fn append(&mut self, s: &str, thinking: bool) {
        if s.is_empty() {
            return;
        }
        if thinking {
            self.thinking.push_str(s);
            // REQ-LLM-002: when `preserve_thinking` is enabled the thought
            // content stays in the assistant payload too (it is not stripped).
            if self.preserve_thinking {
                self.content.push_str(s);
            }
        } else {
            self.content.push_str(s);
        }
    }

    fn current_kind(&self) -> DeltaKind {
        if self.in_thinking {
            DeltaKind::Thinking
        } else {
            DeltaKind::Content
        }
    }

    /// Whether we are currently inside a `[thinking]` block.
    pub fn is_in_thinking(&self) -> bool {
        self.in_thinking
    }

    /// Access the buffered visible content (tags stripped unless preserved).
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Access the buffered thinking channel.
    pub fn thinking(&self) -> &str {
        &self.thinking
    }

    /// Assemble a single assistant `Message` from the demuxed buffers.
    pub fn into_message(self) -> Message {
        Message::Assistant {
            content: if self.content.is_empty() {
                None
            } else {
                Some(self.content)
            },
            reasoning_content: if self.thinking.is_empty() {
                None
            } else {
                Some(self.thinking)
            },
            tool_calls: Vec::new(),
        }
    }

    /// Convenience: demux a fully-assembled raw string in one call.
    pub fn demux_all(&mut self, raw: &str) {
        self.push(raw);
        self.finish();
    }
}

/// If `buf` ends with a non-empty proper prefix of `tag`, return the byte index
/// at which that partial tag begins (so the caller can hold it pending).
fn partial_prefix_split(buf: &str, tag: &str) -> Option<usize> {
    let bl = buf.len();
    let tl = tag.len();
    let start = bl.saturating_sub(tl - 1); // keep at least 1 char of the partial
    for i in (start..bl).rev() {
        if !buf.is_char_boundary(i) {
            continue;
        }
        let suffix = &buf[i..];
        if !suffix.is_empty() && tag.starts_with(suffix) {
            return Some(i);
        }
    }
    None
}

/// Demux a pre-assembled raw assistant string into visible content + thinking.
pub fn demux_stream(raw: &str) -> ThinkingDemuxer {
    let mut d = ThinkingDemuxer::new();
    d.demux_all(raw);
    d
}

/// REQ-LLM-003: Build a *recovery* request from a normal request by applying
/// the one-turn suppression policy:
///
/// - `enable_thinking = false`
/// - `frequency_penalty += 0.5`
/// - `temperature += 0.1`
///
/// The returned request is a mutated copy intended for exactly one turn; the
/// caller is responsible for not reusing it on subsequent turns.
pub fn apply_recovery(req: &ChatRequest, adj: RecoveryAdjustment) -> ChatRequest {
    let mut out = req.clone();
    out.enable_thinking = Some(false);
    out.frequency_penalty =
        Some(req.frequency_penalty.unwrap_or(0.0) + adj.frequency_penalty_delta);
    out.temperature = Some(req.temperature.unwrap_or(0.7) + adj.temperature_delta);
    out
}

/// REQ-LLM-004: Empty-production nudge state.
///
/// If a model stream finishes with 0 bytes of content and 0 tool calls, the
/// agent injects a user nudge `"?"` up to a maximum of 3 attempts before
/// returning a terminal error.
#[derive(Debug, Clone)]
pub struct NudgePolicy {
    max_attempts: u32,
    nudge_text: String,
}

impl Default for NudgePolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            nudge_text: "?".to_string(),
        }
    }
}

impl NudgePolicy {
    pub fn new(max_attempts: u32, nudge_text: impl Into<String>) -> Self {
        Self {
            max_attempts,
            nudge_text: nudge_text.into(),
        }
    }

    /// Return `true` if another nudge is still permitted given `attempts_used`
    /// empty productions so far (attempts are 0-indexed; call *before* using
    /// the nudge).
    pub fn should_nudge(&self, attempts_used: u32) -> bool {
        attempts_used < self.max_attempts
    }

    /// Append a nudge user message to the transcript, returning the new
    /// transcript for the retry.
    pub fn nudge(&self, mut messages: Vec<Message>) -> Vec<Message> {
        messages.push(Message::User {
            content: self.nudge_text.clone(),
        });
        messages
    }

    /// The maximum number of nudges permitted.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }
}

// ---------------------------------------------------------------------------
// Unit tests (Phase D checkpoint: `cargo test --lib test_llm_`)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg(test)]
#[path = "thinking_tests.rs"]
mod tests;
