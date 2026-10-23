//! The per-worker history mirror and the worker restore/installer seam: the two
//! halves of the *worker* side of the session contract.
//!
//! **Capture (mirror-out).** The specialist's `ContextEngine` is a local inside
//! `run_specialist_live_inner` and is unreachable from outside it, so the
//! worker's history is mirrored out **as it is produced**: one
//! `Vec<MessageRecord>` per worker, appended once per engine mutation, kept in
//! memory only, harvested by the committer through [`REGISTRY`]. It never touches
//! disk and never re-enters the engine, which stays authoritative; no journal
//! lock is ever held across file I/O, a registry lookup or another map guard.
//!
//! **Restore (identity + status projection).** A restored worker is inserted into
//! the live `WORKERS` table under its **stored** key with its `completed_seq`
//! verbatim, after the three id floors are raised with `fetch_max`. The record is
//! restored, the *execution* is not: nothing is auto-relaunched, and exactly one
//! synthetic manager notice ([`restore_notice`]) says what was interrupted. The
//! installer lives here because `WORKERS` is private to `orchestrator::workers`,
//! whose seam is [`crate::orchestrator::workers::install_restored_worker`], the
//! `seed_*_floor` helpers and
//! [`crate::orchestrator::workers::retire_restored_worker`].
//!
//! The mirror is bounded by `Config.session_worker_mirror_max_messages`; overflow
//! is counted and reported through [`WorkerJournal::was_truncated`] /
//! [`WorkerJournal::truncation_notice`], never silently dropped.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock};

use tokio::task_local;

use crate::agents::Agent;
use crate::manager::ContextEngine;
use crate::orchestrator::workers::{self, ActiveWorkerInfo, WorkerState};
use crate::session::format::messages::MessageRecord;
use crate::session::format::{
    DanglingCall, DanglingToolCallState, HistorySource, IdentityFloors, NonRestorableKind,
    NonRestorableNotice, ProcessEnding, TimePoint, WorkerCounters, WorkerSection, WorkerStatus,
};
use crate::types::Message;

/// The label a restored in-flight worker carries in the subagent list. The
/// stored label survives inside `WorkerRestoreReport` / [`restore_notice`], which
/// prints both halves (`In Progress -> Interrupted by restart`).
pub const INTERRUPTED_STATUS_LABEL: &str = "Interrupted by restart";

/// How many **finished** journals stay in [`REGISTRY`] before the oldest are
/// dropped. Mirrors the live registry's `MAX_RECENT_COMPLETED` (10): the map is
/// the working set, the bundle files are the durable record, and a journal with
/// no live entry behind it is only kept long enough for the next commit to
/// capture the completed worker's history.
pub const MAX_CLOSED_JOURNALS: usize = 10;

/// The delegation seed a journal carries alongside the mirrored history
/// (`DelegationRequest` / `IsolatedContext`). It is what lets the committer
/// rebuild a full `WorkerSection` from `(WORKERS entry, journal)` without
/// reaching into a live runner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerSeed {
    pub brief: String,
    pub snippets: Vec<String>,
    pub image_urls: Vec<String>,
    pub audio_urls: Vec<String>,
    pub recursion_granted: bool,
}

impl WorkerSeed {
    /// Seed from the runner's isolated context.
    pub fn from_context(ctx: &crate::agents::IsolatedContext) -> Self {
        WorkerSeed {
            brief: ctx.brief.clone(),
            snippets: ctx.snippets.clone(),
            image_urls: ctx.image_urls.clone(),
            audio_urls: ctx.audio_urls.clone(),
            recursion_granted: false,
        }
    }
}

/// What the mirror holds. One mutex per journal, never held across I/O or
/// across a registry/map lookup.
#[derive(Debug, Default)]
struct MirrorState {
    records: Vec<MessageRecord>,
    /// `record_seed` ran: the vector holds the seed (system prompt + brief +
    /// snippets block), and nothing from a live turn has been observed yet.
    seeded: bool,
    /// Engine mutations observed **after** seeding. `0` means the worker
    /// registered but never reached a live mirror point, which is exactly the
    /// `SeedOnly` case.
    live_mutations: usize,
    /// The mirror cap bit: records were refused because of
    /// `session_worker_mirror_max_messages`.
    truncated: bool,
    /// How many records were refused.
    dropped: usize,
    seed: Option<WorkerSeed>,
}

/// A worker's mirrored engine history.
///
/// `Send + Sync`: the state is a `Mutex<Vec<MessageRecord>>` plus copyable
/// identity. It is a *shadow* of the engine, never the engine — the worker's
/// own `ContextEngine` remains authoritative for execution.
#[derive(Debug)]
pub struct WorkerJournal {
    worker_key: String,
    worker_id_seq_value: u64,
    mirror_cap: usize,
    /// The run-ended flag and the close order live **outside** the history
    /// mutex: the registry pruner reads them while it holds the registry write
    /// guard, and it must never take a second journal lock in that window (no
    /// lock nested under a map guard).
    closed: AtomicBool,
    closed_seq: AtomicU64,
    state: Mutex<MirrorState>,
}

impl WorkerJournal {
    fn lock(&self) -> MutexGuard<'_, MirrorState> {
        // Sanctioned poisoning recovery: the state is a `Vec` plus counters, and
        // a panicked holder left it structurally consistent. Recovering it loses
        // at most one mirrored record and never produces a wrong bundle.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The registry key this journal mirrors.
    pub fn worker_key(&self) -> &str {
        &self.worker_key
    }

    /// The `WORKER_ID_SEQ` value drawn when the worker registered.
    pub fn worker_id_seq_value(&self) -> u64 {
        self.worker_id_seq_value
    }

    pub fn mirror_cap(&self) -> usize {
        self.mirror_cap
    }

    /// Seed the mirror from the engine the runner just built (system prompt +
    /// brief), together with the delegation seed. Call **once**, before the
    /// first mirrored mutation.
    pub fn record_seed(&self, engine: &ContextEngine, seed: &WorkerSeed) {
        let records: Vec<MessageRecord> = engine
            .messages()
            .iter()
            .map(MessageRecord::from_wire)
            .collect();
        let mut state = self.lock();
        let (kept, dropped) = clamp_to_cap(records, self.mirror_cap);
        state.records = kept;
        if dropped > 0 {
            state.truncated = true;
            state.dropped += dropped;
        }
        state.seeded = true;
        state.seed = Some(seed.clone());
    }

    /// Mirror one `engine.append(..)`.
    pub fn mirrored_append(&self, msg: &Message) {
        let record = MessageRecord::from_wire(msg);
        let mut state = self.lock();
        state.live_mutations += 1;
        if state.records.len() >= self.mirror_cap {
            state.truncated = true;
            state.dropped += 1;
            return;
        }
        state.records.push(record);
    }

    /// Mirror one `engine.replace_last(..)` as a **replace** of the last mirrored
    /// record, never an append: appending here would make the mirror longer than
    /// the engine and break the `pairing_shape` cross-check.
    pub fn mirrored_replace_last(&self, msg: &Message) {
        let record = MessageRecord::from_wire(msg);
        let mut state = self.lock();
        state.live_mutations += 1;
        if let Some(slot) = state.records.last_mut() {
            *slot = record;
            return;
        }
        // The engine replaced a record the mirror never saw (e.g. the run
        // started mirroring mid-life). Recording it is the faithful option;
        // dropping it would leave the mirror permanently shorter than the
        // engine.
        if state.records.len() < self.mirror_cap {
            state.records.push(record);
        } else {
            state.truncated = true;
            state.dropped += 1;
        }
    }

    /// Collapse the mirror to the engine's new vector — the compaction /
    /// `inject_rebirth_advisory` / `perform_rebirth` case: the mirror must never
    /// re-expand history the worker itself discarded.
    pub fn mirrored_collapse(&self, messages: &[Message]) {
        let records: Vec<MessageRecord> = messages.iter().map(MessageRecord::from_wire).collect();
        self.collapse_records(records);
    }

    fn collapse_records(&self, records: Vec<MessageRecord>) {
        let (kept, dropped) = clamp_to_cap(records, self.mirror_cap);
        let mut state = self.lock();
        state.live_mutations += 1;
        state.records = kept;
        if dropped > 0 {
            state.truncated = true;
            state.dropped += dropped;
        }
    }

    /// A copy of the mirrored history. In memory only.
    pub fn snapshot(&self) -> Vec<MessageRecord> {
        self.lock().records.clone()
    }

