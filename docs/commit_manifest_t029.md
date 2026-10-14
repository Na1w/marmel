# Commit preparation manifest — t-029 (BRANCH B, shell unavailable)

**Shell probe (run exactly ONCE, failed at spawn):** `ERROR: Unable to spawn
/home/fredrik/.cargo/bin/marmel (deleted) because it doesn't exist on the filesystem (ENOENT: No such file or directory)`

No other shell was attempted. Everything below comes from `glob` / `grep_search` / `read_file` against the
working tree. **Nothing was compiled, formatted, tested or committed.** Since `git` is unreachable,
new-vs-modified is inferred from module membership and mission evidence; branch A must confirm each entry with
`git status --porcelain` before staging (`git add <explicit path>` stages untracked and modified alike, so the
command block stays valid either way).

---

## (a) File inventory to stage

### A.1 NEW — persistence module (12 files)

```
src/session/mod.rs              pub mod commit/format/manager_capture/restore/time/worker_journal (:38-43);
                                EXIT_RESTORE_REFUSED = 3 (:50); bundle_dir (:57)
src/session/commit.rs           bundle write protocol; manifest-last commit point
src/session/manager_capture.rs  manager capture seam; LEGACY_TRANSCRIPT_FILE (:46)
src/session/restore.rs          verify-before-parse load_bundle (:72); normalize (:516)
src/session/time.rs             wall + monotonic anchors (anchor_wall :58)
src/session/worker_journal.rs   worker mirror/journal; install_restored_workers (:717)
src/session/format/mod.rs       BUNDLE_DIR_NAME, TimePoint, fingerprint
src/session/format/envelope.rs  manifest model (PinWitness, IdentityFloors)
src/session/format/messages.rs  MessageRecord / ToolCallRecord / ToolFunctionRecord
src/session/format/restore.rs   RepairReport / Refusal / RefusalCode / NonRestorableNotice / RestoreOutcome
src/session/format/ui.rs        UiSection / UiFile / SubagentPanel / ViewState
src/session/format/worker.rs    WorkerSection / WorkerFile / DanglingToolCallState / HistorySource
```

### A.2 NEW — mission test targets (10)

```
tests/test_session_abort_flush.rs   tests/test_session_boot.rs        tests/test_session_bundle.rs
tests/test_session_commit.rs        tests/test_session_restore_dangling.rs
tests/test_session_restore_manager.rs       tests/test_session_restore_panel_abort.rs
tests/test_session_restore_ui.rs    tests/test_session_restore_workers.rs   tests/test_tool_name_literals.rs
```

### A.3 NEW — docs (8 + this manifest)

```
docs/session_restore.md                  user guide: bundle layout, --resume/--fresh, exit codes
docs/design_session_restore_data.md      data model: bundle, pins, normalization N0-N6
docs/design_session_restore_flow.md      triggers T-1..T-10, boot sequence, exit codes
docs/audit_static_t014f_part1.md         static audit 1 (fix_loop, worker_journal, restore, notice, tool_names)
docs/audit_static_t014f_part2.md         static audit 2 (format/ui, tui restore, struct-literal cross-checks)
docs/cleanup_t032.md                     comment / ignore-annotation cleanup
docs/cleanup_t033.md                     follow-up cleanup (src/ui/session.rs, src/session/worker_journal.rs)
docs/verification_session_restore.md     STATIC-only report; all numbers labelled STATIC or TBD-t034
docs/commit_manifest_t029.md             this file
```

### A.4 MODIFIED — inside the t-029 scope list (16)

```
src/manager/context.rs        install_restored_messages (:1151), restore_engine_flags (:1170)
src/manager/phase.rs          bundle path authority (:393), LEGACY_TRANSCRIPT_FILE re-export (:128), clear_capture (:346)
src/orchestrator/notice.rs    worker_inbox_len (:259), peek_worker_inbox (:285)
src/agents/runner/fix_loop.rs mirror_append_here / mirror_collapse_here (:70)
src/ui/mod.rs                 Renderer defaults capture_ui (:152), restore_panes (:164), announce_restore (:173)
src/ui/session.rs             boot_if_requested, write_bundle, set_resume_request, commit_from_panic
src/ui/session_tests.rs       in-crate restore probes (BootReport, exit-3 mapping)
src/ui/tui/mod.rs             TUI impl capture_ui (:1085), restore_panes (:1196), announce_restore (:1358)
src/ui/tui/events.rs          two-stage Ctrl-C arming -> ProcessEnding::UserAbort
src/ui/tui/formatting.rs      pane formatting on the capture/restore surface
src/ui/tui/render.rs          render path + cache invalidation after restore_panes
src/ui/tui/tests.rs           capture/restore unit tests (:2161-:3261)
src/ui/raw.rs                 raw impl capture_ui (:194), restore_panes (:228), announce_restore (:326)
src/tool_names.rs             tool-name constants pinned by tests/test_tool_name_literals.rs
README.md                     `.marmel/session/` row (:276) linking docs/session_restore.md
marmel.toml.example           session_enabled / session_resume / session_commit_min_interval_ms /
                              session_worker_mirror_max_messages / session_commit_on_tool_result /
                              session_max_bundle_bytes (:46-83)
```

