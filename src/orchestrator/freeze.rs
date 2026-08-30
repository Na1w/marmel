//! Deep-Freeze crash recovery (SPEC §3.4 / REQ-ORCH-003 persistence).
//!
//! Subagent state is persisted ONLY for crash-recovery purposes. When a
//! delegation starts, the Manager snapshots the `worker_id`, `agent_name`, and
//! the in-flight `DelegationRequest` into a file-based **Crash Journal**. If
//! the process crashes mid-delegation, the frozen record survives on disk so a
//! later (re)started Manager can rehydrate the frozen subagent with the
//! *identical* `worker_id` to resume it cleanly or fail the task properly.
//!
//! Files (SPEC §3.4), both under the shared
//! `.marmel/` workspace:
//! - `.session_frozen.json` — the currently in-flight (frozen) snapshot.
//! - `.session_journal.json` — an append-only log of freeze/recover events.
//!
//! Rehydration is the SOLE exception to cognitive isolation, and it is scoped
//! strictly to the frozen session: the rehydrated worker receives exactly the
//! preserved in-flight `sub_req` and nothing else.

use crate::agents::{Agent, DelegationRequest};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The Deep-Freeze checkpoint holding the currently in-flight snapshot.
pub const FROZEN_STATE_FILE: &str = ".session_frozen.json";
/// The append-only Crash Journal log.
pub const CRASH_JOURNAL_FILE: &str = ".session_journal.json";

/// A durable snapshot of one in-flight delegation (SPEC §3.4). Persisted to
/// disk the moment a delegation starts so a crash can be recovered later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FreezeSnapshot {
    /// Stable identity of the frozen worker. Rehydration MUST reuse this exact
    /// id so the resumed subagent is the *same* logical worker.
    pub worker_id: String,
    /// The specialist role being executed (`agent_name`).
    pub agent_name: Agent,
    /// The in-flight delegation request that was mid-execution.
    pub sub_req: DelegationRequest,
}

/// Kind of a Crash Journal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalEventKind {
    /// A delegation started and is now frozen on disk.
    Frozen,
    /// The frozen delegation completed (resolved) cleanly.
    Resolved,
    /// The frozen delegation could not be resumed and was failed.
    Failed,
}

/// One append-only Crash Journal record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalEvent {
    pub ts: String,
    pub kind: JournalEventKind,
    pub worker_id: String,
    pub agent: Agent,
    pub task_id: Option<String>,
}

/// File-backed Crash Journal. No shared mutable state — every operation
/// (re)reads/writes the on-disk files, so it is safe to share `&self` across
/// the async delegation path.
#[derive(Debug, Clone)]
pub struct CrashJournal {
    dir: PathBuf,
}

impl CrashJournal {
    /// A journal rooted at `dir` (typically the Manager's `.marmel/` plan dir).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The shared marmel directory the journal lives in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The in-flight frozen-state checkpoint path.
    pub fn frozen_path(&self) -> PathBuf {
        self.dir.join(FROZEN_STATE_FILE)
    }

    /// The append-only Crash Journal path.
    pub fn journal_path(&self) -> PathBuf {
        self.dir.join(CRASH_JOURNAL_FILE)
    }

    /// REQ-ORCH-003 (persistence): snapshot an in-flight delegation before the
    /// worker runs. Generates a fresh `worker_id`, writes `.session_frozen.json`
    /// and appends a `frozen` journal record. Returns the worker_id.
    pub fn snapshot(&self, agent: Agent, req: &DelegationRequest) -> anyhow::Result<String> {
        let worker_id = Uuid::new_v4().to_string();
        self.write_frozen(FreezeSnapshot {
            worker_id: worker_id.clone(),
            agent_name: agent,
            sub_req: req.clone(),
        })?;
        self.append(JournalEvent {
            ts: Self::now(),
            kind: JournalEventKind::Frozen,
            worker_id: worker_id.clone(),
            agent,
            task_id: req.task_id.clone(),
        })?;
        Ok(worker_id)
    }

