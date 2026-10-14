//! t-014b — the two acceptance families of `docs/design_session_restore_data.md`
//! that decide whether a restored session is still *the same session*:
//!
//! * **§9.2 AC-P1 … AC-P5 — the pinned prefix survives the boot path**
//!   (`AC-P1` asserts *after* `ui::session::boot` has run in full, which is what
//!   proves the two pin writers — `ContextEngine::set_system_prompt`
//!   (`src/manager/context.rs:592`) and `ContextEngine::set_goal` (`:604`) — were
//!   skipped. **No test in this file ever calls either of them**; the only engine
//!   that gets a transcript is `build_restored_context` + `boot`.);
//! * **§9.10 AC-D1 … AC-D7 — the dangling tool call survives honestly**: the
//!   unanswered assistant turn is kept with `content: null`, the un-dispatched
//!   call gets one named `ABORTED_TOOL_RESULT` placeholder, an already-dispatched
//!   call becomes a `NonRestorableNotice { kind: PtyProcess }` and is **never**
//!   re-run, an orphan result is pruned and named, the bundle bytes are never
//!   rewritten, and the single-corrupt-worker tolerance degrades loudly instead of
//!   pretending.
//!
//! Fixture idiom is the one already used by `tests/test_session_bundle.rs` and
//! `tests/test_session_restore_manager.rs`: build a `CapturePayload`, commit it
//! with `session::commit::write_bundle` into a private temp directory
//! (`<temp>/marmel-t014b-<tag>-<pid>-<nanos>/.marmel/session`), then read it back
//! through `session::restore::load_bundle` and — for AC-P1…AC-P4 — through the
//! real boot path `ui::session::boot`.
//!
//! Two rules this file never breaks:
//!
//! * **one hasher.** Every "the bytes did not change" assertion re-uses the
//!   crate's own `session::fingerprint` (§7.2) over the member file, including for
//!   `manifest.json` — no second hash function is introduced here (§7.3 L9);
//! * **no weakened assertion.** Where a criterion and the implementation disagree,
//!   the test keeps the criterion's wording and is marked `#[ignore]`, never
//!   relaxed.
//!
//! Hermetic: one unique temp directory per test, deleted at the end; no network,
//! no PTY, no sleeps, no global state that another test could observe (the
//! dangling/partial cases never enter the boot path, so `install_restored_workers`
//! is never reached with fixtures of their own).

use std::path::{Path, PathBuf};

use marmennill::agents::Agent;
use marmennill::config::Config;
use marmennill::manager::context::REBIRTH_CHECKPOINT_PREFIX;
use marmennill::manager::phase::{MARMEL_DIR, Plan};
use marmennill::session::commit::{
    CapturePayload, CommitLock, CommitPolicy, CommitReport, fingerprint, worker_rel_path,
};
use marmennill::session::format::TimePoint;
use marmennill::session::format::envelope::{IdentityFloors, ManagerSection, Manifest, PinWitness};
use marmennill::session::format::{ABORTED_TOOL_RESULT, MANAGER_FILE, MANIFEST_FILE};
use marmennill::session::restore::{load_bundle, pinned_prefix_end_records};
use marmennill::session::{
    DanglingCall, DanglingToolCallState, HistorySource, MessageRecord, NonRestorableKind,
    NonRestorableNotice, ProcessEnding, RefusalCode, RepairReport, RestoreOutcome, RestoredSession,
    ToolCallRecord, ToolFunctionRecord, UiSection, WorkerSection, WorkerStatus,
};
use marmennill::types::Message;

// ------------------------------------------------------------------ fixtures

/// The pinned system prompt as the *previous* process sent it (REQ-CORE-001).
/// Deliberately nothing like the prompt this process composes, so AC-P4 can prove
/// AC-P1 is not a tautology.
const PIN_TEXT: &str = "PINNED SYSTEM PROMPT (REQ-CORE-001): the exact bytes the previous process sent first, kept byte-for-byte.";

/// The pinned goal as the previous process pinned it (REQ-CORE-002).
const GOAL_TEXT: &str = "PINNED GOAL (REQ-CORE-002): keep the prefix identical across the restart.";

/// Budget the fixture commits with — far from `Config::default()` so the boot
/// pass has a live budget of its own and the pinned bytes cannot be confused with
/// a re-composition.
const COMMITTED_BUDGET: usize = 1234;

/// Budget this process boots with.
const LIVE_BUDGET: usize = 4321;

/// Monotonic floors for fixtures: every fixture worker keeps `worker_id_seq_value`
/// and `completed_seq` strictly below these, so L7 (§5.7) never refuses for a
/// reason unrelated to the criterion under test.
const SEQ_FLOOR: u64 = 10;

/// A fresh temp dir whose `.marmel/session` layout mirrors a real plan. The tag is
/// unique per test, so cases never share a directory.
fn bundle_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "marmel-t014b-{tag}-{}-{}",
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

fn sys(content: &str) -> MessageRecord {
    MessageRecord::System {
        content: content.to_string(),
    }
}

fn user(content: &str) -> MessageRecord {
    MessageRecord::User {
        content: content.to_string(),
    }
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

/// The pinned pair, always `[0]`/`[1]`.
fn pinned_pair() -> Vec<MessageRecord> {
    vec![sys(PIN_TEXT), user(GOAL_TEXT)]
}

/// One fully settled round: an assistant turn with one call plus its result.
fn settled_round() -> Vec<MessageRecord> {
    vec![
        assistant(
            Some("I will read the file."),
            None,
            vec![call("p-c1", "read_file", "{\"path\":\"src/main.rs\"}")],
        ),
        tool("p-c1", "file bytes"),
    ]
}

/// A rebirth checkpoint carried as a `System` turn — the only thing that legally
/// extends the pinned window (§4 R-PIN-3: its text is opaque to every pass).
fn checkpoint_turn() -> MessageRecord {
    sys(&format!(
        "{REBIRTH_CHECKPOINT_PREFIX}t-013 shipped the loader; next: keep the prefix identical."
    ))
}

/// AC-P fixture: the pinned pair, a settled round, a checkpoint at index 4 and one
/// more turn. The checkpoint makes the persisted pinned window `5`, i.e. something
/// other than the trivial `2`, so AC-P3 cannot be satisfied by accident.
fn pinned_transcript() -> Vec<MessageRecord> {
    let mut messages = pinned_pair();
    messages.extend(settled_round());
    messages.push(checkpoint_turn());
    messages.push(assistant(Some("Prefix unchanged."), None, vec![]));
    messages
}

/// §9.10 AC-D fixture: the transcript ends with the exact crash shape —
/// `Assistant { content: null, tool_calls: [d-c1, d-c2] }` followed by **exactly
/// one** `Tool { tool_call_id: "d-c1" }`.
fn dangling_transcript() -> Vec<MessageRecord> {
    let mut messages = pinned_pair();
    messages.extend(settled_round());
    messages.push(assistant(
        None,
        None,
        vec![
            call("d-c1", "run_command", "{\"command\":\"cargo check\"}"),
            call("d-c2", "grep_search", "{\"pattern\":\"pin\"}"),
        ],
    ));
    messages.push(tool("d-c1", "first result, already landed"));
    messages
}

/// The hand-written witness for that transcript. `observe()` always reports
/// `dispatched: false` (`src/session/format/worker.rs`), so the fixture states the
/// dispatch flags the design describes instead of deriving them.
fn dangling_witness() -> DanglingToolCallState {
    DanglingToolCallState::Dangling {
        calls: vec![
            DanglingCall {
                tool_call_id: "d-c1".to_string(),
                function_name: "run_command".to_string(),
                arguments: "{\"command\":\"cargo check\"}".to_string(),
                dispatched: true,
            },
            DanglingCall {
                tool_call_id: "d-c2".to_string(),
                function_name: "grep_search".to_string(),
                arguments: "{\"pattern\":\"pin\"}".to_string(),
                dispatched: false,
            },
        ],
    }
}

/// AC-D5 fixture: the same settled transcript plus one `Tool` result whose parent
/// assistant turn is gone.
fn orphan_transcript() -> Vec<MessageRecord> {
    let mut messages = pinned_pair();
    messages.extend(settled_round());
    messages.push(tool(
        "orphan-1",
        "result of an assistant turn that is no longer here",
    ));
    messages
}

/// A `ManagerSection` whose witnesses are exactly what the capture side would
/// record for `messages` — except `dangling`, which every caller states on purpose
/// (that is the half of the fixture AC-D1…AC-D4 read).
fn manager_section(
    messages: Vec<MessageRecord>,
    dangling: DanglingToolCallState,
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
        max_context_tokens: COMMITTED_BUDGET,
        rebirth_advisory_emitted: false,
        consecutive_rebirths: 0,
        dangling,
        messages,
    }
}

