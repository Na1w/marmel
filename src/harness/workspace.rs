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

/// Plan file name inside the marmel directory (reused from `agent::phase`).
pub const PLAN_FILE: &str = crate::agent::phase::PLAN_FILE;
/// Session log file name inside the marmel directory.
pub const LOG_FILE: &str = "marmel.log";
/// Phase-override file name inside the marmel directory (reused from `agent::phase`).
pub const FORCED_PHASE_FILE: &str = crate::agent::phase::FORCED_PHASE_FILE;
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
        Self::at(crate::agent::phase::MARMEL_DIR)
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

    /// Create an isolated, unique temp workspace directory.
    fn temp_workspace() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "marmel_workspace_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// `Workspace::at` + `ensure_writable` creates the directory and validates
    /// write access; canonical paths resolve inside it.
    #[test]
    fn test_workspace_creates_and_validates() {
        let dir = temp_workspace();
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

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `Workspace::new()` defaults to the canonical `.marmel` directory and
    /// validates writability.
    #[test]
    fn test_workspace_new_default() {
        let ws = Workspace::new().unwrap();
        assert_eq!(ws.root(), Path::new(crate::agent::phase::MARMEL_DIR));
        assert!(ws.root().is_dir());
        assert_eq!(ws.plan_path(), ws.root().join(PLAN_FILE));
        assert_eq!(ws.log_path(), ws.root().join(LOG_FILE));
    }
}
