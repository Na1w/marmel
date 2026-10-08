//! Steer notice correlation, worker inbox queuing, and arbitrator-mediated dialogue.
//!
//! Enables bi-directional steering: when the Steer Arbitrator forwards a user
//! inquiry to an active worker, a unique `notice_id` is assigned and queued to the
//! worker's inbox. The worker can inspect the notice and reply using the
//! `reply_to_arbitrator` tool. The Arbitrator mediates the reply (synthesizing
//! the response for the user or posing internal follow-ups) before presenting it.
//!
//! Every queue in this module is bounded and self-cleaning (recon H4): notices
//! expire after [`NOTICE_TTL_MS`], each worker inbox holds at most
//! [`INBOX_CAPACITY`] notices (overflow policy: **drop-oldest**, see
//! [`post_notice_to_worker_tracked`]), the reply store is a bounded ring
//! ([`REPLIED_NOTICES_CAP`]), and per-worker inbox *entries* are reclaimed once
//! the worker is gone instead of leaking map entries. Nothing is discarded
//! without a trace: every drop is counted ([`notice_lifecycle_stats`]), reported
//! on the returning outcome types, and logged with a bounded `tracing::warn!`.
//!
//! Reply targeting is **explicit** (recon H6): a reply only ever resolves the
//! notice whose id it names, and only when that notice addresses the replying
//! worker ([`record_worker_reply_for_notice`], [`notice_addresses_worker`]).
//! There is no "pick the pending notice" and no text matching — a reply that
//! names the wrong, an unknown, or no notice id is rejected with a typed
//! [`NoticeReplyRejection`] and a `tracing::warn!`.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::harness::HarnessStats;
use crate::llm::ChatClient;
use crate::types::{ChatRequest, Message};

static NEXT_NOTICE_ID: AtomicU64 = AtomicU64::new(1);

/// Generate the next unique steering notice identifier.
pub fn next_notice_id() -> String {
    let id = NEXT_NOTICE_ID.fetch_add(1, Ordering::SeqCst);
    format!("notice-{id}")
}

/// A steering notice dispatched by the Steer Arbitrator to an active specialist worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SteerNotice {
    pub notice_id: String,
    pub user_inquiry: String,
    pub target_worker: String,
    pub created_at_ms: u64,
}

/// A specialist worker's reply back to the Steer Arbitrator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SteerNoticeReply {
    pub notice_id: String,
    pub worker_tag: String,
    pub reply_message: String,
    pub replied_at_ms: u64,
}

/// The Arbitrator's evaluation of a specialist's reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerReplyEvaluation {
    /// "SynthesizeResponse" | "AskFollowUp"
    pub decision: String,
    /// Direct, factual response synthesized for the user (in user's language).
    pub response: Option<String>,
    /// Targeted follow-up question for the worker (in English) if more info is needed before answering the user.
    pub follow_up_prompt: Option<String>,
    /// Optional status notice to display to the user while follow-up is in flight.
    pub user_status: Option<String>,
}

// ---------------------------------------------------------------------------
// Storage lifecycle (recon H4): TTL expiry, bounded inboxes, entry reclamation
// ---------------------------------------------------------------------------

/// Notices awaiting a specialist reply, keyed by `notice_id`.
static PENDING_NOTICES: LazyLock<DashMap<String, SteerNotice>> = LazyLock::new(DashMap::new);
/// Per-target notice queues. Keyed by the **address the notice was posted to**
/// (an effective worker key, a role family, a task id, or a broadcast
/// wildcard); which worker actually owns a queued notice is decided at drain
/// time by [`worker_routing_identity`].
static WORKER_INBOXES: LazyLock<DashMap<String, Inbox>> = LazyLock::new(DashMap::new);
/// Replies already recorded by specialists, keyed by `notice_id`. Bounded ring
/// so a long session cannot accumulate replies forever.
static REPLIED_NOTICES: LazyLock<DashMap<String, StoredReply>> = LazyLock::new(DashMap::new);

/// One notice queue, plus the timestamp of its last structural change (push,
/// drain, or sweep). The timestamp drives idle reclamation of map entries.
#[derive(Debug, Default)]
struct Inbox {
    notices: Vec<SteerNotice>,
    last_touched_ms: u64,
}

/// A recorded reply plus the eviction sequence handed to it (oldest = smallest).
#[derive(Debug, Clone)]
struct StoredReply {
    reply: SteerNoticeReply,
    seq: u64,
}

/// Monotonic sequence handed to recorded replies, used to evict the oldest one
/// when the bounded reply ring is over capacity.
static REPLIED_SEQ: AtomicU64 = AtomicU64::new(0);

/// How long a steering notice stays deliverable.
///
/// Rationale: a notice is posted while a specialist is mid-flight and must
/// survive the longest realistic gap between two drain points — one streamed
/// model turn (the streaming path itself is capped by a 60 s timeout) plus its
/// tool round, whose builds/test suites run for minutes, optionally followed by
/// validator fix-loop rounds. Ten minutes sits comfortably above that envelope,
/// while still guaranteeing the notice can never outlive the worker/session it
/// was meant for — which is precisely the recon H4 failure mode (a notice
/// posted on a worker's last turn sat in `PENDING_NOTICES` for the whole
/// process and poisoned later correlation).
pub const NOTICE_TTL_MS: u64 = 10 * 60 * 1000;

/// How long an **empty** per-worker inbox entry survives before it is reclaimed.
/// Equal to the notice TTL: an inbox only ever stays empty because its notices
/// expired or were drained, and either way nothing is expected of it after one
/// TTL window. This is what stops dead worker keys from leaking map entries.
pub const INBOX_IDLE_TTL_MS: u64 = NOTICE_TTL_MS;

/// Maximum number of undelivered notices one worker inbox holds.
///
/// **Overflow policy: drop-oldest.** When a post would exceed the capacity, the
/// *oldest* queued notices are evicted and the newest one is kept. Rationale:
/// steering is last-write-wins user intent — a specialist that has not drained
/// its inbox within one TTL window is by definition behind, so the newest
/// instruction is the actionable one and the stale one is the one already
/// overtaken by events. Dropping the newest would silently ignore the user's
/// latest instruction, which is strictly worse.
///
/// The eviction is never silent: the evicted notices are returned by
/// [`post_notice_to_worker_tracked`], they are removed from `PENDING_NOTICES`
/// (so a later reply to them is rejected explicitly instead of matching a
/// notice nobody will ever act on), they are counted in
/// [`notice_lifecycle_stats`], and they are named in a bounded `tracing::warn!`.
pub const INBOX_CAPACITY: usize = 64;

