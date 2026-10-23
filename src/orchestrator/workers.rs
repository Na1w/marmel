//! Active specialist workers registry, context token tracking, and RAII guards.
//!
//! All worker state lives in a single sharded [`DashMap`] (`WORKERS`). A worker
//! key survives completion: on drop the entry is flipped to a "completed"
//! phase in place (preserving the last-seen context token count), and a
//! bounded 10-entry completed cap evicts the oldest completed entry. No
//! function in this module ever holds more than one map guard at a time.
//!
//! Two registry invariants:
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
    /// Monotonic completion sequence (None while active); used for ordering the
    /// recently-completed list and evicting the oldest completed entry.
    pub completed_seq: Option<u64>,
    pub completed_at: Option<Instant>,
    pub completed_wall: Option<chrono::DateTime<chrono::Local>>,
    /// The value this registration drew from [`WORKER_ID_SEQ`] (see
    /// [`next_worker_id`]). `0` marks a token-count placeholder entry, which is
    /// not a registration. Carried on the entry because the session capture
    /// path persists it: a restored worker is inserted under its stored key and
    /// never re-registers, so the id embedded in `{agent}-wN` / the `#N`
    /// collision handle must stay readable without re-drawing it.
    pub worker_id_seq_value: u64,
    /// Where this worker's **current backend call** stands in its pool key's FIFO,
    /// or `None` while it is not sitting in a queue wait.
    ///
    /// The durable half of the `Queued` transition (fix #3,
    /// `docs/diagnose_subpane_stale_refresh.md` §3). The delegation-event stream is
    /// one-shot — a `Queued` read by `events.drain(..)` before its panel row exists
    /// is gone, and the pool re-reports only when the rank actually moves — so the
    /// same position is mirrored here, on the structure the row projection rebuilds
    /// from. A viewer can then re-derive the `(Queued n/m)` label from the registry
    /// instead of only from an event that happened to arrive in the right order.
    ///
    /// Written by [`set_worker_queue_position`] /
    /// [`set_active_worker_queue_position`] from the pool's
    /// [`crate::orchestrator::pool::PositionReporter`] inside
    /// `llm::client::ChatClient::acquire_slot`, and taken back down when the slot is
    /// acquired, when the wait is abandoned, and when the entry closes. It is
    /// deliberately **not** persisted: a queue rank belongs to a wait inside one
    /// process's pool, and a restored worker gets a fresh one the moment it queues
    /// again.
    pub queue_position: Option<crate::orchestrator::pool::QueuePosition>,
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

/// Unique-id source for worker registry keys.
///
/// Every registration draws a strictly increasing id from this counter, so two
/// workers registered in the same tick can never share a key. It is also the
/// disambiguator used when a registration would otherwise overwrite a *live*
/// registry entry (see [`claim_worker_slot`]).
///
/// **The counter starts at 1, not 0:** id `0` is the reserved marker of "no id was
/// ever drawn", which is how a token-count placeholder written by
/// [`update_active_worker_context`] is recognised (see [`classify_call_class`]).
/// Starting at 0 would hand the very first registration of a process that marker
/// and make a real worker look like a stub to every class-aware consumer.
static WORKER_ID_SEQ: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(1));

/// Next monotonic worker id. Never reused within a process.
fn next_worker_id() -> u64 {
    WORKER_ID_SEQ.fetch_add(1, Ordering::SeqCst)
}

/// Separator marking a collision-resolved ("disambiguated") worker key, e.g.
/// `{agent}-{task}#7`. The base key `{agent}-{task}` is always a prefix of its
/// disambiguated form — which is **why** matching must never be prefix- or
/// substring-based: everything after `#` is an opaque collision handle, and the
/// identity behind it is recovered structurally by
/// [`crate::orchestrator::notice::worker_routing_identity`].
const KEY_DISAMBIGUATOR: char = '#';

/// Bound on key-disambiguation retries. Each retry draws a fresh id from
/// [`WORKER_ID_SEQ`], so a collision on a disambiguated key can only happen if
/// a caller-supplied task id literally spells out that suffix; past this bound
/// the key falls back to a UUID, which cannot collide.
const MAX_KEY_DISAMBIGUATION_RETRIES: usize = 16;

#[cfg(test)]
pub static TEST_WORKERS_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

#[cfg(test)]
pub static TEST_TRACING_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
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
        // can neither re-complete an entry nor complete a still-running
        // worker's entry — `has_active_workers()` and every cancel path filter
        // on `is_active()`.
        if let Some(mut entry) = WORKERS.get_mut(&self.0)
            && entry.is_active()
        {
            let seq = COMPLETED_SEQ.fetch_add(1, Ordering::SeqCst);
            entry.last_tokens = entry.info.context_tokens;
            entry.completed_seq = Some(seq);
            entry.completed_at = Some(Instant::now());
            entry.completed_wall = Some(chrono::Local::now());
            // A closed delegation owns no slot wait: whatever rank this worker had
            // in a FIFO died with the run, and a completed entry must never report
            // one to the read model (the panel's teardown clear keys on
            // `is_active`, but the registry itself must not hold the stale rank).
            entry.queue_position = None;
        }
        // Bounded completed ring: evict oldest completed entries while over cap.
        evict_completed_ring();
        // Inbox hygiene on teardown: a finished worker must not leave an empty
        // notice-inbox entry behind. Empty inboxes only — `reclaim_worker_inbox`
        // never discards a queued notice, so an undrained steer stays pending and
        // a replacement worker under the same key (a retry of the same task) stays
        // steerable. No lock ordering hazard: every `WORKERS` guard above is
        // released first and the reclaim takes no `WORKERS` guard. Infallible: it
        // reports counts and unwraps nothing, so this `Drop` cannot panic.
        super::notice::reclaim_worker_inbox(&self.0);
        // Freeze this worker's mirror so the next bundle commit writes a stable
        // `workers/<key>.json`, and record that an edge happened. Both are
        // infallible and touch **no disk**: `Drop` must never panic, and a
        // worker thread is never a writer — the session thread harvests the
        // registry at its next commit.
        if let Some(journal) = crate::session::worker_journal::registry_get(&self.0) {
            journal.close();
        }
        crate::ui::session::mark_worker_lifecycle_edge();
    }
}

/// Bounded completed ring: evict oldest-first completed entries past
/// [`MAX_RECENT_COMPLETED`], returning how many entries were removed.
///
/// Shared by [`ActiveWorkerGuard::drop`] and by the session restore installer,
/// so there is exactly one eviction policy. Keys are collected into a `Vec`
/// before any removal: no map guard is ever alive while another lookup happens
/// (the module's single-guard rule), so the restore path — where a panic would
/// abort the process — reuses it without a second implementation.
fn evict_completed_ring() -> usize {
    let mut removed_total = 0usize;
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
                removed_total += 1;
            }
        }
        if !removed_any {
            break;
        }
    }
    removed_total
}

/// Raise [`WORKER_ID_SEQ`] to a restore floor.
///
/// `fetch_max`, never an assignment: the floor says "every id below `floor` has
/// already been handed out", and the counter may legitimately be ahead of it (a
/// worker registered in this process before the restore ran). Returns the
/// counter value **before** the raise, so the caller can report the delta.
pub fn seed_worker_id_seq_floor(floor: u64) -> u64 {
    WORKER_ID_SEQ.fetch_max(floor, Ordering::SeqCst)
}

/// Current [`WORKER_ID_SEQ`] state: every id below it has been issued.
pub fn worker_id_seq_floor() -> u64 {
    WORKER_ID_SEQ.load(Ordering::SeqCst)
}