    /// Number of mirrored records, without cloning the vector.
    pub fn len(&self) -> usize {
        self.lock().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().records.is_empty()
    }

    /// Provenance of the mirrored history (`HistorySource`).
    ///
    /// * `LiveMirror` — at least one live engine mutation was mirrored.
    /// * `SeedOnly`   — only the seed was recorded: the worker registered but
    ///   never reached a live mirror point. Reported, never upgraded.
    /// * `NotCaptured`— nothing was recorded at all.
    pub fn history_source(&self) -> HistorySource {
        let state = self.lock();
        if state.live_mutations > 0 {
            HistorySource::LiveMirror
        } else if state.seeded {
            HistorySource::SeedOnly
        } else {
            HistorySource::NotCaptured
        }
    }

    /// Did the mirror cap bite? Drives the truncation notice.
    pub fn was_truncated(&self) -> bool {
        self.lock().truncated
    }

    /// How many records the cap refused.
    pub fn dropped_records(&self) -> usize {
        self.lock().dropped
    }

    /// The recorded delegation seed, if any.
    pub fn seed(&self) -> Option<WorkerSeed> {
        self.lock().seed.clone()
    }

    /// Has the run ended (mirror frozen)?
    /// The run ended: the mirror is frozen, and the entry behind it is either
    /// completed or gone.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Close order of this journal (`0` while it is still open).
    pub fn closed_seq_of(&self) -> u64 {
        self.closed_seq.load(Ordering::SeqCst)
    }

    /// The bundle triple the committer needs: history, its provenance, and the
    /// pairing state observed **from the mirror** (no second pairing rule —
    /// `DanglingToolCallState::observe` is the crate's one vocabulary).
    pub fn project_history(&self) -> (Vec<MessageRecord>, HistorySource, DanglingToolCallState) {
        let state = self.lock();
        let history = state.records.clone();
        let source = if state.live_mutations > 0 {
            HistorySource::LiveMirror
        } else if state.seeded {
            HistorySource::SeedOnly
        } else {
            HistorySource::NotCaptured
        };
        (
            history.clone(),
            source,
            DanglingToolCallState::observe(&history),
        )
    }

    /// The loud statement that history was cut short. `None` while the cap has
    /// not bitten.
    pub fn truncation_notice(&self) -> Option<NonRestorableNotice> {
        let state = self.lock();
        if !state.truncated {
            return None;
        }
        Some(NonRestorableNotice {
            kind: NonRestorableKind::WorkerHistory,
            subject: self.worker_key.clone(),
            detail: format!(
                "the mirror for {key} held at most {cap} records and refused {dropped} of them: \
                 the persisted history is truncated, not complete",
                key = self.worker_key,
                cap = self.mirror_cap,
                dropped = state.dropped,
            ),
        })
    }

    /// Freeze the mirror at end of run and keep it in [`REGISTRY`] only while it
    /// is inside the closed-journal window. Returns the journals pruned.
    pub fn close(&self) -> usize {
        if self.closed.swap(true, Ordering::SeqCst) {
            return 0;
        }
        let seq = CLOSED_SEQ.fetch_add(1, Ordering::SeqCst);
        self.closed_seq.store(seq, Ordering::SeqCst);
        prune_closed_journals()
    }
}

/// Close order of journals, for the closed-journal window.
static CLOSED_SEQ: AtomicU64 = AtomicU64::new(0);

/// RAII close: the runner has many exit paths (`return Ok(..)` on abort, replan,
/// verdict, completion, `?` on an LLM error), and a journal whose run ended but
/// was never frozen would keep being treated as live by the committer. Dropping
/// this guard freezes the mirror exactly once.
#[derive(Debug)]
pub struct JournalCloseGuard {
    journal: Arc<WorkerJournal>,
}

impl JournalCloseGuard {
    pub fn new(journal: Arc<WorkerJournal>) -> Self {
        JournalCloseGuard { journal }
    }

    /// The mirrored handle.
    pub fn journal(&self) -> &Arc<WorkerJournal> {
        &self.journal
    }
}

impl Drop for JournalCloseGuard {
    fn drop(&mut self) {
        self.journal.close();
    }
}

/// Drop the oldest **closed** journals past [`MAX_CLOSED_JOURNALS`], returning
/// how many were dropped. Live (open) journals are never dropped.
fn prune_closed_journals() -> usize {
    let mut registry = registry_write();
    let mut closed: Vec<(u64, String)> = registry
        .iter()
        .filter(|(_, journal)| journal.is_closed())
        .map(|(key, journal)| (journal.closed_seq_of(), key.clone()))
        .collect();
    if closed.len() <= MAX_CLOSED_JOURNALS {
        return 0;
    }
    closed.sort_by_key(|(seq, _)| *seq);
    let to_drop = closed.len() - MAX_CLOSED_JOURNALS;
    let mut dropped = 0usize;
    for (_, key) in closed.into_iter().take(to_drop) {
        if registry.remove(&key).is_some() {
            dropped += 1;
        }
    }
    dropped
}

/// Keep at most `cap` records (oldest first) and report how many were refused.
fn clamp_to_cap(records: Vec<MessageRecord>, cap: usize) -> (Vec<MessageRecord>, usize) {
    if records.len() <= cap {
        return (records, 0);
    }
    let dropped = records.len() - cap;
    let mut kept = records;
    kept.truncate(cap);
    (kept, dropped)
}

// ---------------------------------------------------------------------------
// Task-local handle + process-wide registry
// ---------------------------------------------------------------------------

task_local! {
    /// The journal of the worker currently executing on this task, mirroring how
    /// `CURRENT_WORKER_TOKEN` (`orchestrator::bus`) reaches code that has no
    /// parameter path.
    pub static CURRENT_WORKER_JOURNAL: Arc<WorkerJournal>;
}

/// The current worker's journal, or `None` off the worker path. `try_with`, so
/// this never panics outside a scope.
pub fn current_journal() -> Option<Arc<WorkerJournal>> {
    CURRENT_WORKER_JOURNAL
        .try_with(|journal| journal.clone())
        .ok()
}

/// Run `f` with `journal` visible to [`current_journal`].
///
/// This is how the handle reaches code that mutates the engine in **another**
/// module (`fix_loop::append_tool_result`, the notice injectors): they call the
/// `*_here` helpers, which resolve the journal here. Synchronous scope — the
/// scoped call must not await.
pub fn with_journal<T>(journal: &Arc<WorkerJournal>, f: impl FnOnce() -> T) -> T {
    CURRENT_WORKER_JOURNAL.sync_scope(journal.clone(), f)
}

/// The awaiting form of [`with_journal`], for an engine-mutating call that spans
/// `.await` points (the fix loop).
pub async fn with_journal_async<F, T>(journal: &Arc<WorkerJournal>, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CURRENT_WORKER_JOURNAL.scope(journal.clone(), fut).await
}

/// Process-wide `worker_key -> journal`, consulted by the committer and by the
/// panic path. A `RwLock<BTreeMap>` rather than a `DashMap`: the harvest order is
/// the key order (deterministic member writes), and every access here clones
/// `Arc`s and releases the guard before anything else happens.
pub static REGISTRY: LazyLock<RwLock<BTreeMap<String, Arc<WorkerJournal>>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));

fn registry_write() -> std::sync::RwLockWriteGuard<'static, BTreeMap<String, Arc<WorkerJournal>>> {
    REGISTRY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn registry_read() -> std::sync::RwLockReadGuard<'static, BTreeMap<String, Arc<WorkerJournal>>> {
    REGISTRY
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Open (and register) a worker's mirror. `mirror_cap` is
/// `Config.session_worker_mirror_max_messages`.
pub fn open(worker_key: &str, worker_id_seq_value: u64, mirror_cap: usize) -> Arc<WorkerJournal> {
    let journal = Arc::new(WorkerJournal {
        worker_key: worker_key.to_string(),
        worker_id_seq_value,
        mirror_cap: mirror_cap.max(1),
        closed: AtomicBool::new(false),
        closed_seq: AtomicU64::new(0),
        state: Mutex::new(MirrorState::default()),
    });
    let replaced = registry_write().insert(worker_key.to_string(), journal.clone());
    if replaced.is_some() {
        tracing::warn!(
            "worker journal for '{worker_key}' was already registered; the new mirror replaces it \
             (the old history is no longer harvestable)"
        );
    }
    journal
}

/// The journal behind `worker_key`, if one is registered.
pub fn registry_get(worker_key: &str) -> Option<Arc<WorkerJournal>> {
    registry_read().get(worker_key).cloned()
}

