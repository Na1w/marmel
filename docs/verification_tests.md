# t-012 Audit Pass — Bounded Independent Verification (tests/** comment cleanup + Swedish→English)

Scope audited: `git ls-files tests` (17 files) minus `tests/test_tool_name_literals.rs` = **16 in-scope files**
(`test_ui_session.rs` treated separately per check 6). Working tree untouched — no `stash`/`checkout`/edits.
Date: 2026-10-10. Tool rounds used: ~12 (budget ~10; bounded, all 8 checks reached a conclusion).

| # | Check | Command | Exit | Result |
|---|-------|---------|------|--------|
| 1 | rustfmt | `cargo fmt --all -- --check` | **0** | PASS — 0 output lines, formatting clean |
| 2 | clippy | `cargo clippy --all-targets --all-features -- -D warnings` | **0** | PASS — **0 warnings/diagnostics** (output only `Finished dev profile … in 0.30s`; cache-warm tree, `-D warnings` would have turned any diagnostic into a hard error) |
| 3 | UI-session fixtures | `cargo test --test test_ui_session -- --test-threads=1` | **0** | PASS — `running 21 tests` → **21 passed / 0 failed / 0 ignored** (29.40 s) |
| 4 | Harness UTF-8 payloads | `cargo test --test test_harness -- --test-threads=1` | **0** | PASS — `running 15 tests` → **15 passed / 0 failed / 0 ignored** (2.68 s) |
| 5 | Tool-name literals | `cargo test --test test_tool_name_literals` | **0** | PASS — `running 1 test` → **1 passed / 0 failed**. Working-tree diff = exactly **4 added lines**, all in the `BASELINE` table (see below) — no t-012 content |
| 6 | Code-token integrity | string-aware comment strip + token multiset diff vs `git show HEAD:<file>` | n/a | PASS — 15/15 non-`test_ui_session` files: **0 non-string token deltas**; `test_ui_session.rs`: only string literals differ (12 STR out / 12 STR in) |
| 7 | Swedish sweep | `grep -rnP '[åäöÅÄÖ]' tests/` | 0 hits excluded | PASS — **2 surviving lines**, both §5 UTF8-DATA payloads; **0 Swedish prose** |
| 8 | Open question: `test_prompt_based_tool_gating` | `cargo test --test test_role_gating -- --test-threads=1` | **101** | **PRE-EXISTING / DOCUMENTED BASELINE FAILURE — not a t-012 regression** (evidence below) |

## Check 5 detail — `tests/test_tool_name_literals.rs` diff (complete)
```
@@ -61,6 +61,10 @@ const BASELINE: &[(&str, &str, usize)] = &[
+    // OS program name in a spawn fixture (PATH lookup), not a tool identifier.
+    ("src/harness/pty_tests.rs", "sleep", 1),
+    // OS program name in a spawn fixture (PATH lookup), not a tool identifier.
+    ("src/mcp/client.rs", "sleep", 1),
```
Exactly the pre-existing 4 BASELINE rows (2 `sleep` entries + their 2 explanatory comments) owned by
t-014/t-023. No comment-cleanup or Swedish→English hunks in this file → **no t-012 contamination**. PASS.

## Check 6 detail — token integrity
Method: hand-written string/raw-string/char-aware stripper (`//`, nested `/* */`), then multiset comparison of
`ID:`/`NUM:`/`STR:`/`CHR:`/`P:` tokens, whitespace punctuation ignored (line reflow from comment removal + rustfmt).

* Zero-delta files (15/15): `common/mod.rs`, `test_agent.rs`, `test_context.rs`, `test_dynamic_prompts.rs`,
  `test_harness.rs`, `test_llm.rs`, `test_midturn_notice.rs`, `test_monitor.rs`, `test_orchestrator.rs`,
  `test_plan_checkoff.rs`, `test_role_gating.rs`, `test_specialist_schema_charge.rs`,
  `test_specialist_stream.rs`, `test_steer_history.rs`, `test_validation_loop.rs` → **NONSTR_removed=0, NONSTR_added=0, STR delta=0**.
  Byte shrink only (e.g. `test_specialist_stream.rs` 43822→42504, `test_validation_loop.rs` 61459→60848) = deleted comments.
