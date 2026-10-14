# Comment Audit — Core Batch (`src/agents/**`, `src/harness/**`, `src/llm/**`, `src/net/**`, `src/mcp/**`)

Read-only audit. **No source file was modified.** All `file:line` anchors below were read
directly out of the files at audit time; every quoted comment was copied verbatim.

## Method

| Step | How it was measured |
|---|---|
| Enumeration | `grep -nE '^[[:space:]]*//'` over the 5 scoped trees → every *own-line* comment (doc `///`/`//!` included). |
| Inline comments | String-aware Python scan (skips `"…"`/`r#"…"#`/char literals) for `//` that is **not** at line start → trailing comments. |
| Dead code | Three independent detectors: comment bodies ending in `;` / `{` / `(`, keyword-leading bodies (`let`/`if`/`return`/`assert`/…), and `;\s*$` after comment marker. |
| Redundant banners | Comment lines whose body, after stripping `/`, is only `-=─━~*` run characters (no title text). |
| Duplicates | Exact/near-exact text-set comparison between comment blocks inside each file. |
| Stale | Every referenced identifier / path / numeric constant resolved against the working tree. |
| Guard tests | `tests/test_tool_name_literals.rs` and `tests/test_role_gating.rs` logic re-implemented in Python and re-run against the tree (both currently green). |

## Totals for this batch (baseline for measuring the cleanup delta)

| Metric | Value |
|---|---|
| `.rs` files in scope | **51** |
| Total lines | **27,875** |
| Own-line comment lines (`//`, `///`, `//!`) | **4,599** |
| …of which doc comments (`///` / `//!`) | 3,447 |
| Inline trailing comment lines | **+27** |
| Own-line comment density | **16.5 %** |
| Pure separator ("rule-only") comment lines | **62** |
| Commented-out dead code lines | **0** |

Highest-density files: `src/net/mod.rs` 46.7 %, `src/net/http.rs` 45.3 %,
`src/harness/monitor/mod.rs` 43.0 %, `src/harness/search.rs` 26.5 %,
`src/harness/monitor/common.rs` 26.3 %, `src/harness/pty.rs` 25.8 %.

---

# 1. Removable

Format: `file:line — category — one-line quote`.

## 1.1 `commented-out-dead-code` — none found

**0 occurrences in scope.** Verified three independent ways (§Method). The only regex
hits were prose punctuation inside doc text, e.g. `src/mcp/protocol.rs:15`, `:17`, `:19`
— legitimate spec bullets, not disabled code. No action needed for this category in this
batch.

## 1.2 `redundant-banner` — 62 rule-only lines

These lines carry **zero** information: the body is a bare horizontal rule (`---…`, `──…`
U+2500) with no title. The *titled* banners they pair with are informative and must stay
(e.g. `src/harness/monitor/monitor_tests.rs:311`
`// --- Composed HarnessMonitor (runtime facade) ---`). Recommended fix is to delete the
rule lines (or collapse the pair into a single titled line), not to delete the titles.

```
src/agents/prompt_builder.rs:880          — // -----------------------------------------------------------------
src/agents/prompt_builder.rs:891          — // -----------------------------------------------------------------
src/harness/dispatch_integrity_tests.rs:76 — // ---------------------------------------------------------------------------
src/harness/dispatch_integrity_tests.rs:78 — // ---------------------------------------------------------------------------
src/harness/dispatch_integrity_tests.rs:269 — // ---------------------------------------------------------------------------
src/harness/dispatch_integrity_tests.rs:271 — // ---------------------------------------------------------------------------
src/harness/dispatch_integrity_tests.rs:377 — // ---------------------------------------------------------------------------
src/harness/dispatch_integrity_tests.rs:379 — // ---------------------------------------------------------------------------
src/harness/monitor/code.rs:241           — // ---------------------------------------------------------------------------
src/harness/monitor/code.rs:243           — // ---------------------------------------------------------------------------
src/harness/monitor/mod.rs:30             — // ---------------------------------------------------------------------------
src/harness/monitor/mod.rs:32             — // ---------------------------------------------------------------------------
src/harness/monitor/mod.rs:55             — // ---------------------------------------------------------------------------
src/harness/monitor/mod.rs:57             — // ---------------------------------------------------------------------------
src/harness/monitor/mod.rs:232            — // ---------------------------------------------------------------------------
src/harness/monitor/mod.rs:234            — // ---------------------------------------------------------------------------
src/harness/monitor/mod.rs:236            — // ---------------------------------------------------------------------------   (duplicate banner, see 1.6)
src/harness/monitor/mod.rs:238            — // ---------------------------------------------------------------------------   (duplicate banner, see 1.6)
src/harness/monitor/repetition.rs:98      — // ---------------------------------------------------------------------------
src/harness/monitor/repetition.rs:100     — // ---------------------------------------------------------------------------
src/harness/monitor/repetition.rs:230     — // ---------------------------------------------------------------------------
src/harness/monitor/repetition.rs:232     — // ---------------------------------------------------------------------------
src/harness/monitor/xml.rs:134            — // ---------------------------------------------------------------------------
src/harness/monitor/xml.rs:136            — // ---------------------------------------------------------------------------
src/harness/monitor/xml.rs:432            — // ---------------------------------------------------------------------------   (also stale, see 1.6)
src/harness/monitor/xml.rs:434            — // ---------------------------------------------------------------------------   (also stale, see 1.6)
src/harness/pty.rs:126                    — // ---------------------------------------------------------------------------
src/harness/pty.rs:128                    — // ---------------------------------------------------------------------------
src/harness/pty.rs:443                    — // ---------------------------------------------------------------------------   (block prose at 444-459 MUST stay, see 2.2)
src/harness/pty.rs:460                    — // ---------------------------------------------------------------------------   (block prose at 444-459 MUST stay, see 2.2)
src/harness/pty.rs:970                    — // ---------------------------------------------------------------------------
src/harness/pty.rs:972                    — // ---------------------------------------------------------------------------
src/harness/pty_tests.rs:191              — // ---------------------------------------------------------------------------
src/harness/pty_tests.rs:195              — // ---------------------------------------------------------------------------
src/harness/pty_tests.rs:583              — // ---------------------------------------------------------------------------
src/harness/pty_tests.rs:586              — // ---------------------------------------------------------------------------
src/harness/pty_tests.rs:753              — // ---------------------------------------------------------------------------
src/harness/pty_tests.rs:759              — // ---------------------------------------------------------------------------
src/harness/sandbox.rs:502                — // -----------------------------------------------------------------------
src/harness/sandbox.rs:505                — // -----------------------------------------------------------------------
src/harness/sandbox.rs:549                — // -----------------------------------------------------------------------
src/harness/sandbox.rs:552                — // -----------------------------------------------------------------------
src/harness/sandbox.rs:672                — // -----------------------------------------------------------------------
src/harness/sandbox.rs:674                — // -----------------------------------------------------------------------
src/harness/sandbox.rs:812                — // -----------------------------------------------------------------------
src/harness/sandbox.rs:814                — // -----------------------------------------------------------------------
src/harness/sandbox.rs:902                — // -----------------------------------------------------------------------
src/harness/sandbox.rs:904                — // -----------------------------------------------------------------------
src/llm/stream_tests.rs:229               — // ---------------------------------------------------------------------------
src/llm/stream_tests.rs:234               — // ---------------------------------------------------------------------------
src/llm/thinking.rs:512                   — // ---------------------------------------------------------------------------
src/llm/thinking.rs:514                   — // ---------------------------------------------------------------------------
src/llm/thinking_tests.rs:115             — // ---------------------------------------------------------------------------
src/llm/thinking_tests.rs:120             — // ---------------------------------------------------------------------------
src/mcp/http_tests.rs:41                  — // ──────────────────────────────────────────────────────────────────────────
src/mcp/http_tests.rs:43                  — // ──────────────────────────────────────────────────────────────────────────
src/mcp/http_tests.rs:177                 — // ──────────────────────────────────────────────────────────────────────────
src/mcp/http_tests.rs:179                 — // ──────────────────────────────────────────────────────────────────────────
src/mcp/http_tests.rs:342                 — // ──────────────────────────────────────────────────────────────────────────
src/mcp/http_tests.rs:344                 — // ──────────────────────────────────────────────────────────────────────────
src/mcp/http_tests.rs:403                 — // ──────────────────────────────────────────────────────────────────────────
src/mcp/http_tests.rs:406                 — // ──────────────────────────────────────────────────────────────────────────
```

