# t-032 (edit-only sub-part) — truthfulness pass on the session-restore tests, comments and docs

Scope actually executed: **comments, doc-comments, two `#[ignore = "…"]` string literals, and markdown**.
Zero executable Rust was touched. `src/ui/session.rs` was **not edited** (ownership rule); it was read only.

---

## 0. Environment: probe result and what that rules out

The single allowed probe was run once, not twice, and not re-tried:

```
$ cargo --version
ERROR: Unable to spawn /home/fredrik/.cargo/bin/marmel (deleted) because it doesn't exist
on the filesystem (ENOENT: No such file or directory)
```

Consequence (stated, not glossed over): **every** `run_command` in this session fails at spawn, so nothing
could be compiled, formatted, or executed. Therefore:

- **no test counts exist for this pass** (neither the default run nor the `--ignored` run);
- both `#[ignore]` **attributes were kept**, exactly as instructed — dropping the AC-S4 one was conditional on
  it passing, and "passed" was unobservable;
- there is no `git diff` / `cargo fmt` confirmation available; the "no executable change" guarantee below is
  established by reading every replaced block back, not by a compiler.

Verification method available and used: `grep_search` + `read_file`. Every line number cited in this document
was re-read from the tree (pre-edit **and** post-edit).

---

## 1. Ground truth re-verified (the reason the old prose was false)

| Fact | Verified location |
|---|---|
| `NonRestorableKind::JoinHandle` production emitter #1 (subject = **worker key**) | `src/session/worker_journal.rs:845` (`kind:` line, inside `report.push_notice(NonRestorableNotice {` at 844, `subject: key.clone()` at 846, detail 847-851) |
| `NonRestorableKind::JoinHandle` production emitter #2 (subject = **panel name**) | `src/ui/session.rs:1256-1274` — loop `for panel in &session.ui.subagent_panels` (1256) with `if !panel.is_active { continue; }`, `push_repair_notice(… )` at 1260, `kind:` at **1263**, `subject: panel.name.clone()` at 1264 |
| `NonRestorableKind::JoinHandle` production emitter #3 (raw UI path) | `src/ui/raw.rs:585` (the old citation said `:587` — off by two) |
| In-crate (non-production) sites of the same enum variant | `src/session/worker_journal.rs:1755`, `src/ui/session_tests.rs:1015`, `src/ui/tui/tests.rs:2515` |
| AC-A2 capture half **exists** | `pending_notice_notices` at `src/session/worker_journal.rs:995-1008`; wired into the capture at `src/session/worker_journal.rs:624` (`non_restorable.extend(pending_notice_notices(worker_key));`) inside `project_worker_section` (fn at 591); fed by `peek_worker_inbox` at `src/orchestrator/notice.rs:285-292` |
| Same store as `worker_inbox_len` | `peek_worker_inbox` / `worker_inbox_len` / `has_worker_inbox` share byte-identical key normalisation over the one `WORKER_INBOXES` (`src/orchestrator/notice.rs:98`, normalisations 260/272/286; recorded in `docs/audit_static_t014f_part1.md` §4) |
| `queued_notice_notices` | fn at `src/session/worker_journal.rs:1015`, body opens `(0..count.max(1))` at **1016**; only production caller is `src/ui/session.rs:1166`, guarded by `if queued > 0` at `src/ui/session.rs:1165` |
| Notice dedup | `push_repair_notice` compares `dedup_key()` = `(kind, subject)` at `src/ui/session.rs:1463-1464`; `dedup_key` defined at `src/session/format/restore.rs:227`; same dedup at `src/session/restore.rs:1744-1745` and `src/session/worker_journal.rs:704-705` |
| Anchor targets in `src/ui/tui/mod.rs` | `duration_from_secs` **1510**, `clamp_warning_sentences` **1527**, `TuiRenderer::token_witness_warnings` **1581**, `subagent_detail_from_panel` **1489**, `time_point_from_instant` 1478 |
| Tool-name facts re-read | `TOOL_ARCHIVE_PLAN = "archive_current_plan"` at `src/tool_names.rs:28`; `TERMINAL_LIST_DIRECTORY` 69; `TOOL_LIST_DIRECTORY` 72; `CANONICAL_TOOL_NAMES` at **115** (contains `TOOL_ARCHIVE_PLAN` at 118, contains neither `*_LIST_DIRECTORY`); exclusion stated at `src/tool_names.rs:61-63`; the two constants appear elsewhere only in test fixtures (`fix_loop.rs:2140-2141`, `ui/bridge/tests.rs:2161/2168`) — i.e. **no dispatcher handler** |
| Test-module anchors re-read | `src/session/worker_journal.rs:1232` (`use crate::tool_names::TOOL_RUN_COMMAND;`), **1267** (`name: TOOL_RUN_COMMAND.to_string(),`), **1826** (`function_name: TOOL_RUN_COMMAND.to_string(),`) |

