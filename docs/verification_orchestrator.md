# Verification Report — t-010 (`src/orchestrator/**` comment cleanup + fixture translation)

| Field | Value |
|---|---|
| Audit task | **t-027** (bounded, READ-ONLY audit of completed t-010 work) |
| Workspace | `/home/fredrik/marmel/wip` (branch `1.0.0-refactor`, HEAD `974b601` "Refactor part 1") |
| Scope audited | `src/orchestrator/{bus,delegation,mod,notice,notice_tests,preemption,registry,steer,steer_extractor,steer_tests,tests,workers}.rs` + untouched-claim check on `delegate.rs`, `plan_summary.rs` |
| Audit writes | **zero** source writes — only this file (`docs/verification_orchestrator.md`) plus scratch under `/tmp`. `git status --porcelain` diff-of-diffs confirmed unchanged source state before/after the audit. |
| Baseline reference | `docs/baseline_test_run.md` (serial lib floor), `docs/verification_final.md` |

## Method note (how "comment/fixture-only" was proven mechanically)

Instead of eyeballing hunks, every in-scope file was tokenised and compared HEAD vs worktree with
(1) all comments removed (line, doc `///`, inner-doc `//!`, nested block comments), and
(2) all string-literal **contents** blanked to `"STR"` (raw strings `r#"…"#` handled).
Any surviving token difference would be an executable-logic change (category (c)). Result: **0 code-token
deltas across all 14 files**. String-literal *contents* were then diffed separately and located by test-fixture
hunk header, so every change is provably category (a) or (b).

---

## PASS/FAIL table

| # | Check | Result | Evidence (trimmed) |
|---|---|---|---|
| 1 | Diff is comment/fixture-only (no category-(c) logic line) | **PASS** | 0 code-token deltas in all 14 in-scope files; 77 added / 162 removed lines, all comments or test-fixture string contents |
| 2 | Guard 1 — tool-name literal test + no literal added/removed | **PASS** | `test_no_unexpected_raw_tool_name_literals_in_src ... ok` (1 passed / 0 failed); per-file quoted-literal counts identical HEAD vs worktree |
| 3 | Guard 2 — single-owner needle | **PASS** | `grep -rn 'Steering Notice from Arbitrator' src/` → exactly 1 hit: `src/orchestrator/notice.rs:1070` |
| 4 | Guard 3 — source self-scanning tests | **PASS** | `cargo test --lib orchestrator -- --test-threads=1` → `151 passed; 0 failed`; both `walk_src_for` guards ok |
| 5 | Deterministic gate set (7 commands, serial) | **PASS** | exit codes 0,0,0,0,0,0,0 |
| 6 | Full serial floor `cargo test --lib -- --test-threads=1` | **PASS** | `807 passed; 5 failed` — failure set is exactly the 5 expected tests |
| 7 | No Swedish prose left in scope | **PASS** | 1 surviving `åäö` hit — legitimate in-prompt example data (unchanged since HEAD) |
| 8 | Comment-cleanup quality spot check | **PASS (with notes)** | REQ-*/t-NNN/H1–H6/M8 tag multiset: **zero delta**; no UTF-8/Landlock/sandbox invariant was removed; 9 dead external-contract section pointers removed from `steer.rs` — see Notes |

---

## 1. Diff is comment/fixture-only — PASS

`git diff --stat -- src/orchestrator` (verbatim):

```
 src/orchestrator/bus.rs             | 11 +----
 src/orchestrator/delegation.rs      |  3 --
 src/orchestrator/mod.rs             | 16 +++----
 src/orchestrator/notice.rs          | 14 +-----
 src/orchestrator/notice_tests.rs    | 86 +++++++++++++++----------------------
 src/orchestrator/preemption.rs      | 13 +-----
 src/orchestrator/registry.rs        |  2 -
 src/orchestrator/steer.rs           | 27 ++++++------
 src/orchestrator/steer_extractor.rs |  2 -
 src/orchestrator/steer_tests.rs     |  9 +---
 src/orchestrator/tests.rs           | 38 ++++++----------
 src/orchestrator/workers.rs         | 18 ++------
 12 files changed, 77 insertions(+), 162 deletions(-)
```

Per-file classification (borders = removed `// -----` rule lines that bracketed an already-existing
section title; the title itself is retained):

