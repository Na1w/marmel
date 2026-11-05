# Change surface — `feature/new-ui` @ `9726d26` (plan task t-020)

Written by: validator_specialist, final branch audit. Date: 2026-10-27 (CET).
Mode: **read-only on `src/`**. The only files this task wrote are `docs/change_surface_report.md`
(this file) and an appended Part B in `docs/verification_report.md`. `README.md`, `docs/new_ui.md`,
`docs/runtime_smoke_report.md`, `.marmel*` and `docs.old/` were not touched.

HEAD audited: `9726d26b4740417a923de09ab9fb86252e10ebd7` (`test(ui): runtime smoke verification of the
rebuilt TUI (t-018)`), parent `d8cb75d`. Everything below is measured against the live repository, not
carried over from any earlier report.

---

## 1. The authoritative commit chain

Branch root: `ae8d4d6` (`ui: scaffold new TUI state/layout/theme/components modules`), whose parent is
`b66f669` = the tip of `main` = the tip of `origin/main`. The branch is **linear**: `git log --merges`
over `b66f669..HEAD` returns **0**, and `git rev-list --count --ancestry-path b66f669..HEAD` = **24**.

Ordered oldest → newest (subject lines verbatim from `git log --format='%h parent=%p %s'`):

| # | sha | parent | subject |
|---|---|---|---|
| 1 | `ae8d4d6` | `b66f669` | ui: scaffold new TUI state/layout/theme/components modules |
| 2 | `9165143` | `ae8d4d6` | ui: fix chip clipping, status-bar field ladder and roster rank; add preview evidence |
| 3 | `03cbd14` | `9165143` | ui: make view state real and route the terminal entry path through the new renderer |
| 4 | `7ce7b99` | `03cbd14` | ui: new keymap, focus navigation and shared help legend |
| 5 | `f999378` | `7ce7b99` | ui: plumb worker, queue, error and verdict state into the new panes |
| 6 | `e41d9bf` | `f999378` | ui: test the restore seam and verdict emission; make revocation-led verdicts honest |
| 7 | `12d8f59` | `e41d9bf` | ui: confirm overlay for all destructive actions, unified /reset, sticky error recall |
| 8 | `2445408` | `12d8f59` | ui: gate the sink's mid-turn abort on the renderer too, one owner for the abort vocabulary |
| 9 | `78297f8` | `2445408` | ui: pin the mid-turn gate for both real renderers at the shared abort owner |
| 10 | `8634210` | `78297f8` | ui: surface queued steering, notice targeting and pause reasons |
| 11 | `1b9f24d` | `8634210` | refactor(ui): retire the legacy TUI pane renderers |
| 12 | `3efb975` | `1b9f24d` | test(ui): re-establish the coverage retired with the legacy panes (t-015) |
| 13 | `4fca031` | `3efb975` | test(tui): frame-level pins for the rebuilt UI (spec §10 items 16-21) |
| 14 | `fc058cc` | `4fca031` | docs(ui): user guide for the rebuilt TUI (t-019) |
| 15 | `c6794a6` | `fc058cc` | chore(ui,docs): clear the fmt gate, correct the stale spots in the spec (t-017a) |
| 16 | `4d71e32` | `c6794a6` | fix(session,ui): persist mode, pane focus and follow through a restore (D2, t-017b) |
| 17 | `d5d3bd2` | `4d71e32` | fix(ui): make the advertised slash commands the handled ones (t-021) |
| 18 | `684de35` | `d5d3bd2` | fix(ui): submitting never re-arms transcript follow (D3, spec §10 item 14) |
| 19 | `dc7712f` | `684de35` | test(ui): pin the active task id at the 40-column header floor (D4) |
| 20 | `55ff06c` | `dc7712f` | docs(ui): align the command docs with the commands that are actually handled (t-022) |
| 21 | `5d30439` | `55ff06c` | test(ui): pin acceptance item 17 by behaviour, not by grepping events.rs (D8, t-017e) |
| 22 | `097a4a1` | `5d30439` | fix(ui): stop test-dropped renderers resetting the ambient terminal (D11) |
| 23 | `d8cb75d` | `097a4a1` | fix(ui): inject the frame's token snapshot instead of reading the global (D5) |
| 24 | `9726d26` | `d8cb75d` | test(ui): runtime smoke verification of the rebuilt TUI (t-018) |

