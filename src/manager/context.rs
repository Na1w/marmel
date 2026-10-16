//! Context engine: token counting (cl100k_base BPE), compaction, and rebirth.
//!
//! - The system prompt is locked at `messages[0]` and the user goal at `messages[1]`;
//!   compaction and rebirth never move, prepend to, remove or alter either one.
//! - Compaction triggers above 90% of the budget and targets 70%. The assistant
//!   `tool_calls` ↔ `Tool` pairing invariant is enforced in **both** directions (orphan
//!   `role:"tool"` messages pruned; a missing result synthesized as `"(aborted)"`,
//!   [`repair_tool_call_pairs`]), so a compacted transcript is always a valid
//!   OpenAI-compatible request. The pinned window is charged against that target: a
//!   pinned prefix alone over 70% yields [`CompactionOutcome::TargetUnreachable`], and
//!   reported deltas are measured on the final vector.
//! - Forced rebirth collapses the transcript to `[0]` system prompt, `[1]` pinned goal,
//!   `[2]` the last genuine user instruction (never an injected advisory; omitted when it
//!   would duplicate `[1]`) and a `SYSTEM: REBIRTH CHECKPOINT` line emitted as `User`.
//! - The budget is measured on the whole **request**: [`ContextEngine::should_compact`]
//!   compares [`ContextEngine::request_token_count`] (transcript plus the tool-schema
//!   payload, [`tools_tokens`]) against the same 90% trigger, and the caller that sends
//!   the request owns that charge via [`ContextEngine::set_tools`] with exactly the list
//!   it wires ([`manager_wire_tools`] =
//!   [`crate::types::ToolDef::manager_tools()`] plus the policy-filtered MCP view). Only
//!   a caller that declares nothing at all ends up message-only, and then that count is
//!   a **lower bound** on the real request.

use std::sync::Arc;

use crate::harness::HarnessStats;
use crate::session::format::{
    DanglingToolCallState, ManagerSection, MessageRecord, PinWitness, SESSION_RESTORE_NOTICE_PREFIX,
};
use crate::types::Message;

/// The exact system checkpoint content injected at `messages[3]` on rebirth.
pub const REBIRTH_CHECKPOINT_PREFIX: &str = "(SYSTEM: REBIRTH CHECKPOINT. The previous turn-by-turn history has been compacted. Summarized progress: ";

/// Rebirth advisory trigger ratio (80% of budget).
pub const REBIRTH_ADVISORY_TRIGGER_RATIO: f64 = 0.80;
/// Compaction trigger ratio (90% of budget).
pub const COMPACTION_TRIGGER_RATIO: f64 = 0.90;
/// Compaction target ratio (70% of budget).
pub const COMPACTION_TARGET_RATIO: f64 = 0.70;

/// The exact `SYSTEM: CONTEXT BUDGET ADVISORY` message injected when context reaches 80% of budget.
pub const REBIRTH_ADVISORY_MESSAGE: &str = "(SYSTEM: CONTEXT BUDGET ADVISORY. You have reached 80% of your context budget. You should summarize your progress, key findings, and immediate next steps, then invoke the 'rebirth' tool with your summary before forced context compaction occurs. IMPORTANT: To prevent repeating work, your summary MUST preserve all vital operational state—such as active file paths, exact line numbers or byte offsets reached (e.g. in read_file), intermediate discoveries, and precise next steps so you resume seamlessly without starting over from the beginning.)";

/// BPE tokenizer singleton (cl100k_base).
///
/// The single place in the crate that touches `tiktoken_rs::cl100k_base_singleton()`;
/// every other token count must go through [`count_text_tokens`] (or the
/// message-level helpers built on top of it).
fn bpe() -> &'static tiktoken_rs::CoreBPE {
    tiktoken_rs::cl100k_base_singleton()
}

/// Count the BPE tokens in a free-form text string (cl100k_base).
///
/// Uses `encode_ordinary` (no special tokens), matching [`count_tokens`],
/// [`count_assistant_tokens`] and [`message_tokens`]. Returns `0` for empty
/// input and is deterministic across calls.
pub fn count_text_tokens(text: &str) -> usize {
    bpe().encode_ordinary(text).len()
}

/// Count the total BPE tokens in a message transcript.
///
/// Uses `encode_ordinary` (no special tokens) and adds a small per-message
/// framing overhead, mirroring the standard OpenAI cookbook approximation.
pub fn count_tokens(messages: &[Message]) -> usize {
    messages.iter().map(message_tokens).sum()
}

/// Count BPE tokens for an assistant turn's content, reasoning, and tool calls.
pub fn count_assistant_tokens(
    content: Option<&str>,
    reasoning: Option<&str>,
    tool_calls: &[crate::types::ToolCall],
) -> usize {
    content.map_or(0, count_text_tokens)
        + reasoning.map_or(0, count_text_tokens)
        + tool_calls
            .iter()
            .map(|tc| {
                1 + count_text_tokens(&tc.function.name) + count_text_tokens(&tc.function.arguments)
            })
            .sum::<usize>()
}

