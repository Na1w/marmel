# Swedish-text inventory + translation glossary (Marmel / `marmennill`)

Read-only audit. **No source file was modified.** Scope: every `.rs` file under `src/` and
`tests/` (inline `#[cfg(test)] mod tests` and `*_tests.rs` included).

## 0. Method / how to reproduce

```bash
cd /home/fredrik/marmel/wip
# (1) every line containing a Swedish diacritic
grep -rn -P '[åäöÅÄÖ]' --include='*.rs' src tests
# (2) Swedish words without diacritics (curated word list, word-boundary, case-insensitive)
grep -rn -w -i -E 'gick|testet|codern|debuggern|anropen|passerar|tagit|bort|inga|vad|har|fixat|\
implementerat|instruktionerna|databas|byta|bra|saknad|berodde|isolerat|implementerats|valdes|noll|\
konfiguration|parallellt|utmärkt|gällande' --include='*.rs' src tests
# (3) dedupe (1)+(2)
```

Result: **82 flagged lines in 10 files** (0 false positives after review; the second pass was
cross-checked against a wider function-word list — `inte`, `och`, `att`, `den`, `det`, `de`,
`vi`, `kan`, `av`, `på`, `med`, `till`, `ok`, `ja`, `nej`, `sig`, `han`, `har`, `bra`, `prov`,
`testa`, `tester`, `kod`, `sträng`, `tecken`, `gräns`, `fråga`, `svar`, `anrop`, `meddelande`,
`uppgift`, `leverans`, `säger`, `funkar`, `följdfråga`, `själv`, `wäsen`, `någon/något/några`,
`alltså`, `eftersom`, `trots`, `däremot`, `första`, `sista`, `någon` — and produced no additional
hits beyond those 82 lines).

Per-file distribution:

| File | Flagged lines |
|---|---|
| `src/orchestrator/notice_tests.rs` | 28 |
| `tests/test_ui_session.rs` | 15 |
| `src/ui/helpers.rs` | 12 |
| `src/ui/transcript.rs` | 9 |
| `src/text_util.rs` | 8 |
| `src/tool_args.rs` | 4 |
| `src/harness/pty_tests.rs` | 2 |
| `tests/test_harness.rs` | 2 |
| `src/orchestrator/notice.rs` | 1 |
| `src/harness/pty.rs` | 1 |

Swedish text exists in **exactly two production (non-test) places**: `src/ui/transcript.rs:146`
and `src/orchestrator/notice.rs:1144`. Everything else is test data or a comment.

Classification key: **(a) FIXTURE-PROSE** = translate; **(b) UTF8-DATA** = must be preserved;
**(c) PRODUCTION-LOGIC** = shipped behaviour, handle explicitly.

---

## 1. Complete inventory

### 1.1 `src/ui/transcript.rs` — PRODUCTION + fixture/assertion pairs

| file:line | literal / comment | class |
|---|---|---|
| `src/ui/transcript.rs:146` | `|| last_resp.contains("följdfråga")` | **(c) PRODUCTION-LOGIC** — pending-decision heuristic, see §3 |
| `src/ui/transcript.rs:431` | `text: "Hur går det för codern?".to_string(),` (fixture, `UiRecord::User`) | (a) FIXTURE-PROSE |
| `src/ui/transcript.rs:449` | `text: "\n[Arbitrator]: Codern har tagit bort jit_invalidate_all och alla 7 tester passerar.\n"` (fixture, `UiRecord::SteerResponse`) | (a) FIXTURE-PROSE |
| `src/ui/transcript.rs:455` | `text: "Vad är nästa steg?".to_string(),` | (a) FIXTURE-PROSE |
| `src/ui/transcript.rs:458` | `text: "Kör real-ROM-test med timeout.".to_string(),` | (a) FIXTURE-PROSE |
| `src/ui/transcript.rs:463` | `assert_eq!(history[0].0, "Hur går det för codern?");` | (a) assertion |
| `src/ui/transcript.rs:466` | `"Codern har tagit bort jit_invalidate_all och alla 7 tester passerar."` | (a) assertion |
| `src/ui/transcript.rs:468` | `assert_eq!(history[1].0, "Vad är nästa steg?");` | (a) assertion |
| `src/ui/transcript.rs:469` | `assert_eq!(history[1].1, "Kör real-ROM-test med timeout.");` | (a) assertion |

All eight fixture/assertion lines live in one test:
`test_extract_steering_history_pairs_and_resolves_pending` (`src/ui/transcript.rs:419`).
The only non-Swedish pending marker used there is the English one at line 435
(`"… — awaiting specialist reply."`).

### 1.2 `src/orchestrator/notice.rs` — PRODUCTION prompt

| file:line | literal | class |
|---|---|---|
| `src/orchestrator/notice.rs:1144` | `   - In 'user_status', provide a brief status notice in the user's language (e.g. 'Ställer en följdfråga till Coder för att reda ut detaljerna...').` | **(c) PRODUCTION-LOGIC** — Swedish example inside the shipped `evaluate_worker_reply` system prompt (string starts at `notice.rs:1132`, ends at `:1152`) |

