//! Mission phase state machine, disk plan management, and auto check-off.
//!
//! The on-disk plan decides a mission's phase: [`Plan`] is the single authority
//! for it, the live turn loops consult [`Plan::exists`], [`Plan::pending_tasks`]
//! and [`Plan::is_complete`] (`src/ui/session.rs`, `src/orchestrator/**`), and
//! the executing loop iterates the unchecked items [`Plan`] exposes until every
//! task is marked `[x]` — suppressing conversational filler is that loop's job,
//! not this module's.
//!
//! The plan lives at `.marmel/execution_plan.md` as `- [ ] [t-xxx]` checkboxes
//! (`- [ ] (t-xxx)` and the other spellings are equally valid); its grammar has
//! exactly one owner, [`crate::plan_parse`], and this module owns no regex at
//! all. A task id arrives bound by the caller, decorated onto a marker, or is
//! proven a boundary-safe token by [`crate::plan_parse::line_matches_task_id`],
//! so an id that exists only inside a word or a path (`t-notes` inside
//! `src/chart-notes.md`) can never flip a plan line.
//!
//! Exactly two authorities may flip a box: a **tool call** the loop makes
//! itself, judged by the structured `ToolResult::is_error` flag and written by
//! [`Plan::check_off`] — the only plan mutation, a raw write that derives
//! nothing and is never a success classifier; and a **subagent deliverable**
//! (including a `delegate_task` tool result carrying a plan task id) flipped
//! only through [`Plan::check_plan_on_deliverable`], where `is_error == false`
//! is **not** evidence of completion: only a `MISSION COMPLETE (t-xxx)` marker
//! turns `- [ ] [t-xxx]` into `- [x] [t-xxx]`, while `FAILED`, `REPLAN REQUIRED`
//! or a marker-less deliverable leaves the box unchecked.
//!
//! That marker grammar — marker set, precedence, parser — has one owner,
//! [`crate::markers`], and is FAIL-first: an explicit `FAILED` /
//! `REPLAN REQUIRED` outranks an embedded `MISSION COMPLETE` substring. The
//! check-off verdict is taken solely by [`crate::markers`] —
//! [`MissionMarker::resolve`] for the verdict,
//! [`crate::markers::has_failure_marker`] /
//! [`crate::markers::has_replan_marker`] for presence tests — and
//! [`crate::markers`] binds a `(t-xxx)` id only as a boundary-safe token on the
//! marker's own line. This module declares no success or failure predicate of
//! its own, so `not ok` prose or a word merely containing a marker token cannot
//! decide the outcome.
//! A task id is resolved from an explicit source only: the caller's bound
//! `task_id` (the `delegate_task` argument, normalized by [`crate::task_id`]),
//! the structured [`MissionMarker`] field, or the id decorated onto the marker
//! (`MISSION COMPLETE (t-xxx)`).
//!
//! An unreadable plan is never a finished one: [`Plan::try_pending_tasks`] and
//! [`Plan::try_all_tasks`] separate `Ok(vec![])` ("there is no plan file") from
//! `Err(..)` ("a plan file exists but could not be read"); the `Vec`-shaped
//! [`Plan::pending_tasks`] / [`Plan::all_tasks`] log the failure instead of
//! swallowing it, and [`Plan::is_complete`] returns `false` when the plan cannot
//! be read, so no loop concludes an unreadable plan was finished.
//!
//! `.marmel/forced_phase.txt` is a legacy name nothing writes:
//! [`FORCED_PHASE_FILE`] survives so that [`Plan::clear`] can delete a stale
//! copy left on disk.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Directory (relative to the workspace) holding the plan and phase override.
pub const MARMEL_DIR: &str = ".marmel";
/// Plan file name inside the marmel directory.
pub const PLAN_FILE: &str = "execution_plan.md";
/// Subdirectory of the marmel directory holding the completed-plan snapshots.
///
/// This is the authority for the plan module's own writers (`archive`,
/// `check_off`'s auto-snapshot, `create`'s pre-overwrite preservation); the
/// name matches [`crate::harness::workspace::ARCHIVE_DIR`].
pub const ARCHIVE_SUBDIR: &str = "archive";
/// Prefix of a timestamped completed-plan snapshot inside [`ARCHIVE_SUBDIR`].
pub const SNAPSHOT_PREFIX: &str = "execution_plan_";
/// Name of the **latest** archived-plan convenience copy inside the marmel
/// directory.
///
/// `archive` writes it, the auto-snapshot refreshes it and [`Plan::clear`]
/// deletes it — which is exactly what lets `ui::session` tell an archive from a
/// reset (`plan_archived_during_round` looks for the plan file being gone **and**
/// this file present).
pub const LATEST_ARCHIVE_FILE: &str = "execution_plan_archive.md";
/// Name of the legacy phase-override file inside the marmel directory.
///
/// Nothing writes or reads it. The name survives so a **stale** override left on
/// disk by an older release is still deletable by [`Plan::clear`] (see [`Plan`]),
/// and `harness::Workspace` re-exports it as the canonical path.
pub const FORCED_PHASE_FILE: &str = "forced_phase.txt";
/// Name of the **superseded** manager journal — a re-export of the session-owned
/// [`crate::session::manager_capture::LEGACY_TRANSCRIPT_FILE`], not a second copy.
///
/// The session bundle is the durable state: nothing in the crate writes
/// `.marmel/.session_transcript.json`. The name survives so a **stale** copy left
/// by an older release is still recognized and named out loud instead of merged:
/// [`crate::session::restore`] reports "legacy journals found, not restorable"
/// (ignored must not mean invisible) and [`Plan::clear`] / [`Plan::archive`]
/// delete it with the plan, as they delete the override named by
/// [`FORCED_PHASE_FILE`].
pub use crate::session::manager_capture::LEGACY_TRANSCRIPT_FILE as TRANSCRIPT_FILE;

