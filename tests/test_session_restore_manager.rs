//! t-014a — **manager-history fidelity** acceptance tests, `docs/design_session_restore_data.md`
//! §9.1 AC-M1 … AC-M8, one `#[test]` per criterion (a failure names the criterion).
//!
//! Every case follows the `tests/test_session_bundle.rs` idiom: build a
//! `CapturePayload` in memory, commit it with `session::commit::write_bundle` into
//! a private temp directory (`<temp>/…/.marmel/session`), read it back with
//! `session::restore::load_bundle`, and compare the restored `ManagerSection`
//! against the committed one **as `MessageRecord`s** — never as wire forms,
//! because `types::wire::serialize_assistant_content` (`src/types/wire.rs:30-41`)
//! folds `content: None` into `""` and a wire comparison would therefore be blind
//! to the exact loss this format exists to prevent.
//!
//! What each criterion pins:
//!
//! * **AC-M1** element-by-element record equality of committed vs restored history;
//! * **AC-M2** `content: None` vs `Some("")` survive, *and* the member bytes
//!   literally spell `"content": null` for the `None` case;
//! * **AC-M3** `reasoning_content`: text, absent, and `Some("")` stay distinct;
//! * **AC-M4** `tool_calls` verbatim — count, order, `id`, the `"type"` spelling,
//!   `function.name`, and `function.arguments` **byte-identical**, including an
//!   `{"_error":"malformed_arguments",…}` payload (never re-sanitized,
//!   `src/types/wire.rs:88-108`, `:135-138`) and a non-canonical payload (never
//!   re-serialized);
//! * **AC-M5** `role:"tool"` results byte-identical with the exact `tool_call_id`,
//!   and a committed `"(aborted)"` placeholder (`ABORTED_TOOL_RESULT`,
//!   `src/manager/context.rs:327`/`:390`, `session/format/mod.rs:64`) comes back as
//!   an ordinary result instead of being re-synthesized or special-cased;
//! * **AC-M6** no re-role / no illegal re-order; §8 N2 moves are asserted as
//!   "each moved result sits directly after its own parent assistant, and every
//!   non-`Tool` turn keeps its own place";
//! * **AC-M7** the legacy `SYSTEM: CONTEXT LIMIT EXCEEDED` advisory
//!   (`src/manager/context.rs:287-309`, marker kept in
//!   `INJECTED_ADVISORY_PREFIXES`) returns byte-identical and unpruned, and a
//!   `System`-variant rebirth checkpoint stays `System` (§4 R-PIN-3);
//! * **AC-M8** the engine scalars survive, and a deliberately different `Config`
//!   budget is *reported* with both numbers, changing neither (§4 R-PIN-6).
//!
//! Hermetic: every case owns a unique subdirectory of `std::env::temp_dir()` and
//! deletes it; nothing is written into the repository; no network, no PTY, no
//! sleeps; all assertions are deterministic.

use std::path::{Path, PathBuf};

use marmennill::config::Config;
use marmennill::manager::context::{
    ABORTED_TOOL_RESULT as ENGINE_ABORTED_TOOL_RESULT, INJECTED_ADVISORY_PREFIXES,
    REBIRTH_CHECKPOINT_PREFIX, is_injected_advisory,
};
use marmennill::manager::phase::{MARMEL_DIR, Plan};
use marmennill::session::commit::{CapturePayload, CommitLock, CommitPolicy, CommitReport};
use marmennill::session::format::envelope::{IdentityFloors, ManagerSection, PinWitness};
use marmennill::session::format::{ABORTED_TOOL_RESULT, MANAGER_FILE, MANIFEST_FILE};
use marmennill::session::restore::{load_bundle, pinned_prefix_end_records};
use marmennill::session::{
    DanglingToolCallState, MessageRecord, RepairReport, RestoreOutcome, RestoredSession,
    ToolCallRecord, ToolFunctionRecord, UiSection,
};

/// Budget the fixture commits with, deliberately far from `Config::default()`
/// (8192, `src/config.rs:224`) so a silent adoption would be visible.
const COMMITTED_BUDGET: usize = 1234;

// ------------------------------------------------------------------ fixtures
//
// The same helpers as `tests/test_session_bundle.rs`, narrowed to the manager
// section: these tests pin the manager transcript, so every payload carries an
// empty worker set and the default UI section.

/// A fresh temp dir whose `.marmel/session` layout mirrors a real plan. The tag is
/// unique per test, so cases never share a directory.
fn bundle_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "marmel-t014a-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let bundle = root.join(MARMEL_DIR).join("session");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&bundle).expect("temp bundle dir");
    bundle
}

/// The temp root owning a bundle dir (two levels above `…/session`).
fn root_of(bundle: &Path) -> PathBuf {
    bundle
        .parent()
        .and_then(|p| p.parent())
        .expect("bundle hangs off <root>/.marmel/session")
        .to_path_buf()
}

fn cleanup(bundle: &Path) {
    std::fs::remove_dir_all(root_of(bundle)).ok();
}

/// A `ManagerSection` whose witnesses are exactly what the capture side would
/// record: the pin witness is the byte length of `[0]`/`[1]`, and
/// `pinned_prefix_end` is the loader's own rule (`pinned_prefix_end_records`), so
/// a fixture is never refused for a reason unrelated to the criterion under test.
fn manager_section(messages: Vec<MessageRecord>) -> ManagerSection {
    manager_section_with(messages, COMMITTED_BUDGET, false, 0)
}

