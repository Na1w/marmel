# Design B — Session capture/restore flow and public API (t-006)

| Field | Value |
|---|---|
| File | `docs/design_session_restore_flow.md` |
| Task | **t-006** — design only. No `src/`, no `tests/`, no `.marmel/` changes. |
| Normative input | `docs/design_session_restore_data.md` (Design A, t-018/t-019): §1 decisions D1–D6, §2 anchors, §3 serde vocabulary, §4 pin rules, §5 worker section, §6 UI section, §7 write/commit protocol, §8 on-load normalization, §9 acceptance criteria, §10 non-restorable inventory, §11 audit verdict. **Design A is finished and normative; this document does not restate or amend it — it consumes it.** |
| Verified against | Branch `1.0.0-refactor`, HEAD `bbfb893`. Every `path:line` below was re-read at that commit. |
| Scope of B | **When** a bundle is written (triggers), **what happens to in-flight workers** on resume (one policy), **how the operator turns it on** (config/env/CLI), **the exact Rust surface** that tasks t-007…t-013 will build, and **the order** in which they land. |

Language: **SHALL** = requirement, **SHALL NOT** = prohibition, **MAY** = permitted. `path:line` = `file:line`.
§1–§5 are normative for t-007…t-013. §6 lists open items, each with an owning task.

Two names Design A uses but never defines are **defined here**, because B owns the flow that consumes them:
`session::format::RestoredSession` (§4.6, used by A §3.10 `RestoreOutcome::Restored`/`PartiallyRestored`) and
`session::restore::BootReport` (§4.9). Anything else named in A keeps A's definition unchanged.

---

## 1. Capture triggers — where a bundle is written, and who is allowed to write it

### 1.0 The writer rule (thread model)

**R-TRIG-0 — One writer, the session thread.** A bundle commit SHALL be performed only from the thread that
owns `ctx: ContextEngine`, `ui_transcript: UiTranscript` and `renderer: &mut dyn Renderer` inside
`run_session_with_bounds_report` (`src/ui/session.rs:258`, boot order `:265` → `:273` system prompt → `:280`
`build_manager_context` → `:300-301` transcript paths → `:302` `UiTranscript::new()` → `:319`
`subagents = Vec::new()` → `:380` `ctx.set_goal` → `:391` the session loop).

Why this is a hard rule and not a preference:

- `Renderer` is `pub trait Renderer: Send` (`src/ui/mod.rs:88`) — **`Send`, not `Sync`**. The live renderer
  (`TuiRenderer`, `src/ui/tui/mod.rs:40`) is full of `Cell`/`RefCell` layout caches
  (`:100-122`, e.g. `cached_message_lines: RefCell<Vec<usize>>`), so it is not shareable. A UI capture
  (`capture_ui`, §4.7) is therefore only legal on the session thread, through `&mut *renderer`.
- `ContextEngine.messages` is private (`src/manager/context.rs:558`); the capture seam must be a method on the
  engine, called where the engine lives.
- Design A §11.3-1: `harness::fs::replace_path_lock` (`src/harness/fs.rs:342`) is **private**, so `session/`
  owns its own commit mutex (`commit::CommitLock`, §4.4). That mutex serializes writers, but writer state
  (the `UiSection` diff cache, A §7.2 C2) still lives on the session thread. The mutex is a backstop against
  a re-entrant trigger, not a licence to commit from elsewhere.
- Consequence for the panic hook (§1.8): the panic hook runs on **whichever thread panicked**, and it cannot
  touch `renderer`/`ctx`. It writes only what is reachable from process-global state.

### 1.1 Trigger table (normative)

`T-n` labels are the vocabulary for t-013 (lifecycle triggers) and for §5.

| # | Trigger | Fires at (verified site) | Frequency cap | Bundle members | `ProcessEnding` in manifest |
|---|---|---|---|---|---|
| T-1 | Manager turn boundary | end of one round of the turn loop, immediately after `ensure_pairing_at_boundary("after tool round", …)` `src/ui/session.rs:1788-1795`, and at the top-of-loop exits `:474` / turn-cap `:505-514` (which call the same repair before `break`) | once per turn (max `MAX_TURNS = 100`, `src/manager/loop.rs:51`) | full (manager + ui + workers) | `InProgress` |
| T-2 | **Pre-dispatch** (mandatory) | directly after `ctx.append(assistant); let _ = ctx.save_transcript(&transcript_path);` `src/ui/session.rs:689-690` — the whole `tool_calls` batch is on disk before any dispatch | once per assistant turn with `tool_calls` | full | `InProgress` (with `dangling: Dangling{…}`, A §3.9) |
| T-3 | Tool result landed | each `ctx.append(Message::Tool { … })` and its adjacent journal save: `:1220/1224`, `:1409/1407-1413`, `:1418/1422`, `:1437/1441`, `:1456/1460`, `:1481/1485`, `:1730/1734` | once per landed result | full | `InProgress` |
| T-4 | UI-visible event landed | the existing `UiTranscript` write points — `:382, 400, 445, 707, 844, 985, 1078, 1204, 1211, 1249, 1347, 1370, 1511, 1590, 1712, 1719, 1759, 1874, 1960` plus the helpers at `:2118` (`surface_plan_warning`), `:2173` (`ensure_pairing_at_boundary`), `:2207` (`surface_compaction_outcome`) | coalesced, see §1.4 | ui-only member write (A §7.2 C2 diff) | `InProgress` |
| T-5 | Worker lifecycle edge | worker start `src/orchestrator/mod.rs:301` (`register_active_worker_with_token`, and `fix_loop.rs:1273-1280` for fix-loop workers); complete = `ActiveWorkerGuard::drop` `src/orchestrator/workers.rs:154-211`; fail/complete surfaced at `mod.rs:354` (`DelegationEvent::Failed`) / `:360` (`Completed`), mirrored into the UI at `session.rs:1164/1200` and `:1676/1705`; cancel paths `workers.rs:781-802` / `:805-824`; preemption `preemption.rs:357` (`PREEMPT_ACK_TIMEOUT = 2 s`) with the forced-cancel loop at `:472-486` | once per edge; worker mirror itself is per-mutation (§1.3) | worker member + ui; manager member untouched | `InProgress` |
| T-6 | Abort requested | the abort break in the join loop `src/ui/session.rs:1127-1135` (`user_exit_requested` → `cancel_all()` → `break (…, Err("aborted by user"))`), the sibling breaks `:1008-1017` and `:1019-1033` (`aborted \|\| steer_abort_requested \|\| is_globally_cancelled` → `cancel_all()` + `timeout(500 ms, &mut handle)`), the turn-top break `~:516`, and mid-loop `/abort` `:425-428` | once per abort | full | `UserAbort` |
| T-7 | Two-stage Ctrl-C / Ctrl-D | the *arm* press only changes `confirm_abort` / `abort_armed_at` (`src/ui/tui/events.rs:156-169`, and the ESC arm `:44-59`); the *confirm* press calls `Renderer::request_user_exit` (`:159-162`, Ctrl-D `:171-174`) which the loop observes at `session.rs:1125` → T-6 | once per confirm | full (via T-6) | `UserAbort` |
| T-8 | Panic | the process-wide hook `src/main.rs:276-281` (today: `catch_unwind` → `ui::restore()` → `default_hook(info)`) | once per panic | **partial**: manifest + `manager.json`/`ui.json`/`workers/*` *as they were at the last successful commit*, rewritten from global state only | `PanicHook` |
| T-9 | Clean tail | session-loop tail `src/ui/session.rs:1968-1977` (`renderer.on_event(&Event::Done)`, `flush()`, `shutdown()`, `Ok(SessionStopReport { stops })`) — **today performs no final save**; and `src/main.rs:64-65` (`orchestrator::cancel_all()`, then `rt.shutdown_timeout(300 ms)`) | once per session exit | full | `CleanTail` |
| T-10 | Reset / archive | `/reset` → `handle_reset_command` (`src/ui/helpers.rs:197-218`, call sites `session.rs:358, 432, 1106, 1618, 1945`) → `Plan::clear` (`src/manager/phase.rs:381`, journal deletions `:407-421`); `Plan::archive` (`:537`, deletions `~:562-569`) | once | **delete the bundle** (A D3, C5 sweep), no new bundle | `Reset` / `PlanArchived` marker file only if deletion is blocked (A §7.4) |
| T-11 | Hard-kill (inference, not a writer) | none — see §1.9 | n/a | n/a | loader reports `HardKillInferred` |

### 1.2 Why T-2 is mandatory rather than opportunistic

A §8 H1 and A §9.10 (AC-D1) exist because of one verified ordering: `session.rs:689-690` persists the assistant
turn — with its entire `tool_calls` batch — **before** the fan-out at `:852/876/905/923` starts any tool, and the
pairing repair only runs at the round boundary (`:1788-1795`). A kill between those two points is the common
crash shape. Capture at T-2 is what records `DanglingToolCallState::Dangling { calls: [ … dispatched:
true|false … ] }` (A §3.9): the per-call `dispatched` bit can only be honest if the commit happens at the
dispatch boundary, from the same code that is about to push the `JoinHandle`. The committer at T-2 SHALL mark
calls `dispatched: false` for everything not yet pushed and `dispatched: true` as each handle is pushed, by
re-committing the `manager.json` member after the fan-out completes (`:936` onward). Re-committing one member
is cheap because A §7.2 C2 skips members whose bytes are unchanged.

### 1.3 Cost model: mirror-out vs full commit

Two different clocks, and conflating them is the main cost risk:

- **Worker mirror (per mutation, in memory only).** `worker_journal::WorkerJournal` (§4.8) mirrors **every**
  mutation of a specialist's `ContextEngine` — `append` at `execution.rs:255, 490, 539, 559, 584, 613, 764`,
  `replace_last` at `:524, 552, 573`, and the fix-loop appends at `fix_loop.rs:736, 740, 777, 954, 1046, 1147`
  — plus the collapse on `compact()` / `inject_rebirth_advisory()` (`execution.rs:1033-1042`). It SHALL be an
  in-memory `Vec<MessageRecord>` clone under a `Mutex`, and SHALL NOT touch disk. Cost: one `Message` clone per
  mutation, on the worker thread.
- **Bundle commit (per trigger, on disk).** Only the triggers in §1.1 write. Per A §7.2 the committer diffs
  against the bytes of the last successful commit (C2) and writes changed members `workers → ui → manager`
  (C3) with `manifest.json` **last** (C4). A typical T-5 edge therefore rewrites one worker file and the
  manifest, not the whole bundle.