### 1.1 Reconciliation of `12d8f59` and `2445408` — they are ordinary commits on the chain

Both shas were flagged by a briefing note as "not in the recorded chain". **That premise is wrong.** The
chain that was recorded for the brief (22 shas) is a truncated transcription: the branch has 24 commits,
and these two sit between `e41d9bf` and `78297f8`. Evidence:

* `git cat-file -t 12d8f59` → `commit`; `git cat-file -t 2445408` → `commit` (both exist as commit objects).
* Full shas and parentage: `12d8f5958a44d9acd79e2853bb84c832db0ca957`, parent `e41d9bf…`;
  `2445408e15883b96d22b606a1654411d2124eaa1`, parent `12d8f59…` — i.e. `12d8f59` is the direct parent of
  `2445408`, and both are on the first-parent line.
* `git merge-base --is-ancestor <sha> HEAD` → **YES** for both (reachable, not dangling).
* `git branch --all --contains <sha>` → only `feature/new-ui` (so they are branch-local UI commits, never
  merged anywhere else, and not unrelated objects from another line).
* They appear in `git log --oneline` (HEAD) **and** in `git log --all --oneline`.
* `git reflog` records them as plain `commit:` entries — `12d8f59 HEAD@{2026-10-26 23:10:24}` and
  `2445408 HEAD@{2026-10-26 23:27:42}` — with **no** `commit (amend)` and **no** `rebase` line attached to
  either. Nothing superseded them.

Conclusion, stated explicitly: **`12d8f59` and `2445408` are neither amended/superseded commits, nor
unreachable objects, nor rebase droppings, nor unrelated shas. They are two reachable commits of the
branch chain, and the "missing" flag was an error in the recorded 22-sha chain list.** The same conclusion
was already reached at t-012b (`docs/verification_report.md` §2, gate 1), which recorded a complete
13-sha chain for the then-HEAD `4fca031` including both shas.

### 1.2 The one genuinely unreachable object in the branch window

`3290cdf324af4031a2678aa749562b13d7c88b1f` — subject `fix(ui): submitting never re-arms transcript follow
(D3, spec §10 item 14)`, parent `d5d3bd2`. This is the **pre-amend version** of the D3 fix: the reflog
shows `3290cdf HEAD@{2026-10-27 07:12:57}: commit:` immediately followed by
`684de35 HEAD@{2026-10-27 07:13:09}: commit (amend):` with the same subject. `git merge-base --is-ancestor
3290cdf HEAD` → **NO**, `git branch --contains 3290cdf` → empty, and `git fsck --unreachable --no-reflogs`
lists it. So the branch contains **exactly one amend**, and it left exactly one dangling commit; the live
D3 fix is `684de35`.

Everything else unreachable inside the branch window (2026-10-26 → 2026-10-27) is stash bookkeeping, not
history: `WIP on feature/new-ui: …`, `index on feature/new-ui: …`, `untracked files on feature/new-ui: …`
(`ed1fed0`, `87df187`, `3cbf9ac`, `b6f286a`, `fb5eea7`, `4dc9402`, `64e4337`, `4f5ee54`, `8ef32ea`,
`29fb466`, `6c8ac34`). The repo-wide unreachable-commit count is 244, and the great majority are stash
objects from other branches and pre-branch work. A scan of every unreachable commit's parents against the
HEAD ancestry found **no orphan child of any branch-chain commit** other than `3290cdf` and the stash
bookkeeping above — i.e. no discarded scaffold, no abandoned rebase attempt.

### 1.3 Base-descendant proof; `main` was not rewritten

* `git merge-base main HEAD` = `b66f6692573e91b6db6c874613e49b59d24ac8b2` = the tip of `main` = the tip of
  `origin/main` (`git rev-parse main origin/main` prints the same sha twice).
* `git merge-base --is-ancestor b66f669 HEAD` → true; `git rev-list --count HEAD..main` → **0** (the branch
  is a strict fast-forward descendant: nothing on `main` is missing from the branch).
* The branch root's parent **is** `b66f669` (`ae8d4d6 parent=b66f669`), so the fork point is exactly the
  recorded merge base — there is no hidden rebase onto a newer `main`.
* `git --no-pager reflog show main` → only `pull -r: Fast-forward` entries (`b66f669`, `de3e99c`,
  `fd301a0`, `43ba4fa`, `d66fdde`); the newest `main` movement predates the branch, and there is **no**
  `reset`/`rebase`/forced-update entry in the branch window. Local `main` and `origin/main` agree, so no
  `main` commit was rewritten.
