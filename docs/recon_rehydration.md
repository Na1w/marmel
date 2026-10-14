# Removal Map — Rehydration / Crash-Recovery / Deep-Freeze (feature delete)

Crate: `marmennill` (workspace root `/home/fredrik/marmel/wip`). Read-only recon; no source was modified.

## 0. Baseline facts (verified, not assumed)

- `cargo check --offline --all-targets` → **passes clean** (0 warnings) at HEAD `31f4e90`.
- `src/ui/session/recovery.rs` and `src/ui/session/tools.rs` are **NOT compiled**: there is no
  `mod recovery;` / `mod tools;` anywhere (`git grep -n "mod recovery\|mod tools" HEAD -- src`
  returns only `HEAD:src/types/mod.rs:3: mod tools;`, an unrelated module), and no
  `#[path = "session/…"]` attribute exists. `src/ui/mod.rs:6` declares `pub mod session;`, which
  resolves to `src/ui/session.rs`; submodules in `src/ui/session/` are inert without declarations.
  `git log -S "mod recovery"` / `-S "mod tools;"` find no history of a declaration → they were
  committed (d6627d7) never wired in. Both are dead files on disk.
- `src/ui/session/recovery.rs:11-80` (`recover_frozen_deliverable`) is a **simplified duplicate** of
  the live inline loop `src/ui/session.rs:135-317` (same `journal.is_frozen()` guard, same
  `tokio::spawn(recover_mgr.recover_frozen())`, same 50 ms poll/drain loop). The session.rs copy is
  richer (steer arbitration, `ui_transcript` appends, plan check-off, `[Recovered task …]` injection).
- `src/ui/session/tools.rs` (`DispatchState` + `dispatch_tool_calls`, 722 lines) contains **no**
  rehydration/frozen/journal identifiers → dead refactor artifact, **not** rehydration surface.
- CI gate: `.github/workflows/ci.yml:38` → `cargo clippy --all-targets --all-features -- -D warnings`.
  Any orphaned `use`/variable left behind **fails CI**.
- `PartialConfig` (`src/config.rs:239`) has **no** `#[serde(deny_unknown_fields)]` → stale
  `enable_rehydration = …` keys in user `marmel.toml` are silently ignored after removal (no break).

---

## 1. Removal inventory (file → lines → symbol/test → action → risk if missed)

### A. `src/config.rs`

| Lines | Symbol | Action | Risk if missed |
|---|---|---|---|
| 128-129 | doc + `pub enable_rehydration: bool` on `Config` | delete field | every `enable_rehydration:` literal (10 in tests) still compiles → cannot remove feature |
| 155 | `enable_rehydration: false` in `Config::default()` | delete | E0560 on literal if field already gone / leftover field |
| 198-200 | env `MARMEL_ENABLE_REHYDRATION` parsing | delete block | env var keeps re-enabling the feature in the field |
| 261 | `PartialConfig.enable_rehydration: Option<bool>` (TOML key) | delete | `[general] enable_rehydration` still accepted |
| 342-344 | TOML partial merge `base.enable_rehydration = v` | delete block | key resurrects feature |

No `src/config.rs` unit test references rehydration (verified).

### B. `src/ui/mod.rs` (public `Renderer` trait — cross-file API)

| Lines | Symbol | Action | Risk if missed |
|---|---|---|---|
| 129 | `fn rehydrate_ui(&mut self, _records: &[UiRecord]) {}` | delete default | orphan overrides/tests keep compiling forever |
| 130-133 | `fn rehydrate_messages(&mut self, messages: &[Message])` default body (calls `UiTranscript::from_legacy_messages`) | delete default | legacy-message rehydration path survives via default impl |
| 134 | `fn rehydrate_subagents(&mut self, _subagents: &[SubagentDetail]) {}` | delete default | subagent pane state can still be rehydrated |

Note: `128 set_subagents`, `135 set_thinking_budgets`, `136 reset_active_agent` are **unrelated — KEEP**.

### C. `Renderer` implementations (forced by §B)

