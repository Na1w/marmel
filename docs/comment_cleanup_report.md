# Comment Cleanup + Swedish→English + Portability — Authoritative Mission Report (t-017)

**This file is the single authoritative record of the finished mission.**
The older `docs/cleanup_report.md` belongs to an **earlier mission** (the pre-mission refactor/dedup
pass) and is **superseded by this file**; it was left byte-for-byte untouched. Every figure below is
traceable to a source document or to the task record; where a figure was never captured the cell reads
**not recorded**.

Sources synthesised (no test suite was re-run and no code was re-audited for this report):
`docs/research_swedish_inventory.md`, `docs/research_comments_core.md`, `docs/research_comments_ui_tests.md`,
`docs/baseline_test_run.md`, `docs/verification_harness.md` (t-006 audit), `docs/verification_orchestrator.md`
(t-027 audit), `docs/verification_tests.md` (t-012 audit), `docs/verification_final.md` (t-015 FINAL GATE),
`docs/t007_comment_cleanup_report.md`, `docs/t011_comment_cleanup_report.md`, and the per-task reports in the
plan record (`.marmel/execution_plan.md`).

Provenance (t-015 §0): HEAD = `974b601 "Refactor part 1"`, branch `1.0.0-refactor`; work tree = 89 dirty
files; `git diff HEAD --stat` = **86 files, +711 / −850 (net −139 lines)**; cargo/rustc 1.98.0 (`88d9e12ae`).

---

## 1. Scope & ground rules

| Rule | Statement | Enforcement evidence |
|---|---|---|
| Zero behaviour change | No edit to code tokens, string literals, identifiers, match arms, `cfg`, macros, control flow, assertions or fixture payloads; fmt-only churn tolerated. | String-aware comment-stripped token multiset diff vs `git show HEAD:<file>` in every audit: **0 code-token deltas** in all 14 `src/orchestrator/**` files (t-027 §2), 15/15 non-`test_ui_session` files in `tests/**` (t-012 check 6), 10/10 `src/ui` files (`code_identical=True`, t-026 audit §2), 11/14 harness core files with the 3 exceptions attributable to portability tasks (t-006 audit check 4). |
| Single sanctioned exception | **t-013** — the pending-decision merge heuristic in `src/ui/transcript.rs` (see §7). | t-013 audit: predicate is pre-existing, 3 new pin/regression tests, floor 807/5 → 810/5. |
| UTF-8 charset data protection | `å ä ö`, `é`, emoji, CJK used as **test data / byte-boundary payloads** are sacrosanct; the whitelist is `docs/research_swedish_inventory.md` §5. Sites were located by **content anchor, never line number** (offsets drift as comments are deleted). | 27 surviving diacritic lines match the whitelist 1:1 (t-015 §5); md5 proofs of byte-identity in t-006 audit §6; t-026 audit §4 (13 pinned anchors present exactly once, identical bytes). |
| Two Swedish detection passes | (i) diacritics `grep -rnP '[åäöÅÄÖ]'`; (ii) a curated **diacritic-free** Swedish word pass (`gick|testet|codern|passerar|tagit|bort|…`, `-w -i`) to catch Swedish written without diacritics. | Pass (i) flagged 82 lines in 10 files at inventory time; pass (ii) → 0 hits at the final gate. |
| Needle guards | `tests/test_tool_name_literals.rs` scans every `src/**/*.rs` with a **raw byte search and no comment stripper**; `include_str!` source guards pin comment text. | `cargo test --test test_tool_name_literals` exit 0 at every gate; guard inventory in t-014 audit; BASELINE single writer = t-014. |
| Traceability preserved | `REQ-xxx`, `t-NNN`, `H1–H6`, `M8`, dedup-cluster tags are never stripped. | t-027 §8 tag-family multiset diff = **0 delta** for every family. |

---

## 2. Comment categories actually used, and what was deliberately retained

