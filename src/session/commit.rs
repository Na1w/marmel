//! The commit side of the session bundle: exactly **one** writer routine in the
//! crate, [`write_bundle`], writing a bundle member set in the normative order
//!
//! ```text
//! C3(a) workers/<key>.json   ascending rel_path
//! C3(b) ui.json
//! C3(c) manager.json
//! C4    manifest.json        <- LAST, nothing after it. This rename IS the commit.
//! ```
//!
//! Nothing else may create a file under [`Plan::session_bundle_path`]; deciding
//! *when* to call [`write_bundle`] lives elsewhere, and [`write_bundle`] is the
//! primitive every caller uses. `harness::fs::replace_path_lock` is private, so
//! `session/` owns [`CommitLock`] — in-process serialization only.
//!
//! Every member write goes through [`crate::harness::fs::write_file_atomic`]:
//! unique `O_EXCL` staging name, `write_all` + `flush`, same-directory `rename`,
//! unlink of the staged file on **every** failure path, and **no** `fsync`. So
//! `rename` buys *atomicity* against **process death**, not *durability* against
//! power loss: a newer `manifest.json` over a lost `manager.json` is a
//! `byte_len`/`fingerprint` mismatch ⇒ `Refused`, never a half-restored session.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::harness::fs::write_file_atomic;
use crate::manager::phase::Plan;
use crate::session::format::envelope::{
    IdentityFloors, ManagerFile, ManagerSection, Manifest, ManifestMember, ProcessEnding,
    SectionHeader, SectionKind,
};
use crate::session::format::restore::{Refusal, RefusalCode};
use crate::session::format::ui::{UiFile, UiSection};
use crate::session::format::worker::{WorkerFile, WorkerSection};
use crate::session::format::{
    BUNDLE_DIR_NAME, FORMAT_VERSION, MANAGER_FILE, MANIFEST_FILE, SCHEMA_VERSION, TimePoint,
    UI_FILE, WORKERS_DIR_NAME,
};

/// Re-export so `session::commit::fingerprint` resolves to the one and only
/// fingerprint in the crate: it is **not** `harness::fs::hash_path`, which is a
/// private shard selector over a `Path`.
pub use crate::session::format::fingerprint;

/// Bundle root for a plan — the single path authority.
pub fn bundle_dir(plan: &Plan) -> PathBuf {
    plan.session_bundle_path()
}

/// `manifest.json` — the commit point.
pub fn manifest_path(bundle: &Path) -> PathBuf {
    bundle.join(MANIFEST_FILE)
}

/// `manager.json`.
pub fn manager_path(bundle: &Path) -> PathBuf {
    bundle.join(MANAGER_FILE)
}

/// `ui.json`.
pub fn ui_path(bundle: &Path) -> PathBuf {
    bundle.join(UI_FILE)
}

/// `workers/<escaped-key>.json`.
pub fn worker_path(bundle: &Path, worker_key: &str) -> PathBuf {
    bundle
        .join(WORKERS_DIR_NAME)
        .join(format!("{}.json", percent_escape_key(worker_key)))
}

/// Manifest `rel_path` for a worker member.
pub fn worker_rel_path(worker_key: &str) -> String {
    format!(
        "{}/{}.json",
        WORKERS_DIR_NAME,
        percent_escape_key(worker_key)
    )
}

/// Key escaping: `%` first, or the two later substitutions would be
/// double-escaped (`#` → `%23` would become `%2523`).
pub fn percent_escape_key(worker_key: &str) -> String {
    worker_key
        .replace('%', "%25")
        .replace('#', "%23")
        .replace('/', "%2F")
}

/// Inverse of [`percent_escape_key`]. A name that is not validly escaped is a
/// foreign file, never a member: the loader refuses to *guess* which worker it
/// was supposed to be.
pub fn percent_unescape(escaped: &str) -> Result<String, Refusal> {
    let bytes = escaped.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let (hi, lo) = match (bytes.get(i + 1), bytes.get(i + 2)) {
                    (Some(hi), Some(lo)) => (*hi, *lo),
                    _ => {
                        return Err(Refusal::new(
                            RefusalCode::Json,
                            format!("truncated %-escape in member name {escaped:?}"),
                        ));
                    }
                };
                let decoded = std::str::from_utf8(&[hi, lo])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match decoded {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        let raw = String::from_utf8_lossy(&bytes[i..i + 3]);
                        return Err(Refusal::new(
                            RefusalCode::Json,
                            format!("invalid %-escape {raw:?} in member name {escaped:?}"),
                        ));
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| {
        Refusal::new(
            RefusalCode::Json,
            format!("member name {escaped:?} does not decode to UTF-8"),
        )
    })
}

