# t-011 — Comment cleanup + Swedish→English in `src/ui/bridge/**` + `src/ui/mod.rs`

**Verdict: APPROVED / complete.** Zero behaviour change, zero Swedish in scope, all gates green.
Scope (from `git ls-files`): `src/ui/bridge/{action,arbiter,drain,mod,sink,steer,tests}.rs` + `src/ui/mod.rs` (8 files).
Nothing else was written: `tests/test_tool_name_literals.rs` (BASELINE single writer = t-014) and
`src/ui/{transcript,helpers,raw,session,session_tests}.rs`, `src/ui/tui/**` (t-026) are untouched.

## 1. Comment deltas vs `git show HEAD:<file>` (whole-line comments)

| file | total lines | comment lines HEAD → WT | removed | retained |
|---|---|---|---:|---:|
| src/ui/bridge/action.rs | 209 | 55 → 55 | 0 | 55 |
| src/ui/bridge/arbiter.rs | 1011 | 218 → 218 | 0 | 218 |
| src/ui/bridge/drain.rs | 280 | 9 → 9 | 0 | 9 |
| src/ui/bridge/mod.rs | 15 | 1 → 1 | 0 | 1 |
| src/ui/bridge/sink.rs | 401 | 23 → 23 | 0 | 23 |
| src/ui/bridge/steer.rs | 233 | 18 → 18 | 0 | 18 |
| src/ui/bridge/tests.rs | 2236 → 2219 | 158 → 141 | 17 | 141 |
| src/ui/mod.rs | 140 → 139 | 42 → 35 | 7 | 35 |
| **total** | | | **24** | **480** |

Removed categories (all located by content anchor):
* **8 decorative separator bars** — the bars above/below the four titled banners in `bridge/tests.rs`
  (`` `arbiter` — the single owner of the steer-arbitration round machine ``, `t-035c …`, `t-076 …`, `t-072 …`).
  Titled t-NNN lines kept. Repo-scope check: `0` separator-only comment lines remain in the 8 files.
* **8 restates-code / narration labels** in `bridge/tests.rs` — e.g. the labels preceding
  `assert_eq!(subagents.len(), 1);`, `assert_eq!(steer_queue.len(), 1);`,
  `Event::Message(m) if m.contains("All files inspected cleanly.")`,
  `Event::Status(s) if s.contains("Instruction queued for next turn")`, the two `let steer_responses: Vec<_> = …`
  builders, the `SteerArbEvent::Finished` send in `…_with_streamed_response`, and the `QueueAndContinue`-with-null-response fixture send.
* **1 duplicate-narrative copy** — `/// Shared abort / user-exit flags (trait-default abort surface).` above
  `input_state: crate::ui::InputState` in `struct TestRenderer` (canonical copy kept on `InputState`, `src/ui/mod.rs`).
* **7 over-explained rustdoc echoes** in `src/ui/mod.rs` — field echoes on `thinking` / `content`,
  variant echoes on `Thinking(String)` / `Delegation(DelegationEvent)`, and the trait-default echoes
  `Set the shared abort flag.` / `Whether an abort was requested.` / `Clear the shared abort flag.`
* **0 commented-out dead code, 0 TODO/FIXME/XXX/HACK, 0 trailing end-of-line comments,
  0 stale numeric line pointers** in the 8 files (the §3 “Trl” counts in the research doc were the guard-code
  line `trimmed.starts_with("//")`, not comments).

Kept in place with the pointer corrected (never deleted):
* `bridge/tests.rs` NOTE inside `test_bridge_sink_host_sleep_duration_is_the_owners_clamped_value`:
  a mis-split pointer `` arbiter_upper_budget_is_not_the_sleep\n_tool_ceiling `` → `` see the source guard
  `arbitrator_upper_budget_is_not_the_sleep_tool_ceiling` in `arbiter.rs` `` (target verified at `bridge/arbiter.rs:774`).
* Typo in the doc of `test_bridge_sink_sleep_site_redeclares_no_default_and_no_clamp`: `Needsles` → `Needles`.
* Grammar-only fix in the doc of `MAX_CHAINED_SLEEP_EXTENSIONS` (`bridge/arbiter.rs`): `a arbitrator` → `an arbitrator`.

Everything that survived is a doc comment, a “why” comment, a safety/UTF-8 or floor-vs-clamp ordering
invariant, a lock/cancellation-discipline note, or a `t-NNN` traceability anchor (t-035c, t-054, t-069, t-070,
t-072, t-076).

