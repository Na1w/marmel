//! Whether a resumed session comes back showing **the same screen**: pane bytes,
//! composer text and view state.
//!
//! * **pane bytes are the authority** — `renderer.messages` equals
//!   `committed_ui.chat_messages` element-by-element, asserted *before the first
//!   draw* (embedded `\n`, empty and duplicate entries included); the status bar
//!   is the committed string and not `"Ready"`; no pane line is derived from the
//!   journal; `extract_steering_history()` still yields the committed pairs; a
//!   `[CLI] …` line only the TUI spelled survives though the journal has none.
//! * **composer and view come back** — composer text + caret (multi-line,
//!   multi-byte), `history` / `history_index` / `input_draft`, every scroll offset
//!   and toggle (or a clamp difference named in `warnings`), and a `ui.json`
//!   refusal that leaves every bundle byte exactly where it was.
//!
//! `TuiRenderer`'s pane fields are `pub(crate)`, so the pane model is read back
//! through the crate's own public mirror [`Renderer::capture_ui`] — the seam
//! [`Renderer::restore_panes`] was written to mirror, a pure field transfer both
//! ways. The renderer under test is a **real `TuiRenderer`** wrapped in
//! [`PaneWitness`]: it snapshots the pane model inside `restore_panes` and counts
//! flushes, so `flushes == 0` pins the instant before the first draw.

use std::path::{Path, PathBuf};

use marmennill::config::Config;
use marmennill::manager::phase::{MARMEL_DIR, Plan};
use marmennill::session::commit::{
    CapturePayload, CommitLock, CommitPolicy, fingerprint, write_bundle,
};
use marmennill::session::format::{MANIFEST_FILE, TimePoint, UI_FILE};
use marmennill::session::restore::load_bundle;
use marmennill::session::{
    BudgetWitness, ComposerState, DanglingToolCallState, IdentityFloors, ManagerSection,
    MessageRecord, PinWitness, ProcessEnding, RefusalCode, RestoreOutcome, SubagentPanel,
    UiJournalRecord, UiSection, ViewState,
};
use marmennill::ui::session::{BootReport, BootSink, boot, build_restored_context};
use marmennill::ui::tui::TuiRenderer;
use marmennill::ui::{Event, InputState, Renderer, SubagentDetail, UiRecord, UiTranscript};

/// The budget the fixture commits with, and the budget this process boots with.
/// They differ on purpose: the resulting report warning is what proves a loud
/// `[CLI] Restore warning:` line is an *appended report line* and never a
/// re-derivation of the committed pane bytes.
const COMMITTED_BUDGET: usize = 1234;
const LIVE_BUDGET: usize = 4321;

// ------------------------------------------------------------------ temp bundle

/// A fresh temp dir whose `.marmel/session` layout mirrors a real plan. The tag is
/// unique per test, so cases never share a directory.
fn bundle_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "marmel-t014d-{tag}-{}-{}",
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

// ------------------------------------------------------------- manager fixture