⚠ Exception: `src/harness/monitor/xml.rs:432` and `:434` bracket a banner that itself is
stale (1.6) — but see §4: `xml.rs` comment lines at 16/18/219/220/379 are **load-bearing**
for a repo-wide guard test; 432/434 are not among them, so they are safe to delete.

## 1.3 `restates-code`

The comment says nothing the following statement does not already say.

```
src/harness/pty_tests.rs:117            — restates-code — "// Spawn a shell"            (above `mgr.spawn(...)`)
src/harness/pty_tests.rs:129            — restates-code — "// Write a command"          (above `mgr.write(...)`)
src/harness/pty_tests.rs:154            — restates-code — "// List sessions"            (above `mgr.list()`)
src/harness/pty_tests.rs:159            — restates-code — "// Close session"            (above `mgr.close(...)`)
src/llm/stream_tests.rs:147             — restates-code — "// First chunk passes"
src/llm/stream_tests.rs:174             — restates-code — "// Stream content"
src/llm/stream_tests.rs:187             — restates-code — "// Start thinking block"
src/harness/fs_tests.rs:84              — restates-code — "// Second page."
src/harness/fs_tests.rs:100             — restates-code — "// Final page."
src/harness/fs_tests.rs:242             — restates-code — "// Valid workspace path succeeds"
src/harness/mod.rs:1257                 — restates-code — "// Now rebirth succeeds again"
src/mcp/client.rs:524                   — restates-code — "// Empty input list yields an empty result."
src/agents/catalog.rs:100               — restates-code — "// Built-in skills"          (above a literal `&[...]` of built-ins)
src/agents/catalog.rs:127               — restates-code — "// Built-in baseline archetypes"
src/agents/prompt_builder.rs:447        — restates-code — "// Domain keyword heuristics"
src/agents/prompt_builder.rs:487        — restates-code — "// Gather allowed tools from selected skills and archetype"
src/agents/prompt_builder.rs:511        — restates-code — "// Assemble synthesized prompt"
src/agents/prompt_builder.rs:625        — restates-code — "// 1. Worker prompt"
src/agents/validation.rs:110            — restates-code — "// 2. Extract comments / critique"   (above `args.get("comments")`; paired step marker at :73)
src/llm/stream.rs:26                    — restates-code — "/// Continue streaming chunks."      (on `fn continue_stream(&mut self)`)
src/llm/stream.rs:28                    — restates-code — "/// Abort the stream completely."    (on `fn abort(&mut self)`)
src/llm/thinking.rs:43                  — restates-code — "/// See [`MIN_TEMPERATURE`]."        (doc of `min_temperature()`)
src/llm/thinking.rs:49                  — restates-code — "/// See [`MIN_PENALTY`]."
src/llm/thinking.rs:54                  — restates-code — "/// See [`MIN_TOP_P`]."
src/harness/common.rs:14                — restates-code — "/// Whether this was an error result." (on `pub is_error: bool`)
src/harness/common.rs:97                — restates-code — "/// The Manager."                    (on `manager` field)
src/harness/fs.rs:537                   — restates-code — "// Ensure 0o755 permissions on the newly created parents." (the call is literally `set_dir_permissions_755(parent)`)
```

Borderline — **keep** unless a stylistic sweep is wanted:
`src/harness/mod.rs:1217` and `:1249` (numbered step markers inside a long test; they do
order the assertions). `src/agents/validation.rs:73` (the sibling marker of `:110` — if
`:110` goes, `:73` must go too, otherwise the numbering breaks).

## 1.4 `over-explained-obvious`

```
src/harness/pty_tests.rs:122            — over-explained-obvious — "// Shell banner or prompt"   (inline, on the tautology `assert!(init_out.is_empty() || !init_out.is_empty());`)
src/harness/monitor/monitor_tests.rs:180 — over-explained-obvious — "// length 5"                (inline, on `let block = "abcde";`)
src/harness/monitor/repetition.rs:24    — over-explained-obvious — "/// rolling window."        (:21 and :24 both end with the identical phrase "rolling window."; keep one, drop the echo)
```

## 1.5 `duplicate-narrative`

Same explanation written twice (or three times) in one file. Keep the authoritative copy,
delete the echo.