/// BPE token count of a single message: 3 tokens framing overhead plus the
/// encoded lengths of its content, reasoning, tool calls, or tool content.
fn message_tokens(m: &Message) -> usize {
    3 + match m {
        Message::System { content } | Message::User { content } => count_text_tokens(content),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
        } => count_assistant_tokens(content.as_deref(), reasoning_content.as_deref(), tool_calls),
        Message::Tool { content, .. } => count_text_tokens(content),
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

/// Per-tool-definition framing overhead charged by the provider for the
/// `{"type":"function","function":{…}}` wrapper around every schema.
///
/// Mirrors the per-message framing constant in [`message_tokens`]: the wire object
/// costs a few tokens beyond its serialized body.
pub const TOOL_DEF_FRAMING_TOKENS: usize = 8;

/// The tool names the Manager stream sends by default, i.e. the name set of
/// [`crate::types::ToolDef::manager_tools()`], taken from [`crate::tool_names`]
/// so the budget model and the dispatcher can never drift apart on a typo.
///
/// Used by [`tools_tokens`] tests as the drift guard for the request payload
/// actually produced by `src/llm/stream.rs::build_request`.
pub const MANAGER_TOOL_NAMES: &[&str] = &[
    crate::tool_names::TOOL_DELEGATE_TASK,
    crate::tool_names::TOOL_CREATE_PLAN,
    crate::tool_names::TOOL_ARCHIVE_PLAN,
    crate::tool_names::TOOL_READ_FILE,
    crate::tool_names::TOOL_GREP_SEARCH,
    crate::tool_names::TOOL_GLOB,
    crate::tool_names::TOOL_REBIRTH,
    crate::tool_names::TOOL_SLEEP,
    crate::tool_names::TOOL_REPLY_TO_ARBITRATOR,
];

/// BPE token count of the **tool-schema payload** of a chat request.
///
/// Every request carries `tools: Option<Vec<ToolDef>>` (see
/// `src/llm/stream.rs::build_request` and
/// `src/agents/runner/fix_loop.rs::build_turn_request`), and the budget charges that
/// payload, not only the message array.
///
/// Each definition is charged as its `type` tag, function name, description and the
/// JSON serialization of its `parameters` schema, plus [`TOOL_DEF_FRAMING_TOKENS`].
/// The count is **additive and optional**: an empty (or `None`, i.e. no tools at all)
/// tool list contributes `0`.
pub fn tools_tokens(tools: &[crate::types::ToolDef]) -> usize {
    tools
        .iter()
        .map(|tool| {
            TOOL_DEF_FRAMING_TOKENS
                + count_text_tokens(&tool.kind)
                + count_text_tokens(&tool.function.name)
                + count_text_tokens(&tool.function.description)
                + count_text_tokens(
                    &serde_json::to_string(&tool.function.parameters).unwrap_or_default(),
                )
        })
        .sum()
}

/// BPE token count of a **complete chat request**: the transcript plus the tool
/// schemas sent with it.
///
/// Equal to [`count_tokens`] when `tools` is empty.
pub fn request_tokens(messages: &[Message], tools: &[crate::types::ToolDef]) -> usize {
    count_tokens(messages) + tools_tokens(tools)
}

/// The **exact** tool-schema payload a Manager request carries on the wire.
///
/// The wire owner is `src/llm/stream.rs::build_request`: every Manager request sends
/// [`crate::types::ToolDef::manager_tools()`] plus the *policy-filtered* MCP view of
/// the configured servers ([`crate::harness::allowed_mcp_tools`], so an MCP name the
/// tool policy does not allow is never advertised). The budget model has to charge
/// precisely that list — a superset would compact too early and hide transcript, a
/// subset would under-count and let an over-budget request through.
///
/// It is deliberately a function of the *server names only*: the MCP tool list itself
/// is process-wide, installed when the servers boot and changed when a server
/// (re)connects, so callers re-read it whenever they price a request (see
/// `src/ui/session.rs`).
pub fn manager_wire_tools(mcp_servers: &[String]) -> Vec<crate::types::ToolDef> {
    let mut tools = crate::types::ToolDef::manager_tools();
    for tool in crate::harness::allowed_mcp_tools(mcp_servers) {
        tools.push(crate::types::ToolDef::from_mcp(&tool));
    }
    tools
}

/// Exclusive end index of the pinned prefix of a transcript.
///
/// `messages[0]` (system prompt) and `messages[1]` (immutable goal) are always pinned.
/// If a `REBIRTH CHECKPOINT` exists in `messages[2..]`, the pinned window is extended
/// through it so distilled session state is never dropped during compaction.
///
/// The checkpoint is matched on its **content marker**, not its role:
/// `perform_rebirth` emits it as a `User` message, while a transcript installed
/// wholesale by [`ContextEngine::install_restored_messages`] can still carry the
/// `System` variant. The window is therefore pinned by content, and the installed bytes
/// MUST NOT be re-role'd or re-ordered.
fn pinned_prefix_end(messages: &[Message]) -> usize {
    messages
        .iter()
        .enumerate()
        .rposition(|(i, m)| {
            i >= 2
                && match m {
                    Message::System { content } | Message::User { content } => {
                        content.starts_with(REBIRTH_CHECKPOINT_PREFIX)
                    }
                    Message::Assistant { .. } | Message::Tool { .. } => false,
                }
        })
        .map(|idx| idx + 1)
        .unwrap_or(2)
}

/// Test-only witness: how many writes have gone through a *by-index* pin setter —
/// [`ContextEngine::set_system_prompt`] or [`ContextEngine::set_goal`].
///
/// A restore installs its vector wholesale through
/// [`ContextEngine::install_restored_messages`] and MUST NOT route through either
/// setter: both write `messages[0]`/`messages[1]` **by index**, so a restore that let
/// them run would replace the pinned bytes with the freshly composed prompt and goal.
/// The restore tests snapshot this witness around `install_restored_messages` and
/// require it to stay `0`, which turns that clobber path into a test failure instead of
/// a silent loss of both pins.
#[cfg(test)]
fn note_pin_setter_write(engine: &ContextEngine) {
    engine
        .pin_setter_calls
        .set(engine.pin_setter_calls.get() + 1);
}

/// Byte-length witnesses of the two pinned messages: `messages[0]` is the system
/// prompt, `messages[1]` the immutable goal.
///
/// Lengths, not hashes — cheap, and enough to name *which* pin broke instead of "the
/// transcript differs". The measurement is identical to
/// `session::format::restore::witness_of`, which measures the same pair on the durable
/// `MessageRecord` form: the two MUST agree, or the capture side would witness numbers
/// its own loader cannot reproduce. A transcript that does not carry `System` at `[0]`
/// / `User` at `[1]` witnesses `0`/`0` — capture stays total, and the loader refuses
/// the vector.
fn pin_witness_of(messages: &[Message]) -> PinWitness {
    let system_prompt_len = match messages.first() {
        Some(Message::System { content }) => content.len(),
        _ => 0,
    };
    let goal_len = match messages.get(1) {
        Some(Message::User { content }) => content.len(),
        _ => 0,
    };
    PinWitness {
        system_prompt_len: system_prompt_len as u32,
        goal_len: goal_len as u32,
    }
}

/// Content markers that identify an **injected** advisory/notice — a `Message::User`
/// the runtime put in the transcript itself rather than a turn the user or the model
/// produced. All of them are synthesized by this crate with a fixed leading marker:
///
/// - [`REBIRTH_ADVISORY_MESSAGE`] — the `SYSTEM: CONTEXT BUDGET ADVISORY` injection
///   ([`ContextEngine::inject_rebirth_advisory`]).
/// - `SYSTEM: CONTEXT LIMIT EXCEEDED` — a notice older saved transcripts still contain.
/// - `SYSTEM: Rebirth checkpoint accepted` — the post-rebirth continuation injection
///   (`src/ui/session.rs`, `src/agents/runner/execution.rs`, `src/agents/validation.rs`).
/// - [`REBIRTH_CHECKPOINT_PREFIX`] — the rebirth checkpoint itself, never summarizable
///   as if it were a user instruction.
/// - [`SESSION_RESTORE_NOTICE_PREFIX`] — the one synthetic restore notice a rehydrated
///   session appends ([`ContextEngine::append_restore_notice`]), so `perform_rebirth`
///   can never promote it into the `messages[2]` slot instead of the last genuine
///   instruction.
///
/// [`ContextEngine::perform_rebirth`] uses this list so the rebirth `messages[2]` slot
/// carries the last **genuine** instruction. **The text of these markers is
/// load-bearing:** [`is_injected_advisory`] is a pure `starts_with` scan and the pinned
/// window ([`pinned_prefix_end`]) is found by a content match on
/// [`REBIRTH_CHECKPOINT_PREFIX`] — trimming, re-wrapping, normalizing, rewording or
/// translating one of these prefixes silently moves the pinned window and makes a
/// runtime notice look like a genuine user instruction. A restore loader MUST treat
/// them as opaque bytes.
pub const INJECTED_ADVISORY_PREFIXES: &[&str] = &[
    "(SYSTEM: CONTEXT BUDGET ADVISORY",
    "(SYSTEM: CONTEXT LIMIT EXCEEDED",
    "(SYSTEM: Rebirth checkpoint accepted",
    REBIRTH_CHECKPOINT_PREFIX,
    SESSION_RESTORE_NOTICE_PREFIX,
];

/// Whether `content` is one of the runtime-injected advisory/notice messages
/// listed in [`INJECTED_ADVISORY_PREFIXES`].
pub fn is_injected_advisory(content: &str) -> bool {
    INJECTED_ADVISORY_PREFIXES
        .iter()
        .any(|prefix| content.starts_with(prefix))
}

/// Placeholder content synthesized as the `role:"tool"` result for a
/// `tool_calls` entry whose real result was dropped by compaction or never
/// produced (e.g. the UI aborted the tool loop).
///
/// Keeping a placeholder — instead of deleting the assistant turn — is what
/// makes the transcript a valid OpenAI-compatible chat request while preserving
/// the model's own intent; see [`repair_tool_call_pairs`].
pub const ABORTED_TOOL_RESULT: &str = "(aborted)";

/// Prune orphaned `role:"tool"` messages.
///
/// A tool message is an orphan if its `tool_call_id` does not correspond to a
/// `tool_calls` entry on a *surviving* assistant message. Orphans are dropped so no
/// invalid tool response is ever sent upstream.
///
/// This is only **one half** of the pairing invariant: it removes tool results that
/// outlive their parent. The reverse half (an assistant `tool_calls` entry whose result
/// is missing) is enforced by [`repair_tool_call_pairs`], which reuses this pass rather
/// than duplicating it.
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

/// Enforce **both** directions of the assistant `tool_calls` ↔ `Tool` pairing
/// invariant so the sequence is always valid for an OpenAI-compatible
/// `/chat/completions` request:
///
/// 1. **No orphan `Tool`** — a `role:"tool"` message whose `tool_call_id` has no
///    surviving assistant parent is dropped. This half reuses
///    [`prune_orphan_tool_messages`] instead of re-implementing it, and also drops
///    duplicate results for the same id (keeping the first).
/// 2. **No dangling `tool_calls`** — every `tool_calls` entry of a surviving assistant
///    message is followed by exactly one `Message::Tool` carrying its id, in
///    `tool_calls` order. Missing results are synthesized as
///    `Message::Tool { content: ABORTED_TOOL_RESULT }`.
///
/// Existing results are *moved* so they sit directly after their parent assistant;
/// nothing else is reordered, so `messages[0]` (system prompt) and `messages[1]`
/// (pinned goal) keep their positions — they can only ever be `System`/`User` messages,
/// which this pass never moves. A placeholder is synthesized instead of dropping the
/// assistant turn: that newest assistant message can hold the model's most recent
/// intent, and inside the pinned window it can hold a turn retention deliberately kept,
/// so only the (nonexistent) tool output is lost and the placeholder stays
/// self-describing to the model.
pub fn repair_tool_call_pairs(messages: Vec<Message>) -> Vec<Message> {
    // Half 1: drop orphan tool results (and, by construction of the map below,
    // any duplicate result for an id that is already accounted for).
    let messages = prune_orphan_tool_messages(messages);

    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in &messages {
        if let Message::Tool {
            tool_call_id,
            content,
        } = m
        {
            results
                .entry(tool_call_id.clone())
                .or_insert_with(|| content.clone());
        }
    }

    // Half 2: re-emit every tool result directly under its parent assistant and
    // synthesize `"(aborted)"` for the calls that have no result at all.
    let mut repaired: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            Message::Tool { .. } => {}
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                let synthesized: Vec<Message> = tool_calls
                    .iter()
                    .map(|tc| Message::Tool {
                        tool_call_id: tc.id.clone(),
                        content: results
                            .get(&tc.id)
                            .cloned()
                            .unwrap_or_else(|| ABORTED_TOOL_RESULT.to_string()),
                    })
                    .collect();
                repaired.push(Message::Assistant {
                    content,
                    reasoning_content,
                    tool_calls,
                });
                repaired.extend(synthesized);
            }
            other => repaired.push(other),
        }
    }
    repaired
}