// High-level mission phases: the on-disk plan and its pending/complete state,
// whose single authority is `Plan` plus [`crate::plan_parse`].
// Nothing on the live path switches "phases": enforcement lives in the turn
// loops (`src/ui/session.rs`, `src/orchestrator/**`), which consult
// `Plan::exists()`, `Plan::pending_tasks()` and `Plan::check_plan_on_marker`.

// The plan task-line grammar (`- [ ] [t-xxx]`, `- [ ] (t-xxx)`, `- [X] …`) and
// the whole-document checkbox checks have exactly one owner:
// [`crate::plan_parse`]. This module keeps no copy of those patterns and
// introduces none: every read below delegates to the shared parser, so a
// parenthesised task id is recognised everywhere or nowhere. Nor is a task id
// ever guessed by scanning an arbitrary blob (serialized tool arguments, plan
// text, deliverable prose) for a `t-…` run — that would bind ids hiding inside
// paths and check off tasks that were never executed. Ids arrive bound by the
// caller, decorated on a marker, or proven boundary-safe by
// [`crate::plan_parse::line_matches_task_id`].

/// Mutex ensuring atomic filesystem operations across parallel subagents on the execution plan.
static PLAN_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

type PlanStartTime = (std::time::Instant, chrono::DateTime<chrono::Local>);

static PLAN_STARTED_AT: std::sync::LazyLock<std::sync::RwLock<Option<PlanStartTime>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(None));

static PLAN_COMPLETED_AT: std::sync::LazyLock<std::sync::RwLock<Option<std::time::Instant>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(None));

pub fn record_plan_start() {
    record_plan_start_at(std::time::Instant::now());
}

/// Record that an execution plan started at a specific instant.
pub fn record_plan_start_at(inst: std::time::Instant) {
    if let Ok(mut g) = PLAN_STARTED_AT.write() {
        *g = Some((inst, chrono::Local::now()));
    }
    if let Ok(mut g) = PLAN_COMPLETED_AT.write() {
        *g = None;
    }
}

pub fn record_plan_completed() {
    if let Ok(mut g) = PLAN_COMPLETED_AT.write()
        && g.is_none()
    {
        *g = Some(std::time::Instant::now());
    }
}

/// Retrieve the instant when the plan was completed, if finished.
pub fn get_plan_completed_time() -> Option<std::time::Instant> {
    PLAN_COMPLETED_AT.read().ok().and_then(|g| *g)
}

/// Clear the recorded plan start time upon completion / archive.
pub fn clear_plan_start() {
    if let Ok(mut g) = PLAN_STARTED_AT.write() {
        *g = None;
    }
    if let Ok(mut g) = PLAN_COMPLETED_AT.write() {
        *g = None;
    }
}

/// Retrieve the start time of the active execution plan, falling back to disk metadata if needed.
pub fn get_plan_start_time() -> Option<(std::time::Instant, chrono::DateTime<chrono::Local>)> {
    if let Ok(g) = PLAN_STARTED_AT.read()
        && let Some((inst, wall)) = *g
    {
        return Some((inst, wall));
    }
    let plan_path = std::path::Path::new(MARMEL_DIR).join(PLAN_FILE);
    if let Ok(meta) = std::fs::metadata(&plan_path)
        && let Ok(mod_time) = meta.created().or_else(|_| meta.modified())
    {
        let wall: chrono::DateTime<chrono::Local> = mod_time.into();
        let elapsed = std::time::SystemTime::now()
            .duration_since(mod_time)
            .unwrap_or_default();
        let inst = std::time::Instant::now()
            .checked_sub(elapsed)
            .unwrap_or_else(std::time::Instant::now);
        return Some((inst, wall));
    }
    None
}

/// The terminal marker a subagent appends to its deliverable:
/// `MISSION COMPLETE (task-id)` on success, `FAILED` / `REPLAN REQUIRED` when
/// it cannot.
///
/// The grammar — marker set, precedence rules and parser — has exactly one
/// owner, [`crate::markers`], and this module only re-exports the type so that
/// every existing `crate::manager::phase::MissionMarker` path keeps resolving.
pub use crate::markers::MissionMarker;

/// Manages the on-disk execution plan and its state under a marmel directory.
///
/// The directory is configurable so tests can isolate against a temp dir, but
/// defaults to `./.marmel` for normal operation.
#[derive(Debug, Clone)]
pub struct Plan {
    dir: PathBuf,
}

impl Default for Plan {
    fn default() -> Self {
        Self::at(MARMEL_DIR)
    }
}

