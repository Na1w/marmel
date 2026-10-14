# t-014f — gate verification of the current tree

Verified on: `marmennill 1.0.0`, `cargo 1.98.0 (797e8a9bc 2026-08-05)`, branch `1.0.0-refactor`
(`bbfb893`, dirty tree — nothing was committed by this pass).
Shell probe: `cargo --version` → success, so the full gate ran.

## 1. Commands, exit status, test counts

| # | Command | Exit status | Passed | Failed | Ignored | Notes |
|---|---------|-------------|--------|--------|---------|-------|
| 1 | `cargo check --all-targets` (first run) | **101** | – | – | – | 1 compile error: `E0382` at `tests/test_session_restore_panel_abort.rs:2277` |
| 1b | `cargo check --all-targets` (after repair R1, and re-confirmed at end) | **0** | – | – | – | clean, no warnings |
| 2 | `cargo clippy --all-targets --all-features -- -D warnings` (first run) | **101** | – | – | – | 10 hard lint errors in `tests/test_session_config.rs` (5) and `tests/test_session_restore_ui.rs` (5) |
| 2b | `cargo clippy --all-targets --all-features -- -D warnings` (after repairs R2–R5, re-confirmed at end) | **0** | – | – | – | clean, no warnings |
| 3 | `cargo test --test test_session_restore_panel_abort -- --test-threads=1` — run 1 | **101** | 7 | 1 | 2 | `ac_t2_a_non_finite_or_negative_offset_is_refused_and_never_defaulted_to_now` FAILED |
| 3b | same command — run 2 (flake probe) | **101** | 7 | 1 | 2 | identical failure, same assertion, same line ⇒ **deterministic, not a flake** |
| 4 | `cargo test --test test_session_restore_panel_abort -- --test-threads=1 -- --ignored` (verbatim as specified) | **0** | 0 | 0 | 0 | 10 filtered out — the second `--` is consumed by libtest as filter text, so **no** ignored test actually ran |
| 4b | effective form: `cargo test --test test_session_restore_panel_abort -- --test-threads=1 --ignored` | **0** | 2 | 0 | 0 | 8 filtered out. AC-S4 **and** AC-A2-capture both **pass** (run twice, stable) |
| 5 | `cargo test --test test_tool_name_literals` | **0** | 1 | 0 | 0 | `test_no_unexpected_raw_tool_name_literals_in_src` ok |
| 6 | `cargo test --test test_session_restore_ui` | **0** | 8 | 0 | 1 | as expected (8 passed + 1 ignored: `ac_c3_...`) |
| 6b | `cargo test --test test_session_restore_ui -- --ignored --nocapture` (read-only probe, `#[ignore]` untouched) | **0** | 1 | 0 | 0 | 8 filtered out — AC-C3 **passes** when executed; source of the clamp numbers in §4 |
| 7 | `cargo test --lib ui::` | **0** | 196 | 0 | 0 | matches the expected 196 |
| 8 | `cargo test --lib session::` | **0** | 66 | 0 | 0 | matches the expected 66 |
| 9 | `cargo test --lib agents::` | **0** | 60 | 0 | 0 | no expectation given; all green |
| S | sanity re-run of a target I touched: `cargo test --test test_session_config` | **101** | 18 | 1 | 0 | `load_none_uses_the_searched_paths_and_never_panics` — **sandbox/environment**, see §5 |

## 2. Repairs made (all confined to making the tree compile / pass `-D warnings`)

No assertion was weakened, removed or loosened; no `#[allow(...)]` was added; no `cargo fmt`
was run; `#[ignore]` attributes were not touched.

1. `tests/test_session_restore_panel_abort.rs:2254` — `vec![panel]` → `vec![panel.clone()]`.
   *Why:* hard compile error `E0382` (borrow of moved value `panel` at 2277, where
   `snapshot.subagent_panels[0].logs == panel.logs` is asserted again after `panel` was moved
   into the AC-A1 vector comparison at 2254). `SubagentPanel: Clone` already (used at 2146), so
   cloning the moved side keeps both assertions byte-identical.
2. `tests/test_session_config.rs:282-285`, `:309-312`, `:340-343`, `:381-384`, `:401-408` —
   replaced `let mut base = Config::default(); base.<field> = <value>;` with the equivalent
   struct literal `let base = Config { <field>: <value>, ..Default::default() };`.
   *Why:* `clippy::field_reassign_with_default` is a hard error under `-D clippy::all`
   (`-D warnings`). Field values are unchanged; every assertion in those five tests is untouched.
3. `tests/test_session_restore_ui.rs:14-16` — indented three module-doc continuation lines by two
   spaces. *Why:* `clippy::doc_lazy_continuation` (hard error under `-D clippy::all`); the
   preceding line begins with `+`, which the doc parser reads as a list item. Whitespace-only,
   wording untouched.
4. `tests/test_session_restore_ui.rs:1048` — `committed.composer.input_text.chars().any(|c| !c.is_ascii())`
   → `!committed.composer.input_text.is_ascii()`.
   *Why:* `clippy::needless_character_iteration` (hard error). Logically identical predicate, so
   the `assert!` keeps exactly the same acceptance set.
5. `tests/test_session_restore_ui.rs:1416-1417` — introduced a local
   `type TamperCase<'a> = (&'a str, &'a str, Box<dyn Fn(&mut UiSection) + 'a>);` and annotated the
   vector `Vec<TamperCase<'_>>`.
   *Why:* `clippy::type_complexity` (hard error). A `'static` alias fails to compile (the closures
   capture the locals `text` / `inside_e`), hence the lifetime parameter. No case, tamper lambda or
   assertion changed.

