//! Session-restore coverage for the *workers* and the *identities* of a session
//! across a restart.
//!
//! * the **worker section round-trips exactly**: the committed `worker_key` set is
//!   restored modulo the completed-ring cap, with the drop count equal to the
//!   difference; `history` is equal element-by-element and conditional on
//!   `history_source == LiveMirror`, with a `SeedOnly` fixture refused the claim
//!   and reported as the documented partial; every declared field comes back
//!   verbatim and an unrecognized `status_label` is kept byte-for-byte under
//!   `status == Unknown`; the live `WORKERS` map carries each restored key with its
//!   stored `completed_seq`, `is_active()` ⇔ `completed_seq == None`;
//! * **identity continues across the restart**: no worker id below
//!   `identity_floors.worker_id_seq_floor` is ever re-issued and no restored key is
//!   re-disambiguated with a `#N` suffix; a completion in the new process draws
//!   `completed_seq` strictly above every restored one while the restored relative
//!   order survives; a notice issued after restore can never alias a `notice-<n>`
//!   issued before the crash; `Manifest.session_id` is never re-minted.
//!
//! Fixture idiom is the one already used by `tests/test_session_bundle.rs` and
//! `tests/test_session_restore_dangling.rs`: build a `CapturePayload`, commit it
//! with `session::commit::write_bundle` into a private temp directory, read it
//! back through `session::restore::load_bundle`, and — where the claim is about the
//! *live* process — install it through the seams production uses:
//! `session::install_restored_workers` and, for the ring-cap and session-id rules,
//! the whole boot pass `ui::session::boot`, so the assertions land on
//! `RepairReport` / `BootReport` as the operator sees them (the boot pass copies
//! `worker_report` into `RepairReport`).
//!
//! Two rules this file never breaks:
//!
//! * **no weakened assertion.** Where the stated contract and the implementation
//!   could disagree, the test keeps the contract's wording, pins the
//!   implementation's exact value alongside it, and says so — nothing is relaxed to
//!   make a test pass;
//! * **order independence.** The `WORKERS` map, `WORKER_ID_SEQ`, `COMPLETED_SEQ`
//!   and `NEXT_NOTICE_ID` are process-global, and `remove_worker_entry_for_test`
//!   is `#[cfg(test)]`-only (invisible to an integration test), so this file never
//!   cleans the registry and never counts it absolutely: every registry-touching
//!   test holds one local mutex across commit → install → assert, uses a private
//!   key prefix, and states its expectations relative to the live counters it
//!   reads at runtime (`live_completed_count()`, `worker_id_seq_floor()`,
//!   `notice_id_floor()`), deriving its own `completed_seq` values from
//!   `completed_seq_floor() + 1_000_000` so they are the newest in the process and
//!   the ring can only evict other tests' leftovers. The ring-cap test is the
//!   deliberate exception: it wants the cap to bite, so it commits the lowest seq
//!   values in the file (`1..=14`) and states the survivor set in terms of how many
//!   completed entries it found in the map beforehand. `#[test]` order therefore
//!   cannot change a result.
//!
//! Hermetic: one unique temp directory per test, deleted at the end; no network,
//! no PTY, no sleeps, no reading of another test's leftovers as fact.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use marmennill::agents::Agent;
use marmennill::config::Config;
use marmennill::manager::phase::{MARMEL_DIR, Plan};
use marmennill::orchestrator::notice;
use marmennill::orchestrator::workers::{self, ActiveWorkerInfo, WorkerState};
use marmennill::session::commit::{
    CapturePayload, CommitLock, CommitPolicy, CommitReport, last_manifest, worker_rel_path,
};
use marmennill::session::format::TimePoint;
use marmennill::session::restore::{load_bundle, pinned_prefix_end_records};
use marmennill::session::worker_journal::{project_status, project_worker_section};
use marmennill::session::{
    DanglingToolCallState, HistorySource, IdentityFloors, ManagerSection, MessageRecord,
    NonRestorableKind, NonRestorableNotice, PinWitness, ProcessEnding, RepairReport,
    RestoreOutcome, RestoredSession, ToolCallRecord, ToolFunctionRecord, UiSection, WorkerCounters,
    WorkerSection, WorkerStatus,
};

// ----------------------------------------------------------------- constants

/// `orchestrator::workers::MAX_RECENT_COMPLETED` (`src/orchestrator/workers.rs`).
/// Private to that module, and its value is stated there, so the fixture is built
/// around it rather than re-derived from a map length.
const RING_CAP: usize = 10;

/// Pinned system prompt carried by every fixture bundle.
const PIN_TEXT: &str =
    "PINNED SYSTEM PROMPT (REQ-CORE-001): bytes the previous process sent first.";

/// Pinned goal carried by every fixture bundle.
const GOAL_TEXT: &str = "PINNED GOAL (REQ-CORE-002): the same worker identities after the restart.";

/// Budget the fixtures commit with.
const COMMITTED_BUDGET: usize = 1234;

/// Budget this process boots with — deliberately different, so a re-composition
/// could never be mistaken for a restoration.
const LIVE_BUDGET: usize = 4321;

/// `#` — `KEY_DISAMBIGUATOR` (`src/orchestrator/workers.rs`), the collision handle
/// a restored key must never acquire.
const DISAMBIGUATOR: char = '#';

// -------------------------------------------------------------- serializing

/// Serializes every test in this file that mutates the process-global `WORKERS`
/// map or the process-global identity counters. Without it, two tests would see
/// each other's entries mid-flight (the completed ring is shared, and an
/// eviction is global), which is exactly the order dependence this file forbids.
static REGISTRY: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Poison-tolerant, like every other guard on the session path.
fn registry_lock() -> MutexGuard<'static, ()> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// -------------------------------------------------------------- temp bundle

/// A fresh temp dir whose `.marmel/session` layout mirrors a real plan. The tag
/// is unique per test, so cases never share a directory.
fn bundle_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "marmel-t014c-{tag}-{}-{}",
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

fn root_of(bundle: &Path) -> PathBuf {
    bundle
        .parent()
        .and_then(|p| p.parent())
        .expect("bundle hangs off <root>/.marmel/session")
        .to_path_buf()
}

fn plan_for(bundle: &Path) -> Plan {
    Plan::at(root_of(bundle).join(MARMEL_DIR))
}

fn cleanup(bundle: &Path) {
    std::fs::remove_dir_all(root_of(bundle)).ok();
}

// --------------------------------------------------------- transcript helpers

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

fn pinned_pair() -> Vec<MessageRecord> {
    vec![sys(PIN_TEXT), user(GOAL_TEXT)]
}

fn manager_section(messages: Vec<MessageRecord>) -> ManagerSection {
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
        dangling: DanglingToolCallState::Balanced,
        messages,
    }
}

/// The manager transcript every fixture commits: the pinned pair plus one
/// settled round. Nothing here asserts on it, but a bundle must carry it.
fn manager_transcript() -> Vec<MessageRecord> {
    let mut messages = pinned_pair();
    messages.push(assistant(
        Some("delegating"),
        None,
        vec![call("m-c1", "delegate", "{\"task_id\":\"t-014c\"}")],
    ));
    messages.push(tool("m-c1", "worker finished"));
    messages
}

// ----------------------------------------------------------- worker fixtures

/// Everything a fixture worker states that these tests read. Every other
/// `WorkerSection` field is filled with a distinctive, non-default value so an
/// accidentally-dropped field is caught instead of coincidentally matched.
struct Spec {
    key: String,
    role: Agent,
    task_id: Option<String>,
    worker_id_seq_value: u64,
    status: WorkerStatus,
    status_label: String,
    completed_seq: Option<u64>,
    history_source: HistorySource,
}

/// `Coder`, task-less, `LiveMirror` history — the shape most fixtures need.
fn spec(
    key: &str,
    worker_id: u64,
    status: WorkerStatus,
    label: &str,
    completed: Option<u64>,
) -> Spec {
    Spec {
        key: key.to_string(),
        role: Agent::Coder,
        task_id: None,
        worker_id_seq_value: worker_id,
        status,
        status_label: label.to_string(),
        completed_seq: completed,
        history_source: HistorySource::LiveMirror,
    }
}

impl Spec {
    fn with_role(mut self, role: Agent) -> Self {
        self.role = role;
        self
    }

    fn with_task(mut self, task_id: &str) -> Self {
        self.task_id = Some(task_id.to_string());
        self
    }

    fn with_source(mut self, source: HistorySource) -> Self {
        self.history_source = source;
        self
    }
}