/// `.{stem}.tmp.{pid}.{nanos}.{seq:08}` — the staging name `harness::fs` derives.
/// Consulted only to *recognise* leftovers, never to create one.
pub fn staging_name(stem: &str, pid: u32, nanos: u32, seq: u32) -> String {
    format!(".{stem}.tmp.{pid}.{nanos}.{seq:08}")
}

/// Is `name` a stale staging leftover from a **different** process?
///
/// Leftovers are noise, not state — never treated as members, never reported as
/// orphans. A leftover carrying *this* pid belongs to a write in flight and must
/// not be touched.
pub fn is_stale_staging_name(name: &str, this_pid: u32) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    let Some((stem, tail)) = rest.split_once(".tmp.") else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    let mut parts = tail.split('.');
    let (Some(pid), Some(nanos), Some(seq)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    seq.len() == 8
        && seq.parse::<u32>().is_ok()
        && nanos.parse::<u32>().is_ok()
        && pid.parse::<u32>().is_ok_and(|pid| pid != this_pid)
}

/// The three section families' frame values. `member_count`/`member_index` are
/// literal: each member file *is* a one-member section (`1` for Manager/Ui, `1`
/// per worker file), so the pair is `1`/`0` for every member the committer
/// writes. The loader still cross-checks them — a member from an older or a
/// hand-edited bundle disagrees, and that is a `HeaderManifestDisagreement`.
fn header(session_id: &str, kind: SectionKind, now: &TimePoint) -> SectionHeader {
    SectionHeader {
        format_version: FORMAT_VERSION,
        schema_version: SCHEMA_VERSION,
        session_id: session_id.to_string(),
        kind,
        saved_wall: now.wall,
        saved_elapsed_secs: now.elapsed_secs,
        member_count: 1,
        member_index: 0,
    }
}

/// A member's payload serialized exactly **once**, plus the two integrity
/// fields computed from that same owned buffer. The manifest mirrors these
/// values; the loader compares them against the file.
#[derive(Debug)]
struct MemberBytes {
    rel_path: String,
    kind: SectionKind,
    bytes: Vec<u8>,
    byte_len: u64,
    fingerprint: u64,
    saved: TimePoint,
}

impl MemberBytes {
    fn new(
        rel_path: String,
        kind: SectionKind,
        saved: &TimePoint,
        value: &impl serde::Serialize,
    ) -> Result<Self, Refusal> {
        let frame = SectionBytes::serialize(value)?;
        Ok(MemberBytes {
            rel_path,
            kind,
            byte_len: frame.len() as u64,
            fingerprint: fingerprint(&frame),
            bytes: frame,
            saved: saved.clone(),
        })
    }
}

/// Serialization wrapper so the `serde_json` failure has one owner.
struct SectionBytes;

impl SectionBytes {
    /// Pretty JSON, the same shape the whole bundle is written in, so a reader
    /// can reproduce a fingerprint by hand against the file.
    fn serialize(value: &impl serde::Serialize) -> Result<Vec<u8>, Refusal> {
        serde_json::to_vec_pretty(value).map_err(|err| {
            Refusal::new(
                RefusalCode::Json,
                format!("serializing bundle member failed: {err}"),
            )
        })
    }
}

/// What a trigger hands the committer. `ui` is `None` on the panic path, which
/// cannot reach the renderer; the previous `ui.json` then simply stays
/// unlisted and unrewritten.
#[derive(Debug, Clone)]
pub struct CapturePayload {
    pub manager: ManagerSection,
    pub ui: Option<UiSection>,
    pub workers: Vec<WorkerSection>,
    pub identity_floors: IdentityFloors,
}

