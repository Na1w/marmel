//! Process-global concurrency pool for LLM sessions.
//!
//! One [`Semaphore`] per `(backend_url, model)` pair. That pair — not the host —
//! is the unit the cap applies to, because the resource being protected is one
//! model server's context slots: two models behind one host are two independent
//! caps, and one model behind two spellings of one URL (`…/v1` and `…/v1/`) is
//! one cap. URL normalization therefore goes through [`config::backend_key`] on
//! **both** sides of the lookup (config key and runtime argument), exactly as
//! `ChatClient` already trims the trailing slash before building the wire path.
//!
//! Design contract (docs/concurrency_design_decisions.md §2, §5, §6):
//!
//! - **Unset means unlimited.** [`pool_for`] returns `None` when no cap is
//!   configured, so a caller can skip pool work entirely and keep today's exact
//!   behaviour. No `usize::MAX` sentinel semaphore is ever built.
//! - **Queueing is bounded.** A slot wait is capped at
//!   [`QUEUE_WAIT_TIMEOUT_SECS`] and produces [`PoolError::QueueTimeout`], which
//!   names the cap, the backend and the model so the failing delegation is
//!   attributable. The queue clock is separate from the per-stream read/chunk
//!   watchdogs, which are not even armed yet when acquisition happens.
//! - **Queue positions are FIFO.** Position comes from a monotonic ticket
//!   counter in a per-key ledger ([`QueueTicket`]), never from the iteration
//!   order of the [`DashMap`] that holds the pool (whose shards reshuffle as
//!   entries are inserted). The free-permit shortcut is gated on an empty
//!   ledger, so a fresh caller can never barge past a session already in line.
//! - **Internal fast calls get their own slot.** `SteerArbitrator` and
//!   `NoticeEval` must never queue behind specialist work — that would worsen
//!   the very responsiveness this feature exists to improve. They draw from a
//!   per-key reserve semaphore of capacity [`INTERNAL_RESERVE_CAPACITY`], wait
//!   at most [`INTERNAL_QUEUE_WAIT_SECS`], and then proceed **un-metered** with
//!   a warning ([`ReserveOutcome::FailedOpen`]). The documented cost is at most
//!   `max_sessions + 1` in-flight streams for a key, intentionally bounded.
//! - **Cancellation must not leak permits.** The permit wait selects on the
//!   worker token, the process-wide token and the queue deadline; a waiter that
//!   is cancelled, timed out, or aborted by its caller runs its [`Drop`] chain,
//!   which leaves the queue ledger and lets tokio unregister the semaphore
//!   waiter. Permits are only ever handed out by this module and only ever
//!   returned by the RAII guards' `Drop`.
//!
//! Wiring: [`crate::llm::ChatClient::chat_stream`] is the single acquisition
//! site (one permit per call, held across its whole retry loop), and every call
//! class is installed by the code that makes the call — Manager turn, specialist
//! turn, validator/fix-loop turn, nested delegation, steer arbitrator, notice
//! evaluation and prompt architect. This module never acquires anything itself;
//! it is the seam the client reads.

use std::collections::BTreeSet;
use std::future;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::config;
use crate::orchestrator::bus;

/// Hard bound on how long one backend call may sit waiting for a session slot.
///
/// Per acquisition, deliberately independent of the per-stream read timeout:
/// a stream that never started has no stream watchdog to blame.
pub const QUEUE_WAIT_TIMEOUT_SECS: u64 = 900;

/// Hard bound on how long a reserved internal call (steer arbitrator, notice
/// evaluation) waits for its reserve slot before proceeding un-metered.
///
/// Short by design: an internal fast call must not turn into a ten-minute wait,
/// because that would make steering unresponsive under load — see
/// [`ReserveOutcome::FailedOpen`].
pub const INTERNAL_QUEUE_WAIT_SECS: u64 = 20;

/// Slots in the per-key internal reserve semaphore.
pub const INTERNAL_RESERVE_CAPACITY: usize = 1;

/// How often a queued caller re-reads and re-reports its queue position.
///
/// Only armed when a caller actually asked to be reported to, so an unreported
/// waiter has exactly one timer (the queue deadline) and no polling cost.
const QUEUE_POSITION_REFRESH: Duration = Duration::from_millis(250);

/// Pool key: normalized backend URL + trimmed model.
pub type PoolKey = (String, String);

/// Reduce a `(backend_url, model)` pair to its pool key.
///
/// The URL half goes through [`config::backend_key`] — the same normalization
/// the config lookup uses — so a runtime `http://host/v1/` and a config key
/// `http://host/v1` land on one semaphore. The model half is only trimmed;
/// model names are compared exactly, matching how they are sent on the wire.
pub fn pool_key(backend_url: &str, model: &str) -> PoolKey {
    (config::backend_key(backend_url), model.trim().to_string())
}

/// Human-readable rendering of a pool key, used in logs and error messages.
fn key_label(key: &PoolKey) -> String {
    format!("{} + {}", key.0, key.1)
}

/// Where a queued session stands in its key's FIFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuePosition {
    /// 1-based rank in the queue. `1` means "next in line", not "running".
    pub position: usize,
    /// How many sessions are queued for this key, **including** this one.
    pub of: usize,
}

/// What a caller wants told about queue position.
///
/// `Send + Sync` because the waiting future is expected to live inside a
/// spawned worker. Position is reported at join time and again whenever it
/// changes while waiting (see [`QUEUE_POSITION_REFRESH`]).
pub type PositionReporter = dyn Fn(QueuePosition) + Send + Sync;

/// Why a session slot could not be obtained.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    /// The enclosing worker or the whole process was cancelled while waiting.
    /// Distinct from a timeout: nothing failed, the wait was simply called off.
    #[error("session slot wait for {key} was cancelled")]
    Cancelled {
        /// `backend + model` the wait belonged to.
        key: String,
    },
    /// No slot became available within the queue bound.
    ///
    /// Carries the cap and the key it belongs to so the failing delegation is
    /// attributable, per the design's queue-timeout contract.
    #[error(
        "queue wait for {backend} + {model} expired after {waited_secs}s: cap {cap}, \
         {waiting} session(s) queued (including this one)"
    )]
    QueueTimeout {
        /// Normalized backend URL half of the key.
        backend: String,
        /// Model half of the key.
        model: String,
        /// Configured cap that produced the queue.
        cap: usize,
        /// Queue depth at expiry, including the caller.
        waiting: usize,
        /// Whole seconds actually waited.
        waited_secs: u64,
    },
    /// The pool was shut down while waiting.
    ///
    /// Defensive only: nothing in this module closes a semaphore, so tokio's
    /// "semaphore closed" path is unreachable today. It is surfaced rather than
    /// unwrapped so a future shutdown hook cannot silently wedge a caller.
    #[error("session slot pool for {key} was closed")]
    PoolClosed {
        /// `backend + model` the wait belonged to.
        key: String,
    },
}

/// Per-key FIFO accounting.
///
/// `next_ticket` is strictly increasing and never reused, so a rank is defined
/// by ticket order alone — no map iteration order is ever consulted.
#[derive(Default)]
struct QueueLedger {
    next_ticket: u64,
    waiting: BTreeSet<u64>,
}

/// One `(backend_url, model)` pair's concurrency state.
pub struct PoolEntry {
    key: PoolKey,
    limit: usize,
    semaphore: Arc<Semaphore>,
    reserve: Arc<Semaphore>,
    ledger: Mutex<QueueLedger>,
}

impl PoolEntry {
    fn new(key: PoolKey, limit: usize) -> Self {
        // `Semaphore::new(0)` can never be acquired, which would wedge every
        // session against this backend forever. `config` already fails a `0`
        // cap open to unlimited; this is the belt-and-braces mirror of that
        // floor so no caller can build a dead pool by accident.
        let limit = limit.max(config::MIN_MAX_SESSIONS);
        Self {
            key,
            limit,
            semaphore: Arc::new(Semaphore::new(limit)),
            reserve: Arc::new(Semaphore::new(INTERNAL_RESERVE_CAPACITY)),
            ledger: Mutex::new(QueueLedger::default()),
        }
    }

    /// The key this entry serves.
    pub fn key(&self) -> &PoolKey {
        &self.key
    }

    /// Configured cap for this key.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Permits free right now.
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Sessions currently queued for this key, including any that joined and
    /// have not yet run their [`QueueTicket`] drop.
    pub fn waiting(&self) -> usize {
        self.ledger().waiting.len()
    }

    /// Free slots in the internal reserve ([`INTERNAL_RESERVE_CAPACITY`] total).
    pub fn reserve_available(&self) -> usize {
        self.reserve.available_permits()
    }