```
src/harness/mod.rs:920-921              — duplicate-narrative — "t-034c: no MCP pre-lookup…"           (verbatim duplicate of :562-563)
src/harness/mod.rs:960-961              — duplicate-narrative — "Built-in match exhausted: MCP is consulted last…" (verbatim duplicate of :605-606)
src/harness/mod.rs:1037-1042            — duplicate-narrative — "t-034c ordering: mcp_route refuses any name…" (6-line block, verbatim duplicate of :631-636)
src/harness/mod.rs:1099-1100            — duplicate-narrative — "The verdict vocabulary is owned by crate::agents::validation…" (verbatim duplicate of :692-693)
src/harness/mod.rs:1102                 — duplicate-narrative — "t-051: the allowlist admitted this call; the identity rule may not." (duplicate of :695)
src/harness/monitor/mod.rs:193-198      — duplicate-narrative — feed_text contract restated a 3rd time (module doc :12-13, facade doc :75-79, method doc :193-198)
src/harness/monitor/mod.rs:75-79        — duplicate-narrative — feed_text contract (2nd of the 3 copies above; keep the method doc, it carries the REQ tags)
src/net/retry.rs:66-67                  — duplicate-narrative — backoff formula restated (module doc :4-5, const doc :20, fn doc :66-67)
src/net/retry.rs:20                     — duplicate-narrative — backoff formula restated (2nd of the 3 copies)
src/agents/validation.rs:515            — duplicate-narrative — "Strict abort semantics: cancellation is a failure, not a verdict." (identical to :390)
src/mcp/protocol.rs:199                 — duplicate-narrative — "/// The compact one-line JSON body a transport puts on the wire." (identical to :165)
src/mcp/mod.rs:29                       — duplicate-narrative — "// ── Name policy ───…" code-layout heading duplicates the rustdoc heading "# Name policy (security t-034c)" at :6   (keep :6, its policy prose :6-18 must stay)
src/mcp/mod.rs:91,93,95,97,105          — duplicate-narrative — variant docs reworded from the sibling enum at :44,:46,:48,:50,:61 (mild: separate rustdoc items — safe to keep both; if trimmed, keep the fuller copy)
src/agents/mod.rs:279-282               — duplicate-narrative — test doc restates the dedup cluster C4 narrative already given at :95-101
```

Resolution guidance for `src/harness/mod.rs`: the **async** dispatch path (:562-563,
:605-606, :631-636, :692-695) sits next to the code it explains and the ordering is
additionally documented once at :317-329 — so the sync duplicates at :920-921, :960-961,
:1037-1042, :1099-1102 are the ones to cut. Do **not** cut both members of any pair.

## 1.6 `stale`

Each item was resolved against the working tree; the referenced thing does not exist, or
the number is wrong.