/// Hard bound on simultaneously retained per-worker inbox entries. A target
/// string nobody ever drains (a typo'd or long-gone worker key spelled out by a
/// chatty arbitrator) is evicted oldest-first once this bound is exceeded.
pub const MAX_INBOX_KEYS: usize = 256;

/// Maximum number of recorded specialist replies retained by
/// [`record_worker_reply`] / [`get_worker_reply`]. Oldest-first eviction.
pub const REPLIED_NOTICES_CAP: usize = 64;

/// Number of notice ids named in a lifecycle warning.
const LIFECYCLE_LOG_ID_SAMPLE: usize = 5;

/// Bounded-warning policy: the first `WARN_VERBOSITY` occurrences of a lifecycle
/// class are logged in full, afterwards only every `WARN_EVERY`th. A pathological
/// producer therefore cannot turn a sweep into per-entry log spam.
const WARN_VERBOSITY: u64 = 5;
const WARN_EVERY: u64 = 10;

/// Lifecycle counters — the observable side of every drop/reclaim.
static EXPIRED_NOTICE_TOTAL: AtomicU64 = AtomicU64::new(0);
static CAPACITY_EVICTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static RECLAIMED_INBOX_TOTAL: AtomicU64 = AtomicU64::new(0);
static EXPIRY_WARN_SEQ: AtomicU64 = AtomicU64::new(0);
static OVERFLOW_WARN_SEQ: AtomicU64 = AtomicU64::new(0);
static EVICTION_WARN_SEQ: AtomicU64 = AtomicU64::new(0);
static REPLY_WARN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Virtual offset added to the wall clock by the notice lifecycle.
///
/// This is the injectable-clock seam: tests shift it instead of sleeping, so
/// expiry behaviour is asserted deterministically. It is only ever non-zero in
/// tests ([`advance_notice_clock_ms`] is `#[cfg(test)]`), and it offsets *both*
/// posting timestamps and age computations, so it behaves like "the clock moved"
/// rather than like "the TTL changed".
static CLOCK_OFFSET_MS: AtomicI64 = AtomicI64::new(0);

/// Epoch milliseconds as seen by the notice lifecycle: the same wall clock that
/// stamps `created_at_ms`, plus the test offset. No other time source is used.
fn notice_now_ms() -> u64 {
    let wall_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    wall_ms
        .saturating_add(CLOCK_OFFSET_MS.load(Ordering::Relaxed))
        .max(0) as u64
}

/// Current age of `notice` in milliseconds (0 for a notice from the future).
pub fn notice_age_ms(notice: &SteerNotice) -> u64 {
    notice_now_ms().saturating_sub(notice.created_at_ms)
}

/// True once `notice` is at least [`NOTICE_TTL_MS`] old.
pub fn is_notice_expired(notice: &SteerNotice) -> bool {
    notice_age_ms(notice) >= NOTICE_TTL_MS
}

/// Shift the notice clock forward, without sleeping (test seam).
#[cfg(test)]
pub fn advance_notice_clock_ms(delta_ms: u64) {
    CLOCK_OFFSET_MS.fetch_add(delta_ms as i64, Ordering::SeqCst);
}

/// The notice clock used by this module (test seam for expiry assertions).
#[cfg(test)]
pub fn notice_now_ms_for_test() -> u64 {
    notice_now_ms()
}

/// Snapshot of the lifecycle counters and current storage occupancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoticeLifecycleStats {
    pub pending_notices: usize,
    pub inbox_keys: usize,
    pub queued_notices: usize,
    pub recorded_replies: usize,
    pub expired_dropped_total: u64,
    pub capacity_evicted_total: u64,
    pub inboxes_reclaimed_total: u64,
}

/// Observable totals for every storage-lifecycle action taken so far.
pub fn notice_lifecycle_stats() -> NoticeLifecycleStats {
    let queued_notices: usize = WORKER_INBOXES.iter().map(|e| e.value().notices.len()).sum();
    NoticeLifecycleStats {
        pending_notices: PENDING_NOTICES.len(),
        inbox_keys: WORKER_INBOXES.len(),
        queued_notices,
        recorded_replies: REPLIED_NOTICES.len(),
        expired_dropped_total: EXPIRED_NOTICE_TOTAL.load(Ordering::Relaxed),
        capacity_evicted_total: CAPACITY_EVICTED_TOTAL.load(Ordering::Relaxed),
        inboxes_reclaimed_total: RECLAIMED_INBOX_TOTAL.load(Ordering::Relaxed),
    }
}

/// Observable per-worker occupancy: how many notices are currently queued in the
/// inbox addressed by `worker_key` (normalised exactly like a post).
///
/// This is the diagnostics seam for the bounded-inbox policy: callers can check
/// one worker's queue depth without reading process-global totals.
pub fn worker_inbox_len(worker_key: &str) -> usize {
    let key = worker_key.trim().to_ascii_lowercase();
    WORKER_INBOXES
        .get(&key)
        .map(|entry| entry.notices.len())
        .unwrap_or(0)
}

/// Whether an inbox map entry exists at all for `worker_key`.
///
/// Distinguishes "queue emptied but map entry reclaimed" from "empty queue left
/// behind in the map", which is how dead-worker entries would leak.
pub fn has_worker_inbox(worker_key: &str) -> bool {
    let key = worker_key.trim().to_ascii_lowercase();
    WORKER_INBOXES.contains_key(&key)
}

/// Log one occurrence of a bounded lifecycle warning class.
fn warn_lifecycle(counter: &AtomicU64, message: impl AsRef<str>) {
    let occurrence = counter.fetch_add(1, Ordering::Relaxed);
    if occurrence < WARN_VERBOSITY || occurrence.is_multiple_of(WARN_EVERY) {
        tracing::warn!("{}", message.as_ref());
    }
}

/// Format a bounded id sample for lifecycle warnings.
fn id_sample(ids: &[String]) -> String {
    let head: Vec<&str> = ids
        .iter()
        .take(LIFECYCLE_LOG_ID_SAMPLE)
        .map(String::as_str)
        .collect();
    if ids.len() > LIFECYCLE_LOG_ID_SAMPLE {
        format!(
            "{} (+{} more)",
            head.join(", "),
            ids.len() - LIFECYCLE_LOG_ID_SAMPLE
        )
    } else {
        head.join(", ")
    }
}