    /// The cap semaphore, for callers that need to hand out permits directly
    /// (tests, telemetry). Prefer [`wait_for_slot`].
    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.semaphore
    }

    /// Snapshot of this key's pool state, for the panel and diagnostics.
    pub fn snapshot(&self) -> PoolSnapshot {
        PoolSnapshot {
            limit: self.limit,
            available: self.available(),
            waiting: self.waiting(),
            reserve_available: self.reserve_available(),
        }
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, QueueLedger> {
        // Poisoning is swallowed on purpose: a panicking waiter must not turn
        // into a permanently wedged queue for every later session on this key.
        self.ledger.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn join(&self) -> u64 {
        let mut ledger = self.ledger();
        let ticket = ledger.next_ticket;
        ledger.next_ticket = ledger.next_ticket.wrapping_add(1);
        ledger.waiting.insert(ticket);
        ticket
    }

    fn leave(&self, ticket: u64) {
        self.ledger().waiting.remove(&ticket);
    }

    fn rank(&self, ticket: u64) -> Option<QueuePosition> {
        let ledger = self.ledger();
        if !ledger.waiting.contains(&ticket) {
            return None;
        }
        let of = ledger.waiting.len();
        let ahead = ledger.waiting.iter().take_while(|t| **t < ticket).count();
        Some(QueuePosition {
            position: ahead + 1,
            of,
        })
    }
}

/// Human-readable snapshot in logs and test failures.
///
/// `waiting` is reported only when the ledger lock is free: `Debug` must never
/// block, and never on a lock the code being debugged may already hold.
impl std::fmt::Debug for PoolEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("PoolEntry");
        debug
            .field("backend", &self.key.0)
            .field("model", &self.key.1)
            .field("limit", &self.limit)
            .field("available", &self.available())
            .field("reserve_available", &self.reserve_available());
        if let Ok(ledger) = self.ledger.try_lock() {
            debug.field("waiting", &ledger.waiting.len());
        }
        debug.finish()
    }
}

/// Coarse, copyable view of one key's pool state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolSnapshot {
    /// Configured cap.
    pub limit: usize,
    /// Free permits.
    pub available: usize,
    /// Queued sessions, per the FIFO ledger.
    pub waiting: usize,
    /// Free internal-reserve slots.
    pub reserve_available: usize,
}

/// The pool itself. Entries live for the process lifetime once created.
static POOLS: LazyLock<DashMap<PoolKey, Arc<PoolEntry>>> = LazyLock::new(DashMap::new);

/// Resolve (creating on first use) the entry for a pair, **given** a cap.
///
/// Split out from [`entry_for`] so tests and any future explicit-cap path can
/// build a pool without going through the process-global config.
pub fn entry_with_limit(backend_url: &str, model: &str, limit: usize) -> Arc<PoolEntry> {
    let key = pool_key(backend_url, model);
    if let Some(existing) = POOLS.get(&key) {
        let existing = existing.clone();
        // An entry is created with the cap resolved at first use. A cap change
        // picked up later (config reload) does not retroactively resize a live
        // semaphore; say so loudly instead of silently disagreeing.
        if existing.limit() != limit.max(config::MIN_MAX_SESSIONS) {
            tracing::debug!(
                key = %key_label(&key),
                applied = existing.limit(),
                configured = limit,
                "concurrency cap already fixed for this pair; live pool keeps the applied cap"
            );
        }
        return existing;
    }
    // Build outside the shard lock: `PoolEntry::new` touches no pool state.
    let entry = Arc::new(PoolEntry::new(key.clone(), limit));
    tracing::debug!(
        key = %key_label(&key),
        cap = entry.limit(),
        "concurrency pool opened for this backend+model pair"
    );
    POOLS.entry(key).or_insert(entry).clone()
}

/// The entry for a pair according to the active config; `None` ⇒ **unlimited**.
///
/// Reads the cap through [`config::effective_max_sessions`], so unset, an
/// inactive config, and a rejected (`0`) cap all mean the same thing here: no
/// pool, no queueing, today's behaviour.
///
/// The config surface keys caps by URL only, so each model behind that URL gets
/// **its own** semaphore at the same cap — two models behind one host use twice
/// the cap, which is the documented user-facing rule.
pub fn entry_for(backend_url: &str, model: &str) -> Option<Arc<PoolEntry>> {
    let limit = config::effective_max_sessions(backend_url)?;
    if limit < config::MIN_MAX_SESSIONS {
        tracing::warn!(
            backend = %backend_url,
            model = %model,
            "ignoring concurrency cap below the usable minimum; running unlimited"
        );
        return None;
    }
    Some(entry_with_limit(backend_url, model, limit))
}

/// The cap semaphore for a pair; `None` ⇒ **unlimited**, skip pool work.
///
/// This is the "do I meter this call at all" question, kept separate from the
/// wait so an uncapped call never touches a semaphore.
pub fn pool_for(backend_url: &str, model: &str) -> Option<Arc<Semaphore>> {
    entry_for(backend_url, model).map(|entry| Arc::clone(&entry.semaphore))
}

/// Snapshot of a pair's pool; `None` ⇒ unlimited.
pub fn pool_snapshot(backend_url: &str, model: &str) -> Option<PoolSnapshot> {
    entry_for(backend_url, model).map(|entry| entry.snapshot())
}

/// `true` when a new call on this pair gets a free session slot without queueing.
///
/// Read-only: unlike [`entry_for`] this never opens a pool entry, so asking the
/// question has no effect on what it measures. The three cases:
///
/// - **unlimited** — no cap configured for this backend (`max_sessions` unset,
///   no active config, or a rejected cap): `false`. There is no bounded spare
///   slot to reason about, so callers must keep the existing pause behaviour;
///   an unconfigured limit can never take the "spare capacity" path.
/// - **spare** — a cap is configured and at least one permit is free with
///   nobody already queued for it: `true`.
/// - **full / waiting** — no free permit, or a session is already in the FIFO
///   queue: `false`.
///
/// A capped pair whose pool has not been opened yet holds every permit free, so
/// it reports spare capacity. The free test is the pool's own
/// (see [`wait_for_slot`]'s fast path): `available > 0 && waiting == 0`.
///
/// A best-effort snapshot, **not** a reservation: a concurrent caller can take
/// the slot between this read and the caller's own acquisition.
pub fn has_spare_session_capacity(backend_url: &str, model: &str) -> bool {
    let Some(limit) = config::effective_max_sessions(backend_url) else {
        return false;
    };
    if limit < config::MIN_MAX_SESSIONS {
        // Mirrors `entry_for`: a sub-minimum cap is never a real pool.
        return false;
    }
    let key = pool_key(backend_url, model);
    POOLS
        .get(&key)
        .is_none_or(|entry| entry.waiting() == 0 && entry.available() > 0)
}

/// Number of pool entries currently open. Test/diagnostic only.
pub fn open_pool_count() -> usize {
    POOLS.len()
}

/// Take a FIFO ticket for this key's queue.
///
/// The returned guard holds the queue slot and releases it on drop, which is
/// what makes position accounting correct for a caller that is cancelled, times
/// out, or is aborted mid-wait.
pub fn queue_ticket(entry: &Arc<PoolEntry>) -> QueueTicket {
    let ticket = entry.join();
    QueueTicket {
        entry: Arc::clone(entry),
        ticket,
    }
}

/// A live queue slot. Drop leaves the queue.
#[must_use = "the guard is what holds the queue slot; dropping it immediately leaves the queue"]
pub struct QueueTicket {
    entry: Arc<PoolEntry>,
    ticket: u64,
}

impl QueueTicket {
    /// The monotonic ticket number backing this position.
    pub fn ticket(&self) -> u64 {
        self.ticket
    }

    /// Current FIFO position, or `None` if this ticket already left the queue.
    pub fn position(&self) -> Option<QueuePosition> {
        self.entry.rank(self.ticket)
    }
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.entry.leave(self.ticket);
    }
}

/// A held session slot. The permit returns to its pool when this drops.
#[must_use = "the guard is what holds the session slot; dropping it releases the slot immediately"]
#[derive(Debug)]
pub struct SessionPermit {
    entry: Arc<PoolEntry>,
    /// Write-only on purpose: the only thing this field is for is returning the
    /// permit to the semaphore when this guard drops.
    _permit: OwnedSemaphorePermit,
    waited: Duration,
    position: Option<QueuePosition>,
}

impl SessionPermit {
    fn new(
        entry: &Arc<PoolEntry>,
        permit: OwnedSemaphorePermit,
        waited: Duration,
        position: Option<QueuePosition>,
    ) -> Self {
        Self {
            entry: Arc::clone(entry),
            _permit: permit,
            waited,
            position,
        }
    }

    /// The pair this slot belongs to.
    pub fn key(&self) -> &PoolKey {
        &self.entry.key
    }

    /// Normalized backend URL half of the key.
    pub fn backend(&self) -> &str {
        &self.entry.key.0
    }

    /// Model half of the key.
    pub fn model(&self) -> &str {
        &self.entry.key.1
    }

    /// The cap this slot was drawn against.
    pub fn limit(&self) -> usize {
        self.entry.limit()
    }

    /// Permits still free in this pool while this slot is held.
    pub fn available_permits(&self) -> usize {
        self.entry.available()
    }

    /// How long the wait for this slot took (`ZERO` when no queueing happened).
    pub fn waited(&self) -> Duration {
        self.waited
    }

    /// Queue position this call held, if it ever queued.
    pub fn queue_position(&self) -> Option<QueuePosition> {
        self.position
    }
}