/// The pinned pair plus one settled turn. `[0]`/`[1]` are the two pins the loader
/// insists on; this file is about the panes.
fn manager_transcript() -> Vec<MessageRecord> {
    vec![
        MessageRecord::System {
            content: "t014d pinned system prompt: never re-composed on this path".to_string(),
        },
        MessageRecord::User {
            content: "t014d pinned goal: the pane bytes win for the screen".to_string(),
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

fn payload(ui: UiSection) -> CapturePayload {
    CapturePayload {
        manager: manager_section(),
        ui: Some(ui),
        workers: vec![],
        identity_floors: IdentityFloors {
            worker_id_seq_floor: 1,
            completed_seq_floor: 1,
            notice_id_floor: 1,
        },
    }
}

/// The public commit seam, unthrottled. The committer does NOT validate the UI
/// section, which is what lets a later test commit a section the loader must then
/// refuse.
fn write(bundle: &Path, session_id: &str, ui: UiSection) {
    let policy = CommitPolicy {
        min_interval: std::time::Duration::from_millis(0),
        max_bundle_bytes: 32 * 1024 * 1024,
        allow_throttled: true,
    };
    let report = write_bundle(
        bundle,
        session_id,
        &payload(ui),
        ProcessEnding::InProgress,
        &policy,
        &CommitLock::new(),
    )
    .expect("the fixture bundle must commit");
    assert!(
        !report.members_written.is_empty(),
        "the fixture bundle must be a real commit point"
    );
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

/// The committed `ui.json` read back through the loader: the authority every
/// comparison is made against, so no test compares the boot pass with a fixture it
/// merely built in memory.
fn committed_section(bundle: &Path) -> UiSection {
    let outcome = load_bundle(bundle);
    let RestoreOutcome::Restored { session, .. } = &outcome else {
        panic!(
            "expected a full restore of {}, got {}",
            bundle.display(),
            explain(&outcome)
        );
    };
    session.ui.clone()
}

// ------------------------------------------------------ byte-level witnesses

fn member_bytes(bundle: &Path, rel: &str) -> Vec<u8> {
    let path = bundle.join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn member_paths(bundle: &Path) -> Vec<String> {
    let text = String::from_utf8(member_bytes(bundle, MANIFEST_FILE)).expect("manifest is UTF-8");
    let manifest: serde_json::Value = serde_json::from_str(&text).expect("manifest parses");
    let members = manifest
        .get("members")
        .and_then(|m| m.as_array())
        .expect("members is an array");
    let mut paths: Vec<String> = members
        .iter()
        .filter_map(|m| m.get("rel_path").and_then(|p| p.as_str()))
        .map(str::to_string)
        .collect();
    paths.push(MANIFEST_FILE.to_string());
    paths.sort();
    paths
}

/// `(rel_path, fingerprint)` for every member plus `manifest.json`, hashed with
/// the crate's own hasher — never a second one.
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

// ------------------------------------------------------------ the pane witness

/// A **real** `TuiRenderer` behind a recording `Renderer`, so the boot pass runs
/// its own hook order and the test can read the pane model at the exact instant
/// that matters: after `restore_panes` assigned, before anything was drawn.
struct PaneWitness {
    tui: TuiRenderer,
    /// Captured inside `restore_panes`, immediately after delegating the assign.
    after_restore: Option<UiSection>,
    /// The `flush` count at the moment of that snapshot — the "before the first
    /// draw" witness.
    draws_at_snapshot: usize,
    flushes: usize,
    events: Vec<Event>,
}

impl Default for PaneWitness {
    fn default() -> Self {
        PaneWitness {
            tui: TuiRenderer::new(),
            after_restore: None,
            draws_at_snapshot: 0,
            flushes: 0,
            events: Vec::new(),
        }
    }
}

impl PaneWitness {
    /// The live pane model as the crate's own capture seam reports it.
    fn live(&self) -> UiSection {
        self.tui.capture_ui()
    }

    /// The pane model at the instant after the assign and before any draw.
    fn snapshot(&self) -> &UiSection {
        self.after_restore
            .as_ref()
            .expect("`restore_panes` must have been called by the boot pass")
    }

    /// The status sentences the pass sent to the renderer (loud, never silent).
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
    transcript: UiTranscript,
    renderer: PaneWitness,
    session_id: String,
}

/// THE boot path (`ui::session::boot`) driven end to end with a real TUI renderer:
/// `load_bundle` → floors + `WORKERS` → the engine → the panes → the loud report →
/// the manager notice.
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
        transcript,
        renderer,
        session_id,
    }
}

// --------------------------------------------------------------- UI fixtures

/// The status bar the captured screen was showing. Not `"Ready"`, and free of the
/// two spellings `set_subagents` resets to `"Ready"`, so a later `"Ready"` is a
/// finding, not noise.
fn committed_status_line() -> String {
    "Running (t014d-model) — 2 task(s) in flight".to_string()
}

/// The pane vector under test, holding every awkward shape: an entry with embedded
/// `\n`, an empty entry, a whitespace-only entry, an adjacent duplicate pair, and a
/// repeat of the first entry at the end.
fn pane_lines_awkward() -> Vec<String> {
    vec![
        "User: keep the pane bytes, never a replay".to_string(),
        "Marmennill: I will read the design doc,\nthen write the acceptance tests,\nthen run cargo test.".to_string(),
        String::new(),
        "[Tool Call] read_file(path: \"docs/design_session_restore_data.md\")".to_string(),
        "[Tool Call] read_file(path: \"docs/design_session_restore_data.md\")".to_string(),
        "[CLI] Available commands: /help /thought /abort".to_string(),
        "[CLI] /thought — toggles the thinking pane".to_string(),
        "   ".to_string(),
        "[Tool Result] 1435: **AC-U1** …".to_string(),
        "User: keep the pane bytes, never a replay".to_string(),
    ]
}

/// The journal mirror of that screen — a *different* set of strings from the pane
/// (that is the point), holding no `[CLI]` record at all.
fn journal_awkward() -> Vec<UiJournalRecord> {
    vec![
        UiJournalRecord::User {
            text: "keep the pane bytes, never a replay".to_string(),
        },
        UiJournalRecord::Assistant {
            content: Some("I will read the design doc.".to_string()),
            thinking: Some("check §6.6 first".to_string()),
        },
        UiJournalRecord::ToolCall {
            display: "read_file(path: \"docs/design_session_restore_data.md\")".to_string(),
        },
        UiJournalRecord::ToolResult {
            display: "1435: **AC-U1** …".to_string(),
        },
    ]
}

/// The text a journal record carries, for the "no pane line came from the journal"
/// assertions. `ToolCall`/`ToolResult` carry their `display`, the rest their
/// `text`; `TaskCompleted`/`TaskFailed` carry only an id.
fn journal_text(record: &UiJournalRecord) -> Option<&str> {
    match record {
        UiJournalRecord::User { text }
        | UiJournalRecord::SteerResponse { text }
        | UiJournalRecord::Status { text } => Some(text),
        UiJournalRecord::ToolCall { display } | UiJournalRecord::ToolResult { display } => {
            Some(display)
        }
        UiJournalRecord::Assistant { content, .. } => content.as_deref(),
        UiJournalRecord::TaskCompleted { .. } | UiJournalRecord::TaskFailed { .. } => None,
    }
}

/// The section the pane comparisons commit. Every token witness is `0`, matching a
/// fresh process, so `restore_panes` has nothing to say out loud about counters and
/// the pane vector stays exactly as committed.
fn section_for_panes(pane: Vec<String>, journal: Vec<UiJournalRecord>) -> UiSection {
    UiSection {
        chat_messages: pane,
        status_line: committed_status_line(),
        subagent_panels: vec![],
        journal,
        composer: ComposerState::default(),
        view: ViewState::default(),
        budgets: BudgetWitness::default(),
        plan_content: "Execution plan\n- [x] t-014a loader\n- [ ] t-014d pane tests".to_string(),
        had_active_plan: true,
        active_plan_task: Some("t-014d".to_string()),
        plan_start: Some(TimePoint::now()),
        plan_completed: None,
        current_thought: "half-streamed thinking, still on screen".to_string(),
        current_content: "half-streamed answer, still on screen".to_string(),
        active_agent: Some("Manager".to_string()),
        // Empty on purpose: a non-empty half-sentence is folded into the pane by
        // the next `Status` event, which would mix an unrelated line into the
        // "only report lines were appended" witness.
        steer_sentence_buffer: Some(String::new()),
        tokens_in: 0,
        tokens_out: 0,
        orchestrator_context_tokens: 0,
        abort_armed: false,
        abort_armed_at: None,
    }
}

/// `count` single-line pane entries, so a restored scroll offset sits inside what
/// the live clamp allows instead of being pulled back by it.
fn pane_lines_tall(count: usize) -> Vec<String> {
    (1..=count)
        .map(|i| format!("[Tool Result] pane line {i}"))
        .collect()
}

/// Four inactive panels: `is_active: false` keeps `set_subagents` from flipping
/// `show_subagent_panel` or re-selecting, so what the test reads is what the
/// restore assigned. Panel 2 carries long content so its details-pane offset is
/// inside the clamp too.
fn panels_four() -> Vec<SubagentPanel> {
    (0..4)
        .map(|i| SubagentPanel {
            name: format!("t014dspecialist{i}"),
            task_id: Some(format!("t-014d-{i}")),
            prompt: format!("brief for panel {i}"),
            logs: (0..6)
                .map(|k| format!("log line {k} of panel {i}"))
                .collect(),
            thinking: format!("thinking of panel {i}"),
            content: if i == 2 {
                (0..30)
                    .map(|k| format!("content line {k} of the selected panel"))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                format!("content of panel {i}")
            },
            is_active: false,
            context_tokens: 40 + i,
            worked_duration_secs: 12.5 + i as f64,
            started_at: Some(TimePoint::now()),
            last_activity_at: Some(TimePoint::now()),
            turn_thinking: Some(format!("turn thinking of panel {i}")),
            is_thinking: Some(false),
        })
        .collect()
}

/// Does the live journal hold this committed record, byte for byte? The two enums
/// are the same closed set of eight variants, so this is a rename.
fn transcript_holds(transcript: &UiTranscript, record: &UiJournalRecord) -> bool {
    let mirror = match record {
        UiJournalRecord::User { text } => UiRecord::User { text: text.clone() },
        UiJournalRecord::Assistant { content, thinking } => UiRecord::Assistant {
            content: content.clone(),
            thinking: thinking.clone(),
        },
        UiJournalRecord::SteerResponse { text } => UiRecord::SteerResponse { text: text.clone() },
        UiJournalRecord::ToolCall { display } => UiRecord::ToolCall {
            display: display.clone(),
        },
        UiJournalRecord::ToolResult { display } => UiRecord::ToolResult {
            display: display.clone(),
        },
        UiJournalRecord::TaskCompleted { task_id } => UiRecord::TaskCompleted {
            task_id: task_id.clone(),
        },
        UiJournalRecord::TaskFailed { task_id, reason } => UiRecord::TaskFailed {
            task_id: task_id.clone(),
            reason: reason.clone(),
        },
        UiJournalRecord::Status { text } => UiRecord::Status { text: text.clone() },
    };
    transcript.records().iter().any(|held| held == &mirror)
}

// ==========================================================================
// The pane bytes are the authority
// ==========================================================================

/// `renderer.messages == committed_ui.chat_messages`, element-by-element,
/// asserted **before the first draw**.
///
/// The snapshot is taken inside `Renderer::restore_panes` on the real boot path
/// (see [`PaneWitness`]): the assign has happened, and no `flush` has ever been
/// requested. The whole vector is compared, then every element again by index so a
/// failure names the entry, then the awkward shapes individually — a `Vec<String>`
/// "same length" pass could hide all of them.
#[test]
fn ac_u1_pane_vector_equals_the_committed_vector_element_by_element_before_the_first_draw() {
    let bundle = bundle_dir("ac-u1");
    write(
        &bundle,
        "session-t014d-ac-u1",
        section_for_panes(pane_lines_awkward(), journal_awkward()),
    );
    let committed = committed_section(&bundle);
    assert_eq!(
        committed.chat_messages,
        pane_lines_awkward(),
        "the fixture must survive the commit round-trip, or AC-U1 proves nothing"
    );

    let booted = boot_with_panes(&bundle);
    let snapshot = booted.renderer.snapshot();

    // ---------- the assertion, before any draw ----------
    assert_eq!(
        booted.renderer.draws_at_snapshot, 0,
        "AC-U1/I-U2: the comparison must precede the first draw, and {} flush(es) had \
         already been requested when the pane model was snapshotted",
        booted.renderer.draws_at_snapshot
    );
    assert_eq!(
        snapshot.chat_messages, committed.chat_messages,
        "AC-U1: renderer.messages != committed_ui.chat_messages element-by-element"
    );
    assert_eq!(
        snapshot.chat_messages.len(),
        committed.chat_messages.len(),
        "AC-U1: the restored pane holds a different number of entries"
    );
    for (index, (restored, stored)) in snapshot
        .chat_messages
        .iter()
        .zip(committed.chat_messages.iter())
        .enumerate()
    {
        assert_eq!(
            restored.as_bytes(),
            stored.as_bytes(),
            "AC-U1: pane entry {index} is not the committed bytes"
        );
    }

    // The awkward shapes, pinned individually.
    assert_eq!(
        snapshot.chat_messages[1],
        "Marmennill: I will read the design doc,\nthen write the acceptance tests,\nthen run cargo test.",
        "AC-U1: an entry with embedded newlines must come back with its newlines, \
         not split into several entries"
    );
    assert_eq!(
        snapshot.chat_messages[1].lines().count(),
        3,
        "AC-U1: the embedded newlines stayed inside ONE entry (three display lines, \
         one entry)"
    );
    assert_eq!(
        snapshot.chat_messages[2], "",
        "AC-U1: the empty pane entry must survive as an empty entry, not be dropped"
    );
    assert_eq!(
        snapshot.chat_messages[3], snapshot.chat_messages[4],
        "AC-U1: the duplicate pair must stay two entries in the same order"
    );
    assert_eq!(
        snapshot.chat_messages[0], snapshot.chat_messages[9],
        "AC-U1: a repeat of an earlier entry keeps its own position"
    );
    assert_eq!(
        snapshot.chat_messages[7], "   ",
        "AC-U1: a whitespace-only entry is an entry, not nothing"
    );

    // Still true after the whole pass: the committed vector is a strict prefix of
    // the live pane, and everything behind it is a report line the loader spelled
    // — never a journal or model line.
    let live = booted.renderer.live();
    assert!(
        live.chat_messages.starts_with(&committed.chat_messages),
        "AC-U1: after the whole boot pass the pane no longer opens with the committed \
         vector element-by-element: {:#?}",
        live.chat_messages
    );
    let appended = &live.chat_messages[committed.chat_messages.len()..];
    assert!(
        !appended.is_empty() && appended.iter().all(|line| line.starts_with("[CLI] ")),
        "AC-U1: every line appended after the committed vector must be a spelled \
         report line ('[CLI] …'), got {appended:#?}"
    );
    assert!(
        booted.report.repair.warnings.iter().any(|warning| {
            live.chat_messages
                .iter()
                .any(|pane| pane == &format!("[CLI] Restore warning: {warning}"))
        }),
        "AC-U1: the loud report (D6) must reach the panes without disturbing the \
         committed bytes; {} warning(s) were reported",
        booted.report.repair.warnings.len()
    );

    cleanup(&bundle);
}

/// `renderer.status_line == committed_ui.status_line`, and the bar is **not**
/// `"Ready"`.
///
/// The default is spelled first so this cannot be a tautology: a fresh
/// `TuiRenderer` really does open on `"Ready"`, so if the restore had skipped the
/// field the equality below would fail against a real default.
#[test]
fn ac_u2_status_line_is_the_committed_bar_and_never_the_default_ready() {
    let bundle = bundle_dir("ac-u2");
    write(
        &bundle,
        "session-t014d-ac-u2",
        section_for_panes(pane_lines_awkward(), journal_awkward()),
    );
    let committed = committed_section(&bundle);
    assert_ne!(
        committed.status_line, "Ready",
        "the fixture's committed bar must not be the default"
    );

    // What this process would show had the restore not assigned the field.
    let untouched = TuiRenderer::new().capture_ui();
    assert_eq!(
        untouched.status_line, "Ready",
        "a fresh TUI bar is 'Ready' (tui/mod.rs:188): the AC-U2 comparison is \
         therefore against a real default, not a coincidence"
    );

    let booted = boot_with_panes(&bundle);
    let snapshot = booted.renderer.snapshot();
    assert_eq!(
        booted.renderer.draws_at_snapshot, 0,
        "AC-U2 is asserted before the first draw"
    );
    assert_eq!(
        snapshot.status_line, committed.status_line,
        "AC-U2: renderer.status_line != committed_ui.status_line"
    );
    assert_eq!(
        snapshot.status_line.as_bytes(),
        committed.status_line.as_bytes(),
        "AC-U2: the status bar must be byte-identical, not merely similar"
    );
    assert_ne!(
        snapshot.status_line, "Ready",
        "AC-U2: the status bar reset to the default 'Ready' after restore"
    );

    // Nothing later in the pass may drop the bar back to the default: `boot` ends
    // with one `Event::Status` summary, and `set_subagents` must not have
    // clobbered the restored text.
    let live = booted.renderer.live();
    assert_ne!(
        live.status_line, "Ready",
        "after the full boot pass the bar fell back to 'Ready' instead of carrying \
         restored/live status text"
    );
    assert!(
        !live.status_line.is_empty(),
        "AC-U2: the status bar is never blank after a restore"
    );

    cleanup(&bundle);
}

/// No pane line is produced from the journal.
///
/// The fixture makes the two authorities disagree on purpose: the journal says the
/// operator asked for the FIRST wording, the pane says `"User: … SECOND wording"`.
/// Both must come back unchanged, the pane must contain neither the journal's text
/// nor the journal text dressed in the pane's `"User: "` prefix, and the journal
/// half must still be live: the re-seeded `UiTranscript` yields the committed
/// steering pairs through `extract_steering_history()`.
#[test]
fn ac_u3_no_pane_line_is_derived_from_the_journal_and_the_steering_pairs_survive() {
    let pane = vec![
        "User: the pane says the operator asked for the SECOND wording".to_string(),
        "[CLI] local note the journal never sees".to_string(),
        "Marmennill: answered for the pane, not for the journal".to_string(),
    ];
    let journal = vec![
        UiJournalRecord::User {
            text: "the journal says the operator asked for the FIRST wording".to_string(),
        },
        UiJournalRecord::SteerResponse {
            text: "[Arbitrator]: the arbitrator answered the first wording".to_string(),
        },
        UiJournalRecord::User {
            text: "second steering question exactly as the journal kept it".to_string(),
        },
        UiJournalRecord::SteerResponse {
            text: "second arbitrator answer exactly as the journal kept it".to_string(),
        },
    ];

    let bundle = bundle_dir("ac-u3");
    write(
        &bundle,
        "session-t014d-ac-u3",
        section_for_panes(pane.clone(), journal.clone()),
    );
    let committed = committed_section(&bundle);
    assert_eq!(committed.chat_messages, pane, "fixture round-trip");
    assert_eq!(committed.journal, journal, "fixture round-trip");
    // The fixture really does disagree with itself — that is what turns this into
    // a derivation test instead of a copy test.
    assert_ne!(
        committed.chat_messages[0],
        format!(
            "User: {}",
            journal_text(&committed.journal[0]).expect("the journal User record has text")
        ),
        "AC-U3 fixture: the pane must not accidentally match the derivation it is \
         meant to forbid"
    );

    let booted = boot_with_panes(&bundle);
    let snapshot = booted.renderer.snapshot();

    // (a) the pane came back as the pane, unchanged.
    assert_eq!(
        snapshot.chat_messages, committed.chat_messages,
        "AC-U3: the pane text was not restored unchanged"
    );

    // (b) no pane line is derived from the journal: neither the bare journal text
    // nor the journal text wearing the pane's `"User: "` prefix.
    for record in &committed.journal {
        let Some(text) = journal_text(record) else {
            continue;
        };
        assert!(
            !snapshot.chat_messages.iter().any(|line| line == text),
            "AC-U3: a pane entry is a journal record's text verbatim: {text:?}"
        );
        assert!(
            !snapshot
                .chat_messages
                .iter()
                .any(|line| line == &format!("User: {text}")),
            "AC-U3: a pane entry was built from a journal record as \
             'User: {{text}}': {text:?}"
        );
    }

    // (c) the journal came back too, as the journal — steering half intact.
    assert_eq!(
        booted.report.journal, committed.journal,
        "AC-U3: BootReport::journal must carry the committed journal verbatim \
         (it is the steering mirror, never a pane source)"
    );
    for record in &committed.journal {
        assert!(
            transcript_holds(&booted.transcript, record),
            "AC-U3/I-U4: the live UiTranscript lost the committed journal record {record:?}"
        );
    }

    // (d) `extract_steering_history()` still returns the committed pairs. The
    // `[Arbitrator]:` prefix is the journal's own rendering, which the extractor
    // strips; everything else is byte-for-byte.
    let expected = vec![
        (
            "the journal says the operator asked for the FIRST wording".to_string(),
            "the arbitrator answered the first wording".to_string(),
        ),
        (
            "second steering question exactly as the journal kept it".to_string(),
            "second arbitrator answer exactly as the journal kept it".to_string(),
        ),
    ];
    assert_eq!(
        booted.transcript.extract_steering_history(),
        expected,
        "AC-U3/I-U4: extract_steering_history() must return the committed pairs, in \
         order, after the restore"
    );

    // (e) the two authorities still disagree afterwards: neither was quietly
    // reconciled to the other.
    let live = booted.renderer.live();
    assert_eq!(
        live.chat_messages[..committed.chat_messages.len()],
        committed.chat_messages,
        "AC-U3: the pane authority was rewritten from the journal during the pass"
    );
    assert_eq!(
        booted.report.journal, committed.journal,
        "AC-U3: the journal authority was rewritten from the pane during the pass"
    );
    assert_eq!(
        booted.session_id, "session-t014d-ac-u3",
        "AC-U3: the session the journal belongs to must be the committed one"
    );

    cleanup(&bundle);
}

/// A `"[CLI] …"` line the TUI produced by itself (`/help`, `/thought`) is present
/// after the restore although the journal has no record of it: a journal replay
/// could not have shown it.
#[test]
fn ac_u4_cli_only_lines_survive_restore_although_the_journal_never_recorded_them() {
    let pane = vec![
        "User: /help".to_string(),
        "[CLI] Available commands: /help /thought /abort /resume".to_string(),
        "[CLI] /thought — toggles the thinking pane".to_string(),
        "User: /thought".to_string(),
        "[CLI] thinking pane shown".to_string(),
    ];
    // The journal of the same screen records the two user commands and nothing
    // else: no `[CLI]` record exists anywhere in it.
    let journal = vec![
        UiJournalRecord::User {
            text: "/help".to_string(),
        },
        UiJournalRecord::User {
            text: "/thought".to_string(),
        },
    ];

    let bundle = bundle_dir("ac-u4");
    write(
        &bundle,
        "session-t014d-ac-u4",
        section_for_panes(pane.clone(), journal.clone()),
    );
    let committed = committed_section(&bundle);
    let cli_lines: Vec<&String> = committed
        .chat_messages
        .iter()
        .filter(|line| line.starts_with("[CLI] "))
        .collect();
    assert_eq!(
        cli_lines.len(),
        3,
        "the fixture screen holds three TUI-local '[CLI]' lines"
    );
    assert!(
        committed
            .journal
            .iter()
            .all(|record| !journal_text(record).is_some_and(|text| text.contains("[CLI]"))),
        "AC-U4 fixture: the journal must hold no record of the '[CLI]' lines"
    );

    let booted = boot_with_panes(&bundle);
    let snapshot = booted.renderer.snapshot();

    // Present after the restore, exactly as the screen had them.
    for line in &cli_lines {
        assert!(
            snapshot.chat_messages.iter().any(|pane| pane == *line),
            "AC-U4: the TUI-local line {line:?} is gone after restore — the pane \
             model was rebuilt from something other than the stored bytes"
        );
    }
    assert_eq!(
        snapshot.chat_messages, committed.chat_messages,
        "AC-U4: the whole pane vector, local lines included, must be assigned verbatim"
    );

    // And the journal really has nothing to derive them from — in the durable
    // section, in the boot report, and in the live journal.
    assert!(
        booted
            .report
            .journal
            .iter()
            .all(|record| !journal_text(record).is_some_and(|text| text.contains("[CLI]"))),
        "AC-U4: the boot report's journal gained a '[CLI]' record the bundle never held"
    );
    assert!(
        booted
            .transcript
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                UiRecord::User { .. } | UiRecord::SteerResponse { .. }
            ))
            .all(|record| !format!("{record:?}").contains("[CLI] ")),
        "AC-U4: the re-seeded journal is not the committed journal: a user/steer \
         record carrying a '[CLI]' line appeared"
    );
    // The two commands the journal DID keep are still the journal's own bytes,
    // not the pane's decorated ones.
    assert!(
        transcript_holds(
            &booted.transcript,
            &UiJournalRecord::User {
                text: "/help".to_string()
            }
        ),
        "AC-U4: the journal half must survive alongside the pane half"
    );

    cleanup(&bundle);
}

// ==========================================================================
// Composer, caret, scroll, selection
// ==========================================================================

/// The composer under test: three lines, a 2-byte `é` and a 4-byte `☕`, and a
/// caret sitting mid-document on a char boundary the restore has to land on.
fn composer_multiline() -> ComposerState {
    let input_text =
        "fix the parser café\nthen run cargo test ☕\nthird line, not sent yet".to_string();
    // The byte offset of the third line's first character: mid-document, and
    // guaranteed a char boundary because it was found, not counted by hand.
    let cursor = input_text
        .find("third")
        .expect("the fixture text holds its third line");
    assert!(
        cursor > 0 && cursor < input_text.len(),
        "caret is mid-document"
    );
    assert!(input_text.is_char_boundary(cursor));
    ComposerState {
        input_text,
        cursor,
        input_draft: String::new(),
        history: vec![],
        history_index: None,
    }
}

/// `renderer.input_text == committed.composer.input_text`, multi-line included,
/// and `renderer.cursor == committed.composer.cursor`: the caret lands on the same
/// char boundary.
#[test]
fn ac_c1_composer_text_and_caret_come_back_on_the_same_char_boundary() {
    let bundle = bundle_dir("ac-c1");
    let mut section = section_for_panes(vec!["User: half-typed, never sent".to_string()], vec![]);
    section.composer = composer_multiline();
    write(&bundle, "session-t014d-ac-c1", section);

    let committed = committed_section(&bundle);
    assert_eq!(
        committed.composer.history,
        Vec::<String>::new(),
        "AC-C1 fixture: text and caret only, the history half is AC-C2's"
    );
    assert!(
        committed.composer.input_text.lines().count() >= 3,
        "AC-C1 requires a multi-line composer"
    );
    assert!(
        !committed.composer.input_text.is_ascii(),
        "AC-C1 requires multi-byte text, so a byte-counted caret bug is visible"
    );
    assert!(
        committed.composer.cursor > 0
            && committed.composer.cursor < committed.composer.input_text.len(),
        "AC-C1 requires a mid-document caret"
    );

    let booted = boot_with_panes(&bundle);
    let snapshot = booted.renderer.snapshot();

    assert_eq!(
        snapshot.composer.input_text, committed.composer.input_text,
        "AC-C1: renderer.input_text != committed composer.input_text (multi-line)"
    );
    assert_eq!(
        snapshot.composer.input_text.as_bytes(),
        committed.composer.input_text.as_bytes(),
        "AC-C1: the draft must be byte-identical, embedded newlines included"
    );
    assert_eq!(
        snapshot.composer.input_text.lines().count(),
        committed.composer.input_text.lines().count(),
        "AC-C1: the line structure of the draft changed"
    );
    assert_eq!(
        snapshot.composer.cursor, committed.composer.cursor,
        "AC-C1: renderer.cursor != committed composer.cursor"
    );
    assert!(
        snapshot
            .composer
            .input_text
            .is_char_boundary(snapshot.composer.cursor),
        "AC-C1/U-chk: the caret landed off a char boundary (byte {} of {})",
        snapshot.composer.cursor,
        snapshot.composer.input_text.len()
    );
    // The caret is on the same character, not merely the same offset of a string
    // that happened to be re-encoded: the four bytes behind it are the emoji and
    // its newline, the character in front of it is the third line's `t`.
    let caret = snapshot.composer.cursor;
    assert!(
        snapshot.composer.input_text[..caret].ends_with("☕\n"),
        "AC-C1: the bytes behind the caret are {:?}, i.e. the caret moved by bytes \
         rather than by characters",
        snapshot.composer.input_text[caret.saturating_sub(4)..caret].to_string()
    );
    assert_eq!(
        snapshot.composer.input_text[caret..].chars().next(),
        Some('t'),
        "AC-C1: the caret landed on a different character than the committed one"
    );

    cleanup(&bundle);
}

/// `history`, `history_index` and `input_draft` match.
#[test]
fn ac_c2_composer_history_history_index_and_input_draft_match() {
    let bundle = bundle_dir("ac-c2");
    let mut section = section_for_panes(vec!["User: sent earlier".to_string()], vec![]);
    section.composer = ComposerState {
        input_text: "the draft being typed now".to_string(),
        cursor: 5,
        // The draft saved when history navigation began — newline included.
        input_draft: "half-typed draft\nsecond line of the saved draft".to_string(),
        history: vec![
            "cargo test".to_string(),
            "cargo test".to_string(),
            String::new(),
            "une ligne en français — avec accents".to_string(),
            "fix the plan\nthen continue".to_string(),
        ],
        history_index: Some(1),
    };
    write(&bundle, "session-t014d-ac-c2", section);
    let committed = committed_section(&bundle);
    assert_eq!(
        committed.composer.history_index,
        Some(1),
        "AC-C2 fixture: a navigating index inside history (U-chk-clean)"
    );

    let booted = boot_with_panes(&bundle);
    let snapshot = booted.renderer.snapshot();

    assert_eq!(
        snapshot.composer.history, committed.composer.history,
        "AC-C2: composer history changed (order, duplicates, the empty entry or the \
         multi-line entry)"
    );
    assert_eq!(
        snapshot.composer.history.len(),
        5,
        "AC-C2: the duplicate and the empty history entries must survive as entries"
    );
    assert_eq!(
        snapshot.composer.history[0], snapshot.composer.history[1],
        "AC-C2: a duplicated history entry must not be collapsed"
    );
    assert_eq!(
        snapshot.composer.history[2], "",
        "AC-C2: an empty history entry must not be dropped"
    );
    assert_eq!(
        snapshot.composer.history[4], "fix the plan\nthen continue",
        "AC-C2: a multi-line history entry keeps its newline"
    );
    assert_eq!(
        snapshot.composer.history_index, committed.composer.history_index,
        "AC-C2: history_index changed — Some/None is a real distinction \
         (navigating versus not)"
    );
    assert_eq!(
        snapshot.composer.input_draft, committed.composer.input_draft,
        "AC-C2: input_draft, the draft saved when navigation began, changed"
    );
    assert_eq!(
        snapshot.composer.input_text, committed.composer.input_text,
        "AC-C2: the text being typed now changed while the history was restored"
    );

    cleanup(&bundle);
}

/// The matching half: every restored view value is identical.
///
/// `chat_scroll`, `subagent_scroll`, `subagent_list_scroll` and
/// `selected_subagent_idx` are chosen inside what the live clamp allows on a fresh
/// renderer. `plan_scroll` is `0` here for one honest reason: that clamp's upper
/// bound, `plan_max_scroll`, is derived per frame and is therefore `0` in a
/// process that has not drawn — so a stored non-zero plan offset is exactly the
/// clamp-difference case, asserted in
/// [`ac_c3_scroll_offsets_that_the_live_clamp_moved_are_reported_as_warnings`].
#[test]
fn ac_c3_view_offsets_selection_and_toggles_match_the_committed_view() {
    let bundle = bundle_dir("ac-c3");
    let mut section = section_for_panes(pane_lines_tall(40), vec![]);
    section.subagent_panels = panels_four();
    section.view = ViewState {
        chat_scroll: 20,
        plan_scroll: 0,
        subagent_scroll: 6,
        subagent_list_scroll: 2,
        selected_subagent_idx: 2,
        focused_panel: "subagents".to_string(),
        show_thought: true,
        show_subagent_panel: true,
        show_plan_panel: true,
        chat_auto_scroll: false,
        subagent_autoscroll: true,
        plan_auto_scroll: false,
    };
    assert_eq!(
        section.structural_violations(),
        Vec::<String>::new(),
        "the fixture must be U-chk-clean, or a refusal would not be a finding"
    );
    write(&bundle, "session-t014d-ac-c3", section);
    let committed = committed_section(&bundle);
    assert_eq!(committed.subagent_panels.len(), 4, "fixture panels");
    assert_eq!(committed.chat_messages.len(), 40, "fixture panes");

    let booted = boot_with_panes(&bundle);
    let snapshot = booted.renderer.snapshot();
    let got = &snapshot.view;
    let want = &committed.view;

    // Field by field, so a failure names the field.
    for (field, stored, restored) in [
        (
            "chat_scroll",
            u64::from(want.chat_scroll),
            u64::from(got.chat_scroll),
        ),
        (
            "plan_scroll",
            u64::from(want.plan_scroll),
            u64::from(got.plan_scroll),
        ),
        (
            "subagent_scroll",
            u64::from(want.subagent_scroll),
            u64::from(got.subagent_scroll),
        ),
        (
            "subagent_list_scroll",
            u64::from(want.subagent_list_scroll),
            u64::from(got.subagent_list_scroll),
        ),
        (
            "selected_subagent_idx",
            want.selected_subagent_idx as u64,
            got.selected_subagent_idx as u64,
        ),
    ] {
        assert_eq!(
            restored, stored,
            "AC-C3/I-U6: {field} is {restored}, the bundle holds {stored}, and no \
             warnings entry can excuse it because the restore reports none for it"
        );
    }
    assert_eq!(
        got.focused_panel, want.focused_panel,
        "AC-C3: focused_panel changed (a value outside the legal set is a U-chk \
         refusal, never a silent default)"
    );
    assert_eq!(got.show_thought, want.show_thought, "AC-C3: show_thought");
    assert_eq!(
        got.show_subagent_panel, want.show_subagent_panel,
        "AC-C3: show_subagent_panel"
    );
    assert_eq!(
        got.show_plan_panel, want.show_plan_panel,
        "AC-C3: show_plan_panel"
    );
    assert_eq!(
        got.chat_auto_scroll, want.chat_auto_scroll,
        "AC-C3: chat_auto_scroll"
    );
    assert_eq!(
        got.subagent_autoscroll, want.subagent_autoscroll,
        "AC-C3: subagent_autoscroll"
    );
    assert_eq!(
        got.plan_auto_scroll, want.plan_auto_scroll,
        "AC-C3: plan_auto_scroll"
    );
    assert_eq!(got, want, "AC-C3: the whole ViewState must match");

    // The selection is not just a number: it addresses the panel the bundle
    // stored, so the details pane points at the same subagent.
    assert_eq!(
        snapshot
            .subagent_panels
            .get(got.selected_subagent_idx)
            .map(|panel| panel.name.as_str()),
        committed
            .subagent_panels
            .get(want.selected_subagent_idx)
            .map(|panel| panel.name.as_str()),
        "AC-C3: selected_subagent_idx must address the same panel it addressed \
         before the crash"
    );
    // And nothing was reported as a clamp difference in a fixture where every
    // offset matched.
    assert!(
        !booted
            .report
            .repair
            .warnings
            .iter()
            .any(|line| line.contains("scroll")),
        "AC-C3: the restore reported a scroll difference in a fixture where every \
         offset matched: {:#?}",
        booted.report.repair.warnings
    );

    cleanup(&bundle);
}

/// The second branch, kept honest: an offset the live clamp moved is only legal
/// when a `warnings` entry names the offset **and both values**.
///
/// The clamp does run — `clamp_scrolls()` is the last thing `restore_panes` does,
/// so these offsets really do move. The sentence naming both values comes from
/// `clamp_warning_sentences`, folded into `RepairReport::warnings` at the boot
/// call site before `announce_restore`, with its wording pinned by the lib unit
/// test `a_scroll_offset_the_live_clamp_moved_is_named_with_both_values`.
#[test]
fn ac_c3_scroll_offsets_that_the_live_clamp_moved_are_reported_as_warnings() {
    let bundle = bundle_dir("ac-c3-clamp");
    let mut section = section_for_panes(pane_lines_tall(4), vec![]);
    section.subagent_panels = panels_four();
    section.view = ViewState {
        chat_scroll: 9_000,
        plan_scroll: 12,
        subagent_scroll: 9_000,
        subagent_list_scroll: 40,
        selected_subagent_idx: 2,
        focused_panel: "subagents".to_string(),
        show_thought: true,
        show_subagent_panel: true,
        show_plan_panel: true,
        chat_auto_scroll: false,
        subagent_autoscroll: false,
        plan_auto_scroll: false,
    };
    assert_eq!(
        section.structural_violations(),
        Vec::<String>::new(),
        "a far-out scroll offset is NOT a U-chk violation: §9.6 routes it to the \
         live clamp, and the clamp is required to say so"
    );
    write(&bundle, "session-t014d-ac-c3-clamp", section);
    let committed = committed_section(&bundle);

    let booted = boot_with_panes(&bundle);
    let snapshot = booted.renderer.snapshot();

    let mut offenders = Vec::new();
    for (name, stored, live) in [
        (
            "chat_scroll",
            committed.view.chat_scroll,
            snapshot.view.chat_scroll,
        ),
        (
            "plan_scroll",
            committed.view.plan_scroll,
            snapshot.view.plan_scroll,
        ),
        (
            "subagent_scroll",
            committed.view.subagent_scroll,
            snapshot.view.subagent_scroll,
        ),
        (
            "subagent_list_scroll",
            committed.view.subagent_list_scroll,
            snapshot.view.subagent_list_scroll,
        ),
    ] {
        if stored == live {
            continue;
        }
        let warning = booted.report.repair.warnings.iter().find(|line| {
            line.contains(name)
                && line.contains(&stored.to_string())
                && line.contains(&live.to_string())
        });
        match warning {
            Some(line) => println!("AC-C3: {name} clamped {stored} -> {live}: {line}"),
            None => offenders.push(format!("{name}: stored {stored}, live {live}")),
        }
    }
    assert!(
        offenders.is_empty(),
        "AC-C3/I-U6: these restored offsets were moved by the live clamp without a \
         warnings entry naming the offset and both values: {}",
        offenders.join("; ")
    );

    cleanup(&bundle);
}

// -------------------------------------------------- a bad ui.json is refused

/// Every composer or selection violation is a refusal of the member `ui.json`
/// with code `headerManifestDisagreement`, and the file bytes are untouched. These
/// are preconditions, not repairs: the loader never "fixes" a caret or a
/// selection, installs nothing into the live renderer, and rewrites nothing.
#[test]
fn ac_c4_out_of_range_caret_selection_or_unknown_panel_refuses_and_leaves_bytes_untouched() {
    // The caret cases break the rule in the two ways it is spelled: past the
    // end of `input_text`, and off a char boundary.
    let text = "café ☕ note".to_string();
    let inside_e = text
        .char_indices()
        .find(|(_, c)| *c == 'é')
        .map(|(index, _)| index)
        .expect("the fixture text holds an 'é'");
    type TamperCase<'a> = (&'a str, &'a str, Box<dyn Fn(&mut UiSection) + 'a>);
    let cases: Vec<TamperCase<'_>> = vec![
        (
            "cursor past input_text.len()",
            "composer.cursor",
            Box::new(|section: &mut UiSection| {
                section.composer.input_text = text.clone();
                section.composer.cursor = text.len() + 7;
            }),
        ),
        (
            "cursor off a char boundary",
            "char boundary",
            Box::new(|section: &mut UiSection| {
                section.composer.input_text = text.clone();
                section.composer.cursor = inside_e + 1;
            }),
        ),
        (
            "selected_subagent_idx out of range",
            "selected_subagent_idx",
            Box::new(|section: &mut UiSection| {
                section.subagent_panels = panels_four();
                section.view.selected_subagent_idx = 5;
            }),
        ),
        (
            "unknown focused_panel",
            "focused_panel",
            Box::new(|section: &mut UiSection| {
                section.view.focused_panel = "main".to_string();
            }),
        ),
    ];

    for (label, field, tamper) in cases {
        let bundle = bundle_dir(&format!("ac-c4-{}", field.replace('.', "-")));
        let mut section = section_for_panes(pane_lines_awkward(), journal_awkward());
        tamper(&mut section);
        let violations = section.structural_violations();
        assert!(
            !violations.is_empty() && violations.iter().any(|line| line.contains(field)),
            "AC-C4 fixture ({label}): structural_violations() must name \
             {field:?}, got {violations:?}"
        );
        write(&bundle, "session-t014d-ac-c4", section);
        let before = fingerprints(&bundle);

        // The refusal comes from the real boot path, not from a helper.
        let cfg = Config {
            max_context_tokens: LIVE_BUDGET,
            ..Config::default()
        };
        let plan = Plan::at(root_of(&bundle).join(MARMEL_DIR));
        let mut engine = build_restored_context(&cfg);
        let mut renderer = PaneWitness::default();
        let mut transcript = UiTranscript::new();
        let mut session_id = String::new();
        let mut sink = BootSink {
            ui_transcript: &mut transcript,
        };
        let error = boot(
            &plan,
            &cfg,
            &mut engine,
            &mut renderer,
            &mut session_id,
            &mut sink,
        )
        .unwrap_err();

        assert_eq!(
            error.refusal.code,
            RefusalCode::HeaderManifestDisagreement,
            "AC-C4 ({label}): a U-chk violation must be refused with \
             headerManifestDisagreement, got: {}",
            error.refusal.message
        );
        assert_eq!(
            error.refusal.member.as_deref(),
            Some(UI_FILE),
            "AC-C4 ({label}): the refusal must be scoped to {UI_FILE}, got {:?}: {}",
            error.refusal.member,
            error.refusal.message
        );
        assert!(
            error.refusal.message.contains(field),
            "AC-C4 ({label}): the refusal must quote the offending field \
             ({field:?}), got {:?}",
            error.refusal.message
        );
        assert_eq!(
            error.bundle, bundle,
            "AC-C4 ({label}): the refusal must name the bundle it refused"
        );

        // Fail-closed: nothing of the refused section was installed. (The refusal
        // itself is still announced loudly, so "the pane is empty" is not the
        // witness — the committed bytes being absent is.)
        let live = renderer.live();
        assert!(
            !live
                .chat_messages
                .iter()
                .any(|line| line == &pane_lines_awkward()[0]),
            "AC-C4 ({label}): a refused restore installed pane bytes anyway: {:#?}",
            live.chat_messages
        );
        assert_ne!(
            live.status_line,
            committed_status_line(),
            "AC-C4 ({label}): a refused restore assigned the committed status line"
        );
        assert_ne!(
            live.plan_content,
            section_for_panes(vec![], vec![]).plan_content,
            "AC-C4 ({label}): a refused restore assigned the committed plan pane"
        );
        assert_eq!(
            session_id, "",
            "AC-C4 ({label}): a refused restore adopted a session id"
        );

        // The file bytes are where they were, hashed with the crate's own hasher
        // before and after.
        assert_bundle_bytes_unchanged(&bundle, &before);
        cleanup(&bundle);
    }
}