Context: the same prompt says the answer must be in "the EXACT SAME LANGUAGE as the user's
inquiry" (`:1140`), so the Swedish example *biases the model toward Swedish output* in Swedish
sessions; that output is emitted verbatim to the UI at `src/harness/mod.rs:845-848`
(`"\n[Arbitrator]: {user_status}\n\n"`) and lands in the UI transcript as
`UiRecord::SteerResponse` (`src/ui/session.rs:399, 985, 1078, 1513, 1592`) — which is exactly the
text `transcript.rs:146` later greps for.

### 1.3 `src/orchestrator/notice_tests.rs` — 28 lines, all fixture/assertion prose (a)

Test scopes: `test_handle_reply_to_arbitrator_success_and_validation` (385),
`test_handle_reply_to_arbitrator_ask_followup` (494),
`test_steering_history_multiple_notices_resolved_out_of_order` (585),
`test_evaluate_worker_reply_synthesize_response` (709),
`test_evaluate_worker_reply_ask_followup` (759).

| file:line | role | literal |
|---|---|---|
| 392 | mock fixture (`json!` → SSE) | `"Coder bekräftar att async I/O har implementerats."` |
| 469 | assertion (`text.contains`) | same literal |
| 482 | assertion (`hist[0].1.contains`) | same literal |
| 503 | mock fixture (`user_status`) | `"Ställer följdfråga om bakåtkompatibilitet..."` |
| 556 | assertion (`text.contains`, **no trailing dots**) | `"Ställer följdfråga om bakåtkompatibilitet"` |
| 592 | mock fixture (`response`) | `"Coder har fixat PPC JIT."` |
| 598 | mock fixture (`response`) | `"Debugger har isolerat minnesläckan."` |
| 627 | history seed (inquiry) | `"Vad gör codern?"` |
| 631 | history seed (inquiry) | `"Vad gör debuggern?"` |
| 637 | `post_notice_to_worker("coder", ...)` | `"Vad gör codern?"` |
| 638 | `post_notice_to_worker("debugger", ...)` | `"Vad gör debuggern?"` |
| 645 | worker reply (`json!` tool args) | `"Minnesläckan berodde på en saknad free i malloc-wrapper."` |
| 655 | assertion | `"Vad gör codern?"` |
| 658 | assertion | `"Vad gör debuggern?"` |
| 659 | assertion (`contains`) | `"Debugger har isolerat minnesläckan."` |
| 669 | worker reply (`json!` tool args) | `"Implementerat JIT-instruktionerna för PowerPC 604e."` |
| 680 | assertion | `"Vad gör codern?"` |
| 681 | assertion (`contains`) | `"Coder har fixat PPC JIT."` |
| 685 | assertion | `"Vad gör debuggern?"` |
| 686 | assertion (`contains`) | `"Debugger har isolerat minnesläckan."` |
| 714 | mock fixture (`response`) | `"Coder förklarar att SQLite valdes för att minimera externa beroenden."` |
| 732 | `SteerNotice.user_inquiry` | `"Varför valde du SQLite?"` |
| 743 | `worker_reply` argument | `"SQLite har noll konfiguration och fungerar utmärkt med WAL-mode."` |
| 752 | assertion (`assert_eq!(eval.response…)`) | `"Coder förklarar att SQLite valdes för att minimera externa beroenden."` |
| 766 | mock fixture (`user_status`) | `"Ställer en följdfråga till Coder gällande transaktionsisolering..."` |
| 782 | `SteerNotice.user_inquiry` | `"Kan vi byta till SQLite?"` |
| 793 | `worker_reply` argument | `"Jag kan byta databas."` |
| 809 | assertion (`assert_eq!(eval.user_status…)`) | `"Ställer en följdfråga till Coder gällande transaktionsisolering..."` |

Note: `765`/`804` (`follow_up_prompt` "Will switching to SQLite break…") and `14/21/355/366`
("Why did you use SQLite?" / "Why SQLite?") are **already English** — not Swedish, listed here only
because they are the English siblings of the Swedish lines.

### 1.4 `src/ui/helpers.rs` — mixed (a) prose + (b) UTF-8 data

| file:line | content | class |
|---|---|---|
| 452 | comment: `// slice could panic mid-character on multi-byte text (å/ä/ö, emoji, …).` | (b) UTF8-DATA — doc reference (production comment, `truncate_reason`) |
| 643 | comment: `// "x" + 60x 'ä' = 121 bytes; byte index 120 falls inside an 'ä'.` | (b) UTF8-DATA — numeric claim tied to line 644 |
| 644 | `let s = format!("{}{}", "x", "ä".repeat(60));` (→ `assert_eq!(s.len(), 121)`, `assert!(!s.is_char_boundary(120))` at 645-646) | **(b) UTF8-DATA — DO NOT TRANSLATE/DELETE** |
| 656 | `let long = format!("{}{}", "x", "ä".repeat(150));` (→ `assert_eq!(out.chars().count(), 123)` at 658) | **(b) UTF8-DATA — DO NOT TRANSLATE/DELETE** |
| 673 | `truncate_reason(&"ä".repeat(21), 20),` | **(b) UTF8-DATA** |
| 674 | `format!("{}...", "ä".repeat(20))` | **(b) UTF8-DATA** |
| 684 | `"självråkdiskriminering 👍 ",` (payload inside `FAILED:` reason) | **(b) UTF8-DATA** (the *word* is Swedish but load-bearing: it is the multi-byte word that exercises the 2/3-word-break + char-boundary path) |
| 685 | `"wäsen".repeat(30)` | **(b) UTF8-DATA** |
| 705 | `base_prompt: Some("Schemalägg inga tasks parallellt.".to_string()),` | (a) FIXTURE-PROSE |
| 713 | `assert!(prompt.contains("Schemalägg inga tasks parallellt."));` | (a) assertion |
| 793 | `PLAN_MD` line: `- [x] [t-001] Normalisera plan-läsningen — \`åäö\` ✅` | **(a)+(b) mixed**: prose `Normalisera plan-läsningen` is (a); `` `åäö` `` and `—`/`✅` are (b) and are pinned by the assertion at 944 |
| 944 | `prompt.contains("\`åäö\` ✅") && prompt.contains("\tindented\ttab + trailing spaces")` | **(b) UTF8-DATA assertion** |

