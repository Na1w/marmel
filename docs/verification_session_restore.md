# Session-restore verification report — FINAL (t-036 refresh of the t-016 scaffold)

**Supersedes the numbers of the t-016 scaffold, every one of which was `STATIC` or `TBD-t034`.** All headline numbers
below were **measured in this session** against the tree that is about to be shipped (post-t-037), on
`cargo 1.98.0` / `rustc 1.98.0`, and are labelled `MEASURED-t036`. Numbers inherited from the t-034 re-gate are
labelled `MEASURED-t034`; anything derived only by reading the tree is labelled `STATIC`. A verdict is issued (§7).

**Provenance.** Sources of truth are `docs/verification_t034.md` (249-line final re-gate), `docs/verification_t014f.md`,
`docs/cleanup_t015b.md`, `docs/cleanup_t026.md`, `docs/cleanup_t035.md`, `docs/cleanup_t037.md`. Where a number could
have gone stale after t-037 (t-037 changed two string literals in `src/agents/runner/fix_loop.rs`), the gate was
**re-run in this session** rather than quoted: every per-target number in §2 and the whole-tree figure in §2.3 are
`t036` runs. Raw captures live in `.marmel/t036_logs/` (`whole_tree.log`, `clippy.log`, `fmt_check.txt`,
`rustfmt_27_check.txt`, `lib_filters.log`, `panel_abort_x2.log`, `session_targets.log`) — runtime logs, not source
edits; **t-036 edited no `src/` or `tests/` file** (`git status` unchanged apart from the mission's own pre-existing
working-tree edits).

---

## 1. Scope & method

What the mission covers (anchors re-read from the tree in this run; `STATIC` unless a run backs them):

- **Persistence core `src/session/`** — `mod.rs` (`EXIT_RESTORE_REFUSED = 3` at `src/session/mod.rs:50`; single path
  authority `bundle_dir` at `:57`), `commit.rs` (bundle write protocol, `bundle_dir` at `:73`, and since t-035 the
  production commit-seam guard `check_time_points` at `:648`, called on every commit at `:730` — see §3, AC-T2),
  `restore.rs` (verify-before-parse, `normalize` at `:516`, N6 witness cross-check at `:355`), `manager_capture.rs`
  (`LEGACY_TRANSCRIPT_FILE` at `:46`), `worker_journal.rs` (registry rebuild `install_restored_workers` at `:717`,
  notice projections `pending_notice_notices` at `:995`), `time.rs`, and the format layer
  `src/session/format/{envelope,messages,mod,restore,ui,worker}.rs` (`SCHEMA_VERSION = 1` at `src/session/format/mod.rs:49`,
  unchanged).
- **Normalization N0–N6** — `src/session/restore.rs:494` (doc block) + `:516` (`normalize`) +
  `docs/design_session_restore_data.md:1345` (§8.2 pass order); loudness/report rules §8.4
  (`docs/design_session_restore_data.md:1401`, `:1406`).
- **Worker journal + registry rebuild** — `src/session/worker_journal.rs` (projection `:591`/`:624`, notices
  `:844-851`, `:995-1008`, `:1032-1033`), design §5 (`docs/design_session_restore_data.md:909-1003`).
- **`ContextEngine` restore seams** — `src/manager/context.rs:1151` (`install_restored_messages`), `:1170`
  (`restore_engine_flags`); pin rule R-PIN-2 (`docs/design_session_restore_data.md:863`).
- **`Renderer::capture_ui / restore_panes / announce_restore`** — trait defaults `src/ui/mod.rs:152`, `:164`, `:173`;
  TUI impl `src/ui/tui/mod.rs:1084`, `:1195`, `:1360` (one line higher than the scaffold recorded); raw impl
  `src/ui/raw.rs:194`, `:228`, `:326`; boot call order `src/ui/session.rs:1233-1290` (`capture_ui` `:1233`,
  `restore_panes` `:1234`, capture `:1285`, `announce_restore` `:1290`).
- **Restore-report plumbing** — `clamp_warning_sentences` `src/ui/tui/mod.rs:1533`, `token_witness_warnings`
  `:1582`, `subagent_detail_from_panel` `:1495`; session-side mirrors `token_witness_warnings` `src/ui/session.rs:1540`,
  `scroll_clamp_warnings` `:1568`, `subagent_detail_from_panel` `:1603`; join-handle notice emitter
  `src/ui/session.rs:~1255-1274` (`push_repair_notice` at `:1259`). **The stale comment anchors the scaffold flagged
  (§6.1) are still stale** — re-read in this run they are off by **1** and **6** lines now; cosmetic only.
