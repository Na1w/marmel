//! Mission phase state machine, disk plan management, and auto check-off.
//!
//! REQ-PLAN-001 (Mission Phase States): A mission lives in one of two phases:
//! `Conversational` (read-only tools) or `Executing` (full toolset, unlocked once
//! a plan exists on disk and has been approved).
//!
//! REQ-PLAN-002 (Disk-Driven Automatic Plan Check-off): The plan lives at
//! `.marmel/execution_plan.md` and is formatted with `- [ ] [t-xxx]` checkboxes.
//! When a tool executes successfully (output lacks `ERROR`, `FAILED`, or
//! `REPLAN REQUIRED`), the harness toggles the matching `- [ ] [t-xxx]` to
//! `- [x] [t-xxx]` on disk.
//!
//! REQ-PLAN-002 + REQ-ORCH-005 (Delegation-aware check-off): a *subagent*
//! deliverable is checked off via its terminal marker, not free-form output.
//! Only a `MISSION COMPLETE (t-xxx)` marker flips `- [ ] [t-xxx]` to
//! `- [x] [t-xxx]`; `FAILED` / `REPLAN REQUIRED` markers leave the task
//! unchecked. `MissionMarker::parse` mirrors the specialist markers and
//! `Plan::check_plan_on_marker` applies them to the plan.
//!
//! REQ-PLAN-003 (Silent Dispatcher Enforcement): In `Executing` phase the agent
//! suppresses conversational filler and iterates strictly through unchecked plan
//! items until all tasks are marked `[x]`.
//!
//! REQ-PLAN-004 (Disk Override): If `.marmel/forced_phase.txt` exists containing
//! `Conversational` or `Executing`, it overrides the internal phase calculation
//! on every turn.

use anyhow::{Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Directory (relative to the workspace) holding the plan and phase override.
pub const MARMEL_DIR: &str = ".marmel";
/// Plan file name inside the marmel directory.
pub const PLAN_FILE: &str = "execution_plan.md";
/// Phase-override file name inside the marmel directory.
pub const FORCED_PHASE_FILE: &str = "forced_phase.txt";

/// High-level mission phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionPhase {
    /// Read-only tools only; modifying tools require a plan first.
    Conversational,
    /// Full toolset unlocked; plan exists on disk.
    Executing,
}

impl MissionPhase {
    /// Parse a phase from the text content of `forced_phase.txt`.
    pub fn parse(text: &str) -> Option<MissionPhase> {
        match text.trim().to_ascii_uppercase().as_str() {
            "CONVERSATIONAL" => Some(MissionPhase::Conversational),
            "EXECUTING" => Some(MissionPhase::Executing),
            _ => None,
        }
    }
}

/// A regex matching a task line in the `- [ ] [t-xxx]` format (with optional indentation or bolding).
///
/// The capture group holds the task id (e.g. `t-001`).
fn task_line_re() -> Regex {
    Regex::new(r"(?mi)^\s*[-*]\s*\[\s*\]\s*\*{0,2}\[?(t-[A-Za-z0-9_-]+)\]?\*{0,2}")
        .expect("valid task regex")
}

/// Returns `true` when a tool's output indicates success, i.e. it does not
/// contain the error markers `ERROR`, `FAILED`, or `REPLAN REQUIRED`
/// (REQ-PLAN-002). Comparison is case-insensitive.
pub fn output_is_success(output: &str) -> bool {
    let upper = output.to_ascii_uppercase();
    !upper.contains("ERROR") && !upper.contains("FAILED") && !upper.contains("REPLAN REQUIRED")
}

/// Mutex ensuring atomic filesystem operations across parallel subagents on the execution plan.
static PLAN_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

/// Regex matching a `(t-xxx)` / `[t-xxx]` task-id token. Compiled exactly once
/// via `OnceLock` (CODE_REVIEW Point 2).
static TASK_ID_RE: OnceLock<Regex> = OnceLock::new();

