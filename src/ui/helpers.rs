//! Formatting, command inspection, subagent lifecycle, and error classification helpers.

use super::{Event, QueueState, Renderer, SubagentDetail};
use crate::config::Config;
use crate::manager::context::ContextEngine;
use crate::orchestrator::{DelegationEvent, OrchestratorManager};
use crate::types::Message;
use anyhow::Result;
use std::sync::Arc;

pub fn format_active_subtasks(subagents: &[SubagentDetail]) -> String {
    let global_active = crate::orchestrator::get_active_subtasks_str();
    if global_active != "None" && !global_active.trim().is_empty() {
        return global_active;
    }
    let active: Vec<_> = subagents.iter().filter(|s| s.is_active).collect();
    if active.is_empty() {
        return "None".to_string();
    }
    let mut out = String::new();
    for s in active {
        let elapsed_secs = s.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let task_id_str = s.task_id.as_deref().unwrap_or(&s.name);
        let prompt_str = if s.prompt.is_empty() {
            "None"
        } else {
            &s.prompt
        };
        out.push_str(&format!(
            "- Tool Call ID: {}\n  Subagent: {}\n  Task Prompt: {}\n  Running For: {} seconds\n\n",
            task_id_str, s.name, prompt_str, elapsed_secs
        ));
    }
    out
}

pub fn format_plan_progress_summary(plan_content: &str) -> String {
    crate::orchestrator::generate_plan_progress_summary(plan_content)
}
/// Build the execution-plan section of the system prompt — **fail-closed**. The
/// verdict is delegated to the single plan-read decision in
/// [`super::session::read_plan_gate`] and the body to the plan layer's own reader
/// [`crate::manager::phase::Plan::read`], so the prompt and the session's
/// nudge/completion notices can never disagree about what the plan file said.
///
/// * [`super::session::PlanGate::Read`] — the plan was read and parsed, so its
///   markdown is embedded verbatim.
/// * [`super::session::PlanGate::Unknown`] — the plan exists but could not be read
///   or parsed: an explicit "plan state UNKNOWN" block replaces the markdown and the
///   warning is returned for the caller, because an absent block reads to the model
///   and the UI as *no work pending / all complete*.
///
/// Returns `(prompt_section, warning)`; an empty section with no warning means there
/// is genuinely no plan file on disk.
fn plan_prompt_section(plan: &crate::manager::phase::Plan) -> (String, Option<String>) {
    match super::session::read_plan_gate(plan) {
        super::session::PlanGate::Unknown { warning } => (String::new(), Some(warning)),
        super::session::PlanGate::Read { .. } => {
            match plan.read() {
                Ok(Some(content)) if !content.trim().is_empty() => (
                    format!(
                        "\n## Active Execution Plan (`.marmel/execution_plan.md`)\nThere is an existing execution plan already active on disk:\n```markdown\n{}\n```\nDo NOT call `create_plan` unless you explicitly intend to overwrite the plan. Proceed directly with `delegate_task` to execute any remaining unchecked `- [ ] [t-xxx]` tasks, honouring each line's optional `(validate)` / `(no-validate)` directive where present.\n",
                        content.trim()
                    ),
                    None,
                ),
                // No plan file, or an empty one: nothing pending, nothing to warn about.
                Ok(_) => (String::new(), None),
                // TOCTOU only — the plan file was just read successfully above. Even
                // so it must fail closed: no plan block, and a loud warning.
                Err(e) => (
                    String::new(),
                    Some(format!(
                        "execution plan at {} could not be read: {e:#} — plan state is UNKNOWN, \
                         NOT 'all tasks done'",
                        plan.plan_path().display()
                    )),
                ),
            }
        }
    }
}

/// Rendered in place of the plan block when the plan state is unknown, so an
/// unreadable plan is never presented to the model as a finished plan.
fn plan_unknown_section(plan: &crate::manager::phase::Plan, warning: &str) -> String {
    format!(
        "\n## Execution Plan State: UNKNOWN (fail-closed)\n\
         The on-disk execution plan at `{}` could not be read or parsed:\n\
         {warning}\n\
         The plan state is UNKNOWN: do NOT treat the plan as complete, do NOT assume no work is \
         pending, and do NOT re-run finished work. Re-read that plan file directly (or ask the \
         user) before deciding what to do next.\n",
        plan.plan_path().display(),
    )
}

/// Load the system prompt and report any plan-integrity problem found on the way.
///
/// Returns `(prompt, plan_warning)`. `plan_warning` is the text the caller must
/// surface through the session warning channel (`super::session::surface_plan_warning`:
/// a renderer `Status` event plus a `Status` record in the UI transcript). The
/// helper holds no renderer, so it also (a) logs the warning through
/// `tracing::warn!` — the first half of that same channel — and (b) embeds it in
/// the prompt via [`plan_unknown_section`], so a plan that cannot be read can
/// never reach the model as "no work pending / all complete" even if a caller
/// ignores the returned warning.
pub(crate) fn load_system_prompt_with_plan_and_warning(
    cfg: &Config,
    plan: &crate::manager::phase::Plan,
) -> Result<(String, Option<String>)> {
    let content = if cfg.system_prompt_path.exists() {
        std::fs::read_to_string(&cfg.system_prompt_path)
            .unwrap_or_else(|_| include_str!("../../prompts/system.md").to_string())
    } else {
        include_str!("../../prompts/system.md").to_string()
    };
    let env_block = crate::prompts::format_environment_block();
    let mut prompt = format!("{content}\n\n{env_block}\n");
    if let Some(ref base_prompt) = cfg.base_prompt {
        let trimmed = base_prompt.trim();
        if !trimmed.is_empty() {
            if trimmed.starts_with('#') {
                prompt.push_str(&format!("\n{trimmed}\n"));
            } else {
                prompt.push_str(&format!("\n## Machine Specific Guidance\n{trimmed}\n"));
            }
        }
    }
    let (plan_section, warning) = plan_prompt_section(plan);
    if let Some(warning) = warning.as_ref() {
        tracing::warn!("{warning}");
        prompt.push_str(&plan_unknown_section(plan, warning));
    } else {
        prompt.push_str(&plan_section);
    }
    Ok((prompt, warning))
}

/// The argument knowledge and clipping live in [`crate::tool_args`] (single
/// owner, shared with the specialist runner preview); this is a thin wrapper so
/// the UI keeps a stable local name.
pub(crate) fn format_tool_call_display(name: &str, args_val: &serde_json::Value) -> String {
    crate::tool_args::preview_tool_call(name, args_val)
}
pub fn chunk_utf8(s: &str, max: usize) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let bytes = s.as_bytes();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut end = (start + max).min(bytes.len());
        while end > start && !s.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = start + s[start..].chars().next().map_or(1, |c| c.len_utf8());
        }
        chunks.push(&s[start..end]);
        start = end;
    }
    chunks
}

/// True when a submitted input buffer is **single-line**, i.e. it still carries no
/// line break after surrounding whitespace is dropped.
///
/// Command interpretation is deliberately restricted to single-line submissions: a
/// submitted block that spans more than one line (what a pasted snippet or a
/// multi-paragraph note produces, once the composer re-joins its editor lines with
/// `\n`) is always delivered verbatim as **one** user message / **one** steering
/// instruction, never parsed as a command — even when its first line happens to be
/// `/reset`, `/abort` or the bare word `stop`. A trailing newline coming from a
/// pasted command line is only padding, so `"/reset\n"` keeps working: the test
/// looks at what is left after trimming, which is exactly the "content on more than
/// one line" case.
fn submission_is_single_line(line: &str) -> bool {
    let t = line.trim();
    !t.contains('\n') && !t.contains('\r')
}

/// Whole-string command test — see [`submission_is_single_line`] for why a
/// multi-line submission is never a command.
pub(crate) fn is_abort_command(line: &str) -> bool {
    if !submission_is_single_line(line) {
        return false;
    }
    let t = line.trim();
    t.eq_ignore_ascii_case("/abort")
        || t.eq_ignore_ascii_case("abort")
        || t.eq_ignore_ascii_case("/exit")
        || t.eq_ignore_ascii_case("exit")
        || t.eq_ignore_ascii_case("/quit")
        || t.eq_ignore_ascii_case("quit")
        || t.eq_ignore_ascii_case("/q")
        || t.eq_ignore_ascii_case(":q")
        || t.eq_ignore_ascii_case(":q!")
        || t.eq_ignore_ascii_case("/stop")
        || t.eq_ignore_ascii_case("stop")
        || t.eq_ignore_ascii_case("/cancel")
        || t.eq_ignore_ascii_case("cancel")
}