### A.5 Mission-dependent files MISSING from the t-029 scope list — verify first

In-tree code shows the feature cannot compile without these; if `git status --porcelain` shows them modified they
MUST be staged explicitly (never `-A`), otherwise the staged tree will not build:
`src/lib.rs` (`pub mod session;` :29) · `src/main.rs` (`--resume`/`--fresh` parsing :254-267, `session_flag_error`
:300, RestoreRefused → exit 3 :101-105, panic-hook flush :357-383) · `src/config.rs` (`session_*` keys :198-211,
defaults :239-244, env layer :418-455, partial merge :516-621, tests :1475-1534) ·
`src/agents/runner/execution.rs` (`use crate::session::worker_journal::{..}` :10) · `src/agents/validation.rs`
(journal open / JournalCloseGuard / with_journal_async :338-553) · `src/orchestrator/workers.rs`
(`registry_get` join :205). `Cargo.toml` appears UNCHANGED (no new deps; only serde/uuid/dashmap/tempfile are used) — confirm.

### A.6 MUST NOT be staged

- `.marmel/**` — runtime session bundles (untracked scratch state); `target/**` — build artifacts.
- `tests/test_session_config.rs` — **does not exist**: `glob tests/test_session*.rs` returns exactly the 9
  `test_session_*` targets of A.2 plus `test_tool_name_literals.rs`; staging it would make `git add` fail.
- Any other pre-existing dirty file outside A.1–A.5 (pre-existing formatting diffs in untouched files are out of scope;
  `docs/baseline_test_run.md` pins that dirty baseline). Never `git add -A`, never `git commit --no-verify`.

### A.7 BLOCKERS to settle BEFORE the commit

Re-read verbatim from the tree — two `#[ignore]` witnesses remain in the panel/abort target, one in the UI target:

- `tests/test_session_restore_panel_abort.rs:1503` — "implementation landed at src/ui/session.rs:1256-1274 (kind line
  1263, subject = panel name); awaiting first execution by the t-014f/t-034 gate"
- `tests/test_session_restore_panel_abort.rs:2451` — "capture path exists (pending_notice_notices at
  src/session/worker_journal.rs:995-1008, wired into project_worker_section at worker_journal.rs:624, fed by
  orchestrator::notice::peek_worker_inbox at src/orchestrator/notice.rs:285-292); test pending first execution by the t-014f/t-034 gate"
- `tests/test_session_restore_ui.rs:1327` — "acceptance pending AC-C3 branch 2: the clamp notice is implemented and
  folded into RepairReport::warnings at boot, but the criterion has not yet been observed passing (§6.6 I-U6)"

These are resolved by **t-026 / t-034** and must be settled (executed and un-ignored, or explicitly justified) before commit.

---

## (b) Ready-to-run command block (branch A, verbatim)