| File | +/- | rule-line removals | fixture-string rewrites | code-token deltas (strings blanked) |
|---|---|---|---|---|
| bus.rs | +2 / −9 | 0 | 0 | **0** |
| delegation.rs | +0 / −3 | 0 | 0 | **0** |
| mod.rs | +6 / −10 | 0 | 0 | **0** |
| notice.rs | +2 / −12 | 4 | 0 | **0** |
| notice_tests.rs | +35 / −51 | 6 | 24 | **0** |
| preemption.rs | +2 / −11 | 0 | 0 | **0** |
| registry.rs | +0 / −2 | 0 | 0 | **0** |
| steer.rs | +13 / −14 | 0 | 0 | **0** |
| steer_extractor.rs | +0 / −2 | 0 | 0 | **0** |
| steer_tests.rs | +2 / −7 | 0 | 0 | **0** |
| tests.rs | +12 / −26 | 0 | 0 | **0** |
| workers.rs | +3 / −15 | 10 | 0 | **0** |
| delegate.rs (untouched claim) | — | — | — | **0** (absent from `git status`) |
| plan_summary.rs (untouched claim) | — | — | — | **0** (absent from `git status`) |

Category tally of the 162 removed lines: 22 section-rule lines, 24 Swedish fixture strings,
42 doc-comments (`///`/`//!`), 74 inline comments; 77 added lines: 0 rule lines, 33 fixture strings,
44 comments. **Zero category-(c) lines exist** — i.e. no executable statement, expression, attribute,
signature, match arm or macro call was added, removed or altered anywhere in scope.

Category (b) proof — every string-content change lives inside a `#[test]`/`#[tokio::test]` body
(hunk headers from `git diff -U0` name the enclosing `async fn test_*`). Full set of string changes,
all Swedish→English, none outside `notice_tests.rs`:

```
"Coder bekräftar att async I/O har implementerats."   -> "Coder confirms that async I/O has been implemented."
"Ställer följdfråga om bakåtkompatibilitet..."        -> "Asking a follow-up about backward compatibility..."
"Coder har fixat PPC JIT."                           -> "Coder has fixed the PPC JIT."
"Debugger har isolerat minnesläckan."                 -> "Debugger has isolated the memory leak."
"Vad gör codern?" / "Vad gör debuggern?"              -> "What is the coder doing?" / "What is the debugger doing?"
"Minnesläckan berodde på en saknad free i malloc-wrapper." -> "The memory leak was caused by a missing free in the malloc wrapper."
"malloc-wrapper"                                     -> "malloc wrapper"          (assertion substring kept in sync)
"Implementerat JIT-instruktionerna för PowerPC 604e." -> "Implemented the JIT instructions for PowerPC 604e."
"Coder förklarar att SQLite valdes för att minimera externa beroenden." -> "Coder explains that SQLite was chosen to minimize external dependencies."
"Varför valde du SQLite?"                            -> "Why did you choose SQLite?"
"SQLite har noll konfiguration och fungerar utmärkt med WAL-mode." -> "SQLite has zero configuration and works very well with WAL mode."
"Ställer en följdfråga till Coder gällande transaktionsisolering..." -> "Asking Coder a follow-up about transaction isolation..."
"Kan vi byta till SQLite?" / "Jag kan byta databas." -> "Can we switch to SQLite?" / "I can switch the database."
```

The fixture↔assertion pairs were translated together (e.g. `"malloc-wrapper"` in the mock JSON and the
matching `contains("malloc wrapper")` assertion both changed), so no assertion was weakened, dropped or
tautologised — `notice_tests.rs` string-literal count is identical HEAD vs worktree.

Scope observation (informational, not a defect): the worktree also carries modifications to 33 files
outside `src/orchestrator` (other tasks in flight, e.g. `src/harness/pty_tests.rs`, `src/mcp/client.rs`)
and `tests/test_tool_name_literals.rs` (+4 baseline entries for `sleep` spawn fixtures). Those are outside
the t-010 claim and were excluded from classification; they do not affect any in-scope conclusion.

## 2. Guard 1 — tool-name literals — PASS

```
$ cargo test --test test_tool_name_literals
running 1 test
test test_no_unexpected_raw_tool_name_literals_in_src ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.21s
EXIT=0
```

`grep -c '"delegate_task"\|"read_file"\|"grep_search"' src/orchestrator/*.rs` — HEAD vs worktree:

```
bus.rs 0/0   delegate.rs 0/0   delegation.rs 0/0   mod.rs 0/0   notice.rs 0/0
notice_tests.rs 0/0   plan_summary.rs 0/0   preemption.rs 0/0   registry.rs 0/0
steer.rs 0/0   steer_extractor.rs 0/0   steer_tests.rs 1/1   tests.rs 0/0   workers.rs 0/0
```