/// Every registered journal, in key order. The guard is released before the
/// caller touches anything else.
pub fn registry_snapshot() -> Vec<Arc<WorkerJournal>> {
    registry_read().values().cloned().collect()
}

/// Keys currently mirrored.
pub fn registry_keys() -> Vec<String> {
    registry_read().keys().cloned().collect()
}

pub fn registry_len() -> usize {
    registry_read().len()
}

/// Remove a journal from the registry (the committer calls this for keys absent
/// from `WORKERS`).
pub fn unregister(worker_key: &str) -> Option<Arc<WorkerJournal>> {
    registry_write().remove(worker_key)
}

// ---------------------------------------------------------------------------
// Engine-side glue: the two calls (mirror + engine) stay in one place
// ---------------------------------------------------------------------------

/// `engine.append(msg)` mirrored. The engine stays authoritative; the mirror is
/// a shadow that must never be able to change what the worker does.
pub fn mirror_append(engine: &mut ContextEngine, journal: Option<&WorkerJournal>, msg: Message) {
    if let Some(journal) = journal {
        journal.mirrored_append(&msg);
    }
    engine.append(msg);
}

/// `engine.replace_last(msg)` mirrored as a replace.
pub fn mirror_replace_last(
    engine: &mut ContextEngine,
    journal: Option<&WorkerJournal>,
    msg: Message,
) {
    if let Some(journal) = journal {
        journal.mirrored_replace_last(&msg);
    }
    engine.replace_last(msg);
}

/// Collapse the mirror onto the engine after `compact()` /
/// `inject_rebirth_advisory()` / `perform_rebirth()`.
pub fn mirror_collapse(engine: &ContextEngine, journal: Option<&WorkerJournal>) {
    if let Some(journal) = journal {
        journal.mirrored_collapse(engine.messages());
    }
}

/// Same, resolved from the task-local — the form used by engine mutation sites
/// that have no journal parameter (`runner::fix_loop`).
pub fn mirror_append_here(engine: &mut ContextEngine, msg: Message) {
    mirror_append(engine, current_journal().as_deref(), msg);
}

/// Task-local form of [`mirror_replace_last`].
pub fn mirror_replace_last_here(engine: &mut ContextEngine, msg: Message) {
    mirror_replace_last(engine, current_journal().as_deref(), msg);
}

/// Task-local form of [`mirror_collapse`].
pub fn mirror_collapse_here(engine: &ContextEngine) {
    mirror_collapse(engine, current_journal().as_deref());
}

// ---------------------------------------------------------------------------
// Capture-side projection: live entry + journal -> WorkerSection
// ---------------------------------------------------------------------------

/// The lifecycle class a live entry projects to (the label is display bytes,
/// `status` is control state; an unrecognized label is `Unknown`, never a coerced
/// class).
pub fn project_status(state: &WorkerState) -> WorkerStatus {
    if state.completed_seq.is_none() {
        return WorkerStatus::InProgress;
    }
    match state.info.status.as_str() {
        "Completed" => WorkerStatus::Completed,
        label if label.starts_with("Failed") => WorkerStatus::Failed,
        label if label.starts_with("Aborted") => WorkerStatus::Aborted,
        "Approved" | "Approved (validated)" => WorkerStatus::Completed,
        _ => WorkerStatus::Unknown,
    }
}

/// Project one live registry entry (+ its journal) into the durable
/// `WorkerSection`. Everything the entry cannot carry becomes a
/// `NonRestorableNotice`, and every `Instant` becomes a `TimePoint` through
/// `session::time`.
///
/// `last_activity` is `None`: the live registry has no last-activity clock (only
/// the UI pane does), and inventing one would be a fabricated timestamp.
pub fn project_worker_section(
    worker_key: &str,
    state: &WorkerState,
    role: Agent,
    journal: Option<&WorkerJournal>,
) -> WorkerSection {
    let (history, history_source, dangling) = journal
        .map(|journal| journal.project_history())
        .unwrap_or_else(|| {
            (
                Vec::new(),
                HistorySource::NotCaptured,
                DanglingToolCallState::Balanced,
            )
        });
    let seed = journal
        .and_then(|journal| journal.seed())
        .unwrap_or_default();
    let brief = if seed.brief.is_empty() {
        state.info.prompt.clone()
    } else {
        seed.brief.clone()
    };

    let mut non_restorable = Vec::new();
    if let Some(notice) = journal.and_then(WorkerJournal::truncation_notice) {
        non_restorable.push(notice);
    }
    // A steering notice queued for this worker at the capture instant dies with
    // the process, and the loader of a later process cannot read a dead process's
    // notice store — so it is named here, one entry per queued notice. Sampling
    // only: the inbox is never drained by capture.
    non_restorable.extend(pending_notice_notices(worker_key));
    if history_source != HistorySource::LiveMirror {
        non_restorable.push(NonRestorableNotice {
            kind: NonRestorableKind::WorkerHistory,
            subject: worker_key.to_string(),
            detail: format!(
                "history for {worker_key} is {history_source:?}: the mirrored transcript was \
                 never observed, so the persisted record carries the brief only",
            ),
        });
    }

    WorkerSection {
        worker_key: worker_key.to_string(),
        worker_id_seq_value: state.worker_id_seq_value,
        role,
        task_id: state.info.task_id.clone(),
        status: project_status(state),
        status_label: state.info.status.clone(),
        started: TimePoint::from_instant_with_wall(state.info.started_at, state.info.started_wall),
        completed: state
            .completed_at
            .zip(state.completed_wall)
            .map(|(instant, wall)| TimePoint::from_instant_with_wall(instant, wall)),
        last_activity: None,
        worked_secs: state.info.started_at.elapsed().as_secs_f64(),
        completed_seq: state.completed_seq,
        context_tokens: state.info.context_tokens,
        last_tokens: state.last_tokens,
        counters: WorkerCounters {
            implementation_turns: state.info.implementation_turns,
            validation_rounds: state.info.validation_rounds,
            latest_validator_feedback: state.info.latest_validator_feedback.clone(),
        },
        brief,
        snippets: seed.snippets,
        image_urls: seed.image_urls,
        audio_urls: seed.audio_urls,
        recursion_granted: seed.recursion_granted,
        history,
        history_source,
        dangling,
        unrecoverable_reason: None,
        non_restorable,
    }
}

// ---------------------------------------------------------------------------
// Restore: identity floors, the `WORKERS` rebuild, status projections
// ---------------------------------------------------------------------------

/// What installing restored workers did. Mirrors the worker half of
/// `RepairReport` so the boot caller can print it and never drop it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkerRestoreReport {
    /// Entries inserted into `WORKERS`.
    pub installed: usize,
    /// Keys that were in flight before the restart and are installed as open,
    /// interrupted delegations.
    pub interrupted_keys: Vec<String>,
    /// Keys the loader marked `Unrecoverable` (the one tolerated partial).
    pub unrecoverable_keys: Vec<String>,
    /// Keys with at least one `dispatched` dangling call: a tool may already have
    /// applied its effect.
    pub side_effect_risk_keys: Vec<String>,
    /// Completed entries the live ring could not hold.
    pub completed_dropped_by_ring_cap: usize,
    /// Insertions refused because the key is held by a live entry (the loader
    /// never clobbers a cancellation token).
    pub rejected_keys: Vec<String>,
    /// Every D5 capability that did not survive.
    pub notices: Vec<NonRestorableNotice>,
    /// Loud, human-readable deviations (floor drift, `Unknown` labels,
    /// status/`completed_seq` repair, key collisions).
    pub warnings: Vec<String>,
}

impl WorkerRestoreReport {
    /// Union, deduplicated by `(kind, subject)`.
    pub fn push_notice(&mut self, notice: NonRestorableNotice) {
        let key = notice.dedup_key();
        if !self.notices.iter().any(|n| n.dedup_key() == key) {
            self.notices.push(notice);
        }
    }
}