impl Plan {
    /// Create a plan manager rooted at `dir` (defaults to `./.marmel`).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn plan_path(&self) -> PathBuf {
        self.dir.join(PLAN_FILE)
    }

    /// Path of the legacy `.marmel/forced_phase.txt` override file, which is
    /// deleted together with the plan. No product path writes that file; a copy
    /// an older release left on disk must not survive a `/reset`.
    pub fn forced_phase_path(&self) -> PathBuf {
        self.dir.join(FORCED_PHASE_FILE)
    }

    /// Path of the **superseded** manager journal, kept for one reason only.
    ///
    /// The session bundle at [`Plan::session_bundle_path`] is the durable state
    /// and nothing writes this file. The accessor stays private because
    /// [`Plan::clear`] and [`Plan::archive`] must still delete a **stale** copy
    /// left on disk by an older release, exactly as [`FORCED_PHASE_FILE`]
    /// survives for its stale override.
    fn legacy_transcript_path(&self) -> PathBuf {
        self.dir.join(TRANSCRIPT_FILE)
    }

    /// Path of the **superseded** UI journal, kept for the same reason as
    /// [`Self::legacy_transcript_path`]: nothing writes it — the durable UI state
    /// is the `ui` member of the bundle at [`Plan::session_bundle_path`] — and
    /// [`Plan::clear`] / [`Plan::archive`] must still delete a **stale** copy an
    /// older release left on disk. The name is a plain literal here, just as
    /// [`TRANSCRIPT_FILE`] is a re-export.
    fn legacy_ui_journal_path(&self) -> PathBuf {
        self.dir.join(".ui_transcript.json")
    }

    /// Delete everything this plan owns as **session state**: the superseded
    /// manager journal and the superseded UI journal (stale copies only), the
    /// session bundle, and the in-memory manager capture the session holder keeps
    /// for a committer.
    ///
    /// A cleared plan must leave no session behind to resurrect: a bundle next to
    /// a cleared plan would be loaded by the next boot. The delete is loud on
    /// failure.
    fn sweep_session_state(&self) {
        crate::session::manager_capture::clear_capture();
        let transcript = self.legacy_transcript_path();
        if transcript.exists() {
            tracing::warn!(
                "Plan: deleting superseded manager journal at {}",
                transcript.display()
            );
            let _ = std::fs::remove_file(&transcript);
        }
        let ui_journal = self.legacy_ui_journal_path();
        if ui_journal.exists() {
            tracing::warn!(
                "Plan: deleting superseded UI journal at {}",
                ui_journal.display()
            );
            let _ = std::fs::remove_file(&ui_journal);
        }
        let bundle = self.session_bundle_path();
        if bundle.exists() {
            tracing::warn!("Plan: deleting session bundle at {}", bundle.display());
            if let Err(error) = std::fs::remove_dir_all(&bundle) {
                tracing::warn!(
                    "Plan: session bundle {} could not be deleted: {error}",
                    bundle.display()
                );
            }
        }
    }

    /// Root of the **session bundle** for this plan: `.marmel/session/`, holding
    /// `manifest.json`, `manager.json`, `ui.json` and `workers/<escaped-key>.json`;
    /// the manifest is written **last**, which is what makes a bundle committed.
    ///
    /// This is the single path authority for the bundle: `session::commit` and
    /// `session::restore` never re-derive it from [`MARMEL_DIR`], and callers must
    /// not hardcode `.marmel/session`. The bundle is also the only durable
    /// **manager** journal: the superseded `.marmel/.session_transcript.json`,
    /// whose name survives in
    /// [`crate::session::manager_capture::LEGACY_TRANSCRIPT_FILE`], is never
    /// written — it is only ever named and deleted, like the superseded
    /// `.marmel/.ui_transcript.json`.
    pub fn session_bundle_path(&self) -> PathBuf {
        self.dir.join(crate::session::format::BUNDLE_DIR_NAME)
    }

    /// Write the initial execution plan to `.marmel/execution_plan.md`,
    /// creating the directory if it does not exist.
    ///
    /// The markdown is run through the single newline normalizer,
    /// [`crate::plan_parse::normalize_newlines`], and given exactly one trailing
    /// newline **here, once, at write time**: `create` and `check_off` are the
    /// only writers of the active plan file and `check_off` is byte-faithful, so
    /// the bytes on disk are the normalized bytes written here.
    ///
    /// A **completed** plan that was never archived is snapshotted to
    /// `.marmel/archive/` first (see [`Plan::preserve_completed_plan_unlocked`]):
    /// `create` overwrites unconditionally, and a finished plan silently
    /// disappearing under the next `create_plan` is a recorded archive failure.
    pub fn create(&self, plan_markdown: &str) -> Result<()> {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;

        self.preserve_completed_plan_unlocked()?;

        let mut content = crate::plan_parse::normalize_newlines(plan_markdown);
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }

        let path = self.plan_path();
        std::fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
        record_plan_start();
        tracing::info!(
            "Execution plan created at {} ({} chars):\n{}",
            path.display(),
            plan_markdown.len(),
            plan_markdown.trim()
        );
        Ok(())
    }

    /// Read the raw *active* plan markdown, or `None` if no active plan exists
    /// on disk. There is no stale-archive fallback: an archive is a purely
    /// historical artifact and `is_complete()`, `pending_tasks()` and
    /// `all_tasks()` never read it, so an archived plan cannot resurrect a phase.
    ///
    /// The read takes the same `PLAN_MUTEX` that already guards
    /// `create`/`check_off`/`clear`/`archive`, because `check_off` rewrites the
    /// whole file with a non-atomic `fs::write`: an unguarded reader in another
    /// parallel subagent thread could otherwise observe a half-written plan and
    /// report "no pending tasks". Call sites that **already hold** the guard must
    /// use [`Plan::read_unlocked`] — `std::sync::Mutex` is not re-entrant and
    /// would deadlock. What this does *not* close is cross-process locking:
    /// `PLAN_MUTEX` is per-process, so two `marmel` binaries sharing one
    /// workspace still race.
    pub fn read(&self) -> Result<Option<String>> {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        self.read_unlocked()
    }

    /// [`Plan::read`] for call sites that already hold `PLAN_MUTEX`.
    fn read_unlocked(&self) -> Result<Option<String>> {
        let path = self.plan_path();
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            return Ok(Some(content));
        }
        Ok(None)
    }

    pub fn exists(&self) -> bool {
        self.plan_path().exists()
    }

    /// Clear and remove the active execution plan (and archive) from disk.
    pub fn clear(&self) -> Result<()> {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.plan_path();
        if path.exists() {
            tracing::warn!(
                "Plan::clear: deleting active plan file at {}",
                path.display()
            );
            let _ = std::fs::remove_file(&path);
        }
        let archive = self.latest_archive_path();
        if archive.exists() {
            tracing::warn!(
                "Plan::clear: deleting archive plan file at {}",
                archive.display()
            );
            let _ = std::fs::remove_file(&archive);
        }
        let forced = self.forced_phase_path();
        if forced.exists() {
            tracing::warn!(
                "Plan::clear: deleting forced phase file at {}",
                forced.display()
            );
            let _ = std::fs::remove_file(&forced);
        }
        // Session state: the superseded manager journal and the superseded UI
        // journal (stale copies only), the session bundle that replaced both, plus
        // the in-memory manager capture.
        self.sweep_session_state();
        tracing::warn!("Execution plan CLEARED from disk (all plan files deleted).");
        Ok(())
    }

    /// Parse all *unchecked* task ids (`- [ ] [t-xxx]`, `- [ ] (t-xxx)`, …) from
    /// the plan using the shared grammar in [`crate::plan_parse`], and
    /// **propagate any read failure**.
    ///
    /// Takes `PLAN_MUTEX` and reads through [`Plan::read_unlocked`], so a
    /// concurrent `check_off` can never be observed mid-write.
    ///
    /// * `Ok(vec![])` — there is **no plan file**, so genuinely nothing is pending;
    /// * `Err(_)` — a plan file exists but could not be read/decoded, which must
    ///   never be reported as "nothing pending": an unreadable plan is an
    ///   unfinished one.
    pub fn try_pending_tasks(&self) -> Result<Vec<String>> {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        match self.read_unlocked() {
            Ok(Some(content)) => Ok(parse_unchecked_tasks(&content)),
            Ok(None) => Ok(Vec::new()),
            Err(e) => Err(e.context(format!(
                "pending tasks are UNKNOWN (not 'complete'): execution plan at {} could not be read",
                self.plan_path().display()
            ))),
        }
    }

    /// Parse *all* task ids present in the plan (both `- [ ]` and `- [x]`),
    /// propagating read failures — see [`Plan::try_pending_tasks`].
    pub fn try_all_tasks(&self) -> Result<Vec<String>> {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        match self.read_unlocked() {
            Ok(Some(content)) => Ok(crate::plan_parse::all_task_ids(&content)),
            Ok(None) => Ok(Vec::new()),
            Err(e) => Err(e.context(format!(
                "plan task ids are UNKNOWN (not 'complete'): execution plan at {} could not be read",
                self.plan_path().display()
            ))),
        }
    }

    /// Compatibility wrapper over [`Plan::try_pending_tasks`] for call sites that
    /// still expect a plain `Vec` (`src/orchestrator/mod.rs`, `src/ui/session.rs`,
    /// `src/harness/plan.rs`).
    ///
    /// **A read failure is logged as an error and yields an empty list** — the
    /// empty list alone must never be read as "the plan is done", which is why
    /// the completion authority is [`Plan::is_complete`]: it returns `false` when
    /// the plan cannot be read, so the dispatcher keeps working instead of
    /// declaring victory. New call sites must use [`Plan::try_pending_tasks`].
    pub fn pending_tasks(&self) -> Vec<String> {
        match self.try_pending_tasks() {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::error!(
                    "Plan::pending_tasks: {e:#} — reporting no pending tasks, but the plan is \
                     NOT complete (M8); migrate this call site to try_pending_tasks()"
                );
                Vec::new()
            }
        }
    }

    /// Compatibility wrapper over [`Plan::try_all_tasks`] — see
    /// [`Plan::pending_tasks`] for the unreadable-plan contract.
    pub fn all_tasks(&self) -> Vec<String> {
        match self.try_all_tasks() {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::error!(
                    "Plan::all_tasks: {e:#} — reporting no tasks, but the plan state is UNKNOWN \
                     (M8); migrate this call site to try_all_tasks()"
                );
                Vec::new()
            }
        }
    }

    /// Returns `true` when the plan has at least one task and none remain unchecked.
    ///
    /// An unreadable plan is **never** complete: the failure is logged and
    /// `false` is returned, so no dispatcher can treat "cannot read the plan" as
    /// "nothing pending".
    pub fn is_complete(&self) -> bool {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        self.is_complete_unlocked()
    }

    /// The single completion authority over plan **content**: complete when the
    /// plan has at least one checked task checkbox and no open one. Every gate
    /// that decides "is this plan finished" (`is_complete`, the `check_off`
    /// auto-snapshot, the `create` pre-overwrite preservation) goes through here,
    /// so they can never disagree.
    fn completion_of(content: &str) -> bool {
        !crate::plan_parse::has_unchecked_box(content)
            && crate::plan_parse::has_checked_box(content)
    }

    /// [`Plan::is_complete`] for call sites that already hold `PLAN_MUTEX`
    /// (`archive`, `check_off`) — see [`Plan::read_unlocked`].
    fn is_complete_unlocked(&self) -> bool {
        let content = match self.read_unlocked() {
            Ok(Some(content)) => content,
            Ok(None) => return false,
            Err(e) => {
                tracing::error!("Plan::is_complete: {e:#} — treating the plan as INCOMPLETE (M8)");
                return false;
            }
        };
        Self::completion_of(&content)
    }

    /// Directory holding the completed-plan snapshots (`.marmel/archive/`).
    fn archive_dir(&self) -> PathBuf {
        self.dir.join(ARCHIVE_SUBDIR)
    }

    /// Path of the latest archived-plan convenience copy
    /// (`.marmel/execution_plan_archive.md`).
    fn latest_archive_path(&self) -> PathBuf {
        self.dir.join(LATEST_ARCHIVE_FILE)
    }

    /// Pick the destination for a completed-plan snapshot inside `archive_dir`.
    ///
    /// Collision-safe by construction (the second-resolution UTC name is not
    /// unique on its own):
    ///
    /// * a snapshot holding **exactly these bytes** is reused, so one completed
    ///   plan leaves exactly one snapshot — the `check_off` auto-snapshot and a
    ///   later explicit `archive()` no longer accumulate byte-equivalent
    ///   duplicates (>1 s apart) or silently truncate each other (same second);
    /// * a name already taken by **different** bytes is never truncated: the
    ///   writer steps to `execution_plan_<ts>_2.md`, `_3.md`, … until the name is
    ///   free, so an earlier snapshot can never be overwritten.
    ///
    /// Returns the destination plus whether it already holds these bytes (then no
    /// write is needed). Callers must hold [`PLAN_MUTEX`].
    fn snapshot_dest(archive_dir: &Path, content: &str) -> Result<(PathBuf, bool)> {
        let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut identical: Option<PathBuf> = None;
        match std::fs::read_dir(archive_dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if !name.starts_with(SNAPSHOT_PREFIX) || !name.ends_with(".md") {
                        continue;
                    }
                    taken.insert(name.to_owned());
                    if identical.is_none()
                        && path.is_file()
                        && std::fs::read(&path).is_ok_and(|bytes| bytes == content.as_bytes())
                    {
                        identical = Some(path);
                    }
                }
            }
            Err(e) => tracing::warn!(
                "Plan: could not list {} to check snapshot names for collisions: {e}",
                archive_dir.display()
            ),
        }
        if let Some(existing) = identical {
            return Ok((existing, true));
        }
        let mut name = format!("{SNAPSHOT_PREFIX}{stamp}.md");
        let mut suffix = 1_u32;
        while taken.contains(&name) {
            suffix += 1;
            name = format!("{SNAPSHOT_PREFIX}{stamp}_{suffix}.md");
            if suffix > 100_000 {
                anyhow::bail!(
                    "no free snapshot name for {} in {}",
                    stamp,
                    archive_dir.display()
                );
            }
        }
        Ok((archive_dir.join(name), false))
    }

    /// Write (or reuse — see [`Plan::snapshot_dest`]) a completed-plan snapshot
    /// of `content` under `.marmel/archive/`. Never touches the active plan
    /// file. Callers must hold [`PLAN_MUTEX`] and have created the marmel
    /// directory.
    fn write_snapshot_unlocked(&self, content: &str) -> Result<PathBuf> {
        let archive_dir = self.archive_dir();
        std::fs::create_dir_all(&archive_dir)
            .with_context(|| format!("creating {}", archive_dir.display()))?;
        let (dest, reused) = Self::snapshot_dest(&archive_dir, content)?;
        if reused {
            tracing::info!(
                "Completed execution plan snapshot already present at {} (not written twice)",
                dest.display()
            );
        } else {
            std::fs::write(&dest, content)
                .with_context(|| format!("writing {}", dest.display()))?;
        }
        Ok(dest)
    }

    /// Preserve a **completed** active plan before `create` replaces it, so a
    /// finished plan can never be silently clobbered by the next `create_plan`.
    ///
    /// Only a complete plan is snapshotted: an incomplete plan is the working
    /// checkpoint and is deliberately left for whoever is executing it. The
    /// active file is not removed and no session state is swept — `create` is
    /// installing a new mission in the same directory, this is a copy, not an
    /// archive. A snapshot that already holds these bytes (the `check_off`
    /// auto-snapshot normally did exactly that) costs nothing.
    ///
    /// A snapshot/write failure is propagated: losing the only copy of a
    /// completed plan silently is the bug this closes, so the caller must see it.
    /// An **undecodable** active plan is the one exception — there is nothing
    /// preservable to snapshot, and a corrupt checkpoint must never block a new
    /// `create_plan` (replacing it is the documented recovery path), so it is
    /// reported as an error and preservation is skipped.
    fn preserve_completed_plan_unlocked(&self) -> Result<()> {
        let content = match self.read_unlocked() {
            Ok(Some(content)) => content,
            Ok(None) => return Ok(()),
            Err(e) => {
                tracing::error!(
                    "Plan: the active plan at {} could not be read before `create_plan` replaced \
                     it, so it was NOT preserved: {e:#}",
                    self.plan_path().display()
                );
                return Ok(());
            }
        };
        if !Self::completion_of(&content) {
            return Ok(());
        }
        let dest = self.write_snapshot_unlocked(&content)?;
        let latest = self.latest_archive_path();
        if let Err(e) = std::fs::write(&latest, &content) {
            tracing::warn!(
                "Plan: {} could not be refreshed before create_plan overwrote the completed \
                 plan: {e}",
                latest.display()
            );
        }
        tracing::warn!(
            "Completed execution plan snapshotted to {} before create_plan replaced the active \
             plan file {}",
            dest.display(),
            self.plan_path().display()
        );
        Ok(())
    }

    /// Archive the current execution plan to `.marmel/archive/execution_plan_<timestamp>.md`
    /// and `.marmel/execution_plan_archive.md`, and clean up `.marmel/execution_plan.md`.
    ///
    /// A plan is only archived once it is complete. An incomplete plan (one with
    /// at least one open task checkbox) is the working checkpoint — archiving it
    /// would silently lose it — so this returns `Ok(None)` when `is_complete()`
    /// is false and leaves every file untouched.
    ///
    /// IO failures are **propagated, not swallowed**: if the latest-archive copy
    /// or the removal of the active plan file fails, this returns `Err` naming
    /// the failed step instead of reporting a success the disk does not show (a
    /// swallowed removal left the plan phase EXECUTING, kept
    /// `ui::session::plan_archived_during_round` from ever firing, and let the
    /// next `create_plan` stack on top). The snapshot name is collision-safe
    /// ([`Plan::snapshot_dest`]), so retrying after such a failure reuses the
    /// snapshot already written instead of writing a second copy.
    pub fn archive(&self) -> Result<Option<PathBuf>> {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        if !self.is_complete_unlocked() {
            tracing::warn!(
                "Plan archive skipped: plan is incomplete (contains pending unchecked tasks)."
            );
            return Ok(None);
        }
        let path = self.plan_path();
        let Some(content) = self.read_unlocked()? else {
            tracing::warn!(
                "Plan archive skipped: plan file {} does not exist.",
                path.display()
            );
            return Ok(None);
        };
        let dest = self.write_snapshot_unlocked(&content)?;
        let latest = self.latest_archive_path();
        std::fs::write(&latest, &content)
            .with_context(|| format!("writing {}", latest.display()))?;
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        // An archived plan is a finished session: the superseded manager journal,
        // the superseded UI journal and the session bundle go with it. Only now —
        // every artifact this contract promises is on disk and the active plan is
        // really gone.
        self.sweep_session_state();
        clear_plan_start();
        tracing::warn!(
            "Execution plan completed and ARCHIVED to {} (active plan file {} removed from disk)",
            dest.display(),
            path.display()
        );
        Ok(Some(dest))
    }

    /// Toggle one `- [ ] [t-id]` task to `- [x] [t-id]` on disk. This is the
    /// **only** plan mutation and it derives nothing: the caller must hand it an
    /// id taken from an explicit source — the plan line it is acting on, the
    /// bound `delegate_task` `task_id`, or a marker decoration resolved by
    /// [`Plan::check_plan_on_deliverable`]. The id is normalized by
    /// [`crate::task_id`] and matched by [`crate::plan_parse::check_off_content`],
    /// so an id that only appears inside another task's description never flips
    /// that other line.
    ///
    /// When the final task is checked off and the plan becomes complete, a
    /// completion snapshot is automatically archived to `.marmel/archive/` while
    /// the active plan file stays readable.
    ///
    /// Returns `Ok(true)` if a pending checkbox was flipped, `Ok(false)` if the
    /// task id was not found (or was already checked), and `Err` on IO failure.
    pub fn check_off(&self, task_id: &str) -> Result<bool> {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let Some(content) = self.read_unlocked()? else {
            tracing::warn!("check_off({task_id}): no active plan file on disk");
            return Ok(false);
        };
        let clean_tid = crate::task_id::normalize_task_id_ref(task_id);
        let updated = {
            let (rewritten, flipped) = crate::plan_parse::check_off_content(&content, clean_tid);
            if !flipped {
                tracing::warn!("check_off({task_id}): task id not found or already checked");
                return Ok(false);
            }
            rewritten
        };

        std::fs::write(self.plan_path(), &updated)
            .with_context(|| format!("updating {}", self.plan_path().display()))?;
        let complete = self.is_complete_unlocked();
        tracing::info!(
            "Plan task [{task_id}] checked off on disk (active: {}, complete: {})",
            self.plan_path().display(),
            complete
        );
        // When this flip completes the plan, write a completion snapshot to
        // `.marmel/archive/` (the promise in the `archive` docstring) but KEEP the
        // active file readable until the caller archives explicitly. This is a
        // best-effort snapshot: failing here must not fail the check-off itself —
        // but it must be said out loud, because the snapshot is the only copy of
        // this completed plan if the explicit archive never runs.
        if complete {
            record_plan_completed();
            if let Err(e) = self.write_completed_snapshot(&updated) {
                tracing::error!(
                    "check_off({task_id}): plan is complete but the completion snapshot failed \
                     (the check-off itself succeeded): {e:#}"
                );
            }
        }
        Ok(true)
    }

    /// Write a best-effort completion snapshot of `content` to
    /// `.marmel/archive/execution_plan_<timestamp>.md`, leaving the active
    /// `.marmel/execution_plan.md` untouched. Used by `check_off` when the last
    /// task is flipped and the plan becomes complete.
    ///
    /// The name is collision-safe and deduplicated ([`Plan::snapshot_dest`]): the
    /// explicit `archive()` that follows cannot silently truncate this snapshot
    /// inside the same second, and cannot leave a second byte-equivalent copy
    /// behind when the clock has moved on.
    fn write_completed_snapshot(&self, content: &str) -> Result<PathBuf> {
        let dest = self.write_snapshot_unlocked(content)?;
        let latest = self.latest_archive_path();
        if let Err(e) = std::fs::write(&latest, content) {
            tracing::warn!(
                "Plan: {} could not be refreshed by the completion snapshot: {e}",
                latest.display()
            );
        }
        tracing::info!(
            "Auto-saved completed execution plan snapshot to {}",
            dest.display()
        );
        Ok(dest)
    }

    /// Delegation-aware auto check-off for a subagent deliverable, judged from
    /// its **body text** alone.
    ///
    /// A `MISSION COMPLETE (t-xxx)` terminal marker flips the matching
    /// `- [ ] [t-xxx]` plan line to `- [x] [t-xxx]` on disk; a `FAILED` /
    /// `REPLAN REQUIRED` marker, or any unrecognized output, leaves the task
    /// unchecked. Parsing is delegated to [`crate::markers`], which is FAIL-first:
    /// a line-initial/terminal `FAILED` (or any `REPLAN REQUIRED`) outranks a
    /// `MISSION COMPLETE` merely embedded in the prose of a failed deliverable.
    ///
    /// The task id is resolved from the marker when present (the optional
    /// `task_id` argument may be `None`); callers that already bind a `task_id`
    /// for the `delegate_task` may pass it explicitly, so check-off still works
    /// when the subagent omits the parenthesized id. With neither, the id has to
    /// be recovered from the deliverable itself and is then accepted only when it
    /// also occurs there as a boundary-safe task-id token — see
    /// [`Plan::check_plan_on_deliverable`]. A canned success token carries no
    /// marker and never checks anything off.
    ///
    /// Callers that already hold the structured verdict prefer
    /// [`Plan::check_plan_on_deliverable`], which never re-parses the body.
    ///
    /// Returns `Ok(true)` when the task was checked off on disk, `Ok(false)` when
    /// the marker was not a completion (leaving the task unchecked) or the task id
    /// was not found / already checked, and `Err` on IO failure.
    pub fn check_plan_on_marker(&self, task_id: Option<&str>, deliverable: &str) -> Result<bool> {
        self.check_plan_on_deliverable(None, task_id, deliverable)
    }

    /// Delegation-aware auto check-off that prefers the **structured**
    /// [`MissionMarker`] carried by the deliverable (the `Deliverable.marker`
    /// field) over any re-parse of its body.
    ///
    /// [`MissionMarker::resolve`] uses `marker` verbatim when populated and only
    /// falls back to the positional body parse when it is `None`. A `FAILED` /
    /// `REPLAN REQUIRED` verdict therefore always leaves `- [ ] [t-xxx]`
    /// untouched, no matter what marker text the body happens to quote.
    ///
    /// The task id may only come from an explicit, structured source, tried in
    /// this order: the caller's bound `task_id` (the `delegate_task` argument,
    /// normalized by [`crate::task_id`]), then the id inside a caller-supplied
    /// structured marker, then the id decorated onto the marker
    /// (`MISSION COMPLETE (t-xxx)`). This entry point never scans an arbitrary
    /// blob (tool arguments, plan text, deliverable prose) for a `t-…` token.
    /// When the id is neither bound by the caller nor carried by a structured
    /// marker it came from parsing the deliverable, so it is additionally required
    /// to occur in that deliverable as a boundary-safe task-id token
    /// ([`deliverable_mentions_task_id`]): an id that exists only as a substring
    /// inside a word or a path (`t-notes` inside `src/chart-notes.md`) can never
    /// flip a plan line.
    ///
    /// **A bound id and a marker id must agree.** When the caller binds an id *and*
    /// the resolved marker carries one of its own, a mismatch aborts the check-off
    /// instead of quietly preferring either side: a completion for one task can
    /// never tick another task's box. The box stays pending, which costs at most
    /// one extra round; a wrong tick loses work.
    ///
    /// The `deliverable` argument must be the **real deliverable content** as
    /// returned by the subagent (`Deliverable::content`); a canned verdict token
    /// (`"ok"`, `"success"`, `"done"`) carries no marker and is never accepted as
    /// evidence.
    ///
    /// Returns `Ok(true)` when the task was checked off on disk, `Ok(false)` when
    /// the resolved marker was not a completion, the task id could not be
    /// resolved, a body-derived id failed the token proof, or a bound id disagrees
    /// with the marker's own id, and `Err` on IO failure.
    pub fn check_plan_on_deliverable(
        &self,
        marker: Option<&MissionMarker>,
        task_id: Option<&str>,
        deliverable: &str,
    ) -> Result<bool> {
        let structured = marker;
        let Some(resolved) = MissionMarker::resolve(structured, deliverable) else {
            tracing::warn!(
                "check_plan_on_deliverable: No terminal marker found in deliverable ({} chars)",
                deliverable.len()
            );
            return Ok(false);
        };
        if !resolved.is_complete() {
            tracing::warn!("check_plan_on_deliverable: Marker is not complete: {resolved:?}");
            return Ok(false);
        }
        // Explicit sources first; own the id so no borrow is held past `resolved`.
        let bound = task_id
            .map(crate::task_id::normalize_task_id_ref)
            .filter(|t| !t.is_empty());
        let marker_id = match &resolved {
            MissionMarker::Complete { task_id } => task_id.as_deref(),
            _ => None,
        };
        // Only when *both* explicit sources are absent was the id produced by
        // re-parsing the deliverable body.
        let derived_from_body = structured.is_none() && bound.is_none();
        let tid = bound
            .map(str::to_owned)
            .or_else(|| marker_id.map(str::to_owned));
        let Some(tid) = tid else {
            tracing::warn!(
                "check_plan_on_deliverable: No task_id resolved from marker {resolved:?}"
            );
            return Ok(false);
        };
        if let (Some(bound), Some(marker_id)) = (bound, marker_id)
            && !crate::plan_parse::task_id_eq(bound, marker_id)
        {
            // A completion marker that names a **different** plan task than the
            // id the caller bound is a contradiction, not a tie-break candidate.
            // Ticking the bound box on the strength of another task's completion
            // is precisely the corruption this entry point exists to prevent, so
            // the check-off is refused fail-closed: the box stays pending and can
            // be re-delegated.
            tracing::warn!(
                "check_plan_on_deliverable: refusing check-off — bound task id [{bound}] disagrees \
                 with the marker's own id [{marker_id}]; a completion for one task can never tick \
                 another task's box"
            );
            return Ok(false);
        }
        if derived_from_body && !deliverable_mentions_task_id(deliverable, &tid) {
            tracing::warn!(
                "check_plan_on_deliverable: refusing check-off of [{tid}] — that id is not mentioned as \
                 a task-id token anywhere in the deliverable (H6: ids are never inferred from prose)"
            );
            return Ok(false);
        }
        self.check_off(&tid)
    }
}

