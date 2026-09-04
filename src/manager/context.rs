//! Context engine: token counting (cl100k_base BPE), compaction, and rebirth.
//!
//! # Requirements
//!
//! - **REQ-CORE-001** (KV-Cache Prefix Preservation): the system prompt is
//!   locked strictly at `messages[0]` and must never be moved or prepended
//!   with transient state.
//! - **REQ-CORE-002** (Immutable Goal Pinning): the initial user goal is
//!   locked at `messages[1]`; compaction/rebirth never remove or alter it.
//! - **REQ-CORE-003** (Context Budget & Compaction): token counting uses a
//!   `cl100k_base` BPE singleton. Compaction triggers at > 90% of budget and
//!   targets 70%, keeping `[0]`/`[1]` and pruning orphan `role:"tool"`
//!   messages.
//! - **REQ-CORE-004** (Forced Rebirth): collapse to exactly 4 messages with a
//!   `SYSTEM: REBIRTH CHECKPOINT` injection and bump `session_rebirths`.
//! - **REQ-CORE-005** (Slow-Prefill Cooling): track recovery turns and warn
//!   when consecutive backend prefill calls exceed 300s (suppressed within the
//!   first 5 turns after a rebirth).
//! - **REQ-CORE-006** (UTF-8 Safe String Slicing): all string slicing happens
//!   only at valid UTF-8 character boundaries.

use std::sync::Arc;
use std::time::Duration;

use crate::harness::HarnessStats;
use crate::types::Message;

/// The exact system checkpoint content injected at `messages[3]` on rebirth
/// (REQ-CORE-004).
pub const REBIRTH_CHECKPOINT_PREFIX: &str = "(SYSTEM: REBIRTH CHECKPOINT. The previous turn-by-turn history has been compacted. Summarized progress: ";

/// Rebirth advisory trigger ratio (80% of budget).
pub const REBIRTH_ADVISORY_TRIGGER_RATIO: f64 = 0.80;
/// Compaction trigger ratio (90% of budget).
pub const COMPACTION_TRIGGER_RATIO: f64 = 0.90;
/// Compaction target ratio (70% of budget).
pub const COMPACTION_TARGET_RATIO: f64 = 0.70;
/// Slow-prefill threshold in seconds (REQ-CORE-005).
pub const SLOW_PREFILL_THRESHOLD_SECS: u64 = 300;
/// Minimum recovery turns before a slow-prefill warning is emitted.
const MIN_TURNS_AFTER_REBIRTH: u64 = 5;

/// The exact `SYSTEM: CONTEXT BUDGET ADVISORY` message injected when context reaches 80% of budget.
pub const REBIRTH_ADVISORY_MESSAGE: &str = "(SYSTEM: CONTEXT BUDGET ADVISORY. You have reached 80% of your context budget. You should summarize your progress, key findings, and immediate next steps, then invoke the 'rebirth' tool with your summary before forced context compaction occurs.)";

/// Compaction retry cap: compaction is attempted at most twice (`< 2`),
/// mirroring caesar's `compaction_retry_count < 2` gate (orchestrator.rs:982).
const COMPACTION_RETRY_CAP: u32 = 2;
/// Compaction target ratio for the first retry (70% of the budget).
const COMPACTION_RETRY1_RATIO: f64 = 0.70;
/// Compaction target ratio for the second retry (50% of the budget).
const COMPACTION_RETRY2_RATIO: f64 = 0.50;
/// Target ratio used by `compact_context` when the transcript exceeds the hard
/// limit on retry 1 (80% of the limit, caesar models.rs:473).
const COMPACTION_OVER_LIMIT_TARGET_RATIO: f64 = 0.80;
/// The exact `SYSTEM: CONTEXT LIMIT EXCEEDED` user-message injection emitted
/// after a successful forced compaction (caesar orchestrator.rs:1002).
pub const CONTEXT_LIMIT_EXCEEDED_MESSAGE: &str = "(SYSTEM: CONTEXT LIMIT EXCEEDED. Your context window overflowed. The proxy had to FORCIBLY compact your history by pruning older messages and tool results. You MUST immediately summarize your progress and invoke the 'rebirth' tool with your summary to reset your memory properly.)";

/// BPE tokenizer singleton (cl100k_base).
fn bpe() -> &'static tiktoken_rs::CoreBPE {
    tiktoken_rs::cl100k_base_singleton()
}