* There is no `master` ref in this repository (`git merge-base HEAD master` → "no such ref"), so nothing
  named `master` could have been rewritten either.

---

## 2. `git diff --stat b66f669..HEAD` (verbatim, merge base = `b66f669`)

```
 README.md                                 |    62 +-
 docs/legacy_ui_removal.md                 |   212 +
 docs/new_ui.md                            |   433 +
 docs/new_ui_preview.txt                   |   183 +
 docs/research_ui_dataflow.md              |   253 +
 docs/research_ui_tui_arch.md              |   328 +
 docs/research_ui_ux_baseline.md           |   262 +
 docs/runtime_smoke_report.md              |   432 +
 docs/ui_design_spec.md                    |   486 ++
 docs/verification_report.md               |   251 +
 src/markers.rs                            |    82 +
 src/session/format/mod.rs                 |     4 +-
 src/session/format/ui.rs                  |   322 +-
 src/ui/bridge/action.rs                   |   222 +
 src/ui/bridge/drain.rs                    |   366 +-
 src/ui/bridge/sink.rs                     |   153 +-
 src/ui/bridge/steer.rs                    |    45 +
 src/ui/bridge/tests.rs                    |   255 +
 src/ui/helpers.rs                         |   174 +-
 src/ui/mod.rs                             |   145 +
 src/ui/raw.rs                             |    77 +
 src/ui/session.rs                         |   122 +-
 src/ui/session_tests.rs                   |   257 +
 src/ui/tui/components/activity.rs         |  2031 +++++
 src/ui/tui/components/header.rs           |   870 ++
 src/ui/tui/components/help.rs             |   885 ++
 src/ui/tui/components/input.rs            |   827 ++
 src/ui/tui/components/mod.rs              |   130 +
 src/ui/tui/components/plan_pane.rs        |   760 ++
 src/ui/tui/components/status_bar.rs       |  1258 +++
 src/ui/tui/components/transcript.rs       |  1396 ++++
 src/ui/tui/events.rs                      |  2565 ++++--
 src/ui/tui/formatting.rs                  |     5 -
 src/ui/tui/layout.rs                      |   293 +
 src/ui/tui/mod.rs                         |  1370 ++-
 src/ui/tui/render.rs                      |   859 +-
 src/ui/tui/state.rs                       |   398 +
 src/ui/tui/tests.rs                       | 12413 +++++++++++++++++-----------
 src/ui/tui/tests_new_ui.rs                |  2791 +++++++
 src/ui/tui/theme.rs                       |   228 +
 tests/test_new_ui_runtime_smoke.rs        |   581 ++
 tests/test_session_restore_panel_abort.rs |   232 +-
 tests/test_session_restore_ui.rs          |    14 +
 43 files changed, 28493 insertions(+), 6539 deletions(-)
```

`git diff --shortstat` → **43 files changed, 28 493 insertions(+), 6 539 deletions(-)**.

---

## 3. Per-area summary

### 3.1 New UI modules (the rebuild proper)

11 new source files under `src/ui/tui/` (plus a test module), current line counts:

| File | Lines | Role (spec §3 pane inventory) |
|---|---|---|
| `components/activity.rs` | 2031 | Workers roster + per-worker detail (the replacement for the legacy subagent pane) |
| `components/transcript.rs` | 1396 | Transcript pane: streamed content/thought/steer regions, verdict glyphs, scroll chip |
| `components/status_bar.rs` | 1258 | Two status rows, chip ladder, armed-abort prompt, `val N` |
| `components/help.rs` | 885 | `legend()` / `legend_table()` / `footer_legend()` / confirm-overlay text — the single legend owner |
| `components/header.rs` | 870 | Header chips + `done/total` + active task id, the §7.4 budget ladder |
| `components/input.rs` | 827 | Composer: height ladder, caret, undo/paste hints, focus badge |
| `components/plan_pane.rs` | 760 | Execution-plan pane (checkbox counts, active task, scroll) |
| `components/mod.rs` | 130 | Module surface |
| `state.rs` | 398 | `ViewMode` M1–M4, `FocusPane`, `OverlayState`, `PlanProgress` — real view state, not `Cell`s |
| `layout.rs` | 293 | Region budget, §2.5 degradation ladder, `input_height` |
| `theme.rs` | 228 | Palette slots + `Glyph` table |
| `tests_new_ui.rs` (test module) | 2791 | 36 acceptance-grade frame pins tagged `_item14` … `_item21` |