/// What one storage sweep dropped/reclaimed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoticePruneReport {
    /// Distinct notice ids dropped because they were older than [`NOTICE_TTL_MS`].
    pub expired_dropped: usize,
    /// Their ids (the full list; the log warning samples the first few).
    pub expired_ids: Vec<String>,
    /// Empty per-worker inbox entries reclaimed.
    pub reclaimed_inboxes: usize,
    /// Inbox keys evicted by the [`MAX_INBOX_KEYS`] bound.
    pub evicted_inbox_keys: usize,
    /// Undelivered notices dropped along with those evicted inbox keys.
    pub notices_dropped_by_key_eviction: usize,
}

/// Deterministic TTL/reclaim sweep, run on **every** post and every drain.
///
/// Every entry is judged against one clock snapshot (`now_ms`), so a sweep can
/// never classify two entries written in the same instant differently. Cost is
/// bounded by [`MAX_INBOX_KEYS`] × [`INBOX_CAPACITY`] entries, and the DashMap
/// guards are always released before any removal (never iterate and remove in
/// the same scope).
fn prune_notice_storage() -> NoticePruneReport {
    let now_ms = notice_now_ms();
    let mut report = NoticePruneReport::default();
    let mut expired_ids: Vec<String> = Vec::new();

    // 1. Expired queued notices, per inbox. Guards dropped before removing.
    let inbox_keys: Vec<String> = WORKER_INBOXES.iter().map(|e| e.key().clone()).collect();
    for key in inbox_keys {
        let mut expired_here: Vec<String> = Vec::new();
        let mut now_empty = false;
        let mut idle = false;
        if let Some(mut entry) = WORKER_INBOXES.get_mut(&key) {
            let inbox = entry.value_mut();
            inbox.notices.retain(|notice| {
                let alive = now_ms.saturating_sub(notice.created_at_ms) < NOTICE_TTL_MS;
                if !alive {
                    expired_here.push(notice.notice_id.clone());
                }
                alive
            });
            now_empty = inbox.notices.is_empty();
            idle = now_ms.saturating_sub(inbox.last_touched_ms) >= INBOX_IDLE_TTL_MS;
        }
        let had_expired = !expired_here.is_empty();
        expired_ids.extend(expired_here);

        // An inbox is reclaimed as soon as it holds nothing and nothing is
        // expected of it any more: either its last notices just aged out, or it
        // has been empty and untouched for a whole idle window. This is what
        // keeps keys of dead workers from leaking in the map.
        if now_empty
            && (had_expired || idle)
            && WORKER_INBOXES
                .remove_if(&key, |_key, inbox| inbox.notices.is_empty())
                .is_some()
        {
            report.reclaimed_inboxes += 1;
            RECLAIMED_INBOX_TOTAL.fetch_add(1, Ordering::Relaxed);
        }
    }

    // 2. Expired pending notices (delivered-but-unanswered, or never answered).
    let expired_pending: Vec<String> = PENDING_NOTICES
        .iter()
        .filter(|e| now_ms.saturating_sub(e.value().created_at_ms) >= NOTICE_TTL_MS)
        .map(|e| e.key().clone())
        .collect();
    for id in expired_pending {
        if PENDING_NOTICES.remove(&id).is_some() {
            expired_ids.push(id);
        }
    }

    expired_ids.sort();
    expired_ids.dedup();
    if !expired_ids.is_empty() {
        report.expired_dropped = expired_ids.len();
        report.expired_ids = expired_ids.clone();
        EXPIRED_NOTICE_TOTAL.fetch_add(expired_ids.len() as u64, Ordering::Relaxed);
        warn_lifecycle(
            &EXPIRY_WARN_SEQ,
            format!(
                "steering notices expired after {} ms without being acted on: {} ({} notice(s) dropped from pending + worker inboxes; worker keys that never drain lose their inbox entries)",
                NOTICE_TTL_MS,
                id_sample(&expired_ids),
                expired_ids.len()
            ),
        );
    }

    // 3. Hard bound on distinct inbox keys (targets nobody ever drains).
    if WORKER_INBOXES.len() > MAX_INBOX_KEYS {
        let mut entries: Vec<(String, u64)> = WORKER_INBOXES
            .iter()
            .map(|e| (e.key().clone(), e.value().last_touched_ms))
            .collect();
        entries.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let over = entries.len() - MAX_INBOX_KEYS;
        let mut evicted_ids: Vec<String> = Vec::new();
        for (key, _) in entries.into_iter().take(over) {
            if let Some((_, inbox)) = WORKER_INBOXES.remove(&key) {
                report.evicted_inbox_keys += 1;
                for notice in inbox.notices {
                    PENDING_NOTICES.remove(&notice.notice_id);
                    evicted_ids.push(notice.notice_id);
                }
            }
        }
        report.notices_dropped_by_key_eviction = evicted_ids.len();
        if !evicted_ids.is_empty() {
            CAPACITY_EVICTED_TOTAL.fetch_add(evicted_ids.len() as u64, Ordering::Relaxed);
            warn_lifecycle(
                &EVICTION_WARN_SEQ,
                format!(
                    "worker-inbox table over its bound of {MAX_INBOX_KEYS} targets: evicted {} stale target(s), dropping {} undelivered notice(s): {}",
                    report.evicted_inbox_keys,
                    evicted_ids.len(),
                    id_sample(&evicted_ids)
                ),
            );
        }
    }

    report
}

#[cfg(test)]
pub static TEST_NOTICE_MUTEX: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Outcome of a post: the stored notice plus everything the storage layer had
/// to drop to stay within its bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticePostOutcome {
    pub notice: SteerNotice,
    /// Older notices evicted from the target inbox by the drop-oldest policy.
    pub evicted_older: Vec<SteerNotice>,
    /// Report of the TTL/reclaim sweep this post triggered.
    pub prune: NoticePruneReport,
}

/// Post a steering notice destined for a worker.
pub fn post_notice_to_worker(
    target_worker: &str,
    user_inquiry: &str,
    notice_id_override: Option<&str>,
) -> SteerNotice {
    post_notice_to_worker_tracked(target_worker, user_inquiry, notice_id_override).notice
}