/// A held internal-reserve slot. Released on drop.
#[must_use = "the guard is what holds the reserved slot; dropping it releases it immediately"]
#[derive(Debug)]
pub struct ReservePermit {
    entry: Arc<PoolEntry>,
    /// Write-only on purpose: returning the reserve slot on drop is its only job.
    _permit: OwnedSemaphorePermit,
}

impl ReservePermit {
    fn new(entry: &Arc<PoolEntry>, permit: OwnedSemaphorePermit) -> Self {
        Self {
            entry: Arc::clone(entry),
            _permit: permit,
        }
    }

    /// The pair the reserve slot belongs to.
    pub fn key(&self) -> &PoolKey {
        &self.entry.key
    }

    /// Free reserve slots left while this one is held (`0` at capacity 1).
    pub fn reserve_available(&self) -> usize {
        self.entry.reserve_available()
    }
}

/// How a reserve acquisition ended.
#[derive(Debug)]
pub enum ReserveOutcome {
    /// The reserved slot was obtained; held by the wrapped guard.
    Reserved(ReservePermit),
    /// Nothing is capped for this pair, so there is nothing to reserve.
    Unmetered,
    /// The reserve slot stayed busy for the whole [`INTERNAL_QUEUE_WAIT_SECS`]
    /// wait: proceed **un-metered**. Bounded and intentional (I7) — a cap must
    /// never make steering unresponsive.
    FailedOpen {
        /// Whole seconds spent waiting for the reserve slot.
        waited_secs: u64,
    },
    /// The worker or the process was cancelled while waiting. The caller must
    /// not proceed; it is being torn down.
    Cancelled,
}

/// Wait for a session slot on a specific entry.
///
/// `worker_token` is the enclosing worker's cancellation token (`None` for a
/// call that belongs to no worker, e.g. a manager turn on the UI task). The
/// process-wide token from [`bus::global_cancellation_token`] is always
/// selected on as well.
pub async fn wait_for_slot(
    entry: &Arc<PoolEntry>,
    worker_token: Option<CancellationToken>,
    queue_wait: Duration,
    report: Option<&PositionReporter>,
) -> Result<SessionPermit, PoolError> {
    let started = Instant::now();
    let key_label = key_label(&entry.key);

    // Fast path: a free permit *and* nobody already in line. The ledger check is
    // what keeps a fresh caller from barging past sessions that already hold a
    // queue slot for this key — the ledger, not the semaphore's momentary
    // availability, decides who goes first.
    if entry.waiting() == 0
        && let Ok(permit) = Arc::clone(&entry.semaphore).try_acquire_owned()
    {
        return Ok(SessionPermit::new(entry, permit, Duration::ZERO, None));
    }

    let ticket = queue_ticket(entry);
    let mut position = ticket
        .position()
        .unwrap_or(QueuePosition { position: 1, of: 1 });
    if let Some(report) = report {
        report(position);
    }
    // Attribution comes from the installed call context, so a queued backend
    // call in the log names the delegation it belongs to rather than just the key.
    let call = current_call_context()
        .map(|context| context.describe())
        .unwrap_or_else(|| "unclassified".to_string());
    tracing::debug!(
        key = %key_label,
        position = position.position,
        of = position.of,
        cap = entry.limit(),
        call = %call,
        "session queued waiting for a slot"
    );

    let global = bus::global_cancellation_token();
    let deadline = tokio::time::sleep(queue_wait);
    tokio::pin!(deadline);

    let permit = loop {
        tokio::select! {
            biased;
            _ = wait_for_cancellation(worker_token.as_ref()) => {
                return Err(PoolError::Cancelled { key: key_label });
            }
            _ = global.cancelled() => {
                return Err(PoolError::Cancelled { key: key_label });
            }
            _ = &mut deadline => {
                let waiting = entry.waiting();
                let waited_secs = started.elapsed().as_secs();
                tracing::warn!(
                    key = %key_label,
                    cap = entry.limit(),
                    waiting,
                    waited_secs,
                    "queue wait expired; failing this backend call"
                );
                return Err(PoolError::QueueTimeout {
                    backend: entry.key.0.clone(),
                    model: entry.key.1.clone(),
                    cap: entry.limit(),
                    waiting,
                    waited_secs,
                });
            }
            acquired = Arc::clone(&entry.semaphore).acquire_owned() => {
                match acquired {
                    Ok(permit) => break permit,
                    Err(_) => return Err(PoolError::PoolClosed { key: key_label }),
                }
            }
            // Only meaningful when someone is listening: keep their view of the
            // queue honest as sessions ahead of us leave.
            _ = tokio::time::sleep(QUEUE_POSITION_REFRESH), if report.is_some() => {
                if let Some(next) = ticket.position()
                    && next != position
                {
                    position = next;
                    if let Some(report) = report {
                        report(position);
                    }
                }
            }
        }
    };

    Ok(SessionPermit::new(
        entry,
        permit,
        started.elapsed(),
        Some(position),
    ))
}

/// Wait for a slot on a specific entry using the standard queue bound.
pub async fn wait_for_slot_default(
    entry: &Arc<PoolEntry>,
    worker_token: Option<CancellationToken>,
    report: Option<&PositionReporter>,
) -> Result<SessionPermit, PoolError> {
    wait_for_slot(entry, worker_token, queue_wait_timeout(), report).await
}

/// The standard per-acquisition queue bound.
pub fn queue_wait_timeout() -> Duration {
    Duration::from_secs(QUEUE_WAIT_TIMEOUT_SECS)
}

/// The standard internal-reserve bound.
pub fn internal_queue_wait() -> Duration {
    Duration::from_secs(INTERNAL_QUEUE_WAIT_SECS)
}

/// Acquire a session slot for `(backend_url, model)` under the standard bound.
///
/// `Ok(None)` ⇒ **unlimited** for this pair: no pool exists, so the caller
/// proceeds with no guard and no queueing, exactly as before this feature.
pub async fn acquire_session(
    backend_url: &str,
    model: &str,
    worker_token: Option<CancellationToken>,
    report: Option<&PositionReporter>,
) -> Result<Option<SessionPermit>, PoolError> {
    acquire_session_with_wait(
        backend_url,
        model,
        worker_token,
        queue_wait_timeout(),
        report,
    )
    .await
}

/// [`acquire_session`] with an explicit queue bound (per-acquisition re-arming
/// and tests need to override the 900 s default).
pub async fn acquire_session_with_wait(
    backend_url: &str,
    model: &str,
    worker_token: Option<CancellationToken>,
    queue_wait: Duration,
    report: Option<&PositionReporter>,
) -> Result<Option<SessionPermit>, PoolError> {
    match entry_for(backend_url, model) {
        None => Ok(None),
        Some(entry) => wait_for_slot(&entry, worker_token, queue_wait, report)
            .await
            .map(Some),
    }
}

/// Acquire the internal reserve slot for `(backend_url, model)` under the
/// standard internal bound ([`internal_queue_wait`], i.e.
/// [`INTERNAL_QUEUE_WAIT_SECS`]).
pub async fn acquire_reserve(
    backend_url: &str,
    model: &str,
    worker_token: Option<CancellationToken>,
) -> ReserveOutcome {
    acquire_reserve_with_wait(backend_url, model, worker_token, internal_queue_wait()).await
}

/// [`acquire_reserve`] with an explicit bound on the reserve wait.
///
/// The default stays [`INTERNAL_QUEUE_WAIT_SECS`]; a caller may only ever shrink
/// it (per-call re-arming and tests), never turn the bounded fail-open wait into
/// an unbounded one.
pub async fn acquire_reserve_with_wait(
    backend_url: &str,
    model: &str,
    worker_token: Option<CancellationToken>,
    reserve_wait: Duration,
) -> ReserveOutcome {
    match entry_for(backend_url, model) {
        None => ReserveOutcome::Unmetered,
        Some(entry) => acquire_reserve_for(&entry, worker_token, reserve_wait).await,
    }
}

/// [`acquire_reserve`] against a specific entry and bound.
pub async fn acquire_reserve_for(
    entry: &Arc<PoolEntry>,
    worker_token: Option<CancellationToken>,
    wait: Duration,
) -> ReserveOutcome {
    let started = Instant::now();
    let key_label = key_label(&entry.key);

    if let Ok(permit) = Arc::clone(&entry.reserve).try_acquire_owned() {
        return ReserveOutcome::Reserved(ReservePermit::new(entry, permit));
    }

    let global = bus::global_cancellation_token();
    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);

    // Every arm is terminal: a reserve call either holds its slot, gives up and
    // runs un-metered, or is being cancelled. It never re-waits.
    tokio::select! {
        biased;
        _ = wait_for_cancellation(worker_token.as_ref()) => ReserveOutcome::Cancelled,
        _ = global.cancelled() => ReserveOutcome::Cancelled,
        _ = &mut deadline => {
            tracing::warn!(
                key = %key_label,
                waited_secs = started.elapsed().as_secs(),
                reserve_capacity = INTERNAL_RESERVE_CAPACITY,
                "internal reserve slot busy; running this call un-metered"
            );
            ReserveOutcome::FailedOpen {
                waited_secs: started.elapsed().as_secs(),
            }
        }
        acquired = Arc::clone(&entry.reserve).acquire_owned() => match acquired {
            Ok(permit) => ReserveOutcome::Reserved(ReservePermit::new(entry, permit)),
            // Closed reserve pool is unreachable today; failing open is the
            // only safe reading of it for a fast internal call.
            Err(_) => {
                tracing::warn!(
                    key = %key_label,
                    "internal reserve pool closed; running this call un-metered"
                );
                ReserveOutcome::FailedOpen {
                    waited_secs: started.elapsed().as_secs(),
                }
            }
        },
    }
}