/// Throttling/size policy mirrored from `Config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPolicy {
    /// Minimum wall time between commits (`session_commit_min_interval_ms`).
    /// Enforced by the trigger layer, never here: a committer that got
    /// this far has already been allowed.
    pub min_interval: Duration,
    /// Soft ceiling on total member bytes (`session_max_bundle_bytes`). Crossing
    /// it raises one loud warning ([`size_ceiling_hit`]); *stopping* is the
    /// trigger layer's decision.
    pub max_bundle_bytes: u64,
    /// Whether a `ToolResult` trigger may commit at all
    /// (`session_commit_on_tool_result`).
    pub allow_throttled: bool,
}

impl Default for CommitPolicy {
    fn default() -> Self {
        CommitPolicy {
            min_interval: Duration::from_millis(250),
            max_bundle_bytes: 32 * 1024 * 1024,
            allow_throttled: true,
        }
    }
}

/// What a commit did. Commit results are never discarded: every caller consumes
/// the `CommitReport`/`CommitError` and records it in the capture ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReport {
    /// Members actually renamed this commit (changed members only).
    pub members_written: Vec<String>,
    /// Members whose bytes were byte-identical to the last committed ones.
    pub members_skipped_identical: usize,
    pub manifest_bytes: u64,
    /// Worker files the new manifest does not list, plus stale
    /// staging leftovers from other processes.
    pub orphans_removed: usize,
    pub duration: Duration,
}

/// Why a commit failed.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    /// Directory creation, `read_dir` or deletion failed.
    #[error("session commit: I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A member could not be serialized.
    #[error("session commit: serialization failed: {0}")]
    Serialize(String),
    /// Another commit holds the session commit lock: the caller sets
    /// `throttled_pending` rather than blocking.
    #[error("session commit: another commit is already in flight")]
    Locked,
    /// A member write failed. `write_file_atomic` reports `harness::ToolError`,
    /// which is not an `io::Error`, so the text is carried verbatim (the staged
    /// file has already been unlinked by that helper on this path).
    #[error("session commit: write of member {path} failed: {detail}")]
    Write { path: PathBuf, detail: String },
}

/// Byte-identity diff cache plus the last committed manifest.
///
/// A global by design: there is one bundle per process, and the panic path reads
/// it to rewrite only the manifest. [`LastCommit::bundle`] guards against a
/// second bundle (a test, or a plan switch) silently poisoning the cache.
#[derive(Debug)]
pub struct LastCommit {
    pub bundle: PathBuf,
    pub session_id: String,
    pub manifest: Manifest,
    /// `rel_path` → (`byte_len`, `fingerprint`, exact bytes).
    pub members: std::collections::BTreeMap<String, (u64, u64, Vec<u8>)>,
}

/// The commit cache. Poisoned-lock handling: the state is a plain cache, and the
/// sanctioned `into_inner` recovery is safe here — a stale cache only causes an
/// extra rewrite, never a wrong bundle.
pub static LAST_COMMIT: LazyLock<Mutex<Option<LastCommit>>> = LazyLock::new(|| Mutex::new(None));

fn last_commit_lock() -> MutexGuard<'static, Option<LastCommit>> {
    LAST_COMMIT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Once-only latch for the size-ceiling warning, so a 40 MB bundle does not add
/// a `tracing::warn!` (or a ledger warning) per commit.
static SIZE_CEILING_WARNED: AtomicBool = AtomicBool::new(false);

/// Did the committed byte total cross the configured ceiling?
///
/// Returns `true` **once** per process. The committer never truncates or refuses
/// on size (that would silently drop history); the trigger layer decides whether
/// to keep committing.
pub fn size_ceiling_hit(total_bytes: u64, policy: &CommitPolicy) -> bool {
    if total_bytes <= policy.max_bundle_bytes {
        return false;
    }
    if SIZE_CEILING_WARNED.swap(true, Ordering::SeqCst) {
        false
    } else {
        tracing::warn!(
            total_bytes,
            max_bundle_bytes = policy.max_bundle_bytes,
            "session bundle exceeded session_max_bundle_bytes; committing anyway \
             (truncating history is worse than a large bundle)"
        );
        true
    }
}

/// Session-owned commit mutex: `harness::fs::replace_path_lock` is private, so
/// `session/` owns its own.
///
/// `Send + Sync`. It serializes C1–C5 in-process; cross-process atomicity comes
/// from manifest-last, not from this mutex.
#[derive(Debug, Default)]
pub struct CommitLock {
    state: Mutex<GuardState>,
}