### 1.5 `tests/test_ui_session.rs` — 15 lines (one test: `test_steering_conversation_history_accumulates_worker_reply_to_arbitrator`, line 723)

| file:line | role | literal |
|---|---|---|
| 740 | **mock SSE payload (3-level escaped)** | `…\"response\": \"Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar.\"…` |
| 742 | **mock branch matcher** | `} else if body.contains("Hur gick testet?") {` |
| 745 | mock SSE payload (3-level escaped) | `…\"response\": \"Testet gick bra.\"…` |
| 748 | comment | `// Turn 1: user asks "Hur går det för codern?" -> ForwardToWorker` |
| 750 | mock SSE payload (3-level escaped) | `…\"message\": \"Hur går det för codern?\"…` |
| 774 | comment | `// Turn 1: user asks "Hur går det för codern?"` |
| 780 | fixture (`spawn_steer_arbitration` user msg) | `"Hur går det för codern?"` |
| 802 | assertion | `assert_eq!(hist[0].0, "Hur går det för codern?");` |
| 817 | worker reply (`json!` tool args) | `"Status: tagit bort de två jit_invalidate_all()-anropen och ctest 7/7 passerar."` |
| 826 | assertion | `assert_eq!(hist[0].0, "Hur går det för codern?");` |
| 828 | assertion (`contains`) | `"Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar."` |
| 834 | comment | `// Turn 2: user asks "Hur gick testet?"` |
| 840 | fixture (turn-2 user msg) | `"Hur gick testet?"` |
| 861 | **body lookup** | `.find(\|b\| b.contains("Hur gick testet?"))` |
| 865 | assertion | `…contains("Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar.")` |

### 1.6 `tests/test_harness.rs` — UTF-8 boundary payloads (b)

| file:line | literal | class |
|---|---|---|
| 133 | `let payload = "🦀 åäö αβγ".repeat(30_000); // 630 000 bytes > READ_FILE_MAX_BYTES` | **(b) UTF8-DATA — DO NOT TRANSLATE/DELETE** (drives `payload.len() > READ_FILE_MAX_BYTES` at 134, `payload.chars().take(2000)` at 164/184, and the `!res.content.contains('\u{FFFD}')` split-guard at 167) |
| 210 | `let payload = "héllo wörld 🦀\n".repeat(100); // 1 400 chars / 1 900 bytes` | **(b) UTF8-DATA — DO NOT TRANSLATE/DELETE** (`assert_eq!(res.content.as_bytes(), payload.as_bytes())` at 215-219) |

### 1.7 `src/text_util.rs` — UTF-8 truncation data (b)

| file:line | content | class |
|---|---|---|
| 10 | module doc: `//! never split a multi-byte char (Swedish \`å\`/\`ä\`/\`ö\`, emoji, CJK, …).` | (b) doc reference |
| 82 | comment: `// "å" is 2 bytes: byte-wise \`&s[..3]\` would panic, char-wise is safe.` | (b) doc reference (numeric claim) |
| 83 | `let s = "åäö";` | **(b)** |
| 84 | `assert_eq!(truncate_chars(s, 2), "åä");` | **(b)** |
| 85 | `assert_eq!(truncate_chars(s, 3), "åäö");` | **(b)** |
| 86 | `assert_eq!(truncate_with_ellipsis("åäöabcdef", 6), "åäö...");` | **(b)** |
| 103 | `assert_eq!(truncate_chars("åäö", 0), "");` | **(b)** |
| 121 | `let cut = truncate_chars("a🙂äö漢字", n);` | **(b)** |

Test names: `truncate_chars_sweedish_at_boundary_crossing_limit` (81 — note the misspelled
`sweedish`), `zero_char_limit` (101), `never_splits_multibyte_char` (118).

### 1.8 `src/tool_args.rs` — UTF-8 preview data (b)

| file:line | content | class |
|---|---|---|
| 26 | module doc: `//! can never split a multi-byte character (Swedish \`å\`/\`ä\`/\`ö\`, emoji, CJK, …)` | (b) doc reference |
| 558 | `let swedish = json!({ "k": "åäö".repeat(40) });` (→ char-count arithmetic assertion at 561-564) | **(b)** |
| 600 | comment: `// render): \`åäö\` is 3 characters but 6 bytes.` | (b) doc reference (numeric claim tied to 604) |
| 601 | `let swe = json!({ "plan_markdown": "åäö" });` (→ `assert_eq!(…, "create_plan(plan_markdown: 6 chars)")` at 602-605) | **(b)** |