/// Raise the three identity floors, then re-insert every restored worker under
/// its stored key, then apply the completed-ring cap (in that order).
///
/// **No worker is relaunched here.** In-flight entries come back as open,
/// interrupted delegations with a fresh cancellation token; the manager is told
/// by [`restore_notice`] and decides.
pub fn install_restored_workers(
    sections: &[WorkerSection],
    floors: &IdentityFloors,
) -> WorkerRestoreReport {
    let mut report = WorkerRestoreReport::default();

    // Step 1 — floors first, so nothing below a floor can be re-issued while
    // step 2 runs. `fetch_max`: never lower, never assign.
    let previous_worker_ids = workers::seed_worker_id_seq_floor(floors.worker_id_seq_floor);
    let previous_completed = workers::seed_completed_seq_floor(floors.completed_seq_floor);
    let previous_notices =
        crate::orchestrator::notice::seed_notice_id_floor(floors.notice_id_floor);
    if previous_worker_ids > floors.worker_id_seq_floor {
        report.warnings.push(format!(
            "WORKER_ID_SEQ was already at {previous_worker_ids}, above the bundle floor {} — \
             this process issued ids the bundle never saw",
            floors.worker_id_seq_floor
        ));
    }
    if previous_completed > floors.completed_seq_floor {
        report.warnings.push(format!(
            "COMPLETED_SEQ was already at {previous_completed}, above the bundle floor {}",
            floors.completed_seq_floor
        ));
    }
    if previous_notices > floors.notice_id_floor {
        report.warnings.push(format!(
            "NEXT_NOTICE_ID was already at {previous_notices}, above the bundle floor {}",
            floors.notice_id_floor
        ));
    }

    // Step 2 — insert under the stored key. Sorted by key for a deterministic
    // report; every call takes one short map guard and releases it.
    for section in sections {
        let outcome = install_one_worker(section, &mut report);
        // A refused insertion installed nothing, so it must never be counted.
        let installed = matches!(
            outcome,
            Some(InstallOutcome::Interrupted)
                | Some(InstallOutcome::Terminal)
                | Some(InstallOutcome::Unrecoverable)
        );
        if installed {
            report.installed += 1;
        }
        if matches!(outcome, Some(InstallOutcome::Interrupted)) {
            report.interrupted_keys.push(section.worker_key.clone());
        }
        if matches!(outcome, Some(InstallOutcome::Unrecoverable)) {
            report.unrecoverable_keys.push(section.worker_key.clone());
        }
    }

    // Step 3 — the live map is a bounded working set: the completed ring drops
    // oldest-first past its cap, and that count is reported, not hidden.
    report.completed_dropped_by_ring_cap = workers::enforce_completed_ring_cap();
    report
}

enum InstallOutcome {
    Interrupted,
    Terminal,
    Unrecoverable,
    Rejected,
}

fn install_one_worker(
    section: &WorkerSection,
    report: &mut WorkerRestoreReport,
) -> Option<InstallOutcome> {
    let key = section.worker_key.clone();

    // `inProgress <=> completed_seq == None`. A terminal class with no stored
    // sequence is a contradiction; it is closed through the *existing* completion
    // path (never a re-derivation of the guard's Drop) and the deviation is
    // reported.
    let state = restored_worker_state(section);
    if !workers::install_restored_worker(key.clone(), state) {
        report.rejected_keys.push(key.clone());
        report.warnings.push(format!(
            "restored worker '{key}' was not installed: a live entry already owns that key, and \
             overwriting it would drop that worker's cancellation token"
        ));
        report.push_notice(NonRestorableNotice {
            kind: NonRestorableKind::Other,
            subject: key.clone(),
            detail: format!(
                "the restored record for {key} could not be installed because a live worker owns \
                 that key; the persisted history stays in the bundle and was not merged"
            ),
        });
        return Some(InstallOutcome::Rejected);
    }

    let dispatched = dispatched_calls(section);
    if !dispatched.is_empty() {
        report.side_effect_risk_keys.push(key.clone());
        report.push_notice(NonRestorableNotice {
            kind: NonRestorableKind::PtyProcess,
            subject: key.clone(),
            detail: format!(
                "{key} had {} dispatched tool call(s) ({}) whose PTY may have survived the kill \
                 and may still be mutating the workspace — re-applying them is a double-apply",
                dispatched.len(),
                dispatched.join(", ")
            ),
        });
    }

    match section.status {
        WorkerStatus::InProgress | WorkerStatus::Unknown => {
            // Keep the entry open (`completed_seq: None`) so
            // `has_active_workers()` and every cancel path still see it, and label
            // it as interrupted. A fresh token is minted here (minting is permitted
            // for the *interrupted* projection) so a later `/abort` cannot be
            // confused by a `None` token — and the old token is named as lost.
            workers::set_active_worker_status(&key, INTERRUPTED_STATUS_LABEL);
            report.push_notice(NonRestorableNotice {
                kind: NonRestorableKind::CancellationToken,
                subject: key.clone(),
                detail: format!(
                    "the cancellation token of {key} did not survive; a fresh token was attached, \
                     so cancelling this entry cancels the placeholder, not the old task"
                ),
            });
            report.push_notice(NonRestorableNotice {
                kind: NonRestorableKind::JoinHandle,
                subject: key.clone(),
                detail: format!(
                    "no task exists for {key}: its execution ended with the previous process and \
                     no result will ever arrive for its pending calls"
                ),
            });
            report.push_notice(NonRestorableNotice {
                kind: NonRestorableKind::DropGuard,
                subject: key.clone(),
                detail: format!(
                    "{key} has no completion guard; closing its record goes through \
                     retire_interrupted_worker, which draws COMPLETED_SEQ through the existing path"
                ),
            });
            if section.status == WorkerStatus::Unknown {
                report.warnings.push(format!(
                    "restored worker '{key}' has the unrecognized status label '{}' (kept \
                     verbatim); it is treated as an open delegation",
                    section.status_label
                ));
            }
            if section.history_source != HistorySource::LiveMirror {
                report.push_notice(NonRestorableNotice {
                    kind: NonRestorableKind::WorkerHistory,
                    subject: key.clone(),
                    detail: format!(
                        "the history of {key} is {:?}, not a live mirror: what was persisted is \
                         the seed, and no full transcript can be claimed for it",
                        section.history_source
                    ),
                });
            }
            if section.status == WorkerStatus::InProgress {
                Some(InstallOutcome::Interrupted)
            } else {
                Some(InstallOutcome::Terminal)
            }
        }
        WorkerStatus::Unrecoverable => {
            // The tolerated partial: the pane identity survives, the history does
            // not. Keep it non-active (`is_active: false`).
            if section.completed_seq.is_none() {
                workers::retire_restored_worker(&key, &section.status_label);
                report.warnings.push(format!(
                    "unrecoverable worker '{key}' carried no completed_seq; it was closed through \
                     the existing completion path so it cannot be mistaken for an active worker"
                ));
            }
            report.push_notice(NonRestorableNotice {
                kind: NonRestorableKind::Other,
                subject: key.clone(),
                detail: format!(
                    "the worker file for {key} failed verification{}: its name and task are kept, \
                     its history is gone",
                    section
                        .unrecoverable_reason
                        .as_ref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                ),
            });
            report.push_notice(NonRestorableNotice {
                kind: NonRestorableKind::WorkerHistory,
                subject: key.clone(),
                detail: format!("{key} is Unrecoverable: no message history was restorable"),
            });
            Some(InstallOutcome::Unrecoverable)
        }
        WorkerStatus::Completed | WorkerStatus::Failed | WorkerStatus::Aborted => {
            if section.completed_seq.is_none() {
                workers::retire_restored_worker(&key, &section.status_label);
                report.warnings.push(format!(
                    "restored worker '{key}' had status {:?} but no completed_seq; it was closed \
                     through the existing completion path",
                    section.status
                ));
            }
            // Nothing extra is reported: the manager's own transcript already
            // carries this worker's result.
            Some(InstallOutcome::Terminal)
        }
    }
}

/// Build the live entry for a restored section: identity, counters and clocks
/// verbatim, `Instant`s re-anchored through `session::time`, and a fresh
/// cancellation token (the old one is reported as lost, never re-minted
/// silently).
fn restored_worker_state(section: &WorkerSection) -> WorkerState {
    WorkerState {
        info: ActiveWorkerInfo {
            task_id: section.task_id.clone(),
            agent_name: section.role.as_str().to_string(),
            prompt: section.brief.clone(),
            // The only sanctioned re-anchor, the same trade-off
            // `Plan::get_plan_start_time()` makes (suspend/hibernate and wall
            // steps make the delta wrong by the size of the discontinuity).
            started_at: section.started.to_instant(),
            started_wall: section.started.wall,
            context_tokens: section.context_tokens,
            implementation_turns: section.counters.implementation_turns,
            validation_rounds: section.counters.validation_rounds,
            latest_validator_feedback: section.counters.latest_validator_feedback.clone(),
            status: section.status_label.clone(),
            cancel_token: Some(crate::orchestrator::bus::global_cancellation_token().child_token()),
        },
        // Clocks are carried, durations stay numbers; nothing here is computed as
        // a difference of two re-anchored instants.
        last_tokens: section.last_tokens,
        completed_seq: section.completed_seq,
        completed_at: section.completed.as_ref().map(TimePoint::to_instant),
        completed_wall: section.completed.as_ref().map(|point| point.wall),
        worker_id_seq_value: section.worker_id_seq_value,
        // Queue state is **not** persisted and is deliberately not carried here: a
        // queue rank belongs to a slot wait inside one process's pool, and a restored
        // worker that queues again gets a fresh report from the pool (mirrored onto
        // this entry by `set_worker_queue_position`), which is what the panel's
        // projection re-reads. Nothing stale survives the round-trip.
        queue_position: None,
    }
}

