//! Active specialist workers registry, context token tracking, and RAII guards.
//!
//! All worker state lives in a single sharded [`DashMap`] (`WORKERS`). A worker
//! key survives completion: on drop the entry is flipped to a "completed"
//! phase in place (preserving the last-seen context token count), and a
//! bounded 10-entry completed cap evicts the oldest completed entry. No
//! function in this module ever holds more than one map guard at a time.
//!
//! Two registry invariants (recon H5):
//! 1. Every worker key is unique *among live workers*. Keys are
//!    `{agent}-{task}` when a task id exists, `{agent}-w{n}` (a monotonic id
//!    from `WORKER_ID_SEQ`) when it does not, and `{base}#{n}` when the
//!    natural key is already owned by a live worker.
//! 2. A registration never overwrites a live entry — it allocates a distinct
//!    key instead and reports that key through [`ActiveWorkerGuard`]. Losing a
//!    live entry would lose its `CancellationToken` and make the worker
//!    un-killable.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use dashmap::Entry;

/// Information about a specialist worker currently executing a task.
#[derive(Debug, Clone)]
pub struct ActiveWorkerInfo {
    pub task_id: Option<String>,
    pub agent_name: String,
    pub prompt: String,
    pub started_at: Instant,
    pub started_wall: chrono::DateTime<chrono::Local>,
    pub context_tokens: usize,
    pub implementation_turns: usize,
    pub validation_rounds: usize,
    pub latest_validator_feedback: Option<String>,
    pub status: String,
    pub cancel_token: Option<tokio_util::sync::CancellationToken>,
}

/// Information about a recently completed specialist task.
#[derive(Debug, Clone)]
pub struct CompletedWorkerInfo {
    pub task_id: Option<String>,
    pub agent_name: String,
    pub prompt: String,
    pub started_at: Instant,
    pub started_wall: chrono::DateTime<chrono::Local>,
    pub completed_at: Instant,
    pub completed_wall: chrono::DateTime<chrono::Local>,
    pub duration: std::time::Duration,
    pub implementation_turns: usize,
    pub validation_rounds: usize,
    pub latest_validator_feedback: Option<String>,
    pub status: String,
}

/// Merged per-worker state: the active worker info plus the persisted
/// after-completion context token count and completion timestamps.
#[derive(Debug, Clone)]
pub struct WorkerState {
    pub info: ActiveWorkerInfo,
    /// Last-seen context token count. Always in sync with
    /// [`WorkerState::info.context_tokens`] while the worker is active, and
    /// preserved after completion so `get_active_worker_tokens` keeps
    /// returning the last value for recently-finished workers.
    pub last_tokens: usize,
    /// Monotonic completion sequence (None while active); used to order the
    /// recently-completed list and to evict the oldest completed entry.
    pub completed_seq: Option<u64>,
    pub completed_at: Option<Instant>,
    pub completed_wall: Option<chrono::DateTime<chrono::Local>>,
}

impl WorkerState {
    /// True if this worker is still active (not yet completed).
    pub fn is_active(&self) -> bool {
        self.completed_seq.is_none()
    }

    /// Project this state into the legacy completed-worker view.
    pub fn completed_info(&self) -> Option<CompletedWorkerInfo> {
        let completed_at = self.completed_at?;
        Some(CompletedWorkerInfo {
            task_id: self.info.task_id.clone(),
            agent_name: self.info.agent_name.clone(),
            prompt: self.info.prompt.clone(),
            started_at: self.info.started_at,
            started_wall: self.info.started_wall,
            completed_at,
            completed_wall: self.completed_wall?,
            duration: completed_at - self.info.started_at,
            implementation_turns: self.info.implementation_turns,
            validation_rounds: self.info.validation_rounds,
            latest_validator_feedback: self.info.latest_validator_feedback.clone(),
            status: self.info.status.clone(),
        })
    }
}

/// Bound on how many completed worker entries survive in the map.
const MAX_RECENT_COMPLETED: usize = 10;

/// Single sharded map holding every worker (active and recently completed).
static WORKERS: LazyLock<DashMap<String, WorkerState>> = LazyLock::new(DashMap::new);

/// Monotonic sequence handed out to completed entries (oldest = smallest).
static COMPLETED_SEQ: LazyLock<std::sync::atomic::AtomicU64> = LazyLock::new(|| AtomicU64::new(0));

/// Unique-id source for worker registry keys (recon H5).
///
/// Every registration draws a strictly increasing id from this counter. It
/// replaces the previous pseudo-id `Instant::now().elapsed().as_nanos()`, which
/// is *not* a unique id: the value is a near-constant low-entropy clock offset
/// (measured 202/33/41/39/40 in the audit), so two workers registered in the
/// same tick produced the same key (e.g. `coder-33` twice).
///
/// It is also the disambiguator used when a registration would otherwise
/// overwrite a *live* registry entry (see [`claim_worker_slot`]).
static WORKER_ID_SEQ: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));

/// Next monotonic worker id. Never reused within a process.
fn next_worker_id() -> u64 {
    WORKER_ID_SEQ.fetch_add(1, Ordering::SeqCst)
}

/// Separator marking a collision-resolved ("disambiguated") worker key, e.g.
/// `coder-t-001#7`. The base key `{agent}-{task}` is always a prefix of its
/// disambiguated form — which is **why** matching must never be prefix- or
/// substring-based: everything after `#` is an opaque collision handle, and the
/// identity behind it is recovered structurally by
/// [`crate::orchestrator::notice::worker_routing_identity`] (t-065).
const KEY_DISAMBIGUATOR: char = '#';

/// Bound on key-disambiguation retries. Each retry draws a fresh id from
/// [`WORKER_ID_SEQ`], so a collision on a disambiguated key can only happen if
/// a caller-supplied task id literally spells out that suffix; past this bound
/// the key falls back to a UUID, which cannot collide.
const MAX_KEY_DISAMBIGUATION_RETRIES: usize = 16;

#[cfg(test)]
pub static TEST_WORKERS_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

/// RAII guard that automatically unregisters an active worker on drop and moves it to recently completed.
///
/// The wrapped `String` is the **effective registry key** allocated for that
/// registration (see [`register_active_worker_with_token`]) — it may differ
/// from the natural `{agent}-{task}` key when the natural key was already held
/// by another live worker.
pub struct ActiveWorkerGuard(pub String);

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        // Single entry mutation: flip this worker to completed in place,
        // preserving the key and its last-seen token count. Only a *live* entry
        // is flipped, which makes the drop idempotent: a stale/duplicate guard
        // cannot re-complete an entry, and cannot mark a still-running worker's
        // entry completed (pre-fix, after a clobber) — that hid live workers
        // from `has_active_workers()` and from every cancel path, both of which
        // filter on `is_active()`.
        if let Some(mut entry) = WORKERS.get_mut(&self.0)
            && entry.is_active()
        {
            let seq = COMPLETED_SEQ.fetch_add(1, Ordering::SeqCst);
            entry.last_tokens = entry.info.context_tokens;
            entry.completed_seq = Some(seq);
            entry.completed_at = Some(Instant::now());
            entry.completed_wall = Some(chrono::Local::now());
        }
        // Bounded completed ring: evict oldest completed entries while over cap.
        loop {
            let mut completed: Vec<(String, u64)> = WORKERS
                .iter()
                .filter_map(|e| e.value().completed_seq.map(|s| (e.key().clone(), s)))
                .collect();
            if completed.len() <= MAX_RECENT_COMPLETED {
                break;
            }
            completed.sort_by_key(|(_, s)| *s);
            let to_remove = completed.len().saturating_sub(MAX_RECENT_COMPLETED);
            let mut removed_any = false;
            for (key, _) in completed.into_iter().take(to_remove) {
                if WORKERS.remove(&key).is_some() {
                    removed_any = true;
                }
            }
            if !removed_any {
                break;
            }
        }
        // Inbox hygiene on teardown (t-048): a finished worker must not leave an
        // empty notice-inbox entry behind in the notice store.
        //
        // * Empty inboxes only — `reclaim_worker_inbox` never discards a queued
        //   notice, so a steer posted for this key that was never drained stays
        //   pending and a replacement worker registered under the same key (a
        //   retry of the same task) remains steerable.
        // * No lock ordering hazard: the notice store is a separate map from
        //   `WORKERS`, and this runs after every `WORKERS` guard above has been
        //   released (the completed-ring loop collects keys into a `Vec` before
        //   removing, so no iterator guard is alive here). The call takes no
        //   guard at all, so there is nothing to deadlock against.
        // * Infallible by construction: the reclaim reports counts instead of
        //   `Result`s and unwraps nothing, so this drop path cannot panic —
        //   a panic inside `Drop` while another panic is unwinding would abort
        //   the process.
        super::notice::reclaim_worker_inbox(&self.0);
    }
}

/// Register a subagent worker as active with start timestamp and task prompt.
pub fn register_active_worker(
    task_id: Option<String>,
    agent_name: String,
    prompt: String,
) -> ActiveWorkerGuard {
    register_active_worker_with_token(task_id, agent_name, prompt, None)
}

/// Register a subagent worker as active with start timestamp, task prompt, and optional cancellation token.
///
/// The returned [`ActiveWorkerGuard`] carries the **effective** registry key:
/// when the natural key `{agent}-{task}` is already owned by a live worker, a
/// distinct disambiguated key (`{agent}-{task}#{id}`) is allocated instead of
/// overwriting it (recon H5), and that new key is what the guard reports — so
/// every caller keeps updating and cancelling *its own* entry.
pub fn register_active_worker_with_token(
    task_id: Option<String>,
    agent_name: String,
    prompt: String,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
) -> ActiveWorkerGuard {
    let clean_task_id = task_id
        .as_deref()
        .and_then(crate::task_id::normalize_task_id);

    // Correlation key: agent name + normalized task id (unchanged public
    // format). Workers *without* a task id get the monotonic id from
    // `WORKER_ID_SEQ` instead of the old nanosecond pseudo-id, which collided
    // for anything registered inside the same clock tick.
    let worker_id = next_worker_id();
    let base_key = if let Some(ref t) = clean_task_id {
        format!("{agent_name}-{t}")
    } else {
        format!("{agent_name}-w{worker_id}")
    };

    let effective_token = cancel_token
        .or_else(|| Some(crate::orchestrator::bus::global_cancellation_token().child_token()));

    let started_at = Instant::now();
    let key = claim_worker_slot(
        &base_key,
        clean_task_id,
        agent_name,
        prompt,
        effective_token,
        started_at,
    );
    ActiveWorkerGuard(key)
}