---

## 2. Edits made — `file:line — before → after (why)`

Pre-edit line numbers are the ones verified by grep before each edit; post-edit numbers follow in parentheses.
All edits are in `tests/test_session_restore_panel_abort.rs` unless stated otherwise.

1. **:45-50 → :45-53** (module doc, AC-S4 bullet)
   before: `**AC-S4 (notice half)** cannot be satisfied: the only production emitter of NonRestorableKind::JoinHandle on the restore path is src/session/worker_journal.rs:839-847, whose subject is a **worker key**; nothing anywhere emits a notice whose subject is a panel name.`
   after: `**AC-S4 (notice half)** is implemented but **has never been executed in this session**: there are **two** production emitters — src/session/worker_journal.rs:844-851 (kind line 845, subject = worker key) and src/ui/session.rs:1256-1274 (kind line 1263, subject = panel name, one per restored SubagentPanel with is_active: true) … carries #[ignore] pending its first execution (t-014f / t-034).`
   why: "only emitter" / "nothing anywhere" is false on the current tree; the `839-847` range did not contain the `kind` line either (it is 845, block 844-851).

2. **:52-58 → :54-63** (module doc, AC-A2 capture-half bullet)
   before: `**AC-A2 (capture half)** cannot be satisfied: the capture side never samples PENDING_NOTICES / WORKER_INBOXES (project_worker_section and the boot harvest never call notice::worker_inbox_len), so a notice … leaves no trace in the bundle …`
   after: `**AC-A2 (capture half)** is implemented but **has never been executed in this session**: the capture side does sample the live inbox — project_worker_section folds in pending_notice_notices (call at worker_journal.rs:624, fn 995-1008), reading WORKER_INBOXES through orchestrator::notice::peek_worker_inbox (notice.rs:285-292) rather than through worker_inbox_len … probe stays #[ignore]d until that first execution.`
   why: the capture path landed; the bullet asserted its absence.

3. **:240 → :246**
   before: `/// `duration_from_secs` (`src/ui/tui/mod.rs:1499`): the projection that turns a …`
   after: `/// `duration_from_secs` (`src/ui/tui/mod.rs:1510`): …`
   why: stale anchor. Note the deliberate deviation from the brief: see §4.

4. **:1483-1492 → :1488-1501** (doc block of the AC-S4 test; `#[test]` line kept verbatim)
   before: `**The notice half is a real gap**: the restore path emits JoinHandle only per *worker* (src/session/worker_journal.rs:839-847) … `grep 'NonRestorableKind::JoinHandle' src/` yields that single production site plus in-crate unit tests (src/ui/raw.rs:587, src/ui/session_tests.rs:1015, src/ui/tui/tests.rs); nothing keys a notice on a panel name. A bundle with an active panel and no worker at all therefore announces nothing about the handle that died with the process …`
   after: `**The notice half is implemented but has never been executed here**: two production sites — src/session/worker_journal.rs:844-851 (kind line **845**, subject = worker key) and src/ui/session.rs:1256-1274 (kind line **1263**, one NonRestorableNotice { kind: JoinHandle, subject: <panel name> } per restored SubagentPanel carrying is_active: true) … a third on the raw-UI path (src/ui/raw.rs:585), plus in-crate unit tests (src/ui/session_tests.rs:1015, src/ui/tui/tests.rs:2515) … therefore does announce the handle that died with the process. The assertion below is kept at full strength and ignored **only because it has never been run in this session** — t-014f / t-034 own its first execution.`
   why: three false claims in one paragraph plus the off-by-two `raw.rs:587` citation (item 3 of the brief).

5. **:1493 → :1503** (`#[ignore]` reason literal only; attribute kept)
   before: `#[ignore = "known gap AC-S4: no NonRestorableNotice{kind:JoinHandle, subject:<panel name>} is emitted for a restored active SubagentPanel; the only production JoinHandle emitter on the restore path is src/session/worker_journal.rs:839-847 and its subject is the worker key (design §6.3, §10.3)"]`
   after: `#[ignore = "implementation landed at src/ui/session.rs:1256-1274 (kind line 1263, subject = panel name); awaiting first execution by the t-014f/t-034 gate"]`
   why: brief item 1 — reason must state the real blocker (no execution yet), not a missing implementation.