fn worker(spec: &Spec, history: Vec<MessageRecord>) -> WorkerSection {
    let capture_loss = if spec.history_source == HistorySource::LiveMirror {
        Vec::new()
    } else {
        // The capture side records this loss in the section itself
        // (`worker_journal::project_worker_section`), so a faithful fixture does
        // too, and the round trip must carry it back.
        vec![NonRestorableNotice {
            kind: NonRestorableKind::WorkerHistory,
            subject: spec.key.clone(),
            detail: format!(
                "history for {} is {:?}: the mirrored transcript was never observed, so the \
                 persisted record carries the brief only",
                spec.key, spec.history_source
            ),
        }]
    };
    WorkerSection {
        worker_key: spec.key.clone(),
        worker_id_seq_value: spec.worker_id_seq_value,
        role: spec.role,
        task_id: spec.task_id.clone(),
        status: spec.status.clone(),
        status_label: spec.status_label.clone(),
        started: TimePoint::now(),
        completed: spec.completed_seq.map(|_| TimePoint::now()),
        last_activity: Some(TimePoint::now()),
        worked_secs: 42.5,
        completed_seq: spec.completed_seq,
        context_tokens: 1234,
        last_tokens: 567,
        counters: WorkerCounters {
            implementation_turns: 3,
            validation_rounds: 2,
            latest_validator_feedback: Some(
                "validator: the error path has no assertion".to_string(),
            ),
        },
        brief: format!("brief for {}", spec.key),
        snippets: vec!["snippet-a".to_string(), "snippet-b".to_string()],
        image_urls: vec!["marmel://image/shot-1.png".to_string()],
        audio_urls: vec!["marmel://audio/note-1.wav".to_string()],
        recursion_granted: true,
        history,
        history_source: spec.history_source.clone(),
        dangling: DanglingToolCallState::Balanced,
        unrecoverable_reason: None,
        non_restorable: capture_loss,
    }
}

/// A worker transcript with the awkward shapes `MessageRecord` promises to keep:
/// `content: null` vs `Some("")`, `reasoning_content`, and `arguments` that is
/// not even valid JSON (verbatim, never re-canonicalized).
fn worker_history(key: &str) -> Vec<MessageRecord> {
    vec![
        sys(&format!("specialist prompt for {key}")),
        user(&format!("specialist brief for {key}")),
        assistant(
            None,
            Some("thinking about the file layout"),
            vec![call(
                &format!("{key}-c1"),
                "read_file",
                "{\"path\":\"src/lib.rs\"}",
            )],
        ),
        tool(&format!("{key}-c1"), "lib bytes"),
        assistant(
            Some(""),
            None,
            vec![
                call(
                    &format!("{key}-c2"),
                    "replace",
                    "{\"path\":\"src/lib.rs\",\"old_str\":\"{oops",
                ),
                call(
                    &format!("{key}-c3"),
                    "grep_search",
                    "{\"pattern\":\"ünïcode ✓\"}",
                ),
            ],
        ),
        tool(&format!("{key}-c2"), "replaced 1 occurrence"),
        tool(&format!("{key}-c3"), "no matches"),
        assistant(Some("done"), None, Vec::new()),
    ]
}

// ------------------------------------------------------------- commit / load

fn payload(workers: Vec<WorkerSection>, floors: IdentityFloors) -> CapturePayload {
    CapturePayload {
        manager: manager_section(manager_transcript()),
        ui: Some(UiSection {
            status_line: "t014c status".to_string(),
            ..UiSection::default()
        }),
        workers,
        identity_floors: floors,
    }
}

