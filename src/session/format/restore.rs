//! Loader vocabulary: the outcome of a restore, the machine-readable refusal,
//! the repair report, and the non-restorable notices. `RestoredSession` is
//! defined here too: the verified bundle, inert until an installer runs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::envelope::{IdentityFloors, ManagerSection, Manifest, PinWitness, ProcessEnding};
use super::messages::MessageRecord;
use super::ui::UiSection;
use super::worker::WorkerSection;

/// Result of `session::restore::load_bundle`. **In-memory only** — never
/// serialized: it is the loader's verdict about a bundle, and
/// `ProcessEnding::HardKillInferred` exists precisely because this verdict must
/// not be written back (bundle bytes untouched).
#[derive(Debug)]
pub enum RestoreOutcome {
    /// No `manifest.json` under `Plan::session_bundle_path()`. Not an error. If
    /// legacy journals are present, the caller MUST be told they are not
    /// restorable.
    NoBundle {
        /// The bundle directory that was searched.
        bundle: PathBuf,
        /// Legacy `.session_transcript.json` / `.ui_transcript.json` found next
        /// to the bundle: never merged, never upgraded, named out loud.
        legacy_journals: Vec<PathBuf>,
        /// Member-looking files on disk that the (absent) manifest cannot list.
        orphan_members: Vec<String>,
    },
    /// Fail-closed: the bundle was **not** read into state and **no byte
    /// of it was modified**.
    Refused {
        refusal: Refusal,
        report: RepairReport,
    },
    /// Full restore. `ending` is the *derived* ending (`InProgress` reported as
    /// `HardKillInferred`).
    Restored {
        session: RestoredSession,
        ending: ProcessEnding,
        report: RepairReport,
    },
    /// The single tolerated partial restore: one corrupt worker file. That worker is
    /// present with `WorkerStatus::Unrecoverable`; everything else restored.
    PartiallyRestored {
        session: RestoredSession,
        ending: ProcessEnding,
        unrecoverable_workers: Vec<String>,
        report: RepairReport,
    },
}

/// Machine-readable refusal, so the UI message and the log carry the same
/// numbers a reader would compute by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: RefusalCode,
    pub message: String,
    /// The member the refusal names, when the refusal is member-scoped.
    pub member: Option<String>,
}

impl Refusal {
    pub fn new(code: RefusalCode, message: impl Into<String>) -> Self {
        Refusal {
            code,
            message: message.into(),
            member: None,
        }
    }

    pub fn with_member(mut self, member: impl Into<String>) -> Self {
        self.member = Some(member.into());
        self
    }

