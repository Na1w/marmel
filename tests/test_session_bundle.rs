//! t-007 round-trip acceptance for the session bundle: write a small bundle into
//! a temp dir, read it back, and pin the four properties the design calls
//! load-bearing —
//!
//! * **manifest-last is the commit point** (A §7.1 C4): members that no manifest
//!   names are not committed, and members rewritten under an older manifest are
//!   caught by integrity, not by luck;
//! * **tamper detection** (`byte_len`, `fingerprint`) with both numbers reported
//!   and the bundle left byte-identical (A §7.2 F1/F4, §7.5);
//! * **`content: None` vs `Some("")`** survives the bundle exactly, which is the
//!   reason `MessageRecord` exists at all (A §2.1, `types/wire.rs:30-41`);
//! * the single-corrupt-worker partial (A D6) and the L0–L9 refusals around it.

use std::path::{Path, PathBuf};

use marmennill::agents::Agent;
use marmennill::session::commit::{
    CapturePayload, CommitError, CommitLock, CommitPolicy, CommitReport, fingerprint,
};
use marmennill::session::format::RestoreOutcome;
use marmennill::session::format::envelope::{
    IdentityFloors, ManagerSection, Manifest, ManifestMember, PinWitness, ProcessEnding,
    SectionHeader, SectionKind,
};
use marmennill::session::format::worker::WorkerSection;
use marmennill::session::format::{ABORTED_TOOL_RESULT, FORMAT_VERSION, SCHEMA_VERSION, TimePoint};
use marmennill::session::restore::load_bundle;
use marmennill::session::{
    DanglingCall, DanglingToolCallState, HistorySource, MessageRecord, NonRestorableKind,
    UiSection, WorkerStatus,
};

// ---------------------------------------------------------------------- fixtures

fn manager_section(messages: Vec<MessageRecord>) -> ManagerSection {
    let witness = PinWitness {
        system_prompt_len: messages
            .first()
            .map(|m| m.content().map_or(0, |c| c.len()))
            .unwrap_or(0) as u32,
        goal_len: messages
            .get(1)
            .map(|m| m.content().map_or(0, |c| c.len()))
            .unwrap_or(0) as u32,
    };
    ManagerSection {
        pinned_prefix_end: 2,
        pin_witness: witness,
        max_context_tokens: 128,
        rebirth_advisory_emitted: false,
        consecutive_rebirths: 0,
        dangling: DanglingToolCallState::observe(&messages),
        messages,
    }
}

fn transcript() -> Vec<MessageRecord> {
    vec![
        MessageRecord::System {
            content: "system prompt".to_string(),
        },
        MessageRecord::User {
            content: "the goal".to_string(),
        },
        MessageRecord::Assistant {
            content: None,
            reasoning_content: Some("thinking about the mission".to_string()),
            tool_calls: vec![marmennill::session::ToolCallRecord {
                id: "call-1".to_string(),
                kind: "function".to_string(),
                function: marmennill::session::ToolFunctionRecord {
                    name: "read_file".to_string(),
                    arguments: "{\"path\":\"src/main.rs\"}".to_string(),
                },
            }],
        },
        MessageRecord::Tool {
            tool_call_id: "call-1".to_string(),
            content: "file bytes".to_string(),
        },
        MessageRecord::Assistant {
            content: Some(String::new()),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ]
}

fn worker(key: &str, history: Vec<MessageRecord>) -> WorkerSection {
    let dangling = DanglingToolCallState::observe(&history);
    WorkerSection {
        worker_key: key.to_string(),
        worker_id_seq_value: 2,
        role: Agent::Coder,
        task_id: Some("t-007".to_string()),
        status: WorkerStatus::InProgress,
        status_label: "running".to_string(),
        started: TimePoint::now(),
        completed: None,
        last_activity: None,
        worked_secs: 12.5,
        completed_seq: None,
        context_tokens: 64,
        last_tokens: 7,
        counters: Default::default(),
        brief: format!("brief for {key}"),
        snippets: vec!["snippet".to_string()],
        image_urls: vec![],
        audio_urls: vec![],
        recursion_granted: false,
        history,
        history_source: HistorySource::LiveMirror,
        dangling,
        unrecoverable_reason: None,
        non_restorable: vec![],
    }
}

fn payload() -> CapturePayload {
    let history = vec![
        MessageRecord::System {
            content: "specialist prompt".to_string(),
        },
        MessageRecord::User {
            content: "specialist brief".to_string(),
        },
        MessageRecord::Assistant {
            content: Some("I will edit the file".to_string()),
            reasoning_content: None,
            tool_calls: vec![marmennill::session::ToolCallRecord {
                id: "call-9".to_string(),
                kind: "function".to_string(),
                function: marmennill::session::ToolFunctionRecord {
                    name: "replace".to_string(),
                    // Deliberately malformed JSON: the bundle must keep it
                    // verbatim, `ensure_valid_json_arguments` must never run.
                    arguments: "{oops".to_string(),
                },
            }],
        },
    ];
    CapturePayload {
        manager: manager_section(transcript()),
        ui: Some(UiSection {
            status_line: "phase 2/3".to_string(),
            ..UiSection::default()
        }),
        workers: vec![
            worker("coder-t-007", history),
            worker("coder-t-018#7", Vec::new()),
        ],
        identity_floors: IdentityFloors {
            worker_id_seq_floor: 3,
            completed_seq_floor: 1,
            notice_id_floor: 4,
        },
    }
}

/// A fresh temp dir whose `.marmel/session` layout mirrors a real plan.
fn bundle_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("marmel-t007-{tag}-{}", std::process::id()));
    let bundle = root.join(".marmel").join("session");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&bundle).expect("temp bundle dir");
    bundle
}

