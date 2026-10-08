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
//!   targets 70%, keeping `[0]`/`[1]`. The assistant `tool_calls` ↔ `Tool`
//!   pairing invariant is enforced in **both** directions: orphan
//!   `role:"tool"` messages are pruned, and `tool_calls` entries whose result
//!   is gone get an `"(aborted)"` result synthesized
//!   ([`repair_tool_call_pairs`]), so the compacted transcript is always a
//!   valid OpenAI-compatible chat request. The pinned window is charged against
//!   the target: when it alone exceeds 70% the target is unreachable and
//!   `compact()` reports [`CompactionOutcome::TargetUnreachable`] instead of
//!   claiming success on an over-budget transcript (bug M5); the reported
//!   message/token deltas are always measured on the final vector (bug M5b).
//! - **REQ-CORE-004** (Forced Rebirth): collapse to the 4-message checkpoint
//!   shape — `[0]` system prompt, `[1]` pinned goal, `[2]` last genuine user
//!   instruction, `[3]` the `SYSTEM: REBIRTH CHECKPOINT` injection emitted as a
//!   **`User`** message (bug M12) — and bump `session_rebirths`. `[2]` is
//!   omitted when it would only duplicate the pinned goal, and injected
//!   advisories are never summarized as an instruction (bug M6).
//!
//! # Requirement status (dead-code decision, see `docs/decision_dead_code_manager.md`)
//!
//! - **REQ-CORE-003** is implemented by the live path only: `should_compact()`
//!   (> 90%) + `compact()` (target 70%) + `repair_tool_call_pairs()` (which
//!   reuses `prune_orphan_tool_messages()` for the orphan half and closes the
//!   dangling-`tool_calls` half of the invariant), as
//!   called from `src/ui/session.rs` and `src/agents/runner/{execution,fix_loop}.rs`.
//!   The caesar-style hard-limit **retry escalation** (`compact_with_retry` /
//!   `compact_context` / `force_compact_context` + `COMPACTION_RETRY_CAP` and
//!   the `SYSTEM: CONTEXT LIMIT EXCEEDED` injection) had no non-test caller and
//!   was deleted; nothing in the runtime compacts on a hard-limit overflow.
//!   **Bug M7 (tool-schema accounting, gate t-064):** the budget is measured on
//!   the whole *request*, and the charge is owned by the caller that actually
//!   sends it. [`ContextEngine::should_compact`] compares
//!   [`ContextEngine::request_token_count`]
//!   (transcript plus the serialized `ChatRequest.tools` payload, counted by
//!   [`tools_tokens`]) against the unchanged 90% trigger. The engine starts at
//!   `0` schema tokens — an engine that declares no tools keeps the exact
//!   message-only numbers — and a caller opts in with
//!   [`ContextEngine::set_tools`], which must be handed **exactly** the list the
//!   wire request carries (never a superset, never a subset). The live Manager
//!   path declares [`manager_wire_tools`] (the same list
//!   `src/llm/stream.rs::build_request` builds: [`crate::types::ToolDef::manager_tools()`
//!   plus the policy-filtered MCP view) in `src/ui/session.rs::build_manager_context`
//!   and re-declares it at every budget decision, so a later MCP connection also
//!   re-prices the budget. There are exactly **three** live declarers, each charging
//!   the list it is about to send:
//!   (1) Manager — `src/ui/session.rs::build_manager_context` plus
//!   `src/ui/session.rs::sync_manager_tool_schema`, re-declared at every budget
//!   decision; (2) fix loop — `src/agents/runner/fix_loop.rs::run_fix_loop` via
//!   `fix_loop::charge_engine_tool_schema`; (3) specialist turn (gate t-073) —
//!   `src/agents/runner/execution.rs::build_specialist_context` charges the
//!   advertised view at construction and
//!   `src/agents/runner/execution.rs::sync_specialist_tool_schema` re-prices it
//!   immediately before every budget decision (`execution.rs:1034`), the liveness
//!   of that charge proved by `tests/test_specialist_schema_charge.rs`. A
//!   message-only engine (schema tokens `0`) is therefore only reachable from a
//!   caller that declares nothing at all — which is what
//!   `ContextEngineFactory::specialist_context()` hands out by design; it is a
//!   **lower bound** on the real request, never an over-count, and the three live
//!   declarers above close that gap rather than rely on it.
//!   `ContextEngineFactory::manager_context()` pre-charges the MCP-free Manager
//!   default so a caller that forgets to declare is still closer to the truth.
//!   Neither `count_tokens` nor the ratio constants changed.
//! - **REQ-CORE-005** (Slow-Prefill Cooling, ≥300 s consecutive backend
//!   prefills with a 5-turn post-rebirth cooldown) is **not implemented**: the
//!   `SlowPrefillTracker` that modelled it had no non-test caller and no live
//!   prefill instrumentation exists, so it was deleted rather than left as a
//!   false assurance.
//! - **REQ-CORE-006** (UTF-8 safe slicing) is satisfied *at the call sites*:
//!   every display truncation goes through `crate::text_util` or an
//!   `is_char_boundary`/`floor_char_boundary` loop (e.g. `src/markers.rs`,
//!   `src/ui/helpers.rs`, `src/harness/mod.rs`). The manager-local
//!   `utf8_safe_slice` copy had no non-test caller and was deleted.

