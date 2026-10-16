//! Session persistence core: the single owner of the durable session format.
//! `session/` is the only place in the crate that names a session file, declares
//! a durable `Serialize`/`Deserialize` session type, computes a bundle
//! fingerprint, performs the atomic commit, or loads/repairs/refuses a bundle.
//!
//! * [`format`] — the durable vocabulary (verbatim type/field names)
//! * [`time`] — the `Instant` ↔ wall-clock projection
//! * [`commit`] — `CommitLock` + `write_bundle`: members first, `manifest.json`
//!   last, and that rename IS the commit point
//! * [`restore`] — `load_bundle` on the verify-before-parse ladder, plus
//!   `verify_pins`/`normalize`
//! * [`worker_journal`] — the `WorkerJournal` mirror of each worker's engine
//!   history, the process-level `REGISTRY` harvested at commit time, and the
//!   restore-as-interrupted rehydration of `orchestrator::WORKERS`
//!
//! The `capture` trigger surface is not here. [`manager_capture`] holds the
//! in-memory manager capture; the `ContextEngine` capture/install seams live in
//! `manager::context`, where those rules are that struct's invariants. A stale
//! on-disk journal is named by [`manager_capture::LEGACY_TRANSCRIPT_FILE`] and
//! reported by [`restore`], never rewritten: [`restore`] refuses instead.

pub mod commit;
pub mod format;
pub mod manager_capture;
pub mod restore;
pub mod time;
pub mod worker_journal;

use crate::manager::phase::Plan;

/// Exit code a `--resume` run uses when the bundle exists but was **refused**.
/// `main` returns `Result<()>`, which always exits `1`, so the
/// refusal has to be carried out as an explicit code.
pub const EXIT_RESTORE_REFUSED: i32 = 3;

/// Root of the session bundle for a plan.
///
/// `Plan::session_bundle_path()` is the only path authority: nothing in
/// `session/` re-derives the bundle location from `MARMEL_DIR`, and no caller may
/// hardcode `.marmel/session`.
pub fn bundle_dir(plan: &Plan) -> std::path::PathBuf {
    plan.session_bundle_path()
}

// The whole durable vocabulary is re-exported at the module root so callers
// depend on `session::*` without naming the internal file split.
pub use format::envelope::{
    IdentityFloors, ManagerFile, ManagerSection, Manifest, ManifestMember, PinWitness,
    ProcessEnding, SectionHeader, SectionKind,
};
pub use format::messages::{MessageRecord, ToolCallRecord, ToolFunctionRecord};
pub use format::restore::{
    NonRestorableKind, NonRestorableNotice, PinStage, Refusal, RefusalCode, RepairReport,
    RestoreOutcome, RestoredSession, WorkerRepairCounts,
};
pub use format::ui::{
    BudgetWitness, ComposerState, SubagentPanel, UiFile, UiJournalRecord, UiSection, ViewState,
};
pub use format::worker::{
    DanglingCall, DanglingToolCallState, HistorySource, WorkerCounters, WorkerFile, WorkerSection,
    WorkerStatus,
};
pub use format::{
    ABORTED_TOOL_RESULT, BUNDLE_DIR_NAME, FORMAT_VERSION, MANAGER_FILE, MANIFEST_FILE,
    SCHEMA_VERSION, SESSION_RESTORE_NOTICE_PREFIX, UI_FILE, WORKERS_DIR_NAME, fingerprint,
};
// The worker mirror vocabulary: the handle, the registry
// accessors the committer harvests, and the rehydration entry points.
pub use worker_journal::{
    INTERRUPTED_STATUS_LABEL, MAX_CLOSED_JOURNALS, WorkerJournal, WorkerRestoreReport, WorkerSeed,
    current_journal, install_restored_workers, open as open_worker_journal, registry_get,
    registry_keys, registry_len, registry_snapshot, restore_notice, restore_notice_message,
    retire_interrupted_worker, unregister,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// `Plan::session_bundle_path()` is the single path authority, and
    /// `session::bundle_dir` is a thin alias for it — not a second derivation.
    #[test]
    fn bundle_dir_defers_to_the_plan_as_the_single_path_authority() {
        let plan = Plan::at("/tmp/marmel-t007-plan");
        assert_eq!(bundle_dir(&plan), plan.session_bundle_path());
        assert_eq!(bundle_dir(&plan), plan.dir().join(BUNDLE_DIR_NAME));
        assert_eq!(
            bundle_dir(&plan)
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some("marmel-t007-plan"),
            "the bundle hangs off the plan directory, never a re-derived constant"
        );
        // The default plan is the real one: `.marmel/session`, i.e. a sibling of
        // the legacy journals.
        let default_plan = Plan::default();
        assert_eq!(
            bundle_dir(&default_plan)
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some(".marmel")
        );
    }

    /// The two constants other tasks bind to: exit code 3 for a refused resume
    /// and the notice prefix that makes a restored system line
    /// distinguishable from a real instruction.
    #[test]
    fn the_module_contract_constants_are_stable() {
        assert_eq!(EXIT_RESTORE_REFUSED, 3);
        assert_eq!(SESSION_RESTORE_NOTICE_PREFIX, "(SYSTEM: SESSION RESTORED");
        assert_eq!(FORMAT_VERSION, 1);
        assert_eq!(SCHEMA_VERSION, 1);
    }
}
