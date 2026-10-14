# Comment Audit — Marmel Rust Code (UI / Tests / Manager / Orchestrator / Root Modules)

**Task:** Read-only comment audit of the remaining Marmel Rust code. **No source file was edited.**
**CWD:** `/home/fredrik/marmel/wip`

## Scope & method

Audited (69 files, all `.rs`):

* root modules: `src/config.rs`, `src/debug_log.rs`, `src/lib.rs`, `src/main.rs`, `src/markers.rs`,
  `src/plan_parse.rs`, `src/prompts.rs`, `src/task_id.rs`, `src/text_util.rs`, `src/tool_args.rs`, `src/tool_names.rs`
* `src/manager/**`, `src/orchestrator/**`, `src/ui/**` (incl. `ui/bridge/**`, `ui/tui/**`), `src/types/**`
* `tests/**`

Method: deterministic workspace scan (no internet). For every file: total lines, whole-line comment lines
(trimmed line starts with `//`), doc comments (`///` / `//!`), line comments (`//`), *pure separator bars*
(comment body matches only `[-=~#*.|_ ]+`), and trailing (end-of-line) comments. Removal candidates were
then verified by reading the surrounding code at each cited line (no line number in this document is
inferred; each was opened and inspected).

## Headline totals

| metric | value |
|---|---|
| files in scope | 69 |
| total lines | 53 427 |
| whole-line comment lines | 8 135 (15.2 %) |
| ├ doc comments (`///` / `//!`) | 5 428 |
| └ line comments (`//`) | 2 707 |
| pure separator bars (no label) | 94 |
| trailing (end-of-line) comments | 65 |
| `REQ-xxx` mentions | 150 (19 distinct ids: REQ-CORE-001..006, REQ-HARN-004, REQ-LOOP-002/003/004, REQ-ORCH-001..005, REQ-PLAN-001..004) |
| `t-xxx` gate refs | 412 |
| TODO / FIXME / XXX / HACK in scope | **0** |
| commented-out dead code in scope | **0** (see §1f) |

---

# 1. Removable (no information added)

Format: `file:line — category — "quote"`.

## 1a. `redundant-banner` — 94 pure separator bars

All 94 are separator-only lines (65–77 dash characters, no label). The local pattern is always
**bar / titled line(s) / bar** — verified at e.g. `src/orchestrator/workers.rs:654-656`,
`src/manager/context_tests.rs:721-733`, `src/markers.rs:272-280`. The **titled middle line(s) carry the
information and must stay**; the bars above/below are pure decoration and are removable without loss.

Exact line numbers of the 94 removable bars:

| file | bar lines |
|---|---|
| `src/config.rs` | 754, 761 |
| `src/manager/context_tests.rs` | 721, 733, 1386, 1407, 1690, 1701 |
| `src/manager/phase_tests.rs` | 949, 967, 1294, 1302, 1426, 1430, 1544, 1547 |
| `src/markers.rs` | 272, 280, 377, 389 |
| `src/orchestrator/notice.rs` | 73, 75, 776, 778 |
| `src/orchestrator/notice_tests.rs` | 56, 59, 896, 903, 1312, 1319 |
| `src/orchestrator/workers.rs` | 654, 656, 999, 1001, 1530, 1532, 1685, 1694, 1929, 1934 |
| `src/plan_parse.rs` | 1046, 1048, 1199, 1201 |
| `src/task_id.rs` | 485, 487 |
| `src/ui/bridge/tests.rs` | 533, 535, 995, 998, 1505, 1512, 1847, 1852 |
| `src/ui/helpers.rs` | 781, 788 |
| `src/ui/session.rs` | 1982, 1984, 2030, 2032 |
| `tests/test_context.rs` | 52, 56, 262, 265 |
| `tests/test_harness.rs` | 573, 581 |
| `tests/test_orchestrator.rs` | 100, 102, 163, 165, 194, 196, 218, 220 |
| `tests/test_role_gating.rs` | 353, 355, 850, 852 |
| `tests/test_specialist_stream.rs` | 600, 602 |
| `tests/test_ui_session.rs` | 1169, 1192, 1351, 1360, 1835, 1837, 2013, 2029 |
| `tests/test_validation_loop.rs` | 500, 524, 610, 626, 1019, 1026 |

**Keep (30 titled banners — informative section labels, not removable):**
`src/manager/loop.rs:386` (`// ---- TurnWatchdog ---…`), `:494`; `src/markers.rs:773, 951, 1110, 1217, 1517`;
`src/orchestrator/preemption.rs:821, 1002`; `src/orchestrator/steer_tests.rs:537, 893`;
`src/orchestrator/tests.rs:311, 442, 946`; `src/text_util.rs:52, 78, 98`;
`src/tool_args.rs:77, 360, 410, 433, 491, 568, 636`; `tests/test_harness.rs:110, 284, 466`;
`tests/test_plan_checkoff.rs:270, 314, 349`.

## 1b. `restates-code` — comments that only rename the next statement

Verified by reading the statement immediately following each comment.

**`src/orchestrator/preemption.rs`** — three *exact-duplicate* copies of the same pair of restatements:

* `src/orchestrator/preemption.rs:522 — restates-code — "// Preempt on matching model"` (next statement calls `spawns preempt_conflicting_stream`)
* `src/orchestrator/preemption.rs:527 — restates-code — "// Loop poll_control until Pause is received"` (next line is `let user_input = loop { if let StreamControl::Pause … }`)
* `src/orchestrator/preemption.rs:536 — restates-code — "// Subagent calls on_pause in a task"`
* `src/orchestrator/preemption.rs:559 — restates-code — "// Loop poll_control until Pause is received"` (duplicate of :527)
* `src/orchestrator/preemption.rs:766 — restates-code — "// Loop poll_control until Pause is received"` (duplicate of :527)

**`src/orchestrator/workers.rs`** (inside a test):

* `src/orchestrator/workers.rs:890 — restates-code — "// Cancel specific worker by task_id"` (statement: `cancel_active_worker(None, Some("t-001"))`)
* `src/orchestrator/workers.rs:896 — restates-code — "// Cancel remaining workers with cancel_all_active_workers"`

**`src/ui/transcript.rs`** — rustdoc that echoes one-line method signatures:

* `src/ui/transcript.rs:56 — restates-code — "/// Access the underlying records slice."`
* `src/ui/transcript.rs:61 — restates-code — "/// Check if the transcript has no records."`
* `src/ui/transcript.rs:66 — restates-code — "/// Number of records in the transcript."`
* `src/ui/transcript.rs:71 — restates-code — "/// Append a single record to the transcript."`
* `src/ui/transcript.rs:76 — restates-code — "/// Clear all records."`

**`src/manager/context_tests.rs`**:

* `src/manager/context_tests.rs:599 — restates-code — "// Verify budget target:"`
* `src/manager/context_tests.rs:607 — restates-code — "// Verify pins: [0] system prompt, [1] user goal."`
* `src/manager/context_tests.rs:46 — restates-code — "// Append a few messages; pins must survive."` (first clause restates the append loop; *if* trimmed, keep only "pins must survive")