**R-TRIG-1 (coalescing).** A bundle commit SHALL NOT be started while another is in flight (the
`CommitLock::guard()` mutex, §4.4): the second request SHALL set a `dirty` flag and the writer SHALL perform
exactly one follow-up commit after the first completes. **R-TRIG-2 (rate floor).** T-4 and T-5 SHALL be
throttled by `Config.session_commit_min_interval_ms` (§3.1, default `250`); T-1, T-2, T-3, T-6, T-8, T-9 are
**never** throttled — losing them loses facts. **R-TRIG-3 (no throttling of a mandatory trigger by a pending
throttled one):** a mandatory trigger always wins and cancels the pending one. **R-TRIG-4 (mirror cap).** The
mirror SHALL hold at most `Config.session_worker_mirror_max_messages` (default `4000`) records and SHALL record
a `NonRestorableNotice { kind: WorkerHistory, subject: <worker_key> }` (A §3.11) once the cap bites, so
truncation is reported rather than silent.

### 1.4 What T-4 means for the 22 legacy UI write sites

Design A D2 deletes `UiTranscript::save`/`load`, and A §6.1 makes `UiSection` (pane text) the authority, with
`UiJournalRecord` (`A §6.4`) demoting `UiRecord`. For the flow this means the 22 sites listed in T-4 keep their
*event* semantics but change their *sink*: each becomes `ui_transcript.append(record)` (in-memory, unchanged)
followed by a **T-4 commit request** instead of `let _ = ui_transcript.save(..)`. The trigger SHALL NOT be
conditional on the record kind — the pane text is the authority (A D4), and `TuiRenderer.messages` changes in
places the journal never records (streamed `current_content` / `current_thought`, `SubagentMessage` /
`SubagentThinking` forwarded at `src/orchestrator/preemption.rs:216-238`), which is exactly why A U1 forbids
re-rendering. Because `capture_ui` is a `&self` read of `renderer` (§4.7), T-4's capture is legal from the same
thread that owns it; it is the *commit* that is throttled.

### 1.5 What each trigger records about the ending

`ProcessEnding` (A §3.4) is written by the writer, never inferred. Mapping is fixed: T-1..T-5 → `InProgress`;
T-6/T-7 → `UserAbort`; T-8 → `PanicHook`; T-9 → `CleanTail`; T-10 → deletion, and a blocked deletion leaves
the marker file named by A §7.4 (`Reset`, `PlanArchived`). `Signal { name }` is **never written by this
binary**: no signal handler exists (§1.9), so nobody can observe a signal. `HardKillInferred` is loader-only
(A §3.4, §7.5): `ending == InProgress` in a manifest that survives means the process died without reaching
T-6..T-9.

### 1.6 Ordering constraints between triggers

1. T-2 SHALL run before the first `handles.push(..)` of a round (`:852-934`), and SHALL have returned (not
   merely been requested) — otherwise `dispatched: true` claims a dispatch that a reader cannot verify.
2. T-1 SHALL run after `ensure_pairing_at_boundary` (`:1788-1795`) so the committed manager transcript is
   already pair-valid; a trigger that commits a torn transcript forces `dangling: Dangling` and a needless
   normalization pass on every resume.
3. T-6/T-9 SHALL run **after** `orchestrator::cancel_all()` (`bus.rs:39-43`) so the recorded `status_label`
   (`workers.rs:431`) is the post-abort label, matching A I-W5 ("byte-identical to the pre-crash label").
4. T-5 SHALL NOT hold a `DashMap` guard while committing (A §5.7 step 4; `workers.rs:171-190` is the
   codebase's own precedent, and `Drop` must not panic, `workers.rs:196-207`).
5. T-9 SHALL run before `renderer.shutdown()` (`session.rs:1973`) — the UI capture needs a live renderer — and
   before `main.rs:65` `rt.shutdown_timeout(300 ms)` retires the runtime.

### 1.7 Abort path detail (T-6 / T-7)

The two-stage Ctrl-C in `src/ui/tui/events.rs:156-169` **arms** (`confirm_abort = true`,
`abort_armed_at = Some(Instant::now())`, status line "Abort armed. Press Ctrl+C or ESC again to abort."); the
second press calls `Renderer::request_user_exit(self)` (`src/ui/mod.rs:115`, TUI override
`src/ui/tui/mod.rs:928-931`). Nothing in the *arm* half may capture: the user may disarm (ESC `:44-59`,
`clear_abort` `tui/mod.rs:933-944` also calls `orchestrator::reset_cancellation()` `bus.rs:64-68`). The commit
therefore hangs off the loop's observation of `renderer.user_exit_requested()` (`session.rs:1125`), which is
the single point every exit route converges on, and it must land **before** the `break (…)` at `:1128-1135`
discards the in-flight `JoinHandle`s. Same for the wall-clock/cancel breaks `:1008-1017` / `:1019-1033`.
A §9.9 AC-A1 asserts exactly this shape (`ending: UserAbort` with an in-flight worker restored as
`completed_seq: None`).

### 1.8 Panic hook (T-8) — what a panic can and cannot capture

`src/main.rs:276-281` today does `catch_unwind(AssertUnwindSafe(|| ui::restore()))` then chains the default
hook. t-013 adds a `session::commit::commit_from_panic(...)` call **inside the same `catch_unwind`, before
`default_hook(info)`**, and it is deliberately narrower than a normal commit:

- It SHALL NOT dereference the renderer or the engine (R-TRIG-0, and `TuiRenderer` is `!Sync` §1.0).
- It SHALL write from process-global state only: `WORKERS` (`workers.rs:106`), the worker mirror registry
  (`worker_journal::registry()`, §4.8), `session::commit::LAST_COMMIT` (the member set and the raw bytes of the
  last successful commit, §4.4), and `Plan::session_bundle_path()`.
- Net effect: a rewritten `manifest.json` whose `ending` is `PanicHook`, carrying the member bytes from the
  last successful commit. That is not a lie: A §7.2 C2 says an unchanged member keeps its bytes, and the
  manifest is the thing that changes. If the panic happened before any successful commit, T-8 SHALL write
  nothing and log why.
- It SHALL be pure-sync (no `block_on`) — A §7.5 note "the panic-hook path performs no C5 sweep (it must stay
  pure-sync)".

### 1.9 Hard-kill inference (T-11): why there is no signal handler to hook

Verified at HEAD: `grep -rn "tokio::signal\|ctrl_c\|signal(" src/` returns **no** handler (the only hits for
"signal" are the preemption `PauseSignal` machinery in `src/orchestrator/preemption.rs:133, 191, 239-248`), and
`Cargo.toml` pulls no `ctrlc`/`signal` crate. `kill(2)`/`kill -9`/OOM-kill therefore run **no** user code:
there is no place to write a final marker, and the OS reclaims the process silently. Two related facts make
this a genuine, not theoretical, gap:

- `harness/pty.rs:1560-1570` `kill_process_group` is the only kill site and there is **no `Pdeathsig`**
  anywhere, so tool children may outlive the process and keep mutating the workspace (A §10.2
  `NonRestorableKind::PtyProcess`).
- `impl Drop for TuiRenderer` (`tui/mod.rs:1052-1056`) and the five `Drop` impls in
  `docs/research_restore_lifecycle.md` §1.3 persist nothing.

**Decision (B):** t-006 SHALL NOT add a signal handler in this release. The classification rule is instead
read off the manifest, exactly as A §3.4/§7.5 define:

```text
manifest.ending == InProgress  -> the last commit was an in-loop one (T-1..T-5);
                                  the process ended without reaching T-6..T-9
                               -> RestoreOutcome reports ending == HardKillInferred
                                  (the stored InProgress is kept verbatim in the file;
                                   A §7.3 L3-L5 verifies bytes, never rewrites the value)
manifest.ending == CleanTail   -> T-9 ran; nothing crashed
manifest absent, members exist -> torn commit; A §7.3 L0 -> NoBundle + orphan count
```

Because `HardKillInferred` is the *expected* value for most real resumes, every "previous ending" statement the
restore prints SHALL name the manifest's stored `ending` and the inference together (A §10.7 rule 2: zero
notices for a non-clean ending is a defect). **R-TRIG-5:** adding a handler later is an *additive* change — it
would write `Signal { name }` and would not change any rule above.

### 1.10 Capture failure bookkeeping

A D6 forbids `let _ =` on a commit. Every trigger SHALL thread the `session::commit::CommitReport` (§4.5)
into a per-session `CaptureLedger` (in `session::mod`): counters `commits_ok`, `commits_failed`, last error,
and the member names that failed. On failure the session SHALL keep running (a persistence failure is not a
reason to kill a working manager — that would turn a bug in the new subsystem into a regression in the old
one) but SHALL surface one `Event::Status` warning naming the path and the OS error, and SHALL record a
`NonRestorableNotice { kind: Other, subject: "session/<bundle>" }`. Design A §7.5's fail-closed table governs
the **load** side; the capture side's obligation is loudness plus a persisted fact, never a silent swallow.

---

## 2. Resume policy for in-flight workers — one policy, decided

### 2.0 The ground truth the policy has to survive

Three verified facts, and they are load-bearing:

1. **A task cannot be reattached.** Manager-side tool/delegation work is dispatched as
   `tokio::task::spawn_blocking(move || …)` and collected as `Vec<JoinHandle<_>>` at
   `src/ui/session.rs:852`, `:876`, `:905`, `:923` (plus `:1494` for the single-call path). A `JoinHandle` is a
   handle to a task in *this* process's runtime: after a restart there is no handle to re-acquire, no way to
   `abort()` it, and no way to observe its result. Worker-side teardown is cooperative-only
   (`docs/research_restore_lifecycle.md` §1.5: `handle.abort()` detaches the outer handle; the future stops only
   at a cancellation checkpoint).
2. **The completion bookkeeping is a guard, not a record.** `ActiveWorkerGuard(pub String)`
   (`src/orchestrator/workers.rs:152`) and its `Drop` (`:154-211`) are what flip a `WORKERS` entry to completed
   (`COMPLETED_SEQ.fetch_add` `:166-167`), evict the completed ring (`:172-190`, cap
   `MAX_RECENT_COMPLETED = 10` at `:103`) and reclaim the notice inbox (`:211`). None of that runs after a
   kill, and none of it can be reconstructed by a loader — which is why A §5.3 restores `completed_seq`
   verbatim instead of re-deriving it.