/// Await the worker token, or nothing at all when there is no worker.
async fn wait_for_cancellation(worker: Option<&CancellationToken>) {
    match worker {
        Some(token) => token.cancelled().await,
        None => future::pending::<()>().await,
    }
}

/// Clone the cancellation token of the worker this task belongs to, if any.
///
/// Mirrors how `harness::sleep` and the validator reach
/// [`bus::CURRENT_WORKER_TOKEN`]: code on the call path has no token parameter,
/// so the task-local is the only honest source.
pub fn current_worker_token() -> Option<CancellationToken> {
    bus::CURRENT_WORKER_TOKEN
        .try_with(|token| token.clone())
        .ok()
}

/// Which kind of LLM call is being made, and therefore how it is metered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallClass {
    /// Manager turn (user-facing synthesis / planning round).
    ManagerTurn,
    /// Specialist subagent turn.
    Specialist,
    /// Validator turn.
    Validator,
    /// Prompt-architect turn.
    PromptArchitect,
    /// Steer arbitrator decision call — reserve class.
    SteerArbitrator,
    /// Worker-reply / notice evaluation call — reserve class.
    NoticeEval,
}

impl CallClass {
    /// `true` when this class draws from the per-key cap semaphore.
    pub fn is_capped(self) -> bool {
        matches!(
            self,
            Self::ManagerTurn | Self::Specialist | Self::Validator | Self::PromptArchitect
        )
    }

    /// `true` when this class draws from the per-key reserve instead, and fails
    /// open rather than queueing (see [`ReserveOutcome::FailedOpen`]).
    pub fn uses_reserve(self) -> bool {
        matches!(self, Self::SteerArbitrator | Self::NoticeEval)
    }

    /// Stable label for logs and telemetry.
    pub fn label(self) -> &'static str {
        match self {
            Self::ManagerTurn => "manager_turn",
            Self::Specialist => "specialist",
            Self::Validator => "validator",
            Self::PromptArchitect => "prompt_architect",
            Self::SteerArbitrator => "steer_arbitrator",
            Self::NoticeEval => "notice_eval",
        }
    }
}

/// What kind of call a piece of code is making, plus who is making it.
///
/// Lets a deep call path (`delegate` → runner → client) report *why* it is
/// queueing without threading a class through every signature, and carries the
/// agent/task identity the pool and the Subagents panel attribute a queued call
/// to.
///
/// The context lives in a **task-local**, never a thread-local. A tokio worker
/// thread runs many tasks and one task moves between workers across `.await`, so
/// a thread-local would fail in both directions: a class installed before an
/// await would be missing on the worker that resumes the task, and it would leak
/// onto whichever unrelated task ran next on that thread. [`tokio::task_local!`]
/// binds the value to one future *tree* and re-installs it on every poll, on
/// whichever worker polls it — which is exactly the call path the class belongs
/// to. Synchronous code polled inside that tree (a sync tool handler reaching
/// [`current_call_context`]) reads the same value.
///
/// Task-locals are **not** inherited across `tokio::spawn`,
/// `tokio::task::spawn_blocking`, or a plain `std::thread`. Every boundary on a
/// real call path is crossed explicitly: capture [`current_call_context`] in the
/// parent and re-install it in the child with [`with_call_context_sync`] (see the
/// Manager tool round in `ui::session` and the nested `delegate_task` in
/// [`crate::orchestrator::delegate`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallContext {
    /// How this call is metered.
    pub class: CallClass,
    /// Agent id, when known (worker/agent label).
    pub agent: Option<String>,
    /// Task id, when known.
    pub task: Option<String>,
}

/// Agent label carried by Manager/UI turns. A Manager turn has no plan task, so
/// its context carries an identity but no task id.
pub const MANAGER_AGENT: &str = "manager";

impl CallContext {
    /// Minimal context carrying only the call class.
    pub fn new(class: CallClass) -> Self {
        Self {
            class,
            agent: None,
            task: None,
        }
    }

    /// Context for a named agent, with its plan task id when one is known — the
    /// shape a specialist turn or a nested delegation installs.
    pub fn for_agent(class: CallClass, agent: impl Into<String>, task: Option<String>) -> Self {
        Self {
            class,
            agent: Some(agent.into()),
            task,
        }
    }

    /// The same identity re-metered as `class`: nesting keeps the agent/task
    /// attribution while changing how the call is metered.
    pub fn with_class(&self, class: CallClass) -> Self {
        Self {
            class,
            agent: self.agent.clone(),
            task: self.task.clone(),
        }
    }

    /// Single-line attribution for logs and queue traffic, e.g.
    /// `specialist coder [t-004]`, `manager_turn manager`.
    pub fn describe(&self) -> String {
        let who = match (self.agent.as_deref(), self.task.as_deref()) {
            (Some(agent), Some(task)) => format!(" {agent} [{task}]"),
            (Some(agent), None) => format!(" {agent}"),
            (None, Some(task)) => format!(" [{task}]"),
            (None, None) => String::new(),
        };
        format!("{}{who}", self.class.label())
    }
}

tokio::task_local! {
    /// The [`CallContext`] in force for the future tree being polled.
    static CURRENT_CALL_CONTEXT: CallContext;
}

/// Run `fut` with `context` installed for every LLM call it makes, at any depth
/// of nesting, on whichever worker thread polls it.
///
/// Nesting is an overwrite: the innermost scope wins, and the previous context is
/// restored when the scope's future completes **or is dropped** (cancellation,
/// timeout, panic unwind) — that restore is what guarantees a finished delegation
/// leaves no context behind for the next call on this task.
pub async fn with_call_context<F>(context: CallContext, fut: F) -> F::Output
where
    F: future::Future,
{
    CURRENT_CALL_CONTEXT.scope(context, fut).await
}

/// Synchronous sibling of [`with_call_context`]: installs `context` for the
/// duration of `f` on the calling thread.
///
/// This is the propagation half of every boundary a task-local cannot cross by
/// itself — a `spawn_blocking` tool round, or the `std::thread::scope` +
/// `block_on` shape nested `delegate_task` uses. Like the async scope it restores
/// the previous value on the way out, including on unwind.
pub fn with_call_context_sync<R>(context: CallContext, f: impl FnOnce() -> R) -> R {
    CURRENT_CALL_CONTEXT.sync_scope(context, f)
}

/// The call context in force for the current task, if any.
///
/// `None` means "no class installed": the pool meters such a call against the cap
/// (the pre-feature behaviour) and the queue traffic carries no attribution.
pub fn current_call_context() -> Option<CallContext> {
    CURRENT_CALL_CONTEXT
        .try_with(|context| context.clone())
        .ok()
}

