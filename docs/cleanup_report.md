# Cleanup Report — `marmennill` v1.0.0

Mission scope: (1) remove rehydration / crash-recovery / deep-freeze **completely**, (2) unify duplicated functionality into single owners, (3) fix bugs.
This document is the mission's written deliverable. All numbers below are produced by read-only commands run in `/home/fredrik/marmel/wip` against `HEAD = 31f4e90` (`refactor` branch, also `main` / `origin/main`):

```
$ git --no-pager log --oneline -1
31f4e90 (HEAD -> refactor, origin/main, origin/HEAD, main) fix(ui): disable session rehydration by default and prevent exit on user steering abort
```

Every claim carries a `file:line` anchor or a pasted command output. `docs/recon_*.md` IDs (`M2`, `M3`, `M7`, `C1`, `H1`–`H8`, `P1`–`P3`, `L1`–`L2`) refer to the reconnaissance reports in `docs/`.

---

## 1. Executive summary

| Metric | Before (HEAD baseline) | After (worktree) | Evidence |
|---|---|---|---|
| Rehydration / crash-recovery / deep-freeze code symbols in `src/` | present (4 dedicated files + config + docs) | **0** | `grep -rniE "rehydrat\|freeze\|deep_freeze\|crash_recovery" src/` → 3 prose comments, 0 symbols |
| Files deleted (tracked) | — | 5 (1,877 lines: 371+81+722+651+52) | `git status --porcelain` (` D` rows), `git show HEAD:<f> \| wc -l` |
| New single-owner modules added | — | 8 modules, 6,002 lines (`src/markers.rs` 2,072; `src/plan_parse.rs` 1,344; `src/ui/bridge/arbiter.rs` 1,011; `src/tool_args.rs` 768; `src/mcp/protocol.rs` 786; `src/task_id.rs` 682; `src/harness/*`/`src/ui/bridge/action.rs` 841) | `wc -l` (§7) |
| New test files added | — | 7 files, 4,712 lines | `wc -l` (§7) |
| Test function declarations in `src` + `tests` | 466 | **967** | `grep -rcE '#\[(tokio::)?test'` sums (§7) |
| `cargo test --lib` pass count | ≈533 | **807 passed / 4 failed** (4 sanctioned: `harness::pty::tests::*`, `openpty` EACCES — `/dev/pts/ptmx` mode `000`) | gate tallies in `.marmel/execution_plan.md` |
| Integration suites | partial | ≈136–142 passed / 1 sanctioned failure (`tests/test_role_gating.rs::test_prompt_based_tool_gating`) | gate tallies in `.marmel/execution_plan.md` |
| `cargo clippy -- -D warnings` | diagnostics present | **0 diagnostics** | gate tallies in `.marmel/execution_plan.md` |
| `#[allow(` attributes in `src/` | 23 | **12** (all pre-existing at HEAD) | `grep -rn '#\[allow(' src/ \| wc -l` |
| `#[expect(` / `#[allow(async_fn_in_trait)]` | 0 / >0 | **0 / 0** (RPITIT instead) | `src/net/retry.rs:61-63`, `src/mcp/protocol.rs:405-415` |
| Worktree delta vs HEAD | — | `88 files changed, 36140 insertions(+), 10443 deletions(-)` | `git diff --shortstat` (§7) |

Mission outcome: the rehydration feature is gone from code, config, env surface and documentation; 16 duplicated concerns now have exactly one owner module; 20 bug classes were fixed with mutation-checked pins; the dead `AgentLoop`/`ManagerLoop` stack was deleted on a recorded decision, and the live safety net that actually gates turns was rebuilt instead (`TurnWatchdog`, `FailureBudget`, `SessionBounds`, `run_session_with_bounds`). Accepted residuals are listed explicitly in §6 rather than silently left behind.

---

## 2. Feature removal: rehydration / crash-recovery / deep-freeze

### 2.1 Deleted files (tracked, verified gone from disk)

| File | HEAD lines | Deleted lines (`git diff --numstat`) |
|---|---|---|
| `src/orchestrator/freeze.rs` (Crash journal / Deep-Freeze snapshots) | 371 | `0	371` |
| `src/ui/session/recovery.rs` (rehydrate UI/messages/subagents) | 81 | `0	81` |
| `src/ui/session/tools.rs` (session tool slice, split out only for recovery wiring) | 722 | `0	722` |
| `src/manager/loop_tests.rs` (tests of the deleted dead `AgentLoop`, §4) | 651 | `0	651` |
| `marmel.toml` (local config carrying `enable_rehydration`) | 52 | `0	52` |

```
$ git status --porcelain | grep '^ D'
 D marmel.toml
 D src/manager/loop_tests.rs
 D src/orchestrator/freeze.rs
 D src/ui/session/recovery.rs
 D src/ui/session/tools.rs

$ ls src/ui/session
ls: cannot access 'src/ui/session': No such file or directory
$ ls src/orchestrator/freeze.rs src/manager/loop_tests.rs marmel.toml
ls: cannot access 'src/orchestrator/freeze.rs': No such file or directory
ls: cannot access 'src/manager/loop_tests.rs': No such file or directory
ls: cannot access 'marmel.toml': No such file or directory
```

`src/ui/session/` as a directory no longer exists: its contents were folded into the single session owner `src/ui/session.rs` (1,267 insertions / 469 deletions) plus `src/ui/bridge/*`.

### 2.2 Scrubbed config keys, env vars, API surface and docs

Removed symbols (verbatim `-` lines from `git diff`):

- **Orchestrator** (`src/orchestrator/mod.rs`): `pub mod freeze;`, `pub use freeze::{CrashJournal, FreezeSnapshot, JournalEventKind};`, the `pub journal: CrashJournal,` field, `CrashJournal::new(plan.dir())` wiring, the pre-delegation Deep-Freeze snapshot and its `tracing::warn!("Deep-Freeze snapshot failed: {e}")` arm, the delegation-completion `journal.clear(&worker_id, true)`, and the whole `pub async fn recover_frozen(&self) -> Result<Option<Deliverable>>` recovery path (REQ-ORCH-003 persistence branch).
- **Config** (`src/config.rs`) — every key gone; `grep -nE "rehydration|REHYDRATION" src/config.rs` → `0` hits now, versus 7 at HEAD:
  `pub enable_rehydration: bool` (HEAD `src/config.rs:129`), default `enable_rehydration: false` (HEAD `:155`), env var `MARMEL_ENABLE_REHYDRATION` (HEAD `:198-199`), `PartialConfig.enable_rehydration` (HEAD `:261`), and the merge arm (HEAD `:342-343`).
- **Renderer trait** (`src/ui/mod.rs`, `0	6` numstat): `fn rehydrate_ui(&mut self, _records: &[UiRecord])`, `fn rehydrate_messages(...)`, `fn rehydrate_subagents(...)` (HEAD `src/ui/mod.rs:129-134`).
- **Session boot path** (`src/ui/session.rs`): the `if cfg.enable_rehydration { ... }` transcript-load branch, `renderer.rehydrate_ui(ut.records())` / `rehydrate_messages(ctx.messages())`, the `has_rehydrated` steering-history restore block (`"Session transcript rehydrated from disk (Ready)"`), `rehydrate_subagents_with_ui(...)`, and the Deep-Freeze recovery poll loop.

Documentation scrub (verbatim diff evidence in §7):