/// Count the total BPE tokens in a message transcript.
///
/// Uses `encode_ordinary` (no special tokens) and adds a small per-message
/// framing overhead, mirroring the standard OpenAI cookbook approximation.
pub fn count_tokens(messages: &[Message]) -> usize {
    let enc = bpe();
    messages.iter().map(|m| message_tokens(enc, m)).sum()
}

/// BPE token count of a single message: 3 tokens framing overhead plus the
/// encoded lengths of its content, reasoning, tool calls, or tool content.
fn message_tokens(enc: &tiktoken_rs::CoreBPE, m: &Message) -> usize {
    3 + match m {
        Message::System { content } | Message::User { content } => {
            enc.encode_ordinary(content).len()
        }
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
        } => {
            content.as_ref().map_or(0, |c| enc.encode_ordinary(c).len())
                + reasoning_content
                    .as_ref()
                    .map_or(0, |r| enc.encode_ordinary(r).len())
                + tool_calls
                    .iter()
                    .map(|tc| {
                        1 + enc.encode_ordinary(&tc.function.name).len()
                            + enc.encode_ordinary(&tc.function.arguments).len()
                    })
                    .sum::<usize>()
        }
        Message::Tool { content, .. } => enc.encode_ordinary(content).len(),
    }
}

/// The token count above which a rebirth advisory should be emitted (80% of the budget).
pub fn rebirth_advisory_threshold(max_context_tokens: usize) -> usize {
    (max_context_tokens as f64 * REBIRTH_ADVISORY_TRIGGER_RATIO).round() as usize
}

/// The token count above which compaction is triggered (90% of the budget).
pub fn compaction_threshold(max_context_tokens: usize) -> usize {
    (max_context_tokens as f64 * COMPACTION_TRIGGER_RATIO).round() as usize
}

/// The token count that compaction targets (70% of the budget).
pub fn compaction_target(max_context_tokens: usize) -> usize {
    (max_context_tokens as f64 * COMPACTION_TARGET_RATIO).round() as usize
}

/// Slice a `&str` at UTF-8 safe boundaries (REQ-CORE-006).
///
/// `start`/`end` are byte offsets. `start` is clamped up to the first char
/// boundary at or after it; `end` is clamped down to the last char boundary at
/// or before it. The result never severs a multibyte character.
pub fn utf8_safe_slice(s: &str, start: usize, end: usize) -> &str {
    let len = s.len();
    let mut begin = start.min(len);
    while begin < len && !s.is_char_boundary(begin) {
        begin += 1;
    }
    let mut stop = end.min(len);
    while stop > begin && !s.is_char_boundary(stop) {
        stop -= 1;
    }
    if begin >= stop {
        return "";
    }
    &s[begin..stop]
}

/// Prune orphaned `role:"tool"` messages (REQ-CORE-003).
///
/// A tool message is an orphan if its `tool_call_id` does not correspond to a
/// `tool_calls` entry on a *surviving* assistant message. Orphans are dropped
/// so no invalid tool response is ever sent upstream.
pub fn prune_orphan_tool_messages(messages: Vec<Message>) -> Vec<Message> {
    let valid_ids: std::collections::HashSet<_> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flatten()
        .map(|t| t.id.clone())
        .collect();
    messages
        .into_iter()
        .filter(|m| match m {
            Message::Tool { tool_call_id, .. } => valid_ids.contains(tool_call_id),
            _ => true,
        })
        .collect()
}

/// Track slow backend prefill calls for the cooling heuristic (REQ-CORE-005).
#[derive(Debug, Clone, Default)]
pub struct SlowPrefillTracker {
    /// Recovery turns elapsed since the last rebirth.
    turns_since_rebirth: u64,
    /// How many *consecutive* prefill calls exceeded the 300s threshold.
    consecutive_slow: u32,
}

