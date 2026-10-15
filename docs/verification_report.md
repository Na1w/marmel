# Verification Report — mid-history `system` → `user` wire demotion (`ChatClient::try_chat_once`)

Auditor: independent verification pass (coder_specialist, audit mode). Date: `2026-10-15T02:40:31+02:00`.
Toolchain: `cargo 1.98.0 (797e8a9bc 2026-08-05)`, `rustc 1.98.0 (88d9e12ae 2026-08-18)`, `clippy 0.1.98 (88d9e12ae1 2026-08-18)`.
Workspace: `/home/fredrik/marmel/wip` (package `marmennill`), **uncommitted working-tree change set**.

No source or test file was modified during this audit. The only write performed by the auditor is this report.
One build-cache-only operation was performed (`cargo clean -p marmennill`, 11.9 GiB of `target/` artifacts) — see §8 for why, and note that
**every quoted result below comes from a clean rebuild of the audited working tree.**

---

## 1. Change inventory (what was audited)

`git status --porcelain` (excluding `.marmel*` scratch dirs):

```
 M src/llm/client.rs
 M src/llm/client_tests.rs
?? bugreport.md
?? docs/research_message_wire_types.md
?? docs/research_rebirth_checkpoint_roles.md
?? docs/research_request_boundary.md
?? marmel.toml.df
?? tests/test_rebirth_system_role.rs
```

`git diff --stat`:

```
 src/llm/client.rs       |  26 +++++-
 src/llm/client_tests.rs | 221 ++++++++++++++++++++++++++++++++++++++++++++++++
 2 files changed, 246 insertions(+), 1 deletion(-)
```

* Production change: exactly one hunk, `src/llm/client.rs:540-578` (comment `:540-550`, rewrite loop `:551-578`,
  demotion branch `:552-564`). No other production file is touched.
* Tests added: 4 unit tests in `src/llm/client_tests.rs:941/977/998/1020` + shared helpers
  (`capturing_sse_mock:851`, `wire_roles:876`, `local_roles:888`, `system_msg:892`, `user_msg:898`,
  `checkpoint_content:907`, `sent_body_for:916`), and 4 integration tests in the new file
  `tests/test_rebirth_system_role.rs:204/260/303/353`.
* The untracked `docs/research_*.md`, `bugreport.md`, `marmel.toml.df` are documentation/config artifacts, not code
  (they are byte-unchanged since before the fix per mtimes and are irrelevant to the tests run).

The fix itself (verified byte-for-byte against the working tree):

```rust
// src/llm/client.rs:551-564
for (index, msg) in req_body.messages.iter_mut().enumerate() {
    if index > 0 && matches!(msg, crate::types::Message::System { .. }) {
        let demoted = std::mem::replace(
            msg,
            crate::types::Message::User { content: String::new() },
        );
        *msg = crate::types::Message::User {
            content: demoted.content().unwrap_or_default().to_string(),
        };
        continue;
    }
    ...
```

Ordering inside `try_chat_once` (criterion-relevant): clone at `:535` (`let mut req_body = req.clone();`), normalization
`:540-578`, `tracing::info!` message count `:580-585`, `count_tokens` `:586`, request-body debug dump `:588-591`,
`debug_log::log_llm_request` `:592`, `.json(&req_body)` / `builder.send()` `:595-602`. So token accounting, debug dump
and request logging all observe the **normalized** payload, and no code path can serialize the un-normalized vector.

---

## 2. Acceptance-criteria verdict table

| # | Acceptance criterion | Verdict | Evidence (file:line) |
|---|---|---|---|
| 1 | A history with a system message at index > 0 goes out as `user`; the body satisfies a system-first backend (exactly one `role:"system"`, at index 0) | **PASS** | Production: `src/llm/client.rs:551-564`. Unit: `src/llm/client_tests.rs:941-972` (wire roles `["system","user","user"]` read from the **captured HTTP body**, `:956-960`). Integration: `tests/test_rebirth_system_role.rs:204-252`, incl. the explicit "exactly one system, at index 0" assertion `:244-251` (`assert_eq!(systems, vec![0])`); multi-checkpoint case `:303-338`. Negative control: all 4 integration tests fail without the hunk (§8). |
| 2 | The system message at index 0 keeps its role | **PASS** | Guard `index > 0` at `src/llm/client.rs:552`. Unit: `client_tests.rs:998-1014` (`[system,system,system]` → `["system","user","user"]`, content of all three preserved) and `client_tests.rs:977-994` (index 0 may stay `user`; nothing else moves). Integration: `tests/test_rebirth_system_role.rs:218-222` and `:386-394`. |
| 3 | Rebirth detection via `REBIRTH_CHECKPOINT_PREFIX` still works on the in-memory history | **PASS** | Detectors are content-based and role-tolerant: `src/manager/context.rs:275-290` (`pinned_prefix_end`, matches `System \| User` on the prefix), `:367-373` (`INJECTED_ADVISORY_PREFIXES`, incl. `REBIRTH_CHECKPOINT_PREFIX:95`), `:377-381` (`is_injected_advisory`), `:1030-1065` (`perform_rebirth`); `src/session/restore.rs:769-784` (`pinned_prefix_end_records`). The fix mutates only the clone (`:535`), so in-memory roles are unchanged. Tests: `tests/test_rebirth_system_role.rs:260-297` (after a real send: `matches!(engine.messages()[2], Message::System{..})` `:275-279`, prefix still matched `:283-286`, `is_injected_advisory` `:288`, `pinned_prefix_end()` unchanged `:289-293`), `:303-338` (two checkpoints, pinned window `=8` before and after), `:353-405` (live `perform_rebirth` after a send keeps REQ-CORE pin contract, `pinned_prefix_end()==4` `:401`). |
| 4 | A unit test covers `[system, user, system]` → `[system, user, user]` | **PASS** | `src/llm/client_tests.rs:941` `test_llm_mid_history_system_message_is_demoted_to_user_on_the_wire`: history built at `:947-953` is exactly `[System, User, System(checkpoint)]`, asserted to serialize as `["system","user","user"]` at `:956-960`, byte-identical content at `:961-971`. Runs in the lib target (`cargo test --lib llm::client` → `... ok`). |
| 5 | `(SYSTEM: ...)` text prefix preserved (not stripped); adjacent `User` messages NOT merged | **PASS** | Demotion reuses the original bytes verbatim: `src/llm/client.rs:560-562` (`demoted.content()` → `String`, no trim/strip/re-wrap), and the loop never removes or concatenates elements (length/order preserved implicitly by `iter_mut().enumerate()`). Fixture uses the real constant: `client_tests.rs:907-912` builds from `crate::manager::context::REBIRTH_CHECKPOINT_PREFIX`. Assertions: byte-identical checkpoint bytes on the wire `client_tests.rs:967-971`; wire length `7` "no turn is added, dropped or reordered" `tests/test_rebirth_system_role.rs:217`, byte-identical `:229-233`, legacy advisory kept opaque `:235-239`; length `8` and both checkpoints byte-identical `:314-319`. No merge logic exists anywhere in the hunk (diff review) — back-to-back `user` turns remain separate. |
| 6 | Only the cloned request payload is mutated; caller's vector / `ContextEngine` history untouched | **PASS** | `chat(&self, req: &ChatRequest)` takes the request by reference (`src/llm/client.rs:468`); the rewrite acts on `req_body = req.clone()` (`:535`), and `ChatRequest: Clone` with owned `Vec<Message>`/`String` (`src/types/wire.rs:162-165`, `:7-28`) ⇒ deep copy. Tests: `client_tests.rs:1020-1061` (wire roles normalized `:1046-1050` while the caller's `req.messages` still reports `["system","user","system"]` `:1051-1055` and the checkpoint content is untouched `:1056-1060`); `tests/test_rebirth_system_role.rs:274` (`engine.messages().len()==7` after send) and `:323-337`. |
| 7 | Fix sits at the single request boundary ⇒ covers every call site + restored legacy session bundles | **PASS** | There is exactly one `/chat/completions` serialization point in non-test code: URL built at `src/llm/client.rs:530-533`, body serialized at `:595`, sent at `:602`; `grep -rn "chat/completions" src/` yields no other production POST. Entry points: `chat:468` and `chat_stream:486` → retry policy → `ChatAttempt::attempt` (`:403` `self.client.try_chat_once(...)`) ⇒ every attempt is normalized. Call-site map: manager/UI turn loop `src/ui/session.rs:2201` → `chat_client_turn` `src/llm/stream.rs:675` → `chat_stream_resumable:529` (`.chat_stream` at `:565` and `:580`); steer `src/orchestrator/steer.rs:220` and `:792`; notice `src/orchestrator/notice.rs:1235`; prompt builder `src/agents/prompt_builder.rs:353`; validator `src/agents/validation.rs:273/371`, `:463/523` → `src/agents/runner/fix_loop.rs:904`/`:1257` (`chat_stream_resumable`); runner `src/agents/runner/mod.rs:95`. Restored legacy bundles keep `Message::System` verbatim (`src/session/format/messages.rs:108`, installed wholesale by `src/manager/context.rs:1151-1160` with no role rewriting), and that shape is exercised end-to-end by `tests/test_rebirth_system_role.rs:64-116,204-252`. |