/// Post a steering notice and report what the bounded storage did about it.
///
/// Same posting semantics as [`post_notice_to_worker`], but observable: the
/// per-inbox capacity [`INBOX_CAPACITY`] applies a documented **drop-oldest**
/// policy, and the evicted notices are handed back here (they are also removed
/// from `PENDING_NOTICES` and counted in [`notice_lifecycle_stats`]).
pub fn post_notice_to_worker_tracked(
    target_worker: &str,
    user_inquiry: &str,
    notice_id_override: Option<&str>,
) -> NoticePostOutcome {
    let prune = prune_notice_storage();

    let notice_id = notice_id_override
        .map(str::to_string)
        .unwrap_or_else(next_notice_id);

    let now_ms = notice_now_ms();

    let clean_target = target_worker.trim().to_ascii_lowercase();
    let notice = SteerNotice {
        notice_id: notice_id.clone(),
        user_inquiry: user_inquiry.to_string(),
        target_worker: clean_target.clone(),
        created_at_ms: now_ms,
    };

    PENDING_NOTICES.insert(notice_id, notice.clone());

    let mut evicted_older: Vec<SteerNotice> = Vec::new();
    {
        let mut entry = WORKER_INBOXES
            .entry(clean_target.clone())
            .or_insert_with(|| Inbox {
                notices: Vec::new(),
                last_touched_ms: now_ms,
            });
        let inbox = entry.value_mut();
        inbox.last_touched_ms = now_ms;
        inbox.notices.push(notice.clone());
        while inbox.notices.len() > INBOX_CAPACITY {
            evicted_older.push(inbox.notices.remove(0));
        }
    }

    if !evicted_older.is_empty() {
        for evicted in &evicted_older {
            PENDING_NOTICES.remove(&evicted.notice_id);
        }
        let dropped = evicted_older.len();
        CAPACITY_EVICTED_TOTAL.fetch_add(dropped as u64, Ordering::Relaxed);
        let ids: Vec<String> = evicted_older.iter().map(|n| n.notice_id.clone()).collect();
        warn_lifecycle(
            &OVERFLOW_WARN_SEQ,
            format!(
                "inbox for '{clean_target}' exceeded its capacity of {INBOX_CAPACITY}: dropped the {dropped} oldest undelivered notice(s) ({}) — newest steering instructions win, and the dropped ids are no longer pending, so a reply to them is rejected explicitly",
                id_sample(&ids)
            ),
        );
    }

    NoticePostOutcome {
        notice,
        evicted_older,
        prune,
    }
}

/// Separator marking a collision-resolved ("disambiguated") worker key such as
/// `coder-t-001#7`. Mirrors the private `KEY_DISAMBIGUATOR` of
/// [`crate::orchestrator::workers`]: everything before it is the worker's
/// natural `{agent}-{task}` key, everything after it is an opaque registry
/// handle carrying the id from `WORKER_ID_SEQ`.
const WORKER_KEY_DISAMBIGUATOR: char = '#';

/// Segment separator inside a worker identity (both agent names and task ids are
/// themselves hyphenated, so role matching is anchored at these boundaries).
const WORKER_KEY_SEGMENT: char = '-';

/// Addresses that every worker drains (broadcast steering), kept from the
/// historical routing rules.
const BROADCAST_TARGETS: [&str; 2] = ["*", "worker"];

/// The exact routing identity of one worker, resolved from its registry key.
///
/// The registry stores `WorkerState { info.task_id, info.agent_name, .. }` under
/// a composite key built by
/// [`crate::orchestrator::workers::register_active_worker_with_token`] as
/// `{agent_name}-{task_id}` (or `{agent_name}-w{id}` when there is no task id),
/// plus a `{base}#{id}` suffix when that natural key was already held by a live
/// worker. Those two identity fields are what notices are addressed to, so they
/// are recovered here by *structure* (disambiguator + canonical task-id token)
/// instead of by substring scanning the composite key (recon H1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerRoutingIdentity {
    /// Effective registry key, e.g. `coder-t-001#7`. Used **only** as an opaque
    /// handle for exact equality with an address that spells out that whole key.
    pub(crate) effective_key: String,
    /// Natural key — the effective key with the `#…` disambiguator removed.
    pub(crate) natural_key: String,
    /// Authoritative `WorkerState.info.agent_name` (lower-cased), e.g.
    /// `validator-coder`.
    pub(crate) agent_name: String,
    /// Authoritative `WorkerState.info.task_id` (lower-cased) when the key
    /// carries one, e.g. `t-001`.
    pub(crate) task_id: Option<String>,
}

/// Split a natural `{agent}-{task}` key into its authoritative fields.
///
/// The task id is the *longest* trailing segment that satisfies the canonical
/// task-id grammar ([`crate::plan_parse::is_task_id_token`]), which is exactly
/// what `format!("{agent_name}-{task_id}")` produces at registration time — so
/// `validator-coder-t-001` yields `("validator-coder", Some("t-001"))` and the
/// role name `coder` is *not* a routing candidate for it. A key without any
/// task-shaped segment keeps the whole natural key as the agent name, except
/// for the synthetic `{agent}-w{n}` suffix handed out to task-id-less workers,
/// which is stripped so those workers remain addressable by role name.
fn split_natural_key(natural_key: &str) -> (String, Option<String>) {
    for (idx, ch) in natural_key.char_indices() {
        if ch != WORKER_KEY_SEGMENT || idx == 0 {
            continue;
        }
        let tail = &natural_key[idx + 1..];
        if crate::plan_parse::is_task_id_token(tail) {
            return (natural_key[..idx].to_string(), Some(tail.to_string()));
        }
    }

    // Synthetic worker id from `WORKER_ID_SEQ` (`{agent}-w{n}`), not a task id.
    if let Some(idx) = natural_key.rfind(WORKER_KEY_SEGMENT)
        && idx > 0
    {
        let tail = &natural_key[idx + 1..];
        let is_synthetic_worker_id = tail
            .strip_prefix('w')
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()));
        if is_synthetic_worker_id {
            return (natural_key[..idx].to_string(), None);
        }
    }

    (natural_key.to_string(), None)
}

/// Resolve the exact routing identity of the worker addressed by `worker_key`.
pub(crate) fn worker_routing_identity(worker_key: &str) -> WorkerRoutingIdentity {
    let effective_key = worker_key.trim().to_ascii_lowercase();

    // `#…` is an opaque collision handle, never part of the identity.
    let natural_key = match effective_key.find(WORKER_KEY_DISAMBIGUATOR) {
        Some(idx) => effective_key[..idx].to_string(),
        None => effective_key.clone(),
    };

    let (agent_name, task_id) = split_natural_key(&natural_key);

    WorkerRoutingIdentity {
        effective_key,
        natural_key,
        agent_name,
        task_id,
    }
}