#[derive(Debug, Default)]
struct GuardState {
    in_flight: bool,
    /// A commit was requested while one was running. The guard re-checks it on
    /// drop so the next trigger commits immediately instead of waiting out the
    /// throttle window.
    dirty: bool,
}

impl CommitLock {
    pub fn new() -> Self {
        CommitLock::default()
    }

    /// `None` when a commit is already in flight: the caller sets
    /// `throttled_pending` instead of blocking.
    pub fn try_guard(&self) -> Option<CommitGuard<'_>> {
        let mut state = self.lock_state();
        if state.in_flight {
            state.dirty = true;
            return None;
        }
        state.in_flight = true;
        Some(CommitGuard { lock: self })
    }

    /// Marks a commit as requested. Returns `true` when one was already pending.
    pub fn mark_dirty(&self) -> bool {
        let mut state = self.lock_state();
        std::mem::replace(&mut state.dirty, true)
    }

    /// Consumes the pending-commit flag.
    pub fn take_dirty(&self) -> bool {
        let mut state = self.lock_state();
        std::mem::take(&mut state.dirty)
    }

    pub fn in_flight(&self) -> bool {
        self.lock_state().in_flight
    }

    fn lock_state(&self) -> MutexGuard<'_, GuardState> {
        // Sanctioned pattern: the state is two `bool`s and every mutation is a
        // plain assignment, so a panicked (poisoned) holder left it consistent.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// RAII holder for the commit critical section. `Drop` **never panics**.
#[derive(Debug)]
pub struct CommitGuard<'a> {
    lock: &'a CommitLock,
}

impl Drop for CommitGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.lock.lock_state();
        state.in_flight = false;
        if state.dirty {
            tracing::debug!("session commit lock released with a pending (dirty) commit request");
        }
    }
}

/// THE commit entry point (steps C1–C5): writes the payload set for `session_id`
/// into `bundle` as workers (ascending `rel_path`) → `ui.json` → `manager.json`,
/// then `manifest.json` **last** — that rename *is* the commit. Members
/// byte-identical to the last committed bytes are not rewritten; every write goes
/// through `harness::fs::write_file_atomic` (never `std::fs::write`), with
/// `fingerprint`/`byte_len` from the **same owned buffer**. A non-finite
/// `elapsed_secs` is refused **before C1** ([`check_time_points`]); `ending` is
/// written verbatim, and `HardKillInferred` never is ([`assert_writable_ending`]).
pub fn write_bundle(
    bundle: &Path,
    session_id: &str,
    payload: &CapturePayload,
    ending: ProcessEnding,
    policy: &CommitPolicy,
    lock: &CommitLock,
) -> Result<CommitReport, CommitError> {
    let started = Instant::now();
    let Some(_guard) = lock.try_guard() else {
        return Err(CommitError::Locked);
    };
    let ending = writable_ending(&ending);

    // C1: payloads, serialized once each into owned buffers.
    let members = assemble_members(session_id, payload)?;
    let total_bytes: u64 = members.iter().map(|m| m.byte_len).sum();
    size_ceiling_hit(total_bytes, policy);

    // C2: diff against the last committed bytes.
    let cached = previous_members(bundle, session_id);

    // C3: write the changed members in the normative order. `assemble_members`
    // already returns workers (ascending rel_path) -> ui -> manager.
    let mut members_written = Vec::new();
    let mut members_skipped_identical = 0usize;
    let mut committed = std::collections::BTreeMap::new();
    for member in &members {
        let path = bundle.join(&member.rel_path);
        let identical =
            cached
                .get(&member.rel_path)
                .is_some_and(|(byte_len, fingerprint, bytes)| {
                    *byte_len == member.byte_len
                        && *fingerprint == member.fingerprint
                        && *bytes == member.bytes
                });
        if identical {
            members_skipped_identical += 1;
        } else {
            ensure_parent_dir(&path)?;
            write_file_atomic(&path, &member.bytes).map_err(|err| CommitError::Write {
                path: path.clone(),
                detail: err.to_string(),
            })?;
            members_written.push(member.rel_path.clone());
        }
        committed.insert(
            member.rel_path.clone(),
            (member.byte_len, member.fingerprint, member.bytes.clone()),
        );
    }

    // C4: the manifest, LAST. Nothing is written after it.
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        schema_version: SCHEMA_VERSION,
        session_id: session_id.to_string(),
        committed_wall: chrono::Local::now(),
        anchor_wall: crate::session::time::anchor_wall(),
        ending,
        identity_floors: payload.identity_floors,
        members: members
            .iter()
            .map(|member| ManifestMember {
                kind: member.kind,
                rel_path: member.rel_path.clone(),
                byte_len: member.byte_len,
                fingerprint: member.fingerprint,
                saved_wall: member.saved.wall,
                saved_elapsed_secs: member.saved.elapsed_secs,
            })
            .collect(),
    };
    let manifest_bytes =
        SectionBytes::serialize(&manifest).map_err(|err| CommitError::Serialize(err.message))?;
    let manifest_file = manifest_path(bundle);
    ensure_parent_dir(&manifest_file)?;
    write_file_atomic(&manifest_file, &manifest_bytes).map_err(|err| CommitError::Write {
        path: manifest_file.clone(),
        detail: err.to_string(),
    })?;

    // C5: post-commit cleanup, best effort, never load-bearing.
    let orphans_removed = sweep_orphans(bundle, &manifest);

    // Publish the diff cache only after the commit point, so a failed commit
    // cannot make a later round skip a write that never happened.
    {
        let mut slot = last_commit_lock();
        *slot = Some(LastCommit {
            bundle: bundle.to_path_buf(),
            session_id: session_id.to_string(),
            manifest,
            members: committed,
        });
    }

    Ok(CommitReport {
        members_written,
        members_skipped_identical,
        manifest_bytes: manifest_bytes.len() as u64,
        orphans_removed,
        duration: started.elapsed(),
    })
}