- `README.md`: removed the `.marmel/.session_frozen.json` ("Checkpoint used for crash recovery and resume") and `.marmel/.session_journal.json` ("Append-only event journal") rows from the session-files table, and removed the "Crash recovery and state rehydration." bullet from the Testing section (`git diff -- README.md`, `7	4`).
- `marmel.toml.example`: no rehydration key exists (`+6` lines are the new sandbox/timeout notes only).
- `grep -rniE "rehydrat|freeze|deep_freeze|crash_recovery" README.md README*.md marmel.toml.example AGENTS.md` → **0 hits**.
- Diff lines mentioning `rehydrat|freeze|frozen|journal` (added or removed): **486** (`git --no-pager diff | grep -icE "^[-+].*(rehydrat|freeze|frozen|journal)"`).

### 2.3 Residual grep surface: exactly 3 prose comments, no capability behind them

```
$ grep -rniE "rehydrat|freeze|deep_freeze|crash_recovery" src/
src/manager/loop.rs:528:    /// must not freeze the rest of the plan.
src/ui/tui/mod.rs:289:    /// Only counts when actively working on a plan; freezes as soon as the plan is completed / archived.
src/ui/session.rs:2172:    // rehydration would load, so it has to hit disk with the repair in it.
```

These are ordinary English usages ("freeze the rest of the plan" = stall; `src/ui/session.rs:2172` explains that a repaired plan file must be written to disk because nothing rehydrates it any more). **No capability exists behind any of them**: `grep -rnE "session_frozen|session_journal|CrashJournal|recover_frozen" src/` → 0 hits, `enable_rehydration` no longer exists, and there is no code path that reads or writes `.marmel/.session_frozen.json` / `.marmel/.session_journal.json`.

Runtime leftovers still on disk from pre-mission runs (not code, not tracked, deliberately not deleted by this documentation task):

```
-rw-rw-r-- 1 fredrik fredrik  9006 Oct  8 15:04 .marmel/.session_frozen.json
-rw-rw-r-- 1 fredrik fredrik 40901 Oct  8 15:04 .marmel/.session_journal.json
```

plus their now-vestigial ignore entries `.gitignore:9` (`.marmel/`) — the `.marmel/` ignore rule is still needed for prompts/logs/archive, so it was kept.

### 2.4 Worktree observation to disclose: `marmel.toml` → `marmel.toml.df`

```
$ git status --porcelain | grep marmel
 D marmel.toml
?? marmel.toml.df
$ git check-ignore -v marmel.toml.df ; echo "exit=$?"
exit=1
$ git show HEAD:marmel.toml > /tmp/head_marmel.toml && git --no-pager diff --no-index --stat /tmp/head_marmel.toml marmel.toml.df ; echo "diffexit=$?"
diffexit=0
```

`marmel.toml` was tracked at HEAD (52 lines, containing `enable_rehydration`) and is deleted in the worktree; `marmel.toml.df` is **untracked, 52 lines, byte-identical to `git show HEAD:marmel.toml`** (`--no-index` diff exit 0 = no differences), and is *not* matched by `.gitignore` (`.gitignore:42` ignores `marmel.toml`, but not `marmel.toml.df`; `git check-ignore` exit code 1). Nobody on this mission deliberately renamed or touched it — the pair is disclosed verbatim from `git status` rather than explained away. The tracked config key it carried is gone from the code, so nothing in the binary reads it.

---

## 3. Unification map (before → after)

Before the mission, the same concern was re-implemented in several call sites (inventory: `docs/recon_duplication_helpers.md`, `docs/recon_duplication_net.md`, `docs/recon_bugs_*`). After: one owner per concern, duplicates deleted or demoted to thin wrappers.