```
src/harness/monitor/mod.rs:76           — stale — "output into the 1000-char rolling buffer;…"      — capacity is `TEXT_BUFFER_CAPACITY = 16384` (src/harness/monitor/repetition.rs:15); mod.rs:12 already says 16384 → self-contradiction
src/harness/monitor/mod.rs:194          — stale — "1000-char buffer."                               — same, :15-16 of the same doc block says ≥5/≥5 correctly
src/harness/monitor/mod.rs:73-74        — stale — "a ≥3 identical repetition blocks, or an ≥3 alternating cycle cuts" — real default is 5: repetition.rs:13 `DEFAULT_REPETITION_THRESHOLD: usize = 5`, src/config.rs:77, src/llm/stream.rs:143, marmel.toml.example:48; mod.rs:131 says `repetition_threshold = 5`
src/harness/monitor/mod.rs:151-152      — stale — same "≥3" wording for the cycle detector          — same threshold = 5
src/harness/monitor/mod.rs:236-238      — stale — "// Unit tests (Phase B checkpoint: `cargo test --lib test_monitor_`)" appears TWICE back-to-back (:232-234 and :236-238) and nothing follows either banner; the tests are the `#[cfg(test)] #[path = "monitor_tests.rs"] mod tests;` at :240-242
src/harness/monitor/xml.rs:432-434      — stale — banner "Tool repetition & cycle detector (REQ-HARN-002)" sits at EOF with NO code after it in xml.rs — the detector lives in repetition.rs, re-exported at mod.rs:27 `pub use repetition::{…}`
src/harness/pty.rs:465-467              — stale — "/// HAND-OFF (`src/main.rs` is outside this task's file ownership): `main.rs` still spells this literal out; it should reference [`SANDBOX_EXEC_ARG`]…" — src/main.rs:76 already reads `first == Some(harness::pty::SANDBOX_EXEC_ARG)` → hand-off complete, prose obsolete
src/agents/runner/fix_loop.rs:323       — stale — "/// `verdict_recording_tool_spellings_are_owned_by_tool_names` below)." — no such test exists; the real test is fix_loop.rs:1959 `verdict_recording_spellings_have_a_single_owner_in_tool_names`
```

Borderline `stale` — flag, do not blindly delete:

```
src/agents/runner/fix_loop.rs:1782      — stale(borderline) — "/// Work item 3 — the `dispatch_verdict_tools` decision." — the identifier was removed by t-033c and now survives only inside comments (validation.rs:369,:490 explain the removal); the guard at fix_loop.rs:1915 reassembles the needle from fragments, so this line must keep NOT containing the joined needle. Reword, do not paste the identifier back in.
src/harness/pty.rs:105                  — stale(unverifiable) — "(`caesar/marmennill-cli/src/main.rs` and `caesar/src/agent/types.rs`)" — paths do not exist in this repo (external provenance claim). Only in-scope comment referencing a non-existent path; correct the attribution, keep the OSC-strip semantics it documents.
```

---

# 2. Must keep

These comments encode contracts that are **not visible in the code**, or that tests
depend on. Removing any of them loses information or breaks a build-time guard.

## 2.1 UTF-8 / char-boundary invariants

```
src/harness/fs.rs:117-134          — why the read ceiling is a byte cap on the *loaded head* (256 KiB) and what gets reported back.
src/harness/fs.rs:142-153          — two-stage truncation guard; the cut is always walked onto a char boundary.
src/harness/fs.rs:216-220          — UTF-8 lead-byte length table; invalid lead bytes count as length 1 (no panic).
src/harness/fs.rs:235-241          — `drop_partial_utf8_tail`: "scanning back 4 bytes is sufficient. Never panics…" — the bound is the whole point.
src/harness/fs.rs:253              — inline note that the trim is boundary-safe.
src/harness/mod.rs:414-424         — clip keeps head **and** tail so the `READ_FILE_TRUNCATION_MARKER` footer survives clipping.
src/harness/mod.rs:426-445         — `head_end` / `tail_start` are walked to char boundaries at :434-441; deleting this hides the invariant.
src/harness/pty.rs:30-32           — ring buffer drops its head at a UTF-8 char boundary and counts the dropped bytes.
src/harness/pty.rs:67              — same boundary invariant at the field.
src/harness/pty.rs:974-991         — `is_utf8_lead_byte`; "never … land inside a character".
src/harness/pty.rs:1033            — boundary-safe slice in the window trimmer.
src/harness/pty_tests.rs:678-679   — the UTF-8 boundary invariant **tests** rely on (comment states what is being asserted).
src/harness/pty_tests.rs:742       — lead-byte assertion anchor (`å`).
src/harness/pty_tests.rs:744-750   — lead-byte assertions for `漢`, emoji, and the `ä` continuation byte: this is the executable form of the invariant.
src/llm/thinking.rs:62-64          — a thinking tag can straddle two streamed deltas → explains the split-buffer design.
src/llm/thinking.rs:70-71          — same straddling invariant for the tail hold-back.
src/llm/thinking_tests.rs:346-347  — multi-byte / emoji must never panic mid-tag; REQ-tagged invariant.
src/agents/runner/formatting.rs:27-28 — never panic when slicing multi-byte payloads.
src/harness/fs_tests.rs:149        — multibyte UTF-8 safety at the truncation boundary.
src/harness/fs_tests.rs:155        — multibyte UTF-8 safety for the tail trim.
```

## 2.2 Landlock / sandbox fail-closed rationale

```
src/harness/sandbox.rs:8-32        — the fail-closed contract (t-052): "Ok only when enforcement is in effect".
src/harness/sandbox.rs:45-49       — `EX_UNAVAILABLE` semantics when Landlock is absent.
src/harness/sandbox.rs:52-67       — ABI/feature probing rationale for the ruleset.
src/harness/sandbox.rs:92-115      — which rights are requested and why each is required.
src/harness/sandbox.rs:119-123     — why a partial ruleset is refused rather than degraded.
src/harness/sandbox.rs:146-156     — "deliberately does not consult the opt-out" (enforcement probe must be unconditional).
src/harness/sandbox.rs:188-196     — fail-closed parsing of the opt-out env var (anything odd = enforce).
src/harness/sandbox.rs:220-241     — re-entry rules = a security contract, not convenience.
src/harness/sandbox.rs:279-291     — an enforcement failure is never silent.
src/harness/sandbox.rs:306-307     — "`Ok` alone is *not* success" — the single most load-bearing line in the file.
src/harness/sandbox.rs:324-330     — required vs optional rule groups (which failures are fatal).
src/harness/sandbox.rs:343, :354, :367, :411, :434 — allow-list rule-group rationale.
src/harness/sandbox.rs:467-476     — cause chain attached exactly once (dedup rationale).
src/harness/sandbox.rs:488-490     — rationale for the diagnostics emitted before exit.
src/harness/sandbox.rs:502-505     — titled block: "an enforcement failure is never silent" is asserted by a test (keep title :503-504, drop only the rule lines :502/:505).
src/harness/sandbox.rs:549-555     — "This is the exact hole t-052 closes" — regression rationale for the gate (keep title, drop rules :549/:552).
src/harness/sandbox.rs:592-593, :613-614, :626-627, :640-646, :682, :705-706, :727-729, :779 — per-case fail-closed test rationale (rules :672/:674 removable, prose not).
src/harness/sandbox.rs:1012-1013, :1023-1025 — "a skipped integration test must never look like a passing one".
src/harness/sandbox.rs:1043-1044, :1079-1081, :1122-1123, :1155-1156, :1180, :1196, :1238-1239, :1274-1275 — integration-test enforcement evidence.
src/harness/pty.rs:444-459         — t-034b: sandbox gating derives from the **resolved executable**, never from an executable *name*. (The two bracketing rules :443/:460 are in §1.2; the prose is mandatory — see also §4.5.)
src/harness/pty.rs:462-464         — `SANDBOX_EXEC_ARG` is a protocol literal shared with main.rs.
src/harness/pty.rs:470, :512, :541, :551 — gating call sites: why the gate runs where it does.
src/harness/pty.rs:743-757         — re-exec / gate interaction rationale.
```

## 2.3 Security path checks, name policy, grammar gates

```
src/harness/fs.rs:25-26            — `map_path` is the single workspace→host path translation point.
src/harness/fs.rs:39-40            — canonicalisation happens before the containment test.
src/harness/fs.rs:52-54            — "Prevents path traversal attacks (e.g. ../../etc/passwd or absolute escapes)".
src/harness/fs.rs:361-382          — staged temp-file name construction; NAME_MAX bound; a generated byte "can never become a path separator".
src/harness/fs.rs:415-417          — name-length bound rationale for the staging name.
src/harness/fs.rs:436-438, :464-474, :505-507, :583 — containment / symlink / permission checks.
src/harness/workspace.rs:255-264   — an ungated join would let a tool leave the workspace root.
src/mcp/mod.rs:6-18                — the MCP name policy *is* a sandbox-escape vector (server-controlled names → dispatch).
src/mcp/mod.rs:31-38, :53-59, :100-103, :162-168, :191-195 — name grammar: what is rejected and why.
src/mcp/mod.rs:228-243             — split/compose are exact inverses; the invariant is load-bearing.
src/mcp/mod.rs:255-262             — `terminal__run_command` vs built-in `run_command` collision (name-shadowing hazard).
src/mcp/mod.rs:414, :447, :452     — policy-gate call sites on the dispatch path.
src/harness/mod.rs:80-89, :105-108, :113-121, :144-145, :163-165, :187-192 — tool-name normalization/allowlist rules.
src/harness/mod.rs:221-226, :251-255 — route resolution order notes.
src/harness/mod.rs:317-329         — the authoritative t-034c ordering statement (built-in → MCP last).
src/harness/mod.rs:359-369, :631-636 — `mcp_route` refuses names the policy rejects (keep ONE of the :631-636 / :1037-1042 pair).
src/harness/mod.rs:692-695, :1099-1102 — verdict identity gate (t-051): allowlist admitted it, identity rule may not.
src/harness/mod.rs:980-1000        — `normalize_tool_name` is documented as the single table.
src/agents/prompt_builder.rs:32-45, :56-63, :87-90, :578-590, :660-665 — role gating / prompt-synthesis safety rules.
src/agents/prompt_builder.rs:883-890 — task-id grammar is authoritative here; write is fail-closed → "no file at all" on rejection. (Rules :880/:891 are in §1.2; this prose is not.)
src/agents/validation.rs:96-98, :126-134, :211, :234, :248-258 — verdict-shape validation rules.
src/agents/validation.rs:368-373, :384-393, :489-493, :508-520, :562, :586 — verdict ownership/abort semantics (keep ONE of :390 / :515).
src/agents/runner/execution.rs:38-49, :88-94, :296-318, :653-660, :820-832, :886, :930-938, :1010-1033 — execution-path guards: retry, steering, abort, tool gating.
src/agents/runner/fix_loop.rs:300-312, :314-323 — verdict-recording single-owner statement (keep the prose, fix only the stale test name at :323).
src/agents/runner/fix_loop.rs:825-852, :960-1066, :1735, :1782-1796, :1952-1957, :2008-2012 — fix-loop safety invariants and their guards.
src/agents/runner/assembly.rs:3-5, :60-62, :85-88, :120-121 — role/skill assembly invariants.
src/harness/tool_resolution_tests.rs:5-10, :132, :225, :316, :430, :495, :556, :615 — each test's resolution contract.
src/harness/dispatch_integrity_tests.rs:3, :14, :199, :261, :308, :433, :481 — dispatch integrity contracts (rule lines :76/:78/:269/:271/:377/:379 in §1.2).
```

## 2.4 REQ-xxx / t-0NN traceability

Deleting these comments breaks the requirement → code map. Occurrence counts per in-scope
file (verified with `grep -c`):

| File | REQ tags | Gate tags (`t-0NN`) |
|---|---|---|
| src/harness/monitor/mod.rs | 20 | 0 |
| src/harness/monitor/repetition.rs | 7 | 0 |
| src/harness/search.rs | 4 | 0 |
| src/llm/thinking_tests.rs | 7 | 1 |
| src/llm/thinking.rs | 6 | 0 |
| src/harness/fs.rs | 3 | 0 |
| src/harness/fs_tests.rs | 3 | 0 |
| src/agents/coder.rs | 2 | 0 |
| src/agents/validator.rs | 2 | 0 |
| src/harness/mod.rs | 2 | 38 |
| src/harness/monitor/xml.rs | 2 | 0 |
| src/harness/pty.rs | 2 | 2 |
| src/harness/pty_tests.rs | 2 | 3 |
| src/harness/search_tests.rs | 2 | 0 |
| src/agents/debugger.rs / generalist.rs / researcher.rs | 1 each | 0 |
| src/harness/monitor/code.rs / monitor/common.rs | 1 each | 0 |
| src/agents/runner/fix_loop.rs | 0 | 47 |
| src/agents/prompt_builder.rs | 0 | 45 |
| src/agents/runner/execution.rs | 0 | 27 |
| src/agents/validation.rs | 0 | 26 |
| src/agents/mod.rs | 0 | 19 |
| src/harness/workspace.rs | 0 | 10 |
| src/agents/runner/assembly.rs | 0 | 9 |
| src/harness/sandbox.rs | 0 | 8 |
| src/harness/tool_resolution_tests.rs | 0 | 5 |
| src/harness/sleep.rs | 0 | 2 |
| src/mcp/mod.rs | 0 | 1 |
| src/harness/dispatch_integrity_tests.rs | 0 | 1 |
| src/llm/client_tests.rs | 0 | 1 |
| src/llm/stream.rs | 0 | 1 |

Key anchors:

```
src/harness/fs.rs:4-8                        — REQ-TOOL-002 / -003 / -004 mapped to this module.
src/harness/search.rs:3-5, :66, :68          — REQ-TOOL-005 / -006 (the hard output caps are the requirement).
src/harness/pty.rs:3, :803                   — REQ-TOOL-001.
src/harness/pty_tests.rs:3, :1072            — REQ-TOOL-001 verification.
src/harness/fs_tests.rs:8, :61, :134         — REQ-TOOL-002/003/004 acceptance.
src/harness/search_tests.rs:47, :79          — REQ-TOOL-005/006 acceptance.
src/llm/thinking.rs:3-7                      — REQ-LLM-002 / -003 / -004.
src/llm/thinking_tests.rs:24, :50, :63, :83, :104, :346 — REQ-LLM acceptance per case.
src/harness/monitor/xml.rs:1, monitor/mod.rs:1-16, :62-92, :97-107, :142-160, :193-205 — REQ-HARN-001…004 (correct the ≥3/1000 numbers, keep the tags).
src/harness/monitor/repetition.rs:1, :99, :102, :175, :231, :236 — REQ-HARN-002.
src/harness/monitor/code.rs:1, monitor/common.rs:6 — REQ-HARN headers.
src/agents/coder.rs:3, :17; debugger.rs:3; generalist.rs:3; researcher.rs:3; validator.rs:3, :34 — REQ-AGENT headers.
src/harness/mod.rs:1186                      — "REQ-CORE-001: the system prompt stays at [0]".
src/harness/mod.rs:1190                      — "REQ-CORE-002: the pinned goal stays at [1]".
```

## 2.5 Wire-format examples

Byte-for-byte contract documentation; removing them removes the only in-repo statement
of the wire format.

```
src/mcp/protocol.rs:28-31       — ```text block with the exact `{"jsonrpc":"2.0","id":0,"method":"initialize"…}` handshake / notification / tools/list bodies.
src/mcp/protocol.rs:65          — JSON-RPC method names have a single owner in this module.
src/mcp/protocol.rs:176, :194   — request/response envelope shape.
src/mcp/protocol.rs:543, :550, :623-664 — id-matching and notification-echo cases (the wire shapes are the test data).
src/mcp/protocol.rs:698, :718, :729, :750 — malformed-frame wire examples.
src/harness/monitor/xml.rs:15-20 — the three supported XML encodings for a tool call (canonical wire examples — includes the `"read_file"` literals, see §4.2).
src/harness/monitor/xml.rs:57-62, :87-92, :214-220, :264, :285-286, :296, :300-301, :321, :328-329, :343, :367, :379-380, :427 — parser case examples.
src/harness/monitor/xml.rs:405-412 — `pattern` vs `query` argument-alias reconciliation (recon M5).
src/harness/monitor/monitor_tests.rs:20, :35, :60, :316 — raw XML wire fixtures (code, not comments, but see §4.2).
src/llm/client.rs:65-66         — `finish_reason` vocabulary: `stop` / `tool_calls` / `end_turn`.
src/llm/client.rs:475-479       — `ReplyOutcome` contract.
src/llm/client.rs:639, :669, :674-675, :687, :755-756, :781 — request/response field contracts.
src/net/sse.rs:1-9              — SSE framing contract for this parser.
src/net/sse.rs:16-17, :30-35    — 50 ms pump, abort checked **before** the delta, and `on_delta("")` semantics.
src/net/http.rs:3-8, :20-22, :41-43 — HTTP client wrapper contract (retry/auth boundary).
src/mcp/http.rs:11-14           — MCP-over-HTTP transport contract.
src/llm/stream.rs:450-452       — tool advertising goes through the same policy gate as dispatch.
src/llm/thinking.rs:39-41, :46-47, :52 — provider-accepted parameter ranges; outside them → HTTP 400.
src/llm/client_tests.rs:38-40, :53-57, :100-102, :120-121, :152-153, :189-191, :745, :772, :816 — provider payload examples.
```