* `tests/test_ui_session.rs`: **12 string literals removed, 12 added, 0 added literals containing å/ä/ö**
  (e.g. `"Hur går det för codern?"` → `"How is the coder doing?"`,
  `"Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar."` →
  `"The coder removed the jit_invalidate_all() calls and ctest passes 7/7."`).
* Single apparent non-string delta was a tokenizer artifact of rustfmt reflow, verified against the raw diff:
  HEAD `assert!(hist[0].1.contains(` + multi-line literal vs worktree `….contains("The coder removed …")`
  (diff hunk lines 52 → 59). No identifier, numeric, or punctuation semantics changed. PASS.

## Check 7 detail — Swedish survivors classified
| Location | Line | Classification |
|---|---|---|
| `tests/test_harness.rs:132` | `let payload = "🦀 åäö αβγ".repeat(30_000); // 540 000 bytes > READ_FILE_MAX_BYTES` | **UTF8-DATA** (inventory §5 row 168; `DO NOT TRANSLATE/DELETE`) — PASS |
| `tests/test_harness.rs:209` | `let payload = "héllo wörld 🦀\n".repeat(100); // 1 400 chars / 1 900 bytes` | **UTF8-DATA** (inventory §5 row 169; pinned by `as_bytes()` round-trip assert) — PASS |

Line numbers are −1 vs inventory (133→132, 210→209) because 13 comment lines were deleted above them.
**Swedish prose remaining in `tests/`: 0.** PASS.

## Check 8 — RESOLVED: pre-existing documented baseline failure
(a) Observed now: exit **101**, `running 25 tests` → `test result: FAILED. 24 passed; 1 failed`, failing test
`test_prompt_based_tool_gating`, `thread 'test_prompt_based_tool_gating' panicked at tests/test_role_gating.rs:312:5:`
(assertion message: `Validator with run_command in prompt must be permitted to execute run_command`).
This matches the documented isolated per-target tally **24 passed / 1 failed** byte-for-byte.

(b) Token stream of `tests/test_role_gating.rs` is **IDENTICAL to HEAD** (check 6: 0 non-string and 0 string token
deltas; worktree diff is 5 deleted comment lines, `git diff --stat` → `5 ------`). A comment-only diff cannot alter
this test's outcome → t-012 is causally excluded.

(c) `docs/baseline_test_run.md` (verbatim rows):
```
151: 11. `test_prompt_based_tool_gating` — `tests/test_role_gating.rs:312:5` — `Validator with run_command in prompt must be permitted to execute run_command`
180: | `test_prompt_based_tool_gating` (`test_role_gating`) | **FAILS deterministically** (fails in every run, incl. isolated per-target run) |
198: **CONFIRMED — `tests/test_role_gating.rs::test_prompt_based_tool_gating` STILL FAILS at baseline.**
201: $ cargo test --test test_role_gating      → exit 101
204: thread 'test_prompt_based_tool_gating' panicked at tests/test_role_gating.rs:312:5:
222: Total sanctioned failures at baseline: **5** (4 × `harness::pty::tests::*` + 1 × `test_prompt_based_tool_gating`).
181: | `fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority` (`test_role_gating`) | **PASSES isolated** (24 passed / 1 failed) → flaky, order-dependent |
```
Same file name in `docs/verification_final.md`: **0 hits** (that doc tracks lib/integration gates generically) —
INCONCLUSIVE as corroboration, immaterial given (a)+(b)+baseline_test_run.md.
**Conclusion: PRE-EXISTING, sanctioned baseline failure (sanctioned #5), NOT a t-012 regression.**

## Notes / non-blocking observations
* 12 of 16 in-scope files changed (`+51 / -121`); `common/mod.rs`, `test_llm.rs`, `test_midturn_notice.rs`,
  `test_steer_history.rs` were already comment-clean (0 deltas) — consistent with a comment-only cleanup.
* Check 2's 0.30 s "Finished" indicates a warm cargo cache; the exit code (0) under `-D warnings` is still
  authoritative for the current working tree fingerprints. Not re-cleaned (would touch the build only, not the tree).
* An earlier detached/background run of check 3 was killed mid-suite (process-group teardown, truncated log);
  check 3 was re-executed in the foreground and completed cleanly (exit 0) — result above is the clean run.

VERDICT: APPROVED