No quoted tool-name literal was added or removed (the single `steer_tests.rs` baseline entry —
the `normalize_steer_decision` input at `tests/test_tool_name_literals.rs:66` — is unchanged).
The blanked-string diff independently confirms **no string literal of any kind was added or removed**
(literal counts identical per file).

## 3. Guard 2 — single-owner needle — PASS

```
$ grep -rn 'Steering Notice from Arbitrator' src/
src/orchestrator/notice.rs:1070:        "[Steering Notice from Arbitrator — ID: {notice_id}]:\n\"{inquiry}\"\n\n\
```

Exactly **one** hit, in `src/orchestrator/notice.rs` (`render_notice_for_worker`). The self-scanning guard
that enforces this (`orchestrator::tests::test_notice_transcript_is_rendered_in_exactly_one_place`,
`src/orchestrator/tests.rs:887`) passes in both the filtered and full serial runs.

## 4. Guard 3 — source self-scanning tests — PASS

```
$ cargo test --lib orchestrator -- --test-threads=1
running 151 tests
...
test result: ok. 151 passed; 0 failed; 0 ignored; 0 measured; 661 filtered out; finished in 13.92s
EXIT=0
```

Specific guards (task brief cited ~904–926 / ~1106–1176; actual current locations):

| Guard | Location | Result |
|---|---|---|
| `orchestrator::tests::test_notice_transcript_is_rendered_in_exactly_one_place` | `src/orchestrator/tests.rs:887-912` (needle assembled at runtime: `format!("[Steering {} Arbitrator", "Notice from")`) | `ok` (filtered run **and** full serial run) |
| `walk_src_for` helper | `src/orchestrator/tests.rs:914-928` | exercised by the guard above; unchanged by the diff |
| `orchestrator::steer_tests::steer_production_region_retypes_no_sleep_default_or_clamp_literals` | `src/orchestrator/steer_tests.rs:1109-1174` (`include_str!("steer.rs")`, comment lines excluded, needles runtime-assembled) | `ok` (filtered run **and** full serial run) |
| `orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup` (parallel-isolation flake in baseline) | `src/orchestrator/notice_tests.rs` | `ok` in serial |

The `steer.rs` module-doc/fallback-comment rewrites in this task sit inside regions that the guard
explicitly filters out as comment lines, and no new needle text was introduced on a code line — verified
by the guard passing.

## 5. Deterministic gate set (run exactly these, serially) — PASS

| # | Command | Exit code | Pass/fail counts |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | **0** | no diff emitted |
| 2 | `cargo build` | **0** | `Finished \`dev\` profile [unoptimized + debuginfo] target(s) in 0.28s` |
| 3 | `cargo clippy --all-targets --all-features -- -D warnings` | **0** | `Finished \`dev\` profile ... in 0.29s` (zero warnings) |
| 4 | `cargo test --lib orchestrator -- --test-threads=1` | **0** | `151 passed; 0 failed; 0 ignored; 0 measured; 661 filtered out` |
| 5 | `cargo test --test test_tool_name_literals` | **0** | `1 passed; 0 failed` |
| 6 | `cargo test --test test_orchestrator` | **0** | `8 passed; 0 failed` |
| 7 | `cargo test --test test_steer_history` | **0** | `9 passed; 0 failed` |

Verbatim integration-test names (gate 6/7) confirming orchestrator behaviour is still certified:

```
tests/test_orchestrator.rs: test_check_off_complete_flips ... ok
  test_depth_gate_allows_within_bound ... ok
  test_depth_gate_rejection_emits_no_started_event ... ok
  test_depth_gate_rejects_at_bound_regardless_of_grant ... ok
  test_delegation_events_started_then_completed ... ok
  test_handle_delegate_task_rejection_anchoring ... ok
  test_handle_delegate_task_synchronous_inside_async_context ... ok
  test_orchestrator_abort_signal_lifecycle_and_cancellation ... ok
tests/test_steer_history.rs: 9 passed, 0 failed (incl. bus_correlation_keeps_no_substring_fallback,
  notice_id_is_matched_as_a_whole_token_not_as_a_prefix, unknown_notice_id_appends_instead_of_hijacking…)
```

## 6. Full serial floor — PASS

```
$ cargo test --lib -- --test-threads=1
...
failures:
    harness::pty::tests::test_harness_pty_process_group_kill
    harness::pty::tests::test_interactive_pty_manager_lifecycle
    harness::pty::tests::test_run_command_parses_timeout_seconds_argument
    harness::pty::tests::test_run_command_pty_cancelled_by_worker_token
    harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable

test result: FAILED. 807 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 83.37s
EXIT=101
```