/// Whole-string command test — see [`submission_is_single_line`] for why a
/// multi-line submission is never a command.
pub(crate) fn is_reset_command(line: &str) -> bool {
    if !submission_is_single_line(line) {
        return false;
    }
    let t = line.trim();
    t.eq_ignore_ascii_case("/reset")
        || t.eq_ignore_ascii_case("/reset_plan")
        || t.eq_ignore_ascii_case("/reset-plan")
        || t.eq_ignore_ascii_case("/clear_plan")
        || t.eq_ignore_ascii_case("/clear-plan")
        || t.eq_ignore_ascii_case("/reset_execution_plan")
}

pub(crate) fn handle_reset_command(
    plan: &crate::manager::phase::Plan,
    renderer: &mut dyn Renderer,
    mut ctx: Option<&mut ContextEngine>,
) {
    // `Plan::clear()` already clears the session state along with the plan: the
    // `.session_transcript.json` journal, the session bundle and the in-memory
    // manager capture. No separate transcript unlink is needed here.
    let _ = plan.clear();
    renderer.on_event(&Event::Message(
        "Execution plan has been cleared and reset by user.".to_string(),
    ));
    renderer.on_event(&Event::Status("Execution plan reset".to_string()));
    let _ = renderer.flush();
    if let Some(ref mut ctx) = ctx {
        ctx.append(Message::User {
            content: "[System] User executed /reset. The execution plan has been removed from disk. Return to Conversational phase."
                .to_string(),
        });
    }
}

pub(crate) fn classify_llm_error(e: &anyhow::Error) -> &'static str {
    let msg = format!("{e:#}").to_lowercase();
    if msg.contains("http") || msg.contains("status") || msg.contains("503") || msg.contains("429")
    {
        "http"
    } else if msg.contains("transport") || msg.contains("connection") {
        "connectivity"
    } else if msg.contains("timeout") {
        "timeout"
    } else if msg.contains("stream") || msg.contains("sse") {
        "stream"
    } else {
        "unknown"
    }
}

/// The **display** spelling of a delegation: `{agent}-{normalized task}`, or the
/// bare role id when the delegation carries no task id.
///
/// It is a formatter, **not** an identity: the row identity is the **effective
/// registry key** ([`crate::orchestrator::WorkerEntry::key`]), which this
/// function can never produce on its own (`{agent}-wN` for a taskless worker and
/// `{base}#N` for a live-key collision are allocated by the registry). It survives
/// only as
/// 1. the fallback row name when a lifecycle event arrives for a worker the
///    registry does not (yet) hold, and
/// 2. the natural-key half of [`registry_entry_for_event`]'s resolution.
fn subagent_row_name(agent: crate::agents::Agent, task: Option<&str>) -> (String, Option<String>) {
    let clean_task = task
        .map(|t| crate::task_id::normalize_task_id_ref(t).to_string())
        .filter(|t| !t.is_empty());
    let name = match &clean_task {
        Some(t) => format!("{}-{t}", agent.as_str()),
        None => agent.as_str().to_string(),
    };
    (name, clean_task)
}

/// Whether a registry entry is a **registration** the pane may show, as opposed to
/// a token-count mirror.
///
/// [`crate::orchestrator::WorkerCallClass::Placeholder`] is the registry's own
/// marker and is the class filter the design mandates. The structural half is kept
/// as well because the marker is an id value: `update_active_worker_context` writes
/// a stub whose `agent_name` **is** the key, and no real registration can be shaped
/// like that (a key always carries a task id, a `-wN` id or a `#…` handle). A bundle
/// written before the id-`0` reservation journaled a real registration with that
/// id, and the pane must not lose a live worker over a marker collision.
fn entry_is_registering(entry: &crate::orchestrator::WorkerEntry) -> bool {
    if !matches!(
        entry.call_class,
        crate::orchestrator::WorkerCallClass::Placeholder
    ) {
        return true;
    }
    entry.agent_name != entry.key
}

/// Resolve the **registry entry** a delegation event addresses, through the
/// registry's own read model rather than through a rendered row name.
///
/// The lookup is whole-field (`agent_name` equal, `task_id` equal after
/// decoration stripping) and prefers the entry holding the **natural** key, so
/// a `Started` for `coder-t-01` lands on `coder-t-01` and never on the
/// `{base}#N` duplicate the runner registers under. `Placeholder` stubs are
/// never an addressable worker (they are token-count mirrors, not
/// registrations), and neither is any internal/reserve call class — the registry
/// simply has no such entry.
///
/// `None` means "the registry has no such worker" — the honest answer for an
/// event that arrives before its registration, and the reason the callers keep a
/// derived-name fallback instead of dropping the event.
pub(crate) fn registry_entry_for_event(
    agent: crate::agents::Agent,
    task: Option<&str>,
) -> Option<crate::orchestrator::WorkerEntry> {
    let agent_name = agent.as_str();
    let clean_task = task
        .map(|t| crate::task_id::normalize_task_id_ref(t).to_string())
        .filter(|t| !t.is_empty());
    let mut collided: Option<crate::orchestrator::WorkerEntry> = None;
    for entry in crate::orchestrator::worker_entries() {
        if !entry_is_registering(&entry) {
            continue;
        }
        if !entry.agent_name.eq_ignore_ascii_case(agent_name) {
            continue;
        }
        let task_matches = match (&entry.task_id, &clean_task) {
            (Some(held), Some(wanted)) => {
                crate::task_id::normalize_task_id_ref(held) == wanted.as_str()
            }
            (None, None) => true,
            _ => false,
        };
        if !task_matches {
            continue;
        }
        if !entry.disambiguated {
            return Some(entry);
        }
        if collided.is_none() {
            collided = Some(entry);
        }
    }
    collided
}

/// The row set the pane must show, rebuilt from the worker registry.
///
/// This is the projection that replaces the event-derived row list:
/// **which rows exist** and **whether they are running** come from `WORKERS`,
/// and nothing else. What survives from the previous vector is presentation only — streamed `thinking`/`content`,
/// logs, clocks not yet derived from the registry, and the queue field, which is
/// pool state and stays orthogonal to running.
///
/// Membership rules:
/// * rows are created for the registering classes only — `Specialist`
///   (Manager-dispatched, nested `delegate_task`, and restored alike) and
///   `ValidatorAudit`; a `Placeholder` stub never gets a row;
/// * the row **name is the effective registry key**, so a `{agent}-wN` or a
///   `{base}#N` worker is addressable and its token lookup can never miss;
/// * a row is closed by the registry flipping `is_active` (completion, guard
///   drop after a cancel, or eviction from the completed ring) — never by a
///   writer that decided on its own;
/// * row order is the registry's key order, which is byte-stable across a
///   restore.
///
/// Returns whether anything a viewer can see actually moved, so callers push to
/// the renderer on real edges only.
pub(crate) fn project_subagent_rows(subagents: &mut Vec<SubagentDetail>) -> bool {
    let before = subagent_row_signature(subagents);
    let previous = std::mem::take(subagents);
    let mut projected: Vec<SubagentDetail> = Vec::new();
    for entry in crate::orchestrator::worker_entries() {
        // Rows are created for registering classes only: `Specialist` (Manager,
        // runner, nested `delegate_task`, restored) and `ValidatorAudit`. An
        // internal/reserve class has no entry here at all, and a token mirror never
        // becomes a row.
        if !entry_is_registering(&entry) {
            continue;
        }
        let mut row = match previous.iter().position(|row| row.name == entry.key) {
            Some(idx) => previous[idx].clone(),
            None => SubagentDetail {
                name: entry.key.clone(),
                last_activity_at: Some(std::time::Instant::now()),
                ..Default::default()
            },
        };
        apply_registry_entry(&mut row, &entry);
        projected.push(row);
    }
    // Rows the registry no longer holds are gone; that is how a row closes.
    *subagents = projected;
    before != subagent_row_signature(subagents)
}

/// Fold one authoritative registry entry onto a row, leaving every
/// renderer-owned field (thinking, content, logs, selection, queue) alone.
fn apply_registry_entry(row: &mut SubagentDetail, entry: &crate::orchestrator::WorkerEntry) {
    row.name = entry.key.clone();
    row.task_id = entry.task_id.clone();
    if !entry.prompt.is_empty() {
        row.prompt = entry.prompt.clone();
    }
    row.is_active = entry.is_active;
    let tokens = if entry.is_active {
        entry.context_tokens
    } else {
        entry.last_tokens.max(entry.context_tokens)
    };
    if tokens > 0 {
        row.context_tokens = tokens;
    }
    // The clock is the registry's clock, which is the only one that can still be
    // right after the row was created by stream traffic (or restored with the
    // pane's own re-anchored instant).
    if entry.is_active {
        row.started_at = Some(entry.started_at);
    } else {
        row.started_at = None;
        row.worked_duration = std::time::Duration::from_secs(entry.elapsed_secs);
    }
    // A row that was never running still needs a start stamp for its own clock,
    // and only a live entry can supply one.
    if row.last_activity_at.is_none() {
        row.last_activity_at = Some(std::time::Instant::now());
    }
    // A row that stopped running can never keep a queue position behind: the
    // queue belongs to a live delegation only, and this is the path every
    // non-event teardown (cancel, preemption, guard drop) goes through. The same
    // holds for a paused stream — the pause belongs to a live stream, so a row the
    // registry closed (completion, guard drop after a cancel, eviction) can never
    // keep a stale `Paused` behind.
    if !entry.is_active {
        row.queue = None;
        row.paused = false;
    }
}