/// The class in force, if any — shorthand for callers that only meter on class.
pub fn current_call_class() -> Option<CallClass> {
    CURRENT_CALL_CONTEXT.try_with(|context| context.class).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize every pool test: the pool, the global config, and the global
    /// cancellation token are all process-global, and libtest runs test threads
    /// concurrently.
    ///
    /// A plain `std::sync::Mutex` (mirroring `workers::TEST_WORKERS_MUTEX`) is
    /// taken inside the synchronous drivers below and never held across an
    /// `await`, which is what keeps the async tests free of cross-thread guard
    /// hazards.
    pub static TEST_POOL_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Forget every open pool. Pool entries are keyed by URL+model, so this
    /// makes a test's cap deterministic instead of inheriting a sibling's.
    pub fn reset_pools_for_test() {
        POOLS.clear();
    }

    /// Build/fetch an entry with an explicit cap, bypassing the config lookup.
    pub fn entry_with_limit_for_test(
        backend_url: &str,
        model: &str,
        limit: usize,
    ) -> Arc<PoolEntry> {
        entry_with_limit(backend_url, model, limit)
    }

    fn reset_pool_state() {
        reset_pools_for_test();
        bus::reset_cancellation();
    }

    /// Drive `test` on a current-thread runtime with pool + cancellation state
    /// reset on both sides.
    fn pool_test<R>(test: impl future::Future<Output = R>) -> R {
        // These drivers reset (and in one case cancel) the process-global
        // cancellation token, so they take the same mutex the arbitration tests
        // take. Both guards are held across `block_on` only — never across an
        // `await` — so no async body can carry a cross-thread guard.
        let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _serial = TEST_POOL_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        reset_pool_state();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime for the pool test");
        let result = runtime.block_on(test);
        reset_pool_state();
        result
    }

    /// [`pool_test`] on a multi-thread runtime, for tests that genuinely
    /// concurrent entrants.
    fn pool_test_multi<R>(workers: usize, test: impl future::Future<Output = R>) -> R {
        let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _serial = TEST_POOL_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        reset_pool_state();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(workers)
            .build()
            .expect("multi-thread runtime for the pool test");
        let result = runtime.block_on(test);
        reset_pool_state();
        result
    }

    /// Activate a config with `[orchestration] concurrency` set, for the
    /// duration of `test`.
    ///
    /// Takes `config::sandbox_env_test_lock` because `ACTIVE_CONFIG` is a
    /// process-global shared with `src/config.rs` tests, and drives the async
    /// body inside a synchronous runtime build so no guard is ever held across
    /// an `await`.
    fn config_pool_test<R>(
        max_sessions: Option<usize>,
        overrides: Vec<(String, usize)>,
        test: impl future::Future<Output = R>,
    ) -> R {
        let _workers = crate::orchestrator::workers::TEST_WORKERS_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _serial = TEST_POOL_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _env_serial = config::sandbox_env_test_lock();
        reset_pool_state();

        let mut concurrency = config::ConcurrencyConfig {
            by_backend: std::collections::HashMap::new(),
            max_sessions,
        };
        for (key, value) in overrides {
            concurrency.by_backend.insert(key, value);
        }
        let mut cfg = config::Config::default();
        cfg.orchestration.concurrency = concurrency;
        config::set_active(cfg);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime for the config-backed pool test");
        let result = runtime.block_on(test);

        config::clear_active();
        reset_pool_state();
        result
    }

    #[test]
    fn pool_key_normalizes_url_and_model() {
        assert_eq!(
            pool_key("  http://host:8080/v1/  ", "  llama-3  "),
            ("http://host:8080/v1".to_string(), "llama-3".to_string())
        );
        // Same normalization the config side uses, or an override silently misses.
        assert_eq!(
            pool_key("http://host:8080/v1/", "m").0,
            config::backend_key("http://host:8080/v1/")
        );
        assert_eq!(key_label(&pool_key("http://h/v1", "m")), "http://h/v1 + m");
    }

    #[test]
    fn one_entry_per_backend_model_pair_across_url_spellings() {
        pool_test(async {
            let a = entry_with_limit_for_test("http://pool-identity:8080/v1", "model-a", 2);
            let b = entry_with_limit_for_test("http://pool-identity:8080/v1/", "model-a", 2);
            let c = entry_with_limit_for_test("http://pool-identity:8080/v1", " model-a ", 2);
            assert!(Arc::ptr_eq(&a, &b), "trailing slash must not fork the pool");
            assert!(
                Arc::ptr_eq(&a, &c),
                "model whitespace must not fork the pool"
            );

            let d = entry_with_limit_for_test("http://pool-identity:8080/v1", "model-b", 2);
            assert!(
                !Arc::ptr_eq(&a, &d),
                "a different model is a different pool"
            );
            let e = entry_with_limit_for_test("http://other:8080/v1", "model-a", 2);
            assert!(
                !Arc::ptr_eq(&a, &e),
                "a different backend is a different pool"
            );

            // A cap already fixed for a live pair is not retroactively resized.
            let resized = entry_with_limit_for_test("http://pool-identity:8080/v1", "model-a", 5);
            assert!(Arc::ptr_eq(&a, &resized));
            assert_eq!(resized.limit(), 2);
        });
    }

    #[test]
    fn zero_cap_cannot_wedge_the_pool() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-zero:8080/v1", "m", 0);
            assert_eq!(entry.limit(), config::MIN_MAX_SESSIONS);
            assert_eq!(entry.available(), config::MIN_MAX_SESSIONS);
        });
    }

    #[test]
    fn unset_cap_means_no_pool_and_no_queueing() {
        config_pool_test(None, vec![], async {
            let url = "http://pool-unset:8080/v1";
            assert!(pool_for(url, "m").is_none(), "unset must mean unlimited");
            assert!(entry_for(url, "m").is_none());
            assert!(pool_snapshot(url, "m").is_none());
            let permit = acquire_session(url, "m", None, None).await.unwrap();
            assert!(permit.is_none(), "unlimited calls get no guard at all");
            assert_eq!(open_pool_count(), 0, "unlimited calls create no pool");
        });
    }

    /// An unlimited pair — no cap configured, or a cap rejected as unusable —
    /// never reports spare capacity, so a caller gating on it keeps the pause
    /// behaviour. The query must not register a pool entry either.
    #[test]
    fn unlimited_pairs_report_no_spare_capacity() {
        for max_sessions in [None, Some(0)] {
            config_pool_test(max_sessions, vec![], async {
                let url = "http://pool-spare-open:8080/v1";
                assert!(
                    !has_spare_session_capacity(url, "m"),
                    "no usable cap ({max_sessions:?}) means unlimited, so there is no spare slot to hand out"
                );
                assert_eq!(
                    open_pool_count(),
                    0,
                    "asking about capacity must not open a pool entry"
                );
            });
        }

        // A per-URL override is what makes a pair capped at all when the global
        // key is unset; the lookup goes through the same URL normalization.
        config_pool_test(
            None,
            vec![("http://pool-spare-override:8080/v1".to_string(), 1usize)],
            async {
                assert!(has_spare_session_capacity(
                    "http://pool-spare-override:8080/v1/",
                    "m"
                ));
                assert!(
                    !has_spare_session_capacity("http://pool-spare-open:8080/v1", "m"),
                    "a backend with no override and no global cap is unlimited"
                );
                assert_eq!(open_pool_count(), 0, "still no entry created by the query");
            },
        );
    }

    /// A capped pair mirrors the pool's own free test: spare while a permit is
    /// free and nobody is queued, not spare once it is saturated or someone is
    /// already waiting for a slot.
    #[test]
    fn spare_capacity_mirrors_the_pool_free_test() {
        config_pool_test(Some(2), vec![], async {
            let url = "http://pool-spare:8080/v1";
            // Capped but never used: every permit is free, and the query still
            // created nothing.
            assert!(
                has_spare_session_capacity(url, "m"),
                "a capped pair with an unopened pool has spare capacity"
            );
            assert_eq!(open_pool_count(), 0, "the query must not open a pool entry");

            let entry = entry_for(url, "m").expect("configured cap");
            assert_eq!(
                open_pool_count(),
                1,
                "only the acquiring path opens an entry"
            );

            let held = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();
            assert!(
                has_spare_session_capacity(url, "m"),
                "one free permit of two is still spare"
            );

            let full = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();
            assert!(
                !has_spare_session_capacity(url, "m"),
                "a saturated pair has no spare slot"
            );
            drop(held);
            assert!(
                has_spare_session_capacity(url, "m"),
                "a freed permit is spare again"
            );

            // A session already in line owns the pool even with a permit free.
            let ticket = queue_ticket(&entry);
            assert!(
                !has_spare_session_capacity(url, "m"),
                "a queued session means no spare capacity"
            );
            drop(ticket);
            assert!(
                has_spare_session_capacity(url, "m"),
                "leaving the queue restores spare capacity"
            );

            // The cap, and therefore the answer, is per `backend + model`.
            let other = entry_for(url, "model-two").expect("configured cap");
            let other_first = wait_for_slot(&other, None, Duration::from_millis(50), None)
                .await
                .unwrap();
            let other_second = wait_for_slot(&other, None, Duration::from_millis(50), None)
                .await
                .unwrap();
            assert!(
                !has_spare_session_capacity(url, "model-two"),
                "the saturated model reports no spare capacity"
            );
            assert!(
                has_spare_session_capacity(url, "m"),
                "the other model behind the same backend is a separate pool"
            );
            drop((full, other_first, other_second));
        });
    }

    #[test]
    fn cap_from_config_is_applied_per_url_and_model() {
        config_pool_test(
            Some(2),
            vec![("http://pool-cfg:8080/v1".to_string(), 3usize)],
            async {
                // Per-URL override wins for that URL, and a trailing slash on the
                // runtime URL still matches the config key.
                let entry = entry_for("http://pool-cfg:8080/v1/", "m").expect("override cap");
                assert_eq!(entry.limit(), 3);
                assert_eq!(
                    pool_snapshot("http://pool-cfg:8080/v1", "m").unwrap().limit,
                    3
                );

                // Every other backend falls back to max_sessions.
                let other = entry_for("http://pool-other:8080/v1", "m").expect("default cap");
                assert_eq!(other.limit(), 2);

                // A capped pair does get a guard.
                let permit = acquire_session("http://pool-other:8080/v1", "m", None, None)
                    .await
                    .unwrap();
                let permit = permit.expect("capped call must hold a permit");
                assert_eq!(permit.backend(), "http://pool-other:8080/v1");
                assert_eq!(permit.model(), "m");
                assert_eq!(permit.waited(), Duration::ZERO);
                assert_eq!(permit.queue_position(), None, "no queueing happened");
                assert_eq!(other.available(), 1);
                drop(permit);
                assert_eq!(other.available(), 2);
            },
        );
    }

    #[test]
    fn queue_timeout_is_bounded_and_attributed() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-timeout:8080/v1", "mm", 1);
            let _held = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();

            let started = Instant::now();
            let err = wait_for_slot(&entry, None, Duration::from_millis(30), None)
                .await
                .expect_err("slot is held, so this must time out");
            let waited = started.elapsed();

            // Design contract: the message names the cap and the key.
            let rendered = err.to_string();
            match err {
                PoolError::QueueTimeout {
                    backend,
                    model,
                    cap,
                    waiting,
                    waited_secs,
                } => {
                    assert_eq!(backend, "http://pool-timeout:8080/v1");
                    assert_eq!(model, "mm");
                    assert_eq!(cap, 1, "the error must name the cap");
                    assert_eq!(waiting, 1, "depth includes the caller");
                    assert!(waited_secs < 1);
                    // Bounded: no way this should approach the queue bound.
                    assert!(waited < Duration::from_secs(2), "waited {waited:?}");
                    assert!(rendered.contains("cap 1"));
                    assert!(rendered.contains("http://pool-timeout:8080/v1 + mm"));
                }
                other => panic!("expected QueueTimeout, got {other:?}"),
            }
            assert_eq!(
                entry.waiting(),
                0,
                "timed-out waiter must have left the queue"
            );
        });
    }

    #[test]
    fn dropping_a_permit_frees_the_slot() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-drop:8080/v1", "m", 2);
            let first = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();
            let second = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();
            assert_eq!(entry.available(), 0);

            let third = wait_for_slot(&entry, None, Duration::from_millis(30), None).await;
            assert!(matches!(third, Err(PoolError::QueueTimeout { .. })));

            drop(first);
            // A free slot is taken by the fast path: no ticket, no queueing.
            let got = wait_for_slot(&entry, None, Duration::from_millis(200), None)
                .await
                .expect("released slot must be reusable");
            assert_eq!(got.waited(), Duration::ZERO, "free slot means no queueing");
            assert_eq!(got.queue_position(), None, "no rank without queueing");
            assert_eq!(
                got.available_permits(),
                0,
                "both remaining slots are held while this one is held"
            );

            // With every slot held again, a fresh caller really does queue, and
            // its rank is the FIFO ledger's, not the semaphore's.
            let queued_entry = Arc::clone(&entry);
            let queued = tokio::spawn(async move {
                wait_for_slot(&queued_entry, None, Duration::from_millis(400), None).await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(entry.waiting(), 1, "the fresh caller must be queued");
            drop(second);
            let queued = queued
                .await
                .expect("no panic")
                .expect("must get the freed slot");
            assert_eq!(
                queued.queue_position().map(|p| p.position),
                Some(1),
                "the only waiter is rank 1"
            );
            assert!(queued.waited() > Duration::ZERO, "this one did queue");

            drop(got);
            drop(queued);
            assert_eq!(entry.available(), 2);
            assert_eq!(entry.waiting(), 0);
        });
    }

    #[test]
    fn a_caller_in_line_blocks_the_fast_path() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-barge:8080/v1", "m", 1);
            let held = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();

            // A caller that holds a queue slot but has not been handed the
            // permit yet.
            let ahead = queue_ticket(&entry);
            drop(held);
            assert_eq!(
                entry.available(),
                1,
                "the slot is free while the other caller is still in line"
            );

            // A fresh caller must take its rank behind that caller rather than
            // barge in through the free-permit shortcut.
            let seen: Arc<Mutex<Vec<QueuePosition>>> = Arc::default();
            let reporter = Arc::clone(&seen);
            let got = wait_for_slot(
                &entry,
                None,
                Duration::from_millis(200),
                Some(&move |pos| reporter.lock().unwrap().push(pos)),
            )
            .await
            .expect("a slot is free, so the queue path resolves at once");

            assert_eq!(
                got.queue_position(),
                Some(QueuePosition { position: 2, of: 2 }),
                "the fresh caller queued behind the caller already in line"
            );
            assert_eq!(
                *seen.lock().unwrap(),
                vec![QueuePosition { position: 2, of: 2 }],
                "and reported that rank"
            );
            assert_eq!(got.available_permits(), 0);

            drop(ahead);
            drop(got);
            assert_eq!(entry.available(), 1);
            assert_eq!(entry.waiting(), 0);
        });
    }

    #[test]
    fn worker_token_cancellation_ends_the_wait_without_error_or_leak() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-cancel:8080/v1", "m", 1);
            let _held = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();

            let token = CancellationToken::new();
            let waiter_entry = Arc::clone(&entry);
            let waiter_token = token.clone();
            let waiter = tokio::spawn(async move {
                wait_for_slot(
                    &waiter_entry,
                    Some(waiter_token),
                    Duration::from_secs(30),
                    None,
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(entry.waiting(), 1, "waiter should be queued");

            token.cancel();
            let outcome = waiter.await.expect("waiter task must not panic");
            assert!(
                matches!(outcome, Err(PoolError::Cancelled { .. })),
                "worker cancellation must be a distinguishable Cancelled, got {outcome:?}"
            );
            assert_eq!(entry.waiting(), 0, "cancelled waiter must leave the queue");

            drop(_held);
            assert_eq!(
                entry.available(),
                1,
                "cancelling a waiter must not consume a permit"
            );
        });
    }

    #[test]
    fn global_cancellation_wakes_a_queued_waiter() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-global:8080/v1", "m", 1);
            let _held = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();

            let waiter_entry = Arc::clone(&entry);
            let waiter = tokio::spawn(async move {
                wait_for_slot(&waiter_entry, None, Duration::from_secs(30), None).await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(entry.waiting(), 1);

            bus::global_cancellation_token().cancel();
            let outcome = waiter.await.expect("waiter task must not panic");
            assert!(
                matches!(outcome, Err(PoolError::Cancelled { .. })),
                "process-wide cancellation must release the waiter, got {outcome:?}"
            );
            assert_eq!(entry.waiting(), 0);
        });
    }

    #[test]
    fn aborted_waiter_leaks_no_permit_and_no_queue_slot() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-abort:8080/v1", "m", 1);
            let held = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();

            let waiter_entry = Arc::clone(&entry);
            let waiter = tokio::spawn(async move {
                wait_for_slot(
                    &waiter_entry,
                    None,
                    Duration::from_secs(30),
                    // A reporter that never returns: proves a mid-wait drop runs
                    // the ledger cleanup on the abort path too.
                    Some(&|_pos| {}),
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(entry.waiting(), 1);

            waiter.abort();
            // Give the aborted task its drop point.
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(
                entry.waiting(),
                0,
                "an aborted waiter must leave the FIFO ledger"
            );

            drop(held);
            assert_eq!(
                entry.available(),
                1,
                "aborted waiter must not consume a permit"
            );
            let next = wait_for_slot(&entry, None, Duration::from_millis(200), None)
                .await
                .expect("the freed slot must be acquirable");
            drop(next);
            assert_eq!(entry.available(), 1);
        });
    }

    #[test]
    fn queue_ticket_drop_leaves_the_ledger_immediately() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-ticket:8080/v1", "m", 1);
            {
                let first = queue_ticket(&entry);
                let second = queue_ticket(&entry);
                assert_eq!(entry.waiting(), 2);
                assert_eq!(first.position().unwrap().position, 1);
                assert_eq!(second.position().unwrap().position, 2);
                assert!(first.ticket() < second.ticket(), "tickets are monotonic");
            }
            assert_eq!(entry.waiting(), 0);
        });
    }

    #[test]
    fn reporter_sees_join_position_and_a_refresh_when_the_queue_shifts() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-report:8080/v1", "m", 1);
            let _held = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();

            let seen: Arc<Mutex<Vec<QueuePosition>>> = Arc::default();

            let first_seen = Arc::clone(&seen);
            let first_entry = Arc::clone(&entry);
            let first = CancellationToken::new();
            let first_task = tokio::spawn(async move {
                let entry = first_entry;
                wait_for_slot(
                    &entry,
                    Some(first),
                    Duration::from_secs(30),
                    Some(&move |pos| first_seen.lock().unwrap().push(pos)),
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;

            // The second waiter queues too, and stays queued (long deadline, its
            // own cancellation token) so the first waiter's refresh has to notice
            // the queue grow behind it.
            let second_token = CancellationToken::new();
            let second_entry = Arc::clone(&entry);
            let second_waiter = second_token.clone();
            let second_task = tokio::spawn(async move {
                wait_for_slot(
                    &second_entry,
                    Some(second_waiter),
                    Duration::from_secs(30),
                    Some(&move |_pos| {}),
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;

            {
                let seen = seen.lock().unwrap();
                assert_eq!(
                    seen.first(),
                    Some(&QueuePosition { position: 1, of: 1 }),
                    "first reporter must see its join position"
                );
            }
            assert_eq!(entry.waiting(), 2, "both waiters must hold queue slots");

            // Past the refresh interval the first waiter must notice that the
            // queue grew behind it, and restate the same rank.
            tokio::time::sleep(QUEUE_POSITION_REFRESH * 2).await;
            {
                let seen = seen.lock().unwrap();
                assert!(
                    seen.contains(&QueuePosition { position: 1, of: 2 }),
                    "queue growth must be re-reported, saw {seen:?}"
                );
                assert!(
                    seen.iter().all(|p| p.position == 1),
                    "rank ahead of us never changes once fixed, saw {seen:?}"
                );
            }

            // Cancel the tail so the freed slot is unambiguously the first
            // waiter's.
            second_token.cancel();
            let second = second_task.await.expect("no panic");
            assert!(
                matches!(second, Err(PoolError::Cancelled { .. })),
                "cancelled tail waiter, got {second:?}"
            );
            assert_eq!(entry.waiting(), 1, "cancelled waiter left the queue");

            drop(_held);
            let first = first_task.await.expect("no panic");
            assert!(first.is_ok(), "first waiter should get the freed slot");
        });
    }

    #[test]
    fn fifo_positions_are_unique_and_stable_across_fifty_entrants() {
        const ENTRANTS: usize = 50;
        pool_test_multi(4, async {
            let entry = entry_with_limit_for_test("http://pool-fifo:8080/v1", "m", 1);
            let barrier = Arc::new(tokio::sync::Barrier::new(ENTRANTS));

            let mut handles = Vec::with_capacity(ENTRANTS);
            for _ in 0..ENTRANTS {
                let entry = Arc::clone(&entry);
                let barrier = Arc::clone(&barrier);
                handles.push(tokio::spawn(async move {
                    let ticket = queue_ticket(&entry);
                    // Nobody reads a position until every entrant holds a ticket,
                    // so `of` is fixed and any order dependence would show up as
                    // a missing or duplicated rank.
                    barrier.wait().await;
                    let first = ticket.position();
                    let again = ticket.position();
                    (ticket, first, again)
                }));
            }

            let mut held = Vec::with_capacity(ENTRANTS);
            for handle in handles {
                let (ticket, first, again) = handle.await.expect("entrant task panicked");
                assert_eq!(first, again, "position must be stable across reads");
                held.push((ticket, first.expect("ticket still queued")));
            }

            assert_eq!(entry.waiting(), ENTRANTS);
            let mut ranks: Vec<usize> = held.iter().map(|(_, p)| p.position).collect();
            ranks.sort_unstable();
            let expected: Vec<usize> = (1..=ENTRANTS).collect();
            assert_eq!(ranks, expected, "ranks must be exactly 1..=N, once each");
            assert!(
                held.iter().all(|(_, p)| p.of == ENTRANTS),
                "every entrant must see the same queue depth"
            );
            let mut tickets: Vec<u64> = held.iter().map(|(t, _)| t.ticket()).collect();
            tickets.sort_unstable();
            tickets.dedup();
            assert_eq!(tickets.len(), ENTRANTS, "tickets must be unique");

            // Removing a middle entrant shifts exactly the tail down by one.
            let middle = held
                .iter()
                .position(|(_, p)| p.position == 25)
                .expect("rank 25 must exist");
            let (dropped, _before) = held.remove(middle);
            assert!(
                dropped.position().is_some(),
                "the dropped entrant must still have held a queue slot"
            );
            drop(dropped);
            assert_eq!(entry.waiting(), ENTRANTS - 1);

            for (ticket, before) in &held {
                let after = ticket.position().expect("still queued");
                assert_eq!(after.of, ENTRANTS - 1);
                let expected_rank = if before.position < 25 {
                    before.position
                } else {
                    before.position - 1
                };
                assert_eq!(
                    after.position, expected_rank,
                    "rank {} must become {expected_rank}",
                    before.position
                );
            }
        });
    }

    #[test]
    fn reserve_holds_one_slot_and_fails_open() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-reserve:8080/v1", "m", 1);
            assert_eq!(entry.reserve_available(), INTERNAL_RESERVE_CAPACITY);

            let first = acquire_reserve_for(&entry, None, Duration::from_millis(20)).await;
            assert!(
                matches!(first, ReserveOutcome::Reserved(_)),
                "first reserve call must get the slot, got {first:?}"
            );

            let started = Instant::now();
            let second = acquire_reserve_for(&entry, None, Duration::from_millis(20)).await;
            match second {
                ReserveOutcome::FailedOpen { waited_secs } => {
                    assert_eq!(waited_secs, 0);
                    assert!(started.elapsed() < Duration::from_secs(2));
                }
                other => panic!("expected FailedOpen, got {other:?}"),
            }
            assert_eq!(
                entry.reserve_available(),
                0,
                "fail-open must not hand out a second slot"
            );

            // Releasing the slot makes it available again.
            if let ReserveOutcome::Reserved(permit) = first {
                drop(permit);
            }
            assert_eq!(entry.reserve_available(), INTERNAL_RESERVE_CAPACITY);
            let third = acquire_reserve_for(&entry, None, Duration::from_millis(20)).await;
            assert!(
                matches!(third, ReserveOutcome::Reserved(_)),
                "reserve slot must be reusable, got {third:?}"
            );

            // The cap semaphore is untouched by reserve traffic.
            assert_eq!(entry.available(), 1);
        });
    }

    #[test]
    fn reserve_cancellation_is_distinguishable() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-reserve-cancel:8080/v1", "m", 1);
            let _held = acquire_reserve_for(&entry, None, Duration::from_millis(20)).await;

            let token = CancellationToken::new();
            let waiter_entry = Arc::clone(&entry);
            let waiter_token = token.clone();
            let waiter = tokio::spawn(async move {
                acquire_reserve_for(&waiter_entry, Some(waiter_token), Duration::from_secs(30))
                    .await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
            let outcome = waiter.await.expect("no panic");
            assert!(
                matches!(outcome, ReserveOutcome::Cancelled),
                "expected Cancelled, got {outcome:?}"
            );
        });
    }

    #[test]
    fn reserve_is_unmetered_when_nothing_is_capped() {
        config_pool_test(None, vec![], async {
            let outcome = acquire_reserve("http://pool-reserve-unset:8080/v1", "m", None).await;
            assert!(
                matches!(outcome, ReserveOutcome::Unmetered),
                "uncapped pair has no reserve to take, got {outcome:?}"
            );
            assert_eq!(open_pool_count(), 0);
        });
    }

    #[test]
    fn call_classes_split_between_cap_and_reserve() {
        let capped = [
            CallClass::ManagerTurn,
            CallClass::Specialist,
            CallClass::Validator,
            CallClass::PromptArchitect,
        ];
        let reserved = [CallClass::SteerArbitrator, CallClass::NoticeEval];

        for class in capped {
            assert!(class.is_capped(), "{class:?} must be capped");
            assert!(!class.uses_reserve(), "{class:?} must not use the reserve");
        }
        for class in reserved {
            assert!(class.uses_reserve(), "{class:?} must use the reserve");
            assert!(
                !class.is_capped(),
                "{class:?} must not queue behind capped work"
            );
        }

        let labels: Vec<&str> = [
            CallClass::ManagerTurn,
            CallClass::Specialist,
            CallClass::Validator,
            CallClass::PromptArchitect,
            CallClass::SteerArbitrator,
            CallClass::NoticeEval,
        ]
        .iter()
        .map(|c| c.label())
        .collect();
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            labels.len(),
            "labels must be distinct: {labels:?}"
        );
    }

    /// A throwaway current-thread runtime for call-context tests. Nothing here
    /// touches pool, config or cancellation state, so no test mutex is needed.
    fn context_test<R>(test: impl future::Future<Output = R>) -> R {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime for the call-context test")
            .block_on(test)
    }

    /// [`context_test`] on a multi-thread runtime, i.e. with real task migration.
    fn context_test_multi<R>(workers: usize, test: impl future::Future<Output = R>) -> R {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(workers)
            .build()
            .expect("multi-thread runtime for the call-context test")
            .block_on(test)
    }

    #[test]
    fn call_context_is_scoped_and_nestable() {
        context_test(async {
            assert_eq!(current_call_context(), None, "starts unset");

            let outer =
                CallContext::for_agent(CallClass::Specialist, "coder", Some("t-006".to_string()));
            let seen = with_call_context(outer.clone(), async {
                assert_eq!(current_call_context().as_ref(), Some(&outer));

                let inner = CallContext::new(CallClass::SteerArbitrator);
                let innermost = with_call_context(inner.clone(), async {
                    assert_eq!(
                        current_call_class(),
                        Some(CallClass::SteerArbitrator),
                        "innermost context wins"
                    );
                    current_call_context()
                })
                .await;

                assert_eq!(innermost.as_ref(), Some(&inner));
                assert_eq!(
                    current_call_context().as_ref(),
                    Some(&outer),
                    "the inner scope must restore the outer one"
                );
                current_call_context()
            })
            .await;

            assert_eq!(seen.as_ref(), Some(&outer));
            assert_eq!(
                current_call_context(),
                None,
                "the outer scope clearing must clear the context"
            );
        });
    }

    /// The regression the task-local replaces: a thread-local context is visible
    /// to every task polled on the same worker thread. On a current-thread
    /// runtime these tasks interleave deterministically, so a thread-local
    /// cross-talks every time (the sibling assertions below fail) while the
    /// task-local cannot.
    #[test]
    fn call_context_does_not_cross_task_boundaries_on_one_thread() {
        context_test(async {
            let manager = tokio::spawn(with_call_context(
                CallContext::for_agent(CallClass::ManagerTurn, MANAGER_AGENT, None),
                async move {
                    for _ in 0..8 {
                        tokio::task::yield_now().await;
                        assert_eq!(
                            current_call_class(),
                            Some(CallClass::ManagerTurn),
                            "a sibling task must not overwrite this task's class"
                        );
                    }
                    current_call_context()
                },
            ));
            let specialist = tokio::spawn(with_call_context(
                CallContext::for_agent(CallClass::Specialist, "coder", Some("t-002".to_string())),
                async move {
                    for _ in 0..8 {
                        tokio::task::yield_now().await;
                        let context = current_call_context().expect("context must stay installed");
                        assert_eq!(context.class, CallClass::Specialist);
                        assert_eq!(
                            context.task.as_deref(),
                            Some("t-002"),
                            "identity must not be replaced by a sibling delegation"
                        );
                    }
                    current_call_context()
                },
            ));
            let unscoped = tokio::spawn(async move {
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                    assert_eq!(
                        current_call_context(),
                        None,
                        "a task that installed nothing must see nothing"
                    );
                }
                current_call_context()
            });

            let (manager, specialist, unscoped) = tokio::join!(manager, specialist, unscoped);
            assert_eq!(
                manager.expect("manager task").map(|c| c.class),
                Some(CallClass::ManagerTurn)
            );
            assert_eq!(
                specialist.expect("specialist task").map(|c| c.class),
                Some(CallClass::Specialist)
            );
            assert_eq!(unscoped.expect("unscoped task"), None);
            assert_eq!(
                current_call_context(),
                None,
                "the driving task was never touched by its children"
            );
        });
    }

    /// A task moves between tokio workers at await points, so the class installed
    /// before an await must still be readable after it, on whichever worker
    /// resumes the task — at any nesting depth.
    #[test]
    fn call_context_survives_awaits_across_worker_threads() {
        context_test_multi(4, async {
            let tasks: Vec<_> = (0..16)
                .map(|index| {
                    let task_id = format!("t-{index:03}");
                    tokio::spawn(with_call_context(
                        CallContext::for_agent(
                            CallClass::Specialist,
                            "researcher",
                            Some(task_id.clone()),
                        ),
                        async move {
                            for _ in 0..12 {
                                tokio::task::yield_now().await;
                                tokio::time::sleep(Duration::from_millis(1)).await;
                                let context = current_call_context()
                                    .expect("the context must survive an await on any worker");
                                assert_eq!(context.class, CallClass::Specialist);
                                assert_eq!(context.agent.as_deref(), Some("researcher"));
                                assert_eq!(context.task.as_deref(), Some(task_id.as_str()));
                            }
                        },
                    ))
                })
                .collect();

            for task in tasks {
                task.await.expect("scoped task ran to completion");
            }
            assert_eq!(
                current_call_context(),
                None,
                "no context may leak back to the driving task"
            );
        });
    }

    /// A scoped future dropped mid-flight (cancellation, timeout, unwind) must
    /// leave nothing installed for whatever runs next on this task.
    #[test]
    fn a_dropped_call_context_scope_leaves_nothing_behind() {
        context_test(async {
            let outcome = tokio::time::timeout(
                Duration::from_millis(10),
                with_call_context(CallContext::new(CallClass::Specialist), async {
                    future::pending::<()>().await;
                }),
            )
            .await;

            assert!(outcome.is_err(), "the scoped future must be cancelled");
            assert_eq!(
                current_call_context(),
                None,
                "a cancelled scope must not leave a context behind"
            );
        });
    }

    /// Task-locals are not inherited by spawned work, so every boundary has to be
    /// crossed on purpose. This pins both halves: the boundary does **not**
    /// inherit, and the explicit re-install works and unwinds cleanly.
    #[test]
    fn call_context_crosses_boundaries_only_when_propagated() {
        context_test(async {
            let outer =
                CallContext::for_agent(CallClass::Specialist, "coder", Some("t-707".to_string()));
            with_call_context(outer.clone(), async {
                // Synchronous code polled inside the same task reads it.
                assert_eq!(current_call_context().as_ref(), Some(&outer));

                let captured = current_call_context();
                let (inherited, propagated) = tokio::task::spawn_blocking(move || {
                    let inherited = current_call_context();
                    let propagated = captured
                        .and_then(|context| with_call_context_sync(context, current_call_context));
                    (inherited, propagated)
                })
                .await
                .expect("blocking closure ran");

                assert_eq!(
                    inherited, None,
                    "a blocking closure must be handed the context explicitly"
                );
                assert_eq!(
                    propagated.as_ref(),
                    Some(&outer),
                    "explicit propagation carries the parent context across"
                );
            })
            .await;

            assert_eq!(current_call_context(), None, "the scope must be gone");
        });
    }

    /// The shape nested `delegate_task` uses: its own OS thread inside `block_on`.
    /// The sync scope must install the inherited context there, let the nested
    /// delegation overwrite it, and restore the parent context when the nested
    /// delegation returns.
    #[test]
    fn nested_sync_scope_overwrites_and_clears_on_a_foreign_thread() {
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let parent = CallContext::for_agent(CallClass::ManagerTurn, MANAGER_AGENT, None);
                with_call_context_sync(parent.clone(), || {
                    assert_eq!(
                        current_call_context().as_ref(),
                        Some(&parent),
                        "the parent context is inherited explicitly"
                    );

                    let nested = CallContext::for_agent(
                        CallClass::Specialist,
                        "debugger",
                        Some("t-808".to_string()),
                    );
                    let seen = with_call_context_sync(nested.clone(), current_call_context);
                    assert_eq!(
                        seen.as_ref(),
                        Some(&nested),
                        "the nested delegation overwrites the inherited context"
                    );
                    assert_eq!(
                        current_call_context().as_ref(),
                        Some(&parent),
                        "the nested context must be cleared when that delegation returns"
                    );
                });
                assert_eq!(current_call_context(), None, "the outer scope clears too");
            });
            worker.join().expect("the nested thread must not panic");
        });
    }

    #[test]
    fn describe_names_the_class_agent_and_task() {
        assert_eq!(
            CallContext::new(CallClass::ManagerTurn).describe(),
            "manager_turn"
        );
        assert_eq!(
            CallContext::for_agent(CallClass::ManagerTurn, MANAGER_AGENT, None).describe(),
            "manager_turn manager"
        );
        assert_eq!(
            CallContext::for_agent(CallClass::Specialist, "coder", Some("t-004".to_string()))
                .describe(),
            "specialist coder [t-004]"
        );
        assert_eq!(
            CallContext {
                class: CallClass::Specialist,
                agent: None,
                task: Some("t-005".to_string()),
            }
            .describe(),
            "specialist [t-005]"
        );
    }

    /// Nesting must be able to re-meter without losing attribution: the parent
    /// context is untouched by `with_class`.
    #[test]
    fn with_class_remeters_without_losing_identity() {
        let specialist =
            CallContext::for_agent(CallClass::Specialist, "coder", Some("t-010".to_string()));
        let remetered = specialist.with_class(CallClass::Validator);

        assert_eq!(remetered.class, CallClass::Validator);
        assert_eq!(remetered.agent.as_deref(), Some("coder"));
        assert_eq!(remetered.task.as_deref(), Some("t-010"));
        assert_eq!(
            specialist.class,
            CallClass::Specialist,
            "the parent context must not be mutated"
        );
    }

    #[test]
    fn snapshot_tracks_limits_permits_and_queue_depth() {
        pool_test(async {
            let entry = entry_with_limit_for_test("http://pool-snapshot:8080/v1", "m", 2);
            assert_eq!(
                entry.snapshot(),
                PoolSnapshot {
                    limit: 2,
                    available: 2,
                    waiting: 0,
                    reserve_available: INTERNAL_RESERVE_CAPACITY,
                }
            );

            let held = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();
            let held_second = wait_for_slot(&entry, None, Duration::from_millis(50), None)
                .await
                .unwrap();
            assert_eq!(
                entry.snapshot(),
                PoolSnapshot {
                    limit: 2,
                    available: 0,
                    waiting: 0,
                    reserve_available: INTERNAL_RESERVE_CAPACITY,
                },
                "both slots held, nobody queued"
            );

            let queued_entry = Arc::clone(&entry);
            let queued = tokio::spawn(async move {
                wait_for_slot(
                    &queued_entry,
                    None,
                    Duration::from_millis(400),
                    Some(&|_pos| {}),
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(
                entry.snapshot(),
                PoolSnapshot {
                    limit: 2,
                    available: 0,
                    waiting: 1,
                    reserve_available: INTERNAL_RESERVE_CAPACITY,
                }
            );

            drop(held_second);
            let got = queued.await.expect("no panic").expect("must get the slot");
            assert_eq!(got.queue_position().unwrap().position, 1);
            assert_eq!(got.limit(), 2);
            drop(held);
            drop(got);
            assert_eq!(entry.waiting(), 0);
            assert_eq!(entry.available(), 2);
        });
    }

    #[test]
    fn current_worker_token_reads_the_task_local() {
        pool_test(async {
            assert!(
                current_worker_token().is_none(),
                "no worker scope => no token"
            );

            let token = CancellationToken::new();
            let scoped = token.clone();
            let inner = bus::CURRENT_WORKER_TOKEN
                .scope(scoped, async move { current_worker_token() })
                .await;
            assert_eq!(
                inner.map(|t| t.is_cancelled()),
                Some(false),
                "token must be readable inside its scope"
            );
            assert!(current_worker_token().is_none(), "scope must not leak");
        });
    }
}