fn manager_section_with(
    messages: Vec<MessageRecord>,
    max_context_tokens: usize,
    rebirth_advisory_emitted: bool,
    consecutive_rebirths: usize,
) -> ManagerSection {
    ManagerSection {
        pinned_prefix_end: pinned_prefix_end_records(&messages) as u32,
        pin_witness: PinWitness {
            system_prompt_len: messages
                .first()
                .and_then(|m| m.content())
                .map_or(0, |c| c.len()) as u32,
            goal_len: messages
                .get(1)
                .and_then(|m| m.content())
                .map_or(0, |c| c.len()) as u32,
        },
        max_context_tokens,
        rebirth_advisory_emitted,
        consecutive_rebirths,
        dangling: DanglingToolCallState::observe(&messages),
        messages,
    }
}

fn payload(messages: Vec<MessageRecord>) -> CapturePayload {
    payload_section(manager_section(messages))
}

fn payload_section(manager: ManagerSection) -> CapturePayload {
    CapturePayload {
        manager,
        ui: Some(UiSection {
            status_line: "t014a status".to_string(),
            ..UiSection::default()
        }),
        workers: Vec::new(),
        identity_floors: IdentityFloors {
            worker_id_seq_floor: 1,
            completed_seq_floor: 1,
            notice_id_floor: 1,
        },
    }
}

