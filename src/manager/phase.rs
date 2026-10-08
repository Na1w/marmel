//! Mission phase state machine, disk plan management, and auto check-off.
//!
//! REQ-PLAN-001 (Mission Phase States): what decides the phase of a mission is
//! the plan file on disk — the live turn loops consult [`Plan::exists`],
//! [`Plan::pending_tasks`] and [`Plan::is_complete`] (`src/ui/session.rs`,
//! `src/orchestrator/**`). The `MissionPhase` enum, `Plan::determine_phase`,
//! `Plan::forced_phase` and `Plan::is_silent_dispatcher` that used to model the
//! same thing here were **never consulted by any live path** and have been
//! deleted (recon item L8; evidence and the wire-or-delete decision are recorded
//! on [`Plan`]).
//!
//! REQ-PLAN-002 (Disk-Driven Automatic Plan Check-off): The plan lives at
//! `.marmel/execution_plan.md` and is formatted with `- [ ] [t-xxx]` checkboxes
//! (`- [ ] (t-xxx)` and the other spellings are equally valid — the grammar has
//! one owner, [`crate::plan_parse`]). Exactly two authorities may flip a box:
//!
//! * a **tool call** the loop makes itself, with an explicitly bound plan id:
//!   the live gate is the structured `ToolResult::is_error` flag and the raw
//!   mutation is [`Plan::check_off`]. As of t-035a **no live call site uses this
//!   half any more** — every plan box the session loops could tick came from a
//!   delegation, and both of those sites now go through the marker gate below.
//!   `check_off` therefore stays what it always was: a raw mutation that derives
//!   nothing (kept for the plan-grammar tests and any future tool-bound update),
//!   never a success classifier;
//! * a **subagent deliverable** — including a `delegate_task` tool result that
//!   carries a plan task id — is flipped only through
//!   [`Plan::check_plan_on_deliverable`] (REQ-PLAN-002 + REQ-ORCH-005, below).
//!   For this half `is_error == false` is **not** evidence of completion: a
//!   deliverable with no terminal completion marker leaves its box unchecked.
//!   Both live session loops and the orchestrator's `apply_check_off` route
//!   through that single gate (t-035a).
//!
//! The free-form substring heuristic that used to back the tool half
//! (`output_is_success` + `Plan::check_off_on_success`) is **deleted**: recon
//! item M9 (`docs/recon_bugs_manager.md`) recorded that it mis-classified any
//! output merely containing `error` / `failed` (`thiserror`, `src/error.rs`,
//! `errors.log`), and `docs/decision_dead_code_manager.md` §6 left the
//! wire-or-delete decision here — after the dead `AgentLoop` was removed neither
//! had a production caller left, so wiring them up again would re-introduce a
//! second, divergent success matcher (and its H6 canned-`"ok"` failure mode)
//! beside the marker gate.
//!
//! REQ-PLAN-002 + REQ-ORCH-005 (Delegation-aware check-off): a *subagent*
//! deliverable is checked off via its terminal marker, not free-form output.
//! Only a `MISSION COMPLETE (t-xxx)` marker flips `- [ ] [t-xxx]` to
//! `- [x] [t-xxx]`; `FAILED` / `REPLAN REQUIRED` markers leave the task
//! unchecked. The marker grammar (marker set, precedence, parser) has a single
//! owner, [`crate::markers`], and is FAIL-first: an explicit `FAILED` /
//! `REPLAN REQUIRED` signal outranks an embedded `MISSION COMPLETE` substring.
//! **Bug M9 (one matcher, not two):** the check-off decision is taken solely by
//! [`crate::markers`] — [`MissionMarker::resolve`] for the verdict plus
//! [`crate::markers::has_failure_marker`] /
//! [`crate::markers::has_replan_marker`] for presence tests. This module
//! declares no success or failure predicate of its own (the duplicated pair it
//! once carried tested `MISSION COMPLETE` as a substring *before* the failure
//! test), so `not ok` prose, a benign `test result: ok. … 0 failed` counter, or
//! a word that merely contains a marker token cannot decide the outcome; the
//! adversarial cases are pinned in the tests of this module.
//! `Plan::check_plan_on_marker` (content-only) and
//! `Plan::check_plan_on_deliverable` (structured marker preferred) apply it to
//! the plan. The **task id** is resolved from an explicit source only: the
//! caller's bound `task_id` (the `delegate_task` argument, normalized by
//! [`crate::task_id`]), the structured [`MissionMarker`] field, or the id
//! decorated onto the marker (`MISSION COMPLETE (t-001)`). This module owns no
//! regex and never scans an arbitrary blob for a `t-…` token — the H6 fallback
//! (`manager/loop.rs::extract_task_id`, recon `docs/recon_bugs_manager.md` H6)
//! is deleted and nothing in the live path replaces it. As the last line of
//! defence, an id that was neither bound by the caller nor carried by a
//! structured marker must additionally appear in the deliverable as a
//! boundary-safe task-id token ([`crate::plan_parse::line_matches_task_id`]),
//! so an id that exists only as a substring inside a word or a path
//! (`t-notes` inside `src/chart-notes.md`) can never flip a plan line. The
//! marker layer applies the same proof at its own end: [`crate::markers`] binds
//! a `(t-xxx)` id only when it is a boundary-safe token on the marker's own
//! line, and no longer scans the whole deliverable for the first `t-…` run.
//!
//! REQ-PLAN-003 (Silent Dispatcher Enforcement): in the executing phase the
//! agent suppresses conversational filler and iterates strictly through
//! unchecked plan items until every task is marked `[x]`. The enforcement point
//! is the live delegation loop (`src/orchestrator/**` driving
//! `Plan::pending_tasks()` / `Plan::is_complete()`); this module supplies the
//! state it iterates on, not the suppression itself.
//!
//! **Bug M8 (unreadable plan ≠ finished plan).** `pending_tasks`/`all_tasks`
//! used to fold a failed `fs::read_to_string` into an empty `Vec`, i.e. "no
//! pending work", which the dispatcher and the UI auto-nudge read as "the plan
//! is done". The error-propagating forms are [`Plan::try_pending_tasks`] and
//! [`Plan::try_all_tasks`]: `Ok(vec![])` means "there is no plan file", `Err(..)`
//! means "the plan exists but could not be read". The `Vec`-shaped
//! [`Plan::pending_tasks`]/[`Plan::all_tasks`] remain for call sites that cannot
//! handle a `Result` yet; they log the failure with `tracing::error!` instead of
//! swallowing it, and the completion gate [`Plan::is_complete`] returns `false`
//! when the plan cannot be read, so no loop can ever conclude that an
//! unreadable plan was finished.
//!
//! REQ-PLAN-004 (Disk Override): **not implemented on the live path.** No code
//! in the crate writes `.marmel/forced_phase.txt` and the reader
//! (`Plan::forced_phase`) was deleted as unreachable (recon item L8, see the
//! note on [`Plan`]). [`FORCED_PHASE_FILE`] survives as the name of that legacy
//! file so [`Plan::clear`] can still delete a stale override left on disk.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Directory (relative to the workspace) holding the plan and phase override.
pub const MARMEL_DIR: &str = ".marmel";
/// Plan file name inside the marmel directory.
pub const PLAN_FILE: &str = "execution_plan.md";
/// Name of the legacy phase-override file inside the marmel directory.
///
/// Nothing in the crate writes or reads it any more — the REQ-PLAN-004 gate was
/// deleted as unreachable (recon L8, see the note above [`Plan`]). The constant
/// survives because [`Plan::clear`] deletes a stale override left on disk by an
/// older release, and `harness::Workspace` re-exports it as the canonical path.
pub const FORCED_PHASE_FILE: &str = "forced_phase.txt";
/// Session transcript file name inside the marmel directory.
pub const TRANSCRIPT_FILE: &str = ".session_transcript.json";
/// UI transcript file name inside the marmel directory.
pub const UI_TRANSCRIPT_FILE: &str = ".ui_transcript.json";