6. **:2286-2295 → :2297-2307** (doc block of the **non-ignored** `ac_a2_the_notices_are_present_specific_and_the_outcome_is_restored_not_complete`)
   before: `**Stated limit on the `PendingNotice` half:** the capture side never samples `PENDING_NOTICES` / `WORKER_INBOXES` — neither … nor the boot harvest calls `notice::worker_inbox_len` — so a notice that really was queued when the process died leaves nothing in the bundle …`
   after: `**Stated basis for the `PendingNotice` half:** the capture side **does** sample `PENDING_NOTICES` / `WORKER_INBOXES` — project_worker_section folds in pending_notice_notices (fn 995-1008, call at worker_journal.rs:624), which peeks the very inbox notice::worker_inbox_len counts (orchestrator::notice::peek_worker_inbox, src/orchestrator/notice.rs:285-292) — so a queued notice does reach the bundle; that capture half is pinned by the probe ac_a2_capture_side_records_the_notice_queued_at_abort_time, still #[ignore]d because it has never been executed. Boot reads the count from section.non_restorable and re-spells it (src/ui/session.rs:1157-1181) …`
   why: same false claim sitting on a **live** (non-ignored) test's doc; comment-only, no behaviour touched. (Beyond the literal list in the brief, flagged in §3.)

7. **:2426-2431 → :2440-2449** (doc block of the ignored AC-A2 capture probe; `#[test]` line kept verbatim)
   before: `It does not: `project_worker_section` never calls `notice::worker_inbox_len`, so `section.non_restorable` stays empty and boot (which counts that field) has nothing to re-spell.`
   after: `The capture path provides one: pending_notice_notices (src/session/worker_journal.rs:995-1008) is folded into section.non_restorable by project_worker_section at src/session/worker_journal.rs:624, reading the live inbox through orchestrator::notice::peek_worker_inbox (src/orchestrator/notice.rs:285-292) — the same WORKER_INBOXES entry that notice::worker_inbox_len counts, not a second store. Kept at full strength; the #[ignore] below records only that this probe has never been executed.`
   why: false claim; this is the doc the audit already labelled false (`docs/audit_static_t014f_part1.md` §5.1).

8. **:2432 → :2451** (`#[ignore]` reason literal only; attribute kept)
   before: `#[ignore = "known gap AC-A2: the capture side never samples PENDING_NOTICES/WORKER_INBOXES (src/session/worker_journal.rs project_worker_section and the boot harvest never call notice::worker_inbox_len), so a notice queued at abort time produces no PendingNotice in the committed worker section"]`
   after: `#[ignore = "capture path exists (pending_notice_notices at src/session/worker_journal.rs:995-1008, wired into project_worker_section at worker_journal.rs:624, fed by orchestrator::notice::peek_worker_inbox at src/orchestrator/notice.rs:285-292); test pending first execution by the t-014f/t-034 gate"]`
   why: brief item 2.

9. **`docs/audit_static_t014f_part1.md`** — appended **§7 "Audited inventory (t-032 addendum — still read-only: no compiler, no test run)"** after §6, no rewrite of existing content:
   - pointer line to **§3** of that document for the full `TOOL_*` / `TERMINAL_*` name → value table (nothing duplicated);
   - anomaly 1: `TOOL_ARCHIVE_PLAN = "archive_current_plan"` (`src/tool_names.rs:28`) — name ≠ value stem;
   - anomaly 2: `TOOL_LIST_DIRECTORY` (`:72`) / `TERMINAL_LIST_DIRECTORY` (`:69`) have no dispatcher handler and are deliberately absent from `CANONICAL_TOOL_NAMES` (`src/tool_names.rs:115`, statement at `:61-63`);
   - corrected anchors `src/session/worker_journal.rs:1232 / 1267 / 1826` explicitly replacing the stale `1198 / 1233 / 1792`;
   - re-verified notice-plumbing facts (two `JoinHandle` production emitters + `raw.rs:585`; the AC-A2 capture chain), noting that both `#[ignore]` attributes were kept.

**Safety self-check (read back after editing):** `#[test]` attributes at 1502 / 2309 / 2450, `#[ignore]` at
1503 / 2451, test `fn` signatures at 1504 / 2310 / 2452 — unchanged. No import, signature, visibility,
assertion, or other attribute was modified anywhere in this pass.

---

## 3. Where I went one step beyond the literal list (and why)

The brief's list covered the `#[ignore]` reasons but the same two false claims also sit in **doc-comments**
attached to those tests (items 6 and 7 above) and in the file's module doc (items 1 and 2). Those are
comment/doc-comment lines, which the safety rule explicitly permits, and leaving them would have kept the file
self-contradictory (accurate `#[ignore]` reason next to an inaccurate `///` paragraph). No other file was
touched.

