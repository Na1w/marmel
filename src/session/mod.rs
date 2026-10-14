//! Session persistence core: the single owner of the durable session format.
//!
//! Design A (`docs/design_session_restore_data.md`) §1 D1 makes `session/` the
//! only place in the crate that names a session file, declares a durable
//! `Serialize`/`Deserialize` session type, computes a bundle fingerprint,
//! performs the atomic commit, or loads/repairs/refuses a bundle.
//!
//! This task (t-007) builds the persistence core:
//! * [`format`] — the complete §3 durable vocabulary (verbatim type/field names),
//! * [`time`] — the `Instant` ↔ wall-clock projection of D5,
//! * [`commit`] — `CommitLock` + `write_bundle` (members first, `manifest.json`
//!   LAST = the commit point, A §7.1),
//! * [`restore`] — `load_bundle` walking the L0–L9 verify-before-parse ladder
//!   (A §7.3) and `verify_pins`/`normalize`.
//!
//! * [`worker_journal`] — the t-010 worker mirror: the append-side
//!   `WorkerJournal` that mirrors each worker's engine history (A §5, flow
//!   §4.8), the task-local `CURRENT_WORKER_JOURNAL` handle and the process-level
//!   `REGISTRY` the session thread harvests at commit time, plus the O3
//!   restore-as-interrupted rehydration of `orchestrator::WORKERS`.
//!
//! Deliberately **not** here yet (owned by later tasks, and additive-only by
//! design): boot-time restore wiring and the `capture` trigger surface
//! (t-011/t-012/t-013). What t-009 did add: the `ContextEngine` capture/install
//! seams themselves (in `manager::context`, because the rules are that struct's
//! invariants), the in-memory manager capture holder ([`manager_capture`]), and the
//! deletion of the superseded `.session_transcript.json` journal — its name now
//! lives here as [`manager_capture::LEGACY_TRANSCRIPT_FILE`], because D1 makes this
//! module the only place that names a session file, and [`restore`] still has to
//! name a stale copy in its "legacy journals found, not restorable" warning (A D2).
//! The UI journal went the same way in t-025b: `UiTranscript::save`/`load`, the
//! public file-name constant and the `.ui_transcript.json` writes are deleted, so
//! the bundle is the **sole** persistence owner and both legacy journals are only
//! ever named (in [`restore`]) and swept (in `manager::phase::Plan`'s sweep and the
//! session's `/reset`), never written again. The in-memory `UiTranscript` journal
//! stays live as the chat record and the steering-replay source.

pub mod commit;
pub mod format;
pub mod manager_capture;
pub mod restore;
pub mod time;
pub mod worker_journal;

use crate::manager::phase::Plan;

/// Exit code a `--resume` run uses when the bundle exists but was **refused**
/// (flow §3.5). `main` returns `Result<()>`, which always exits `1`, so the
/// refusal has to be carried out as an explicit code.
pub const EXIT_RESTORE_REFUSED: i32 = 3;

/// Root of the session bundle for a plan (flow §4.2).
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
// The worker mirror vocabulary (t-010, flow §4.8): the handle, the registry
// accessors the committer harvests, and the O3 rehydration entry points.
pub use worker_journal::{
    INTERRUPTED_STATUS_LABEL, MAX_CLOSED_JOURNALS, WorkerJournal, WorkerRestoreReport, WorkerSeed,
    current_journal, install_restored_workers, open as open_worker_journal, registry_get,
    registry_keys, registry_len, registry_snapshot, restore_notice, restore_notice_message,
    retire_interrupted_worker, unregister,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Flow §4.2: `Plan::session_bundle_path()` is the single path authority, and
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
        // the legacy journals that t-009/t-013 delete.
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
    /// (flow §3.5) and the notice prefix that makes a restored system line
    /// distinguishable from a real instruction (flow §2.4).
    #[test]
    fn the_module_contract_constants_are_stable() {
        assert_eq!(EXIT_RESTORE_REFUSED, 3);
        assert_eq!(SESSION_RESTORE_NOTICE_PREFIX, "(SYSTEM: SESSION RESTORED");
        assert_eq!(FORMAT_VERSION, 1);
        assert_eq!(SCHEMA_VERSION, 1);
    }
}
