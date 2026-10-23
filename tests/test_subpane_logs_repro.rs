//! CONTRACT / REPRO: subagent **LOG LINES** must survive every authoritative push into the TUI
//! Subagents panel (branch `fix/subagent-panel-refresh`, written against `eaca9ac`).
//!
//! The queue-state half of the original report was fixed by `eaca9ac`
//! (`tests/test_subpane_queued_refresh.rs`). The log half was R1: `set_subagents` adopted the
//! incoming log list unconditionally (`src/ui/tui/mod.rs:1131`) while `thinking`/`content` are
//! merge-guarded right below it (`src/ui/tui/mod.rs:1164-1169`), so every push took back the lines
//! the renderer had appended from live status / tool traffic. `src/ui/tui/mod.rs` now merges
//! (`merge_logs`, monotone adoption + dedup + per-row cap), which is what turns **A** and **B** from
//! repros into pinned tests. **C** (addressing, R4) and **D** (selection, R5) stay red and are
//! `#[ignore]`d as gated follow-ups — see `docs/research/logs_fix_spec.md` §6.
//!
//! Each scenario drives the **real** `TuiRenderer` through its **real** public seams
//! (`set_subagents` + `on_event`) and reads the panel back through the crate's own pane seam,
//! `Renderer::capture_ui()` -> `UiSection::subagent_panels`, which is the 1:1 projection of the very
//! vector `tui::render::render_subagents` draws (see `subagent_panel_from_detail`,
//! `src/ui/tui/mod.rs:1592` copies `logs` verbatim). No sleeps, no terminal, no mock backend: all
//! five are pure and deterministic.
//!
//! * **A — the wipe (GREEN with R1).** A log line the renderer appended itself from a live
//!   `Event::Status` (the production spelling is `"{agent}-{task}: thinking / calling model
//!   ({model})..."`, `src/agents/runner/execution.rs:496-498`) must still be drawable after the very
//!   next authoritative push, which carries only the loop's lifecycle line — all the loop side ever
//!   has.
//! * **B — the one-way sync (GREEN with R1).** A **queued** nested / steering child has no
//!   loop-side log source at all: the registry read model (`orchestrator::WorkerEntry`,
//!   `src/orchestrator/workers.rs:410-465`) carries **no** log field, so the projection opens its row
//!   with `logs: vec![]` (`src/ui/helpers.rs:393-397`) and every queue-rank edge re-pushes that empty
//!   list. The worker really does emit status lines and the renderer really does append them — with
//!   the merge those lines survive every re-projection. Durability *through the registry* (R2) is
//!   still open, which is what the pinned one-way-sync witness below keeps honest.
//! * **C — the addressing gap (`#[ignore]`d, R4).** A duplicate registration takes a `{base}#N`
//!   collision handle (`src/orchestrator/workers.rs:717`), so its row is keyed
//!   `coder-t-903#<seq>`, while the runner's status tag is `coder-t-903`
//!   (`src/agents/runner/execution.rs:255-258`). The renderer's status router matches on the row name
//!   (`src/ui/tui/mod.rs:769-771`), so the line is routed onto the **sibling** row and the
//!   collision-handled row gets none of its own lines.
//! * **D — the drawn row (`#[ignore]`d, R5).** The `[Logs]` block belongs to
//!   `selected_subagent_idx`, and a mid-turn activation re-points it at a row the panel has no logs
//!   for.
//! * **E — gap G4, pinned (GREEN with R1).** The reported scenario end to end: a **queued** specialist
//!   (rank mirrored onto its registry entry) starts producing status lines *while it waits*, then
//!   acquires the slot on the same live entry. Both G4 halves are asserted on every push the drain
//!   seam and the in-sink pump emit: the queue label is not traded for content, and the content is not
//!   traded for the label (`docs/research/subagent_panel_tests.md:260`).

use std::sync::LazyLock;
use std::sync::Mutex;

use tokio_util::sync::CancellationToken;

use marmennill::orchestrator::pool::QueuePosition;
use marmennill::orchestrator::register_active_worker_with_token;
use marmennill::orchestrator::workers::{self, worker_entries};
use marmennill::ui::tui::TuiRenderer;
use marmennill::ui::{Event, QueueState, Renderer, SubagentDetail};