Criteria score: **7 / 7 PASS.**

---

## 3. Verbatim command results

All commands were run from `/home/fredrik/marmel/wip` after `cargo clean -p marmennill` (i.e. no cached
`marmennill` artifacts, no cross-tree contamination — see §8). Dependency artifacts were reused from `target/`.

### 3.1 `cargo build`

```
   Compiling marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 21.70s
```

A repeat run on the same tree (cache warm) reported: `Finished \`dev\` profile [unoptimized + debuginfo] target(s) in 0.31s`.

### 3.2 `cargo test --lib llm::client`

```
running 22 tests
test llm::client::tests::test_chat_error_retryability_matches_shared_policy ... ok
test llm::client::tests::test_consume_event_does_not_double_count_tokens ... ok
test llm::client::tests::test_consume_event_marks_done_abort_and_continue ... ok
test llm::client::tests::test_llm_uses_shared_policy_values ... ok
test llm::client::tests::test_policy_refused_mcp_names_are_never_advertised ... ok
test llm::client::tests::test_chat_client_default_and_custom_watchdogs ... ok
test llm::client::tests::test_cancellation_before_response_is_cancelled_not_empty_success ... ok
test llm::client::tests::test_genuine_empty_completion_with_finish_reason_is_complete ... ok
test llm::client::tests::test_llm_non_retryable_error_short_circuits ... ok
test llm::client::tests::test_stream_ending_without_done_marker_is_truncated ... ok
test llm::client::tests::test_llm_recovery_request_body_stays_in_documented_ranges ... ok
test llm::client::tests::test_llm_first_position_keeps_its_role_when_it_is_not_system ... ok
test llm::client::tests::test_llm_only_the_first_of_consecutive_system_messages_stays_system ... ok
test llm::client::tests::test_truncated_tool_call_arguments_are_not_parsed_as_a_call ... ok
test llm::client::tests::test_mid_stream_cancellation_is_cancelled_and_never_retried ... ok
test llm::client::tests::test_llm_mid_history_system_message_is_demoted_to_user_on_the_wire ... ok
test llm::client::tests::test_completed_tool_call_stream_still_parses ... ok
test llm::client::tests::test_llm_system_demotion_mutates_only_the_cloned_payload_not_the_callers_history ... ok
test llm::client::tests::test_llm_on_delta_abort_during_prefill ... ok
test llm::client::tests::test_llm_retry_exhaustion ... ok
test llm::client::tests::test_llm_retry_backoff ... ok
test llm::client::tests::test_llm_prefill_delay_triggers_initial_timeout_and_retries ... ok

test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 930 filtered out; finished in 5.50s
```

All 4 new unit tests are present and green; the previously flaky
`test_policy_refused_mcp_names_are_never_advertised` passed here and in both full-lib runs.

### 3.3 `cargo test --test test_rebirth_system_role`

```
     Running tests/test_rebirth_system_role.rs (target/debug/deps/test_rebirth_system_role-b2dac7c0fa9e2f50)

running 4 tests
test test_restored_legacy_system_checkpoint_stays_detectable_in_memory ... ok
test test_every_mid_history_system_turn_is_demoted_and_index_zero_is_untouched ... ok
test test_restored_legacy_system_checkpoint_goes_out_as_user ... ok
test test_reborn_history_keeps_pin_contract_and_sends_checkpoint_as_user ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.50s
```

### 3.4 `cargo test --test test_llm`

```
running 5 tests
test test_assistant_message_content_never_serializes_as_null ... ok
test test_integration_thinking_demuxer_streaming_chunks ... ok
test test_integration_chat_request_payload_construction ... ok
test test_stream_cut_after_deltas_sends_exactly_one_request ... ok
test test_failure_before_any_delta_is_still_retried_once ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.48s
```

### 3.5 `cargo test --test test_context`

```
running 8 tests
test test_integration_repair_tool_call_pairs_helper ... ok
test test_integration_context_engine_lifecycle ... ok
test test_integration_rebirth_checkpoint_shape_after_injected_advisory ... ok
test test_integration_compaction_keeps_tool_call_pairing_valid ... ok
test test_live_manager_compaction_triggers_earlier_than_message_only_baseline ... ok
test test_specialist_advertised_list_precharge_raises_request_size ... ok
test test_live_manager_context_charges_its_advertised_tool_schema ... ok
test test_integration_compaction_reports_failure_for_unreachable_target ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.45s
```

### 3.6 `cargo test --test test_role_gating`

```
test test_prompt_based_tool_gating ... FAILED
...
---- test_prompt_based_tool_gating stdout ----

thread 'test_prompt_based_tool_gating' (1133330) panicked at tests/test_role_gating.rs:312:5:
Validator with run_command in prompt must be permitted to execute run_command
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    test_prompt_based_tool_gating

test result: FAILED. 24 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.55s

error: test failed, to rerun pass `--test test_role_gating`
```

Excerpt of the same run's test lines (all 23 other tests in that suite reported `ok`):

```
test test_prompt_based_tool_gating ... FAILED
test validator_may_record_an_explicit_approval ... ok
test repeated_self_verdict_attempts_fail_the_run_without_recording_a_verdict ... ok
test worker_self_approval_is_refused_and_the_verdict_file_is_untouched ... ok
```

