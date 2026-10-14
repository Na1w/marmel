# Static audit t-030 — tool-name constants + notice plumbing (read-only, no compiler)

## 0. Method and limits

- Method: read-only inspection only — `grep_search`, `glob`, and `read_file` in windows ≤4000 characters.
  Zero edits to source, zero build commands, zero test runs.
- **"Compiles cleanly" is a static claim, not a compile verdict.** Nothing in this document was produced by a
  compiler, and none was run. It records what a reader can establish from the text of the files: declaration
  sites, import groups, use sites, signatures, struct fields and arity.
- **t-014f / t-034 own the real gates.** The authoritative compile/test/format verdicts belong to those tasks;
  this document is the on-disk record of the t-030 read-only pass (its findings had never been written down).
- Scope: six audited files — `src/agents/runner/fix_loop.rs`, `src/session/worker_journal.rs`,
  `src/session/restore.rs`, `src/orchestrator/notice.rs`, `src/tool_names.rs`, plus the notice-facing part of
  `src/ui/session.rs`.

## 1. Ground truth

- The tool-name module is `src/tool_names.rs`, declared `pub mod tool_names;` at `src/lib.rs:38`
  (re-verified), so `crate::tool_names::*` is reachable from every lib module and from their
  `#[cfg(test)] mod tests` blocks.
- `Cargo.toml`: crate `marmennill`, `edition = "2024"` (lines 1-4). `[lints.rust]` at line 67 with
  `unused = "deny"` (68), `dead_code = "deny"` (69), `unused_imports = "deny"` (70), and `[lints.clippy]`
  `all = { level = "deny", priority = -1 }` (72-73). An unused import is therefore a **hard compile error**,
  which is why every import group was checked against its use sites rather than sampled.

## 2. Audited sites (verified line numbers, replacing the earlier stale notes)

- `src/agents/runner/fix_loop.rs` — single test module `mod tests` at **1299** (runs to 2436).
  - Import group 1300-1307; tool-name line at **1306** `{TOOL_REPLY_TO_ARBITRATOR, TOOL_RUN_COMMAND}`,
    used at **1349**, **1396**, **1511** — both imports used.
  - Second group 1531-1546; tool-name line at **1540** `{TOOL_LEAVE_VERDICT, TOOL_READ_FILE, TOOL_REBIRTH,
    TOOL_WRITE_FILE}`, used at **1650**, **1664**, **1885**, **1924**, **1929**, **1968** — all four used.
  - Third group 2062-2068 (e.g. `CompactionOutcome` at 2068) — checked for overlap with the tool names.
  - **Zero name collisions across the three groups (no E0252).** Note `TOOL_ERROR_PREFIX: &str = "ERROR: "` at
    `fix_loop.rs:113` is *not* a tool name and is not part of the swap surface.
- `src/session/worker_journal.rs` — test-module tool-name import at **1232**
  (`use crate::tool_names::TOOL_RUN_COMMAND;`), use sites **1267** (`name:`) and **1826**
  (`function_name:`). The earlier notes said 1198 / 1233 / 1792 — **those were stale**; 1232 / 1267 / 1826 are
  the verified positions.
- `src/session/restore.rs` — single test module `mod tests` starting **1851** (through EOF), tool-name import
  at **1857** `{TOOL_GLOB, TOOL_READ_FILE, TOOL_REPLACE, TOOL_RUN_COMMAND}`. Use sites: GLOB 2095/2267,
  REPLACE 2094/2192, RUN_COMMAND 2045/2077/2096/2489/2534/2594, READ_FILE 1997/2093/2189/2267/2291/2331/2424/
  2436/2461/2513/2540 — **all four imports used**. Test helpers accept `&'static str` arguments, which is what
  makes the constant swap type-compatible: `call(id: &str, name: &str, arguments: &str)` at **1879**, `asst` at
  **1890**, `tool` at **1898**. `DanglingCall` literals at **2487-2491** and **2511-2515** (the swapped
  `function_name` lines are 2489 and 2513) carry all four fields (`tool_call_id`, `function_name`, `arguments`,
  `dispatched` — `src/session/format/worker.rs:162-168`). `dispatched_dangling` exists at **`restore.rs:1728`**.
  `RepairReport.normalization_passes: u8` at `src/session/format/restore.rs:152` and `NormalizeReport.passes: u8`
  at **`:269`** both exist. Repo-wide grep for the swapped tool-name string literals in the three swapped files
  returns **0 leftovers**.
