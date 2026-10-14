//! t-014e — the three acceptance families of `docs/design_session_restore_data.md`
//! that decide whether an **aborted subagent** comes back honest:
//!
//! * **§9.7 AC-S1 … AC-S4 — the subagent panel** (AC-S1 the restored vector
//!   equals the committed one in length and order with every listed field equal
//!   and `logs` compared element-by-element; AC-S2 `worked_duration_secs` is the
//!   stored number and is **never** re-derived from the two re-anchored clocks
//!   (D5 T3); AC-S3 the two clocks re-anchor inside the documented error term
//!   (§5.6 T2) while their `wall` stamps stay byte-exact; AC-S4 an
//!   `is_active: true` panel keeps its bytes **and** is reported as a lost
//!   `JoinHandle` named by the panel name — §6.3, §10.3);
//! * **§9.8 AC-T1 … AC-T2 — the time base** (AC-T1 every restored
//!   `elapsed_secs` is measured against the very anchor `Manifest.anchor_wall`
//!   names, and the ordering of `saved_elapsed_secs` between members is the
//!   committed ordering (§5.6 T4); AC-T2 a NaN / infinite / negative monotonic
//!   offset is a refusal, never a silent default to "now");
//! * **§9.9 AC-A1 … AC-A3 — abort mid-subagent** (AC-A1 one live worker + one
//!   active panel + composer draft + a non-zero `chat_scroll`, aborted, committed
//!   with `ending: UserAbort`, coming back as it was; AC-A2 the notices are
//!   present and specific — `CancellationToken`, `JoinHandle`, `DropGuard` per
//!   in-flight worker plus the queued `PendingNotice` — with `Restored` as the
//!   outcome and the worker keys as subjects; AC-A3 the restored cancellation
//!   token provably is **not** the old one, the worker is reported as
//!   non-cancellable until re-armed, and nothing in the restore path re-launches
//!   it).
//!
//! ## How each authority is observed
//!
//! * the **committed bytes** are the authority: `CapturePayload` →
//!   `session::commit::write_bundle` → `session::restore::load_bundle`, plus the
//!   raw member JSON read back from disk (`ui.json`, `workers/<key>.json`,
//!   `manifest.json`) so a claim about a byte is checked against the byte;
//! * the **live panes** are read through a real `TuiRenderer` wrapped in
//!   [`PaneWitness`], which snapshots the pane model *inside* `restore_panes`
//!   (the post-assign, pre-draw instant of I-U2) and records the exact
//!   `Vec<SubagentDetail>` that `ui::session::boot` handed to `set_subagents`
//!   (`src/ui/session.rs:1235-1244`) — that is the only public mirror of the
//!   crate's private pane fields;
//! * the **live worker** is read through the same registry production uses
//!   (`orchestrator::workers::worker_state_snapshot`, `session::registry_get`,
//!   `orchestrator::notice::worker_inbox_len`).
//!
//! ## Honest limits, stated instead of hidden
//!
//! * **AC-S4 (notice half)** has **two** production emitters of
//!   `NonRestorableKind::JoinHandle` — `src/session/worker_journal.rs:844-851`
//!   (kind line 845), whose subject is a **worker key**, and
//!   `src/ui/session.rs:1256-1274` (kind line 1263), which emits one notice per
//!   restored `SubagentPanel` with `is_active: true` whose subject is the **panel
//!   name**. The byte half is pinned in AC-S1 (`is_active: true` survives, never
//!   flipped); the notice half keeps the criterion's wording and runs in this
//!   gate — the `#[ignore]` it carried while awaiting its first execution was
//!   dropped by t-035 after it passed under `--ignored`.
//! * **AC-A2 (capture half)**: the capture side does sample the live inbox —
//!   `session::worker_journal::project_worker_section` folds in
//!   `pending_notice_notices` (call at `src/session/worker_journal.rs:624`, fn
//!   995-1008), which reads `WORKER_INBOXES` through
//!   `orchestrator::notice::peek_worker_inbox` (`src/orchestrator/notice.rs:285-292`)
//!   rather than through `worker_inbox_len`. AC-A2 still supplies the queued count
//!   through the channel boot reads (`section.non_restorable`,
//!   `src/ui/session.rs:1157-1181`); the separate capture probe now runs in this
//!   gate too (t-035 dropped its `#[ignore]` once it had executed green).
//! * **AC-T2 (non-finite values)**: `serde_json` does **not** refuse NaN/infinity
//!   — `serialize_f64` writes them as `null` — so the commit seam refuses them
//!   itself: `check_time_points` (`src/session/commit.rs:641`, called at
//!   `src/session/commit.rs:723` inside `assemble_members`) runs *before* C1
//!   serializes anything, so a non-finite offset never becomes a bundle byte and
//!   there is no manifest to load. That keeps the load-side sentence at
//!   `src/session/restore.rs:1101` unreachable through the commit seam; AC-T2
//!   pins the seam (no bundle, no manifest, `Err`) and pins the load-side refusal
//!   for the value that *does* reach it (negative, which §7.1 C1–C4 deliberately
//!   leaves to the loader rather than repairing it on the way out).
//! * **AC-A1 (live trigger half)**: the in-loop wiring "operator aborted ⇒ the
//!   commit is stamped `userAbort`" is a session-loop concern and is pinned by
//!   `tests/test_session_abort_flush.rs`; this file pins the data half (the
//!   committed `userAbort` byte, and everything that comes back with it).
//!
//! Two rules this file never breaks:
//!
//! * **one hasher** — every "the bytes did not change" witness re-uses the
//!   crate's own `session::commit::fingerprint` (§7.2), never a second one;
//! * **no weakened assertion** — where a criterion and the implementation
//!   disagree, the test keeps the criterion's wording and carries `#[ignore]`.
//!
//! Hermetic: one unique temp directory per test
//! (`<tmp>/marmel-t014e-<tag>-<pid>-<nanos>/.marmel/session`), removed at the
//! end; no network, no PTY, no terminal, no sleeps. The process-global
//! `WORKERS` map, journal `REGISTRY`, notice store and identity counters are
//! serialized by one local mutex, every key is private to this file
//! (`t014e…`), and expectations are stated relative to the live counters read at
//! runtime — never absolutely. Fixture ids are `t014e-`.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use marmennill::agents::Agent;
use marmennill::config::Config;
use marmennill::manager::phase::{MARMEL_DIR, Plan};
use marmennill::orchestrator::{notice, workers};
use marmennill::session::commit::{
    CapturePayload, CommitError, CommitLock, CommitPolicy, CommitReport, fingerprint,
    worker_rel_path, write_bundle,
};
use marmennill::session::format::{MANIFEST_FILE, TimePoint, UI_FILE};
use marmennill::session::restore::load_bundle;
use marmennill::session::time::anchor_wall;
use marmennill::session::worker_journal::{project_worker_section, queued_notice_notices};
use marmennill::session::{
    ComposerState, DanglingToolCallState, HistorySource, INTERRUPTED_STATUS_LABEL, IdentityFloors,
    ManagerSection, MessageRecord, NonRestorableKind, NonRestorableNotice, PinWitness,
    ProcessEnding, RefusalCode, RestoreOutcome, RestoredSession, SubagentPanel, ToolCallRecord,
    ToolFunctionRecord, UiSection, ViewState, WorkerCounters, WorkerSection, WorkerStatus,
};
use marmennill::ui::session::{BootReport, BootSink, boot, build_restored_context};
use marmennill::ui::tui::TuiRenderer;
use marmennill::ui::{Event, InputState, Renderer, SubagentDetail, UiTranscript};
use tokio_util::sync::CancellationToken;

// ----------------------------------------------------------------- constants

/// The budget every fixture commits with.
const COMMITTED_BUDGET: usize = 1234;

/// Budget this process boots with — deliberately different, so a re-composition
/// could never be mistaken for a restoration.
const LIVE_BUDGET: usize = 4321;

/// §5.6 T2's documented error term for a re-anchored `Instant`: the pair
/// survives suspend/hibernate and wall-clock steps, and the residual drift
/// between the monotonic and the wall clock is what a restored `Instant` may be
/// off by. Two seconds is that term; every re-anchor witness is stated against
/// it, and a *stored* value is compared bit-for-bit instead.
const RE_ANCHOR_TOLERANCE_SECS: f64 = 2.0;

// ------------------------------------------------------------------- gating

/// Serializes every test in this file that reads or mutates the process-global
/// `WORKERS` map, the journal `REGISTRY`, the notice store or the identity
/// counters, so no two of them interleave and no expectation can be polluted by
/// another test's live entry.
static REGISTRY: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Poison-tolerant, like every other guard on the session path.
fn registry_lock() -> MutexGuard<'static, ()> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------- temp bundle

/// A fresh temp dir whose `.marmel/session` layout mirrors a real plan. The tag
/// is unique per test, so cases never share a directory.
fn bundle_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "marmel-t014e-{tag}-{}-{}",
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

fn cleanup(bundle: &Path) {
    std::fs::remove_dir_all(root_of(bundle)).ok();
}

// ---------------------------------------------------------------- time helpers

/// A synthetic `TimePoint` that is **exactly** consistent with this process's
/// anchor: `wall == anchor_wall + elapsed_secs`. Used where the criterion is
/// about the anchor relation itself (AC-T1), because any drift would then be a
/// finding instead of an artifact of the fixture.
fn point(elapsed_secs: f64) -> TimePoint {
    TimePoint {
        elapsed_secs,
        wall: anchor_wall()
            .checked_add_signed(chrono::Duration::nanoseconds(
                (elapsed_secs * 1e9).round() as i64
            ))
            .expect("the synthetic clock point stays inside chrono's range"),
    }
}

/// A realistic pair of clocks sampled **by the old process**: `elapsed_secs` is
/// this process's anchor age plus how long ago the event happened, `wall` is
/// `now - that much`. `started_back` / `last_back` are the ages in seconds, so
/// the pair's own delta is `started_back - last_back` and can be set to anything
/// independent of `worked_duration_secs` (AC-S2 needs exactly that).
fn clock_pair(started_back: f64, last_back: f64) -> (TimePoint, TimePoint) {
    let age = TimePoint::now().elapsed_secs;
    let base = Local::now();
    let at = |back: f64| TimePoint {
        elapsed_secs: age + back,
        wall: base
            .checked_sub_signed(chrono::Duration::nanoseconds((back * 1e9).round() as i64))
            .expect("the sampled clock stays inside chrono's range"),
    };
    (at(started_back), at(last_back))
}

/// How far one restored offset sits from "what `wall` says, measured from the
/// anchor `manifest.anchor_wall` names". Zero means the offset really is measured
/// against that anchor (AC-T1).
fn anchor_consistency(value: &TimePoint, anchor: &DateTime<Local>) -> f64 {
    let wall_delta = value
        .wall
        .signed_duration_since(*anchor)
        .to_std()
        .map(|d| d.as_secs_f64())
        .unwrap_or(f64::NAN);
    (wall_delta - value.elapsed_secs).abs()
}

/// The re-anchoring error of one durable clock point read in *this* process:
/// `to_instant()` (monotonic side) versus the `wall` stamp (calendar side), both
/// expressed as "how long ago". A restored `Instant` may be off by suspend /
/// wall-step drift, never by the value itself (AC-S3, §5.6 T2).
fn re_anchor_error(value: &TimePoint) -> f64 {
    let from_instant = Instant::now()
        .saturating_duration_since(value.to_instant())
        .as_secs_f64();
    let from_wall = Local::now()
        .signed_duration_since(value.wall)
        .to_std()
        .map(|d| d.as_secs_f64())
        .unwrap_or(f64::NAN);
    (from_instant - from_wall).abs()
}