/// The worker registry is process-global, so the two registry-backed scenarios never overlap. A
/// sibling scenario deliberately fails an assertion while holding it, so the lock is taken through
/// poisoning and never propagates a `PoisonError`.
static REGISTRY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn registry_gate() -> std::sync::MutexGuard<'static, ()> {
    REGISTRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The log lines the Subagents panel can draw for one row right now, read through the crate's own
/// pane-capture seam (`capture_ui` -> `subagent_panels`).
fn drawn_logs(tui: &TuiRenderer, name: &str) -> Vec<String> {
    tui.capture_ui()
        .subagent_panels
        .iter()
        .find(|panel| panel.name == name)
        .map(|panel| panel.logs.clone())
        .unwrap_or_else(|| panic!("the panel has no row named {name}"))
}

/// The one row the detail pane actually draws: `render_subagents` reads
/// `self.subagents.get(self.selected_subagent_idx)` (`src/ui/tui/render.rs:793`, and the `[Logs]`
/// block itself is at `:867-886`), and `capture_ui().view.selected_subagent_idx` is that same index.
fn drawn_row(tui: &TuiRenderer) -> (usize, String, Vec<String>) {
    let ui = tui.capture_ui();
    let idx = ui.view.selected_subagent_idx;
    let panel = ui
        .subagent_panels
        .get(idx)
        .unwrap_or_else(|| panic!("no panel row at selection index {idx}"));
    (idx, panel.name.clone(), panel.logs.clone())
}

/// A row exactly as the session loop's projection hands it over for a live registry entry: the
/// identity, liveness and slot state come from `WorkerEntry` (`helpers::apply_registry_entry`,
/// `src/ui/helpers.rs:440-499`), and the **log vector is whatever the entry can supply — which is
/// nothing**, because `WorkerEntry` has no log field, so a freshly projected row is created with
/// `logs: vec![]` (`src/ui/helpers.rs:393-397`).
fn projected_row(
    key: &str,
    is_active: bool,
    queue: Option<QueueState>,
    logs: Vec<String>,
) -> SubagentDetail {
    SubagentDetail {
        name: key.to_string(),
        is_active,
        queue,
        logs,
        ..SubagentDetail::default()
    }
}

/// The production status spelling for one specialist (`execution.rs:255-258` builds the tag,
/// `:496-498` emits the sentence).
fn status_line(tag: &str) -> Event {
    Event::Status(format!("{tag}: thinking / calling model (gpt-6-x)..."))
}

// ---------------------------------------------------------------------------
// A — the renderer's own log line does not survive the next authoritative push
// ---------------------------------------------------------------------------

/// A streamed status line is on the row, and the next `set_subagents` for the **same** worker —
/// carrying only the loop's lifecycle line, which is all the loop side ever has — takes it back off.
#[test]
fn a_status_log_line_the_renderer_appended_is_erased_by_the_next_authoritative_push() {
    let key = "coder-t-901";
    let mut tui = TuiRenderer::new();

    // 1. The loop opens the row with its one lifecycle line.
    tui.set_subagents(vec![projected_row(
        key,
        true,
        None,
        vec!["started task t-901".to_string()],
    )]);
    assert_eq!(
        drawn_logs(&tui, key),
        vec!["started task t-901".to_string()],
        "control: the row starts with only the loop's lifecycle line"
    );

    // 2. Live stream traffic: the renderer routes the status sentence onto that row
    //    (`src/ui/tui/mod.rs:769-784`), so the panel momentarily has two lines.
    tui.on_event(&status_line(key));
    let after_status = drawn_logs(&tui, key);
    assert_eq!(
        after_status.len(),
        2,
        "control: the renderer appends the status line itself, got {after_status:?}"
    );

    // 3. The next authoritative push for the same worker. This is the ordinary cadence at HEAD:
    //    every delegation drain (`session.rs:2073/2245/2713/2816/3095/3309/3395`) and the in-sink
    //    pump every 250 ms (`src/ui/bridge/sink.rs:40-51`) re-push the loop's rows, and the loop's
    //    rows carry lifecycle lines only.
    tui.set_subagents(vec![projected_row(
        key,
        true,
        None,
        vec!["started task t-901".to_string()],
    )]);

    // The user-visible claim: the streamed progress line is still on screen.
    let after_push = drawn_logs(&tui, key);
    assert!(
        after_push
            .iter()
            .any(|line| line.contains("thinking / calling model")),
        "REPRO A (log wipe): the streamed status line must still be drawable after the next \
         authoritative push, but `set_subagents` replaced the row's logs unconditionally \
         (src/ui/tui/mod.rs:1131). after_status = {after_status:?}, after_push = {after_push:?}"
    );
}