**`src/orchestrator/notice_tests.rs`**:

* `:459 — restates-code — "// Notice is now resolved"`
* `:463 — restates-code — "// Verify Event::SteerResponse was emitted to the user"` (duplicates the `expect` message at :466)
* `:474 — restates-code — "// Verify steering history was updated with specialist details and synthesized response"`
* `:550, :561, :569 — restates-code —` same `// Verify …` pattern

**`src/ui/bridge/tests.rs`**:

* `:179 — restates-code — "// Verify the explanation was sent to the renderer as SteerResponse"`
* `:190 — restates-code — "// Verify status was also emitted"`

**`src/ui/tui/tests.rs`**:

* `:1228 — restates-code — "// Step left 5 times"`
* `:1234 — restates-code — "// Insert character at cursor"`
* `:1239 — restates-code — "// Step right 2 times"`
* `:1244 — restates-code — "// Delete backward"`
* `:1560 — restates-code — "// Now record plan start locally on renderer"`

**`tests/`**:

* `tests/test_context.rs:20 — restates-code — "// Add user and assistant messages"`
* `tests/test_dynamic_prompts.rs:191 — restates-code — "// 3. Discover catalog"`
* `tests/test_dynamic_prompts.rs:194 — restates-code — "// Verify skill discovered"`
* `tests/test_dynamic_prompts.rs:200 — restates-code — "// Verify custom archetype discovered"`
* `tests/test_harness.rs:106 — restates-code — "// Reset cancellation token after test"`
* `tests/test_ui_session.rs:493 — restates-code — "// Spawn mock streaming server"`
* `tests/test_ui_session.rs:503 — restates-code — "// Send chunk 1"`
* `tests/test_ui_session.rs:560 — restates-code — "// Spawn specialist stream in background"`
* `tests/test_ui_session.rs:659` and `:786 — restates-code / duplicate-narrative — "// Wait for first arbitration to finish"` (exact duplicate pair)

**Heuristic pool (for a follow-up sweep):** 59 comment lines in scope begin with
`Step / Assert / Check / Verify / Call / Set / Create / Add / Append / Run / Spawn / Build / Wait / Send /
Press / Record / Reset` — i.e. narration of the code below rather than rationale. The entries above are the
subset individually confirmed to add nothing; the remainder often encode *why* and should be judged
case-by-case.

## 1c. `over-explained-obvious` — rustdoc echoing the signature/type

69 short doc blocks whose text overlaps the item name. Representative confirmed list:

* `src/config.rs:90 — over-explained-obvious — "/// Source module path."` (item: `pub module: String`)
* `src/config.rs:94 — over-explained-obvious — "/// Optional model override for this specialist."` (also duplicated at `src/orchestrator/registry.rs:23`)
* `src/config.rs:162 — over-explained-obvious — "/// Orchestration block."`
* `src/debug_log.rs:61 — over-explained-obvious — "/// Write raw text safely to the debug log file."` (also :74, :315, :332)
* `src/main.rs:15 — over-explained-obvious — "/// Optional initial prompt to start the session."`
* `src/manager/context.rs:483, :497, :540 — over-explained-obvious — "/// The 70% target …"` (three near-identical copies of the same threshold statement)
* `src/manager/loop.rs:215 — over-explained-obvious — "/// Idle time since the last observed progress."` (also :301, :306)
* `src/manager/phase.rs:283 — over-explained-obvious — "/// The absolute path of the marmel directory."`
* `src/manager/phase.rs:382 — over-explained-obvious — "/// Returns \`true\` when a plan file exists on disk."`
* `src/orchestrator/delegation.rs:83 — over-explained-obvious — "/// The root depth (the Manager)."` (also :105)
* `src/orchestrator/delegation.rs:20 — duplicate-narrative — "/// Specialists table: role id -> { module, tools: [...] }."` (duplicate of `src/config.rs:42`)
* `src/orchestrator/notice.rs:649 — over-explained-obvious —` signature echo
* `src/plan_parse.rs:58 — over-explained-obvious — "/// Whether the checkbox is ticked."`
* `src/task_id.rs:75 — over-explained-obvious —` signature echo
* `src/types/wire.rs:7 — over-explained-obvious — "/// A message in the chat transcript."`
* `src/ui/mod.rs:31, :33, :41, :48, :60 — over-explained-obvious —` one-line trait-method echoes
* `src/ui/helpers.rs:131 — over-explained-obvious — "/// Strip XML thinking tags from a rendered line."`
* `src/ui/tui/mod.rs:83 — over-explained-obvious — "/// List of active specialist subagents."` (also :91)
* `src/ui/tui/render.rs:266 — over-explained-obvious — "/// Draw one complete frame using the current buffer state."`
* test-helper rustdoc: `src/manager/context_tests.rs:8, :1235`; `tests/test_context.rs:58, :91`;
  `tests/test_llm.rs:115`; `tests/test_midturn_notice.rs:47`; `tests/test_plan_checkoff.rs:24`;
  `tests/test_role_gating.rs:880`; `tests/test_specialist_schema_charge.rs:80` (identical text to
  `tests/test_midturn_notice.rs:47 — "/// One streamed assistant turn carrying only text."`), `:270`;
  `tests/test_specialist_stream.rs:626`

## 1d. `duplicate-narrative`

* `/// Shared abort / user-exit flags (trait-default abort surface).` ×5 —
  `src/ui/raw.rs:49`, `src/ui/tui/mod.rs:63`, `src/ui/bridge/tests.rs:8`, `src/ui/session_tests.rs:161`,
  `tests/test_ui_session.rs:60` → keep one (on the defining trait) + pointers.
* 4-line **Hermeticity** block ×6 verbatim — `tests/test_specialist_stream.rs:50-53, 161-164, 236-239,
  343-346, 434-437, 506-509` → keep ONE copy (or hoist to `tests/common/mod.rs`) + pointer lines.
  Note: the *rationale* (gate `t-033b`) must survive — collapse, do not delete.
* `// Loop poll_control until Pause is received` ×3 — `src/orchestrator/preemption.rs:527, 559, 766`.
* Duplicated H6 narrative: `src/manager/phase_tests.rs:949-967` block vs `src/markers.rs:951-958` +
  `966-970` — the same deleted-`extract_task_id` / `legacy_*` story is told twice; keep one canonical copy.
* Duplication scan (76 identical separator lines + these) also found `/// Build a manager rooted at a fresh
  temp plan dir for tests.` ×2 and other repeated doc lines.

## 1e. `stale` — wrong line numbers / pointers to non-existent code

**Action rule: keep the claim, drop or re-point the pointer.** Every row below was verified by opening the
cited location.