Nothing in the seven source files flagged as "edited blind" produced compile fallout:
`src/agents/runner/fix_loop.rs`, `src/session/worker_journal.rs`, `src/session/restore.rs`,
`src/orchestrator/notice.rs`, `src/session/format/worker.rs`, `src/session/format/restore.rs`,
`src/ui/session.rs`, `src/ui/tui/mod.rs` all pass `cargo check --all-targets` and clippy as found.

## 3. Observed AC-S4 and AC-A2 results

| Criterion / test | Location | Observed |
|------------------|----------|----------|
| **AC-S4** `ac_s4_an_active_panel_restores_its_byte_and_emits_a_join_handle_notice_naming_it` | `tests/test_session_restore_panel_abort.rs:1503` (`#[ignore]`, untouched) | **PASS** — executed via `-- --test-threads=1 --ignored`, `ok` twice in a row (2 passed / 0 failed / 8 filtered). Its `#[ignore]` reason (`src/ui/session.rs:1256-1274`, kind line 1263, subject = panel name) is now satisfied: **first execution observed by this gate — the attribute is now stale and ready for the owning agent to drop.** |
| **AC-A2** (notice half) `ac_a2_the_notices_are_present_specific_and_the_outcome_is_restored_not_complete` | runs un-ignored in item 3 | **PASS** (`ok`, both runs) |
| **AC-A2** (capture half) `ac_a2_capture_side_records_the_notice_queued_at_abort_time` | `tests/test_session_restore_panel_abort.rs:2451` (`#[ignore]`, untouched) | **PASS** — executed via `--ignored`, `ok` twice. Capture chain `project_worker_section` → `pending_notice_notices` (`src/session/worker_journal.rs:995-1008`, call at `:624`) → `peek_worker_inbox` (`src/orchestrator/notice.rs:285-292`) behaves. **Also ready for the owning agent to un-ignore.** |

## 4. Observed clamp-warning values (AC-C3, `tests/test_session_restore_ui.rs`, run with `--ignored --nocapture`)

| Offset | Stored | Live after clamp | Warning sentence emitted |
|--------|--------|------------------|--------------------------|
| `chat_scroll` | **9000** | **0** | `chat_scroll clamped by the live geometry: 9000 -> 0: the stored value does not fit the restored content, and the live value is what the panes show now` |
| `plan_scroll` | **12** | **0** | `plan_scroll clamped by the live geometry: 12 -> 0: …` (same phrasing) |
| `subagent_scroll` | **9000** | **28** | `subagent_scroll clamped by the live geometry: 9000 -> 28: …` |
| `subagent_list_scroll` | **40** | **3** | `subagent_list_scroll clamped by the live geometry: 40 -> 3: …` |

All four names and both values are named in the emitted `RepairReport::warnings` sentence
(`clamp_warning_sentences`, `src/ui/tui/mod.rs:1527`, folded in via `scroll_clamp_warnings`,
`src/ui/session.rs:1570` → `src/ui/session.rs:1287`), so AC-C3 branch 2 is observed passing —
the `#[ignore]` at `tests/test_session_restore_ui.rs:1327` is likewise stale (left untouched here;
that line belongs to another agent).

## 5. Verdict

**Not green — one real, deterministic failure; everything else passes.**

* **FAIL: `ac_t2_a_non_finite_or_negative_offset_is_refused_and_never_defaulted_to_now`**
  (`tests/test_session_restore_panel_abort.rs:2040`, exit 101; identical in two `--test-threads=1`
  runs, so not a flake). Branch (c) commits a `SubagentPanel` whose `started_at.elapsed_secs` is
  `NaN` and expects the **commit seam** to refuse; `commit(...)` returns `Ok` and the non-finite
  value reaches the bundle. Branches (a) and (b) — the negative-offset *loader* refusals — pass.
  Root cause is a production gap, not a test bug: the sanity guard
  `"<field> is not a sane TimePoint"` exists only on the load/validate side
  (`src/session/format/ui.rs:385` and `:402`); there is no finite/normal check in the commit path
  (`grep is_finite|is_normal src/session/commit.rs` → nothing). Left untouched per the "never
  weaken an assertion" rule — this needs a commit-side guard.
* The other two acceptance families in that target are green: **AC-S1/2/3 ok, AC-T1 ok,
  AC-A1/AC-A2/AC-A3 ok.**
* **Expected counts all matched**: 196 (`--lib ui::`), 66 (`--lib session::`), 60
  (`--lib agents::`), 8 + 1 ignored (`test_session_restore_ui`), 1 (`test_tool_name_literals`).
* **Environmental, not a code defect:** `cargo test --test test_session_config` (run only as a
  sanity check of my own repairs) fails `load_none_uses_the_searched_paths_and_never_panics` at
  `tests/test_session_config.rs:537` because `load(None)` walks `$HOME` candidates and `/home/fredrik`
  is not readable in this sandbox (`Permission denied (os error 13)`). Reproducible with `--exact`.
  The 18 other tests in that target pass.
* **Gate hygiene note:** the literal command `… -- --test-threads=1 -- --ignored` runs **0 tests**
  (the stray `--` becomes filter text). Use `--test-threads=1 --ignored` for the ignored runs.
* With the exception of AC-T2, the tree compiles clean and is clippy-clean under
  `--all-targets --all-features -D warnings` after repairs R1–R5.