// ---------------------------------------------------------------------------
// B — a queued child: the authoritative path has no log channel at all
// ---------------------------------------------------------------------------

/// A queued child whose worker emits status lines ends up with `logs: 0` in what the panel can
/// draw, because every push the loop performs is a registry projection and the registry has no log
/// field to project.
#[test]
fn a_queued_child_with_live_status_traffic_is_handed_to_the_panel_with_no_logs_at_all() {
    let _serial = registry_gate();

    let task = "t-902";
    let guard = register_active_worker_with_token(
        Some(task.to_string()),
        "coder".to_string(),
        "hold the line and wait for a backend slot".to_string(),
        Some(CancellationToken::new()),
    );
    let entry = worker_entries()
        .into_iter()
        .find(|entry| entry.task_id.as_deref() == Some(task))
        .expect("the registration is live in the registry read model");
    let key = entry.key.clone();

    // Witness for the one-way sync: the authoritative row model has no field that could ever carry
    // log text, so nothing downstream can project a log line.
    let authoritative_model = format!("{entry:?}");
    assert!(
        !authoritative_model.contains("logs:") && !authoritative_model.contains("log_lines:"),
        "REPRO B (one-way sync): `WorkerEntry` has no log field to project, yet the registry is \
         the only source the loop's rows come from. entry = {authoritative_model}"
    );

    // The child waits for a backend slot: the pool's rank is mirrored onto the entry, which is what
    // makes the loop re-push this row on every rank edge (fix #2 + fix #3 of `eaca9ac`).
    assert!(
        workers::set_worker_queue_position(
            "coder",
            Some(task),
            Some(QueuePosition { position: 1, of: 1 }),
        ),
        "the queue rank must land on the live entry"
    );

    let mut tui = TuiRenderer::new();

    // Push #1 — the projection opens the row. Fresh rows are created with `logs: vec![]`
    // (`src/ui/helpers.rs:393-397`), and the queue label is mirrored from the entry.
    let rank = worker_entries()
        .into_iter()
        .find(|e| e.key == key)
        .expect("the entry is still live")
        .queue_position
        .expect("the rank was mirrored onto the entry");
    let loop_queue = Some(QueueState {
        position: rank.position,
        of: rank.of,
    });
    tui.set_subagents(vec![projected_row(&key, true, loop_queue, vec![])]);
    // Control: the queue half of `eaca9ac` really does work — the rank is mirrored onto the registry
    // entry the projection reads, and the row the panel receives carries it (`render.rs:717` builds
    // the `(Queued n/m)` label from `sa.queue.filter(|_| sa.is_active)`). The log half does not.
    let mirrored = worker_entries()
        .into_iter()
        .find(|e| e.key == key)
        .expect("the entry is still live")
        .queue_position;
    assert_eq!(
        mirrored.map(|q| (q.position, q.of)),
        Some((1usize, 1usize)),
        "the queue half of the fix works: the rank reaches the row"
    );
    assert_eq!(
        drawn_logs(&tui, &key),
        Vec::<String>::new(),
        "control: a projected row starts with no log lines"
    );

    // The worker is live and talking: two status sentences plus a tool line, all addressed with the
    // tag the runner spells (`execution.rs:255-258`). The renderer appends them itself.
    tui.on_event(&status_line(&key));
    tui.on_event(&Event::Status(format!(
        "{key}: calling backend (gpt-6-x) for {task}"
    )));
    tui.on_event(&Event::ToolCall("run_command(cargo test)".to_string()));
    let live = drawn_logs(&tui, &key);
    assert!(
        live.len() >= 2,
        "control: the renderer really does append the child's live traffic, got {live:?}"
    );

    // Now the queue-rank edges the commit added fire: each one re-projects and re-pushes the loop's
    // rows, and every one of those rows carries `logs: vec![]`.
    let mut pushed_with_logs = 0usize;
    let mut draws_after_last_push: Vec<String> = Vec::new();
    for rank_n in 2..=4 {
        let mut rows = vec![];
        // A faithful projection: identity + rank from the registry, nothing else.
        let fresh = worker_entries()
            .into_iter()
            .find(|e| e.key == key)
            .expect("still live");
        rows.push(projected_row(
            &fresh.key,
            fresh.is_active,
            fresh.queue_position.map(|q| QueueState {
                position: q.position,
                of: q.of,
            }),
            // what `helpers::apply_registry_entry` can supply for this field: nothing.
            Vec::new(),
        ));
        let _ = rank_n;
        tui.set_subagents(rows);
        let after = drawn_logs(&tui, &key);
        if !after.is_empty() {
            pushed_with_logs += 1;
        }
        draws_after_last_push = after;
    }

    assert_eq!(
        pushed_with_logs, 3,
        "REPRO B (logs: 0): every push after the child started talking must still let the panel \
         draw its log lines; instead all three re-projections left the row empty. logs after the \
         last push = {draws_after_last_push:?}, live lines seen before the pushes = {live:?}"
    );
    assert!(
        draws_after_last_push.len() >= 2,
        "REPRO B (logs: 0): the queued child's log lines are gone from what the panel can draw. \
         final drawn logs = {draws_after_last_push:?}"
    );

    drop(guard);
}