/// Parse all unchecked (`- [ ] [t-xxx]`, `- [ ] (t-xxx)`, …) task ids from raw
/// plan markdown. Thin alias onto the single grammar owner,
/// [`crate::plan_parse::unchecked_task_ids`]; kept so existing consumers
/// (`Plan::pending_tasks`, harness `create_plan` reporting) do not change.
pub fn parse_unchecked_tasks(markdown: &str) -> Vec<String> {
    crate::plan_parse::unchecked_task_ids(markdown)
}

/// `true` when the deliverable text mentions `task_id` as a **boundary-safe
/// task-id token**.
///
/// The matching rule is not owned here: every line is handed to
/// [`crate::plan_parse::line_matches_task_id`], the plan grammar owner's
/// boundary-aware matcher. That is what makes the check word-boundary safe —
/// `t-notes` is *not* a token inside `src/chart-notes.md` (the `t-…` run starts
/// mid-word), while it is one inside `MISSION COMPLETE (t-notes)`.
///
/// [`Plan::check_plan_on_deliverable`] uses it as the last line of defence: an id
/// recovered from free text must really be named by that text, otherwise a
/// completion for one task could silently check off an unrelated plan line.
#[must_use]
fn deliverable_mentions_task_id(deliverable: &str, task_id: &str) -> bool {
    deliverable
        .lines()
        .any(|line| crate::plan_parse::line_matches_task_id(line, task_id))
}

#[cfg(test)]
#[path = "phase_tests.rs"]
mod tests;