/// Extract the first `(t-xxx)` task-id token from text. Used to bind a
/// `MISSION COMPLETE (t-xxx)` marker to its plan line (REQ-ORCH-005). The
/// regex is intentionally self-contained so this module carries no dependency
/// on the agents module.
fn find_task_id(text: &str) -> Option<String> {
    let re = TASK_ID_RE.get_or_init(|| {
        Regex::new(r"\(?\[?(t-[A-Za-z0-9_-]+)\]?\)?").expect("valid task-id regex")
    });
    re.captures(text)
        .map(|c| c.get(1).unwrap().as_str().to_string())
}

/// The terminal marker a subagent appends to its deliverable
/// (REQ-ORCH-005): `MISSION COMPLETE (task-id)` on success, or `FAILED` /
/// `REPLAN REQUIRED` with a reason when it cannot.
///
/// This is a lightweight, plan-side view of the same marker emitted by the
/// specialist layer, kept local so `phase.rs` stays decoupled from the agents
/// module while still satisfying REQ-ORCH-005 return semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionMarker {
    /// Task fully satisfied. `task_id` matches the plan line to auto-check.
    Complete { task_id: Option<String> },
    /// Task could not be completed; report reason + partial result.
    Failed { reason: String },
    /// Task could not be completed AND the plan/goal needs revisiting.
    Replan { reason: String },
}

impl MissionMarker {
    /// Parse the terminal marker out of a subagent's final text
    /// (REQ-ORCH-005). Matched case-insensitively anywhere in the deliverable.
    /// `REPLAN REQUIRED` MUST be matched before the generic `FAILED` check so
    /// a `REPLAN REQUIRED ... FAILED` string classifies as a replan, exactly as
    /// the specialist markers do.
    pub fn parse(text: &str) -> Option<MissionMarker> {
        let upper = text.to_ascii_uppercase();
        if upper.contains("REPLAN REQUIRED") {
            return Some(MissionMarker::Replan {
                reason: text.to_string(),
            });
        }
        if upper.contains("MISSION COMPLETE") {
            return Some(MissionMarker::Complete {
                task_id: find_task_id(text),
            });
        }
        if upper.contains("FAILED") {
            return Some(MissionMarker::Failed {
                reason: text.to_string(),
            });
        }
        None
    }

