# t-033 — reconcile the boot-time "pending notices" surfacing (plan Phase 3)

## 0. Shell probe — Branch B taken

The process-spawning tool is dead for this session, exactly as reported. **One probe
only**, as instructed, never re-probed:

```
$ cargo --version
ERROR: Unable to spawn /home/fredrik/.cargo/bin/marmel (deleted) because it doesn't exist
on the filesystem (ENOENT: No such file or directory)
```

Therefore **BRANCH B**: only comment / doc-comment edits, zero executable lines touched.
No `cargo check`, `cargo clippy`, or `cargo test` was run, and none was attempted a second
time (§6). `grep_search` / `read_file` / `glob` all work and are what every claim below is
verified with.

Files touched: `src/ui/session.rs` (3 comment lines), `src/session/worker_journal.rs`
(1 doc-comment block), `docs/cleanup_t033.md` (this file). Nothing else — no attribute,
rename, import, assertion, signature, or blank-line change anywhere.

---

## 1. The comment edits (before → after, with the verification that justified each)

All three anchors were **verified against the real definition line in `src/ui/tui/mod.rs`
before being written**, using `grep_search` on the function definitions:

```
src/ui/tui/mod.rs:1478  fn time_point_from_instant(instant: std::time::Instant) -> …
src/ui/tui/mod.rs:1489  fn subagent_detail_from_panel(panel: &…SubagentPanel) -> SubagentDetail {
src/ui/tui/mod.rs:1581  pub fn token_witness_warnings(
```

### 1.1 `src/ui/session.rs:1294` — comment (line 1293 names the symbol)

before:
```rust
    // **Documented deviation:** `TuiRenderer::token_witness_warnings`
    // (`ui/tui/mod.rs:1547`) is an *inherent* method, so it is not reachable
```
after:
```rust
    // **Documented deviation:** `TuiRenderer::token_witness_warnings`
    // (`ui/tui/mod.rs:1581`) is an *inherent* method, so it is not reachable
```
why: the sentence names `TuiRenderer::token_witness_warnings`, which is `pub fn` at
`src/ui/tui/mod.rs:1581` (not 1547). Stale anchor from a pre-t-032 file growth. The "not
reachable through `&mut dyn Renderer`" claim is still true — 1581 is an inherent (`impl
TuiRenderer`) method, not part of the `Renderer` trait — so only the number moved.

### 1.2 `src/ui/session.rs:1539` — doc comment on the session-side twin

before:
```rust
/// The token-witness comparison behind `TuiRenderer::token_witness_warnings`
/// (`ui/tui/mod.rs:1547`), phrased identically so a report reads the same
```
after:
```rust
/// The token-witness comparison behind `TuiRenderer::token_witness_warnings`
/// (`ui/tui/mod.rs:1581`), phrased identically so a report reads the same
```
why: same symbol as §1.1 (`fn token_witness_warnings` in `ui/session.rs:1542` mirrors the
`TuiRenderer` method at 1581). Line length unchanged (4 chars → 4 chars).

### 1.3 `src/ui/session.rs:1594` — doc comment on `subagent_detail_from_panel`

Verified-before-touch check the brief explicitly asked for: line 1593 reads
``/// [`SubagentDetail`] model. Mirrors `ui::tui::subagent_detail_from_panel` ``, and the
following sentence continues "`… which is private to a file this task does not own: the
session loop needs the same projection …". So the anchor refers to
**`subagent_detail_from_panel`**, which is at `src/ui/tui/mod.rs:1489`. It does **not** refer
to `time_point_from_instant` (which really is at `1478`, exactly as the auditor warned — that
number is simply the wrong one for *this* sentence). No other `ui/tui/mod.rs:NNNN` anchor in
`src/ui/session.rs` mentions either symbol, and the remaining anchors (233 `:1602`,
1280 `:1066-1068`) were left alone — not part of this task and not re-verified.

before:
```rust
/// (`ui/tui/mod.rs:1478`), which is private to a file this task does not own: the
```
after:
```rust
/// (`ui/tui/mod.rs:1489`), which is private to a file this task does not own: the
```
why: the named symbol lives at 1489. `ui/tui/mod.rs:1489` is `fn subagent_detail_from_panel`
— private (no `pub`), which is exactly why `src/ui/session.rs:1597` carries its own copy.

### 1.4 `src/session/worker_journal.rs:1011-1031` — doc comment of `queued_notice_notices`

