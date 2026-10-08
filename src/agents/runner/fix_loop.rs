//! Shared validator/fix-loop driver.
//!
//! **Consolidation (duplicates.md §6b):** three near-identical copies of the
//! "LLM turn → tool dispatch → verdict" loop existed:
//! `run_automated_validation_inner` and `run_plan_validation_inner`
//! (validation.rs) plus the specialist streaming turn in `runner/execution.rs`.
//! They had already drifted semantically (abort → `Ok(false, …)` vs `Err(…)`),
//! which is exactly the class of bug this consolidation eliminates.
//!
//! ## Abort-semantics decision (deliberate, documented)
//!
//! **All abort/cancellation paths now return `Err` (strict failure), never
//! `Ok(false, …)` (soft rejection).** Rationale:
//! - A cancelled validation is NOT a verdict. Returning `Ok(false)` ("rejected")
//!   would let downstream logic treat a user-initiated abort as an authoritative
//!   negative verdict, potentially triggering revision loops or rejections that
//!   the user never asked for.
//! - `Err` propagates to the caller, which already special-cases cancellation
//!   (e.g. `harness/plan.rs` checks `token.is_cancelled()` before treating an
//!   `Err` as a skip) — so the strict path is fully supported by all consumers.
//! - The deliverable loop and the plan loop previously disagreed on this; the
//!   stricter `Err` behavior (previously used by the deliverable loop) wins.
//!
//! Likewise, a hard LLM/backend error now always returns `Err` (previously the
//! deliverable loop `break`-ed out of its turn loop and the post-loop fallback
//! *assumed approved* — the unsafe direction for a validator).
//!
//! ## Bounds of one fix-loop run (gate t-033c)
//!
//! The fix loop used to be **unbounded at its own level**: it had no round cap
//! and no wall-clock deadline, so one validator / plan-audit pass could spin
//! forever (the outer Manager turn has [`crate::manager::r#loop::MAX_TURNS`] and
//! the t-031c turn watchdog, but neither counts the *inner* rounds of this loop).
//! One fix-loop run is now bounded twice:
//!
//! * [`MAX_FIX_LOOP_ROUNDS`] — an explicit iteration (round) cap;
//! * [`FIX_LOOP_IDLE_LIMIT_SECS`] / [`FIX_LOOP_HARD_CAP_SECS`] — a wall-clock
//!   deadline enforced by the crate's existing [`TurnWatchdog`] /
//!   [`DeadlineKind`] machinery (no second timer is invented), with the idle
//!   window fed by observable progress (a completed tool call, an injected
//!   steering notice).
//!
//! Exhausting either bound returns [`FixLoopResult::Exhausted`] with an explicit
//! reason and a `tracing::warn!` — never a silent success.
//!
//! ## Fail-closed verdict contract (gate t-033c, contract change)
//!
//! After the three verdict reminders, a validator that still never called
//! `leave_verdict` **no longer auto-approves** its deliverable. Not having
//! recorded a verdict is not approval: the loop returns `approved = false` with
//! an explicit "no verdict recorded" reason, the deliverable fails and its plan
//! line stays unchecked.

use crate::agents::Agent;
use crate::agents::runner::execution::may_record_verdict;
use crate::agents::runner::formatting::{format_tool_args_full, format_tool_args_preview};
use crate::agents::validation::{
    NO_EXPLICIT_APPROVAL_REASON, is_leave_verdict_tool, parse_verdict_args,
};
use crate::harness::ToolError;
use crate::harness::monitor::{HarnessMonitor, Intervention, RepetitionDetector};
use crate::manager::ContextEngine;
use crate::manager::context::CompactionOutcome;
use crate::manager::r#loop::{DeadlineKind, TURN_WATCHDOG_SECS, TurnWatchdog};
use crate::orchestrator::SpecialistRegistry;
use crate::orchestrator::{
    ActiveWorkerGuard, PreemptibleStreamSink, emit_status, is_current_or_global_cancelled,
    register_active_worker_with_token, update_active_worker_context,
};
use crate::types::{ChatRequest, Message, ToolCall, ToolDef};
use crate::{harness, llm, tool_names};
use std::time::Duration;

/// Outcome of one fix-loop run, before the caller maps it to its own
/// `(bool, String)` contract or error type.
#[derive(Debug)]
pub enum FixLoopResult {
    /// The model submitted a `leave_verdict` (or the caller's verdict callback
    /// fired): `(approved, critique)`.
    Verdict { approved: bool, critique: String },
    /// Aborted by cancellation / steer preemption. Callers MUST surface this as
    /// a failure (see module docs for the strict-abort decision).
    Aborted,
    /// The run hit one of its own bounds — the round cap ([`MAX_FIX_LOOP_ROUNDS`])
    /// or the wall-clock deadline ([`FIX_LOOP_IDLE_LIMIT_SECS`] /
    /// [`FIX_LOOP_HARD_CAP_SECS`]) — without ever recording a verdict.
    ///
    /// This is an **explicit failed outcome**, carrying the reason the loop was
    /// torn down. It is deliberately not a `Verdict { approved: false, … }`: the
    /// loop never reached a verdict, and callers must be able to tell "the auditor
    /// ran out of budget" from "the auditor rejected the deliverable".
    Exhausted { reason: String },
}

// ── Fix-loop bounds (gate t-033c) ──────────────────────────────────────────

/// How many LLM rounds **one** fix-loop run may take (validator pass, plan audit
/// or any other [`run_fix_loop`] driver).
///
/// A validator pass normally concludes in a handful of rounds (inspection tools +
/// one verdict); 40 leaves generous headroom for a legitimate deep inspection
/// while making an unbounded inner loop impossible. Deliberately a constant
/// rather than a config knob: it is a runaway guard, not a tuning surface.
pub const MAX_FIX_LOOP_ROUNDS: usize = 40;

/// How many verdict reminders a validator gets before the run is concluded
/// **without** a verdict (fail-closed, gate t-033c).
const VERDICT_REMINDERS: usize = 3;

/// Prefix that marks a tool result as a failure for this driver's own dispatch
/// calls — [`dispatch_tool_call`] classifies success by this prefix, so it is a
/// named constant rather than a literal repeated at the call site.
pub const TOOL_ERROR_PREFIX: &str = "ERROR: ";

/// The repetition-intervention fallback handed to [`dispatch_tool_call`]. The
/// verdict tool is named through its constant (gate t-033e: no raw tool-name
/// literal anywhere in this file); the `{tool}` placeholder is substituted by
/// the dispatcher.
pub fn repetition_intervention_fallback() -> String {
    format!(
        "Tool repetition detected for '{{tool}}'. Conclude by calling {} or proceed with the task.",
        tool_names::TOOL_LEAVE_VERDICT,
    )
}

/// Idle bound of one fix-loop run, i.e. how long it may go with **no observable
/// progress** before it is torn down. Reuses the crate's turn-watchdog bound
/// ([`TURN_WATCHDOG_SECS`]) so there is exactly one wall-clock vocabulary.
pub const FIX_LOOP_IDLE_LIMIT_SECS: u64 = TURN_WATCHDOG_SECS;

/// Absolute wall-clock bound of one fix-loop run, progress or not.
///
/// Must stay **strictly inside** the enclosing Manager turn hard cap
/// ([`TURN_HARD_CAP_SECS`]) — otherwise the fix loop could outlive the turn that
/// owns it and the outer watchdog would be the first to fire, which is exactly
/// the "no bound at the fix-loop's own level" situation this constant removes.
pub const FIX_LOOP_HARD_CAP_SECS: u64 = 30 * 60;

/// The iteration + wall-clock bounds of one [`run_fix_loop`] run.
///
/// The wall-clock half is the crate's existing [`TurnWatchdog`] (idle window +
/// absolute cap) reporting [`DeadlineKind`]; the iteration half is a plain round
/// counter compared against `max_rounds`. Injectable (`at`) so both bounds are
/// testable without waiting for real time to pass.
#[derive(Debug, Clone)]
pub struct FixLoopBounds {
    watchdog: TurnWatchdog,
    max_rounds: usize,
    idle_limit: Duration,
    hard_limit: Duration,
}

impl Default for FixLoopBounds {
    fn default() -> Self {
        Self::new(MAX_FIX_LOOP_ROUNDS)
    }
}

impl FixLoopBounds {
    /// Arm bounds with the crate-default wall-clock limits.
    pub fn new(max_rounds: usize) -> Self {
        Self::at(
            std::time::Instant::now(),
            max_rounds,
            Duration::from_secs(FIX_LOOP_IDLE_LIMIT_SECS),
            Duration::from_secs(FIX_LOOP_HARD_CAP_SECS),
        )
    }

    /// Arm bounds anchored at an explicit instant (test seam).
    pub fn at(
        now: std::time::Instant,
        max_rounds: usize,
        idle_limit: Duration,
        hard_limit: Duration,
    ) -> Self {
        Self {
            watchdog: TurnWatchdog::at(now, idle_limit, hard_limit),
            max_rounds,
            idle_limit,
            hard_limit,
        }
    }

    /// `true` when `round` (1-based) exceeds the iteration cap.
    pub fn round_cap_reached(&self, round: usize) -> bool {
        round > self.max_rounds
    }

    /// The iteration cap itself, for reason strings and tests.
    pub fn max_rounds(&self) -> usize {
        self.max_rounds
    }

    /// Which wall-clock bound the run has blown (`None` = still inside both).
    pub fn deadline_expired(&self) -> Option<DeadlineKind> {
        self.watchdog.expired()
    }

    /// [`FixLoopBounds::deadline_expired`] anchored at an explicit instant
    /// (test seam).
    pub fn deadline_expired_at(&self, now: std::time::Instant) -> Option<DeadlineKind> {
        self.watchdog.expired_at(now)
    }

    /// Record observable forward progress, resetting the stalled-run window.
    pub fn note_round_progress(&mut self) {
        self.watchdog.note_progress();
    }

    /// [`FixLoopBounds::note_round_progress`] anchored at an explicit instant
    /// (test seam, mirroring `TurnWatchdog::note_progress_at`).
    pub fn note_round_progress_at(&mut self, now: std::time::Instant) {
        self.watchdog.note_progress_at(now);
    }

    /// Reason for blowing the iteration cap.
    pub fn round_cap_reason(&self) -> String {
        format!(
            "fix-loop iteration cap: this validation run exceeded its {} rounds without recording a verdict — run aborted and reported as not approved",
            self.max_rounds
        )
    }

    /// Reason for blowing a wall-clock bound, spelled by the crate's own
    /// [`DeadlineKind::describe`] so the wording matches the turn watchdog.
    pub fn deadline_reason(&self, kind: DeadlineKind) -> String {
        format!(
            "{} — no verdict was recorded and the deliverable is reported as not approved",
            kind.describe(self.idle_limit, self.hard_limit)
        )
    }
}