| # | stale comment | why it is wrong (verified) |
|---|---|---|
| 1 | `src/manager/phase_tests.rs:955 — stale — "(\`loop.rs:121-140\`)"` | `loop.rs:121` is the `DeadlineKind::HardCap` format string; `extract_task_id` no longer exists in code (only mentioned in comments `phase.rs:66, 165, 708`) |
| 2 | `src/manager/phase_tests.rs:960` and `:979 — stale — "(loop.rs:524: plan.check_off_on_success(task, \"ok\"))"` | `loop.rs:524` is `assert_eq!(budget.record(key), FailureVerdict::Continue { count: 1 });` |
| 3 | `src/markers.rs:966 — stale — "src/agents/mod.rs:122-136"` | those lines are the `recursion_granted` field doc + `IsolatedContext` fields |
| 4 | `src/markers.rs:967 — stale — "(character-for-character) in src/manager/phase.rs:222-243"` | `phase.rs:222-243` is `get_plan_start_time()`; `phase.rs` no longer parses markers — it re-exports `pub use crate::markers::MissionMarker;` at 260 |
| 5 | `src/manager/context_tests.rs:1405 — stale — "context.rs:530-539"` and `:1419 — "git HEAD src/manager/context.rs:518-539"` | 518-539 is now the `messages_removed()` / `tokens_reclaimed()` accessors |
| 6 | `src/orchestrator/mod.rs:208 — stale — "tools_manager.rs:914"` and `tests/test_orchestrator.rs:9`, `:106 — same` | `tools_manager.rs` does not exist anywhere in the repo |
| 7 | `src/orchestrator/notice.rs:1035 — stale — "src/harness/mod.rs:516"` | that line is `}` closing `dispatch_for_async`; real call site is `harness/mod.rs:797` (doc at 764) |
| 8 | `src/orchestrator/notice_tests.rs:1534 — stale — "src/harness/mod.rs:562"` | that line is an unrelated MCP-order comment |
| 9 | `src/orchestrator/workers.rs:1025 — stale — "(workers.rs:165 pre-fix)"` | line 165 is `{` |
| 10 | `tests/test_ui_session.rs:1180 — stale — "cancel_all_active_workers() (src/orchestrator/workers.rs:527)"` | function is at `workers.rs:807`; 527 is `))` |
| 11 | `tests/test_ui_session.rs:1183` and `:1253 — stale — "renderer.flush()? at src/ui/session.rs:506 (and :705)"` | `session.rs:506` is `"before turn-budget exit",`; real flush sites are 543/574/634/651/710/766 (+406/945/992/1085) |
| 12 | `src/orchestrator/steer.rs:3 — stale — "call_steer_arbitrator"` | no such function; real entries: `arbitrate_steer_context_stream` (134), `arbitrate_steer_stream` (547), `arbitrate_steer` (574) |
| 13 | `src/orchestrator/steer.rs:4 — stale — "REFERENCE_ORCHESTRATION_CONTRACT.md"` | file absent from repo (`docs/` holds only `recon_*`, `decision_*`, `cleanup_*` files + `baseline_test_run.md`) |
| 14 | `src/manager/phase.rs:673 — stale — "(\`task_id_override\` may be \`None\`)"` | no such identifier in repo; real params: `check_plan_on_deliverable(marker: Option<&MissionMarker>, task_id: Option<&str>, deliverable: &str)` (731-736), `check_plan_on_marker(task_id, deliverable)` (687) |
| 15 | `src/plan_parse.rs:13 — stale — table row "agents::prompt_builder::PLAN_TASK_RE"` | deleted symbol (`prompt_builder.rs:560, 802` call it "the old private PLAN_TASK_RE") |
| 16 | `src/markers.rs:1604 — stale — "LEGACY_MARKER_COMPLETE"` and `src/tool_names.rs:289 — "Terminal_Sleep"` | illustrative names; low priority |

**Accurate today but unpinned (fragile — convert to symbol references):**

* `src/manager/context.rs:63` and `:1128` cite `execution.rs:1034` — verified correct *now*
  (`execution.rs:1034` = `sync_specialist_tool_schema(&mut engine, &tools);` immediately before
  `if engine.should_compact()`), but nothing pins the number.
* `tests/test_specialist_schema_charge.rs:15` cites `execution.rs:345` — line 345 is inside the
  verdict-file error format block, **not** `build_turn_request` ⇒ stale-ish.
* `tests/test_ui_session.rs:1178` cites `src/harness/pty.rs:253` — 253 is a doc-comment bullet of
  `kill_and_reap` (255).
* `tests/test_ui_session.rs:1190` cites `src/orchestrator/mod.rs:276-281` — 276 is
  `crate::agents::AgentBlueprint::load_from_disk(&path).ok()` (roughly right, fragile).

## 1f. `commented-out-dead-code` — **none found**

Searched comment bodies ending in `;` / `{` / `=>` / `)`, bodies starting with
`let / if / for / match / return / assert! / use / fn / pub / Some / Ok / Err`, and macro syntax: **zero
hits in scope.** No `TODO` / `FIXME` / `XXX` / `HACK` anywhere in scope.

The nearest matches are **prose enumerating deleted code** (`src/config.rs:756-760`,
`src/manager/phase_tests.rs:955-961`, `src/tool_names.rs:65, 213`) and — critically — **real `cfg(test)`
witness functions**, which are *not* commented-out code and **MUST NOT be deleted**:
`src/manager/phase_tests.rs:971-1005` (`legacy_blob_scan_task_id`, `legacy_canned_token_passes_gate`,
`legacy_substring_says_failure`, `legacy_check_off_line`), `src/markers.rs:959` (`legacy_unbounded_task_id`),
`:971` (`legacy_substring_first_parse`). These are deliberate non-vacuous regression witnesses.

---

# 2. Must keep (with reason)

## 2a. Sandbox / security invariants

* `src/config.rs:28-31` — `SANDBOX_OPTOUT_ENV` name used verbatim in the attributable opt-out label.
* `src/config.rs:139-151` — fail-closed Landlock rationale + `SANDBOX_REFUSE_EXIT_CODE`; opt-out honoured
  from **either** source.
* `src/config.rs:274-302` — `sandbox_disabled` / config-merge precedence semantics.
* `src/config.rs:756-760`, `:787`, `:824`, `:890`, `:1003` — rationale for the **pty-free** sandbox test
  design (explains why the test cannot go through a pty; gate hazard documentation).
* `src/main.rs:71-97` — `SANDBOX_EXEC_ARG` re-entry sentinel; opt-out honoured before any probe.
* `src/orchestrator/registry.rs:165` — "never let a dispatched worker use tools the registry would gate".
* `src/orchestrator/tests.rs:946-951` — gate `t-055`: task-id grammar validates **before** any disk prompt
  join ⇒ path-traversal refusal.
* `src/task_id.rs` — grammar-authority comments around the `:485-487` banner and the `validate_task_id`
  docs: the grammar *is* the security boundary.
* Identity/exact-match invariants: `src/orchestrator/preemption.rs:85, 643, 678`;
  `src/orchestrator/workers.rs:2109, 2203`; `src/orchestrator/bus.rs:159` (why `key.contains("")` is always
  true, why truncated tags address nobody) and `tests/test_steer_history.rs:298-303`.