/// `tool_call_id (function)` for every `dispatched` dangling call.
fn dispatched_calls(section: &WorkerSection) -> Vec<String> {
    match &section.dangling {
        DanglingToolCallState::Dangling { calls } => calls
            .iter()
            .filter(|call| call.dispatched)
            .map(|call| format!("{} ({})", call.function_name, call.tool_call_id))
            .collect(),
        _ => Vec::new(),
    }
}

/// Close a restored, interrupted record: the manager chose to retire the
/// delegation, re-delegate it, or continue it, so the open record must be closed
/// the way a real completion closes it — through the existing `COMPLETED_SEQ`
/// path, the ring eviction and the inbox reclaim — without ever synthesizing an
/// `ActiveWorkerGuard`.
pub fn retire_interrupted_worker(worker_key: &str) -> Option<u64> {
    workers::retire_restored_worker(worker_key, "Retired after restart")
}

/// One `NonRestorableNotice` **per notice** sitting in the live inbox of
/// `worker_key` at the capture instant.
///
/// The notice queue is process memory: what is queued here when the process dies
/// can never be read back by the loader, so capture is the only instant at which
/// it can be recorded. `subject` is the worker key (the same subject the
/// `CancellationToken`/`JoinHandle`/`DropGuard` entries use) and `detail` names
/// the **notice id**, so an operator can see which steering input was lost.
/// Read-only by construction: [`crate::orchestrator::notice::peek_worker_inbox`]
/// copies the queue and the caller never drains or expires it.
pub fn pending_notice_notices(worker_key: &str) -> Vec<NonRestorableNotice> {
    crate::orchestrator::notice::peek_worker_inbox(worker_key)
        .into_iter()
        .map(|notice| NonRestorableNotice {
            kind: NonRestorableKind::PendingNotice,
            subject: worker_key.to_string(),
            detail: format!(
                "steering notice {} was queued for {worker_key} at capture time and was never \
                 delivered: it lived in the memory of the process that ended, and the notice id \
                 counter was preserved so the id is not reused",
                notice.notice_id
            ),
        })
        .collect()
}