3. **A worker's message history was unreachable.** The specialist's `ContextEngine` is a local inside
   `run_specialist_live_inner` (`src/agents/runner/execution.rs:175`, built at `:250` by
   `build_specialist_context` `:131`). Nothing outside that function could see it, so the prior art re-ran
   workers from the brief with zero accumulated history. Design A §5.5 + A §3.8 `HistorySource` fix the
   *capture* side (`LiveMirror` / `SeedOnly` / `NotCaptured`); the mirror API is §4.8 here.

Consequence: **the execution is gone; only its record survives.** Any policy that pretends otherwise is a lie.
A §9.9 AC-A3 states the same constraint from the other end: "nothing in the restore path re-launches the
worker (t-006 owns re-launch)". This section is that decision.

### 2.1 The three candidate policies

| Option | What it would do | Why it fails / why it is kept |
|---|---|---|
| **O1 — requeue from the brief** (prior art) | For every `inProgress` worker, re-dispatch a fresh specialist from `WorkerSection.brief` / `snippets` (the `DelegationRequest` seed, A §5.3) | Throws away `history` even when `history_source == LiveMirror` — i.e. throws away the one thing this whole design buys. Re-executes every tool call the old worker already executed: files written twice, `run_command` side effects applied twice, PTYs doubled (A §10.2 `PtyProcess`). It is also what `31f4e90:src/orchestrator/recovery.rs:432/:471` did and why it "restored" sessions that had to redo all their work. **Rejected.** |
| **O2 — continue from restored history** (auto-resume) | Rebuild the engine from `history`, re-register the worker, and immediately run one more turn so the worker picks up mid-thought | Tempting and partially honest, but three hard blockers: (a) the turn would re-issue the *pending* `tool_calls` that were in flight at capture — A §5.4 says a `dispatched: true` call plus a surviving PTY is a **double-apply**; (b) re-entering `run_specialist_live_inner` re-enters the fix-loop/validator state machine whose bounded counters (A §5.3 `counters`) would be re-armed by a different process than the one that spent them; (c) re-launch means minting a `JoinHandle` the manager never asked for, so the manager's own transcript (a `tool_calls` entry whose result is being synthesized to `"(aborted)"`, A §8.1 N2) contradicts a live worker underneath it. **Rejected as an automatic action; kept as an explicitly *manager-initiated* action** (O3-C below). |
| **O3 — mark interrupted** | Restore the record and every byte of mirrored history, close the execution honestly, and hand the decision back to the manager | Satisfies "same in-flight subagents" for everything that is restorable (identity, key, task binding, status, tokens, counters, brief, pane text, history) while never claiming a capability that cannot survive. The unavoidable losses are named by A §3.11 notices. **Chosen.** |

### 2.2 **P-R1 (the policy): RESTORE-AS-INTERRUPTED, MANAGER-DECIDES, NEVER AUTO-RELAUNCH**

For every restored worker whose stored `status == WorkerStatus::InProgress` (`A §3.8`):

1. **Install the record, not the execution.** `WORKERS` gets the entry under the stored `worker_key`
   (`A §5.7` step 2: `WORKERS.insert`, never `claim_worker_slot` / `register_active_worker_with_token`
   (`workers.rs:229` / `:309`), which would re-draw `WORKER_ID_SEQ` and disambiguate the key). `completed_seq`
   stays `None` so `has_active_workers()` (`workers.rs:483`) and every cancel path (`:781-824`) see it — A
   AC-A1 requires exactly this.
2. **Close the execution half.** The boot function (R4, §4.9) SHALL immediately, and in the same pass:
   - set `status_label = "Interrupted by restart"` through `set_active_worker_status` (`workers.rs:431`) — the
     only registry mutation this policy performs, and the subagent list shows it verbatim;
   - keep `status == InProgress` on disk and in memory (the record of *what it was doing*, which is what A
     I-W5 pins: `inProgress ⇔ completed_seq == None`), so the entry stays visible to cancel/steer routing,
     which is the honest state: it is still an open delegation that nobody is working on;
   - mint a fresh `CancellationToken` and attach it to the entry, so a later `/abort`, `cancel_active_worker`
     (`:781`) or `cancel_all_active_workers` (`:805`) cannot be confused by a `None` token — A §5.7 step 5
     permits minting only for a re-launched worker, so t-008/t-010 SHALL mint it as part of the *interrupted*
     projection and emit `NonRestorableNotice { kind: CancellationToken, … }` naming the old token (A §3.11,
     §10.1: "the restored worker cannot be cancelled through the old token");
   - **not** create an `ActiveWorkerGuard`. The guard's `Drop` (`:154-211`) is the only thing that draws
     `COMPLETED_SEQ` and evicts the ring; synthesizing it would fabricate a completion the worker never had.
     The interruption is instead closed by an explicit API call (`restore::retire_interrupted_worker`, §4.9)
     that the manager triggers when it re-decides — see step 5.
3. **Say what is gone.** Per worker, emit the A §3.11 set: `CancellationToken`, `JoinHandle`, `DropGuard`,
   plus `PtyProcess` whenever any `DanglingCall.dispatched == true` (A §5.4, AC-D4), plus `PendingNotice` for
   each notice that was in `PENDING_NOTICES` / `WORKER_INBOXES` (`notice.rs:76, 81`) at capture time
   (`drain` helpers `:668/:682/:686`). A §10.7 rule 1: union, deduplicated by `(kind, subject)`; rule 2: a
   non-clean ending with zero notices is a defect.
4. **Never re-execute.** The restore path SHALL NOT call `harness::dispatch*`, `run_specialist_live`
   (`execution.rs:160`), `try_run_specialist_live` (`runner/mod.rs:62`), or any tool. AC-D4: "no tool is
   re-executed by the restore path".
5. **Give the manager the facts and let it re-decide.** §2.4 spells the synthetic message. The manager then
   chooses among: *re-delegate* (a **new** worker, new key, new brief — the restored record is retired),
   *retire* (accept the interruption, close the record), or *continue with the mirrored history* (O3-C below).

### 2.3 Per-status behaviour (normative table)

| Stored `status` (A §3.8) | Live registry after boot | Subagent pane (A §6.1/§6.3) | Manager is told | Re-launch allowed? |
|---|---|---|---|---|
| `InProgress` + `history_source == LiveMirror` | entry present, `completed_seq: None`, `status_label: "Interrupted by restart"`, fresh token | restored byte-exact (`is_active: true`, `logs`/`thinking`/`content` verbatim, `context_tokens` non-zero), **then** one appended log line `"[interrupted by restart — history restored, execution not resumed]"` | notice `N-MGR-1` (§2.4) + one `Event::Status` per worker | only by manager action: **O3-C** (§2.5) |
| `InProgress` + `history_source == SeedOnly` | same, plus `unrecoverable_reason: None` (it *is* restorable, just thin) | same + the append above + a second line `"[history not captured: brief only]"` | `N-MGR-1` naming `WorkerHistory` explicitly | **No.** Brief-only means O1 is the only option, and O1 is rejected. Retire, or re-delegate fresh. |
| `InProgress` + `NotCaptured` | same | same, `logs` may be empty | `NonRestorableNotice { kind: WorkerHistory }` + `N-MGR-1` | No |
| any status with ≥1 `DanglingCall.dispatched == true` | as above, and the entry is flagged `side_effect_risk = true` in the boot report | pane gets `"[⚠ a tool call was dispatched and may have applied changes]"` | `N-MGR-1` gets a `PtyProcess`/`Other` clause listing each `tool_call_id` + `function_name` + verbatim `arguments` (A §3.9 `DanglingCall`) | Only **retire** or **fresh re-delegate with a new brief**; O3-C is blocked (the pending call is exactly the double-apply) |
| `InProgress` + worker file failed integrity (A §7.5, ≥2 corrupt ⇒ `Refused`; 1 corrupt ⇒ below) | entry present but `status = Unrecoverable`, `unrecoverable_reason = "…fingerprint mismatch…"` (A §3.8) | `is_active: false`, name/task kept, `logs` = one line naming the refusal reason | `NonRestorableNotice { kind: Other }` + `PartiallyRestored.unrecoverable_workers` (AC-D7) | No |
| `Completed` / `Failed` / `Aborted` | entry inserted with its stored `completed_seq` (ring cap applied, `completed_dropped_by_ring_cap` reported, A §5.7 step 3) | restored verbatim, `is_active: false` | nothing extra — the manager's own transcript already carries the result | No (nothing to resume) |
| `Unknown` (label not recognized, A §5.3) | present, verbatim `status_label` kept | restored verbatim + `"[status label not recognized: <label>]"` | one `warnings` entry with both sides | Manager decides; treat as `InProgress` for re-decision purposes |

### 2.4 How the manager is told (exactly one synthetic message)

The manager's transcript is the *only* channel the manager reads, so the fact goes there — and only there once,
with the UI notices as mirrors (A §8.4 three destinations).

- **`N-MGR-1` is a `Message::User`** appended **after** the normalized restored vector (A §8 N0–N6) and
  **before** any new user input, by the boot function (R4). It is a `User` variant because
  `pinned_prefix_end` (`src/manager/context.rs:270`) scans `System | User` by content and `compact()`
  (`:751`) keeps `messages[..prefix_end]`: a `System` insert would be a re-role (forbidden, A R-PIN-3), and a
  `User` insert at index ≥ 2 cannot disturb the pins (A §4 R-PIN-1 fixes `[0]`/`[1]`).
- Its content SHALL begin with a **new** registered marker, so the runtime recognizes it as its own
  injection and `perform_rebirth` (`context.rs:935`, the `skip(2).rev()` scan at `:949-957`) never mistakes it
  for a genuine instruction: `session::format::SESSION_RESTORE_NOTICE_PREFIX = "(SYSTEM: SESSION RESTORED"`.
  t-008 SHALL add that prefix to `INJECTED_ADVISORY_PREFIXES` (`context.rs:305-309`) — the only place the
  grammar recognizes injections (A R-PIN-4). This is a *manager-grammar* change and therefore belongs to the
  task that owns the manager capture seam (t-009), not to the loader.
- Shape (one message per restored session, workers listed inside it — never one message per worker, which
  would re-price the context `n` times):

