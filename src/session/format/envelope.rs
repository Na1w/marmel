//! The bundle envelope: manifest, member frames, endings, identity floors, and
//! the manager section (Design A §3.2–§3.5).

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use super::messages::MessageRecord;
use super::worker::DanglingToolCallState;

/// The bundle's root document and **atomicity point** (A §1 D3): written last,
/// and the only file that names the member set. A manifest that parses and whose
/// members all verify describes a set of files that each completed an atomic
/// rename before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Envelope rules version (`FORMAT_VERSION`). Mismatch ⇒ `Refused`.
    pub format_version: u32,
    /// Field-shape version (`SCHEMA_VERSION`). Mismatch ⇒ `Refused`.
    pub schema_version: u32,
    /// Opaque per-session identifier, minted when the session's first commit is
    /// made and stable for the bundle's lifetime. Every member header repeats
    /// it: a member from a different session is a hard integrity failure.
    pub session_id: String,
    /// Wall clock at the instant this manifest was committed. Used for
    /// human-facing forensics and for the `NoBundle`/`InProgress` narrative.
    pub committed_wall: DateTime<Local>,
    /// Wall-clock companion of the **process monotonic anchor** that every
    /// `TimePoint::elapsed_secs` in this bundle is measured from (A §5 D5).
    pub anchor_wall: DateTime<Local>,
    /// How the writing process believes it is ending. `InProgress` = a periodic
    /// commit that was never finalized (legitimate, and the common case for a
    /// hard kill).
    pub ending: ProcessEnding,
    /// Monotonic-counter floors that MUST be re-seeded on restore so ids are
    /// never re-issued (A §5.7).
    pub identity_floors: IdentityFloors,
    /// Every member this manifest commits. Order is irrelevant; `rel_path` is
    /// the join key. A file on disk that is not listed here is an orphan and is
    /// ignored (never merged).
    pub members: Vec<ManifestMember>,
}

/// One committed bundle member. The three integrity fields (`rel_path`,
/// `byte_len`, `fingerprint`) are what a reader verifies **before** parsing, so
/// corruption is detected without trusting the file's own header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestMember {
    /// Which section this member carries.
    pub kind: SectionKind,
    /// Path relative to `Plan::session_bundle_path()`, e.g. `"manager.json"`,
    /// `"ui.json"`, `"workers/coder-t-018%237.json"` (key escaping: A §5.2).
    pub rel_path: String,
    /// Exact byte length of the member file as committed. A mismatch ⇒ `Refused`
    /// (or `Unrecoverable`, if and only if the member is a worker file and this
    /// is the only corrupt one — A D6).
    pub byte_len: u64,
    /// FNV-1a 64-bit fingerprint over the member file's exact bytes (A §7.2).
    pub fingerprint: u64,
    /// Wall clock when this member's payload was captured (not when it was
    /// renamed). Two members captured at different points in the loop can
    /// disagree; this is what makes the disagreement visible.
    pub saved_wall: DateTime<Local>,
    /// Seconds since the anchor of `Manifest.anchor_wall`, for the same instant
    /// as `saved_wall`. Monotonic **within one process**, unlike wall clock, so
    /// this — not `saved_wall` — is what orders members and detects reordering.
    pub saved_elapsed_secs: f64,
}

/// The three section families. `Worker` members carry their registry key inside
/// the section (`WorkerSection::worker_key`), never in the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SectionKind {
    Manager,
    Ui,
    Worker,
}

/// Frame shared by **every** member file (`manager.json`, `ui.json`, each
/// `workers/<key>.json`): `struct XFile { header: SectionHeader, section:
/// XSection }`.
///
/// The header is a *witness*, not an input: the loader cross-checks it against
/// the manifest and refuses on any disagreement. A section never trusts its own
/// header for integrity — the manifest is the authority (A §1 D3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionHeader {
    /// Redundant with `Manifest::format_version` on purpose: a member file that
    /// outlives its manifest (orphan) still self-identifies and can be refused
    /// with a precise reason instead of a deserialization error.
    pub format_version: u32,
    pub schema_version: u32,
    /// MUST equal `Manifest::session_id`.
    pub session_id: String,
    pub kind: SectionKind,
    /// MUST equal the corresponding `ManifestMember::saved_wall`.
    pub saved_wall: DateTime<Local>,
    /// MUST equal the corresponding `ManifestMember::saved_elapsed_secs`.
    pub saved_elapsed_secs: f64,
    /// Number of members this section is part of (`1` for Manager/Ui, `1` per
    /// worker file). Combined with `member_index`, this lets the loader detect a
    /// truncated *member set* even if the manifest itself is half-plausible.
    pub member_count: u32,
    pub member_index: u32,
}