/// `HardKillInferred` is a loader-side sentinel: writing it would let a process
/// claim a death it never observed — the exact lie the sentinel exists to
/// prevent. A caller that has nothing better to say must pass `InProgress`, which
/// is what this guard substitutes (loudly) rather than writing the sentinel or
/// panicking inside a commit.
pub fn writable_ending(ending: &ProcessEnding) -> ProcessEnding {
    if matches!(ending, ProcessEnding::HardKillInferred) {
        tracing::error!(
            "session commit: ProcessEnding::HardKillInferred is loader-only; committing \
             InProgress instead so the bundle never claims an unobserved death"
        );
        return ProcessEnding::InProgress;
    }
    ending.clone()
}

/// Write-side clock sanity: a `TimePoint` whose `elapsed_secs` is NaN or
/// ±infinity is refused **before C1 serializes anything**. `serde_json` does not
/// error on a non-finite float (`serialize_f64` writes `null`), so without this
/// check a corrupt clock would be committed as `"elapsed_secs": null` and only
/// surface at load — a silent default. The refusal names member, field and value.
/// A negative offset is **not** refused here (the load side reports it as a
/// structural violation ⇒ `Refused`), only warned about, since `session::time`
/// floors every captured offset at `0.0`. Nothing substitutes `TimePoint::now()`.
fn check_time_points(payload: &CapturePayload) -> Result<(), CommitError> {
    // One located sentence per suspect clock point: a non-finite one refuses the
    // commit, a merely-negative one only warns (see the rule above).
    let mut offenders: Vec<String> = Vec::new();
    let mut negatives: Vec<String> = Vec::new();
    {
        let mut check = |site: String, field: &str, point: &TimePoint| {
            let value = point.elapsed_secs;
            let named = format!("{site} {field} elapsed_secs = {value}");
            if !value.is_finite() {
                offenders.push(named);
            } else if value < 0.0 {
                negatives.push(named);
            }
        };
        // Worker clocks — the same triple the loader walks on the load side.
        for worker in &payload.workers {
            let member = worker_rel_path(&worker.worker_key);
            check(member.clone(), "started", &worker.started);
            for (field, point) in [
                ("completed", worker.completed.as_ref()),
                ("last_activity", worker.last_activity.as_ref()),
            ] {
                if let Some(point) = point {
                    check(member.clone(), field, point);
                }
            }
        }
        // UI clocks — the panel pair plus the plan/abort witnesses, i.e. exactly
        // the set the UI loader guards on the way back in.
        if let Some(ui) = &payload.ui {
            for panel in &ui.subagent_panels {
                let site = format!("{UI_FILE} subagent {}", panel.name);
                for (field, point) in [
                    ("started_at", &panel.started_at),
                    ("last_activity_at", &panel.last_activity_at),
                ] {
                    if let Some(point) = point {
                        check(site.clone(), field, point);
                    }
                }
            }
            for (field, point) in [
                ("plan_start", &ui.plan_start),
                ("plan_completed", &ui.plan_completed),
                ("abort_armed_at", &ui.abort_armed_at),
            ] {
                if let Some(point) = point {
                    check(UI_FILE.to_string(), field, point);
                }
            }
        }
    }
    for negative in &negatives {
        tracing::warn!(
            "session commit: {negative} is a negative monotonic offset; it is written \
             verbatim (§7.1 C1–C4: the committer does not validate sections) so the loader \
             can refuse it by name (§5.6) — it is never defaulted to now"
        );
    }
    if offenders.is_empty() {
        return Ok(());
    }
    let detail = format!(
        "A §5.6: {} carry a non-finite monotonic offset — NaN/±infinity cannot become a \
         JSON number (`serde_json` writes `null`, a silent default), so the commit is \
         refused before C1: nothing was written, the bundle is untouched, and the value is \
         never defaulted to now",
        offenders.join("; ")
    );
    tracing::warn!("session commit refused before any write: {detail}");
    Err(CommitError::Serialize(detail))
}