```text
(SYSTEM: SESSION RESTORED. This process restarted from .marmel/session (session id <id>,
previous ending: HardKillInferred, stored ending: InProgress). The work below was in flight
when the previous process stopped. Its history is restored; its execution is NOT running —
no task exists to continue it and no result will ever arrive for its pending calls.

  [1] coder-t-006          status: In Progress -> Interrupted by restart
      task t-006 | 41 turns | 12 validation rounds | 18 431 ctx tokens | worked 6m12s
      history: restored (112 messages, live mirror)
      pending tool calls: run_command (dispatched: yes -> the command may have run)
      lost: CancellationToken, JoinHandle, DropGuard, PtyProcess
      -> do NOT assume the task is finished; do NOT re-run the dispatched command blindly

  [2] researcher-w3        status: Completed  | 14 turns | result recorded in this transcript

To continue item [1], either delegate the task again from a fresh brief, or call
continue_worker with the worker key above to resume it from its restored history.
Decide from the current workspace state, not from this notice.
```

- The message SHALL be generated from the restored `WorkerSection`s by `session::restore` and SHALL NOT
  invent status it did not read: every field above is a field of `WorkerSection` (A §5.3) or of
  `DanglingCall` (A §3.9). A §10.7 rule 4 (all three destinations, no `let _ =`) applies to the mirrored
  copies: `Event::Status` for the status line, one `UiJournalRecord` (A §6.4), `tracing::warn!`, and the pane
  line from §2.3.
- **`Tool` placeholders stay as A §8 says.** The manager's own dangling `tool_calls` from `session.rs:689-690`
  are healed by N2 into `Message::Tool { tool_call_id, content: ABORTED_TOOL_RESULT }` — the manager sees its
  own call answered `"(aborted)"`. `N-MGR-1` explains *why* it says `"(aborted)"`; it does not replace it.

### 2.5 **O3-C** — the only sanctioned way to continue a worker: manager-initiated, one turn, no pending calls

Because the manager must be able to re-decide without side effects, B defines a narrow continuation seam —
**available but never automatic**:

- A new manager tool, working name `continue_worker { worker_key, instruction }`, whose handler (a) takes the
  restored `WorkerSection.history`, (b) runs the pairing check **again** on it and refuses if any pending
  `tool_calls` remain, (c) re-enters the specialist through the *same* seam a delegation uses
  (`runner/mod.rs:62` → `execution.rs:160`) with the mirrored history installed in place of the fresh seed
  (i.e. `build_specialist_context` (`execution.rs:131`) plus a new `install_restored_messages` call, §4.6),
  and (d) re-registers under the **existing** key (no `claim_worker_slot` re-mint, A §5.7 step 2).
- Hard gates: `DanglingToolCallState::Balanced` or `Healed`; `history_source == LiveMirror`; not flagged
  `side_effect_risk`; the worker is currently `InProgress`. Any gate failure returns a normal tool error that
  names the gate, so the model can choose to re-delegate instead.
- The tool is **not** part of this task's deliverable beyond its contract here; §6 R-8 assigns it to t-013
  (trigger wiring) with the handler owned by t-010.
- Effect on §2.2 step 2's open-ended entry: choosing O3-C or "re-delegate" SHALL be followed by
  `restore::retire_interrupted_worker(key)` (§4.9), which is the explicit, non-`Drop` way to close a restored
  record: it draws `COMPLETED_SEQ` **through the existing code path** and sets the label, so the ring and the
  identity floors stay consistent with A §5.7.

### 2.6 What "the same in-flight subagents" honestly means (the promise boundary)

Restored **exactly**: worker key and identity (`worker_id_seq_value`, plus the floors of A §5.7), role,
`task_id` binding, `status`, `status_label` (before the interruption line), `completed_seq`, context tokens,
counters (`implementation_turns`, `validation_rounds`, `latest_validator_feedback`), brief/snippets/URLs,
mirrored `history`, and every pane byte (A §6, U1–U5). Restored **approximated**: durations, re-derived from
`TimePoint` pairs (A §5.6). Lost, loudly, per worker: `CancellationToken`, `JoinHandle`, `DropGuard`
completion semantics, `PtyProcess` children, queued-but-undelivered notices (`PendingNotice`),
`HarnessStats` intervention counters, the tool-schema charge (`ToolSchemaCharge`), and the *execution* itself.
Anyone reading this policy should read that last item as the headline: the resume is exact about **state**,
and explicitly non-exact about **running work**.

---

## 3. Config, env and CLI surface

### 3.1 `Config` / `PartialConfig` keys (exact)

Style follows the existing flat top-level keys (`command_timeout_secs`, `sandbox_disabled`, `ui_mode`,
`max_thinking_tokens`, all at `src/config.rs:121-170`) rather than a new `[session]` table. A nested table
would need a third `Partial*` mirror (`PartialMonitoringConfig` `:397`, `PartialOrchestrationConfig` `:410`
are the two that exist today) and every mirror is a place keys silently drift — see the drift warning below.

Add to `Config` (`src/config.rs:121`, after `max_fix_loop_rounds`):

```rust
/// Session capture/rehydration master switch (Design A D2: replaces the
/// deleted `enable_rehydration`). Default `true`: a bundle is always written.
pub session_enabled: bool,
/// Rehydrate from the bundle at boot. Default `false`: a fresh start is the
/// default behaviour; `--resume` / `MARMEL_RESUME` turn it on for one run.
pub session_resume: bool,
/// Floor between throttled commits (T-4 / T-5, flow §1.3 R-TRIG-2). Default 250 ms.
pub session_commit_min_interval_ms: u64,
/// Cap on the in-memory per-worker mirror (flow §1.3 R-TRIG-4). Default 4000 records.
pub session_worker_mirror_max_messages: usize,
/// Commit at every landed tool result (T-3). `false` keeps T-1/T-2/T-6/T-9 and
/// drops T-3/T-4: fewer writes, coarser crash granularity. Default `true`.
pub session_commit_on_tool_result: bool,
/// Soft ceiling on total member bytes before the committer stops writing
/// throttled triggers and warns once. Default 32 MiB.
pub session_max_bundle_bytes: u64,
```

Add the mirror fields to `PartialConfig` (`src/config.rs:372-395`, same order) as `Option<bool>` /
`Option<u64>` / `Option<usize>`, and the corresponding arms in `merge` (`:416`), which is a plain
`if let Some(v) = partial.<key> { base.<key> = v; }` chain (`:417-470`). `Config` keeps its struct-level
`#[serde(default)]` (`:120`), so a config file that mentions none of the new keys still loads.

Defaults go in `impl Default for Config` (`:174+`): `session_enabled: true`, `session_resume: false`,
`session_commit_min_interval_ms: 250`, `session_worker_mirror_max_messages: 4000`,
`session_commit_on_tool_result: true`, `session_max_bundle_bytes: 32 * 1024 * 1024`.

**Drift warning — why tests are mandatory, with the in-repo proof.** `PartialConfig` has **no**
`#[serde(deny_unknown_fields)]` (contrast every `session::format` type in A §3, which does have it), so a
misspelled key in `marmel.toml` is silently dropped by `toml::from_str::<PartialConfig>` (`:316`). This drift
already exists: `Config.max_fix_loop_rounds` (`src/config.rs:171`) has **no** counterpart in `PartialConfig`
(`:373-395`), so that key is today unsettable from the config file and nothing complains. Therefore t-007
SHALL add, in `src/config.rs`'s test module (existing pattern: `let partial: PartialConfig =
toml::from_str(toml_str).expect("parses")` at `:656, 680, …`):

- **T-CFG-1** — one test per new key: TOML snippet → `merge(Config::default(), partial)` → assert the resolved
  value. Six keys, six tests. (This is the only mechanism that catches a missing `PartialConfig` field.)
- **T-CFG-2** — a source-text guard, following the repo's existing source-scanning tests
  (`config.rs:1179`, `harness/sandbox.rs:828`): for each `session_*` key, assert the name appears in
  `Config`, in `PartialConfig`, inside `merge`, and in `marmel.toml.example`.
- **T-CFG-3** — an unknown-key guard for the *bundle*, not for config: A §3 already puts
  `deny_unknown_fields` on every format type and A §7.3 L3 maps a violation to
  `RefusedCode::UnknownField`; the test asserts a `session_enabled`-style typo inside a bundle member is a
  refusal. This is the counterpart that keeps the two vocabularies from drifting apart.
- `marmel.toml.example` gains a commented block (sections today: `[monitoring]` `:46`, `[orchestration]` `:64`,
  `[mcp_servers.*]` `:137`) documenting all six keys with their defaults.

### 3.2 Environment overrides

`config::load` (`src/config.rs:311-337`) applies env after file merge, in this order:
CLI `--config` → `./.marmel.toml` → `~/.config/marmel/config.toml` → env → defaults. New env keys follow the
existing `MARMEL_*` shape (`MARMEL_AUTH_TOKEN` `:319`, `MARMEL_BACKEND_URL` `:324`, `MARMEL_MODEL` `:329`):

| Env | Type | Meaning | Precedence |
|---|---|---|---|
| `MARMEL_RESUME` | `1`/`true` ⇒ true | resume for this run; equivalent to `--resume` | flag beats env beats file beats default |
| `MARMEL_SESSION_DISABLE` | `1`/`true` ⇒ disables capture | **one-way switch**: can only turn capture *off*, never on | OR'd into `!session_enabled`, mirroring how `MARMEL_DISABLE_SANDBOX` ORs with `sandbox_disabled` (`config.rs:147`, `sandbox_opt_out` `:306`) |
| `MARMEL_SESSION_COMMIT_MIN_MS` | integer ms | throttling floor | beats file |

One-way disable is deliberate: capture-on is the safety property the whole design rests on (a run that
captured nothing can be re-started by nobody), so an env var must not be able to turn it on against an explicit
`session_enabled = false` in the file.

### 3.3 The `--resume` flag (hand-rolled parser)

`CliArgs` (`src/main.rs:8-16`) gains `resume: bool` and `fresh: bool`. `parse_args` (`:196-235`) gains, inside
the existing `match arg.as_str()` (`:200-230`) **before** the `other =>` fall-through at `:223`:

```rust
"--resume" => args.resume = true,
"--fresh"  => args.fresh = true,
```

Placement matters: the `other =>` arm treats an unrecognized argument as the positional `PROMPT`
(`:224-226`) and only prints "ignoring extra argument" for a second one (`:227`), so a forgotten arm turns
`--resume` into the session's goal text. Update the help block (`:214-219`) with both flags and the
`USAGE:` line. `std::process::exit(0)` for `-h` (`:221`) and `exit(2)` for a valueless `--config` (`:206`) are
unchanged.

Resolution, computed once in `main` (`src/main.rs:28-33`, after `config::load`):

```text
resume_requested = args.resume || env MARMEL_RESUME || cfg.session_resume
                 && !(args.fresh)                       // --fresh always wins, and is the escape hatch
