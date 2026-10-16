//! The durable session vocabulary — the only place in the crate that names a
//! session file or declares a session `Serialize`/`Deserialize` type.
//!
//! Wire encoding is `serde_json` with 2-space pretty printing, so a bundle is
//! human-inspectable and diff-able: inspection is part of "loudly reported".
//! `#[serde(deny_unknown_fields)]` appears on every durable
//! struct — a bundle written by a *newer* release MUST be refused, never
//! partially read. Forward tolerance comes from `FORMAT_VERSION` /
//! `SCHEMA_VERSION` gating, not from silently ignoring fields.
//!
//! The file split below is internal; every type is re-exported here so callers
//! see one flat set of names.

pub mod envelope;
pub mod messages;
pub mod restore;
pub mod ui;
pub mod worker;

pub use envelope::{
    IdentityFloors, ManagerFile, ManagerSection, Manifest, ManifestMember, PinWitness,
    ProcessEnding, SectionHeader, SectionKind,
};
pub use messages::{MessageRecord, ToolCallRecord, ToolFunctionRecord};
pub use restore::{
    NonRestorableKind, NonRestorableNotice, NormalizeReport, PinStage, Refusal, RefusalCode,
    RepairReport, RestoreOutcome, RestoredSession, WorkerRepairCounts, witness_of,
};
pub use ui::{
    BudgetWitness, ComposerState, FOCUSED_PANELS, SubagentPanel, UiFile, UiJournalRecord,
    UiSection, ViewState,
};
pub use worker::{
    DanglingCall, DanglingToolCallState, HistorySource, WorkerCounters, WorkerFile, WorkerSection,
    WorkerStatus,
};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// Bump on any change to the **envelope/frame** rules (bundle layout, manifest
/// semantics, atomicity point). A reader that does not implement the file's
/// `format_version` MUST refuse rather than read a subset.
pub const FORMAT_VERSION: u32 = 1;

/// Bump on any change to the **field-level** shape of the types in this module
/// (`Manifest`, `ManagerSection`, `WorkerSection`, `UiSection`, `MessageRecord`…).
pub const SCHEMA_VERSION: u32 = 1;

/// Directory name under `Plan::session_bundle_path()`. The bundle is a
/// directory, never a single file — the manifest-last commit needs several
/// independently atomic renames.
pub const BUNDLE_DIR_NAME: &str = "session";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const MANAGER_FILE: &str = "manager.json";
pub const UI_FILE: &str = "ui.json";
pub const WORKERS_DIR_NAME: &str = "workers";

/// Placeholder for a `role:"tool"` result that was never produced. Owned here so
/// the capture side and the load side can never diverge from each other; mirrors
/// `manager::context::ABORTED_TOOL_RESULT`, whose value is pinned by the
/// compile-time equality assertion below.
pub const ABORTED_TOOL_RESULT: &str = "(aborted)";

/// The marker the one synthetic restore notice (`N-MGR-1`) starts
/// with. Registering it in the manager's advisory grammar is the manager's job; the
/// **constant** is owned here so the loader and the manager grammar cannot
/// drift apart.
pub const SESSION_RESTORE_NOTICE_PREFIX: &str = "(SYSTEM: SESSION RESTORED";

/// The FNV-1a 64-bit fingerprint over a member file's exact bytes.
///
/// Hand-rolled on purpose: `fnv` is only a transitive dependency, and the same two
/// constants already exist in the crate (`harness::fs::hash_path`, which is a
/// private shard selector and not a reuse candidate). A unit test pins the parity.
/// The fingerprint covers the whole member file, header included: there is no
/// partial or canonicalized fingerprint and no hash of the *parsed* value.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    hash
}

/// Durable projection of one `std::time::Instant`.
///
/// `session::time` is the only module allowed to build or read the *instant*
/// half of this pair (the math lives there as an `impl` block); the struct is
/// here because it is part of the durable shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimePoint {
    /// Seconds since the **capture process's monotonic anchor** (whose
    /// wall-clock position is `Manifest::anchor_wall`). Monotonic within one
    /// process; the only reliable ordering witness.
    pub elapsed_secs: f64,
    /// Absolute wall clock sampled together with `elapsed_secs`. It re-anchors an
    /// `Instant` in the new process, following the sanctioned
    /// precedent `manager::phase::get_plan_start_time`, and prints an honest
    /// timestamp.
    pub wall: DateTime<Local>,
}

impl TimePoint {
    /// The last clock rule: a `TimePoint` whose `elapsed_secs` is negative, NaN or
    /// infinite is a structural violation ⇒ `Refused`. No defaulting to "now".
    pub fn is_sane(&self) -> bool {
        self.elapsed_secs.is_finite() && self.elapsed_secs >= 0.0
    }
}

/// Compile-time pin: the session placeholder must be the manager's placeholder,
/// so the capture side and the load side can never spell
/// the aborted result differently.
const _: () = assert!(str_eq(
    ABORTED_TOOL_RESULT,
    crate::manager::context::ABORTED_TOOL_RESULT
));

const fn str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_the_harness_fnv_constants() {
        // The constants are a copy of `harness::fs::hash_path`'s.
        // Pinning the parity means a future edit to either
        // constant has to be a deliberate, attributed decision.
        let mut reference: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in b"manager.json" {
            reference ^= *byte as u64;
            reference = reference.wrapping_mul(0x0000_0100_0000_01b3);
        }
        assert_eq!(fingerprint(b"manager.json"), reference);

        // Known FNV-1a 64 vectors (the published `fnv1a_64` set: empty string is
        // the offset basis, `"a"` and `"foobar"` are the two standard strings).
        assert_eq!(fingerprint(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fingerprint(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fingerprint(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn time_point_sanity_rejects_non_finite_and_negative() {
        let wall = Local::now();
        assert!(
            TimePoint {
                elapsed_secs: 0.0,
                wall,
            }
            .is_sane()
        );
        assert!(
            !TimePoint {
                elapsed_secs: -0.5,
                wall
            }
            .is_sane()
        );
        assert!(
            !TimePoint {
                elapsed_secs: f64::NAN,
                wall
            }
            .is_sane()
        );
        assert!(
            !TimePoint {
                elapsed_secs: f64::INFINITY,
                wall
            }
            .is_sane()
        );
    }

    #[test]
    fn every_member_type_carries_deny_unknown_fields() {
        // Behavioural counterpart of the version pin: an extra field is a parse
        // error, not a silently ignored key. One probe per frame is enough to
        // prove the attribute is on the envelope.
        let err = serde_json::from_str::<ManifestMember>(
            r#"{"kind":"manager","rel_path":"manager.json","byte_len":1,"fingerprint":1,
                "saved_wall":"2025-01-01T00:00:00+01:00","saved_elapsed_secs":0.0,"future":true}"#,
        )
        .expect_err("an unknown field must be refused, never ignored");
        assert!(
            err.to_string().contains("unknown field"),
            "expected an unknown-field refusal, got {err}"
        );
    }
}