| File:lines | Symbol | Action | Risk if missed |
|---|---|---|---|
| `src/ui/tui/mod.rs:1074-1084` | `rehydrate_subagents` override (`show_subagent_panel = true`, selection) | delete override | dead code; `show_subagent_panel` semantics silently differ |
| `src/ui/tui/mod.rs:1086-1143` | `rehydrate_ui` override (message replay + chat scroll) | delete override | dead code path |
| `src/ui/raw.rs:161-208` | `rehydrate_ui` override in `RawRenderer` | delete override | dead code path |
| `src/ui/raw.rs:216-250` | unit test `test_raw_renderer_rehydrate_ui` | delete test (keep `mod tests` at 211-251) | E0599 compile error in `--all-targets` |
| `src/ui/session_tests.rs:177+`, `src/ui/bridge/tests.rs:19+` | mocks `RecordingRenderer` / `TestRenderer` | **no change** (verified they do not override rehydrate_*) | — |

### D. `src/ui/session.rs` (the live rehydration + recovery surface)

| Lines | Symbol / block | Action | Risk if missed |
|---|---|---|---|
| 53-54 | `ui_transcript_path` / `transcript_path` | **KEEP** | breaks transcript persistence (out of scope) |
| 56-96 | `let (mut ui_transcript, transcript_loaded, has_rehydrated) = if cfg.enable_rehydration { … }` incl. `UiTranscript::load` (58), `ctx.load_transcript` (70), `renderer.rehydrate_ui` (82), `UiTranscript::from_legacy_messages` + `renderer.rehydrate_messages` + legacy migration save (85-88) | replace whole tuple with `let mut ui_transcript = UiTranscript::new();` | startup still loads disk state; `transcript_loaded`/`has_rehydrated` dangle → compile errors |
| 98-111 | `if has_rehydrated { … ui_transcript.extract_steering_history() … "Session transcript rehydrated from disk (Ready)" }` | delete block | steering history re-injected from disk |
| 113-131 | `let mut subagents = if cfg.enable_rehydration { rehydrate_subagents_with_ui(ctx.messages(), manager…&m.journal, None, Some(&plan), Some(&ui_transcript)) … renderer.rehydrate_subagents(&sa) … }` | replace with `let mut subagents = Vec::new();` | last in-crate consumer of `OrchestratorManager::journal` + helpers API |
| **135-317** | inline Deep-Freeze recovery loop (`while cfg.enable_rehydration && … mgr.journal.is_frozen()`, `frozen()` (141-145), status msg (148-151), `tokio::spawn(…recover_frozen())` (153-154), drain/steer loop (156-231), arb join (243-266), deliverable fold-in + `[Recovered task …]` + plan notice (268-317)) | **delete (the ONLY surviving removal path for this logic)** | duplicate recovery entry point; duplicate of `recovery.rs` (see §3) |
| 328-333 | `has_pending_plan = if cfg.enable_rehydration { !pending_tasks.is_empty() } else { false }` | **decision required** (see §5.1) | either plan-resume silently dies, or `has_pending_plan` becomes always-false dead logic |
| 335-345, 460-466, 475-487, 503-511, 570 | pending-plan prompt / resume message / SYSTEM NOTICE / steer-arbitration gate `has_pending_plan \|\| …` | edit consistently with the §5.1 decision | dangling variable (E0425) or lost plan-resume UX |
| 373-474 | `if has_rehydrated { …arbitrate steering at resume prompt… } else { break trimmed }` | **edit**: must collapse to the `else` arm (or unconditional arbitration if plan-resume is kept) | **highest regression risk**: goal/`UiRecord::User` handling silently inverted |
| 476 | `if transcript_loaded { ctx.messages().get(1) … }` | delete (`transcript_loaded` no longer exists) | compile error |
| 497-501 | `if !has_rehydrated { ctx.set_goal(goal); ui_transcript.append(UiRecord::User…); save }` | **make unconditional** | if left gated on a removed var or inverted, the user goal never reaches context/UI transcript |

### E. `src/ui/helpers.rs`