Supporting plumbing that the new panes needed: `src/ui/mod.rs` (+145: `SubagentDetail::validation_rounds`
/`status_label`, spec §9.3 delta D1), `src/ui/bridge/action.rs` (+222 new actions),
`src/ui/bridge/steer.rs` (+45), `src/ui/bridge/drain.rs` (+356/−10), `src/ui/bridge/sink.rs` (+153/−),
`src/ui/raw.rs` (+77, the headless renderer routed through the same state), `src/ui/helpers.rs`
(+174/−, abort/reset vocabulary split), `src/ui/session.rs` (+122/−).

### 3.2 Retired legacy render code

One dedicated commit: `1b9f24d refactor(ui): retire the legacy TUI pane renderers` (**+2 290 / −6 485**, the
single largest deletion of the branch). No file was deleted — the retirement happened *inside* files, which
is why the diff shows **0 `D` entries** in `git diff --name-status`:

* `src/ui/tui/render.rs` **+14 / −845** — the legacy pane renderers are gone; the file is now the thin
  compatibility/entry layer (859 changed lines).
* `src/ui/tui/mod.rs` +1 301 / −69 — legacy `Cell`-based view fields replaced by `state.rs` view state;
  `capture_ui`/`restore_panes` rewired (spec §9.2 allowed rewriting, not deleting, those converters).
* `src/ui/tui/formatting.rs` −5 — only the 5 lines the retirement made unnecessary; every exported
  formatting symbol the spec (§9.1 tail) says must stay, stays.
* `src/ui/tui/tests.rs` +7 357 / −5 056 — the legacy-pane tests went with the panes, then `3efb975
  test(ui): re-establish the coverage retired with the legacy panes (t-015)` (+2 194) put equivalent
  coverage back against the new modules.
* The retirement map itself is documented in `docs/legacy_ui_removal.md` (212 lines, added on-branch).

### 3.3 Events / keymap

`src/ui/tui/events.rs` +2 094 / −471 (2 565 changed lines): the new keymap (Tab/Shift+Tab focus ring, F1
help, `?` on empty composer only, F2 mode, F3/F4/F5 panes, `x` cancel-one, `e` sticky error, Ctrl+T/F/A/P
chips, Ctrl+C arm, Ctrl+D → `Quit`), the mouse arms deleted, and the single `ConfirmAction` vocabulary
(`Quit | CancelWorker{key,task_id} | CancelAll | Command{line}`) landed in `12d8f59` (+1 266/−102);
`2445408` (+280/−27) and `78297f8` (+22/−0) moved the mid-turn abort gate for **both** real renderers onto
that one owner. The bare-word abort aliases stopped being commands (`src/ui/helpers.rs:197` now matches
slash spellings only; the legacy vocabulary is kept behind a named renderer-gated seam
`is_legacy_abort_command`, `src/ui/helpers.rs:240`).

### 3.4 State / persistence

* `src/ui/tui/state.rs` (new, 398) is the view-state owner; `mod.rs` keeps the legacy `Cell`s only as a
  mirror surface for the old scroll maths.
* `src/session/format/ui.rs` +313 / −9 — the spec §9.4 persistence delta D2: `ViewState` gained
  `view_mode: Option<String>` (`:305`) and `follow: Option<bool>` (`:306`), `FOCUSED_PANELS` gained
  `"input"` (`:362`, now `["chat","input","plan","subagents"]`), and `VIEW_MODES` validation (`:404`)
  rejects an unknown spelling. Old bundles stay loadable: absent `view_mode`/`follow` are `None` and legal
  (`:321`, tests `the_mode_the_focus_and_the_follow_flag_round_trip_through_the_durable_format` at `:604`
  and `a_view_written_before_the_mode_and_follow_fields_still_loads_unclaimed` at `:638`).
  Landed in `4d71e32` (+155/−9 on this file; `f999378` had already added +158 for the queue/paused pair).
* Follow is now a projection: `set_follow` (`mod.rs:1263`) and the single named writer of the legacy
  mirror, `project_follow_mirror` (`mod.rs:1280`); `events.rs` no longer assigns `chat_auto_scroll`
  anywhere outside its own tests.

### 3.5 Tests added