---

## 4. Deliberate deviation from the brief (item 4)

The brief said line 240 "cites `src/ui/tui/mod.rs:1499` for `clamp_warning_sentences`, which is actually at
`src/ui/tui/mod.rs:1527`". Re-reading the line: the sentence names **`duration_from_secs`**, not
`clamp_warning_sentences`, and `duration_from_secs` is at **1510** (1527 is indeed `clamp_warning_sentences`).
I therefore corrected the anchor to the target the comment actually names (`:1510`) rather than re-pointing the
sentence at an unrelated function. If the intent was to re-point the comment at `clamp_warning_sentences`, that
is a semantic change to the doc text and was **not** made — flag it back and it is a one-line follow-up.

---

## 5. Deferred item A — stale comment anchors in `src/ui/session.rs` (NOT edited: forbidden file, owned elsewhere)

One-line TODO for the owner of `src/ui/session.rs` (all three are comment/doc lines; verified targets in
`src/ui/tui/mod.rs`):

- `src/ui/session.rs:1294` — ``(`ui/tui/mod.rs:1547`)`` → ``(`ui/tui/mod.rs:1581`)`` — the sentence names
  `TuiRenderer::token_witness_warnings`, which is at **1581**.
- `src/ui/session.rs:1539` — ``(`ui/tui/mod.rs:1547`)`` → ``(`ui/tui/mod.rs:1581`)`` — same symbol.
- `src/ui/session.rs:1594` — ``(`ui/tui/mod.rs:1478`)`` → ``(`ui/tui/mod.rs:1489`)`` — the sentence names
  `ui::tui::subagent_detail_from_panel`, which is at **1489**. (Caveat so nobody "fixes" it back:
  `time_point_from_instant` *is* at 1478, so `:1478` is wrong only where the prose names
  `subagent_detail_from_panel`.)

---

## 6. Deferred item B — `queued_notice_notices` 0→1 inflation and the missing `TEST_NOTICE_MUTEX` guard
(assessed here, **not** applied: both fixes require changing executable Rust)

### B.1 `(0..count.max(1))` at `src/session/worker_journal.rs:1016`

Facts: `pub fn queued_notice_notices(worker_key: &str, count: usize) -> Vec<NonRestorableNotice>` at 1015,
body `(0..count.max(1))` at **1016**, emitting `count.max(1)` byte-identical `PendingNotice` entries whose
`detail` asserts that a steering notice "was never delivered and its queue did not survive the restart".

Severity assessment — **latent, not currently shipped**:

- The only production caller is `src/ui/session.rs:1166`, and it sits inside `if queued > 0` (`session.rs:1165`),
  where `queued` is the count of capture-side `PendingNotice` entries tallied at `session.rs:1157-1163`. So
  today `count` can never be 0 at that call site: **no operator-facing over-claim ships right now**, and the
  fix cannot change production behaviour.
- It is still a real defect: (i) the function is `pub`, and its own doc says "One NonRestorableNotice **per**
  queued-but-undelivered notice", so `count = 0` → 1 notice is a fabricated loss for the next caller;
  (ii) the fabricated detail is *less* truthful than the sibling `pending_notice_notices` (995-1008), which names
  the actual `notice.notice_id` — the same information loss already recorded in
  `docs/audit_static_t014f_part1.md` §5.2; (iii) independent of `max(1)`, all N re-spelled entries share
  `(kind, subject)`, and `push_repair_notice` dedups on exactly that key (`src/ui/session.rs:1463-1464`,
  `dedup_key` at `src/session/format/restore.rs:227`), so N queued notices for one worker collapse to **one**
  reported line. Any "one notice per queued notice" claim is therefore only true per *distinct worker key* —
  worth stating in the doc block (1012-1014) at the same time as the code fix.

Exact edit for the shell-enabled pass (`src/session/worker_journal.rs:1016`):

```rust
    (0..count)
```

…plus a regression guard in the same `mod tests` (there is none today: a repo-wide grep for
`queued_notice_notices` returns only the definition at `worker_journal.rs:1015`, the production caller at
`src/ui/session.rs:1166`, the integration-test uses in `tests/test_session_restore_panel_abort.rs`, and a
comment in `src/ui/session_tests.rs:610` — i.e. no unit test in this module):

```rust
    assert!(crate::session::worker_journal::queued_notice_notices("coder-t032-zero", 0).is_empty());
    assert_eq!(
        crate::session::worker_journal::queued_notice_notices("coder-t032-two", 2).len(),
        2
    );
```