impl WorkerRoutingIdentity {
    /// True when `target` addresses this worker **exactly**.
    ///
    /// Accepted addresses (in order): the broadcast wildcards `*` / `worker`;
    /// the exact effective registry key; the exact natural `{agent}-{task}` key;
    /// the exact agent name, or a leading role-family segment of it
    /// (`validator` → agent `validator-coder`); the exact task id. Every rule is
    /// a whole-field equality at a segment boundary — `contains` / unanchored
    /// prefix matching is deliberately absent, which is what let a
    /// `validator-coder-t-001` worker steal notices addressed to `coder`, and a
    /// `coder-t-00` prefix steal notices addressed to `coder-t-001` (recon H1).
    pub(crate) fn routes(&self, target: &str) -> bool {
        // Same decoration tolerance used for task ids everywhere else
        // (`[coder-t-001]` and `"t-001"` address the same worker).
        let target = crate::task_id::normalize_task_id_ref(target).to_ascii_lowercase();

        if target.is_empty() {
            return false;
        }

        if BROADCAST_TARGETS.contains(&target.as_str()) {
            return true;
        }

        if target == self.effective_key || target == self.natural_key {
            return true;
        }

        if target == self.agent_name
            || self
                .agent_name
                .starts_with(&format!("{target}{WORKER_KEY_SEGMENT}"))
        {
            return true;
        }

        self.task_id
            .as_deref()
            .is_some_and(|tid| crate::plan_parse::task_id_eq(tid, &target))
    }
}

/// What a drain did, beyond the notices it handed out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoticeDrainOutcome {
    /// Notices delivered to this worker, oldest first.
    pub delivered: Vec<SteerNotice>,
    /// Notices dropped by the TTL sweep this drain triggered.
    pub expired_dropped: usize,
    /// Their ids (sampled).
    pub expired_ids: Vec<String>,
    /// Inbox map entries reclaimed (empty inboxes of drained/gone workers).
    pub reclaimed_inboxes: usize,
}

/// Drain all pending notices addressed to this worker.
///
/// `worker_key` is the worker's **effective registry key** (the `ActiveWorkerGuard`
/// key, e.g. `coder-t-001` or the collision-disambiguated `coder-t-001#7`). It is
/// treated as an opaque handle: routing compares the notice target against the
/// exact identity fields recovered from it (agent name, task id, natural key,
/// whole key) and against the broadcast wildcards — never against a substring of
/// the key (recon H1).
///
/// Each drain also runs the TTL/reclaim sweep, and an inbox that has been fully
/// drained is removed from the table instead of lingering as an empty `Vec`
/// (recon H4).
pub fn drain_worker_notices(worker_key: &str) -> Vec<SteerNotice> {
    drain_worker_notices_report(worker_key).delivered
}

/// Drain notices for a worker **mid-turn** — i.e. from inside a live specialist
/// turn, not only at turn start (recon H4).
///
/// Identical routing semantics to [`drain_worker_notices`]; the separate name is
/// the seam for the live loop: it is cheap, idempotent, self-cleaning, and safe
/// to call after each tool round, so a notice posted while the specialist is
/// inside a long turn reaches the model on the *next* request (or before the
/// turn's terminal verdict) instead of only after the current turn finishes —
/// and a notice posted on what turns out to be the worker's last turn still
/// reaches it instead of rotting in the inbox until the TTL drops it.
pub fn drain_worker_notices_mid_turn(worker_key: &str) -> Vec<SteerNotice> {
    drain_worker_notices_report(worker_key).delivered
}

/// Full-visibility drain: the delivered notices plus what the sweep expired and
/// how many inbox entries were reclaimed.
pub fn drain_worker_notices_report(worker_key: &str) -> NoticeDrainOutcome {
    let prune = prune_notice_storage();
    let identity = worker_routing_identity(worker_key);

    // Collect the matching keys first: DashMap iterators must never be alive
    // while an entry is removed.
    let matched: Vec<String> = WORKER_INBOXES
        .iter()
        .filter(|e| identity.routes(e.key()))
        .map(|e| e.key().clone())
        .collect();

    let mut delivered: Vec<SteerNotice> = Vec::new();
    let mut reclaimed_inboxes = prune.reclaimed_inboxes;

    for key in matched {
        if let Some((_, inbox)) = WORKER_INBOXES.remove(&key) {
            if inbox.notices.is_empty() {
                reclaimed_inboxes += 1;
                RECLAIMED_INBOX_TOTAL.fetch_add(1, Ordering::Relaxed);
            }
            delivered.extend(inbox.notices);
        }
    }

    // Chronological order across every inbox that routed to this worker
    // (stable sort, so inboxes drained in the same millisecond keep their order).
    delivered.sort_by_key(|notice| notice.created_at_ms);

    NoticeDrainOutcome {
        delivered,
        expired_dropped: prune.expired_dropped,
        expired_ids: prune.expired_ids,
        reclaimed_inboxes,
    }
}

/// What [`reclaim_worker_inbox`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InboxReclaimReport {
    /// Empty inbox entries removed for this worker.
    pub entries_removed: usize,
    /// Queued notices deliberately **kept** (a reclaim never discards a
    /// notice — undelivered notices stay until the TTL sweep reports them).
    pub retained_queued_notices: usize,
}

/// Reclaim the inbox entries belonging to a worker that is gone.
///
/// Measured on the same exact-match routing as a drain, and deliberately only
/// removes **empty** entries: a queued notice is never discarded here, because a
/// replacement worker registered under the same key (a retry of the same task)
/// must still be steerable. Notices a departed worker never drained are left to
/// the TTL sweep, which drops them deterministically and logs them.
///
/// This belongs on the worker-teardown path (`ActiveWorkerGuard::drop` in
/// `src/orchestrator/workers.rs`) so a finished worker leaves no map entries
/// behind; the drain-time removal in [`drain_worker_notices_report`] and the
/// idle window in [`prune_notice_storage`] are the safety nets for workers that
/// never call it.
pub fn reclaim_worker_inbox(worker_key: &str) -> InboxReclaimReport {
    let identity = worker_routing_identity(worker_key);
    let keys: Vec<(String, usize)> = WORKER_INBOXES
        .iter()
        .filter(|e| identity.routes(e.key()))
        .map(|e| (e.key().clone(), e.value().notices.len()))
        .collect();

    let mut report = InboxReclaimReport::default();
    for (key, queued) in keys {
        if queued > 0 {
            report.retained_queued_notices += queued;
            continue;
        }
        if WORKER_INBOXES
            .remove_if(&key, |_key, inbox| inbox.notices.is_empty())
            .is_some()
        {
            report.entries_removed += 1;
            RECLAIMED_INBOX_TOTAL.fetch_add(1, Ordering::Relaxed);
        }
    }
    report
}