capture_enabled  = args.* none required
                   && cfg.session_enabled
                   && !truthy(env MARMEL_SESSION_DISABLE)
```

`--resume` **and** `--fresh` together is a usage error → `eprintln!` + `std::process::exit(2)`, matching the
`--config`-without-a-value precedent (`:205-206`). `--resume` while capture is disabled is the same usage
error: resuming a bundle this run would never have written is not a meaningful state.

Plumbing: `resume_requested` is passed down as one new parameter on the session entry points
(`run_session` `src/ui/session.rs:220`, `run_session_with_bounds` `:238`,
`run_session_with_bounds_report` `:258`) and on `ui::tui::run` (`src/ui/tui/mod.rs:1070`) /
`ui::raw::run` (`src/ui/raw.rs:20`). It is NOT smuggled through `Config`, because `Config` is process-global
(`config::set_active`, `:202`) and the resume decision is per-run. The alternative — mutating `cfg.session_resume`
in `main` — is what the `args.debug` precedent does (`main.rs:29-31`), and it is explicitly rejected here: the
bundle-load decision is read at exactly one place (R4, §4.9) and should take an explicit argument.

### 3.4 Fresh start vs resume: what differs at boot

| Step (verified) | Fresh (today, unchanged) | Resume (`--resume`) |
|---|---|---|
| `main.rs:28-33` config + `set_active` | unchanged | unchanged |
| `main.rs:46-54` runtime + MCP boot | unchanged | unchanged (MCP tools must be re-advertised; `ToolSchemaCharge` notice, A §3.11) |
| `main.rs:56` `boot_manager` (`:187-195`) | unchanged | unchanged (plan file and `Plan::default()` at `phase.rs:269` are the live plan the bundle belongs to) |
| `session.rs:273` `load_system_prompt_with_plan_and_warning` (`helpers.rs:111`) | run | **skipped on the restore branch** — A R-PIN-2: composing the prompt from live `Config` + plan state is what makes `[0]` different across a restart; the difference becomes a `warnings` entry instead |
| `session.rs:280` `build_manager_context` (`:182`, `set_system_prompt` at `:184`) | run | engine is still constructed (budgets, tool-schema charge) but the boot function installs the restored vector wholesale through `install_restored_messages` (§4.6) and SHALL NOT let `set_system_prompt` / `set_goal` write `[0]`/`[1]` |
| `session.rs:300-301` transcript paths | `plan.transcript_path()` / `ui_transcript_path()` (deleted per A D2) | `plan.session_bundle_path()` (new, §4.2) |
| `session.rs:302` `UiTranscript::new()` | unchanged | replaced by the restored `UiSection` + a reconstructed journal, then `renderer.restore_panes(&ui)` (§4.7) |
| `session.rs:319` `subagents = Vec::new()` | unchanged | the restored `SubagentPanel`s → `SubagentDetail` + `renderer.set_subagents` (`ui/mod.rs:127`) |
| `session.rs:380` `ctx.set_goal(goal)` + `:382` `UiRecord::User` + save | run | **skipped**: the goal is `messages[1]` (REQ-CORE-002, A §4). If `--resume` is combined with a positional `PROMPT`, the prompt is appended as an ordinary `Message::User` *after* `N-MGR-1` (§2.4) and never becomes `[1]` |
| `session.rs:346-377` goal prompt / `read_input` | run | skipped: `initial`/goal already exist, so the loop must not re-ask (the early `return Ok(SessionStopReport::default())` paths at `:353-354` / `:373-374` would otherwise end a resumed session before its first turn) |
| `session.rs:389` `reset_cancellation()` | run | run — the fresh global token (`bus.rs:64-68`) is what the re-minted per-worker tokens hang off |
| `session.rs:1968-1977` loop tail | today: no final save | T-9 commit with `CleanTail` |

### 3.5 Missing / refused bundle, and exit codes

| Situation | Decision (B) | Output | Exit code |
|---|---|---|---|
| No bundle, no `--resume` (T-0 default) | fresh start, capture on | one `Event::Status` + one `tracing::debug!` line naming the bundle path | unchanged (`0` on success, `1` if the session itself errored, as today via `main() -> Result<()>`) |
| No bundle **with** `--resume` | **fresh start**, loudly (A §7.3 L0 `NoBundle`; A D2: legacy `.session_transcript.json` / `.ui_transcript.json` are named as "found, not restorable" and never merged) | `Event::Status` + `UiJournalRecord` + `tracing::warn!` naming the searched path, the orphan-member count if any, and the legacy-journal warning | `0` (the run is a normal fresh run) |
| Bundle refused (`RestoreOutcome::Refused`, any `RefusalCode`) with `--resume` | **hard stop.** Do not start a fresh session under a flag that asked for the old one — silently replacing a refused resume with a new session is the fail-open behaviour A §11.2/D6 exists to remove | the refusal `code`/`member`/`message`, the computed `byte_len`/`fingerprint` pair, the bundle path, and "the bundle was NOT modified" (A §7.5) | **new named constant** `session::EXIT_RESTORE_REFUSED: i32 = 3` |
| Bundle refused without `--resume` | irrelevant (nothing was asked) and capture continues over the same path, with the refusal reported once as a warning | `tracing::warn!` + one `Status` | unchanged |
| `PartiallyRestored` (`unrecoverable_workers` non-empty, A AC-D7) | resume, with the unusable workers projected as `Unrecoverable` | the report, per worker | `0` |
| `Restored` with a large report (prunes/syntheses) | resume | the report always, never only in the log (A §8.4) | `0` |
| `--resume` + capture disabled, or `--resume --fresh` | usage error | `eprintln!` | `2` |

Exit-code mechanics: `main` returns `Result<()>` (`src/main.rs:18`), so any `Err` exits `1`. To get `3` the
error must be typed and mapped after teardown, so the teardown order is preserved:

```text
let res = if use_raw { rt.block_on(ui::raw::run(...)) } else { rt.block_on(ui::tui::run(...)) };  // :58-62
orchestrator::cancel_all();                                        // :64  (must still run)
rt.shutdown_timeout(Duration::from_millis(300));                   // :65
match res {
    Err(e) if e.downcast_ref::<session::RestoreRefused>().is_some() => std::process::exit(session::EXIT_RESTORE_REFUSED),
    other => other,                                                // existing anyhow path -> 1
}
```

`session::RestoreRefused` is the `Error`-implementing mirror of `RestoreOutcome::Refused` (§4.5): the outcome
type is data, the error type exists only to carry it out of `run_session_with_bounds_report` to `main`. The
`73` sandbox refusal code (`harness::sandbox::SANDBOX_REFUSE_EXIT_CODE`, `sandbox.rs:50`, used at
`main.rs:132/173/183`) is untouched; `3` is chosen because `1` (runtime error) and `2` (usage) are taken and
`73` is taken by the sandbox protocol.

---

## 4. Module layout and exact public Rust API signatures

Signatures only, no bodies. Names in `session::format::*` are Design A's and are **not** redefined here — they
are referenced so the flow's signatures are checkable. New items introduced by **this** document are marked
**(B)**.

### 4.1 File tree

```text
src/session/
  mod.rs              pub mod declarations; CaptureLedger; CaptureTrigger; CommitPolicy glue;
                      EXIT_RESTORE_REFUSED (B, §3.5)
  format/mod.rs       Design A §3 vocabulary verbatim: *File/*Section frames, MessageRecord,
                      ToolCallRecord, ToolFunctionRecord, ProcessEnding, IdentityFloors, Manifest,
                      ManifestMember, SectionKind, SectionHeader, WorkerStatus, HistorySource,
                      DanglingToolCallState, DanglingCall, TimePoint, RestoreOutcome, Refusal,
                      RefusalCode, RepairReport, NonRestorableNotice, NonRestorableKind,
                      RestoredSession (B defines it, §4.6), SESSION_RESTORE_NOTICE_PREFIX (B, §2.4),
                      const assertions pinning ABORTED_TOOL_RESULT == manager::context::ABORTED_TOOL_RESULT
  time.rs             anchor resolution + TimePoint math (A §2 anchors, §5.6)
  commit.rs           CommitLock, CommitPolicy, CommitReport (B), write_bundle, member diff/sweep,
                      fingerprint, bundle paths; LAST_COMMIT (B, §1.8)
  restore.rs          load_bundle, normalize, verify (A §7.3 L0-L9), install (R1-R5), BootReport (B, §4.9),
                      RestoreRefused error (B, §3.5), retire_interrupted_worker (B, §2.5)
  worker_journal.rs   WorkerJournal, registry, mirror-out API (§4.8)
```

`src/lib.rs` gains `pub mod session;` — the module list is alphabetical-ish
(`agents, config, debug_log, harness, llm, manager, markers, mcp, net, orchestrator, plan_parse, prompts,
task_id, text_util, tool_args, tool_names, types, ui` at `src/lib.rs:4-35`), so it lands between `prompts`
(`:24`) and `task_id` (`:26`).

### 4.2 Bundle paths

```rust
// src/manager/phase.rs  (alongside transcript_path():298 / ui_transcript_path():302, which A D2 deletes)
impl Plan {
    /// Root of the session bundle: `<plan dir>/session` (`.marmel/session` for the default plan).
    pub fn session_bundle_path(&self) -> std::path::PathBuf;
}

// src/session/commit.rs
pub const BUNDLE_DIR_NAME: &str = "session";      // == A §3.1 BUNDLE_DIR_NAME
pub fn bundle_dir(plan: &crate::manager::phase::Plan) -> std::path::PathBuf;   // == Plan::session_bundle_path()
pub fn manifest_path(bundle: &std::path::Path) -> std::path::PathBuf;          // bundle/MANIFEST_FILE
pub fn manager_path(bundle: &std::path::Path) -> std::path::PathBuf;
pub fn ui_path(bundle: &std::path::Path) -> std::path::PathBuf;
pub fn worker_path(bundle: &std::path::Path, worker_key: &str) -> std::path::PathBuf;
pub fn percent_escape_key(worker_key: &str) -> String;   // A §5.2: '%' first, then '#'->%23, '/'->%2F
pub fn percent_unescape(component: &str) -> Result<String, Refusal>;
```

`Plan::session_bundle_path()` is the **only** path authority; nothing in `session/` may re-derive the bundle
location from `MARMEL_DIR` (`phase.rs:269-272`), and no caller may hardcode `.marmel/session`.

### 4.3 Trigger surface (`session::mod`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTrigger {
    ManagerTurnBoundary,   // T-1
    PreDispatch,           // T-2  (never throttled)
    ToolResult,            // T-3
    UiEvent,               // T-4
    WorkerLifecycle,       // T-5
    Abort,                 // T-6 / T-7
    Panic,                 // T-8
    CleanTail,             // T-9
    Reset,                 // T-10
}

#[derive(Debug, Default)]
pub struct CaptureLedger {                 // §1.10
    pub commits_ok: u64,
    pub commits_failed: u64,
    pub last_error: Option<String>,
    pub last_ok_wall: Option<chrono::DateTime<chrono::Local>>,
    pub throttled_pending: bool,
}

/// The one call sites use. Session-thread only (R-TRIG-0): it takes `&mut` to the three
/// pieces of state that live on that thread, which is what makes misuse a compile error.
pub fn capture(
    trigger: CaptureTrigger,
    plan: &crate::manager::phase::Plan,
    ctx: &crate::manager::ContextEngine,
    renderer: &mut dyn crate::ui::Renderer,
    ui: &crate::ui::UiTranscript,
    cfg: &crate::config::Config,
    ledger: &mut CaptureLedger,
    ending: ProcessEnding,
) -> Result<CommitReport, CommitError>;

/// T-8: pure-sync, global-state-only commit (flow §1.8). Never touches renderer/ctx.
pub fn commit_from_panic(
    plan: &crate::manager::phase::Plan,
) -> Result<CommitReport, CommitError>;

/// T-10: delete the bundle (A §7.4 C5 sweep semantics), reporting a blocked deletion.
pub fn discard_bundle(
    plan: &crate::manager::phase::Plan,
    reason: ProcessEnding,
) -> Result<(), CommitError>;

pub const EXIT_RESTORE_REFUSED: i32 = 3;               // §3.5
```

### 4.4 Commit

```rust
// src/session/commit.rs
/// Session-owned commit mutex. `harness::fs::replace_path_lock` is PRIVATE
/// (src/harness/fs.rs:342) — Design A §11.3-1 — so `session/` owns its own lock.
/// `Send + Sync`; the *payload builders* are not, so callers must satisfy R-TRIG-0.
pub struct CommitLock { /* Mutex<GuardState> + dirty flag */ }
impl CommitLock {
    pub fn new() -> Self;
    /// `None` when a commit is already in flight: the caller sets `throttled_pending`
    /// instead of blocking (R-TRIG-1, flow §1.3).
    pub fn try_guard(&self) -> Option<CommitGuard>;
}
pub struct CommitGuard(/* MutexGuard */);
impl Drop for CommitGuard { /* re-checks the dirty flag; never panics */ }