before (lines 1011-1014, 4 lines):
```rust
/// One `NonRestorableNotice` per queued-but-undelivered notice the previous
/// process held for `worker_key` (flow §2.2 step 3). The loader cannot read the
/// old process's notice store — it does not exist — so the caller (the boot
/// wiring) supplies the count it captured.
```
after (lines 1011-1031, 21 lines):
```rust
/// Synthesises one `NonRestorableNotice` **per unit of `count`** for the
/// queued-but-undelivered notices the previous process held for `worker_key`
/// (flow §2.2 step 3). The loader cannot read the old process's notice store —
/// it does not exist — so the caller (the boot wiring) supplies the count it
/// captured, and what comes back re-spells that count: unlike
/// [`pending_notice_notices`], which recorded one entry per real notice (naming
/// its notice id) into `WorkerSection::non_restorable` at capture time, nothing
/// here names a notice id.
///
/// Current behaviour, stated exactly as the body reads: the range is
/// `(0..count.max(1))`, so `count = 0` still yields **ONE** entry — a loss that
/// was never recorded — and all the entries for one key are byte-identical.
///
/// The rendered output collapses: `push_repair_notice` (`ui/session.rs:1463`)
/// dedups on [`NonRestorableNotice::dedup_key`] = `(kind, subject)` and the
/// subject here is always `worker_key`, so N synthesised notices for one worker
/// key reach the report as **one line per distinct worker key**.
///
/// TODO(t-034): `(0..count.max(1))` → `(0..count)` in the body below, so the
/// `pub` contract ("per notice") holds and a zero count yields nothing, plus
/// the matching guard assertions in this module's `mod tests`.
```
why: the old text asserted "One … **per** queued-but-undelivered notice", which the body
(`(0..count.max(1))`, `worker_journal.rs:1033`) does not deliver for `count = 0`, and it hid
that all N entries share one `(kind, subject)` dedup key. The new block states the three
truths the brief listed: one entry **per queued notice count**, `count = 0` still yields ONE
entry because of `(0..count.max(1))`, and the rendered output dedups to one line per distinct
worker key — plus the `TODO(t-034)` marker. The doc block is `///` lines only; the signature
and body were re-read after the edit and are byte-identical to before.

**Resulting line shift inside `src/session/worker_journal.rs` (+17 for everything below
1014):** `pub fn queued_notice_notices` 1015 → **1032**, its `(0..count.max(1))` body line
1016 → **1033**, `mod tests` 1229 → **1246**, `#[test]` 1704 → **1721**,
`fn restored_workers_come_back_interrupted_and_are_never_relaunched` 1705 → **1722**, the
global-inbox read `crate::orchestrator::notice::worker_inbox_len(key)` 1748 → **1765**.
All re-verified with `grep_search` after the edit. §3 below uses the NEW anchors.

`src/ui/session.rs` line count is unchanged (three equal-length number substitutions).

---

## 2. Verified state of the boot path this task is about (facts for t-034, nothing edited)

`src/ui/session.rs` boot block, verified span (the brief's "1157-1181" is off; the real
block is 1153-1173, with the tally starting at 1157):

```
1153  // The capture-side losses are part of the worker sections: the loader cannot
1154  // read the dead process's notice store, so the boot wiring supplies the
1155  // queued-notice count (flow §2.2 step 3) and merges every other recorded loss.
1156  for section in &session.workers {
1157      let mut queued = 0usize;
1158      for notice in &section.non_restorable {
1159          if notice.kind == NonRestorableKind::PendingNotice {
1160              queued += 1;
1161          } else {
1162              push_repair_notice(&mut repair, notice.clone());
1163          }
1164      }
1165      if queued > 0 {
1166          for notice in crate::session::worker_journal::queued_notice_notices(
1167              &section.worker_key,
1168              queued,
1169          ) {
1170              push_repair_notice(&mut repair, notice);
1171          }
1172      }
1173  }
```

Supporting facts, each grepped, not assumed:

- `pending_notice_notices` — `src/session/worker_journal.rs:995` (body 996-1008); its
  `detail` names `notice.notice_id`, so the per-notice-id record already exists in
  `WorkerSection::non_restorable`.
- capture wiring — `src/session/worker_journal.rs:624`
  `non_restorable.extend(pending_notice_notices(worker_key));` inside
  `project_worker_section` (fn at 591).