/// Why the writing process is ending, as **it** knew it. This is a statement
/// about the *session*, not about file integrity: A D6's fail-closed rule keys on
/// structural integrity, never on `ending`.
///
/// Writer-side set: everything except `HardKillInferred`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ProcessEnding {
    /// A periodic (in-loop) commit that was never finalized. The process was
    /// killed without getting to write a terminal manifest — SIGKILL, OOM, a
    /// power cut. **Restorable**, and the normal case.
    InProgress,
    /// The session loop returned normally (turn cap, bounds stop, clean exit).
    CleanTail,
    /// User abort: Ctrl-C / the TUI abort path / a steering abort.
    UserAbort,
    /// `Plan::clear` / `/reset` deleted the plan and the session artefacts.
    /// MUST NOT be resurrected.
    Reset,
    /// The plan was archived: the session is **finished**, a different verdict
    /// from "interrupted".
    PlanArchived,
    /// Written from the panic hook (`src/main.rs`), which is pure-sync by
    /// necessity.
    PanicHook,
    /// Written from a real signal handler. `name` is the signal as spelled by
    /// the handler (`"SIGINT"`, `"SIGTERM"`, `"SIGHUP"`), not a number.
    Signal { name: String },
    /// **Loader-side sentinel; never written.** Reported by `session::restore`
    /// when it finds `ending == InProgress`: the process never recorded its own
    /// death, so the ending is inferred. Surfacing it as a distinct value keeps
    /// the UI honest without rewriting the bundle bytes (A D6).
    HardKillInferred,
}

/// Floors for the process-global monotonic counters that mint identities.
///
/// Each value is the counter's state at commit time: **every id strictly below it
/// has already been handed out.** Restore MUST raise the corresponding static
/// with `fetch_max(floor)` before any worker is re-registered or any notice is
/// issued (A §5.7 step 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityFloors {
    /// `orchestrator::workers::WORKER_ID_SEQ`. Seeded so `{agent}-wN` keys and
    /// `#`-disambiguators cannot be re-issued.
    pub worker_id_seq_floor: u64,
    /// `orchestrator::workers::COMPLETED_SEQ`. Seeded so a fresh completion never
    /// sorts before, or ties with, a restored completed entry.
    pub completed_seq_floor: u64,
    /// `orchestrator::notice::NEXT_NOTICE_ID` (ids spelled `notice-{n}`, origin
    /// **1**). Seeded so a notice id can never alias across the restart.
    pub notice_id_floor: u64,
}

impl IdentityFloors {
    /// Structural sanity (A §7.3 L7): a floor that is below the highest counter
    /// value the bundle itself recorded is a contradiction — the bundle is
    /// claiming an id was never issued while a member says it was.
    ///
    /// Returns a sentence per violation, so the refusal can quote the numbers.
    pub fn sanity_violations(&self, workers: &[super::worker::WorkerSection]) -> Vec<String> {
        let mut out = Vec::new();
        for w in workers {
            if w.worker_id_seq_value >= self.worker_id_seq_floor {
                out.push(format!(
                    "worker {} drew WORKER_ID_SEQ {} but the manifest floor is {}",
                    w.worker_key, w.worker_id_seq_value, self.worker_id_seq_floor
                ));
            }
            if let Some(seq) = w.completed_seq
                && seq >= self.completed_seq_floor
            {
                out.push(format!(
                    "worker {} drew COMPLETED_SEQ {} but the manifest floor is {}",
                    w.worker_key, seq, self.completed_seq_floor
                ));
            }
        }
        out
    }
}

/// On-disk shape of the `manager.json` member.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerFile {
    pub header: SectionHeader,
    pub section: ManagerSection,
}

/// The exact Manager transcript, plus the engine scalars that change the **next
/// request**. Derived from `manager::ContextEngine`, which has no serde shape of
/// its own.
///
/// Not persisted, by decision (each yields a `NonRestorableNotice` instead):
/// `stats: Option<Arc<HarnessStats>>` and `tool_schema_tokens` (re-charged from
/// live MCP state at boot; a persisted value would be a *lie* if the MCP set
/// changed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSection {
    /// The transcript, verbatim and in order, using `MessageRecord` — never
    /// `Vec<Message>` (A §2.1, the `serialize_assistant_content` trap).
    pub messages: Vec<MessageRecord>,
    /// `pinned_prefix_end(&messages)` as computed by the writing process.
    /// Restore recomputes it and MUST refuse on mismatch (A §4 R-PIN-5). `2` is
    /// the ordinary value.
    pub pinned_prefix_end: u32,
    /// Witness for the two pins, so a mismatch names *which* pin broke instead
    /// of "the transcript differs". Lengths, not hashes: cheap, and enough to
    /// localize.
    pub pin_witness: PinWitness,
    /// Engine budget. Re-read from `Config` at boot today; persisted so a
    /// restore can **report** a budget change rather than silently re-pricing
    /// the transcript (A §4 R-PIN-6).
    pub max_context_tokens: usize,
    /// Advisory latch — persists behaviour across the restart (without it the
    /// rebirth advisory re-fires).
    pub rebirth_advisory_emitted: bool,
    /// Rebirth cooldown counter (`consecutive_rebirths`).
    pub consecutive_rebirths: usize,
    /// Pairing state of the transcript at capture instant. A crash lands between
    /// the assistant-with-`tool_calls` write and that round's tool results, so
    /// this is expected to be non-`Balanced` often.
    pub dangling: DanglingToolCallState,
}

/// Byte-length witnesses for the two pinned messages (REQ-CORE-001/002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinWitness {
    /// Byte length of `messages[0].content` (the system prompt, REQ-CORE-001).
    /// The prompt is composed from `Config` + plan state at boot and therefore
    /// changes across a restart — this witness is what proves the restored value
    /// is the *old* one.
    pub system_prompt_len: u32,
    /// Byte length of `messages[1].content` (the pinned goal, REQ-CORE-002).
    pub goal_len: u32,
}