/// Builds the member list in write order: workers by ascending `rel_path`, then
/// `ui.json`, then `manager.json`.
fn assemble_members(
    session_id: &str,
    payload: &CapturePayload,
) -> Result<Vec<MemberBytes>, CommitError> {
    // The write-side clock check runs before C1, so a non-finite clock never
    // reaches a bundle byte and the bundle stays exactly as it was.
    check_time_points(payload)?;
    let mut members = Vec::with_capacity(payload.workers.len() + 2);
    // One capture instant for the whole set. Every member header and every
    // manifest member entry mirror the *same* `saved_wall` /
    // `saved_elapsed_secs`, which is what makes the cross-check meaningful.
    let saved = TimePoint::now();

    let mut workers: Vec<&WorkerSection> = payload.workers.iter().collect();
    workers.sort_by_key(|worker| worker_rel_path(&worker.worker_key));
    for worker in workers {
        let frame = WorkerFile {
            header: header(session_id, SectionKind::Worker, &saved),
            section: worker.clone(),
        };
        members.push(
            MemberBytes::new(
                worker_rel_path(&worker.worker_key),
                SectionKind::Worker,
                &saved,
                &frame,
            )
            .map_err(|err| CommitError::Serialize(err.message))?,
        );
    }

    if let Some(ui) = &payload.ui {
        let frame = UiFile {
            header: header(session_id, SectionKind::Ui, &saved),
            section: ui.clone(),
        };
        members.push(
            MemberBytes::new(UI_FILE.to_string(), SectionKind::Ui, &saved, &frame)
                .map_err(|err| CommitError::Serialize(err.message))?,
        );
    }

    let frame = ManagerFile {
        header: header(session_id, SectionKind::Manager, &saved),
        section: payload.manager.clone(),
    };
    members.push(
        MemberBytes::new(
            MANAGER_FILE.to_string(),
            SectionKind::Manager,
            &saved,
            &frame,
        )
        .map_err(|err| CommitError::Serialize(err.message))?,
    );

    Ok(members)
}

/// The cache used for the C2 diff, or empty when it does not describe this bundle.
fn previous_members(
    bundle: &Path,
    session_id: &str,
) -> std::collections::BTreeMap<String, (u64, u64, Vec<u8>)> {
    let slot = last_commit_lock();
    match slot.as_ref() {
        Some(last) if last.bundle == bundle && last.session_id == session_id => {
            last.members.clone()
        }
        _ => std::collections::BTreeMap::new(),
    }
}

/// `create_dir_all` for the member's directory (`session/` or `session/workers/`).
fn ensure_parent_dir(path: &Path) -> Result<(), CommitError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|err| CommitError::Io {
        path: dir.to_path_buf(),
        source: err,
    })
}