| # | Category removed | Definition (as applied) | Deliberately RETAINED instead |
|---|---|---|---|
| 1 | Decorative separator banners | Comment lines whose body is only `----`/`────`/`====` rules (62 in the core batch, 94 in the UI/tests batch). Titled middle lines carry the information and stay. | Titled gate headings, e.g. `src/orchestrator/tests.rs:428-432` M8 block, `src/agents/prompt_builder.rs` “Gate t-055 — the two prompt WRITE sites”, `src/tool_args.rs:77`/`:621` (dedup gate t-044) kept bars-stripped. |
| 2 | Code restatements / assert echoes | Comments that only rename the next statement (`// Subagents lifecycle.`, `// Before rebirth: large context`, `/// Continue streaming chunks.`). | Assert messages that carry the invariant (`"isolated context has exactly 2 messages"`), and step markers that order a long test (`src/harness/mod.rs:1207/1239`, judged “borderline — keep”). |
| 3 | Commented-out dead code | Comment bodies ending in `;`/`{`/`(`, keyword-leading bodies. | **0 occurrences found in either audit batch** (`research_comments_core.md` §1.1, `research_comments_ui_tests.md` §1f). Never deleted: the `#[cfg(test)]` `legacy_*` witness functions (`phase_tests.rs:971-1005`, `markers.rs:959`, `:971`) — the non-vacuous half of the regression guards. |
| 4 | Duplicate narrative | Same explanation written 2–6× in a file; keep one canonical copy + pointer. | The reasoning itself: `src/harness/mod.rs` async copies kept / sync duplicates cut; the 6× Hermeticity block collapsed to one copy with the `t-033b` rationale intact; lock-pattern docs (`Intentionally a swap…`, `Contention-free pattern…`) deduped to one canonical copy in `bus.rs`. |
| 5 | Over-explained obvious / signature-echo rustdoc | Rustdoc echoing the item name or type (`/// Whether the checkbox is checked.` on `pub is_error: bool`). | Public API doc comments (4 restored in t-007 §G), field invariants, and the ratchet justifications required by `MIN_JUSTIFICATION_LEN` (`src/markers.rs:1552-1572`). |
| 6 | Stale file:line pointers | Pointers to non-existent files/lines or wrong numbers. Rule applied: **keep the claim, fix or re-point the pointer** — never delete the invariant sentence. | The claim, re-anchored symbolically: `net/retry.rs:13` `::post` → `crate::mcp::http::HttpSseConnection::send_request`; `mcp/protocol.rs:318-321` → symbol-level pointer; `notice.rs:1035` → `crate::harness::handle_reply_to_arbitrator_async`. |
| 7 | (Kept everywhere) | — | Doc comments; “why” comments; **UTF-8 char-boundary rules** (`text_util.rs:1-9/20`, `ui/helpers.rs` presentation budget + panic hazard, `markers.rs:566`); **Landlock / sandbox fail-closed rationale** (`config.rs:139-151`, `main.rs:71-97`, `sandbox.rs` pty-free test design); `REQ-xxx` / `t-NNN` traceability (150 REQ mentions + 412 `t-xxx` gate refs in the UI batch). |

---

## 3. Per-module comment delta (t-015 §6 verbatim) + per-task attribution

Counting method (identical at both ends): `grep -cE '^[[:space:]]*(//|///|//!)'` — whole-line comments,
`///`/`//!` included; HEAD text via `git show HEAD:<file>`. The HEAD column reproduces
`docs/baseline_test_run.md` §8 exactly (`src/harness` = core+monitor = 2187, `src/ui` = root+bridge+tui = 1530),
which validates the method.

| Module tree | Files | Comments at HEAD | Comments in work tree | Δ | Attribution (per-task figures; "not recorded" = never captured) |
|---|---:|---:|---:|---:|---|
| `src/agents` | 15 | 1 175 | 1 159 | **−16** | t-005: **17 comment lines removed** (task record: 2 + 7 + 3 + 3 + 2 across `catalog/prompt_builder/validation/mod/execution/fix_loop`, plus 6 stale `duplicates.md §6a/§6b` pointers rewritten in place). The 1-line removal-vs-net reconciliation is **not separately recorded**. |
| `src/harness` (core) | 14 | 1 712 | 1 724 | **+12** | t-006: **−61 removed / +73 added** (per-file HEAD→WT: common 17→15, fs 155→154, fs_tests 29→26, mod 220→208, plan 6→6, pty 411→403, pty_tests 126→140, sandbox 222→212, search 142→142, search_tests 46→83, sleep 111→111, tool_resolution_tests 101→104, workspace 60→60, dispatch_integrity_tests 66→60 — sums to 1 712→1 724). The `+14/+37/+3` additions belong to t-018/t-020/t-024 portability docs, not to t-006. |
| `src/harness/monitor` | 6 | 475 | 452 | **−23** | t-025: **not separately recorded** (module net only). |
| `src/llm` | 7 | 483 | 474 | **−9** | t-007 §G (authoritative): stream_tests −5, thinking −5, thinking_tests −2, client_tests 0 (t-024 rationale +3). |
| `src/mcp` | 5 | 617 | 633 | **+16** | t-007 §G: http_tests −8, mcp/mod −1, protocol −2/+3, retry-equivalents; `+29` rationale lines in `mcp/client.rs` are t-019 PATH/`cfg` documentation. |
| `src/net` | 4 | 137 | 137 | **0** | t-007 §G: `net/retry.rs` −1/+1 (stale pointer re-pointed). |
| `src/manager` | 6 | 1 649 | 1 580 | **−69** | t-009: **not separately recorded** (module net only). |
| `src/orchestrator` | 14 | 1 851 | 1 790 | **−61** | t-010: **77 insertions / 162 deletions** across 12 files; removed-line classes = 22 section-rule lines + 24 Swedish fixture strings + 42 doc comments + 74 inline; added = 33 fixture strings + 44 comments + 0 rule lines. t-013 also edited `notice.rs` (prompt hoist + 1 test); its comment delta is **not separately recorded**. |
| `src/types` | 3 | 17 | 13 | **−4** | t-008 (`wire.rs` 2 + duplicate, `tools.rs` 2). |
| `src/ui` (root) | 6 | 704 | 701 | **−3** | t-026 (root files) + t-011 (`ui/mod.rs` −7) + **t-013 additions** in `transcript.rs` (3 tests + invariant doc), which is why the net is only −3. |
| `src/ui/bridge` | 7 | 482 | 465 | **−17** | t-011: **−24 comment lines** in its 8-file scope (`bridge/tests.rs` 158→141, `src/ui/mod.rs` 42→35, all other files 0; 480 retained). Bridge tree alone = −17. |
| `src/ui/tui` | 5 | 344 | 299 | **−45** | t-026: per-file deltas events 38→36, formatting 68→64, mod 82→81, render 57→46, tests 115→88. |
| root `src/*.rs` | 11 | 1 664 | 1 622 | **−42** | t-008 (bars 12 = config 2 + plan_parse 4 + task_id 2 + markers 4; over-explained 12; restatements 6; stale 3) + t-028 **−17** (`tool_args.rs` −10 comments, `tool_names.rs` −7). t-008's own total is **not separately recorded**; `src/lib.rs` (16) and `src/prompts.rs` (3) were correctly left unchanged. |
| `tests/` | 17 | 1 424 | 1 350 | **−74** | t-012: **76 comment lines removed** (12 of 16 files changed, +51/−121). The residual +2 is the t-023/t-014 BASELINE comment lines in `test_tool_name_literals.rs`. |
| **TOTAL** | **104** | **12 734** | **12 399** | **−335** | Positive deltas are intentional: `src/mcp` +16 = `client.rs` PATH/spawn rationale; harness core +12 = `search_tests.rs`/`pty_tests.rs` cfg-pair rationale. Removing them would fail the Windows cross-compile gate. |