    /// Returns `true` when the marker is a successful completion, i.e. it
    /// carries `MISSION COMPLETE` (REQ-ORCH-005). Only this marker flips a
    /// plan line to `[x]`; `FAILED` / `REPLAN REQUIRED` never do.
    pub fn is_complete(&self) -> bool {
        matches!(self, MissionMarker::Complete { .. })
    }
}

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

    /// The absolute path of the forced-phase override file.
    pub fn forced_phase_path(&self) -> PathBuf {
        self.dir.join(FORCED_PHASE_FILE)
    }

    /// REQ-PLAN-001: write the initial execution plan to `.marmel/execution_plan.md`,
    /// creating the directory if it does not exist.
    pub fn create(&self, plan_markdown: &str) -> Result<()> {
        let _guard = PLAN_MUTEX.lock().unwrap();
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;

        let path = self.plan_path();
        std::fs::write(&path, plan_markdown)
            .with_context(|| format!("writing {}", path.display()))?;
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
    /// `pending_tasks()`, `all_tasks()`, and `determine_phase()` must never read
    /// it, otherwise an archived plan would resurrect the `Executing` phase.
    pub fn read(&self) -> Result<Option<String>> {
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
        let _guard = PLAN_MUTEX.lock().unwrap();
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
        tracing::warn!("Execution plan CLEARED from disk (all plan files deleted).");
        Ok(())
    }

    /// Parse all *unchecked* task ids (`- [ ] [t-xxx]`) from the plan.
    pub fn pending_tasks(&self) -> Vec<String> {
        match self.read() {
            Ok(Some(content)) => parse_unchecked_tasks(&content),
            _ => Vec::new(),
        }
    }

    /// Parse *all* task ids present in the plan (both `- [ ]` and `- [x]`).
    pub fn all_tasks(&self) -> Vec<String> {
        let re = Regex::new(
            r"(?mi)^\s*[-*]\s*\[\s*[ xX]?\s*\]\s*\*{0,2}\[?(t-[A-Za-z0-9_-]+)\]?\*{0,2}",
        )
        .expect("valid task regex");
        match self.read() {
            Ok(Some(content)) => re
                .captures_iter(&content)
                .map(|c| c[1].to_string())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Returns `true` when the plan has at least one task and none remain unchecked.
    pub fn is_complete(&self) -> bool {
        let Some(content) = self.read().ok().flatten() else {
            return false;
        };
        // If there are ANY unchecked checkboxes (`[ ]` or `( )`) anywhere in the plan, it is NOT complete!
        let re_unchecked = Regex::new(r"\[\s*\]|\(\s*\)").expect("unchecked box regex");
        if re_unchecked.is_match(&content) {
            return false;
        }
        // Must contain at least one completed checkbox ([x] or (x))
        let re_checked = Regex::new(r"\[[xX]\]|\([xX]\)").expect("checked box regex");
        re_checked.is_match(&content)
    }

    /// Archive the current execution plan to `.marmel/archive/execution_plan_<timestamp>.md`
    /// and `.marmel/execution_plan_archive.md`, and clean up `.marmel/execution_plan.md`.
    ///
    /// t-203 (a): a plan is only archived once it is complete. An incomplete plan
    /// (one with at least one unchecked box) is the working checkpoint — archiving
    /// it would silently lose it, so this returns `Ok(None)` when `is_complete()`
    /// is false and leaves every file untouched.
    pub fn archive(&self) -> Result<Option<PathBuf>> {
        let _guard = PLAN_MUTEX.lock().unwrap();
        if !self.is_complete() {
            tracing::warn!(
                "Plan archive skipped: plan is incomplete (contains pending unchecked tasks)."
            );
            return Ok(None);
        }
        let path = self.plan_path();
        if !path.exists() {
            tracing::warn!(
                "Plan archive skipped: plan file {} does not exist.",
                path.display()
            );
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let archive_dir = self.dir.join("archive");
        std::fs::create_dir_all(&archive_dir)
            .with_context(|| format!("creating {}", archive_dir.display()))?;
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let dest = archive_dir.join(format!("execution_plan_{ts}.md"));
        std::fs::write(&dest, &content).with_context(|| format!("writing {}", dest.display()))?;
        let latest = self.dir.join("execution_plan_archive.md");
        let _ = std::fs::write(&latest, &content);
        let _ = std::fs::remove_file(&path);
        tracing::warn!(
            "Execution plan completed and ARCHIVED to {} (active plan file {} removed from disk)",
            dest.display(),
            path.display()
        );
        Ok(Some(dest))
    }

    /// REQ-PLAN-002: toggle a single `- [ ] [t-id]` task to `- [x] [t-id]` on disk.
    ///
    /// When the final task is checked off and the plan becomes complete, a snapshot
    /// is automatically archived to `.marmel/archive/`.
    ///
    /// Returns `Ok(true)` if a pending checkbox was flipped, `Ok(false)` if the
    /// task id was not found (or was already checked), and `Err` on IO failure.
    pub fn check_off(&self, task_id: &str) -> Result<bool> {
        let _guard = PLAN_MUTEX.lock().unwrap();
        let Some(content) = self.read()? else {
            tracing::warn!("check_off({task_id}): no active plan file on disk");
            return Ok(false);
        };
        let tid_lower = task_id.to_ascii_lowercase();
        let mut flipped = false;
        let re_box = Regex::new(r"\[\s*\]").expect("box regex");
        let updated = content
            .lines()
            .map(|line| {
                if !flipped {
                    let line_lower = line.to_ascii_lowercase();
                    if line_lower.contains(&tid_lower) && re_box.is_match(line) {
                        flipped = true;
                        return re_box.replace(line, "[x]").to_string();
                    }
                }
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        if flipped {
            std::fs::write(self.plan_path(), &updated)
                .with_context(|| format!("updating {}", self.plan_path().display()))?;
            let complete = self.is_complete();
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
                let _ = self.write_completed_snapshot(&updated);
            }
        } else {
            tracing::warn!("check_off({task_id}): task id not found or already checked");
        }
        Ok(flipped)
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

    /// REQ-PLAN-002: auto check-off helper. If `output` indicates success
    /// (no `ERROR`/`FAILED`/`REPLAN REQUIRED`), check off `task_id` on disk.
    ///
    /// Returns `Ok(true)` when the task was checked off, `Ok(false)` when the
    /// output was an error (leaving the task unchecked) or the task was not found.
    pub fn check_off_on_success(&self, task_id: &str, output: &str) -> Result<bool> {
        if !output_is_success(output) {
            return Ok(false);
        }
        self.check_off(task_id)
    }

    /// REQ-PLAN-002 + REQ-ORCH-005: delegation-aware auto check-off for a
    /// subagent deliverable.
    ///
    /// When the deliverable carries a `MISSION COMPLETE (t-xxx)` terminal
    /// marker, the matching `- [ ] [t-xxx]` plan line is flipped to
    /// `- [x] [t-xxx]` on disk. A `FAILED` / `REPLAN REQUIRED` marker (or any
    /// unrecognized output) leaves the task unchecked.
    ///
    /// The task id is resolved from the marker when present (`task_id_override`
    /// may be `None`); callers that already bound a `task_id` (REQ-ORCH-005
    /// `delegate_task` binding) may pass it explicitly so check-off still works
    /// even if the subagent omitted the parenthesized id.
    ///
    /// Returns `Ok(true)` when the task was checked off on disk, `Ok(false)`
    /// when the marker was not a completion (leaving the task unchecked) or the
    /// task id was not found / already checked, and `Err` on IO failure.
    pub fn check_plan_on_marker(&self, task_id: Option<&str>, deliverable: &str) -> Result<bool> {
        let Some(marker) = MissionMarker::parse(deliverable) else {
            tracing::warn!(
                "check_plan_on_marker: No terminal marker found in deliverable ({} chars)",
                deliverable.len()
            );
            return Ok(false);
        };
        if !marker.is_complete() {
            tracing::warn!("check_plan_on_marker: Marker is not complete: {marker:?}");
            return Ok(false);
        }
        // Resolve the task id: the explicit override takes precedence, falling
        // back to the marker's own `(t-xxx)` token. Own the id so no borrow is
        // held past the local `marker`.
        let tid = task_id.map(str::to_string).or_else(|| {
            if let MissionMarker::Complete { task_id } = &marker {
                task_id.clone()
            } else {
                None
            }
        });
        let Some(tid) = tid else {
            tracing::warn!("check_plan_on_marker: No task_id resolved from marker {marker:?}");
            return Ok(false);
        };
        self.check_off(&tid)
    }

    /// REQ-PLAN-004: read the forced-phase override, if present and valid.
    pub fn forced_phase(&self) -> Option<MissionPhase> {
        let path = self.forced_phase_path();
        if !path.exists() {
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        MissionPhase::parse(&text)
    }

    /// REQ-PLAN-001/004: compute the current mission phase.
    ///
    /// A disk override (`.marmel/forced_phase.txt`) takes precedence. Otherwise,
    /// `Executing` is returned once a plan exists on disk; `Conversational`
    /// otherwise.
    pub fn determine_phase(&self) -> MissionPhase {
        if let Some(forced) = self.forced_phase() {
            return forced;
        }
        if self.exists() {
            MissionPhase::Executing
        } else {
            MissionPhase::Conversational
        }
    }

    /// REQ-PLAN-003: in `Executing` phase the agent acts as a silent dispatcher —
    /// conversational filler is suppressed and it iterates strictly through
    /// unchecked plan items.
    pub fn is_silent_dispatcher(phase: MissionPhase) -> bool {
        phase == MissionPhase::Executing
    }
}

/// Parse all unchecked (`- [ ] [t-xxx]`) task ids from raw plan markdown.
pub fn parse_unchecked_tasks(markdown: &str) -> Vec<String> {
    let re = task_line_re();
    re.captures_iter(markdown)
        .map(|c| c[1].to_string())
        .collect()
}

#[cfg(test)]
#[path = "phase_tests.rs"]
mod tests;