### 3.7 Session-restore suites — `cargo test --test test_session_restore_manager --test test_session_restore_workers --test test_session_restore_dangling --test test_session_commit --test test_session_boot`

```
     Running tests/test_session_boot.rs (target/debug/deps/test_session_boot-dbc3e66add414c39)
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.61s
     Running tests/test_session_commit.rs (target/debug/deps/test_session_commit-93f25ec0772b4656)
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.47s
     Running tests/test_session_restore_dangling.rs (target/debug/deps/test_session_restore_dangling-2bb3053520159741)
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.48s
     Running tests/test_session_restore_manager.rs (target/debug/deps/test_session_restore_manager-3569b8db5bc503fd)
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.43s
     Running tests/test_session_restore_workers.rs (target/debug/deps/test_session_restore_workers-7df6e1d5ab2b051e)
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.47s
```

Total 43 passed / 0 failed. Role-relevant pins that guard criterion 3/6/7 stay green:
`ac_m6_no_re_role_and_no_illegal_reorder`, `ac_m7_legacy_advisory_text_and_system_checkpoint_are_opaque`,
`ac_p1_pinned_pair_survives_the_full_boot_path_byte_for_byte`,
`ac_p3_recomputed_pinned_prefix_end_matches_the_persisted_value` (exercises `restore.rs:769-784`),
`ac_p2_pin_witness_agrees_with_the_installed_vector`.

Full per-test lists for these five suites were captured verbatim in an earlier identical run (43 individual
`... ok` lines, no failures).

### 3.8 Clippy

```
### cargo clippy --lib ###
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 10.81s
### cargo clippy --lib --tests ###
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 20.30s
### cargo clippy --test test_rebirth_system_role ###
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.27s
```

Zero diagnostics for all three invocations. Note `Cargo.toml:72-73` sets
`[lints.clippy] all = { level = "deny", priority = -1 }`, and `Cargo.toml:67-70` denies `unused`/`dead_code`/`unused_imports` —
so any real clippy finding in a linted target would be a hard error, not a warning. Confirmed live in the verbose
invocation recorded in §7.

### 3.9 `cargo test --lib` (full lib suite, regression count)

Run 1 (final tree):

```
running 952 tests
test result: FAILED. 947 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 16.08s

failures:
    harness::pty::tests::test_harness_pty_process_group_kill
    harness::pty::tests::test_interactive_pty_manager_lifecycle
    harness::pty::tests::test_run_command_parses_timeout_seconds_argument
    harness::pty::tests::test_run_command_pty_cancelled_by_worker_token
    orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation
```

Run 2 (flakiness probe, same tree):

```
test result: FAILED. 946 passed; 6 failed; 0 ignored; 0 measured; 0 filtered out; finished in 16.13s

failures:
    harness::pty::tests::test_harness_pty_process_group_kill
    harness::pty::tests::test_interactive_pty_manager_lifecycle
    harness::pty::tests::test_run_command_parses_timeout_seconds_argument
    harness::pty::tests::test_run_command_pty_cancelled_by_worker_token
    orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup
    orchestrator::notice_tests::test_task_id_address_matches_exact_task_id_only
```

The `harness::pty` set is invariant; the `orchestrator::notice_tests` membership rotates run-to-run ⇒ flaky set,
not a regression. Representative failure text (run 1):

```
thread 'orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation' (1124943) panicked at src/orchestrator/notice_tests.rs:457:10:
Event::SteerResponse should be emitted: Disconnected
```

and the PTY signature:

```
thread 'harness::pty::tests::test_harness_pty_process_group_kill' (1126668) panicked at src/harness/pty_tests.rs:16:47:
spawn sandbox: Execution(failed to openpty: Os { code: 13, kind: PermissionDenied, message: "Permission denied" })
```

Regression count: **952 lib tests, 947 passed / 5 failed**, i.e. the 5 new-test-inclusive lib total grew from the
930-filtered baseline (`930 filtered + 22 selected`) and no `llm::client` test regressed.

---

## 4. Pre-existing / environmental vs new failures

**New failures introduced by this change: NONE.** Every failure observed is on the sanctioned baseline list
(`docs/baseline_test_run.md`) or is demonstrably flaky/global-state, and none of them touches `llm::client`
message roles or the changed hunk.

| Failure | Classification | Evidence it is NOT caused by this diff |
|---|---|---|
| `tests/test_role_gating.rs::test_prompt_based_tool_gating` (`:312:5`) | **PRE-EXISTING, deterministic, documented baseline failure** | `docs/baseline_test_run.md:151` lists it; `:180` "FAILS deterministically (fails in every run…)"; `:198` "CONFIRMED — … STILL FAILS at baseline"; `:222` counts it among 5 sanctioned baseline failures; `docs/cleanup_t037.md:100,109` and `docs/verification_tests.md:16` re-confirm it. The assertion under test is tool-dispatch gating (`dispatch_for(&tool_run, ToolCaller::SpecialistWithTools{…})`, `tests/test_role_gating.rs:296-315`) — no LLM transport, no message role. The diff touches only `src/llm/client.rs:540-578`. |
| 4 × `harness::pty::tests::*` — `failed to openpty: PermissionDenied` | **ENVIRONMENTAL (pre-existing)** | `docs/baseline_test_run.md:138-140` and `:214` ("`openpty` EACCES because `/dev/pts/ptmx` is mode `000`"), `:222` (sanctioned). Same 4 tests, same `openpty` errno, identical to the recorded baseline set; `harness/pty` never constructs chat messages. |
| `orchestrator::notice_tests::*` (rotating: `…_success_and_validation`, `…_ask_followup`, `test_task_id_address_matches_exact_task_id_only`) | **FLAKY (pre-existing global-state contention)** | Different members fail in the two runs above (5 vs 6 failures; 947 vs 946 passed) and `…_success_and_validation` passed in 3/3 isolated re-runs: `cargo test --lib orchestrator::notice_tests::test_handle_reply_to_arbitrator` → `test result: ok. 2 passed; 0 failed` ×3. `docs/baseline_test_run.md:175-177` already classifies these as "passes isolated → flaky". The failure mode is `Event::SteerResponse … Disconnected` (`src/orchestrator/notice_tests.rs:457`), i.e. the process-global event sender (`crate::orchestrator::set_event_sender`, `notice_tests.rs:406`) being swapped by a concurrently running test — a receiver race, not a payload/role problem. Those tests do send through `ChatClient`, and their mock answers `200` in both outcomes. |
| `llm::client::tests::test_policy_refused_mcp_names_are_never_advertised` | **Did not fail in any run of this audit** | `... ok` in `cargo test --lib llm::client` (§3.2) and in both full-lib runs (`grep -c 'test_policy_refused_mcp_names_are_never_advertised ... ok'` = 1 in each log). Baseline documents it as occasionally flaky; not triggered here. |
| (none) | **NEW** | No failure in any of the 13 executed commands can be attributed to the diff. All 8 new tests pass; the 5 failing lib tests and 1 failing integration test are all on the documented baseline/flaky sets. |

---

## 5. Test-validity check (negative control)

To rule out vacuous tests, the change was reverted **in a throwaway copy of the tree** (`/tmp/negctl`, sources only;
`diff` proved that copy's `src/llm/client.rs` was byte-identical to `git show HEAD:src/llm/client.rs`, while the new
test file was byte-identical to the audited one) and the new tests were re-run there:

```
$ cargo test --test test_rebirth_system_role          # pre-fix client.rs
test result: FAILED. 0 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out;
---- test_restored_legacy_system_checkpoint_goes_out_as_user stdout ----
assertion `left == right` failed: the legacy mid-history System checkpoint must be sent as `user`
  left: "system"
 right: "user"
```

```
$ cargo test --lib llm::client                        # pre-fix client.rs
test result: FAILED. 18 passed; 4 failed; 0 ignored; 0 measured; 930 filtered out;
failures:
    llm::client::tests::test_llm_first_position_keeps_its_role_when_it_is_not_system
    llm::client::tests::test_llm_mid_history_system_message_is_demoted_to_user_on_the_wire
    llm::client::tests::test_llm_only_the_first_of_consecutive_system_messages_stays_system
    llm::client::tests::test_llm_system_demotion_mutates_only_the_cloned_payload_not_the_callers_history
```

All 8 new tests fail without the hunk and pass with it ⇒ genuine regression guards, and the 18 pre-existing
`llm::client` tests are unaffected in either state. `/tmp/negctl` was deleted afterwards; the audited tree was
untouched (verified by `git diff --stat` re-check, identical 26/221 line stats).

---

## 6. Independent confirmation of the boundary claim (criterion 7)

Static verification performed by the auditor (not taken on faith):

1. Only one production serialization of `ChatRequest` exists: `src/llm/client.rs:595` (`client.post(&url).json(&req_body)`),
   with the URL built at `:530-533`. `grep -rn "chat/completions" src/` returns only that site plus test/mock fixtures.
2. `try_chat_once` is reached exclusively through `chat_stream` (`:486`) via the shared retry policy —
   `ChatAttempt::attempt` at `:403` — and `chat` (`:468`) delegates to `chat_stream`. Therefore every attempt of every
   retry re-normalizes, and no call site can bypass the demotion.
3. `chat_stream_resumable` (`src/llm/stream.rs:529`) and `chat_client_turn` (`:675`) both bottom out in
   `client.chat_stream(...)` (`:565`, `:580`), so resumable/steered turns and the live manager loop
   (`src/ui/session.rs:2201`) are covered.
4. Every other request builder in production (`steer.rs:198/773`, `notice.rs:1216`, `prompt_builder.rs:335`) places its
   `Message::System` at index 0, so the demotion is a no-op for them — consistent with criterion 2.
5. Restored legacy bundles really do put `Message::System` at index > 0 in the live vector
   (`src/session/format/messages.rs:108` maps `MessageRecord::System → Message::System` verbatim;
   `src/manager/context.rs:1151-1160` installs records wholesale, explicitly "no re-role";
   documented at `src/manager/context.rs:268-274`). This is the live production source of the failure today, and it is
   the exact fixture shape used by `tests/test_rebirth_system_role.rs:64-108`.

---

## 7. Resolving the reported discrepancy — `cargo clippy --lib --tests` vs `--test test_rebirth_system_role`

**Verdict: there is NO clippy denial. `cargo clippy --lib --tests` is clean. The report of
`needless_range_loop` at `tests/test_rebirth_system_role.rs:383` is not reproducible against the current working tree.**

Established facts:

1. After `cargo clean -p marmennill` (which deleted every `marmennill` clippy fingerprint, 13 568 files), both
   invocations were re-executed from scratch and produced zero diagnostics (§3.8: `--lib` 10.81 s,
   `--lib --tests` 20.30 s, `--test test_rebirth_system_role` cached-clean). An additional forced re-lint of the whole
   package (`cargo clippy --lib --tests -- -D warnings`) also completed clean.
2. `--tests` demonstrably **does** lint the new integration target. Verbatim unit command captured with `-v`:
   ```
   Running `.../bin/clippy-driver .../bin/rustc --crate-name test_rebirth_system_role --edition=2024
   tests/test_rebirth_system_role.rs --error-format=json ... '--deny=clippy::all' --deny=unused_imports
   --deny=unused --deny=dead_code --test ... -C extra-filename=-d034acbacfcbc1f1 --out-dir
   /home/fredrik/marmel/wip/target/debug/deps ...`
   ```
   (i.e. the file is linted with the package-level `deny(clippy::all)` from `Cargo.toml:72-73`).
3. The lint in question cannot fire on this file: `clippy::needless_range_loop` only triggers on index-based range
   loops (`for i in 0..xs.len()`), and the file contains none. Exhaustive loop inventory
   (`grep -n "for " tests/test_rebirth_system_role.rs`):
   * `:324` — `for index in [2usize, 7] {`
   * `:392` — `for (index, msg) in wire.iter().enumerate().skip(1) {` (both `index` and `msg` used; idiomatic)

   Current line 383 is `let second_backend = sse_backend().await;` — not a loop at all.
4. Consequence for the disagreement: the "clippy `--lib --tests` fails" report was almost certainly made against an
   **earlier revision of the test file** (which is untracked, so no git history exists to confirm the intermediate
   text), or against a contaminated build. The later report ("`--test test_rebirth_system_role` clean") matches the
   current tree, and is consistent with `--lib --tests` also being clean.

**No follow-up fix is required for clippy, and nothing in this area blocks acceptance.** If the reporting implementer
can reproduce the denial, they must re-run `cargo clean -p marmennill && cargo clippy --lib --tests` on the current
tree; the auditor could not reproduce it under any of the three invocations plus a forced re-lint.

---

## 8. Method note / integrity disclosure

During the negative control (§5) the auditor briefly pointed the copy's `CARGO_TARGET_DIR` at the audited workspace's
`target/`. Cargo then reused a locally built test binary across the two source trees, and one follow-up
`cargo test --lib llm::client` in the audited tree reported the 4 new tests failing (18 passed / 4 failed) — an artifact
of the pre-fix binary from the copy, **not** a state of the audited sources. Detected immediately; the auditor ran
`cargo clean -p marmennill` (build output only — no source/test file was touched) and re-executed every command in §3.
`git status`/`git diff --stat` were re-checked before and after: the working tree still contains exactly the change set
described in §1. All numbers quoted in §3 are post-clean rebuilds of the audited tree.

---

## 9. Observations (non-blocking)

1. **Stale doc comment in the new unit-test block** — `src/llm/client_tests.rs:904-906` says
   `ContextEngine::perform_rebirth` "appends [the injection] in the middle of the history as a `Message::System`".
   At HEAD `perform_rebirth` already emits the checkpoint as **`Message::User`**
   (`src/manager/context.rs:1006-1009`, `:1062-1064`, "bug M12"), which is also what
   `tests/test_rebirth_system_role.rs:374-381` asserts. So the *bugreport's* repro (System checkpoint injected by a
   fresh rebirth) is no longer the live source; the live source is **restored legacy bundles**
   (`src/session/format/messages.rs:108`) plus any future mid-history `System`. The fix is still the correct single
   boundary, and the test fixture (a hand-built `Message::System` checkpoint) remains valid — but the wording should be
   corrected to avoid teaching the wrong mechanism. Documentation-only follow-up.
