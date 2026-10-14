# t-026 — I-U6 scroll-clamp warning: close-out (AC-C3 branch 2 un-ignored)

Date: t-026 cleanup pass. Scope: the `#[ignore]` gate on the AC-C3 clamp-warning test and the
stale prose around it. **No production change was required** — the fold already exists and behaves.

## 1. What changed

All edits are in `tests/test_session_restore_ui.rs` (owned by t-026). Nothing under
`src/**` was touched, and no assertion, expected value or `#[allow(...)]` was added, changed
or weakened.

| # | File:line (pre-edit) | Edit |
|---|----------------------|------|
| 1 | `tests/test_session_restore_ui.rs:1327` | **Deleted** the attribute `#[ignore = "acceptance pending AC-C3 branch 2: the clamp notice is implemented and folded into RepairReport::warnings at boot, but the criterion has not yet been observed passing (§6.6 I-U6)"]`. The criterion is now observed passing (see §3/§4), so the "not yet observed" premise is false. |
| 2 | `tests/test_session_restore_ui.rs:1320-1325` | Removed the now-stale tail of the doc comment above `#[test]` that justified the attribute ("The attribute kept below is therefore NOT a claim that the gap is unfilled: it marks the criterion as acceptance-pending. Un-ignoring requires an observed passing run … and no such run has been recorded, so the attribute stays until that evidence exists."). Kept the factual part naming the producer (`clamp_warning_sentences`, `src/ui/tui/mod.rs:1527`), the boot fold (`scroll_clamp_warnings` → call site before `announce_restore`) and the pinning lib unit test. |
| 3 | `tests/test_session_restore_ui.rs:36-39` | Module-header rule "no weakened assertion" previously said the file's answer to a criterion/implementation disagreement is to "keep the criterion's wording and carry `#[ignore]` (see `ac_c3_…`)". Reworded: the criterion's wording is kept instead of relaxing the comparison, **and no test in this file is `#[ignore]`d** — the cross-reference to an ignored test is gone. |

Post-edit anchors: doc comment `tests/test_session_restore_ui.rs:1310-1319`, `#[test]`
at `:1320`, `fn ac_c3_scroll_offsets_that_the_live_clamp_moved_are_reported_as_warnings` at
`:1321`. File length 1544 → 1537 lines. `grep -n "ignore" tests/test_session_restore_ui.rs`
now matches only the prose line `:38`.

## 2. Production side (read-only verification, unchanged)

* Clamp-notice producer, single source of the sentence: `pub(crate) fn clamp_warning_sentences`
  at `src/ui/tui/mod.rs:1527` — emits `"<field> clamped by the live geometry: <stored> -> <live>: …"`.
* Boot fold wrapper: free fn `scroll_clamp_warnings` at `src/ui/session.rs:1570` (compares the
  committed `UiSection` against the post-restore `renderer.capture_ui()`).
* Call site in the boot fold region: `src/ui/session.rs:1286-1291` — for each warning:
  `tracing::warn!("session restore (ui.json): {warning}")` + `push_repair_warning(&mut repair, warning)`,
  **before** `renderer.announce_restore(&repair)` at `:1291`, so a moved offset is stated once.
* The live clamps themselves: `clamp_scrolls()` as the last step of `restore_panes`
  (`src/ui/tui/mod.rs`), plus the `set_subagents` re-clamp of `selected_subagent_idx`
  (`src/ui/tui/mod.rs:1066-1068`).

## 3. Before / after test counts

| Command | Before | After |
|---------|--------|-------|
| `cargo test --test test_session_restore_ui` | 8 passed / 0 failed / **1 ignored** | **9 passed / 0 failed / 0 ignored** |
| `cargo test --test test_session_restore_ui -- --ignored --nocapture` | 1 passed (probe only) | 0 run (nothing ignored left) |
| `cargo test --lib ui::` | 196 passed | **196 passed** / 0 failed |
| `cargo clippy --all-targets --all-features -- -D warnings` | exit 0 | **exit 0** |
| `cargo check --all-targets` | exit 0 | exit 0 (implied by the clippy run) |

## 4. Observed clamp-warning lines (this run, `--nocapture`, test now runs by default)

```
AC-C3: chat_scroll clamped 9000 -> 0: chat_scroll clamped by the live geometry: 9000 -> 0: the stored value does not fit the restored content, and the live value is what the panes show now
AC-C3: plan_scroll clamped 12 -> 0: plan_scroll clamped by the live geometry: 12 -> 0: the stored value does not fit the restored content, and the live value is what the panes show now
AC-C3: subagent_scroll clamped 9000 -> 28: subagent_scroll clamped by the live geometry: 9000 -> 28: the stored value does not fit the restored content, and the live value is what the panes show now
AC-C3: subagent_list_scroll clamped 40 -> 3: subagent_list_scroll clamped by the live geometry: 40 -> 3: the stored value does not fit the restored content, and the live value is what the panes show now
```

Same four values recorded earlier by `docs/verification_t014f.md` §4 (`9000->0`, `12->0`,
`9000->28`, `40->3`) — reproduced here without the `--ignored` probe, i.e. inside the normal
gate run. `selected_subagent_idx` did not move in this fixture, so it correctly emits no sentence.

## 5. Notes / follow-ups not in my scope

* `docs/verification_t014f.md` still describes the attribute as "stale (left untouched here)"
  and the suite as `8 passed + 1 ignored` (lines 20-21, 82). That file is not in the t-026
  owned set, so it was not edited; it is now out of date on this point.
* No `cargo fmt` was run (the tree is not rustfmt-default-clean at HEAD and repo-wide fmt was
  out of bounds); the edits are comment-only, so no reformatting was needed.
* No git commit was made.