```bash
# 1. Scoped formatting ONLY — never repo-wide `cargo fmt`
rustfmt --edition 2024 \
  src/session/mod.rs src/session/commit.rs src/session/manager_capture.rs src/session/restore.rs \
  src/session/time.rs src/session/worker_journal.rs \
  src/session/format/mod.rs src/session/format/envelope.rs src/session/format/messages.rs \
  src/session/format/restore.rs src/session/format/ui.rs src/session/format/worker.rs \
  src/manager/context.rs src/manager/phase.rs src/orchestrator/notice.rs src/agents/runner/fix_loop.rs \
  src/ui/mod.rs src/ui/session.rs src/ui/session_tests.rs src/ui/raw.rs \
  src/ui/tui/mod.rs src/ui/tui/events.rs src/ui/tui/formatting.rs src/ui/tui/render.rs src/ui/tui/tests.rs \
  src/tool_names.rs \
  tests/test_session_abort_flush.rs tests/test_session_boot.rs tests/test_session_bundle.rs \
  tests/test_session_commit.rs tests/test_session_restore_dangling.rs tests/test_session_restore_manager.rs \
  tests/test_session_restore_panel_abort.rs tests/test_session_restore_ui.rs tests/test_session_restore_workers.rs \
  tests/test_tool_name_literals.rs

# 2. Gate 1 — must be clean
cargo clippy --all-targets --all-features -- -D warnings
# 3. Gate 2 — record passed/failed totals and name every failure
cargo test

# 4. Stage explicitly (A.1-A.4); add A.5 files only if git status shows them modified
git add src/session/mod.rs src/session/commit.rs src/session/manager_capture.rs src/session/restore.rs \
  src/session/time.rs src/session/worker_journal.rs \
  src/session/format/mod.rs src/session/format/envelope.rs src/session/format/messages.rs \
  src/session/format/restore.rs src/session/format/ui.rs src/session/format/worker.rs \
  src/manager/context.rs src/manager/phase.rs src/orchestrator/notice.rs src/agents/runner/fix_loop.rs \
  src/ui/mod.rs src/ui/session.rs src/ui/session_tests.rs src/ui/raw.rs \
  src/ui/tui/mod.rs src/ui/tui/events.rs src/ui/tui/formatting.rs src/ui/tui/render.rs src/ui/tui/tests.rs \
  src/tool_names.rs
git add tests/test_session_abort_flush.rs tests/test_session_boot.rs tests/test_session_bundle.rs \
  tests/test_session_commit.rs tests/test_session_restore_dangling.rs tests/test_session_restore_manager.rs \
  tests/test_session_restore_panel_abort.rs tests/test_session_restore_ui.rs tests/test_session_restore_workers.rs \
  tests/test_tool_name_literals.rs
git add docs/session_restore.md docs/design_session_restore_data.md docs/design_session_restore_flow.md \
  docs/audit_static_t014f_part1.md docs/audit_static_t014f_part2.md docs/cleanup_t032.md docs/cleanup_t033.md \
  docs/verification_session_restore.md docs/commit_manifest_t029.md README.md marmel.toml.example
git status --short   # nothing outside the list above may be staged

git commit -m "$(cat <<'MSG'
Add session bundle capture and restore with rehydrated UI and workers

Persistence: src/session/{mod,commit,restore,manager_capture,time,worker_journal} plus the
src/session/format envelope/manager/ui/worker/restore wire model; the manifest rename is the
commit point and restore is verify-before-parse with normalization N0-N6 reported in RepairReport.
Manager seam: ContextEngine::install_restored_messages / restore_engine_flags (manager/context.rs)
and the single bundle-path authority in manager/phase.rs.
UI seam: Renderer::capture_ui / restore_panes / announce_restore (ui/mod.rs) implemented for TUI
(ui/tui/*) and raw (ui/raw.rs), booted from ui/session.rs; exit 3 via session::EXIT_RESTORE_REFUSED.
Workers: mirror + registry rebuild in session::worker_journal, joined through orchestrator/notice.rs
(worker_inbox_len, peek_worker_inbox) and agents/runner/fix_loop.rs.
New test targets: test_session_abort_flush, test_session_boot, test_session_bundle,
test_session_commit, test_session_restore_dangling, test_session_restore_manager,
test_session_restore_panel_abort, test_session_restore_ui, test_session_restore_workers,
test_tool_name_literals.
Docs: session_restore, design_session_restore_{data,flow}, audit_static_t014f_part{1,2},
cleanup_t032, cleanup_t033, verification_session_restore, commit_manifest_t029.
MSG
)"

# 5. Post-commit evidence
git rev-parse HEAD && git status --short
```

Known pre-existing failures that may remain without blocking: 4× `harness::pty::*`, the `orchestrator::notice_tests`
tracing flakes, `orchestrator::steer_tests::test_unknown_steer_subtask_action_rejection_is_logged`,
`orchestrator::workers::tests::test_completion_leaves_no_stale_registry_entry`,
`harness::dispatch_integrity_tests::test_reply_without_notice_id_is_rejected_and_no_notice_is_guessed`,
`ui::bridge::tests::test_bridge_notice_capacity_drop*`, `test_role_gating::test_prompt_based_tool_gating`.
Any NEW failure, or any clippy warning ⇒ do not commit.

---

## (c) Honesty note

Nothing in this branch was compiled, formatted, tested or committed — `run_command` dies at spawn, so every statement above is a static tree reading; the clippy/test gate results are owed by t-034/t-026, which must also settle the three `#[ignore]` witnesses in A.7.
The commit itself is owed by branch A once a working shell exists; this manifest is preparation only, and no file other than this one was touched.
