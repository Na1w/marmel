# t-031 — strict static audit of the UI-side edits (part 2 of 2)

Note: zero edits and zero build commands were performed — everything below comes only from `grep_search`
and targeted `read_file` windows (`cargo`, `clippy`, `rustfmt`, `git` were never run; process spawning is
dead in this session).

## Scope and headline

* **No compile-breaking (hard) error was found** in any of the three audited areas. Every hard-error
  hypothesis the task asked me to test was falsified with evidence:
  * A1 `clamp_warning_sentences` exists and is reachable: `src/ui/tui/mod.rs:1527`
    `pub(crate) fn clamp_warning_sentences(moved: &[(&str, u64, u64)]) -> Vec<String>`, a **free function at
    module scope in `crate::ui::tui`** (between `fn duration_from_secs` at `src/ui/tui/mod.rs:1510` and
    `impl TuiRenderer {` at `src/ui/tui/mod.rs:434` … the impl spans to past 1620), declared before
    `pub(crate) fn` bodies at 1489/1510/1581. `src/ui/mod.rs:8` is `pub mod tui;`, so
    `crate::ui::tui::clamp_warning_sentences` is visible from `src/ui/session.rs:1571`.
  * A2 no leftover reference to the removed inherent `TuiRenderer::scroll_clamp_warnings`: crate-wide grep for
    `scroll_clamp_warnings` yields exactly three hits — the free `fn scroll_clamp_warnings(stored: &UiSection,
    live: &UiSection) -> Vec<String>` at `src/ui/session.rs:1570` (defined **and** called at
    `src/ui/session.rs:1287`, so not dead), and a prose mention in the doc comment at
    `src/ui/tui/mod.rs:1523-1525`. No method call `renderer.scroll_clamp_warnings` /
    `TuiRenderer::scroll_clamp_warnings` survives → no `unused`/`dead_code` violation under
    `Cargo.toml:67-70` (`unused = "deny"`, `dead_code = "deny"`, `unused_imports = "deny"`) and
    `Cargo.toml:71-73` (`all = deny`).
  * A3 the element type matches the producer: `src/ui/session.rs:1571-1596` builds an array of
    `(&'static str, u64, u64)` from `stored.view.*`/`live.view.*` using `u64::from(...)` for
    `chat_scroll`/`plan_scroll`/`subagent_scroll`/`subagent_list_scroll` (all `u16`,
    `src/session/format/ui.rs:262-265`) and `as u64` for `selected_subagent_idx` (`usize`,
    `src/session/format/ui.rs:266`). Same shape in the unit-test copy at `src/ui/tui/tests.rs:2998-3018`.
    `&[(&str,u64,u64); 5]` → `&[(&str,u64,u64)]` unsize coercion is fine and the `&'static str` field names
    outlive the call.
  * A4 the new unit test references only existing items: `src/ui/tui/tests.rs:2917-2957`
    (`#[test] fn a_scroll_offset_the_live_clamp_moved_is_named_with_both_values()`), fixture writes to
    `TuiRenderer` fields `chat_scroll/plan_scroll/subagent_scroll/subagent_list_scroll`
    (`src/ui/tui/mod.rs:72,74,87,89`, all `pub(crate) u16`) and `selected_subagent_idx`
    (`src/ui/tui/mod.rs:85`, `pub(crate) usize`); `ViewState` literals use real fields and `chat_scroll: 9_000`
    fits `u16`; helper `fn clamp_report(...)` (`src/ui/tui/tests.rs:2998`) is called at `:2941`, `:2947`,
    `:2963`, `:2974` → not dead. `src/ui/tui/tests.rs:1` is `use super::*;`, so the `pub(crate)` fn is in
    scope. Expected-count check: `src/ui/` currently contains **191 `#[test]` + 5 `#[tokio::test]` = 196 test
    functions and 0 `#[ignore]`**, i.e. consistent with 195 → 196 (baseline itself is not verifiable, §3).
  * A5 no now-unused import in `src/ui/tui/mod.rs`: every top-level import (lines 12-30) is still referenced
    (`Event` e.g. `:459,462,656`, `Renderer` `:434`, `SubagentDetail` `:83,276`, `Config` `:256`,
    `Duration` `:240,377,406,410`, `Rect` `:137-140,231-234`, `Receiver/Sender/channel` `:127,128,174`,
    `Arc` `:1639`, `io` `:362,438,442,978,1628`, `Terminal/CrosstermBackend` `:130`, `OrchestratorManager`
    `:1639`). `UiSection`/`ViewState` are used fully qualified (`:1085,1091,1126,1196,1583`), so removing the
    old method could not orphan an import.
  * B1 order in `pub fn boot` (`src/ui/session.rs:1076`), section 3 of the pass:
    `:1234 let live_ui = renderer.capture_ui();` → `:1235 renderer.restore_panes(&session.ui);` →
    `:1237-1243` panel projection → `:1244 renderer.set_subagents(panels);` → `:1256-1274` panel
    `JoinHandle` notices → `:1286 let restored_ui = renderer.capture_ui();` →
    `:1287-1290` `for warning in scroll_clamp_warnings(&session.ui, &restored_ui) { tracing::warn!(…);
    push_repair_warning(&mut repair, warning); }` → `:1291 renderer.announce_restore(&repair);` →
    `:1298-1301` token witnesses folded **after** the announcement (documented deviation). The required
    relative order (`set_subagents → capture_ui → warn+push_repair_warning → announce_restore`) holds; see §2
    for the two extra steps.
  * B2/B3 types and fields: `push_repair_notice(report: &mut RepairReport, notice: NonRestorableNotice)` at
    `src/ui/session.rs:1462` and `push_repair_warning(report: &mut RepairReport, warning: String)` at
    `:1470` are private free fns in the **same module** as the call sites, hence visible. The literal at
    `src/ui/session.rs:1262-1264` uses `kind`, `subject`, `detail` — exactly the three fields of
    `NonRestorableNotice` (`src/session/format/restore.rs:188-197`); `NonRestorableKind::JoinHandle` exists
    (`src/session/format/restore.rs:201`). `session.ui.subagent_panels` is spelled correctly
    (`src/session/format/ui.rs` → `UiSection` field; used at `src/ui/session.rs:1240,1256,1372` and
    `BootReport::subagent_panels` at `:1045`), and the element type `SubagentPanel` has `name: String`
    (`src/session/format/ui.rs:169-170`) and `is_active: bool` (`:177`).
  * B4 `capture_ui()` returns `UiSection` by value (`src/ui/mod.rs:152` trait default; TUI impl
    `src/ui/tui/mod.rs:1085`); both results are consumed by reference against `&UiSection` parameters
    (`src/ui/session.rs:1570` and `:1542`), so `&restored_ui`/`&live_ui` match; neither binding is unused
    (`:1299`, `:1287`).
  * B5 dedup is real: `push_repair_notice` (`src/ui/session.rs:1462-1468`) compares `notice.dedup_key()`, and
    `NonRestorableNotice::dedup_key()` returns `(self.kind.clone(), self.subject.clone())`
    (`src/session/format/restore.rs:225-229`). `src/session/worker_journal.rs:843-847` emits
    `JoinHandle` with `subject: key.clone()` (worker key); the new panel notices use `subject: panel.name`.
    No swallowing in practice because the two subject spaces differ (§2 first bullet).
  * B6 no new unused variable/import in the region: `live_ui` (`:1234`) read at `:1299`, `restored_ui`
    (`:1286`) read at `:1287`, `panels` (`:1237`) moved into `:1244`, `panel`/`warning` read in their loops;
    `NonRestorableKind`/`NonRestorableNotice` are imported at `src/ui/session.rs:19` and used at
    `:1178,1190,1262`.
  * C imports: every symbol in `tests/test_session_restore_panel_abort.rs:85-111` has at least one use site
    (`MARMEL_DIR` `:157,881`; `Plan` `:881`; `notice`/`workers` `:534-536,2225,2435`; `fingerprint` `:678`;
    `worker_rel_path` `:1993`; `write_bundle` `:568`; `TimePoint` `:181`; `MANIFEST_FILE` `:668,694`;
    `UI_FILE` `:1347,1351`; `load_bundle` `:616,629`; `anchor_wall` `:184,1614`;
    `project_worker_section` `:2450,2504`; `queued_notice_notices` `:2305`; `CancellationToken` `:2439,2483,2591`;
    `boot` `:894`; `build_restored_context` `:887`; `BootSink` `:891`; `BootReport` `:868`; `TuiRenderer`
    `:771,785`; `Event` `:777,809,820`; `InputState` `:834,837`; `Renderer` `:816`; `SubagentDetail`
    `:779,843`; `UiTranscript` `:889`; the 22-name `marmennill::session::{…}` block is re-exported verbatim by
    `src/session/mod.rs:63-90` and each name is used at least once) → no `unused_imports` violation.
  * C struct literals (field-by-field): `WorkerSection` (`:406-441`) vs `src/session/format/worker.rs:24-93` ✓;
    `WorkerCounters` (`:425`) ✓; `SubagentPanel` (`:491-506`) vs `src/session/format/ui.rs:166-201`
    (13 fields: `name, task_id, prompt, logs, thinking, content, is_active, context_tokens,
    worked_duration_secs, started_at, last_activity_at, turn_thinking, is_thinking`) ✓; `ManagerSection`
    (`:314-324`) vs `src/session/format/envelope.rs:211-238` (7 fields) ✓; `PinWitness` (`:316-319`) vs
    `envelope.rs:241-250` ✓; `IdentityFloors` (`:533-537`) vs `envelope.rs:153-166` ✓; `ComposerState`
    (`:2119-2125`, all 5 fields, no `..`) vs `src/session/format/ui.rs:245-257` ✓; `CapturePayload`
    (`:545-550`) vs `src/session/commit.rs:272` ✓; `CommitPolicy` (`:563-567`, 3 fields) vs
    `commit.rs:281-292` ✓; `write_bundle` argument list (`:568-575`) matches `commit.rs:493-500`
    (`&Path, &str, &CapturePayload, ProcessEnding, &CommitPolicy, &CommitLock`) ✓.
  * C dead_code / unused_variables: every private helper in the test file has a call site
    (`registry_lock` `:941`+, `bundle_dir`, `root_of` `:881`, `cleanup` `:1152`, `point`, `clock_pair` `:1169`,
    `anchor_consistency` `:1670`, `re_anchor_error` `:1433`, `expected_detail_duration` `:1147,1299`,
    `sys/user/assistant/call/tool`, `manager_transcript` `:313`, `manager_section` `:546`,
    `worker_history` `:400`, `worker_fixture` `:1576`, `panel_fixture` `:997`, `pane_lines` `:524`,
    `base_ui` `:1007`, `floors` `:1018`, `payload` `:1018`, `commit` `:585,2023`, `write` `:1015`,
    `explain` `:622`, `restored_session` `:644`, `refused` `:1985`, `committed_ui` `:1023`,
    `committed_workers` `:1679`, `member_bytes` `:678`, `member_paths` `:676`, `fingerprints` `:685`,
    `assert_bundle_bytes_unchanged` `:1966`, `manifest_json` `:1803`, `section_json` `:1347`,
    `notice_for` `:1526`, `assert_panel_fields_eq` `:1050`, `PaneWitness::snapshot` `:1069`,
    `PaneWitness::status_events` `:915`, `boot_with_panes` `:1060`). Every struct field is read
    (`after_restore` `:798`, `draws_at_snapshot` `:1062`, `captured_subagents` `:1110`, `report` `:1526`,
    `session_id` `:2204`, `renderer` `:915`), and no destructured binding is left unread in the bodies I read
    (`composer_witness` `:2143`→`:2249`, `scroll_witness` `:2144`→`:2253`, `guard` `:2435`→`:2441,2461`).
    `PaneWitness` implements **all** required `Renderer` methods (`init`/`on_event`/`flush`/`poll_input`/
    `read_input`/`input_state`/`input_state_shared`/`shutdown` + defaults overridden), matching
    `src/ui/mod.rs:88-173`.
  * C every public symbol the test calls exists in `src/`: `INTERRUPTED_STATUS_LABEL`
    (`src/session/worker_journal.rs:61`), `project_worker_section` (`:591`), `queued_notice_notices` (`:1015`),
    `pending_notice_notices` (`:995`), `anchor_wall` (`src/session/time.rs:58`), `load_bundle`
    (`src/session/restore.rs:72`), `fingerprint` (`src/session/commit.rs:70` re-export of `format`'s `:81`),
    `worker_rel_path` (`commit.rs:100`), `write_bundle` (`commit.rs:493`), `CommitLock` (`:403`),
    `CommitError::Serialize` (`:323,333`, `thiserror::Error` at `:322` ⇒ `Display` for `panic!("{error}")`),
    `MARMEL_DIR` (`src/manager/phase.rs:106`), `MANIFEST_FILE`/`UI_FILE`
    (`src/session/format/mod.rs:55,57`), `TimePoint` (`:97`), `workers::{worker_id_seq_floor:258,
    completed_seq_floor:269, worker_state_snapshot:341, register_active_worker_with_token:381,
    has_active_workers:659, cancel_active_worker:957}`, `notice::{notice_id_floor:49, worker_inbox_len:259,
    post_notice_to_worker:463}`, `boot`/`boot_if_requested`/`build_restored_context`/`BootSink`/`BootReport`
    (`src/ui/session.rs:1076,1392,1417,1016,1027`), `MessageRecord::content()`
    (`src/session/format/messages.rs:144`), `DanglingToolCallState::observe`
    (`src/session/format/worker.rs:176`), `RepairReport`/`Refusal`/`RestoreOutcome`
    (`src/session/format/restore.rs:16,19,103,235`), `ActiveWorkerGuard(pub String)` (hence `guard.0`).
    `tokio_util`, `chrono`, `serde_json`, `anyhow` are all `[dependencies]` (`Cargo.toml:12-56`), therefore
    linkable from an integration-test target.
* Everything that could not be settled statically is listed in §3.

## 1. CONFIRMED errors

No hard (compile-breaking) error was found; the confirmed defects are factual/test-hygiene ones that will
mislead the next reader and leave an acceptance criterion unobserved.

1. `tests/test_session_restore_panel_abort.rs:1493` — the `#[ignore = "known gap AC-S4: no
   NonRestorableNotice{kind:JoinHandle, subject:<panel name>} is emitted for a restored active
   SubagentPanel; the only production JoinHandle emitter on the restore path is
   src/session/worker_journal.rs:839-847 and its subject is the worker key (design §6.3, §10.3)"]` reason is
   **falsified by this task's own code**: `src/ui/session.rs:1256-1274` now emits exactly that notice with
   `kind: NonRestorableKind::JoinHandle` (`:1263`) and `subject: panel.name.clone()` (`:1264`) for every
   restored panel with `is_active: true`. Consequence: the criterion `ac_s4_…` (fn at `:1494`, `#[test]` at
   `:1492`) stays ignored, so the new emitter has **no executing coverage** and `cargo test` reports green
   without AC-S4 ever being observed. *Minimal fix:* delete line 1493 (leave `#[test]` at `:1492`) and run
   `cargo test --test test_session_restore_panel_abort ac_s4` to confirm it now passes.
   (Not a compile error — ignored tests still compile, and the body compiles: `notice_for` at `:705-714`
   matches the call at `:1526`, `PanelFixture { name, is_active, logs, thinking, content, ..panel_fixture(…) }`
   at `:1500-1508` uses real fields, `booted.report.repair.notices` matches `BootReport::repair` /
   `RepairReport::notices`.)
2. `tests/test_session_restore_panel_abort.rs:1484-1487` and module doc `:47` — the prose makes the same now
   false claim ("`grep 'NonRestorableKind::JoinHandle' src/` yields that single production site plus in-crate
   unit tests (`src/ui/raw.rs:587`, `src/ui/session_tests.rs:1015`, `src/ui/tui/tests.rs`); nothing keys a
   notice on a panel name"). Actual sites today: `src/session/worker_journal.rs:845` (production, subject =
   worker key), **`src/ui/session.rs:1263` (production, subject = panel name)**, `src/ui/raw.rs:585` (unit
   test; the cited line 587 is off by 2), `src/ui/session_tests.rs:1015`, `src/ui/tui/tests.rs:2515`.
   *Minimal fix:* rewrite both blocks to name the panel-name emitter and correct `raw.rs:585`.
3. Stale source anchors in the same edits (documentation drift that will misdirect the next audit):
   * `src/ui/session.rs:1294` and `src/ui/session.rs:1539` cite `ui/tui/mod.rs:1547` for
     `TuiRenderer::token_witness_warnings`; the method is at `src/ui/tui/mod.rs:1581`.
   * `src/ui/session.rs:1594` cites `ui/tui/mod.rs:1478` for `subagent_detail_from_panel`; it is at
     `src/ui/tui/mod.rs:1489`.
   * `tests/test_session_restore_panel_abort.rs:240` cites `src/ui/tui/mod.rs:1499` for `duration_from_secs`;
     it is at `src/ui/tui/mod.rs:1510`.
   *Minimal fix:* repoint the three anchors (the two `src/ui/session.rs` ones name the same symbol).
   Accurate by contrast: `src/ui/session.rs:1280-1281` (`ui/tui/events.rs:465-481` →
   `src/ui/tui/events.rs:465 pub(crate) fn clamp_scrolls`, and `ui/tui/mod.rs:1066-1068` →
   `src/ui/tui/mod.rs:1066-1068` `selected_subagent_idx` clamp).

## 2. LIKELY issues

* **Notice-key space overlap, not separated by construction.** `push_repair_notice` dedups on
  `(kind, subject)` (`src/ui/session.rs:1462-1468`, key from
  `src/session/format/restore.rs:225-229`) and the new panel notices reuse `NonRestorableKind::JoinHandle`.
  Worker notices are pushed first (`src/ui/session.rs:1147`, fed by
  `src/session/worker_journal.rs:843-847` with `subject = key`), so a panel whose `name` equalled a worker key
  would have its notice silently dropped. Practically safe: `SubagentPanel::name` is documented as
  "the specialist role id (`coder`, `researcher`, …)" (`src/session/format/ui.rs:168-170`) while worker keys
  are `{agent}-{task}` / `{agent}-wN`, so the spaces do not collide — but nothing enforces it, and the test
  fixture name `t014e-ac-s4-panel-coder` (`tests/…:1496`) is far from a real key, so the test would not catch
  a collision either. Consider a distinct kind or a `panel:` subject prefix.
* **Order deviations vs. the task's stated sequence (benign but real):** an extra
  `renderer.capture_ui()` is taken *before* `restore_panes` (`src/ui/session.rs:1234`, needed by the token
  witnesses at `:1298-1301`), and the panel-notice loop sits *between* `set_subagents` (`:1244`) and the
  post-restore `capture_ui` (`:1286`). Net relative order of the required steps is unchanged; token witnesses
  are intentionally folded after `announce_restore` (`:1292-1301`), which means those sentences reach
  `BootReport::repair.warnings` but **never** the panes or the journaled `UiRecord::Status` for this boot — an
  asymmetry a reader of `BootReport` versus the screen will see.
* **New unit test is order-coupled.** `src/ui/tui/tests.rs:2940` asserts `warnings.len() == 5` and then
  indexes `warnings[0..4]` against the five field names in the exact order the two builders list them
  (`src/ui/tui/mod.rs:1527` iteration; `src/ui/tui/tests.rs:2999-3017`; production
  `src/ui/session.rs:1571-1596`). Reordering one list and not the other compiles fine and fails the test — or,
  worse, passes while naming the wrong field. A `contains`-per-field assertion would be sturdier.
* **Runtime-only expectations inside statically-clean tests:** `assert!(r.chat_scroll < 9_000)`
  (`src/ui/tui/tests.rs:2965`) and the AC-S4 detail assertions depend on `restore_panes`/`clamp_scrolls` and
  on `announce_restore` actually drawing; unverifiable here.
* `#[ignore]` inventory (complete, with reasons):
  * `tests/test_session_restore_panel_abort.rs:1493` — AC-S4 (see §1.1; reason now false).
  * `tests/test_session_restore_panel_abort.rs:2432` — `"known gap AC-A2: the capture side never samples
    PENDING_NOTICES/WORKER_INBOXES (src/session/worker_journal.rs project_worker_section and the boot harvest
    never call notice::worker_inbox_len), so a notice queued at abort time produces no PendingNotice in the
    committed worker section"` (fn at `:2433`). This one is **still true**: `src/session/worker_journal.rs:631`
    calls `pending_notice_notices(worker_key)`, and grep finds `notice::worker_inbox_len` only in
    `src/orchestrator/notice.rs:259` (definition) and in tests.
  * `tests/test_session_restore_ui.rs:1327` — `"acceptance pending AC-C3 branch 2: the clamp notice is
    implemented and folded into RepairReport::warnings at boot, but the criterion has not yet been observed
    passing (§6.6 I-U6)"`, rewritten by t-026 and consistent with the new code (doc at `:1316` already names
    `clamp_warning_sentences`); the only remaining wart is that a *implemented* criterion stays ignored —
    promoting it to a running test is the natural follow-up of §1.1.

## 3. Impossible to verify without a compiler

* The exact `cargo test ui::` count: `src/ui/` holds 191 `#[test]` + 5 `#[tokio::test]` = 196 test functions
  and no `#[ignore]` — consistent with the expected 195 → 196, but the pre-edit baseline (195) and the
  possibility that another `ui::` test was added/removed in the same uncommitted change cannot be established
  without `cargo test --output-format` or `git`.
* Anything requiring typeck/borrowck: that `&[(&str,u64,u64); 5]` coerces to `&[(&str,u64,u64)]`, that
  `u64::from` resolves only for the `u16` fields (types were read, not checked), that `&mut PaneWitness`
  coerces to `&mut dyn Renderer` and that `PaneWitness: Send` (trait bound at `src/ui/mod.rs:88`), that
  `RestoreRefused`'s `Debug`/field access in the `.unwrap_or_else` at `tests/…:894-910` typechecks, and that
  `Result<BootReport, RestoreRefused>::expect` bounds are satisfied.
* Whether `clippy = deny` (`Cargo.toml:71-73`) fires anywhere on the new code (e.g. `iter().copied().filter()`
  style suggestions, `needless_range_loop`, `to_string_in_format_args`, `unwrap_or_else` lints) — clippy
  cannot be run.
* Whether any `unused`/`unused_variables` finding exists in parts of `tests/test_session_restore_panel_abort.rs`
  that were only sampled (lines ~940-1490, 1553-2090, 2296-2430, 2477-2660 were read only in part); every
  helper, import, struct field and destructured binding I inspected was used.
* Whether the new panel `JoinHandle` notices actually reach the three loud destinations at runtime
  (`announce_restore` → panes + journal + `tracing::warn!`) and whether the resulting
  `RepairReport` still satisfies any downstream count expectations (e.g. code or tests asserting an exact
  notice count for a bundle with active panels).
* Round-trip/serde behaviour (`#[serde(deny_unknown_fields)]` on `NonRestorableNotice`, `ManagerSection`,
  `SubagentPanel`) and the test's JSON path assumptions
  (`manifest_json(&bundle)["ending"]["kind"]` at `tests/…:2154`).