## 2b. UTF-8 truncation invariants

* `src/text_util.rs:1-9` and `:20` — never panics at a non-char boundary; dedup C7 note.
* `src/manager/context.rs:78-82` — REQ-CORE-006 satisfied at call sites; records the deleted
  `utf8_safe_slice` copy.
* `src/markers.rs:566` — ASCII bytes never occur inside a multi-byte UTF-8 sequence ⇒ justifies byte scanning.
* `src/tool_args.rs:25-27` — all clipping goes through `text_util::truncate_chars`.
* `src/ui/helpers.rs:391, 413, 609, 630, 641, 655` — 120-char presentation budget; the
  "byte index is not a char boundary" panic hazard.
* `tests/test_harness.rs:124-130` and `:511`; `src/manager/phase_tests.rs:1665, 1674` (invalid-UTF-8 plan);
  `src/types/wire.rs:143`.

## 2c. REQ-xxx traceability (150 mentions) — do not strip

Per-file REQ mention counts: `src/manager/context.rs` 28 (module doc 5, 8, 10, 22, 31, 73, 78 plus
90, 260, 261, 328, 372, 488, 591, 596, 605, 733, 747, 930, 932, 933, 1049, 1053, 1055, 1057, 1078, 1106,
1110), `src/orchestrator/mod.rs` 23, `src/manager/phase.rs` 19, `src/manager/loop.rs` 12,
`src/orchestrator/tests.rs` 10, `src/orchestrator/delegate.rs` 9, `src/manager/phase_tests.rs` 9,
`tests/test_role_gating.rs` 9, `src/ui/session.rs` 8, `src/manager/context_tests.rs` 7,
`src/orchestrator/registry.rs` 5, `src/markers.rs` 5, `src/orchestrator/delegation.rs` 2,
`tests/test_ui_session.rs` 2, `tests/test_context.rs` 1, `src/tool_names.rs` 1.
Plus **412 `t-xxx` gate references** across the scope.

`src/markers.rs:1529-1545` and `:1552-1572` + `MIN_JUSTIFICATION_LEN` (`:1550`) are load-bearing: the
whitelist **ratchet** test requires ≥40-char justifications, i.e. the comments are part of the test contract.

## 2d. Gate-hazard docs that are accurate (keep)

* `src/orchestrator/mod.rs:315-326` — explains that *this file* declares `#[cfg(test)] mod notice_tests;`
  at lines 17-18, so line-by-line scans stop at line 18. **Verified accurate.**
* `src/markers.rs:1552-1572` — see 2c.

## 2e. Prompt / user-visible string text that tests assert on (comment + string must stay)

* `src/markers.rs:72-82` — `MARKER_COMPLETE` = `"MISSION COMPLETE"` (REQ-ORCH-005), `MARKER_FAILED`,
  `MARKER_REPLAN`, and the `*_LOWER` copies ← asserted in `tests/test_specialist_stream.rs:760, 590, 326`.
* `src/manager/context.rs:100-101` — `REBIRTH_ADVISORY_MESSAGE` exact text ←
  `src/manager/context_tests.rs:434`, `tests/test_context.rs:376`.
* `src/manager/phase.rs:108` — `PLAN_FILE` = `"execution_plan.md"` ← `src/orchestrator/tests.rs:543`.
* `src/orchestrator/notice.rs:1079` — `"[Steering Notice from Arbitrator — ID: {notice_id}]:…"` ←
  `src/orchestrator/tests.rs:878-882` (and the single-rendering guard, §4.2).
* `src/ui/bridge/arbiter.rs:280-282` — `"Steering instruction carried to the next seam — {reason}"` ←
  `src/ui/bridge/tests.rs:1200-1205` ("carried to the next seam").
* `src/ui/bridge/arbiter.rs:272-277` (sleep-finished status) and `:381, 384, 386, 391`
  (`"[Steering Arbitrator sleeping for …s…]"`) ← `src/ui/bridge/tests.rs:634, 638, 642`.
* `src/ui/bridge/sink.rs:175` and `src/ui/bridge/drain.rs:78` — `"\n[Steering Subtask — {} ({}):]\n"`.
* `src/ui/transcript.rs:186-188` — parses `"(SYSTEM: REBIRTH CHECKPOINT:"` (produced in `context.rs`,
  asserted in `context_tests.rs:285, 1771, 1781`).
* `src/markers.rs:393` — `FAILED_COLON_PREFIX` = `"FAILED:"`; `src/markers.rs:84-86` — benign-counter
  sanitize note.
* `src/prompts.rs:26-40` — `format_environment_block` (`"## Workspace & Environment"`,
  `"Current Working Directory (CWD):"`) asserted by `src/prompts.rs` tests ~47-52.
* Prompt bodies live in `prompts/*.md` via `include_str!` (`src/prompts.rs:3-24`,
  `src/orchestrator/steer.rs:26` `STEER_ARBITRATOR_SYSTEM_PROMPT`) — comment edits in `.rs` do not touch them,
  but `tests/test_dynamic_prompts.rs` asserts on prompt/frontmatter text (80, 81, 121, 147-157, 207-209).
* `src/orchestrator/tests.rs` critique fixtures `"VALIDATOR REJECTION"` (248, and the ~880-903 region).
* `src/tool_names.rs:23, 39, 42, 49, 51, 76, 205, 314, 634` ← `tests/test_llm.rs:57, 58` and
  `tests/test_specialist_stream.rs:63`.

## 2f. Test-protocol / hermeticity and concurrency-doc pairs (keep a single copy)

* The Hermeticity **rationale** (gate `t-033b`) — collapse the 6 copies to 1 (see §1d).
* `tests/test_harness.rs:806-849` — escape/isolation assertions.
* `src/ui/bridge/tests.rs:2065, 2085`; `src/config.rs:759-760` (pty-free sandbox rationale).
* `src/orchestrator/workers.rs:1485` ("Guards drop in registration order"), `:1771` (no lock held across
  `await`), `:326`, `:957`.
* The duplicated lock-pattern doc pairs found in `context.rs` / `notice.rs`:
  *"Intentionally a swap (not a once-only OnceLock)"*, *"Contention-free pattern: clone the sender out under
  the read lock, drop the guard…"*, *"must never happen while the guard is alive"* — keep one canonical copy
  each (dedupe, do not delete the reasoning).

---

# 3. Per-file comment counts (baseline for delta measurement)

Columns: **Total** lines · **Cmt** whole-line comment lines · **Doc** (`///`/`//!`) · **Line** (`//`) ·
**Sep** pure separator bars · **Trl** trailing end-of-line comments · **%** = Cmt/Total.