| Lines | Symbol | Action | Risk if missed |
|---|---|---|---|
| 61-69 | `if cfg.enable_rehydration && …` "## Active Execution Plan" prompt block inside `load_system_prompt_with_plan` | **edit**: ungate (keep prompt block) or delete block | compile error after §A; or plan context lost from system prompt |
| 409-436 | `pub fn load_prompt_from_disk` | delete (its only callers are 569/675/756/845/894/922, all inside the deleted rehydrator) | **dead_code warning → CI fails** (`helpers` is `pub(crate)`, `src/ui/mod.rs:4`) |
| 438-500 | `fn is_subagent_in_flight(journal, plan, …)` (CrashJournal `.frozen_all()`/`.journal()`/`JournalEventKind::Frozen`) | delete | orphan (its 3 callers are all in §E rehydrator) → warning |
| 502-518 | `#[allow(dead_code)] pub fn rehydrate_subagents(…)` wrapper (510 attr) | delete | unused-API + crash-journal coupling remains |
| 520-931 | `pub fn rehydrate_subagents_with_ui(…)` incl. frozen-snapshot pass (530-611), journal-event pass (731-796), UI-transcript pass (798-825), recovered-deliverable pass (827-863), plan pass (865-913), prompt backfill (915-927) | delete whole fn | the core rehydrator remains reachable from tests |
| 1041-1130 | tests `test_rehydrate_subagents_from_messages`, `…_with_recovered_deliverable`, `…_from_plan` | delete | E0425/E0433 after fn removal |
| 1168-1306 | tests `…_active_when_in_flight`, `…_from_crash_journal_frozen` (uses `freeze::CrashJournal` at 1216), `…_loads_prompt_from_disk`, `…_ui_transcript_completion` | delete | blocks §F/E deletion ordering |
| 1039 | `use crate::types::{Message, ToolCall};` in `mod tests` | becomes unused → delete/edit | **CI `-D warnings` failure** |
| 369-407 (`clean_task_id`, `find_subagent_mut`), 933+ (`extract_failure_reason`) | **KEEP** — live callers `src/ui/session.rs:1075,1461,272,303`, `src/orchestrator/mod.rs:498,500` | — | deleting breaks the delegation/steer path |

### F. `src/orchestrator/mod.rs`

| Lines | Symbol | Action | Risk if missed |
|---|---|---|---|
| 16 | `pub mod freeze;` | delete decl | module + its tests keep compiling, feature stays alive |
| 58 | `pub use freeze::{CrashJournal, FreezeSnapshot, JournalEventKind};` | delete re-export | public API leak; `orchestrator/tests.rs` (`use super::*`) breaks |
| 100-102 | doc + `pub journal: CrashJournal` field | delete field | all `m.journal` test/`session.rs` call sites persist |
| 139, 148 | `let journal = CrashJournal::new(plan.dir());` + `journal,` in struct literal | delete both | E0063/E0425 compile break |
| 243-254 | Deep-Freeze `self.journal.snapshot(entry.agent, &req)` inside `delegate()` (`let worker_id = …`) | delete block **including the `worker_id` binding** | unused `worker_id` → warning; journal writes still on disk |
| 330-335 | `if !worker_id.is_empty() { let _ = self.journal.clear(&worker_id, true); }` | delete block | references deleted `worker_id` → compile error |
| 365-516 | doc (365-378) + `pub async fn recover_frozen(&self) -> Result<Option<Deliverable>>` (379-516; registry re-resolve 386-405, prompt reload 410-424, worker run 440-472, `journal.clear` 484-487, events 489-510, `apply_check_off` 512-515) | delete whole method (`apply_check_off` at 531+ is shared — **KEEP**) | re-delegation entry point survives |

### G. `src/orchestrator/freeze.rs` — **delete entire file (371 lines)**

`FROZEN_STATE_FILE`(25), `CRASH_JOURNAL_FILE`(27), `FreezeSnapshot`(24-40), `JournalEventKind`(42-52),
`JournalEvent`(54-62), `JOURNAL_MUTEX`(64), `RawFrozen`(69), `CrashJournal`(74-256: `new/dir/frozen_path/
journal_path/snapshot/frozen/frozen_all/clear/journal/is_frozen/now/read_frozen_list/write_frozen_list/append`,
`Default` 258), unit tests 264-371. Risk if missed: public crash-journal API + `.session_*.json` artifacts remain.