/// The class/handle badge a row label carries, **derived at render time** from
/// the row's registry key — never stored on the row, because the row is a
/// projection and this is a fact about the registry entry behind it. `entry` is
/// that entry when the caller already holds the snapshot it rendered from.
///
/// * `Audit` — the entry is a validator/plan-audit run
///   ([`crate::orchestrator::WorkerCallClass::ValidatorAudit`]), which registers
///   in the validator role family (`validator-coder`, `validator-planner`) and
///   therefore never appears in a `delegate_task` event stream.
/// * `Dup #N` — the entry had to take a `{base}#N` collision handle because a
///   live worker already owned the natural key (the Manager registers
///   `{agent}-{task}` at dispatch and the runner registers it again at execution).
///
/// When the key is not (or no longer) in the registry, the same two facts are read
/// off the key's own structure, so a transient row is labelled consistently.
pub(crate) fn subagent_row_badge(
    entry: Option<&crate::orchestrator::WorkerEntry>,
    name: &str,
) -> Option<String> {
    if let Some(entry) = entry {
        if matches!(
            entry.call_class,
            crate::orchestrator::WorkerCallClass::ValidatorAudit
        ) {
            return Some("Audit".to_string());
        }
        return entry
            .collision_handle
            .as_ref()
            .map(|handle| format!("Dup #{handle}"));
    }
    match name.split_once('#') {
        Some((_, handle)) if !handle.is_empty() => return Some(format!("Dup #{handle}")),
        _ => {}
    }
    let head = name.split('-').next().unwrap_or("");
    if head.eq_ignore_ascii_case("validator") {
        return Some("Audit".to_string());
    }
    None
}

/// What a viewer can see about the row **set**: identity, liveness, arbitration
/// pause and ctx. Deliberately blind to clocks and streamed text, which move on
/// their own. `paused` is part of it because the panel's label word changes with
/// it: a row that flips to or out of a arbitration pause must be re-pushed even
/// when nothing else about it moved.
fn subagent_row_signature(rows: &[SubagentDetail]) -> Vec<(String, bool, bool, usize)> {
    rows.iter()
        .map(|row| {
            (
                row.name.clone(),
                row.is_active,
                row.paused,
                row.context_tokens,
            )
        })
        .collect()
}

/// One authoritative row for a **restored** registry key — the restore-side half
/// of the same projection, so a restored pane and a restored registry name the
/// same object.
///
/// `None` when the key is not in the registry (the bundle recorded a panel with
/// no worker behind it, which is a real and reported case).
pub(crate) fn subagent_row_for_registry_key(key: &str) -> Option<SubagentDetail> {
    let entry = crate::orchestrator::worker_entries()
        .into_iter()
        .find(|entry| entry.key == key)?;
    if !entry_is_registering(&entry) {
        return None;
    }
    let mut row = SubagentDetail {
        name: key.to_string(),
        ..Default::default()
    };
    apply_registry_entry(&mut row, &entry);
    Some(row)
}

pub(crate) fn update_subagent_lifecycle(
    subagents: &mut Vec<SubagentDetail>,
    agent: crate::agents::Agent,
    task: Option<String>,
    prompt: Option<String>,
    started: bool,
) {
    // Identity first comes from the registry, so two live workers never collapse
    // into one row and a `{base}#N` / `{agent}-wN` worker is addressed by the
    // key the Manager and the arbitrator use.
    let entry = registry_entry_for_event(agent, task.as_deref());
    let (name, clean_task) = match entry.as_ref() {
        Some(entry) => (
            entry.key.clone(),
            entry
                .task_id
                .as_deref()
                .map(|t| crate::task_id::normalize_task_id_ref(t).to_string())
                .filter(|t| !t.is_empty()),
        ),
        None => subagent_row_name(agent, task.as_deref()),
    };
    let task_str = clean_task.clone().unwrap_or_default();
    let log_entry = if started {
        format!("started task {task_str}")
    } else {
        format!("completed task {task_str}")
    };
    // Tokens come from the same snapshot as the row identity — never from a
    // per-field read keyed on a display name (`get_active_worker_tokens`).
    let active_tokens = entry
        .as_ref()
        .map(|entry| {
            if entry.is_active {
                entry.context_tokens
            } else {
                entry.last_tokens.max(entry.context_tokens)
            }
        })
        .unwrap_or(0);
    let now = std::time::Instant::now();
    if let Some(existing) = subagents.iter_mut().find(|s| s.name == name) {
        if let Some(entry) = entry.as_ref() {
            apply_registry_entry(existing, entry);
        }
        if existing.is_active && !started {
            if let Some(st) = existing.started_at.take() {
                existing.worked_duration += st.elapsed();
            }
        } else if !existing.is_active && started {
            existing.started_at = Some(now);
        }
        existing.is_active = started;
        // Closing a delegation takes the queue state down with it, whichever way
        // it closed — completion, failure or a cancel that landed while the call
        // was still waiting for a slot. No queue row ever outlives its
        // delegation (design §7). The arbitration pause goes down the same way: a
        // stream of a closed delegation cannot be held by the arbitrator any more.
        if !started {
            existing.queue = None;
            existing.paused = false;
        }
        existing.last_activity_at = Some(now);
        if active_tokens > 0 {
            existing.context_tokens = active_tokens;
        }
        if started {
            existing.task_id = task;
            if let Some(p) = prompt {
                existing.prompt = p;
            }
        }
        existing.logs.push(log_entry);
    } else {
        let mut row = SubagentDetail {
            name,
            task_id: task,
            prompt: prompt.unwrap_or_default(),
            started_at: if started { Some(now) } else { None },
            worked_duration: std::time::Duration::ZERO,
            last_activity_at: Some(now),
            logs: vec![log_entry],
            thinking: String::new(),
            content: String::new(),
            is_active: started,
            context_tokens: active_tokens,
            queue: None,
            paused: false,
        };
        if let Some(entry) = entry.as_ref() {
            apply_registry_entry(&mut row, entry);
        }
        subagents.push(row);
    }
}

/// Apply a concurrency-slot transition to an existing panel row.
///
/// This is the queue-state half of the same single authoritative writer:
/// [`DelegationEvent::Queued`] passes `Some(..)`, [`DelegationEvent::Running`]
/// passes `None`. The transition is deliberately orthogonal to
/// [`update_subagent_lifecycle`]:
///
/// * it never touches `is_active` — a queued delegation is still dispatched and
///   still belongs to the round, so its clocks, panel visibility and selection
///   stay exactly as the lifecycle path left them;
/// * it never touches `started_at`/`worked_duration`, so a wait in the queue is
///   never mistaken for worked time;
/// * it never **creates** a row — structurally so, since it takes a slice, not a
///   `Vec`. Rows belong to the lifecycle path, whose `Started` event precedes
///   any `Queued` transition inside the one event vector. Creating rows here
///   would let a delegation whose lifecycle this stream does not own (a nested
///   `delegate_task`, whose own Manager holds those events) leave a permanently
///   active, permanently queued panel row behind.
///
/// Returns whether a row was updated, so callers only re-push the panel when
/// anything actually changed.
pub(crate) fn update_subagent_queue(
    subagents: &mut [SubagentDetail],
    agent: crate::agents::Agent,
    task: Option<String>,
    queue: Option<QueueState>,
) -> bool {
    // Same identity rule as the lifecycle path: the effective registry key wins,
    // and the derived spelling is only a fallback. A queue transition still
    // cannot create a row — queue state is orthogonal to running and attaches to
    // a row the projection already owns.
    let derived = subagent_row_name(agent, task.as_deref()).0;
    let key = registry_entry_for_event(agent, task.as_deref())
        .map(|entry| entry.key)
        .filter(|key| subagents.iter().any(|row| row.name == *key))
        .unwrap_or_else(|| derived.clone());
    let Some(existing) = subagents.iter_mut().find(|s| s.name == key) else {
        tracing::debug!(
            "queue transition for {key} has no panel row yet; the worker registry owns row existence, so it is skipped"
        );
        return false;
    };
    existing.queue = queue;
    // A slot transition is real progress on this delegation: it is the one piece
    // of activity a queued call produces, and it keeps the row's idle accounting
    // honest without moving any clock.
    existing.last_activity_at = Some(std::time::Instant::now());
    true
}