/// Everything the shared driver needs to run the turn loop.
///
/// Built by thin wrappers (validation.rs / execution.rs) so each call site
/// keeps only its prompt/brief/status specifics.
pub struct LoopParams<'a> {
    pub client: &'a llm::ChatClient,
    pub model: String,
    /// Stream/worker tag, e.g. `validator-coder-t-xxx` or `coder-t-xxx`.
    pub tag: String,
    /// Worker name shown in the stream registry, e.g. `validator-coder`.
    pub worker_name: String,
    pub task_id: Option<String>,
    /// Active-worker registry key (the `ActiveWorkerGuard.0` value).
    pub worker_key: String,
    pub engine: &'a mut ContextEngine,
    pub tools: Vec<ToolDef>,
    pub token: &'a tokio_util::sync::CancellationToken,
    pub mon_cfg: &'a crate::config::MonitoringConfig,
    pub cfg: &'a crate::config::Config,
    /// Sampling temperature (0.0 for validators, cfg.temperature for specialists).
    pub temperature: f32,
    /// Status line emitted once per turn, e.g.
    /// `"{tag}: evaluating test & inspection output (turn {turn})..."`.
    /// The driver appends ` (turn N)` automatically; keep the trailing `...`.
    pub status_template: String,
    /// Role label for `debug_log::log_validation_verdict`.
    pub verdict_log_role: String,
    /// Iteration + wall-clock bounds of this run (gate t-033c, see
    /// [`FixLoopBounds`]). Exhausting either bound ends the run with
    /// [`FixLoopResult::Exhausted`] — an explicit failed outcome, never a
    /// silent success.
    pub bounds: FixLoopBounds,
    /// Prefix for the "aborted before tool {name}" warn log.
    pub abort_log_prefix: String,
    /// Whether to emit a per-tool `emit_status` line (deliverable/specialist
    /// loops do; the plan auditor loop historically did not).
    pub emit_tool_status: bool,
    /// User-level notice injected when a `rebirth` call succeeds (wording
    /// differs between validator and specialist loops).
    pub rebirth_notice: String,
}

/// Build the canonical `ChatRequest` shared by all fix-loop turn calls
/// (previously a 10-line literal duplicated ×3).
pub fn build_turn_request(
    model: &str,
    engine: &ContextEngine,
    tools: &[ToolDef],
    cfg: &crate::config::Config,
    temperature: f32,
) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: engine.messages().to_vec(),
        tools: Some(tools.to_vec()),
        stream: Some(true),
        enable_thinking: None,
        temperature: Some(temperature),
        top_p: Some(cfg.top_p),
        presence_penalty: Some(cfg.presence_penalty),
        frequency_penalty: Some(cfg.frequency_penalty),
    }
}

// ── Fix-loop tool blueprint (gate t-033e) ────────────────────────────────────
//
// The "blueprint" of a fix-loop run is the set of tool schemas handed to the
// model plus the tool calls that same run is allowed to dispatch. It is spelled
// **exclusively** with the constants of [`crate::tool_names`]: no tool name is
// ever re-typed as a string literal in this file, so the blueprint, the
// dispatcher (`src/harness/mod.rs`) and the specialist registry can never drift
// apart by a typo.
//
// The classes exist so the blueprint is reasoned about **by class** rather than
// by ad-hoc name comparisons. The verdict class is the one a role must never
// receive unless the crate's public role gate [`may_record_verdict`]
// (`runner/execution.rs`, gate t-033b) accepts that role. This block does **not**
// restate that gate: it calls it, so the identity rule lives in exactly one place.

/// The tools that record a validation verdict (bare + `terminal__` variant).
///
/// **The spellings are owned by [`tool_names`]** (gate t-064): the verdict rows of
/// [`tool_names::TOOL_ALIAS_TABLE`] enumerated by [`tool_names::tool_spellings_for`]
/// and the predicate [`tool_names::is_leave_verdict_tool_name`] are the single
/// source of truth, and [`is_verdict_recording_tool`] answers from that owner —
/// never from this list. This constant survives only as the compile-time mirror of
/// the two canonical spellings for callers that need a `&'static [&'static str]`
/// (it is asserted to be a subset of the owner's spelling family by
/// `verdict_recording_tool_spellings_are_owned_by_tool_names` below).
pub const VERDICT_RECORDING_TOOLS: &[&str] = &[
    tool_names::TOOL_LEAVE_VERDICT,
    tool_names::TERMINAL_LEAVE_VERDICT,
];

/// The verdict spelling family exactly as [`tool_names`] owns it: the alias-table
/// rows that resolve onto the verdict tool plus its canonical spellings. Callers
/// that need to enumerate verdict spellings must use this (or
/// [`is_verdict_recording_tool`]) instead of re-typing names.
pub fn verdict_recording_tool_spellings() -> Vec<&'static str> {
    tool_names::tool_spellings_for(tool_names::TOOL_LEAVE_VERDICT)
}

/// Tools advertised to every fix-loop role regardless of allow-lists: answering a
/// steering notice is not a privilege (it is the same contract
/// `dispatch_specialist` applies to [`tool_names::TOOL_REPLY_TO_ARBITRATOR`]).
pub const ALWAYS_ADVERTISED_TOOLS: &[&str] = &[tool_names::TOOL_REPLY_TO_ARBITRATOR];

/// Read-only inspection tools an auditor may be advertised.
///
/// `list_directory` / `terminal__list_directory` used to be listed here while the
/// alias rows that claimed those spellings were still in the tool-name table.
/// They are gone (gate t-064): `src/harness/mod.rs` has **no** built-in dispatch
/// arm for `list_directory` (the alias rows were deliberately removed in t-069,
/// see the note on `tool_names::CANONICAL_TOOL_NAMES`), so advertising them
/// promised the model a tool nobody serves — the call comes back
/// `ToolError::UnknownTool` (or falls through to an MCP server that does publish
/// the name, in which case the MCP view of the blueprint carries it, not this
/// list). The constants in `src/tool_names.rs` stay; only the unclaimed
/// advertisement is removed.
pub const INSPECTION_TOOLS: &[&str] = &[
    tool_names::TOOL_READ_FILE,
    tool_names::TOOL_GREP_SEARCH,
    tool_names::TOOL_GLOB,
    tool_names::TERMINAL_READ_FILE,
    tool_names::TERMINAL_GREP_SEARCH,
    tool_names::TERMINAL_GLOB,
    tool_names::TOOL_PTY_SPAWN,
    tool_names::TOOL_PTY_WRITE,
    tool_names::TOOL_PTY_READ,
    tool_names::TOOL_PTY_CLOSE,
    tool_names::TOOL_PTY_LIST,
    tool_names::TOOL_REBIRTH,
    tool_names::TOOL_SLEEP,
];

/// File-mutating / command-executing tools: never part of an auditor blueprint
/// unless the caller's allow-list explicitly grants them.
pub const MUTATING_TOOLS: &[&str] = &[
    tool_names::TOOL_WRITE_FILE,
    tool_names::TOOL_REPLACE,
    tool_names::TOOL_RUN_COMMAND,
    tool_names::TERMINAL_WRITE_FILE,
    tool_names::TERMINAL_REPLACE,
    tool_names::TERMINAL_RUN_COMMAND,
];

/// Which blueprint class a tool name belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlueprintToolClass {
    /// Advertised to every role ([`ALWAYS_ADVERTISED_TOOLS`]).
    AlwaysAdvertised,
    /// Read-only inspection ([`INSPECTION_TOOLS`]).
    Inspection,
    /// File mutation / execution ([`MUTATING_TOOLS`]).
    Mutating,
    /// Verdict recording ([`VERDICT_RECORDING_TOOLS`]) — role-gated.
    Verdict,
    /// Unclassified (Manager-only tools, MCP names, aliases): decided purely by
    /// the caller's allow-list / registry entry.
    Other,
}

/// Classify a tool name (aliases and `terminal__` variants included).
pub fn blueprint_tool_class(name: &str) -> BlueprintToolClass {
    if is_verdict_recording_tool(name) {
        return BlueprintToolClass::Verdict;
    }
    if ALWAYS_ADVERTISED_TOOLS.contains(&name) {
        return BlueprintToolClass::AlwaysAdvertised;
    }
    let norm = harness::normalize_tool_name(name);
    if INSPECTION_TOOLS.contains(&norm.as_str()) || INSPECTION_TOOLS.contains(&name) {
        BlueprintToolClass::Inspection
    } else if MUTATING_TOOLS.contains(&norm.as_str()) || MUTATING_TOOLS.contains(&name) {
        BlueprintToolClass::Mutating
    } else {
        BlueprintToolClass::Other
    }
}

/// `true` when `name` names a verdict-recording tool.
///
/// The answer comes from the crate's single owner of verdict spellings
/// ([`crate::agents::validation::is_leave_verdict_tool`] → [`tool_names::is_leave_verdict_tool_name`],
/// i.e. the verdict rows of [`tool_names::TOOL_ALIAS_TABLE`] plus the
/// [`tool_names::LEAVE_VERDICT_NAME_SUFFIXES`] rule). Gate t-064 removed the
/// hand-maintained [`VERDICT_RECORDING_TOOLS`] lookup from here: a locally kept
/// list can only drift from the owner, and a spelling the owner accepts but this
/// list missed would silently re-open the verdict class to a role the role gate
/// refuses.
pub fn is_verdict_recording_tool(name: &str) -> bool {
    is_leave_verdict_tool(name)
}

/// The fix loop's one and only question about verdict authority. It delegates to
/// the crate's **public** role gate [`may_record_verdict`] instead of restating
/// it: a caller without a role (the Manager) or with any non-validator role has
/// no verdict authority, and no allow-list or prompt blueprint can widen that.
pub fn caller_may_record_verdict(caller: &harness::ToolCaller) -> bool {
    let registry = SpecialistRegistry::canonical();
    caller
        .agent()
        .is_some_and(|agent| may_record_verdict(agent, &registry))
}

/// The fail-closed verdict rejection, spelled with the crate's existing
/// verdict-rejection vocabulary: a typed [`ToolError::Forbidden`] (the very
/// variant the agent-loop gate in `runner/execution.rs` reports) rendered into
/// the tool-error text the caller sees. No new error type is invented.
fn verdict_role_rejection(caller: &harness::ToolCaller, tool: &str) -> String {
    let forbidden = ToolError::Forbidden {
        tool: tool.to_string(),
        caller: caller.role_name(),
    };
    format!(
        "VERDICT REJECTED — {forbidden}. Only the {} role may record a validation verdict: {} cannot approve or reject its own deliverable. No verdict was recorded and no verdict file was touched — the deliverable must still pass the automated validator.",
        Agent::Validator.as_str(),
        caller.role_name(),
    )
}

/// Role-aware blueprint allow-list (gate t-033e): an explicit prompt/blueprint
/// allow-list may name anything, but a verdict-recording tool is **dropped** for
/// any role the public gate refuses. A blueprint is a description of intent,
/// never an authority.
pub fn role_filtered_blueprint(
    caller: &harness::ToolCaller,
    allowed_tools: &[String],
) -> Vec<String> {
    let may_record = caller_may_record_verdict(caller);
    allowed_tools
        .iter()
        .filter(|name| may_record || !is_verdict_recording_tool(name))
        .cloned()
        .collect()
}