/// Raise [`COMPLETED_SEQ`] to a restore floor — see
/// [`seed_worker_id_seq_floor`] for the ordering rule.
pub fn seed_completed_seq_floor(floor: u64) -> u64 {
    COMPLETED_SEQ.fetch_max(floor, Ordering::SeqCst)
}

/// Current [`COMPLETED_SEQ`] state.
pub fn completed_seq_floor() -> u64 {
    COMPLETED_SEQ.load(Ordering::SeqCst)
}

/// Insert a **restored** worker under its stored registry key — the only registry
/// mutation the session loader is given: key and `completed_seq` are written
/// verbatim and no id is re-drawn. Deliberately **not**
/// [`register_active_worker_with_token`] / [`claim_worker_slot`]: those draw a
/// fresh `WORKER_ID_SEQ` id and may disambiguate the key, which is identity drift.
///
/// Returns `false` (inserting nothing) when a **live** entry holds the key, the
/// same refusal [`claim_worker_slot`] makes; the caller reports the collision.
pub fn install_restored_worker(worker_key: String, state: WorkerState) -> bool {
    match WORKERS.entry(worker_key) {
        Entry::Occupied(mut occupied) => {
            if occupied.get().is_active() {
                return false;
            }
            occupied.insert(state);
            true
        }
        Entry::Vacant(vacant) => {
            vacant.insert(state);
            true
        }
    }
}

/// Enforce the completed-ring cap over a set of freshly re-inserted entries and
/// report how many completed entries the live map could not hold: the files are
/// the durable record, the map is the working set.
pub fn enforce_completed_ring_cap() -> usize {
    evict_completed_ring()
}

/// Close a restored record without an [`ActiveWorkerGuard`] (the manager chose
/// to retire / re-delegate / continue, so the open delegation closes the way a
/// real completion closes it). Runs the existing teardown steps — draw
/// `COMPLETED_SEQ`, flip to completed, evict the ring, reclaim the notice
/// inbox — and returns the drawn sequence (`Some(existing)` when already
/// closed, `None` when the key is unknown). No guard is synthesized: its `Drop`
/// would report a completion the worker never had.
pub fn retire_restored_worker(worker_key: &str, status_label: &str) -> Option<u64> {
    let seq = {
        let mut entry = WORKERS.get_mut(worker_key)?;
        if !entry.is_active() {
            Some(entry.completed_seq.unwrap_or(0))
        } else {
            let seq = COMPLETED_SEQ.fetch_add(1, Ordering::SeqCst);
            entry.last_tokens = entry.info.context_tokens;
            entry.completed_seq = Some(seq);
            entry.completed_at = Some(Instant::now());
            entry.completed_wall = Some(chrono::Local::now());
            entry.info.status = status_label.to_string();
            // Same teardown as an [`ActiveWorkerGuard`] drop: a closed entry never
            // reports a slot rank.
            entry.queue_position = None;
            Some(seq)
        }
    };
    // Guards released above before any further lookup (single-guard rule).
    evict_completed_ring();
    super::notice::reclaim_worker_inbox(worker_key);
    seq
}

/// Clone one registry entry: the capture side projects a live `WorkerState` into
/// a `WorkerSection`, and `WORKERS` is private to this module.
pub fn worker_state_snapshot(worker_key: &str) -> Option<WorkerState> {
    WORKERS.get(worker_key).map(|entry| entry.value().clone())
}

/// Snapshot every entry as `(key, state)`, no guard outliving the call. The
/// committer walks this and joins it with [`crate::session::worker_journal`] to
/// build the `workers/*.json` members.
pub fn live_worker_states() -> Vec<(String, WorkerState)> {
    let mut out: Vec<(String, WorkerState)> = WORKERS
        .iter()
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Role-family marker of a validation pass. Validators register under
/// `validator-{agent}` / `validator-planner` (see `src/agents/validation.rs`), so
/// the **leading segment** of the agent name identifies the class — anchored at a
/// segment boundary exactly like the notice router's role-family rule
/// ([`crate::orchestrator::notice::WorkerRoutingIdentity::routes`]), never an
/// unanchored prefix match.
const VALIDATOR_ROLE_FAMILY: &str = "validator";

/// What kind of work a registry entry represents.
///
/// Derived from fields the registry already holds — the read model adds **no new
/// state**. These are the classes the table can actually distinguish today:
/// * [`WorkerCallClass::Specialist`] — a `delegate_task` run, whether dispatched
///   by the Manager, nested inside another specialist, or restored from a session
///   bundle.
/// * [`WorkerCallClass::ValidatorAudit`] — a validator / plan-audit pass, i.e. a
///   worker in the `validator` role family.
/// * [`WorkerCallClass::Placeholder`] — the token-count stub
///   [`update_active_worker_context`] leaves behind for a key with no live entry:
///   never a registration (`worker_id_seq_value == 0`), never active.
///
/// **Nesting depth is deliberately absent**: nothing in [`WorkerState`] records a
/// parent or a depth, so this surface refuses to invent one. A nested child is
/// reported as `Specialist` — correct membership, no fabricated label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerCallClass {
    Specialist,
    ValidatorAudit,
    Placeholder,
}

/// One row of the **public worker read model**: everything a consumer (the TUI
/// subagent pane, the arbitrator prompt surface) needs to render one worker,
/// projected out of the private `WORKERS` table so the pane can become a pure
/// projection of the registry instead of a parallel, event-derived list.
///
/// Every field is cloned out of the private [`WorkerState`]; the struct leaks no
/// private type (no `WorkerState`, no `DashMap` guard, no `CancellationToken`).
#[derive(Debug, Clone)]
pub struct WorkerEntry {
    /// The **effective** registry key — the [`ActiveWorkerGuard`] handle:
    /// `{agent}-{task}`, `{agent}-w{n}` or `{base}#{id}`. Consumers must key on
    /// this, never on a display name derived from the agent enum.
    pub key: String,
    /// The effective key with the `#…` collision handle removed.
    pub natural_key: String,
    /// The `#…` collision handle itself (`Some` only for a disambiguated key).
    pub collision_handle: Option<String>,
    /// Shorthand for `collision_handle.is_some()`: this registration had to take
    /// a distinct key because a live worker already owned the natural one.
    pub disambiguated: bool,
    pub agent_name: String,
    pub task_id: Option<String>,
    /// Specialist / validator-audit / token-placeholder (see [`WorkerCallClass`]).
    pub call_class: WorkerCallClass,
    /// The **liveness authority**: `completed_seq.is_none()`
    /// ([`WorkerState::is_active`]). A terminal *label* (`Aborted`, `Approved`,
    /// `Rejected`) never flips this, so a consumer reads this field and never has
    /// to infer liveness from `status_label`.
    pub is_active: bool,
    /// Descriptive status string as stored (mirrors the `Status:` line of
    /// [`get_active_subtasks_str`]).
    pub status_label: String,
    pub prompt: String,
    pub started_at: Instant,
    pub started_wall: chrono::DateTime<chrono::Local>,
    /// Seconds of work time: live elapsed while [`Self::is_active`], total run
    /// duration once completed — the same two numbers the active and completed
    /// sections of [`get_active_subtasks_str`] print.
    pub elapsed_secs: u64,
    /// Monotonic completion sequence (`None` while active). Callers that want the
    /// prompt's most-recent-first layout sort on this.
    pub completed_seq: Option<u64>,
    pub completed_wall: Option<chrono::DateTime<chrono::Local>>,
    /// Context tokens as tracked right now for an active worker.
    pub context_tokens: usize,
    /// Last-seen token count, preserved after completion — the value
    /// [`get_active_worker_tokens`] returns for this key.
    pub last_tokens: usize,
    pub implementation_turns: usize,
    pub validation_rounds: usize,
    pub latest_validator_feedback: Option<String>,
    /// Where this worker's current backend call stands in its pool key's FIFO
    /// (`position` = 1-based rank, `of` = queue depth including this caller), or
    /// `None` while it is not waiting for a slot.
    ///
    /// The registry is the **durable source of truth** for this state, which is what
    /// lets a row projection re-derive a `(Queued n/m)` label instead of keeping only
    /// what a one-shot delegation event happened to fold onto the row (fix #3,
    /// `docs/diagnose_subpane_stale_refresh.md` §3). Written on the pool's
    /// position-report edge by [`set_worker_queue_position`], and taken back down on
    /// slot acquisition, on an abandoned wait and when the entry closes — so a
    /// consumer reads it together with [`Self::is_active`] and never has to guess.
    pub queue_position: Option<crate::orchestrator::pool::QueuePosition>,
}