/// A fixture worker: paired history, clocks sane, sequence numbers below the
/// floors, and a caller-supplied pairing witness.
fn worker(
    key: &str,
    seq: u64,
    history: Vec<MessageRecord>,
    dangling: DanglingToolCallState,
) -> WorkerSection {
    WorkerSection {
        worker_key: key.to_string(),
        worker_id_seq_value: seq,
        role: Agent::Coder,
        task_id: Some("t-014b".to_string()),
        status: WorkerStatus::InProgress,
        status_label: "running".to_string(),
        started: TimePoint::now(),
        completed: None,
        last_activity: None,
        worked_secs: 7.5,
        completed_seq: Some(seq),
        context_tokens: 96,
        last_tokens: 11,
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

/// A worker transcript with one fully settled round on top of its own seed pair.
fn worker_history(key: &str) -> Vec<MessageRecord> {
    vec![
        sys(&format!("specialist prompt for {key}")),
        user(&format!("specialist brief for {key}")),
        assistant(
            Some("reading"),
            None,
            vec![call(
                &format!("{key}-c1"),
                "read_file",
                "{\"path\":\"src/lib.rs\"}",
            )],
        ),
        tool(&format!("{key}-c1"), "lib bytes"),
    ]
}

fn payload(manager: ManagerSection, workers: Vec<WorkerSection>) -> CapturePayload {
    CapturePayload {
        manager,
        ui: Some(UiSection {
            status_line: "t014b status".to_string(),
            ..UiSection::default()
        }),
        workers,
        identity_floors: IdentityFloors {
            worker_id_seq_floor: SEQ_FLOOR,
            completed_seq_floor: SEQ_FLOOR,
            notice_id_floor: 1,
        },
    }
}

/// The public commit seam (`session::commit::write_bundle`), unthrottled.
fn write(
    bundle: &Path,
    session_id: &str,
    payload: &CapturePayload,
    ending: ProcessEnding,
) -> CommitReport {
    let policy = CommitPolicy {
        min_interval: std::time::Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    marmennill::session::commit::write_bundle(
        bundle,
        session_id,
        payload,
        ending,
        &policy,
        &CommitLock::new(),
    )
    .expect("commit must succeed")
}

/// One human-readable sentence for "this outcome was not the expected one".
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
        RestoreOutcome::Refused { refusal, .. } => format!(
            "Refused [{}] member {:?}: {}",
            refusal.code.label(),
            refusal.member,
            refusal.message
        ),
        RestoreOutcome::PartiallyRestored {
            unrecoverable_workers,
            ..
        } => format!("PartiallyRestored {unrecoverable_workers:?}"),
        RestoreOutcome::Restored { .. } => "Restored".to_string(),
    }
}

/// Restore, insisting on a full restore: a refusal is a finding, not a fixture
/// detail, so it fails the test with the loader's own sentence.
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

/// The refusal of an outcome, panicking with the full outcome sentence otherwise.
fn refusal_of(outcome: &RestoreOutcome) -> marmennill::session::Refusal {
    match outcome {
        RestoreOutcome::Refused { refusal, .. } => refusal.clone(),
        other => panic!("expected a Refused outcome, got {}", explain(other)),
    }
}

// ------------------------------------------------------- byte-level witnesses

fn member_bytes(bundle: &Path, rel: &str) -> Vec<u8> {
    let path = bundle.join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn member_text(bundle: &Path, rel: &str) -> String {
    let bytes = member_bytes(bundle, rel);
    String::from_utf8(bytes).unwrap_or_else(|e| panic!("{rel} is not UTF-8: {e}"))
}

/// Every member the manifest names, plus `manifest.json` itself.
fn member_paths(bundle: &Path) -> Vec<String> {
    let text = member_text(bundle, MANIFEST_FILE);
    let manifest: Manifest =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{MANIFEST_FILE} must parse: {e}"));
    let mut paths: Vec<String> = manifest
        .members
        .iter()
        .map(|m| m.rel_path.clone())
        .collect();
    paths.push(MANIFEST_FILE.to_string());
    paths.sort();
    paths
}

/// `(rel_path, fingerprint)` for every member and for the manifest, using the
/// crate's own §7.2 hasher — never a second one.
fn fingerprints(bundle: &Path) -> Vec<(String, u64)> {
    let mut table: Vec<(String, u64)> = member_paths(bundle)
        .into_iter()
        .map(|rel| (rel.clone(), fingerprint(&member_bytes(bundle, &rel))))
        .collect();
    table.sort();
    table
}

/// Asserts that a restore left every committed byte where it found it (§7.3 L9,
/// AC-D6) — members and manifest alike.
fn assert_bundle_bytes_unchanged(bundle: &Path, before: &[(String, u64)]) {
    let after = fingerprints(bundle);
    assert_eq!(
        after,
        before,
        "the restore path rewrote bundle bytes ({} member(s) hashed before and after): \
         the normalized vector may only appear on the NEXT commit",
        before.len()
    );
}

/// Rewrites the `byte_len`/`fingerprint` of one member inside the manifest so a
/// hand-patched member still passes L4/L5 — isolating the check under test. Uses
/// the crate's `fingerprint`, so the probe introduces no second hasher.
fn patch_member_witness(manifest_text: &str, rel_path: &str, bytes: &[u8]) -> String {
    let mut doc: serde_json::Value =
        serde_json::from_str(manifest_text).expect("the committed manifest parses");
    let members = doc
        .get_mut("members")
        .and_then(|m| m.as_array_mut())
        .expect("members is an array");
    let member = members
        .iter_mut()
        .find(|m| m.get("rel_path").and_then(|p| p.as_str()) == Some(rel_path))
        .unwrap_or_else(|| panic!("{rel_path} is not listed in the manifest"));
    member["byte_len"] = serde_json::Value::from(bytes.len() as u64);
    member["fingerprint"] = serde_json::Value::from(fingerprint(bytes));
    serde_json::to_string_pretty(&doc).expect("re-serialize manifest")
}

/// Flips one byte in the middle of one worker member — same `byte_len`, different
/// `fingerprint`, so L4 passes and L5 is what fires (§7.3 order). Returns the
/// member's relative path.
fn corrupt_one_byte(bundle: &Path, worker_key: &str) -> String {
    let rel = worker_rel_path(worker_key);
    let path = bundle.join(&rel);
    let mut bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let at = bytes.len() / 2;
    bytes[at] ^= 0x20;
    std::fs::write(&path, &bytes).expect("flip one byte of the worker member");
    rel
}

// --------------------------------------------------------------- transcript aids

/// The report's warning sentences, for assertion messages.
fn warnings(report: &RepairReport) -> &[String] {
    &report.warnings
}

/// The role-tag sequence, spelled as on the wire.
fn roles(messages: &[MessageRecord]) -> Vec<&'static str> {
    messages.iter().map(MessageRecord::role).collect()
}

/// Every `Tool` turn of a transcript, in order.
fn results(messages: &[MessageRecord]) -> Vec<(&str, &str)> {
    messages
        .iter()
        .filter_map(|record| match record {
            MessageRecord::Tool {
                tool_call_id,
                content,
            } => Some((tool_call_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect()
}

/// The transcript with every `Tool` turn removed: what the §8 pass set is
/// forbidden to touch (AC-D2/AC-D5 read this against the installed vector).
fn non_tool(messages: &[MessageRecord]) -> Vec<&MessageRecord> {
    messages
        .iter()
        .filter(|record| !matches!(record, MessageRecord::Tool { .. }))
        .collect()
}

fn index_of_assistant_with(messages: &[MessageRecord], ids: &[&str]) -> usize {
    messages
        .iter()
        .position(|record| match record {
            MessageRecord::Assistant { tool_calls, .. } => tool_calls
                .iter()
                .map(|c| c.id.as_str())
                .eq(ids.iter().copied()),
            _ => false,
        })
        .unwrap_or_else(|| panic!("no assistant turn carries exactly the calls {ids:?}"))
}

fn index_of_result(messages: &[MessageRecord], id: &str) -> usize {
    messages
        .iter()
        .position(|record| matches!(record, MessageRecord::Tool { tool_call_id, .. } if tool_call_id == id))
        .unwrap_or_else(|| panic!("no tool result carries tool_call_id {id}"))
}

/// The bytes of `messages[index]` when it is a `System` turn — the pinned witness
/// reader shared by AC-P1/AC-P4.
fn system_bytes(messages: &[MessageRecord], index: usize) -> String {
    match messages.get(index) {
        Some(MessageRecord::System { content }) => content.clone(),
        other => panic!("messages[{index}] is not a System turn, it is {other:?}"),
    }
}

/// The bytes of `messages[index]` when it is a `User` turn.
fn user_bytes(messages: &[MessageRecord], index: usize) -> String {
    match messages.get(index) {
        Some(MessageRecord::User { content }) => content.clone(),
        other => panic!("messages[{index}] is not a User turn, it is {other:?}"),
    }
}

/// The prompt a **fresh** boot of this process would install at `[0]`:
/// `ui::helpers::load_system_prompt_with_plan_and_warning` (`src/ui/helpers.rs:111-124`)
/// composes it as `"{system.md}\n\n{environment block}\n"` when
/// `Config::system_prompt_path` does not exist, which is the case here. AC-P4
/// needs exactly these bytes to prove the pinned pair survived rather than being
/// re-composed.
fn fresh_composed_prompt() -> String {
    format!(
        "{}\n\n{}\n",
        marmennill::prompts::SYSTEM_PROMPT,
        marmennill::prompts::format_environment_block()
    )
}

// ------------------------------------------------------------------ boot harness

/// Minimal `Renderer` for the boot pass: records every event, so "the restore was
/// announced, not silent" is checkable.
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

/// Everything a full boot left behind: the report, the engine that was really
/// installed, the renderer's status lines and the session id boot minted/kept.
struct Booted {
    report: marmennill::ui::session::BootReport,
    engine: marmennill::manager::ContextEngine,
    statuses: Vec<String>,
    session_id: String,
}

/// THE boot path (`ui::session::boot`, flow §4.9) driven end to end: `load_bundle`
/// → floors + `WORKERS` → the engine → the panes → the loud report → `N-MGR-1`.
/// AC-P1/AC-P2/AC-P3/AC-P4 assert on what this returns, i.e. **after** the whole
/// pass ran: had `set_system_prompt` or `set_goal` run, `[0]`/`[1]` would already
/// carry this process's bytes by now.
fn boot_bundle(bundle: &Path) -> Booted {
    let cfg = Config {
        max_context_tokens: LIVE_BUDGET,
        ..Config::default()
    };
    let plan = Plan::at(root_of(bundle).join(MARMEL_DIR));
    assert_eq!(
        marmennill::session::bundle_dir(&plan),
        bundle,
        "the fixture bundle is exactly where the boot pass looks"
    );
    let mut engine = marmennill::ui::session::build_restored_context(&cfg);
    let mut renderer = BootRenderer::default();
    let mut transcript = marmennill::ui::UiTranscript::new();
    let mut session_id = String::new();
    let mut sink = marmennill::ui::session::BootSink {
        ui_transcript: &mut transcript,
    };
    let report = marmennill::ui::session::boot(
        &plan,
        &cfg,
        &mut engine,
        &mut renderer,
        &mut session_id,
        &mut sink,
    )
    .expect("a sound bundle must boot, not refuse");
    Booted {
        report,
        engine,
        statuses: renderer.statuses(),
        session_id,
    }
}

// ==========================================================================
// §9.2 — the pinned prefix (AC-P1 … AC-P5)
// ==========================================================================

/// **AC-P1** — after the **full** boot path has run, `messages[0]` is still the
/// `System` pin byte-for-byte and `messages[1]` is still the `User` pin
/// byte-for-byte. Nothing here calls `set_system_prompt` / `set_goal`: the proof
/// that the two pin writers were skipped is that `ui::session::boot` ran to
/// completion (including `N-MGR-1`, the one synthetic notice appended at index
/// `>= 2`) and `[0]`/`[1]` are still the committed bytes.
#[test]
fn ac_p1_pinned_pair_survives_the_full_boot_path_byte_for_byte() {
    let bundle = bundle_dir("ac-p1");
    let committed = pinned_transcript();
    let section = manager_section(committed.clone(), DanglingToolCallState::Balanced);
    write(
        &bundle,
        "session-ac-p1",
        &payload(section.clone(), vec![]),
        ProcessEnding::InProgress,
    );

    // The restored records themselves: [0] System, [1] User, byte-equal.
    let (restored, _) = reload(&bundle);
    assert_eq!(
        restored.manager.messages[0],
        MessageRecord::System {
            content: PIN_TEXT.to_string()
        },
        "restored.messages[0] must be the System pin verbatim"
    );
    assert_eq!(
        restored.manager.messages[1],
        MessageRecord::User {
            content: GOAL_TEXT.to_string()
        },
        "restored.messages[1] must be the User pin verbatim"
    );
    assert_eq!(system_bytes(&restored.manager.messages, 0), PIN_TEXT);
    assert_eq!(user_bytes(&restored.manager.messages, 1), GOAL_TEXT);

    // ---------- the assertion that matters: AFTER the whole boot pass ----------
    let booted = boot_bundle(&bundle);
    let installed = booted.engine.messages();
    assert!(
        installed.len() >= 2,
        "the boot path must have installed the pinned pair, it installed {} message(s)",
        installed.len()
    );
    match &installed[0] {
        Message::System { content } => assert_eq!(
            content.as_bytes(),
            PIN_TEXT.as_bytes(),
            "AC-P1: after boot, messages[0] is not the committed System pin — a pin writer ran"
        ),
        other => panic!(
            "AC-P1: after boot, messages[0] must still be the System pin (REQ-CORE-001), got {other:?}"
        ),
    }
    match &installed[1] {
        Message::User { content } => assert_eq!(
            content.as_bytes(),
            GOAL_TEXT.as_bytes(),
            "AC-P1: after boot, messages[1] is not the committed User pin — `set_goal` ran"
        ),
        other => panic!(
            "AC-P1: after boot, messages[1] must still be the pinned goal (REQ-CORE-002), got {other:?}"
        ),
    }
    // Byte-equal, not merely equal-length, and equal to what the bundle holds.
    let (again, _) = reload(&bundle);
    let installed_records: Vec<MessageRecord> =
        installed.iter().map(MessageRecord::from_wire).collect();
    assert_eq!(
        system_bytes(&installed_records, 0),
        system_bytes(&again.manager.messages, 0),
        "the installed [0] must be the bundle's [0], byte for byte"
    );
    assert_eq!(
        user_bytes(&installed_records, 1),
        user_bytes(&again.manager.messages, 1),
        "the installed [1] must be the bundle's [1], byte for byte"
    );

    // The boot pass appended exactly one thing, at index >= 2: N-MGR-1. That the
    // vector grew *behind* the pins is what a pin-preserving restore looks like.
    assert_eq!(
        installed.len(),
        again.manager.messages.len() + 1,
        "boot appends exactly the one synthetic restore notice"
    );
    let notice = match installed.last() {
        Some(Message::User { content }) => content.clone(),
        other => panic!("the restore notice must be a User turn, got {other:?}"),
    };
    assert!(
        notice.starts_with(marmennill::session::SESSION_RESTORE_NOTICE_PREFIX),
        "the appended notice must carry the restore prefix, got {notice:?}"
    );
    assert_eq!(
        &installed_records[..again.manager.messages.len()],
        again.manager.messages.as_slice(),
        "every restored turn keeps its index and its bytes; only the notice was appended"
    );

    // Loud, not silent: the goal is reported from the restored [1], and the boot
    // pass says out loud that its own composed prompt was NOT installed.
    assert_eq!(
        booted.report.goal, GOAL_TEXT,
        "BootReport::goal must be the restored messages[1] verbatim, never re-typed"
    );
    assert!(
        booted
            .statuses
            .iter()
            .any(|line| line.contains(&booted.session_id)),
        "the restore must be announced, not silent"
    );
    cleanup(&bundle);
}

/// **AC-P2** — on the vector that was actually installed, the pin witness equals
/// the byte length of the message it witnesses:
/// `pin_witness.system_prompt_len == messages[0].content.len()` and
/// `pin_witness.goal_len == messages[1].content.len()`.
#[test]
fn ac_p2_pin_witness_agrees_with_the_installed_vector() {
    let bundle = bundle_dir("ac-p2");
    let committed = pinned_transcript();
    let section = manager_section(committed.clone(), DanglingToolCallState::Balanced);
    write(
        &bundle,
        "session-ac-p2",
        &payload(section.clone(), vec![]),
        ProcessEnding::InProgress,
    );

    let (restored, _) = reload(&bundle);
    let booted = boot_bundle(&bundle);
    let installed = booted.engine.messages();
    let witness = booted.engine.pin_witness();

    let Message::System { content: system } = &installed[0] else {
        panic!("AC-P2: messages[0] must be the System pin after boot");
    };
    let Message::User { content: goal } = &installed[1] else {
        panic!("AC-P2: messages[1] must be the pinned goal after boot");
    };
    assert_eq!(
        witness.system_prompt_len,
        system.len() as u32,
        "the system-prompt witness must be the installed messages[0] byte length"
    );
    assert_eq!(
        witness.goal_len,
        goal.len() as u32,
        "the goal witness must be the installed messages[1] byte length"
    );

    // …and those are the *committed* lengths, i.e. the identity survived the boot
    // pass rather than being re-measured over re-composed bytes.
    assert_eq!(
        witness,
        PinWitness {
            system_prompt_len: PIN_TEXT.len() as u32,
            goal_len: GOAL_TEXT.len() as u32,
        },
        "the installed witness must be the committed witness"
    );
    assert_eq!(
        witness, restored.manager.pin_witness,
        "the loader's witness and the installed engine's witness must agree"
    );
    assert!(
        booted.report.repair.pins_verified,
        "the loader must have verified the pins on the installed vector too"
    );
    cleanup(&bundle);
}

/// **AC-P3** — the recomputed pinned window equals the persisted one *after*
/// normalization (§8 N3). The fixture carries a rebirth checkpoint at index 4, so
/// the window is `5`, not the trivial `2`: matching on `2` would prove nothing.
#[test]
fn ac_p3_recomputed_pinned_prefix_end_matches_the_persisted_value() {
    let bundle = bundle_dir("ac-p3");
    let committed = pinned_transcript();
    let section = manager_section(committed.clone(), DanglingToolCallState::Balanced);
    write(
        &bundle,
        "session-ac-p3",
        &payload(section.clone(), vec![]),
        ProcessEnding::InProgress,
    );
    // The fixture's own claim, stated out loud: the checkpoint is what extends the
    // window, and it is at index 4.
    let persisted = pinned_prefix_end_records(&committed);
    assert_eq!(
        persisted, 5,
        "the fixture must exercise a non-trivial pinned window (checkpoint at index 4)"
    );

    let (restored, report) = reload(&bundle);
    assert_eq!(
        restored.manager.pinned_prefix_end, persisted as u32,
        "the persisted pinned_prefix_end must survive the round trip"
    );
    assert_eq!(
        pinned_prefix_end_records(&restored.manager.messages) as u32,
        restored.manager.pinned_prefix_end,
        "N3: recomputed pinned_prefix_end must equal the persisted value on the normalized vector"
    );
    assert_eq!(
        report.pinned_prefix_end_expected,
        Some(persisted as u32),
        "the report must carry the persisted value"
    );
    assert_eq!(
        report.pinned_prefix_end_actual,
        Some(persisted as u32),
        "the report must carry the recomputed-after-normalization value"
    );
    assert!(
        report
            .warnings
            .iter()
            .all(|w| !w.contains("pinned_prefix_end")),
        "no pass may have moved the window: {:?}",
        warnings(&report)
    );

    // The engine's own recomputation over the installed vector (the vector the
    // next `compact()` and `perform_rebirth()` read) says the same number — even
    // though boot appended the synthetic notice at index >= 2.
    let booted = boot_bundle(&bundle);
    assert_eq!(
        booted.engine.pinned_prefix_end(),
        persisted,
        "AC-P3: ContextEngine::pinned_prefix_end() on the installed vector must equal the persisted value"
    );
    assert_eq!(
        booted.report.repair.pinned_prefix_end_expected,
        Some(persisted as u32)
    );
    assert_eq!(
        booted.report.repair.pinned_prefix_end_actual,
        Some(persisted as u32)
    );
    cleanup(&bundle);
}

/// **AC-P4** — the prompt this boot composed is **different** from the pinned
/// bytes. Without this, AC-P1 could pass by accident: if the fixture pinned the
/// same text this process composes, "the pins were not overwritten" and "they were
/// overwritten with the same thing" would be indistinguishable.
#[test]
fn ac_p4_the_prompt_this_boot_composed_is_not_the_pinned_bytes() {
    let bundle = bundle_dir("ac-p4");
    let committed = pinned_transcript();
    let section = manager_section(committed.clone(), DanglingToolCallState::Balanced);
    write(
        &bundle,
        "session-ac-p4",
        &payload(section.clone(), vec![]),
        ProcessEnding::InProgress,
    );

    let fresh_prompt = fresh_composed_prompt();
    assert!(
        fresh_prompt.len() > PIN_TEXT.len(),
        "the comparator must be a real composed prompt (system.md + environment block), \
         got {} byte(s)",
        fresh_prompt.len()
    );

    let (restored, _) = reload(&bundle);
    assert_ne!(
        fresh_prompt,
        system_bytes(&restored.manager.messages, 0),
        "AC-P4: the composed prompt must differ from the pinned bytes, or AC-P1 proves nothing"
    );

    let booted = boot_bundle(&bundle);
    let installed = booted.engine.messages();
    let Message::System { content } = &installed[0] else {
        panic!("AC-P4: messages[0] must be the System pin after boot");
    };
    assert_ne!(
        fresh_prompt,
        content.as_str(),
        "AC-P4: the installed [0] is byte-identical to what this process composes — \
         `set_system_prompt` ran on a restored transcript"
    );
    assert_ne!(
        fresh_prompt.len() as u32,
        booted.engine.pin_witness().system_prompt_len,
        "the pin witness must still measure the OLD prompt, not the composed one"
    );

    // The boot pass says so out loud, quoting both witness lengths and the stored
    // window: this is R-PIN-2's loud half, and it only exists because the composed
    // bytes were deliberately not installed.
    let sentence = booted
        .report
        .repair
        .warnings
        .iter()
        .find(|warning| warning.contains("was NOT installed"))
        .unwrap_or_else(|| {
            panic!(
                "the boot pass must report the un-installed prompt, got {:?}",
                booted.report.repair.warnings
            )
        });
    assert!(
        sentence.contains(&PIN_TEXT.len().to_string())
            && sentence.contains(&GOAL_TEXT.len().to_string())
            && sentence.contains(&pinned_prefix_end_records(&committed).to_string()),
        "the sentence must name both witness lengths and the stored window: {sentence}"
    );
    cleanup(&bundle);
}

/// **AC-P5** — a bundle whose pin was truncated by hand is **refused**, not
/// repaired: `Refused { code: PinViolation, member: "manager.json" }`, and the
/// member bytes are unchanged afterwards. The manifest witness is re-sealed with
/// the crate's own `fingerprint` so L4/L5 pass and the pin rule is the only thing
/// that can fire; every member and `manifest.json` is then hashed with that same
/// hasher before and after (§7.2 — no second hasher).
#[test]
fn ac_p5_a_hand_edited_pin_is_refused_and_no_byte_of_the_bundle_changes() {
    let bundle = bundle_dir("ac-p5");
    let committed = pinned_transcript();
    let section = manager_section(committed.clone(), DanglingToolCallState::Balanced);
    write(
        &bundle,
        "session-ac-p5",
        &payload(section.clone(), vec![]),
        ProcessEnding::InProgress,
    );

    // Hand-truncate the pinned system prompt inside manager.json, then re-seal the
    // manifest so the tampering is invisible to byte_len/fingerprint.
    let mut doc: serde_json::Value =
        serde_json::from_str(&member_text(&bundle, MANAGER_FILE)).expect("manager.json parses");
    let edited = format!("{} (hand-edited)", &PIN_TEXT[..24]);
    doc["section"]["messages"][0]["content"] = serde_json::Value::String(edited.clone());
    assert_eq!(
        doc["section"]["pin_witness"]["system_prompt_len"].as_u64(),
        Some(PIN_TEXT.len() as u64),
        "the probe edits the transcript only — the recorded witness is left as committed"
    );
    let patched = serde_json::to_vec_pretty(&doc).expect("re-serialize manager.json");
    std::fs::write(bundle.join(MANAGER_FILE), &patched).expect("write hand-edited manager.json");
    let fresh_member = std::fs::read(bundle.join(MANAGER_FILE)).expect("read back");
    let manifest_text = member_text(&bundle, MANIFEST_FILE);
    let resealed = patch_member_witness(&manifest_text, MANAGER_FILE, &fresh_member);
    std::fs::write(bundle.join(MANIFEST_FILE), resealed.as_bytes()).expect("re-seal manifest");

    // Bytes as they stand NOW (hand-edited, re-sealed) are the baseline the loader
    // must leave untouched.
    let before = fingerprints(&bundle);
    assert_eq!(
        before.len(),
        3,
        "the fixture bundle is manager.json + ui.json + manifest.json"
    );

    let outcome = load_bundle(&bundle);
    let refusal = refusal_of(&outcome);
    assert_eq!(
        refusal.code,
        RefusalCode::PinViolation,
        "a hand-edited pin must refuse with pinViolation, got {}",
        explain(&outcome)
    );
    assert_eq!(
        refusal.member.as_deref(),
        Some(MANAGER_FILE),
        "the refusal must name the member that carries the pin"
    );
    assert!(
        refusal.message.contains("step 5"),
        "the pin-witness step must be the one that fired: {}",
        refusal.message
    );
    assert!(
        refusal.message.contains(&PIN_TEXT.len().to_string())
            && refusal.message.contains(&edited.len().to_string()),
        "the sentence must name both numbers so a human can localize the edit: {}",
        refusal.message
    );

    // Fail-closed: the loader refused, so it touched nothing.
    assert_bundle_bytes_unchanged(&bundle, &before);
    assert_eq!(
        member_text(&bundle, MANAGER_FILE),
        String::from_utf8(fresh_member).expect("utf-8"),
        "the hand-edited member is still the hand-edited member: the loader does not \
         'fix' a refused bundle"
    );
    cleanup(&bundle);
}

// ==========================================================================
// §9.10 — the dangling tool call (AC-D1 … AC-D7)
// ==========================================================================

/// **AC-D1** — the crash shape is reported and healed, with every synthesized id
/// named: a manager transcript ending `Assistant { content: null, tool_calls:
/// [d-c1, d-c2] }` plus **exactly one** `Tool { "d-c1" }`, recorded
/// `ending: InProgress` and
/// `dangling: Dangling { [d-c1 dispatched, d-c2 not dispatched] }`, restores as
/// `Restored` with `ending == HardKillInferred`, the real result for `d-c1` still
/// directly under its own assistant, one `(aborted)` placeholder for `d-c2`,
/// `aborted_results_synthesized == 1`, `normalization_passes == 2` and
/// `normalization_stable == true`.
#[test]
fn ac_d1_the_dangling_pair_is_reported_and_healed_with_one_named_placeholder() {
    let bundle = bundle_dir("ac-d1");
    let committed = dangling_transcript();
    // The fixture IS the §9.10 shape; say so before asserting anything about it.
    assert_eq!(
        roles(&committed),
        vec!["system", "user", "assistant", "tool", "assistant", "tool"],
        "the transcript must end with the unanswered assistant turn and one result"
    );
    assert_eq!(
        index_of_assistant_with(&committed, &["d-c1", "d-c2"]),
        committed.len() - 2,
        "the content-less assistant must be the second-to-last record"
    );
    assert_eq!(
        results(&committed),
        vec![
            ("p-c1", "file bytes"),
            ("d-c1", "first result, already landed")
        ],
        "exactly one result exists for the two dangling calls"
    );

    let section = manager_section(committed.clone(), dangling_witness());
    let commit = write(
        &bundle,
        "session-ac-d1",
        &payload(section.clone(), vec![]),
        ProcessEnding::InProgress,
    );
    assert!(commit.members_written.contains(&MANAGER_FILE.to_string()));

    let outcome = load_bundle(&bundle);
    let RestoreOutcome::Restored {
        session,
        ending,
        report,
    } = outcome
    else {
        panic!("AC-D1: expected a full restore, got {}", explain(&outcome));
    };

    // The manifest still claims InProgress; the loader only *reports* the kill.
    assert_eq!(session.manifest.ending, ProcessEnding::InProgress);
    assert_eq!(ending, ProcessEnding::HardKillInferred);

    // The recorded pairing witness survives verbatim.
    assert_eq!(
        session.manager.dangling,
        dangling_witness(),
        "AC-D1: the recorded dangling witness must be returned verbatim, never re-derived"
    );

    // The installed transcript: the real result for d-c1 sits directly under its
    // own assistant, and d-c2 got exactly one placeholder.
    let installed = &session.manager.messages;
    let parent = index_of_assistant_with(installed, &["d-c1", "d-c2"]);
    assert_eq!(
        installed.get(parent + 1),
        Some(&MessageRecord::Tool {
            tool_call_id: "d-c1".to_string(),
            content: "first result, already landed".to_string(),
        }),
        "AC-D1: the landed result must stay directly under its own assistant turn"
    );
    assert_eq!(
        installed.get(parent + 2),
        Some(&MessageRecord::Tool {
            tool_call_id: "d-c2".to_string(),
            content: ABORTED_TOOL_RESULT.to_string(),
        }),
        "AC-D1: the un-dispatched call must get one {ABORTED_TOOL_RESULT} result, directly \
         after the block that belongs to its own assistant"
    );
    assert_eq!(
        installed.len(),
        committed.len() + 1,
        "exactly one record was added"
    );
    assert_eq!(
        results(installed).len(),
        results(&committed).len() + 1,
        "no other tool result may appear: the restore re-runs nothing"
    );

    // Bookkeeping, in the report and loudly.
    assert_eq!(
        report.aborted_results_synthesized,
        1,
        "exactly one placeholder: {:?}",
        warnings(&report)
    );
    assert_eq!(report.orphan_tool_results_pruned, 0);
    assert_eq!(report.tool_results_regrouped, 0);
    assert_eq!(
        report.normalization_passes, 2,
        "§8 N5: the pass set runs twice"
    );
    assert!(
        report.normalization_stable,
        "the second pass must be a no-op"
    );
    assert_eq!(report.manager_messages_before, committed.len());
    assert_eq!(report.manager_messages_after, committed.len() + 1);
    assert!(
        warnings(&report).iter().any(|w| w.contains("d-c2")),
        "the synthesized id must be named in the report: {:?}",
        warnings(&report)
    );
    assert!(
        warnings(&report)
            .iter()
            .any(|w| w.contains("recorded dangling state")),
        "N6 cross-check: the stored witness and the healed vector disagree, and that is \
         reported, never reconciled: {:?}",
        warnings(&report)
    );

    // The healed vector is what the engine receives, and the engine's own observer
    // (`ContextEngine::dangling_state`, src/manager/context.rs:1196) reads it as
    // **healed**: no call is left unanswered, and the one placeholder it finds is
    // named by id — the vocabulary `DanglingToolCallState::Healed` exists for.
    let mut engine = marmennill::manager::ContextEngine::new(LIVE_BUDGET);
    engine.install_restored_messages(installed.clone());
    assert_eq!(
        engine.dangling_state(),
        DanglingToolCallState::Healed {
            aborted_tool_call_ids: vec!["d-c2".to_string()]
        },
        "AC-D1: after the repair the installed transcript must hold no dangling call, \
         and the observer must name the placeholder it sees"
    );
    cleanup(&bundle);
}

/// **AC-D2** — the unanswered assistant turn is **not dropped**, and its
/// `content: None` is still `None` (not `""`, not a synthesized sentence). This is
/// the whole reason `MessageRecord` exists: the wire type folds `None` into `""`.
#[test]
fn ac_d2_the_unanswered_assistant_turn_survives_with_content_still_none() {
    let bundle = bundle_dir("ac-d2");
    let committed = dangling_transcript();
    let expected_calls = vec![
        call("d-c1", "run_command", "{\"command\":\"cargo check\"}"),
        call("d-c2", "grep_search", "{\"pattern\":\"pin\"}"),
    ];
    write(
        &bundle,
        "session-ac-d2",
        &payload(
            manager_section(committed.clone(), dangling_witness()),
            vec![],
        ),
        ProcessEnding::InProgress,
    );
    // On disk the fact is spelled `null`, so the criterion is checkable in the file.
    assert!(
        member_text(&bundle, MANAGER_FILE).contains("\"content\": null"),
        "the committed member must carry a JSON null for the content-less assistant turn"
    );

    let (session, report) = reload(&bundle);
    let installed = &session.manager.messages;

    // Nothing but `Tool` turns was ever added or moved: every non-`Tool` record is
    // still there, in order, byte-for-byte — including the unanswered assistant.
    assert_eq!(
        non_tool(installed),
        non_tool(&committed),
        "AC-D2: the pass set may only touch Tool turns; a turn was dropped, re-roledd or reordered"
    );
    assert!(report.aborted_results_synthesized >= 1);

    let parent = index_of_assistant_with(installed, &["d-c1", "d-c2"]);
    let MessageRecord::Assistant {
        content,
        reasoning_content,
        tool_calls,
    } = &installed[parent]
    else {
        panic!("AC-D2: the assistant turn at index {parent} is not an assistant turn any more")
    };
    assert_eq!(
        content.as_deref(),
        None,
        "AC-D2: content of the unanswered assistant turn must stay None (JSON null)"
    );
    assert_eq!(
        reasoning_content.as_deref(),
        None,
        "the reasoning slot must stay absent too: no pass may invent text"
    );
    assert_eq!(
        tool_calls, &expected_calls,
        "the two unanswered calls must survive verbatim, including the one that got a placeholder"
    );

    // Same verdict after the wire conversion the engine install performs.
    let mut engine = marmennill::manager::ContextEngine::new(LIVE_BUDGET);
    engine.install_restored_messages(installed.clone());
    match &engine.messages()[parent] {
        Message::Assistant { content, .. } => assert!(
            content.is_none(),
            "AC-D2: the installed wire message folded content: None into Some(\"\")"
        ),
        other => panic!("AC-D2: installed record {parent} is not an assistant turn: {other:?}"),
    }
    cleanup(&bundle);
}

/// **AC-D3** — the synthesis happens *behind* the pins: `messages[0]` and
/// `messages[1]` keep their indices and their bytes, and `pinned_prefix_end` is
/// unchanged by the repair.
#[test]
fn ac_d3_the_pins_keep_their_indices_and_bytes_through_the_synthesis() {
    let bundle = bundle_dir("ac-d3");
    let committed = dangling_transcript();
    let persisted = pinned_prefix_end_records(&committed) as u32;
    assert_eq!(
        persisted, 2,
        "this fixture has no checkpoint: the window is the pair"
    );
    write(
        &bundle,
        "ac-d3",
        &payload(
            manager_section(committed.clone(), dangling_witness()),
            vec![],
        ),
        ProcessEnding::InProgress,
    );

    let (session, report) = reload(&bundle);
    let installed = &session.manager.messages;
    assert_eq!(
        installed[0],
        MessageRecord::System {
            content: PIN_TEXT.to_string()
        },
        "AC-D3: messages[0] must still be the System pin, at index 0"
    );
    assert_eq!(
        installed[1],
        MessageRecord::User {
            content: GOAL_TEXT.to_string()
        },
        "AC-D3: messages[1] must still be the pinned goal, at index 1"
    );
    assert_eq!(roles(installed)[0], "system");
    assert_eq!(roles(installed)[1], "user");
    assert_eq!(report.pinned_prefix_end_expected, Some(persisted));
    assert_eq!(
        report.pinned_prefix_end_actual,
        Some(persisted),
        "§8 N3: the repair must not move the pinned window"
    );
    assert_eq!(
        pinned_prefix_end_records(installed) as u32,
        session.manager.pinned_prefix_end,
        "the installed window must equal the persisted one"
    );
    assert_eq!(session.manager.pinned_prefix_end, persisted);
    assert!(report.pins_verified);
    // The synthesized record landed behind the window, never inside it.
    let synthesized = index_of_result(installed, "d-c2");
    assert!(
        synthesized >= persisted as usize,
        "the placeholder went inside the pinned window (index {synthesized} < {persisted})"
    );

    // And it stays true through the full boot path.
    let booted = boot_bundle(&bundle);
    assert_eq!(booted.engine.pinned_prefix_end(), persisted as usize);
    let installed_records: Vec<MessageRecord> = booted
        .engine
        .messages()
        .iter()
        .map(MessageRecord::from_wire)
        .collect();
    assert_eq!(system_bytes(&installed_records, 0), PIN_TEXT);
    assert_eq!(user_bytes(&installed_records, 1), GOAL_TEXT);
    assert_eq!(index_of_result(&installed_records, "d-c2"), synthesized);
    cleanup(&bundle);
}

/// **AC-D4** — a call that had already been **dispatched** is a lost capability,
/// not a repair: it raises `NonRestorableNotice { kind: PtyProcess }` naming the
/// subject (`manager` for the manager transcript, `worker <key>` for a worker),
/// says the tool *may have mutated the workspace*, and promises not to re-run it —
/// and the restore path really does not re-run anything: the only new result is the
/// placeholder, and every file in the session's own directory tree is untouched.
#[test]
fn ac_d4_a_dispatched_call_becomes_a_pty_notice_and_is_never_re_run() {
    // ---------------- manager transcript ----------------
    let bundle = bundle_dir("ac-d4");
    let committed = dangling_transcript();
    write(
        &bundle,
        "session-ac-d4",
        &payload(
            manager_section(committed.clone(), dangling_witness()),
            vec![],
        ),
        ProcessEnding::InProgress,
    );
    // A workspace witness: the restore must not create, touch or rewrite anything
    // anywhere under the session's temp root.
    let sentinel = root_of(&bundle).join("workspace-sentinel.txt");
    std::fs::write(&sentinel, b"do not re-run me").expect("write workspace sentinel");
    let sentinel_before = fingerprint(&std::fs::read(&sentinel).expect("read sentinel"));
    let sentinel_mtime = std::fs::metadata(&sentinel)
        .expect("sentinel metadata")
        .modified()
        .expect("mtime");
    let before = fingerprints(&bundle);

    let (session, report) = reload(&bundle);

    let pty: Vec<&NonRestorableNotice> = report
        .notices
        .iter()
        .filter(|notice| notice.kind == NonRestorableKind::PtyProcess)
        .collect();
    assert_eq!(
        pty.len(),
        1,
        "exactly one PTY/process notice for the dispatched call: {:?}",
        report.notices
    );
    let notice = pty[0];
    assert_eq!(
        notice.subject, "manager",
        "the notice must name the subject it applies to"
    );
    assert!(
        notice.detail.contains("d-c1") && notice.detail.contains("run_command"),
        "the dispatched call must be identified by id and function: {}",
        notice.detail
    );
    assert!(
        !notice.detail.contains("d-c2"),
        "the un-dispatched call is a repair, not a lost capability: {}",
        notice.detail
    );
    assert!(
        notice
            .detail
            .contains("may have run and changed the workspace"),
        "the notice must refuse to claim the effect did not happen: {}",
        notice.detail
    );
    assert!(
        notice.detail.contains("will not re-run"),
        "the notice must promise no re-execution: {}",
        notice.detail
    );
    assert!(
        warnings(&report)
            .iter()
            .any(|w| w.contains("d-c1") && w.contains("dispatched but never answered")),
        "the dispatched call is also named in the warnings: {:?}",
        warnings(&report)
    );

    // No tool was re-executed: the transcript gained exactly one record, and that
    // record is the placeholder — not a real result from a second run.
    assert_eq!(
        results(&session.manager.messages),
        vec![
            ("p-c1", "file bytes"),
            ("d-c1", "first result, already landed"),
            ("d-c2", ABORTED_TOOL_RESULT),
        ],
        "AC-D4: the only new tool result is the placeholder; no re-executed tool produced output"
    );
    assert_eq!(report.aborted_results_synthesized, 1);
    assert_eq!(report.orphan_tool_results_pruned, 0);
    // …and nothing on disk moved: not a bundle byte, not the sentinel.
    assert_bundle_bytes_unchanged(&bundle, &before);
    assert_eq!(
        fingerprint(&std::fs::read(&sentinel).expect("read sentinel after restore")),
        sentinel_before,
        "the restore path wrote into the workspace"
    );
    assert_eq!(
        std::fs::metadata(&sentinel)
            .expect("sentinel metadata")
            .modified()
            .expect("mtime"),
        sentinel_mtime,
        "the restore path touched a workspace file"
    );
    cleanup(&bundle);

    // ---------------- worker transcript: same rule, worker subject ----------------
    let worker_bundle = bundle_dir("ac-d4-worker");
    let key = "coder-ac-d4";
    let worker_witness = DanglingToolCallState::Dangling {
        calls: vec![DanglingCall {
            tool_call_id: "w-c9".to_string(),
            function_name: "replace".to_string(),
            arguments: "{\"path\":\"src/lib.rs\"}".to_string(),
            dispatched: true,
        }],
    };
    write(
        &worker_bundle,
        "session-ac-d4-worker",
        &payload(
            manager_section(pinned_transcript(), DanglingToolCallState::Balanced),
            vec![worker(key, 2, worker_history(key), worker_witness)],
        ),
        ProcessEnding::InProgress,
    );
    let (worker_session, worker_report) = reload(&worker_bundle);
    let worker_notice = worker_report
        .notices
        .iter()
        .find(|notice| notice.kind == NonRestorableKind::PtyProcess)
        .unwrap_or_else(|| {
            panic!(
                "AC-D4: a dispatched worker call must raise a PtyProcess notice: {:?}",
                worker_report.notices
            )
        });
    assert_eq!(
        worker_notice.subject,
        format!("worker {key}"),
        "the worker spelling of the notice must name the worker subject"
    );
    assert!(worker_notice.detail.contains("w-c9"));
    assert_eq!(
        worker_session.workers[0].history,
        worker_history(key),
        "the worker's settled history is untouched: no placeholder, no re-run"
    );
    assert_eq!(worker_report.aborted_results_synthesized, 0);
    cleanup(&worker_bundle);
}

/// **AC-D5** — the orphan variant: a `Tool` result whose parent assistant turn is
/// gone is pruned (§8 N1), counted once, and its id appears in `warnings`; the rest
/// of the transcript is element-by-element equal to what was committed.
#[test]
fn ac_d5_an_orphan_tool_result_is_pruned_counted_and_named() {
    let bundle = bundle_dir("ac-d5");
    let committed = orphan_transcript();
    let section = manager_section(
        committed.clone(),
        DanglingToolCallState::OrphanedResults {
            tool_call_ids: vec!["orphan-1".to_string()],
        },
    );
    write(
        &bundle,
        "session-ac-d5",
        &payload(section.clone(), vec![]),
        ProcessEnding::InProgress,
    );
    assert!(
        committed.iter().any(|record| matches!(record, MessageRecord::Tool { tool_call_id, .. } if tool_call_id == "orphan-1")),
        "the fixture must carry the orphan result"
    );

    let (session, report) = reload(&bundle);
    assert_eq!(
        report.orphan_tool_results_pruned, 1,
        "AC-D5: exactly one orphan result must be pruned"
    );
    assert_eq!(
        report.aborted_results_synthesized, 0,
        "N2 must invent nothing here"
    );
    assert!(
        warnings(&report).iter().any(|w| w.contains("orphan-1")),
        "the pruned id must appear in warnings: {:?}",
        warnings(&report)
    );
    assert!(
        !results(&session.manager.messages)
            .iter()
            .any(|(id, _)| *id == "orphan-1"),
        "the orphan must be gone from the installed vector"
    );

    // Everything else is element-wise equal, in order.
    let mut expected = committed.clone();
    expected.retain(|record| {
        !matches!(record, MessageRecord::Tool { tool_call_id, .. } if tool_call_id == "orphan-1")
    });
    assert_eq!(
        session.manager.messages, expected,
        "AC-D5: only the orphan may be gone — one more record moved or changed"
    );
    assert_eq!(
        system_bytes(&session.manager.messages, 0),
        PIN_TEXT,
        "the pins survive a prune"
    );
    assert_eq!(
        user_bytes(&session.manager.messages, 1),
        GOAL_TEXT,
        "the pins survive a prune"
    );
    assert_eq!(
        session.manager.pinned_prefix_end,
        pinned_prefix_end_records(&session.manager.messages) as u32
    );
    assert!(report.pins_verified);
    // The stored witness says `OrphanedResults`; after the prune the vector reads
    // `Balanced`. §8.2 N6: that disagreement is reported, never reconciled.
    assert!(
        warnings(&report)
            .iter()
            .any(|w| w.contains("recorded dangling state")),
        "N6 must report the witness/vector disagreement: {:?}",
        warnings(&report)
    );
    assert_eq!(
        session.manager.dangling,
        DanglingToolCallState::OrphanedResults {
            tool_call_ids: vec!["orphan-1".to_string()]
        },
        "the recorded witness itself is preserved verbatim, disagreement or not"
    );
    cleanup(&bundle);
}

/// **AC-D6** — the read-only rule (§7.3 L9): after a restore that repaired a
/// transcript, `fingerprint` of **every** member file *and* `manifest.json` is
/// identical to before the load. The normalized bytes appear only on the **next**
/// commit, which is the only writer of bundle bytes.
#[test]
fn ac_d6_the_bundle_is_byte_identical_after_restore_and_only_the_next_commit_rewrites() {
    let bundle = bundle_dir("ac-d6");
    let committed = dangling_transcript();
    write(
        &bundle,
        "session-ac-d6",
        &payload(
            manager_section(committed.clone(), dangling_witness()),
            vec![],
        ),
        ProcessEnding::InProgress,
    );

    let before = fingerprints(&bundle);
    assert_eq!(before.len(), 3, "manager.json + ui.json + manifest.json");
    let (session, report) = reload(&bundle);
    assert_eq!(
        report.aborted_results_synthesized, 1,
        "the fixture must really need a repair, or AC-D6 proves nothing"
    );

    // Every member, and the manifest itself, byte-for-byte where they were.
    assert_bundle_bytes_unchanged(&bundle, &before);
    assert_eq!(
        fingerprints(&bundle),
        before,
        "re-hashing a second time must also be stable"
    );
    assert!(
        member_text(&bundle, MANIFEST_FILE).contains("\"inProgress\""),
        "the loader reports HardKillInferred but never rewrites the manifest ending"
    );
    assert!(
        !member_text(&bundle, MANAGER_FILE).contains(ABORTED_TOOL_RESULT),
        "the placeholder must not have been written back into the committed member"
    );
    // And the installed vector really is the repaired one — the difference lives in
    // memory, not on disk.
    assert!(session.manager.messages.iter().any(|record| matches!(
        record,
        MessageRecord::Tool { tool_call_id, content }
            if tool_call_id == "d-c2" && content == ABORTED_TOOL_RESULT
    )));

    // The NEXT commit is what makes the normalized vector durable.
    let next = write(
        &bundle,
        "session-ac-d6",
        &payload(session.manager.clone(), session.workers.clone()),
        ProcessEnding::InProgress,
    );
    assert!(next.members_written.contains(&MANAGER_FILE.to_string()));
    let after = fingerprints(&bundle);
    assert_ne!(
        after, before,
        "the next commit must have rewritten manager.json"
    );
    let manager_now = member_text(&bundle, MANAGER_FILE);
    assert!(
        manager_now.contains(ABORTED_TOOL_RESULT) && manager_now.contains("d-c2"),
        "the next commit is the first place the normalized bytes appear"
    );

    // …and the rewritten bundle needs no second repair.
    let (second, second_report) = reload(&bundle);
    assert_eq!(second_report.aborted_results_synthesized, 0);
    assert_eq!(second_report.orphan_tool_results_pruned, 0);
    assert_eq!(second.manager.messages, session.manager.messages);
    cleanup(&bundle);
}

/// **AC-D7** — the corrupt-member ladder: **two** corrupt worker files (one flipped
/// byte each, so every fingerprint mismatches) mean `Refused` with every member
/// byte unchanged; **exactly one** corrupt worker means `PartiallyRestored` with
/// `unrecoverable_workers == [<that key>]`, while every other criterion keeps
/// holding for the surviving workers.
#[test]
fn ac_d7_two_corrupt_workers_refuse_one_corrupt_worker_degrades_loudly() {
    const ALPHA: &str = "coder-ac-d7-alpha";
    const BETA: &str = "coder-ac-d7-beta";
    let workers = vec![
        worker(
            ALPHA,
            2,
            worker_history(ALPHA),
            DanglingToolCallState::Balanced,
        ),
        worker(
            BETA,
            3,
            worker_history(BETA),
            DanglingToolCallState::Balanced,
        ),
    ];

    // ---------------- two corrupt members: fail closed ----------------
    let bundle = bundle_dir("ac-d7-two");
    write(
        &bundle,
        "session-ac-d7",
        &payload(
            manager_section(pinned_transcript(), DanglingToolCallState::Balanced),
            workers.clone(),
        ),
        ProcessEnding::InProgress,
    );
    let alpha_rel = corrupt_one_byte(&bundle, ALPHA);
    let beta_rel = corrupt_one_byte(&bundle, BETA);
    let before = fingerprints(&bundle);
    assert_eq!(before.len(), 5, "manager + ui + 2 workers + manifest");

    let outcome = load_bundle(&bundle);
    let refusal = refusal_of(&outcome);
    assert_eq!(
        refusal.code,
        RefusalCode::Fingerprint,
        "a flipped byte keeps byte_len and breaks the fingerprint: {}",
        explain(&outcome)
    );
    assert!(
        refusal.member.as_deref() == Some(alpha_rel.as_str())
            || refusal.member.as_deref() == Some(beta_rel.as_str()),
        "the refusal must name a failing worker member, got {:?}",
        refusal.member
    );
    assert!(
        refusal.message.contains("single"),
        "the sentence must explain why the single-corrupt-worker tolerance does not apply: {}",
        refusal.message
    );
    assert!(
        refusal.message.contains(&alpha_rel) && refusal.message.contains(&beta_rel),
        "every failing member must be named: {}",
        refusal.message
    );
    assert_bundle_bytes_unchanged(&bundle, &before);
    cleanup(&bundle);

    // ---------------- exactly one corrupt member: loud partial ----------------
    let one = bundle_dir("ac-d7-one");
    write(
        &one,
        "session-ac-d7-one",
        &payload(
            manager_section(pinned_transcript(), DanglingToolCallState::Balanced),
            workers,
        ),
        ProcessEnding::InProgress,
    );
    let corrupt_rel = corrupt_one_byte(&one, BETA);
    let before_one = fingerprints(&one);

    let outcome = load_bundle(&one);
    let RestoreOutcome::PartiallyRestored {
        session,
        unrecoverable_workers,
        report,
        ending,
    } = outcome
    else {
        panic!(
            "AC-D7: exactly one corrupt worker member must partially restore, got {}",
            explain(&outcome)
        );
    };
    assert_eq!(unrecoverable_workers, vec![BETA.to_string()]);
    assert_eq!(report.workers_unrecoverable, vec![BETA.to_string()]);
    assert_eq!(ending, ProcessEnding::HardKillInferred);

    // The lost worker is reported, not hidden: identity kept, history gone, reason
    // verbatim, and a `WorkerHistory` notice naming its key.
    let beta = session
        .workers
        .iter()
        .find(|w| w.worker_key == BETA)
        .expect("the unrecoverable worker keeps its pane identity");
    assert_eq!(beta.status, WorkerStatus::Unrecoverable);
    assert!(beta.history.is_empty(), "its history is gone, not invented");
    assert!(
        beta.unrecoverable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("fingerprint")),
        "the reason must be the refusal's own sentence: {:?}",
        beta.unrecoverable_reason
    );
    assert!(
        report
            .notices
            .iter()
            .any(|notice| notice.kind == NonRestorableKind::WorkerHistory
                && notice.subject == BETA),
        "AC-D7: the loss must be a first-class notice: {:?}",
        report.notices
    );
    assert!(
        warnings(&report).iter().any(|w| w.contains(&corrupt_rel)),
        "the failing member path must appear in warnings: {:?}",
        warnings(&report)
    );

    // Every other criterion still holds for the surviving worker.
    let alpha = session
        .workers
        .iter()
        .find(|w| w.worker_key == ALPHA)
        .expect("the surviving worker must be restored");
    assert_eq!(
        alpha.history,
        worker_history(ALPHA),
        "AC-D7: the surviving worker's history must be element-wise equal"
    );
    assert_eq!(alpha.status, WorkerStatus::InProgress);
    assert_eq!(alpha.worker_id_seq_value, 2);
    assert_eq!(alpha.completed_seq, Some(2));
    assert_eq!(alpha.brief, format!("brief for {ALPHA}"));
    assert_eq!(report.workers_restored, 1);
    let counts = report
        .worker_counts
        .iter()
        .find(|c| c.key == ALPHA)
        .expect("per-worker counts for the survivor");
    assert_eq!(counts.before, worker_history(ALPHA).len());
    assert_eq!(
        counts.before, counts.after,
        "no repair was needed for the survivor"
    );
    // …and for the manager: AC-P1's pins and AC-D1's verdicts still hold here.
    assert_eq!(
        system_bytes(&session.manager.messages, 0),
        PIN_TEXT,
        "AC-P1's pinned pair still holds on a partially restored bundle"
    );
    assert_eq!(
        user_bytes(&session.manager.messages, 1),
        GOAL_TEXT,
        "AC-P1's pinned pair still holds on a partially restored bundle"
    );
    assert!(report.pins_verified);
    assert_eq!(report.aborted_results_synthesized, 0);
    assert_eq!(report.normalization_passes, 2);
    assert!(report.normalization_stable);

    // Read-only, even for the corrupt member: the loader never repairs a file.
    assert_bundle_bytes_unchanged(&one, &before_one);
    cleanup(&one);
}