use std::sync::Arc;

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
/// `{"type":"function","function":{…}}` wrapper around every schema
/// (bug M7).
///
/// Mirrors the per-message framing constant in [`message_tokens`]: the wire
/// object costs a few tokens beyond its serialized body.
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

/// BPE token count of the **tool-schema payload** of a chat request (bug M7).
///
/// Every request carries `tools: Option<Vec<ToolDef>>` (see
/// `src/llm/stream.rs::build_request` and
/// `src/agents/runner/fix_loop.rs::build_turn_request`), but the budget model
/// used to count only the message array, so the real request size was
/// systematically underestimated by the size of the schemas.
///
/// Each definition is charged as its `type` tag, function name, description and
/// the JSON serialization of its `parameters` schema, plus
/// [`TOOL_DEF_FRAMING_TOKENS`]. The count is **additive and optional**: an empty
/// (or `None`, i.e. no tools at all) tool list contributes `0`, so callers that
/// send no tools keep the exact pre-M7 numbers.
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
/// schemas sent with it (bug M7).
///
/// Equal to [`count_tokens`] when `tools` is empty, which keeps every
/// tool-less caller numerically identical to the pre-M7 behaviour.
pub fn request_tokens(messages: &[Message], tools: &[crate::types::ToolDef]) -> usize {
    count_tokens(messages) + tools_tokens(tools)
}

/// The **exact** tool-schema payload a Manager request carries on the wire.
///
/// The wire owner is `src/llm/stream.rs::build_request`: every Manager request
/// sends [`crate::types::ToolDef::manager_tools()`] plus the *policy-filtered* MCP
/// view of the configured servers ([`crate::harness::allowed_mcp_tools`], so a MCP
/// name the tool-policy gate refused is never advertised). The budget model has to
/// charge precisely that list — a superset would compact too early and hide
/// transcript, a subset would under-count and let an over-budget request through.
///
/// It is deliberately a function of the *server names only*: the MCP tool list
/// itself comes from the process-wide MCP gate, which is installed when the
/// servers boot and can change when a server (re)connects, so callers re-read it
/// whenever they price a request (see `src/ui/session.rs`).
pub fn manager_wire_tools(mcp_servers: &[String]) -> Vec<crate::types::ToolDef> {
    let mut tools = crate::types::ToolDef::manager_tools();
    for tool in crate::harness::allowed_mcp_tools(mcp_servers) {
        tools.push(crate::types::ToolDef::from_mcp(&tool));
    }
    tools
}