pub struct CommitPolicy {
    pub min_interval: std::time::Duration,   // Config.session_commit_min_interval_ms
    pub max_bundle_bytes: u64,               // Config.session_max_bundle_bytes
    pub allow_throttled: bool,               // Config.session_commit_on_tool_result
}

/// What a trigger hands the committer. `ui` is captured by `Renderer::capture_ui` (§4.7)
/// and is `None` on the panic path, which cannot reach the renderer.
pub struct CapturePayload {
    pub manager: ManagerSection,
    pub ui: Option<UiSection>,
    pub workers: Vec<WorkerSection>,
    pub identity_floors: IdentityFloors,
}

#[derive(Debug)]
pub struct CommitReport {                    // (B) — Result must never be discarded (A D6)
    pub members_written: Vec<String>,        // changed only (A §7.2 C2)
    pub members_skipped_identical: usize,
    pub manifest_bytes: u64,
    pub orphans_removed: usize,              // A §7.4 C5
    pub duration: std::time::Duration,
}

#[derive(Debug, thiserror::Error /* crate already uses anyhow/thiserror patterns */)]
pub enum CommitError { Io { path: std::path::PathBuf, source: std::io::Error }, Serialize(String), Locked }

/// THE commit entry point. Order is normative (A §7.1-§7.2): members
/// workers -> ui -> manager (C3), then `manifest.json` LAST (C4) as the commit point.
/// Every member write goes through `harness::fs::write_file_atomic` (fs.rs:475-503).
/// Durability caveat recorded here because it is a property of the helper, not of this
/// function: atomicity vs process death, NOT fsync durability (Design A §11.3-3).
pub fn write_bundle(
    bundle: &std::path::Path,
    session_id: &str,
    payload: &CapturePayload,
    ending: ProcessEnding,
    policy: &CommitPolicy,
    lock: &CommitLock,
) -> Result<CommitReport, CommitError>;

/// Byte-identity diff cache (A §7.2 C2) + the last committed manifest.
/// The panic path reads this to rewrite only the manifest (flow §1.8).
pub struct LastCommit { /* HashMap<rel_path, (u64 byte_len, u64 fingerprint, Vec<u8> bytes)>, Manifest */ }
pub static LAST_COMMIT: std::sync::LazyLock<std::sync::Mutex<Option<LastCommit>>> = /* … */;

/// FNV-1a over the exact member bytes (A §7.1). Constants SHALL be a copy, since
/// harness/fs.rs:352-358 does not expose them; a unit test SHALL pin the parity.
pub fn fingerprint(bytes: &[u8]) -> u64;
```

### 4.5 Restore (read side)

```rust
// src/session/restore.rs
/// L0-L9 of Design A §7.3, read-only with respect to disk (A D6). No state installed
/// as a side effect of loading: `RestoredSession` is inert until `install` (§4.9) runs.
pub fn load_bundle(bundle: &std::path::Path) -> RestoreOutcome;

/// A §8 N0-N6 for one transcript. The manager vector and every worker vector go through
/// this SAME function (A §8.2: one grammar, one owner). No pairing rule lives in `session/`.
pub fn normalize(
    messages: Vec<MessageRecord>,
    recorded: &DanglingToolCallState,
    pins: &PinWitness,
    pinned_prefix_end: usize,
    engine: &mut crate::manager::ContextEngine,   // carries the repair grammar; no state kept
) -> Result<(Vec<MessageRecord>, NormalizeReport), Refusal>;

pub struct NormalizeReport {                  // feeds RepairReport (A §3.10)
    pub pruned_orphan_ids: Vec<String>,
    pub synthesized_aborted_ids: Vec<String>,
    pub regrouped_ids: Vec<String>,
    pub passes: u8,                            // always 2 (A §8.2 N5)
    pub stable: bool,
    pub pinned_prefix_end_before: usize,
    pub pinned_prefix_end_after: usize,
}

/// `messages[0..2]` pin verification, A §4 R-PIN-5 steps 1-5, and the post-normalization
/// re-check (A §8.2 N3).
pub fn verify_pins(section: &ManagerSection, on: PinStage) -> Result<(), Refusal>;
pub enum PinStage { Stored, Installed }

/// Carries a refusal out to `main` for exit code 3 (§3.5).
#[derive(Debug)] pub struct RestoreRefused { pub refusal: Refusal, pub bundle: std::path::PathBuf }
impl std::error::Error for RestoreRefused {}

/// The explicit close of a restored interrupted record (flow §2.5). Goes through the
/// existing completion bookkeeping instead of fabricating an `ActiveWorkerGuard`.
pub fn retire_interrupted_worker(worker_key: &str, label: &str) -> bool;
```

### 4.6 Manager capture / restore seam on `ContextEngine` (`src/manager/context.rs`)

The engine owns the message vector privately (`:558`), so the capture and restore seams are methods on it.
Both are additive; the pairing/advisory grammar (`prune_orphan_tool_messages:339`,
`repair_tool_call_pairs:384`, `ensure_tool_call_pairs:897`, `pairing_shape:444`,
`INJECTED_ADVISORY_PREFIXES:305`, `pinned_prefix_end:270`) is **reused, never duplicated** (A D1).

```rust
impl ContextEngine {
    /// CAPTURE. Snapshot of everything `ManagerSection` needs (A §3.5) — no mutation,
    /// no repair, no truncation: this is exactly `messages()` (`:633`) at this instant.
    pub fn capture_manager_section(&self) -> session::format::ManagerSection;

    /// RESTORE (A §4 R-PIN-2, working name kept). Installs the vector WHOLESALE.
    /// SHALL NOT call `set_system_prompt` (`:592`) or `set_goal` (`:604`), before, after
    /// or during — and SHALL NOT pad an empty system prompt (`:604-611`).
    /// Panics on a length < 2 (a caller bug; the loader already refuses it, A R-PIN-5).
    pub fn install_restored_messages(&mut self, messages: Vec<session::format::MessageRecord>);

    /// Restore of the non-message engine state (A §3.5 fields
    /// `rebirth_advisory_emitted` / `consecutive_rebirths`), bypassing
    /// `inject_rebirth_advisory` (`:708`) and `reset_*` (`:702`, `:716`).
    pub fn restore_engine_flags(&mut self, rebirth_advisory_emitted: bool, consecutive_rebirths: usize);

    /// Witness recomputation used by restore's N3/N6 and by capture.
    pub fn pinned_prefix_end(&self) -> usize;               // exposes the private `pinned_prefix_end` (`:270`)
    pub fn pin_witness(&self) -> session::format::PinWitness;
    pub fn dangling_state(&self) -> session::format::DanglingToolCallState;
    /// The one place `SESSION_RESTORE_NOTICE_PREFIX` is appended, so the notice is a
    /// `Message::User` at index >= 2 and the pins are untouched (flow §2.4).
    pub fn append_restore_notice(&mut self, notice: &str);
}
```

`t-009` additionally adds `"(SYSTEM: SESSION RESTORED"` to `INJECTED_ADVISORY_PREFIXES` (`context.rs:305-309`)
so `is_injected_advisory` (`:314`) recognizes the notice and `perform_rebirth` (`:935`, scan `:949-957`) can
never promote it into the `messages[2]` slot.

Deleted by `t-009` (Design A D2): `ContextEngine::save_transcript` (`:988`) and `load_transcript` (`:998`).

### 4.7 Renderer hooks (`src/ui/mod.rs`, `pub trait Renderer: Send` at `:88`)

Both hooks are **added to the trait with default bodies**, so the raw renderer (`src/ui/raw.rs:20`) keeps
compiling unchanged and only `TuiRenderer` overrides them.

```rust
pub trait Renderer: Send {
    // … existing members unchanged (init :89 … set_subagents :127, set_thinking_budgets :128,
    //    reset_active_agent :129) …