/// [`assemble_tools`] driven by the caller itself (its blueprint allow-list is
/// filtered through [`role_filtered_blueprint`] first).
pub fn assemble_tools_for_caller(
    caller: &harness::ToolCaller,
    entry_allows: impl Fn(&str) -> bool,
    mcp_servers: &[String],
) -> Vec<ToolDef> {
    let blueprint = caller
        .allowed_tools()
        .map(|allowed| role_filtered_blueprint(caller, allowed));
    assemble_tools(blueprint.as_deref(), entry_allows, mcp_servers)
}

/// The tool schemas one fix-loop run may actually advertise (gate t-033e).
///
/// Even when a caller assembled its tool list from a blueprint that named a
/// verdict tool, a role rejected by [`may_record_verdict`] never sees a verdict
/// tool in the schema list: the fix loop strips the whole verdict class from its
/// own advertised view. Fail-closed by construction — a tool the model is never
/// told about cannot be requested from the blueprint.
pub fn advertised_tools_for_caller(
    tools: &[ToolDef],
    caller: &harness::ToolCaller,
) -> Vec<ToolDef> {
    let may_record = caller_may_record_verdict(caller);
    tools
        .iter()
        .filter(|tool| may_record || !is_verdict_recording_tool(&tool.function.name))
        .cloned()
        .collect()
}

/// Charge `engine` with the exact tool schemas of one fix-loop run (bug M7,
/// gate t-064) and return the charged schema token count.
///
/// [`crate::manager::context::ContextEngine::should_compact`] measures the whole
/// request, but its tool-schema term is only as good as what the caller declares:
/// an engine that never calls [`ContextEngine::set_tools`](crate::manager::context::ContextEngine::set_tools)
/// budgets the transcript alone and therefore under-prices every turn by the size
/// of the advertised schemas. The value passed here **must** be the same list
/// handed to [`build_turn_request`] — never a superset (which would compact
/// transcript away too early) and never a subset (which would let an over-budget
/// request through).
///
/// `run_fix_loop` calls it once per run with its [`advertised_tools_for_caller`]
/// view, which is exactly what the run sends on every turn. The specialist turn in
/// `runner/execution.rs` builds its own advertised list through
/// `execution::specialist_advertised_tools`; that list is likewise the one it
/// sends, so charging the engine with it here is the same one-liner.
pub fn charge_engine_tool_schema(engine: &mut ContextEngine, advertised: &[ToolDef]) -> usize {
    let previous = engine.tool_schema_tokens();
    engine.set_tools(advertised);
    let charged = engine.tool_schema_tokens();
    if previous != charged {
        tracing::debug!(
            "fix-loop tool-schema budget {}: {previous} -> {charged} tokens across {} advertised tool(s)",
            if previous == 0 {
                "charged"
            } else {
                "re-priced"
            },
            advertised.len(),
        );
    }
    charged
}

/// Assemble the tool list: `ToolDef::default_tools()` filtered by either an
/// explicit prompt-blueprint allow-list or the registry entry's namespaces,
/// plus MCP tools for the given servers. (Previously duplicated ×3.)
///
/// **Verdict-class hardening (gate t-033e):** a prompt/blueprint allow-list can
/// never grant the verdict class on its own — only the registry/entry authority
/// can even propose it, and the fix loop then strips it from the advertised view
/// of any role [`may_record_verdict`] refuses (see [`advertised_tools_for_caller`]).
pub fn assemble_tools(
    allowed_tools: Option<&[String]>,
    entry_allows: impl Fn(&str) -> bool,
    mcp_servers: &[String],
) -> Vec<ToolDef> {
    let mut tools = Vec::new();
    for tool in ToolDef::default_tools() {
        let name = tool.function.name.as_str();
        let is_allowed = match blueprint_tool_class(name) {
            BlueprintToolClass::AlwaysAdvertised => true,
            // Not grantable by a blueprint alone: the entry/registry authority has
            // to propose it, and the loop-level role gate has to accept the role.
            BlueprintToolClass::Verdict => entry_allows(name),
            _ => {
                if let Some(allowed) = allowed_tools {
                    allowed.iter().any(|t| {
                        let norm = harness::normalize_tool_name(t);
                        norm == name || t == name
                    })
                } else {
                    entry_allows(name)
                }
            }
        };
        if is_allowed {
            tools.push(tool);
        }
    }
    // Advertising de-dup (gate t-033c, folded from t-034c): the schema list
    // handed to a model must be the **policy-filtered** MCP view, never the raw
    // registry. `harness::allowed_mcp_tools` is the single advertising entry
    // point (`src/harness/mod.rs`), so a name the MCP name policy refused — or
    // one that collides with a built-in — can never even be offered to a model,
    // let alone dispatched by a worker that copied it out of its own tool list.
    for tool in harness::allowed_mcp_tools(mcp_servers) {
        tools.push(ToolDef::from_mcp(&tool));
    }
    tools
}

/// Resolve the validator backend/token/model fallback chain
/// (specialist override → validator entry → global config).
/// Previously duplicated ×2 in validation.rs.
pub fn resolve_validator_backend(
    cfg: &crate::config::Config,
    specialist: crate::agents::Agent,
) -> (String, String, String) {
    let specialist_cfg = cfg.orchestration.specialists.get(specialist.as_str());
    let validator_cfg = cfg
        .orchestration
        .specialists
        .get(crate::agents::Agent::Validator.as_str());
    let backend = specialist_cfg
        .and_then(|sc| sc.validator_backend_url.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.backend_url.as_ref()))
        .unwrap_or(&cfg.backend_url)
        .to_string();
    let token = specialist_cfg
        .and_then(|sc| sc.validator_auth_token.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.auth_token.as_ref()))
        .unwrap_or(&cfg.auth_token)
        .to_string();
    let model = specialist_cfg
        .and_then(|sc| sc.validator_model.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.model.as_ref()))
        .cloned()
        .unwrap_or_else(|| cfg.model.clone());
    (backend, token, model)
}

/// Shared tool-dispatch core: monitor intervention check → cancellation check
/// → `dispatch_for_async_with_engine`. Returns `(content, execution_succeeded)`
/// where `content` starts with `"ERROR: "` on failure. Previously duplicated
/// across the deliverable loop, the plan loop, and the specialist loop.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_tool_call(
    monitor: &mut HarnessMonitor,
    tc: &ToolCall,
    caller: harness::ToolCaller,
    engine: &mut ContextEngine,
    token: &tokio_util::sync::CancellationToken,
    abort_log_prefix: &str,
    emit_tool_status: bool,
    error_prefix: &str,
    intervention_fallback: &str,
) -> Option<(String, bool)> {
    // Verdict dispatch gate (gate t-033e): a fix loop must never *dispatch* a
    // verdict tool for a role that [`may_record_verdict`] refuses, even if a
    // blueprint allow-list named it and the tool therefore reached this point.
    // The refusal is a typed [`ToolError::Forbidden`] surfaced as a tool error —
    // logged, visible to the caller, and never handed to the dispatcher, so no
    // verdict state is touched.
    if is_verdict_recording_tool(&tc.function.name) && !caller_may_record_verdict(&caller) {
        let rejection = verdict_role_rejection(&caller, &tc.function.name);
        tracing::warn!(
            "{abort_log_prefix} refused to dispatch verdict tool {} for caller '{}'",
            tc.function.name,
            caller.role_name()
        );
        return Some((format!("{error_prefix}{rejection}"), false));
    }

    if tc.is_malformed() {
        let err_msg = format!(
            "{error_prefix}Invalid or truncated arguments for tool '{}': output was cut off or contained unterminated JSON. Please reissue the tool call with complete, valid JSON arguments.",
            tc.function.name
        );
        tracing::warn!(
            "{abort_log_prefix} tool {} had malformed/truncated arguments",
            tc.function.name
        );
        return Some((err_msg, false));
    }

    let args_val: serde_json::Value = serde_json::from_str(&tc.function.arguments)
        .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
    if emit_tool_status {
        let desc = format_tool_args_preview(&tc.function.name, &args_val);
        emit_status(format!(
            "{}: running {}({desc})",
            abort_log_prefix, tc.function.name
        ));
    }
    let full_args = format_tool_args_full(&tc.function.name, &args_val);
    tracing::info!(
        "{abort_log_prefix} invoking tool: {}({full_args})",
        tc.function.name
    );

    let intervention = monitor.observe_tool(&tc.function.name, &args_val);
    let content = match intervention {
        Intervention::Block | Intervention::Cut => {
            // `intervention_fallback` is a template containing `{tool}`.
            let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                format!(
                    "{error_prefix}{}",
                    intervention_fallback.replace("{tool}", &tc.function.name)
                )
            });
            tracing::warn!(
                "{abort_log_prefix} tool {} blocked by repetition detector",
                tc.function.name
            );
            err_msg
        }
        Intervention::None => {
            if token.is_cancelled() || is_current_or_global_cancelled() {
                tracing::warn!(
                    "{abort_log_prefix}: aborted before dispatching tool {}",
                    tc.function.name
                );
                return None;
            }
            let invocation = harness::ToolInvocation {
                name: tc.function.name.clone(),
                arguments: args_val,
            };
            match harness::dispatch_for_async_with_engine(&invocation, caller, Some(engine)).await {
                Ok(r) => {
                    tracing::info!(
                        "{abort_log_prefix} tool {} completed with {} chars",
                        tc.function.name,
                        r.content.len()
                    );
                    r.content
                }
                Err(e) => {
                    tracing::warn!("{abort_log_prefix} tool {} error: {e}", tc.function.name);
                    format!("{error_prefix}{e}")
                }
            }
        }
    };
    let execution_succeeded = !content.starts_with(error_prefix);
    Some((content, execution_succeeded))
}

/// Append a tool result to the engine with the shared `rebirth` special-case:
/// a *successful* rebirth replaces the tool result with a user-level
/// checkpoint notice instead of a `Tool` message (a failed rebirth is kept as
/// a normal `Tool` error result).
pub fn append_tool_result(
    engine: &mut ContextEngine,
    tc: &ToolCall,
    content: String,
    execution_succeeded: bool,
    rebirth_notice: &str,
) {
    let is_rebirth = tc.function.name == tool_names::TOOL_REBIRTH;
    if is_rebirth && execution_succeeded {
        engine.append(Message::User {
            content: rebirth_notice.to_string(),
        });
    } else {
        engine.append(Message::Tool {
            tool_call_id: tc.id.clone(),
            content,
        });
    }
}