// ---------------------------------------------------------------------------
// C — a collision-handled key cannot be addressed by the status router at all
// ---------------------------------------------------------------------------

/// Two live registrations of one identity are two workers: the second takes a `{base}#N` key
/// (`src/orchestrator/workers.rs:717`), so its row is keyed with the handle — while every status
/// sentence about it is spelled without the handle (`src/agents/runner/execution.rs:255-258`).
/// The renderer's status router matches on the row name and `break`s on the first hit
/// (`src/ui/tui/mod.rs:769-793`), so the line lands on the **sibling** row and the
/// collision-handled row never gets a single log line, with or without a push. The fallback writer
/// cannot save it either: it needs `active_agent` to name that row, and `set_subagents` only ever
/// sets `active_agent` away from `"Manager"` when it was already away from it
/// (`src/ui/tui/mod.rs:1185-1203`).
#[test]
#[ignore = "deferred follow-up #1 (R4): the status router cannot address a `{base}#N` / `-wN` row key; needs the runner tag spelled with `WorkerEntry::key` and the first-hit `break` at src/ui/tui/mod.rs:792 removed (docs/research/logs_fix_spec.md §6.1)"]
fn a_status_line_never_reaches_a_row_keyed_with_a_collision_handle() {
    let _serial = registry_gate();

    let task = "t-903";
    let first = register_active_worker_with_token(
        Some(task.to_string()),
        "coder".to_string(),
        "first child".to_string(),
        Some(CancellationToken::new()),
    );
    let second = register_active_worker_with_token(
        Some(task.to_string()),
        "coder".to_string(),
        "second child".to_string(),
        Some(CancellationToken::new()),
    );
    let keys: Vec<String> = worker_entries()
        .into_iter()
        .filter(|entry| entry.task_id.as_deref() == Some(task))
        .map(|entry| entry.key)
        .collect();
    let disambiguated = keys
        .iter()
        .find(|key| key.contains('#'))
        .cloned()
        .unwrap_or_else(|| {
            panic!("the duplicate registration must take a `#...` handle, keys: {keys:?}")
        });
    let plain = keys
        .iter()
        .find(|key| !key.contains('#'))
        .cloned()
        .unwrap_or_else(|| panic!("the first registration keeps the plain key, keys: {keys:?}"));
    let tag = plain.clone(); // `execution.rs:255-258` spells `{agent}-{task}`, never with the handle
    let _ = (&first, &second);

    let mut tui = TuiRenderer::new();
    // A projection hands over both live rows, plain key first (registry order).
    tui.set_subagents(vec![
        projected_row(&plain, true, None, vec![format!("started task {task}")]),
        projected_row(
            &disambiguated,
            true,
            None,
            vec![format!("started task {task}")],
        ),
    ]);

    // Both children are talking; both sentences carry the runner's tag.
    tui.on_event(&status_line(&tag));
    tui.on_event(&Event::Status(format!(
        "{tag}: calling backend (gpt-6-x) for {task}"
    )));

    // Control: an unhandled row with the same tag is addressable, so the sentences themselves are
    // routable — the failure is specific to the handled key.
    let sibling = drawn_logs(&tui, &plain);
    assert!(
        sibling
            .iter()
            .any(|line| line.contains("thinking / calling model")),
        "control: the router addresses the plain-keyed row, got {sibling:?}"
    );

    // The claim: the second worker's own row must carry its own progress.
    let logs = drawn_logs(&tui, &disambiguated);
    assert!(
        logs.iter()
            .any(|line| line.contains("thinking / calling model")),
        "REPRO C (addressing): the child's status lines are spelled `{tag}: ...` while its row is \
         keyed `{disambiguated}`, and the router matches the row name and breaks on the first hit \
         (src/ui/tui/mod.rs:769-793), so the line went to the sibling row {plain:?}. drawn logs on \
         the handled row = {logs:?}, drawn logs on the sibling = {sibling:?}"
    );
}