### H. `src/orchestrator/tests.rs`

- 310-439 contiguous Deep-Freeze block: `test_orchestr_delegate_snapshots_and_clears` (310-335),
  `test_orchestr_recover_frozen_rehydrates_identical_worker` (337-382),
  `test_orchestr_recover_frozen_none_when_clean` (384-395),
  `test_orchestr_recover_frozen_fails_when_role_unknown` (397-439, asserts `"cannot be rehydrated"` at 420).
  Action: delete. Risk: `m.journal` / `JournalEventKind` (via `use super::*` at line 1) → compile errors.
  Tests at 284-308 and 441+ are unrelated — keep.

### I. `src/ui/tui/tests.rs`

- 1518-1804 contiguous rehydration block: `test_rehydrate_subagents_enables_panel_and_selects_latest`
  (1518-1569), `test_rehydrate_subagents_selects_active_subagent` (1571-1614),
  `test_rehydrate_messages_summarizes_delegation_results` (1616-1661),
  `test_rehydrate_messages_preserves_order_thinking_and_filters_synthetic` (1663-1751),
  `test_rehydrate_ui_records_renders_clean_chat_history` (1753-1804). Action: delete.
  Risk: E0599 (trait methods removed). Neighbors 1496-1516 (`test_format_terminal_math`) and
  1806+ (`test_tui_submit_empty_line_…`) are **KEEP**. `set_subagents` tests (455-520, 2339-2360) **KEEP**.

### J. `tests/test_ui_session.rs` (integration)

| Lines | Symbol | Action | Risk if missed |
|---|---|---|---|
| 62-63 | mock fields `rehydrated`, `rehydrated_ui` in `ScriptedRenderer` | delete fields | used by 8 deleted tests + constructors |
| 76-77, 90-91 | `rehydrated: Vec::new(), rehydrated_ui: Vec::new(),` in `new()` / `with_poll()` | delete | E0560 |
| 105-107, 108-112, 116-118 | mock overrides `rehydrate_ui`, `rehydrate_messages` (mirrors trait default, uses `from_legacy_messages` at 110), `rehydrate_subagents` | delete | E0407 "method not a member of trait `Renderer`" |
| 160 | `enable_rehydration: true` in `config_for_backend` (helper used by ~15 tests) | delete key | compile break after §A |
| 630-732 | `test_ui_session_rehydrates_transcript_and_resumes_plan` | delete | transcript-load + plan-resume assertions |
| 734-857 | `test_ui_session_recovers_frozen_and_injects_deliverable` (`journal.snapshot` 803-805, `is_frozen` 806/816) | delete | requires `OrchestratorManager::journal` |
| 859-955 | `test_ui_session_rehydrates_subagents_and_populates_agent_pane` | delete | renderer rehydrate assertion |
| 1376-1447 | `test_ui_session_rehydrates_without_plan_if_transcript_exists` | delete | depends on `renderer.rehydrated` |
| 1449-1526 | `test_ui_session_recovered_deliverable_placed_after_rehydrated_transcript` | delete | uses `mgr.journal` (1505) |
| 1528-1624 | `test_ui_session_saves_and_rehydrates_ui_transcript` — **split**: keep the save/`UiTranscript::load` half (1560-1585), drop session-2 rehydration half (1586-1623) or delete whole test | edit/delete | the KEPT journal-persistence coverage is lost if deleted wholesale |
| 1626-1710 | `test_ui_session_migrates_legacy_transcript_to_ui_transcript` | delete (legacy migration only happens inside the `enable_rehydration` branch, session.rs:85-88) | migration code path removed → test fails |
| 1712-1769 | `test_ui_session_rehydrates_steering_history_from_ui_transcript` | delete | `extract_steering_history` re-injection removed |
| 1774-1848 | `test_ui_session_recovery_arbitrates_user_input_and_streams_steer_response` (doc 1771-1773) | delete | exercises deleted session.rs recovery loop |
| 1852-1916 | `test_ui_session_recovers_multiple_frozen_tasks_sequentially` (`frozen_all` 1905/1915) | delete | journal API dependency |
| 1922-2019 | `test_ui_session_resume_prompt_steer_abort_immediately_continues_session` (doc 1918-1921; needs a pre-saved UI transcript + pending plan → depends on §D §5.1 decision) | **rewrite or delete** (coupled, not crash-journal) | silently fails if plan-resume gating changes |
| 2026-2121 | `test_ui_session_disabled_rehydration_starts_fresh_and_skips_recovery` (doc 2021-2025, asserts 2095-2110) | delete (its premise is the config key) — **but port 2112-2120** ("new session must still save transcripts", UI-transcript journal) to a kept test | loses the only "write-side journal" guard |
| — | After deletions, re-check imports at 1-40 (`marmennill::agents::Agent`, `MissionMarker`, `UiTranscript`, `UiRecord`) for unused → **CI** | edit | `-D warnings` failure |