2. **Coverage gap (minor):** the new suite pins `ContextEngine::pinned_prefix_end` (the wire/engine side) but does not
   directly exercise the `MessageRecord` twin `src/session/restore.rs:pinned_prefix_end_records`; that detector is
   covered by the existing session suites (`ac_p3_recomputed_pinned_prefix_end_matches_the_persisted_value`,
   `ac_m7_legacy_advisory_text_and_system_checkpoint_are_opaque`), both green. Optional: one assertion calling
   `pinned_prefix_end_records` on `legacy_restored_records()` would close the loop named in criterion 3.
3. `src/manager/context.rs:153` (`count_tokens`) matches `System | User` on content, so post-demotion token accounting
   is byte-equivalent — no hidden budget drift from the fix (consistent with `ac_m8_engine_scalars_survive_and_budget_drift_is_reported`
   passing).
4. Demotion correctly skips the assistant-rewrite branch via `continue` (`src/llm/client.rs:563`), and cannot affect
   `Assistant`/`Tool` variants because of the `matches!(…, System)` guard — pairing repair is untouched
   (`test_integration_compaction_keeps_tool_call_pairing_valid`, `ac_d*` suites green).
5. Open questions from `bugreport.md` are answered conservatively and in line with acceptance criterion 5: the
   `(SYSTEM: …)` text prefix is **kept**, and adjacent `User` turns are **not merged** (evidence in row 5 of §2).

---

## 10. Final verdict

**ACCEPT WITH FOLLOW-UPS.**

* All 7 acceptance criteria: **PASS** (code + test evidence in §2, boundary proof in §6).
* Build clean; 13 of 13 executed commands behave as required; 8/8 new tests pass, and all 8 are proven genuine
  regression guards by the pre-fix negative control (§5).
* Zero failures attributable to the change (§4): the only failures are the documented baseline PTY-environment set
  (4 × `openpty` EACCES), the documented deterministic `test_prompt_based_tool_gating`, and rotation-flaky
  `orchestrator::notice_tests::*` (verified passing in isolation).
* The disputed clippy claim is **false against the current tree**: `cargo clippy --lib`, `cargo clippy --lib --tests`
  and `cargo clippy --test test_rebirth_system_role` are all clean (§7). No clippy follow-up is required and nothing
  here blocks acceptance.
* Remaining follow-ups are non-blocking and documentation/coverage-level: fix the stale "perform_rebirth appends as
  `Message::System`" wording in `src/llm/client_tests.rs:904-906` (and the corresponding framing in `bugreport.md`),
  and optionally add one direct assertion against `src/session/restore.rs:pinned_prefix_end_records`.

---

# §11 Final clean-lint & full-suite verification (post-formatting remediation)

**Task:** t-012 — final authoritative verification run of the whole workspace after the formatting remediation.
**Executed:** `2026-10-15`, workspace `/home/fredrik/marmel/wip`, package `marmennill v1.0.0`, branch `1.0.0-refactor`,
HEAD `9fab839 2026-10-15 01:20:14 +0200 fmt`.
**Toolchain:** `cargo 1.98.0 (797e8a9bc 2026-08-05)`, `rustc 1.98.0 (88d9e12ae 2026-08-18)`,
`rustfmt 1.9.0-stable (88d9e12ae1 2026-08-18)`, `clippy 0.1.98 (88d9e12ae1 2026-08-18)`.
**Read-only guarantee:** no source or test file was created, edited, moved or reverted during this run. The only file
written is this appended section. Working-tree integrity before and after the run is byte-identical:

```
$ md5sum src/llm/client.rs src/llm/client_tests.rs tests/test_rebirth_system_role.rs
9309e811c76bdbb8f65c14a4f816aeeb  src/llm/client.rs
ff776f4243f3c6d3a4b6aa43b2f9a353  src/llm/client_tests.rs
7fab806fc18e42fa1b49de86c43ee816  tests/test_rebirth_system_role.rs
$ git diff --stat | cat
 src/llm/client.rs       |  26 +++++-
 src/llm/client_tests.rs | 223 ++++++++++++++++++++++++++++++++++++++++++++++++
 2 files changed, 248 insertions(+), 1 deletion(-)
```

Only `target/` artifacts were deleted/rebuilt (`cargo clean -p marmennill` before every gated command, as required).
Section numbering: the existing audit already occupies §1–§10, so this final section is §11 (the requested
"§9 Final clean-lint & full-suite verification" title would have collided).

## 11.1 Gate 1 — formatting

```
$ cargo clean -p marmennill
     Removed 311 files, 260.6MiB total
=== cargo fmt --all -- --check ===
FMT_EXIT=0
```

`cargo fmt --all -- --check` produced **no output at all** and exited **0**. The formatting remediation is confirmed:
nothing in the workspace (including the remediated `tests/test_rebirth_system_role.rs`, mtime
`2026-10-15 03:12:36`, 408 lines) is unformatted.

## 11.2 Gate 2 — clippy (exact CI gate command)

```
$ cargo clean -p marmennill
     Removed 311 files, 260.6MiB total
$ CARGO_TERM_COLOR=never cargo clippy --all-targets --all-features -- -D warnings
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 18.61s
$ echo $?
0
$ grep -c -E '^(warning|error)' <clippy log>
0
```

Exit **0**, **0 diagnostics**. The run genuinely re-checked the audited package from a clean package state
(`Checking marmennill …` over 348 units on the preceding identical invocation, 20.36 s, also exit 0), and it covers
`--all-targets` (lib + `marmel` bin + all 27 integration-test binaries) and `--all-features`.

**CI step parity statement:** the two commands executed here are byte-for-byte the CI gate commands.
`.github/workflows/ci.yml` → `jobs.lint.steps` uses `run: cargo fmt --all -- --check` ("Check code formatting") and
`run: cargo clippy --all-targets --all-features -- -D warnings` ("Run Clippy"), and the `jobs.test` matrix repeats the
same two commands per OS ("Check code formatting" / "Run Clippy for this target"). Therefore a green lint job on
Linux is guaranteed for the fmt and clippy steps; the unresolved cross-platform residual risk documented in
`docs/cross_platform_gating.md` §C.4 (apple-darwin / windows-msvc `cfg` classes cannot be checked from this Linux host)
is unaffected by, and not masked by, this verification.

## 11.3 Gate 3 — build all targets

```
$ cargo clean -p marmennill
     Removed 311 files, 260.6MiB total
$ CARGO_TERM_COLOR=never cargo build --all-targets
   Compiling marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 42.93s
$ echo $?
0
$ grep -c -E '^(warning|error)' <build log>
0
```

Exit **0**, **warning count = 0** (the `[lints.rust] unused/dead_code/unused_imports = "deny"` and
`[lints.clippy] all = "deny"` tables in `Cargo.toml` mean a non-zero warning count could not even be tolerated).

## 11.4 Gate 4 — full test suite

### 11.4.1 The spec command `cargo test` (fail-fast, verbatim)