/// Resolve the **panel row** a stream spelling belongs to.
///
/// A stream tag is spelled `{agent}-{task}` or bare `{agent}`
/// (`orchestrator/execution.rs`), while a row is named by the **effective registry
/// key** (`WorkerEntry::key`), which may carry a handle the tag can never spell:
/// `{agent}-wN` for a taskless worker, `{base}#N` when a live worker already owned
/// the natural key. Every consumer of a stream spelling — the streamed text, the
/// thinking flags and the arbitration pause — must land on the same row, so the
/// rule is owned once here instead of per call site.
///
/// Order, strongest first: exact key, natural key (the tag with the `#…` collision
/// handle stripped), the `{agent}-wN` form of a bare agent tag, then task-id
/// equality. Falling back to the tag itself is what keeps the first event for a
/// not-yet-registered worker working: the row is then created under the tag and
/// re-labelled by `set_subagents` when the registry publishes the real key.
pub(crate) fn subagent_row_key_for_tag(rows: &[SubagentDetail], tag: &str) -> String {
    if rows.iter().any(|s| s.name == tag) {
        return tag.to_string();
    }
    let digit_run = |rest: &str| {
        rest.strip_prefix("-w")
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    };
    let natural = rows
        .iter()
        .find(|s| match s.name.split_once('#') {
            Some((head, handle)) => head == tag && !handle.is_empty(),
            None => false,
        })
        .or_else(|| {
            rows.iter()
                .find(|s| s.name.strip_prefix(tag).is_some_and(digit_run))
        });
    if let Some(row) = natural {
        return row.name.clone();
    }
    let by_task = rows.iter().find(|s| {
        let clean = s
            .task_id
            .as_deref()
            .map(|t| crate::task_id::normalize_task_id_ref(t).to_string())
            .unwrap_or_default();
        !clean.is_empty() && (tag == clean.as_str() || tag.ends_with(&format!("-{clean}")))
    });
    if let Some(row) = by_task {
        return row.name.clone();
    }
    tag.to_string()
}

/// Apply a **stream-arbitration pause transition** to an existing panel row.
///
/// The transition is emitted by the preemption handshake itself, so it is a fact
/// about a stream that really yielded (or took back) its model slot — never an
/// inference from an active row, which is what keeps the conditional-pause gate
/// honest: when steering arbitration skipped pausing because the arbitrator had
/// spare session capacity, no transition is emitted at all and this row keeps
/// reading `(Active)`.
///
/// Like [`update_subagent_queue`] it is deliberately orthogonal to the lifecycle
/// path and structurally cannot **create** a row (it takes a slice): rows belong
/// to the registry projection, and a pause for a worker with no row is a no-op.
/// Every teardown that closes a row (`apply_registry_entry`, the lifecycle arm)
/// takes `paused` back down with it, so the flag never goes stale.
///
/// Returns whether a row was updated, so callers only re-push the panel when
/// anything actually changed.
pub(crate) fn update_subagent_paused(
    subagents: &mut [SubagentDetail],
    agent_tag: &str,
    paused: bool,
) -> bool {
    let key = subagent_row_key_for_tag(subagents, agent_tag);
    let Some(existing) = subagents.iter_mut().find(|s| s.name == key) else {
        tracing::debug!(
            "stream-pause transition for {key} has no panel row yet; the worker registry owns row existence, so it is skipped"
        );
        return false;
    };
    if existing.paused == paused {
        return false;
    }
    existing.paused = paused;
    // Yielding or reclaiming a model slot is real activity on this row — it is the
    // only thing a paused stream produces — so the idle accounting stays honest.
    existing.last_activity_at = Some(std::time::Instant::now());
    true
}

pub(crate) fn drain_delegation_events_with_transcript(
    manager: Option<&OrchestratorManager>,
    renderer: &mut dyn Renderer,
    subagents: &mut Vec<SubagentDetail>,
    mut ui_transcript: Option<&mut crate::ui::UiTranscript>,
) {
    let Some(manager) = manager else {
        return;
    };
    // Register this Manager's event vector as the process-wide delegation sink.
    // The concurrency gate in `ChatClient::acquire_slot` cannot thread the
    // Manager through its signatures, so it appends its `Queued`/`Running`
    // transitions through that sink — into this very vector, which is what keeps
    // the authoritative order (`Started -> Queued* -> Running -> Completed |
    // Failed`) intact and keeps one writer for the panel. The compare-and-set
    // keeps the write lock off the hot path: only a Manager swap pays for it.
    if crate::orchestrator::delegation_event_sink()
        .is_none_or(|sink| !Arc::ptr_eq(&sink, &manager.delegation_events))
    {
        crate::orchestrator::set_delegation_event_sink(Arc::clone(&manager.delegation_events));
    }
    let Ok(mut events) = manager.delegation_events.lock() else {
        return;
    };
    let mut changed = false;
    for event in events.drain(..) {
        match &event {
            DelegationEvent::Started { agent, task } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, true);
                changed = true;
            }
            // Slot state inside an already-dispatched delegation: same row, same
            // clocks, same active flag — only the queue field moves.
            DelegationEvent::Queued {
                agent,
                task,
                position,
                of,
            } => {
                if update_subagent_queue(
                    subagents,
                    *agent,
                    task.clone(),
                    Some(QueueState {
                        position: *position,
                        of: *of,
                    }),
                ) {
                    changed = true;
                }
            }
            DelegationEvent::Running { agent, task } => {
                if update_subagent_queue(subagents, *agent, task.clone(), None) {
                    changed = true;
                }
            }
            DelegationEvent::Completed { agent, task } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, false);
                changed = true;
                if let Some(ref mut tr) = ui_transcript {
                    let task_id = task.clone().unwrap_or_else(|| agent.to_string());
                    tr.append(crate::ui::UiRecord::TaskCompleted { task_id });
                }
            }
            DelegationEvent::Failed {
                agent,
                task,
                reason,
            } => {
                update_subagent_lifecycle(subagents, *agent, task.clone(), None, false);
                changed = true;
                if let Some(ref mut tr) = ui_transcript {
                    let task_id = task.clone().unwrap_or_else(|| agent.to_string());
                    tr.append(crate::ui::UiRecord::TaskFailed {
                        task_id,
                        reason: reason.clone(),
                    });
                }
            }
        }
        renderer.on_event(&Event::Delegation(event));
    }
    if changed {
        renderer.set_subagents(subagents.clone());
    }
}

/// Sanitize task id string by stripping markdown enclosing characters.
pub fn clean_task_id(tid: &str) -> String {
    crate::task_id::normalize_task_id_ref(tid).to_string()
}

/// Find a subagent in the list by matching name, task id, or suffix.
pub fn find_subagent_mut<'a>(
    subagents: &'a mut [SubagentDetail],
    name: &str,
    task_id: Option<&str>,
) -> Option<&'a mut SubagentDetail> {
    subagents.iter_mut().find(|s| {
        if s.name == name {
            return true;
        }
        if let (Some(a), Some(b)) = (s.task_id.as_deref(), task_id) {
            let a_clean = clean_task_id(a);
            let b_clean = clean_task_id(b);
            if !a_clean.is_empty() && a_clean == b_clean {
                return true;
            }
        }
        if let Some(tid) = task_id {
            let clean = clean_task_id(tid);
            if !clean.is_empty() && s.name.ends_with(&clean) {
                return true;
            }
        }
        if let Some(s_tid) = s.task_id.as_deref() {
            let clean = clean_task_id(s_tid);
            if !clean.is_empty() && name.ends_with(&clean) {
                return true;
            }
        }
        false
    })
}

/// Extract a concise, human-readable failure reason from specialist or tool error outputs.
///
/// The marker-shaped part of the vocabulary (`REPLAN REQUIRED`, `FAILED (…)`,
/// `FAILED:`) is recognised by [`crate::markers::failure_reason`] — the single
/// owner of that grammar. Everything after it (`VALIDATOR REJECTION:`, `ERROR:`,
/// abort lines, first-line fallback) is UI wording, and the truncation budget is
/// presentation, so both stay here.
pub fn extract_failure_reason(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "unknown failure".to_string();
    }

    // 1. Look for VALIDATOR REJECTION: <critique>
    if let Some(pos) = trimmed.find("VALIDATOR REJECTION:") {
        let after = &trimmed[pos + "VALIDATOR REJECTION:".len()..];
        let first_line = after.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let clean = first_line.trim().trim_end_matches('.').trim();
        if !clean.is_empty() && clean != "---------------" {
            return truncate_reason(clean, 120);
        }
    }

    // 2. The marker-shaped verdicts (`REPLAN REQUIRED (<task>): …`,
    //    `FAILED (<reason>)`, `FAILED: <reason>`) are parsed by their single
    //    owner, `crate::markers`, which walks one failure-prefix table. The
    //    120-char budget stays here: truncation is presentation, not grammar,
    //    and the rendered bytes are unchanged.
    if let Some(reason) = crate::markers::failure_reason(trimmed) {
        return truncate_reason(reason, 120);
    }

    // 3. Look for ERROR: <reason>
    if let Some(pos) = trimmed.find("ERROR:") {
        let after = &trimmed[pos + "ERROR:".len()..];
        let first_line = after.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let clean = first_line.trim();
        if !clean.is_empty() {
            return truncate_reason(clean, 120);
        }
    }

    // 4. Look for lines starting with "Task aborted"
    for line in trimmed.lines() {
        let l = line.trim();
        if l.to_ascii_lowercase().starts_with("task aborted") {
            return truncate_reason(l, 120);
        }
    }

    // 5. Fallback: first non-empty line
    let first_line = trimmed
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(trimmed);
    let clean = first_line
        .trim()
        .trim_matches(|c| c == '*' || c == '`' || c == '#')
        .trim();
    truncate_reason(clean, 120)
}