// High-level mission phases.
//
// **Deleted (recon item L8, `docs/recon_bugs_manager.md`).** `MissionPhase`,
// `MissionPhase::parse`, `Plan::forced_phase`, `Plan::determine_phase` and
// `Plan::is_silent_dispatcher` formed a phase gate that nothing on the live
// path ever consulted:
//
// * no caller outside `src/manager/` referenced any of them — the only
//   surviving uses were the `manager::mod` re-export and this module's own
//   unit tests, which certified an unwired path;
// * nothing in the crate ever **writes** `.marmel/forced_phase.txt`, so the
//   REQ-PLAN-004 disk override could never be triggered from a running binary
//   (`forced_phase()` always returned `None`);
// * the enforcement that actually ships lives in the live turn loops —
//   `src/ui/session.rs` (interactive Manager turn loop: which tools are
//   offered, plan-driven dispatch) and `src/orchestrator/**`
//   (`run_executing`/`delegate_task` as the Silent Dispatcher, REQ-ORCH-001 /
//   REQ-PLAN-003). Both consult the plan through `Plan::exists()`,
//   `Plan::pending_tasks()` and `Plan::check_plan_on_marker`, not through a
//   phase enum.
//
// Wiring the gate would have meant re-implementing tool gating in files owned
// by other tasks (`src/ui/**`, `src/orchestrator/**`) — out of scope here and a
// second source of truth. Deleting it removes no behaviour: no call site could
// observe a difference. What REQ-PLAN-001/003 still require of this module is
// what it does provide — the on-disk plan and its pending/complete state, the
// single authority for that being `Plan` + [`crate::plan_parse`].
//
// If REQ-PLAN-004 (disk override) is wanted again, it must be implemented as a
// live-path feature: a writer for the override file plus a consultation point
// in the live loops. `FORCED_PHASE_FILE` stays for that purpose and for the
// `/reset` cleanup in [`Plan::clear`].