/// Cheap structural fingerprint of a transcript: the role tag of every message
/// plus the `tool_call_id`s it carries, in order.
///
/// Used only by [`ContextEngine::ensure_tool_call_pairs`] to decide whether the
/// repair **changed** anything, so a caller can persist/report exactly once per
/// real repair. Message *content* is deliberately not part of the shape:
/// [`repair_tool_call_pairs`] never edits content — it prunes, re-groups and
/// synthesizes messages, and all three are visible in this shape.
fn pairing_shape(messages: &[Message]) -> Vec<(&'static str, Vec<String>)> {
    messages
        .iter()
        .map(|m| match m {
            Message::System { .. } => ("system", Vec::new()),
            Message::User { .. } => ("user", Vec::new()),
            Message::Assistant { tool_calls, .. } => (
                "assistant",
                tool_calls.iter().map(|tc| tc.id.clone()).collect(),
            ),
            Message::Tool { tool_call_id, .. } => ("tool", vec![tool_call_id.clone()]),
        })
        .collect()
}

/// Outcome of [`ContextEngine::compact`] — the caller must be able to tell a
/// compaction that actually reached the 70% target from one that could not.
///
/// The counters are always measured on the **final** message vector: after the
/// retention window, after the pairing repair (orphan pruning + placeholder synthesis)
/// and after the bounded re-trim. Window arithmetic from an earlier stage is stale
/// whenever pruning or synthesis changes the vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionOutcome {
    /// The transcript now fits the 70% target.
    Compacted {
        /// Tokens counted before compaction.
        initial_tokens: usize,
        /// Tokens counted on the final vector.
        final_tokens: usize,
        /// `initial_tokens - final_tokens`.
        tokens_reclaimed: usize,
        /// Messages dropped, as the true delta of the final vector
        /// (`messages_before - messages_after`).
        messages_removed: usize,
        /// The 70% target the transcript was brought under.
        target: usize,
    },
    /// The 70% target is **unreachable**: the pinned prefix
    /// (`messages[0]`/`messages[1]`, plus a pinned `REBIRTH CHECKPOINT`, see
    /// [`pinned_prefix_end`]) is sacrosanct yet already costs more than the target.
    /// Compaction trims everything it is allowed to trim and then reports this failure
    /// instead of pretending the transcript fits the budget.
    TargetUnreachable {
        /// Tokens of the pinned prefix that alone exceed the target.
        pinned_tokens: usize,
        /// Number of pinned messages.
        pinned_messages: usize,
        target: usize,
        /// Tokens counted before compaction.
        initial_tokens: usize,
        /// Tokens left after trimming everything removable (still `> target`).
        final_tokens: usize,
        /// `initial_tokens - final_tokens` (the trimming that *was* possible).
        tokens_reclaimed: usize,
        /// Messages dropped while trimming (still not enough).
        messages_removed: usize,
    },
}