fn truncate_reason(s: &str, max_len: usize) -> String {
    let s = s.trim();
    // Char-boundary-safe prefix (shared helper): a byte slice like `s[..max_len]`
    // can panic mid-character on multi-byte text (å/ä/ö, emoji, …).
    let head = crate::text_util::truncate_chars(s, max_len);
    if head.len() == s.len() {
        s.to_string()
    } else {
        let mut truncated = head.to_string();
        if let Some(last_space) = truncated.rfind(' ')
            && truncated[..last_space].chars().count() > max_len * 2 / 3
        {
            truncated.truncate(last_space);
        }
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_abort_command_recognizes_variants() {
        assert!(is_abort_command("/abort"));
        assert!(is_abort_command("abort"));
        assert!(is_abort_command("ABORT"));
        assert!(is_abort_command("/exit"));
        assert!(is_abort_command("exit"));
        assert!(is_abort_command("/quit"));
        assert!(is_abort_command("quit"));
        assert!(is_abort_command("/q"));
        assert!(is_abort_command(":q"));
        assert!(is_abort_command(":q!"));
        assert!(is_abort_command("/stop"));
        assert!(is_abort_command("stop"));
        assert!(is_abort_command("/cancel"));
        assert!(is_abort_command("cancel"));
        assert!(!is_abort_command("continue"));
        assert!(!is_abort_command("status"));
    }

    /// Command interpretation is restricted to **single-line** submissions: a
    /// submitted block that spans more than one line — what a pasted snippet
    /// produces, delivered as ONE steering message — is never parsed as a command,
    /// even when its first line is the command word.
    #[test]
    fn test_is_abort_command_rejects_multi_line_submissions() {
        // Single-line variants keep working, padding included.
        assert!(is_abort_command("  STOP  "));
        assert!(is_abort_command("/abort\n"));
        // Multi-line submissions are ordinary text, never an abort.
        assert!(!is_abort_command("stop\nplease stop"));
        assert!(!is_abort_command("/abort\nmen fortsätt"));
        assert!(!is_abort_command("please stop\nnow"));
        assert!(!is_abort_command("stop\r\nnow"));
        assert!(!is_abort_command("/abort\n\n/abort"));
    }

    /// Same single-line gate for the reset aliases: the first line of a pasted
    /// block must not be able to wipe the execution plan.
    #[test]
    fn test_is_reset_command_rejects_multi_line_submissions() {
        assert!(is_reset_command("/reset"));
        assert!(is_reset_command("  /RESET  \n"));
        assert!(!is_reset_command("/reset\nnästa rad"));
        assert!(!is_reset_command("/reset\n/reset"));
        assert!(!is_reset_command("reset the plan\n/reset"));
        assert!(!is_reset_command("/reset_plan\r\nkeep the plan"));
    }

    /// The gate itself: trailing/leading newlines are padding, a line break
    /// **between content** is what makes a submission multi-line.
    #[test]
    fn test_submission_is_single_line_separates_padding_from_a_second_line() {
        for single in [
            "",
            "   ",
            "/reset",
            "  /RESET  \n",
            "/abort\n",
            "/abort\r\n",
            "en rad",
            "\n/reset\n",
        ] {
            assert!(
                submission_is_single_line(single),
                "{single:?} is single-line"
            );
        }
        for multi in [
            "/reset\nnästa rad",
            "stop\nplease stop",
            "a\r\nb",
            "x\ny\nz",
            "\n\n/reset\n\nmen mer\n",
        ] {
            assert!(
                !submission_is_single_line(multi),
                "{multi:?} spans more than one line"
            );
        }
    }

    #[test]
    fn test_format_tool_call_utf8_char_boundary_no_panic() {
        let cmd_prefix = "c".repeat(56);
        let cmd = format!("{cmd_prefix}—cargo check");
        let args = serde_json::json!({ "command": cmd });
        let formatted = format_tool_call_display(crate::tool_names::TOOL_RUN_COMMAND, &args);
        assert!(formatted.starts_with("run_command("));
        assert!(formatted.ends_with("…)"));

        let custom_prefix = "d".repeat(56);
        let custom_args = serde_json::json!({ "arg": format!("{custom_prefix}—value") });
        let custom_formatted = format_tool_call_display("custom", &custom_args);
        assert!(custom_formatted.starts_with("custom("));
        assert!(custom_formatted.ends_with("…)"));
    }

    #[test]
    fn test_extract_failure_reason() {
        assert_eq!(
            extract_failure_reason(
                "VALIDATOR REJECTION: Specialist generated conversational text without executing any tools.\n---------------\nFAILED"
            ),
            "Specialist generated conversational text without executing any tools"
        );
        assert_eq!(
            extract_failure_reason(
                "REPLAN REQUIRED (t-002): task too complex — exceeded reasoning budget"
            ),
            "task too complex — exceeded reasoning budget"
        );
        assert_eq!(
            extract_failure_reason("REPLAN REQUIRED: missing dependency foo"),
            "missing dependency foo"
        );
        assert_eq!(
            extract_failure_reason("Task aborted by user instruction.\n\nFAILED (aborted)"),
            "aborted"
        );
        assert_eq!(
            extract_failure_reason("FAILED: compilation error on line 42"),
            "compilation error on line 42"
        );
        assert_eq!(
            extract_failure_reason("ERROR: timeout waiting for response"),
            "timeout waiting for response"
        );
        assert_eq!(
            extract_failure_reason("Custom single-line failure message"),
            "Custom single-line failure message"
        );
        assert_eq!(extract_failure_reason("   "), "unknown failure");
    }

    /// Byte-pin (table driven): the marker-shaped verdicts are read through
    /// [`crate::markers::failure_reason`], and the **rendered output of this UI
    /// helper is unchanged** — same reason text, same 120-char budget.
    #[test]
    fn test_extract_failure_reason_marker_shapes_are_byte_pinned() {
        let cases: &[(&str, &str)] = &[
            // `FAILED (<reason>)`
            (
                "Task aborted by user instruction.\n\nFAILED (aborted)",
                "aborted",
            ),
            (
                "Report\n\nFAILED (Validator rejected deliverable)",
                "Validator rejected deliverable",
            ),
            ("FAILED (nested (inner) outer)", "nested (inner"),
            // `FAILED: <reason>`
            (
                "FAILED: compilation error on line 42",
                "compilation error on line 42",
            ),
            ("FAILED: first line\nignored second line", "first line"),
            ("FAILED:\n\n  wrapped reason\n", "wrapped reason"),
            // `REPLAN REQUIRED …`
            (
                "REPLAN REQUIRED (t-002): task too complex — exceeded reasoning budget",
                "task too complex — exceeded reasoning budget",
            ),
            (
                "REPLAN REQUIRED: missing dependency foo",
                "missing dependency foo",
            ),
            ("REPLAN REQUIRED bare verdict text", "bare verdict text"),
            // a replan verdict wins over a FAILED shape in the same body
            (
                "FAILED: stale note\nREPLAN REQUIRED: the decomposition is wrong",
                "the decomposition is wrong",
            ),
            // whitespace around the whole body must not move the reason
            ("  \n FAILED ( padded )  \n", "padded"),
        ];
        for (input, expected) in cases {
            assert_eq!(extract_failure_reason(input), *expected, "input: {input:?}");
        }

        // Verdict shapes with no reason clause fall through to the UI's own
        // non-marker heuristics.
        let fallthrough: &[(&str, &str)] = &[
            ("FAILED ()", "FAILED ()"),
            ("REPLAN REQUIRED (t-003)", "REPLAN REQUIRED (t-003)"),
            (
                "ERROR: timeout waiting for response",
                "timeout waiting for response",
            ),
            (
                "Task aborted by user instruction.",
                "Task aborted by user instruction.",
            ),
        ];
        for (input, expected) in fallthrough {
            assert_eq!(extract_failure_reason(input), *expected, "input: {input:?}");
        }
    }

    /// For any marker-shaped input the UI's answer is exactly the marker owner's
    /// reason, truncated by the UI's own budget — the two layers cannot drift apart
    /// silently.
    #[test]
    fn test_extract_failure_reason_delegates_to_the_marker_owner() {
        let inputs = [
            "FAILED (aborted)",
            "Task aborted by user instruction.\n\nFAILED (aborted)",
            "FAILED: build broke",
            "REPLAN REQUIRED (t-002): plan is wrong",
            "REPLAN REQUIRED: plan is wrong",
            "preface\n\nFAILED (Validator rejected deliverable)\ntrailer",
        ];
        for input in inputs {
            let owner = crate::markers::failure_reason(input.trim())
                .unwrap_or_else(|| panic!("{input:?} must yield a marker reason"));
            assert_eq!(
                extract_failure_reason(input),
                truncate_reason(owner, 120),
                "input: {input:?}"
            );
        }
        // The budget is UI-side: a long marker reason is still truncated here,
        // never by the marker owner.
        let long = "FAILED: ".to_string() + &"problem ".repeat(40);
        let out = extract_failure_reason(&long);
        assert!(out.ends_with("..."), "{out}");
        assert!(out.chars().count() <= 123, "{out}");
    }

    #[test]
    fn test_truncate_reason_multibyte_no_panic() {
        // A byte slice like `&s[..max_len]` panics with "byte index is not a char
        // boundary" on multi-byte reasons crossing the slice point; these inputs
        // cross it deliberately.
        // "x" + 60x 'ä' = 121 bytes; byte index 120 falls inside an 'ä'.
        let s = format!("{}{}", "x", "ä".repeat(60));
        assert_eq!(s.len(), 121);
        assert!(!s.is_char_boundary(120));
        assert_eq!(truncate_reason(&s, 120), s);

        // "a" + 30x emoji = 121 bytes; byte index 120 falls inside a 🙂.
        let e = format!("{}{}", "a", "🙂".repeat(30));
        assert_eq!(e.len(), 121);
        assert!(!e.is_char_boundary(120));
        assert_eq!(truncate_reason(&e, 120), e);

        // Long enough to also exercise the truncation branch with multi-byte text.
        let long = format!("{}{}", "x", "ä".repeat(150));
        let out = truncate_reason(&long, 120);
        assert_eq!(out.chars().count(), 123); // 120 chars + "..."
        assert!(out.ends_with("..."));
        assert!(!out.contains(std::char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn test_truncate_reason_ascii_word_break_unchanged() {
        // Visible output preserved for ASCII: word break at the last space
        // past 2/3 of the limit, then "...".
        assert_eq!(
            truncate_reason("the quick brown fox jumps over", 20),
            "the quick brown fox..."
        );
        assert_eq!(truncate_reason(&"a".repeat(20), 20), "a".repeat(20));
        assert_eq!(
            truncate_reason(&"ä".repeat(21), 20),
            format!("{}...", "ä".repeat(20))
        );
    }

    #[test]
    fn test_extract_failure_reason_multibyte_no_panic() {
        // End-to-end at the call site: a >120-char multi-byte FAILED reason.
        let reason = format!(
            "{}{}{}",
            "problem ",
            "självråkdiskriminering 👍 ",
            "wäsen".repeat(30)
        );
        let out = extract_failure_reason(&format!("FAILED: {reason}"));
        assert!(out.ends_with("..."));
        assert!(!out.contains(std::char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn test_load_system_prompt_without_base_prompt() {
        let cfg = Config::default();
        let plan = crate::manager::phase::Plan::default();
        let prompt = load_system_prompt_with_plan_and_warning(&cfg, &plan)
            .map(|(prompt, _warning)| prompt)
            .expect("load success");
        assert!(!prompt.contains("## Machine Specific Guidance"));
    }

    #[test]
    fn test_load_system_prompt_with_base_prompt() {
        let cfg = Config {
            base_prompt: Some("Do not schedule any tasks in parallel.".to_string()),
            ..Default::default()
        };
        let plan = crate::manager::phase::Plan::default();
        let prompt = load_system_prompt_with_plan_and_warning(&cfg, &plan)
            .map(|(prompt, _warning)| prompt)
            .expect("load success");
        assert!(prompt.contains("## Machine Specific Guidance"));
        assert!(prompt.contains("Do not schedule any tasks in parallel."));
    }

    #[test]
    fn test_load_system_prompt_with_heading_base_prompt() {
        let cfg = Config {
            base_prompt: Some("### Custom Instructions\nSingle worker only.".to_string()),
            ..Default::default()
        };
        let plan = crate::manager::phase::Plan::default();
        let prompt = load_system_prompt_with_plan_and_warning(&cfg, &plan)
            .map(|(prompt, _warning)| prompt)
            .expect("load success");
        assert!(!prompt.contains("## Machine Specific Guidance"));
        assert!(prompt.contains("### Custom Instructions\nSingle worker only."));
    }

    #[test]
    fn test_load_system_prompt_with_whitespace_base_prompt() {
        let cfg = Config {
            base_prompt: Some("   \n\t  ".to_string()),
            ..Default::default()
        };
        let plan = crate::manager::phase::Plan::default();
        let prompt = load_system_prompt_with_plan_and_warning(&cfg, &plan)
            .map(|(prompt, _warning)| prompt)
            .expect("load success");
        assert!(!prompt.contains("## Machine Specific Guidance"));
    }

    /// This owned UI seam is a pure delegation to `crate::task_id`: re-spelling the
    /// trimming chain here fails this test.
    #[test]
    fn test_clean_task_id_delegates_to_task_id_normalizer() {
        for raw in [
            "[t-001]",
            "\"t-001\"",
            "  ('t-001')  ",
            "t-001",
            "[t-001",
            "[t-001]  ",
        ] {
            assert_eq!(clean_task_id(raw), "t-001", "owned UI seam for {raw:?}");
            assert_eq!(
                clean_task_id(raw),
                crate::task_id::normalize_task_id_ref(raw),
                "owned UI seam diverges from the canonical normalizer for {raw:?}"
            );
        }
        for raw in ["", "[]", "   ", "\"\"", "[ ]", "[\"\"]"] {
            assert_eq!(clean_task_id(raw), "", "owned UI seam for {raw:?}");
        }
    }

    /// The `find_subagent_mut` matching layer on top of the seam must keep
    /// decorated and plain spellings interchangeable, and must never let an
    /// empty normalization match everything.
    #[test]
    fn test_find_subagent_mut_matches_decorated_task_ids() {
        let mut subs = vec![SubagentDetail {
            name: "coder-t-001".to_string(),
            ..Default::default()
        }];
        assert!(find_subagent_mut(&mut subs, "coder", Some("[t-001]")).is_some());
        assert!(find_subagent_mut(&mut subs, "coder", Some("t-001")).is_some());
        assert!(find_subagent_mut(&mut subs, "coder", Some("[]")).is_none());
    }

    /// Regression guard: the pane's row set is a **projection of the worker
    /// registry**. Rows exist only for registering classes, are named by the
    /// **effective registry key** (so two live workers for one agent+task get two
    /// rows instead of merging into one), keep their presentation state, and go
    /// idle or disappear exactly when the registry says so — never because an
    /// event handler decided so.
    ///
    /// `WORKERS` is a process-wide table, so every assertion here is scoped to the
    /// keys this test owns; other tests may hold their own entries at the same time.
    #[test]
    fn subagent_rows_are_a_projection_of_the_worker_registry() {
        use crate::orchestrator::workers;
        // `WORKERS` is process-global and other suites assert **global** invariants over
        // it (the completed-ring cap), so a test that registers live workers takes the
        // crate's own serialization seam for that table.
        let _workers = workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Private task ids keep these keys out of the way of any other test that
        // reads the shared registry.
        let first = workers::register_active_worker(
            Some("t-9001".to_string()),
            "coder".to_string(),
            "brief one".to_string(),
        );
        let second = workers::register_active_worker(
            Some("t-9001".to_string()),
            "coder".to_string(),
            "brief two".to_string(),
        );
        let (key_a, key_b) = (first.0.clone(), second.0.clone());
        // A token-count stub is not a registration, so it must never become a row.
        workers::update_active_worker_context("coder-t-9002", 4242);
        let stub = "coder-t-9002".to_string();
        assert_ne!(key_a, key_b, "two live workers hold two distinct keys");
        let mine = |rows: &[SubagentDetail]| -> Vec<SubagentDetail> {
            rows.iter()
                .filter(|r| r.name == key_a || r.name == key_b || r.name == stub)
                .cloned()
                .collect()
        };
        let mut names = vec![key_a.clone(), key_b.clone()];
        names.sort();

        let mut rows = vec![SubagentDetail {
            name: key_a.clone(),
            thinking: "streamed thought".to_string(),
            logs: vec!["started task t-9001".to_string()],
            queue: Some(crate::ui::QueueState { position: 2, of: 3 }),
            paused: false,
            ..Default::default()
        }];
        assert!(
            project_subagent_rows(&mut rows),
            "the registry holds live workers the pane did not know about"
        );
        let scoped: Vec<String> = mine(&rows).iter().map(|r| r.name.clone()).collect();
        assert_eq!(
            scoped, names,
            "one row per registry entry, in registry key order, and never one for a stub"
        );
        let row_a = rows.iter().find(|r| r.name == key_a).expect("row A");
        let row_b = rows.iter().find(|r| r.name == key_b).expect("row B");
        assert_eq!(
            row_a.thinking, "streamed thought",
            "presentation state survives the rebuild"
        );
        assert!(
            row_a.is_active && row_b.is_active,
            "both workers are running for the Manager and the arbitrator"
        );
        assert!(
            row_a.started_at.is_some() && row_b.started_at.is_some(),
            "the clock comes from the registry entry"
        );
        assert_eq!(row_b.prompt, "brief two", "the brief comes along");

        // The badges a viewer needs to tell the rows apart: a duplicate-key
        // registration says `Dup #N`, a validator-family key says `Audit`.
        for key in [&key_a, &key_b] {
            match key.split_once('#') {
                Some((_, handle)) => assert_eq!(
                    subagent_row_badge(None, key),
                    Some(format!("Dup #{handle}")),
                    "a duplicate-key row says so"
                ),
                None => assert_eq!(
                    subagent_row_badge(None, key),
                    None,
                    "a plain key has no badge"
                ),
            }
        }
        assert_eq!(
            subagent_row_badge(None, "validator-coder-t-9"),
            Some("Audit".to_string()),
            "a validator-family row reads as an audit"
        );

        // Identity: a lifecycle event naming agent+task resolves through the
        // registry, so it never opens a third row for a worker that has one.
        update_subagent_lifecycle(
            &mut rows,
            crate::agents::Agent::Coder,
            Some("t-9001".to_string()),
            None,
            true,
        );
        assert_eq!(
            mine(&rows).len(),
            2,
            "the event folds into a registry row instead of adding one"
        );
        // And a queue transition never creates a row either.
        assert!(
            !update_subagent_queue(
                &mut rows,
                crate::agents::Agent::Coder,
                Some("t-9003".to_string()),
                Some(crate::ui::QueueState { position: 1, of: 2 })
            ),
            "queue state is orthogonal to row existence"
        );
        assert_eq!(mine(&rows).len(), 2);

        // One worker's guard drops: the registry flips its entry to completed, and
        // THAT is what closes the row.
        drop(second);
        assert!(
            project_subagent_rows(&mut rows),
            "a completed entry changes what the pane shows"
        );
        let closed = rows
            .iter()
            .find(|r| r.name == key_b)
            .expect("the completed ring still holds the entry, so the row stays");
        assert!(!closed.is_active, "the registry closed the row");
        assert!(
            closed.started_at.is_none(),
            "no live clock for a finished run"
        );
        assert!(closed.queue.is_none(), "no queue position outlives the run");
        assert!(
            rows.iter()
                .find(|r| r.name == key_a)
                .is_some_and(|r| r.is_active),
            "the other live worker keeps its row"
        );

        // The entry leaving the registry is the other half of closing: no entry, no row.
        for key in [&key_a, &key_b, &stub] {
            workers::remove_worker_entry_for_test(key);
        }
        assert!(project_subagent_rows(&mut rows));
        assert!(
            mine(&rows).is_empty(),
            "a row whose entry left the registry is gone"
        );
    }

    /// The arbitration pause is a **row slot orthogonal to liveness**: it is set
    /// and cleared only by the transition the preemption handshake actually
    /// delivered, it never overloads `is_active`, and it always lands on the row
    /// the stream tag resolves to — including the spellings a tag can never
    /// produce on its own (`{base}#N`, `{agent}-wN`).
    #[test]
    fn pause_transitions_fold_into_the_row_a_stream_tag_names() {
        let rows = || {
            vec![
                SubagentDetail {
                    name: "coder-t-9100".to_string(),
                    is_active: true,
                    ..Default::default()
                },
                SubagentDetail {
                    name: "planner-t-9101#2".to_string(),
                    is_active: true,
                    ..Default::default()
                },
                SubagentDetail {
                    name: "researcher-w4".to_string(),
                    is_active: true,
                    ..Default::default()
                },
            ]
        };
        let mut subagents = rows();

        assert!(
            update_subagent_paused(&mut subagents, "coder-t-9100", true),
            "a delivered pause is a visible change"
        );
        assert!(
            subagents[0].paused,
            "the row the tag spells is the row that pauses"
        );
        assert!(
            !subagents[1].paused && !subagents[2].paused,
            "sibling rows of the same agent are untouched"
        );
        assert!(
            subagents[0].is_active,
            "a parked stream is still a live worker: `paused` never overloads `is_active`"
        );
        assert!(
            !update_subagent_paused(&mut subagents, "coder-t-9100", true),
            "a repeat of the same transition changes nothing, so nothing is re-pushed"
        );

        // A collision handle the stream tag can never spell still resolves.
        assert!(update_subagent_paused(
            &mut subagents,
            "planner-t-9101",
            true
        ));
        assert!(subagents[1].paused);
        // So does the `{agent}-wN` key behind a bare agent tag.
        assert!(update_subagent_paused(&mut subagents, "researcher", true));
        assert!(subagents[2].paused);
        assert_eq!(
            subagent_row_key_for_tag(&subagents, "planner-t-9101"),
            "planner-t-9101#2"
        );
        assert_eq!(
            subagent_row_key_for_tag(&subagents, "researcher"),
            "researcher-w4"
        );
        // Unknown spellings fall back to the tag, which is what keeps the
        // pre-registration path working without inventing a row here.
        assert_eq!(
            subagent_row_key_for_tag(&subagents, "manager-t-9102"),
            "manager-t-9102"
        );

        // The resume handshake clears it.
        assert!(update_subagent_paused(
            &mut subagents,
            "coder-t-9100",
            false
        ));
        assert!(!subagents[0].paused);
        assert!(
            subagents[1].paused && subagents[2].paused,
            "one stream resuming says nothing about the others"
        );

        // A pause for a worker with no row is a no-op: rows belong to the
        // registry projection, so this can never open a panel row by itself.
        let mut subagents = rows();
        assert!(
            !update_subagent_paused(&mut subagents, "validator-coder-t-9103", true),
            "no row, no pause"
        );
        assert_eq!(
            subagents.iter().filter(|r| r.paused).count(),
            0,
            "and nothing was created behind the scenes"
        );
        assert_eq!(subagents.len(), 3);
    }

    /// A paused row survives a projection rebuild while the worker is live, and
    /// the **registry closing the row** is what takes `Paused` back down — so the
    /// label can never linger after a completion, cancel or guard drop.
    #[test]
    fn a_paused_row_is_cleared_by_the_registry_that_closed_it() {
        use crate::orchestrator::workers;
        let _workers = workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let guard = workers::register_active_worker(
            Some("t-9110".to_string()),
            "coder".to_string(),
            "brief for a parked stream".to_string(),
        );
        let key = guard.0.clone();

        let mut rows = vec![SubagentDetail {
            name: key.clone(),
            is_active: true,
            paused: true,
            queue: Some(crate::ui::QueueState { position: 1, of: 2 }),
            ..Default::default()
        }];
        // The pause was delivered by the handshake, so a projection rebuild has to
        // keep it: the registry knows nothing about the arbitration handshake and
        // must not silently reset a state it never reported. (The change verdict is
        // deliberately not asserted here — `WORKERS` is process-global, so other
        // suites' entries move it; the scoped rows are what matter.)
        let _ = project_subagent_rows(&mut rows);
        let row = rows.iter().find(|r| r.name == key).expect("live row");
        assert!(row.paused, "the projection preserves a delivered pause");
        assert!(row.is_active, "and the worker is still running");

        // The delivered transitions themselves flow through the same row.
        assert!(
            !update_subagent_paused(&mut rows, "coder-t-9110", true),
            "already paused: a repeat is not a change"
        );
        assert!(
            update_subagent_paused(&mut rows, "coder-t-9110", false),
            "the reclaim handshake clears it"
        );
        assert!(
            update_subagent_paused(&mut rows, "coder-t-9110", true),
            "and the next arbitration round arms it again"
        );

        // The guard drops: the registry closes the entry, and that is what clears
        // a `Paused` that would otherwise be stale forever.
        drop(guard);
        assert!(
            project_subagent_rows(&mut rows),
            "a paused row whose worker completed is a visible change"
        );
        let closed = rows
            .iter()
            .find(|r| r.name == key)
            .expect("the completed ring keeps the row visible");
        assert!(!closed.is_active, "the registry closed the row");
        assert!(
            !closed.paused,
            "a pause belongs to a live stream and never outlives it"
        );
        assert!(closed.queue.is_none(), "and neither does a queue position");

        // `paused` is part of what the projection compares **on its own**: with the
        // registry already reporting the row closed, a stale flag left behind by any
        // other writer is itself a visible change, and the rebuild takes it down.
        if let Some(stale) = rows.iter_mut().find(|r| r.name == key) {
            stale.paused = true;
        }
        assert!(
            project_subagent_rows(&mut rows),
            "a stale pause on an already-closed row is a visible change"
        );
        assert!(
            !rows
                .iter()
                .find(|r| r.name == key)
                .expect("the closed row is still projected")
                .paused,
            "and the rebuild clears it"
        );

        workers::remove_worker_entry_for_test(&key);
    }

    // The system prompt must read the plan fail-closed: a plan file that exists
    // but cannot be read or parsed is never presented as "no plan".
    //
    // Every test below roots its plan in a private temporary workspace through
    // `harness::with_workspace_root`, so the repository's real `.marmel/` is
    // never touched, and none of them asserts on a process-global counter.

    /// A two-task plan with multi-byte text, nested markdown and a tab-indented
    /// line: the readable-plan path must embed this body byte for byte.
    const PLAN_MD: &str = "# Execution Plan\n\
- [x] [t-001] Normalize plan reading — `åäö` ✅\n\
- [ ] [t-002] Surface plan-read errors\n\
\n\tindented\ttab + trailing spaces   \n";

    /// Run `f` against a plan rooted inside a private temporary workspace.
    /// The `TempDir` and the plan path are handed back so assertions can still
    /// inspect the real file while the workspace is alive.
    fn with_isolated_plan<T, F: std::future::Future<Output = T>>(
        f: impl FnOnce(crate::manager::phase::Plan) -> F,
    ) -> (tempfile::TempDir, std::path::PathBuf, T) {
        let tmp = tempfile::tempdir().expect("isolated workspace root");
        let root = tmp.path().to_path_buf();
        // `harness::with_workspace_root` is the repo's own scoping seam; drive it
        // from a plain pty-free unit test without spinning up a runtime.
        let (path, out) = futures::executor::block_on(crate::harness::with_workspace_root(
            root.clone(),
            async move {
                let plan = crate::manager::phase::Plan::at(root.join(".marmel"));
                std::fs::create_dir_all(plan.dir()).expect("create isolated .marmel dir");
                let path = plan.plan_path();
                (path, f(plan).await)
            },
        ));
        (tmp, path, out)
    }

    /// (1) A plan that exists but cannot be **read** must hand back a surfaced
    /// warning, and the prompt must never be presented as a finished plan.
    #[test]
    fn test_load_system_prompt_unreadable_plan_is_surfaced_and_not_complete() {
        let (_tmp, plan_path, (prompt, warning)) = with_isolated_plan(|plan| async move {
            // Not valid UTF-8: `Plan::read` reports an error instead of folding
            // the failure into "no plan".
            std::fs::write(plan.plan_path(), b"\xff\xfe\x00 not a plan").expect("write plan bytes");
            assert!(plan.exists(), "the plan file must exist on disk");
            assert!(
                plan.read().is_err(),
                "non-UTF-8 plan bytes must make the plan read fail"
            );
            load_system_prompt_with_plan_and_warning(&Config::default(), &plan)
                .expect("the prompt itself must still load")
        });

        let warning = warning.expect("an unreadable plan must hand back a warning to surface");
        assert!(
            warning.contains("plan state is UNKNOWN"),
            "the warning must say the plan state is unknown: {warning}"
        );
        assert!(
            warning.contains(&plan_path.display().to_string()),
            "the warning must name the plan file the user can act on: {warning}"
        );

        // Fail-closed: the prompt carries the warning visibly and must not
        // present a plan block, which the model and the UI would read as "no
        // work pending / all complete".
        assert!(
            prompt.contains("## Execution Plan State: UNKNOWN"),
            "the prompt itself must carry the UNKNOWN section:\n{prompt}"
        );
        assert!(
            prompt.contains("plan state is UNKNOWN"),
            "the prompt must repeat the surfaced warning text:\n{prompt}"
        );
        assert!(
            prompt.contains(&plan_path.display().to_string()),
            "the prompt must name the unreadable plan file:\n{prompt}"
        );
        assert!(
            !prompt.contains("## Active Execution Plan"),
            "no plan block may be rendered for an unreadable plan:\n{prompt}"
        );
        assert!(
            !prompt.contains("tasks are now COMPLETE"),
            "an unreadable plan must never be presented as complete:\n{prompt}"
        );
        assert!(
            !prompt.contains("All execution plan tasks"),
            "an unreadable plan must never be reported as all-complete:\n{prompt}"
        );
    }

    /// A plan that **is** readable but carries no parseable task line is equally
    /// unknown — the shared plan-read decision owns that verdict, not this helper.
    #[test]
    fn test_load_system_prompt_unparseable_plan_is_surfaced_and_not_complete() {
        let (_tmp, _plan_path, (prompt, warning)) = with_isolated_plan(|plan| async move {
            std::fs::write(plan.plan_path(), "just prose, no task boxes at all\n").expect("write");
            load_system_prompt_with_plan_and_warning(&Config::default(), &plan)
                .expect("prompt loads")
        });
        let warning = warning.expect("an unparseable plan must hand back a warning");
        assert!(warning.contains("plan state is UNKNOWN"), "{warning}");
        assert!(prompt.contains("## Execution Plan State: UNKNOWN"));
        assert!(!prompt.contains("## Active Execution Plan"));
    }

    /// No plan file on disk is genuinely "nothing pending" and must **not** warn
    /// — the shared plan-read decision keeps that case apart from a failed read.
    #[test]
    fn test_load_system_prompt_missing_plan_does_not_warn() {
        let (_tmp, _plan_path, (prompt, warning)) = with_isolated_plan(|plan| async move {
            assert!(!plan.exists(), "no plan file in the fresh workspace");
            load_system_prompt_with_plan_and_warning(&Config::default(), &plan)
                .expect("prompt loads")
        });
        assert!(warning.is_none(), "a missing plan file must not warn");
        assert!(!prompt.contains("UNKNOWN"), "no UNKNOWN section: {prompt}");
        assert!(!prompt.contains("## Active Execution Plan"), "{prompt}");
    }

    /// A readable plan still embeds the plan block byte-faithfully, and the
    /// loader's prompt projection stays identical across calls.
    #[test]
    fn test_load_system_prompt_readable_plan_embeds_plan_block_byte_faithfully() {
        let cfg = Config::default();
        let (_tmp, plan_path, (prompt, warning, projected_prompt)) =
            with_isolated_plan(|plan| async move {
                let cfg = cfg.clone();
                plan.create(PLAN_MD)
                    .expect("create plan in isolated workspace");
                let (prompt, warning) =
                    load_system_prompt_with_plan_and_warning(&cfg, &plan).expect("prompt loads");
                // Projection equivalent: this call with the warning discarded must
                // hand back exactly the prompt the warning-carrying call produced, so
                // the prompt-only projection stays covered.
                let projected = load_system_prompt_with_plan_and_warning(&cfg, &plan)
                    .map(|(prompt, _warning)| prompt)
                    .expect("prompt loads");
                (prompt, warning, projected)
            });

        assert!(warning.is_none(), "a readable plan must not warn");
        assert!(
            prompt.contains("## Active Execution Plan"),
            "the plan block must be present:\n{prompt}"
        );
        assert!(
            !prompt.contains("UNKNOWN"),
            "a readable plan must not carry the UNKNOWN section"
        );

        // Byte-faithful against the bytes actually on disk: no re-spelling of
        // newlines, tabs, markdown or multi-byte text.
        let on_disk = std::fs::read_to_string(&plan_path).expect("plan file on disk");
        assert!(
            prompt.contains(on_disk.trim()),
            "the plan body must be embedded verbatim\n--- on disk ---\n{on_disk}\n--- prompt ---\n{prompt}"
        );
        assert!(
            prompt.contains("`åäö` ✅") && prompt.contains("\tindented\ttab + trailing spaces"),
            "multi-byte text and tabs must survive verbatim:\n{prompt}"
        );
        assert_eq!(
            projected_prompt, prompt,
            "the wrapper-equivalent projection must return the same prompt as the warning-carrying loader"
        );
    }

    /// Non-vacuity witness: a shape that swallows the read result
    /// (`if let Ok(Some(..))`) yields a prompt with **no** plan section at all for
    /// the unreadable plan above — i.e. fail-open, which this test rules out.
    #[test]
    fn test_load_system_prompt_fail_open_shape_would_have_been_silent() {
        let (_tmp, _plan_path, ()) = with_isolated_plan(|plan| async move {
            std::fs::write(plan.plan_path(), b"\xff\xfe\x00 not a plan").expect("write plan bytes");
            let swallowed: Option<String> = plan.read().ok().flatten();
            assert!(
                swallowed.is_none(),
                "the old `if let Ok(Some(..))` shape must be provably unable to see this plan"
            );
        });
    }
}