| file | Total | Cmt | Doc | Line | Sep | Trl | % |
|---|---:|---:|---:|---:|---:|---:|---:|
| src/config.rs | 1112 | 133 | 111 | 22 | 2 | 14 | 12.0 |
| src/debug_log.rs | 452 | 29 | 29 | 0 | 0 | 1 | 6.4 |
| src/lib.rs | 35 | 16 | 16 | 0 | 0 | 0 | 45.7 |
| src/main.rs | 396 | 50 | 43 | 7 | 0 | 0 | 12.6 |
| src/manager/context.rs | 1146 | 506 | 468 | 38 | 0 | 0 | 44.2 |
| src/manager/context_tests.rs | 2127 | 276 | 102 | 174 | 6 | 6 | 13.0 |
| src/manager/loop.rs | 551 | 185 | 174 | 11 | 0 | 0 | 33.6 |
| src/manager/mod.rs | 24 | 16 | 13 | 3 | 0 | 0 | 66.7 |
| src/manager/phase.rs | 826 | 389 | 323 | 66 | 0 | 0 | 47.1 |
| src/manager/phase_tests.rs | 1717 | 277 | 147 | 130 | 8 | 0 | 16.1 |
| src/markers.rs | 2072 | 564 | 473 | 91 | 4 | 1 | 27.2 |
| src/orchestrator/bus.rs | 217 | 75 | 69 | 6 | 0 | 0 | 34.6 |
| src/orchestrator/delegate.rs | 272 | 53 | 25 | 28 | 0 | 0 | 19.5 |
| src/orchestrator/delegation.rs | 108 | 29 | 29 | 0 | 0 | 0 | 26.9 |
| src/orchestrator/mod.rs | 569 | 175 | 115 | 60 | 0 | 0 | 30.8 |
| src/orchestrator/notice.rs | 1239 | 349 | 317 | 32 | 4 | 0 | 28.2 |
| src/orchestrator/notice_tests.rs | 1706 | 175 | 67 | 108 | 6 | 0 | 10.3 |
| src/orchestrator/plan_summary.rs | 176 | 14 | 14 | 0 | 0 | 0 | 8.0 |
| src/orchestrator/preemption.rs | 1265 | 174 | 107 | 67 | 0 | 0 | 13.8 |
| src/orchestrator/registry.rs | 212 | 36 | 29 | 7 | 0 | 0 | 17.0 |
| src/orchestrator/steer.rs | 813 | 154 | 142 | 12 | 0 | 0 | 18.9 |
| src/orchestrator/steer_extractor.rs | 118 | 3 | 1 | 2 | 0 | 0 | 2.5 |
| src/orchestrator/steer_tests.rs | 1176 | 61 | 39 | 22 | 0 | 2 | 5.2 |
| src/orchestrator/tests.rs | 1161 | 148 | 81 | 67 | 0 | 4 | 12.7 |
| src/orchestrator/workers.rs | 2319 | 405 | 258 | 147 | 10 | 1 | 17.5 |
| src/plan_parse.rs | 1344 | 258 | 211 | 47 | 4 | 0 | 19.2 |
| src/prompts.rs | 54 | 3 | 3 | 0 | 0 | 0 | 5.6 |
| src/task_id.rs | 682 | 157 | 136 | 21 | 2 | 0 | 23.0 |
| src/text_util.rs | 130 | 38 | 29 | 9 | 0 | 3 | 29.2 |
| src/tool_args.rs | 768 | 202 | 142 | 60 | 0 | 1 | 26.3 |
| src/tool_names.rs | 742 | 214 | 126 | 88 | 0 | 0 | 28.8 |
| src/types/mod.rs | 10 | 1 | 1 | 0 | 0 | 0 | 10.0 |
| src/types/tools.rs | 579 | 7 | 5 | 2 | 0 | 2 | 1.2 |
| src/types/wire.rs | 293 | 9 | 9 | 0 | 0 | 0 | 3.1 |
| src/ui/bridge/action.rs | 209 | 55 | 46 | 9 | 0 | 0 | 26.3 |
| src/ui/bridge/arbiter.rs | 1011 | 218 | 189 | 29 | 0 | 1 | 21.6 |
| src/ui/bridge/drain.rs | 280 | 9 | 1 | 8 | 0 | 0 | 3.2 |
| src/ui/bridge/mod.rs | 15 | 1 | 1 | 0 | 0 | 0 | 6.7 |
| src/ui/bridge/sink.rs | 401 | 23 | 12 | 11 | 0 | 0 | 5.7 |
| src/ui/bridge/steer.rs | 233 | 18 | 9 | 9 | 0 | 0 | 7.7 |
| src/ui/bridge/tests.rs | 2236 | 158 | 63 | 95 | 8 | 1 | 7.1 |
| src/ui/helpers.rs | 967 | 127 | 74 | 53 | 2 | 1 | 13.1 |
| src/ui/mod.rs | 140 | 42 | 42 | 0 | 0 | 0 | 30.0 |
| src/ui/raw.rs | 229 | 35 | 25 | 10 | 0 | 0 | 15.3 |
| src/ui/session.rs | 2437 | 436 | 205 | 231 | 4 | 0 | 17.9 |
| src/ui/session_tests.rs | 437 | 3 | 1 | 2 | 0 | 2 | 0.7 |
| src/ui/transcript.rs | 618 | 61 | 38 | 23 | 0 | 0 | 9.9 |
| src/ui/tui/events.rs | 842 | 38 | 22 | 16 | 0 | 0 | 4.5 |
| src/ui/tui/formatting.rs | 919 | 68 | 57 | 11 | 0 | 0 | 7.4 |
| src/ui/tui/mod.rs | 1083 | 82 | 69 | 13 | 0 | 0 | 7.6 |
| src/ui/tui/render.rs | 1107 | 47 | 18 | 29 | 0 | 10 | 4.2 |
| src/ui/tui/tests.rs | 2148 | 109 | 7 | 102 | 0 | 6 | 5.1 |
| tests/common/mod.rs | 23 | 6 | 6 | 0 | 0 | 1 | 26.1 |
| tests/test_agent.rs | 127 | 10 | 2 | 8 | 0 | 0 | 7.9 |
| tests/test_context.rs | 584 | 59 | 23 | 36 | 4 | 0 | 10.1 |
| tests/test_dynamic_prompts.rs | 210 | 18 | 8 | 10 | 0 | 1 | 8.6 |
| tests/test_harness.rs | 850 | 87 | 42 | 45 | 2 | 2 | 10.2 |
| tests/test_llm.rs | 310 | 23 | 17 | 6 | 0 | 1 | 7.4 |
| tests/test_midturn_notice.rs | 372 | 41 | 31 | 10 | 0 | 0 | 11.0 |
| tests/test_monitor.rs | 61 | 6 | 1 | 5 | 0 | 0 | 9.8 |
| tests/test_orchestrator.rs | 304 | 58 | 36 | 22 | 8 | 2 | 19.1 |
| tests/test_plan_checkoff.rs | 535 | 74 | 43 | 31 | 0 | 0 | 13.8 |
| tests/test_role_gating.rs | 2026 | 210 | 93 | 117 | 4 | 0 | 10.4 |
| tests/test_specialist_schema_charge.rs | 558 | 115 | 93 | 22 | 0 | 0 | 20.6 |
| tests/test_specialist_stream.rs | 974 | 84 | 26 | 58 | 2 | 0 | 8.6 |
| tests/test_steer_history.rs | 338 | 68 | 62 | 6 | 0 | 1 | 20.1 |
| tests/test_tool_name_literals.rs | 134 | 35 | 17 | 18 | 0 | 0 | 26.1 |
| tests/test_ui_session.rs | 2873 | 385 | 178 | 207 | 8 | 1 | 13.4 |
| tests/test_validation_loop.rs | 1425 | 145 | 17 | 128 | 6 | 0 | 10.2 |
| **TOTAL** | **53 427** | **8 135** | **5 428** | **2 707** | **94** | **65** | **15.2** |