    /// REQ-ORCH-003 (persistence): the current pending (frozen) snapshot, or
    /// `None` when no delegation is frozen. This is the rehydration source.
    pub fn frozen(&self) -> anyhow::Result<Option<FreezeSnapshot>> {
        let path = self.frozen_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)?;
        if raw.trim().is_empty() {
            return Ok(None);
        }
        let snap: FreezeSnapshot = serde_json::from_str(&raw)?;
        Ok(Some(snap))
    }

    /// Clear the frozen checkpoint once a delegation terminates (cleanly or
    /// after a failed resume). Only clears when the frozen checkpoint actually
    /// belongs to `worker_id` (prevents stomping a *different* in-flight
    /// freeze). Appends a `Resolved`/`Failed` journal event preserving the
    /// frozen delegation's agent and task identity for audit.
    pub fn clear(&self, worker_id: &str, resolved: bool) -> anyhow::Result<()> {
        let kind = if resolved {
            JournalEventKind::Resolved
        } else {
            JournalEventKind::Failed
        };
        // Capture the frozen record's agent + task before removing it so the
        // journal event is accurate.
        let (agent, task_id) = match self.frozen()? {
            Some(snap) if snap.worker_id == worker_id => {
                let agent = snap.agent_name;
                let task_id = snap.sub_req.task_id;
                let _ = std::fs::remove_file(self.frozen_path());
                (agent, task_id)
            }
            _ => (Agent::Coder, None), // no-op guard: nothing owned by us frozen
        };
        self.append(JournalEvent {
            ts: Self::now(),
            kind,
            worker_id: worker_id.to_string(),
            agent,
            task_id,
        })?;
        Ok(())
    }

    /// The pending Crash Journal entries (audit/recovery diagnostics).
    pub fn journal(&self) -> anyhow::Result<Vec<JournalEvent>> {
        let path = self.journal_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&path)?;
        Ok(raw
            .lines()
            .filter_map(|l| serde_json::from_str::<JournalEvent>(l).ok())
            .collect())
    }

    /// Whether a frozen snapshot exists (used by the recovery bootstrap).
    pub fn is_frozen(&self) -> bool {
        self.frozen().map(|o| o.is_some()).unwrap_or(false)
    }

    /// Current UTC timestamp (RFC 3339) for journal records.
    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn write_frozen(&self, snap: FreezeSnapshot) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_string_pretty(&snap)?;
        std::fs::write(self.frozen_path(), json)?;
        Ok(())
    }

    fn append(&self, event: JournalEvent) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let line = serde_json::to_string(&event)?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path())?;
        writeln!(f, "{line}")?;
        Ok(())
    }
}

impl Default for CrashJournal {
    fn default() -> Self {
        Self::new(crate::agent::phase::MARMEL_DIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Agent;

    fn sample_req() -> DelegationRequest {
        DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "Implement the widget parser.".to_string(),
            snippets: vec![],
            task_id: Some("t-101".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        }
    }

    #[test]
    fn test_freeze_snapshot_roundtrip_and_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let j = CrashJournal::new(tmp.path());

        // Nothing frozen initially.
        assert!(!j.is_frozen());
        assert!(j.frozen().unwrap().is_none());

        // Freeze a delegation; a fresh worker_id is minted.
        let wid = j.snapshot(Agent::Coder, &sample_req()).unwrap();
        assert!(!wid.is_empty());
        assert!(j.is_frozen());

        // The frozen snapshot round-trips with identical worker_id + request.
        let snap = j.frozen().unwrap().expect("frozen snapshot present");
        assert_eq!(snap.worker_id, wid);
        assert_eq!(snap.agent_name, Agent::Coder);
        assert_eq!(snap.sub_req.task_id.as_deref(), Some("t-101"));
        assert_eq!(snap.sub_req.prompt, "Implement the widget parser.");

        // The journal log carries a Frozen record.
        let log = j.journal().unwrap();
        assert!(
            log.iter()
                .any(|e| e.kind == JournalEventKind::Frozen && e.worker_id == wid)
        );

        // Clear marks it resolved and removes the frozen checkpoint.
        j.clear(&wid, true).unwrap();
        assert!(!j.is_frozen());
        let log = j.journal().unwrap();
        assert!(
            log.iter()
                .any(|e| e.kind == JournalEventKind::Resolved && e.worker_id == wid)
        );
    }

    #[test]
    fn test_freeze_clear_ignores_foreign_worker() {
        let tmp = tempfile::tempdir().unwrap();
        let j = CrashJournal::new(tmp.path());
        let wid = j.snapshot(Agent::Generalist, &sample_req()).unwrap();
        // A *different* worker_id must not un-freeze the in-flight one.
        j.clear("some-other-worker", true).unwrap();
        assert!(j.is_frozen());
        assert_eq!(j.frozen().unwrap().unwrap().worker_id, wid);
    }

    #[test]
    fn test_freeze_isolated_from_other_instances() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ja = CrashJournal::new(a.path());
        let jb = CrashJournal::new(b.path());
        ja.snapshot(Agent::Researcher, &sample_req()).unwrap();
        // Distinct directories are distinct journals.
        assert!(ja.is_frozen());
        assert!(!jb.is_frozen());
    }
}