```
$ CARGO_TERM_COLOR=never cargo test
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.27s
     Running unittests src/lib.rs (target/debug/deps/marmennill-25e1737d17c27a1e)

running 952 tests
...
failures:

---- harness::pty::tests::test_run_command_parses_timeout_seconds_argument stdout ----
thread 'harness::pty::tests::test_run_command_parses_timeout_seconds_argument' (1175260) panicked at src/harness/pty_tests.rs:102:34:
executes with custom timeout: Execution(failed to openpty: Os { code: 13, kind: PermissionDenied, message: "Permission denied" })
---- harness::pty::tests::test_interactive_pty_manager_lifecycle stdout ----
thread 'harness::pty::tests::test_interactive_pty_manager_lifecycle' (1175256) panicked at src/harness/pty_tests.rs:120:10:
spawn session: Execution(Failed to create PTY: failed to openpty: Os { code: 13, kind: PermissionDenied, message: "Permission denied" })
---- harness::pty::tests::test_run_command_pty_cancelled_by_worker_token stdout ----
thread 'harness::pty::tests::test_run_command_pty_cancelled_by_worker_token' (1175262) panicked at src/harness/pty_tests.rs:179:5:
assertion failed: res.is_ok()
---- harness::pty::tests::test_harness_pty_process_group_kill stdout ----
thread 'harness::pty::tests::test_harness_pty_process_group_kill' (1175254) panicked at src/harness/pty_tests.rs:16:47:
spawn sandbox: Execution(failed to openpty: Os { code: 13, kind: PermissionDenied, message: "Permission denied" })
---- llm::client::tests::test_policy_refused_mcp_names_are_never_advertised stdout ----
thread 'llm::client::tests::test_policy_refused_mcp_names_are_never_advertised' (1175417) panicked at src/llm/client_tests.rs:736:5:
the policy-approved tool must be advertised, got ["delegate_task", "create_plan", "archive_current_plan", "read_file", "grep_search", "glob", "rebirth", "sleep", "reply_to_arbitrator"]
---- orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup stdout ----
thread 'orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup' (1175677) panicked at src/orchestrator/notice_tests.rs:547:18:
Expected SteerResponse, got SubagentMessage { agent_tag: "validator-planner", text: "Specialist Coder explains that timeout is already set to 15s." }
---- orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation stdout ----
thread 'orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation' (1175678) panicked at src/orchestrator/notice_tests.rs:457:10:
Event::SteerResponse should be emitted: Disconnected
---- ui::tests::one_corrupt_worker_file_is_the_tolerated_partial_and_the_rest_installs stdout ----
thread 'ui::tests::one_corrupt_worker_file_is_the_tolerated_partial_and_the_rest_installs' (1176661) panicked at src/ui/session_tests.rs:2772:9:
assertion `left == right` failed
  left: "Aborted"
 right: "Interrupted by restart"

failures:
    harness::pty::tests::test_harness_pty_process_group_kill
    harness::pty::tests::test_interactive_pty_manager_lifecycle
    harness::pty::tests::test_run_command_parses_timeout_seconds_argument
    harness::pty::tests::test_run_command_pty_cancelled_by_worker_token
    llm::client::tests::test_policy_refused_mcp_names_are_never_advertised
    orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup
    orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation
    ui::tests::one_corrupt_worker_file_is_the_tolerated_partial_and_the_rest_installs

test result: FAILED. 944 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 16.24s

error: test failed, to rerun pass `--lib`
```

**Method note (why a second full-suite invocation is reported):** `cargo test` is fail-fast — after the 8 lib
failures it aborted with `error: test failed, to rerun pass '--lib'` and **never reached any integration-test
binary** (nor the doc tests). That is the same behaviour CI has (`cargo test --all-targets --all-features`). To
satisfy "whole workspace, all integration tests + lib", the suite was therefore also executed with
`cargo test --no-fail-fast`, which runs all 30 targets and yields the per-binary tallies below. Both runs are
reported; neither hides the other.

### 11.4.2 Per-binary totals — `cargo test --no-fail-fast` (all 30 targets)

```
$ CARGO_TERM_COLOR=never cargo test --no-fail-fast   → exit 101
error: 2 targets failed:
    `--lib`
    `--test test_role_gating`
```

| # | Target | Result | Passed | Failed |
|---|---|---|---:|---:|
| 1 | `unittests src/lib.rs` | FAILED | 947 | 5 |
| 2 | `unittests src/main.rs` | ok | 14 | 0 |
| 3 | `tests/test_agent.rs` | ok | 2 | 0 |
| 4 | `tests/test_context.rs` | ok | 8 | 0 |
| 5 | `tests/test_dynamic_prompts.rs` | ok | 4 | 0 |
| 6 | `tests/test_harness.rs` | ok | 15 | 0 |
| 7 | `tests/test_llm.rs` | ok | 5 | 0 |
| 8 | `tests/test_midturn_notice.rs` | ok | 3 | 0 |
| 9 | `tests/test_monitor.rs` | ok | 2 | 0 |
| 10 | `tests/test_orchestrator.rs` | ok | 8 | 0 |
| 11 | `tests/test_plan_checkoff.rs` | ok | 6 | 0 |
| 12 | **`tests/test_rebirth_system_role.rs`** | **ok** | **4** | **0** |
| 13 | `tests/test_role_gating.rs` | FAILED | 24 | 1 |
| 14 | `tests/test_session_abort_flush.rs` | ok | 6 | 0 |
| 15 | `tests/test_session_boot.rs` | ok | 6 | 0 |
| 16 | `tests/test_session_bundle.rs` | ok | 22 | 0 |
| 17 | `tests/test_session_commit.rs` | ok | 9 | 0 |
| 18 | `tests/test_session_config.rs` | ok | 35 | 0 |
| 19 | `tests/test_session_restore_dangling.rs` | ok | 12 | 0 |
| 20 | `tests/test_session_restore_manager.rs` | ok | 8 | 0 |
| 21 | `tests/test_session_restore_panel_abort.rs` | ok | 10 | 0 |
| 22 | `tests/test_session_restore_ui.rs` | ok | 9 | 0 |
| 23 | `tests/test_session_restore_workers.rs` | ok | 8 | 0 |
| 24 | `tests/test_specialist_schema_charge.rs` | ok | 4 | 0 |
| 25 | `tests/test_specialist_stream.rs` | ok | 10 | 0 |
| 26 | `tests/test_steer_history.rs` | ok | 9 | 0 |
| 27 | `tests/test_tool_name_literals.rs` | ok | 1 | 0 |
| 28 | `tests/test_ui_session.rs` | ok | 21 | 0 |
| 29 | `tests/test_validation_loop.rs` | ok | 14 | 0 |
| 30 | `Doc-tests marmennill` (`running 0 tests`) | ok | 0 | 0 |
| | **TOTAL** | **6 failures** | **1026** | **6** |

Lib failures in this run (verbatim):

```
failures:
    harness::pty::tests::test_harness_pty_process_group_kill
    harness::pty::tests::test_interactive_pty_manager_lifecycle
    harness::pty::tests::test_run_command_parses_timeout_seconds_argument
    harness::pty::tests::test_run_command_pty_cancelled_by_worker_token
    orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation

test result: FAILED. 947 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 16.43s
```

Integration failures in this run (verbatim, the only one):

```
---- test_prompt_based_tool_gating stdout ----
thread 'test_prompt_based_tool_gating' (1179428) panicked at tests/test_role_gating.rs:312:5:
Validator with run_command in prompt must be permitted to execute run_command

failures:
    test_prompt_based_tool_gating

test result: FAILED. 24 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.60s
```