### K. `tests/test_orchestrator.rs`

| Lines | Symbol | Action | Risk if missed |
|---|---|---|---|
| 12-14 | module-doc bullet 2 "Deep-Freeze snapshot/recover" | edit doc | stale contract claim |
| 166-238 | section header + `test_deep_freeze_snapshots_and_clears` (172-196), `test_deep_freeze_recover_rehydrates` (198-227), `test_deep_freeze_recover_none_when_clean` (229-237) | delete section | `m.journal`, `marmennill::orchestrator::JournalEventKind` (188/192) → compile error |
| 103-165, 239+ | depth gate / events / check-off / abort tests | **KEEP** | — |

### L. Docs / repo hygiene

| File:lines | Item | Action | Risk if missed |
|---|---|---|---|
| `README.md:272` | `.marmel/.session_frozen.json` row | delete row | documents a file that is no longer written |
| `README.md:273` | `.marmel/.session_journal.json` row | delete row | this is the **crash** journal, NOT the UI transcript journal |
| `README.md:362` | "Crash recovery and state rehydration." test-suite bullet | delete bullet | false test claim |
| `README.md:268-273` | gap: `.marmel/.ui_transcript.json` (`phase.rs:42 UI_TRANSCRIPT_FILE`) has no table row | optional add (kept feature) | — |
| `.gitignore:29-31` | `.session_frozen.json`, `.session_journal.json` + comment | optional delete (harmless if kept until rewrite) | stale ignore rules |
| `marmel.toml.example`, `marmel.toml.df`, `marmel.toml.cloud`, `prompts/*`, `AGENTS.md` | **verified clean** — no rehydration key/mention | no change | — |

---

## 2. Recommended safe deletion ORDER (crate compiles after every step)

1. **Orphans first**: `rm src/ui/session/recovery.rs src/ui/session/tools.rs`. Zero compile impact
   (not compiled). Also removes the temptation to "fix the wiring" instead of deleting.
2. **`src/ui/session.rs`** (§D): the single largest edit — drop the rehydration tuple (56-96),
   steering-rehydrate block (98-111), subagent rehydrator call (113-131), the whole recovery loop
   (**135-317**), and resolve `has_rehydrated` (373-474 → unconditional goal/append; 497-501 →
   unconditional) plus the `has_pending_plan` decision (§5.1). All in-crate uses of `journal` and the
   `rehydrate_*` trait calls vanish here. `journal`/`freeze` remain but unused (public → no warning).
3. **`src/ui/helpers.rs`** (§E): delete `is_subagent_in_flight`, `rehydrate_subagents`,
   `rehydrate_subagents_with_ui`, `load_prompt_from_disk`, the 7 rehydrate unit tests, fix the
   `mod tests` import (1039), and ungate/delete the `enable_rehydration` prompt block (61-69).
4. **Renderer surface** (§B + §C): `src/ui/mod.rs` defaults, `tui/mod.rs` overrides, `raw.rs`
   override + its unit test, `tui/tests.rs` 1518-1804. Safe only after step 2 removed all call sites.
