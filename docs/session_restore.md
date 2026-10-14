# Session Restore

Marmel can snapshot a run to disk and put it back. This guide is the user-facing
description: what a bundle contains, how to ask for it, what comes back, what
never can, and what the three failure verdicts mean.

Design detail lives in two companion documents:

- `docs/design_session_restore_data.md` — the durable data model (bundle layout,
  member types, pin rules, normalization, non-restorable inventory).
- `docs/design_session_restore_flow.md` — the triggers, boot sequence and exit
  codes.

---

## 1. The session bundle

The snapshot is a **directory**, not a file: `.marmel/session/`
(`Plan::session_bundle_path()`, `src/manager/phase.rs`).

| Path | What it is |
|---|---|
| `manifest.json` | The root document and **commit point**. Format/schema versions, the session id, the wall + monotonic anchors, how the writing process believes it ended, the identity floors, and the member list with each member's exact byte length and FNV-1a fingerprint. |
| `manager.json` | The Manager section: message history, pin witness, engine scalars. |
| `ui.json` | The UI section: every pane's text, composer, view state, subagent panels, witnesses. |
| `workers/<key>.json` | One member per subagent, named after its registry key with escaping (e.g. `workers/coder-t-018%237.json`). |

Members are written workers → `ui.json` → `manager.json`, and
**`manifest.json` is written last — that rename *is* the commit**
(`src/session/commit.rs`). If the process dies before the manifest lands, there
is simply no committed bundle for that instant: the previous manifest is still
what the next boot reads. Two consequences worth knowing:

- A bundle is only as good as its manifest. A member on disk that the manifest
  does not name is an **orphan** and is ignored, never merged.
- Every member write is atomic with respect to **process death** (stage to a
  unique name, then rename in the same directory). The writer performs no
  `fsync`, so a whole-machine power loss is a different threat: it can leave a
  manifest whose member bytes disagree. That is reported as a refusal, never as
  a half-restored session.

---

## 2. Asking for a restore

| How | Effect |
|---|---|
| `marmel --resume` | Rehydrate the previous run from `.marmel/session/` for this run. |
| `marmel --fresh` | Force a fresh start. Overrides `session_resume = true` and `MARMEL_RESUME`. `--fresh` always wins; it is the escape hatch. |
| `session_resume = true` in `marmel.toml` | Resume by default; a single run can opt out with `--fresh`. |
| `MARMEL_RESUME=1` | Same as `--resume` for one run. |

Precedence: **`--fresh` > `--resume` > `MARMEL_RESUME` / `session_resume`**.
With no flag and no config key, a run starts fresh — the bundle is still written.

Two combinations are rejected before anything starts (no config is published, no
runtime is built, no bundle is touched) and exit with code **2**:

- `--resume` together with `--fresh`;
- `--resume` while capture is off (`session_enabled = false`, or
  `MARMEL_SESSION_DISABLE`), because that would resume from a bundle this run
  would never write.

The knobs that decide *what gets written* are the six config keys
(`src/config.rs`, mirrored in `marmel.toml.example`):

| Key | Type | Default | Notes |
|---|---|---|---|
| `session_enabled` | bool | `true` | Master capture switch. `MARMEL_SESSION_DISABLE` is a **one-way** switch: it can turn capture off, never on. |
| `session_resume` | bool | `false` | Restore at boot. `--resume` / `MARMEL_RESUME` set it for one run. |
| `session_commit_min_interval_ms` | integer | `250` | Throttle floor between commits. `MARMEL_SESSION_COMMIT_MIN_MS` overrides it; a non-integer is reported and ignored. |
| `session_worker_mirror_max_messages` | integer | `4000` | Cap on the in-memory per-worker transcript mirror. |
| `session_commit_on_tool_result` | bool | `true` | Commit at every landed tool result. |
| `session_max_bundle_bytes` | integer | `33554432` (32 MiB) | Soft ceiling on member bytes: crossing it warns once and stops throttled triggers. |

Environment booleans (`MARMEL_RESUME`, `MARMEL_SESSION_DISABLE`) accept
`1` / `true` / `yes` / `on` for on and `0` / `false` / `no` / `off` for off; an
unrecognized value is reported and leaves the configured value alone.

---

## 3. What comes back

Everything below is installed from the bundle before the first draw, in one
restore pass (`src/ui/session.rs`, boot path).

### Manager

- The complete message history, installed wholesale — `role:"tool"` results,
  thinking blocks, advisory text and rebirth checkpoints included, byte for byte.