### 11.4.3 lib-suite stability probe (6 full-lib executions, same binary, no source change)

| Run | Command | Passed | Failed | Non-pty failures |
|---|---|---:|---:|---|
| A | `cargo test` (spec, fail-fast) | 944 | 8 | `notice_tests::…ask_followup`, `notice_tests::…success_and_validation`, `llm::client::tests::test_policy_refused_mcp_names_are_never_advertised`, `ui::tests::one_corrupt_worker_file_…` |
| B | `cargo test --no-fail-fast` | 947 | 5 | `notice_tests::…success_and_validation` |
| C | `cargo test --lib` (repeat 1) | 945 | 7 | `notice_tests::…ask_followup`, `notice_tests::…success_and_validation`, `notice_tests::test_task_id_address_matches_exact_task_id_only` |
| D | `cargo test --lib` (repeat 2) | 946 | 6 | `notice_tests::…ask_followup`, `notice_tests::…success_and_validation` |
| E | `cargo test --lib` (repeat 3) | 946 | 6 | `notice_tests::…success_and_validation`, `notice_tests::test_task_id_address_matches_exact_task_id_only` |
| F | `cargo test --lib -- --test-threads=1` (serial, 76.02 s) | 947 | 5 | `ui::tui::tests::capture_records_the_plan_pane_and_restore_puts_its_text_back` |

**`harness::pty::tests::*` failure count = 4 in every single run (A–F): unchanged vs the documented baseline** —
no drift, and the environment cause is reproduced on disk:

```
$ ls -l /dev/pts/ptmx
c--------- 1 root root 5, 2 Oct  4 19:40 /dev/pts/ptmx      # mode 000 → openpty EACCES
```

## 11.5 Failure classification — NEW vs pre-existing / environmental

The failure union over all 6 full-suite runs contains **9 distinct test names**. Every one is classified below.

| Failing test | Runs where it failed | Classification | Evidence / mechanism |
|---|---|---|---|
| `harness::pty::tests::test_harness_pty_process_group_kill` | A–F (6/6) | **PRE-EXISTING, ENVIRONMENTAL** (sanctioned) | `openpty: PermissionDenied` at `src/harness/pty_tests.rs:16:47`; `/dev/pts/ptmx` mode `000` (above). `docs/baseline_test_run.md:138-140`, `:214`, `:222`. Count 4/4 stable. |
| `harness::pty::tests::test_interactive_pty_manager_lifecycle` | A–F (6/6) | **PRE-EXISTING, ENVIRONMENTAL** (sanctioned) | same EACCES, `src/harness/pty_tests.rs:120:10`. |
| `harness::pty::tests::test_run_command_parses_timeout_seconds_argument` | A–F (6/6) | **PRE-EXISTING, ENVIRONMENTAL** (sanctioned) | same EACCES, `src/harness/pty_tests.rs:102:34`. |
| `harness::pty::tests::test_run_command_pty_cancelled_by_worker_token` | A–F (6/6) | **PRE-EXISTING, ENVIRONMENTAL** (sanctioned, cascade of the EACCES) | `assertion failed: res.is_ok()` at `src/harness/pty_tests.rs:179:5`; the PTY it needs cannot be opened. |
| `tests/test_role_gating.rs::test_prompt_based_tool_gating` | both suite runs (integration) | **PRE-EXISTING, DETERMINISTIC** (sanctioned) | `tests/test_role_gating.rs:312:5`, same name/line/message as `docs/baseline_test_run.md:151,180,198,222`. Tool-dispatch gating, no LLM transport, no message roles. |
| `orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation` | A, B, C, D, E (5/6) | **PRE-EXISTING FLAKY** (global-state contention) | rotating membership across runs (`Event::SteerResponse … Disconnected`, `src/orchestrator/notice_tests.rs:457`); passes serially (F); documented flaky in `docs/baseline_test_run.md:175-177` and §4 above. |
| `orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup` | A, C, D (3/6) | **PRE-EXISTING FLAKY** (same global event-sender race) | `src/orchestrator/notice_tests.rs:547:18` `Expected SteerResponse, got SubagentMessage{…}`; vanishes in runs B/E/F. |
| `orchestrator::notice_tests::test_task_id_address_matches_exact_task_id_only` | C, E (2/6) | **PRE-EXISTING FLAKY** (same family, rotating member) | appears/disappears between identical runs. |
| `llm::client::tests::test_policy_refused_mcp_names_are_never_advertised` | A only (1/6) | **NOT NEW — flaky (outside the sanctioned list, investigated)** | (i) passes **5/5 in isolation** and `ok` inside `cargo test --lib llm::client` (22/22, §11.6) and in runs B–F; (ii) the failure mode is the scripted MCP server's `myserver__do_thing` missing from the advertised list — the test swaps the **process-global MCP registry** under `MCP_REGISTRY_LOCK` (`src/llm/client_tests.rs:680`) and boots `sh -c "cat … ; sleep 30"` fixtures (`:660-676`), i.e. handshake/registry timing; (iii) it calls `build_request(&cfg, Vec::new())` — an **empty** message vector — so the changed hunk (a loop over `req_body.messages` inside `ChatClient::try_chat_once`) is unreachable from this test. Integrity note: the earlier §4 claim that the baseline "documents it as occasionally flaky" is *not* substantiated (`grep` finds no occurrence in `docs/baseline_test_run.md`), so it was verified directly here instead. |
| `ui::tests::one_corrupt_worker_file_is_the_tolerated_partial_and_the_rest_installs` | A only (1/6) | **NOT NEW — cross-test global-state contention (outside the sanctioned list, investigated)** | Observed assertion `src/ui/session_tests.rs:2772:9` `left: "Aborted"` / `right: "Interrupted by restart"` (`INTERRUPTED_STATUS_LABEL`). Mechanism is process-global: `cancel_all_active_workers()` (`src/orchestrator/workers.rs:981-1001`) walks the whole `WORKERS` registry and stamps **every** active worker with `"Aborted"`; it is reached concurrently by sibling lib tests (`src/orchestrator/workers.rs:1179, 1345, 1448, 1576, 1621`), by `orchestrator::bus::cancel_all()` (`src/orchestrator/bus.rs:38-43`) and by `HarnessMonitor::abort()` (`src/orchestrator/mod.rs:527-529`). The test installs its workers under fixed keys (`t022-partial-good-a/b/bad`, `src/ui/session_tests.rs:2647-2649`) and asserts their label ~0.4 s later — a race with any concurrent abort sweep. Passes **5/5 in isolation**, 5/5 when paired with `orchestrator::workers::tests::test_cancel_active_worker_and_cancel_all` at `--test-threads=2`, and in runs B–F. Its body (`:2644-2800`) contains **no** `ChatClient`/`try_chat_once` reference, so the changed hunk is unreachable. |
| `ui::tui::tests::capture_records_the_plan_pane_and_restore_puts_its_text_back` | F only (1/6, serial run) | **NOT NEW — load/timing-sensitive test (outside the sanctioned list, investigated)** | Panic at `src/ui/tui/tests.rs:2621:5`: `plan start position TimePoint { elapsed_secs: 17.214638526, … }` vs the assertion `start.elapsed_secs >= 0.0 && start.elapsed_secs < 5.0`. `elapsed_secs` is, by the test's own comment, "a position on the process timeline", so the assertion is a function of **binary uptime**: it can only pass while the test binary is < 5 s old. It failed exactly in the run whose binary ran 76.02 s (serial) and passed in all ~16 s parallel runs and **5/5 in isolation**. `src/ui/tui/tests.rs` contains **0** `ChatClient` references. Pre-existing latent timing flake; will bite CI on loaded runners regardless of this change. |