**Reproduce the counts** with a small Python script that walks `src` (the scoped modules above) plus `tests`
and counts lines whose trimmed text starts with `//`; classify doc = `///` / `//!`; separator = comment body
matching only `[-=~#*.|_ ]+`. Trailing comments counted separately (text after code on the same line).

> **Caveat:** the counts include **whole-line comments only**. The 65 trailing (end-of-line) comments are
> excluded from the removal lists in §1 and would not change §3 numbers if touched.

---

# 4. Source-scanning guard tests (comments that can break tests)

These tests read **`src/` files as text** and assert on substrings/counts. They decide whether a comment
deletion (or addition) is safe.

| # | guard | what it scans | comments included? | verdict for comment deletion |
|---|---|---|---|---|
| 1 | `tests/test_tool_name_literals.rs` (BASELINE table) | every `src/**/*.rs` except `tool_names.rs` | **YES** (whole-file content) | Safe **in scope** (see 4.1) |
| 2 | `src/orchestrator/tests.rs:893-944` needle count | all of `src/` via `walk_src_for` | **YES** | Safe (4.2) |
| 3 | `src/orchestrator/steer_tests.rs:1106-1176` | `include_str!("steer.rs")` | **stripped** | Safe (4.3) |
| 4 | `src/ui/bridge/tests.rs:2095-2148` | `include_str!("sink.rs")` | **stripped** | Safe (4.4) |
| 5 | `src/ui/bridge/tests.rs:2150-2236` | `src/ui/bridge/**` whole files | **YES** | Safe today — but *adding* quoted tool names in comments breaks it (4.5) |
| 6 | `src/ui/bridge/action.rs:104-152` | `include_str!("action.rs")` up to `#[cfg(test)]` | **NO exclusion** | Adding a quoted alias in a doc comment breaks it (4.6) |
| 7 | `src/ui/bridge/arbiter.rs:769-827` | `include_str!("arbiter.rs")` | **stripped** | Safe (4.7) |
| 8 | `src/markers.rs` single-owner grammar guards | `src/` production libraries | **excluded** | Safe, with one rule (4.8) |
| 9 | `src/manager/context_tests.rs:2082-2127` | doc-comment lines of `context.rs` | **doc lines only** | **BROKEN by deleting comments — load-bearing (4.9)** |
| 10 | `src/config.rs:1090-1111` | `src/harness/pty.rs` whole file | YES | Out-of-scope file; deleting in-scope comments safe (4.10) |
| 11 | `src/tool_names.rs:584-613` | `src/harness/mod.rs`, `src/orchestrator/steer.rs` | **YES** | Safe today; never add quoted tool names to `steer.rs` comments (4.11) |
| 12 | `tests/test_steer_history.rs:304-338` | `include_str!("../src/orchestrator/bus.rs")` | `//` lines filtered | Safe (4.12) |
| 13 | `tests/test_role_gating.rs:1423-1486` | `src/agents/runner/fix_loop.rs` literals | **skips** `//` and `/* */` | Safe (4.13) |

## 4.1 `tests/test_tool_name_literals.rs` — BASELINE table (`:52-79`)

* walk: `:86-99` (`walk_src`, skips `tool_names.rs`); test body `:101-134`; `KNOWN_TOOL_NAMES` `:19-47`.
* The counter counts occurrences of `"{name}"` in the **whole file content**, so **comments are included**.
* **In-scope BASELINE rows and the comments that justify them:**
  * `:54` — `("src/config.rs", "delegate_task", 2)` justified by comment `:53`
  * `:56` — `("src/debug_log.rs", "delegate_task", 1)` justified by comment `:55`
  * `:65` — `("src/orchestrator/steer_tests.rs", "delegate_task", 1)` justified by comment `:64`
  * `:67` — `("src/types/wire.rs", "read_file", 1)` justified by comment `:66`
* Out-of-scope rows `:58-63` (`monitor_tests.rs`, `xml.rs`). Note for completeness: the `xml.rs` rows are
  justified **by comment text** — the actual counted hits are comments at
  `src/harness/monitor/xml.rs:16, 18, 219, 220, 379` (`read_file`×4 + `write_file`×1). Deleting **those**
  comments **breaks the guard** — but that file is **out of this audit's scope**.
* **Verified in-scope counted hits are all CODE lines:** `src/config.rs:582, 583` (TOML fixture),
  `src/debug_log.rs:445` (assert), `src/orchestrator/steer_tests.rs:471` (`normalize_steer_decision` input),
  `src/types/wire.rs:241` (JSON fixture).
* **Conclusion:** the BASELINE-justifying comments live in `tests/`, which is **never scanned** ⇒ they are
  documentation only and **safe to delete**; the **code literals are the load-bearing part**.

## 4.2 `src/orchestrator/tests.rs:893-944` — single-rendering guard

`test_notice_transcript_is_rendered_in_exactly_one_place`: needle assembled at `:905`
(`format!("[Steering {} Arbitrator", "Notice from")`); `walk_src_for` at `:908` and `:928-944` counts
matches over **all of `src/`, comments included**; asserts `hits.len() == 1`, `total == 1`, and `hits[0]`
ends with `orchestrator/notice.rs`. Only production occurrence: `src/orchestrator/notice.rs:1079`.
⇒ **Deleting comments is safe.** The invariant to preserve: **no comment anywhere in `src/` may contain
`[Steering Notice from Arbitrator`.**

## 4.3 `src/orchestrator/steer_tests.rs:1106-1176`

`steer_production_region_retypes_no_sleep_default_or_clamp_literals`: `include_str!("steer.rs")` (`:1116`);
production region = split on `#[cfg(test)]` (`:1117-1120` — `steer.rs` has **no** `cfg(test)`, so the whole
file); **comments stripped** at `:1121-1129`; requires `fn sleep_steer_decision(` (`:1132`); forbids
`unwrap_or(5`, `unwrap_or(300`, `Some(5)`, `clamp(1,`, `clamp(300,`, `MIN_SINGLE_SLEEP_SECONDS`,
`get("seconds"`, `get("duration"`, `get("duration_seconds"`, `999999`, `300` (`:1136-1155`); exactly 1
`sleep_duration_secs(` (`:1160-1165`); exactly 1 `SLEEP_DEFAULT_SECS` (`:1166-1171`); `is_sleep_tool_name(`
(`:1172-1175`). ⇒ comments are stripped ⇒ deletion safe.

