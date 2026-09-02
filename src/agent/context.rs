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

/// Compaction trigger ratio (90% of budget).
const COMPACTION_TRIGGER_RATIO: f64 = 0.90;
/// Compaction target ratio (70% of budget).
const COMPACTION_TARGET_RATIO: f64 = 0.70;
/// Slow-prefill threshold in seconds (REQ-CORE-005).
pub const SLOW_PREFILL_THRESHOLD_SECS: u64 = 300;
/// Minimum recovery turns before a slow-prefill warning is emitted.
const MIN_TURNS_AFTER_REBIRTH: u64 = 5;

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
}

impl ContextEngine {
    pub fn new(max_context_tokens: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_context_tokens,
            stats: None,
            prefill: SlowPrefillTracker::new(),
            compaction_retry_count: 0,
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
mod tests {
    use super::*;
    use crate::types::ToolCall;

    /// Build an assistant message carrying one tool call with the given id.
    fn assistant_with_tool(id: &str) -> Message {
        Message::Assistant {
            content: Some("let me call a tool".to_string()),
            reasoning_content: None,
            tool_calls: vec![ToolCall::new(id, "read_file", "{\"path\":\"x\"}")],
        }
    }

    /// Build a tool-response message tied to a tool call id.
    fn tool_response(id: &str) -> Message {
        Message::Tool {
            tool_call_id: id.to_string(),
            content: "tool output".to_string(),
        }
    }

    /// Build a transcript of the given length (after the pinned system+goal).
    fn fill_transcript(max_tokens: usize, extra_turns: usize) -> ContextEngine {
        let mut engine = ContextEngine::new(max_tokens);
        engine.set_system_prompt("You are a helpful coding assistant.".to_string());
        engine.set_goal("Refactor the parser module.".to_string());
        for i in 0..extra_turns {
            engine.append(Message::User {
                content: format!("Turn {i}: please make the following change to the file."),
            });
            engine.append(Message::Assistant {
                content: Some(format!("Understood, working on turn {i} now.")),
                reasoning_content: None,
                tool_calls: vec![],
            });
        }
        engine
    }

    #[test]
    fn test_context_locking() {
        let mut engine = fill_transcript(200, 5);
        let original_system = match &engine.messages()[0] {
            Message::System { content } => content.clone(),
            _ => panic!("messages[0] should be System"),
        };
        let original_goal = match &engine.messages()[1] {
            Message::User { content } => content.clone(),
            _ => panic!("messages[1] should be User goal"),
        };

        // Append a few messages; pins must survive.
        engine.append(Message::User {
            content: "one more turn".to_string(),
        });
        assert_eq!(
            match &engine.messages()[0] {
                Message::System { content } => content,
                _ => "",
            },
            original_system
        );
        assert_eq!(
            match &engine.messages()[1] {
                Message::User { content } => content,
                _ => "",
            },
            original_goal
        );

        // Rebirth must also preserve messages[0] and messages[1].
        engine.perform_rebirth("compacted after locking test");
        assert_eq!(engine.messages().len(), 4);
        assert_eq!(
            match &engine.messages()[0] {
                Message::System { content } => content,
                _ => "",
            },
            original_system
        );
        assert_eq!(
            match &engine.messages()[1] {
                Message::User { content } => content,
                _ => "",
            },
            original_goal
        );
    }

    #[test]
    fn test_context_compaction_orphan_removal() {
        let mut engine = ContextEngine::new(120);
        engine.set_system_prompt("You are a helpful coding assistant.".to_string());
        engine.set_goal("Refactor the parser module.".to_string());

        // A valid assistant->tool pair, plus an orphaned tool message whose
        // assistant call has been dropped (simulating an earlier prune).
        engine.append(assistant_with_tool("call_abc"));
        engine.append(tool_response("call_abc"));
        engine.append(tool_response("call_orphan")); // no matching assistant

        // Force the transcript well over the trigger threshold.
        for i in 0..12 {
            engine.append(Message::User {
                content: format!(
                    "This is a fairly verbose user instruction number {i} with padding text to consume many tokens."
                ),
            });
            engine.append(Message::Assistant {
                content: Some(format!(
                    "Assistant acknowledging instruction {i} with a lengthy verbose reply full of detail."
                )),
                reasoning_content: None,
                tool_calls: vec![],
            });
        }

        assert!(
            engine.should_compact(),
            "transcript should exceed 90% budget"
        );

        engine.compact();

        let target = compaction_target(120);
        assert!(
            engine.token_count() <= target,
            "compact should bring transcript to <= 70% (got {})",
            engine.token_count()
        );

        // Pins survive.
        assert!(matches!(engine.messages()[0], Message::System { .. }));
        assert!(matches!(engine.messages()[1], Message::User { .. }));

        // No orphaned tool message survives.
        let tool_ids: Vec<String> = engine
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !tool_ids.iter().any(|id| id == "call_orphan"),
            "orphaned tool message must be removed"
        );
    }

    #[test]
    fn test_context_rebirth_reconstruction() {
        let stats = Arc::new(HarnessStats::new());
        let mut engine = fill_transcript(500, 6);
        engine.set_stats(stats.clone());
        engine.append(Message::User {
            content: "Final instruction distinct from the goal.".to_string(),
        });

        engine.perform_rebirth("rewrote the module under test");

        let msgs = engine.messages();
        assert_eq!(msgs.len(), 4, "rebirth must collapse to exactly 4 messages");

        // [0] system, [1] goal, [2] last user instruction, [3] checkpoint.
        assert!(matches!(msgs[0], Message::System { .. }));
        assert!(matches!(msgs[1], Message::User { .. }));
        match &msgs[2] {
            Message::User { content } => {
                assert_eq!(content, "Final instruction distinct from the goal.")
            }
            _ => panic!("messages[2] should be the last user instruction"),
        }
        match &msgs[3] {
            Message::System { content } => {
                assert!(
                    content.starts_with(REBIRTH_CHECKPOINT_PREFIX),
                    "messages[3] must be the REBIRTH CHECKPOINT injection"
                );
                assert!(content.contains("rewrote the module under test"));
            }
            _ => panic!("messages[3] should be the checkpoint system message"),
        }

        // The session_rebirths counter must be incremented.
        assert_eq!(
            stats
                .session_rebirths
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn test_context_utf8_safe_slicing() {
        // Multibyte chars (é, ö, €, 中, 文) must never be severed mid-byte
        // regardless of the requested byte offsets (REQ-CORE-006).
        let s = "héllo wörld € 中文";

        // Offsets landing mid-char must be clamped to char boundaries.
        let sliced = utf8_safe_slice(s, 2, 14);
        // `sliced` is produced by indexing at valid boundaries, so it must be
        // a self-consistent UTF-8 slice (no panic / no split char).
        let _ = sliced.chars().count();
        assert_eq!(sliced, std::str::from_utf8(sliced.as_bytes()).unwrap());
        // Every byte in the output must be part of whole chars — i.e. the
        // output must round-trip through char boundaries losslessly.
        assert_eq!(sliced.chars().collect::<String>(), sliced);

        // Out-of-range offsets must clamp rather than panic.
        assert_eq!(utf8_safe_slice(s, 40, 100), "");

        // Empty input is a no-op.
        assert_eq!(utf8_safe_slice("", 0, 0), "");
    }

    #[test]
    fn test_context_factory_manager_prefix_locked() {
        let f = ContextEngineFactory::new(2048);
        let mut ctx = f.manager_context(
            "You are the Manager.".to_string(),
            "Ship the feature.".to_string(),
        );
        // Manager prefix: [0]=system, [1]=goal (REQ-CORE-001/002).
        assert!(matches!(ctx.messages()[0], Message::System { .. }));
        assert!(matches!(ctx.messages()[1], Message::User { .. }));

        // Appending turns must never move the pinned prefix.
        ctx.append(Message::User {
            content: "steer".to_string(),
        });
        assert_eq!(
            match &ctx.messages()[0] {
                Message::System { content } => content,
                _ => "",
            },
            "You are the Manager."
        );
        assert_eq!(
            match &ctx.messages()[1] {
                Message::User { content } => content,
                _ => "",
            },
            "Ship the feature."
        );
    }

    #[test]
    fn test_context_factory_specialist_isolated_prefix() {
        let f = ContextEngineFactory::new(2048);
        let mut spec = f.specialist_context(
            "You are the Coder specialist.".to_string(),
            "Implement the parser.".to_string(),
        );
        // Exactly two seeded messages: role at [0], brief at [1] (REQ-ORCH-003).
        assert_eq!(spec.messages().len(), 2);
        match &spec.messages()[0] {
            Message::System { content } => assert_eq!(content, "You are the Coder specialist."),
            _ => panic!("specialist messages[0] must be the role system prompt"),
        }
        match &spec.messages()[1] {
            Message::User { content } => assert_eq!(content, "Implement the parser."),
            _ => panic!("specialist messages[1] must be the task brief goal"),
        }

        // Isolation: append local work; [0]/[1] stay pinned.
        spec.append(Message::Assistant {
            content: Some("on it".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        });
        assert!(matches!(spec.messages()[0], Message::System { .. }));
        assert!(matches!(spec.messages()[1], Message::User { .. }));
    }

    #[test]
    fn test_context_factory_specialists_isolated_from_each_other() {
        let f = ContextEngineFactory::new(2048);
        // Two distinct specialists get fully independent, prefixed engines.
        let mut coder = f.specialist_context("Coder role".to_string(), "build".to_string());
        let researcher =
            f.specialist_context("Researcher role".to_string(), "research".to_string());
        coder.append(Message::User {
            content: "coder-only turn".to_string(),
        });
        // The researcher engine must NOT see the coder's appended history.
        assert_eq!(
            researcher.messages().len(),
            2,
            "each specialist is freshly isolated"
        );
        assert!(matches!(researcher.messages()[0], Message::System { .. }));
        assert!(matches!(researcher.messages()[1], Message::User { .. }));
        let researcher_goal = match &researcher.messages()[1] {
            Message::User { content } => content,
            _ => "",
        };
        assert_eq!(researcher_goal, "research");
    }

    /// Build a transcript that is well over the given budget so compaction
    /// actually removes messages.
    fn fill_over_budget(max_tokens: usize) -> ContextEngine {
        let mut engine = ContextEngine::new(max_tokens);
        engine.set_system_prompt("You are a helpful coding assistant.".to_string());
        engine.set_goal("Refactor the parser module.".to_string());
        for i in 0..20 {
            engine.append(Message::User {
                content: format!(
                    "Turn {i}: please make the following verbose change to the file with lots of padding text to consume many tokens."
                ),
            });
            engine.append(Message::Assistant {
                content: Some(format!(
                    "Understood, working on turn {i} now with a lengthy verbose reply full of detail."
                )),
                reasoning_content: None,
                tool_calls: vec![],
            });
        }
        engine
    }

    /// Build a transcript whose token count lands strictly between the 90%
    /// trigger threshold and the hard `max_tokens` limit, so the retry-1 ratio
    /// path (70%) is exercised rather than the over-limit 80% path.
    fn fill_in_trigger_window(max_tokens: usize) -> ContextEngine {
        let mut engine = ContextEngine::new(max_tokens);
        engine.set_system_prompt("You are a helpful coding assistant.".to_string());
        engine.set_goal("Refactor the parser module.".to_string());
        let mut i = 0usize;
        loop {
            engine.append(Message::User {
                content: format!(
                    "Turn {i}: please make the following verbose change to the file with lots of padding text to consume many tokens."
                ),
            });
            engine.append(Message::Assistant {
                content: Some(format!(
                    "Understood, working on turn {i} now with a lengthy verbose reply full of detail."
                )),
                reasoning_content: None,
                tool_calls: vec![],
            });
            i += 1;
            let n = engine.token_count();
            if n > compaction_threshold(max_tokens) {
                // Stop once we cross the 90% trigger; the transcript must stay
                // under the hard limit for the ratio path to apply.
                assert!(
                    n <= max_tokens,
                    "test transcript must stay under the hard limit (got {n} > {max_tokens})"
                );
                break;
            }
        }
        engine
    }

    /// Token count of the transcript excluding the injected
    /// `SYSTEM: CONTEXT LIMIT EXCEEDED` message, so compaction targets can be
    /// asserted independently of the post-compaction injection.
    fn count_without_injection(engine: &ContextEngine) -> usize {
        count_tokens(
            &engine
                .messages()
                .iter()
                .filter(|m| {
                    !matches!(m, Message::User { content } if content == CONTEXT_LIMIT_EXCEEDED_MESSAGE)
                })
                .cloned()
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn test_compaction_retry_escalation_70_then_50() {
        // Budget 1000 gives a 100-token trigger window (90%..100%), wide enough
        // for the ~48-token per-turn increment to land inside it.
        let mut engine = fill_in_trigger_window(1000);
        let limit = engine.max_context_tokens();

        // Retry 1: ratio path targets 70% of the pre-compaction token count.
        let pre1 = engine.token_count();
        let compacted = engine.compact_with_retry(limit);
        assert!(compacted, "first retry must compact");
        assert_eq!(engine.compaction_retry_count(), 1);
        let target1 = ((pre1.min(limit) as f64) * COMPACTION_RETRY1_RATIO).round() as usize;
        assert!(
            count_without_injection(&engine) <= target1,
            "retry 1 should bring transcript to <= 70% of pre-compaction count (got {}, target {})",
            count_without_injection(&engine),
            target1
        );

        // The CONTEXT LIMIT EXCEEDED injection must be present after success.
        assert!(
            engine
                .messages()
                .iter()
                .any(|m| matches!(m, Message::User { content } if content == CONTEXT_LIMIT_EXCEEDED_MESSAGE)),
            "CONTEXT LIMIT EXCEEDED injection must be appended after a successful compaction"
        );

        // Re-fill to force a second retry (back into the trigger window).
        let mut i = 0usize;
        while engine.token_count() <= compaction_threshold(limit) {
            engine.append(Message::User {
                content: format!(
                    "More turn {i}: please make the following verbose change to the file with lots of padding text to consume many tokens."
                ),
            });
            engine.append(Message::Assistant {
                content: Some(format!(
                    "Understood, working on more turn {i} now with a lengthy verbose reply full of detail."
                )),
                reasoning_content: None,
                tool_calls: vec![],
            });
            i += 1;
        }

        // Retry 2: ratio path targets 50% of the pre-compaction token count.
        let pre2 = engine.token_count();
        let compacted2 = engine.compact_with_retry(limit);
        assert!(compacted2, "second retry must compact");
        assert_eq!(engine.compaction_retry_count(), 2);
        let target2 = ((pre2.min(limit) as f64) * COMPACTION_RETRY2_RATIO).round() as usize;
        assert!(
            count_without_injection(&engine) <= target2,
            "retry 2 should bring transcript to <= 50% of pre-compaction count (got {}, target {})",
            count_without_injection(&engine),
            target2
        );

        // Third attempt is gated by `compaction_retry_count < 2`.
        let compacted3 = engine.compact_with_retry(limit);
        assert!(!compacted3, "retry cap reached: no further compaction");
        assert_eq!(engine.compaction_retry_count(), 2);

        // Reset restores the counter.
        engine.reset_compaction_retry_count();
        assert_eq!(engine.compaction_retry_count(), 0);
    }

    #[test]
    fn test_compaction_retry1_over_limit_uses_80_percent() {
        // When retry 1 finds the transcript over the hard limit, it uses the
        // 80%-of-limit target (caesar `compact_context`).
        let mut engine = fill_over_budget(200);
        let limit = engine.max_context_tokens();
        assert!(
            engine.token_count() > limit,
            "test transcript must exceed the hard limit"
        );

        let compacted = engine.compact_with_retry(limit);
        assert!(compacted);
        assert_eq!(engine.compaction_retry_count(), 1);
        let target = ((limit as f64) * COMPACTION_OVER_LIMIT_TARGET_RATIO).round() as usize;
        assert!(
            count_without_injection(&engine) <= target,
            "retry 1 over-limit should bring transcript to <= 80% of limit (got {}, target {})",
            count_without_injection(&engine),
            target
        );
    }

    #[test]
    fn test_compaction_retry_prunes_orphan_tools() {
        let mut engine = ContextEngine::new(120);
        engine.set_system_prompt("You are a helpful coding assistant.".to_string());
        engine.set_goal("Refactor the parser module.".to_string());

        // A valid assistant->tool pair, plus an orphaned tool message whose
        // assistant call will be dropped during compaction.
        engine.append(assistant_with_tool("call_abc"));
        engine.append(tool_response("call_abc"));
        engine.append(tool_response("call_orphan")); // no matching assistant

        // Force the transcript well over the trigger threshold.
        for i in 0..12 {
            engine.append(Message::User {
                content: format!(
                    "This is a fairly verbose user instruction number {i} with padding text to consume many tokens."
                ),
            });
            engine.append(Message::Assistant {
                content: Some(format!(
                    "Assistant acknowledging instruction {i} with a lengthy verbose reply full of detail."
                )),
                reasoning_content: None,
                tool_calls: vec![],
            });
        }

        assert!(
            engine.should_compact(),
            "transcript should exceed 90% budget"
        );

        let compacted = engine.compact_with_retry(engine.max_context_tokens());
        assert!(compacted, "retry compaction must succeed");

        // No orphaned tool message survives.
        let tool_ids: Vec<String> = engine
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !tool_ids.iter().any(|id| id == "call_orphan"),
            "orphaned tool message must be removed"
        );

        // Pins survive.
        assert!(matches!(engine.messages()[0], Message::System { .. }));
        assert!(matches!(engine.messages()[1], Message::User { .. }));
    }

    #[test]
    fn test_compaction_retry_cap_blocks_third_attempt() {
        let mut engine = fill_over_budget(200);
        let limit = engine.max_context_tokens();

        // Two successful retries consume the cap.
        assert!(engine.compact_with_retry(limit));
        assert!(engine.compact_with_retry(limit));
        assert_eq!(engine.compaction_retry_count(), 2);

        // A third attempt is refused without mutating the transcript.
        let before = engine.messages().len();
        assert!(!engine.compact_with_retry(limit));
        assert_eq!(engine.messages().len(), before);
    }

    #[test]
    fn test_slow_prefill_threshold_is_inclusive_at_300s() {
        // REQ-CORE-005 / caesar `record_backend_duration`: a prefill of exactly
        // 300s is a slow prefill (`duration_secs >= 300`).
        let mut t = SlowPrefillTracker::new();
        // Two consecutive slow prefills at exactly the 300s boundary, after the
        // rebirth cooldown has elapsed, must emit a warning.
        t.note_rebirth();
        for _ in 0..MIN_TURNS_AFTER_REBIRTH {
            t.record_prefill(Duration::from_secs(1)); // fast recovery turns
        }
        // First 300s call: consecutive_slow = 1, so no emit yet.
        assert!(
            !t.record_prefill(Duration::from_secs(300)),
            "300s must count as slow but needs 2 consecutive to emit"
        );
        // Second consecutive 300s call: emits.
        assert!(
            t.record_prefill(Duration::from_secs(300)),
            "2nd consecutive 300s prefill must emit"
        );
    }

    #[test]
    fn test_slow_prefill_requires_two_consecutive_slow() {
        // caesar `record_backend_duration`: a single slow prefill must NOT emit;
        // only the 2nd consecutive slow prefill on the same model does.
        let mut t = SlowPrefillTracker::new();
        t.note_rebirth();
        for _ in 0..MIN_TURNS_AFTER_REBIRTH {
            t.record_prefill(Duration::from_secs(1));
        }
        assert!(
            !t.record_prefill(Duration::from_secs(400)),
            "first slow prefill must not emit (needs 2 consecutive)"
        );
        assert!(
            t.record_prefill(Duration::from_secs(400)),
            "second consecutive slow prefill must emit"
        );
    }

    #[test]
    fn test_slow_prefill_cooldown_boundary_suppresses_turns_1_to_4() {
        // REQ-CORE-005 / caesar `record_backend_duration`: within the first
        // `MIN_TURNS_AFTER_REBIRTH` (5) turns after a rebirth, slow-prefill
        // warnings are suppressed. Turns 1..4 must be suppressed; turn 5+ emits.
        let mut t = SlowPrefillTracker::new();
        t.note_rebirth(); // turns_since_rebirth = 0

        // Turns 1..4: each `record_prefill` advances the recovery-turn counter,
        // so turns 1-4 stay below the cooldown and are suppressed.
        for turn in 1..MIN_TURNS_AFTER_REBIRTH {
            assert!(
                !t.record_prefill(Duration::from_secs(400)),
                "turn {turn} (within cooldown) must suppress slow-prefill warnings"
            );
        }

        // Turn 5 (== MIN_TURNS_AFTER_REBIRTH): cooldown elapsed, warning emits.
        assert!(
            t.record_prefill(Duration::from_secs(400)),
            "turn 5 (cooldown elapsed) must emit a slow-prefill warning"
        );
    }

    #[test]
    fn test_slow_prefill_fast_prefill_resets_consecutive_count() {
        // caesar `record_backend_duration`: a fast prefill (< 300s) resets the
        // consecutive-slow counter, so a later slow prefill starts over at 1.
        let mut t = SlowPrefillTracker::new();
        t.note_rebirth();
        for _ in 0..MIN_TURNS_AFTER_REBIRTH {
            t.record_prefill(Duration::from_secs(1));
        }
        assert!(!t.record_prefill(Duration::from_secs(400))); // slow #1
        assert!(!t.record_prefill(Duration::from_secs(1))); // fast resets
        assert!(
            !t.record_prefill(Duration::from_secs(400)),
            "after a reset, a single slow prefill must not emit"
        );
        assert!(
            t.record_prefill(Duration::from_secs(400)),
            "second consecutive slow after reset must emit"
        );
    }
}