- dedup — `push_repair_notice` at `src/ui/session.rs:1462-1466` keys on
  `NonRestorableNotice::dedup_key()` = `(kind.clone(), subject.clone())`,
  `src/session/format/restore.rs:227-229`. First writer wins; later same-key notices are
  dropped.
- only production caller of `queued_notice_notices` = `src/ui/session.rs:1166`, guarded by
  `if queued > 0` (1165) → the `max(1)` inflation never ships today, it is latent on a `pub`
  boundary.
- integration-test uses: `tests/test_session_restore_panel_abort.rs:107` (import) and
  `:2319` (`non_restorable: queued_notice_notices(key, 1)` — count 1, so `(0..count)` changes
  nothing there).
- unit-test fixture: `src/ui/session_tests.rs:583 fn boot_worker(key: &str)` carries ONE
  `PendingNotice` whose `detail` is `format!("{key} had one queued notice when the process
  died")` (`:611-615`), with the comment at `:610` saying boot must turn it into
  `queued_notice_notices`' own sentence — that comment becomes wrong once §3.1 lands.
- existing unit assertion: `src/ui/session_tests.rs:1013-1025` only checks
  `kinds.contains(&expected)` for the six kinds (no count, no detail) → §3.1 breaks nothing.

---

## 3. The Branch-A patch set, ready to apply verbatim by t-034

Line numbers are **post-§1.4** (i.e. they already include the +17 shift in
`worker_journal.rs`).

### 3.1 `src/ui/session.rs:1153-1173` — surface the real per-notice records

before = the 21-line block quoted in §2.

after:
```rust
    // Every capture-side loss lives in the worker section, `PendingNotice`
    // included. `pending_notice_notices` (`session::worker_journal`) already
    // recorded one entry per queued notice, naming its notice id, so boot
    // surfaces those records verbatim instead of re-spelling a count
    // (flow §2.2 step 3). `push_repair_notice` dedups on `(kind, subject)` and
    // every one of those entries carries `subject == worker_key`, so the
    // report says exactly one line per distinct worker key — the detail it
    // carries is the first record captured for that key, notice id included.
    for section in &session.workers {
        for notice in &section.non_restorable {
            push_repair_notice(&mut repair, notice.clone());
        }
    }
```
Effect: the operator-facing line keeps the captured notice id
("`steering notice <id> was queued for <key> at capture time and was never delivered …`")
instead of the generic re-spelling. Notice **counts do not change** (dedup already collapsed
N per-key entries to one before and does after), so no existing count assertion can break.
`NonRestorableKind` stays used in this file (`ToolSchemaCharge` 1179, `HarnessStats` 1191,
`JoinHandle` 1263) → no unused-import risk, and `unused_imports = "deny"` is live
(`Cargo.toml:70`).

### 3.2 The dedup-key decision — **decision: "per distinct worker key"** (option B)

Two ways to keep N notices for one key visible:

- **Option A — vary the subject (e.g. `format!("{worker_key} (notice {id})`)`: BLOCKED.**
  `tests/test_session_restore_panel_abort.rs:2404-2413` asserts
  ```rust
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
  ```
  Any subject mutation turns `strays` non-empty and fails AC-A2. Relaxing that assertion
  would be weakening it — forbidden by this task's rules.
- **Option B (chosen) — keep `subject == worker_key` and state "one line per distinct worker
  key".** No assertion moves; §3.1's comment says it out loud, and §1.4's doc block says it
  for the synthesising sibling.

Optional follow-up if per-notice-id granularity is ever required (NOT part of t-034's minimal
patch, it changes dedup semantics for every notice kind): give `dedup_key` a third component
(`src/session/format/restore.rs:227`) — needs its own task plus a full re-read of
`docs/design_session_restore_data.md` §10.3/§10.4 and of every notice-count assertion.

### 3.3 `src/session/worker_journal.rs:1033` — the `pub` contract fix

before:
```rust
    (0..count.max(1))
```
after:
```rust
    (0..count)