- The **pinned pair stays pinned**: `messages[0]` (system prompt) and
  `messages[1]` (immutable goal) come back unchanged, and the resume path never
  runs the prompt/goal setters, so the KV-cache prefix and the compaction window
  survive. A witness for both lengths and for the pinned prefix end is stored in
  the bundle and re-checked on load; a mismatch is a refusal, not a repair.
- Engine scalars (`max_context_tokens`, rebirth advisory counters). If the
  current config's context budget differs from the stored one, both numbers are
  reported and neither is rewritten.

### Every subagent

For each worker member:

- Its **full conversation history** — the specialist's system prompt, the brief,
  the snippets block, every assistant turn with reasoning and tool calls, every
  tool result, every nudge injection, and any compaction outcome — plus the
  seed (brief, snippets, media urls, recursion grant).
- Its identity and status: registry key (kept literally, never re-minted),
  worker id sequence value, role, task id, lifecycle class and the free-form
  status label verbatim, context tokens, turn/validation counters, and the
  pending/dangling tool-call state.
- History that was **never mirrored** (cap reached, capture not attached) is not
  silently presented as a full transcript: the member says so and the restore
  says so.

### The plan panel

The plan box comes back as pane text, not as a re-render: `plan_content`,
whether a plan was active (`had_active_plan`), the task it flagged as in flight
(`active_plan_task`), and the two display timers it drew (`plan_start`,
`plan_completed`). The plan file on disk, `.marmel/execution_plan.md`, remains
the authority for the checkboxes themselves.

### Every UI pane and control

- Chat pane lines (`chat_messages`) — element by element, including lines only
  the TUI ever produced (`/help`, `/thought`) and multi-line entries. The
  pane text is the authority: nothing is regenerated from a journal record.
- Subagent panes: logs, thinking, content, active flag, context tokens, worked
  duration, and the per-turn thinking state that drives the detail pane's
  spinner and thinking bar.
- Status line, the streaming (in-flight) thinking and answer text of the chat
  pane, and the agent name the status line was crediting.