impl SlowPrefillTracker {
    /// Create an empty tracker (0 turns, 0 slow calls).
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the duration of one backend prefill call.
    ///
    /// Increments `turns_since_rebirth` and returns `true` when a slow-prefill
    /// rebirth warning should be emitted: a prefill met or exceeded the
    /// threshold, at least two *consecutive* prefills were slow, and at least
    /// [`MIN_TURNS_AFTER_REBIRTH`] recovery turns have elapsed since the last
    /// rebirth (turns 1..MIN_TURNS_AFTER_REBIRTH are suppressed).
    pub fn record_prefill(&mut self, duration: Duration) -> bool {
        self.turns_since_rebirth = self.turns_since_rebirth.saturating_add(1);
        if duration.as_secs() >= SLOW_PREFILL_THRESHOLD_SECS {
            self.consecutive_slow = self.consecutive_slow.saturating_add(1);
            self.consecutive_slow >= 2 && self.turns_since_rebirth >= MIN_TURNS_AFTER_REBIRTH
        } else {
            self.consecutive_slow = 0;
            false
        }
    }

    /// Reset the recovery-turn counter after a rebirth.
    pub fn note_rebirth(&mut self) {
        self.turns_since_rebirth = 0;
        self.consecutive_slow = 0;
    }

    /// The number of recovery turns since the last rebirth.
    pub fn turns_since_rebirth(&self) -> u64 {
        self.turns_since_rebirth
    }
}

/// Context engine holding the message transcript, token budget, and rebirth
/// statistics.
#[derive(Debug, Clone)]
pub struct ContextEngine {
    /// The message transcript. `messages[0]` is the system prompt and
    /// `messages[1]` is the immutable goal.
    messages: Vec<Message>,
    /// Maximum context tokens before compaction is required.
    max_context_tokens: usize,
    /// Resilience intervention registry (shared with the monitor/LLM layers).
    stats: Option<Arc<HarnessStats>>,
    /// Slow-prefill cooling tracker.
    prefill: SlowPrefillTracker,
    /// Compaction retry counter (max 2), mirroring caesar's
    /// `compaction_retry_count < 2` gate. Reset to 0 on a successful backend
    /// response.
    compaction_retry_count: u32,
    /// Whether a rebirth advisory has already been emitted for the current context window.
    rebirth_advisory_emitted: bool,
}