/// The public commit seam (`session::commit::write_bundle`), unthrottled.
fn write(bundle: &Path, payload: &CapturePayload) -> CommitReport {
    let policy = CommitPolicy {
        min_interval: std::time::Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    marmennill::session::commit::write_bundle(
        bundle,
        "session-t014a",
        payload,
        marmennill::session::ProcessEnding::InProgress,
        &policy,
        &CommitLock::new(),
    )
    .expect("commit must succeed")
}

/// Restore, insisting on a full restore: a refusal is a finding, not a fixture
/// detail, so it must fail the test with the loader's own sentence.
fn reload(bundle: &Path) -> (RestoredSession, RepairReport) {
    let outcome = load_bundle(bundle);
    let RestoreOutcome::Restored {
        session, report, ..
    } = outcome
    else {
        panic!(
            "expected a full restore of {}, got {}",
            bundle.display(),
            explain(&outcome)
        );
    };
    (session, report)
}

fn explain(outcome: &RestoreOutcome) -> String {
    match outcome {
        RestoreOutcome::NoBundle {
            bundle,
            orphan_members,
            ..
        } => format!(
            "NoBundle at {} ({} orphan member(s))",
            bundle.display(),
            orphan_members.len()
        ),
        RestoreOutcome::Refused { refusal, .. } => {
            format!("Refused [{}]: {}", refusal.code.label(), refusal.message)
        }
        RestoreOutcome::PartiallyRestored {
            unrecoverable_workers,
            ..
        } => format!("PartiallyRestored {unrecoverable_workers:?}"),
        RestoreOutcome::Restored { .. } => "Restored".to_string(),
    }
}

/// The raw bytes of a committed member, as text — the witness for every
/// "the file itself says so" assertion.
fn member_text(bundle: &Path, rel: &str) -> String {
    let path = bundle.join(rel);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    String::from_utf8(bytes).unwrap_or_else(|e| panic!("{} is not UTF-8: {e}", path.display()))
}

/// The JSON spelling of a string literal, i.e. exactly what must appear in the
/// member file for a payload to be stored verbatim rather than re-written.
fn json_literal(value: &str) -> String {
    serde_json::to_string(value).expect("a string always serializes")
}

fn assistant(
    content: Option<&str>,
    reasoning: Option<&str>,
    calls: Vec<ToolCallRecord>,
) -> MessageRecord {
    MessageRecord::Assistant {
        content: content.map(str::to_string),
        reasoning_content: reasoning.map(str::to_string),
        tool_calls: calls,
    }
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCallRecord {
    ToolCallRecord {
        id: id.to_string(),
        kind: "function".to_string(),
        function: ToolFunctionRecord {
            name: name.to_string(),
            arguments: arguments.to_string(),
        },
    }
}

fn tool(id: &str, content: &str) -> MessageRecord {
    MessageRecord::Tool {
        tool_call_id: id.to_string(),
        content: content.to_string(),
    }
}

/// The role-tag sequence, spelled as on the wire.
fn roles(messages: &[MessageRecord]) -> Vec<&'static str> {
    messages.iter().map(MessageRecord::role).collect()
}

/// The transcript with every `Tool` turn removed: what the §8 pass set is
/// forbidden to touch.
fn non_tool(messages: &[MessageRecord]) -> Vec<&MessageRecord> {
    messages
        .iter()
        .filter(|record| !matches!(record, MessageRecord::Tool { .. }))
        .collect()
}

fn index_of_call(messages: &[MessageRecord], id: &str) -> usize {
    messages
        .iter()
        .position(|record| match record {
            MessageRecord::Assistant { tool_calls, .. } => tool_calls.iter().any(|c| c.id == id),
            _ => false,
        })
        .unwrap_or_else(|| panic!("no assistant turn carries tool_call_id {id}"))
}

fn index_of_result(messages: &[MessageRecord], id: &str) -> usize {
    messages
        .iter()
        .position(|record| matches!(record, MessageRecord::Tool { tool_call_id, .. } if tool_call_id == id))
        .unwrap_or_else(|| panic!("no tool result carries tool_call_id {id}"))
}

/// Every repair counter of a restore that must be a pure no-op.
/// §8 N2's only legal re-order: a `Tool` result may move, but it must land in the
/// run of results that belongs to **its own** parent assistant — a block starting
/// directly after that assistant, covering exactly its calls, in call order.
fn assert_results_grouped_with_parent(messages: &[MessageRecord]) {
    for (index, record) in messages.iter().enumerate() {
        let MessageRecord::Assistant { tool_calls, .. } = record else {
            continue;
        };
        if tool_calls.is_empty() {
            continue;
        }
        let expected: Vec<&str> = tool_calls.iter().map(|call| call.id.as_str()).collect();
        let mut got: Vec<&str> = Vec::new();
        let mut cursor = index + 1;
        while let Some(MessageRecord::Tool { tool_call_id, .. }) = messages.get(cursor) {
            got.push(tool_call_id.as_str());
            cursor += 1;
        }
        assert_eq!(
            got, expected,
            "the tool-result block after messages[{index}] must be that assistant's own\
             calls, in call order"
        );
    }
}

fn assert_untouched_by_repairs(report: &RepairReport, expected_len: usize) {
    assert_eq!(
        report.manager_messages_before, report.manager_messages_after,
        "the pass set changed the transcript length {} -> {}",
        report.manager_messages_before, report.manager_messages_after
    );
    assert_eq!(report.manager_messages_after, expected_len);
    assert_eq!(report.orphan_tool_results_pruned, 0);
    assert_eq!(report.aborted_results_synthesized, 0);
    assert_eq!(report.tool_results_regrouped, 0);
    assert!(report.normalization_stable);
    assert_eq!(
        report.normalization_passes, 2,
        "A §8.2 N5 runs the pass set twice"
    );
    assert!(report.pins_verified);
}

// ------------------------------------------------------- AC-M1 (design §9.1 1)

/// The committed manager history and the restored one are equal **element by
/// element** as `MessageRecord`s.
#[test]
fn ac_m1_restored_manager_history_equals_committed_element_by_element() {
    let bundle = bundle_dir("ac-m1");
    let committed = vec![
        MessageRecord::System {
            content: "t014a system prompt".to_string(),
        },
        MessageRecord::User {
            content: "t014a goal: pin the transcript".to_string(),
        },
        assistant(
            None,
            Some("decide which file to read"),
            vec![call(
                "m1-1",
                marmennill::tool_names::TOOL_READ_FILE,
                "{\"path\":\"src/main.rs\"}",
            )],
        ),
        tool("m1-1", "file bytes"),
        assistant(Some(""), None, Vec::new()),
        MessageRecord::User {
            content: "steer: keep going".to_string(),
        },
        assistant(Some("MISSION COMPLETE"), None, Vec::new()),
    ];
    let original = payload(committed.clone());
    write(&bundle, &original);

    let (session, report) = reload(&bundle);
    let restored = &session.manager.messages;

    assert_eq!(restored.len(), committed.len(), "element count must match");
    for (index, (was, now)) in committed.iter().zip(restored.iter()).enumerate() {
        assert_eq!(was, now, "messages[{index}] is not the same MessageRecord");
        assert_eq!(was.role(), now.role(), "messages[{index}] changed role tag");
    }
    assert_eq!(
        &session.manager, &original.manager,
        "the whole section matches"
    );
    assert_eq!(
        roles(restored),
        roles(&committed),
        "role-tag sequence identical"
    );
    assert_untouched_by_repairs(&report, committed.len());

    // The comparison above is on records, not wire forms: `[2]` is the turn the
    // wire type cannot represent, and it is still `None` here.
    let MessageRecord::Assistant { content, .. } = &restored[2] else {
        panic!("messages[2] must be an assistant turn");
    };
    assert_eq!(content, &None, "record-level comparison, not wire-level");
    cleanup(&bundle);
}

// ------------------------------------------------------- AC-M2 (design §9.1 2)

/// `content: None` and `content: Some("")` are two different facts, and the
/// **member bytes** say so (`"content": null` vs `"content": ""`).
#[test]
fn ac_m2_content_none_vs_empty_string_survive() {
    let bundle = bundle_dir("ac-m2");
    let committed = vec![
        MessageRecord::System {
            content: "t014a system prompt".to_string(),
        },
        MessageRecord::User {
            content: "t014a goal".to_string(),
        },
        // The tool-only assistant turn: no content at all, exactly one call.
        assistant(
            None,
            None,
            vec![call(
                "m2-none",
                marmennill::tool_names::TOOL_READ_FILE,
                "{\"path\":\"README.md\"}",
            )],
        ),
        tool("m2-none", "bytes"),
        // A turn whose content was produced and is the empty string.
        assistant(Some(""), None, Vec::new()),
    ];
    write(&bundle, &payload(committed.clone()));

    let (session, report) = reload(&bundle);
    let restored = &session.manager.messages;

    let MessageRecord::Assistant {
        content: none_case,
        tool_calls,
        ..
    } = &restored[2]
    else {
        panic!("messages[2] must be the tool-only assistant turn");
    };
    assert_eq!(none_case, &None, "content: None must not degrade into \"\"");
    assert_eq!(tool_calls.len(), 1, "its single call must still be there");

    let MessageRecord::Assistant {
        content: empty_case,
        ..
    } = &restored[4]
    else {
        panic!("messages[4] must be the empty-content assistant turn");
    };
    assert_eq!(
        empty_case,
        &Some(String::new()),
        "content: \"\" must not become None"
    );
    assert_eq!(restored, &committed, "the whole history is unchanged");
    assert_untouched_by_repairs(&report, committed.len());

    // The assertion the criterion asks for: the serialized member, not only the
    // decoded value. `session::commit` writes members with `to_vec_pretty`
    // (`src/session/commit.rs:259`), so `null` is spelled with one space.
    let text = member_text(&bundle, MANAGER_FILE);
    assert_eq!(
        text.matches(r#""content": null"#).count(),
        1,
        "the None turn must be stored as \"content\": null in {MANAGER_FILE}"
    );
    assert_eq!(
        text.matches(r#""content": """#).count(),
        1,
        "the empty-content turn must be stored as \"content\": \"\""
    );

    // Why the member bytes, and not the wire form, are the authority: the very
    // same record, handed to the wire type, loses the distinction — this is the
    // trap `src/types/wire.rs:30-41` documents.
    let wire = restored[2].clone().into_wire();
    let wire_json = serde_json::to_string(&wire).expect("a wire message serializes");
    assert!(
        wire_json.contains(r#""content":"""#),
        "the wire serializer folds None into \"\" — which is exactly why the \
         bundle must not go through it, got {wire_json}"
    );
    assert!(
        !wire_json.contains(r#""content":null"#),
        "no wire round-trip could ever restore None, got {wire_json}"
    );
    cleanup(&bundle);
}

// ------------------------------------------------------- AC-M3 (design §9.1 3)

/// `reasoning_content`: text stays text, absent stays absent (the key is not even
/// written), and `Some("")` stays `Some("")`.
#[test]
fn ac_m3_reasoning_content_survives() {
    let bundle = bundle_dir("ac-m3");
    let text_reasoning = "step 1: read; step 2: edit — \"quoted\", \\ back, äöæ ✓";
    let committed = vec![
        MessageRecord::System {
            content: "t014a system prompt".to_string(),
        },
        MessageRecord::User {
            content: "t014a goal".to_string(),
        },
        assistant(Some("first"), Some(text_reasoning), Vec::new()),
        assistant(Some("second"), None, Vec::new()),
        assistant(Some("third"), Some(""), Vec::new()),
    ];
    write(&bundle, &payload(committed.clone()));

    let (session, report) = reload(&bundle);
    let restored = &session.manager.messages;
    let MessageRecord::Assistant {
        reasoning_content: first,
        ..
    } = &restored[2]
    else {
        panic!("messages[2] must be an assistant turn");
    };
    assert_eq!(
        first.as_deref(),
        Some(text_reasoning),
        "Some(text) must come back byte-identical"
    );
    assert_eq!(first.as_ref().map(String::len), Some(text_reasoning.len()));

    let MessageRecord::Assistant {
        reasoning_content: second,
        ..
    } = &restored[3]
    else {
        panic!("messages[3] must be an assistant turn");
    };
    assert_eq!(second, &None, "absent reasoning_content must stay absent");

    let MessageRecord::Assistant {
        reasoning_content: third,
        ..
    } = &restored[4]
    else {
        panic!("messages[4] must be an assistant turn");
    };
    assert_eq!(
        third,
        &Some(String::new()),
        "an empty reasoning string must stay Some(\"\"), not become absent"
    );

    assert_eq!(restored, &committed, "the whole history is unchanged");
    assert_untouched_by_repairs(&report, committed.len());

    // Member-level: exactly two `reasoning_content` keys — the absent turn must
    // not be materialized as `null`.
    let text = member_text(&bundle, MANAGER_FILE);
    assert_eq!(
        text.matches("\"reasoning_content\":").count(),
        2,
        "absent must stay absent in the member bytes"
    );
    assert!(
        text.contains(&json_literal(text_reasoning)),
        "the reasoning text must be stored verbatim"
    );
    assert!(
        text.contains(r#""reasoning_content": """#),
        "the empty reasoning string must be stored as \"\""
    );
    cleanup(&bundle);
}

// ------------------------------------------------------- AC-M4 (design §9.1 4)

/// `tool_calls` come back verbatim: same count, same order, same `id`, the `type`
/// key spelled exactly `"function"`, same `function.name`, and
/// `function.arguments` **byte-identical** — including a payload that already
/// carries `{"_error":"malformed_arguments",…}` and one with non-canonical key
/// order and whitespace.
#[test]
fn ac_m4_tool_calls_survive_verbatim() {
    let bundle = bundle_dir("ac-m4");
    // Ordered as the model emitted them. Each one is a trap for a different
    // prohibited rewrite of history.
    let plain = "{\"path\":\"src/main.rs\",\"offset\":0}";
    // Non-canonical key order + irregular whitespace: a `from_str` → re-serialize
    // round-trip would sort the keys and re-flow the spacing.
    let noncanonical =
        "{ \"z\" : 1,  \"a\":2, \"nested\": { \"y\": true ,\"x\": null }, \"m\": \"a\\nb\" }";
    // Already healed by the live boundary repair, with padding: `
    // ensure_valid_json_arguments` would TRIM it (`src/types/wire.rs:88-108`),
    // `sanitize_arguments` would rewrite it (`:135-138`).
    let already_malformed = "  {\"_error\":\"malformed_arguments\",\"_raw\":\"{oops\"}  ";
    // Not JSON at all: a sanitizer would replace it with an `_error` envelope.
    let unterminated = "{oops";

    let committed = vec![
        MessageRecord::System {
            content: "t014a system prompt".to_string(),
        },
        MessageRecord::User {
            content: "t014a goal".to_string(),
        },
        assistant(
            Some("four calls"),
            None,
            vec![
                call("m4-1", marmennill::tool_names::TOOL_READ_FILE, plain),
                call("m4-2", marmennill::tool_names::TOOL_REPLACE, noncanonical),
                call(
                    "m4-3",
                    marmennill::tool_names::TOOL_GREP_SEARCH,
                    already_malformed,
                ),
                call(
                    "m4-4",
                    marmennill::tool_names::TOOL_WRITE_FILE,
                    unterminated,
                ),
            ],
        ),
        tool("m4-1", "ok-1"),
        tool("m4-2", "ok-2"),
        tool("m4-3", "ok-3"),
        tool("m4-4", "ok-4"),
    ];
    write(&bundle, &payload(committed.clone()));

    let (session, report) = reload(&bundle);
    let restored = &session.manager.messages;
    assert_eq!(restored, &committed, "the whole history is unchanged");
    assert_untouched_by_repairs(&report, committed.len());

    let MessageRecord::Assistant { tool_calls, .. } = &restored[2] else {
        panic!("messages[2] must carry the tool calls");
    };
    let committed_calls = match &committed[2] {
        MessageRecord::Assistant { tool_calls, .. } => tool_calls,
        other => panic!("fixture bug: {other:?}"),
    };

    // Count and order.
    assert_eq!(
        tool_calls.len(),
        committed_calls.len(),
        "call count must survive"
    );
    let ids: Vec<&str> = tool_calls.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["m4-1", "m4-2", "m4-3", "m4-4"],
        "same ids, same order"
    );

    for (was, now) in committed_calls.iter().zip(tool_calls.iter()) {
        // The `type` key's own value, spelled exactly like the wire default.
        assert_eq!(&now.kind, "function", "type must stay spelled \"function\"");
        assert_eq!(
            now.function.name, was.function.name,
            "function.name changed for {}",
            was.id
        );
        assert_eq!(
            now.function.arguments.as_bytes(),
            was.function.arguments.as_bytes(),
            "function.arguments for {} must be BYTE-identical",
            was.id
        );
    }

    // The two traps, individually.
    let healed = &tool_calls[2].function.arguments;
    assert_eq!(healed, already_malformed, "not re-sanitized");
    assert!(
        healed.starts_with(' ') && healed.ends_with(' '),
        "the padding a sanitizer would have trimmed is still there: {healed:?}"
    );
    assert!(
        healed.contains(r#""_error":"malformed_arguments""#),
        "the payload the live repair wrote must be kept, not re-derived: {healed}"
    );

    let raw = &tool_calls[3].function.arguments;
    assert_eq!(raw, unterminated, "a truncated payload is kept as it was");
    assert!(
        !raw.contains("_error"),
        "the restore must NOT re-sanitize `{{oops` into an _error envelope: {raw}"
    );

    let shuffled = &tool_calls[1].function.arguments;
    assert_eq!(shuffled, noncanonical, "not re-serialized");
    assert!(
        shuffled.contains("\"z\" : 1") && shuffled.contains("\"y\": true ,"),
        "non-canonical key order and whitespace must survive: {shuffled}"
    );

    // `is_malformed` is a read-out, never a repair (`format/messages.rs`): it
    // reports the two broken payloads and leaves them byte-identical.
    assert!(!tool_calls[0].is_malformed());
    assert!(tool_calls[2].is_malformed() && tool_calls[3].is_malformed());
    let again = ToolCallRecord::from_wire(&tool_calls[3].clone().into_wire());
    assert_eq!(
        again.function.arguments.as_bytes(),
        unterminated.as_bytes(),
        "even the in-memory wire conversion must not touch the bytes"
    );

    // Member bytes: the `type` spelling and each payload's exact JSON literal.
    let text = member_text(&bundle, MANAGER_FILE);
    assert_eq!(
        text.matches(r#""type": "function""#).count(),
        committed_calls.len(),
        "every call is written with a literal \"type\": \"function\" key"
    );
    for arguments in [plain, noncanonical, already_malformed, unterminated] {
        assert!(
            text.contains(&json_literal(arguments)),
            "{MANAGER_FILE} must contain the payload verbatim: {arguments:?}"
        );
    }
    cleanup(&bundle);
}

// ------------------------------------------------------- AC-M5 (design §9.1 5)

/// `role:"tool"` results come back byte-identical with the exact `tool_call_id`,
/// and the `"(aborted)"` placeholder left by the live boundary repair is an
/// ordinary result — never re-synthesized, never treated as a marker.
#[test]
fn ac_m5_tool_results_survive_byte_identical() {
    let bundle = bundle_dir("ac-m5");
    let nasty = "line one\nline two\ttabbed \"quoted\" \\ backslash \\ äöæ ✓ \u{1f702}";
    let committed = vec![
        MessageRecord::System {
            content: "t014a system prompt".to_string(),
        },
        MessageRecord::User {
            content: "t014a goal".to_string(),
        },
        assistant(
            None,
            None,
            vec![
                call(
                    "m5-a",
                    marmennill::tool_names::TOOL_GREP_SEARCH,
                    "{\"pattern\":\"x\"}",
                ),
                call(
                    "m5-b",
                    marmennill::tool_names::TOOL_GLOB,
                    "{\"pattern\":\"*.rs\"}",
                ),
                call(
                    "m5-c",
                    marmennill::tool_names::TOOL_SLEEP,
                    "{\"seconds\":1}",
                ),
            ],
        ),
        tool("m5-a", nasty),
        tool("m5-b", ""),
        // Committed by the live boundary repair (`context.rs:327`/`:390`), not by
        // the restore: after this point the transcript was already paired.
        tool("m5-c", ABORTED_TOOL_RESULT),
    ];
    write(&bundle, &payload(committed.clone()));

    let (session, report) = reload(&bundle);
    let restored = &session.manager.messages;
    assert_eq!(restored, &committed, "every result survives byte-identical");
    assert_untouched_by_repairs(&report, committed.len());
    assert_eq!(
        report.aborted_results_synthesized, 0,
        "a committed (aborted) result must never be re-synthesized"
    );

    for (id, content) in [("m5-a", nasty), ("m5-b", ""), ("m5-c", ABORTED_TOOL_RESULT)] {
        let index = index_of_result(restored, id);
        let MessageRecord::Tool {
            tool_call_id,
            content: got,
        } = &restored[index]
        else {
            panic!("messages[{index}] must be a tool result");
        };
        assert_eq!(tool_call_id, id, "exact tool_call_id preserved");
        assert_eq!(
            got.as_bytes(),
            content.as_bytes(),
            "result content for {id} must be byte-identical"
        );
        assert_eq!(got.len(), content.len(), "no re-encoding for {id}");
    }

    // The placeholder is an ordinary result: same variant, same shape as its two
    // siblings, and it still sits directly under its own assistant turn.
    let aborted_index = index_of_result(restored, "m5-c");
    let MessageRecord::Tool { content, .. } = &restored[aborted_index] else {
        panic!("the aborted turn must be a tool result");
    };
    assert_eq!(content, ABORTED_TOOL_RESULT);
    assert_eq!(
        content.as_str(),
        ENGINE_ABORTED_TOOL_RESULT,
        "both owners spell the placeholder identically"
    );
    assert_eq!(
        aborted_index,
        index_of_call(restored, "m5-c") + 3,
        "it is an ordinary result inside its parent's result block"
    );
    assert!(
        !report
            .warnings
            .iter()
            .any(|warning| warning.contains("m5-c")),
        "nothing was repaired, so nothing may be announced about m5-c: {:?}",
        report.warnings
    );

    // Member bytes: the awkward content is stored in its JSON literal form, and
    // the empty result is `""`, not absent and not null.
    let text = member_text(&bundle, MANAGER_FILE);
    assert!(
        text.contains(&json_literal(nasty)),
        "the byte-awkward result must be stored verbatim"
    );
    assert!(
        text.contains(r#""content": """#),
        "an empty tool result must be stored as \"\""
    );
    assert!(
        text.contains(&json_literal(ABORTED_TOOL_RESULT)),
        "the aborted placeholder is stored like any other result"
    );
    cleanup(&bundle);
}

// ------------------------------------------------------- AC-M6 (design §9.1 6)

/// No re-role, no re-order. `Tool` results may move (that is §8 N2) — and then
/// each moved result must sit directly after its own parent assistant, while
/// every non-`Tool` turn keeps its own place and identity.
#[test]
fn ac_m6_no_re_role_and_no_illegal_reorder() {
    // ---------- case A: the moves are inside one result block ----------
    let bundle = bundle_dir("ac-m6-a");
    let committed = vec![
        MessageRecord::System {
            content: "t014a system prompt".to_string(),
        },
        MessageRecord::User {
            content: "t014a goal".to_string(),
        },
        assistant(
            Some("two calls"),
            None,
            vec![
                call(
                    "m6-x",
                    marmennill::tool_names::TOOL_READ_FILE,
                    "{\"path\":\"a.rs\"}",
                ),
                call(
                    "m6-y",
                    marmennill::tool_names::TOOL_READ_FILE,
                    "{\"path\":\"b.rs\"}",
                ),
            ],
        ),
        // Results answering the calls in the opposite order.
        tool("m6-y", "b bytes"),
        tool("m6-x", "a bytes"),
    ];
    write(&bundle, &payload(committed.clone()));

    let (session, report) = reload(&bundle);
    let restored = &session.manager.messages;

    assert_eq!(restored.len(), committed.len(), "nothing added or dropped");
    assert_eq!(
        roles(restored),
        roles(&committed),
        "the role-tag sequence is identical"
    );
    assert_eq!(non_tool(restored), non_tool(&committed));
    // Pure `Tool` permutation: every non-`Tool` index is literally unchanged.
    for (index, record) in committed.iter().enumerate() {
        if !matches!(record, MessageRecord::Tool { .. }) {
            assert_eq!(
                restored.get(index),
                Some(record),
                "non-tool messages[{index}] moved"
            );
        }
    }
    assert_eq!(report.orphan_tool_results_pruned, 0);
    assert_eq!(report.aborted_results_synthesized, 0);
    assert_eq!(
        report.tool_results_regrouped, 2,
        "both results were re-grouped into call order"
    );
    assert!(report.normalization_stable);
    assert_results_grouped_with_parent(restored);
    for id in ["m6-x", "m6-y"] {
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("regrouped") && warning.contains(id)),
            "A §8.4: a move must be named out loud, got {:?}",
            report.warnings
        );
    }
    // Moved, never rewritten: the two results still carry their own bytes.
    assert_eq!(
        restored[index_of_result(restored, "m6-x")],
        committed[index_of_result(&committed, "m6-x")]
    );
    assert_eq!(
        restored[index_of_result(restored, "m6-y")],
        committed[index_of_result(&committed, "m6-y")]
    );
    cleanup(&bundle);

    // ---------- case B: one result stranded behind a later turn ----------
    let bundle = bundle_dir("ac-m6-b");
    let committed = vec![
        MessageRecord::System {
            content: "t014a system prompt".to_string(),
        },
        MessageRecord::User {
            content: "t014a goal".to_string(),
        },
        assistant(
            Some("working"),
            None,
            vec![call(
                "m6-1",
                marmennill::tool_names::TOOL_READ_FILE,
                "{\"path\":\"c.rs\"}",
            )],
        ),
        MessageRecord::User {
            content: "steer: hurry up".to_string(),
        },
        tool("m6-1", "late result"),
    ];
    write(&bundle, &payload(committed.clone()));

    let (session, report) = reload(&bundle);
    let restored = &session.manager.messages;

    assert_eq!(restored.len(), committed.len(), "no turn was dropped");
    assert_eq!(report.orphan_tool_results_pruned, 0);
    assert_eq!(report.aborted_results_synthesized, 0);
    assert_eq!(report.tool_results_regrouped, 1);

    // No re-role: the role-tag multiset is untouched, and the tag of every turn
    // is still the tag it was committed with.
    let mut was_roles = roles(&committed);
    let mut now_roles = roles(restored);
    was_roles.sort_unstable();
    now_roles.sort_unstable();
    assert_eq!(
        now_roles, was_roles,
        "the pass set may not re-role or drop a turn"
    );
    assert_eq!(
        non_tool(restored),
        non_tool(&committed),
        "every non-tool turn is identical, in the same order"
    );
    // Index within the non-tool subsequence is the honest reading of "the
    // non-Tool turns keep their places": only `Tool` messages moved past them.
    for (index, record) in non_tool(&committed).iter().enumerate() {
        assert_eq!(
            &non_tool(restored)[index],
            record,
            "non-tool turn #{index} moved"
        );
    }
    assert_eq!(
        roles(restored),
        vec!["system", "user", "assistant", "tool", "user"],
        "the stranded result moved up, nothing else re-roled"
    );
    assert_eq!(
        index_of_result(restored, "m6-1"),
        index_of_call(restored, "m6-1") + 1,
        "the moved result sits directly after its own parent assistant"
    );
    assert_eq!(
        restored[index_of_result(restored, "m6-1")],
        committed[index_of_result(&committed, "m6-1")],
        "moved, never rewritten"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("regrouped tool result m6-1")),
        "the move is reported with its ids and indices, got {:?}",
        report.warnings
    );
    cleanup(&bundle);
}

// ------------------------------------------------------- AC-M7 (design §9.1 7)

/// The legacy `SYSTEM: CONTEXT LIMIT EXCEEDED` advisory is opaque bytes: it comes
/// back byte-identical and unpruned, and a `System`-variant rebirth checkpoint
/// stays `System` (§4 R-PIN-3/R-PIN-4).
#[test]
fn ac_m7_legacy_advisory_text_and_system_checkpoint_are_opaque() {
    let bundle = bundle_dir("ac-m7");
    // The marker the current binary no longer emits but old bundles carry
    // (`context.rs:287-309`, kept in `INJECTED_ADVISORY_PREFIXES:367-372`).
    let advisory = "(SYSTEM: CONTEXT LIMIT EXCEEDED — compaction dropped 12 older \
                    turn(s).\nContinue from the last genuine instruction.)";
    // The historic `System` spelling of a rebirth checkpoint (the current binary
    // emits the `User` spelling, bug M12).
    let checkpoint = format!("{REBIRTH_CHECKPOINT_PREFIX}3 turns distilled: goal pinned.)");
    let committed = vec![
        MessageRecord::System {
            content: "t014a system prompt".to_string(),
        },
        MessageRecord::User {
            content: "t014a goal".to_string(),
        },
        assistant(
            None,
            None,
            vec![call(
                "m7-1",
                marmennill::tool_names::TOOL_READ_FILE,
                "{\"path\":\"d.rs\"}",
            )],
        ),
        tool("m7-1", "d bytes"),
        MessageRecord::System {
            content: checkpoint.clone(),
        },
        MessageRecord::User {
            content: advisory.to_string(),
        },
        assistant(Some("continuing after the advisory"), None, Vec::new()),
    ];
    write(&bundle, &payload(committed.clone()));

    // The fixture really does carry both markers — otherwise this test would be
    // asserting nothing about the grammar it names.
    assert!(advisory.starts_with("(SYSTEM: CONTEXT LIMIT EXCEEDED"));
    assert!(
        INJECTED_ADVISORY_PREFIXES
            .iter()
            .any(|prefix| advisory.starts_with(prefix))
    );
    assert!(is_injected_advisory(advisory));
    assert!(is_injected_advisory(&checkpoint));

    let (session, report) = reload(&bundle);
    let restored = &session.manager.messages;

    assert_eq!(restored.len(), committed.len(), "nothing was pruned");
    assert_eq!(restored, &committed, "every turn is byte-identical");
    assert_untouched_by_repairs(&report, committed.len());

    let MessageRecord::User { content } = &restored[5] else {
        panic!(
            "the legacy advisory must come back as the same turn, got {:?}",
            restored[5]
        )
    };
    assert_eq!(
        content.as_bytes(),
        advisory.as_bytes(),
        "the legacy advisory text is opaque: never trimmed, reworded or pruned"
    );
    assert!(
        is_injected_advisory(content),
        "still recognizable as injected"
    );

    let MessageRecord::System { content } = &restored[4] else {
        panic!(
            "R-PIN-3: a System-variant checkpoint must NOT be re-roled, got {:?}",
            restored[4]
        )
    };
    assert_eq!(content.as_bytes(), checkpoint.as_bytes());

    // The pinned window is the loader's own rule, and it was neither moved nor
    // silently re-derived: it extends through the checkpoint at [4].
    assert_eq!(session.manager.pinned_prefix_end, 5);
    assert_eq!(report.pinned_prefix_end_expected, Some(5));
    assert_eq!(report.pinned_prefix_end_actual, Some(5));
    assert!(report.pins_verified);
    assert!(
        report.warnings.is_empty(),
        "a clean restore announces no repair: {:?}",
        report.warnings
    );

    // Member bytes carry the advisory verbatim, in its JSON literal form.
    let text = member_text(&bundle, MANAGER_FILE);
    assert!(
        text.contains(&json_literal(advisory)),
        "the advisory is stored as it was committed"
    );
    assert!(text.contains(&json_literal(&checkpoint)));
    cleanup(&bundle);
}

// ------------------------------------------------------- AC-M8 (design §9.1 8)

/// The engine scalars survive the bundle, and a deliberately different `Config`
/// budget produces a `warnings` entry naming **both** numbers while changing
/// neither (§4 R-PIN-6).
#[test]
fn ac_m8_engine_scalars_survive_and_budget_drift_is_reported() {
    const STORED_BUDGET: usize = COMMITTED_BUDGET; // 1234
    const LIVE_BUDGET: usize = 4321;

    let bundle = bundle_dir("ac-m8");
    let committed = vec![
        MessageRecord::System {
            content: "t014a system prompt".to_string(),
        },
        MessageRecord::User {
            content: "t014a goal".to_string(),
        },
        assistant(
            Some("one call"),
            None,
            vec![call(
                "m8-1",
                marmennill::tool_names::TOOL_READ_FILE,
                "{\"path\":\"e.rs\"}",
            )],
        ),
        tool("m8-1", "e bytes"),
    ];
    let section = manager_section_with(committed.clone(), STORED_BUDGET, true, 3);
    let original = payload_section(section.clone());
    write(&bundle, &original);

    // ---------- the scalars themselves ----------
    let (session, report) = reload(&bundle);
    assert_eq!(session.manager.max_context_tokens, STORED_BUDGET);
    assert!(
        session.manager.rebirth_advisory_emitted,
        "the rebirth advisory latch must survive, or the advisory re-fires"
    );
    assert_eq!(session.manager.consecutive_rebirths, 3);
    assert_eq!(
        &session.manager, &section,
        "the whole manager section matches"
    );
    assert_untouched_by_repairs(&report, committed.len());

    // The install seam keeps the scalars and does NOT adopt the stored budget.
    let mut engine = marmennill::manager::ContextEngine::new(LIVE_BUDGET);
    engine.install_restored_messages(session.manager.messages.clone());
    engine.restore_engine_flags(
        session.manager.rebirth_advisory_emitted,
        session.manager.consecutive_rebirths,
    );
    assert_eq!(engine.max_context_tokens(), LIVE_BUDGET);
    assert!(engine.rebirth_advisory_emitted());
    assert_eq!(engine.consecutive_rebirths(), 3);
    assert_eq!(engine.messages().len(), committed.len());

    // ---------- a different Config budget: reported, never reconciled ----------
    // `ui::session::boot` is the public seam that owns the R-PIN-6 comparison
    // (`src/ui/session.rs:1219-1226`): `load_bundle` alone has no Config.
    let cfg = Config {
        max_context_tokens: LIVE_BUDGET,
        ..Config::default()
    };
    let plan = Plan::at(root_of(&bundle).join(MARMEL_DIR));
    assert_eq!(
        marmennill::session::bundle_dir(&plan),
        bundle,
        "the fixture bundle is exactly where the boot pass looks"
    );

    let mut boot_engine = marmennill::ui::session::build_restored_context(&cfg);
    let mut renderer = BootRenderer::default();
    let mut transcript = marmennill::ui::UiTranscript::new();
    let mut session_id = String::new();
    let mut sink = marmennill::ui::session::BootSink {
        ui_transcript: &mut transcript,
    };
    let boot = marmennill::ui::session::boot(
        &plan,
        &cfg,
        &mut boot_engine,
        &mut renderer,
        &mut session_id,
        &mut sink,
    )
    .expect("a sound bundle must boot, not refuse");

    let warnings = &boot.repair.warnings;
    let drift = warnings
        .iter()
        .find(|warning| warning.contains("max_context_tokens"))
        .unwrap_or_else(|| panic!("R-PIN-6: the budget drift must be reported, got {warnings:?}"));
    assert!(
        drift.contains(&STORED_BUDGET.to_string()) && drift.contains(&LIVE_BUDGET.to_string()),
        "the sentence must name BOTH numbers: {drift}"
    );

    // Neither number changed: the live engine budgets against the live Config…
    assert_eq!(boot_engine.max_context_tokens(), LIVE_BUDGET);
    // …and the stored number is still the stored number, on disk and in memory.
    assert_eq!(session.manager.max_context_tokens, STORED_BUDGET);
    let (again, _) = reload(&bundle);
    assert_eq!(again.manager.max_context_tokens, STORED_BUDGET);
    assert!(
        again.manager.rebirth_advisory_emitted,
        "the advisory flag survives"
    );
    assert_eq!(again.manager.consecutive_rebirths, 3);
    // The bundle bytes were never rewritten to agree with either budget (A D6).
    let manifest = member_text(&bundle, MANIFEST_FILE);
    let member = member_text(&bundle, MANAGER_FILE);
    assert!(
        member.contains(&format!("\"max_context_tokens\": {STORED_BUDGET}")),
        "the committed budget is untouched on disk"
    );
    assert!(!member.contains(&format!("\"max_context_tokens\": {LIVE_BUDGET}")));
    assert!(manifest.contains("session-t014a"));
    // And the scalars the boot pass installed are the stored ones.
    assert!(boot_engine.rebirth_advisory_emitted());
    assert_eq!(boot_engine.consecutive_rebirths(), 3);
    assert!(
        renderer
            .statuses()
            .iter()
            .any(|line| line.contains(&session_id)),
        "the restore is announced, not silent"
    );
    cleanup(&bundle);
}

// ------------------------------------------------------------------ renderer

/// Minimal `Renderer` for the boot pass: records every event so "loud" can be
/// asserted, and supplies a pane model for `capture_ui`.
#[derive(Default)]
struct BootRenderer {
    events: Vec<marmennill::ui::Event>,
    input_state: marmennill::ui::InputState,
}

impl BootRenderer {
    fn statuses(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                marmennill::ui::Event::Status(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

impl marmennill::ui::Renderer for BootRenderer {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &marmennill::ui::Event) {
        self.events.push(event.clone());
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn input_state(&mut self) -> &mut marmennill::ui::InputState {
        &mut self.input_state
    }
    fn input_state_shared(&self) -> &marmennill::ui::InputState {
        &self.input_state
    }
    fn shutdown(&mut self) {}
}