`807 passed; 5 failed` = expected floor, and the failure set is **exactly** the five sanctioned
environment/pre-existing failures (`docs/baseline_test_run.md:138-142`). **Zero additional serial
failures**; no `orchestrator::*` test fails.

## 7. No Swedish prose left in scope — PASS

```
$ grep -rnE "[åäöÅÄÖ]" src/orchestrator/
src/orchestrator/notice.rs:1134:   - In 'user_status', provide a brief status notice in the user's language (e.g. 'Ställer en följdfråga till Coder för att reda ut detaljerna...').
```

Judgement of the single hit: it is **production prompt text** (the steer-arbitrator reply contract) whose
purpose is to instruct the model to answer in the *user's* language; the Swedish clause is the illustrative
payload, i.e. legitimate UTF-8 data, not Swedish prose narration. It is byte-identical at HEAD (not touched
by t-010), and removing it would change production prompt behaviour — correctly left alone. HEAD had 25 such
lines (`notice.rs` ×1 + `notice_tests.rs` ×24); all 24 test-fixture lines were translated.

## 8. Comment-cleanup quality spot check — PASS (with notes)

Sample coverage: the task asked for ~40 removed-comment hunks; **all 162 removed lines / 77 added lines in
scope were reviewed** (100 % superset), plus an automated traceability-tag multiset diff and a safety-keyword
sweep.

**Traceability tags — HEAD vs worktree (scope-wide multiset, 14 files):**

| Tag family | HEAD | Worktree | Delta |
|---|---|---|---|
| `REQ-ORCH-*`, `REQ-PLAN-*` | present | present | **0** |
| `t-NNN` (incl. `t-031h`, `t-048`, `t-055`, `t-065`, `t-068`) | present | present | **0** |
| `H1`–`H6`, `M8` | present | present | **0** |
| `REFERENCE_ORCHESTRATION_CONTRACT.md §5` | 1 | 0 | **−1** |
| `caesar §5.1` / `caesar §5.3` | 1 / 6 | 0 / 0 | **−7** |

Every `REQ-xxx`/`t-NNN`/`Hn`/`M8` gate marker survived. The section-header rewrites kept the full gate prose
and only dropped the ASCII box rules, e.g. `src/orchestrator/tests.rs:428-432` still reads

```
// M8: the Silent Dispatcher must never read "the plan could not be read" as
// "the mission is finished" (`docs/recon_bugs_manager.md` M8). The gate now uses
// the fallible plan API added in t-031h (`Plan::try_pending_tasks`), so a read
// failure is an Err, a successfully-read empty pending list is the only
// completion signal, and dispatch of real pending tasks is unchanged.
```

and `workers.rs:1675-1683` (Gate t-055 body) and `tests.rs:930-935` (Gate t-055) retain their full rationale text.

**Safety-invariant survival audit (every removal that could carry one):**

