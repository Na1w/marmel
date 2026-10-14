# Audit report — t-006: harness CORE comment-cleanup + Swedish→English edit set

Auditor: independent (read-only). Baseline: `HEAD = 974b601 "Refactor part 1"`, branch `1.0.0-refactor`.
Scope audited (14 files): `src/harness/{common,fs,fs_tests,mod,plan,pty,pty_tests,sandbox,search,search_tests,sleep,tool_resolution_tests,workspace,dispatch_integrity_tests}.rs`. `src/harness/monitor/**` excluded (different, approved task).

## Summary

| # | Check | Result | Key evidence |
|---|-------|--------|--------------|
| 1 | `cargo fmt --all -- --check` | **PASS** | exit 0, 0 lines output |
| 2 | `cargo clippy --all-targets --all-features -- -D warnings` | **PASS** | exit 0, warning count = 0 |
| 3 | `cargo test --lib -- harness --test-threads=1` | **PASS** (see note) | exit 101; 463 run, 457 pass, 6 fail; the 5 `harness` failures are exactly the sanctioned set |
| 4 | Code-token delta vs `git show HEAD:` | **PASS (attributed)** | 11/14 files = 0; 3 files non-zero, 100% attributable to t-018/t-020/t-024 portability code (see §4) |
| 5 | Swedish prose (`åäöÅÄÖ` + curated word pass) | **PASS** | 3 diacritic survivors = UTF8-DATA whitelist; 7 word hits = English homographs; 0 Swedish prose |
| 6 | UTF-8 fixture byte-identity vs HEAD | **PASS** | md5 of fixture-line content identical in both files (`wäsen` absent, N/A) |
| 7 | Portability regression guard (t-018/t-020/t-024) | **PASS** | `grep -rn '"/bin/' src/harness/` empty (exit 1); all guards present |
| 8 | Tool-name literal guard | **PASS** with 1 spec deviation | 0 added/removed/reworded literals in scope files; guard file *is* modified (attributed to t-018, see §8) |

## Details

**1.** `cargo fmt --all -- --check` → `FMT_EXIT=0`, output file empty (0 lines).

**2.** `cargo clippy --all-targets --all-features -- -D warnings` → `CLIPPY_EXIT=0`; `grep -cE '^(warning|error)'` = **0**. (`-D warnings` would have turned any warning into a hard error.)

**3.** `cargo test --lib -- harness --test-threads=1` → exit 101, `running 463 tests`, `457 passed; 6 failed; 0 ignored; 0 filtered out`.
Failures inside `harness::` (5 — exact match to the sanctioned list):
`harness::pty::tests::test_harness_pty_process_group_kill` (pty_tests.rs:16), `test_interactive_pty_manager_lifecycle` (:121), `test_run_command_parses_timeout_seconds_argument` (:102), `test_run_command_pty_cancelled_by_worker_token` (:183) — all `failed to openpty: Os { code: 13, PermissionDenied }` = `/dev/pts/ptmx` EACCES environment limit; and `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable` (mod.rs:896).
*Note:* libtest reported `0 filtered out`, i.e. the `harness` filter was not applied and the whole lib suite ran. One extra failure therefore appeared: `orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup` (src/orchestrator/notice_tests.rs:258, "SteerResponse emitted for user status: Disconnected") — outside the audited `src/harness/**` CORE scope, in a file owned by another task; not attributable to this edit set.

**4.** Method: strip whole-line comments only (lines whose trimmed content starts with `//`; trailing comments left intact), tokenize (`/tmp/audit_tok.py`), SequenceMatcher diff vs `git show HEAD:<file>`.

| file | HEAD tokens | delta |
|---|---|---|
| common.rs / fs.rs / fs_tests.rs / mod.rs / plan.rs / pty.rs / sandbox.rs / search.rs / sleep.rs / workspace.rs / dispatch_integrity_tests.rs | 898 / 3916 / 1738 / 7569 / 754 / 7600 / 6452 / 2660 / 2913 / 1561 / 2548 | **0** each |
| pty_tests.rs | 5852 | 391 (49 removed / 168 added by multiset diff) |
| search_tests.rs | 3619 | 363 (0 removed / 363 added → pure insertion) |
| tool_resolution_tests.rs | 3915 | 2 (`"/bin/sh"` → `"sh"`) |

Attribution (multiset token diff, so nothing is hidden by ordering noise): every delta token belongs to code introduced by the previously approved portability tasks, which check 7 *requires* to still be present — t-018 (`short_lived_child_spec`, `long_running_child_spec`, `spawn_real_child`, `assert_gone_from_process_table` + `#[cfg(unix)]`/`#[cfg(not(unix))]` pair, `cfg!(windows)`, `"cmd"`/`"/C"`/`"ping -n 30 127.0.0.1"`, `"/bin/true"`+`"/bin/sleep"`→PATH-resolved `"true"`/`"sleep"`, `/usr/local/bin/marmel`→`/usr/local/libexec/marmel`), t-020 (`WINDOWS_PATH_PREFIXES`, `normalize_for_compare`, `normalize_paths` + its guard test), t-024 (`"/bin/sh"`→`"sh"`). Zero tokens can be attributed to comment translation. The literal check-4 rule ("0 unless inside a test-data string literal") is unsatisfiable here because the HEAD baseline predates t-018/t-020/t-024 uncommitted work — repairing it would mean reverting approved code, contradicting check 7.