// ---------------------------------------------------------------------------
// D — the row the panel actually draws is the one the new push just selected
// ---------------------------------------------------------------------------

/// The `[Logs]` block belongs to the **selected** row
/// (`self.subagents.get(self.selected_subagent_idx)`, `src/ui/tui/render.rs:793`, block at
/// `:867-886`), and `set_subagents` re-points that index at any row it sees flip to active
/// (`activated_name`, `src/ui/tui/mod.rs:1127-1129` + `:1214-1218`). Because `eaca9ac` makes queued
/// siblings appear — and re-appear on every rank edge — mid-turn, the row whose logs the operator was
/// reading stops being the drawn row at the same moment its own lines are replaced. The panel is then
/// left with **no** log line to draw at all.
#[test]
#[ignore = "deferred follow-up #3 (R5): the detail pane follows `selected_subagent_idx`, and a mid-turn activation still steals it (src/ui/tui/mod.rs:1214-1218 vs src/ui/tui/events.rs:693-695); needs a pinned selection policy covering both writers (docs/research/logs_fix_spec.md §6.3)"]
fn a_new_midturn_row_moves_the_selection_onto_a_row_the_panel_has_no_logs_for() {
    let talker = "coder-t-904";
    let latecomer = "tester-t-904";
    let mut tui = TuiRenderer::new();

    // The operator is watching a running child with two log lines.
    tui.set_subagents(vec![projected_row(
        talker,
        true,
        None,
        vec![format!("started task {talker}")],
    )]);
    tui.on_event(&status_line(talker));
    let (idx, name, logs) = drawn_row(&tui);
    assert_eq!(
        (idx, name.as_str(), logs.len()),
        (0, talker, 2),
        "control: the detail pane draws the running child and it has two log lines"
    );

    // Mid-turn, the projection hands over a second live row (a sibling that just got a queue rank)
    // and re-pushes the running child with the loop's lifecycle list.
    tui.set_subagents(vec![
        projected_row(talker, true, None, vec![format!("started task {talker}")]),
        projected_row(
            latecomer,
            true,
            Some(QueueState { position: 1, of: 2 }),
            Vec::new(),
        ),
    ]);

    let (idx, name, logs) = drawn_row(&tui);
    assert!(
        !logs.is_empty(),
        "REPRO D (selection + wipe): the detail pane must keep drawing log lines for the delegation \
         it was showing; the push moved the selection to the newly active row and replaced the \
         running row's logs. drawn row = index {idx}, name {name:?}, logs = {logs:?}"
    );
}

// ---------------------------------------------------------------------------
// E — G4: a queued specialist that starts producing lines keeps them through
//     every authoritative push (delegation drain + mid-turn pump)
// ---------------------------------------------------------------------------