## 2. Swedish → English

**No translation was required.** `grep -rnP '[åäöÅÄÖ]' src/ui/bridge src/ui/mod.rs` → no output, exit 1;
a Swedish function-word sweep (`och|med|att|inte|eller|därför|…`, word-boundary, case-insensitive) → exit 1.
`docs/research_swedish_inventory.md` agrees: Swedish exists in `src/ui/transcript.rs` (9) and
`src/ui/helpers.rs` (12) (t-026/t-013 territory), not in t-011 scope. Glossary byte-identity therefore
trivially holds; no Swedish literal was written into any comment.

## 3. Zero code-token change (string-aware proof)

Comment-stripped, string-preserving comparison vs `git show HEAD:<file>`:

* **Line-level code-token diff = 0** for all files except `src/ui/mod.rs` (10 lines = rustfmt reflow of two
  enum variants onto multiple lines).
* **Whitespace-insensitive code stream identical = True** for all files except `src/ui/mod.rs`, where the
  char-level opcode diff is exactly two `insert '' → ','` — rustfmt trailing commas at
  `SubagentMessage { agent_tag: String, text: String }` and `SubagentThinking { … }` (fmt-only churn, sanctioned).
* **String-literal byte identity: 503 literals across the 8 files, 0 changed.**
* UTF-8 data fixtures byte-identical: non-ASCII chars inside code = action 1, arbiter 10, drain 3, bridge/mod 0,
  sink 11, steer 2, tests 6, ui/mod 0 — all unchanged (only the em-dash `—` in user-visible status strings).

## 4. Bridge guards (needle status)

Located by guard-test NAME, not line number:

| guard test | site | needle mechanism | status |
|---|---|---|---|
| `bridge_action_module_retains_no_tool_alias_literals` | `src/ui/bridge/action.rs:110` | `include_str!("action.rs")` production region, comments included | action.rs comments byte-identical to HEAD ⇒ pass |
| `arbitrator_upper_budget_is_not_the_sleep_tool_ceiling` | `src/ui/bridge/arbiter.rs:774` | `include_str!("arbiter.rs")`, comments stripped | required/forbidden identifiers untouched ⇒ pass |
| `test_bridge_sink_sleep_site_redeclares_no_default_and_no_clamp` | `src/ui/bridge/tests.rs:2082` | `include_str!("sink.rs")`, comments stripped, digit-free resolver body | pass |
| `test_no_raw_tool_name_literals_in_bridge_tree` | `src/ui/bridge/tests.rs:2139` | raw `read_to_string` of every `src/ui/bridge` file, **comments included** | no quoted tool name in any in-scope comment ⇒ pass |
| text-pinned wording (`carried to the next seam`, `[Steering Arbitrator sleeping for …s…]`, woke/cancelled, `Instruction queued for next turn`, `\n[Steering Subtask — {} ({}):]\n`) | `bridge/arbiter.rs`, `bridge/sink.rs:175`, `bridge/drain.rs:78` | substring asserts in `bridge/tests.rs` | byte-identical (literal diff 0) ⇒ pass |

**Nothing for t-014 reconciliation:** no pinned needle was changed. The only comment text reworded is the
`bridge/tests.rs` NOTE pointer and the `Needles` typo, which no guard scans. No BASELINE row was touched and
no quoted tool name was added, deleted, reworded or moved in `src/`.

## 5. Gates (real exit codes)

| command | exit | result |
|---|---:|---|
| `cargo fmt --all -- --check` | 0 | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | 0 | 0 diagnostics (also after `touch`ing all 8 in-scope files → forced re-check `Checking marmennill … Finished`, exit 0) |
| `cargo test --lib -- ui::bridge --test-threads=1` | 0 | 38 passed / 0 failed / 774 filtered |
| `cargo test --lib -- ui:: --test-threads=1` | 0 | 147 passed / 0 failed / 665 filtered |
| `cargo test --test test_tool_name_literals` | 0 | 1 passed |
| `cargo test --lib -- --test-threads=1` (reference) | 101 | 807 passed / 5 failed = exactly the sanctioned set: 4× `harness::pty::tests::*` (`/dev/pts/ptmx` EACCES) + `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable`; `orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup` green in single-thread mode |
| `grep -rnP '[åäöÅÄÖ]' src/ui/bridge src/ui/mod.rs` | 1 | no matches |

Zero failures anywhere in `ui::bridge/**` or `src/ui/mod.rs`.