impl CompactionOutcome {
    /// `true` only when the transcript really fits the target.
    pub fn succeeded(&self) -> bool {
        matches!(self, CompactionOutcome::Compacted { .. })
    }

    pub fn messages_removed(&self) -> usize {
        match self {
            CompactionOutcome::Compacted {
                messages_removed, ..
            }
            | CompactionOutcome::TargetUnreachable {
                messages_removed, ..
            } => *messages_removed,
        }
    }

    pub fn tokens_reclaimed(&self) -> usize {
        match self {
            CompactionOutcome::Compacted {
                tokens_reclaimed, ..
            }
            | CompactionOutcome::TargetUnreachable {
                tokens_reclaimed, ..
            } => *tokens_reclaimed,
        }
    }

    pub fn target(&self) -> usize {
        match self {
            CompactionOutcome::Compacted { target, .. }
            | CompactionOutcome::TargetUnreachable { target, .. } => *target,
        }
    }

    /// Token count of the transcript after the call.
    pub fn final_tokens(&self) -> usize {
        match self {
            CompactionOutcome::Compacted { final_tokens, .. }
            | CompactionOutcome::TargetUnreachable { final_tokens, .. } => *final_tokens,
        }
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
    /// Whether a rebirth advisory has already been emitted for the current context window.
    rebirth_advisory_emitted: bool,
    /// Recovery turns elapsed since the last rebirth without another rebirth attempt.
    consecutive_rebirths: usize,
    /// Cached BPE token count of the tool schemas sent with this engine's requests.
    /// `0` means "no tool payload accounted for", i.e. message-only accounting.
    tool_schema_tokens: usize,
    /// Test-only witness: counts every write that went through a by-index pin setter
    /// (`set_system_prompt` / `set_goal`). See [`ContextEngine::pin_setter_calls`].
    #[cfg(test)]
    pin_setter_calls: std::cell::Cell<u64>,
}

impl ContextEngine {
    pub fn new(max_context_tokens: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_context_tokens,
            stats: None,
            rebirth_advisory_emitted: false,
            consecutive_rebirths: 0,
            tool_schema_tokens: 0,
            #[cfg(test)]
            pin_setter_calls: std::cell::Cell::new(0),
        }
    }

    /// Attach the shared stats registry so compaction/rebirth can record
    /// interventions.
    pub fn set_stats(&mut self, stats: Arc<HarnessStats>) {
        self.stats = Some(stats);
    }

    /// How many writes this engine has taken through a by-index pin setter
    /// (`set_system_prompt` / `set_goal`).
    ///
    /// Test-only witness: a capture→restore round trip must leave this at the value it
    /// had before [`ContextEngine::install_restored_messages`] ran.
    #[cfg(test)]
    pub fn pin_setter_calls(&self) -> u64 {
        self.pin_setter_calls.get()
    }

    /// Lock the system prompt at `messages[0]`.
    ///
    /// A **by-index write**: it belongs to the fresh-boot path only. The restore path
    /// MUST NOT call it — the test-only `pin_setter_calls` witness counts every call, so
    /// the restore tests can prove the seam never routes through here.
    pub fn set_system_prompt(&mut self, prompt: String) {
        #[cfg(test)]
        note_pin_setter_write(self);
        if self.messages.is_empty() {
            self.messages.push(Message::System { content: prompt });
        } else {
            self.messages[0] = Message::System { content: prompt };
        }
    }