/// Delete worker files the new manifest does not list, and stale
/// `.{stem}.tmp.*` leftovers that are not this process's.
///
/// Best effort: an `Err` is logged and counted as "not removed", never returned
/// — the commit already happened, and failing to tidy must not be reported as a
/// failed commit.
fn sweep_orphans(bundle: &Path, manifest: &Manifest) -> usize {
    let listed: BTreeSet<&str> = manifest
        .members
        .iter()
        .map(|m| m.rel_path.as_str())
        .collect();
    let pid = std::process::id();
    let mut removed = 0usize;

    // Workers the manifest does not name. Only `*.json` files: staging names
    // and dot-entries are noise, not members.
    let workers_dir = bundle.join(WORKERS_DIR_NAME);
    match std::fs::read_dir(&workers_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy().into_owned();
                if name.starts_with('.') {
                    if is_stale_staging_name(&name, pid) {
                        remove_orphan(&entry.path(), &mut removed);
                    }
                    continue;
                }
                let rel = format!("{WORKERS_DIR_NAME}/{name}");
                if name.ends_with(".json") && !listed.contains(rel.as_str()) {
                    remove_orphan(&entry.path(), &mut removed);
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(
                "session commit sweep: cannot read {}: {err}",
                workers_dir.display()
            );
        }
    }

    // Stale staging leftovers in the bundle directory itself.
    match std::fs::read_dir(bundle) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy().into_owned();
                if is_stale_staging_name(&name, pid) {
                    remove_orphan(&entry.path(), &mut removed);
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(
                "session commit sweep: cannot read {}: {err}",
                bundle.display()
            );
        }
    }

    removed
}

fn remove_orphan(path: &Path, removed: &mut usize) {
    match std::fs::remove_file(path) {
        Ok(()) => *removed += 1,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(
                "session commit sweep: cannot remove {}: {err}",
                path.display()
            );
        }
    }
}

/// The last committed manifest for `bundle`, if any (read side of the cache;
/// the panic path uses it to rewrite only the manifest).
pub fn last_manifest(bundle: &Path) -> Option<Manifest> {
    let slot = last_commit_lock();
    slot.as_ref()
        .filter(|last| last.bundle == bundle)
        .map(|last| last.manifest.clone())
}