/// Exclusive end index of the pinned prefix of a transcript.
///
/// `messages[0]` (system prompt, REQ-CORE-001) and `messages[1]` (immutable
/// goal, REQ-CORE-002) are always pinned. If a `REBIRTH CHECKPOINT` exists in
/// `messages[2..]`, the pinned window is extended through it so distilled
/// session state is never dropped during compaction.
///
/// The checkpoint is matched on its **content marker**, not its role:
/// `perform_rebirth` emits it as a `User` message (bug M12) while transcripts
/// reloaded from disk ([`ContextEngine::load_transcript`]) can still carry the
/// historic `System` variant.
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

/// Content markers that identify an **injected** advisory/notice message — a
/// `Message::User` the runtime put in the transcript itself rather than a turn
/// the user or the model produced. All of them are synthesized by this crate
/// with a fixed leading marker:
///
/// - [`REBIRTH_ADVISORY_MESSAGE`] — the `SYSTEM: CONTEXT BUDGET ADVISORY`
///   injection ([`ContextEngine::inject_rebirth_advisory`]).
/// - `SYSTEM: CONTEXT LIMIT EXCEEDED` — the hard-limit notice deleted by the
///   `compact_with_retry` dead-code decision (H7); transcripts saved before
///   that deletion still contain it.
/// - `SYSTEM: Rebirth checkpoint accepted` — the post-rebirth continuation
///   injection (`src/ui/session.rs`, `src/agents/runner/execution.rs`,
///   `src/agents/validation.rs`).
/// - [`REBIRTH_CHECKPOINT_PREFIX`] — the rebirth checkpoint itself, which must
///   never be summarized as if it were a user instruction.
///
/// Used by [`ContextEngine::perform_rebirth`] so the rebirth `messages[2]` slot
/// carries the last **genuine** instruction (bug M6, `docs/recon_bugs_manager.md`).
pub const INJECTED_ADVISORY_PREFIXES: &[&str] = &[
    "(SYSTEM: CONTEXT BUDGET ADVISORY",
    "(SYSTEM: CONTEXT LIMIT EXCEEDED",
    "(SYSTEM: Rebirth checkpoint accepted",
    REBIRTH_CHECKPOINT_PREFIX,
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

/// Prune orphaned `role:"tool"` messages (REQ-CORE-003).
///
/// A tool message is an orphan if its `tool_call_id` does not correspond to a
/// `tool_calls` entry on a *surviving* assistant message. Orphans are dropped
/// so no invalid tool response is ever sent upstream.
///
/// This is only **one half** of the pairing invariant: it removes tool results
/// that outlive their parent. The reverse half (an assistant `tool_calls` entry
/// whose result is missing) is enforced by [`repair_tool_call_pairs`], which
/// reuses this pass rather than duplicating it.
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
/// `/chat/completions` request (bug H1, `docs/recon_bugs_manager.md`):
///
/// 1. **No orphan `Tool`** — a `role:"tool"` message whose `tool_call_id` has no
///    surviving assistant parent is dropped. This half reuses
///    [`prune_orphan_tool_messages`] instead of re-implementing it, and also
///    drops duplicate results for the same id (keeping the first).
/// 2. **No dangling `tool_calls`** — every `tool_calls` entry of a surviving
///    assistant message is followed by exactly one `Message::Tool` carrying its
///    id, in `tool_calls` order. Missing results are synthesized as
///    `Message::Tool { content: ABORTED_TOOL_RESULT }`.
///
/// Existing results are *moved* so they sit directly after their parent
/// assistant; nothing else is reordered, so `messages[0]` (system prompt) and
/// `messages[1]` (pinned goal) keep their positions (REQ-CORE-001/002) — they
/// can only ever be `System`/`User` messages, which this pass never moves.
///
/// **Design choice (option (a)):** synthesize an `"(aborted)"` placeholder
/// rather than dropping the assistant turn (option (b)). Rationale: the live
/// trigger for H1 is `src/ui/session.rs` breaking its tool loop on abort/exit,
/// which leaves the assistant-with-`tool_calls` as the *newest* message; option
/// (b) would delete the model's most recent reasoning/intent (and, for the
/// pinned window, could delete a turn the retention policy deliberately kept).
/// A placeholder keeps the pairing valid while losing only the (nonexistent)
/// tool output, and it is self-describing to the model.
pub fn repair_tool_call_pairs(messages: Vec<Message>) -> Vec<Message> {
    // Half 1: drop orphan tool results (and, by construction of the map below,
    // any duplicate result for an id that is already accounted for).
    let messages = prune_orphan_tool_messages(messages);

    // Index the surviving tool results by call id, keeping the first occurrence.
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
            // Tool results are re-emitted grouped under their assistant parent.
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
/// compaction that actually reached the 70% target from one that could not
/// (bug M5, `docs/recon_bugs_manager.md`).
///
/// The counters are always measured on the **final** message vector, i.e. after
/// the retention window, after the pairing repair (orphan pruning + placeholder
/// synthesis) and after the bounded re-trim (bug M5b: the pre-pruning count
/// `initial_len - (prefix_end + kept_tail)` reported by the old
/// `compact_to_target` is stale for any pass where pruning or synthesis changes
/// the vector).
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
    /// [`pinned_prefix_end`]) is sacrosanct per REQ-CORE-001/002 yet already
    /// costs more than the target. Compaction trims everything it is allowed to
    /// trim and then reports this failure instead of pretending the transcript
    /// fits the budget.
    TargetUnreachable {
        /// Tokens of the pinned prefix that alone exceed the target.
        pinned_tokens: usize,
        /// Number of pinned messages.
        pinned_messages: usize,
        /// The unreachable 70% target.
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

    /// Messages removed, measured on the final vector.
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

    /// Tokens reclaimed, measured on the final vector.
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

    /// The 70% target compaction aimed at.
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
    /// Cached BPE token count of the tool schemas sent with this engine's
    /// requests (bug M7). `0` means "no tool payload accounted for", which
    /// reproduces the pre-M7 numbers exactly.
    tool_schema_tokens: usize,
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

    /// Declare the tool schemas this engine's requests will carry, so the budget
    /// model measures the **real** request size (bug M7, gate t-064).
    ///
    /// `tools` must be exactly the list the next request sends — the same
    /// `Vec<ToolDef>` handed to `ChatRequest.tools` — because the count is cached
    /// and reused by every later budget decision. Pass an empty slice to go back
    /// to message-only accounting. Callers whose advertised list can change
    /// mid-life (MCP servers connecting later, per-run role filtering) must
    /// re-declare it, which is why the live callers re-declare at every budget
    /// decision instead of once at construction.
    pub fn set_tools(&mut self, tools: &[crate::types::ToolDef]) {
        self.tool_schema_tokens = tools_tokens(tools);
    }

    /// Cached token count of the tool-schema payload (see [`ContextEngine::set_tools`]).
    pub fn tool_schema_tokens(&self) -> usize {
        self.tool_schema_tokens
    }

    /// Token count of the request this engine would send right now: transcript
    /// plus tool schemas (bug M7). Identical to [`ContextEngine::token_count`]
    /// when no tools were declared.
    pub fn request_token_count(&self) -> usize {
        self.token_count() + self.tool_schema_tokens
    }

    /// Whether compaction should trigger right now for a request carrying
    /// `tools` (> 90% of budget measured on transcript **plus** tool schemas,
    /// bug M7).
    ///
    /// Equivalent to [`ContextEngine::should_compact`] after
    /// [`ContextEngine::set_tools(tools)`][ContextEngine::set_tools], without
    /// mutating the engine.
    pub fn should_compact_with_tools(&self, tools: &[crate::types::ToolDef]) -> bool {
        request_tokens(&self.messages, tools) > compaction_threshold(self.max_context_tokens)
    }

    /// Whether compaction should trigger right now (> 90% of budget).
    ///
    /// The measurement is the full request size — transcript plus the declared
    /// tool schemas — so a tool-heavy request can no longer slip past the
    /// trigger (bug M7). Engines with no declared tools are unchanged.
    pub fn should_compact(&self) -> bool {
        self.request_token_count() > compaction_threshold(self.max_context_tokens)
    }

    /// Whether a rebirth advisory should be emitted right now (> 80% of budget, not yet emitted, and no consecutive rebirth).
    pub fn should_advise_rebirth(&self) -> bool {
        !self.rebirth_advisory_emitted
            && self.consecutive_rebirths == 0
            && self.request_token_count() > rebirth_advisory_threshold(self.max_context_tokens)
    }

    /// Number of consecutive rebirth executions without other actions.
    pub fn consecutive_rebirths(&self) -> usize {
        self.consecutive_rebirths
    }

    /// Reset the consecutive rebirth counter when other productive work is executed.
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

    /// Whether a rebirth advisory has already been emitted for this context generation.
    pub fn rebirth_advisory_emitted(&self) -> bool {
        self.rebirth_advisory_emitted
    }

    /// Compact the transcript down to ~70% of the budget (REQ-CORE-003).
    ///
    /// Always keeps `messages[0]` (System) and `messages[1]` (Goal). The most
    /// recent turn pairs are preserved first; once the target budget is
    /// reached, older turns are dropped. The assistant `tool_calls` ↔ `Tool`
    /// pairing invariant is then enforced over the **entire** retained sequence
    /// (including the pinned window): orphaned `role:"tool"` messages are
    /// pruned and `tool_calls` entries that lost their result get an
    /// `"(aborted)"` result synthesized, so the compacted transcript is always
    /// a valid OpenAI-compatible chat request (see [`repair_tool_call_pairs`]).
    ///
    /// **Target safety (bug M5).** The pinned window returned by
    /// [`pinned_prefix_end`] is part of the budget arithmetic, not a free
    /// rider: if the pinned prefix alone already costs more than the 70% target
    /// the target is unreachable, because REQ-CORE-001/002 forbid dropping it.
    /// Compaction then trims everything it is allowed to trim and returns
    /// [`CompactionOutcome::TargetUnreachable`] instead of reporting success on
    /// an over-budget transcript (the live loops at `src/ui/session.rs` and
    /// `src/agents/runner/{execution,fix_loop}.rs` can use that to stop
    /// re-compacting a context that can never fit, e.g. by asking for a
    /// rebirth). The same failure is returned if the re-trim stops making
    /// progress, which also keeps that loop terminating.
    ///
    /// **Truthful accounting (bug M5b).** The reported message/token deltas are
    /// computed from the **final** message vector — after the retention window,
    /// after the pairing repair and after the re-trim — never from the
    /// pre-pruning window arithmetic.
    pub fn compact(&mut self) -> CompactionOutcome {
        let initial_tokens = self.token_count();
        let initial_msgs = self.messages.len();
        let target = compaction_target(self.max_context_tokens);

        // Always pin the first two messages (system and goal).
        // If an active REBIRTH CHECKPOINT exists in messages[2..], pin up through the checkpoint
        // so that distilled session state is never dropped during compaction.
        // M5: that pinned window is charged against the target — it can never
        // be dropped, so it decides whether the target is reachable at all.
        let prefix_end = pinned_prefix_end(&self.messages).min(self.messages.len());
        let pinned_messages = prefix_end;
        let pinned_tokens = count_tokens(&self.messages[..prefix_end]);

        if pinned_tokens > target {
            // Unreachable by construction: trim every non-pinned message and
            // report the failure instead of silently under-delivering.
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
        // `total` starts at the pinned prefix cost (M5: the pinned prefix is
        // budgeted, the tail only gets what the target leaves over it).
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

        // Enforce BOTH halves of the assistant `tool_calls` <-> `Tool` pairing
        // invariant (bug H1) over the whole retained sequence: dropping old
        // turns can orphan surviving `Tool` messages, and a surviving assistant
        // can be missing results either because the results fell out of the
        // window or because the tool loop was aborted upstream
        // (`src/ui/session.rs`) so they were never produced. Running the repair
        // over `kept` (and not just over `kept[prefix_end..]`) means the pinned
        // window cannot smuggle a dangling `tool_calls` entry upstream either.
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

        // M5b: the reclaimed counts are measured on the final vector, so they
        // include everything the pruning, the placeholder synthesis and the
        // re-trim actually did — not the pre-pruning window arithmetic.
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

    /// Enforce the assistant `tool_calls` ↔ `Tool` pairing invariant on the
    /// **live** transcript and report whether anything had to be repaired.
    ///
    /// This is the idempotent public entry point for the invariant outside of
    /// compaction (t-063). Before it existed, [`repair_tool_call_pairs`] had no
    /// non-test caller other than [`ContextEngine::compact`], and `compact()` is
    /// gated at > 90% utilization — so the transcripts that actually end a turn
    /// early (session abort, user exit, wall-clock bound, failure budget) were
    /// re-sent verbatim with an assistant `tool_calls` entry and no `role:"tool"`
    /// result for the calls that were skipped: a provider 400.
    ///
    /// **Idempotent:** running it on an already valid transcript is a no-op that
    /// returns `false`, so a call site can safely be a belt-and-braces guard on
    /// top of a boundary repair.
    ///
    /// **Grammar:** the pass itself is owned by [`repair_tool_call_pairs`] and is
    /// reused verbatim — this wrapper does not add, drop or reword any rule, it
    /// only applies that pass to `self.messages` in place.
    ///
    /// Returns `true` when the transcript was changed (a repair was needed),
    /// `false` when it already satisfied the invariant.
    pub fn ensure_tool_call_pairs(&mut self) -> bool {
        let before = pairing_shape(&self.messages);
        let repaired = repair_tool_call_pairs(std::mem::take(&mut self.messages));
        let changed = pairing_shape(&repaired) != before;
        self.messages = repaired;
        changed
    }

    /// Collapse the history into the rebirth checkpoint shape (REQ-CORE-004).
    ///
    /// - `messages[0]`: static system prompt (REQ-CORE-001).
    /// - `messages[1]`: original user goal (REQ-CORE-002).
    /// - `messages[2]`: the last **genuine** user instruction, when there is
    ///   one (see below).
    /// - last: the `SYSTEM: REBIRTH CHECKPOINT` injection, emitted as a
    ///   **`User`** message (bug M12: strict providers reject a non-leading
    ///   `system` turn). The [`REBIRTH_CHECKPOINT_PREFIX`] text is unchanged,
    ///   and [`pinned_prefix_end`] keeps pinning it through later compactions.
    ///
    /// Two history-hygiene rules (bugs M6/M12, `docs/recon_bugs_manager.md`):
    ///
    /// 1. Runtime-injected advisories/notices ([`INJECTED_ADVISORY_PREFIXES`]:
    ///    the `CONTEXT BUDGET ADVISORY`, the legacy `CONTEXT LIMIT EXCEEDED`
    ///    notice, the post-rebirth continuation injection, and an earlier
    ///    checkpoint) are never summarized as `messages[2]` — the real last
    ///    instruction would be lost behind boilerplate.
    /// 2. The `messages[2]` slot is **omitted** when the only candidate is the
    ///    pinned goal itself, so the goal appears exactly once in the collapsed
    ///    transcript instead of being duplicated verbatim at `[1]` and `[2]`.
    ///    Rebirth therefore collapses to 4 messages when a distinct instruction
    ///    exists and to 3 when it does not.
    ///
    /// The collapse also preserves the assistant `tool_calls` ↔ `Tool` pairing
    /// invariant: every emitted message is `System`/`User`, so no `tool_calls`
    /// entry and no orphan `Tool` can survive a rebirth.
    ///
    /// The `session_rebirths` counter in the attached [`HarnessStats`] is
    /// incremented.
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
        // every runtime-injected advisory/notice message (bug M6).
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
        // M12: never duplicate the pinned goal at `[2]`.
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

    /// Save the full message transcript to disk as JSON.
    pub fn save_transcript(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.messages)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load the message transcript from disk if it exists and contains at least 2 messages (system + goal).
    pub fn load_transcript(&mut self, path: &std::path::Path) -> anyhow::Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        let data = std::fs::read_to_string(path)?;
        let msgs: Vec<Message> = match serde_json::from_str(&data) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    "Failed to deserialize session transcript from {}: {e}",
                    path.display()
                );
                return Ok(false);
            }
        };
        if msgs.len() < 2 {
            return Ok(false);
        }
        self.messages = msgs;
        Ok(true)
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
    ///
    /// The MCP-free Manager tool schemas are charged to the budget up front
    /// (bug M7, gate t-064): every Manager request goes through
    /// `src/llm/stream.rs::build_request`, which always sends
    /// [`ToolDef::manager_tools()`] — names pinned to [`MANAGER_TOOL_NAMES`] so
    /// the accounting cannot drift from the wire set. A configured MCP server
    /// **widens** that wire list, so this default is then a strict subset: the
    /// live Manager path does not use this constructor and re-declares the exact
    /// list with [`ContextEngine::set_tools`] on [`manager_wire_tools`] instead
    /// (see `src/ui/session.rs::build_manager_context`, which charges once and
    /// then re-prices through `src/ui/session.rs::sync_manager_tool_schema` at
    /// every budget decision — one of the three live declarers named in the
    /// module doc, alongside `fix_loop::charge_engine_tool_schema` and
    /// `execution::build_specialist_context` /
    /// `execution::sync_specialist_tool_schema`).
    pub fn manager_context(&self, system_prompt: String, goal: String) -> ContextEngine {
        let mut ctx = ContextEngine::new(self.max_context_tokens);
        ctx.set_system_prompt(system_prompt);
        ctx.set_goal(goal);
        ctx.set_tools(&crate::types::ToolDef::manager_tools());
        ctx
    }

    /// Build a specialist's fully isolated context engine (REQ-ORCH-003).
    ///
    /// The produced engine carries ONLY `role_prompt` at `messages[0]` and
    /// `brief` at `messages[1]` (the pinned subagent "goal", mirroring
    /// REQ-CORE-002 at the subagent scope). No Manager history is ever copied.
    /// A freshly built engine therefore starts with exactly two messages and a
    /// stable KV-cache prefix for the specialist's backend.
    ///
    /// No tool schemas are pre-charged **by this constructor**: a specialist's
    /// tool list is a role-filtered subset of [`ToolDef::default_tools()`] plus
    /// MCP tools, and the authority for it is
    /// `src/agents/runner/execution.rs::specialist_advertised_tools`
    /// (see also `src/agents/runner/fix_loop.rs::assemble_tools`), so a caller
    /// that wants the true request size must declare that exact list with
    /// [`ContextEngine::set_tools`]. Three callers do exactly that in the shipped
    /// binary (see Bug M7 in the module doc): the fix loop
    /// (`fix_loop::run_fix_loop` → `fix_loop::charge_engine_tool_schema`), the
    /// Manager path (`ui/session.rs::build_manager_context` →
    /// `sync_manager_tool_schema`), and the specialist turn (gate t-073):
    /// `execution::build_specialist_context` calls this constructor and charges
    /// the advertised view right away, and
    /// `execution::sync_specialist_tool_schema` re-prices it immediately before
    /// every budget decision (`execution.rs:1034`) — liveness proved by
    /// `tests/test_specialist_schema_charge.rs`.
    ///
    /// Only an engine that never goes through one of those declarers — i.e. built
    /// here and left alone — stays message-only, and only for such an engine is
    /// the budget a **lower bound** on the real request, never an over-count. The
    /// live specialist path is charged and re-priced, so its number is not a
    /// message-only lower bound.
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