---

## 4. Every Swedish → English decision

Glossary ids G1–G5 from `docs/research_swedish_inventory.md` §2. Swedish originals are quoted verbatim
here (documentation only — no Swedish literal was ever written into `src/`).

| ID | Original (verbatim) | Replacement (verbatim) | File + content anchor | Paired assertion / fixture updated with it |
|---|---|---|---|---|
| G1 | `Ställer en följdfråga till Coder för att reda ut detaljerna...` | `Asking Coder a follow-up to clarify the details...` | **production prompt**: `src/orchestrator/notice.rs` `worker_reply_to_arbitrator` system prompt (HEAD :1144) → now `pub(crate) const WORKER_REPLY_SYSTEM_PROMPT` (t-013) | `worker_reply_user_status_example()` test seam + `notice_tests.rs:811` equality pin; consumer `src/ui/transcript.rs:146` predicate (t-013) |
| G1 | `Ställer en följdfråga om bakåtkompatibilitet...` | `Asking a follow-up about backward compatibility...` | `src/orchestrator/notice_tests.rs` fixture (HEAD :503) | prefix assertion at HEAD :556 (post-t-013 anchors :493/:545) — prefix relation intact |
| G1 | `Ställer en följdfråga till Coder gällande transaktionsisolering...` | `Asking Coder a follow-up about transaction isolation...` | `src/orchestrator/notice_tests.rs` (HEAD :766) | exact pair assertion HEAD :809 → :796 |
| G4 | `Coder bekräftar att async I/O har implementerats.` | `Coder confirms that async I/O has been implemented.` | `src/orchestrator/notice_tests.rs:392` | matching transcript assertion in the same test |
| G4 | `Coder har fixat PPC JIT.` | `Coder has fixed the PPC JIT.` | `notice_tests.rs:592` | assertions :681 |
| G4 | `Debugger har isolerat minnesläckan.` | `Debugger has isolated the memory leak.` | `notice_tests.rs:598` | assertions :659, :686 |
| G4 | `Vad gör codern?` / `Vad gör debuggern?` | `What is the coder doing?` / `What is the debugger doing?` | `notice_tests.rs:627` | assertions :637, :655, :680 |
| G4 | `Minnesläckan berodde på en saknad free i malloc-wrapper.` | `The memory leak was caused by a missing free in the malloc wrapper.` | `notice_tests.rs:631, 669` | **assertion substring `malloc-wrapper` → `malloc wrapper`** (:638, :658, :685) |
| G4 | `Implementerat JIT-instruktionerna för PowerPC 604e.` | `Implemented the JIT instructions for PowerPC 604e.` | `notice_tests.rs:645` | same test's answer assertion |
| G4 | `Coder förklarar att SQLite valdes för att minimera externa beroenden.` | `Coder explains that SQLite was chosen to minimize external dependencies.` | `notice_tests.rs:714` | assertions :752 |
| G4 | `Varför valde du SQLite?` | `Why did you choose SQLite?` | `notice_tests.rs:732` | question-side assertion |
| G4 | `SQLite har noll konfiguration och fungerar utmärkt med WAL-mode.` | `SQLite has zero configuration and works very well with WAL mode.` | `notice_tests.rs:743` | answer-side assertion |
| G4 | `Kan vi byta till SQLite?` / `Jag kan byta databas.` | `Can we switch to SQLite?` / `I can switch the database.` | `notice_tests.rs:782, 793` | question/answer pair assertions |
| G2 | `Hur går det för codern?` | `How is the coder doing?` | `src/ui/transcript.rs:431` (t-026) | assertion `transcript.rs:463` |
| G2 | `Hur går det för codern?` | `How is the coder doing?` | `tests/test_ui_session.rs:750` (SSE payload), `:780` (fixture) + comments `:748`, `:774` (t-012) | assertions `:802`, `:826` |
| G3 | `Codern har tagit bort jit_invalidate_all och alla 7 tester passerar.` | `The coder removed jit_invalidate_all and all 7 tests pass.` | `src/ui/transcript.rs:449` (t-026) | assertion `transcript.rs:466` |
| G3 | `Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar.` | `The coder removed the jit_invalidate_all() calls and ctest passes 7/7.` | `tests/test_ui_session.rs:740` (SSE, escaped), `:828`, `:865` (t-012) | assertions `:828`, `:865`; non-containment assert `:830` (`contains("[Specialist coder]:")`) preserved |
| G4 | `Vad är nästa steg?` | `What is the next step?` | `src/ui/transcript.rs:455` (t-026) | assertion `transcript.rs:468` |
| G4 | `Kör real-ROM-test med timeout.` | `Run the real-ROM test with a timeout.` | `src/ui/transcript.rs:458` (t-026) | assertion `transcript.rs:469` |
| G4 | `Schemalägg inga tasks parallellt.` | `Do not schedule any tasks in parallel.` | `src/ui/helpers.rs:705` (tool-call display fixture, t-026) | assertion `src/ui/helpers.rs:713` |
| G4 | `Normalisera plan-läsningen` (prose half of a mixed PLAN_MD line) | `Normalize plan reading` | `src/ui/helpers.rs:793` — the `` `åäö` ✅ `` token on the **same line** stayed byte-identical | assertion `src/ui/helpers.rs:940` `prompt.contains("`åäö` ✅")` unchanged |
| G4 | `Hur gick testet?` | `How did the test go?` | `tests/test_ui_session.rs:840` (fixture), `:861` (request-body lookup) + comment `:834` (t-012) | **mock branch matcher** `:742` `} else if body.contains("Hur gick testet?")` — translated in the same pass, or the fixture would no longer hit its branch |
| G4 | `Testet gick bra.` | `The test went fine.` | `tests/test_ui_session.rs:745` (SSE payload, t-012) | history assertion in the same test |
| G4 | `Status: tagit bort de två jit_invalidate_all()-anropen och ctest 7/7 passerar.` | `Status: removed the two jit_invalidate_all() calls and ctest passes 7/7.` | `tests/test_ui_session.rs:817` (t-012) | same test's steering-history assertion |
| G5 | — | — | The declared `src/ui/helpers.rs` ↔ `tests/test_ui_session.rs` pair **does not exist** (empty intersection, verified twice). Real helpers pairs are internal (`705↔713`, `793↔944`). | No cross-file needle existed → nothing to reconcile. |
| harness | `CODE_REVIEW #4 (Beroende av `.marmel` katalogens existens)` | `CODE_REVIEW #4 (Dependence on the existence of the `.marmel` directory)` | `src/harness/workspace.rs:3` module doc (t-006, the only Swedish site in harness core) | none (comment prose only) |