    /// CAPTURE the pane model (A §6.1 `UiSection`). `&self`: a capture never mutates the UI.
    /// Legal ONLY on the session thread — `Renderer: Send` but `TuiRenderer` holds
    /// `Cell`/`RefCell` layout state (`src/ui/tui/mod.rs:100-122`) so it is NOT `Sync`,
    /// and `dyn Renderer` therefore cannot be captured through a shared reference from
    /// another thread. That is enforced by the signature of `capture` (§4.3), which takes
    /// `&mut dyn Renderer` from the session loop.
    fn capture_ui(&self) -> session::format::UiSection { session::format::UiSection::default() }

    /// REHYDRATE the panes from the stored section (A D4: pane text is the authority;
    /// U1 forbids regenerating a line from a journal record). Assign, do not append,
    /// do not re-render, and re-clamp scroll/caret after assignment exactly as
    /// `TuiRenderer`'s own resize path does (e.g. `src/ui/tui/events.rs:466-480`).
    fn restore_panes(&mut self, section: &session::format::UiSection);

    /// One-shot surfacing of the restore report (A §8.4: loud, three destinations).
    fn announce_restore(&mut self, report: &session::format::RepairReport) {}
}
```

`TuiRenderer` implements the pair as a **pure field transfer**, both directions:
`messages ↔ chat_messages`, `status_line`, `subagents ↔ subagent_panels` (A §6.3 `SubagentPanel`; the
`Instant`-based fields of `SubagentDetail` (`src/ui/mod.rs:19-37`) are converted through `TimePoint` per A
§5.6 — `worked_duration_secs`, `started_at`, `last_activity_at`), textarea `input_text`/`cursor`/history ↔
`ComposerState`, every scroll/toggle/focus field ↔ `ViewState`, and the budget view ↔ `BudgetWitness`
(`set_thinking_budgets` `:128` stays live-config-driven; the witness is for the *report*).

### 4.8 `WorkerJournal` — the mirror-out API (`src/session/worker_journal.rs`)

Answers Design A §5.5: the specialist engine is a local at `execution.rs:175`, so the mirror handle must be
threaded in the way `CURRENT_WORKER_TOKEN` already is (`bus.rs:33-35`, scoped at `execution.rs:166-171`).

```rust
/// Handle held by the worker runner and by the registry. `Send + Sync`
/// (its only state is `Mutex<Vec<MessageRecord>>` + copyable identity).
pub struct WorkerJournal { /* worker_key, worker_id_seq_value, Mutex<Vec<MessageRecord>>,
                              HistorySource, cap, truncation notice flag */ }

pub fn open(
    worker_key: &str,
    worker_id_seq_value: u64,
    mirror_cap: usize,                       // Config.session_worker_mirror_max_messages
) -> std::sync::Arc<WorkerJournal>;

/// Task-local for the current worker, mirroring `CURRENT_WORKER_TOKEN` (`bus.rs:33-35`).
tokio::task_local! {
    pub static CURRENT_WORKER_JOURNAL: std::sync::Arc<WorkerJournal>;
}
pub fn current_journal() -> Option<std::sync::Arc<WorkerJournal>>;   // try_with, never panics

/// Process-wide key -> journal registry, consulted by the committer (T-5) and by the panic path.
pub static REGISTRY: std::sync::LazyLock<std::sync::RwLock<std::collections::BTreeMap<String, std::sync::Arc<WorkerJournal>>>>;
pub fn registry_snapshot() -> Vec<std::sync::Arc<WorkerJournal>>;   // never holds a guard across a write
pub fn unregister(worker_key: &str) -> Option<std::sync::Arc<WorkerJournal>>;

impl WorkerJournal {
    /// Called from EVERY engine mutation site — `execution.rs:255, 490, 539, 559, 584, 613, 764`
    /// and `fix_loop.rs:736, 740, 777, 954, 1046, 1147`.
    pub fn mirrored_append(&self, msg: &crate::types::Message);
    /// `execution.rs:524, 552, 573`: replace, do NOT append.
    pub fn mirrored_replace_last(&self, msg: &crate::types::Message);
    /// `compact()` / `inject_rebirth_advisory()` (`execution.rs:1033-1042`): collapse the
    /// mirror to the new vector (A §5.5).
    pub fn mirrored_collapse(&self, messages: &[crate::types::Message]);
    /// In-memory only. SHALL NOT touch disk (flow §1.3).
    pub fn snapshot(&self) -> Vec<MessageRecord>;
    pub fn history_source(&self) -> HistorySource;     // NotCaptured | SeedOnly | LiveMirror
    pub fn was_truncated(&self) -> bool;               // drives the R-TRIG-4 notice
    /// Full `WorkerSection` projection for the bundle (identity fields are supplied by
    /// the caller from `WORKERS`, A §5.3; the journal supplies only history + source).
    pub fn project_history(&self) -> (Vec<MessageRecord>, HistorySource, DanglingToolCallState);
}
```

`MessageRecord::from_wire` / `into_wire` (A §3.7) are the only conversions; the mirror SHALL NOT route through
`serde`, `ToolCall::new`, `sanitize_arguments` or `ensure_valid_json_arguments` (`types/wire.rs:88-108`,
`:135-138`) — those rewrite arguments (A §3.7).

### 4.9 Boot: load → normalize → install ctx + `WORKERS` + UI in one pass

```rust
// src/session/restore.rs
#[derive(Debug)]
pub struct BootReport {                       // (B) what the caller prints, never drops
    pub outcome: RestoreOutcome,
    pub manager_messages: usize,
    pub workers_installed: usize,
    pub interrupted_worker_keys: Vec<String>,
    pub side_effect_risk_keys: Vec<String>,
    pub restore_notice: Option<String>,       // N-MGR-1 text (flow §2.4)
    pub ui_restored: bool,
}

/// THE boot function. One pass, on the session thread, called from
/// `run_session_with_bounds_report` after the `Plan` exists (the analogue of the seams
/// at `src/ui/session.rs:273-319`) and before `reset_cancellation()` (`:389`) / the loop (`:391`).
/// Order is fixed by A §5.7 + §7.3 L9: verify (L0-L7) -> normalize (L8) -> floors ->
/// install `WORKERS` -> install the engine -> restore the panes -> append `N-MGR-1`.
pub fn boot(
    plan: &crate::manager::phase::Plan,
    cfg: &crate::config::Config,
    ctx: &mut crate::manager::ContextEngine,
    renderer: &mut dyn crate::ui::Renderer,
    session_id: &mut String,
) -> Result<BootReport, RestoreRefused>;

/// The `--resume` gate (§3.3-§3.5). Returns `Ok(None)` for `NoBundle` (fresh start,
/// warned) and `Err(RestoreRefused)` for a refusal.
pub fn boot_if_requested(
    resume_requested: bool,
    plan: &crate::manager::phase::Plan,
    cfg: &crate::config::Config,
    ctx: &mut crate::manager::ContextEngine,
    renderer: &mut dyn crate::ui::Renderer,
    session_id: &mut String,
) -> Result<Option<BootReport>, RestoreRefused>;