// The plan task-line grammar (`- [ ] [t-xxx]`, `- [ ] (t-xxx)`, `- [X] …`) and
// the whole-document checkbox gates live in exactly one place:
// [`crate::plan_parse`] (dedup cluster C3). This module used to keep four
// private regexes here (`TASK_LINE_RE`, `ALL_TASKS_RE`, `UNCHECKED_BOX_RE`,
// `CHECKED_BOX_RE`) plus a per-call regex inside `check_off`; they had drifted
// apart — a parenthesised task id such as `- [ ] (t-002) migrate schema` was
// recognised by none of them, so that plan could never be listed as pending nor
// checked off (`docs/recon_bugs_harness_llm_ui.md` §3 C1/C2). The copies are
// gone; every read below delegates to the shared parser.
//
// **Invariant (H6 hardening, task t-031d): this module owns no regex at all.**
// The deleted `manager/loop.rs::extract_task_id` derived a task id by running
// `\(?\[?(t-[A-Za-z0-9_-]+)\]?\)?` over the whole serialized tool-argument blob,
// which matched ids hiding inside paths (`"path":"src/chart-notes.md"` →
// `t-notes`) and therefore checked off tasks that were never executed
// (`docs/recon_bugs_manager.md` H6). Nothing here may re-introduce that scan:
// ids arrive bound by the caller, decorated on a marker, or proven as a
// boundary-safe token by [`crate::plan_parse::line_matches_task_id`].

/// Mutex ensuring atomic filesystem operations across parallel subagents on the execution plan.
static PLAN_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

type PlanStartTime = (std::time::Instant, chrono::DateTime<chrono::Local>);

static PLAN_STARTED_AT: std::sync::LazyLock<std::sync::RwLock<Option<PlanStartTime>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(None));