/// Build the fresh registry state written by [`claim_worker_slot`].
fn new_worker_state(
    task_id: Option<String>,
    agent_name: &str,
    prompt: &str,
    cancel_token: &Option<tokio_util::sync::CancellationToken>,
    started_at: Instant,
    initial_tokens: usize,
) -> WorkerState {
    WorkerState {
        info: ActiveWorkerInfo {
            task_id,
            agent_name: agent_name.to_string(),
            prompt: prompt.to_string(),
            started_at,
            started_wall: chrono::Local::now(),
            context_tokens: initial_tokens,
            implementation_turns: 0,
            validation_rounds: 0,
            latest_validator_feedback: None,
            status: "In Progress".to_string(),
            cancel_token: cancel_token.clone(),
        },
        last_tokens: initial_tokens,
        completed_seq: None,
        completed_at: None,
        completed_wall: None,
    }
}

/// Claim a registry slot for a new worker, **never overwriting a live entry**.
///
/// * Key free → insert under the natural key.
/// * Key held only by a *completed* entry → the natural key is reused and the
///   entry replaced, preserving the last-seen token count (legacy behavior).
/// * Key held by a **live** entry → a distinct disambiguated key
///   `{base}#{id}` is allocated and a `warn!` is emitted. Overwriting would
///   drop the live worker's `CancellationToken` on the floor: the running
///   worker stays active but is no longer reachable from
///   [`cancel_active_worker`] / [`cancel_all_active_workers`], and the first
///   guard's `drop` would then flip the *borrowed* entry to completed, hiding
///   the still-running worker from `has_active_workers()` too.
///
/// Returns the effective key, which the caller must use for all later updates.
fn claim_worker_slot(
    base_key: &str,
    task_id: Option<String>,
    agent_name: String,
    prompt: String,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    started_at: Instant,
) -> String {
    let mut key = base_key.to_string();
    let mut retries: usize = 0;

    loop {
        match WORKERS.entry(key.clone()) {
            Entry::Occupied(mut occupied) => {
                if occupied.get().is_active() {
                    // Live entry: refuse the silent overwrite and take a
                    // distinct id instead. `occupied` is released before the
                    // next lookup so at most one map guard is held at a time.
                    retries += 1;
                    let disambiguated = if retries <= MAX_KEY_DISAMBIGUATION_RETRIES {
                        format!("{base_key}{KEY_DISAMBIGUATOR}{}", next_worker_id())
                    } else {
                        // Each retry draws a fresh monotonic id, so reaching
                        // this bound means a caller-supplied task id literally
                        // spells out the suffix; a UUID cannot collide.
                        format!("{base_key}{KEY_DISAMBIGUATOR}u{}", uuid::Uuid::new_v4())
                    };
                    tracing::warn!(
                        "worker registry key '{key}' is held by a live worker; allocating '{disambiguated}' instead of clobbering its cancellation token"
                    );
                    drop(occupied);
                    key = disambiguated;
                    continue;
                }

                // Stale (already-completed) entry: reuse the key and inherit
                // its last-seen token count, as before.
                let initial_tokens = occupied.get().last_tokens;
                occupied.insert(new_worker_state(
                    task_id.clone(),
                    &agent_name,
                    &prompt,
                    &cancel_token,
                    started_at,
                    initial_tokens,
                ));
                return key;
            }
            Entry::Vacant(vacant) => {
                vacant.insert(new_worker_state(
                    task_id.clone(),
                    &agent_name,
                    &prompt,
                    &cancel_token,
                    started_at,
                    0,
                ));
                return key;
            }
        }
    }
}

/// Update the active specialist worker's context token count.
///
/// Uses a single `entry` transaction so a worker that registered between the
/// lookup and the write can never have its live entry replaced by the
/// token-count placeholder (which would drop its cancellation token).
pub fn update_active_worker_context(key: &str, tokens: usize) {
    match WORKERS.entry(key.to_string()) {
        Entry::Occupied(mut occupied) => {
            let entry = occupied.get_mut();
            entry.last_tokens = tokens;
            if entry.is_active() {
                entry.info.context_tokens = tokens;
            }
        }
        Entry::Vacant(vacant) => {
            // Preserve the legacy behavior of persisting the token count for keys
            // with no live entry (e.g. updated after completion or eviction).
            vacant.insert(WorkerState {
                info: ActiveWorkerInfo {
                    task_id: None,
                    agent_name: key.to_string(),
                    prompt: String::new(),
                    started_at: Instant::now(),
                    started_wall: chrono::Local::now(),
                    context_tokens: tokens,
                    implementation_turns: 0,
                    validation_rounds: 0,
                    latest_validator_feedback: None,
                    status: "Completed".to_string(),
                    cancel_token: None,
                },
                last_tokens: tokens,
                completed_seq: Some(COMPLETED_SEQ.fetch_add(1, Ordering::SeqCst)),
                completed_at: Some(Instant::now()),
                completed_wall: Some(chrono::Local::now()),
            });
        }
    }
}

/// Update the active specialist worker's implementation turn, validation rounds, and latest validator critique/feedback.
pub fn update_active_worker_progress(
    key: &str,
    turns: usize,
    val_rounds: usize,
    feedback: Option<String>,
) {
    if let Some(mut entry) = WORKERS.get_mut(key)
        && entry.is_active()
    {
        entry.info.implementation_turns = turns;
        entry.info.validation_rounds = val_rounds;
        if let Some(fb) = feedback {
            entry.info.latest_validator_feedback = Some(fb);
        }
    }
}

/// Set the descriptive status of an active specialist worker (e.g. "Approved", "Revising", "Failed", "Aborted").
pub fn set_active_worker_status(key: &str, status: &str) {
    if let Some(mut entry) = WORKERS.get_mut(key)
        && entry.is_active()
    {
        entry.info.status = status.to_string();
    }
}

/// Get the context token count for an active specialist worker by its key (e.g. `coder-t-001`).
pub fn get_active_worker_tokens(key: &str) -> Option<usize> {
    WORKERS.get(key).map(|e| e.last_tokens)
}

/// Format the active specialist context tokens for display in the status bar.
/// Returns None if no specialist workers are active or context is 0.
pub fn get_active_specialist_context_str() -> Option<String> {
    let mut active: Vec<(String, WorkerState)> = WORKERS
        .iter()
        .filter(|e| e.value().is_active())
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    active.sort_by(|a, b| a.0.cmp(&b.0));
    if active.is_empty() {
        return None;
    }
    let entries: Vec<String> = active
        .iter()
        .filter(|(_, w)| w.info.context_tokens > 0)
        .map(|(_, w)| {
            let count_str = if w.info.context_tokens >= 1_000_000 {
                format!("{:.1}M", w.info.context_tokens as f64 / 1_000_000.0)
            } else if w.info.context_tokens >= 1_000 {
                format!("{:.1}k", w.info.context_tokens as f64 / 1_000.0)
            } else {
                format!("{}", w.info.context_tokens)
            };
            if let Some(ref tid) = w.info.task_id {
                format!("{}-{}: {}", w.info.agent_name, tid, count_str)
            } else {
                format!("{}: {}", w.info.agent_name, count_str)
            }
        })
        .collect();

    if entries.is_empty() {
        None
    } else {
        Some(entries.join(", "))
    }
}

/// Returns true if there are currently any active background specialist workers.
pub fn has_active_workers() -> bool {
    WORKERS.iter().any(|e| e.value().is_active())
}

/// Helper to format elapsed durations into human-readable minutes and seconds (e.g. "2m 15s" or "45s").
pub fn format_duration_human(secs: u64) -> String {
    let mins = secs / 60;
    let rem_secs = secs % 60;
    if mins == 0 {
        format!("{rem_secs}s")
    } else {
        format!("{mins}m {rem_secs}s")
    }
}

/// Prompt file of a registered worker, gated on the canonical task-id grammar.
///
/// Gate t-055 (single grammar authority): this lookup turns a worker's task id —
/// originally LLM output from `delegate_task` / a plan checkbox — into the file
/// name of `.marmel/prompts/<task_id>.md`. The join goes through
/// [`crate::task_id::validate_task_id`], which is the only place in the crate
/// that decides what a task id may be; nothing here re-implements or extends
/// that grammar.
///
/// Contract, mirroring [`crate::harness::workspace::Workspace::prompt_path_for_task`]:
/// * the id is normalized first (decoration stripping only, unchanged order) and
///   the **normalized** value is what is validated and joined — byte-for-byte,
///   never sanitized, trimmed, clamped or mended into a different file name;
/// * `Ok(None)` means "there is no task id to look up";
/// * `Err(TaskIdError)` means the id was refused, and the caller must then skip
///   the read entirely (fail closed) rather than trying a repaired id.
fn worker_prompt_path(
    raw_task_id: Option<&str>,
) -> Result<Option<std::path::PathBuf>, crate::task_id::TaskIdError> {
    let Some(raw) = raw_task_id else {
        return Ok(None);
    };
    let clean = crate::task_id::normalize_task_id_ref(raw);
    let id = crate::task_id::validate_task_id(clean)?;
    Ok(Some(
        crate::harness::get_workspace_root()
            .join(crate::manager::phase::MARMEL_DIR)
            .join("prompts")
            .join(format!("{id}.md")),
    ))
}