See §6 for the counted evolution. New/changed test assets: `src/ui/tui/tests_new_ui.rs` (new, 36 tests),
`src/ui/tui/tests.rs` (+7 357/−5 056, net new pins), in-component `mod tests` inside every new component,
`src/ui/session_tests.rs` +255, `src/ui/bridge/tests.rs` +255, and integration: `tests/test_new_ui_runtime_smoke.rs`
(new, 581 lines, 8 public-seam tests), `tests/test_session_restore_panel_abort.rs` +232/− (the D8 item-17
vehicle `ac_a4_a_named_cancellation_reaches_one_worker_while_the_broadcast_reaches_every_one`,
`tests/test_session_restore_panel_abort.rs:2799`), `tests/test_session_restore_ui.rs` +14.

### 3.6 Docs added

`docs/ui_design_spec.md` (486, the binding contract), `docs/new_ui.md` (433, user guide),
`docs/legacy_ui_removal.md` (212, retirement map), `docs/new_ui_preview.txt` (183, deterministic frame
dumps at 100x30 / 60x16 / 40x10), `docs/runtime_smoke_report.md` (432, t-018 evidence),
`docs/verification_report.md` (251 + this Part B), `docs/research_ui_{dataflow,tui_arch,ux_baseline}.md`
(253 / 328 / 262, pre-design research), `README.md` +62/− (slash-command and keymap sections re-pointed at
the commands that are actually handled, `fc058cc` and `55ff06c`).

---

## 4. File inventory

### 4.1 Added (`A` in `git diff --name-status`) — 15 files

`docs/legacy_ui_removal.md`, `docs/new_ui.md`, `docs/new_ui_preview.txt`, `docs/research_ui_dataflow.md`,
`docs/research_ui_tui_arch.md`, `docs/research_ui_ux_baseline.md`, `docs/runtime_smoke_report.md`,
`docs/ui_design_spec.md`, `docs/verification_report.md`, `src/ui/tui/components/activity.rs`,
`components/header.rs`, `components/help.rs`, `components/input.rs`, `components/mod.rs`,
`components/plan_pane.rs`, `components/status_bar.rs`, `components/transcript.rs`, `src/ui/tui/layout.rs`,
`src/ui/tui/state.rs`, `src/ui/tui/tests_new_ui.rs`, `src/ui/tui/theme.rs`,
`tests/test_new_ui_runtime_smoke.rs`.

(10 docs + 11 `src/ui/tui/**` files + 1 integration test file = the code additions of the branch.)

### 4.2 Deleted — **0 files**

`git diff --name-status b66f669..HEAD` contains no `D` entry. Legacy code was removed *within* files
(`render.rs` −845, `mod.rs` −69, `formatting.rs` −5, `tests.rs` −5 056), so a file-level deletion count of
zero is expected and is not evidence that nothing was retired. `docs/legacy_ui_removal.md` is the pane by
pane record.

### 4.3 Modified — 28 files; heavily modified (≥ 300 changed lines)

| File | +/− | Churn |
|---|---|---|
| `src/ui/tui/tests.rs` | +7 357 / −5 056 | 12 413 |
| `src/ui/tui/tests_new_ui.rs` (new) | +2 791 / −0 | 2 791 |
| `src/ui/tui/events.rs` | +2 094 / −471 | 2 565 |
| `src/ui/tui/components/activity.rs` (new) | +2 031 / −0 | 2 031 |
| `src/ui/tui/components/transcript.rs` (new) | +1 396 / −0 | 1 396 |
| `src/ui/tui/mod.rs` | +1 301 / −69 | 1 370 |
| `src/ui/tui/components/status_bar.rs` (new) | +1 258 / −0 | 1 258 |
| `src/ui/tui/render.rs` | +14 / −845 | 859 |
| `src/ui/tui/components/help.rs` (new) | +885 / −0 | 885 |
| `src/ui/tui/components/header.rs` (new) | +870 / −0 | 870 |
| `src/ui/tui/components/input.rs` (new) | +827 / −0 | 827 |
| `src/ui/tui/components/plan_pane.rs` (new) | +760 / −0 | 760 |
| `tests/test_new_ui_runtime_smoke.rs` (new) | +581 / −0 | 581 |
| `src/ui/bridge/drain.rs` | +356 / −10 | 366 |
| `src/session/format/ui.rs` | +313 / −9 | 322 |
| `src/ui/tui/state.rs` (new) | +398 / −0 | 398 |