| Removed line (HEAD location) | Where the invariant survives | Verdict |
|---|---|---|
| `/// Intentionally a swap (not a once-only OnceLock)… The write guard is held only for the swap itself…` (`bus.rs:85-89`) | Canonical copy kept verbatim on `set_status_sender` (`bus.rs:72-76`); `set_event_sender` now points at it: `/// Same swap pattern as [`set_status_sender`].` (`bus.rs:85`) | dedup — OK |
| `/// Contention-free pattern: clone the sender out under the read lock, drop the guard, and only then perform the channel send with NO lock held. The send must never happen while the guard is alive…` (`bus.rs:111-115`) | Canonical copy kept verbatim on `emit_status` (`bus.rs:94-98`); `emit_event` → `/// Same contention-free pattern as [`emit_status`].` (`bus.rs:107`) | dedup — OK |
| `//  … mirrors caesar's `Orchestrator` gate (tools_manager.rs:914) which is UNCONDITIONAL — the bound is enforced regardless of `recursion_granted` … must not emit a spurious lifecycle event).` (`mod.rs:207-213`) | Same sentence retained in `mod.rs:205-210`; only the dead external file:line pointer dropped; `REQ-ORCH-001` kept | OK |
| `/// Callers that should adopt the strict form: `src/harness/mod.rs:516` (`record_worker_reply(caller, notice_id, message)`) — and the `Err` branch right below it…` (`notice.rs:1035-1038`) | Replaced by the accurate symbolic target `crate::harness::handle_reply_to_arbitrator_async` (`notice.rs:1027-1028`); verified that function exists (`src/harness/mod.rs:777`) **and** branches on `rejection.kind()` (`src/harness/mod.rs:810`); the removed `src/harness/mod.rs:516` call site no longer exists (`grep -n "record_worker_reply(" src/harness/mod.rs` → no hit) | stale pointer fixed — OK |
| `/// Their ids (sampled).` / `/// Full-visibility drain: …` (`notice.rs:651`, `687`) | Field docs remain on `NoticeDrainOutcome` (`expired_ids`, `expired_dropped`, `reclaimed_entries`) | OK (minor doc thinning) |
| `/// … (`workers.rs:165` pre-fix), a near-constant reading (measured 202/33/41/39/40). They must get distinct, monotonic ids and BOTH must be reachable by the cancel path.` (`workers.rs:1025`) | Full invariant text kept at `workers.rs:1017-1019`; only the self-file line pointer removed | OK |
| `// The deliverable's content must reference the isolated role/brief and never contain any "Manager transcript"…` / `// …prove the isolation invariant at the message level.` / `// Exactly two messages…` (`tests.rs:49-50`, `52-53`, `56`) | Invariant still stated twice upstream: `REQ-ORCH-003` doc block (`tests.rs:24-28`) and inline comment (`tests.rs:35-36`); assertion message `"isolated context has exactly 2 messages"` retained | OK (duplicate test narration removed) |
| `// Before rebirth: large context` / `// After rebirth or compaction: context count drops` and ~15 similar test step comments (`tests.rs`, `preemption.rs`, `steer_tests.rs`, `workers.rs`, `notice_tests.rs`) | Each restates the immediately following statement; test names carry the intent (e.g. `test_each_preemption_ack_is_paired_with_its_own_stream`) | OK — matches "comments must not restate code" |
| UTF-8 char-boundary comments, Landlock / sandbox fail-closed rationale | **None exist in this scope** at HEAD or in the worktree (`grep -hoiE "utf-8|landlock|sandbox|fail.?closed"` → 0 hits in both). No such comment was removed. | n/a |

**Newly introduced comments — restatement check:** the only genuinely new comment text is
1. `/// Same swap pattern as [`set_status_sender`].` (`bus.rs:85`)
2. `/// Same contention-free pattern as [`emit_status`].` (`bus.rs:107`)
3. `//! [`arbitrate_steer_context_stream`] is the entry point the arbitration path drives; [`arbitrate_steer_stream`] / [`arbitrate_steer`] are its wrappers.` (`steer.rs:7-8`)
4. the re-wrapped retentions listed above.

None restates code; all three cross-references resolve to real items (`steer.rs:134`, `546`, `573`;
`bus.rs:77`, `99`) — no hallucinated doc links. The 20 box-rule removals reduced line noise without
losing any heading text.

### Notes / non-blocking observations
1. **`steer.rs` lost its external-contract section pointers.** HEAD carried `REFERENCE_ORCHESTRATION_CONTRACT.md §5`,
   `caesar §5.1` and `caesar §5.3` ×6 (`steer.rs:3-4,10,67,73,76,595,608,623` at HEAD); all 9 are gone.
   They are **not** `REQ-xxx`/`t-NNN` tags, and the referenced artifacts do not exist in this repository
   (`docs/research_comments_ui_tests.md:212` already recorded `REFERENCE_ORCHESTRATION_CONTRACT.md` as
   "file absent from repo / stale"), so this is defensible dead-pointer cleanup — but it does remove the
   only markers tying each fallback branch to the reference contract. Residual lineage survives
   (`caesar \`SteerDecisionResponse\` shape` at `steer.rs:6`). Follow-up suggestion (not a fix performed here):
   re-anchor those 6 fallback docs to an in-repo contract doc.
2. `tests/test_orchestrator.rs:6` still cites `wip/REFERENCE_ORCHESTRATION_CONTRACT.md`; untouched by t-010
   (outside scope) and now the last such stale citation — cosmetic backlog item.
3. `docs/baseline_test_run.md:229` quotes a serial floor of `806 passed / 5 failed`; the current tree yields
   `807 / 5` (one extra passing test from a different task). The t-027 acceptance floor (`807/5`) is met.

---

## Violations

**None.** No category-(c) executable line exists in the diff, no guard regressed, no `REQ-xxx`/`t-NNN`
traceability or safety/UTF-8/Landlock invariant was deleted, and no new comment restates code.

VERDICT: APPROVED
