//! Centralized handling of the `.marmel` workspace directory.
//!
//! CODE_REVIEW #4 (Beroende av `.marmel` katalogens existens): previously the
//! `.marmel` directory was created ad-hoc via scattered
//! `std::fs::create_dir_all(".marmel")` calls, risking race conditions and
//! confusing permissions errors on a read-only filesystem. This module owns the
//! workspace directory: [`Workspace::new`] creates it and validates write
//! permissions with a probe file exactly once at boot, and every canonical path
//! (execution plan, session log, forced-phase override, archive) resolves
//! through this single struct.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Plan file name inside the marmel directory (reused from `manager::phase`).
pub const PLAN_FILE: &str = crate::manager::phase::PLAN_FILE;
/// Session log file name inside the marmel directory.
pub const LOG_FILE: &str = "marmel.log";
/// Phase-override file name inside the marmel directory (reused from `manager::phase`).
pub const FORCED_PHASE_FILE: &str = crate::manager::phase::FORCED_PHASE_FILE;
/// Archive subdirectory name inside the marmel directory.
pub const ARCHIVE_DIR: &str = "archive";

/// Centralized owner of the `.marmel` workspace directory.
///
/// Constructing a [`Workspace`] via [`Workspace::new`] creates the directory
/// (if missing) and verifies it is writable by writing and removing a probe
/// file, so a read-only filesystem fails fast at boot instead of surfacing
/// confusing errors later. The directory is configurable so tests can isolate
/// against a temp dir, but defaults to `./.marmel` for normal operation.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::at(crate::manager::phase::MARMEL_DIR)
    }
}

impl Workspace {
    /// Create a workspace rooted at `dir` (defaults to `./.marmel`).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { root: dir.into() }
    }

    /// Boot-time constructor: create the workspace directory and validate that
    /// it is writable.
    ///
    /// This creates the directory (if missing) and verifies write access by
    /// writing and removing a probe file. Returns an error when the directory
    /// cannot be created or is not writable.
    pub fn new() -> Result<Self> {
        let ws = Self::default();
        ws.ensure_writable()?;
        Ok(ws)
    }

    /// The workspace root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical path of the execution plan file.
    pub fn plan_path(&self) -> PathBuf {
        self.root.join(PLAN_FILE)
    }

    /// Canonical path of the session log file.
    pub fn log_path(&self) -> PathBuf {
        self.root.join(LOG_FILE)
    }

    /// Canonical path of the forced-phase override file.
    pub fn forced_phase_path(&self) -> PathBuf {
        self.root.join(FORCED_PHASE_FILE)
    }

    /// Canonical path of the archive subdirectory.
    pub fn archive_dir(&self) -> PathBuf {
        self.root.join(ARCHIVE_DIR)
    }

    /// Canonical path of the synthesized prompts directory (`.marmel/prompts/`).
    pub fn prompts_dir(&self) -> PathBuf {
        self.root.join("prompts")
    }

    /// Path to a specific synthesized prompt markdown file
    /// (`.marmel/prompts/<task_id>.md`).
    ///
    /// Gate t-046: the task id is validated as a single path segment before it is
    /// joined onto the prompts directory, so an id such as `../../etc/x`, `a/b`
    /// or `..\\..\\x` is reported as an error instead of silently becoming a path
    /// outside that directory. The id is never sanitized, trimmed or clamped —
    /// callers must propagate [`crate::task_id::TaskIdError`].
    pub fn prompt_path_for_task(
        &self,
        task_id: &str,
    ) -> std::result::Result<PathBuf, crate::task_id::TaskIdError> {
        let clean = crate::task_id::normalize_task_id_ref(task_id);
        let id = crate::task_id::validate_task_id(clean)?;
        Ok(self.prompts_dir().join(format!("{id}.md")))
    }

    /// Create the workspace directory and validate it is writable by writing
    /// and removing a probe file.
    pub fn ensure_writable(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating workspace dir {}", self.root.display()))?;
        let probe = self
            .root
            .join(format!(".marmel_probe_{}", std::process::id()));
        std::fs::write(&probe, "marmel-write-probe")
            .with_context(|| format!("workspace {} is not writable", self.root.display()))?;
        std::fs::remove_file(&probe)
            .with_context(|| format!("cleaning up probe file {}", probe.display()))?;
        Ok(())
    }
}