/// Classify one entry from data the registry already stores.
fn classify_call_class(agent_name: &str, worker_id_seq_value: u64) -> WorkerCallClass {
    // `0` is the marker of a token-count placeholder, which is not a registration.
    // Real registrations draw from `WORKER_ID_SEQ`, which starts at 1 precisely so
    // this marker can never collide with a live worker's id.
    if worker_id_seq_value == 0 {
        return WorkerCallClass::Placeholder;
    }
    let name = agent_name.trim().to_ascii_lowercase();
    let in_validator_family = name == VALIDATOR_ROLE_FAMILY
        || name
            .strip_prefix(VALIDATOR_ROLE_FAMILY)
            .is_some_and(|rest| rest.starts_with('-'));
    if in_validator_family {
        WorkerCallClass::ValidatorAudit
    } else {
        WorkerCallClass::Specialist
    }
}

/// Project one registry entry into the public read model. Infallible, and it
/// takes **no map guard**, so it is safe to call from inside a `WORKERS.iter()`
/// scan (the module's single-guard rule).
fn project_worker_entry(key: &str, state: &WorkerState) -> WorkerEntry {
    // Structural split on the disambiguator — `#…` is an opaque collision handle,
    // so everything before it is the natural key and everything after it is the
    // handle, with no substring guessing about agent or task names.
    let (natural_key, collision_handle) = match key.find(KEY_DISAMBIGUATOR) {
        Some(idx) => (
            key[..idx].to_string(),
            Some(key[idx + KEY_DISAMBIGUATOR.len_utf8()..].to_string()),
        ),
        None => (key.to_string(), None),
    };

    let elapsed_secs = match state.completed_at {
        Some(finished) => finished
            .saturating_duration_since(state.info.started_at)
            .as_secs(),
        None => state.info.started_at.elapsed().as_secs(),
    };

    WorkerEntry {
        key: key.to_string(),
        natural_key,
        collision_handle,
        disambiguated: key.contains(KEY_DISAMBIGUATOR),
        agent_name: state.info.agent_name.clone(),
        task_id: state.info.task_id.clone(),
        call_class: classify_call_class(&state.info.agent_name, state.worker_id_seq_value),
        is_active: state.is_active(),
        status_label: state.info.status.clone(),
        prompt: state.info.prompt.clone(),
        started_at: state.info.started_at,
        started_wall: state.info.started_wall,
        elapsed_secs,
        completed_seq: state.completed_seq,
        completed_wall: state.completed_wall,
        context_tokens: state.info.context_tokens,
        last_tokens: state.last_tokens,
        implementation_turns: state.info.implementation_turns,
        validation_rounds: state.info.validation_rounds,
        latest_validator_feedback: state.info.latest_validator_feedback.clone(),
        queue_position: state.queue_position,
    }
}