/// Gap **G4** (`docs/research/subagent_panel_tests.md:260`) end to end: a specialist is **queued**
/// waiting for a concurrency slot, the panel shows it as queued, and *then* it starts producing log
/// lines — while the pool still reports it waiting. It later acquires the slot: the same live registry
/// entry, one row, no re-registration ("then starts"). From that point on the loop keeps pushing the
/// row, at every delegation drain (`src/ui/session.rs::project_then_drain_delegation_events`) and at
/// the in-sink pump cadence (`src/ui/bridge/sink.rs:40-51`), and every one of those pushes carries
/// only the loop's lifecycle lines.
///
/// Both halves of the G4 contract are asserted on **every** push:
/// * **(a) the label is never traded for content** — the row the seam hands over still satisfies the
///   queue-marker draw predicate (`render.rs:717`, mirrored below) while the specialist waits;
/// * **(b) the content is never traded for the label** — what the panel can draw
///   (`capture_ui().subagent_panels[].logs`, the 1:1 input of `render_subagents`) still holds the
///   child's streamed lines, and the list never shrinks, because live log text has exactly one
///   writer (this renderer) while the loop side can only ever supply lifecycle lines.
///
/// Registry-backed and therefore serialized against **B** by `REGISTRY_LOCK`; fully deterministic, no
/// sleeps, no pool round-trip, no terminal.
#[test]
fn a_queued_specialist_that_starts_emitting_lines_keeps_them_through_every_authoritative_push() {
    let _serial = registry_gate();

    let task = "t-906";
    let guard = register_active_worker_with_token(
        Some(task.to_string()),
        "coder".to_string(),
        "queued for a backend slot, then streaming".to_string(),
        Some(CancellationToken::new()),
    );
    let key = worker_entries()
        .into_iter()
        .find(|entry| entry.task_id.as_deref() == Some(task))
        .expect("the registration is live in the registry read model")
        .key
        .clone();

    // The pool's rank is mirrored onto the entry — exactly the writer that makes the loop re-project
    // and re-push this row on every rank edge (`eaca9ac`), i.e. the pushes under test here.
    assert!(
        workers::set_worker_queue_position(
            "coder",
            Some(task),
            Some(QueuePosition { position: 1, of: 1 }),
        ),
        "the queue rank must land on the live entry"
    );

    // The queue marker's draw condition, verbatim from `render.rs:717`
    // (`let queued = sa.queue.filter(|_| sa.is_active);`), asserted on the row the seam hands over —
    // `capture_ui()`'s panel model carries no queue field.
    let draws_queued = |row: &SubagentDetail| row.is_active && row.queue.is_some();

    // A faithful session-loop projection of the live entry: identity, liveness and slot state come
    // from the registry read model (`helpers::apply_registry_entry`), and the log vector is what the
    // loop itself owns — the lifecycle lines of `helpers::update_subagent_lifecycle` (`:627`/`:629`).
    let project = |loop_logs: &[String]| -> SubagentDetail {
        let entry = worker_entries()
            .into_iter()
            .find(|entry| entry.key == key)
            .expect("the entry is still live in the registry read model");
        projected_row(
            &entry.key,
            entry.is_active,
            entry.queue_position.map(|q| QueueState {
                position: q.position,
                of: q.of,
            }),
            loop_logs.to_vec(),
        )
    };

    let mut tui = TuiRenderer::new();

    // Push #1 — the drain seam opens the row for the **queued** child with its one lifecycle line.
    let mut loop_logs = vec![format!("started task {task}")];
    let row = project(&loop_logs);
    assert!(
        draws_queued(&row),
        "control: the waiting specialist's row satisfies the queue-label predicate before it talks"
    );
    tui.set_subagents(vec![row]);
    assert_eq!(
        drawn_logs(&tui, &key),
        loop_logs,
        "control: the row opens with only the loop's lifecycle line"
    );

    // The child produces activity **while it still waits for the slot**: the renderer routes both
    // production spellings onto the row itself (`src/ui/tui/mod.rs:818-845`).
    tui.on_event(&status_line(&key));
    tui.on_event(&Event::Status(format!(
        "{key}: calling backend (gpt-6-x) for {task}"
    )));
    let live = drawn_logs(&tui, &key);
    assert_eq!(
        live.len(),
        3,
        "control: the renderer appended the queued child's two live sentences, got {live:?}"
    );
    let streamed = &live[1..];

    // Every authoritative push below is checked the same way: the streamed lines are still there,
    // the list never shrinks, and each lifecycle line the loop re-pushes appears exactly once.
    let check = |stage: &str, after: &[String], lifecycle: &[String]| {
        for line in streamed {
            assert!(
                after.iter().any(|drawn| drawn == line),
                "GAP G4 (log lines survive the {stage} push): the line the queued specialist \
                 produced is gone from what the Subagents panel can draw. the loop side carried \
                 only {lifecycle:?} and `set_subagents` took the row's logs from it. panel draws \
                 {after:?}, streamed lines that must survive = {streamed:?}"
            );
        }
        assert!(
            after.len() >= streamed.len(),
            "GAP G4 (no shrink on the {stage} push): the panel's log list for the row got shorter \
             than the {expected} lines it already had — live log text has exactly one writer, so a \
             push may only ever add. loop side carried {lifecycle:?}, panel draws {after:?}",
            expected = streamed.len(),
        );
        for line in lifecycle {
            let hits = after.iter().filter(|drawn| *drawn == line).count();
            assert_eq!(
                hits, 1,
                "GAP G4 (no duplication across the {stage} push): the loop re-pushes its lifecycle \
                 line and the panel shows it {hits} times. panel = {after:?}"
            );
        }
    };

    // Rank edge: the pool re-ranks the waiting specialist, so the drain seam re-projects and re-pushes
    // the same row (fix #2/#3 of `eaca9ac`).
    assert!(
        workers::set_worker_queue_position(
            "coder",
            Some(task),
            Some(QueuePosition { position: 2, of: 2 }),
        ),
        "the re-rank must land on the same live entry"
    );
    let row = project(&loop_logs);
    assert!(
        draws_queued(&row),
        "GAP G4 (a): the queue label must not be traded for content — the row the drain seam pushes \
         for the still-waiting specialist must still satisfy `is_active && queue.is_some()` \
         (render.rs:717), got queue = {:?}",
        row.queue
    );
    tui.set_subagents(vec![row]);
    let after_rank_edge = drawn_logs(&tui, &key);
    check("rank-edge", &after_rank_edge, &loop_logs);
    assert!(
        after_rank_edge.len() > loop_logs.len(),
        "GAP G4 (b): after the queued child produced its first lines the pushed row must carry more \
         than the loop's own lifecycle content. loop = {loop_logs:?}, panel = {after_rank_edge:?}"
    );

    // A second drain for the same delegation re-pushes the identical row.
    tui.set_subagents(vec![project(&loop_logs)]);
    let after_second_drain = drawn_logs(&tui, &key);
    check("second-drain", &after_second_drain, &loop_logs);
    assert_eq!(
        after_second_drain, after_rank_edge,
        "GAP G4 (idempotent drain): re-pushing the same row must not change what the panel can draw"
    );

    // Then the specialist **starts**: the pool takes its rank back down and the same live entry is
    // active with a slot, so the next projection carries `queue: None` for the row already on screen.
    assert!(
        workers::set_worker_queue_position("coder", Some(task), None),
        "taking the rank back down must land on the same live entry"
    );
    let row = project(&loop_logs);
    assert!(
        row.is_active && row.queue.is_none(),
        "control: the specialist now holds a slot, so its row is active with no queue label, got \
         is_active = {}, queue = {:?}",
        row.is_active,
        row.queue
    );
    tui.set_subagents(vec![row]);
    let after_start = drawn_logs(&tui, &key);
    check("slot-acquisition", &after_start, &loop_logs);
    assert!(
        after_start.len() > loop_logs.len(),
        "GAP G4 (b): the log content must not be traded for the queue label — the row that just \
         became active must still carry the lines produced while it waited. loop = {loop_logs:?}, \
         panel = {after_start:?}"
    );

    // The mid-turn pump (`src/ui/bridge/sink.rs:40-51`) pushes on its own cadence while the Manager
    // turn is open, with the same loop-side content.
    tui.set_subagents(vec![project(&loop_logs)]);
    let after_pump = drawn_logs(&tui, &key);
    check("in-sink-pump", &after_pump, &loop_logs);

    // Completion edge: the loop appends its second lifecycle line and hands the row over again.
    loop_logs.push(format!("completed task {task}"));
    tui.set_subagents(vec![project(&loop_logs)]);
    let after_done = drawn_logs(&tui, &key);
    check("completion", &after_done, &loop_logs);
    assert_eq!(
        after_done.len(),
        loop_logs.len() + streamed.len(),
        "GAP G4 (exact merge): the drawn log list must be the loop's lifecycle lines plus the \
         child's streamed lines, each exactly once. loop = {loop_logs:?}, streamed = {streamed:?}, \
         panel = {after_done:?}"
    );

    // The row the detail pane actually draws is that specialist, with its logs intact.
    let (idx, name, logs) = drawn_row(&tui);
    assert_eq!(
        (idx, name.as_str(), logs.as_slice()),
        (0usize, key.as_str(), after_done.as_slice()),
        "GAP G4 (drawn row): the `[Logs]` block belongs to the selected row, and that row must be \
         the specialist with its merged log lines"
    );

    drop(guard);
}