Lightly modified: `src/ui/bridge/sink.rs` 153, `src/ui/helpers.rs` 174, `src/ui/mod.rs` 145,
`src/ui/session.rs` 122, `src/ui/session_tests.rs` 257, `src/ui/bridge/tests.rs` 255,
`src/markers.rs` 82, `src/ui/raw.rs` 77, `src/ui/bridge/steer.rs` 45,
`tests/test_session_restore_panel_abort.rs` 232, `tests/test_session_restore_ui.rs` 14,
`src/session/format/mod.rs` 4, `src/ui/tui/formatting.rs` 5, `README.md` 62.

---

## 5. What the branch touched OUTSIDE the UI surface — and why

The UI surface is taken as `src/ui/**` (tui, components, bridge, session, helpers, raw, mod). Outside it:

| Path | Change | Reason | Legitimate? |
|---|---|---|---|
| `src/session/format/ui.rs` | +313 / −9 | Spec §9.4 "persistence delta D2": durable `view_mode`, `follow`, `"input"` focus spelling, plus the pre-D2 compatibility path | Yes — the contract demanded it; `4d71e32` (+155/−9), queue/paused pair from `f999378` (+158) |
| `src/session/format/mod.rs` | 4 (re-export only) | Expose the widened `ViewState` fields | Yes, mechanical |
| `src/markers.rs` | +82 / −0 | New `has_revocation_marker()` (`src/markers.rs:360+`) so the transcript pane can label a **revoked** verdict honestly instead of re-spelling the token at a display site | Yes — one commit (`e41d9bf`, +82/−0), predicate kept inside the marker grammar owner; a UI call site never re-implements the token |
| `tests/test_session_restore_panel_abort.rs` | +232 / − | D8: item-17 cancellation pinned by behaviour through the recording renderer (`ac_a4_…`, `:2799`) | Yes — test only, `5d30439` |
| `tests/test_session_restore_ui.rs` | +14 | D2 fixture fields (`view_mode`, `follow`) so the restore fixtures stay valid against the widened bundle | Yes — test only |
| `tests/test_new_ui_runtime_smoke.rs` | +581 (new) | t-018 public-seam smoke over the `Renderer` trait (frame path is `pub(crate)`, so no integration test can paint) | Yes — test only, `9726d26` |
| `README.md` | +62 / − | Re-pointed the key/slash-command sections at the commands the new renderer actually handles | Docs, `fc058cc` / `55ff06c` |
| `docs/**` | 8 new files + this report | Contract, guide, retirement map, previews, research, verification evidence | Docs |

**Explicitly untouched by the branch** (verified with an empty `git log --oneline b66f669..HEAD -- <paths>`):
`src/main.rs`, `src/harness/**`, `src/llm/**`, `src/orchestrator/**`, `src/manager/**`, `src/agents/**`,
`src/net/**`, `src/mcp/**`, `src/types/**`, `src/plan_parse.rs`, `src/config.rs`, `src/tool_names.rs`,
`src/task_id.rs`, `src/tool_args.rs`, `src/text_util.rs`, `src/prompts.rs`, `src/debug_log.rs`,
`Cargo.toml`, `Cargo.lock`. Consequences:

* **No dependency change** — spec §0's "no new crates" invariant holds (no Cargo file in the diff).
* **D10 is provably pre-existing**: the catch-all PROMPT arm that swallows `--version` lives in
  `src/main.rs:324-330`, and `src/main.rs` has **zero commits** on this branch.
* The four red `harness::pty` lib tests and the red `test_role_gating` integration test sit in code this
  branch never edited.

### 5.1 One architectural deviation worth naming