### 1.9 `src/harness/pty.rs` / `src/harness/pty_tests.rs` — byte-level UTF-8 docs (b)

| file:line | content | class |
|---|---|---|
| `src/harness/pty.rs:980` | `/// (never a split \`å\`/\`é\`/\`🙂\`, and never a mid-character byte slice).` | (b) doc reference |
| `src/harness/pty_tests.rs:744` | `assert!(is_utf8_lead_byte(0xC3)); // 2-byte lead (\`å\`, \`ä\`, \`ö\`)` | **(b)** — the comment names the byte `0xC3`; keep both |
| `src/harness/pty_tests.rs:750` | `assert!(!is_utf8_lead_byte(0xA4)); // second byte of \`ä\`` | **(b)** — the comment names the continuation byte `0xA4` |

### 1.10 Out of scope (for awareness only — **do not let a cleanup task rewrite these**)

* `docs/recon_bugs_harness_llm_ui.md:301` — existing U10 finding that already names the Swedish
  `följdfråga` heuristic in `src/ui/transcript.rs:104-148`.
* Persisted runtime data (historical user sessions, not source): `.marmel/.ui_transcript.json`
  (19 lines with diacritics), `.marmel/.session_transcript.json` (13),
  `.marmel/execution_plan.md` (3), `.marmel/prompts/t-011-validation.md` (2), plus `.marmel.old/*`.
  These are re-loaded byte-wise (`UiTranscript::load`, `transcript.rs:82-91`); translating them
  would silently rewrite real session history.
* No Swedish exists in `prompts/`, `README.md`, `marmel.toml.example`, `marmel.toml.cloud`,
  `marmel.toml.df`, or `.github/`.

---

## 2. GLOSSARY — single source of truth for the translation

Rule: a literal's **fixture site(s) and assertion site(s) must be changed to the identical byte
string in the same commit**. The English strings below are the agreed replacements (ASCII-only, no
apostrophes, no raw quotes — chosen so they stay safe inside the SSE/JSON nesting in §4).

### G1. Cross-module pair — `src/orchestrator/notice_tests.rs` ↔ `src/ui/transcript.rs` ↔ `src/orchestrator/notice.rs`

Shared token: **`följdfråga`**.

| site | file:line | what it is |
|---|---|---|
| production heuristic | `src/ui/transcript.rs:146` | substring match on a SteerResponse ("pending decision") |
| production prompt example | `src/orchestrator/notice.rs:1144` | the Swedish text the model is *invited* to produce |
| fixture | `src/orchestrator/notice_tests.rs:503` | `"Ställer följdfråga om bakåtkompatibilitet..."` |
| assertion | `src/orchestrator/notice_tests.rs:556` | `text.contains("Ställer följdfråga om bakåtkompatibilitet")` (prefix, no `...`) |
| fixture | `src/orchestrator/notice_tests.rs:766` | `"Ställer en följdfråga till Coder gällande transaktionsisolering..."` |
| assertion | `src/orchestrator/notice_tests.rs:809` | `assert_eq!(eval.user_status.as_deref(), Some("Ställer en följdfråga till Coder gällande transaktionsisolering..."))` (exact) |

Recommended replacements:

| Swedish | English replacement (exact) |
|---|---|
| `Ställer en följdfråga till Coder för att reda ut detaljerna...` (notice.rs:1144) | `Asking Coder a follow-up to clarify the details...` |
| `Ställer följdfråga om bakåtkompatibilitet...` (503) | `Asking a follow-up about backward compatibility...` |
| ↳ assertion prefix (556) | `Asking a follow-up about backward compatibility` (must remain a byte-prefix of the 503 replacement) |
| `Ställer en följdfråga till Coder gällande transaktionsisolering...` (766 **and** 809) | `Asking Coder a follow-up about transaction isolation...` (both sites byte-identical) |
| `följdfråga` matcher (transcript.rs:146) | **delete the arm** (see §3) — it has no English counterpart to substitute |

### G2. Cross-module pair — `src/ui/transcript.rs` ↔ `tests/test_ui_session.rs`

Literal **`Hur går det för codern?`** → **`How is the coder doing?`**

| site | file:line | role |
|---|---|---|
| `src/ui/transcript.rs:431` | fixture (`UiRecord::User`) |
| `src/ui/transcript.rs:463` | assertion (`assert_eq!`) |
| `tests/test_ui_session.rs:748` | comment (cosmetic, update for consistency) |
| `tests/test_ui_session.rs:750` | mock SSE payload (escaped, 3 levels) — `message` of the `ForwardNotice` subtask |
| `tests/test_ui_session.rs:774` | comment |
| `tests/test_ui_session.rs:780` | fixture (user msg into `spawn_steer_arbitration`) |
| `tests/test_ui_session.rs:802` | assertion (`assert_eq!(hist[0].0, …)`) |
| `tests/test_ui_session.rs:826` | assertion (`assert_eq!(hist[0].0, …)`) |

### G3. Near-duplicate pair that must NOT be conflated (different spellings)

| Swedish literal | sites | English replacement (exact) |
|---|---|---|
| `Codern har tagit bort jit_invalidate_all och alla 7 tester passerar.` | `src/ui/transcript.rs:449` (fixture), `:466` (assertion) | `The coder removed jit_invalidate_all and all 7 tests pass.` |
| `Codern har tagit bort jit_invalidate_all()-anropen och ctest 7/7 passerar.` | `tests/test_ui_session.rs:740` (SSE fixture), `:828` (assertion), `:865` (assertion) | `The coder removed the jit_invalidate_all() calls and ctest passes 7/7.` |