/// The public commit seam, unthrottled.
fn write(bundle: &Path, session_id: &str, payload: &CapturePayload) -> CommitReport {
    let policy = CommitPolicy {
        min_interval: std::time::Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    marmennill::session::commit::write_bundle(
        bundle,
        session_id,
        payload,
        ProcessEnding::InProgress,
        &policy,
        &CommitLock::new(),
    )
    .expect("commit must succeed")
}

/// One human-readable sentence for "this outcome was not the expected one".
fn explain(outcome: &RestoreOutcome) -> String {
    match outcome {
        RestoreOutcome::NoBundle { bundle, .. } => {
            format!("NoBundle at {}", bundle.display())
        }
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

// ------------------------------------------------------------ boot harness

/// Minimal `Renderer` for the boot pass: records events and lets the pump capture
/// an honest (empty) `UiSection` through the trait's documented default.
#[derive(Default)]
struct BootRenderer {
    events: Vec<marmennill::ui::Event>,
    input_state: marmennill::ui::InputState,
}

impl BootRenderer {
    /// The status lines the boot pass announced: repair and loss are never
    /// silent.
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

/// Everything a full boot left behind.
struct Booted {
    report: marmennill::ui::session::BootReport,
    engine: marmennill::manager::ContextEngine,
    session_id: String,
    plan: Plan,
    /// The status lines the boot pass surfaced: a silent repair is forbidden, so
    /// "the operator was told" is checkable.
    statuses: Vec<String>,
}

/// THE boot path (`ui::session::boot`): `load_bundle` → floors + `WORKERS` → the
/// engine → the panes → the loud report. Used where a claim is about a
/// `RepairReport` field or about the session identity.
fn boot_bundle(bundle: &Path) -> Booted {
    let cfg = Config {
        max_context_tokens: LIVE_BUDGET,
        ..Config::default()
    };
    let plan = plan_for(bundle);
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
    assert!(
        report.restored,
        "the boot pass reported a fresh start instead of restoring the bundle"
    );
    Booted {
        statuses: renderer.statuses(),
        report,
        engine,
        session_id,
        plan,
    }
}

/// The boot pass's `Config` (the pump needs the same one).
fn boot_config() -> Config {
    Config {
        max_context_tokens: LIVE_BUDGET,
        ..Config::default()
    }
}

// ------------------------------------------------------- live-registry reads

fn live_completed_count() -> usize {
    workers::live_worker_states()
        .into_iter()
        .filter(|(_, state)| state.completed_seq.is_some())
        .count()
}

/// Every live key under a fixture's private prefix. Prefix filtering isolates one
/// test's own entries from another test's leftovers; it is never a basis for
/// reasoning about a worker's identity.
fn live_keys_with_prefix(prefix: &str) -> BTreeSet<String> {
    workers::live_worker_states()
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with(prefix))
        .collect()
}

fn live_entry(key: &str) -> WorkerState {
    workers::worker_state_snapshot(key)
        .unwrap_or_else(|| panic!("the live WORKERS map must hold an entry for '{key}'"))
}

fn committed_keys(sections: &[WorkerSection]) -> BTreeSet<String> {
    sections.iter().map(|w| w.worker_key.clone()).collect()
}

fn completed_pairs(sections: &[WorkerSection]) -> Vec<(u64, String)> {
    sections
        .iter()
        .filter_map(|w| w.completed_seq.map(|seq| (seq, w.worker_key.clone())))
        .collect()
}

// ==========================================================================
// The worker section
// ==========================================================================

/// The restored `worker_key` set equals the committed set modulo the completed-ring
/// cap, with `RepairReport.completed_dropped_by_ring_cap` equal to the difference.
///
/// The fixture commits **16** workers: 14 completed (with 14 distinct
/// `completed_seq` values) and 2 in flight. 14 > `RING_CAP`, so the cap must
/// bite. Two halves are asserted:
///
/// 1. **the loader** returns all 16 keys — the files are the durable record and
///    the cap is a live-map property, never a loader one;
/// 2. **the live `WORKERS` map** (after the real boot pass, so the number is the
///    `RepairReport` field the boot pass copies there) holds the committed set minus
///    exactly the entries the ring evicted, and
///    `completed_dropped_by_ring_cap == committed − present`.
///
/// Order independence: the live map may already hold `K` completed entries left
/// by an earlier test in this binary, so the expectation is stated in terms of
/// `K`, read at runtime. The fixture's own `completed_seq` values (`1..=14`) are
/// the lowest in the binary on purpose, so the eviction always lands on this
/// test's entries and `dropped == difference` is the exact claim rather than a
/// count polluted by other tests' leftovers.
#[test]
fn ac_w1_the_restored_key_set_equals_the_committed_set_modulo_the_completed_ring_cap() {
    let _registry = registry_lock();
    let bundle = bundle_dir("ac-w1");
    const PREFIX: &str = "coder-acw1";

    let mut sections = Vec::new();
    for n in 0..14u64 {
        let key = format!("{PREFIX}-w{n}");
        sections.push(worker(
            &spec(
                &key,
                n + 1,
                WorkerStatus::Completed,
                "Completed",
                // Lowest seq values in this binary: the ring always evicts these
                // first, so the reported drop count is this fixture's difference.
                Some(n + 1),
            ),
            worker_history(&key),
        ));
    }
    let in_flight_a = format!("{PREFIX}-flight-a");
    let in_flight_b = format!("{PREFIX}-flight-b");
    for key in [&in_flight_a, &in_flight_b] {
        sections.push(worker(
            &spec(key, 15, WorkerStatus::InProgress, "In Progress", None),
            worker_history(key),
        ));
    }
    let payload = payload(
        sections.clone(),
        IdentityFloors {
            worker_id_seq_floor: 20,
            completed_seq_floor: 15,
            notice_id_floor: 1,
        },
    );
    write(&bundle, "session-ac-w1", &payload);

    // ---- half 1: the loader returns every committed key --------------------
    let (restored, report) = reload(&bundle);
    let committed = committed_keys(&sections);
    assert_eq!(committed.len(), 16, "fixture sanity");
    assert_eq!(
        committed_keys(&restored.workers),
        committed,
        "AC-W1/§5.8 I-W1: load_bundle must return every committed worker_key — the \
         completed-ring cap bounds the live working set, never the durable record"
    );
    assert_eq!(
        report.workers_restored,
        committed.len(),
        "RepairReport::workers_restored must count every restored section"
    );

    // ---- half 2: the live map, through the real boot wiring ----------------
    let held_before = live_completed_count();
    assert!(
        held_before <= RING_CAP,
        "the live map should never hold more than {RING_CAP} completed entries, it holds \
         {held_before} — the eviction policy this criterion depends on is not the one assumed"
    );

    let booted = boot_bundle(&bundle);
    let dropped = booted.report.repair.completed_dropped_by_ring_cap;
    let present = live_keys_with_prefix(PREFIX);

    let difference = committed.len() - present.len();
    assert_eq!(
        dropped,
        difference,
        "AC-W1: RepairReport.completed_dropped_by_ring_cap must equal the difference between \
         the committed set ({} key(s)) and the set that survived in the live map ({} key(s)); \
         the report says {dropped}",
        committed.len(),
        present.len()
    );
    assert!(
        dropped > 0,
        "the fixture must actually exercise the cap: 14 committed completed entries against a \
         ring of {RING_CAP} dropped {dropped}"
    );
    assert_eq!(
        dropped,
        held_before + 14 - RING_CAP,
        "the ring evicts oldest-first past its cap: {held_before} pre-existing completed \
         entr(ies) plus this bundle's 14 against a cap of {RING_CAP} must evict exactly that \
         many, and the report must say so"
    );
    assert_eq!(
        booted.report.workers_installed,
        committed.len(),
        "every restored worker was installed under its stored key; the ring cap runs after the \
         insertion, so `installed` counts all of them"
    );

    // The survivors are exactly the highest-seq committed completed entries: the
    // `K` entries already in the map (higher seq) fill the ring first, so
    // `RING_CAP - K` of this fixture's own entries can remain.
    let mut ordered = completed_pairs(&sections);
    ordered.sort();
    let keep = RING_CAP - held_before;
    let expected_survivors: BTreeSet<String> = ordered
        .iter()
        .rev()
        .take(keep)
        .map(|(_, key)| key.clone())
        .collect();
    let present_completed: BTreeSet<String> = present
        .iter()
        .filter(|key| {
            live_entry(key).completed_seq.is_some() && **key != in_flight_a && **key != in_flight_b
        })
        .cloned()
        .collect();
    assert_eq!(
        present_completed, expected_survivors,
        "AC-W1: the survivors must be the highest-seq committed completed entries (the ring \
         evicts oldest-first); {keep} of 14 could remain"
    );

    // An in-flight entry is never capped away: it has no `completed_seq`, so it is
    // not part of the ring at all.
    for key in [&in_flight_a, &in_flight_b] {
        assert!(
            present.contains(key),
            "AC-W1: the in-flight key '{key}' must always survive — the completed-ring cap \
             bounds completed entries only"
        );
    }

    // A drop this visible is never silent — the boot pass announced the
    // restore rather than swallowing the difference.
    assert!(
        !booted.statuses.is_empty(),
        "AC-W1/§8.4: the boot pass must say out loud what it restored (and what it dropped); \
         it surfaced no status line at all"
    );

    cleanup(&bundle);
}

/// Per worker, `history` is element-by-element equal **and**
/// `history_source == LiveMirror`; a `SeedOnly` fixture must NOT satisfy that
/// conjunction and must instead produce the reported partial.
///
/// Three things are asserted, and none of them is a re-statement of another:
///
/// * the `LiveMirror` worker's history comes back element-by-element equal —
///   including `content: null` vs `Some("")`, `reasoning_content`, and
///   `arguments` that is not valid JSON (kept verbatim);
/// * the `SeedOnly` worker is **not** presented as an exact history: its
///   `history_source` stays `SeedOnly` (so the conjunction fails by the provenance
///   term, exactly as required) and its `history` is the empty vector the capture
///   side wrote;
/// * the partial is *reported*, twice over: the capture-side `NonRestorableNotice
///   { kind: WorkerHistory }` survives the round trip inside the section, and the
///   installer re-states it for the live projection — while the `LiveMirror`
///   worker never appears in a `WorkerHistory` notice.
#[test]
fn ac_w2_history_is_element_by_element_equal_only_for_a_live_mirror_and_a_seed_only_worker_is_reported()
 {
    let _registry = registry_lock();
    let bundle = bundle_dir("ac-w2");
    const PREFIX: &str = "coder-acw2";

    let mirror_key = format!("{PREFIX}-mirror");
    let seed_key = format!("{PREFIX}-seedonly");
    let mirror_spec = spec(
        &mirror_key,
        1_000_001,
        WorkerStatus::InProgress,
        "Revising (rejected pass 1/5)",
        None,
    );
    let seed_spec = spec(
        &seed_key,
        1_000_002,
        WorkerStatus::InProgress,
        "In Progress",
        None,
    )
    .with_source(HistorySource::SeedOnly);

    let mirror_history = worker_history(&mirror_key);
    let sections = vec![
        worker(&mirror_spec, mirror_history.clone()),
        worker(&seed_spec, Vec::new()),
    ];
    let payload = payload(
        sections.clone(),
        IdentityFloors {
            worker_id_seq_floor: 1_000_100,
            completed_seq_floor: 1_000_100,
            notice_id_floor: 1,
        },
    );
    write(&bundle, "session-ac-w2", &payload);

    let (restored, report) = reload(&bundle);
    assert_eq!(report.workers_restored, 2, "{:?}", report.warnings);

    let mirror = restored
        .workers
        .iter()
        .find(|w| w.worker_key == mirror_key)
        .expect("the LiveMirror worker must be restored");
    let seed_only = restored
        .workers
        .iter()
        .find(|w| w.worker_key == seed_key)
        .expect("the SeedOnly worker must be restored");

    // ---- provenance decides, and it survives the round trip --------------
    assert_eq!(
        mirror.history_source,
        HistorySource::LiveMirror,
        "AC-W2/§5.8 I-W4: 'full history restored' may only be claimed for a LiveMirror history"
    );
    assert_eq!(
        seed_only.history_source,
        HistorySource::SeedOnly,
        "AC-W2: a SeedOnly fixture must never be silently upgraded to LiveMirror"
    );
    assert_ne!(
        seed_only.history_source,
        HistorySource::LiveMirror,
        "AC-W2: the SeedOnly fixture must NOT satisfy the LiveMirror term of the criterion"
    );

    // ---- element-by-element equality for the LiveMirror worker --------------
    assert_eq!(
        mirror.history.len(),
        mirror_history.len(),
        "AC-W2: the mirrored history must come back with the same record count, got {} for {}",
        mirror.history.len(),
        mirror_history.len()
    );
    for (index, (committed, given)) in mirror_history.iter().zip(mirror.history.iter()).enumerate()
    {
        assert_eq!(
            given, committed,
            "AC-W2: history[{index}] of {mirror_key} differs from the committed record \
             (restored {given:?} vs committed {committed:?})"
        );
    }
    // The awkward shapes the "element-by-element" wording is about,
    // named out loud so a passing test cannot be a coincidence.
    assert_eq!(
        mirror.history[2].content(),
        None,
        "AC-W2/§5.8 I-W4: content: null must stay null"
    );
    assert_eq!(
        mirror.history[4].content().map(str::to_string),
        Some(String::new()),
        "AC-W2/§5.8 I-W4: content: \"\" is a different fact from null and must stay \"\""
    );
    let MessageRecord::Assistant {
        reasoning_content, ..
    } = &mirror.history[2]
    else {
        panic!("fixture shape: history[2] is an assistant turn");
    };
    assert_eq!(
        reasoning_content.as_deref(),
        Some("thinking about the file layout"),
        "AC-W2/§5.8 I-W4: reasoning_content must round-trip"
    );
    let MessageRecord::Assistant { tool_calls, .. } = &mirror.history[4] else {
        panic!("fixture shape: history[4] is an assistant turn with calls");
    };
    assert_eq!(
        tool_calls[0].function.arguments, "{\"path\":\"src/lib.rs\",\"old_str\":\"{oops",
        "AC-W2/§5.8 I-W4: arguments is an opaque string and must be verbatim, never re-canonicalized"
    );
    assert_eq!(
        tool_calls[1].function.arguments, "{\"pattern\":\"ünïcode ✓\"}",
        "AC-W2: non-ASCII arguments must survive byte-for-byte"
    );

    // The repair report agrees that nothing had to be healed for this worker.
    let mirror_counts = report
        .worker_counts
        .iter()
        .find(|counts| counts.key == mirror_key)
        .unwrap_or_else(|| panic!("RepairReport::worker_counts must name {mirror_key}"));
    assert_eq!(
        (mirror_counts.before, mirror_counts.after),
        (mirror_history.len(), mirror_history.len()),
        "AC-W2: a pair-valid mirrored history must be reported as untouched"
    );
    assert!(
        mirror_counts.applied_rules.is_empty(),
        "AC-W2: no repair rule may have fired on a mirrored history: {:?}",
        mirror_counts.applied_rules
    );

    // ---- the SeedOnly partial: reported, never presented as a history -------
    assert!(
        seed_only.history.is_empty(),
        "§5.5 rule 5: a SeedOnly worker is written with an empty history — the loader must not \
         invent records, it got {:?}",
        seed_only.history
    );
    assert!(
        seed_only
            .non_restorable
            .iter()
            .any(|notice| notice.kind == NonRestorableKind::WorkerHistory
                && notice.subject == seed_key),
        "AC-W2: the capture-side WorkerHistory loss must survive inside the section: {:?}",
        seed_only.non_restorable
    );

    let install =
        marmennill::session::install_restored_workers(&restored.workers, &restored.identity_floors);
    assert_eq!(install.installed, 2, "{:?}", install.warnings);
    let reported: Vec<&NonRestorableNotice> = install
        .notices
        .iter()
        .filter(|notice| notice.kind == NonRestorableKind::WorkerHistory)
        .collect();
    assert!(
        reported.iter().any(|notice| notice.subject == seed_key),
        "AC-W2: the SeedOnly partial must be reported as a NonRestorableNotice of kind \
         WorkerHistory naming {seed_key}: {:?}",
        install.notices
    );
    assert!(
        !reported.iter().any(|notice| notice.subject == mirror_key),
        "AC-W2: the LiveMirror worker must NOT be reported as a history partial"
    );

    cleanup(&bundle);
}

/// Per worker: `role`, `task_id`, `status`, `status_label`,
/// `context_tokens`, `last_tokens`, `counters.*`, `brief`, `snippets`,
/// `image_urls`, `audio_urls`, `recursion_granted` all equal, and an
/// unrecognized `status_label` keeps the label verbatim with
/// `status == Unknown`.
///
/// Both directions are covered, because "unrecognized" is decided by the capture
/// side and only *stored* by the loader:
///
/// * **capture** — `project_status` / `project_worker_section` on a live entry
///   whose free-form label this build does not know must yield
///   `WorkerStatus::Unknown` with the label bytes untouched (never coerced into
///   `Completed`/`Failed`/`Aborted`);
/// * **load** — every declared field of the committed section comes back equal,
///   field by field, and the `Unknown` + verbatim-label pair round-trips.
#[test]
fn ac_w3_every_declared_field_is_restored_verbatim_and_an_unrecognized_label_keeps_its_bytes() {
    let bundle = bundle_dir("ac-w3");
    const PREFIX: &str = "coder-acw3";

    let known_key = format!("{PREFIX}-approved");
    let unknown_key = format!("{PREFIX}-unknown");
    let known_spec = spec(
        &known_key,
        2_000_001,
        WorkerStatus::Completed,
        "Approved (validated)",
        Some(2_000_001),
    )
    .with_role(Agent::Researcher)
    .with_task("t-014c-known");
    // An unrecognized label this build does not know: the capture side classifies
    // it `Unknown`, and the label must be kept byte-for-byte.
    let unknown_label = "Revising (rejected pass 3/5)";
    let unknown_spec = spec(
        &unknown_key,
        2_000_002,
        WorkerStatus::Unknown,
        unknown_label,
        None,
    )
    .with_role(Agent::Validator)
    .with_task("t-014c-unknown");

    let sections = vec![
        worker(&known_spec, worker_history(&known_key)),
        worker(&unknown_spec, worker_history(&unknown_key)),
    ];
    let payload = payload(
        sections.clone(),
        IdentityFloors {
            worker_id_seq_floor: 2_000_100,
            completed_seq_floor: 2_000_100,
            notice_id_floor: 1,
        },
    );
    write(&bundle, "session-ac-w3", &payload);

    let (restored, report) = reload(&bundle);
    assert_eq!(report.workers_restored, 2, "{:?}", report.warnings);

    for committed in &sections {
        let got = restored
            .workers
            .iter()
            .find(|w| w.worker_key == committed.worker_key)
            .unwrap_or_else(|| {
                panic!(
                    "AC-W3: {} is missing from the restored set",
                    committed.worker_key
                )
            });

        // Every declared field, asserted one at a time so a failure names the
        // field instead of dumping two sections.
        assert_eq!(
            got.worker_key, committed.worker_key,
            "AC-W3: worker_key must be the exact stored key (the authority, §5.2)"
        );
        assert_eq!(
            got.worker_id_seq_value, committed.worker_id_seq_value,
            "AC-W3: worker_id_seq_value must be the stored draw, never a re-draw"
        );
        assert_eq!(
            got.role, committed.role,
            "AC-W3: role of {}",
            committed.worker_key
        );
        assert_eq!(
            got.task_id, committed.task_id,
            "AC-W3: task_id binding of {}",
            committed.worker_key
        );
        assert_eq!(
            got.status, committed.status,
            "AC-W3: status (lifecycle class) of {}",
            committed.worker_key
        );
        assert_eq!(
            got.status_label, committed.status_label,
            "AC-W3: status_label must be the pre-crash display bytes verbatim"
        );
        assert_eq!(
            got.status_label.as_bytes(),
            committed.status_label.as_bytes(),
            "AC-W3: status_label must be byte-identical, not merely equal"
        );
        assert_eq!(
            got.context_tokens, committed.context_tokens,
            "AC-W3: context_tokens of {} must not reset to 0",
            committed.worker_key
        );
        assert_eq!(
            got.last_tokens, committed.last_tokens,
            "AC-W3: last_tokens of {}",
            committed.worker_key
        );
        assert_eq!(
            got.counters.implementation_turns, committed.counters.implementation_turns,
            "AC-W3: counters.implementation_turns of {}",
            committed.worker_key
        );
        assert_eq!(
            got.counters.validation_rounds, committed.counters.validation_rounds,
            "AC-W3: counters.validation_rounds of {} gates the fix loop",
            committed.worker_key
        );
        assert_eq!(
            got.counters.latest_validator_feedback, committed.counters.latest_validator_feedback,
            "AC-W3: counters.latest_validator_feedback of {}",
            committed.worker_key
        );
        assert_eq!(
            got.brief, committed.brief,
            "AC-W3: brief (the delegation seed) of {}",
            committed.worker_key
        );
        assert_eq!(
            got.snippets, committed.snippets,
            "AC-W3: snippets of {}",
            committed.worker_key
        );
        assert_eq!(
            got.image_urls, committed.image_urls,
            "AC-W3: image_urls of {}",
            committed.worker_key
        );
        assert_eq!(
            got.audio_urls, committed.audio_urls,
            "AC-W3: audio_urls of {}",
            committed.worker_key
        );
        assert_eq!(
            got.recursion_granted, committed.recursion_granted,
            "AC-W3: recursion_granted of {}",
            committed.worker_key
        );

        // The clock points, asserted alongside the fields displayed next to
        // them: they must round-trip too, and are never recomputed.
        assert_eq!(
            got.worked_secs, committed.worked_secs,
            "AC-W3/§5.6 T3: worked_secs is a stored number, never a difference"
        );
        assert_eq!(
            got.started.wall, committed.started.wall,
            "AC-W3: started.wall of {} must round-trip",
            committed.worker_key
        );
        assert_eq!(
            got.completed.is_some(),
            committed.completed.is_some(),
            "AC-W3: the completed clock must be present exactly when it was"
        );
        assert_eq!(
            got.last_activity.is_some(),
            committed.last_activity.is_some(),
            "AC-W3: last_activity must be present exactly when it was"
        );
    }

    // ---- the unrecognized-label half, on the LOAD side ---------------------
    let unknown = restored
        .workers
        .iter()
        .find(|w| w.worker_key == unknown_key)
        .expect("the Unknown fixture must be restored");
    assert_eq!(
        unknown.status,
        WorkerStatus::Unknown,
        "AC-W3: an unrecognized status_label must arrive as status == Unknown, never coerced"
    );
    assert_eq!(
        unknown.status_label, unknown_label,
        "AC-W3: the unrecognized label must be kept verbatim, it arrived as {:?}",
        unknown.status_label
    );

    // ---- the unrecognized-label half, on the CAPTURE side ------------------
    // `status` is decided here, so this is where "unrecognized" is actually proven.
    let live = WorkerState {
        info: ActiveWorkerInfo {
            task_id: Some("t-014c-live".to_string()),
            agent_name: Agent::Coder.as_str().to_string(),
            prompt: "the live brief".to_string(),
            started_at: Instant::now(),
            started_wall: chrono::Local::now(),
            context_tokens: 4242,
            implementation_turns: 5,
            validation_rounds: 3,
            latest_validator_feedback: Some("validator: keep the label verbatim".to_string()),
            status: unknown_label.to_string(),
            cancel_token: None,
        },
        last_tokens: 4242,
        completed_seq: Some(77),
        completed_at: Some(Instant::now()),
        completed_wall: Some(chrono::Local::now()),
        worker_id_seq_value: 41,
    };
    assert_eq!(
        project_status(&live),
        WorkerStatus::Unknown,
        "AC-W3: project_status must classify an unrecognized label as Unknown"
    );
    let projected = project_worker_section(&format!("{PREFIX}-live"), &live, Agent::Coder, None);
    assert_eq!(projected.status, WorkerStatus::Unknown);
    assert_eq!(
        projected.status_label.as_bytes(),
        unknown_label.as_bytes(),
        "AC-W3: the capture side must write the unrecognized label verbatim"
    );
    // The recognized vocabulary still classifies, so `Unknown` is a verdict
    // rather than the catch-all the test would otherwise be proving.
    for (label, expected) in [
        ("Completed", WorkerStatus::Completed),
        ("Approved (validated)", WorkerStatus::Completed),
        ("Failed: validator critique", WorkerStatus::Failed),
        ("Aborted by user", WorkerStatus::Aborted),
    ] {
        let mut labelled = live.clone();
        labelled.info.status = label.to_string();
        assert_eq!(
            project_status(&labelled),
            expected,
            "AC-W3: the known label {label:?} must classify as {expected:?}"
        );
    }

    cleanup(&bundle);
}

/// The live `WORKERS` map contains an entry under each restored key with
/// `completed_seq` equal to the stored value, and `is_active()` ⇔
/// `completed_seq == None`.
///
/// The fixture mixes the three terminal classes (each carrying a stored
/// `completed_seq`) with three open records (`completed_seq: None`: two
/// `InProgress`, one `Unknown` from an unrecognized label), and one key carries a
/// `#7` collision handle so the *literal* key is asserted too. `completed_seq`
/// values are derived from the live `COMPLETED_SEQ` floor at runtime — the ring is
/// process-global, and a fixture whose entries can be evicted by another test's
/// leftovers cannot prove anything about *carrying* the stored value.
#[test]
fn ac_w4_the_live_registry_holds_each_restored_key_with_its_stored_completed_seq() {
    let _registry = registry_lock();
    let bundle = bundle_dir("ac-w4");
    const PREFIX: &str = "coder-acw4";
    // The completed ring (`MAX_RECENT_COMPLETED`) is process-global, so a fixture
    // that hard-codes its `completed_seq` values can have its *own* entries evicted
    // by whatever another test in this binary installed first — and then the
    // claim ("the live map holds the stored value") is testing nothing.
    // `COMPLETED_SEQ` is advanced past every value it hands out, so every entry
    // already in the map carries a seq strictly below `completed_seq_floor()`:
    // deriving this fixture's seqs from that floor makes them the newest in the
    // process, so evictions land on other tests' leftovers and never on these six
    // entries. Order-independent by construction, not by luck.
    let seq_base = workers::completed_seq_floor() + 1_000_000;

    let completed_key = format!("{PREFIX}-done");
    let failed_key = format!("{PREFIX}-failed");
    let disambiguated_key = format!("{PREFIX}-done{DISAMBIGUATOR}7");
    let open_key = format!("{PREFIX}-no-seq");
    let in_flight_key = format!("{PREFIX}-flight");
    let unknown_key = format!("{PREFIX}-unknown");
    /// A label this build's `project_status` vocabulary does not recognize, so the
    /// section is classified `Unknown` and the label must survive verbatim.
    const UNKNOWN_LABEL: &str = "Revising (rejected pass 3/5)";

    let sections = vec![
        worker(
            &spec(
                &completed_key,
                4_000_001,
                WorkerStatus::Completed,
                "Completed",
                Some(seq_base + 1),
            )
            .with_task("t-014c-w4"),
            worker_history(&completed_key),
        ),
        worker(
            &spec(
                &failed_key,
                4_000_002,
                WorkerStatus::Failed,
                "Failed: validator critique",
                Some(seq_base + 2),
            ),
            worker_history(&failed_key),
        ),
        worker(
            &spec(
                &disambiguated_key,
                4_000_003,
                WorkerStatus::Aborted,
                "Aborted by user",
                Some(seq_base + 3),
            ),
            worker_history(&disambiguated_key),
        ),
        worker(
            &spec(
                &open_key,
                4_000_004,
                WorkerStatus::InProgress,
                "In Progress",
                None,
            ),
            worker_history(&open_key),
        ),
        worker(
            &spec(
                &in_flight_key,
                4_000_005,
                WorkerStatus::InProgress,
                "Revising (rejected pass 2/5)",
                None,
            )
            .with_task("t-014c-w4b"),
            worker_history(&in_flight_key),
        ),
        worker(
            &spec(
                &unknown_key,
                4_000_006,
                WorkerStatus::Unknown,
                UNKNOWN_LABEL,
                None,
            ),
            worker_history(&unknown_key),
        ),
    ];
    let stored: Vec<(String, Option<u64>)> = sections
        .iter()
        .map(|w| (w.worker_key.clone(), w.completed_seq))
        .collect();

    let payload = payload(
        sections.clone(),
        IdentityFloors {
            worker_id_seq_floor: 4_000_100,
            completed_seq_floor: seq_base + 1_000_100,
            notice_id_floor: 1,
        },
    );
    write(&bundle, "session-ac-w4", &payload);

    let (restored, _) = reload(&bundle);
    let install =
        marmennill::session::install_restored_workers(&restored.workers, &restored.identity_floors);
    assert_eq!(install.installed, sections.len(), "{:?}", install.warnings);
    assert!(
        install.rejected_keys.is_empty(),
        "AC-W4: an install must never be refused for a fixture key: {:?}",
        install.rejected_keys
    );

    // Every restored key is present under that exact key, with the stored
    // `completed_seq`, and `is_active()` is exactly the `None` test.
    for (key, stored_seq) in &stored {
        let state = live_entry(key);
        assert_eq!(
            state.completed_seq, *stored_seq,
            "AC-W4/§5.8 I-W5: the live entry for '{key}' must carry completed_seq verbatim \
             (stored {stored_seq:?}, live {:?})",
            state.completed_seq
        );
        assert_eq!(
            state.is_active(),
            stored_seq.is_none(),
            "AC-W4: is_active() must be exactly `completed_seq == None` for '{key}'"
        );
        assert_eq!(
            state.info.task_id,
            sections
                .iter()
                .find(|w| &w.worker_key == key)
                .and_then(|w| w.task_id.clone()),
            "AC-W4: the task binding must be re-established under the same key"
        );
        assert!(
            state.info.cancel_token.is_some(),
            "AC-W4/§5.7 step 5: every installed entry carries a cancellation token (the old one \
             is reported as lost, never silently reused)"
        );
    }

    // The key set is literal — `#7` collision handle included, never re-derived.
    assert_eq!(
        live_keys_with_prefix(PREFIX),
        committed_keys(&sections),
        "AC-W4/§5.8 I-W1: the live map must hold exactly the restored keys under this fixture's \
         prefix, `#` collision handle included (the ring cap cannot drop these: their seq values \
         are the newest in this binary)"
    );
    assert!(
        live_keys_with_prefix(PREFIX).contains(&disambiguated_key),
        "AC-W4: the stored key '{disambiguated_key}' must be inserted verbatim"
    );

    // The open records (`completed_seq: None`) are the ones the restore keeps
    // open, so cancel and routing still see them, and they come back labelled by
    // the documented interrupted label — the one label change this build reports
    // rather than hides.
    let open_keys: Vec<String> = sections
        .iter()
        .filter(|w| w.completed_seq.is_none())
        .map(|w| w.worker_key.clone())
        .collect();
    for key in &open_keys {
        let state = live_entry(key);
        assert!(
            state.is_active(),
            "AC-W4/§5.8 I-W5: '{key}' carries completed_seq None, so is_active() must be true \
             (open, so has_active_workers() and every cancel path see it)"
        );
        assert_eq!(
            state.info.status,
            marmennill::session::INTERRUPTED_STATUS_LABEL,
            "flow §2.2: an open record comes back labelled as interrupted"
        );
    }

    // What the installer *reports* as an interrupted delegation is narrower than
    // what it keeps open, and that is pinned here rather than smoothed over: an
    // `InProgress` record lands in `interrupted_keys`, an `Unknown` one (the
    // unrecognized label) is installed as an open delegation too — fresh token,
    // no join handle, no completion guard — but is surfaced through an explicit
    // warning naming the kept-verbatim label instead of through that list.
    let mut reported_interrupted = install.interrupted_keys.clone();
    reported_interrupted.sort();
    let mut in_progress_keys: Vec<String> = sections
        .iter()
        .filter(|w| w.status == WorkerStatus::InProgress)
        .map(|w| w.worker_key.clone())
        .collect();
    in_progress_keys.sort();
    assert_eq!(
        reported_interrupted, in_progress_keys,
        "AC-W4: every InProgress record must be reported as an open, interrupted delegation"
    );
    let unknown_warned = install.warnings.iter().any(|warning| {
        warning.contains(&unknown_key)
            && warning.contains(UNKNOWN_LABEL)
            && warning.contains("open delegation")
    });
    assert!(
        unknown_warned,
        "AC-W4: the Unknown-status record must be reported out loud (kept label + open \
         delegation), its warnings are {:?}",
        install.warnings
    );

    for key in [&completed_key, &failed_key, &disambiguated_key] {
        assert!(
            !live_entry(key).is_active(),
            "AC-W4/§5.8 I-W5: the terminal record '{key}' carries a stored completed_seq, so it \
             must never look like an active worker"
        );
    }
    assert!(
        workers::has_active_workers(),
        "AC-W4: a restored in-flight delegation must keep the manager's cancel path armed"
    );

    cleanup(&bundle);
}

// ==========================================================================
// Identity continuity across the restart
// ==========================================================================

/// After restore, a fresh worker-id draw is at or above
/// `identity_floors.worker_id_seq_floor`; every restored `worker_id_seq_value`
/// is below the floor, and **no restored key is re-disambiguated
/// with a `#N` suffix** (the key set is compared literally).
///
/// One wording point, pinned rather than papered over: the crate defines a floor
/// as "every id strictly below it has already been handed out"
/// (`orchestrator::workers`, and `IdentityFloors`' own doc in
/// `src/session/format/envelope.rs`), so the floor value itself is the
/// *first legal* draw. A draw is therefore `>= floor` (never below it), and only
/// from the second draw onward is it strictly `> floor`. Both numbers are
/// asserted, together with the exact expected draw, so the difference between the
/// literal `> floor` wording and the documented convention is visible in the
/// failure output — the assertion is not relaxed to whichever form happens to
/// pass.
#[test]
fn ac_i1_no_id_below_the_worker_id_floor_is_re_issued_and_no_restored_key_is_re_disambiguated() {
    let _registry = registry_lock();
    let bundle = bundle_dir("ac-i1");
    const PREFIX: &str = "coder-ac-i1";
    const ID_FLOOR: u64 = 5_000_100;
    // As in the live-map test: completed_seq values are derived from the live
    // counter so this fixture's own entries are always the newest in the ring and
    // can never be evicted by another test's leftovers (order independence).
    let seq_base = workers::completed_seq_floor() + 1_000_000;

    // The natural key of the worker this process will register after the restore.
    let collision_task = "ac-i1-natural";
    let natural_key = format!("coder-{collision_task}");
    let carried_key = format!("{PREFIX}-carried{DISAMBIGUATOR}7");

    let sections = vec![
        worker(
            &spec(
                &natural_key,
                5_000_001,
                WorkerStatus::InProgress,
                "In Progress",
                None,
            )
            .with_task(collision_task),
            worker_history(&natural_key),
        ),
        worker(
            &spec(
                &carried_key,
                5_000_002,
                WorkerStatus::InProgress,
                "Revising (rejected pass 1/5)",
                None,
            )
            .with_task("ac-i1-carried"),
            worker_history(&carried_key),
        ),
        worker(
            &spec(
                format!("{PREFIX}-tail").as_str(),
                5_000_003,
                WorkerStatus::Completed,
                "Completed",
                Some(seq_base + 1),
            )
            .with_task("ac-i1-tail"),
            worker_history(&format!("{PREFIX}-tail")),
        ),
    ];
    let carried_ids: Vec<u64> = sections.iter().map(|w| w.worker_id_seq_value).collect();
    let carried_max = carried_ids.iter().copied().max().unwrap_or(0);
    assert!(
        carried_max < ID_FLOOR,
        "fixture sanity: a carried id must sit below the bundle's floor"
    );

    let payload = payload(
        sections.clone(),
        IdentityFloors {
            worker_id_seq_floor: ID_FLOOR,
            completed_seq_floor: seq_base + 1_000_100,
            notice_id_floor: 1,
        },
    );
    write(&bundle, "session-ac-i1", &payload);

    let (restored, _) = reload(&bundle);
    let committed = committed_keys(&sections);

    // ---- every restored id is below the floor, in the bundle and in the map --
    for section in &restored.workers {
        assert!(
            section.worker_id_seq_value < restored.identity_floors.worker_id_seq_floor,
            "AC-I1/§5.8 I-W2: {} carried WORKER_ID_SEQ {} which is not below the floor {}",
            section.worker_key,
            section.worker_id_seq_value,
            restored.identity_floors.worker_id_seq_floor
        );
    }

    let counter_before = workers::worker_id_seq_floor();
    let install =
        marmennill::session::install_restored_workers(&restored.workers, &restored.identity_floors);
    assert_eq!(install.installed, sections.len(), "{:?}", install.warnings);

    // ---- the floor is raised with fetch_max, never lowered ------------------
    let counter_after = workers::worker_id_seq_floor();
    assert!(
        counter_after >= ID_FLOOR,
        "AC-I1/§5.7 step 1: install must raise WORKER_ID_SEQ to the bundle floor {ID_FLOOR}, \
         it is {counter_after}"
    );
    assert!(
        counter_after >= counter_before,
        "AC-I1: the floor raise is a fetch_max — it must never lower a counter this process \
         already advanced ({counter_before} -> {counter_after})"
    );
    for section in &restored.workers {
        let state = live_entry(&section.worker_key);
        assert_eq!(
            state.worker_id_seq_value, section.worker_id_seq_value,
            "AC-I1/§5.3: the stored WORKER_ID_SEQ draw must be carried into the live entry, \
             never re-drawn"
        );
    }

    // ---- no restored key is re-disambiguated --------------------------------
    let after_restore = live_keys_with_prefix(PREFIX);
    assert_eq!(
        after_restore, committed,
        "AC-I1: the restored key set must be literal — no restored key may gain a `#N` \
         collision handle"
    );
    assert!(
        after_restore.contains(&carried_key),
        "AC-I1: the stored collision handle in '{carried_key}' must be kept verbatim, it is \
         part of the identity"
    );
    for key in &committed {
        assert!(
            after_restore.contains(key),
            "AC-I1: the committed key '{key}' is absent from the rebuilt table"
        );
    }

    // ---- a worker registered after the restore draws above the floor --------
    // Its natural key is the live restored key, so `claim_worker_slot` must
    // disambiguate the NEW worker and leave the restored one untouched — the
    // identity drift the floor rule exists to prevent.
    let first_draw_expected = counter_before.max(ID_FLOOR);
    let first = workers::register_active_worker(
        Some(collision_task.to_string()),
        Agent::Coder.as_str().to_string(),
        "registered after the restore".to_string(),
    );
    let first_state = live_entry(&first.0);
    assert!(
        first.0 != natural_key && !committed.contains(&first.0),
        "AC-I1: the post-restore registration must never clobber the restored key — it took '{}'",
        first.0
    );
    assert!(
        first.0.contains(DISAMBIGUATOR),
        "AC-I1: the colliding newcomer, not the restored worker, carries the `#N` handle \
         (it took '{}')",
        first.0
    );
    assert_eq!(
        first_state.worker_id_seq_value, first_draw_expected,
        "AC-I1: the first draw after a restore lands exactly on the floor (the floor is the \
         first unissued id, `workers.rs:256-262`); expected {first_draw_expected}, given {}",
        first_state.worker_id_seq_value
    );
    assert!(
        first_state.worker_id_seq_value >= ID_FLOOR,
        "AC-I1/§5.8 I-W2: no id below the floor {ID_FLOOR} may be re-issued, the first draw was \
         {}",
        first_state.worker_id_seq_value
    );
    assert!(
        first_state.worker_id_seq_value > carried_max,
        "AC-I1: a post-restore draw must be strictly above every id the bundle carried \
         (max {carried_max}), it drew {}",
        first_state.worker_id_seq_value
    );
    for carried in &carried_ids {
        assert_ne!(
            first_state.worker_id_seq_value, *carried,
            "AC-I1: the post-restore draw aliases the carried id {carried}"
        );
    }

    // The second draw is strictly above the floor — the strict `> floor` wording
    // holds for every draw once the floor's own first value is spent.
    let second = workers::register_active_worker(
        Some("ac-i1-second".to_string()),
        Agent::Coder.as_str().to_string(),
        "second registration after the restore".to_string(),
    );
    let second_state = live_entry(&second.0);
    assert!(
        second_state.worker_id_seq_value > ID_FLOOR,
        "AC-I1: a worker-id draw after the restore must be strictly above the floor \
         {ID_FLOOR}, it drew {}",
        second_state.worker_id_seq_value
    );
    assert!(
        second_state.worker_id_seq_value > first_state.worker_id_seq_value,
        "AC-I1: ids are never reused within a process"
    );
    assert_eq!(
        live_keys_with_prefix(PREFIX),
        {
            let mut expected = committed.clone();
            expected.insert(first.0.clone());
            expected.insert(second.0.clone());
            expected
        },
        "AC-I1: the two newcomers join the table; not one restored key changed"
    );

    // Leave the registry the way the next test expects: the two newcomers are
    // real registrations, so closing them through their own guards is the honest
    // teardown (each draws a fresh completed_seq, which is above every seq this
    // binary has seen — it cannot reorder another test's restored entries).
    drop(second);
    drop(first);

    cleanup(&bundle);
}

/// A completion in the new process draws `completed_seq` strictly above
/// `max(all restored completed_seq)`, and restored completed entries keep their
/// relative order.
#[test]
fn ac_i2_a_completion_in_the_new_process_draws_above_every_restored_completed_seq_and_order_holds()
{
    let _registry = registry_lock();
    let bundle = bundle_dir("ac-i2");
    const PREFIX: &str = "coder-ac-i2";
    // Seq values are derived from the live counter (as in the live-map test):
    // four completed records must all survive the ring cap so their relative
    // order can be compared, whatever ran before this test in the same binary.
    let seq_base = workers::completed_seq_floor() + 1_000_000;

    let in_flight_key = format!("{PREFIX}-flight");
    // Committed in a scrambled order relative to their seq values, so "keeps
    // their relative order" cannot be satisfied by array order alone.
    let sections = vec![
        worker(
            &spec(
                &format!("{PREFIX}-third"),
                6_000_001,
                WorkerStatus::Completed,
                "Completed",
                Some(seq_base + 3),
            ),
            worker_history(&format!("{PREFIX}-third")),
        ),
        worker(
            &spec(
                &format!("{PREFIX}-first"),
                6_000_002,
                WorkerStatus::Completed,
                "Completed",
                Some(seq_base + 1),
            ),
            worker_history(&format!("{PREFIX}-first")),
        ),
        worker(
            &spec(
                &format!("{PREFIX}-fourth"),
                6_000_003,
                WorkerStatus::Aborted,
                "Aborted by user",
                Some(seq_base + 4),
            ),
            worker_history(&format!("{PREFIX}-fourth")),
        ),
        worker(
            &spec(
                &format!("{PREFIX}-second"),
                6_000_004,
                WorkerStatus::Failed,
                "Failed: validator critique",
                Some(seq_base + 2),
            ),
            worker_history(&format!("{PREFIX}-second")),
        ),
        worker(
            &spec(
                &in_flight_key,
                6_000_005,
                WorkerStatus::InProgress,
                "In Progress",
                None,
            ),
            worker_history(&in_flight_key),
        ),
    ];
    let payload = payload(
        sections.clone(),
        IdentityFloors {
            worker_id_seq_floor: 6_000_100,
            completed_seq_floor: seq_base + 1_000_100,
            notice_id_floor: 1,
        },
    );
    write(&bundle, "session-ac-i2", &payload);

    let (restored, _) = reload(&bundle);
    let install =
        marmennill::session::install_restored_workers(&restored.workers, &restored.identity_floors);
    assert_eq!(install.installed, sections.len(), "{:?}", install.warnings);
    assert_eq!(
        live_keys_with_prefix(PREFIX),
        committed_keys(&sections),
        "AC-I2: all four completed records must still be in the live map to compare their order"
    );

    // ---- relative order of the restored completed entries -------------------
    let mut committed_order: Vec<(u64, String)> = completed_pairs(&sections);
    committed_order.sort_by_key(|(seq, _)| *seq);
    let mut live_order: Vec<(u64, String)> = workers::live_worker_states()
        .into_iter()
        .filter(|(key, _)| key.starts_with(PREFIX))
        .filter_map(|(key, state)| state.completed_seq.map(|seq| (seq, key)))
        .collect();
    live_order.sort_by_key(|(seq, _)| *seq);
    assert_eq!(
        live_order, committed_order,
        "AC-I2/§5.8 I-W3: the restored completed entries must keep their relative order — \
         every completed_seq is carried, none re-drawn"
    );

    // ---- a completion in the new process draws strictly above all of them ---
    let max_restored = committed_order
        .iter()
        .map(|(seq, _)| *seq)
        .max()
        .expect("fixture has completed entries");
    let retired = marmennill::session::retire_interrupted_worker(&in_flight_key)
        .expect("closing an open restored record must draw a completed_seq");
    assert!(
        retired > max_restored,
        "AC-I2/§5.8 I-W3: a completion in the new process must draw a value strictly above \
         max(all restored completed_seq) = {max_restored}, it drew {retired}"
    );
    assert!(
        retired >= seq_base + 1_000_100,
        "AC-I2/§5.7 step 1: the draw comes from a counter raised to the bundle floor \
         {}, it drew {retired}",
        seq_base + 1_000_100
    );
    let state = live_entry(&in_flight_key);
    assert_eq!(
        state.completed_seq,
        Some(retired),
        "AC-I2: the closed record keeps the seq it just drew"
    );
    assert!(
        !state.is_active(),
        "AC-I2: a closed record must stop looking like an active worker"
    );

    // The new entry lands at the end; the restored order is untouched.
    let mut live_after: Vec<(u64, String)> = workers::live_worker_states()
        .into_iter()
        .filter(|(key, _)| key.starts_with(PREFIX))
        .filter_map(|(key, state)| state.completed_seq.map(|seq| (seq, key)))
        .collect();
    live_after.sort_by_key(|(seq, _)| *seq);
    assert_eq!(
        live_after.last(),
        Some(&(retired, in_flight_key.clone())),
        "AC-I2: the new completion sorts after every restored one"
    );
    assert_eq!(
        live_after[..live_after.len() - 1],
        committed_order,
        "AC-I2: the restored relative order must be unchanged by the new completion"
    );

    cleanup(&bundle);
}

/// A notice issued after restore has `NEXT_NOTICE_ID` strictly above
/// `identity_floors.notice_id_floor`, and no new id matches a `notice-<n>` that
/// was handed out before the crash (`orchestrator::notice::next_notice_id` spells it
/// that way).
///
/// The bundle's `notice_id_floor` is read from the live counter at fixture time,
/// so the ids issued before the crash are exactly `notice-1 .. notice-{floor-1}` — the
/// floor claims were already handed out. The test asserts the issued id, the
/// counter after issuing, and the absence of any alias, without depending on how
/// far this binary had already advanced the counter (the mutex guarantees no
/// other test in this file moves it concurrently).
#[test]
fn ac_i3_a_notice_issued_after_restore_never_aliases_a_pre_crash_notice_id() {
    let _registry = registry_lock();
    let bundle = bundle_dir("ac-i3");
    const PREFIX: &str = "coder-ac-i3";

    let counter_before = notice::notice_id_floor();
    let floor = counter_before + 100;

    let key = format!("{PREFIX}-flight");
    let sections = vec![worker(
        &spec(
            &key,
            7_000_001,
            WorkerStatus::InProgress,
            "In Progress",
            None,
        ),
        worker_history(&key),
    )];
    let payload = payload(
        sections.clone(),
        IdentityFloors {
            worker_id_seq_floor: 7_000_100,
            completed_seq_floor: 1,
            notice_id_floor: floor,
        },
    );
    write(&bundle, "session-ac-i3", &payload);

    let (restored, _) = reload(&bundle);
    assert_eq!(
        restored.identity_floors.notice_id_floor, floor,
        "AC-I3: the bundle must carry the notice floor it was committed with"
    );

    let install =
        marmennill::session::install_restored_workers(&restored.workers, &restored.identity_floors);
    assert_eq!(install.installed, 1, "{:?}", install.warnings);
    assert_eq!(
        notice::notice_id_floor(),
        floor,
        "AC-I3/§5.7 step 1: NEXT_NOTICE_ID must be raised to the bundle floor {floor} before \
         anything can issue a notice, it is {}",
        notice::notice_id_floor()
    );

    // Every id the previous process can have used, per the floor it recorded.
    let pre_crash: BTreeSet<String> = (1..floor).map(|n| format!("notice-{n}")).collect();

    let issued = notice::next_notice_id();
    let number: u64 = issued
        .strip_prefix("notice-")
        .unwrap_or_else(|| panic!("AC-I3: notice ids are spelled `notice-<n>`, got {issued}"))
        .parse()
        .unwrap_or_else(|e| panic!("AC-I3: notice id {issued} carries no parsable number: {e}"));

    assert_eq!(
        number, floor,
        "AC-I3: the first notice issued after a restore is the floor itself — every id below it \
         is already spent"
    );
    assert!(
        !pre_crash.contains(&issued),
        "AC-I3: the new notice id {issued} aliases an id the previous process already used \
         (the floor {floor} was not applied)"
    );
    assert!(
        notice::notice_id_floor() > floor,
        "AC-I3: after issuing, NEXT_NOTICE_ID must be strictly above the floor {floor}, it is {}",
        notice::notice_id_floor()
    );
    assert_eq!(
        notice::notice_id_floor(),
        number + 1,
        "AC-I3: issuing {issued} must consume exactly one id"
    );

    let second = notice::next_notice_id();
    assert_ne!(
        second, issued,
        "AC-I3: two notices in one process must never share an id"
    );
    assert!(
        !pre_crash.contains(&second),
        "AC-I3: the second notice id {second} aliases a pre-crash id"
    );
    // The immediate neighbour of the floor is the id a restore-without-floors
    // would have re-used first; naming it keeps the aliasing hazard concrete.
    let neighbour = format!("notice-{}", floor - 1);
    assert!(
        second != neighbour && issued != neighbour,
        "AC-I3: {neighbour} was issued by the previous process and must not be re-issued"
    );

    cleanup(&bundle);
}

/// `Manifest.session_id` is unchanged by a restore, and the next
/// commit writes the same `session_id` (no re-minting).
///
/// Asserted on the whole identity chain, because "the same session" is what a
/// reader believes when the UI prints an id after a resume:
///
/// * the committed manifest and every committed member header carry the id;
/// * `load_bundle` reports the same id in `RestoredSession::session_id` **and**
///   in the verified `Manifest` it hands back;
/// * the full boot pass keeps it (its out-parameter and `BootReport`), i.e. it
///   does not fall into `SessionCommitPump::new`'s `uuid::new_v4()` fresh-session
///   branch;
/// * the next commit through the real pump writes a manifest — and member
///   headers — with that same id.
#[test]
fn ac_i4_the_session_id_survives_the_restore_and_the_next_commit_writes_the_same_id() {
    let _registry = registry_lock();
    let bundle = bundle_dir("ac-i4");
    const PREFIX: &str = "coder-ac-i4";
    const SESSION_ID: &str = "session-t014c-ac-i4-1a2b3c4d";

    let key = format!("{PREFIX}-flight");
    let sections = vec![worker(
        &spec(
            &key,
            8_000_001,
            WorkerStatus::InProgress,
            "In Progress",
            None,
        )
        .with_task("ac-i4"),
        worker_history(&key),
    )];
    let payload = payload(
        sections.clone(),
        IdentityFloors {
            worker_id_seq_floor: 8_000_100,
            completed_seq_floor: 1,
            notice_id_floor: 1,
        },
    );
    write(&bundle, SESSION_ID, &payload);

    // ---- the committed bundle: manifest and every member header -------------
    let on_disk = last_manifest(&bundle).expect("the commit wrote a manifest");
    assert_eq!(on_disk.session_id, SESSION_ID, "AC-I4: the manifest id");
    let mut headers = vec![
        marmennill::session::commit::manifest_path(&bundle),
        marmennill::session::commit::manager_path(&bundle),
        marmennill::session::commit::ui_path(&bundle),
        bundle.join(worker_rel_path(&key)),
    ];
    headers.sort();
    let mut seen = 0usize;
    for path in &headers {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let doc: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{} parses: {e}", path.display()));
        // `manifest.json` has no header; every member file does.
        if let Some(header) = doc.get("header") {
            let id = header
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("{} must carry header.session_id", path.display()));
            assert_eq!(
                id,
                SESSION_ID,
                "AC-I4: the member header at {} must carry the same session id",
                path.display()
            );
            seen += 1;
        }
    }
    assert!(
        seen >= 3,
        "fixture sanity: manager/ui/worker members must all carry a session id header, saw {seen}"
    );

    // ---- the loader keeps it -------------------------------------------------
    let (restored, _) = reload(&bundle);
    assert_eq!(
        restored.session_id, SESSION_ID,
        "AC-I4: load_bundle must report the committed session id, unchanged"
    );
    assert_eq!(
        restored.manifest.session_id, restored.session_id,
        "AC-I4: RestoredSession::session_id must be the verified manifest's id, not a new one"
    );

    // ---- the full boot pass keeps it ----------------------------------------
    let booted = boot_bundle(&bundle);
    assert_eq!(
        booted.session_id, SESSION_ID,
        "AC-I4: boot must hand back the restored id (identity continuity, flow §4.9)"
    );
    assert_eq!(
        booted.report.session_id, SESSION_ID,
        "AC-I4: BootReport must print the restored id, not a freshly minted one"
    );

    // ---- the next commit writes the same id ---------------------------------
    // `SessionCommitPump::new` mints `uuid::new_v4()` only for an EMPTY id: the
    // restored id must come back out of it untouched.
    let cfg = boot_config();
    let mut pump =
        marmennill::ui::session::SessionCommitPump::new(&booted.plan, &cfg, &booted.session_id);
    assert_eq!(
        pump.session_id(),
        SESSION_ID,
        "AC-I4: the pump must not re-mint a session id for a resumed session"
    );
    let renderer = BootRenderer::default();
    let decision = pump.request(
        marmennill::ui::session::CommitTrigger::CleanTail,
        &booted.engine,
        &renderer,
    );
    let marmennill::ui::session::CommitDecision::Written { members } = decision else {
        panic!("AC-I4: the next commit must be written, the pump decided {decision:?}");
    };
    assert!(members > 0, "AC-I4: the next commit wrote no member");

    let next = last_manifest(&bundle).expect("the pump wrote a manifest");
    assert_eq!(
        next.session_id, SESSION_ID,
        "AC-I4: the next commit writes the same session_id (no re-minting)"
    );
    let (again, _) = reload(&bundle);
    assert_eq!(
        again.session_id, SESSION_ID,
        "AC-I4: the bundle written after the restore still carries the original session id"
    );
    assert_eq!(
        again.manifest.session_id, SESSION_ID,
        "AC-I4: the re-committed manifest must not have re-minted the id"
    );
    assert!(
        committed_keys(&again.workers)
            .iter()
            .any(|k| k.starts_with(PREFIX)),
        "AC-I4: the re-committed bundle must still be the same session's worker set"
    );

    cleanup(&bundle);
}