/// Directory name of the bundle, re-exported for callers that must not hardcode
/// it.
pub const BUNDLE_DIRECTORY_NAME: &str = BUNDLE_DIR_NAME;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_round_trips_and_orders_percent_first() {
        // The `#`/`/` substitutions must not be re-escaped by the `%` rule.
        assert_eq!(percent_escape_key("coder-t-018#7"), "coder-t-018%237");
        assert_eq!(percent_escape_key("a%b"), "a%25b");
        assert_eq!(percent_escape_key("a/b#c"), "a%2Fb%23c");
        assert_eq!(percent_escape_key("plain-w1"), "plain-w1");
        for key in [
            "coder-t-018#7",
            "a%b",
            "a/b#c",
            "w-1",
            "%23already",
            " researcher ",
        ] {
            assert_eq!(percent_unescape(&percent_escape_key(key)).unwrap(), key);
        }
    }

    #[test]
    fn unescape_rejects_malformed_escapes() {
        for bad in ["a%2", "a%", "a%zz", "a%2G"] {
            assert!(
                percent_unescape(bad).is_err(),
                "{bad} must not decode into a member name"
            );
        }
    }

    #[test]
    fn escaped_names_are_injective_so_two_workers_cannot_collide() {
        let a = percent_escape_key("a#b");
        let b = percent_escape_key("a%23b");
        assert_ne!(a, b, "colliding bundle paths would silently merge workers");
    }

    #[test]
    fn staging_leftovers_are_recognised_only_for_other_pids() {
        let foreign = staging_name("manager.json", 1, 123, 7);
        assert!(is_stale_staging_name(&foreign, std::process::id()));
        let mine = staging_name("manager.json", std::process::id(), 123, 7);
        assert!(!is_stale_staging_name(&mine, std::process::id()));
        for not in [
            "manager.json",
            ".session",
            ".hidden",
            ".x.tmp.1.2",
            ".x.tmp.1.2.3.4",
            ".x.tmp.a.2.00000007",
            ".x.tmp.1.2.7",
        ] {
            assert!(
                !is_stale_staging_name(not, 999_999),
                "{not} is not a leftover"
            );
        }
    }

    #[test]
    fn hard_kill_inferred_is_never_committed() {
        assert_eq!(
            writable_ending(&ProcessEnding::InProgress),
            ProcessEnding::InProgress
        );
        assert_eq!(
            writable_ending(&ProcessEnding::PanicHook),
            ProcessEnding::PanicHook
        );
        assert_eq!(
            writable_ending(&ProcessEnding::HardKillInferred),
            ProcessEnding::InProgress,
            "the loader-side sentinel must never reach disk"
        );
    }

    #[test]
    fn commit_lock_is_try_lock_shaped() {
        let lock = CommitLock::new();
        let guard = lock.try_guard().expect("first guard");
        assert!(lock.in_flight());
        assert!(
            lock.try_guard().is_none(),
            "a second committer must not block, it must be told to retry"
        );
        drop(guard);
        assert!(!lock.in_flight());
        assert!(lock.try_guard().is_some());
    }

    #[test]
    fn commit_lock_dirty_flag_survives_release() {
        let lock = CommitLock::new();
        let guard = lock.try_guard().expect("guard");
        assert!(!lock.mark_dirty(), "nothing pending yet");
        assert!(lock.mark_dirty(), "still pending");
        drop(guard);
        assert!(!lock.in_flight());
        assert!(
            lock.take_dirty(),
            "the pending request must outlive the guard"
        );
        assert!(!lock.take_dirty());
    }

    #[test]
    fn size_ceiling_warns_once_and_only_above_the_ceiling() {
        let policy = CommitPolicy {
            min_interval: Duration::from_millis(0),
            max_bundle_bytes: 10,
            allow_throttled: true,
        };
        assert!(
            !size_ceiling_hit(10, &policy),
            "at the ceiling is not over it"
        );
        assert!(!size_ceiling_hit(9, &policy));
    }

    #[test]
    fn write_order_is_workers_then_ui_then_manager() {
        let payload = CapturePayload {
            manager: manager_fixture(),
            ui: Some(UiSection::default()),
            workers: vec![
                worker_fixture("researcher-t-002#1"),
                worker_fixture("coder-t-018#7"),
            ],
            identity_floors: IdentityFloors {
                worker_id_seq_floor: 3,
                completed_seq_floor: 2,
                notice_id_floor: 1,
            },
        };
        let members = assemble_members("s-1", &payload).expect("members");
        let rels: Vec<&str> = members.iter().map(|m| m.rel_path.as_str()).collect();
        assert_eq!(
            rels,
            vec![
                "workers/coder-t-018%237.json",
                "workers/researcher-t-002%231.json",
                "ui.json",
                "manager.json",
            ],
            "A §7.1 C3 is normative order"
        );
    }

    fn manager_fixture() -> ManagerSection {
        ManagerSection {
            messages: vec![],
            pinned_prefix_end: 2,
            pin_witness: crate::session::format::envelope::PinWitness {
                system_prompt_len: 0,
                goal_len: 0,
            },
            max_context_tokens: 100,
            rebirth_advisory_emitted: false,
            consecutive_rebirths: 0,
            dangling: crate::session::format::worker::DanglingToolCallState::Balanced,
        }
    }

    fn worker_fixture(key: &str) -> WorkerSection {
        WorkerSection {
            worker_key: key.to_string(),
            worker_id_seq_value: 1,
            role: crate::agents::Agent::Coder,
            task_id: None,
            status: crate::session::format::worker::WorkerStatus::InProgress,
            status_label: "running".to_string(),
            started: TimePoint::now(),
            completed: None,
            last_activity: None,
            worked_secs: 1.0,
            completed_seq: None,
            context_tokens: 0,
            last_tokens: 0,
            counters: crate::session::format::worker::WorkerCounters::default(),
            brief: String::new(),
            snippets: vec![],
            image_urls: vec![],
            audio_urls: vec![],
            recursion_granted: false,
            history: vec![],
            history_source: crate::session::format::worker::HistorySource::NotCaptured,
            dangling: crate::session::format::worker::DanglingToolCallState::Balanced,
            unrecoverable_reason: None,
            non_restorable: vec![],
        }
    }
}