/// Drain the steering notices addressed to `worker_key` and append them to the
/// transcript, rendered by the crate's **single** notice renderer
/// ([`crate::orchestrator::render_notice_for_worker`]).
///
/// This is the one and only place in the crate that turns a
/// [`crate::orchestrator::SteerNotice`] into transcript text; the two inline
/// renderings this replaced (`runner/execution.rs` and the turn-start block here)
/// re-spelled the same format string, which is how the reply contract drifted
/// away from what `render_notice_for_worker` promises the worker (verbatim notice
/// id + the exact reply call).
///
/// `mid_turn` selects the drain seam:
/// * `false` — the turn-start drain, as before.
/// * `true` — the after-each-tool-round drain (t-048): a notice posted **while a
///   turn is in flight** is injected into the SAME turn, before compaction and
///   before a verdict/`break` can end the loop, instead of waiting for a turn
///   boundary the worker may never cross again.
///
/// Both seams route through the same exact-identity routing
/// (`drain_worker_notices` / [`crate::orchestrator::drain_worker_notices_mid_turn`]),
/// so a notice addressed to a *different* worker is never injected here — it stays
/// queued for that worker.
///
/// Returns the number of notices injected.
fn inject_drained_notices(
    engine: &mut ContextEngine,
    notices: Vec<crate::orchestrator::SteerNotice>,
) -> usize {
    let injected = notices.len();
    for notice in notices {
        engine.append(Message::User {
            content: crate::orchestrator::render_notice_for_worker(&notice),
        });
    }
    injected
}

/// Turn-start notice drain + injection (see [`inject_drained_notices`]).
pub fn inject_worker_notices_at_turn_start(engine: &mut ContextEngine, worker_key: &str) -> usize {
    inject_drained_notices(
        engine,
        crate::orchestrator::drain_worker_notices(worker_key),
    )
}

/// Mid-turn notice drain + injection (see [`inject_drained_notices`]).
///
/// Called after every tool round of a live worker loop, so a steer posted
/// mid-turn reaches the model on that worker's very next request.
pub fn inject_worker_notices_mid_turn(engine: &mut ContextEngine, worker_key: &str) -> usize {
    let injected = inject_drained_notices(
        engine,
        crate::orchestrator::drain_worker_notices_mid_turn(worker_key),
    );
    if injected > 0 {
        tracing::info!(
            "{worker_key}: {injected} steering notice(s) injected mid-turn (same turn, before compaction)"
        );
    }
    injected
}

/// The shared fix-loop driver: cancel check → worker context update → status
/// emit → turn-start notice drain → streaming LLM turn → abort/steer handling →
/// XML rescue → assistant folding → verdict detection → nudge-on-silence → tool
/// dispatch → mid-turn notice drain → compaction. One implementation for the
/// deliverable validator, the plan auditor, and (via `stream_single_turn`) the
/// specialist turn.
///
/// Returns `Err` on hard LLM errors and maps aborts to `FixLoopResult::Aborted`
/// (strict semantics — see module docs).
pub async fn run_fix_loop(
    p: &mut LoopParams<'_>,
    monitor: &mut HarnessMonitor,
    caller: harness::ToolCaller,
) -> Result<FixLoopResult, anyhow::Error> {
    let mut rep_detector =
        RepetitionDetector::new(p.mon_cfg.repetition_threshold, p.mon_cfg.min_pattern_len);
    let mut verdict_nudge_count = 0usize;
    let mut consecutive_malformed_tool_calls = 0u32;
    let mut turn = 0usize;

    // Role gate for the whole run (gate t-033e): the fix loop asks the crate's
    // public gate [`may_record_verdict`] exactly once, and everything verdict-shaped
    // downstream (what is advertised, what is nudged, what is consumed, what is
    // dispatched) is decided by that single answer. A blueprint allow-list naming a
    // verdict tool cannot widen it.
    let verdict_authority = caller_may_record_verdict(&caller);
    // The advertised schema view — the blueprint the model actually sees.
    let advertised = advertised_tools_for_caller(&p.tools, &caller);
    // Gate t-064 (offender E): the request this loop sends carries exactly
    // `advertised` (see `build_turn_request` below), so the engine is charged with
    // exactly that list before its first budget decision. From here on
    // `should_compact()` / `should_advise_rebirth()` price the **real** request
    // (transcript + tool schemas) instead of the transcript alone.
    charge_engine_tool_schema(p.engine, &advertised);

    loop {
        turn += 1;

        // Fix-loop bounds (gate t-033c): one run may not spin forever. The
        // iteration cap and the wall-clock deadline are evaluated **before** the
        // round's LLM call, so the run is torn down instead of paying for yet
        // another turn, and both exhaustion paths return an explicit failed
        // outcome (`Exhausted` + reason + `warn!`) — never a silent approval.
        if p.bounds.round_cap_reached(turn) {
            let reason = p.bounds.round_cap_reason();
            tracing::warn!(
                "{}: fix-loop bound exhausted — {reason} (round {turn}, cap {})",
                p.tag,
                p.bounds.max_rounds()
            );
            emit_status(format!("{}: {reason}", p.tag));
            return Ok(FixLoopResult::Exhausted { reason });
        }
        if let Some(kind) = p.bounds.deadline_expired() {
            let reason = p.bounds.deadline_reason(kind);
            tracing::warn!("{}: fix-loop deadline exhausted — {reason}", p.tag);
            emit_status(format!("{}: {reason}", p.tag));
            return Ok(FixLoopResult::Exhausted { reason });
        }

        // Behavior-preserving: the original validator loops checked only their
        // own token here (not the global token). The stricter abort *semantics*
        // (Err, not Ok(false)) is applied below; the *trigger* stays token-only
        // to avoid new aborts from unrelated global cancels in parallel runs.
        if p.token.is_cancelled() {
            tracing::warn!("{}: aborted by cancellation token", p.tag);
            return Ok(FixLoopResult::Aborted);
        }
        update_active_worker_context(&p.worker_key, p.engine.token_count());
        emit_status(format!("{} (turn {turn})", p.status_template));

        // Turn-start notice drain — rendered by the crate's single notice
        // renderer (see `inject_worker_notices_at_turn_start`).
        inject_worker_notices_at_turn_start(p.engine, &p.worker_key);

        let req = build_turn_request(&p.model, p.engine, &advertised, p.cfg, p.temperature);

        let max_tokens = p.mon_cfg.max_stream_tokens.max(256);
        let max_thinking_tokens = p.mon_cfg.max_thinking_tokens.max(256);
        let mut sink = PreemptibleStreamSink::register_full(
            &p.tag,
            Some(p.worker_name.clone()),
            p.task_id.clone(),
            Some(p.token.clone()),
            &p.model,
        );
        let stream_out = llm::chat_stream_resumable(
            p.client,
            &req,
            &mut sink,
            max_tokens,
            max_thinking_tokens,
            &mut rep_detector,
            false,
            Some(p.token),
        )
        .await;

        let out = match stream_out {
            Ok(o) => o,
            Err(e) => {
                // Strict semantics: a cancelled stream is an abort; any other
                // transport/backend error is a hard failure (previously the
                // deliverable loop broke out and *assumed approved* — unsafe).
                if p.token.is_cancelled() || is_current_or_global_cancelled() {
                    tracing::warn!("{}: aborted during LLM call", p.tag);
                    return Ok(FixLoopResult::Aborted);
                }
                tracing::error!("{} LLM chat call error on turn {turn}: {e:?}", p.tag);
                return Err(e);
            }
        };

        // Behavior-preserving: steer + own-token only (matches both original
        // validator loops); global cancel is honored at the tool-dispatch step.
        if out.was_aborted_by_steer || p.token.is_cancelled() {
            tracing::warn!("{}: aborted during LLM call", p.tag);
            return Ok(FixLoopResult::Aborted);
        }

        let reply = out.reply;
        if out.budget_exceeded {
            tracing::warn!(
                "{}: maximum single-turn output budget of {max_tokens} tokens exceeded",
                p.tag
            );
        }
        if out.thinking_budget_exceeded {
            tracing::warn!(
                "{}: maximum single-turn reasoning budget of {max_thinking_tokens} tokens exceeded",
                p.tag
            );
            crate::orchestrator::emit_status(format!(
                "{}: reasoning budget ({max_thinking_tokens} tokens) reached — nudging out of thinking",
                p.tag
            ));
        }

        let mut tool_calls = reply.tool_calls.clone();
        if tool_calls.is_empty() && p.cfg.enable_xml_rescue {
            let rescued = monitor.rescue_xml(&reply.content);
            if !rescued.is_empty() {
                tool_calls = rescued;
            }
        }

        p.engine.append(Message::Assistant {
            content: Some(reply.content.clone()),
            reasoning_content: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning.clone())
            },
            tool_calls: tool_calls.clone(),
        });

        for tc in &tool_calls {
            if is_leave_verdict_tool(&tc.function.name) {
                // **Role gate (gate t-033e).** This loop does not record a verdict
                // for just any caller: only a caller accepted by the crate's public
                // gate [`may_record_verdict`] may conclude a fix-loop run with one.
                // Anything else fails closed — the call is not consumed as a
                // verdict, no verdict is logged, nothing is dispatched, and the run
                // ends NOT approved carrying the typed rejection
                // ([`ToolError::Forbidden`]). No blueprint allow-list can widen this.
                if !verdict_authority {
                    let rejection = verdict_role_rejection(&caller, &tc.function.name);
                    append_tool_result(
                        p.engine,
                        tc,
                        format!("{TOOL_ERROR_PREFIX}{rejection}"),
                        false,
                        &p.rebirth_notice,
                    );
                    tracing::warn!(
                        "{}: verdict call refused by the role gate (caller '{}' may not record verdicts) — failing closed",
                        p.tag,
                        caller.role_name()
                    );
                    emit_status(format!(
                        "{}: VERDICT REJECTED — only {} may record validation verdicts",
                        p.tag,
                        Agent::Validator.as_str()
                    ));
                    update_active_worker_context(&p.worker_key, p.engine.token_count());
                    return Ok(FixLoopResult::Verdict {
                        approved: false,
                        critique: rejection,
                    });
                }

                // The verdict tool is **terminal-only** inside this loop (gate
                // t-033c): it is consumed here and never handed to the tool
                // dispatcher, so no fix-loop switch can make the loop execute a
                // verdict call on a worker's behalf. (A *worker's* own verdict
                // attempt is refused further upstream by the identity gate
                // `runner/execution.rs::may_record_verdict`, t-033b, and inside
                // this loop by the gate above, t-033e.)
                let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
                let (approved, critique) = match parse_verdict_args(&args_val) {
                    Some(v) => v,
                    // Fail-closed guard (gate t-033c): `parse_verdict_args`
                    // resolves every payload to a verdict, so this arm exists
                    // only to guarantee that a future regression can never
                    // resurrect the historical "assume approved" default.
                    None => (false, NO_EXPLICIT_APPROVAL_REASON.to_string()),
                };
                crate::debug_log::log_validation_verdict(&p.verdict_log_role, approved, &critique);
                tracing::info!(
                    "Validator recorded verdict via {}: approved={}, critique:\n{}",
                    tool_names::TOOL_LEAVE_VERDICT,
                    approved,
                    critique
                );
                return Ok(FixLoopResult::Verdict { approved, critique });
            }
        }

        if tool_calls.is_empty() {
            // A role without verdict authority is never nudged towards a verdict
            // tool (it is not even advertised to it, gate t-033e): it concludes the
            // run immediately with the fail-closed outcome below instead.
            if verdict_authority && verdict_nudge_count < VERDICT_REMINDERS {
                verdict_nudge_count += 1;
                let notice = if out.thinking_budget_exceeded {
                    format!(
                        "SYSTEM NOTICE: Maximum reasoning budget of {max_thinking_tokens} tokens reached for this turn. Stop internal reasoning immediately. You have not submitted a verdict using the '{}' tool (reminder {verdict_nudge_count}/{VERDICT_REMINDERS}). Proceed directly to call the '{}' tool with verdict ('APPROVED' or 'REJECTED') and comments, or invoke required inspection tools.",
                        tool_names::TOOL_LEAVE_VERDICT,
                        tool_names::TOOL_LEAVE_VERDICT,
                    )
                } else {
                    format!(
                        "System: You have not submitted a verdict using the '{}' tool (reminder {verdict_nudge_count}/{VERDICT_REMINDERS}). Do not output text. If your analysis and verification are complete, you MUST call the '{}' tool with verdict ('APPROVED' or 'REJECTED') and comments. If you need to perform further verification, invoke the appropriate tools.",
                        tool_names::TOOL_LEAVE_VERDICT,
                        tool_names::TOOL_LEAVE_VERDICT,
                    )
                };
                p.engine.append(Message::User { content: notice });
                continue;
            } else {
                // **Fail-closed (contract change, gate t-033c).** This branch used
                // to end the run with `approved = true` ("assumed approved"), which
                // let a deliverable be certified by a validator that never
                // recorded any verdict. Not having recorded a verdict is not a
                // verdict: the run now ends NOT approved, with an explicit reason,
                // so the deliverable fails and its plan line stays unchecked.
                let reason = if verdict_authority {
                    format!(
                        "{} did not call {} after {VERDICT_REMINDERS} reminders: no validation verdict was recorded. {NO_EXPLICIT_APPROVAL_REASON}",
                        p.tag,
                        tool_names::TOOL_LEAVE_VERDICT,
                    )
                } else {
                    // Gate t-033e: a caller the public verdict gate refuses can
                    // never conclude this run with a verdict, so there is nothing
                    // to wait for — carry the typed rejection as the reason.
                    format!(
                        "{} — {NO_EXPLICIT_APPROVAL_REASON}",
                        verdict_role_rejection(&caller, tool_names::TOOL_LEAVE_VERDICT),
                    )
                };
                let outcome_note = if verdict_authority {
                    format!("no verdict after {VERDICT_REMINDERS} reminders")
                } else {
                    format!("caller '{}' has no verdict authority", caller.role_name())
                };
                tracing::warn!(
                    "{}: failing closed — {outcome_note} (deliverable NOT approved)",
                    p.tag
                );
                emit_status(format!(
                    "{}: NO VERDICT ({outcome_note}) — deliverable reported as NOT approved",
                    p.tag
                ));
                return Ok(FixLoopResult::Verdict {
                    approved: false,
                    critique: reason,
                });
            }
        }

        let mut turn_had_malformed = false;
        let mut turn_had_successful_tool_call = false;
        for tc in &tool_calls {
            if tc.is_malformed() {
                turn_had_malformed = true;
            }
            if p.token.is_cancelled() || is_current_or_global_cancelled() {
                tracing::warn!(
                    "{}: aborted before tool {}",
                    p.abort_log_prefix,
                    tc.function.name
                );
                return Ok(FixLoopResult::Aborted);
            }
            match dispatch_tool_call(
                monitor,
                tc,
                caller.clone(),
                p.engine,
                p.token,
                &p.abort_log_prefix,
                p.emit_tool_status,
                TOOL_ERROR_PREFIX,
                &repetition_intervention_fallback(),
            )
            .await
            {
                Some((content, succeeded)) => {
                    if succeeded {
                        turn_had_successful_tool_call = true;
                    }
                    append_tool_result(p.engine, tc, content, succeeded, &p.rebirth_notice);
                }
                None => {
                    return Ok(FixLoopResult::Aborted);
                }
            }
            update_active_worker_context(&p.worker_key, p.engine.token_count());
        }

        // Mid-turn notice drain (t-048): notices posted while this validator /
        // auditor turn was in flight are injected right after the tool round, in
        // the SAME turn — before compaction and before any of the turn's exit
        // paths end the loop — so the steer reaches this worker's very next
        // request instead of rotting in its inbox until the TTL sweep drops it.
        let notices_injected = inject_worker_notices_mid_turn(p.engine, &p.worker_key);

        // Wall-clock progress (gate t-033c): observable forward progress — a tool
        // call that actually completed, or a steering notice that reached this
        // worker — refreshes the stalled-run window of the deadline. A run that
        // only produces prose never resets it, so a spinning auditor is torn down
        // by the idle bound while a busy, productive one is never cut off.
        if turn_had_successful_tool_call || notices_injected > 0 {
            p.bounds.note_round_progress();
        }

        if turn_had_malformed {
            consecutive_malformed_tool_calls += 1;
            if consecutive_malformed_tool_calls >= 5 {
                tracing::warn!(
                    "{}: model produced malformed/truncated tool calls 5 times consecutively — terminating fix loop",
                    p.tag
                );
                return Ok(FixLoopResult::Verdict {
                    approved: false,
                    critique: "Specialist/Validator repeatedly produced malformed or truncated tool calls 5 times consecutively.".to_string(),
                });
            }
        } else if turn_had_successful_tool_call {
            consecutive_malformed_tool_calls = 0;
        }
        if p.engine.should_compact() {
            // Gate t-064: the outcome is never discarded. `CompactionOutcome` is
            // the only evidence of whether the 70% target was actually reached,
            // and `TargetUnreachable` means the run keeps working over budget —
            // the same lie `surface_compaction_outcome` removed on the Manager
            // path (t-063). The fix loop has no renderer, so it surfaces through
            // the orchestrator status channel (the worker line the UI already
            // reads) plus `tracing::warn!`.
            let outcome = p.engine.compact();
            tracing::info!("{}: automatic context compaction: {outcome:?}", p.tag);
            if let Some(notice) = fix_loop_compaction_notice(&p.tag, &outcome) {
                tracing::warn!("{notice}");
                emit_status(notice);
            }
        } else if p.engine.should_advise_rebirth() {
            p.engine.inject_rebirth_advisory();
        }
    }
}