**Comparison against the documented baseline:** the baseline additionally recorded unsanctioned lib failures
(`harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable`,
`harness::tool_resolution_tests::*` ×2, `tool_names::tests::claimed_aliases_never_fall_through_to_unknown_tool`,
`ui::bridge::tests::test_bridge_notice_capacity_drop_…`) — **none of them appeared in any of the 6 runs above**,
so the tree is strictly no worse (and in that respect better) than `docs/baseline_test_run.md`.

**NEW failures attributable to the change set (`src/llm/client.rs` hunk + 8 new tests + formatting): 0.**
Supporting facts: the production diff is a single hunk confined to message-role normalization inside
`ChatClient::try_chat_once` (`git diff -- src/llm/client.rs` = one hunk, `@@ -537,7 +537,31 @@`); the 4 new lib
tests and the 4 new integration tests all pass; no run produced a failure in any `llm::client` role test, any
`tests/test_rebirth_system_role.rs` test, or any session-restore / tool-pairing suite.

## 11.6 The 8 new tests for the system-role fix — all green

```
$ cargo test --lib llm::client
running 22 tests
test llm::client::tests::test_llm_first_position_keeps_its_role_when_it_is_not_system ... ok
test llm::client::tests::test_llm_only_the_first_of_consecutive_system_messages_stays_system ... ok
test llm::client::tests::test_llm_mid_history_system_message_is_demoted_to_user_on_the_wire ... ok
test llm::client::tests::test_llm_system_demotion_mutates_only_the_cloned_payload_not_the_callers_history ... ok
test llm::client::tests::test_policy_refused_mcp_names_are_never_advertised ... ok
... (17 other llm::client tests, all `ok`)
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 930 filtered out; finished in 5.51s
```

| # | New lib test (`cargo test --lib llm::client`) | Status |
|---|---|---|
| 1 | `llm::client::tests::test_llm_mid_history_system_message_is_demoted_to_user_on_the_wire` | **ok** |
| 2 | `llm::client::tests::test_llm_first_position_keeps_its_role_when_it_is_not_system` | **ok** |
| 3 | `llm::client::tests::test_llm_only_the_first_of_consecutive_system_messages_stays_system` | **ok** |
| 4 | `llm::client::tests::test_llm_system_demotion_mutates_only_the_cloned_payload_not_the_callers_history` | **ok** |

```
$ cargo test --test test_rebirth_system_role
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.25s
     Running tests/test_rebirth_system_role.rs (target/debug/deps/test_rebirth_system_role-b2dac7c0fa9e2f50)

running 4 tests
test test_restored_legacy_system_checkpoint_stays_detectable_in_memory ... ok
test test_every_mid_history_system_turn_is_demoted_and_index_zero_is_untouched ... ok
test test_restored_legacy_system_checkpoint_goes_out_as_user ... ok
test test_reborn_history_keeps_pin_contract_and_sends_checkpoint_as_user ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.52s
exit=0
```

| # | New integration test (`cargo test --test test_rebirth_system_role`) | Status |
|---|---|---|
| 5 | `test_restored_legacy_system_checkpoint_goes_out_as_user` | **ok** |
| 6 | `test_restored_legacy_system_checkpoint_stays_detectable_in_memory` | **ok** |
| 7 | `test_every_mid_history_system_turn_is_demoted_and_index_zero_is_untouched` | **ok** |
| 8 | `test_reborn_history_keeps_pin_contract_and_sends_checkpoint_as_user` | **ok** |

**8 / 8 pass.** The formatted file (`408` lines, mtime `2026-10-15 03:12:36`) is the one that ran; formatting did not
alter test semantics (the negative control in §5 already proved the 8 are genuine regression guards, and their
assertions are unchanged by `rustfmt`).

## 11.7 CI-coverage warning — the change set is still UNTRACKED / UNCOMMITTED

```
$ git status --porcelain
 M src/llm/client.rs
 M src/llm/client_tests.rs
?? .marmel.old.3/
?? .marmel.old/
?? .marmel.old2/
?? bugreport.md
?? docs/lint_inventory.md
?? docs/research_message_wire_types.md
?? docs/research_rebirth_checkpoint_roles.md
?? docs/research_request_boundary.md
?? docs/verification_report.md
?? marmel.toml.df
?? tests/test_rebirth_system_role.rs
```

* **`tests/test_rebirth_system_role.rs` is UNTRACKED (`??`).** CI checks out the repository with
  `actions/checkout@v4`, so this file — and therefore all 4 new integration tests verified in §11.6 — is **not part of
  any CI run until it is `git add`ed and committed**. Everything green here for that target is local-only evidence.
* **`src/llm/client.rs` and `src/llm/client_tests.rs` are modified but unstaged (` M`).** The production fix and the
  4 new lib tests are likewise absent from CI until staged and committed.
* **The `docs/*` artifacts of this workstream are UNTRACKED**: `docs/verification_report.md` (this audit),
  `docs/lint_inventory.md`, `docs/research_message_wire_types.md`, `docs/research_rebirth_checkpoint_roles.md`,
  `docs/research_request_boundary.md` (plus `bugreport.md` and `marmel.toml.df`). They must be `git add`ed to be part
  of the repository history; `.marmel.old*/` are scratch directories and should stay out (or be ignored).
* Suggested action (not performed here — this task is read-only for everything but this report):
  `git add src/llm/client.rs src/llm/client_tests.rs tests/test_rebirth_system_role.rs docs/verification_report.md docs/lint_inventory.md docs/research_*.md bugreport.md`
  then re-verify on the pushed branch so the CI `lint` + `test` legs actually gate the change.

## 11.8 Final statement

| Gate | Command (identical to `ci.yml`) | Exit | Diagnostics | Verdict |
|---|---|---:|---|---|
| Format | `cargo fmt --all -- --check` | 0 | none (no output) | **fmt-clean** |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | 0 | 0 | **clippy-clean** |
| Build | `cargo build --all-targets` | 0 | 0 warnings | clean |
| Tests | `cargo test` / `cargo test --no-fail-fast` | 101 | 6 failures in the no-fail-fast run (4 pty + 1 `notice_tests` + 1 sanctioned `test_prompt_based_tool_gating`) | **0 NEW failures** |

**The workspace is fmt-clean, clippy-clean and regression-free.** Both CI lint gates pass with zero diagnostics from a
clean package state; the full 30-target suite runs 1032 tests with 1026 passing; the `harness::pty::*` environmental
failure count is unchanged at exactly 4 across six executions; the only deterministic failures are the two documented
baseline entries; and every additional failure observed in any run was reproduced as isolation-passing, order- or
load-dependent flakiness with a documented process-global mechanism, and none of them can reach the changed hunk.
Remaining risk is process-hygiene, not code: the change set and its tests are still untracked/uncommitted, so CI does
not yet gate them (§11.7).