## 2.6 Invariants tests rely on (monitor semantics)

```
src/harness/monitor/mod.rs:105-107  — the `armed` flag increments exactly once per intervention.
src/harness/monitor/mod.rs:112-114, :130-131, :136-137, :142-145, :150-154, :160-162 — facade thresholds/counters the tests assert (fix the ≥3 numbers at :151-152, keep the rest).
src/harness/monitor/mod.rs:193-198 (single copy), :204-206, :215-216, :221, :226 — feed_text / reset contract asserted by monitor_tests.
src/harness/monitor/repetition.rs:10-28 — every window/capacity constant and its rationale.
src/harness/monitor/repetition.rs:32-36 — the pagination exemption (why repeated page markers do NOT count).
src/harness/monitor/repetition.rs:50-56, :175, :193-202 — `≥threshold` semantics and the `threshold*2` cycle window.
src/harness/monitor/repetition.rs:236-255 (incl. :249-252) — O(window) cost discipline for the tail-period scan (recon M3).
src/harness/monitor/repetition.rs:353, :363-369, :376-383, :419, :432-435, :440, :468, :510, :550, :573, :604-607, :624-625 — per-case expectations the tests encode.
src/harness/monitor/monitor_tests.rs:94, :231, :272, :287, :298, :307, :326, :348-356, :373, :393-394, :549, :586, :600, :613, :649, :654, :700, :728, :760, :804, :840, :848, :866 — case intent comments that name the boundary being probed (:175 is borderline).
```