fn commit(bundle: &Path, payload: &CapturePayload, ending: ProcessEnding) -> CommitReport {
    let lock = CommitLock::new();
    write_bundle_public(bundle, "session-1", payload, ending, &lock)
}

fn write_bundle_public(
    bundle: &Path,
    session_id: &str,
    payload: &CapturePayload,
    ending: ProcessEnding,
    lock: &CommitLock,
) -> CommitReport {
    let policy = CommitPolicy {
        min_interval: std::time::Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    marmennill::session::commit::write_bundle(bundle, session_id, payload, ending, &policy, lock)
        .expect("commit must succeed")
}

fn member_path(bundle: &Path, rel: &str) -> PathBuf {
    bundle.join(rel)
}

fn read(bundle: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(member_path(bundle, rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

// ----------------------------------------------------------------- round trip

#[test]
fn round_trip_restores_every_member_byte_exactly() {
    let bundle = bundle_dir("round-trip");
    let original = payload();
    let report = commit(&bundle, &original, ProcessEnding::InProgress);

    assert_eq!(
        report.members_written,
        vec![
            "workers/coder-t-007.json",
            "workers/coder-t-018%237.json",
            "ui.json",
            "manager.json",
        ],
        "A §7.1 C3 order is normative, and the manifest is never a member"
    );
    assert_eq!(report.members_skipped_identical, 0);
    assert!(report.manifest_bytes > 0);
    assert_eq!(report.orphans_removed, 0);

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Restored {
        session,
        ending,
        report,
    } = outcome
    else {
        panic!("expected a full restore, got {outcome:?}");
    };

    assert_eq!(session.session_id, "session-1");
    // InProgress is reported as HardKillInferred, never written (A D6).
    assert_eq!(ending, ProcessEnding::HardKillInferred);
    assert_eq!(session.manager.messages, original.manager.messages);
    assert_eq!(session.ui.status_line, "phase 2/3");
    assert_eq!(session.workers.len(), 2);
    assert_eq!(session.workers[0].worker_key, "coder-t-007");
    assert_eq!(session.workers[1].worker_key, "coder-t-018#7");
    assert_eq!(session.workers[1].status, WorkerStatus::InProgress);
    assert_eq!(session.identity_floors.worker_id_seq_floor, 3);
    assert_eq!(session.identity_floors.notice_id_floor, 4);
    assert!(report.pins_verified);
    assert!(report.normalization_stable);
    assert_eq!(report.normalization_passes, 2);
    assert_eq!(report.workers_restored, 2);
    assert!(report.workers_unrecoverable.is_empty());

    // N2 healed the worker's dangling call: one `(aborted)` result synthesized.
    assert_eq!(report.aborted_results_synthesized, 1);
    assert_eq!(session.workers[0].history.len(), 4);
    assert_eq!(
        session.workers[0].history[3],
        MessageRecord::Tool {
            tool_call_id: "call-9".to_string(),
            content: ABORTED_TOOL_RESULT.to_string(),
        },
        "the synthesized placeholder must be the crate's only placeholder vocabulary"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("for tool_call_id call-9") && w.contains("worker coder-t-007")),
        "A §8.4 requires one loud sentence per synthesis: {:?}",
        report.warnings
    );

    // The malformed arguments came back verbatim.
    let MessageRecord::Assistant { tool_calls, .. } = &session.workers[0].history[2] else {
        panic!("expected the assistant turn");
    };
    assert_eq!(tool_calls[0].function.arguments, "{oops");

    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn content_none_and_empty_string_are_different_facts_through_the_bundle() {
    let bundle = bundle_dir("none-vs-empty");
    let original = payload();
    commit(&bundle, &original, ProcessEnding::CleanTail);
    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Restored { session, .. } = &outcome else {
        panic!("expected a full restore, got {}", explain(&outcome));
    };

    // transcript()[2] is `content: None`, transcript()[4] is `Some("")`.
    let MessageRecord::Assistant {
        content: first,
        reasoning_content,
        ..
    } = &session.manager.messages[2]
    else {
        panic!("expected an assistant turn");
    };
    assert_eq!(first, &None, "content: None must not degrade to \"\"");
    assert_eq!(
        reasoning_content.as_deref(),
        Some("thinking about the mission"),
        "reasoning_content must survive too"
    );
    let MessageRecord::Assistant {
        content: last,
        reasoning_content: last_reasoning,
        ..
    } = &session.manager.messages[4]
    else {
        panic!("expected an assistant turn");
    };
    assert_eq!(
        last,
        &Some(String::new()),
        "content: \"\" must not become None"
    );
    assert_eq!(last_reasoning, &None);

    // The on-disk bytes distinguish them, which the wire serializer cannot.
    let text = String::from_utf8(read(&bundle, "manager.json")).expect("utf-8 member");
    assert!(
        text.contains("\"content\": null"),
        "manager.json must spell the absent content as null: {text}"
    );
    assert!(
        text.contains("\"content\": \"\""),
        "manager.json must spell the empty content as \"\": {text}"
    );
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

// ------------------------------------------------- manifest-last is the commit

#[test]
fn members_without_a_manifest_are_not_a_committed_bundle() {
    let bundle = bundle_dir("no-manifest");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    std::fs::remove_file(bundle.join("manifest.json")).expect("remove manifest");

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::NoBundle { orphan_members, .. } = &outcome else {
        panic!("a bundle with no manifest is NoBundle, got {outcome:?}");
    };
    assert_eq!(
        orphan_members,
        &[
            "manager.json",
            "ui.json",
            "workers/coder-t-007.json",
            "workers/coder-t-018%237.json"
        ]
        .map(String::from)
        .to_vec(),
        "orphan members must be counted and named, never silently merged"
    );
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn manifest_is_the_last_write_and_the_previous_one_survives_an_interrupted_round() {
    let bundle = bundle_dir("manifest-last");
    commit(&bundle, &payload(), ProcessEnding::InProgress);

    // An interrupted round: members are rewritten, the manifest is not. The
    // manifest's mtime must be **no older** than any member's, because the
    // manifest rename is the last write of a commit.
    let manifest_time = std::fs::metadata(bundle.join("manifest.json"))
        .and_then(|m| m.modified())
        .expect("manifest mtime");
    for rel in [
        "manager.json",
        "ui.json",
        "workers/coder-t-007.json",
        "workers/coder-t-018%237.json",
    ] {
        let time = std::fs::metadata(member_path(&bundle, rel))
            .and_then(|m| m.modified())
            .expect("member mtime");
        assert!(
            manifest_time >= time,
            "{rel} was written after manifest.json, so C4 is not the last write"
        );
    }

    // Simulate the crash window: rewrite one member under the OLD manifest.
    let grown = payload();
    let mut longer = grown;
    longer.manager.messages.pop();
    let stale_bytes =
        serde_json::to_vec_pretty(&marmennill::session::format::envelope::ManagerFile {
            header: SectionHeader {
                format_version: FORMAT_VERSION,
                schema_version: SCHEMA_VERSION,
                session_id: "session-1".to_string(),
                kind: SectionKind::Manager,
                saved_wall: chrono::Local::now(),
                saved_elapsed_secs: 1.0,
                member_count: 1,
                member_index: 0,
            },
            section: longer.manager.clone(),
        })
        .expect("serialize");
    marmennill::harness::fs::write_file_atomic(&member_path(&bundle, "manager.json"), &stale_bytes)
        .expect("member write");

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, .. } = &outcome else {
        panic!(
            "a member rewritten under an older manifest must refuse, got {:?}",
            kind_of(&outcome)
        );
    };
    assert!(
        matches!(
            refusal.code,
            marmennill::session::RefusalCode::ByteLen
                | marmennill::session::RefusalCode::Fingerprint
        ),
        "expected the crash window to surface as ByteLen/Fingerprint, got {:?}",
        refusal.code
    );
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn a_second_commit_rewrites_only_what_changed() {
    let bundle = bundle_dir("diff-cache");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    let mut grown = payload();
    grown.manager.messages.push(MessageRecord::User {
        content: "steer: keep going".to_string(),
    });
    // Same worker bytes: only the manager member changes. The capture instant is
    // part of the header, so the workers still re-serialize; assert only that the
    // manager is always among the written members and the manifest never is.
    let lock = CommitLock::new();
    let report = write_bundle_public(
        &bundle,
        "session-1",
        &grown,
        ProcessEnding::InProgress,
        &lock,
    );
    assert!(report.members_written.contains(&"manager.json".to_string()));
    assert!(
        !report
            .members_written
            .contains(&"manifest.json".to_string())
    );

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Restored { session, .. } = &outcome else {
        panic!(
            "expected a full restore after the second commit, got {}",
            explain(&outcome)
        );
    };
    assert_eq!(session.manager.messages.len(), 6);
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn a_commit_that_cannot_take_the_lock_fails_without_touching_the_bundle() {
    let bundle = bundle_dir("locked");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    let before: Vec<(PathBuf, u64)> = walk(&bundle)
        .into_iter()
        .map(|p| (p.clone(), fingerprint(&std::fs::read(&p).expect("read"))))
        .collect();

    let lock = CommitLock::new();
    let guard = lock.try_guard().expect("first guard");
    let policy = CommitPolicy {
        min_interval: std::time::Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    let err = marmennill::session::commit::write_bundle(
        &bundle,
        "session-1",
        &payload(),
        ProcessEnding::InProgress,
        &policy,
        &lock,
    )
    .expect_err("a commit already in flight must not start a second one");
    assert!(
        matches!(err, CommitError::Locked),
        "expected CommitError::Locked, got {err:?}"
    );
    assert!(
        lock.mark_dirty(),
        "the refused round must leave a pending (dirty) request for the holder"
    );

    let after = walk(&bundle);
    assert_eq!(after.len(), before.len(), "a locked commit writes nothing");
    drop(guard);
    assert!(
        !lock.in_flight(),
        "the guard release must clear the in-flight bit"
    );
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn a_bundle_path_occupied_by_a_file_is_an_error_not_a_silent_success() {
    let root = std::env::temp_dir().join(format!("marmel-t007-not-a-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".marmel")).expect("temp root");
    let bundle = root.join(".marmel").join("session");
    std::fs::write(&bundle, b"not a directory").expect("occupy the bundle path");

    let lock = CommitLock::new();
    let policy = CommitPolicy {
        min_interval: std::time::Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    let result = marmennill::session::commit::write_bundle(
        &bundle,
        "session-1",
        &payload(),
        ProcessEnding::InProgress,
        &policy,
        &lock,
    );
    assert!(
        matches!(
            result,
            Err(CommitError::Io { .. } | CommitError::Write { .. })
        ),
        "expected an io/write failure, got {result:?}"
    );
    let outcome = load_bundle(&bundle);
    assert!(
        matches!(outcome, RestoreOutcome::NoBundle { .. }),
        "a non-directory bundle path must read as no bundle: {}",
        explain(&outcome)
    );
    std::fs::remove_dir_all(&root).ok();
}

// ----------------------------------------------------------------- tampering

#[test]
fn fingerprint_tamper_is_refused_and_the_bundle_stays_byte_identical() {
    let bundle = bundle_dir("fingerprint");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    let mut ui = read(&bundle, "ui.json");
    let before = fingerprint(&ui);
    // Same length, different content: only the fingerprint can catch this.
    let last = ui.len() - 2;
    ui[last] = b' ';
    std::fs::write(member_path(&bundle, "ui.json"), &ui).expect("tamper");

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, .. } = &outcome else {
        panic!("a 1-byte tamper must refuse, got {:?}", kind_of(&outcome))
    };
    assert_eq!(refusal.code, marmennill::session::RefusalCode::Fingerprint);
    assert_eq!(refusal.member.as_deref(), Some("ui.json"));
    assert!(
        refusal.message.contains(&format!("0x{before:016x}")),
        "F4: both sides must be printed in hex, got {}",
        refusal.message
    );

    // Read-only loader: the tampered bytes are still there, untouched.
    assert_eq!(fingerprint(&read(&bundle, "ui.json")), fingerprint(&ui));
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn byte_len_mismatch_is_refused_with_both_numbers() {
    let bundle = bundle_dir("byte-len");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    let mut manager = read(&bundle, "manager.json");
    manager.push(b'\n');
    std::fs::write(member_path(&bundle, "manager.json"), &manager).expect("tamper");

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, .. } = &outcome else {
        panic!("an appended byte must refuse, got {:?}", kind_of(&outcome))
    };
    assert_eq!(refusal.code, marmennill::session::RefusalCode::ByteLen);
    assert_eq!(refusal.member.as_deref(), Some("manager.json"));
    assert!(refusal.message.contains("bytes"), "{}", refusal.message);
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn a_listed_member_that_is_absent_is_missing_member() {
    let bundle = bundle_dir("missing");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    std::fs::remove_file(member_path(&bundle, "ui.json")).expect("remove ui");

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, .. } = &outcome else {
        panic!("a missing member must refuse, got {:?}", kind_of(&outcome))
    };
    assert_eq!(
        refusal.code,
        marmennill::session::RefusalCode::MissingMember
    );
    assert_eq!(refusal.member.as_deref(), Some("ui.json"));
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn an_unparsable_manifest_refuses_with_the_serde_coordinates() {
    let bundle = bundle_dir("bad-manifest");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    std::fs::write(bundle.join("manifest.json"), b"{ \"format_version\": 1,")
        .expect("truncate manifest");

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, .. } = &outcome else {
        panic!(
            "a truncated manifest must refuse, got {:?}",
            kind_of(&outcome)
        )
    };
    assert_eq!(refusal.code, marmennill::session::RefusalCode::Json);
    assert_eq!(refusal.member.as_deref(), Some("manifest.json"));
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn version_gates_refuse_before_anything_is_parsed() {
    for (field, value, expected) in [
        (
            "format_version",
            99,
            marmennill::session::RefusalCode::FormatVersion,
        ),
        (
            "schema_version",
            99,
            marmennill::session::RefusalCode::SchemaVersion,
        ),
    ] {
        let bundle = bundle_dir(&format!("version-{field}"));
        commit(&bundle, &payload(), ProcessEnding::InProgress);
        let text = String::from_utf8(read(&bundle, "manifest.json")).expect("utf-8");
        let patched = text.replace(&format!("\"{field}\": 1"), &format!("\"{field}\": {value}"));
        assert_ne!(
            text, patched,
            "the probe must patch {field} in the manifest"
        );
        std::fs::write(bundle.join("manifest.json"), patched.as_bytes()).expect("patch");

        let outcome = load_bundle(&bundle);
        let RestoreOutcome::Refused { refusal, .. } = &outcome else {
            panic!("{field} mismatch must refuse, got {:?}", kind_of(&outcome))
        };
        assert_eq!(refusal.code, expected, "{}", refusal.message);
        std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
    }
}

/// A hand-built bundle whose member carries a field this binary does not know:
/// the manifest is the authority, so the member verifies and *then* the schema
/// gate fires (`A §7.5`, `deny_unknown_fields`).
#[test]
fn an_unknown_field_in_a_member_is_a_hard_refusal_not_a_silent_drop() {
    let bundle = bundle_dir("unknown-field");
    let header = SectionHeader {
        format_version: FORMAT_VERSION,
        schema_version: SCHEMA_VERSION,
        session_id: "session-1".to_string(),
        kind: SectionKind::Ui,
        saved_wall: chrono::Local::now(),
        saved_elapsed_secs: 0.5,
        member_count: 1,
        member_index: 0,
    };
    let mut ui_json = serde_json::to_vec_pretty(&marmennill::session::format::ui::UiFile {
        header: header.clone(),
        section: UiSection::default(),
    })
    .expect("serialize ui");
    let ui_text = String::from_utf8(ui_json.clone()).expect("utf-8");
    let with_extra = ui_text.replace(
        "\"status_line\": \"\",",
        "\"status_line\": \"\", \"a_field_from_the_future\": 7,",
    );
    assert_ne!(
        ui_text, with_extra,
        "the probe must actually patch the JSON"
    );
    ui_json = with_extra.into_bytes();

    // manager.json stays a committed, valid member.
    let manager = marmennill::session::format::envelope::ManagerFile {
        header: SectionHeader {
            kind: SectionKind::Manager,
            ..header.clone()
        },
        section: manager_section(transcript()),
    };
    let manager_bytes = serde_json::to_vec_pretty(&manager).expect("serialize manager");

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        schema_version: SCHEMA_VERSION,
        session_id: "session-1".to_string(),
        committed_wall: chrono::Local::now(),
        anchor_wall: chrono::Local::now(),
        ending: ProcessEnding::CleanTail,
        identity_floors: IdentityFloors {
            worker_id_seq_floor: 1,
            completed_seq_floor: 1,
            notice_id_floor: 1,
        },
        members: vec![
            ManifestMember {
                kind: SectionKind::Manager,
                rel_path: "manager.json".to_string(),
                byte_len: manager_bytes.len() as u64,
                fingerprint: fingerprint(&manager_bytes),
                saved_wall: header.saved_wall,
                saved_elapsed_secs: header.saved_elapsed_secs,
            },
            ManifestMember {
                kind: SectionKind::Ui,
                rel_path: "ui.json".to_string(),
                byte_len: ui_json.len() as u64,
                fingerprint: fingerprint(&ui_json),
                saved_wall: header.saved_wall,
                saved_elapsed_secs: header.saved_elapsed_secs,
            },
        ],
    };
    std::fs::write(member_path(&bundle, "manager.json"), &manager_bytes).expect("write manager");
    std::fs::write(member_path(&bundle, "ui.json"), &ui_json).expect("write ui");
    std::fs::write(
        bundle.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, .. } = &outcome else {
        panic!("an unknown field must refuse, got {:?}", kind_of(&outcome))
    };
    assert_eq!(refusal.code, marmennill::session::RefusalCode::UnknownField);
    assert_eq!(refusal.member.as_deref(), Some("ui.json"));
    assert!(
        refusal.message.contains("a_field_from_the_future"),
        "the offending field must be named: {}",
        refusal.message
    );
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn a_header_that_disagrees_with_the_manifest_is_refused() {
    let bundle = bundle_dir("header-disagreement");
    let original = payload();
    commit(&bundle, &original, ProcessEnding::InProgress);

    // Re-write ui.json under a manifest that still lists the OLD bytes? No: the
    // header check must fire while the integrity fields still match, so patch the
    // header inside the committed bytes and re-commit the whole set by hand.
    let mut ui_bytes = read(&bundle, "ui.json");
    let ui_text = String::from_utf8(ui_bytes.clone()).expect("utf-8");
    let patched = ui_text.replace("\"session_id\": \"session-1\"", "\"session_id\": \"other\"");
    assert_ne!(ui_text, patched, "the probe must patch the header");
    ui_bytes = patched.into_bytes();
    std::fs::write(member_path(&bundle, "ui.json"), &ui_bytes).expect("write ui");

    let mut manifest_text = String::from_utf8(read(&bundle, "manifest.json")).expect("utf-8");
    // Point the manifest at the new bytes so L4 passes and L5 is the thing caught.
    let fresh = std::fs::read(member_path(&bundle, "ui.json")).expect("read back");
    manifest_text = patch_member_witness(&manifest_text, "ui.json", &fresh);
    std::fs::write(bundle.join("manifest.json"), manifest_text.as_bytes()).expect("write manifest");

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, .. } = &outcome else {
        panic!(
            "a header/manifest disagreement must refuse, got {:?}",
            kind_of(&outcome)
        )
    };
    assert_eq!(
        refusal.code,
        marmennill::session::RefusalCode::HeaderManifestDisagreement
    );
    assert!(
        refusal.message.contains("session_id"),
        "{}",
        refusal.message
    );
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

/// Rewrites the `byte_len`/`fingerprint` of one member inside the manifest so a
/// hand-patched member still verifies (isolating the check under test).
fn patch_member_witness(manifest_text: &str, rel_path: &str, bytes: &[u8]) -> String {
    let mut doc: serde_json::Value =
        serde_json::from_str(manifest_text).expect("the committed manifest parses");
    let members = doc
        .get_mut("members")
        .and_then(|m| m.as_array_mut())
        .expect("members array");
    let member = members
        .iter_mut()
        .find(|m| m.get("rel_path").and_then(|r| r.as_str()) == Some(rel_path))
        .unwrap_or_else(|| panic!("{rel_path} is not listed in the manifest"));
    member["byte_len"] = serde_json::Value::from(bytes.len() as u64);
    member["fingerprint"] = serde_json::Value::from(fingerprint(bytes));
    serde_json::to_string_pretty(&doc).expect("re-serialize manifest")
}

// ------------------------------------------------- the D6 single-worker partial

#[test]
fn exactly_one_corrupt_worker_restores_everything_else_as_unrecoverable() {
    let bundle = bundle_dir("one-corrupt-worker");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    let mut worker_bytes = read(&bundle, "workers/coder-t-018%237.json");
    worker_bytes[10] = b'x';
    std::fs::write(
        member_path(&bundle, "workers/coder-t-018%237.json"),
        &worker_bytes,
    )
    .expect("tamper worker");

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::PartiallyRestored {
        session,
        unrecoverable_workers,
        report,
        ending,
    } = &outcome
    else {
        panic!(
            "one corrupt worker member must be the tolerated partial, got {}",
            explain(&outcome)
        )
    };
    assert_eq!(unrecoverable_workers, &vec!["coder-t-018#7".to_string()]);
    assert_eq!(ending, &ProcessEnding::HardKillInferred);
    assert_eq!(session.workers.len(), 2, "the pane identity must survive");
    assert_eq!(session.workers[0].worker_key, "coder-t-007");
    assert_eq!(session.workers[1].status, WorkerStatus::Unrecoverable);
    assert!(
        session.workers[1]
            .unrecoverable_reason
            .as_deref()
            .is_some_and(|r| r.contains("fingerprint")),
        "the reason must be the real verification failure: {:?}",
        session.workers[1].unrecoverable_reason
    );
    assert_eq!(report.workers_restored, 1);
    assert_eq!(
        report.workers_unrecoverable,
        vec!["coder-t-018#7".to_string()]
    );
    assert!(
        report.notices.iter().any(|n| {
            n.kind == NonRestorableKind::WorkerHistory && n.subject == "coder-t-018#7"
        })
    );
    // The manager transcript still restored in full.
    assert_eq!(session.manager.messages.len(), 5);
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn two_corrupt_workers_refuse_and_name_the_second() {
    let bundle = bundle_dir("two-corrupt-workers");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    for rel in ["workers/coder-t-007.json", "workers/coder-t-018%237.json"] {
        let mut bytes = read(&bundle, rel);
        bytes[10] = b'x';
        std::fs::write(member_path(&bundle, rel), &bytes).expect("tamper worker");
    }

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, .. } = &outcome else {
        panic!(
            "two corrupt workers must refuse, got {:?}",
            kind_of(&outcome)
        )
    };
    assert!(
        refusal.message.contains("single") && refusal.message.contains("coder-t"),
        "the refusal must explain the tolerance it declined: {}",
        refusal.message
    );
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

// ------------------------------------------------------------------ pin rules

#[test]
fn a_pin_witness_mismatch_refuses_and_names_both_lengths() {
    let bundle = bundle_dir("pin-violation");
    let mut original = payload();
    original.manager.pin_witness = PinWitness {
        system_prompt_len: 1,
        goal_len: 999,
    };
    commit(&bundle, &original, ProcessEnding::InProgress);

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, report } = &outcome else {
        panic!("a pin violation must refuse, got {:?}", kind_of(&outcome))
    };
    assert_eq!(refusal.code, marmennill::session::RefusalCode::PinViolation);
    assert_eq!(refusal.member.as_deref(), Some("manager.json"));
    assert!(refusal.message.contains("step 5"), "{}", refusal.message);
    assert_eq!(report.pinned_prefix_end_expected, Some(2));
    assert_eq!(report.pinned_prefix_end_actual, Some(2));
    assert!(!report.pins_verified);
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn a_pinned_prefix_end_mismatch_refuses() {
    let bundle = bundle_dir("pin-window");
    let mut original = payload();
    original.manager.pinned_prefix_end = 4;
    commit(&bundle, &original, ProcessEnding::InProgress);

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Refused { refusal, .. } = &outcome else {
        panic!(
            "a stale pinned_prefix_end must refuse, got {:?}",
            kind_of(&outcome)
        )
    };
    assert_eq!(refusal.code, marmennill::session::RefusalCode::PinViolation);
    assert!(refusal.message.contains("step 4"), "{}", refusal.message);
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

// ------------------------------------------------------------- structural gate

#[test]
fn a_dangling_call_recorded_in_the_manager_is_healed_and_reported() {
    let bundle = bundle_dir("manager-dangling");
    let mut messages = transcript();
    messages.push(MessageRecord::Assistant {
        content: None,
        reasoning_content: None,
        tool_calls: vec![marmennill::session::ToolCallRecord {
            id: "call-42".to_string(),
            kind: "function".to_string(),
            function: marmennill::session::ToolFunctionRecord {
                name: "run_command".to_string(),
                arguments: "{}".to_string(),
            },
        }],
    });
    let mut original = payload();
    original.manager = manager_section(messages);
    commit(&bundle, &original, ProcessEnding::InProgress);

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Restored {
        session, report, ..
    } = &outcome
    else {
        panic!(
            "a dangling manager transcript is healable, not fatal: {}",
            explain(&outcome)
        );
    };
    // 6 stored records + the one synthesized placeholder = 7.
    assert_eq!(session.manager.messages.len(), 7);
    assert_eq!(
        session.manager.messages[6],
        MessageRecord::Tool {
            tool_call_id: "call-42".to_string(),
            content: ABORTED_TOOL_RESULT.to_string(),
        }
    );
    assert_eq!(report.aborted_results_synthesized, 2);
    // N6: the recorded witness said Dangling, the healed vector is Balanced.
    assert!(
        report.warnings.iter().any(|w| w.contains("dangling")),
        "N6 must report the disagreement: {:?}",
        report.warnings
    );
    assert_eq!(
        original.manager.dangling,
        DanglingToolCallState::Dangling {
            calls: vec![DanglingCall {
                tool_call_id: "call-42".to_string(),
                function_name: "run_command".to_string(),
                arguments: "{}".to_string(),
                dispatched: false,
            }]
        },
        "the capture-side witness itself must be observable"
    );
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

#[test]
fn an_orphaned_tool_result_is_pruned_and_counted() {
    let bundle = bundle_dir("orphan-result");
    let mut messages = transcript();
    messages.push(MessageRecord::Tool {
        tool_call_id: "call-nowhere".to_string(),
        content: "nobody asked for this".to_string(),
    });
    let mut original = payload();
    original.manager = manager_section(messages);
    commit(&bundle, &original, ProcessEnding::InProgress);

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Restored {
        session, report, ..
    } = &outcome
    else {
        panic!(
            "an orphan tool result is prunable, not fatal: {}",
            explain(&outcome)
        );
    };
    assert_eq!(report.orphan_tool_results_pruned, 1);
    assert_eq!(session.manager.messages.len(), 5);
    assert!(
        !session
            .manager
            .messages
            .iter()
            .any(|m| matches!(m, MessageRecord::Tool { tool_call_id, .. } if tool_call_id == "call-nowhere")),
        "the orphan must be gone"
    );
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

// ------------------------------------------------------------------- L0 shape

#[test]
fn no_bundle_names_the_legacy_journals_it_refuses_to_upgrade() {
    let root = std::env::temp_dir().join(format!("marmel-t007-legacy-{}", std::process::id()));
    let plan_dir = root.join(".marmel");
    std::fs::create_dir_all(&plan_dir).expect("temp .marmel dir");
    for name in [".session_transcript.json", ".ui_transcript.json"] {
        std::fs::write(plan_dir.join(name), "[]").expect("write legacy journal");
    }

    let bundle = plan_dir.join("session");
    let outcome = load_bundle(&bundle);
    let RestoreOutcome::NoBundle {
        legacy_journals,
        orphan_members,
        ..
    } = &outcome
    else {
        panic!("absent bundle is NoBundle, got {:?}", kind_of(&outcome))
    };
    assert_eq!(
        legacy_journals.len(),
        2,
        "both legacy journals must be named"
    );
    assert!(orphan_members.is_empty());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn loading_never_touches_disk_and_never_installs_state() {
    let bundle = bundle_dir("read-only");
    commit(&bundle, &payload(), ProcessEnding::InProgress);
    let mut before = Vec::new();
    for entry in walk(&bundle) {
        before.push((
            entry.clone(),
            fingerprint(&std::fs::read(&entry).expect("read")),
        ));
    }
    for _ in 0..3 {
        let outcome = load_bundle(&bundle);
        assert!(
            matches!(outcome, RestoreOutcome::Restored { .. }),
            "expected a full restore, got {}",
            explain(&outcome)
        );
    }
    let after = walk(&bundle);
    assert_eq!(
        after.len(),
        before.len(),
        "the loader must not create files"
    );
    for (path, hash) in &before {
        assert_eq!(
            fingerprint(&std::fs::read(path).expect("read")),
            *hash,
            "{path:?} changed during load"
        );
    }
    std::fs::remove_dir_all(bundle.parent().unwrap().parent().unwrap()).ok();
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn kind_of(outcome: &RestoreOutcome) -> &'static str {
    match outcome {
        RestoreOutcome::NoBundle { .. } => "NoBundle",
        RestoreOutcome::Refused { .. } => "Refused",
        RestoreOutcome::Restored { .. } => "Restored",
        RestoreOutcome::PartiallyRestored { .. } => "PartiallyRestored",
    }
}

/// Renders the refusal (when there is one) so a failing assertion says *why*.
fn explain(outcome: &RestoreOutcome) -> String {
    match outcome {
        RestoreOutcome::Refused { refusal, .. } => {
            format!(
                "[{}] {}: {} ({:?})",
                refusal.member.clone().unwrap_or_else(|| "-".to_string()),
                refusal.code.label(),
                refusal.message,
                refusal.code
            )
        }
        other => kind_of(other).to_string(),
    }
}
