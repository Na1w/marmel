# Research: git baseline + automated-test surface for the two defects

Read-only research pass. No source file was modified, nothing was committed, no branch was created.
Workspace root: `/home/fredrik/marmel/wip` (package `marmennill`, binary `marmel`).

---

## 1. Git baseline

| Item | Value |
|---|---|
| Branch checked out | `main` (`git branch --show-current`) |
| HEAD sha | `de3e99c89b18bf43fbb471930bd9f7a40e1ca1b2` |
| HEAD subject | `Fix/subagent panel refresh (#25)` |
| Default / base branch | `main` — `origin/HEAD -> origin/main`; HEAD is exactly `origin/main` (nothing ahead/behind) |
| Tracked tree | **Clean.** `git status --porcelain --untracked-files=no` → empty |
| Untracked | 17 `marmel` state dirs only: `.marmel.old/`, `.marmel.old2/`, `.marmel.old.3/4/5/6/8/9/10/11/12/13/14/16/17/18/19/` (all `.marmel.old*`) — no source-file drift |
| Ignored-but-present | `.marmel/` in CWD (`execution_plan.md`, `marmel.log`, `prompts/`, `session/`) — see §5, it is the known flakiness trigger |
| Remote | `origin https://github.com/Na1w/marmel.git` (fetch+push) |
| Tracked file count | 184 |

Recent history (`git log --oneline -15`, newest first):

```
de3e99c (HEAD -> main, origin/main, origin/HEAD) Fix/subagent panel refresh (#25)
fd301a0 Fix multi-line paste being sent as one steering command per line (#24)
43ba4fa Conditional stream pause during steering arbitration + Paused state in subagent panel (#23)
d66fdde fix(ui): apply terminal math formatting only to genuine math (#22)
4d94274 Update dependencies (#21)
94f328b fix test
29039e6 Make the TUI subagent pane a projection of the worker registry (#19)
4dfe690 fix: treat an absent validation brief as a design opt-out, not a task failure (#18)
b08923e Landlock improvements (#17)
62cb756 fix: report missing TLS trust store instead of panicking (#16)
981b35b feat: limit concurrent LLM sessions per backend/model (#15)
3f238ff feat: add per-role reasoning_effort configuration (#12)
5c2231b clean up (#11)
a33f329 Refactor (#10)
31f4e90 (refactor) fix(ui): disable session rehydration by default and prevent exit on user steering abort
```

Branch relatedness screen (48 local+remote branches; none is an obvious carrier of either defect):