Spec §9.1 told four `plan_parse`-consumed helpers to **move** into `src/plan_parse.rs` and
`clamp_warning_sentences` to move to `src/ui/notes.rs`. At HEAD they did **not** move:
`visual_line_offset_of_first_pending` / `extract_plan_line_task_id` / `line_matches_task_id` /
`visual_line_offset_of_task` still live in `src/ui/tui/formatting.rs:998,1028,1034,1040`, and
`src/plan_parse.rs:2746,2762,2767,2775` reaches **into** `crate::ui::tui::formatting::*` (inside its own
`#[cfg(test)] mod tests`); `clamp_warning_sentences` is still `src/ui/tui/mod.rs:2921` with
`src/ui/session.rs:1547` calling `crate::ui::tui::clamp_warning_sentences`. The one genuine name collision
was resolved the way §9.1 allowed — a single surviving grammar owner
`crate::plan_parse::line_matches_task_id` (`src/plan_parse.rs:593`), with `formatting.rs:1034` as the thin
delegate and the boundary documented in prose at `src/plan_parse.rs:573` and
`docs/legacy_ui_removal.md:37-52`. So the *dependency direction* the spec asked for (ui → plan_parse) is
inverted (plan_parse → ui::tui) and `src/ui/notes.rs` does not exist. Everything compiles and all
`plan_parse` tests pass, but this is a real, recorded deviation from a binding §9.1 instruction rather than
a completed item — see the acceptance matrix in `docs/verification_report.md` Part B, additional gate (b).

---

## 6. Test-count evolution (start of branch → now)

Measured, not copied. "Start of branch" = a scratch copy of the merge-base tree (`git archive b66f669`)
compiled against the workspace target dir, counted with `cargo test --lib … -- --list`.

| Measure | `b66f669` (branch start) | HEAD `9726d26` | Δ |
|---|---|---|---|
| lib test cases (`-- --list`) | **1 245** | **1 512** (1 508 pass + 4 fail) | **+267** |
| lib cases matching the `ui::` filter | **294** | **560** | **+266** |
| `#[test]` + `#[tokio::test]` attributes under `src/` | 1 273 | 1 540 | +267 |
| test files under `tests/` | 38 entries / 35 with tests | 39 entries / 36 with tests | +1 file |
| test attributes under `tests/` | 338 | 347 | +9 |
| integration binaries actually built | — | **38** | — |

Per-file test-attribute counts (`#[test]`/`#[tokio::test]`):

| File | start | HEAD | Δ |
|---|---|---|---|
| `src/ui/tui/tests.rs` | 160 | 214 | +54 |
| `src/ui/tui/tests_new_ui.rs` | — (new) | 36 | +36 |
| `src/ui/session_tests.rs` | 31 | 34 | +3 |
| `src/ui/bridge/tests.rs` | 45 | 47 | +2 |
| `src/ui/tui/components/*.rs` (7 files with in-file `mod tests`: activity 33, input 20, help 16, status_bar 16, transcript 26, header 13, plan_pane 12) | — | 136 | +136 |
| `src/ui/tui/events.rs` (in-module tests, new home for keymap/command tests) | 0 | 17 | +17 |

Note the shape: the legacy retirement deleted tests in the same commit that deleted the panes
(`1b9f24d`, −5 056 test lines), and `3efb975` (+2 194) plus `4fca031` (+1 847) rebuilt that coverage
against the new modules — so the branch is **net +267 lib tests**, and the UI share of that net gain is
+266.

---

## 7. Hygiene deltas measured on the diff

* `#[allow(` occurrences in `src/`: **26 at branch start → 20 at HEAD.** Zero added
  (`git diff b66f669..HEAD -- src/ | grep '^+.*#\[allow'` → empty). **Six removed**, all
  `#[allow(dead_code)]`: the five composer cursor helpers (`src/ui/tui/events.rs:398,415,431,446,459` at
  start) deleted by `7ce7b99`, and one cached `plan_width`-adjacent field in `src/ui/tui/mod.rs:187`
  deleted by `9165143`. Spec §0's "adding `#[allow(dead_code)]` is forbidden" invariant holds.
* The 20 surviving `#[allow(` are all pre-existing and outside `src/ui/tui/**`: 9 in
  `src/orchestrator/tests.rs`, 2 `src/orchestrator/workers.rs`, 2 `src/agents/runner/fix_loop.rs`, and one
  each in `src/orchestrator/notice_tests.rs`, `src/agents/mod.rs`, `src/llm/stream.rs`, `src/llm/client.rs`,
  `src/ui/session_tests.rs`, `src/ui/bridge/steer.rs`, `src/ui/bridge/tests.rs` (the three `src/ui` ones
  were present at `b66f669` too — verified by `git grep -n "#\[allow(" b66f669 -- src/ui`).
* **`src/ui/tui/**` contains zero `#[allow(` at HEAD.**
* Working tree noise: 21 untracked entries (`.marmel.old*/`, `docs.old/`) predate this audit and were never
  staged or committed; `git status --porcelain` during the audit showed only those plus the two documents
  this task writes.