```
why: the doc block (1011-1031) promises one entry per queued notice; `max(1)` fabricates a
loss for `count = 0`. Production caller never passes 0 (§2), integration tests pass 1, so the
change is behaviour-safe; §3.4 pins it.

### 3.4 guard assertions in `src/session/worker_journal.rs` `mod tests` (mod at 1246)

Add a test at the end of `mod tests` (there is no unit test for this function today — a
repo-wide grep for `queued_notice_notices` finds only the definition 1032, the production
caller `src/ui/session.rs:1166`, the two integration uses, and the comment at
`src/ui/session_tests.rs:610`):

```rust
    #[test]
    fn queued_notice_notices_yields_exactly_one_entry_per_count() {
        // `count = 0` must not invent a loss (the old `(0..count.max(1))` did).
        assert!(
            super::queued_notice_notices("coder-t033-zero", 0).is_empty(),
            "a zero count must yield no notice at all"
        );
        assert_eq!(
            super::queued_notice_notices("coder-t033-two", 2).len(),
            2,
            "the `pub` contract is one entry per queued notice"
        );
        for notice in super::queued_notice_notices("coder-t033-two", 2) {
            assert_eq!(notice.kind, NonRestorableKind::PendingNotice);
            assert_eq!(notice.subject, "coder-t033-two");
        }
    }
```
`super::` keeps the test free of a new `use` line (`unused_imports = "deny"`,
`Cargo.toml:70`); the fully-qualified `crate::session::worker_journal::…` form proposed at
`docs/cleanup_t032.md:181-184` works equally.
`NonRestorableKind` is already in scope inside this `mod tests` (it is used by the existing
tests, e.g. the `JoinHandle`/`CancellationToken`/`DropGuard` loop at the interrupted-worker
test), so no import is needed. The `#[test]` attribute here is new-test scaffolding, not a
change to any pre-existing attribute.

### 3.5 stricter unit assertion for §3.1 (no existing assertion is loosened)

In `src/ui/session_tests.rs`, inside
`resume_boot_installs_manager_history_workers_and_panes_in_one_pass`
(`#[test]` 831, fn 832; `let key = "coder-t012-resume";` at 835), immediately after the
six-kind loop that ends at 1025:

```rust
    // t-033: the boot path surfaces the CAPTURED record verbatim — the detail stored in
    // `WorkerSection::non_restorable` is what the operator reads, never a re-spelled
    // count sentence, and exactly one line survives per distinct worker key.
    let pending: Vec<&crate::session::NonRestorableNotice> = report
        .repair
        .notices
        .iter()
        .filter(|notice| notice.kind == NonRestorableKind::PendingNotice)
        .collect();
    assert_eq!(pending.len(), 1, "one line per distinct worker key, got {pending:?}");
    assert_eq!(pending[0].subject, key, "the subject stays the worker key (AC-A2)");
    assert_eq!(
        pending[0].detail,
        format!("{key} had one queued notice when the process died"),
        "the captured per-notice record is surfaced unchanged, got {:?}",
        pending[0].detail
    );
    assert!(
        !pending[0].detail.contains("queue did not survive the restart"),
        "the synthesised `queued_notice_notices` sentence must not replace the captured one"
    );
```
This is a **strict** superset of the pre-existing `kinds.contains(&PendingNotice)` check and
it fails against the current (pre-§3.1) code — i.e. it is the regression guard for exactly
the defect being fixed. Also fix the now-false comment at `src/ui/session_tests.rs:608-611`
(it claims boot turns the entry into `queued_notice_notices`' own sentence).

### 3.6 the missing mutex guard — `src/session/worker_journal.rs:1721-1725`

`restored_workers_come_back_interrupted_and_are_never_relaunched` (`#[test]` 1721, fn 1722)
holds only the **sync** `crate::orchestrator::workers::TEST_WORKERS_MUTEX` (1723-1725) while
reading the global `WORKER_INBOXES` indirectly through
`crate::orchestrator::notice::worker_inbox_len(key)` at **1765** (`assert_eq!(…, 0, "install
must not steer or dispatch anything to the worker")`). A foreign test that posts a notice for
the same key (the integration suite does, `tests/test_session_restore_panel_abort.rs:2328`)
can flip that `== 0` under `--test-threads` > 1.

Insert, directly after the existing `_workers` guard block (1723-1725):
```rust
        let _notices = crate::orchestrator::notice::TEST_NOTICE_MUTEX
            .blocking_lock();
```
Verified facts:
- declaration `src/orchestrator/notice.rs:447-448` — `#[cfg(test)] pub static
  TEST_NOTICE_MUTEX: LazyLock<tokio::sync::Mutex<()>>` (an **async** tokio mutex, hence
  `blocking_lock()` from a non-async `#[test]`);
- visibility: `src/lib.rs:18 pub mod orchestrator;` + `src/orchestrator/mod.rs:16 pub mod
  notice;` → `crate::orchestrator::notice::TEST_NOTICE_MUTEX` resolves from
  `src/session/worker_journal.rs`'s `#[cfg(test)] mod tests`;
- lock order: take it **after** `TEST_WORKERS_MUTEX`, matching the siblings
  `src/orchestrator/workers.rs:1818, 1852, 1881, 1913`.
- **Correction to the brief's premise**: the precedent at `src/orchestrator/workers.rs:1818+`
  is `let _notice_lock = crate::orchestrator::notice::TEST_NOTICE_MUTEX.lock().await;` — those
  are **async** tests, not `blocking_lock()`. A repo-wide grep for `blocking_lock()` finds it
  **nowhere in `src/`**: it appears only in `docs/cleanup_t032.md:224` and
  `docs/verification_session_restore.md:182` (both proposing this very snippet). So
  `blocking_lock()` is the correct tokio API for a sync test but is **unproven in this
  crate's compile** until t-034 runs `cargo check --all-targets`; if it does not resolve (a
  tokio `sync` feature question), fall back to `#[tokio::test]` + `.lock().await` as the
  siblings do, which is the larger, body-wide change.