/// R1-R5 of A §5.7, in order, with the ring cap and the floors. `session/` owns the
/// installer because `WORKERS` (workers.rs:106) is private to `orchestrator::workers`:
/// `t-010` adds the narrow `pub fn install_restored_worker(state: WorkerState) -> bool`
/// and the `fetch_max` floor helpers next to the statics (workers.rs:109/121,
/// notice.rs:34) — no other registry mutation is exposed.
fn install_workers(session: &RestoredSession, floors: &IdentityFloors) -> Vec<String>;
```

```rust
// src/session/format/mod.rs — the type A §3.10 uses but never defines.
#[derive(Debug, Clone)]
pub struct RestoredSession {
    pub session_id: String,
    pub manager: ManagerSection,
    pub ui: UiSection,
    pub workers: Vec<WorkerSection>,
    pub identity_floors: IdentityFloors,
    pub manifest: Manifest,          // the verified manifest, so the caller can quote ending/committed_wall
    pub bundle: std::path::PathBuf,
}
```

`boot` is deliberately **not** `async`: everything it touches is synchronous (file reads, the `DashMap`,
`renderer` fields), and keeping it sync is what makes the panic path and the TUI/raw boot paths share one code
path without a runtime dependency.

---

## 5. Sequencing and integration map

### 5.1 Task numbering bridge (read this before the table)

Design A's text refers to its own plan numbering: **`t-018`/`t-019` are Design A itself** (already done), and
its one forward reference — "`ContextEngine::install_restored_messages` … owned by `session/`'s contract with
`manager::`, t-010" (A §4 R-PIN-2) — is the item this plan calls **t-009** (manager capture). No other A
forward reference exists. Implementation tasks are the ones named below.

### 5.2 Element → task map

| Design element (this doc) | Built by | Files touched | Depends on |
|---|---|---|---|
| `src/session/` skeleton, `format/` vocabulary, `time.rs`, `Plan::session_bundle_path()`, `lib.rs` module, `write_bundle` + `CommitLock` + `fingerprint` + `LAST_COMMIT` + `CommitReport` (§4.1-§4.4) | **t-007 persistence core** | `src/session/{mod,format/mod,time,commit}.rs`, `src/lib.rs`, `src/manager/phase.rs` (path helper + delete `TRANSCRIPT_FILE:117` / `UI_TRANSCRIPT_FILE:119` consumers), `src/config.rs` (six keys, §3.1) | — |
| `load_bundle`, `verify_pins`, `normalize`, `RestoreOutcome`/`Refusal` plumbing, `RestoredSession`, L0–L9 + §7.5 table, bundle `deny_unknown_fields` test (T-CFG-3) | **t-008 normalization / load** | `src/session/restore.rs`, `src/session/format/mod.rs` | t-007 |
| `capture_manager_section`, `install_restored_messages`, `restore_engine_flags`, `pinned_prefix_end`/`pin_witness`/`dangling_state`/`append_restore_notice`, `SESSION_RESTORE_NOTICE_PREFIX` added to `INJECTED_ADVISORY_PREFIXES:305`, deletion of `save_transcript:988` / `load_transcript:998` (§4.6) | **t-009 manager capture** | `src/manager/context.rs`, `src/session/format/mod.rs` | t-007, t-008 |
| `WorkerJournal` + `CURRENT_WORKER_JOURNAL` + `REGISTRY`, mirror calls at `execution.rs:255/490/524/539/552/559/573/584/613/764` + `execution.rs:1033-1042` + `fix_loop.rs:736/740/777/954/1046/1147`, `install_restored_worker` + floor `fetch_max` helpers in `workers.rs`/`notice.rs` (§4.8, §4.9 `install_workers`) | **t-010 worker capture** | `src/session/worker_journal.rs`, `src/agents/runner/{execution,fix_loop}.rs`, `src/orchestrator/{workers,notice}.rs` | t-007; §2 policy from this doc |
| `Renderer::capture_ui` / `restore_panes` / `announce_restore` + `TuiRenderer` field transfers + `UiSection`↔`SubagentDetail`/`ComposerState`/`ViewState` conversions (§4.7) | **t-011 renderer hooks** | `src/ui/mod.rs`, `src/ui/tui/mod.rs`, (raw default `src/ui/raw.rs`) | t-007 |
| boot wiring: `boot_if_requested` call inside `run_session_with_bounds_report`, the resume-branch skips of `:273`/`:380`/`:346-377`, `session_id` lifetime, `RestoreRefused` → `main` exit 3, `--resume`/`--fresh` in `CliArgs`/`parse_args`/help, `MARMEL_*` env keys (§3.3-§3.5, §4.9) | **t-012 boot wiring** | `src/ui/session.rs`, `src/main.rs`, `src/ui/tui/mod.rs:1070`, `src/ui/raw.rs:20`, `src/config.rs` (env) | **t-009 then t-011** (needs the engine seam and the pane hooks) |
| Triggers T-1…T-10 in the session loop and orchestrator, `CaptureLedger`, throttling, `commit_from_panic` in the hook (`main.rs:276-281`), `discard_bundle` on `/reset`/archive, the `N-MGR-1` emission, `continue_worker` trigger wiring + `retire_interrupted_worker` (§1, §2) | **t-013 lifecycle triggers** | `src/ui/session.rs`, `src/ui/tui/events.rs`, `src/orchestrator/{mod,workers,preemption}.rs`, `src/main.rs` | **t-012** |

### 5.3 The strict serialization on `src/ui/session.rs` (2 430 lines at HEAD)

`src/ui/session.rs` is the single most-contended file in this feature: 22 `ctx.save_transcript` sites
(`690, 717, 727, 956, 1224, 1272, 1371, 1407, 1413, 1422, 1441, 1460, 1485, 1734, 1810, 1820, 1848, 1884,
1916, 1932, 1958, 2166`), 22 `ui_transcript.save` sites (`382, 400, 445, 707, 844, 985, 1078, 1204, 1211,
1249, 1347, 1370, 1511, 1590, 1712, 1719, 1759, 1874, 1960, 2118, 2173, 2207`), one 1 500-line loop
(`:258` → `:1977`), and the abort/wall-clock breaks at `:1008-1017`, `:1019-1033`, `:1127-1135`. Three tasks
must edit it. Therefore:

```text
t-009 (manager capture)   ->  t-012 (boot wiring)   ->  t-013 (lifecycle triggers)
      src/manager/              session.rs boot:         session.rs loop +
      context.rs only           :273-:391 + main.rs      :474-:1977 + orchestrator
```

- **t-009 does not touch `session.rs`.** It changes the engine seam, so its risk is confined to
  `manager/context.rs` and its own tests.
- **t-012 owns the boot region only** (`run_session_with_bounds_report` `:258` through `reset_cancellation()`
  `:389`, plus `main.rs`, `tui/mod.rs:1070`, `raw.rs:20`). It does **not** touch any of the 44 save sites and
  does not move the loop body — that keeps its diff to a few hundred lines at the top of the function.
- **t-013 owns the loop body and the trigger sites**, including the deletion of the 44 legacy save calls, and
  it lands **after** t-012 because a trigger needs a bundle that boot can already read: a trigger without a
  loader writes files nobody can consume, and a loader without triggers can only be tested with hand-made
  bundles.
- Merge rule for anyone working in parallel: a change to `src/ui/session.rs` between `:258` and `:391` belongs
  to t-012; a change after `:391` belongs to t-013. If both must land, t-012 first, rebase t-013.
- Guard test for the serialization: `tests/test_ui_session.rs` already drives the real loop with small
  `SessionBounds` (the doc comment at `src/ui/session.rs:227-232` says so). t-012 adds the
  "resume-then-first-turn" case; t-013 adds the trigger-frequency cases. Neither edits the other's test.

---


## 6. Risks and open questions (10)

Each item names its **resolution owner**. Items marked *(question)* need a decision before the owning task
starts, not during it.

1. **`RestoredSession` was undefined in Design A** (used at A §3.10 `Restored`/`PartiallyRestored`, never
   declared — verified: two occurrences, both uses). I define it in §4.6/§4.9 to keep t-007/t-008 unblocked.
   If A's author intended a different shape (e.g. owning `Manifest` vs borrowing it), that is a **naming-level
   amendment to A §3.10**, not a flow change. → **Owner: t-007** (lands the type); arbitrator to confirm the
   shape before t-008 codes against it. *(question)*
2. **`WORKERS` and the three identity statics are private** (`workers.rs:106/109/121`, `notice.rs:34`), so
   restore cannot install a worker or raise a floor without new `pub` seams in `orchestrator`. Every such seam
   is a place a future caller could re-mint identity (A §5.7 step 2 forbids exactly that). Mitigation: one
   narrow `install_restored_worker(WorkerState) -> bool` + floor `fetch_max` helpers, `pub(crate)` if
   `session/` can reach it, and a test that `claim_worker_slot` is never reached during restore. → **Owner:
   t-010**; visibility question answered the same day.
3. **No fsync ⇒ a resume can be older than the operator thinks.** A §11.3-3: `write_file_atomic`
   (`fs.rs:475-503`) is atomic vs process death but not durable vs power loss. `manifest.json` being last
   guarantees self-consistency, not recency. Decision: do **not** add `sync_all` on the hot path; add it only
   on T-6/T-9 (abort/clean tail, once per session), and print the manifest's `committed_wall` with every
   restore so the operator sees the actual cut-off instant. → **Owner: t-007** (helper + policy flag),
   **t-013** (calls it from T-6/T-9).
4. **Trigger storms during a wide fan-out.** A 4-tool round emits T-2, four T-3s, several T-4s and T-5s; with
   `MAX_TURNS = 100` (`manager/loop.rs:51`) and 10 completed workers, a chatty session can write the bundle
   thousands of times. C2's byte-diff plus the `session_commit_min_interval_ms` floor bound it, but the
   **manager member is the expensive one** (full transcript re-serialize per commit) — the cost is
   O(transcript bytes) per T-3. Open: incremental manager member (append-only) vs accept-the-cost. Decision
   deferred to measurement, with a hard requirement that whatever ships keeps A C4 (manifest last) and never
   grows unbounded. → **Owner: t-013** (measure), escalate to the arbitrator if the profile shows > 5 %
   wall-clock overhead on a delegation-heavy turn.
5. **The phantom-active problem is a direct consequence of obeying A §5.3/AC-A1.** Restoring `inProgress` with
   `completed_seq: None` makes `has_active_workers()` (`workers.rs:483`) true, which gates steer arbitration
   (`session.rs:446`, `bridge/steer.rs:65/99`, `bridge/sink.rs:381`) and `/abort` semantics for the whole
   session, while nothing is actually running. §2.2 mitigates (fresh token, `status_label` rewrite, pane
   annotation) but does not remove it, and A's invariant is explicit, so B will not silently contradict it.
   Candidate fix, needs A's agreement: add `WorkerState.restored_without_task: bool` (or a
   `WorkerStatus::Interrupted`) so the "counts as active" predicate can be honest without breaking I-W5.
   → **Owner: t-010** implements, **arbitrator** approves the A-side amendment. *(question)*
6. **`continue_worker` (O3-C) is a new manager tool**, which means tool-schema re-pricing
   (`sync_manager_tool_schema`, advertised list charged per request) and role gating (`src/ui/bridge` tool
   policy) both change; adding it late re-prices every restored session. Fallback if it is judged too
   invasive: drop the tool and expose O3-C only as `--resume-worker <key>` at boot, which keeps the manager's
   tool list untouched. → **Owner: t-013** (trigger) with **t-010** (handler); decision needed at t-012
   completion. *(question)*
7. **Panic-path commits can be misleading if `LAST_COMMIT` is stale.** §1.8 rewrites a manifest from the last
   successful commit's bytes; if that commit predates the panic by several turns, `ending: PanicHook`
   overstates freshness. Mitigation: the panic manifest also records `committed_wall` of the *member* bytes
   (already per-member in `ManifestMember.saved_wall`, A §3.2) and the loader prints both instants when they
   differ by more than `session_commit_min_interval_ms`. → **Owner: t-007** (field/report), **t-013** (hook).
8. **Double-delegation after a resume.** The manager's transcript already says its delegation call returned
   `"(aborted)"` (A §8 N1), so a model that reads `N-MGR-1` may re-delegate the same task while the restored
   record is still active and plan-task check-off still points at the old key. Mitigation planned:
   `N-MGR-1` names the live worker key + task id and instructs to continue-or-retire explicitly, plus
   `retire_interrupted_worker` on any manager decision. Open: whether a *hard* block is needed (refuse a
   `delegate` whose `task_id` matches an active restored worker). → **Owner: t-013**, decision at integration.
   *(question)*
9. **Two `ProcessEnding` vocabularies could drift** — `HardKillInferred`/`Signal` are loader-only (A §3.4) and
   B's §1.5 mapping is the only writer rule. A test that enumerates every `ProcessEnding` variant and asserts
   exactly which writer may produce it is the only thing keeping them aligned. → **Owner: t-008** (the
   enumeration test lives next to `load_bundle`).
10. **`--resume` against a bundle written by a different binary.** A §7.3 L3 refuses on
    `format_version`/`schema_version` mismatch, which is correct but means an upgrade silently makes every
    existing bundle unresumable, with no migration story and no operator-visible "your old sessions are now
    unreadable" step. Minimum acceptable: on `FormatVersion`/`SchemaVersion` refusal, print the bundle's
    `committed_wall` and session id plus "N bundles unreadable", and (open) a `session::format` migration shim
    later. → **Owner: t-008** (report), **arbitrator** decides whether a migration shim is in scope at all.
    *(question)*

---