**Trees/files with no Swedish prose at all** (diacritic pass + curated diacritic-free word pass both empty):
`src/llm/**`, `src/net/**`, `src/mcp/**` (t-007 audit §RULE 3), `src/manager/**` (t-009), `src/ui/bridge/**`
(t-011 §2), and the four root files `src/tool_args.rs`, `src/tool_names.rs`, `src/lib.rs`, `src/prompts.rs`
(t-028 audit item 6). `src/agents/**` likewise had **0** Swedish sites (t-005 record).

---

## 5. Preserved UTF-8 fixture list (final §5 whitelist as measured by t-015)

`grep -rnP '[åäöÅÄÖ]' src/ tests/` → **27 lines**, matching `docs/research_swedish_inventory.md` §5
line-for-line, **zero survivors outside the whitelist**. (`src/ui/transcript.rs` and
`src/orchestrator/notice.rs`: 0 survivors.)

| File | Line(s) | Classification |
|---|---|---|
| `src/harness/pty_tests.rs` | 778 | byte-value comment (2-byte lead `å/ä/ö`, `is_utf8_lead_byte(0xC3)`) |
| `src/harness/pty_tests.rs` | 784 | byte-value comment (continuation byte `0xA4` — a *wrong* answer) |
| `src/harness/pty.rs` | 972 | doc stating the invariant (never a split `å`/`é`/`🙂`) |
| `src/text_util.rs` | 10, 82, 83, 84, 85, 86, 103, 121 | module doc (10); numeric byte-count claim (82); charset payloads + boundary assertions (83–86, 103); mixed-script loop incl. `🙂`, `漢字` (121) |
| `src/tool_args.rs` | 26 | doc stating the UTF-8 invariant |
| `src/tool_args.rs` | 546 | charset payload inside char-arithmetic test |
| `src/tool_args.rs` | 586, 587 | byte-value comment + mixed-token payload (`"åäö"` → `"create_plan(plan_markdown: 6 chars)"`) |
| `src/ui/helpers.rs` | 450 | doc stating the non-boundary panic hazard |
| `src/ui/helpers.rs` | 641, 642 | byte/boundary arithmetic payload (`"x" + 60×'ä'` = 121 bytes, 120 not a boundary) |
| `src/ui/helpers.rs` | 654, 671, 672 | truncation-budget payloads (`"ä".repeat(150)` → 123 chars; `"ä"×20 + "..."`) |
| `src/ui/helpers.rs` | 682, 683 | **pinned mixed token** `"självråkdiskriminering 👍 "` (load-bearing `find_tool_call_boundary` invariant) + charset payload `"wäsen".repeat(30)` |
| `src/ui/helpers.rs` | 789, 940 | **pinned mixed token** `` `åäö` ✅ `` inside PLAN_MD fixture, pinned by the assertion at :940 |
| `tests/test_harness.rs` | 132 | charset payload `🦀 åäö αβγ …` × 30 000 (comment re-derived to 540 000 bytes, not the inventory's 510 000) |
| `tests/test_harness.rs` | 209 | charset payload `"héllo wörld 🦀\n"` × 100 (byte-identity + `len ≤ 301`) |

Word pass (ii): the curated diacritic-free Swedish list (`-w -i`) went from **82 flagged lines** at inventory
time to **0 hits** at the final gate. Line numbers in research §5 are HEAD-era and drifted
(e.g. `harness/pty.rs` 980→972, `test_harness.rs` 133/210→132/209, `helpers.rs` 793/944 unchanged); identity was
proved by content anchor + md5 (t-006 audit §6).

---

## 6. Cross-platform portability fixes (Phase 2: t-018…t-023 + t-024)

| Task | File(s) | What changed |
|---|---|---|
| t-018 | `src/harness/pty_tests.rs` | PATH-resolved spawn fixtures (`"/bin/true"`,`"/bin/sleep"` → `"true"`,`"sleep"`); `cfg!(windows)` spawn specs (`short_lived_child_spec:335`, `long_running_child_spec:347`, `spawn_real_child:358`); **paired** `#[cfg(unix)]` / `#[cfg(not(unix))]` `assert_gone_from_process_table` (unix :374 / not(unix) :387) with the unix-only zombie assertions at :403/:423; `#[cfg(not(unix))] let code = 0;` inside `waitpid_gone_error` (:265); `/usr/local/bin/marmel` → `/usr/local/libexec/marmel`. |
| t-019 | `src/mcp/client.rs` | Absolute `/bin/*` program paths → bare PATH names: `"cat"`:569, `"sleep"`:605, `"sh"`:693, `"tee"`:672, `"cat"`:826, plus a `cfg!(windows)` `cmd /C ping` branch (:597) and +29 rationale comment lines. |
| t-020 | `src/harness/search_tests.rs` | `WINDOWS_PATH_PREFIXES` + `normalize_for_compare` (:46, `if cfg!(unix) { return raw.to_owned() }`) and `normalize_paths` (:67); guard test `normalize_for_compare_is_identity_on_unix_and_canonical_on_windows` (:732); +37 comment lines. |
| t-021 | `src/mcp/http_tests.rs`, `src/mcp/protocol.rs` | `"tools/call"` → `METHOD_TOOLS_CALL` (declared `protocol.rs:74`, used `:160`); 6 references at `http_tests.rs:20,57,384,460,500,551`. The **6 remaining raw `"tools/call"`** occurrences are JSON wire-text fixtures (`protocol.rs:74/561/777/782`, `http_tests.rs:61/391`) and stay. |
| t-022 | `docs/verification_final.md` | Phase-2 gate document (later overwritten by t-015 with rows 1–10 preserved verbatim). |
| t-023 | `tests/test_tool_name_literals.rs` | BASELINE rows for the OS program name, verbatim: `// OS program name in a spawn fixture (PATH lookup), not a tool identifier.` + `("src/harness/pty_tests.rs", "sleep", 1),` + same comment + `("src/mcp/client.rs", "sleep", 1),`. No tool-name literal was added to `src/`; BASELINE single writer = t-014. |
| t-024 | `src/harness/tool_resolution_tests.rs:92`, `src/llm/client_tests.rs:667` | Last two `"/bin/sh"` → PATH-resolved `"sh"` (+3 rationale comment lines in each file). |

Final portability gate (t-015 §7):

| Check | Result |
|---|---|
| `grep -rn '"/bin/' src/ tests/` | **0 hits** |
| `cfg!(windows)` spawn specs | 5 sites: `prompts.rs:30`, `search_tests.rs:746`, `pty_tests.rs:336`, `pty_tests.rs:348`, `mcp/client.rs:597` |
| Paired `#[cfg(unix)]` / `#[cfg(not(unix))]` | 31 / 8; pairs at `fs.rs:518+553`, `pty_tests.rs:265+386`, `pty.rs:823+1578`, `main.rs:175`, `config.rs:547` |
| Unix no-op path/permission normalizers | `fs.rs:518` `preserve_target_permissions` + `fs.rs:553` `set_dir_permissions_755` (`#[cfg(not(unix))] {}` no-op stubs, no-op test at :1152), `pty_tests.rs:386` not(unix) `assert_gone_from_process_table` → `Ok(())` |
| `cargo check --target x86_64-pc-windows-gnu --all-targets` | **exit 0**, 18 warning lines = **17** real warnings (all Windows-only dead-code: `tool_resolution_tests` 8, `sandbox` 4, `llm/client_tests` 3, `mcp/client` 2) + 1 summary ⇒ **tracked as non-blocking**, none comment/translation/UTF-8 related |

---

## 7. The t-013 merge-marker decision

`src/ui/transcript.rs:146` previously read (4 hardcoded literals, one of them Swedish):

```rust
if last_resp.contains("awaiting specialist reply") || last_resp.contains("Decision:")
    || last_resp.contains("följdfråga") || last_resp.contains("follow-up") {
```

**Why option 2 (delegate to `orchestrator::steer::steering_entry_is_pending` backed by
`PENDING_ENTRY_MARKERS`) was chosen:**

| Option | Outcome |
|---|---|
| 1 — drop the `följdfråga` clause (or the whole arm) | Cheapest and zero-Swedish, but leaves the **same string vocabulary duplicated** in two modules and keeps a wording-keyed heuristic with no pin. |
| 3 — typed `pending: bool` flag on the record | Most semantically correct and immune to rewording, but requires serde change + default for pre-existing journals, and the flag **cannot be recovered for journals already written** with the Swedish string. Larger behaviour-change surface than this mission sanctions. |
| **2 — delegate (chosen)** | The marker vocabulary already has an owner: `PENDING_ENTRY_MARKERS` (`src/orchestrator/steer.rs:460-461`) + `pub(crate) fn steering_entry_is_pending` (`:465` HEAD, `:464` work tree), already consumed live by `orchestrator::bus::record_steering_exchange` (`bus.rs:182, 196`). One vocabulary then governs live history **and** journal replay; no new marker vocabulary was invented. Documented as an invariant in the doc comment at `transcript.rs:106-113`. |

**Documented behaviour delta (accepted, pinned):** the merge now **also accepts `"ForwardToWorker"`**
(a marker the orchestrator already treats as pending) and **no longer matches Swedish prose**.

**Prompt hoist:** the previously inline system prompt became `pub(crate) const WORKER_REPLY_SYSTEM_PROMPT`
(`src/orchestrator/notice.rs:1121`, consumed at `:1183`), shipped bytes otherwise unchanged, plus a test seam
`worker_reply_user_status_example()` (`notice.rs:1149-1158`, `#[cfg(test)]`) so the **producer** can be pinned
against the **consumer**.

**The 3 new tests:**

| Test | Location | What it pins |
|---|---|---|
| `test_extract_steering_history_merges_the_pending_status_the_prompt_invites` | `src/ui/transcript.rs:476` | PIN: reads the `user_status` example **out of the shipped prompt**, asserts `steering_entry_is_pending(...)` is true, `history.len() == 1`, and that the resolved answer **replaces** the pending placeholder. Re-wording the prompt out of the vocabulary fails here. |
| `test_extract_steering_history_does_not_merge_a_swedish_only_pending_status` | `src/ui/transcript.rs:512` | REGRESSION: the Swedish-only placeholder is **not** pending (`!steering_entry_is_pending(...)`) and the two answers are concatenated (`"{pending}\n{resolved}"`) — the accepted, cosmetic cost of dropping the arm, pinned so the heuristic cannot silently widen. |
| `test_worker_reply_prompt_user_status_example_is_pending_and_language_neutral` | `src/orchestrator/notice_tests.rs:811` | Producer side: G1 bytes exactly (`"Asking Coder a follow-up to clarify the details..."`), `steering_entry_is_pending(...)` true, and `WORKER_REPLY_SYSTEM_PROMPT.is_ascii()`. |

**The `\u{e4}/\u{f6}/\u{e5}` escape trick:** the negative test must assert that a *Swedish* status line is
no longer treated as pending, but writing raw `å/ä/ö` into `src/` would re-open the diacritic gate and create a
false UTF8-DATA whitelist entry. So the fixture at `transcript.rs:514` is spelled with Unicode escapes:
`"St\u{e4}ller en f\u{f6}ljdfra\u{e5}ga till Coder f\u{f6}r att reda ut detaljerna..."` — it compiles to the
identical byte string while `grep -P '[åäöÅÄÖ]'` stays clean (doc-comment rationale at `:505-510`; recorded as
an accepted residual in §9).

Net effect on the test floor: **807/5 → 810/5** (lib total 811 → 815).

---

## 8. Verification summary (t-015 FINAL GATE, `docs/verification_final.md`)

| Gate | Command | Exit | Numbers |
|---|---|---:|---|
| Build | `cargo build` | **0** | 0 warnings (10.35 s) |
| Format | `cargo fmt --all -- --check` | **0** | 0 diff lines |
| Lint | `cargo clippy --all-targets --all-features -- -D warnings` | **0** | 0 diagnostics (warm cache, 0.30 s — exit code authoritative under `-D warnings`) |
| Full suite | `cargo test --no-fail-fast` | 101 | 949 passed / 11 failed (baseline was 944/12) |
| Serial floor | `cargo test --lib -- --test-threads=1` | 101 | **810 passed / 5 failed** = exactly the sanctioned set |
| Integration | each of 16 `tests/` targets, serially | 101 overall | **136 passed / 1 failed** |

The 5 sanctioned serial failures (named):

1. `harness::pty::tests::test_harness_pty_process_group_kill`
2. `harness::pty::tests::test_interactive_pty_manager_lifecycle`
3. `harness::pty::tests::test_run_command_parses_timeout_seconds_argument`
4. `harness::pty::tests::test_run_command_pty_cancelled_by_worker_token`
5. `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable`

Cause of rows 1–4: **`/dev/pts/ptmx` mode `000` → `openpty` EACCES** (environment; identical at baseline).
Row 5 is the deterministic baseline routing failure. The one integration failure,
`test_role_gating::test_prompt_based_tool_gating` (`tests/test_role_gating.rs:312`), is **documented as
pre-existing** at baseline (`docs/baseline_test_run.md` §6) and is **not** a mission regression — reproduced
`24 passed / 1 failed`, with the file's token stream identical to HEAD (comment-only diff).

**Parallel-mode flake rows 1–10**: `docs/verification_final.md` §9 “Portability gate history (rows 1–10)”
was retained verbatim by t-015 (verified at line 171 of that file) and records the order-dependent parallel
failures (`notice_tests::test_handle_reply_to_arbitrator_ask_followup` + 4 sandbox/config `ACTIVE_CONFIG`
tests) as flake-only: they pass in isolation and never appear in the serial floor.

---

## 9. Accepted residuals & non-blocking items

| Item | Classification | Disposition |
|---|---|---|
| `src/ui/transcript.rs:514` Swedish negative-test fixture written as `\u{e4}/\u{f6}/\u{e5}` escapes | Invisible to the mandated diacritic grep; deliberate, documented in its doc comment (`:505-510`); the 1 raw-escape line in `src/` | **Accepted residual** — it is the payload the brief demanded; the audits flagged the report wording, not the code. |
| 17 Windows-only dead-code warnings (`cargo check --target x86_64-pc-windows-gnu --all-targets`) | Pre-existing, confined to 4 files (`tool_resolution_tests` 8, `sandbox` 4, `llm/client_tests` 3, `mcp/client` 2) | **Tracked, non-blocking** (t-015 §7). |
| `test_role_gating::test_prompt_based_tool_gating` failure | Pre-existing at baseline (`docs/baseline_test_run.md` §6) | Sanctioned; causally excluded (comment-only diff in that file). |
| INCONCLUSIVE audit sub-item (t-012 audit, check 8) | `docs/verification_final.md` contained **0 mentions of the test name**, so it offered no corroboration; resolved by citing `docs/baseline_test_run.md` rows 151/180/198/222 instead | Recorded as INCONCLUSIVE-but-immaterial. |
| Loss of external contract pointers in `src/orchestrator/steer.rs` (t-027 note 1) | `REFERENCE_ORCHESTRATION_CONTRACT.md §5` −1 and `caesar §5.1/§5.3` −7: those artifacts do not exist in this repo | Defensible dead-pointer cleanup; suggested follow-up = re-anchor the 6 fallback docs to an in-repo contract doc. `tests/test_orchestrator.rs:6` still cites `wip/REFERENCE_ORCHESTRATION_CONTRACT.md` — cosmetic backlog. |
| `tui/mod.rs` doc fragment `/// Defaults to \`0\`.` and `helpers.rs` doc opening mid-explanation (t-026 audit nits 2–3) | Style only | Non-blocking; restoring a lead sentence is optional. |
| Report arithmetic discrepancies | t-026 per-file Δ sums to −63 vs stated −57 (per-file numbers confirmed correct); t-011 24 vs 25 removals (net vs pure-removal convention); t-028 baseline 202 vs 203 (counting convention); t-028 −15 lines vs −10 comments (5 adjacent blank lines dropped with the banners) | Documentation-only; no functional impact. |
| Per-task comment deltas never captured for t-009 (`src/manager`) and t-025 (`src/harness/monitor`); t-008 split inside the root tree | Measurement gap, not a code gap | Recorded as **not separately recorded** in §3; module nets stand. |
| `docs/cleanup_report.md` (earlier mission) | Historical | Left untouched; **superseded by this file** (stated at the head of this report). |

---

## 10. Traceability appendix — task → file set → audit artifact

| Task | File set touched | Deliverable / audit artifact |
|---|---|---|
| t-001 | (read-only) all `src/`, `tests/` | `docs/research_swedish_inventory.md` (inventory, glossary G1–G5, §3.3 merge-marker analysis, §5 UTF8-DATA whitelist) |
| t-002 | (read-only) `src/agents`, `src/harness`, `src/llm`, `src/net`, `src/mcp` | `docs/research_comments_core.md` |
| t-003 | (read-only) `src/ui`, `src/manager`, `src/orchestrator`, root `src/*.rs`, `tests/` | `docs/research_comments_ui_tests.md` |
| t-004 | (read-only) whole crate | `docs/baseline_test_run.md` (pre-change test floor + §8 comment snapshot = 12 734) |
| t-005 | `src/agents/**` | per-task report in the plan record (no separate doc) |
| t-006 | `src/harness/**` core (14 files) | `docs/verification_harness.md` (t-006 audit, VERDICT APPROVED) |
| t-007 | `src/llm/**`, `src/net/**`, `src/mcp/**` (16 files) | `docs/t007_comment_cleanup_report.md` (§G authoritative), `docs/t007_diff_evidence.md`, `docs/t007_oneshot_verify.sh` |
| t-008 | 11 root `src/*.rs` + `src/types/**` | per-task report in the plan record (no separate doc) |
| t-009 | `src/manager/**` | per-task report in the plan record (no separate doc) |
| t-010 | `src/orchestrator/**` (14 files) | `docs/verification_orchestrator.md` (t-027 audit, VERDICT APPROVED) |
| t-011 | `src/ui/bridge/**` + `src/ui/mod.rs` | `docs/t011_comment_cleanup_report.md` |
| t-012 | `tests/**` (16 files, `test_tool_name_literals.rs` excluded) | `docs/verification_tests.md` (t-012 audit, VERDICT APPROVED) |
| t-013 | `src/ui/transcript.rs`, `src/orchestrator/notice.rs`, `src/orchestrator/notice_tests.rs` | audit record in the mission log (no separate doc); 3 tests named in §7 |
| t-014 | `tests/test_tool_name_literals.rs` (BASELINE reconciliation, zero edits needed) | audit record in the mission log (no separate doc) |
| t-015 | (read-only) whole workspace | `docs/verification_final.md` (FINAL GATE; §9 retains portability rows 1–10 verbatim) |
| t-016 | conditional follow-up | **SKIPPED** — justified by the CLEAN t-015 gate verdict |
| t-017 | this document only | `docs/comment_cleanup_report.md` (this file) |
| t-018 | `src/harness/pty_tests.rs` | Phase-2 gate rows (preserved in `docs/verification_final.md` §9) |
| t-019 | `src/mcp/client.rs` | Phase-2 gate rows (ibid.) + t-007 audit RULE 5 corroboration |
| t-020 | `src/harness/search_tests.rs` | Phase-2 gate rows (ibid.) + t-006 audit check 4 |
| t-021 | `src/mcp/http_tests.rs`, `src/mcp/protocol.rs` | Phase-2 gate rows (ibid.) + t-007 audit RULE 5 |
| t-022 | gate document | `docs/verification_final.md` (Phase-2 version, later overwritten by t-015 with history preserved) |
| t-023 | `tests/test_tool_name_literals.rs` (BASELINE `sleep` rows) | diff quoted verbatim in `docs/verification_tests.md` check 5 |
| t-024 | `src/harness/tool_resolution_tests.rs`, `src/llm/client_tests.rs` | `docs/verification_final.md` §7 (0 `"/bin/` hits) |
| t-025 | `src/harness/monitor/**` | per-task report in the plan record (no separate doc) |
| t-026 | `src/ui/{helpers,raw,session,session_tests,transcript}.rs`, `src/ui/tui/**` | audit record in the mission log (no separate doc) |
| t-027 | audit of t-010 | `docs/verification_orchestrator.md` |
| t-028 | `src/tool_args.rs`, `src/tool_names.rs` (+ `src/lib.rs`, `src/prompts.rs` verified clean) | audit record in the mission log (no separate doc) |

STATUS: MISSION COMPLETE — FINAL GATE CLEAN

## Validation (t-017 independent re-check — this section is the only edit made to this file)

| # | Check | Verdict + evidence |
|---|---|---|
| 1 | File present, non-empty, 10 sections | PASS — 296 lines / 34 314 bytes. Headings found: §1 Scope & ground rules (l.21), §2 Comment categories actually used, and what was deliberately retained (34), §3 Per-module comment delta + per-task attribution (48), §4 Every Swedish → English decision (75), §5 Preserved UTF-8 fixture list (116), §6 Cross-platform portability fixes (146), §7 The t-013 merge-marker decision (170), §8 Verification summary (215), §9 Accepted residuals & non-blocking items (247), §10 Traceability appendix (263) |
| 2 | Closing status line | PASS — final line (296) verbatim `STATUS: MISSION COMPLETE — FINAL GATE CLEAN` |
| 3 | Figures vs `docs/verification_final.md` (t-015) | PASS, no mismatch — 12 734 → 12 399 (−335) [final §6 l.134, §10 l.169]; serial floor 810 passed / 5 failed [l.46]; integration 136/1 [l.41, 88]; diacritic survivors 27 [l.92]; curated word pass 82 → 0 [l.99]; `grep -rn '"/bin/'` 0 hits [l.146]; `cargo check --target x86_64-pc-windows-gnu --all-targets` exit 0 [l.153] |
| 4 | §5 27-line survivor list | PASS — 8 `src/text_util.rs` + 4 `src/tool_args.rs` + 10 `src/ui/helpers.rs` + 1 `src/harness/pty.rs` + 2 `src/harness/pty_tests.rs` + 2 `tests/test_harness.rs` = 27, file-and-line identical to final §5 (l.92–96) and to the research §5 whitelist (HEAD anchors 980 / 744,750 / 558 / 600-601 / 452 / 643-644 / 133,210 — drift disclosed in the §5 note). All six spot-check files present; zero survivors outside the list |
| 5 | No invented numbers | PASS — all 15 §3 rows reproduce final §6 (l.120–134) exactly, incl. 104 / 12 734 / 12 399 / −335; attributions traced: t007 §G l.18–34 (llm −9, mcp +16, net 0), `verification_orchestrator.md` l.54 + 77–78 (77/162; 22/24/42/74), `verification_tests.md` l.80 (76 removed, 12 of 16 files, +51/−121), t011 l.18/20 (−24; 158→141; 42→35; 480 retained), plan record l.39/42/43 (t-006 −61, t-026 −57, t-028 −17). t-009, t-025, t-008 split and t-013 comment delta are written **not recorded**, never guessed |
| 6 | Working-tree integrity | PASS — `git status --porcelain` lists `?? docs/` (this file is new/untracked); `git diff --stat -- docs/cleanup_report.md` → empty, mtime 2026-10-08 15:22 predates the final gate (2026-10-10 12:02); `find src -type f -newer docs/verification_final.md` → 0 files, so no `src/**` was touched by this documentation step |

Non-blocking nits (line-number attribution only, no figure is wrong): §5 note claims “helpers.rs 793/944 unchanged” whereas its own table and final §5 give 789/940; the t-006 “+73 added” and its 14-file breakdown appear in neither `verification_harness.md` nor the plan-record line (they do sum exactly to the gate row 1712→1724); `research_swedish_inventory.md` §5 prose says “30 UTF8-DATA lines” while its own enumeration lists 27.

VALIDATION VERDICT: APPROVED