- Path note: the `format/…` files cited below live under `src/session/format/`, not `src/format/`
  (`src/session/format/restore.rs`, `src/session/format/worker.rs`). Line numbers are as stated.

## 3. Constant inventory (`src/tool_names.rs`) — name → exact value

| const | line | value |
|---|---|---|
| TOOL_DELEGATE_TASK | 12 | delegate_task |
| TOOL_READ_FILE | 14 | read_file |
| TOOL_WRITE_FILE | 16 | write_file |
| TOOL_REPLACE | 18 | replace |
| TOOL_RUN_COMMAND | 20 | run_command |
| TOOL_GREP_SEARCH | 22 | grep_search |
| TOOL_GLOB | 24 | glob |
| TOOL_CREATE_PLAN | 26 | create_plan |
| TOOL_ARCHIVE_PLAN | 28 | archive_current_plan  ← name ≠ value stem |
| TOOL_REBIRTH | 30 | rebirth |
| TOOL_PTY_SPAWN | 32 | pty_spawn |
| TOOL_PTY_WRITE | 34 | pty_write |
| TOOL_PTY_READ | 36 | pty_read |
| TOOL_PTY_CLOSE | 38 | pty_close |
| TOOL_PTY_LIST | 40 | pty_list |
| TOOL_LEAVE_VERDICT | 42 | leave_verdict |
| TOOL_SLEEP | 44 | sleep |
| TOOL_REPLY_TO_ARBITRATOR | 46 | reply_to_arbitrator |
| TERMINAL_READ_FILE | 49 | terminal__read_file |
| TERMINAL_WRITE_FILE | 51 | terminal__write_file |
| TERMINAL_REPLACE | 53 | terminal__replace |
| TERMINAL_RUN_COMMAND | 55 | terminal__run_command |
| TERMINAL_GREP_SEARCH | 57 | terminal__grep_search |
| TERMINAL_GLOB | 59 | terminal__glob |
| TERMINAL_LIST_DIRECTORY | 69 | terminal__list_directory (no dispatcher handler; absent from CANONICAL_TOOL_NAMES) |
| TOOL_LIST_DIRECTORY | 72 | list_directory (no dispatcher handler) |
| TERMINAL_SLEEP | 74 | terminal__sleep |
| TERMINAL_LEAVE_VERDICT | 76 | terminal__leave_verdict |
| TERMINAL_PREFIX | 81 | terminal__ |

Every row above was re-read from the file; the names whose stem differs from the value
(`TOOL_ARCHIVE_PLAN`, and all `TERMINAL_*`) are the swap-risk surface.

Non-`&str` items in the same module: `CANONICAL_TOOL_NAMES` (**115**), `TOOL_ALIAS_TABLE` (**157**),
`LEAVE_VERDICT_NAME_SUFFIXES` (**309**, value `["__leave_verdict", "_leave_verdict"]`); functions
`terminal_tool` **85**, `is_canonical_tool_name` **223**, `normalize_tool_alias` **232**,
`canonical_tool_spelling` **244**, `tool_spellings_for` **251**, `is_sleep_tool_name` **269**,
`fold_tool_spelling` **276**, `is_sleep_tool_grammar_spelling` **287**, `is_leave_verdict_tool_name` **315**.
The module comment at 61-63 states explicitly that the `list_directory` pair has **no built-in handler** and is
deliberately excluded from `CANONICAL_TOOL_NAMES` and `TOOL_ALIAS_TABLE`, so the dispatcher claims neither.

## 4. Notice plumbing verdicts