### 3.7 documentation that goes stale once §3.1 lands (t-034 should update in the same pass)

- `docs/verification_session_restore.md:176` and `:185` ("Boot reads the count … re-spells
  it", "`PendingNotice` tally … kind test at `:1159`, guard `:1165`, call `:1166`") — the
  tally/call lines cease to exist.
- `tests/test_session_restore_panel_abort.rs:2304-2308` — the AC-A2 doc paragraph ("Boot reads
  the count from `section.non_restorable` and re-spells it
  (`src/ui/session.rs:1157-1181`), so this test supplies the queued count through that
  channel") — becomes false; the fixture at `:2319` still works (boot now forwards it
  verbatim), but the sentence must say "surfaces verbatim". `#[test]` for AC-A2 is at 2309.
- the two `#[ignore = "…"]` reasons at `tests/test_session_restore_panel_abort.rs:1503` and
  `:2451` quote the same anchors (`worker_journal.rs:995-1008`, `worker_journal.rs:624`) —
  only touch them if the ignored probe is finally executed.

### 3.8 the commands t-034 must run and report (never run here)

```
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --test test_session_restore_panel_abort -- --test-threads=1
cargo test --test test_session_restore_panel_abort -- --ignored
cargo test --lib session::     # expected 66 (+1 for §3.4 ⇒ 67)
cargo test --lib ui::          # expected 196 (+0; §3.5 adds assertions inside an existing test)
```
No `#[allow]` anywhere; no git commit.

---

## 4. Self-check after editing (how "zero executable lines changed" was verified without a shell)

- `grep_search "ui/tui/mod\.rs:1\d\d\d" src/ui/session.rs` → now reads `233 :1602`,
  `1280 :1066-1068`, `1294 :1581`, `1539 :1581`, `1594 :1489`. Exactly three lines differ
  from before, all comment lines, all four characters wide.
- `read_file src/session/worker_journal.rs` (offset 41100) after the edit: the new `///`
  block, then `pub fn queued_notice_notices(worker_key: &str, count: usize) ->
  Vec<NonRestorableNotice> {` and `(0..count.max(1))` — signature and body byte-identical to
  the pre-edit read (offset 40600 chunk), i.e. §3.3 deliberately **not** applied here.
- `grep_search "TODO\(t-034\)|^pub fn queued_notice_notices" src/session/worker_journal.rs`
  → 1029 / 1032: the TODO is a `///` line; the `pub fn` line is unshifted in content.
- No `#[cfg]`, `#[test]`, `#[ignore]`, `#[allow]`, `use`, or `fn` line was touched.

## 5. Line-count ledger of this pass

| file | change |
| --- | --- |
| `src/ui/session.rs` | 3 comment lines re-anchored; file length unchanged (equal-length substitutions) |
| `src/session/worker_journal.rs` | 1 doc block 4 → 21 lines (+17); body/signature untouched; everything below 1014 shifts +17 |
| `docs/cleanup_t033.md` | new (this file) |

## 6. Counts owed

**Not obtainable in this session.** `cargo check` / `clippy` / `test` were never run (single
failed probe, §0), so the expected `66` (`--lib session::`) and `196` (`--lib ui::`) counts,
the AC-A2 integration run and the `--ignored` run are owed by t-034, which owns §3.
