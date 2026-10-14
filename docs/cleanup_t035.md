# t-035 — AC-T2 write-side guard + two obsolete `#[ignore]`s dropped

## 1. The production defect (AC-T2)

`ac_t2_a_non_finite_or_negative_offset_is_refused_and_never_defaulted_to_now`
(`tests/test_session_restore_panel_abort.rs:1900`, the AC-T2 `fn` now at line 1905 after
the doc-only edits) failed deterministically at the `panic!` on line 2040, its NaN branch:

```text
AC-T2: a NaN elapsed_secs was accepted by the commit seam (/tmp/marmel-t014e-ac-t2-NaN-…)
       — it must never reach a bundle byte
```

Root cause, verified against the pinned dependency source: the commit seam's only
"guard" against a non-finite clock was an **assumption about `serde_json`**, and that
assumption is false. `serde_json` 1.0.151 (`src/ser.rs:167-177`) does **not** error on
`NaN`/`±infinity` — `serialize_f64` writes them as `null`. So `assemble_members`
serialized a panel clock as `"elapsed_secs": null`, C3/C4 renamed the members and the
manifest, and the corruption only surfaced at load time. That is a silent default on
the write path: the value the operator committed is not the value on disk, and the
bundle looks like a valid commit point until someone reads it.

## 2. The fix (production)

**Guard site:** `src/session/commit.rs:648` — `fn check_time_points(payload: &CapturePayload)
-> Result<(), CommitError>`, called at **`src/session/commit.rs:730`**, the first statement
of `assemble_members` (fn at `:724`), i.e. *before* C1 serializes anything and therefore
before any C3 rename and before the C4 manifest.

Behaviour:

* Every `TimePoint` the committer is about to serialize is located (worker
  `started` / `completed` / `last_activity`; panel `started_at` / `last_activity_at`;
  `plan_start` / `plan_completed` / `abort_armed_at`) — exactly the field set the
  load-side guards walk (`src/session/restore.rs:1084-1105`,
  `src/session/format/ui.rs:376-404`, the latter being the `:385` / `:402` sites).