    /// The refusal sentence a user is shown — the refusal message is a
    /// first-class deliverable: code, member, the numbers involved, and the
    /// promise that the bundle was not modified).
    pub fn display(&self, bundle: &std::path::Path) -> String {
        match &self.member {
            Some(member) => format!(
                "session restore refused [{code}] {member}: {msg} — the bundle at {bundle} was \
                 NOT modified; it is still on disk unchanged",
                code = self.code.label(),
                msg = self.message,
                bundle = bundle.display(),
            ),
            None => format!(
                "session restore refused [{code}]: {msg} — the bundle at {bundle} was NOT \
                 modified; it is still on disk unchanged",
                code = self.code.label(),
                msg = self.message,
                bundle = bundle.display(),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalCode {
    FormatVersion,
    SchemaVersion,
    UnknownField,
    Json,
    MissingMember,
    ByteLen,
    Fingerprint,
    PinViolation,
    NormalizationUnstable,
    HeaderManifestDisagreement,
    Io,
}

impl RefusalCode {
    /// Stable short label for logs and the UI status line.
    pub fn label(&self) -> &'static str {
        match self {
            RefusalCode::FormatVersion => "formatVersion",
            RefusalCode::SchemaVersion => "schemaVersion",
            RefusalCode::UnknownField => "unknownField",
            RefusalCode::Json => "json",
            RefusalCode::MissingMember => "missingMember",
            RefusalCode::ByteLen => "byteLen",
            RefusalCode::Fingerprint => "fingerprint",
            RefusalCode::PinViolation => "pinViolation",
            RefusalCode::NormalizationUnstable => "normalizationUnstable",
            RefusalCode::HeaderManifestDisagreement => "headerManifestDisagreement",
            RefusalCode::Io => "io",
        }
    }
}

/// What the loader had to do to make the transcript sendable, and everything it
/// could not restore. One object, three destinations: UI status line, journaled
/// UI record, `tracing::warn!` — never a discarded `let _`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairReport {
    pub pins_verified: bool,
    /// Recomputed vs. persisted `ManagerSection::pinned_prefix_end`.
    pub pinned_prefix_end_expected: Option<u32>,
    pub pinned_prefix_end_actual: Option<u32>,
    /// Counts from the normalization passes.
    pub orphan_tool_results_pruned: usize,
    pub aborted_results_synthesized: usize,
    pub tool_results_regrouped: usize,
    /// MUST be `2` on any path that ran normalization, and `normalization_stable`
    /// MUST be `true` — otherwise the outcome is `Refused`.
    pub normalization_passes: u8,
    pub normalization_stable: bool,
    pub manager_messages_before: usize,
    pub manager_messages_after: usize,
    pub ui_lines_restored: usize,
    pub workers_restored: usize,
    pub workers_unrecoverable: Vec<String>,
    /// Completed entries dropped because the live ring holds ≤10.
    pub completed_dropped_by_ring_cap: usize,
    /// Every `NonRestorableNotice` for a capability that was lost.
    pub notices: Vec<NonRestorableNotice>,
    pub warnings: Vec<String>,
    /// Per-worker before/after history counts. The manager-level shape carries
    /// manager counts only, which would make per-worker acceptance criteria
    /// string-matching; the `warnings` sentences the repair contract requires are
    /// still emitted, so both readers are served.
    pub worker_counts: Vec<WorkerRepairCounts>,
}

/// Per-worker normalization bookkeeping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRepairCounts {
    pub key: String,
    /// History length as decoded.
    pub before: usize,
    /// History length after the passes.
    pub after: usize,
    /// Which rules applied, in pass order.
    pub applied_rules: Vec<String>,
}

/// A first-class, user-visible statement that a capability did not survive the
/// restart (emitted by every lossy projection); collected into
/// `RepairReport`, the UI, and the journaled UI record. Not a log line, and
/// never a silent re-mint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NonRestorableNotice {
    pub kind: NonRestorableKind,
    /// What it applies to: a worker key, a task id, or the session.
    pub subject: String,
    /// One English sentence naming the consequence and the expected user action.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NonRestorableKind {
    /// `CancellationToken` — the restored worker cannot be cancelled through the
    /// old token.
    CancellationToken,
    /// `JoinHandle` — no task exists to join or abort.
    JoinHandle,
    /// PTY / process group — OS children may have survived the kill and may
    /// still be mutating the workspace.
    PtyProcess,
    /// RAII guard (`ActiveWorkerGuard`) — its completion/eviction side effects
    /// will not run.
    DropGuard,
    /// Queued-but-undelivered steering notices.
    PendingNotice,
    /// `HarnessStats` intervention counters.
    HarnessStats,
    /// `tool_schema_tokens` — re-priced from live MCP state.
    ToolSchemaCharge,
    /// A worker whose `history` is `SeedOnly`/`NotCaptured`, or `Unrecoverable`.
    WorkerHistory,
    Other,
}

impl NonRestorableNotice {
    /// Deduplicated union key: capture may record
    /// capture-time losses, the loader always adds load-time losses, and the
    /// deduplicated union is what lands in `RepairReport::notices`.
    pub fn dedup_key(&self) -> (NonRestorableKind, String) {
        (self.kind.clone(), self.subject.clone())
    }
}