// ---------------------------------------------------------------------------
// Reply targeting (recon H6): a reply must name the notice it answers
// ---------------------------------------------------------------------------

/// Why a specialist reply was refused by [`record_worker_reply_for_notice`].
///
/// Every variant is terminal and deterministic: nothing is recorded in the
/// reply store, no pending notice is consumed, and **no notice is ever
/// guessed**. The historical `PENDING_NOTICES.len() == 1 → take the first one`
/// branch has been removed, so a reply that does not name exactly one pending
/// notice addressed to the replying worker can no longer resolve a different
/// notice (the recon H6 failure mode: answering notice B closed notice A, and
/// `evaluate_worker_reply` then answered the user from the wrong
/// `user_inquiry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeReplyRejection {
    /// The reply carried no notice id at all (empty / whitespace-only).
    MissingNoticeId,
    /// The id names nothing currently pending: it never existed, or it expired
    /// or was capacity-evicted before the reply arrived.
    UnknownNoticeId {
        notice_id: String,
        worker_tag: String,
    },
    /// The id was already resolved by an earlier reply (and has not been
    /// re-posted since), so this is a duplicate.
    AlreadyReplied {
        notice_id: String,
        worker_tag: String,
    },
    /// The id is a pending notice, but it addresses a **different worker**.
    WorkerMismatch {
        notice_id: String,
        worker_tag: String,
        addressed_to: String,
    },
}

impl NoticeReplyRejection {
    /// Stable machine-readable class, for callers that need to branch on the
    /// reason without matching on prose.
    pub fn kind(&self) -> &'static str {
        match self {
            NoticeReplyRejection::MissingNoticeId => "missing-notice-id",
            NoticeReplyRejection::UnknownNoticeId { .. } => "unknown-notice-id",
            NoticeReplyRejection::AlreadyReplied { .. } => "already-replied",
            NoticeReplyRejection::WorkerMismatch { .. } => "worker-mismatch",
        }
    }
}

impl std::fmt::Display for NoticeReplyRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoticeReplyRejection::MissingNoticeId => write!(
                f,
                "a reply must carry an explicit `notice_id` naming the steering notice it answers (the id shown in the delivered notice); no notice is guessed"
            ),
            NoticeReplyRejection::UnknownNoticeId {
                notice_id,
                worker_tag,
            } => write!(
                f,
                "notice '{notice_id}' is not pending for worker '{worker_tag}' (unknown, expired, or dropped before this reply) — no notice was guessed instead"
            ),
            NoticeReplyRejection::AlreadyReplied {
                notice_id,
                worker_tag,
            } => write!(
                f,
                "notice '{notice_id}' was already resolved by an earlier reply (second reply from worker '{worker_tag}' ignored)"
            ),
            NoticeReplyRejection::WorkerMismatch {
                notice_id,
                worker_tag,
                addressed_to,
            } => write!(
                f,
                "notice '{notice_id}' is pending but addressed to '{addressed_to}', not to the replying worker '{worker_tag}'"
            ),
        }
    }
}

/// Whether `notice` addresses the worker identified by `worker_key`.
///
/// This is the **same identity resolution used for routing**
/// ([`worker_routing_identity`] + [`WorkerRoutingIdentity::routes`]): whole-field
/// equality at segment boundaries, never a substring or unanchored prefix, so a
/// reply from `coder-t-001` cannot claim a notice addressed to `coder-t-002`,
/// and a `coder` reply cannot claim a `validator-coder-t-001` notice.
///
/// The rule is evaluated from both sides because the reply path only knows what
/// the harness hands it: `ToolCaller::role_name()` is a bare role (`coder`)
/// while a notice is often addressed to a full registry key
/// (`coder-t-001`/`coder-t-001#7`). Evaluating the pair in both directions
/// keeps those two spellings of the *same* worker interchangeable; two
/// genuinely different workers fail in both directions.
pub fn notice_addresses_worker(notice: &SteerNotice, worker_key: &str) -> bool {
    let replying = worker_routing_identity(worker_key);
    if replying.routes(&notice.target_worker) {
        return true;
    }
    worker_routing_identity(&notice.target_worker).routes(worker_key)
}

/// Resolve a queued-but-undelivered copy of a notice that an explicit reply has
/// just resolved, so the worker is never asked again to answer a notice it has
/// already answered. Bounded by [`MAX_INBOX_KEYS`] × [`INBOX_CAPACITY`].
fn drop_queued_notice_copies(notice_id: &str) {
    let keys: Vec<String> = WORKER_INBOXES
        .iter()
        .filter(|e| {
            e.value()
                .notices
                .iter()
                .any(|notice| notice.notice_id == notice_id)
        })
        .map(|e| e.key().clone())
        .collect();

    for key in keys {
        if let Some(mut entry) = WORKER_INBOXES.get_mut(&key) {
            let before = entry.notices.len();
            entry.notices.retain(|notice| notice.notice_id != notice_id);
            if before != entry.notices.len() {
                entry.last_touched_ms = notice_now_ms();
            }
        }
    }
}