5. **`tests/test_ui_session.rs`** (§J): mock fields/overrides, `enable_rehydration:` keys, and all
   rehydration/recovery tests. Must precede steps 6-7 because it touches both `mgr.journal` and the
   config field.
6. **Orchestrator** (§F + §G + §H + §K): remove `journal` field/construction/`snapshot`/`clear` and
   `recover_frozen` from `mod.rs`; delete `freeze.rs`, `pub mod freeze;` (16) and re-export (58);
   delete `orchestrator/tests.rs:310-439` and `tests/test_orchestrator.rs:12-14,166-238`.
   Do these as one atomic step (field + its tests).
7. **Config last** (§A): field, `Default`, env var, `PartialConfig`, TOML merge. Last, because every
   `enable_rehydration:` literal (10 sites) must already be gone.
8. **Docs/hygiene** (§L), then `cargo fmt --all` → `cargo clippy --all-targets --all-features -- -D warnings`
   → `cargo test --all-targets --all-features`.

### Duplicate-logic rule (§D vs. orphan)
`src/ui/session.rs:135-317` and `src/ui/session/recovery.rs:11-80` are two copies of the same
`recover_frozen` drive-and-drain logic. **Only one removal path may survive**: delete the orphan file
 outright (step 1) and delete the inline loop in `session.rs` (step 2). Never add `mod recovery;` and
never extract the inline loop into a new module — that re-introduces the feature under review. The same
one-path rule applies to `src/ui/session/tools.rs` (dead duplicate of session.rs's inline tool dispatch
at session.rs:685-1560): delete the file, keep the inline code.

---

## 3. MUST KEEP — UI transcript journal (`src/ui/transcript.rs`) is OUT OF SCOPE

`src/ui/transcript.rs` ("Persistent UI transcript journal for interactive sessions", line 1) is an
**unrelated feature**: the visible chat-history journal (`.marmel/.ui_transcript.json`). It must **not**
be deleted with the crash journal.

- **Belongs to the UI transcript journal (KEEP):** `UiRecord` (transcript.rs:13), `UiTranscript` (43)
  and all its methods `new/records/records_mut/is_empty/len/append/clear/load/save/
  extract_steering_history/from_legacy_messages` (49-276), its unit tests (278-469);
  `Plan::ui_transcript_path` + `UI_TRANSCRIPT_FILE` (`src/manager/phase.rs:42,293`) and the
  `Plan::clear`/archive deletion of the UI transcript (phase.rs:374-380, 456-458); every
  `ui_transcript.append/save/remove_file(&ui_transcript_path)` call in `src/ui/session.rs`
  (53, 166, 202, 253, 294, 367, 377, 429, 500, 524, 561, 569, 727, 847, 931, 1000, 1032, 1107, 1114,
  1200, 1223, 1331, 1386, 1418, 1496, 1503, 1614, 1624); the `ui_transcript` plumbing in
  `src/ui/bridge/drain.rs` / `drain_steer_arbitration_events_with_transcript` /
  `drain_delegation_events_with_transcript`; `UiRecord`/`UiTranscript` re-exports (`src/ui/mod.rs:12`);
  legacy context transcript (`TRANSCRIPT_FILE`/`.session_transcript.json`, `Plan::transcript_path`,
  `ContextEngine::save_transcript`) — all kept.
- **Belongs to the crash journal (DELETE):** `CrashJournal`, `FreezeSnapshot`, `JournalEvent`,
  `JournalEventKind`, `RawFrozen`, `JOURNAL_MUTEX`, `FROZEN_STATE_FILE`, `CRASH_JOURNAL_FILE`,
  `CrashJournal::journal()` / `journal_path()` / `frozen()` / `frozen_all()` / `is_frozen()` /
  `snapshot()` / `clear()`, field `OrchestratorManager::journal`, `recover_frozen`, and the
  `journal: Option<&…CrashJournal>` parameters in `rehydrate_subagents*` / `is_subagent_in_flight`;
  files `.session_frozen.json`, `.session_journal.json`; README rows 272-273.