They describe the same scenario but are *different strings* in *different modules*; a cleanup that
"harmonises" them will break `assert_eq!`/`contains` in the other module.

### G4. Remaining single-module pairs

| Swedish literal | fixture site(s) | assertion site(s) | English replacement (exact) |
|---|---|---|---|
| `Coder bekräftar att async I/O har implementerats.` | notice_tests.rs:392 | 469, 482 | `Coder confirms that async I/O has been implemented.` |
| `Coder har fixat PPC JIT.` | notice_tests.rs:592 | 681 | `Coder has fixed the PPC JIT.` |
| `Debugger har isolerat minnesläckan.` | notice_tests.rs:598 | 659, 686 | `Debugger has isolated the memory leak.` |
| `Vad gör codern?` | notice_tests.rs:627, 637 | 655, 680 | `What is the coder doing?` |
| `Vad gör debuggern?` | notice_tests.rs:631, 638 | 658, 685 | `What is the debugger doing?` |
| `Minnesläckan berodde på en saknad free i malloc-wrapper.` | notice_tests.rs:645 | (none directly; flows into `hist[1].1` → 660 `contains("malloc-wrapper")`) | `The memory leak was caused by a missing free in the malloc wrapper.` — **keep the token `malloc wrapper`** so the 660 assertion can stay `contains("malloc-wrapper")` or be updated to a token present in the new text |
| `Implementerat JIT-instruktionerna för PowerPC 604e.` | notice_tests.rs:669 | (none directly; flows into `hist[0].1` → 682 `contains("PowerPC 604e")`) | `Implemented the JIT instructions for PowerPC 604e.` (keeps `PowerPC 604e`) |
| `Coder förklarar att SQLite valdes för att minimera externa beroenden.` | notice_tests.rs:714 | 752 (`assert_eq!`) | `Coder explains that SQLite was chosen to minimize external dependencies.` |
| `Varför valde du SQLite?` | notice_tests.rs:732 | (flows into `ToolResult` text; not asserted) | `Why did you choose SQLite?` |
| `SQLite har noll konfiguration och fungerar utmärkt med WAL-mode.` | notice_tests.rs:743 | (input only) | `SQLite has zero configuration and works very well with WAL mode.` |
| `Kan vi byta till SQLite?` | notice_tests.rs:782 | (input only) | `Can we switch to SQLite?` |
| `Jag kan byta databas.` | notice_tests.rs:793 | (input only) | `I can switch the database.` |
| `Vad är nästa steg?` | transcript.rs:455 | 468 | `What is the next step?` |
| `Kör real-ROM-test med timeout.` | transcript.rs:458 | 469 | `Run the real-ROM test with a timeout.` |
| `Hur gick testet?` | test_ui_session.rs:840 (fixture) | **742 (mock branch matcher)**, 861 (request-body lookup), 834 (comment) | `How did the test go?` — see R2, this one is behaviour-wiring, not just data |
| `Testet gick bra.` | test_ui_session.rs:745 (SSE fixture) | none | `The test went fine.` |
| `Status: tagit bort de två jit_invalidate_all()-anropen och ctest 7/7 passerar.` | test_ui_session.rs:817 | indirectly 830 (`contains("[Specialist coder]:")`) | `Status: removed the two jit_invalidate_all() calls and ctest passes 7/7.` — see R5 |
| `Schemalägg inga tasks parallellt.` | helpers.rs:705 | 713 | `Do not schedule any tasks in parallel.` (matches the already-English sibling at `src/config.rs:732`: `Do not schedule tasks in parallel on this machine.`) |
| `Normalisera plan-läsningen` | helpers.rs:793 (`PLAN_MD`, used once at 913) | not asserted literally (939-942 compares against the on-disk file) | `Normalize plan reading` — **leave `` `åäö` ✅ `` and the `—` on line 793 untouched**; 944 pins `` `åäö` ✅ `` |

### G5. Declared pair that does **not** exist (verified)

The task brief mentions a fixture/assertion pair **between `src/ui/helpers.rs` and
`tests/test_ui_session.rs`**. There is none: the intersection of Swedish literals between those two
files is empty (`grep -P '[åäöÅÄÖ]'` in `tests/test_ui_session.rs` returns 7 lines, none of which
occurs in `src/ui/helpers.rs`, and vice versa). `helpers.rs` Swedish is paired **within its own
file**: 705 ↔ 713 (base prompt) and 793 ↔ 944 (plan fixture ↔ assertion). The nearest genuine
`helpers.rs` ↔ `test_ui_session.rs` coupling is *behavioural, not literal*: the system-prompt plan
block (`src/ui/helpers.rs:123-129` `## Machine Specific Guidance`, `helpers.rs:792-795` `PLAN_MD`)
against the English plan bodies at `tests/test_ui_session.rs:1874` and the prompt-shape assertions
at `tests/test_ui_session.rs:1997` and `:2007` — no Swedish on either side, so translating the
`helpers.rs` prose cannot affect `test_ui_session.rs`.

---

## 3. `src/ui/transcript.rs:146` — what the Swedish matcher actually does

### 3.1 The heuristic