    /// Pin the goal at `messages[1]`.
    ///
    /// If the transcript has no system prompt yet, a placeholder is inserted first to
    /// keep `messages[0]`/`messages[1]` indexing stable.
    ///
    /// Both halves are forbidden on the restore branch: the write is by index, and the
    /// empty-`System` padding would paper over what the loader is required to refuse — a
    /// restored transcript with no real system prompt is a `Refused(PinViolation)`, not
    /// something to pad.
    pub fn set_goal(&mut self, goal: String) {
        #[cfg(test)]
        note_pin_setter_write(self);
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

    pub fn token_count(&self) -> usize {
        count_tokens(&self.messages)
    }

    /// Declare the tool schemas this engine's requests will carry, so the budget model
    /// measures the **real** request size.
    ///
    /// `tools` must be exactly the list the next request sends — the same
    /// `Vec<ToolDef>` handed to `ChatRequest.tools` — because the count is cached and
    /// reused by every later budget decision. Pass an empty slice to go back to
    /// message-only accounting. A caller whose advertised list can change mid-life (MCP
    /// servers connecting later, per-run role filtering) re-declares it at every budget
    /// decision instead of once at construction.
    pub fn set_tools(&mut self, tools: &[crate::types::ToolDef]) {
        self.tool_schema_tokens = tools_tokens(tools);
    }

    /// Cached token count of the tool-schema payload (see [`ContextEngine::set_tools`]).
    pub fn tool_schema_tokens(&self) -> usize {
        self.tool_schema_tokens
    }

    /// Token count of the request this engine would send right now: transcript plus
    /// tool schemas. Identical to [`ContextEngine::token_count`] when no tools were
    /// declared.
    pub fn request_token_count(&self) -> usize {
        self.token_count() + self.tool_schema_tokens
    }

    /// Whether compaction should trigger right now for a request carrying `tools`
    /// (> 90% of budget measured on transcript **plus** tool schemas).
    ///
    /// Equivalent to [`ContextEngine::should_compact`] after
    /// [`ContextEngine::set_tools(tools)`][ContextEngine::set_tools], without
    /// mutating the engine.
    pub fn should_compact_with_tools(&self, tools: &[crate::types::ToolDef]) -> bool {
        request_tokens(&self.messages, tools) > compaction_threshold(self.max_context_tokens)
    }

    /// Whether compaction should trigger right now (> 90% of budget).
    ///
    /// The measurement is the full request size — transcript plus the declared tool
    /// schemas — so a tool-heavy request cannot slip past the trigger. An engine with no
    /// declared tools is measured on its messages alone.
    pub fn should_compact(&self) -> bool {
        self.request_token_count() > compaction_threshold(self.max_context_tokens)
    }

    /// Whether a rebirth advisory should be emitted right now (> 80% of budget, not yet emitted, and no consecutive rebirth).
    pub fn should_advise_rebirth(&self) -> bool {
        !self.rebirth_advisory_emitted
            && self.consecutive_rebirths == 0
            && self.request_token_count() > rebirth_advisory_threshold(self.max_context_tokens)
    }

    pub fn consecutive_rebirths(&self) -> usize {
        self.consecutive_rebirths
    }

    pub fn reset_consecutive_rebirths(&mut self) {
        self.consecutive_rebirths = 0;
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

    pub fn rebirth_advisory_emitted(&self) -> bool {
        self.rebirth_advisory_emitted
    }

    /// Compact the transcript down to ~70% of the budget, always keeping
    /// `messages[0]` (System) and `messages[1]` (Goal): newest turn pairs first, older
    /// turns dropped once the target is reached. The assistant `tool_calls` ↔ `Tool`
    /// pairing invariant is then enforced over the **entire** retained sequence, pinned
    /// window included, so the result is always a valid OpenAI-compatible chat request
    /// ([`repair_tool_call_pairs`]).
    ///
    /// **Target safety.** The pinned window from [`pinned_prefix_end`] is charged against
    /// the target, and the pins may never be dropped: when the pinned prefix alone costs
    /// more than the 70% target, compaction trims everything it may trim and returns
    /// [`CompactionOutcome::TargetUnreachable`] instead of reporting success on an
    /// over-budget transcript, so the live loops at `src/ui/session.rs` and
    /// `src/agents/runner/{execution,fix_loop}.rs` stop re-compacting a context that can
    /// never fit (e.g. by asking for a rebirth). A re-trim that stops making progress
    /// ends the same way, which keeps that loop terminating.
    ///
    /// **Truthful accounting.** The reported message/token deltas are measured on the
    /// **final** vector — after the retention window, the pairing repair and the re-trim.
    pub fn compact(&mut self) -> CompactionOutcome {
        let initial_tokens = self.token_count();
        let initial_msgs = self.messages.len();
        let target = compaction_target(self.max_context_tokens);

        // The pinned window is charged against the target: it can never be dropped, so
        // it decides whether the target is reachable at all.
        let prefix_end = pinned_prefix_end(&self.messages).min(self.messages.len());
        let pinned_messages = prefix_end;
        let pinned_tokens = count_tokens(&self.messages[..prefix_end]);

        if pinned_tokens > target {
            let trimmed = repair_tool_call_pairs(self.messages[..prefix_end].to_vec());
            self.messages = trimmed;
            let final_tokens = self.token_count();
            tracing::warn!(
                "Context compaction could not reach target (automatic): pinned prefix of {pinned_tokens} tokens ({pinned_messages} msgs) already exceeds the {target}-token target; transcript is still {final_tokens} tokens (was {initial_tokens})"
            );
            if let Some(stats) = &self.stats {
                stats.record_compaction();
            }
            return CompactionOutcome::TargetUnreachable {
                pinned_tokens,
                pinned_messages,
                target,
                initial_tokens,
                final_tokens,
                tokens_reclaimed: initial_tokens.saturating_sub(final_tokens),
                messages_removed: initial_msgs.saturating_sub(self.messages.len()),
            };
        }

        let mut kept: Vec<Message> = self.messages.iter().take(prefix_end).cloned().collect();
        let tail: Vec<Message> = self.messages.iter().skip(prefix_end).cloned().collect();
        let mut kept_tail: Vec<Message> = Vec::new();
        // `total` starts at the pinned prefix cost (the pinned prefix is budgeted; the
        // tail only gets what the target leaves over it).
        let mut total = pinned_tokens;

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

        let mut repaired = repair_tool_call_pairs(kept);

        // Synthesizing placeholder results adds a few tokens on top of the
        // retention window, so re-trim the oldest *non-pinned* message until the
        // 70% target holds again (budget ratios/thresholds are unchanged). Each
        // drop is followed by a repair so a removed assistant never leaves an
        // orphaned `Tool` behind. The loop is bounded: a drop that does not
        // strictly shrink the transcript (the repair immediately re-synthesizes
        // what was dropped because its parent assistant sits inside the pinned
        // window) means no further progress is possible, so it stops and the
        // target is reported as unreachable instead of spinning.
        let mut unreachable = false;
        while count_tokens(&repaired) > target {
            let pinned = pinned_prefix_end(&repaired).min(repaired.len());
            if repaired.len() <= pinned {
                unreachable = true;
                break;
            }
            let before = count_tokens(&repaired);
            repaired.remove(pinned);
            repaired = repair_tool_call_pairs(repaired);
            if count_tokens(&repaired) >= before {
                unreachable = true;
                break;
            }
        }

        self.messages = repaired;

        let final_tokens = self.token_count();
        let final_msgs = self.messages.len();
        let messages_removed = initial_msgs.saturating_sub(final_msgs);
        let tokens_reclaimed = initial_tokens.saturating_sub(final_tokens);

        if unreachable {
            let pinned_end = pinned_prefix_end(&self.messages).min(self.messages.len());
            let pinned_tokens = count_tokens(&self.messages[..pinned_end]);
            tracing::warn!(
                "Context compaction could not reach target (automatic): nothing removable left; pinned prefix of {pinned_tokens} tokens ({pinned_end} msgs) exceeds the {target}-token target, transcript still {final_tokens} tokens (was {initial_tokens})"
            );
            if let Some(stats) = &self.stats {
                stats.record_compaction();
            }
            return CompactionOutcome::TargetUnreachable {
                pinned_tokens,
                pinned_messages: pinned_end,
                target,
                initial_tokens,
                final_tokens,
                tokens_reclaimed,
                messages_removed,
            };
        }

        tracing::info!(
            "Context compaction executed (automatic): {initial_tokens} tokens ({initial_msgs} msgs) -> {final_tokens} tokens ({final_msgs} msgs, target budget: {target}, removed: {messages_removed} msgs / {tokens_reclaimed} tokens)"
        );

        if self.token_count() <= rebirth_advisory_threshold(self.max_context_tokens) {
            self.rebirth_advisory_emitted = false;
        }

        if let Some(stats) = &self.stats {
            stats.record_compaction();
        }

        CompactionOutcome::Compacted {
            initial_tokens,
            final_tokens,
            tokens_reclaimed,
            messages_removed,
            target,
        }
    }

    /// Enforce the assistant `tool_calls` ↔ `Tool` pairing invariant on the **live**
    /// transcript and report whether anything had to be repaired.
    ///
    /// This is the idempotent public entry point for the invariant:
    /// [`repair_tool_call_pairs`] is otherwise reached only from
    /// [`ContextEngine::compact`], which only runs above 90% utilization, so a turn that
    /// ends early (session abort, user exit, wall-clock bound, failure budget) would be
    /// re-sent with an assistant `tool_calls` entry and no `role:"tool"` result for the
    /// calls that were skipped — a provider 400.
    ///
    /// **Idempotent:** on an already valid transcript this is a no-op returning `false`,
    /// so a call site can use it as a belt-and-braces guard on top of a boundary repair.
    ///
    /// **Grammar:** the pass itself is owned by [`repair_tool_call_pairs`] and is reused
    /// verbatim — this wrapper adds, drops or rewords no rule, it only applies that pass
    /// to `self.messages` in place.
    ///
    /// Returns `true` when the transcript was changed (a repair was needed), `false`
    /// when it already satisfied the invariant.
    pub fn ensure_tool_call_pairs(&mut self) -> bool {
        let before = pairing_shape(&self.messages);
        let repaired = repair_tool_call_pairs(std::mem::take(&mut self.messages));
        let changed = pairing_shape(&repaired) != before;
        self.messages = repaired;
        changed
    }

    /// Collapse the history into the rebirth checkpoint shape.
    ///
    /// - `messages[0]`: the static system prompt.
    /// - `messages[1]`: the original user goal.
    /// - `messages[2]`: the last **genuine** user instruction, when there is one.
    /// - last: the `SYSTEM: REBIRTH CHECKPOINT` injection, emitted as a **`User`**
    ///   message (strict providers reject a non-leading `system` turn). The
    ///   [`REBIRTH_CHECKPOINT_PREFIX`] text is unchanged and [`pinned_prefix_end`] keeps
    ///   pinning it through later compactions.
    ///
    /// Two history-hygiene rules:
    ///
    /// 1. Runtime-injected advisories/notices ([`INJECTED_ADVISORY_PREFIXES`]: the
    ///    `CONTEXT BUDGET ADVISORY`, the legacy `CONTEXT LIMIT EXCEEDED` notice, the
    ///    post-rebirth continuation injection, and an earlier checkpoint) are never
    ///    summarized as `messages[2]` — the real last instruction would be lost behind
    ///    boilerplate.
    /// 2. The `messages[2]` slot is **omitted** when the only candidate is the pinned
    ///    goal itself, so the goal appears exactly once instead of being duplicated at
    ///    `[1]` and `[2]`: rebirth collapses to 4 messages when a distinct instruction
    ///    exists and to 3 when it does not.
    ///
    /// Every emitted message is `System`/`User`, so no `tool_calls` entry and no orphan
    /// `Tool` can survive a rebirth: the pairing invariant holds by construction. The
    /// `session_rebirths` counter in the attached [`HarnessStats`] is incremented.
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

        // Determine the last user instruction distinct from the goal, skipping
        // every runtime-injected advisory/notice message.
        let last_user_instruction = self.messages.iter().skip(2).rev().find_map(|m| match m {
            Message::User { content } if *content != goal && !is_injected_advisory(content) => {
                Some(content.clone())
            }
            _ => None,
        });

        let checkpoint = format!("{REBIRTH_CHECKPOINT_PREFIX}{summary})");
        let mut collapsed = vec![
            Message::System { content: system },
            Message::User { content: goal },
        ];
        // Never duplicate the pinned goal at `[2]`.
        if let Some(instruction) = last_user_instruction {
            collapsed.push(Message::User {
                content: instruction,
            });
        }
        collapsed.push(Message::User {
            content: checkpoint,
        });
        self.messages = collapsed;

        let final_tokens = self.token_count();
        let final_msgs = self.messages.len();
        tracing::info!(
            "Context compaction executed (rebirth): {initial_tokens} tokens ({initial_msgs} msgs) -> {final_tokens} tokens ({final_msgs} msgs), summary: {summary}"
        );

        self.consecutive_rebirths = self.consecutive_rebirths.saturating_add(1);
        if self.token_count() <= rebirth_advisory_threshold(self.max_context_tokens) {
            self.rebirth_advisory_emitted = false;
        }
        if let Some(stats) = &self.stats {
            stats.record_rebirth();
        }
    }

    // ---------------------------------------------------------------------
    // Session capture / restore seams
    // ---------------------------------------------------------------------

    /// CAPTURE — the exact `ManagerSection` snapshot of this engine: the transcript
    /// **verbatim and in order**, plus the scalars that change the next request and the
    /// witnesses a restore verifies.
    ///
    /// A pure read: it never mutates, repairs, prunes, truncates, re-roles or re-orders
    /// `messages()`. The transcript is mapped through [`MessageRecord::from_wire`] field
    /// by field — never through a JSON round-trip of the wire type, because
    /// `types::wire::serialize_assistant_content` (`src/types/wire.rs`) writes
    /// `content: None` as `""`, erasing the difference between "the model produced no
    /// content" and "the model produced an empty string". `reasoning_content` and the
    /// verbatim `ToolCall.function.arguments` ride along unchanged: arguments are opaque
    /// bytes here, never re-serialized and never run through
    /// `ensure_valid_json_arguments` (that would *rewrite* history).
    ///
    /// `pinned_prefix_end` and `pin_witness` are witnesses computed from the very same
    /// vector, so a restore can prove it installed the transcript that was captured.
    pub fn capture_manager_section(&self) -> ManagerSection {
        let messages: Vec<MessageRecord> =
            self.messages.iter().map(MessageRecord::from_wire).collect();
        let pinned_prefix_end = pinned_prefix_end(&self.messages);
        let pin_witness = pin_witness_of(&self.messages);
        let dangling = DanglingToolCallState::observe(&messages);
        ManagerSection {
            messages,
            // `2` for an engine shorter than the pinned pair: the loader refuses such a
            // transcript rather than inventing a window.
            pinned_prefix_end: pinned_prefix_end as u32,
            pin_witness,
            max_context_tokens: self.max_context_tokens,
            rebirth_advisory_emitted: self.rebirth_advisory_emitted,
            consecutive_rebirths: self.consecutive_rebirths,
            dangling,
        }
    }

    /// RESTORE — install a restored transcript **wholesale**.
    ///
    /// The only sanctioned way a persisted manager transcript enters the engine, and it
    /// deliberately does **not** go through [`ContextEngine::set_system_prompt`] or
    /// [`ContextEngine::set_goal`]: both write **by index**, and the boot path calls them
    /// unconditionally (`src/ui/session.rs`: `build_manager_context` →
    /// `set_system_prompt`, then `set_goal` once a goal exists). A restore routed through
    /// them clobbers the two pins with freshly composed bytes — load-then-set overwrites
    /// `[0]`/`[1]`, set-then-load throws away the vector it just restored — silently
    /// losing the KV-cache prefix identity and the immutable goal while every pane still
    /// looks right.
    ///
    /// The vector is installed **as-is**: no role is changed, nothing is re-ordered,
    /// padded or rewritten. [`pinned_prefix_end`] identifies the pinned window by a
    /// content match on [`REBIRTH_CHECKPOINT_PREFIX`], and `compact()` keeps
    /// `messages[..pinned_prefix_end]` unconditionally, so a re-role or re-order silently
    /// moves that window and changes which turns survive the next compaction — and
    /// `perform_rebirth`, which re-reads `[0]`/`[1]` positionally, would collapse to a
    /// system-less, goal-less transcript.
    ///
    /// Panics when `messages` holds fewer than two records: that is a caller bug, because
    /// `session::restore` refuses such a bundle before anything is installed.
    pub fn install_restored_messages(&mut self, messages: Vec<MessageRecord>) {
        assert!(
            messages.len() >= 2,
            "install_restored_messages: a manager transcript must carry at least the pinned \
             pair (system prompt + goal); the loader refuses anything shorter (A §4 R-PIN-5), \
             got {} record(s)",
            messages.len()
        );
        self.messages = messages.into_iter().map(MessageRecord::into_wire).collect();
    }

    /// Restore the non-message engine scalars captured by
    /// [`ContextEngine::capture_manager_section`].
    ///
    /// Direct field assignment on purpose: [`ContextEngine::inject_rebirth_advisory`]
    /// appends a message and [`ContextEngine::reset_rebirth_advisory`] /
    /// [`ContextEngine::reset_consecutive_rebirths`] change behaviour the stored state
    /// said was already spent. Without the latch the rebirth advisory re-fires on a
    /// rehydrated session that had already been advised.
    pub fn restore_engine_flags(
        &mut self,
        rebirth_advisory_emitted: bool,
        consecutive_rebirths: usize,
    ) {
        self.rebirth_advisory_emitted = rebirth_advisory_emitted;
        self.consecutive_rebirths = consecutive_rebirths;
    }

    /// The exclusive end index of this engine's pinned window — the private
    /// [`pinned_prefix_end`] rule exposed for capture, for the loader's cross-checks, and
    /// for `RepairReport::pinned_prefix_end_{expected,actual}`.
    pub fn pinned_prefix_end(&self) -> usize {
        pinned_prefix_end(&self.messages)
    }

    /// Byte-length witnesses of the two pins. This is the same measurement the loader
    /// recomputes, so a mismatch names *which* pin moved.
    pub fn pin_witness(&self) -> PinWitness {
        pin_witness_of(&self.messages)
    }

    /// The assistant `tool_calls` ↔ `Tool` pairing state of the transcript at this
    /// instant, as the durable `DanglingToolCallState`. The loader cross-checks the same
    /// observer — there is no second pairing rule — and the read is purely observational:
    /// capture never heals.
    pub fn dangling_state(&self) -> DanglingToolCallState {
        let records: Vec<MessageRecord> =
            self.messages.iter().map(MessageRecord::from_wire).collect();
        DanglingToolCallState::observe(&records)
    }

    /// Append the one synthetic restore notice to the transcript.
    ///
    /// This is the **only** place the notice is added, and it is added as a
    /// `Message::User` at index `>= 2`, which is what keeps the pins untouched: nothing
    /// at `[0]`/`[1]` is read or written, and the vector is only extended.
    /// `SESSION_RESTORE_NOTICE_PREFIX` is a member of [`INJECTED_ADVISORY_PREFIXES`], so
    /// `perform_rebirth` can never promote the notice into the `messages[2]` slot instead
    /// of the last genuine instruction.
    ///
    /// The text is stored verbatim — never trimmed, wrapped or re-rendered. A transcript
    /// without the pinned pair gets a `tracing::warn!` and no append: a notice is never
    /// allowed to *become* a pin.
    pub fn append_restore_notice(&mut self, notice: &str) {
        if self.messages.len() < 2 {
            tracing::warn!(
                "append_restore_notice: transcript holds {} message(s), so the pinned pair is \
                 absent — the restore notice is NOT appended (a rehydrated session must be \
                 installed through install_restored_messages first)",
                self.messages.len()
            );
            return;
        }
        if !notice.starts_with(SESSION_RESTORE_NOTICE_PREFIX) {
            tracing::warn!(
                "append_restore_notice: notice does not start with \
                 {SESSION_RESTORE_NOTICE_PREFIX:?}, so the runtime's advisory grammar \
                 ([INJECTED_ADVISORY_PREFIXES]) will not recognize it as injected"
            );
        }
        self.messages.push(Message::User {
            content: notice.to_string(),
        });
    }
}

/// Per-agent [`ContextEngine`] factory.
///
/// Produces isolated, KV-cache-prefix-preserving contexts for the Manager and every
/// Specialist. Each engine is seeded so that `messages[0]` is the agent's own role
/// system prompt and `messages[1]` is its pinned goal/task brief, guaranteeing:
///
/// - KV-cache prefix preservation at the *agent* scope: the system/role prompt is locked
///   at `[0]` for the lifetime of the engine.
/// - Goal pinning: the user goal (Manager) or task brief (specialist) is pinned at `[1]`
///   and survives compaction/rebirth.
/// - Strict isolation: a specialist context contains ONLY its own role prompt + brief —
///   never the Manager's transcript. The factory is the canonical construction point so
///   no code path can seed a specialist with Manager history.
#[derive(Debug, Clone)]
pub struct ContextEngineFactory {
    /// Shared token budget for every produced engine.
    max_context_tokens: usize,
}

impl ContextEngineFactory {
    pub fn new(max_context_tokens: usize) -> Self {
        Self { max_context_tokens }
    }