/// The verified bundle, inert until an installer runs. Nothing in
/// this type has touched live state.
#[derive(Debug, Clone)]
pub struct RestoredSession {
    pub session_id: String,
    pub manager: ManagerSection,
    pub ui: UiSection,
    pub workers: Vec<WorkerSection>,
    pub identity_floors: IdentityFloors,
    /// The verified manifest, so the caller can quote `ending`/`committed_wall`.
    pub manifest: Manifest,
    pub bundle: PathBuf,
}

/// What one normalization pass set did to one transcript; feeds
/// `RepairReport`.
///
/// Every count here is paired with [`AppliedRule`] entries in `applied`: silent
/// repair is banned, so a number without the ids and message indices behind it
/// is not a report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NormalizeReport {
    pub pruned_orphan_ids: Vec<String>,
    pub synthesized_aborted_ids: Vec<String>,
    pub regrouped_ids: Vec<String>,
    /// Duplicate `role:"tool"` results dropped while keeping the first
    /// (`manager::context::repair_tool_call_pairs`). `RepairReport` has no
    /// dedicated duplicate field, so these are counted here and
    /// spelled out in `RepairReport::warnings`.
    pub dropped_duplicate_ids: Vec<String>,
    /// Every rule the pass set applied, in pass order, with the id and the
    /// affected message index.
    pub applied: Vec<AppliedRule>,
    /// What the pairing-witness cross-check found — the recorded witness against
    /// what the passes actually did: reported, never silently fixed.
    pub n6_disagreements: Vec<String>,
    /// Always `2`: the passes run exactly twice.
    pub passes: u8,
    pub stable: bool,
    pub pinned_prefix_end_before: usize,
    pub pinned_prefix_end_after: usize,
    /// History length as decoded / as installed (before/after counts — the
    /// per-worker half of which `RepairReport` carries in `worker_counts`).
    pub messages_before: usize,
    pub messages_after: usize,
}

/// One applied normalization rule: which pass, which
/// `tool_call_id`, and which message it acted on. `index_before` is the index in
/// the vector as decoded, `index_after` the index in the healed one — a
/// rule that added a message has no `index_before`, one that removed a message has
/// no `index_after`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRule {
    pub rule: &'static str,
    pub tool_call_id: String,
    pub index_before: Option<usize>,
    pub index_after: Option<usize>,
}

impl AppliedRule {
    /// The orphan-tool-result prune: `prune_orphan_tool_messages`.
    pub const N1_PRUNE: &'static str = "N1 prune orphan result";
    /// The missing-result half of `repair_tool_call_pairs`.
    pub const N2_SYNTHESIZE: &'static str = "N2 synthesize aborted result";
    /// The move half of `repair_tool_call_pairs`.
    pub const N2_REGROUP: &'static str = "N2 regroup result";
    /// The duplicate-keeping-first half of `repair_tool_call_pairs`.
    pub const N2_DEDUPLICATE: &'static str = "N2 drop duplicate result";
}

/// Which vector the pins are being verified on: the stored
/// transcript, or the normalized one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinStage {
    Stored,
    Installed,
}

impl PinStage {
    pub fn label(&self) -> &'static str {
        match self {
            PinStage::Stored => "stored",
            PinStage::Installed => "installed",
        }
    }
}

/// Convenience: recompute the pin witness from a transcript, exactly the way the
/// capture side does it.
pub fn witness_of(messages: &[MessageRecord]) -> Option<PinWitness> {
    if messages.len() < 2 {
        return None;
    }
    let (system_prompt_len, goal_len) = match (&messages[0], &messages[1]) {
        (MessageRecord::System { content: a }, MessageRecord::User { content: b }) => {
            (a.len() as u32, b.len() as u32)
        }
        _ => return None,
    };
    Some(PinWitness {
        system_prompt_len,
        goal_len,
    })
}