/// The user-visible line for a fix-loop compaction that did **not** reach its
/// 70% target — `None` when it did (gate t-064).
///
/// Mirrors `src/ui/session.rs::surface_compaction_outcome`: this loop has no
/// renderer, so the caller routes the text through the orchestrator status
/// channel and `tracing::warn!`. It must never claim a compaction that did not
/// happen: [`CompactionOutcome::TargetUnreachable`] means everything trimmable was
/// trimmed and the transcript is still over the compaction threshold.
pub fn fix_loop_compaction_notice(tag: &str, outcome: &CompactionOutcome) -> Option<String> {
    if outcome.succeeded() {
        return None;
    }
    Some(format!(
        "{tag}: automatic context compaction could not reach its target — removed {} messages / {} tokens and the transcript is still {} tokens against a {}-token target, i.e. still above the compaction threshold. The run continues over budget; an explicit checkpoint pass is required.",
        outcome.messages_removed(),
        outcome.tokens_reclaimed(),
        outcome.final_tokens(),
        outcome.target(),
    ))
}

/// One streaming LLM turn (request build + sink registration +
/// `chat_stream_resumable` + abort classification). Used by the specialist
/// execution loop, which keeps its own tool-dispatch/revision state machine
/// but shares the streaming plumbing with the validator loops.
///
/// Returns `Err` on hard errors; `Ok(None)` on abort/cancellation.
#[allow(clippy::too_many_arguments)]
pub async fn stream_single_turn(
    client: &llm::ChatClient,
    req: &ChatRequest,
    tag: &str,
    worker_name: &str,
    model: &str,
    task_id: Option<String>,
    token: &tokio_util::sync::CancellationToken,
    max_tokens: usize,
    max_thinking_tokens: usize,
    rep_detector: &mut RepetitionDetector,
) -> Result<Option<llm::ResumableStreamOutput>, anyhow::Error> {
    let mut sink = PreemptibleStreamSink::register_full(
        tag,
        Some(worker_name.to_string()),
        task_id,
        Some(token.clone()),
        model,
    );
    let out = llm::chat_stream_resumable(
        client,
        req,
        &mut sink,
        max_tokens,
        max_thinking_tokens,
        rep_detector,
        false,
        Some(token),
    )
    .await;
    match out {
        Ok(o) => {
            if o.was_aborted_by_steer || token.is_cancelled() || is_current_or_global_cancelled() {
                tracing::warn!("{tag}: aborted during LLM call");
                Ok(None)
            } else {
                Ok(Some(o))
            }
        }
        Err(e) => {
            if token.is_cancelled() || is_current_or_global_cancelled() {
                tracing::warn!("{tag}: aborted during LLM call");
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// Register the active worker for a fix-loop run (shared scaffolding).
pub fn register_loop_worker(
    task_id: Option<String>,
    worker_name: String,
    description: String,
    token: &tokio_util::sync::CancellationToken,
) -> ActiveWorkerGuard {
    register_active_worker_with_token(task_id, worker_name, description, Some(token.clone()))
}

#[cfg(test)]
mod tests {
    use super::{inject_worker_notices_at_turn_start, inject_worker_notices_mid_turn};
    use crate::manager::{ContextEngine, ContextEngineFactory};
    use crate::orchestrator::notice::{TEST_NOTICE_MUTEX, has_worker_inbox, worker_inbox_len};
    use crate::orchestrator::{post_notice_to_worker, render_notice_for_worker};
    use crate::tool_names::TOOL_REPLY_TO_ARBITRATOR;
    use crate::types::Message;

    fn specialist_engine() -> ContextEngine {
        ContextEngineFactory::new(64_000)
            .specialist_context("role prompt".to_string(), "brief".to_string())
    }

    fn user_texts(engine: &ContextEngine) -> Vec<String> {
        engine
            .messages()
            .iter()
            .filter_map(|msg| match msg {
                Message::User { content } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    /// The mid-turn seam (t-048): a notice posted while a turn is in flight is
    /// drained and injected for the worker it addresses, and the injected text is
    /// byte-identical to the crate's single notice rendering. A notice addressed
    /// to another worker is never injected — it stays queued for that worker.
    #[tokio::test]
    async fn mid_turn_drain_injects_only_its_own_worker_with_the_single_rendering() {
        let _notice_lock = TEST_NOTICE_MUTEX.lock().await;

        let mine = "midturnfix-t-1";
        let foreign = "otherfix-t-2";
        let posted =
            post_notice_to_worker(mine, "prefer the streaming path", Some("notice-mt-self"));
        let foreign_notice =
            post_notice_to_worker(foreign, "not addressed to you", Some("notice-mt-foreign"));

        let mut engine = specialist_engine();
        let before = engine.messages().len();

        let injected = inject_worker_notices_mid_turn(&mut engine, mine);

        assert!(
            injected >= 1,
            "the notice addressed to {mine} must be injected mid-turn"
        );
        let texts = user_texts(&engine);
        assert!(
            texts.contains(&render_notice_for_worker(&posted)),
            "the injected text must be exactly what the crate's single renderer produces"
        );
        let foreign_render = render_notice_for_worker(&foreign_notice);
        assert!(
            !texts.contains(&foreign_render),
            "a notice addressed to another worker must never be injected for this worker"
        );
        assert_eq!(
            engine.messages().len(),
            before + injected,
            "each drained notice is injected as exactly one transcript message"
        );
        assert_eq!(
            worker_inbox_len(mine),
            0,
            "the mid-turn drain consumes what it injected"
        );
        assert_eq!(
            worker_inbox_len(foreign),
            1,
            "the other worker's queue is untouched"
        );
    }

    /// De-duplication (t-048): the turn-start seam and the mid-turn seam emit the
    /// **same** rendering — the one from `render_notice_for_worker` — so both turn
    /// boundaries honour one identical reply contract (notice id verbatim + the
    /// exact reply call).
    #[tokio::test]
    async fn turn_start_and_mid_turn_seams_share_one_rendering() {
        let _notice_lock = TEST_NOTICE_MUTEX.lock().await;

        let start_key = "renderstart-t-1";
        let mid_key = "rendermid-t-1";
        let at_start = post_notice_to_worker(
            start_key,
            "steer at turn start",
            Some("notice-render-start"),
        );
        let at_mid = post_notice_to_worker(mid_key, "steer mid turn", Some("notice-render-mid"));

        let mut engine_start = specialist_engine();
        let mut engine_mid = specialist_engine();
        assert_eq!(
            inject_worker_notices_at_turn_start(&mut engine_start, start_key),
            1
        );
        assert_eq!(inject_worker_notices_mid_turn(&mut engine_mid, mid_key), 1);

        let rendered_start = render_notice_for_worker(&at_start);
        let rendered_mid = render_notice_for_worker(&at_mid);
        let texts_start = user_texts(&engine_start);
        let texts_mid = user_texts(&engine_mid);
        assert!(texts_start.contains(&rendered_start));
        assert!(texts_mid.contains(&rendered_mid));

        let notice_text = |texts: &[String], notice_id: &str| -> String {
            texts
                .iter()
                .find(|text| text.contains(notice_id))
                .cloned()
                .unwrap_or_else(|| panic!("notice {notice_id} was not injected as a user message"))
        };
        let text_start = notice_text(&texts_start, &at_start.notice_id);
        let text_mid = notice_text(&texts_mid, &at_mid.notice_id);

        // Same contract from both seams: the shared reply tool and the strict
        // reply wording are present, and the two renderings differ only in the
        // notice id / inquiry fields.
        for text in [text_start.as_str(), text_mid.as_str()] {
            assert!(text.contains(TOOL_REPLY_TO_ARBITRATOR));
            assert!(text.contains("Reply to this notice id only"));
        }
        let shape = |text: &str, notice_id: &str, inquiry: &str| -> String {
            text.replace(notice_id, "<id>")
                .replace(inquiry, "<inquiry>")
        };
        assert_eq!(
            shape(&text_start, &at_start.notice_id, &at_start.user_inquiry),
            shape(&text_mid, &at_mid.notice_id, &at_mid.user_inquiry),
            "both drain seams must render the notice identically"
        );
        assert!(
            !has_worker_inbox(start_key) && !has_worker_inbox(mid_key),
            "a drained inbox is reclaimed, so finished workers leave no entries behind"
        );
    }

    // ── Fix-loop bounds, fail-closed verdicts and verdict-tool routing (t-033c) ──

    use super::{
        FIX_LOOP_HARD_CAP_SECS, FIX_LOOP_IDLE_LIMIT_SECS, FixLoopBounds, FixLoopResult, LoopParams,
        MAX_FIX_LOOP_ROUNDS, VERDICT_REMINDERS, register_loop_worker, run_fix_loop,
    };
    use crate::agents::Agent;
    use crate::agents::validation::NO_EXPLICIT_APPROVAL_REASON;
    use crate::config::{Config, MonitoringConfig};
    use crate::harness::monitor::HarnessMonitor;
    use crate::manager::r#loop::{DeadlineKind, TURN_HARD_CAP_SECS, TURN_WATCHDOG_SECS};
    use crate::tool_names::{TOOL_LEAVE_VERDICT, TOOL_READ_FILE, TOOL_REBIRTH, TOOL_WRITE_FILE};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sse_text(text: &str) -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "id": "chatcmpl-fixloop",
                "choices": [{
                    "delta": { "content": text },
                    "finish_reason": "stop"
                }]
            })
        )
    }

    /// One SSE response carrying several tool calls, in wire order.
    fn sse_tool_calls(calls: &[(&str, &str, serde_json::Value)]) -> String {
        let items: Vec<serde_json::Value> = calls
            .iter()
            .enumerate()
            .map(|(idx, (id, name, args))| {
                serde_json::json!({
                    "index": idx,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args.to_string() }
                })
            })
            .collect();
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "id": "chatcmpl-fixloop",
                "choices": [{
                    "delta": { "content": null, "tool_calls": items },
                    "finish_reason": "tool_calls"
                }]
            })
        )
    }

    /// Replay `turns` in order, falling back to a sentinel reply, and count the
    /// LLM calls so a test can prove how many rounds the loop was allowed to take.
    async fn mock_turns(turns: Vec<String>) -> (MockServer, Arc<AtomicUsize>) {
        let server = MockServer::start().await;
        let counter = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&counter);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_req: &wiremock::Request| {
                let idx = captured.fetch_add(1, Ordering::SeqCst);
                let body = turns
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| sse_text("Unexpected call"));
                ResponseTemplate::new(200).set_body_string(body)
            })
            .mount(&server)
            .await;
        (server, counter)
    }

    /// Drive [`run_fix_loop`] against a replaying mock backend.
    ///
    /// `caller` is deliberately chosen by the test: a verdict-routing assertion
    /// must be made with a caller that is *allowed* to run the tool in question,
    /// otherwise "it was not dispatched" could be true for the wrong reason.
    async fn drive(
        server: &MockServer,
        bounds: FixLoopBounds,
        caller: crate::harness::ToolCaller,
        tag: &str,
    ) -> FixLoopResult {
        let cfg = Config::default();
        let mon_cfg = MonitoringConfig::default();
        let backend = format!("{}/v1", server.uri());
        let client = crate::llm::ChatClient::new_with_token(&backend, "test-model", "test-token");
        let token = CancellationToken::new();
        let guard = register_loop_worker(
            Some("t-bounds".to_string()),
            "validator-coder".to_string(),
            "bounded fix-loop run".to_string(),
            &token,
        );
        let mut engine = ContextEngineFactory::new(64_000)
            .specialist_context("validator role prompt".to_string(), "brief".to_string());
        let mut params = LoopParams {
            client: &client,
            model: "test-model".to_string(),
            tag: tag.to_string(),
            worker_name: "validator-coder".to_string(),
            task_id: Some("t-bounds".to_string()),
            worker_key: guard.0.clone(),
            engine: &mut engine,
            tools: Vec::new(),
            token: &token,
            mon_cfg: &mon_cfg,
            cfg: &cfg,
            temperature: 0.0,
            status_template: format!("{tag}: evaluating deliverable..."),
            verdict_log_role: "coder".to_string(),
            bounds,
            abort_log_prefix: tag.to_string(),
            emit_tool_status: false,
            rebirth_notice: format!("({TOOL_REBIRTH} checkpoint accepted)"),
        };
        let mut monitor = HarnessMonitor::new_with_config(
            std::sync::Arc::new(crate::harness::HarnessStats::new()),
            &mon_cfg,
        );
        run_fix_loop(&mut params, &mut monitor, caller)
            .await
            .expect("a bounded or verdict-less run is an outcome, never a hard error")
    }

    fn read_missing_file_call() -> String {
        sse_tool_calls(&[(
            "call_read",
            TOOL_READ_FILE,
            serde_json::json!({"path": "definitely-missing-file.rs"}),
        )])
    }

    /// Work item 1 — the iteration cap. The loop used to have no bound at its own
    /// level: a validator that keeps calling tools could spin forever.
    #[tokio::test]
    async fn iteration_cap_stops_the_run_with_an_explicit_failed_outcome() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let outcome = crate::harness::with_workspace_root(root, async {
            let (server, counter) =
                mock_turns(vec![read_missing_file_call(); MAX_FIX_LOOP_ROUNDS]).await;
            let bounds = FixLoopBounds::at(
                Instant::now(),
                2,
                Duration::from_secs(FIX_LOOP_IDLE_LIMIT_SECS),
                Duration::from_secs(FIX_LOOP_HARD_CAP_SECS),
            );
            let outcome = drive(
                &server,
                bounds,
                crate::harness::ToolCaller::Specialist(Agent::Validator),
                "validator-coder-cap",
            )
            .await;
            assert_eq!(
                counter.load(Ordering::SeqCst),
                2,
                "the cap must stop the loop after exactly max_rounds LLM rounds"
            );
            outcome
        })
        .await;

        match outcome {
            FixLoopResult::Exhausted { reason } => {
                assert!(
                    reason.contains("fix-loop iteration cap") && reason.contains("2 rounds"),
                    "the reason must name the blown bound: {reason}"
                );
            }
            other => {
                panic!("an exhausted round cap must be an explicit failed outcome, got {other:?}")
            }
        }
    }

    /// Work item 1 — the wall-clock deadline, enforced through the crate's own
    /// [`DeadlineKind`] vocabulary rather than a second timer.
    #[tokio::test]
    async fn wall_clock_deadline_stops_the_run_before_any_round() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let outcome = crate::harness::with_workspace_root(root, async {
            let (server, counter) = mock_turns(vec![sse_text("still auditing…")]).await;
            // Armed one hour in the past: the absolute bound is already blown.
            let bounds = FixLoopBounds::at(
                Instant::now() - Duration::from_secs(3 * 60 * 60),
                MAX_FIX_LOOP_ROUNDS,
                Duration::from_secs(FIX_LOOP_IDLE_LIMIT_SECS),
                Duration::from_secs(FIX_LOOP_HARD_CAP_SECS),
            );
            let outcome = drive(
                &server,
                bounds,
                crate::harness::ToolCaller::Specialist(Agent::Validator),
                "validator-coder-deadline",
            )
            .await;
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "a blown deadline must be caught before paying for another LLM round"
            );
            outcome
        })
        .await;

        match outcome {
            FixLoopResult::Exhausted { reason } => {
                assert!(
                    reason.contains(DeadlineKind::HardCap.label()),
                    "the reason must come from the crate's deadline vocabulary: {reason}"
                );
                assert!(
                    reason.contains("not approved"),
                    "a blown deadline must be reported as not approved: {reason}"
                );
            }
            FixLoopResult::Verdict { approved, .. } => {
                panic!("a blown deadline must not resolve to a verdict (approved={approved})")
            }
            FixLoopResult::Aborted => panic!("a blown deadline is not a cancellation abort"),
        }
    }

    /// Work item 1 — the bounds themselves: idle bound reuses the crate's turn
    /// watchdog, the hard cap stays strictly inside the enclosing turn's cap, and
    /// both deadline kinds are distinguishable.
    #[test]
    fn fix_loop_bounds_reuse_the_crate_deadline_machinery() {
        assert_eq!(MAX_FIX_LOOP_ROUNDS, 40, "the documented round cap");
        assert_eq!(
            FIX_LOOP_IDLE_LIMIT_SECS, TURN_WATCHDOG_SECS,
            "the fix loop must reuse the crate's watchdog bound, not invent one"
        );
        // The caps are read through locals so the comparison is evaluated on data
        // rather than folded into a compile-time constant.
        let fix_loop_hard_cap = FIX_LOOP_HARD_CAP_SECS;
        let turn_hard_cap = TURN_HARD_CAP_SECS;
        assert!(
            fix_loop_hard_cap < turn_hard_cap,
            "a fix loop must not be able to outlive the turn that owns it \
             ({fix_loop_hard_cap}s vs {turn_hard_cap}s)"
        );

        let start = Instant::now();
        let idle_only = FixLoopBounds::at(
            start,
            MAX_FIX_LOOP_ROUNDS,
            Duration::from_secs(10),
            Duration::from_secs(1000),
        );
        assert_eq!(idle_only.deadline_expired_at(start), None);
        assert_eq!(
            idle_only.deadline_expired_at(start + Duration::from_secs(11)),
            Some(DeadlineKind::Stalled),
            "no observable progress for the idle window is a stalled run"
        );
        assert_eq!(
            idle_only.deadline_expired_at(start + Duration::from_secs(1001)),
            Some(DeadlineKind::HardCap),
            "the absolute bound wins over the idle bound"
        );

        // Progress resets only the idle window: a run that already sat idle past
        // the window is torn down, but a round that produced real progress starts
        // a fresh idle window — and the absolute cap is never reset.
        let mut bounded = FixLoopBounds::at(
            start - Duration::from_secs(12),
            MAX_FIX_LOOP_ROUNDS,
            Duration::from_secs(10),
            Duration::from_secs(1000),
        );
        assert_eq!(
            bounded.deadline_expired_at(start),
            Some(DeadlineKind::Stalled),
            "sitting idle past the window is a stalled run"
        );
        bounded.note_round_progress_at(start);
        assert_eq!(
            bounded.deadline_expired_at(start),
            None,
            "a round that produced real progress must refresh the idle window"
        );
        assert_eq!(
            bounded.deadline_expired_at(start + Duration::from_secs(11)),
            Some(DeadlineKind::Stalled),
            "the refreshed idle window still expires on its own"
        );

        // Progress never postpones the absolute cap: it is measured from the
        // arming instant, not from the last productive round.
        let mut absolute = FixLoopBounds::at(
            start,
            MAX_FIX_LOOP_ROUNDS,
            Duration::from_secs(100),
            Duration::from_secs(60),
        );
        absolute.note_round_progress_at(start + Duration::from_secs(30));
        assert_eq!(
            absolute.deadline_expired_at(start + Duration::from_secs(59)),
            None,
            "still inside the absolute cap"
        );
        assert_eq!(
            absolute.deadline_expired_at(start + Duration::from_secs(61)),
            Some(DeadlineKind::HardCap),
            "a productive round must not buy back absolute wall-clock"
        );
        assert!(bounded.round_cap_reached(bounded.max_rounds() + 1));
        assert!(!bounded.round_cap_reached(bounded.max_rounds()));
    }

    /// Work item 2 — fail-closed contract change. Three reminders and no verdict
    /// used to end the run with `approved = true` ("assumed approved"); a missing
    /// verdict is now NOT an approval.
    #[tokio::test]
    async fn no_verdict_after_reminders_fails_closed_instead_of_approving() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let outcome = crate::harness::with_workspace_root(root, async {
            let chatter: Vec<String> = (0..VERDICT_REMINDERS + 2)
                .map(|i| sse_text(&format!("I checked the deliverable, it looks fine ({i}).")))
                .collect();
            let (server, counter) = mock_turns(chatter).await;
            let outcome = drive(
                &server,
                FixLoopBounds::default(),
                crate::harness::ToolCaller::Specialist(Agent::Validator),
                "validator-coder-chatter",
            )
            .await;
            assert_eq!(
                counter.load(Ordering::SeqCst),
                VERDICT_REMINDERS + 1,
                "the loop gets exactly {VERDICT_REMINDERS} reminders and then concludes"
            );
            outcome
        })
        .await;

        match outcome {
            FixLoopResult::Verdict { approved, critique } => {
                assert!(
                    !approved,
                    "a validator that never called {TOOL_LEAVE_VERDICT} must not approve the deliverable"
                );
                assert!(
                    critique.contains("no validation verdict was recorded"),
                    "the critique must say no verdict was recorded: {critique}"
                );
                assert!(
                    critique.contains(NO_EXPLICIT_APPROVAL_REASON),
                    "the critique must carry the crate's fail-closed reason: {critique}"
                );
            }
            other => panic!("expected a fail-closed verdict, got {other:?}"),
        }
    }

    /// Work item 3 — the `dispatch_verdict_tools` decision.
    ///
    /// The verdict tool is terminal-only: the shared driver consumes it and ends
    /// the run, and it never hands the call to the tool dispatcher — not even for
    /// a caller that is fully allowed to run the *other* tool in the same turn,
    /// and not even for the tool that follows the verdict in the same response.
    /// That is what makes the removed switch meaningless: no setting of it could
    /// have let a worker record its own verdict through this loop, and the t-033b
    /// identity gate (`runner/execution.rs::may_record_verdict`) is the only
    /// authority over who may record one.
    ///
    /// The caller is deliberately the **validator**: since gate t-033e a caller
    /// without verdict authority never reaches the "recorded a verdict" path at all
    /// (see [`verdict_call_from_a_role_without_verdict_authority_fails_closed`]),
    /// so only an authorised caller can isolate the *terminal-only* property.
    #[tokio::test]
    async fn verdict_tool_is_terminal_only_and_never_reaches_the_dispatcher() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let (outcome, calls) = crate::harness::with_workspace_root(root.clone(), async {
            let turn = sse_tool_calls(&[
                (
                    "call_verdict",
                    TOOL_LEAVE_VERDICT,
                    serde_json::json!({"verdict": "APPROVED", "comments": "Inspected and clean."}),
                ),
                (
                    "call_write",
                    TOOL_WRITE_FILE,
                    serde_json::json!({"path": "written-after-verdict.txt", "content": "must not exist"}),
                ),
            ]);
            let (server, counter) = mock_turns(vec![turn]).await;
            // An authorised caller whose dispatcher WOULD honour the write_file
            // call: a missing file is proof the loop never dispatched it.
            let outcome = drive(&server, FixLoopBounds::default(), crate::harness::ToolCaller::Specialist(Agent::Validator), "validator-coder-verdict").await;
            (outcome, counter.load(Ordering::SeqCst))
        })
        .await;

        assert!(
            matches!(outcome, FixLoopResult::Verdict { approved: true, .. }),
            "the verdict call concludes the loop: {calls} LLM round(s) used"
        );
        assert_eq!(
            calls, 1,
            "the run must conclude on the verdict, without another round"
        );
        assert!(
            !root.join("written-after-verdict.txt").exists(),
            "no tool call in the verdict's own turn may be dispatched — the fix loop does not execute tools after a verdict"
        );
    }

    /// Gate t-033e — the same turn driven by a role the public verdict gate
    /// refuses. The loop must not record the verdict, not dispatch anything from
    /// that turn, and end NOT approved carrying the typed rejection
    /// (`ToolError::Forbidden` text).
    #[tokio::test]
    async fn verdict_call_from_a_role_without_verdict_authority_fails_closed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let outcome = crate::harness::with_workspace_root(root.clone(), async {
            let turn = sse_tool_calls(&[
                (
                    "call_self_verdict",
                    TOOL_LEAVE_VERDICT,
                    serde_json::json!({"verdict": "APPROVED", "comments": "SELF-APPROVAL-CANARY"}),
                ),
                (
                    "call_write",
                    TOOL_WRITE_FILE,
                    serde_json::json!({"path": "dispatched-after-refusal.txt", "content": "must not exist"}),
                ),
            ]);
            let (server, counter) = mock_turns(vec![turn]).await;
            let outcome = drive(
                &server,
                FixLoopBounds::default(),
                crate::harness::ToolCaller::Specialist(Agent::Coder),
                "coder-self-verdict",
            )
            .await;
            assert_eq!(
                counter.load(Ordering::SeqCst),
                1,
                "the refusal must end the run in the same round, without another LLM turn"
            );
            outcome
        })
        .await;

        match outcome {
            FixLoopResult::Verdict { approved, critique } => {
                assert!(
                    !approved,
                    "a non-validator verdict call must never approve: {critique}"
                );
                assert!(
                    critique.contains("VERDICT REJECTED"),
                    "the rejection must reuse the crate's verdict-rejection vocabulary: {critique}"
                );
                assert!(
                    critique.contains("is forbidden for caller"),
                    "the rejection must be the typed Forbidden rejection: {critique}"
                );
                assert!(
                    !critique.contains("SELF-APPROVAL-CANARY"),
                    "a refused verdict payload must never surface as a verdict: {critique}"
                );
            }
            other => panic!("expected a fail-closed verdict outcome, got {other:?}"),
        }
        assert!(
            !root.join("dispatched-after-refusal.txt").exists(),
            "a refused verdict turn must not dispatch any tool"
        );
    }

    /// Work item 3 (structural half) — the allow/deny switch is gone, and MCP
    /// tool advertising goes through the policy-filtered helper (work item 5).
    ///
    /// Source-shape guard, in the style of `tests/test_tool_name_literals.rs`: the
    /// needles are assembled at runtime so this test's own text cannot satisfy
    /// them.
    #[test]
    fn switch_is_gone_and_mcp_advertising_goes_through_the_policy_helper() {
        let src = include_str!("fix_loop.rs");

        // (a) The undecided allow/deny switch no longer exists anywhere in the
        //     driver: neither the field nor a read of it.
        let switch_name = ["dispatch", "_verdict", "_tools"].concat();
        let field_decl = ["pub ", switch_name.as_str(), ": bool"].concat();
        assert!(
            !src.contains(&field_decl),
            "the {switch_name} field must be removed: it decided nothing and could only ever widen the verdict identity gate"
        );
        let field_read = ["p.", switch_name.as_str()].concat();
        assert!(
            !src.contains(&field_read),
            "no code may read {switch_name} again"
        );

        // (b) MCP tool advertising uses the policy-filtered helper, never the raw
        //     registry view.
        let policy_helper = ["allowed", "_mcp_tools", "("].concat();
        assert!(
            src.contains(&policy_helper),
            "assemble_tools must advertise MCP tools through harness::allowed_mcp_tools"
        );
        let raw_registry_view = ["mcp", ".tools_for_servers", "("].concat();
        assert!(
            !src.contains(&raw_registry_view),
            "assemble_tools must not enumerate the raw MCP registry: a policy-refused name could be advertised to the model"
        );
    }

    // ── gate t-064: single-owner verdict spellings, claimed inspection tools,
    //    and a live tool-schema charge for the run ────────────────────────────

    use super::BlueprintToolClass;
    use super::{
        INSPECTION_TOOLS, VERDICT_RECORDING_TOOLS, advertised_tools_for_caller, assemble_tools,
        blueprint_tool_class, charge_engine_tool_schema, fix_loop_compaction_notice,
        is_verdict_recording_tool, verdict_recording_tool_spellings,
    };
    use crate::manager::context::CompactionOutcome;

    /// The verdict spelling family is owned by `crate::tool_names`: the local
    /// mirror may only contain spellings that owner enumerates, and the predicate
    /// the whole blueprint asks (`is_verdict_recording_tool`) must answer from the
    /// owner. Drift here would silently re-open the verdict class to a role the
    /// role gate refuses — a name the blueprint does not classify `Verdict` is
    /// grantable by a plain prompt allow-list.
    #[test]
    fn verdict_recording_spellings_have_a_single_owner_in_tool_names() {
        let owner = verdict_recording_tool_spellings();
        assert!(
            owner.contains(&crate::tool_names::TOOL_LEAVE_VERDICT)
                && owner.contains(&crate::tool_names::TERMINAL_LEAVE_VERDICT),
            "the owner must enumerate both canonical verdict spellings: {owner:?}"
        );
        for spelling in VERDICT_RECORDING_TOOLS {
            assert!(
                owner.iter().any(|owned| owned == spelling),
                "the local mirror carries {spelling}, which the alias-table owner does not enumerate: {owner:?}"
            );
            assert!(
                is_verdict_recording_tool(spelling),
                "{spelling} must be recognised by the predicate the blueprint asks"
            );
        }
        // The predicate is the owner's predicate — no second verdict vocabulary.
        // The spellings under test are **built** from the owner's vocabulary (the
        // alias rows reachable through `tool_spellings_for`, the namespaced suffix
        // rule, and the canonical constant) rather than re-typed, which is also
        // what the fix-loop source guard in `tests/test_role_gating.rs` demands.
        let mut names: Vec<String> = verdict_recording_tool_spellings()
            .into_iter()
            .map(str::to_string)
            .collect();
        names.extend(
            crate::tool_names::LEAVE_VERDICT_NAME_SUFFIXES
                .iter()
                .map(|suffix| format!("{}{suffix}", Agent::Validator.as_str())),
        );
        names.push(crate::tool_names::TOOL_LEAVE_VERDICT.to_ascii_uppercase());
        for name in &names {
            assert!(
                is_verdict_recording_tool(name)
                    == crate::agents::validation::is_leave_verdict_tool(name),
                "{name}: the blueprint predicate and the crate owner must agree"
            );
            assert!(
                is_verdict_recording_tool(name),
                "{name} names the verdict tool and must never classify as an ordinary tool"
            );
        }
        assert!(
            !is_verdict_recording_tool(crate::tool_names::TOOL_READ_FILE),
            "an ordinary tool must stay ordinary"
        );
    }

    /// Every advertised inspection tool must be a spelling a built-in handler
    /// actually serves. `list_directory` / `terminal__list_directory` were removed
    /// from [`INSPECTION_TOOLS`] for exactly this reason: t-069 deleted the alias
    /// rows that claimed them and no dispatch arm implements them, so advertising
    /// them promised the model a tool that answers `UnknownTool`.
    #[test]
    fn inspection_tools_are_all_claimed_by_a_builtin_handler() {
        for name in INSPECTION_TOOLS {
            assert!(
                crate::tool_names::is_canonical_tool_name(name),
                "INSPECTION_TOOLS advertises {name}, which is not a canonical spelling a built-in handler answers to"
            );
        }
        for name in [
            crate::tool_names::TOOL_LIST_DIRECTORY,
            crate::tool_names::TERMINAL_LIST_DIRECTORY,
        ] {
            assert!(
                !INSPECTION_TOOLS.contains(&name),
                "{name} has no built-in dispatch arm and must not be advertised by the blueprint"
            );
            assert_eq!(
                blueprint_tool_class(name),
                BlueprintToolClass::Other,
                "{name} must not be classified as an inspection tool"
            );
        }
    }

    /// Drive [`run_fix_loop`] with an explicit tool list against an unreachable
    /// backend: with a round cap of `0` the driver exits at the top of its first
    /// round, so no HTTP happens while the run's setup (verdict gate, advertised
    /// view, tool-schema charge) is still exercised end to end.
    async fn drive_bounded_run_with_tools(
        engine: &mut ContextEngine,
        tools: Vec<crate::types::ToolDef>,
        caller: crate::harness::ToolCaller,
    ) -> FixLoopResult {
        let cfg = Config::default();
        let mon_cfg = MonitoringConfig::default();
        let client = crate::llm::ChatClient::new_with_token(
            "http://127.0.0.1:1/v1",
            "test-model",
            "test-token",
        );
        let token = CancellationToken::new();
        let guard = register_loop_worker(
            Some("t-schema".to_string()),
            "validator-coder".to_string(),
            "tool-schema budget run".to_string(),
            &token,
        );
        let mut params = LoopParams {
            client: &client,
            model: "test-model".to_string(),
            tag: "validator-coder-t-schema".to_string(),
            worker_name: "validator-coder".to_string(),
            task_id: Some("t-schema".to_string()),
            worker_key: guard.0.clone(),
            engine,
            tools,
            token: &token,
            mon_cfg: &mon_cfg,
            cfg: &cfg,
            temperature: 0.0,
            status_template: "validator-coder-t-schema: evaluating deliverable...".to_string(),
            verdict_log_role: "coder".to_string(),
            bounds: FixLoopBounds::new(0),
            abort_log_prefix: "validator-coder-t-schema".to_string(),
            emit_tool_status: false,
            rebirth_notice: format!("({TOOL_REBIRTH} checkpoint accepted)"),
        };
        let mut monitor = HarnessMonitor::new_with_config(
            std::sync::Arc::new(crate::harness::HarnessStats::new()),
            &mon_cfg,
        );
        run_fix_loop(&mut params, &mut monitor, caller)
            .await
            .expect("a bounded run is an outcome, never a hard error")
    }

    /// Gate t-064, live proof on the shared fix-loop driver: a run charges its
    /// engine with exactly the schemas it advertises, so `should_compact()` prices
    /// the real request. The same transcript priced message-only (the pre-t-064
    /// behaviour) stays under the 90% trigger — that is the under-count the gate
    /// flagged.
    #[tokio::test]
    async fn fix_loop_run_charges_its_advertised_tool_schema() {
        let tools = assemble_tools(None, |_| true, &[]);
        assert!(
            !tools.is_empty(),
            "the default blueprint must carry schemas"
        );
        let caller = crate::harness::ToolCaller::Specialist(Agent::Validator);
        let advertised = advertised_tools_for_caller(&tools, &caller);
        let schema = crate::manager::context::tools_tokens(&advertised);
        assert!(schema > 0, "a realistic blueprint must cost schema tokens");

        // Budget = transcript + schema: the 90% trigger then lands strictly
        // between the message-only baseline and the real request size.
        let probe = ContextEngineFactory::new(2_000)
            .specialist_context("role prompt".to_string(), "brief".to_string());
        let messages = probe.token_count();
        let mut engine = ContextEngineFactory::new(messages + schema)
            .specialist_context("role prompt".to_string(), "brief".to_string());
        assert_eq!(engine.token_count(), messages);
        assert_eq!(
            engine.tool_schema_tokens(),
            0,
            "a specialist engine starts uncharged — the caller must declare its list"
        );

        let outcome = drive_bounded_run_with_tools(&mut engine, tools, caller).await;
        assert!(
            matches!(outcome, FixLoopResult::Exhausted { .. }),
            "a round cap of 0 must end the run before any turn: {outcome:?}"
        );

        assert_eq!(
            engine.tool_schema_tokens(),
            schema,
            "the run must charge exactly its advertised view — no superset, no subset"
        );
        assert_eq!(
            engine.request_token_count(),
            engine.token_count() + schema,
            "request_token_count must be transcript + advertised schemas"
        );
        assert!(
            engine.request_token_count() > engine.token_count(),
            "the live request size must be strictly above the message-only baseline"
        );
        assert!(
            engine.should_compact(),
            "the charged engine must trigger compaction where the message-only baseline does not"
        );

        let mut message_only = engine.clone();
        message_only.set_tools(&[]);
        assert_eq!(message_only.tool_schema_tokens(), 0);
        assert!(
            !message_only.should_compact(),
            "the identical transcript priced without schemas stays under the trigger — this is the inert-budget defect"
        );
    }

    /// The charge helper is exact and idempotent: re-charging the same advertised
    /// view cannot drift the budget, and a narrower view lowers it by exactly the
    /// schemas it dropped.
    #[test]
    fn charge_engine_tool_schema_is_exact_and_idempotent() {
        let mut engine = ContextEngineFactory::new(64_000)
            .specialist_context("role prompt".to_string(), "brief".to_string());
        let tools = assemble_tools(None, |_| true, &[]);
        let first = charge_engine_tool_schema(&mut engine, &tools);
        let again = charge_engine_tool_schema(&mut engine, &tools);
        assert_eq!(first, again, "re-charging the same list must not drift");
        assert_eq!(first, crate::manager::context::tools_tokens(&tools));
        let narrower = vec![tools[0].clone()];
        let narrower_charge = charge_engine_tool_schema(&mut engine, &narrower);
        assert!(
            narrower_charge < first,
            "charging a narrower list must lower the charge: {narrower_charge} vs {first}"
        );
        assert_eq!(
            charge_engine_tool_schema(&mut engine, &[]),
            0,
            "an empty advertised view falls back to message-only accounting"
        );
    }

    /// The fix loop must not discard [`CompactionOutcome`]: a compaction that
    /// never reached its 70% target is a reportable failure, not a success.
    #[test]
    fn compaction_outcome_is_surfaced_not_discarded() {
        let compacted = CompactionOutcome::Compacted {
            initial_tokens: 900,
            final_tokens: 600,
            tokens_reclaimed: 300,
            messages_removed: 2,
            target: 700,
        };
        assert!(
            fix_loop_compaction_notice("validator-coder-t-1", &compacted).is_none(),
            "a compaction that reached its target needs no warning"
        );

        let unreachable = CompactionOutcome::TargetUnreachable {
            pinned_tokens: 900,
            pinned_messages: 2,
            target: 700,
            initial_tokens: 1_200,
            final_tokens: 850,
            tokens_reclaimed: 350,
            messages_removed: 4,
        };
        let notice = fix_loop_compaction_notice("validator-coder-t-1", &unreachable)
            .expect("an unreachable target must be surfaced");
        assert!(notice.contains("validator-coder-t-1"), "{notice}");
        assert!(notice.contains("could not reach its target"), "{notice}");
        for number in ["350", "850", "700"] {
            assert!(
                notice.contains(number),
                "{notice} must carry the counter {number}"
            );
        }
    }
}