/// The registry's **live** entries as `(effective key, info)` pairs, sorted by
/// key (the same order [`live_worker_states`] and the active section of
/// [`get_active_subtasks_str`] use).
///
/// The key is the **effective** handle allocated by [`claim_worker_slot`] — it
/// includes the synthetic `{agent}-w{n}` form for taskless registrations and the
/// `{base}#N` form for registrations whose natural key was already owned by a
/// live worker. Consumers therefore never have to reconstruct a key, and a
/// per-field key lookup can never miss a disambiguated worker.
///
/// One item per matching entry: nothing is merged, deduplicated or renamed.
pub fn active_worker_entries() -> Vec<(String, ActiveWorkerInfo)> {
    let mut out: Vec<(String, ActiveWorkerInfo)> = WORKERS
        .iter()
        .filter(|e| e.value().is_active())
        .map(|e| (e.key().clone(), e.value().info.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The **recently completed** ring as `(effective key, info)` pairs — the same
/// entries the `Recently Completed Subagents:` section of
/// [`get_active_subtasks_str`] surfaces, kept for [`MAX_RECENT_COMPLETED`]
/// evictions. Sorted by key for stable rendering; sort on
/// [`WorkerEntry::completed_seq`] via [`worker_entries`] for the prompt's
/// most-recent-first layout.
pub fn recent_completed_entries() -> Vec<(String, ActiveWorkerInfo)> {
    let mut out: Vec<(String, ActiveWorkerInfo)> = WORKERS
        .iter()
        .filter(|e| !e.value().is_active())
        .map(|e| (e.key().clone(), e.value().info.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The whole registry as one deterministic read model: every entry — live **and**
/// the recently completed ring — as a single key-sorted projection with liveness
/// spelled out by [`WorkerEntry::is_active`].
///
/// This is the surface a projection consumer (pane / arbitrator view) is meant to
/// render: no label-string parsing for liveness, no key reconstruction, and
/// `WORKERS` / [`WorkerState`] stay private to this module.
pub fn worker_entries() -> Vec<WorkerEntry> {
    let mut out: Vec<WorkerEntry> = WORKERS
        .iter()
        .map(|e| project_worker_entry(e.key(), e.value()))
        .collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

/// The **effective registry key** of the worker that owns the cancellation token
/// currently in scope ([`crate::orchestrator::CURRENT_WORKER_TOKEN`]).
///
/// The specialist runner registers itself under that key
/// ([`register_active_worker_with_token`]) and drains its notice inbox with it, so a tool
/// that must speak *as* the running worker — `reply_to_arbitrator` above all — has to name
/// the same identity the notice store filed the notice under. Token identity is exact
/// (`CancellationToken::eq` compares the shared inner state, so a clone of the worker's own
/// token matches and a parent/child token does not).
///
/// `None` when no worker token is in scope (a Manager turn, the UI bridge, a unit test) or
/// when the registration carries no token: callers then fall back to the role name they
/// already have.
pub fn active_worker_key_for_current_token() -> Option<String> {
    let token = crate::orchestrator::CURRENT_WORKER_TOKEN
        .try_with(|t| t.clone())
        .ok()?;
    WORKERS
        .iter()
        .find(|e| {
            e.value().is_active()
                && e.value()
                    .info
                    .cancel_token
                    .as_ref()
                    .is_some_and(|t| *t == token)
        })
        .map(|e| e.key().clone())
}

/// Test-only: drop one entry from the table outright (no `COMPLETED_SEQ` draw,
/// no ring eviction) so a restore test can leave the shared table exactly as it
/// found it. Never used by production code paths.
#[cfg(test)]
pub fn remove_worker_entry_for_test(worker_key: &str) -> Option<WorkerState> {
    WORKERS.remove(worker_key).map(|(_, state)| state)
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
/// overwriting it, and that new key is what the guard reports — so every caller
/// keeps updating and cancelling *its own* entry.
pub fn register_active_worker_with_token(
    task_id: Option<String>,
    agent_name: String,
    prompt: String,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
) -> ActiveWorkerGuard {
    let clean_task_id = task_id
        .as_deref()
        .and_then(crate::task_id::normalize_task_id);

    // Correlation key: agent name + normalized task id (public format is
    // stable). Workers *without* a task id get the monotonic id from
    // `WORKER_ID_SEQ`, so two registrations inside the same clock tick cannot
    // share a key.
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
        worker_id,
    );
    // A worker started: only the edge is recorded here — the journal is opened
    // by the runner and harvested/written by the session thread at its next
    // commit, never from this thread.
    crate::ui::session::mark_worker_lifecycle_edge();
    ActiveWorkerGuard(key)
}

/// Build the fresh registry state written by [`claim_worker_slot`].
#[allow(clippy::too_many_arguments)]
fn new_worker_state(
    task_id: Option<String>,
    agent_name: &str,
    prompt: &str,
    cancel_token: &Option<tokio_util::sync::CancellationToken>,
    started_at: Instant,
    initial_tokens: usize,
    worker_id_seq_value: u64,
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
        worker_id_seq_value,
        // A fresh registration is not waiting for a slot: the pool reports a rank
        // only when a backend call actually has to queue.
        queue_position: None,
    }
}

/// Claim a registry slot for a new worker, **never overwriting a live entry**.
/// Key free → insert under the natural key. Key held only by a *completed* entry →
/// the natural key is reused and replaced, preserving the last-seen token count.
/// Key held by a **live** entry → a distinct disambiguated key `{base}#{id}` is
/// allocated and a `warn!` is emitted: overwriting would drop the live worker's
/// `CancellationToken`, leaving it active but unreachable from
/// [`cancel_active_worker`] / [`cancel_all_active_workers`]; the first guard's
/// `drop` would flip the *borrowed* entry to completed. Returns the effective key.
fn claim_worker_slot(
    base_key: &str,
    task_id: Option<String>,
    agent_name: String,
    prompt: String,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    started_at: Instant,
    worker_id_seq_value: u64,
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
                // its last-seen token count.
                let initial_tokens = occupied.get().last_tokens;
                occupied.insert(new_worker_state(
                    task_id.clone(),
                    &agent_name,
                    &prompt,
                    &cancel_token,
                    started_at,
                    initial_tokens,
                    worker_id_seq_value,
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
                    worker_id_seq_value,
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
                // Not a registration: no id is drawn for it, so
                // `worker_id_seq_value` stays meaningful for real
                // registrations only.
                worker_id_seq_value: 0,
                // A token-count stub is never a live worker, so it never has a
                // slot rank.
                queue_position: None,
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
    let updated = if let Some(mut entry) = WORKERS.get_mut(key)
        && entry.is_active()
    {
        entry.info.status = status.to_string();
        true
    } else {
        false
    };
    // `status_label` is a persisted worker field, so a completion / failure /
    // abort label is a lifecycle edge. The guard above is released before this
    // call (no map guard across a step), and recording an edge is an atomic
    // store — no disk, no commit.
    if updated {
        crate::ui::session::mark_worker_lifecycle_edge();
    }
}

/// Record — or take back down — the concurrency-slot **queue position** of a live
/// worker, addressed by its effective registry key.
///
/// The registry is the durable source of truth for this state (fix #3,
/// `docs/diagnose_subpane_stale_refresh.md` §3): the pool reports a rank once per
/// acquisition and afterwards only when the rank moves, and the matching
/// [`crate::orchestrator::DelegationEvent::Queued`] is a **one-shot** event that the
/// session drains with `events.drain(..)` — a `Queued` that reaches the drain before
/// the row projection has opened the row is consumed and never replayed. Mirroring
/// the position here is what lets any later projection re-derive the panel's
/// `(Queued n/m)` label from the registry alone.
///
/// `None` means "this worker holds no slot wait" — written when the permit is
/// acquired, when a wait is abandoned, and on entry teardown. Only a **live** entry
/// is touched: an entry the registry already closed has no wait to report (its own
/// teardown cleared the field), so this returns `false` rather than resurrecting a
/// rank on a completed worker.
///
/// No lifecycle edge is marked: `queue_position` is not a persisted worker field, and
/// the panel is driven by its projection boundaries, which every queue transition
/// sits inside.
pub fn set_active_worker_queue_position(
    key: &str,
    position: Option<crate::orchestrator::pool::QueuePosition>,
) -> bool {
    match WORKERS.get_mut(key) {
        Some(mut entry) if entry.is_active() => {
            entry.queue_position = position;
            true
        }
        _ => false,
    }
}

/// Resolve the **live** registry entry a pool queue report belongs to, and write its
/// position.
///
/// Queue reports reach this from the concurrency gate (`llm::client::ChatClient::acquire_slot`),
/// which knows the agent and plan task of the call that is waiting — never a registry
/// key. The correlation rule must therefore be the one
/// [`register_active_worker_with_token`] uses to **build** the key, so a report can
/// only ever land on the entry the panel row of that delegation is named after:
///
/// * the **natural** key `{agent}-{normalized task}` when a live entry holds it — the
///   same preference `ui::helpers::registry_entry_for_event` applies to a delegation
///   event, which keeps the event half and the registry half of one transition on the
///   same entry;
/// * otherwise a `{base}#…` collision handle, and only when **exactly one** live entry
///   carries it: two live registrations of one identity are two workers, and guessing
///   which one is waiting would pin one delegation's rank to the other's row.
///
/// `false` is the honest answer for everything the report cannot address: no task id
/// (a task-less worker's key carries a drawn `WORKER_ID_SEQ` id no report can spell),
/// no live entry for that identity, or an ambiguous one.
pub fn set_worker_queue_position(
    agent_name: &str,
    task_id: Option<&str>,
    position: Option<crate::orchestrator::pool::QueuePosition>,
) -> bool {
    let clean_task = task_id.and_then(crate::task_id::normalize_task_id);
    let Some(clean_task) = clean_task else {
        return false;
    };
    let base_key = format!("{agent_name}-{clean_task}");
    // Natural key first, and only a live entry. The lookup is bound to a `bool` so no
    // map guard can possibly outlive the expression before the write below (the
    // module's single-guard rule).
    let natural_is_live = WORKERS
        .get(&base_key)
        .is_some_and(|entry| entry.value().is_active());
    if natural_is_live {
        return set_active_worker_queue_position(&base_key, position);
    }
    // No live natural key: a duplicate registration of the same identity took a
    // `{base}#…` handle instead. The disambiguator is the anchor, so this is a
    // structural match on the whole `{base}` segment and never a prefix guess about
    // an agent or task name.
    let prefixed = format!("{base_key}{KEY_DISAMBIGUATOR}");
    let mut candidates = WORKERS
        .iter()
        .filter(|entry| entry.value().is_active() && entry.key().starts_with(&prefixed))
        .map(|entry| entry.key().clone())
        .collect::<Vec<String>>()
        .into_iter();
    match (candidates.next(), candidates.next()) {
        // Exactly one live registration behind this identity: that is the entry waiting.
        (Some(key), None) => set_active_worker_queue_position(&key, position),
        // Two or more: ambiguous, and a rank pinned to the wrong row is worse than no
        // rank at all.
        (Some(_), Some(_)) => {
            tracing::debug!(
                agent = agent_name,
                task = %clean_task,
                "queue report is ambiguous across live registrations of this identity; \
                 leaving the registry queue state untouched"
            );
            false
        }
        // Nothing live to write.
        (None, _) => false,
    }
}

/// The queue position currently recorded for one live registry entry, if any.
///
/// A read model for consumers (tests, diagnostics) that must not reach into
/// [`WorkerState`].
pub fn active_worker_queue_position(key: &str) -> Option<crate::orchestrator::pool::QueuePosition> {
    WORKERS
        .get(key)
        .filter(|entry| entry.value().is_active())
        .and_then(|entry| entry.value().queue_position)
}

/// Get the context token count for an active specialist worker by its registry
/// key (e.g. `{agent}-{task}`).
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

/// Prompt file of a registered worker: a task id — LLM output from `delegate_task`
/// or a plan checkbox — becomes `.marmel/prompts/<task_id>.md` only through
/// [`crate::task_id::validate_task_id`], the crate's single authority on an id.
/// Mirroring [`crate::harness::workspace::Workspace::prompt_path_for_task`], the
/// id is normalized first (decoration stripping only) and that **normalized**
/// value is validated and joined — never sanitized, trimmed, clamped or mended.
/// `Ok(None)` = no task id; `Err(TaskIdError)` = refused: skip the read (fail
/// closed) rather than retrying a repaired id.
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
                "- Tool Call ID: {}\n  Subagent Tag: {}\n  Address This Worker With: {}\n  Subagent: {}\n  Status: {}\n  Task Prompt: {}\n  Started At: {} (running for {}, {elapsed_secs} total seconds)\n  Implementation Turns: {}\n  Validation Rounds: {}\n",
                task_id_str, id, id, info.agent_name, info.status, info.prompt, start_wall_str,
                duration_str, info.implementation_turns, info.validation_rounds
            ));
            // The per-worker prompt read is grammar-validated (see
            // [`worker_prompt_path`]): a rejected task id never reaches the
            // read, so the worker renders in the same "prompt unavailable"
            // shape as when its prompt file is missing, and the id is never
            // repaired into some other file name.
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

// Exact worker identity — the notice router's model, reused verbatim.

/// The **exact routing identity** of a registered worker — not a second identity
/// model: it defers to the single authority used for notice routing,
/// [`crate::orchestrator::notice::worker_routing_identity`] (effective key vs.
/// natural `{agent}-{task}` key, task id taken from the canonical grammar via
/// [`crate::plan_parse::is_task_id_token`]), then pins the two **authoritative**
/// fields off the registry entry — what [`register_active_worker_with_token`]
/// joined — so a task-shaped agent name cannot split wrongly; a missing field
/// falls back to the key-split value, keeping `{agent}-w{n}` workers addressable.
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
/// Tested through the shared identity model ([`worker_identity`] +
/// [`crate::orchestrator::notice::WorkerRoutingIdentity::routes`]), never by
/// substring matching or free-text fields: exact task id (case- and
/// decoration-tolerant through the canonical normalizer), exact natural key,
/// exact effective key (including a `#…` collision handle), exact agent name, or
/// a broadcast (`*` / `worker`). An empty or decoration-only address matches none.
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
/// **exactly**? Both arguments go through the notice router's whole-field
/// equality rules ([`crate::orchestrator::notice::WorkerRoutingIdentity::routes`]):
/// agent name (`coder`), role family (`validator` → `validator-coder`), the whole
/// natural or effective key (`{agent}-{task}`, `{agent}-{task}#7`) or a broadcast
/// (`*`, `worker`) — but a task id or key must match as a whole token, so a `t-1`
/// cancel never kills the worker running `t-10`. An absent argument is a wildcard;
/// both present means **both** must match the same worker (see [`get_active_subtask_by_id`]).
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
/// Targeting is **exact identity matching**, the same rules notice routing
/// uses — see [`worker_matches`]. A `t-1` target cancels the worker running
/// `t-1` and never the one running `t-10`; a bare agent name, a role family, a
/// whole natural/effective key, or an explicit broadcast (`*` / `worker`)
/// address their workers as usual.
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

    /// The restore installer writes the **stored** key and the stored
    /// `completed_seq` verbatim, refuses to clobber a live entry (the same
    /// refusal [`claim_worker_slot`] makes, so a cancellation token is never
    /// dropped), and closing a restored record goes through the existing
    /// `COMPLETED_SEQ` path — never a synthesized [`ActiveWorkerGuard`].
    #[test]
    fn test_restore_installer_keeps_keys_and_closes_through_completed_seq() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let key = "restored-t-001";
        let restored_key = remove_worker_entry_for_test(key);
        assert!(restored_key.is_none(), "the fixture key must start free");

        let base_completed = completed_seq_floor();
        let base_ids = worker_id_seq_floor();
        seed_worker_id_seq_floor(base_ids + 500);
        seed_completed_seq_floor(base_completed + 500);

        // A live worker owns the natural key of the registration below.
        let guard = register_active_worker_with_token(
            Some("t-collide".to_string()),
            "restored".to_string(),
            "Live worker".to_string(),
            None,
        );

        let state = new_worker_state(
            Some("t-001".to_string()),
            "restored",
            "Restored brief",
            &None,
            Instant::now(),
            0,
            base_ids + 3,
        );
        assert!(
            install_restored_worker(key.to_string(), state.clone()),
            "a free key installs verbatim"
        );
        assert!(
            !install_restored_worker(guard.0.clone(), state.clone()),
            "a key owned by a live worker must never be overwritten"
        );
        assert!(
            worker_state_snapshot(key).as_ref().map(|s| s.is_active()) == Some(true),
            "an interrupted record stays open so cancel/routing still see it"
        );

        // Nothing that registers after the floors were raised can re-issue an
        // id the bundle already carried.
        let fresh = register_active_worker_with_token(
            None,
            "floored".to_string(),
            "Post-restore worker".to_string(),
            None,
        );
        let fresh_state = worker_state_snapshot(&fresh.0).expect("fresh entry");
        assert!(
            fresh_state.worker_id_seq_value > base_ids + 3,
            "fresh id {} must exceed the carried id {}",
            fresh_state.worker_id_seq_value,
            base_ids + 3
        );
        assert_eq!(
            fresh.0,
            format!("floored-w{}", fresh_state.worker_id_seq_value),
            "the key must carry the raised id, not one the bundle already used"
        );

        let seq = retire_restored_worker(key, "Retired after restart").expect("closed");
        assert!(
            seq > base_completed + 500 - 1,
            "the drawn sequence {seq} must sit above the restore floor"
        );
        let closed = worker_state_snapshot(key).expect("closed record");
        assert_eq!(closed.completed_seq, Some(seq));
        assert_eq!(closed.info.status, "Retired after restart");
        assert!(!closed.is_active(), "a closed record is no longer a worker");
        assert!(
            !crate::orchestrator::notice::has_worker_inbox(key),
            "retiring reclaims the worker's notice inbox"
        );

        remove_worker_entry_for_test(key);
        remove_worker_entry_for_test(&fresh.0);
        let live_key = guard.0.clone();
        drop(guard);
        remove_worker_entry_for_test(&live_key);
    }

    /// The floors move with `fetch_max`, never an assignment, so a restore can
    /// never rewind an id counter and hand out a used id.
    #[test]
    fn test_restore_floors_move_up_only() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let ids = worker_id_seq_floor();
        assert_eq!(seed_worker_id_seq_floor(ids.saturating_sub(10)), ids);
        assert_eq!(worker_id_seq_floor(), ids, "a lower floor changes nothing");
        seed_worker_id_seq_floor(ids + 40);
        assert_eq!(worker_id_seq_floor(), ids + 40);

        let completed = completed_seq_floor();
        assert_eq!(
            seed_completed_seq_floor(completed.saturating_sub(10)),
            completed
        );
        assert_eq!(completed_seq_floor(), completed);
        seed_completed_seq_floor(completed + 40);
        assert_eq!(completed_seq_floor(), completed + 40);
    }

    #[test]
    fn test_worker_progress_and_completed_history() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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

        let cancelled = cancel_active_worker(None, Some("t-001"));
        assert!(cancelled);
        assert!(token1.is_cancelled());
        assert!(!token2.is_cancelled());

        let count = cancel_all_active_workers();
        assert!(count >= 1);
        assert!(token2.is_cancelled());

        drop(guard1);
        drop(guard2);
    }

    #[test]
    fn test_format_workers_summary_utf8_char_boundary_no_panic() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Concurrency stress: 50 concurrent workers hammer the single sharded
    /// `WORKERS` DashMap — each registers via
    /// `register_active_worker_with_token`, interleaves
    /// `update_active_worker_context` + `set_active_worker_status`, then drops
    /// its guard (single-entry mutation + bounded-ring eviction). The whole
    /// batch must finish well within a generous wall-clock bound (no
    /// deadlock), and the completed ring must stay capped at 10 entries.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_workers_stress_concurrent_register_update_drop() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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

    // Unique worker ids: registry entries are never clobbered.

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

    /// (i) Two workers registered back-to-back with **no task id** — the shape
    /// that would produce identical keys if the id source were a near-constant
    /// clock reading — must get distinct, monotonic ids from `WORKER_ID_SEQ` and
    /// BOTH must stay reachable by the cancel path.
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

    /// (i/ii) Same agent + same task id ⇒ identical natural key (two overlapping
    /// steer rounds both yield `steer-task-1`). The second registration must not
    /// clobber the first worker's live entry: the entry at the original key must
    /// still hold the first worker's state and token, and
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

    /// (ii) Full Scenario-A repro: after a key collision the *first* guard's
    /// `drop` must not flip the shared entry to completed — that hides the still
    /// running second worker from `has_active_workers()` and from every cancel
    /// path (they all filter on `is_active()`).
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
    /// worker that inherited the key. Completing somebody else's live entry is
    /// what makes a running worker un-killable: `cancel_active_worker` and
    /// `cancel_all_active_workers` both skip non-active entries.
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

        // Keys with no live entry (evicted/completed) still store the token
        // count.
        update_active_worker_context("h5phantom-t-777", 42);
        assert_eq!(get_active_worker_tokens("h5phantom-t-777"), Some(42));

        drop(guard);
    }

    /// (ii) Race guard for the same clobber class: `update_active_worker_context`
    /// is one `entry` transaction, so a registration landing between the lookup
    /// and the write can never have its live entry (and cancellation token)
    /// replaced by the token-less placeholder.
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
        // … so a targeted cancel does not match them either.
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

    // Teardown inbox reclaim

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

    // The per-worker `.marmel/prompts/<task_id>.md` READ site.
    //
    // `worker_prompt_path` is the only join for this read and it routes the
    // already-normalized id through `crate::task_id::validate_task_id`, the
    // single grammar authority in the crate. The call site fails closed: a
    // rejected id is never read, never mended into a different file name, and
    // renders exactly like a missing prompt file (JIT synthesis stays the
    // fallback), with a WARN naming the rejected id and the reason.

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

    /// WARN capture counting only the rejections emitted by `worker_prompt_path`.
    #[derive(Clone, Default)]
    struct RejectTaskIdWarns(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    struct WarnMessageVisitor<'a> {
        found: &'a mut Option<String>,
    }

    impl tracing::field::Visit for WarnMessageVisitor<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                *self.found = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.found = Some(format!("{value:?}"));
            }
        }
    }

    impl tracing::Subscriber for RejectTaskIdWarns {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.level() <= &tracing::Level::WARN
        }

        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::WARN)
        }

        fn register_callsite(
            &self,
            _metadata: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
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
    /// blueprint planted *outside* the prompts dir (where an unvalidated
    /// `prompts/../escape.md` join would land) must never be read.
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
            // 1. A typed refusal per hostile spelling, and no path is ever
            //    handed back to the caller.
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
            let _tracing_lock = TEST_TRACING_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            // Ensure the rejection warning callsite in get_active_subtasks_str is registered
            // in CALLSITES before rebuilding interest for our test subscriber.
            let _ = get_active_subtasks_str();
            tracing::subscriber::with_default(RejectTaskIdWarns(counter.clone()), || {
                tracing::callsite::rebuild_interest_cache();
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
            drop(_tracing_lock);
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

    /// A normal task id — decorated or not — still normalizes, validates and
    /// round-trips through the prompts directory, so validation costs nothing
    /// on the happy path.
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

    // Cancel and diagnostic targeting address a worker by its exact
    // routing identity, never by a substring of a key, a task id or a prompt.
    // Fixtures mirror the `worker_routing_identity` / `routes()` identity tests
    // in `notice_tests`.

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

        // Absent targets are wildcards, but never a match-all; when both are
        // present, both must address the same worker (AND composition).
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

    /// (a, continued) A truncated task id or tag addresses nobody — matching is
    /// whole-field only — while the decorated exact id still does.
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

        // No prompt-substring lookup path exists.
        assert!(
            get_active_subtask_by_id("t-7777").is_none(),
            "a task id may not be looked up through a worker's free-text prompt"
        );
        // Fragments of a live key resolve to nobody.
        assert!(get_active_subtask_by_id("t065lookshort-t-").is_none());
        assert!(get_active_subtask_by_id("t065lookshort-t-10").is_none());
        // Empty / decoration-only ids match nobody, never *every* active worker:
        // the address is normalized first and an empty address matches nothing.
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
        // role must not reach it (matching is whole-field, leading-segment only).
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

    /// Task A read model: `active_worker_entries()`, `recent_completed_entries()`
    /// and `worker_entries()` hand out the **effective** registry key (including
    /// the `{agent}-wN` and `{base}#N` forms), an explicit `is_active` boolean,
    /// the token counters and a call class — and they leak neither `WORKERS` nor
    /// `WorkerState`.
    #[test]
    fn test_worker_read_model_exposes_effective_keys_and_explicit_liveness() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let base_ids = worker_id_seq_floor();
        seed_worker_id_seq_floor(base_ids + 700);

        let g_specialist = register_active_worker(
            Some("t-701".to_string()),
            "rdmodelspecialist".to_string(),
            "read model specialist".to_string(),
        );
        let g_taskless = register_active_worker(
            None,
            "rdmodeltaskless".to_string(),
            "read model taskless worker".to_string(),
        );
        let g_dup_natural = register_active_worker(
            Some("t-702".to_string()),
            "rdmodeldup".to_string(),
            "natural key holder".to_string(),
        );
        let g_dup_disambiguated = register_active_worker(
            Some("t-702".to_string()),
            "rdmodeldup".to_string(),
            "collision-handle twin".to_string(),
        );
        let g_validator = register_active_worker(
            Some("t-703".to_string()),
            "validator-rdmodelcoder".to_string(),
            "validation pass".to_string(),
        );
        // A finished worker must show up too — the same entries the
        // `Recently Completed Subagents:` section surfaces — but with liveness
        // spelled out by a boolean, never inferred from a label.
        let g_finished = register_active_worker(
            Some("t-704".to_string()),
            "rdmodelfinished".to_string(),
            "already done".to_string(),
        );
        let finished_key = g_finished.0.clone();
        drop(g_finished);

        // A terminal label must not flip liveness.
        set_active_worker_status(&g_specialist.0, "Aborted");
        // Token counters travel with the row.
        update_active_worker_context(&g_specialist.0, 1_234);
        // A key with no live entry leaves a token-count placeholder behind.
        update_active_worker_context("rdmodelphantom-t-705", 991);

        let entries = worker_entries();
        let mine: Vec<&WorkerEntry> = entries
            .iter()
            .filter(|e| e.key.contains("rdmodel"))
            .collect();
        assert_eq!(
            mine.len(),
            7,
            "five live workers, one completed worker, one token-count placeholder"
        );

        // Deterministic, key-sorted order for every read API.
        assert!(
            entries.windows(2).all(|w| w[0].key <= w[1].key),
            "worker_entries() must be key-sorted so a projection renders stably"
        );

        let active_keys: Vec<String> = active_worker_entries()
            .into_iter()
            .map(|(key, _info)| key)
            .filter(|key| key.contains("rdmodel"))
            .collect();
        let completed_keys: Vec<String> = recent_completed_entries()
            .into_iter()
            .map(|(key, _info)| key)
            .filter(|key| key.contains("rdmodel"))
            .collect();
        let mut expected_active = vec![
            g_dup_disambiguated.0.clone(),
            g_dup_natural.0.clone(),
            g_specialist.0.clone(),
            g_taskless.0.clone(),
            g_validator.0.clone(),
        ];
        expected_active.sort();
        assert_eq!(
            active_keys, expected_active,
            "one item per live entry, keyed by the effective key, in key order"
        );
        assert_eq!(
            completed_keys,
            vec![finished_key.clone(), "rdmodelphantom-t-705".to_string()],
            "the same entries the `Recently Completed Subagents:` section surfaces \
             are part of the read model, in key order"
        );

        // `is_active` is the liveness authority and agrees with the phase-split
        // read APIs — a consumer never has to infer liveness from a label string.
        let mut mine_active: Vec<String> = mine
            .iter()
            .filter(|e| e.is_active)
            .map(|e| e.key.clone())
            .collect();
        mine_active.sort();
        assert_eq!(
            mine_active, expected_active,
            "`is_active` == the active phase"
        );
        let mut mine_completed: Vec<String> = mine
            .iter()
            .filter(|e| !e.is_active)
            .map(|e| e.key.clone())
            .collect();
        mine_completed.sort();
        assert_eq!(
            mine_completed, completed_keys,
            "`!is_active` == the completed phase"
        );

        let find = |key: &str| {
            entries
                .iter()
                .find(|e| e.key == key)
                .unwrap_or_else(|| panic!("the read model must carry the key {key}"))
        };

        // `{agent}-wN`: the synthetic key is exposed verbatim.
        assert!(
            g_taskless.0.starts_with("rdmodeltaskless-w"),
            "a taskless registration keeps its monotonic key: {}",
            g_taskless.0
        );
        let taskless = find(&g_taskless.0);
        assert_eq!(taskless.call_class, WorkerCallClass::Specialist);
        assert!(taskless.task_id.is_none());
        assert!(!taskless.disambiguated);

        // `{base}#N`: the collision handle is exposed as its own field, and the
        // natural key stays addressable.
        assert!(
            g_dup_disambiguated.0.starts_with("rdmodeldup-t-702#"),
            "the collision twin keeps its disambiguated key: {}",
            g_dup_disambiguated.0
        );
        let dup = find(&g_dup_disambiguated.0);
        assert!(dup.disambiguated);
        assert_eq!(dup.natural_key, g_dup_natural.0);
        assert_eq!(
            dup.collision_handle.as_deref(),
            g_dup_disambiguated.0.split('#').nth(1),
            "everything after `#` is the opaque collision handle"
        );
        assert_eq!(find(&g_dup_natural.0).collision_handle, None);

        // Call class: the validator family is distinguishable from a specialist.
        assert_eq!(
            find(&g_validator.0).call_class,
            WorkerCallClass::ValidatorAudit
        );
        assert_eq!(
            find(&g_specialist.0).call_class,
            WorkerCallClass::Specialist
        );

        // Liveness is a boolean, not a label reading.
        let specialist = find(&g_specialist.0);
        assert!(
            specialist.is_active,
            "a label change never completes a worker"
        );
        assert_eq!(specialist.status_label, "Aborted");
        assert_eq!(specialist.context_tokens, 1_234);
        assert_eq!(specialist.last_tokens, 1_234);
        assert_eq!(get_active_worker_tokens(&g_specialist.0), Some(1_234));
        assert!(specialist.completed_seq.is_none());

        // Completed entries: closed, sequenced, still carrying their tokens.
        let finished = find(&finished_key);
        assert!(
            !finished.is_active,
            "a completed entry is not a live worker"
        );
        assert!(finished.completed_seq.is_some());
        assert!(finished.completed_wall.is_some());

        // A token-count placeholder is never a live worker and is labelled as
        // what it is.
        let phantom = find("rdmodelphantom-t-705");
        assert_eq!(phantom.call_class, WorkerCallClass::Placeholder);
        assert!(!phantom.is_active);
        assert_eq!(phantom.last_tokens, 991);

        for key in [
            &g_specialist.0,
            &g_taskless.0,
            &g_dup_natural.0,
            &g_dup_disambiguated.0,
            &g_validator.0,
            &finished_key,
        ] {
            remove_worker_entry_for_test(key);
        }
        remove_worker_entry_for_test("rdmodelphantom-t-705");
        drop(g_specialist);
        drop(g_taskless);
        drop(g_dup_natural);
        drop(g_dup_disambiguated);
        drop(g_validator);
    }

    /// The read model's classification and key split are structural, never
    /// substring-based: the validator family is anchored at a segment boundary,
    /// `worker_id_seq_value == 0` marks a placeholder, and everything after `#`
    /// is an opaque collision handle (numeric **or** the UUID fallback).
    #[test]
    fn test_worker_read_model_classification_and_key_split_are_structural() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        assert_eq!(
            classify_call_class("validator", 1),
            WorkerCallClass::ValidatorAudit
        );
        assert_eq!(
            classify_call_class("Validator-Coder", 3),
            WorkerCallClass::ValidatorAudit,
            "family matching is case-insensitive"
        );
        assert_eq!(
            classify_call_class("validatorx", 3),
            WorkerCallClass::Specialist,
            "an unanchored prefix must not classify a worker as a validator"
        );
        assert_eq!(
            classify_call_class("planner-validator", 3),
            WorkerCallClass::Specialist,
            "only a leading segment is a role family"
        );
        assert_eq!(classify_call_class("coder", 3), WorkerCallClass::Specialist);
        assert_eq!(
            classify_call_class("coder", 0),
            WorkerCallClass::Placeholder,
            "the token-count stub is not a registration"
        );

        let state = new_worker_state(
            Some("t-01".to_string()),
            "coder",
            "brief",
            &None,
            Instant::now(),
            7,
            41,
        );
        let plain = project_worker_entry("coder-t-01", &state);
        assert_eq!(plain.natural_key, "coder-t-01");
        assert_eq!(plain.collision_handle, None);
        assert!(!plain.disambiguated);
        assert!(plain.is_active, "a fresh state is active on registration");
        assert_eq!(plain.last_tokens, 7);

        let numbered = project_worker_entry("coder-t-01#7", &state);
        assert_eq!(numbered.natural_key, "coder-t-01");
        assert_eq!(numbered.collision_handle.as_deref(), Some("7"));
        assert!(numbered.disambiguated);

        let uuided =
            project_worker_entry("coder-t-01#u11111111-2222-3333-4444-555555555555", &state);
        assert_eq!(uuided.natural_key, "coder-t-01");
        assert_eq!(
            uuided.collision_handle.as_deref(),
            Some("u11111111-2222-3333-4444-555555555555"),
            "a hyphenated UUID handle is opaque, not a task id"
        );
    }

    /// **Queue-state durability (fix #3).** The registry must be a durable source of
    /// truth for a slot rank, not a pass-through: a rank reported through
    /// [`set_worker_queue_position`] is readable from the projected entry every later
    /// consumer sees, it is replaced (never accumulated) by the next report, and each
    /// way a wait ends — permit acquired, guard dropped, entry already closed — takes
    /// it back down instead of pinning a rank to a dead delegation.
    #[test]
    fn test_queue_position_is_durable_on_the_registry_entry() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let rank =
            |position: usize, of: usize| crate::orchestrator::pool::QueuePosition { position, of };
        let projected = |key: &str| {
            worker_entries()
                .into_iter()
                .find(|entry| entry.key == key)
                .expect("the registry holds this worker")
        };

        let guard = register_active_worker(
            Some("t-9301".to_string()),
            "coder".to_string(),
            "brief for a delegation waiting on a backend slot".to_string(),
        );
        let key = guard.0.clone();
        assert_eq!(key, "coder-t-9301", "the natural key is the row name");
        assert_eq!(
            projected(&key).queue_position,
            None,
            "a fresh registration holds no slot wait"
        );

        // The pool's report at queue-entry time lands on the entry — and it resolves
        // through the *decorated* spelling a caller may carry, because the correlation
        // rule normalizes exactly like the registration did.
        assert!(
            set_worker_queue_position("coder", Some("[t-9301]"), Some(rank(2, 3))),
            "a live natural key is addressable by agent + plan task"
        );
        assert_eq!(
            projected(&key).queue_position,
            Some(rank(2, 3)),
            "the rank survives the projection that reads it back"
        );
        assert_eq!(active_worker_queue_position(&key), Some(rank(2, 3)));

        // A later rank move replaces it for as long as the wait lasts.
        assert!(set_worker_queue_position(
            "coder",
            Some("t-9301"),
            Some(rank(1, 3))
        ));
        assert_eq!(
            projected(&key).queue_position,
            Some(rank(1, 3)),
            "the rank is replaced, never accumulated"
        );

        // Acquiring the permit clears it: the panel must never keep showing
        // `(Queued 1/3)` for a call that is running.
        assert!(set_active_worker_queue_position(&key, None));
        assert_eq!(
            projected(&key).queue_position,
            None,
            "running is not waiting"
        );

        // A wait that ends by teardown is cleared **by the teardown**, so a completed
        // entry never reports a rank to the read model or to a projection.
        set_worker_queue_position("coder", Some("t-9301"), Some(rank(3, 4)));
        assert_eq!(projected(&key).queue_position, Some(rank(3, 4)));
        drop(guard);
        let closed = projected(&key);
        assert!(!closed.is_active, "the guard closed the entry");
        assert_eq!(closed.queue_position, None, "and cleared the rank");
        assert_eq!(
            active_worker_queue_position(&key),
            None,
            "the read model agrees"
        );
        assert!(
            !set_active_worker_queue_position(&key, Some(rank(1, 1))),
            "a closed entry never gets a rank back"
        );
        remove_worker_entry_for_test(&key);
    }

    /// A queue report spells an agent and a plan task, never a registry key, so the
    /// correlation rule has to be the one [`claim_worker_slot`] used to **build** the
    /// key: a live natural key wins, a lone `{base}#…` handle is resolved, and two live
    /// registrations of one identity are refused rather than guessed.
    #[test]
    fn test_queue_position_report_resolves_a_disambiguated_key_and_refuses_an_ambiguous_one() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let rank = crate::orchestrator::pool::QueuePosition { position: 1, of: 2 };
        let brief = "brief for a queued delegation".to_string();

        let first = register_active_worker(
            Some("t-9302".to_string()),
            "coder".to_string(),
            brief.clone(),
        );
        let second = register_active_worker(
            Some("t-9302".to_string()),
            "coder".to_string(),
            brief.clone(),
        );
        let third = register_active_worker(Some("t-9302".to_string()), "coder".to_string(), brief);
        let (first_key, second_key, third_key) =
            (first.0.clone(), second.0.clone(), third.0.clone());
        assert_eq!(
            first_key, "coder-t-9302",
            "the first registration owns the natural key"
        );
        assert!(
            second_key.starts_with("coder-t-9302#") && third_key.starts_with("coder-t-9302#"),
            "a live natural key forces `{{base}}#…` handles, got {second_key} and {third_key}"
        );

        // Three live registrations of one identity: the natural key is what the row of
        // *that* delegation is named after, so the rank goes there and nowhere else.
        assert!(set_worker_queue_position(
            "coder",
            Some("t-9302"),
            Some(rank)
        ));
        assert_eq!(active_worker_queue_position(&first_key), Some(rank));
        assert_eq!(
            active_worker_queue_position(&second_key),
            None,
            "a report never spreads across registrations"
        );
        assert_eq!(active_worker_queue_position(&third_key), None);

        // The natural holder closes: two live `{base}#…` registrations remain and the
        // report cannot tell which one is waiting. A rank pinned to the wrong row is
        // worse than no rank, so the registry is left untouched.
        drop(first);
        assert_eq!(
            active_worker_queue_position(&second_key),
            None,
            "the closed natural holder keeps no rank either"
        );
        assert!(
            !set_worker_queue_position("coder", Some("t-9302"), Some(rank)),
            "an ambiguous identity is refused"
        );
        assert_eq!(active_worker_queue_position(&second_key), None);
        assert_eq!(active_worker_queue_position(&third_key), None);

        // One live registration remains: exactly the entry the panel row names, so the
        // report resolves its collision handle.
        drop(second);
        assert!(
            set_worker_queue_position("coder", Some("t-9302"), Some(rank)),
            "a lone `{{base}}#…` registration is addressable"
        );
        assert_eq!(active_worker_queue_position(&third_key), Some(rank));
        assert_eq!(
            worker_entries()
                .into_iter()
                .find(|entry| entry.key == third_key)
                .expect("the disambiguated entry is still registered")
                .queue_position,
            Some(rank),
            "and it reaches the projection like any other rank"
        );

        drop(third);
        remove_worker_entry_for_test(&first_key);
        remove_worker_entry_for_test(&second_key);
        remove_worker_entry_for_test(&third_key);
    }

    /// The report must never invent or guess an entry: a task-less worker's key carries
    /// a drawn `WORKER_ID_SEQ` id no report can spell, an id that normalizes away is no
    /// identity at all, and an identity with no live entry has nothing to write.
    #[test]
    fn test_queue_position_report_refuses_an_identity_it_cannot_address() {
        let _lock = TEST_WORKERS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let rank = crate::orchestrator::pool::QueuePosition { position: 1, of: 1 };

        assert!(
            !set_worker_queue_position("coder", None, Some(rank)),
            "a task-less worker is not addressable by agent name alone"
        );
        assert!(
            !set_worker_queue_position("coder", Some("  []  "), Some(rank)),
            "an id that normalizes away is no identity"
        );
        assert!(
            !set_worker_queue_position("coder", Some("t-9303-never-registered"), Some(rank)),
            "no live entry, no write"
        );
        assert!(
            !set_active_worker_queue_position("coder-t-9303-never-registered", Some(rank)),
            "and no write to a key the registry does not hold"
        );
        assert_eq!(
            active_worker_queue_position("coder-t-9303-never-registered"),
            None,
            "nothing was resurrected"
        );

        let guard = register_active_worker(None, "coder".to_string(), "no plan task".to_string());
        let key = guard.0.clone();
        assert!(
            key.starts_with("coder-w"),
            "a task-less registration gets a drawn id: {}",
            key
        );
        assert!(
            !set_worker_queue_position("coder", None, Some(rank)),
            "and its key cannot be spelled from a queue report"
        );
        assert_eq!(
            active_worker_queue_position(&key),
            None,
            "so it keeps no rank, and no phantom entry was created for the report"
        );
        assert!(
            worker_entries()
                .iter()
                .all(|entry| !entry.key.starts_with("coder-t-9303")),
            "a refused report never invents a registry entry to put a rank on"
        );
        drop(guard);
        remove_worker_entry_for_test(&key);
    }
}