- **Naming trap:** `journal` appears in *both* features. `transcript.rs:1,10,41` call the UI transcript
  a "journal"; `freeze.rs` calls `.session_journal.json` the "Crash Journal". Match on the type/file,
  never on the word `journal`.
- **Consequence to record, not to fix:** after removal, `UiTranscript::load` (transcript.rs:86) and
  `ContextEngine::load_transcript` (src/manager/context.rs:626) have **zero non-test callers** in the
  crate (their only caller is `src/ui/session.rs:58,70`). They stay (public API of a kept module → no
  dead-code warning); the rewrite gets to decide their fate.

---

## 4. Cross-references forced by public-API changes

- **`Renderer` trait (`src/ui/mod.rs:129,130-133,134`)** — removing the three `rehydrate_*` defaults
  is a breaking change for every implementor. Implementors in this repo (only these five, verified):
  `src/ui/tui/mod.rs:406`, `src/ui/raw.rs:73`, `src/ui/session_tests.rs:177`,
  `src/ui/bridge/tests.rs:19`, `tests/test_ui_session.rs:98`. Overrides exist only in tui/raw/test_ui
  (see §C, §J); the other two mocks are untouched. No other crate consumes `marmennill` in this repo,
  and no `Cargo.toml` `[lints]` section exists, so the only compile-visible fallout is
  `E0407 method is not a member of trait Renderer`.
- **`OrchestratorManager::journal` (public field, orchestrator/mod.rs:102) + `recover_frozen` (379)** —
  consumers: `src/ui/session.rs:116,137,141`, `src/orchestrator/tests.rs`, `tests/test_orchestrator.rs`,
  `tests/test_ui_session.rs` (803, 1505, 1820, 1899-1905, 2082-2107), `src/ui/helpers.rs` (params).
- **`marmennill::orchestrator::freeze` module + re-exports (mod.rs:16,58)** — public removal; also
  removes `JournalEventKind` used by name in `tests/test_orchestrator.rs:188,192`.
- **`Config::enable_rehydration` + `MARMEL_ENABLE_REHYDRATION` + TOML key** — struct-literal breaks in
  `tests/test_ui_session.rs:160,689,782,905,1419,1485,1550,1646,1734,2061`; `src/config.rs:155`.
  Other `Config { … }` literals (`src/ui/session_tests.rs:13`, `src/ui/helpers.rs:1355+`,
  `src/orchestrator/notice_tests.rs:110/219/321`, `tests/test_specialist_stream.rs`,
  `tests/test_ui_session.rs:1104`) all use `..Config::default()` → unaffected.
  Old `marmel.toml` files keep the key harmlessly (no `deny_unknown_fields`).
- **Test-only API already flagged dead:** `rehydrate_subagents` carries `#[allow(dead_code)]`
  (`src/ui/helpers.rs:510`) — evidence it is test-only; delete together with the rest.
- **CI:** `-D warnings` (`.github/workflows/ci.yml:38`) makes "leftover unused import/variable" a hard
  failure — re-check `src/ui/helpers.rs:1039`, `tests/test_ui_session.rs` header imports, and
  `worker_id` in `orchestrator/mod.rs` (243-254/330-335) after each step.

## 5. Open decisions to settle before the deletion PR

1. **`has_pending_plan` (session.rs:328-333)**: today plan-resume prompting is gated on
   `enable_rehydration`. Options: (a) ungate → `!pending_tasks.is_empty()` (keeps resume UX, keeps
   `tests/test_ui_session.rs:1922-2019` alive), or (b) delete plan-resume as part of the surface
   (then also delete session.rs:335-345, 460-466, 475-487, 503-511 and simplify 570 + that test).
   Pick one and apply everywhere; a partial change leaves `has_pending_plan` always-false.
2. **`load_system_prompt_with_plan` plan block (helpers.rs:61-69)**: ungate (recommended; system-prompt
   context is independent of rehydration) or delete.
3. **Journal write-side tests**: keep a replacement for
   `tests/test_ui_session.rs:1528-1560` (save path) and `:2112-2120` ("new session must still save
   transcripts") so the KEPT UI transcript journal stays covered after the rehydration tests go away.