/// Record a specialist worker's reply against the notice it explicitly names.
///
/// The strict reply contract — what a worker must send:
///
/// 1. `notice_id`: the **verbatim** id of a notice that was rendered into this
///    worker's own context (see [`render_notice_for_worker`], which surfaces it
///    as `ID: <notice_id>` and instructs the worker to echo it back).
/// 2. `worker_tag`: the identity of the replying worker — its effective
///    registry key (`coder-t-001`, or the disambiguated `coder-t-001#7`) or the
///    bare role name the harness reports (`coder`). The notice must be addressed
///    to that worker, judged by the routing rules ([`notice_addresses_worker`]).
/// 3. `message`: the reply text.
///
/// Any deviation is rejected with a typed [`NoticeReplyRejection`] plus a
/// `tracing::warn!` naming the id and the worker. Nothing is recorded, no
/// pending notice is consumed, and no notice is ever picked by "newest/first
/// pending" or by fuzzy text matching.
pub fn record_worker_reply_for_notice(
    worker_tag: &str,
    notice_id: &str,
    message: &str,
) -> Result<SteerNotice, NoticeReplyRejection> {
    let worker_tag = worker_tag.trim();
    let notice_id = notice_id.trim();

    let reject = |rejection: NoticeReplyRejection| {
        // One warning per rejected reply: unlike the storage sweeps this is
        // bounded by real tool calls, and each rejection is a correlation
        // failure an operator has to be able to see.
        tracing::warn!(
            "specialist reply from worker '{}' rejected ({}): {rejection}",
            worker_tag,
            rejection.kind()
        );
        Err(rejection)
    };

    if notice_id.is_empty() {
        return reject(NoticeReplyRejection::MissingNoticeId);
    }

    let pending = PENDING_NOTICES.get(notice_id).map(|entry| entry.clone());
    let Some(notice) = pending else {
        // Distinguish "already answered" from "never pending" so a duplicated
        // reply cannot be mistaken for a fresh one.
        let rejection = if REPLIED_NOTICES.contains_key(notice_id) {
            NoticeReplyRejection::AlreadyReplied {
                notice_id: notice_id.to_string(),
                worker_tag: worker_tag.to_string(),
            }
        } else {
            NoticeReplyRejection::UnknownNoticeId {
                notice_id: notice_id.to_string(),
                worker_tag: worker_tag.to_string(),
            }
        };
        return reject(rejection);
    };

    if !notice_addresses_worker(&notice, worker_tag) {
        return reject(NoticeReplyRejection::WorkerMismatch {
            notice_id: notice_id.to_string(),
            worker_tag: worker_tag.to_string(),
            addressed_to: notice.target_worker.clone(),
        });
    }

    // Consume exactly the notice that was named. `remove_if` keeps this atomic
    // against a concurrent sweep that may have expired the same entry between
    // the lookup above and here.
    let resolved = PENDING_NOTICES
        .remove_if(notice_id, |_, entry| entry.notice_id == notice_id)
        .map(|(_, notice)| notice);
    let Some(resolved) = resolved else {
        return reject(NoticeReplyRejection::UnknownNoticeId {
            notice_id: notice_id.to_string(),
            worker_tag: worker_tag.to_string(),
        });
    };

    drop_queued_notice_copies(&resolved.notice_id);

    let now_ms = notice_now_ms();
    let seq = REPLIED_SEQ.fetch_add(1, Ordering::SeqCst);
    REPLIED_NOTICES.insert(
        resolved.notice_id.clone(),
        StoredReply {
            reply: SteerNoticeReply {
                notice_id: resolved.notice_id.clone(),
                worker_tag: worker_tag.to_string(),
                reply_message: message.to_string(),
                replied_at_ms: now_ms,
            },
            seq,
        },
    );

    // Bounded reply ring: the oldest recorded replies age out first, so
    // `get_worker_reply` stays correct for everything currently in flight while
    // a long session cannot accumulate replies without limit.
    if REPLIED_NOTICES.len() > REPLIED_NOTICES_CAP {
        let mut oldest: Vec<(String, u64)> = REPLIED_NOTICES
            .iter()
            .map(|e| (e.key().clone(), e.value().seq))
            .collect();
        oldest.sort_by_key(|(id, reply_seq)| (*reply_seq, id.clone()));
        let over = oldest.len() - REPLIED_NOTICES_CAP;
        for (id, _) in oldest.into_iter().take(over) {
            REPLIED_NOTICES.remove(&id);
        }
        warn_lifecycle(
            &REPLY_WARN_SEQ,
            format!(
                "reply store exceeded its bound of {REPLIED_NOTICES_CAP}: dropped the {over} oldest recorded specialist reply/replies"
            ),
        );
    }

    Ok(resolved)
}

/// Source-compatible wrapper around [`record_worker_reply_for_notice`].
///
/// Same signature (and the same `Result<_, String>` error channel) the existing
/// callers already use, so no call site has to change to become strict. New
/// callers should adopt [`record_worker_reply_for_notice`] and branch on
/// [`NoticeReplyRejection::kind()`] instead of on prose. Callers that should
/// adopt the strict form: `src/harness/mod.rs:516` (`record_worker_reply(caller,
/// notice_id, message)` — and the `Err` branch right below it, which currently
/// re-reads the pending notice instead of surfacing the rejection).
pub fn record_worker_reply(
    worker_tag: &str,
    notice_id: &str,
    message: &str,
) -> Result<SteerNotice, String> {
    record_worker_reply_for_notice(worker_tag, notice_id, message)
        .map_err(|rejection| rejection.to_string())
}

/// Get a pending notice by ID.
///
/// Exact id only — there is no "most recent pending notice" variant, and there
/// must never be one (recon H6).
///
/// A lookup does not run the sweep (expiry is applied on the next post/drain),
/// so a notice that has aged out can still be read here until the next
/// post/drain removes it.
pub fn get_pending_notice(notice_id: &str) -> Option<SteerNotice> {
    PENDING_NOTICES.get(notice_id).map(|e| e.clone())
}

/// [`get_pending_notice`] scoped to one worker: the notice is returned only when
/// the id matches exactly **and** the notice addresses `worker_key` under the
/// routing rules. This is the seam for callers that used to read an unscoped
/// pending notice after a reply failed.
pub fn get_pending_notice_for_worker(notice_id: &str, worker_key: &str) -> Option<SteerNotice> {
    PENDING_NOTICES
        .get(notice_id)
        .map(|entry| entry.clone())
        .filter(|notice| notice_addresses_worker(notice, worker_key))
}

/// Canonical rendering of a notice into the worker's context.
///
/// The strict reply contract is only satisfiable if the worker is told the id,
/// so this is the text that makes it so: the id appears verbatim and the
/// required tool call is spelled out. The two inline copies of this rendering
/// (`src/agents/runner/execution.rs` and `src/agents/runner/fix_loop.rs`) should
/// call this instead of re-formatting the notice.
pub fn render_notice_for_worker(notice: &SteerNotice) -> String {
    format!(
        "[Steering Notice from Arbitrator — ID: {notice_id}]:\n\"{inquiry}\"\n\n\
         To reply to the Arbitrator regarding this notice, invoke the `{reply_tool}` tool with `notice_id: \"{notice_id}\"` and your `message`.\n\
         Reply to this notice id only — a reply naming another id, or no id, is rejected.",
        notice_id = notice.notice_id,
        inquiry = notice.user_inquiry,
        reply_tool = crate::tool_names::TOOL_REPLY_TO_ARBITRATOR,
    )
}