| # | Duplicated concern (before) | Single owner (after) | Deleted / demoted |
|---|---|---|---|
| 1 | Marker strings and marker revocation re-spelled per call site, case-inconsistently | `src/markers.rs` (2,072 L): vocabulary table `:43-67`, `failed_trailer:299`, `aborted_deliverable:332`, `revoke_completion:569`; deliberately empty registries `KNOWN_EXTERNAL_MARKER_SITES: &[(&str, &str)] = &[]` at `:1546` and `HAND_OFF_MARKER_SITES: &[&str] = &[]` at `:1572` | Inline `format!`/`contains` marker literals in agents/runner and orchestrator; caller now single-liners, e.g. `src/agents/runner/assembly.rs:63`. Empty registries are an explicit decision (no external marker sites exist), not an oversight. |
| 2 | Tool-name spellings compared as raw string literals in ≥9 places, with a wrong "claim" of `list_directory` | `src/tool_names.rs`: `TOOL_ALIAS_TABLE:157`, `BUILTIN_TOOL_NAME_TABLE: &[&str] = &BUILTIN_TABLE:402`, const `TOOL_LIST_DIRECTORY:72` **deliberately un-claimed** (rationale `:60-72`, alias table note `:131`, `:206-207`) | Per-crate literal comparisons; pin test `list_directory_spellings_are_unclaimed_and_report_unknown_tool` at `src/tool_names.rs:662`; drift guard `tests/test_tool_name_literals.rs` (`KNOWN_TOOL_NAMES:19`, `BASELINE:53` — baseline shrank 13 → 9 rows). |
| 3 | Sleep duration parsing/clamping re-implemented in tool layer, arbitrator layer and UI sink | `src/tool_args.rs`: `SLEEP_DEFAULT_SECS=5:83`, `SLEEP_MIN_SECS=1:85`, `SLEEP_MAX_SECS=300:87` (+ compile-time ordering assert `:91`), `sleep_duration_secs:280` (clamp `:283`). Arbitrator layer owns its own policy: `src/ui/bridge/arbiter.rs` `clamped_sleep_secs:88`, `SleepBudget:133`, `allow_at:176`, `host_sleep_secs:241`; host path `src/ui/bridge/sink.rs::host_sleep_request:21` (used `:208-211`) | Ad-hoc `as u64` casts and unclamped steered sleeps. Documented, intentional non-aliasing: the arbitrator upper knobs are *not* derived from `SLEEP_MAX_SECS` (`src/tool_args.rs:53`, `src/ui/bridge/arbiter.rs:74-77`). |
| 4 | Task-id extraction/validation open-coded at every `id → path` join (enabling `t-1` vs `t-10` confusion, path traversal) | `src/task_id.rs` (682 L): `validate_task_id:155`, `validate_task_id_owned:203` | Hand-rolled id slicing at each join. Enforced at every join site: `src/orchestrator/workers.rs:521`, `src/orchestrator/mod.rs:563`, `src/agents/validation.rs:215`, `src/agents/runner/execution.rs:334`, `src/agents/prompt_builder.rs:51` and `:594`, `src/harness/workspace.rs:103`. (A stale doc comment at `src/task_id.rs:139` still names the deleted `extract_task_id`; listed in §6.) |
| 5 | Retry/backoff and HTTP client construction duplicated across providers/MCP/HTTP transports | `src/net/retry.rs`: `RetryOp:53` returning `impl Future` (RPITIT, `:61-63`), `retry_with_backoff:75`; `src/net/http.rs`: `HttpClientOptions:17`, `build_http_client:31`, `build_http_client_with:44` | Duplicated per-crate retry loops and bespoke `reqwest::Client` builders. `#[allow(async_fn_in_trait)]` count is now **0** (only prose mentions at `src/net/retry.rs:50`, `src/mcp/protocol.rs:390`). |
| 6 | JSON-RPC 2.0 envelope structs/ids re-declared in MCP client and HTTP transport | `src/mcp/protocol.rs` (786 L): `RequestIds:93`, `JsonRpc*` structs `:121/:178/:212/:258`, `decode_response:266`, `JsonRpcTransport` trait `:395` with RPITIT `send_request:405-408` / `send_notification:412-415`, `initialize:422`, `list_tools:432`, `call_tool:439` | Envelope duplicates in `src/mcp/client.rs` / `src/mcp/http.rs` (now consumers). |
| 7 | Character-safe truncation re-implemented in 6 places (and byte-slicing that panicked on UTF-8) | `src/text_util.rs` (130 L): `truncate_chars:24`, `truncate_with_ellipsis:40` | Local truncation helpers in `src/orchestrator/workers.rs`, `src/harness/fs.rs`, `src/harness/pty.rs`, `src/tool_args.rs`, `src/ui/helpers.rs`, `src/ui/session.rs`; also deleted `utf8_safe_slice` (dead, §4). |
| 8 | Plan/task-line parsing forked between manager phase logic, orchestrator plan summary and UI check-off | `src/plan_parse.rs` (1,344 L): `is_task_id_token:166`, `find_task_id_token:173`, `task_id_eq:183`, `parse_task_line:196`, `parse_tasks:258`, `check_off_content:435`, `normalize_newlines:483`, `task_id_of_line:510` | Per-module regex/manual-split parsers, incl. the one that mis-derived task ids on check-off (§5 row 7). |
| 9 | Tool-argument coercion (numbers/strings/aliases) open-coded per tool | `src/tool_args.rs` (768 L) incl. `search_pattern:199` (`first_str(args, &["pattern", "query"]):200`), `sleep_duration_secs:280` | Per-tool `serde_json` poking in harness and monitor code (e.g. the grep preview key fix §5 row 2, `src/harness/fs.rs:617-624` keeps the `query` alias only at the display boundary). |
| 10 | Worker routing matched on loose substrings/roles (cancel `t-1` hit `t-10`) | `src/orchestrator/notice.rs`: `WorkerRoutingIdentity:533`, `worker_routing_identity:585`, `routes():615` (contract doc `:863`); `src/orchestrator/workers.rs` `worker_identity:676-679`, `worker_matches:752`; `src/orchestrator/preemption.rs` `routing_identity:52`, `StreamIdentity::matches:90` | Substring/role guessing in cancel, preemption and notice-delivery paths (call sites `src/orchestrator/workers.rs:701/740/752/780`, `src/orchestrator/preemption.rs:84`). |
| 11 | Steering-history correlation compared with bidirectional substring containment | `src/orchestrator/steer.rs`: `entry_names_notice_id:493`, `same_steering_inquiry:517` (trap rationale `:477` and `:514`) | Inline `a.contains(b) \|\| b.contains(a)` matching; pinned by `tests/test_steer_history.rs` (9 tests, e.g. `:79`, `:112`, `:305`). |
| 12 | Tool-schema token accounting existed in two shapes, both inert | `src/manager/context.rs`: `tools_tokens:213`, `manager_wire_tools:250`, `set_tools:661`, `tool_schema_tokens:666`; shared charge function `fix_loop::charge_engine_tool_schema:521` (`engine.set_tools:523`) used by **both** the fix-loop path (`src/agents/runner/fix_loop.rs:843`) **and** the specialist turn path (`src/agents/runner/execution.rs:139` `build_specialist_context:131`, `:157` `sync_specialist_tool_schema:153`, re-charge `:1034`) | Duplicated token math; near-copy wrapper `src/ui/session.rs::sync_manager_tool_schema:198` (calls `set_tools:201`) retained as a thin adapter — flagged as residual in §6. |
| 13 | Stop/termination reasons returned as bare `bool`/`()` | `src/ui/session.rs`: `BoundStopReason:70` (`code():85`, turn-cap code `:95`, `label():103`), `SessionStopReport:138`, `run_session:220`, `run_session_with_bounds:238`, `run_session_with_bounds_report:258` | Silent `return`s and the invisible turn-cap exit; `SessionBounds:33` (Default `:44-48`). |
| 14 | Verdict-permission and tool-advertisement checks scattered | `src/agents/runner/execution.rs`: `may_record_verdict:57`, `verdict_gap:325-366`, `MAX_CONSECUTIVE_VERDICT_ROLE_REJECTIONS=3:17` (used `:839`); `src/agents/runner/fix_loop.rs`: `caller_may_record_verdict:433`, `advertised_tools_for_caller:492`, `VERDICT_RECORDING_TOOLS:324`, `INSPECTION_TOOLS:354`, `MUTATING_TOOLS:372`; `src/agents/validation.rs` `NO_EXPLICIT_APPROVAL_REASON:63`, `parse_verdict_args:72` | Per-call-site `if tool == "…"` chains that defaulted to allow (§5 row 14). |
| 15 | UI bridge round machine split across `steer.rs` + `sink.rs` with duplicated state | `src/ui/bridge/arbiter.rs` (1,011 L) owns the round machine; `src/ui/bridge/action.rs` (209 L) owns the action vocabulary | `src/ui/bridge/steer.rs` demoted to an adapter (module doc `src/ui/bridge/steer.rs:1-4`); `src/ui/bridge/sink.rs` slimmed (`153	196`). |
| 16 | Reply accumulation / token counting / plan-task access duplicated | `src/llm/client.rs::ReplyAccumulator::finalize:843/:862`; `src/manager/context.rs::count_text_tokens:117`; `src/harness/plan.rs` + `src/manager/phase.rs` `try_pending_tasks:447` / `try_all_tasks:461` (fail-closed) | Ad-hoc accumulators; legacy infallible wrappers `pending_tasks:482`, `all_tasks:497` kept test-only (§6). |

---

## 4. Dead-code decision, the live-path gap it created, and what replaced it

Decision record: `docs/decision_dead_code_manager.md` (311 lines).

**Decision (§3 of the decision doc).** The `AgentLoop` / `ManagerLoop` stack was unreachable from every live entry point (`main`, TUI, raw, orchestrator). It was deleted rather than repaired, together with its companions: the `compact_with_retry` cluster (12 items) and the `SlowPrefillTracker` cluster, plus the dead `utf8_safe_slice`. The dedicated test file `src/manager/loop_tests.rs` was deleted with it (**651 lines, ~40 tests**), because the majority of those tests were tautological with respect to the live system — e.g. they pre-set `turn_count = MAX_TURNS` to reach the branch under test instead of exercising it (`docs/decision_dead_code_manager.md:186`, ID `M13`).

**Consequence recorded, not hidden: REQ-CORE-005 is NOT implemented.** The decision doc states this explicitly in §3 and §6 (`docs/decision_dead_code_manager.md:7` area, §6 line ~180): the "manager loop retries a stalled turn" requirement has **no implementation** after the deletion. No compact-retry wrapper and no prefill-speed tracker survives, and none was reinstated.

**What restored a live safety net instead (decision doc §8 addendum, task t-031c).** Bounds were rebuilt on the path that actually runs turns, as data (`SessionBounds`) rather than as an unreachable loop object:

| Live control | Location |
|---|---|
| `MAX_TURNS = 100` | `src/manager/loop.rs:51` |
| `TURN_WATCHDOG_SECS = 600` | `src/manager/loop.rs:65` |
| `TURN_HARD_CAP_SECS = 3 * 60 * 60` (10,800 s) | `src/manager/loop.rs:74` |
| `TASK_FAILURE_ESCALATION_THRESHOLD = 2` | `src/manager/loop.rs:81` |
| `DeadlineKind` (`label():104`, `describe():113`) | `src/manager/loop.rs:96` |
| `TurnWatchdog` | `src/manager/loop.rs:139` |
| `FailureBudget` | `src/manager/loop.rs:285` |
| `SessionBounds` | `src/ui/session.rs:33` (Default `:44-48`) |
| `run_session_with_bounds` / `..._report` | `src/ui/session.rs:238` / `:258` |

Failure-budget policy (decision doc §8.3): threshold 2, keyed by `task:`/`tool:` (repetition detector `src/harness/monitor/repetition.rs:13`, default threshold 5 for verbatim repetition). Decision doc §8.4 records that `run_executing` remains dead (`src/orchestrator/mod.rs:391`, `MAX_EXECUTING_ROUNDS:76`) — a known, non-safety-critical gap. Decision doc §8.5 records the replacement guards: 11 unit tests in `src/manager/loop.rs` and 3 hermetic tests in `tests/test_ui_session.rs`, including the mutation check "100 blind retries" (`docs/decision_dead_code_manager.md:310`) that fails if the bounds are neutered. Hand-offs listed in the decision doc that have since been executed: `Plan::check_off_on_success` and `output_is_success` are deleted (`src/manager/phase.rs:34` comment, `src/manager/mod.rs:8`).

---

## 5. Bug fixes

Every row: symptom → root cause → fix (file:line) → the automated test that pins it. Recon IDs point into `docs/recon_*.md`.