/// Helper to construct numbered backup paths: `marmel.log.1`, `marmel.log.2`, etc.
pub fn backup_path(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Rotate a log file if its size exceeds `max_bytes` (or unconditionally if `max_bytes == 0`).
pub fn rotate_log_file(path: &Path, max_bytes: u64, backups: u32) {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if max_bytes > 0 && size <= max_bytes {
        return;
    }

    for i in (1..backups).rev() {
        let src = backup_path(path, i);
        let dst = backup_path(path, i + 1);
        let _ = std::fs::rename(&src, &dst);
    }

    let _ = std::fs::rename(path, backup_path(path, 1));

    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Workspace::at` + `ensure_writable` creates the directory and validates
    /// write access; canonical paths resolve inside it.
    #[test]
    fn test_workspace_creates_and_validates() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("isolated_workspace");
        assert!(!dir.exists());
        let ws = Workspace::at(&dir);
        ws.ensure_writable().unwrap();
        assert!(dir.is_dir(), "workspace dir must be created");

        assert_eq!(ws.plan_path(), dir.join(PLAN_FILE));
        assert_eq!(ws.log_path(), dir.join(LOG_FILE));
        assert_eq!(ws.forced_phase_path(), dir.join(FORCED_PHASE_FILE));
        assert_eq!(ws.archive_dir(), dir.join(ARCHIVE_DIR));

        // No probe file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            leftovers.is_empty(),
            "probe must be cleaned up: {leftovers:?}"
        );
    }

    /// `Workspace::new()` defaults to the canonical `.marmel` directory and
    /// validates writability.
    #[test]
    fn test_workspace_new_default() {
        let ws = Workspace::new().unwrap();
        assert_eq!(ws.root(), Path::new(crate::manager::phase::MARMEL_DIR));
        assert!(ws.root().is_dir());
        assert_eq!(ws.plan_path(), ws.root().join(PLAN_FILE));
        assert_eq!(ws.log_path(), ws.root().join(LOG_FILE));
    }

    /// Gate t-046: an accepted task id resolves to a single file name *inside*
    /// the prompts directory — decoration is normalized, the rest is verbatim.
    #[test]
    fn prompt_path_for_task_resolves_inside_the_prompts_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("ws_prompt_paths");
        let ws = Workspace::at(&root);
        ws.ensure_writable().expect("workspace must be writable");

        for (raw, file_name) in [
            ("t-001", "t-001.md"),
            ("  [t-001]  ", "t-001.md"),
            ("task-t-046", "task-t-046.md"),
            ("t_val_01", "t_val_01.md"),
        ] {
            let path = ws
                .prompt_path_for_task(raw)
                .unwrap_or_else(|err| panic!("{raw:?} must be accepted: {err}"));
            assert_eq!(path, ws.prompts_dir().join(file_name), "for {raw:?}");
            let relative = path.strip_prefix(&root).unwrap_or_else(|_| {
                panic!("{raw:?} escaped the workspace root: {}", path.display())
            });
            assert_eq!(relative, Path::new("prompts").join(file_name));
        }
    }

    /// Gate t-046: a hostile task id is refused outright. The call reports the
    /// typed rejection and leaves nothing behind — no file in the workspace, and
    /// nothing at the location the unguarded join would have reached.
    #[test]
    fn prompt_path_for_task_rejects_hostile_ids_and_creates_no_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("ws_hostile_ids");
        let ws = Workspace::at(&root);
        ws.ensure_writable().expect("workspace must be writable");

        for hostile in [
            "../../etc/x",
            "../../escape",
            "a/b",
            "..\\..\\escape",
            "..",
            ".",
            ".hidden",
            "t-001/extra",
            "t 001",
            "",
            "   ",
        ] {
            let err = ws
                .prompt_path_for_task(hostile)
                .err()
                .unwrap_or_else(|| panic!("{hostile:?} must be refused"));
            assert!(
                !err.to_string().is_empty(),
                "rejection of {hostile:?} must carry a reason"
            );
        }

        // Nothing was created anywhere in the workspace.
        let entries: Vec<String> = std::fs::read_dir(&root)
            .expect("read workspace")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert!(entries.is_empty(), "no file may be created: {entries:?}");

        // Control: the ungated join injects parent-directory components, i.e. it
        // leaves the workspace root as soon as the path is resolved (it would
        // land in the temp directory that holds this workspace).
        let would_be = root.join("prompts").join("../../escape.md");
        assert!(
            would_be
                .components()
                .any(|c| c == std::path::Component::ParentDir),
            "control: the ungated join must carry a '..' component: {would_be:?}"
        );
        let resolved_escape = temp.path().join("escape.md");
        assert!(
            !resolved_escape.exists(),
            "the escape target {resolved_escape:?} must not exist"
        );
    }
}