/// Get a recorded worker reply by notice ID.
pub fn get_worker_reply(notice_id: &str) -> Option<SteerNoticeReply> {
    REPLIED_NOTICES.get(notice_id).map(|e| e.reply.clone())
}

/// Run the TTL/expiry + inbox-reclamation sweep explicitly and report what it
/// dropped. Posts and drains call it implicitly; this is the explicit seam for
/// callers that want to force it (and for tests).
pub fn prune_expired_notices() -> NoticePruneReport {
    prune_notice_storage()
}

/// Clear all notices, inboxes, and replies (primarily for test isolation).
///
/// Also resets the lifecycle counters and the injected clock offset, so an
/// expiry test can never leak a shifted clock into the next test.
pub fn clear_all_notices() {
    PENDING_NOTICES.clear();
    WORKER_INBOXES.clear();
    REPLIED_NOTICES.clear();
    CLOCK_OFFSET_MS.store(0, Ordering::SeqCst);
    EXPIRED_NOTICE_TOTAL.store(0, Ordering::Relaxed);
    CAPACITY_EVICTED_TOTAL.store(0, Ordering::Relaxed);
    RECLAIMED_INBOX_TOTAL.store(0, Ordering::Relaxed);
    EXPIRY_WARN_SEQ.store(0, Ordering::Relaxed);
    OVERFLOW_WARN_SEQ.store(0, Ordering::Relaxed);
    EVICTION_WARN_SEQ.store(0, Ordering::Relaxed);
    REPLY_WARN_SEQ.store(0, Ordering::Relaxed);
}

/// Evaluate a specialist worker's reply through the Steer Arbitrator model:
/// either synthesize a factual direct answer for the user, or formulate an internal
/// follow-up question for the worker before presenting anything to the user.
pub async fn evaluate_worker_reply<F>(
    client: &ChatClient,
    stats: &HarnessStats,
    notice: &SteerNotice,
    worker_tag: &str,
    worker_reply: &str,
    mut on_delta: F,
) -> Result<WorkerReplyEvaluation, anyhow::Error>
where
    F: FnMut(&str) + Send,
{
    let system_prompt = "\
You are Marmel's Steer Arbitrator mediating between the user and active specialist workers.
The user previously sent a steering inquiry or instruction to a specialist worker.
The specialist worker has now replied back to YOU (the Arbitrator).

Your role:
Evaluate the worker's reply:
1. If the worker's reply directly and satisfactorily addresses the user's inquiry, choose 'SynthesizeResponse'.
   - In 'response', formulate a direct, factual, and concise answer to the user in the EXACT SAME LANGUAGE as the user's inquiry.
   - Be clear, polite, and factual. Zero filler, no conversational meta-disclaimers.
2. If the worker's reply is incomplete, ambiguous, contradictory, or raises a concern that requires clarification from the worker before answering the user, choose 'AskFollowUp'.
   - In 'follow_up_prompt', specify the clear, targeted question for the worker in English.
   - In 'user_status', provide a brief status notice in the user's language (e.g. 'Ställer en följdfråga till Coder för att reda ut detaljerna...').

You MUST output ONLY a valid JSON object matching:
{
  \"decision\": \"SynthesizeResponse\" | \"AskFollowUp\",
  \"response\": \"Direct answer to the user in their language (required if decision is SynthesizeResponse)\",
  \"follow_up_prompt\": \"Follow-up question for the worker in English (required if decision is AskFollowUp)\",
  \"user_status\": \"Brief status notice for the user (optional for AskFollowUp)\"
}";

    let user_prompt = format!(
        "Original User Inquiry:\n\"{}\"\n\nSpecialist Worker [{}] Reply to Arbitrator:\n\"{}\"\n\nPlease output your evaluation JSON.",
        notice.user_inquiry, worker_tag, worker_reply
    );

    let req = ChatRequest {
        model: String::new(),
        messages: vec![
            Message::System {
                content: system_prompt.to_string(),
            },
            Message::User {
                content: user_prompt,
            },
        ],
        temperature: Some(0.0),
        top_p: Some(0.9),
        frequency_penalty: None,
        presence_penalty: None,
        stream: Some(true),
        enable_thinking: Some(false),
        tools: None,
    };

    let mut full_accum = String::new();
    let reply_res = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        client.chat_stream(&req, |chunk| {
            full_accum.push_str(chunk);
            true
        }),
    )
    .await;

    let raw = match reply_res {
        Ok(Ok(r)) => {
            if !full_accum.trim().is_empty() {
                full_accum
            } else {
                r.content
            }
        }
        _ => String::new(),
    };

    let raw_trimmed = raw.trim();
    let json_text = if let Some(stripped) = raw_trimmed.strip_prefix("```json") {
        stripped.trim_end_matches("```").trim()
    } else if let Some(stripped) = raw_trimmed.strip_prefix("```") {
        stripped.trim_end_matches("```").trim()
    } else if let Some(start) = raw_trimmed.find('{') {
        if let Some(end) = raw_trimmed.rfind('}') {
            &raw_trimmed[start..=end]
        } else {
            raw_trimmed
        }
    } else {
        raw_trimmed
    };

    let parsed: Option<WorkerReplyEvaluation> = serde_json::from_str(json_text).ok();
    if let Some(eval) = parsed {
        stats.record_steer_arbitration();
        if eval.decision.eq_ignore_ascii_case("SynthesizeResponse")
            && let Some(ref resp) = eval.response
        {
            on_delta(resp);
        }
        return Ok(eval);
    }

    let fallback_resp = if !raw_trimmed.is_empty() && !raw_trimmed.starts_with('{') {
        raw_trimmed.to_string()
    } else {
        format!("Specialist [{worker_tag}] explains: {worker_reply}")
    };
    on_delta(&fallback_resp);
    stats.record_steer_arbitration();

    Ok(WorkerReplyEvaluation {
        decision: "SynthesizeResponse".to_string(),
        response: Some(fallback_resp),
        follow_up_prompt: None,
        user_status: None,
    })
}