- **21-field `UiSection` (t-020)** — `src/session/format/ui.rs`: 7 original + 14 appended `#[serde(default)]` fields
  = 21 fields, `SCHEMA_VERSION` unchanged (`STATIC`).
- **Boot pass** — `src/ui/session.rs:1076` (`boot`), phase seam `Plan::session_bundle_path()` `src/manager/phase.rs:392`,
  refusal → exit 3 mapping `src/main.rs:105`, panic hook `src/main.rs:353`.
- **`SessionCommitPump` / triggers** — `src/ui/session.rs:295` (`CommitTrigger`), `:366` (`SessionCommitLedger`),
  `:472` (`SessionCommitPump`).
- **Docs** — `docs/design_session_restore_data.md`, `docs/design_session_restore_flow.md`, `docs/session_restore.md`,
  `docs/audit_static_t014f_part1.md`, `docs/audit_static_t014f_part2.md`, plus the mission cleanup notes
  `docs/cleanup_t015b.md`, `docs/cleanup_t026.md`, `docs/cleanup_t032.md`, `docs/cleanup_t035.md`,
  `docs/cleanup_t037.md`, `docs/verification_t014f.md`, `docs/verification_t034.md`.

**Method actually available now (this is the scaffold's biggest correction):** a working toolchain. `cargo check`,
`cargo clippy`, `cargo test` (per-target and whole-tree, parallel and serial) and `rustfmt --check` were all executed
in this session; `run_command` no longer dies at spawn.

**Formatting state.** `rustfmt --edition 2024 --config skip_children=true --check` over exactly the 27 mission-listed
files is a **no-op**: exit 0, 0 bytes of diff (`MEASURED-t036`, `.marmel/t036_logs/rustfmt_27_check.txt`). At t-034 the
same command *applied* whitespace-only changes to **13 of the 27** files (`docs/verification_t034.md` §3.1) and the
post-format re-gate proved nothing observable changed. Repo-wide `cargo fmt -- --check` still exits 1 with **25 hunks
in 2 files** — `src/ui/raw.rs` (7) and `src/ui/tui/tests.rs` (18), 319 output lines (`MEASURED-t036`,
`.marmel/t036_logs/fmt_check.txt`) — deliberately untouched, since they are pre-existing and outside the mission's
file list. **The "138 pre-existing repo-wide diffs" figure carried by earlier briefs does not match this tree** and
should not be quoted; t-034 §3.3 reached the same conclusion.

---

## 2. Gate ledger and per-target results (all `MEASURED-t036` on the shipped tree)

### 2.1 Build and lint gates

| Command | Exit | Result |
|---|---|---|
| `cargo check --all-targets` | **0** | clean (0 errors, 0 warnings) |
| `cargo clippy --all-targets --all-features -- -D warnings` | **0** | 0 diagnostics |
| `rustfmt --edition 2024 --config skip_children=true --check` (27 mission files) | **0** | no-op, 0-byte diff |
| `cargo fmt -- --check` (whole repo, **not** a mission gate) | 1 | 25 hunks / 2 files, pre-existing (§1) |

### 2.2 Session-related targets — every target, current numbers

| Target (command `cargo test --test <name>`) | passed | failed | ignored | exit |
|---|---|---|---|---|
| `test_session_restore_panel_abort` (run **serial**: `-- --test-threads=1`, twice) | **10** | **0** | **0** | 0 |
| `test_session_restore_ui` | **9** | **0** | **0** | 0 |
| `test_session_config` (35 also serial `-- --test-threads=1`) | **35** | **0** | **0** | 0 |
| `test_tool_name_literals` | **1** | **0** | **0** | 0 |
| `test_session_bundle` | **22** | **0** | **0** | 0 |
| `test_session_boot` | **6** | **0** | **0** | 0 |
| `test_session_commit` | **9** | **0** | **0** | 0 |
| `test_session_abort_flush` | **6** | **0** | **0** | 0 |
| `test_session_restore_manager` | **8** | **0** | **0** | 0 |
| `test_session_restore_dangling` | **12** | **0** | **0** | 0 |
| `test_session_restore_workers` | **8** | **0** | **0** | 0 |
| `test_ui_session` | **21** | **0** | **0** | 0 |
| **Session-target subtotal** | **147** | **0** | **0** | — |

`panel_abort` is run serially (it drives process/abort timing), and both serial runs were green: `10/0/0`, `10/0/0`
(`.marmel/t036_logs/panel_abort_x2.log`). `test_session_config` is hermetic (t-015b) and green both parallel and
serial.

### 2.3 `--lib` module filters (same tree, fresh in t-036)

| Command | passed | failed | ignored | filtered out |
|---|---|---|---|---|
| `cargo test --lib -- session::` | **66** | 0 | 0 | 882 |
| `cargo test --lib -- ui::` | **196** | 0 | 0 | 752 |
| `cargo test --lib -- manager::` | **76** | 0 | 0 | 872 |
| `cargo test --lib -- agents::` | **60** | 0 | 0 | 888 |

Lib total in this tree: **948 declared tests** (66 + 196 + 76 + 60 = 398 session/UI/manager/agents-relevant).

### 2.4 Whole tree — `cargo test --no-fail-fast` (the number that describes the shipped tree)

**1217 passed / 7 failed / 0 ignored, exit 101, across 29 targets** (lib + bin + 26 integration targets + doctests).
Per target, from `.marmel/t036_logs/whole_tree.log`:

```
--lib 942/6 FAILED   src/main.rs (bin) 14/0   test_agent 2/0    test_context 8/0    test_dynamic_prompts 4/0
test_harness 15/0    test_llm 5/0             test_midturn_notice 3/0              test_monitor 2/0
test_orchestrator 8/0                        test_plan_checkoff 6/0               test_role_gating 24/1 FAILED
test_session_abort_flush 6/0   test_session_boot 6/0   test_session_bundle 22/0   test_session_commit 9/0
test_session_config 35/0       test_session_restore_dangling 12/0                 test_session_restore_manager 8/0
test_session_restore_panel_abort 10/0        test_session_restore_ui 9/0          test_session_restore_workers 8/0
test_specialist_schema_charge 4/0            test_specialist_stream 10/0          test_steer_history 9/0
test_tool_name_literals 1/0    test_ui_session 21/0     test_validation_loop 14/0 Doc-tests 0/0
TOTAL: 1217 passed / 7 failed / 0 ignored
```

**Reconciliation with t-034's 1216/8 (arithmetic checked, and it holds):** t-034's post-format whole-tree figure was
**1216 passed / 8 failed** with `test_role_gating 23/2` and `--lib 942/6`. t-037 then fixed one of those two
role_gating failures by rewording two literals, so in this tree `test_role_gating` runs **24/1**: one failing test
became a passing test, which is exactly `+1 passed / −1 failed` → **1217 / 7**. The two whole-tree runs therefore
describe the same 29-target tree modulo t-037, and the t-034 figure **predates t-037**; the figure to ship against is
**1217 / 7**, re-measured here.

**Honest flake caveat on the exact count.** The whole-tree failure *count* is not a stable constant, because two of the
seven are order-dependent async flakes (§5). Failure **membership** in `--lib` moved across five t-036 `cargo test --lib`
runs: 943/5, 944/4, 943/5, 943/5, 943/5 (the varying member was
`orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation` twice,
`orchestrator::notice_tests::…ask_followup…` once, and none once); the whole-tree run quoted above landed at 942/6.
The **deterministic core is 5** (4 × `harness::pty::tests::*` + `test_role_gating::test_prompt_based_tool_gating`);
everything above 5 in any given run is a registered flake that passes in isolation.

---

## 3. Acceptance-criteria map (`docs/design_session_restore_data.md` §9)

AC groups and their design anchors (re-read, `STATIC`): §9 at `:1423`; 9.1 AC-M1–8 `:1429`, 9.2 AC-P1–5 `:1440`,
9.3 AC-W1–4 `:1448`, 9.4 AC-I1–4 `:1455`, 9.5 AC-U1–4 `:1462`, 9.6 AC-C1–4 `:1469`, 9.7 AC-S1–4 `:1476`,
9.8 AC-T1–2 `:1483`, 9.9 AC-A1–3 `:1488`, 9.10 AC-D1–7 `:1495`. `fn` line numbers below were re-grepped in this
session and **differ from the scaffold's** where t-026/t-035 inserted code. Status column is the *measured* verdict:
every one of the 45 criteria is proven by a test that **executed green** in §2 (47 witness functions = 8 + 12 + 8 + 9 + 10,
i.e. exactly the executed tallies).

| AC | Witness test (file:line of `fn`) | Status |
|---|---|---|
| AC-M1 | `tests/test_session_restore_manager.rs:335` `ac_m1_restored_manager_history_equals_committed_element_by_element` | **PASS** |
| AC-M2 | `:396` `ac_m2_content_none_vs_empty_string_survive` | **PASS** |
| AC-M3 | `:487` `ac_m3_reasoning_content_survives` | **PASS** |
| AC-M4 | `:571` `ac_m4_tool_calls_survive_verbatim` | **PASS** |
| AC-M5 | `:720` `ac_m5_tool_results_survive_byte_identical` | **PASS** |
| AC-M6 | `:836` `ac_m6_no_re_role_and_no_illegal_reorder` | **PASS** |
| AC-M7 | `:1006` `ac_m7_legacy_advisory_text_and_system_checkpoint_are_opaque` | **PASS** |
| AC-M8 | `:1112` `ac_m8_engine_scalars_survive_and_budget_drift_is_reported` | **PASS** (target 8/0/0) |
| AC-P1 | `tests/test_session_restore_dangling.rs:683` `ac_p1_pinned_pair_survives_the_full_boot_path_byte_for_byte` | **PASS** |
| AC-P2 | `:798` `ac_p2_pin_witness_agrees_with_the_installed_vector` | **PASS** |
| AC-P3 | `:856` `ac_p3_recomputed_pinned_prefix_end_matches_the_persisted_value` | **PASS** |
| AC-P4 | `:928` `ac_p4_the_prompt_this_boot_composed_is_not_the_pinned_bytes` | **PASS** |
| AC-P5 | `:1002` `ac_p5_a_hand_edited_pin_is_refused_and_no_byte_of_the_bundle_changes` | **PASS** |
| AC-W1 | `tests/test_session_restore_workers.rs:620` `ac_w1_the_restored_key_set_equals_the_committed_set_…` | **PASS** |
| AC-W2 | `:780` `ac_w2_history_is_element_by_element_equal_only_for_a_live_mirror_…` | **PASS** |
| AC-W3 | `:973` `ac_w3_every_declared_field_is_restored_verbatim_and_an_unrecognized_label_keeps_its_bytes` | **PASS** |
| AC-W4 | `:1221` `ac_w4_the_live_registry_holds_each_restored_key_with_its_stored_completed_seq` | **PASS** |
| AC-I1 | `:1466` `ac_i1_no_id_below_the_worker_id_floor_is_re_issued_…` | **PASS** |
| AC-I2 | `:1682` `ac_i2_a_completion_in_the_new_process_draws_above_every_restored_completed_seq_and_order_holds` | **PASS** |
| AC-I3 | `:1844` `ac_i3_a_notice_issued_after_restore_never_aliases_a_pre_crash_notice_id` | **PASS** |
| AC-I4 | `:1956` `ac_i4_the_session_id_survives_the_restore_and_the_next_commit_writes_the_same_id` | **PASS** (target 8/0/0) |
| AC-U1 | `tests/test_session_restore_ui.rs:597` `ac_u1_pane_vector_equals_the_committed_vector_…before_the_first_draw` | **PASS** |
| AC-U2 | `:711` `ac_u2_status_line_is_the_committed_bar_and_never_the_default_ready` | **PASS** |
| AC-U3 | `:778` `ac_u3_no_pane_line_is_derived_from_the_journal_and_the_steering_pairs_survive` | **PASS** |
| AC-U4 | `:906` `ac_u4_cli_only_lines_survive_restore_although_the_journal_never_recorded_them` | **PASS** |
| AC-C1 | `:1036` `ac_c1_composer_text_and_caret_come_back_on_the_same_char_boundary` | **PASS** |
| AC-C2 | `:1113` `ac_c2_composer_history_history_index_and_input_draft_match` | **PASS** |
| AC-C3 | `:1192` `ac_c3_view_offsets_selection_and_toggles_match_the_committed_view` **and** `:1330` `ac_c3_scroll_offsets_that_the_live_clamp_moved_are_reported_as_warnings` | **PASS — both branches, as real executed tests** (see note below) |
| AC-C4 | `:1413` `ac_c4_out_of_range_caret_selection_or_unknown_panel_refuses_and_leaves_bytes_untouched` | **PASS** (target 9/0/0) |
| AC-S1 | `tests/test_session_restore_panel_abort.rs:961` `ac_s1_restored_panel_vector_equals_the_committed_one_in_length_and_order` | **PASS** |
| AC-S2 | `:1185` `ac_s2_worked_duration_secs_is_carried_and_never_recomputed_from_the_two_clocks` | **PASS** |
| AC-S3 | `:1344` `ac_s3_panel_clocks_re_anchor_within_the_error_term_and_their_wall_bytes_are_exact` | **PASS** |
| AC-S4 | `:1532` `ac_s4_an_active_panel_restores_its_byte_and_emits_a_join_handle_notice_naming_it` | **PASS — as a real executed test** (see note) |
| AC-T1 | `:1596` `ac_t1_every_restored_offset_is_measured_against_the_anchor_the_manifest_names` | **PASS** |
| AC-T2 | `:1944` `ac_t2_a_non_finite_or_negative_offset_is_refused_and_never_defaulted_to_now` | **PASS — now enforced in production** (see note) |
| AC-A1 | `:2143` `ac_a1_abort_mid_subagent_commits_user_abort_and_comes_back_as_it_was` | **PASS** |
| AC-A2 | `:2360` `ac_a2_the_notices_are_present_specific_and_the_outcome_is_restored_not_complete` **and** `:2507` `ac_a2_capture_side_records_the_notice_queued_at_abort_time` | **PASS — both sides, as real executed tests** (see note) |
| AC-A3 | `:2550` `ac_a3_the_restored_token_is_not_the_old_one_and_nothing_in_the_restore_re_launches_it` | **PASS** (target 10/0/0 serial) |
| AC-D1 | `tests/test_session_restore_dangling.rs:1090` `ac_d1_the_dangling_pair_is_reported_and_healed_with_one_named_placeholder` | **PASS** |
| AC-D2 | `:1229` `ac_d2_the_unanswered_assistant_turn_survives_with_content_still_none` | **PASS** |
| AC-D3 | `:1304` `ac_d3_the_pins_keep_their_indices_and_bytes_through_the_synthesis` | **PASS** |
| AC-D4 | `:1382` `ac_d4_a_dispatched_call_becomes_a_pty_notice_and_is_never_re_run` | **PASS** |
| AC-D5 | `:1534` `ac_d5_an_orphan_tool_result_is_pruned_counted_and_named` | **PASS** |
| AC-D6 | `:1623` `ac_d6_the_bundle_is_byte_identical_after_restore_and_only_the_next_commit_rewrites` | **PASS** |
| AC-D7 | `:1700` `ac_d7_two_corrupt_workers_refuse_one_corrupt_worker_degrades_loudly` | **PASS** (target 12/0/0) |

**The four criteria that used to be open gaps are now closed, and that is the headline of this section.** The scaffold
(§3, `docs/verification_session_restore.md` as written at t-016) carried **three `#[ignore]`d witnesses** and one
never-executed probe. All four are now **real tests that execute in the ordinary gate and pass**:

- **AC-S4** — the `#[ignore = …]` at the old `tests/test_session_restore_panel_abort.rs:1503` was **dropped by t-035**
  (`docs/cleanup_t035.md`); the witness `ac_s4_…` (`:1532`) ran green in both serial runs of §2.2. Emitters:
  `src/session/worker_journal.rs:844-851` + `src/ui/session.rs:~1255-1274`.
- **AC-A2 capture side** — the `#[ignore = …]` at the old `:2451` was likewise **dropped by t-035**;
  `ac_a2_capture_side_records_the_notice_queued_at_abort_time` (`:2507`) ran green. Chain:
  `worker_journal.rs:624` → `:995-1008` → `src/orchestrator/notice.rs:285-292`.
- **AC-C3 branch 2 (§6.6 I-U6 clamp)** — the `#[ignore = …]` at the old `tests/test_session_restore_ui.rs:1327` was
  **dropped by t-026** (`docs/cleanup_t026.md`); `ac_c3_scroll_offsets_that_the_live_clamp_moved_are_reported_as_warnings`
  (`:1330`) ran green, which is why that target moved from `8/0/1` to **9/0/0**.
- **AC-T2** — this one was never `#[ignore]`d; it was **RED**: a non-finite/negative time point was silently defaulted
  by serde_json instead of being refused. t-035 added the production guard `check_time_points`
  (`src/session/commit.rs:648`, called from the commit path at `:730`), and `ac_t2_…` (`:1944`) now passes.

**Zero `#[ignore]` attributes remain anywhere in the mission's test targets or in `src/`** — verified by
`grep '#\[ignore'` in this session: **0 attributes**, 8 prose-only mentions (doc comments recording that the ignores
were dropped). No criterion is "written but unproven".

Protocol / boot / pump / ending coverage that §9 does not itself enumerate, all green:
`tests/test_session_bundle.rs` (22, incl. `exactly_one_corrupt_worker_…`, cited by
`docs/design_session_restore_data.md:1417`), `tests/test_session_boot.rs` (6 — and these **do** drive the real binary
via `CARGO_BIN_EXE_marmel`, `tests/test_session_boot.rs:223`), `tests/test_session_commit.rs` (9),
`tests/test_session_abort_flush.rs` (6), `tests/test_session_config.rs` (35, t-015b), `tests/test_ui_session.rs` (21).

---

## 4. Test-target inventory — declared counts vs. executed tallies (now cross-checked, not static-only)

The scaffold could only *count attributes*. In this tree the attribute counts and the executed tallies agree exactly,
so the inventory is no longer a prediction:

| Target | `#[test]` | `#[tokio::test]` | `#[ignore]` | declared | **executed (t-036)** |
|---|---|---|---|---|---|
| `tests/test_session_bundle.rs` | 22 | 0 | 0 | 22 | **22 / 0 / 0** |
| `tests/test_session_boot.rs` | 6 | 0 | 0 | 6 | **6 / 0 / 0** |
| `tests/test_session_commit.rs` | 0 | 9 | 0 | 9 | **9 / 0 / 0** |
| `tests/test_session_abort_flush.rs` | 0 | 6 | 0 | 6 | **6 / 0 / 0** |
| `tests/test_session_config.rs` (t-015b) | 35 | 0 | 0 | 35 | **35 / 0 / 0** |
| `tests/test_session_restore_manager.rs` | 8 | 0 | 0 | 8 | **8 / 0 / 0** |
| `tests/test_session_restore_dangling.rs` | 12 | 0 | 0 | 12 | **12 / 0 / 0** |
| `tests/test_session_restore_workers.rs` | 8 | 0 | 0 | 8 | **8 / 0 / 0** |
| `tests/test_session_restore_ui.rs` | 9 | 0 | **0** (was 1) | 9 | **9 / 0 / 0** |
| `tests/test_session_restore_panel_abort.rs` | 10 | 0 | **0** (was 2) | 10 | **10 / 0 / 0** |
| `tests/test_tool_name_literals.rs` | 1 | 0 | 0 | 1 | **1 / 0 / 0** |
| `tests/test_ui_session.rs` | 0 | 21 | 0 | 21 | **21 / 0 / 0** |
| **Total** | 111 | 36 | **0** | **147** | **147 / 0 / 0** |

In-crate module tallies are now **executed filter results**, not greps: `session::` **66**, `ui::` **196**,
`manager::` **76**, `agents::` **60** (§2.3) — the `ui::` figure confirms `docs/audit_static_t014f_part2.md:214`, and
the `manager::` figure confirms the scaffold's `STATIC` 76.

---

## 5. Known baseline failures, with isolation verdicts

The shipped tree's whole-tree run (§2.4) has **7 failures**. Each was re-run **in isolation, 3× per test**, in this
session:

| # | Failure | Isolation verdict (3 runs each) | Classification |
|---|---|---|---|
| 1 | `harness::pty::tests::test_harness_pty_process_group_kill` | **FAILS in isolation (3/3)** | environment-forced baseline |
| 2 | `harness::pty::tests::test_interactive_pty_manager_lifecycle` | **FAILS in isolation (3/3)** | environment-forced baseline |
| 3 | `harness::pty::tests::test_run_command_parses_timeout_seconds_argument` | **FAILS in isolation (3/3)** | environment-forced baseline |
| 4 | `harness::pty::tests::test_run_command_pty_cancelled_by_worker_token` | **FAILS in isolation (3/3)** | environment-forced baseline |
| 5 | `orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation` | **PASSES in isolation (3/3)** | tracing-race flake (order-dependent) |
| 6 | `orchestrator::workers::tests::test_restore_installer_keeps_keys_and_closes_through_completed_seq` | **PASSES in isolation (3/3)** | order-dependent flake — **mission-added test, not baseline** (see note) |
| 7 | `test_role_gating::test_prompt_based_tool_gating` (`tests/test_role_gating.rs:312`) | **FAILS in isolation (3/3)** | documented deterministic baseline: `docs/baseline_test_run.md:151`, `:180`, `:198` |

- **1–4 (PTY):** the sandbox denies `openpty`; `/dev/pts/ptmx` is mode `c---------`, so the harness gets
  `PermissionDenied`. They fail identically at pristine `HEAD` (`bbfb893`) and never touch session code.
  Sanctioned by `docs/baseline_test_run.md:138-141`, `:222`.
- **5:** registered baseline flake (`docs/baseline_test_run.md:145-146`, `:175-177`, "passes isolated → flaky").
- **6 — stated plainly so nothing is swept under the rug:** this test is **not** a pre-existing baseline failure. It was
  added by the mission's own `src/orchestrator/workers.rs` work (it does not exist at `HEAD bbfb893`), and it fails
  only in the parallel whole-tree `--lib` run — never in isolation. Together with #5 it belongs to one mechanism: the
  shared global test guards `TEST_WORKERS_MUTEX` (`src/orchestrator/workers.rs:152`) and the tokio
  `TEST_NOTICE_MUTEX` (`src/orchestrator/notice.rs:448`). The async guard-drop tests
  (`src/orchestrator/workers.rs:~1816-1913`) take only `TEST_NOTICE_MUTEX`, while the sync restore-installer / id-floor
  tests take `TEST_WORKERS_MUTEX` and read shared floors seeded at +500 (`completed_seq_floor`,
  `worker_id_seq_floor`), so interleaving can move a floor mid-test. Sampling in this session:
  `cargo test --lib -- orchestrator::workers::` failed 1 of 5 runs (`test_completion_leaves_no_stale_registry_entry`,
  another mission-visible test that passes in isolation); `--lib -- orchestrator::workers:: orchestrator::notice_tests::`
  failed once, passed once. The t-032 deferred fix **B(b)** (taking `TEST_NOTICE_MUTEX` inside
  `src/session/worker_journal.rs` inbox assertions — takes `TEST_WORKERS_MUTEX` at `:1568`, `:1695`, `:1723`, `:1797`
  but not the notice guard) is still unapplied; applying it is what would make this family deterministic.
- **7:** the only *deterministic* non-environmental failure, and it is pre-existing, documented baseline
  (`docs/baseline_test_run.md:151`, `:180`, `:198`).

**What is gone:** `test_role_gating::fix_loop_source_has_no_raw_tool_name_string_literals` — the one **NEW** failure
t-034 flagged as blocking (`docs/verification_t034.md` §4.1/§5) — is **fixed by t-037** and green in this run
(`test fix_loop_source_has_no_raw_tool_name_string_literals ... ok`; `test_role_gating` = **24/1/0**). The fix was two
string literals in the `src/agents/runner/fix_loop.rs` test module, `"(rebirth advisory)"` → `"(context advisory)"`
(`src/agents/runner/fix_loop.rs:1355`, `:1402`; no more raw tool-name substrings in that file). `docs/cleanup_t037.md`.

**Statement required by the brief:** of the 7 current failures, **6 are baseline/environment** (4 PTY + 2 registered
tracing/guard flakes) and **1 is deterministic baseline** (`test_prompt_based_tool_gating`). **Zero NEW failures remain
attributable to the session-restore work.** The one caveat worth shipping with is failure #6 above: a mission-*added*
orchestrator test that is order-dependent (passes in isolation, fails in the parallel whole-tree run). It is a test
hygiene issue in the orchestrator's shared-lock test harness — not a defect in `src/session/`, and it does not fail any
session-restore criterion.

---

## 6. Coverage gaps / not asserted (and what became of the scaffold's open items)

### 6.1 What is genuinely not asserted by `tests/`

- **The `src/main.rs` CLI-precedence path.** `--resume` / `--fresh` parsing (`src/main.rs:255`, `:256` inside
  `parse_args` at `:237`), the contradictory-flag / capture-off **usage error → `std::process::exit(2)`**
  (`src/main.rs:46`, `:247`, message built by `session_flag_error` at `:300`/`:302`), and the literal
  **`std::process::exit(marmennill::session::EXIT_RESTORE_REFUSED)`** (`src/main.rs:105`) live in the **binary** target.
  This is precisely the gap `docs/cleanup_t015b.md` §7 and the header of `tests/test_session_config.rs:35-52` document:
  an integration test under `tests/` links the library only, so those slices are unreachable *from that target*
  (`test_session_config` covers the precedence through the public handoff seam `ui::session::set_resume_request` /
  `resume_requested`, plus the constant `EXIT_RESTORE_REFUSED == 3`).
  **Stated precisely rather than overstated:** the binary paths *are* exercised from two places that are not
  `test_session_config` — the **14 `#[cfg(test)]` unit tests in `src/main.rs` (`mod tests` at `:396`), which run as the
  `src/main.rs` bin target (14/0/0 in §2.4)**, and the **6 `tests/test_session_boot.rs` tests, which spawn the real
  binary** (`env!("CARGO_BIN_EXE_marmel")`, `tests/test_session_boot.rs:223`) and assert `Some(2)` usage exits
  (`:305`, `:342`), `Some(EXIT_RESTORE_REFUSED)` refusals (`:371`, `:427`, with the `== 3` equality pinned at `:374`),
  and byte-identical bundles for refused runs. So the residual gap is narrow: **coverage of the parse/exit code is
  split across the bin-target unit tests and the binary-spawning boot target, and no single integration target owns
  the whole `--resume`/`--fresh` precedence chain end-to-end.**
- **`PartialConfig` / `merge` are module-private** (`src/config.rs:494` and `:543`; the anchors recorded in
  `docs/cleanup_t015b.md` §6/§7 — `:516-521`, `:605-621` — have shifted since t-015b and no longer resolve), so TOML
  behaviour is observable
  from `tests/` only through `load()` (`src/config.rs:360`). Because `load` always starts from `Config::default()`,
  `merge`'s `None` arms can only be asserted as "absent key keeps the *default*", never as "absent key keeps a
  pre-set non-default base value". Covered that way by `tests/test_session_config.rs`
  (`docs/cleanup_t015b.md` §6: six-keys `Some` arms, `None`-arm defaults, typed-mismatch rejection, unknown-key drop).
- **Deferred fix A (t-032) — still open, cosmetic only.** The stale comment anchors in `src/ui/session.rs` were
  re-read in this run and are **still wrong**, by a different margin than the scaffold recorded: `src/ui/session.rs:1293`
  and `:1537` cite ``(`ui/tui/mod.rs:1581`)`` while `TuiRenderer::token_witness_warnings` is at
  **`src/ui/tui/mod.rs:1582`** (off by **1**), and `src/ui/session.rs:1600` cites ``(`ui/tui/mod.rs:1489`)`` while
  `subagent_detail_from_panel` is at **`:1495`** (off by **6**). The third citation checked here,
  ``(`ui/tui/mod.rs:1066-1068`)`` at `src/ui/session.rs:1279`, **is** correct (the `selected_subagent_idx` re-clamp is
  at `src/ui/tui/mod.rs:1066-1067`). Nothing functional depends on these; the same-file mirrors are
  `token_witness_warnings` `src/ui/session.rs:1540` and `subagent_detail_from_panel` `:1603`.
  Source list: `docs/cleanup_t032.md:129-140`.
- **Deferred fix B(a) (t-032) — still open, and still latent.** `queued_notice_notices`
  (`src/session/worker_journal.rs:1032`) still opens `(0..count.max(1))` (`:1033`), so `count = 0` would fabricate one
  `PendingNotice`; the only production caller is guarded by `if queued > 0`. The fix and its two-assertion regression
  guard are spelled out in `docs/cleanup_t032.md:169-186`, and the marker is present in the source as
  `TODO(t-034)` at `src/session/worker_journal.rs:1029`. Not a correctness defect on any executed path.
- **Deferred fix B(b) (t-032) — still open**, and it is the mechanism behind §5 row 6: the `session::worker_journal`
  inbox assertions take `TEST_WORKERS_MUTEX` but never the tokio `TEST_NOTICE_MUTEX`.
- **Repo-wide formatting** is not clean and was not made clean on purpose: 25 pre-existing hunks in `src/ui/raw.rs` and
  `src/ui/tui/tests.rs` (§1). `cargo fmt` was not run in write mode by this task.

### 6.2 Scaffold claims corrected by this refresh

`TBD-t034` placeholders → measured (§2); "three ignored witnesses awaiting execution" → **0 ignores, all executed**
(§3); "`tests/test_session_config.rs` does not exist" → exists, 35/0/0 (t-015b); baseline table rows that were
"brief-asserted, no `docs/` copy" (`orchestrator::steer_tests::…`, `orchestrator::workers::…`,
`harness::dispatch_integrity_tests::…`, `ui::bridge::tests::…`) → **did not fire in this tree**;
"the +1 in `test_role_gating`" (`fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority`) → **passes**;
the "138 pre-existing `cargo fmt --check` diffs" and the "pristine HEAD = 815/5" figures → **not reproducible** in this
tree (25 hunks; whole-tree total is 1217/7) and should stop being quoted.

---

## 7. Verdict

**PASS.** On the shipped (post-t-037) tree, measured in this session: `cargo check --all-targets` **exit 0**;
`cargo clippy --all-targets --all-features -- -D warnings` **exit 0**; **all 12 session targets green — 147 passed /
0 failed / 0 ignored** (incl. `test_session_restore_panel_abort` 10/0/0 serial twice, `test_session_restore_ui`
9/0/0, `test_session_config` 35/0/0); `--lib` filters `session::` 66 / `ui::` 196 / `manager::` 76 / `agents::` 60, all
0 failed 0 ignored; whole tree **1217 passed / 7 failed / 0 ignored** across 29 targets, where the 7 are **6 baseline
(environment-forced PTY ×4 + registered guard/tracing flakes) + 1 deterministic documented baseline** and **0 are
attributable to the session-restore work**. Every acceptance criterion **AC-M1–8, AC-P1–5, AC-W1–4, AC-I1–4, AC-U1–4,
AC-C1–4, AC-S1–4, AC-T1–2, AC-A1–3, AC-D1–7 is proven by an executed, passing test — including AC-S4, AC-A2
(capture side), AC-C3 branch 2 and AC-T2, which were the four open gaps in the scaffold and are no longer ignored or
red.** The scoped `rustfmt` pass is a no-op on the mission's 27 files; 25 pre-existing repo-wide fmt hunks remain, by
design. No source or test file was edited by t-036, no commit was made, and `cargo fmt` was never run in write mode.