---

# 3. Per-file comment counts (cleanup-delta baseline)

"Own-line" = a line whose first non-space token is `//`, `///`, `//!` (this is exactly
what the future cleanup delta will be measured against). "Inline" = trailing comment after
code, detected with a string-aware scanner. Ratio = own-line / total.

| # | File | Total lines | Own-line comments | Inline | Ratio |
|---:|---|---:|---:|---:|---:|
| 1 | src/agents/catalog.rs | 622 | 25 | 0 | 4.0 % |
| 2 | src/agents/coder.rs | 66 | 12 | 0 | 18.2 % |
| 3 | src/agents/debugger.rs | 66 | 9 | 0 | 13.6 % |
| 4 | src/agents/generalist.rs | 42 | 7 | 0 | 16.7 % |
| 5 | src/agents/mod.rs | 449 | 66 | 0 | 14.7 % |
| 6 | src/agents/planner.rs | 60 | 9 | 0 | 15.0 % |
| 7 | src/agents/prompt_builder.rs | 1130 | 113 | 0 | 10.0 % |
| 8 | src/agents/researcher.rs | 59 | 9 | 0 | 15.3 % |
| 9 | src/agents/runner/assembly.rs | 174 | 18 | 0 | 10.3 % |
| 10 | src/agents/runner/execution.rs | 1106 | 211 | 0 | 19.1 % |
| 11 | src/agents/runner/fix_loop.rs | 2215 | 499 | 0 | 22.5 % |
| 12 | src/agents/runner/formatting.rs | 58 | 11 | 0 | 19.0 % |
| 13 | src/agents/runner/mod.rs | 101 | 2 | 0 | 2.0 % |
| 14 | src/agents/validation.rs | 1052 | 166 | 0 | 15.8 % |
| 15 | src/agents/validator.rs | 101 | 18 | 0 | 17.8 % |
| 16 | src/harness/common.rs | 159 | 17 | 0 | 10.7 % |
| 17 | src/harness/dispatch_integrity_tests.rs | 491 | 66 | 0 | 13.4 % |
| 18 | src/harness/fs.rs | 696 | 155 | **+1** | 22.3 % |
| 19 | src/harness/fs_tests.rs | 296 | 29 | 0 | 9.8 % |
| 20 | src/harness/mod.rs | 1331 | 220 | 0 | 16.5 % |
| 21 | src/harness/monitor/code.rs | 243 | 11 | 0 | 4.5 % |
| 22 | src/harness/monitor/common.rs | 38 | 10 | 0 | 26.3 % |
| 23 | src/harness/monitor/mod.rs | 242 | 104 | 0 | 43.0 % |
| 24 | src/harness/monitor/monitor_tests.rs | 897 | 91 | **+4** | 10.1 % |
| 25 | src/harness/monitor/repetition.rs | 653 | 161 | **+1** | 24.7 % |
| 26 | src/harness/monitor/xml.rs | 434 | 98 | 0 | 22.6 % |
| 27 | src/harness/plan.rs | 115 | 6 | 0 | 5.2 % |
| 28 | src/harness/pty.rs | 1593 | 411 | 0 | 25.8 % |
| 29 | src/harness/pty_tests.rs | 1091 | 126 | **+5** | 11.5 % |
| 30 | src/harness/sandbox.rs | 1278 | 222 | 0 | 17.4 % |
| 31 | src/harness/search.rs | 536 | 142 | 0 | 26.5 % |
| 32 | src/harness/search_tests.rs | 642 | 46 | **+10** | 7.2 % |
| 33 | src/harness/sleep.rs | 563 | 111 | 0 | 19.7 % |
| 34 | src/harness/tool_resolution_tests.rs | 776 | 101 | 0 | 13.0 % |
| 35 | src/harness/workspace.rs | 278 | 60 | 0 | 21.6 % |
| 36 | src/llm/client.rs | 1090 | 193 | 0 | 17.7 % |
| 37 | src/llm/client_tests.rs | 834 | 62 | 0 | 7.4 % |
| 38 | src/llm/mod.rs | 18 | 2 | 0 | 11.1 % |
| 39 | src/llm/stream.rs | 869 | 57 | 0 | 6.6 % |
| 40 | src/llm/stream_tests.rs | 443 | 27 | 0 | 6.1 % |
| 41 | src/llm/thinking.rs | 519 | 110 | **+1** | 21.2 % |
| 42 | src/llm/thinking_tests.rs | 413 | 32 | **+5** | 7.7 % |
| 43 | src/mcp/client.rs | 811 | 91 | 0 | 11.2 % |
| 44 | src/mcp/http.rs | 579 | 126 | 0 | 21.8 % |
| 45 | src/mcp/http_tests.rs | 870 | 112 | 0 | 12.9 % |
| 46 | src/mcp/mod.rs | 470 | 101 | 0 | 21.5 % |
| 47 | src/mcp/protocol.rs | 786 | 187 | 0 | 23.8 % |
| 48 | src/net/http.rs | 53 | 24 | 0 | 45.3 % |
| 49 | src/net/mod.rs | 15 | 7 | 0 | 46.7 % |
| 50 | src/net/retry.rs | 368 | 81 | 0 | 22.0 % |
| 51 | src/net/sse.rs | 84 | 25 | 0 | 29.8 % |
| — | **TOTAL (51 files)** | **27,875** | **4,599** | **+27** | **16.5 %** |