* `fix/subagent-panel-refresh` = HEAD (PR #25, already merged).
* `fix/paste-multiline-steering`, `feat/conditional-stream-pause-on-steering`, `fix/subagent-pane-registry-projection`, `dependency-bump`, `landlock-improvements`, `concurrency`, `reasoning-effort` — all already merged into `main`.
* **`validation-directive` == `fixes` == `c24d6b0`** — its single commit (`fix: treat an absent validation brief as a design opt-out, not a task failure`) is already on `main` as `4dfe690 (#18)`. This is the branch that produced today's `(no-validate)` / skip-record behaviour, so it is the closest *historical* relative of defect (b), but it is not a live fix branch.
* **`validator-ui-bug`** — local-only (no remote counterpart), sits at `ab7ba2f "temp"` on top of `b08923e` (6 commits behind `main`). Name is suggestive of validation work but the commit is a scratch commit; not usable as a base.
* Others (`0.7.0`, `1.0.0`, `1.0.0-refactor`, `ca-panic`, `chat-panel-improvement`, `cleanup`, `marmel-0.5/0.6`, `marmel-refactor`, `mcp-integration`, `refactor`, `sandbox`, `research-mcp-experiment`) — stale/unrelated.

Recommendation for implementers: branch from `main` at `de3e99c`. Nothing must be assumed from `validator-ui-bug`.

**Caveat — `docs/` does not exist in this checkout.** `git ls-files | grep '^docs'` returns nothing and `ls -d docs` fails, yet `Cargo.toml`, `scripts/cross_platform_check.sh` and many `src/**` doc-comments reference `docs/cross_platform_gating.md`, `docs/concurrency_design_decisions.md`, `docs/fix_verification.md`, `docs/research/logs_fix_spec.md`. Those referenced design docs are **not available** here; do not treat their claims as verifiable. This file itself (`docs/research_test_surface.md`) creates a new untracked directory and is **not** gitignored (`git check-ignore` exit 1), so it will show up as `?? docs/` in `git status`.

---

## 2. Build / test topology and canonical commands

From `Cargo.toml`:

* `[package] name = "marmennill"`, `version = "1.0.0"`, `edition = "2024"`, `rust-version = "1.98"`, `default-run = "marmel"`.
* `[[bin]] name = "marmel" path = "src/main.rs"`; the **lib target `marmennill` is auto-detected** from `src/lib.rs` (no explicit `[lib]`). Integration tests import the crate as `marmennill::…`.
* **No `[[test]]` sections** → every `tests/*.rs` file is its own integration test binary whose target name is the file stem (`--test test_validation_loop`, etc.). `tests/common/mod.rs` is a shared helper module, **not** a test target.
* `[dev-dependencies]`: `tempfile 3.27`, `wiremock 0.6` (all LLM-facing tests are offline against wiremock).
* Lints are hard: `[lints.rust] unused/dead_code/unused_imports = "deny"` and `[lints.clippy] all = "deny"`. New test code with an unused import breaks `cargo test --all-targets`.

Canonical commands found in the repo:

| Source | Command |
|---|---|
| `README.md:563` (§Testing) | `cargo test` |
| `README.md:582` | `cargo clippy --all-targets --all-features`, plus `cargo fmt -- --check` |
| `README.md:95` | `cargo build --release` |
| `scripts/cross_platform_check.sh:362-366` (CI-equivalent host legs) | `cargo fmt --all -- --check`; `cargo clippy --all-targets --all-features -- -D warnings`; `cargo test --all-targets --all-features --no-fail-fast -- --test-threads=1 "${SKIPS[@]}"`; `cargo test --doc --all-features` |

Target split (verified by listing, not execution):

* **lib unit tests** (`target/debug/deps/marmennill-…`): **1188 tests**. Includes the `src/**/*_tests.rs` modules wired in via `#[cfg(test)] mod …` and the inline `#[cfg(test)] mod tests` modules. Module paths are the wiring names, e.g. `orchestrator::notice_tests::*` (`src/orchestrator/mod.rs:18-19`), `harness::dispatch_integrity_tests::*` (`src/harness/mod.rs:15-17`, via `#[path = "dispatch_integrity_tests.rs"]`), `harness::tests::*` (`src/harness/mod.rs:1076`), `orchestrator::tests::*` (`src/orchestrator/mod.rs:523`), `agents::prompt_builder::tests::*` (`src/agents/prompt_builder.rs:987`), `agents::runner::execution::tests::*` (`src/agents/runner/execution.rs:1231`), `agents::runner::fix_loop::tests::*` (`fix_loop.rs:1392`), `agents::validation::tests::*` (`src/agents/validation.rs:743`), `orchestrator::delegate::tests::*` (`delegate.rs:341`), `plan_parse::tests::*` (`src/plan_parse.rs:884`), `ui::tests::*` (`src/ui/mod.rs:236-238`, `#[path = "session_tests.rs"]`), `ui::bridge::tests::*` (`src/ui/bridge/mod.rs:9-10`).
* **integration tests**: one binary per `tests/*.rs` (38 files + `common/`). Relevant ones and their verified test counts: `test_ui_session` 21, `test_validation_loop` 19, `test_dynamic_prompts` 10, `test_steer_history` 9, `test_validator_concurrency` 8, `test_plan_checkoff` 8, `test_delegation_queue_events` 6, `test_midturn_notice` 3, `test_role_gating` 25.

Toolchain confirmation (a single quick build, as permitted):

```
$ cargo --version && rustc --version
cargo 1.99.1 (5f94df478 2026-08-27)
rustc 1.99.1 (58cf8f9c3 2026-10-13)

$ time cargo build --lib --offline
   Compiling marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 28.89s   # exit 0, no warnings
```

I additionally compiled (never ran) test binaries to verify target names and test-name spellings used in §3:
`cargo test --lib --offline -- --list` (1188 tests), and `-- --list` for the targets
`test_steer_history`, `test_validation_loop`, `test_dynamic_prompts`, `test_validator_concurrency`,
`test_plan_checkoff`, `test_midturn_notice`, `test_delegation_queue_events`, `test_ui_session`, `test_role_gating`.
All commands in §3 are therefore syntactically verified to resolve (they can be pasted as-is).

Argument-passing gotcha: libtest flags and name filters go **after the second `--`**, and multiple substring filters are supported (verified):
`cargo test --lib -- --test-threads=1 orchestrator::notice_tests` and
`cargo test --lib -- orchestrator::notice_tests::test_all_agent_archetypes harness::dispatch_integrity_tests::test_reply` both work.

---

## 3. Defect → test map

### Defect A — `reply_to_arbitrator` round trip (notice posted → worker replies → arbitrator answers)

Production anchors (read-only observation, for context on what each test pins):
`src/harness/mod.rs:589` (Manager caller), `:699` (Specialist caller), `:738 handle_reply_to_arbitrator_async`, `:869 handle_reply_to_arbitrator` (sync wrapper);
`src/orchestrator/notice.rs:430 TEST_NOTICE_MUTEX`, `:779 NoticeReplyRejection`, `:856 notice_addresses_worker`, `:898 record_worker_reply_for_notice`, `:1042 render_notice_for_worker`, `:1151 evaluate_worker_reply`;
`src/orchestrator/registry.rs:108-111` (`SpecialistEntry::allows` short-circuits `reply_to_arbitrator` to `true`);
`src/orchestrator/mod.rs` re-exports (asserted by `orchestrator::tests::test_notice_api_is_reachable_through_the_crate_level_path`).

| Test file :: test (line) | What it pins | Exact `cargo test` invocation |
|---|---|---|
| `src/orchestrator/notice_tests.rs:379 :: test_handle_reply_to_arbitrator_success_and_validation` | Full dispatch round trip: missing `notice_id` → error, missing `message` → error, valid reply → `recorded successfully`, notice consumed, reply recorded, `Event::SteerResponse` emitted, steering history entry rewritten | `cargo test --lib -- orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation` |
| `…notice_tests.rs:484 :: test_handle_reply_to_arbitrator_ask_followup` | `AskFollowUp` branch: follow-up re-posted to the same worker under the same dialogue, history keeps `awaiting specialist reply` | `cargo test --lib -- orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup` |
| `…notice_tests.rs:866 :: test_all_agent_archetypes_allow_reply_to_arbitrator` | Every archetype (Coder/Researcher/Debugger/Validator/Generalist/Planner) is allowed the tool via `caller_allows_tool` | `cargo test --lib -- orchestrator::notice_tests::test_all_agent_archetypes_allow_reply_to_arbitrator` |
| `…notice_tests.rs:891 :: test_assemble_tools_includes_reply_to_arbitrator_unconditionally` | `agents::runner::fix_loop::assemble_tools` advertises the tool even for a restricted `allowed_tools` list | `cargo test --lib -- orchestrator::notice_tests::test_assemble_tools_includes_reply_to_arbitrator_unconditionally` |
| `…notice_tests.rs:1562 :: test_follow_up_repost_of_same_notice_id_is_answerable_again` (doc comment at **1557** describing the harness re-post) | Re-posting the same `notice_id` makes it answerable again; a second reply to a resolved id stays `already-replied` | `cargo test --lib -- orchestrator::notice_tests::test_follow_up_repost_of_same_notice_id_is_answerable_again` |
| `…notice_tests.rs:347, 1346, 1383, 1468, 1524, 1610, 1683, 1716` (`test_record_worker_reply_matches_pending_notice`, `test_reply_with_explicit_id_resolves_exactly_that_notice`, `test_reply_naming_another_workers_notice_id_is_rejected`, `test_reply_without_explicit_id_is_rejected_and_logged`, `test_duplicate_reply_for_resolved_notice_is_rejected`, `test_reply_identity_matches_the_same_routing_rules`, `test_reply_retires_the_queued_copy_of_the_resolved_notice`, `test_rendered_notice_surfaces_the_id_the_reply_must_name`) | Store-level reply contract: exact-id resolution, cross-worker rejection, no implicit guessing, duplicate rejection, identity tolerance, inbox retirement, rendered id | `cargo test --lib -- --test-threads=1 orchestrator::notice_tests` (whole module, 44 tests) |
| `…notice_tests.rs:572 :: test_steering_history_multiple_notices_resolved_out_of_order` | Out-of-order replies update the right history entry | `cargo test --lib -- orchestrator::notice_tests::test_steering_history_multiple_notices_resolved_out_of_order` |
| `…notice_tests.rs:696, 746, 826, 810` (`test_evaluate_worker_reply_synthesize_response`, `…_ask_followup`, `…_fallback_on_plain_text`, `test_worker_reply_prompt_user_status_example_is_pending_and_language_neutral`) | Arbitrator evaluation of the worker reply (decision JSON + plain-text fallback), prompt example wording stays "pending" | `cargo test --lib -- orchestrator::notice_tests::test_evaluate_worker_reply orchestrator::notice_tests::test_worker_reply_prompt_user_status_example` |
| `…notice_tests.rs:1275, 1315` (`test_mid_turn_drain_routes_exactly_and_is_idempotent`, `test_mid_turn_drain_drops_expired_notice_with_report`) | Notice reaches the worker inside the in-flight turn | `cargo test --lib -- --test-threads=1 orchestrator::notice_tests::test_mid_turn_drain` |
| `src/harness/dispatch_integrity_tests.rs:78, 125, 176, 226` (`test_reply_naming_another_workers_notice_is_rejected_and_stays_pending`, `test_reply_without_notice_id_is_rejected_and_no_notice_is_guessed`, `test_reply_with_explicit_notice_id_resolves_and_records_the_reply`, `test_reply_to_an_unknown_notice_id_is_rejected_without_state_change`) | **Dispatcher** layer: error text names the rejection kind (`worker-mismatch`, `unknown-notice-id`), notice stays pending, no UI event on rejection, correctly-targeted reply resolves | `cargo test --lib -- --test-threads=1 harness::dispatch_integrity_tests::test_reply` (4 tests) |
| `src/harness/dispatch_integrity_tests.rs:293, 301, 314, 324, 346` (verdict-identity set) | Neighbouring integrity rule in the same target | `cargo test --lib -- harness::dispatch_integrity_tests` |
| `tests/test_steer_history.rs:79, 112, 139, 165, 197, 240, 255, 280, 306` (9 plain `#[test]`s) | Steering-history correlation is identity-keyed, never substring: `t-1`≠`t-10`, `notice-1`≠`notice-10`, unknown id appends instead of hijacking, `bus_correlation_keeps_no_substring_fallback` | `cargo test --test test_steer_history -- --test-threads=1` |
| `tests/test_ui_session.rs:736 :: test_steering_conversation_history_accumulates_worker_reply_to_arbitrator` (region 736–831) | End-to-end: arbitrator forwards a notice, history shows `awaiting specialist reply` + the generated `notice-…`, then `marmennill::harness::handle_reply_to_arbitrator_async("coder", …)` updates the same entry and the evaluation prompt is what the mock keys on | `cargo test --test test_ui_session -- --test-threads=1 test_steering_conversation_history_accumulates_worker_reply_to_arbitrator` |
| `tests/test_ui_session.rs:631 :: test_steering_conversation_history_accumulates_and_passes_to_arbitrator` | Accumulated steer conversation is replayed into the next arbitrator request (no worker reply involved) | `cargo test --test test_ui_session -- --test-threads=1 test_steering_conversation_history_accumulates_and_passes_to_arbitrator` |
| `tests/test_midturn_notice.rs:97, 199, 284` | Mid-turn drain against `run_specialist_live`: same-turn injection, injection before a verdict turn can conclude, other-worker notices not injected | `cargo test --test test_midturn_notice -- --test-threads=1` |
| `tests/test_delegation_queue_events.rs:215, 327, 537, 573, 675, 746` | Adjacent plumbing only (Queued/Running edges on the delegation stream, `pool::CallContext` attribution). Run it when the fix touches bus/pool/sink code; it does **not** cover replies | `cargo test --test test_delegation_queue_events -- --test-threads=1` |
| `src/orchestrator/steer_tests.rs:20-243, 538-794` | Arbitration decision vocabulary, `extract_tasks_to_delegate`, subtask action spellings (uses `TOOL_REPLY_TO_ARBITRATOR` literals at 557/605) | `cargo test --lib -- orchestrator::steer_tests` |
| `src/ui/bridge/tests.rs:40-2413` (`test_drain_steer_*`, `test_arbiter_*`, `test_bridge_notice_capacity_drop_is_observable_and_inbox_stays_bounded` at 2177) | Drain side of the arbitration loop + notice capacity drop observability | `cargo test --lib -- ui::bridge::tests` |
| `src/orchestrator/tests.rs:851, 890` (`test_notice_api_is_reachable_through_the_crate_level_path`, `test_notice_transcript_is_rendered_in_exactly_one_place`) | Public re-export surface + single rendering site (breaks if a second renderer is added) | `cargo test --lib -- orchestrator::tests::test_notice` |

One-liners for a full Defect-A sweep:

```bash
# lib layer (notice store + dispatcher + steer/bridge), serialized
cargo test --lib --all-features -- --test-threads=1 \
    orchestrator::notice_tests harness::dispatch_integrity_tests \
    orchestrator::steer_tests ui::bridge::tests orchestrator::tests::test_notice
# integration layer
cargo test --all-features --no-fail-fast -- --test-threads=1 \
    --test test_steer_history --test test_midturn_notice --test test_ui_session
```

Note (verified): `--test` may be repeated, and the trailing filter list applies per target.

### Defect B — ad-hoc task should default to *no validation*

Production anchors: the decision site is `src/agents/runner/execution.rs:376-391` — inside `validation_probe_engaged(...)` the `clean_task_id.as_deref()` **`None` arm** produces a verdict gap: `"the run carries no task id, so no \`-validation.md\` verdict file could be located under …"`. Ad-hoc steering subtasks never take that arm: `src/orchestrator/steer.rs:672-680` and `:705-710` invent synthetic ids (`steer-task-1`, or the `tool_call_id`), and such ids have no plan line, so `PromptBuilder::pregenerate_for_plan_offline` writes neither `<tid>-validation.md` nor `<tid>-validation-skip.md` → the on-disk probe reports `Absent` with no skip record → fail-closed gap (`execution.rs` + `fix_loop.rs:459 validation_skip_verdict` + `validation.rs:134 probe_validation_brief` / `:164 validation_brief_absent`). Config knobs in play: `enable_validator`, `max_validator_iterations` (`src/config.rs:230,235`).

| Test file :: test (line) | What it pins | Exact `cargo test` invocation |
|---|---|---|
| `tests/test_validation_loop.rs:1802 :: test_a_no_validate_coder_line_authors_no_brief_and_never_reaches_the_validator` | `(no-validate)` plan line → no brief, validator never invoked | `cargo test --test test_validation_loop -- --test-threads=1 test_a_no_validate_coder_line_authors_no_brief_and_never_reaches_the_validator` |
| `tests/test_validation_loop.rs:1175 :: test_missing_validation_verdict_file_fails_and_leaves_plan_unchecked` | Today's **fail-closed** default when a verdict file is missing (the behaviour an ad-hoc default must not silently regress) | `cargo test --test test_validation_loop -- --test-threads=1 test_missing_validation_verdict_file_fails_and_leaves_plan_unchecked` |
| `tests/test_validation_loop.rs:1263, 1372, 1458 :: test_validation_skip_record_completes_the_run_and_checks_the_plan_line`, `test_blank_validation_skip_record_is_not_a_decision_and_fails_closed`, `test_failed_validation_brief_write_is_not_reported_as_a_skip` | Skip-record semantics end to end, incl. blank record | `cargo test --test test_validation_loop -- --test-threads=1 test_validation_skip_record_completes_the_run_and_checks_the_plan_line test_blank_validation_skip_record_is_not_a_decision_and_fails_closed test_failed_validation_brief_write_is_not_reported_as_a_skip` |
| `tests/test_validation_loop.rs:59, 201, 281, 387, 503, 594, 691, 788, 885` | Rejection/revision loop, chatter loop termination, repetition nudge, marker-less nudge, validator reminded 3× then fail-closed, `leave_verdict` stops the loop | `cargo test --test test_validation_loop -- --test-threads=1` (19 tests) |
| `tests/test_validation_loop.rs:1560, 1625, 1691, 1751, 1908` | Present-verdict approval/rejection, unreadable/blank verdict hard failure, `(validate)` researcher line drives a verdict round | `cargo test --test test_validation_loop -- --test-threads=1 test_present_validation_verdict test_validation_verdict test_a_validate_researcher_line` |
| `tests/test_dynamic_prompts.rs:31, 129, 171, 289, 361, 410, 490` | Prompt-pair pre-generation on disk, `(no-validate)` → `-validation-skip.md`, replanning drops the stale opposite record, directive-order inertness, directive inside description is not a directive, directive before task id authors nothing, progress summary never echoes the directive | `cargo test --test test_dynamic_prompts -- --test-threads=1` (10 tests) |
| `src/agents/prompt_builder.rs:1104, 1227, 1284, 1327, 1356, 1390, 1455, 1527, 1563 :: test_pregenerate_records_the_no_validation_decision`, `test_validation_skip_record_lookup_and_stale_cleanup`, `test_replan_to_no_validate_drops_the_stale_validation_brief`, `test_auto_directive_yields_neither_a_stale_record_nor_a_missing_brief`, `test_no_validate_writes_no_record_for_a_rejected_task_id`, `test_failed_validation_brief_write_leaves_no_skip_record_and_warns`, `test_validation_directive_is_the_owner_not_the_wording`, `test_parse_tasks_from_plan_carries_the_validation_directive` | **Write side** of the directive → brief / skip-record decision (`PromptBuilder::validation_write_decision`, `src/agents/prompt_builder.rs:674-700`) | `cargo test --lib -- agents::prompt_builder::tests` (18 tests) |
| `src/agents/runner/execution.rs:1245, 1290-ish :: hard_opt_out_roles_are_never_validation_eligible_under_any_record_combination`, `a_skip_record_only_ever_takes_validation_off_for_an_eligible_role` | The three predicates `validation_configured_for_role` / `validation_probe_engaged` / `validation_verdict_required` (`:32, :42, :61`) and their precedence | `cargo test --lib -- agents::runner::execution::tests` (2 tests) |
| `src/agents/runner/fix_loop.rs:2638, 2676, 2721 :: absent_brief_plus_skip_record_ends_the_pass_not_rejected`, `blank_or_unreadable_brief_is_never_rescued_by_a_skip_record`, `absent_brief_without_record_or_with_a_refused_id_fails_closed` | Collapse-site guard `validation_skip_verdict` (`:459`) driven through `harness::with_workspace_root` | `cargo test --lib -- --test-threads=1 agents::runner::fix_loop::tests::absent_brief agents::runner::fix_loop::tests::blank_or_unreadable_brief` |
| `src/agents/runner/fix_loop.rs:… :: no_verdict_after_reminders_fails_closed_instead_of_approving`, `verdict_call_from_a_role_without_verdict_authority_fails_closed`, `iteration_cap_stops_the_run_with_an_explicit_failed_outcome` | Fail-closed collapse and verdict authority inside the fix loop | `cargo test --lib -- --test-threads=1 agents::runner::fix_loop::tests` |
| `src/agents/validation.rs:… :: missing_validation_verdict_file_is_not_an_approval`, `unreadable_or_blank_validation_verdict_file_is_not_an_approval`, `plan_designated_skip_record_is_not_a_rejection`, `blank_skip_record_keeps_the_fail_closed_verdict_gap`, `blank_or_unreadable_verdict_file_is_not_rescued_by_a_skip_record`, `hostile_task_id_cannot_reach_a_verdict_file_outside_the_prompts_dir` | **Read side**: the single probe `probe_validation_brief` (`:134`) and `validation_brief_absent` (`:164`) | `cargo test --lib -- agents::validation::tests` (18 tests) |
| `src/plan_parse.rs:884 mod tests :: directive_lookup_is_a_closed_and_fail_closed_set`, `directive_is_stripped_from_the_body_but_raw_and_description_stay_verbatim`, `a_directive_before_the_id_never_becomes_a_task_id`, `check_off_round_trip_keeps_directive_and_role_groups_byte_identical`, … | Plan-line grammar owner of `(validate)` / `(no-validate)` (`ValidationDirective`, `src/plan_parse.rs:68-104`, `split_validation_directive:780`) | `cargo test --lib -- plan_parse::tests` |
| `src/orchestrator/delegate.rs:381, 462 :: brief_for_task_never_carries_the_reserved_validation_directive`, `strip_reserved_validation_tokens_touches_only_the_reserved_vocabulary` | Brief hygiene: reserved tokens stripped from delegated briefs, prose parentheses untouched | `cargo test --lib -- orchestrator::delegate::tests` |
| `tests/test_plan_checkoff.rs:54, 113, 178, 261, 404, 475, 543, 620` | Check-off boundary: marker gating, wrong-task marker, parse errors surfaced, `a_directive_before_the_task_id_owns_no_box_and_fails_closed`, CRLF plan with directive | `cargo test --test test_plan_checkoff -- --test-threads=1` |
| `src/orchestrator/tests.rs:186 :: test_orchestr_apply_checkoff_only_checks_on_genuine_complete` (stale/revoked-marker block at **204–217**), plus `:144`, `:222`, `:76`, `:98`, `:118` | `apply_check_off`: a `Complete` marker whose **content** lost `MISSION COMPLETE` (revoked/stale) must not tick the box; `Failed`/replan leave it unchecked | `cargo test --lib -- orchestrator::tests::test_orchestr_apply_checkoff orchestrator::tests::test_orchestr_task_checkoff` |
| `tests/test_validator_concurrency.rs:316, 350, 443, 511, 556, 626, 689, 816` | Validator/fix-loop/audit turns are pooled against the validator pair's cap with `CallClass::Validator`; no slot held across the tool round; `max_sessions=1` does not self-deadlock. 4 of them use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` → timing-sensitive | `cargo test --test test_validator_concurrency -- --test-threads=1` |
| `tests/test_role_gating.rs:380, 419, 447, 516, 674, 775, 970` | Verdict authority (`leave_verdict` is validator-only through dispatch), self-approval refused, blueprint naming the tool grants nothing | `cargo test --test test_role_gating -- --test-threads=1 leave_verdict validator_may_record an_auditor worker_self_approval repeated_self_verdict` |
| `src/orchestrator/steer_tests.rs:255 :: test_execute_steer_subtask_runs_specialist` + `:175, 206, 228, 243, 767, 794` | Where an ad-hoc task id is invented (`steer-task-N`) — the entry point of defect (b) | `cargo test --lib -- orchestrator::steer_tests` |

One-liners for a full Defect-B sweep:

```bash
cargo test --lib --all-features -- --test-threads=1 \
    agents::prompt_builder::tests agents::runner::execution::tests \
    agents::runner::fix_loop::tests agents::validation::tests \
    plan_parse::tests orchestrator::delegate::tests \
    orchestrator::tests::test_orchestr_apply_checkoff orchestrator::steer_tests
cargo test --all-features --no-fail-fast -- --test-threads=1 \
    --test test_validation_loop --test test_dynamic_prompts \
    --test test_plan_checkoff --test test_validator_concurrency
```

---

## 4. Where new regression tests belong

### (a) `reply_to_arbitrator` round trip

Add at the layer that owns the broken behaviour — the repo already has a clean three-layer split:

1. **Notice-store contract** → `src/orchestrator/notice_tests.rs` (module path `orchestrator::notice_tests`).
   Pattern to copy: `test_handle_reply_to_arbitrator_success_and_validation` (line 379) for dispatch-level round trips, and `test_duplicate_reply_for_resolved_notice_is_rejected` (1524) for pure store assertions on `record_worker_reply_for_notice` (`src/orchestrator/notice.rs:898`).
   Mandatory hygiene in this file: take `TEST_NOTICE_MUTEX` (`notice.rs:430`) via `let _lock = TEST_NOTICE_MUTEX.lock().await;`, call `clear_all_notices()` first, use unique `notice-…`/`…-t-NNNN` keys, assert per-notice-id only (never global counters), and restore the active config (the file uses `crate::config::set_active`; `dispatch_integrity_tests.rs` has the better pattern: `ActiveConfigGuard`, lines 33-57). Use the file-local `sse_delta(…)` helper for the wiremock arbitrator frame.
2. **Dispatcher integrity** → `src/harness/dispatch_integrity_tests.rs` (`harness::dispatch_integrity_tests`), lines 72-262. This is where a *rejected* reply must be asserted: error string names the rejection kind (`worker-mismatch` / `unknown-notice-id` / `already-replied` — `NoticeReplyRejection::kind()`), the notice stays pending, no reply recorded, and **no UI event is emitted** on rejection.
3. **End-to-end bus / UI bridge** →
   * correlation: `tests/test_steer_history.rs` (pure, fast, `#[test]`, must run `--test-threads=1`);
   * full arbitration conversation: `tests/test_ui_session.rs` next to line 736 (it drives `marmennill::harness::handle_reply_to_arbitrator_async` with the id scraped out of the recorded history text — the most faithful round-trip harness available);
   * mid-turn delivery: `tests/test_midturn_notice.rs`.
4. Do **not** add the regression only under `tests/test_delegation_queue_events.rs` — that target covers pool queue edges, not replies.

Cross-cutting guard: `tests/test_tool_name_literals.rs::test_no_unexpected_raw_tool_name_literals_in_src` walks **`src/**/*.rs` only** (excluding `src/tool_names.rs`) and compares per-file counts of raw literals such as `"reply_to_arbitrator"` against a maintained baseline table. Therefore any new test *inside `src/`* must use `crate::tool_names::TOOL_REPLY_TO_ARBITRATOR`; a raw literal there fails that guard. Files under `tests/` are not scanned.

### (b) ad-hoc task → no-validate default

* **Unit, decision predicate (cheapest, most durable):** `src/agents/runner/execution.rs`, inline `#[cfg(test)] mod tests` (line 1231). The three helpers `validation_configured_for_role` (`:32`), `validation_probe_engaged` (`:42`), `validation_verdict_required` (`:61`) are the whole precedence rule; the current gap is produced by the `clean_task_id == None` arm at `:384-391`, so an ad-hoc/absent-id (or synthetic `steer-task-*`) default belongs as a predicate test here first.
* **Collapse-site guard:** `src/agents/runner/fix_loop.rs` `mod tests` (line 1392; guard tests at 2596-2760). Copy `guard_verdict(…)` (line ~2601) which wraps `validation_skip_verdict` in `crate::harness::with_workspace_root(root, …)` — that is the sanctioned way to point the probes at a scratch `.marmel/prompts` and keep the repo's real `.marmel/` untouched.
* **Read side:** `src/agents/validation.rs` `mod tests` (line 743) if the probe itself changes (`probe_validation_brief:134`, `validation_brief_absent:164`) — the code comments make "one probe, three read sites" an explicit invariant, so any widening must be asserted at all three sites (`agents/validation.rs`, `runner/execution.rs`, `runner/fix_loop.rs:459`).
* **Write side (if a skip record is now written for ad-hoc runs):** `src/agents/prompt_builder.rs` `mod tests` (line 987) — pattern `test_pregenerate_records_the_no_validation_decision` (1104) / `test_validation_skip_record_lookup_and_stale_cleanup` (1227).
* **Grammar (if a new directive/default token is introduced):** `src/plan_parse.rs` `mod tests` (884) plus `src/agents/prompt_builder.rs:1527/1563` (`test_parse_tasks_from_plan_*`).
* **End-to-end proof (the one a reviewer will want):** `tests/test_validation_loop.rs`. Drive `run_specialist_live` with a temp workspace root and an ad-hoc delegation (no plan line; `task_id: None` or `steer-task-N`), asserting the run completes not-rejected and the plan/check-off posture is honest. Siblings to mirror: `test_a_no_validate_coder_line_authors_no_brief_and_never_reaches_the_validator` (1802), `test_validation_skip_record_completes_the_run_and_checks_the_plan_line` (1263), and — for the behaviour that must **not** regress — `test_missing_validation_verdict_file_fails_and_leaves_plan_unchecked` (1175).
* **Arbitration entry point:** `src/orchestrator/steer_tests.rs:255 test_execute_steer_subtask_runs_specialist` — if the ad-hoc default is implemented in `execute_steer_subtask` (`src/orchestrator/steer.rs:725`) or in `extract_tasks_to_delegate` (`src/orchestrator/steer.rs:652-722`, id synthesis at `:672-680` and `:705-710`), pin it there.
* **If config surface changes** (`enable_validator` / `max_validator_iterations` in `[specialists]`): update `tests/test_reasoning_effort.rs:153` (TOML fixture that parses these keys) and `marmel.toml.example` / `marmel.toml.cloud` / `marmel.toml.df`.
* **Check-off honesty:** `tests/test_plan_checkoff.rs` and `src/orchestrator/tests.rs:186` (stale/revoked marker at 204-217) — an ad-hoc no-validate default must still never tick a box that has no genuine completion marker.

---

## 5. `#[ignore]`d, skipped, flaky, and state-sensitive tests

**`#[ignore]` attributes in the whole workspace: exactly 2**, both in `tests/test_subpane_logs_repro.rs` (unrelated to these two defects, but they run in that target):

| Line | Test | Ignore reason (abridged) |
|---|---|---|
| 327 (`#[ignore]` at 326) | `a_status_line_never_reaches_a_row_keyed_with_a_collision_handle` | deferred follow-up #1 (R4): status router cannot address a `{base}#N` / `-wN` row key; cites `docs/research/logs_fix_spec.md §6.1` (**file absent in this checkout**) |
| 416 (`#[ignore]` at 415) | `a_new_midturn_row_moves_the_selection_onto_a_row_the_panel_has_no_logs_for` | deferred follow-up #3 (R5): detail pane follows `selected_subagent_idx`; cites `docs/research/logs_fix_spec.md §6.3` (absent) |

So `cargo test` reports them as `ignored`; `cargo test --test test_subpane_logs_repro` runs 4 of 6.

**CI-mandated skips** — `scripts/cross_platform_check.sh:88-95` (`SKIPS` array; rationale block at lines 66-87) applied at line 364-365:

```
--skip harness::pty::tests::test_harness_pty_process_group_kill
--skip harness::pty::tests::test_interactive_pty_manager_lifecycle
--skip harness::pty::tests::test_run_command_parses_timeout_seconds_argument
--skip harness::pty::tests::test_run_command_pty_cancelled_by_worker_token
--skip harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable
--skip test_prompt_based_tool_gating
```

* The first four are `PTY-BLOCKED` (`openpty` fails on the CI host), not defects. On **this** host `/dev/ptmx` (`crw-rw-rw- 5,2`) and a `/dev/pts` slave do exist, so they may pass — but a plain `cargo test` run here will attempt them.
* `test_prompt_based_tool_gating` lives in `tests/test_role_gating.rs:294` (PTY path reached via `dispatch_for(run_command …)`); it is skipped in CI (`PTY-PATH-BLOCKED`).
* **`harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable` is the flagged FLAKY / STATE-SENSITIVE one** — defined at `src/harness/mod.rs:1200`; the script's own comment says it "fails at `src/harness/mod.rs:1261` iff a `.marmel/` session dir exists in CWD (`src/harness/mod.rs:1251`); passes in a clean tree, so CI never sees it and this skip hides a working-tree/state problem".
  ⚠️ **`.marmel/` is present in this workspace CWD** (`execution_plan.md`, `marmel.log`, `prompts/`, `session/`). A validator running bare `cargo test` here should therefore expect this single lib test to fail for state reasons — not because of the two defects. Either keep the `--skip`, or run the suite from a clean tree. Do not "fix" it by deleting `.marmel*` directories without approval (they are the untracked `.marmel.old*` state dirs plus the live `.marmel/`).

**Serialization requirements (process-global state):** run these with `-- --test-threads=1` —

* `orchestrator::notice_tests` / `harness::dispatch_integrity_tests` / anything touching the notice store (mutex `TEST_NOTICE_MUTEX`, `src/orchestrator/notice.rs:430`; `clear_all_notices()` is global).
* `tests/test_steer_history.rs` — header line 26-27 states it must be run with `cargo test --test test_steer_history -- --test-threads=1` (`STEERING_HISTORY` in `src/orchestrator/bus.rs`).
* `tests/test_plan_checkoff.rs` — header line 19 states `cargo test --test test_plan_checkoff -- --test-threads=1`.
* `tests/test_midturn_notice.rs` (semaphore `RUN_SEM`), `tests/test_delegation_queue_events.rs` and `tests/test_validator_concurrency.rs` (pool registry, active config, cancellation token, delegation sink, status sender are all global), `tests/test_ui_session.rs` (`TEST_MUTEX`).
* Timing-sensitive pool tests: `tests/test_validator_concurrency.rs:443, 556, 689, 816` use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`.
* Some tests intentionally point at an unreachable backend (`http://127.0.0.1:11434`, e.g. `src/orchestrator/steer_tests.rs:255`) — a loopback-blocked sandbox would surface as a false failure there.

**Doc-drift to note for the validator:** `Cargo.toml:66` and many `src/**` comments reference `docs/…` files that are not tracked (`docs/cross_platform_gating.md`, `docs/concurrency_design_decisions.md`, `docs/fix_verification.md`, `docs/research/logs_fix_spec.md`). Only `README.md` (§Testing, lines 559-586) and `scripts/cross_platform_check.sh` are authoritative for commands in this checkout.

---

## 6. Verification log (what was actually run — all read-only except the permitted build/list)

```
git status --porcelain                      -> only '?? .marmel.old*' (17 dirs)
git status --porcelain --untracked-files=no -> (empty)
git branch --show-current                   -> main
git rev-parse HEAD                          -> de3e99c89b18bf43fbb471930bd9f7a40e1ca1b2
git log --oneline -15                       -> see §1
git remote -v                               -> origin https://github.com/Na1w/marmel.git
git branch -a / git branch -r --contains HEAD -> origin/HEAD -> origin/main; origin/main contains HEAD
git ls-files docs                           -> (empty); git check-ignore docs/... -> not ignored
cargo --version / rustc --version           -> 1.99.1 / 1.99.1
cargo build --lib --offline                 -> Finished in 28.89s (success, no warnings)
cargo test --lib --offline -- --list        -> 1188 tests (names quoted in §3 were copied from this output)
cargo test --test <T> --offline -- --list   -> T = test_steer_history(9), test_validation_loop(19),
                                               test_dynamic_prompts(10), test_validator_concurrency(8),
                                               test_plan_checkoff(8), test_midturn_notice(3),
                                               test_delegation_queue_events(6), test_ui_session(21),
                                               test_role_gating(25)
```

No test body was executed; no source file was written; no git mutation (no branch, no commit, no stash).

MISSION COMPLETE
