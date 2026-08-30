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

const OPEN_TAG: &str = "[thinking]";
const CLOSE_TAG: &str = "[/thinking]";

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

    /// Push a raw delta and classify it. Content inside a `[thinking]` block is
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
            let tag = if self.in_thinking {
                CLOSE_TAG
            } else {
                OPEN_TAG
            };
            match self.pending.find(tag) {
                Some(idx) => {
                    // Commit content before the tag into the current channel.
                    let pre = self.pending[..idx].to_string();
                    if !pre.is_empty() {
                        self.append(&pre, self.in_thinking);
                        emit(self.current_kind(), &pre);
                    }

                    // Handle the tag itself.
                    if self.preserve_thinking {
                        self.content.push_str(tag);
                        emit(DeltaKind::Content, tag);
                    }
                    self.in_thinking = !self.in_thinking;
                    kind = self.current_kind();

                    self.pending = self.pending[idx + tag.len()..].to_string();
                }
                None => {
                    // No complete tag: check whether the tail is a partial tag
                    // prefix that may continue in the next delta. If so, hold it.
                    if let Some(split) = partial_prefix_split(&self.pending, tag) {
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
mod tests {
    use super::*;
    use crate::types::Message;

    fn msg(content: &str) -> Message {
        Message::User {
            content: content.to_string(),
        }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: "m".to_string(),
            messages: vec![msg("hi")],
            temperature: Some(0.7),
            top_p: None,
            frequency_penalty: Some(0.2),
            presence_penalty: None,
            stream: Some(true),
            enable_thinking: Some(true),
            tools: None,
        }
    }

    /// REQ-LLM-002: interleaved `[thinking]` tags separate the reasoning
    /// channel from the main payload, even when tags span delta boundaries.
    #[test]
    fn test_llm_stream_thinking_demux() {
        let raw = "Hello[thinking]Let me parse this carefully[/thinking] world";
        let mut d = ThinkingDemuxer::new();
        // Feed it one char at a time to exercise incremental tag-boundary handling.
        for ch in raw.chars() {
            d.push(&ch.to_string());
        }
        assert_eq!(
            d.content(),
            "Hello world",
            "tags must be stripped from payload"
        );
        assert_eq!(
            d.thinking(),
            "Let me parse this carefully",
            "thinking content routed to dedicated channel"
        );
        assert!(
            !d.is_in_thinking(),
            "must not be left inside a thinking block"
        );
    }

    /// REQ-LLM-002: `preserve_thinking = true` keeps the raw tags in the payload.
    #[test]
    fn test_llm_thinking_preserve() {
        let mut d = ThinkingDemuxer::with_preserve(true);
        d.demux_all("a[thinking]b[/thinking]c");
        assert_eq!(
            d.content(),
            "a[thinking]b[/thinking]c",
            "tags preserved in payload when configured"
        );
        assert_eq!(d.thinking(), "b");
    }

    /// REQ-LLM-002: into_message carries reasoning in a separate field.
    #[test]
    fn test_llm_into_message_strips_thinking() {
        let mut d = ThinkingDemuxer::new();
        d.demux_all("vis[thinking]reason[/thinking]ible");
        let m = d.into_message();
        match m {
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("visible"));
                assert_eq!(reasoning_content.as_deref(), Some("reason"));
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected assistant message"),
        }
    }

    /// REQ-LLM-003: a recovery request disables thinking and shifts
    /// frequency_penalty (+0.5) and temperature (+0.1) for one turn.
    #[test]
    fn test_llm_recovery_suppresses_thinking() {
        let base = req();
        let recovered = apply_recovery(&base, RecoveryAdjustment::default());

        assert_eq!(recovered.enable_thinking, Some(false));
        assert_eq!(
            recovered.frequency_penalty,
            Some(0.2 + 0.5),
            "frequency_penalty += 0.5"
        );
        assert_eq!(recovered.temperature, Some(0.7 + 0.1), "temperature += 0.1");

        // Original request must be untouched (one-turn semantics).
        assert_eq!(base.enable_thinking, Some(true));
        assert_eq!(base.frequency_penalty, Some(0.2));
        assert_eq!(base.temperature, Some(0.7));
    }

    /// REQ-LLM-003: recovery works when penalties/temperature are None.
    #[test]
    fn test_llm_recovery_defaults_when_absent() {
        let mut base = req();
        base.frequency_penalty = None;
        base.temperature = None;
        let recovered = apply_recovery(&base, RecoveryAdjustment::default());
        assert_eq!(recovered.frequency_penalty, Some(0.5));
        assert_eq!(recovered.temperature, Some(0.8)); // 0.7 default + 0.1
    }

    /// REQ-LLM-004: nudge policy allows up to 3 empty-production attempts.
    #[test]
    fn test_llm_empty_production_nudges() {
        let policy = NudgePolicy::default();
        assert!(policy.should_nudge(0), "attempt 1 of 3 allowed");
        assert!(policy.should_nudge(1), "attempt 2 of 3 allowed");
        assert!(policy.should_nudge(2), "attempt 3 of 3 allowed");
        assert!(!policy.should_nudge(3), "3 used -> terminal error");

        let messages = vec![msg("hi")];
        let nudged = policy.nudge(messages.clone());
        assert_eq!(nudged.len(), 2, "a user nudge is appended");
        match &nudged[1] {
            Message::User { content } => assert_eq!(content, "?"),
            _ => panic!("nudge must be a user message"),
        }
    }

    /// REQ-LLM-002: multi-byte UTF-8 characters and emojis (e.g. 🔍, 🚀)
    /// must not panic on char boundary slicing during partial tag detection.
    #[test]
    fn test_llm_stream_thinking_demux_multibyte_utf8() {
        let raw =
            "1. **Analyze**: analyze this code. 🔍 [thinking]reasoning steps 🚀[/thinking] done!";
        let mut d = ThinkingDemuxer::new();
        for ch in raw.chars() {
            d.push(&ch.to_string());
        }
        assert_eq!(d.content(), "1. **Analyze**: analyze this code. 🔍  done!");
        assert_eq!(d.thinking(), "reasoning steps 🚀");
    }

    #[test]
    fn test_partial_prefix_split_multibyte() {
        // String ending with multi-byte emoji followed by partial tag prefix.
        let buf = "Hello 🔍[thin";
        assert_eq!(partial_prefix_split(buf, OPEN_TAG), Some("Hello 🔍".len()));

        // String ending directly with multi-byte emoji (no tag prefix).
        let buf_emoji = "Hello 🔍";
        assert_eq!(partial_prefix_split(buf_emoji, OPEN_TAG), None);
    }

    #[test]
    fn test_llm_stream_thinking_incremental_emission() {
        let mut d = ThinkingDemuxer::new();
        let mut emitted_content = String::new();
        let mut emitted_thinking = String::new();

        let chunks = vec![
            "Hello, ",
            "[thin",
            "king]Let me ",
            "reason[/thin",
            "king] world!",
        ];

        for chunk in chunks {
            d.push_delta(chunk, |kind, text| match kind {
                DeltaKind::Content => emitted_content.push_str(text),
                DeltaKind::Thinking => emitted_thinking.push_str(text),
            });
        }
        d.finish_delta(|kind, text| match kind {
            DeltaKind::Content => emitted_content.push_str(text),
            DeltaKind::Thinking => emitted_thinking.push_str(text),
        });

        assert_eq!(emitted_content, "Hello,  world!");
        assert_eq!(emitted_thinking, "Let me reason");
        assert_eq!(d.content(), "Hello,  world!");
        assert_eq!(d.thinking(), "Let me reason");
    }
}