`UiTranscript::extract_steering_history` (`src/ui/transcript.rs:111-165`) walks the persisted UI
journal and pairs each non-synthetic `UiRecord::User` with the following
`UiRecord::SteerResponse`s:

1. `text` is trimmed and its `"[Arbitrator]:"` prefix is stripped (`:129-134`).
2. Empty responses are skipped (`:136-138`).
3. If the current user question equals the most recent history entry's question (`:140-142`), the
   new response is either a **replacement** or an **append**, decided by
   `last_resp.contains("awaiting specialist reply") || last_resp.starts_with("Decision:") ||
   last_resp.contains("följdfråga") || last_resp.contains("follow-up")` (`:144-148`):
   * **true** → `*last_resp = clean_resp` (the placeholder/pending line is *overwritten* by the real
     answer, so the history keeps one entry per question);
   * **false** → `last_resp.push('\n'); last_resp.push_str(&clean_resp)` (two genuine answers are
     concatenated).
4. If there is no pending question, a new `(user_q, clean_resp)` entry is pushed (`:154-156`), or
   `("User inquiry", clean_resp)` when `current_user` is `None` (`:157-159`).

So `följdfråga` at `:146` is one of four **literal-substring markers that mean "this SteerResponse
was only a pending-decision placeholder"**. The Swedish arm exists because of a specific
production chain:

* `src/orchestrator/notice.rs:1144` instructs the arbitrator model to phrase `user_status` in the
  *user's language* with a **Swedish example** (`Ställer en följdfråga till Coder …`),
* `src/harness/mod.rs:840-848` emits that `user_status` verbatim as
  `Event::SteerResponse("\n[Arbitrator]: {user_status}\n\n")`,
* `src/ui/session.rs:399 / 985 / 1078 / 1513 / 1592` persist it as `UiRecord::SteerResponse`,
* and `transcript.rs:146` then greps that stored text for the Swedish word to decide "replace,
  don't append".