## 4.4 `src/ui/bridge/tests.rs:2095-2148`

`test_bridge_sink_sleep_site_redeclares_no_default_and_no_clamp`: `include_str!("sink.rs")` (`:2100`),
comments stripped (`:2101-2109`), requires `clamped_sleep_secs(` (`:2111-2115`), forbids `unwrap_or(5`,
`unwrap_or(SLEEP_DEFAULT_SECS`, `MIN`/`MAX_SINGLE_SLEEP_SECONDS`, `SLEEP_MIN_SECS`, `SLEEP_MAX_SECS`
(`:2117-2125`), and asserts the `host_sleep_request` **body contains no ASCII digit** (`:2134-2147`) — body
sliced from the comment-stripped `code`. ⇒ deletion safe.

## 4.5 `src/ui/bridge/tests.rs:2150-2236`

`test_no_raw_tool_name_literals_in_bridge_tree`: walks `src/ui/bridge` (`:2212`) reading **whole files
(comments included)**; needles built at runtime from `crate::tool_names` consts (`:2158-2189`); zero
tolerance. **Verified:** no in-scope comment in `src/ui/bridge` currently contains a quoted tool name ⇒
deletion safe; **adding** one would break it.

## 4.6 `src/ui/bridge/action.rs:104-152`

`bridge_action_module_retains_no_tool_alias_literals`: `include_str!("action.rs")` (`:111`); production =
split on `#[cfg(test)]` (`:112-115`, real attr at `action.rs:89`); **no comment exclusion** ⇒ *any* doc
comment in the production region quoting a tool alias breaks the guard. Requires
`normalize_steer_subtask_action(`; forbids `fn normalize_`, `.parse::<`, `FromStr`.

## 4.7 `src/ui/bridge/arbiter.rs:769-827`

`arbitrator_upper_budget_is_not_the_sleep_tool_ceiling`: `include_str!("arbiter.rs")` (`:775`; real
`cfg(test)` at `:671`), comments stripped (`:782-790`), forbids `SLEEP_MAX_SECS`, requires `SLEEP_MIN_SECS`,
`SLEEP_DEFAULT_SECS`, `MAX_SINGLE_SLEEP_SECONDS`, `MAX_CHAINED_SLEEP_SECONDS`. ⇒ deletion safe (but the
*required* identifiers must remain in code).

## 4.8 `src/markers.rs` single-owner grammar guards

* `only_markers_module_spells_marker_literals_in_production_code` (`:1655-1689`) scanning
  `production_marker_libraries` (`:1617-1649`) over `src/` (skips `*_tests.rs` / `tests.rs`);
  `MARKER_LITERALS = [MARKER_COMPLETE, MARKER_REPLAN, MARKER_FAILED]` (`:1523`); whitelist
  `KNOWN_EXTERNAL_MARKER_SITES` is **empty** (`:1546`) with ratchet `whitelist_violations` (`:1694-1725`) and
  `MIN_JUSTIFICATION_LEN = 40` (`:1550`).
* `external_marker_whitelist_is_empty` (`:1730-1750`).
* `abort_sites_call_the_body_owner_instead_of_building_it` (`:1380-1413`) scanning
  `src/orchestrator/mod.rs`, `delegate.rs`, `agents/mod.rs`, `agents/runner/execution.rs` (brace-aware):
  requires `aborted_deliverable(`; forbids `"Task aborted by user instruction"`, `"Task {context}"`,
  `"\n\nFAILED"`.
* consumer guard (`:2042-2070`): each `MARKER_*` const must have a production consumer outside `markers.rs`.
* `production_lines_numbered` (`:1971-2011`): non-brace-aware mode takes lines until the first line starting
  with `#[cfg(test)]`, then **filters comment lines** (`//`, `/*`, `*`); brace-aware mode skips the
  `cfg(test)` item. ⇒ **comments are excluded from all markers guards ⇒ safe to delete**, with one rule:
  **a comment line must never START with `#[cfg(test)]` in a production region** (it would truncate the scan).

## 4.9 ⚠ `src/manager/context_tests.rs:2082-2127` — the ONLY guard broken by deleting comments

`tool_schema_docs_name_every_live_declarer_and_scope_the_lower_bound_caveat`:
`include_str!("context.rs")`, builds a string from **doc-comment lines only** (`//!` + `///`), then asserts
the docs contain: `"lower bound"`, `build_manager_context`, `sync_manager_tool_schema`,
`charge_engine_tool_schema`, `build_specialist_context`, `sync_specialist_tool_schema`,
`"test_specialist_schema_charge"`, `"declares nothing at all"`, `"never goes through"`.
⇒ **The named doc comments in `src/manager/context.rs` are load-bearing**: the module doc (~lines 1-90) and
the factory docs (~1049-1130). **Do not delete or paraphrase those phrases.**

## 4.10 `src/config.rs:1090-1111`

`harness_call_site_wires_both_config_knobs` reads `src/harness/pty.rs` **whole file (comments included)**
requiring `crate::config::sandbox_opt_out(opt_out_from_env())` (pty.rs:718),
`crate::config::effective_command_timeout_secs(` (pty.rs:892), `opt_out_from_env()` (pty.rs:655). The
scanned file is out of scope; in-scope comment deletions cannot affect it.

## 4.11 `src/tool_names.rs:584-613`

`routed_call_sites_do_not_retype_tool_alias_literals` — whole-file scan of `src/harness/mod.rs` and
**`src/orchestrator/steer.rs` (in scope)**, **comments included**, needles = `"{alias}"` / `"{canonical}"`
from `TOOL_ALIAS_TABLE` + `CANONICAL_TOOL_NAMES`. Verified **zero hits** in `steer.rs` today (its comments
do not quote tool names) ⇒ deletion safe; adding a quoted name in a `steer.rs` comment would break it.

## 4.12 `tests/test_steer_history.rs:304-338`

`bus_correlation_keeps_no_substring_fallback`: `include_str!("../src/orchestrator/bus.rs")`, only `//` lines
filtered (`:309`), forbids substring needles, requires `entry_names_notice_id(` and `same_steering_inquiry(`.
⇒ comment deletion safe (required identifiers must stay in code).

## 4.13 `tests/test_role_gating.rs:1423-1486+`

`fix_loop_source_has_no_raw_tool_name_string_literals` reads `src/agents/runner/fix_loop.rs` and extracts
string literals via `string_literals_in_source` (`:1488-…`) which **skips `//` and `/* */`** ⇒ comment
deletion safe there.

## 4.14 Not source-scanning (temp-fixture based — unaffected)

`tests/test_harness.rs:110-130, 466-548, 800-850`; `src/orchestrator/workers.rs:1859`;
`src/ui/helpers.rs:116, 938`.

## 4.15 Other verified facts

