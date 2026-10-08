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

/// Documented valid range for `temperature` on an OpenAI-compatible
/// `/chat/completions` endpoint: `MIN_TEMPERATURE..=MAX_TEMPERATURE` (0.0..=2.0).
/// Outside it the provider answers **400**, so recovery requests are clamped here.
pub const MIN_TEMPERATURE: f32 = 0.0;
/// See [`MIN_TEMPERATURE`].
pub const MAX_TEMPERATURE: f32 = 2.0;

/// Documented valid range for `frequency_penalty` and `presence_penalty`:
/// `-2.0..=2.0`.
pub const MIN_PENALTY: f32 = -2.0;
/// See [`MIN_PENALTY`].
pub const MAX_PENALTY: f32 = 2.0;

/// Documented valid range for `top_p`: `0.0..=1.0`.
pub const MIN_TOP_P: f32 = 0.0;
/// See [`MIN_TOP_P`].
pub const MAX_TOP_P: f32 = 1.0;

/// `temperature` used when the request leaves it unset (mirrors `Config::default`).
const DEFAULT_TEMPERATURE: f32 = 0.7;
/// `frequency_penalty` used when the request leaves it unset (mirrors `Config::default`).
const DEFAULT_FREQUENCY_PENALTY: f32 = 0.0;

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
/// Every sampling parameter of the returned request is additionally clamped into
/// the provider's **documented valid range** ([`MIN_TEMPERATURE`]..=[`MAX_TEMPERATURE`]
/// for `temperature`, [`MIN_PENALTY`]..=[`MAX_PENALTY`] for both penalties,
/// [`MIN_TOP_P`]..=[`MAX_TOP_P`] for `top_p`). Without that, the recovery deltas
/// push a legal configured value out of range (e.g. `frequency_penalty = 2.0`
/// becomes `2.5`) and the provider answers **HTTP 400** — the recovery turn then
/// fails instead of recovering. Any such rewrite is a typed [`RecoveryClamp`]
/// and is logged once per request (old → new), never silently.
///
/// The returned request is a mutated copy intended for exactly one turn; the
/// caller is responsible for not reusing it on subsequent turns.
pub fn apply_recovery(req: &ChatRequest, adj: RecoveryAdjustment) -> ChatRequest {
    let outcome = apply_recovery_report(req, adj);
    outcome.log_clamps();
    outcome.request
}

/// Same policy as [`apply_recovery`], but returning the typed report of what had
/// to be clamped so callers/tests can assert the outcome, not just the numbers.
pub fn apply_recovery_report(req: &ChatRequest, adj: RecoveryAdjustment) -> RecoveryOutcome {
    let mut out = req.clone();
    let mut clamps = Vec::new();

    out.enable_thinking = Some(false);

    // The recovery shift is what overflows the documented range, so the shifted
    // value — not the raw configured one — is what gets clamped.
    out.frequency_penalty = clamp_param(
        "frequency_penalty",
        Some(
            req.frequency_penalty.unwrap_or(DEFAULT_FREQUENCY_PENALTY)
                + adj.frequency_penalty_delta,
        ),
        (MIN_PENALTY, MAX_PENALTY),
        &mut clamps,
    );
    out.temperature = clamp_param(
        "temperature",
        Some(req.temperature.unwrap_or(DEFAULT_TEMPERATURE) + adj.temperature_delta),
        (MIN_TEMPERATURE, MAX_TEMPERATURE),
        &mut clamps,
    );

    // Pass-through sampling parameters belong to the request that is actually put
    // on the wire, so they are validated here too.
    out.top_p = clamp_param("top_p", out.top_p, (MIN_TOP_P, MAX_TOP_P), &mut clamps);
    out.presence_penalty = clamp_param(
        "presence_penalty",
        out.presence_penalty,
        (MIN_PENALTY, MAX_PENALTY),
        &mut clamps,
    );

    RecoveryOutcome {
        request: out,
        clamps,
    }
}

/// Clamp one optional parameter into `range`, pushing a [`RecoveryClamp`] when
/// the value actually moved. `None` stays `None` (the field is not serialized).
fn clamp_param(
    param: &'static str,
    value: Option<f32>,
    range: (f32, f32),
    clamps: &mut Vec<RecoveryClamp>,
) -> Option<f32> {
    let requested = value?;
    let (min, max) = range;
    let (applied, reason) = if !requested.is_finite() {
        (min, ClampReason::NotFinite)
    } else if requested < min {
        (min, ClampReason::BelowRange)
    } else if requested > max {
        (max, ClampReason::AboveRange)
    } else {
        return Some(requested);
    };
    clamps.push(RecoveryClamp {
        param,
        requested,
        applied,
        valid_range: range,
        reason,
    });
    Some(applied)
}

/// Why a recovery parameter had to be rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClampReason {
    /// Below the documented minimum.
    BelowRange,
    /// Above the documented maximum.
    AboveRange,
    /// Not a finite number (`NaN` / `±inf`): cannot be serialized meaningfully.
    NotFinite,
}

impl ClampReason {
    /// Machine-readable reason, used in [`RecoveryClamp::label`].
    pub fn label(self) -> &'static str {
        match self {
            ClampReason::BelowRange => "below-range",
            ClampReason::AboveRange => "above-range",
            ClampReason::NotFinite => "not-finite",
        }
    }
}

/// One parameter rewritten by the recovery policy because the requested value was
/// outside the documented valid range.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryClamp {
    /// Wire name of the parameter (as it appears in the request body).
    pub param: &'static str,
    /// The value that was asked for.
    pub requested: f32,
    /// The value applied instead.
    pub applied: f32,
    /// The documented valid range `(min, max)`.
    pub valid_range: (f32, f32),
    /// Why it moved.
    pub reason: ClampReason,
}

impl RecoveryClamp {
    /// Old → new, with the documented range: `temperature 2.1 -> 2 (documented 0..=2, above-range)`.
    pub fn label(&self) -> String {
        let (min, max) = self.valid_range;
        format!(
            "{} {} -> {} (documented {min}..={max}, {})",
            self.param,
            self.requested,
            self.applied,
            self.reason.label()
        )
    }
}

/// The typed outcome of [`apply_recovery_report`].
#[derive(Debug, Clone)]
pub struct RecoveryOutcome {
    /// The recovery request, ready to issue: every sampling parameter is inside
    /// its documented range, so the provider cannot reject it as invalid.
    pub request: ChatRequest,
    /// One entry per out-of-range parameter (empty when nothing had to change).
    pub clamps: Vec<RecoveryClamp>,
}

impl RecoveryOutcome {
    /// Log every clamp applied, **once per request**, naming old → new. A clamped
    /// recovery turn is therefore always visible in the log.
    pub fn log_clamps(&self) {
        if self.clamps.is_empty() {
            return;
        }
        let changes = self
            .clamps
            .iter()
            .map(RecoveryClamp::label)
            .collect::<Vec<_>>()
            .join("; ");
        tracing::warn!(
            clamped = %changes,
            "recovery request had out-of-range parameters; clamped into the documented range instead of issuing a request the provider rejects with 400"
        );
    }
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