Removal ceiling implied by §1 (a bound for delta planning, not a target):

| Category | Listed items | Comment lines removable |
|---|---:|---:|
| `redundant-banner` (rule-only) | 62 | 62 |
| `restates-code` | 27 | 27 |
| `over-explained-obvious` | 3 | 3 |
| `stale` — deletable lines not already counted above (`mod.rs:237` title, `xml.rs:433` title, `pty.rs:465-467` hand-off prose) | — | 5 |
| `duplicate-narrative` — echo copies only (excludes the `mcp/mod.rs:91-105` "mild" pair, which is safer to keep) | 14 | 28 |
| **Total** | — | **125** |

That is **125 of 4,599 comment lines ≈ 2.7 %** of this batch's comment volume (and
`stale` rewordings at `monitor/mod.rs:73-76, :151-152, :194` and `fix_loop.rs:323` change
text but not line count). The overwhelming majority of the 4,599 comment lines is
load-bearing (§2) or at least non-offending.

---

# 4. Source-scanning guard tests

Two repo-wide tests read **source text** and assert on it. They constrain what a comment
cleanup may do, because one of them counts literals inside comments.

## 4.1 `tests/test_tool_name_literals.rs` — exact per-file `"tool_name"` literal counts

Mechanics (file read end-to-end at audit time):

* Module doc, lines **1-13**: states the purpose — a raw `"tool_name"` string literal in
  `src/` bypasses `src/tool_names.rs`, the single owner of tool-name spellings.
* `KNOWN_TOOL_NAMES`, lines **18-49** — **28** names: 19 bare (`read_file`, `write_file`,
  `replace`, `run_command`, `grep_search`, `glob`, `create_plan`, `archive_current_plan`,
  `rebirth`, `pty_spawn`, `pty_write`, `pty_read`, `pty_close`, `pty_list`, `leave_verdict`,
  `sleep`, `reply_to_arbitrator`, `delegate_task`, `list_directory`) plus the 9
  `terminal__`-prefixed variants.
* `BASELINE`, lines **51-83** — **(file, name, count)** rows; 9 rows / 13 literals
  today, with historical notes about t-069 / t-070 at lines **67-82**.
  Rows inside my scope:

  | BASELINE row | count |
  |---|---:|
  | `("src/harness/monitor/monitor_tests.rs", "glob", 1)` | 1 |
  | `("src/harness/monitor/monitor_tests.rs", "read_file", 2)` | 2 |
  | `("src/harness/monitor/monitor_tests.rs", "write_file", 1)` | 1 |
  | `("src/harness/monitor/xml.rs", "read_file", 4)` | 4 |
  | `("src/harness/monitor/xml.rs", "write_file", 1)` | 1 |

  Out-of-scope rows: `src/config.rs` `delegate_task` 2; `src/debug_log.rs` `delegate_task` 1;
  `src/orchestrator/steer_tests.rs` `delegate_task` 1; `src/types/wire.rs` `read_file` 1.
* `count_literal`, lines **85-88** — `content.matches("\"name\"").count()` over the **raw
  file text**. ⇒ **There is no comment stripper: doc comments, block comments and inline
  comments are counted exactly like code.**
* `walk_src`, lines **90-104** — recursive `read_dir` rooted at `src/`, keeps `*.rs`,
  skips only the file named `tool_names.rs`; paths are workspace-relative (`src/...`).
* The test, lines **106-136** — asserts the scan found a non-empty file set, then for every
  scanned file × every known name asserts `actual == expected`; mismatches are collected and
  reported as `file: "name" literal count = actual, expected (baseline)`.

Verification: I re-implemented `walk_src` + `count_literal` + the assertion in Python and
ran it over the tree — **102 `src/**/*.rs` files scanned, 0 violations; the BASELINE is
currently green.**

## 4.2 The in-scope literals that make comments load-bearing — **do not remove or paraphrase**

Every one of the 5 `xml.rs` literals lives **inside a comment**:

```
src/harness/monitor/xml.rs:16   — "read_file"  — doc comment: XML tool-call encoding example (module doc block, lines 15-20)
src/harness/monitor/xml.rs:18   — "read_file"  — doc comment: second supported encoding example (same block)
src/harness/monitor/xml.rs:219  — "read_file"  — inline comment above the Pattern-2 matcher
src/harness/monitor/xml.rs:379  — "read_file"  — doc comment: `/// Read the value of an attribute like function="read_file"`
src/harness/monitor/xml.rs:220  — "write_file" — inline comment `// or <tool_call function="write_file" path="foo.md">…`
```

Consequences for the cleanup:

* Deleting, shortening or rewording `xml.rs:15-20`, `:219-220` or `:379` drops the count
  from 4 → n and **fails** `test_no_unexpected_raw_tool_name_literals_in_src` — unless
  `BASELINE` is edited in the same commit, which is the worse outcome (it silently
  loosens the guard).
* The `monitor_tests.rs` literals (`"read_file"` at **20** and **316**, `"write_file"` at
  **35**, `"glob"` at **60**) are inside `r#"…"#` XML wire fixtures — code, not comments —
  so they are untouched by a comment pass, but they must not be "cleaned up" either.
* Grep over the whole scoped batch: **no other in-scope file contains a quoted tool name.**
  `src/mcp/mod.rs:11, :233, :259` and the `monitor_tests.rs` fixtures mention
  `terminal__run_command` in **backticks / unquoted prose**, which the guard does not see.
  ⇒ Converting such prose into a quoted literal would break the guard; keep them unquoted.
* Net rule for this batch: `src/harness/monitor/xml.rs` comments 16, 18, 219, 220, 379 are
  **exempt from every category in §1** (none of them is listed as removable above), and the
  §1.2 removal of `xml.rs:432`/`:434` is safe because those lines contain no literals.

## 4.3 `tests/test_role_gating.rs::fix_loop_source_has_no_raw_tool_name_string_literals`

Lines **1423-1484** of `tests/test_role_gating.rs`.

* Doc, **1423-1426**: "Stricter than the repo-wide guard: it flags a tool name *inside* a
  longer literal too."