**5.** `grep -nE "[åäöÅÄÖ]" src/harness/*.rs` (non-recursive, monitor excluded) → 3 survivors, all UTF8-DATA per `docs/research_swedish_inventory.md` §5 ("Never touch the 30 UTF8-DATA lines", which lists `src/harness/pty.rs:980`, `src/harness/pty_tests.rs:744, 750` at HEAD; those lines now sit at the shifted worktree numbers below):
- `src/harness/pty.rs:972` — `` `å`/`é`/`🙂` `` → **UTF8-DATA**
- `src/harness/pty_tests.rs:778` — `` 2-byte lead (`å`, `ä`, `ö`) `` → **UTF8-DATA**
- `src/harness/pty_tests.rs:784` — `` second byte of `ä` `` → **UTF8-DATA**
Swedish word pass (`grep -rniw -E 'och|med|för|att|den|det|som|vid|på|av|en|ett|inte|så|här|där|när|hur|ska|kan|bör|göra|katalog|över|under|mellan|samma|varje|endast'`) → 7 hits, all the English preposition "under": `mod.rs:760`, `pty_tests.rs:331`, `search_tests.rs:44`, `search_tests.rs:730`, `tool_resolution_tests.rs:102`, `:138`, `:695` → **all English, not Swedish prose**. `making` → 0 hits. `use` was deliberately not word-grepped (Rust keyword = pure code noise); no Swedish-prose context found adjacent to any hit. **No Swedish prose remains.**

**6.** Fixture lines matched by `å|ä|ö|é|🙂|漢|wäsen`: `pty.rs` 1 line (WT 972 = HEAD 980), `pty_tests.rs` 6 lines (WT 717, 719, 741, 778, 779, 784 = HEAD 683, 685, 707, 744, 745, 750). Content-only md5 (line numbers stripped) is **identical HEAD vs WT**: `pty.rs 84044909c29a6b9fc7a43010dae09c2d`, `pty_tests.rs 5b35996ac9269efa8ff8c853b9961b96`. `od -c` confirms unchanged byte sequences: `303 245`(å), `303 244`(ä), `303 266`(ö), `303 251`(é), `360 237 231 202`(🙂), `346 274 242`(漢). `wäsen`: 0 occurrences anywhere in `src/harness/` (N/A). Only *line numbers* shifted (comment-line count changes), which is expected.

**7.** `grep -rn '"/bin/' src/harness/` → **empty** (exit 1) — includes monitor. t-018 present in `pty_tests.rs`: `#[cfg(unix)]` at 6/93/112/123/146/263/321/373/429, `#[cfg(not(unix))]` at 265/386, `cfg!(windows)` at 336/348, helpers `short_lived_child_spec` 335, `long_running_child_spec` 347, `spawn_real_child` 358, `assert_gone_from_process_table` 374 (unix) + 387 (not(unix)) with zombie assertions at 403/423. t-020 present in `search_tests.rs`: `normalize_for_compare` 46 (with `if cfg!(unix) { return raw.to_owned() }` at 47), `normalize_paths` 67, guard test `normalize_for_compare_is_identity_on_unix_and_canonical_on_windows` 732 (unix branch 733-742). t-024 present in `tool_resolution_tests.rs:92` `command: Some("sh".to_string())`.

**8.** Counted occurrences of each quoted tool-name literal (`read_file, write_file, replace, run_command, grep_search, glob, rebirth, reply_to_arbitrator, pty_spawn, pty_write, pty_read, pty_close, pty_list, leave_verdict, write_to_file, edit_file, shell, task, todo`) in every one of the 14 scope files, HEAD vs WT → **`TOOLNAME_LITERAL_DIFF = 0`**: none added, deleted, reworded or moved.
Deviation from the stated expectation: `tests/test_tool_name_literals.rs` **is** modified in the working tree (`+4` lines, no deletions) — it registers two new baseline entries for the OS program name `"sleep"` (`("src/harness/pty_tests.rs","sleep",1)` and `("src/mcp/client.rs","sleep",1)`), which is the required companion bookkeeping for the t-018 PATH-resolved `"sleep"` spawn fixture (check 7); without it the guard test would fail. It adds no tool-name literal and touches none of the 14 scope files, so it is not attributable to the comment edit set.

## Non-blocking observations (no repair task required for this edit set)

1. `tests/test_tool_name_literals.rs:+64..+67` — modified guard file (t-018 companion change; spec expected it untouched).
2. `src/orchestrator/notice_tests.rs:258` — `test_handle_reply_to_arbitrator_ask_followup` fails, out of scope (other task/file), surfaced only because the `harness` name filter was not applied by libtest (`0 filtered out`).
3. `docs/research_swedish_inventory.md` §5 whitelist line numbers for the harness UTF8-DATA lines are HEAD-era (980 / 744 / 750) and now read 972 / 778 / 784; content is byte-identical.

VERDICT: APPROVED