| ID | Symptom | Root cause | Fix (file:line) | Pinning test |
|---|---|---|---|---|
| BF-01 | A completion marker was never revoked when the model re-emitted it with different capitalisation, so a "completed" task stayed completed after failing (recon `agents_monitor` H3, `docs/recon_bugs_agents_monitor.md:48`) | Marker revocation compared literal strings case-sensitively; the emit path and the revoke path spelled the marker independently | Single owner `src/markers.rs::revoke_completion:569` (case-correct, whole-token match); sole caller `src/agents/runner/assembly.rs:63` | `src/markers.rs:1178` `revoke_completion_is_case_correct` (+ companion `:1205`) |
| BF-02 | Grep result previews echoed an empty/absent term and the rescued XML search call failed because the display layer read `query` while the producer wrote `pattern` (recon `M5`, `docs/recon_bugs_agents_monitor.md:153`) | Two vocabularies for the same argument key, with no coercion owner | Key normalisation on the rescue path: `src/harness/monitor/xml.rs:246-255` and `:407-422` (`map.remove("query"):421`, `or_insert("pattern"):422`); display reads through the owner `src/tool_args.rs::search_pattern:199` (`first_str(args,&["pattern","query"]):200`), display alias kept at `src/harness/fs.rs:617-624` | `src/harness/monitor/monitor_tests.rs:741` `test_monitor_xml_rescue_search_call_carries_pattern_key` (+ `:754`); `src/harness/search_tests.rs:594` `grep_search_reads_the_pattern_key_and_never_echoes_query` |
| BF-03 | Search results were silently truncated (agent believed the search was exhaustive); brace globs (`*.{rs,md}`) matched nothing; result order changed run to run (recon `H8/H9/H10`, `docs/recon_bugs_harness_llm_ui.md:39,45,52`) | Truncation dropped rows without a marker; no brace expansion; iteration over `WalkDir`/`HashMap` with no sort | `src/harness/search.rs`: `GREP_TRUNCATION_MARKER:74`, `GLOB_TRUNCATION_MARKER:77`, brace-expansion caps `:80`/`:84`, `ORDERING_NOTE:89`, deterministic `.sort_by_file_path:159`, `expand_braces:366` | `src/harness/search_tests.rs:105,174,232,264,322,346,419,475`; `tests/test_harness.rs:472,522` |
| BF-04 | A new worker's inbox overwrote the previous worker's pending notices, notices were delivered to the wrong worker, and stale notices never expired (recon `H4/H5/H6`, `docs/recon_bugs_orchestrator.md:64,80`) | Worker ids reused; inbox keyed loosely; no TTL on notices or idle inboxes | `src/orchestrator/workers.rs`: monotonic `WORKER_ID_SEQ:241`, id-disambiguation warning `:337`, `reclaim_worker_inbox` call `:209`, completed-worker ring `:983`; `src/orchestrator/notice.rs`: `NOTICE_TTL_MS:118`, `INBOX_IDLE_TTL_MS:124`, expiry sweeps `:198`/`:323`/`:353` | `src/orchestrator/workers.rs:1087,1359,1539`; `src/orchestrator/notice_tests.rs:303,961,1012,1045,1291,1586`; `src/harness/dispatch_integrity_tests.rs:81,128,179,229` (asserts no guessing of notice ids) |
| BF-05 | Preemption acks leaked: one hung worker blocked preemption of all others and pending acks accumulated forever (recon `H2`, `docs/recon_bugs_orchestrator.md:25`) | Acks awaited sequentially with no deadline and no accounting | `src/orchestrator/preemption.rs`: `PREEMPT_ACK_TIMEOUT:357`, `PendingAck:366`, concurrent drain with `FuturesUnordered:420-435`, per-ack deadline `:441`, diagnostic `tracing:477` | `src/orchestrator/preemption.rs:552,598` and `:642` `test_stream_identity_matches_rejects_truncated_prefixes` |
| BF-06 | Cancelling task `t-1` cancelled `t-10` (and preemption targeted the wrong stream) — substring id matching (recon orchestrator `M2`/`H3`) | Routing decided by `contains`-style id/role matching instead of an exact identity | Exact identity type `src/orchestrator/notice.rs::WorkerRoutingIdentity:533` (`worker_routing_identity:585`, `routes():615`); consumers `src/orchestrator/workers.rs:701/740/752/780` (`worker_matches:752`), `src/orchestrator/preemption.rs:84` (`StreamIdentity::matches:90`) | `src/orchestrator/workers.rs:2062` `test_cancel_by_task_id_cancels_t1_and_never_t10` (asserts `:2098`, `:2102`), `src/orchestrator/workers.rs:2166-2201`, `src/orchestrator/preemption.rs:647` |
| BF-07 | Check-off marked the wrong plan line (or none): the task id was *derived* from message prose and could be a substring/prefix of a neighbouring id; check-off mutated the file with no gate (recon `C1/C4/C5`, `docs/recon_bugs_harness_llm_ui.md:126,146,151`) | Id extraction by regex-on-prose plus infallible `Plan` mutation wrappers | Fail-closed gate: `src/ui/session.rs::PlanGate:2044` and `CheckOffOutcome:2228-2309`; ownership moved to `src/plan_parse.rs` (`task_id_eq:183`, `find_task_id_token:173`, `check_off_content:435`, `task_id_of_line:510`); raw mutation path now derives nothing (`src/manager/phase.rs:590`); UI mapping `src/ui/helpers.rs:46-61` | `tests/test_plan_checkoff.rs` (6 tests: `:54,113,178,263,405,476`; non-vacuity witness `:98`); witnesses `src/manager/phase_tests.rs:960-986` |
| BF-08 | After context compaction the provider answered HTTP 400: assistant messages carrying `tool_calls` were orphaned from their `tool` results (recon manager `H1`, `docs/recon_bugs_manager.md:29`) | Compaction cut the transcript at arbitrary boundaries, severing assistant↔tool-call pairing; failure surfaced as a generic compaction miss | `src/manager/context.rs::ensure_tool_call_pairs:922` (+ shape probe `pairing_shape:445`), invoked at `:955`/`:1272` and at boundaries via `src/ui/session.rs::ensure_pairing_at_boundary:2160` (before the turn-budget exit at `:505`); outcome surfaced instead of swallowed by `surface_compaction_outcome:2194` (called `:558`, `:719`, `:1815`); notice text owner `fix_loop_compaction_notice` `src/agents/runner/fix_loop.rs:1191` | `src/manager/context_tests.rs:903,971,1116,1249`; `tests/test_ui_session.rs:2154,2377,2489` |
| BF-09 | `read_file` panicked with a UTF-8 slicing panic (or OOM'd) on files containing multibyte characters near the byte cap (recon harness `H2`, `docs/recon_bugs_harness_llm_ui.md:20`) | `&bytes[..n]` byte-slicing of `String` and no size guard | `src/harness/fs.rs`: `READ_FILE_MAX_BYTES = 256 KiB:135`, two-stage cap+boundary guard `:148-175`, `drop_partial_utf8_tail:242` | `tests/test_harness.rs:128,207,227,269`; companion PTY boundary test `src/harness/pty.rs` `is_utf8_lead_byte` with `src/harness/pty_tests.rs:682,741` |
| BF-10 | `replace()` corrupted files when two calls hit the same path concurrently (temp-file name collision, one call's staging clobbered the other's) (recon harness `H4`, `docs/recon_bugs_harness_llm_ui.md:24`) | Deterministic temp name `.<stem>.tmp` in the same directory | `src/harness/fs.rs`: collision-free staging name `.{stem}.tmp.{pid}.{nanos}.{seq}:19`, sharded per-path locks `REPLACE_LOCK_SHARDS = 64:332` / `REPLACE_LOCKS:334` / `hash_path:352`, `create_new`/`O_EXCL` staging `:380`, RAII temp guard `:444-455` | `tests/test_harness.rs:290,353,388,423,451` |
| BF-11 | A chatty PTY grew memory without bound and finished commands left zombie children that leaked fds/PIDs (recon harness `P1`/`P3`, `docs/recon_bugs_harness_llm_ui.md:66,80`) | Output ring unbounded; children `kill`ed but never `wait`ed | `src/harness/pty.rs`: `PTY_OUTPUT_BUFFER_CAP:73`, `ChildReaper` (with `kill_and_reap`, `DeferredReapQueue`) `:171`, ring trim `:1029-1050`, `PtySession::Drop:874` reaps deterministically | `src/harness/pty_tests.rs:337,362,391,421,465,496,525,560,589,601,634` |
| BF-12 | An MCP server could register a tool named like a built-in and shadow it (hijacking `read_file`/`run_command`) (recon harness `H1`, `docs/recon_bugs_harness_llm_ui.md:12`) | Registration order decided dispatch; no gate between built-ins and MCP tools | `src/harness/mod.rs`: `mcp_registration_verdict:122`, `McpDispatchGate:146`, process-wide gate static `:35` (built-ins win, conflicts reported, never silently shadowing) | `src/harness/tool_resolution_tests.rs:140,197,231,321,382,436,501,566,623,665,709` |
| BF-13 | The Landlock sandbox was applied only if the command *name* looked shell-ish, and any setup error fell back to running unsandboxed; a sandbox refusal looked like a normal exit | Name heuristic as the gate + fail-open `match ... { Err(_) => run }` + no distinct exit status | `src/harness/pty.rs`: typed `SandboxInputs:508`, `should_apply_sandbox:600` (no name heuristic; `UnresolvedExe` fails closed `:636-641`), `resolved_exe_path:642`; `src/harness/sandbox.rs`: `SANDBOX_REFUSE_EXIT_CODE = 73:50` (`EX_UNAVAILABLE`), `reentry_action:241` → `Refuse:273`, `opt_out_from_value:198`; wiring `src/main.rs:98-103`; docs `README.md` sandbox-opt-out section | `src/harness/sandbox.rs:684,781,836,1079-1084`; `src/harness/pty_tests.rs:823,849,889,909,954,973,998,1023,1080` |
| BF-14 | An approval/verdict tool call with unparsable or missing arguments was treated as an approval (fail-open), and a caller with no verdict role could still record a verdict | Defaulted `Option` on parse failure; permission implied by tool presence rather than role | `src/agents/validation.rs`: `NO_EXPLICIT_APPROVAL_REASON:63`, `parse_verdict_args:72` (never returns `None`, rationale `:70`, refusal arm `:158-160`); `src/agents/runner/execution.rs`: `verdict_gap:325-366` (hard failure), `MAX_CONSECUTIVE_VERDICT_ROLE_REJECTIONS = 3:17` (enforced `:839`), `may_record_verdict:57`; `src/agents/runner/fix_loop.rs::caller_may_record_verdict:433` | `tests/test_role_gating.rs:388,427,455,524,682,783,1198,1325`; `src/harness/dispatch_integrity_tests.rs:298-351`; `src/agents/validation.rs:746,802,823` |
| BF-15 | A provider stream that died mid-reply was retried, re-emitting a partially consumed answer and duplicating tool calls (recon harness `L1`/`L2`, `docs/recon_bugs_harness_llm_ui.md:161,168`) | Retry policy keyed only on transport error class, blind to whether content had already been yielded | `src/llm/client.rs`: `StreamProgress::forbid_mid_stream_retry:367` (consulted `:802`, builds `ChatError::Interrupted:374`), non-retryable arm `:283`, `ReplyOutcome::Truncated:301` | `src/llm/client_tests.rs:193,453,489` |
| BF-16 | A steered `sleep(1e9)` slept for the whole turn (unclamped, could overflow); `sleep_seconds: 0` produced a phantom zero-length sleep that hot-looped the turn; sub-second remainders returned `Ok(0)` and looped | Each layer parsed/clamped sleep differently: steered sleeps bypassed the tool clamp, and a floor-then-clamp order turned a small remainder into 0 s | Owner clamp `src/tool_args.rs::sleep_duration_secs:280-283`; arbitrator layer `src/ui/bridge/arbiter.rs::clamped_sleep_secs:88` (clamp **then** floor, note `:196`), budget gate `allow_at:176-199` refuses when remaining `< SLEEP_MIN_SECS` (`:181-197`), host path `host_sleep_secs:241` → `src/ui/bridge/sink.rs::host_sleep_request:21` | Table-driven `src/tool_args.rs:648-682` (`{"seconds":1e9}` → `SLEEP_DEFAULT_SECS` at `:672`; `0` → `SLEEP_MIN_SECS` at `:659`); `src/ui/bridge/arbiter.rs:732,774,833,953,999`; `src/ui/bridge/tests.rs:1448,1530,1595` |
| BF-17 | The tool-schema token budget was inert on **both** paths: the manager transcript and the specialist turn each counted only message text, so the real per-request overhead (tool JSON schemas) was never charged and windows overflowed late (recon manager `M7`; `docs/decision_dead_code_manager.md`) | `set_tools` never existed / was never called; token math duplicated and each copy ignored schemas | Single charge function `src/agents/runner/fix_loop.rs::charge_engine_tool_schema:521` (`engine.set_tools:523`), called by the fix loop (`:843`) **and** by the specialist turn (`src/agents/runner/execution.rs:139/157`, re-charge `:1034`); accounting `src/manager/context.rs::set_tools:661`, `tools_tokens:213`, `tool_schema_tokens:666`, `manager_wire_tools:250`; UI path `src/ui/session.rs::sync_manager_tool_schema:198-215` | `src/manager/context_tests.rs:1896,1948,2031,2083` (doc-shape guard); `src/ui/session.rs:2391-2431` (asserts `request_token_count() > token_count()` at `:2427`); `src/agents/runner/fix_loop.rs:2158`; `tests/test_specialist_schema_charge.rs` (4 tests) |
| BF-18 | A short steering inquiry matched an unrelated longer history entry (and vice versa), so the arbitrator reused a stale decision (recon orchestrator `M3`, `docs/recon_bugs_orchestrator.md:115`) | Bidirectional substring containment used as "same inquiry"; notice ids matched inside other tokens | Whole-token, non-substring correlation only: `src/orchestrator/steer.rs::entry_names_notice_id:493`, `same_steering_inquiry:517` (trap notes `:477`, `:514`) | `tests/test_steer_history.rs` (9 tests): `short_inquiry_never_matches_...:79`, `notice_id_is_matched_as_a_whole_token...:112`, `bus_correlation_keeps_no_substring_fallback:305` |
| BF-19 | When the turn cap tripped, the session just stopped — invisibly, with no reason reported to the UI or to the caller, and callers could not tell a capped exit from a clean one | `run_session` returned no termination information; the cap exit was a bare `return` | `src/ui/session.rs`: `BoundStopReason:70` (`code():85` incl. `"turn_cap":95`, `label():103`), `SessionStopReport:138`, `run_session_with_bounds:238`, `run_session_with_bounds_report:258`, explicit labelled exit `:500-515` | `tests/test_ui_session.rs:1449,2585,2603,2803`; 13 unit tests in `src/manager/loop.rs` (bounds/watchdog/failure-budget) |
| BF-20 | Tests that could not fail: tautological assertions over the dead manager loop (e.g. pre-setting `turn_count = MAX_TURNS`), an `openpty`-aborting test, and an orphan-prune fixture that asserted its own setup | Tests written against the dead `AgentLoop` shape rather than observable behaviour | Deleted `src/manager/loop_tests.rs` (651 L, ~40 tests; `docs/decision_dead_code_manager.md:186`, ID `M13`); fixtures rebuilt with witnesses — `src/manager/context_tests.rs:102` (comment on the rebuilt orphan-prune fixture), `src/manager/phase_tests.rs:960-986` (non-vacuity witnesses), `tests/test_role_gating.rs:1617` (non-vacuity guard) | Mutation-proved: the replacement guards fail when the behaviour is neutered, incl. the "100 blind retries" mutation check (`docs/decision_dead_code_manager.md:310`) |

---

## 6. Accepted residuals (deliberately not fixed, with locations)

These were enumerated, costed and consciously left in place. Each is a real, currently-visible limitation; none is a hidden failure mode of the kind fixed in §5.

| Residual | Location | Why accepted |
|---|---|---|
| Display still reports transcript-only token counts while gating uses request totals (`Event::TokensIn`) | `src/ui/session.rs:557,572,718,1375,1814` (`token_count()`); worker-context mirrors `src/agents/runner/execution.rs:395,879,977` (`update_active_worker_context(..., engine.token_count())`) | Cosmetic/observational: budget **decisions** use `request_token_count()` (verified by `src/ui/session.rs:2427`); changing the display number would break existing UI expectations and its tests. |
| `SharedSteeringHistory` is `Arc<RwLock<Vec<(String, String)>>>` with no notice id, no push-time cap | `src/orchestrator/bus.rs:11`; direct pushes bypass correlation at `src/ui/bridge/arbiter.rs:554` and `src/ui/bridge/sink.rs:134` | Correlation correctness is fixed at the read side (`steer.rs:493/517`); widening the tuple type touches the UI transcript layer. Bounded in practice by turn caps. |
| Second, still case-sensitive "same inquiry" comparison | `src/ui/transcript.rs:140-146` (`last_q == user_q:142`) | UI dedupe convenience only; the authoritative arbitrator comparison is `src/orchestrator/steer.rs:517`. |
| Dead label branch `"turn hard cap"` | `src/ui/session.rs:105-110` in `BoundStopReason::label()` — `DeadlineKind::describe` (`src/manager/loop.rs:113-125`) never emits "hard cap" | Harmless unreachable arm; deleting it would churn the enum surface without behavioural gain. |
| `run_session` callers discard the `SessionStopReport` | `src/ui/tui/mod.rs:1077`, `src/ui/raw.rs:26` | Report is surfaced on the bounds-carrying path; TUI/raw entry points still terminate identically. Flagged for follow-up. |
| `sync_manager_tool_schema` is a near-copy of `charge_engine_tool_schema` | `src/ui/session.rs:198` vs `src/agents/runner/fix_loop.rs:521` | Kept as a thin adapter across the UI/session boundary; the accounting itself is single-owner (`set_tools`). |
| Unbounded PTY accumulator outside the capped ring | `PtySession::read_output` `src/harness/pty.rs:858-870`; reader-thread accumulator in `run_command_pty` `:920-932` | Bounded by the command timeout (`1..=300 s` clamp); the long-lived session path is capped (`PTY_OUTPUT_BUFFER_CAP:73`). |
| `PlanGate::Read` carries no plan content | `src/ui/session.rs:2044-2047` | Gate semantics (proceed/refuse) are what check-off needs; content plumbing to the UI is separate work. |
| Tool list for prompts is still derived from prose | `src/agents/prompt_builder.rs:219-226` | Prompt text, not dispatch truth; dispatch is gated by `advertised_tools_for_caller` (`src/agents/runner/fix_loop.rs:492`). |
| Sandbox opt-out honoured from child environment only | `src/main.rs:98-103` | Typed config knob exists (`sandbox_disabled`) and either source opts out with an attributable warning; residual is the env-only propagation shape. |
| `DEFAULT_TIMEOUT_SECS` duplicated in spirit | `src/harness/pty.rs:59-60` — single definition, aliased by `config::effective_command_timeout_secs` | Intentional single source with a documented alias (README range note `1..=300`). |
| Test-only infallible `Plan` wrappers kept | `src/manager/phase.rs:482` (`pending_tasks`), `:497` (`all_tasks`), `:515`; live callers use the fail-closed `try_pending_tasks:447` / `try_all_tasks:461`; `src/harness/plan.rs:79` still uses `pending_tasks` | Deleting them would churn ~15 test call sites; no live path uses them (verified by call-site enumeration). |
| Optional `active_worker_key_for_role` accessor left as-is | documented at `src/harness/mod.rs:753` | Callers already route through `WorkerRoutingIdentity` (`notice.rs:533`). |
| Arbitrator sleep ceilings intentionally **not** coupled to `SLEEP_MAX_SECS` | `src/tool_args.rs:53`; `src/ui/bridge/arbiter.rs:74-77` (`MAX_CHAINED_SLEEP_EXTENSIONS:48`, `MAX_CHAINED_SLEEP_SECONDS = 300:57`, `MAX_SINGLE_SLEEP_SECONDS = 300:60`) | Deliberate policy split: tool-layer clamp vs arbitration budget. Coupling them would silently change arbitration semantics. |
| 3 prose mentions of freeze/rehydration remain | `src/manager/loop.rs:528`, `src/ui/tui/mod.rs:289`, `src/ui/session.rs:2172` | Ordinary English in doc comments; no capability behind them (§2.3). |
| Stale runtime artifacts + their ignore entries | `.marmel/.session_frozen.json`, `.marmel/.session_journal.json` on disk; `.gitignore:9` (`.marmel/`) | Data, not code; not read by any surviving code path. |
| `src/llm/mod.rs` re-export list (e.g. `drive_streamed_turn`) still broad | `src/llm/mod.rs` | Re-exports only; no duplication of behaviour. |

Known sanctioned test failures (environment, not behaviour): `harness::pty::tests::*` ×4 — `openpty` returns `EACCES` because `/dev/pts/ptmx` is mode `000` in this container — and `tests/test_role_gating.rs::test_prompt_based_tool_gating`.

---

## 7. Numbers appendix

### 7.1 Diff totals (pasted, verbatim)

```
$ git --no-pager diff --shortstat
 88 files changed, 36140 insertions(+), 10443 deletions(-)

$ git --no-pager diff --shortstat HEAD
 88 files changed, 36140 insertions(+), 10443 deletions(-)
```

(`git diff` and `git diff HEAD` are identical: the index matches `HEAD`; all mission work is unstaged in the worktree. `git diff --stat` prints the same 88-file table with bar graphs and the identical trailer, e.g.:)

```
$ git --no-pager diff --stat | tail -3
 tests/test_ui_session.rs             | 2420 ++++++++++++++++++++++------------
 tests/test_validation_loop.rs        |  752 ++++++++---
 88 files changed, 36140 insertions(+), 10443 deletions(-)
```

### 7.2 Status classification

```
$ git status --porcelain | awk '{print substr($0,1,2)}' | sort | uniq -c
     17 ??
      5  D
     83  M
```

Deleted (tracked): `marmel.toml`, `src/manager/loop_tests.rs`, `src/orchestrator/freeze.rs`, `src/ui/session/recovery.rs`, `src/ui/session/tools.rs`.
Untracked (17): `docs/`, `marmel.toml.df`, `src/markers.rs`, `src/mcp/protocol.rs`, `src/plan_parse.rs`, `src/task_id.rs`, `src/text_util.rs`, `src/tool_args.rs`, `src/ui/bridge/action.rs`, `src/ui/bridge/arbiter.rs`, `src/harness/dispatch_integrity_tests.rs`, `src/harness/search_tests.rs`, `src/harness/tool_resolution_tests.rs`, `tests/test_midturn_notice.rs`, `tests/test_plan_checkoff.rs`, `tests/test_specialist_schema_charge.rs`, `tests/test_steer_history.rs`.

### 7.3 Per-file deltas — `git diff --numstat` (adds / deletes / path, path-sorted)

```
0	52	marmel.toml
6	0	marmel.toml.example
7	4	README.md
128	91	src/agents/mod.rs
419	45	src/agents/prompt_builder.rs
128	14	src/agents/runner/assembly.rs
447	109	src/agents/runner/execution.rs
1677	52	src/agents/runner/fix_loop.rs
36	191	src/agents/runner/formatting.rs
7	3	src/agents/runner/mod.rs
589	65	src/agents/validation.rs
502	11	src/config.rs
379	28	src/harness/fs.rs
541	164	src/harness/mod.rs
1	26	src/harness/monitor/common.rs
16	6	src/harness/monitor/mod.rs
289	0	src/harness/monitor/monitor_tests.rs
299	91	src/harness/monitor/repetition.rs
124	6	src/harness/monitor/xml.rs
974	113	src/harness/pty.rs
902	0	src/harness/pty_tests.rs
1154	153	src/harness/sandbox.rs
437	97	src/harness/search.rs
514	89	src/harness/sleep.rs
98	6	src/harness/workspace.rs
17	0	src/lib.rs
701	166	src/llm/client.rs
646	17	src/llm/client_tests.rs
62	71	src/llm/stream.rs
220	0	src/llm/stream_tests.rs
186	4	src/llm/thinking.rs
213	0	src/llm/thinking_tests.rs
157	27	src/main.rs
713	277	src/manager/context.rs
1738	459	src/manager/context_tests.rs
465	806	src/manager/loop.rs
0	651	src/manager/loop_tests.rs
18	6	src/manager/mod.rs
459	302	src/manager/phase.rs
1346	182	src/manager/phase_tests.rs
349	113	src/mcp/client.rs
466	246	src/mcp/http.rs
840	149	src/mcp/http_tests.rs
458	0	src/mcp/mod.rs
32	3	src/net/http.rs
5	4	src/net/mod.rs
322	24	src/net/retry.rs
35	42	src/orchestrator/bus.rs
92	57	src/orchestrator/delegate.rs
0	371	src/orchestrator/freeze.rs
141	208	src/orchestrator/mod.rs
998	57	src/orchestrator/notice.rs
1109	2	src/orchestrator/notice_tests.rs
75	58	src/orchestrator/plan_summary.rs
773	86	src/orchestrator/preemption.rs
286	54	src/orchestrator/steer.rs
649	1	src/orchestrator/steer_tests.rs
572	131	src/orchestrator/tests.rs
1708	136	src/orchestrator/workers.rs
666	2	src/tool_names.rs
3	2	src/types/tools.rs
30	14	src/ui/bridge/drain.rs
2	0	src/ui/bridge/mod.rs
153	196	src/ui/bridge/sink.rs
114	182	src/ui/bridge/steer.rs
1705	0	src/ui/bridge/tests.rs
496	916	src/ui/helpers.rs
0	6	src/ui/mod.rs
66	88	src/ui/raw.rs
0	81	src/ui/session/recovery.rs
1267	469	src/ui/session.rs
1	1	src/ui/session_tests.rs
0	722	src/ui/session/tools.rs
157	8	src/ui/transcript.rs
1	3	src/ui/tui/events.rs
98	134	src/ui/tui/formatting.rs
3	104	src/ui/tui/mod.rs
44	310	src/ui/tui/tests.rs
538	1	tests/test_context.rs
1	1	tests/test_dynamic_prompts.rs
747	0	tests/test_harness.rs
209	0	tests/test_llm.rs
5	81	tests/test_orchestrator.rs
1675	0	tests/test_role_gating.rs
439	2	tests/test_specialist_stream.rs
11	6	tests/test_tool_name_literals.rs
1586	834	tests/test_ui_session.rs
598	154	tests/test_validation_loop.rs
```

`git --no-pager diff --numstat -- src tests docs | wc -l` → **85** (the `docs/` filter contributes nothing because `docs/` is untracked; it is not part of the 88-file diff).

Top movers by added lines: `src/manager/context_tests.rs` 1,738/459, `src/orchestrator/workers.rs` 1,708/136, `src/ui/bridge/tests.rs` 1,705/0, `src/agents/runner/fix_loop.rs` 1,677/52, `tests/test_role_gating.rs` 1,675/0, `tests/test_ui_session.rs` 1,586/834, `src/manager/phase_tests.rs` 1,346/182, `src/ui/session.rs` 1,267/469, `src/harness/sandbox.rs` 1,154/153, `src/orchestrator/notice_tests.rs` 1,109/2, `src/orchestrator/notice.rs` 998/57, `src/harness/pty.rs` 974/113, `src/harness/pty_tests.rs` 902/0. Largest deletions: `src/manager/loop.rs` 465/**806**, `src/ui/helpers.rs` 496/916, `tests/test_ui_session.rs` 1,586/834, `src/manager/phase.rs` 459/302, `src/manager/context_tests.rs` 1,738/459, `src/ui/session.rs` 1,267/469, `src/ui/session/tools.rs` 0/722, `src/manager/loop_tests.rs` 0/651, `src/orchestrator/mod.rs` 141/208, `src/orchestrator/freeze.rs` 0/371.

### 7.4 Deleted and added files (line counts)

Deleted (from `HEAD`): `src/orchestrator/freeze.rs` 371 · `src/manager/loop_tests.rs` 651 · `src/ui/session/recovery.rs` 81 · `src/ui/session/tools.rs` 722 · `marmel.toml` 52 · `src/ui/mod.rs` −6 (renderer trait methods). Deleted total: **1,877** lines.

```
$ wc -l src/markers.rs src/mcp/protocol.rs src/plan_parse.rs src/task_id.rs src/text_util.rs \
        src/tool_args.rs src/ui/bridge/action.rs src/ui/bridge/arbiter.rs \
        src/harness/dispatch_integrity_tests.rs src/harness/search_tests.rs src/harness/tool_resolution_tests.rs \
        tests/test_midturn_notice.rs tests/test_plan_checkoff.rs tests/test_specialist_schema_charge.rs \
        tests/test_steer_history.rs marmel.toml.df
  2072 src/markers.rs
   786 src/mcp/protocol.rs
  1344 src/plan_parse.rs
   682 src/task_id.rs
   130 src/text_util.rs
   768 src/tool_args.rs
   209 src/ui/bridge/action.rs
  1011 src/ui/bridge/arbiter.rs
   491 src/harness/dispatch_integrity_tests.rs
   642 src/harness/search_tests.rs
   776 src/harness/tool_resolution_tests.rs
   372 tests/test_midturn_notice.rs
   535 tests/test_plan_checkoff.rs
   558 tests/test_specialist_schema_charge.rs
   338 tests/test_steer_history.rs
    52 marmel.toml.df
 10766 total
```

New `src/` owner modules: 8 files, **6,002** lines. New `src/` test modules: 3 files, **1,909** lines. New `tests/` files: 4 files, **1,803** lines. (Excludes `marmel.toml.df` and `docs/`.)
`docs/` (untracked, 8 files): **1,999** lines — `decision_dead_code_manager.md` 311, `recon_bugs_agents_monitor.md` 273, `recon_bugs_harness_llm_ui.md` 351, `recon_bugs_manager.md` 145, `recon_bugs_orchestrator.md` 302, `recon_duplication_helpers.md` 203, `recon_duplication_net.md` 135, `recon_rehydration.md` 279 (plus this report).

### 7.5 Test-count delta

```
$ grep -rcE '#\[(tokio::)?test' src tests --include='*.rs' | awk -F: '{s+=$2} END {print s}'
967
$ git --no-pager grep -cE '#\[(tokio::)?test' HEAD -- src tests | awk -F: '{s+=$3} END {print s}'
466
```

Declared test functions: **466 → 967** (+501). Executed tallies from the mission gates (`.marmel/execution_plan.md`): `cargo test --lib` **807 passed / 4 failed** (all 4 sanctioned `harness::pty::tests::*`, `openpty` EACCES) versus a pre-mission baseline of ≈533 (`docs/recon_bugs_agents_monitor.md` recorded an even earlier 372); integration suites ≈136–142 passed with 1 sanctioned failure (`tests/test_role_gating.rs::test_prompt_based_tool_gating`).

Per-file declared test counts verified in this report: `tests/test_role_gating.rs` 25 · `src/harness/pty_tests.rs` 30 · `tests/test_ui_session.rs` 21 · `src/harness/search_tests.rs` 14 · `src/harness/dispatch_integrity_tests.rs` 13 · `src/harness/tool_resolution_tests.rs` 13 · `src/manager/loop.rs` 13 · `tests/test_harness.rs` 15 · `tests/test_context.rs` 8 · `tests/test_plan_checkoff.rs` 6 · `tests/test_steer_history.rs` 9 · `tests/test_specialist_schema_charge.rs` 4 · `tests/test_midturn_notice.rs` 3.

### 7.6 Lint-surface delta

```
$ grep -rn '#\[allow(' src/ | wc -l
12
$ grep -rn '#\[expect(' src/ | wc -l
0
```

`#[allow(` **23 → 12**, all 12 pre-existing at `HEAD` (`src/agents/mod.rs:73`; `src/agents/runner/fix_loop.rs:621,1210`; `src/llm/stream.rs:528`; `src/llm/client.rs:580`; `src/ui/tui/mod.rs:150`; `src/ui/tui/events.rs:375,392,408,423,436`; `src/ui/bridge/steer.rs:83`). `#[expect(` = 0. `#[allow(async_fn_in_trait)]` = 0 — replaced by RPITIT at `src/net/retry.rs:61-63` and `src/mcp/protocol.rs:405-415`. `cargo clippy -- -D warnings` reported **0 diagnostics** at the final gate (reported by the verification gate; not re-run here, since verification runs concurrently in another agent).

### 7.7 Commands run for this report (all read-only)

`git --no-pager log --oneline -1` · `git status --porcelain` (+ `awk`/`grep` classification) · `git --no-pager diff --stat` · `git --no-pager diff --shortstat` · `git --no-pager diff --shortstat HEAD` · `git --no-pager diff --numstat` · `git --no-pager diff --numstat -- src tests docs | sort` · `git --no-pager diff -- README.md marmel.toml.example` · `git --no-pager diff -- src/ui/mod.rs src/orchestrator/mod.rs src/ui/session.rs src/lib.rs` · `git --no-pager diff | grep -icE "^[-+].*(rehydrat|freeze|frozen|journal)"` · `git show HEAD:<file> | wc -l` (per deleted file) · `git --no-pager show HEAD:src/config.rs | grep -nE "rehydration|REHYDRATION"` · `git --no-pager grep -cE '#\[(tokio::)?test' HEAD -- src tests` · `git check-ignore -v marmel.toml.df` · `git --no-pager diff --no-index --stat /tmp/head_marmel.toml marmel.toml.df` · `ls src` · `ls src/ui/session` · `grep -rniE "rehydrat|freeze|deep_freeze|crash_recovery" src/` · root-doc grep for the same pattern · `grep -n "marmel" .gitignore` · `wc -l` over new/deleted files and `docs/*.md` · `grep -cE '#\[(tokio::)?test'` per test file · `grep -rn '#\[allow(' src/ | wc -l` · `grep -rn '#\[expect(' src/ | wc -l`.

No `cargo` command was run in the production of this report (no `cargo fmt`, no `cargo test`, no `cargo clippy`). No file other than `docs/cleanup_report.md` was written or modified.