* A non-finite `elapsed_secs` (`!value.is_finite()`, the finite half of the same
  `TimePoint::is_sane` predicate the loader uses) → `Err(CommitError::Serialize(detail))`
  **plus** `tracing::warn!("session commit refused before any write: …")`. `detail`
  names member `rel_path`, field and the offending value, per §7.5 ("the refusal
  message is a first-class deliverable"), and states "never defaulted to now".
* Nothing is substituted: `TimePoint::now()` is never written in place of a bad clock,
  and no member, no staging file and no `manifest.json` is produced — the bundle stays
  byte-identical and `load_bundle` still reports `NoBundle`.
* The guard is pure (reads the payload, repairs nothing), mirroring the load side.
* A **negative** offset gets a `tracing::warn!` naming member/field/value but is
  committed verbatim — see §4.

No test assertion was changed, no `#[allow]` was added, `src/ui/**`,
`tests/test_session_restore_ui.rs`, `tests/test_session_config.rs` and
`src/session/worker_journal.rs` were not touched. `src/session/time.rs` and
`src/session/format/ui.rs` needed no change: the guard belongs at the commit seam,
and the load-side guard already exists.

## 3. Design-doc wording followed

* **§5.6, last bullet (line 973):** "A `TimePoint` whose `wall` does not parse, or whose
  `elapsed_secs` is negative/NaN/inf, is a structural violation ⇒ `Refused` (D6).
  **No defaulting to \"now\"**." — the "no defaulting" half is what the write side
  violated (serde_json defaulted `NaN → null`), hence the write-side guard.
* **§9.8 item 35 / AC-T2 (line 1486):** "a `TimePoint` with `elapsed_secs`
  NaN/inf/negative ⇒ `Refused`, no defaulting to \"now\" (§5.6)."
* **§7.1 C1–C4 (lines 1218-1244):** the committer "computes payloads / serializes once
  each / writes members / manifest last"; C4 is the atomicity point, and section
  *validation* is not part of C1–C4 — so the guard is placed inside C1 (before any
  write) rather than turning the committer into a validator.
* **§7.5 fail-closed decision tree (line 1314):** the row "a section's structural
  preconditions fail (`TimePoint` NaN/negative §5.6 …) | `Refused` | **untouched** |
  field path and value" — the write-side refusal reproduces the same three properties
  (refusal, untouched bytes, field path + value in the message).
* **D6 fail-CLOSED** and §7.5's "The refusal message is a first-class deliverable."

## 4. Doc/test reconciliation (reported discrepancy)

The task brief asked for a guard that refuses "a non-finite (`NaN`/infinite) **or
negative** time value" at the commit seam. **The design doc does not say that, and the
test contradicts it in two branches**, so the doc (and the test) won:

* §5.6/§7.5 put the negative case on the **load** side: "structural violation ⇒
  `Refused`". §7.1 C1–C4 explicitly keeps the committer out of section validation.
* AC-T2 branch **(a)** commits a panel clock of `-1.0` and then requires the **loader**
  to refuse it with `member == ui.json` and a message containing `is not a sane
  TimePoint` and `-1.0`, with the `-1.0` byte **still on disk**
  (`assert_bundle_bytes_unchanged`). Branch **(b)** does the same for a worker clock of
  `-0.5`, naming the worker member and saying "never defaulted to now".
  A write-side refusal of negatives would make both branches fail at the fixture
  helper (`the fixture bundle must commit`), i.e. it would contradict the documented
  division of labour, not confirm it.
* What is genuinely unreachable on the load side is the **non-finite** case (AC-T2
  branch (c) says so out loud and pins the seam instead: `Err`, no manifest, `NoBundle`).
  That is precisely the case now refused at `src/session/commit.rs:648`.

Resolution implemented: **non-finite → refused at the commit seam (new); negative →
committed verbatim with a `tracing::warn!` and refused by the loader (documented, already
pinned).** Neither path ever defaults to `now()`.

## 5. The two dropped `#[ignore]`s

Both were verified green before removal, and again after removal:

```text
# before removal (t-035 verification), `-- --test-threads=1 --ignored`
running 2 tests
test ac_a2_capture_side_records_the_notice_queued_at_abort_time ... ok
test ac_s4_an_active_panel_restores_its_byte_and_emits_a_join_handle_notice_naming_it ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 8 filtered out

# after removal, inside the normal gate (run 1 and run 2, both identical)
test ac_s4_an_active_panel_restores_its_byte_and_emits_a_join_handle_notice_naming_it ... ok
test ac_a2_capture_side_records_the_notice_queued_at_abort_time ... ok
```

* **AC-S4** — `#[ignore = "implementation landed at src/ui/session.rs:1256-1274 …"]`
  removed; the test `fn` now sits at `tests/test_session_restore_panel_abort.rs:1507`.
  Its attached doc-comment prose was rewritten from "implemented but has never been
  executed here … carried at full strength and ignored only because it has never been
  run" to the truthful statement that the notice **is** emitted by production
  (`src/ui/session.rs:1256-1274`, kind line 1263, subject = panel name) and now runs in
  the ordinary gate.
* **AC-A2 capture half** — `#[ignore = "capture path exists (pending_notice_notices at
  src/session/worker_journal.rs:995-1008 …); test pending first execution …"]` removed;
  `fn` now at `tests/test_session_restore_panel_abort.rs:2459`. Its attached prose was
  rewritten from "the `#[ignore]` below records only that this probe has never been
  executed" to "kept at full strength and run in the ordinary gate".
* Supporting prose that referenced the two ignores was corrected so no lie survives in
  the file: the module-header "Honest limits" bullets (AC-S4, AC-A2, AC-T2), the AC-A2
  (load-half) doc reference to the capture probe, the AC-T2 test's own branch-(c)
  explanation (it no longer claims "`serde_json` refuses a non-finite float"), and the
  `commit()` helper's doc. **No assertion, matcher or expected value was altered**, and
  `grep '^#\[ignore' tests/test_session_restore_panel_abort.rs` now yields 0 hits.

## 6. Before / after test counts

| Suite | Before | After |
|---|---|---|
| `cargo test --test test_session_restore_panel_abort -- --test-threads=1` | 10 tests: **7 passed / 1 failed / 2 ignored** (the failure was `ac_t2_a_…`) | 10 tests: **10 passed / 0 failed / 0 ignored** (run twice, identical) |
| `cargo test --lib session::` | 66 passed | **66 passed** (unchanged) |

## 7. Gate (all green, in order)

1. `cargo check --all-targets` → `Finished dev profile`, no warnings.
2. `cargo test --test test_session_restore_panel_abort -- --test-threads=1` → run 1:
   `10 passed; 0 failed; 0 ignored`; run 2: `10 passed; 0 failed; 0 ignored`.
3. `cargo test --lib session::` → `66 passed; 0 failed; 0 ignored; 882 filtered out`.
4. `cargo clippy --all-targets --all-features -- -D warnings` → exit code **0**.

Extra regression sweeps (not in the gate, all green):
`test_session_commit` 9, `test_session_bundle` 22, `test_session_abort_flush` 6,
`test_session_boot` 6, `test_session_restore_dangling` 12,
`test_session_restore_manager` 8, `test_session_restore_workers` 8,
`test_session_restore_ui` 9, `test_session_config` 35, `test_ui_session` 21 — all
`0 failed`.

The full `cargo test --lib` run (not in the gate) shows 5-6 failures that are
**environment/ordering only and unrelated to this change**: four in
`harness::pty::tests::*` (sandbox denies `openpty`: `Permission denied`) and
1-2 flaky `orchestrator::notice_tests::*` (shared global notice store; they pass when
run alone, and no test in that module reaches `session::commit`).

`cargo fmt` was not run repo-wide (forbidden; the checkout already carries ~211
pre-existing fmt diffs). `cargo fmt -- --check` reports **no** new diff in
`src/session/commit.rs`, and none at any of the edited regions of
`tests/test_session_restore_panel_abort.rs`.

## 8. Files touched

* `src/session/commit.rs` — new `check_time_points` guard + call in `assemble_members`
  + doc updates on `write_bundle`.
* `tests/test_session_restore_panel_abort.rs` — two `#[ignore]` attributes removed and
  their (and only their) attached prose made truthful.
* `docs/cleanup_t035.md` — this note.