This is exactly the fragility already recorded at `docs/recon_bugs_harness_llm_ui.md:301`
(U10: "the 'pending decision' heuristic matches literal substrings including Swedish
`följdfråga`, `awaiting specialist reply`, `Decision:` ⇒ breaks on any reworded prompt. Fix: typed
pending-decision records").

### 3.2 Which tests cover it

| test | file:line | covers `:146` (`följdfråga` arm)? |
|---|---|---|
| `test_extract_steering_history_pairs_and_resolves_pending` | `src/ui/transcript.rs:419-470` | **No.** It is the *only* caller of `extract_steering_history` (call at `:461`) and it triggers the merge through the English arm `"awaiting specialist reply"` (fixture at `:435`). It asserts the *result* of the merge at `:464-467`. |
| `test_ui_transcript_roundtrip_serialization` / `save_and_load` / `from_legacy_messages_*` / `test_delegate_summary_*` | `src/ui/transcript.rs:280, 326, 342, 477, 551` | No — different code path. |
| `test_handle_reply_to_arbitrator_ask_followup` | `src/orchestrator/notice_tests.rs:494` | Produces exactly the Swedish `user_status` (`:503`) and asserts the emitted `SteerResponse` contains it (`:556`) — but **never feeds it into `extract_steering_history`**. |
| `test_evaluate_worker_reply_ask_followup` | `src/orchestrator/notice_tests.rs:759` | Asserts the Swedish `user_status` value itself (`:809`); no transcript involved. |
| `test_steering_conversation_history_accumulates_worker_reply_to_arbitrator` | `tests/test_ui_session.rs:723-868` | Exercises the **bus** history (`record_steering_exchange`), not the transcript heuristic. |
| `tests/test_steer_history.rs` (whole file, 338 lines) | e.g. `:102, 128, 155, 186, 269, 292` | Bus-side correlation only; uses `pending()` = `"Forwarded notice {id} to {target} (awaiting specialist reply)"` (`:67-69`). |

**Conclusion: the `följdfråga` arm at `transcript.rs:146` has zero test coverage.** `git log
-S'följdfråga' -- src/ui/transcript.rs` → single commit `f080737` ("feat(steer): persist and
rehydrate specialist replies in arbitrator history"); `git log -S'extract_steering_history'` →
`f080737`, `974b601`. At HEAD the method has **no in-crate caller** (only its own unit test), i.e.
the heuristic is currently reachable only through the public API — the live session keeps steering
history in `STEERING_HISTORY` via the bus (`src/ui/session.rs:294-297`, `src/orchestrator/bus.rs:170-217`).

### 3.3 What a language-neutral replacement would require

The bus side already has the language-neutral shape: `src/orchestrator/steer.rs:460-470`

```rust
const PENDING_ENTRY_MARKERS: [&str; 3] =
    ["awaiting specialist reply", "ForwardToWorker", "follow-up"];

pub(crate) fn steering_entry_is_pending(entry_response: &str) -> bool {
    entry_response.starts_with("Decision:")
        || PENDING_ENTRY_MARKERS.iter().any(|marker| entry_response.contains(*marker))
}
```

— note it contains **no Swedish marker at all**. Options, in order of safety:

1. **Delete the arm** `|| last_resp.contains("följdfråga")` and keep the other three. Cheapest,
   aligns the transcript predicate with `steer::steering_entry_is_pending` (minus
   `"ForwardToWorker"`), and no test currently exercises the arm, so it cannot regress a test.
   Behavioural cost: in a Swedish session a `user_status` SteerResponse that happens *not* to
   contain "awaiting specialist reply"/"follow-up"/"Decision:" would stop being replaced and instead
   be concatenated with `'\n'` (cosmetic history duplication — the same class as U10).
2. **Share one owner**: call `crate::orchestrator::steering_entry_is_pending(...)` from
   `extract_steering_history` instead of re-listing literals. Requires exposing the `pub(crate)`
   helper across modules; behaviour change: the transcript arm would additionally accept
   `"ForwardToWorker"`.
3. **Do it properly (the U10 fix)**: stop detecting "pending" by prose at all — emit the pending
   placeholder as a typed record/field (e.g. `UiRecord::SteerResponse { text, pending: bool }` or a
   dedicated `PendingDecision` variant, and/or record the notice id) so the merge condition is an
   identity check, exactly like `bus.rs:184-210` (`entry_names_notice_id` +
   `steering_entry_is_pending`). Cost: `UiRecord` is `Serialize`/`Deserialize` and persisted in
   `.marmel/.ui_transcript.json` (`transcript.rs:9, 82-103`), so the new field needs
   `#[serde(default)]` for old journals, and **existing persisted Swedish transcripts keep their old
   text** — a typed flag cannot be recovered from them, so the substring arms must survive (or
   degrade gracefully) for historical data.
4. Whichever option is chosen: **add a test that pins the replacement predicate** (today the arm is
   untested, so "no test fails" is not evidence of correctness), and translate `notice.rs:1144` in
   the same commit so no Swedish text is being generated while the matcher is gone.

---

## 4. Risks

**R1 — Escaping hazard (3-level nesting).** `tests/test_ui_session.rs:740`, `:745`, `:750` hold the
Swedish text inside a Rust string literal that *is* an SSE body that *contains* a JSON object whose
`delta.content` is *itself* a JSON string. The Swedish sits between `\\\"…\\\"`. Translation rules:
keep the `\\\"` delimiters byte-identical, introduce **no** raw `"`, no backslash, no literal
newline, and keep the payload parseable as JSON after two unwrappings. Compare the English siblings
at `tests/test_ui_session.rs:629`, `:633`, `:881`, `:965`, `:416` for the proven shape.
(`src/orchestrator/notice_tests.rs` fixtures at 392/503/592/598/645/669/714/766 are `json!` values
that are serde-serialized by `sse_delta` at `:692-706`; serde_json emits non-ASCII verbatim, so
those are escaping-safe — and become *simpler* once ASCII.)

**R2 — A Swedish literal that is control flow, not data.** `tests/test_ui_session.rs:742`
(`} else if body.contains("Hur gick testet?") {`) selects which canned response the mock returns,
and `:861` (`.find(|b| b.contains("Hur gick testet?"))`) selects which recorded request body is
asserted. Both are paired with the fixture at `:840`. Translating only some of these three makes the
mock return the turn-1 `ForwardToWorker` body for turn 2, and `:862`
(`expect("Turn 2 request must exist")`) then panics — a confusing failure, not a clean assert.
Same file, same rule for `:780` ↔ `:802`/`:826`.

**R3 — Byte/length assertions that depend on multi-byte Swedish data (never translate these to
ASCII):**

| site | assertion | why it is byte-dependent |
|---|---|---|
| `src/ui/helpers.rs:644-646` | `assert_eq!(s.len(), 121)`; `assert!(!s.is_char_boundary(120))` | `"ä".repeat(60)` = 120 bytes (2 B each); an ASCII word would make 120 a valid boundary and the regression guard vacuous |
| `src/ui/helpers.rs:656-660` | `assert_eq!(out.chars().count(), 123)`; `!out.contains(REPLACEMENT_CHARACTER)` | `"ä".repeat(150)` = 301 bytes / 151 chars; drives the truncation branch (`head.len() != s.len()` compares **bytes**, `helpers.rs:454`) |
| `src/ui/helpers.rs:671-675` | `truncate_reason(&"ä".repeat(21), 20) == "ä"×20 + "..."` | 21 chars = 42 bytes vs 20 chars = 40 bytes |
| `src/ui/helpers.rs:681-689` | `out.ends_with("...")`, no `U+FFFD` | `självråkdiskriminering 👍 ` + `wäsen`×30 exercise the multi-byte word-break path (`rfind(' ')` + `chars().count() > max_len*2/3`, `helpers.rs:458-462`) |
| `src/text_util.rs:83-86` | `truncate_chars("åäö",2) == "åä"`, `truncate_with_ellipsis("åäöabcdef",6) == "åäö..."` | `å` = 2 bytes ⇒ byte-index 3 is mid-char; ASCII would remove the hazard |
| `src/text_util.rs:103, 121` | `truncate_chars("åäö",0) == ""`; `for n in 0..=10 { … }` over `"a🙂äö漢字"` | char-boundary validity across 2/4/3-byte units |
| `src/tool_args.rs:558-565` | `clipped.chars().count() == "custom(".len() + DECORATED_BUDGET - CLIP_SLACK + 1 + 1` | char-count arithmetic on `"åäö"×40` |
| `src/tool_args.rs:601-605` | `assert_eq!(preview_tool_call(TOOL_CREATE_PLAN, &swe), "create_plan(plan_markdown: 6 chars)")` | **`åäö` is 3 chars but 6 bytes, and the code reports the byte count under a "chars" label** — replacing the payload changes the expected literal `6 chars` |
| `tests/test_harness.rs:133-134` | `assert!(payload.len() > READ_FILE_MAX_BYTES)` | `"🦀 åäö αβγ"` = 9 chars / **18 bytes** × 30 000 = **540 000 bytes** |
| `tests/test_harness.rs:164-165, 184-185` | `res.content.starts_with(&payload.chars().take(2000)…)` | the expected head is derived from the payload itself |
| `tests/test_harness.rs:215-219` | `assert_eq!(res.content.as_bytes(), payload.as_bytes())` | byte-identity with `"héllo wörld 🦀\n"`×100 |
| `src/harness/pty_tests.rs:744, 750` | `is_utf8_lead_byte(0xC3)` / `!is_utf8_lead_byte(0xA4)` | the comments name `å/ä/ö`; the numbers are the real assertion — keep comment and byte in sync |

**R4 — Stale byte-count comments inside protected UTF-8 data (flag, do not silently "fix"):**
`tests/test_harness.rs:131-133` claims *"21 bytes per repetition … 630 000 bytes"*, but the literal
`"🦀 åäö αβγ"` is 18 bytes/repetition ⇒ 540 000 bytes (measured; `payload.len() > READ_FILE_MAX_BYTES`
still holds). `:210`'s "1 400 chars / 1 900 bytes" is correct (14 chars / 19 bytes × 100).
`src/ui/helpers.rs:643` ("121 bytes") and `src/tool_args.rs:600` ("3 characters but 6 bytes") are
correct. Any cleanup task that edits these payloads must recompute, not trust, the comments.

**R5 — A behavioural coupling hidden behind a translation.** `src/harness/mod.rs:874-878`:
`let full_history_resp = if resp.contains(message) { resp.clone() } else {
format!("{resp}\n[Specialist {caller}]: {message}") };`
In `tests/test_ui_session.rs`, `resp` is the mock's Swedish answer (`:740`) and `message` is the
worker's Swedish reply (`:817`); today `resp` does **not** contain `message`, which is what makes
`assert!(hist[0].1.contains("[Specialist coder]:"))` at `:830` pass. If a translation makes the two
strings equal or nested (e.g. by reusing one English sentence for both), `:830` breaks — and the
same `contains` relation is mirrored in `src/orchestrator/notice_tests.rs:484-488` and `:574-580`.
Keep fixture texts mutually non-containing when translating.

**R6 — Prefix/exact-match asymmetries.** `notice_tests.rs:503` (fixture) ends with `...` while the
assertion at `:556` is a `contains` of the same text **without** the dots — the "no dots" prefix
relation must survive translation. `:766`/`:809` are an `assert_eq!` pair (exact, dots included).
`transcript.rs:449` wraps its Swedish inside the production envelope `"\n[Arbitrator]: …\n"`, and the
strip happens at `transcript.rs:131` — translate only the inner sentence, keep the envelope bytes.

**R7 — Untested production arm.** Removing `transcript.rs:146` cannot break any test today (§3.2);
therefore "tests pass" is *not* proof of equivalence. Land the §3.3 option 1 or 2 together with a new
test that pins the intended pending-marker set (mirroring `tests/test_steer_history.rs:67-69` style).

**R8 — Runtime/persisted data.** `.marmel/.ui_transcript.json` and `.marmel/.session_transcript.json`
contain historical Swedish from real sessions. Translating fixture prose does **not** invalidate
them (`extract_steering_history` only filters English synthetic prefixes, `transcript.rs:118-123`),
but they must not be "cleaned up" as if they were source files.

**R9 — Cosmetic, not behavioural:** test-name spelling `truncate_chars_sweedish_at_boundary_crossing_limit`
(`src/text_util.rs:81`, doubled "e") and the misspelling-adjacent naming of Swedish payloads
(`let swedish` / `let swe`, `src/tool_args.rs:558, 601`) — safe to rename, no assertions depend on
identifiers here.

---

## 5. Bottom line for a cleanup task

* Translate **G1-G5** (48 fixture/assertion/comment sites across 4 files:
  `src/orchestrator/notice_tests.rs` (28), `tests/test_ui_session.rs` (15),
  `src/ui/transcript.rs` (8), `src/ui/helpers.rs` (2: 705+713) plus the prose half of
  `helpers.rs:793`) — always fixture + assertions in the same commit, using the exact strings in §2.
* **Never** touch the 30 UTF8-DATA lines: `src/text_util.rs:10, 82-86, 103, 121`;
  `src/tool_args.rs:26, 558, 600-601`; `src/ui/helpers.rs:452, 643-644, 656, 673-674, 684-685`, and
  the `` `åäö` ✅ `` token on `src/ui/helpers.rs:793` + the assertion at `:944`;
  `src/harness/pty.rs:980`; `src/harness/pty_tests.rs:744, 750`;
  `tests/test_harness.rs:133, 210`.
* PRODUCTION-LOGIC: `src/ui/transcript.rs:146` → drop the Swedish arm (or move to
  `steer::steering_entry_is_pending`) **and** translate the `src/orchestrator/notice.rs:1144`
  example in the same commit; add a coverage test for the replacement predicate.