/// Synthesises one `NonRestorableNotice` **per unit of `count`** for the notices
/// the previous process had queued, undelivered, for `worker_key`: the loader
/// cannot read the old process's notice store, so the caller (the boot wiring)
/// supplies the count it captured. Unlike [`pending_notice_notices`], which
/// recorded one entry per real notice (naming its notice id) at capture time,
/// nothing here names an id.
///
/// As the body reads, `(0..count.max(1))` means `count = 0` still yields **ONE**
/// entry and every entry for one key is byte-identical; `push_repair_notice`
/// dedups on [`NonRestorableNotice::dedup_key`] = `(kind, subject)` with `subject`
/// always `worker_key`, so N notices for one worker reach the report as **one
/// line per distinct worker key**.
pub fn queued_notice_notices(worker_key: &str, count: usize) -> Vec<NonRestorableNotice> {
    (0..count.max(1))
        .map(|_| NonRestorableNotice {
            kind: NonRestorableKind::PendingNotice,
            subject: worker_key.to_string(),
            detail: format!(
                "a steering notice queued for {worker_key} in the previous process was never \
                 delivered and its queue did not survive the restart"
            ),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The single synthetic manager notice
// ---------------------------------------------------------------------------

/// Exactly one synthetic notice for the manager, summarizing what was
/// interrupted. Every field is a field of `WorkerSection` or of `DanglingCall`:
/// the notice never invents status it did not read.
///
/// Its first line starts with `session::format::SESSION_RESTORE_NOTICE_PREFIX` so
/// the manager grammar recognizes it as its own injection. The **caller** (the
/// boot wiring) appends it to the restored manager transcript — the loader must
/// not re-enter the engine, and the message must land after the normalized vector
/// and before any new user input.
pub fn restore_notice(
    session_id: &str,
    stored_ending: &ProcessEnding,
    derived_ending: &ProcessEnding,
    sections: &[WorkerSection],
    report: &WorkerRestoreReport,
) -> String {
    let mut out = String::new();
    out.push_str(crate::session::format::SESSION_RESTORE_NOTICE_PREFIX);
    out.push_str(&format!(
        "). This process restarted from its session bundle (session id {session_id}, previous \
         ending: {derived_ending:?}, stored ending: {stored_ending:?}). "
    ));
    if report.interrupted_keys.is_empty() {
        out.push_str("No specialist worker was in flight when the previous process stopped.");
        return out;
    }
    out.push_str(
        "The work below was in flight when the previous process stopped. Its history is restored; \
         its execution is NOT running — no task exists to continue it and no result will ever \
         arrive for its pending calls.",
    );

    for (index, section) in sections.iter().enumerate() {
        let interrupted = report.interrupted_keys.contains(&section.worker_key);
        let status_suffix = if interrupted {
            format!(" -> {INTERRUPTED_STATUS_LABEL}")
        } else {
            String::new()
        };
        let turns = format!("{} turns", section.counters.implementation_turns);
        out.push_str(&format!(
            "\n\n  [{}] {:<22} status: {}{} | {} | {} validation rounds | {} ctx tokens | worked {}",
            index + 1,
            section.worker_key,
            section.status_label,
            status_suffix,
            turns,
            group_thousands(section.counters.validation_rounds),
            group_thousands(section.context_tokens),
            workers::format_duration_human(section.worked_secs.max(0.0) as u64),
        ));
        if let Some(task_id) = &section.task_id {
            out.push_str(&format!(" | task {task_id}"));
        }
        out.push_str("\n      ");
        out.push_str(&history_line(section));
        if let Some(pending) = pending_calls_line(section) {
            out.push_str("\n      ");
            out.push_str(&pending);
        }
        let lost = lost_kinds_for(section, report);
        if !lost.is_empty() {
            out.push_str(&format!("\n      lost: {}", lost.join(", ")));
        }
        if interrupted {
            out.push_str(
                "\n      -> do NOT assume the task is finished; do NOT re-run a dispatched command blindly",
            );
        }
    }

    out.push_str("\n\n");
    let blocked = &report.side_effect_risk_keys;
    if blocked.is_empty() {
        out.push_str(
            "To continue an interrupted item, either delegate the task again from a fresh brief, \
             or continue it from its restored history. Decide from the current workspace state, \
             not from this notice.",
        );
    } else {
        out.push_str(
            "Items whose tool call was dispatched cannot be continued safely — the call may \
             already have applied changes. For those, retire the record or delegate again from a \
             fresh brief. Decide from the current workspace state, not from this notice.",
        );
    }
    out
}

/// [`restore_notice`] wrapped as the single `Message::User` the manager reads (a
/// `User` insert at index >= 2 cannot disturb the pins; a `System` insert would be
/// a re-role).
pub fn restore_notice_message(
    session_id: &str,
    stored_ending: &ProcessEnding,
    derived_ending: &ProcessEnding,
    sections: &[WorkerSection],
    report: &WorkerRestoreReport,
) -> Message {
    Message::User {
        content: restore_notice(session_id, stored_ending, derived_ending, sections, report),
    }
}

fn history_line(section: &WorkerSection) -> String {
    match section.status {
        WorkerStatus::Unrecoverable => format!(
            "history: unrecoverable ({})",
            section
                .unrecoverable_reason
                .as_deref()
                .unwrap_or("verification failed")
        ),
        _ => match section.history_source {
            HistorySource::LiveMirror => format!(
                "history: restored ({} messages, live mirror)",
                group_thousands(section.history.len())
            ),
            HistorySource::SeedOnly => format!(
                "history: NOT captured — brief and snippets only ({:?}); the worker registered \
                 but no live turn was mirrored",
                section.history_source
            ),
            HistorySource::NotCaptured => {
                format!("history: NOT captured ({:?})", section.history_source)
            }
        },
    }
}

fn pending_calls_line(section: &WorkerSection) -> Option<String> {
    match &section.dangling {
        DanglingToolCallState::Dangling { calls } if !calls.is_empty() => Some(format!(
            "pending tool calls: {}",
            calls
                .iter()
                .map(describe_call)
                .collect::<Vec<_>>()
                .join("; ")
        )),
        DanglingToolCallState::Healed {
            aborted_tool_call_ids,
        } if !aborted_tool_call_ids.is_empty() => Some(format!(
            "pending tool calls: healed before capture ({})",
            aborted_tool_call_ids.join(", ")
        )),
        DanglingToolCallState::OrphanedResults { tool_call_ids } if !tool_call_ids.is_empty() => {
            Some(format!(
                "orphaned tool results: {}",
                tool_call_ids.join(", ")
            ))
        }
        _ => None,
    }
}

fn describe_call(call: &DanglingCall) -> String {
    format!(
        "{} (dispatched: {}){}",
        call.function_name,
        if call.dispatched { "yes" } else { "no" },
        if call.dispatched {
            " -> the command may have run"
        } else {
            ""
        }
    )
}

/// The `lost:` clause for one worker, read from the notices actually emitted
/// (never a fixed list — a completed worker gets no clause).
fn lost_kinds_for(section: &WorkerSection, report: &WorkerRestoreReport) -> Vec<String> {
    let mut kinds: Vec<String> = report
        .notices
        .iter()
        .filter(|notice| notice.subject == section.worker_key)
        .map(|notice| format!("{:?}", notice.kind))
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

/// `18 431`-style digit grouping, so the totals in the notice read alike.
fn group_thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::format::TimePoint;
    use crate::tool_names::TOOL_RUN_COMMAND;
    use chrono::Local;

    /// The `REGISTRY` is process-global and the closed-journal pruner looks at
    /// every entry in it, so registry-facing tests must not interleave.
    static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn specialist_engine(max_context_tokens: usize) -> ContextEngine {
        crate::manager::ContextEngineFactory::new(max_context_tokens)
            .specialist_context("role prompt".to_string(), "brief text".to_string())
    }

    fn engine_records(engine: &ContextEngine) -> Vec<MessageRecord> {
        engine
            .messages()
            .iter()
            .map(MessageRecord::from_wire)
            .collect()
    }

    fn seed() -> WorkerSeed {
        WorkerSeed {
            brief: "brief text".to_string(),
            snippets: vec!["snippet-a".to_string()],
            ..Default::default()
        }
    }

    /// Arguments deliberately left **unbalanced**: the mirror must copy them
    /// verbatim, never sanitize or re-canonicalize them.
    fn tool_call() -> crate::types::ToolCall {
        crate::types::ToolCall {
            id: "call-1".to_string(),
            kind: "function".to_string(),
            function: crate::types::ToolFunction {
                name: TOOL_RUN_COMMAND.to_string(),
                arguments: "{\"command\":\"ls -la\"".to_string(),
            },
        }
    }

    fn section(
        key: &str,
        worker_id_seq_value: u64,
        status: WorkerStatus,
        status_label: &str,
        completed_seq: Option<u64>,
    ) -> WorkerSection {
        WorkerSection {
            worker_key: key.to_string(),
            worker_id_seq_value,
            role: Agent::Coder,
            task_id: Some("t-010".to_string()),
            status,
            status_label: status_label.to_string(),
            started: TimePoint {
                elapsed_secs: 1.0,
                wall: Local::now(),
            },
            completed: None,
            last_activity: None,
            worked_secs: 42.0,
            completed_seq,
            context_tokens: 1234,
            last_tokens: 0,
            counters: WorkerCounters {
                implementation_turns: 3,
                validation_rounds: 1,
                latest_validator_feedback: None,
            },
            brief: "restore brief".to_string(),
            snippets: Vec::new(),
            image_urls: Vec::new(),
            audio_urls: Vec::new(),
            recursion_granted: false,
            history: Vec::new(),
            history_source: HistorySource::SeedOnly,
            dangling: DanglingToolCallState::Balanced,
            unrecoverable_reason: None,
            non_restorable: Vec::new(),
        }
    }

    fn live_state(status_label: &str, completed_seq: Option<u64>) -> WorkerState {
        WorkerState {
            info: ActiveWorkerInfo {
                task_id: Some("t-010".to_string()),
                agent_name: "coder".to_string(),
                prompt: "restore brief".to_string(),
                started_at: std::time::Instant::now(),
                started_wall: Local::now(),
                context_tokens: 1234,
                implementation_turns: 3,
                validation_rounds: 1,
                latest_validator_feedback: None,
                status: status_label.to_string(),
                cancel_token: Some(
                    crate::orchestrator::bus::global_cancellation_token().child_token(),
                ),
            },
            last_tokens: 0,
            completed_seq,
            completed_at: None,
            completed_wall: None,
            worker_id_seq_value: 3,
            queue_position: None,
        }
    }

    // (a) the mirror follows the engine, mutation for mutation

    #[test]
    fn mirrored_append_and_replace_last_track_the_engine_exactly() {
        let _registry = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut engine = specialist_engine(64_000);
        let journal = open("coder-t010-mirror", 3, 128);

        journal.record_seed(&engine, &seed());
        assert_eq!(
            journal.snapshot(),
            engine_records(&engine),
            "the seed must mirror exactly what the engine already held"
        );
        assert_eq!(journal.history_source(), HistorySource::SeedOnly);

        mirror_append(
            &mut engine,
            Some(&journal),
            Message::Assistant {
                content: Some("working".to_string()),
                reasoning_content: Some("thinking".to_string()),
                tool_calls: vec![tool_call()],
            },
        );
        mirror_append(
            &mut engine,
            Some(&journal),
            Message::Tool {
                tool_call_id: "call-1".to_string(),
                content: "ok".to_string(),
            },
        );
        let before_replace = journal.snapshot();
        let mirrored_args = before_replace
            .iter()
            .find_map(|record| match record {
                MessageRecord::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                    Some(tool_calls[0].function.arguments.clone())
                }
                _ => None,
            })
            .expect("the assistant record with its tool call must be mirrored");
        assert_eq!(
            mirrored_args, "{\"command\":\"ls -la\"",
            "malformed arguments must be copied verbatim, never sanitized"
        );

        // `replace_last` must REPLACE: the mirror may never end up longer than
        // the engine (that would break the pairing cross-check).
        let len_before = journal.len();
        mirror_replace_last(
            &mut engine,
            Some(&journal),
            Message::Assistant {
                content: Some("[Generation truncated]".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
            },
        );
        assert_eq!(journal.len(), len_before, "replace must not append");
        mirror_append(
            &mut engine,
            Some(&journal),
            Message::User {
                content: "SYSTEM NOTICE".to_string(),
            },
        );

        assert_eq!(journal.len(), engine.messages().len());
        assert_eq!(journal.snapshot(), engine_records(&engine));
        assert_eq!(journal.history_source(), HistorySource::LiveMirror);
        assert!(!journal.was_truncated());
        journal.close();
        unregister("coder-t010-mirror");
    }

    #[test]
    fn mirrored_collapse_follows_a_real_compaction() {
        let _registry = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Small enough budget that the tail crosses the 90% compaction trigger.
        let mut engine = specialist_engine(400);
        let journal = open("coder-t010-compact", 4, 512);
        journal.record_seed(&engine, &seed());
        for index in 0..12 {
            mirror_append(
                &mut engine,
                Some(&journal),
                Message::User {
                    content: format!("{index} {}", "lorem ipsum dolor sit amet ".repeat(30)),
                },
            );
        }
        assert_eq!(journal.len(), engine.messages().len());
        let messages_before = engine.messages().len();
        assert!(
            engine.should_compact(),
            "the fixture must actually cross the compaction trigger"
        );

        engine.compact();
        mirror_collapse(&engine, Some(&journal));

        assert!(
            engine.messages().len() < messages_before,
            "the engine must have dropped transcript for this test to mean anything"
        );
        assert_eq!(
            journal.snapshot(),
            engine_records(&engine),
            "the mirror must collapse to the engine's new vector"
        );
        journal.close();
        unregister("coder-t010-compact");
    }

    #[test]
    fn the_mirror_is_bounded_by_the_configured_cap_and_reports_the_cut() {
        let _registry = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut engine = specialist_engine(64_000);
        let journal = open("coder-t010-cap", 5, 3);
        journal.record_seed(&engine, &seed());
        for index in 0..5 {
            mirror_append(
                &mut engine,
                Some(&journal),
                Message::User {
                    content: format!("turn {index}"),
                },
            );
        }

        let all = engine_records(&engine);
        assert_eq!(journal.len(), 3, "the cap is a hard bound");
        assert_eq!(
            journal.snapshot(),
            all[..3],
            "what is kept must stay the oldest records in order"
        );
        assert!(journal.was_truncated());
        assert_eq!(journal.dropped_records(), 4);
        let notice = journal
            .truncation_notice()
            .expect("a truncated mirror must announce itself");
        assert_eq!(notice.kind, NonRestorableKind::WorkerHistory);
        assert!(
            notice.detail.contains("refused 4"),
            "the notice must carry the count: {}",
            notice.detail
        );
        journal.close();
        unregister("coder-t010-cap");
    }

    // (d) the `SeedOnly` path reports honestly

    #[test]
    fn a_seed_only_mirror_is_never_presented_as_a_live_mirror() {
        let _registry = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let engine = specialist_engine(64_000);
        let journal = open("coder-t010-seedonly", 6, 64);
        journal.record_seed(&engine, &seed());

        assert_eq!(journal.history_source(), HistorySource::SeedOnly);
        let (history, source, dangling) = journal.project_history();
        assert_eq!(source, HistorySource::SeedOnly);
        assert_eq!(history, engine_records(&engine));
        assert_eq!(dangling, DanglingToolCallState::Balanced);
        assert!(!journal.was_truncated());

        // The projection says so out loud.
        let state = live_state("In Progress", None);
        let projected =
            project_worker_section("coder-t010-seedonly", &state, Agent::Coder, Some(&journal));
        assert_eq!(projected.history_source, HistorySource::SeedOnly);
        assert!(
            projected.non_restorable.iter().any(|notice| {
                notice.kind == NonRestorableKind::WorkerHistory
                    && notice.detail.contains("SeedOnly")
            }),
            "a seed-only history must produce a loud NonRestorableNotice: {:?}",
            projected.non_restorable
        );
        journal.close();
        unregister("coder-t010-seedonly");
    }

    // status projections for non-serializable / label-only state

    #[test]
    fn status_and_time_projections_follow_the_design_rules() {
        let mut state = live_state("Revising (rejected pass 2/5)", None);
        state.completed_seq = None;
        state.info.status = "Revising (rejected pass 2/5)".to_string();
        assert_eq!(project_status(&state), WorkerStatus::InProgress);
        state.completed_seq = Some(1);
        assert_eq!(project_status(&state), WorkerStatus::Unknown);
        state.info.status = "Completed".to_string();
        assert_eq!(project_status(&state), WorkerStatus::Completed);
        state.info.status = "Approved".to_string();
        assert_eq!(project_status(&state), WorkerStatus::Completed);
        state.info.status = "Failed: validator critique".to_string();
        assert_eq!(project_status(&state), WorkerStatus::Failed);
        state.info.status = "Aborted".to_string();
        assert_eq!(project_status(&state), WorkerStatus::Aborted);
    }

    // (b) restore rebuilds WORKERS with identical keys and monotonic floors

    #[test]
    fn restore_rebuilds_workers_with_identical_keys_and_monotonic_id_floors() {
        let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let base_ids = workers::worker_id_seq_floor();
        let base_completed = workers::completed_seq_floor();
        let base_notices = crate::orchestrator::notice::notice_id_floor();

        let interrupted_key = "coder-t010-restore-a";
        let disambiguated_key = "coder-t010-restore-a#7";
        let completed_key = "coder-t010-restore-b";
        let sections = vec![
            section(
                interrupted_key,
                base_ids + 3,
                WorkerStatus::InProgress,
                "In Progress",
                None,
            ),
            section(
                disambiguated_key,
                base_ids + 11,
                WorkerStatus::InProgress,
                "Revising (rejected pass 1/5)",
                None,
            ),
            section(
                completed_key,
                base_ids + 7,
                WorkerStatus::Completed,
                "Completed",
                Some(base_completed + 2),
            ),
        ];
        let floors = IdentityFloors {
            worker_id_seq_floor: base_ids + 100,
            completed_seq_floor: base_completed + 100,
            notice_id_floor: base_notices + 100,
        };

        let report = install_restored_workers(&sections, &floors);
        assert_eq!(report.installed, 3, "{:?}", report.warnings);
        assert!(report.rejected_keys.is_empty());
        assert_eq!(
            report.interrupted_keys,
            vec![interrupted_key.to_string(), disambiguated_key.to_string()]
        );

        // Identical keys — verbatim, `#7` disambiguator included, never
        // re-derived.
        let present: Vec<String> = workers::live_worker_states()
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| key.starts_with("coder-t010-restore"))
            .collect();
        assert_eq!(
            present,
            vec![
                interrupted_key.to_string(),
                disambiguated_key.to_string(),
                completed_key.to_string()
            ],
            "keys must be rebuilt verbatim, `#7` collision handle included, in the \
             table's own key order"
        );
        let carried = workers::worker_state_snapshot(completed_key).expect("completed entry");
        assert_eq!(
            carried.completed_seq,
            Some(base_completed + 2),
            "completed_seq must be carried verbatim, never re-drawn"
        );
        assert_eq!(carried.worker_id_seq_value, base_ids + 7);
        assert_eq!(carried.info.prompt, "restore brief");
        assert!(
            !carried.is_active(),
            "a completed record must stay completed after restore"
        );

        // Floors: `fetch_max`, so they are raised and never lowered.
        assert_eq!(workers::worker_id_seq_floor(), base_ids + 100);
        assert_eq!(workers::completed_seq_floor(), base_completed + 100);
        assert_eq!(
            crate::orchestrator::notice::notice_id_floor(),
            base_notices + 100
        );

        // A worker registered **after** the restore must draw an id above every
        // id the bundle carried, and must not clobber the restored key: it gets a
        // disambiguator instead (`claim_worker_slot`).
        let guard = crate::orchestrator::workers::register_active_worker_with_token(
            Some("t010-restore-a".to_string()),
            "coder".to_string(),
            "post-restore".to_string(),
            None,
        );
        let fresh = workers::worker_state_snapshot(&guard.0).expect("fresh entry");
        assert!(
            guard.0 != interrupted_key && guard.0 != disambiguated_key,
            "the restored live key must not be overwritten: got {}",
            guard.0
        );
        assert!(
            fresh.worker_id_seq_value > base_ids + 11,
            "fresh id {} must exceed the highest carried id {}",
            fresh.worker_id_seq_value,
            base_ids + 11
        );
        let fresh_key = guard.0.clone();
        drop(guard);

        // A closing record draws a sequence above every carried one, so the
        // completed ring can never sort a new completion before an old one.
        let retired = retire_interrupted_worker(interrupted_key).expect("retired");
        assert!(
            retired > base_completed + 2,
            "drawn seq {retired} must exceed the carried seq {}",
            base_completed + 2
        );

        for key in [interrupted_key, disambiguated_key, completed_key] {
            workers::remove_worker_entry_for_test(key);
        }
        workers::remove_worker_entry_for_test(&fresh_key);
    }

    #[test]
    fn id_floors_are_raised_never_lowered() {
        let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let current = workers::worker_id_seq_floor();
        let previous = workers::seed_worker_id_seq_floor(current.saturating_sub(1));
        assert_eq!(previous, current, "fetch_max reports the previous value");
        assert_eq!(
            workers::worker_id_seq_floor(),
            current,
            "a floor below the counter must never lower it"
        );
        let raised = current + 50;
        workers::seed_worker_id_seq_floor(raised);
        assert_eq!(workers::worker_id_seq_floor(), raised);

        let completed = workers::completed_seq_floor();
        workers::seed_completed_seq_floor(completed.saturating_sub(1));
        assert_eq!(workers::completed_seq_floor(), completed);

        let notices = crate::orchestrator::notice::notice_id_floor();
        crate::orchestrator::notice::seed_notice_id_floor(notices.saturating_sub(1));
        assert_eq!(crate::orchestrator::notice::notice_id_floor(), notices);
    }

    // (c) restored workers are interrupted, and nothing is ever relaunched

    #[test]
    fn restored_workers_come_back_interrupted_and_are_never_relaunched() {
        let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = "coder-t010-interrupted";
        let base_completed = workers::completed_seq_floor();
        let sections = vec![section(
            key,
            workers::worker_id_seq_floor() + 1,
            WorkerStatus::InProgress,
            "In Progress",
            None,
        )];
        let floors = IdentityFloors {
            worker_id_seq_floor: workers::worker_id_seq_floor(),
            completed_seq_floor: base_completed,
            notice_id_floor: crate::orchestrator::notice::notice_id_floor(),
        };

        let report = install_restored_workers(&sections, &floors);
        assert_eq!(report.installed, 1);
        assert_eq!(report.interrupted_keys, vec![key.to_string()]);

        let state = workers::worker_state_snapshot(key).expect("restored entry");
        assert!(
            state.is_active(),
            "the record stays open so cancel/routing still see it"
        );
        assert_eq!(state.info.status, INTERRUPTED_STATUS_LABEL);
        assert!(
            state.completed_seq.is_none(),
            "no completion was recorded — nothing ran, so nothing completed"
        );
        assert!(
            state.info.cancel_token.is_some(),
            "a fresh token is attached (the old one is reported as lost)"
        );
        assert_eq!(
            workers::completed_seq_floor(),
            base_completed,
            "install must not draw COMPLETED_SEQ: no worker completed"
        );
        assert_eq!(
            crate::orchestrator::notice::worker_inbox_len(key),
            0,
            "install must not steer or dispatch anything to the worker"
        );

        // The lost capabilities are named, never silently swallowed.
        for kind in [
            NonRestorableKind::JoinHandle,
            NonRestorableKind::CancellationToken,
            NonRestorableKind::DropGuard,
        ] {
            assert!(
                report
                    .notices
                    .iter()
                    .any(|notice| { notice.kind == kind && notice.subject == key }),
                "missing {kind:?} projection in {:?}",
                report.notices
            );
        }

        // Re-installing the same record must not clobber the open entry.
        let again = install_restored_workers(&sections, &floors);
        assert_eq!(again.rejected_keys, vec![key.to_string()]);
        assert_eq!(again.installed, 0);

        assert!(workers::has_active_workers());
        workers::remove_worker_entry_for_test(key);
    }

    #[test]
    fn an_unrecoverable_record_is_kept_and_closed_out_of_the_active_set() {
        let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = "coder-t010-unrecoverable";
        let mut broken = section(
            key,
            workers::worker_id_seq_floor() + 1,
            WorkerStatus::Unrecoverable,
            "Unrecoverable",
            None,
        );
        broken.unrecoverable_reason = Some("fingerprint mismatch".to_string());
        let report = install_restored_workers(
            &[broken],
            &IdentityFloors {
                worker_id_seq_floor: workers::worker_id_seq_floor(),
                completed_seq_floor: workers::completed_seq_floor(),
                notice_id_floor: crate::orchestrator::notice::notice_id_floor(),
            },
        );
        assert_eq!(report.unrecoverable_keys, vec![key.to_string()]);
        let state = workers::worker_state_snapshot(key).expect("kept pane identity");
        assert!(
            !state.is_active(),
            "an Unrecoverable record must never look like an active worker"
        );
        assert!(report.notices.iter().any(|notice| {
            notice.kind == NonRestorableKind::WorkerHistory && notice.subject == key
        }));
        workers::remove_worker_entry_for_test(key);
    }

    #[test]
    fn exactly_one_restore_notice_is_produced_for_the_manager() {
        let key = "coder-t010-notice";
        let mut seed_only = section(
            key,
            7,
            WorkerStatus::InProgress,
            "Revising (rejected pass 3/5)",
            None,
        );
        seed_only.history_source = HistorySource::SeedOnly;
        seed_only.dangling = DanglingToolCallState::Dangling {
            calls: vec![DanglingCall {
                tool_call_id: "call-9".to_string(),
                function_name: TOOL_RUN_COMMAND.to_string(),
                arguments: "{\"command\":\"rm -rf target\"".to_string(),
                dispatched: true,
            }],
        };
        let report = WorkerRestoreReport {
            installed: 1,
            interrupted_keys: vec![key.to_string()],
            side_effect_risk_keys: vec![key.to_string()],
            ..Default::default()
        };
        let message = restore_notice_message(
            "session-abc",
            &ProcessEnding::InProgress,
            &ProcessEnding::UserAbort,
            &[seed_only.clone()],
            &report,
        );
        let content = match &message {
            Message::User { content } => content.clone(),
            other => panic!("N-MGR-1 must be a Message::User, got {other:?}"),
        };
        assert!(
            content.starts_with(crate::session::format::SESSION_RESTORE_NOTICE_PREFIX),
            "the manager grammar recognizes the notice by its prefix: {content}"
        );
        assert!(content.contains(key));
        assert!(content.contains(&format!("-> {INTERRUPTED_STATUS_LABEL}")));
        assert!(
            content.contains("NOT captured"),
            "SeedOnly must read honestly"
        );
        assert!(content.contains("dispatched: yes"));
        assert!(
            content.contains("cannot be continued safely"),
            "a dispatched call must be flagged as a double-apply risk"
        );
        // The loader exposes the text; it never appends it into an engine.
        assert_eq!(
            restore_notice(
                "session-abc",
                &ProcessEnding::InProgress,
                &ProcessEnding::UserAbort,
                &[seed_only],
                &report
            ),
            content
        );
    }

    // the task-local handle reaches the engine mutation sites in other modules

    #[tokio::test]
    async fn the_task_local_handle_reaches_mutation_sites_in_other_modules() {
        // The registry is process-global, so the registry-touching assertions are
        // serialised with the other journal tests. The guard is always released
        // before the `await`: neither the mirror nor the registry may hold a lock
        // across a suspension point.
        let journal = open("validator-t010-tasklocal", 9, 64);
        let mut engine = specialist_engine(64_000);
        journal.record_seed(&engine, &seed());
        {
            let _registry = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            assert!(
                registry_get("validator-t010-tasklocal").is_some(),
                "the registry holds the journal while its run is live"
            );
        }

        let scoped = journal.clone();
        let engine = with_journal_async(&journal, async move {
            assert!(
                current_journal().is_some(),
                "the handle must be visible inside the scope, the way the worker \
                 cancellation token is"
            );
            mirror_append_here(
                &mut engine,
                Message::User {
                    content: "tool result".to_string(),
                },
            );
            with_journal(&scoped, || {
                mirror_append_here(
                    &mut engine,
                    Message::Tool {
                        tool_call_id: "call-2".to_string(),
                        content: "sync scope".to_string(),
                    },
                );
            });
            engine
        })
        .await;

        assert_eq!(
            journal.snapshot(),
            engine_records(&engine),
            "the fix-loop path must mirror through the task-local"
        );
        assert_eq!(journal.history_source(), HistorySource::LiveMirror);
        assert!(
            current_journal().is_none(),
            "the scope must not leak past the call"
        );
        {
            let _registry = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            journal.close();
            assert!(journal.is_closed());
            assert_eq!(journal.close(), 0, "a second close never re-prunes");
            assert!(
                registry_get("validator-t010-tasklocal").is_some(),
                "a closed journal stays harvestable inside the window"
            );
            unregister("validator-t010-tasklocal");
        }
    }

    #[test]
    fn the_registry_keeps_only_the_newest_closed_journals() {
        let _registry = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The window is process-global, so closed journals left behind by other
        // tests are not part of what this test may reason about. Open journals are
        // deliberately left alone: an in-flight run must never be pruned.
        for journal in registry_snapshot() {
            if journal.is_closed() {
                unregister(journal.worker_key());
            }
        }

        let mut keys = Vec::new();
        for index in 0..MAX_CLOSED_JOURNALS + 5 {
            let key = format!("coder-t010-prune-{index:02}");
            let journal = open(&key, index as u64, 16);
            keys.push(key);
            journal.close();
        }
        let live_key = "coder-t010-prune-live";
        open(live_key, 999, 16);

        let survivors = keys
            .iter()
            .filter(|key| registry_get(key).is_some())
            .count();
        assert_eq!(
            survivors, MAX_CLOSED_JOURNALS,
            "closed journals past the window must be pruned, leaving exactly the newest \
             {MAX_CLOSED_JOURNALS}: {survivors}"
        );
        assert!(
            registry_get(&keys[0]).is_none(),
            "the oldest closed journal must be gone"
        );
        assert!(
            registry_get(&keys[keys.len() - 1]).is_some(),
            "the newest closed journal must survive"
        );
        assert!(
            registry_get(live_key).is_some(),
            "an open journal is never pruned"
        );
        for key in keys {
            unregister(&key);
        }
        assert!(unregister(live_key).is_some());
    }
}