* **No doctests in scope** (only a ```` ```text ```` fence at `src/markers.rs:312-316`;
  `src/plan_parse.rs:251, 264, 266` mention fences inline) ⇒ `cargo test --doc` is unaffected by comment
  removal.
* CI (`.github/workflows/ci.yml`) runs `cargo fmt --all -- --check` and
  `cargo clippy --all-targets --all-features -- -D warnings`; rustfmt does not reformat comments; there is
  **no `#![warn(missing_docs)]`** in `src/lib.rs` ⇒ removing rustdoc will not fail CI.

## 4.16 Load-bearing vs safe — edit rules

**Load-bearing comments (must not be removed or paraphrased):**

1. `src/manager/context.rs` module doc (~1-90) + factory docs (~1049-1130) containing every phrase listed
   in §4.9 — **the only comment-deletion-breaking guard in scope.**
2. The ≥40-char justifications behind the `src/markers.rs` whitelist ratchet (§2c / §4.8) — the ratchet
   *requires* them (`MIN_JUSTIFICATION_LEN = 40`).
3. Any comment inside a `BASELINE` table row rationale in `tests/test_tool_name_literals.rs` is **not**
   scanned (tests/ is never read by the walker) — safe, but keep them for human auditability if you prefer.

**Safe to delete, but obey these rules when editing nearby comments:**

* Never introduce a quoted tool name/alias (`"read_file"`, `"delegate_task"`, …) in a comment under `src/`
  — §4.1 counts whole files; §4.5 (`src/ui/bridge/**`), §4.6 (`action.rs` production region) and §4.11
  (`steer.rs`) have **zero tolerance**.
* Never write a comment containing `[Steering Notice from Arbitrator` anywhere in `src/` (§4.2 asserts
  exactly one occurrence).
* Never let a line in a production region **start** with `#[cfg(test)]` inside a comment (§4.8 truncates the
  scan at such a line).
* Guards §4.3, §4.4, §4.7, §4.12, §4.13 strip comments ⇒ they are immune to deletion but their **required
  code identifiers** must survive any edit.

---

# 5. Tests asserting on prompt / string text adjacent to comments

| assertion site | asserted text (fixture/needle) | production string + adjacent comment |
|---|---|---|
| `tests/test_specialist_stream.rs:326, 590, 760` | `MISSION COMPLETE` / failure & replan markers | `src/markers.rs:72-82` (`MARKER_COMPLETE` etc., REQ-ORCH-005 comment block) |
| `tests/test_context.rs:376`; `src/manager/context_tests.rs:434` | exact `REBIRTH_ADVISORY_MESSAGE` text | `src/manager/context.rs:100-101` (comment cites REQ-CORE-006) |
| `src/orchestrator/tests.rs:543` | `"execution_plan.md"` | `src/manager/phase.rs:108` `PLAN_FILE` (REQ traceability comment) |
| `src/orchestrator/tests.rs:878-882` (and 893-944) | notice rendering, `[Steering Notice from Arbitrator …` | `src/orchestrator/notice.rs:1079` (commented single-owner region 1035/1079) |
| `src/ui/bridge/tests.rs:634, 638, 642`; `:2060-2091` | `"[Steering Arbitrator sleeping for …s…]"`, sleep-finished status | `src/ui/bridge/arbiter.rs:272-277, 381, 384, 386, 391` (comments explain the ceiling semantics) |
| `src/ui/bridge/tests.rs:1200-1205` | `"carried to the next seam"` | `src/ui/bridge/arbiter.rs:280-282` |
| `src/ui/bridge/tests.rs:188, 193` | `"Instruction queued for next turn"` | `src/ui/bridge/steer.rs` / drain-sink rendering (`sink.rs:175`, `drain.rs:78` `"\n[Steering Subtask — {} ({}):]\n"`) |
| `src/manager/context_tests.rs:285, 1771, 1781` | `"(SYSTEM: REBIRTH CHECKPOINT:"` | produced in `src/manager/context.rs`, parsed at `src/ui/transcript.rs:186-188` |
| `src/orchestrator/steer_tests.rs:471` (`"delegate_task"` literal) + decision strings `"Sleep"` | `normalize_steer_decision` inputs | `src/orchestrator/steer.rs` (its header comments §1e are stale — fix the pointer, keep the string) |
| `tests/test_role_gating.rs:1400-1412` | `"VERDICT REJECTED"`, `"is forbidden for caller"` | `src/manager/loop.rs` / validation fixtures (comment-adjacent) |
| `src/types/wire.rs:241` | JSON fixture containing `"read_file"` | BASELINE row `tests/test_tool_name_literals.rs:67` + justifying comment `:66` |
| `src/config.rs:582-583` | TOML fixture containing `"delegate_task"` ×2 | BASELINE row `:54` + comment `:53` |
| `src/debug_log.rs:445` | log assertion containing `"delegate_task"` | BASELINE row `:56` + comment `:55` |
| `src/prompts.rs:47-52` (own tests) | `"## Workspace & Environment"`, `"Current Working Directory (CWD):"` | `src/prompts.rs:26-40` `format_environment_block` |
| `tests/test_dynamic_prompts.rs:80, 81, 121, 147-157, 207-209` | prompt/frontmatter text | `prompts/*.md` via `include_str!` (`src/prompts.rs:3-24`) — comment edits in `.rs` cannot affect these |
| `src/orchestrator/tests.rs:248` (+ ~880-903) | `"VALIDATOR REJECTION"` | critique fixture, comment-adjacent |
| `src/markers.rs` internal fixtures | `:763, 768, 787, 791, 840, 879, 1076, 1132, 1297, 1301` | marker literals near banner comments (bars removable, §1a) |

---

# 6. Delta-measurement protocol

1. Re-run the §3 counting script before and after any cleanup; diff the **Cmt** column per file. The
   baseline is the table above (8 135 comment lines / 53 427 total, 15.2 %).
2. Suggested removal order (lowest risk first):
   1. **94 separator bars** (§1a) — zero semantic risk, no guard in §4 is affected (bars are comment lines;
      the markers guards filter comment lines, and `tests/test_role_gating.rs` skips comments).
   2. **`restates-code`** (§1b) — confirm each is not a `t-xxx`/`REQ-xxx` anchor first.
   3. **`duplicate-narrative`** (§1d) — collapse to a single canonical copy + pointer.
   4. **`over-explained-obvious`** rustdoc (§1c) — never touch `src/manager/context.rs` doc phrases (§4.9).
   5. **`stale`** (§1e) — *fix the pointer, keep the claim*; do not delete the invariant sentence.
3. Verify after each batch:
   `cargo test --lib`, `cargo test --test test_tool_name_literals`, `cargo test --test test_steer_history`,
   `cargo test --test test_role_gating`, `cargo clippy --all-targets --all-features -- -D warnings`,
   `cargo fmt --all -- --check`.
4. Never delete the `#[cfg(test)]` `legacy_*` witness functions listed in §1f — they look like dead code but
   are the non-vacuous half of the regression guards.

**Read-only attestation:** this audit modified no `.rs` file; only this report was written
(`docs/research_comments_ui_tests.md`).