/// `duration_from_secs` (`src/ui/tui/mod.rs:1510`): the projection that turns a
/// stored `worked_duration_secs` back into the `Duration` the pane shows.
fn expected_detail_duration(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
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

/// The pinned pair plus one settled turn: `[0]`/`[1]` are the two pins the loader
/// insists on (§4). This file is about panels, clocks and abort notices.
fn manager_transcript() -> Vec<MessageRecord> {
    vec![
        MessageRecord::System {
            content: "t014e pinned system prompt: never re-composed on this path".to_string(),
        },
        MessageRecord::User {
            content: "t014e pinned goal: the aborted subagent comes back honest".to_string(),
        },
        MessageRecord::Assistant {
            content: Some("working".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ]
}

fn manager_section() -> ManagerSection {
    let messages = manager_transcript();
    ManagerSection {
        pinned_prefix_end: 2,
        pin_witness: PinWitness {
            system_prompt_len: messages[0].content().map_or(0, str::len) as u32,
            goal_len: messages[1].content().map_or(0, str::len) as u32,
        },
        max_context_tokens: COMMITTED_BUDGET,
        rebirth_advisory_emitted: false,
        consecutive_rebirths: 0,
        dangling: DanglingToolCallState::observe(&messages),
        messages,
    }
}

/// A worker transcript with at least one assistant turn whose `tool_calls` are
/// each answered by a paired `Tool` result — the shape AC-A1 names for a worker
/// that was mid-round when the operator aborted.
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
        tool(
            &format!("{key}-c1"),
            "pub fn parse(input: &str) -> Out { … }",
        ),
        assistant(
            Some(""),
            None,
            vec![
                call(
                    &format!("{key}-c2"),
                    "replace",
                    "{\"path\":\"src/lib.rs\" not valid json",
                ),
                call(
                    &format!("{key}-c3"),
                    "grep_search",
                    "{\"pattern\":\"ünïcode ✓\"}",
                ),
            ],
        ),
        tool(&format!("{key}-c2"), "aborted by user"),
        tool(&format!("{key}-c3"), "1 match in src/lib.rs"),
    ]
}

// ----------------------------------------------------------- worker fixtures

/// Everything a fixture worker states that these criteria read. Every other
/// `WorkerSection` field is filled with a distinctive, non-default value inside
/// [`WorkerFixture::build`], so an accidentally dropped field is caught instead
/// of coincidentally matched.
struct WorkerFixture {
    key: String,
    role: Agent,
    task_id: Option<String>,
    worker_id_seq_value: u64,
    status: WorkerStatus,
    status_label: String,
    completed_seq: Option<u64>,
    history_source: HistorySource,
    worked_secs: f64,
    started: Option<TimePoint>,
    completed: Option<TimePoint>,
    last_activity: Option<TimePoint>,
    non_restorable: Vec<NonRestorableNotice>,
    history: Vec<MessageRecord>,
}

/// `Coder`, `LiveMirror` history with paired tool calls, `InProgress` /
/// `"In Progress"` / `completed_seq: None` — the in-flight shape §9.9 describes.
fn worker_fixture(key: &str) -> WorkerFixture {
    WorkerFixture {
        key: key.to_string(),
        role: Agent::Coder,
        task_id: None,
        // Above every id this process has issued, below the floor this file
        // commits with (see [`floors`]).
        worker_id_seq_value: workers::worker_id_seq_floor() + 7,
        status: WorkerStatus::InProgress,
        status_label: "In Progress".to_string(),
        completed_seq: None,
        history_source: HistorySource::LiveMirror,
        worked_secs: 42.5,
        started: None,
        completed: None,
        last_activity: None,
        non_restorable: Vec::new(),
        history: worker_history(key),
    }
}

impl WorkerFixture {
    fn build(&self) -> WorkerSection {
        WorkerSection {
            worker_key: self.key.clone(),
            worker_id_seq_value: self.worker_id_seq_value,
            role: self.role,
            task_id: self.task_id.clone(),
            status: self.status.clone(),
            status_label: self.status_label.clone(),
            started: self.started.clone().unwrap_or_else(TimePoint::now),
            completed: self
                .completed
                .clone()
                .or_else(|| self.completed_seq.map(|_| TimePoint::now())),
            last_activity: self
                .last_activity
                .clone()
                .or_else(|| self.completed_seq.map(|_| TimePoint::now())),
            worked_secs: self.worked_secs,
            completed_seq: self.completed_seq,
            context_tokens: 1234,
            last_tokens: 567,
            counters: WorkerCounters {
                implementation_turns: 3,
                validation_rounds: 2,
                latest_validator_feedback: Some(
                    "validator: the error path has no assertion".to_string(),
                ),
            },
            brief: format!("brief for {}", self.key),
            snippets: vec!["snippet-a".to_string(), "snippet-b".to_string()],
            image_urls: vec!["marmel://image/shot-1.png".to_string()],
            audio_urls: vec!["marmel://audio/note-1.wav".to_string()],
            recursion_granted: true,
            history: self.history.clone(),
            history_source: self.history_source.clone(),
            dangling: DanglingToolCallState::observe(&self.history),
            unrecoverable_reason: None,
            non_restorable: self.non_restorable.clone(),
        }
    }
}

// ------------------------------------------------------------ panel fixtures

/// The `SubagentPanel` fields these criteria read. `SubagentPanel` itself has no
/// `Default` (`src/session/format/ui.rs:168`), so the fixture is the shorthand
/// and every test varies it with struct-update syntax.
struct PanelFixture {
    name: String,
    task_id: Option<String>,
    prompt: String,
    logs: Vec<String>,
    thinking: String,
    content: String,
    is_active: bool,
    context_tokens: usize,
    worked_secs: f64,
    started_at: Option<TimePoint>,
    last_activity_at: Option<TimePoint>,
    turn_thinking: Option<String>,
    is_thinking: Option<bool>,
}

/// A non-trivial active panel: every text field distinct and non-empty.
fn panel_fixture(name: &str) -> PanelFixture {
    PanelFixture {
        name: name.to_string(),
        task_id: Some(format!("t-014e-{name}")),
        prompt: format!("t014e brief for {name}"),
        logs: vec![
            format!("tool read_file started for {name}"),
            format!("replace 1 occurrence for {name}"),
        ],
        thinking: format!("thinking of {name}"),
        content: format!("content of {name}"),
        is_active: true,
        context_tokens: 987_654,
        worked_secs: 42.5,
        started_at: None,
        last_activity_at: None,
        turn_thinking: None,
        is_thinking: None,
    }
}

impl PanelFixture {
    fn build(&self) -> SubagentPanel {
        SubagentPanel {
            name: self.name.clone(),
            task_id: self.task_id.clone(),
            prompt: self.prompt.clone(),
            logs: self.logs.clone(),
            thinking: self.thinking.clone(),
            content: self.content.clone(),
            is_active: self.is_active,
            context_tokens: self.context_tokens,
            worked_duration_secs: self.worked_secs,
            started_at: self.started_at.clone(),
            last_activity_at: self.last_activity_at.clone(),
            turn_thinking: self.turn_thinking.clone(),
            is_thinking: self.is_thinking,
        }
    }
}

// ----------------------------------------------------------------- ui fixture

/// `count` single-line pane entries. AC-A1 needs a tall pane so the restored
/// `chat_scroll` sits far inside what the live clamp
/// (`src/ui/tui/events.rs:465-481`) allows instead of being pulled back by it.
fn pane_lines(count: usize) -> Vec<String> {
    (0..count)
        .map(|i| format!("[Tool Result] t014e pane line {i:03}"))
        .collect()
}

/// The screen the aborted session was showing: 200 pane lines and a status bar
/// that is not the default `"Ready"`.
fn base_ui() -> UiSection {
    UiSection {
        chat_messages: pane_lines(200),
        status_line: "Running (t014e-model) — 1 task(s) in flight".to_string(),
        ..UiSection::default()
    }
}

/// Identity floors strictly above every value this file's fixtures carry, read
/// from the live counters so no expectation depends on another test's leftovers.
fn floors() -> IdentityFloors {
    IdentityFloors {
        worker_id_seq_floor: workers::worker_id_seq_floor() + 5_000,
        completed_seq_floor: workers::completed_seq_floor() + 5_000,
        notice_id_floor: notice::notice_id_floor() + 5_000,
    }
}

fn payload(
    ui: UiSection,
    workers: Vec<WorkerSection>,
    identity_floors: IdentityFloors,
) -> CapturePayload {
    CapturePayload {
        manager: manager_section(),
        ui: Some(ui),
        workers,
        identity_floors,
    }
}

/// The public commit seam, unthrottled. The committer does NOT validate the UI
/// section (§7 C1–C4) — which is what lets AC-T2 commit a *negative* clock the
/// loader must then refuse — while `assemble_members` does refuse a **non-finite**
/// offset up front (`check_time_points`, `src/session/commit.rs:641`), which is
/// what AC-T2's NaN/infinity branch pins.
fn commit(
    bundle: &Path,
    session_id: &str,
    capture: &CapturePayload,
    ending: ProcessEnding,
) -> Result<CommitReport, CommitError> {
    let policy = CommitPolicy {
        min_interval: std::time::Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    write_bundle(
        bundle,
        session_id,
        capture,
        ending,
        &policy,
        &CommitLock::new(),
    )
}

/// Commit and insist it is a real commit point.
fn write(
    bundle: &Path,
    session_id: &str,
    capture: &CapturePayload,
    ending: ProcessEnding,
) -> CommitReport {
    let report = commit(bundle, session_id, capture, ending)
        .unwrap_or_else(|error| panic!("the fixture bundle must commit: {error}"));
    assert!(
        !report.members_written.is_empty(),
        "the fixture bundle must be a real commit point"
    );
    report
}

/// One human-readable sentence for an unexpected outcome.
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

/// A full restore, or a panic that names what the loader actually said.
fn restored_session(bundle: &Path) -> (RestoredSession, ProcessEnding) {
    let outcome = load_bundle(bundle);
    match outcome {
        RestoreOutcome::Restored {
            session, ending, ..
        } => (session, ending),
        other => panic!(
            "expected a full restore of {}, got {}",
            bundle.display(),
            explain(&other)
        ),
    }
}

/// A refusal, or a panic that names what the loader actually said.
fn refused(bundle: &Path) -> (RefusalCode, Option<String>, String) {
    let outcome = load_bundle(bundle);
    match outcome {
        RestoreOutcome::Refused { refusal, .. } => (refusal.code, refusal.member, refusal.message),
        other => panic!(
            "expected a refusal at {}, got {}",
            bundle.display(),
            explain(&other)
        ),
    }
}

/// The committed `ui.json` read back through the loader: the authority every
/// AC-S* comparison is made against, so no test compares the boot pass with a
/// fixture it merely built in memory.
fn committed_ui(bundle: &Path) -> UiSection {
    restored_session(bundle).0.ui
}

/// The committed worker sections read back through the loader, keyed for lookup.
fn committed_workers(bundle: &Path) -> Vec<WorkerSection> {
    restored_session(bundle).0.workers
}

// ------------------------------------------------------ byte-level witnesses

fn member_bytes(bundle: &Path, rel: &str) -> Vec<u8> {
    let path = bundle.join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn member_paths(bundle: &Path) -> Vec<String> {
    let mut paths: Vec<String> = manifest_json(bundle)
        .get("members")
        .and_then(|m| m.as_array())
        .expect("members is an array")
        .iter()
        .filter_map(|m| m.get("rel_path").and_then(|p| p.as_str()))
        .map(str::to_string)
        .collect();
    paths.push(MANIFEST_FILE.to_string());
    paths.sort();
    paths
}

/// `(rel_path, fingerprint)` for every member plus `manifest.json`, hashed with
/// the crate's own §7.2 hasher — never a second one.
fn fingerprints(bundle: &Path) -> Vec<(String, u64)> {
    let mut table: Vec<(String, u64)> = member_paths(bundle)
        .into_iter()
        .map(|rel| (rel.clone(), fingerprint(&member_bytes(bundle, &rel))))
        .collect();
    table.sort();
    table
}

fn assert_bundle_bytes_unchanged(bundle: &Path, before: &[(String, u64)]) {
    let after = fingerprints(bundle);
    assert_eq!(
        after,
        before,
        "the refused restore rewrote bundle bytes ({} member(s) hashed before and after)",
        before.len()
    );
}

fn manifest_json(bundle: &Path) -> serde_json::Value {
    let text = String::from_utf8(member_bytes(bundle, MANIFEST_FILE)).expect("manifest is UTF-8");
    serde_json::from_str(&text).expect("manifest parses")
}

fn section_json(bundle: &Path, rel: &str) -> serde_json::Value {
    let text = String::from_utf8(member_bytes(bundle, rel)).expect("every member file is UTF-8");
    serde_json::from_str(&text).expect("the member file parses")
}

/// The notice for one `(kind, subject)` pair, if any.
fn notice_for<'a>(
    notices: &'a [NonRestorableNotice],
    kind: &NonRestorableKind,
    subject: &str,
) -> Option<&'a NonRestorableNotice> {
    notices
        .iter()
        .find(|notice| &notice.kind == kind && notice.subject == subject)
}

/// AC-S1's field list, one assertion per field, so a failure names the field.
/// `logs` is compared element-by-element, `worked_duration_secs` bit-for-bit
/// (never with a float tolerance).
fn assert_panel_fields_eq(index: usize, what: &str, got: &SubagentPanel, want: &SubagentPanel) {
    assert_eq!(got.name, want.name, "{what}: panel {index} `name` differs");
    assert_eq!(
        got.task_id, want.task_id,
        "{what}: panel {index} `task_id` differs"
    );
    assert_eq!(
        got.prompt, want.prompt,
        "{what}: panel {index} `prompt` differs"
    );
    assert_eq!(
        got.thinking, want.thinking,
        "{what}: panel {index} `thinking` differs"
    );
    assert_eq!(
        got.content, want.content,
        "{what}: panel {index} `content` differs"
    );
    assert_eq!(
        got.is_active, want.is_active,
        "{what}: panel {index} `is_active` differs (an active panel must never be \
         flipped to inactive: N-R1)"
    );
    assert_eq!(
        got.context_tokens, want.context_tokens,
        "{what}: panel {index} `context_tokens` differs"
    );
    assert_eq!(
        got.worked_duration_secs.to_bits(),
        want.worked_duration_secs.to_bits(),
        "{what}: panel {index} `worked_duration_secs` is not the stored number"
    );
    assert_eq!(
        got.logs.len(),
        want.logs.len(),
        "{what}: panel {index} `logs` has a different number of entries"
    );
    for (entry, (got_log, want_log)) in got.logs.iter().zip(want.logs.iter()).enumerate() {
        assert_eq!(
            got_log.as_bytes(),
            want_log.as_bytes(),
            "{what}: panel {index} log entry {entry} is not the committed bytes"
        );
    }
}

// ------------------------------------------------------------ the pane witness

/// A **real** `TuiRenderer` behind a recording `Renderer`, so the boot pass runs
/// its own hook order and the test can read (a) the pane model at the exact
/// instant I-U2 means and (b) the exact `SubagentDetail` vector the boot pass
/// handed to `set_subagents`.
struct PaneWitness {
    tui: TuiRenderer,
    /// Captured inside `restore_panes`, immediately after delegating the assign.
    after_restore: Option<UiSection>,
    /// The `flush` count at the moment of that snapshot — "before the first draw".
    draws_at_snapshot: usize,
    flushes: usize,
    events: Vec<Event>,
    /// Every `SubagentDetail` the boot pass advertised, in call order.
    captured_subagents: Vec<SubagentDetail>,
}

impl Default for PaneWitness {
    fn default() -> Self {
        PaneWitness {
            tui: TuiRenderer::new(),
            after_restore: None,
            draws_at_snapshot: 0,
            flushes: 0,
            events: Vec::new(),
            captured_subagents: Vec::new(),
        }
    }
}

impl PaneWitness {
    /// The pane model at the post-assign, pre-draw instant.
    fn snapshot(&self) -> &UiSection {
        self.after_restore
            .as_ref()
            .expect("`restore_panes` must have been called by the boot pass")
    }

    /// The status sentences the pass sent to the renderer (D6: loud, never
    /// silent).
    fn status_events(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Status(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Renderer for PaneWitness {
    fn init(&mut self) -> anyhow::Result<()> {
        self.tui.init()
    }
    fn on_event(&mut self, event: &Event) {
        self.events.push(event.clone());
        self.tui.on_event(event);
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        self.flushes += 1;
        self.tui.flush()
    }
    fn poll_input(&mut self) -> Option<String> {
        self.tui.poll_input()
    }
    fn read_input(&mut self) -> Option<String> {
        self.tui.read_input()
    }
    fn input_state(&mut self) -> &mut InputState {
        self.tui.input_state()
    }
    fn input_state_shared(&self) -> &InputState {
        self.tui.input_state_shared()
    }
    fn shutdown(&mut self) {
        self.tui.shutdown();
    }
    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        self.captured_subagents.extend(subagents.clone());
        self.tui.set_subagents(subagents);
    }
    fn set_thinking_budgets(&mut self, cfg: &Config) {
        self.tui.set_thinking_budgets(cfg);
    }
    fn reset_active_agent(&mut self) {
        self.tui.reset_active_agent();
    }
    fn capture_ui(&self) -> UiSection {
        self.tui.capture_ui()
    }
    fn restore_panes(&mut self, section: &UiSection) {
        self.tui.restore_panes(section);
        self.draws_at_snapshot = self.flushes;
        self.after_restore = Some(self.tui.capture_ui());
    }
    fn announce_restore(&mut self, report: &marmennill::session::RepairReport) {
        self.tui.announce_restore(report);
    }
}

/// Everything one full boot pass left behind.
struct Booted {
    report: BootReport,
    renderer: PaneWitness,
    session_id: String,
}

/// THE boot path (`ui::session::boot`, flow §4.9) driven end to end with a real
/// TUI renderer: `load_bundle` → floors + `WORKERS` → the engine → the panes →
/// the loud report → `N-MGR-1`.
fn boot_with_panes(bundle: &Path) -> Booted {
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
    let mut engine = build_restored_context(&cfg);
    let mut renderer = PaneWitness::default();
    let mut transcript = UiTranscript::new();
    let mut session_id = String::new();
    let mut sink = BootSink {
        ui_transcript: &mut transcript,
    };
    let report = boot(
        &plan,
        &cfg,
        &mut engine,
        &mut renderer,
        &mut session_id,
        &mut sink,
    )
    .unwrap_or_else(|error| {
        panic!(
            "a sound bundle must boot, not refuse: {} [{}] {}",
            error.refusal.code.label(),
            error.refusal.member.as_deref().unwrap_or("-"),
            error.refusal.message
        )
    });
    assert_eq!(
        renderer.flushes, 0,
        "the boot pass must install the panes without drawing"
    );
    assert!(
        !renderer.status_events().is_empty(),
        "the restore must be announced to the renderer, not silent (D6)"
    );
    Booted {
        report,
        renderer,
        session_id,
    }
}

// ===========================================================================
// §9.7 — the subagent panel
// ===========================================================================

/// **AC-S1** — the restored panel vector equals the committed one in length and
/// order, and per entry `name`, `task_id`, `prompt`, `logs` (element-by-element),
/// `thinking`, `content`, `is_active` and `context_tokens` are equal.
///
/// Asserted at three depths: `load_bundle` (the bytes), `BootReport::subagent_panels`
/// (what the caller sees), and the live pane model at the pre-draw instant plus
/// the exact `SubagentDetail` vector handed to `set_subagents`. The fixture
/// carries the awkward shapes that make this a copy test and not a tautology: an
/// empty panel, a duplicate `name`, a multi-line prompt, a repeated and an empty
/// log entry, multi-byte text, and one panel with **no** clocks at all.
#[test]
fn ac_s1_restored_panel_vector_equals_the_committed_one_in_length_and_order() {
    let _guard = registry_lock();
    let bundle = bundle_dir("ac-s1");
    let panels: Vec<SubagentPanel> = vec![
        // (0) the empty-but-real panel: every text field empty, context_tokens 0.
        PanelFixture {
            name: "coder".to_string(),
            task_id: None,
            prompt: String::new(),
            logs: Vec::new(),
            thinking: String::new(),
            content: String::new(),
            is_active: false,
            context_tokens: 0,
            worked_secs: 0.0,
            started_at: Some(point(0.0)),
            last_activity_at: Some(point(0.5)),
            turn_thinking: None,
            is_thinking: None,
        }
        .build(),
        // (1) the awkward active panel.
        PanelFixture {
            name: "researcher-ünïcode ✓".to_string(),
            task_id: Some("t-014e-1".to_string()),
            prompt: "multi\nline prompt\nwith a trailing blank\n".to_string(),
            logs: vec![
                "line one".to_string(),
                "line one".to_string(),
                String::new(),
                "   ".to_string(),
            ],
            thinking: "thinking ✓".to_string(),
            content: "content body ✓".to_string(),
            is_active: true,
            context_tokens: 987_654,
            worked_secs: 12.5,
            started_at: Some(point(3.0)),
            last_activity_at: Some(point(9.0)),
            turn_thinking: Some(String::new()),
            is_thinking: Some(true),
        }
        .build(),
        // (2) a duplicate `name` with a different task, and NO clocks.
        PanelFixture {
            name: "coder".to_string(),
            task_id: Some("t-014e-2".to_string()),
            logs: vec!["l1".to_string()],
            thinking: "t".to_string(),
            content: "c".to_string(),
            is_active: false,
            context_tokens: 7,
            worked_secs: 3.75,
            started_at: None,
            last_activity_at: None,
            turn_thinking: Some("turn thought".to_string()),
            is_thinking: Some(false),
            ..panel_fixture("coder")
        }
        .build(),
    ];
    let ui = UiSection {
        subagent_panels: panels.clone(),
        view: ViewState {
            selected_subagent_idx: 1,
            ..ViewState::default()
        },
        ..base_ui()
    };
    assert_eq!(
        ui.structural_violations(),
        Vec::<String>::new(),
        "the AC-S1 fixture must be a sound section, or a refusal would mask the \
         panel comparison"
    );
    write(
        &bundle,
        "session-t014e-ac-s1",
        &payload(ui, Vec::new(), floors()),
        ProcessEnding::InProgress,
    );

    // (a) the bytes: the whole struct, clocks included, is the authority.
    let committed = committed_ui(&bundle);
    assert_eq!(
        committed.subagent_panels, panels,
        "AC-S1: the committed `ui.json` does not hold the panel vector that was \
         captured (clocks are part of the struct)"
    );
    assert_eq!(
        committed.subagent_panels.len(),
        panels.len(),
        "AC-S1: the restored vector has a different length"
    );
    let committed_names: Vec<&str> = committed
        .subagent_panels
        .iter()
        .map(|panel| panel.name.as_str())
        .collect();
    let wanted_names: Vec<&str> = panels.iter().map(|panel| panel.name.as_str()).collect();
    assert_eq!(
        committed_names, wanted_names,
        "AC-S1: the restored order is not the committed order"
    );
    for (index, (got, want)) in committed
        .subagent_panels
        .iter()
        .zip(panels.iter())
        .enumerate()
    {
        assert_panel_fields_eq(index, "load_bundle", got, want);
    }
    // The AC-S4 **byte** half is pinned here, where it is observable: the active
    // panel's byte comes back active, never flipped to `false`.
    assert!(
        committed.subagent_panels[1].is_active,
        "AC-S4 byte half / N-R1: an active panel's byte came back inactive"
    );

    // (b) the full boot pass.
    let booted = boot_with_panes(&bundle);
    assert_eq!(
        booted.renderer.draws_at_snapshot, 0,
        "the pane witnesses below are taken before the first draw"
    );
    assert_eq!(
        booted.report.subagent_panels, committed.subagent_panels,
        "AC-S1: BootReport::subagent_panels is not the committed vector"
    );
    let snapshot = booted.renderer.snapshot();
    assert_eq!(
        snapshot.subagent_panels.len(),
        committed.subagent_panels.len(),
        "AC-S1: the live pane model holds a different number of subagent panels"
    );
    for (index, want) in committed.subagent_panels.iter().enumerate() {
        let got = &snapshot.subagent_panels[index];
        assert_panel_fields_eq(index, "live pane snapshot", got, want);
        // The clocks survive as clocks. Their *Instant* side is re-anchored by
        // design (`TimePoint::to_instant()` rebuilds it from the `wall` delta,
        // §5.6 T2), so the live pane's `elapsed_secs` is this process's offset,
        // not the stored one — AC-S3 owns that invariant and asserts it against
        // the durable pair. What is pinned here is that no clock was dropped or
        // invented, and that the re-anchored pair is still a sane clock.
        assert_eq!(
            got.started_at.is_some(),
            want.started_at.is_some(),
            "AC-S1: panel {index} `started_at` presence changed"
        );
        assert_eq!(
            got.last_activity_at.is_some(),
            want.last_activity_at.is_some(),
            "AC-S1: panel {index} `last_activity_at` presence changed"
        );
        for (field, value) in [
            ("started_at", got.started_at.as_ref()),
            ("last_activity_at", got.last_activity_at.as_ref()),
        ] {
            if let Some(value) = value {
                assert!(
                    value.is_sane(),
                    "AC-S1: the live pane's re-anchored `{field}` for panel {index} is \
                     not a sane clock point: {value:?}"
                );
            }
        }
    }

    // (c) the subagent pane model the boot pass advertised (flow §3.4's channel).
    assert_eq!(
        booted.renderer.captured_subagents.len(),
        panels.len(),
        "AC-S1: `set_subagents` was handed a different number of panels"
    );
    for (index, want) in committed.subagent_panels.iter().enumerate() {
        let detail = &booted.renderer.captured_subagents[index];
        assert_eq!(detail.name, want.name, "AC-S1: pane {index} `name` differs");
        assert_eq!(
            detail.task_id, want.task_id,
            "AC-S1: pane {index} `task_id` differs"
        );
        assert_eq!(
            detail.prompt, want.prompt,
            "AC-S1: pane {index} `prompt` differs"
        );
        assert_eq!(
            detail.logs, want.logs,
            "AC-S1: pane {index} `logs` differ element-by-element"
        );
        assert_eq!(
            detail.thinking, want.thinking,
            "AC-S1: pane {index} `thinking` differs"
        );
        assert_eq!(
            detail.content, want.content,
            "AC-S1: pane {index} `content` differs"
        );
        assert_eq!(
            detail.is_active, want.is_active,
            "AC-S1: pane {index} `is_active` differs"
        );
        assert_eq!(
            detail.context_tokens, want.context_tokens,
            "AC-S1: pane {index} `context_tokens` differs"
        );
        assert_eq!(
            detail.worked_duration,
            expected_detail_duration(want.worked_duration_secs),
            "AC-S1: pane {index} `worked_duration` is not the stored seconds"
        );
    }

    cleanup(&bundle);
}

/// **AC-S2** — `worked_duration_secs` equals the committed number and is **not**
/// recomputed from `started_at` / `last_activity_at` (D5 T3).
///
/// The fixture is built so the subtraction hypothesis is *falsifiable for every
/// panel*: panel A stores 12.5 s while its two clocks are 3600 s apart, panel B
/// stores 90 s while its two clocks are identical, panel C stores 0 s while its
/// clocks are 3600 s apart (in the sane order). If the implementation derived the
/// duration from the clocks, no equality below could hold by accident.
#[test]
fn ac_s2_worked_duration_secs_is_carried_and_never_recomputed_from_the_two_clocks() {
    let _guard = registry_lock();
    let bundle = bundle_dir("ac-s2");
    // (A) 12.5 s stored against clocks 3600 s apart; (B) 90 s stored against two
    // identical clocks; (C) 0 s stored against clocks 3600 s apart (sane order).
    let (a_started, a_last) = clock_pair(3612.5, 12.5);
    let (b_started, b_last) = clock_pair(20.0, 20.0);
    let (c_started, c_last) = clock_pair(3700.0, 100.0);
    let panels: Vec<SubagentPanel> = vec![
        PanelFixture {
            name: "t014e-ac-s2-a".to_string(),
            worked_secs: 12.5,
            started_at: Some(a_started),
            last_activity_at: Some(a_last),
            ..panel_fixture("a")
        }
        .build(),
        PanelFixture {
            name: "t014e-ac-s2-b".to_string(),
            worked_secs: 90.0,
            started_at: Some(b_started),
            last_activity_at: Some(b_last),
            ..panel_fixture("b")
        }
        .build(),
        PanelFixture {
            name: "t014e-ac-s2-c".to_string(),
            worked_secs: 0.0,
            started_at: Some(c_started),
            last_activity_at: Some(c_last),
            ..panel_fixture("c")
        }
        .build(),
    ];

    // The fixture proves the two hypotheses disagree *before* anything is stored.
    for panel in &panels {
        let started = panel
            .started_at
            .as_ref()
            .expect("every AC-S2 panel carries a start clock");
        let last = panel
            .last_activity_at
            .as_ref()
            .expect("every AC-S2 panel carries a last-activity clock");
        let delta_elapsed = (last.elapsed_secs - started.elapsed_secs).abs();
        let delta_wall = (last
            .wall
            .signed_duration_since(started.wall)
            .num_nanoseconds()
            .expect("the clock delta fits in nanoseconds") as f64
            / 1e9)
            .abs();
        assert!(
            (delta_elapsed - panel.worked_duration_secs).abs() > 1.0
                && (delta_wall - panel.worked_duration_secs).abs() > 1.0,
            "AC-S2 fixture: panel {} stores {} s, which is within 1 s of what the \
             two clocks would give (elapsed delta {delta_elapsed} s, wall delta \
             {delta_wall} s) — the equality below would prove nothing",
            panel.name,
            panel.worked_duration_secs,
        );
    }

    let ui = UiSection {
        subagent_panels: panels.clone(),
        ..base_ui()
    };
    assert_eq!(
        ui.structural_violations(),
        Vec::<String>::new(),
        "the AC-S2 fixture must be a sound section"
    );
    write(
        &bundle,
        "session-t014e-ac-s2",
        &payload(ui, Vec::new(), floors()),
        ProcessEnding::InProgress,
    );

    let committed = committed_ui(&bundle);
    assert_eq!(
        committed.subagent_panels, panels,
        "AC-S2: the committed panel vector changed"
    );
    let restored = restored_session(&bundle).0;
    let booted = boot_with_panes(&bundle);
    let snapshot = booted.renderer.snapshot();
    for (index, want) in panels.iter().enumerate() {
        let got = &snapshot.subagent_panels[index];
        assert_eq!(
            got.worked_duration_secs.to_bits(),
            want.worked_duration_secs.to_bits(),
            "AC-S2 (D5 T3): panel {} came back with {} s instead of the stored {} s",
            want.name,
            got.worked_duration_secs,
            want.worked_duration_secs,
        );
        // And it is provably NOT the clock delta. The witness is the DURABLE pair
        // (which the loader hands back byte-exact); the live pane's `Instant` side
        // is re-anchored from the wall by design (§5.6 T2, AC-S3) and therefore
        // cannot serve as a subtraction witness at all.
        let durable = restored
            .ui
            .subagent_panels
            .get(index)
            .unwrap_or_else(|| panic!("AC-S2: panel {index} did not come back at all"));
        assert_eq!(
            durable.worked_duration_secs.to_bits(),
            want.worked_duration_secs.to_bits(),
            "AC-S2 (D5 T3): the durable byte for panel {} is not the stored seconds",
            want.name,
        );
        let started = durable
            .started_at
            .as_ref()
            .expect("the clock pair must come back");
        let last = durable
            .last_activity_at
            .as_ref()
            .expect("the clock pair must come back");
        let delta_elapsed = (last.elapsed_secs - started.elapsed_secs).abs();
        let delta_wall = (last
            .wall
            .signed_duration_since(started.wall)
            .num_nanoseconds()
            .expect("the clock delta fits in nanoseconds") as f64
            / 1e9)
            .abs();
        assert!(
            (durable.worked_duration_secs - delta_elapsed).abs() > 1.0
                && (durable.worked_duration_secs - delta_wall).abs() > 1.0,
            "AC-S2 (D5 T3): panel {}'s duration ({}) is what subtracting its two \
             clocks would give (elapsed delta {delta_elapsed} s, wall delta \
             {delta_wall} s) — it was recomputed, not carried",
            want.name,
            durable.worked_duration_secs,
        );
        // The pane's own `Duration` is the stored number too.
        assert_eq!(
            booted.renderer.captured_subagents[index].worked_duration,
            expected_detail_duration(want.worked_duration_secs),
            "AC-S2: pane {}'s `worked_duration` is not the stored seconds",
            want.name,
        );
    }

    cleanup(&bundle);
}

/// **AC-S3** — `started_at` / `last_activity_at` re-anchor within the documented
/// error term (§5.6 T2) and their `wall` values are byte-exact.
///
/// The clocks are sampled the way the old process would have sampled them
/// (`elapsed_secs` measured against the *previous* process's anchor age, `wall`
/// an absolute calendar stamp): that is exactly the situation re-anchoring must
/// survive. The stored `wall` text is looked for in the raw `ui.json` bytes, and
/// the live `Instant` side is required to be ~90 s old, never "now".
#[test]
fn ac_s3_panel_clocks_re_anchor_within_the_error_term_and_their_wall_bytes_are_exact() {
    let _guard = registry_lock();
    let bundle = bundle_dir("ac-s3");
    let (started, last) = clock_pair(90.0, 0.0);
    let panel = PanelFixture {
        name: "t014e-ac-s3-coder".to_string(),
        is_active: true,
        started_at: Some(started.clone()),
        last_activity_at: Some(last.clone()),
        worked_secs: 90.0,
        ..panel_fixture("coder")
    }
    .build();
    let ui = UiSection {
        subagent_panels: vec![panel.clone()],
        ..base_ui()
    };
    assert_eq!(
        ui.structural_violations(),
        Vec::<String>::new(),
        "the AC-S3 fixture must be a sound section"
    );
    write(
        &bundle,
        "session-t014e-ac-s3",
        &payload(ui, Vec::new(), floors()),
        ProcessEnding::InProgress,
    );

    // (a) the bytes carry the wall stamp verbatim.
    let section = section_json(&bundle, UI_FILE);
    let panels = section["section"]["subagent_panels"]
        .as_array()
        .expect("subagent_panels is an array");
    let raw = String::from_utf8(member_bytes(&bundle, UI_FILE)).expect("ui.json is UTF-8");
    for (field, want) in [("started_at", &started), ("last_activity_at", &last)] {
        let stamp = panels
            .first()
            .and_then(|panel| panel.get(field))
            .and_then(|entry| entry.get("wall"))
            .and_then(|wall| wall.as_str())
            .unwrap_or_else(|| panic!("ui.json has no `subagent_panels[0].{field}.wall`: {raw}"));
        assert_eq!(
            stamp,
            want.wall.to_rfc3339(),
            "AC-S3: the `{field}` wall stamp in ui.json is not the captured bytes"
        );
        let parsed = DateTime::parse_from_rfc3339(stamp)
            .unwrap_or_else(|error| panic!("`{field}` wall is not RFC 3339: {error}"));
        assert_eq!(
            parsed.with_timezone(&Local),
            want.wall,
            "AC-S3: `{field}` lost sub-second precision on the way to disk"
        );
    }

    // (b) the loaded clocks: same values, same ordering.
    let (session, _ending) = restored_session(&bundle);
    let restored = &session.ui.subagent_panels[0];
    assert_eq!(
        restored
            .started_at
            .as_ref()
            .expect("started_at")
            .elapsed_secs
            .to_bits(),
        started.elapsed_secs.to_bits(),
        "AC-S3: `started_at.elapsed_secs` was rewritten on the way back"
    );
    assert_eq!(
        restored
            .last_activity_at
            .as_ref()
            .expect("last_activity_at")
            .elapsed_secs
            .to_bits(),
        last.elapsed_secs.to_bits(),
        "AC-S3: `last_activity_at.elapsed_secs` was rewritten on the way back"
    );
    assert_eq!(
        restored.started_at.as_ref().expect("started_at").wall,
        started.wall,
        "AC-S3: `started_at.wall` is not the captured instant"
    );
    assert_eq!(
        restored
            .last_activity_at
            .as_ref()
            .expect("last_activity_at")
            .wall,
        last.wall,
        "AC-S3: `last_activity_at.wall` is not the captured instant"
    );
    assert!(
        restored
            .last_activity_at
            .as_ref()
            .expect("last_activity_at")
            .wall
            > restored.started_at.as_ref().expect("started_at").wall,
        "AC-S3: the clock ordering flipped"
    );

    // (c) the re-anchor error term (§5.6 T2) — for both clocks. Stated honestly:
    // `to_instant()` is *defined* as `now - (wall_now - wall_recorded)`, so this
    // residual measures the drift/clamp of that reconstruction, not an independent
    // cross-clock reconstruction. The load-bearing witness against "defaulted to
    // now" is the age assertion immediately below.
    for (field, value) in [
        (
            "started_at",
            restored.started_at.as_ref().expect("started_at"),
        ),
        (
            "last_activity_at",
            restored
                .last_activity_at
                .as_ref()
                .expect("last_activity_at"),
        ),
    ] {
        let error = re_anchor_error(value);
        assert!(
            error < RE_ANCHOR_TOLERANCE_SECS,
            "AC-S3 (§5.6 T2): `{field}` re-anchors {error} s away from its own wall \
             stamp, outside the documented error term of \
             {RE_ANCHOR_TOLERANCE_SECS} s"
        );
    }
    assert!(
        Instant::now()
            .saturating_duration_since(
                restored
                    .started_at
                    .as_ref()
                    .expect("started_at")
                    .to_instant()
            )
            .as_secs_f64()
            > 88.0,
        "AC-S3: the restored start clock was defaulted to now — the pane would say \
         the subagent started this second"
    );

    // (d) the live pane: two `Instant`s inside the same error term of the durable
    // 90 s gap, never a zero-length one.
    let booted = boot_with_panes(&bundle);
    let detail = &booted.renderer.captured_subagents[0];
    let live_started = detail
        .started_at
        .expect("the live pane must carry a start instant");
    let live_last = detail
        .last_activity_at
        .expect("the live pane must carry a last-activity instant");
    let live_gap = live_last
        .saturating_duration_since(live_started)
        .as_secs_f64();
    assert!(
        (live_gap - 90.0).abs() < RE_ANCHOR_TOLERANCE_SECS,
        "AC-S3: the live pane shows a {live_gap} s span where the bundle records \
         90 s — the two re-anchored instants no longer agree to within the error term"
    );

    cleanup(&bundle);
}

/// **AC-S4** — a panel with `is_active: true` restores its bytes **and** emits
/// `NonRestorableNotice { kind: JoinHandle, subject: <panel name> }` (§6.3,
/// §10.3: "A restored `SubagentPanel { is_active: true }` SHALL emit `JoinHandle`
/// with `subject: <panel name>` even though the pane byte is kept").
///
/// **The byte half is green and pinned elsewhere** (`ac_s1_…` asserts the active
/// panel's `is_active` and every field come back unchanged). **The notice half is
/// emitted by production**: the restore path emits `JoinHandle` from **two**
/// production sites —
/// `src/session/worker_journal.rs:844-851` (kind line **845**), where `subject`
/// is the worker key, and `src/ui/session.rs:1256-1274` (kind line **1263**),
/// which pushes one `NonRestorableNotice { kind: JoinHandle, subject:
/// <panel name> }` per restored `SubagentPanel` carrying `is_active: true`.
/// `grep 'NonRestorableKind::JoinHandle' src/` yields those two production sites,
/// a third on the raw-UI path (`src/ui/raw.rs:585`), plus in-crate unit tests
/// (`src/ui/session_tests.rs:1015`, `src/ui/tui/tests.rs:2515`). A bundle with an
/// active panel and no worker section therefore does announce the handle that
/// died with the process. The assertion below is kept at full strength; the
/// `#[ignore]` this probe carried while it awaited its first execution was
/// dropped by t-035 after it passed under `--ignored` twice, so it now runs in
/// the ordinary gate.
#[test]
fn ac_s4_an_active_panel_restores_its_byte_and_emits_a_join_handle_notice_naming_it() {
    let _guard = registry_lock();
    let panel_name = "t014e-ac-s4-panel-coder";
    let bundle = bundle_dir("ac-s4");
    let panel = PanelFixture {
        name: panel_name.to_string(),
        is_active: true,
        logs: vec!["tool replace dispatched".to_string()],
        thinking: "still thinking".to_string(),
        content: "half-written answer".to_string(),
        ..panel_fixture(panel_name)
    }
    .build();
    let ui = UiSection {
        subagent_panels: vec![panel.clone()],
        ..base_ui()
    };
    // No worker section: the criterion is about the *panel*, not the worker.
    write(
        &bundle,
        "session-t014e-ac-s4",
        &payload(ui, Vec::new(), floors()),
        ProcessEnding::InProgress,
    );
    let committed = committed_ui(&bundle);
    assert_eq!(
        committed.subagent_panels,
        vec![panel.clone()],
        "AC-S4: the active panel's byte must be kept (§6.3)"
    );

    let booted = boot_with_panes(&bundle);
    let notices = &booted.report.repair.notices;
    let handle =
        notice_for(notices, &NonRestorableKind::JoinHandle, panel_name).unwrap_or_else(|| {
            panic!(
                "AC-S4 (§10.3): a restored active panel must emit \
         JoinHandle with subject = the panel name; notices were {notices:#?}"
            )
        });
    assert!(
        !handle.detail.is_empty(),
        "AC-S4: the JoinHandle notice for {panel_name} carries no explanation"
    );

    cleanup(&bundle);
}

// ===========================================================================
// §9.8 — the time base
// ===========================================================================

/// **AC-T1** — every restored `TimePoint.elapsed_secs` is measured against the
/// same anchor as `Manifest.anchor_wall`, and the ordering of
/// `saved_elapsed_secs` between members matches the committed ordering (§5.6 T4).
///
/// Within one process the anchor cannot drift, so this test pins the three
/// observable forms the claim takes: (a) `manifest.anchor_wall` *is* the anchor
/// this process measures against; (b) for every restored clock point — panel,
/// plan, abort and worker — `wall - anchor_wall == elapsed_secs`, i.e. the offset
/// really is on that anchor's scale, and it arrives bit-for-bit; (c) the
/// `saved_elapsed_secs` the manifest and each member header carry are the same
/// capture instant in the committed order, element-for-element.
#[test]
fn ac_t1_every_restored_offset_is_measured_against_the_anchor_the_manifest_names() {
    let _guard = registry_lock();
    let bundle = bundle_dir("ac-t1");
    let keys = [
        "t014et1-coder-t-91001",
        "t014et1-coder-t-91002",
        "t014et1-coder-t-91003",
    ];
    // Fixture clocks: synthetic anchor-exact points, one real `TimePoint::now()`
    // pair, and worker clocks in a deliberately non-monotonic order across
    // members so "same anchor" cannot be inferred from ordering alone.
    let worker_clocks: Vec<(TimePoint, Option<TimePoint>)> = vec![
        (point(0.25), Some(point(1.0))),
        (TimePoint::now(), None),
        (point(0.75), None),
    ];
    let workers: Vec<WorkerSection> = keys
        .iter()
        .zip(worker_clocks.iter())
        .map(|(key, (started, last))| {
            WorkerFixture {
                started: Some(started.clone()),
                last_activity: last.clone(),
                ..worker_fixture(key)
            }
            .build()
        })
        .collect();
    let panel_clocks = (point(2.75), Some(point(3.0)));
    let ui = UiSection {
        subagent_panels: vec![
            PanelFixture {
                name: "t014e-ac-t1-panel".to_string(),
                started_at: Some(panel_clocks.0.clone()),
                last_activity_at: panel_clocks.1.clone(),
                ..panel_fixture("t1")
            }
            .build(),
        ],
        plan_start: Some(point(1.5)),
        plan_completed: Some(point(2.0)),
        abort_armed: true,
        abort_armed_at: Some(point(2.5)),
        ..base_ui()
    };
    assert_eq!(
        ui.structural_violations(),
        Vec::<String>::new(),
        "the AC-T1 fixture must be a sound section"
    );
    write(
        &bundle,
        "session-t014e-ac-t1",
        &payload(ui, workers.clone(), floors()),
        ProcessEnding::InProgress,
    );

    let (session, _ending) = restored_session(&bundle);

    // (a) the anchor the manifest names is the anchor this process measures
    // `TimePoint::now()` against — the one relation an integration test can prove.
    assert_eq!(
        session.manifest.anchor_wall,
        anchor_wall(),
        "AC-T1: Manifest.anchor_wall is not this process's anchor"
    );
    assert_eq!(
        session.manifest.anchor_wall.to_rfc3339(),
        anchor_wall().to_rfc3339(),
        "AC-T1: Manifest.anchor_wall is not byte-identical to this process's anchor"
    );

    // (b) every restored clock point sits on that anchor's scale.
    let mut points: Vec<(String, TimePoint)> = Vec::new();
    for panel in &session.ui.subagent_panels {
        if let Some(started) = &panel.started_at {
            points.push((format!("panel {} started_at", panel.name), started.clone()));
        }
        if let Some(last) = &panel.last_activity_at {
            points.push((
                format!("panel {} last_activity_at", panel.name),
                last.clone(),
            ));
        }
    }
    for (field, value) in [
        ("plan_start", &session.ui.plan_start),
        ("plan_completed", &session.ui.plan_completed),
        ("abort_armed_at", &session.ui.abort_armed_at),
    ] {
        if let Some(value) = value {
            points.push((field.to_string(), value.clone()));
        }
    }
    for section in &session.workers {
        points.push((
            format!("{} started", section.worker_key),
            section.started.clone(),
        ));
        if let Some(completed) = &section.completed {
            points.push((
                format!("{} completed", section.worker_key),
                completed.clone(),
            ));
        }
        if let Some(last) = &section.last_activity {
            points.push((
                format!("{} last_activity", section.worker_key),
                last.clone(),
            ));
        }
    }
    assert_eq!(
        points.len(),
        2 + 3 + (2 + 1 + 1),
        "AC-T1 fixture: the expected number of clock points changed"
    );
    for (label, value) in &points {
        assert!(
            value.is_sane(),
            "AC-T1: the restored clock point {label} is not sane: {value:?}"
        );
        let drift = anchor_consistency(value, &session.manifest.anchor_wall);
        assert!(
            drift < RE_ANCHOR_TOLERANCE_SECS,
            "AC-T1 (§5.6 T4): {label} is measured {drift} s away from the anchor \
             Manifest.anchor_wall names, outside the documented error term",
        );
    }

    // The committed offsets arrive bit-for-bit, in the committed order.
    let committed = committed_workers(&bundle);
    for (index, section) in committed.iter().enumerate() {
        let restored = &session.workers[index];
        let (want_started, want_last) = &worker_clocks[index];
        assert_eq!(
            restored.worker_key, section.worker_key,
            "AC-T1: worker member {index} is not the committed one"
        );
        assert_eq!(
            restored.started.elapsed_secs.to_bits(),
            want_started.elapsed_secs.to_bits(),
            "AC-T1: worker {} `started.elapsed_secs` is not the captured offset",
            restored.worker_key
        );
        assert_eq!(
            restored.started.wall, want_started.wall,
            "AC-T1: worker {} `started.wall` is not the captured stamp",
            restored.worker_key
        );
        match (want_last, &restored.last_activity) {
            (None, None) => {}
            (Some(want), Some(got)) => {
                assert_eq!(
                    got.elapsed_secs.to_bits(),
                    want.elapsed_secs.to_bits(),
                    "AC-T1: worker {} `last_activity.elapsed_secs` is not the \
                     captured offset",
                    restored.worker_key
                );
                assert_eq!(
                    got.wall, want.wall,
                    "AC-T1: worker {} `last_activity.wall` is not the captured stamp",
                    restored.worker_key
                );
            }
            (want, got) => panic!(
                "AC-T1: worker {} `last_activity` presence changed ({want:?} captured, \
                 {got:?} restored)",
                restored.worker_key
            ),
        }
        assert_eq!(
            restored.last_activity, section.last_activity,
            "AC-T1: worker {} `last_activity` changed on the way back",
            section.worker_key
        );
    }
    let committed_panel = &committed_ui(&bundle).subagent_panels[0];
    assert_eq!(
        session.ui.subagent_panels[0].started_at, committed_panel.started_at,
        "AC-T1: the panel clocks changed on the way back"
    );

    // Every section clock point, in the captured order, element-for-element — the
    // ordering half of the criterion read through the sections themselves.
    let wanted_offsets: Vec<f64> = vec![
        panel_clocks.0.elapsed_secs,
        panel_clocks
            .1
            .as_ref()
            .expect("the fixture panel carries a last-activity clock")
            .elapsed_secs,
        1.5,
        2.0,
        2.5,
        worker_clocks[0].0.elapsed_secs,
        worker_clocks[0]
            .1
            .as_ref()
            .expect("worker 0 carries a last-activity clock")
            .elapsed_secs,
        worker_clocks[1].0.elapsed_secs,
        worker_clocks[2].0.elapsed_secs,
    ];
    let got_offsets: Vec<f64> = vec![
        session.ui.subagent_panels[0]
            .started_at
            .as_ref()
            .expect("started_at")
            .elapsed_secs,
        session.ui.subagent_panels[0]
            .last_activity_at
            .as_ref()
            .expect("last_activity_at")
            .elapsed_secs,
        session
            .ui
            .plan_start
            .as_ref()
            .expect("plan_start")
            .elapsed_secs,
        session
            .ui
            .plan_completed
            .as_ref()
            .expect("plan_completed")
            .elapsed_secs,
        session
            .ui
            .abort_armed_at
            .as_ref()
            .expect("abort_armed_at")
            .elapsed_secs,
        session.workers[0].started.elapsed_secs,
        session.workers[0]
            .last_activity
            .as_ref()
            .expect("last_activity")
            .elapsed_secs,
        session.workers[1].started.elapsed_secs,
        session.workers[2].started.elapsed_secs,
    ];
    assert_eq!(
        got_offsets.len(),
        wanted_offsets.len(),
        "AC-T1: the number of clock points in the sections changed"
    );
    for (position, (got, want)) in got_offsets.iter().zip(wanted_offsets.iter()).enumerate() {
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "AC-T1: clock point {position} of the committed order came back as \
             {got} instead of {want}"
        );
    }

    // (c) `saved_elapsed_secs`: one capture instant, same order, member header
    // mirrors the manifest entry (§5.6 T4 / L5).
    let manifest = manifest_json(&bundle);
    let members = manifest["members"].as_array().expect("members is an array");
    assert_eq!(
        members.len(),
        session.manifest.members.len(),
        "AC-T1: the manifest on disk and the restored manifest disagree about the member list"
    );
    for (index, entry) in session.manifest.members.iter().enumerate() {
        let on_disk = &members[index];
        assert_eq!(
            on_disk["rel_path"].as_str(),
            Some(entry.rel_path.as_str()),
            "AC-T1: member {index} is listed out of the committed order"
        );
        assert_eq!(
            on_disk["saved_wall"].as_str(),
            Some(entry.saved_wall.to_rfc3339().as_str()),
            "AC-T1: member {index} `saved_wall` is not the committed stamp"
        );
        assert_eq!(
            on_disk["saved_elapsed_secs"]
                .as_f64()
                .expect("saved_elapsed_secs is a number")
                .to_bits(),
            entry.saved_elapsed_secs.to_bits(),
            "AC-T1: member {index} `saved_elapsed_secs` is not the committed offset"
        );
        let header = section_json(&bundle, &entry.rel_path)["header"].clone();
        assert_eq!(
            header["saved_elapsed_secs"]
                .as_f64()
                .expect("header saved_elapsed_secs is a number")
                .to_bits(),
            entry.saved_elapsed_secs.to_bits(),
            "AC-T1 (L5): the header of {} disagrees with its manifest entry",
            entry.rel_path
        );
    }
    let saved_offsets: Vec<f64> = session
        .manifest
        .members
        .iter()
        .map(|entry| entry.saved_elapsed_secs)
        .collect();
    for pair in saved_offsets.windows(2) {
        assert_eq!(
            pair[0].to_bits(),
            pair[1].to_bits(),
            "AC-T1 (§3.2/§3.3): one commit stamped two members at different capture \
             instants ({} vs {})",
            pair[0],
            pair[1]
        );
    }
    let saved_walls: Vec<String> = session
        .manifest
        .members
        .iter()
        .map(|entry| entry.saved_wall.to_rfc3339())
        .collect();
    for pair in saved_walls.windows(2) {
        assert_eq!(
            pair[0], pair[1],
            "AC-T1: one commit stamped two members with different `saved_wall`"
        );
    }

    cleanup(&bundle);
}

/// **AC-T2** — a `TimePoint` with `elapsed_secs` NaN / infinity / negative is
/// **Refused**, with no silent defaulting to "now" (§5.6).
///
/// Four branches, all pinned at full strength:
///
/// * **negative in `ui.json`** → committed (the committer does not validate) and
///   refused by the loader, naming the member `ui.json`, with the bad byte still
///   on disk afterwards (nothing was defaulted);
/// * **worker clock negative** → refused, naming that worker's member;
/// * **NaN / ±infinity** → these can never *become* a bundle byte: the commit
///   seam refuses them at `check_time_points` (`src/session/commit.rs:641`, run
///   inside `assemble_members` before C1), because `assemble_members` serializes
///   every member before the first write and `serde_json` would turn a non-finite
///   float into `null` — a silent default, which §5.6 forbids. The commit
///   therefore returns `CommitError::Serialize` and there is no manifest to load.
///   The load-side sentence at `src/session/restore.rs:1101` is therefore
///   unreachable through the commit seam, and this test says so out loud instead
///   of pretending to reach it.
#[test]
fn ac_t2_a_non_finite_or_negative_offset_is_refused_and_never_defaulted_to_now() {
    let _guard = registry_lock();
    // The sanity predicate itself, so the three shapes below cannot be waved
    // through as "sane" by a fixture bug.
    for (label, value) in [
        ("negative", -0.5_f64),
        ("NaN", f64::NAN),
        ("+infinity", f64::INFINITY),
        ("-infinity", f64::NEG_INFINITY),
    ] {
        assert!(
            !TimePoint {
                elapsed_secs: value,
                wall: anchor_wall(),
            }
            .is_sane(),
            "AC-T2 fixture: {label} ({value}) is reported as a sane TimePoint"
        );
    }
    assert!(
        TimePoint::now().is_sane(),
        "AC-T2 fixture: a real clock point is not sane"
    );

    // (a) negative offset in a panel clock: commits, then refuses.
    let dirty = bundle_dir("ac-t2-negative");
    let panel = PanelFixture {
        name: "t014e-ac-t2-panel".to_string(),
        started_at: Some(TimePoint {
            elapsed_secs: -1.0,
            wall: anchor_wall(),
        }),
        last_activity_at: Some(TimePoint::now()),
        ..panel_fixture("dirty")
    }
    .build();
    let ui = UiSection {
        subagent_panels: vec![panel],
        ..base_ui()
    };
    // Deliberately U-chk-dirty: the committer does not validate the section
    // (§7 C1–C4), which is what makes the loader's refusal the load-bearing
    // check. Stated, not hidden.
    assert!(
        !ui.structural_violations().is_empty(),
        "AC-T2 fixture: the section must already be known-dirty, or the loader's \
         refusal proves nothing"
    );
    write(
        &dirty,
        "session-t014e-ac-t2-negative",
        &payload(ui, Vec::new(), floors()),
        ProcessEnding::InProgress,
    );
    let before = fingerprints(&dirty);
    let (code, member, message) = refused(&dirty);
    assert_eq!(
        code,
        RefusalCode::HeaderManifestDisagreement,
        "AC-T2 (§5.6): a negative monotonic offset must be a refusal, got [{code:?}] {message}"
    );
    assert_eq!(
        member.as_deref(),
        Some(UI_FILE),
        "AC-T2: the refusal must name `ui.json`, got {member:?} ({message})"
    );
    assert!(
        message.contains("is not a sane TimePoint") && message.contains("-1.0"),
        "AC-T2: the refusal must state the offending value, got: {message}"
    );
    let raw = String::from_utf8_lossy(&member_bytes(&dirty, UI_FILE)).into_owned();
    assert!(
        raw.contains("-1.0"),
        "AC-T2: the loader defaulted the bad offset before reading it — the byte \
         with -1.0 is no longer in ui.json"
    );
    assert_bundle_bytes_unchanged(&dirty, &before);

    // (b) worker clock negative: refused, naming the worker member.
    let key = "t014et2-coder-t-91010";
    let dirty_worker = bundle_dir("ac-t2-worker");
    let section = WorkerFixture {
        started: Some(TimePoint {
            elapsed_secs: -0.5,
            wall: anchor_wall(),
        }),
        ..worker_fixture(key)
    }
    .build();
    write(
        &dirty_worker,
        "session-t014e-ac-t2-worker",
        &payload(base_ui(), vec![section], floors()),
        ProcessEnding::InProgress,
    );
    let (code, member, message) = refused(&dirty_worker);
    assert_eq!(
        code,
        RefusalCode::HeaderManifestDisagreement,
        "AC-T2 (§5.6): a negative worker offset must be a refusal, got [{code:?}] {message}"
    );
    assert_eq!(
        member.as_deref(),
        Some(worker_rel_path(key).as_str()),
        "AC-T2: the refusal must name the worker's member, got {member:?} ({message})"
    );
    assert!(
        message.contains(key)
            && message.contains("started")
            && message.contains("never defaulted to now"),
        "AC-T2: the refusal must name the key, the field and the no-default rule, \
         got: {message}"
    );

    // (c) NaN and ±infinity: never writable, so never loadable.
    for (label, value) in [
        ("NaN", f64::NAN),
        ("+infinity", f64::INFINITY),
        ("-infinity", f64::NEG_INFINITY),
    ] {
        let bundle = bundle_dir(&format!("ac-t2-{label}"));
        let panel = PanelFixture {
            name: "t014e-ac-t2-nonfinite".to_string(),
            started_at: Some(TimePoint {
                elapsed_secs: value,
                wall: anchor_wall(),
            }),
            ..panel_fixture("nonfinite")
        }
        .build();
        let ui = UiSection {
            subagent_panels: vec![panel],
            ..base_ui()
        };
        let outcome = commit(
            &bundle,
            "session-t014e-ac-t2-nonfinite",
            &payload(ui, Vec::new(), floors()),
            ProcessEnding::InProgress,
        );
        let Err(error) = outcome else {
            panic!(
                "AC-T2: a {label} elapsed_secs was accepted by the commit seam \
                 ({}) — it must never reach a bundle byte",
                bundle.display()
            );
        };
        match &error {
            CommitError::Serialize(text) => assert!(
                !text.is_empty(),
                "AC-T2: the {label} refusal carries no reason"
            ),
            other => panic!(
                "AC-T2: a {label} elapsed_secs must be a serialization refusal, got {other:?}"
            ),
        }
        assert!(
            !bundle.join(MANIFEST_FILE).exists(),
            "AC-T2: a {label} clock point produced a manifest — a partial bundle is \
             now readable as a commit point"
        );
        match load_bundle(&bundle) {
            RestoreOutcome::NoBundle { .. } => {}
            other => panic!(
                "AC-T2: after refusing to write a {label} offset, {} is not an \
                 untouched directory: {}",
                bundle.display(),
                explain(&other)
            ),
        }
        cleanup(&bundle);
    }

    cleanup(&dirty);
    cleanup(&dirty_worker);
}

// ===========================================================================
// §9.9 — abort mid-subagent
// ===========================================================================

/// **AC-A1** — the abort fixture: one active worker (`status_label:
/// "In Progress"`, `completed_seq: None`, `history_source: LiveMirror`, ≥1
/// assistant turn with paired `tool_calls`), one `SubagentPanel` with
/// `is_active: true` and non-empty `logs` / `thinking` / `content`, a composer
/// draft, a non-zero `chat_scroll`; the session commits with
/// `ending: UserAbort`.
///
/// Everything then comes back as it was: the on-disk ending is `userAbort`, the
/// restored ending is `UserAbort` (never the `HardKillInferred` fallback), the
/// worker is `InProgress` with `completed_seq: None` and its mirrored history,
/// the panel vector is byte-equal, the composer draft and the non-zero scroll
/// offset survive the live clamp.
///
/// **Stated limit:** the *live trigger* half of §9.9 ("the operator aborted, so
/// the stamp is `userAbort`") is session-loop wiring pinned by
/// `tests/test_session_abort_flush.rs`; this test pins the data half, i.e. that a
/// `UserAbort` commit point carries and returns the whole aborted state.
#[test]
fn ac_a1_abort_mid_subagent_commits_user_abort_and_comes_back_as_it_was() {
    let _guard = registry_lock();
    let bundle = bundle_dir("ac-a1");
    let key = "t014ea1-coder-t-90001";
    let (started, last) = clock_pair(30.0, 5.0);
    let (panel_started, panel_last) = clock_pair(30.0, 5.0);
    let section = WorkerFixture {
        task_id: Some("t-90001".to_string()),
        started: Some(started),
        last_activity: Some(last),
        ..worker_fixture(key)
    }
    .build();
    let panel = PanelFixture {
        name: "coder".to_string(),
        task_id: Some("t-90001".to_string()),
        is_active: true,
        logs: vec![
            "tool read_file started".to_string(),
            "replace 1 occurrence".to_string(),
        ],
        thinking: "planning the change".to_string(),
        content: "edited src/lib.rs".to_string(),
        worked_secs: 42.5,
        started_at: Some(panel_started),
        last_activity_at: Some(panel_last),
        ..panel_fixture("coder")
    }
    .build();
    let ui = UiSection {
        subagent_panels: vec![panel.clone()],
        composer: ComposerState {
            input_text: "keep this draft\nsecond line".to_string(),
            cursor: 13,
            input_draft: "draft before history".to_string(),
            history: vec!["/thought".to_string(), "/abort".to_string()],
            history_index: Some(0),
        },
        view: ViewState {
            chat_scroll: 20,
            focused_panel: "subagents".to_string(),
            show_subagent_panel: true,
            selected_subagent_idx: 0,
            ..ViewState::default()
        },
        ..base_ui()
    };
    assert_eq!(
        ui.structural_violations(),
        Vec::<String>::new(),
        "the AC-A1 fixture must be a sound section"
    );
    // The composer and scroll witnesses are captured *before* the section moves
    // into the commit payload, so the screen comparison below is against what the
    // pre-crash screen held, not against something the loader just handed back.
    let composer_witness = ui.composer.clone();
    let scroll_witness = ui.view.chat_scroll;
    write(
        &bundle,
        "session-t014e-ac-a1",
        &payload(ui, vec![section], floors()),
        ProcessEnding::UserAbort,
    );

    // The on-disk ending is the abort, verbatim (§3.10 spelling `userAbort`).
    assert_eq!(
        manifest_json(&bundle)["ending"]["kind"].as_str(),
        Some("userAbort"),
        "AC-A1: the committed manifest does not say the operator aborted"
    );

    let (session, ending) = restored_session(&bundle);
    assert_eq!(
        ending,
        ProcessEnding::UserAbort,
        "AC-A1: the restored ending is not UserAbort"
    );
    assert_ne!(
        ending,
        ProcessEnding::HardKillInferred,
        "AC-A1: an explicit abort must never be re-labelled as an inferred hard kill"
    );
    let committed = &session.workers[0];
    assert_eq!(
        committed.status_label, "In Progress",
        "AC-A1: the committed worker label is not the in-flight label"
    );
    assert_eq!(
        committed.status,
        WorkerStatus::InProgress,
        "AC-A1: the committed worker status is not InProgress"
    );
    assert_eq!(
        committed.completed_seq, None,
        "AC-A1: an aborted worker has no completed_seq — claiming one would be the \
         silent 'complete' the criterion forbids"
    );
    assert_eq!(
        committed.history_source,
        HistorySource::LiveMirror,
        "AC-A1: the aborted worker's history must be the live mirror"
    );
    assert!(
        committed.history.iter().any(|record| matches!(
            record,
            MessageRecord::Assistant { tool_calls, .. } if !tool_calls.is_empty()
        )),
        "AC-A1 fixture: the worker history must hold at least one assistant turn \
         with tool calls: {:#?}",
        committed.history
    );

    // The full boot pass: the aborted session is `Restored`, and the worker is
    // reported as interrupted rather than completed.
    let booted = boot_with_panes(&bundle);
    assert_eq!(
        booted.session_id, "session-t014e-ac-a1",
        "AC-A1/I4: the boot pass re-minted the session id"
    );
    assert_eq!(
        booted.report.previous_ending,
        Some(ProcessEnding::UserAbort),
        "AC-A1: BootReport::previous_ending is not the stored abort ending"
    );
    assert!(booted.report.restored, "AC-A1: the boot did not restore");
    assert!(
        !booted.report.partial,
        "AC-A1: an aborted-but-complete bundle must not be reported as partial: {:?}",
        booted.report.unrecoverable_workers
    );
    assert_eq!(
        booted.report.interrupted_worker_keys,
        vec![key.to_string()],
        "AC-A1: the in-flight worker must be reported as interrupted"
    );

    // The live registry: the worker is back, still in flight, with a new label.
    let live = workers::worker_state_snapshot(key).expect("AC-A1: the worker is in WORKERS");
    assert_eq!(
        live.completed_seq, None,
        "AC-A1: the entry grew a completed_seq"
    );
    assert!(live.is_active(), "AC-A1: the entry must still be active");
    assert!(
        workers::has_active_workers(),
        "AC-A1: the restored session claims an in-flight worker but has none"
    );
    assert_eq!(
        live.info.status, INTERRUPTED_STATUS_LABEL,
        "AC-A1 (documented deviation, flow §2.2): the live label is the interrupted \
         label, while the committed byte keeps `In Progress`"
    );
    assert_eq!(
        session.workers[0].status_label, "In Progress",
        "AC-A1: the committed worker byte was rewritten"
    );

    // The screen: the panel vector, the draft and the non-zero scroll offset.
    assert_eq!(
        booted.report.subagent_panels,
        vec![panel.clone()],
        "AC-A1: the active subagent panel did not come back as it was"
    );
    let snapshot = booted.renderer.snapshot();
    assert_eq!(
        snapshot.composer, composer_witness,
        "AC-A1: the composer draft/caret/history did not come back"
    );
    assert_eq!(
        snapshot.view.chat_scroll, scroll_witness,
        "AC-A1: the non-zero chat_scroll did not survive the live clamp"
    );
    assert!(
        !booted
            .report
            .repair
            .warnings
            .iter()
            .any(|warning| warning.contains("scroll")),
        "AC-A1: the restore reported a scroll difference where none was expected: \
         {:#?}",
        booted.report.repair.warnings
    );
    assert_eq!(
        snapshot.subagent_panels[0].logs, panel.logs,
        "AC-A1: the live subagent pane lost the aborted worker's logs"
    );
    assert!(
        snapshot.subagent_panels[0].is_active,
        "AC-A1: the live subagent pane flipped the panel to inactive"
    );

    cleanup(&bundle);
}

/// **AC-A2** — the notices are present and specific: `CancellationToken`,
/// `JoinHandle`, `DropGuard` for each in-flight worker, plus a `PendingNotice`
/// for a notice queued at abort time; the outcome is `Restored` (never silently
/// "complete"), `report.notices` is non-empty, and its subjects are the worker
/// keys.
///
/// **Stated basis for the `PendingNotice` half:** the capture side **does** sample
/// `PENDING_NOTICES` / `WORKER_INBOXES` —
/// `session::worker_journal::project_worker_section` folds in
/// `pending_notice_notices` (fn 995-1008, call at `worker_journal.rs:624`), which
/// peeks the very inbox `notice::worker_inbox_len` counts
/// (`orchestrator::notice::peek_worker_inbox`, `src/orchestrator/notice.rs:285-292`)
/// — so a notice that really was queued when the process died does reach the
/// bundle; that capture half is pinned by the probe
/// `ac_a2_capture_side_records_the_notice_queued_at_abort_time`, which runs in
/// the ordinary gate (t-035 dropped its `#[ignore]` once it had executed green).
/// Boot reads the count from
/// `section.non_restorable` and re-spells it
/// (`src/ui/session.rs:1157-1181`), so this test supplies the queued count
/// through that channel — the only one the restore path reads — while also
/// posting real live notices so the invariant is anchored in a real queue.
#[test]
fn ac_a2_the_notices_are_present_specific_and_the_outcome_is_restored_not_complete() {
    let _guard = registry_lock();
    let bundle = bundle_dir("ac-a2");
    let keys = ["t014ea2-coder-t-90151", "t014ea2-coder-t-90152"];
    let sections: Vec<WorkerSection> = keys
        .iter()
        .map(|key| {
            WorkerFixture {
                task_id: Some(key.rsplit('-').next().expect("task id tail").to_string()),
                non_restorable: queued_notice_notices(key, 1),
                ..worker_fixture(key)
            }
            .build()
        })
        .collect();

    // A notice really queued at abort time, in the real store, for each worker.
    for key in keys {
        let _ = notice::post_notice_to_worker(key, "the validator wants an assertion", None);
        assert_eq!(
            notice::worker_inbox_len(key),
            1,
            "AC-A2 fixture: a notice must be queued for {key} at abort time"
        );
    }

    write(
        &bundle,
        "session-t014e-ac-a2",
        &payload(base_ui(), sections, floors()),
        ProcessEnding::UserAbort,
    );

    // The outcome is `Restored`: not `PartiallyRestored`, and never a completion.
    let (session, ending) = restored_session(&bundle);
    assert_eq!(
        ending,
        ProcessEnding::UserAbort,
        "AC-A2: not the abort ending"
    );
    for section in &session.workers {
        assert_ne!(
            section.status,
            WorkerStatus::Completed,
            "AC-A2: the aborted worker {} came back as completed",
            section.worker_key
        );
        assert_eq!(
            section.completed_seq, None,
            "AC-A2: the aborted worker {} came back with a completed_seq",
            section.worker_key
        );
    }

    let booted = boot_with_panes(&bundle);
    assert!(booted.report.restored, "AC-A2: the boot did not restore");
    assert!(
        !booted.report.partial,
        "AC-A2: the outcome must be Restored, never the silent-complete or partial \
         branch: {:?}",
        booted.report.unrecoverable_workers
    );
    let notices = &booted.report.repair.notices;
    assert!(
        !notices.is_empty(),
        "AC-A2 (D6): a restore that lost capabilities must report them, not stay silent"
    );
    for key in keys {
        for kind in [
            NonRestorableKind::CancellationToken,
            NonRestorableKind::JoinHandle,
            NonRestorableKind::DropGuard,
            NonRestorableKind::PendingNotice,
        ] {
            let found = notice_for(notices, &kind, key).unwrap_or_else(|| {
                panic!(
                    "AC-A2: no {kind:?} notice naming the in-flight worker {key}; \
                     notices were {notices:#?}"
                )
            });
            assert!(
                !found.detail.is_empty(),
                "AC-A2: the {kind:?} notice for {key} carries no explanation"
            );
        }
    }
    // The two losses every process takes with it, so a reader can tell "worker
    // capability" from "session capability".
    for kind in [
        NonRestorableKind::ToolSchemaCharge,
        NonRestorableKind::HarnessStats,
    ] {
        assert!(
            notice_for(notices, &kind, "session").is_some(),
            "AC-A2: no {kind:?} notice for the session; notices were {notices:#?}"
        );
    }
    // The subjects are the worker keys (plus the two session-wide losses).
    let strays: Vec<&str> = notices
        .iter()
        .filter(|notice| !keys.contains(&notice.subject.as_str()) && notice.subject != "session")
        .map(|notice| notice.subject.as_str())
        .collect();
    assert!(
        strays.is_empty(),
        "AC-A2: every notice subject must be a worker key of this bundle (or the \
         session itself), got {strays:?}"
    );
    for key in keys {
        assert!(
            notices.iter().any(|notice| notice.subject == *key),
            "AC-A2: the worker keys must appear as notice subjects; {key} does not"
        );
    }
    // And the operator sees it in the notice text (N-MGR-1).
    let notice_text = booted
        .report
        .restore_notice
        .clone()
        .expect("AC-A2: the restore notice must exist for an aborted session");
    for key in keys {
        assert!(
            notice_text.contains(key),
            "AC-A2: the restore notice does not name {key}: {notice_text}"
        );
    }
    assert!(
        notice_text.contains("no task exists to continue it"),
        "AC-A2: the restore notice must say the worker is not running: {notice_text}"
    );

    cleanup(&bundle);
}

/// **AC-A2 capture half** — a notice that really was queued in the pending /
/// notice-inbox structures at abort time SHALL leave a trace the loader can
/// report. The capture path provides one: `pending_notice_notices`
/// (`src/session/worker_journal.rs:995-1008`) is folded into
/// `section.non_restorable` by `project_worker_section` at
/// `src/session/worker_journal.rs:624`, reading the live inbox through
/// `orchestrator::notice::peek_worker_inbox`
/// (`src/orchestrator/notice.rs:285-292`) — the same `WORKER_INBOXES` entry that
/// `notice::worker_inbox_len` counts, not a second store. Kept at full strength
/// and run in the ordinary gate: the `#[ignore]` this probe carried while it
/// awaited its first execution was dropped by t-035, after it had passed under
/// `--ignored` twice.
#[test]
fn ac_a2_capture_side_records_the_notice_queued_at_abort_time() {
    let _guard = registry_lock();
    let guard = workers::register_active_worker_with_token(
        Some("t-90153".to_string()),
        Agent::Coder.as_str().to_string(),
        "t014e queued notice probe".to_string(),
        Some(CancellationToken::new()),
    );
    let key = guard.0.clone();
    let _ = notice::post_notice_to_worker(&key, "the arbitrator wants a re-plan", None);
    assert_eq!(
        notice::worker_inbox_len(&key),
        1,
        "AC-A2 fixture: the notice must be in the real inbox"
    );

    let state = workers::worker_state_snapshot(&key).expect("the worker is live");
    let section = project_worker_section(&key, &state, Agent::Coder, None);
    assert!(
        section
            .non_restorable
            .iter()
            .any(|notice| notice.kind == NonRestorableKind::PendingNotice && notice.subject == key),
        "AC-A2: a notice queued in the live inbox at capture time must be recorded \
         in the worker section (the loader cannot read a dead process's notice \
         store); the section says {:#?}",
        section.non_restorable
    );
    drop(guard);
}

/// **AC-A3** — the cancellation token of a restored worker is **not** the old one
/// (it cannot be), the worker is reported as non-cancellable until it is
/// re-armed, and nothing in the restore path re-launches it.
///
/// The witness is designed so the identity of the two tokens is decidable rather
/// than asserted: the pre-crash token is **cancelled** before the crash (that is
/// the abort), and the token attached to the restored entry is not — plus a fresh
/// child of the restored token is not cancelled while a child of the old one
/// still is, which is only possible for a different token tree. Then nothing
/// re-launched: no journal, no inbox traffic, no completion, no drawn
/// `completed_seq`, and the live entry is labelled interrupted.
#[test]
fn ac_a3_the_restored_token_is_not_the_old_one_and_nothing_in_the_restore_re_launches_it() {
    let _guard = registry_lock();
    let bundle = bundle_dir("ac-a3");
    let task = "t-90143";

    // Live, mid-round, with a token of its own.
    let old_token = CancellationToken::new();
    let old_witness = old_token.child_token();
    let guard = workers::register_active_worker_with_token(
        Some(task.to_string()),
        Agent::Coder.as_str().to_string(),
        "t014e abort mid subagent".to_string(),
        Some(old_token.clone()),
    );
    let key = guard.0.clone();
    let state = workers::worker_state_snapshot(&key).expect("the worker is live");
    assert!(
        state.info.cancel_token.is_some(),
        "AC-A3 fixture: a live worker carries a cancellation token"
    );
    assert!(
        !old_token.is_cancelled(),
        "AC-A3 fixture: the old token must be armed before the abort"
    );

    // Capture while it is still live — the committer's own projection, so the
    // committed byte is the real in-flight shape AC-A1/AC-A3 describe.
    let section = project_worker_section(&key, &state, Agent::Coder, None);
    assert_eq!(
        section.status,
        WorkerStatus::InProgress,
        "AC-A3 fixture: the captured worker must be InProgress"
    );
    assert_eq!(
        section.status_label, "In Progress",
        "AC-A3 fixture: the captured label must be the in-flight label"
    );
    assert_eq!(
        section.completed_seq, None,
        "AC-A3 fixture: an in-flight worker has no completed_seq"
    );

    // The abort itself: the operator cancelled this worker.
    assert!(
        workers::cancel_active_worker(None, Some(task)),
        "AC-A3 fixture: the abort must find the live worker {key}"
    );
    assert!(
        old_token.is_cancelled(),
        "AC-A3: the abort did not cancel the old token"
    );
    assert!(
        old_witness.is_cancelled(),
        "AC-A3: the old token's tree did not go down with it"
    );
    assert_eq!(
        workers::worker_state_snapshot(&key)
            .expect("the entry is still present")
            .info
            .status,
        "Aborted",
        "AC-A3 fixture: the live entry was not marked Aborted"
    );

    // Pre-crash teardown: the entry leaves the live set, so the install seam
    // accepts the key (`install_restored_worker` refuses only a *live* entry).
    drop(guard);
    let floors_before = workers::completed_seq_floor();
    let identity = floors();
    let declared_completed_floor = identity.completed_seq_floor;
    write(
        &bundle,
        "session-t014e-ac-a3",
        &payload(base_ui(), vec![section], identity),
        ProcessEnding::UserAbort,
    );
    let before = fingerprints(&bundle);

    let (session, ending) = restored_session(&bundle);
    assert_eq!(
        ending,
        ProcessEnding::UserAbort,
        "AC-A3: not the abort ending"
    );
    assert_eq!(
        session.workers[0].completed_seq, None,
        "AC-A3: the bundle still says the worker never completed"
    );

    let booted = boot_with_panes(&bundle);
    let live = workers::worker_state_snapshot(&key).expect("AC-A3: the worker is back in WORKERS");
    let token = live
        .info
        .cancel_token
        .clone()
        .expect("AC-A3: a fresh token must be attached to the restored entry");

    // Not the old one — and provably so: the old token is cancelled, this one is
    // not, and the two token trees behave differently.
    assert!(
        old_token.is_cancelled(),
        "AC-A3: the old token must still be cancelled after the restart"
    );
    assert!(
        !token.is_cancelled(),
        "AC-A3: the restored entry carries the old (cancelled) token — cancelling it \
         would do nothing, and the operator would be told otherwise"
    );
    let fresh_child = token.child_token();
    assert!(
        !fresh_child.is_cancelled(),
        "AC-A3: the restored token belongs to a cancelled tree"
    );
    assert!(
        old_witness.is_cancelled(),
        "AC-A3: the old token's tree is no longer cancelled"
    );
    assert!(
        !CancellationToken::new().is_cancelled(),
        "AC-A3 fixture: the cancellation witness itself is broken"
    );

    // Reported as non-cancellable-until-rearmed, in the implementation's own words.
    let notices = &booted.report.repair.notices;
    let cancellation = notice_for(notices, &NonRestorableKind::CancellationToken, &key)
        .unwrap_or_else(|| panic!("AC-A3: no CancellationToken notice for {key}: {notices:#?}"));
    assert!(
        cancellation.detail.contains("did not survive")
            && cancellation.detail.contains("fresh token"),
        "AC-A3: the notice must state that the token did not survive and a fresh one \
         was attached, got: {}",
        cancellation.detail
    );
    let handle = notice_for(notices, &NonRestorableKind::JoinHandle, &key)
        .unwrap_or_else(|| panic!("AC-A3: no JoinHandle notice for {key}: {notices:#?}"));
    assert!(
        handle.detail.contains("no task exists for") && handle.detail.contains(&key),
        "AC-A3 (§10.3 non-cancellable until re-armed): the notice must say no task \
         exists for this key, got: {}",
        handle.detail
    );
    let notice_text = booted
        .report
        .restore_notice
        .clone()
        .expect("AC-A3: the restore notice must exist");
    assert!(
        notice_text.contains(&key) && notice_text.contains("no task exists to continue it"),
        "AC-A3: the operator notice must say the worker is not running: {notice_text}"
    );

    // Nothing re-launched it: no execution, no result, no drawn id, no traffic.
    assert_eq!(
        live.completed_seq, None,
        "AC-A3: the restore drew a completion"
    );
    assert!(
        live.completed_at.is_none() && live.completed_wall.is_none(),
        "AC-A3: the restore stamped a completion time: {:?} / {:?}",
        live.completed_at,
        live.completed_wall
    );
    assert_eq!(
        live.info.status, INTERRUPTED_STATUS_LABEL,
        "AC-A3: the entry is not labelled interrupted — the screen would claim a \
         running subagent"
    );
    // The counter may move for exactly one reason: the bundle's own declared
    // floor is raised into it (`seed_completed_seq_floor` is a `fetch_max`, and
    // this fixture deliberately declares a floor above the live counter). What
    // must never happen is a draw beyond that — a drawn seq means a run finished.
    assert_eq!(
        workers::completed_seq_floor(),
        floors_before.max(declared_completed_floor),
        "AC-A3: the restore path drew a completed_seq, i.e. something finished"
    );
    assert!(
        marmennill::session::registry_get(&key).is_none(),
        "AC-A3: a worker journal was attached to {key}, so a run is mirroring into it"
    );
    assert_eq!(
        notice::worker_inbox_len(&key),
        0,
        "AC-A3: notice traffic started flowing to a worker that is not running"
    );
    assert_bundle_bytes_unchanged(&bundle, &before);

    cleanup(&bundle);
}