    pub fn max_context_tokens(&self) -> usize {
        self.max_context_tokens
    }

    /// Build the Manager's context engine.
    ///
    /// The Manager owns the *user* conversation, so its `[0]` is the global system
    /// prompt and `[1]` is the user's goal; this engine accumulates the full interactive
    /// transcript and is distinct from every specialist's.
    ///
    /// The MCP-free Manager tool schemas are charged to the budget up front: every
    /// Manager request goes through `src/llm/stream.rs::build_request`, which always
    /// sends [`ToolDef::manager_tools()`] — names pinned to [`MANAGER_TOOL_NAMES`] so the
    /// accounting cannot drift from the wire set. A configured MCP server **widens** that
    /// wire list, so this charge is then a strict subset: the live Manager path skips
    /// this constructor and re-declares the exact list with [`ContextEngine::set_tools`]
    /// on [`manager_wire_tools`] — `src/ui/session.rs::build_manager_context` charges
    /// once and `sync_manager_tool_schema` re-prices it at every budget decision.
    pub fn manager_context(&self, system_prompt: String, goal: String) -> ContextEngine {
        let mut ctx = ContextEngine::new(self.max_context_tokens);
        ctx.set_system_prompt(system_prompt);
        ctx.set_goal(goal);
        ctx.set_tools(&crate::types::ToolDef::manager_tools());
        ctx
    }