impl ContextEngine {
    pub fn new(max_context_tokens: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_context_tokens,
            stats: None,
            prefill: SlowPrefillTracker::new(),
            compaction_retry_count: 0,
            rebirth_advisory_emitted: false,
        }
    }

    /// Attach the shared stats registry so compaction/rebirth can record
    /// interventions (REQ-HARN-004).
    pub fn set_stats(&mut self, stats: Arc<HarnessStats>) {
        self.stats = Some(stats);
    }

    /// Lock the system prompt at `messages[0]` (REQ-CORE-001).
    pub fn set_system_prompt(&mut self, prompt: String) {
        if self.messages.is_empty() {
            self.messages.push(Message::System { content: prompt });
        } else {
            self.messages[0] = Message::System { content: prompt };
        }
    }

    /// Pin the goal at `messages[1]` (REQ-CORE-002).
    ///
    /// If the transcript has no system prompt yet, a placeholder is inserted
    /// first to keep `messages[0]`/`messages[1]` indexing stable.
    pub fn set_goal(&mut self, goal: String) {
        if self.messages.is_empty() {
            self.messages.push(Message::System {
                content: String::new(),
            });
        }
        if self.messages.len() == 1 {
            self.messages.push(Message::User { content: goal });
        } else {
            self.messages[1] = Message::User { content: goal };
        }
    }

    pub fn append(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    pub fn pop_last(&mut self) -> Option<Message> {
        self.messages.pop()
    }

    pub fn replace_last(&mut self, msg: Message) {
        if let Some(last) = self.messages.last_mut() {
            *last = msg;
        } else {
            self.messages.push(msg);
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn max_context_tokens(&self) -> usize {
        self.max_context_tokens
    }

    /// The current token count of the transcript.
    pub fn token_count(&self) -> usize {
        count_tokens(&self.messages)
    }

    /// Whether compaction should trigger right now (> 90% of budget).
    pub fn should_compact(&self) -> bool {
        self.token_count() > compaction_threshold(self.max_context_tokens)
    }

    /// Whether a rebirth advisory should be emitted right now (> 80% of budget and not yet emitted).
    pub fn should_advise_rebirth(&self) -> bool {
        !self.rebirth_advisory_emitted
            && self.token_count() > rebirth_advisory_threshold(self.max_context_tokens)
    }

    /// Inject the `SYSTEM: CONTEXT BUDGET ADVISORY` user message instructing the
    /// agent to summarize and invoke `rebirth`.
    pub fn inject_rebirth_advisory(&mut self) {
        self.rebirth_advisory_emitted = true;
        self.messages.push(Message::User {
            content: REBIRTH_ADVISORY_MESSAGE.to_string(),
        });
    }

    /// Reset the rebirth advisory emitted flag.
    pub fn reset_rebirth_advisory(&mut self) {
        self.rebirth_advisory_emitted = false;
    }

    /// Whether a rebirth advisory has already been emitted for this context generation.
    pub fn rebirth_advisory_emitted(&self) -> bool {
        self.rebirth_advisory_emitted
    }

    /// Compact the transcript down to ~70% of the budget (REQ-CORE-003).
    ///
    /// Always keeps `messages[0]` (System) and `messages[1]` (Goal). The most
    /// recent turn pairs are preserved first; once the target budget is
    /// reached, older turns are dropped. Any `role:"tool"` messages that
    /// outlive their assistant tool call are pruned.
    pub fn compact(&mut self) {
        let initial_tokens = self.token_count();
        let initial_msgs = self.messages.len();
        let target = compaction_target(self.max_context_tokens);

        // Always pin the first two messages.
        let mut kept: Vec<Message> = self.messages.iter().take(2).cloned().collect();

        // Preserve the most recent turns (index 2 onward) while under budget.
        let tail: Vec<Message> = self.messages.iter().skip(2).cloned().collect();
        let mut kept_tail: Vec<Message> = Vec::new();
        let mut total = count_tokens(&kept);

        for m in tail.into_iter().rev() {
            let cost = count_tokens(std::slice::from_ref(&m));
            if total + cost > target && !kept_tail.is_empty() {
                break;
            }
            total += cost;
            kept_tail.push(m);
        }
        kept_tail.reverse();
        kept.extend(kept_tail);

        // Eliminate orphaned tool messages introduced by the pruning above.
        self.messages = prune_orphan_tool_messages(kept);

        let final_tokens = self.token_count();
        let final_msgs = self.messages.len();
        tracing::info!(
            "Context compaction executed (automatic): {initial_tokens} tokens ({initial_msgs} msgs) -> {final_tokens} tokens ({final_msgs} msgs, target budget: {target})"
        );

        if self.token_count() <= rebirth_advisory_threshold(self.max_context_tokens) {
            self.rebirth_advisory_emitted = false;
        }

        if let Some(stats) = &self.stats {
            stats.record_compaction();
        }
    }

    /// The current compaction retry count (0, 1, or 2).
    pub fn compaction_retry_count(&self) -> u32 {
        self.compaction_retry_count
    }

    /// Reset the compaction retry counter to 0 (called on a successful backend
    /// response, mirroring caesar orchestrator.rs:1042).
    pub fn reset_compaction_retry_count(&mut self) {
        self.compaction_retry_count = 0;
    }

    /// Compact the transcript under caesar's retry-escalation semantics
    /// (REQ-CORE-003, caesar orchestrator.rs:982-1002).
    ///
    /// Gated by `compaction_retry_count < 2`:
    /// - **Retry 1** (`compaction_retry_count == 1`): if the transcript exceeds
    ///   the hard `limit`, call `compact_context(limit)` (targets 80% of the
    ///   limit); otherwise fall through to the ratio path targeting 70%.
    /// - **Retry 2** (`compaction_retry_count == 2`): ratio path targeting 50%.
    ///
    /// On success it records the compaction stat and injects the
    /// `SYSTEM: CONTEXT LIMIT EXCEEDED` user message instructing the agent to
    /// summarize and invoke `rebirth`. Returns `true` when a compaction was
    /// performed, `false` when the retry cap was reached or nothing was removed.
    pub fn compact_with_retry(&mut self, limit: usize) -> bool {
        if self.compaction_retry_count >= COMPACTION_RETRY_CAP {
            return false;
        }
        self.compaction_retry_count += 1;
        let initial_tokens = self.token_count();
        let initial_msgs = self.messages.len();

        let compacted = if self.compaction_retry_count == 1 && self.token_count() > limit {
            // Retry 1 over the hard limit: compact_context targets 80% of limit.
            self.compact_context(limit)
        } else {
            // Ratio path: 70% on retry 1, 50% on retry 2.
            let ratio = if self.compaction_retry_count == 1 {
                COMPACTION_RETRY1_RATIO
            } else {
                COMPACTION_RETRY2_RATIO
            };
            let current = self.token_count();
            let target = ((current.min(limit) as f64) * ratio).round() as usize;
            self.force_compact_context(target)
        };

        if compacted {
            let final_tokens = self.token_count();
            let final_msgs = self.messages.len();
            tracing::info!(
                "Context compaction executed (forced/retry {}): {initial_tokens} tokens ({initial_msgs} msgs) -> {final_tokens} tokens ({final_msgs} msgs, limit: {limit})",
                self.compaction_retry_count
            );
            if let Some(stats) = &self.stats {
                stats.record_compaction();
            }
            if self.token_count() <= rebirth_advisory_threshold(self.max_context_tokens) {
                self.rebirth_advisory_emitted = false;
            }
            self.inject_context_limit_exceeded();
        }
        compacted
    }

    /// Compact the transcript down to 80% of `max_tokens` when it currently
    /// exceeds `max_tokens` (caesar `compact_context`, models.rs:470).
    ///
    /// Returns `false` (no-op) when the transcript is already within `max_tokens`.
    pub fn compact_context(&mut self, max_tokens: usize) -> bool {
        let current = self.token_count();
        if current <= max_tokens {
            return false;
        }
        let target = ((max_tokens as f64) * COMPACTION_OVER_LIMIT_TARGET_RATIO).round() as usize;
        self.compact_to_target(target, false)
    }

    /// Force a compaction down to `target_tokens`, removing at least one
    /// message even if already under target (caesar `force_compact_context`,
    /// models.rs:480).
    pub fn force_compact_context(&mut self, target_tokens: usize) -> bool {
        self.compact_to_target(target_tokens, true)
    }

    /// Shared compaction core: pin `[0]`/`[1]`, keep the most recent turns
    /// while under `target_tokens`, and prune orphaned tool messages.
    ///
    /// When `force` is true, at least one message is always removed even if the
    /// transcript is already under target.
    fn compact_to_target(&mut self, target_tokens: usize, force: bool) -> bool {
        let current = self.token_count();
        if !force && current <= target_tokens {
            return false;
        }

        // Always pin the first two messages.
        let mut kept: Vec<Message> = self.messages.iter().take(2).cloned().collect();

        // Preserve the most recent turns (index 2 onward) while under budget.
        let tail: Vec<Message> = self.messages.iter().skip(2).cloned().collect();
        let mut kept_tail: Vec<Message> = Vec::new();
        let mut total = count_tokens(&kept);

        for m in tail.into_iter().rev() {
            let cost = count_tokens(std::slice::from_ref(&m));
            if total + cost > target_tokens && !kept_tail.is_empty() {
                break;
            }
            total += cost;
            kept_tail.push(m);
        }
        kept_tail.reverse();
        let removed = self.messages.len().saturating_sub(2 + kept_tail.len());
        kept.extend(kept_tail);

        // Eliminate orphaned tool messages introduced by the pruning above.
        self.messages = prune_orphan_tool_messages(kept);

        removed > 0
    }

    /// Inject the `SYSTEM: CONTEXT LIMIT EXCEEDED` user message after a
    /// successful forced compaction (caesar orchestrator.rs:1002). The message
    /// instructs the agent to summarize its progress and invoke `rebirth`.
    pub fn inject_context_limit_exceeded(&mut self) {
        self.messages.push(Message::User {
            content: CONTEXT_LIMIT_EXCEEDED_MESSAGE.to_string(),
        });
    }

    /// Collapse the entire history into exactly 4 messages (REQ-CORE-004).
    ///
    /// - `messages[0]`: static system prompt.
    /// - `messages[1]`: original user goal.
    /// - `messages[2]`: last user instruction (or the goal when not distinct).
    /// - `messages[3]`: the `SYSTEM: REBIRTH CHECKPOINT` injection.
    ///
    /// The `session_rebirths` counter in the attached [`HarnessStats`] is
    /// incremented, and the slow-prefill cooling tracker is reset.
    pub fn perform_rebirth(&mut self, summary: &str) {
        let initial_tokens = self.token_count();
        let initial_msgs = self.messages.len();
        let system = match self.messages.first() {
            Some(Message::System { content }) => content.clone(),
            _ => String::new(),
        };
        let goal = match self.messages.get(1) {
            Some(Message::User { content }) => content.clone(),
            _ => String::new(),
        };

        // Determine the last user instruction distinct from the goal.
        let last_user_instruction = self.messages.iter().skip(2).rev().find_map(|m| match m {
            Message::User { content } if *content != goal => Some(content.clone()),
            _ => None,
        });
        let slot2 = last_user_instruction.unwrap_or_else(|| goal.clone());

        let checkpoint = format!("{REBIRTH_CHECKPOINT_PREFIX}{summary})");
        self.messages = vec![
            Message::System { content: system },
            Message::User { content: goal },
            Message::User { content: slot2 },
            Message::System {
                content: checkpoint,
            },
        ];

        let final_tokens = self.token_count();
        let final_msgs = self.messages.len();
        tracing::info!(
            "Context compaction executed (rebirth): {initial_tokens} tokens ({initial_msgs} msgs) -> {final_tokens} tokens ({final_msgs} msgs), summary: {summary}"
        );

        self.rebirth_advisory_emitted = false;
        if let Some(stats) = &self.stats {
            stats.record_rebirth();
        }
        self.prefill.note_rebirth();
    }

    /// Access the slow-prefill cooling tracker.
    pub fn prefill_tracker(&self) -> &SlowPrefillTracker {
        &self.prefill
    }

    /// Mutable access to the slow-prefill cooling tracker.
    pub fn prefill_tracker_mut(&mut self) -> &mut SlowPrefillTracker {
        &mut self.prefill
    }
}

/// Per-agent [`ContextEngine`] factory.
///
/// Produces isolated, KV-cache-prefix-preserving contexts for the Manager and
/// every Specialist (REQ-ORCH-003). Each engine is seeded so that
/// `messages[0]` is the agent's own role system prompt and `messages[1]` is
/// its pinned goal/task brief, guaranteeing:
///
/// - **REQ-CORE-001** KV-cache prefix preservation at the *agent* scope: the
///   system/role prompt is locked at `[0]` for the lifetime of the engine.
/// - **REQ-CORE-002** goal pinning: the user goal (Manager) or task brief
///   (specialist) is pinned at `[1]` and survives compaction/rebirth.
/// - **REQ-ORCH-003** strict isolation: a specialist context contains ONLY its
///   own role prompt + brief — never the Manager's transcript. The factory is
///   the canonical construction point so no code path can seed a specialist
///   with Manager history.
#[derive(Debug, Clone)]
pub struct ContextEngineFactory {
    /// Shared token budget for every produced engine.
    max_context_tokens: usize,
}

impl ContextEngineFactory {
    /// Create a factory bound to a token budget for all produced engines.
    pub fn new(max_context_tokens: usize) -> Self {
        Self { max_context_tokens }
    }

    /// The context budget applied to every produced engine.
    pub fn max_context_tokens(&self) -> usize {
        self.max_context_tokens
    }

    /// Build the Manager's context engine (REQ-CORE-001/002 at the top level).
    ///
    /// The Manager owns the *user* conversation, so its `[0]` is the global
    /// system prompt and `[1]` is the user's goal. This engine accumulates the
    /// full interactive transcript and is distinct from every specialist's.
    pub fn manager_context(&self, system_prompt: String, goal: String) -> ContextEngine {
        let mut ctx = ContextEngine::new(self.max_context_tokens);
        ctx.set_system_prompt(system_prompt);
        ctx.set_goal(goal);
        ctx
    }

    /// Build a specialist's fully isolated context engine (REQ-ORCH-003).
    ///
    /// The produced engine carries ONLY `role_prompt` at `messages[0]` and
    /// `brief` at `messages[1]` (the pinned subagent "goal", mirroring
    /// REQ-CORE-002 at the subagent scope). No Manager history is ever copied.
    /// A freshly built engine therefore starts with exactly two messages and a
    /// stable KV-cache prefix for the specialist's backend.
    pub fn specialist_context(&self, role_prompt: String, brief: String) -> ContextEngine {
        let mut ctx = ContextEngine::new(self.max_context_tokens);
        ctx.set_system_prompt(role_prompt);
        ctx.set_goal(brief);
        ctx
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