(If the module needs no new import, the fully-qualified paths above avoid a `use` line, which matters because
`unused_imports = "deny"` sits in `[lints.rust]` at `Cargo.toml:67-70` — re-read: `unused` 68, `dead_code` 69,
`unused_imports` 70, as `docs/audit_static_t014f_part1.md` §1 records.)

### B.2 Missing `orchestrator::notice::TEST_NOTICE_MUTEX` guard on the `src/session/worker_journal.rs` tests

Confirmed mutex path and visibility (grepped, not assumed):

- declaration: `pub static TEST_NOTICE_MUTEX: LazyLock<tokio::sync::Mutex<()>> = …` at
  **`src/orchestrator/notice.rs:448`** — it is a **tokio (async)** mutex, not a std one;
- visible from `src/session/worker_journal.rs`: `src/lib.rs:18 pub mod orchestrator;` +
  `src/orchestrator/mod.rs:16 pub mod notice;` — so `crate::orchestrator::notice::TEST_NOTICE_MUTEX` resolves
  from the session module and its `#[cfg(test)] mod tests` (same visibility basis the audit recorded for the
  notice accessors, §4);
- policy text: `docs/cross_platform_verification.md:150` (sibling modules serialize through
  `orchestrator::notice::TEST_NOTICE_MUTEX`); already flagged as a LIKELY flake in
  `docs/audit_static_t014f_part1.md` §5.3.

Confirmed exposure in `src/session/worker_journal.rs`:

- exactly **one** read of the global inbox in that file's tests: **line 1748**
  `crate::orchestrator::notice::worker_inbox_len(key), 0, "install must not steer or dispatch anything to the worker"`
  inside `restored_workers_come_back_interrupted_and_are_never_relaunched` (fn at **1705**, `#[test]` at **1704**),
  which holds only `crate::orchestrator::workers::TEST_WORKERS_MUTEX` (a **sync** `std::sync::Mutex`, taken with
  `.lock().unwrap_or_else(|e| e.into_inner())`);
- no test in that file posts a notice (`post_notice_to_worker` appears nowhere in it), so the failure mode is a
  **foreign** test queueing a notice for the same key while this one asserts `== 0` — a cross-suite flake under
  `--test-threads` > 1, matching the documented policy;
- exposure is widening: since `project_worker_section` now samples the inbox itself (`worker_journal.rs:624` →
  `peek_worker_inbox`), *any* capture-side test added to this module becomes an inbox reader and needs the same guard.

Exact edit the shell-enabled pass should make (preferred; keeps the test synchronous — `worker_journal.rs:1704-1708`
stays `#[test]`), inserted immediately after the existing `_workers` guard block:

```rust
        let _notices = crate::orchestrator::notice::TEST_NOTICE_MUTEX
            .blocking_lock();
```

Rationale / risk notes: `blocking_lock()` is the tokio API for taking a `tokio::sync::Mutex` from a non-async
thread and panics only when called from inside an async context, which this `#[test]` is not; the leading `_`
keeps `unused = "deny"` (`Cargo.toml:68`) quiet. Lock ordering: take it **after** `TEST_WORKERS_MUTEX`, as the
sibling suites do (`src/orchestrator/workers.rs:1818/1852/1881/1913` and `src/agents/runner/fix_loop.rs:1331/1420/1471`
all take the notice lock inside the workers-lock scope), so no test in the crate ever takes the two in opposite
order.

Alternative, if the pass prefers literal parity with those siblings: change `#[test]` at **1704** to
`#[tokio::test]` and use `let _notices = crate::orchestrator::notice::TEST_NOTICE_MUTEX.lock().await;` as the
first statement. Larger blast radius (the whole body moves onto a runtime), hence not the recommendation.

Not verifiable in this session (no compiler): that `blocking_lock()` resolves unambiguously on this tokio
version, and that `src/session/*` has no *other* inbox reader. The shell-enabled pass should confirm with
`cargo test --lib` plus `grep -n "worker_inbox_len\|has_worker_inbox\|peek_worker_inbox\|post_notice_to_worker" src/session/`.

---

## 7. Test counts

**Not obtainable.** The single `cargo --version` probe failed (ENOENT on the deleted `marmel` binary backing
the process spawner), so the requested
`cargo test --test test_session_restore_panel_abort -- --test-threads=1` and
`… -- --ignored --test-threads=1` runs were never attempted, and the AC-S4 `#[ignore]` was **not** dropped
(its removal was conditional on a passing run). Counts are owed by the t-014f / t-034 gate, which is exactly
what the two rewritten `#[ignore]` reasons now say.