    /// Build a specialist's fully isolated context engine.
    ///
    /// The produced engine carries ONLY `role_prompt` at `messages[0]` and `brief` at
    /// `messages[1]` (the pinned subagent "goal") — no Manager history is ever copied —
    /// so it starts at exactly two messages with a stable KV-cache prefix.
    ///
    /// No tool schemas are charged **by this constructor**: a specialist's tool list is a
    /// role-filtered subset of [`ToolDef::default_tools()`] plus MCP tools, owned by
    /// `src/agents/runner/execution.rs::specialist_advertised_tools` (also
    /// `src/agents/runner/fix_loop.rs::assemble_tools`), so a caller that wants the true
    /// request size must declare that exact list with [`ContextEngine::set_tools`]. Three
    /// callers do exactly that in the shipped binary: the fix loop
    /// (`fix_loop::run_fix_loop` → `fix_loop::charge_engine_tool_schema`), the Manager
    /// path (`ui/session.rs::build_manager_context` → `sync_manager_tool_schema`), and
    /// the specialist turn: `execution::build_specialist_context` calls this constructor
    /// and charges the advertised view right away, and
    /// `execution::sync_specialist_tool_schema` re-prices it immediately before every
    /// budget decision of the specialist turn loop — liveness proved by
    /// `tests/test_specialist_schema_charge.rs`. Only an engine that never goes through
    /// one of those declarers — built here and left alone — stays message-only, which is
    /// what a caller that declares nothing at all ends up with; only for such an engine
    /// is the budget a **lower bound** on the real request, never an over-count.
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