- Composer: the half-typed message verbatim, the **caret** (re-anchored on a
  character boundary through the renderer's own sync path), the draft saved when
  history browsing began, the command history and the position in it.
- View state: scroll offsets for chat / plan / subagents / subagent list, the
  selected subagent, the focused panel, and the panel toggles
  (`show_thought`, `show_subagent_panel`, `show_plan_panel`).

### Witnesses (recorded, deliberately not adopted)

Some numbers are persisted only so drift can be stated out loud: the token
counters (`tokens_in`, `tokens_out`, `orchestrator_context_tokens`), the
context-budget witness, and the record that a confirm-abort window was armed when
the session was captured. These are **never** written back into live counters, and
the live renderer is left **unarmed** — re-arming a three-second abort window
after a restart would let one keypress abort a session nobody asked to abort.
Each of these is announced as a warning instead.

### Identity floors

The bundle stores the floors of the three process-global counters — worker ids,
completed-entry sequence numbers, and notice ids. On restore the counters are
raised to those floors (`fetch_max`, never lowered), so no worker key, completion
order or notice id from the previous run is ever re-issued. The floor is the
**first unissued** value: the next draw equals the floor, and every draw after
that is above it.

---

## 4. What cannot be restored

A bundle holds bytes, not a live process. These do not survive, and the restore
says so out loud (as a `Not restorable after restart` line in the panes and a
warning in the log) instead of quietly faking them back:

| Gone | What you see instead |
|---|---|
| **Live PTY processes and their process groups** | A tool subprocess that was running when the process died was not killed, its output was not saved, and it will not be re-attached or re-run — files it was writing may be partial. A subagent with dispatched tool calls is flagged as side-effect risk: re-running it could apply the same effect twice. |
| **Join handles / the driving tasks** | Nothing can be joined, aborted or awaited; the completion bookkeeping that the dead task's guard would have run never runs. |
| **Cancellation tokens** | The old cancel control is gone. A fresh control is attached to the restored record, so cancelling it acts on the placeholder, not on the old task. |
| **Queued notices** | Steering notices that were posted but not delivered, and replies typed but not yet collected, are gone. Their *ids* are preserved as a floor, so no old notice id is reused. |
| **Timers and monotonic clocks** | Elapsed-time figures are re-derived from wall-clock anchors, never from the previous process's clock. Animation clocks and per-frame caches are not stored at all. |
| Terminal geometry and OS scrollback | Pane text is restored exactly and re-wrapped for the current window; a scroll offset that the new window cannot hold is clamped, and the move is reported with both values. |
| Composer undo/redo and in-widget selection anchors | The text and caret come back; the undo history does not. |
| Never-mirrored worker history, and an unrecoverable worker file | Only the seed is restorable, and that is stated per subagent. |
| Live counters and the tool-schema token charge | Re-derived from live state, and reported when they differ from the stored numbers. |

---

## 5. Three verdicts

### 5.1 Nothing to restore (fresh start)

No `manifest.json` under `.marmel/session/`: the run simply starts fresh, with a
line in the panes naming the path that was searched and any orphan member files
found. Exit code `0`.

This is also where the **legacy journals** are named. Older releases wrote
`.marmel/.session_transcript.json` and `.marmel/.ui_transcript.json`. Those files
are never written again, never merged, and never silently upgraded into a bundle —
they carry no version, no session id and no ending, so the fail-closed rule
forbids guessing at them. A fresh boot reports them as "found, not restorable" and
names both paths. They are swept by `/reset` (and `/clear_plan`), which goes
through `Plan::clear()`; `Plan::archive()` sweeps them too, together with the
bundle itself.

### 5.2 Refused

A refused restore is a hard stop. The bundle is **not** read into live state,
**nothing is installed**, **no fresh session is started under a flag that asked
for the old one**, and **no byte of the bundle is modified**. The process exits
with `session::EXIT_RESTORE_REFUSED` = **3** (`src/session/mod.rs`; mapped in
`src/main.rs` after the screen has been restored and every worker cancelled).

A refusal prints the refusal code, the offending member path, the two numbers
that disagreed, the bundle path, and the sentence that the bundle is still on
disk unchanged. Typical causes: a format or schema version the reader does not
implement, an unknown field, unreadable JSON, a member the manifest names but the
disk lacks, a byte-length or fingerprint mismatch, a pin that moved, a
normalization pass that would change the pinned window, a header/manifest
disagreement, or a `ui.json` that fails its structural preconditions (caret past
the end of the text, an out-of-range selection, an unknown focused panel).

The bundle is never rewritten by a restore: repairs happen in memory, and
normalized bytes are written only at the next commit trigger.

### 5.3 Partially restored (one corrupt worker file)

There is exactly **one** tolerated corruption: a single **worker** member whose
bytes do not verify (or a worker member the manifest names but the disk is
missing). That one subagent is installed as **`Unrecoverable`** — its identity
and pane presence are kept so the screen still shows that the subagent existed,
while its history is honestly reported as gone. Everything else (manager, UI,
every other subagent) is restored normally, and the outcome is reported as
partially restored with the unrecoverable keys named.

The tolerance is deliberately narrow: one member, worker kind only, everything
else verified. A second corrupt worker file, or a corrupt `manager.json` /
`ui.json` / `manifest.json`, is a refusal under §5.2.

---

## 6. A subagent that was mid-flight when the process died

A subagent that was running is **not** running after a restore. It comes back as
an open, **interrupted** delegation:

- the entry stays open, so "are workers active?" and every abort path still see
  it;
- its status label becomes `Interrupted by restart`, and the subagent pane keeps
  its pre-crash text with a line recording that the history was restored and the
  execution was not resumed;
- its history is restored byte-exactly (when it was mirrored);
- its old cancellation control is gone and a fresh one is attached, which is
  reported;
- if it had dispatched a tool call, the possible side effect is flagged
  (see §4).

**It is never relaunched automatically.** Auto-resume would re-issue the tool
calls that were in flight when the snapshot was taken, re-enter a validator and
fix-loop whose bounded counters were spent by a different process, and start a
task the Manager's own transcript does not know about. Resuming that subagent's
*work* is a Manager decision, made after the restore notice tells it what came
back; resuming the *session* is what `--resume` does.

A restored worker whose status label is not one of the recognized lifecycle
labels keeps its label verbatim, is treated as an open delegation, and is
reported as a warning naming both the key and the label.

---

## 7. Practical notes

- **Commits are frequent but throttled.** A bundle is normally minutes-old at
  most after a crash; the load-bearing commits (clean exit, abort, panic) run
  even when the throttle is still closed.
- **A commit does not make the session live.** If a bundle looks wrong, restore
  it with `--resume` and read the restore lines in the chat pane: every repair,
  drop, clamp and loss is stated there and in the log.
- **A refused restore is recoverable by hand.** Because a refusal writes nothing,
  you can inspect or repair the member files and try again — the manifest lists
  each member's expected byte length and fingerprint so a disagreement is
  checkable.
- **`/reset` deletes the session**, not just the plan: the bundle and both legacy
  journals go with it, precisely so a cleared plan cannot resurrect a session the
  user just erased.