* Reads `src/agents/runner/fix_loop.rs` (**1429-1430**).
* Needles are **built at runtime** from `marmennill::tool_names::TOOL_*` constants
  (**1434-1454**, 19 entries) plus `terminal_tool(name)` (**1456-1458**) — with the
  comment at **1432-1433** explaining that this is "so this test's own text can never
  satisfy them". (This is exactly why the needles in the in-source guards below are also
  assembled from fragments.)
* **1460** calls `string_literals_in_source(&src)`; **1466-1470** require that no extracted
  literal contains any needle; **1480-1483** additionally asserts `src.contains("tool_names::")`.
* Helper `string_literals_in_source`, lines **1488-1560**: a character scanner that tracks
  string state and **skips `//` comments (1510-1515, including `///`), nested `/* */`
  blocks, char literals and lifetimes**.

Verification: Python replica over `fix_loop.rs` → **271 string literals, 0 offenders,
`tool_names::` present; currently green.**

**Critical asymmetry between the two guards** (this is the trap for a comment sweep):

| Guard | Sees comments? | Effect on cleanup |
|---|---|---|
| `test_tool_name_literals.rs` | **Yes** — raw text `matches()` | A quoted tool name in a comment **counts**; deleting such a comment breaks the count. |
| `test_role_gating.rs::…fix_loop…` | **No** — comments skipped | A quoted tool name in a `fix_loop.rs` comment is invisible here, but **would still be caught** by the repo-wide guard (it counts `"read_file"` etc. verbatim). |

Therefore: never introduce a quoted tool-name spelling into any comment, and never delete
one that the BASELINE depends on. In-scope files where a quoted tool name is currently
present at all: `src/harness/monitor/xml.rs` (5) and `src/harness/monitor/monitor_tests.rs`
(4, all in fixtures). `src/agents/runner/fix_loop.rs` contains **0** — keep it that way when
editing its 499 comment lines (the largest comment budget in the batch).

## 4.4 In-scope self-scanning guards built on `include_str!` (comments are load-bearing)

These live inside the scoped files and scan **their own source**, comments included.

```
src/agents/runner/fix_loop.rs:1910-1938 — switch_is_gone_and_mcp_advertising_goes_through_the_policy_helper
    include_str!("fix_loop.rs") at :1911. Forbids `pub dispatch_verdict_tools: bool` and
    `p.dispatch_verdict_tools` (needles assembled :1915-1921), REQUIRES `allowed_mcp_tools(`
    (:1929 — satisfied only by the code at :581 `for tool in harness::allowed_mcp_tools(mcp_servers)`)
    and forbids `mcp.tools_for_servers(` (:1934). Comments :1913-1914 and :1927-1928 explain
    the needle assembly ⇒ MUST KEEP (whole-line comments are part of the scanned text).

src/agents/validation.rs:640-678 — test_validation_module_enumerates_no_verdict_spelling_of_its_own
    include_str!("validation.rs"); production part = text before `#[cfg(test)]` (:643-646).
    Forbids quoted verdict spellings taken from `tool_spellings_for(TERMINAL_LEAVE_VERDICT)` +
    `LEAVE_VERDICT_NAME_SUFFIXES` (:648-654) — production part includes comments ⇒ MUST KEEP
    doc :635-639; requires `is_leave_verdict_tool_name(` (:671) and the exact line
    `pub fn is_leave_verdict_tool(name: &str) -> bool {` (:675) — that line must not be reflowed.

src/harness/sleep.rs:519-560 — sleep_handlers_retype_no_default_or_clamp_literals
    include_str!("sleep.rs"); production part = before `#[cfg(test)]` (which starts at sleep.rs:128).
    Production text (comments included) must NOT contain `unwrap_or(5`, `clamp(1`, `clamp(300`,
    `300`, `DEFAULT_sleep`, `max_sleep`; requires exactly 2 × `TOOL_SLEEP_duration_secs(` and
    2 × each of `Ok(sleep_cancelled_before_start()`, `Ok(sleep_interrupted()`, `Ok(sleep_completed(`.
    Verified clean today (the `300`s live at :202 and :228, i.e. in the test part).
    ⇒ MUST KEEP doc :513-518 and inline :522-523, :543-544. Note: writing "300" in a sleep.rs
    comment would fail this guard — see the §2/§1 note that sleep.rs has no such line today.

src/harness/pty_tests.rs:888-904 — test_sandbox_gate_source_has_no_executable_name_heuristic
    Reads src/harness/pty.rs. Forbids `is_marmel_binary` and the quoted literals `"marmel"` /
    `"marmel.exe"` **anywhere in the file, comments included**; requires `SANDBOX_OPTOUT_ENV`
    and `should_apply_sandbox`. pty.rs is clean today (its bare `marmel` at :448 and :750 is
    unquoted and therefore fine). ⇒ MUST KEEP doc :886-887; do not add a quoted `"marmel"`
    to a pty.rs comment.

src/harness/sandbox.rs:816-870 — source guards that read `src/main.rs` (:820, :857)
    Doc :816-817 ("must never discard the enforcement result and must never re-spell the
    protocol literal") and :853-854 ("The needle is assembled from parts so this file never
    contains it either") ⇒ MUST KEEP. Rule lines :812/:814 are the §1.2 banners only.
```

## 4.5 Cleanup rules implied by §4 (checklist for whoever executes the removals)

1. Do not touch `src/harness/monitor/xml.rs` lines **15-20, 219, 220, 379** — 5 of the 13
   BASELINE literals live there (all in comments).
2. Do not add any `"tool_name"`-quoted spelling to any comment in the 51 scoped files.
3. When removing a §1.2 banner pair, remove the rule lines only — the **title** line carries
   the section name and, in `sandbox.rs:503-504` / `:550-551` / `pty_tests.rs` etc., is
   referenced by the surrounding prose.
4. Never reflow `src/agents/validation.rs:675` (`pub fn is_leave_verdict_tool(name: &str) -> bool {`)
   — the guard matches that exact line.
5. Keep `src/agents/runner/fix_loop.rs` free of quoted tool names (guard 4.3 + repo-wide
   guard), and keep its `tool_names::` references.
6. Fix the stale numbers in `src/harness/monitor/mod.rs:73-76, :151-152, :194`
   (`1000` → `16384`, `≥3` → `≥5`) rather than deleting the sentences — they carry REQ tags.
7. Re-run after any edit: `cargo test --test test_tool_name_literals` and
   `cargo test --test test_role_gating fix_loop_source_has_no_raw_tool_name_string_literals`,
   plus the in-source guards (`cargo test --lib` in the affected modules).

---

Audit performed read-only; `git status` shows no modification to any file under
`src/agents`, `src/harness`, `src/llm`, `src/net`, `src/mcp`.