/// Formats all currently active and recently completed subagent workers with their tool call ID, prompt, running time,
/// implementation turns, validation rounds, and latest validator feedback.
pub fn get_active_subtasks_str() -> String {
    // Active workers, in key order (matches the previous BTreeMap ordering).
    let mut active: Vec<(String, WorkerState)> = WORKERS
        .iter()
        .filter(|e| e.value().is_active())
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    active.sort_by(|a, b| a.0.cmp(&b.0));

    // Recently completed workers, most recent first (matches the previous
    // Vec push-order rendered in reverse).
    let mut completed: Vec<(String, WorkerState)> = WORKERS
        .iter()
        .filter(|e| !e.value().is_active())
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    completed.sort_by(|a, b| {
        b.1.completed_seq
            .unwrap_or(u64::MAX)
            .cmp(&a.1.completed_seq.unwrap_or(u64::MAX))
    });

    if active.is_empty() && completed.is_empty() {
        return "None".to_string();
    }

    let mut out = String::new();
    if !active.is_empty() {
        out.push_str("Active Background Subagents:\n");
        for (id, state) in &active {
            let info = &state.info;
            let elapsed_secs = info.started_at.elapsed().as_secs();
            let duration_str = format_duration_human(elapsed_secs);
            let task_id_str = info.task_id.as_deref().unwrap_or(id);
            let start_wall_str = info.started_wall.format("%H:%M:%S");
            out.push_str(&format!(
                "- Tool Call ID: {}\n  Subagent Tag: {}\n  Subagent: {}\n  Status: {}\n  Task Prompt: {}\n  Started At: {} (running for {}, {elapsed_secs} total seconds)\n  Implementation Turns: {}\n  Validation Rounds: {}\n",
                task_id_str, id, info.agent_name, info.status, info.prompt, start_wall_str,
                duration_str, info.implementation_turns, info.validation_rounds
            ));
            // Gate t-055: the per-worker prompt read is grammar-gated (see
            // [`worker_prompt_path`]). A rejected task id never reaches the read,
            // so the worker is rendered exactly as it already is when its prompt
            // file is missing — the "prompt unavailable" shape — and the id is
            // never repaired into some other file name.
            let prompt_file = match worker_prompt_path(info.task_id.as_deref()) {
                Ok(path) => path,
                Err(err) => {
                    tracing::warn!(
                        "Rejected task id {:?} for active worker {id}: {err}. \
                         Its synthesized prompt is treated as unavailable, so no \
                         read of .marmel/prompts/ is attempted and JIT prompt \
                         synthesis stays the fallback.",
                        info.task_id
                    );
                    None
                }
            };
            if let Some(prompt_file) = prompt_file
                && let Ok(bp) = crate::agents::AgentBlueprint::load_from_disk(&prompt_file)
            {
                out.push_str(&format!(
                    "  Assigned Role: {}\n  Allowed Tools: {}\n  Assigned Skills: {}\n",
                    bp.role_name,
                    bp.allowed_tools.join(", "),
                    if bp.selected_skills.is_empty() {
                        "None".to_string()
                    } else {
                        bp.selected_skills.join(", ")
                    }
                ));
            }
            if let Some(ref fb) = info.latest_validator_feedback {
                let summary = crate::text_util::truncate_with_ellipsis(fb.trim(), 300);
                out.push_str(&format!(
                    "  Latest Validator Feedback: \"{}\"\n",
                    summary.replace('\n', " ")
                ));
            } else {
                out.push_str("  Latest Validator Feedback: None\n");
            }
            out.push('\n');
        }
    }

    if !completed.is_empty() {
        out.push_str("Recently Completed Subagents:\n");
        for (_id, state) in &completed {
            let info = &state.info;
            let duration_str = format_duration_human(
                state
                    .completed_at
                    .map(|c| c - info.started_at)
                    .unwrap_or_default()
                    .as_secs(),
            );
            let task_id_str = info.task_id.as_deref().unwrap_or(&info.agent_name);
            let start_wall_str = info.started_wall.format("%H:%M:%S");
            let finish_wall_str = state
                .completed_wall
                .unwrap_or(info.started_wall)
                .format("%H:%M:%S");
            out.push_str(&format!(
                "- Tool Call ID: {}\n  Subagent: {}\n  Status: {}\n  Task Prompt: {}\n  Started At: {}\n  Finished At: {} (total duration: {})\n  Implementation Turns: {}\n  Validation Rounds: {}\n",
                task_id_str, info.agent_name, info.status, info.prompt, start_wall_str, finish_wall_str, duration_str, info.implementation_turns, info.validation_rounds
            ));
            if let Some(ref fb) = info.latest_validator_feedback {
                let summary = crate::text_util::truncate_with_ellipsis(fb.trim(), 300);
                out.push_str(&format!(
                    "  Latest Validator Feedback: \"{}\"\n",
                    summary.replace('\n', " ")
                ));
            } else {
                out.push_str("  Latest Validator Feedback: None\n");
            }
            out.push('\n');
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Exact worker identity (t-065) — the notice router's model, reused verbatim.
// ---------------------------------------------------------------------------

/// The **exact routing identity** of a registered worker.
///
/// This is not a second identity model: it delegates to the single authority
/// used for notice routing,
/// [`crate::orchestrator::notice::worker_routing_identity`] (effective registry
/// key vs. natural `{agent}-{task}` key, the `#…` collision handle stripped,
/// the task id taken from the canonical task-id grammar via
/// [`crate::plan_parse::is_task_id_token`]), and then pins the two
/// **authoritative** fields off the registry entry itself.
///
/// Those fields are what the key was *built from* at registration time
/// ([`register_active_worker_with_token`] joins `agent_name` + the normalized
/// `task_id`), so preferring them over a re-parse of the composite key is
/// strictly more precise — an agent name whose own tail is task-shaped (e.g.
/// `reviewer-t-x` running `t-001`, key `reviewer-t-x-t-001`) would otherwise be
/// split into the wrong agent/task pair. A field the entry does not carry falls
/// back to the value split out of the key, so task-less workers (`{agent}-w{n}`)
/// keep resolving to their bare agent name.
fn worker_identity(
    key: &str,
    info: &ActiveWorkerInfo,
) -> crate::orchestrator::notice::WorkerRoutingIdentity {
    let mut identity = crate::orchestrator::notice::worker_routing_identity(key);

    let authoritative_agent = info.agent_name.trim();
    if !authoritative_agent.is_empty() {
        identity.agent_name = authoritative_agent.to_ascii_lowercase();
    }

    let authoritative_task = info
        .task_id
        .as_deref()
        .map(|raw| crate::task_id::normalize_task_id_ref(raw).to_ascii_lowercase())
        .filter(|clean| !clean.is_empty());
    identity.task_id = authoritative_task.or(identity.task_id);

    identity
}

/// Helper to find the active worker addressed **exactly** by `task_id`.
///
/// t-065: this used to be a substring hunt — the address was tested with
/// `contains` against the registry key *and* against the worker's free-text
/// prompt — so asking for `t-1` returned the worker running `t-10` (and any
/// worker whose prompt merely happened to mention `t-1`). It now asks the shared
/// identity model whether the address names this worker exactly
/// ([`worker_identity`] + [`crate::orchestrator::notice::WorkerRoutingIdentity::routes`]):
/// the exact task id (case- and decoration-tolerant through the canonical
/// normalizer), the exact natural key, the exact effective key (including a
/// `#…` collision handle), the exact agent name, or a deliberate broadcast
/// address (`*` / `worker`).
///
/// An empty or decoration-only address matches **nothing**; under the old
/// `key.contains(&tid)` rule an empty id matched *every* active worker.
pub fn get_active_subtask_by_id(task_id: &str) -> Option<(String, String)> {
    let address = crate::task_id::normalize_task_id_ref(task_id).to_ascii_lowercase();
    if address.is_empty() {
        return None;
    }

    let mut active: Vec<(String, WorkerState)> = WORKERS
        .iter()
        .filter(|e| e.value().is_active())
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    active.sort_by(|a, b| a.0.cmp(&b.0));
    let (_key, state) = active
        .iter()
        .find(|(key, state)| worker_identity(key, &state.info).routes(&address))?;
    let running_time = format_duration_human(state.info.started_at.elapsed().as_secs());
    Some((state.info.agent_name.clone(), running_time))
}

/// Does the caller's `(target_agent, target_task)` pair address this worker
/// **exactly**?
///
/// Both arguments are routed through the notice router's whole-field equality
/// rules ([`crate::orchestrator::notice::WorkerRoutingIdentity::routes`]), so a
/// cancel can still be aimed by agent name (`coder`), by role family
/// (`validator` → `validator-coder`), by natural/effective key
/// (`coder-t-001`, `coder-t-001#7`) or as an explicit broadcast (`*`,
/// `worker`) — while a task id or key must match as a whole token. A
/// `t-1` cancel no longer kills `t-10`, and a `coder-t-0` cancel no longer
/// kills `coder-t-001`: every earlier `contains` arm (key substring, task-id
/// substring in either direction, agent-name substring in either direction) is
/// deliberately absent, and no caller in this crate relies on them — the steer
/// arbitrator is instructed to name the target by its `tool_call_id` (e.g.
/// `t-001`) or its exact `agent_name` (see `prompts/steer_arbitrator.md`), and
/// [`get_active_subtask_by_id`] is fed task ids parsed by `plan_parse`. There is
/// therefore no legacy "unique substring match" path left behind.
///
/// Argument composition is unchanged: an absent (`None`/empty/decoration-only)
/// argument is a wildcard, and when both are present **both** must address the
/// same worker.
fn worker_matches(
    info: &ActiveWorkerInfo,
    key: &str,
    target_agent: Option<&str>,
    target_task: Option<&str>,
) -> bool {
    let identity = worker_identity(key, info);

    let clean_agent =
        crate::task_id::normalize_task_id_ref(target_agent.unwrap_or("")).to_ascii_lowercase();
    let clean_task =
        crate::task_id::normalize_task_id_ref(target_task.unwrap_or("")).to_ascii_lowercase();

    if clean_agent.is_empty() && clean_task.is_empty() {
        return false;
    }

    let agent_matches = clean_agent.is_empty() || identity.routes(&clean_agent);
    let task_matches = clean_task.is_empty() || identity.routes(&clean_task);

    agent_matches && task_matches
}

/// Cancel an active specialist worker matching target_agent and/or target_task_id.
/// Returns true if at least one matching active worker was found and cancelled.
///
/// Targeting is **exact identity matching** (t-065), the same rules notice
/// routing uses — see [`worker_matches`]. A `t-1` target cancels the worker
/// running `t-1` and never the one running `t-10`; a bare agent name, a role
/// family, a whole natural/effective key, or an explicit broadcast (`*` /
/// `worker`) still address their workers as before.
pub fn cancel_active_worker(target_agent: Option<&str>, target_task_id: Option<&str>) -> bool {
    let mut to_cancel = Vec::new();
    for e in WORKERS.iter() {
        let key = e.key();
        let state = e.value();
        if state.is_active() && worker_matches(&state.info, key, target_agent, target_task_id) {
            to_cancel.push((key.clone(), state.info.cancel_token.clone()));
        }
    }

    let found = !to_cancel.is_empty();
    for (key, token_opt) in to_cancel {
        if let Some(token) = token_opt {
            token.cancel();
        }
        set_active_worker_status(&key, "Aborted");
        crate::orchestrator::emit_status(format!(
            "[Steering] Cancelled active specialist worker '{key}'"
        ));
    }
    found
}

/// Cancel all active specialist workers across the registry.
pub fn cancel_all_active_workers() -> usize {
    let mut to_cancel = Vec::new();
    for e in WORKERS.iter() {
        let key = e.key();
        let state = e.value();
        if state.is_active() {
            to_cancel.push((key.clone(), state.info.cancel_token.clone()));
        }
    }

    let count = to_cancel.len();
    for (key, token_opt) in to_cancel {
        if let Some(token) = token_opt {
            token.cancel();
        }
        set_active_worker_status(&key, "Aborted");
        crate::orchestrator::emit_status(format!(
            "[Steering] Aborted active specialist worker '{key}'"
        ));
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_progress_and_completed_history() {
        let guard = register_active_worker(
            Some("task-test-1".to_string()),
            "coder".to_string(),
            "Write parser tests".to_string(),
        );

        update_active_worker_progress(&guard.0, 3, 1, Some("Missing edge case".to_string()));
        set_active_worker_status(&guard.0, "Revising");

        let status_str = get_active_subtasks_str();
        assert!(status_str.contains("Active Background Subagents:"));
        assert!(status_str.contains("Implementation Turns: 3"));
        assert!(status_str.contains("Validation Rounds: 1"));
        assert!(status_str.contains("Missing edge case"));
        assert!(status_str.contains("Status: Revising"));
        assert!(status_str.contains("Started At:"));

        set_active_worker_status(&guard.0, "Approved");
        update_active_worker_progress(&guard.0, 4, 2, Some("All checks passed".to_string()));
        drop(guard);

        let completed_str = get_active_subtasks_str();
        assert!(completed_str.contains("Recently Completed Subagents:"));
        assert!(completed_str.contains("Implementation Turns: 4"));
        assert!(completed_str.contains("Validation Rounds: 2"));
        assert!(completed_str.contains("Status: Approved"));
        assert!(completed_str.contains("All checks passed"));
        assert!(completed_str.contains("Started At:"));
        assert!(completed_str.contains("Finished At:"));
    }

    #[test]
    fn test_cancel_active_worker_and_cancel_all() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let token1 = tokio_util::sync::CancellationToken::new();
        let token2 = tokio_util::sync::CancellationToken::new();

        let guard1 = register_active_worker_with_token(
            Some("t-001".to_string()),
            "coder".to_string(),
            "Writing parser".to_string(),
            Some(token1.clone()),
        );

        let guard2 = register_active_worker_with_token(
            Some("t-002".to_string()),
            "researcher".to_string(),
            "Investigating bug".to_string(),
            Some(token2.clone()),
        );

        assert!(!token1.is_cancelled());
        assert!(!token2.is_cancelled());

        // Cancel specific worker by task_id
        let cancelled = cancel_active_worker(None, Some("t-001"));
        assert!(cancelled);
        assert!(token1.is_cancelled());
        assert!(!token2.is_cancelled());

        // Cancel remaining workers with cancel_all_active_workers
        let count = cancel_all_active_workers();
        assert!(count >= 1);
        assert!(token2.is_cancelled());

        drop(guard1);
        drop(guard2);
    }

    #[test]
    fn test_format_workers_summary_utf8_char_boundary_no_panic() {
        let prefix = "e".repeat(296);
        let fb = format!(
            "{prefix}—feedback text that exceeds 300 characters easily and has em-dash right at the cut boundary"
        );
        let guard = register_active_worker(
            Some("task-utf8".to_string()),
            "coder".to_string(),
            "UTF8 test".to_string(),
        );
        update_active_worker_progress(&guard.0, 1, 1, Some(fb));

        let status_str = get_active_subtasks_str();
        assert!(status_str.contains("Latest Validator Feedback:"));
        assert!(status_str.contains("..."));

        drop(guard);
        let completed_str = get_active_subtasks_str();
        assert!(completed_str.contains("Latest Validator Feedback:"));
        assert!(completed_str.contains("..."));
    }

    /// Concurrency regression (Phase 2): 50 concurrent workers hammer the
    /// single sharded `WORKERS` DashMap — each registers via
    /// `register_active_worker_with_token`, interleaves
    /// `update_active_worker_context` + `set_active_worker_status`, then drops
    /// its guard (single-entry mutation + bounded-ring eviction). The whole
    /// batch must finish well within a generous wall-clock bound (no
    /// deadlock), and the completed ring must stay capped at 10 entries.
    #[tokio::test]
    async fn test_workers_stress_concurrent_register_update_drop() {
        let n = 50;
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            handles.push(tokio::spawn(async move {
                let guard = register_active_worker_with_token(
                    Some(format!("stress-{i}")),
                    format!("agent-{i}"),
                    format!("stress prompt {i}"),
                    None,
                );
                // Interleave context-token updates and status changes on the
                // same entry while other workers churn the map concurrently.
                for j in 0..5 {
                    update_active_worker_context(&guard.0, 100 * (j + 1));
                    set_active_worker_status(&guard.0, &format!("status-{j}"));
                }
                drop(guard);
            }));
        }
        // Generous bound: a correct implementation finishes in <1s; a
        // deadlock would hang until this fires.
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            for h in handles {
                h.await.expect("worker task panicked");
            }
        })
        .await
        .expect("50 concurrent worker register/update/drop tasks must complete (no deadlock)");

        // Every one of OUR 50 workers must be completed (not active) after its
        // guard dropped. (We check our own keys, not `has_active_workers()`,
        // because other lib tests share the process-global map concurrently.)
        let my_active = (0..n)
            .filter(|i| {
                WORKERS
                    .get(&format!("agent-{i}-stress-{i}"))
                    .is_some_and(|e| e.is_active())
            })
            .count();
        assert_eq!(
            my_active, 0,
            "all 50 stress workers must have completed after their guards dropped"
        );
        // Bounded completed ring: the cap is a hard invariant of the drop
        // path. Other lib tests churn the shared global map concurrently, so
        // a *momentary* count of 11 can be observed mid-drop; poll until the
        // ring settles at or below the cap (it must, given every drop evicts).
        let mut settled = false;
        for _ in 0..100 {
            let completed_count = WORKERS.iter().filter(|e| !e.value().is_active()).count();
            if completed_count <= MAX_RECENT_COMPLETED {
                settled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            settled,
            "completed ring must settle at <= {MAX_RECENT_COMPLETED} entries"
        );
    }

    // ------------------------------------------------------------------
    // Recon H5 — unique worker ids + registry entries are never clobbered.
    // ------------------------------------------------------------------

    /// Live (active) registry entries whose key starts with `prefix`.
    fn active_entries_with_prefix(prefix: &str) -> Vec<String> {
        WORKERS
            .iter()
            .filter(|e| e.value().is_active() && e.key().starts_with(prefix))
            .map(|e| e.key().clone())
            .collect()
    }

    /// Extract the monotonic worker id from a task-less worker key
    /// (`{agent}-w{n}`). Panics if the key does not use that scheme, which is
    /// what pins the id source (a clock-derived key fails here).
    fn taskless_worker_id(key: &str, agent: &str) -> u64 {
        let rest = key.strip_prefix(&format!("{agent}-w")).unwrap_or_else(|| {
            panic!("task-less worker key {key:?} must use the monotonic `-w<id>` id scheme")
        });
        rest.parse::<u64>()
            .unwrap_or_else(|e| panic!("worker id in {key:?} is not a number: {e}"))
    }

    /// (i) Two workers registered back-to-back with **no task id** — the exact
    /// shape that produced identical keys pre-fix, because the pseudo-id was
    /// `Instant::now().elapsed().as_nanos()` (`workers.rs:165` pre-fix), a
    /// near-constant reading (measured 202/33/41/39/40). They must get distinct,
    /// monotonic ids and BOTH must be reachable by the cancel path.
    #[test]
    fn test_same_tick_taskless_workers_get_distinct_ids_and_are_both_cancellable() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let agent = "h5tick";
        let token1 = tokio_util::sync::CancellationToken::new();
        let token2 = tokio_util::sync::CancellationToken::new();

        let g1 = register_active_worker_with_token(
            None,
            agent.to_string(),
            "first brief".to_string(),
            Some(token1.clone()),
        );
        let g2 = register_active_worker_with_token(
            None,
            agent.to_string(),
            "second brief".to_string(),
            Some(token2.clone()),
        );

        assert_ne!(
            g1.0, g2.0,
            "two task-less workers of the same agent registered back-to-back must not share a key"
        );
        let id1 = taskless_worker_id(&g1.0, agent);
        let id2 = taskless_worker_id(&g2.0, agent);
        assert!(
            id2 > id1,
            "worker ids must come from a monotonic counter: {id1} -> {id2}"
        );

        let live = active_entries_with_prefix(agent);
        assert!(
            live.contains(&g1.0) && live.contains(&g2.0),
            "both workers must be live in the registry, live={live:?}"
        );

        let cancelled = cancel_all_active_workers();
        assert!(
            cancelled >= 2,
            "cancel_all_active_workers must reach both workers, got {cancelled}"
        );
        assert!(
            token1.is_cancelled() && token2.is_cancelled(),
            "both workers' cancellation tokens must be reachable"
        );

        drop(g1);
        drop(g2);
    }

    /// (i/ii) Same agent + same task id ⇒ identical natural key (recon Scenario
    /// A: overlapping steer rounds both yield `steer-task-1`). The second
    /// registration must not clobber the first worker's live entry: the entry at
    /// the original key must still hold the first worker's state and token, and
    /// `cancel_all_active_workers` must reach both.
    #[test]
    fn test_colliding_registration_does_not_drop_first_workers_token() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let agent = "h5clobber";
        let token_a = tokio_util::sync::CancellationToken::new();
        let token_b = tokio_util::sync::CancellationToken::new();

        let ga = register_active_worker_with_token(
            Some("t-001".to_string()),
            agent.to_string(),
            "first brief".to_string(),
            Some(token_a.clone()),
        );
        let gb = register_active_worker_with_token(
            Some("t-001".to_string()),
            agent.to_string(),
            "second brief".to_string(),
            Some(token_b.clone()),
        );

        assert_ne!(
            ga.0, gb.0,
            "a colliding registration must allocate a distinct id instead of reusing the live key"
        );

        // Every lookup is scoped so at most one map guard is held at a time.
        let first = {
            let e = WORKERS
                .get(&ga.0)
                .unwrap_or_else(|| panic!("first worker's entry {} is gone", ga.0));
            (
                e.info.prompt.clone(),
                e.info.task_id.clone(),
                e.is_active(),
                e.info.cancel_token.as_ref().map(|t| t.is_cancelled()),
            )
        };
        let second = {
            let e = WORKERS
                .get(&gb.0)
                .unwrap_or_else(|| panic!("second worker's entry {} is missing", gb.0));
            (e.info.prompt.clone(), e.info.task_id.clone(), e.is_active())
        };

        assert_eq!(
            first.0, "first brief",
            "the live entry at {} was clobbered by a colliding registration",
            ga.0
        );
        assert_eq!(first.1.as_deref(), Some("t-001"));
        assert!(
            first.2,
            "the first worker must still be live in the registry"
        );
        assert_eq!(first.3, Some(false), "the first worker keeps its own token");
        assert_eq!(second.0, "second brief");
        assert!(second.2, "the second worker must be live under its own key");
        assert_eq!(
            second.1.as_deref(),
            Some("t-001"),
            "the disambiguated worker keeps the correlation key so task-id targeting still finds it"
        );

        // Cancelling the first worker marks *its* entry only — proof that the
        // stored tokens are two distinct tokens, not one shared/clobbered one.
        token_a.cancel();
        let a_cancelled = WORKERS.get(&ga.0).is_some_and(|e| {
            e.info
                .cancel_token
                .as_ref()
                .is_some_and(|t| t.is_cancelled())
        });
        let b_cancelled = WORKERS.get(&gb.0).is_some_and(|e| {
            e.info
                .cancel_token
                .as_ref()
                .is_some_and(|t| t.is_cancelled())
        });
        assert!(a_cancelled, "first worker's token must be its own");
        assert!(
            !b_cancelled,
            "the second worker's token must not be aliased to the first worker's"
        );

        let cancelled = cancel_all_active_workers();
        assert!(
            cancelled >= 2,
            "cancel_all_active_workers must reach both colliding workers, got {cancelled}"
        );
        assert!(
            token_b.is_cancelled(),
            "the second worker's cancellation token must be reachable"
        );

        drop(ga);
        drop(gb);
    }

    /// (ii) Full Scenario-A repro: after the collision, the *first* guard's
    /// `drop` pre-fix flipped the shared entry to completed, so the still
    /// running second worker disappeared from `has_active_workers()` and from
    /// every cancel path (they all filter on `is_active()`).
    #[test]
    fn test_first_guard_drop_cannot_hide_second_live_worker() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let agent = "h5drop";
        let token_a = tokio_util::sync::CancellationToken::new();
        let token_b = tokio_util::sync::CancellationToken::new();

        let ga = register_active_worker_with_token(
            Some("t-001".to_string()),
            agent.to_string(),
            "steer round A".to_string(),
            Some(token_a.clone()),
        );
        let gb = register_active_worker_with_token(
            Some("t-001".to_string()),
            agent.to_string(),
            "steer round B".to_string(),
            Some(token_b.clone()),
        );

        // Round A finishes first while round B is still running.
        drop(ga);

        let b_live = WORKERS.get(&gb.0).is_some_and(|e| e.is_active());
        assert!(
            b_live,
            "worker {} must stay live/registered after an unrelated guard completed",
            gb.0
        );

        let cancelled = cancel_active_worker(Some(agent), Some("t-001"));
        assert!(
            cancelled,
            "the still-running worker must be reachable by an agent/task targeted cancel"
        );
        assert!(
            token_b.is_cancelled(),
            "the still-running worker's cancellation token must still be reachable"
        );

        drop(gb);
    }

    /// (ii) Hardening of the drop path: dropping a stale guard for an entry that
    /// is **already completed** must be a no-op — it must not bump the completed
    /// ring sequence, re-stamp timestamps, or touch the re-registered live
    /// worker that inherited the key. A guard drop that flipped somebody else's
    /// live entry to completed is precisely what makes a running worker
    /// un-killable, since `cancel_active_worker` and `cancel_all_active_workers`
    /// both skip non-active entries.
    #[test]
    fn test_stale_guard_drop_is_noop_and_key_reuse_keeps_worker_cancellable() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let agent = "h5stale";
        let token1 = tokio_util::sync::CancellationToken::new();
        let token2 = tokio_util::sync::CancellationToken::new();

        let g1 = register_active_worker_with_token(
            Some("t-009".to_string()),
            agent.to_string(),
            "first run".to_string(),
            Some(token1.clone()),
        );
        let key = g1.0.clone();
        update_active_worker_context(&key, 1_234);
        drop(g1); // entry flipped to completed, key retained by the ring

        let before = WORKERS
            .get(&key)
            .map(|e| (e.completed_seq, e.completed_at, e.last_tokens))
            .expect("completed entry must still be registered right after its guard dropped");

        let stale = ActiveWorkerGuard(key.clone());
        drop(stale);

        let after = WORKERS
            .get(&key)
            .map(|e| (e.completed_seq, e.completed_at, e.last_tokens))
            .expect("stale guard drop must not evict the entry");
        assert_eq!(
            after, before,
            "dropping a stale guard for a completed entry must be a no-op"
        );

        // The natural key is reused by the next registration for the same
        // agent+task, and the new worker must be live, cancellable, and keep the
        // last-seen context token count.
        let g2 = register_active_worker_with_token(
            Some("t-009".to_string()),
            agent.to_string(),
            "second run".to_string(),
            Some(token2.clone()),
        );
        assert_eq!(g2.0, key, "a completed key is expected to be reused");
        assert_eq!(
            get_active_worker_tokens(&key),
            Some(1_234),
            "re-registering a completed key must inherit its last-seen token count"
        );
        assert!(
            WORKERS.get(&g2.0).is_some_and(|e| e.is_active()),
            "the re-registered worker must be live"
        );
        assert!(
            WORKERS
                .get(&g2.0)
                .is_some_and(|e| e.info.prompt == "second run"),
            "the re-registered worker owns the entry (no state left over from the finished worker)"
        );

        let cancelled = cancel_all_active_workers();
        assert!(cancelled >= 1, "the live worker must be cancellable");
        assert!(
            token2.is_cancelled(),
            "the live worker's token must survive a stale guard drop"
        );
        assert!(
            !token1.is_cancelled(),
            "the already-finished worker's token must not be cancelled again"
        );

        drop(g2);
    }

    /// (ii) Companion guard: `update_active_worker_context` must never replace a
    /// live entry either. Pre-fix it did `get_mut(...).unwrap_or_else(insert)`,
    /// so a registration landing between the lookup and the insert was clobbered
    /// by the token-count placeholder — again losing the cancellation token.
    #[test]
    fn test_update_active_worker_context_never_replaces_a_live_entry() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let agent = "h5tok";
        let token = tokio_util::sync::CancellationToken::new();

        let guard = register_active_worker_with_token(
            Some("t-050".to_string()),
            agent.to_string(),
            "brief with a token".to_string(),
            Some(token.clone()),
        );

        update_active_worker_context(&guard.0, 4_321);

        let state = WORKERS
            .get(&guard.0)
            .expect("the entry must still exist after a token-count update");
        assert!(state.is_active(), "the live entry must survive the update");
        assert_eq!(state.info.prompt, "brief with a token");
        assert_eq!(state.last_tokens, 4_321);
        assert!(
            state.info.cancel_token.is_some(),
            "the live worker's cancellation token must not be dropped by an update"
        );
        drop(state);

        let cancelled = cancel_all_active_workers();
        assert!(cancelled >= 1, "the worker must still be cancellable");
        assert!(token.is_cancelled());

        // Legacy behavior kept for keys with no live entry (evicted/completed):
        // the token count is still persisted.
        update_active_worker_context("h5phantom-t-777", 42);
        assert_eq!(get_active_worker_tokens("h5phantom-t-777"), Some(42));

        drop(guard);
    }

    /// (ii) Race guard for the same clobber class: pre-fix
    /// `update_active_worker_context` did `get_mut(key)` → on miss
    /// `insert(placeholder)`, so a registration landing between the lookup and
    /// the insert had its live entry (and cancellation token) replaced by the
    /// token-less placeholder. The update now runs as one `entry` transaction.
    #[test]
    fn test_concurrent_update_and_registration_never_clobbers_live_entry() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        const WORKERS_PER_ROUND: usize = 8;
        const ROUNDS: usize = 40;

        for round in 0..ROUNDS {
            // Fresh agent+task per round, so the updaters really do take the
            // "entry absent" path and the registrations all collide on the same
            // natural key.
            let agent = format!("h5race-{round}");
            let task = format!("t-race-{round}");

            std::thread::scope(|scope| {
                // A barrier lines every thread up at the entrance of the two
                // registry calls so their internal lookup/write windows overlap.
                let barrier = std::sync::Arc::new(std::sync::Barrier::new(2 * WORKERS_PER_ROUND));
                let mut updater_handles = Vec::new();
                for _ in 0..WORKERS_PER_ROUND {
                    let key = format!("{agent}-{task}");
                    let barrier = std::sync::Arc::clone(&barrier);
                    updater_handles.push(scope.spawn(move || {
                        barrier.wait();
                        update_active_worker_context(&key, 7);
                    }));
                }

                let mut registrant_handles = Vec::new();
                for i in 0..WORKERS_PER_ROUND {
                    let agent = agent.clone();
                    let task = task.clone();
                    let barrier = std::sync::Arc::clone(&barrier);
                    registrant_handles.push(scope.spawn(move || {
                        barrier.wait();
                        let token = tokio_util::sync::CancellationToken::new();
                        let guard = register_active_worker_with_token(
                            Some(task),
                            agent,
                            format!("brief-{i}"),
                            Some(token.clone()),
                        );
                        (guard, token, format!("brief-{i}"))
                    }));
                }

                let live: Vec<_> = registrant_handles
                    .into_iter()
                    .map(|h| h.join().expect("registrant thread must not panic"))
                    .collect();
                for h in updater_handles {
                    h.join().expect("updater thread must not panic");
                }

                // Every registered worker still owns a live entry carrying its
                // own brief and its own cancellation token.
                let mut keys = Vec::new();
                for (guard, token, brief) in &live {
                    let intact = WORKERS.get(&guard.0).is_some_and(|e| {
                        e.is_active()
                            && &e.info.prompt == brief
                            && e.info.cancel_token.as_ref().is_some_and(|t| {
                                !t.is_cancelled()
                                    && e.info.task_id.as_deref() == Some(task.as_str())
                            })
                    });
                    assert!(
                        intact,
                        "round {round}: worker entry {} was clobbered (lost brief {} or its cancellation token)",
                        guard.0, brief
                    );
                    assert!(!token.is_cancelled());
                    keys.push(guard.0.clone());
                }
                // All 8 registrations of the same agent+task got distinct keys.
                keys.sort();
                keys.dedup();
                assert_eq!(
                    keys.len(),
                    WORKERS_PER_ROUND,
                    "round {round}: concurrent registrations of one agent+task must not share keys"
                );

                // And every one of them is reachable by the cancel path.
                let cancelled = cancel_active_worker(Some(agent.as_str()), Some(task.as_str()));
                assert!(
                    cancelled,
                    "round {round}: the targeted cancel must find the registered workers"
                );
                assert!(
                    live.iter().all(|(_, token, _)| token.is_cancelled()),
                    "round {round}: every live worker's cancellation token must stay reachable"
                );

                for (guard, _, _) in live {
                    drop(guard);
                }
            });
        }
    }

    /// (iii) Completion leaves no stale entry: nothing stays *active* after its
    /// guard dropped, targeted cancels stop seeing it, and the bounded completed
    /// ring evicts the oldest completions outright.
    #[test]
    fn test_completion_leaves_no_stale_registry_entry() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let agent = "h5evict";
        let total = MAX_RECENT_COMPLETED + 3;

        let mut guards = Vec::new();
        let mut keys = Vec::new();
        for i in 0..total {
            let guard = register_active_worker_with_token(
                Some(format!("t-evict-{i}")),
                agent.to_string(),
                format!("eviction brief {i}"),
                None,
            );
            keys.push(guard.0.clone());
            guards.push(guard);
        }
        assert_eq!(
            active_entries_with_prefix(agent).len(),
            total,
            "every registration must own a live entry"
        );

        // Guards drop in registration order.
        for guard in guards {
            drop(guard);
        }

        // (1) No stale *live* entry survives completion …
        let still_live = active_entries_with_prefix(agent);
        assert!(
            still_live.is_empty(),
            "completed workers must not stay live in the registry: {still_live:?}"
        );
        // … so targeted cancels no longer match them.
        assert!(
            !cancel_active_worker(Some(agent), None),
            "completed workers must not be reported as cancel targets"
        );

        // (2) The completed ring is capped, so the oldest of *our* entries are
        // removed from the map entirely (eviction order is oldest completion
        // first, and ours are the most recent completions overall).
        for key in keys.iter().take(total - MAX_RECENT_COMPLETED) {
            assert!(
                WORKERS.get(key).is_none(),
                "stale completed entry {key} was never evicted from the registry"
            );
            assert_eq!(
                get_active_worker_tokens(key),
                None,
                "evicted entry {key} must not linger in lookups"
            );
        }

        // (3) Whatever survived is completed, never active, and the cap holds.
        let present = keys.iter().filter(|k| WORKERS.get(*k).is_some()).count();
        assert!(
            present <= MAX_RECENT_COMPLETED,
            "completed ring must stay capped at {MAX_RECENT_COMPLETED}, {present} of our entries survived"
        );
        let global_completed = WORKERS.iter().filter(|e| !e.value().is_active()).count();
        assert!(
            global_completed <= MAX_RECENT_COMPLETED,
            "registry must not accumulate completed entries beyond the cap ({global_completed})"
        );
    }

    // ---------------------------------------------------------------------
    // Teardown inbox reclaim (t-048)
    // ---------------------------------------------------------------------

    /// `ActiveWorkerGuard::drop` reclaims the finished worker's **empty** notice
    /// inbox. "Empty but present" is the real leak shape: a resolved notice drops
    /// its queued copy and leaves the map entry behind, so a dead worker's key
    /// would otherwise linger in the notice store until the idle window.
    #[tokio::test]
    async fn test_guard_drop_reclaims_finished_workers_empty_notice_inbox() {
        let _notice_lock = crate::orchestrator::notice::TEST_NOTICE_MUTEX.lock().await;

        let guard = register_active_worker_with_token(
            Some("t-inbox-reclaim".to_string()),
            "inboxreclaim".to_string(),
            "Reclaim the empty inbox".to_string(),
            None,
        );
        let key = guard.0.clone();

        let posted = crate::orchestrator::post_notice_to_worker(&key, "please wrap up", None);
        // The reply path drops the queued copy of the notice it resolves, which
        // leaves exactly the entry under test: an inbox holding nothing.
        crate::orchestrator::record_worker_reply_for_notice(&key, &posted.notice_id, "wrapping up")
            .expect("a reply naming its own notice id from its own worker is accepted");
        assert!(
            crate::orchestrator::notice::has_worker_inbox(&key),
            "a resolved notice must leave an empty inbox entry behind for the teardown path to reclaim"
        );

        drop(guard);

        assert!(
            !crate::orchestrator::notice::has_worker_inbox(&key),
            "a finished worker must not leave an empty inbox entry in the notice store"
        );
    }

    /// The teardown reclaim is restricted to **empty** inboxes: a notice that was
    /// never drained is never discarded when the worker goes away, so a
    /// replacement worker under the same key (a retry of the same task) stays
    /// steerable and the notice stays pending.
    #[tokio::test]
    async fn test_guard_drop_never_discards_a_queued_notice() {
        let _notice_lock = crate::orchestrator::notice::TEST_NOTICE_MUTEX.lock().await;

        let guard = register_active_worker_with_token(
            Some("t-inbox-retained".to_string()),
            "inboxretained".to_string(),
            "Leave the queued notice alone".to_string(),
            None,
        );
        let key = guard.0.clone();
        let posted = crate::orchestrator::post_notice_to_worker(&key, "still queued", None);

        drop(guard);

        assert_eq!(
            crate::orchestrator::notice::worker_inbox_len(&key),
            1,
            "teardown must not discard an undelivered notice"
        );
        assert!(
            crate::orchestrator::get_pending_notice(&posted.notice_id).is_some(),
            "a notice that was never delivered must stay pending after the worker is gone"
        );
    }

    /// Reclaim routing is exact-identity like every other notice operation: the
    /// teardown of one worker never touches another worker's inbox — including a
    /// worker whose key shares the leading role segment.
    #[tokio::test]
    async fn test_guard_drop_reclaim_does_not_touch_other_worker_inboxes() {
        let _notice_lock = crate::orchestrator::notice::TEST_NOTICE_MUTEX.lock().await;

        let guard = register_active_worker_with_token(
            Some("t-inbox-peer".to_string()),
            "inboxpeer".to_string(),
            "Reclaim only my own inbox".to_string(),
            None,
        );
        let peer_key = "inboxpeer-t-inbox-peer";
        let neighbour_key = "inboxpeer-t-inbox-neighbour";
        crate::orchestrator::post_notice_to_worker(neighbour_key, "not yours", None);
        // Drain the departing worker's own address space first, so only the
        // neighbour's entry could possibly be affected by the teardown reclaim.
        crate::orchestrator::drain_worker_notices(peer_key);
        assert_eq!(guard.0, peer_key, "the guard must own the drained key");

        drop(guard);

        assert_eq!(
            crate::orchestrator::notice::worker_inbox_len(neighbour_key),
            1,
            "another worker's queued notice must survive this worker's teardown"
        );
    }

    /// The drop path runs the reclaim for every worker without panicking and
    /// without deadlocking on the notice store: 24 workers register, post and
    /// drain notices, and drop concurrently on real threads. Every teardown must
    /// report back inside the timeout, and every *undrained* notice must still be
    /// queued afterwards.
    #[tokio::test]
    async fn test_guard_drop_reclaim_does_not_panic_or_deadlock_under_concurrent_teardown() {
        let _notice_lock = crate::orchestrator::notice::TEST_NOTICE_MUTEX.lock().await;

        let (tx, rx) = std::sync::mpsc::channel::<(u8, String)>();
        for i in 0..24u8 {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let task_id = format!("t-teardown-{i}");
                let guard = register_active_worker_with_token(
                    Some(task_id),
                    "teardownagent".to_string(),
                    "concurrent teardown".to_string(),
                    None,
                );
                let key = guard.0.clone();
                crate::orchestrator::post_notice_to_worker(&key, "steer", None);
                if i % 2 == 0 {
                    // Drained inboxes are empty at teardown → reclaimed.
                    crate::orchestrator::drain_worker_notices(&key);
                }
                drop(guard);
                let _ = tx.send((i, key));
            });
        }
        drop(tx);

        let mut keys: Vec<(u8, String)> = Vec::new();
        for _ in 0..24 {
            let pair = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("worker teardown must not deadlock on the notice store");
            keys.push(pair);
        }

        for (i, key) in &keys {
            if i % 2 == 0 {
                assert_eq!(
                    crate::orchestrator::notice::worker_inbox_len(key),
                    0,
                    "drained inbox {key} must be reclaimed at teardown"
                );
            } else {
                assert_eq!(
                    crate::orchestrator::notice::worker_inbox_len(key),
                    1,
                    "queued notice for {key} must survive teardown"
                );
            }
        }
    }

    // -----------------------------------------------------------------
    // Gate t-055 — the per-worker `.marmel/prompts/<task_id>.md` READ site.
    //
    // `worker_prompt_path` is the only join for this read and it routes the
    // already-normalized id through `crate::task_id::validate_task_id`, the
    // single grammar authority in the crate. The call site fails closed: a
    // rejected id is never read, never mended into a different file name, and
    // renders exactly like a missing prompt file (JIT synthesis stays the
    // fallback), with a WARN naming the rejected id and the reason.
    // -----------------------------------------------------------------

    /// A task id with accepted grammar but above `MAX_TASK_ID_LEN`.
    fn overlong_worker_task_id() -> String {
        format!("t-{}", "x".repeat(70))
    }

    /// The rendered summary section belonging to one worker key.
    ///
    /// `get_active_subtasks_str()` renders every worker in the process, so the
    /// assertions below are scoped to the worker this test registered.
    fn section_for_worker(rendered: &str, key: &str) -> String {
        let marker = format!("  Subagent Tag: {key}\n");
        let at = rendered
            .find(&marker)
            .unwrap_or_else(|| panic!("worker {key} is missing from the summary:\n{rendered}"));
        let rest = &rendered[at + marker.len()..];
        let tag_end = rest.find("  Subagent Tag: ").unwrap_or(rest.len());
        let block_end = rest[..tag_end].find("- Tool Call ID:").unwrap_or(tag_end);
        rest[..block_end].to_string()
    }

    /// WARN capture counting only the rejections emitted by this gate.
    #[derive(Clone, Default)]
    struct RejectTaskIdWarns(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    struct WarnMessageVisitor<'a> {
        found: &'a mut Option<String>,
    }

    impl tracing::field::Visit for WarnMessageVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.found = Some(format!("{value:?}"));
            }
        }
    }

    impl tracing::Subscriber for RejectTaskIdWarns {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.level() == &tracing::Level::WARN
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _record: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().level() != &tracing::Level::WARN {
                return;
            }
            let mut found = None;
            event.record(&mut WarnMessageVisitor { found: &mut found });
            if let Some(message) = found
                && message.contains("Rejected task id")
            {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// Rejected ids must be typed refusals (never a path), and a worker carrying
    /// one must render exactly like a worker whose prompt file is missing — the
    /// blueprint planted *outside* the prompts dir (where the un-gated
    /// `prompts/../escape.md` join used to land) must never be read.
    #[test]
    fn test_worker_prompt_path_rejects_hostile_task_ids_and_skips_the_read() {
        // A plain `#[test]` with its own current-thread runtime: the registry
        // mutex is a std mutex and must be held for the whole probe without ever
        // crossing an `await` point.
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("ws_worker_prompt_hostile");
        let marmel = root.join(crate::manager::phase::MARMEL_DIR);
        let prompts = marmel.join("prompts");
        std::fs::create_dir_all(&prompts).expect("prompts dir");
        let planted = marmel.join("escape.md");
        std::fs::write(
            &planted,
            "---\nrole_name: \"planted_outside_prompts\"\n---\n\nPlanted blueprint outside the prompts directory.\n",
        )
        .expect("plant blueprint");

        let long = overlong_worker_task_id();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime to scope the workspace root");
        rt.block_on(crate::harness::with_workspace_root(&root, async {
            // 1. The gate itself: a typed refusal per hostile spelling, and no
            //    path is ever handed back to the caller.
            assert_eq!(
                worker_prompt_path(Some("../escape")).err(),
                Some(crate::task_id::TaskIdError::DotDotSegment)
            );
            assert_eq!(
                worker_prompt_path(Some("a/b")).err(),
                Some(crate::task_id::TaskIdError::PathSeparator { ch: '/' })
            );
            assert_eq!(
                worker_prompt_path(Some("..")).err(),
                Some(crate::task_id::TaskIdError::DotDotSegment)
            );
            assert_eq!(
                worker_prompt_path(Some("")).err(),
                Some(crate::task_id::TaskIdError::Empty)
            );
            assert_eq!(
                worker_prompt_path(Some(long.as_str())).err(),
                Some(crate::task_id::TaskIdError::TooLong {
                    len: long.chars().count(),
                    max: crate::task_id::MAX_TASK_ID_LEN,
                })
            );
            assert!(
                worker_prompt_path(None)
                    .expect("no task id is not a rejection")
                    .is_none(),
                "a worker without a task id has nothing to look up"
            );

            // 2. The call site: registration with a hostile id is still a live
            //    worker, but no prompt detail is rendered and the rejection is
            //    logged loudly.
            let guard = register_active_worker(
                Some("../escape".to_string()),
                "hostileagent".to_string(),
                "Read a prompt file outside the prompts dir".to_string(),
            );
            let key = guard.0.clone();
            let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            tracing::subscriber::with_default(RejectTaskIdWarns(counter.clone()), || {
                let section = section_for_worker(&get_active_subtasks_str(), &key);
                assert!(
                    section.contains("Subagent: hostileagent"),
                    "the worker itself must still be rendered: {section}"
                );
                assert!(
                    !section.contains("Assigned Role:"),
                    "a rejected task id must never read a prompt file: {section}"
                );
                assert!(
                    !section.contains("planted_outside_prompts"),
                    "the blueprint outside the prompts dir must never surface: {section}"
                );
            });
            assert!(
                counter.load(std::sync::atomic::Ordering::Relaxed) >= 1,
                "the rejection must be reported with a tracing::warn! naming the id"
            );
            drop(guard);
        }));

        assert!(
            planted.exists(),
            "the planted file outside the prompts dir must be untouched"
        );
        let stray: Vec<_> = std::fs::read_dir(&prompts)
            .expect("prompts dir still exists")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(
            stray.is_empty(),
            "no file may be created in the prompts dir by a rejected id: {stray:?}"
        );
    }

    /// A normal `t-0NN` id — decorated or not — still normalizes, validates and
    /// round-trips through the prompts directory, so the gate costs nothing on
    /// the happy path.
    #[test]
    fn test_worker_prompt_path_round_trips_a_normal_task_id() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("ws_worker_prompt_roundtrip");
        let prompts = root.join(crate::manager::phase::MARMEL_DIR).join("prompts");
        let stored = crate::agents::AgentBlueprint {
            role_name: "coder_specialist".to_string(),
            reasoning: "round trip".to_string(),
            selected_skills: vec!["testing".to_string()],
            allowed_tools: vec![crate::tool_names::TOOL_READ_FILE.to_string()],
            system_prompt: "Round-trip the t-055 prompt.".to_string(),
            task_id: Some("t-055".to_string()),
        };
        let written = stored
            .save_to_disk(&prompts)
            .expect("a normal task id must persist its prompt");
        assert_eq!(written, prompts.join("t-055.md"));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime to scope the workspace root");
        rt.block_on(crate::harness::with_workspace_root(&root, async {
            let canonical_prompts = prompts.canonicalize().expect("canonical prompts dir");
            let path = worker_prompt_path(Some("[t-055]"))
                .expect("a normal task id is accepted by the grammar authority")
                .expect("a path is built for an accepted id");
            assert_eq!(
                path,
                canonical_prompts.join("t-055.md"),
                "the decorated id resolves to one file name inside the prompts dir"
            );

            let guard = register_active_worker(
                Some("[t-055]".to_string()),
                "coder".to_string(),
                "Implement t-055 and run the tests".to_string(),
            );
            let key = guard.0.clone();
            let section = section_for_worker(&get_active_subtasks_str(), &key);
            assert!(
                section.contains("Assigned Role: coder_specialist"),
                "an accepted id still reads its prompt file: {section}"
            );
            assert!(
                section.contains(&format!(
                    "Allowed Tools: {}",
                    crate::tool_names::TOOL_READ_FILE
                )),
                "the prompt detail must round-trip: {section}"
            );
            drop(guard);
        }));
    }

    // ------------------------------------------------------------------
    // t-065 — cancel and diagnostic targeting address a worker by its exact
    // routing identity, never by a substring of a key, a task id or a prompt.
    // Fixtures mirror the `worker_routing_identity` / `routes()` identity tests
    // in `notice_tests`.
    // ------------------------------------------------------------------

    /// An [`ActiveWorkerInfo`] that never touches the global registry, so the
    /// matcher itself can be asserted directly.
    fn probe_info(agent: &str, task: Option<&str>) -> ActiveWorkerInfo {
        ActiveWorkerInfo {
            task_id: task.map(str::to_string),
            agent_name: agent.to_string(),
            prompt: String::new(),
            started_at: Instant::now(),
            started_wall: chrono::Local::now(),
            context_tokens: 0,
            implementation_turns: 0,
            validation_rounds: 0,
            latest_validator_feedback: None,
            status: "In Progress".to_string(),
            cancel_token: None,
        }
    }

    /// The identity fields behind a registry key are the notice router's fields,
    /// pinned to the authoritative values of the entry.
    #[test]
    fn test_worker_identity_reuses_the_notice_routing_identity_model() {
        let info = probe_info("validator-coder", Some("t-001"));
        let identity = worker_identity("validator-coder-t-001#7", &info);
        assert_eq!(identity.effective_key, "validator-coder-t-001#7");
        assert_eq!(identity.natural_key, "validator-coder-t-001");
        assert_eq!(identity.agent_name, "validator-coder");
        assert_eq!(identity.task_id.as_deref(), Some("t-001"));

        // The task-shaped tail of an *agent name* must not be re-split into a
        // task id: the authoritative fields win over re-parsing the key.
        let tricky = probe_info("reviewer-t-x", Some("t-001"));
        let tricky_identity = worker_identity("reviewer-t-x-t-001", &tricky);
        assert_eq!(tricky_identity.agent_name, "reviewer-t-x");
        assert_eq!(tricky_identity.task_id.as_deref(), Some("t-001"));
        assert!(worker_matches(
            &tricky,
            "reviewer-t-x-t-001",
            Some("reviewer-t-x"),
            None
        ));
        assert!(worker_matches(
            &tricky,
            "reviewer-t-x-t-001",
            Some("reviewer"),
            None
        ));
        assert!(!worker_matches(
            &tricky,
            "reviewer-t-x-t-001",
            Some("t-x"),
            None
        ));
        assert!(!worker_matches(
            &tricky,
            "reviewer-t-x-t-001",
            Some("reviewer-t-x-t-00"),
            None
        ));
        assert!(worker_matches(
            &tricky,
            "reviewer-t-x-t-001",
            Some("reviewer"),
            Some("t-001")
        ));

        // A task-less worker resolves to its bare agent name.
        let taskless = probe_info("t065taskless", None);
        let taskless_identity = worker_identity("t065taskless-w9", &taskless);
        assert_eq!(taskless_identity.agent_name, "t065taskless");
        assert_eq!(taskless_identity.task_id, None);

        // Exact addresses route; every fragment does not.
        let key = "validator-coder-t-001#7";
        for exact in [
            "t-001",
            "[t-001]",
            "T-001",
            "validator-coder-t-001",
            "validator-coder-t-001#7",
            "validator-coder",
            "validator",
        ] {
            assert!(
                worker_matches(&info, key, Some(exact), None)
                    || worker_matches(&info, key, None, Some(exact)),
                "`{exact}` addresses this worker exactly"
            );
        }
        for fragment in [
            "t-00",
            "t-0010",
            "coder",
            "coder-t-001",
            "validator-coder-t-00",
            "validator-coder-t-001#9",
            "coder-t-001#7",
            "oder",
        ] {
            assert!(
                !worker_matches(&info, key, Some(fragment), None)
                    && !worker_matches(&info, key, None, Some(fragment)),
                "`{fragment}` is only a fragment of the identity and must not match"
            );
        }

        // Absent targets are wildcards, but never a match-all; both present
        // must hold (composition unchanged from the legacy AND behaviour).
        assert!(!worker_matches(&info, key, None, None));
        assert!(!worker_matches(&info, key, Some(""), Some("[]")));
        assert!(worker_matches(&info, key, Some("validator"), Some("t-001")));
        assert!(!worker_matches(
            &info,
            key,
            Some("researcher"),
            Some("t-001")
        ));
        assert!(!worker_matches(&info, key, Some("validator"), Some("t-00")));

        // The deliberate broadcast vocabulary of the router.
        assert!(worker_matches(&info, key, Some("worker"), None));
        assert!(worker_matches(&info, key, None, Some("*")));
    }

    /// (a) The `t-1` vs `t-10` pair on the CANCEL path.
    #[test]
    fn test_cancel_by_task_id_cancels_t1_and_never_t10() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let short_token = tokio_util::sync::CancellationToken::new();
        let long_token = tokio_util::sync::CancellationToken::new();

        let g_short = register_active_worker_with_token(
            Some("t-1".to_string()),
            "t065short".to_string(),
            "worker owning t-1".to_string(),
            Some(short_token.clone()),
        );
        let g_long = register_active_worker_with_token(
            Some("t-10".to_string()),
            "t065long".to_string(),
            "worker owning t-10".to_string(),
            Some(long_token.clone()),
        );

        // The fixture is exactly the trap: `t065long-t-10` contains `t-1`.
        assert_eq!(g_short.0, "t065short-t-1");
        assert_eq!(g_long.0, "t065long-t-10");
        assert!(
            g_long.0.contains("t-1"),
            "the legacy substring matcher had nothing to distinguish these keys"
        );
        assert!(!short_token.is_cancelled());
        assert!(!long_token.is_cancelled());

        assert!(
            cancel_active_worker(None, Some("t-1")),
            "the exact task id must cancel its own worker"
        );
        assert!(short_token.is_cancelled());
        assert!(
            !long_token.is_cancelled(),
            "a cancel aimed at `t-1` must never cancel the `t-10` worker"
        );

        // The sibling id still cancels its own worker.
        assert!(cancel_active_worker(None, Some("t-10")));
        assert!(long_token.is_cancelled());

        drop(g_short);
        drop(g_long);
    }

    /// (a, continued) A truncated task id or tag — the old `contains` arms —
    /// addresses nobody, while the decorated exact id still does.
    #[test]
    fn test_cancel_by_truncated_id_or_key_matches_nobody() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let token = tokio_util::sync::CancellationToken::new();
        let guard = register_active_worker_with_token(
            Some("t-001".to_string()),
            "t065trunc".to_string(),
            "worker owning t-001".to_string(),
            Some(token.clone()),
        );
        assert_eq!(guard.0, "t065trunc-t-001");

        for fragment in [
            "t065trunc-t-0",
            "t065trunc-t-00",
            "t065trunc-t-0010",
            "t-0",
            "t-00",
            "t-0010",
            "t065trun",
        ] {
            assert!(
                !cancel_active_worker(None, Some(fragment)),
                "`{fragment}` is a fragment of `{}` and must cancel nobody",
                guard.0
            );
            assert!(
                !cancel_active_worker(Some(fragment), None),
                "`{fragment}` is not an agent name either"
            );
            assert!(
                !token.is_cancelled(),
                "`{fragment}` must not cancel the worker"
            );
        }

        // Decoration/case tolerance is untouched.
        assert!(cancel_active_worker(None, Some("[T-001]")));
        assert!(token.is_cancelled());

        drop(guard);
    }

    /// (b) The same pair on the diagnostic lookup `get_active_subtask_by_id`.
    #[test]
    fn test_get_active_subtask_by_id_is_exact_not_substring() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let g_short = register_active_worker(
            Some("t-1".to_string()),
            "t065lookshort".to_string(),
            "implement the short id".to_string(),
        );
        let g_long = register_active_worker(
            Some("t-10".to_string()),
            "t065looklong".to_string(),
            "implement t-10 — its prompt also mentions t-1 in prose".to_string(),
        );
        let g_taskless = register_active_worker(
            None,
            "t065looktaskless".to_string(),
            "a task-less worker whose prompt mentions t-7777".to_string(),
        );

        let agent_of = |id: &str| get_active_subtask_by_id(id).map(|(agent, _)| agent);
        assert_eq!(agent_of("t-1").as_deref(), Some("t065lookshort"));
        assert_eq!(
            agent_of("t-10").as_deref(),
            Some("t065looklong"),
            "the longer id must resolve to its own worker"
        );
        assert_eq!(
            agent_of("[T-1]").as_deref(),
            Some("t065lookshort"),
            "task-id decoration tolerance is unchanged"
        );
        assert_eq!(
            agent_of("t065lookshort-t-1").as_deref(),
            Some("t065lookshort"),
            "the whole natural key still resolves"
        );

        // The legacy prompt-substring fallback is gone.
        assert!(
            get_active_subtask_by_id("t-7777").is_none(),
            "a task id may not be looked up through a worker's free-text prompt"
        );
        // Fragments of a live key resolve to nobody.
        assert!(get_active_subtask_by_id("t065lookshort-t-").is_none());
        assert!(get_active_subtask_by_id("t065lookshort-t-10").is_none());
        // Empty / decoration-only ids used to match *every* active worker
        // (`key.contains("")` is always true).
        assert!(get_active_subtask_by_id("").is_none());
        assert!(get_active_subtask_by_id("[]").is_none());

        drop(g_short);
        drop(g_long);
        drop(g_taskless);
    }

    /// (d) The legitimate cancel forms: exact agent name, role family, whole
    /// natural/effective key, and the deliberate broadcast.
    #[test]
    fn test_cancel_by_name_role_family_key_and_broadcast_still_work() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let coder_token = tokio_util::sync::CancellationToken::new();
        let validator_token = tokio_util::sync::CancellationToken::new();
        let dup_a_token = tokio_util::sync::CancellationToken::new();
        let dup_b_token = tokio_util::sync::CancellationToken::new();
        let broadcast_token = tokio_util::sync::CancellationToken::new();

        let g_coder = register_active_worker_with_token(
            Some("t-501".to_string()),
            "t065coder".to_string(),
            "cancel me by name".to_string(),
            Some(coder_token.clone()),
        );
        let g_validator = register_active_worker_with_token(
            Some("t-502".to_string()),
            "t065validator-coder".to_string(),
            "cancel me by role family".to_string(),
            Some(validator_token.clone()),
        );
        // Same agent + same task id twice ⇒ the second one gets the
        // collision-disambiguated key `{base}#{id}`.
        let g_dup_a = register_active_worker_with_token(
            Some("t-505".to_string()),
            "t065dup".to_string(),
            "natural key holder".to_string(),
            Some(dup_a_token.clone()),
        );
        let g_dup_b = register_active_worker_with_token(
            Some("t-505".to_string()),
            "t065dup".to_string(),
            "disambiguated twin".to_string(),
            Some(dup_b_token.clone()),
        );
        let g_broadcast = register_active_worker_with_token(
            Some("t-506".to_string()),
            "t065broadcast".to_string(),
            "broadcast target".to_string(),
            Some(broadcast_token.clone()),
        );

        // Exact agent name.
        assert!(cancel_active_worker(Some("t065coder"), None));
        assert!(coder_token.is_cancelled());
        assert!(!validator_token.is_cancelled());

        // Role family: a leading segment of a hyphenated role name.
        assert!(cancel_active_worker(Some("t065validator"), None));
        assert!(validator_token.is_cancelled());

        // A role-name segment that is not a *leading* segment of the worker's
        // role must not reach it (the legacy `contains` arm did).
        let substring_token = tokio_util::sync::CancellationToken::new();
        let g_substring = register_active_worker_with_token(
            Some("t-503".to_string()),
            "t065substring-family".to_string(),
            "a substring role target must not cancel me".to_string(),
            Some(substring_token.clone()),
        );
        assert!(
            !cancel_active_worker(Some("family"), None),
            "`family` is only a trailing segment of `t065substring-family`"
        );
        assert!(
            !substring_token.is_cancelled(),
            "a fragment of a role name must never fire a cancellation"
        );
        // The leading segment does reach it (role family).
        assert!(cancel_active_worker(Some("t065substring"), None));
        assert!(substring_token.is_cancelled());

        // Whole effective key reaches only the disambiguated twin.
        assert!(g_dup_b.0.starts_with("t065dup-t-505#"));
        assert_ne!(g_dup_a.0, g_dup_b.0);
        assert!(cancel_active_worker(None, Some(g_dup_b.0.as_str())));
        assert!(
            dup_b_token.is_cancelled(),
            "the exact effective key must reach its own worker"
        );
        assert!(
            !dup_a_token.is_cancelled(),
            "a sibling sharing the natural key must not be reached by a `#handle` address"
        );

        // The natural key addresses both workers holding it (documented
        // broadcast-by-natural-key behaviour of the router).
        assert!(cancel_active_worker(None, Some("t065dup-t-505")));
        assert!(dup_a_token.is_cancelled());

        // Deliberate broadcast through the router's broadcast vocabulary.
        assert!(cancel_active_worker(None, Some("worker")));
        assert!(
            broadcast_token.is_cancelled(),
            "an explicit broadcast cancel must still work"
        );

        drop(g_coder);
        drop(g_validator);
        drop(g_substring);
        drop(g_dup_a);
        drop(g_dup_b);
        drop(g_broadcast);
    }
}