- `src/orchestrator/notice.rs:285-292` — `pub fn peek_worker_inbox(worker_key: &str) -> Vec<SteerNotice>`.
  Its key normalisation `worker_key.trim().to_ascii_lowercase()` (line 286) is **byte-identical** to that of
  `worker_inbox_len` (fn at 259, normalisation 260) and `has_worker_inbox` (fn at 271, normalisation 272), so
  the three accessors address the same inbox. `WORKER_INBOXES` is a `LazyLock<DashMap<String, Inbox>>` (98) and
  `struct Inbox { notices: Vec<SteerNotice>, .. }` at **106-107**. Returning `Vec<SteerNotice>` by value
  requires `SteerNotice: Clone`, which is satisfied (`#[derive(Debug, Clone, ..)]` at **60**, `pub struct
  SteerNotice` at **61**). Visibility from `src/session/*` is real: `lib.rs:18 pub mod orchestrator;` plus
  `orchestrator/mod.rs:16 pub mod notice;`, so `dead_code = deny` cannot fire on these accessors.
- `src/session/worker_journal.rs:995-1008` — `pub fn pending_notice_notices(worker_key: &str) ->
  Vec<NonRestorableNotice>`; it maps each captured notice to `NonRestorableKind::PendingNotice` (999) with a
  **per-notice-id** detail (names `notice.notice_id`). Call site at **`:624`**,
  `non_restorable.extend(pending_notice_notices(worker_key));`, inside
  `pub fn project_worker_section(worker_key: &str, …)` (**`:591`**) — types line up with
  `WorkerSection::non_restorable: Vec<NonRestorableNotice>` (`src/session/format/worker.rs:90`).
  Sibling `queued_notice_notices(worker_key, count)` at **`:1015`** is used at `src/ui/session.rs:1166`.
- `NonRestorableNotice { kind, subject, detail }` at `src/session/format/restore.rs:190`;
  `enum NonRestorableKind` at **`:200`**, containing `CancellationToken` (203), `PtyProcess` (208),
  `PendingNotice` (213), `HarnessStats` (215) among others; dedup on `(kind, subject)` via
  `dedup_key() -> (NonRestorableKind, String)` at **`:227`** (block 225-229), consumed at
  `restore.rs:1744-1745`, `worker_journal.rs:704-705`, `ui/session.rs:1463-1464`.

## 5. Confirmed NON-COMPILE defects found by the audit (owned by t-032/t-033)

1. `tests/test_session_restore_panel_abort.rs:2432` still carries
   `#[ignore = "known gap AC-A2: the capture side never samples PENDING_NOTICES/WORKER_INBOXES … never call
   notice::worker_inbox_len …"]`, a claim the new capture path (`worker_journal.rs:624`, and the test file's own
   `notice::worker_inbox_len` reads at 2316/2444/2651) has made **false**; the doc comment immediately above it
   (block ending at 2431, false claim sentence at 2429) is likewise false. The attribute and the doc are both
   stale, not the code.
2. `src/ui/session.rs:1157-1181` counts capture-side `PendingNotice` entries (test at 1159) and re-spells them
   through `queued_notice_notices` (1166), **discarding the per-notice-id detail** recorded by
   `pending_notice_notices` (999-1007). Compiles fine; loses operator-facing information.
3. LIKELY flake: the `src/session/worker_journal.rs` tests touch the global `WORKER_INBOXES` without holding
   `orchestrator::notice::TEST_NOTICE_MUTEX` — that mutex appears nowhere in `worker_journal.rs`, while sibling
   suites do hold it (`fix_loop.rs:1331/1420/1471`, `orchestrator/workers.rs:1818+`; policy text at
   `docs/cross_platform_verification.md:150`). Separately, `queued_notice_notices`'s `(0..count.max(1))`
   (`worker_journal.rs:1016`) silently inflates a `count` of 0 into one emitted notice.

## 6. Verdict

**ZERO confirmed compile errors in the six audited files.** Every tool-name import group has live use sites
(so `unused_imports = deny` cannot fire), no group introduces a duplicate binding (no E0252), every helper
signature and struct literal matches its declared arity, and both accessors and the notice types are visible
from their call sites.