static PLAN_COMPLETED_AT: std::sync::LazyLock<std::sync::RwLock<Option<std::time::Instant>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(None));

/// Record that an execution plan started right now.
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

/// Record that an execution plan completed all tasks right now.
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

/// The terminal marker a subagent appends to its deliverable (REQ-ORCH-005):
/// `MISSION COMPLETE (task-id)` on success, `FAILED` / `REPLAN REQUIRED` when
/// it cannot.
///
/// The grammar — marker set, precedence rules and parser — has exactly one
/// owner, [`crate::markers`]. This module used to carry a verbatim copy of the
/// enum, `contains_failed_marker` and `parse` (dedup cluster C4,
/// `docs/recon_duplication_helpers.md` §2.4); that copy tested `MISSION
/// COMPLETE` as a substring *before* `FAILED`, so a FAILED deliverable that
/// merely mentions `MISSION COMPLETE` in its prose was classified `Complete`
/// and flipped `- [ ] [t-xxx]` → `- [x] [t-xxx]` on disk (bug C1,
/// `docs/recon_bugs_manager.md`). The copy is gone; the name is re-exported so
/// every existing `crate::manager::phase::MissionMarker` path keeps resolving.
pub use crate::markers::MissionMarker;

/// Manages the on-disk execution plan and phase gating under a marmel directory.
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

    /// The absolute path of the marmel directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The absolute path of the execution plan file.
    pub fn plan_path(&self) -> PathBuf {
        self.dir.join(PLAN_FILE)
    }

    /// REQ-PLAN-004 cleanup: a legacy `.marmel/forced_phase.txt` override file
    /// is deleted together with the plan. No product path writes that file and
    /// the phase-gate helpers that used to read it are gone (recon L8, see the
    /// module docs), but a file left over from an older release must not
    /// survive a `/reset`.
    pub fn forced_phase_path(&self) -> PathBuf {
        self.dir.join(FORCED_PHASE_FILE)
    }

    /// The absolute path of the session transcript file.
    pub fn transcript_path(&self) -> PathBuf {
        self.dir.join(TRANSCRIPT_FILE)
    }

    /// The absolute path of the UI transcript file.
    pub fn ui_transcript_path(&self) -> PathBuf {
        self.dir.join(UI_TRANSCRIPT_FILE)
    }

    /// REQ-PLAN-001: write the initial execution plan to `.marmel/execution_plan.md`,
    /// creating the directory if it does not exist.
    ///
    /// Recon item **L6** (`docs/recon_bugs_manager.md`): the markdown is run
    /// through the single newline normalizer, [`crate::plan_parse::normalize_newlines`],
    /// and given exactly one trailing newline **here, once, at write time**.
    /// `create` and `check_off` are the only writers of the active plan file,
    /// and `check_off` is now byte-faithful, so a CRLF plan authored by a model
    /// no longer gets its whole body re-spelled on the first check-off — the
    /// bytes on disk are the normalized bytes written here.
    pub fn create(&self, plan_markdown: &str) -> Result<()> {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;

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
    /// on disk (t-203).
    ///
    /// The stale-archive fallback has been removed: when `.marmel/execution_plan.md`
    /// is absent, `None` is returned regardless of whether an archived snapshot
    /// exists. The archive is a purely historical artifact — `is_complete()`,
    /// `pending_tasks()` and `all_tasks()` must never read it, otherwise an
    /// archived plan would resurrect the executing phase.
    ///
    /// Recon item **L5** (`docs/recon_bugs_manager.md`): the read now takes the
    /// same `PLAN_MUTEX` that already guards `create`/`check_off`/`clear`/
    /// `archive`. `check_off` rewrites the whole file with a non-atomic
    /// `fs::write`, so an unguarded reader running in another parallel
    /// subagent thread could observe a half-written plan and report "no
    /// pending tasks" (or fail to find the line it is about to tick). The
    /// guard is what makes the read consistent with the write.
    ///
    /// Call sites that **already hold** the guard must use
    /// [`Plan::read_unlocked`] — `std::sync::Mutex` is not re-entrant and would
    /// deadlock (`check_off` and `archive` are exactly those sites). The gap
    /// this does *not* close is cross-process locking: `PLAN_MUTEX` is
    /// per-process, so two `marmel` binaries sharing one workspace still race.
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

    /// Returns `true` when a plan file exists on disk.
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
        let archive = self.dir.join("execution_plan_archive.md");
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
        let transcript = self.transcript_path();
        if transcript.exists() {
            tracing::warn!(
                "Plan::clear: deleting transcript file at {}",
                transcript.display()
            );
            let _ = std::fs::remove_file(&transcript);
        }
        let ui_transcript = self.ui_transcript_path();
        if ui_transcript.exists() {
            tracing::warn!(
                "Plan::clear: deleting UI transcript file at {}",
                ui_transcript.display()
            );
            let _ = std::fs::remove_file(&ui_transcript);
        }
        tracing::warn!("Execution plan CLEARED from disk (all plan files deleted).");
        Ok(())
    }

    /// Parse all *unchecked* task ids (`- [ ] [t-xxx]`, `- [ ] (t-xxx)`, …) from
    /// the plan, using the shared grammar in [`crate::plan_parse`], and
    /// **propagate any read failure** (bug M8).
    ///
    /// Takes `PLAN_MUTEX` (recon L5) and reads through [`Plan::read_unlocked`],
    /// so a concurrent `check_off` can never be observed mid-write.
    ///
    /// The distinction M8 is about:
    /// * `Ok(vec![])` — there is **no plan file**, so genuinely nothing is pending;
    /// * `Err(_)` — a plan file exists but could not be read/decoded, which must
    ///   never be reported as "nothing pending". The pre-M7 code collapsed both
    ///   into an empty `Vec`, so an unreadable plan made the Silent Dispatcher
    ///   (and the UI auto-nudge) believe the plan was finished.
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
    /// propagating read failures — see [`Plan::try_pending_tasks`] (bug M8).
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
    /// `src/harness/plan.rs`, `src/plan_parse.rs` tests).
    ///
    /// **A read failure is logged as an error and yields an empty list** — the
    /// empty list alone must never be read as "the plan is done", which is why
    /// the completion gate is [`Plan::is_complete`] (it returns `false` when the
    /// plan cannot be read, so the dispatcher keeps working instead of declaring
    /// victory). New call sites must use [`Plan::try_pending_tasks`].
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
    /// [`Plan::pending_tasks`] for the M8 contract.
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
    /// An unreadable plan is **never** complete (bug M8): the failure is logged
    /// and `false` is returned, so no dispatcher can treat "cannot read the plan"
    /// as "nothing pending".
    pub fn is_complete(&self) -> bool {
        let _guard = PLAN_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        self.is_complete_unlocked()
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
        // If there are ANY unchecked checkboxes (`[ ]` or `( )`) anywhere in the plan, it is NOT complete!
        if crate::plan_parse::has_unchecked_box(&content) {
            return false;
        }
        // Must contain at least one completed checkbox ([x] or (x))
        crate::plan_parse::has_checked_box(&content)
    }

    /// Archive the current execution plan to `.marmel/archive/execution_plan_<timestamp>.md`
    /// and `.marmel/execution_plan_archive.md`, and clean up `.marmel/execution_plan.md`.
    ///
    /// t-203 (a): a plan is only archived once it is complete. An incomplete plan
    /// (one with at least one unchecked box) is the working checkpoint — archiving
    /// it would silently lose it, so this returns `Ok(None)` when `is_complete()`
    /// is false and leaves every file untouched.
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
        let archive_dir = self.dir.join("archive");
        std::fs::create_dir_all(&archive_dir)
            .with_context(|| format!("creating {}", archive_dir.display()))?;
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let dest = archive_dir.join(format!("execution_plan_{ts}.md"));
        std::fs::write(&dest, &content).with_context(|| format!("writing {}", dest.display()))?;
        let latest = self.dir.join("execution_plan_archive.md");
        let _ = std::fs::write(&latest, &content);
        let _ = std::fs::remove_file(&path);
        let transcript = self.transcript_path();
        if transcript.exists() {
            let _ = std::fs::remove_file(&transcript);
        }
        let ui_transcript = self.ui_transcript_path();
        if ui_transcript.exists() {
            let _ = std::fs::remove_file(&ui_transcript);
        }
        clear_plan_start();
        tracing::warn!(
            "Execution plan completed and ARCHIVED to {} (active plan file {} removed from disk)",
            dest.display(),
            path.display()
        );
        Ok(Some(dest))
    }

    /// REQ-PLAN-002: toggle a single `- [ ] [t-id]` task to `- [x] [t-id]` on disk.
    ///
    /// This is the raw disk mutation: it derives nothing. The caller must hand
    /// it an id taken from an explicit source — the plan line it is acting on,
    /// the bound `delegate_task` `task_id`, or a marker decoration resolved by
    /// [`Plan::check_plan_on_deliverable`]. The id is normalized by
    /// [`crate::task_id`] and matched by [`crate::plan_parse::check_off_content`],
    /// so an id that only appears inside another task's description never flips
    /// that other line.
    ///
    /// When the final task is checked off and the plan becomes complete, a snapshot
    /// is automatically archived to `.marmel/archive/`.
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
        // t-203 (c): when this flip completes the plan, auto-archive the
        // completion snapshot to `.marmel/archive/` (per the `archive`
        // docstring's promise) but KEEP the active file readable until the
        // caller explicitly archives. This is a best-effort snapshot; a
        // failure here must not fail the check-off itself.
        if complete {
            record_plan_completed();
            let _ = self.write_completed_snapshot(&updated);
        }
        Ok(true)
    }

    /// t-203 (c): write a best-effort completion snapshot of `content` to
    /// `.marmel/archive/execution_plan_<timestamp>.md`, leaving the active
    /// `.marmel/execution_plan.md` untouched. Used by `check_off` when the last
    /// task is flipped to complete the plan.
    fn write_completed_snapshot(&self, content: &str) -> Result<PathBuf> {
        let archive_dir = self.dir.join("archive");
        std::fs::create_dir_all(&archive_dir)
            .with_context(|| format!("creating {}", archive_dir.display()))?;
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let dest = archive_dir.join(format!("execution_plan_{ts}.md"));
        std::fs::write(&dest, content).with_context(|| format!("writing {}", dest.display()))?;
        let latest = self.dir.join("execution_plan_archive.md");
        let _ = std::fs::write(&latest, content);
        tracing::info!(
            "Auto-saved completed execution plan snapshot to {}",
            dest.display()
        );
        Ok(dest)
    }

    /// REQ-PLAN-002 + REQ-ORCH-005: delegation-aware auto check-off for a
    /// subagent deliverable, judged from its **body text** alone.
    ///
    /// When the deliverable carries a `MISSION COMPLETE (t-xxx)` terminal
    /// marker, the matching `- [ ] [t-xxx]` plan line is flipped to
    /// `- [x] [t-xxx]` on disk. A `FAILED` / `REPLAN REQUIRED` marker (or any
    /// unrecognized output) leaves the task unchecked. Parsing is delegated to
    /// [`crate::markers`], which is FAIL-first: a line-initial/terminal
    /// `FAILED` (or any `REPLAN REQUIRED`) outranks a `MISSION COMPLETE` that
    /// is merely embedded in the prose of a failed deliverable.
    ///
    /// Callers that already hold the structured verdict should prefer
    /// [`Plan::check_plan_on_deliverable`], which never re-parses the body.
    ///
    /// The task id is resolved from the marker when present (`task_id_override`
    /// may be `None`); callers that already bound a `task_id` (REQ-ORCH-005
    /// `delegate_task` binding) may pass it explicitly so check-off still works
    /// even if the subagent omitted the parenthesized id. Passing `None` for
    /// both means the id has to be recovered from the deliverable itself, which
    /// is only accepted when the id also occurs there as a boundary-safe
    /// task-id token — see [`Plan::check_plan_on_deliverable`] (H6 hardening).
    ///
    /// `deliverable` must be the real deliverable text; a canned success token
    /// carries no marker and never checks anything off.
    ///
    /// Returns `Ok(true)` when the task was checked off on disk, `Ok(false)`
    /// when the marker was not a completion (leaving the task unchecked) or the
    /// task id was not found / already checked, and `Err` on IO failure.
    pub fn check_plan_on_marker(&self, task_id: Option<&str>, deliverable: &str) -> Result<bool> {
        self.check_plan_on_deliverable(None, task_id, deliverable)
    }

    /// REQ-PLAN-002 + REQ-ORCH-005: delegation-aware auto check-off that
    /// prefers the **structured** [`MissionMarker`] carried by the deliverable
    /// (the `Deliverable.marker` field) over any re-parse of its body — the
    /// minimal fix recorded for bug C1 (`docs/recon_bugs_manager.md`).
    ///
    /// [`MissionMarker::resolve`] uses `marker` verbatim when populated and only
    /// falls back to the positional body parse when it is `None`. A `FAILED` /
    /// `REPLAN REQUIRED` verdict therefore always leaves `- [ ] [t-xxx]`
    /// untouched, no matter what marker text the body happens to quote.
    ///
    /// **Hardened task-id resolution (H6).** The id may only come from an
    /// explicit, structured source, tried in this order: the caller's bound
    /// `task_id` (the `delegate_task` argument, normalized by
    /// [`crate::task_id`]), then the id inside a caller-supplied structured
    /// marker, then the id decorated onto the marker (`MISSION COMPLETE (t-001)`).
    /// This entry point never scans an arbitrary blob (tool arguments, plan
    /// text, deliverable prose) for a `t-…` token — the deleted
    /// `manager/loop.rs::extract_task_id` fallback must not be re-created here.
    /// When the id is neither bound by the caller nor carried by a structured
    /// marker it was produced by parsing the deliverable, so it is additionally
    /// required to occur in that deliverable as a boundary-safe task-id token
    /// ([`deliverable_mentions_task_id`]): an id that exists only as a
    /// substring inside a word or a path (`t-notes` inside
    /// `src/chart-notes.md`) can never flip a plan line.
    ///
    /// The `deliverable` argument must be the **real deliverable content** as
    /// returned by the subagent (`Deliverable::content`); a canned verdict token
    /// (`"ok"`, `"success"`, `"done"`) carries no marker and is therefore never
    /// accepted as evidence.
    ///
    /// **A bound id and a marker id must agree (t-035a).** When the caller binds
    /// an id *and* the resolved marker carries one of its own, a mismatch aborts
    /// the check-off instead of quietly preferring either side: a deliverable
    /// that completed `t-002` must never tick `- [ ] [t-001]`. The box stays
    /// pending, which costs at most one extra round; a wrong tick loses work.
    ///
    /// Returns `Ok(true)` when the task was checked off on disk, `Ok(false)`
    /// when the resolved marker was not a completion, the task id could not be
    /// resolved, a body-derived id failed the token proof, or a bound id
    /// disagrees with the marker's own id, and `Err` on IO failure.
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
            // t-035a: a completion marker that names a **different** plan task
            // than the id the caller bound is a contradiction, not a tie-break
            // candidate. Ticking the bound box on the strength of another
            // task's completion is precisely the corruption this entry point
            // exists to prevent, so the check-off is refused fail-closed: the
            // box stays pending and can be re-delegated.
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
/// Used by [`Plan::check_plan_on_deliverable`] as the last line of defence
/// against the H6 failure mode (`docs/recon_bugs_manager.md` H6): an id
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