Not verifiable statically (explicitly out of reach for this pass): real type inference behind the swaps; value
equality of swaps whose original string literal is unknown — a `terminal__`-vs-bare mix-up would compile and
still pass its in-file assertions; serde wire spellings (`PendingNotice` → `"pendingNotice"`, given the
`rename_all` attributes on these types); and `SCHEMA_VERSION` / `FORMAT_VERSION`.

Verification notes: re-verified `src/lib.rs:38`, `Cargo.toml:67-73`, `notice.rs:285`,
`worker_journal.rs:1232` (+1267/1826), `restore.rs:1857` and the whole constant table — all as stated.
Corrections applied against the brief: the `#[ignore]` attribute is at `tests/test_session_restore_panel_abort.rs:2432`
(not 2431) with its stale doc block ending at 2431; `restore.rs`'s `mod tests` opens at 1851; the `format/*`
files are under `src/session/format/` (line numbers unchanged); `DanglingCall` literals are at 2487-2491 and
2511-2515 (swapped `function_name` lines 2489/2513); `is_leave_verdict_tool_name` is at 315; fix_loop's
tool-name import lines are 1306 and 1540 inside the stated group ranges; `Inbox` is at `notice.rs:106-107`
while `WORKER_INBOXES` itself is at 98.

## 7. Audited inventory (t-032 addendum — still read-only: no compiler, no test run)

Full `TOOL_*` / `TERMINAL_*` name → value table: **§3 of this document** ("Constant inventory
(`src/tool_names.rs`) — name → exact value"); nothing is duplicated below.

Two anomalies pinned for future swap passes:

- `TOOL_ARCHIVE_PLAN = "archive_current_plan"` (`src/tool_names.rs:28`) — **name ≠ value stem**: the constant
  says `archive_plan`, the string is `archive_current_plan`. A swap that writes the stem instead of reading
  the constant compiles and silently tests a name the dispatcher never sees.
- `TOOL_LIST_DIRECTORY` (`:72`, `"list_directory"`) and `TERMINAL_LIST_DIRECTORY` (`:69`,
  `"terminal__list_directory"`) have **no dispatcher handler** and are **deliberately absent** from
  `CANONICAL_TOOL_NAMES` (`src/tool_names.rs:115`); the module comment at `:61-63` states the exclusion.
  Repo-wide, the two constants appear only in `src/tool_names.rs` and in test fixtures
  (`fix_loop.rs:2140-2141`, `ui/bridge/tests.rs:2161/2168`) — a swap that treats them as dispatchable tools
  asserts behavior that does not exist.

Corrected anchors (these replace the stale **1198 / 1233 / 1792** notes carried by earlier briefs):
`src/session/worker_journal.rs:1232` (`use crate::tool_names::TOOL_RUN_COMMAND;`), **1267**
(`name: TOOL_RUN_COMMAND.to_string(),`), **1826** (`function_name: TOOL_RUN_COMMAND.to_string(),`).

Re-verified against the current tree — these make two `#[ignore]` **reason strings** and their doc blocks in
`tests/test_session_restore_panel_abort.rs` false; no code changed:

- The panel-name `JoinHandle` emitter exists at `src/ui/session.rs:1256-1274` (`kind` line **1263**,
  `subject: panel.name.clone()`, one notice per restored `SubagentPanel` with `is_active: true`). So
  `worker_journal.rs:844-851` (`kind` line **845**, `subject: key.clone()`) is **not** the only production
  `JoinHandle` emitter; the third production site is `src/ui/raw.rs:585` (the stale note said `:587`).
- The AC-A2 capture half exists: `pending_notice_notices` at `src/session/worker_journal.rs:995-1008`, wired
  into `project_worker_section` at `:624`, fed by `orchestrator::notice::peek_worker_inbox` at
  `src/orchestrator/notice.rs:285-292` — the same `WORKER_INBOXES` entry `worker_inbox_len` counts
  (byte-identical key normalisation, §4 above). Both `#[ignore]` attributes were kept: the two tests have
  still never been executed; they are waiting on the t-014f / t-034 gate.
